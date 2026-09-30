//! Long-lived Pi RPC agent pool.
//!
//! `pi --mode rpc` is a persistent JSONL-controlled process. Instead of
//! spawning a fresh `pi` per request, this adapter keeps a pool of them: an
//! incoming request reuses an idle agent, and a new agent is spawned when none
//! is available. The pool has no limit of its own; the single global
//! `[session] workers` count caps how many agents may live at once. Idle
//! agents are evicted after a configurable TTL.
//!
//! By default a process is bound to one conversation: a request for a
//! different conversation in the same workspace starts a new process so it
//! resumes its own session instead of inheriting the process's earlier one.
//! `session_per_conversation = false` restores workspace-level reuse, where an
//! idle process is handed to any conversation and carries its session.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdout};
use tokio::sync::{Notify, mpsc};
use uuid::Uuid;

use crate::agent::prompt::{build_follow_up_prompt, build_prompt};
use crate::agent::session::SessionStore;
use crate::agent::{
    Agent, AgentContext, AgentOutcome, AgentRequest, LiveOutput, SteerReceipt, TokenUsage,
    conversation_key,
};
use crate::config::PiRpcConfig;
use crate::error::{BotError, Result};
use crate::executor::{ExecSpec, Prepared};

/// Environment variables from the caller (an AaaU session, an interactive
/// shell, ...) that must not leak into a managed agent.
const SCRUBBED_ENV: &[&str] = &[
    "AAAU_SESSION_ID",
    "AAAU_EDITOR_SOCKET",
    "TERM_PROGRAM",
    "EDITOR",
    "VISUAL",
];

/// Arguments for a `pi --mode rpc` process.
///
/// `session_id` is only used when sessions are persisted (`no_session =
/// false`), so an evicted process resumes its conversation from disk. With
/// `no_session = true` the process is explicitly ephemeral.
fn rpc_arguments(
    config: &PiRpcConfig,
    session_id: Option<&str>,
    model: Option<&str>,
) -> Vec<String> {
    let mut args = vec!["--mode".to_owned(), "rpc".to_owned()];
    if config.approve {
        args.push("--approve".to_owned());
    }
    if config.no_session {
        args.push("--no-session".to_owned());
    } else if let Some(session_id) = session_id {
        args.push("--session-id".to_owned());
        args.push(session_id.to_owned());
    }
    args.extend(config.args.iter().cloned());
    // A per-user model is appended unless the operator already set one.
    if let Some(model) = model.map(str::trim).filter(|model| !model.is_empty())
        && crate::agent::command::model_arg(&args, false).is_none()
    {
        args.push("--model".to_owned());
        args.push(model.to_owned());
    }
    args
}

/// A single `pi --mode rpc` subprocess.
pub struct PiRpcClient {
    child: Child,
    writer: mpsc::UnboundedSender<Value>,
    lines: Lines<BufReader<ChildStdout>>,
    next_id: u64,
    workspace: PathBuf,
    /// Guard that kills the run's cgroup when this client is dropped or
    /// killed. `None` for the test-only direct executor.
    _cgroup: Option<crate::executor::CgroupGuard>,
}

/// A cloneable handle for injecting commands into a live `pi --mode rpc`
/// process without owning its reader.
#[derive(Clone)]
pub struct PiRpcWriter {
    tx: mpsc::UnboundedSender<Value>,
}

impl PiRpcWriter {
    fn send(&self, value: Value) -> Result<()> {
        self.tx.send(value).map_err(|_| BotError::Agent {
            name: "pi-rpc".into(),
            reason: "pi process is no longer accepting input".into(),
        })
    }
}

impl PiRpcClient {
    /// Ask the live agent for its selected model. A missing model is normal
    /// before provider selection or with older RPC implementations.
    pub async fn current_model(&mut self) -> Result<Option<String>> {
        let request_id = self.next_request_id();
        self.send(&json!({ "id": request_id, "type": "get_state" }))
            .await?;
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let record = self.next_record_before(Some(deadline)).await?;
            if record["type"] == "response" && record["id"].as_str() == Some(request_id.as_str()) {
                if record["success"].as_bool() == Some(false) {
                    return Ok(None);
                }
                let model = &record["data"]["model"];
                return Ok(match (model["provider"].as_str(), model["id"].as_str()) {
                    (Some(provider), Some(id)) if !provider.is_empty() && !id.is_empty() => {
                        Some(format!("{provider}/{id}"))
                    }
                    (_, Some(id)) if !id.is_empty() => Some(id.to_owned()),
                    _ => None,
                });
            }
        }
    }
    /// Spawn a new RPC agent in `workspace`.
    ///
    /// When `session_id` is set (and sessions are persisted) the process is
    /// started with `--session-id <id>`, so an evicted or restarted agent can
    /// resume the same on-disk conversation instead of cold-starting.
    pub fn spawn(
        config: &PiRpcConfig,
        workspace: &Path,
        credentials: &[(String, String)],
        session_id: Option<&str>,
        executor: &crate::executor::Executor,
        host_user: Option<&str>,
        model: Option<&str>,
    ) -> Result<Self> {
        let mut env: Vec<(String, String)> = config
            .env
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        env.extend(credentials.iter().cloned());
        let spec = ExecSpec {
            program: config.command.clone(),
            args: rpc_arguments(config, session_id, model),
            env,
            cwd: (!workspace.as_os_str().is_empty()).then(|| workspace.to_path_buf()),
            host_user: host_user.map(str::to_owned),
        };
        let Prepared {
            command: mut cmd,
            cgroup,
        } = executor.command(&spec).map_err(|error| BotError::Agent {
            name: "pi-rpc".into(),
            reason: error.to_string(),
        })?;
        for key in SCRUBBED_ENV {
            cmd.env_remove(key);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|error| BotError::Agent {
            name: "pi-rpc".into(),
            reason: format!("failed to spawn `{}`: {error}", config.command),
        })?;

        let stdin = child.stdin.take().ok_or_else(|| BotError::Agent {
            name: "pi-rpc".into(),
            reason: "pi stdin was not captured".into(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| BotError::Agent {
            name: "pi-rpc".into(),
            reason: "pi stdout was not captured".into(),
        })?;

        // Own stdin from a small writer task so a follow-up can be injected
        // while this client's reader is checked out by a run in flight.
        let (writer, mut writer_rx) = mpsc::unbounded_channel::<Value>();
        tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(value) = writer_rx.recv().await {
                let Ok(mut line) = serde_json::to_string(&value) else {
                    continue;
                };
                line.push('\n');
                if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
                    tracing::debug!("pi stdin closed; stopping writer task");
                    break;
                }
            }
        });

        tracing::debug!(workspace = %workspace.display(), "spawned pi rpc agent");

        Ok(Self {
            child,
            writer,
            lines: BufReader::new(stdout).lines(),
            next_id: 1,
            workspace: workspace.to_path_buf(),
            _cgroup: cgroup,
        })
    }

    /// A cloneable handle that injects commands into this process.
    pub fn writer(&self) -> PiRpcWriter {
        PiRpcWriter {
            tx: self.writer.clone(),
        }
    }

    /// Process id of the child, while it is alive.
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    /// Whether the child is still running.
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Terminate the child.
    pub fn kill(&mut self) {
        let _ = self.child.start_kill();
    }

    fn next_request_id(&mut self) -> String {
        let id = self.next_id;
        self.next_id += 1;
        format!("req-{id}")
    }

    async fn send(&mut self, value: &Value) -> Result<()> {
        self.writer
            .send(value.clone())
            .map_err(|_| BotError::Agent {
                name: "pi-rpc".into(),
                reason: "pi process is no longer accepting input".into(),
            })
    }

    /// Read the next JSONL record, skipping malformed lines.
    async fn next_record(&mut self) -> Result<Option<Value>> {
        loop {
            let Some(line) = self.lines.next_line().await? else {
                return Ok(None);
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(line) {
                Ok(value) => return Ok(Some(value)),
                Err(error) => {
                    tracing::debug!(%error, "ignoring non-JSON line from pi");
                }
            }
        }
    }

    async fn next_record_before(&mut self, deadline: Option<Instant>) -> Result<Value> {
        let record = match deadline {
            Some(deadline) => {
                let remaining = deadline
                    .checked_duration_since(Instant::now())
                    .ok_or_else(|| self.timeout_error())?;
                match tokio::time::timeout(remaining, self.next_record()).await {
                    Err(_) => return Err(self.timeout_error()),
                    Ok(result) => result?,
                }
            }
            None => self.next_record().await?,
        };
        record.ok_or_else(|| BotError::Agent {
            name: "pi-rpc".into(),
            reason: "pi exited before the run settled".into(),
        })
    }

    fn timeout_error(&self) -> BotError {
        BotError::Agent {
            name: "pi-rpc".into(),
            reason: format!("pi timed out (workspace {})", self.workspace.display()),
        }
    }

    /// Send a prompt and wait until `agent_settled`, returning the assistant's
    /// final text. When `timeout` is `None` the wait is unbounded: the call
    /// only returns once the agent settles or its process exits.
    pub async fn prompt(&mut self, message: &str, timeout: Option<Duration>) -> Result<String> {
        self.prompt_with_output(message, timeout, None)
            .await
            .map(|(text, _)| text)
    }

    async fn prompt_with_output(
        &mut self,
        message: &str,
        timeout: Option<Duration>,
        live_output: Option<&LiveOutput>,
    ) -> Result<(String, TokenUsage)> {
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        let request_id = self.next_request_id();
        self.send(&json!({
            "id": request_id,
            "type": "prompt",
            "message": message,
        }))
        .await?;

        let mut streamed = String::new();
        let mut usage = TokenUsage::default();
        let mut streamed_usage = TokenUsage::default();
        loop {
            let record = self.next_record_before(deadline).await?;
            match record["type"].as_str().unwrap_or_default() {
                "response" => {
                    if record["id"].as_str() == Some(request_id.as_str())
                        && record["success"].as_bool() == Some(false)
                    {
                        let error = record["error"]
                            .as_str()
                            .unwrap_or("pi rejected the prompt")
                            .to_owned();
                        return Err(BotError::Agent {
                            name: "pi-rpc".into(),
                            reason: error,
                        });
                    }
                }
                "message_update" => {
                    if let Some(event) = record.get("assistantMessageEvent")
                        && event["type"] == "text_delta"
                        && let Some(delta) = event["delta"].as_str()
                    {
                        streamed.push_str(delta);
                        if let Some(live_output) = live_output {
                            live_output.append(delta.as_bytes());
                        }
                    }
                    // The partial message carries the running usage; keep it
                    // as a fallback in case the final `message_end` is missed.
                    if let Some(partial) = assistant_usage(&record["usage"]) {
                        streamed_usage = partial;
                    }
                }
                "message_end" => {
                    if let Some(settled) = assistant_usage(&record["message"]["usage"]) {
                        usage.prompt_tokens += settled.prompt_tokens;
                        usage.cached_tokens += settled.cached_tokens;
                    }
                }
                "agent_settled" => break,
                _ => {}
            }
        }

        // Prefer pi's authoritative final message; fall back to the stream.
        let text = match self.get_last_assistant_text(deadline).await {
            Ok(Some(text)) if !text.trim().is_empty() => text,
            _ => streamed,
        };
        if usage.prompt_tokens == 0 {
            usage = streamed_usage;
        }
        Ok((text, usage))
    }

    async fn get_last_assistant_text(
        &mut self,
        deadline: Option<Instant>,
    ) -> Result<Option<String>> {
        let request_id = self.next_request_id();
        self.send(&json!({
            "id": request_id,
            "type": "get_last_assistant_text",
        }))
        .await?;

        loop {
            let record = self.next_record_before(deadline).await?;
            if record["type"] == "response" && record["id"].as_str() == Some(request_id.as_str()) {
                return Ok(record["data"]["text"].as_str().map(str::to_owned));
            }
        }
    }
}

/// Read one pi usage object (`input` = cache miss, `cacheRead` = cache hit).
///
/// Returns `None` when the object carries neither field, so an unrelated
/// `message_end` (for example a tool result) does not count as a zero-token
/// model call.
fn assistant_usage(value: &Value) -> Option<TokenUsage> {
    let input = value["input"].as_u64();
    let cached = value["cacheRead"].as_u64();
    if input.is_none() && cached.is_none() {
        return None;
    }
    let input = input.unwrap_or(0);
    let cached = cached.unwrap_or(0);
    Some(TokenUsage {
        prompt_tokens: input + cached,
        cached_tokens: cached,
    })
}

/// State of the pool.
#[derive(Default)]
struct PoolState {
    agents: Vec<PoolEntry>,
    /// Conversation (issue / pull request) key -> last agent id. Prefer that
    /// agent when idle, while allowing another one to handle a busy thread.
    conversations: HashMap<String, Uuid>,
}

struct PoolEntry {
    id: Uuid,
    key: String,
    pid: Option<u32>,
    workspace: PathBuf,
    /// Linux account this process runs as, so a process spawned for one user
    /// is never reused for another.
    host_user: Option<String>,
    /// Model this process was started with, so a differently configured
    /// process is never reused.
    model: Option<String>,
    client: Option<PiRpcClient>,
    /// Writer for the live process. Kept on the entry while `client` is
    /// checked out by a run in flight, so a follow-up can be injected without
    /// spawning another agent.
    writer: Option<PiRpcWriter>,
    busy: bool,
    last_used: Instant,
}

/// Shared pool internals.
struct PoolInner {
    config: PiRpcConfig,
    /// Cap on live `pi` processes. This is the single global
    /// `[session] workers` count, not a separate pool limit.
    max_agents: usize,
    /// Conversation -> backend session id, so evicted processes resume.
    sessions: Arc<SessionStore>,
    state: Mutex<PoolState>,
    notify: Notify,
}

impl PoolInner {
    /// Drop dead or expired idle agents, and any conversation mapping whose
    /// agent no longer exists. Callers hold the state lock.
    fn reap(&self, state: &mut PoolState) {
        let ttl = Duration::from_secs(self.config.idle_ttl_secs.max(1));
        state.agents.retain_mut(|entry| {
            if entry.busy {
                return true;
            }

            // Reap an idle agent that already exited (`try_wait` on liveness).
            let alive = entry
                .client
                .as_mut()
                .map(PiRpcClient::is_alive)
                .unwrap_or(false);
            if !alive {
                entry.client = None;
                return false;
            }

            if entry.last_used.elapsed() >= ttl {
                tracing::info!(
                    key = %entry.key,
                    workspace = %entry.workspace.display(),
                    pid = ?entry.pid,
                    "evicting idle pi agent"
                );
                if let Some(client) = entry.client.as_mut() {
                    client.kill();
                }
                entry.client = None;
                return false;
            }
            true
        });

        // A mapping is stale once its agent is gone. Dropping it lets the next
        // mention in that thread start a fresh agent instead of waiting for a
        // process that will never return.
        let live: HashSet<Uuid> = state.agents.iter().map(|entry| entry.id).collect();
        let before = state.conversations.len();
        state.conversations.retain(|_, id| live.contains(id));
        let evicted = before - state.conversations.len();
        if evicted > 0 {
            tracing::debug!(evicted, "evicted stale conversation mappings");
        }
    }

    /// Check out a client for `key`, spawning one if necessary.
    async fn acquire(
        self: &Arc<Self>,
        key: &str,
        workspace: &Path,
        credentials: &[(String, String)],
        executor: &crate::executor::Executor,
        host_user: Option<&str>,
        model: Option<&str>,
    ) -> Result<PoolGuard> {
        let deadline = (self.config.timeout_secs != 0)
            .then(|| Instant::now() + Duration::from_secs(self.config.timeout_secs));

        loop {
            // Register before inspecting the pool so a release between the
            // inspection and the wait cannot be missed.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut state = self.state.lock().expect("pi pool mutex poisoned");
                self.reap(&mut state);

                let preferred = state.conversations.get(key).copied();
                let idle = state
                    .agents
                    .iter()
                    .position(|entry| {
                        Some(entry.id) == preferred
                            && entry.workspace == workspace
                            && entry.host_user.as_deref() == host_user
                            && entry.model.as_deref() == model
                            && !entry.busy
                            && entry.client.is_some()
                    })
                    .or_else(|| {
                        // A process that already holds another conversation's
                        // session must not be handed to this one unless the
                        // operator opted out of per-conversation sessions.
                        if self.config.session_per_conversation {
                            return None;
                        }
                        state.agents.iter().position(|entry| {
                            entry.workspace == workspace
                                && entry.host_user.as_deref() == host_user
                                && entry.model.as_deref() == model
                                && !entry.busy
                                && entry.client.is_some()
                        })
                    });
                if let Some(index) = idle {
                    let entry = &mut state.agents[index];
                    entry.busy = true;
                    entry.key = key.to_owned();
                    let id = entry.id;
                    let pid = entry.pid;
                    let client = entry.client.take();
                    state.conversations.insert(key.to_owned(), id);
                    tracing::debug!(key, pid = ?pid, "reusing idle pi agent");
                    return Ok(PoolGuard {
                        inner: Arc::clone(self),
                        id,
                        client,
                    });
                }

                // A live process cannot change its working directory. Make
                // room for this workspace when the pool is full of idle
                // processes belonging to other workspaces.
                if state.agents.len() >= self.max_agents
                    && let Some(index) = state.agents.iter().position(|entry| !entry.busy)
                {
                    let mut evicted = state.agents.remove(index);
                    if let Some(client) = evicted.client.as_mut() {
                        client.kill();
                    }
                    state.conversations.retain(|_, id| *id != evicted.id);
                }

                if state.agents.len() < self.max_agents {
                    let id = Uuid::new_v4();
                    let session_id = (!self.config.no_session)
                        .then(|| self.sessions.deterministic_id("pi-rpc", key));
                    let client = PiRpcClient::spawn(
                        &self.config,
                        workspace,
                        credentials,
                        session_id.as_deref(),
                        executor,
                        host_user,
                        model,
                    )?;
                    let pid = client.pid();
                    let writer = Some(client.writer());
                    state.agents.push(PoolEntry {
                        id,
                        key: key.to_owned(),
                        pid,
                        workspace: workspace.to_path_buf(),
                        host_user: host_user.map(str::to_owned),
                        model: model.map(str::to_owned),
                        client: None,
                        writer,
                        busy: true,
                        last_used: Instant::now(),
                    });
                    state.conversations.insert(key.to_owned(), id);
                    tracing::info!(key, pid = ?pid, max = self.max_agents, "spawned pi agent");
                    return Ok(PoolGuard {
                        inner: Arc::clone(self),
                        id,
                        client: Some(client),
                    });
                }
            }

            match deadline {
                Some(deadline) => {
                    let remaining =
                        deadline
                            .checked_duration_since(Instant::now())
                            .ok_or_else(|| BotError::Agent {
                                name: "pi-rpc".into(),
                                reason: "timed out waiting for an idle pi agent".into(),
                            })?;
                    if tokio::time::timeout(remaining, notified).await.is_err() {
                        return Err(BotError::Agent {
                            name: "pi-rpc".into(),
                            reason: "timed out waiting for an idle pi agent".into(),
                        });
                    }
                }
                None => notified.await,
            }
        }
    }
}

/// A checked-out agent. Returning it to the pool happens on drop.
pub struct PoolGuard {
    inner: Arc<PoolInner>,
    id: Uuid,
    client: Option<PiRpcClient>,
}

impl PoolGuard {
    fn client_mut(&mut self) -> Result<&mut PiRpcClient> {
        self.client.as_mut().ok_or_else(|| BotError::Agent {
            name: "pi-rpc".into(),
            reason: "agent was invalidated".into(),
        })
    }

    /// Drop the underlying client instead of returning it to the pool.
    fn invalidate(&mut self) {
        if let Some(mut client) = self.client.take() {
            client.kill();
        }
    }
}

impl Drop for PoolGuard {
    fn drop(&mut self) {
        let client = self.client.take();
        {
            let mut state = self.inner.state.lock().expect("pi pool mutex poisoned");
            if let Some(entry) = state.agents.iter_mut().find(|entry| entry.id == self.id) {
                entry.last_used = Instant::now();
                entry.busy = false;

                let kept = match client {
                    Some(client) => {
                        let mut client = client;
                        if client.is_alive() {
                            entry.client = Some(client);
                            true
                        } else {
                            entry.client = None;
                            false
                        }
                    }
                    None => {
                        entry.client = None;
                        false
                    }
                };
                if !kept {
                    state.agents.retain(|entry| entry.id != self.id);
                    state.conversations.retain(|_, id| *id != self.id);
                }
            }
        }
        self.inner.notify.notify_waiters();
    }
}

/// Pooled Pi RPC adapter.
pub struct PiPoolAgent {
    inner: Arc<PoolInner>,
}

impl PiPoolAgent {
    /// Build the pool. `max_agents` is the single global `[session] workers`
    /// count; there is no separate pool limit.
    pub fn new(config: &PiRpcConfig, sessions: Arc<SessionStore>, max_agents: usize) -> Self {
        Self {
            inner: Arc::new(PoolInner {
                config: config.clone(),
                max_agents: max_agents.max(1),
                sessions,
                state: Mutex::new(PoolState::default()),
                notify: Notify::new(),
            }),
        }
    }

    /// Number of live agents (mainly useful for tests/diagnostics).
    pub fn live_agents(&self) -> usize {
        self.inner
            .state
            .lock()
            .expect("pi pool mutex poisoned")
            .agents
            .len()
    }

    /// Agent currently bound to a conversation key, if any. Internal data used
    /// to keep a thread on one instance; it is never sent to an agent.
    pub fn conversation_binding(&self, key: &str) -> Option<Uuid> {
        self.inner
            .state
            .lock()
            .expect("pi pool mutex poisoned")
            .conversations
            .get(key)
            .copied()
    }
}

#[async_trait::async_trait]
impl Agent for PiPoolAgent {
    fn name(&self) -> &str {
        "pi-rpc"
    }

    async fn run(&self, request: &AgentRequest, context: &AgentContext) -> Result<AgentOutcome> {
        let started = Instant::now();
        let key = conversation_key(context);

        let mut guard = self
            .inner
            .acquire(
                &key,
                &context.workspace,
                &context.credentials,
                context.executor.as_ref(),
                context.host_user.as_deref(),
                context.model.as_deref(),
            )
            .await?;
        let prompt = build_prompt(request, context);
        let timeout = (self.inner.config.timeout_secs != 0)
            .then(|| Duration::from_secs(self.inner.config.timeout_secs));

        let model = guard.client_mut()?.current_model().await.unwrap_or(None);
        *context.reported_model.lock().expect("model mutex poisoned") = model.clone();
        match guard
            .client_mut()?
            .prompt_with_output(&prompt, timeout, context.live_output.as_ref())
            .await
        {
            Ok((text, usage)) => {
                let mut outcome = AgentOutcome::success(text, started.elapsed());
                outcome.model = model;
                outcome.usage = (usage.prompt_tokens > 0).then_some(usage);
                Ok(outcome)
            }
            Err(error) => {
                guard.invalidate();
                let mut outcome = AgentOutcome::failure(error.to_string(), started.elapsed());
                outcome.model = model;
                Ok(outcome)
            }
        }
    }

    async fn follow_up(
        &self,
        request: &AgentRequest,
        context: &AgentContext,
    ) -> Result<Option<SteerReceipt>> {
        let key = conversation_key(context);
        let writer = {
            let state = self.inner.state.lock().expect("pi pool mutex poisoned");
            state
                .agents
                .iter()
                .find(|entry| entry.busy && entry.key == key)
                .and_then(|entry| entry.writer.clone())
        };
        let Some(writer) = writer else {
            return Ok(None);
        };

        writer.send(json!({
            "type": "steer",
            "message": build_follow_up_prompt(request),
        }))?;
        tracing::info!(key, "injected a steer into a live pi agent");
        Ok(Some(SteerReceipt::merged()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::IssueRef;
    use crate::location::ForgeKind;

    fn cfg() -> PiRpcConfig {
        PiRpcConfig {
            command: "cat".into(),
            timeout_secs: 5,
            ..Default::default()
        }
    }

    fn store() -> Arc<SessionStore> {
        Arc::new(SessionStore::default())
    }

    #[test]
    fn normalizes_pi_usage() {
        let usage =
            assistant_usage(&json!({"input": 100, "cacheRead": 900, "output": 12})).unwrap();
        assert_eq!(usage.prompt_tokens, 1_000);
        assert_eq!(usage.cached_tokens, 900);
        assert!((usage.hit_rate().unwrap() - 90.0).abs() < f64::EPSILON);
        // A tool-result message carries no prompt usage and must be ignored.
        assert_eq!(assistant_usage(&json!({"output": 1})), None);
    }

    #[test]
    fn rpc_arguments_persist_a_session_id_by_default() {
        let config = PiRpcConfig {
            approve: true,
            no_session: false,
            ..Default::default()
        };
        let args = rpc_arguments(&config, Some("session-123"), None);
        assert_eq!(&args[..2], ["--mode", "rpc"]);
        assert!(args.contains(&"--approve".to_owned()));
        assert!(args.contains(&"--session-id".to_owned()));
        assert!(args.contains(&"session-123".to_owned()));
        assert!(!args.contains(&"--no-session".to_owned()));
    }

    #[test]
    fn rpc_arguments_apply_a_per_user_model() {
        let config = PiRpcConfig::default();
        let args = rpc_arguments(&config, None, Some("gpt-fast"));
        assert!(args.windows(2).any(|w| w == ["--model", "gpt-fast"]));

        // An operator-configured model wins.
        let config = PiRpcConfig {
            args: vec!["--model".into(), "operator".into()],
            ..Default::default()
        };
        let args = rpc_arguments(&config, None, Some("gpt-fast"));
        assert!(!args.contains(&"gpt-fast".to_owned()));
        assert!(args.contains(&"operator".to_owned()));
    }

    #[test]
    fn rpc_arguments_are_ephemeral_when_sessions_are_disabled() {
        let config = PiRpcConfig {
            no_session: true,
            ..Default::default()
        };
        let args = rpc_arguments(&config, Some("session-123"), None);
        assert!(args.contains(&"--no-session".to_owned()));
        assert!(!args.iter().any(|arg| arg == "--session-id"));
    }

    #[test]
    fn session_ids_are_deterministic_per_conversation() {
        let agent = PiPoolAgent::new(&cfg(), store(), 1);
        let first = agent
            .inner
            .sessions
            .deterministic_id("pi-rpc", "forgejo:o/r:1");
        let again = agent
            .inner
            .sessions
            .deterministic_id("pi-rpc", "forgejo:o/r:1");
        let other = agent
            .inner
            .sessions
            .deterministic_id("pi-rpc", "forgejo:o/r:2");
        assert_eq!(first, again);
        assert_ne!(first, other);
    }

    #[test]
    fn prompt_includes_request() {
        let request = AgentRequest {
            location: url::Url::parse("https://forge.example.com/o/r/issues/1").unwrap(),
            message: "fix the bug".into(),
        };
        let context = AgentContext {
            repository: "o/r".into(),
            requester: "alice".into(),
            issue_number: Some(1),
            ..Default::default()
        };
        let prompt = build_prompt(&request, &context);
        assert!(prompt.contains("fix the bug"));
        assert!(prompt.contains("o/r"));
        assert!(prompt.contains("request a review from the caller (@alice)"));
        assert!(prompt.contains("Reply on the forge when you are done"));
    }

    #[test]
    fn reaps_dead_idle_agents_but_keeps_busy_ones() {
        let agent = PiPoolAgent::new(&cfg(), store(), 1);
        let mut state = agent.inner.state.lock().unwrap();
        state.agents.push(PoolEntry {
            id: Uuid::new_v4(),
            key: "dead".into(),
            pid: None,
            workspace: PathBuf::from("/tmp"),
            host_user: None,
            model: None,
            client: None,
            writer: None,
            busy: false,
            last_used: Instant::now(),
        });
        state.agents.push(PoolEntry {
            id: Uuid::new_v4(),
            key: "busy".into(),
            pid: None,
            workspace: PathBuf::from("/tmp"),
            host_user: None,
            model: None,
            client: None,
            writer: None,
            busy: true,
            last_used: Instant::now(),
        });
        agent.inner.reap(&mut state);
        assert_eq!(state.agents.len(), 1);
        assert_eq!(state.agents[0].key, "busy");
    }

    #[test]
    fn reaps_stale_conversation_mappings() {
        let agent = PiPoolAgent::new(&cfg(), store(), 1);
        let mut state = agent.inner.state.lock().unwrap();
        let kept = Uuid::new_v4();
        state.agents.push(PoolEntry {
            id: kept,
            key: "live".into(),
            pid: None,
            workspace: PathBuf::from("/tmp"),
            host_user: None,
            model: None,
            client: None,
            writer: None,
            busy: true,
            last_used: Instant::now(),
        });
        state.conversations.insert("live".into(), kept);
        state.conversations.insert("stale".into(), Uuid::new_v4());

        agent.inner.reap(&mut state);

        assert_eq!(state.conversations.len(), 1);
        assert_eq!(state.conversations.get("live"), Some(&kept));
        assert!(!state.conversations.contains_key("stale"));
    }

    // This executable is checked in, so concurrent tests never fork while
    // another test is still writing the inode they are about to execute.
    fn fake_pi_command() -> String {
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake_pi_rpc.py").into()
    }

    #[tokio::test]
    async fn busy_thread_spawns_another_agent_then_reuses_idle_one() {
        let dir = tempfile::tempdir().unwrap();

        let mut config = PiRpcConfig {
            command: fake_pi_command(),
            timeout_secs: 10,
            // Legacy workspace-level reuse: an idle process may serve a
            // different conversation and carry its session.
            session_per_conversation: false,
            ..Default::default()
        };
        config.env.insert("FAKE_PI_DELAY".into(), "1".into());
        let agent = Arc::new(PiPoolAgent::new(&config, store(), 4));

        let request = AgentRequest {
            location: url::Url::parse("http://forge.local/o/r/issues/1").unwrap(),
            message: "go".into(),
        };
        let context = AgentContext {
            workspace: dir.path().to_path_buf(),
            forge: Some(ForgeKind::Forgejo),
            repository: "o/r".into(),
            issue_number: Some(1),
            ..Default::default()
        };
        let key = conversation_key(&context);

        let spawn = |agent: Arc<PiPoolAgent>| {
            let request = request.clone();
            let context = context.clone();
            tokio::spawn(async move { agent.run(&request, &context).await })
        };

        let first = spawn(Arc::clone(&agent));
        // Wait for the first request to spawn and bind its agent. Poll
        // instead of sleeping a fixed amount, so the test is not
        // timing-sensitive when the suite runs under load.
        let mut bound = None;
        for _ in 0..500 {
            if let Some(id) = agent.conversation_binding(&key) {
                bound = Some(id);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let bound = bound.expect("agent should be bound to the thread");
        assert_eq!(agent.live_agents(), 1);

        // The bound agent is busy, so the second request starts another one.
        let second = spawn(Arc::clone(&agent));
        for _ in 0..500 {
            if agent.live_agents() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(agent.live_agents(), 2);
        assert_ne!(agent.conversation_binding(&key), Some(bound));

        assert!(first.await.unwrap().unwrap().success);
        assert!(second.await.unwrap().unwrap().success);
        assert_eq!(agent.live_agents(), 2);

        // A different thread in the same workspace uses a free process.
        let mut other_context = context.clone();
        other_context.issue_number = Some(2);
        let outcome = agent.run(&request, &other_context).await.unwrap();
        assert!(outcome.success);
        assert_eq!(agent.live_agents(), 2);
        assert!(
            agent
                .conversation_binding(&conversation_key(&other_context))
                .is_some()
        );
    }

    #[tokio::test]
    async fn waits_only_when_pool_is_full() {
        let dir = tempfile::tempdir().unwrap();

        let mut config = PiRpcConfig {
            command: fake_pi_command(),
            timeout_secs: 10,
            ..Default::default()
        };
        config.env.insert("FAKE_PI_DELAY".into(), "1".into());
        let agent = Arc::new(PiPoolAgent::new(&config, store(), 1));
        let workspace = dir.path();
        let first = agent
            .inner
            .acquire(
                "first",
                workspace,
                &[],
                &crate::executor::Executor::direct(),
                None,
                None,
            )
            .await
            .unwrap();
        let first_id = first.id;

        let waiting_agent = Arc::clone(&agent);
        let waiting_workspace = workspace.to_path_buf();
        let waiter = tokio::spawn(async move {
            waiting_agent
                .inner
                .acquire(
                    "second",
                    &waiting_workspace,
                    &[],
                    &crate::executor::Executor::direct(),
                    None,
                    None,
                )
                .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiter.is_finished());
        assert_eq!(agent.live_agents(), 1);

        drop(first);
        let second = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        // By default the freed process belongs to another conversation, so it
        // is evicted and a fresh one is spawned for `second`.
        assert_ne!(second.id, first_id);
        assert_eq!(agent.conversation_binding("first"), None);
        assert_eq!(agent.live_agents(), 1);
    }

    #[tokio::test]
    async fn reuses_idle_agent_before_spawning_for_busy_thread() {
        let dir = tempfile::tempdir().unwrap();

        let config = PiRpcConfig {
            command: fake_pi_command(),
            // Legacy workspace-level reuse, enabled explicitly here.
            session_per_conversation: false,
            ..Default::default()
        };
        let agent = PiPoolAgent::new(&config, store(), 3);
        let workspace = dir.path();
        let bound = agent
            .inner
            .acquire(
                "thread",
                workspace,
                &[],
                &crate::executor::Executor::direct(),
                None,
                None,
            )
            .await
            .unwrap();
        let other = agent
            .inner
            .acquire(
                "other",
                workspace,
                &[],
                &crate::executor::Executor::direct(),
                None,
                None,
            )
            .await
            .unwrap();
        let other_id = other.id;
        drop(other);

        let reused = agent
            .inner
            .acquire(
                "thread",
                workspace,
                &[],
                &crate::executor::Executor::direct(),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(reused.id, other_id);
        assert_eq!(agent.live_agents(), 2);
        assert_eq!(agent.conversation_binding("thread"), Some(other_id));
        drop(bound);
    }

    #[tokio::test]
    async fn starts_a_new_process_for_a_new_conversation_by_default() {
        let dir = tempfile::tempdir().unwrap();

        let config = PiRpcConfig {
            command: fake_pi_command(),
            ..Default::default()
        };
        assert!(config.session_per_conversation);
        let agent = PiPoolAgent::new(&config, store(), 3);
        let workspace = dir.path();

        let first = agent
            .inner
            .acquire(
                "first",
                workspace,
                &[],
                &crate::executor::Executor::direct(),
                None,
                None,
            )
            .await
            .unwrap();
        let first_id = first.id;
        drop(first);

        // The process bound to `first` is idle, but a different conversation
        // must not inherit its session: a new process is spawned instead.
        let second = agent
            .inner
            .acquire(
                "second",
                workspace,
                &[],
                &crate::executor::Executor::direct(),
                None,
                None,
            )
            .await
            .unwrap();
        assert_ne!(second.id, first_id);
        assert_eq!(agent.live_agents(), 2);
        assert_eq!(agent.conversation_binding("first"), Some(first_id));
        assert_eq!(agent.conversation_binding("second"), Some(second.id));
    }

    #[tokio::test]
    async fn replays_a_conversation_on_its_own_process() {
        let dir = tempfile::tempdir().unwrap();

        let config = PiRpcConfig {
            command: fake_pi_command(),
            ..Default::default()
        };
        let agent = PiPoolAgent::new(&config, store(), 3);
        let workspace = dir.path();

        let first = agent
            .inner
            .acquire(
                "thread",
                workspace,
                &[],
                &crate::executor::Executor::direct(),
                None,
                None,
            )
            .await
            .unwrap();
        let first_id = first.id;
        drop(first);

        // The same conversation always comes back to its own process, so its
        // session stays warm.
        let again = agent
            .inner
            .acquire(
                "thread",
                workspace,
                &[],
                &crate::executor::Executor::direct(),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(again.id, first_id);
        assert_eq!(agent.live_agents(), 1);
    }

    #[tokio::test]
    async fn replaces_idle_agent_from_another_workspace_when_full() {
        let dir = tempfile::tempdir().unwrap();

        let config = PiRpcConfig {
            command: fake_pi_command(),
            ..Default::default()
        };
        let agent = PiPoolAgent::new(&config, store(), 1);
        let first_workspace = dir.path();
        let second_workspace = dir.path().join("second");
        std::fs::create_dir(&second_workspace).unwrap();
        let first = agent
            .inner
            .acquire(
                "first",
                first_workspace,
                &[],
                &crate::executor::Executor::direct(),
                None,
                None,
            )
            .await
            .unwrap();
        let first_id = first.id;
        drop(first);

        let second = agent
            .inner
            .acquire(
                "second",
                &second_workspace,
                &[],
                &crate::executor::Executor::direct(),
                None,
                None,
            )
            .await
            .unwrap();
        assert_ne!(second.id, first_id);
        assert_eq!(agent.live_agents(), 1);
        assert_eq!(agent.conversation_binding("first"), None);
    }

    #[tokio::test]
    async fn disabled_timeout_lets_a_slow_agent_finish() {
        let dir = tempfile::tempdir().unwrap();

        let mut config = PiRpcConfig {
            command: fake_pi_command(),
            timeout_secs: 0,
            ..Default::default()
        };
        config.env.insert("FAKE_PI_DELAY".into(), "1".into());
        let agent = PiPoolAgent::new(&config, store(), 1);

        let request = AgentRequest {
            location: url::Url::parse("http://forge.local/o/r/issues/1").unwrap(),
            message: "go".into(),
        };
        let context = AgentContext {
            workspace: dir.path().to_path_buf(),
            forge: Some(ForgeKind::Forgejo),
            repository: "o/r".into(),
            issue_number: Some(1),
            ..Default::default()
        };

        // With the limit disabled the run has no deadline and must wait for
        // the fake agent to settle instead of failing.
        let outcome = agent.run(&request, &context).await.unwrap();
        assert!(outcome.success);
        assert_eq!(outcome.summary, "fake-result");
    }

    #[tokio::test]
    async fn reports_pi_prompt_cache_usage() {
        let dir = tempfile::tempdir().unwrap();
        let config = PiRpcConfig {
            command: fake_pi_command(),
            timeout_secs: 5,
            ..Default::default()
        };
        let agent = PiPoolAgent::new(&config, store(), 1);

        let request = AgentRequest {
            location: url::Url::parse("http://forge.local/o/r/issues/1").unwrap(),
            message: "go".into(),
        };
        let context = AgentContext {
            workspace: dir.path().to_path_buf(),
            forge: Some(ForgeKind::Forgejo),
            repository: "o/r".into(),
            issue_number: Some(1),
            ..Default::default()
        };

        let outcome = agent.run(&request, &context).await.unwrap();
        let usage = outcome.usage.expect("pi reports prompt usage");
        assert_eq!(usage.prompt_tokens, 1_000);
        assert_eq!(usage.cached_tokens, 900);
        assert_eq!(usage.hit_rate(), Some(90.0));
    }

    #[tokio::test]
    async fn injects_a_follow_up_into_a_waiting_run_without_spawning() {
        let dir = tempfile::tempdir().unwrap();
        let steer_log = dir.path().join("steer.log");
        let prompt_log = dir.path().join("prompt.log");
        let result = dir.path().join("result.txt");

        let mut config = PiRpcConfig {
            command: fake_pi_command(),
            timeout_secs: 10,
            ..Default::default()
        };
        config
            .env
            .insert("FAKE_PI_WAIT_FOR_STEER".into(), "1".into());
        config
            .env
            .insert("FAKE_PI_STEER_LOG".into(), steer_log.display().to_string());
        config.env.insert(
            "FAKE_PI_PROMPT_LOG".into(),
            prompt_log.display().to_string(),
        );
        config
            .env
            .insert("FAKE_PI_RESULT".into(), result.display().to_string());
        config
            .env
            .insert("FAKE_PI_STREAM_TEXT".into(), "working now".into());
        let agent = Arc::new(PiPoolAgent::new(&config, store(), 2));

        let request = AgentRequest {
            location: url::Url::parse("http://forge.local/o/r/issues/1").unwrap(),
            message: "go".into(),
        };
        let output = LiveOutput::default();
        let context = AgentContext {
            workspace: dir.path().to_path_buf(),
            forge: Some(ForgeKind::Forgejo),
            repository: "o/r".into(),
            issue_number: Some(1),
            live_output: Some(output.clone()),
            ..Default::default()
        };
        let key = conversation_key(&context);

        let run = {
            let agent = Arc::clone(&agent);
            let request = request.clone();
            let context = context.clone();
            tokio::spawn(async move { agent.run(&request, &context).await })
        };

        // Wait until the run has acquired the process and is waiting for input.
        let mut bound = None;
        for _ in 0..500 {
            if let Some(id) = agent.conversation_binding(&key) {
                bound = Some(id);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        bound.expect("agent should be bound to the thread");
        for _ in 0..500 {
            if prompt_log.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(prompt_log.exists(), "agent should have received the prompt");

        for _ in 0..100 {
            if output.text().contains("working now") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(output.text().contains("working now"));
        assert!(!run.is_finished(), "Pi text should appear during the run");

        let follow_up_request = AgentRequest {
            location: request.location.clone(),
            message: "also run the linter".into(),
        };
        let receipt = agent
            .follow_up(&follow_up_request, &context)
            .await
            .unwrap()
            .expect("a live run should accept the follow-up");
        assert!(receipt.notice.contains("Merged"));

        let outcome = run.await.unwrap().unwrap();
        assert!(outcome.success);
        assert_eq!(outcome.model.as_deref(), Some("test/fake-pi"));
        // The follow-up went to the same process; no second agent was spawned.
        assert_eq!(agent.live_agents(), 1);
        let logged = std::fs::read_to_string(&steer_log).unwrap();
        assert!(logged.contains("also run the linter"), "{logged}");
        assert!(
            outcome.summary.contains("also run the linter"),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn follow_up_without_a_live_run_is_not_accepted() {
        let config = PiRpcConfig {
            command: fake_pi_command(),
            ..Default::default()
        };
        let agent = PiPoolAgent::new(&config, store(), 2);
        let request = AgentRequest {
            location: url::Url::parse("http://forge.local/o/r/issues/1").unwrap(),
            message: "go".into(),
        };
        let context = AgentContext {
            repository: "o/r".into(),
            issue_number: Some(1),
            ..Default::default()
        };

        // Nothing is running, so the adapter defers to the queue.
        assert!(agent.follow_up(&request, &context).await.unwrap().is_none());
    }

    #[test]
    fn conversation_key_folds_pr_onto_linked_issue() {
        let pr = AgentContext {
            forge: Some(ForgeKind::Forgejo),
            repository: "o/r".into(),
            issue_number: Some(12),
            is_pull_request: true,
            linked_issue: Some(IssueRef {
                repository: None,
                number: 5,
            }),
            ..Default::default()
        };
        assert_eq!(conversation_key(&pr), "forgejo:o/r:5");

        let unlinked = AgentContext {
            forge: Some(ForgeKind::Forgejo),
            repository: "o/r".into(),
            issue_number: Some(12),
            is_pull_request: true,
            linked_issue: None,
            ..Default::default()
        };
        assert_eq!(conversation_key(&unlinked), "forgejo:o/r:12");

        let issue = AgentContext {
            forge: Some(ForgeKind::Forgejo),
            repository: "o/r".into(),
            issue_number: Some(5),
            ..Default::default()
        };
        assert_eq!(conversation_key(&issue), "forgejo:o/r:5");

        // A cross-repository link keeps the linked issue's owner/repo, so the
        // PR and the issue it closes share one key.
        let cross_repo = AgentContext {
            forge: Some(ForgeKind::Forgejo),
            repository: "o/r".into(),
            issue_number: Some(12),
            is_pull_request: true,
            linked_issue: Some(IssueRef {
                repository: Some("other/repo".into()),
                number: 5,
            }),
            ..Default::default()
        };
        assert_eq!(conversation_key(&cross_repo), "forgejo:other/repo:5");
    }
}

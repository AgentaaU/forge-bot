//! Codex app-server adapter with same-turn steering.
//!
//! `codex app-server` is a long-lived JSON process (not JSON-RPC 2.0) that
//! supports `turn/steer`: a follow-up is appended to the active turn without
//! starting a new one. This adapter drives one app-server process per run,
//! persists the Codex thread id so later comments resume the same conversation,
//! and keeps the process reachable from [`Agent::follow_up`] while its turn is
//! in flight.
//!
//! Older Codex builds and operator-supplied `command`/`args` never reach this
//! module: [`crate::agent::codex::build`] keeps the one-shot `codex exec`
//! adapter for them, and this adapter reports an [`wire::unsupported`] error so
//! the wrapper can fall back when the installed binary has no app-server.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::Instant;

use crate::agent::prompt::{build_follow_up_prompt, build_prompt};
use crate::agent::session::SessionStore;
use crate::agent::wire::{self, LiveRegistration, WireProcess};
use crate::agent::{
    AgentContext, AgentOutcome, AgentRequest, LiveOutput, SteerReceipt, TokenUsage,
    conversation_key,
};
use crate::config::AgentConfig;
use crate::error::{BotError, Result};
use crate::executor::ExecSpec;

/// How long a live turn has to acknowledge a `turn/steer` before the delivery
/// is treated as uncertain. An explicit rejection is still queued.
const STEER_ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// A live app-server process bound to one conversation.
struct LiveSession {
    process: Arc<WireProcess>,
    thread_id: Mutex<Option<String>>,
    /// Turn currently accepting steers, if any.
    active_turn: Mutex<Option<String>>,
}

/// Codex app-server configuration derived from `[agents.codex]`.
#[derive(Debug, Clone, Default)]
pub struct CodexAppServerConfig {
    pub program: String,
    pub env: std::collections::BTreeMap<String, String>,
    pub timeout: Option<Duration>,
}

impl CodexAppServerConfig {
    fn from_agent(config: &AgentConfig) -> Self {
        Self {
            program: config.command.clone().unwrap_or_else(|| "codex".to_owned()),
            env: config.env.clone(),
            timeout: config
                .timeout_secs
                .filter(|seconds| *seconds != 0)
                .map(Duration::from_secs),
        }
    }
}

/// A Codex app-server-backed agent.
pub struct CodexAppServerAgent {
    config: CodexAppServerConfig,
    sessions: Arc<SessionStore>,
    live: Mutex<HashMap<String, Arc<LiveSession>>>,
}

impl CodexAppServerAgent {
    pub fn new(config: &AgentConfig, sessions: Arc<SessionStore>) -> Self {
        Self {
            config: CodexAppServerConfig::from_agent(config),
            sessions,
            live: Mutex::new(HashMap::new()),
        }
    }

    /// Identity of a live run. Reusing a process is only valid for the same
    /// workspace, account and model; anything else must not be steered because
    /// Codex cannot change those mid-turn.
    fn signature(&self, context: &AgentContext) -> String {
        format!(
            "{}|{}|{}",
            context.workspace.display(),
            context.host_user.as_deref().unwrap_or_default(),
            context.model.as_deref().unwrap_or_default(),
        )
    }

    fn map_key(&self, context: &AgentContext) -> String {
        format!(
            "{}\u{1e}{}",
            conversation_key(context),
            self.signature(context)
        )
    }

    /// Number of live app-server processes (diagnostics and tests).
    pub fn live_agents(&self) -> usize {
        self.live.lock().expect("codex live mutex poisoned").len()
    }

    fn exec_spec(&self, request: &AgentRequest, context: &AgentContext) -> ExecSpec {
        let mut env: Vec<(String, String)> = self
            .config
            .env
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        env.extend(context.environment(request));
        ExecSpec {
            program: self.config.program.clone(),
            args: vec![
                "app-server".to_owned(),
                "--listen".to_owned(),
                "stdio://".to_owned(),
            ],
            env,
            cwd: (!context.workspace.as_os_str().is_empty()).then(|| context.workspace.clone()),
            host_user: context.host_user.clone(),
        }
    }

    pub async fn run(
        &self,
        request: &AgentRequest,
        context: &AgentContext,
    ) -> Result<AgentOutcome> {
        let started = Instant::now();
        if !context.workspace.as_os_str().is_empty() {
            context
                .executor
                .create_dir_all(&context.workspace, context.host_user.as_deref())
                .await?;
        }
        let prompt = build_prompt(request, context);
        let key = conversation_key(context);
        let map_key = self.map_key(context);

        // Drop a stale process for this exact run identity, if any.
        if let Some(stale) = self
            .live
            .lock()
            .expect("codex live mutex poisoned")
            .remove(&map_key)
        {
            stale.process.kill();
        }

        let spec = self.exec_spec(request, context);
        let process = WireProcess::spawn("codex", false, &spec, context.executor.as_ref())?;
        let live = Arc::new(LiveSession {
            process: Arc::clone(&process),
            thread_id: Mutex::new(None),
            active_turn: Mutex::new(None),
        });
        // Registered only for the turn. If this future is cancelled (a
        // terminated thread) the registration is dropped with it, which
        // unregisters and kills the process before the cancellation returns.
        let mut result = {
            let _registration = LiveRegistration::new(
                &self.live,
                map_key.clone(),
                Arc::clone(&live),
                Arc::clone(&process),
            );
            self.run_turn(&process, &live, &prompt, &key, context, started)
                .await
        };
        if let Ok(outcome) = &mut result {
            outcome.model = context
                .reported_model
                .lock()
                .expect("model mutex poisoned")
                .clone();
        }
        result
    }

    async fn run_turn(
        &self,
        process: &WireProcess,
        live: &LiveSession,
        prompt: &str,
        key: &str,
        context: &AgentContext,
        started: Instant,
    ) -> Result<AgentOutcome> {
        let deadline = self.config.timeout.map(|timeout| Instant::now() + timeout);
        process
            .request(
                "initialize",
                json!({
                    "clientInfo": {
                        "name": "forge-bot",
                        "version": env!("CARGO_PKG_VERSION"),
                    }
                }),
                deadline,
            )
            .await
            .map_err(|error| wire::unsupported("codex", error))?;

        let thread_id = self
            .open_thread(process, live, key, context, deadline)
            .await?;
        let effort = turn_effort(process, context, deadline).await;

        // Subscribe before `turn/start` so no part of the turn is missed.
        let mut events = process.subscribe();
        let mut turn = json!({
            "threadId": thread_id,
            "input": [{ "type": "text", "text": prompt }],
        });
        if let Some(effort) = effort {
            turn["effort"] = json!(effort);
        }
        let response = process
            .request("turn/start", turn, deadline)
            .await
            .map_err(|error| BotError::Agent {
                name: "codex".into(),
                reason: error.to_string(),
            })?;
        let turn_id = response["turn"]["id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        if turn_id.is_empty() {
            return Ok(AgentOutcome::failure(
                "codex app-server did not return a turn id".to_owned(),
                started.elapsed(),
            ));
        }
        *live.active_turn.lock().expect("codex turn mutex poisoned") = Some(turn_id.clone());

        let mut text = String::new();
        let mut usage = TokenUsage::default();
        let turn = loop {
            let event = match deadline {
                Some(deadline) => {
                    match tokio::time::timeout_at(deadline, process.next_event(&mut events)).await {
                        Ok(Some(event)) => event,
                        Ok(None) => {
                            *live.active_turn.lock().expect("codex turn mutex poisoned") = None;
                            return Ok(AgentOutcome::failure(
                                "codex app-server exited before the turn completed".to_owned(),
                                started.elapsed(),
                            ));
                        }
                        Err(_) => {
                            let _ = process
                                .request(
                                    "turn/interrupt",
                                    json!({ "threadId": thread_id, "turnId": turn_id }),
                                    Some(Instant::now() + Duration::from_secs(2)),
                                )
                                .await;
                            *live.active_turn.lock().expect("codex turn mutex poisoned") = None;
                            let timeout = self.config.timeout.unwrap_or_default();
                            return Ok(AgentOutcome::failure(
                                format!("agent timed out after {timeout:?}"),
                                started.elapsed(),
                            ));
                        }
                    }
                }
                None => match process.next_event(&mut events).await {
                    Some(event) => event,
                    None => {
                        *live.active_turn.lock().expect("codex turn mutex poisoned") = None;
                        return Ok(AgentOutcome::failure(
                            "codex app-server exited before the turn completed".to_owned(),
                            started.elapsed(),
                        ));
                    }
                },
            };
            match event["method"].as_str().unwrap_or_default() {
                "turn/completed" => {
                    if event["params"]["turn"]["id"].as_str() == Some(turn_id.as_str()) {
                        break event["params"]["turn"].clone();
                    }
                }
                "item/agentMessage/delta" => {
                    if event["params"]["turnId"].as_str() == Some(turn_id.as_str())
                        && let Some(delta) = event["params"]["delta"].as_str()
                    {
                        text.push_str(delta);
                        append_live(&context.live_output, delta);
                    }
                }
                "thread/tokenUsage/updated" => {
                    if event["params"]["turnId"].as_str() == Some(turn_id.as_str()) {
                        let last = &event["params"]["tokenUsage"]["last"];
                        usage = TokenUsage {
                            prompt_tokens: last["inputTokens"].as_u64().unwrap_or(0),
                            cached_tokens: last["cachedInputTokens"].as_u64().unwrap_or(0),
                        };
                    }
                }
                _ if event.get("id").is_some() => answer_server_request(process, &event),
                _ => {}
            }
        };
        *live.active_turn.lock().expect("codex turn mutex poisoned") = None;

        // The completed turn carries the authoritative final message.
        let final_text = turn
            .get("items")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter(|item| item["type"] == "agentMessage")
                    .filter_map(|item| item["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(text);

        let status = turn["status"].as_str().unwrap_or("failed");
        let mut outcome = if status == "completed" {
            AgentOutcome::success(final_text, started.elapsed())
        } else {
            let detail = turn["error"]["message"]
                .as_str()
                .filter(|message| !message.trim().is_empty())
                .unwrap_or(status);
            AgentOutcome::failure(format!("codex turn {status}: {detail}"), started.elapsed())
        };
        outcome.usage = (usage.prompt_tokens > 0).then_some(usage);
        Ok(outcome)
    }

    /// Start a fresh Codex thread or resume the one remembered for `key`.
    async fn open_thread(
        &self,
        process: &WireProcess,
        live: &LiveSession,
        key: &str,
        context: &AgentContext,
        deadline: Option<Instant>,
    ) -> Result<String> {
        let cwd = context.workspace.display().to_string();
        let model = context.model.clone();
        if let Some(id) = self.sessions.get("codex", key) {
            let mut params = json!({
                "threadId": id,
                "cwd": cwd,
                "approvalPolicy": "never",
                "sandbox": "danger-full-access",
            });
            if let Some(model) = &model {
                params["model"] = json!(model);
            }
            match process.request("thread/resume", params, deadline).await {
                Ok(result) => {
                    if let Some(thread) = result["thread"]["id"].as_str() {
                        *live.thread_id.lock().expect("codex thread mutex poisoned") =
                            Some(thread.to_owned());
                        report_model(context, &result);
                        return Ok(thread.to_owned());
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "could not resume codex thread; starting a new one");
                }
            }
        }

        let mut params = json!({
            "cwd": cwd,
            "approvalPolicy": "never",
            "sandbox": "danger-full-access",
        });
        if let Some(model) = &model {
            params["model"] = json!(model);
        }
        let result = process
            .request("thread/start", params, deadline)
            .await
            .map_err(|error| wire::unsupported("codex", error))?;
        let thread = result["thread"]["id"]
            .as_str()
            .ok_or_else(|| wire::unsupported("codex", "thread/start returned no thread id"))?
            .to_owned();
        report_model(context, &result);
        self.sessions.set("codex", key, &thread);
        *live.thread_id.lock().expect("codex thread mutex poisoned") = Some(thread.clone());
        Ok(thread)
    }

    pub async fn follow_up(
        &self,
        request: &AgentRequest,
        context: &AgentContext,
    ) -> Result<Option<SteerReceipt>> {
        self.follow_up_with_deadline(request, context, STEER_ACK_TIMEOUT)
            .await
    }

    /// [`Self::follow_up`] with an injectable acknowledgment deadline.
    async fn follow_up_with_deadline(
        &self,
        request: &AgentRequest,
        context: &AgentContext,
        ack_timeout: Duration,
    ) -> Result<Option<SteerReceipt>> {
        let map_key = self.map_key(context);
        let Some(live) = self
            .live
            .lock()
            .expect("codex live mutex poisoned")
            .get(&map_key)
            .cloned()
        else {
            return Ok(None);
        };
        let thread_id = live
            .thread_id
            .lock()
            .expect("codex thread mutex poisoned")
            .clone();
        let turn_id = live
            .active_turn
            .lock()
            .expect("codex turn mutex poisoned")
            .clone();
        let (Some(thread_id), Some(turn_id)) = (thread_id, turn_id) else {
            return Ok(None);
        };

        // A short deadline keeps a completed or wedged turn from blocking the
        // scheduler. An explicit rejection is safely queued as the next turn,
        // but an unacknowledged write may already have been consumed, so it is
        // reported as uncertain and never replayed.
        let deadline = Some(Instant::now() + ack_timeout);
        match live
            .process
            .request(
                "turn/steer",
                json!({
                    "threadId": thread_id,
                    "expectedTurnId": turn_id,
                    "input": [{ "type": "text", "text": build_follow_up_prompt(request) }],
                }),
                deadline,
            )
            .await
        {
            Ok(_) => {
                tracing::info!(key = %conversation_key(context), "steered the live codex turn");
                Ok(Some(SteerReceipt::merged()))
            }
            Err(error) if wire::is_uncertain(&error) => {
                tracing::warn!(%error, "codex steer acknowledgment was not confirmed; not replaying it");
                Ok(Some(SteerReceipt::uncertain()))
            }
            Err(error) => {
                tracing::warn!(%error, "codex rejected the follow-up; queueing it");
                Ok(None)
            }
        }
    }
}

/// Upper bound on `model/list` pages read while looking for a model default.
const MAX_MODEL_PAGES: usize = 20;

/// The `turn/start.effort` for this turn.
///
/// The app-server treats effort as sticky: it applies to this turn and every
/// later one on the thread, and omitting it or sending null keeps the previous
/// override. So a turn without an `/admin` effort must send the reset value
/// explicitly. That is the configured default, or the model's default when
/// none is configured. `None` (no effort sent) only when the server reports
/// neither.
async fn turn_effort(
    process: &WireProcess,
    context: &AgentContext,
    deadline: Option<Instant>,
) -> Option<String> {
    if let Some(effort) = context.effort.as_deref().filter(|e| !e.is_empty()) {
        return Some(effort.to_owned());
    }
    let model = context
        .reported_model
        .lock()
        .expect("model mutex poisoned")
        .clone();
    default_effort(process, &context.workspace, model.as_deref(), deadline).await
}

/// The reasoning effort a thread uses without an override: the configured
/// `model_reasoning_effort` for the workspace, else the model's own default.
async fn default_effort(
    process: &WireProcess,
    workspace: &std::path::Path,
    model: Option<&str>,
    deadline: Option<Instant>,
) -> Option<String> {
    let mut config_params = json!({});
    if !workspace.as_os_str().is_empty() {
        config_params["cwd"] = json!(workspace.display().to_string());
    }
    let configured = process
        .request("config/read", config_params, deadline)
        .await
        .ok()
        .and_then(|result| {
            result["config"]["model_reasoning_effort"]
                .as_str()
                .map(str::to_owned)
        })
        .filter(|effort| !effort.trim().is_empty());
    if configured.is_some() {
        return configured;
    }

    let model = model?;
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_MODEL_PAGES {
        let mut params = json!({ "includeHidden": true });
        if let Some(cursor) = &cursor {
            params["cursor"] = json!(cursor);
        }
        let page = process.request("model/list", params, deadline).await.ok()?;
        let default = page["data"]
            .as_array()?
            .iter()
            .find(|entry| entry["model"].as_str() == Some(model))
            .and_then(|entry| entry["defaultReasoningEffort"].as_str())
            .filter(|effort| !effort.trim().is_empty());
        if let Some(default) = default {
            return Some(default.to_owned());
        }
        cursor = Some(page["nextCursor"].as_str()?.to_owned());
    }
    None
}

/// Observe the server's effective model without selecting or guessing one.
fn report_model(context: &AgentContext, response: &Value) {
    *context.reported_model.lock().expect("model mutex poisoned") = response["model"]
        .as_str()
        .filter(|model| !model.trim().is_empty())
        .map(str::to_owned);
}

fn append_live(live_output: &Option<LiveOutput>, text: &str) {
    if let Some(live_output) = live_output {
        live_output.append(text.as_bytes());
    }
}

/// Answer a server-initiated request so an unattended turn cannot hang.
///
/// The adapter runs with `approvalPolicy = never` and `danger-full-access`, so
/// approval requests should not occur; this is a defensive fallback.
fn answer_server_request(process: &WireProcess, event: &Value) {
    let method = event["method"].as_str().unwrap_or_default();
    let result = if method.contains("pproval") {
        json!({ "decision": "approved" })
    } else {
        json!({})
    };
    tracing::debug!(method, "answering codex app-server request");
    process.respond(&event["id"], result);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::Executor;

    fn script() -> String {
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/fake_codex_app_server.py"
        )
        .into()
    }

    fn context(dir: &std::path::Path) -> AgentContext {
        AgentContext {
            workspace: dir.to_path_buf(),
            forge: Some(crate::location::ForgeKind::Forgejo),
            repository: "o/r".into(),
            issue_number: Some(1),
            ..Default::default()
        }
    }

    fn request() -> AgentRequest {
        AgentRequest {
            location: "https://forge.example.com/o/r/issues/1".parse().unwrap(),
            message: "go".into(),
        }
    }

    fn agent(
        config: crate::config::AgentConfig,
        sessions: Arc<SessionStore>,
    ) -> CodexAppServerAgent {
        CodexAppServerAgent::new(&config, sessions)
    }

    fn base_config(log: &std::path::Path) -> crate::config::AgentConfig {
        crate::config::AgentConfig {
            command: Some(script()),
            env: [("FAKE_CODEX_LOG".into(), log.display().to_string())].into(),
            ..Default::default()
        }
    }

    #[test]
    fn only_reports_a_nonempty_model_returned_by_the_server() {
        let ctx = AgentContext {
            model: Some("requested".into()),
            ..Default::default()
        };
        for response in [json!({}), json!({"model": null}), json!({"model": "  "})] {
            report_model(&ctx, &json!({"model": "previous"}));
            report_model(&ctx, &response);
            assert_eq!(*ctx.reported_model.lock().unwrap(), None);
        }
        report_model(&ctx, &json!({"model": "effective"}));
        assert_eq!(
            ctx.reported_model.lock().unwrap().as_deref(),
            Some("effective")
        );
    }

    #[tokio::test]
    async fn runs_a_turn_and_reports_text_usage_and_model() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log.jsonl");
        let sessions = Arc::new(SessionStore::load(dir.path()));
        let agent = agent(base_config(&log), Arc::clone(&sessions));
        let ctx = context(dir.path());
        let outcome = agent.run(&request(), &ctx).await.unwrap();
        assert!(outcome.success, "{outcome:?}");
        assert_eq!(outcome.model.as_deref(), Some("codex-default"));
        assert_eq!(
            ctx.reported_model.lock().unwrap().as_deref(),
            Some("codex-default")
        );
        assert_eq!(outcome.summary, "CODEX-REPLY");
        let logged = std::fs::read_to_string(&log).unwrap();
        assert!(
            !logged.contains("\"model\":"),
            "must preserve Codex defaults: {logged}"
        );
        let usage = outcome.usage.expect("usage");
        assert_eq!(usage.prompt_tokens, 1_000);
        assert_eq!(usage.cached_tokens, 750);
        // The thread id was persisted for the next comment.
        assert_eq!(
            sessions.get("codex", "forgejo:o/r:1").as_deref(),
            Some("thread-1")
        );
    }

    #[tokio::test]
    async fn steers_a_live_turn_without_a_second_process() {
        let dir = tempfile::tempdir().unwrap();
        let steer_log = dir.path().join("steer.log");
        let mut config = base_config(&dir.path().join("log.jsonl"));
        config
            .env
            .insert("FAKE_CODEX_WAIT_FOR_STEER".into(), "1".into());
        config.env.insert(
            "FAKE_CODEX_STEER_LOG".into(),
            steer_log.display().to_string(),
        );
        let sessions = Arc::new(SessionStore::load(dir.path()));
        let agent = Arc::new(agent(config, sessions));
        let ctx = context(dir.path());

        let run = {
            let agent = Arc::clone(&agent);
            let request = request();
            let ctx = ctx.clone();
            tokio::spawn(async move { agent.run(&request, &ctx).await })
        };
        // Wait until the turn is active and the process is acceptings steers.
        let mut active = false;
        for _ in 0..500 {
            if agent.live_agents() > 0 {
                let live = agent
                    .live
                    .lock()
                    .unwrap()
                    .get(&agent.map_key(&ctx))
                    .cloned();
                if let Some(live) = live
                    && live.active_turn.lock().unwrap().is_some()
                {
                    active = true;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(active, "codex turn should be active");
        assert_eq!(
            ctx.reported_model.lock().unwrap().as_deref(),
            Some("codex-default")
        );
        assert!(!run.is_finished());

        let receipt = agent
            .follow_up(
                &AgentRequest {
                    location: request().location,
                    message: "also run the linter".into(),
                },
                &ctx,
            )
            .await
            .unwrap()
            .expect("a live turn should accept the steer");
        assert!(receipt.notice.contains("Merged"));

        let outcome = run.await.unwrap().unwrap();
        assert!(outcome.success, "{outcome:?}");
        assert_eq!(outcome.model.as_deref(), Some("codex-default"));
        assert!(
            outcome.summary.contains("also run the linter"),
            "{outcome:?}"
        );
        assert!(steer_log.exists());
        assert_eq!(agent.live_agents(), 0);
    }

    #[tokio::test]
    async fn resumes_a_thread_and_reports_the_effective_model() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log.jsonl");
        let sessions = Arc::new(SessionStore::load(dir.path()));
        let mut config = base_config(&log);
        config
            .env
            .insert("FAKE_CODEX_RESUME_MODEL".into(), "resolved-resume".into());
        let agent = agent(config, Arc::clone(&sessions));
        let mut ctx = context(dir.path());
        ctx.model = Some("gpt-fast".into());

        for expected in ["gpt-fast", "resolved-resume"] {
            let outcome = agent.run(&request(), &ctx).await.unwrap();
            assert!(outcome.success);
            assert_eq!(outcome.model.as_deref(), Some(expected));
            assert_eq!(
                ctx.reported_model.lock().unwrap().as_deref(),
                Some(expected)
            );
        }

        let logged = std::fs::read_to_string(&log).unwrap();
        assert!(logged.contains("thread/start"), "{logged}");
        assert!(logged.contains("thread/resume"), "{logged}");
        assert!(logged.contains("gpt-fast"), "{logged}");
    }

    fn turn_params(log: &std::path::Path) -> Vec<Value> {
        std::fs::read_to_string(log)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|request| request["method"] == "turn/start")
            .map(|request| request["params"].clone())
            .collect()
    }

    /// The effective effort the server reported on each `thread/resume`.
    fn resumed_efforts(effective_log: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(effective_log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[tokio::test]
    async fn sends_the_requested_effort_on_each_turn() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log.jsonl");
        let sessions = Arc::new(SessionStore::load(dir.path()));
        let mut config = base_config(&log);
        config
            .env
            .insert("FAKE_CODEX_CONFIG_EFFORT".into(), "low".into());
        let agent = agent(config, Arc::clone(&sessions));

        let mut ctx = context(dir.path());
        ctx.effort = Some("high".into());
        assert!(agent.run(&request(), &ctx).await.unwrap().success);
        // Without an effort the turn sends the configured default explicitly.
        ctx.effort = None;
        assert!(agent.run(&request(), &ctx).await.unwrap().success);

        let turns = turn_params(&log);
        assert_eq!(turns.len(), 2, "{turns:?}");
        assert_eq!(turns[0]["effort"], "high");
        assert_eq!(turns[1]["effort"], "low", "{turns:?}");
    }

    #[tokio::test]
    async fn clearing_an_effort_restores_the_default_on_a_resumed_thread() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log.jsonl");
        let effective_log = dir.path().join("effective.log");
        let sessions = Arc::new(SessionStore::load(dir.path()));
        let mut config = base_config(&log);
        config
            .env
            .insert("FAKE_CODEX_CONFIG_EFFORT".into(), "low".into());
        config.env.insert(
            "FAKE_CODEX_STATE".into(),
            dir.path().join("state.json").display().to_string(),
        );
        config.env.insert(
            "FAKE_CODEX_EFFECTIVE_LOG".into(),
            effective_log.display().to_string(),
        );
        let agent = agent(config, Arc::clone(&sessions));
        let mut ctx = context(dir.path());

        // Set `high`, then clear it. Each run is a new app-server process that
        // resumes the same thread.
        ctx.effort = Some("high".into());
        assert!(agent.run(&request(), &ctx).await.unwrap().success);
        ctx.effort = None;
        assert!(agent.run(&request(), &ctx).await.unwrap().success);
        assert!(agent.run(&request(), &ctx).await.unwrap().success);

        // The resume before the clearing turn still sees `high`; after the
        // clearing turn the thread is back on the configured `low`.
        assert_eq!(resumed_efforts(&effective_log), ["high", "low"]);
        let turns = turn_params(&log);
        assert_eq!(turns[1]["effort"], "low", "{turns:?}");
        assert_eq!(turns[2]["effort"], "low", "{turns:?}");
        // The conversation itself was kept.
        assert_eq!(
            sessions.get("codex", "forgejo:o/r:1").as_deref(),
            Some("thread-1")
        );
    }

    #[tokio::test]
    async fn clearing_an_effort_falls_back_to_the_model_default() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log.jsonl");
        let effective_log = dir.path().join("effective.log");
        let mut config = base_config(&log);
        config
            .env
            .insert("FAKE_CODEX_MODEL_EFFORT".into(), "medium".into());
        config.env.insert(
            "FAKE_CODEX_STATE".into(),
            dir.path().join("state.json").display().to_string(),
        );
        config.env.insert(
            "FAKE_CODEX_EFFECTIVE_LOG".into(),
            effective_log.display().to_string(),
        );
        let agent = agent(config, Arc::new(SessionStore::load(dir.path())));
        let mut ctx = context(dir.path());

        ctx.effort = Some("high".into());
        assert!(agent.run(&request(), &ctx).await.unwrap().success);
        // No `model_reasoning_effort` is configured, so the model's own default
        // is restored.
        ctx.effort = None;
        assert!(agent.run(&request(), &ctx).await.unwrap().success);
        assert!(agent.run(&request(), &ctx).await.unwrap().success);
        assert_eq!(resumed_efforts(&effective_log), ["high", "medium"]);

        // A model the server does not list has no known default: nothing is
        // sent rather than a guessed value.
        let mut unlisted = context(dir.path());
        unlisted.model = Some("gpt-unlisted".into());
        assert!(agent.run(&request(), &unlisted).await.unwrap().success);
        let turns = turn_params(&log);
        assert_eq!(turns.len(), 4, "{turns:?}");
        assert_eq!(turns[1]["effort"], "medium", "{turns:?}");
        assert!(turns[3].get("effort").is_none(), "{turns:?}");
    }

    #[tokio::test]
    async fn missing_server_model_remains_unknown_on_start_and_resume() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = base_config(&dir.path().join("log.jsonl"));
        config
            .env
            .insert("FAKE_CODEX_OMIT_MODEL".into(), "1".into());
        let agent = agent(config, Arc::new(SessionStore::load(dir.path())));
        let mut ctx = context(dir.path());
        ctx.model = Some("requested".into());
        for _ in 0..2 {
            *ctx.reported_model.lock().unwrap() = Some("stale".into());
            let outcome = agent.run(&request(), &ctx).await.unwrap();
            assert!(outcome.success);
            assert_eq!(outcome.model, None);
            assert_eq!(*ctx.reported_model.lock().unwrap(), None);
        }
    }

    #[tokio::test]
    async fn failed_turn_is_reported_with_its_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = base_config(&dir.path().join("log.jsonl"));
        config
            .env
            .insert("FAKE_CODEX_STATUS".into(), "failed".into());
        let agent = agent(config, Arc::new(SessionStore::load(dir.path())));
        let outcome = agent.run(&request(), &context(dir.path())).await.unwrap();
        assert!(!outcome.success);
        assert_eq!(outcome.model.as_deref(), Some("codex-default"));
        assert!(outcome.summary.contains("failed"), "{outcome:?}");
        assert!(outcome.summary.contains("refused"), "{outcome:?}");
    }

    #[tokio::test]
    async fn a_slow_turn_times_out_and_is_interrupted() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = base_config(&dir.path().join("log.jsonl"));
        config.timeout_secs = Some(1);
        config.env.insert("FAKE_CODEX_DELAY".into(), "10".into());
        let agent = agent(config, Arc::new(SessionStore::load(dir.path())));
        let outcome = agent.run(&request(), &context(dir.path())).await.unwrap();
        assert!(!outcome.success);
        assert!(outcome.summary.contains("timed out"), "{outcome:?}");
    }

    #[tokio::test]
    async fn a_server_request_is_answered() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = base_config(&dir.path().join("log.jsonl"));
        config.env.insert("FAKE_CODEX_APPROVAL".into(), "1".into());
        let agent = agent(config, Arc::new(SessionStore::load(dir.path())));
        let outcome = agent.run(&request(), &context(dir.path())).await.unwrap();
        assert!(outcome.success, "{outcome:?}");
    }

    #[tokio::test]
    async fn a_rejected_steer_is_queued() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = base_config(&dir.path().join("log.jsonl"));
        config
            .env
            .insert("FAKE_CODEX_WAIT_FOR_STEER".into(), "1".into());
        config
            .env
            .insert("FAKE_CODEX_REJECT_STEER".into(), "1".into());
        let agent = Arc::new(agent(config, Arc::new(SessionStore::load(dir.path()))));
        let ctx = context(dir.path());
        let run = {
            let agent = Arc::clone(&agent);
            let request = request();
            let ctx = ctx.clone();
            tokio::spawn(async move { agent.run(&request, &ctx).await })
        };
        wait_for_active_turn(&agent, &ctx).await;
        assert!(agent.follow_up(&request(), &ctx).await.unwrap().is_none());
        run.await.unwrap().unwrap();
    }

    /// Wait until the spawned run has an active turn accepting steers.
    async fn wait_for_active_turn(agent: &Arc<CodexAppServerAgent>, ctx: &AgentContext) {
        for _ in 0..500 {
            let live = agent.live.lock().unwrap().get(&agent.map_key(ctx)).cloned();
            if live.is_some_and(|live| live.active_turn.lock().unwrap().is_some()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("codex turn did not become active");
    }

    /// Cancelling the run (an operator terminating the thread) must stop the
    /// app-server process and unregister it. A turn that is still held open
    /// must not stay steerable or running after the cancellation returns.
    #[tokio::test]
    async fn cancelling_a_held_turn_stops_its_process_and_unregisters_it() {
        use crate::agent::wire::test_support::{read_pid, wait_until_exited};
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let mut config = base_config(&dir.path().join("log.jsonl"));
        config
            .env
            .insert("FAKE_CODEX_WAIT_FOR_STEER".into(), "1".into());
        config
            .env
            .insert("FAKE_CODEX_PID_FILE".into(), pid_file.display().to_string());
        let agent = Arc::new(agent(config, Arc::new(SessionStore::load(dir.path()))));
        let ctx = context(dir.path());
        let run = {
            let agent = Arc::clone(&agent);
            let request = request();
            let ctx = ctx.clone();
            tokio::spawn(async move { agent.run(&request, &ctx).await })
        };
        wait_for_active_turn(&agent, &ctx).await;
        let pid = read_pid(&pid_file).await;

        run.abort();
        assert!(run.await.unwrap_err().is_cancelled());

        assert_eq!(
            agent.live_agents(),
            0,
            "the cancelled process stays registered"
        );
        assert!(agent.follow_up(&request(), &ctx).await.unwrap().is_none());
        wait_until_exited(pid).await;
    }

    #[tokio::test]
    async fn a_turn_dying_after_start_fails_promptly() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = base_config(&dir.path().join("log.jsonl"));
        config
            .env
            .insert("FAKE_CODEX_EXIT_AFTER_TURN_START".into(), "1".into());
        let agent = agent(config, Arc::new(SessionStore::load(dir.path())));
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            agent.run(&request(), &context(dir.path())),
        )
        .await
        .expect("a dead app-server must not hang the turn")
        .unwrap();
        assert!(!outcome.success);
        assert!(outcome.summary.contains("exited"), "{outcome:?}");
    }

    #[tokio::test]
    async fn an_unacknowledged_steer_is_not_replayed() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = base_config(&dir.path().join("log.jsonl"));
        config
            .env
            .insert("FAKE_CODEX_WAIT_FOR_STEER".into(), "1".into());
        config
            .env
            .insert("FAKE_CODEX_STEER_DELAY".into(), "1".into());
        let agent = Arc::new(agent(config, Arc::new(SessionStore::load(dir.path()))));
        let ctx = context(dir.path());
        let run = {
            let agent = Arc::clone(&agent);
            let request = request();
            let ctx = ctx.clone();
            tokio::spawn(async move { agent.run(&request, &ctx).await })
        };
        wait_for_active_turn(&agent, &ctx).await;

        let receipt = agent
            .follow_up_with_deadline(&request(), &ctx, Duration::from_millis(200))
            .await
            .unwrap()
            .expect("an unacknowledged steer must not be queued for replay");
        assert!(!receipt.notice.contains("Merged"), "{}", receipt.notice);
        assert!(
            receipt.notice.contains("not confirmed"),
            "{}",
            receipt.notice
        );
        let outcome = run.await.unwrap().unwrap();
        assert!(outcome.success, "{outcome:?}");
    }

    #[tokio::test]
    async fn a_steer_disconnect_is_not_replayed() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = base_config(&dir.path().join("log.jsonl"));
        config
            .env
            .insert("FAKE_CODEX_WAIT_FOR_STEER".into(), "1".into());
        config
            .env
            .insert("FAKE_CODEX_STEER_EXIT".into(), "1".into());
        let agent = Arc::new(agent(config, Arc::new(SessionStore::load(dir.path()))));
        let ctx = context(dir.path());
        let run = {
            let agent = Arc::clone(&agent);
            let request = request();
            let ctx = ctx.clone();
            tokio::spawn(async move { agent.run(&request, &ctx).await })
        };
        wait_for_active_turn(&agent, &ctx).await;

        let receipt = agent
            .follow_up_with_deadline(&request(), &ctx, Duration::from_millis(500))
            .await
            .unwrap()
            .expect("a disconnected steer must not be queued for replay");
        assert!(!receipt.notice.contains("Merged"), "{}", receipt.notice);
        let outcome = run.await.unwrap().unwrap();
        assert!(!outcome.success, "{outcome:?}");
    }

    #[tokio::test]
    async fn a_turn_start_error_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = base_config(&dir.path().join("log.jsonl"));
        config.env.insert("FAKE_CODEX_FAIL_TURN".into(), "1".into());
        let agent = agent(config, Arc::new(SessionStore::load(dir.path())));
        let error = agent
            .run(&request(), &context(dir.path()))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("turn start failed"), "{error}");
    }

    #[tokio::test]
    async fn follow_up_without_a_live_turn_is_queued() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = Arc::new(SessionStore::load(dir.path()));
        let agent = agent(base_config(&dir.path().join("log.jsonl")), sessions);
        assert!(
            agent
                .follow_up(&request(), &context(dir.path()))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_process_that_rejects_initialize_is_reported_unsupported() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("fake.sh");
        std::fs::write(&fake, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config = crate::config::AgentConfig {
            command: Some(fake.display().to_string()),
            ..Default::default()
        };
        let agent = agent(config, Arc::new(SessionStore::load(dir.path())));
        let error = agent
            .run(&request(), &context(dir.path()))
            .await
            .unwrap_err();
        assert!(wire::is_unsupported(&error), "{error}");
        let _ = Executor::direct();
    }
}

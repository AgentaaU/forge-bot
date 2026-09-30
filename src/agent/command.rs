//! Generic subprocess-backed agent adapter.
//!
//! Every concrete adapter is a thin configuration of [`CommandAgent`]: a
//! program, some arguments, and how the prompt is delivered. This keeps adding
//! a new CLI a matter of a few lines.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::agent::prompt::build_prompt;
use crate::agent::session::SessionStore;
use crate::agent::{
    Agent, AgentContext, AgentOutcome, AgentRequest, LiveOutput, TokenUsage, conversation_key,
};
use crate::config::{OutputFormat, PromptDelivery};
use crate::error::{BotError, Result};
use crate::executor::ExecSpec;

/// Maximum number of characters of captured output kept in the summary.
const OUTPUT_LIMIT: usize = 4000;

async fn capture_stream<R: AsyncRead + Unpin>(
    mut reader: R,
    live_output: Option<LiveOutput>,
) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut chunk = [0; 8192];
    loop {
        let count = reader.read(&mut chunk).await?;
        if count == 0 {
            break;
        }
        output.extend_from_slice(&chunk[..count]);
        if let Some(live_output) = &live_output {
            live_output.append(&chunk[..count]);
        }
    }
    Ok(output)
}

/// An [`Agent`] implemented by spawning a child process.
#[derive(Debug, Clone)]
pub struct CommandAgent {
    name: String,
    program: String,
    args: Vec<String>,
    prompt: PromptDelivery,
    timeout: Option<Duration>,
    env: BTreeMap<String, String>,
    dangerously_skip_permissions: bool,
    session: Option<SessionContinuation>,
    result_format: OutputFormat,
    /// Index at which a per-user `--model <id>` is inserted. `None` appends it
    /// before the prompt; adapters whose prompt-consuming flag must stay last
    /// (agy `--print`) pin it to that flag's index.
    model_at: Option<usize>,
}

/// How a [`CommandAgent`] continues the conversation for one thread.
#[derive(Debug, Clone)]
pub struct SessionContinuation {
    store: Arc<SessionStore>,
    style: SessionStyle,
}

/// Per-adapter rules for resuming a backend conversation.
///
/// `{session}` is replaced with the backend session id and `{reply_file}` with
/// a scratch file the CLI may write its final message to.
#[derive(Debug, Clone)]
pub struct SessionStyle {
    /// Args appended when starting a new conversation.
    pub create_args: Vec<String>,
    /// Args used for a later comment in the same conversation.
    pub resume_args: Vec<String>,
    /// Where `resume_args` are inserted into the base args; `None` appends.
    pub resume_at: Option<usize>,
    /// Read `{reply_file}` for the reply instead of stdout.
    pub reply_from_file: bool,
    /// Discover the new session id in `--json` output (`codex`).
    pub capture_id: bool,
    /// On resume, ignore the base args and use `resume_args` alone. Needed
    /// when the resume subcommand rejects flags the base command accepts
    /// (`codex exec resume` has no `--color`/`--sandbox`).
    pub replace_on_resume: bool,
}

/// Resolved session arguments for one invocation.
#[derive(Debug, Default)]
struct SessionPlan {
    /// Extra args to add to the base argument list.
    args: Vec<String>,
    /// Insert `args` at this index; `None` appends.
    at: Option<usize>,
    /// Scratch file the CLI writes its final message to.
    reply_file: Option<PathBuf>,
    /// `(conversation, id)` to remember when the id is known up front.
    persist: Option<(String, String)>,
    /// Conversation whose captured id should be remembered on success.
    capture: Option<String>,
    /// Drop the base args and use `args` alone (resume subcommands).
    replace_base: bool,
    /// Existing backend session id, if this invocation resumes one.
    session_id: Option<String>,
}

/// Substitute `{session}` / `{reply_file}` into a session arg template.
fn interpolate(template: &[String], session: &str, reply: Option<&str>) -> Vec<String> {
    template
        .iter()
        .map(|arg| {
            arg.replace("{session}", session)
                .replace("{reply_file}", reply.unwrap_or_default())
        })
        .collect()
}

/// Extract the `thread_id` from a codex `--json` JSONL stream.
fn parse_thread_id(stdout: &str) -> Option<String> {
    for line in stdout.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        if value["type"] == "thread.started"
            && let Some(id) = value["thread_id"].as_str()
        {
            return Some(id.to_owned());
        }
    }
    None
}

/// Sum codex `turn.completed` usage from a `--json` JSONL stream.
///
/// Codex reports `input_tokens` as the whole prompt and `cached_input_tokens`
/// as the cached subset, so they map straight onto [`TokenUsage`]. A run may
/// emit more than one completed turn (for example when it is resumed), hence
/// the sum.
fn parse_codex_usage(stdout: &str) -> Option<TokenUsage> {
    let mut usage = TokenUsage::default();
    let mut seen = false;
    for line in stdout.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        if value["type"] != "turn.completed" {
            continue;
        }
        let Some(turn) = value.get("usage") else {
            continue;
        };
        usage.prompt_tokens += turn["input_tokens"].as_u64().unwrap_or(0);
        usage.cached_tokens += turn["cached_input_tokens"].as_u64().unwrap_or(0);
        seen = true;
    }
    seen.then_some(usage)
}

/// A machine-readable CLI result: the reply text and/or prompt usage.
#[derive(Debug, Default)]
struct JsonResult {
    text: Option<String>,
    usage: Option<TokenUsage>,
    is_error: bool,
}

/// Parse the last JSON result object from a command's stdout.
///
/// Returns an object that carries a reply (`response` for agy, `result` for
/// claude) or a usage object, so an adapter that streams events is handled by
/// taking the final result.
fn parse_result_json(stdout: &str) -> Option<JsonResult> {
    let mut result = None;
    for line in stdout.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        if !value.is_object() {
            continue;
        }
        let text = value
            .get("response")
            .or_else(|| value.get("result"))
            .and_then(|text| text.as_str())
            .map(str::to_owned);
        let usage = value.get("usage").and_then(normalize_usage);
        if text.is_some() || usage.is_some() {
            result = Some(JsonResult {
                text,
                usage,
                is_error: value["is_error"].as_bool().unwrap_or(false),
            });
        }
    }
    result
}

/// Normalize the usage-object shapes the CLIs emit.
///
/// * Pi: `input` is the cache miss and `cacheRead` the hit.
/// * Codex: `input_tokens` already includes `cached_input_tokens`.
/// * Antigravity (`agy`): `input_tokens` is the prompt and `cache_read_tokens`
///   the cached subset.
/// * Claude Code: `input_tokens` excludes `cache_read_input_tokens` and
///   `cache_creation_input_tokens`.
fn normalize_usage(value: &serde_json::Value) -> Option<TokenUsage> {
    if let (Some(input), Some(cache_read)) = (value["input"].as_u64(), value["cacheRead"].as_u64())
    {
        return Some(TokenUsage {
            prompt_tokens: input + cache_read,
            cached_tokens: cache_read,
        });
    }
    let input = value["input_tokens"].as_u64()?;
    if let Some(cached) = value["cached_input_tokens"].as_u64() {
        return Some(TokenUsage {
            prompt_tokens: input,
            cached_tokens: cached,
        });
    }
    if let Some(cache_read) = value["cache_read_tokens"].as_u64() {
        return Some(TokenUsage {
            prompt_tokens: input,
            cached_tokens: cache_read,
        });
    }
    let read = value["cache_read_input_tokens"].as_u64().unwrap_or(0);
    let created = value["cache_creation_input_tokens"].as_u64().unwrap_or(0);
    if value.get("cache_read_input_tokens").is_some()
        || value.get("cache_creation_input_tokens").is_some()
    {
        return Some(TokenUsage {
            prompt_tokens: input + read + created,
            cached_tokens: read,
        });
    }
    None
}

/// Read a model option supplied by the operator without adding or changing
/// any argument passed to the agent.
pub(crate) fn model_arg(args: &[String], short: bool) -> Option<String> {
    args.iter()
        .enumerate()
        .filter_map(|(index, arg)| {
            if arg == "--model" || (short && arg == "-m") {
                args.get(index + 1)
                    .filter(|value| !value.starts_with('-'))
                    .cloned()
            } else {
                arg.strip_prefix("--model=")
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
            }
        })
        .next_back()
}

/// Insert the addressed user's `--model <id>` unless the operator already
/// configured one. `at` pins the insertion before a prompt-consuming flag
/// (agy `--print`); `None` appends it just before the prompt.
fn apply_model(args: &mut Vec<String>, model: Option<&str>, at: Option<usize>) {
    if let Some(model) = model.map(str::trim).filter(|model| !model.is_empty())
        && model_arg(args, false).is_none()
    {
        let at = at.unwrap_or(args.len()).min(args.len());
        args.insert(at, model.to_owned());
        args.insert(at, "--model".to_owned());
    }
}

impl CommandAgent {
    pub fn new(name: impl Into<String>, program: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            program: program.into(),
            args: Vec::new(),
            prompt: PromptDelivery::Stdin,
            timeout: None,
            env: BTreeMap::new(),
            dangerously_skip_permissions: false,
            session: None,
            result_format: OutputFormat::Text,
            model_at: None,
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Drop every occurrence of `arg`, so an adapter that must re-add a
    /// position-sensitive flag (agy's `--print`) does not duplicate one the
    /// operator supplied.
    pub fn remove_arg(mut self, arg: &str) -> Self {
        self.args.retain(|existing| existing != arg);
        self
    }

    /// Replace the program that is executed.
    pub fn with_program(mut self, program: impl Into<String>) -> Self {
        self.program = program.into();
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn prompt(mut self, delivery: PromptDelivery) -> Self {
        self.prompt = delivery;
        self
    }

    /// Set a wall-clock limit for the command. Without this the agent runs
    /// until its process exits.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    pub fn envs<I, K, V>(mut self, envs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        self.env
            .extend(envs.into_iter().map(|(k, v)| (k.into(), v.into())));
        self
    }

    pub fn dangerously_skip_permissions(mut self, yes: bool) -> Self {
        self.dangerously_skip_permissions = yes;
        self
    }

    /// Insert a per-user model argument at `at` instead of appending it. Used
    /// by adapters that require a prompt-consuming flag to remain last.
    pub fn model_at(mut self, at: usize) -> Self {
        self.model_at = Some(at);
        self
    }

    /// Ask the CLI for one JSON result object and parse the reply and token
    /// usage from it.
    ///
    /// Adds `--output-format json` unless the operator already chose an
    /// output format, so a configured `--output-format text` is respected and
    /// the result is left as plain text.
    pub fn with_json_output(mut self) -> Self {
        let explicit = self
            .args
            .iter()
            .any(|arg| arg == "--output-format" || arg.starts_with("--output-format="));
        if !explicit {
            self.args.push("--output-format".into());
            self.args.push("json".into());
            self.result_format = OutputFormat::Json;
        }
        self
    }

    /// Reuse one backend conversation per thread. `store` persists the
    /// backend session id so the next comment resumes the same context.
    pub fn session(mut self, style: SessionStyle, store: Arc<SessionStore>) -> Self {
        self.session = Some(SessionContinuation { store, style });
        self
    }

    /// Resolve the session arguments for one request.
    fn session_plan(&self, context: &AgentContext) -> SessionPlan {
        let Some(continuation) = &self.session else {
            return SessionPlan::default();
        };
        let style = &continuation.style;
        let key = conversation_key(context);
        let reply_file = style.reply_from_file.then(|| {
            std::env::temp_dir().join(format!("forge-bot-reply-{}.txt", uuid::Uuid::new_v4()))
        });
        let reply = reply_file
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());

        let mut plan = SessionPlan {
            reply_file,
            ..Default::default()
        };

        match continuation.store.get(&self.name, &key) {
            Some(id) => {
                plan.session_id = Some(id.clone());
                plan.args = interpolate(&style.resume_args, &id, reply.as_deref());
                plan.at = style.resume_at;
                plan.replace_base = style.replace_on_resume;
            }
            None => {
                let id = if style.capture_id {
                    String::new()
                } else {
                    // A CLI may create its session before failing (or before
                    // the gateway is interrupted). With no saved successful
                    // mapping, reusing a deterministic ID would collide with
                    // that orphaned session. New attempts need fresh IDs;
                    // successful IDs are still persisted and resumed above.
                    uuid::Uuid::new_v4().to_string()
                };
                plan.args = interpolate(&style.create_args, &id, reply.as_deref());
                plan.at = None;
                if style.capture_id {
                    plan.capture = Some(key);
                } else {
                    plan.persist = Some((key, id));
                }
            }
        }
        plan
    }

    /// Remember the backend session id after a successful run.
    fn record_session(&self, plan: &SessionPlan, stdout: &str) {
        let Some(continuation) = &self.session else {
            return;
        };
        if let Some((key, id)) = &plan.persist {
            continuation.store.set(&self.name, key, id);
        }
        if let Some(key) = &plan.capture
            && let Some(id) = parse_thread_id(stdout)
        {
            continuation.store.set(&self.name, key, &id);
        }
    }

    pub fn dangerously_skip_permissions_enabled(&self) -> bool {
        self.dangerously_skip_permissions
    }

    /// Apply a partial user override, keeping defaults for unset fields.
    pub fn apply_config(mut self, config: &crate::config::AgentConfig) -> Self {
        if let Some(command) = &config.command {
            self.program = command.clone();
        }
        if let Some(args) = &config.args {
            self.args = args.clone();
        }
        if let Some(prompt) = config.prompt {
            self.prompt = prompt;
        }
        if let Some(format) = config.output_format {
            self.result_format = format;
        }
        if let Some(timeout) = config.timeout_secs {
            // 0 disables the wall-clock limit entirely.
            self.timeout = (timeout != 0).then(|| Duration::from_secs(timeout));
        }
        if let Some(dangerous) = config.dangerously_skip_permissions {
            self.dangerously_skip_permissions = dangerous;
        }
        self.env.extend(config.env.clone());
        self
    }

    pub fn program(&self) -> &str {
        &self.program
    }

    /// Arguments the adapter will pass to the program (including defaults and
    /// any applied overrides).
    pub fn arguments(&self) -> &[String] {
        &self.args
    }
}

#[async_trait::async_trait]
impl Agent for CommandAgent {
    fn name(&self) -> &str {
        &self.name
    }

    async fn run(&self, request: &AgentRequest, context: &AgentContext) -> Result<AgentOutcome> {
        let workspace: PathBuf = context.workspace.clone();
        if !workspace.as_os_str().is_empty() {
            context
                .executor
                .create_dir_all(&workspace, context.host_user.as_deref())
                .await?;
        }

        let prompt = build_prompt(request, context);
        let started = Instant::now();
        let plan = self.session_plan(context);
        let mut args = if plan.replace_base {
            Vec::new()
        } else {
            self.args.clone()
        };
        match plan.at {
            Some(at) => {
                let at = at.min(args.len());
                for (offset, arg) in plan.args.iter().enumerate() {
                    args.insert(at + offset, arg.clone());
                }
            }
            None => args.extend(plan.args.iter().cloned()),
        }

        // Apply the addressed user's model before reading the effective model
        // for reporting. An explicit `--model` already in the configured args
        // wins, so operator intent is never overridden.
        apply_model(&mut args, context.model.as_deref(), self.model_at);

        let configured_model = match self.name.as_str() {
            "agy" => crate::agent::agy::configured_model(&args, self.env.get("HOME")),
            "kimi" => crate::agent::kimi::configured_model(
                &args,
                self.env.get("HOME"),
                self.env.get("KIMI_CODE_HOME"),
                &workspace,
            ),
            _ => None,
        };
        *context.reported_model.lock().expect("model mutex poisoned") = configured_model.clone();

        let mut call_args = args;
        if self.prompt == PromptDelivery::Arg {
            call_args.push(prompt.clone());
        }
        let mut env: Vec<(String, String)> = self
            .env
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        env.extend(context.environment(request));
        let spec = ExecSpec {
            program: self.program.clone(),
            args: call_args,
            env,
            cwd: (!workspace.as_os_str().is_empty()).then(|| workspace.clone()),
            host_user: context.host_user.clone(),
        };
        let crate::executor::Prepared {
            command: mut cmd,
            cgroup,
        } = context
            .executor
            .command(&spec)
            .map_err(|error| BotError::Agent {
                name: self.name.clone(),
                reason: error.to_string(),
            })?;
        cmd.stdin(if self.prompt == PromptDelivery::Stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
        tracing::info!(
            agent = %self.name,
            program = %self.program,
            workspace = %workspace.display(),
            "starting agent"
        );

        let program = self.program.clone();
        let name = self.name.clone();
        let prompt_for_spawn = prompt.clone();
        let live_output = context.live_output.clone();

        let run = async move {
            // Hold the run's cgroup guard for the whole run so cancellation
            // stops the unit's descendants.
            let _keep_cgroup = cgroup;
            let mut child = spawn_retrying_busy(&mut cmd, &name, &program).await?;

            if let Some(mut stdin) = child.stdin.take() {
                if let Err(error) = stdin.write_all(prompt_for_spawn.as_bytes()).await {
                    // A one-shot command may exit before reading its prompt,
                    // which surfaces as a broken pipe. That is not itself a
                    // failure: the child's exit status and output are what
                    // matter, so carry on and let `wait_with_output` decide.
                    if !matches!(
                        error.kind(),
                        std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                    ) {
                        return Err(BotError::Agent {
                            name: name.clone(),
                            reason: format!("failed to write prompt to stdin: {error}"),
                        });
                    }
                }
                // Dropping stdin signals EOF to the child.
                drop(stdin);
            }

            let stdout = child.stdout.take().expect("stdout was piped");
            let stderr = child.stderr.take().expect("stderr was piped");
            tokio::try_join!(
                child.wait(),
                capture_stream(stdout, live_output.clone()),
                capture_stream(stderr, live_output),
            )
            .map_err(|e| BotError::Agent {
                name: name.clone(),
                reason: format!("failed while waiting for `{program}`: {e}"),
            })
        };

        let result = match self.timeout {
            Some(timeout) => match tokio::time::timeout(timeout, run).await {
                Err(_) => {
                    return Ok(AgentOutcome::failure(
                        format!("agent timed out after {timeout:?}"),
                        started.elapsed(),
                    ));
                }
                Ok(result) => result,
            },
            None => run.await,
        };

        match result {
            Err(err) => Err(err),
            Ok((status, stdout_bytes, stderr_bytes)) => {
                let stdout = String::from_utf8_lossy(&stdout_bytes);
                let stderr = String::from_utf8_lossy(&stderr_bytes);
                // A JSON-result CLI prints the reply (and usage) as one object;
                // prefer that text over the raw stream.
                let json = (self.result_format == OutputFormat::Json)
                    .then(|| parse_result_json(&stdout))
                    .flatten();
                let fallback = match json.as_ref().and_then(|result| result.text.as_deref()) {
                    Some(text) if !text.trim().is_empty() => text.to_owned(),
                    _ => summarize(&stdout, &stderr),
                };
                let observed_model = match self.name.as_str() {
                    "codex" => {
                        let id = parse_thread_id(&stdout).or_else(|| plan.session_id.clone());
                        let home = self.env.get("CODEX_HOME").map(PathBuf::from);
                        id.as_deref().and_then(|id| {
                            crate::agent::codex::model_from_session(id, home.as_deref())
                        })
                    }
                    "claude" => {
                        let id = plan
                            .session_id
                            .as_deref()
                            .or_else(|| plan.persist.as_ref().map(|(_, id)| id.as_str()));
                        let root = self.env.get("CLAUDE_CONFIG_DIR").map(PathBuf::from);
                        id.and_then(|id| {
                            crate::agent::claude::model_from_session(id, root.as_deref())
                        })
                    }
                    _ => configured_model,
                };
                *context.reported_model.lock().expect("model mutex poisoned") =
                    observed_model.clone();

                // Codex reports usage in its own event stream; the JSON-result
                // adapters (agy, claude, ...) carry it in the result object.
                let usage = if self.name == "codex" {
                    parse_codex_usage(&stdout)
                } else {
                    json.as_ref().and_then(|result| result.usage)
                };

                if status.success() && !json.as_ref().is_some_and(|result| result.is_error) {
                    let summary = match &plan.reply_file {
                        Some(path) => {
                            let reply = context
                                .executor
                                .read_reply_file(path, context.host_user.as_deref());
                            let _ = std::fs::remove_file(path);
                            let text = match reply {
                                Ok(text) => text,
                                Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
                                Err(_) => return Err(BotError::Agent {
                                    name: self.name.clone(),
                                    reason: "reply file rejected: expected a bounded regular file owned by the agent".into(),
                                }),
                            };
                            let text = text.trim();
                            if text.is_empty() {
                                fallback
                            } else {
                                text.to_owned()
                            }
                        }
                        None => fallback,
                    };
                    self.record_session(&plan, &stdout);
                    let mut outcome = AgentOutcome::success(summary, started.elapsed());
                    outcome.model = observed_model;
                    outcome.usage = usage;
                    Ok(outcome)
                } else {
                    if let Some(path) = &plan.reply_file {
                        let _ = std::fs::remove_file(path);
                    }
                    let mut outcome = AgentOutcome::failure(
                        format!(
                            "agent exited with {}: {}",
                            status
                                .code()
                                .map(|c| c.to_string())
                                .unwrap_or_else(|| "signal".into()),
                            json.as_ref()
                                .and_then(|result| result.text.as_deref())
                                .filter(|text| !text.trim().is_empty())
                                .map(|text| summarize(text, ""))
                                .unwrap_or_else(|| summarize(&format!("{stdout}\n{stderr}"), ""))
                        ),
                        started.elapsed(),
                    );
                    outcome.model = if matches!(self.name.as_str(), "agy" | "kimi") {
                        None
                    } else {
                        observed_model
                    };
                    outcome.usage = usage;
                    *context.reported_model.lock().expect("model mutex poisoned") =
                        outcome.model.clone();
                    Ok(outcome)
                }
            }
        }
    }
}

/// Keep the tail of the output, preferring stdout over stderr.
fn summarize(stdout: &str, stderr: &str) -> String {
    let trimmed = stdout.trim();
    let source = if trimmed.is_empty() {
        stderr.trim()
    } else {
        trimmed
    };
    if source.len() <= OUTPUT_LIMIT {
        return source.to_owned();
    }
    let start = source.len() - OUTPUT_LIMIT;
    // Do not split a UTF-8 code point.
    let start = source
        .char_indices()
        .map(|(i, _)| i)
        .find(|&i| i >= start)
        .unwrap_or(source.len());
    format!("…{}", &source[start..])
}

/// Wait before retrying a spawn whose executable is still busy.
const BUSY_SPAWN_ATTEMPTS: usize = 5;

/// Spawn `cmd`, retrying while the kernel reports the program is still being
/// written. Linux returns `ETXTBSY` (`ExecutableFileBusy`) when a just-written
/// executable is still held open by a concurrent `fork`, which parallel tests
/// that write a fake CLI hit by chance, and when an operator replaces the
/// binary while a run starts. Retrying briefly resolves both.
async fn spawn_retrying_busy(
    cmd: &mut Command,
    name: &str,
    program: &str,
) -> Result<tokio::process::Child> {
    let mut attempt = 0;
    loop {
        match cmd.spawn() {
            Ok(child) => return Ok(child),
            Err(error)
                if error.kind() == std::io::ErrorKind::ExecutableFileBusy
                    && attempt < BUSY_SPAWN_ATTEMPTS =>
            {
                attempt += 1;
                tokio::time::sleep(std::time::Duration::from_millis(10 * attempt as u64)).await;
            }
            Err(error) => {
                return Err(BotError::Agent {
                    name: name.to_owned(),
                    reason: format!("failed to spawn `{program}`: {error}"),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::ReplyTarget;

    #[test]
    fn summarize_prefers_stdout() {
        assert_eq!(summarize("hello", "warn"), "hello");
        assert_eq!(summarize("", "warn"), "warn");
    }

    #[test]
    fn summarize_truncates_tail() {
        let long = "a".repeat(OUTPUT_LIMIT + 100);
        let out = summarize(&long, "");
        assert!(out.starts_with('…'));
        assert_eq!(out.chars().count(), OUTPUT_LIMIT + 1);
    }

    #[test]
    fn parses_codex_turn_usage() {
        let stdout = concat!(
            "{\"type\":\"thread.started\",\"thread_id\":\"t\"}\n",
            "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":1000,\"cached_input_tokens\":900,\"output_tokens\":5}}\n",
            "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":500,\"cached_input_tokens\":400,\"output_tokens\":1}}\n",
        );
        let usage = parse_codex_usage(stdout).unwrap();
        assert_eq!(usage.prompt_tokens, 1_500);
        assert_eq!(usage.cached_tokens, 1_300);
        assert!((usage.hit_rate().unwrap() - 86.666).abs() < 0.01);
        assert!(parse_codex_usage("not json").is_none());
        assert!(parse_codex_usage("{\"type\":\"turn.started\"}").is_none());
    }

    #[test]
    fn parses_json_result_usage() {
        // Antigravity: `input_tokens` is the prompt, `cache_read_tokens` the hit.
        let agy = r#"{"response":"hi","usage":{"input_tokens":1000,"cache_read_tokens":900,"total_tokens":1100}}"#;
        let result = parse_result_json(agy).unwrap();
        assert_eq!(result.text.as_deref(), Some("hi"));
        let usage = result.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 1_000);
        assert_eq!(usage.cached_tokens, 900);
        assert_eq!(usage.hit_rate(), Some(90.0));

        // Claude Code: `input_tokens` excludes cache read/creation.
        let claude = r#"{"type":"result","result":"done","usage":{"input_tokens":100,"cache_read_input_tokens":800,"cache_creation_input_tokens":100}}"#;
        let result = parse_result_json(claude).unwrap();
        assert_eq!(result.text.as_deref(), Some("done"));
        let usage = result.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 1_000);
        assert_eq!(usage.cached_tokens, 800);

        // A Claude run with no cache hit still reports its prompt.
        let cold = r#"{"result":"x","usage":{"input_tokens":500,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}"#;
        let usage = parse_result_json(cold).unwrap().usage.unwrap();
        assert_eq!(usage.prompt_tokens, 500);
        assert_eq!(usage.cached_tokens, 0);

        // Nothing machine-readable is ignored.
        assert!(parse_result_json("plain text").is_none());
        assert!(parse_result_json(r#"{"type":"event"}"#).is_none());
    }

    #[test]
    fn apply_config_keeps_the_configured_arguments() {
        let config = crate::config::AgentConfig {
            args: Some(vec!["exec".into(), "--flag".into(), "value".into()]),
            ..Default::default()
        };
        let agent = CommandAgent::new("codex", "codex").apply_config(&config);
        // The arguments are exactly what the configuration asked for.
        assert_eq!(
            agent.arguments().to_vec(),
            vec![
                "exec".to_string(),
                "--flag".to_string(),
                "value".to_string()
            ]
        );
    }

    #[test]
    fn apply_model_appends_before_the_prompt_and_respects_an_explicit_choice() {
        let mut args = vec!["exec".to_owned()];
        apply_model(&mut args, Some("gpt-fast"), None);
        assert_eq!(args, ["exec", "--model", "gpt-fast"]);

        // An operator-configured model is never overridden.
        let mut args = vec![
            "exec".to_owned(),
            "--model".to_owned(),
            "operator".to_owned(),
        ];
        apply_model(&mut args, Some("gpt-fast"), None);
        assert_eq!(args, ["exec", "--model", "operator"]);

        // A pinned index keeps a prompt-consuming flag last (agy --print).
        let mut args = vec![
            "--dangerously-skip-permissions".to_owned(),
            "--print".to_owned(),
        ];
        apply_model(&mut args, Some("gpt-fast"), Some(1));
        assert_eq!(
            args,
            [
                "--dangerously-skip-permissions",
                "--model",
                "gpt-fast",
                "--print"
            ]
        );

        // Nothing to apply is a no-op.
        let mut args = vec!["exec".to_owned()];
        apply_model(&mut args, None, None);
        apply_model(&mut args, Some("  "), None);
        assert_eq!(args, ["exec"]);
    }

    #[test]
    fn prompt_points_review_mentions_at_their_thread() {
        let request = AgentRequest {
            location: url::Url::parse("https://forge.example.com/o/r/pulls/22#issuecomment-9039")
                .unwrap(),
            message: "why?".into(),
        };
        let mut context = AgentContext {
            repository: "o/r".into(),
            requester: "alice".into(),
            issue_number: Some(22),
            is_pull_request: true,
            ..Default::default()
        };

        // A pull-request conversation mention does not request another review.
        let conversation = build_prompt(&request, &context);
        assert!(!conversation.contains("inline pull-request review comment"));
        assert!(!conversation.contains("request a review from the caller"));

        context.reply_target = ReplyTarget::ReviewComment(crate::forge::ReviewCommentTarget {
            review_id: 103,
            path: "src/agent/registry.rs".into(),
            line: -12,
            extra_lines_count: 0,
        });
        let review = build_prompt(&request, &context);
        assert!(review.contains("inline pull-request review comment"));
        assert!(review.contains("review id 103"));
        assert!(review.contains("src/agent/registry.rs"));
        assert!(review.contains("-12"));
        assert!(review.contains("instead of opening a new top-level comment"));
    }

    #[tokio::test]
    async fn runs_echo_with_stdin_prompt() {
        // `cat` ignores arguments and echoes stdin, standing in for an agent
        // that reads its prompt from stdin.
        let agent = CommandAgent::new("echoer", "cat").timeout(Duration::from_secs(5));
        let request = AgentRequest {
            location: url::Url::parse("https://forge.example.com/o/r/issues/1").unwrap(),
            message: "PING".into(),
        };
        let ctx = AgentContext::default();
        let outcome = agent.run(&request, &ctx).await.unwrap();
        // The prompt contains the message; cat echoes it back.
        assert!(outcome.success);
        assert!(outcome.summary.contains("PING"));
    }

    #[tokio::test]
    async fn streams_output_before_the_command_finishes() {
        let agent = CommandAgent::new("streamer", "sh")
            .args(["-c", "printf 'first\\n'; sleep 0.3; printf 'last\\n'"]);
        let request = AgentRequest {
            location: url::Url::parse("https://forge.example.com/o/r/issues/1").unwrap(),
            message: "go".into(),
        };
        let output = LiveOutput::default();
        let context = AgentContext {
            live_output: Some(output.clone()),
            ..Default::default()
        };
        let run = tokio::spawn(async move { agent.run(&request, &context).await });
        for _ in 0..50 {
            if output.text().contains("first") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(output.text().contains("first"));
        assert!(!run.is_finished(), "output must be visible while running");
        assert!(run.await.unwrap().unwrap().success);
        assert!(output.text().contains("last"));
    }

    #[tokio::test]
    async fn failed_command_keeps_retry_hint_from_stderr() {
        let agent = CommandAgent::new("limited", "sh")
            .args(["-c", "printf 'structured output\\n'; printf 'rate limit: try again in 2 minutes\\n' >&2; exit 1"]);
        let request = AgentRequest {
            location: url::Url::parse("https://forge.example.com/o/r/issues/1").unwrap(),
            message: "x".into(),
        };
        let outcome = agent.run(&request, &AgentContext::default()).await.unwrap();
        assert!(!outcome.success);
        assert!(outcome.summary.contains("structured output"));
        assert!(
            outcome
                .summary
                .contains("rate limit: try again in 2 minutes")
        );
    }

    #[tokio::test]
    async fn missing_program_is_reported() {
        let agent = CommandAgent::new("nope", "definitely-not-a-real-binary-xyz")
            .timeout(Duration::from_secs(5));
        let request = AgentRequest {
            location: url::Url::parse("https://forge.example.com/o/r/issues/1").unwrap(),
            message: "x".into(),
        };
        let err = agent
            .run(&request, &AgentContext::default())
            .await
            .unwrap_err();
        assert!(matches!(err, BotError::Agent { .. }));
    }

    #[tokio::test]
    async fn config_timeout_zero_disables_the_limit() {
        let config = crate::config::AgentConfig {
            timeout_secs: Some(0),
            ..Default::default()
        };
        let agent = CommandAgent::new("sleeper", "sh")
            .arg("-c")
            .arg("sleep 1; printf done")
            .apply_config(&config);
        let request = AgentRequest {
            location: url::Url::parse("https://forge.example.com/o/r/issues/1").unwrap(),
            message: "x".into(),
        };
        let outcome = agent.run(&request, &AgentContext::default()).await.unwrap();
        assert!(outcome.success);
        assert!(outcome.summary.contains("done"));
    }

    #[cfg(unix)]
    fn write_executable(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn context_for(dir: &std::path::Path) -> AgentContext {
        AgentContext {
            workspace: dir.to_path_buf(),
            repository: "o/r".into(),
            issue_number: Some(1),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn json_failure_reports_the_cli_error_without_dumping_metadata() {
        let request = AgentRequest {
            location: "https://forge.invalid/o/r/issues/2".parse().unwrap(),
            message: "go".into(),
        };
        for exit in [0, 1] {
            let agent = CommandAgent::new("fake", "sh")
                .args(["-c".to_owned(), format!("printf '%s' '{{\"type\":\"result\",\"is_error\":true,\"result\":\"Not logged in · Please run /login\",\"modelUsage\":{{}},\"session_id\":\"unused\"}}'; exit {exit}")])
                .with_json_output();
            let outcome = agent.run(&request, &AgentContext::default()).await.unwrap();
            assert!(
                !outcome.success,
                "a CLI error result must fail even with exit zero"
            );
            assert!(outcome.summary.contains("Not logged in"));
            assert!(!outcome.summary.contains("modelUsage"));
            assert!(!outcome.summary.contains("session_id"));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn agent_cannot_return_a_symlink_as_its_reply() {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("synthetic-gateway-secret");
        std::fs::write(&secret, "SYNTHETIC_GATEWAY_SECRET").unwrap();
        let agent = CommandAgent::new("fake", "/bin/sh")
            .args([
                "-c",
                "cat >/dev/null; ln -s \"$PROBE_SECRET\" \"$1\"",
                "probe",
            ])
            .env("PROBE_SECRET", secret.display().to_string())
            .session(
                SessionStyle {
                    create_args: vec!["{reply_file}".into()],
                    resume_args: vec![],
                    resume_at: None,
                    reply_from_file: true,
                    capture_id: false,
                    replace_on_resume: false,
                },
                Arc::new(SessionStore::default()),
            );
        let request = AgentRequest {
            location: "https://forge.invalid/o/r/issues/1".parse().unwrap(),
            message: "probe".into(),
        };
        let error = agent
            .run(&request, &context_for(dir.path()))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("reply file rejected"));
        assert!(!error.to_string().contains("SYNTHETIC_GATEWAY_SECRET"));
        assert_eq!(
            std::fs::read_to_string(secret).unwrap(),
            "SYNTHETIC_GATEWAY_SECRET"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sessions_are_created_then_resumed_per_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_executable(
            dir.path(),
            "fake.sh",
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$FAKE_LOG\"\ncat >/dev/null\necho REPLY\n",
        );
        let log = dir.path().join("args.log");
        let store = Arc::new(SessionStore::load(dir.path()));
        let agent = CommandAgent::new("fake", script.display().to_string())
            .env("FAKE_LOG", log.display().to_string())
            .session(
                SessionStyle {
                    create_args: vec!["--session-id".into(), "{session}".into()],
                    resume_args: vec!["--resume".into(), "{session}".into()],
                    resume_at: None,
                    reply_from_file: false,
                    capture_id: false,
                    replace_on_resume: false,
                },
                Arc::clone(&store),
            );
        let request = AgentRequest {
            location: url::Url::parse("http://forge.local/o/r/issues/1").unwrap(),
            message: "go".into(),
        };
        let context = context_for(dir.path());

        assert!(agent.run(&request, &context).await.unwrap().success);
        assert!(agent.run(&request, &context).await.unwrap().success);

        let logged = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = logged.lines().collect();
        assert_eq!(lines.len(), 2, "{logged}");
        let id = lines[0]
            .strip_prefix("--session-id ")
            .expect("first run creates the session");
        assert_eq!(lines[1], format!("--resume {id}"));

        // A different conversation gets its own session.
        let other = AgentContext {
            issue_number: Some(2),
            ..context_for(dir.path())
        };
        assert!(agent.run(&request, &other).await.unwrap().success);
        let logged = std::fs::read_to_string(&log).unwrap();
        assert!(logged.lines().last().unwrap().starts_with("--session-id "));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn codex_thread_id_is_captured_and_resumed() {
        const FAKE_CODEX: &str = r#"#!/usr/bin/env python3
import os, sys
args = sys.argv[1:]
out = None
for i, a in enumerate(args):
    if a == "-o":
        out = args[i + 1]
with open(os.environ["FAKE_LOG"], "a") as f:
    f.write(" ".join(args) + "\n")
sys.stdin.read()
print('{"type":"thread.started","thread_id":"tid-123"}')
from pathlib import Path
sessions = Path(os.environ["CODEX_HOME"]) / "sessions" / "2026" / "09" / "28"
sessions.mkdir(parents=True, exist_ok=True)
(sessions / "rollout-2026-09-28T00-00-00-tid-123.jsonl").write_text('{"type":"turn_context","payload":{"model":"test/codex-model"}}\n')
if out:
    with open(out, "w") as f:
        f.write("CODEX-REPLY")
"#;
        let dir = tempfile::tempdir().unwrap();
        let script = write_executable(dir.path(), "fake_codex.py", FAKE_CODEX);
        let log = dir.path().join("args.log");
        let store = Arc::new(SessionStore::load(dir.path()));
        let agent = CommandAgent::new("codex", script.display().to_string())
            .arg("exec")
            .env("FAKE_LOG", log.display().to_string())
            .env("CODEX_HOME", dir.path().display().to_string())
            .session(
                SessionStyle {
                    create_args: vec!["--json".into(), "-o".into(), "{reply_file}".into()],
                    resume_args: vec![
                        "exec".into(),
                        "resume".into(),
                        "{session}".into(),
                        "-o".into(),
                        "{reply_file}".into(),
                    ],
                    resume_at: None,
                    reply_from_file: true,
                    capture_id: true,
                    replace_on_resume: true,
                },
                Arc::clone(&store),
            );
        let request = AgentRequest {
            location: url::Url::parse("http://forge.local/o/r/issues/1").unwrap(),
            message: "go".into(),
        };
        let context = context_for(dir.path());

        let first = agent.run(&request, &context).await.unwrap();
        assert!(first.success);
        assert_eq!(first.summary, "CODEX-REPLY");
        assert_eq!(first.model.as_deref(), Some("test/codex-model"));
        assert_eq!(store.get("codex", "o/r:1"), Some("tid-123".to_string()));

        let second = agent.run(&request, &context).await.unwrap();
        assert!(second.success);
        assert_eq!(second.summary, "CODEX-REPLY");
        assert_eq!(second.model.as_deref(), Some("test/codex-model"));

        let logged = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = logged.lines().collect();
        assert_eq!(lines.len(), 2, "{logged}");
        assert!(lines[0].starts_with("exec --json -o "), "{}", lines[0]);
        assert!(
            lines[1].starts_with("exec resume tid-123 -o "),
            "{}",
            lines[1]
        );
    }
}

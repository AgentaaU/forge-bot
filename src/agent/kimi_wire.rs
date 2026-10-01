//! Kimi wire-mode adapter with same-turn steering.
//!
//! `kimi --wire` is a JSON-RPC 2.0 process over stdin/stdout. Wire 1.4 adds a
//! `steer` request that injects a user message into the active turn; Wire 1.5
//! adds the matching `SteerInput` consumption event. This adapter drives one
//! process per run and keeps it reachable from [`Agent::follow_up`] while its
//! prompt is outstanding.
//!
//! Wire mode is only used when the operator did not override the Kimi command
//! or arguments. When the installed CLI predates wire mode the process exits
//! before answering, and the adapter reports [`wire::unsupported`] so the
//! wrapper falls back to the one-shot `kimi --print` adapter.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio::time::Instant;

use crate::agent::prompt::{build_follow_up_prompt, build_prompt};
use crate::agent::wire::{self, WireProcess};
use crate::agent::{
    AgentContext, AgentOutcome, AgentRequest, SteerReceipt, TokenUsage, conversation_key,
};
use crate::config::AgentConfig;
use crate::error::Result;
use crate::executor::ExecSpec;

/// How long a live prompt has to acknowledge a `steer` before the delivery is
/// treated as uncertain. An explicit rejection is still queued.
const STEER_ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// A live wire process bound to one conversation.
struct LiveSession {
    process: Arc<WireProcess>,
    /// Whether a prompt is currently outstanding (and thus steerable).
    active: Mutex<bool>,
}

/// Kimi wire configuration derived from `[agents.kimi]`.
#[derive(Debug, Clone, Default)]
pub struct KimiWireConfig {
    program: String,
    args: Vec<String>,
    env: std::collections::BTreeMap<String, String>,
    timeout: Option<Duration>,
}

impl KimiWireConfig {
    fn from_agent(config: &AgentConfig) -> Self {
        Self {
            program: config.command.clone().unwrap_or_else(|| "kimi".to_owned()),
            args: config.args.clone().unwrap_or_default(),
            env: config.env.clone(),
            timeout: config
                .timeout_secs
                .filter(|seconds| *seconds != 0)
                .map(Duration::from_secs),
        }
    }
}

/// A Kimi wire-mode agent.
pub struct KimiWireAgent {
    config: KimiWireConfig,
    live: Mutex<HashMap<String, Arc<LiveSession>>>,
}

impl KimiWireAgent {
    pub fn new(config: &AgentConfig) -> Self {
        Self {
            config: KimiWireConfig::from_agent(config),
            live: Mutex::new(HashMap::new()),
        }
    }

    /// Identity of a live run: workspace, account and model must match, since
    /// the process cannot change any of them mid-turn.
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

    /// Number of live wire processes (diagnostics and tests).
    pub fn live_agents(&self) -> usize {
        self.live.lock().expect("kimi live mutex poisoned").len()
    }

    fn exec_spec(&self, request: &AgentRequest, context: &AgentContext) -> ExecSpec {
        let mut args = vec!["--wire".to_owned()];
        args.extend(self.config.args.iter().cloned());
        // A per-user model is only added when the operator has not chosen one.
        if let Some(model) = context
            .model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty())
            && crate::agent::command::model_arg(&args, true).is_none()
        {
            args.push("--model".to_owned());
            args.push(model.to_owned());
        }
        let mut env: Vec<(String, String)> = self
            .config
            .env
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        env.extend(context.environment(request));
        ExecSpec {
            program: self.config.program.clone(),
            args,
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
        let map_key = self.map_key(context);
        if let Some(stale) = self
            .live
            .lock()
            .expect("kimi live mutex poisoned")
            .remove(&map_key)
        {
            stale.process.kill();
        }

        let spec = self.exec_spec(request, context);
        let process = WireProcess::spawn("kimi", true, &spec, context.executor.as_ref())?;
        let live = Arc::new(LiveSession {
            process: Arc::clone(&process),
            active: Mutex::new(false),
        });
        self.live
            .lock()
            .expect("kimi live mutex poisoned")
            .insert(map_key.clone(), Arc::clone(&live));

        let result = self
            .run_turn(&process, &live, &prompt, context, started)
            .await;

        self.live
            .lock()
            .expect("kimi live mutex poisoned")
            .remove(&map_key);
        process.kill();
        result
    }

    async fn run_turn(
        &self,
        process: &WireProcess,
        live: &LiveSession,
        prompt: &str,
        context: &AgentContext,
        started: Instant,
    ) -> Result<AgentOutcome> {
        let deadline = self.config.timeout.map(|timeout| Instant::now() + timeout);

        // `initialize` is optional (Wire 1.1+); an older server answers with
        // method-not-found and we continue. A process that exits first means
        // the CLI does not support wire mode at all.
        let mut events = process.subscribe();
        if let Err(error) = process
            .request(
                "initialize",
                json!({
                    "protocol_version": "1.4",
                    "client": { "name": "forge-bot", "version": env!("CARGO_PKG_VERSION") },
                }),
                deadline,
            )
            .await
            && !process.is_alive()
        {
            return Err(wire::unsupported("kimi", error));
        }

        *live.active.lock().expect("kimi active mutex poisoned") = true;
        let prompt_future = process.request("prompt", json!({ "user_input": prompt }), deadline);
        tokio::pin!(prompt_future);

        let mut text = String::new();
        let mut usage = TokenUsage::default();
        let response = loop {
            tokio::select! {
                response = &mut prompt_future => break response,
                event = process.next_event(&mut events) => {
                    match event {
                        Some(event) => handle_event(process, &event, context, &mut text, &mut usage),
                        None => {
                            *live.active.lock().expect("kimi active mutex poisoned") = false;
                            return Ok(AgentOutcome::failure(
                                "kimi wire process exited before the prompt completed".to_owned(),
                                started.elapsed(),
                            ));
                        }
                    }
                }
            }
        };
        *live.active.lock().expect("kimi active mutex poisoned") = false;

        // The prompt response can be selected before events the server emitted
        // just before it. Drain whatever is already buffered so the final
        // reply text is not lost.
        loop {
            match events.try_recv() {
                Ok(event) => handle_event(process, &event, context, &mut text, &mut usage),
                Err(broadcast::error::TryRecvError::Lagged(skipped)) => {
                    tracing::debug!(skipped, "kimi event stream lagged");
                }
                Err(_) => break,
            }
        }

        let result = match response {
            Ok(result) => result,
            // The prompt is already submitted, so a failure here is an
            // execution failure, never a reason to replay the task through
            // the one-shot `kimi --print` adapter. Compatibility fallback is
            // reserved for wire-mode detection before any work is submitted.
            Err(error) => {
                return Ok(AgentOutcome::failure(error.to_string(), started.elapsed()));
            }
        };
        let status = result["status"].as_str().unwrap_or("failed");
        let mut outcome = if status == "finished" {
            AgentOutcome::success(text, started.elapsed())
        } else {
            AgentOutcome::failure(format!("kimi turn {status}"), started.elapsed())
        };
        outcome.usage = (usage.prompt_tokens > 0).then_some(usage);
        Ok(outcome)
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
            .expect("kimi live mutex poisoned")
            .get(&map_key)
            .cloned()
        else {
            return Ok(None);
        };
        if !*live.active.lock().expect("kimi active mutex poisoned") {
            return Ok(None);
        }

        // An explicit rejection is safely queued as the next turn, but a steer
        // written without an acknowledgment may already have been consumed, so
        // it is reported as uncertain and never replayed.
        let deadline = Some(Instant::now() + ack_timeout);
        match live
            .process
            .request(
                "steer",
                json!({ "user_input": build_follow_up_prompt(request) }),
                deadline,
            )
            .await
        {
            Ok(result) if result["status"].as_str() == Some("steered") => {
                tracing::info!(key = %conversation_key(context), "steered the live kimi turn");
                Ok(Some(SteerReceipt::merged()))
            }
            Ok(result) => {
                tracing::warn!(?result, "kimi did not accept the steer; queueing it");
                Ok(None)
            }
            Err(error) if wire::is_uncertain(&error) => {
                tracing::warn!(%error, "kimi steer acknowledgment was not confirmed; not replaying it");
                Ok(Some(SteerReceipt::uncertain()))
            }
            Err(error) => {
                tracing::warn!(%error, "kimi rejected the follow-up; queueing it");
                Ok(None)
            }
        }
    }
}

/// Interpret one wire notification, accumulating reply text and token usage.
fn handle_event(
    process: &WireProcess,
    event: &Value,
    context: &AgentContext,
    text: &mut String,
    usage: &mut TokenUsage,
) {
    match event["method"].as_str() {
        Some("event") => {
            let params = &event["params"];
            match params["type"].as_str() {
                Some("ContentPart") => {
                    let payload = &params["payload"];
                    if payload["type"] == "text"
                        && let Some(delta) = payload["text"].as_str()
                    {
                        text.push_str(delta);
                        if let Some(live_output) = &context.live_output {
                            live_output.append(delta.as_bytes());
                        }
                    }
                }
                Some("StatusUpdate") => {
                    if let Some(tokens) = params["payload"]["token_usage"].as_object() {
                        let read = tokens
                            .get("input_cache_read")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        let created = tokens
                            .get("input_cache_creation")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        let other = tokens
                            .get("input_other")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        *usage = TokenUsage {
                            prompt_tokens: other + read + created,
                            cached_tokens: read,
                        };
                    }
                }
                _ => {}
            }
        }
        Some("request") => answer_server_request(process, event),
        _ => {}
    }
}

/// Answer a server-initiated request so an unattended turn cannot hang.
///
/// The bot has no interactive user, so approvals are granted, questions are
/// dismissed, and external tool calls (which the bot never registers) fail.
fn answer_server_request(process: &WireProcess, event: &Value) {
    let params = &event["params"];
    let payload = &params["payload"];
    let request_id = payload["id"].clone();
    let result = match params["type"].as_str() {
        Some("ApprovalRequest") => json!({
            "request_id": request_id,
            "response": "approve",
        }),
        Some("QuestionRequest") => json!({
            "request_id": request_id,
            "answers": {},
        }),
        Some("ToolCallRequest") => json!({
            "tool_call_id": request_id,
            "return_value": {
                "is_error": true,
                "output": "",
                "message": "external tools are not available to forge-bot",
            },
        }),
        Some("HookRequest") => json!({
            "request_id": request_id,
            "action": "allow",
        }),
        other => {
            tracing::debug!(?other, "ignoring unknown kimi server request");
            json!({})
        }
    };
    process.respond(&event["id"], result);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script() -> String {
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/fake_kimi_wire.py"
        )
        .into()
    }

    fn context(dir: &std::path::Path) -> AgentContext {
        AgentContext {
            workspace: dir.to_path_buf(),
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

    fn config(log: &std::path::Path) -> AgentConfig {
        AgentConfig {
            command: Some(script()),
            env: [("FAKE_KIMI_LOG".into(), log.display().to_string())].into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn runs_a_turn_and_reports_text_and_cache_usage() {
        let dir = tempfile::tempdir().unwrap();
        let agent = KimiWireAgent::new(&config(&dir.path().join("log.jsonl")));
        let outcome = agent.run(&request(), &context(dir.path())).await.unwrap();
        assert!(outcome.success, "{outcome:?}");
        assert_eq!(outcome.summary, "KIMI-REPLY");
        let usage = outcome.usage.expect("usage");
        assert_eq!(usage.prompt_tokens, 1_000);
        assert_eq!(usage.cached_tokens, 250);
    }

    #[tokio::test]
    async fn steers_a_live_turn_without_a_second_process() {
        let dir = tempfile::tempdir().unwrap();
        let steer_log = dir.path().join("steer.log");
        let mut cfg = config(&dir.path().join("log.jsonl"));
        cfg.env
            .insert("FAKE_KIMI_WAIT_FOR_STEER".into(), "1".into());
        cfg.env.insert(
            "FAKE_KIMI_STEER_LOG".into(),
            steer_log.display().to_string(),
        );
        let agent = Arc::new(KimiWireAgent::new(&cfg));
        let ctx = context(dir.path());

        let run = {
            let agent = Arc::clone(&agent);
            let request = request();
            let ctx = ctx.clone();
            tokio::spawn(async move { agent.run(&request, &ctx).await })
        };
        let mut active = false;
        for _ in 0..500 {
            if let Some(live) = agent
                .live
                .lock()
                .unwrap()
                .get(&agent.map_key(&ctx))
                .cloned()
                && *live.active.lock().unwrap()
            {
                active = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(active, "kimi prompt should be active");

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
            .expect("a live prompt should accept the steer");
        assert!(receipt.notice.contains("Merged"));

        let outcome = run.await.unwrap().unwrap();
        assert!(outcome.success, "{outcome:?}");
        assert!(
            outcome.summary.contains("also run the linter"),
            "{outcome:?}"
        );
        assert!(steer_log.exists());
        assert_eq!(agent.live_agents(), 0);
    }

    #[tokio::test]
    async fn follow_up_without_a_live_prompt_is_queued() {
        let dir = tempfile::tempdir().unwrap();
        let agent = KimiWireAgent::new(&config(&dir.path().join("log.jsonl")));
        assert!(
            agent
                .follow_up(&request(), &context(dir.path()))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn passes_a_per_user_model_on_the_command_line() {
        let dir = tempfile::tempdir().unwrap();
        let agent = KimiWireAgent::new(&config(&dir.path().join("log.jsonl")));
        let mut ctx = context(dir.path());
        ctx.model = Some("kimi-fast".into());
        let spec = agent.exec_spec(&request(), &ctx);
        assert!(
            spec.args
                .windows(2)
                .any(|pair| pair == ["--model", "kimi-fast"])
        );
        // An operator-supplied model wins.
        let mut explicit = config(&dir.path().join("log.jsonl"));
        explicit.args = Some(vec!["--wire".into(), "--model".into(), "operator".into()]);
        let agent = KimiWireAgent::new(&explicit);
        let spec = agent.exec_spec(&request(), &ctx);
        assert!(!spec.args.contains(&"kimi-fast".to_owned()));
        assert!(spec.args.contains(&"operator".to_owned()));
    }

    #[tokio::test]
    async fn a_cancelled_turn_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = config(&dir.path().join("log.jsonl"));
        cfg.env
            .insert("FAKE_KIMI_STATUS".into(), "cancelled".into());
        let agent = KimiWireAgent::new(&cfg);
        let outcome = agent.run(&request(), &context(dir.path())).await.unwrap();
        assert!(!outcome.success);
        assert!(outcome.summary.contains("cancelled"), "{outcome:?}");
    }

    #[tokio::test]
    async fn a_server_request_is_answered() {
        let dir = tempfile::tempdir().unwrap();
        let approval_log = dir.path().join("approval.log");
        let mut cfg = config(&dir.path().join("log.jsonl"));
        cfg.env.insert("FAKE_KIMI_APPROVAL".into(), "1".into());
        cfg.env.insert(
            "FAKE_KIMI_APPROVAL_LOG".into(),
            approval_log.display().to_string(),
        );
        let agent = KimiWireAgent::new(&cfg);
        let outcome = agent.run(&request(), &context(dir.path())).await.unwrap();
        assert!(outcome.success, "{outcome:?}");
        let logged = std::fs::read_to_string(&approval_log).unwrap();
        assert!(logged.contains("approve"), "{logged}");
    }

    #[tokio::test]
    async fn a_rejected_steer_is_queued() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = config(&dir.path().join("log.jsonl"));
        cfg.env
            .insert("FAKE_KIMI_WAIT_FOR_STEER".into(), "1".into());
        cfg.env.insert("FAKE_KIMI_REJECT_STEER".into(), "1".into());
        let agent = Arc::new(KimiWireAgent::new(&cfg));
        let ctx = context(dir.path());
        let run = {
            let agent = Arc::clone(&agent);
            let request = request();
            let ctx = ctx.clone();
            tokio::spawn(async move { agent.run(&request, &ctx).await })
        };
        for _ in 0..500 {
            let live = agent
                .live
                .lock()
                .unwrap()
                .get(&agent.map_key(&ctx))
                .cloned();
            if live.is_some_and(|live| *live.active.lock().unwrap()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(agent.follow_up(&request(), &ctx).await.unwrap().is_none());
        run.await.unwrap().unwrap();
    }

    /// Wait until the spawned run has a prompt accepting steers.
    async fn wait_for_active_prompt(agent: &Arc<KimiWireAgent>, ctx: &AgentContext) {
        for _ in 0..500 {
            let live = agent.live.lock().unwrap().get(&agent.map_key(ctx)).cloned();
            if live.is_some_and(|live| *live.active.lock().unwrap()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("kimi prompt did not become active");
    }

    #[tokio::test]
    async fn an_unacknowledged_steer_is_not_replayed() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = config(&dir.path().join("log.jsonl"));
        cfg.env
            .insert("FAKE_KIMI_WAIT_FOR_STEER".into(), "1".into());
        cfg.env.insert("FAKE_KIMI_STEER_DELAY".into(), "1".into());
        let agent = Arc::new(KimiWireAgent::new(&cfg));
        let ctx = context(dir.path());
        let run = {
            let agent = Arc::clone(&agent);
            let request = request();
            let ctx = ctx.clone();
            tokio::spawn(async move { agent.run(&request, &ctx).await })
        };
        wait_for_active_prompt(&agent, &ctx).await;

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
        let mut cfg = config(&dir.path().join("log.jsonl"));
        cfg.env
            .insert("FAKE_KIMI_WAIT_FOR_STEER".into(), "1".into());
        cfg.env.insert("FAKE_KIMI_STEER_EXIT".into(), "1".into());
        let agent = Arc::new(KimiWireAgent::new(&cfg));
        let ctx = context(dir.path());
        let run = {
            let agent = Arc::clone(&agent);
            let request = request();
            let ctx = ctx.clone();
            tokio::spawn(async move { agent.run(&request, &ctx).await })
        };
        wait_for_active_prompt(&agent, &ctx).await;

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
    async fn reply_text_is_streamed_to_live_output() {
        let dir = tempfile::tempdir().unwrap();
        let agent = KimiWireAgent::new(&config(&dir.path().join("log.jsonl")));
        let output = crate::agent::LiveOutput::default();
        let ctx = AgentContext {
            live_output: Some(output.clone()),
            ..context(dir.path())
        };
        let outcome = agent.run(&request(), &ctx).await.unwrap();
        assert!(outcome.success);
        assert!(output.text().contains("KIMI-REPLY"));
    }

    #[tokio::test]
    async fn answers_every_server_request_kind() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("echo.log");
        let script = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/fake_wire_echo.py"
        );
        let spec = ExecSpec {
            program: script.into(),
            env: vec![("FAKE_ECHO_LOG".into(), log.display().to_string())],
            ..Default::default()
        };
        let process =
            WireProcess::spawn("echo", true, &spec, &crate::executor::Executor::direct()).unwrap();
        for (kind, id) in [
            ("ApprovalRequest", "a"),
            ("QuestionRequest", "q"),
            ("ToolCallRequest", "t"),
            ("HookRequest", "h"),
            ("SomethingElse", "o"),
        ] {
            let event = json!({
                "method": "request",
                "id": format!("srv-{id}"),
                "params": { "type": kind, "payload": { "id": id } },
            });
            answer_server_request(&process, &event);
        }
        for _ in 0..200 {
            if std::fs::read_to_string(&log)
                .map(|text| text.contains("srv-o"))
                .unwrap_or(false)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let logged = std::fs::read_to_string(&log).unwrap();
        for id in ["srv-a", "srv-q", "srv-t", "srv-h", "srv-o"] {
            assert!(logged.contains(id), "{id} missing from {logged}");
        }
        process.kill();
    }

    #[tokio::test]
    async fn a_cli_without_wire_mode_is_reported_unsupported() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("kimi.sh");
        std::fs::write(&fake, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cfg = AgentConfig {
            command: Some(fake.display().to_string()),
            ..Default::default()
        };
        let agent = KimiWireAgent::new(&cfg);
        let error = agent
            .run(&request(), &context(dir.path()))
            .await
            .unwrap_err();
        assert!(wire::is_unsupported(&error), "{error}");
    }
}

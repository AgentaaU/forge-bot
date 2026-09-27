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

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::agent::prompt::build_prompt;
use crate::agent::session::SessionStore;
use crate::agent::{Agent, AgentContext, AgentOutcome, AgentRequest, conversation_key};
use crate::config::PromptDelivery;
use crate::error::{BotError, Result};

/// Maximum number of characters of captured output kept in the summary.
const OUTPUT_LIMIT: usize = 4000;

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

/// Find the value of a `--model` / `-m` flag in an argument list.
///
/// Handles the `--model value`, `--model=value`, `-m value` and `-m=value`
/// forms used by the CLI agents.
fn extract_model(args: &[String]) -> Option<&str> {
    for (index, arg) in args.iter().enumerate() {
        if let Some(value) = arg
            .strip_prefix("--model=")
            .or_else(|| arg.strip_prefix("-m="))
            && !value.is_empty()
        {
            return Some(value);
        }
        if (arg == "--model" || arg == "-m")
            && let Some(value) = args.get(index + 1)
        {
            return Some(value);
        }
    }
    None
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
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
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
                plan.args = interpolate(&style.resume_args, &id, reply.as_deref());
                plan.at = style.resume_at;
                plan.replace_base = style.replace_on_resume;
            }
            None => {
                let id = if style.capture_id {
                    String::new()
                } else {
                    continuation.store.deterministic_id(&self.name, &key)
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

    fn model(&self) -> Option<&str> {
        extract_model(&self.args)
    }

    async fn run(&self, request: &AgentRequest, context: &AgentContext) -> Result<AgentOutcome> {
        let workspace: PathBuf = context.workspace.clone();
        if !workspace.as_os_str().is_empty() {
            tokio::fs::create_dir_all(&workspace).await?;
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

        let mut cmd = Command::new(&self.program);
        cmd.args(&args)
            .envs(self.env.clone())
            .envs(context.environment(request))
            .stdin(if self.prompt == PromptDelivery::Stdin {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        if !workspace.as_os_str().is_empty() {
            cmd.current_dir(&workspace);
        }

        if self.prompt == PromptDelivery::Arg {
            cmd.arg(&prompt);
        }

        tracing::info!(
            agent = %self.name,
            program = %self.program,
            workspace = %workspace.display(),
            "starting agent"
        );

        let program = self.program.clone();
        let name = self.name.clone();
        let prompt_for_spawn = prompt.clone();

        let run = async move {
            let mut child = cmd.spawn().map_err(|e| BotError::Agent {
                name: name.clone(),
                reason: format!("failed to spawn `{program}`: {e}"),
            })?;

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

            child.wait_with_output().await.map_err(|e| BotError::Agent {
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
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let fallback = summarize(&stdout, &stderr);

                if output.status.success() {
                    let summary = match &plan.reply_file {
                        Some(path) => {
                            let text = std::fs::read_to_string(path).unwrap_or_default();
                            let _ = std::fs::remove_file(path);
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
                    Ok(AgentOutcome::success(summary, started.elapsed()))
                } else {
                    if let Some(path) = &plan.reply_file {
                        let _ = std::fs::remove_file(path);
                    }
                    Ok(AgentOutcome::failure(
                        format!(
                            "agent exited with {}: {}",
                            output
                                .status
                                .code()
                                .map(|c| c.to_string())
                                .unwrap_or_else(|| "signal".into()),
                            fallback
                        ),
                        started.elapsed(),
                    ))
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
    fn extracts_model_from_arguments() {
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            extract_model(&args(&["exec", "--model", "gpt-5"])),
            Some("gpt-5")
        );
        assert_eq!(
            extract_model(&args(&["exec", "--model=gpt-5"])),
            Some("gpt-5")
        );
        assert_eq!(extract_model(&args(&["-m", "gpt-5"])), Some("gpt-5"));
        assert_eq!(extract_model(&args(&["-m=gpt-5"])), Some("gpt-5"));
        assert_eq!(extract_model(&args(&["--model"])), None);
        assert_eq!(extract_model(&args(&["--model="])), None);
        assert_eq!(extract_model(&args(&["exec"])), None);
    }

    #[test]
    fn apply_config_reports_the_model_without_changing_the_arguments() {
        let config = crate::config::AgentConfig {
            args: Some(vec!["exec".into(), "--model".into(), "gpt-5-codex".into()]),
            ..Default::default()
        };
        let agent = CommandAgent::new("codex", "codex").apply_config(&config);
        // The model is only reported, never injected: the arguments are exactly
        // what the configuration asked for.
        assert_eq!(agent.model(), Some("gpt-5-codex"));
        assert_eq!(
            agent.arguments().to_vec(),
            vec![
                "exec".to_string(),
                "--model".to_string(),
                "gpt-5-codex".to_string()
            ]
        );
    }

    #[test]
    fn model_in_arguments_is_reported_without_config() {
        let agent = CommandAgent::new("codex", "codex").args(["--model", "o3"]);
        assert_eq!(agent.model(), Some("o3"));
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
        assert_eq!(store.get("codex", "o/r:1"), Some("tid-123".to_string()));

        let second = agent.run(&request, &context).await.unwrap();
        assert!(second.success);
        assert_eq!(second.summary, "CODEX-REPLY");

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

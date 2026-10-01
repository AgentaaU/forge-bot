//! Codex CLI adapter.
//!
//! `codex exec` runs non-interactively and reads the prompt from stdin when no
//! prompt argument is given. It always runs with
//! `--dangerously-bypass-approvals-and-sandbox` (issue #33): the bot is
//! non-interactive, and the Linux sandbox needs `bwrap`, which is unavailable
//! on some hosts. Codex is never run inside its sandbox.
//!
//! Codex persists each conversation under a `thread_id`. A later comment in the
//! same thread resumes it with `codex exec resume <id>`, which keeps the model
//! context (and the provider's prompt cache) warm.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::agent::codex_app_server::CodexAppServerAgent;
use crate::agent::command::{CommandAgent, SessionStyle};
use crate::agent::session::SessionStore;
use crate::agent::{Agent, AgentContext, AgentOutcome, AgentRequest, SteerReceipt, wire};
use crate::config::{AgentConfig, PromptDelivery};
use crate::error::Result;

/// Built-in Codex adapter defaults.
pub fn default_agent() -> CommandAgent {
    CommandAgent::new("codex", "codex")
        .args(["exec", "--skip-git-repo-check", "--color", "never"])
        .prompt(PromptDelivery::Stdin)
}

/// Build a Codex adapter, applying user overrides.
///
/// The default binary is driven through `codex app-server`, which supports
/// same-turn steering. A custom `command` or `args` keeps the one-shot
/// `codex exec` adapter, and an app-server that cannot start falls back to it
/// at run time; either way a supported default binary is never broken by the
/// richer path.
pub fn build(config: &AgentConfig, sessions: Arc<SessionStore>) -> CodexAgent {
    let one_shot = build_one_shot(config, Arc::clone(&sessions));
    let app_server = (config.command.is_none() && config.args.is_none())
        .then(|| CodexAppServerAgent::new(config, Arc::clone(&sessions)));
    CodexAgent {
        one_shot,
        app_server,
    }
}

/// Build the one-shot `codex exec` adapter.
pub fn build_one_shot(config: &AgentConfig, sessions: Arc<SessionStore>) -> CommandAgent {
    let agent = default_agent()
        .apply_config(config)
        .arg("--dangerously-bypass-approvals-and-sandbox");

    // `codex exec resume` rejects `--color`/`--sandbox`, so use the standalone
    // resume command while preserving the unconditional permission bypass.
    let resume_args = vec![
        "exec".into(),
        "resume".into(),
        "{session}".into(),
        "--skip-git-repo-check".into(),
        "-o".into(),
        "{reply_file}".into(),
        "--dangerously-bypass-approvals-and-sandbox".into(),
    ];

    agent.session(
        SessionStyle {
            // A fresh conversation reports its id on stdout as `thread.started`
            // and writes the final message to `-o`.
            create_args: vec!["--json".into(), "-o".into(), "{reply_file}".into()],
            resume_args,
            resume_at: None,
            reply_from_file: true,
            capture_id: true,
            replace_on_resume: true,
        },
        sessions,
    )
}

/// The registered Codex adapter: app-server steering with a one-shot fallback.
pub struct CodexAgent {
    one_shot: CommandAgent,
    app_server: Option<CodexAppServerAgent>,
}

impl CodexAgent {
    /// Arguments the one-shot adapter would pass to the program.
    pub fn arguments(&self) -> &[String] {
        self.one_shot.arguments()
    }

    /// Program the one-shot adapter would execute.
    pub fn program(&self) -> &str {
        self.one_shot.program()
    }
}

#[async_trait::async_trait]
impl Agent for CodexAgent {
    fn name(&self) -> &str {
        "codex"
    }

    async fn run(&self, request: &AgentRequest, context: &AgentContext) -> Result<AgentOutcome> {
        if let Some(app_server) = &self.app_server {
            match app_server.run(request, context).await {
                Ok(outcome) => return Ok(outcome),
                Err(error) if wire::is_unsupported(&error) => {
                    tracing::info!(
                        %error,
                        "codex app-server is unavailable; falling back to codex exec"
                    );
                }
                Err(error) => return Err(error),
            }
        }
        self.one_shot.run(request, context).await
    }

    async fn follow_up(
        &self,
        request: &AgentRequest,
        context: &AgentContext,
    ) -> Result<Option<SteerReceipt>> {
        match &self.app_server {
            Some(app_server) => app_server.follow_up(request, context).await,
            None => Ok(None),
        }
    }
}

/// Read the model Codex recorded for its most recent turn in this thread.
/// Codex's `exec --json` stream currently omits the model, but the rollout
/// records the effective model in each `turn_context`.
pub(crate) fn model_from_session(thread_id: &str, home: Option<&Path>) -> Option<String> {
    let root = home.map(Path::to_path_buf).or_else(|| {
        std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
    })?;
    let mut dirs = vec![root.join("sessions")];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(&format!("-{thread_id}.jsonl")))
            {
                let contents = std::fs::read_to_string(path).ok()?;
                return contents.lines().rev().find_map(|line| {
                    let value: serde_json::Value = serde_json::from_str(line).ok()?;
                    (value["type"] == "turn_context")
                        .then(|| value["payload"]["model"].as_str().map(str::to_owned))
                        .flatten()
                });
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Agent, AgentContext, AgentRequest};

    fn store() -> Arc<SessionStore> {
        Arc::new(SessionStore::default())
    }

    #[test]
    fn reads_latest_model_from_codex_rollout() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions/2026/09/28");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join("rollout-2026-09-28T00-00-00-thread-123.jsonl"),
            "{\"type\":\"turn_context\",\"payload\":{\"model\":\"old\"}}\n{\"type\":\"event_msg\"}\n{\"type\":\"turn_context\",\"payload\":{\"model\":\"new\"}}\n",
        ).unwrap();
        assert_eq!(
            model_from_session("thread-123", Some(dir.path())).as_deref(),
            Some("new")
        );
        assert_eq!(model_from_session("other", Some(dir.path())), None);
    }

    #[test]
    fn defaults_use_codex_exec_with_stdin_prompt() {
        let agent = default_agent();
        assert_eq!(agent.name(), "codex");
        assert_eq!(agent.program(), "codex");
        assert_eq!(
            &agent.arguments()[..3],
            ["exec", "--skip-git-repo-check", "--color"]
        );
    }

    #[test]
    fn always_bypasses_the_sandbox() {
        for config in [
            AgentConfig::default(),
            AgentConfig {
                dangerously_skip_permissions: Some(false),
                ..Default::default()
            },
        ] {
            let agent = build(&config, store());
            assert!(
                agent
                    .arguments()
                    .iter()
                    .any(|a| a == "--dangerously-bypass-approvals-and-sandbox"),
                "codex must always bypass approvals and its sandbox"
            );
            assert!(!agent.arguments().iter().any(|a| a == "--sandbox"));
        }
    }

    #[test]
    fn user_args_override_defaults_but_still_get_the_bypass() {
        let config = AgentConfig {
            args: Some(vec!["exec".into(), "-".into()]),
            ..Default::default()
        };
        let agent = build(&config, store());
        assert_eq!(&agent.arguments()[..2], ["exec", "-"]);
        assert!(
            agent
                .arguments()
                .iter()
                .any(|a| a == "--dangerously-bypass-approvals-and-sandbox")
        );
    }

    #[cfg(unix)]
    fn write_executable(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn session_create_and_resume_always_bypass_without_color_or_sandbox_flags() {
        const FAKE_CODEX: &str = r##"#!/usr/bin/env python3
import json, os, sys
args = sys.argv[1:]
with open(os.environ["FAKE_LOG"], "a") as f:
    f.write(json.dumps(args) + "\n")
out = args[args.index("-o") + 1]
sys.stdin.read()
print(json.dumps({"type": "thread.started", "thread_id": "tid-123"}))
print(json.dumps({"type": "turn.completed", "usage": {"input_tokens": 1000, "cached_input_tokens": 750, "output_tokens": 5}}))
with open(out, "w") as f:
    f.write("CODEX-REPLY")
"##;
        let dir = tempfile::tempdir().unwrap();
        let script = write_executable(dir.path(), "fake_codex.py", FAKE_CODEX);
        let log = dir.path().join("args.jsonl");
        let config = AgentConfig {
            command: Some(script.display().to_string()),
            dangerously_skip_permissions: Some(false),
            env: [("FAKE_LOG".into(), log.display().to_string())].into(),
            ..Default::default()
        };
        let sessions = Arc::new(SessionStore::load(dir.path()));
        let agent = build(&config, Arc::clone(&sessions));
        let request = AgentRequest {
            location: url::Url::parse("http://forge.local/o/r/issues/1").unwrap(),
            message: "go".into(),
        };
        let context = AgentContext {
            workspace: dir.path().to_path_buf(),
            repository: "o/r".into(),
            issue_number: Some(1),
            ..Default::default()
        };

        let first = agent.run(&request, &context).await.unwrap();
        assert!(first.success);
        let first_usage = first.usage.expect("codex reports prompt usage");
        assert_eq!(first_usage.prompt_tokens, 1_000);
        assert_eq!(first_usage.cached_tokens, 750);
        let second = agent.run(&request, &context).await.unwrap();
        assert!(second.success);
        assert_eq!(second.usage.unwrap().hit_rate(), Some(75.0));

        let logged = std::fs::read_to_string(&log).unwrap();
        let calls: Vec<Vec<String>> = logged
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(calls.len(), 2, "{logged}");
        assert!(calls[0].starts_with(&[
            "exec".into(),
            "--skip-git-repo-check".into(),
            "--color".into(),
            "never".into(),
        ]));
        assert!(calls[0].contains(&"--dangerously-bypass-approvals-and-sandbox".into()));
        assert!(calls[0].contains(&"--json".into()));
        assert_eq!(calls[1][..3], ["exec", "resume", "tid-123"]);
        assert!(calls[1].contains(&"--dangerously-bypass-approvals-and-sandbox".into()));
        assert!(
            !calls[1]
                .iter()
                .any(|arg| arg == "--color" || arg == "--sandbox")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn app_server_failure_falls_back_to_one_shot() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_executable(
            dir.path(),
            "fallback.py",
            "#!/usr/bin/env python3\nimport json,sys\nargs=sys.argv[1:]\nout=args[args.index('-o')+1]\nsys.stdin.read()\nprint(json.dumps({'type':'thread.started','thread_id':'tid-1'}))\nprint(json.dumps({'type':'turn.completed','usage':{'input_tokens':1,'cached_input_tokens':0,'output_tokens':1}}))\nopen(out,'w').write('FELL-BACK')\n",
        );
        let sessions = Arc::new(SessionStore::default());
        let one_shot = build_one_shot(
            &AgentConfig {
                command: Some(script.display().to_string()),
                ..Default::default()
            },
            Arc::clone(&sessions),
        );
        let app_server = CodexAppServerAgent::new(
            &AgentConfig {
                command: Some("/nonexistent/codex".into()),
                ..Default::default()
            },
            sessions,
        );
        let agent = CodexAgent {
            one_shot,
            app_server: Some(app_server),
        };
        let request = AgentRequest {
            location: "https://forge.example.com/o/r/issues/1".parse().unwrap(),
            message: "go".into(),
        };
        let context = AgentContext {
            workspace: dir.path().to_path_buf(),
            repository: "o/r".into(),
            issue_number: Some(1),
            ..Default::default()
        };
        let outcome = agent.run(&request, &context).await.unwrap();
        assert!(outcome.success, "{outcome:?}");
        assert_eq!(outcome.summary, "FELL-BACK");
    }
}

//! Claude Code adapter.
//!
//! `claude --print <prompt>` runs non-interactively. It bypasses permission
//! checks by default (issue #33); `dangerously_skip_permissions = false` opts
//! out. The first comment in a thread creates the session with `--session-id`,
//! later comments resume it with `--resume`, so the model keeps the conversation
//! context.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::agent::command::{CommandAgent, SessionStyle};
use crate::agent::session::SessionStore;
use crate::config::{AgentConfig, PromptDelivery};

/// Built-in Claude Code adapter defaults.
pub fn default_agent() -> CommandAgent {
    CommandAgent::new("claude", "claude")
        .args(["--print"])
        .prompt(PromptDelivery::Arg)
}

/// Build a Claude Code adapter, applying user overrides.
pub fn build(config: &AgentConfig, sessions: Arc<SessionStore>) -> CommandAgent {
    let auto = config.dangerously_skip_permissions.unwrap_or(true);
    let agent = default_agent()
        .apply_config(config)
        .dangerously_skip_permissions(auto)
        .session(
            SessionStyle {
                create_args: vec!["--session-id".into(), "{session}".into()],
                resume_args: vec!["--resume".into(), "{session}".into()],
                resume_at: None,
                reply_from_file: false,
                capture_id: false,
                replace_on_resume: false,
            },
            sessions,
        );
    if auto {
        agent.arg("--dangerously-skip-permissions")
    } else {
        agent
    }
}

/// Read the model used by the latest assistant turn in a Claude session.
/// The print-mode result does not identify it, but the session transcript does.
pub(crate) fn model_from_session(session_id: &str, config_dir: Option<&Path>) -> Option<String> {
    let root = config_dir.map(Path::to_path_buf).or_else(|| {
        std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude")))
    })?;
    let entries = std::fs::read_dir(root.join("projects")).ok()?;
    for entry in entries.flatten() {
        let path = entry.path().join(format!("{session_id}.jsonl"));
        let Ok(contents) = std::fs::read_to_string(path) else {
            continue;
        };
        return contents.lines().rev().find_map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).ok()?;
            if value["type"] != "assistant" {
                return None;
            }
            value["message"]["model"]
                .as_str()
                .filter(|model| !model.is_empty() && *model != "<synthetic>")
                .map(str::to_owned)
        });
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
    fn reads_last_assistant_model_from_session() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("projects/-tmp-project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("session-123.jsonl"),
            "{\"type\":\"assistant\",\"message\":{\"model\":\"claude-old\"}}\n{\"type\":\"user\"}\n{\"type\":\"assistant\",\"message\":{\"model\":\"claude-new\"}}\n").unwrap();
        assert_eq!(
            model_from_session("session-123", Some(dir.path())).as_deref(),
            Some("claude-new")
        );
        assert_eq!(model_from_session("missing", Some(dir.path())), None);
    }

    #[test]
    fn skips_permissions_by_default() {
        let agent = build(&AgentConfig::default(), store());
        assert!(
            agent
                .arguments()
                .iter()
                .any(|arg| arg == "--dangerously-skip-permissions")
        );
    }

    #[test]
    fn can_opt_out_of_skipping_permissions() {
        let config = AgentConfig {
            dangerously_skip_permissions: Some(false),
            ..Default::default()
        };
        let agent = build(&config, store());
        assert!(
            !agent
                .arguments()
                .iter()
                .any(|arg| arg == "--dangerously-skip-permissions")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn records_model_from_session_without_changing_reply() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-claude.sh");
        std::fs::write(&script, "#!/bin/sh\nwhile [ $# -gt 0 ]; do\n  if [ \"$1\" = --session-id ] || [ \"$1\" = --resume ]; then id=$2; break; fi\n  shift\ndone\nmkdir -p \"$CLAUDE_CONFIG_DIR/projects/test\"\nprintf '%s\\n' '{\"type\":\"assistant\",\"message\":{\"model\":\"claude-test-model\"}}' > \"$CLAUDE_CONFIG_DIR/projects/test/$id.jsonl\"\nprintf 'CLAUDE-REPLY'\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let agent = build(
            &AgentConfig {
                command: Some(script.display().to_string()),
                env: [("CLAUDE_CONFIG_DIR".into(), dir.path().display().to_string())].into(),
                ..Default::default()
            },
            Arc::new(SessionStore::load(dir.path())),
        );
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
        assert!(outcome.success);
        assert_eq!(outcome.summary, "CLAUDE-REPLY");
        assert_eq!(outcome.model.as_deref(), Some("claude-test-model"));
    }
}

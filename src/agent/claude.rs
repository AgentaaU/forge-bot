//! Claude Code adapter.
//!
//! `claude --print <prompt>` runs non-interactively. It bypasses permission
//! checks by default (issue #33); `dangerously_skip_permissions = false` opts
//! out. The first comment in a thread creates the session with `--session-id`,
//! later comments resume it with `--resume`, so the model keeps the conversation
//! context. New attempts use fresh session IDs: a failed CLI can leave a local
//! session behind even though the gateway has no successful mapping to resume.

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
        .with_json_output()
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
    use crate::agent::{Agent, AgentContext, AgentRequest, TokenUsage};

    fn store() -> Arc<SessionStore> {
        Arc::new(SessionStore::default())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_creation_does_not_collide_on_retry_and_success_resumes() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-claude.sh");
        std::fs::write(
            &script,
            r#"#!/bin/sh
while [ $# -gt 0 ]; do
    case "$1" in
        --session-id|--resume) mode=$1; id=$2; break ;;
    esac
    shift
done
printf '%s %s\n' "$mode" "$id" >> "$FAKE_STATE/args.log"
if [ "$mode" = --session-id ]; then
    if [ -e "$FAKE_STATE/session-$id" ]; then
        echo "Error: Session ID $id is already in use." >&2
        exit 1
    fi
    touch "$FAKE_STATE/session-$id"
    if [ ! -e "$FAKE_STATE/failed-once" ]; then
        touch "$FAKE_STATE/failed-once"
        echo 'Not logged in · Please run /login' >&2
        exit 1
    fi
elif [ ! -e "$FAKE_STATE/session-$id" ]; then
    echo 'No session to resume' >&2
    exit 1
fi
echo '{"type":"result","result":"ok"}'
"#,
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config = AgentConfig {
            command: Some(script.display().to_string()),
            env: [("FAKE_STATE".into(), dir.path().display().to_string())].into(),
            ..Default::default()
        };
        let request = AgentRequest {
            location: "https://forge.invalid/o/r/issues/2".parse().unwrap(),
            message: "research".into(),
        };
        let context = AgentContext {
            workspace: dir.path().to_owned(),
            repository: "o/r".into(),
            issue_number: Some(2),
            user_id: Some("reviewer".into()),
            ..Default::default()
        };
        let agent = build(&config, Arc::new(SessionStore::load(dir.path())));
        let failed = agent.run(&request, &context).await.unwrap();
        assert!(!failed.success);
        assert!(failed.summary.contains("Not logged in"));
        // Simulate restarting the gateway after the failed creation: the CLI
        // has retained its session, but the gateway has no successful mapping.
        let agent = build(&config, Arc::new(SessionStore::load(dir.path())));
        let retry = agent.run(&request, &context).await.unwrap();
        assert!(retry.success, "{}", retry.summary);
        // Successful sessions still survive gateway restarts and resume.
        let agent = build(&config, Arc::new(SessionStore::load(dir.path())));
        assert!(agent.run(&request, &context).await.unwrap().success);
        let log = std::fs::read_to_string(dir.path().join("args.log")).unwrap();
        let lines: Vec<_> = log.lines().collect();
        assert_eq!(lines.len(), 3);
        let first = lines[0].strip_prefix("--session-id ").unwrap();
        let retry = lines[1].strip_prefix("--session-id ").unwrap();
        assert_ne!(first, retry);
        assert_eq!(lines[2], format!("--resume {retry}"));
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
    async fn records_model_from_session_and_usage_from_json_result() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-claude.sh");
        std::fs::write(&script, "#!/bin/sh\nwhile [ $# -gt 0 ]; do\n  if [ \"$1\" = --session-id ] || [ \"$1\" = --resume ]; then id=$2; break; fi\n  shift\ndone\nmkdir -p \"$CLAUDE_CONFIG_DIR/projects/test\"\nprintf '%s\\n' '{\"type\":\"assistant\",\"message\":{\"model\":\"claude-test-model\"}}' > \"$CLAUDE_CONFIG_DIR/projects/test/$id.jsonl\"\nprintf '{\"type\":\"result\",\"result\":\"CLAUDE-REPLY\",\"usage\":{\"input_tokens\":100,\"cache_read_input_tokens\":800,\"cache_creation_input_tokens\":100}}'\n").unwrap();
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
        assert_eq!(
            outcome.usage,
            Some(TokenUsage {
                prompt_tokens: 1_000,
                cached_tokens: 800,
            })
        );
    }
}

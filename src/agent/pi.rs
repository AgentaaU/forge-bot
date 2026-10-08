//! Pi coding agent adapter.
//!
//! Pi takes the prompt as a positional argument in `--print` mode. It runs
//! with `--approve` by default (issue #33) so project-local files are trusted
//! without a prompt; `dangerously_skip_permissions = false` opts out. Pi also
//! accepts a session id and creates the session when it does not exist yet, so
//! every comment in a thread keeps the same conversation.

use std::sync::Arc;

use crate::agent::command::{CommandAgent, EffortStyle, SessionStyle};
use crate::agent::session::SessionStore;
use crate::config::{AgentConfig, PromptDelivery};

/// Built-in Pi adapter defaults.
pub fn default_agent() -> CommandAgent {
    CommandAgent::new("pi", "pi")
        .args(["--print", "--mode", "text"])
        .prompt(PromptDelivery::Arg)
}

/// Build a Pi adapter, applying user overrides.
pub fn build(config: &AgentConfig, sessions: Arc<SessionStore>) -> CommandAgent {
    let auto = config.dangerously_skip_permissions.unwrap_or(true);
    let agent = default_agent()
        .apply_config(config)
        .dangerously_skip_permissions(auto)
        .effort_style(EffortStyle::Flag("--thinking"))
        .session(
            SessionStyle {
                create_args: vec!["--session-id".into(), "{session}".into()],
                // `--session-id` creates the session if it is missing, so the same
                // call resumes an existing one.
                resume_args: vec!["--session-id".into(), "{session}".into()],
                resume_at: None,
                reply_from_file: false,
                capture_id: false,
                replace_on_resume: false,
            },
            sessions,
        );
    if auto { agent.arg("--approve") } else { agent }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Arc<SessionStore> {
        Arc::new(SessionStore::default())
    }

    #[test]
    fn approves_by_default() {
        let agent = build(&AgentConfig::default(), store());
        assert!(agent.arguments().iter().any(|arg| arg == "--approve"));
    }

    #[test]
    fn can_opt_out_of_approval() {
        let config = AgentConfig {
            dangerously_skip_permissions: Some(false),
            ..Default::default()
        };
        let agent = build(&config, store());
        assert!(!agent.arguments().iter().any(|arg| arg == "--approve"));
    }
}

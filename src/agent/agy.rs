//! Antigravity CLI adapter.
//!
//! `agy --print <prompt>` runs one headless turn and writes the response to
//! stdout. In headless mode, tools that need approval are denied unless the
//! operator configures permissions or uses `--dangerously-skip-permissions`.
//!
//! Unlike Claude Code, `agy --print` takes the prompt as its value rather than
//! as a positional argument. The prompt is appended after every configured
//! argument, so `--print` must be the last configured argument; a flag placed
//! between `--print` and the prompt is consumed as the prompt.

use std::path::PathBuf;

use crate::agent::command::{CommandAgent, model_arg};
use crate::config::{AgentConfig, PromptDelivery};

/// Built-in Antigravity CLI adapter defaults.
///
/// `--print` is added by [`build`] after any permission flag, because it must
/// immediately precede the prompt that [`CommandAgent`] appends.
pub fn default_agent() -> CommandAgent {
    CommandAgent::new("agy", "agy").prompt(PromptDelivery::Arg)
}

/// Build an Antigravity CLI adapter, applying user overrides.
pub fn build(config: &AgentConfig) -> CommandAgent {
    let auto = config.dangerously_skip_permissions.unwrap_or(true);
    let agent = default_agent()
        .apply_config(config)
        .dangerously_skip_permissions(auto);
    let agent = if auto {
        agent.arg("--dangerously-skip-permissions")
    } else {
        agent
    };
    // `--print` consumes the next argument as its prompt, and the prompt is
    // appended after all configured arguments. Add `--print` last so agy does
    // not mistake `--dangerously-skip-permissions` for the prompt.
    agent.arg("--print")
}

/// AGY's print output omits its default model. Read the same persisted
/// selection that the CLI loads when no model argument was supplied.
pub(crate) fn configured_model(args: &[String], home: Option<&String>) -> Option<String> {
    if let Some(model) = model_arg(args, false) {
        return Some(model);
    }
    let home = home
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))?;
    let path = home.join(".gemini/antigravity-cli/settings.json");
    let data = std::fs::read(path).ok()?;
    let settings: serde_json::Value = serde_json::from_slice(&data).ok()?;
    settings["model"]
        .as_str()
        .filter(|model| !model.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Agent, AgentContext, AgentRequest};

    #[test]
    fn defaults_run_headless_with_permissions() {
        let agent = build(&AgentConfig::default());
        assert_eq!(agent.name(), "agy");
        assert_eq!(agent.program(), "agy");
        // `--print` must stay last: it consumes the next argument (the prompt)
        // as its value, so a flag after it would be taken as the prompt.
        assert_eq!(
            agent.arguments(),
            ["--dangerously-skip-permissions", "--print"]
        );
    }

    #[test]
    fn permissions_can_be_configured_externally() {
        let agent = build(&AgentConfig {
            dangerously_skip_permissions: Some(false),
            ..Default::default()
        });
        assert_eq!(agent.arguments(), ["--print"]);
    }

    #[test]
    fn reads_selected_model_without_changing_agent_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join(".gemini/antigravity-cli/settings.json");
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::fs::write(settings, r#"{"model":"Gemini 3.8 Flash (Medium)"}"#).unwrap();
        let home = dir.path().display().to_string();
        assert_eq!(
            configured_model(&[], Some(&home)).as_deref(),
            Some("Gemini 3.8 Flash (Medium)")
        );
        assert_eq!(
            configured_model(&["--model=gemini-test".into()], Some(&home)).as_deref(),
            Some("gemini-test")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn passes_prompt_as_argument_and_captures_reply() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-agy.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\n[ \"$1\" = --dangerously-skip-permissions ] || exit 1\n[ \"$2\" = --print ] || exit 2\ncase \"$3\" in *PING*) printf 'agy reply' ;; *) exit 3 ;; esac\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let agent = build(&AgentConfig {
            command: Some(script.display().to_string()),
            env: [("HOME".into(), dir.path().display().to_string())].into(),
            ..Default::default()
        });
        let settings = dir.path().join(".gemini/antigravity-cli/settings.json");
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::fs::write(settings, r#"{"model":"agy-test-model"}"#).unwrap();
        let request = AgentRequest {
            location: url::Url::parse("https://forge.example.com/o/r/issues/1").unwrap(),
            message: "PING".into(),
        };
        let outcome = agent.run(&request, &AgentContext::default()).await.unwrap();
        assert!(outcome.success);
        assert_eq!(outcome.summary, "agy reply");
        assert_eq!(outcome.model.as_deref(), Some("agy-test-model"));
    }
}

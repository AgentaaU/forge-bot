//! Kimi CLI adapter.
//!
//! The Kimi CLI is not installed everywhere; this adapter provides sensible
//! defaults that can be fully overridden through configuration.

use std::path::PathBuf;

use crate::agent::command::{CommandAgent, model_arg};
use crate::config::{AgentConfig, PromptDelivery};

/// Built-in Kimi adapter defaults.
pub fn default_agent() -> CommandAgent {
    CommandAgent::new("kimi", "kimi")
        .args(["--print"])
        .prompt(PromptDelivery::Arg)
}

/// Build a Kimi adapter, applying user overrides.
pub fn build(config: &AgentConfig) -> CommandAgent {
    default_agent().apply_config(config)
}

/// Kimi's print output contains messages only. Resolve the model name from
/// the same configuration and arguments the CLI uses for this invocation.
pub(crate) fn configured_model(
    args: &[String],
    home: Option<&String>,
    kimi_home: Option<&String>,
    workspace: &std::path::Path,
) -> Option<String> {
    let explicit = model_arg(args, true);
    let inline = option_value(args, "--config");
    let config_path = option_value(args, "--config-file")
        .map(|path| {
            let path = PathBuf::from(path);
            if path.is_relative() && !workspace.as_os_str().is_empty() {
                workspace.join(path)
            } else {
                path
            }
        })
        .or_else(|| {
            let root = kimi_home.map(PathBuf::from).or_else(|| {
                home.map(PathBuf::from)
                    .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
                    .map(|home| home.join(".kimi"))
            })?;
            Some(root.join("config.toml"))
        });
    let data = inline.or_else(|| config_path.and_then(|path| std::fs::read_to_string(path).ok()));
    let config = data.as_deref().and_then(parse_config);
    let name = explicit.or_else(|| {
        config
            .as_ref()
            .and_then(|value| value["default_model"].as_str().map(str::to_owned))
    })?;
    Some(
        config
            .as_ref()
            .and_then(|value| value["models"][&name]["model"].as_str())
            .filter(|id| !id.is_empty())
            .unwrap_or(&name)
            .to_owned(),
    )
}

fn option_value(args: &[String], option: &str) -> Option<String> {
    args.iter()
        .enumerate()
        .filter_map(|(index, arg)| {
            if arg == option {
                args.get(index + 1).cloned()
            } else {
                arg.strip_prefix(&format!("{option}=")).map(str::to_owned)
            }
        })
        .next_back()
}

fn parse_config(data: &str) -> Option<serde_json::Value> {
    serde_json::from_str(data).ok().or_else(|| {
        toml::from_str::<toml::Value>(data)
            .ok()
            .and_then(|value| serde_json::to_value(value).ok())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Agent, AgentContext, AgentRequest};

    #[test]
    fn reads_effective_model_from_kimi_config_and_override() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(&config,
            "default_model = 'coding'\n[models.coding]\nmodel = 'kimi-default'\n[models.alternate]\nmodel = 'kimi-alt'\n").unwrap();
        let args = vec!["--config-file".into(), "config.toml".into()];
        assert_eq!(
            configured_model(&args, None, None, dir.path()).as_deref(),
            Some("kimi-default")
        );
        let mut overridden = args;
        overridden.extend(["-m".into(), "alternate".into()]);
        assert_eq!(
            configured_model(&overridden, None, None, dir.path()).as_deref(),
            Some("kimi-alt")
        );
    }

    #[test]
    fn reads_inline_json_and_unknown_model_name() {
        let args = vec![
            "--config".into(),
            r#"{"default_model":"first","models":{"first":{"model":"kimi-first"}}}"#.into(),
        ];
        assert_eq!(
            configured_model(&args, None, None, std::path::Path::new("")).as_deref(),
            Some("kimi-first")
        );
        let args = vec!["--model=custom-model".into()];
        assert_eq!(
            configured_model(&args, None, None, std::path::Path::new("")).as_deref(),
            Some("custom-model")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reports_configured_model_with_original_reply() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "default_model = 'coding'\n[models.coding]\nmodel = 'kimi-test-model'\n",
        )
        .unwrap();
        let script = dir.path().join("fake-kimi.sh");
        std::fs::write(&script, "#!/bin/sh\nprintf 'KIMI-REPLY'\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let agent = build(&AgentConfig {
            command: Some(script.display().to_string()),
            env: [("KIMI_CODE_HOME".into(), dir.path().display().to_string())].into(),
            ..Default::default()
        });
        let request = AgentRequest {
            location: "https://forge.example.com/o/r/issues/1".parse().unwrap(),
            message: "go".into(),
        };
        let outcome = agent.run(&request, &AgentContext::default()).await.unwrap();
        assert!(outcome.success);
        assert_eq!(outcome.summary, "KIMI-REPLY");
        assert_eq!(outcome.model.as_deref(), Some("kimi-test-model"));
    }
}

//! Kimi CLI adapter.
//!
//! The Kimi CLI is not installed everywhere; this adapter provides sensible
//! defaults that can be fully overridden through configuration.

use std::path::PathBuf;

use crate::agent::command::{CommandAgent, model_arg};
use crate::agent::kimi_wire::KimiWireAgent;
use crate::agent::{Agent, AgentContext, AgentOutcome, AgentRequest, Result, SteerReceipt, wire};
use crate::config::{AgentConfig, PromptDelivery};

/// Built-in Kimi adapter defaults.
pub fn default_agent() -> CommandAgent {
    CommandAgent::new("kimi", "kimi")
        .args(["--print"])
        .prompt(PromptDelivery::Arg)
}

/// Build a Kimi adapter, applying user overrides.
///
/// The default binary is driven through `kimi --wire`, which supports
/// same-turn steering. A custom `command` or `args` keeps the one-shot
/// `kimi --print` adapter, and a CLI without wire mode falls back to it at run
/// time.
pub fn build(config: &AgentConfig) -> KimiAgent {
    let one_shot = default_agent().apply_config(config);
    let wire =
        (config.command.is_none() && config.args.is_none()).then(|| KimiWireAgent::new(config));
    KimiAgent { one_shot, wire }
}

/// The registered Kimi adapter: wire steering with a one-shot fallback.
pub struct KimiAgent {
    one_shot: CommandAgent,
    wire: Option<KimiWireAgent>,
}

impl KimiAgent {
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
impl Agent for KimiAgent {
    fn name(&self) -> &str {
        "kimi"
    }

    async fn run(&self, request: &AgentRequest, context: &AgentContext) -> Result<AgentOutcome> {
        if let Some(wire_mode) = &self.wire {
            match wire_mode.run(request, context).await {
                Ok(outcome) => return Ok(outcome),
                Err(error) if wire::is_unsupported(&error) => {
                    tracing::info!(
                        %error,
                        "kimi wire mode is unavailable; falling back to kimi --print"
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
        match &self.wire {
            Some(wire_mode) => wire_mode.follow_up(request, context).await,
            None => Ok(None),
        }
    }
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

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_failure_falls_back_to_one_shot() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("one-shot.sh");
        std::fs::write(&script, "#!/bin/sh\nprintf 'KIMI-ONESHOT'\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let one_shot = default_agent().apply_config(&AgentConfig {
            command: Some(script.display().to_string()),
            ..Default::default()
        });
        let wire = KimiWireAgent::new(&AgentConfig {
            command: Some("/nonexistent/kimi".into()),
            ..Default::default()
        });
        let agent = KimiAgent {
            one_shot,
            wire: Some(wire),
        };
        let request = AgentRequest {
            location: "https://forge.example.com/o/r/issues/1".parse().unwrap(),
            message: "go".into(),
        };
        let outcome = agent.run(&request, &AgentContext::default()).await.unwrap();
        assert!(outcome.success, "{outcome:?}");
        assert_eq!(outcome.summary, "KIMI-ONESHOT");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_crash_after_the_prompt_is_not_replayed() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("one-shot-ran");
        let one_shot_script = dir.path().join("one-shot.sh");
        std::fs::write(
            &one_shot_script,
            "#!/bin/sh\ntouch \"$FAKE_ONESHOT_MARKER\"\nprintf 'KIMI-ONESHOT'\n",
        )
        .unwrap();
        std::fs::set_permissions(&one_shot_script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let prompt_log = dir.path().join("prompt.log");
        let wire_script = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/fake_kimi_wire.py"
        );
        let one_shot = default_agent().apply_config(&AgentConfig {
            command: Some(one_shot_script.display().to_string()),
            env: [("FAKE_ONESHOT_MARKER".into(), marker.display().to_string())].into(),
            ..Default::default()
        });
        let wire = KimiWireAgent::new(&AgentConfig {
            command: Some(wire_script.into()),
            env: [
                ("FAKE_KIMI_EXIT_ON_PROMPT".into(), "1".into()),
                (
                    "FAKE_KIMI_PROMPT_LOG".into(),
                    prompt_log.display().to_string(),
                ),
            ]
            .into(),
            ..Default::default()
        });
        let agent = KimiAgent {
            one_shot,
            wire: Some(wire),
        };
        let request = AgentRequest {
            location: "https://forge.example.com/o/r/issues/1".parse().unwrap(),
            message: "go".into(),
        };
        let outcome = agent.run(&request, &AgentContext::default()).await.unwrap();
        assert!(!outcome.success, "{outcome:?}");
        assert!(
            prompt_log.exists(),
            "the prompt side effect should be recorded before the crash"
        );
        assert!(
            !marker.exists(),
            "the one-shot adapter must not run after the prompt was submitted"
        );
    }
}

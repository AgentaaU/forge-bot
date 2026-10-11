//! Read-only model discovery using the agents' own catalogs, under the user's
//! Linux account. No prompt, turn, or model override is sent.

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::Instant;

use super::pi_rpc::{LaunchSettings, PiRpcClient};
use super::wire::WireProcess;
use crate::config::{Config, PiRpcConfig};
use crate::error::{BotError, Result};
use crate::executor::{ExecSpec, Executor};

fn malformed(agent: &str) -> BotError {
    BotError::Agent {
        name: agent.into(),
        reason: "invalid model catalog response".into(),
    }
}

pub(crate) fn pi_models(data: &Value) -> Result<Vec<String>> {
    data["models"]
        .as_array()
        .ok_or_else(|| malformed("pi"))?
        .iter()
        .map(|model| {
            let provider = model["provider"].as_str().filter(|s| !s.is_empty());
            let id = model["id"].as_str().filter(|s| !s.is_empty());
            match (provider, id) {
                (Some(provider), Some(id)) => Ok(format!("{provider}/{id}")),
                _ => Err(malformed("pi")),
            }
        })
        .collect()
}

// Explicitly kill even when a query future is cancelled: WireProcess's reader
// and writer tasks otherwise keep running until the process exits.
struct QueryProcess(std::sync::Arc<WireProcess>);
impl Drop for QueryProcess {
    fn drop(&mut self) {
        self.0.kill();
    }
}

/// Translate only catalog/provider overrides and known execution-only options.
/// Unknown options (including profiles and workspace changes) may affect the
/// catalog, so refuse discovery rather than substitute the account's defaults.
fn codex_discovery_args(args: &[String]) -> Option<Vec<String>> {
    let mut discovery = vec!["app-server".into(), "--listen".into(), "stdio://".into()];
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        let config = if matches!(arg, "-c" | "--config") {
            index += 1;
            Some(args.get(index)?.as_str())
        } else {
            arg.strip_prefix("--config=")
                .or_else(|| arg.strip_prefix("-c"))
                .map(|value| value.strip_prefix('=').unwrap_or(value))
        };
        if let Some(config) = config {
            let (key, _) = config.split_once('=')?;
            match key.trim() {
                "model_provider" | "model_catalog_json" => {
                    discovery.extend(["-c".into(), config.into()]);
                }
                key if key.starts_with("model_providers.") => {
                    discovery.extend(["-c".into(), config.into()]);
                }
                // Discovery must not select a model or effort.
                "model" | "model_reasoning_effort" | "model_reasoning_summary" => {}
                _ => return None,
            }
        } else if arg == "exec" && index == 0 {
            // Replace the execution subcommand with app-server.
        } else if matches!(
            arg,
            "--json"
                | "--skip-git-repo-check"
                | "--full-auto"
                | "--dangerously-bypass-approvals-and-sandbox"
        ) {
            // These control execution, not catalog discovery.
        } else if matches!(
            arg,
            "--model" | "-m" | "--color" | "--output-last-message" | "-o"
        ) {
            index += 1;
            let value = args.get(index)?;
            if value.starts_with('-') {
                return None;
            }
        } else if arg.starts_with("--model=")
            || arg.starts_with("--color=")
            || arg.starts_with("--output-last-message=")
        {
            // Attached execution-only option values.
        } else {
            return None;
        }
        index += 1;
    }
    Some(discovery)
}

/// `None` means the adapter has no model discovery protocol. Errors leave the
/// admin page's saved/configured choices usable and are never cooldown events.
pub async fn available_models(
    config: &Config,
    agent: &str,
    executor: &Executor,
    host_user: &str,
) -> Result<Option<Vec<String>>> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let home = executor
        .account(host_user)
        .map(|account| account.home.clone());
    let query = async {
        match agent {
            "codex" => {
                let cfg = config
                    .agents
                    .overrides
                    .get("codex")
                    .cloned()
                    .unwrap_or_default();
                let Some(args) = codex_discovery_args(cfg.args.as_deref().unwrap_or_default())
                else {
                    return Ok(None);
                };
                let spec = ExecSpec {
                    program: cfg.command.unwrap_or_else(|| "codex".into()),
                    args,
                    env: cfg.env.into_iter().collect(),
                    host_user: Some(host_user.into()),
                    cwd: home.clone(),
                };
                let process = QueryProcess(WireProcess::spawn("codex", false, &spec, executor)?);
                process
                    .0
                    .request(
                        "initialize",
                        json!({"clientInfo": {
                            "name": "forge-bot", "version": env!("CARGO_PKG_VERSION")
                        }}),
                        Some(deadline),
                    )
                    .await?;
                let mut models = Vec::new();
                let mut cursor = None;
                for _ in 0..100 {
                    let page = process
                        .0
                        .request(
                            "model/list",
                            json!({
                                "cursor": cursor, "includeHidden": false
                            }),
                            Some(deadline),
                        )
                        .await?;
                    for model in page["data"].as_array().ok_or_else(|| malformed("codex"))? {
                        models.push(
                            model["model"]
                                .as_str()
                                .filter(|s| !s.is_empty())
                                .ok_or_else(|| malformed("codex"))?
                                .to_owned(),
                        );
                    }
                    match page["nextCursor"].as_str().filter(|s| !s.is_empty()) {
                        Some(next) if cursor.as_deref() != Some(next) => {
                            cursor = Some(next.to_owned())
                        }
                        None => return Ok(Some(models)),
                        _ => return Err(malformed("codex")),
                    }
                }
                Err(malformed("codex"))
            }
            "pi-rpc" | "pi" => {
                let mut cfg = if agent == "pi-rpc" {
                    config.pi_rpc.clone()
                } else {
                    let cfg = config
                        .agents
                        .overrides
                        .get("pi")
                        .cloned()
                        .unwrap_or_default();
                    // Custom one-shot arguments may not support RPC discovery.
                    // Keep them so the query sees the same extensions/providers.
                    PiRpcConfig {
                        command: cfg.command.unwrap_or_else(|| "pi".into()),
                        args: cfg.args.unwrap_or_default(),
                        env: cfg.env,
                        approve: cfg.dangerously_skip_permissions.unwrap_or(true),
                        ..Default::default()
                    }
                };
                cfg.no_session = true;
                let mut client = PiRpcClient::spawn(
                    &cfg,
                    home.as_deref().unwrap_or_else(|| Path::new("")),
                    &[],
                    None,
                    executor,
                    Some(host_user),
                    LaunchSettings::default(),
                )?;
                Ok(Some(client.available_models().await?))
            }
            _ => Ok(None),
        }
    };
    tokio::time::timeout_at(deadline, query)
        .await
        .map_err(|_| BotError::Agent {
            name: agent.into(),
            reason: "model discovery timed out".into(),
        })?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> String {
        format!(
            "{}/tests/fixtures/fake_model_catalog.py",
            env!("CARGO_MANIFEST_DIR")
        )
    }

    #[tokio::test]
    async fn queries_agent_catalogs_without_starting_a_turn_or_selecting_a_model() {
        let mut config = Config::default();
        config.agents.overrides.insert(
            "codex".into(),
            crate::config::AgentConfig {
                command: Some(fixture()),
                ..Default::default()
            },
        );
        config.pi_rpc.command = fixture();
        config.agents.overrides.insert(
            "pi".into(),
            crate::config::AgentConfig {
                command: Some(fixture()),
                ..Default::default()
            },
        );
        let executor = Executor::direct();
        assert_eq!(
            available_models(&config, "codex", &executor, "agent")
                .await
                .unwrap(),
            Some(vec!["first".into(), "second".into()])
        );
        for name in ["pi", "pi-rpc"] {
            assert_eq!(
                available_models(&config, name, &executor, "agent")
                    .await
                    .unwrap(),
                Some(vec!["test/pi-model".into()])
            );
        }
        assert_eq!(
            available_models(&config, "custom", &executor, "agent")
                .await
                .unwrap(),
            None
        );
        config
            .agents
            .overrides
            .get_mut("codex")
            .unwrap()
            .env
            .insert("CATALOG_FAIL".into(), "1".into());
        assert!(
            available_models(&config, "codex", &executor, "agent")
                .await
                .is_err()
        );
        config.pi_rpc.env.insert("CATALOG_FAIL".into(), "1".into());
        assert!(
            available_models(&config, "pi-rpc", &executor, "agent")
                .await
                .is_err()
        );
    }

    #[test]
    fn translates_catalog_config_and_rejects_ambiguous_arguments() {
        for option in [
            vec!["-c", "model_catalog_json=\"/tmp/catalog.json\""],
            vec!["--config", "model_catalog_json=\"/tmp/catalog.json\""],
            vec!["--config=model_catalog_json=\"/tmp/catalog.json\""],
            vec!["-cmodel_catalog_json=\"/tmp/catalog.json\""],
            vec!["-c=model_catalog_json=\"/tmp/catalog.json\""],
        ] {
            let mut args = vec!["exec".to_owned()];
            args.extend(option.into_iter().map(str::to_owned));
            args.extend(
                [
                    "--json",
                    "--model",
                    "ignored",
                    "-c",
                    "model_reasoning_effort=\"high\"",
                ]
                .map(str::to_owned),
            );
            assert_eq!(
                codex_discovery_args(&args).unwrap(),
                [
                    "app-server",
                    "--listen",
                    "stdio://",
                    "-c",
                    "model_catalog_json=\"/tmp/catalog.json\""
                ]
            );
        }
        for args in [
            vec!["exec", "--profile", "custom"],
            vec!["exec", "-c"],
            vec!["exec", "-c", "unknown_setting=true"],
            vec!["exec", "--cd", "/tmp"],
            vec!["exec", "a prompt"],
            vec!["exec", "--model"],
        ] {
            assert!(
                codex_discovery_args(&args.into_iter().map(str::to_owned).collect::<Vec<_>>())
                    .is_none()
            );
        }
    }

    #[tokio::test]
    async fn codex_discovery_uses_overridden_catalog_and_skips_unsupported_launches() {
        let mut config = Config::default();
        let cfg = crate::config::AgentConfig {
            command: Some(fixture()),
            args: Some(
                [
                    "exec",
                    "-c",
                    "model_catalog_json=\"/tmp/catalog.json\"",
                    "--config=model_provider=\"test\"",
                    "-cmodel_providers.test.name=\"Test\"",
                    "--model",
                    "ignored",
                    "--json",
                ]
                .map(str::to_owned)
                .to_vec(),
            ),
            env: [("CATALOG_REQUIRE_CONFIG".into(), "1".into())].into(),
            ..Default::default()
        };
        config.agents.overrides.insert("codex".into(), cfg);
        let executor = Executor::direct();
        assert_eq!(
            available_models(&config, "codex", &executor, "agent")
                .await
                .unwrap(),
            Some(vec!["configured-catalog-model".into(), "second".into()])
        );
        let cfg = config.agents.overrides.get_mut("codex").unwrap();
        cfg.command = Some("must-not-be-launched".into());
        cfg.args = Some(["exec", "--profile", "custom"].map(str::to_owned).to_vec());
        assert_eq!(
            available_models(&config, "codex", &executor, "agent")
                .await
                .unwrap(),
            None
        );
    }

    #[test]
    fn pi_catalog_preserves_provider_identity() {
        assert_eq!(
            pi_models(&json!({"models": [
                {"provider": "one", "id": "shared"},
                {"provider": "two", "id": "shared"}
            ]}))
            .unwrap(),
            ["one/shared", "two/shared"]
        );
        assert!(pi_models(&json!({"models": [{"id": "missing-provider"}]})).is_err());
        assert!(pi_models(&json!({})).is_err());
    }
}

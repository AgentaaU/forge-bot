//! Agent registry.
//!
//! Holds every configured [`Agent`] and resolves the adapter to use for a
//! request. The built-ins (Codex, Antigravity, Pi, Claude Code, Kimi) are always
//! available and can be overridden or extended from configuration.
//!
//! Codex leads the built-in fallback order when no sequence is configured.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::agent::command::CommandAgent;
use crate::agent::pi_rpc::PiPoolAgent;
use crate::agent::{Agent, agy, claude, codex, kimi, pi};
use crate::config::Config;
use crate::error::{BotError, Result};

/// Names of the adapters that are always registered.
pub const BUILTIN_AGENTS: &[&str] = &["codex", "agy", "pi-rpc", "pi", "claude", "kimi"];

/// Why an agent was temporarily marked unavailable.
///
/// Recorded alongside the cooldown so fallback notices can explain *why* an
/// agent was skipped instead of only naming it (issue #94).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnavailableReason {
    /// The agent reported a quota, rate or provider-capacity error.
    CapacityLimit,
    /// The agent process could not be started (missing binary, bad command).
    StartFailed,
    /// Any other failure that put the agent on cooldown.
    Failed,
}

impl UnavailableReason {
    /// Short, user-facing label used in fallback notices.
    pub fn label(&self) -> &'static str {
        match self {
            Self::CapacityLimit => "capacity limit",
            Self::StartFailed => "start failed",
            Self::Failed => "failed",
        }
    }
}

impl std::fmt::Display for UnavailableReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// A cooldown window together with the reason it was recorded.
#[derive(Debug, Clone)]
struct Unavailable {
    /// Instant at which the agent may be tried again.
    until: Instant,
    /// Why the agent is out of rotation.
    reason: UnavailableReason,
}

/// Resolves agent names to adapters.
///
/// The registry also remembers which agents are temporarily unavailable
/// because they reported a quota, rate or capacity error, or because their
/// adapter could not be started. That state is shared by every clone of the
/// [`Arc`] the dispatcher and the HTTP layer hold, so a hit on one job makes
/// later jobs skip the agent too.
pub struct AgentRegistry {
    agents: BTreeMap<String, Arc<dyn Agent>>,
    default: String,
    sequence: Vec<String>,
    /// Agent name -> cooldown window and the reason for it.
    unavailable: Mutex<HashMap<String, Unavailable>>,
}

impl AgentRegistry {
    /// Build the registry from configuration.
    pub fn from_config(config: &Config) -> Self {
        let mut agents: BTreeMap<String, Arc<dyn Agent>> = BTreeMap::new();

        let overrides = &config.agents.overrides;

        // One backend session per `(agent, conversation)` so a later comment
        // in the same thread resumes the same context instead of cold-starting.
        let sessions = Arc::new(crate::agent::session::SessionStore::load(
            &crate::config::expand_tilde(&config.session.dir),
        ));

        // `pi` is disabled by default: `pi-rpc` is the default Pi backend now.
        // The one-shot adapter is kept and can be re-enabled with
        // `[agents.pi] enabled = true`.
        let enabled = |name: &str| -> bool {
            overrides
                .get(name)
                .and_then(|cfg| cfg.enabled)
                .unwrap_or(name != "pi")
        };

        if enabled("codex") {
            let codex_cfg = overrides.get("codex").cloned().unwrap_or_default();
            agents.insert(
                "codex".into(),
                Arc::new(codex::build(&codex_cfg, Arc::clone(&sessions))),
            );
        }

        if enabled("agy") {
            let agy_cfg = overrides.get("agy").cloned().unwrap_or_default();
            agents.insert("agy".into(), Arc::new(agy::build(&agy_cfg)));
        }

        if enabled("pi") {
            let pi_cfg = overrides.get("pi").cloned().unwrap_or_default();
            agents.insert(
                "pi".into(),
                Arc::new(pi::build(&pi_cfg, Arc::clone(&sessions))),
            );
        }

        if enabled("claude") {
            let claude_cfg = overrides.get("claude").cloned().unwrap_or_default();
            agents.insert(
                "claude".into(),
                Arc::new(claude::build(&claude_cfg, Arc::clone(&sessions))),
            );
        }

        if enabled("kimi") {
            let kimi_cfg = overrides.get("kimi").cloned().unwrap_or_default();
            agents.insert("kimi".into(), Arc::new(kimi::build(&kimi_cfg)));
        }

        // Pooled Pi RPC adapter, configured from its own `[pi_rpc]` section.
        // Its live-process count is the single global `[session] workers` cap,
        // so the pool has no separate `max_agents` limit.
        agents.insert(
            "pi-rpc".into(),
            Arc::new(PiPoolAgent::new(
                &config.pi_rpc,
                Arc::clone(&sessions),
                config.session.workers.max(1),
            )),
        );

        // Custom adapters: any override that is not a built-in must provide a
        // command to run.
        for (name, cfg) in overrides {
            if BUILTIN_AGENTS.contains(&name.as_str()) || agents.contains_key(name) {
                continue;
            }
            if cfg.enabled == Some(false) {
                continue;
            }
            match &cfg.command {
                Some(command) if !command.is_empty() => {
                    let agent = CommandAgent::new(name.clone(), command).apply_config(cfg);
                    agents.insert(name.clone(), Arc::new(agent));
                }
                _ => {
                    tracing::warn!(
                        agent = %name,
                        "custom agent has no `command` configured; skipping"
                    );
                }
            }
        }

        let builtin_order = || {
            let mut names: Vec<String> = BUILTIN_AGENTS
                .iter()
                .filter(|name| agents.contains_key(**name))
                .map(|name| (*name).to_owned())
                .collect();
            names.extend(
                agents
                    .keys()
                    .filter(|name| !BUILTIN_AGENTS.contains(&name.as_str()))
                    .cloned(),
            );
            names
        };
        let sequence = if config.agent_sequence.is_empty() {
            builtin_order()
        } else {
            let mut sequence = Vec::new();
            for name in &config.agent_sequence {
                if !agents.contains_key(name) {
                    tracing::warn!(agent = %name, "agent in sequence is not registered; skipping");
                } else if !sequence.contains(name) {
                    sequence.push(name.clone());
                }
            }
            sequence
        };

        let default = sequence
            .first()
            .cloned()
            .unwrap_or_else(|| "codex".to_owned());

        Self {
            agents,
            default,
            sequence,
            unavailable: Mutex::new(HashMap::new()),
        }
    }

    /// The first registered agent in the configured or built-in sequence.
    pub fn default_name(&self) -> &str {
        &self.default
    }

    /// Look up an agent by name.
    pub fn get(&self, name: &str) -> Result<Arc<dyn Agent>> {
        self.agents
            .get(name)
            .cloned()
            .ok_or_else(|| BotError::UnknownAgent(name.to_owned()))
    }

    /// Register a test agent, keeping the preference order usable.
    #[cfg(test)]
    pub(crate) fn insert_for_test(&mut self, name: &str, agent: Arc<dyn Agent>) {
        self.agents.insert(name.to_owned(), agent);
        if !self.sequence.iter().any(|existing| existing == name) {
            self.sequence.push(name.to_owned());
        }
        if !self.agents.contains_key(&self.default) {
            self.default = name.to_owned();
        }
    }

    /// Resolve an explicit name or fall back to the default.
    pub fn resolve(&self, name: Option<&str>) -> Result<Arc<dyn Agent>> {
        match name {
            Some(name) if !name.is_empty() => self.get(name),
            _ => self.get(&self.default),
        }
    }

    /// Model an agent is currently launched with, when known. Powers the
    /// status page's Model column; it only reads the adapter.
    pub fn model(&self, name: &str) -> Option<String> {
        self.agents
            .get(name)
            .and_then(|agent| agent.model())
            .map(str::to_owned)
    }

    /// All registered agent names in preference order: built-ins in
    /// [`BUILTIN_AGENTS`] order (so `codex` leads), then custom adapters
    /// alphabetically.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = BUILTIN_AGENTS
            .iter()
            .filter(|name| self.agents.contains_key(**name))
            .map(|name| (*name).to_owned())
            .collect();
        names.extend(
            self.agents
                .keys()
                .filter(|name| !BUILTIN_AGENTS.contains(&name.as_str()))
                .cloned(),
        );
        names
    }

    /// All registered agent names in the actual preference order the fallback
    /// uses: the configured `agent_sequence` when set, otherwise the built-in
    /// order. Unlike [`Self::names`], this reflects `agent_sequence`, so callers
    /// can tell which agents were passed over between two candidates.
    pub fn ordered_names(&self) -> Vec<String> {
        self.sequence.clone()
    }

    /// Mark an agent unavailable until `cooldown` has elapsed.
    ///
    /// Kept for callers that only know an agent is out; it records
    /// [`UnavailableReason::CapacityLimit`], matching the registry's original
    /// capacity-only behaviour. Use [`Self::mark_unavailable_with_reason`] to
    /// record a more specific cause.
    pub fn mark_unavailable(&self, name: &str, cooldown: Duration) {
        self.mark_unavailable_with_reason(name, cooldown, UnavailableReason::CapacityLimit);
    }

    /// Mark an agent unavailable for `cooldown`, recording why so fallback
    /// notices can explain the skip.
    pub fn mark_unavailable_with_reason(
        &self,
        name: &str,
        cooldown: Duration,
        reason: UnavailableReason,
    ) {
        let until = Instant::now() + cooldown;
        self.unavailable
            .lock()
            .expect("agent availability mutex poisoned")
            .insert(
                name.to_owned(),
                Unavailable {
                    until,
                    reason: reason.clone(),
                },
            );
        tracing::warn!(
            agent = %name,
            reason = %reason,
            retry_after_secs = cooldown.as_secs(),
            "agent marked unavailable"
        );
    }

    /// Clear an agent's cooldown, making it eligible again immediately.
    pub fn mark_available(&self, name: &str) {
        self.unavailable
            .lock()
            .expect("agent availability mutex poisoned")
            .remove(name);
    }

    /// Whether an agent may currently be used. Expired entries are forgotten.
    pub fn is_available(&self, name: &str) -> bool {
        let now = Instant::now();
        let mut unavailable = self
            .unavailable
            .lock()
            .expect("agent availability mutex poisoned");
        match unavailable.get(name) {
            Some(entry) if entry.until > now => false,
            Some(_) => {
                unavailable.remove(name);
                true
            }
            None => true,
        }
    }

    /// The reason `name` is currently unavailable, if it is. Expired entries
    /// are forgotten, matching [`Self::is_available`].
    pub fn unavailable_reason(&self, name: &str) -> Option<UnavailableReason> {
        let now = Instant::now();
        let mut unavailable = self
            .unavailable
            .lock()
            .expect("agent availability mutex poisoned");
        match unavailable.get(name) {
            Some(entry) if entry.until > now => Some(entry.reason.clone()),
            Some(_) => {
                unavailable.remove(name);
                None
            }
            None => None,
        }
    }

    /// Every currently unavailable agent in preference order, with its reason.
    ///
    /// Used by the terminal "no available agent" reply to explain why nothing
    /// could run.
    pub fn unavailable_agents(&self) -> Vec<(String, UnavailableReason)> {
        self.sequence
            .iter()
            .filter_map(|name| {
                self.unavailable_reason(name)
                    .map(|reason| (name.clone(), reason))
            })
            .collect()
    }

    /// Names of the agents that are registered and not capacity-limited, in
    /// configured sequence order, or built-in order when no sequence is set.
    pub fn available_names(&self) -> Vec<String> {
        self.sequence
            .iter()
            .filter(|name| self.is_available(name))
            .cloned()
            .collect()
    }
}

impl std::fmt::Debug for AgentRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentRegistry")
            .field("agents", &self.agents.keys().collect::<Vec<_>>())
            .field("default", &self.default)
            .field("available", &self.available_names())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AgentConfig;

    #[test]
    fn registers_builtins_except_the_opt_in_pi() {
        let registry = AgentRegistry::from_config(&Config::default());
        for name in BUILTIN_AGENTS {
            if *name == "pi" {
                // The one-shot adapter is kept but off by default.
                assert!(registry.get(name).is_err());
                assert!(!registry.names().contains(&name.to_string()));
                continue;
            }
            assert!(registry.names().contains(&name.to_string()));
            assert_eq!(registry.get(name).unwrap().name(), *name);
        }
        // `pi-rpc` is the default Pi backend now.
        assert!(registry.names().contains(&"pi-rpc".to_owned()));

        // The one-shot adapter can be enabled explicitly.
        let mut config = Config::default();
        config.agents.overrides.insert(
            "pi".into(),
            AgentConfig {
                enabled: Some(true),
                ..Default::default()
            },
        );
        let registry = AgentRegistry::from_config(&config);
        assert_eq!(registry.get("pi").unwrap().name(), "pi");
        assert!(registry.names().contains(&"pi".to_owned()));
    }

    #[test]
    fn reports_configured_models() {
        let mut config = Config::default();
        config.pi_rpc.model = Some("deepseek-flash".into());
        config.agents.overrides.insert(
            "codex".into(),
            AgentConfig {
                args: Some(vec!["--model".into(), "gpt-5-codex".into()]),
                ..Default::default()
            },
        );
        let registry = AgentRegistry::from_config(&config);
        assert_eq!(registry.model("pi-rpc"), Some("deepseek-flash".to_owned()));
        assert_eq!(registry.model("codex"), Some("gpt-5-codex".to_owned()));
        assert_eq!(registry.model("agy"), None);
        assert_eq!(registry.model("missing"), None);
    }

    #[test]
    fn custom_agent_requires_command() {
        let mut config = Config::default();
        config
            .agents
            .overrides
            .insert("custom".into(), AgentConfig::default());
        let registry = AgentRegistry::from_config(&config);
        assert!(registry.get("custom").is_err());

        config.agents.overrides.insert(
            "custom".into(),
            AgentConfig {
                command: Some("my-agent".into()),
                ..Default::default()
            },
        );
        let registry = AgentRegistry::from_config(&config);
        assert_eq!(registry.get("custom").unwrap().name(), "custom");
    }

    #[test]
    fn unknown_agent_is_reported() {
        let registry = AgentRegistry::from_config(&Config::default());
        assert!(matches!(
            registry.get("does-not-exist"),
            Err(BotError::UnknownAgent(_))
        ));
    }

    #[test]
    fn codex_is_the_default_and_first_listed_choice() {
        let registry = AgentRegistry::from_config(&Config::default());
        assert_eq!(registry.default_name(), "codex");
        assert_eq!(registry.names().first().map(String::as_str), Some("codex"));
        assert_eq!(
            registry.available_names().first().map(String::as_str),
            Some("codex")
        );
        assert_eq!(registry.available_names()[..3], ["codex", "agy", "pi-rpc"]);
    }

    #[test]
    fn configured_sequence_controls_default_and_fallback_order() {
        let config = Config {
            agent_sequence: vec![
                "pi-rpc".into(),
                "claude".into(),
                "pi-rpc".into(),
                "unknown".into(),
                "pi".into(),
            ],
            ..Default::default()
        };
        let registry = AgentRegistry::from_config(&config);
        assert_eq!(registry.default_name(), "pi-rpc");
        assert_eq!(registry.available_names(), ["pi-rpc", "claude"]);
        // Other registered agents remain selectable explicitly.
        assert!(registry.get("codex").is_ok());

        registry.mark_unavailable("pi-rpc", Duration::from_secs(60));
        assert_eq!(registry.available_names(), ["claude"]);
    }

    #[test]
    fn ordered_names_reflects_the_configured_sequence() {
        // Built-in order when no sequence is configured.
        let registry = AgentRegistry::from_config(&Config::default());
        assert_eq!(&registry.ordered_names()[..3], ["codex", "agy", "pi-rpc"]);

        // The configured sequence, including agents that are unavailable.
        let config = Config {
            agent_sequence: vec!["pi-rpc".into(), "claude".into()],
            ..Default::default()
        };
        let registry = AgentRegistry::from_config(&config);
        registry.mark_unavailable("claude", Duration::from_secs(60));
        assert_eq!(registry.ordered_names(), ["pi-rpc", "claude"]);
    }

    #[test]
    fn capacity_limited_agents_are_skipped_until_the_cooldown_expires() {
        let registry = AgentRegistry::from_config(&Config::default());
        assert!(registry.is_available("codex"));
        assert!(registry.available_names().contains(&"codex".to_owned()));

        registry.mark_unavailable("codex", Duration::from_secs(60));
        assert!(!registry.is_available("codex"));
        assert!(!registry.available_names().contains(&"codex".to_owned()));
        // Other agents are unaffected.
        assert!(registry.available_names().contains(&"pi-rpc".to_owned()));

        // An already-expired entry is treated as available again.
        registry.mark_unavailable("codex", Duration::ZERO);
        assert!(registry.is_available("codex"));

        registry.mark_unavailable("codex", Duration::from_secs(60));
        registry.mark_available("codex");
        assert!(registry.is_available("codex"));
    }

    #[test]
    fn records_and_expires_the_unavailable_reason() {
        let registry = AgentRegistry::from_config(&Config::default());
        registry.mark_unavailable_with_reason(
            "codex",
            Duration::from_secs(60),
            UnavailableReason::StartFailed,
        );
        assert_eq!(
            registry.unavailable_reason("codex"),
            Some(UnavailableReason::StartFailed)
        );
        assert_eq!(
            registry.unavailable_agents(),
            vec![("codex".to_owned(), UnavailableReason::StartFailed)]
        );

        // The plain helper still records the original capacity reason.
        registry.mark_unavailable("codex", Duration::from_secs(60));
        assert_eq!(
            registry.unavailable_reason("codex"),
            Some(UnavailableReason::CapacityLimit)
        );

        // An expired entry is forgotten, reason included.
        registry.mark_unavailable_with_reason("codex", Duration::ZERO, UnavailableReason::Failed);
        assert_eq!(registry.unavailable_reason("codex"), None);
        assert!(registry.unavailable_agents().is_empty());
    }

    #[test]
    fn unavailable_reason_labels_are_user_facing() {
        assert_eq!(UnavailableReason::CapacityLimit.label(), "capacity limit");
        assert_eq!(UnavailableReason::StartFailed.label(), "start failed");
        assert_eq!(UnavailableReason::Failed.label(), "failed");
        assert_eq!(
            UnavailableReason::CapacityLimit.to_string(),
            "capacity limit"
        );
    }
}

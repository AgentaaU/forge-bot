//! Configuration loading.
//!
//! Configuration is read from a TOML file (path given by `FORGE_BOT_CONFIG`,
//! defaulting to `forge-bot.toml`) and then selectively overridden by
//! environment variables so that secrets never have to live in the file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{BotError, Result};

/// Fully resolved bot configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Address the webhook server binds to.
    pub bind: String,
    /// Trigger string users mention, e.g. `@agent`.
    pub mention: String,
    /// Agents to try in order. The first registered agent handles mentions
    /// without an explicit selector; an empty list uses the built-in order.
    pub agent_sequence: Vec<String>,
    /// Per-forge adapter configuration. Flattened so `[forgejo]`,
    /// `[github]` and `[gitlab]` are top-level tables.
    #[serde(flatten)]
    pub forges: Forges,
    pub policy: PolicyConfig,
    pub workspace: WorkspaceConfig,
    pub reply: ReplyConfig,
    pub session: SessionConfig,
    pub agents: AgentConfigs,
    /// Behaviour when an agent hits a quota, rate or capacity limit.
    /// `[quota]` is accepted as a backwards-compatible alias.
    #[serde(alias = "quota")]
    pub capacity: CapacityConfig,
    /// Long-lived Pi RPC agent pool (`pi-rpc` adapter).
    pub pi_rpc: PiRpcConfig,
    /// Forge polling ingester, used when webhooks cannot be configured.
    pub poller: PollerConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:8080".to_owned(),
            mention: "@agent".to_owned(),
            agent_sequence: Vec::new(),
            forges: Forges::default(),
            policy: PolicyConfig::default(),
            workspace: WorkspaceConfig::default(),
            reply: ReplyConfig::default(),
            session: SessionConfig::default(),
            agents: AgentConfigs::default(),
            capacity: CapacityConfig::default(),
            pi_rpc: PiRpcConfig::default(),
            poller: PollerConfig::default(),
        }
    }
}

impl Config {
    /// Load configuration from the given path (or the `FORGE_BOT_CONFIG`
    /// environment variable) and apply environment overrides.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let env_path = std::env::var("FORGE_BOT_CONFIG").ok().map(PathBuf::from);
        let path = path.map(Path::to_path_buf).or(env_path);

        let mut config = match path {
            Some(path) => {
                if path.exists() {
                    let raw = std::fs::read_to_string(&path)?;
                    toml::from_str(&raw).map_err(|e| {
                        BotError::Config(format!("failed to parse {}: {e}", path.display()))
                    })?
                } else {
                    Config::default()
                }
            }
            None => Config::default(),
        };

        config.apply_defaults();
        config.apply_env();
        Ok(config)
    }

    fn apply_defaults(&mut self) {
        if self.bind.is_empty() {
            self.bind = "0.0.0.0:8080".to_owned();
        }
        if self.mention.is_empty() {
            self.mention = "@agent".to_owned();
        }
        self.policy.ensure_defaults();
        self.workspace.ensure_defaults();
        self.session.ensure_defaults();
        self.capacity.ensure_defaults();
        if self.pi_rpc.command.is_empty() {
            self.pi_rpc.command = "pi".to_owned();
        }
        if self.poller.interval_secs == 0 {
            self.poller.interval_secs = 15;
        }
        if self.poller.discover_interval_secs == 0 {
            self.poller.discover_interval_secs = 300;
        }
        if self.poller.page_limit == 0 {
            self.poller.page_limit = 50;
        }
    }

    fn apply_env(&mut self) {
        if let Ok(v) = std::env::var("FORGE_BOT_BIND") {
            self.bind = v;
        }
        if let Ok(v) = std::env::var("FORGE_BOT_MENTION") {
            self.mention = v;
        }
        if let Ok(v) = std::env::var("FORGE_BOT_MAX_CONCURRENCY")
            && let Ok(n) = v.parse()
        {
            self.session.workers = n;
        }
        if let Ok(v) = std::env::var("FORGE_BOT_WORKSPACE_ROOT") {
            self.workspace.root = PathBuf::from(v);
        }
        if let Ok(v) = std::env::var("FORGE_BOT_SESSION_DIR") {
            self.session.dir = PathBuf::from(v);
        }

        if let Some(forgejo) = self.forges.forgejo.as_mut() {
            if let Ok(v) = std::env::var("FORGEJO_WEBHOOK_SECRET") {
                forgejo.webhook_secret = Some(v);
            }
            if let Ok(v) = std::env::var("FORGEJO_TOKEN") {
                forgejo.token = Some(v);
            }
            if let Ok(v) = std::env::var("FORGEJO_BASE_URL") {
                forgejo.base_url = v;
            }
            if let Ok(v) = std::env::var("FORGEJO_BOT_USERNAME") {
                forgejo.bot_username = Some(v);
            }
        }
    }

    /// Resolve the trigger string to its canonical form.
    pub fn trigger(&self) -> &str {
        self.mention.trim()
    }

    /// Credentials exposed to agents for the given forge. A generic
    /// `FORGE_URL` / `FORGE_TOKEN` pair is always included so agents can be
    /// forge agnostic.
    pub fn credentials_for(&self, forge: crate::location::ForgeKind) -> Vec<(String, String)> {
        use crate::location::ForgeKind;

        let mut creds: Vec<(String, String)> = Vec::new();
        match forge {
            ForgeKind::Forgejo | ForgeKind::Gitea => {
                if let Some(cfg) = &self.forges.forgejo {
                    creds.push(("FORGEJO_URL".into(), cfg.base_url.clone()));
                    if let Some(token) = &cfg.token {
                        creds.push(("FORGEJO_TOKEN".into(), token.clone()));
                        creds.push(("GITEA_TOKEN".into(), token.clone()));
                    }
                }
            }
            ForgeKind::GitHub => {
                if let Some(cfg) = &self.forges.github {
                    creds.push(("GITHUB_URL".into(), cfg.base_url.clone()));
                    if let Some(token) = &cfg.token {
                        creds.push(("GITHUB_TOKEN".into(), token.clone()));
                        creds.push(("GH_TOKEN".into(), token.clone()));
                    }
                }
            }
            ForgeKind::GitLab => {
                if let Some(cfg) = &self.forges.gitlab {
                    creds.push(("GITLAB_URL".into(), cfg.base_url.clone()));
                    if let Some(token) = &cfg.token {
                        creds.push(("GITLAB_TOKEN".into(), token.clone()));
                    }
                }
            }
            ForgeKind::Unknown => {}
        }

        if let Some((url, token)) = self.generic_credentials(forge) {
            creds.push(("FORGE_URL".into(), url));
            creds.push(("FORGE_TOKEN".into(), token));
        }

        creds
    }

    fn generic_credentials(&self, forge: crate::location::ForgeKind) -> Option<(String, String)> {
        use crate::location::ForgeKind;
        match forge {
            ForgeKind::Forgejo | ForgeKind::Gitea => self
                .forges
                .forgejo
                .as_ref()
                .and_then(|c| c.token.clone().map(|t| (c.base_url.clone(), t))),
            ForgeKind::GitHub => self
                .forges
                .github
                .as_ref()
                .and_then(|c| c.token.clone().map(|t| (c.base_url.clone(), t))),
            ForgeKind::GitLab => self
                .forges
                .gitlab
                .as_ref()
                .and_then(|c| c.token.clone().map(|t| (c.base_url.clone(), t))),
            ForgeKind::Unknown => None,
        }
    }
}

/// Per-forge adapter configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Forges {
    pub forgejo: Option<ForgejoConfig>,
    pub github: Option<GithubConfig>,
    pub gitlab: Option<GitlabConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ForgejoConfig {
    /// Base URL of the Forgejo instance, e.g. `https://forge.example.com`.
    pub base_url: String,
    /// Shared secret configured on the webhook.
    pub webhook_secret: Option<String>,
    /// Token used by the bot to post comments / clone repositories.
    pub token: Option<String>,
    /// Username of the bot, used to ignore its own comments.
    pub bot_username: Option<String>,
}

impl Default for ForgejoConfig {
    fn default() -> Self {
        Self {
            base_url: "http://localhost:3000".to_owned(),
            webhook_secret: None,
            token: None,
            bot_username: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GithubConfig {
    pub base_url: String,
    pub webhook_secret: Option<String>,
    pub token: Option<String>,
    pub bot_username: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GitlabConfig {
    pub base_url: String,
    pub webhook_secret: Option<String>,
    pub token: Option<String>,
    pub bot_username: Option<String>,
}

/// Who is allowed to trigger the bot.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct PolicyConfig {
    /// When true, every user and repository is allowed. Intended for local
    /// debugging only.
    pub allow_all: bool,
    pub allowed_users: Vec<String>,
    pub allowed_repos: Vec<String>,
    /// Users whose comments are always ignored (normally the bot itself).
    pub ignored_users: Vec<String>,
}

impl PolicyConfig {
    fn ensure_defaults(&mut self) {
        if self.allowed_users.is_empty() && self.allowed_repos.is_empty() {
            // An empty policy would silently deny everything. Be explicit about
            // the safe default while still allowing operators to opt into
            // `allow_all`.
            if !self.allow_all {
                tracing::warn!(
                    "policy: no allowed_users/allowed_repos configured; the bot will reject \
                     every trigger until the policy is filled in or `allow_all = true` is set"
                );
            }
        }
    }
}

/// Where agents get a checkout of the repository.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceConfig {
    /// Clone the repository before invoking an agent.
    pub enabled: bool,
    pub root: PathBuf,
    /// Reuse a workspace for the same repository number across turns.
    pub reuse: bool,
    /// Git author used for agent commits.
    pub git_author_name: String,
    pub git_author_email: String,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            root: PathBuf::from("/tmp/forge-bot-workspaces"),
            reuse: true,
            git_author_name: "forge-bot".to_owned(),
            git_author_email: "forge-bot@localhost".to_owned(),
        }
    }
}

impl WorkspaceConfig {
    fn ensure_defaults(&mut self) {
        if self.root.as_os_str().is_empty() {
            self.root = PathBuf::from("/tmp/forge-bot-workspaces");
        }
        if self.git_author_name.is_empty() {
            self.git_author_name = "forge-bot".to_owned();
        }
        if self.git_author_email.is_empty() {
            self.git_author_email = "forge-bot@localhost".to_owned();
        }
    }
}

/// Whether the gateway should post a comment back to the forge.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ReplyConfig {
    /// Post a short acknowledgement when a job starts, edited in place as
    /// fallbacks occur so a job keeps one status comment.
    pub ack: bool,
    /// Post a result comment when a job finishes. Disabled by default because
    /// agents normally reply themselves; enable it when they do not.
    pub result: bool,
}

impl Default for ReplyConfig {
    fn default() -> Self {
        Self {
            ack: true,
            result: false,
        }
    }
}

/// Job queue and session persistence.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionConfig {
    /// Directory used to persist jobs and sessions.
    pub dir: PathBuf,
    /// Maximum number of agent runs that may be in flight at once. A worker
    /// slot is held for the whole job (workspace preparation, agent run and
    /// reply), so this also bounds concurrent workspace preparation. Because a
    /// conversation always runs at most one agent at a time, it is likewise
    /// the number of different conversations that can run in parallel, and the
    /// single global cap shared by every adapter, pooled (`pi-rpc`) and
    /// one-shot (`codex`, `pi`, ...) alike.
    pub workers: usize,
    /// Maximum size of the in-memory job queue.
    pub queue_capacity: usize,
    /// Requeue persisted (unfinished) jobs on startup.
    pub recover: bool,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            dir: PathBuf::from("/tmp/forge-bot-state"),
            workers: 16,
            queue_capacity: 256,
            recover: true,
        }
    }
}

impl SessionConfig {
    fn ensure_defaults(&mut self) {
        if self.dir.as_os_str().is_empty() {
            self.dir = PathBuf::from("/tmp/forge-bot-state");
        }
        if self.workers == 0 {
            self.workers = 1;
        }
        if self.queue_capacity == 0 {
            self.queue_capacity = 256;
        }
    }
}

/// Configuration for each agent adapter, keyed by adapter name.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentConfigs {
    /// Per-agent overrides. Flattened so `[agents.codex]` works in TOML.
    #[serde(flatten)]
    pub overrides: BTreeMap<String, AgentConfig>,
}

impl AgentConfigs {
    pub fn get(&self, name: &str) -> Option<&AgentConfig> {
        self.overrides.get(name)
    }
}

/// How to launch one agent adapter.
///
/// All fields are optional so that a partial override (for example only the
/// command) keeps the adapter's sensible defaults for the rest.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    /// Whether the adapter is registered at all. Defaults to `true` for every
    /// adapter except the one-shot `pi`, which is off by default because
    /// `pi-rpc` is the default Pi backend. Set `[agents.pi] enabled = true` to
    /// bring the one-shot adapter back.
    pub enabled: Option<bool>,
    pub command: Option<String>,
    /// Extra arguments passed before the prompt.
    pub args: Option<Vec<String>>,
    /// How the prompt is delivered: `stdin` (default) or `arg`.
    pub prompt: Option<PromptDelivery>,
    /// Wall-clock limit in seconds; `0` disables it so the agent runs to
    /// completion instead of being killed when a timer expires.
    pub timeout_secs: Option<u64>,
    /// Extra environment variables for the process.
    pub env: BTreeMap<String, String>,
    /// Whether the agent runs with an unrestricted sandbox. Recorded for
    /// operators and forwarded to adapters that understand it.
    pub dangerously_skip_permissions: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum PromptDelivery {
    #[default]
    Stdin,
    Arg,
}

/// Default cooldown before a capacity-limited agent is retried: five hours.
///
/// Agent quota windows (Codex weekly limits, Anthropic 5-hour sessions, ...)
/// typically reset on a multi-hour cadence, so retrying sooner mostly just
/// burns another failed run.
pub const DEFAULT_CAPACITY_COOLDOWN_SECS: u64 = 5 * 60 * 60;

/// How the dispatcher reacts when an agent fails.
///
/// When `fallback` is enabled, any failed run is handed to the next available
/// agent instead of leaving the thread unanswered; a failure whose output
/// looks like a quota, rate-limit or capacity/overload message (see
/// [`crate::agent::capacity`]) additionally marks that agent as unavailable for
/// the reported retry window or the configured fallback. Ordinary failures do
/// not disable the agent. If no agent is available the bot replies
/// `No available agent`, and a run that failed for
/// any other reason is reported with its error.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CapacityConfig {
    /// Retry a job on another available agent when the chosen one is at
    /// capacity. When false the job simply fails and the agent is still
    /// skipped for `cooldown_secs`.
    pub fallback: bool,
    /// Fallback when a capacity response has no usable retry interval, and
    /// the window used when an agent cannot be started. Defaults to five hours.
    pub cooldown_secs: u64,
    /// Extra, case-insensitive substrings that count as a capacity message, in
    /// addition to the built-in markers.
    pub markers: Vec<String>,
}

impl Default for CapacityConfig {
    fn default() -> Self {
        Self {
            fallback: true,
            cooldown_secs: DEFAULT_CAPACITY_COOLDOWN_SECS,
            markers: Vec::new(),
        }
    }
}

impl CapacityConfig {
    fn ensure_defaults(&mut self) {
        if self.cooldown_secs == 0 {
            self.cooldown_secs = DEFAULT_CAPACITY_COOLDOWN_SECS;
        }
    }
}

/// Configuration for the long-lived Pi RPC agent pool.
///
/// Unlike the one-shot `pi` adapter, `pi-rpc` keeps `pi --mode rpc`
/// subprocesses alive and reuses them for subsequent requests. When every
/// agent in the pool is busy (or the pool is empty) a new one is spawned.
///
/// The pool is not configured separately: `[session] workers` is the single
/// total count of agents and bounds the pool too. No other count limit is
/// applied here.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PiRpcConfig {
    /// The `pi` executable to run.
    pub command: String,
    /// Extra arguments appended after `--mode rpc` (and the flags below).
    pub args: Vec<String>,
    /// Kill an unused agent after this many seconds.
    pub idle_ttl_secs: u64,
    /// Wall-clock limit for a single request, in seconds. `0` disables the
    /// limit entirely, so an agent runs until it settles or its process exits;
    /// the gateway never kills a working agent just because a timer expired.
    pub timeout_secs: u64,
    /// Pass `--approve` so project-local files are trusted.
    pub approve: bool,
    /// Disable pi's session persistence (`--no-session`).
    pub no_session: bool,
    /// Keep one backend session per conversation.
    ///
    /// When `true` (the default), a pooled process only serves the
    /// conversation it is currently bound to: reusing it for a different
    /// conversation in the same workspace would make that conversation inherit
    /// the earlier session, so the pool starts a new process instead (which
    /// resumes the new conversation's own deterministic session). Set to
    /// `false` to let an idle process be reused for any conversation in the
    /// workspace, carrying its earlier session.
    pub session_per_conversation: bool,
    /// Extra environment variables for the spawned agents.
    pub env: BTreeMap<String, String>,
}

impl Default for PiRpcConfig {
    fn default() -> Self {
        Self {
            command: "pi".to_owned(),
            args: Vec::new(),
            idle_ttl_secs: 900,
            timeout_secs: 0,
            approve: true,
            // Persist the backend session so an evicted/restarted process can
            // resume the same conversation instead of cold-starting.
            no_session: false,
            // Start a new session for a new conversation by default instead of
            // handing it a process that already carries another conversation.
            session_per_conversation: true,
            env: BTreeMap::new(),
        }
    }
}

/// Configuration for the polling ingester.
///
/// Forge webhooks are the preferred trigger, but a bot account that is only a
/// repository collaborator cannot create them. Polling reuses the same
/// pipeline with nothing more than a read token.
///
/// When `repositories` is empty the poller discovers every repository visible
/// to the token (via `/repos/search`) and refreshes that list every
/// `discover_interval_secs`, so newly created repositories are picked up
/// automatically. Set `repositories` to an explicit `owner/repo` list to
/// restrict polling instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PollerConfig {
    pub enabled: bool,
    /// Seconds between polls.
    pub interval_secs: u64,
    /// Explicit repositories to watch; empty means "all visible repositories".
    pub repositories: Vec<String>,
    /// How often to refresh the discovered repository list.
    pub discover_interval_secs: u64,
    /// How far back to look for a repository the first time it is seen.
    pub lookback_secs: u64,
    /// Page size for the comments and repository search endpoints.
    pub page_limit: usize,
}

impl Default for PollerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_secs: 15,
            repositories: Vec::new(),
            discover_interval_secs: 300,
            lookback_secs: 3600,
            page_limit: 50,
        }
    }
}

/// Expand a leading `~` in a path into `$HOME`.
pub fn expand_tilde(path: &Path) -> PathBuf {
    let Some(s) = path.to_str() else {
        return path.to_path_buf();
    };
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs_home() {
            return home.join(rest);
        }
    } else if s == "~"
        && let Some(home) = dirs_home()
    {
        return home;
    }
    path.to_path_buf()
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let config = Config::default();
        let mut config = config;
        config.apply_defaults();
        assert_eq!(config.bind, "0.0.0.0:8080");
        assert_eq!(config.mention, "@agent");
        assert!(config.agent_sequence.is_empty());
        assert_eq!(config.session.workers, 16);
        assert!(config.workspace.enabled);
        assert!(config.reply.ack);
        assert!(!config.reply.result);
    }

    #[test]
    fn result_reply_can_be_enabled_explicitly() {
        let config: Config = toml::from_str("[reply]\nresult = true\n").unwrap();
        assert!(config.reply.result);
    }

    #[test]
    fn parses_agent_sequence() {
        let config: Config =
            toml::from_str("agent_sequence = [\"pi-rpc\", \"codex\", \"claude\"]\n").unwrap();
        assert_eq!(config.agent_sequence, ["pi-rpc", "codex", "claude"]);
    }

    #[test]
    fn parses_toml() {
        let raw = r#"
bind = "127.0.0.1:9000"
mention = "@bot"
agent_sequence = ["pi", "codex"]

[forgejo]
base_url = "https://forge.example.com"
webhook_secret = "s3cret"
token = "tok"
bot_username = "botty"

[policy]
allowed_users = ["alice"]

[capacity]
fallback = false
cooldown_secs = 120
markers = ["no tokens left"]

[agents.codex]
command = "codex"
args = ["exec", "-"]
timeout_secs = 60
"#;
        let config: Config = toml::from_str(raw).unwrap();
        assert_eq!(config.bind, "127.0.0.1:9000");
        assert_eq!(config.mention, "@bot");
        assert_eq!(config.agent_sequence, ["pi", "codex"]);
        let forgejo = config.forges.forgejo.unwrap();
        assert_eq!(forgejo.base_url, "https://forge.example.com");
        assert_eq!(config.agents.get("codex").unwrap().timeout_secs, Some(60));
        assert!(!config.capacity.fallback);
        assert_eq!(config.capacity.cooldown_secs, 120);
        assert_eq!(config.capacity.markers, vec!["no tokens left".to_owned()]);
    }

    #[test]
    fn parses_agent_enabled_flag() {
        let config: Config = toml::from_str("[agents.pi]\nenabled = true\n").unwrap();
        assert_eq!(config.agents.get("pi").unwrap().enabled, Some(true));
    }

    #[test]
    fn keeps_one_session_per_conversation_by_default() {
        assert!(PiRpcConfig::default().session_per_conversation);

        let config: Config =
            toml::from_str("[pi_rpc]\nsession_per_conversation = false\n").unwrap();
        assert!(!config.pi_rpc.session_per_conversation);
    }

    #[test]
    fn removed_pi_rpc_max_agents_is_ignored() {
        // `[pi_rpc] max_agents` used to cap the pool separately. It is gone:
        // `[session] workers` is the single total agent count now, but an old
        // config that still carries the field must keep loading.
        let config: Config =
            toml::from_str("[session]\nworkers = 7\n\n[pi_rpc]\nmax_agents = 2\n").unwrap();
        assert_eq!(config.session.workers, 7);
        assert!(config.pi_rpc.session_per_conversation);
    }

    #[test]
    fn capacity_cooldown_defaults_to_five_hours() {
        assert_eq!(CapacityConfig::default().cooldown_secs, 5 * 60 * 60);
        assert_eq!(
            DEFAULT_CAPACITY_COOLDOWN_SECS,
            5 * 60 * 60,
            "the documented default is five hours"
        );

        // An explicit zero is treated as "unset" and normalized to the default.
        let mut config = CapacityConfig {
            cooldown_secs: 0,
            ..Default::default()
        };
        config.ensure_defaults();
        assert_eq!(config.cooldown_secs, DEFAULT_CAPACITY_COOLDOWN_SECS);
    }

    #[test]
    fn accepts_quota_as_an_alias_for_capacity() {
        let raw = r#"
[quota]
fallback = true
cooldown_secs = 42
markers = ["overloaded"]
"#;
        let config: Config = toml::from_str(raw).unwrap();
        assert!(config.capacity.fallback);
        assert_eq!(config.capacity.cooldown_secs, 42);
        assert_eq!(config.capacity.markers, vec!["overloaded".to_owned()]);
    }

    #[test]
    fn expands_tilde() {
        assert_eq!(expand_tilde(Path::new("/abs/x")), PathBuf::from("/abs/x"));
        // `~/x` either expands against $HOME or is left untouched; in both
        // cases the final component is `x`.
        assert!(expand_tilde(Path::new("~/x")).ends_with("x"));
    }

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn save_env(keys: &[&str]) -> Vec<(String, Option<std::ffi::OsString>)> {
        keys.iter()
            .map(|key| ((*key).to_owned(), std::env::var_os(key)))
            .collect()
    }

    fn restore_env(saved: &[(String, Option<std::ffi::OsString>)]) {
        for (key, value) in saved {
            // SAFETY: every caller holds `ENV_LOCK` while mutating the process
            // environment, so no concurrent test observes a partial update.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    #[test]
    fn expands_tilde_with_home_and_non_utf8() {
        let _guard = env_guard();
        let saved = save_env(&["HOME"]);
        unsafe { std::env::set_var("HOME", "/home/tester") };
        assert_eq!(
            expand_tilde(Path::new("~/x")),
            PathBuf::from("/home/tester/x")
        );
        assert_eq!(expand_tilde(Path::new("~")), PathBuf::from("/home/tester"));
        restore_env(&saved);

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let raw = std::ffi::OsStr::from_bytes(b"\xFF~");
            assert_eq!(expand_tilde(Path::new(raw)), PathBuf::from(raw));
        }
    }

    #[test]
    fn credentials_for_each_forge() {
        let mut config = Config::default();
        config.forges.forgejo = Some(ForgejoConfig {
            base_url: "http://forge.example.com".into(),
            token: Some("ft".into()),
            ..Default::default()
        });
        let creds = config.credentials_for(crate::location::ForgeKind::Forgejo);
        assert!(creds.contains(&("FORGEJO_URL".into(), "http://forge.example.com".into())));
        assert!(creds.contains(&("FORGEJO_TOKEN".into(), "ft".into())));
        assert!(creds.contains(&("GITEA_TOKEN".into(), "ft".into())));
        assert!(creds.contains(&("FORGE_URL".into(), "http://forge.example.com".into())));
        assert!(creds.contains(&("FORGE_TOKEN".into(), "ft".into())));

        config.forges.github = Some(GithubConfig {
            base_url: "http://gh.example.com".into(),
            token: Some("gt".into()),
            ..Default::default()
        });
        let creds = config.credentials_for(crate::location::ForgeKind::GitHub);
        assert!(creds.contains(&("GITHUB_URL".into(), "http://gh.example.com".into())));
        assert!(creds.contains(&("GITHUB_TOKEN".into(), "gt".into())));
        assert!(creds.contains(&("GH_TOKEN".into(), "gt".into())));
        assert!(creds.contains(&("FORGE_TOKEN".into(), "gt".into())));

        config.forges.gitlab = Some(GitlabConfig {
            base_url: "http://gl.example.com".into(),
            token: Some("lt".into()),
            ..Default::default()
        });
        let creds = config.credentials_for(crate::location::ForgeKind::GitLab);
        assert!(creds.contains(&("GITLAB_URL".into(), "http://gl.example.com".into())));
        assert!(creds.contains(&("GITLAB_TOKEN".into(), "lt".into())));
        assert!(creds.contains(&("FORGE_TOKEN".into(), "lt".into())));

        // An unknown forge exposes no credentials at all.
        assert!(
            config
                .credentials_for(crate::location::ForgeKind::Unknown)
                .is_empty()
        );
    }

    #[test]
    fn credentials_without_a_token_are_url_only() {
        let mut config = Config::default();
        config.forges.forgejo = Some(ForgejoConfig {
            base_url: "http://forge.example.com".into(),
            token: None,
            ..Default::default()
        });
        let creds = config.credentials_for(crate::location::ForgeKind::Gitea);
        assert!(creds.contains(&("FORGEJO_URL".into(), "http://forge.example.com".into())));
        assert!(!creds.iter().any(|(key, _)| key == "FORGE_TOKEN"));
    }

    #[test]
    fn apply_defaults_fills_blank_fields() {
        let mut config = Config::default();
        config.bind.clear();
        config.mention.clear();
        config.pi_rpc.command.clear();
        config.poller.interval_secs = 0;
        config.poller.discover_interval_secs = 0;
        config.poller.page_limit = 0;
        config.workspace.root = PathBuf::new();
        config.workspace.git_author_name.clear();
        config.workspace.git_author_email.clear();
        config.session.dir = PathBuf::new();
        config.session.workers = 0;
        config.session.queue_capacity = 0;

        config.apply_defaults();

        assert_eq!(config.bind, "0.0.0.0:8080");
        assert_eq!(config.mention, "@agent");
        assert_eq!(config.pi_rpc.command, "pi");
        assert_eq!(config.poller.interval_secs, 15);
        assert_eq!(config.poller.discover_interval_secs, 300);
        assert_eq!(config.poller.page_limit, 50);
        assert_eq!(
            config.workspace.root,
            PathBuf::from("/tmp/forge-bot-workspaces")
        );
        assert_eq!(config.workspace.git_author_name, "forge-bot");
        assert_eq!(config.workspace.git_author_email, "forge-bot@localhost");
        assert_eq!(config.session.dir, PathBuf::from("/tmp/forge-bot-state"));
        assert_eq!(config.session.workers, 1);
        assert_eq!(config.session.queue_capacity, 256);
    }

    #[test]
    fn trigger_is_trimmed() {
        let config = Config {
            mention: "  @hey  ".into(),
            ..Default::default()
        };
        assert_eq!(config.trigger(), "@hey");
    }

    #[test]
    fn apply_env_overrides_all_fields() {
        let _guard = env_guard();
        let keys = [
            "FORGE_BOT_BIND",
            "FORGE_BOT_MENTION",
            "FORGE_BOT_MAX_CONCURRENCY",
            "FORGE_BOT_WORKSPACE_ROOT",
            "FORGE_BOT_SESSION_DIR",
            "FORGEJO_WEBHOOK_SECRET",
            "FORGEJO_TOKEN",
            "FORGEJO_BASE_URL",
            "FORGEJO_BOT_USERNAME",
        ];
        let saved = save_env(&keys);
        unsafe {
            std::env::set_var("FORGE_BOT_BIND", "1.2.3.4:1234");
            std::env::set_var("FORGE_BOT_MENTION", "@env");
            std::env::set_var("FORGE_BOT_MAX_CONCURRENCY", "7");
            std::env::set_var("FORGE_BOT_WORKSPACE_ROOT", "/tmp/env-ws");
            std::env::set_var("FORGE_BOT_SESSION_DIR", "/tmp/env-state");
            std::env::set_var("FORGEJO_WEBHOOK_SECRET", "env-secret");
            std::env::set_var("FORGEJO_TOKEN", "env-token");
            std::env::set_var("FORGEJO_BASE_URL", "http://env-forge");
            std::env::set_var("FORGEJO_BOT_USERNAME", "env-bot");
        }

        let mut config = Config::default();
        config.forges.forgejo = Some(ForgejoConfig::default());
        config.apply_env();

        assert_eq!(config.bind, "1.2.3.4:1234");
        assert_eq!(config.mention, "@env");
        assert_eq!(config.session.workers, 7);
        assert_eq!(config.workspace.root, PathBuf::from("/tmp/env-ws"));
        assert_eq!(config.session.dir, PathBuf::from("/tmp/env-state"));
        let forgejo = config.forges.forgejo.unwrap();
        assert_eq!(forgejo.webhook_secret.as_deref(), Some("env-secret"));
        assert_eq!(forgejo.token.as_deref(), Some("env-token"));
        assert_eq!(forgejo.base_url, "http://env-forge");
        assert_eq!(forgejo.bot_username.as_deref(), Some("env-bot"));

        restore_env(&saved);
    }

    #[test]
    fn load_reads_or_falls_back_to_defaults() {
        let _guard = env_guard();
        // Keep ambient Forgejo env vars from leaking into the loaded config.
        let saved = save_env(&["FORGE_BOT_BIND", "FORGE_BOT_MENTION", "FORGEJO_TOKEN"]);
        unsafe {
            std::env::remove_var("FORGE_BOT_BIND");
            std::env::remove_var("FORGE_BOT_MENTION");
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "bind = \"127.0.0.1:9999\"\nmention = \"@bot\"\n").unwrap();
        let config = Config::load(Some(&path)).unwrap();
        assert_eq!(config.bind, "127.0.0.1:9999");
        assert_eq!(config.mention, "@bot");

        // A missing file is fine: the defaults plus env apply.
        let config = Config::load(Some(&dir.path().join("missing.toml"))).unwrap();
        assert_eq!(config.bind, "0.0.0.0:8080");

        // Structurally invalid TOML is a configuration error.
        std::fs::write(&path, "not = [valid").unwrap();
        let error = Config::load(Some(&path)).unwrap_err();
        assert!(error.to_string().contains("failed to parse"));

        restore_env(&saved);
    }
}

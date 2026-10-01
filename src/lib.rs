//! forge-bot — a forge-agnostic bot that routes `@agent` mentions to coding
//! agents.
//!
//! The architecture is deliberately small:
//!
//! ```text
//! Forge → webhook → ForgeAdapter → ForgeMessage
//!       → mention + policy → Dispatcher → AgentAdapter → CLI agent
//! ```
//!
//! The gateway only ever hands an agent a location URL and a message; the agent
//! is responsible for reading context, changing code, testing, pushing and
//! replying.

pub mod agent;
mod auto_trigger;
pub mod config;
pub mod error;
pub mod executor;
pub mod forge;
pub mod forge_api;
pub mod identity;
pub mod location;
pub mod mention;
pub mod policy;
pub mod poller;
pub mod session;
pub mod webhook;
pub mod workspace;

use std::collections::HashMap;
use std::sync::Arc;

use tracing_subscriber::EnvFilter;

use crate::agent::AgentRegistry;
use crate::config::Config;
use crate::error::Result;
use crate::forge::ForgeAdapter;
use crate::forge::forgejo::ForgejoAdapter;
use crate::forge::github::GithubAdapter;
use crate::forge::gitlab::GitlabAdapter;
use crate::forge_api::HttpForgeApi;
use crate::policy::Policy;
use crate::session::{Dispatcher, SessionStore};
use crate::webhook::AppState;

/// Initialize tracing. Safe to call more than once.
pub fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("forge_bot=info,tower_http=info,axum=info,warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

/// Instantiate the forge adapters enabled by configuration.
pub fn build_adapters(config: &Config) -> HashMap<String, Arc<dyn ForgeAdapter>> {
    let mut adapters: HashMap<String, Arc<dyn ForgeAdapter>> = HashMap::new();

    if let Some(forgejo) = &config.forges.forgejo {
        adapters.insert("forgejo".into(), Arc::new(ForgejoAdapter::new(forgejo)));
        // Gitea shares the Forgejo wire format.
        adapters.insert("gitea".into(), Arc::new(ForgejoAdapter::new(forgejo)));
    }
    if let Some(github) = &config.forges.github {
        adapters.insert("github".into(), Arc::new(GithubAdapter::new(github)));
    }
    if let Some(gitlab) = &config.forges.gitlab {
        adapters.insert("gitlab".into(), Arc::new(GitlabAdapter::new(gitlab)));
    }

    adapters
}

/// Build the policy, ignoring unconfigured forge bot accounts.
pub fn build_policy(config: &Config) -> Policy {
    let mut policy = Policy::new(&config.policy);
    if let Some(forgejo) = &config.forges.forgejo
        && let Some(user) = &forgejo.bot_username
        && !config
            .effective_logins()
            .iter()
            .any(|login| login.eq_ignore_ascii_case(user))
    {
        policy.ignore_user(user);
    }
    if let Some(github) = &config.forges.github
        && let Some(user) = &github.bot_username
    {
        policy.ignore_user(user);
    }
    if let Some(gitlab) = &config.forges.gitlab
        && let Some(user) = &gitlab.bot_username
    {
        policy.ignore_user(user);
    }
    policy
}

/// Assemble the complete application state.
pub fn build_app(config: Config) -> Result<AppState> {
    let config = Arc::new(config);

    let adapters = build_adapters(&config);
    let agents = Arc::new(AgentRegistry::from_config(&config));
    let policy = build_policy(&config);
    let sessions = Arc::new(SessionStore::open(config::expand_tilde(
        &config.session.dir,
    ))?);
    let api = Arc::new(HttpForgeApi::new((*config).clone())?);
    let dispatcher = Dispatcher::new(config.clone(), agents.clone(), sessions, api, policy)?;

    Ok(AppState::new(config.clone(), adapters, agents, dispatcher))
}

/// Run the webhook server until the process is stopped.
pub async fn serve(config: Config) -> Result<()> {
    let bind = config.bind.clone();
    let poller_enabled = config.poller.enabled;
    let app = build_app(config)?;

    if poller_enabled {
        let poller = Arc::new(poller::Poller::new(
            app.config.clone(),
            app.dispatcher.clone(),
        )?);
        tokio::spawn(poller.run());
    }

    let router = webhook::router(app);

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .map_err(|e| crate::error::BotError::Config(format!("cannot bind {bind}: {e}")))?;

    tracing::info!(%bind, "forge-bot listening");
    axum::serve(listener, router).await?;
    Ok(())
}

/// Run only the polling ingester (no webhook listener).
pub async fn poll(config: Config) -> Result<()> {
    let app = build_app(config)?;
    let poller = Arc::new(poller::Poller::new(
        app.config.clone(),
        app.dispatcher.clone(),
    )?);
    poller.run().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ForgejoConfig, GithubConfig, GitlabConfig, UserConfig, UserRole};

    fn config_with_all_forges(dir: &std::path::Path) -> Config {
        let mut config = Config::default();
        config.session.dir = dir.to_path_buf();
        config.workspace.enabled = false;
        config.reply.ack = false;
        config.reply.result = false;
        config.forges.forgejo = Some(ForgejoConfig {
            base_url: "http://forge.example.com".into(),
            bot_username: Some("forgejo-bot".into()),
            ..Default::default()
        });
        config.forges.github = Some(GithubConfig {
            base_url: "http://github.example.com".into(),
            bot_username: Some("github-bot".into()),
            ..Default::default()
        });
        config.forges.gitlab = Some(GitlabConfig {
            base_url: "http://gitlab.example.com".into(),
            bot_username: Some("gitlab-bot".into()),
            ..Default::default()
        });
        // Every run belongs to a configured user now; tests spawn directly
        // because the cgroup backend needs root.
        let passwd = dir.join("passwd");
        std::fs::write(
            &passwd,
            format!(
                "agent:x:1000:1000::{}:/bin/bash\n",
                dir.join("home/agent").display()
            ),
        )
        .unwrap();
        config.executor.passwd_file = passwd;
        config.executor.direct = true;
        config.users.insert(
            "primary".into(),
            UserConfig {
                role: UserRole::Default,
                host_user: "agent".into(),
                agent: None,
                agent_model: None,
                token: None,
            },
        );
        config
    }

    #[test]
    fn init_tracing_is_idempotent() {
        init_tracing();
        init_tracing();
    }

    #[test]
    fn builds_an_adapter_for_every_configured_forge() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_all_forges(dir.path());
        let adapters = build_adapters(&config);
        for name in ["forgejo", "gitea", "github", "gitlab"] {
            assert!(adapters.contains_key(name), "missing adapter {name}");
        }

        // An empty configuration wires no adapters.
        assert!(build_adapters(&Config::default()).is_empty());
    }

    #[test]
    fn policy_ignores_unconfigured_bot_users() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_all_forges(dir.path());
        let policy = build_policy(&config);
        assert!(!policy.is_ignored("forgejo-bot"));
        assert!(policy.is_ignored("github-bot"));
        assert!(policy.is_ignored("gitlab-bot"));
    }

    #[test]
    fn policy_allows_explicit_agent_handoffs() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_all_forges(dir.path());
        let user = |role, host: &str| UserConfig {
            role,
            host_user: host.into(),
            agent: None,
            agent_model: None,
            token: None,
        };
        config
            .users
            .insert("forgejo-bot".into(), user(UserRole::Default, "agent"));
        config.users.insert(
            "forgejo-reviewer".into(),
            user(UserRole::Reviewer, "reviewer"),
        );
        let policy = build_policy(&config);
        assert!(!policy.is_ignored("forgejo-bot"));
        assert!(!policy.is_ignored("forgejo-reviewer"));
    }

    #[tokio::test]
    async fn build_app_wires_adapters_and_dispatcher() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_all_forges(dir.path());
        let app = build_app(config).unwrap();
        assert_eq!(app.adapters.len(), 4);
        assert!(app.config.forges.github.is_some());
    }

    #[tokio::test]
    async fn serve_binds_and_stops() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_all_forges(dir.path());
        config.bind = "127.0.0.1:0".into();
        let handle = tokio::spawn(serve(config));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        handle.abort();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn poll_starts_the_ingester() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_all_forges(dir.path());
        config.poller.interval_secs = 1;
        let handle = tokio::spawn(poll(config));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        handle.abort();
        let _ = handle.await;
    }
}

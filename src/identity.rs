//! Explicit multi-user identity resolution.
//!
//! Every run belongs to a configured `[users.<id>]` account; there is no
//! implicit single-account mode. [`Identities::resolve`] turns the configured
//! users into validated [`UserRuntime`]s that carry the effective login, Linux
//! account, optional adapter, model and token.
//!
//! Resolution is deliberately separate from construction: [`Identities::resolve`]
//! only reads configuration and the registered agent names, so it can run
//! before any network or filesystem work and reject invalid configurations
//! before the first webhook is acknowledged.

use std::collections::HashSet;
use std::sync::Arc;

use crate::config::{Config, UserRole, is_valid_user_identifier};
use crate::error::{BotError, Result};

/// The resolved runtime of one agent user.
#[derive(Debug, Clone)]
pub struct UserRuntime {
    /// Stable table key, used to scope state and to address the user.
    pub id: String,
    /// Effective Forgejo login this user is addressed by.
    pub login: String,
    /// Real Linux account the agent runs as.
    pub host_user: String,
    /// Role of the user.
    pub role: UserRole,
    /// Optional adapter override.
    pub agent: Option<String>,
    /// Optional model ID configured for this user.
    pub agent_model: Option<String>,
    /// Forge token for this user. `None` means no own token; the default user
    /// may fall back to the global token at request time.
    pub token: Option<String>,
}

impl UserRuntime {
    /// The mention that addresses this user.
    pub fn trigger(&self) -> String {
        format!("@{}", self.login)
    }

    /// Token to use for this user. A user's own token always wins; only the
    /// default user falls back to the global token, so a non-default user
    /// never acts as the default bot.
    pub fn effective_token<'a>(&'a self, global: Option<&'a str>) -> Option<&'a str> {
        self.token
            .as_deref()
            .or_else(|| (self.role == UserRole::Default).then_some(global).flatten())
    }

    /// Whether `body` addresses this login as a complete handle.
    ///
    /// The trailing boundary prevents `@bot` from capturing `@bot-reviewer`,
    /// and the leading boundary keeps email-like `user@bot` text from matching.
    pub fn is_mentioned(&self, body: &str) -> bool {
        mention_login(body, &self.login)
    }
}

/// The resolved identity set for a running bot.
#[derive(Debug, Clone)]
pub struct Identities {
    users: Vec<Arc<UserRuntime>>,
}

impl Identities {
    /// Every configured user in configuration order.
    pub fn users(&self) -> &[Arc<UserRuntime>] {
        &self.users
    }

    /// Look up a user by id.
    pub fn get(&self, user_id: &str) -> Option<&Arc<UserRuntime>> {
        self.users.iter().find(|user| user.id == user_id)
    }

    /// The one user with `role = "default"`.
    pub fn default_user(&self) -> &Arc<UserRuntime> {
        self.users
            .iter()
            .find(|user| user.role == UserRole::Default)
            .expect("resolution guarantees exactly one default user")
    }

    /// Resolve the single user a comment body addresses.
    ///
    /// Rejects an empty mention (no configured user) and any comment that
    /// addresses more than one distinct user.
    pub fn recipient(&self, body: &str) -> Result<&Arc<UserRuntime>> {
        match self.matching_users(body).as_slice() {
            [] => Err(BotError::Unauthorized(
                "no configured agent user was mentioned".into(),
            )),
            [only] => Ok(only),
            many => {
                let logins: Vec<&str> = many.iter().map(|user| user.login.as_str()).collect();
                Err(BotError::Unauthorized(format!(
                    "comment addresses multiple agent users: {}",
                    logins.join(", ")
                )))
            }
        }
    }

    /// Every configured user a comment body addresses, in configuration order.
    pub fn matching_users(&self, body: &str) -> Vec<&Arc<UserRuntime>> {
        self.users
            .iter()
            .filter(|user| user.is_mentioned(body))
            .collect()
    }

    /// Resolve identities from configuration.
    ///
    /// `agent_names` is the set of registered adapter names, used to reject a
    /// user that selects an unknown adapter. At least one `[users.*]` table is
    /// required; the legacy single-account mode was removed.
    pub fn resolve(config: &Config, agent_names: &[String]) -> Result<Self> {
        if config.users.is_empty() {
            return Err(BotError::Config(
                "at least one [users.*] table is required; forge-bot no longer runs a \
                 single implicit account"
                    .into(),
            ));
        }
        if config.forges.forgejo.is_none() {
            return Err(BotError::Config(
                "explicit [users.*] configuration requires a [forgejo] section".into(),
            ));
        }

        let mut users = Vec::with_capacity(config.users.len());
        let mut defaults = 0usize;
        let mut seen_logins: HashSet<String> = HashSet::new();

        for (id, user) in &config.users {
            user.validate_name_and_account(id)?;
            if user.role == UserRole::Default {
                defaults += 1;
            }

            let login = config.effective_login(id, user.role);
            if !is_valid_user_identifier(&login) {
                return Err(BotError::Config(format!(
                    "user `{id}` derives an invalid login `{login}`"
                )));
            }
            if !seen_logins.insert(login.to_ascii_lowercase()) {
                return Err(BotError::Config(format!(
                    "user `{id}` derives a duplicate login `{login}`"
                )));
            }

            if let Some(agent) = &user.agent
                && !agent_names.iter().any(|name| name == agent)
            {
                return Err(BotError::Config(format!(
                    "user `{id}` selects unregistered agent `{agent}`"
                )));
            }

            users.push(Arc::new(UserRuntime {
                id: id.clone(),
                login,
                host_user: user.host_user.trim().to_owned(),
                role: user.role,
                agent: user.agent.clone(),
                agent_model: user.agent_model.clone(),
                token: user.token.clone(),
            }));
        }

        match defaults {
            1 => {}
            0 => {
                return Err(BotError::Config(
                    "explicit [users.*] configuration needs exactly one role = \"default\"".into(),
                ));
            }
            n => {
                return Err(BotError::Config(format!(
                    "explicit [users.*] configuration has {n} default users; exactly one is required"
                )));
            }
        }

        Ok(Self { users })
    }
}

/// Whether `body` contains `@<login>` as a complete handle.
fn mention_login(body: &str, login: &str) -> bool {
    if login.is_empty() {
        return false;
    }
    let needle = login.as_bytes();
    let bytes = body.as_bytes();
    let mut search = 0usize;
    while let Some(rel) = body[search..].find('@') {
        let at = search + rel;
        let before_ok = at == 0 || {
            let prev = body[..at].chars().next_back().unwrap();
            !(prev.is_alphanumeric() || matches!(prev, '_' | '-' | '.'))
        };
        if before_ok {
            let after = &bytes[at + 1..];
            if after.len() >= needle.len() && after[..needle.len()].eq_ignore_ascii_case(needle) {
                let tail = &after[needle.len()..];
                let tail_ok = tail.first().is_none_or(|&byte| {
                    !(byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
                });
                if tail_ok {
                    return true;
                }
            }
        }
        search = at + 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ForgejoConfig, UserConfig};

    fn config_with_users(users: &[(&str, UserRole, &str)]) -> Config {
        let mut config = Config::default();
        config.forges.forgejo = Some(ForgejoConfig {
            bot_username: Some("shylock-bot".into()),
            ..Default::default()
        });
        for (id, role, host) in users {
            config.users.insert(
                (*id).to_owned(),
                UserConfig {
                    role: *role,
                    host_user: (*host).to_owned(),
                    agent: None,
                    agent_model: None,
                    token: None,
                },
            );
        }
        config
    }

    fn names() -> Vec<String> {
        vec!["codex".into(), "pi-rpc".into()]
    }

    #[test]
    fn empty_users_is_rejected() {
        let config = Config::default();
        let error = Identities::resolve(&config, &names()).unwrap_err();
        assert!(error.to_string().contains("[users.*]"));
    }

    #[test]
    fn resolves_users_with_derived_logins() {
        let config = config_with_users(&[
            ("shylock-bot", UserRole::Default, "agent"),
            ("shylock-reviewer", UserRole::Reviewer, "reviewer"),
        ]);
        let identities = Identities::resolve(&config, &names()).unwrap();
        assert_eq!(identities.users().len(), 2);
        // The default inherits the configured bot username...
        assert_eq!(identities.default_user().login, "shylock-bot");
        // ... while a reviewer uses its table key.
        assert_eq!(
            identities.get("shylock-reviewer").unwrap().login,
            "shylock-reviewer"
        );
    }

    #[test]
    fn effective_token_prefers_own_and_only_the_default_inherits_global() {
        let mut config = config_with_users(&[
            ("shylock-bot", UserRole::Default, "agent"),
            ("shylock-reviewer", UserRole::Reviewer, "reviewer"),
        ]);
        config.users.get_mut("shylock-reviewer").unwrap().token = Some("reviewer-token".into());
        let identities = Identities::resolve(&config, &names()).unwrap();
        assert_eq!(
            identities
                .get("shylock-bot")
                .unwrap()
                .effective_token(Some("global")),
            Some("global")
        );
        assert_eq!(
            identities
                .get("shylock-reviewer")
                .unwrap()
                .effective_token(Some("global")),
            Some("reviewer-token")
        );

        // Without its own token a reviewer never inherits the global one.
        config.users.get_mut("shylock-reviewer").unwrap().token = None;
        let identities = Identities::resolve(&config, &names()).unwrap();
        assert_eq!(
            identities
                .get("shylock-reviewer")
                .unwrap()
                .effective_token(Some("global")),
            None
        );
        assert_eq!(
            identities
                .get("shylock-bot")
                .unwrap()
                .effective_token(Some("global")),
            Some("global")
        );
    }

    #[test]
    fn default_login_falls_back_to_table_key() {
        let mut config = config_with_users(&[("primary", UserRole::Default, "agent")]);
        config.forges.forgejo.as_mut().unwrap().bot_username = None;
        let identities = Identities::resolve(&config, &names()).unwrap();
        assert_eq!(identities.default_user().login, "primary");
    }

    #[test]
    fn requires_exactly_one_default() {
        let none = config_with_users(&[("a", UserRole::Reviewer, "agent")]);
        assert!(Identities::resolve(&none, &names()).is_err());

        let two = config_with_users(&[
            ("a", UserRole::Default, "agent"),
            ("b", UserRole::Default, "agent2"),
        ]);
        assert!(Identities::resolve(&two, &names()).is_err());
    }

    #[test]
    fn rejects_duplicate_logins_case_insensitively() {
        // The default derives `shylock-bot`; the reviewer id collides with it.
        let config = config_with_users(&[
            ("shylock-bot", UserRole::Default, "agent"),
            ("SHYLOCK-BOT", UserRole::Reviewer, "reviewer"),
        ]);
        let error = Identities::resolve(&config, &names()).unwrap_err();
        assert!(error.to_string().contains("duplicate login"));
    }

    #[test]
    fn rejects_invalid_ids_accounts_and_agents() {
        let bad_id = config_with_users(&[("bad id", UserRole::Default, "agent")]);
        assert!(Identities::resolve(&bad_id, &names()).is_err());

        let bad_account = config_with_users(&[("a", UserRole::Default, "root")]);
        assert!(Identities::resolve(&bad_account, &names()).is_err());

        let mut unknown_agent = config_with_users(&[("a", UserRole::Default, "agent")]);
        unknown_agent.users.get_mut("a").unwrap().agent = Some("nope".into());
        assert!(Identities::resolve(&unknown_agent, &names()).is_err());
    }

    #[test]
    fn requires_forgejo() {
        let mut config = config_with_users(&[("a", UserRole::Default, "agent")]);
        config.forges.forgejo = None;
        assert!(Identities::resolve(&config, &names()).is_err());
    }

    #[test]
    fn recipient_requires_exactly_one_user() {
        let config = config_with_users(&[
            ("shylock-bot", UserRole::Default, "agent"),
            ("shylock-reviewer", UserRole::Reviewer, "reviewer"),
        ]);
        let identities = Identities::resolve(&config, &names()).unwrap();

        let user = identities.recipient("@shylock-reviewer do it").unwrap();
        assert_eq!(user.id, "shylock-reviewer");

        assert!(identities.recipient("no mention here").is_err());
        assert!(
            identities
                .recipient("@shylock-bot and @shylock-reviewer")
                .is_err()
        );
    }

    #[test]
    fn login_boundary_does_not_capture_a_longer_handle() {
        let config = config_with_users(&[
            ("bot", UserRole::Default, "agent"),
            ("bot-reviewer", UserRole::Reviewer, "reviewer"),
        ]);
        let identities = Identities::resolve(&config, &names()).unwrap();
        let user = identities.recipient("@bot-reviewer review").unwrap();
        assert_eq!(user.id, "bot-reviewer");
    }

    #[test]
    fn mention_login_rules() {
        assert!(mention_login("@bot hello", "bot"));
        assert!(mention_login("hey @bot", "bot"));
        assert!(mention_login("hey @Bot!", "bot"));
        assert!(!mention_login("@bot-reviewer", "bot"));
        assert!(!mention_login("mail me at user@bot.com", "bot"));
        assert!(!mention_login("@bot", ""));
        assert!(!mention_login("nothing", "bot"));
        assert!(mention_login("@böt?", "böt"));
    }

    #[test]
    fn accepts_user_model_without_adapter_mapping() {
        let mut config = config_with_users(&[("a", UserRole::Default, "agent")]);
        config.users.get_mut("a").unwrap().agent = Some("codex".into());
        config.users.get_mut("a").unwrap().agent_model = Some("gpt-fast".into());
        let identities = Identities::resolve(&config, &names()).unwrap();
        assert_eq!(
            identities.users()[0].agent_model.as_deref(),
            Some("gpt-fast")
        );
    }

    #[test]
    fn parses_explicit_user_table() {
        let raw = r#"
[forgejo]
bot_username = "shylock-bot"

[users.shylock-bot]
role = "default"
host_user = "agent"

[users.shylock-reviewer]
role = "reviewer"
host_user = "shylock-reviewer"
agent = "codex"
agent_model = "gpt-fast"
"#;
        let config: Config = toml::from_str(raw).unwrap();
        assert_eq!(config.users.len(), 2);
        assert_eq!(
            config
                .users
                .get("shylock-reviewer")
                .unwrap()
                .agent
                .as_deref(),
            Some("codex")
        );
        assert_eq!(
            config.effective_login("shylock-bot", UserRole::Default),
            "shylock-bot"
        );
        let identities = Identities::resolve(&config, &names()).unwrap();
        assert_eq!(identities.users().len(), 2);
    }

    #[test]
    fn role_is_required() {
        let raw = "[users.a]\nhost_user = \"agent\"\n";
        assert!(toml::from_str::<Config>(raw).is_err());
    }
}

//! Authorization policy.
//!
//! A webhook is authenticated by its signature, but authentication is not
//! authorization: we still need to decide whether *this user* may trigger the
//! bot on *this repository*.

use std::collections::HashSet;

use crate::config::PolicyConfig;
use crate::error::{BotError, Result};
use crate::forge::ForgeMessage;

/// Authorizes incoming triggers.
#[derive(Debug, Clone, Default)]
pub struct Policy {
    allow_all: bool,
    allowed_users: HashSet<String>,
    allowed_repos: HashSet<String>,
    ignored_users: HashSet<String>,
}

impl Policy {
    pub fn new(config: &PolicyConfig) -> Self {
        Self {
            allow_all: config.allow_all,
            allowed_users: config
                .allowed_users
                .iter()
                .map(|u| u.to_lowercase())
                .collect(),
            allowed_repos: config
                .allowed_repos
                .iter()
                .map(|r| r.to_lowercase())
                .collect(),
            ignored_users: config
                .ignored_users
                .iter()
                .map(|u| u.to_lowercase())
                .collect(),
        }
    }

    /// Add a user to the ignore list (e.g. the bot's own login).
    pub fn ignore_user(&mut self, user: impl Into<String>) {
        self.ignored_users.insert(user.into().to_lowercase());
    }

    /// Should the author be skipped (bot self-replies, known loops, ...)?
    pub fn is_ignored(&self, author: &str) -> bool {
        self.ignored_users.contains(&author.to_lowercase())
    }

    /// Authorize a message, returning a descriptive error when denied.
    pub fn authorize(&self, message: &ForgeMessage) -> Result<()> {
        self.authorize_with_agent(message, false)
    }

    /// Configured agents may hand work to peers within the repository policy.
    pub(crate) fn authorize_with_agent(&self, message: &ForgeMessage, agent: bool) -> Result<()> {
        if self.is_ignored(&message.author) {
            return Err(BotError::Unauthorized(format!(
                "author `{}` is ignored",
                message.author
            )));
        }

        if self.allow_all
            || (agent
                && (!self.allowed_users.is_empty() || !self.allowed_repos.is_empty())
                && (self.allowed_repos.is_empty()
                    || self
                        .allowed_repos
                        .contains(&message.repository.to_lowercase())))
        {
            return Ok(());
        }

        // An empty policy denies everyone: authentication is not
        // authorization, and the operator must opt in.
        if self.allowed_users.is_empty() && self.allowed_repos.is_empty() {
            return Err(BotError::Unauthorized(
                "no users or repositories are allow-listed".into(),
            ));
        }

        let author = message.author.to_lowercase();
        let repo = message.repository.to_lowercase();

        // A non-empty list restricts; an empty list means "no restriction" for
        // that dimension, as long as the other dimension is configured.
        let user_ok = self.allowed_users.is_empty() || self.allowed_users.contains(&author);
        let repo_ok = self.allowed_repos.is_empty() || self.allowed_repos.contains(&repo);

        if user_ok && repo_ok {
            Ok(())
        } else {
            Err(BotError::Unauthorized(format!(
                "`{}` is not allowed to trigger the bot on `{}`",
                message.author, message.repository
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::location::ForgeKind;
    use url::Url;

    fn msg(author: &str, repo: &str) -> ForgeMessage {
        ForgeMessage {
            forge: ForgeKind::Forgejo,
            location: Url::parse("https://forge.example.com/o/r/issues/1").unwrap(),
            body: "@agent hi".into(),
            author: author.into(),
            repository: repo.into(),
            comment_id: None,
            number: Some(1),
            is_pull_request: false,
            linked_issue: None,
            event: "issue_comment".into(),
            title: None,
            reply_target: Default::default(),
        }
    }

    #[test]
    fn denies_by_default() {
        let policy = Policy::new(&PolicyConfig::default());
        assert!(policy.authorize(&msg("alice", "o/r")).is_err());
    }

    #[test]
    fn allow_all_allows_everyone() {
        let policy = Policy::new(&PolicyConfig {
            allow_all: true,
            ..Default::default()
        });
        assert!(policy.authorize(&msg("stranger", "any/repo")).is_ok());
    }

    #[test]
    fn user_allow_list_is_case_insensitive() {
        let policy = Policy::new(&PolicyConfig {
            allowed_users: vec!["Alice".into()],
            ..Default::default()
        });
        assert!(policy.authorize(&msg("alice", "o/r")).is_ok());
        assert!(policy.authorize(&msg("bob", "o/r")).is_err());
    }

    #[test]
    fn repo_allow_list() {
        let policy = Policy::new(&PolicyConfig {
            allowed_repos: vec!["Org/Repo".into()],
            ..Default::default()
        });
        assert!(policy.authorize(&msg("anyone", "org/repo")).is_ok());
        assert!(policy.authorize(&msg("anyone", "org/other")).is_err());
    }

    #[test]
    fn agent_handoffs_keep_repository_and_explicit_ignore_restrictions() {
        let mut policy = Policy::new(&PolicyConfig {
            allowed_users: vec!["alice".into()],
            allowed_repos: vec!["o/r".into()],
            ..Default::default()
        });
        assert!(
            policy
                .authorize_with_agent(&msg("bot", "o/r"), true)
                .is_ok()
        );
        assert!(policy.authorize(&msg("bot", "o/r")).is_err());
        assert!(
            policy
                .authorize_with_agent(&msg("bot", "o/other"), true)
                .is_err()
        );
        policy.ignore_user("bot");
        assert!(
            policy
                .authorize_with_agent(&msg("bot", "o/r"), true)
                .is_err()
        );
    }

    #[test]
    fn ignored_user_wins() {
        let mut policy = Policy::new(&PolicyConfig {
            allow_all: true,
            ..Default::default()
        });
        policy.ignore_user("botty");
        assert!(policy.authorize(&msg("botty", "o/r")).is_err());
    }
}

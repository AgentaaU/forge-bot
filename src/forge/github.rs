//! GitHub webhook adapter.
//!
//! GitHub reuses the same `issue_comment` payload shape as Forgejo/Gitea, so
//! parsing is delegated to the shared helper. Verification uses the
//! `X-Hub-Signature-256` HMAC header.

use axum::http::HeaderMap;

use crate::config::GithubConfig;
use crate::error::{BotError, Result};
use crate::forge::{ForgeAdapter, ForgeMessage, parse_comment_payload, verify_hmac_sha256};
use crate::location::ForgeKind;

/// GitHub webhook adapter.
#[derive(Debug, Clone)]
pub struct GithubAdapter {
    base_url: String,
    secret: Option<Vec<u8>>,
    bot_username: Option<String>,
    pub token: Option<String>,
}

impl GithubAdapter {
    pub fn new(config: &GithubConfig) -> Self {
        let base_url = if config.base_url.is_empty() {
            "https://github.com".to_owned()
        } else {
            config.base_url.trim_end_matches('/').to_owned()
        };
        Self {
            base_url,
            secret: config
                .webhook_secret
                .as_ref()
                .map(|s| s.as_bytes().to_vec()),
            bot_username: config.bot_username.clone(),
            token: config.token.clone(),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn bot_username(&self) -> Option<&str> {
        self.bot_username.as_deref()
    }
}

#[async_trait::async_trait]
impl ForgeAdapter for GithubAdapter {
    fn kind(&self) -> ForgeKind {
        ForgeKind::GitHub
    }

    fn slug(&self) -> &'static str {
        "github"
    }

    fn verify(&self, headers: &HeaderMap, body: &[u8]) -> Result<()> {
        let Some(secret) = self.secret.as_ref().filter(|secret| !secret.is_empty()) else {
            return Err(BotError::Verification(
                "webhook secret is absent or empty".into(),
            ));
        };

        let signature = headers
            .get("x-hub-signature-256")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| BotError::Verification("missing X-Hub-Signature-256 header".into()))?;

        if verify_hmac_sha256(secret, body, signature) {
            Ok(())
        } else {
            Err(BotError::Verification("signature mismatch".into()))
        }
    }

    fn event(&self, headers: &HeaderMap) -> String {
        headers
            .get("x-github-event")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    }

    fn parse(&self, headers: &HeaderMap, body: &[u8]) -> Result<Vec<ForgeMessage>> {
        let event = self.event(headers);
        if event != "issue_comment" && !event.is_empty() {
            return Ok(Vec::new());
        }
        parse_comment_payload(ForgeKind::GitHub, &self.base_url, body, &event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::hmac_sha256_hex;

    #[test]
    fn empty_secret_rejects_even_an_empty_key_signature() {
        let adapter = GithubAdapter::new(&GithubConfig {
            webhook_secret: Some(String::new()),
            ..Default::default()
        });
        let mut headers = HeaderMap::new();
        let signature = crate::forge::hmac_sha256_hex(b"", b"body");
        headers.insert("x-forgejo-signature", signature.parse().unwrap());
        headers.insert(
            "x-hub-signature-256",
            format!("sha256={signature}").parse().unwrap(),
        );
        headers.insert("x-gitlab-token", "".parse().unwrap());
        assert!(adapter.verify(&headers, b"body").is_err());
    }

    #[test]
    fn parses_github_issue_comment() {
        let a = GithubAdapter::new(&GithubConfig {
            base_url: String::new(),
            webhook_secret: Some("s".into()),
            token: None,
            bot_username: None,
        });
        let raw = r#"{
            "action": "created",
            "issue": {"number": 12, "title": "bug",
                      "html_url": "https://github.com/o/r/issues/12"},
            "comment": {"id": 9, "body": "@agent fix",
                        "html_url": "https://github.com/o/r/issues/12#issuecomment-9",
                        "user": {"login": "dev"}},
            "repository": {"full_name": "o/r"}
        }"#;
        let body = raw.as_bytes();
        let sig = hmac_sha256_hex(b"s", body);
        let mut h = HeaderMap::new();
        h.insert("x-github-event", "issue_comment".parse().unwrap());
        h.insert(
            "x-hub-signature-256",
            format!("sha256={sig}").parse().unwrap(),
        );
        let msgs = a.handle(&h, body).unwrap();
        assert_eq!(msgs[0].forge, ForgeKind::GitHub);
        assert_eq!(msgs[0].number, Some(12));
    }

    #[test]
    fn metadata_and_verification() {
        let a = GithubAdapter::new(&GithubConfig {
            base_url: String::new(),
            webhook_secret: None,
            token: Some("t".into()),
            bot_username: Some("bot".into()),
        });
        assert_eq!(a.base_url(), "https://github.com");
        assert_eq!(a.bot_username(), Some("bot"));
        assert_eq!(a.kind(), ForgeKind::GitHub);
        assert_eq!(a.slug(), "github");
        // No secret configured: verification is rejected.
        assert!(a.verify(&HeaderMap::new(), b"body").is_err());

        let signed = GithubAdapter::new(&GithubConfig {
            base_url: "https://github.example.com/".into(),
            webhook_secret: Some("s".into()),
            ..Default::default()
        });
        assert_eq!(signed.base_url(), "https://github.example.com");
        // Missing signature header.
        assert!(matches!(
            signed.verify(&HeaderMap::new(), b"body").unwrap_err(),
            BotError::Verification(_)
        ));
        // Wrong signature.
        let mut bad = HeaderMap::new();
        bad.insert("x-hub-signature-256", "sha256=deadbeef".parse().unwrap());
        assert!(signed.verify(&bad, b"body").is_err());
    }

    #[test]
    fn ignores_non_issue_events_and_parses_empty_event() {
        let a = GithubAdapter::new(&GithubConfig::default());
        let mut push = HeaderMap::new();
        push.insert("x-github-event", "push".parse().unwrap());
        assert!(a.parse(&push, b"{}").unwrap().is_empty());

        let raw = r#"{
            "issue": {"number": 1},
            "comment": {"id": 1, "body": "@agent go"},
            "repository": {"full_name": "o/r"}
        }"#;
        let msgs = a.parse(&HeaderMap::new(), raw.as_bytes()).unwrap();
        assert_eq!(msgs.len(), 1);
    }
}

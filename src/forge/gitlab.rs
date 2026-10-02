//! GitLab webhook adapter.
//!
//! GitLab sends a different payload shape (`object_kind: "note"`) and
//! authenticates webhooks with a plain secret in the `X-Gitlab-Token` header.

use axum::http::HeaderMap;
use serde_json::Value;
use url::Url;

use crate::config::GitlabConfig;
use crate::error::{BotError, Result};
use crate::forge::{ForgeAdapter, ForgeMessage, constant_time_eq, linked_issue_ref};
use crate::location::ForgeKind;

/// GitLab webhook adapter.
#[derive(Debug, Clone)]
pub struct GitlabAdapter {
    base_url: String,
    secret: Option<Vec<u8>>,
    bot_username: Option<String>,
    pub token: Option<String>,
}

impl GitlabAdapter {
    pub fn new(config: &GitlabConfig) -> Self {
        let base_url = if config.base_url.is_empty() {
            "https://gitlab.com".to_owned()
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
impl ForgeAdapter for GitlabAdapter {
    fn kind(&self) -> ForgeKind {
        ForgeKind::GitLab
    }

    fn slug(&self) -> &'static str {
        "gitlab"
    }

    fn verify(&self, headers: &HeaderMap, _body: &[u8]) -> Result<()> {
        let Some(secret) = self.secret.as_ref().filter(|secret| !secret.is_empty()) else {
            return Err(BotError::Verification(
                "webhook secret is absent or empty".into(),
            ));
        };

        let provided = headers
            .get("x-gitlab-token")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| BotError::Verification("missing X-Gitlab-Token header".into()))?;

        if constant_time_eq(secret, provided.as_bytes()) {
            Ok(())
        } else {
            Err(BotError::Verification("token mismatch".into()))
        }
    }

    fn event(&self, headers: &HeaderMap) -> String {
        headers
            .get("x-gitlab-event")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    }

    fn parse(&self, headers: &HeaderMap, body: &[u8]) -> Result<Vec<ForgeMessage>> {
        let event = self.event(headers);
        let payload: Value = serde_json::from_slice(body)?;

        let object_kind = payload
            .get("object_kind")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if object_kind != "note" && event != "Note Hook" && !event.is_empty() {
            return Ok(Vec::new());
        }

        let attrs = payload
            .get("object_attributes")
            .ok_or_else(|| BotError::InvalidPayload("missing object_attributes".into()))?;

        // Ignore system notes (labels, merges, ...) which have no real body.
        if attrs
            .get("system")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Ok(Vec::new());
        }

        let Some(note) = attrs.get("note").and_then(Value::as_str) else {
            return Ok(Vec::new());
        };

        let noteable_type = attrs
            .get("noteable_type")
            .and_then(Value::as_str)
            .unwrap_or("Issue");
        let is_pull_request = noteable_type.eq_ignore_ascii_case("MergeRequest");

        let project = payload
            .get("project")
            .ok_or_else(|| BotError::InvalidPayload("missing project".into()))?;
        let full_name = project
            .get("path_with_namespace")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                BotError::InvalidPayload("missing project.path_with_namespace".into())
            })?;

        let author = payload
            .get("user")
            .and_then(|u| u.get("username"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();

        let comment_id = attrs.get("id").and_then(Value::as_i64);
        let number = if is_pull_request {
            payload
                .get("merge_request")
                .and_then(|m| m.get("iid"))
                .and_then(Value::as_u64)
        } else {
            payload
                .get("issue")
                .and_then(|i| i.get("iid"))
                .and_then(Value::as_u64)
        };
        let title = if is_pull_request {
            payload
                .get("merge_request")
                .and_then(|m| m.get("title"))
                .and_then(Value::as_str)
        } else {
            payload
                .get("issue")
                .and_then(|i| i.get("title"))
                .and_then(Value::as_str)
        }
        .map(str::to_owned);

        // A merge request links to the issue it closes through its
        // description; keep both on the same conversation when it does.
        let linked_issue = if is_pull_request {
            payload
                .get("merge_request")
                .and_then(|m| m.get("description"))
                .and_then(Value::as_str)
                .and_then(linked_issue_ref)
        } else {
            None
        };

        let location = attrs
            .get("url")
            .and_then(Value::as_str)
            .and_then(|s| Url::parse(s).ok())
            .or_else(|| {
                project
                    .get("web_url")
                    .and_then(Value::as_str)
                    .and_then(|s| Url::parse(s).ok())
            })
            .ok_or_else(|| BotError::InvalidLocation {
                location: self.base_url.clone(),
                reason: "no URL in GitLab note payload".into(),
            })?;

        Ok(vec![ForgeMessage {
            forge: ForgeKind::GitLab,
            location,
            body: note.to_owned(),
            author,
            repository: full_name.to_owned(),
            comment_id,
            number,
            is_pull_request,
            linked_issue,
            event,
            title,
            reply_target: Default::default(),
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::IssueRef;

    fn adapter() -> GitlabAdapter {
        GitlabAdapter::new(&GitlabConfig {
            base_url: "https://gitlab.example.com".into(),
            webhook_secret: Some("glsecret".into()),
            token: None,
            bot_username: None,
        })
    }

    #[test]
    fn empty_secret_rejects_even_an_empty_key_signature() {
        let adapter = GitlabAdapter::new(&GitlabConfig {
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
    fn verifies_token() {
        let a = adapter();
        let mut h = HeaderMap::new();
        h.insert("x-gitlab-token", "glsecret".parse().unwrap());
        assert!(a.verify(&h, b"").is_ok());

        let mut bad = HeaderMap::new();
        bad.insert("x-gitlab-token", "nope".parse().unwrap());
        assert!(a.verify(&bad, b"").is_err());
    }

    #[test]
    fn parses_merge_request_note() {
        let a = adapter();
        let raw = r#"{
            "object_kind": "note",
            "user": {"username": "dev"},
            "project": {"path_with_namespace": "group/proj",
                        "web_url": "https://gitlab.example.com/group/proj"},
            "object_attributes": {
                "note": "@agent review this",
                "id": 42,
                "system": false,
                "noteable_type": "MergeRequest",
                "url": "https://gitlab.example.com/group/proj/-/merge_requests/9#note_42"
            },
            "merge_request": {"iid": 9, "title": "Add feature"}
        }"#;
        let mut h = HeaderMap::new();
        h.insert("x-gitlab-event", "Note Hook".parse().unwrap());
        let msgs = a.parse(&h, raw.as_bytes()).unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].is_pull_request);
        assert_eq!(msgs[0].number, Some(9));
        assert_eq!(msgs[0].repository, "group/proj");
        assert_eq!(msgs[0].comment_id, Some(42));
    }

    #[test]
    fn links_merge_request_to_issue_in_another_project() {
        let a = adapter();
        let raw = r#"{
            "object_kind": "note",
            "user": {"username": "dev"},
            "project": {"path_with_namespace": "group/proj",
                        "web_url": "https://gitlab.example.com/group/proj"},
            "object_attributes": {
                "note": "@agent review this",
                "id": 42,
                "system": false,
                "noteable_type": "MergeRequest",
                "url": "https://gitlab.example.com/group/proj/-/merge_requests/9#note_42"
            },
            "merge_request": {
                "iid": 9,
                "title": "Add feature",
                "description": "This closes other/sub/proj#5."
            }
        }"#;
        let mut h = HeaderMap::new();
        h.insert("x-gitlab-event", "Note Hook".parse().unwrap());
        let msgs = a.parse(&h, raw.as_bytes()).unwrap();
        assert_eq!(
            msgs[0].linked_issue,
            Some(IssueRef {
                repository: Some("other/sub/proj".into()),
                number: 5,
            })
        );
    }

    #[test]
    fn verify_handles_missing_secret_and_header() {
        let no_secret = GitlabAdapter::new(&GitlabConfig {
            base_url: String::new(),
            webhook_secret: None,
            bot_username: Some("bot".into()),
            ..Default::default()
        });
        assert_eq!(no_secret.base_url(), "https://gitlab.com");
        assert_eq!(no_secret.bot_username(), Some("bot"));
        assert_eq!(no_secret.kind(), ForgeKind::GitLab);
        assert_eq!(no_secret.slug(), "gitlab");
        assert!(no_secret.verify(&HeaderMap::new(), b"").is_err());

        let with_secret = adapter();
        let error = with_secret.verify(&HeaderMap::new(), b"").unwrap_err();
        assert!(matches!(error, BotError::Verification(_)));
    }

    #[test]
    fn ignores_non_note_and_system_events() {
        let a = adapter();
        let mut h = HeaderMap::new();
        h.insert("x-gitlab-event", "Push Hook".parse().unwrap());
        assert!(
            a.parse(&h, br#"{"object_kind":"push"}"#)
                .unwrap()
                .is_empty()
        );

        let system = r#"{
            "object_kind": "note",
            "project": {"path_with_namespace": "g/p"},
            "object_attributes": {"system": true, "note": "label added"}
        }"#;
        assert!(
            a.parse(&HeaderMap::new(), system.as_bytes())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn parses_issue_note_without_an_event_header() {
        let a = adapter();
        let raw = r#"{
            "object_kind": "note",
            "user": {"username": "dev"},
            "project": {"path_with_namespace": "group/proj",
                        "web_url": "https://gitlab.example.com/group/proj"},
            "object_attributes": {
                "note": "@agent fix this",
                "id": 7,
                "system": false,
                "noteable_type": "Issue",
                "url": "https://gitlab.example.com/group/proj/-/issues/3#note_7"
            },
            "issue": {"iid": 3, "title": "Bug"}
        }"#;
        let msgs = a.parse(&HeaderMap::new(), raw.as_bytes()).unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(!msgs[0].is_pull_request);
        assert_eq!(msgs[0].number, Some(3));
        assert_eq!(msgs[0].title.as_deref(), Some("Bug"));
        assert_eq!(msgs[0].linked_issue, None);
    }

    #[test]
    fn rejects_incomplete_payloads() {
        let a = adapter();
        // Missing object_attributes.
        assert!(
            a.parse(&HeaderMap::new(), br#"{"object_kind":"note"}"#)
                .is_err()
        );
        // Missing note body is simply ignored.
        let no_note = br#"{"object_kind":"note","object_attributes":{"system":false}}"#;
        assert!(a.parse(&HeaderMap::new(), no_note).unwrap().is_empty());
        // Missing project.
        let no_project =
            br#"{"object_kind":"note","object_attributes":{"system":false,"note":"x"}}"#;
        assert!(a.parse(&HeaderMap::new(), no_project).is_err());
        // Missing path_with_namespace.
        let no_path = br#"{"object_kind":"note","project":{},"object_attributes":{"system":false,"note":"x"}}"#;
        assert!(a.parse(&HeaderMap::new(), no_path).is_err());
        // No URL anywhere.
        let no_url = br#"{"object_kind":"note","project":{"path_with_namespace":"g/p"},"object_attributes":{"system":false,"note":"x"}}"#;
        assert!(matches!(
            a.parse(&HeaderMap::new(), no_url).unwrap_err(),
            BotError::InvalidLocation { .. }
        ));
    }
}

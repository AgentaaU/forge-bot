//! Forgejo / Gitea webhook adapter.

use axum::http::HeaderMap;
use serde_json::Value;
use url::Url;

use crate::config::ForgejoConfig;
use crate::error::{BotError, Result};
use crate::forge::{
    ForgeAdapter, ForgeMessage, ReplyTarget, ReviewCommentTarget, linked_issue_ref,
    parse_comment_payload, parse_description_payload, verify_hmac_sha256,
};
use crate::location::ForgeKind;

/// Forgejo webhook adapter.
#[derive(Debug, Clone)]
pub struct ForgejoAdapter {
    base_url: String,
    secret: Option<Vec<u8>>,
    bot_username: Option<String>,
    pub token: Option<String>,
}

impl ForgejoAdapter {
    pub fn new(config: &ForgejoConfig) -> Self {
        Self {
            base_url: config.base_url.trim_end_matches('/').to_owned(),
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
impl ForgeAdapter for ForgejoAdapter {
    fn kind(&self) -> ForgeKind {
        ForgeKind::Forgejo
    }

    fn slug(&self) -> &'static str {
        "forgejo"
    }

    fn verify(&self, headers: &HeaderMap, body: &[u8]) -> Result<()> {
        let Some(secret) = self.secret.as_ref() else {
            tracing::warn!("forgejo: no webhook secret configured; skipping verification");
            return Ok(());
        };

        let signature = headers
            .get("x-forgejo-signature")
            .or_else(|| headers.get("x-gitea-signature"))
            .and_then(|v| v.to_str().ok());

        let Some(signature) = signature else {
            return Err(BotError::Verification(
                "missing X-Forgejo-Signature header".into(),
            ));
        };

        if verify_hmac_sha256(secret, body, signature) {
            Ok(())
        } else {
            Err(BotError::Verification("signature mismatch".into()))
        }
    }

    fn event(&self, headers: &HeaderMap) -> String {
        headers
            .get("x-forgejo-event")
            .or_else(|| headers.get("x-gitea-event"))
            .or_else(|| headers.get("x-github-event"))
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    }

    fn parse(&self, headers: &HeaderMap, body: &[u8]) -> Result<Vec<ForgeMessage>> {
        let event = self.event(headers);

        match event.as_str() {
            // Gitea/Forgejo signal conversation comments with `issue_comment`
            // plus a `pull_request` object; older versions inline inline review
            // comments in `pull_request_review_comment` events.
            "issue_comment" | "pull_request_review_comment" | "" => {
                let mut messages =
                    parse_comment_payload(ForgeKind::Forgejo, &self.base_url, body, &event)?;
                // The shared parser cannot tell a review-comment reply from a
                // conversation comment, so recover the thread coordinates when
                // the payload happens to carry them.
                apply_review_reply_target(&mut messages, body);
                Ok(messages)
            }
            // Forgejo signals reviews with `pull_request_comment` but does not
            // inline the comments; `enrich` fetches them from the API.
            "pull_request_comment" => Ok(Vec::new()),
            // Mentions in an issue or pull-request description.
            "issues" | "pull_request" => {
                parse_description_payload(ForgeKind::Forgejo, &self.base_url, body, &event)
            }
            _ => Ok(Vec::new()),
        }
    }

    async fn enrich(
        &self,
        messages: &mut Vec<ForgeMessage>,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Result<()> {
        if self.event(headers) != "pull_request_comment" {
            // Forgejo sends replies to inline review comments as issue_comment
            // events, often without the review id, path, or position. Resolve
            // the comment id through the review API before dispatching it.
            for message in messages.iter_mut().filter(|message| {
                message.is_pull_request
                    && message.comment_id.is_some()
                    && matches!(message.reply_target, ReplyTarget::Conversation)
                    && self.bot_username.as_deref() != Some(message.author.as_str())
            }) {
                if let Some(target) = self.find_review_reply_target(message).await? {
                    message.reply_target = target;
                }
            }
            return Ok(());
        }

        let payload: Value = serde_json::from_slice(body)?;
        let Some(review) = payload.get("review") else {
            return Ok(());
        };
        if review.get("type").and_then(Value::as_str) != Some("pull_request_review_comment") {
            return Ok(());
        }

        // The review body can itself carry a mention.
        if let Some(content) = review.get("content").and_then(Value::as_str)
            && !content.trim().is_empty()
            && let Some(message) = review_message(&payload, None, content)
        {
            messages.push(message);
        }

        // Inline comments are absent from the payload; look them up.
        let Some(token) = self.token.as_deref() else {
            return Ok(());
        };
        let Some(full_name) = payload
            .pointer("/repository/full_name")
            .and_then(Value::as_str)
        else {
            return Ok(());
        };
        let Some(number) = payload
            .pointer("/pull_request/number")
            .and_then(Value::as_u64)
        else {
            return Ok(());
        };

        match self
            .fetch_review_comments(&payload, full_name, number, token)
            .await
        {
            Ok(comments) => messages.extend(comments),
            Err(error) => {
                tracing::warn!(%full_name, number, %error, "failed to fetch review comments")
            }
        }
        Ok(())
    }
}

impl ForgejoAdapter {
    async fn find_review_reply_target(
        &self,
        message: &ForgeMessage,
    ) -> Result<Option<ReplyTarget>> {
        let (Some(token), Some(number), Some(comment_id)) =
            (self.token.as_deref(), message.number, message.comment_id)
        else {
            return Ok(None);
        };
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()?;
        let auth = format!("token {token}");
        let base = format!(
            "{}/api/v1/repos/{}/pulls/{number}/reviews",
            self.base_url, message.repository
        );

        for page in 1.. {
            let reviews: Vec<Value> = client
                .get(&base)
                .query(&[("limit", 100), ("page", page)])
                .header("Authorization", &auth)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            for review in reviews.iter().rev() {
                let Some(review_id) = review.get("id").and_then(Value::as_i64) else {
                    continue;
                };
                let comments_url = format!("{base}/{review_id}/comments");
                for comment_page in 1.. {
                    let comments: Vec<Value> = client
                        .get(&comments_url)
                        .query(&[("limit", 100), ("page", comment_page)])
                        .header("Authorization", &auth)
                        .send()
                        .await?
                        .error_for_status()?
                        .json()
                        .await?;
                    if let Some(comment) = comments.iter().find(|comment| {
                        comment.get("id").and_then(Value::as_i64) == Some(comment_id)
                    }) {
                        return Ok(review_reply_target_with_id(comment, Some(review_id)));
                    }
                    if comments.len() < 100 {
                        break;
                    }
                }
            }
            if reviews.len() < 100 {
                break;
            }
        }
        Ok(None)
    }

    /// Fetch the inline comments of the newest review on a pull request. The
    /// review is selected from the API because Forgejo's webhook payload does
    /// not identify it.
    async fn fetch_review_comments(
        &self,
        payload: &Value,
        full_name: &str,
        number: u64,
        token: &str,
    ) -> Result<Vec<ForgeMessage>> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()?;
        let auth = format!("token {token}");

        let reviews_url = format!(
            "{}/api/v1/repos/{full_name}/pulls/{number}/reviews",
            self.base_url
        );
        let reviews: Value = client
            .get(&reviews_url)
            .header("Authorization", &auth)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let Some(review_id) = reviews.as_array().and_then(|reviews| {
            reviews
                .iter()
                .filter_map(|review| review.get("id").and_then(Value::as_u64))
                .max()
        }) else {
            return Ok(Vec::new());
        };

        let comments_url = format!(
            "{}/api/v1/repos/{full_name}/pulls/{number}/reviews/{review_id}/comments",
            self.base_url
        );
        let comments: Value = client
            .get(&comments_url)
            .header("Authorization", &auth)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        Ok(comments
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter_map(|comment| {
                let body = comment.get("body").and_then(Value::as_str)?;
                review_message(payload, Some(comment), body)
            })
            .collect())
    }
}

/// Convert a review body or an inline review comment into a [`ForgeMessage`].
fn review_message(payload: &Value, comment: Option<&Value>, body: &str) -> Option<ForgeMessage> {
    let repository = payload
        .pointer("/repository/full_name")
        .and_then(Value::as_str)?;
    let pull_request = payload.get("pull_request")?;
    let number = pull_request.get("number").and_then(Value::as_u64);
    let author = comment
        .and_then(|comment| comment.pointer("/user/login"))
        .or_else(|| payload.pointer("/sender/login"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let comment_id = comment
        .and_then(|comment| comment.get("id"))
        .and_then(Value::as_i64);
    let location = comment
        .and_then(|comment| comment.get("html_url"))
        .and_then(Value::as_str)
        .and_then(|url| Url::parse(url).ok())
        .or_else(|| {
            pull_request
                .get("html_url")
                .and_then(Value::as_str)
                .and_then(|url| Url::parse(url).ok())
        })?;
    let title = pull_request
        .get("title")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let linked_issue = pull_request
        .get("body")
        .and_then(Value::as_str)
        .and_then(linked_issue_ref);

    // A review body has no code line to anchor to, so only inline comments get
    // a review-thread reply target.
    let reply_target = comment.and_then(review_reply_target).unwrap_or_default();

    Some(ForgeMessage {
        forge: ForgeKind::Forgejo,
        location,
        body: body.to_owned(),
        author,
        repository: repository.to_owned(),
        comment_id,
        number,
        is_pull_request: true,
        linked_issue,
        event: "pull_request_comment".into(),
        title,
        reply_target,
    })
}

/// Build the reply target for an inline review comment.
///
/// Forgejo exposes the new-side line as `position` and the old-side line as
/// `original_position`; exactly one of them is non-zero. It maps them onto a
/// signed line where the new side is positive and the old side is negative.
fn review_reply_target(comment: &Value) -> Option<ReplyTarget> {
    let review_id = comment
        .get("pull_request_review_id")
        .and_then(Value::as_i64);
    review_reply_target_with_id(comment, review_id)
}

/// Like [`review_reply_target`], but with an explicit review id so a caller can
/// fall back to the enclosing review when the comment omits it.
fn review_reply_target_with_id(comment: &Value, review_id: Option<i64>) -> Option<ReplyTarget> {
    let review_id = review_id?;
    let path = comment.get("path")?.as_str()?.to_owned();
    let new_position = comment.get("position").and_then(Value::as_i64).unwrap_or(0);
    let old_position = comment
        .get("original_position")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let line = if new_position > 0 {
        new_position
    } else if old_position > 0 {
        -old_position
    } else {
        0
    };
    let extra_lines_count = comment
        .get("extra_lines_count")
        .and_then(Value::as_i64)
        .unwrap_or(0);

    Some(ReplyTarget::ReviewComment(ReviewCommentTarget {
        review_id,
        path,
        line,
        extra_lines_count,
    }))
}

/// Patch parsed messages with the review thread they belong to.
///
/// Forgejo can inline an inline-review comment in the webhook payload (the
/// `pull_request_review_comment` event, or a reply delivered as an
/// `issue_comment`). The generic parser defaults those to the conversation,
/// which detaches the answer from the line under discussion; when the comment
/// carries review coordinates, re-target the message at that thread.
fn apply_review_reply_target(messages: &mut [ForgeMessage], body: &[u8]) {
    let Ok(payload) = serde_json::from_slice::<Value>(body) else {
        return;
    };
    let Some(comment) = payload.get("comment") else {
        return;
    };
    // Older payloads omit the review id on the comment but still carry the
    // review object; fall back to it rather than dropping the thread.
    let review_id = comment
        .get("pull_request_review_id")
        .and_then(Value::as_i64)
        .or_else(|| payload.pointer("/review/id").and_then(Value::as_i64));
    let Some(target) = review_reply_target_with_id(comment, review_id) else {
        return;
    };
    let comment_id = comment.get("id").and_then(Value::as_i64);
    for message in messages.iter_mut() {
        if message.comment_id == comment_id {
            message.reply_target = target.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::{IssueRef, hmac_sha256_hex};

    fn adapter() -> ForgejoAdapter {
        ForgejoAdapter::new(&ForgejoConfig {
            base_url: "http://forge.local:3000".into(),
            webhook_secret: Some("hush".into()),
            token: None,
            bot_username: Some("botty".into()),
            ..Default::default()
        })
    }

    fn headers(event: &str, sig: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("x-forgejo-event", event.parse().unwrap());
        if let Some(sig) = sig {
            h.insert("x-forgejo-signature", sig.parse().unwrap());
        }
        h
    }

    #[test]
    fn metadata_and_signature_handling() {
        let with_secret = adapter();
        assert_eq!(with_secret.base_url(), "http://forge.local:3000");
        assert_eq!(with_secret.bot_username(), Some("botty"));
        assert_eq!(with_secret.kind(), ForgeKind::Forgejo);
        assert_eq!(with_secret.slug(), "forgejo");

        // A missing signature is rejected.
        assert!(matches!(
            with_secret.verify(&HeaderMap::new(), b"body").unwrap_err(),
            BotError::Verification(_)
        ));
        let sig = hmac_sha256_hex(b"hush", b"body");
        // The Gitea header name is accepted as an alias.
        let mut gitea = HeaderMap::new();
        gitea.insert("x-gitea-signature", sig.parse().unwrap());
        assert!(with_secret.verify(&gitea, b"body").is_ok());
        // A wrong signature is rejected, a correct one accepted.
        let mut bad = headers("issue_comment", Some("deadbeef"));
        assert!(with_secret.verify(&bad, b"body").is_err());
        bad.insert("x-forgejo-signature", sig.parse().unwrap());
        assert!(with_secret.verify(&bad, b"body").is_ok());

        // No configured secret skips verification.
        let no_secret = ForgejoAdapter::new(&ForgejoConfig {
            base_url: "http://x/".into(),
            ..Default::default()
        });
        assert_eq!(no_secret.base_url(), "http://x");
        assert!(no_secret.verify(&HeaderMap::new(), b"body").is_ok());
    }

    const PAYLOAD: &str = r#"{
        "action": "created",
        "issue": {
            "number": 1,
            "title": "initial plan",
            "html_url": "http://forge.local:3000/shylock/forge-bot/issues/1"
        },
        "comment": {
            "id": 55,
            "body": "@agent do the thing",
            "html_url": "http://forge.local:3000/shylock/forge-bot/issues/1#issuecomment-55",
            "user": {"login": "shylock"}
        },
        "repository": {"full_name": "shylock/forge-bot"},
        "sender": {"login": "shylock"}
    }"#;

    #[test]
    fn verifies_and_parses() {
        let a = adapter();
        let body = PAYLOAD.as_bytes();
        let sig = hmac_sha256_hex(b"hush", body);
        let h = headers("issue_comment", Some(&sig));
        let msgs = a.handle(&h, body).unwrap();
        assert_eq!(msgs.len(), 1);
        let m = &msgs[0];
        assert_eq!(m.author, "shylock");
        assert_eq!(m.repository, "shylock/forge-bot");
        assert_eq!(m.number, Some(1));
        assert_eq!(m.comment_id, Some(55));
        assert!(!m.is_pull_request);
        assert_eq!(m.location.fragment(), Some("issuecomment-55"));
    }

    #[test]
    fn rejects_bad_signature() {
        let a = adapter();
        let h = headers("issue_comment", Some("nope"));
        assert!(a.handle(&h, PAYLOAD.as_bytes()).is_err());
    }

    #[test]
    fn ignores_non_comment_events() {
        let a = adapter();
        let body = PAYLOAD.as_bytes();
        let sig = hmac_sha256_hex(b"hush", body);
        let h = headers("push", Some(&sig));
        assert!(a.handle(&h, body).unwrap().is_empty());
    }

    #[test]
    fn ignores_edited_comments() {
        let a = adapter();
        let body = PAYLOAD
            .replace("\"action\": \"created\"", "\"action\": \"edited\"")
            .into_bytes();
        let sig = hmac_sha256_hex(b"hush", &body);
        let h = headers("issue_comment", Some(&sig));
        assert!(a.handle(&h, &body).unwrap().is_empty());
    }

    #[test]
    fn detects_pull_request() {
        let a = adapter();
        let raw = r#"{
            "action": "created",
            "issue": {"number": 3, "pull_request": {"url": "x"},
                      "html_url": "http://forge.local:3000/a/b/pulls/3"},
            "comment": {"id": 1, "body": "@agent x", "user": {"login": "u"}},
            "repository": {"full_name": "a/b"}
        }"#;
        let body = raw.as_bytes();
        let sig = hmac_sha256_hex(b"hush", body);
        let h = headers("issue_comment", Some(&sig));
        let m = &a.handle(&h, body).unwrap()[0];
        assert!(m.is_pull_request);
    }

    #[test]
    fn links_pull_request_to_issue_from_description() {
        let a = adapter();
        let raw = r#"{
            "action": "created",
            "issue": {
                "number": 12,
                "body": "This fixes #5 and adds tests.",
                "pull_request": {"url": "x"},
                "html_url": "http://forge.local:3000/a/b/pulls/12"
            },
            "comment": {"id": 3, "body": "@agent x", "user": {"login": "u"}},
            "repository": {"full_name": "a/b"}
        }"#;
        let body = raw.as_bytes();
        let sig = hmac_sha256_hex(b"hush", body);
        let h = headers("issue_comment", Some(&sig));
        let m = &a.handle(&h, body).unwrap()[0];
        assert!(m.is_pull_request);
        assert_eq!(m.number, Some(12));
        assert_eq!(
            m.linked_issue,
            Some(IssueRef {
                repository: None,
                number: 5,
            })
        );
    }

    #[test]
    fn links_pull_request_to_issue_in_another_repository() {
        let a = adapter();
        let raw = r#"{
            "action": "created",
            "issue": {
                "number": 12,
                "body": "This fixes other/repo#5 and adds tests.",
                "pull_request": {"url": "x"},
                "html_url": "http://forge.local:3000/a/b/pulls/12"
            },
            "comment": {"id": 3, "body": "@agent x", "user": {"login": "u"}},
            "repository": {"full_name": "a/b"}
        }"#;
        let body = raw.as_bytes();
        let sig = hmac_sha256_hex(b"hush", body);
        let h = headers("issue_comment", Some(&sig));
        let m = &a.handle(&h, body).unwrap()[0];
        assert_eq!(
            m.linked_issue,
            Some(IssueRef {
                repository: Some("other/repo".into()),
                number: 5,
            })
        );
    }

    #[test]
    fn parses_issue_description() {
        let a = adapter();
        let raw = r#"{
            "action": "opened",
            "issue": {
                "number": 4,
                "title": "broken build",
                "body": "@agent please look at this",
                "html_url": "http://forge.local:3000/a/b/issues/4",
                "user": {"login": "u"}
            },
            "repository": {"full_name": "a/b"}
        }"#;
        let body = raw.as_bytes();
        let sig = hmac_sha256_hex(b"hush", body);
        let h = headers("issues", Some(&sig));
        let msgs = a.handle(&h, body).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].number, Some(4));
        assert_eq!(msgs[0].comment_id, None);
        assert!(!msgs[0].is_pull_request);
        assert_eq!(msgs[0].body, "@agent please look at this");
        assert_eq!(msgs[0].title.as_deref(), Some("broken build"));
    }

    #[test]
    fn parses_pull_request_description() {
        let a = adapter();
        let raw = r#"{
            "action": "opened",
            "pull_request": {
                "number": 7,
                "title": "add feature",
                "body": "@agent review this",
                "html_url": "http://forge.local:3000/a/b/pulls/7",
                "user": {"login": "u"}
            },
            "repository": {"full_name": "a/b"}
        }"#;
        let body = raw.as_bytes();
        let sig = hmac_sha256_hex(b"hush", body);
        let h = headers("pull_request", Some(&sig));
        let msgs = a.handle(&h, body).unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].is_pull_request);
        assert_eq!(msgs[0].number, Some(7));
        assert_eq!(
            msgs[0].location.as_str(),
            "http://forge.local:3000/a/b/pulls/7"
        );
    }

    #[test]
    fn ignores_closed_description_events() {
        let a = adapter();
        let raw = r#"{
            "action": "closed",
            "issue": {"number": 4, "body": "@agent x",
                      "html_url": "http://forge.local:3000/a/b/issues/4"},
            "repository": {"full_name": "a/b"}
        }"#;
        let body = raw.as_bytes();
        let sig = hmac_sha256_hex(b"hush", body);
        let h = headers("issues", Some(&sig));
        assert!(a.handle(&h, body).unwrap().is_empty());
    }

    fn review_headers() -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("x-forgejo-event", "pull_request_comment".parse().unwrap());
        h
    }

    fn review_payload(content: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "action": "reviewed",
            "number": 16,
            "pull_request": {
                "number": 16,
                "title": "feat: something",
                "body": "This fixes #5.",
                "html_url": "http://forge.local:3000/a/b/pulls/16"
            },
            "review": {"type": "pull_request_review_comment", "content": content},
            "repository": {"full_name": "a/b"},
            "sender": {"login": "shylock"}
        }))
        .unwrap()
    }

    #[test]
    fn pull_request_comment_does_not_error_parsing() {
        let a = adapter();
        let body = review_payload("");
        let h = review_headers();
        assert!(a.parse(&h, &body).unwrap().is_empty());
    }

    #[tokio::test]
    async fn enriches_a_review_body_mention() {
        let a = adapter();
        let body = review_payload("@agent please look at this review");
        let mut messages = Vec::new();
        a.enrich(&mut messages, &review_headers(), &body)
            .await
            .unwrap();

        assert_eq!(messages.len(), 1);
        let message = &messages[0];
        assert_eq!(message.body, "@agent please look at this review");
        assert_eq!(message.author, "shylock");
        assert_eq!(message.number, Some(16));
        assert_eq!(
            message.linked_issue,
            Some(IssueRef {
                repository: None,
                number: 5
            })
        );
        assert!(message.is_pull_request);
        assert_eq!(message.comment_id, None);
        // A review body is not anchored to a code line.
        assert_eq!(message.reply_target, ReplyTarget::Conversation);
    }

    #[test]
    fn builds_message_from_an_inline_review_comment() {
        let payload = serde_json::from_slice::<Value>(&review_payload("")).unwrap();
        let comment = serde_json::json!({
            "id": 8991,
            "body": "@agent inline",
            "html_url": "http://forge.local:3000/a/b/pulls/16#issuecomment-8991",
            "user": {"login": "shylock"},
            "pull_request_review_id": 103,
            "path": "src/main.rs",
            "position": 30,
            "original_position": 0,
            "extra_lines_count": 2
        });
        let message = review_message(&payload, Some(&comment), "@agent inline").unwrap();
        assert_eq!(message.comment_id, Some(8991));
        assert_eq!(message.body, "@agent inline");
        assert_eq!(message.author, "shylock");
        assert_eq!(message.number, Some(16));
        assert_eq!(
            message.location.as_str(),
            "http://forge.local:3000/a/b/pulls/16#issuecomment-8991"
        );
        assert!(message.is_pull_request);
        // Inline comments carry the coordinates needed to answer in-thread.
        assert_eq!(
            message.reply_target,
            ReplyTarget::ReviewComment(ReviewCommentTarget {
                review_id: 103,
                path: "src/main.rs".into(),
                line: 30,
                extra_lines_count: 2,
            })
        );
    }

    #[test]
    fn inline_comment_on_the_old_side_uses_a_negative_line() {
        let payload = serde_json::from_slice::<Value>(&review_payload("")).unwrap();
        let comment = serde_json::json!({
            "id": 8992,
            "body": "@agent old",
            "html_url": "http://forge.local:3000/a/b/pulls/16#issuecomment-8992",
            "user": {"login": "shylock"},
            "pull_request_review_id": 103,
            "path": "src/main.rs",
            "position": 0,
            "original_position": 12
        });
        let message = review_message(&payload, Some(&comment), "@agent old").unwrap();
        assert_eq!(
            message.reply_target,
            ReplyTarget::ReviewComment(ReviewCommentTarget {
                review_id: 103,
                path: "src/main.rs".into(),
                line: -12,
                extra_lines_count: 0,
            })
        );
    }

    #[test]
    fn inlined_review_comment_reply_keeps_its_thread() {
        let a = adapter();
        let payload = serde_json::json!({
            "action": "created",
            "number": 16,
            "pull_request": {
                "number": 16,
                "title": "feat: something",
                "body": "This fixes #5.",
                "html_url": "http://forge.local:3000/a/b/pulls/16"
            },
            "comment": {
                "id": 9093,
                "body": "@agent why?",
                "html_url": "http://forge.local:3000/a/b/pulls/16#issuecomment-9093",
                "user": {"login": "shylock"},
                "pull_request_review_id": 111,
                "path": "src/agent/command.rs",
                "position": 161,
                "original_position": 0,
                "extra_lines_count": 1
            },
            "repository": {"full_name": "a/b"},
            "sender": {"login": "shylock"}
        });
        let body = serde_json::to_vec(&payload).unwrap();
        let messages = a
            .parse(&headers("pull_request_review_comment", None), &body)
            .unwrap();

        assert_eq!(messages.len(), 1);
        let message = &messages[0];
        assert_eq!(message.comment_id, Some(9093));
        assert!(message.is_pull_request);
        assert_eq!(
            message.reply_target,
            ReplyTarget::ReviewComment(ReviewCommentTarget {
                review_id: 111,
                path: "src/agent/command.rs".into(),
                line: 161,
                extra_lines_count: 1,
            })
        );
    }

    #[test]
    fn inlined_review_comment_falls_back_to_the_review_id() {
        let a = adapter();
        let payload = serde_json::json!({
            "action": "created",
            "number": 16,
            "pull_request": {
                "number": 16,
                "html_url": "http://forge.local:3000/a/b/pulls/16"
            },
            "review": {"id": 111},
            "comment": {
                "id": 9094,
                "body": "@agent reply",
                "html_url": "http://forge.local:3000/a/b/pulls/16#issuecomment-9094",
                "user": {"login": "shylock"},
                "path": "src/agent/command.rs",
                "position": 0,
                "original_position": 7
            },
            "repository": {"full_name": "a/b"},
            "sender": {"login": "shylock"}
        });
        let body = serde_json::to_vec(&payload).unwrap();
        let messages = a.parse(&headers("issue_comment", None), &body).unwrap();

        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].reply_target,
            ReplyTarget::ReviewComment(ReviewCommentTarget {
                review_id: 111,
                path: "src/agent/command.rs".into(),
                line: -7,
                extra_lines_count: 0,
            })
        );
    }

    #[tokio::test]
    async fn fetches_inline_review_comments_from_the_api() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buffer = [0u8; 4096];
                let read = socket.read(&mut buffer).await.unwrap();
                let request = String::from_utf8_lossy(&buffer[..read]);
                let body = if request.contains("/comments") {
                    r#"[{"id":8991,"body":"@agent inline","html_url":"http://forge.local/a/b/pulls/16#issuecomment-8991","user":{"login":"shylock"},"pull_request_review_id":103,"path":"src/main.rs","position":30,"original_position":0}]"#
                } else if request.contains("/pulls/16/reviews") {
                    r#"[{"id":100,"comments_count":1},{"id":102,"comments_count":1}]"#
                } else {
                    "[]"
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });

        let a = ForgejoAdapter::new(&ForgejoConfig {
            base_url: format!("http://{addr}"),
            webhook_secret: None,
            token: Some("test-token".into()),
            bot_username: None,
            ..Default::default()
        });
        let body = review_payload("");
        let mut messages = Vec::new();
        a.enrich(&mut messages, &review_headers(), &body)
            .await
            .unwrap();
        server.await.unwrap();

        assert_eq!(messages.len(), 1);
        let message = &messages[0];
        assert_eq!(message.comment_id, Some(8991));
        assert_eq!(message.body, "@agent inline");
        assert_eq!(message.author, "shylock");
        assert_eq!(message.number, Some(16));
        assert!(message.is_pull_request);
        assert_eq!(
            message.reply_target,
            ReplyTarget::ReviewComment(ReviewCommentTarget {
                review_id: 103,
                path: "src/main.rs".into(),
                line: 30,
                extra_lines_count: 0,
            })
        );
    }

    #[tokio::test]
    async fn resolves_issue_comment_reply_to_an_older_review_thread() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..3 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buffer = [0u8; 4096];
                let read = socket.read(&mut buffer).await.unwrap();
                let request = String::from_utf8_lossy(&buffer[..read]);
                let body = if request.contains("/reviews/121/comments") {
                    "[]"
                } else if request.contains("/reviews/120/comments") {
                    r#"[{"id":9528,"path":"src/agent/command.rs","position":301,"original_position":0}]"#
                } else {
                    r#"[{"id":120},{"id":121}]"#
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });

        let a = ForgejoAdapter::new(&ForgejoConfig {
            base_url: format!("http://{addr}"),
            token: Some("test-token".into()),
            ..Default::default()
        });
        let payload = serde_json::json!({
            "action": "created",
            "issue": {"number": 54, "pull_request": {"url": "x"},
                      "html_url": "http://forge.local/a/b/pulls/54"},
            "comment": {"id": 9528, "body": "@agent avoid duplication",
                        "html_url": "http://forge.local/a/b/pulls/54#issuecomment-9528",
                        "user": {"login": "shylock"}},
            "repository": {"full_name": "a/b"}
        });
        let body = serde_json::to_vec(&payload).unwrap();
        let headers = headers("issue_comment", None);
        let mut messages = a.parse(&headers, &body).unwrap();
        assert_eq!(messages[0].reply_target, ReplyTarget::Conversation);
        a.enrich(&mut messages, &headers, &body).await.unwrap();
        server.await.unwrap();
        assert_eq!(
            messages[0].reply_target,
            ReplyTarget::ReviewComment(ReviewCommentTarget {
                review_id: 120,
                path: "src/agent/command.rs".into(),
                line: 301,
                extra_lines_count: 0,
            })
        );
    }
}

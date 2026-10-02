//! Forge integrations.
//!
//! Every forge implements [`ForgeAdapter`]. The adapter is responsible for
//! verifying a webhook and normalizing its payload into a [`ForgeMessage`].
//! Everything downstream (mention detection, policy, agent selection) is forge
//! agnostic.

pub mod forgejo;
pub mod github;
pub mod gitlab;

use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::Result;
use crate::location::ForgeKind;

/// A reference to an issue or pull request.
///
/// The repository is optional: `None` means "the same repository as the
/// message that carried the reference", while `Some("owner/repo")` is an
/// explicit cross-repository reference (for example `Fixes other/repo#7`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueRef {
    /// `owner/repo`, when the reference names one explicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    /// Issue or pull request number.
    pub number: u64,
}

/// Where a reply to an incoming comment should be posted.
///
/// Most mentions are answered in the issue / pull-request conversation. An
/// inline code-review comment, however, forms its own thread, and answering it
/// with a top-level conversation comment detaches the answer from the line it
/// discusses. The reply target records enough forge-agnostic detail to post the
/// answer back into the same thread.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReplyTarget {
    /// A normal conversation comment on the issue or pull request.
    #[default]
    Conversation,
    /// A thread of inline comments attached to a code line of a review.
    ReviewComment(ReviewCommentTarget),
}

/// Identity of an inline review thread.
///
/// Forgejo / Gitea group inline comments by the review they belong to plus the
/// file path and line position, so replying means creating a comment with the
/// same coordinates. `line` follows Forgejo's convention: a positive value
/// anchors the comment on the new side of the diff, a negative value on the old
/// side, and zero is unset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewCommentTarget {
    /// Id of the review the comment belongs to.
    pub review_id: i64,
    /// Path of the commented file, relative to the repository root.
    pub path: String,
    /// Line of the diff the comment is anchored to.
    #[serde(default)]
    pub line: i64,
    /// Number of additional lines the comment spans.
    #[serde(default)]
    pub extra_lines_count: i64,
}

/// A normalized comment / issue event received from a forge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgeMessage {
    pub forge: ForgeKind,
    /// Canonical location of the object the comment belongs to.
    pub location: Url,
    /// Raw comment body (mention extraction happens later).
    pub body: String,
    /// Login of the comment author.
    pub author: String,
    /// `owner/repo`.
    pub repository: String,
    /// Id of the comment, when available.
    pub comment_id: Option<i64>,
    /// Issue / pull request number, when available.
    pub number: Option<u64>,
    /// True when the comment belongs to a pull request.
    pub is_pull_request: bool,
    /// Issue the pull request closes / fixes / resolves, when the description
    /// references one. This is internal routing metadata: it keeps a pull
    /// request and its issue on the same conversation, and is never sent to
    /// the agent.
    #[serde(default)]
    pub linked_issue: Option<IssueRef>,
    /// The forge event name, e.g. `issue_comment`.
    pub event: String,
    /// Title of the issue / pull request, used to enrich the agent prompt.
    pub title: Option<String>,
    /// Where the reply to this message should be posted. Defaults to the
    /// issue / pull-request conversation.
    #[serde(default)]
    pub reply_target: ReplyTarget,
}

impl ForgeMessage {
    /// Stable key identifying the conversation (thread) this message belongs
    /// to. Pull requests are folded onto their linked issue when one is known,
    /// so a pull request and the issue it closes share an agent/session. The
    /// key never leaves the gateway.
    pub fn conversation_key(&self) -> String {
        // A pull request folds onto the issue it closes; when that issue lives
        // in another repository, use *its* owner/repo so both threads share a
        // key instead of colliding with a same-numbered issue here.
        let (repository, number) = if self.is_pull_request {
            match &self.linked_issue {
                Some(linked) => (
                    linked.repository.as_deref().unwrap_or(&self.repository),
                    Some(linked.number),
                ),
                None => (self.repository.as_str(), self.number),
            }
        } else {
            (self.repository.as_str(), self.number)
        };
        format!(
            "{}:{}:{}",
            self.forge,
            repository,
            number.unwrap_or_default()
        )
    }

    /// Repository identifier without the owner.
    pub fn repo_name(&self) -> &str {
        self.repository
            .split_once('/')
            .map(|(_, r)| r)
            .unwrap_or(&self.repository)
    }

    /// Owner part of the repository identifier.
    pub fn owner(&self) -> &str {
        self.repository
            .split_once('/')
            .map(|(o, _)| o)
            .unwrap_or_default()
    }
}

/// A forge webhook adapter.
#[async_trait::async_trait]
pub trait ForgeAdapter: Send + Sync {
    /// The forge this adapter serves.
    fn kind(&self) -> ForgeKind;

    /// Short name used in the webhook route, e.g. `forgejo`.
    fn slug(&self) -> &'static str;

    /// Verify the request signature. Implementations must use a constant time
    /// comparison and must not leak the expected value.
    fn verify(&self, headers: &HeaderMap, body: &[u8]) -> Result<()>;

    /// Extract the event name from the request headers.
    fn event(&self, headers: &HeaderMap) -> String;

    /// Parse a verified payload into zero or more messages.
    ///
    /// Returning an empty vector means "this event is not actionable".
    fn parse(&self, headers: &HeaderMap, body: &[u8]) -> Result<Vec<ForgeMessage>>;

    /// Convenience helper combining verification and parsing.
    fn handle(&self, headers: &HeaderMap, body: &[u8]) -> Result<Vec<ForgeMessage>> {
        self.verify(headers, body)?;
        self.parse(headers, body)
    }

    /// Add messages that the raw delivery does not carry.
    ///
    /// A forge may omit part of an event from the webhook payload. Forgejo, for
    /// example, signals a pull-request review with `pull_request_comment` but
    /// does not inline the review's comments, so the only way to see an inline
    /// review comment mention is to fetch it through the API. The default
    /// implementation adds nothing.
    async fn enrich(
        &self,
        _messages: &mut Vec<ForgeMessage>,
        _headers: &HeaderMap,
        _body: &[u8],
    ) -> Result<()> {
        Ok(())
    }
}

/// Compute the lowercase hex HMAC-SHA256 of `body` using `secret`.
pub fn hmac_sha256_hex(secret: &[u8], body: &[u8]) -> String {
    use hmac::{KeyInit, Mac};

    let mut mac =
        hmac::Hmac::<sha2::Sha256>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

/// Constant-time equality for two byte slices.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Verify an HMAC-SHA256 hex signature header, tolerating the optional
/// `sha256=` prefix used by GitHub.
pub fn verify_hmac_sha256(secret: &[u8], body: &[u8], provided: &str) -> bool {
    let expected = hmac_sha256_hex(secret, body);
    let provided = provided.strip_prefix("sha256=").unwrap_or(provided);
    constant_time_eq(expected.as_bytes(), provided.trim().as_bytes())
}

/// Shared parser for the `issue_comment` shaped payload used by Forgejo, Gitea
/// and GitHub. It normalizes the payload into a [`ForgeMessage`].
pub(crate) fn parse_comment_payload(
    forge: ForgeKind,
    base_url: &str,
    body: &[u8],
    event: &str,
) -> Result<Vec<ForgeMessage>> {
    use serde_json::Value;

    let payload: Value = serde_json::from_slice(body)?;

    let action = payload
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("created");
    if !action.is_empty() && action != "created" {
        return Ok(Vec::new());
    }

    let comment = payload
        .get("comment")
        .ok_or_else(|| crate::error::BotError::InvalidPayload("missing `comment` object".into()))?;
    let Some(body_text) = comment.get("body").and_then(Value::as_str) else {
        return Ok(Vec::new());
    };

    let issue = payload.get("issue").or_else(|| payload.get("pull_request"));
    let repository = payload.get("repository").ok_or_else(|| {
        crate::error::BotError::InvalidPayload("missing `repository` object".into())
    })?;

    let full_name = repository
        .get("full_name")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            crate::error::BotError::InvalidPayload("missing repository.full_name".into())
        })?;

    let author = comment
        .get("user")
        .or_else(|| payload.get("sender"))
        .and_then(|u| u.get("login"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();

    let is_pull_request = issue
        .and_then(|i| i.get("pull_request"))
        .map(|v| !v.is_null())
        .unwrap_or_else(|| event.contains("pull_request"));

    let number = issue
        .and_then(|i| i.get("number"))
        .and_then(Value::as_u64)
        .or_else(|| payload.get("number").and_then(Value::as_u64));

    let comment_id = comment.get("id").and_then(Value::as_i64);
    let title = issue
        .and_then(|i| i.get("title"))
        .and_then(Value::as_str)
        .map(str::to_owned);

    let linked_issue = if is_pull_request {
        issue
            .and_then(|i| i.get("body"))
            .and_then(Value::as_str)
            .and_then(linked_issue_ref)
    } else {
        None
    };

    let location = build_location(
        base_url,
        repository,
        issue,
        comment,
        full_name,
        number,
        is_pull_request,
    )?;

    Ok(vec![ForgeMessage {
        forge,
        location,
        body: body_text.to_owned(),
        author,
        repository: full_name.to_owned(),
        comment_id,
        number,
        is_pull_request,
        linked_issue,
        event: event.to_owned(),
        title,
        reply_target: ReplyTarget::Conversation,
    }])
}

/// Parse an `issues` / `pull_request` payload where the mention is in the
/// issue or pull-request description rather than a comment.
pub(crate) fn parse_description_payload(
    forge: ForgeKind,
    base_url: &str,
    body: &[u8],
    event: &str,
) -> Result<Vec<ForgeMessage>> {
    use serde_json::Value;

    let payload: Value = serde_json::from_slice(body)?;
    let action = payload
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("opened");
    // Only react to creation/edits; ignore close, label, milestone, ...
    if !matches!(action, "opened" | "created" | "edited" | "") {
        return Ok(Vec::new());
    }

    let object = payload.get("issue").or_else(|| payload.get("pull_request"));
    let Some(object) = object else {
        return Ok(Vec::new());
    };
    let Some(body_text) = object.get("body").and_then(Value::as_str) else {
        return Ok(Vec::new());
    };
    if body_text.trim().is_empty() {
        return Ok(Vec::new());
    }

    let repository = payload.get("repository").ok_or_else(|| {
        crate::error::BotError::InvalidPayload("missing `repository` object".into())
    })?;
    let full_name = repository
        .get("full_name")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            crate::error::BotError::InvalidPayload("missing repository.full_name".into())
        })?;

    let is_pull_request = payload.get("pull_request").is_some() || event.contains("pull_request");
    let actor = if action == "edited" {
        payload.get("sender")
    } else {
        object.get("user").or_else(|| payload.get("sender"))
    };
    let author = actor
        .and_then(|u| u.get("login"))
        .and_then(Value::as_str)
        .filter(|login| !login.trim().is_empty())
        .ok_or_else(|| {
            crate::error::BotError::InvalidPayload("missing description event actor".into())
        })?
        .to_owned();
    let number = object.get("number").and_then(Value::as_u64);
    let title = object
        .get("title")
        .and_then(Value::as_str)
        .map(str::to_owned);

    let linked_issue = if is_pull_request {
        linked_issue_ref(body_text)
    } else {
        None
    };

    let location = description_location(
        base_url,
        repository,
        object,
        full_name,
        number,
        is_pull_request,
    )?;

    Ok(vec![ForgeMessage {
        forge,
        location,
        body: body_text.to_owned(),
        author,
        repository: full_name.to_owned(),
        comment_id: None,
        number,
        is_pull_request,
        linked_issue,
        event: event.to_owned(),
        title,
        reply_target: ReplyTarget::Conversation,
    }])
}

/// Find the first issue referenced with a closing keyword
/// (`close(s|d)`, `fix(es|ed)`, `resolve(s|d)`) in a description.
///
/// Forgejo, GitHub and GitLab all recognize those keywords to link a pull
/// request to the issue it addresses. The reference may be `#123` (same
/// repository) or `owner/repo#123` (another repository); keeping the
/// owner/repo lets a pull request and the issue it closes share one
/// conversation even across repositories.
pub(crate) fn linked_issue_ref(body: &str) -> Option<IssueRef> {
    // Longest forms first so `closes` is not shadowed by `close`.
    const KEYWORDS: [&str; 9] = [
        "closes", "closed", "close", "fixes", "fixed", "fix", "resolves", "resolved", "resolve",
    ];

    let lower = body.to_ascii_lowercase();
    // Track the earliest reference in the text so `Fixes #3, closes #4`
    // resolves to 3 regardless of the order keywords are checked.
    let mut best: Option<(usize, IssueRef)> = None;
    for keyword in KEYWORDS {
        let mut from = 0;
        while let Some(offset) = lower[from..].find(keyword) {
            let start = from + offset;
            let end = start + keyword.len();
            from = end;

            // Require a word boundary on both sides of the keyword so neither
            // `closely` nor `fixedly` is mistaken for a closing keyword.
            if start > 0 && is_word_byte(lower.as_bytes()[start - 1]) {
                continue;
            }
            if lower
                .as_bytes()
                .get(end)
                .is_some_and(|byte| is_word_byte(*byte))
            {
                continue;
            }

            let mut rest = lower[end..].trim_start();
            rest = rest.strip_prefix(':').unwrap_or(rest).trim_start();

            // The reference is the next whitespace-delimited token, shaped like
            // `[owner/repo]#123`.
            let token = rest.split_whitespace().next().unwrap_or_default();
            let Some((repo_part, num_part)) = token.split_once('#') else {
                continue;
            };
            // Trim surrounding punctuation, then require a plausible
            // `owner/repo` qualifier (reject URLs, bare words, ...).
            let repo_part = repo_part.trim_matches(|c: char| !is_repo_char(c));
            if !repo_part.is_empty()
                && (!repo_part.contains('/') || !repo_part.chars().all(is_repo_char))
            {
                continue;
            }
            let digits: String = num_part.chars().take_while(char::is_ascii_digit).collect();
            let Ok(number) = digits.parse::<u64>() else {
                continue;
            };
            if number == 0 {
                continue;
            }

            let reference = IssueRef {
                repository: if repo_part.is_empty() {
                    None
                } else {
                    Some(repo_part.to_owned())
                },
                number,
            };
            if best.as_ref().is_none_or(|(seen, _)| start < *seen) {
                best = Some((start, reference));
            }
            // The first valid reference for this keyword wins.
            break;
        }
    }
    best.map(|(_, reference)| reference)
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn is_repo_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/')
}

fn description_location(
    base_url: &str,
    repository: &serde_json::Value,
    object: &serde_json::Value,
    full_name: &str,
    number: Option<u64>,
    is_pull_request: bool,
) -> Result<Url> {
    if let Some(url) = object
        .get("html_url")
        .and_then(serde_json::Value::as_str)
        .and_then(|s| Url::parse(s).ok())
    {
        return Ok(url);
    }

    let base = repository
        .get("html_url")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{base_url}/{full_name}"));
    let resource = if is_pull_request { "pulls" } else { "issues" };
    Url::parse(&format!("{base}/{resource}/{}", number.unwrap_or_default())).map_err(|e| {
        crate::error::BotError::InvalidLocation {
            location: full_name.to_owned(),
            reason: e.to_string(),
        }
    })
}

/// Build the canonical location URL for a comment.
fn build_location(
    base_url: &str,
    repository: &serde_json::Value,
    issue: Option<&serde_json::Value>,
    comment: &serde_json::Value,
    full_name: &str,
    number: Option<u64>,
    is_pull_request: bool,
) -> Result<Url> {
    if let Some(url) = comment
        .get("html_url")
        .and_then(serde_json::Value::as_str)
        .and_then(|s| Url::parse(s).ok())
    {
        return Ok(url);
    }

    if let Some(url) = issue
        .and_then(|i| i.get("html_url"))
        .and_then(serde_json::Value::as_str)
        .and_then(|s| Url::parse(s).ok())
    {
        return Ok(url);
    }

    let repo_url = repository
        .get("html_url")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{base_url}/{full_name}"));
    let resource = if is_pull_request { "pulls" } else { "issues" };
    let number = number.unwrap_or_default();
    Url::parse(&format!("{repo_url}/{resource}/{number}")).map_err(|e| {
        crate::error::BotError::InvalidLocation {
            location: repo_url,
            reason: e.to_string(),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn description_edits_authorize_editor_and_require_identity() {
        let policy = crate::policy::Policy::new(&crate::config::PolicyConfig {
            allowed_users: vec!["trusted".into()],
            ..Default::default()
        });
        for (key, event) in [("issue", "issues"), ("pull_request", "pull_request")] {
            let mut payload = serde_json::json!({
                "action": "edited", "sender": {"login": "untrusted-editor"},
                "repository": {"full_name": "o/r"},
                key: {"number": 1, "body": "@agent do work", "user": {"login": "trusted"}}
            });
            let messages = parse_description_payload(
                ForgeKind::Forgejo,
                "https://forge.test",
                &serde_json::to_vec(&payload).unwrap(),
                event,
            )
            .unwrap();
            assert_eq!(messages[0].author, "untrusted-editor");
            assert!(policy.authorize(&messages[0]).is_err());
            for sender in [
                serde_json::Value::Null,
                serde_json::json!({}),
                serde_json::json!({"login": ""}),
            ] {
                payload["sender"] = sender;
                assert!(
                    parse_description_payload(
                        ForgeKind::Forgejo,
                        "https://forge.test",
                        &serde_json::to_vec(&payload).unwrap(),
                        event
                    )
                    .is_err()
                );
            }
            payload["sender"] = serde_json::json!({"login": "trusted"});
            payload[key]["user"]["login"] = serde_json::json!("untrusted-author");
            let messages = parse_description_payload(
                ForgeKind::Forgejo,
                "https://forge.test",
                &serde_json::to_vec(&payload).unwrap(),
                event,
            )
            .unwrap();
            assert!(policy.authorize(&messages[0]).is_ok());
        }
    }

    #[test]
    fn hmac_matches_known_vector() {
        // RFC 4231 test case 1.
        let key = [0x0b; 20];
        let data = b"Hi There";
        assert_eq!(
            hmac_sha256_hex(&key, data),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn extracts_linked_issue_from_closing_keywords() {
        fn same(number: u64) -> Option<IssueRef> {
            Some(IssueRef {
                repository: None,
                number,
            })
        }

        assert_eq!(linked_issue_ref("Fixes #42"), same(42));
        assert_eq!(linked_issue_ref("closes: #7"), same(7));
        assert_eq!(linked_issue_ref("Resolved #100 and more"), same(100));
        assert_eq!(linked_issue_ref("This does not close anything"), None);
        // `closely` / `fixedly` must not be mistaken for closing keywords.
        assert_eq!(linked_issue_ref("closely #3"), None);
        assert_eq!(linked_issue_ref("fixedly #3"), None);
        // The first keyword wins.
        assert_eq!(linked_issue_ref("Fixes #3, closes #4"), same(3));
        // A repository-qualified reference keeps its owner/repo so a
        // cross-repository issue can be linked, not just its number.
        assert_eq!(
            linked_issue_ref("Fixes other/repo#8"),
            Some(IssueRef {
                repository: Some("other/repo".into()),
                number: 8,
            })
        );
        assert_eq!(
            linked_issue_ref("closes group/sub/proj#9."),
            Some(IssueRef {
                repository: Some("group/sub/proj".into()),
                number: 9,
            })
        );
        // Surrounding punctuation is ignored.
        assert_eq!(linked_issue_ref("fixes (#11)"), same(11));
        // A URL fragment is not a closing reference.
        assert_eq!(linked_issue_ref("fixes https://example.com/page#12"), None);
    }

    #[test]
    fn conversation_key_uses_linked_issue_repository() {
        let mut message = ForgeMessage {
            forge: ForgeKind::Forgejo,
            location: Url::parse("http://forge.local/a/b/issues/12").unwrap(),
            body: String::new(),
            author: "u".into(),
            repository: "a/b".into(),
            comment_id: None,
            number: Some(12),
            is_pull_request: true,
            linked_issue: Some(IssueRef {
                repository: Some("other/repo".into()),
                number: 5,
            }),
            event: "issue_comment".into(),
            title: None,
            reply_target: ReplyTarget::Conversation,
        };
        assert_eq!(message.conversation_key(), "forgejo:other/repo:5");

        message.linked_issue = Some(IssueRef {
            repository: None,
            number: 5,
        });
        assert_eq!(message.conversation_key(), "forgejo:a/b:5");
    }

    #[test]
    fn reply_target_round_trips_and_defaults_to_conversation() {
        let target = ReplyTarget::ReviewComment(ReviewCommentTarget {
            review_id: 7,
            path: "src/lib.rs".into(),
            line: -3,
            extra_lines_count: 1,
        });
        let json = serde_json::to_string(&target).unwrap();
        assert!(json.contains("\"kind\":\"review_comment\""));
        assert_eq!(serde_json::from_str::<ReplyTarget>(&json).unwrap(), target);

        // A job persisted before the field existed must still load.
        let message: ForgeMessage = serde_json::from_str(
            r#"{"forge":"forgejo","location":"http://forge.local/a/b/issues/1",
                "body":"x","author":"u","repository":"a/b","comment_id":null,
                "number":1,"is_pull_request":false,"event":"issues","title":null}"#,
        )
        .unwrap();
        assert_eq!(message.reply_target, ReplyTarget::Conversation);
    }

    #[test]
    fn verify_accepts_prefix_and_rejects_tampering() {
        let secret = b"topsecret";
        let body = br#"{"action":"created"}"#;
        let sig = hmac_sha256_hex(secret, body);
        assert!(verify_hmac_sha256(secret, body, &sig));
        assert!(verify_hmac_sha256(secret, body, &format!("sha256={sig}")));
        assert!(!verify_hmac_sha256(secret, body, "deadbeef"));
        assert!(!verify_hmac_sha256(secret, b"other", &sig));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"abc", b"abc"));
    }

    fn comment_body(payload: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&payload).unwrap()
    }

    #[test]
    fn parse_comment_payload_handles_location_fallbacks() {
        // The comment html_url wins.
        let body = comment_body(serde_json::json!({
            "action": "created",
            "issue": {"number": 4, "title": "t", "html_url": "http://f/a/b/issues/4"},
            "comment": {"id": 9, "body": "@agent hi", "html_url": "http://f/a/b/issues/4#issuecomment-9", "user": {"login": "alice"}},
            "repository": {"full_name": "a/b"}
        }));
        let messages = parse_comment_payload(
            ForgeKind::Forgejo,
            "http://fallback",
            &body,
            "issue_comment",
        )
        .unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].location.as_str(),
            "http://f/a/b/issues/4#issuecomment-9"
        );
        assert_eq!(messages[0].author, "alice");
        assert_eq!(messages[0].number, Some(4));
        assert!(!messages[0].is_pull_request);

        // No comment html_url: fall back to the issue html_url.
        let body = comment_body(serde_json::json!({
            "issue": {"number": 4, "html_url": "http://f/a/b/issues/4"},
            "comment": {"id": 9, "body": "@agent hi"},
            "repository": {"full_name": "a/b"}
        }));
        let messages = parse_comment_payload(
            ForgeKind::Forgejo,
            "http://fallback",
            &body,
            "issue_comment",
        )
        .unwrap();
        assert_eq!(messages[0].location.as_str(), "http://f/a/b/issues/4");

        // Only a broken html_url: build from the repository html_url.
        let body = comment_body(serde_json::json!({
            "issue": {"number": 4, "html_url": "not a url"},
            "comment": {"id": 9, "body": "@agent hi", "html_url": "also bad"},
            "repository": {"full_name": "a/b", "html_url": "http://f/a/b"}
        }));
        let messages = parse_comment_payload(
            ForgeKind::Forgejo,
            "http://fallback",
            &body,
            "issue_comment",
        )
        .unwrap();
        assert_eq!(messages[0].location.as_str(), "http://f/a/b/issues/4");

        // No repository html_url: build from base_url and full_name.
        let body = comment_body(serde_json::json!({
            "issue": {"number": 4},
            "comment": {"id": 9, "body": "@agent hi"},
            "repository": {"full_name": "a/b"}
        }));
        let messages =
            parse_comment_payload(ForgeKind::Forgejo, "http://base", &body, "issue_comment")
                .unwrap();
        assert_eq!(messages[0].location.as_str(), "http://base/a/b/issues/4");
    }

    #[test]
    fn parse_comment_payload_handles_pull_request_shape() {
        let body = comment_body(serde_json::json!({
            "action": "created",
            "pull_request": {"number": 5, "title": "pr", "body": "Fixes #2", "user": {"login": "bob"}},
            "comment": {"id": 1, "body": "@agent go", "html_url": "http://f/a/b/pulls/5#issuecomment-1", "user": {"login": "bob"}},
            "repository": {"full_name": "a/b"}
        }));
        let messages =
            parse_comment_payload(ForgeKind::GitHub, "http://f", &body, "pull_request_comment")
                .unwrap();
        assert!(messages[0].is_pull_request);
        assert_eq!(messages[0].author, "bob");
        assert_eq!(messages[0].comment_id, Some(1));
        assert_eq!(messages[0].linked_issue.as_ref().unwrap().number, 2);
        assert_eq!(messages[0].title.as_deref(), Some("pr"));
    }

    #[test]
    fn parse_comment_payload_ignores_or_rejects_bad_shapes() {
        let cases: Vec<(&[u8], bool)> = vec![
            (
                br#"{"action":"deleted","comment":{},"repository":{"full_name":"a/b"}}"#,
                true,
            ),
            (br#"{"repository":{"full_name":"a/b"}}"#, false),
            (
                br#"{"comment":{"id":1},"repository":{"full_name":"a/b"}}"#,
                true,
            ),
            (br#"{"comment":{"id":1,"body":"x"}}"#, false),
            (br#"{"comment":{"id":1,"body":"x"},"repository":{}}"#, false),
            (b"not json", false),
        ];
        for (body, should_be_empty) in cases {
            let result =
                parse_comment_payload(ForgeKind::Forgejo, "http://f", body, "issue_comment");
            if should_be_empty {
                assert!(
                    result.unwrap().is_empty(),
                    "body: {}",
                    String::from_utf8_lossy(body)
                );
            } else {
                assert!(result.is_err(), "body: {}", String::from_utf8_lossy(body));
            }
        }
    }

    #[test]
    fn parse_description_payload_handles_shapes() {
        // A pull-request description builds a PR location and links its issue.
        let body = comment_body(serde_json::json!({
            "action": "opened",
            "pull_request": {"number": 5, "title": "pr", "body": "Closes #12", "user": {"login": "bob"}},
            "repository": {"full_name": "a/b", "html_url": "http://f/a/b"}
        }));
        let messages =
            parse_description_payload(ForgeKind::GitHub, "http://f", &body, "pull_request")
                .unwrap();
        assert_eq!(messages.len(), 1);
        assert!(messages[0].is_pull_request);
        assert_eq!(messages[0].location.as_str(), "http://f/a/b/pulls/5");
        assert_eq!(messages[0].linked_issue.as_ref().unwrap().number, 12);
        assert_eq!(messages[0].comment_id, None);

        // An issue description uses the issues resource.
        let body = comment_body(serde_json::json!({
            "action": "edited",
            "sender": {"login": "editor"},
            "issue": {"number": 7, "body": "hello", "html_url": "http://f/a/b/issues/7"},
            "repository": {"full_name": "a/b"}
        }));
        let messages =
            parse_description_payload(ForgeKind::GitHub, "http://f", &body, "issues").unwrap();
        assert!(!messages[0].is_pull_request);
        assert_eq!(messages[0].linked_issue, None);
        assert_eq!(messages[0].location.as_str(), "http://f/a/b/issues/7");

        // Non-actionable / incomplete payloads are dropped.
        for raw in [
            serde_json::json!({"action": "closed", "issue": {"body": "x"}}),
            serde_json::json!({"action": "opened", "repository": {"full_name": "a/b"}}),
            serde_json::json!({"action": "opened", "issue": {"body": "   "}, "repository": {"full_name": "a/b"}}),
        ] {
            let raw = serde_json::to_vec(&raw).unwrap();
            assert!(
                parse_description_payload(ForgeKind::GitHub, "http://f", &raw, "issues")
                    .unwrap()
                    .is_empty()
            );
        }

        // A missing repository is a hard error.
        let raw = br#"{"issue":{"body":"x"}}"#;
        assert!(parse_description_payload(ForgeKind::GitHub, "http://f", raw, "issues").is_err());
        let raw = br#"{"issue":{"body":"x"},"repository":{}}"#;
        assert!(parse_description_payload(ForgeKind::GitHub, "http://f", raw, "issues").is_err());
    }
}

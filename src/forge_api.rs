//! Posting comments back to a forge.
//!
//! The gateway uses this for acknowledgements and result summaries. Agents may
//! also post comments themselves using the credentials exported into their
//! environment.

use std::time::Duration;

use async_trait::async_trait;
use url::Url;

use crate::config::Config;
use crate::error::{BotError, Result};
use crate::forge::{ForgeMessage, ReplyTarget, ReviewCommentTarget};
use crate::location::{ForgeKind, ForgeLocation};

/// Minimal forge API used by the gateway.
#[async_trait]
pub trait ForgeApi: Send + Sync {
    /// Read the PR author's login when supported by the forge.
    async fn pull_request_author(&self, _message: &ForgeMessage) -> Result<Option<String>> {
        Ok(None)
    }

    /// Post `body` as a comment on the issue / pull request at `location`.
    async fn post_comment(&self, location: &Url, body: &str) -> Result<()>;

    /// Post `body` as a reply to the comment that triggered `message`.
    ///
    /// The default keeps every reply in the issue / pull-request
    /// conversation. Forges that can thread an inline review comment override
    /// this so the answer stays attached to the code line it discusses.
    async fn reply(&self, message: &ForgeMessage, body: &str) -> Result<()> {
        self.post_comment(&message.location, body).await
    }

    /// Post `body` as a reply and return the new comment's id when the forge
    /// supports editing it later.
    ///
    /// The default does nothing and returns `None`: a caller that cannot track
    /// the comment buffers its notices and posts them together instead.
    async fn reply_tracked(&self, message: &ForgeMessage, body: &str) -> Result<Option<String>> {
        let _ = (message, body);
        Ok(None)
    }

    /// Replace the body of the comment `comment_id` previously returned by
    /// [`Self::reply_tracked`]. The default posts `body` as a new reply so a
    /// forge without edit support still delivers the update.
    async fn update_reply(
        &self,
        message: &ForgeMessage,
        comment_id: &str,
        body: &str,
    ) -> Result<()> {
        let _ = comment_id;
        self.reply(message, body).await
    }
}

/// A [`ForgeApi`] that posts over HTTP to the supported forges.
pub struct HttpForgeApi {
    client: reqwest::Client,
    config: Config,
    /// Effective Forgejo/Gitea token for this client. It is set from the
    /// addressed user's own token, so one user never replies with another's
    /// credential.
    forgejo_token: Option<String>,
}

impl HttpForgeApi {
    pub fn new(config: Config) -> Result<Self> {
        let token = config.forges.forgejo.as_ref().and_then(|c| c.token.clone());
        Self::with_forgejo_token(config, token)
    }

    /// Build a client that uses `token` for Forgejo/Gitea requests instead of
    /// the global token.
    pub fn with_forgejo_token(config: Config, token: Option<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("forge-bot/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            client,
            config,
            forgejo_token: token,
        })
    }

    /// Replace the body of a Forgejo issue comment.
    async fn update_forgejo(
        &self,
        loc: &ForgeLocation,
        comment_id: &str,
        body: &str,
    ) -> Result<()> {
        let cfg = self
            .config
            .forges
            .forgejo
            .as_ref()
            .ok_or_else(|| BotError::ForgeApi("forgejo is not configured".into()))?;
        let url = format!(
            "{}/api/v1/repos/{}/{}/issues/comments/{}",
            cfg.base_url.trim_end_matches('/'),
            loc.owner,
            loc.repo,
            comment_id
        );
        let mut req = self
            .client
            .patch(&url)
            .json(&serde_json::json!({ "body": body }));
        if let Some(token) = &self.forgejo_token {
            req = req.header("Authorization", format!("token {token}"));
        }
        send(req).await
    }

    /// Post `body` as a Forgejo issue comment and return its id.
    async fn tracked_forgejo(&self, loc: &ForgeLocation, body: &str) -> Result<Option<String>> {
        let cfg = self
            .config
            .forges
            .forgejo
            .as_ref()
            .ok_or_else(|| BotError::ForgeApi("forgejo is not configured".into()))?;
        let number = loc
            .number
            .ok_or_else(|| BotError::ForgeApi("location has no issue number".into()))?;
        let url = format!(
            "{}/api/v1/repos/{}/{}/issues/{}/comments",
            cfg.base_url.trim_end_matches('/'),
            loc.owner,
            loc.repo,
            number
        );
        let mut req = self
            .client
            .post(&url)
            .json(&serde_json::json!({ "body": body }));
        if let Some(token) = &self.forgejo_token {
            req = req.header("Authorization", format!("token {token}"));
        }
        let response = req.send().await?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(forge_error(status, &text));
        }
        Ok(serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|comment| comment.get("id").and_then(serde_json::Value::as_i64))
            .map(|id| id.to_string()))
    }

    /// Post `body` as a Forgejo issue comment.
    async fn post_forgejo(&self, loc: &ForgeLocation, body: &str) -> Result<()> {
        let cfg = self
            .config
            .forges
            .forgejo
            .as_ref()
            .ok_or_else(|| BotError::ForgeApi("forgejo is not configured".into()))?;
        let number = loc
            .number
            .ok_or_else(|| BotError::ForgeApi("location has no issue number".into()))?;
        let url = format!(
            "{}/api/v1/repos/{}/{}/issues/{}/comments",
            cfg.base_url.trim_end_matches('/'),
            loc.owner,
            loc.repo,
            number
        );
        let mut req = self
            .client
            .post(&url)
            .json(&serde_json::json!({ "body": body }));
        if let Some(token) = &self.forgejo_token {
            req = req.header("Authorization", format!("token {token}"));
        }
        send(req).await
    }

    /// Post a reply inside the inline review thread `target` identifies.
    ///
    /// Forgejo groups review comments by review, file path and line, so the
    /// reply is created through the review comment endpoint with the same
    /// coordinates as the original mention.
    async fn post_forgejo_review(
        &self,
        loc: &ForgeLocation,
        target: &ReviewCommentTarget,
        body: &str,
    ) -> Result<()> {
        self.tracked_forgejo_review(loc, target, body)
            .await
            .map(|_| ())
    }

    /// Post a reply inside the inline review thread `target` identifies and
    /// return the new comment id.
    ///
    /// The review-comment route has no edit method, but Forgejo edits a review
    /// comment through the generic issue-comment endpoint, so the id is enough
    /// to update this reply in place later.
    async fn tracked_forgejo_review(
        &self,
        loc: &ForgeLocation,
        target: &ReviewCommentTarget,
        body: &str,
    ) -> Result<Option<String>> {
        let cfg = self
            .config
            .forges
            .forgejo
            .as_ref()
            .ok_or_else(|| BotError::ForgeApi("forgejo is not configured".into()))?;
        let number = loc
            .number
            .ok_or_else(|| BotError::ForgeApi("location has no issue number".into()))?;
        let url = format!(
            "{}/api/v1/repos/{}/{}/pulls/{}/reviews/{}/comments",
            cfg.base_url.trim_end_matches('/'),
            loc.owner,
            loc.repo,
            number,
            target.review_id
        );
        let (new_position, old_position) = if target.line < 0 {
            (0, -target.line)
        } else {
            (target.line, 0)
        };
        let mut req = self.client.post(&url).json(&serde_json::json!({
            "body": body,
            "path": target.path,
            "new_position": new_position,
            "old_position": old_position,
            "extra_lines_count": target.extra_lines_count,
        }));
        if let Some(token) = &self.forgejo_token {
            req = req.header("Authorization", format!("token {token}"));
        }
        let response = req.send().await?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(forge_error(status, &text));
        }
        Ok(serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|comment| comment.get("id").and_then(serde_json::Value::as_i64))
            .map(|id| id.to_string()))
    }

    async fn post_github(&self, loc: &ForgeLocation, body: &str) -> Result<()> {
        let cfg = self.config.forges.github.as_ref();
        let base = cfg
            .map(|c| c.base_url.trim_end_matches('/').to_owned())
            .filter(|b| !b.is_empty())
            .unwrap_or_else(|| "https://api.github.com".to_owned());
        let number = loc
            .number
            .ok_or_else(|| BotError::ForgeApi("location has no issue number".into()))?;
        let url = format!(
            "{}/repos/{}/{}/issues/{}/comments",
            base, loc.owner, loc.repo, number
        );
        let mut req = self
            .client
            .post(&url)
            .json(&serde_json::json!({ "body": body }));
        if let Some(token) = cfg.and_then(|c| c.token.as_ref()) {
            req = req
                .header("Authorization", format!("Bearer {token}"))
                .header("Accept", "application/vnd.github+json");
        }
        send(req).await
    }

    async fn post_gitlab(&self, loc: &ForgeLocation, body: &str) -> Result<()> {
        let cfg = self
            .config
            .forges
            .gitlab
            .as_ref()
            .ok_or_else(|| BotError::ForgeApi("gitlab is not configured".into()))?;
        let base = if cfg.base_url.is_empty() {
            "https://gitlab.com".to_owned()
        } else {
            cfg.base_url.trim_end_matches('/').to_owned()
        };
        let number = loc
            .number
            .ok_or_else(|| BotError::ForgeApi("location has no issue number".into()))?;
        let project = urlencoding(&format!("{}/{}", loc.owner, loc.repo));
        let resource = match loc.kind {
            crate::location::LocationKind::PullRequest => "merge_requests",
            _ => "issues",
        };
        let url = format!("{base}/api/v4/projects/{project}/{resource}/{number}/notes");
        let mut req = self
            .client
            .post(&url)
            .json(&serde_json::json!({ "body": body }));
        if let Some(token) = &cfg.token {
            req = req.header("PRIVATE-TOKEN", token);
        }
        send(req).await
    }
}

#[async_trait]
impl ForgeApi for HttpForgeApi {
    async fn pull_request_author(&self, message: &ForgeMessage) -> Result<Option<String>> {
        if !message.is_pull_request
            || !matches!(message.forge, ForgeKind::Forgejo | ForgeKind::Gitea)
        {
            return Ok(None);
        }
        let Some(number) = message.number else {
            return Ok(None);
        };
        let cfg = self
            .config
            .forges
            .forgejo
            .as_ref()
            .ok_or_else(|| BotError::ForgeApi("forgejo is not configured".into()))?;
        let url = format!(
            "{}/api/v1/repos/{}/pulls/{number}",
            cfg.base_url.trim_end_matches('/'),
            message.repository
        );
        let mut request = self.client.get(url);
        if let Some(token) = &self.forgejo_token {
            request = request.header("Authorization", format!("token {token}"));
        }
        let pr: serde_json::Value = request.send().await?.error_for_status()?.json().await?;
        Ok(pr
            .pointer("/user/login")
            .and_then(serde_json::Value::as_str)
            .filter(|login| crate::config::is_valid_user_identifier(login))
            .map(str::to_owned))
    }

    async fn post_comment(&self, location: &Url, body: &str) -> Result<()> {
        let loc = ForgeLocation::parse(location)?;
        match loc.forge {
            ForgeKind::Forgejo | ForgeKind::Gitea => self.post_forgejo(&loc, body).await,
            ForgeKind::GitHub => self.post_github(&loc, body).await,
            ForgeKind::GitLab => self.post_gitlab(&loc, body).await,
            ForgeKind::Unknown => Err(BotError::UnsupportedForge(loc.forge.to_string())),
        }
    }

    async fn reply(&self, message: &ForgeMessage, body: &str) -> Result<()> {
        if let ReplyTarget::ReviewComment(target) = &message.reply_target {
            let loc = ForgeLocation::parse(&message.location)?;
            // Without a line the review endpoint cannot anchor the comment, so
            // fall back to a normal conversation reply.
            if target.line != 0 && matches!(loc.forge, ForgeKind::Forgejo | ForgeKind::Gitea) {
                return self.post_forgejo_review(&loc, target, body).await;
            }
        }
        self.post_comment(&message.location, body).await
    }

    async fn reply_tracked(&self, message: &ForgeMessage, body: &str) -> Result<Option<String>> {
        let loc = ForgeLocation::parse(&message.location)?;
        if let ReplyTarget::ReviewComment(target) = &message.reply_target
            && target.line != 0
            && matches!(loc.forge, ForgeKind::Forgejo | ForgeKind::Gitea)
        {
            // The review-comment route has no edit method, but Forgejo edits
            // review comments through the generic issue-comment endpoint, so
            // track the new id and update it in place later.
            return self.tracked_forgejo_review(&loc, target, body).await;
        }
        match loc.forge {
            ForgeKind::Forgejo | ForgeKind::Gitea => self.tracked_forgejo(&loc, body).await,
            _ => Ok(None),
        }
    }

    async fn update_reply(
        &self,
        message: &ForgeMessage,
        comment_id: &str,
        body: &str,
    ) -> Result<()> {
        let loc = ForgeLocation::parse(&message.location)?;
        match loc.forge {
            ForgeKind::Forgejo | ForgeKind::Gitea => {
                self.update_forgejo(&loc, comment_id, body).await
            }
            _ => self.reply(message, body).await,
        }
    }
}

async fn send(request: reqwest::RequestBuilder) -> Result<()> {
    let response = request.send().await?;
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let text = response.text().await.unwrap_or_default();
    Err(forge_error(status, &text))
}

/// Turn a non-success forge response into a [`BotError`].
fn forge_error(status: reqwest::StatusCode, body: &str) -> BotError {
    let detail = format!("forge returned {status}: {}", body.trim());
    if matches!(
        status,
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
    ) {
        BotError::ForgePermissionDenied(detail)
    } else {
        BotError::ForgeApi(detail)
    }
}

/// A [`ForgeApi`] that drops comments, useful for tests and dry runs.
#[derive(Debug, Default, Clone)]
pub struct NoopForgeApi;

#[async_trait]
impl ForgeApi for NoopForgeApi {
    async fn post_comment(&self, location: &Url, body: &str) -> Result<()> {
        tracing::debug!(%location, %body, "noop forge api: dropping comment");
        Ok(())
    }
}

/// A [`ForgeApi`] that records every posted comment, for tests.
#[derive(Debug, Default)]
pub struct RecordingForgeApi {
    comments: std::sync::Mutex<Vec<(Url, String)>>,
}

impl RecordingForgeApi {
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of the comments posted so far, oldest first.
    pub fn comments(&self) -> Vec<(Url, String)> {
        self.comments
            .lock()
            .expect("recording forge api mutex poisoned")
            .clone()
    }
}

#[async_trait]
impl ForgeApi for RecordingForgeApi {
    async fn post_comment(&self, location: &Url, body: &str) -> Result<()> {
        self.comments
            .lock()
            .expect("recording forge api mutex poisoned")
            .push((location.clone(), body.to_owned()));
        Ok(())
    }
}

/// Percent-encode a string for use inside a URL path segment.
fn urlencoding(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ForgejoConfig};
    use crate::forge::{ForgeMessage, ReplyTarget, ReviewCommentTarget};

    #[test]
    fn encodes_project_path() {
        assert_eq!(urlencoding("group/proj"), "group%2Fproj");
        assert_eq!(urlencoding("a.b-c_d"), "a.b-c_d");
    }

    fn review_message() -> ForgeMessage {
        ForgeMessage {
            forge: ForgeKind::Forgejo,
            location: Url::parse("http://forge.local/a/b/pulls/22#issuecomment-9039").unwrap(),
            body: "@agent how long does this live?".into(),
            author: "shylock".into(),
            repository: "a/b".into(),
            comment_id: Some(9039),
            number: Some(22),
            is_pull_request: true,
            linked_issue: None,
            event: "pull_request_comment".into(),
            title: None,
            reply_target: ReplyTarget::ReviewComment(ReviewCommentTarget {
                review_id: 103,
                path: "src/agent/registry.rs".into(),
                line: 30,
                extra_lines_count: 0,
            }),
        }
    }

    #[tokio::test]
    async fn reads_pr_author_with_addressed_users_token() {
        use axum::{Json, Router, routing::get};
        let app = Router::new().route(
            "/api/v1/repos/a/b/pulls/22",
            get(|headers: axum::http::HeaderMap| async move {
                assert_eq!(headers["authorization"], "token reviewer-token");
                Json(serde_json::json!({"user": {"login": "submitter-bot"}}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut config = Config::default();
        config.forges.forgejo = Some(ForgejoConfig {
            base_url: format!("http://{addr}"),
            token: Some("global-token".into()),
            ..Default::default()
        });
        let api = HttpForgeApi::with_forgejo_token(config, Some("reviewer-token".into())).unwrap();
        assert_eq!(
            api.pull_request_author(&review_message())
                .await
                .unwrap()
                .as_deref(),
            Some("submitter-bot")
        );
        let mut issue = review_message();
        issue.is_pull_request = false;
        assert!(api.pull_request_author(&issue).await.unwrap().is_none());
        server.abort();
    }

    #[tokio::test]
    async fn replies_inline_review_comments_on_forgejo() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 8192];
            let read = socket.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            let response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
            let _ = socket.write_all(response.as_bytes()).await;
            request
        });

        let mut config = Config::default();
        config.forges.forgejo = Some(ForgejoConfig {
            base_url: format!("http://{addr}"),
            token: Some("secret".into()),
            ..Default::default()
        });
        let api = HttpForgeApi::new(config).unwrap();
        api.reply(&review_message(), "It lives for one hour.")
            .await
            .unwrap();

        let request = server.await.unwrap();
        assert!(
            request.starts_with("POST /api/v1/repos/a/b/pulls/22/reviews/103/comments "),
            "unexpected request line: {request}"
        );
        let lower = request.to_lowercase();
        assert!(lower.contains("authorization: token secret"));
        assert!(request.contains("\"path\":\"src/agent/registry.rs\""));
        assert!(request.contains("\"new_position\":30"));
        assert!(request.contains("\"old_position\":0"));
        assert!(request.contains("It lives for one hour."));
    }

    #[tokio::test]
    async fn old_side_review_replies_use_the_old_position() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 8192];
            let read = socket.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            let response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
            let _ = socket.write_all(response.as_bytes()).await;
            request
        });

        let mut config = Config::default();
        config.forges.forgejo = Some(ForgejoConfig {
            base_url: format!("http://{addr}"),
            token: None,
            ..Default::default()
        });
        let api = HttpForgeApi::new(config).unwrap();
        let mut message = review_message();
        message.location = Url::parse("http://forge.local/a/b/pulls/22#issuecomment-8992").unwrap();
        message.reply_target = ReplyTarget::ReviewComment(ReviewCommentTarget {
            review_id: 103,
            path: "src/agent/registry.rs".into(),
            line: -12,
            extra_lines_count: 0,
        });
        api.reply(&message, "reply").await.unwrap();

        let request = server.await.unwrap();
        assert!(request.contains("\"new_position\":0"));
        assert!(request.contains("\"old_position\":12"));
    }

    /// Start a one-request HTTP server that answers with `status` and returns
    /// the raw request it received.
    async fn one_shot(status: u16) -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0u8; 16384];
            let read = socket.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            let payload = if status == 200 { "{}" } else { "denied" };
            let response = format!(
                "HTTP/1.1 {status} STATUS\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            request
        });
        (format!("http://{addr}"), handle)
    }

    fn config_with(
        forgejo: Option<ForgejoConfig>,
        github: Option<crate::config::GithubConfig>,
        gitlab: Option<crate::config::GitlabConfig>,
    ) -> Config {
        let mut config = Config::default();
        config.forges.forgejo = forgejo;
        config.forges.github = github;
        config.forges.gitlab = gitlab;
        config
    }

    #[tokio::test]
    async fn posts_issue_comments_to_forgejo() {
        let (base, server) = one_shot(200).await;
        let config = config_with(
            Some(ForgejoConfig {
                base_url: base,
                token: Some("secret".into()),
                ..Default::default()
            }),
            None,
            None,
        );
        let api = HttpForgeApi::new(config).unwrap();
        let location = Url::parse("http://forge.local/a/b/issues/7#issuecomment-1").unwrap();
        api.post_comment(&location, "hello there").await.unwrap();

        let request = server.await.unwrap();
        assert!(request.starts_with("POST /api/v1/repos/a/b/issues/7/comments "));
        assert!(
            request
                .to_lowercase()
                .contains("authorization: token secret")
        );
        assert!(request.contains("hello there"));
    }

    #[tokio::test]
    async fn posts_issue_comments_to_github() {
        let (base, server) = one_shot(200).await;
        let config = config_with(
            None,
            Some(crate::config::GithubConfig {
                base_url: base,
                token: Some("gh".into()),
                ..Default::default()
            }),
            None,
        );
        let api = HttpForgeApi::new(config).unwrap();
        let location = Url::parse("https://github.com/a/b/issues/7#issuecomment-1").unwrap();
        api.post_comment(&location, "hello there").await.unwrap();

        let request = server.await.unwrap();
        assert!(request.starts_with("POST /repos/a/b/issues/7/comments "));
        assert!(request.to_lowercase().contains("authorization: bearer gh"));
        assert!(request.contains("accept: application/vnd.github+json"));
    }

    #[tokio::test]
    async fn posts_issue_notes_to_gitlab() {
        let (base, server) = one_shot(200).await;
        let config = config_with(
            None,
            None,
            Some(crate::config::GitlabConfig {
                base_url: base,
                token: Some("gl".into()),
                ..Default::default()
            }),
        );
        let api = HttpForgeApi::new(config).unwrap();
        let location = Url::parse("https://gitlab.com/a/b/issues/7#note_1").unwrap();
        api.post_comment(&location, "hello there").await.unwrap();

        let request = server.await.unwrap();
        assert!(request.starts_with("POST /api/v4/projects/a%2Fb/issues/7/notes "));
        assert!(request.to_lowercase().contains("private-token: gl"));
    }

    #[tokio::test]
    async fn posts_merge_request_notes_to_gitlab() {
        let (base, server) = one_shot(200).await;
        let config = config_with(
            None,
            None,
            Some(crate::config::GitlabConfig {
                base_url: base,
                ..Default::default()
            }),
        );
        let api = HttpForgeApi::new(config).unwrap();
        let location = Url::parse("https://gitlab.com/a/b/merge_requests/7").unwrap();
        api.post_comment(&location, "hello").await.unwrap();

        let request = server.await.unwrap();
        assert!(request.starts_with("POST /api/v4/projects/a%2Fb/merge_requests/7/notes "));
    }

    #[tokio::test]
    async fn unknown_forge_cannot_post() {
        let api = HttpForgeApi::new(Config::default()).unwrap();
        let location = Url::parse("https://example.com/a/b/issues/7").unwrap();
        // `example.com` defaults to Forgejo, which is not configured here.
        let error = api.post_comment(&location, "hi").await.unwrap_err();
        assert!(error.to_string().contains("forgejo is not configured"));
    }

    #[tokio::test]
    async fn forgejo_requires_an_issue_number() {
        let api =
            HttpForgeApi::new(config_with(Some(ForgejoConfig::default()), None, None)).unwrap();
        let location = Url::parse("http://forge.local/a/b").unwrap();
        let error = api.post_comment(&location, "hi").await.unwrap_err();
        assert!(error.to_string().contains("no issue number"));
    }

    #[tokio::test]
    async fn forbidden_responses_are_permission_errors() {
        let (base, server) = one_shot(403).await;
        let config = config_with(
            Some(ForgejoConfig {
                base_url: base,
                ..Default::default()
            }),
            None,
            None,
        );
        let api = HttpForgeApi::new(config).unwrap();
        let location = Url::parse("http://forge.local/a/b/issues/7").unwrap();
        let error = api.post_comment(&location, "hi").await.unwrap_err();
        assert!(matches!(error, BotError::ForgePermissionDenied(_)));
        assert!(server.await.unwrap().starts_with("POST "));
    }

    #[tokio::test]
    async fn server_errors_are_forge_api_errors() {
        let (base, server) = one_shot(500).await;
        let config = config_with(
            Some(ForgejoConfig {
                base_url: base,
                ..Default::default()
            }),
            None,
            None,
        );
        let api = HttpForgeApi::new(config).unwrap();
        let location = Url::parse("http://forge.local/a/b/issues/7").unwrap();
        let error = api.post_comment(&location, "hi").await.unwrap_err();
        assert!(matches!(error, BotError::ForgeApi(_)));
        assert!(error.to_string().contains("denied"));
        assert!(server.await.unwrap().starts_with("POST "));
    }

    #[tokio::test]
    async fn unauthorized_responses_are_permission_errors() {
        let (base, server) = one_shot(401).await;
        let config = config_with(
            Some(ForgejoConfig {
                base_url: base,
                ..Default::default()
            }),
            None,
            None,
        );
        let api = HttpForgeApi::new(config).unwrap();
        let location = Url::parse("http://forge.local/a/b/issues/7").unwrap();
        let error = api.post_comment(&location, "hi").await.unwrap_err();
        assert!(matches!(error, BotError::ForgePermissionDenied(_)));
        assert!(server.await.unwrap().starts_with("POST "));
    }
    fn issue_message() -> ForgeMessage {
        ForgeMessage {
            forge: ForgeKind::Forgejo,
            location: Url::parse("http://forge.local/a/b/issues/22#issuecomment-9039").unwrap(),
            body: "@agent go".into(),
            author: "shylock".into(),
            repository: "a/b".into(),
            comment_id: Some(9039),
            number: Some(22),
            is_pull_request: false,
            linked_issue: None,
            event: "issue_comment".into(),
            title: None,
            reply_target: ReplyTarget::default(),
        }
    }

    #[tokio::test]
    async fn tracks_and_edits_forgejo_issue_comments() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for response_body in ["{\"id\":42}", "{}"] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buffer = [0u8; 8192];
                let read = socket.read(&mut buffer).await.unwrap();
                requests.push(String::from_utf8_lossy(&buffer[..read]).to_string());
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
            requests
        });

        let config = config_with(
            Some(ForgejoConfig {
                base_url: format!("http://{addr}"),
                token: Some("secret".into()),
                ..Default::default()
            }),
            None,
            None,
        );
        let api = HttpForgeApi::new(config).unwrap();

        let id = api.reply_tracked(&issue_message(), "On it.").await.unwrap();
        assert_eq!(id.as_deref(), Some("42"));

        api.update_reply(&issue_message(), "42", "On it. Switching.")
            .await
            .unwrap();

        let requests = server.await.unwrap();
        assert!(
            requests[0].starts_with("POST /api/v1/repos/a/b/issues/22/comments "),
            "unexpected request: {}",
            requests[0]
        );
        assert!(
            requests[1].starts_with("PATCH /api/v1/repos/a/b/issues/comments/42 "),
            "unexpected request: {}",
            requests[1]
        );
        assert!(requests[1].contains("On it. Switching."));
    }

    #[tokio::test]
    async fn tracks_and_edits_forgejo_review_comments() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for response_body in ["{\"id\":42}", "{}"] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buffer = [0u8; 8192];
                let read = socket.read(&mut buffer).await.unwrap();
                requests.push(String::from_utf8_lossy(&buffer[..read]).to_string());
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
            requests
        });

        let config = config_with(
            Some(ForgejoConfig {
                base_url: format!("http://{addr}"),
                token: Some("secret".into()),
                ..Default::default()
            }),
            None,
            None,
        );
        let api = HttpForgeApi::new(config).unwrap();

        let id = api
            .reply_tracked(&review_message(), "On it.")
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("42"));

        // The review endpoint has no edit route, but the generic issue-comment
        // endpoint updates the review comment in place.
        api.update_reply(&review_message(), "42", "On it. Switching.")
            .await
            .unwrap();

        let requests = server.await.unwrap();
        assert!(
            requests[0].starts_with("POST /api/v1/repos/a/b/pulls/22/reviews/103/comments "),
            "unexpected request: {}",
            requests[0]
        );
        assert!(
            requests[0].contains("\"path\":\"src/agent/registry.rs\""),
            "unexpected request: {}",
            requests[0]
        );
        assert!(
            requests[1].starts_with("PATCH /api/v1/repos/a/b/issues/comments/42 "),
            "unexpected request: {}",
            requests[1]
        );
        assert!(requests[1].contains("On it. Switching."));
    }

    #[tokio::test]
    async fn tracked_post_surfaces_permission_errors() {
        let (base, server) = one_shot(401).await;
        let api = HttpForgeApi::new(config_with(
            Some(ForgejoConfig {
                base_url: base,
                token: Some("secret".into()),
                ..Default::default()
            }),
            None,
            None,
        ))
        .unwrap();
        let err = api
            .reply_tracked(&issue_message(), "On it.")
            .await
            .unwrap_err();
        assert!(matches!(err, BotError::ForgePermissionDenied(_)));
        assert!(server.await.unwrap().starts_with("POST "));
    }
}

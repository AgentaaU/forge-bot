//! Polling ingester.
//!
//! A repository collaborator without admin rights cannot create a Forgejo
//! webhook. Polling conversation comments and submitted review bodies feeds
//! the same [`Dispatcher`] pipeline the webhook handler uses, so all the
//! mention detection, authorization, agent selection and reply logic is
//! shared.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use crate::config::Config;
use crate::error::{BotError, Result};
use crate::forge::ForgeMessage;
use crate::location::ForgeKind;
use crate::session::Dispatcher;

/// High-water mark for one repository.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Cursor {
    last_id: i64,
    last_time: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    seen_reviews: HashMap<i64, String>,
}

/// Cached list of repositories to poll.
#[derive(Default)]
struct RepoCache {
    repos: Vec<String>,
    refreshed: Option<Instant>,
}

/// Watches repositories for new `@agent` mentions.
pub struct Poller {
    config: Arc<Config>,
    dispatcher: Arc<Dispatcher>,
    client: reqwest::Client,
    state_path: PathBuf,
    cursors: Mutex<HashMap<String, Cursor>>,
    repos: Mutex<RepoCache>,
}

impl Poller {
    pub fn new(config: Arc<Config>, dispatcher: Arc<Dispatcher>) -> Result<Self> {
        let state_dir = crate::config::expand_tilde(&config.session.dir);
        std::fs::create_dir_all(&state_dir)?;
        let state_path = state_dir.join("poller.json");

        let cursors = match std::fs::read_to_string(&state_path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|error| {
                tracing::warn!(%error, "ignoring unreadable poller state");
                HashMap::new()
            }),
            Err(_) => HashMap::new(),
        };

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("forge-bot/", env!("CARGO_PKG_VERSION")))
            .build()?;

        Ok(Self {
            config,
            dispatcher,
            client,
            state_path,
            cursors: Mutex::new(cursors),
            repos: Mutex::new(RepoCache::default()),
        })
    }

    /// Run until the process exits.
    pub async fn run(self: Arc<Self>) {
        let interval = Duration::from_secs(self.config.poller.interval_secs.max(1));
        tracing::info!(
            repositories = ?self.config.poller.repositories,
            interval_secs = interval.as_secs(),
            discover = self.config.poller.repositories.is_empty(),
            "poller started"
        );
        loop {
            if let Err(error) = self.tick().await {
                tracing::warn!(%error, "poll failed");
            }
            tokio::time::sleep(interval).await;
        }
    }

    /// One polling pass over every repository in scope.
    pub async fn tick(&self) -> Result<()> {
        let forgejo = self
            .config
            .forges
            .forgejo
            .as_ref()
            .ok_or_else(|| BotError::Config("poller requires a [forgejo] section".into()))?;

        let base = forgejo.base_url.trim_end_matches('/').to_owned();
        // Poll as the default user, which is the account automatic work runs
        // under. A user with its own token never falls back to the legacy one.
        let global_token = forgejo.token.as_deref();
        let default_user = self.dispatcher.identities().default_user();
        let token = default_user
            .effective_token(global_token)
            .map(str::to_owned);

        for repo in self.repositories(&base, token.as_deref()).await? {
            if let Err(error) = self.poll_repo(&base, token.as_deref(), &repo).await {
                tracing::warn!(repo = %repo, %error, "failed to poll repository");
            }
            if let Err(error) = self.poll_reviews(&base, token.as_deref(), &repo).await {
                tracing::warn!(repo = %repo, %error, "failed to poll reviews");
            }
        }

        self.persist();
        Ok(())
    }

    /// Repositories to poll: the explicit list when configured, otherwise all
    /// repositories visible to the token (refreshed periodically).
    async fn repositories(&self, base: &str, token: Option<&str>) -> Result<Vec<String>> {
        if !self.config.poller.repositories.is_empty() {
            return Ok(self.config.poller.repositories.clone());
        }

        let ttl = Duration::from_secs(self.config.poller.discover_interval_secs.max(1));
        let fresh = {
            let cache = self.repos.lock().expect("poller repo cache poisoned");
            !cache.repos.is_empty()
                && cache
                    .refreshed
                    .map(|at| at.elapsed() < ttl)
                    .unwrap_or(false)
        };
        if fresh {
            return Ok(self
                .repos
                .lock()
                .expect("poller repo cache poisoned")
                .repos
                .clone());
        }

        match self.discover_repositories(base, token).await {
            Ok(repos) => {
                tracing::info!(count = repos.len(), "discovered repositories");
                let mut cache = self.repos.lock().expect("poller repo cache poisoned");
                cache.repos = repos.clone();
                cache.refreshed = Some(Instant::now());
                Ok(repos)
            }
            Err(error) => {
                // Keep serving the previous list if discovery fails.
                let cached = self
                    .repos
                    .lock()
                    .expect("poller repo cache poisoned")
                    .repos
                    .clone();
                if cached.is_empty() {
                    Err(error)
                } else {
                    tracing::warn!(%error, "repository discovery failed; using cached list");
                    Ok(cached)
                }
            }
        }
    }

    /// Enumerate every repository visible to the token.
    async fn discover_repositories(&self, base: &str, token: Option<&str>) -> Result<Vec<String>> {
        let limit = self.config.poller.page_limit.max(1);
        let url = format!("{base}/api/v1/repos/search");
        let mut repos = Vec::new();

        for page in 1..=100 {
            let mut request = self.client.get(&url).query(&[
                ("limit", limit.to_string()),
                ("page", page.to_string()),
                ("sort", "id".to_owned()),
                ("order", "asc".to_owned()),
            ]);
            if let Some(token) = token {
                request = request.header("Authorization", format!("token {token}"));
            }

            let response = request.send().await?;
            if !response.status().is_success() {
                return Err(BotError::ForgeApi(format!(
                    "repository search returned {}",
                    response.status()
                )));
            }
            let payload: Value = response.json().await?;
            let page_repos = repos_from_search_page(&payload);
            let full_page = payload["data"].as_array().map(Vec::len).unwrap_or(0) >= limit;
            repos.extend(page_repos);
            if !full_page {
                break;
            }
        }

        repos.sort();
        repos.dedup();
        Ok(repos)
    }

    async fn poll_repo(&self, base: &str, token: Option<&str>, repo: &str) -> Result<()> {
        let since = self.since_for(repo);
        let limit = self.config.poller.page_limit.max(1);
        let url = format!("{base}/api/v1/repos/{repo}/issues/comments");

        let mut request = self
            .client
            .get(&url)
            .query(&[("limit", limit.to_string()), ("since", since)]);
        if let Some(token) = token {
            request = request.header("Authorization", format!("token {token}"));
        }

        let response = request.send().await?;
        let status = response.status();
        // Repositories the token cannot read are expected when scanning the
        // whole instance; skip them quietly.
        if matches!(
            status,
            reqwest::StatusCode::UNAUTHORIZED
                | reqwest::StatusCode::FORBIDDEN
                | reqwest::StatusCode::NOT_FOUND
        ) {
            tracing::debug!(repo, %status, "skipping inaccessible repository");
            return Ok(());
        }
        if !status.is_success() {
            return Err(BotError::ForgeApi(format!(
                "comments query returned {status}"
            )));
        }
        let mut comments: Vec<Value> = response.json().await?;
        comments.sort_by_key(|comment| comment["id"].as_i64().unwrap_or_default());

        let mut cursor = self.cursor(repo);
        for comment in comments {
            let id = comment["id"].as_i64().unwrap_or_default();
            if id <= cursor.last_id {
                continue;
            }
            // Advance the cursor even for comments we ignore so they are not
            // reconsidered on the next pass.
            cursor.last_id = id;
            if let Some(created) = comment["created_at"].as_str() {
                cursor.last_time = Some(normalize_time(created));
            }

            let Some(message) = message_from_comment(repo, &comment) else {
                continue;
            };
            self.dispatch_message(message).await;
        }

        self.set_cursor(repo, cursor);
        Ok(())
    }

    async fn dispatch_message(&self, message: ForgeMessage) {
        let repo = message.repository.clone();
        // Record human mentions before the routing ignore rules: a polled
        // agent comment that mentions a human must reach `/notifications`
        // even when the comment itself is skipped for routing. The shared
        // recorder dedupes a comment also seen by the webhook ingester.
        self.dispatcher.record_human_notifications(&message);
        if self.dispatcher.policy().is_ignored(&message.author) {
            return;
        }
        let (mention, agent_name) = match self.dispatcher.route(&message) {
            Ok(Some(routed)) => routed,
            Ok(None) => return,
            Err(BotError::Unauthorized(reason)) => {
                tracing::info!(%reason, repo, "ignored unroutable polled trigger");
                return;
            }
            Err(error) => {
                tracing::warn!(%error, repo, "failed to route polled trigger");
                return;
            }
        };

        match self.dispatcher.submit(message, mention, &agent_name).await {
            Ok(job_id) => tracing::info!(%job_id, repo, "accepted polled trigger"),
            Err(BotError::Unauthorized(reason)) => {
                tracing::info!(%reason, repo, "ignored unauthorized trigger");
            }
            Err(error) => tracing::warn!(%error, repo, "failed to enqueue polled trigger"),
        }
    }

    /// Review bodies are deliberately absent from Forgejo's issue-comments API.
    /// Find updated PRs, then read their submitted reviews with a separate cursor.
    async fn poll_reviews(&self, base: &str, token: Option<&str>, repo: &str) -> Result<()> {
        let key = format!("{repo}:reviews");
        let since = self.since_for(&key);
        // Keep a one-second overlap for API timestamps with second precision.
        let next_since =
            (Utc::now() - chrono::Duration::seconds(1)).to_rfc3339_opts(SecondsFormat::Secs, true);
        let mut cursor = self.cursor(&key);
        let pulls_url = format!("{base}/api/v1/repos/{repo}/issues");
        let Some(pulls) = self
            .fetch_pages(
                &pulls_url,
                token,
                &[("type", "pulls"), ("state", "all"), ("since", &since)],
            )
            .await?
        else {
            return Ok(());
        };
        for pull in pulls {
            let Some(number) = pull["number"].as_u64() else {
                continue;
            };
            let url = format!("{base}/api/v1/repos/{repo}/pulls/{number}/reviews");
            let reviews = self
                .fetch_pages(&url, token, &[])
                .await?
                .ok_or_else(|| BotError::ForgeApi("reviews query is inaccessible".into()))?;
            for review in reviews {
                if !matches!(
                    review["state"].as_str(),
                    Some("APPROVED" | "REQUEST_CHANGES" | "COMMENT")
                ) {
                    continue;
                }
                let (Some(id), Some(updated)) = (
                    review["id"].as_i64(),
                    review["updated_at"]
                        .as_str()
                        .or_else(|| review["submitted_at"].as_str()),
                ) else {
                    continue;
                };
                let updated = normalize_time(updated);
                // Forgejo submitted_at is the creation time, even after a pending
                // review is submitted. Its updated_at tracks that transition.
                if updated < since
                    || cursor
                        .seen_reviews
                        .get(&id)
                        .is_some_and(|seen| seen >= &updated)
                {
                    continue;
                }
                if let Some(message) = message_from_review(repo, &pull, &review) {
                    self.dispatch_message(message).await;
                }
                cursor.seen_reviews.insert(id, updated);
                self.set_cursor(&key, cursor.clone());
            }
        }
        cursor
            .seen_reviews
            .retain(|_, updated| *updated >= next_since);
        cursor.last_time = Some(next_since);
        self.set_cursor(&key, cursor);
        Ok(())
    }

    async fn fetch_pages(
        &self,
        url: &str,
        token: Option<&str>,
        query: &[(&str, &str)],
    ) -> Result<Option<Vec<Value>>> {
        let limit = self.config.poller.page_limit.max(1);
        let mut items = Vec::new();
        for page in 1.. {
            let mut request = self
                .client
                .get(url)
                .query(query)
                .query(&[("limit", limit), ("page", page)]);
            if let Some(token) = token {
                request = request.header("Authorization", format!("token {token}"));
            }
            let response = request.send().await?;
            if matches!(
                response.status(),
                reqwest::StatusCode::UNAUTHORIZED
                    | reqwest::StatusCode::FORBIDDEN
                    | reqwest::StatusCode::NOT_FOUND
            ) {
                return Ok(None);
            }
            let batch: Vec<Value> = response.error_for_status()?.json().await?;
            let done = batch.len() < limit;
            items.extend(batch);
            if done {
                break;
            }
        }
        Ok(Some(items))
    }

    fn cursor(&self, repo: &str) -> Cursor {
        self.cursors
            .lock()
            .expect("poller mutex poisoned")
            .get(repo)
            .cloned()
            .unwrap_or_default()
    }

    fn set_cursor(&self, repo: &str, cursor: Cursor) {
        self.cursors
            .lock()
            .expect("poller mutex poisoned")
            .insert(repo.to_owned(), cursor);
    }

    /// The `since` query value for a repository.
    fn since_for(&self, repo: &str) -> String {
        if let Some(time) = self.cursor(repo).last_time {
            return time;
        }
        let lookback = chrono::Duration::seconds(self.config.poller.lookback_secs as i64);
        (Utc::now() - lookback).to_rfc3339_opts(SecondsFormat::Secs, true)
    }

    fn persist(&self) {
        let cursors = self.cursors.lock().expect("poller mutex poisoned");
        match serde_json::to_vec_pretty(&*cursors) {
            Ok(raw) => {
                let tmp = self.state_path.with_extension("json.tmp");
                if std::fs::write(&tmp, raw)
                    .and_then(|_| std::fs::rename(&tmp, &self.state_path))
                    .is_err()
                {
                    tracing::warn!(path = %self.state_path.display(), "failed to persist poller state");
                }
            }
            Err(error) => tracing::warn!(%error, "failed to serialize poller state"),
        }
    }
}

/// Extract the `owner/repo` names from a `/repos/search` page, skipping
/// repositories without issues or that are archived.
fn repos_from_search_page(payload: &Value) -> Vec<String> {
    payload["data"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter(|repo| repo["has_issues"].as_bool().unwrap_or(true))
        .filter(|repo| !repo["archived"].as_bool().unwrap_or(false))
        .filter_map(|repo| repo["full_name"].as_str().map(str::to_owned))
        .collect()
}

/// Normalize a Forgejo timestamp to UTC RFC3339 so it can be sent back as a
/// `since` query value without an unescaped `+` offset.
fn normalize_time(raw: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|time| {
            time.with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::Secs, true)
        })
        .unwrap_or_else(|_| raw.to_owned())
}

/// Convert a Forgejo comment payload into a normalized message.
fn message_from_comment(repo: &str, comment: &Value) -> Option<ForgeMessage> {
    let body = comment["body"].as_str()?.to_owned();
    let author = comment["user"]["login"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let location = comment["html_url"]
        .as_str()
        .and_then(|s| Url::parse(s).ok())?;

    let comment_id = comment["id"].as_i64();
    let number = comment["issue_url"]
        .as_str()
        .and_then(|url| url.rsplit('/').next())
        .and_then(|tail| tail.parse::<u64>().ok());
    let is_pull_request = comment["pull_request_url"]
        .as_str()
        .map(|url| !url.is_empty())
        .unwrap_or(false);

    Some(ForgeMessage {
        forge: ForgeKind::Forgejo,
        location,
        body,
        author,
        repository: repo.to_owned(),
        comment_id,
        number,
        is_pull_request,
        linked_issue: None,
        event: "issue_comment".into(),
        title: None,
        reply_target: Default::default(),
    })
}

/// Reviews have their own IDs; do not use them as issue-comment IDs.
fn message_from_review(repo: &str, pull: &Value, review: &Value) -> Option<ForgeMessage> {
    let event = match review["state"].as_str()? {
        "APPROVED" => "pull_request_approved",
        "REQUEST_CHANGES" => "pull_request_rejected",
        "COMMENT" => "pull_request_comment",
        _ => return None,
    };
    Some(ForgeMessage {
        forge: ForgeKind::Forgejo,
        location: Url::parse(review["html_url"].as_str()?).ok()?,
        body: review["body"].as_str()?.to_owned(),
        author: review["user"]["login"].as_str()?.to_owned(),
        repository: repo.to_owned(),
        comment_id: None,
        number: pull["number"].as_u64(),
        is_pull_request: true,
        linked_issue: pull["body"]
            .as_str()
            .and_then(crate::forge::linked_issue_ref),
        event: event.into(),
        title: pull["title"].as_str().map(str::to_owned),
        reply_target: Default::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn filters_search_page() {
        let payload = json!({
            "ok": true,
            "data": [
                {"full_name": "a/one", "has_issues": true, "archived": false},
                {"full_name": "a/two", "has_issues": false, "archived": false},
                {"full_name": "a/three", "has_issues": true, "archived": true},
                {"full_name": "a/four", "has_issues": true, "archived": false}
            ]
        });
        assert_eq!(
            repos_from_search_page(&payload),
            vec!["a/one".to_string(), "a/four".to_string()]
        );
    }

    #[test]
    fn converts_comment_to_message() {
        let comment = json!({
            "id": 77,
            "body": "@shylock-bot do it",
            "html_url": "http://forge.local:3000/o/r/issues/3#issuecomment-77",
            "issue_url": "http://forge.local:3000/o/r/issues/3",
            "pull_request_url": "",
            "user": { "login": "alice" }
        });
        let message = message_from_comment("o/r", &comment).unwrap();
        assert_eq!(message.author, "alice");
        assert_eq!(message.number, Some(3));
        assert_eq!(message.comment_id, Some(77));
        assert!(!message.is_pull_request);
        assert_eq!(message.forge, ForgeKind::Forgejo);
    }

    #[test]
    fn normalizes_offsets_to_utc() {
        assert_eq!(
            normalize_time("2026-09-24T20:49:34+08:00"),
            "2026-09-24T12:49:34Z"
        );
        assert_eq!(
            normalize_time("2026-09-24T12:49:34Z"),
            "2026-09-24T12:49:34Z"
        );
    }

    #[test]
    fn detects_pull_request_comments() {
        let comment = json!({
            "id": 1,
            "body": "@shylock-bot x",
            "html_url": "http://forge.local:3000/o/r/pulls/5#issuecomment-1",
            "issue_url": "http://forge.local:3000/o/r/issues/5",
            "pull_request_url": "http://forge.local:3000/o/r/pulls/5",
            "user": { "login": "bob" }
        });
        let message = message_from_comment("o/r", &comment).unwrap();
        assert!(message.is_pull_request);
        assert_eq!(message.number, Some(5));
    }

    #[test]
    fn invalid_time_is_preserved() {
        assert_eq!(normalize_time("not-a-timestamp"), "not-a-timestamp");
    }

    #[test]
    fn incomplete_comments_are_dropped() {
        // No body.
        assert!(message_from_comment("o/r", &json!({ "html_url": "http://x/1" })).is_none());
        // Body but no parseable html_url.
        assert!(
            message_from_comment("o/r", &json!({ "body": "hi" })).is_none(),
            "comments without a location are dropped"
        );
        // Missing optional fields fall back to defaults.
        let comment = json!({
            "id": 9,
            "body": "hi",
            "html_url": "http://x/o/r/issues/1#issuecomment-9",
            "user": {}
        });
        let message = message_from_comment("o/r", &comment).unwrap();
        assert_eq!(message.author, "");
        assert_eq!(message.number, None);
        assert!(!message.is_pull_request);
        assert_eq!(message.comment_id, Some(9));
    }

    #[test]
    fn search_page_handles_shapes() {
        assert!(repos_from_search_page(&json!({ "data": [] })).is_empty());
        assert!(repos_from_search_page(&json!({})).is_empty());
        // Missing flags default to "has issues" and "not archived".
        assert_eq!(
            repos_from_search_page(&json!({
                "data": [{ "full_name": "a/b" }]
            })),
            vec!["a/b".to_owned()]
        );
    }

    // --- Integration-style tests against a local mock Forgejo API. -------------
    //
    // These drive the real `Poller` over HTTP so the request construction,
    // cursor handling, repository discovery, and error branches stay covered
    // without touching a live forge.

    use crate::agent::AgentRegistry;
    use crate::config::{AgentConfig, ForgejoConfig};
    use crate::forge_api::NoopForgeApi;
    use crate::session::{Dispatcher, SessionStore};

    #[derive(Clone)]
    enum MockReply {
        Json(Value),
        Status(u16),
    }

    #[derive(Default)]
    struct MockState {
        search: Mutex<Vec<Value>>,
        search_status: Mutex<Option<u16>>,
        comments: Mutex<HashMap<String, MockReply>>,
        requests: Mutex<Vec<String>>,
        pages: Mutex<HashMap<String, MockReply>>,
    }

    async fn mock_handler(
        axum::extract::State(state): axum::extract::State<Arc<MockState>>,
        method: axum::http::Method,
        uri: axum::http::Uri,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;

        state
            .requests
            .lock()
            .unwrap()
            .push(format!("{method} {uri}"));
        let path = uri.path().to_owned();

        let page_key = format!(
            "{}?page={}",
            path,
            uri.query()
                .and_then(|query| query.split('&').find_map(|pair| pair.strip_prefix("page=")))
                .unwrap_or("1")
        );
        if let Some(reply) = state.pages.lock().unwrap().get(&page_key).cloned() {
            return match reply {
                MockReply::Json(value) => axum::response::Json(value).into_response(),
                MockReply::Status(code) => axum::http::StatusCode::from_u16(code)
                    .unwrap()
                    .into_response(),
            };
        }
        if path.ends_with("/issues") || path.ends_with("/reviews") {
            return axum::response::Json(json!([])).into_response();
        }

        if path == "/api/v1/repos/search" {
            if let Some(code) = *state.search_status.lock().unwrap() {
                return axum::http::StatusCode::from_u16(code)
                    .unwrap()
                    .into_response();
            }
            let page = uri
                .query()
                .and_then(|query| query.split('&').find_map(|pair| pair.strip_prefix("page=")))
                .and_then(|page| page.parse::<usize>().ok())
                .unwrap_or(1);
            let body = state
                .search
                .lock()
                .unwrap()
                .get(page.saturating_sub(1))
                .cloned()
                .unwrap_or_else(|| json!({ "data": [] }));
            return axum::response::Json(body).into_response();
        }

        if let Some(repo) = path
            .strip_prefix("/api/v1/repos/")
            .and_then(|rest| rest.strip_suffix("/issues/comments"))
        {
            match state.comments.lock().unwrap().get(repo).cloned() {
                Some(MockReply::Json(value)) => {
                    return axum::response::Json(value).into_response();
                }
                Some(MockReply::Status(code)) => {
                    return axum::http::StatusCode::from_u16(code)
                        .unwrap()
                        .into_response();
                }
                None => return axum::http::StatusCode::NOT_FOUND.into_response(),
            }
        }

        axum::http::StatusCode::NOT_FOUND.into_response()
    }

    async fn start_mock(state: Arc<MockState>) -> String {
        let app = axum::Router::new().fallback(mock_handler).with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    fn test_config(dir: &std::path::Path, base_url: &str) -> Config {
        let mut config = Config::default();
        config.policy.allow_all = true;
        config.workspace.enabled = false;
        config.reply.ack = false;
        config.reply.result = false;
        config.session.dir = dir.to_path_buf();
        config.session.workers = 1;
        config.agent_sequence = vec!["custom".into()];
        config.forges.forgejo = Some(ForgejoConfig {
            base_url: base_url.to_owned(),
            token: Some("tok".into()),
            webhook_secret: None,
            bot_username: Some("shylock-bot".into()),
            ..Default::default()
        });
        // Every run belongs to a configured user; tests spawn directly because
        // the cgroup backend needs root.
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
        config.executor.set_direct_for_tests();
        config.users.insert(
            "default".into(),
            crate::config::UserConfig {
                role: crate::config::UserRole::Default,
                host_user: "agent".into(),
                agent: None,
                agent_model: None,
                token: None,
            },
        );
        config.agents.overrides.insert(
            "custom".into(),
            AgentConfig {
                command: Some("cat".into()),
                ..Default::default()
            },
        );
        config
    }

    fn build_poller(config: Config) -> (Poller, Arc<SessionStore>) {
        let config = Arc::new(config);
        let sessions = Arc::new(SessionStore::open(&config.session.dir).unwrap());
        let dispatcher = Dispatcher::new(
            config.clone(),
            Arc::new(AgentRegistry::from_config(&config)),
            sessions.clone(),
            Arc::new(NoopForgeApi),
            crate::build_policy(&config),
        )
        .unwrap();
        (Poller::new(config, dispatcher).unwrap(), sessions)
    }

    fn comment(id: i64, body: &str, author: &str) -> Value {
        json!({
            "id": id,
            "body": body,
            "html_url": format!("http://forge.local:3000/o/r/issues/3#issuecomment-{id}"),
            "issue_url": "http://forge.local:3000/o/r/issues/3",
            "pull_request_url": "",
            "user": { "login": author },
            "created_at": "2026-09-24T20:49:34+08:00"
        })
    }

    fn review(id: i64, state: &str, submitted: &str) -> Value {
        json!({
            "id": id, "state": state, "submitted_at": submitted, "updated_at": submitted,
            "body": "@shylock-bot fix the blocking review finding",
            "user": {"login": "shylock-reviewer"},
            "html_url": format!("http://forge.local:3000/o/r/pulls/159#issuecomment-{}", id + 13000)
        })
    }

    #[tokio::test]
    async fn polls_review_bodies_with_pagination_and_restart_dedupe() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(MockState::default());
        let submitted = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
        state.pages.lock().unwrap().extend([
            (
                "/api/v1/repos/o/r/issues?page=1".into(),
                MockReply::Json(json!([
                    {"number": 159, "title": "test PR", "body": "Fixes #156"}
                ])),
            ),
            (
                "/api/v1/repos/o/r/pulls/159/reviews?page=1".into(),
                MockReply::Json(json!([review(397, "PENDING", &submitted)])),
            ),
            (
                "/api/v1/repos/o/r/pulls/159/reviews?page=2".into(),
                MockReply::Json(json!([review(398, "REQUEST_CHANGES", &submitted)])),
            ),
        ]);
        let base = start_mock(state.clone()).await;
        let mut config = test_config(dir.path(), &base);
        config.poller.repositories = vec!["o/r".into()];
        config.poller.page_limit = 1;
        // Match the peer handoff in the reported failure.
        let mut reviewer = config.users["default"].clone();
        reviewer.role = crate::config::UserRole::Reviewer;
        config.users.insert("shylock-reviewer".into(), reviewer);
        let (poller, sessions) = build_poller(config.clone());
        poller.tick().await.unwrap();
        for _ in 0..200 {
            if sessions
                .get("user:default:forgejo:o/r:pr:159")
                .is_some_and(|s| s.runs.len() == 1 && s.runs[0].success == Some(true))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let session = sessions.get("user:default:forgejo:o/r:pr:159").unwrap();
        assert_eq!(session.runs.len(), 1);
        assert!(
            session.runs[0]
                .summary
                .as_deref()
                .unwrap()
                .contains("blocking review finding")
        );
        assert_eq!(session.runs[0].success, Some(true));
        let requests = state.requests.lock().unwrap().clone();
        assert!(
            requests.iter().any(|r| r.contains("type=pulls")
                && r.contains("state=all")
                && r.contains("since="))
        );
        assert!(
            requests
                .iter()
                .any(|r| r.contains("reviews?limit=1&page=3"))
        );

        let restarted = Poller::new(Arc::new(config), poller.dispatcher.clone()).unwrap();
        restarted.tick().await.unwrap();
        assert_eq!(
            sessions
                .get("user:default:forgejo:o/r:pr:159")
                .unwrap()
                .runs
                .len(),
            1
        );
        assert!(sessions.pending_jobs().unwrap().is_empty());

        // Submitting a pending review preserves its creation timestamp.
        let mut late_review = review(397, "COMMENT", &submitted);
        late_review["updated_at"] = json!(Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true));
        state.pages.lock().unwrap().insert(
            "/api/v1/repos/o/r/pulls/159/reviews?page=1".into(),
            MockReply::Json(json!([late_review])),
        );
        restarted.tick().await.unwrap();
        for _ in 0..200 {
            if sessions
                .get("user:default:forgejo:o/r:pr:159")
                .unwrap()
                .runs
                .len()
                == 2
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            sessions
                .get("user:default:forgejo:o/r:pr:159")
                .unwrap()
                .runs
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn late_pending_review_uses_updated_time_and_dispatches_once_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(MockState::default());
        let created =
            (Utc::now() - chrono::Duration::hours(1)).to_rfc3339_opts(SecondsFormat::Secs, true);
        let mut pending = review(397, "PENDING", &created);
        pending["updated_at"] = json!(created);
        state.pages.lock().unwrap().extend([
            (
                "/api/v1/repos/o/r/issues?page=1".into(),
                MockReply::Json(json!([{"number": 159}])),
            ),
            (
                "/api/v1/repos/o/r/pulls/159/reviews?page=1".into(),
                MockReply::Json(json!([pending])),
            ),
        ]);
        let base = start_mock(state.clone()).await;
        let mut config = test_config(dir.path(), &base);
        config.poller.repositories = vec!["o/r".into()];
        let mut reviewer = config.users["default"].clone();
        reviewer.role = crate::config::UserRole::Reviewer;
        config.users.insert("shylock-reviewer".into(), reviewer);
        let (poller, sessions) = build_poller(config.clone());
        poller.tick().await.unwrap();
        assert!(sessions.get("user:default:forgejo:o/r:pr:159").is_none());
        assert!(created < poller.cursor("o/r:reviews").last_time.unwrap());

        // Forgejo preserves CreatedUnix/submitted_at when submitting a pending
        // review; only its state, content and UpdatedUnix/updated_at advance.
        let updated = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
        assert!(updated > poller.cursor("o/r:reviews").last_time.unwrap());
        let mut submitted = review(397, "REQUEST_CHANGES", &created);
        submitted["updated_at"] = json!(updated);
        state.pages.lock().unwrap().insert(
            "/api/v1/repos/o/r/pulls/159/reviews?page=1".into(),
            MockReply::Json(json!([submitted])),
        );
        poller.tick().await.unwrap();
        for _ in 0..200 {
            if sessions
                .get("user:default:forgejo:o/r:pr:159")
                .is_some_and(|s| s.runs.len() == 1 && s.runs[0].success == Some(true))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let session = sessions
            .get("user:default:forgejo:o/r:pr:159")
            .expect("the old pending review must dispatch on submission");
        assert_eq!(session.runs.len(), 1);
        assert_eq!(session.runs[0].success, Some(true));
        assert!(
            session.runs[0]
                .summary
                .as_deref()
                .unwrap()
                .contains("blocking review finding")
        );

        poller.tick().await.unwrap();
        let restarted = Poller::new(Arc::new(config), poller.dispatcher.clone()).unwrap();
        restarted.tick().await.unwrap();
        restarted.tick().await.unwrap();
        assert!(sessions.pending_jobs().unwrap().is_empty());
        assert_eq!(
            sessions
                .get("user:default:forgejo:o/r:pr:159")
                .unwrap()
                .runs
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn review_api_failure_keeps_cursor_for_retry() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(MockState::default());
        state.pages.lock().unwrap().extend([
            (
                "/api/v1/repos/o/r/issues?page=1".into(),
                MockReply::Json(json!([{"number": 159}])),
            ),
            (
                "/api/v1/repos/o/r/pulls/159/reviews?page=1".into(),
                MockReply::Status(500),
            ),
        ]);
        let base = start_mock(state).await;
        let (poller, _) = build_poller(test_config(dir.path(), &base));
        assert!(
            poller
                .poll_reviews(&base, Some("tok"), "o/r")
                .await
                .is_err()
        );
        assert!(poller.cursor("o/r:reviews").last_time.is_none());
    }

    #[test]
    fn review_messages_preserve_author_and_pr_context() {
        let pull = json!({"number": 159, "title": "test", "body": "Fixes #156"});
        for (state, event) in [
            ("APPROVED", "pull_request_approved"),
            ("COMMENT", "pull_request_comment"),
            ("REQUEST_CHANGES", "pull_request_rejected"),
        ] {
            let message =
                message_from_review("o/r", &pull, &review(398, state, "2026-10-01T15:52:36Z"))
                    .unwrap();
            assert_eq!(message.author, "shylock-reviewer");
            assert_eq!(message.number, Some(159));
            assert_eq!(message.comment_id, None);
            assert_eq!(message.event, event);
            assert_eq!(message.title.as_deref(), Some("test"));
            assert_eq!(message.linked_issue.unwrap().number, 156);
            assert!(message.is_pull_request);
        }
        assert!(message_from_review("o/r", &pull, &review(1, "PENDING", "")).is_none());
    }

    #[tokio::test]
    async fn polls_explicit_repositories_and_persists_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(MockState::default());
        state.comments.lock().unwrap().insert(
            "o/r".into(),
            MockReply::Json(json!([comment(1, "@shylock-bot go", "alice")])),
        );
        let base = start_mock(state.clone()).await;

        let mut config = test_config(dir.path(), &base);
        config.poller.repositories = vec!["o/r".into()];
        let (poller, _sessions) = build_poller(config);

        poller.tick().await.unwrap();

        assert!(
            state
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.starts_with("GET /api/v1/repos/o/r/issues/comments?"))
        );

        let raw = std::fs::read_to_string(poller.state_path.clone()).unwrap();
        let cursors: HashMap<String, Cursor> = serde_json::from_str(&raw).unwrap();
        assert_eq!(cursors["o/r"].last_id, 1);
        assert_eq!(
            cursors["o/r"].last_time.as_deref(),
            Some("2026-09-24T12:49:34Z")
        );
    }

    #[tokio::test]
    async fn since_query_reuses_the_stored_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(MockState::default());
        state
            .comments
            .lock()
            .unwrap()
            .insert("o/r".into(), MockReply::Json(json!([])));
        let base = start_mock(state.clone()).await;

        let mut config = test_config(dir.path(), &base);
        config.poller.repositories = vec!["o/r".into()];
        let (poller, _sessions) = build_poller(config);
        poller.set_cursor(
            "o/r",
            Cursor {
                last_id: 7,
                last_time: Some("2026-09-24T12:00:00Z".into()),
                ..Default::default()
            },
        );

        poller.tick().await.unwrap();

        let requests = state.requests.lock().unwrap();
        let comments = requests
            .iter()
            .find(|request| request.contains("/repos/o/r/issues/comments"))
            .expect("comments request");
        assert!(
            comments.contains("since=2026-09-24T12%3A00%3A00Z")
                || comments.contains("since=2026-09-24T12:00:00Z"),
            "unexpected since: {comments}"
        );
    }

    #[tokio::test]
    async fn discovers_repositories_and_uses_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(MockState::default());
        *state.search.lock().unwrap() = vec![
            json!({"data": [
                {"full_name": "a/one", "has_issues": true, "archived": false},
                {"full_name": "a/skip", "has_issues": false, "archived": false}
            ]}),
            json!({"data": [
                {"full_name": "a/two", "has_issues": true, "archived": false}
            ]}),
        ];
        for repo in ["a/one", "a/two"] {
            state
                .comments
                .lock()
                .unwrap()
                .insert(repo.into(), MockReply::Json(json!([])));
        }
        let base = start_mock(state.clone()).await;

        let mut config = test_config(dir.path(), &base);
        config.poller.page_limit = 2;
        config.poller.discover_interval_secs = 300;
        let (poller, _sessions) = build_poller(config);

        poller.tick().await.unwrap();
        let searches = state
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.starts_with("GET /api/v1/repos/search"))
            .count();
        assert_eq!(searches, 2, "a full page must be followed by a second page");
        assert!(
            state
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.contains("/repos/a/one/issues/comments"))
        );
        assert!(
            !state
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.contains("/repos/a/skip/issues/comments")),
            "repositories without issues must be skipped"
        );

        // The second tick is served from the discovery cache.
        let before = state.requests.lock().unwrap().len();
        poller.tick().await.unwrap();
        let searches = state
            .requests
            .lock()
            .unwrap()
            .iter()
            .skip(before)
            .filter(|request| request.starts_with("GET /api/v1/repos/search"))
            .count();
        assert_eq!(searches, 0);
    }

    #[tokio::test]
    async fn discovery_failure_without_cache_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(MockState::default());
        *state.search_status.lock().unwrap() = Some(500);
        let base = start_mock(state).await;

        let config = test_config(dir.path(), &base);
        let (poller, _sessions) = build_poller(config);
        assert!(poller.tick().await.is_err());
    }

    #[tokio::test]
    async fn discovery_failure_keeps_the_cached_list() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(MockState::default());
        *state.search.lock().unwrap() = vec![json!({"data": [
            {"full_name": "a/one", "has_issues": true, "archived": false}
        ]})];
        state
            .comments
            .lock()
            .unwrap()
            .insert("a/one".into(), MockReply::Json(json!([])));
        let base = start_mock(state.clone()).await;

        let mut config = test_config(dir.path(), &base);
        config.poller.page_limit = 2;
        config.poller.discover_interval_secs = 1;
        let (poller, _sessions) = build_poller(config);
        poller.tick().await.unwrap();

        // Force the cache stale and make discovery fail.
        {
            let mut cache = poller.repos.lock().unwrap();
            cache.refreshed = Some(Instant::now() - Duration::from_secs(60));
        }
        *state.search_status.lock().unwrap() = Some(500);

        poller.tick().await.unwrap();
        assert!(
            state
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request.contains("/repos/a/one/issues/comments"))
                .count()
                >= 2,
            "the cached repository is still polled"
        );
    }

    #[tokio::test]
    async fn inaccessible_and_failing_repositories_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(MockState::default());
        {
            let mut comments = state.comments.lock().unwrap();
            comments.insert("o/forbidden".into(), MockReply::Status(403));
            comments.insert("o/missing".into(), MockReply::Status(404));
            comments.insert("o/unauthorized".into(), MockReply::Status(401));
            comments.insert("o/error".into(), MockReply::Status(500));
            comments.insert("o/ok".into(), MockReply::Json(json!([])));
        }
        let base = start_mock(state.clone()).await;

        let mut config = test_config(dir.path(), &base);
        config.poller.repositories = vec![
            "o/forbidden".into(),
            "o/missing".into(),
            "o/unauthorized".into(),
            "o/error".into(),
            "o/ok".into(),
        ];
        let (poller, _sessions) = build_poller(config);

        // A failing repo is logged and the sweep continues.
        poller.tick().await.unwrap();
        assert!(
            state
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.contains("/repos/o/ok/issues/comments"))
        );
    }

    #[tokio::test]
    async fn poll_ignores_own_comments_unmentioned_and_unknown_agents() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(MockState::default());
        state.comments.lock().unwrap().insert(
            "o/r".into(),
            MockReply::Json(json!([
                comment(1, "@shylock-bot --agent=custom go", "shylock-bot"),
                comment(2, "just chatting", "alice"),
                comment(3, "@shylock-bot --agent=missing go", "alice"),
                comment(4, "@shylock-bot --agent=custom go", "alice")
            ])),
        );
        let base = start_mock(state).await;

        let mut config = test_config(dir.path(), &base);
        config.poller.repositories = vec!["o/r".into()];
        let (poller, sessions) = build_poller(config);

        poller.tick().await.unwrap();

        // Only the fourth comment is a valid trigger for an existing agent.
        for _ in 0..200 {
            if sessions.pending_jobs().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let key = "user:default:forgejo:o/r:issue:3";
        let session = sessions
            .get(key)
            .expect("the valid mention creates a session");
        assert_eq!(session.runs.len(), 1);
    }

    #[tokio::test]
    async fn unauthorized_mentions_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(MockState::default());
        state.comments.lock().unwrap().insert(
            "o/r".into(),
            MockReply::Json(json!([comment(
                1,
                "@shylock-bot --agent=custom go",
                "alice"
            )])),
        );
        let base = start_mock(state).await;

        let mut config = test_config(dir.path(), &base);
        config.policy.allow_all = false;
        config.poller.repositories = vec!["o/r".into()];
        let (poller, sessions) = build_poller(config);

        poller.tick().await.unwrap();
        assert!(sessions.pending_jobs().unwrap().is_empty());
        assert!(sessions.get("user:default:forgejo:o/r:issue:3").is_none());
    }

    #[tokio::test]
    async fn poll_failure_is_logged_by_tick() {
        // `tick` swallows per-repository failures; a 500 must not abort the run.
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(MockState::default());
        state
            .comments
            .lock()
            .unwrap()
            .insert("o/r".into(), MockReply::Status(500));
        let base = start_mock(state).await;

        let mut config = test_config(dir.path(), &base);
        config.poller.repositories = vec!["o/r".into()];
        let (poller, _sessions) = build_poller(config);
        poller.tick().await.unwrap();
    }

    #[tokio::test]
    async fn persist_failure_is_non_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(MockState::default());
        state
            .comments
            .lock()
            .unwrap()
            .insert("o/r".into(), MockReply::Json(json!([])));
        let base = start_mock(state).await;

        let mut config = test_config(dir.path(), &base);
        config.poller.repositories = vec!["o/r".into()];
        let (mut poller, _sessions) = build_poller(config);
        poller.state_path = PathBuf::from("/proc/does-not-exist/poller.json");
        poller.tick().await.unwrap();
    }

    #[tokio::test]
    async fn run_loop_ticks_until_aborted() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(MockState::default());
        state
            .comments
            .lock()
            .unwrap()
            .insert("o/r".into(), MockReply::Json(json!([])));
        let base = start_mock(state.clone()).await;

        let mut config = test_config(dir.path(), &base);
        config.poller.repositories = vec!["o/r".into()];
        config.poller.interval_secs = 1;
        let (poller, _sessions) = build_poller(config);

        let handle = tokio::spawn(Arc::new(poller).run());
        for _ in 0..200 {
            if state
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.contains("/repos/o/r/issues/comments"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        handle.abort();
        assert!(
            state
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.contains("/repos/o/r/issues/comments"))
        );
    }

    #[tokio::test]
    async fn poll_records_human_mentions_before_routing_ignores() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(MockState::default());
        state.comments.lock().unwrap().insert(
            "o/r".into(),
            MockReply::Json(json!([comment(
                1,
                "I need a person. @alice please help.",
                "shylock-bot"
            )])),
        );
        let base = start_mock(state).await;

        let mut config = test_config(dir.path(), &base);
        config.poller.repositories = vec!["o/r".into()];
        // The agent author is ignored for routing; the human request must still
        // be recorded before that check.
        config.policy.ignored_users = vec!["shylock-bot".into()];
        config.users.insert(
            "alice".into(),
            crate::config::UserConfig {
                role: crate::config::UserRole::Human,
                host_user: String::new(),
                agent: None,
                agent_model: None,
                token: None,
            },
        );
        let (poller, _sessions) = build_poller(config);

        poller.tick().await.unwrap();

        let recorded = poller.dispatcher.notifier().since("alice", 0, None);
        assert_eq!(recorded.len(), 1, "polling must record the human mention");
        assert_eq!(recorded[0].author, "shylock-bot");

        // The webhook ingester shares the dispatcher's recorder and dedupe, so
        // the same comment seen by both ingesters notifies only once.
        let message = message_from_comment(
            "o/r",
            &comment(1, "I need a person. @alice please help.", "shylock-bot"),
        )
        .unwrap();
        poller.dispatcher.record_human_notifications(&message);
        assert_eq!(
            poller.dispatcher.notifier().since("alice", 0, None).len(),
            1
        );
    }
}

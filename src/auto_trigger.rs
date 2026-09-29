//! Signed Forgejo events that start an agent without a mention.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::Value;
use url::Url;

use crate::config::Config;
use crate::error::{BotError, Result};
use crate::forge::{ForgeMessage, ReplyTarget, linked_issue_ref};
use crate::location::ForgeKind;
use crate::mention::Mention;
use crate::session::Dispatcher;

const MAX_SEEN: usize = 4096;

/// Forgejo computes pull-request mergeability asynchronously. Right after a
/// push it reports `mergeable: false` while the conflict check is still queued
/// (or while it is running), so an event delivered during that window must not
/// be mistaken for a real conflict. Re-check the candidates for a short,
/// bounded window: a clean merge settles to `true`, a conflict stays `false`.
/// The total wait stays well under Forgejo's default 5s webhook timeout.
const CONFLICT_CHECK_ATTEMPTS: usize = 4;
const CONFLICT_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// Synthetic author recorded on messages that a signed forge event starts
/// without a user mention. The queue uses it to word its acknowledgement as an
/// automatic trigger instead of implying a human mentioned the bot.
pub(crate) const AUTO_TRIGGER_AUTHOR: &str = "forgejo-event";

pub(crate) struct AutoTrigger {
    client: reqwest::Client,
    path: PathBuf,
    seen: Mutex<Seen>,
}

#[derive(Default)]
struct Seen {
    order: VecDeque<String>,
    keys: HashSet<String>,
}

impl Seen {
    fn insert(&mut self, key: String) -> bool {
        if !self.keys.insert(key.clone()) {
            return false;
        }
        self.order.push_back(key);
        while self.order.len() > MAX_SEEN {
            if let Some(old) = self.order.pop_front() {
                self.keys.remove(&old);
            }
        }
        true
    }

    fn remove(&mut self, key: &str) {
        self.keys.remove(key);
        self.order.retain(|entry| entry != key);
    }
}

impl AutoTrigger {
    pub(crate) fn new(config: &Config) -> Self {
        let path = crate::config::expand_tilde(&config.session.dir).join("auto-triggers.json");
        let mut seen = Seen::default();
        if let Ok(raw) = std::fs::read(&path) {
            match serde_json::from_slice::<Vec<String>>(&raw) {
                Ok(keys) => {
                    for key in keys {
                        seen.insert(key);
                    }
                }
                Err(error) => tracing::warn!(%error, "ignoring invalid auto-trigger state"),
            }
        }
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .expect("HTTP client configuration is valid"),
            path,
            seen: Mutex::new(seen),
        }
    }

    pub(crate) async fn handle(
        &self,
        config: &Config,
        dispatcher: &Dispatcher,
        event: &str,
        body: &[u8],
    ) -> Result<usize> {
        if !matches!(event, "action_run_failure" | "pull_request" | "push") {
            return Ok(0);
        }
        let payload: Value = serde_json::from_slice(body)?;
        let repo = if event == "action_run_failure" {
            payload.pointer("/run/repository/full_name")
        } else {
            payload.pointer("/repository/full_name")
        }
        .and_then(Value::as_str)
        .unwrap_or_default();
        if repo.is_empty() {
            return Ok(0);
        }
        let Some(forgejo) = config.forges.forgejo.as_ref() else {
            return Ok(0);
        };
        // Mentionless execution requires an authenticated delivery.
        if forgejo.webhook_secret.is_none() {
            return Ok(0);
        }
        let Some(token) = forgejo.token.as_deref() else {
            return Err(BotError::Config(
                "automatic Forgejo triggers require a Forgejo API token".into(),
            ));
        };
        let base = forgejo.base_url.trim_end_matches('/');
        match event {
            "action_run_failure" => {
                self.ci_failure(base, token, repo, &payload, dispatcher)
                    .await
            }
            "pull_request" | "push" => {
                self.conflicts(base, token, repo, event, &payload, dispatcher)
                    .await
            }
            _ => Ok(0),
        }
    }

    async fn ci_failure(
        &self,
        base: &str,
        token: &str,
        repo: &str,
        payload: &Value,
        dispatcher: &Dispatcher,
    ) -> Result<usize> {
        let Some(run) = payload.get("run") else {
            return Ok(0);
        };
        let (Some(run_id), Some(run_sha)) = (
            run.get("id").and_then(Value::as_i64),
            run.get("commit_sha").and_then(Value::as_str),
        ) else {
            return Ok(0);
        };
        let trigger: Value = run
            .get("event_payload")
            .and_then(Value::as_str)
            .and_then(|raw| serde_json::from_str(raw).ok())
            .unwrap_or(Value::Null);
        let number = trigger
            .pointer("/pull_request/number")
            .and_then(Value::as_u64);
        let candidates = if let Some(number) = number {
            vec![number]
        } else {
            self.open_prs(base, token, repo)
                .await?
                .iter()
                .filter_map(|pr| pr.get("number").and_then(Value::as_u64))
                .collect()
        };
        let mut accepted = 0;
        for number in candidates {
            let pr = self.get_pr(base, token, repo, number).await?;
            if pr.get("state").and_then(Value::as_str) != Some("open") {
                continue;
            }
            let head = pr.pointer("/head/sha").and_then(Value::as_str);
            let event_head = trigger
                .pointer("/pull_request/head/sha")
                .and_then(Value::as_str);
            if head.is_none() || (head != Some(run_sha) && head != event_head) {
                continue; // A newer push already superseded this failure.
            }
            let key = format!("ci:{repo}:{run_id}:{number}");
            let run_url = run
                .get("html_url")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let instruction = format!(
                "Investigate and fix the failed Forgejo Actions run {run_id} for this pull request. Run: {run_url}. Verify the fix and update the branch."
            );
            let message =
                auto_message(base, repo, number, &pr, "action_run_failure", &instruction)?;
            accepted += self.submit(dispatcher, key, message, instruction).await?;
        }
        Ok(accepted)
    }

    async fn conflicts(
        &self,
        base: &str,
        token: &str,
        repo: &str,
        event: &str,
        payload: &Value,
        dispatcher: &Dispatcher,
    ) -> Result<usize> {
        let candidates = if event == "pull_request" {
            let action = payload
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !matches!(
                action,
                "opened" | "reopened" | "synchronized" | "synchronize" | "edited"
            ) {
                return Ok(0);
            }
            payload
                .pointer("/pull_request/number")
                .and_then(Value::as_u64)
                .into_iter()
                .collect::<Vec<_>>()
        } else {
            let Some(branch) = payload
                .get("ref")
                .and_then(Value::as_str)
                .and_then(|r| r.strip_prefix("refs/heads/"))
            else {
                return Ok(0);
            };
            self.open_prs(base, token, repo)
                .await?
                .iter()
                .filter(|pr| pr.pointer("/base/ref").and_then(Value::as_str) == Some(branch))
                .filter_map(|pr| pr.get("number").and_then(Value::as_u64))
                .collect()
        };
        // Forgejo's mergeability check is asynchronous: an event that arrives
        // right after a push sees `mergeable: false` even when the branch
        // merges cleanly. Collect the candidates that currently look
        // conflicting and give Forgejo a moment to finish before acting. A
        // candidate whose head/base pair was already handled is skipped
        // without waiting.
        let mut pending: Vec<(u64, Value)> = Vec::new();
        for number in candidates {
            let pr = self.get_pr(base, token, repo, number).await?;
            if !is_open_conflicting_candidate(&pr) {
                continue;
            }
            if conflict_key(repo, number, &pr).is_some_and(|key| self.is_seen(&key)) {
                continue;
            }
            pending.push((number, pr));
        }
        for _ in 0..CONFLICT_CHECK_ATTEMPTS {
            if pending.is_empty() {
                break;
            }
            tokio::time::sleep(CONFLICT_CHECK_INTERVAL).await;
            let mut next = Vec::with_capacity(pending.len());
            for (number, _) in pending {
                let pr = self.get_pr(base, token, repo, number).await?;
                if is_open_conflicting_candidate(&pr) {
                    next.push((number, pr));
                }
            }
            pending = next;
        }

        let mut accepted = 0;
        for (number, pr) in pending {
            let Some(key) = conflict_key(repo, number, &pr) else {
                continue;
            };
            // `mergeable: false` cannot distinguish a real conflict from a
            // check that is still running or has failed, and the bounded
            // settle window is not a guarantee. Ask Forgejo to update the
            // branch: a branch that merges cleanly is updated (or is already
            // current) without an agent, while a genuine conflict fails. Only
            // start an agent when Forgejo confirms the conflict, so a clean
            // branch never produces an agent run or a "no conflict" comment.
            match self.update_branch(base, token, repo, number).await {
                BranchUpdate::Clean => {
                    self.remember(&key)?;
                    continue;
                }
                BranchUpdate::Conflict => {}
                // Without an authoritative answer, keep the previous
                // behaviour and let the agent look.
                BranchUpdate::Unavailable => {}
            }
            let instruction = "Resolve the merge conflict between this pull request and its base branch. Verify the result and update the pull request branch.".to_owned();
            let message = auto_message(base, repo, number, &pr, "merge_conflict", &instruction)?;
            accepted += self.submit(dispatcher, key, message, instruction).await?;
        }
        Ok(accepted)
    }

    /// Ask Forgejo itself whether the branch still merges. Updating a clean
    /// branch is the work the agent would otherwise do; a conflicted branch
    /// returns `409` and is left to the agent.
    async fn update_branch(
        &self,
        base: &str,
        token: &str,
        repo: &str,
        number: u64,
    ) -> BranchUpdate {
        let url = format!("{base}/api/v1/repos/{repo}/pulls/{number}/update");
        let response = match self
            .client
            .post(&url)
            .header("Authorization", format!("token {token}"))
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!(%error, pr = number, "could not verify merge conflict");
                return BranchUpdate::Unavailable;
            }
        };
        match response.status() {
            reqwest::StatusCode::OK => BranchUpdate::Clean,
            reqwest::StatusCode::CONFLICT => BranchUpdate::Conflict,
            status => {
                tracing::warn!(%status, pr = number, "could not verify merge conflict");
                BranchUpdate::Unavailable
            }
        }
    }

    /// Record `key` as handled and persist the seen set. Returns `false` when
    /// the key was already handled.
    fn remember(&self, key: &str) -> Result<bool> {
        {
            let mut seen = self.seen.lock().expect("auto-trigger state poisoned");
            if !seen.insert(key.to_owned()) {
                return Ok(false);
            }
        }
        self.persist_seen()?;
        Ok(true)
    }

    fn forget(&self, key: &str) {
        self.seen
            .lock()
            .expect("auto-trigger state poisoned")
            .remove(key);
    }

    fn persist_seen(&self) -> Result<()> {
        let seen = self.seen.lock().expect("auto-trigger state poisoned");
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec(&seen.order)?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(tmp, &self.path)?;
        Ok(())
    }

    async fn submit(
        &self,
        dispatcher: &Dispatcher,
        key: String,
        message: ForgeMessage,
        instruction: String,
    ) -> Result<usize> {
        if !self.remember(&key)? {
            return Ok(0);
        }
        let mention = Mention {
            agent: None,
            message: instruction,
        };
        let result = dispatcher
            .submit_auto(message, mention, dispatcher.default_agent_name())
            .await;
        match result {
            Ok(_) => Ok(1),
            Err(error) => {
                self.forget(&key);
                Err(error)
            }
        }
    }

    fn is_seen(&self, key: &str) -> bool {
        self.seen
            .lock()
            .expect("auto-trigger state poisoned")
            .keys
            .contains(key)
    }

    async fn get_pr(&self, base: &str, token: &str, repo: &str, number: u64) -> Result<Value> {
        self.get_json(&format!("{base}/api/v1/repos/{repo}/pulls/{number}"), token)
            .await
    }

    async fn open_prs(&self, base: &str, token: &str, repo: &str) -> Result<Vec<Value>> {
        let mut all = Vec::new();
        for page in 1..=100 {
            let url = format!("{base}/api/v1/repos/{repo}/pulls?state=open&limit=100&page={page}");
            let page_items: Vec<Value> = self.get_json(&url, token).await?;
            let done = page_items.len() < 100;
            all.extend(page_items);
            if done {
                break;
            }
        }
        Ok(all)
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str, token: &str) -> Result<T> {
        Ok(self
            .client
            .get(url)
            .header("Authorization", format!("token {token}"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }
}

/// Outcome of asking Forgejo to merge the base branch into the head branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BranchUpdate {
    /// The branch merges cleanly: Forgejo updated it or it was already
    /// current. There is no conflict to resolve.
    Clean,
    /// Forgejo reports a real conflict while updating the branch.
    Conflict,
    /// The conflict could not be confirmed (permission error, transient
    /// failure, or an older Forgejo without the endpoint).
    Unavailable,
}

/// Whether the pull request is a candidate for automatic conflict
/// resolution: still open, not a draft, and currently reporting
/// `mergeable: false`.
///
/// `mergeable: false` is also returned while Forgejo's conflict check is in
/// progress or has failed, so callers must let the value settle before
/// treating it as a genuine conflict.
fn is_open_conflicting_candidate(pr: &Value) -> bool {
    pr.get("state").and_then(Value::as_str) == Some("open")
        && pr.get("draft").and_then(Value::as_bool) != Some(true)
        && pr.get("mergeable").and_then(Value::as_bool) == Some(false)
}

/// Stable dedupe key for a conflict trigger: the PR and the exact head/base
/// commit pair that was checked. A later push to either branch produces a new
/// key and is eligible again.
fn conflict_key(repo: &str, number: u64, pr: &Value) -> Option<String> {
    let head = pr.pointer("/head/sha").and_then(Value::as_str)?;
    let base_sha = pr.pointer("/base/sha").and_then(Value::as_str)?;
    Some(format!("conflict:{repo}:{number}:{head}:{base_sha}"))
}

fn auto_message(
    base: &str,
    repo: &str,
    number: u64,
    pr: &Value,
    event: &str,
    instruction: &str,
) -> Result<ForgeMessage> {
    // Prefer the public `html_url` the API returns so every user-facing link
    // (the status page, the agent prompt) points at the web UI rather than the
    // internal API `base_url`.
    let location_text = pr
        .get("html_url")
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{base}/{repo}/pulls/{number}"));
    let location = Url::parse(&location_text).map_err(|error| BotError::InvalidLocation {
        location: location_text,
        reason: error.to_string(),
    })?;
    let body = pr.get("body").and_then(Value::as_str).unwrap_or_default();
    Ok(ForgeMessage {
        forge: ForgeKind::Forgejo,
        location,
        body: instruction.to_owned(),
        author: AUTO_TRIGGER_AUTHOR.into(),
        repository: repo.to_owned(),
        comment_id: None,
        number: Some(number),
        is_pull_request: true,
        linked_issue: linked_issue_ref(body),
        event: event.to_owned(),
        title: pr.get("title").and_then(Value::as_str).map(str::to_owned),
        reply_target: ReplyTarget::Conversation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn auto_message_prefers_the_public_html_url() {
        let pr = json!({
            "html_url": "https://forgejo.example.com/o/r/pulls/7",
            "title": "t",
            "body": "Fixes #3",
        });
        let message = auto_message(
            "http://127.0.0.1:3000",
            "o/r",
            7,
            &pr,
            "merge_conflict",
            "resolve",
        )
        .unwrap();
        assert_eq!(
            message.location.as_str(),
            "https://forgejo.example.com/o/r/pulls/7"
        );
    }

    #[test]
    fn auto_message_falls_back_to_the_api_base() {
        let pr = json!({ "title": "t" });
        let message = auto_message(
            "http://127.0.0.1:3000",
            "o/r",
            7,
            &pr,
            "merge_conflict",
            "resolve",
        )
        .unwrap();
        assert_eq!(
            message.location.as_str(),
            "http://127.0.0.1:3000/o/r/pulls/7"
        );
    }

    #[test]
    fn conflicting_candidate_requires_open_undrafted_false_mergeable() {
        let base = json!({
            "state": "open",
            "draft": false,
            "mergeable": false,
            "head": {"sha": "h"},
            "base": {"sha": "b"},
        });
        assert!(is_open_conflicting_candidate(&base));

        for (key, value) in [
            ("state", json!("closed")),
            ("draft", json!(true)),
            ("mergeable", json!(true)),
            ("mergeable", Value::Null),
        ] {
            let mut pr = base.clone();
            pr[key] = value;
            assert!(
                !is_open_conflicting_candidate(&pr),
                "{key} = {pr} must not be a candidate"
            );
        }
    }

    #[test]
    fn conflict_key_tracks_the_checked_commit_pair() {
        let pr = json!({"head": {"sha": "h1"}, "base": {"sha": "b1"}});
        assert_eq!(
            conflict_key("o/r", 7, &pr).as_deref(),
            Some("conflict:o/r:7:h1:b1")
        );
        assert!(conflict_key("o/r", 7, &json!({"head": {"sha": "h1"}})).is_none());
    }
}

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
        let mut accepted = 0;
        for number in candidates {
            let pr = self.get_pr(base, token, repo, number).await?;
            if pr.get("state").and_then(Value::as_str) != Some("open")
                || pr.get("draft").and_then(Value::as_bool) == Some(true)
                || pr.get("mergeable").and_then(Value::as_bool) != Some(false)
            {
                continue;
            }
            let (Some(head), Some(base_sha)) = (
                pr.pointer("/head/sha").and_then(Value::as_str),
                pr.pointer("/base/sha").and_then(Value::as_str),
            ) else {
                continue;
            };
            let key = format!("conflict:{repo}:{number}:{head}:{base_sha}");
            let instruction = "Resolve the merge conflict between this pull request and its base branch. Verify the result and update the pull request branch.".to_owned();
            let message = auto_message(base, repo, number, &pr, "merge_conflict", &instruction)?;
            accepted += self.submit(dispatcher, key, message, instruction).await?;
        }
        Ok(accepted)
    }

    async fn submit(
        &self,
        dispatcher: &Dispatcher,
        key: String,
        message: ForgeMessage,
        instruction: String,
    ) -> Result<usize> {
        {
            let mut seen = self.seen.lock().expect("auto-trigger state poisoned");
            if !seen.insert(key.clone()) {
                return Ok(0);
            }
        }
        let mention = Mention {
            agent: None,
            message: instruction,
        };
        let result = dispatcher
            .submit_auto(message, mention, dispatcher.default_agent_name())
            .await;
        match result {
            Ok(_) => {
                let seen = self.seen.lock().expect("auto-trigger state poisoned");
                if let Some(parent) = self.path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let bytes = serde_json::to_vec(&seen.order)?;
                let tmp = self.path.with_extension("json.tmp");
                std::fs::write(&tmp, bytes)?;
                std::fs::rename(tmp, &self.path)?;
                Ok(1)
            }
            Err(error) => {
                self.seen
                    .lock()
                    .expect("auto-trigger state poisoned")
                    .remove(&key);
                Err(error)
            }
        }
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

fn auto_message(
    base: &str,
    repo: &str,
    number: u64,
    pr: &Value,
    event: &str,
    instruction: &str,
) -> Result<ForgeMessage> {
    let location_text = format!("{base}/{repo}/pulls/{number}");
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

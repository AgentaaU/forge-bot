//! On-disk session and job persistence.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::agent::{AgentOutcome, LiveOutput, TokenUsage};
use crate::error::Result;
use crate::forge::ForgeMessage;
use crate::session::Job;

/// One execution of an agent inside a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub job_id: Uuid,
    pub agent: String,
    /// The instruction from the triggering mention. Older records omit it.
    #[serde(default)]
    pub message: Option<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub success: Option<bool>,
    pub summary: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// Effective reasoning effort, absent in older records.
    #[serde(default)]
    pub effort: Option<String>,
    /// Provider prompt-cache accounting, when the adapter reported it.
    #[serde(default)]
    pub cache: Option<TokenUsage>,
}

impl RunRecord {
    /// Close a run that can no longer be executing. Used when a restart finds a
    /// run still marked in flight: the process that owned it is gone, so it can
    /// never finish on its own.
    fn interrupt(&mut self, at: DateTime<Utc>) {
        self.finished_at = Some(at);
        self.success = Some(false);
        if self.summary.is_none() {
            self.summary = Some(INTERRUPTED_SUMMARY.to_owned());
        }
    }
}

/// Summary stored on a run that a restart interrupted before it finished.
const INTERRUPTED_SUMMARY: &str = "Run interrupted before it finished (the bot restarted).";

/// A conversation with the bot about one forge object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub key: String,
    pub repository: String,
    pub location: String,
    pub agent: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub runs: Vec<RunRecord>,
}

impl Session {
    /// Whether the newest run is still in flight.
    ///
    /// A session runs at most one job at a time, so only the last record can be
    /// unfinished. Checking the newest run (rather than any run) keeps the
    /// status page honest even if older records were left open by an earlier
    /// process.
    pub fn is_running(&self) -> bool {
        self.runs
            .last()
            .is_some_and(|run| run.finished_at.is_none())
    }
}

/// File (under the store directory) holding the operator's per-user agent
/// overrides set from `/admin`.
const USER_SETTINGS_FILE: &str = "agent-settings.json";

/// Operator overrides for one configured agent user, set from `/admin`. A
/// `None` field falls back to the user's configuration (`agent_model`) or to
/// the agent's own default (effort).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserAgentSettings {
    /// Adapter owning these overrides. Older records without ownership are
    /// ignored by runs until an operator saves settings with an owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Model ID that replaces the user's `agent_model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Reasoning effort level for the user's configured agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

impl UserAgentSettings {
    /// Whether no override is set.
    pub fn is_empty(&self) -> bool {
        self.model.is_none() && self.effort.is_none()
    }
}

/// Explicit adapter/model choice shared by an issue and its linked PRs.
/// Reported models and capacity fallbacks never change this preference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationSelection {
    pub agent: String,
    pub model: Option<String>,
}

const CONVERSATION_SELECTIONS_FILE: &str = "conversation-selections.json";

/// Persists sessions and pending jobs under a directory.
pub struct SessionStore {
    dir: PathBuf,
    sessions: Mutex<HashMap<String, Session>>,
    live_output: Mutex<HashMap<Uuid, LiveOutput>>,
    user_settings: Mutex<BTreeMap<String, UserAgentSettings>>,
    repository_settings: Mutex<BTreeMap<String, UserAgentSettings>>,
    conversation_selections: Mutex<BTreeMap<String, ConversationSelection>>,
}

/// Read the saved overrides. A missing file means none; an unreadable one is
/// logged and ignored so a bad file cannot stop the bot from starting.
fn load_user_settings(path: &Path) -> BTreeMap<String, UserAgentSettings> {
    match std::fs::read(path) {
        Ok(raw) => serde_json::from_slice(&raw).unwrap_or_else(|error| {
            tracing::warn!(file = %path.display(), %error, "ignoring unreadable agent settings");
            BTreeMap::new()
        }),
        Err(_) => BTreeMap::new(),
    }
}

impl SessionStore {
    /// Open (creating if needed) a store and load persisted sessions.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(dir.join("sessions"))?;
        std::fs::create_dir_all(dir.join("jobs"))?;

        let mut sessions = HashMap::new();
        for entry in std::fs::read_dir(dir.join("sessions"))? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match std::fs::read_to_string(&path)
                .map_err(crate::error::BotError::from)
                .and_then(|raw| serde_json::from_str::<Session>(&raw).map_err(Into::into))
            {
                Ok(session) => {
                    sessions.insert(session.key.clone(), session);
                }
                Err(error) => {
                    tracing::warn!(file = %path.display(), %error, "skipping unreadable session");
                }
            }
        }

        let user_settings = load_user_settings(&dir.join(USER_SETTINGS_FILE));
        let repository_settings = load_user_settings(&dir.join("repository-agent-settings.json"));
        let selection_path = dir.join(CONVERSATION_SELECTIONS_FILE);
        let conversation_selections = match std::fs::read(&selection_path) {
            Ok(raw) => serde_json::from_slice(&raw)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(error.into()),
        };
        let store = Self {
            dir,
            sessions: Mutex::new(sessions),
            live_output: Mutex::new(HashMap::new()),
            user_settings: Mutex::new(user_settings),
            repository_settings: Mutex::new(repository_settings),
            conversation_selections: Mutex::new(conversation_selections),
        };
        // A run can only be executing in the process that started it, so any
        // run persisted without an end time when the store opens is stale.
        // Close it now so the status page does not report a phantom running
        // thread after a restart (issue #110).
        store.interrupt_stale_runs();
        Ok(store)
    }

    /// Stable key for the conversation a message belongs to.
    ///
    /// `user_id` namespaces the key so two accounts never share a session or
    /// job queue. `None` is only reachable from tests.
    pub fn key(message: &ForgeMessage, user_id: Option<&str>) -> String {
        let base = format!(
            "{}:{}:{}:{}",
            message.forge,
            message.repository,
            if message.is_pull_request {
                "pr"
            } else {
                "issue"
            },
            message.number.unwrap_or_default()
        );
        match user_id {
            Some(user) => format!("user:{user}:{base}"),
            None => base,
        }
    }

    /// Keep execution/status keys separate, but share explicit choices with
    /// the linked issue. Include the user and forge to isolate agent accounts.
    fn selection_key(message: &ForgeMessage, user_id: Option<&str>) -> String {
        let mut conversation = message.clone();
        if message.is_pull_request
            && let Some(issue) = &message.linked_issue
        {
            conversation.repository = issue
                .repository
                .clone()
                .unwrap_or_else(|| message.repository.clone());
            conversation.number = Some(issue.number);
            conversation.is_pull_request = false;
        }
        Self::key(&conversation, user_id)
    }

    pub fn conversation_selection(
        &self,
        message: &ForgeMessage,
        user_id: Option<&str>,
    ) -> Option<ConversationSelection> {
        self.conversation_selections
            .lock()
            .expect("selection mutex poisoned")
            .get(&Self::selection_key(message, user_id))
            .cloned()
    }

    /// Apply the last explicit choice and save any new one before enqueueing.
    /// The job receives a snapshot, so queued/recovered work cannot accidentally
    /// pick up a later mention's settings. Effort remains invocation-only.
    pub fn select_conversation(
        &self,
        job: &mut Job,
        validate_agent: impl FnOnce(&str) -> Result<()>,
    ) -> Result<()> {
        let key = Self::selection_key(&job.message, job.user_id.as_deref());
        let mut selections = self
            .conversation_selections
            .lock()
            .expect("selection mutex poisoned");
        let previous = selections.get(&key);
        let explicit_agent = job.mention.agent.as_deref().filter(|name| !name.is_empty());
        let agent = explicit_agent
            .map(str::to_owned)
            .or_else(|| previous.map(|selection| selection.agent.clone()))
            .unwrap_or_else(|| job.agent.clone());
        validate_agent(&agent)?;
        let model = job.mention.model.clone().or_else(|| {
            previous
                .filter(|selection| selection.agent == agent)
                .and_then(|selection| selection.model.clone())
        });
        if explicit_agent.is_some() || job.mention.model.is_some() {
            let mut next = selections.clone();
            next.insert(
                key,
                ConversationSelection {
                    agent: agent.clone(),
                    model: model.clone(),
                },
            );
            let path = self.dir.join(CONVERSATION_SELECTIONS_FILE);
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec_pretty(&next)?)?;
            std::fs::rename(tmp, path)?;
            *selections = next;
        }
        job.agent = agent;
        job.mention.model = model;
        Ok(())
    }

    /// Start (or continue) a session for a job.
    pub fn begin(&self, job: &Job) -> Result<Session> {
        let key = job.session_key();
        let mut sessions = self.sessions.lock().expect("session mutex poisoned");

        let session = sessions.entry(key.clone()).or_insert_with(|| Session {
            key: key.clone(),
            repository: job.message.repository.clone(),
            location: job.message.location.to_string(),
            agent: job.agent.clone(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            runs: Vec::new(),
        });

        let now = Utc::now();
        session.agent = job.agent.clone();
        session.location = job.message.location.to_string();
        session.updated_at = now;

        // A session runs at most one job at a time, so any run still marked
        // unfinished here cannot belong to a live process: the store was just
        // reopened, or a replayed job reached `begin` a second time. Close it
        // so `finish` cannot close this older record and leave the new run
        // unfinished forever (issue #110).
        for run in &mut session.runs {
            if run.finished_at.is_none() {
                run.interrupt(now);
            }
        }

        session.runs.push(RunRecord {
            job_id: job.id,
            agent: job.agent.clone(),
            message: Some(job.mention.message.clone()),
            started_at: now,
            finished_at: None,
            success: None,
            summary: None,
            model: None,
            effort: None,
            cache: None,
        });

        self.persist_locked(session)?;
        self.live_output
            .lock()
            .expect("live output map poisoned")
            .insert(job.id, LiveOutput::default());
        Ok(session.clone())
    }

    /// Persist the adapter about to run, including a fallback, so status
    /// snapshots identify it while the job is still in flight.
    pub fn set_running_agent(&self, key: &str, job_id: Uuid, agent: &str) -> Result<()> {
        let mut sessions = self.sessions.lock().expect("session mutex poisoned");
        let Some(session) = sessions.get_mut(key) else {
            return Ok(());
        };
        let Some(run) = session
            .runs
            .iter_mut()
            .rev()
            .find(|run| run.job_id == job_id && run.finished_at.is_none())
        else {
            return Ok(());
        };
        run.agent = agent.to_owned();
        session.agent = agent.to_owned();
        session.updated_at = Utc::now();
        self.persist_locked(session)
    }

    /// Record the outcome of a run. `agent` is the adapter that actually ran,
    /// which may differ from the requested one when the dispatcher fell back
    /// after a capacity error.
    pub fn finish(
        &self,
        key: &str,
        job_id: Uuid,
        agent: &str,
        outcome: &AgentOutcome,
    ) -> Result<()> {
        let mut sessions = self.sessions.lock().expect("session mutex poisoned");
        let Some(session) = sessions.get_mut(key) else {
            return Ok(());
        };
        if let Some(run) = session.runs.iter_mut().rev().find(|r| r.job_id == job_id) {
            run.agent = agent.to_owned();
            run.finished_at = Some(Utc::now());
            run.success = Some(outcome.success);
            run.summary = Some(outcome.summary.clone());
            run.model = outcome.model.clone();
            run.effort = outcome.effort.clone();
            run.cache = outcome.usage;
        }
        session.agent = agent.to_owned();
        session.updated_at = Utc::now();
        self.persist_locked(session)?;
        self.live_output
            .lock()
            .expect("live output map poisoned")
            .remove(&job_id);
        Ok(())
    }

    /// Fetch a stored session.
    pub fn get(&self, key: &str) -> Option<Session> {
        self.sessions
            .lock()
            .expect("session mutex poisoned")
            .get(key)
            .cloned()
    }

    pub fn live_output(&self, job_id: Uuid) -> Option<LiveOutput> {
        self.live_output
            .lock()
            .expect("live output map poisoned")
            .get(&job_id)
            .cloned()
    }

    /// All stored sessions, in no particular order. Used by the status page.
    pub fn list(&self) -> Vec<Session> {
        self.sessions
            .lock()
            .expect("session mutex poisoned")
            .values()
            .cloned()
            .collect()
    }

    /// Evict idle sessions whose last activity is older than `retention`.
    ///
    /// The status page is rebuilt from the persisted sessions, so dropping the
    /// stale ones bounds both the memory the store holds and the size of the
    /// page. A session with a run in flight or a pending job is kept: a
    /// follow-up that is about to continue the conversation must not lose the
    /// history it resumes. Evicted sessions are removed from memory and disk so
    /// a restart does not load them again.
    ///
    /// A zero `retention` disables eviction. Returns how many sessions were
    /// evicted.
    pub fn evict_stale(&self, retention: Duration) -> Result<usize> {
        if retention.is_zero() {
            return Ok(0);
        }
        // A running job's own file is still pending until it finishes, so this
        // also protects the run in flight even if its session record was lost.
        let protected: HashSet<String> = self
            .pending_jobs()?
            .into_iter()
            .map(|job| job.session_key())
            .collect();

        let now = Utc::now();
        let mut sessions = self.sessions.lock().expect("session mutex poisoned");
        let stale: Vec<String> = sessions
            .values()
            .filter(|session| {
                !session.is_running()
                    && !protected.contains(&session.key)
                    && now
                        .signed_duration_since(session.updated_at)
                        .to_std()
                        .is_ok_and(|age| age >= retention)
            })
            .map(|session| session.key.clone())
            .collect();

        for key in &stale {
            sessions.remove(key);
            let path = self
                .dir
                .join("sessions")
                .join(format!("{}.json", file_key(key)));
            if let Err(error) = std::fs::remove_file(&path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(key = %key, %error, "failed to remove evicted session");
            }
        }
        Ok(stale.len())
    }

    /// Close every run left in flight by a previous process.
    ///
    /// The store is opened once when the bot starts. Any run persisted without
    /// an end time belonged to a process that is gone, so it can never
    /// complete; leaving it open would make the status page report a phantom
    /// running thread forever (issue #110).
    fn interrupt_stale_runs(&self) {
        let now = Utc::now();
        let mut sessions = self.sessions.lock().expect("session mutex poisoned");
        for session in sessions.values_mut() {
            let mut interrupted = false;
            for run in &mut session.runs {
                if run.finished_at.is_none() {
                    run.interrupt(now);
                    interrupted = true;
                }
            }
            if interrupted && let Err(error) = self.persist_locked(session) {
                tracing::warn!(key = %session.key, %error, "failed to persist interrupted run");
            }
        }
    }

    fn persist_locked(&self, session: &Session) -> Result<()> {
        let path = self
            .dir
            .join("sessions")
            .join(format!("{}.json", file_key(&session.key)));
        let raw = serde_json::to_vec_pretty(session)?;
        // Write to a temporary file then rename so readers never see a partial
        // document.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, raw)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// The operator's overrides for `user_id`; empty when none were set.
    pub fn user_settings(&self, user_id: &str) -> UserAgentSettings {
        self.user_settings
            .lock()
            .expect("user settings mutex poisoned")
            .get(user_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Replace the overrides for `user_id` and persist them. Empty settings
    /// remove the entry, so the user falls back to its configuration.
    pub fn set_user_settings(&self, user_id: &str, settings: UserAgentSettings) -> Result<()> {
        let mut all = self
            .user_settings
            .lock()
            .expect("user settings mutex poisoned");
        let previous = all.get(user_id).cloned();
        if settings.is_empty() {
            all.remove(user_id);
        } else {
            all.insert(user_id.to_owned(), settings);
        }
        if let Err(error) = self.persist_user_settings(&all) {
            // Keep memory and disk in agreement: a failed write must not leave
            // an override that a restart would silently lose.
            match previous {
                Some(previous) => all.insert(user_id.to_owned(), previous),
                None => all.remove(user_id),
            };
            return Err(error);
        }
        Ok(())
    }

    fn persist_user_settings(&self, all: &BTreeMap<String, UserAgentSettings>) -> Result<()> {
        let path = self.dir.join(USER_SETTINGS_FILE);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(all)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Repository overrides are isolated by repository and configured user.
    pub fn repository_settings(&self, repository: &str, user: &str) -> Option<UserAgentSettings> {
        let key = serde_json::to_string(&(repository, user)).expect("string tuple");
        self.repository_settings
            .lock()
            .expect("repository settings mutex poisoned")
            .get(&key)
            .cloned()
    }

    pub fn set_repository_settings(
        &self,
        repository: &str,
        user: &str,
        settings: Option<UserAgentSettings>,
    ) -> Result<()> {
        let key = serde_json::to_string(&(repository, user)).expect("string tuple");
        let mut all = self
            .repository_settings
            .lock()
            .expect("repository settings mutex poisoned");
        let mut next = all.clone();
        if let Some(settings) = settings {
            next.insert(key, settings);
        } else {
            next.remove(&key);
        }
        let path = self.dir.join("repository-agent-settings.json");
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&next)?)?;
        std::fs::rename(&tmp, &path)?;
        *all = next;
        Ok(())
    }

    /// Persist a pending job.
    pub fn save_job(&self, job: &Job) -> Result<()> {
        let path = self.dir.join("jobs").join(format!("{}.json", job.id));
        std::fs::write(path, serde_json::to_vec_pretty(job)?)?;
        Ok(())
    }

    /// Remove a completed job.
    pub fn remove_job(&self, id: Uuid) -> Result<()> {
        let path = self.dir.join("jobs").join(format!("{id}.json"));
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }

    /// Load jobs that were pending when the process stopped.
    pub fn pending_jobs(&self) -> Result<Vec<Job>> {
        let mut jobs = Vec::new();
        for entry in std::fs::read_dir(self.dir.join("jobs"))? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match std::fs::read_to_string(&path)
                .map_err(crate::error::BotError::from)
                .and_then(|raw| serde_json::from_str::<Job>(&raw).map_err(Into::into))
            {
                Ok(job) => jobs.push(job),
                Err(error) => {
                    tracing::warn!(file = %path.display(), %error, "skipping unreadable job");
                }
            }
        }
        jobs.sort_by_key(|j| j.created_at);
        Ok(jobs)
    }
}

/// Turn a session key into something safe for a filename.
fn file_key(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::location::ForgeKind;
    use crate::mention::Mention;
    use std::time::Duration;
    use url::Url;

    fn job() -> Job {
        Job {
            model_agent: None,
            configured_model_agent: None,
            id: Uuid::new_v4(),
            message: ForgeMessage {
                forge: ForgeKind::Forgejo,
                location: Url::parse("http://forge.local/a/b/issues/2").unwrap(),
                body: "@agent x".into(),
                author: "u".into(),
                repository: "a/b".into(),
                comment_id: Some(1),
                number: Some(2),
                is_pull_request: false,
                linked_issue: None,
                event: "issue_comment".into(),
                title: None,
                reply_target: Default::default(),
            },
            mention: Mention {
                model: None,
                effort: None,
                agent: None,
                message: "x".into(),
            },
            agent: "codex".into(),
            user_id: None,
            created_at: Utc::now(),
            status_comment: None,
            waiting: false,
        }
    }

    #[test]
    fn conversation_choices_survive_restart_and_linked_prs_without_leaking() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open(dir.path()).unwrap();
        let mut original = job();
        original.user_id = Some("bot".into());
        original.mention.agent = Some("codex".into());
        original.mention.model = Some("gpt-6-astra".into());
        original.mention.effort = Some("high".into());
        store
            .select_conversation(&mut original, |_| Ok(()))
            .unwrap();
        drop(store);
        let store = SessionStore::open(dir.path()).unwrap();
        let mut follow_up = job();
        follow_up.user_id = Some("bot".into());
        follow_up.agent = "pi-rpc".into();
        follow_up.message.is_pull_request = true;
        follow_up.message.number = Some(3);
        follow_up.message.linked_issue = Some(crate::forge::IssueRef {
            repository: None,
            number: 2,
        });
        let fresh = follow_up.clone();
        store
            .select_conversation(&mut follow_up, |_| Ok(()))
            .unwrap();
        assert_eq!(follow_up.agent, "codex");
        assert_eq!(follow_up.mention.model.as_deref(), Some("gpt-6-astra"));
        assert_eq!(follow_up.mention.effort, None);
        // Recovery retains the accepted snapshot even after another selection.
        store.save_job(&follow_up).unwrap();
        let mut changed = fresh.clone();
        changed.mention.agent = Some("claude".into());
        store.select_conversation(&mut changed, |_| Ok(())).unwrap();
        assert_eq!(changed.mention.model, None);
        let recovered = store.pending_jobs().unwrap();
        assert_eq!(recovered[0].agent, "codex");
        assert_eq!(recovered[0].mention.model.as_deref(), Some("gpt-6-astra"));
        let mut issue = job();
        issue.user_id = Some("bot".into());
        store.select_conversation(&mut issue, |_| Ok(())).unwrap();
        assert_eq!(issue.agent, "claude", "PR updates the shared issue choice");

        for variant in 0..5 {
            let mut other = fresh.clone();
            match variant {
                0 => other.user_id = Some("reviewer".into()),
                1 => other.message.repository = "other/repo".into(),
                2 => other.message.forge = ForgeKind::GitHub,
                3 => other.message.linked_issue = None,
                _ => other.message.linked_issue.as_mut().unwrap().number = 9,
            }
            store.select_conversation(&mut other, |_| Ok(())).unwrap();
            assert_eq!(other.agent, "pi-rpc", "variant {variant}");
            assert_eq!(other.mention.model, None);
        }
        let mut cross_repo = fresh;
        cross_repo.message.repository = "other/repo".into();
        cross_repo.message.linked_issue.as_mut().unwrap().repository = Some("a/b".into());
        store
            .select_conversation(&mut cross_repo, |_| Ok(()))
            .unwrap();
        assert_eq!(cross_repo.agent, "claude");
    }

    #[test]
    fn conversation_choice_updates_are_explicit_and_transactional() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open(dir.path()).unwrap();
        let mut original = job();
        original.mention.model = Some("model-one".into());
        store
            .select_conversation(&mut original, |_| Ok(()))
            .unwrap();
        let mut same = job();
        same.mention.agent = Some("codex".into());
        store.select_conversation(&mut same, |_| Ok(())).unwrap();
        assert_eq!(same.mention.model.as_deref(), Some("model-one"));
        let mut change_model = job();
        change_model.agent = "pi-rpc".into();
        change_model.mention.model = Some("model-two".into());
        store
            .select_conversation(&mut change_model, |_| Ok(()))
            .unwrap();
        assert_eq!(change_model.agent, "codex");
        assert_eq!(change_model.mention.model.as_deref(), Some("model-two"));

        let mut invalid = job();
        invalid.mention.agent = Some("typo".into());
        assert!(
            store
                .select_conversation(&mut invalid, |name| Err(
                    crate::error::BotError::UnknownAgent(name.into())
                ))
                .is_err()
        );
        // Failed disk writes must leave the remembered preference untouched.
        let tmp = dir
            .path()
            .join(CONVERSATION_SELECTIONS_FILE)
            .with_extension("json.tmp");
        std::fs::create_dir(&tmp).unwrap();
        assert!(store.select_conversation(&mut invalid, |_| Ok(())).is_err());
        std::fs::remove_dir(tmp).unwrap();
        let reopened = SessionStore::open(dir.path()).unwrap();
        for store in [&store, &reopened] {
            let mut plain = job();
            plain.agent = "pi-rpc".into();
            store.select_conversation(&mut plain, |_| Ok(())).unwrap();
            assert_eq!(plain.agent, "codex");
            assert_eq!(plain.mention.model.as_deref(), Some("model-two"));
        }
    }

    #[test]
    fn pending_jobs_preserve_settings_adapter_and_accept_older_records() {
        let mut pending = job();
        pending.model_agent = Some("primary".into());
        let mut value = serde_json::to_value(&pending).unwrap();
        let restored: Job = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(restored.model_agent.as_deref(), Some("primary"));
        value.as_object_mut().unwrap().remove("model_agent");
        let legacy: Job = serde_json::from_value(value).unwrap();
        assert_eq!(legacy.model_agent, None);
    }

    #[test]
    fn keys_are_namespaced_per_user() {
        let message = job().message;
        let legacy = SessionStore::key(&message, None);
        let user = SessionStore::key(&message, Some("reviewer"));
        assert!(!legacy.starts_with("user:"));
        assert!(user.starts_with("user:reviewer:"));
        assert_ne!(legacy, user);
        // A different user is a different namespace again.
        assert_ne!(user, SessionStore::key(&message, Some("other")));
    }

    #[test]
    fn records_run_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open(dir.path()).unwrap();
        let job = job();
        let key = job.session_key();

        let session = store.begin(&job).unwrap();
        assert_eq!(session.runs.len(), 1);
        assert_eq!(session.runs[0].success, None);

        let mut outcome = AgentOutcome::success("done", Duration::from_millis(5));
        outcome.model = Some("test/example".into());
        outcome.effort = Some("high".into());
        outcome.usage = Some(TokenUsage {
            prompt_tokens: 1_000,
            cached_tokens: 750,
        });
        store.finish(&key, job.id, "pi", &outcome).unwrap();

        let stored = store.get(&key).unwrap();
        assert_eq!(stored.runs[0].success, Some(true));
        assert_eq!(stored.runs[0].agent, "pi");
        assert_eq!(stored.runs[0].summary.as_deref(), Some("done"));
        assert_eq!(stored.runs[0].model.as_deref(), Some("test/example"));
        assert_eq!(stored.runs[0].effort, outcome.effort);
        assert_eq!(stored.runs[0].cache, outcome.usage);
        let reopened = SessionStore::open(dir.path()).unwrap();
        assert_eq!(reopened.get(&key).unwrap().runs[0].model, outcome.model);
        assert_eq!(reopened.get(&key).unwrap().runs[0].effort, outcome.effort);
        assert_eq!(reopened.get(&key).unwrap().runs[0].cache, outcome.usage);
        let mut legacy = serde_json::to_value(&stored.runs[0]).unwrap();
        legacy.as_object_mut().unwrap().remove("effort");
        assert_eq!(
            serde_json::from_value::<RunRecord>(legacy).unwrap().effort,
            None
        );
    }

    #[test]
    fn reopening_interrupts_a_run_left_in_flight() {
        let dir = tempfile::tempdir().unwrap();
        let key = {
            let store = SessionStore::open(dir.path()).unwrap();
            let job = job();
            store.begin(&job).unwrap();
            job.session_key()
        };

        // A restart reopens the store. The run could not have survived it, so
        // the status page must no longer report it as running (issue #110).
        let store = SessionStore::open(dir.path()).unwrap();
        let session = store.get(&key).unwrap();
        assert!(!session.is_running());
        let run = &session.runs[0];
        assert!(run.finished_at.is_some());
        assert_eq!(run.success, Some(false));
        assert!(run.summary.as_deref().unwrap().contains("interrupted"));
    }

    #[test]
    fn finish_closes_the_newest_record_for_a_replayed_job() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open(dir.path()).unwrap();
        let job = job();
        let key = job.session_key();

        // A job replayed before the first record finished must not leave the
        // second record open when `finish` runs.
        store.begin(&job).unwrap();
        store.begin(&job).unwrap();
        let outcome = AgentOutcome::success("done", Duration::from_millis(5));
        store.finish(&key, job.id, "pi", &outcome).unwrap();

        let session = store.get(&key).unwrap();
        assert_eq!(session.runs.len(), 2);
        assert!(!session.is_running());
        assert!(session.runs.iter().all(|run| run.finished_at.is_some()));
        assert_eq!(session.runs[1].success, Some(true));
        assert_eq!(session.runs[1].agent, "pi");
    }

    #[test]
    fn reloads_sessions_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = SessionStore::open(dir.path()).unwrap();
            store.begin(&job()).unwrap();
        }
        let store = SessionStore::open(dir.path()).unwrap();
        assert!(
            store
                .get(&SessionStore::key(&job().message, None))
                .is_some()
        );
        assert_eq!(store.list().len(), 1);
    }

    #[test]
    fn persists_and_removes_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open(dir.path()).unwrap();
        let job = job();
        store.save_job(&job).unwrap();
        let pending = store.pending_jobs().unwrap();
        assert_eq!(pending.len(), 1);
        store.remove_job(job.id).unwrap();
        assert!(store.pending_jobs().unwrap().is_empty());
    }

    #[test]
    fn persists_the_user_id_for_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open(dir.path()).unwrap();
        let mut scoped = job();
        scoped.user_id = Some("reviewer".into());
        store.save_job(&scoped).unwrap();
        let pending = store.pending_jobs().unwrap();
        assert_eq!(pending[0].user_id.as_deref(), Some("reviewer"));
        assert!(pending[0].session_key().starts_with("user:reviewer:"));

        // A legacy job round-trips as the legacy target.
        let legacy = job();
        store.save_job(&legacy).unwrap();
        let pending = store.pending_jobs().unwrap();
        assert!(
            pending
                .iter()
                .any(|job| job.id == legacy.id && job.user_id.is_none())
        );
    }

    /// Age a stored session so the eviction test does not have to wait.
    fn age_session(store: &SessionStore, key: &str, age: chrono::Duration) {
        let mut sessions = store.sessions.lock().unwrap();
        sessions.get_mut(key).unwrap().updated_at = Utc::now() - age;
    }

    #[test]
    fn evicts_idle_sessions_past_retention() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open(dir.path()).unwrap();
        let job = job();
        let key = job.session_key();
        store.begin(&job).unwrap();
        let outcome = AgentOutcome::success("done", Duration::from_millis(5));
        store.finish(&key, job.id, "pi", &outcome).unwrap();
        age_session(&store, &key, chrono::Duration::days(8));

        let removed = store
            .evict_stale(Duration::from_secs(7 * 24 * 60 * 60))
            .unwrap();
        assert_eq!(removed, 1);
        assert!(store.get(&key).is_none());
        let path = dir
            .path()
            .join("sessions")
            .join(format!("{}.json", file_key(&key)));
        assert!(
            !path.exists(),
            "an evicted session must leave no file behind"
        );
    }

    #[test]
    fn keeps_a_session_with_a_pending_job() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open(dir.path()).unwrap();
        let job = job();
        let key = job.session_key();
        store.begin(&job).unwrap();
        let outcome = AgentOutcome::success("done", Duration::from_millis(5));
        store.finish(&key, job.id, "pi", &outcome).unwrap();
        store.save_job(&job).unwrap();
        age_session(&store, &key, chrono::Duration::days(30));

        // A queued mention is about to continue the conversation, so its
        // history must survive even though it is idle right now.
        assert_eq!(store.evict_stale(Duration::from_secs(60)).unwrap(), 0);
        assert!(store.get(&key).is_some());
    }

    #[test]
    fn keeps_a_session_with_a_run_in_flight() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open(dir.path()).unwrap();
        let job = job();
        let key = job.session_key();
        // `begin` leaves the run unfinished: the agent is still working.
        store.begin(&job).unwrap();
        age_session(&store, &key, chrono::Duration::days(30));

        assert_eq!(store.evict_stale(Duration::from_secs(60)).unwrap(), 0);
        assert!(store.get(&key).is_some());
    }

    #[test]
    fn zero_retention_disables_eviction() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open(dir.path()).unwrap();
        let job = job();
        let key = job.session_key();
        store.begin(&job).unwrap();
        let outcome = AgentOutcome::success("done", Duration::from_millis(5));
        store.finish(&key, job.id, "pi", &outcome).unwrap();
        age_session(&store, &key, chrono::Duration::days(365));

        assert_eq!(store.evict_stale(Duration::ZERO).unwrap(), 0);
        assert!(store.get(&key).is_some());
    }
}

#[cfg(test)]
mod user_settings_tests {
    use super::*;

    fn settings(model: &str, effort: &str) -> UserAgentSettings {
        UserAgentSettings {
            agent: None,
            model: Some(model.into()),
            effort: Some(effort.into()),
        }
    }

    #[test]
    fn settings_persist_across_reopen_and_empty_settings_clear_them() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open(dir.path()).unwrap();
        assert!(store.user_settings("default").is_empty());

        store
            .set_user_settings("default", settings("gpt-fast", "high"))
            .unwrap();
        store
            .set_user_settings("reviewer", UserAgentSettings::default())
            .unwrap();
        drop(store);

        let reopened = SessionStore::open(dir.path()).unwrap();
        assert_eq!(
            reopened.user_settings("default"),
            settings("gpt-fast", "high")
        );
        // Empty settings were never stored, so the user keeps its configuration.
        assert!(reopened.user_settings("reviewer").is_empty());

        reopened
            .set_user_settings("default", UserAgentSettings::default())
            .unwrap();
        let reopened = SessionStore::open(dir.path()).unwrap();
        assert!(reopened.user_settings("default").is_empty());
    }

    #[test]
    fn unreadable_settings_file_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(USER_SETTINGS_FILE), "not json").unwrap();
        let store = SessionStore::open(dir.path()).unwrap();
        assert!(store.user_settings("default").is_empty());
    }

    #[test]
    fn failed_save_restores_the_previous_settings() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open(dir.path()).unwrap();
        store
            .set_user_settings("default", settings("old", "low"))
            .unwrap();

        // A directory where the temporary file belongs makes the write fail.
        let tmp = dir.path().join("agent-settings.json.tmp");
        std::fs::create_dir(&tmp).unwrap();
        assert!(
            store
                .set_user_settings("default", settings("new", "high"))
                .is_err()
        );
        assert!(
            store
                .set_user_settings("fresh", settings("new", "high"))
                .is_err()
        );
        assert_eq!(store.user_settings("default"), settings("old", "low"));
        assert!(store.user_settings("fresh").is_empty());

        std::fs::remove_dir(&tmp).unwrap();
        store
            .set_user_settings("default", settings("new", "high"))
            .unwrap();
        assert_eq!(store.user_settings("default"), settings("new", "high"));
    }
}

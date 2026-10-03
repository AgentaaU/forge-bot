//! On-disk session and job persistence.

use std::collections::{HashMap, HashSet};
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

/// Persists sessions and pending jobs under a directory.
pub struct SessionStore {
    dir: PathBuf,
    sessions: Mutex<HashMap<String, Session>>,
    live_output: Mutex<HashMap<Uuid, LiveOutput>>,
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

        let store = Self {
            dir,
            sessions: Mutex::new(sessions),
            live_output: Mutex::new(HashMap::new()),
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
        assert_eq!(stored.runs[0].cache, outcome.usage);
        let reopened = SessionStore::open(dir.path()).unwrap();
        assert_eq!(reopened.get(&key).unwrap().runs[0].model, outcome.model);
        assert_eq!(reopened.get(&key).unwrap().runs[0].cache, outcome.usage);
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

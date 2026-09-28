//! On-disk session and job persistence.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::agent::{AgentOutcome, LiveOutput};
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
}

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

        Ok(Self {
            dir,
            sessions: Mutex::new(sessions),
            live_output: Mutex::new(HashMap::new()),
        })
    }

    /// Stable key for the conversation a message belongs to.
    pub fn key(message: &ForgeMessage) -> String {
        format!(
            "{}:{}:{}:{}",
            message.forge,
            message.repository,
            if message.is_pull_request {
                "pr"
            } else {
                "issue"
            },
            message.number.unwrap_or_default()
        )
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

        session.agent = job.agent.clone();
        session.location = job.message.location.to_string();
        session.updated_at = Utc::now();
        session.runs.push(RunRecord {
            job_id: job.id,
            agent: job.agent.clone(),
            message: Some(job.mention.message.clone()),
            started_at: Utc::now(),
            finished_at: None,
            success: None,
            summary: None,
            model: None,
        });

        self.persist_locked(session)?;
        self.live_output
            .lock()
            .expect("live output map poisoned")
            .insert(job.id, LiveOutput::default());
        Ok(session.clone())
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
        if let Some(run) = session.runs.iter_mut().find(|r| r.job_id == job_id) {
            run.agent = agent.to_owned();
            run.finished_at = Some(Utc::now());
            run.success = Some(outcome.success);
            run.summary = Some(outcome.summary.clone());
            run.model = outcome.model.clone();
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
            created_at: Utc::now(),
            status_comment: None,
            waiting: false,
        }
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
        store.finish(&key, job.id, "pi", &outcome).unwrap();

        let stored = store.get(&key).unwrap();
        assert_eq!(stored.runs[0].success, Some(true));
        assert_eq!(stored.runs[0].agent, "pi");
        assert_eq!(stored.runs[0].summary.as_deref(), Some("done"));
        assert_eq!(stored.runs[0].model.as_deref(), Some("test/example"));
        let reopened = SessionStore::open(dir.path()).unwrap();
        assert_eq!(reopened.get(&key).unwrap().runs[0].model, outcome.model);
    }

    #[test]
    fn reloads_sessions_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = SessionStore::open(dir.path()).unwrap();
            store.begin(&job()).unwrap();
        }
        let store = SessionStore::open(dir.path()).unwrap();
        assert!(store.get(&SessionStore::key(&job().message)).is_some());
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
}

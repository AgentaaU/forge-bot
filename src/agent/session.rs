//! Conversation-scoped backend sessions.
//!
//! One-shot CLIs (`codex`, `pi`, `claude`, ...) start from scratch on every
//! comment, so the model loses the conversation and the provider loses the
//! prompt/KV cache that made the previous turn cheap. This store remembers one
//! backend session id per `(agent, conversation)` pair, so the next comment in
//! the same thread can resume the same conversation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use uuid::Uuid;

/// File (relative to the gateway state directory) holding the mappings.
const FILE: &str = "agent-sessions.json";

/// A small, process-wide map of conversation -> backend session id.
///
/// Failures to read or write the backing file are deliberately ignored: a lost
/// session only costs a cold start, it must never fail a job.
#[derive(Debug, Default)]
pub struct SessionStore {
    path: PathBuf,
    ids: Mutex<HashMap<String, String>>,
}

impl SessionStore {
    /// Load the store from `state_dir`, treating a missing or corrupt file as
    /// an empty map.
    pub fn load(state_dir: &Path) -> Self {
        let path = state_dir.join(FILE);
        let ids = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            path,
            ids: Mutex::new(ids),
        }
    }

    fn slot(agent: &str, key: &str) -> String {
        format!("{agent}\u{1f}{key}")
    }

    /// Backend session id for `agent` in the `key` conversation, if any.
    pub fn get(&self, agent: &str, key: &str) -> Option<String> {
        self.ids
            .lock()
            .expect("session store poisoned")
            .get(&Self::slot(agent, key))
            .cloned()
    }

    /// Remember the backend session id for later comments in the conversation.
    pub fn set(&self, agent: &str, key: &str, session: &str) {
        let mut ids = self.ids.lock().expect("session store poisoned");
        ids.insert(Self::slot(agent, key), session.to_owned());
        self.persist(&ids);
    }

    /// A stable UUID for a conversation, for CLIs that want to be handed their
    /// own id and create the session when it does not exist yet (`claude`).
    pub fn deterministic_id(&self, agent: &str, key: &str) -> String {
        Uuid::new_v5(
            &Uuid::NAMESPACE_URL,
            format!("forge-bot:{agent}:{key}").as_bytes(),
        )
        .to_string()
    }

    fn persist(&self, ids: &HashMap<String, String>) {
        // A store built with `Default` (tests, diagnostics) has no backing
        // file; never scatter a relative `.json.tmp` into the working dir.
        if self.path.as_os_str().is_empty() {
            return;
        }
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let Ok(data) = serde_json::to_vec_pretty(ids) else {
            return;
        };
        let tmp = self.path.with_extension("json.tmp");
        if std::fs::write(&tmp, data).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_ids_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = SessionStore::load(dir.path());
            assert_eq!(store.get("codex", "forgejo:o/r:1"), None);
            store.set("codex", "forgejo:o/r:1", "session-a");
        }
        let reloaded = SessionStore::load(dir.path());
        assert_eq!(
            reloaded.get("codex", "forgejo:o/r:1"),
            Some("session-a".to_owned())
        );
        // A different agent or conversation is independent.
        assert_eq!(reloaded.get("claude", "forgejo:o/r:1"), None);
        assert_eq!(reloaded.get("codex", "forgejo:o/r:2"), None);
    }

    #[test]
    fn deterministic_ids_are_stable_and_distinct() {
        let store = SessionStore::default();
        let a = store.deterministic_id("claude", "forgejo:o/r:1");
        let b = store.deterministic_id("claude", "forgejo:o/r:1");
        let c = store.deterministic_id("claude", "forgejo:o/r:2");
        assert_eq!(a, b);
        assert_ne!(a, c);
        // uuid v5 shape
        assert_eq!(a.len(), 36);
    }
}

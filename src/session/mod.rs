//! Job queue and session persistence.
//!
//! A webhook creates a [`Job`]. Jobs are queued, persisted to disk (so a crash
//! or restart does not lose them), and executed by a bounded scheduler. Runs
//! are serialized per conversation and different conversations run in
//! parallel, so a busy thread never blocks an unrelated issue or pull request.
//! Each repository/issue pair gets a [`Session`](store::Session) that groups
//! successive runs, which is what makes follow-up mentions in the same thread
//! feel like a conversation.

pub mod queue;
pub mod store;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::forge::ForgeMessage;
use crate::mention::Mention;

pub use queue::Dispatcher;
pub use store::{RunRecord, Session, SessionStore};

/// One unit of work handed to an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: Uuid,
    pub message: ForgeMessage,
    pub mention: Mention,
    /// Resolved agent name.
    pub agent: String,
    pub created_at: DateTime<Utc>,
    /// Forge comment id of the status comment, when the forge supports editing
    /// it. `submit` posts the acknowledgement and records the id so the worker
    /// can append each fallback notice to the same comment instead of posting a
    /// new one per step (issue #77).
    #[serde(default)]
    pub status_comment: Option<String>,
    /// An acknowledgement was posted but cannot be edited, as with an inline
    /// Forgejo review comment. Avoid posting the same acknowledgement again
    /// when the worker finishes.
    #[serde(default)]
    pub ack_sent: bool,
}

impl Job {
    /// Stable key identifying the conversation, used for session persistence.
    pub fn session_key(&self) -> String {
        SessionStore::key(&self.message)
    }
}

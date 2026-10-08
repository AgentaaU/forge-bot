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
pub mod statistics;
pub mod status;
pub mod store;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::forge::ForgeMessage;
use crate::mention::Mention;

pub use queue::Dispatcher;
pub use status::{ThreadState, ThreadStatus};
pub use store::{RunRecord, Session, SessionStore, UserAgentSettings};

/// One unit of work handed to an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: Uuid,
    pub message: ForgeMessage,
    pub mention: Mention,
    /// Resolved agent name.
    pub agent: String,
    /// User id this job belongs to. Persisted so recovery runs under the
    /// same account instead of re-deriving it from the message.
    #[serde(default)]
    pub user_id: Option<String>,
    pub created_at: DateTime<Utc>,
    /// Forge comment id of the status comment, when the forge supports editing
    /// it. `submit` posts the acknowledgement and records the id so the worker
    /// can append each fallback notice to the same comment instead of posting a
    /// new one per step (issue #77).
    #[serde(default)]
    pub status_comment: Option<String>,
    /// Whether `submit` acknowledged this job as waiting behind the run already
    /// in flight for its conversation. The worker rewrites that acknowledgement
    /// to name the running agent once the job actually starts.
    #[serde(default)]
    pub waiting: bool,
}

impl Job {
    /// Stable key identifying the conversation, used for session persistence.
    pub fn session_key(&self) -> String {
        SessionStore::key(&self.message, self.user_id.as_deref())
    }
}

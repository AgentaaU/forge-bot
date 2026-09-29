//! Coding agent adapters.
//!
//! The gateway knows nothing about how an agent works: it builds an
//! [`AgentRequest`] from the webhook and hands it to an [`Agent`]. Adapters for
//! concrete CLIs (Codex, Pi, Claude Code, Kimi, ...) live in submodules.

pub mod agy;
pub mod capacity;
pub mod claude;
pub mod codex;
pub mod command;
pub mod kimi;
pub mod pi;
pub mod pi_rpc;
mod prompt;
pub mod registry;
pub mod session;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::Result;
use crate::forge::{IssueRef, ReplyTarget};
use crate::location::ForgeKind;

pub use registry::{AgentRegistry, UnavailableReason};

/// The only thing the gateway sends to an agent, exactly as in the design:
/// where the request came from and what was asked.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRequest {
    pub location: Url,
    pub message: String,
}

/// Additional, non-authoritative context handed to an adapter. It is kept
/// separate from [`AgentRequest`] because the request is the stable contract,
/// while this is convenience metadata.
#[derive(Debug, Clone, Default)]
pub struct AgentContext {
    pub workspace: PathBuf,
    pub forge: Option<ForgeKind>,
    pub repository: String,
    /// Login of the forge user who requested this run.
    pub requester: String,
    pub issue_number: Option<u64>,
    pub is_pull_request: bool,
    /// Issue a pull request closes, when known. Used to prefer the same pooled
    /// agent for both threads; it is never exposed to the agent.
    pub linked_issue: Option<IssueRef>,
    pub title: Option<String>,
    /// Thread the agent should answer in when it posts its own reply.
    ///
    /// The gateway also uses this target for optional result summaries.
    /// Agents need the coordinates to reply in an inline review thread.
    pub reply_target: ReplyTarget,
    /// Environment variables carrying forge credentials.
    pub credentials: Vec<(String, String)>,
    /// Recent output from the current run, shared with the status page.
    pub live_output: Option<LiveOutput>,
    /// Shared with the status page while this run is active.
    pub reported_model: Arc<Mutex<Option<String>>>,
}

/// Bounded output buffer for a run in progress. It is never persisted.
#[derive(Debug, Clone, Default)]
pub struct LiveOutput(Arc<Mutex<Vec<u8>>>);

impl LiveOutput {
    const LIMIT: usize = 1024 * 1024;

    pub fn append(&self, bytes: &[u8]) {
        let mut output = self.0.lock().expect("live output mutex poisoned");
        output.extend_from_slice(bytes);
        if output.len() > Self::LIMIT {
            let excess = output.len() - Self::LIMIT;
            output.drain(..excess);
        }
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("live output mutex poisoned")).into_owned()
    }
}

impl AgentContext {
    /// Whether the triggering mention came from an issue rather than a pull request.
    pub fn is_issue(&self) -> bool {
        !self.is_pull_request
    }

    /// Variables exported to the agent process.
    pub fn environment(&self, request: &AgentRequest) -> Vec<(String, String)> {
        let mut env = vec![
            (
                "FORGE_BOT_LOCATION".to_owned(),
                request.location.to_string(),
            ),
            ("FORGE_BOT_MESSAGE".to_owned(), request.message.clone()),
            (
                "FORGE_BOT_WORKSPACE".to_owned(),
                self.workspace.display().to_string(),
            ),
            ("FORGE_BOT_REPOSITORY".to_owned(), self.repository.clone()),
        ];

        if let Some(forge) = self.forge {
            env.push(("FORGE_BOT_FORGE".to_owned(), forge.as_str().to_owned()));
        }
        if let Some(number) = self.issue_number {
            env.push(("FORGE_BOT_ISSUE_NUMBER".to_owned(), number.to_string()));
        }
        env.push((
            "FORGE_BOT_IS_PULL_REQUEST".to_owned(),
            self.is_pull_request.to_string(),
        ));

        env.extend(self.credentials.iter().cloned());
        env
    }
}

/// Stable conversation key used to route one thread to a backend session and
/// prefer its most recently used pooled agent.
///
/// A pull request is folded onto the issue it closes when the description
/// references one, so both threads share a routing key and backend session. This
/// is internal routing data and is deliberately never rendered into a prompt.
pub fn conversation_key(context: &AgentContext) -> String {
    if context.repository.is_empty() {
        return context.workspace.to_string_lossy().into_owned();
    }
    // A pull request folds onto the issue it closes. When that issue is in
    // another repository, use its owner/repo so the two threads share a key.
    let (repository, number) = if context.is_pull_request {
        match &context.linked_issue {
            Some(linked) => (
                linked.repository.as_deref().unwrap_or(&context.repository),
                Some(linked.number),
            ),
            None => (context.repository.as_str(), context.issue_number),
        }
    } else {
        (context.repository.as_str(), context.issue_number)
    };
    match context.forge {
        Some(forge) => format!(
            "{}:{}:{}",
            forge.as_str(),
            repository,
            number.unwrap_or_default()
        ),
        None => format!("{repository}:{}", number.unwrap_or_default()),
    }
}

/// Provider-reported prompt token usage for one agent run.
///
/// The two adapters report this differently: Codex counts
/// `cached_input_tokens` as a subset of `input_tokens`, while Pi reports the
/// cache miss (`input`) and the cache hit (`cacheRead`) separately. Both are
/// normalised here to (total prompt, cached subset) so the hit rate is
/// `cached / prompt` either way.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Whole prompt billed for the run (cache hits plus fresh input).
    #[serde(default)]
    pub prompt_tokens: u64,
    /// Part of `prompt_tokens` the provider served from its prompt cache.
    #[serde(default)]
    pub cached_tokens: u64,
}

impl TokenUsage {
    /// Percentage of prompt tokens served from cache, or `None` when the
    /// provider reported no prompt tokens at all.
    pub fn hit_rate(&self) -> Option<f64> {
        (self.prompt_tokens > 0)
            .then(|| 100.0 * self.cached_tokens as f64 / self.prompt_tokens as f64)
    }
}

/// What an agent reports back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentOutcome {
    pub success: bool,
    pub summary: String,
    pub duration: Duration,
    /// Model reported by the agent, when its protocol exposes one.
    #[serde(default)]
    pub model: Option<String>,
    /// Provider prompt-cache accounting, when the adapter can read it.
    #[serde(default)]
    pub usage: Option<TokenUsage>,
}
impl AgentOutcome {
    pub fn success(summary: impl Into<String>, duration: Duration) -> Self {
        Self {
            success: true,
            summary: summary.into(),
            duration,
            model: None,
            usage: None,
        }
    }

    pub fn failure(summary: impl Into<String>, duration: Duration) -> Self {
        Self {
            success: false,
            summary: summary.into(),
            duration,
            model: None,
            usage: None,
        }
    }
}

/// Confirmation that a follow-up was delivered into an agent run in flight.
///
/// Returned by [`Agent::follow_up`] so the scheduler can post a short notice
/// instead of silently dropping the comment or queueing a second run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteerReceipt {
    /// Short status posted in the thread, e.g. `📎 merged into the current run`.
    pub notice: String,
}

impl SteerReceipt {
    /// The default receipt for a follow-up merged into the live run.
    pub fn merged() -> Self {
        Self {
            notice: "📎 Merged into the current run.".to_owned(),
        }
    }
}

/// A coding agent.
#[async_trait::async_trait]
pub trait Agent: Send + Sync {
    /// Adapter name, e.g. `codex`.
    fn name(&self) -> &str;

    /// Run the agent for one request. Implementations should be idempotent and
    /// must not panic on agent failure.
    async fn run(&self, request: &AgentRequest, context: &AgentContext) -> Result<AgentOutcome>;

    /// Deliver a follow-up into a conversation that already has a run in
    /// flight, without starting another agent.
    ///
    /// `Ok(None)` means the adapter has nothing live to steer (for example a
    /// one-shot CLI), so the caller queues the job exactly as before. The
    /// default implementation does exactly that.
    async fn follow_up(
        &self,
        _request: &AgentRequest,
        _context: &AgentContext,
    ) -> Result<Option<SteerReceipt>> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_contains_request_and_credentials() {
        let request = AgentRequest {
            location: Url::parse("https://forge.example.com/o/r/issues/1").unwrap(),
            message: "do it".into(),
        };
        let ctx = AgentContext {
            workspace: PathBuf::from("/tmp/ws"),
            forge: Some(ForgeKind::Forgejo),
            repository: "o/r".into(),
            requester: "alice".into(),
            issue_number: Some(1),
            is_pull_request: false,
            linked_issue: None,
            title: None,
            reply_target: ReplyTarget::Conversation,
            credentials: vec![("FORGEJO_TOKEN".into(), "secret".into())],
            live_output: None,
            reported_model: Default::default(),
        };
        let env = ctx.environment(&request);
        assert!(
            env.iter()
                .any(|(k, v)| k == "FORGEJO_TOKEN" && v == "secret")
        );
        assert!(env.iter().any(|(k, _)| k == "FORGE_BOT_LOCATION"));
        assert!(
            env.iter()
                .any(|(k, v)| k == "FORGE_BOT_ISSUE_NUMBER" && v == "1")
        );
    }

    #[test]
    fn live_output_keeps_only_recent_bytes() {
        let output = LiveOutput::default();
        output.append(&vec![b'a'; LiveOutput::LIMIT]);
        output.append(b"end");
        let text = output.text();
        assert_eq!(text.len(), LiveOutput::LIMIT);
        assert!(text.ends_with("end"));
    }
}

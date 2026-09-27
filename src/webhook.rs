//! HTTP webhook receiver.
//!
//! One route per forge accepts the raw webhook, delegates verification and
//! parsing to the matching [`ForgeAdapter`], extracts `@agent` mentions, and
//! hands actionable jobs to the [`Dispatcher`].

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;

use crate::agent::AgentRegistry;
use crate::config::Config;
use crate::error::BotError;
use crate::forge::ForgeAdapter;
use crate::mention::extract_mention;
use crate::session::Dispatcher;
use crate::session::status;

/// Shared state for the webhook server.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub adapters: Arc<std::collections::HashMap<String, Arc<dyn ForgeAdapter>>>,
    pub agents: Arc<AgentRegistry>,
    pub dispatcher: Arc<Dispatcher>,
    dedupe: Arc<Mutex<RecentComments>>,
}

impl AppState {
    pub fn new(
        config: Arc<Config>,
        adapters: std::collections::HashMap<String, Arc<dyn ForgeAdapter>>,
        agents: Arc<AgentRegistry>,
        dispatcher: Arc<Dispatcher>,
    ) -> Self {
        Self {
            config,
            adapters: Arc::new(adapters),
            agents,
            dispatcher,
            dedupe: Arc::new(Mutex::new(RecentComments::new(1024))),
        }
    }

    fn seen(&self, key: &str) -> bool {
        self.dedupe
            .lock()
            .expect("dedupe mutex poisoned")
            .insert(key)
    }
}

/// A small bounded set remembering recently handled comments.
struct RecentComments {
    order: VecDeque<String>,
    set: HashSet<String>,
    capacity: usize,
}

impl RecentComments {
    fn new(capacity: usize) -> Self {
        Self {
            order: VecDeque::new(),
            set: HashSet::new(),
            capacity,
        }
    }

    /// Returns true when the key was already present.
    fn insert(&mut self, key: &str) -> bool {
        if self.set.contains(key) {
            return true;
        }
        self.set.insert(key.to_owned());
        self.order.push_back(key.to_owned());
        while self.order.len() > self.capacity {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
        false
    }
}

/// Stable key used to ignore duplicate deliveries. Comments use their id;
/// description events use the issue number plus a hash of the body, so an edit
/// that changes the text is treated as a new delivery but a re-delivery is not.
fn delivery_key(message: &crate::forge::ForgeMessage) -> String {
    match message.comment_id {
        Some(id) => format!("{}:{}:c{id}", message.forge, message.repository),
        None => {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(message.body.as_bytes());
            let digest = hex::encode(hasher.finalize());
            format!(
                "{}:{}:d{}:{digest}",
                message.forge,
                message.repository,
                message.number.unwrap_or_default()
            )
        }
    }
}

/// Build the axum router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/healthz", get(healthz))
        .route("/status", get(status_page))
        .route("/status.json", get(status_json))
        .route("/webhooks/{forge}", post(receive))
        .route("/webhook/{forge}", post(receive))
        .with_state(state)
}

async fn root(State(state): State<AppState>) -> impl IntoResponse {
    Json(json!({
        "name": "forge-bot",
        "version": env!("CARGO_PKG_VERSION"),
        "forges": state.adapters.keys().collect::<Vec<_>>(),
        "agents": state.agents.names(),
        "mention": state.config.trigger(),
        "status": "/status",
    }))
}

/// Human-readable status page for every thread the bot knows about.
async fn status_page(State(state): State<AppState>, Query(query): Query<StatusQuery>) -> Response {
    match state.dispatcher.threads() {
        Ok(threads) => Html(status::render_html(&threads, query.q.as_deref())).into_response(),
        Err(error) => {
            tracing::warn!(%error, "failed to build thread status");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to read thread status: {error}\n"),
            )
                .into_response()
        }
    }
}

/// Machine-readable form of the status page, optionally filtered by `q`.
async fn status_json(State(state): State<AppState>, Query(query): Query<StatusQuery>) -> Response {
    match state.dispatcher.threads() {
        Ok(threads) => {
            Json(json!({ "threads": status::search(&threads, query.q.as_deref()) })).into_response()
        }
        Err(error) => {
            tracing::warn!(%error, "failed to build thread status");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": error.to_string() })),
            )
                .into_response()
        }
    }
}

/// Optional search query for the status routes: a comment or issue/PR URL.
#[derive(Debug, Default, serde::Deserialize)]
struct StatusQuery {
    q: Option<String>,
}

async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok\n")
}

/// Handle a webhook delivery.
async fn receive(
    State(state): State<AppState>,
    Path(forge): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let Some(adapter) = state.adapters.get(&forge).cloned() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("unknown forge `{forge}`") })),
        );
    };

    // Verify and parse.
    let mut messages = match adapter.handle(&headers, &body) {
        Ok(messages) => messages,
        Err(BotError::Verification(reason)) => {
            tracing::warn!(forge = %forge, %reason, "rejected webhook");
            return (StatusCode::UNAUTHORIZED, Json(json!({ "error": reason })));
        }
        Err(error) => {
            tracing::warn!(forge = %forge, %error, "failed to handle webhook");
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": error.to_string() })),
            );
        }
    };

    // Some forges omit part of an event from the payload; let the adapter fill
    // it in (Forgejo inline review comments).
    if let Err(error) = adapter.enrich(&mut messages, &headers, &body).await {
        tracing::warn!(forge = %forge, %error, "failed to enrich webhook");
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": error.to_string() })),
        );
    }

    let mut accepted = 0usize;
    tracing::debug!(
        forge = %forge,
        event = %adapter.event(&headers),
        messages = messages.len(),
        "webhook received"
    );
    for message in messages {
        // Never react to our own comments.
        if state.dispatcher.policy().is_ignored(&message.author) {
            continue;
        }

        let Some(mention) = extract_mention(&message.body, state.config.trigger()) else {
            continue;
        };

        let dedupe_key = delivery_key(&message);
        if state.seen(&dedupe_key) {
            tracing::debug!(%dedupe_key, "ignoring duplicate webhook delivery");
            continue;
        }

        let agent_name = mention
            .agent
            .clone()
            .filter(|a| !a.is_empty())
            .unwrap_or_else(|| state.agents.default_name().to_owned());

        match state.dispatcher.submit(message, mention, &agent_name).await {
            Ok(job_id) => {
                accepted += 1;
                tracing::info!(%job_id, agent = %agent_name, "accepted trigger");
            }
            Err(BotError::Unauthorized(reason)) => {
                tracing::info!(%reason, "ignored unauthorized trigger");
            }
            Err(BotError::UnknownAgent(name)) => {
                tracing::warn!(agent = %name, "trigger referenced an unknown agent");
            }
            Err(error) => {
                tracing::error!(%error, "failed to enqueue job");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": error.to_string() })),
                );
            }
        }
    }

    (StatusCode::ACCEPTED, Json(json!({ "accepted": accepted })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedupe_remembers_and_evicts() {
        let mut recent = RecentComments::new(2);
        assert!(!recent.insert("a"));
        assert!(recent.insert("a"));
        assert!(!recent.insert("b"));
        assert!(!recent.insert("c"));
        assert!(!recent.insert("a")); // evicted
    }

    #[test]
    fn delivery_key_uses_id_or_body_hash() {
        use crate::location::ForgeKind;
        use url::Url;

        let base = |comment_id, body: &str| crate::forge::ForgeMessage {
            forge: ForgeKind::Forgejo,
            location: Url::parse("http://forge.local/a/b/issues/3").unwrap(),
            body: body.to_owned(),
            author: "u".into(),
            repository: "a/b".into(),
            comment_id,
            number: Some(3),
            is_pull_request: false,
            linked_issue: None,
            event: "issues".into(),
            title: None,
            reply_target: Default::default(),
        };

        // Comments key on their id.
        assert_eq!(delivery_key(&base(Some(5), "hi")), "forgejo:a/b:c5");

        // Descriptions key on the body, so an edit is a new delivery but a
        // re-delivery of the same text is not.
        let first = delivery_key(&base(None, "@agent do it"));
        assert_eq!(first, delivery_key(&base(None, "@agent do it")));
        assert_ne!(first, delivery_key(&base(None, "@agent do it now")));
    }
}

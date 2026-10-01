//! HTTP webhook receiver.
//!
//! One route per forge accepts the raw webhook, delegates verification and
//! parsing to the matching [`ForgeAdapter`], extracts `@agent` mentions, and
//! hands actionable jobs to the [`Dispatcher`].

use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;

use crate::agent::AgentRegistry;
use crate::auto_trigger::AutoTrigger;
use crate::config::Config;
use crate::error::BotError;
use crate::forge::ForgeAdapter;
use crate::notify::{RecentComments, delivery_key};
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
    /// Latest client diagnostic report uploaded from `/notifications`. The
    /// Android notification path depends on OS/browser state the server cannot
    /// see, so the page uploads its self-test here for the operator to read.
    diagnostics: Arc<Mutex<Option<serde_json::Value>>>,
    auto: Arc<AutoTrigger>,
}

impl AppState {
    pub fn new(
        config: Arc<Config>,
        adapters: std::collections::HashMap<String, Arc<dyn ForgeAdapter>>,
        agents: Arc<AgentRegistry>,
        dispatcher: Arc<Dispatcher>,
    ) -> Self {
        let auto = Arc::new(AutoTrigger::new(&config));
        Self {
            config,
            adapters: Arc::new(adapters),
            agents,
            dispatcher,
            dedupe: Arc::new(Mutex::new(RecentComments::new(1024))),
            diagnostics: Arc::new(Mutex::new(None)),
            auto,
        }
    }

    fn seen(&self, key: &str) -> bool {
        self.dedupe
            .lock()
            .expect("dedupe mutex poisoned")
            .insert(key)
    }
}

/// Build the axum router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/healthz", get(healthz))
        .route("/status", get(status_page))
        .route("/status/details", get(status_details))
        .route("/status.json", get(status_json))
        .route("/notifications", get(notifications_page))
        .route("/notifications.json", get(notifications_json))
        .route("/notifications.webmanifest", get(notifications_manifest))
        .route("/notifications/icon.png", get(notifications_icon))
        .route("/notifications/badge.png", get(notifications_badge))
        .route(
            "/notifications/diagnostics",
            get(notifications_diagnostics).post(notifications_diagnostics_upload),
        )
        .route("/notifications/sw.js", get(notifications_service_worker))
        .route("/notify", get(notifications_page))
        .route("/notify.json", get(notifications_json))
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
        "notifications": "/notifications",
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

/// Run history for a thread selected from the status table.
async fn status_details(
    State(state): State<AppState>,
    Query(query): Query<DetailsQuery>,
) -> Response {
    match state.dispatcher.threads() {
        Ok(threads) => match threads.iter().find(|thread| thread.key == query.key) {
            Some(thread) => {
                let session = state.dispatcher.session(&query.key);
                let live_output = session
                    .as_ref()
                    .and_then(|session| session.runs.last())
                    .filter(|run| run.finished_at.is_none())
                    .and_then(|run| state.dispatcher.live_output(run.job_id))
                    .map(|output| output.text());
                Html(status::render_details(
                    thread,
                    session.as_ref(),
                    live_output.as_deref(),
                ))
                .into_response()
            }
            None => (StatusCode::NOT_FOUND, "thread not found\n").into_response(),
        },
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

#[derive(serde::Deserialize)]
struct DetailsQuery {
    key: String,
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

/// Human notification page: browser system notifications on desktop and on
/// mobile over HTTPS through a service worker.
async fn notifications_page(State(state): State<AppState>) -> Html<String> {
    let humans: Vec<String> = state
        .dispatcher
        .identities()
        .humans()
        .iter()
        .map(|human| human.login.clone())
        .collect();
    Html(crate::notify::render_html(&humans))
}

#[derive(Debug, Default, serde::Deserialize)]
struct NotificationsQuery {
    recipient: String,
    #[serde(default)]
    after: u64,
    /// Generation the page last saw. A mismatch means the server restarted and
    /// ids began again, so the cursor must be reset.
    #[serde(default)]
    generation: Option<String>,
}

/// Notifications for one human newer than `after`.
async fn notifications_json(
    State(state): State<AppState>,
    Query(query): Query<NotificationsQuery>,
) -> Response {
    if query.recipient.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "missing recipient" })),
        )
            .into_response();
    }
    let notifier = state.dispatcher.notifier();
    let generation = notifier.generation();
    let notifications = notifier.since(&query.recipient, query.after, query.generation.as_deref());
    Json(json!({
        "generation": generation,
        "notifications": notifications,
    }))
    .into_response()
}

/// The web app manifest linked from the notification page, so an iOS or
/// Android "Add to Home Screen" install is a standalone app rather than a
/// bookmark.
async fn notifications_manifest() -> Response {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "application/manifest+json",
        )],
        crate::notify::manifest_json(),
    )
        .into_response()
}

/// The notification large icon (also the web app icon).
async fn notifications_icon() -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "image/png")],
        axum::body::Bytes::from_static(crate::notify::notification_icon_png()),
    )
        .into_response()
}

/// The monochrome badge Android uses for the status-bar icon.
async fn notifications_badge() -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "image/png")],
        axum::body::Bytes::from_static(crate::notify::notification_badge_png()),
    )
        .into_response()
}

/// Upload a diagnostic report from the notification page.
async fn notifications_diagnostics_upload(
    State(state): State<AppState>,
    Json(report): Json<serde_json::Value>,
) -> Response {
    if !report.is_object() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "diagnostics must be a JSON object" })),
        )
            .into_response();
    }
    tracing::info!(report = %report, "received notification diagnostics");
    *state
        .diagnostics
        .lock()
        .expect("diagnostics mutex poisoned") = Some(report);
    StatusCode::NO_CONTENT.into_response()
}

/// Return the most recently uploaded diagnostic report.
async fn notifications_diagnostics(State(state): State<AppState>) -> Response {
    match state
        .diagnostics
        .lock()
        .expect("diagnostics mutex poisoned")
        .clone()
    {
        Some(report) => Json(report).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no diagnostics uploaded yet" })),
        )
            .into_response(),
    }
}

/// The service worker the notification page registers so mobile browsers can
/// use `ServiceWorkerRegistration.showNotification`.
async fn notifications_service_worker() -> Response {
    (
        [
            (
                axum::http::header::CONTENT_TYPE,
                "text/javascript; charset=utf-8",
            ),
            (
                axum::http::header::HeaderName::from_static("service-worker-allowed"),
                "/",
            ),
        ],
        crate::notify::service_worker_js(),
    )
        .into_response()
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
        // An agent that mentions a configured human is asking for help. Record
        // a notification for the web page before the agent's own comment is
        // ignored for routing. The poller shares this recorder and its dedupe,
        // so a comment seen by both ingesters notifies only once.
        state.dispatcher.record_human_notifications(&message);

        // Never react to our own comments.
        if state.dispatcher.policy().is_ignored(&message.author) {
            continue;
        }

        // Explicit mode addresses a configured login; legacy mode uses the
        // global trigger. An ambiguous mention is ignored before any ack.
        let (mention, agent_name) = match state.dispatcher.route(&message) {
            Ok(Some(routed)) => routed,
            Ok(None) => continue,
            Err(BotError::Unauthorized(reason)) => {
                tracing::info!(%reason, "ignored unroutable trigger");
                continue;
            }
            Err(error) => {
                tracing::warn!(%error, "failed to route trigger");
                continue;
            }
        };

        let dedupe_key = delivery_key(&message);
        if state.seen(&dedupe_key) {
            tracing::debug!(%dedupe_key, "ignoring duplicate webhook delivery");
            continue;
        }

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

    // A mentioned PR description already started a job for this delivery.
    if forge == "forgejo" && accepted == 0 {
        match state
            .auto
            .handle(
                &state.config,
                &state.dispatcher,
                &adapter.event(&headers),
                &body,
            )
            .await
        {
            Ok(count) => accepted += count,
            Err(error) => {
                tracing::warn!(%error, "failed to process automatic trigger");
                return (
                    StatusCode::BAD_GATEWAY,
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

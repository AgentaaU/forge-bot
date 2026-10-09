//! HTTP webhook receiver.
//!
//! One route per forge accepts the raw webhook, delegates verification and
//! parsing to the matching [`ForgeAdapter`], extracts `@agent` mentions, and
//! hands actionable jobs to the [`Dispatcher`].

use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;

use crate::agent::{AgentRegistry, EFFORT_LEVELS, is_effort_level};
use crate::auto_trigger::AutoTrigger;
use crate::config::Config;
use crate::error::BotError;
use crate::forge::ForgeAdapter;
use crate::notify::{RecentComments, delivery_key};
use crate::session::statistics;
use crate::session::status;
use crate::session::{Dispatcher, ThreadState, UserAgentSettings};

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
        .route("/admin", get(admin_page))
        .route("/admin/reset-cooldown", post(reset_cooldown))
        .route("/admin/agent-ranking", post(save_agent_ranking))
        .route("/admin/terminate-thread", post(terminate_thread))
        .route("/admin/agent-settings", post(save_agent_settings))
        .route("/status", get(status_page))
        .route("/status/details", get(status_details))
        .route("/status.json", get(status_json))
        .route("/statistics", get(statistics_page))
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
        .route(
            "/notifications/push",
            get(push_config)
                .post(push_subscribe)
                .delete(push_unsubscribe),
        )
        .route("/notifications/ca.crt", get(notifications_ca_cert))
        .route("/notify", get(notifications_page))
        .route("/notify.json", get(notifications_json))
        .route("/webhooks/{forge}", post(receive))
        .route("/webhook/{forge}", post(receive))
        .with_state(state)
}

/// Index of the browser pages. Clients that prefer HTML (see
/// [`accepts_html`]) get an HTML page linking to every GET page; other clients
/// keep the JSON summary. Both representations vary on `Accept`, so shared
/// caches never hand one representation to a client that asked for the other.
async fn root(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !accepts_html(&headers) {
        return (
            [(header::VARY, "Accept")],
            Json(json!({
                "name": "forge-bot",
                "version": env!("CARGO_PKG_VERSION"),
                "forges": state.adapters.keys().collect::<Vec<_>>(),
                "agents": state.agents.names(),
                "mention": state.config.trigger(),
                "status": "/status",
                "statistics": "/statistics",
                "admin": "/admin",
                "notifications": "/notifications",
            })),
        )
            .into_response();
    }

    let pages = [
        ("/status", "Live status of every known thread"),
        ("/statistics", "Token use per model, repository and thread"),
        (
            "/admin",
            "Agent cooldowns, models, effort and running threads",
        ),
        (
            "/notifications",
            "Browser and mobile notifications for humans",
        ),
        ("/healthz", "Liveness check"),
    ];
    let mut links = String::new();
    for (path, description) in pages {
        let path = status::escape_html(path);
        links.push_str(&format!(
            "<tr><td><a href=\"{path}\">{path}</a></td><td>{}</td></tr>",
            status::escape_html(description)
        ));
    }
    let forges = state
        .adapters
        .keys()
        .map(|forge| status::escape_html(forge))
        .collect::<Vec<_>>()
        .join(", ");
    let agents = state
        .agents
        .names()
        .iter()
        .map(|agent| status::escape_html(agent))
        .collect::<Vec<_>>()
        .join(", ");
    (
        [(header::VARY, "Accept")],
        Html(
            include_str!("../web/index.html")
                .replace("{version}", env!("CARGO_PKG_VERSION"))
                .replace("{mention}", &status::escape_html(state.config.trigger()))
                .replace("{forges}", &forges)
                .replace("{agents}", &agents)
                .replace("{links}", &links),
        ),
    )
        .into_response()
}

/// Whether the `Accept` header makes `text/html` the preferred representation.
///
/// Per RFC 9110 §12.5.1 the most specific matching media range decides: an
/// exact `text/html` range beats `text/*`, and `*/*` is ignored so that
/// generic clients (curl, API libraries) keep the JSON summary. Types and
/// subtypes compare case-insensitively, parameters other than `q` are
/// ignored, and a quality of zero (RFC 9110 §12.4.2) means "not acceptable".
fn accepts_html(headers: &HeaderMap) -> bool {
    let mut exact: Option<f32> = None;
    let mut subtype_wildcard: Option<f32> = None;
    for value in headers.get_all(header::ACCEPT) {
        let Ok(value) = value.to_str() else {
            continue;
        };
        for range in value.split(',') {
            let mut parts = range.split(';');
            let media_type = parts.next().unwrap_or_default().trim();
            let Some(quality) = parse_quality(parts) else {
                continue;
            };
            let slot = if media_type.eq_ignore_ascii_case("text/html") {
                &mut exact
            } else if media_type.eq_ignore_ascii_case("text/*") {
                &mut subtype_wildcard
            } else {
                continue;
            };
            *slot = Some(slot.map_or(quality, |best| best.max(quality)));
        }
    }
    exact.or(subtype_wildcard).is_some_and(|q| q > 0.0)
}

/// Reads the `q` parameter from a media range's parameters; a missing `q`
/// means 1. Returns `None` for a malformed quality so the range is skipped.
fn parse_quality<'a>(parameters: impl Iterator<Item = &'a str>) -> Option<f32> {
    let mut quality = 1.0;
    for parameter in parameters {
        let Some((name, value)) = parameter.split_once('=') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case("q") {
            continue;
        }
        quality = parse_qvalue(value.trim())?;
    }
    Some(quality)
}

/// RFC 9110 §12.4.2 `qvalue`: `0[.0-3 digits]` or `1[.0-3 zeros]`.
fn parse_qvalue(value: &str) -> Option<f32> {
    let (whole, fraction) = match value.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (value, None),
    };
    let fraction_ok =
        |digit: fn(char) -> bool| fraction.is_none_or(|f| f.len() <= 3 && f.chars().all(digit));
    match whole {
        "0" if fraction_ok(|c| c.is_ascii_digit()) => value.parse().ok(),
        "1" if fraction_ok(|c| c == '0') => value.parse().ok(),
        _ => None,
    }
}

/// Operator controls use the same registry as dispatch and fallback selection.
async fn admin_page(State(state): State<AppState>) -> Html<String> {
    let mut rows = String::new();
    let ranked = state.agents.ordered_names();
    let mut names = ranked.clone();
    names.extend(
        state
            .agents
            .names()
            .into_iter()
            .filter(|name| !ranked.contains(name)),
    );
    for name in names {
        let cooldown = state.agents.cooldown_remaining(&name);
        let availability = match cooldown {
            Some(remaining) => format!(
                "{} ({} seconds remaining)",
                state
                    .agents
                    .unavailable_reason(&name)
                    .map(|reason| reason.label())
                    .unwrap_or("unavailable"),
                remaining.as_secs().saturating_add(1),
            ),
            None => "Available".to_owned(),
        };
        let priority = ranked.iter().position(|candidate| candidate == &name)
            .map(|index| format!("<span>{}</span> <button data-move=\"up\">Move up</button> <button data-move=\"down\">Move down</button>", index + 1))
            .unwrap_or_else(|| "Outside automatic selection".to_owned());
        let ranked_attribute = if ranked.contains(&name) {
            "data-ranked"
        } else {
            ""
        };
        let name = status::escape_html(&name);
        rows.push_str(&format!(
            include_str!("../web/admin-row.html"),
            availability = availability,
            priority = priority,
            ranked_attribute = ranked_attribute,
            name = name
        ));
    }
    let user_rows = agent_user_rows(&state);
    let thread_rows = running_thread_rows(&state);
    Html(format!(
        include_str!("../web/admin.html"),
        capacity = worker_capacity_summary(&state.dispatcher.capacity()),
        rows = rows,
        user_rows = user_rows,
        thread_rows = thread_rows
    ))
}

#[derive(serde::Deserialize)]
struct AgentRankingRequest {
    agents: Vec<String>,
}

async fn save_agent_ranking(
    State(state): State<AppState>,
    Json(request): Json<AgentRankingRequest>,
) -> Response {
    if !state.agents.set_ranking(request.agents) {
        return (
            StatusCode::BAD_REQUEST,
            "ranking must contain every automatic-selection agent exactly once\n",
        )
            .into_response();
    }
    StatusCode::NO_CONTENT.into_response()
}

/// One sentence describing worker slots, so an operator can see whether new
/// mentions will start at once or wait behind the running conversations.
fn worker_capacity_summary(capacity: &crate::session::queue::Capacity) -> String {
    let plural = |n: usize, word: &str| {
        if n == 1 {
            format!("{n} {word}")
        } else {
            format!("{n} {word}s")
        }
    };
    let queued = if capacity.queued == 0 {
        "no jobs queued".to_owned()
    } else {
        format!("{} queued", plural(capacity.queued, "job"))
    };
    format!(
        "{} of {} busy, {} free; {}.",
        capacity.busy,
        plural(capacity.workers, "worker slot"),
        capacity.free(),
        queued,
    )
}

/// Longest model ID an operator may enter. Real identifiers are far shorter;
/// the cap only bounds what a single request can store.
const MAX_MODEL_LEN: usize = 200;

/// One row per configured agent user, with its current overrides and the
/// controls to change them. Human recipients never run an agent, so they are
/// left out.
fn agent_user_rows(state: &AppState) -> String {
    let mut rows = String::new();
    let reported_models = state.dispatcher.reported_models();
    for (id, user) in &state.config.users {
        if !user.role.is_agent() {
            continue;
        }
        let agent = user
            .agent
            .clone()
            .unwrap_or_else(|| state.agents.default_name().to_owned());
        let settings = state.dispatcher.user_settings(id);
        let supported = state
            .agents
            .get(&agent)
            .is_ok_and(|agent| agent.supports_effort());
        let effort_options = effort_options(settings.effort.as_deref(), supported);
        rows.push_str(&format!(
            include_str!("../web/admin-user-row.html"),
            user = status::escape_html(id),
            agent = status::escape_html(&agent),
            model_options = model_options(
                settings.model.as_deref(),
                user.agent_model.as_deref(),
                state
                    .config
                    .users
                    .iter()
                    .filter(|(_, candidate)| {
                        candidate.role.is_agent()
                            && candidate
                                .agent
                                .as_deref()
                                .unwrap_or(&state.agents.default_name())
                                == agent
                    })
                    .flat_map(|(candidate_id, candidate)| {
                        [
                            candidate.agent_model.clone(),
                            state.dispatcher.user_settings(candidate_id).model,
                        ]
                    })
                    .flatten()
                    .chain(
                        reported_models
                            .iter()
                            .filter(|(reporting_agent, _)| reporting_agent == &agent)
                            .map(|(_, model)| model.clone())
                    ),
            ),
            effort_disabled = if supported { "" } else { " disabled" },
            effort_options = effort_options,
        ));
    }
    rows
}

/// Model choices come from this adapter's configuration and reported runs,
/// never a hardcoded provider catalog. Merely rendering them selects no model.
fn model_options(
    current: Option<&str>,
    configured: Option<&str>,
    known: impl IntoIterator<Item = String>,
) -> String {
    let default_label = configured
        .map(|model| format!("Configured default ({model})"))
        .unwrap_or_else(|| "Agent default".to_owned());
    let mut options = format!(
        "<option value=\"\"{}>{}</option>",
        if current.is_none() { " selected" } else { "" },
        status::escape_html(&default_label),
    );
    let models: std::collections::BTreeSet<_> = known
        .into_iter()
        .chain(current.map(str::to_owned))
        .chain(configured.map(str::to_owned))
        .filter(|model| !model.is_empty())
        .collect();
    for model in models {
        let escaped = status::escape_html(&model);
        options.push_str(&format!(
            "<option value=\"{escaped}\"{}>{escaped}</option>",
            if current == Some(model.as_str()) {
                " selected"
            } else {
                ""
            },
        ));
    }
    options.push_str("<option data-custom-model>Custom model…</option>");
    options
}

/// `<option>`s for the effort select. The empty choice keeps the agent's own
/// default. An agent without effort support gets one disabled explanation.
fn effort_options(current: Option<&str>, supported: bool) -> String {
    if !supported {
        return "<option value=\"\">Not supported by this agent</option>".to_owned();
    }
    let mut options = String::from("<option value=\"\">Agent default</option>");
    for level in EFFORT_LEVELS {
        let selected = if current == Some(level) {
            " selected"
        } else {
            ""
        };
        options.push_str(&format!(
            "<option value=\"{level}\"{selected}>{level}</option>"
        ));
    }
    options
}

/// One row per thread with an agent run in flight, each with a terminate
/// button keyed by the thread's conversation key.
fn running_thread_rows(state: &AppState) -> String {
    let threads = match state.dispatcher.threads() {
        Ok(threads) => threads,
        Err(error) => {
            tracing::warn!(%error, "failed to list threads for the admin page");
            return String::new();
        }
    };
    let now = chrono::Utc::now();
    let mut rows = String::new();
    for thread in threads
        .iter()
        .filter(|thread| thread.state == ThreadState::Running)
    {
        let number = thread.number.map(|n| format!(" #{n}")).unwrap_or_default();
        let since = thread
            .running_since
            .map(|since| status::humanize_age(now, since))
            .unwrap_or_else(|| "—".to_owned());
        rows.push_str(&format!(
            include_str!("../web/admin-thread-row.html"),
            location = status::escape_html(&thread.location),
            repository = status::escape_html(&thread.repository),
            number = status::escape_html(&number),
            kind = status::escape_html(&thread.thread_type),
            agent = status::escape_html(&thread.agent),
            since = status::escape_html(&since),
            key = status::escape_html(&thread.key),
        ));
    }
    rows
}

#[derive(serde::Deserialize)]
struct TerminateThread {
    key: String,
}

// Same JSON-only guard as cooldown resets: a cross-origin form cannot stop a run.
async fn terminate_thread(
    State(state): State<AppState>,
    Json(request): Json<TerminateThread>,
) -> Response {
    if !state.dispatcher.terminate(&request.key) {
        return (StatusCode::NOT_FOUND, "thread is not running\n").into_response();
    }
    tracing::info!(key = %request.key, "thread terminated by admin");
    StatusCode::NO_CONTENT.into_response()
}

#[derive(serde::Deserialize)]
struct AgentSettingsRequest {
    user: String,
    /// Empty restores the configured `agent_model`.
    #[serde(default)]
    model: String,
    /// Empty restores the agent's own effort default.
    #[serde(default)]
    effort: String,
}

// JSON-only, like the other admin controls, so a cross-origin form cannot
// change which model or effort a user's runs get.
async fn save_agent_settings(
    State(state): State<AppState>,
    Json(request): Json<AgentSettingsRequest>,
) -> Response {
    let Some(user) = state
        .config
        .users
        .get(&request.user)
        .filter(|user| user.role.is_agent())
    else {
        return (StatusCode::NOT_FOUND, "unknown agent user\n").into_response();
    };
    let model = request.model.trim();
    if model.len() > MAX_MODEL_LEN || model.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return (StatusCode::BAD_REQUEST, "invalid model\n").into_response();
    }
    let effort = request.effort.trim();
    if !effort.is_empty() {
        if !is_effort_level(effort) {
            return (
                StatusCode::BAD_REQUEST,
                format!("effort must be one of {}\n", EFFORT_LEVELS.join(", ")),
            )
                .into_response();
        }
        let agent = user
            .agent
            .clone()
            .unwrap_or_else(|| state.agents.default_name().to_owned());
        if !state
            .agents
            .get(&agent)
            .is_ok_and(|agent| agent.supports_effort())
        {
            return (
                StatusCode::BAD_REQUEST,
                format!("agent `{agent}` does not support an effort level\n"),
            )
                .into_response();
        }
    }
    let settings = UserAgentSettings {
        model: (!model.is_empty()).then(|| model.to_owned()),
        effort: (!effort.is_empty()).then(|| effort.to_owned()),
    };
    if let Err(error) = state.dispatcher.set_user_settings(&request.user, settings) {
        tracing::warn!(user = %request.user, %error, "failed to save agent settings");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to save agent settings\n",
        )
            .into_response();
    }
    tracing::info!(user = %request.user, "agent settings changed by admin");
    StatusCode::NO_CONTENT.into_response()
}

#[derive(serde::Deserialize)]
struct ResetCooldown {
    agent: String,
}

// Requiring JSON keeps cross-origin HTML forms from changing cooldown state.
async fn reset_cooldown(
    State(state): State<AppState>,
    Json(request): Json<ResetCooldown>,
) -> Response {
    if state.agents.get(&request.agent).is_err() {
        return (StatusCode::NOT_FOUND, "unknown agent\n").into_response();
    }
    state.agents.mark_available(&request.agent);
    tracing::info!(agent = %request.agent, "agent cooldown reset by admin");
    StatusCode::NO_CONTENT.into_response()
}

/// Human-readable status page for every thread the bot knows about.
async fn status_page(State(state): State<AppState>, Query(query): Query<StatusQuery>) -> Response {
    match state.dispatcher.threads() {
        Ok(threads) => fresh_html(status::render_html(&threads, query.q.as_deref())),
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
                fresh_html(status::render_details(
                    thread,
                    session.as_ref(),
                    live_output.as_deref(),
                ))
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

/// Wrap a status page in a response that is never cached. The pages refresh
/// themselves every couple of seconds, so a cached copy would keep showing
/// stale live output even after the agent made progress.
fn fresh_html(body: String) -> Response {
    let mut response = Html(body).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
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

/// Prompt-token consumption per thread and repository, split by model.
async fn statistics_page(State(state): State<AppState>) -> Response {
    let sessions = state.dispatcher.sessions();
    let stats = statistics::build(&sessions, |location| {
        state.dispatcher.web_location(location)
    });
    fresh_html(statistics::render_html(&stats))
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
    // Only offer the download when the file is actually present, so the page
    // never links to a 404.
    let ca_cert = state
        .config
        .notifications
        .ca_cert_path
        .as_ref()
        .is_some_and(|path| path.is_file());
    Html(crate::notify::render_html(&humans, ca_cert))
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

/// The optional CA certificate offered as a download on the notification page,
/// so a mobile device can install the private CA and register the notification
/// service worker.
async fn notifications_ca_cert(State(state): State<AppState>) -> Response {
    let Some(path) = state.config.notifications.ca_cert_path.as_ref() else {
        return (StatusCode::NOT_FOUND, "no CA certificate configured\n").into_response();
    };
    match std::fs::read(path) {
        Ok(bytes) => (
            [
                (
                    axum::http::header::CONTENT_TYPE,
                    "application/x-x509-ca-cert",
                ),
                (
                    axum::http::header::CONTENT_DISPOSITION,
                    "attachment; filename=\"forge-bot-ca.crt\"",
                ),
            ],
            bytes,
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "failed to read CA certificate");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to read CA certificate\n",
            )
                .into_response()
        }
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
#[tracing::instrument(skip_all, fields(forge = %forge, event = tracing::field::Empty, delivery_id = tracing::field::Empty))]
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

    let event = adapter.event(&headers);
    let delivery_id = headers
        .get("x-forgejo-delivery")
        .or_else(|| headers.get("x-gitea-delivery"))
        .or_else(|| headers.get("x-github-delivery"))
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    tracing::Span::current().record("event", &event);
    tracing::Span::current().record("delivery_id", delivery_id);

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
    let extracted = messages.len();
    tracing::info!(messages = extracted, "webhook messages extracted");
    for message in messages {
        tracing::info!(
            author = %message.author,
            repository = %message.repository,
            number = ?message.number,
            comment_id = ?message.comment_id,
            "processing webhook message"
        );
        // An agent that mentions a configured human is asking for help. Record
        // a notification for the web page before the agent's own comment is
        // ignored for routing. The poller shares this recorder and its dedupe,
        // so a comment seen by both ingesters notifies only once.
        state.dispatcher.record_human_notifications(&message);

        // Never react to our own comments.
        if state.dispatcher.policy().is_ignored(&message.author) {
            tracing::info!(author = %message.author, reason = "ignored_author", "webhook message skipped");
            continue;
        }

        // Explicit mode addresses a configured login; legacy mode uses the
        // global trigger. An ambiguous mention is ignored before any ack.
        let (mention, agent_name) = match state.dispatcher.route(&message) {
            Ok(Some(routed)) => routed,
            Ok(None) => {
                tracing::info!(author = %message.author, reason = "no_configured_recipient", "webhook message skipped");
                continue;
            }
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
            tracing::info!(%dedupe_key, reason = "duplicate", "webhook message skipped");
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

    tracing::info!(
        messages = extracted,
        accepted,
        "webhook processing completed"
    );
    (StatusCode::ACCEPTED, Json(json!({ "accepted": accepted })))
}

async fn push_config(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(match state.config.notifications.transport {
        crate::config::NotificationTransport::Polling => json!({"transport": "polling"}),
        crate::config::NotificationTransport::WebPush => match state.dispatcher.push() {
            Some(push) => json!({"transport": "web_push", "public_key": push.public_key}),
            None => json!({"transport": "web_push", "error":
                "Web Push requires notifications.vapid_private_key_path and notifications.vapid_subject."}),
        },
    })
}

#[derive(serde::Deserialize)]
struct PushSubscriptionRequest {
    recipient: String,
    subscription: web_push::SubscriptionInfo,
}

async fn push_subscribe(
    State(state): State<AppState>,
    Json(request): Json<PushSubscriptionRequest>,
) -> Response {
    let Some(push) = state.dispatcher.push() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    if !state.dispatcher.identities().users().iter().any(|user| {
        user.role == crate::config::UserRole::Human
            && user.login.eq_ignore_ascii_case(&request.recipient)
    }) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    match push.subscribe(&request.recipient, request.subscription) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => (StatusCode::BAD_REQUEST, "invalid push subscription").into_response(),
    }
}

#[derive(serde::Deserialize)]
struct PushUnsubscribeRequest {
    endpoint: String,
}

async fn push_unsubscribe(
    State(state): State<AppState>,
    Json(request): Json<PushUnsubscribeRequest>,
) -> Response {
    let Some(push) = state.dispatcher.push() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match push.unsubscribe(&request.endpoint) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_choices_preserve_defaults_and_escape_and_deduplicate_ids() {
        let options = model_options(
            Some("saved"),
            Some("configured"),
            ["reported", "reported", "<model\"&>", ""].map(str::to_owned),
        );
        assert!(options.contains("<option value=\"saved\" selected>saved</option>"));
        assert!(options.contains("Configured default (configured)"));
        assert!(options.contains("<option value=\"configured\">configured</option>"));
        assert_eq!(options.matches("value=\"reported\"").count(), 1);
        assert!(options.contains("&lt;model&quot;&amp;&gt;"));
        assert!(!options.contains("<model"));
        assert_eq!(options.matches(" selected").count(), 1);

        let options = model_options(None, None, ["reported".to_owned()]);
        assert!(options.contains("<option value=\"\" selected>Agent default</option>"));
        assert!(!options.contains("value=\"reported\" selected"));
    }

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
    fn accept_header_selects_html_by_media_range() {
        let accept = |values: &[&str]| {
            let mut headers = HeaderMap::new();
            for value in values {
                headers.append(header::ACCEPT, HeaderValue::from_str(value).unwrap());
            }
            accepts_html(&headers)
        };
        // No header, generic clients and JSON requests keep the JSON summary.
        assert!(!accept(&[]));
        assert!(!accept(&["*/*"]));
        assert!(!accept(&["application/json"]));
        // Browsers and explicit HTML requests get the page.
        assert!(accept(&["text/html"]));
        assert!(accept(&["text/html,application/xhtml+xml,*/*;q=0.8"]));
        assert!(accept(&["text/*"]));
        assert!(accept(&["text/html;charset=utf-8"]));
        assert!(accept(&["text/html;q=0.5"]));
        assert!(accept(&["text/html;q=0.000, text/html;q=0.1"]));
        assert!(accept(&["application/json", "text/html"]));
        // Media types compare exactly, ignoring ASCII case.
        assert!(accept(&["TEXT/HTML"]));
        assert!(!accept(&["text/html-not-really"]));
        assert!(!accept(&["text/htmlx, text/plain"]));
        // A zero quality rejects HTML even when JSON is also acceptable.
        assert!(!accept(&["application/json, text/html;q=0"]));
        assert!(!accept(&["text/html;q=0.000"]));
        assert!(!accept(&["text/html;Q=0"]));
        // The most specific range wins over a wildcard.
        assert!(!accept(&["text/*, text/html;q=0"]));
        assert!(!accept(&["text/*;q=0, text/html;q=0"]));
        assert!(accept(&["text/*;q=0, text/html"]));
        // Malformed qualities are ignored rather than treated as acceptance.
        assert!(!accept(&["text/html;q=2"]));
        assert!(!accept(&["text/html;q=abc"]));
        assert!(!accept(&["text/html;q=0.0001"]));
        assert!(!accept(&["text/html;q=-1"]));
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

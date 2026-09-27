//! End-to-end webhook flow tests.
//!
//! These exercise the whole path: signed HTTP webhook → Forgejo adapter →
//! mention extraction → policy → dispatcher → agent.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use forge_bot::agent::AgentRegistry;
use forge_bot::config::{AgentConfig, Config};
use forge_bot::forge::hmac_sha256_hex;
use forge_bot::forge_api::NoopForgeApi;
use forge_bot::session::{Dispatcher, SessionStore};

const SECRET: &str = "hush";

const PAYLOAD: &str = r#"{
    "action": "created",
    "issue": {
        "number": 1,
        "title": "initial plan",
        "html_url": "http://forge.local:3000/shylock/forge-bot/issues/1"
    },
    "comment": {
        "id": 77,
        "body": "@agent:custom please do the thing",
        "html_url": "http://forge.local:3000/shylock/forge-bot/issues/1#issuecomment-77",
        "user": {"login": "shylock"}
    },
    "repository": {"full_name": "shylock/forge-bot"},
    "sender": {"login": "shylock"}
}"#;

struct Harness {
    app: axum::Router,
    sessions: Arc<SessionStore>,
}

#[allow(clippy::field_reassign_with_default)]
fn harness(dir: &std::path::Path) -> Harness {
    harness_with(dir, |_| {})
}

#[allow(clippy::field_reassign_with_default)]
fn harness_with(dir: &std::path::Path, configure: impl FnOnce(&mut Config)) -> Harness {
    let mut config = Config::default();
    config.bind = "127.0.0.1:0".into();
    config.policy.allow_all = true;
    config.workspace.enabled = false;
    config.reply.ack = false;
    config.reply.result = false;
    config.session.dir = dir.to_path_buf();
    config.session.workers = 1;
    config.agent_sequence = vec!["custom".into()];
    config.forges.forgejo = Some(forge_bot::config::ForgejoConfig {
        base_url: "http://forge.local:3000".into(),
        webhook_secret: Some(SECRET.into()),
        token: None,
        bot_username: Some("shylock-bot".into()),
    });
    // `cat` echoes the prompt, standing in for a real CLI agent.
    config.agents.overrides.insert(
        "custom".into(),
        AgentConfig {
            command: Some("cat".into()),
            ..Default::default()
        },
    );
    configure(&mut config);

    let config = Arc::new(config);
    let adapters = forge_bot::build_adapters(&config);
    let policy = forge_bot::build_policy(&config);
    let sessions = Arc::new(SessionStore::open(dir).unwrap());
    let agents = Arc::new(AgentRegistry::from_config(&config));
    let dispatcher = Dispatcher::new(
        config.clone(),
        agents.clone(),
        sessions.clone(),
        Arc::new(NoopForgeApi),
        policy,
    )
    .unwrap();

    let state = forge_bot::webhook::AppState::new(config.clone(), adapters, agents, dispatcher);

    Harness {
        app: forge_bot::webhook::router(state),
        sessions,
    }
}

fn signed_request(event: &str, payload: &str) -> Request<Body> {
    let signature = hmac_sha256_hex(SECRET.as_bytes(), payload.as_bytes());
    Request::builder()
        .method("POST")
        .uri("/webhooks/forgejo")
        .header("x-forgejo-event", event)
        .header("x-forgejo-signature", signature)
        .header("content-type", "application/json")
        .body(Body::from(payload.to_owned()))
        .unwrap()
}

#[tokio::test]
async fn accepts_signed_mention_and_runs_agent() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(dir.path());

    let response = harness
        .app
        .clone()
        .oneshot(signed_request("issue_comment", PAYLOAD))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);

    // The job runs asynchronously; wait for it to drain.
    for _ in 0..200 {
        if harness.sessions.pending_jobs().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let key = "forgejo:shylock/forge-bot:issue:1";
    let session = harness.sessions.get(key).expect("session should exist");
    assert_eq!(session.runs.len(), 1);
    assert_eq!(session.runs[0].success, Some(true));
    assert!(
        session.runs[0]
            .summary
            .as_deref()
            .unwrap()
            .contains("please do the thing")
    );
}

#[tokio::test]
async fn unqualified_mention_uses_first_agent_in_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(dir.path());
    let payload = PAYLOAD.replace("@agent:custom", "@agent");

    let response = harness
        .app
        .clone()
        .oneshot(signed_request("issue_comment", &payload))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);

    for _ in 0..200 {
        if harness.sessions.pending_jobs().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let session = harness
        .sessions
        .get("forgejo:shylock/forge-bot:issue:1")
        .expect("session should exist");
    assert_eq!(session.runs.len(), 1);
    assert_eq!(session.runs[0].agent, "custom");
    assert_eq!(session.runs[0].success, Some(true));
}

const REVIEW_PAYLOAD: &str = r#"{
    "action": "reviewed",
    "number": 16,
    "pull_request": {
        "number": 16,
        "title": "feat: something",
        "body": "This closes #5.",
        "html_url": "http://forge.local:3000/shylock/forge-bot/pulls/16"
    },
    "review": {"type": "pull_request_review_comment", "content": "@agent:custom review this please"},
    "repository": {"full_name": "shylock/forge-bot"},
    "sender": {"login": "shylock"}
}"#;

#[tokio::test]
async fn handles_pull_request_review_events() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(dir.path());

    let response = harness
        .app
        .clone()
        .oneshot(signed_request("pull_request_comment", REVIEW_PAYLOAD))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);

    for _ in 0..200 {
        if harness.sessions.pending_jobs().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let session = harness
        .sessions
        .get("forgejo:shylock/forge-bot:pr:16")
        .expect("review session should exist");
    assert_eq!(session.runs.len(), 1);
    assert_eq!(session.runs[0].success, Some(true));
    assert!(
        session.runs[0]
            .summary
            .as_deref()
            .unwrap()
            .contains("review this please")
    );
}

#[tokio::test]
async fn rejects_bad_signature() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(dir.path());

    let request = Request::builder()
        .method("POST")
        .uri("/webhooks/forgejo")
        .header("x-forgejo-event", "issue_comment")
        .header("x-forgejo-signature", "deadbeef")
        .body(Body::from(PAYLOAD.to_owned()))
        .unwrap();

    let response = harness.app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn ignores_comment_without_mention() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(dir.path());

    let payload = PAYLOAD.replace("@agent:custom please do the thing", "just a normal comment");
    let response = harness
        .app
        .clone()
        .oneshot(signed_request("issue_comment", &payload))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    // Nothing should have been queued.
    assert!(harness.sessions.pending_jobs().unwrap().is_empty());
}

#[tokio::test]
async fn unknown_forge_returns_404() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(dir.path());

    let request = Request::builder()
        .method("POST")
        .uri("/webhooks/bitbucket")
        .body(Body::from("{}"))
        .unwrap();
    let response = harness.app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn root_lists_forges_and_healthz_is_ok() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(dir.path());

    let root = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(root.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(root.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(text.contains("forge-bot"));
    assert!(text.contains("forgejo"));
    assert!(text.contains("custom"));

    let health = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);
}

#[tokio::test]
async fn duplicate_deliveries_are_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(dir.path());

    for _ in 0..2 {
        let response = harness
            .app
            .clone()
            .oneshot(signed_request("issue_comment", PAYLOAD))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
    }

    for _ in 0..200 {
        if harness.sessions.pending_jobs().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let session = harness
        .sessions
        .get("forgejo:shylock/forge-bot:issue:1")
        .expect("session should exist");
    assert_eq!(session.runs.len(), 1, "the duplicate must be dropped");
}

#[tokio::test]
async fn malformed_payload_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(dir.path());

    let response = harness
        .app
        .clone()
        .oneshot(signed_request("issue_comment", "not json"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn unknown_agent_is_not_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(dir.path());
    let payload = PAYLOAD.replace("@agent:custom", "@agent:missing");

    let response = harness
        .app
        .clone()
        .oneshot(signed_request("issue_comment", &payload))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert!(harness.sessions.pending_jobs().unwrap().is_empty());
}

#[tokio::test]
async fn unauthorized_trigger_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness_with(dir.path(), |config| {
        config.policy.allow_all = false;
        config.policy.allowed_users = vec!["someone-else".into()];
    });

    let response = harness
        .app
        .clone()
        .oneshot(signed_request("issue_comment", PAYLOAD))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert!(harness.sessions.pending_jobs().unwrap().is_empty());
}

#[tokio::test]
async fn comment_from_the_bot_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(dir.path());
    let payload = PAYLOAD.replace(
        "\"user\": {\"login\": \"shylock\"}",
        "\"user\": {\"login\": \"shylock-bot\"}",
    );

    let response = harness
        .app
        .clone()
        .oneshot(signed_request("issue_comment", &payload))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert!(harness.sessions.pending_jobs().unwrap().is_empty());
}

async fn body_text(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn status_page_renders_when_idle() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(dir.path());

    let response = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_text(response).await;
    assert!(html.contains("forge-bot — thread status"), "{html}");
    assert!(html.contains("No threads yet."), "{html}");
}

#[tokio::test]
async fn status_json_lists_a_finished_thread() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(dir.path());

    let response = harness
        .app
        .clone()
        .oneshot(signed_request("issue_comment", PAYLOAD))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);

    for _ in 0..200 {
        if harness.sessions.pending_jobs().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let response = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/status.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let text = body_text(response).await;
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    let threads = value["threads"].as_array().expect("threads array");
    assert_eq!(threads.len(), 1, "{text}");
    assert_eq!(threads[0]["state"], "idle");
    assert_eq!(threads[0]["repository"], "shylock/forge-bot");
    assert_eq!(threads[0]["thread_type"], "issue");
    assert_eq!(threads[0]["number"], 1);
    assert!(threads[0].get("model").is_some(), "{text}");
}

#[tokio::test]
async fn status_json_reports_the_agent_model() {
    let dir = tempfile::tempdir().unwrap();
    // `true` ignores the configured `--model` flag, so the run still succeeds.
    let harness = harness_with(dir.path(), |config| {
        let agent = config.agents.overrides.get_mut("custom").unwrap();
        agent.command = Some("true".into());
        agent.args = Some(vec!["--model".into(), "deepseek-flash".into()]);
    });

    let response = harness
        .app
        .clone()
        .oneshot(signed_request("issue_comment", PAYLOAD))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);

    for _ in 0..200 {
        if harness.sessions.pending_jobs().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let response = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/status.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let text = body_text(response).await;
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["threads"][0]["model"], "deepseek-flash", "{text}");

    let response = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let html = body_text(response).await;
    assert!(html.contains("<th>Model</th>"), "{html}");
    assert!(html.contains("deepseek-flash"), "{html}");
}

#[tokio::test]
async fn status_search_finds_a_thread_by_comment_url() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(dir.path());

    let response = harness
        .app
        .clone()
        .oneshot(signed_request("issue_comment", PAYLOAD))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    for _ in 0..200 {
        if harness.sessions.pending_jobs().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // `#` is percent-encoded so it reaches the server as part of `q`.
    let found = "https://forgejo.shylockhg.me/shylock/forge-bot/issues/1%23issuecomment-77";
    let response = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/status?q={found}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_text(response).await;
    assert!(html.contains("1 of 1 thread(s) match"), "{html}");
    assert!(html.contains("shylock/forge-bot"), "{html}");

    let response = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/status.json?q={found}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value: serde_json::Value = serde_json::from_str(&body_text(response).await).unwrap();
    assert_eq!(value["threads"].as_array().unwrap().len(), 1);

    let missing = "https://forgejo.shylockhg.me/shylock/forge-bot/issues/999";
    let response = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/status?q={missing}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let html = body_text(response).await;
    assert!(html.contains("No thread matches"), "{html}");
}

#[tokio::test]
async fn status_routes_do_not_change_state() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(dir.path());

    let response = harness
        .app
        .clone()
        .oneshot(signed_request("issue_comment", PAYLOAD))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    for _ in 0..200 {
        if harness.sessions.pending_jobs().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let before = state_fingerprint(dir.path());
    assert!(
        !before.is_empty(),
        "expected persisted state to fingerprint"
    );

    // Plain and filtered reads of both the HTML and JSON views.
    for uri in [
        "/status",
        "/status.json",
        "/status?q=https%3A%2F%2Fforge.local%3A3000%2Fshylock%2Fforge-bot%2Fissues%2F1%23issuecomment-77",
        "/status.json?q=https%3A%2F%2Fforge.local%3A3000%2Fshylock%2Fforge-bot%2Fissues%2F1%23issuecomment-77",
    ] {
        let response = harness
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
        let _ = body_text(response).await;
    }

    // Reading the page must not have created, rewritten or removed anything.
    assert_eq!(state_fingerprint(dir.path()), before);
}

/// Sorted `relative path -> contents` for every file under `dir`, so a test can
/// assert that reading the status page left persisted state byte-for-byte
/// unchanged.
fn state_fingerprint(dir: &std::path::Path) -> std::collections::BTreeMap<String, String> {
    let mut files = std::collections::BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(path) = stack.pop() {
        for entry in std::fs::read_dir(&path).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let relative = path
                    .strip_prefix(dir)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                files.insert(relative, std::fs::read_to_string(&path).unwrap());
            }
        }
    }
    files
}

#[tokio::test]
async fn status_routes_report_storage_errors() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(dir.path());
    // Removing the job queue makes `pending_jobs()` fail, which both routes
    // surface as a 500 rather than panicking.
    std::fs::remove_dir(dir.path().join("jobs")).unwrap();

    for (uri, marker) in [
        ("/status", "failed to read thread status"),
        ("/status.json", "\"error\""),
    ] {
        let response = harness
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let text = body_text(response).await;
        assert!(text.contains(marker), "{uri}: {text}");
    }
}

//! End-to-end webhook flow tests.
//!
//! These exercise the whole path: signed HTTP webhook → Forgejo adapter →
//! mention extraction → policy → dispatcher → agent.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::{
    Json, Router,
    routing::{get, post},
};
use serde_json::{Value, json};
use tower::ServiceExt;

use forge_bot::agent::AgentRegistry;
use forge_bot::config::{AgentConfig, Config, UserConfig, UserRole};
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
        "body": "@shylock-bot --agent=custom please do the thing",
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
    // The executor resolves explicit host_users at startup; give every test a
    // passwd fixture with the accounts the explicit-user test configures.
    let passwd = dir.join("passwd");
    std::fs::write(
        &passwd,
        format!(
            "agent:x:1000:1000::{}:/bin/bash\nreviewer:x:1001:1001::{}:/bin/bash\n",
            dir.join("home/agent").display(),
            dir.join("home/reviewer").display()
        ),
    )
    .unwrap();
    config.executor.passwd_file = passwd;
    config.executor.set_direct_for_tests();
    config.forges.forgejo = Some(forge_bot::config::ForgejoConfig {
        base_url: "http://forge.local:3000".into(),
        webhook_secret: Some(SECRET.into()),
        token: None,
        bot_username: Some("shylock-bot".into()),
        ..Default::default()
    });
    // Every run belongs to a configured user.
    config.users.insert(
        "default".into(),
        UserConfig {
            role: UserRole::Default,
            host_user: "agent".into(),
            agent: None,
            agent_model: None,
            token: None,
        },
    );
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

fn pr_payload(mergeable: bool) -> Value {
    json!({
        "number": 7, "state": "open", "title": "Fix it", "body": "",
        "mergeable": mergeable,
        "head": {"sha": "head123"},
        "base": {"sha": "base123", "ref": "main"}
    })
}

async fn mock_pr_api(mergeable: bool) -> (String, tokio::task::JoinHandle<()>) {
    mock_pr_api_sequence(vec![mergeable]).await
}

/// Serve the PR detail endpoint with a sequence of `mergeable` values (the
/// last one repeats) so tests can model Forgejo's asynchronous conflict check:
/// the first response can still report `false` while the real value settles.
async fn mock_pr_api_sequence(mergeables: Vec<bool>) -> (String, tokio::task::JoinHandle<()>) {
    assert!(!mergeables.is_empty(), "need at least one mergeable value");
    let list = pr_payload(mergeables[0]);
    let detail_mergeables = mergeables;
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let detail_calls = calls.clone();
    let app = Router::new()
        .route(
            "/api/v1/repos/shylock/forge-bot/pulls/7",
            get(move || {
                let mergeables = detail_mergeables.clone();
                let calls = detail_calls.clone();
                async move {
                    let index = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let mergeable = mergeables[index.min(mergeables.len() - 1)];
                    Json(pr_payload(mergeable))
                }
            }),
        )
        .route(
            "/api/v1/repos/shylock/forge-bot/pulls",
            get(move || async move { Json(vec![list]) }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, task)
}

/// Like [`mock_pr_api_sequence`], but also answers Forgejo's branch-update
/// endpoint with `update_status` so tests can model a clean or conflicting
/// branch independently of the asynchronous `mergeable` flag.
async fn mock_pr_api_with_update(
    mergeable: bool,
    update_status: StatusCode,
) -> (String, tokio::task::JoinHandle<()>) {
    let list = pr_payload(mergeable);
    let app = Router::new()
        .route(
            "/api/v1/repos/shylock/forge-bot/pulls/7",
            get(move || async move { Json(pr_payload(mergeable)) }),
        )
        .route(
            "/api/v1/repos/shylock/forge-bot/pulls",
            get(move || async move { Json(vec![list]) }),
        )
        .route(
            "/api/v1/repos/shylock/forge-bot/pulls/7/update",
            post(move || async move { update_status }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, task)
}

async fn accepted(app: &Router, event: &str, payload: &str) -> usize {
    let response = app
        .clone()
        .oneshot(signed_request(event, payload))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice::<Value>(&bytes).unwrap()["accepted"]
        .as_u64()
        .unwrap() as usize
}

#[tokio::test]
async fn auto_ci_failure_requires_secret_and_current_pr_and_dedupes() {
    let dir = tempfile::tempdir().unwrap();
    let (url, server) = mock_pr_api(true).await;
    let payload = json!({
        "action": "failure", "run": {
            "id": 88, "commit_sha": "merge123", "html_url": "https://forge.test/actions/runs/88",
            "repository": {"full_name": "shylock/forge-bot"},
            "event_payload": "{\"pull_request\":{\"number\":7,\"head\":{\"sha\":\"head123\"}}}"
        }
    })
    .to_string();
    let disabled = harness_with(dir.path(), |config| {
        config.forges.forgejo.as_mut().unwrap().base_url = url.clone();
        config.forges.forgejo.as_mut().unwrap().token = Some("test".into());
        config.forges.forgejo.as_mut().unwrap().webhook_secret = None;
    });
    assert_eq!(
        accepted(&disabled.app, "action_run_failure", &payload).await,
        0
    );

    let dir2 = tempfile::tempdir().unwrap();
    let enabled = harness_with(dir2.path(), |config| {
        config.forges.forgejo.as_mut().unwrap().base_url = url.clone();
        config.forges.forgejo.as_mut().unwrap().token = Some("test".into());
    });
    assert_eq!(
        accepted(&enabled.app, "action_run_failure", &payload).await,
        1
    );
    assert_eq!(
        accepted(&enabled.app, "action_run_failure", &payload).await,
        0
    );
    let restarted = harness_with(dir2.path(), |config| {
        config.forges.forgejo.as_mut().unwrap().base_url = url;
        config.forges.forgejo.as_mut().unwrap().token = Some("test".into());
    });
    assert_eq!(
        accepted(&restarted.app, "action_run_failure", &payload).await,
        0
    );
    let stale = payload.replace("head123", "oldhead");
    assert_eq!(
        accepted(&enabled.app, "action_run_failure", &stale).await,
        0
    );
    server.abort();
}

#[tokio::test]
async fn auto_conflict_handles_pr_and_base_push_once() {
    let dir = tempfile::tempdir().unwrap();
    let (url, server) = mock_pr_api(false).await;
    let harness = harness_with(dir.path(), |config| {
        config.forges.forgejo.as_mut().unwrap().base_url = url;
        config.forges.forgejo.as_mut().unwrap().token = Some("test".into());
    });
    let pr = json!({"action":"opened", "repository":{"full_name":"shylock/forge-bot"},
        "pull_request":{"number":7,"body":""}})
    .to_string();
    let push = json!({"ref":"refs/heads/main", "repository":{"full_name":"shylock/forge-bot"}})
        .to_string();
    assert_eq!(accepted(&harness.app, "pull_request", &pr).await, 1);
    assert_eq!(accepted(&harness.app, "push", &push).await, 0);
    assert_eq!(accepted(&harness.app, "pull_request", &pr).await, 0);
    server.abort();
}

#[tokio::test]
async fn auto_conflict_base_push_detects_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let (url, server) = mock_pr_api(false).await;
    let harness = harness_with(dir.path(), |config| {
        config.forges.forgejo.as_mut().unwrap().base_url = url;
        config.forges.forgejo.as_mut().unwrap().token = Some("test".into());
    });
    let other = json!({"ref":"refs/heads/other", "repository":{"full_name":"shylock/forge-bot"}})
        .to_string();
    let push = json!({"ref":"refs/heads/main", "repository":{"full_name":"shylock/forge-bot"}})
        .to_string();
    assert_eq!(accepted(&harness.app, "push", &other).await, 0);
    assert_eq!(accepted(&harness.app, "push", &push).await, 1);
    server.abort();
}

#[tokio::test]
async fn auto_conflict_ignores_mergeable_pr() {
    let dir = tempfile::tempdir().unwrap();
    let (url, server) = mock_pr_api(true).await;
    let harness = harness_with(dir.path(), |config| {
        config.forges.forgejo.as_mut().unwrap().base_url = url;
        config.forges.forgejo.as_mut().unwrap().token = Some("test".into());
    });
    let pr = json!({"action":"opened", "repository":{"full_name":"shylock/forge-bot"},
        "pull_request":{"number":7,"body":""}})
    .to_string();
    assert_eq!(accepted(&harness.app, "pull_request", &pr).await, 0);
    server.abort();
}

#[tokio::test]
async fn auto_conflict_ignores_pr_while_forgejo_is_still_checking() {
    let dir = tempfile::tempdir().unwrap();
    // Forgejo first reports `mergeable: false` while its conflict check is
    // queued, then settles to mergeable. The bot must not start an agent just
    // because the PR is behind the base branch.
    let (url, server) = mock_pr_api_sequence(vec![false, true]).await;
    let harness = harness_with(dir.path(), |config| {
        config.forges.forgejo.as_mut().unwrap().base_url = url;
        config.forges.forgejo.as_mut().unwrap().token = Some("test".into());
    });
    let pr = json!({"action":"synchronized", "repository":{"full_name":"shylock/forge-bot"},
        "pull_request":{"number":7,"body":""}})
    .to_string();
    assert_eq!(accepted(&harness.app, "pull_request", &pr).await, 0);
    server.abort();
}

#[tokio::test]
async fn auto_conflict_update_of_a_clean_branch_skips_the_agent() {
    let dir = tempfile::tempdir().unwrap();
    // Forgejo keeps reporting `mergeable: false` (e.g. its check is slow) but
    // the branch actually merges cleanly, so updating it succeeds. The bot
    // must not start an agent for a branch that is not conflicting.
    let (url, server) = mock_pr_api_with_update(false, StatusCode::OK).await;
    let harness = harness_with(dir.path(), |config| {
        config.forges.forgejo.as_mut().unwrap().base_url = url;
        config.forges.forgejo.as_mut().unwrap().token = Some("test".into());
    });
    let pr = json!({"action":"synchronized", "repository":{"full_name":"shylock/forge-bot"},
        "pull_request":{"number":7,"body":""}})
    .to_string();
    assert_eq!(accepted(&harness.app, "pull_request", &pr).await, 0);
    // The pair is remembered so a re-delivery does not re-check it.
    assert_eq!(accepted(&harness.app, "pull_request", &pr).await, 0);
    server.abort();
}

#[tokio::test]
async fn auto_conflict_update_of_a_conflicting_branch_starts_the_agent() {
    let dir = tempfile::tempdir().unwrap();
    let (url, server) = mock_pr_api_with_update(false, StatusCode::CONFLICT).await;
    let harness = harness_with(dir.path(), |config| {
        config.forges.forgejo.as_mut().unwrap().base_url = url;
        config.forges.forgejo.as_mut().unwrap().token = Some("test".into());
    });
    let pr = json!({"action":"synchronized", "repository":{"full_name":"shylock/forge-bot"},
        "pull_request":{"number":7,"body":""}})
    .to_string();
    assert_eq!(accepted(&harness.app, "pull_request", &pr).await, 1);
    server.abort();
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

    let key = "user:default:forgejo:shylock/forge-bot:issue:1";
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
async fn explicit_users_route_by_login() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness_with(dir.path(), |config| {
        config.users.clear();
        config.policy.allow_all = false;
        config.policy.allowed_users = vec!["shylock".into()];
        config.policy.allowed_repos = vec!["shylock/forge-bot".into()];
        let user = |role, host: &str, agent: Option<&str>| UserConfig {
            role,
            host_user: host.into(),
            agent: agent.map(str::to_owned),
            agent_model: None,
            token: None,
        };
        config.users.insert(
            "shylock-bot".into(),
            user(UserRole::Default, "agent", Some("custom")),
        );
        config.users.insert(
            "shylock-reviewer".into(),
            user(UserRole::Reviewer, "reviewer", None),
        );
    });

    // Each delivery needs its own comment id or dedupe drops it.
    let payload = |id: i64, body: &str| {
        PAYLOAD
            .replace("\"id\": 77", &format!("\"id\": {id}"))
            .replace("@shylock-bot --agent=custom please do the thing", body)
    };

    // A reviewer mention is accepted and runs the registry default adapter.
    assert_eq!(
        accepted(
            &harness.app,
            "issue_comment",
            &payload(101, "@shylock-reviewer please review")
        )
        .await,
        1
    );

    // The default user is addressed by the configured bot username.
    assert_eq!(
        accepted(
            &harness.app,
            "issue_comment",
            &payload(102, "@shylock-bot --agent=custom do it")
        )
        .await,
        1
    );

    // A comment addressing two users is rejected before acknowledgement.
    assert_eq!(
        accepted(
            &harness.app,
            "issue_comment",
            &payload(103, "@shylock-bot and @shylock-reviewer")
        )
        .await,
        0
    );

    // Configured peers can invoke each other even with a human-only allow-list.
    let peer = payload(105, "@shylock-reviewer review the updated PR")
        .replace("\"login\": \"shylock\"", "\"login\": \"shylock-bot\"");
    assert_eq!(accepted(&harness.app, "issue_comment", &peer).await, 1);
    let reply = payload(106, "@shylock-bot fix these findings")
        .replace("\"login\": \"shylock\"", "\"login\": \"shylock-reviewer\"");
    assert_eq!(accepted(&harness.app, "issue_comment", &reply).await, 1);
    let self_mention = payload(107, "@shylock-reviewer review again")
        .replace("\"login\": \"shylock\"", "\"login\": \"shylock-reviewer\"");
    assert_eq!(
        accepted(&harness.app, "issue_comment", &self_mention).await,
        0
    );

    // A mention of an unconfigured login does nothing.
    assert_eq!(
        accepted(
            &harness.app,
            "issue_comment",
            &payload(104, "@someone-else hi")
        )
        .await,
        0
    );
    for _ in 0..200 {
        if harness.sessions.pending_jobs().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let submitter = harness
        .sessions
        .get("user:shylock-bot:forgejo:shylock/forge-bot:issue:1")
        .unwrap();
    assert!(
        submitter.runs[0]
            .summary
            .as_ref()
            .unwrap()
            .contains("mentioning only @shylock-reviewer")
    );
    let reviewer = harness
        .sessions
        .get("user:shylock-reviewer:forgejo:shylock/forge-bot:issue:1")
        .unwrap();
    assert!(
        reviewer.runs[0]
            .summary
            .as_ref()
            .unwrap()
            .contains("approve the final head")
    );
}

#[tokio::test]
async fn unqualified_mention_uses_first_agent_in_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(dir.path());
    let payload = PAYLOAD.replace("@shylock-bot --agent=custom", "@shylock-bot");

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
        .get("user:default:forgejo:shylock/forge-bot:issue:1")
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
    "review": {"type": "pull_request_review_comment", "content": "@shylock-bot --agent=custom review this please"},
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
        .get("user:default:forgejo:shylock/forge-bot:pr:16")
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

    let payload = PAYLOAD.replace(
        "@shylock-bot --agent=custom please do the thing",
        "just a normal comment",
    );
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
        .get("user:default:forgejo:shylock/forge-bot:issue:1")
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
    let payload = PAYLOAD.replace(
        "@shylock-bot --agent=custom",
        "@shylock-bot --agent=missing",
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
async fn details_show_output_while_the_agent_is_running() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness_with(dir.path(), |config| {
        let agent = config.agents.overrides.get_mut("custom").unwrap();
        agent.command = Some("sh".into());
        agent.args = Some(vec![
            "-c".into(),
            "printf 'live tick\\n'; sleep 1; printf 'finished\\n'".into(),
        ]);
    });
    let response = harness
        .app
        .clone()
        .oneshot(signed_request("issue_comment", PAYLOAD))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);

    let key = "user:default:forgejo:shylock/forge-bot:issue:1";
    let mut live = None;
    for _ in 0..100 {
        if let Some(session) = harness.sessions.get(key)
            && let Some(run) = session.runs.last()
            && let Some(output) = harness.sessions.live_output(run.job_id)
            && output.text().contains("live tick")
        {
            live = Some(run.job_id);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let job_id = live.expect("output should be available before the run finishes");
    assert!(
        harness.sessions.get(key).unwrap().runs[0]
            .finished_at
            .is_none()
    );
    let response = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/status/details?key=user%3Adefault%3Aforgejo%3Ashylock%2Fforge-bot%3Aissue%3A1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_text(response).await;
    assert!(html.contains("<h3>Live output</h3>"), "{html}");
    assert!(html.contains("live tick"), "{html}");

    for _ in 0..200 {
        if harness.sessions.get(key).unwrap().runs[0]
            .finished_at
            .is_some()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(harness.sessions.live_output(job_id).is_none());
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

    let key = threads[0]["key"].as_str().unwrap();
    let uri = format!(
        "/status/details?{}",
        url::form_urlencoded::Serializer::new(String::new())
            .append_pair("key", key)
            .finish()
    );
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
    assert_eq!(response.status(), StatusCode::OK);
    let details = body_text(response).await;
    assert!(details.contains("<h3>Request</h3>"), "{details}");
    assert!(details.contains("<h3>Result</h3>"), "{details}");
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
        "/status/details?key=user%3Adefault%3Aforgejo%3Ashylock%2Fforge-bot%3Aissue%3A1",
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

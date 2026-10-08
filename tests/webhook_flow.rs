//! End-to-end webhook flow tests.
//!
//! These exercise the whole path: signed HTTP webhook → Forgejo adapter →
//! mention extraction → policy → dispatcher → agent.

use std::sync::Arc;
use std::time::Duration;

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
    agents: Arc<AgentRegistry>,
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
    config.policy.auto_allowed_repos = vec!["shylock/forge-bot".into()];
    config.policy.auto_allowed_pr_authors = vec!["shylock".into()];
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

    let state =
        forge_bot::webhook::AppState::new(config.clone(), adapters, agents.clone(), dispatcher);

    Harness {
        app: forge_bot::webhook::router(state),
        sessions,
        agents,
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
        "user": {"login": "shylock"},
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
        // A conversation comment on a PR is enriched by looking for a review
        // thread; an empty page tells the adapter there is none.
        .route(
            "/api/v1/repos/shylock/forge-bot/pulls/7/reviews",
            get(|| async { Json(Vec::<Value>::new()) }),
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

/// Wait for every queued job to finish so a test can inspect the session
/// records the runs produced.
async fn wait_for_jobs(sessions: &SessionStore) {
    for _ in 0..400 {
        if sessions.pending_jobs().unwrap().is_empty() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("jobs did not drain");
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
    let response = disabled
        .app
        .clone()
        .oneshot(signed_request("action_run_failure", &payload))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

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

/// Issue #137: a signed forge event has no mention to choose an adapter, so it
/// must continue the conversation with the agent the thread already used
/// instead of snapping back to the registry default.
#[tokio::test]
async fn auto_trigger_reuses_the_threads_previous_agent() {
    let dir = tempfile::tempdir().unwrap();
    let (url, server) = mock_pr_api_with_update(false, StatusCode::CONFLICT).await;
    let harness = harness_with(dir.path(), |config| {
        config.agent_sequence = vec!["custom".into(), "other".into()];
        config.agents.overrides.insert(
            "other".into(),
            AgentConfig {
                command: Some("cat".into()),
                ..Default::default()
            },
        );
        config.forges.forgejo.as_mut().unwrap().base_url = url;
        config.forges.forgejo.as_mut().unwrap().token = Some("test".into());
    });
    let key = "user:default:forgejo:shylock/forge-bot:pr:7";

    // A human mention explicitly selects the non-default `other` adapter.
    let mention = json!({
        "action": "created",
        "issue": {"number": 7, "title": "Fix it", "pull_request": {"url": "x"},
                  "html_url": "http://forge.local:3000/shylock/forge-bot/pulls/7"},
        "comment": {"id": 77, "body": "@shylock-bot --agent=other please do the thing",
                    "user": {"login": "shylock"}},
        "repository": {"full_name": "shylock/forge-bot"},
        "sender": {"login": "shylock"}
    })
    .to_string();
    assert_eq!(accepted(&harness.app, "issue_comment", &mention).await, 1);
    wait_for_jobs(&harness.sessions).await;
    assert_eq!(
        harness
            .sessions
            .get(key)
            .unwrap()
            .runs
            .last()
            .unwrap()
            .agent,
        "other"
    );

    // The automatic conflict trigger has no mention but must keep using
    // `other` for the same thread rather than falling back to `custom`.
    let pr = json!({"action":"synchronized", "repository":{"full_name":"shylock/forge-bot"},
        "pull_request":{"number":7,"body":""}})
    .to_string();
    assert_eq!(accepted(&harness.app, "pull_request", &pr).await, 1);
    wait_for_jobs(&harness.sessions).await;
    let session = harness.sessions.get(key).unwrap();
    assert_eq!(session.agent, "other");
    assert_eq!(session.runs.last().unwrap().agent, "other");
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

/// A harness with two configured agent users so mention routing can be
/// exercised against ambiguous and quoted comments.
fn two_user_routing_harness(dir: &std::path::Path) -> Harness {
    harness_with(dir, |config| {
        config.users.clear();
        config.policy.allow_all = false;
        config.policy.allowed_users = vec!["shylock".into()];
        config.policy.allowed_repos = vec!["shylock/forge-bot".into()];
        let user = |role, host: &str| UserConfig {
            role,
            host_user: host.into(),
            agent: None,
            agent_model: None,
            token: None,
        };
        config
            .users
            .insert("shylock-bot".into(), user(UserRole::Default, "agent"));
        config.users.insert(
            "shylock-reviewer".into(),
            user(UserRole::Reviewer, "reviewer"),
        );
    })
}

#[tokio::test]
async fn quoted_agent_mention_does_not_block_routing() {
    let dir = tempfile::tempdir().unwrap();
    let harness = two_user_routing_harness(dir.path());

    // Regression for issue #196: the quoted `@shylock-bot` in the report must
    // not make the comment look like it addresses two configured users.
    let payload = PAYLOAD.replace("\"id\": 77", "\"id\": 208").replace(
        "@shylock-bot --agent=custom please do the thing",
        "@shylock-reviewer please review `b8affa0`; the dry run renders `@shylock-bot` too",
    );
    assert_eq!(accepted(&harness.app, "issue_comment", &payload).await, 1);

    wait_for_jobs(&harness.sessions).await;
    let reviewer = harness
        .sessions
        .get("user:shylock-reviewer:forgejo:shylock/forge-bot:issue:1")
        .expect("reviewer session should exist");
    assert_eq!(reviewer.runs.len(), 1);
}

/// A comment whose inline backtick runs have unequal lengths is not a code
/// span, so both mentions are addressed and the ambiguous comment is rejected.
#[tokio::test]
async fn unequal_inline_backtick_runs_keep_both_mentions() {
    let dir = tempfile::tempdir().unwrap();
    let harness = two_user_routing_harness(dir.path());

    let payload = PAYLOAD.replace("\"id\": 77", "\"id\": 209").replace(
        "@shylock-bot --agent=custom please do the thing",
        "@shylock-reviewer review `literal @shylock-bot please``now",
    );
    assert_eq!(accepted(&harness.app, "issue_comment", &payload).await, 0);
}

/// A code span may cross a line break inside a paragraph, so the quoted
/// `@shylock-bot` must not be routed even though it is on its own line.
#[tokio::test]
async fn inline_code_span_crossing_a_line_hides_the_mention() {
    let dir = tempfile::tempdir().unwrap();
    let harness = two_user_routing_harness(dir.path());

    let payload = PAYLOAD.replace("\"id\": 77", "\"id\": 210").replace(
        "@shylock-bot --agent=custom please do the thing",
        "@shylock-reviewer review `quoted\\n@shylock-bot`",
    );
    assert_eq!(accepted(&harness.app, "issue_comment", &payload).await, 1);

    wait_for_jobs(&harness.sessions).await;
    let reviewer = harness
        .sessions
        .get("user:shylock-reviewer:forgejo:shylock/forge-bot:issue:1")
        .expect("reviewer session should exist");
    assert_eq!(reviewer.runs.len(), 1);
}

/// A fenced block only closes on a run of the same length as its opener, and
/// only when nothing but whitespace follows it.
#[tokio::test]
async fn fence_closer_must_match_the_opening_run() {
    for (id, body) in [
        (
            211,
            "@shylock-reviewer review\\n````\\n```\\n@shylock-bot\\n````",
        ),
        (
            212,
            "@shylock-reviewer review\\n```\\n```example\\n@shylock-bot\\n```",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let harness = two_user_routing_harness(dir.path());
        let payload = PAYLOAD
            .replace("\"id\": 77", &format!("\"id\": {id}"))
            .replace("@shylock-bot --agent=custom please do the thing", body);
        assert_eq!(accepted(&harness.app, "issue_comment", &payload).await, 1);

        wait_for_jobs(&harness.sessions).await;
        let reviewer = harness
            .sessions
            .get("user:shylock-reviewer:forgejo:shylock/forge-bot:issue:1")
            .expect("reviewer session should exist");
        assert_eq!(reviewer.runs.len(), 1);
    }
}

/// A backtick fence's info string cannot contain backticks and top-level
/// fences are indented at most three spaces; otherwise the rest of the
/// comment must still be routed.
#[tokio::test]
async fn invalid_fence_openers_do_not_drop_the_request() {
    for (id, body) in [
        (213, r"```@shylock-bot```\n@shylock-reviewer review"),
        (214, r"    ```\n@shylock-reviewer review"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let harness = two_user_routing_harness(dir.path());
        let payload = PAYLOAD
            .replace("\"id\": 77", &format!("\"id\": {id}"))
            .replace("@shylock-bot --agent=custom please do the thing", body);
        assert_eq!(accepted(&harness.app, "issue_comment", &payload).await, 1);

        wait_for_jobs(&harness.sessions).await;
        let reviewer = harness
            .sessions
            .get("user:shylock-reviewer:forgejo:shylock/forge-bot:issue:1")
            .expect("reviewer session should exist");
        assert_eq!(reviewer.runs.len(), 1);
    }
}

/// Escaped backticks do not delimit code, so both configured users are
/// addressed and the ambiguous comment is rejected.
#[tokio::test]
async fn escaped_backticks_keep_both_mentions_ambiguous() {
    let dir = tempfile::tempdir().unwrap();
    let harness = two_user_routing_harness(dir.path());

    let payload = PAYLOAD.replace("\"id\": 77", "\"id\": 215").replace(
        "@shylock-bot --agent=custom please do the thing",
        r"@shylock-reviewer review \\`literal @shylock-bot please\\`",
    );
    assert_eq!(accepted(&harness.app, "issue_comment", &payload).await, 0);
}

/// A thematic break interrupts the paragraph, so the backticks do not pair and
/// both mentions remain addressed, rejecting the ambiguous comment.
#[tokio::test]
async fn block_interrupt_stops_inline_span_routing() {
    let dir = tempfile::tempdir().unwrap();
    let harness = two_user_routing_harness(dir.path());

    let payload = PAYLOAD.replace("\"id\": 77", "\"id\": 216").replace(
        "@shylock-bot --agent=custom please do the thing",
        r"@shylock-reviewer review `quoted\n***\n@shylock-bot please`",
    );
    assert_eq!(accepted(&harness.app, "issue_comment", &payload).await, 0);
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

// A rejecting review is delivered under `pull_request_rejected` with
// `review.type = pull_request_review_rejected`. It is a distinct event from a
// plain review comment, so it must be handled explicitly.
const REJECTED_REVIEW_PAYLOAD: &str = r#"{
    "action": "reviewed",
    "number": 17,
    "pull_request": {
        "number": 17,
        "title": "feat: something else",
        "body": "This closes #6.",
        "html_url": "http://forge.local:3000/shylock/forge-bot/pulls/17"
    },
    "review": {"type": "pull_request_review_rejected", "content": "@shylock-bot --agent=custom fix the review finding"},
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
async fn handles_rejected_review_bodies() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(dir.path());

    let response = harness
        .app
        .clone()
        .oneshot(signed_request(
            "pull_request_rejected",
            REJECTED_REVIEW_PAYLOAD,
        ))
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
        .get("user:default:forgejo:shylock/forge-bot:pr:17")
        .expect("rejected-review session should exist");
    assert_eq!(session.runs.len(), 1);
    assert_eq!(session.runs[0].success, Some(true));
    assert!(
        session.runs[0]
            .summary
            .as_deref()
            .unwrap()
            .contains("fix the review finding")
    );
}

#[tokio::test]
async fn rejected_review_peer_handoff_works_with_polling_disabled() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness_with(dir.path(), |config| {
        config.poller.enabled = false;
        config.policy.allow_all = false;
        config.policy.allowed_users = vec!["shylock".into()];
        config.policy.allowed_repos = vec!["shylock/forge-bot".into()];
        config.users.insert(
            "shylock-reviewer".into(),
            UserConfig {
                role: UserRole::Reviewer,
                host_user: "reviewer".into(),
                agent: None,
                agent_model: None,
                token: None,
            },
        );
    });
    let payload = REJECTED_REVIEW_PAYLOAD.replace(
        "\"sender\": {\"login\": \"shylock\"}",
        "\"sender\": {\"login\": \"shylock-reviewer\"}",
    );
    assert_eq!(
        accepted(&harness.app, "pull_request_rejected", &payload).await,
        1
    );
    for _ in 0..200 {
        if harness
            .sessions
            .get("user:default:forgejo:shylock/forge-bot:pr:17")
            .is_some_and(|s| s.runs.len() == 1 && s.runs[0].success == Some(true))
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let session = harness
        .sessions
        .get("user:default:forgejo:shylock/forge-bot:pr:17")
        .unwrap();
    assert_eq!(session.runs.len(), 1);
    assert_eq!(session.runs[0].success, Some(true));
    assert!(
        session.runs[0]
            .summary
            .as_deref()
            .unwrap()
            .contains("fix the review finding")
    );
    assert_eq!(
        accepted(&harness.app, "pull_request_rejected", &payload).await,
        0
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
async fn browser_index_links_every_sub_page() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(dir.path());

    let response = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/")
                .header("accept", "text/html,application/xhtml+xml,*/*;q=0.8")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap()
        .to_owned();
    assert!(content_type.starts_with("text/html"), "{content_type}");
    let html = body_text(response).await;
    for path in [
        "/status",
        "/status.json",
        "/admin",
        "/notifications",
        "/notifications/diagnostics",
        "/notifications.json",
        "/healthz",
    ] {
        assert!(
            html.contains(&format!("href=\"{path}\"")),
            "missing {path}: {html}"
        );
    }
    assert!(html.contains("forgejo"), "{html}");
    assert!(html.contains("custom"), "{html}");

    // Clients that do not ask for HTML keep the JSON summary.
    let response = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/")
                .header("accept", "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body_text(response).await).unwrap();
    assert_eq!(json["status"], "/status");
}

#[tokio::test]
async fn root_negotiates_representation_and_varies_on_accept() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(dir.path());

    // (Accept header, expect HTML)
    let cases = [
        (Some("application/json, text/html;q=0"), false),
        (Some("text/html-not-really"), false),
        (Some("TEXT/HTML"), true),
        (Some("text/html;q=0.5, application/json"), true),
        (Some("*/*"), false),
        (None, false),
    ];
    for (accept, expect_html) in cases {
        let mut request = Request::builder().method("GET").uri("/");
        if let Some(accept) = accept {
            request = request.header("accept", accept);
        }
        let response = harness
            .app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{accept:?}");
        assert_eq!(
            response.headers().get("vary").and_then(|v| v.to_str().ok()),
            Some("Accept"),
            "{accept:?}"
        );
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap()
            .to_owned();
        let text = body_text(response).await;
        if expect_html {
            assert!(
                content_type.starts_with("text/html"),
                "{accept:?}: {content_type}"
            );
            assert!(text.contains("href=\"/status\""), "{accept:?}");
        } else {
            assert!(
                content_type.starts_with("application/json"),
                "{accept:?}: {content_type}"
            );
            let json: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(json["status"], "/status", "{accept:?}");
        }
    }
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
async fn statistics_page_totals_tokens_by_model_thread_and_repository() {
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    // Recent timestamps: sessions idle past the retention window are evicted
    // when the dispatcher starts.
    let now = chrono::Utc::now().to_rfc3339();
    let run = |job: &str, model: Option<&str>, cache: Option<(u64, u64)>| {
        json!({
            "job_id": job,
            "agent": "custom",
            "started_at": now,
            "finished_at": now,
            "success": true,
            "summary": "done",
            "model": model,
            "cache": cache.map(|(prompt_tokens, cached_tokens)| {
                json!({"prompt_tokens": prompt_tokens, "cached_tokens": cached_tokens})
            }),
        })
    };
    let session = |key: &str, number: u64, runs: Vec<Value>| {
        json!({
            "key": key,
            "repository": "shylock/forge-bot",
            "location": format!("http://forge.local:3000/shylock/forge-bot/issues/{number}"),
            "agent": "custom",
            "created_at": now,
            "updated_at": now,
            "runs": runs,
        })
    };
    std::fs::write(
        sessions_dir.join("one.json"),
        session(
            "forgejo:shylock/forge-bot:issue:1",
            1,
            vec![
                run(
                    "00000000-0000-4000-8000-000000000001",
                    Some("model-<x>"),
                    Some((1_234_567, 1_000_000)),
                ),
                run("00000000-0000-4000-8000-000000000002", None, None),
            ],
        )
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        sessions_dir.join("two.json"),
        session(
            "forgejo:shylock/forge-bot:pr:2",
            2,
            vec![run(
                "00000000-0000-4000-8000-000000000003",
                Some("model-<x>"),
                Some((100, 0)),
            )],
        )
        .to_string(),
    )
    .unwrap();
    let harness = harness(dir.path());

    let response = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/statistics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("no-store")
    );
    let html = body_text(response).await;
    assert!(html.contains("forge-bot — token statistics"), "{html}");
    // Model names are escaped, and the per-model total adds both threads.
    assert!(html.contains("model-&lt;x&gt;"), "{html}");
    assert!(html.contains("1,234,667"), "{html}");
    assert!(html.contains("1,000,000"), "{html}");
    assert!(html.contains("81.0%"), "{html}");
    assert!(html.contains("unknown"), "{html}");
    assert!(
        html.contains("href=\"http://forge.local:3000/shylock/forge-bot/issues/1\""),
        "{html}"
    );
    assert!(html.contains("forgejo:shylock/forge-bot:pr:2"), "{html}");
}

#[tokio::test]
async fn statistics_page_renders_when_empty() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(dir.path());
    let response = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/statistics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_text(response).await;
    assert!(html.contains("No runs recorded yet."), "{html}");
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
    assert_eq!(
        response
            .headers()
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("no-store")
    );
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

fn agent_payload(comment_id: u64, author: &str, body: &str) -> String {
    json!({
        "action": "created",
        "issue": {
            "number": 1,
            "title": "initial plan",
            "html_url": "http://forge.local:3000/shylock/forge-bot/issues/1"
        },
        "comment": {
            "id": comment_id,
            "body": body,
            "html_url": format!(
                "http://forge.local:3000/shylock/forge-bot/issues/1#issuecomment-{comment_id}"
            ),
            "user": {"login": author}
        },
        "repository": {"full_name": "shylock/forge-bot"},
        "sender": {"login": author}
    })
    .to_string()
}

fn with_human(config: &mut Config) {
    config.users.insert(
        "alice".into(),
        UserConfig {
            role: UserRole::Human,
            host_user: String::new(),
            agent: None,
            agent_model: None,
            token: None,
        },
    );
}

async fn notification_json(harness: &Harness, recipient: &str) -> Value {
    let response = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/notifications.json?recipient={recipient}&after=0"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_str(&body_text(response).await).unwrap()
}

#[tokio::test]
async fn agent_comment_mentioning_a_human_records_a_notification() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness_with(dir.path(), with_human);

    let payload = agent_payload(
        99,
        "shylock-bot",
        "I need a human to register the service account. @alice please help.",
    );
    let response = harness
        .app
        .clone()
        .oneshot(signed_request("issue_comment", &payload))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);

    let json = notification_json(&harness, "alice").await;
    let notifications = json["notifications"].as_array().unwrap();
    assert_eq!(notifications.len(), 1, "{json}");
    assert_eq!(notifications[0]["author"], "shylock-bot");
    assert_eq!(notifications[0]["recipient"], "alice");
    assert_eq!(notifications[0]["generation"], json["generation"]);
    assert_eq!(notifications[0]["repository"], "shylock/forge-bot");
    assert!(
        notifications[0]["message"]
            .as_str()
            .unwrap()
            .contains("register the service account")
    );

    // The page lists the configured human.
    let response = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/notifications")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_text(response).await;
    assert!(html.contains("value=\"alice\""), "{html}");
}

#[tokio::test]
async fn human_mention_from_a_non_agent_does_not_notify() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness_with(dir.path(), with_human);

    let payload = agent_payload(100, "shylock", "hey @alice please look at this");
    let response = harness
        .app
        .clone()
        .oneshot(signed_request("issue_comment", &payload))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);

    let json = notification_json(&harness, "alice").await;
    assert!(
        json["notifications"].as_array().unwrap().is_empty(),
        "{json}"
    );
}

#[tokio::test]
async fn notification_endpoint_rejects_a_missing_recipient() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness_with(dir.path(), with_human);
    let response = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/notifications.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn retried_agent_comment_notifies_a_human_once() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness_with(dir.path(), with_human);
    let payload = agent_payload(101, "shylock-bot", "@alice please approve the request");

    for _ in 0..2 {
        let response = harness
            .app
            .clone()
            .oneshot(signed_request("issue_comment", &payload))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
    }

    let json = notification_json(&harness, "alice").await;
    assert_eq!(json["notifications"].as_array().unwrap().len(), 1, "{json}");
}

async fn get_json(harness: &Harness, uri: &str) -> Value {
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
    serde_json::from_str(&body_text(response).await).unwrap()
}

#[tokio::test]
async fn human_author_does_not_get_the_agent_policy_exemption() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness_with(dir.path(), |config| {
        config.policy.allow_all = false;
        config.policy.allowed_users = vec!["someone-else".into()];
        with_human(config);
        config.users.insert(
            "reviewer".into(),
            UserConfig {
                role: UserRole::Reviewer,
                host_user: "reviewer".into(),
                agent: None,
                agent_model: None,
                token: Some("reviewer-token".into()),
            },
        );
    });

    // A configured human is a notification recipient, not an agent, so it must
    // clear the normal allow-list instead of inheriting the handoff exemption.
    let payload = agent_payload(200, "alice", "@shylock-bot --agent=custom please do it");
    let response = harness
        .app
        .clone()
        .oneshot(signed_request("issue_comment", &payload))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body: Value = serde_json::from_str(&body_text(response).await).unwrap();
    assert_eq!(body["accepted"], 0, "{body}");

    // A genuine agent-to-peer handoff still uses the exemption.
    let payload = agent_payload(201, "shylock-bot", "@reviewer --agent=custom please review");
    let response = harness
        .app
        .clone()
        .oneshot(signed_request("issue_comment", &payload))
        .await
        .unwrap();
    let body: Value = serde_json::from_str(&body_text(response).await).unwrap();
    assert_eq!(body["accepted"], 1, "{body}");
}

#[tokio::test]
async fn notifications_page_serves_a_service_worker() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness_with(dir.path(), with_human);

    let response = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/notifications/sw.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(content_type.contains("javascript"), "{content_type}");
    // The broad scope is what lets `navigator.serviceWorker.ready` resolve on
    // both `/notifications` and `/notify`.
    assert_eq!(
        response
            .headers()
            .get("service-worker-allowed")
            .and_then(|value| value.to_str().ok()),
        Some("/")
    );
    let script = body_text(response).await;
    assert!(script.contains("addEventListener"), "{script}");

    // The page registers that worker and prefers `showNotification`, the only
    // notification API available on Android and iOS browsers.
    let response = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/notifications")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let html = body_text(response).await;
    assert!(
        html.contains("navigator.serviceWorker.register('/notifications/sw.js', { scope: '/' })"),
        "{html}"
    );
    assert!(html.contains("registration.showNotification"), "{html}");
}

#[tokio::test]
async fn notifications_page_links_an_installable_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness_with(dir.path(), with_human);

    let response = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/notifications.webmanifest")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(content_type.contains("manifest"), "{content_type}");
    let manifest: Value = serde_json::from_str(&body_text(response).await).unwrap();
    assert_eq!(manifest["display"], "standalone", "{manifest}");
    assert_eq!(manifest["start_url"], "/notifications", "{manifest}");

    // Without a manifest and the Apple meta tag, an iOS Home Screen save is a
    // bookmark that reopens in the default browser.
    let response = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/notifications")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let html = body_text(response).await;
    assert!(html.contains("rel=\"manifest\""), "{html}");
    assert!(html.contains("/notifications.webmanifest"), "{html}");
    assert!(
        html.contains("name=\"apple-mobile-web-app-capable\" content=\"yes\""),
        "{html}"
    );
}

#[tokio::test]
async fn notifications_serve_png_icon_and_badge() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness_with(dir.path(), with_human);

    for uri in ["/notifications/icon.png", "/notifications/badge.png"] {
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
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("image/png"),
            "{uri}"
        );
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "{uri} is not a PNG");
    }
}

#[tokio::test]
async fn notifications_serve_the_configured_ca_certificate() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness_with(dir.path(), with_human);

    // No CA configured: the route reports that rather than serving anything.
    let response = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/notifications/ca.crt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // With a configured file, the bytes are served as a certificate download.
    let cert = dir.path().join("ca.crt");
    std::fs::write(&cert, b"-----BEGIN CERTIFICATE-----\n").unwrap();
    let harness = harness_with(dir.path(), |config| {
        config.notifications.ca_cert_path = Some(cert.clone());
        with_human(config);
    });
    let response = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/notifications/ca.crt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/x-x509-ca-cert")
    );
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::CONTENT_DISPOSITION)
            .and_then(|value| value.to_str().ok()),
        Some("attachment; filename=\"forge-bot-ca.crt\"")
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&body[..], b"-----BEGIN CERTIFICATE-----\n");
}

#[tokio::test]
async fn notifications_store_and_return_diagnostics() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness_with(dir.path(), with_human);

    // Nothing uploaded yet.
    let response = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/notifications/diagnostics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let report = json!({
        "userAgent": "Android Chrome",
        "notificationPermission": "granted",
        "displayTest": "resolved",
    });
    let response = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/notifications/diagnostics")
                .header("content-type", "application/json")
                .body(Body::from(report.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let response = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/notifications/diagnostics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = serde_json::from_str(&body_text(response).await).unwrap();
    assert_eq!(body["userAgent"], "Android Chrome", "{body}");
    assert_eq!(body["displayTest"], "resolved", "{body}");

    // A non-object body is rejected rather than stored.
    let response = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/notifications/diagnostics")
                .header("content-type", "application/json")
                .body(Body::from("[]"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn notifications_json_resets_a_stale_generation() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness_with(dir.path(), with_human);
    let payload = agent_payload(102, "shylock-bot", "@alice please rotate the key");
    let response = harness
        .app
        .clone()
        .oneshot(signed_request("issue_comment", &payload))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);

    let json = notification_json(&harness, "alice").await;
    let generation = json["generation"].as_str().unwrap().to_owned();
    let id = json["notifications"][0]["id"].as_u64().unwrap();

    // The current generation keeps the cursor, so the entry is not redelivered.
    let current = get_json(
        &harness,
        &format!("/notifications.json?recipient=alice&after={id}&generation={generation}"),
    )
    .await;
    assert!(
        current["notifications"].as_array().unwrap().is_empty(),
        "{current}"
    );

    // A generation from a previous process resets the cursor, so a page left
    // open across a restart does not skip the new entries.
    let stale = get_json(
        &harness,
        &format!("/notifications.json?recipient=alice&after={id}&generation=previous-process"),
    )
    .await;
    assert_eq!(
        stale["notifications"].as_array().unwrap().len(),
        1,
        "{stale}"
    );
}

#[tokio::test]
async fn automatic_policy_denies_before_api_access() {
    for mode in ["empty", "excluded", "scope"] {
        let dir = tempfile::tempdir().unwrap();
        let harness = harness_with(dir.path(), |config| {
            // No token and an unreachable API: denied scope must return before either is used.
            config.forges.forgejo.as_mut().unwrap().base_url = "http://127.0.0.1:1".into();
            match mode {
                "empty" => config.policy.auto_allowed_repos.clear(),
                "excluded" => config.policy.allowed_repos = vec!["other/repo".into()],
                _ => config.policy.auto_allowed_repos = vec!["other/repo".into()],
            }
        });
        for (event, payload) in [
            (
                "action_run_failure",
                json!({"run": {"repository": {"full_name": "shylock/forge-bot"}}}),
            ),
            (
                "push",
                json!({"repository": {"full_name": "shylock/forge-bot"}, "ref": "refs/heads/main"}),
            ),
            (
                "pull_request",
                json!({"action": "synchronized", "repository": {"full_name": "shylock/forge-bot"}, "pull_request": {"number": 7}}),
            ),
        ] {
            assert_eq!(accepted(&harness.app, event, &payload.to_string()).await, 0);
        }
        assert!(harness.sessions.pending_jobs().unwrap().is_empty());
    }
}

#[tokio::test]
async fn automatic_work_rejects_untrusted_or_missing_pr_author_before_mutation() {
    for author in [Some("untrusted"), None] {
        let dir = tempfile::tempdir().unwrap();
        let mut pr = pr_payload(false);
        pr["user"] = author.map(|a| json!({"login": a})).unwrap_or(Value::Null);
        let mutations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = mutations.clone();
        let api = Router::new()
            .route(
                "/api/v1/repos/shylock/forge-bot/pulls/7",
                get(move || {
                    let pr = pr.clone();
                    async move { Json(pr) }
                }),
            )
            .route(
                "/api/v1/repos/shylock/forge-bot/pulls/7/update",
                post(move || {
                    let counter = counter.clone();
                    async move {
                        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        StatusCode::CONFLICT
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, api).await.unwrap() });
        let harness = harness_with(dir.path(), |config| {
            config.forges.forgejo.as_mut().unwrap().base_url = url;
            config.forges.forgejo.as_mut().unwrap().token = Some("test".into());
        });
        let conflict = json!({"action": "synchronized", "repository": {"full_name": "shylock/forge-bot"}, "pull_request": {"number": 7}});
        let ci = json!({"run": {"id": 88, "commit_sha": "head123", "repository": {"full_name": "shylock/forge-bot"}, "event_payload": "{\"pull_request\":{\"number\":7}}"}});
        for (event, payload) in [("pull_request", conflict), ("action_run_failure", ci)] {
            assert_eq!(accepted(&harness.app, event, &payload.to_string()).await, 0);
        }
        assert_eq!(mutations.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(harness.sessions.pending_jobs().unwrap().is_empty());
        server.abort();
    }
}

#[tokio::test]
async fn push_api_registers_humans_and_rejects_invalid_subscriptions() {
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("vapid.pem");
    std::fs::write(&key, include_bytes!("fixtures/vapid-test.pem")).unwrap();
    let harness = harness_with(dir.path(), |config| {
        with_human(config);
        config.notifications.vapid_private_key_path = Some(key);
        config.notifications.vapid_subject = Some("mailto:operator@example.com".into());
    });
    let response = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/notifications/push")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let data: Value = serde_json::from_str(&body_text(response).await).unwrap();
    assert_eq!(data["transport"], "web_push");
    assert_eq!(data["public_key"].as_str().unwrap().len(), 87);
    assert!(data.get("private_key").is_none());
    let mut body = serde_json::json!({
        "recipient": "alice",
        "subscription": {
            "endpoint": "https://fcm.googleapis.com/test",
            "expirationTime": null,
            "keys": {
                "p256dh": "BGa4N1PI79lboMR_YrwCiCsgp35DRvedt7opHcf0yM3iOBTSoQYqQLwWxAfRKE6tsDnReWmhsImkhDF_DBdkNSU",
                "auth": "EvcWjEgzr4rbvhfi3yds0A"
            }
        }
    });
    for (recipient, expected) in [
        ("alice", StatusCode::NO_CONTENT),
        ("ALICE", StatusCode::NO_CONTENT),
        ("shylock-bot", StatusCode::BAD_REQUEST),
        ("unknown", StatusCode::BAD_REQUEST),
    ] {
        body["recipient"] = recipient.into();
        let response = harness
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/notifications/push")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
    body["recipient"] = "alice".into();
    body["subscription"]["endpoint"] = "https://localhost/private".into();
    let response = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/notifications/push")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/notifications/push")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"endpoint":"https://fcm.googleapis.com/test"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn push_api_reports_missing_configuration_without_changing_mode() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness_with(dir.path(), with_human);
    let response = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/notifications/push")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let data: Value = serde_json::from_str(&body_text(response).await).unwrap();
    assert_eq!(data["transport"], "web_push");
    assert!(
        data["error"]
            .as_str()
            .unwrap()
            .contains("vapid_private_key_path")
    );
    let response = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/notifications/push")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"endpoint":"https://fcm.googleapis.com/test"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn push_api_honors_explicit_polling_mode() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness_with(dir.path(), |config| {
        with_human(config);
        config.notifications.transport = forge_bot::config::NotificationTransport::Polling;
    });
    let response = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/notifications/push")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let data: Value = serde_json::from_str(&body_text(response).await).unwrap();
    assert_eq!(data, serde_json::json!({"transport": "polling"}));
}

#[tokio::test]
async fn admin_resets_only_the_selected_agent_cooldown() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(dir.path());
    h.agents
        .mark_unavailable("custom", Duration::from_secs(3600));
    h.agents
        .mark_unavailable("codex", Duration::from_secs(3600));
    let response = h
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let html = std::str::from_utf8(&body).unwrap();
    assert!(html.contains("Reset cooldown"));
    assert!(html.contains("Remaining capacity"));
    assert!(
        html.contains("0 of 1 worker slot busy, 1 free; no jobs queued."),
        "{html}"
    );
    assert!(html.contains("capacity limit"));
    assert!(!h.agents.is_available("custom"));

    for (agent, expected) in [
        ("missing", StatusCode::NOT_FOUND),
        ("custom", StatusCode::NO_CONTENT),
        ("custom", StatusCode::NO_CONTENT),
    ] {
        let response = h
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/reset-cooldown")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"agent": agent}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
    assert!(h.agents.is_available("custom"));
    assert_eq!(h.agents.cooldown_remaining("custom"), None);
    assert!(!h.agents.is_available("codex"));

    for method in ["GET", "POST"] {
        let response = h
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri("/admin/reset-cooldown")
                    .body(Body::from("agent=codex"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(response.status().is_client_error());
        assert!(!h.agents.is_available("codex"));
    }
}

#[tokio::test]
async fn admin_terminate_rejects_threads_that_are_not_running() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(dir.path());
    let response = h
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let html = std::str::from_utf8(&body).unwrap();
    assert!(html.contains("Running threads"));
    assert!(!html.contains("Terminate thread</button>"));

    // A form post cannot stop a run; only the JSON body reaches the handler.
    let response = h
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/terminate-thread")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"key": "forgejo:o/r:issue:1"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let response = h
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/terminate-thread")
                .body(Body::from("key=forgejo:o/r:issue:1"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(response.status().is_client_error());
}

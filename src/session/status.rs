//! A read-only snapshot of every conversation the bot is tracking.
//!
//! The dispatcher already persists the two pieces of state the status page
//! needs — sessions (with one run record per agent run) and not-yet-finished
//! jobs — so this module reconstructs the current state from those instead of
//! keeping a second in-memory registry that could drift or be lost on restart.
//!
//! A conversation is *running* when its newest run has no `finished_at`, and
//! *queued* when it has no run in flight but still has a pending job. Anything
//! else is *idle*.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::Serialize;

use super::Job;
use super::store::Session;

/// What a conversation is doing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadState {
    /// An agent is working on the thread.
    Running,
    /// A mention is waiting for a free worker or for the current run to end.
    Queued,
    /// The thread has no work in flight.
    Idle,
}

impl ThreadState {
    /// Lower-case label used by the HTML page and the JSON payload.
    pub fn label(self) -> &'static str {
        match self {
            ThreadState::Running => "running",
            ThreadState::Queued => "queued",
            ThreadState::Idle => "idle",
        }
    }

    /// Sort weight: active threads come before idle ones.
    fn rank(self) -> u8 {
        match self {
            ThreadState::Running => 0,
            ThreadState::Queued => 1,
            ThreadState::Idle => 2,
        }
    }
}

/// One row of the status page.
#[derive(Debug, Clone, Serialize)]
pub struct ThreadStatus {
    /// Stable conversation key, e.g. `forgejo:owner/repo:issue:12`.
    pub key: String,
    pub repository: String,
    /// Canonical forge URL of the issue or pull request.
    pub location: String,
    pub number: Option<u64>,
    /// `issue` or `pull_request`.
    pub thread_type: String,
    pub state: ThreadState,
    /// Agent currently running, next to run, or the one that ran last.
    pub agent: String,
    /// Model reported by the active or most recent run, if known.
    pub model: Option<String>,
    /// Follow-up mentions waiting behind the current run.
    pub queued: usize,
    /// Total number of runs recorded for the conversation.
    pub runs: usize,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// When the in-flight run started, if any.
    pub running_since: Option<DateTime<Utc>>,
    /// Whether the most recent finished run succeeded.
    pub last_success: Option<bool>,
    pub last_summary: Option<String>,
    pub last_finished_at: Option<DateTime<Utc>>,
}

/// Build the status snapshot from persisted sessions and pending jobs.
///
/// `pending` is expected to be ordered oldest-first, as returned by
/// [`SessionStore::pending_jobs`](super::SessionStore::pending_jobs); the first
/// pending job of a conversation is the one a worker picks up next.
pub fn snapshot(sessions: Vec<Session>, pending: Vec<Job>) -> Vec<ThreadStatus> {
    let mut pending_by_key: HashMap<String, Vec<Job>> = HashMap::new();
    for job in pending {
        pending_by_key
            .entry(job.session_key())
            .or_default()
            .push(job);
    }

    let mut threads = Vec::with_capacity(sessions.len() + pending_by_key.len());
    let mut seen = HashSet::new();

    for session in sessions {
        seen.insert(session.key.clone());
        let waiting = pending_by_key.remove(&session.key).unwrap_or_default();
        threads.push(from_session(session, waiting));
    }

    // A mention can be queued before its session exists: the dispatcher only
    // creates the session when a worker picks the job up, so a thread can wait
    // in the queue with no run records at all.
    for (key, mut waiting) in pending_by_key {
        let first = waiting.remove(0);
        let updated_at = waiting
            .last()
            .map_or(first.created_at, |job| job.created_at);
        threads.push(ThreadStatus {
            key,
            repository: first.message.repository.clone(),
            location: first.message.location.to_string(),
            number: first.message.number,
            thread_type: thread_type(first.message.is_pull_request).to_owned(),
            state: ThreadState::Queued,
            agent: first.agent.clone(),
            model: None,
            queued: waiting.len() + 1,
            runs: 0,
            created_at: first.created_at,
            updated_at,
            running_since: None,
            last_success: None,
            last_summary: None,
            last_finished_at: None,
        });
    }

    threads.sort_by(|a, b| {
        a.state
            .rank()
            .cmp(&b.state.rank())
            .then_with(|| b.updated_at.cmp(&a.updated_at))
            .then_with(|| a.key.cmp(&b.key))
    });
    threads
}

fn from_session(session: Session, waiting: Vec<Job>) -> ThreadStatus {
    let running = session
        .runs
        .iter()
        .rev()
        .find(|run| run.finished_at.is_none());
    let last = session
        .runs
        .iter()
        .rev()
        .find(|run| run.finished_at.is_some());
    let (number, thread_type) = key_parts(&session.key);

    let (state, agent, running_since) = match running {
        Some(run) => (
            ThreadState::Running,
            run.agent.clone(),
            Some(run.started_at),
        ),
        None if !waiting.is_empty() => (ThreadState::Queued, waiting[0].agent.clone(), None),
        None => (
            ThreadState::Idle,
            last.map_or_else(|| session.agent.clone(), |run| run.agent.clone()),
            None,
        ),
    };

    // While a run is in flight its own job file is still pending, so it must
    // not be counted as a follow-up waiting behind it.
    let queued = waiting.len().saturating_sub(usize::from(running.is_some()));

    ThreadStatus {
        key: session.key,
        repository: session.repository,
        location: session.location,
        number,
        thread_type,
        state,
        agent,
        model: running.or(last).and_then(|run| run.model.clone()),
        queued,
        runs: session.runs.len(),
        created_at: session.created_at,
        updated_at: session.updated_at,
        running_since,
        last_success: last.and_then(|run| run.success),
        last_summary: last.and_then(|run| run.summary.clone()),
        last_finished_at: last.and_then(|run| run.finished_at),
    }
}

/// Recover the issue/PR number and kind from a session key, which is the only
/// place the persisted session keeps them. Falls back to a plain issue when
/// the key is not in the expected `forge:repository:kind:number` shape.
fn key_parts(key: &str) -> (Option<u64>, String) {
    let parts: Vec<&str> = key.splitn(4, ':').collect();
    match parts.as_slice() {
        [_, _, kind, number] => (number.parse().ok(), thread_type(*kind == "pr").to_owned()),
        _ => (None, "issue".to_owned()),
    }
}

fn thread_type(is_pull_request: bool) -> &'static str {
    if is_pull_request {
        "pull_request"
    } else {
        "issue"
    }
}

/// Coordinates parsed from a comment or issue/PR URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadRef {
    /// `owner/repo`.
    pub repository: String,
    pub number: u64,
    /// `issue` or `pull_request`.
    pub thread_type: String,
    /// Comment id from the URL fragment, when present.
    pub comment_id: Option<u64>,
}

/// Parse a comment or issue/PR URL into the coordinates of the thread it
/// belongs to.
///
/// Accepts full URLs (`https://forge/owner/repo/pulls/460#issuecomment-10561`),
/// GitHub (`/pull/460`), GitLab (`/-/merge_requests/460#note_105`), and the bare
/// path (`owner/repo/issues/12`). The host is ignored, so a URL copied from any
/// view of the same thread resolves correctly.
pub fn parse_thread_ref(query: &str) -> Option<ThreadRef> {
    let (head, fragment) = match query.split_once('#') {
        Some((head, fragment)) => (head.trim(), Some(fragment)),
        None => (query.trim(), None),
    };
    let path = url::Url::parse(head).map_or_else(|_| head.to_owned(), |url| url.path().to_owned());
    let segments: Vec<&str> = path
        .split('/')
        .filter(|segment| !segment.is_empty() && *segment != "-")
        .collect();
    let marker = segments.iter().position(|segment| {
        matches!(
            *segment,
            "issues" | "pulls" | "pull" | "merge_requests" | "merge_request"
        )
    })?;
    if marker < 2 {
        return None;
    }
    let thread_type = match segments[marker] {
        "pulls" | "pull" | "merge_requests" | "merge_request" => "pull_request",
        _ => "issue",
    };
    let number = segments
        .get(marker + 1)?
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    Some(ThreadRef {
        repository: format!("{}/{}", segments[marker - 2], segments[marker - 1]),
        number,
        thread_type: thread_type.to_owned(),
        comment_id: fragment.and_then(comment_id),
    })
}

/// Trailing digits of a URL fragment such as `issuecomment-10561` or
/// `note_10561`.
fn comment_id(fragment: &str) -> Option<u64> {
    fragment
        .rsplit(|c: char| !c.is_ascii_digit())
        .next()
        .filter(|digits| !digits.is_empty())
        .and_then(|digits| digits.parse().ok())
}

/// Filter `threads` to those named by a comment or issue/PR URL.
///
/// A blank query returns every thread. A query that cannot be parsed as a
/// thread reference returns no matches. The match ignores the forge host and
/// the comment fragment: a comment URL resolves to the thread it belongs to.
pub fn search<'a>(threads: &'a [ThreadStatus], query: Option<&str>) -> Vec<&'a ThreadStatus> {
    let Some(query) = query.map(str::trim).filter(|query| !query.is_empty()) else {
        return threads.iter().collect();
    };
    let Some(reference) = parse_thread_ref(query) else {
        return Vec::new();
    };
    threads
        .iter()
        .filter(|thread| {
            thread.repository == reference.repository
                && thread.number == Some(reference.number)
                && thread.thread_type == reference.thread_type
        })
        .collect()
}

/// Render the status snapshot as a self-contained HTML page.
///
/// When `query` names a thread (a comment or issue/PR URL), only matching rows
/// are shown and the search box keeps the query so the auto-refresh preserves
/// the filter.
pub fn render_html(threads: &[ThreadStatus], query: Option<&str>) -> String {
    let query = query.map(str::trim).filter(|query| !query.is_empty());
    let matched = search(threads, query);
    let running = matched
        .iter()
        .filter(|thread| thread.state == ThreadState::Running)
        .count();
    let queued = matched
        .iter()
        .filter(|thread| thread.state == ThreadState::Queued)
        .count();
    let now = Utc::now();

    let mut rows = String::new();
    if matched.is_empty() {
        let message = match query {
            Some(query) => format!("No thread matches “{}”.", escape_html(query)),
            None => "No threads yet.".to_owned(),
        };
        rows.push_str(&format!(
            "<tr><td colspan=\"9\" class=\"empty\">{message}</td></tr>"
        ));
    }
    for thread in &matched {
        rows.push_str(&render_row(thread, now));
    }

    let summary = match query {
        Some(query) => format!(
            "{} of {} thread(s) match “{}”: {running} running · {queued} queued.",
            matched.len(),
            threads.len(),
            escape_html(query),
        ),
        None => format!(
            "{} thread(s): {running} running · {queued} queued.",
            threads.len(),
        ),
    };
    let search_value = escape_html(query.unwrap_or_default());
    let clear = if query.is_some() {
        "<a class=\"clear\" href=\"/status\">clear</a>"
    } else {
        ""
    };

    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta http-equiv="refresh" content="15">
<title>forge-bot — thread status</title>
<style>
:root {{ color-scheme: light dark; }}
body {{ font-family: system-ui, -apple-system, "Segoe UI", sans-serif; margin: 2rem; }}
h1 {{ font-size: 1.35rem; margin: 0 0 .75rem; }}
p.sub {{ color: #888; margin: 0 0 1.25rem; }}
form.search {{ display: flex; gap: .5rem; align-items: center; margin: 0 0 1rem; }}
form.search input {{ flex: 1; max-width: 34rem; padding: .4rem .55rem; font: inherit; color: inherit; background: transparent; border: 1px solid #8886; border-radius: .35rem; }}
form.search button {{ padding: .4rem .75rem; font: inherit; color: inherit; background: #8881; border: 1px solid #8886; border-radius: .35rem; cursor: pointer; }}
form.search a.clear {{ color: #888; }}
table {{ border-collapse: collapse; width: 100%; font-size: .92rem; }}
th, td {{ text-align: left; padding: .5rem .65rem; border-bottom: 1px solid #8883; vertical-align: top; }}
th {{ font-weight: 600; }}
td.repo {{ color: #888; font-size: .85rem; }}
tr.empty td {{ text-align: center; color: #888; padding: 2rem; }}
a {{ color: inherit; }}
.state {{ display: inline-block; border-radius: 999px; padding: .12rem .55rem; font-size: .78rem; font-weight: 600; }}
.state.running {{ background: #2f9e44; color: #fff; }}
.state.queued {{ background: #f08c00; color: #fff; }}
.state.idle {{ background: #868e9633; }}
</style>
</head>
<body>
<h1>forge-bot — thread status</h1>
<form class="search" method="get" action="/status">
<input type="search" name="q" value="{query}" placeholder="owner/repo/pulls/123#issuecomment-456" aria-label="Search by comment URL">
<button type="submit">Search</button>
{clear}
</form>
<p class="sub">{summary} Refreshes every 15s.</p>
<table>
<thead><tr><th>State</th><th>Thread</th><th>Agent</th><th>Model</th><th>Queued</th><th>Runs</th><th>Updated</th><th>Last result</th><th>Details</th></tr></thead>
<tbody>
{rows}
</tbody>
</table>
</body>
</html>
"#,
        query = search_value,
        summary = summary,
        clear = clear,
        rows = rows,
    )
}

fn render_row(thread: &ThreadStatus, now: DateTime<Utc>) -> String {
    let details_url = format!(
        "/status/details?{}",
        url::form_urlencoded::Serializer::new(String::new())
            .append_pair("key", &thread.key)
            .finish()
    );
    let number = thread.number.map(|n| format!(" #{n}")).unwrap_or_default();
    let thread_cell = format!(
        "<a href=\"{location}\">{repository}{number}</a><div class=\"repo\">{kind}</div>",
        location = escape_html(&thread.location),
        repository = escape_html(&thread.repository),
        number = escape_html(&number),
        kind = escape_html(&thread.thread_type),
    );

    let agent = if thread.state == ThreadState::Running {
        match thread.running_since {
            Some(since) => format!(
                "{}<div class=\"repo\">for {}</div>",
                escape_html(&thread.agent),
                escape_html(&humanize_age(now, since))
            ),
            None => escape_html(&thread.agent),
        }
    } else {
        escape_html(&thread.agent)
    };

    let last = match thread.last_success {
        Some(true) => format!(
            "✅ {}",
            escape_html(&humanize_age(
                now,
                thread.last_finished_at.unwrap_or(thread.updated_at)
            ))
        ),
        Some(false) => format!(
            "❌ {}",
            escape_html(&humanize_age(
                now,
                thread.last_finished_at.unwrap_or(thread.updated_at)
            ))
        ),
        None => "—".to_owned(),
    };

    format!(
        "<tr><td><span class=\"state {state}\">{label}</span></td>\
<td>{thread_cell}</td>\
<td>{agent}</td>\
<td>{model}</td>\
<td>{queued}</td>\
<td>{runs}</td>\
<td>{updated}</td>\
<td>{last}</td>\
<td><a href=\"{details_url}\">View</a></td></tr>\n",
        state = thread.state.label(),
        label = thread.state.label(),
        model = thread
            .model
            .as_deref()
            .map(escape_html)
            .unwrap_or_else(|| "—".to_owned()),
        queued = thread.queued,
        runs = thread.runs,
        updated = escape_html(&humanize_age(now, thread.updated_at)),
    )
}

/// Render the persisted conversation history. A queued thread may have no
/// session yet, so its details page still renders with an empty run list.
pub fn render_details(
    thread: &ThreadStatus,
    session: Option<&Session>,
    live_output: Option<&str>,
) -> String {
    let title = format!(
        "{} #{}",
        thread.repository,
        thread.number.unwrap_or_default()
    );
    let mut runs = String::new();
    if let Some(session) = session {
        for (index, run) in session.runs.iter().enumerate().rev() {
            let result = match run.success {
                Some(true) => "Succeeded",
                Some(false) => "Failed",
                None => "Running",
            };
            let message = run
                .message
                .as_deref()
                .unwrap_or("Unavailable for this run.");
            let summary = run.summary.as_deref().unwrap_or("No result yet.");
            let finished = run
                .finished_at
                .map(|time| time.to_rfc3339())
                .unwrap_or_else(|| "In progress".to_owned());
            runs.push_str(&format!(
                "<section><h2>Run {number}: {agent} · {result}</h2>\
<p>Started: <time>{started}</time> · Finished: <time>{finished}</time></p>\
<h3>Request</h3><pre>{message}</pre>\
<h3>Result</h3><pre>{summary}</pre></section>",
                number = index + 1,
                agent = escape_html(&run.agent),
                started = escape_html(&run.started_at.to_rfc3339()),
                finished = escape_html(&finished),
                message = escape_html(message),
                summary = escape_html(summary),
            ));
            if run.finished_at.is_none() {
                let output = live_output
                    .filter(|output| !output.is_empty())
                    .unwrap_or("Waiting for agent output.");
                runs.push_str(&format!(
                    "<h3>Live output</h3><pre>{}</pre>",
                    escape_html(output)
                ));
            }
        }
    }
    if runs.is_empty() {
        runs.push_str("<p>No runs yet.</p>");
    }
    let refresh = if thread.state == ThreadState::Running {
        "<meta http-equiv=\"refresh\" content=\"2\">"
    } else {
        ""
    };
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
{refresh}<title>{title} — session details</title><style>\
:root {{ color-scheme: light dark; }}\
body {{ font-family: system-ui, sans-serif; max-width: 70rem; margin: 2rem auto; padding: 0 1rem; }}\
section {{ border-top: 1px solid #8885; margin-top: 1.5rem; }}\
pre {{ white-space: pre-wrap; overflow-wrap: anywhere; padding: 1rem; background: #8882; border-radius: .4rem; }}\
a {{ color: inherit; }}</style></head><body>\
<p><a href=\"/status\">← Status</a></p><h1>{title}</h1>\
<p><a href=\"{location}\">Open thread</a> · {state} · {count} run(s)</p>\
{runs}</body></html>",
        title = escape_html(&title),
        location = escape_html(&thread.location),
        state = thread.state.label(),
        count = thread.runs,
    )
}

/// Escape an arbitrary string for safe interpolation into HTML text or a
/// double-quoted attribute.
pub fn escape_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Render a timestamp as roughly how long ago it was, e.g. `3m ago`.
fn humanize_age(now: DateTime<Utc>, then: DateTime<Utc>) -> String {
    let seconds = (now - then).num_seconds().max(0);
    if seconds < 60 {
        format!("{seconds}s ago")
    } else if seconds < 60 * 60 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 24 * 60 * 60 {
        format!("{}h ago", seconds / (60 * 60))
    } else {
        format!("{}d ago", seconds / (24 * 60 * 60))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::location::ForgeKind;
    use crate::mention::Mention;
    use crate::session::store::{RunRecord, Session};
    use url::Url;

    fn session(key: &str, runs: Vec<RunRecord>) -> Session {
        let now = Utc::now();
        Session {
            key: key.to_owned(),
            repository: "owner/repo".into(),
            location: "http://forge.local/owner/repo/issues/1".into(),
            agent: "codex".into(),
            created_at: now,
            updated_at: now,
            runs,
        }
    }

    fn job(number: u64, is_pr: bool, agent: &str) -> Job {
        let path = if is_pr { "pulls" } else { "issues" };
        Job {
            id: uuid::Uuid::new_v4(),
            message: crate::forge::ForgeMessage {
                forge: ForgeKind::Forgejo,
                location: Url::parse(&format!("http://forge.local/owner/repo/{path}/{number}"))
                    .unwrap(),
                body: "@agent go".into(),
                author: "alice".into(),
                repository: "owner/repo".into(),
                comment_id: Some(1),
                number: Some(number),
                is_pull_request: is_pr,
                linked_issue: None,
                event: "issue_comment".into(),
                title: None,
                reply_target: Default::default(),
            },
            mention: Mention {
                agent: None,
                message: "go".into(),
            },
            agent: agent.into(),
            created_at: Utc::now(),
            status_comment: None,
            waiting: false,
        }
    }

    fn finished(success: bool) -> RunRecord {
        RunRecord {
            job_id: uuid::Uuid::new_v4(),
            agent: "codex".into(),
            message: Some("fix the issue".into()),
            started_at: Utc::now() - chrono::Duration::seconds(30),
            finished_at: Some(Utc::now() - chrono::Duration::seconds(10)),
            success: Some(success),
            summary: Some("done".into()),
            model: Some("openai/test-model".into()),
        }
    }

    fn running() -> RunRecord {
        RunRecord {
            job_id: uuid::Uuid::new_v4(),
            agent: "pi-rpc".into(),
            message: Some("continue".into()),
            started_at: Utc::now() - chrono::Duration::seconds(5),
            finished_at: None,
            success: None,
            summary: None,
            model: None,
        }
    }

    #[test]
    fn idle_session_reports_last_result() {
        let key = "forgejo:owner/repo:issue:1";
        let threads = snapshot(vec![session(key, vec![finished(true)])], vec![]);
        assert_eq!(threads.len(), 1);
        let thread = &threads[0];
        assert_eq!(thread.state, ThreadState::Idle);
        assert_eq!(thread.agent, "codex");
        assert_eq!(thread.model.as_deref(), Some("openai/test-model"));
        assert_eq!(thread.number, Some(1));
        assert_eq!(thread.thread_type, "issue");
        assert_eq!(thread.runs, 1);
        assert_eq!(thread.last_success, Some(true));
        assert_eq!(thread.queued, 0);
    }

    #[test]
    fn running_session_does_not_count_its_own_job_as_queued() {
        let key = "forgejo:owner/repo:issue:1";
        let threads = snapshot(
            vec![session(key, vec![finished(false), running()])],
            vec![job(1, false, "pi-rpc"), job(1, false, "codex")],
        );
        let thread = &threads[0];
        assert_eq!(thread.state, ThreadState::Running);
        assert_eq!(thread.agent, "pi-rpc");
        assert!(thread.running_since.is_some());
        // Two pending jobs: the running one, plus one follow-up.
        assert_eq!(thread.queued, 1);
        assert_eq!(thread.runs, 2);
        // The last *finished* run is reported, not the in-flight one.
        assert_eq!(thread.last_success, Some(false));
    }

    #[test]
    fn queued_session_without_running_job_names_next_agent() {
        let key = "forgejo:owner/repo:pr:7";
        let threads = snapshot(
            vec![session(key, vec![finished(true)])],
            vec![job(7, true, "agy")],
        );
        let thread = &threads[0];
        assert_eq!(thread.state, ThreadState::Queued);
        assert_eq!(thread.agent, "agy");
        assert_eq!(thread.thread_type, "pull_request");
        assert_eq!(thread.number, Some(7));
        assert_eq!(thread.queued, 1);
        assert_eq!(thread.last_success, Some(true));
    }

    #[test]
    fn queued_thread_without_session_is_listed() {
        let mut first = job(9, false, "codex");
        first.created_at = Utc::now() - chrono::Duration::seconds(60);
        let mut second = job(9, false, "pi-rpc");
        second.created_at = Utc::now();
        let threads = snapshot(vec![], vec![first, second]);
        assert_eq!(threads.len(), 1);
        let thread = &threads[0];
        assert_eq!(thread.state, ThreadState::Queued);
        assert_eq!(thread.agent, "codex");
        assert_eq!(thread.queued, 2);
        assert_eq!(thread.runs, 0);
        assert_eq!(thread.last_success, None);
    }

    #[test]
    fn active_threads_sort_before_idle_ones() {
        let running_key = "forgejo:owner/repo:issue:1";
        let idle_key = "forgejo:owner/repo:issue:2";
        let threads = snapshot(
            vec![
                session(idle_key, vec![finished(true)]),
                session(running_key, vec![running()]),
            ],
            vec![],
        );
        assert_eq!(threads[0].state, ThreadState::Running);
        assert_eq!(threads[1].state, ThreadState::Idle);
    }

    #[test]
    fn unexpected_key_shape_still_renders() {
        let (number, kind) = key_parts("malformed");
        assert_eq!(number, None);
        assert_eq!(kind, "issue");
        assert_eq!(
            key_parts("forgejo:o/r:pr:3"),
            (Some(3), "pull_request".into())
        );
    }

    #[test]
    fn humanizes_age_buckets() {
        let now = Utc::now();
        let ago = |secs| now - chrono::Duration::seconds(secs);
        assert_eq!(humanize_age(now, ago(5)), "5s ago");
        assert_eq!(humanize_age(now, ago(90)), "1m ago");
        assert_eq!(humanize_age(now, ago(3 * 60 * 60)), "3h ago");
        assert_eq!(humanize_age(now, ago(2 * 24 * 60 * 60)), "2d ago");
        // A clock skew must not produce a negative age.
        assert_eq!(
            humanize_age(now, now + chrono::Duration::seconds(5)),
            "0s ago"
        );
    }

    #[test]
    fn html_escapes_untrusted_text() {
        assert_eq!(
            escape_html(r#"<b>"x"&'</b>"#),
            "&lt;b&gt;&quot;x&quot;&amp;&#39;&lt;/b&gt;"
        );
    }

    #[test]
    fn html_page_lists_threads_and_states() {
        let key = "forgejo:owner/repo:issue:1";
        let threads = snapshot(vec![session(key, vec![running()])], vec![]);
        let html = render_html(&threads, None);
        assert!(html.contains("forge-bot — thread status"));
        assert!(html.contains("state running"));
        assert!(html.contains("owner/repo"));
        assert!(html.contains("pi-rpc"));
        assert!(html.contains("http://forge.local/owner/repo/issues/1"));
        assert!(html.contains("<th>Details</th>"));
        assert!(html.contains("/status/details?key=forgejo%3Aowner%2Frepo%3Aissue%3A1"));
    }

    #[test]
    fn details_show_run_history_and_escape_content() {
        let key = "forgejo:owner/repo:issue:1";
        let mut record = finished(true);
        record.message = Some("<script>alert(1)</script>".into());
        record.summary = Some("<img src=x>".into());
        let stored = session(key, vec![record]);
        let threads = snapshot(vec![stored.clone()], vec![]);
        let html = render_details(&threads[0], Some(&stored), None);
        assert!(html.contains("Run 1: codex · Succeeded"));
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(html.contains("&lt;img src=x&gt;"));
        assert!(!html.contains("<script>"));
        assert!(!html.contains("<img"));
    }

    #[test]
    fn details_show_escaped_live_output_and_refresh() {
        let stored = session("forgejo:owner/repo:issue:1", vec![running()]);
        let threads = snapshot(vec![stored.clone()], vec![]);
        let html = render_details(&threads[0], Some(&stored), Some("<script>live</script>"));
        assert!(html.contains("http-equiv=\"refresh\" content=\"2\""));
        assert!(html.contains("<h3>Live output</h3>"));
        assert!(html.contains("&lt;script&gt;live&lt;/script&gt;"));
        assert!(!html.contains("<script>"));
    }

    #[test]
    fn html_page_handles_no_threads() {
        let html = render_html(&[], None);
        assert!(html.contains("No threads yet."));
        assert!(html.contains("0 thread(s)"));
    }

    #[test]
    fn html_row_escapes_dynamic_fields() {
        let now = Utc::now();
        let thread = ThreadStatus {
            key: "k".into(),
            repository: "<script>alert(1)</script>".into(),
            location: "http://x/\" onmouseover=\"evil()".into(),
            number: None,
            thread_type: "issue".into(),
            state: ThreadState::Idle,
            agent: "<img src=x>".into(),
            model: Some("<script>model</script>".into()),
            queued: 0,
            runs: 0,
            created_at: now,
            updated_at: now,
            running_since: None,
            last_success: Some(false),
            last_summary: None,
            last_finished_at: Some(now),
        };
        let html = render_row(&thread, now);
        assert!(!html.contains("<script>"));
        assert!(!html.contains("<img"));
        assert!(!html.contains("<script>model"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("&lt;script&gt;model&lt;/script&gt;"));
        assert!(html.contains("&quot; onmouseover=&quot;"));
        assert!(html.contains("❌"));
    }

    #[test]
    fn parses_forgejo_comment_urls() {
        let reference = parse_thread_ref(
            "https://forgejo.shylockhg.me/shylock/stock-analysis/pulls/460#issuecomment-10561",
        )
        .unwrap();
        assert_eq!(reference.repository, "shylock/stock-analysis");
        assert_eq!(reference.number, 460);
        assert_eq!(reference.thread_type, "pull_request");
        assert_eq!(reference.comment_id, Some(10561));
    }

    #[test]
    fn parses_github_gitlab_and_bare_paths() {
        let github = parse_thread_ref("https://github.com/o/r/pull/7#issuecomment-9").unwrap();
        assert_eq!(github.repository, "o/r");
        assert_eq!(github.number, 7);
        assert_eq!(github.thread_type, "pull_request");
        assert_eq!(github.comment_id, Some(9));

        let gitlab = parse_thread_ref("https://gitlab.com/o/r/-/issues/3#note_42").unwrap();
        assert_eq!(gitlab.repository, "o/r");
        assert_eq!(gitlab.number, 3);
        assert_eq!(gitlab.thread_type, "issue");
        assert_eq!(gitlab.comment_id, Some(42));

        let bare = parse_thread_ref("o/r/issues/12").unwrap();
        assert_eq!(bare.repository, "o/r");
        assert_eq!(bare.number, 12);
        assert_eq!(bare.thread_type, "issue");
        assert_eq!(bare.comment_id, None);
    }

    #[test]
    fn unparseable_search_returns_nothing() {
        assert_eq!(parse_thread_ref("not a thread"), None);
        assert_eq!(parse_thread_ref("https://forge/o/r"), None);
        assert_eq!(parse_thread_ref(""), None);
    }

    #[test]
    fn search_matches_the_comment_thread() {
        let threads = snapshot(
            vec![],
            vec![job(460, true, "pi-rpc"), job(12, false, "codex")],
        );
        let matched = search(
            &threads,
            Some("https://forgejo.shylockhg.me/owner/repo/pulls/460#issuecomment-10561"),
        );
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].number, Some(460));
        assert_eq!(matched[0].thread_type, "pull_request");

        // The wrong kind for the same number matches nothing.
        assert!(search(&threads, Some("https://forge/owner/repo/issues/460")).is_empty());
        // A blank query returns every thread.
        assert_eq!(search(&threads, Some("  ")).len(), 2);
        assert_eq!(search(&threads, None).len(), 2);
    }

    #[test]
    fn html_search_filters_rows_and_keeps_the_query() {
        let threads = snapshot(
            vec![],
            vec![job(460, true, "pi-rpc"), job(12, false, "codex")],
        );
        let html = render_html(
            &threads,
            Some("https://forgejo.shylockhg.me/owner/repo/pulls/460#issuecomment-10561"),
        );
        assert!(html.contains("1 of 2 thread(s) match"), "{html}");
        assert!(html.contains("pulls/460#issuecomment-10561"), "{html}");
        assert!(html.contains("pi-rpc"), "{html}");
        assert!(!html.contains("codex"), "{html}");
        assert!(html.contains("href=\"/status\">clear</a>"), "{html}");
    }

    #[test]
    fn html_search_without_matches_says_so() {
        let threads = snapshot(vec![], vec![job(12, false, "codex")]);
        let html = render_html(&threads, Some("https://forge/owner/repo/issues/999"));
        assert!(html.contains("No thread matches"), "{html}");
        assert!(html.contains("0 of 1 thread(s) match"), "{html}");
    }
}

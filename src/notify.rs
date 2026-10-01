//! Human notifications for agent requests that need a person.
//!
//! An agent mentions a configured `human` login when it needs something only a
//! person can do: a privilege request, an account registration, a secret, and
//! so on. forge-bot records that mention as a [`Notification`]. The
//! `/notifications` web page polls the bounded log and raises a browser system
//! notification.
//!
//! Desktop browsers use the [`Notification`] constructor, but Android and iOS
//! browsers reject it and require
//! `ServiceWorkerRegistration.showNotification`, so the page registers a small
//! service worker and prefers `showNotification` when it is available. Service
//! workers need a secure context: serve forge-bot over HTTPS for a mobile
//! browser, and on iOS 16.4+ add the page to the Home Screen before granting
//! notification permission. See the WebKit web-push requirements:
//! <https://webkit.org/blog/13878/web-push-for-web-apps-on-ios-and-ipados/>.
//!
//! The log is deliberately in memory and bounded: a notification is a nudge,
//! not an audit trail, and the forge comment that mentioned the human remains
//! the durable record. Because ids restart at one when the process restarts,
//! the [`Notifier`] also carries a per-process `generation`; a page that sends
//! an older generation has its cursor reset so it does not skip the new
//! entries.

use std::collections::{HashSet, VecDeque};
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

use crate::forge::ForgeMessage;

/// The `/notifications` page script, kept in its own file so it can be
/// exercised directly and so it does not need brace-escaping inside the HTML
/// template.
const PAGE_SCRIPT: &str = include_str!("notifications.js");

/// One pending human notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Notification {
    /// Monotonic id used by the web page as a `since` cursor.
    pub id: u64,
    /// Configured human login the notification is addressed to.
    pub recipient: String,
    /// Login of the agent that asked for help.
    pub author: String,
    /// Repository the request came from.
    pub repository: String,
    /// Canonical forge URL of the issue or pull request.
    pub location: String,
    /// The agent's comment body, trimmed to a readable length.
    pub message: String,
    pub created_at: DateTime<Utc>,
}

/// In-memory, bounded log of human notifications.
#[derive(Debug)]
pub struct Notifier {
    state: Mutex<NotifierState>,
}

struct NotifierState {
    /// Identifies this process instance. Ids restart at one on restart, so the
    /// page compares generations before trusting its cursor.
    generation: String,
    next_id: u64,
    log: VecDeque<Notification>,
}

impl std::fmt::Debug for NotifierState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NotifierState")
            .field("generation", &self.generation)
            .field("next_id", &self.next_id)
            .field("log_len", &self.log.len())
            .finish()
    }
}

impl Default for Notifier {
    fn default() -> Self {
        Self::new()
    }
}

/// How many unread notifications are kept. The oldest are dropped first, so a
/// page that has been away for a long time still sees the most recent nudges
/// instead of growing without bound.
const CAPACITY: usize = 1024;

/// Maximum length of the stored message snippet.
const MAX_MESSAGE: usize = 500;

impl Notifier {
    /// Create an empty notifier with a fresh process generation.
    pub fn new() -> Self {
        Self {
            state: Mutex::new(NotifierState {
                generation: Uuid::new_v4().to_string(),
                next_id: 0,
                log: VecDeque::new(),
            }),
        }
    }

    /// Identifier for this process's notification log.
    pub fn generation(&self) -> String {
        self.state
            .lock()
            .expect("notifier mutex poisoned")
            .generation
            .clone()
    }

    /// Record one notification and return its id.
    ///
    /// `body` is trimmed and truncated so a very long agent comment cannot
    /// make the page unwieldy. Returns `None` when the message is empty: an
    /// empty mention is not an actionable human request.
    pub fn record(
        &self,
        recipient: &str,
        author: &str,
        repository: &str,
        location: &str,
        body: &str,
    ) -> Option<u64> {
        let message = truncate(body.trim(), MAX_MESSAGE);
        if message.is_empty() {
            return None;
        }
        let mut state = self.state.lock().expect("notifier mutex poisoned");
        state.next_id += 1;
        let id = state.next_id;
        state.log.push_back(Notification {
            id,
            recipient: recipient.to_owned(),
            author: author.to_owned(),
            repository: repository.to_owned(),
            location: location.to_owned(),
            message,
            created_at: Utc::now(),
        });
        while state.log.len() > CAPACITY {
            state.log.pop_front();
        }
        Some(id)
    }

    /// Notifications for `recipient` newer than `after`, oldest first.
    ///
    /// `generation` is the value the page last saw. When it names an earlier
    /// process, the cursor is reset to zero: ids began at one again, so an old
    /// cursor would hide every new notification.
    pub fn since(
        &self,
        recipient: &str,
        after: u64,
        generation: Option<&str>,
    ) -> Vec<Notification> {
        let state = self.state.lock().expect("notifier mutex poisoned");
        let after = match generation {
            Some(generation) if generation != state.generation => 0,
            _ => after,
        };
        state
            .log
            .iter()
            .filter(|n| n.id > after && n.recipient.eq_ignore_ascii_case(recipient))
            .cloned()
            .collect()
    }

    /// The highest id currently stored, so a fresh page can skip the backlog.
    pub fn latest_id(&self) -> u64 {
        self.state
            .lock()
            .expect("notifier mutex poisoned")
            .log
            .back()
            .map_or(0, |n| n.id)
    }
}

/// Truncate `value` to at most `limit` bytes on a character boundary.
fn truncate(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_owned();
    }
    let mut end = limit;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

/// A small bounded set remembering recently seen keys.
pub(crate) struct RecentComments {
    order: VecDeque<String>,
    set: HashSet<String>,
    capacity: usize,
}

impl RecentComments {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            order: VecDeque::new(),
            set: HashSet::new(),
            capacity,
        }
    }

    /// Returns true when the key was already present.
    pub(crate) fn insert(&mut self, key: &str) -> bool {
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

/// Stable key used to dedupe notifications for the same delivery. Comments use
/// their id; description events use the issue number plus a hash of the body,
/// so an edited description is a new delivery but a re-delivery is not.
pub(crate) fn delivery_key(message: &ForgeMessage) -> String {
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

/// Render the `/notifications` page.
///
/// `humans` is every configured `human` login. The page asks for browser
/// notification permission, then polls [`Notifier::since`] and raises a system
/// notification for each new entry.
pub fn render_html(humans: &[String]) -> String {
    let mut options = String::new();
    for (index, login) in humans.iter().enumerate() {
        let selected = if index == 0 { " selected" } else { "" };
        options.push_str(&format!(
            "<option value=\"{login}\"{selected}>{login}</option>",
        ));
    }
    let empty_hint = if humans.is_empty() {
        "No `human` users are configured. Add `[users.<id>] role = \"human\"` to \
         receive notifications."
    } else {
        ""
    };

    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<link rel="manifest" href="/notifications.webmanifest">
<meta name="mobile-web-app-capable" content="yes">
<meta name="apple-mobile-web-app-capable" content="yes">
<meta name="apple-mobile-web-app-status-bar-style" content="default">
<meta name="apple-mobile-web-app-title" content="forge-bot">
<title>forge-bot notifications</title>
<style>
body {{ font-family: system-ui, sans-serif; margin: 2rem auto; max-width: 48rem; padding: 0 1rem; }}
.row {{ display: flex; gap: .5rem; align-items: center; flex-wrap: wrap; }}
select, button {{ font: inherit; padding: .4rem .6rem; }}
#log {{ list-style: none; padding: 0; }}
#log li {{ border: 1px solid #ddd; border-radius: .4rem; margin: .5rem 0; padding: .6rem; }}
#log a {{ font-weight: 600; }}
small {{ color: #666; }}
</style>
</head>
<body>
<h1>forge-bot notifications</h1>
<p>Pick your human account, enable browser notifications, and keep this page open.
Desktop browsers use the page directly. Mobile browsers require HTTPS and a
service worker; on iOS 16.4+ add this page to the Home Screen first, then grant
notification permission. Use <em>Send test notification</em> to confirm the
browser can raise them.</p>
<p id="hint"><em>{empty_hint}</em></p>
<div class="row">
<label>Human <select id="recipient">{options}</select></label>
<button id="enable">Enable notifications</button>
<button id="test">Send test notification</button>
<button id="refresh">Refresh</button>
</div>
<p><small id="status"></small></p>
<ul id="log"></ul>
<script>
{PAGE_SCRIPT}
</script>
</body>
</html>
"#,
    )
}

/// The service worker backing `/notifications`.
///
/// It has no fetch handler; mobile browsers only need a registered worker so
/// the page can call `ServiceWorkerRegistration.showNotification`.
pub fn service_worker_js() -> &'static str {
    r#"// Minimal service worker: mobile browsers require a registration before the
// page may call `showNotification`, which is the only notification API on
// Android and iOS.
self.addEventListener('install', function () { self.skipWaiting(); });
self.addEventListener('activate', function (event) {
  event.waitUntil(self.clients.claim());
});
"#
}

/// Web app manifest so "Add to Home Screen" installs a notification-capable
/// standalone app instead of an ordinary browser bookmark.
///
/// iOS only treats a Home Screen item as a web app when the page declares a
/// manifest with a non-`browser` display mode (or the legacy
/// `apple-mobile-web-app-capable` meta tag); otherwise the saved item reopens
/// in the default browser, where these notifications are unavailable.
pub fn manifest_json() -> &'static str {
    r##"{
  "name": "forge-bot notifications",
  "short_name": "forge-bot",
  "description": "Browser notifications for forge-bot human requests.",
  "start_url": "/notifications",
  "scope": "/",
  "display": "standalone",
  "background_color": "#ffffff",
  "theme_color": "#ffffff"
}
"##
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_filters_by_recipient_and_cursor() {
        let notifier = Notifier::new();
        let first = notifier
            .record("alice", "bot", "o/r", "http://f/o/r/issues/1", "need a key")
            .unwrap();
        let second = notifier
            .record("bob", "bot", "o/r", "http://f/o/r/issues/2", "need access")
            .unwrap();

        assert_eq!(notifier.since("alice", 0, None).len(), 1);
        assert_eq!(notifier.since("alice", 0, None)[0].id, first);
        assert_eq!(notifier.since("ALICE", 0, None).len(), 1);
        assert!(notifier.since("alice", first, None).is_empty());
        assert_eq!(notifier.since("bob", 0, None)[0].id, second);
        assert_eq!(notifier.latest_id(), second);
    }

    #[test]
    fn an_old_generation_resets_the_cursor() {
        let notifier = Notifier::new();
        notifier.record("alice", "bot", "o/r", "http://f", "need a key");
        let generation = notifier.generation();

        // The current generation keeps the cursor, so the entry is not redelivered.
        assert!(notifier.since("alice", 1, Some(&generation)).is_empty());
        // A cursor from a previous process must not hide the restart's entries.
        assert_eq!(
            notifier.since("alice", 1, Some("previous-process")).len(),
            1
        );
        // A fresh page with no generation still honours its explicit cursor.
        assert!(notifier.since("alice", 1, None).is_empty());

        // A second process mints a different generation.
        assert_ne!(generation, Notifier::new().generation());
    }

    #[test]
    fn empty_messages_are_not_recorded() {
        let notifier = Notifier::new();
        assert_eq!(
            notifier.record("alice", "bot", "o/r", "http://f", "   "),
            None
        );
        assert!(notifier.since("alice", 0, None).is_empty());
    }

    #[test]
    fn message_is_truncated_on_a_char_boundary() {
        let notifier = Notifier::new();
        let body = "é".repeat(MAX_MESSAGE);
        notifier.record("alice", "bot", "o/r", "http://f", &body);
        let message = &notifier.since("alice", 0, None)[0].message;
        assert!(message.len() <= MAX_MESSAGE + '…'.len_utf8());
        assert!(message.ends_with('…'));
    }

    #[test]
    fn ring_buffer_keeps_the_newest_notifications() {
        let notifier = Notifier::new();
        for index in 0..(CAPACITY + 5) {
            notifier.record("alice", "bot", "o/r", "http://f", &format!("n{index}"));
        }
        let all = notifier.since("alice", 0, None);
        assert_eq!(all.len(), CAPACITY);
        assert!(all.first().unwrap().message.starts_with("n5"));
    }

    #[test]
    fn page_renders_humans_and_empty_hint() {
        let html = render_html(&["alice".into(), "bob".into()]);
        assert!(html.contains("value=\"alice\""));
        assert!(html.contains("value=\"bob\""));
        assert!(html.contains("/notifications.json"));

        let empty = render_html(&[]);
        assert!(empty.contains("No `human` users are configured"));
    }

    #[test]
    fn page_uses_a_service_worker_and_per_recipient_cursors() {
        let html = render_html(&["alice".into(), "bob".into()]);
        // Mobile browsers reject `new Notification`, so the supported path is
        // `ServiceWorkerRegistration.showNotification`.
        assert!(
            html.contains(
                "navigator.serviceWorker.register('/notifications/sw.js', { scope: '/' })"
            )
        );
        assert!(html.contains("registration.showNotification"));
        // The worker must be active before the first poll so mobile browsers do
        // not fall back to the unsupported constructor on initial load.
        assert!(html.contains("setupServiceWorker().then(function () {"));
        // Cursors are per recipient and reset when the process generation
        // changes.
        assert!(html.contains("const cursors = {}"));
        assert!(html.contains("data.generation !== generation"));
        // A stale response is rejected, and an in-flight old-generation batch
        // is abandoned after each awaited display.
        assert!(html.contains("function isCurrent(seq, recipient)"));
        assert!(html.contains("seq === requestSeq"));
        assert!(html.contains("generation !== batchGeneration"));
        // Deleting (not clearing) the maps stops an in-flight old-generation
        // display from writing back into the fresh state.
        assert!(
            html.contains("for (const key of Object.keys(delivered)) { delete delivered[key]; }")
        );
        assert!(
            html.contains("for (const key of Object.keys(displaying)) { delete displaying[key]; }")
        );
        assert!(!html.contains("let after = 0"));
    }

    #[test]
    fn page_is_installable_as_a_standalone_web_app() {
        let html = render_html(&["alice".into()]);
        // Without these declarations iOS saves a plain bookmark that reopens
        // in the default browser, where notifications are unavailable.
        assert!(html.contains("rel=\"manifest\""), "{html}");
        assert!(html.contains("/notifications.webmanifest"), "{html}");
        assert!(
            html.contains("name=\"apple-mobile-web-app-capable\" content=\"yes\""),
            "{html}"
        );
        assert!(
            html.contains("name=\"mobile-web-app-capable\" content=\"yes\""),
            "{html}"
        );

        let manifest = manifest_json();
        assert!(
            manifest.contains("\"display\": \"standalone\""),
            "{manifest}"
        );
        assert!(
            manifest.contains("\"start_url\": \"/notifications\""),
            "{manifest}"
        );
        assert!(manifest.contains("\"scope\": \"/\""), "{manifest}");
    }

    #[test]
    fn page_waits_for_an_active_service_worker() {
        let html = render_html(&["alice".into()]);
        assert!(html.contains("function activeRegistration"), "{html}");
        assert!(html.contains("await activeRegistration(pending)"), "{html}");
        assert!(
            html.contains("worker.addEventListener('statechange'"),
            "{html}"
        );
        assert!(html.contains("'activated'"), "{html}");
        // The broad scope is what lets `navigator.serviceWorker.ready` resolve
        // on both `/notifications` and `/notify`.
        assert!(html.contains("{ scope: '/' }"), "{html}");
    }

    #[test]
    fn page_only_consumes_a_notification_once_it_is_displayed() {
        let html = render_html(&["alice".into()]);
        // A rejected display (for example permission not yet granted on a new
        // Android install) must not advance the cursor past the entry.
        assert!(
            html.contains("const displayed = await handle(recipient, notification)"),
            "{html}"
        );
        assert!(
            html.contains("await registration.showNotification(title, options)"),
            "{html}"
        );
        assert!(html.contains("rendered.has(notification.id)"), "{html}");
        assert!(html.contains("permission !== 'granted'"), "{html}");
        assert!(html.contains("Click \"Enable notifications\""), "{html}");
        // Granting permission re-polls immediately instead of losing the batch.
        assert!(
            html.contains("if (permission === 'granted') { poll(); }"),
            "{html}"
        );
        // Only a contiguous handled prefix may advance the cursor, so a later
        // success never consumes an earlier failure.
        assert!(
            html.contains("if (handled) { committed = notification.id; }"),
            "{html}"
        );
        // A successfully displayed entry is remembered separately so retrying
        // an earlier failure does not raise the later entry a second time, and
        // overlapping polls share one in-flight display per id.
        assert!(html.contains("const delivered = {}"), "{html}");
        assert!(html.contains("deliveredSet.has(notification.id)"), "{html}");
        assert!(html.contains("inFlight.get(notification.id)"), "{html}");
    }
}

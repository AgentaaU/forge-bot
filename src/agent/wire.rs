//! Shared line-delimited JSON process plumbing for steering adapters.
//!
//! Both the Codex app-server and Kimi wire-mode adapters keep a child process
//! alive for the duration of a run and speak a newline-delimited JSON protocol
//! over its stdin/stdout. This module owns everything that does not depend on
//! the concrete protocol:
//!
//! * spawning the child through the cgroup [`Executor`](crate::executor::Executor),
//! * writing requests and correlating responses by id,
//! * fanning server notifications out to the run loop while the request
//!   response is awaited elsewhere, and
//! * answering server-initiated requests so an unattended run cannot hang.
//!
//! Protocol adapters build the request envelopes and interpret the results.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::time::Instant;

use crate::error::{BotError, Result};
use crate::executor::{ExecSpec, Prepared};

/// How many notifications a run loop may lag behind before the oldest are
/// dropped. Agent turns emit many small deltas; a generous buffer avoids a lag
/// error during normal operation.
const EVENT_BUFFER: usize = 8192;

/// Environment variables of the caller that must not leak into a managed
/// agent (mirrors `pi_rpc`).
const SCRUBBED_ENV: &[&str] = &[
    "AAAU_SESSION_ID",
    "AAAU_EDITOR_SOCKET",
    "TERM_PROGRAM",
    "EDITOR",
    "VISUAL",
];

/// Marker embedded in an [`BotError::Agent`] reason when the installed CLI
/// does not implement the persistent protocol. Callers use it to fall back to
/// the one-shot adapter instead of treating the failure as a real agent error.
pub const UNSUPPORTED_MARKER: &str = "persistent protocol is unavailable";

/// Build the error that tells a caller to fall back to the one-shot adapter.
pub fn unsupported(name: &str, detail: impl std::fmt::Display) -> BotError {
    BotError::Agent {
        name: name.to_owned(),
        reason: format!("{UNSUPPORTED_MARKER}: {detail}"),
    }
}

/// Whether `error` asks the caller to fall back to the one-shot adapter.
pub fn is_unsupported(error: &BotError) -> bool {
    matches!(error, BotError::Agent { reason, .. } if reason.contains(UNSUPPORTED_MARKER))
}

/// Marker embedded in an [`BotError::Agent`] reason when a request was written
/// to the process but no response arrived (a timeout or a disconnect). The
/// peer may already have received and acted on it, so callers must not blindly
/// retry it through another adapter.
pub const UNCERTAIN_MARKER: &str = "delivery is uncertain";

/// Build the error that tells a caller the request may already have been
/// accepted by the agent.
pub fn uncertain(name: &str, method: &str, detail: impl std::fmt::Display) -> BotError {
    BotError::Agent {
        name: name.to_owned(),
        reason: format!("{UNCERTAIN_MARKER}: {method}: {detail}"),
    }
}

/// Whether `error` means the request may have reached the peer even though no
/// response arrived. Such a request must not be replayed automatically.
pub fn is_uncertain(error: &BotError) -> bool {
    matches!(error, BotError::Agent { reason, .. } if reason.contains(UNCERTAIN_MARKER))
}

/// A live, bidirectional JSON process.
pub struct WireProcess {
    label: String,
    /// Whether messages carry a `"jsonrpc":"2.0"` field (Kimi) or not (Codex).
    jsonrpc: bool,
    child: Mutex<Child>,
    writer: mpsc::UnboundedSender<Value>,
    next_id: AtomicU64,
    /// Request id -> response sender. The reader task removes the entry and
    /// sends the response.
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>,
    events: broadcast::Sender<Value>,
    /// Set once the reader task stops (EOF or a read error), so notification
    /// consumers stop waiting even though [`WireProcess`] keeps its broadcast
    /// sender alive for late subscribers.
    closed: watch::Sender<bool>,
    /// Set once the reader task sees EOF, so a request that arrives after the
    /// process is gone fails immediately instead of waiting forever.
    exited: Arc<AtomicBool>,
    /// Keeps the run's cgroup alive for as long as the process is.
    _cgroup: Option<crate::executor::CgroupGuard>,
}

impl WireProcess {
    /// Spawn `spec` and start its reader/writer tasks.
    ///
    /// A spawn failure is reported as "unsupported" when the program simply
    /// cannot start the persistent mode, so the caller can fall back to a
    /// one-shot invocation.
    pub fn spawn(
        label: &str,
        jsonrpc: bool,
        spec: &ExecSpec,
        executor: &crate::executor::Executor,
    ) -> Result<Arc<Self>> {
        let Prepared {
            command: mut cmd,
            cgroup,
        } = executor.command(spec).map_err(|error| BotError::Agent {
            name: label.to_owned(),
            reason: error.to_string(),
        })?;
        for key in SCRUBBED_ENV {
            cmd.env_remove(key);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = {
            let mut attempt = 0;
            loop {
                match cmd.spawn() {
                    Ok(child) => break child,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::ExecutableFileBusy
                            && attempt < 5 =>
                    {
                        attempt += 1;
                        std::thread::sleep(Duration::from_millis(10 * attempt as u64));
                    }
                    Err(error) => {
                        return Err(unsupported(
                            label,
                            format!(
                                "failed to spawn `{}` in persistent mode: {error}",
                                spec.program
                            ),
                        ));
                    }
                }
            }
        };
        let stdin = child.stdin.take().ok_or_else(|| BotError::Agent {
            name: label.to_owned(),
            reason: "persistent process stdin was not captured".into(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| BotError::Agent {
            name: label.to_owned(),
            reason: "persistent process stdout was not captured".into(),
        })?;

        let (writer, writer_rx) = mpsc::unbounded_channel::<Value>();
        tokio::spawn(write_lines(stdin, writer_rx));

        let (events, _) = broadcast::channel(EVENT_BUFFER);
        let (closed, _) = watch::channel(false);
        let pending: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let exited = Arc::new(AtomicBool::new(false));
        tokio::spawn(read_lines(
            label.to_owned(),
            stdout,
            Arc::clone(&pending),
            events.clone(),
            closed.clone(),
            Arc::clone(&exited),
        ));

        Ok(Arc::new(Self {
            label: label.to_owned(),
            jsonrpc,
            child: Mutex::new(child),
            writer,
            next_id: AtomicU64::new(1),
            pending,
            events,
            closed,
            exited,
            _cgroup: cgroup,
        }))
    }

    /// Subscribe to server notifications. The run loop reads from the returned
    /// receiver; responses are delivered to the matching [`Self::request`].
    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.events.subscribe()
    }

    /// Send a request and wait for its response.
    ///
    /// `deadline` bounds the wait; `None` waits until the process answers or
    /// exits. A JSON-RPC error object is turned into an [`BotError::Agent`].
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        deadline: Option<Instant>,
    ) -> Result<Value> {
        let id = format!("forge-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .expect("wire pending mutex poisoned")
            .insert(id.clone(), tx);
        // Insert before the liveness check so an EOF clears this entry for us
        // (dropping the sender) instead of leaving us waiting forever.
        if self.exited.load(Ordering::Acquire) || !self.is_alive() {
            self.forget(&id);
            return Err(BotError::Agent {
                name: self.label.clone(),
                reason: format!("{method} failed: process exited before answering"),
            });
        }

        let mut message = json!({ "id": id, "method": method, "params": params });
        if self.jsonrpc {
            message["jsonrpc"] = json!("2.0");
        }
        if self.writer.send(message).is_err() {
            self.pending
                .lock()
                .expect("wire pending mutex poisoned")
                .remove(&id);
            return Err(BotError::Agent {
                name: self.label.clone(),
                reason: format!("{method} could not be sent: process is gone"),
            });
        }

        let response = match deadline {
            Some(deadline) => match tokio::time::timeout_at(deadline, rx).await {
                Ok(Ok(value)) => value,
                Ok(Err(_)) => {
                    self.forget(&id);
                    return Err(uncertain(
                        &self.label,
                        method,
                        "process exited before answering",
                    ));
                }
                Err(_) => {
                    self.forget(&id);
                    return Err(uncertain(
                        &self.label,
                        method,
                        "timed out awaiting a response",
                    ));
                }
            },
            None => match rx.await {
                Ok(value) => value,
                Err(_) => {
                    self.forget(&id);
                    return Err(uncertain(
                        &self.label,
                        method,
                        "process exited before answering",
                    ));
                }
            },
        };
        self.forget(&id);

        if let Some(error) = response.get("error") {
            return Err(BotError::Agent {
                name: self.label.clone(),
                reason: format!("{method} rejected: {error}"),
            });
        }
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Answer a server-initiated request, unblocking the agent.
    pub fn respond(&self, id: &Value, result: Value) {
        let mut message = json!({ "id": id, "result": result });
        if self.jsonrpc {
            message["jsonrpc"] = json!("2.0");
        }
        let _ = self.writer.send(message);
    }

    /// Wait for the next server notification, or `None` once the reader task
    /// has stopped (EOF or a read error).
    ///
    /// A plain [`broadcast::Receiver`] cannot report that here because
    /// [`WireProcess`] keeps a sender alive so a late subscriber can still
    /// attach. Waiting on this method instead makes a process that dies
    /// mid-turn fail promptly rather than hanging a conversation forever.
    pub async fn next_event(&self, events: &mut broadcast::Receiver<Value>) -> Option<Value> {
        loop {
            let mut closed = self.closed.subscribe();
            if *closed.borrow_and_update() {
                return None;
            }
            tokio::select! {
                _ = closed.changed() => return None,
                result = events.recv() => match result {
                    Ok(value) => return Some(value),
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::debug!(skipped, "wire event stream lagged");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                },
            }
        }
    }

    /// Whether the child is still running. Once the reader task has observed
    /// EOF this is false even if the OS has not reaped the child yet.
    pub fn is_alive(&self) -> bool {
        !self.exited.load(Ordering::Acquire)
            && matches!(
                self.child
                    .lock()
                    .expect("wire child mutex poisoned")
                    .try_wait(),
                Ok(None)
            )
    }

    /// Terminate the child (and, through the cgroup guard, its descendants).
    pub fn kill(&self) {
        let _ = self
            .child
            .lock()
            .expect("wire child mutex poisoned")
            .start_kill();
    }

    fn forget(&self, id: &str) {
        self.pending
            .lock()
            .expect("wire pending mutex poisoned")
            .remove(id);
    }
}

/// Drain the writer channel into the child's stdin, one JSON line at a time.
async fn write_lines(mut stdin: ChildStdin, mut rx: mpsc::UnboundedReceiver<Value>) {
    while let Some(value) = rx.recv().await {
        let Ok(mut line) = serde_json::to_string(&value) else {
            continue;
        };
        line.push('\n');
        if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
            break;
        }
    }
}

/// Read JSON lines from the child, resolving pending requests and broadcasting
/// notifications.
async fn read_lines(
    label: String,
    stdout: ChildStdout,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>,
    events: broadcast::Sender<Value>,
    closed: watch::Sender<bool>,
    exited: Arc<AtomicBool>,
) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            tracing::debug!(%label, "ignoring non-JSON persistent-protocol line");
            continue;
        };
        // A response has an id and no method; a notification or server request
        // has a method. Server requests also carry an id and are left for the
        // run loop to answer.
        let is_response = value.get("method").is_none() && value.get("id").is_some();
        if is_response {
            let id = id_string(&value["id"]);
            if let Some(tx) = pending
                .lock()
                .expect("wire pending mutex poisoned")
                .remove(&id)
            {
                let _ = tx.send(value);
            }
            continue;
        }
        let _ = events.send(value);
    }
    // The process is gone: fail every outstanding request and stop event
    // consumers from waiting on a stream that will never produce again.
    // `send_replace` retains the shutdown value even when no `next_event`
    // receiver exists yet, which a plain `send` would not.
    exited.store(true, Ordering::Release);
    pending.lock().expect("wire pending mutex poisoned").clear();
    closed.send_replace(true);
}

/// Normalise a JSON-RPC id (string or number) to the string key we store.
fn id_string(id: &Value) -> String {
    match id {
        Value::String(value) => value.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::Executor;

    fn line(spec: ExecSpec) -> ExecSpec {
        spec
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn correlates_responses_and_broadcasts_notifications() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("echo.py");
        std::fs::write(
            &script,
            "#!/usr/bin/env python3\nimport json,sys\nfor line in sys.stdin:\n    m=json.loads(line)\n    print(json.dumps({'method':'note','params':{'n':m['id']}}),flush=True)\n    print(json.dumps({'id':m['id'],'result':{'echo':m['method']}}),flush=True)\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let spec = line(ExecSpec {
            program: script.display().to_string(),
            ..Default::default()
        });
        let process = WireProcess::spawn("echo", false, &spec, &Executor::direct()).unwrap();
        let mut events = process.subscribe();

        let result = process
            .request("hello", json!({"a": 1}), None)
            .await
            .unwrap();
        assert_eq!(result["echo"], "hello");

        let note = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(note["method"], "note");
    }

    #[tokio::test]
    async fn a_missing_program_is_reported_as_unsupported() {
        let spec = ExecSpec {
            program: "/nonexistent/definitely-not-a-program".into(),
            ..Default::default()
        };
        let error = match WireProcess::spawn("x", false, &spec, &Executor::direct()) {
            Ok(_) => panic!("a missing program should not spawn"),
            Err(error) => error,
        };
        assert!(is_unsupported(&error), "{error}");
    }

    #[cfg(unix)]
    fn write_executable(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    fn spawn_script(dir: &std::path::Path, body: &str) -> Arc<WireProcess> {
        let path = write_executable(dir, "agent.py", body);
        let spec = ExecSpec {
            program: path.display().to_string(),
            ..Default::default()
        };
        WireProcess::spawn("test", false, &spec, &Executor::direct()).unwrap()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn request_surfaces_a_protocol_error() {
        let dir = tempfile::tempdir().unwrap();
        let process = spawn_script(
            dir.path(),
            "#!/usr/bin/env python3\nimport json,sys\nfor line in sys.stdin:\n    m=json.loads(line)\n    print(json.dumps({'id':m['id'],'error':{'code':-32601,'message':'nope'}}),flush=True)\n",
        );
        let error = process
            .request("missing", json!({}), None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("nope"), "{error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn request_times_out_when_the_server_never_answers() {
        let dir = tempfile::tempdir().unwrap();
        let process = spawn_script(
            dir.path(),
            "#!/usr/bin/env python3\nimport sys\nfor _ in sys.stdin:\n    pass\n",
        );
        let deadline = Some(tokio::time::Instant::now() + std::time::Duration::from_millis(100));
        let error = process
            .request("slow", json!({}), deadline)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error}");
        assert!(is_uncertain(&error), "{error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_protocol_rejection_is_not_uncertain() {
        let dir = tempfile::tempdir().unwrap();
        let process = spawn_script(
            dir.path(),
            "#!/usr/bin/env python3\nimport json,sys\nfor line in sys.stdin:\n    m=json.loads(line)\n    print(json.dumps({'id':m['id'],'error':{'code':-32601,'message':'nope'}}),flush=True)\n",
        );
        let error = process
            .request("missing", json!({}), None)
            .await
            .unwrap_err();
        assert!(!is_uncertain(&error), "{error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn eof_closes_the_notification_stream() {
        let dir = tempfile::tempdir().unwrap();
        let process = spawn_script(dir.path(), "#!/bin/sh\nexit 0\n");
        let mut events = process.subscribe();
        let next = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            process.next_event(&mut events),
        )
        .await
        .expect("EOF should close the notification stream promptly");
        assert!(next.is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn eof_before_next_event_is_not_lost() {
        let dir = tempfile::tempdir().unwrap();
        let process = spawn_script(dir.path(), "#!/bin/sh\nexit 0\n");
        // Wait until the reader task has observed EOF *before* anyone waits on
        // the notification stream, so no receiver exists when the shutdown is
        // published.
        for _ in 0..500 {
            if process.exited.load(Ordering::Acquire) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            process.exited.load(Ordering::Acquire),
            "EOF was never observed"
        );
        let mut events = process.subscribe();
        let next = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            process.next_event(&mut events),
        )
        .await
        .expect("an EOF observed before the wait must still close the stream");
        assert!(next.is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn request_fails_when_the_process_exits_first() {
        let dir = tempfile::tempdir().unwrap();
        let process = spawn_script(dir.path(), "#!/bin/sh\nexit 0\n");
        // Give the reader task a chance to observe EOF and clear pending.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let error = process.request("gone", json!({}), None).await.unwrap_err();
        assert!(error.to_string().contains("exited"), "{error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn respond_writes_an_answer_and_liveness_tracks_the_child() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("echo.log");
        let path = write_executable(
            dir.path(),
            "echo.py",
            "#!/usr/bin/env python3\nimport os,sys\nlog=os.environ['FAKE_ECHO_LOG']\nfor line in sys.stdin:\n    open(log,'a').write(line)\n",
        );
        let spec = ExecSpec {
            program: path.display().to_string(),
            env: vec![("FAKE_ECHO_LOG".into(), log.display().to_string())],
            ..Default::default()
        };
        let process = WireProcess::spawn("echo", true, &spec, &Executor::direct()).unwrap();
        assert!(process.is_alive());
        process.respond(&json!("srv-1"), json!({"ok": true}));
        for _ in 0..200 {
            if std::fs::read_to_string(&log)
                .map(|text| text.contains("srv-1"))
                .unwrap_or(false)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let logged = std::fs::read_to_string(&log).unwrap();
        assert!(logged.contains("\"jsonrpc\":\"2.0\""), "{logged}");
        process.kill();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ignores_non_json_lines() {
        let dir = tempfile::tempdir().unwrap();
        let process = spawn_script(
            dir.path(),
            "#!/usr/bin/env python3\nimport json,sys\nprint('not json',flush=True)\nfor line in sys.stdin:\n    m=json.loads(line)\n    print(json.dumps({'id':m['id'],'result':{'ok':True}}),flush=True)\n",
        );
        let result = process.request("ping", json!({}), None).await.unwrap();
        assert_eq!(result["ok"], true);
    }

    #[test]
    fn id_string_normalises_numbers() {
        assert_eq!(id_string(&json!(7)), "7");
        assert_eq!(id_string(&json!("abc")), "abc");
    }
}

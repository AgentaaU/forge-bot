//! Bounded job scheduler.
//!
//! One conversation (repository + issue/PR) runs at most one agent at a time,
//! so a follow-up mention does not race the run it is meant to continue. The
//! `[session] workers` limit is the single global cap on concurrent agent
//! runs: it bounds how many *different* conversations run in parallel, and
//! because a conversation never runs more than one agent at a time it also
//! bounds how many agent processes (pooled or one-shot) may be in flight.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::agent::capacity::is_capacity_limited;
use crate::agent::{AgentContext, AgentOutcome, AgentRegistry, AgentRequest};
use crate::config::Config;
use crate::error::{BotError, Result};
use crate::forge::{ForgeMessage, ReplyTarget};
use crate::forge_api::ForgeApi;
use crate::mention::Mention;
use crate::policy::Policy;
use crate::session::{Job, SessionStore};
use crate::workspace::WorkspaceManager;

/// Routes jobs from webhooks to agents.
pub struct Dispatcher {
    inner: Arc<Inner>,
}

struct Inner {
    config: Arc<Config>,
    agents: Arc<AgentRegistry>,
    sessions: Arc<SessionStore>,
    api: Arc<dyn ForgeApi>,
    workspaces: WorkspaceManager,
    policy: Policy,
    tx: mpsc::Sender<Job>,
}

impl Dispatcher {
    /// Create the dispatcher and start its worker pool.
    pub fn new(
        config: Arc<Config>,
        agents: Arc<AgentRegistry>,
        sessions: Arc<SessionStore>,
        api: Arc<dyn ForgeApi>,
        policy: Policy,
    ) -> Result<Arc<Self>> {
        let capacity = config.session.queue_capacity.max(1);
        let (tx, rx) = mpsc::channel(capacity);

        let inner = Arc::new(Inner {
            workspaces: WorkspaceManager::new(&config.workspace),
            config: config.clone(),
            agents,
            sessions,
            api,
            policy,
            tx,
        });

        // Recover jobs that were queued when the process stopped.
        if config.session.recover {
            match inner.sessions.pending_jobs() {
                Ok(jobs) if !jobs.is_empty() => {
                    tracing::info!(count = jobs.len(), "recovering pending jobs");
                    for job in jobs {
                        if let Err(error) = inner.tx.try_send(job) {
                            tracing::warn!(%error, "could not requeue recovered job");
                        }
                    }
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(%error, "failed to load pending jobs"),
            }
        }

        let (done_tx, done_rx) = mpsc::unbounded_channel();
        tokio::spawn(scheduler_loop(inner.clone(), rx, done_tx, done_rx));

        Ok(Arc::new(Self { inner }))
    }

    /// Authorize and enqueue a trigger.
    pub async fn submit(
        &self,
        message: ForgeMessage,
        mention: Mention,
        agent_name: &str,
    ) -> Result<Uuid> {
        self.inner.policy.authorize(&message)?;
        // Resolve eagerly so an unknown agent fails before we persist a job.
        let _ = self.inner.agents.get(agent_name)?;

        let mut job = Job {
            id: Uuid::new_v4(),
            message,
            mention,
            agent: agent_name.to_owned(),
            created_at: Utc::now(),
            status_comment: None,
            ack_sent: false,
        };

        // Acknowledge as soon as the job is accepted. Inline review comments
        // cannot be edited through Forgejo's API, but still need an immediate
        // reply in their thread. Other forges can return an editable comment
        // id, allowing fallback notices to update the acknowledgement.
        if self.inner.config.reply.ack {
            let ack = format!("🤖 On it — running agent **{}**.", job.agent);
            if matches!(job.message.reply_target, ReplyTarget::ReviewComment(_)) {
                job.ack_sent = self.inner.reply(&job.message, &ack).await;
            } else {
                job.status_comment = self.inner.reply_tracked(&job.message, &ack).await;
            }
        }

        self.inner.sessions.save_job(&job)?;
        self.inner
            .tx
            .send(job.clone())
            .await
            .map_err(|_| BotError::Other(anyhow::anyhow!("job queue is closed")))?;

        tracing::info!(job = %job.id, agent = %job.agent, repo = %job.message.repository, "job queued");
        Ok(job.id)
    }

    /// Access to the policy (used by tests and the HTTP layer).
    pub fn policy(&self) -> &Policy {
        &self.inner.policy
    }

    /// The agent selected for mentions without an explicit adapter name.
    pub fn default_agent_name(&self) -> &str {
        self.inner.agents.default_name()
    }
}

/// Receive jobs and run them with bounded concurrency.
///
/// Jobs are scheduled per conversation: at most one run per session key, and
/// up to `[session] workers` distinct conversations at a time. A slot is held
/// for the whole job (workspace preparation, agent run and reply). Because a
/// conversation runs at most one agent at a time, that same limit is the
/// global cap on how many agent processes may be in flight, so a burst of
/// mentions can never start more agents than `workers` even across adapters.
async fn scheduler_loop(
    inner: Arc<Inner>,
    mut rx: mpsc::Receiver<Job>,
    done_tx: mpsc::UnboundedSender<String>,
    mut done_rx: mpsc::UnboundedReceiver<String>,
) {
    let concurrency = inner.config.session.workers.max(1);
    tracing::info!(concurrency, "agent worker pool started");

    // Sessions with a run in flight.
    let mut running: HashSet<String> = HashSet::new();
    // Follow-up jobs waiting for their conversation to become free.
    let mut queues: HashMap<String, VecDeque<Job>> = HashMap::new();
    // Conversations with queued work that are not running, in arrival order.
    let mut ready: VecDeque<String> = VecDeque::new();

    loop {
        tokio::select! {
            incoming = rx.recv() => {
                let Some(job) = incoming else { break };
                let key = job.session_key();
                if running.contains(&key) {
                    // The conversation is busy: queue behind the current run
                    // rather than starting a second agent for the same thread.
                    queues.entry(key).or_default().push_back(job);
                } else if running.len() < concurrency {
                    running.insert(key.clone());
                    spawn_job(&inner, job, key, &done_tx);
                } else {
                    if queues.entry(key.clone()).or_default().is_empty() {
                        ready.push_back(key.clone());
                    }
                    queues.get_mut(&key).expect("queue exists").push_back(job);
                }
            }
            Some(key) = done_rx.recv() => {
                running.remove(&key);
                // Jobs that arrived while this conversation ran still need
                // to be dispatched.
                if queues.get(&key).is_some_and(|queue| !queue.is_empty()) {
                    ready.push_back(key);
                }
            }
        }

        // Start queued conversations while capacity remains. Skipping keys
        // that are already running is a no-op defence: they never enter
        // `ready` while running.
        while running.len() < concurrency {
            let Some(position) = ready.iter().position(|key| !running.contains(key)) else {
                break;
            };
            let key = ready.remove(position).expect("position is valid");
            let Some(mut queue) = queues.remove(&key) else {
                continue;
            };
            let Some(job) = queue.pop_front() else {
                continue;
            };
            if !queue.is_empty() {
                queues.insert(key.clone(), queue);
                ready.push_back(key.clone());
            }
            running.insert(key.clone());
            spawn_job(&inner, job, key, &done_tx);
        }
    }
}

/// Run one job and report the conversation back to the scheduler when done.
fn spawn_job(inner: &Arc<Inner>, job: Job, key: String, done_tx: &mpsc::UnboundedSender<String>) {
    let inner = inner.clone();
    let done = DoneGuard {
        key: Some(key),
        tx: done_tx.clone(),
    };
    tokio::spawn(async move {
        let _done = done;
        inner.handle(job).await;
    });
}

/// Releases a conversation back to the scheduler even if its job panics, so a
/// panicking run cannot permanently occupy a worker slot.
struct DoneGuard {
    key: Option<String>,
    tx: mpsc::UnboundedSender<String>,
}

impl Drop for DoneGuard {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            let _ = self.tx.send(key);
        }
    }
}

impl Inner {
    async fn handle(&self, job: Job) {
        let key = job.session_key();

        // The acknowledgement and every fallback notice share one status
        // comment. When `submit` tracked the acknowledgement we edit it in
        // place; otherwise the notices are buffered here and posted once when
        // the calling sequence settles (issue #77). The headline names the
        // agent that actually runs, rewritten as the sequence falls back
        // (issue #80).
        let mut notices: Vec<String> = Vec::new();
        let mut running_agent = job.agent.clone();

        if let Err(error) = self.sessions.begin(&job) {
            tracing::warn!(%error, "failed to persist session start");
        }

        let credentials = self.config.credentials_for(job.message.forge);
        let workspace = match self.workspaces.prepare(&job.message, &credentials).await {
            Ok(workspace) => workspace,
            Err(error) => {
                if job.status_comment.is_none() {
                    self.flush_status(&job.message, &running_agent, &mut notices, job.ack_sent)
                        .await;
                }
                self.finish(
                    &key,
                    &job,
                    &job.agent,
                    &permission_aware_failure(&job, &error),
                )
                .await;
                return;
            }
        };

        // Resolve eagerly so an unknown agent fails before we run anything.
        if let Err(error) = self.agents.get(&job.agent) {
            if job.status_comment.is_none() {
                self.flush_status(&job.message, &running_agent, &mut notices, job.ack_sent)
                    .await;
            }
            self.finish(
                &key,
                &job,
                &job.agent,
                &AgentOutcome::failure(error.to_string(), Default::default()),
            )
            .await;
            return;
        }

        let request = AgentRequest {
            location: job.message.location.clone(),
            message: job.mention.message_or_default().to_owned(),
        };
        let context = AgentContext {
            workspace,
            forge: Some(job.message.forge),
            repository: job.message.repository.clone(),
            requester: job.message.author.clone(),
            issue_number: job.message.number,
            is_pull_request: job.message.is_pull_request,
            linked_issue: job.message.linked_issue.clone(),
            title: job.message.title.clone(),
            reply_target: job.message.reply_target.clone(),
            credentials,
        };

        // The requested agent first, then every other available agent. Agents
        // known to be at capacity are skipped entirely.
        let candidates = self.candidate_agents(&job.agent);
        if candidates.is_empty() {
            if job.status_comment.is_none() {
                self.flush_status(&job.message, &running_agent, &mut notices, job.ack_sent)
                    .await;
            }
            self.finish_no_agent(&key, &job).await;
            return;
        }

        let mut last_outcome: Option<AgentOutcome> = None;
        let mut used_agent = candidates[0].clone();
        let mut unavailable_hits = 0usize;
        // Why the previous candidate stopped, so the "switching" notice can
        // name the right reason. Capacity hits are called out as such; every
        // other failure reads as a plain failure.
        let mut previous_was_capacity = false;

        for (index, name) in candidates.iter().enumerate() {
            used_agent = name.clone();
            running_agent = name.clone();

            // Buffer the notice for this step so the whole calling sequence
            // shares one comment. Every agent skipped because it is at capacity
            // is named, so an intermediate fallback (for example `agy`) is
            // never silently passed over.
            if self.config.reply.ack {
                let notice = if index == 0 {
                    if name == &job.agent {
                        None
                    } else {
                        let skipped = self.skipped_agents(&job.agent, name);
                        Some(match skipped.as_slice() {
                            [] => format!("⚠️ Running agent **{name}** instead."),
                            [only] => format!(
                                "⚠️ Agent **{only}** is at capacity; running agent **{name}** instead."
                            ),
                            many => format!(
                                "⚠️ Agents {} are at capacity; running agent **{name}** instead.",
                                agent_list(many)
                            ),
                        })
                    }
                } else {
                    let previous = &candidates[index - 1];
                    let skipped = self.skipped_agents(previous, name);
                    Some(if previous_was_capacity {
                        // `skipped` starts with `previous`, because a capacity
                        // hit marks it unavailable before the next candidate is
                        // chosen.
                        match skipped.as_slice() {
                            [] | [_] => format!(
                                "⚠️ Agent **{previous}** hit a capacity limit; switching to **{name}**."
                            ),
                            many => format!(
                                "⚠️ Agents {} hit a capacity limit; switching to **{name}**.",
                                agent_list(many)
                            ),
                        }
                    } else if skipped.is_empty() {
                        format!("⚠️ Agent **{previous}** failed; switching to **{name}**.")
                    } else {
                        format!(
                            "⚠️ Agent **{previous}** failed; {} at capacity; switching to **{name}**.",
                            match skipped.as_slice() {
                                [only] => format!("agent **{only}** is"),
                                many => format!("agents {} are", agent_list(many)),
                            }
                        )
                    })
                };
                if let Some(notice) = notice {
                    notices.push(notice);
                    // When the acknowledgement is a tracked comment, edit it
                    // instead of posting another one. `running_agent` is the
                    // candidate about to run, so the headline names it.
                    if let Some(id) = &job.status_comment {
                        self.sync_status(&job.message, id, &running_agent, &notices)
                            .await;
                    }
                }
            }

            let agent = match self.agents.get(name) {
                Ok(agent) => agent,
                Err(error) => {
                    last_outcome =
                        Some(AgentOutcome::failure(error.to_string(), Default::default()));
                    break;
                }
            };

            let cooldown = Duration::from_secs(self.config.capacity.cooldown_secs.max(1));

            let outcome = match agent.run(&request, &context).await {
                Ok(outcome) => outcome,
                Err(error) if error.is_agent_unavailable() => {
                    // The adapter cannot start at all (missing binary, wrong
                    // command, ...). Skip it like a capacity hit so the next
                    // configured agent gets a chance.
                    self.agents.mark_unavailable(name, cooldown);
                    unavailable_hits += 1;
                    tracing::warn!(job = %job.id, agent = %name, %error, "agent cannot be started");
                    last_outcome = Some(permission_aware_failure(&job, &error));
                    previous_was_capacity = false;
                    continue;
                }
                Err(error) => permission_aware_failure(&job, &error),
            };

            let capacity_limited = !outcome.success
                && is_capacity_limited(&outcome.summary, &self.config.capacity.markers);

            if capacity_limited {
                self.agents.mark_unavailable(name, cooldown);
                unavailable_hits += 1;
                tracing::warn!(job = %job.id, agent = %name, "agent hit a capacity limit");
                last_outcome = Some(outcome);
                previous_was_capacity = true;
                continue;
            }

            if !outcome.success && self.config.capacity.fallback {
                // The agent never processed the message (non-zero exit, spawn
                // error, ...). Hand the job to the next candidate exactly like
                // a capacity hit, so a broken adapter cannot leave the thread
                // unanswered while a working agent is available.
                tracing::warn!(job = %job.id, agent = %name, "agent failed; trying the next candidate");
                last_outcome = Some(outcome);
                previous_was_capacity = false;
                continue;
            }

            last_outcome = Some(outcome);
            break;
        }

        // Post buffered notices when the forge could not edit the acknowledgement.
        if job.status_comment.is_none() {
            self.flush_status(&job.message, &running_agent, &mut notices, job.ack_sent)
                .await;
        }

        let Some(outcome) = last_outcome else {
            self.finish_no_agent(&key, &job).await;
            return;
        };

        // Every candidate we tried was unavailable (capacity or could not
        // start).
        // When fallback is enabled this means nothing is available right now,
        // so say so explicitly instead of blaming one agent. With fallback
        // disabled the caller asked us not to look further, so report the run
        // itself.
        if self.config.capacity.fallback && unavailable_hits == candidates.len() {
            self.finish_no_agent(&key, &job).await;
        } else {
            self.finish(&key, &job, &used_agent, &outcome).await;
        }
    }

    /// Agents to try for a job, in order.
    fn candidate_agents(&self, requested: &str) -> Vec<String> {
        let mut candidates = Vec::new();
        if self.agents.is_available(requested) {
            candidates.push(requested.to_owned());
        }
        if self.config.capacity.fallback {
            for name in self.agents.available_names() {
                if name != requested {
                    candidates.push(name);
                }
            }
        }
        candidates
    }

    /// Agents passed over between two fallback steps because they are at
    /// capacity.
    ///
    /// `from` is the agent being left (the requested agent before the first
    /// candidate, otherwise the previously tried one) and `to` is the next
    /// candidate that will run. The result follows the configured preference
    /// order and includes `from` when it is unavailable, so a switch notice can
    /// name every agent the fallback skipped instead of omitting one (for
    /// example `agy`) and leaving the operator to guess why.
    ///
    /// Naming every skipped agent keeps each notice accurate; the notices are
    /// collected and posted as a single status comment (issue #77).
    fn skipped_agents(&self, from: &str, to: &str) -> Vec<String> {
        let order = self.agents.ordered_names();
        let mut skipped = Vec::new();
        if !self.agents.is_available(from) {
            skipped.push(from.to_owned());
        }
        if let (Some(start), Some(end)) = (
            order.iter().position(|name| name == from),
            order.iter().position(|name| name == to),
        ) && end > start
        {
            for name in &order[start + 1..end] {
                if !self.agents.is_available(name) {
                    skipped.push(name.clone());
                }
            }
        }
        skipped
    }

    /// Append the buffered notices to the tracked status comment in place,
    /// rewriting the headline to name the agent that actually runs.
    async fn sync_status(
        &self,
        message: &ForgeMessage,
        comment_id: &str,
        agent: &str,
        notices: &[String],
    ) {
        let body = format!("forge-bot: {}", status_body(agent, notices));
        if let Err(error) = self.api.update_reply(message, comment_id, &body).await {
            tracing::warn!(
                location = %message.location,
                %error,
                "failed to update the status comment"
            );
        }
    }

    /// Post the buffered status of a calling sequence as a single comment.
    ///
    /// When an inline acknowledgement was already posted, only fallback
    /// notices are sent here. Otherwise the acknowledgement and notices share
    /// one comment. The buffer is cleared so later callers do not repeat it.
    async fn flush_status(
        &self,
        message: &ForgeMessage,
        agent: &str,
        notices: &mut Vec<String>,
        ack_sent: bool,
    ) {
        if (ack_sent || !self.config.reply.ack) && notices.is_empty() {
            return;
        }
        let body = if ack_sent {
            notices.join("\n")
        } else {
            status_body(agent, notices)
        };
        notices.clear();
        self.reply(message, &body).await;
    }

    async fn finish(&self, key: &str, job: &Job, agent: &str, outcome: &AgentOutcome) {
        self.persist_outcome(key, job, agent, outcome);

        // A successful agent normally posts its own reply, so result comments
        // stay opt-in. A failed agent may never have received the message and
        // cannot reply, so failures are always surfaced; otherwise the thread
        // would go silent. This mirrors `finish_no_agent`, which is likewise
        // posted unconditionally.
        if outcome.success && !self.config.reply.result {
            return;
        }

        let status = if outcome.success {
            "✅ finished"
        } else {
            "❌ failed"
        };
        let summary = truncate(&outcome.summary, 6000);
        let body = if summary.trim().is_empty() {
            format!("🤖 Agent **{agent}** {status} in {:?}.", outcome.duration)
        } else {
            format!(
                "🤖 Agent **{agent}** {status} in {:?}.\n\n{}",
                outcome.duration, summary
            )
        };
        self.reply(&job.message, &body).await;
    }

    /// Report that no agent can take the job. This is a terminal, actionable
    /// error, so it is always posted even when result replies are disabled.
    async fn finish_no_agent(&self, key: &str, job: &Job) {
        let outcome = AgentOutcome::failure(NO_AVAILABLE_AGENT, Default::default());
        self.persist_outcome(key, job, &job.agent, &outcome);
        self.reply(&job.message, NO_AVAILABLE_AGENT).await;
    }

    fn persist_outcome(&self, key: &str, job: &Job, agent: &str, outcome: &AgentOutcome) {
        if let Err(error) = self.sessions.finish(key, job.id, agent, outcome) {
            tracing::warn!(%error, "failed to persist session result");
        }
        if let Err(error) = self.sessions.remove_job(job.id) {
            tracing::warn!(%error, "failed to remove persisted job");
        }
    }

    async fn reply(&self, message: &ForgeMessage, body: &str) -> bool {
        let reply = format!("forge-bot: {body}");
        match self.api.reply(message, &reply).await {
            Ok(()) => return true,
            Err(error) if error.is_permission_denied() => {
                tracing::warn!(
                    location = %message.location,
                    %error,
                    "cannot reply: the forge denied comment permission"
                );
            }
            Err(error) => {
                tracing::warn!(%error, location = %message.location, "failed to post comment")
            }
        }
        false
    }

    /// Post `body` and return the new comment id when the forge can edit it.
    async fn reply_tracked(&self, message: &ForgeMessage, body: &str) -> Option<String> {
        let reply = format!("forge-bot: {body}");
        match self.api.reply_tracked(message, &reply).await {
            Ok(id) => id,
            Err(error) if error.is_permission_denied() => {
                tracing::warn!(
                    location = %message.location,
                    %error,
                    "cannot reply: the forge denied comment permission"
                );
                None
            }
            Err(error) => {
                tracing::warn!(%error, location = %message.location, "failed to post comment");
                None
            }
        }
    }
}

/// Reply posted when every configured agent is at capacity.
pub const NO_AVAILABLE_AGENT: &str = "No available agent. Every configured agent has hit a quota or capacity limit; \
     please try again later.";

/// Build a failure outcome, replacing forge permission errors with a clear
/// user-facing message.
fn permission_aware_failure(job: &Job, error: &BotError) -> AgentOutcome {
    if error.is_permission_denied() {
        tracing::warn!(
            job = %job.id,
            repo = %job.message.repository,
            %error,
            "forge denied permission"
        );
        AgentOutcome::failure(
            format!(
                "🔒 Permission Deny of {}: the bot is not allowed to access `{}`. \
                 Please ask a repository owner to grant the bot access, then mention it again.",
                job.message.forge, job.message.repository
            ),
            Default::default(),
        )
    } else {
        AgentOutcome::failure(error.to_string(), Default::default())
    }
}

/// Render the shared status comment: the "on it" headline naming the agent
/// that runs, followed by every fallback notice collected so far.
fn status_body(agent: &str, notices: &[String]) -> String {
    let mut parts = Vec::with_capacity(notices.len() + 1);
    parts.push(format!("🤖 On it — running agent **{agent}**."));
    parts.extend(notices.iter().cloned());
    parts.join(" ")
}

/// Render agent names for a fallback notice, e.g. `**codex**, **agy**`.
fn agent_list(names: &[String]) -> String {
    names
        .iter()
        .map(|name| format!("**{name}**"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn truncate(input: &str, max: usize) -> String {
    if input.chars().count() <= max {
        return input.to_owned();
    }
    let truncated: String = input.chars().take(max).collect();
    format!("{truncated}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentRegistry;
    use crate::forge_api::{NoopForgeApi, RecordingForgeApi};
    use crate::location::ForgeKind;
    use url::Url;

    fn test_config(dir: &std::path::Path) -> Config {
        let mut config = Config::default();
        config.session.dir = dir.to_path_buf();
        config.session.workers = 1;
        config.workspace.enabled = false;
        config.reply.ack = false;
        config.reply.result = false;
        config
    }

    fn message(repo: &str) -> ForgeMessage {
        ForgeMessage {
            forge: ForgeKind::Forgejo,
            location: Url::parse("http://forge.local/o/r/issues/1").unwrap(),
            body: "@agent:custom go".into(),
            author: "alice".into(),
            repository: repo.into(),
            comment_id: Some(1),
            number: Some(1),
            is_pull_request: false,
            linked_issue: None,
            event: "issue_comment".into(),
            title: None,
            reply_target: Default::default(),
        }
    }

    #[tokio::test]
    async fn acknowledges_when_the_job_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config(dir.path());
        config.reply.ack = true;
        config.policy.allow_all = true;
        config.agents.overrides.insert(
            "custom".into(),
            crate::config::AgentConfig {
                command: Some("cat".into()),
                ..Default::default()
            },
        );
        let config = Arc::new(config);

        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let api = Arc::new(RecordingForgeApi::new());
        let dispatcher = Dispatcher::new(
            config.clone(),
            Arc::new(AgentRegistry::from_config(&config)),
            sessions.clone(),
            api.clone(),
            Policy::new(&config.policy),
        )
        .unwrap();

        dispatcher
            .submit(
                message("o/r"),
                Mention {
                    agent: Some("custom".into()),
                    message: "go".into(),
                },
                "custom",
            )
            .await
            .unwrap();
        wait_for_drain(&sessions).await;

        let comments = api.comments();
        assert_eq!(comments.len(), 1, "exactly one acknowledgement");
        assert!(comments[0].1.starts_with("forge-bot: 🤖 On it"));
        assert!(comments[0].1.contains("custom"));
    }

    #[tokio::test]
    async fn result_reply_has_bot_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config(dir.path());
        config.reply.result = true;
        config.policy.allow_all = true;
        config.agents.overrides.insert(
            "custom".into(),
            crate::config::AgentConfig {
                command: Some("cat".into()),
                ..Default::default()
            },
        );
        let config = Arc::new(config);
        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let api = Arc::new(RecordingForgeApi::new());
        let dispatcher = Dispatcher::new(
            config.clone(),
            Arc::new(AgentRegistry::from_config(&config)),
            sessions.clone(),
            api.clone(),
            Policy::new(&config.policy),
        )
        .unwrap();

        dispatcher
            .submit(
                message("o/r"),
                Mention {
                    agent: Some("custom".into()),
                    message: "go".into(),
                },
                "custom",
            )
            .await
            .unwrap();
        wait_for_drain(&sessions).await;

        for _ in 0..100 {
            if !api.comments().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let comments = api.comments();
        assert_eq!(comments.len(), 1);
        assert!(
            comments[0]
                .1
                .starts_with("forge-bot: 🤖 Agent **custom** ✅ finished")
        );
    }

    /// Forge API that records the reply target of every reply, so tests can
    /// assert that a mention is answered in its own thread.
    #[derive(Default)]
    struct ThreadAwareApi {
        replies: std::sync::Mutex<Vec<crate::forge::ReplyTarget>>,
    }

    #[async_trait::async_trait]
    impl ForgeApi for ThreadAwareApi {
        async fn post_comment(
            &self,
            _location: &url::Url,
            _body: &str,
        ) -> crate::error::Result<()> {
            unreachable!("thread-aware API must reply through `reply`")
        }

        async fn reply(&self, message: &ForgeMessage, _body: &str) -> crate::error::Result<()> {
            self.replies
                .lock()
                .unwrap()
                .push(message.reply_target.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn review_mention_acknowledgement_stays_in_thread() {
        use crate::forge::{ReplyTarget, ReviewCommentTarget};

        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config(dir.path());
        config.reply.ack = true;
        config.policy.allow_all = true;
        config.agents.overrides.insert(
            "custom".into(),
            crate::config::AgentConfig {
                command: Some("sh".into()),
                args: Some(vec!["-c".into(), "sleep 0.2".into()]),
                ..Default::default()
            },
        );
        let config = Arc::new(config);

        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let api = Arc::new(ThreadAwareApi::default());
        let dispatcher = Dispatcher::new(
            config.clone(),
            Arc::new(AgentRegistry::from_config(&config)),
            sessions.clone(),
            api.clone(),
            Policy::new(&config.policy),
        )
        .unwrap();

        let mut message = message("o/r");
        message.is_pull_request = true;
        message.number = Some(22);
        message.location = Url::parse("http://forge.local/o/r/pulls/22#issuecomment-1").unwrap();
        message.reply_target = ReplyTarget::ReviewComment(ReviewCommentTarget {
            review_id: 9,
            path: "src/lib.rs".into(),
            line: 4,
            extra_lines_count: 0,
        });

        dispatcher
            .submit(
                message,
                Mention {
                    agent: Some("custom".into()),
                    message: "go".into(),
                },
                "custom",
            )
            .await
            .unwrap();

        assert_eq!(
            api.replies.lock().unwrap().len(),
            1,
            "the inline acknowledgement must be posted before the agent finishes"
        );

        wait_for_drain(&sessions).await;

        let replies = api.replies.lock().unwrap().clone();
        assert_eq!(replies.len(), 1, "exactly one acknowledgement");
        assert!(matches!(
            replies[0],
            ReplyTarget::ReviewComment(ReviewCommentTarget { review_id: 9, .. })
        ));
    }

    #[tokio::test]
    async fn submit_persists_and_runs_custom_agent() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config(dir.path());
        config.agents.overrides.insert(
            "custom".into(),
            crate::config::AgentConfig {
                // `cat` echoes the prompt, so the run succeeds.
                command: Some("cat".into()),
                ..Default::default()
            },
        );
        config.policy.allow_all = true;
        let config = Arc::new(config);

        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let dispatcher = Dispatcher::new(
            config.clone(),
            Arc::new(AgentRegistry::from_config(&config)),
            sessions.clone(),
            Arc::new(NoopForgeApi),
            Policy::new(&config.policy),
        )
        .unwrap();

        let id = dispatcher
            .submit(
                message("o/r"),
                Mention {
                    agent: Some("custom".into()),
                    message: "go".into(),
                },
                "custom",
            )
            .await
            .unwrap();
        assert!(!id.is_nil());

        // Wait for the worker to finish.
        for _ in 0..100 {
            if sessions.pending_jobs().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(sessions.pending_jobs().unwrap().is_empty());
        let session = sessions.get(&SessionStore::key(&message("o/r"))).unwrap();
        assert_eq!(session.runs[0].success, Some(true));
    }

    #[test]
    fn permission_failure_is_user_facing() {
        let job = Job {
            id: Uuid::new_v4(),
            message: message("o/r"),
            mention: Mention {
                agent: None,
                message: "x".into(),
            },
            agent: "pi-rpc".into(),
            created_at: Utc::now(),
            status_comment: None,
            ack_sent: false,
        };
        let outcome = permission_aware_failure(
            &job,
            &BotError::ForgePermissionDenied("forge returned 403".into()),
        );
        assert!(!outcome.success);
        assert!(outcome.summary.contains("Permission Deny"));
        assert!(outcome.summary.contains("o/r"));
    }

    #[tokio::test]
    async fn unauthorized_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let config = Arc::new(test_config(dir.path()));
        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let dispatcher = Dispatcher::new(
            config.clone(),
            Arc::new(AgentRegistry::from_config(&config)),
            sessions,
            Arc::new(NoopForgeApi),
            Policy::new(&config.policy),
        )
        .unwrap();

        let err = dispatcher
            .submit(
                message("o/r"),
                Mention {
                    agent: None,
                    message: "go".into(),
                },
                "codex",
            )
            .await
            .unwrap_err();
        assert!(matches!(err, BotError::Unauthorized(_)));
    }

    /// Forge API that records every comment, so tests can assert on replies.
    #[derive(Default)]
    struct RecordingApi {
        comments: std::sync::Mutex<Vec<String>>,
    }

    impl RecordingApi {
        fn comments(&self) -> Vec<String> {
            self.comments.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl ForgeApi for RecordingApi {
        async fn post_comment(&self, _location: &url::Url, body: &str) -> crate::error::Result<()> {
            self.comments.lock().unwrap().push(body.to_owned());
            Ok(())
        }
    }

    /// Forge API that supports editing the tracked status comment, so tests
    /// can prove the acknowledgement is updated in place across fallbacks.
    #[derive(Default)]
    struct EditableApi {
        comments: std::sync::Mutex<Vec<String>>,
    }

    impl EditableApi {
        fn comments(&self) -> Vec<String> {
            self.comments.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl ForgeApi for EditableApi {
        async fn post_comment(&self, _location: &url::Url, body: &str) -> crate::error::Result<()> {
            self.comments.lock().unwrap().push(body.to_owned());
            Ok(())
        }

        async fn reply_tracked(
            &self,
            _message: &ForgeMessage,
            body: &str,
        ) -> crate::error::Result<Option<String>> {
            let mut comments = self.comments.lock().unwrap();
            comments.push(body.to_owned());
            Ok(Some((comments.len() - 1).to_string()))
        }

        async fn update_reply(
            &self,
            _message: &ForgeMessage,
            comment_id: &str,
            body: &str,
        ) -> crate::error::Result<()> {
            let index: usize = comment_id.parse().expect("comment id");
            let mut comments = self.comments.lock().unwrap();
            comments[index] = body.to_owned();
            Ok(())
        }
    }

    async fn wait_for_drain(sessions: &SessionStore) {
        for _ in 0..200 {
            if sessions.pending_jobs().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(sessions.pending_jobs().unwrap().is_empty());
    }

    /// Registry with every built-in disabled, so fallback order is
    /// deterministic in tests.
    fn isolated_registry(config: &Config, keep: &[&str]) -> Arc<AgentRegistry> {
        let registry = AgentRegistry::from_config(config);
        for name in registry.names() {
            if !keep.contains(&name.as_str()) {
                registry.mark_unavailable(&name, Duration::from_secs(3600));
            }
        }
        Arc::new(registry)
    }

    fn capacity_config(dir: &std::path::Path) -> Config {
        let mut config = test_config(dir);
        config.policy.allow_all = true;
        // A shell that reports a quota error and exits non-zero.
        config.agents.overrides.insert(
            "capacity-agent".into(),
            crate::config::AgentConfig {
                command: Some("sh".into()),
                args: Some(vec![
                    "-c".into(),
                    "echo 'You have hit your usage limit' >&2; exit 1".into(),
                ]),
                ..Default::default()
            },
        );
        // A shell that reports a provider overload and exits non-zero.
        config.agents.overrides.insert(
            "overloaded-agent".into(),
            crate::config::AgentConfig {
                command: Some("sh".into()),
                args: Some(vec![
                    "-c".into(),
                    "echo 'overloaded_error: The server is currently overloaded' >&2; exit 1"
                        .into(),
                ]),
                ..Default::default()
            },
        );
        // A shell that fails for an ordinary, non-capacity reason. Its output
        // carries no capacity marker, so it must still be retried on the next
        // candidate just like a capacity hit.
        config.agents.overrides.insert(
            "broken-agent".into(),
            crate::config::AgentConfig {
                command: Some("sh".into()),
                args: Some(vec![
                    "-c".into(),
                    "echo 'Error: --print took the wrong argument' >&2; exit 2".into(),
                ]),
                ..Default::default()
            },
        );
        // A shell that echoes its stdin, standing in for a healthy agent.
        config.agents.overrides.insert(
            "good-agent".into(),
            crate::config::AgentConfig {
                command: Some("cat".into()),
                ..Default::default()
            },
        );
        // Sorts between `capacity-agent` and `good-agent`, so tests can assert
        // that an intermediate agent skipped for capacity is named.
        config.agents.overrides.insert(
            "flag-agent".into(),
            crate::config::AgentConfig {
                command: Some("cat".into()),
                ..Default::default()
            },
        );
        config
    }

    #[tokio::test]
    async fn falls_back_to_another_agent_when_capacity_limited() {
        let dir = tempfile::tempdir().unwrap();
        let config = Arc::new(capacity_config(dir.path()));
        let registry = isolated_registry(&config, &["capacity-agent", "good-agent"]);
        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let api = Arc::new(RecordingApi::default());
        let dispatcher = Dispatcher::new(
            config.clone(),
            registry.clone(),
            sessions.clone(),
            api.clone(),
            Policy::new(&config.policy),
        )
        .unwrap();

        dispatcher
            .submit(
                message("o/r"),
                Mention {
                    agent: Some("capacity-agent".into()),
                    message: "go".into(),
                },
                "capacity-agent",
            )
            .await
            .unwrap();

        wait_for_drain(&sessions).await;

        let session = sessions.get(&SessionStore::key(&message("o/r"))).unwrap();
        assert_eq!(
            session.runs[0].success,
            Some(true),
            "summary: {:?}",
            session.runs[0].summary
        );
        assert_eq!(session.runs[0].agent, "good-agent");
        assert!(
            !registry.is_available("capacity-agent"),
            "capacity-limited agent should be skipped"
        );
    }

    #[tokio::test]
    async fn falls_back_when_the_provider_is_overloaded() {
        let dir = tempfile::tempdir().unwrap();
        let config = Arc::new(capacity_config(dir.path()));
        let registry = isolated_registry(&config, &["overloaded-agent", "good-agent"]);
        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let api = Arc::new(RecordingApi::default());
        let dispatcher = Dispatcher::new(
            config.clone(),
            registry.clone(),
            sessions.clone(),
            api.clone(),
            Policy::new(&config.policy),
        )
        .unwrap();

        dispatcher
            .submit(
                message("o/r"),
                Mention {
                    agent: Some("overloaded-agent".into()),
                    message: "go".into(),
                },
                "overloaded-agent",
            )
            .await
            .unwrap();

        wait_for_drain(&sessions).await;

        let session = sessions.get(&SessionStore::key(&message("o/r"))).unwrap();
        assert_eq!(
            session.runs[0].success,
            Some(true),
            "summary: {:?}",
            session.runs[0].summary
        );
        assert_eq!(session.runs[0].agent, "good-agent");
        assert!(!registry.is_available("overloaded-agent"));
    }

    #[tokio::test]
    async fn falls_back_when_an_agent_cannot_be_started() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = capacity_config(dir.path());
        // An adapter whose binary is not installed: it cannot be started.
        config.agents.overrides.insert(
            "missing-agent".into(),
            crate::config::AgentConfig {
                command: Some("definitely-not-a-real-binary-xyz".into()),
                ..Default::default()
            },
        );
        let config = Arc::new(config);
        let registry = isolated_registry(&config, &["missing-agent", "good-agent"]);
        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let api = Arc::new(RecordingApi::default());
        let dispatcher = Dispatcher::new(
            config.clone(),
            registry.clone(),
            sessions.clone(),
            api.clone(),
            Policy::new(&config.policy),
        )
        .unwrap();

        dispatcher
            .submit(
                message("o/r"),
                Mention {
                    agent: Some("missing-agent".into()),
                    message: "go".into(),
                },
                "missing-agent",
            )
            .await
            .unwrap();

        wait_for_drain(&sessions).await;

        let session = sessions.get(&SessionStore::key(&message("o/r"))).unwrap();
        assert_eq!(session.runs[0].success, Some(true));
        assert_eq!(session.runs[0].agent, "good-agent");
        assert!(!registry.is_available("missing-agent"));
    }

    #[tokio::test]
    async fn falls_back_when_an_agent_fails() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = capacity_config(dir.path());
        config.reply.ack = true;
        let config = Arc::new(config);
        let registry = isolated_registry(&config, &["broken-agent", "good-agent"]);
        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let api = Arc::new(RecordingApi::default());
        let dispatcher = Dispatcher::new(
            config.clone(),
            registry.clone(),
            sessions.clone(),
            api.clone(),
            Policy::new(&config.policy),
        )
        .unwrap();

        dispatcher
            .submit(
                message("o/r"),
                Mention {
                    agent: Some("broken-agent".into()),
                    message: "go".into(),
                },
                "broken-agent",
            )
            .await
            .unwrap();

        wait_for_drain(&sessions).await;

        let session = sessions.get(&SessionStore::key(&message("o/r"))).unwrap();
        assert_eq!(
            session.runs[0].success,
            Some(true),
            "summary: {:?}",
            session.runs[0].summary
        );
        assert_eq!(session.runs[0].agent, "good-agent");
        // An ordinary failure is not a capacity hit, so the agent stays
        // available for later jobs.
        assert!(registry.is_available("broken-agent"));
        // The hand-off is announced instead of leaving the thread silent.
        let comments = api.comments();
        assert!(
            comments.iter().any(|c| {
                c.contains("broken-agent") && c.contains("failed") && c.contains("good-agent")
            }),
            "the failure hand-off should be announced: {comments:?}"
        );
    }

    #[tokio::test]
    async fn failure_is_reported_when_fallback_is_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = capacity_config(dir.path());
        config.capacity.fallback = false;
        // Result replies stay disabled; a failure must be posted anyway.
        config.reply.result = false;
        let config = Arc::new(config);
        let registry = isolated_registry(&config, &["broken-agent"]);
        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let api = Arc::new(RecordingApi::default());
        let dispatcher = Dispatcher::new(
            config.clone(),
            registry.clone(),
            sessions.clone(),
            api.clone(),
            Policy::new(&config.policy),
        )
        .unwrap();

        dispatcher
            .submit(
                message("o/r"),
                Mention {
                    agent: Some("broken-agent".into()),
                    message: "go".into(),
                },
                "broken-agent",
            )
            .await
            .unwrap();

        wait_for_drain(&sessions).await;

        let session = sessions.get(&SessionStore::key(&message("o/r"))).unwrap();
        assert_eq!(session.runs[0].success, Some(false));

        let comments = api.comments();
        assert!(
            comments.iter().any(|c| {
                c.contains("broken-agent")
                    && c.contains("❌ failed")
                    && c.contains("wrong argument")
            }),
            "a failed run must be reported even with result replies disabled: {comments:?}"
        );
    }

    #[tokio::test]
    async fn reports_no_available_agent_when_capacity_is_exhausted() {
        let dir = tempfile::tempdir().unwrap();
        let config = Arc::new(capacity_config(dir.path()));
        let registry = isolated_registry(&config, &["capacity-agent"]);
        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let api = Arc::new(RecordingApi::default());
        let dispatcher = Dispatcher::new(
            config.clone(),
            registry.clone(),
            sessions.clone(),
            api.clone(),
            Policy::new(&config.policy),
        )
        .unwrap();

        dispatcher
            .submit(
                message("o/r"),
                Mention {
                    agent: Some("capacity-agent".into()),
                    message: "go".into(),
                },
                "capacity-agent",
            )
            .await
            .unwrap();

        wait_for_drain(&sessions).await;

        let session = sessions.get(&SessionStore::key(&message("o/r"))).unwrap();
        assert_eq!(session.runs[0].success, Some(false));
        assert!(
            session.runs[0]
                .summary
                .as_deref()
                .unwrap()
                .contains("No available agent")
        );
        assert!(
            api.comments()
                .iter()
                .any(|body| body.starts_with("forge-bot: No available agent")),
            "the bot must reply that no agent is available"
        );
    }

    #[tokio::test]
    async fn capacity_failure_is_reported_when_fallback_is_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = capacity_config(dir.path());
        config.capacity.fallback = false;
        let config = Arc::new(config);
        let registry = isolated_registry(&config, &["capacity-agent", "good-agent"]);
        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let api = Arc::new(RecordingApi::default());
        let dispatcher = Dispatcher::new(
            config.clone(),
            registry.clone(),
            sessions.clone(),
            api.clone(),
            Policy::new(&config.policy),
        )
        .unwrap();

        dispatcher
            .submit(
                message("o/r"),
                Mention {
                    agent: Some("capacity-agent".into()),
                    message: "go".into(),
                },
                "capacity-agent",
            )
            .await
            .unwrap();

        wait_for_drain(&sessions).await;

        let session = sessions.get(&SessionStore::key(&message("o/r"))).unwrap();
        // The capacity error itself is surfaced; `good-agent` was never asked.
        assert_eq!(session.runs[0].success, Some(false));
        assert_eq!(session.runs[0].agent, "capacity-agent");
        assert!(
            session.runs[0]
                .summary
                .as_deref()
                .unwrap()
                .contains("usage limit"),
            "summary: {:?}",
            session.runs[0].summary
        );
        assert!(!registry.is_available("capacity-agent"));
        assert!(registry.is_available("good-agent"));
    }

    #[tokio::test]
    async fn skips_an_agent_already_known_to_be_capacity_limited() {
        let dir = tempfile::tempdir().unwrap();
        let config = Arc::new(capacity_config(dir.path()));
        let registry = isolated_registry(&config, &["capacity-agent", "good-agent"]);
        // A previous job already exhausted the requested agent.
        registry.mark_unavailable("capacity-agent", Duration::from_secs(3600));
        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let api = Arc::new(RecordingApi::default());
        let dispatcher = Dispatcher::new(
            config.clone(),
            registry.clone(),
            sessions.clone(),
            api.clone(),
            Policy::new(&config.policy),
        )
        .unwrap();

        dispatcher
            .submit(
                message("o/r"),
                Mention {
                    agent: Some("capacity-agent".into()),
                    message: "go".into(),
                },
                "capacity-agent",
            )
            .await
            .unwrap();

        wait_for_drain(&sessions).await;

        let session = sessions.get(&SessionStore::key(&message("o/r"))).unwrap();
        assert_eq!(session.runs[0].success, Some(true));
        assert_eq!(session.runs[0].agent, "good-agent");
    }

    /// The ack is posted once by `submit` as soon as the mention is accepted;
    /// a later fallback must only add the "switching" notice, not repeat the
    /// "on it" acknowledgement.
    #[tokio::test]
    async fn ack_is_not_duplicated_when_falling_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = capacity_config(dir.path());
        config.reply.ack = true;
        let config = Arc::new(config);
        let registry = isolated_registry(&config, &["capacity-agent", "good-agent"]);
        // A previous job already exhausted the requested agent.
        registry.mark_unavailable("capacity-agent", Duration::from_secs(3600));
        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let api = Arc::new(RecordingApi::default());
        let dispatcher = Dispatcher::new(
            config.clone(),
            registry.clone(),
            sessions.clone(),
            api.clone(),
            Policy::new(&config.policy),
        )
        .unwrap();

        dispatcher
            .submit(
                message("o/r"),
                Mention {
                    agent: Some("capacity-agent".into()),
                    message: "go".into(),
                },
                "capacity-agent",
            )
            .await
            .unwrap();

        wait_for_drain(&sessions).await;

        let session = sessions.get(&SessionStore::key(&message("o/r"))).unwrap();
        assert_eq!(session.runs[0].success, Some(true));
        assert_eq!(session.runs[0].agent, "good-agent");

        let comments = api.comments();
        assert!(comments.iter().all(|body| body.starts_with("forge-bot: ")));
        assert_eq!(
            comments.iter().filter(|c| c.contains("On it")).count(),
            1,
            "the prompt acknowledgement must not be duplicated: {comments:?}"
        );
        assert!(
            comments
                .iter()
                .any(|c| c.contains("at capacity") && c.contains("instead")),
            "the fallback should be announced: {comments:?}"
        );
    }

    #[tokio::test]
    async fn inline_ack_is_immediate_and_fallback_only_posts_the_notice() {
        use crate::forge::{ReplyTarget, ReviewCommentTarget};

        let dir = tempfile::tempdir().unwrap();
        let mut config = capacity_config(dir.path());
        config.reply.ack = true;
        let config = Arc::new(config);
        let registry = isolated_registry(&config, &["capacity-agent", "good-agent"]);
        registry.mark_unavailable("capacity-agent", Duration::from_secs(3600));
        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let api = Arc::new(RecordingApi::default());
        let dispatcher = Dispatcher::new(
            config.clone(),
            registry,
            sessions.clone(),
            api.clone(),
            Policy::new(&config.policy),
        )
        .unwrap();
        let mut review = message("o/r");
        review.is_pull_request = true;
        review.location = Url::parse("http://forge.local/o/r/pulls/1#issuecomment-1").unwrap();
        review.reply_target = ReplyTarget::ReviewComment(ReviewCommentTarget {
            review_id: 9,
            path: "src/lib.rs".into(),
            line: 4,
            extra_lines_count: 0,
        });

        dispatcher
            .submit(
                review,
                Mention {
                    agent: Some("capacity-agent".into()),
                    message: "go".into(),
                },
                "capacity-agent",
            )
            .await
            .unwrap();
        assert_eq!(
            api.comments().len(),
            1,
            "acknowledgement must precede the run"
        );
        wait_for_drain(&sessions).await;

        let comments = api.comments();
        assert_eq!(
            comments.len(),
            2,
            "one acknowledgement and one fallback notice: {comments:?}"
        );
        assert!(comments[0].contains("On it"));
        assert!(comments[1].contains("good-agent") && comments[1].contains("instead"));
        assert!(!comments[1].contains("On it"));
    }

    /// Regression test for issue #78: when several fallback agents are at
    /// capacity, the notice must name every one it skips. `flag-agent` sorts
    /// between `capacity-agent` and `good-agent`, mirroring `agy` sitting
    /// between `codex` and `pi-rpc`, and must not be silently dropped.
    #[tokio::test]
    async fn capacity_notice_names_every_skipped_agent() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = capacity_config(dir.path());
        config.reply.ack = true;
        let config = Arc::new(config);
        let registry = isolated_registry(&config, &["capacity-agent", "flag-agent", "good-agent"]);
        // The requested agent and the intermediate fallback are both out.
        registry.mark_unavailable("capacity-agent", Duration::from_secs(3600));
        registry.mark_unavailable("flag-agent", Duration::from_secs(3600));
        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let api = Arc::new(RecordingApi::default());
        let dispatcher = Dispatcher::new(
            config.clone(),
            registry.clone(),
            sessions.clone(),
            api.clone(),
            Policy::new(&config.policy),
        )
        .unwrap();

        dispatcher
            .submit(
                message("o/r"),
                Mention {
                    agent: Some("capacity-agent".into()),
                    message: "go".into(),
                },
                "capacity-agent",
            )
            .await
            .unwrap();

        wait_for_drain(&sessions).await;

        let session = sessions.get(&SessionStore::key(&message("o/r"))).unwrap();
        assert_eq!(session.runs[0].agent, "good-agent");

        let comments = api.comments();
        assert!(
            comments.iter().any(|c| {
                c.contains("capacity-agent")
                    && c.contains("flag-agent")
                    && c.contains("good-agent")
                    && c.contains("instead")
            }),
            "the notice must name every skipped agent, including the middle one: {comments:?}"
        );
    }

    /// The same guarantee on a later fallback step: after the running agent
    /// hits capacity, a capacity-limited agent between it and the next healthy
    /// one is still named.
    #[tokio::test]
    async fn capacity_notice_names_skipped_agent_after_a_capacity_hit() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = capacity_config(dir.path());
        config.reply.ack = true;
        let config = Arc::new(config);
        let registry = isolated_registry(&config, &["capacity-agent", "flag-agent", "good-agent"]);
        registry.mark_unavailable("flag-agent", Duration::from_secs(3600));
        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let api = Arc::new(RecordingApi::default());
        let dispatcher = Dispatcher::new(
            config.clone(),
            registry.clone(),
            sessions.clone(),
            api.clone(),
            Policy::new(&config.policy),
        )
        .unwrap();

        dispatcher
            .submit(
                message("o/r"),
                Mention {
                    agent: Some("capacity-agent".into()),
                    message: "go".into(),
                },
                "capacity-agent",
            )
            .await
            .unwrap();

        wait_for_drain(&sessions).await;

        let session = sessions.get(&SessionStore::key(&message("o/r"))).unwrap();
        assert_eq!(session.runs[0].agent, "good-agent");

        let comments = api.comments();
        assert!(
            comments.iter().any(|c| {
                c.contains("capacity-agent")
                    && c.contains("flag-agent")
                    && c.contains("switching to **good-agent**")
            }),
            "the switch notice must name the skipped agent: {comments:?}"
        );
    }

    /// Regression test for issue #77: the acknowledgement and every fallback
    /// notice are merged into a single status comment instead of one comment
    /// per step.
    #[tokio::test]
    async fn fallback_notices_are_merged_into_one_comment() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = capacity_config(dir.path());
        config.reply.ack = true;
        let config = Arc::new(config);
        let registry =
            isolated_registry(&config, &["capacity-agent", "broken-agent", "good-agent"]);
        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let api = Arc::new(RecordingApi::default());
        let dispatcher = Dispatcher::new(
            config.clone(),
            registry.clone(),
            sessions.clone(),
            api.clone(),
            Policy::new(&config.policy),
        )
        .unwrap();

        dispatcher
            .submit(
                message("o/r"),
                Mention {
                    agent: Some("capacity-agent".into()),
                    message: "go".into(),
                },
                "capacity-agent",
            )
            .await
            .unwrap();

        wait_for_drain(&sessions).await;

        let session = sessions.get(&SessionStore::key(&message("o/r"))).unwrap();
        assert_eq!(session.runs[0].agent, "good-agent");

        let comments = api.comments();
        assert_eq!(
            comments.len(),
            1,
            "the calling sequence must be one comment: {comments:?}"
        );
        let status = &comments[0];
        assert!(status.starts_with("forge-bot: 🤖 On it"), "{status}");
        assert!(status.contains("capacity-agent"), "{status}");
        assert!(status.contains("broken-agent"), "{status}");
        assert!(status.contains("switching to **good-agent**"), "{status}");
        // Issue #80: the headline must name the agent that actually runs, not
        // the requested agent that was skipped.
        assert!(
            status.contains("running agent **good-agent**"),
            "the headline must name the final agent: {status}"
        );
        assert!(
            !status.contains("running agent **capacity-agent**"),
            "the headline must not name the skipped agent: {status}"
        );
    }

    /// With a forge that supports editing, the acknowledgement is posted on
    /// accept and every fallback notice is appended to that same comment, so
    /// the thread still gets one status comment (issue #77).
    #[tokio::test]
    async fn tracked_status_comment_is_edited_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = capacity_config(dir.path());
        config.reply.ack = true;
        let config = Arc::new(config);
        let registry =
            isolated_registry(&config, &["capacity-agent", "broken-agent", "good-agent"]);
        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let api = Arc::new(EditableApi::default());
        let dispatcher = Dispatcher::new(
            config.clone(),
            registry.clone(),
            sessions.clone(),
            api.clone(),
            Policy::new(&config.policy),
        )
        .unwrap();

        dispatcher
            .submit(
                message("o/r"),
                Mention {
                    agent: Some("capacity-agent".into()),
                    message: "go".into(),
                },
                "capacity-agent",
            )
            .await
            .unwrap();

        wait_for_drain(&sessions).await;

        let comments = api.comments();
        assert_eq!(
            comments.len(),
            1,
            "the tracked acknowledgement must be edited, not duplicated: {comments:?}"
        );
        let status = &comments[0];
        assert!(status.starts_with("forge-bot: 🤖 On it"), "{status}");
        assert!(status.contains("capacity-agent"), "{status}");
        assert!(status.contains("broken-agent"), "{status}");
        assert!(status.contains("switching to **good-agent**"), "{status}");
        // Issue #80: the tracked comment is edited as the sequence falls back,
        // so its headline also names the final agent.
        assert!(
            status.contains("running agent **good-agent**"),
            "the headline must name the final agent: {status}"
        );
        assert!(
            !status.contains("running agent **capacity-agent**"),
            "the headline must not name the skipped agent: {status}"
        );
    }

    /// The tracked acknowledgement is posted by `submit`, before the worker
    /// runs, so a busy or slow agent still does not leave the thread silent.
    #[tokio::test]
    async fn tracked_acknowledgement_is_posted_on_accept() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config(dir.path());
        config.reply.ack = true;
        config.policy.allow_all = true;
        config.agents.overrides.insert(
            "custom".into(),
            crate::config::AgentConfig {
                command: Some("cat".into()),
                ..Default::default()
            },
        );
        let config = Arc::new(config);
        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let api = Arc::new(EditableApi::default());
        let dispatcher = Dispatcher::new(
            config.clone(),
            Arc::new(AgentRegistry::from_config(&config)),
            sessions,
            api.clone(),
            Policy::new(&config.policy),
        )
        .unwrap();

        dispatcher
            .submit(
                message("o/r"),
                Mention {
                    agent: Some("custom".into()),
                    message: "go".into(),
                },
                "custom",
            )
            .await
            .unwrap();

        // `submit` awaits the tracked post, so the acknowledgement is visible
        // without waiting for the worker.
        let comments = api.comments();
        assert_eq!(comments.len(), 1, "{comments:?}");
        assert!(
            comments[0].starts_with("forge-bot: 🤖 On it"),
            "{}",
            comments[0]
        );
    }

    // --- conversation scheduling ------------------------------------------

    /// Message for a specific issue number, so tests can address distinct
    /// conversations.
    fn message_at(repo: &str, number: u64) -> ForgeMessage {
        let mut message = message(repo);
        message.number = Some(number);
        message.comment_id = Some(number as i64);
        message.location =
            Url::parse(&format!("http://forge.local/{repo}/issues/{number}")).unwrap();
        message
    }

    /// Blocking shell agent: it logs `start:<token>` for the token found in the
    /// prompt, waits for `$AGENT_RELEASE/<token>`, then logs `end:<token>`. The
    /// token lets a test hold several runs open and observe their overlap.
    const GATE_AGENT: &str = r#"
prompt=$(cat)
token=$(printf '%s' "$prompt" | grep -o 'TOKEN_[A-Z]' | head -n1)
echo "start:$token" >> "$AGENT_LOG"
while [ ! -e "$AGENT_RELEASE/$token" ]; do sleep 0.02; done
echo "end:$token" >> "$AGENT_LOG"
"#;

    fn gated_config(
        dir: &std::path::Path,
        workers: usize,
    ) -> (Config, std::path::PathBuf, std::path::PathBuf) {
        let mut config = test_config(dir);
        config.session.workers = workers;
        config.policy.allow_all = true;
        let log = dir.join("agent.log");
        let release = dir.join("release");
        std::fs::create_dir_all(&release).unwrap();
        config.agents.overrides.insert(
            "gate".into(),
            crate::config::AgentConfig {
                command: Some("sh".into()),
                args: Some(vec!["-c".into(), GATE_AGENT.trim().into()]),
                env: [
                    ("AGENT_LOG".to_string(), log.display().to_string()),
                    ("AGENT_RELEASE".to_string(), release.display().to_string()),
                ]
                .into_iter()
                .collect(),
                ..Default::default()
            },
        );
        (config, log, release)
    }

    async fn wait_for_log(log: &std::path::Path, needle: &str) {
        for _ in 0..500 {
            if std::fs::read_to_string(log)
                .map(|contents| contents.contains(needle))
                .unwrap_or(false)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!(
            "timed out waiting for {needle:?} in {}",
            std::fs::read_to_string(log).unwrap_or_default()
        );
    }

    fn started_count(log: &std::path::Path) -> usize {
        std::fs::read_to_string(log)
            .map(|contents| {
                contents
                    .lines()
                    .filter(|line| line.starts_with("start:"))
                    .count()
            })
            .unwrap_or(0)
    }

    async fn wait_for_started(log: &std::path::Path, expected: usize) {
        for _ in 0..500 {
            if started_count(log) == expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!(
            "timed out waiting for {expected} agents to start; log: {}",
            std::fs::read_to_string(log).unwrap_or_default()
        );
    }

    fn release_token(release: &std::path::Path, token: &str) {
        std::fs::write(release.join(token), b"").unwrap();
    }

    fn gated_dispatcher(
        dir: &std::path::Path,
        workers: usize,
    ) -> (
        Arc<Dispatcher>,
        Arc<SessionStore>,
        std::path::PathBuf,
        std::path::PathBuf,
    ) {
        let (config, log, release) = gated_config(dir, workers);
        let config = Arc::new(config);
        let registry = isolated_registry(&config, &["gate"]);
        let sessions = Arc::new(SessionStore::open(dir).unwrap());
        let dispatcher = Dispatcher::new(
            config.clone(),
            registry,
            sessions.clone(),
            Arc::new(NoopForgeApi),
            Policy::new(&config.policy),
        )
        .unwrap();
        (dispatcher, sessions, log, release)
    }

    async fn submit_token(dispatcher: &Dispatcher, repo: &str, number: u64, token: &str) {
        dispatcher
            .submit(
                message_at(repo, number),
                Mention {
                    agent: Some("gate".into()),
                    message: token.into(),
                },
                "gate",
            )
            .await
            .unwrap();
    }

    /// Comments on different issues must not be serialized behind each other
    /// (issue #35).
    #[tokio::test]
    async fn different_conversations_run_in_parallel() {
        let dir = tempfile::tempdir().unwrap();
        let (dispatcher, sessions, log, release) = gated_dispatcher(dir.path(), 2);

        submit_token(&dispatcher, "o/r", 11, "TOKEN_A").await;
        submit_token(&dispatcher, "o/r", 22, "TOKEN_B").await;

        // Both runs must be in flight at the same time.
        wait_for_log(&log, "start:TOKEN_A").await;
        wait_for_log(&log, "start:TOKEN_B").await;
        let contents = std::fs::read_to_string(&log).unwrap();
        assert!(
            !contents.contains("end:TOKEN_A"),
            "A must still run: {contents}"
        );
        assert!(
            !contents.contains("end:TOKEN_B"),
            "B must still run: {contents}"
        );

        release_token(&release, "TOKEN_A");
        release_token(&release, "TOKEN_B");
        wait_for_drain(&sessions).await;
    }

    /// `[session] workers` is the single cap shared by every adapter: a burst
    /// of mentions on distinct conversations can never run more agent
    /// processes than the configured number of workers (issue #45).
    #[tokio::test]
    async fn workers_bound_all_agents() {
        let dir = tempfile::tempdir().unwrap();
        // Two conversations may run at once; the third must wait for a slot.
        let (config, log, release) = gated_config(dir.path(), 2);
        let config = Arc::new(config);
        let registry = isolated_registry(&config, &["gate"]);
        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let dispatcher = Dispatcher::new(
            config.clone(),
            registry,
            sessions.clone(),
            Arc::new(NoopForgeApi),
            Policy::new(&config.policy),
        )
        .unwrap();

        for (number, token) in [(11, "TOKEN_A"), (22, "TOKEN_B"), (33, "TOKEN_C")] {
            submit_token(&dispatcher, "o/r", number, token).await;
        }

        // Exactly two agents may be in flight; a third would exceed the cap.
        wait_for_started(&log, 2).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            started_count(&log),
            2,
            "a third agent must wait for a free slot: {}",
            std::fs::read_to_string(&log).unwrap()
        );

        // Freeing one slot lets the next queued agent start.
        let first = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("start:").map(str::to_owned))
            .expect("one agent started");
        release_token(&release, &first);
        wait_for_started(&log, 3).await;

        for token in ["TOKEN_A", "TOKEN_B", "TOKEN_C"] {
            release_token(&release, token);
        }
        wait_for_drain(&sessions).await;
    }

    /// Two mentions in the same conversation must not run at the same time.
    #[tokio::test]
    async fn same_conversation_runs_are_serialized() {
        let dir = tempfile::tempdir().unwrap();
        // Spare capacity: the conversation must still serialize itself.
        let (dispatcher, sessions, log, release) = gated_dispatcher(dir.path(), 2);

        submit_token(&dispatcher, "o/r", 7, "TOKEN_A").await;
        wait_for_log(&log, "start:TOKEN_A").await;
        submit_token(&dispatcher, "o/r", 7, "TOKEN_B").await;

        // Give the scheduler a chance to (incorrectly) start the follow-up.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let contents = std::fs::read_to_string(&log).unwrap();
        assert!(
            !contents.contains("start:TOKEN_B"),
            "the follow-up must wait for the current run: {contents}"
        );

        release_token(&release, "TOKEN_A");
        wait_for_log(&log, "end:TOKEN_A").await;
        wait_for_log(&log, "start:TOKEN_B").await;
        release_token(&release, "TOKEN_B");
        wait_for_drain(&sessions).await;

        let contents = std::fs::read_to_string(&log).unwrap();
        let first_start = contents.find("start:TOKEN_A").unwrap();
        let first_end = contents.find("end:TOKEN_A").unwrap();
        let second_start = contents.find("start:TOKEN_B").unwrap();
        assert!(
            first_start < first_end && first_end < second_start,
            "runs must be ordered and not overlap: {contents}"
        );
    }

    /// Jobs recovered from disk after a restart must respect the same
    /// per-conversation serialization as live mentions: a crashed process that
    /// left two unfinished jobs for one thread must not run them at once
    /// (issue #36).
    #[tokio::test]
    async fn recovered_jobs_for_one_conversation_run_one_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let (config, log, release) = gated_config(dir.path(), 2);
        let config = Arc::new(config);
        let registry = isolated_registry(&config, &["gate"]);
        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());

        // Leave two unfinished jobs for the same conversation behind, as a
        // restart with a job still in flight would.
        for (index, token) in ["TOKEN_A", "TOKEN_B"].into_iter().enumerate() {
            let mut job = Job {
                id: Uuid::new_v4(),
                message: message_at("o/r", 7),
                mention: Mention {
                    agent: Some("gate".into()),
                    message: token.into(),
                },
                agent: "gate".into(),
                created_at: Utc::now() + chrono::Duration::seconds(index as i64),
                status_comment: None,
                ack_sent: false,
            };
            job.message.comment_id = Some(index as i64);
            sessions.save_job(&job).unwrap();
        }

        let dispatcher = Dispatcher::new(
            config.clone(),
            registry,
            sessions.clone(),
            Arc::new(NoopForgeApi),
            Policy::new(&config.policy),
        )
        .unwrap();

        wait_for_log(&log, "start:TOKEN_A").await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let contents = std::fs::read_to_string(&log).unwrap();
        assert!(
            !contents.contains("start:TOKEN_B"),
            "a recovered follow-up must wait for the recovered run: {contents}"
        );

        release_token(&release, "TOKEN_A");
        wait_for_log(&log, "start:TOKEN_B").await;
        release_token(&release, "TOKEN_B");
        wait_for_drain(&sessions).await;
        drop(dispatcher);
    }

    /// A follow-up queued behind a busy conversation must not starve a mention
    /// on another conversation when a worker frees up.
    #[tokio::test]
    async fn a_queued_follow_up_does_not_starve_another_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let (dispatcher, sessions, log, release) = gated_dispatcher(dir.path(), 1);

        submit_token(&dispatcher, "o/r", 7, "TOKEN_A").await;
        wait_for_log(&log, "start:TOKEN_A").await;
        // Follow-up on A's conversation, then a mention on a different issue.
        submit_token(&dispatcher, "o/r", 7, "TOKEN_B").await;
        submit_token(&dispatcher, "o/r", 8, "TOKEN_C").await;

        release_token(&release, "TOKEN_A");
        // The unrelated conversation runs next; the follow-up waits its turn.
        wait_for_log(&log, "start:TOKEN_C").await;
        let contents = std::fs::read_to_string(&log).unwrap();
        assert!(
            !contents.contains("start:TOKEN_B"),
            "the follow-up must not jump ahead of the other conversation: {contents}"
        );

        release_token(&release, "TOKEN_C");
        wait_for_log(&log, "start:TOKEN_B").await;
        release_token(&release, "TOKEN_B");
        wait_for_drain(&sessions).await;
    }
}

//! Bounded job scheduler.
//!
//! One conversation (repository + issue/PR) runs at most one agent at a time,
//! so a follow-up mention does not race the run it is meant to continue. The
//! `[session] workers` limit is the single global cap on concurrent agent
//! runs: it bounds how many *different* conversations run in parallel, and
//! because a conversation never runs more than one agent at a time it also
//! bounds how many agent processes (pooled or one-shot) may be in flight.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::agent::capacity::is_capacity_limited;
use crate::agent::{
    Agent, AgentContext, AgentOutcome, AgentRegistry, AgentRequest, UnavailableReason,
};
use crate::auto_trigger::AUTO_TRIGGER_AUTHOR;
use crate::config::Config;
use crate::error::{BotError, Result};
use crate::executor::Executor;
use crate::forge::ForgeMessage;
use crate::forge_api::{ForgeApi, HttpForgeApi};
use crate::identity::Identities;
use crate::mention::{Mention, extract_mention};
use crate::notify::{Notifier, RecentComments, delivery_key};
use crate::policy::Policy;
use crate::session::status::{self, ThreadStatus};
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
    /// Resolved user identities. At least one `[users.*]` table is required.
    identities: Arc<Identities>,
    /// Launches agents, the Pi pool, and workspace git under the right account.
    executor: Arc<Executor>,
    /// Per-user Forgejo API clients, keyed by user id, so a reply uses the
    /// addressed account's token.
    user_apis: HashMap<String, Arc<dyn ForgeApi>>,
    /// In-memory log backing `/notifications`, shared by the webhook and poll
    /// ingesters so both record the same human mentions.
    notifier: Notifier,
    /// Bounded set of delivery keys already notified, so a comment seen by both
    /// ingesters (or redelivered) notifies a human only once.
    notified: Mutex<RecentComments>,
    tx: mpsc::Sender<Job>,
    /// Conversation key -> agent currently running for it, so a follow-up can
    /// be delivered into the live run instead of queueing a second one.
    running: Mutex<HashMap<String, RunningAgent>>,
}

/// The agent currently running a conversation, with the context it was given.
struct RunningAgent {
    agent: Arc<dyn Agent>,
    context: AgentContext,
}

/// Registers a running agent in [`Inner::running`] until the guard drops, even
/// if the job panics.
struct RunningGuard<'a> {
    running: &'a Mutex<HashMap<String, RunningAgent>>,
    key: String,
}

impl<'a> RunningGuard<'a> {
    fn enter(
        running: &'a Mutex<HashMap<String, RunningAgent>>,
        key: String,
        agent: Arc<dyn Agent>,
        context: AgentContext,
    ) -> Self {
        running
            .lock()
            .expect("running agent mutex poisoned")
            .insert(key.clone(), RunningAgent { agent, context });
        Self { running, key }
    }
}

impl Drop for RunningGuard<'_> {
    fn drop(&mut self) {
        self.running
            .lock()
            .expect("running agent mutex poisoned")
            .remove(&self.key);
    }
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

        // Resolve identities before the pool starts so an invalid `[users.*]`
        // configuration fails fast instead of accepting webhooks it cannot
        // route.
        let identities = Arc::new(Identities::resolve(&config, &agents.names())?);

        // Build the executor and validate every host account before serving.
        // A systemd deployment with an unknown/root host_user is rejected here
        // rather than at the first mention.
        let executor = Arc::new(Executor::from_config(&config.executor)?);
        executor.validate_users(&identities)?;

        // One API client per configured user so gateway replies use that
        // user's own token. A user without a token (only the default may
        // inherit the global token) gets an unauthenticated client. A direct
        // (test) executor keeps the injected `api` for every user so tests can
        // record replies.
        let mut user_apis: HashMap<String, Arc<dyn ForgeApi>> = HashMap::new();
        if !config.executor.direct {
            let global_token = config
                .forges
                .forgejo
                .as_ref()
                .and_then(|forgejo| forgejo.token.clone());
            for user in identities.users() {
                let token = user
                    .effective_token(global_token.as_deref())
                    .map(str::to_owned);
                let api = HttpForgeApi::with_forgejo_token((*config).clone(), token)?;
                user_apis.insert(user.id.clone(), Arc::new(api));
            }
        }

        let inner = Arc::new(Inner {
            workspaces: WorkspaceManager::new(&config.workspace, executor.clone()),
            config: config.clone(),
            agents,
            sessions,
            api,
            policy,
            identities,
            executor,
            user_apis,
            notifier: Notifier::new(),
            notified: Mutex::new(RecentComments::new(1024)),
            tx,
            running: Mutex::new(HashMap::new()),
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

        // Thread status is rebuilt from the persisted sessions, so an
        // always-on process would otherwise accumulate one record per thread
        // forever and eventually run out of memory (issue #119). Drop the
        // idle threads past the retention window now, then keep doing so in
        // the background.
        if inner.config.session.retention_secs > 0 {
            if let Err(error) = evict_stale_sessions(&inner) {
                tracing::warn!(%error, "failed to evict stale thread status at startup");
            }
            tokio::spawn(eviction_loop(inner.clone()));
        }

        Ok(Arc::new(Self { inner }))
    }

    /// Authorize and enqueue a trigger.
    pub async fn submit(
        &self,
        message: ForgeMessage,
        mention: Mention,
        agent_name: &str,
    ) -> Result<Uuid> {
        let author = (message.forge == crate::location::ForgeKind::Forgejo)
            .then(|| self.inner.identities.by_login(&message.author))
            .flatten();
        // Only a configured *agent* may hand work to a peer. A configured
        // human is a notification recipient, not an agent, so it must clear
        // the normal user allow-list like any other author.
        let author_is_agent = author.as_ref().is_some_and(|user| user.is_agent());
        if author_is_agent {
            let recipient = self.inner.identities.recipient(&message.body)?;
            if recipient.login.eq_ignore_ascii_case(&message.author) {
                return Err(BotError::Unauthorized(
                    "agent self-mention is ignored".into(),
                ));
            }
        }
        self.inner
            .policy
            .authorize_with_agent(&message, author_is_agent)?;
        self.enqueue(message, mention, agent_name).await
    }

    pub(crate) fn auto_authorized(&self, repo: &str, pr_author: &str) -> bool {
        self.inner.policy.authorize_auto(repo, pr_author).is_ok()
    }

    /// Recheck automatic policy at the queue boundary using the API's PR author.
    pub(crate) async fn submit_auto(
        &self,
        message: ForgeMessage,
        mention: Mention,
        agent_name: &str,
        pr_author: &str,
    ) -> Result<Uuid> {
        self.inner
            .policy
            .authorize_auto(&message.repository, pr_author)?;
        self.enqueue(message, mention, agent_name).await
    }

    async fn enqueue(
        &self,
        message: ForgeMessage,
        mention: Mention,
        agent_name: &str,
    ) -> Result<Uuid> {
        // Resolve eagerly so an unknown agent fails before we persist a job.
        // A typo in `--agent=` is a caller error, not something to swallow, so
        // report it in the thread with the registered names.
        let resolved = self.inner.agents.get(agent_name);
        let user = self.inner.recipient_for(&message);
        let user_id = Some(user.id.clone());
        if let Err(BotError::UnknownAgent(name)) = &resolved {
            let body = unknown_agent_message(name, &self.inner.agents.names());
            self.inner.reply(&message, &body, user_id.as_deref()).await;
        }
        resolved?;

        // A mention that lands while its conversation is busy will wait for the
        // run already in flight instead of starting, so say that rather than
        // claiming the agent is running.
        let key = SessionStore::key(&message, user_id.as_deref());
        let waiting = self.inner.thread_is_busy(&key);
        let mut job = Job {
            id: Uuid::new_v4(),
            message,
            mention,
            agent: agent_name.to_owned(),
            user_id,
            created_at: Utc::now(),
            status_comment: None,
            waiting,
        };

        // Acknowledge as soon as the job is accepted. When the forge can edit
        // the comment, remember its id so each fallback notice can be appended
        // to the same comment (issue #77). Otherwise the worker buffers the
        // notices and posts them together.
        if self.inner.config.reply.ack {
            // An automatic trigger acknowledges at reception even when the
            // conversation is busy: the point is to say why the bot is there.
            // A human mention that has to wait keeps the plain waiting text.
            let ack = if waiting && !is_auto_trigger(&job.message) {
                WAITING_ACK.to_owned()
            } else {
                running_headline(&job.message, &job.agent)
            };
            job.status_comment = self
                .inner
                .reply_tracked(&job.message, &ack, job.user_id.as_deref())
                .await;
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

    /// The resolved user identities.
    pub fn identities(&self) -> &Identities {
        &self.inner.identities
    }

    /// The in-memory log of human notifications served by `/notifications`.
    pub fn notifier(&self) -> &Notifier {
        &self.inner.notifier
    }

    /// Record a web-page notification for every configured human an agent
    /// comment mentions.
    ///
    /// Both the webhook receiver and the poller call this before their own
    /// ignore rules, so an agent's request reaches `/notifications` even when
    /// the comment is skipped for routing, and a comment seen by both
    /// ingesters is recorded once. Only comments authored by a configured
    /// agent user can notify, so a human mentioning another human does not.
    pub fn record_human_notifications(&self, message: &ForgeMessage) {
        let identities = &self.inner.identities;
        let Some(author) = identities.by_login(&message.author) else {
            return;
        };
        if !author.is_agent() {
            return;
        }
        let mentions = identities.human_mentions(&message.body);
        let base = delivery_key(message);
        for human in mentions {
            if human.login.eq_ignore_ascii_case(&message.author) {
                continue;
            }
            let key = format!("{base}:{}", human.login.to_ascii_lowercase());
            if self
                .inner
                .notified
                .lock()
                .expect("dedupe mutex poisoned")
                .insert(&key)
            {
                continue;
            }
            let id = self.inner.notifier.record(
                &human.login,
                &message.author,
                &message.repository,
                message.location.as_str(),
                &message.body,
            );
            tracing::info!(
                notification = ?id,
                recipient = %human.login,
                author = %message.author,
                "recorded human notification"
            );
        }
    }
    /// Resolve the mention and adapter for a message.
    ///
    /// A configured user is addressed by its derived login; the adapter
    /// precedence is `--agent` → user `agent` → registry default.
    ///
    /// Returns `Ok(None)` when the message does not address a configured user.
    /// A comment that addresses more than one configured user is rejected
    /// rather than routed arbitrarily.
    pub fn route(&self, message: &ForgeMessage) -> Result<Option<(Mention, String)>> {
        let matches = self.inner.identities.matching_users(&message.body);
        let recipient = match matches.as_slice() {
            [] => return Ok(None),
            [only] => *only,
            many => {
                let logins: Vec<&str> = many.iter().map(|user| user.login.as_str()).collect();
                return Err(BotError::Unauthorized(format!(
                    "comment addresses multiple agent users: {}",
                    logins.join(", ")
                )));
            }
        };
        let Some(mention) = extract_mention(&message.body, &recipient.trigger()) else {
            return Ok(None);
        };
        let agent = mention
            .agent
            .clone()
            .filter(|name| !name.is_empty())
            .or_else(|| recipient.agent.clone())
            .unwrap_or_else(|| self.inner.agents.default_name().to_owned());
        Ok(Some((mention, agent)))
    }

    /// The agent selected for mentions without an explicit adapter name.
    pub fn default_agent_name(&self) -> &str {
        self.inner.agents.default_name()
    }

    /// Adapter to use for a signed forge event that has no user mention.
    ///
    /// An automatic trigger (a failed CI run or a merge conflict) continues the
    /// conversation the thread already started, so it reuses the adapter that
    /// most recently ran for that conversation instead of jumping back to the
    /// registry default. The registry default is used for a thread with no
    /// history and when the remembered adapter is no longer registered, so a
    /// removed/renamed adapter cannot leave an automatic trigger unroutable.
    pub fn auto_trigger_agent(&self, message: &ForgeMessage) -> String {
        let user = self.inner.recipient_for(message);
        let key = SessionStore::key(message, Some(&user.id));
        self.inner
            .sessions
            .get(&key)
            .map(|session| session.agent)
            .filter(|agent| self.inner.agents.get(agent).is_ok())
            .unwrap_or_else(|| self.inner.agents.default_name().to_owned())
    }

    /// A snapshot of every conversation the bot is tracking, newest activity
    /// first within each state. Powers the `/status` page.
    pub fn threads(&self) -> Result<Vec<ThreadStatus>> {
        let sessions = self.inner.sessions.list();
        let pending = self.inner.sessions.pending_jobs()?;
        let mut threads = status::snapshot(sessions, pending);
        // Locations built from the internal API base (auto-triggers) must link
        // to the public web UI when the two differ.
        if let Some(forgejo) = self.inner.config.forges.forgejo.as_ref() {
            let web = forgejo.web_base();
            if web.trim_end_matches('/') != forgejo.base_url.trim_end_matches('/') {
                for thread in &mut threads {
                    thread.location =
                        status::public_location(&thread.location, &forgejo.base_url, web);
                }
            }
        }
        let running = self
            .inner
            .running
            .lock()
            .expect("running agent mutex poisoned");
        for thread in &mut threads {
            if let Some(entry) = running.get(&thread.key) {
                thread.model = entry
                    .context
                    .reported_model
                    .lock()
                    .expect("model mutex poisoned")
                    .clone();
            }
        }
        Ok(threads)
    }

    /// Read the persisted run history for one conversation.
    pub fn session(&self, key: &str) -> Option<crate::session::Session> {
        self.inner.sessions.get(key)
    }

    pub fn live_output(&self, job_id: Uuid) -> Option<crate::agent::LiveOutput> {
        self.inner.sessions.live_output(job_id)
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
                    // The conversation is busy: try to deliver the follow-up
                    // into the live run before queueing another one. A
                    // persistent adapter takes it mid-run; a one-shot adapter
                    // returns `None` and the job keeps its old queueing path.
                    if !inner.try_merge_follow_up(&job).await {
                        queues.entry(key).or_default().push_back(job);
                    }
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

/// How often the dispatcher looks for stale thread status. The scan is cheap
/// and running it every few minutes keeps memory close to the retention
/// window instead of waiting for an exact deadline.
const EVICTION_INTERVAL: Duration = Duration::from_secs(300);

/// Evict every idle thread older than the configured retention window.
fn evict_stale_sessions(inner: &Arc<Inner>) -> Result<usize> {
    let retention = Duration::from_secs(inner.config.session.retention_secs);
    inner.sessions.evict_stale(retention)
}

/// Periodically drop idle thread status past the retention window so a
/// long-running bot does not grow without bound.
async fn eviction_loop(inner: Arc<Inner>) {
    loop {
        tokio::time::sleep(EVICTION_INTERVAL).await;
        match evict_stale_sessions(&inner) {
            Ok(0) => {}
            Ok(count) => tracing::info!(count, "evicted stale thread status"),
            Err(error) => tracing::warn!(%error, "failed to evict stale thread status"),
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
    /// API client for `user_id`. Callers that must not fall back resolve the
    /// user first; `user_apis` is populated for every configured user.
    fn api_for(&self, user_id: Option<&str>) -> &Arc<dyn ForgeApi> {
        user_id
            .and_then(|id| self.user_apis.get(id))
            .unwrap_or(&self.api)
    }

    /// User a message is addressed to, or the default user for an automatic
    /// trigger without a mention.
    fn recipient_for(&self, message: &ForgeMessage) -> Arc<crate::identity::UserRuntime> {
        self.identities
            .recipient(&message.body)
            .ok()
            .cloned()
            .unwrap_or_else(|| self.identities.default_user().clone())
    }

    /// Resolve a persisted job's user, refusing to substitute another account
    /// when the configuration changed or the job predates per-user identity.
    fn user_for(&self, user_id: Option<&str>) -> Result<Arc<crate::identity::UserRuntime>> {
        let id = user_id.ok_or_else(|| {
            BotError::Config(
                "job has no user id; forge-bot no longer supports a single implicit account".into(),
            )
        })?;
        self.identities.get(id).cloned().ok_or_else(|| {
            BotError::Config(format!(
                "job references unknown user `{id}`; refusing to run it as another account"
            ))
        })
    }

    /// Whether `key` already has a run in flight or a mention waiting in the
    /// queue. `submit` uses this to acknowledge a queued mention honestly
    /// instead of claiming the agent is already running.
    fn thread_is_busy(&self, key: &str) -> bool {
        // A run in flight for this conversation.
        if let Some(session) = self.sessions.get(key)
            && session.is_running()
        {
            return true;
        }
        // A mention already waiting behind that run.
        if self
            .sessions
            .pending_jobs()
            .map(|jobs| jobs.iter().any(|job| job.session_key() == key))
            .unwrap_or(false)
        {
            return true;
        }
        // Every worker is busy, so even an idle conversation must wait.
        let workers = self.config.session.workers.max(1);
        self.sessions
            .list()
            .iter()
            .filter(|session| session.is_running())
            .count()
            >= workers
    }

    /// Try to deliver `job` into the run already in flight for its
    /// conversation.
    ///
    /// Returns `true` when the message was merged, so the caller must not
    /// queue the job. An adapter with no live process (or a one-shot CLI)
    /// returns `None`, keeping the existing "queue the next turn" behaviour.
    async fn try_merge_follow_up(&self, job: &Job) -> bool {
        let key = job.session_key();
        let running = {
            let agents = self.running.lock().expect("running agent mutex poisoned");
            agents
                .get(&key)
                .map(|running| (running.agent.clone(), running.context.clone()))
        };
        let Some((agent, context)) = running else {
            return false;
        };

        let request = AgentRequest {
            location: job.message.location.clone(),
            message: job.mention.message_or_default().to_owned(),
        };
        match agent.follow_up(&request, &context).await {
            Ok(Some(receipt)) => {
                tracing::info!(
                    job = %job.id,
                    agent = %agent.name(),
                    key,
                    "merged follow-up into the running agent"
                );
                self.ack_merged(job, &receipt.notice).await;
                // The follow-up is now part of the run in flight, so drop its
                // persisted job instead of replaying it after a restart.
                if let Err(error) = self.sessions.remove_job(job.id) {
                    tracing::warn!(%error, job = %job.id, "failed to remove merged follow-up job");
                }
                true
            }
            Ok(None) => false,
            Err(error) => {
                tracing::warn!(job = %job.id, %error, "could not merge follow-up; queueing it");
                false
            }
        }
    }

    /// Tell the thread that a follow-up was merged into a run already in
    /// flight, rewriting the tracked acknowledgement when the forge supports
    /// editing it.
    async fn ack_merged(&self, job: &Job, notice: &str) {
        if !self.config.reply.ack {
            return;
        }
        let notice = merged_notice(job, notice);
        match &job.status_comment {
            Some(id) => {
                let body = format!("forge-bot: {notice}");
                if let Err(error) = self
                    .api_for(job.user_id.as_deref())
                    .update_reply(&job.message, id, &body)
                    .await
                {
                    tracing::warn!(%error, "failed to update the merged follow-up status");
                }
            }
            None => {
                self.reply(&job.message, &notice, job.user_id.as_deref())
                    .await
            }
        }
    }

    async fn handle(&self, job: Job) {
        let key = job.session_key();
        let user_id = job.user_id.clone();

        // Resolve the persisted identity before doing any work. A job whose
        // user disappeared from the configuration must not silently run as
        // another account.
        let user = match self.user_for(user_id.as_deref()) {
            Ok(user) => user,
            Err(error) => {
                self.finish(
                    &key,
                    &job,
                    &job.agent,
                    &AgentOutcome::failure(error.to_string(), Default::default()),
                )
                .await;
                return;
            }
        };

        // The acknowledgement and every fallback notice share one status
        // comment. When `submit` tracked the acknowledgement we edit it in
        // place; otherwise the notices are buffered here and posted once when
        // the calling sequence settles (issue #77). The headline names the
        // agent that actually runs, rewritten as the sequence falls back
        // (issue #80).
        let mut notices: Vec<String> = Vec::new();
        let mut running_agent = job.agent.clone();

        // A queued job acknowledged itself as waiting; now that it is starting,
        // rewrite that same comment to name the agent that runs.
        if job.waiting
            && self.config.reply.ack
            && let Some(id) = &job.status_comment
        {
            self.sync_status(
                &job.message,
                id,
                &running_agent,
                &notices,
                user_id.as_deref(),
            )
            .await;
        }

        if let Err(error) = self.sessions.begin(&job) {
            tracing::warn!(%error, "failed to persist session start");
        }

        // The addressed user's own token, with the default user inheriting the
        // global token. A non-default user never falls back to the global
        // token, so a `@reviewer` run replies as the reviewer.
        let global_token = self
            .config
            .forges
            .forgejo
            .as_ref()
            .and_then(|forgejo| forgejo.token.as_deref());
        let token = user.effective_token(global_token).map(str::to_owned);
        let credentials = self
            .config
            .credentials_for(job.message.forge, token.as_deref());
        let host_user = (!user.host_user.is_empty()).then(|| user.host_user.clone());
        // A user's model belongs to its configured adapter (or the registry
        // default when omitted). Provider-specific IDs must not be injected
        // into explicit alternate adapters or automatic fallbacks.
        let model_agent = user
            .agent
            .as_deref()
            .unwrap_or_else(|| self.agents.default_name());
        let workspace = match self
            .workspaces
            .prepare(
                &job.message,
                &credentials,
                host_user.as_deref(),
                user_id.as_deref(),
            )
            .await
        {
            Ok(workspace) => workspace,
            Err(error) => {
                if job.status_comment.is_none() {
                    self.flush_status(
                        &job.message,
                        &running_agent,
                        &mut notices,
                        user_id.as_deref(),
                    )
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
                self.flush_status(
                    &job.message,
                    &running_agent,
                    &mut notices,
                    user_id.as_deref(),
                )
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
        let pull_request_author =
            if user.role == crate::config::UserRole::Reviewer && job.message.is_pull_request {
                match self
                    .api_for(user_id.as_deref())
                    .pull_request_author(&job.message)
                    .await
                {
                    Ok(author) => author,
                    Err(error) => {
                        tracing::warn!(%error, "could not resolve PR author for reviewer");
                        None
                    }
                }
            } else {
                None
            };
        let mut context = AgentContext {
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
            live_output: self.sessions.live_output(job.id),
            reported_model: Default::default(),
            executor: self.executor.clone(),
            host_user,
            user_id: user_id.clone(),
            model: None,
            reviewer: self
                .identities
                .reviewer_for(&user.login)
                .map(|reviewer| reviewer.login.clone()),
            is_reviewer: user.role == crate::config::UserRole::Reviewer,
            human: self.identities.human_for().map(|human| human.login.clone()),
            pull_request_author,
        };

        // The requested agent first, then every other available agent. The
        // requested agent is tried even if it is cooling down, so a stale
        // capacity mark cannot silence the caller's choice; only automatic
        // fallbacks skip a known-unavailable agent. The list always contains at
        // least the requested agent, so it is never empty.
        let candidates = self.candidate_agents(&job.agent);

        let mut last_outcome: Option<AgentOutcome> = None;
        let mut used_agent = candidates[0].clone();
        let mut unavailable_hits = 0usize;
        // Why the previous candidate stopped, so the "switching" notice can
        // name the right reason. `None` means it failed for an ordinary,
        // unclassified reason.
        let mut previous_reason: Option<UnavailableReason> = None;

        for (index, name) in candidates.iter().enumerate() {
            context.model = (name == model_agent)
                .then(|| user.agent_model.clone())
                .flatten();
            used_agent = name.clone();
            running_agent = name.clone();
            *context.reported_model.lock().expect("model mutex poisoned") = None;

            // The requested agent always leads the candidate list, so index 0
            // is exactly `job.agent` and needs no "instead" notice. Later
            // candidates are fallbacks: name every agent passed over with the
            // reason it left rotation (issues #77, #94).
            if self.config.reply.ack && index > 0 {
                let previous = &candidates[index - 1];
                let skipped = self.skipped_agents(previous, name);
                // `skipped` starts with `previous` when it was marked
                // unavailable. Combine it with the just-observed reason so the
                // notice names the agent that stopped and every other agent
                // passed over on the way to `name`, each with its own reason.
                let mut stopped = vec![(
                    previous.clone(),
                    previous_reason.clone().unwrap_or(UnavailableReason::Failed),
                )];
                stopped.extend(skipped.into_iter().filter(|(agent, _)| agent != previous));
                let notice = format!(
                    "⚠️ {}; switching to **{name}**.",
                    describe_unavailable(&stopped)
                );
                notices.push(notice);
                // When the acknowledgement is a tracked comment, edit it
                // instead of posting another one. `running_agent` is the
                // candidate about to run, so the headline names it.
                if let Some(id) = &job.status_comment {
                    self.sync_status(
                        &job.message,
                        id,
                        &running_agent,
                        &notices,
                        job.user_id.as_deref(),
                    )
                    .await;
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

            // A start failure has no provider response to inspect.
            let fallback_cooldown = Duration::from_secs(self.config.capacity.cooldown_secs.max(1));

            // Publish the running agent so a same-thread follow-up can be
            // delivered into it while it works. The guard clears the entry
            // when this candidate finishes (including on a fallback).
            let _running =
                RunningGuard::enter(&self.running, key.clone(), agent.clone(), context.clone());

            let outcome = match agent.run(&request, &context).await {
                Ok(outcome) => outcome,
                Err(error) if error.is_agent_unavailable() => {
                    // The adapter cannot start at all (missing binary, wrong
                    // command, ...). Skip it like a capacity hit so the next
                    // configured agent gets a chance.
                    self.agents.mark_unavailable_with_reason(
                        name,
                        fallback_cooldown,
                        UnavailableReason::StartFailed,
                    );
                    unavailable_hits += 1;
                    tracing::warn!(job = %job.id, agent = %name, %error, "agent cannot be started");
                    last_outcome = Some(permission_aware_failure(&job, &error));
                    previous_reason = Some(UnavailableReason::StartFailed);
                    continue;
                }
                Err(error) => permission_aware_failure(&job, &error),
            };

            // A successful run clears any earlier capacity/start-failure
            // cooldown, so the requested agent is not reported as unavailable
            // once it has recovered.
            if outcome.success {
                self.agents.mark_available(name);
            }

            let capacity_limited = !outcome.success
                && is_capacity_limited(&outcome.summary, &self.config.capacity.markers);

            if capacity_limited {
                let cooldown =
                    crate::agent::capacity::retry_after(&outcome.summary, chrono::Utc::now())
                        .unwrap_or(fallback_cooldown);
                self.agents.mark_unavailable_with_reason(
                    name,
                    cooldown,
                    UnavailableReason::CapacityLimit,
                );
                unavailable_hits += 1;
                tracing::warn!(job = %job.id, agent = %name, "agent hit a capacity limit");
                last_outcome = Some(outcome);
                previous_reason = Some(UnavailableReason::CapacityLimit);
                continue;
            }

            if !outcome.success && self.config.capacity.fallback {
                // The agent never processed the message (non-zero exit, spawn
                // error, ...). Hand the job to the next candidate exactly like
                // a capacity hit, so a broken adapter cannot leave the thread
                // unanswered while a working agent is available.
                tracing::warn!(job = %job.id, agent = %name, "agent failed; trying the next candidate");
                last_outcome = Some(outcome);
                previous_reason = None;
                continue;
            }

            last_outcome = Some(outcome);
            break;
        }

        // Post the buffered acknowledgement and fallback notices together when
        // the forge could not track the comment for in-place edits.
        if job.status_comment.is_none() {
            self.flush_status(
                &job.message,
                &running_agent,
                &mut notices,
                job.user_id.as_deref(),
            )
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
    ///
    /// The requested agent always leads, even when it is cooling down after an
    /// earlier capacity or start failure. The cooldown is a hint that an agent
    /// was out of rotation *when it was last called*, not a permanent verdict:
    /// the quota may have reset, so the caller's chosen (or the configured
    /// default) agent is tried again and a successful run clears the mark
    /// (issue #100). The cooldown still keeps an unavailable agent out of the
    /// automatic fallback list, so a known-bad agent is not retried as a
    /// fallback by every job.
    fn candidate_agents(&self, requested: &str) -> Vec<String> {
        let mut candidates = vec![requested.to_owned()];
        if self.config.capacity.fallback {
            for name in self.agents.available_names() {
                if name != requested {
                    candidates.push(name);
                }
            }
        }
        candidates
    }

    /// Agents passed over between two fallback steps, with the reason each
    /// one is unavailable.
    ///
    /// `from` is the previously tried agent and `to` is the next candidate
    /// that will run. The result follows the configured preference order and
    /// includes `from` when it is unavailable, so a switch notice can name
    /// every agent the fallback skipped instead of omitting one (for example
    /// `agy`) and leaving the operator to guess why.
    ///
    /// Naming every skipped agent with its reason keeps each notice accurate;
    /// the notices are collected and posted as a single status comment
    /// (issues #77, #94).
    fn skipped_agents(&self, from: &str, to: &str) -> Vec<(String, UnavailableReason)> {
        let order = self.agents.ordered_names();
        let mut skipped = Vec::new();
        if let Some(reason) = self.agents.unavailable_reason(from) {
            skipped.push((from.to_owned(), reason));
        }
        if let (Some(start), Some(end)) = (
            order.iter().position(|name| name == from),
            order.iter().position(|name| name == to),
        ) && end > start
        {
            for name in &order[start + 1..end] {
                if let Some(reason) = self.agents.unavailable_reason(name) {
                    skipped.push((name.clone(), reason));
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
        user_id: Option<&str>,
    ) {
        let body = format!("forge-bot: {}", status_body(message, agent, notices));
        if let Err(error) = self
            .api_for(user_id)
            .update_reply(message, comment_id, &body)
            .await
        {
            tracing::warn!(
                location = %message.location,
                %error,
                "failed to update the status comment"
            );
        }
    }

    /// Post the buffered status of a calling sequence as a single comment.
    ///
    /// Used when the forge cannot track the acknowledgement for in-place
    /// edits: the acknowledgement and the per-step fallback notices collected
    /// in `notices` are joined so a fallback is one comment instead of one per
    /// step (issue #77). The buffer is cleared so later callers (for example
    /// the result reply) do not repeat it.
    async fn flush_status(
        &self,
        message: &ForgeMessage,
        agent: &str,
        notices: &mut Vec<String>,
        user_id: Option<&str>,
    ) {
        if !self.config.reply.ack && notices.is_empty() {
            return;
        }
        let body = status_body(message, agent, notices);
        notices.clear();
        self.reply(message, &body, user_id).await;
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
        self.reply(&job.message, &body, job.user_id.as_deref())
            .await;
    }

    /// Report that no agent can take the job. This is a terminal, actionable
    /// error, so it is always posted even when result replies are disabled.
    /// The reply names the reason each configured agent is out of rotation
    /// when the registry knows it (issue #94).
    async fn finish_no_agent(&self, key: &str, job: &Job) {
        let message = no_available_agent_message(&self.agents.unavailable_agents());
        let outcome = AgentOutcome::failure(&message, Default::default());
        self.persist_outcome(key, job, &job.agent, &outcome);
        self.reply(&job.message, &message, job.user_id.as_deref())
            .await;
    }

    fn persist_outcome(&self, key: &str, job: &Job, agent: &str, outcome: &AgentOutcome) {
        if let Some(usage) = outcome.usage {
            match usage.hit_rate() {
                Some(hit) => tracing::info!(
                    job = %job.id,
                    agent,
                    prompt_tokens = usage.prompt_tokens,
                    cached_tokens = usage.cached_tokens,
                    hit_rate = hit,
                    "agent prompt cache"
                ),
                None => tracing::info!(
                    job = %job.id,
                    agent,
                    "agent prompt cache: no prompt tokens reported"
                ),
            }
        }
        if let Err(error) = self.sessions.finish(key, job.id, agent, outcome) {
            tracing::warn!(%error, "failed to persist session result");
        }
        if let Err(error) = self.sessions.remove_job(job.id) {
            tracing::warn!(%error, "failed to remove persisted job");
        }
    }

    async fn reply(&self, message: &ForgeMessage, body: &str, user_id: Option<&str>) {
        let reply = format!("forge-bot: {body}");
        match self.api_for(user_id).reply(message, &reply).await {
            Ok(()) => {}
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
    }

    /// Post `body` and return the new comment id when the forge can edit it.
    async fn reply_tracked(
        &self,
        message: &ForgeMessage,
        body: &str,
        user_id: Option<&str>,
    ) -> Option<String> {
        let reply = format!("forge-bot: {body}");
        match self.api_for(user_id).reply_tracked(message, &reply).await {
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

/// Acknowledgement posted when a mention must wait for the run already in
/// flight for its conversation. The worker rewrites it to the running headline
/// once the job actually starts.
pub const WAITING_ACK: &str = "🤖 Waiting for the previous job to finish.";

/// Reply posted when every configured agent is currently unavailable and the
/// registry has no reasons recorded for them.
pub const NO_AVAILABLE_AGENT: &str = "No available agent. Every configured agent is currently unavailable; \
     please try again later.";

/// Build the terminal "no available agent" reply, naming the reason each
/// configured agent is out of rotation when it is known.
fn no_available_agent_message(unavailable: &[(String, UnavailableReason)]) -> String {
    if unavailable.is_empty() {
        return NO_AVAILABLE_AGENT.to_owned();
    }
    let rendered = unavailable
        .iter()
        .map(|(name, reason)| unavailable_agent(name, reason))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "No available agent. Every configured agent is currently unavailable: {rendered}. \
         Please try again later."
    )
}

/// Build the terminal reply for an unknown `--agent=` selection. It names the
/// registered agents so the caller can retry with a valid one, and suggests the
/// closest match when the name looks like a typo.
fn unknown_agent_message(name: &str, available: &[String]) -> String {
    let list = available.join(", ");
    let suggestion = closest_agent(name, available)
        .map(|candidate| format!(" Did you mean `{candidate}`?"))
        .unwrap_or_default();
    format!("🤖 Unknown agent `{name}`.{suggestion} Available agents: {list}.")
}

/// The registered agent closest to `name` by edit distance, when that distance
/// is small enough to be a typo rather than an unrelated name.
fn closest_agent<'a>(name: &str, available: &'a [String]) -> Option<&'a str> {
    let requested = name.to_lowercase();
    let threshold = match requested.chars().count() {
        0..=3 => 1,
        4..=6 => 2,
        _ => 3,
    };
    available
        .iter()
        .map(|candidate| {
            let distance = edit_distance(&requested, &candidate.to_lowercase());
            (candidate, distance)
        })
        .filter(|(_, distance)| *distance <= threshold)
        .min_by_key(|(_, distance)| *distance)
        .map(|(candidate, _)| candidate.as_str())
}

/// Levenshtein edit distance between two strings, counted in characters.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0usize; b.len() + 1];
    for (i, a_char) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, b_char) in b.iter().enumerate() {
            let substitution = previous[j] + usize::from(a_char != b_char);
            let insertion = current[j] + 1;
            let deletion = previous[j + 1] + 1;
            current[j + 1] = substitution.min(insertion).min(deletion);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()]
}

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
fn status_body(message: &ForgeMessage, agent: &str, notices: &[String]) -> String {
    let mut parts = Vec::with_capacity(notices.len() + 1);
    parts.push(running_headline(message, agent));
    parts.extend(notices.iter().cloned());
    parts.join(" ")
}

/// The "on it" headline. A job started by a signed forge event instead of a
/// mention states what triggered it, so the thread is not left guessing why the
/// bot replied.
fn running_headline(message: &ForgeMessage, agent: &str) -> String {
    match auto_trigger_origin(message) {
        Some(origin) => {
            format!("🤖 On it — triggered by {origin}; running agent **{agent}**.")
        }
        None => format!("🤖 On it — running agent **{agent}**."),
    }
}

/// True when a signed forge event started this job instead of a human
/// mention.
fn is_auto_trigger(message: &ForgeMessage) -> bool {
    message.author == AUTO_TRIGGER_AUTHOR
}

/// Describe why a signed forge event started this job, or `None` when the job
/// came from a human mention.
fn auto_trigger_origin(message: &ForgeMessage) -> Option<&'static str> {
    if !is_auto_trigger(message) {
        return None;
    }
    Some(match message.event.as_str() {
        "action_run_failure" => "a failed CI run",
        "merge_conflict" => "a merge conflict",
        _ => "an automatic forge event",
    })
}

/// Name the follow-up that was merged into a live run so the thread can tell an
/// automatic trigger from a human reply.
fn merged_notice(job: &Job, notice: &str) -> String {
    match auto_trigger_origin(&job.message) {
        Some(origin) => format!("{notice} Triggered by {origin}."),
        None if !job.message.author.is_empty() => {
            format!("{notice} Follow-up from @{}.", job.message.author)
        }
        None => notice.to_owned(),
    }
}

/// Render one unavailable agent with its reason, e.g.
/// `**codex** (capacity limit)`.
fn unavailable_agent(name: &str, reason: &UnavailableReason) -> String {
    format!("**{name}** ({reason})")
}

/// Render a set of skipped agents and their reasons as a sentence fragment,
/// e.g. `Agents **codex** (capacity limit), **agy** (start failed) are
/// unavailable`.
fn describe_unavailable(agents: &[(String, UnavailableReason)]) -> String {
    let rendered = agents
        .iter()
        .map(|(name, reason)| unavailable_agent(name, reason))
        .collect::<Vec<_>>()
        .join(", ");
    match agents {
        [_] => format!("Agent {rendered} is unavailable"),
        _ => format!("Agents {rendered} are unavailable"),
    }
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
        // Every run belongs to a configured user now. Tests spawn fake CLIs
        // directly because the cgroup backend needs root.
        config.forges.forgejo = Some(crate::config::ForgejoConfig {
            bot_username: Some("agent".into()),
            ..Default::default()
        });
        let passwd = dir.join("passwd");
        std::fs::write(
            &passwd,
            format!(
                "agent:x:1000:1000::{}:/bin/bash\n",
                dir.join("home/agent").display()
            ),
        )
        .unwrap();
        config.executor.passwd_file = passwd;
        config.executor.direct = true;
        config.users.insert(
            "default".into(),
            crate::config::UserConfig {
                role: crate::config::UserRole::Default,
                host_user: "agent".into(),
                agent: None,
                agent_model: None,
                token: None,
            },
        );
        config
    }

    fn message(repo: &str) -> ForgeMessage {
        ForgeMessage {
            forge: ForgeKind::Forgejo,
            location: Url::parse("http://forge.local/o/r/issues/1").unwrap(),
            body: "@agent --agent=custom go".into(),
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

    /// Build a dispatcher without touching the network. The scheduler loop is
    /// started but stays idle because no job is submitted.
    fn dispatcher_for(config: Config, dir: &std::path::Path) -> Arc<Dispatcher> {
        let config = Arc::new(config);
        let sessions = Arc::new(SessionStore::open(dir).unwrap());
        Dispatcher::new(
            config.clone(),
            Arc::new(AgentRegistry::from_config(&config)),
            sessions,
            Arc::new(NoopForgeApi),
            Policy::new(&config.policy),
        )
        .unwrap()
    }

    fn explicit_config(dir: &std::path::Path) -> Config {
        let mut config = test_config(dir);
        config.users.clear();
        config.forges.forgejo = Some(crate::config::ForgejoConfig {
            bot_username: Some("shylock-bot".into()),
            ..Default::default()
        });
        // The executor resolves every explicit host_user at startup, so give
        // the test a passwd fixture with the accounts it configures.
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
        config.agents.overrides.insert(
            "custom".into(),
            crate::config::AgentConfig {
                command: Some("cat".into()),
                ..Default::default()
            },
        );
        let user = |role, host: &str, agent: Option<&str>| crate::config::UserConfig {
            role,
            host_user: host.into(),
            agent: agent.map(str::to_owned),
            agent_model: None,
            token: None,
        };
        config.users.insert(
            "shylock-bot".into(),
            user(crate::config::UserRole::Default, "agent", Some("custom")),
        );
        config.users.insert(
            "shylock-reviewer".into(),
            user(crate::config::UserRole::Reviewer, "reviewer", None),
        );
        config
    }

    #[tokio::test]
    async fn route_uses_the_default_user_login() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let dispatcher = dispatcher_for(config, dir.path());

        let msg = message("o/r");
        let (mention, agent) = dispatcher.route(&msg).unwrap().unwrap();
        assert_eq!(mention.agent.as_deref(), Some("custom"));
        assert_eq!(agent, "custom");

        let mut ignored = message("o/r");
        ignored.body = "no mention".into();
        assert!(dispatcher.route(&ignored).unwrap().is_none());
    }

    #[tokio::test]
    async fn route_selects_the_addressed_user() {
        let dir = tempfile::tempdir().unwrap();
        let config = explicit_config(dir.path());
        let dispatcher = dispatcher_for(config, dir.path());

        // The default user's configured adapter wins over the registry default.
        let mut to_default = message("o/r");
        to_default.body = "@shylock-bot do it".into();
        let (mention, agent) = dispatcher.route(&to_default).unwrap().unwrap();
        assert_eq!(mention.message, "do it");
        assert_eq!(agent, "custom");

        // An explicit `--agent` still takes precedence.
        to_default.body = "@shylock-bot --agent=codex do it".into();
        let (_, agent) = dispatcher.route(&to_default).unwrap().unwrap();
        assert_eq!(agent, "codex");

        // A reviewer with no adapter uses the registry default.
        let mut to_reviewer = message("o/r");
        to_reviewer.body = "@shylock-reviewer please review".into();
        let (mention, agent) = dispatcher.route(&to_reviewer).unwrap().unwrap();
        assert_eq!(mention.message, "please review");
        assert_eq!(agent, dispatcher.default_agent_name());

        // A body that addresses no configured user is ignored.
        let mut none = message("o/r");
        none.body = "@someone-else hi".into();
        assert!(dispatcher.route(&none).unwrap().is_none());

        // Addressing two users is rejected instead of routed arbitrarily.
        let mut both = message("o/r");
        both.body = "@shylock-bot and @shylock-reviewer".into();
        assert!(dispatcher.route(&both).is_err());
    }

    #[tokio::test]
    async fn host_user_follows_the_addressed_explicit_user() {
        let dir = tempfile::tempdir().unwrap();
        let config = explicit_config(dir.path());
        let dispatcher = dispatcher_for(config, dir.path());

        let mut to_default = message("o/r");
        to_default.body = "@shylock-bot do it".into();
        let user = dispatcher.inner.recipient_for(&to_default);
        assert_eq!(user.id, "shylock-bot");
        assert_eq!(user.host_user, "agent");

        let mut to_reviewer = message("o/r");
        to_reviewer.body = "@shylock-reviewer review".into();
        let user = dispatcher.inner.recipient_for(&to_reviewer);
        assert_eq!(user.id, "shylock-reviewer");
        assert_eq!(user.host_user, "reviewer");

        // An automatic trigger without a mention targets the default user.
        let mut auto = message("o/r");
        auto.body = "no mention here".into();
        assert_eq!(dispatcher.inner.recipient_for(&auto).id, "shylock-bot");

        // A persisted id resolves to the same user, and an unknown one is
        // rejected rather than substituted.
        assert_eq!(
            dispatcher
                .inner
                .user_for(Some("shylock-reviewer"))
                .unwrap()
                .id,
            "shylock-reviewer"
        );
        assert!(dispatcher.inner.user_for(Some("ghost")).is_err());
    }

    #[tokio::test]
    async fn user_for_requires_a_user_id() {
        let dir = tempfile::tempdir().unwrap();
        let dispatcher = dispatcher_for(test_config(dir.path()), dir.path());
        assert!(dispatcher.inner.user_for(None).is_err());
    }

    /// A signed forge event has no mention to select an adapter, so it must
    /// continue the conversation's own agent instead of snapping back to the
    /// registry default (issue #137).
    #[tokio::test]
    async fn auto_trigger_reuses_the_conversations_previous_agent() {
        let dir = tempfile::tempdir().unwrap();
        let config = explicit_config(dir.path());
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

        let mut auto = message("o/r");
        auto.body = "Investigate the failed CI run".into();
        auto.author = crate::auto_trigger::AUTO_TRIGGER_AUTHOR.into();

        // A thread with no history still uses the registry default.
        assert_eq!(
            dispatcher.auto_trigger_agent(&auto),
            dispatcher.default_agent_name()
        );

        // Seed the conversation with a run by a non-default adapter, as an
        // earlier human mention with `--agent=custom` would have.
        let user = dispatcher.inner.recipient_for(&auto);
        let job = Job {
            id: Uuid::new_v4(),
            message: auto.clone(),
            mention: Mention {
                agent: None,
                message: "resolve".into(),
            },
            agent: "custom".into(),
            user_id: Some(user.id.clone()),
            created_at: Utc::now(),
            status_comment: None,
            waiting: false,
        };
        sessions.begin(&job).unwrap();

        assert_eq!(dispatcher.auto_trigger_agent(&auto), "custom");

        // A remembered adapter that is no longer registered cannot strand the
        // trigger; the default is used instead.
        let mut removed = auto.clone();
        removed.number = Some(2);
        let user = dispatcher.inner.recipient_for(&removed);
        let job = Job {
            id: Uuid::new_v4(),
            message: removed.clone(),
            mention: Mention {
                agent: None,
                message: "resolve".into(),
            },
            agent: "ghost".into(),
            user_id: Some(user.id.clone()),
            created_at: Utc::now(),
            status_comment: None,
            waiting: false,
        };
        sessions.begin(&job).unwrap();
        assert_eq!(
            dispatcher.auto_trigger_agent(&removed),
            dispatcher.default_agent_name()
        );
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
                command: Some("cat".into()),
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
        let session = sessions
            .get(&SessionStore::key(&message("o/r"), Some("default")))
            .unwrap();
        assert_eq!(session.runs[0].success, Some(true));
    }

    #[tokio::test]
    async fn unknown_agent_is_reported_in_the_thread() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config(dir.path());
        config.policy.allow_all = true;
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

        // A typo in the requested agent name is a caller error, so it must be
        // reported in the thread instead of being silently dropped.
        let error = dispatcher
            .submit(
                message("o/r"),
                Mention {
                    agent: Some("pi-rcp".into()),
                    message: "go".into(),
                },
                "pi-rcp",
            )
            .await
            .unwrap_err();
        assert!(matches!(error, BotError::UnknownAgent(ref name) if name == "pi-rcp"));

        let comments = api.comments();
        assert_eq!(comments.len(), 1, "the invalid call is reported once");
        let body = &comments[0].1;
        assert!(body.contains("Unknown agent `pi-rcp`"), "body: {body}");
        assert!(body.contains("Did you mean `pi-rpc`?"), "body: {body}");
        assert!(body.contains("Available agents:"), "body: {body}");
        assert!(body.contains("pi-rpc"), "body: {body}");
        assert!(sessions.pending_jobs().unwrap().is_empty());
        assert!(
            sessions
                .get(&SessionStore::key(&message("o/r"), Some("default")))
                .is_none()
        );
    }

    #[test]
    fn unknown_agent_message_suggests_a_typo_and_lists_agents() {
        let available: Vec<String> = ["codex", "agy", "pi-rpc", "claude", "kimi"]
            .iter()
            .map(|name| (*name).to_owned())
            .collect();

        let with_suggestion = unknown_agent_message("pi-rcp", &available);
        assert!(with_suggestion.contains("Did you mean `pi-rpc`?"));
        assert!(with_suggestion.contains("Available agents: codex, agy, pi-rpc, claude, kimi."));

        let without_suggestion = unknown_agent_message("totally-unrelated", &available);
        assert!(!without_suggestion.contains("Did you mean"));
        assert!(without_suggestion.contains("Unknown agent `totally-unrelated`."));
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
            user_id: None,
            created_at: Utc::now(),
            status_comment: None,
            waiting: false,
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

    /// Test agent that blocks in `run` until released by a follow-up, recording
    /// every injected message so tests can prove mid-run delivery.
    struct SteeringAgent {
        runs: std::sync::atomic::AtomicUsize,
        release: tokio::sync::Notify,
        steers: std::sync::Mutex<Vec<String>>,
    }

    impl SteeringAgent {
        fn new() -> Self {
            Self {
                runs: std::sync::atomic::AtomicUsize::new(0),
                release: tokio::sync::Notify::new(),
                steers: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn runs(&self) -> usize {
            self.runs.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn steers(&self) -> Vec<String> {
            self.steers.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl Agent for SteeringAgent {
        fn name(&self) -> &str {
            "steering"
        }

        async fn run(
            &self,
            _request: &AgentRequest,
            _context: &AgentContext,
        ) -> Result<AgentOutcome> {
            self.runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.release.notified().await;
            Ok(AgentOutcome::success("done", Duration::ZERO))
        }

        async fn follow_up(
            &self,
            request: &AgentRequest,
            _context: &AgentContext,
        ) -> Result<Option<crate::agent::SteerReceipt>> {
            self.steers.lock().unwrap().push(request.message.clone());
            self.release.notify_one();
            Ok(Some(crate::agent::SteerReceipt::merged()))
        }
    }

    #[tokio::test]
    async fn same_thread_follow_up_is_merged_into_the_running_agent() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config(dir.path());
        config.reply.ack = true;
        config.policy.allow_all = true;
        config.agent_sequence = vec!["steering".into()];
        let config = Arc::new(config);

        let steering = Arc::new(SteeringAgent::new());
        let mut registry = AgentRegistry::from_config(&config);
        registry.insert_for_test("steering", steering.clone());
        // Keep the fallback deterministic: only the test agent is available.
        for name in registry.names() {
            if name != "steering" {
                registry.mark_unavailable(&name, Duration::from_secs(3600));
            }
        }
        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let api = Arc::new(RecordingForgeApi::new());
        let dispatcher = Dispatcher::new(
            config.clone(),
            Arc::new(registry),
            sessions.clone(),
            api.clone(),
            Policy::new(&config.policy),
        )
        .unwrap();

        // First mention starts the run, which blocks until steered.
        dispatcher
            .submit(
                message("o/r"),
                Mention {
                    agent: Some("steering".into()),
                    message: "first".into(),
                },
                "steering",
            )
            .await
            .unwrap();
        for _ in 0..500 {
            if steering.runs() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(steering.runs(), 1);

        // Second mention in the same thread is merged into that run.
        dispatcher
            .submit(
                message("o/r"),
                Mention {
                    agent: Some("steering".into()),
                    message: "second".into(),
                },
                "steering",
            )
            .await
            .unwrap();

        wait_for_drain(&sessions).await;

        assert_eq!(
            steering.runs(),
            1,
            "the follow-up must not start a second run"
        );
        assert_eq!(steering.steers(), vec!["second".to_owned()]);
        let comments = api.comments();
        assert!(
            comments.iter().any(|(_, body)| body
                == "forge-bot: 📎 Merged into the current run. Follow-up from @alice."),
            "the thread should show the merge notice and its author: {comments:?}"
        );
    }

    /// An automatic trigger merged into a live run names its origin, so the
    /// thread can tell it apart from a human follow-up.
    #[tokio::test]
    async fn merged_auto_trigger_names_its_origin() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config(dir.path());
        config.reply.ack = true;
        config.policy.allow_all = true;
        config.agent_sequence = vec!["steering".into()];
        config.policy.auto_allowed_repos = vec!["o/r".into()];
        config.policy.auto_allowed_pr_authors = vec!["trusted".into()];
        let config = Arc::new(config);

        let steering = Arc::new(SteeringAgent::new());
        let mut registry = AgentRegistry::from_config(&config);
        registry.insert_for_test("steering", steering.clone());
        for name in registry.names() {
            if name != "steering" {
                registry.mark_unavailable(&name, Duration::from_secs(3600));
            }
        }
        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let api = Arc::new(RecordingForgeApi::new());
        let dispatcher = Dispatcher::new(
            config.clone(),
            Arc::new(registry),
            sessions.clone(),
            api.clone(),
            Policy::new(&config.policy),
        )
        .unwrap();

        // A human mention starts a run, which blocks until steered.
        dispatcher
            .submit(
                message("o/r"),
                Mention {
                    agent: Some("steering".into()),
                    message: "first".into(),
                },
                "steering",
            )
            .await
            .unwrap();
        for _ in 0..500 {
            if steering.runs() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(steering.runs(), 1);

        // An automatic merge-conflict trigger arrives while the run is live and
        // is merged into it instead of starting a second run.
        let mut auto = message_at("o/r", 1);
        auto.author = crate::auto_trigger::AUTO_TRIGGER_AUTHOR.into();
        auto.event = "merge_conflict".into();
        auto.comment_id = None;
        dispatcher
            .submit_auto(
                auto,
                Mention {
                    agent: Some("steering".into()),
                    message: "resolve".into(),
                },
                "steering",
                "trusted",
            )
            .await
            .unwrap();

        wait_for_drain(&sessions).await;

        let comments = api.comments();
        assert!(
            comments.iter().any(|(_, body)| body
                == "forge-bot: 📎 Merged into the current run. Triggered by a merge conflict."),
            "the merged acknowledgement must name the automatic trigger: {comments:?}"
        );
    }

    /// The merged acknowledgement names every automatic origin (and the author
    /// for a human follow-up).
    #[test]
    fn merged_notice_names_the_trigger() {
        let auto_job = |event: &str| Job {
            id: Uuid::new_v4(),
            message: ForgeMessage {
                forge: crate::location::ForgeKind::Forgejo,
                location: Url::parse("http://forge.local/o/r/pulls/1").unwrap(),
                body: String::new(),
                author: crate::auto_trigger::AUTO_TRIGGER_AUTHOR.into(),
                repository: "o/r".into(),
                comment_id: None,
                number: Some(1),
                is_pull_request: true,
                linked_issue: None,
                event: event.into(),
                title: None,
                reply_target: Default::default(),
            },
            mention: Mention {
                agent: None,
                message: "x".into(),
            },
            agent: "codex".into(),
            user_id: None,
            created_at: Utc::now(),
            status_comment: None,
            waiting: false,
        };
        let notice = "📎 Merged into the current run.";
        assert_eq!(
            merged_notice(&auto_job("action_run_failure"), notice),
            "📎 Merged into the current run. Triggered by a failed CI run."
        );
        assert_eq!(
            merged_notice(&auto_job("merge_conflict"), notice),
            "📎 Merged into the current run. Triggered by a merge conflict."
        );
        assert_eq!(
            merged_notice(&auto_job("future_event"), notice),
            "📎 Merged into the current run. Triggered by an automatic forge event."
        );

        let mut human = auto_job("issue_comment");
        human.message.author = "shylock".into();
        assert_eq!(
            merged_notice(&human, notice),
            "📎 Merged into the current run. Follow-up from @shylock."
        );
    }

    #[tokio::test]
    async fn non_steerable_agent_keeps_the_queueing_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config(dir.path());
        config.policy.allow_all = true;
        config.agents.overrides.insert(
            "plain".into(),
            crate::config::AgentConfig {
                command: Some("cat".into()),
                ..Default::default()
            },
        );
        let config = Arc::new(config);
        let registry = AgentRegistry::from_config(&config);
        let plain = registry.get("plain").unwrap();
        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let dispatcher = Dispatcher::new(
            config.clone(),
            Arc::new(registry),
            sessions,
            Arc::new(NoopForgeApi),
            Policy::new(&config.policy),
        )
        .unwrap();

        let job = Job {
            id: Uuid::new_v4(),
            message: message("o/r"),
            mention: Mention {
                agent: Some("plain".into()),
                message: "second".into(),
            },
            agent: "plain".into(),
            user_id: None,
            created_at: Utc::now(),
            status_comment: None,
            waiting: false,
        };
        let key = job.session_key();
        dispatcher.inner.running.lock().unwrap().insert(
            key,
            RunningAgent {
                agent: plain,
                context: AgentContext {
                    repository: "o/r".into(),
                    issue_number: Some(1),
                    ..Default::default()
                },
            },
        );

        // The one-shot adapter returns `None`, so the scheduler queues instead.
        assert!(!dispatcher.inner.try_merge_follow_up(&job).await);
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

        let session = sessions
            .get(&SessionStore::key(&message("o/r"), Some("default")))
            .unwrap();
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

    /// A fake adapter whose failed response gives a retry interval.
    struct CooldownAgent {
        response: &'static str,
    }

    #[async_trait::async_trait]
    impl Agent for CooldownAgent {
        fn name(&self) -> &str {
            "cooldown-agent"
        }

        async fn run(
            &self,
            _request: &AgentRequest,
            _context: &AgentContext,
        ) -> Result<AgentOutcome> {
            Ok(AgentOutcome::failure(self.response, Duration::ZERO))
        }
    }

    /// A provider retry hint takes precedence over the configured fallback.
    #[tokio::test]
    async fn uses_the_agent_response_cooldown() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config(dir.path());
        config.policy.allow_all = true;
        config.agent_sequence = vec!["cooldown-agent".into(), "good-agent".into()];
        // Deliberately different from the response value so the test can
        // tell which one was applied.
        config.capacity.cooldown_secs = 3600;
        let config = Arc::new(config);

        let mut registry = AgentRegistry::from_config(&config);
        registry.insert_for_test(
            "cooldown-agent",
            Arc::new(CooldownAgent {
                response: "You have hit your usage limit. Try again in 30 seconds.",
            }),
        );
        registry.insert_for_test(
            "good-agent",
            Arc::new(crate::agent::command::CommandAgent::new(
                "good-agent",
                "cat",
            )),
        );
        // Keep the fallback deterministic: only these two agents are available.
        for name in registry.names() {
            if name != "cooldown-agent" && name != "good-agent" {
                registry.mark_unavailable(&name, Duration::from_secs(3600));
            }
        }
        let registry = Arc::new(registry);
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
                    agent: Some("cooldown-agent".into()),
                    message: "go".into(),
                },
                "cooldown-agent",
            )
            .await
            .unwrap();

        wait_for_drain(&sessions).await;

        let remaining = registry
            .cooldown_remaining("cooldown-agent")
            .expect("the capacity-limited agent must be cooling down");
        assert!(
            remaining > Duration::from_secs(20) && remaining <= Duration::from_secs(30),
            "the response cooldown must be used, not the config value, got {remaining:?}"
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

        let session = sessions
            .get(&SessionStore::key(&message("o/r"), Some("default")))
            .unwrap();
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

        let session = sessions
            .get(&SessionStore::key(&message("o/r"), Some("default")))
            .unwrap();
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

        let session = sessions
            .get(&SessionStore::key(&message("o/r"), Some("default")))
            .unwrap();
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
    async fn configured_user_model_does_not_leak_into_other_adapters() {
        for configured_agent in [Some("primary"), None] {
            let dir = tempfile::tempdir().unwrap();
            let mut config = test_config(dir.path());
            config.policy.allow_all = true;
            config.agent_sequence = vec!["primary".into(), "secondary".into()];
            let user = config.users.get_mut("default").unwrap();
            user.agent = configured_agent.map(str::to_owned);
            user.agent_model = Some("primary-provider-model".into());
            for (name, exit) in [("primary", 1), ("secondary", 0)] {
                config.agents.overrides.insert(
                    name.into(),
                    crate::config::AgentConfig {
                        command: Some("sh".into()),
                        args: Some(vec![
                            "-c".into(),
                            format!("printf '%s\\n' \"$*\" > \"$ARGS_LOG\"; exit {exit}"),
                            "probe".into(),
                        ]),
                        env: [(
                            "ARGS_LOG".into(),
                            dir.path().join(name).display().to_string(),
                        )]
                        .into(),
                        ..Default::default()
                    },
                );
            }
            let config = Arc::new(config);
            let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
            let dispatcher = Dispatcher::new(
                config.clone(),
                isolated_registry(&config, &["primary", "secondary"]),
                sessions.clone(),
                Arc::new(NoopForgeApi),
                Policy::new(&config.policy),
            )
            .unwrap();
            dispatcher
                .submit(
                    message("o/r"),
                    Mention {
                        agent: Some("primary".into()),
                        message: "go".into(),
                    },
                    "primary",
                )
                .await
                .unwrap();
            wait_for_drain(&sessions).await;
            assert!(
                std::fs::read_to_string(dir.path().join("primary"))
                    .unwrap()
                    .contains("--model primary-provider-model")
            );
            assert!(
                std::fs::read_to_string(dir.path().join("secondary"))
                    .unwrap()
                    .trim()
                    .is_empty(),
                "automatic fallback must use its own model defaults"
            );
            // An explicit adapter override must also keep the other provider's
            // model out of its command line.
            dispatcher
                .submit(
                    message("o/r"),
                    Mention {
                        agent: Some("secondary".into()),
                        message: "go".into(),
                    },
                    "secondary",
                )
                .await
                .unwrap();
            wait_for_drain(&sessions).await;
            assert!(
                std::fs::read_to_string(dir.path().join("secondary"))
                    .unwrap()
                    .trim()
                    .is_empty()
            );
        }
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

        let session = sessions
            .get(&SessionStore::key(&message("o/r"), Some("default")))
            .unwrap();
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

        let session = sessions
            .get(&SessionStore::key(&message("o/r"), Some("default")))
            .unwrap();
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
        // Issue #94: the terminal reply must also explain why nothing ran.
        assert!(
            api.comments()
                .iter()
                .any(|body| body.contains("(capacity limit)")),
            "the no-agent reply must name the reason: {:?}",
            api.comments()
        );
    }

    #[test]
    fn no_available_agent_message_names_reasons_and_falls_back() {
        assert_eq!(no_available_agent_message(&[]), NO_AVAILABLE_AGENT);
        let message = no_available_agent_message(&[
            ("codex".into(), UnavailableReason::CapacityLimit),
            ("agy".into(), UnavailableReason::StartFailed),
        ]);
        assert!(message.starts_with("No available agent"), "{message}");
        assert!(message.contains("**codex** (capacity limit)"), "{message}");
        assert!(message.contains("**agy** (start failed)"), "{message}");
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

        let session = sessions
            .get(&SessionStore::key(&message("o/r"), Some("default")))
            .unwrap();
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

    /// Issue #100: an agent on cooldown must still be called when it is the
    /// requested (or default) agent, because the underlying quota may have
    /// reset. A successful probe clears the cooldown.
    #[tokio::test]
    async fn retries_the_requested_agent_even_when_cooling_down() {
        let dir = tempfile::tempdir().unwrap();
        let config = Arc::new(capacity_config(dir.path()));
        let registry = isolated_registry(&config, &["good-agent"]);
        // A previous job already marked the requested agent unavailable.
        registry.mark_unavailable("good-agent", Duration::from_secs(3600));
        assert!(!registry.is_available("good-agent"));
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
                    agent: Some("good-agent".into()),
                    message: "go".into(),
                },
                "good-agent",
            )
            .await
            .unwrap();

        wait_for_drain(&sessions).await;

        let session = sessions
            .get(&SessionStore::key(&message("o/r"), Some("default")))
            .unwrap();
        assert_eq!(session.runs[0].success, Some(true));
        assert_eq!(
            session.runs[0].agent, "good-agent",
            "the requested agent must be called even while cooling down"
        );
        assert!(
            registry.is_available("good-agent"),
            "a successful run must clear the cooldown"
        );
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

        let session = sessions
            .get(&SessionStore::key(&message("o/r"), Some("default")))
            .unwrap();
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
            comments.iter().any(|c| {
                c.contains("capacity-agent") && c.contains("switching to **good-agent**")
            }),
            "the fallback should be announced: {comments:?}"
        );
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

        let session = sessions
            .get(&SessionStore::key(&message("o/r"), Some("default")))
            .unwrap();
        assert_eq!(session.runs[0].agent, "good-agent");

        let comments = api.comments();
        assert!(
            comments.iter().any(|c| {
                c.contains("capacity-agent")
                    && c.contains("flag-agent")
                    && c.contains("good-agent")
                    && c.contains("switching to **good-agent**")
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

        let session = sessions
            .get(&SessionStore::key(&message("o/r"), Some("default")))
            .unwrap();
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

    /// Issue #94: the first fallback notice must state *why* each requested
    /// agent is unavailable, not only that it is.
    #[tokio::test]
    async fn notice_reports_the_reason_agents_are_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = capacity_config(dir.path());
        config.reply.ack = true;
        let config = Arc::new(config);
        let registry = isolated_registry(&config, &["capacity-agent", "flag-agent", "good-agent"]);
        // A previous job exhausted `capacity-agent` and could not start
        // `flag-agent`; this mirrors the real `codex` + `agy` fallback that
        // prompted issue #94.
        registry.mark_unavailable_with_reason(
            "capacity-agent",
            Duration::from_secs(3600),
            UnavailableReason::CapacityLimit,
        );
        registry.mark_unavailable_with_reason(
            "flag-agent",
            Duration::from_secs(3600),
            UnavailableReason::StartFailed,
        );
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

        let comments = api.comments();
        assert!(
            comments.iter().any(|c| {
                c.contains("capacity-agent")
                    && c.contains("(capacity limit)")
                    && c.contains("flag-agent")
                    && c.contains("(start failed)")
                    && c.contains("switching to **good-agent**")
            }),
            "the notice must explain why each agent is unavailable: {comments:?}"
        );
    }

    /// Issue #94: a run that cannot start is reported as "start failed", and a
    /// capacity-marked agent passed over on the way keeps its own reason.
    #[tokio::test]
    async fn switch_notice_reports_each_agents_reason() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = capacity_config(dir.path());
        config.reply.ack = true;
        config.agents.overrides.insert(
            "missing-agent".into(),
            crate::config::AgentConfig {
                command: Some("definitely-not-a-real-binary-xyz".into()),
                ..Default::default()
            },
        );
        let config = Arc::new(config);
        let registry =
            isolated_registry(&config, &["missing-agent", "capacity-agent", "good-agent"]);
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

        let session = sessions
            .get(&SessionStore::key(&message("o/r"), Some("default")))
            .unwrap();
        assert_eq!(session.runs[0].agent, "good-agent");

        let status = api.comments().join("\n");
        assert!(status.contains("(start failed)"), "{status}");
        assert!(status.contains("(capacity limit)"), "{status}");
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

        let session = sessions
            .get(&SessionStore::key(&message("o/r"), Some("default")))
            .unwrap();
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

    /// Inline review comments use the same tracked flow as any other comment:
    /// the acknowledgement is posted on accept and fallback notices edit that
    /// same review comment in place.
    #[tokio::test]
    async fn inline_review_acknowledgement_is_edited_in_place() {
        use crate::forge::{ReplyTarget, ReviewCommentTarget};

        let dir = tempfile::tempdir().unwrap();
        let mut config = capacity_config(dir.path());
        config.reply.ack = true;
        let config = Arc::new(config);
        let registry = isolated_registry(&config, &["capacity-agent", "good-agent"]);
        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let api = Arc::new(EditableApi::default());
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

        wait_for_drain(&sessions).await;

        let comments = api.comments();
        assert_eq!(
            comments.len(),
            1,
            "the inline acknowledgement must be edited, not duplicated: {comments:?}"
        );
        assert!(comments[0].contains("On it"), "{comments:?}");
        assert!(
            comments[0].contains("switching to **good-agent**"),
            "the fallback notice must edit the same review comment: {comments:?}"
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

    /// A mention that arrives while its conversation is busy must not claim the
    /// agent is already running: it is acknowledged as waiting, and the same
    /// comment is rewritten once the job actually starts.
    #[tokio::test]
    async fn queued_follow_up_is_acknowledged_as_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let (mut config, log, release) = gated_config(dir.path(), 2);
        config.reply.ack = true;
        let config = Arc::new(config);
        let registry = isolated_registry(&config, &["gate"]);
        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let api = Arc::new(EditableApi::default());
        let dispatcher = Dispatcher::new(
            config.clone(),
            registry,
            sessions.clone(),
            api.clone(),
            Policy::new(&config.policy),
        )
        .unwrap();

        submit_token(&dispatcher, "o/r", 7, "TOKEN_A").await;
        wait_for_log(&log, "start:TOKEN_A").await;
        submit_token(&dispatcher, "o/r", 7, "TOKEN_B").await;

        // The follow-up is queued, so it says so instead of "running agent".
        let waiting = api
            .comments()
            .into_iter()
            .find(|comment| comment.contains("Waiting"))
            .expect("the queued mention must be acknowledged as waiting");
        assert_eq!(
            waiting,
            "forge-bot: 🤖 Waiting for the previous job to finish."
        );

        release_token(&release, "TOKEN_A");
        wait_for_log(&log, "start:TOKEN_B").await;

        // Once it starts, the waiting acknowledgement becomes the running
        // headline.
        let comments = api.comments();
        assert!(
            comments
                .iter()
                .any(|comment| comment.contains("running agent **gate**")),
            "the waiting acknowledgement must be rewritten once it runs: {comments:?}"
        );
        assert!(
            !comments.iter().any(|comment| comment.contains("Waiting")),
            "no stale waiting acknowledgement may remain: {comments:?}"
        );

        release_token(&release, "TOKEN_B");
        wait_for_drain(&sessions).await;
    }

    /// The same honesty when the conversation is idle but every worker is
    /// busy: the mention waits for a free worker, not for a run on its own
    /// thread.
    #[tokio::test]
    async fn queued_mention_waits_for_a_free_worker() {
        let dir = tempfile::tempdir().unwrap();
        let (mut config, log, release) = gated_config(dir.path(), 1);
        config.reply.ack = true;
        let config = Arc::new(config);
        let registry = isolated_registry(&config, &["gate"]);
        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let api = Arc::new(EditableApi::default());
        let dispatcher = Dispatcher::new(
            config.clone(),
            registry,
            sessions.clone(),
            api.clone(),
            Policy::new(&config.policy),
        )
        .unwrap();

        submit_token(&dispatcher, "o/r", 1, "TOKEN_A").await;
        wait_for_log(&log, "start:TOKEN_A").await;
        submit_token(&dispatcher, "o/r", 2, "TOKEN_B").await;

        // The queued job for issue 2 keeps a third mention waiting even before
        // any run exists for that conversation.
        assert!(
            dispatcher
                .inner
                .thread_is_busy(&SessionStore::key(&message_at("o/r", 2), Some("default"))),
            "a pending job must mark its conversation busy"
        );

        let waiting = api
            .comments()
            .into_iter()
            .find(|comment| comment.contains("Waiting"))
            .expect("a mention with no free worker must be acknowledged as waiting");
        assert_eq!(
            waiting,
            "forge-bot: 🤖 Waiting for the previous job to finish."
        );

        release_token(&release, "TOKEN_A");
        wait_for_log(&log, "start:TOKEN_B").await;
        assert!(
            api.comments()
                .iter()
                .any(|comment| comment.contains("running agent **gate**")),
            "the waiting acknowledgement must be rewritten once it runs"
        );

        release_token(&release, "TOKEN_B");
        wait_for_drain(&sessions).await;
    }

    /// A signed forge event states what triggered the run instead of implying
    /// a user mentioned the bot.
    #[tokio::test]
    async fn auto_trigger_ack_names_its_origin() {
        let dir = tempfile::tempdir().unwrap();
        let (mut config, log, release) = gated_config(dir.path(), 1);
        config.reply.ack = true;
        config.policy.auto_allowed_repos = vec!["o/r".into()];
        config.policy.auto_allowed_pr_authors = vec!["trusted".into()];
        let config = Arc::new(config);
        let registry = isolated_registry(&config, &["gate"]);
        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let api = Arc::new(EditableApi::default());
        let dispatcher = Dispatcher::new(
            config.clone(),
            registry,
            sessions.clone(),
            api.clone(),
            Policy::new(&config.policy),
        )
        .unwrap();

        // The queue boundary must reject denied authors and repository scope
        // even if a caller bypasses the webhook's automatic-trigger handler.
        for (repo, author) in [("o/r", "untrusted"), ("o/other", "trusted")] {
            let result = dispatcher
                .submit_auto(
                    message_at(repo, 9),
                    Mention {
                        agent: None,
                        message: "denied".into(),
                    },
                    "gate",
                    author,
                )
                .await;
            assert!(matches!(result, Err(BotError::Unauthorized(_))));
        }
        assert!(sessions.pending_jobs().unwrap().is_empty());
        assert!(api.comments().is_empty());

        let mut message = message_at("o/r", 9);
        message.author = crate::auto_trigger::AUTO_TRIGGER_AUTHOR.into();
        message.event = "action_run_failure".into();
        message.comment_id = None;
        dispatcher
            .submit_auto(
                message,
                Mention {
                    agent: None,
                    message: "TOKEN_A".into(),
                },
                "gate",
                "trusted",
            )
            .await
            .unwrap();

        let ack = api
            .comments()
            .into_iter()
            .next()
            .expect("the automatic trigger must be acknowledged");
        assert_eq!(
            ack,
            "forge-bot: 🤖 On it — triggered by a failed CI run; running agent **gate**."
        );

        wait_for_log(&log, "start:TOKEN_A").await;
        release_token(&release, "TOKEN_A");
        wait_for_drain(&sessions).await;
    }

    /// An automatic trigger is acknowledged at reception, even while the
    /// conversation is busy, naming what triggered it.
    #[tokio::test]
    async fn auto_trigger_is_acknowledged_while_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let (mut config, log, release) = gated_config(dir.path(), 1);
        config.reply.ack = true;
        config.policy.auto_allowed_repos = vec!["o/r".into()];
        config.policy.auto_allowed_pr_authors = vec!["trusted".into()];
        let config = Arc::new(config);
        let registry = isolated_registry(&config, &["gate"]);
        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        let api = Arc::new(EditableApi::default());
        let dispatcher = Dispatcher::new(
            config.clone(),
            registry,
            sessions.clone(),
            api.clone(),
            Policy::new(&config.policy),
        )
        .unwrap();

        // Occupy the conversation with a human mention first.
        submit_token(&dispatcher, "o/r", 9, "TOKEN_A").await;
        wait_for_log(&log, "start:TOKEN_A").await;

        let mut message = message_at("o/r", 9);
        message.author = crate::auto_trigger::AUTO_TRIGGER_AUTHOR.into();
        message.event = "merge_conflict".into();
        message.comment_id = None;
        dispatcher
            .submit_auto(
                message,
                Mention {
                    agent: None,
                    message: "TOKEN_B".into(),
                },
                "gate",
                "trusted",
            )
            .await
            .unwrap();

        let comments = api.comments();
        assert!(
            comments.iter().any(|comment| comment
                == "forge-bot: 🤖 On it — triggered by a merge conflict; running agent **gate**."),
            "the automatic trigger must be acknowledged with its origin: {comments:?}"
        );
        assert!(
            !comments.iter().any(|comment| comment.contains("Waiting")),
            "the waiting acknowledgement must stay unchanged: {comments:?}"
        );

        release_token(&release, "TOKEN_A");
        wait_for_log(&log, "start:TOKEN_B").await;
        release_token(&release, "TOKEN_B");
        wait_for_drain(&sessions).await;
    }

    /// The origin wording only applies to signed forge events; a human mention
    /// keeps the plain headline.
    #[test]
    fn auto_trigger_origin_maps_events() {
        let mut message = message_at("o/r", 1);
        message.author = crate::auto_trigger::AUTO_TRIGGER_AUTHOR.into();
        message.event = "action_run_failure".into();
        assert!(is_auto_trigger(&message));
        assert_eq!(auto_trigger_origin(&message), Some("a failed CI run"));
        assert_eq!(
            running_headline(&message, "codex"),
            "🤖 On it — triggered by a failed CI run; running agent **codex**."
        );

        message.event = "merge_conflict".into();
        assert_eq!(auto_trigger_origin(&message), Some("a merge conflict"));

        message.event = "future_event".into();
        assert_eq!(
            auto_trigger_origin(&message),
            Some("an automatic forge event")
        );

        let human = message_at("o/r", 2);
        assert!(!is_auto_trigger(&human));
        assert_eq!(auto_trigger_origin(&human), None);
        assert_eq!(
            running_headline(&human, "codex"),
            "🤖 On it — running agent **codex**."
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
                user_id: Some("default".into()),
                created_at: Utc::now() + chrono::Duration::seconds(index as i64),
                status_comment: None,
                waiting: false,
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

    /// A stale thread left on disk must not survive the next start, or a bot
    /// that never runs long enough to evict in the background would keep
    /// growing its status forever (issue #119).
    #[tokio::test]
    async fn dispatcher_evicts_stale_thread_status_on_start() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config(dir.path());
        config.session.retention_secs = 60;
        let config = Arc::new(config);

        let key = "forgejo:o/r:issue:1";
        let old = Utc::now() - chrono::Duration::hours(2);
        let session = crate::session::Session {
            key: key.into(),
            repository: "o/r".into(),
            location: "http://forge.local/o/r/issues/1".into(),
            agent: "codex".into(),
            created_at: old,
            updated_at: old,
            runs: Vec::new(),
        };
        std::fs::create_dir_all(dir.path().join("sessions")).unwrap();
        std::fs::write(
            dir.path().join("sessions").join("stale.json"),
            serde_json::to_vec_pretty(&session).unwrap(),
        )
        .unwrap();

        let sessions = Arc::new(SessionStore::open(dir.path()).unwrap());
        assert!(sessions.get(key).is_some(), "the store loads it first");

        let _dispatcher = Dispatcher::new(
            config.clone(),
            Arc::new(AgentRegistry::from_config(&config)),
            sessions.clone(),
            Arc::new(NoopForgeApi),
            Policy::new(&config.policy),
        )
        .unwrap();

        assert!(
            sessions.get(key).is_none(),
            "an idle thread past the retention window is evicted at start"
        );
    }
}

# forge-bot

A forge-agnostic bot that turns `@agent` mentions into coding-agent runs.

Mention the bot in a comment on an issue or pull request:

```text
@agent investigate this test failure and fix it
```

and it will route the request to a coding agent (Codex, Antigravity CLI, Pi, Claude Code, Kimi,
or anything you configure), which reads the surrounding context, changes code,
runs tests, pushes, and replies.

The design keeps a hard boundary:

```text
Forge → webhook → gateway → (location URL, message) → agent → does everything
```

The gateway never builds context for the agent. It hands it a location and a
message; the agent decides what to look at.

## How it works

```text
Forgejo / GitHub / GitLab
        │ webhook (signed)
        ▼
┌──────────────────────────────┐
│ Gateway                      │
│  verify signature            │
│  detect @agent mention       │
│  authorize author/repo       │
│  extract location + message  │
│  select agent                │
└──────────────┬───────────────┘
               │ AgentRequest { location, message }
               ▼
┌──────────────────────────────┐
│ Dispatcher                   │
│  per-conversation scheduler  │
│  session persistence         │
│  workspace checkout          │
└──────────────┬───────────────┘
               ▼
   Codex · Pi · Claude Code · Kimi · custom
               │
               ├─ inspect forge
               ├─ inspect/modify code
               ├─ run commands / tests
               ├─ commit / push
               └─ reply
```

### Core contracts

The gateway only sends this:

```rust
pub struct AgentRequest {
    pub location: Url,
    pub message: String,
}
```

Forge and agent implementations are independent:

```rust
#[async_trait]
pub trait ForgeAdapter: Send + Sync {
    fn verify(&self, headers: &HeaderMap, body: &[u8]) -> Result<()>;
    fn parse(&self, headers: &HeaderMap, body: &[u8]) -> Result<Vec<ForgeMessage>>;
    // ...
}

#[async_trait]
pub trait Agent: Send + Sync {
    fn name(&self) -> &str;
    async fn run(&self, request: &AgentRequest, context: &AgentContext) -> Result<AgentOutcome>;
}
```

## Status

Implemented:

- [x] Forgejo webhook receiver
- [x] Webhook signature verification (HMAC-SHA256)
- [x] `@agent` mention detection (case-insensitive, optional `@agent --agent=<name>`)
- [x] Authorization (allow-list users/repos, ignore self)
- [x] Location URL + message extraction
- [x] Codex adapter
- [x] Antigravity CLI (`agy`) / Pi / Kimi / Claude Code adapters
- [x] Auto-approve: codex always bypasses its sandbox; agy and claude skip permission
  checks and pi trusts project files by default (`dangerously_skip_permissions = false`
  opts agy, claude, and pi out)
- [x] Long-lived Pi RPC agent pool (`pi-rpc`), the default Pi backend: reuses an
  idle agent for any conversation in the same workspace/user/model, and persists
  each conversation's `--session-id` so an evicted process resumes it. The pool
  has no separate size knob: `[session] workers` is the single total agent
  count. By default a reused process starts the new conversation's own session
  (`new_session` for a brand-new thread, or a switch back to the session file it
  used before), so a new conversation never inherits another one's context
  (`session_per_conversation`). The one-shot `pi` adapter is kept but disabled
  by default (`[agents.pi] enabled = true`)
- [x] Per-thread agent sessions: one conversation per `owner/repo` issue or PR,
  resumed by `pi-rpc` and the one-shot `codex`/`pi`/`claude` adapters, so the
  model context (and its prompt cache) is reused across comments
- [x] Same-thread follow-ups: a second mention while a run is in flight is
  merged into that same run instead of waiting for it to settle and becoming
  the next turn. `pi-rpc` receives it as a `steer`, the default `codex`
  binary is driven through `codex app-server` and uses `turn/steer`, and the
  default `kimi` binary uses wire mode's `steer` (Wire 1.4+). The thread gets
  a "📎 Merged into the current run." notice. Adapters with no live steering
  channel (an operator-supplied command/args, a Codex/Kimi build without the
  persistent protocol, `agy`, `claude`) queue the follow-up and run it as the
  next turn
- [x] Agent forge access (credentials via environment, optional checkout)
- [x] Per-conversation job scheduling: at most one run per issue/PR at a time,
  while different conversations run in parallel. Since a conversation runs at
  most one agent at a time, `[session] workers` is also the single global cap
  on concurrent agent processes across every adapter (pooled and one-shot),
  plus on-disk session/job persistence
- [x] GitHub and GitLab adapters
- [x] Per-user systemd service (no root)
- [x] Polling ingester for deployments where the bot cannot create a webhook: discovers every repository visible to the token and refreshes the list, so new repositories are picked up automatically
- [x] Forgejo Actions failure and pull-request conflict triggers for repositories covered by the signed webhook; an automatic trigger continues the thread's conversation, reusing the adapter that last ran there (or the registry default for a new thread)
- [x] Agent capacity / quota handling: a failed run that looks like a usage limit, rate limit or provider overload marks the agent unavailable for a cooldown and the job is retried on another available agent (adapters that cannot even start are skipped too). The requested (or default) agent is always tried first, even while it is cooling down, so a recovered quota is picked up without a restart; a successful run clears the mark. The cooldown only removes an agent from the *automatic* fallback list, and the fallback notice names each skipped agent with the reason it was taken out of rotation (for example `capacity limit` or `start failed`); when none is left the bot replies `No available agent` listing those reasons
- [x] Web status page: `/status` renders every known thread with its state
  (running / queued / idle), the agent involved, the queued follow-ups and the
  last result, and searches by comment/issue URL;
  `/status.json` serves the same snapshot (and filter) for scripts
- [x] Human notifications: a `role = "human"` account is a person, not an
  agent. Agents are told to mention one when they need a privilege request,
  an account registration or another human action; `/notifications` turns the
  mention into a browser system notification (service-worker based, so it also
  works on Android and iOS over HTTPS)

Still open (see the issue's roadmap):

- [ ] Stronger agent sandboxing / isolation
- [ ] Per-repository agent selection

## Releases

Push a `v*` tag to build a Linux release on the self-hosted runner. The release
contains a `forge-bot-<tag>-linux-<architecture>.tar.gz` archive and a
`SHA256SUMS` file.

## Quick start

```bash
cargo build --release

cp config.example.toml forge-bot.toml
$EDITOR forge-bot.toml

# Validate the configuration and see what was loaded.
cargo run -- check

# Run the server (default command).
cargo run -- serve

# Or run only the polling ingester, without binding a port.
cargo run -- poll
```

For a full deployment walkthrough — registering the Forgejo webhook, running
as a service, secrets, verification and troubleshooting — see
[`deploy.md`](deploy.md). forge-bot always runs as the root-controlled system
service and executes every agent as a configured `[users.*].host_user`:

```bash
cargo build --release
sudo ./contrib/install-system.sh   # systemctl status forge-bot
```

Secrets can be supplied through the environment instead of the file:

```bash
export FORGEJO_WEBHOOK_SECRET=...
export FORGEJO_TOKEN=...
cargo run -- serve
```

## Configuration

See [`config.example.toml`](config.example.toml) for the full reference. The
most important options:

| Key | Meaning |
| --- | --- |
| `bind` | Address the webhook server listens on. |
| `mention` | Trigger string, default `@agent`. |
| `agent_sequence` | Ordered agent names for unqualified mentions and capacity fallback. The first registered name becomes the default; only listed agents are fallback candidates. When omitted, the built-in order starts with `codex`. Explicit `@agent --agent=<name>` still takes priority. |
| `[forgejo]` | `base_url`, `webhook_secret`, `token`, `bot_username`. |
| `[policy]` | `allow_all`, `allowed_users`, `allowed_repos`. |
| `[workspace]` | Whether to clone a checkout, and where. |
| `[reply]` | `ack` defaults to true; the acknowledgement and any fallback notices share one status comment. `result` defaults to false because agents normally reply themselves; set `result = true` to also post completion summaries. |
| `[session]` | Queue/state directory, worker count (the global cap on concurrent agent runs), recovery, and how long an idle thread keeps its status (`retention_secs`, `0` disables eviction). |
| `[capacity]` | Capacity/quota detection: `fallback`, `cooldown_secs` (default 5 h), extra `markers` (`[quota]` is an alias). |
| `[agents.<name>]` | Per-agent `command`, `args`, `prompt`, `timeout_secs`, `env`. |
| `[users.<id>]` | Required agent users: `role` (`default` or `reviewer`), `host_user`, `token`, `agent`, `agent_model`. A `role = "human"` table is a notification recipient and needs no `host_user`. At least one table is required. |
| `[executor]` | cgroup executor settings: `cgroup_root`, `passwd_file`. |

Configuration is loaded from `FORGE_BOT_CONFIG` (or `--config`), falling back to
`forge-bot.toml`, falling back to defaults. Environment variables override file
values.

### Agent users

Every run belongs to a configured `[users.<id>]` account. At least one table is
required; there is no single implicit account. Exactly one user must have
`role = "default"`:

```toml
[forgejo]
bot_username = "forge-bot"

[users.forge-bot]
role = "default"      # exactly one user must be the default
host_user = "agent"   # real, non-root Linux account
token = "..."         # falls back to [forgejo].token when omitted

[users.forge-reviewer]
role = "reviewer"
host_user = "forge-reviewer"
token = "..."         # required for a non-default user to act as itself
agent = "codex"        # optional adapter override
agent_model = "gpt-fast"   # optional model passed to the agent

[users.alice]
role = "human"        # a person to notify, not an agent; no host_user needed
```

When an agent submits or updates a PR, its prompt asks it to post a separate
comment mentioning one configured `role = "reviewer"` account. Forge-bot
selects the first reviewer in configuration order, excluding the running user;
without a reviewer, no automatic reviewer handoff is requested. Reviewers are
given the PR author login by forge-bot and instructed to mention that author
for fixes and ask for another review after updates. If the author lookup fails,
the reviewer is asked to read it from the forge. On a successful review, they rebase/squash to one commit, verify the
final result, approve, and enable fast-forward auto-merge. These are agent
instructions; completion depends on the agent and its forge permissions.
Configured accounts can mention peers within the repository policy even when
`allowed_users` lists only humans. Self-mentions, ambiguous mentions, and
explicitly ignored authors remain blocked. Use a separate comment containing
only the intended agent handle for each handoff.

A `role = "human"` user is a person, not an agent. It is never routed a run,
its `host_user` may be omitted, and it is not added to the policy's ignored
logins, so a human can still trigger an agent. Agents are told to mention the
first configured human when they need a person to do something they cannot (a
privilege request, an account registration, a secret, ...). When an agent
comment mentions a human, the bot records a notification for that human; see
[Human notifications](#human-notifications).

Each user is addressed by its own login (`@forge-reviewer`). The default user
logs in as `forgejo.bot_username`, or its table key when that is unset; a
non-default user logs in as its table key. An `--agent=` in the mention still
takes precedence, then the user's `agent`, then the registry default. A
comment that addresses more than one configured user is ignored rather than
routed arbitrarily. Configured agent users may address one another, but may
not invoke themselves.

Every operation for a run uses the addressed user's identity:

* **Token**: the gateway's replies, the credentials exported to the agent, and
  polling use that user's `token`. A non-default user never falls back to the
  legacy `[forgejo].token`, so `@forge-reviewer` replies as the reviewer.
* **Workspace**: the checkout lives directly in the `host_user`'s home
  (`<home>/<owner__repo-number>`), so users never share a working tree and the
  dropped-privilege process creates and owns the directories it writes. The
  gateway never creates or changes ownership of workspace paths as root.
* **Session**: job, thread and backend-session keys are prefixed with the
  `user_id`, and the `pi-rpc` pool only reuses a process for the same user and
  model. Two users in the same thread get independent conversations.
* **Model**: `agent_model` is passed as `--model <id>` unless the adapter's
  configured `args` already set one. It applies to the user's configured
  `agent` (or the registry default when `agent` is omitted). Alternate
  adapters and automatic fallbacks keep their own model defaults.
* **Recovery**: `user_id` is persisted with the job, so a restarted bot resumes
  the job under the same account and refuses an id that no longer exists.

### Process isolation (the cgroup executor)

forge-bot runs as the root-controlled system service. Every run is forked
natively, moved into a per-run cgroup v2 and dropped to its `host_user` before
`exec`; there is no single-account fallback and no external
`systemd-run`/`systemctl` process.

```toml
[executor]
# Root of the cgroup v2 hierarchy (optional).
cgroup_root = "/sys/fs/cgroup/forge-bot"
```

The host must mount cgroup v2 at `/sys/fs/cgroup`; whole-cgroup cancellation
needs `cgroup.kill` (Linux 5.14+), otherwise only the direct child is killed.
`forge-bot check` verifies that the cgroup root is a real cgroup v2 directory.
Every spawn also requires successful attachment to its run cgroup; an open,
write, or incomplete PID-write failure aborts execution.

* The target account, UID and GID are resolved from `passwd_file` at startup;
  an unknown account, UID 0, or two users sharing a UID is rejected before the
  bot serves.
* Children start from a controlled environment (`HOME`, `USER`, `LOGNAME`,
  `PATH`, and `XDG_RUNTIME_DIR` when `/run/user/<uid>` exists) plus the
  adapter's `env` and that run's forge credentials. The caller's environment is
  not inherited. The agent is a normal process for the account and can exec
  external commands (`git`, build tools, tests, ...) with that user's
  permissions. Session-scoped tools (`systemctl --user`, D-Bus) need
  `loginctl enable-linger <host_user>`, which creates `/run/user/<uid>`.
* Secrets are passed in the child's environment (`execve`'s envp), never on a
  command line, so they do not appear in `ps`.
* CLI reply files must be regular files owned by the target account, with a
  single link and at most 1 MiB of UTF-8 text. The gateway rejects symlinks,
  hard links, FIFOs, devices, and files belonging to another account.
* A checked-out `pi-rpc` process is only reused for the same user and model,
  so one identity never inherits another's live session.
* Each run gets `<cgroup_root>/<user>/run-<uuid>/`, so all of one account's
  work shares a parent cgroup that an operator can inspect or stop.
* Cancelling a run (timeout, shutdown, dropped future) writes `cgroup.kill`,
  which stops the run's whole cgroup, not just the direct child.

Still follow-ups from the [#135](https://forgejo.shylockhg.me/shylock/forge-bot/issues/135)
plan: per-target polling cursors (the poller currently polls as the default
user), ownership/cleanup of the workspace tree, and a D-Bus helper so secrets
are never materialised as a file at all.

## Wiring a Forgejo webhook

See [`doc/forgejo-webhook.md`](doc/forgejo-webhook.md) for the full guide
(repository / organization / user / system scopes, events, API examples, the
loopback caveat, and verification).

Quick repository hook: **Settings → Webhooks → Add webhook → Forgejo**, target
`http://<host>:8080/webhooks/forgejo`, secret = `FORGEJO_WEBHOOK_SECRET`, event
**Issue comments**.

The endpoint also accepts GitHub (`/webhooks/github`) and GitLab
(`/webhooks/gitlab`) webhooks, selected by URL path.

## Status page

While the server is running, `GET /status` renders a self-refreshing HTML page
listing every thread the bot knows about: conversations with an agent in flight
(*running*), mentions waiting for a worker (*queued*), and idle threads with
their most recent result. Each row names the agent. `GET /status.json`
returns the same snapshot as JSON for dashboards or scripts, and `GET /`
reports the enabled forges and agents.

The **Details** link opens `/status/details?key=...`, showing each run's
request, agent, timestamps, and result summary. Requests are saved for new
runs; older runs show “Unavailable for this run.” The result is the summary
recorded by the agent adapter, which may be shorter than its full transcript.
While a run is active, the details page refreshes every two seconds and shows
recent command output or Pi RPC assistant text as it arrives. Live output is
kept in memory (up to 1 MiB per run) and disappears when the run finishes;
the final result summary remains in the run history.

The Model column shows the model known for the current or most recent run.
Pi RPC reads the live model through `get_state`; Codex and Claude read their
session records after a run. Antigravity and Kimi read the selected model from
their CLI arguments or configuration. A dash means no model was available.
The JSON snapshot includes `model` as a nullable field.

The page has a search box: paste a comment or issue/pull-request URL (for
example `https://forgejo.example.com/owner/repo/pulls/460#issuecomment-10561`)
to filter the table to that thread. The same filter is available to scripts as
`GET /status.json?q=<url>`.

An idle thread is kept for `[session] retention_secs` (default seven days)
and then evicted from memory and disk, so a bot that runs for a long time does
not accumulate status until it runs out of memory. A thread with a run in
flight or a mention waiting is never evicted. Set `retention_secs = 0` to keep
every thread forever.

## Human notifications

When a run needs something only a person can do, the agent is prompted to post
a comment mentioning one configured `role = "human"` account (the first in
configuration order) and to say exactly what it needs. The gateway records the
mention as a notification without routing another agent run.

`GET /notifications` is a small web page that turns those notifications into
browser system notifications. Pick the human account, grant notification
permission, and keep the page open; the page polls
`GET /notifications.json?recipient=<login>&after=<id>` every five seconds and
raises a system notification for each new entry.

Desktop browsers use the page directly. Android and iOS browsers reject the
`Notification` constructor and require
`ServiceWorkerRegistration.showNotification`, so the page registers a small
service worker (`/notifications/sw.js`, scope `/`) and prefers
`showNotification` when it is available. The page also links a web app manifest
(`/notifications.webmanifest`, `display: "standalone"`) and the
`apple-mobile-web-app-capable` metadata, so "Add to Home Screen" installs a
notification-capable app rather than a bookmark that reopens in the default
browser. Service workers only run in a secure context, and notification
permission is per-origin:

- serve forge-bot over **HTTPS** (or `localhost` during development), and
- on **iOS 16.4+**, add the page to the **Home Screen** first, then open it from
there and grant notification permission. Safari tabs cannot receive these
notifications.

The cursors are kept per human account, so switching accounts does not hide a
recipient's pending notifications, and a server restart is detected through a
per-process generation that resets stale cursors instead of skipping the new
entries. The notification log is in memory and bounded (the newest 1024
entries); the forge comment that mentioned the human remains the durable
record.

## Trigger syntax

```text
@agent fix the failing test          # default agent
@agent --agent=codex refactor this module   # pick an agent explicitly
@agent --agent=agy fix the build            # Antigravity CLI
@agent --agent=pi-rpc review the diff       # any configured adapter
```

When a provider reaches capacity, the default fallback order is `codex` →
`agy` → `pi-rpc` → `claude` → `kimi`. The one-shot `pi` adapter joins after
`pi-rpc` when enabled. Authenticate `agy` interactively once before using it
through the bot.

An agent that hits a capacity limit is skipped for a cooldown as an automatic
fallback, but the requested (or default) agent is always tried
first, even while it is cooling down: the quota may have reset, and a
successful run clears the mark. This is why a thread can still be answered by
`codex` immediately after an earlier capacity notice. When a failed response
includes an explicit retry interval or reset time, that sets the cooldown;
times without a timezone use the bot host's local timezone. Otherwise,
`[capacity] cooldown_secs` is used.

The example config sets `agent_sequence = ["codex", "agy", "pi-rpc", "claude"]`
at the top level. Use registered adapter names; `agy` is Antigravity
and `claude` is Claude Code. An explicit `@agent --agent=<name>` runs first even if it
is absent from the sequence. Agents not listed are not tried as fallbacks.
Keep the space before `--agent` so Forgejo renders `@agent` as a clickable
mention. Colon selector forms are no longer accepted.

The mention is matched case-insensitively and only at a word boundary, so
`foo@agent.com` does not trigger it. Self-mentions are ignored to avoid loops.

## Security

- Every webhook is signature-verified when a secret is configured. Never run
  without one in production.
- Authorization is separate from authentication. With no allow-list and no
  `allow_all`, the bot rejects everything.
- Unconfigured forge bot logins are ignored automatically; configured accounts
  may invoke peers, with self-mentions blocked.
- Agents receive a scoped forge token plus `FORGEJO_URL`/`FORGE_TOKEN` in their
  environment. They are *not* given an administrator token; grant only the
  scopes needed to comment and push.
- Workspace Git commands use an environment-backed credential helper. Tokens
  never enter clone arguments or remote URLs; configured credential-store
  helpers are disabled for these commands, and errors redact token values.
- `workspace.enabled = false` runs agents in an empty directory and lets them
  access the forge themselves.
- The status pages have no authentication: they list thread URLs and show
  agent requests and result summaries. Keep the listener on an
  internal interface or put it behind a reverse proxy with access control.

## Project layout

```text
src/
├── agent/          # Agent trait + Codex/Antigravity/Pi/Claude/Kimi adapters
├── forge/          # ForgeAdapter trait + Forgejo/GitHub/GitLab
├── session/        # scheduler, workers, session/job persistence
├── webhook.rs      # axum HTTP routes
├── config.rs       # configuration
├── location.rs     # URL → normalized ForgeLocation
├── mention.rs      # @agent parsing
├── policy.rs       # authorization
├── workspace.rs    # checkout management
├── forge_api.rs    # posting comments back
└── main.rs         # CLI entry point
```

## Development

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all
./contrib/coverage.sh --fail-under-lines 95
```

The test suite covers URL parsing, mention extraction, HMAC verification,
payload normalization for all three forges, policy decisions, session
persistence, the dispatcher, and an end-to-end signed webhook flow.

CI splits into two concurrent jobs: `lint` runs `cargo fmt` and `cargo clippy`,
while `test` runs the whole suite once through `cargo-llvm-cov` (so the tests are
validated and covered in a single pass) and posts the result as a comment on the
pull request, updating the same comment on every push via
[`contrib/coverage-comment.sh`](contrib/coverage-comment.sh).

## Research notes

Measurements that informed the design live under [`doc/`](doc/):

- [`doc/kvcache-hit-rate.md`](doc/kvcache-hit-rate.md) — prompt/KV cache hit
  rate of the Codex and Pi sessions forge-bot invokes, with the reproducer
  [`contrib/analyze-kvcache.py`](contrib/analyze-kvcache.py).

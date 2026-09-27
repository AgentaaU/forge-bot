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
- [x] `@agent` mention detection (case-insensitive, optional `@agent:<name>`)
- [x] Authorization (allow-list users/repos, ignore self)
- [x] Location URL + message extraction
- [x] Codex adapter
- [x] Antigravity CLI (`agy`) / Pi / Kimi / Claude Code adapters
- [x] Auto-approve: codex always bypasses its sandbox; agy and claude skip permission
  checks and pi trusts project files by default (`dangerously_skip_permissions = false`
  opts agy, claude, and pi out)
- [x] Long-lived Pi RPC agent pool (`pi-rpc`), the default Pi backend: reuses an
  idle agent, spawns one when all are busy, and persists a deterministic
  `--session-id` so an evicted process resumes its conversation. The pool has
  no separate size knob: `[session] workers` is the single total agent count. A
  new conversation starts a new session by default (`session_per_conversation`).
  The one-shot `pi` adapter is kept but disabled by default
  (`[agents.pi] enabled = true`)
- [x] Per-thread agent sessions: one conversation per `owner/repo` issue or PR,
  resumed by `pi-rpc` and the one-shot `codex`/`pi`/`claude` adapters, so the
  model context (and its prompt cache) is reused across comments
- [x] Agent forge access (credentials via environment, optional checkout)
- [x] Per-conversation job scheduling: at most one run per issue/PR at a time,
  while different conversations run in parallel. Since a conversation runs at
  most one agent at a time, `[session] workers` is also the single global cap
  on concurrent agent processes across every adapter (pooled and one-shot),
  plus on-disk session/job persistence
- [x] GitHub and GitLab adapters
- [x] Per-user systemd service (no root)
- [x] Polling ingester for deployments where the bot cannot create a webhook: discovers every repository visible to the token and refreshes the list, so new repositories are picked up automatically
- [x] Agent capacity / quota handling: a failed run that looks like a usage limit, rate limit or provider overload marks the agent unavailable for a cooldown and the job is retried on another available agent (adapters that cannot even start are skipped too); when none is left the bot replies `No available agent`

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
[`deploy.md`](deploy.md). The recommended way to run it is the per-user
systemd service, which needs no root and cannot touch other accounts:

```bash
cargo build --release
./contrib/install-user.sh      # systemctl --user status forge-bot
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
| `agent_sequence` | Ordered agent names for unqualified mentions and capacity fallback. The first registered name becomes the default; only listed agents are fallback candidates. When omitted, the built-in order starts with `codex`. Explicit `@agent:<name>` still takes priority. |
| `[forgejo]` | `base_url`, `webhook_secret`, `token`, `bot_username`. |
| `[policy]` | `allow_all`, `allowed_users`, `allowed_repos`. |
| `[workspace]` | Whether to clone a checkout, and where. |
| `[reply]` | `ack` defaults to true; the acknowledgement and any fallback notices share one status comment. `result` defaults to false because agents normally reply themselves; set `result = true` to also post completion summaries. |
| `[session]` | Queue/state directory, worker count (the global cap on concurrent agent runs), recovery. |
| `[capacity]` | Capacity/quota detection: `fallback`, `cooldown_secs` (default 5 h), extra `markers` (`[quota]` is an alias). |
| `[agents.<name>]` | Per-agent `command`, `args`, `prompt`, `timeout_secs`, `env`. |

Configuration is loaded from `FORGE_BOT_CONFIG` (or `--config`), falling back to
`forge-bot.toml`, falling back to defaults. Environment variables override file
values.

## Wiring a Forgejo webhook

See [`doc/forgejo-webhook.md`](doc/forgejo-webhook.md) for the full guide
(repository / organization / user / system scopes, events, API examples, the
loopback caveat, and verification).

Quick repository hook: **Settings → Webhooks → Add webhook → Forgejo**, target
`http://<host>:8080/webhooks/forgejo`, secret = `FORGEJO_WEBHOOK_SECRET`, event
**Issue comments**.

The endpoint also accepts GitHub (`/webhooks/github`) and GitLab
(`/webhooks/gitlab`) webhooks, selected by URL path.

## Trigger syntax

```text
@agent fix the failing test          # default agent
@agent:codex refactor this module    # pick an agent explicitly
@agent:agy fix the build             # Antigravity CLI
@agent:pi review the diff            # any configured adapter
```

When a provider reaches capacity, the default fallback order is `codex` →
`agy` → `pi-rpc` → `claude` → `kimi`. The one-shot `pi` adapter joins after
`pi-rpc` when enabled. Authenticate `agy` interactively once before using it
through the bot.

The example config sets `agent_sequence = ["codex", "agy", "pi-rpc", "claude"]`
at the top level. Use registered adapter names; `agy` is Antigravity
and `claude` is Claude Code. An explicit `@agent:<name>` runs first even if it
is absent from the sequence. Agents not listed are not tried as fallbacks.

The mention is matched case-insensitively and only at a word boundary, so
`foo@agent.com` does not trigger it. Bot comments are ignored to avoid loops.

## Security

- Every webhook is signature-verified when a secret is configured. Never run
  without one in production.
- Authorization is separate from authentication. With no allow-list and no
  `allow_all`, the bot rejects everything.
- The bot's own login is added to the ignore list automatically.
- Agents receive a scoped forge token plus `FORGEJO_URL`/`FORGE_TOKEN` in their
  environment. They are *not* given an administrator token; grant only the
  scopes needed to comment and push.
- Workspaces are cloned with an authenticated URL and the credential is
  stripped from `origin` afterwards.
- `workspace.enabled = false` runs agents in an empty directory and lets them
  access the forge themselves.

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

CI runs the suite through `cargo-llvm-cov` and posts the result as a comment on
the pull request (updating the same comment on every push), using
[`contrib/coverage-comment.sh`](contrib/coverage-comment.sh).

## Research notes

Measurements that informed the design live under [`doc/`](doc/):

- [`doc/kvcache-hit-rate.md`](doc/kvcache-hit-rate.md) — prompt/KV cache hit
  rate of the Codex and Pi sessions forge-bot invokes, with the reproducer
  [`contrib/analyze-kvcache.py`](contrib/analyze-kvcache.py).

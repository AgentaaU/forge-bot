# Deploying forge-bot

forge-bot turns `@<bot>` mentions into agent runs. A mention can be discovered
two ways:

| Trigger | Needs |
| --- | --- |
| **Webhook** (preferred) | Repository owner/admin to register the hook |
| **Poller** (fallback) | Read access only |

Both feed the same pipeline (mention → policy → queue → agent → reply). The
supporting files live in [`contrib/`](contrib/):

| File | Purpose |
| --- | --- |
| `config.example.toml` (repo root) | Annotated configuration reference |
| [`doc/forgejo-webhook.md`](doc/forgejo-webhook.md) | Hook scopes, events and API examples |
| `contrib/forge-bot.user.service` | systemd **user** unit (recommended) |
| `contrib/forge-bot.user.toml.example` | Per-user config with `~` state paths |
| `contrib/install-user.sh` | Install binary + config + user unit, no root needed |
| `contrib/forge-bot.env.example` | Environment/secret template |
| `contrib/run.sh` | Rootless launcher (`setsid` + `nohup`) |
| `contrib/detect-repo.sh` | Print `owner/repo` for the current git remote |
| `contrib/register-webhook.sh` | Create the Forgejo webhook via the API |
| `contrib/test-webhook.sh` | Send a signed test delivery |

**Secrets are never stored in this document or the config.** Supply the token
and webhook secret through the environment or the `0600` env file.

---

## 1. Build

```bash
cargo build --release
cargo test --all
```

## 2. Configure

```bash
cp config.example.toml forge-bot.toml
$EDITOR forge-bot.toml
```

The important fields:

* `mention`, `agent_sequence` (codex is the first choice by default; the
  `pi-rpc` pool is the default Pi fallback and persists its sessions. Set
  `agent_sequence = ["pi-rpc", "codex", "agy", "claude"]` to change the order. The
  one-shot `pi` adapter is disabled by default; enable it with
  `[agents.pi] enabled = true`);
* `[forgejo]` `base_url`, `bot_username`;
* `[policy]` `allowed_users` / `allowed_repos`; an empty `allowed_repos` lets
  the configured users trigger the bot on any repository, while leaving both
  lists empty denies everyone;
* `[pi_rpc]` TTL, timeout, model/provider, and
  `no_session = false` to persist conversations. The pool has no size limit of
  its own; `[session] workers` is the single total agent count. By default
  `session_per_conversation = true` keeps one session per conversation: a new
  conversation starts a fresh process instead of inheriting another
  conversation's session; set it to `false` to reuse idle processes across
  conversations in a workspace;
* `[poller]` `enabled = true` for the webhook-less fallback. With an empty
  `repositories` list the bot polls **every repository visible to its token**
  and refreshes that list every `discover_interval_secs`, so repositories
  created later are picked up automatically. Set `repositories = ["owner/repo"]`
  to restrict polling.

When a mention is found in a repository the bot cannot act on, it replies
`Permission Deny of <forge>` if it is still allowed to comment; if Forgejo also
denies commenting, the rejection is logged (nothing else can be delivered to
that thread).

Secrets come from the environment (or an `EnvironmentFile`):

```bash
export FORGEJO_TOKEN=...            # scopes: write:issue, write:repository
export FORGEJO_WEBHOOK_SECRET=...   # openssl rand -hex 32
```

Validate:

```bash
./target/release/forge-bot --config forge-bot.toml check
```

## 3. Register the webhook

The helper defaults to a **user-level** hook — every repository owned by the
token's user:

```bash
export FORGEJO_URL=http://127.0.0.1:3000
export FORGEJO_TOKEN=...              # token with write:user
export FORGEJO_WEBHOOK_SECRET=...
./contrib/register-webhook.sh
```

Use `SCOPE=repo` (with `REPO`, defaulting to the current git remote) for a
single repository, `SCOPE=org ORG=...` for an organization, or `SCOPE=system`
for the whole instance. See [`doc/forgejo-webhook.md`](doc/forgejo-webhook.md).
Then set `[poller] enabled = false`. If Forgejo refuses to deliver to loopback,
add `127.0.0.1` to `[webhook] ALLOWED_HOST_LIST` in `app.ini`, or keep the
poller enabled instead. A collaborator token cannot create hooks — use the
poller in that case.

## 4. Start

Prefer the **per-user service**: it runs as your account, cannot reach other
users' processes or system state, and needs no root.

```bash
# as the user that owns the checkout / pi auth
cargo build --release
./contrib/install-user.sh
systemctl --user status forge-bot
journalctl --user -u forge-bot -f        # or: tail -f ~/.local/state/forge-bot/forge-bot.log
```

The installer copies `contrib/forge-bot.user.toml.example` when you have no
`forge-bot.toml` yet; state and workspaces then live under
`~/.local/state/forge-bot/`. To keep the service running while logged out, run
the installer with `ENABLE_LINGER=1` (it calls
`loginctl enable-linger "$USER"`).

**Rootless launcher (no systemd):**

```bash
export FORGEJO_TOKEN=...
./contrib/run.sh ~/.config/forge-bot/forge-bot.toml
kill "$(cat ~/.local/state/forge-bot/forge-bot.pid)"   # stop
```

Do not run the bot itself as root; install-user.sh never runs as root at all.

## 5. Verify

```bash
curl -s http://127.0.0.1:8080/healthz                  # ok
FORGEJO_WEBHOOK_SECRET=... ./contrib/test-webhook.sh   # signed delivery
```

Then post `@<bot> reply with pong` from an allowed user and check that a reply
comment appears. The first mention spawns a `pi --mode rpc` agent; later
mentions reuse it while it is idle.

## 6. Operations

* **Logs**: `journalctl --user -u forge-bot -f`, or the mirrored file
  `~/.local/state/forge-bot/forge-bot.log` written by the user service.
* **State**: `state/jobs/`, `state/sessions/`, `state/poller.json`.
* **Upgrade**: rebuild, reinstall the binary, restart the service.
* **Uninstall**: stop and remove the unit/binary/config/state.

## 7. Troubleshooting

| Symptom | Fix |
| --- | --- |
| Hook creation says *"owner or admin write"* | Token is only a collaborator. Use an owner token or the poller. |
| Webhook `401` | Secret mismatch or missing `X-Forgejo-Signature`. |
| Forgejo cannot deliver to `127.0.0.1` | Allow loopback in `[webhook] ALLOWED_HOST_LIST`, or use the poller. |
| Poller never triggers | `poller.enabled = false`, or the token cannot see the repository. Reset `state/poller.json` if a cursor ran ahead. |
| `failed to spawn pi` (or `agy`) | `~/.local/bin` is not on the service `PATH`. The shipped unit sets it; for a custom unit add `Environment=PATH=%h/.local/bin:/usr/local/bin:/usr/bin`, or set `[pi_rpc] command` to an absolute path. |
| User service exits with `status=209/STDOUT` | The log directory is missing. `mkdir -p ~/.local/state/forge-bot` (`install-user.sh` does this) and restart. |
| No reply comment | `[reply] result = false`, or the token lacks `write:issue`. |
| Jobs pile up | Raise `[session] workers`, the single cap on concurrent agent runs (pooled `pi-rpc` and one-shot alike). |

## 8. This environment

* Bot runs as the `agent` account (which also owns the `pi` auth under
  `/home/agent/.pi`). Run it with `contrib/install-user.sh` so the service is
  scoped to that account.
* Binary: `~/.local/bin/forge-bot`; config: `~/.config/forge-bot/forge-bot.toml`
  (mode `0600`); state: `~/.local/state/forge-bot/`.
* `shylock-bot` is only a collaborator, so the webhook cannot be created by the
  bot; `[poller] enabled = true` is used, with an empty `repositories` list so
  every repository visible to the bot is watched (new repositories included).
  Register the hook as `shylock` (or another admin) if you want push delivery,
  then disable the poller.
* A mention can only be answered on repositories where the bot can read the
  comments and post a reply; inaccessible repositories are skipped.
* Keep the token in the process environment only.

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
| `config.example.toml` (repo root) | Annotated config, per-user by default |
| [`doc/forgejo-webhook.md`](doc/forgejo-webhook.md) | Hook scopes, events and API examples |
| `contrib/forge-bot.system.service` | systemd **system** unit |
| `contrib/install-system.sh` | Install the root-owned system service and per-user executor |
| `contrib/forge-bot.env.example` | Environment/secret template |
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
  its own; `[session] workers` is the single total agent count. An idle process
  is reused for any conversation in the same workspace/user/model. By default
  `session_per_conversation = true` keeps one session per conversation: before
  prompting, a reused process is switched back to the session file that
  conversation used earlier, or starts a fresh session (`new_session`) for a
  brand-new thread, so a new thread never inherits another conversation's
  context; set it to `false` to reuse idle processes across conversations in a
  workspace and carry their earlier session;
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

forge-bot runs as a root-controlled system service and executes every agent as
the `[users.*].host_user` it is addressed as. The gateway forks natively, moves
the child into a per-run cgroup v2 and drops to that account before `exec`;
there is no external `systemd-run` or `systemctl` process.

```bash
sudo cargo build --release
sudo ./contrib/install-system.sh
$EDITOR /etc/forge-bot/forge-bot.toml    # configure the [users.*] accounts
sudo systemctl restart forge-bot
```

The installer creates `/etc/forge-bot`, `/var/lib/forge-bot/state`
(mode `0700`, root-only), and `/var/log/forge-bot`, installs the unit as
`/etc/systemd/system/forge-bot.service`, and leaves the config/env files for the
operator to edit. The unit points `FORGE_BOT_SESSION_DIR` at the state path so
state survives a restart. Checkouts are not kept under `/var/lib`: each explicit
`[users.*]` run checks out directly in its `host_user`'s home. Before starting,
create the non-root accounts named in
`host_user` and add at least one `[users.*]` table. The executor rejects unknown
accounts, root, and duplicate UIDs at startup, and passes secrets in the child's
environment rather than on a command line. The host must mount cgroup v2 at
`/sys/fs/cgroup` (`forge-bot check` verifies the cgroup root; whole-cgroup
cancellation needs `cgroup.kill`, Linux 5.14+). Agents are normal processes for
their account and can run external commands with that user's permissions; run
`loginctl enable-linger <host_user>` if an agent also needs a user D-Bus or
`systemctl --user` session.

Authenticate each agent CLI as its configured `host_user`. Signing in as root
or as the gateway operator does not authenticate another Linux account, and
the gateway does not inherit provider credentials from the operator's shell.
For Claude, an administrator can run these commands outside the service
sandbox (replace `reviewer` and the binary path with the installed account/CLI):

```bash
sudo -u reviewer -H /usr/bin/claude auth login
sudo -u reviewer -H /usr/bin/claude auth status
```

Repeat provider setup for every CLI that may run as a fallback. A Forgejo
token only grants forge access; it does not authenticate a model provider.

Each run is isolated by identity: the addressed user's `token` is used for the
agent environment and gateway replies, the checkout is rooted in the addressed
user's `host_user` home (`<home>/<owner__repo-number>`), and
job/session/backend-session keys are
namespaced by `user_id`. Set a `token` on every non-default user; the default
user falls back to the global `[forgejo] token`. `agent_model` applies only to
the user's configured adapter (or the registry default when omitted); other
adapters keep their own model defaults. Cancelling a run writes `cgroup.kill` to stop the whole run
cgroup.

## 5. Verify

```bash
curl -s http://127.0.0.1:8080/healthz                  # ok
FORGEJO_WEBHOOK_SECRET=... ./contrib/test-webhook.sh   # signed delivery
```

Then post `@<bot> reply with pong` from an allowed user and check that a reply
comment appears. The first mention spawns a `pi --mode rpc` agent; later
mentions reuse it while it is idle.

## 6. Operations

* **Logs**: `journalctl -u forge-bot -f`, or the mirrored file
  `/var/log/forge-bot/forge-bot.log`.
* **State**: `/var/lib/forge-bot/state/` (`jobs/`, `sessions/`, `poller.json`).
* **Upgrade**: rebuild, reinstall the binary, restart the service.
* **Uninstall**: stop and remove the unit/binary/config/state.

## 7. Troubleshooting

| Symptom | Fix |
| --- | --- |
| Hook creation says *"owner or admin write"* | Token is only a collaborator. Use an owner token or the poller. |
| Webhook `401` | Secret mismatch or missing `X-Forgejo-Signature`. |
| Forgejo cannot deliver to `127.0.0.1` | Allow loopback in `[webhook] ALLOWED_HOST_LIST`, or use the poller. |
| Poller never triggers | `poller.enabled = false`, or the token cannot see the repository. Reset `state/poller.json` if a cursor ran ahead. |
| `failed to spawn pi` (or `agy`) | The agent's `~/.local/bin` is not on the unit `PATH`. The executor prepends it; otherwise set `[pi_rpc] command` to an absolute path. |
| `No available agent` | The agent binary is missing or the account cannot execute it; check `journalctl -u forge-bot`. |
| No reply comment | `[reply] result = false`, or the token lacks `write:issue`. |
| Jobs pile up | Raise `[session] workers`, the single cap on concurrent agent runs (pooled `pi-rpc` and one-shot alike). |

## 8. This environment

* Bot runs as a root-controlled system service; each agent runs as its
  configured `[users.*].host_user`.
* Binary: `/usr/local/bin/forge-bot`; config: `/etc/forge-bot/forge-bot.toml`
  (mode `0600`); state: `/var/lib/forge-bot/state/`.
* `shylock-bot` is only a collaborator, so the webhook cannot be created by the
  bot; `[poller] enabled = true` is used, with an empty `repositories` list so
  every repository visible to the bot is watched (new repositories included).
  Register the hook as `shylock` (or another admin) if you want push delivery,
  then disable the poller.
* A mention can only be answered on repositories where the bot can read the
  comments and post a reply; inaccessible repositories are skipped.
* Keep the token in the process environment only.

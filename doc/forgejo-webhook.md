# Registering Forgejo webhooks

How to point Forgejo at forge-bot so `@<bot>` mentions are delivered as
webhooks. There are four hook scopes; pick the broadest one you have rights
for. A **system** (or user/org) hook covers repositories created later, so you
do not have to touch it again.

| Scope | Covers | Who can create it |
| --- | --- | --- |
| Repository | one repository | repository owner or admin |
| Organization | every repository in an organization | organization owner |
| User | every repository owned by a user | that user |
| System | every repository on the instance | instance administrator |

The hook target is always the bot's webhook route:

```text
http://<bot-host>:8080/webhooks/forgejo
```

`127.0.0.1` is fine when Forgejo and the bot run on the same host.

---

## Before you start

1. The bot is running and reachable from the Forgejo server
   (`curl http://<bot-host>:8080/healthz`).
2. You have a shared secret. Generate one and put the **same** value in the
   bot config (`[forgejo] webhook_secret`, or `FORGEJO_WEBHOOK_SECRET`) and in
   the hook:

   ```bash
   openssl rand -hex 32
   ```

3. The hook creator has the required rights (table above) and, for the API, a
   token with enough scope:
   * repository hook: `write:repository` on the repo (owner/admin),
   * organization hook: org admin,
   * user hook: the user's own token,
   * system hook: an instance-admin token (`write:admin` scope).

---

## Shared request values

All four scopes send the same payload; only the endpoint differs. Put the
target URL and secret in the environment first:

```bash
export FORGEJO_URL=http://127.0.0.1:3000
export FORGEJO_WEBHOOK_SECRET=$(openssl rand -hex 32)
export BOT_URL=http://127.0.0.1:8080/webhooks/forgejo

# Reuse this body for every scope below.
export HOOK_BODY='{
  "type": "forgejo",
  "active": true,
  "events": ["issue_comment", "pull_request_comment", "pull_request_review_comment", "pull_request_review_approved", "pull_request_review_rejected", "issues", "pull_request", "action_run_failure", "push"],
  "config": {
    "url": "'"$BOT_URL"'",
    "content_type": "json",
    "secret": "'"$FORGEJO_WEBHOOK_SECRET"'",
    "http_method": "post"
  }
}'
```

Copy `FORGEJO_WEBHOOK_SECRET` into the bot config/EnvironmentFile as well.

---

## Helper script

[`contrib/register-webhook.sh`](../contrib/register-webhook.sh) creates hooks
for any scope. It defaults to a **user-level** hook:

```bash
# user-level: every repository owned by the token's user (default)
./contrib/register-webhook.sh

# a single repository (REPO defaults to the current git remote)
SCOPE=repo ./contrib/register-webhook.sh

# an organization
SCOPE=org ORG=my-org ./contrib/register-webhook.sh

# whole instance (instance-admin token)
SCOPE=system ./contrib/register-webhook.sh
```

It validates its inputs and prints the required scope/role when Forgejo
rejects the request. Set `HOOK_ID=<id>` to `PATCH` an existing hook instead of
creating a new one (useful after changing `EVENTS`):

```bash
# list hooks to find the id, then update it
curl -s -H "Authorization: token $TOKEN" "$FORGEJO_URL/api/v1/user/hooks"
HOOK_ID=3 ./contrib/register-webhook.sh
```

---

## 1. Repository webhook

**UI:** Repository → **Settings → Webhooks → Add Webhook → Forgejo**.

**API:**

```bash
export TOKEN=...        # owner/admin token
export REPO=owner/repo

curl -X POST \
  -H "Authorization: token $TOKEN" \
  -H "Content-Type: application/json" \
  "$FORGEJO_URL/api/v1/repos/$REPO/hooks" \
  -d "$HOOK_BODY"
```

## 2. Organization webhook

**UI:** Organization → **Settings → Webhooks → Add Webhook → Forgejo**.

**API:**

```bash
export TOKEN=...        # organization owner token
export ORG=my-org

curl -X POST \
  -H "Authorization: token $TOKEN" \
  -H "Content-Type: application/json" \
  "$FORGEJO_URL/api/v1/orgs/$ORG/hooks" \
  -d "$HOOK_BODY"
```

## 3. User webhook (all repositories of one user)

The cheapest instance-wide option when the repositories belong to one account.

**UI:** User **Settings → Webhooks → Add Webhook → Forgejo**.

**API:**

```bash
export TOKEN=...        # that user's own token

curl -X POST \
  -H "Authorization: token $TOKEN" \
  -H "Content-Type: application/json" \
  "$FORGEJO_URL/api/v1/user/hooks" \
  -d "$HOOK_BODY"
```

## 4. System webhook (whole instance)

**UI:** **Site Administration → System Webhooks → Add Webhook → Forgejo**.

**API:**

```bash
export TOKEN=...        # instance-admin token

curl -X POST \
  -H "Authorization: token $TOKEN" \
  -H "Content-Type: application/json" \
  "$FORGEJO_URL/api/v1/admin/hooks" \
  -d "$HOOK_BODY"
```

This fires for every repository on the instance, including ones created later.

---

## Events

Forgejo lets a hook subscribe to several events. The body above enables the
ones that can carry a mention:

| Event | Payload | forge-bot status |
| --- | --- | --- |
| `issue_comment` | issue + PR conversation comments | handled |
| `pull_request_comment` | PR conversation comments | handled |
| `pull_request_review_comment` | a submitted review with no verdict (body + inline comments) | handled |
| `pull_request_review_approved` | an approving review (body + inline comments) | handled |
| `pull_request_review_rejected` | a requesting-changes review (body + inline comments) | handled |
| `issues` | issue opened/edited (description) | handled |
| `pull_request` | PR opened/edited (description) | handled |

A submitted review is delivered under one of the three
`pull_request_review_*` subscription names, chosen by the review verdict
(plain, approved or requesting changes). Forgejo then renames the event in the
`X-Forgejo-Event` header to `pull_request_comment`, `pull_request_approved` or
`pull_request_rejected`. Subscribe to all three so a mention in any review is
delivered; forge-bot handles each name.

Forgejo's review payload only carries the review body; it does not inline the
individual review comments. When the bot sees a review it fetches the newest
review's comments through the API to find mentions in them, so the token needs
read access to the repository.

A mention in an inline review comment is answered **in the same thread**: the
bot posts the acknowledgement and the agent's summary back as review comments
on the same review, file and line, instead of appending a top-level comment to
the pull request. The review-comment endpoint has no edit route, but Forgejo
updates a review comment through the generic issue-comment endpoint, so the
acknowledgement and any fallback notices are edited into the same review
comment in place. This needs write access to the repository, which is the same
permission required to comment at all.

Description events are deduplicated by a hash of the body, so an edit that
changes the text triggers once while re-deliveries of the same text are
ignored. Events without a mention or supported automatic trigger are accepted
(`202` with `accepted: 0`) and ignored.

## Automatic CI and conflict triggers

The webhook's scope determines which repositories send events, and its event
selection determines which automatic triggers are delivered. Execution also requires
explicit `[policy].auto_allowed_repos` and `auto_allowed_pr_authors` allow-lists;
both default to empty (deny). `allow_all` does not enable automatic work, and
`allowed_repos` restrictions still apply. Only allow PR authors whose code you
trust with the bot credentials, including authors of fork PRs. Subscribe to
`action_run_failure` for Forgejo Actions failures, and `pull_request` plus
`push` for merge conflicts. These triggers run without a mention and require a
configured Forgejo API token and webhook secret. The normal mention
authorization policy continues to apply to mentions.

`action_run_failure` handles Forgejo Actions runs associated with a current
open pull request. A run for an older PR head is ignored. `pull_request` events
and pushes to a PR's base branch check the current PR API `mergeable` value and
start an agent only when it is `false` and the PR is not a draft. An automatic
trigger has no mention to select an adapter, so it continues the thread's
conversation by reusing the adapter that last ran there; a thread with no
history uses the registry default. Forgejo reports `mergeable = false` both for
a real conflict and while its asynchronous conflict check is still running, so
the bot re-checks a candidate for a short, bounded window before acting; a
branch that merges cleanly settles to `mergeable = true` and is left alone.
The bot remembers processed run IDs and head/base commit pairs in
`auto-triggers.json` under the session directory, including across restarts.

When a candidate still reports `mergeable = false` after that window, the bot
asks Forgejo to merge the base branch into the head branch before starting an
agent. A branch that actually merges cleanly is updated (or is already current)
and no agent is started, so a PR that merely looked conflicting while Forgejo
was still checking never gets an agent run or a "no conflict" comment. Only a
`409` from the update, which means a real conflict, starts the agent.

When updating an existing hook, add `action_run_failure` and `push` to its
events. The registration script includes them in its default event list.

---

## Loopback / local addresses

By default Forgejo may refuse to deliver to `127.0.0.1` or private addresses.
If Test Delivery fails with a host error, the instance administrator must
allow the target in `app.ini`:

```ini
[webhook]
ALLOWED_HOST_LIST = 127.0.0.1, localhost
# during bring-up only:
# ALLOWED_HOST_LIST = *
```

then restart Forgejo. If that is not an option, keep the bot's poller enabled
instead.

---

## Verify

1. **UI:** use **Test Delivery** on the hook — expect HTTP `202` and
   `{"accepted":0}` (a test payload has no mention).
2. **Locally sign a delivery:**

   ```bash
   FORGEJO_WEBHOOK_SECRET=... ./contrib/test-webhook.sh
   ```

3. **Post a real mention** from an allowed user and watch the bot log:

   ```bash
   journalctl -u forge-bot -f          # or ~/.local/state/forge-bot/forge-bot.log
   ```

   You should see `job queued`, `spawned pi agent`, and a reply comment on the
   issue/PR.

At info level, each delivery logs its event and delivery ID, the extracted
message count, author/repository/PR coordinates, routing skips, and the final
accepted count. Match `delivery_id` to Forgejo's delivery history when a review
returns `202` with `accepted: 0`. Look for `ignored_author`,
`no_configured_recipient`, `duplicate`, or an authorization/routing reason.
Review extraction also reports missing reviews, unsupported review types, and
empty bodies or missing coordinates. Payload bodies, signatures, tokens and
models are not included in these diagnostics.

Requests with a missing or wrong `X-Forgejo-Signature` get `401 Unauthorized`.

---

## Manage hooks

```bash
# repository / organization / user / system
curl -H "Authorization: token $TOKEN" "$FORGEJO_URL/api/v1/repos/$REPO/hooks"
curl -H "Authorization: token $TOKEN" "$FORGEJO_URL/api/v1/orgs/$ORG/hooks"
curl -H "Authorization: token $TOKEN" "$FORGEJO_URL/api/v1/user/hooks"
curl -H "Authorization: token $TOKEN" "$FORGEJO_URL/api/v1/admin/hooks"

# delete (use the id from the list)
curl -X DELETE -H "Authorization: token $TOKEN" \
  "$FORGEJO_URL/api/v1/repos/$REPO/hooks/<id>"
```

---

## Webhook vs poller

Run **one** delivery path per repository:

* Webhooks are push-based (near real-time, no polling load) but need a hook
  with the right scope.
* The poller needs only read access and no hook, at the cost of a polling
  interval and per-repo requests.

Once a webhook covers a repository, set `[poller] enabled = false` in the bot
config (or restrict `[poller] repositories` to the ones still not covered).
Running both paths would deliver the same comment twice, because the webhook
dedupe and the poller cursor are independent.

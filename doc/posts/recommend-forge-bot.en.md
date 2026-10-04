# forge-bot: put coding agents where your team already works

A failing test has an issue. A proposed fix has a pull request. The discussion,
logs, and review comments are already there. Why move the task into another chat
window just to ask a coding agent for help?

[forge-bot](https://forgejo.shylockhg.me/shylock/forge-bot) lets you start that work
from the conversation itself. Mention the configured bot account in an issue or
pull-request comment, describe the task, and the bot dispatches a coding agent.
The agent can inspect the repository and forge discussion, change code, run
checks, push a branch, and reply with the result, subject to its tools and
permissions.

For example, with a bot configured as `@agent`:

```text
@agent investigate the failing test, fix it, and open a pull request
```

The comment becomes the starting point for work that your team can review in
the usual place.

## Your forge, your agent

forge-bot supports Forgejo, GitHub, and GitLab. Its agent adapters include Codex,
Antigravity CLI, Pi, Claude Code, and Kimi, and you can configure a custom command.
That makes it useful if you host your own forge or want to keep using the coding
agent you already know.

You can also select an adapter for a particular request:

```text
@agent --agent=claude review this change and explain any correctness risks
```

By default, adapters use their own model defaults. A configured agent user can
have an explicit model setting; forge-bot does not need to impose one model on
every agent to coordinate the work.

The architecture is deliberately small at the handoff: the gateway verifies
and authorizes the event, then sends the agent the comment's location and
message. The agent decides which context to read and how to carry out the task.

## A conversation that can continue

A useful coding assistant needs more than a one-off prompt. forge-bot keeps
per-thread sessions for supported adapters, so follow-up requests can resume
the conversation. The default Codex, Pi RPC, and Kimi integrations can also
steer an active run when another mention arrives in the same thread. Other
adapters queue the follow-up for the next turn.

Work in a single thread is serialized, while different threads can run in
parallel within a configured worker limit. Capacity failures can fall back to
another available adapter. A status page shows running and queued work, the
agent involved, and recent results.

You can configure separate agent accounts for implementation and review, each
with its own forge identity, Linux account, workspace, and session. forge-bot
instructs an author agent to hand a pull request to a configured reviewer in a
separate comment. This supports a review workflow; it does not guarantee that
an agent will find every defect or complete every requested action.

## Who should try it?

I would recommend forge-bot to maintainers who want to delegate concrete tasks
without moving the discussion away from their repository: investigating a
failure, proposing a small fix, updating documentation, or reviewing a diff.
It is especially interesting for self-hosted Forgejo users and teams that want
to choose their own agent tools.

There is real operational responsibility. forge-bot runs as a root-controlled
service and executes agents as configured non-root Linux users with per-run
cgroups. Agents can execute commands, and several adapters enable automatic
permission approval by default. Account and process separation should not be
treated as a complete sandbox for untrusted code. Start with trusted users and
repositories, scoped forge tokens, and protected status and admin routes.

## Give it a concrete first task

Start with the project's [README](https://forgejo.shylockhg.me/shylock/forge-bot/src/branch/main/README.md),
[example configuration](https://forgejo.shylockhg.me/shylock/forge-bot/src/branch/main/config.example.toml),
and [deployment guide](https://forgejo.shylockhg.me/shylock/forge-bot/src/branch/main/deploy.md).
Configure an agent account and authorization policy, authenticate the agent
CLI, and connect signed webhooks. Polling is also available when you cannot
create a webhook.

Then try a small, reviewable task in a trusted repository. Read the diff, check
the validation results, and continue the discussion in the same thread.

That is the reason to try forge-bot: it makes the issue or pull request a place
where coding work can begin, continue, and come back for review.

# Repository Guidelines

## Agent model selection

**Never change the model of an agent when creating it.** An agent — whether a
new adapter or a spawned process — must run with its own default model.
forge-bot does not select, override, inject, or report models.

The only exception is an explicit model requirement from the user's AT. The AT
is the `agent…` part of an `@agent… do something` mention, i.e. the text that
selects the agent. In that case use exactly the required model and document why.

# Repository Guidelines

## Agent model selection

By default, every adapter and spawned process uses its own model defaults.
forge-bot must not silently select, override, or inject models.

After the explicit-user refactor, `[users.*].agent_model` may select a model
for that configured agent user only. This permission does not apply to
legacy configurations without user tables.

An explicit model requirement in the user's AT—the `agent…` part of an
`@agent… do something` mention—takes precedence for that invocation.
Use exactly the required model and document why.

Models reported by agents may be stored and displayed in forge-bot status.

# Web pages

All browser assets live here: HTML templates (including page styles and admin
script), notification JavaScript and service worker, web app manifest, and PNG
icons. Rust embeds them with `include_str!` / `include_bytes!`, so deployments
still need only the forge-bot binary and changes require rebuilding it.

HTML templates passed to Rust's `format!` use `{name}` (or `{}`) placeholders.
Literal CSS/JavaScript braces in those templates must be doubled (`{{` / `}}`).
The standalone JavaScript and manifest files use ordinary braces. Rust keeps
routing, data preparation, and escaping of dynamic values.

Run `cargo test --all --locked` for rendering and route coverage and
`node contrib/validate-notify-page.mjs` for notification browser regressions.

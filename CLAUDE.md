# heartbeat-rs

Heartbeat-rs — PTY-based session wrapper for headless Claude dispatch.

Rust crate. Wraps Claude in a PTY so it can be driven programmatically from
outside a terminal. Used by the hearth orchestration layer to dispatch companion
agents without an interactive session.

## Runbook

`~/.soren/shelf/guides/headless-orchestration-runbook.md` — how heartbeat-rs
fits into dispatch, PTY mechanics, session lifecycle, and integration with hearth.

## Maintenance Rule

If you change code in this crate, check whether the runbook needs updating.
If the behavior changed, update the runbook. The runbook is only useful if it
matches reality.

# heartbeat-rs

Heartbeat-rs — PTY-based session wrapper for headless Claude dispatch.

Rust crate. Wraps Claude in a PTY so it can be driven programmatically from
outside a terminal. Used by the hearth orchestration layer to dispatch companion
agents without an interactive session.

## Runbook

`~/.soren/shelf/guides/headless-orchestration-runbook.md` — how heartbeat-rs
fits into dispatch, PTY mechanics, session lifecycle, and integration with hearth.

## Maintenance Rule

**Before working in this directory, read the runbook.** It has architecture decisions, known gotchas, and context you need.

If you change code here, update the runbook to match. The runbook is only useful if it matches reality.

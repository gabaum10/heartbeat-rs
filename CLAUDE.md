# heartbeat-rs

PTY-based session wrapper and stop hook for headless Claude Code agent loops.

Two binaries:

- **`heartbeat-stop`** — Claude Code stop hook. Runs at the end of each agent turn. Manages in-flight state, inbox polling, and session recovery signals.
- **`heartbeat-launch`** — PTY launcher. Wraps a Claude process in a PTY so it can be driven programmatically without an interactive terminal. Feature-gated: build with `--features launch`.

The library crate (`heartbeat_rs`) exposes the core primitives shared between both binaries: error types, hook logic, in-flight tracking, inbox handling, PTY spawn layer, and recovery.

## Build

```sh
# Stop hook only (default)
cargo build --release

# Both binaries
cargo build --release --features launch
```

## Test

```sh
# Core tests
cargo test

# Launch tests (requires feature)
cargo test --features launch
```

## Source Layout

| File | Purpose |
|------|---------|
| `src/main.rs` | `heartbeat-stop` binary entry point |
| `src/launch.rs` | `heartbeat-launch` binary entry point |
| `src/lib.rs` | Library root, module exports |
| `src/hook.rs` | Stop hook logic |
| `src/in_flight.rs` | In-flight session state tracking |
| `src/inbox.rs` | Inbox polling |
| `src/pty.rs` | PTY spawn layer (launch feature) |
| `src/recover.rs` | Session recovery |
| `src/error.rs` | Error types |

## Maintenance Rule

**Before working in this codebase, read the source layout above and check `CHANGELOG.md` for recent changes.** Architecture decisions and known gotchas live in code comments.

If you change behavior, update `CHANGELOG.md` and verify any callers still work.

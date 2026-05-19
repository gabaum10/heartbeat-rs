# Error Handling Strategy for heartbeat-rs

## What This Is

A design + build task. Raccoons debate the error handling strategy first, then build it.

## The Crate

heartbeat-rs — a stop-hook dispatch library/binary for Claude Code. Converts headless CLI dispatch to interactive sessions via a JSONL inbox + byte-offset cursor + stop-hook state machine.

**Published as:** `heartbeat-rs` on crates.io (v0.2.0)
**It is both a library and a binary.**

### Source Layout

| File | What It Does |
|------|-------------|
| `src/main.rs` | CLI binary (clap). Parses args, calls `hook::run()` |
| `src/hook.rs` | The `run()` state machine — core loop. Reads inbox, manages stop-hook protocol, handles in-flight state |
| `src/inbox.rs` | Byte-offset JSONL reader with deferred-ack. `read_next()`, offset management, `offset_file_for()` |
| `src/in_flight.rs` | Crash-recovery artifact. Write/check/remove `.in-flight` files |
| `src/recover.rs` | Orphan recovery: retry, deadletter, or drop entries that were in-flight when a previous run crashed |
| `src/lib.rs` | Library re-exports for consumers |

### Two Modes

- **drain:** Exits when inbox is empty after processing. Used by timer-triggered consumers (fen email triage, scrapyard).
- **persist:** Idles when inbox is empty, keeps checking. Used by long-running supervisors.

## Current Error Handling State

- `io::Result` return types in inbox.rs
- `.unwrap_or(0)` on offset reads (silent fallback)
- Three production `.unwrap()` calls in inbox.rs (lines ~195, 214, 215)
- No error crate (no anyhow, no thiserror)
- No structured error types
- No consumer-facing error reporting beyond exit codes
- `recover.rs` has its own recovery semantics but no integration with a unified error strategy

## The Design Space

### 1. Error Type Architecture

The crate is both a library and a binary. Where's the boundary?

- **Library errors** (consumers import these): inbox read failures, offset corruption, in-flight conflicts. These need to be typed so consumers can match on them.
- **Binary errors** (CLI only): arg parsing, config issues, stdout/stderr formatting. These can be stringly-typed or use anyhow.

Options: custom error enum with `From` impls, `thiserror` for the library + `anyhow` for the binary, pure `thiserror` everywhere, or something else.

### 2. Failure Semantics by Component

What happens when each thing breaks?

| Component | Failure | Current Behavior | Question |
|-----------|---------|-----------------|----------|
| Inbox read | Corrupt JSONL, bad UTF-8, truncated line | Unclear | Skip entry? Deadletter? Bail? |
| Offset file | Missing | `.unwrap_or(0)` — starts from beginning | Is silent restart correct? Should it warn? |
| Offset file | Corrupt (non-numeric content) | `.unwrap_or(0)` — silent fallback | Same question |
| `.in-flight` | Can't write | Unclear | Block processing? Log and continue? |
| `.in-flight` | Can't read/remove | Unclear | Orphan recovery handles some of this |
| Stop hook output | Stdout write fails | Would panic | How should the binary handle this? |
| Mode: drain | All entries processed, inbox empty | Clean exit | Correct, but should it report what it processed? |
| Mode: persist | Idle tick, nothing to do | Continues | Correct |

### 3. Recovery vs Propagation Rules

- **Silent recovery:** Fix it, continue, don't bother the consumer. Good for transient issues (offset file missing → start from 0).
- **Loud recovery:** Fix it, continue, but LOG what happened. Good for things that shouldn't happen but aren't fatal.
- **Propagation:** Don't fix it. Return an error to the caller. Good for things the consumer needs to know about.
- **Fatal:** Bail immediately. Good for things that mean the state is unrecoverable.

The strategy needs rules for when each applies.

### 4. Consumer Contract

Consumers are bash scripts and Rust CLIs that spawn heartbeat-stop as a subprocess. They care about:

- **Exit codes:** What does 0 vs 1 vs other mean?
- **Stderr:** Is there structured diagnostic output, or just raw error messages?
- **Partial processing:** If 3 of 5 inbox entries process successfully and the 4th fails, what state is the inbox in? Can the consumer retry safely?

## Constraints

- The library API is published on crates.io. Breaking changes need a version bump.
- Consumers (hearth, omnitool/normandy) currently expect the existing function signatures. Additive changes are free; signature changes need migration.
- `recover.rs` already has recovery semantics — the strategy should compose with it, not replace it.
- Stop hook protocol: stdout is sacred (JSON decisions to Claude). Errors go to stderr or structured diagnostics, never stdout.
- The `.unwrap()` calls are the most urgent targets — those are crashes waiting to happen in production.

## Done Looks Like

- A custom error type (enum or thiserror-derived) covering all failure modes
- Every `.unwrap()` in non-test code replaced with proper error handling
- `.unwrap_or(0)` calls evaluated — keep if the silent fallback is correct, replace if it's hiding bugs
- Clear failure semantics per component (the table above filled in with decisions)
- Consumer contract documented: exit codes, stderr behavior, partial-processing guarantees
- All existing tests still pass
- New tests for error paths (at minimum: corrupt inbox, missing offset, bad UTF-8 input)

## Test Suite

```bash
cargo test
```

All tests must pass before and after. New error-path tests are expected.

# Build Spec — Error Handling Strategy for heartbeat-rs

**Source:** Raccoon consensus proposal (proposal.md), sharpened by DJ.
**Target:** v0.3.0 (breaking change — library function signatures change).
**Mode:** Swarm. Each raccoon owns one or more modules. El-notario assembles.

---

## 0. What DJ Changed vs the Proposal

1. **Two new variants added** — proposal punted on "other IO errors when reading .in-flight" and "IO errors reading offset (not parse)". Both punts violate the proposal's own "discriminated variants only" principle. Added: `OffsetRead { path, source }` and `InFlightRead { path, source }`. Final enum has **9 variants**, not 7.
2. **Work-order claim corrected** — work order says "three production `.unwrap()` calls in inbox.rs (lines ~195, 214, 215)". Verified: line 195 is `.unwrap_or(Path::new("."))` (already safe); lines 214–215 are inside `#[cfg(test)]`. The ONLY actual production-code crash risk is the `.unwrap_or(0)` calls at inbox.rs:74 and inbox.rs:142, which silently swallow corrupt-offset bugs. These are the real targets.
3. **write_offset atomicity verified** — inbox.rs:181–189 uses tmp + fsync + rename. Proposal's partial-processing guarantee §4 holds.
4. **Module decomposition for the swarm build** added (§9).
5. **Explicit error.rs content** included so every raccoon's worktree converges on byte-identical content (§1).

Everything else from the proposal stands.

---

## 1. The Canonical `src/error.rs`

Every raccoon writes EXACTLY this file. No variations.

```rust
//! Error types for the heartbeat-rs library.
//!
//! Library functions return `Result<T, HeartbeatError>`. The binary
//! translates errors into exit codes and stderr messages. Discriminated
//! variants only — no catch-all `Io(io::Error)`, because the entire point
//! of a typed error is matchability.

use std::io;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum HeartbeatError {
    /// IO error while reading the inbox.
    #[error("inbox read error: {0}")]
    InboxRead(#[source] io::Error),

    /// Offset file exists but its content is not a valid u64.
    /// `content` is the trimmed string we tried to parse.
    #[error("offset file corrupt at {path}: expected numeric offset, got {content:?}")]
    OffsetCorrupt { path: PathBuf, content: String },

    /// IO error while reading the offset file (other than `NotFound`).
    #[error("offset file read error at {path}: {source}")]
    OffsetRead {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// IO error while writing the offset file.
    #[error("offset file write error at {path}: {source}")]
    OffsetWrite {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// `.in-flight` file exists but its JSON is corrupt.
    #[error(".in-flight corrupt at {path}: {source}")]
    InFlightCorrupt {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    /// IO error while reading the `.in-flight` file (other than `NotFound`).
    #[error(".in-flight read error at {path}: {source}")]
    InFlightRead {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// IO error while writing the `.in-flight` file.
    #[error(".in-flight write error at {path}: {source}")]
    InFlightWrite {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// `.responded` present without `.in-flight` — recovery state inconsistent.
    #[error(
        "inconsistent state: .responded present without .in-flight at {io_dir}; \
         run `heartbeat-stop recover`"
    )]
    InconsistentState { io_dir: PathBuf },

    /// IO error while writing the dead-letter file during orphan recovery.
    #[error("dead-letter write error at {path}: {source}")]
    DeadLetterWrite {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Library-wide result alias.
pub type Result<T> = std::result::Result<T, HeartbeatError>;
```

---

## 2. `src/lib.rs` Additions

Add to the existing lib.rs (do not delete existing re-exports):

```rust
pub mod error;
pub use error::{HeartbeatError, Result};
```

---

## 3. `Cargo.toml` Diff

```toml
[package]
# ...existing fields...
version = "0.3.0"   # was 0.2.0

[dependencies]
clap = { version = "4", features = ["derive"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
sha2 = "0.10"
hex = "0.4"
thiserror = "2"     # NEW
```

---

## 4. Filled-in Failure Table (Final, Build From This)

| Component                 | Failure                                | Decision              | Variant / Behavior                                                                 |
|---------------------------|----------------------------------------|-----------------------|------------------------------------------------------------------------------------|
| Inbox read                | File `NotFound`                        | Silent                | `Ok(None)` (no entry)                                                              |
| Inbox read                | Other IO error                         | Propagate             | `Err(InboxRead(e))`                                                                |
| Inbox read                | Bad UTF-8 / truncated line             | Silent recovery       | `from_utf8_lossy` + byte-level newline search; existing behavior, no change       |
| Offset file               | `NotFound`                             | Silent                | `read_offset` → `Ok(None)`; callers use `0`                                       |
| Offset file               | Exists but non-numeric                 | Propagate             | `Err(OffsetCorrupt { path, content })`                                            |
| Offset file               | Other IO error (read)                  | Propagate             | `Err(OffsetRead { path, source })`                                                |
| Offset file               | Write error (tmp/rename/fsync)         | Propagate             | `Err(OffsetWrite { path, source })`                                               |
| `.in-flight` write        | Any IO error                           | Propagate             | `Err(InFlightWrite { path, source })`                                             |
| `.in-flight` read         | `NotFound`                             | Silent                | `Ok(None)`                                                                         |
| `.in-flight` read         | Corrupt JSON                           | Propagate             | `Err(InFlightCorrupt { path, source })`                                           |
| `.in-flight` read         | Other IO error                         | Propagate             | `Err(InFlightRead { path, source })`                                              |
| `.in-flight` remove       | Any error                              | Best-effort           | `let _ = fs::remove_file(...)` — stale `.in-flight` is recoverable via cursor    |
| Stop hook stdout          | Write fails, `BrokenPipe`              | Silent / exit 0       | Parent dead, decision moot                                                         |
| Stop hook stdout          | Write fails, anything else             | Fatal / exit 1        | `eprintln!("heartbeat-stop: fatal: stdout write failed: {e}"); exit(1)`           |
| `.responded` w/o `.in-flight` | Inconsistent state                | Propagate             | `Err(InconsistentState { io_dir })`                                               |
| Dead-letter write         | Any IO error                           | Propagate             | `Err(DeadLetterWrite { path, source })`                                           |
| Mode: drain — inbox empty | Normal                                 | No change             | `Ok(Decision::Approve)` (fail-open default)                                       |
| Mode: persist — idle tick | Normal                                 | No change             | `Ok(Decision::IdleTick)`                                                          |

**Note on corrupt-offset semantics:** the library never decides to "warn + fall back to 0" — that's a presentation choice. The library returns `Err(OffsetCorrupt)` and lets the caller decide. The binary (`main.rs` and the `recover` subcommand) decide for themselves whether to warn-and-continue or abort. Default for `hook::run`: propagate to caller (which is `main.rs`), which fail-opens. Default for `recover::recover`: propagate, which exits 1. See §6 for the exact mapping.

---

## 5. Recovery vs Propagation Principles

1. **Silent recovery** — failure is indistinguishable from a correct first-run state AND the safe default is unambiguously correct (missing inbox, missing offset).
2. **Propagate as typed error** — failure reveals invalid state that cannot be safely auto-corrected without risking data loss, or the caller must make a policy decision (corrupt offset, .in-flight write failure, dead-letter write failure, corrupt .in-flight, inconsistent state).
3. **Best-effort `let _ =`** — for removal operations where the artifact's continued presence is recoverable by existing mechanisms (`.in-flight` removal in `acknowledge`).
4. **Binary-level fatal** — continuing would violate the stop hook protocol or corrupt external state (stdout write failure other than BrokenPipe).

**Principle: libraries return typed errors, binaries decide what to log.** No `eprintln!` inside library functions. `main.rs` holds all presentation logic.

---

## 6. Consumer Contract

### Exit Codes (Binary)

| Code | When                                                                                       |
|------|--------------------------------------------------------------------------------------------|
| `0`  | Hook path normal completion (Approve, Block, or IdleTick)                                  |
| `0`  | Hook path error → fail-open with Approve decision (existing behavior preserved)            |
| `0`  | stdout `BrokenPipe` (parent dead, decision moot)                                           |
| `0`  | `recover` subcommand success                                                               |
| `1`  | Missing required arg (`--inbox`)                                                           |
| `1`  | `recover` subcommand error (any `HeartbeatError`)                                          |
| `1`  | stdout write failure other than `BrokenPipe`                                               |

### Stderr Format

Plain-text lines, no JSON. stdout is sacred — never write diagnostics there.

```
heartbeat-stop: error: <HeartbeatError Display>     # hook errors caught in main.rs
heartbeat-stop: warn: <message>                      # loud recoveries (e.g. corrupt offset → 0)
heartbeat-stop: fatal: stdout write failed: <e>      # only this triggers exit 1 from the hook path
heartbeat-stop recover: <RecoveryOutcome:?>          # existing recover output, unchanged
```

### Partial Processing Guarantee

- Entries 0..K-1 processed successfully → cursor advanced past each (deferred-ack).
- Entry K fails:
  - **Before `.in-flight` write:** cursor at `K.start_offset`, no `.in-flight`. Next run re-delivers K.
  - **After `.in-flight` write:** cursor at `K.start_offset`, `.in-flight` present. Next startup's `recover` applies configured orphan policy.
  - **During `write_offset` (offset advance):** `write_offset` is atomic (tmp + fsync + rename; verified inbox.rs:181–189). Old offset is intact on rename failure. Entry K not acknowledged. Next run re-delivers K.
- **Consumers can safely retry after any error.**

### Unsupported

Two hook processes racing on the same inbox is undefined behavior. The last atomic `.in-flight` write wins; Claude Code sees two Block decisions. Not a library bug — document as unsupported.

---

## 7. `read_offset` Signature Change (the migration pattern)

Current: `pub fn read_offset(path: &Path) -> Option<u64>`
New:     `pub fn read_offset(path: &Path) -> Result<Option<u64>>`

Cases:
- `Ok(None)`      — file does not exist (silent: caller uses 0)
- `Ok(Some(n))`   — exists, parsed successfully
- `Err(OffsetCorrupt { path, content })` — exists but content is not a valid u64
- `Err(OffsetRead  { path, source })`    — exists, IO error other than NotFound

Callers that want the warn-and-fallback behavior do it explicitly:

```rust
let start_offset = match inbox::read_offset(offset_file) {
    Ok(None) => 0,
    Ok(Some(n)) => n,
    Err(e) => {
        eprintln!("heartbeat-stop: warn: {e}; restarting from offset 0");
        0
    }
};
```

`main.rs` and `hook::run` should NOT auto-fallback. They should propagate. The `recover` subcommand should also propagate (returns `Err`, exits 1). Inside `inbox::read_next_entry` and `inbox::acknowledge`, propagate too — these are library functions and shouldn't make policy decisions about corruption.

The warn-and-fallback pattern above is the EXPECTED USAGE PATTERN for consumers that want gentle handling, but the library itself never picks `0` silently for a corrupt file.

---

## 8. Preserved Disagreements (For the Record)

These were live debate disagreements that the team resolved. Recorded so the build doesn't reopen them:

- **Io catch-all variant** — REJECTED (lil-grabby proposed, roach + trashboat killed). Discriminated variants only.
- **thiserror in library** — ADOPTED (roach initially opposed, conceded after seeing zero runtime cost and zero API surface).
- **`From<HeartbeatError> for io::Error` shim** — REJECTED (roach proposed to skip semver bump, trashboat killed because it erases variant info). Take the semver bump.
- **stdout BrokenPipe vs other write errors** — SPLIT (lil-grabby's catch). BrokenPipe → exit 0. Other write errors → exit 1.

---

## 9. Build Decomposition (Swarm)

**Six raccoons, six pieces.** Each works in their pre-created worktree. Each writes the canonical `src/error.rs` from §1 verbatim, the `src/lib.rs` additions from §2, and the `Cargo.toml` thiserror addition from §3 — **plus** their assigned module(s). Tests must pass in their own worktree before they hand off.

| Raccoon       | Worktree | Files Owned                                            | Why                                                                 |
|---------------|----------|--------------------------------------------------------|---------------------------------------------------------------------|
| el-notario    | 1        | `src/error.rs` (canonical), `src/lib.rs`, `Cargo.toml` | Contracts & API surface — el-notario's lane. Also assembles later. |
| roach         | 2        | `src/inbox.rs`                                         | Production-critical migration; defensive code is roach's lens.     |
| trashboat     | 3        | `src/hook.rs`                                          | State machine; structural reframing if `?` propagation needs care. |
| garbaggio     | 4        | `src/in_flight.rs`, `src/recover.rs`                   | Composes with existing recovery semantics — divergent thinking.    |
| one-ply       | 5        | `src/main.rs`                                          | Tight stdout BrokenPipe block; minimalism wins.                    |
| lil-grabby    | 6        | NEW error-path tests, integrated into `src/*.rs` `#[cfg(test)]` modules | Edge cases & constraint coverage. |

### Interface Contract (every raccoon must respect)

Every raccoon's worktree MUST end with the following invariants. Failure to meet any of these counts as RED for that raccoon.

1. `src/error.rs` content is byte-for-byte the §1 enum. Do not deviate.
2. `Cargo.toml` has `thiserror = "2"` in `[dependencies]` and `version = "0.3.0"`.
3. `src/lib.rs` has `pub mod error;` and `pub use error::{HeartbeatError, Result};` added — but otherwise unchanged.
4. Every public library function that previously returned `io::Result<T>` now returns `heartbeat_rs::Result<T>` (i.e. `Result<T, HeartbeatError>`).
5. `cargo build` succeeds in their worktree.
6. `cargo test` passes in their worktree (existing tests retyped, plus their new ones if any).
7. No `eprintln!`, `println!`, or `print!` inside library functions (only in `main.rs`).
8. No `.unwrap()` or `.expect()` outside `#[cfg(test)]` blocks.
9. `let _ = fs::remove_file(...)` is the only acceptable form for `.in-flight` removal in `acknowledge`.

### Module-Specific Instructions

#### El-notario (worktree-1) — error.rs + lib.rs + Cargo.toml + the lib-export contract

- Write `src/error.rs` per §1 verbatim.
- Add the lines from §2 to `src/lib.rs`.
- Edit `Cargo.toml` per §3.
- Confirm `cargo build --lib` succeeds with just these changes (it should — error.rs is self-contained).
- The other raccoons' work depends on these files being canonical. Your job is to make them.

#### Roach (worktree-2) — inbox.rs

- Change `read_offset` per §7.
- Change `read_next_entry`: `io::Result<Option<InboxEntry>>` → `Result<Option<InboxEntry>>`. Replace the line-74 `read_offset(...).unwrap_or(0)` with explicit match (propagate `OffsetCorrupt` and `OffsetRead`).
- Change `acknowledge`: `io::Result<()>` → `Result<()>`. Replace the line-142 `read_offset(...).unwrap_or(0)` with explicit match. Map `write_offset` errors to `OffsetWrite`. Use `let _ = fs::remove_file(in_flight_path)` for the .in-flight removal.
- Change `write_offset`: `io::Result<()>` → `Result<()>`. Map errors to `OffsetWrite { path, source }`. Keep tmp + fsync + rename.
- Map all `io::Error` from inbox file operations (`File::open`, `read`, etc.) to `HeartbeatError::InboxRead`.
- Update the existing `#[cfg(test)]` assertions so types compile. Do not change test logic.

#### Trashboat (worktree-3) — hook.rs

- Change `run` return type: `io::Result<Decision>` → `Result<Decision>`.
- Replace `io::Error::new(io::ErrorKind::InvalidData, "inconsistent state...")` with `HeartbeatError::InconsistentState { io_dir: io_dir.to_owned() }`.
- Map `.in-flight` write errors (`inflight.write_to(...)`) to `InFlightWrite` if not already done in `in_flight.rs` (garbaggio owns that file; coordinate via the canonical signatures below).
- `?` operators propagate `HeartbeatError` directly — no `From` impls needed.
- Update `#[cfg(test)]` assertions for the new return type.

#### Garbaggio (worktree-4) — in_flight.rs + recover.rs

- `in_flight.rs`:
  - Change `write_to`: `io::Result<()>` → `Result<()>`. Map errors to `InFlightWrite { path, source }`.
  - Change `read_from`: `io::Result<Option<Self>>` → `Result<Option<Self>>`. `NotFound` → `Ok(None)`. `serde_json::Error` → `Err(InFlightCorrupt)`. Other IO → `Err(InFlightRead)`.
- `recover.rs`:
  - Change `recover` return type: `io::Result<RecoveryOutcome>` → `Result<RecoveryOutcome>`.
  - Replace `read_offset(...).unwrap_or(0)` with explicit match.
  - Map dead-letter write errors to `HeartbeatError::DeadLetterWrite { path, source }`.
  - `RecoveryOutcome` enum stays success-only — DO NOT add error variants there. Errors come back via `Err(HeartbeatError::...)`.
- Update `#[cfg(test)]` assertions.

#### One-Ply (worktree-5) — main.rs

The stdout-write hardening, kept tight. Replace `print!("{}", output)` (wherever it appears in the hook path) with:

```rust
use std::io::Write;

if let Err(e) = std::io::stdout().write_all(output.as_bytes()) {
    if e.kind() == std::io::ErrorKind::BrokenPipe {
        std::process::exit(0);
    }
    eprintln!("heartbeat-stop: fatal: stdout write failed: {e}");
    std::process::exit(1);
}
```

Update the hook error handler to format `HeartbeatError` via its `Display` impl:

```rust
match hook::run(&inbox, &mode) {
    Ok(decision) => { /* existing stdout path */ }
    Err(e) => {
        eprintln!("heartbeat-stop: error: {e}");
        // fail-open: emit Approve decision, exit 0 (existing behavior)
    }
}
```

For the `recover` subcommand, propagate errors with exit 1:

```rust
match recover::recover(&inbox, policy) {
    Ok(outcome) => { eprintln!("heartbeat-stop recover: {outcome:?}"); }
    Err(e) => { eprintln!("heartbeat-stop recover: {e}"); std::process::exit(1); }
}
```

Keep it minimal. No unnecessary helpers.

#### Lil-Grabby (worktree-6) — NEW error-path tests

Add tests for every new error variant. Place them in the appropriate file's `#[cfg(test)]` module (NOT new files unless really needed). Required tests:

1. **`inbox.rs`**: corrupt offset file → `read_next_entry` returns `Err(HeartbeatError::OffsetCorrupt { path, content })` with the correct path and content captured.
2. **`inbox.rs`**: corrupt offset file → `acknowledge` returns the same `Err(OffsetCorrupt)`.
3. **`inbox.rs`**: existing `invalid_utf8_before_newline_cursor_stays_aligned` and `invalid_utf8_no_newline_cursor_stays_aligned` retyped — still must pass.
4. **`in_flight.rs`**: corrupt `.in-flight` JSON → `read_from` returns `Err(HeartbeatError::InFlightCorrupt)`.
5. **`recover.rs`**: corrupt offset → `recover` returns `Err(HeartbeatError::OffsetCorrupt)`.
6. **`recover.rs`** (existing) `recover_errors_on_corrupt_in_flight` — verify it now returns the typed variant.

Optional but encouraged:
- **`inbox.rs`**: offset file with extra whitespace/newlines parses successfully (regression: trim before parse).
- **`hook.rs`**: `.responded` without `.in-flight` → `run` returns `Err(InconsistentState { io_dir })`.

You also coordinate the `read_offset` semantics — verify the explicit-match pattern from §7 in your own worktree by writing one consumer-style test that demonstrates warn-and-fallback.

---

## 10. Assembly Notes (For El-Notario, Phase 2)

When you assemble (separate dispatch, after the swarm completes), the canonical pickup is:

| File                    | Pick from worktree |
|-------------------------|--------------------|
| `src/error.rs`          | 1 (el-notario)     |
| `src/lib.rs`            | 1 (el-notario)     |
| `src/inbox.rs`          | 2 (roach)          |
| `src/hook.rs`           | 3 (trashboat)      |
| `src/in_flight.rs`      | 4 (garbaggio)      |
| `src/recover.rs`        | 4 (garbaggio)      |
| `src/main.rs`           | 5 (one-ply)        |
| `Cargo.toml`            | 1 (el-notario)     |
| `Cargo.lock`            | regenerate         |
| Test additions          | 6 (lil-grabby) — merge into the appropriate files' `#[cfg(test)]` modules |

After stitching:
1. Run `cargo build`.
2. Run `cargo test`.
3. Run `cargo clippy --all-targets -- -D warnings` (best effort — if it complains about untouched code, ignore; if it complains about the new code, fix).
4. Commit only `src/`, `Cargo.toml`, `Cargo.lock` (NOT `.scrapyard-*`, `.claude/`, `tasks/`).

Target branch: `scrapyard/2026-05-19-114203` in `/home/gabaum10/projects/heartbeat-rs`.

---

## 11. Done When

- All 9 enum variants in `src/error.rs` as specified.
- No `.unwrap()` outside `#[cfg(test)]` in any `src/*.rs`.
- No `.unwrap_or(0)` on `read_offset` results (replaced with explicit match).
- All existing tests pass after type migration.
- New tests from §9 (lil-grabby) added and passing.
- `cargo build` clean.
- `cargo test` GREEN.
- Branch `scrapyard/2026-05-19-114203` committed against target repo.

---

## Build Decomposition (from debate)

# Decomposition Plan — Error Handling Strategy Build

## Subtasks

1. **error-contract**: Write canonical `src/error.rs`, update `src/lib.rs` to re-export, bump `Cargo.toml` to v0.3.0 with thiserror. → **el-notario** (worktree-1). Why: contracts are el-notario's lane; also the file every other raccoon depends on for compilation.

2. **inbox-migration**: Migrate `src/inbox.rs` to typed errors — `read_offset` signature change, replace `.unwrap_or(0)` with explicit match, map all IO errors to discriminated variants. → **roach** (worktree-2). Why: production-critical, defensive code is roach's lens.

3. **hook-migration**: Migrate `src/hook.rs` — `run` return type, replace stringly-typed `io::Error::new(InvalidData,...)` with `InconsistentState`. → **trashboat** (worktree-3). Why: state machine; structural reframing if `?` propagation gets weird.

4. **in_flight+recover-migration**: Migrate `src/in_flight.rs` and `src/recover.rs` together. Both compose with existing recovery semantics (`RecoveryOutcome` enum stays). → **garbaggio** (worktree-4). Why: composes with the bespoke recovery machinery — needs someone who'll explore the seam, not just type-rewrite.

5. **main-stdout-hardening**: Migrate `src/main.rs` — stdout BrokenPipe handling, `HeartbeatError` display, recover-subcommand error mapping. → **one-ply** (worktree-5). Why: small tight block of code; minimalism wins.

6. **error-path-tests**: New tests in `#[cfg(test)]` modules of each `src/*.rs` for corrupt offset, corrupt .in-flight, inconsistent-state. → **lil-grabby** (worktree-6). Why: edge-case constraint hunter; this is the test-writing slot.

## Integration Order

All 6 raccoons run in parallel. Each writes a SELF-CONTAINED change in their worktree that compiles + passes tests independently. They duplicate the canonical `src/error.rs`, `src/lib.rs` additions, and `Cargo.toml` changes from §1–3 of the spec — verbatim, byte-for-byte.

Assembly (el-notario, separate dispatch) picks one canonical file per slot per the table in spec.md §10.

## Interface Contracts (Strict)

Every raccoon's worktree at end-of-task must satisfy ALL of these:

1. `src/error.rs` byte-identical to spec §1.
2. `Cargo.toml`: `thiserror = "2"`, `version = "0.3.0"`.
3. `src/lib.rs`: `pub mod error;` + `pub use error::{HeartbeatError, Result};` added; rest unchanged.
4. `cargo build` succeeds.
5. `cargo test` GREEN (existing tests retyped + any new ones).
6. No `eprintln!`/`println!`/`print!` in library code (anything under `src/` except `main.rs`).
7. No `.unwrap()`/`.expect()` outside `#[cfg(test)]`.
8. Public lib API: every former `io::Result<T>` → `heartbeat_rs::Result<T>`.

## Why This Decomposition

- The canonical `error.rs` is small (~70 lines) — every raccoon writes it, so there's no serialization bottleneck.
- Modules are sized roughly proportional to raccoon strength: roach gets the longest module (inbox.rs, ~700 lines), one-ply gets the smallest (main.rs, ~60 lines).
- Garbaggio's two files (in_flight.rs + recover.rs) share recovery semantics, so coupling them in one head avoids inconsistency at the seam.
- Lil-grabby owns NEW tests across all files — she doesn't migrate any module, she ADDS coverage for the new error variants. Other raccoons retype existing tests in their own files; she writes the new ones.

## Risks to Watch

- Two raccoons might converge on slightly different `error.rs` content despite the spec — el-notario must pick one as canonical at assembly time.
- Lil-grabby's tests in `inbox.rs#[cfg(test)]` overlap with roach's retyped tests in the same module — el-notario must merge both sets of test fns at assembly time, not pick one.
- One-ply's main.rs depends on `main.rs` having a specific current shape — must read it before migrating.
- Garbaggio might go off-script on `recover.rs` and try to redesign `RecoveryOutcome` — spec explicitly forbids this. Hold the line at assembly.

---

## Build Instructions

This spec was produced by a raccoon debate. The decomposition above assigns each raccoon their piece. Use SWARM mode — each raccoon gets their assigned subtask. Do NOT use compete mode. The architecture decisions are made; execute them.

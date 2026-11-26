# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Fixed

- A fail-open Approve (the hook errored) under idle keepalive no longer touches
  the `--signal-file`. Touching it ended the run, and on a queued
  `soren-headless` run that silently dropped the rest of the queue (launcher
  exit 0). With the file untouched the session idles, heartbeat-launch's
  keepalive starts a new turn, and the next Stop re-delivers. `heartbeat-launch`
  now sets `HEARTBEAT_IDLE_KEEPALIVE=1` in the child's environment when
  `--idle-timeout > 0` (and removes any inherited value otherwise);
  `heartbeat-stop` reads it. Bounded: after 3 consecutive skipped touches
  (counted in `.fail-open-skips` beside the inbox, cleared by any pass that
  does not error) the next error touches the file and ends the run as before,
  so a persistent error cannot loop. Without the variable (no keepalive) the
  fail-open path is unchanged: Approve plus signal touch. Because the variable
  comes from the launcher, no hook command or settings change is needed.
  Each skip costs one real keepalive turn (up to 3 per persistent error), and
  `--max-idle-retries` below 3 can end the run first (exit 125).
- Offset and `.in-flight` writes no longer share a fixed `<file>.tmp` name.
  Two processes writing the same directory (hook and launcher) could truncate
  or rename each other's temp, leaving an empty or missing `.inbox-offset` /
  `.in-flight`, which surfaced as a fail-open Approve and an exit-signal touch
  that ends a live run. Each write now uses a temp unique to the writer
  (pid plus counter, created exclusively, in the same directory so the rename
  stays atomic) and removes it on failure.
- `.responded` present without `.in-flight` (an operator removing `.in-flight`,
  or a crash between the two removals in an acknowledge) used to return
  `InconsistentState` on every pass until `recover` ran. The hook now clears
  `.responded` on the pass that detects it and still returns the error for
  that one pass (fail-open, as before); the next pass is an ordinary turn-0
  pass and delivers the entry at the cursor. An entry the agent already
  processed can therefore be delivered again (at-least-once); use `recover`
  first to drop or dead-letter it instead. The `InconsistentState` message now
  says this, and the `.in-flight write error` message also covers
  `.responded`. No public API, CLI flag, or exit code changed.

## [0.6.0] - 2026-11-05

### Changed

- `heartbeat-launch` no longer reports success when the child exits 0 after an
  idle keepalive it never recovered from. If at least one keepalive was
  injected and the session produced no genuine output after it (output at or
  after the keepalive plus the 20s grace window; earlier output is the PTY
  echoing the injected bytes), a child exit of 0 now exits **122** with a
  `heartbeat-launch:` stderr line. Claude Code's bypass-mode startup disclaimer
  reads the keepalive's ESC as "exit" and exits 0, which previously looked like
  a completed pass. A non-zero child code is still forwarded unchanged, with a
  stderr note. Runs that never inject a keepalive (including the default
  `--idle-timeout 0`) are unchanged. The `--exit-signal` path is unchanged.
  Known limit: a child that genuinely recovers and then exits 0 on its own
  within 20s of the keepalive reads as unrecovered (122).
- `pty::RunResult` gains the public field `unrecovered_keepalive: bool` and is
  now `#[non_exhaustive]`, so later fields won't be a breaking change.

## [0.5.6] - 2026-10-05

Also covers 0.5.5.

### Added

- `--version` on both `heartbeat-launch` and `heartbeat-stop`.

### Fixed

- Idle keepalive in `heartbeat-launch` sends a single ESC, waits 1s, then the
  prompt and CR. The previous back-to-back ESC ESC landed inside Claude Code's
  800ms double-press window, opening the Rewind picker and losing the prompt;
  the gap also stops the ESC merging with the prompt's first character into an
  Alt-key chord.
- Idle watchdog in `heartbeat-launch` (`src/pty.rs`) can exhaust again. A
  keepalive injection used to overwrite the reader's last-output timestamp, so
  once the grace window passed the retry counter reset with no real output and
  the maximum idle retries was never reached. Injections are now tracked
  separately, silence is measured from the later of real output and the last
  injection, and the counter resets only when output arrives after the
  injection's grace window.

## [0.5.0] - 2026-05-31

### Removed

- PTY queue mode: `--queue`, `--queue-sentinel`, `--queue-boot-delay`,
  `--queue-entry-template`, `--queue-done-message` flags and the
  `run_with_queue` / `QueueConfig` / `QueueState` / sentinel-reader internals (#10).
  Prompt delivery is now exclusively the heartbeat-stop inbox/Stop-hook path.

## [0.4.0] - 2026-05-20

### Added

- **`heartbeat-launch` binary** — a thin PTY wrapper that spawns an arbitrary command inside a real pseudo-terminal. Designed to satisfy Claude Code's `isTTY` check so it runs in interactive `cli` mode rather than `sdk-cli` mode.
- **`portable-pty` integration** — cross-platform PTY allocation via `portable-pty` (Unix PTY + Windows ConPTY). Handles stdout forwarding, child exit polling, and configurable timeout with SIGKILL on expiry.
- **Feature gate: `launch`** — `heartbeat-launch` and its dependencies (`portable-pty`, `anyhow`) are compiled only when `--features launch` is passed. The default build (`heartbeat-stop` only) remains dependency-minimal.
- **`--timeout` flag for `heartbeat-launch`** — seconds before the child is killed (SIGKILL). `0` means no timeout. Defaults to 3600s.
- **`--cwd` flag for `heartbeat-launch`** — working directory for the child process. Defaults to `.`.
- **`--exit-signal` flag for `heartbeat-launch`** — path to a signal file. When this file exists, `heartbeat-launch` sends SIGTERM to the child and removes the file, allowing external coordination of clean session shutdown.
- **`--signal-file` flag for `heartbeat-stop`** — path to a signal file. The stop hook writes this file when it decides to approve the stop, letting launchers detect clean drain completion without polling.

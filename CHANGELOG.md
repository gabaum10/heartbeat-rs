# Changelog

All notable changes to this project will be documented in this file.

## [0.4.0] - 2026-05-20

### Added

- **`heartbeat-launch` binary** — a thin PTY wrapper that spawns an arbitrary command inside a real pseudo-terminal. Designed to satisfy Claude Code's `isTTY` check so it runs in interactive `cli` mode rather than `sdk-cli` mode.
- **`portable-pty` integration** — cross-platform PTY allocation via `portable-pty` (Unix PTY + Windows ConPTY). Handles stdout forwarding, child exit polling, and configurable timeout with SIGKILL on expiry.
- **Feature gate: `launch`** — `heartbeat-launch` and its dependencies (`portable-pty`, `anyhow`) are compiled only when `--features launch` is passed. The default build (`heartbeat-stop` only) remains dependency-minimal.
- **`--timeout` flag for `heartbeat-launch`** — seconds before the child is killed (SIGKILL). `0` means no timeout. Defaults to 3600s.
- **`--cwd` flag for `heartbeat-launch`** — working directory for the child process. Defaults to `.`.

# heartbeat-rs

A Rust stop hook for autonomous Claude Code agent loops. Drains a JSONL message inbox across multiple agent turns without a persistent supervisor process.

## What It Does

Claude Code supports a [stop hook](https://docs.anthropic.com/en/docs/claude-code/hooks) -- a command that runs after every agent response and controls whether the session continues or ends. `heartbeat-rs` implements a message-queue dispatch pattern on top of this hook:

1. An external process writes prompts to a JSONL inbox file.
2. Claude Code starts an interactive session.
3. After each agent response, the stop hook reads the next message from the inbox and injects it as the next user turn.
4. When the inbox is empty, the hook approves the stop and the session exits cleanly.

This turns Claude Code's interactive session model into a message-driven automation runtime. Each session gets full tool access, clean context, and the same execution environment as a human-operated session -- but the messages come from a file queue instead of a keyboard.

## How It Works

Claude Code's `.claude/settings.json` supports a `Stop` hook. The hook's stdout determines what happens next:

- Output `{"decision":"block","reason":"<message>"}` -- the session continues; `reason` becomes the next user turn.
- Output nothing -- the session ends (stop approved).

`heartbeat-stop` implements a state machine around this protocol. It reads from a JSONL inbox file at a byte offset. A `.responded` flag file bridges turns, and a `.in-flight` artifact bridges sessions.

**Deferred-acknowledgement design:** The offset cursor advances only when the agent acknowledges an entry (on the next hook tick after a response), not when the entry is first read. This eliminates the silent-drop window where a launcher or agent crash between delivery and acknowledgement would lose the entry. A `.in-flight` file records the entry in transit so crash recovery can distinguish stale orphans from live ones.

## Installation

Once published to crates.io:

```bash
# Install heartbeat-stop only (default, dependency-minimal)
cargo install heartbeat-rs

# Install both heartbeat-stop and heartbeat-launch (requires PTY support)
cargo install heartbeat-rs --features launch
```

Build from source:

```bash
git clone https://github.com/gabaum10/heartbeat-rs
cd heartbeat-rs

# heartbeat-stop only
cargo build --release
cp target/release/heartbeat-stop ~/.local/bin/

# heartbeat-stop + heartbeat-launch
cargo build --release --features launch
cp target/release/heartbeat-stop ~/.local/bin/
cp target/release/heartbeat-launch ~/.local/bin/
```

After cloning, activate the git hooks:

```bash
git config core.hooksPath .githooks
```

This enables the `cargo fmt` pre-commit check. Run `cargo fmt` before committing if the hook rejects your changes.

## Usage

```bash
# Stop hook (call from .claude/settings.json)
heartbeat-stop --inbox /path/to/inbox.jsonl --mode drain
heartbeat-stop --inbox /path/to/inbox.jsonl --mode persist
heartbeat-stop --inbox /path/to/inbox.jsonl --mode persist --idle-interval 300

# Orphan recovery (call from your launcher before resetting the inbox)
heartbeat-stop recover --inbox /path/to/inbox.jsonl --on-orphan deadletter
heartbeat-stop recover --inbox /path/to/inbox.jsonl --on-orphan retry
heartbeat-stop recover --inbox /path/to/inbox.jsonl --on-orphan drop
```

### heartbeat-launch (requires `--features launch`)

`heartbeat-launch` spawns an arbitrary command inside a PTY. Its primary use is ensuring Claude Code detects a real TTY and runs in interactive `cli` mode rather than `sdk-cli` mode.

```bash
# Basic: launch a command inside a PTY (1-hour default timeout)
heartbeat-launch -- claude --model claude-opus-4-5 "Read CLAUDE.md"

# With explicit working directory and timeout
heartbeat-launch --cwd /path/to/agent/workspace --timeout 7200 -- claude "Read CLAUDE.md"

# No timeout (run until the command exits)
heartbeat-launch --timeout 0 -- claude "Read CLAUDE.md"
```

**`heartbeat-launch` is not a session manager.** It allocates a PTY, spawns the command, streams stdout, and exits when the command exits. Everything else -- inbox setup, `settings.json`, stop hook wiring, orphan recovery -- is the consumer's responsibility.

| Flag | Default | Description |
|------|---------|-------------|
| `--cwd <dir>` | `.` | Working directory for the child process. |
| `--timeout <secs>` | `3600` | Seconds before the child is killed (SIGKILL). `0` means no timeout. |

Exit codes mirror the child process. Timeout exits with code `124` (same convention as `timeout(1)` on Linux).

## Modes

| Mode | Behavior |
|------|----------|
| `drain` | Approves stop when the inbox is empty. The agent processes all queued messages, then exits. Use for timer-triggered dispatch, batch processing, or single-event sessions. |
| `persist` | Sends idle ticks when the inbox is empty, keeping the session alive indefinitely. Use for long-running supervisor patterns where new messages may arrive at any time. |

## Flags

| Flag | Default | Description |
|------|---------|-------------|
| `--inbox <path>` | required | Path to the JSONL inbox file. |
| `--mode <mode>` | `drain` | Operating mode: `drain` or `persist`. |
| `--idle-interval <seconds>` | `2` | Seconds to sleep between consecutive idle ticks in `persist` mode. Only applies when the inbox is empty. The first inbox check is always immediate; this delay governs the gap between idle ticks. Set higher (e.g. `300`) for consumers where the inbox is populated infrequently. |

**Important:** `--idle-interval` causes the hook process to sleep inside the hook invocation. Ensure your `"timeout"` value in `.claude/settings.json` is greater than `--idle-interval`, or Claude Code will kill the hook before the sleep completes. For a 300-second interval, set `"timeout": 310` or higher. Messages arriving during the sleep wait until the next hook invocation.

## Configuration

In the agent workspace `.claude/settings.json`:

```json
{
  "hooks": {
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "heartbeat-stop --inbox /path/to/agent/inbox.jsonl --mode drain",
            "timeout": 30
          }
        ]
      }
    ]
  }
}
```

For a persistent supervisor with a 5-minute idle interval:

```json
{
  "hooks": {
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "heartbeat-stop --inbox /path/to/agent/inbox.jsonl --mode persist --idle-interval 300",
            "timeout": 310
          }
        ]
      }
    ]
  }
}
```

## Writing to the Inbox

Single-line messages can be written directly:

```bash
echo "triage these emails" >> /path/to/inbox.jsonl
```

Multi-line messages must be JSON-encoded so the inbox stays valid JSONL (one entry per line):

```bash
PROMPT="Check the dashboard\nThen summarize what needs attention"
echo "$PROMPT" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read().rstrip("\n")))' >> /path/to/inbox.jsonl
```

The hook detects the leading `"` and unwraps the JSON string before delivery, so Claude receives the original multi-line content.

## Architecture: Launcher + Hook

`heartbeat-rs` is the dispatch layer, not the launcher. Each consumer writes a wrapper script that handles their specific trigger and starts the session. The crate handles everything after launch.

```
┌─────────────────────────┐
│  LAUNCHER (your script) │  Polls for work, writes to inbox, starts claude
└───────────┬─────────────┘
            │ optionally via heartbeat-launch (PTY wrapper)
┌───────────▼─────────────┐
│  CLAUDE CODE SESSION    │  Reads CLAUDE.md, responds
└───────────┬─────────────┘
            │ stop hook fires after every response
┌───────────▼─────────────┐
│  heartbeat-stop (hook)  │  Reads inbox, delivers or drains
└─────────────────────────┘
```

### PTY layer (`heartbeat-launch`)

Claude Code checks whether its stdout is a TTY to decide whether to run in interactive `cli` mode (full UI, tool rendering) or headless `sdk-cli` mode. When launched from a script, there is no TTY and Claude defaults to `sdk-cli`. `heartbeat-launch` allocates a real PTY via `portable-pty` and spawns the child inside it, so Claude's `isTTY` check succeeds.

The PTY layer is thin: it allocates the pair, spawns the command on the slave side, drops the slave so the master sees EOF on child exit, and runs a background thread to forward the master's output to the caller's stdout. The main thread polls for child exit in a 100ms loop and enforces the configurable timeout.

`heartbeat-launch` is feature-gated (`--features launch`) to keep the default binary's dependency footprint minimal. Scripts that don't need TTY allocation can use `heartbeat-stop` directly with `claude --print` or in environments where a TTY is already present.

**The launcher is load-bearing.** It's where you poll for new work (IMAP, ticket API, file watcher, CI webhook), format the prompt, write to the inbox, and start `claude`. Different use cases write different launchers. The hook binary is the same everywhere.

### Example: Timer-Triggered Email Triage

```bash
#!/bin/bash
AGENT_DIR="/path/to/agent"
INBOX="$AGENT_DIR/inbox.jsonl"

# Step 1: recover any orphan from a prior crashed session BEFORE resetting.
# Use 'drop' if your upstream source (e.g., IMAP) is the retry mechanism.
heartbeat-stop recover --inbox "$INBOX" --on-orphan drop 2>> triage.log || true

# Step 2: reset inbox for fresh cycle.
> "$INBOX"
echo -n "0" > "$AGENT_DIR/.inbox-offset"

# Step 3: write new work and launch.
EMAILS=$(poll-imap --once)
echo "$EMAILS" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))' >> "$INBOX"
cd /path/to/agent/workspace
claude --allowedTools "..." "Read CLAUDE.md"
```

### Example: Event-Driven CI Integration

```bash
#!/bin/bash
AGENT_DIR="/path/to/agent"
INBOX="$AGENT_DIR/inbox.jsonl"

# Recover before reset. Use 'deadletter' if duplicate processing is unacceptable.
heartbeat-stop recover --inbox "$INBOX" --on-orphan deadletter 2>> ci.log || true

echo "Review the PR diff at $PR_URL and check for security issues" >> "$INBOX"
cd /path/to/agent/workspace
claude --allowedTools "Bash,Read" "Read CLAUDE.md"
```

## State Machine (drain mode — Fix B)

```
Per-entry lifecycle:

  [queued]      offset < EOF, no .in-flight
      |
      | hook reads entry, writes .in-flight, touches .responded
      v
  [in-flight]   .in-flight exists, .responded exists, offset at entry start
      |
      | agent responds, hook fires next tick
      v
  [acknowledged] hook advances offset, removes .in-flight + .responded
      |
      | hook reads next entry or approves stop
      v
  [completed]   offset past entry, no on-disk state
```

The `.responded` flag bridges turns. The `.in-flight` artifact bridges sessions.

**Key difference from the original design:** the offset cursor does NOT advance on read. It advances on acknowledge (the next hook tick after the agent responds). This means:

- If the launcher or agent crashes between delivery and acknowledgement, the entry is NOT silently lost.
- At next startup, `.in-flight` is present and the launcher can apply recovery policy.
- The cursor value now means "everything before this byte was **acknowledged**," not just "delivered."

## Orphan Recovery

When a session ends without completing an acknowledgement (launcher killed, agent timeout, hook IO error), the `.in-flight` file persists. The next launcher cycle must check for it before resetting the inbox.

Call `heartbeat-stop recover` before truncating the inbox:

```bash
heartbeat-stop recover --inbox "$INBOX" --on-orphan deadletter
```

### Orphan Policies (`--on-orphan`)

| Policy | Behavior | Use when |
|--------|----------|----------|
| `deadletter` (default) | Appends orphan to `.dead-letter.jsonl`, advances cursor | Duplicate side effects are unacceptable; operator reviews dead-letter |
| `retry` | Resets cursor to `start_offset` of orphan so next session re-delivers it | Work is idempotent, or the agent never actually saw the entry |
| `drop` | Advances cursor past orphan, deletes `.in-flight` | Upstream source (IMAP, ticket API) is the retry mechanism |

**Retry policy detail:** The orphan entry is already in `inbox.jsonl` at its original position — `recover` runs before the launcher truncates the inbox. The `retry` policy does NOT copy or prepend anything. It walks the cursor back to `start_offset` so the hook re-delivers the same bytes on the next session start. This means N crash-and-retry cycles leave the inbox unchanged in size; the same entry is re-offered each time. If the agent's work is not idempotent, use `deadletter` instead.

**WARNING — launchers using `retry` policy MUST preserve `inbox.jsonl` across cycles. Do NOT truncate the inbox after `recover`.** The retry semantic depends on the orphan bytes remaining at `start_offset`. Truncating erases them silently and permanently — there is no upstream source to recover from (that is why `retry` was chosen over `drop`). The truncate-and-reset pattern shown in the Fen example launcher below only works with `drop` and `deadletter` policies.

```bash
# BROKEN — silently loses the orphan that retry just preserved
heartbeat-stop recover --inbox "$INBOX" --on-orphan retry
> "$INBOX"                          # orphan bytes erased here
echo -n "0" > "$AGENT_DIR/.inbox-offset"

# CORRECT — write new work into the existing inbox; retry re-delivers orphan first
heartbeat-stop recover --inbox "$INBOX" --on-orphan retry
# Do NOT truncate. Append new entries after the existing content if needed.
echo "$NEW_WORK" >> "$INBOX"
```

**`recover` is the single cleanup point for all inbox-side session artifacts.** On every successful path it removes both `.in-flight` AND `.responded`. Launchers do not need to remove `.responded` separately — calling `recover` before the next session is sufficient:

```bash
heartbeat-stop recover --inbox "$INBOX" --on-orphan retry
# No rm .responded needed — recover handles it.
cd "$WORKSPACE" && claude ...
```

**`drop` caveat:** the orphan's `raw_line` is not preserved anywhere. If there is no upstream retry source, the entry is lost. Only use `drop` when an external system (IMAP, ticket queue) will re-surface the work on the next poll.

### Stale vs. Live Orphans

`recover` distinguishes two cases automatically:

- **Live orphan** — cursor is at or before `start_offset` (entry was never acknowledged). Apply the configured policy.
- **Stale orphan** — cursor has reached or passed `end_offset` (`current_offset >= end_offset`). The entry was acknowledged in step 1 of the ack sequence but `.in-flight` removal was interrupted. Silently delete `.in-flight`. No policy action needed — the entry was already processed.

### Concurrency contract

**`recover` must not run concurrently with itself or with a live hook session.**

Running two `recover` invocations in parallel on the same inbox dir is unsafe: both read `.in-flight`, both attempt to apply policy, one will hit a "file not found" error on `.in-flight` removal and exit non-zero, and `deadletter` entries will be duplicated. The launcher's PID-file lock (`$AGENT_DIR/.wrapper.pid`) prevents the realistic case where a launcher cycle overlaps with a running session. Do not invoke `recover` manually while a session is active.

If your policy is `retry` or `deadletter`, do NOT use `|| true` to swallow recover's exit code — a recover failure (e.g., corrupt `.in-flight`) should halt the cycle so the orphan is not silently discarded. For `drop` policy (where loss is acceptable), `|| true` is safe.

## On-Disk Artifacts

| File | Purpose | Lifecycle |
|------|---------|-----------|
| `inbox.jsonl` | The message queue | Written by launcher, read by hook |
| `.inbox-offset` | Byte cursor (acknowledged position) | Written by hook on acknowledge |
| `.responded` | "Agent just replied" signal | Touched on delivery, removed on next tick |
| `.in-flight` | Entry currently being processed | Written on delivery, removed on acknowledge |
| `.dead-letter.jsonl` | Orphans moved by deadletter policy | Append-only; operator drains manually |

**IMPORTANT for launcher authors:** Do NOT delete `.in-flight` in your failure branch. If `claude` exits non-zero, preserve `.in-flight` and let the next cycle's `recover` call handle it. Deleting `.in-flight` in the failure path defeats the entire safety property of Fix B.

```bash
# WRONG: this silently drops the orphan
if cd "$WORKSPACE" && claude ...; then
    # success
else
    rm -f "$AGENT_DIR/.in-flight"  # DON'T DO THIS
fi

# RIGHT: let the next cycle's recover call handle the orphan
if cd "$WORKSPACE" && claude ...; then
    # success
else
    echo "session failed — orphan will be handled on next cycle" >> "$LOG"
fi
# At the top of the next cycle:
heartbeat-stop recover --inbox "$INBOX" --on-orphan deadletter
```

**IMPORTANT:** Each consumer must use its own `$AGENT_DIR`. Two launchers sharing the same inbox directory will corrupt each other's state. One directory, one consumer, full stop.

## Use Cases

- **Timer-driven automation.** A cron job or systemd timer polls for work, queues prompts, and launches a session that processes them and exits.
- **Event-driven processing.** A webhook handler or file watcher writes to the inbox; the session handles the event and shuts down.
- **Batch dispatch.** Queue multiple prompts, launch once, drain them all in sequence with clean context boundaries between messages.
- **CI/CD integration.** Trigger agent sessions from pipeline steps for code review, test analysis, or deployment verification.
- **Persistent supervisor.** In `persist` mode, keep a session alive to handle messages as they arrive over an extended period.

## Security Notes

- No network access. Reads one local file, writes one flag file and one offset file.
- Byte offset is written atomically with `fsync` and a rename -- crash-safe, no torn writes.
- `.in-flight` is written atomically (tmp + fsync + rename). Same crash safety.
- On IO error, the hook approves the stop (fail-open) and logs to stderr. Safer than blocking indefinitely.
- Inbox content is delivered verbatim to Claude. Sanitize prompts at the write site.

## Credits

Inspired by [claude-heartbeat](https://github.com/Siigari/claude-heartbeat). The core algorithm -- byte-offset JSONL consumption, `.responded` flag state machine, block/approve protocol -- is adapted from that project's JS implementation. This crate extracts the stop hook primitive as a focused, dependency-minimal Rust binary with hardened IO and a test suite.

## License

MIT -- see [LICENSE](LICENSE).

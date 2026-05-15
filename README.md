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

**Fix B (current):** The offset cursor is deferred -- it advances only when the agent acknowledges an entry (on the next hook tick), not when the entry is first read. This eliminates the silent-drop window where a launcher or agent crash between delivery and acknowledgement would lose the entry. A `.in-flight` file records the entry in transit so crash recovery can distinguish stale orphans from live ones.

## Installation

Once published to crates.io:

```bash
cargo install heartbeat-rs
```

Build from source:

```bash
git clone https://github.com/gabaum10/heartbeat-rs
cd heartbeat-rs
cargo build --release
cp target/release/heartbeat-stop ~/.local/bin/
```

## Usage

```bash
# Stop hook (call from .claude/settings.json)
heartbeat-stop --inbox /path/to/inbox.jsonl --mode drain
heartbeat-stop --inbox /path/to/inbox.jsonl --mode persist

# Orphan recovery (call from your launcher before resetting the inbox)
heartbeat-stop recover --inbox /path/to/inbox.jsonl --on-orphan deadletter
heartbeat-stop recover --inbox /path/to/inbox.jsonl --on-orphan retry
heartbeat-stop recover --inbox /path/to/inbox.jsonl --on-orphan drop
```

## Modes

| Mode | Behavior |
|------|----------|
| `drain` | Approves stop when the inbox is empty. The agent processes all queued messages, then exits. Use for timer-triggered dispatch, batch processing, or single-event sessions. |
| `persist` | Sends idle ticks when the inbox is empty, keeping the session alive indefinitely. Use for long-running supervisor patterns where new messages may arrive at any time. |

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
            │ launches interactive session
┌───────────▼─────────────┐
│  CLAUDE CODE SESSION    │  Reads CLAUDE.md, responds
└───────────┬─────────────┘
            │ stop hook fires after every response
┌───────────▼─────────────┐
│  heartbeat-stop (hook)  │  Reads inbox, delivers or drains
└─────────────────────────┘
```

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
| `retry` | Prepends orphan back to inbox as first entry | Work is idempotent, or the agent never actually saw the entry |
| `drop` | Deletes `.in-flight`, advances cursor | Upstream source (IMAP, ticket API) is the retry mechanism |

### Stale vs. Live Orphans

`recover` distinguishes two cases automatically:

- **Live orphan** — cursor is at `start_offset` (entry was never acknowledged). Apply the configured policy.
- **Stale orphan** — cursor has already advanced past `end_offset` (entry was acknowledged in a prior step, but `.in-flight` removal was interrupted). Silently delete `.in-flight`. No policy needed.

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

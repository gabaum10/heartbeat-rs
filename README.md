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

`heartbeat-stop` implements a state machine around this protocol. It reads from a JSONL inbox file at a byte offset, tracking position atomically so messages are never re-delivered. A `.responded` flag file bridges turns: when a message is delivered, the flag is created; on the next invocation the hook sees the flag, removes it, and checks whether another message is waiting. If yes, deliver and block again. If no, approve -- the session exits cleanly.

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
heartbeat-stop --inbox /path/to/inbox.jsonl --mode drain
heartbeat-stop --inbox /path/to/inbox.jsonl --mode persist
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
# Runs on a systemd timer or cron schedule
EMAILS=$(poll-imap --once)
echo "$EMAILS" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))' >> /path/to/inbox.jsonl
cd /path/to/agent/workspace
claude --allowedTools "..." "Read CLAUDE.md"
```

### Example: Event-Driven CI Integration

```bash
#!/bin/bash
# Triggered by a webhook or file watch
echo "Review the PR diff at $PR_URL and check for security issues" >> /path/to/inbox.jsonl
cd /path/to/agent/workspace
claude --allowedTools "Bash,Read" "Read CLAUDE.md"
```

## Session Flow

```
1. Launcher writes prompt(s) to inbox.jsonl
2. Launcher starts: claude --allowedTools "..." "Read CLAUDE.md"
3. Claude reads CLAUDE.md, responds
4. Stop hook fires: reads inbox.jsonl, delivers prompt, blocks stop
5. Claude processes the prompt, executes tool calls, responds
6. Stop hook fires: .responded flag exists, inbox now empty, approves stop
7. Session exits cleanly
```

## State Machine (drain mode)

```
1. If .responded flag exists:
   - Remove it
   - If inbox has another message: deliver it, set .responded, block
   - If inbox is empty: approve (session ends)
2. If no .responded flag:
   - If inbox has a message: deliver it, set .responded, block
   - If inbox is empty: approve (session ends)
```

The `.responded` flag lets the hook drain a queue across multiple agent turns without keeping a supervisor process alive.

## Use Cases

- **Timer-driven automation.** A cron job or systemd timer polls for work, queues prompts, and launches a session that processes them and exits.
- **Event-driven processing.** A webhook handler or file watcher writes to the inbox; the session handles the event and shuts down.
- **Batch dispatch.** Queue multiple prompts, launch once, drain them all in sequence with clean context boundaries between messages.
- **CI/CD integration.** Trigger agent sessions from pipeline steps for code review, test analysis, or deployment verification.
- **Persistent supervisor.** In `persist` mode, keep a session alive to handle messages as they arrive over an extended period.

## Security Notes

- No network access. Reads one local file, writes one flag file and one offset file.
- Byte offset is written atomically with `fsync` and a rename -- crash-safe, no torn writes.
- On IO error, the hook approves the stop (fail-open) and logs to stderr. Safer than blocking indefinitely.
- Inbox content is delivered verbatim to Claude. Sanitize prompts at the write site.

## Credits

Inspired by [claude-heartbeat](https://github.com/Siigari/claude-heartbeat). The core algorithm -- byte-offset JSONL consumption, `.responded` flag state machine, block/approve protocol -- is adapted from that project's JS implementation. This crate extracts the stop hook primitive as a focused, dependency-minimal Rust binary with hardened IO and a test suite.

## License

MIT -- see [LICENSE](LICENSE).

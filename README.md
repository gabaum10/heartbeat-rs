# heartbeat-rs

A focused Rust stop hook for autonomous Claude Code agent loops — keeps a headless `-p` session alive while an inbox has messages, then exits cleanly.

## The Problem

Anthropic's June 2026 SDK credit separation means `claude -p` (non-interactive dispatch) and interactive sessions now draw from separate credit pools. Headless `-p` calls that need to remain interactive — reading messages, executing tools across multiple turns — can't easily be kept alive without a persistent process or a polling loop. `heartbeat-rs` solves this without either: it runs as a Claude Code `Stop` hook, inspecting a message inbox after every agent turn and either injecting the next message or approving the session's exit.

## How It Works

Claude Code's `.claude/settings.json` supports a `Stop` hook — a command that runs after every Claude response. The hook's stdout determines what happens next:

- Output `{"decision":"block","reason":"<message>"}` — the session continues; `reason` becomes the next user turn.
- Output nothing — the session ends (stop approved).

`heartbeat-stop` implements a state machine around this protocol. It reads from a JSONL inbox file at a byte offset, tracking position atomically so messages are never re-delivered. A `.responded` flag file bridges turns: when a message is delivered, the flag is created; on the next invocation the hook sees the flag, removes it, and checks whether another message is waiting. If yes, deliver and block again. If no, approve — the session exits cleanly.

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
| `drain` | Approves stop when the inbox is empty. Use for timer-triggered or fresh-per-event sessions — the agent runs until all queued messages are processed, then exits. |
| `persist` | Sends idle ticks when the inbox is empty, keeping the session alive indefinitely. Use for a persistent supervisor pattern. |

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

## Typical Session Flow

```
1. External script writes a prompt to inbox.jsonl
2. Session launches: claude --allowedTools "..." "Read CLAUDE.md"
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

## Security Notes

- No network access. Reads one local file, writes one flag file and one offset file.
- Byte offset is written atomically with `fsync` and a rename — crash-safe, no torn writes.
- On IO error, the hook approves the stop (fail-open) and logs to stderr. Safer than blocking indefinitely.
- Inbox content is delivered verbatim to Claude. Sanitize prompts at the write site.

## Credits

Inspired by [claude-heartbeat](https://github.com/Siigari/claude-heartbeat). The core algorithm — byte-offset JSONL consumption, `.responded` flag state machine, block/approve protocol — is adapted from that project's JS implementation. This crate extracts the essential stop hook primitive as a focused, dependency-minimal Rust binary with hardened IO and a test suite.

## License

MIT — see [LICENSE](LICENSE).

//! Stop hook protocol and state machine.
//!
//! Implements the Claude Code stop hook decision protocol:
//!   - Output `{"decision":"block","reason":"<message>"}` → session continues
//!     with `reason` as the next user turn.
//!   - Output nothing (empty stdout) → session ends (stop is approved).
//!
//! The state machine manages multi-message drain via a `.responded` flag file.
//! When a message is delivered, `.responded` is created. On the next hook
//! invocation, `.responded` signals "agent just replied — check for more."
//!
//! State machine (drain mode):
//! ```text
//! 1. If .responded flag exists:
//!    - Remove it (consume the signal)
//!    - If inbox has another message: deliver it, set .responded, block
//!    - If inbox empty: approve (session ends)
//! 2. If no .responded flag:
//!    - If inbox has message: deliver it, set .responded, block
//!    - If inbox empty: approve (session ends)
//! ```
//!
//! Persist mode adds idle ticks when the inbox is empty, keeping the session
//! alive indefinitely for a future persistent supervisor.

use std::fs;
use std::io;
use std::path::Path;

use crate::inbox;

/// Operating mode for the stop hook.
#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    /// Exit session when inbox is drained.
    Drain,
    /// Send idle ticks when inbox is empty (persistent supervisor).
    Persist,
}

/// Output from running the hook state machine.
#[derive(Debug, PartialEq)]
pub enum Decision {
    /// Block the stop: inject `reason` as the next user turn.
    Block(String),
    /// Approve the stop: session ends (empty stdout).
    Approve,
    /// Send idle tick: keep session alive, no real message.
    IdleTick,
}

/// Run the stop hook state machine.
///
/// `inbox_path` — path to the JSONL inbox file.
/// `mode` — drain or persist.
///
/// Returns the decision. The caller is responsible for writing output and
/// exiting.
pub fn run(inbox_path: &Path, mode: &Mode) -> io::Result<Decision> {
    let io_dir = inbox_path.parent().unwrap_or(Path::new("."));
    let responded_flag = io_dir.join(".responded");
    let offset_file = inbox::offset_file_for(inbox_path);

    if responded_flag.exists() {
        // Agent just replied to the previous message. Remove the flag.
        let _ = fs::remove_file(&responded_flag);

        // Check if there's another queued message.
        match inbox::read_next_line(inbox_path, &offset_file)? {
            Some(msg) => {
                touch(&responded_flag)?;
                return Ok(Decision::Block(msg));
            }
            None => {
                // Inbox drained after response. Session ends in drain mode,
                // idle tick in persist mode.
                return Ok(approve_or_idle(mode));
            }
        }
    }

    // No .responded flag — first invocation or post-idle.
    match inbox::read_next_line(inbox_path, &offset_file)? {
        Some(msg) => {
            touch(&responded_flag)?;
            Ok(Decision::Block(msg))
        }
        None => Ok(approve_or_idle(mode)),
    }
}

/// Serialize a decision to the format Claude Code expects on stdout.
///
/// Block: `{"decision":"block","reason":"<message>"}`
/// Approve: empty string (no output)
/// IdleTick: block with a minimal idle marker
pub fn serialize(decision: &Decision) -> String {
    match decision {
        Decision::Block(reason) => {
            let obj = serde_json::json!({
                "decision": "block",
                "reason": reason
            });
            obj.to_string()
        }
        Decision::Approve => String::new(),
        Decision::IdleTick => {
            let obj = serde_json::json!({
                "decision": "block",
                "reason": "--- IDLE TICK ---"
            });
            obj.to_string()
        }
    }
}

fn approve_or_idle(mode: &Mode) -> Decision {
    match mode {
        Mode::Drain => Decision::Approve,
        Mode::Persist => Decision::IdleTick,
    }
}

/// Create or touch a flag file.
fn touch(path: &Path) -> io::Result<()> {
    fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn make_inbox(dir: &TempDir) -> PathBuf {
        dir.path().join("inbox.jsonl")
    }

    fn write_line(inbox: &Path, line: &str) {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(inbox)
            .unwrap();
        writeln!(f, "{}", line).unwrap();
    }

    // Helper: reset the offset file so tests start fresh.
    fn reset_offset(inbox: &Path) {
        let offset = inbox::offset_file_for(inbox);
        let _ = fs::remove_file(&offset);
    }

    // -------------------------------------------------------------------------
    // No .responded flag cases
    // -------------------------------------------------------------------------

    #[test]
    fn no_flag_with_message_blocks() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir);
        reset_offset(&inbox);

        write_line(&inbox, "triage these emails please");

        let decision = run(&inbox, &Mode::Drain).unwrap();
        assert_eq!(
            decision,
            Decision::Block("triage these emails please".to_string())
        );

        // .responded flag should exist
        assert!(dir.path().join(".responded").exists());
    }

    #[test]
    fn no_flag_empty_inbox_approves_in_drain_mode() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir);
        reset_offset(&inbox);

        // No inbox file at all
        let decision = run(&inbox, &Mode::Drain).unwrap();
        assert_eq!(decision, Decision::Approve);
    }

    #[test]
    fn no_flag_empty_inbox_idles_in_persist_mode() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir);
        reset_offset(&inbox);

        let decision = run(&inbox, &Mode::Persist).unwrap();
        assert_eq!(decision, Decision::IdleTick);
    }

    // -------------------------------------------------------------------------
    // .responded flag cases
    // -------------------------------------------------------------------------

    #[test]
    fn flag_with_more_messages_delivers_next() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir);
        reset_offset(&inbox);

        write_line(&inbox, "message one");
        write_line(&inbox, "message two");

        // Simulate: first message was already delivered and consumed.
        // We fast-forward the offset past the first line.
        let offset_file = inbox::offset_file_for(&inbox);
        // Read the first line to advance the offset.
        inbox::read_next_line(&inbox, &offset_file).unwrap();

        // Now set the responded flag as if the agent just replied.
        let responded = dir.path().join(".responded");
        touch(&responded).unwrap();

        let decision = run(&inbox, &Mode::Drain).unwrap();
        assert_eq!(decision, Decision::Block("message two".to_string()));

        // Flag should exist again (ready for next round)
        assert!(responded.exists());
    }

    #[test]
    fn flag_with_empty_inbox_approves_in_drain_mode() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir);
        reset_offset(&inbox);

        write_line(&inbox, "only message");

        // Consume the only message first (advance offset).
        let offset_file = inbox::offset_file_for(&inbox);
        inbox::read_next_line(&inbox, &offset_file).unwrap();

        // Set responded flag as if agent just replied.
        let responded = dir.path().join(".responded");
        touch(&responded).unwrap();

        let decision = run(&inbox, &Mode::Drain).unwrap();
        assert_eq!(decision, Decision::Approve);

        // Flag should be gone
        assert!(!responded.exists());
    }

    #[test]
    fn flag_with_empty_inbox_idles_in_persist_mode() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir);
        reset_offset(&inbox);

        write_line(&inbox, "only message");

        let offset_file = inbox::offset_file_for(&inbox);
        inbox::read_next_line(&inbox, &offset_file).unwrap();

        let responded = dir.path().join(".responded");
        touch(&responded).unwrap();

        let decision = run(&inbox, &Mode::Persist).unwrap();
        assert_eq!(decision, Decision::IdleTick);
    }

    // -------------------------------------------------------------------------
    // Serialization
    // -------------------------------------------------------------------------

    #[test]
    fn block_serializes_correctly() {
        let out = serialize(&Decision::Block("do the thing".to_string()));
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["decision"], "block");
        assert_eq!(parsed["reason"], "do the thing");
    }

    #[test]
    fn approve_serializes_to_empty_string() {
        let out = serialize(&Decision::Approve);
        assert_eq!(out, "");
    }

    #[test]
    fn idle_tick_serializes_as_block() {
        let out = serialize(&Decision::IdleTick);
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["decision"], "block");
    }
}

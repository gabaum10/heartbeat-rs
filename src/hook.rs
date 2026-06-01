//! Stop hook protocol and state machine.
//!
//! Implements the Claude Code stop hook decision protocol:
//!   - Output `{"decision":"block","reason":"<message>"}` → session continues
//!     with `reason` as the next user turn.
//!   - Output nothing (empty stdout) → session ends (stop is approved).
//!
//! ## Fix B state machine (drain mode)
//!
//! ```text
//! Per-entry lifecycle:
//!
//!   [queued]      offset < EOF, no .in-flight present
//!       |
//!       | hook reads entry, writes .in-flight {entry, id}, touches .responded
//!       v
//!   [in-flight]   .in-flight exists, .responded exists, offset still at entry start
//!       |
//!       | agent responds, hook fires next tick
//!       v
//!   [acknowledged] hook removes .in-flight + .responded, advances offset
//!       |
//!       | hook reads next entry or approves stop
//!       v
//!   [completed]   offset past entry, no on-disk state
//! ```
//!
//! The `.responded` flag bridges turns. The `.in-flight` artifact bridges
//! sessions — it lets a new launcher startup detect an orphaned entry and
//! apply the configured recovery policy.
//!
//! Persist mode adds idle ticks when the inbox is empty, keeping the session
//! alive indefinitely for a future persistent supervisor.

use std::fs;
use std::path::Path;
use std::time::Duration;

use crate::error::{HeartbeatError, Result};
use crate::in_flight::{self, InFlightEntry};
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
/// `idle_interval_secs` — seconds to sleep before emitting an idle tick in
///   persist mode. Only applied when the inbox is empty and no real message
///   is pending. The first check of the inbox is always immediate; this delay
///   governs the gap between consecutive idle ticks. Use 0 to disable sleeping
///   (tests and low-latency consumers).
///
/// Returns the decision. The caller is responsible for writing output and
/// exiting.
pub fn run(inbox_path: &Path, mode: &Mode, idle_interval_secs: u64) -> Result<Decision> {
    let io_dir = inbox_path.parent().unwrap_or(Path::new("."));
    let responded_flag = io_dir.join(".responded");
    let offset_file = inbox::offset_file_for(inbox_path);
    let in_flight_path = in_flight::in_flight_file_for(inbox_path);

    if responded_flag.exists() {
        // Agent just replied to the previous message.
        // Acknowledge the in-flight entry: advance cursor + remove .in-flight.
        //
        // Order of writes matches §2 of spec:
        //   1. Read current in-flight to get end_offset for cursor advance.
        //   2. Call acknowledge (advance offset, remove .in-flight).
        //   3. Remove .responded.
        //   4. Read next entry (deferred — no cursor advance yet).
        //   5. If found: write new .in-flight, touch .responded, emit Block.
        //   6. If empty: emit Block /exit (drain) / IdleTick (persist).
        let in_flight_entry = match InFlightEntry::read_from(&in_flight_path)? {
            Some(entry) => entry,
            None => {
                // .responded exists but .in-flight is missing. This is an
                // inconsistent on-disk state — the most likely cause is an
                // operator manually removing .in-flight to "unstick" a session.
                // Silently proceeding would re-deliver the entry at the current
                // cursor position, which the agent already processed.
                //
                // Return an error so the failure is visible in stderr. The
                // hook's fail-open handler in main.rs emits this and approves
                // the stop. The operator (or launcher startup script) should
                // run `heartbeat-stop recover` before launching a new session —
                // recover removes both .in-flight and .responded, fully
                // clearing this inconsistent state.
                // InconsistentState carries io_dir so the operator can find
                // the artifacts that need recovery. The Display impl on
                // HeartbeatError includes the full recovery instruction.
                return Err(HeartbeatError::InconsistentState {
                    io_dir: io_dir.to_owned(),
                });
            }
        };

        // Ack-step 1: advance cursor past the in-flight entry, remove .in-flight.
        inbox::acknowledge(&offset_file, in_flight_entry.end_offset, &in_flight_path)?;

        // Ack-step 2: remove .responded.
        let _ = fs::remove_file(&responded_flag);

        // Step 3: check for next entry.
        match inbox::read_next_entry(inbox_path, &offset_file)? {
            Some(next) => {
                // Deliver next: write .in-flight, touch .responded, emit Block.
                let new_in_flight =
                    InFlightEntry::new(&next.raw_line, next.start_offset, next.end_offset);
                new_in_flight.write_to(&in_flight_path)?;
                touch(&responded_flag)?;
                return Ok(Decision::Block(next.decoded));
            }
            None => {
                // Inbox drained. Session ends in drain mode, idle tick in persist.
                return Ok(approve_or_idle(mode, idle_interval_secs));
            }
        }
    }

    // No .responded flag — first invocation (turn 0, agent reading CLAUDE.md)
    // or post-idle. Check for next entry.
    match inbox::read_next_entry(inbox_path, &offset_file)? {
        Some(entry) => {
            // Deliver: write .in-flight first (for crash recoverability),
            // then touch .responded, then emit Block.
            let new_in_flight =
                InFlightEntry::new(&entry.raw_line, entry.start_offset, entry.end_offset);
            new_in_flight.write_to(&in_flight_path)?;
            touch(&responded_flag)?;
            Ok(Decision::Block(entry.decoded))
        }
        None => Ok(approve_or_idle(mode, idle_interval_secs)),
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

fn approve_or_idle(mode: &Mode, idle_interval_secs: u64) -> Decision {
    match mode {
        Mode::Drain => Decision::Block("/exit".to_string()),
        Mode::Persist => {
            if idle_interval_secs > 0 {
                std::thread::sleep(Duration::from_secs(idle_interval_secs));
            }
            Decision::IdleTick
        }
    }
}

/// Create or touch a flag file (`.responded`).
///
/// Failure mapping note: `.responded` is a delivery-sequence artifact like
/// `.in-flight`. The error enum has no dedicated `.responded` variant — the
/// failure table (§4) doesn't enumerate it as a distinct case. We map to
/// `InFlightWrite` because (a) it's the closest semantic match and (b) the
/// caller treats the delivery sequence as atomic: failing to touch `.responded`
/// is equivalent to failing to write `.in-flight`. The path in the error
/// carries the actual `.responded` path so the operator can diagnose.
fn touch(path: &Path) -> Result<()> {
    fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|source| HeartbeatError::InFlightWrite {
            path: path.to_owned(),
            source,
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::in_flight::InFlightEntry;
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

    fn responded(dir: &TempDir) -> PathBuf {
        dir.path().join(".responded")
    }

    fn in_flight(dir: &TempDir) -> PathBuf {
        dir.path().join(".in-flight")
    }

    fn offset(dir: &TempDir) -> PathBuf {
        dir.path().join(".inbox-offset")
    }

    // -------------------------------------------------------------------------
    // No .responded flag cases (first invocation / turn 0)
    // -------------------------------------------------------------------------

    #[test]
    fn no_flag_with_message_blocks_and_writes_in_flight() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir);

        write_line(&inbox, "triage these emails please");

        let decision = run(&inbox, &Mode::Drain, 0).unwrap();
        assert_eq!(
            decision,
            Decision::Block("triage these emails please".to_string())
        );

        // .responded flag should exist
        assert!(responded(&dir).exists());

        // .in-flight should exist with correct content
        let inf = InFlightEntry::read_from(&in_flight(&dir)).unwrap().unwrap();
        assert_eq!(inf.raw_line, "triage these emails please");
        assert_eq!(inf.start_offset, 0);
    }

    #[test]
    fn no_flag_empty_inbox_approves_in_drain_mode() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir);

        let decision = run(&inbox, &Mode::Drain, 0).unwrap();
        assert_eq!(decision, Decision::Block("/exit".to_string()));
    }

    #[test]
    fn no_flag_empty_inbox_idles_in_persist_mode() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir);

        let decision = run(&inbox, &Mode::Persist, 0).unwrap();
        assert_eq!(decision, Decision::IdleTick);
    }

    // -------------------------------------------------------------------------
    // .responded flag cases (agent just replied)
    // -------------------------------------------------------------------------

    #[test]
    fn flag_with_more_messages_acknowledges_and_delivers_next() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir);

        write_line(&inbox, "message one");
        write_line(&inbox, "message two");

        // Simulate: first message was delivered, .in-flight written, .responded set.
        // We do this by running hook for the first time.
        let first = run(&inbox, &Mode::Drain, 0).unwrap();
        assert_eq!(first, Decision::Block("message one".to_string()));
        assert!(responded(&dir).exists());
        assert!(in_flight(&dir).exists());

        // Cursor should NOT be advanced yet (Fix B).
        let cur = inbox::read_offset(&offset(&dir)).unwrap().unwrap_or(0);
        assert_eq!(cur, 0, "cursor must not advance on delivery in Fix B");

        // Now simulate: agent responded. Hook fires again.
        let second = run(&inbox, &Mode::Drain, 0).unwrap();
        assert_eq!(second, Decision::Block("message two".to_string()));

        // .responded should exist for next round.
        assert!(responded(&dir).exists());
        // .in-flight should exist for message two.
        let inf2 = InFlightEntry::read_from(&in_flight(&dir)).unwrap().unwrap();
        assert_eq!(inf2.raw_line, "message two");
    }

    #[test]
    fn flag_with_empty_inbox_acknowledges_and_approves_in_drain_mode() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir);

        write_line(&inbox, "only message");

        // First tick: deliver.
        run(&inbox, &Mode::Drain, 0).unwrap();

        // Second tick: agent replied, inbox empty → block /exit.
        let decision = run(&inbox, &Mode::Drain, 0).unwrap();
        assert_eq!(decision, Decision::Block("/exit".to_string()));

        // .responded and .in-flight should both be gone.
        assert!(!responded(&dir).exists());
        assert!(!in_flight(&dir).exists());
    }

    #[test]
    fn flag_with_empty_inbox_idles_in_persist_mode() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir);

        write_line(&inbox, "only message");
        run(&inbox, &Mode::Persist, 0).unwrap();

        let decision = run(&inbox, &Mode::Persist, 0).unwrap();
        assert_eq!(decision, Decision::IdleTick);
    }

    // -------------------------------------------------------------------------
    // Fix B deferred-ack property
    // -------------------------------------------------------------------------

    #[test]
    fn cursor_advances_only_on_second_tick_not_on_delivery() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir);

        write_line(&inbox, "entry A");

        // Before any tick, cursor is 0.
        assert_eq!(inbox::read_offset(&offset(&dir)).unwrap().unwrap_or(0), 0);

        // Tick 1: delivery. Cursor must remain at 0.
        run(&inbox, &Mode::Drain, 0).unwrap();
        assert_eq!(
            inbox::read_offset(&offset(&dir)).unwrap().unwrap_or(0),
            0,
            "cursor must not advance on delivery"
        );

        // Tick 2: ack + approve. Cursor must advance past entry A.
        run(&inbox, &Mode::Drain, 0).unwrap();
        let after = inbox::read_offset(&offset(&dir)).unwrap().unwrap_or(0);
        assert!(after > 0, "cursor must advance after ack");
    }

    #[test]
    fn in_flight_removed_after_ack() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir);

        write_line(&inbox, "entry");

        // Tick 1: deliver → .in-flight written.
        run(&inbox, &Mode::Drain, 0).unwrap();
        assert!(in_flight(&dir).exists());

        // Tick 2: ack → .in-flight removed.
        run(&inbox, &Mode::Drain, 0).unwrap();
        assert!(!in_flight(&dir).exists());
    }

    // -------------------------------------------------------------------------
    // Crash window tests — all three scenarios from §2
    // -------------------------------------------------------------------------

    /// Crash window A: session ends with .in-flight on disk (launcher crash).
    /// On next startup, .in-flight present, .responded absent, cursor at start_offset.
    /// Expected: orphan signal visible, entry re-deliverable.
    #[test]
    fn crash_window_a_launcher_crash_leaves_live_orphan() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir);

        write_line(&inbox, "entry K");

        // Deliver entry K (writes .in-flight + .responded).
        run(&inbox, &Mode::Drain, 0).unwrap();

        // Simulate: launcher crashes. .in-flight and .responded both present.
        // Cursor at 0 (entry K's start_offset).
        let inf = InFlightEntry::read_from(&in_flight(&dir)).unwrap().unwrap();
        let current_cursor = inbox::read_offset(&offset(&dir)).unwrap().unwrap_or(0);

        // Live orphan: cursor at start_offset, not past end_offset.
        assert!(!inf.is_stale(current_cursor));
        assert_eq!(inf.start_offset, 0);

        // Recovery: launcher should apply orphan policy (retry/deadletter/drop).
        // The hook itself doesn't run recovery — that's the launcher's job.
        // We verify the on-disk signal is correct.
        assert!(in_flight(&dir).exists());
    }

    /// Crash window B: agent crashes mid-turn (.responded exists, .in-flight exists).
    /// Identical on-disk state to window A. Same detection.
    #[test]
    fn crash_window_b_agent_crash_leaves_same_orphan_signal() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir);

        write_line(&inbox, "entry K");

        run(&inbox, &Mode::Drain, 0).unwrap();

        // Agent crash: both artifacts present, cursor unmoved.
        assert!(responded(&dir).exists());
        assert!(in_flight(&dir).exists());
        let cur = inbox::read_offset(&offset(&dir)).unwrap().unwrap_or(0);
        assert_eq!(cur, 0);

        // Same orphan detection as window A.
        let inf = InFlightEntry::read_from(&in_flight(&dir)).unwrap().unwrap();
        assert!(!inf.is_stale(cur));
    }

    /// Crash window C: hook crashes AFTER ack step 1 (cursor advanced),
    /// BEFORE ack step 2 (.in-flight removal). Stale orphan case.
    #[test]
    fn crash_window_c_stale_orphan_cursor_past_end_offset() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir);

        write_line(&inbox, "entry K\n");

        // Deliver entry K.
        run(&inbox, &Mode::Drain, 0).unwrap();
        let inf_before = InFlightEntry::read_from(&in_flight(&dir)).unwrap().unwrap();

        // Simulate: ack step 1 succeeded (cursor advanced), step 2 did not.
        inbox::write_offset(&offset(&dir), inf_before.end_offset).unwrap();
        // .in-flight still present (step 2 didn't run).

        // Now launcher reads .in-flight and checks cursor.
        let cur = inbox::read_offset(&offset(&dir)).unwrap().unwrap();
        let inf = InFlightEntry::read_from(&in_flight(&dir)).unwrap().unwrap();

        // cursor == end_offset: entry is fully past the cursor.
        // is_stale uses >= so this is correctly detected as stale.
        assert!(
            inf.is_stale(cur),
            "cursor at end_offset must be detected as stale"
        );
        assert!(
            cur >= inf.end_offset,
            "cursor at or past end_offset means entry was acknowledged"
        );
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

    // -------------------------------------------------------------------------
    // F12 regression: .responded without .in-flight must error, not re-deliver
    // -------------------------------------------------------------------------

    #[test]
    fn responded_without_in_flight_returns_error() {
        // Operator manually removes .in-flight while .responded is present.
        // Previously the hook would silently re-deliver the entry at the
        // current cursor. Now it must return an explicit error so the failure
        // is visible in stderr and the operator is directed to `recover`.
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir);

        write_line(&inbox, "entry K");

        // Tick 1: deliver. Writes .in-flight + .responded.
        run(&inbox, &Mode::Drain, 0).unwrap();
        assert!(in_flight(&dir).exists());
        assert!(responded(&dir).exists());

        // Simulate: operator removes .in-flight to "unstick" the session.
        fs::remove_file(in_flight(&dir)).unwrap();

        // Tick 2: hook sees .responded without .in-flight. Must return Err.
        let result = run(&inbox, &Mode::Drain, 0);
        assert!(
            result.is_err(),
            "hook must error on .responded without .in-flight, not silently re-deliver"
        );
        let err = result.unwrap_err();
        // Error must be the typed InconsistentState variant — not a raw io::Error.
        assert!(
            matches!(err, HeartbeatError::InconsistentState { .. }),
            "expected InconsistentState variant, got: {:?}",
            err
        );
        // Display impl (via thiserror) must name the inconsistency.
        assert!(
            err.to_string().contains("inconsistent state"),
            "error message must name the inconsistency: {}",
            err
        );
    }

    // -------------------------------------------------------------------------
    // NEW: inbox hardening tests
    // -------------------------------------------------------------------------

    /// Drain mode with a nonexistent inbox file (no file created at all) →
    /// Decision::Block("/exit"). Exercises the NotFound → Ok(None) path in
    /// inbox::read_next_entry: the inbox file doesn't exist, read_next_entry
    /// returns Ok(None), and the hook must approve rather than error.
    #[test]
    fn drain_nonexistent_inbox_approves() {
        let dir = TempDir::new().unwrap();
        // Deliberately do NOT create the inbox file.
        let inbox = dir.path().join("inbox.jsonl");

        let decision = run(&inbox, &Mode::Drain, 0).unwrap();
        assert_eq!(
            decision,
            Decision::Block("/exit".to_string()),
            "nonexistent inbox must approve in drain mode — no entries means done"
        );
    }

    /// Blank line between two real messages: hook delivers first, skips the
    /// blank line at the inbox level, then delivers second on the next round.
    /// Tests the blank-skip property at the hook orchestration level (not just
    /// inbox unit level).
    #[test]
    fn blank_line_between_entries_skipped_at_hook_level() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir);

        // Write: real message, blank line, real message.
        write_line(&inbox, "first entry");
        write_line(&inbox, "");
        write_line(&inbox, "second entry");

        // Tick 1: deliver first entry.
        let d1 = run(&inbox, &Mode::Drain, 0).unwrap();
        assert_eq!(d1, Decision::Block("first entry".to_string()));
        assert!(responded(&dir).exists());

        // Tick 2: ack first, skip blank, deliver second.
        let d2 = run(&inbox, &Mode::Drain, 0).unwrap();
        assert_eq!(
            d2,
            Decision::Block("second entry".to_string()),
            "blank line between entries must be skipped — second entry delivered directly"
        );

        // Tick 3: ack second, inbox drained → approve.
        let d3 = run(&inbox, &Mode::Drain, 0).unwrap();
        assert_eq!(d3, Decision::Block("/exit".to_string()));
    }

    // -------------------------------------------------------------------------
    // NEW (assembly graft): multi-entry byte-order delivery test
    // -------------------------------------------------------------------------

    /// Three-entry inbox delivers entries in byte order across successive
    /// deliver→ack ticks, then approves once drained.
    ///
    /// This validates the full deliver→ack→advance cycle end-to-end at the hook
    /// orchestration level: each Block carries the correct content in file order,
    /// the cursor advances only on ack (Fix B), and the terminal drain-exit fires
    /// exactly when the last entry is acknowledged with nothing remaining.
    ///
    /// Entries and their byte extents (all plain-text with '\n'):
    ///   "alpha\n"   → 6 bytes  (start=0,  end=6)
    ///   "bravo\n"   → 6 bytes  (start=6,  end=12)
    ///   "charlie\n" → 8 bytes  (start=12, end=20)
    #[test]
    fn multi_entry_inbox_delivers_in_byte_order_then_approves() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir);

        write_line(&inbox, "alpha");
        write_line(&inbox, "bravo");
        write_line(&inbox, "charlie");

        // Tick 1: first invocation, no .responded → deliver "alpha".
        let d1 = run(&inbox, &Mode::Drain, 0).unwrap();
        assert_eq!(
            d1,
            Decision::Block("alpha".to_string()),
            "tick 1 must deliver first entry"
        );
        assert!(responded(&dir).exists(), "tick 1 must set .responded");
        // Fix B: cursor must NOT advance on delivery.
        assert_eq!(
            inbox::read_offset(&offset(&dir)).unwrap().unwrap_or(0),
            0,
            "cursor must stay at 0 after delivery of first entry (Fix B)"
        );

        // Tick 2: .responded present → ack "alpha", deliver "bravo".
        let d2 = run(&inbox, &Mode::Drain, 0).unwrap();
        assert_eq!(
            d2,
            Decision::Block("bravo".to_string()),
            "tick 2 must deliver second entry"
        );
        // Cursor must now sit at end_offset of "alpha" = 6.
        assert_eq!(
            inbox::read_offset(&offset(&dir)).unwrap().unwrap_or(0),
            6,
            "cursor must advance to 6 (past 'alpha\\n') after ack"
        );
        assert!(responded(&dir).exists(), "tick 2 must re-set .responded");

        // Tick 3: .responded present → ack "bravo", deliver "charlie".
        let d3 = run(&inbox, &Mode::Drain, 0).unwrap();
        assert_eq!(
            d3,
            Decision::Block("charlie".to_string()),
            "tick 3 must deliver third entry"
        );
        // Cursor must now sit at end_offset of "bravo" = 12.
        assert_eq!(
            inbox::read_offset(&offset(&dir)).unwrap().unwrap_or(0),
            12,
            "cursor must advance to 12 (past 'bravo\\n') after ack"
        );
        assert!(responded(&dir).exists(), "tick 3 must re-set .responded");
        assert!(
            in_flight(&dir).exists(),
            "tick 3 must write .in-flight for 'charlie'"
        );

        // Tick 4: .responded present → ack "charlie", inbox drained → Block /exit.
        let d4 = run(&inbox, &Mode::Drain, 0).unwrap();
        assert_eq!(
            d4,
            Decision::Block("/exit".to_string()),
            "tick 4 must return drain-exit block once all entries are drained"
        );
        // Cursor must now sit at end_offset of "charlie" = 20.
        assert_eq!(
            inbox::read_offset(&offset(&dir)).unwrap().unwrap_or(0),
            20,
            "cursor must advance to 20 (past 'charlie\\n') after final ack"
        );
        // Both .responded and .in-flight must be cleaned up.
        assert!(
            !responded(&dir).exists(),
            ".responded must be removed after final ack"
        );
        assert!(
            !in_flight(&dir).exists(),
            ".in-flight must be removed after final ack"
        );
    }

    // -------------------------------------------------------------------------
    // NEW: Optional hook.rs test — InconsistentState variant (lil-grabby §9)
    // -------------------------------------------------------------------------

    /// Optional (§9 encouraged): .responded present without .in-flight →
    /// run → Err(HeartbeatError::InconsistentState { io_dir }) with correct dir.
    ///
    /// This is a tighter version of responded_without_in_flight_returns_error,
    /// directly asserting the typed variant and the io_dir field.
    #[test]
    fn responded_without_in_flight_returns_inconsistent_state_with_io_dir() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir);

        // Create .responded but no .in-flight.
        write_line(&inbox, "msg");
        // Tick 1: deliver (creates both .responded and .in-flight).
        run(&inbox, &Mode::Drain, 0).unwrap();
        // Remove .in-flight only.
        fs::remove_file(in_flight(&dir)).unwrap();
        assert!(responded(&dir).exists());
        assert!(!in_flight(&dir).exists());

        let result = run(&inbox, &Mode::Drain, 0);
        match result {
            Err(HeartbeatError::InconsistentState { io_dir }) => {
                // io_dir must be inbox's parent directory.
                assert_eq!(
                    io_dir,
                    inbox.parent().unwrap(),
                    "io_dir must be the parent directory of the inbox"
                );
            }
            other => panic!(
                "expected Err(HeartbeatError::InconsistentState {{ io_dir }}), got {:?}",
                other
            ),
        }
    }
}

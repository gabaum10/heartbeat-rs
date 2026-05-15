//! Orphan recovery: the `heartbeat-stop recover` subcommand.
//!
//! Launchers call this before truncating the inbox or starting a new cycle.
//! It detects any `.in-flight` artifact left from a prior session and applies
//! the configured `OrphanPolicy`.
//!
//! ## Why launchers must call this before reset
//!
//! The Fen launcher pattern does `> "$INBOX"; echo -n "0" > .inbox-offset`
//! to start fresh. If a prior session left a `.in-flight` artifact, blowing
//! away the inbox and resetting the offset silently defeats Fix B — the
//! orphan evidence is gone. Running `recover` first gives the launcher the
//! chance to apply the configured policy before wiping state.
//!
//! ## Policies
//!
//! - `retry` — prepend orphan's `raw_line` back to the inbox so it is
//!   delivered first in the next session. Use when agent-side work is
//!   idempotent or the entry was never seen by the agent (C1/C2 crash cases).
//!   Risk: duplicate side effects if the agent already processed it (C3 case).
//!
//! - `deadletter` (default) — move orphan contents to `.dead-letter.jsonl`,
//!   advance cursor past the entry, delete `.in-flight`. Use when duplicate
//!   side effects are unacceptable. Requires operator attention to drain
//!   the dead-letter file.
//!
//! - `drop` — delete `.in-flight` and advance cursor. Use when the retry
//!   mechanism is upstream (e.g., Fen's IMAP layer will re-fetch unread mail)
//!   and loss is acceptable.
//!
//! ## Stale orphan fast path
//!
//! If `.in-flight.start_offset` < current cursor, the entry was already
//! acknowledged in a prior step but `.in-flight` was not removed (crash
//! between ack step 1 and step 2). This is a stale orphan — delete
//! `.in-flight` without action. No policy needed.
//!
//! ## Return value
//!
//! `recover` returns a `RecoveryOutcome` describing what happened. The
//! launcher can log this or use it to decide whether to send a notification.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use crate::in_flight::{self, InFlightEntry};
use crate::inbox;

/// How the launcher should handle an orphaned in-flight entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrphanPolicy {
    /// Re-deliver the orphan as the first entry of the next session.
    /// Best for idempotent workloads or entries the agent never actually saw.
    Retry,
    /// Move orphan to `.dead-letter.jsonl`, advance cursor past the entry.
    /// Best when duplicate side effects are unacceptable. Default.
    DeadLetter,
    /// Delete `.in-flight` and advance cursor. Accept the loss.
    /// Best when an upstream retry mechanism (e.g., IMAP re-fetch) covers it.
    Drop,
}

/// The result of running orphan recovery.
#[derive(Debug, Clone, PartialEq)]
pub enum RecoveryOutcome {
    /// No `.in-flight` file found. Nothing to do.
    NothingToRecover,
    /// `.in-flight` existed but was stale (cursor already past entry). Deleted.
    StaleOrphanDeleted { entry_id: String },
    /// Entry was re-queued at the front of the inbox (retry policy).
    ReQueued { entry_id: String },
    /// Entry was moved to `.dead-letter.jsonl` (deadletter policy).
    DeadLettered { entry_id: String },
    /// Entry was dropped (drop policy).
    Dropped { entry_id: String },
}

/// Run orphan recovery for the inbox at `inbox_path` with the given `policy`.
///
/// Must be called BEFORE truncating the inbox or resetting the offset file.
///
/// Returns a `RecoveryOutcome` describing what happened.
pub fn recover(inbox_path: &Path, policy: OrphanPolicy) -> io::Result<RecoveryOutcome> {
    let io_dir = inbox_path.parent().unwrap_or(Path::new("."));
    let in_flight_path = in_flight::in_flight_file_for(inbox_path);
    let offset_file = inbox::offset_file_for(inbox_path);

    // No .in-flight: nothing to recover.
    let entry = match InFlightEntry::read_from(&in_flight_path)? {
        Some(e) => e,
        None => return Ok(RecoveryOutcome::NothingToRecover),
    };

    let current_offset = inbox::read_offset(&offset_file).unwrap_or(0);

    // Stale orphan: cursor already advanced past the entry's end.
    // Crash occurred between ack step 1 (cursor advance) and step 2
    // (.in-flight removal). Entry was already acknowledged — just clean up.
    if current_offset >= entry.end_offset {
        fs::remove_file(&in_flight_path)?;
        return Ok(RecoveryOutcome::StaleOrphanDeleted {
            entry_id: entry.entry_id,
        });
    }

    // Live orphan: apply policy.
    match policy {
        OrphanPolicy::Retry => {
            // The orphan is already in inbox.jsonl at entry.start_offset —
            // recover always runs BEFORE the launcher truncates the inbox.
            // There is nothing to prepend. The correct repair is:
            //   1. Reset the cursor to entry.start_offset (walk it back to K).
            //   2. Remove .in-flight (will be rewritten on next delivery tick).
            //
            // This means the next session reads K first, exactly as if K had
            // never been delivered. No duplicate is created because we are not
            // copying bytes — we are moving a cursor.
            //
            // Idempotency note: if the agent DID process K (crash scenario C3),
            // the side effect fires twice. This is the documented contract for
            // retry policy — the caller is responsible for idempotency at the
            // agent layer. See spec §6 risk #5.
            inbox::write_offset(&offset_file, entry.start_offset)?;
            // Remove .in-flight — it will be rewritten on next delivery tick.
            fs::remove_file(&in_flight_path)?;
            Ok(RecoveryOutcome::ReQueued {
                entry_id: entry.entry_id,
            })
        }

        OrphanPolicy::DeadLetter => {
            // Append orphan to .dead-letter.jsonl in the inbox dir.
            let dead_letter_path = io_dir.join(".dead-letter.jsonl");
            let record = serde_json::json!({
                "entry_id": entry.entry_id,
                "start_offset": entry.start_offset,
                "end_offset": entry.end_offset,
                "raw_line": entry.raw_line,
                "delivered_at": entry.delivered_at,
            });
            let mut f = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&dead_letter_path)?;
            writeln!(f, "{}", record)?;
            f.sync_all()?;
            // Advance cursor past the orphaned entry.
            inbox::write_offset(&offset_file, entry.end_offset)?;
            // Remove .in-flight.
            fs::remove_file(&in_flight_path)?;
            Ok(RecoveryOutcome::DeadLettered {
                entry_id: entry.entry_id,
            })
        }

        OrphanPolicy::Drop => {
            // Advance cursor past the entry, delete .in-flight. Accept the loss.
            inbox::write_offset(&offset_file, entry.end_offset)?;
            fs::remove_file(&in_flight_path)?;
            Ok(RecoveryOutcome::Dropped {
                entry_id: entry.entry_id,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hook;
    use crate::in_flight::InFlightEntry;
    use crate::inbox;
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

    fn in_flight(dir: &TempDir) -> PathBuf {
        dir.path().join(".in-flight")
    }

    fn dead_letter(dir: &TempDir) -> PathBuf {
        dir.path().join(".dead-letter.jsonl")
    }

    // -------------------------------------------------------------------------
    // NothingToRecover
    // -------------------------------------------------------------------------

    #[test]
    fn no_in_flight_returns_nothing_to_recover() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir);
        write_line(&inbox, "entry");

        let outcome = recover(&inbox, OrphanPolicy::DeadLetter).unwrap();
        assert_eq!(outcome, RecoveryOutcome::NothingToRecover);
    }

    // -------------------------------------------------------------------------
    // Stale orphan
    // -------------------------------------------------------------------------

    #[test]
    fn stale_orphan_deleted_when_cursor_past_end_offset() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir);
        let offset_file = dir.path().join(".inbox-offset");
        let in_flight_path = in_flight(&dir);

        write_line(&inbox, "entry K");

        // Simulate: ack step 1 ran (cursor past end), step 2 did not.
        let entry = InFlightEntry::new("entry K", 0, 8); // "entry K\n" = 8 bytes
        entry.write_to(&in_flight_path).unwrap();
        inbox::write_offset(&offset_file, 9).unwrap(); // cursor past end_offset

        let outcome = recover(&inbox, OrphanPolicy::DeadLetter).unwrap();
        match outcome {
            RecoveryOutcome::StaleOrphanDeleted { .. } => {}
            other => panic!("expected StaleOrphanDeleted, got {:?}", other),
        }
        assert!(!in_flight_path.exists());
    }

    // -------------------------------------------------------------------------
    // Retry policy
    // -------------------------------------------------------------------------

    #[test]
    fn retry_resets_cursor_to_start_offset_no_duplicate() {
        // BLOCKER regression: retry must NOT duplicate the entry.
        // The orphan is already at its original offset; recover resets the
        // cursor to start_offset rather than prepending a second copy.
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir);
        let offset_file = dir.path().join(".inbox-offset");
        let in_flight_path = in_flight(&dir);

        // Write two entries, deliver the first (leaving it as orphan).
        write_line(&inbox, "entry K");
        write_line(&inbox, "entry K+1");

        // Snapshot inbox content before recovery.
        let inbox_before = fs::read_to_string(&inbox).unwrap();

        // Simulate first delivery via hook::run.
        hook::run(&inbox, &hook::Mode::Drain).unwrap();
        // Cursor still at 0 (Fix B). .in-flight written.
        assert!(in_flight_path.exists());

        // Now simulate: session ends without ack. Call recover with retry.
        let outcome = recover(&inbox, OrphanPolicy::Retry).unwrap();
        match &outcome {
            RecoveryOutcome::ReQueued { entry_id } => {
                assert!(!entry_id.is_empty());
            }
            other => panic!("expected ReQueued, got {:?}", other),
        }

        // .in-flight should be gone.
        assert!(!in_flight_path.exists());

        // Cursor reset to start_offset of entry K (which is 0).
        let cur = inbox::read_offset(&offset_file).unwrap();
        assert_eq!(cur, 0, "cursor must be reset to start_offset of orphan");

        // CRITICAL: inbox contents must be identical to before recovery.
        // No line was prepended; cursor was walked back instead.
        let inbox_after = fs::read_to_string(&inbox).unwrap();
        assert_eq!(
            inbox_before, inbox_after,
            "retry must not modify inbox contents — orphan is already in place"
        );

        // Entry K appears exactly once in the inbox.
        let count = inbox_after.lines().filter(|l| *l == "entry K").count();
        assert_eq!(count, 1, "entry K must appear exactly once after retry");
    }

    #[test]
    fn retry_on_poison_entry_does_not_grow_inbox_over_5_cycles() {
        // BLOCKER regression: retry on a repeatedly-failing entry must not
        // grow the inbox. Five cycles, inbox line count must stay constant.
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir);
        let in_flight_path = in_flight(&dir);

        write_line(&inbox, "poison");
        let initial_line_count = fs::read_to_string(&inbox).unwrap().lines().count();

        for cycle in 1..=5 {
            // Deliver (writes .in-flight, cursor stays at 0).
            hook::run(&inbox, &hook::Mode::Drain).unwrap();
            assert!(in_flight_path.exists(), "cycle {}: .in-flight must exist after delivery", cycle);

            // Simulate: session fails without ack.
            let _ = fs::remove_file(dir.path().join(".responded"));

            // Recover with retry: must reset cursor, not prepend.
            recover(&inbox, OrphanPolicy::Retry).unwrap();

            let line_count = fs::read_to_string(&inbox).unwrap().lines().count();
            assert_eq!(
                line_count, initial_line_count,
                "cycle {}: inbox must not grow — had {} lines, now {} lines",
                cycle, initial_line_count, line_count
            );
        }
    }

    // -------------------------------------------------------------------------
    // Dead-letter policy
    // -------------------------------------------------------------------------

    #[test]
    fn deadletter_moves_orphan_to_dead_letter_file() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir);
        let offset_file = dir.path().join(".inbox-offset");
        let in_flight_path = in_flight(&dir);
        let dl_path = dead_letter(&dir);

        write_line(&inbox, "entry K");

        hook::run(&inbox, &hook::Mode::Drain).unwrap();
        assert!(in_flight_path.exists());

        let outcome = recover(&inbox, OrphanPolicy::DeadLetter).unwrap();
        match &outcome {
            RecoveryOutcome::DeadLettered { entry_id } => {
                assert!(!entry_id.is_empty());
            }
            other => panic!("expected DeadLettered, got {:?}", other),
        }

        // .in-flight gone.
        assert!(!in_flight_path.exists());
        // Dead-letter file created.
        assert!(dl_path.exists());
        // Dead-letter file contains valid JSON with the entry_id.
        let dl_contents = fs::read_to_string(&dl_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(dl_contents.trim()).unwrap();
        assert_eq!(parsed["raw_line"], "entry K");

        // Cursor advanced past entry.
        let cur = inbox::read_offset(&offset_file).unwrap();
        assert!(cur > 0, "cursor must advance after dead-letter");
    }

    #[test]
    fn deadletter_appends_multiple_orphans() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir);
        let dl_path = dead_letter(&dir);

        // Simulate two separate orphan cycles.
        for entry_text in &["orphan one", "orphan two"] {
            // Create a fresh inbox for each orphan.
            let _ = fs::remove_file(&inbox);
            inbox::write_offset(&dir.path().join(".inbox-offset"), 0).unwrap();
            write_line(&inbox, entry_text);
            hook::run(&inbox, &hook::Mode::Drain).unwrap();
            recover(&inbox, OrphanPolicy::DeadLetter).unwrap();
        }

        let dl_contents = fs::read_to_string(&dl_path).unwrap();
        let lines: Vec<&str> = dl_contents.lines().collect();
        assert_eq!(lines.len(), 2, "dead-letter must have two entries");
    }

    // -------------------------------------------------------------------------
    // Drop policy
    // -------------------------------------------------------------------------

    #[test]
    fn drop_deletes_in_flight_and_advances_cursor() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir);
        let offset_file = dir.path().join(".inbox-offset");
        let in_flight_path = in_flight(&dir);

        write_line(&inbox, "entry K");
        hook::run(&inbox, &hook::Mode::Drain).unwrap();
        assert!(in_flight_path.exists());

        let outcome = recover(&inbox, OrphanPolicy::Drop).unwrap();
        match &outcome {
            RecoveryOutcome::Dropped { entry_id } => {
                assert!(!entry_id.is_empty());
            }
            other => panic!("expected Dropped, got {:?}", other),
        }

        assert!(!in_flight_path.exists());
        let cur = inbox::read_offset(&offset_file).unwrap();
        assert!(cur > 0);
    }

    // -------------------------------------------------------------------------
    // Fen N=1 compatibility path
    // -------------------------------------------------------------------------

    /// In the Fen happy path (no crash), recover sees no .in-flight and
    /// returns NothingToRecover. The launcher can safely reset the inbox.
    #[test]
    fn fen_n1_happy_path_no_orphan() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir);

        write_line(&inbox, "fen triage batch");

        // Full tick cycle: deliver + ack (session ends cleanly).
        hook::run(&inbox, &hook::Mode::Drain).unwrap(); // deliver
        hook::run(&inbox, &hook::Mode::Drain).unwrap(); // ack + approve

        // .in-flight should be gone after clean session.
        assert!(!in_flight(&dir).exists());

        // recover sees nothing.
        let outcome = recover(&inbox, OrphanPolicy::Drop).unwrap();
        assert_eq!(outcome, RecoveryOutcome::NothingToRecover);
    }

    /// In the Fen failure path (agent crashes), .in-flight is present.
    /// Fen's policy is "drop and move on" — IMAP is the retry mechanism.
    #[test]
    fn fen_n1_failure_path_drop_policy() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir);
        let in_flight_path = in_flight(&dir);

        write_line(&inbox, "fen triage batch");

        // Deliver only — simulate agent crash before ack.
        hook::run(&inbox, &hook::Mode::Drain).unwrap();
        assert!(in_flight_path.exists());

        // Fen's launcher calls recover with drop.
        let outcome = recover(&inbox, OrphanPolicy::Drop).unwrap();
        match &outcome {
            RecoveryOutcome::Dropped { .. } => {}
            other => panic!("expected Dropped for Fen policy, got {:?}", other),
        }

        // .in-flight gone. Inbox can now be safely truncated + reset.
        assert!(!in_flight_path.exists());
    }
}

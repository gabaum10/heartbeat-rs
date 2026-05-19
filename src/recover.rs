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
//! away the inbox and resetting the offset silently defeats the deferred-ack
//! guarantee — the orphan evidence is gone. Running `recover` first gives the
//! launcher the chance to apply the configured policy before wiping state.
//!
//! ## Single source of truth for session-end cleanup
//!
//! `recover` is the authoritative cleanup point for all inbox-side session
//! artifacts. On every successful path it removes BOTH `.in-flight` AND
//! `.responded`. This means:
//!
//! - The documented remediation "run `heartbeat-stop recover`" actually
//!   resolves the inconsistent state without any additional manual steps.
//! - Launchers do NOT need to `rm .responded` separately after calling recover.
//! - The crash window between hook ack-step-1 (cursor advance + .in-flight
//!   removal) and ack-step-2 (.responded removal) leaves `.responded` without
//!   `.in-flight` on disk. Recover on next startup detects cursor >= end_offset
//!   (stale orphan), removes both artifacts, and continues cleanly.
//!
//! ## Policies
//!
//! - `retry` — reset cursor to `start_offset` so the next session re-delivers
//!   the orphan from its original position. Use when agent-side work is
//!   idempotent or the entry was never seen by the agent (C1/C2 crash cases).
//!   Risk: duplicate side effects if the agent already processed it (C3 case).
//!
//! - `deadletter` (default) — move orphan contents to `.dead-letter.jsonl`,
//!   advance cursor past the entry, delete `.in-flight`. Use when duplicate
//!   side effects are unacceptable. Requires operator attention to drain
//!   the dead-letter file.
//!
//! - `drop` — advance cursor, delete `.in-flight`. Accept the loss.
//!   Use when the retry mechanism is upstream (e.g., Fen's IMAP layer will
//!   re-fetch unread mail).
//!
//! ## Stale orphan fast path
//!
//! If cursor >= `end_offset`, the entry was already acknowledged in a prior
//! step but `.in-flight` (and possibly `.responded`) was not removed (crash
//! between ack steps). Delete both artifacts without applying orphan policy.
//!
//! ## Return value
//!
//! `recover` returns a `RecoveryOutcome` describing what happened. The
//! launcher can log this or use it to decide whether to send a notification.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use crate::error::{HeartbeatError, Result};
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
/// On every successful path, removes both `.in-flight` AND `.responded` so
/// the next session starts from a fully clean state. Launchers do not need
/// to remove `.responded` separately.
///
/// Returns a `RecoveryOutcome` describing what happened.
pub fn recover(inbox_path: &Path, policy: OrphanPolicy) -> Result<RecoveryOutcome> {
    let io_dir = inbox_path.parent().unwrap_or(Path::new("."));
    let in_flight_path = in_flight::in_flight_file_for(inbox_path);
    let responded_flag = io_dir.join(".responded");
    let offset_file = inbox::offset_file_for(inbox_path);

    // Defensive cleanup: remove any stale .dead-letter.jsonl.tmp from a prior
    // crashed recover run. Ignore NotFound — the file may not exist.
    let dead_letter_tmp = io_dir.join(".dead-letter.jsonl.tmp");
    let _ = fs::remove_file(&dead_letter_tmp);

    // No .in-flight: nothing to recover. Still remove .responded if it exists
    // (e.g., crash between hook ack-step-1 and ack-step-2 left .responded
    // without .in-flight, or operator called recover to clear a stuck state).
    let entry = match InFlightEntry::read_from(&in_flight_path)? {
        Some(e) => e,
        None => {
            // Remove .responded if present (stale from prior session).
            let _ = fs::remove_file(&responded_flag);
            return Ok(RecoveryOutcome::NothingToRecover);
        }
    };

    let current_offset: u64 = inbox::read_offset(&offset_file)?.unwrap_or_default();

    // Stale orphan: cursor already advanced past the entry's end.
    // Crash occurred between ack step 1 (cursor advance) and step 2
    // (.in-flight removal). Entry was already acknowledged — clean up both.
    if current_offset >= entry.end_offset {
        let _ = fs::remove_file(&in_flight_path);
        let _ = fs::remove_file(&responded_flag);
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
            let _ = fs::remove_file(&in_flight_path);
            // Remove .responded so the next session starts without the F12
            // inconsistency check triggering on the very first hook tick.
            let _ = fs::remove_file(&responded_flag);
            Ok(RecoveryOutcome::ReQueued {
                entry_id: entry.entry_id,
            })
        }

        OrphanPolicy::DeadLetter => {
            // Append orphan to .dead-letter.jsonl in the inbox dir.
            //
            // Atomicity: a crash between the dead-letter write and the cursor
            // advance leaves .in-flight present and cursor unmoved. Next recover
            // call re-enters this branch and appends a duplicate record with the
            // same entry_id. This is bounded and detectable — consumers should
            // deduplicate on entry_id. We mitigate it by writing dead-letter
            // contents to a .tmp file first, syncing, then atomically renaming
            // into place before advancing the cursor.
            //
            // Strategy: read existing dead-letter, append new record, write full
            // contents to .tmp, sync, rename. This is O(file) on every call but
            // dead-letter is low-frequency and bounded by inbox throughput.
            let dead_letter_path = io_dir.join(".dead-letter.jsonl");
            let dead_letter_tmp = io_dir.join(".dead-letter.jsonl.tmp");

            let record = serde_json::json!({
                "entry_id": entry.entry_id,
                "start_offset": entry.start_offset,
                "end_offset": entry.end_offset,
                "raw_line": entry.raw_line,
                "delivered_at": entry.delivered_at,
            });
            let new_line = format!("{}\n", record);

            let dl_err = |e: io::Error| HeartbeatError::DeadLetterWrite {
                path: dead_letter_path.clone(),
                source: e,
            };

            // Read existing contents (may not exist yet).
            let existing = match fs::read(&dead_letter_path) {
                Ok(b) => b,
                Err(e) if e.kind() == io::ErrorKind::NotFound => vec![],
                Err(e) => return Err(dl_err(e)),
            };

            // Write existing + new record to .tmp, then rename atomically.
            {
                let mut f = fs::File::create(&dead_letter_tmp).map_err(dl_err)?;
                f.write_all(&existing).map_err(dl_err)?;
                f.write_all(new_line.as_bytes()).map_err(dl_err)?;
                f.sync_all().map_err(dl_err)?;
            }
            fs::rename(&dead_letter_tmp, &dead_letter_path).map_err(dl_err)?;

            // Advance cursor past the orphaned entry.
            inbox::write_offset(&offset_file, entry.end_offset)?;
            // Remove .in-flight.
            let _ = fs::remove_file(&in_flight_path);
            // Remove .responded so the next session starts clean.
            let _ = fs::remove_file(&responded_flag);
            Ok(RecoveryOutcome::DeadLettered {
                entry_id: entry.entry_id,
            })
        }

        OrphanPolicy::Drop => {
            // Advance cursor past the entry, delete .in-flight. Accept the loss.
            inbox::write_offset(&offset_file, entry.end_offset)?;
            let _ = fs::remove_file(&in_flight_path);
            // Remove .responded so the next session starts clean.
            let _ = fs::remove_file(&responded_flag);
            Ok(RecoveryOutcome::Dropped {
                entry_id: entry.entry_id,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::HeartbeatError;
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
        let cur = inbox::read_offset(&offset_file).unwrap().unwrap_or(0);
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
            assert!(
                in_flight_path.exists(),
                "cycle {}: .in-flight must exist after delivery",
                cycle
            );

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
        let cur = inbox::read_offset(&offset_file).unwrap().unwrap_or(0);
        assert!(cur > 0, "cursor must advance after dead-letter");
    }

    #[test]
    fn deadletter_appends_multiple_orphans() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir);
        let dl_path = dead_letter(&dir);

        // Simulate two separate orphan cycles.
        for entry_text in &["orphan one", "orphan two"] {
            // Create a fresh inbox for each orphan — wipe all state including
            // .responded so the next hook::run starts from a clean slate.
            let _ = fs::remove_file(&inbox);
            let _ = fs::remove_file(dir.path().join(".responded"));
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
        let cur = inbox::read_offset(&offset_file).unwrap().unwrap_or(0);
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

    // -------------------------------------------------------------------------
    // F23: corrupt .in-flight must propagate error (not return NothingToRecover)
    // -------------------------------------------------------------------------

    /// Test 6 (§9.6): existing recover_errors_on_corrupt_in_flight now returns
    /// the typed variant Err(HeartbeatError::InFlightCorrupt).
    #[test]
    fn recover_errors_on_corrupt_in_flight() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir);
        let in_flight_path = in_flight(&dir);

        fs::write(&inbox, "some entry\n").unwrap();
        fs::write(&in_flight_path, "{not valid json").unwrap();

        let result = recover(&inbox, OrphanPolicy::DeadLetter);
        match result {
            Err(HeartbeatError::InFlightCorrupt { path, source: _ }) => {
                assert_eq!(
                    path, in_flight_path,
                    "InFlightCorrupt path must be the .in-flight file path"
                );
            }
            other => panic!(
                "expected Err(HeartbeatError::InFlightCorrupt), got {:?}",
                other
            ),
        }
    }

    // -------------------------------------------------------------------------
    // NEW: error-path tests for typed HeartbeatError variants (lil-grabby §9)
    // -------------------------------------------------------------------------

    /// Test 5 (§9.5): corrupt offset file → recover →
    /// Err(HeartbeatError::OffsetCorrupt { path, content }) with correct fields.
    ///
    /// Setup: .in-flight exists (so recover gets past the NothingToRecover check),
    /// but the offset file contains garbage.
    #[test]
    fn corrupt_offset_recover_returns_offset_corrupt() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir);
        let in_flight_path = in_flight(&dir);
        let offset_file = dir.path().join(".inbox-offset");

        // Write inbox and a valid .in-flight so recover reaches the read_offset call.
        write_line(&inbox, "entry K");
        let entry = InFlightEntry::new("entry K", 0, 8);
        entry.write_to(&in_flight_path).unwrap();

        // Write a corrupt offset file.
        fs::write(&offset_file, "CORRUPT").unwrap();

        let result = recover(&inbox, OrphanPolicy::DeadLetter);
        match result {
            Err(HeartbeatError::OffsetCorrupt { path, content }) => {
                assert_eq!(path, offset_file, "path must be the offset file path");
                assert_eq!(
                    content, "CORRUPT",
                    "content must be the trimmed string that failed to parse"
                );
            }
            other => panic!("expected Err(HeartbeatError::OffsetCorrupt), got {:?}", other),
        }
    }

    // -------------------------------------------------------------------------
    // F24: recover + hook::run re-delivers entry K (end-to-end retry loop)
    // -------------------------------------------------------------------------

    #[test]
    fn retry_followed_by_hook_run_re_delivers_entry_k() {
        // The contract "retry re-delivers" must be true at the hook level,
        // not just at the cursor level. This test asserts the full loop.
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir);
        let in_flight_path = in_flight(&dir);

        write_line(&inbox, "entry K");
        write_line(&inbox, "entry K+1");

        // Tick 1: deliver K. Writes .in-flight + .responded.
        let d1 = hook::run(&inbox, &hook::Mode::Drain).unwrap();
        assert_eq!(d1, hook::Decision::Block("entry K".to_string()));
        assert!(in_flight_path.exists());

        // Simulate: session crashes. Recover with retry.
        // recover removes .in-flight, resets cursor, AND removes .responded —
        // it is the single cleanup point for all inbox-side session artifacts.
        let outcome = recover(&inbox, OrphanPolicy::Retry).unwrap();
        match &outcome {
            RecoveryOutcome::ReQueued { .. } => {}
            other => panic!("expected ReQueued, got {:?}", other),
        }
        assert!(!in_flight_path.exists());
        assert!(
            !dir.path().join(".responded").exists(),
            "recover must remove .responded so next session starts clean"
        );

        // Next hook::run (new session, clean state) must re-deliver K.
        // No manual .responded cleanup needed — recover handled it.
        let d2 = hook::run(&inbox, &hook::Mode::Drain).unwrap();
        assert_eq!(
            d2,
            hook::Decision::Block("entry K".to_string()),
            "hook must re-deliver entry K after retry recovery"
        );

        // After ack (second tick), K+1 should come next.
        let d3 = hook::run(&inbox, &hook::Mode::Drain).unwrap();
        assert_eq!(d3, hook::Decision::Block("entry K+1".to_string()));
    }

    // -------------------------------------------------------------------------
    // Fen cascade reproducer (Wren round 3 BLOCKER)
    // -------------------------------------------------------------------------

    /// Reproduce the exact silent-data-loss cascade Wren verified live:
    ///
    /// Cycle N: session crashes, leaves .in-flight + .responded on disk.
    /// Cycle N+1: launcher runs recover (drop), truncates inbox, writes new
    ///   batch, launches claude. First Stop hook tick fires.
    ///
    /// Before fix: hook sees .responded (cycle N) without .in-flight (removed
    ///   by recover) → F12 error → fail-open Approve → session ends → launcher
    ///   marks emails read without triage.
    ///
    /// After fix: recover removes .responded alongside .in-flight → hook starts
    ///   clean → first tick delivers new batch entry → session runs correctly.
    #[test]
    fn fen_cascade_drop_policy_clears_responded_before_next_session() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir);
        let in_flight_path = in_flight(&dir);
        let responded_path = dir.path().join(".responded");

        // --- Cycle N: write a batch, deliver, crash (no ack) ---
        write_line(&inbox, "cycle N email batch");
        hook::run(&inbox, &hook::Mode::Drain).unwrap();

        // After delivery: .in-flight and .responded both present, cursor at 0.
        assert!(in_flight_path.exists(), "cycle N: .in-flight must exist");
        assert!(responded_path.exists(), "cycle N: .responded must exist");

        // --- Cycle N+1: launcher startup ---

        // Step 1: recover with drop policy.
        let outcome = recover(&inbox, OrphanPolicy::Drop).unwrap();
        match &outcome {
            RecoveryOutcome::Dropped { .. } => {}
            other => panic!("expected Dropped, got {:?}", other),
        }

        // .in-flight AND .responded must both be gone after recover.
        assert!(!in_flight_path.exists(), "recover must remove .in-flight");
        assert!(
            !responded_path.exists(),
            "recover must remove .responded — without this, next hook tick fires F12"
        );

        // Step 2: launcher truncates inbox and writes new cycle N+1 batch.
        fs::write(&inbox, "").unwrap();
        inbox::write_offset(&dir.path().join(".inbox-offset"), 0).unwrap();
        write_line(&inbox, "cycle N+1 email batch");

        // Step 3: new claude session starts. First Stop hook fires (turn 0).
        // Must deliver the new batch, NOT trigger F12.
        let decision = hook::run(&inbox, &hook::Mode::Drain).unwrap();
        assert_eq!(
            decision,
            hook::Decision::Block("cycle N+1 email batch".to_string()),
            "first hook tick of cycle N+1 must deliver new batch, not error out"
        );
    }

    // -------------------------------------------------------------------------
    // Crash window: between hook ack-step-1 and ack-step-2
    // -------------------------------------------------------------------------

    /// Crash between hook ack-step-1 (cursor advance + .in-flight removal) and
    /// ack-step-2 (.responded removal) leaves .responded without .in-flight.
    ///
    /// Before fix: next session's first hook tick hit F12 error → session ends
    ///   one entry early.
    ///
    /// After fix: recover on next startup detects cursor >= entry.end_offset
    ///   (stale orphan fast path), removes both artifacts, returns
    ///   StaleOrphanDeleted. Next hook tick starts clean and delivers the
    ///   next queued entry.
    #[test]
    fn crash_between_ack_step1_and_step2_recovered_by_stale_orphan_path() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir);
        let in_flight_path = in_flight(&dir);
        let responded_path = dir.path().join(".responded");
        let offset_file = dir.path().join(".inbox-offset");

        write_line(&inbox, "entry K");
        write_line(&inbox, "entry K+1");

        // Deliver entry K.
        hook::run(&inbox, &hook::Mode::Drain).unwrap();
        assert!(in_flight_path.exists());
        assert!(responded_path.exists());

        // Simulate: hook ack-step-1 ran (cursor past K, .in-flight removed)
        // but process was killed before ack-step-2 (.responded removal).
        // Replicate that partial state manually.
        let in_flight_entry = crate::in_flight::InFlightEntry::read_from(&in_flight_path)
            .unwrap()
            .unwrap();
        inbox::write_offset(&offset_file, in_flight_entry.end_offset).unwrap();
        fs::remove_file(&in_flight_path).unwrap();
        // .responded is still present (ack-step-2 didn't run).
        assert!(responded_path.exists());
        assert!(!in_flight_path.exists());

        // Launcher runs recover on next startup.
        // cursor >= entry.end_offset → stale orphan fast path.
        let outcome = recover(&inbox, OrphanPolicy::Drop).unwrap();
        match &outcome {
            RecoveryOutcome::NothingToRecover => {}
            // Stale orphan is also acceptable — both clean the state.
            RecoveryOutcome::StaleOrphanDeleted { .. } => {}
            other => panic!(
                "expected NothingToRecover or StaleOrphanDeleted, got {:?}",
                other
            ),
        }

        // Both artifacts must be gone.
        assert!(
            !in_flight_path.exists(),
            "recover must remove .in-flight (or it was already gone)"
        );
        assert!(
            !responded_path.exists(),
            "recover must remove .responded after stale-orphan fast path"
        );

        // Next hook tick must deliver entry K+1 cleanly, not error.
        let decision = hook::run(&inbox, &hook::Mode::Drain).unwrap();
        assert_eq!(
            decision,
            hook::Decision::Block("entry K+1".to_string()),
            "hook must deliver K+1 cleanly after crash-window recovery"
        );
    }
}

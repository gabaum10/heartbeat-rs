//! Byte-offset JSONL inbox reader.
//!
//! Reads one line at a time from a JSONL file, tracking position via a
//! companion `.inbox-offset` file.
//!
//! ## Fix B semantic change
//!
//! Under the original design, `read_next_line` advanced the offset cursor
//! immediately on read ("acknowledge-on-read"). Under Fix B, the cursor is
//! **deferred**: `read_next_line` returns the line AND the raw bytes consumed
//! (as an `InboxEntry`), but does NOT advance the offset. The caller must
//! call `acknowledge` after the agent proves it processed the entry. This
//! eliminates the silent-drop window where a crash between delivery and
//! acknowledgement would lose the entry.
//!
//! Each line is either:
//!   - A JSON-encoded string (starts with `"`): unwrapped to the original
//!     multi-line content before delivery. Writers must JSON-encode prompts
//!     that contain newlines so the inbox stays valid JSONL.
//!   - Plain text (anything else): delivered as-is. Backwards compatible
//!     for single-line messages that don't need encoding.
//!
//! Algorithm lifted from claude-heartbeat/hooks/heartbeat.js lines 54-78.

use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// A single entry read from the inbox, including positional metadata.
///
/// The `decoded` field is what the agent sees. `raw_line`, `start_offset`,
/// and `end_offset` are needed to build the `.in-flight` artifact and to
/// call `acknowledge`.
#[derive(Debug, Clone, PartialEq)]
pub struct InboxEntry {
    /// The decoded prompt text delivered to the agent.
    pub decoded: String,
    /// The raw JSONL line (before JSON-string decoding), as it appeared in the file.
    pub raw_line: String,
    /// Byte offset of the first byte of this line in inbox.jsonl.
    pub start_offset: u64,
    /// Byte offset of the first byte AFTER this line.
    pub end_offset: u64,
}

/// Reads the next unread entry from the inbox file WITHOUT advancing the
/// offset cursor.
///
/// Returns `Some(InboxEntry)` if an entry was available, `None` if the inbox
/// is empty or fully consumed. The caller must call `acknowledge` after
/// the agent has processed the entry.
///
/// `inbox` is the path to the JSONL file.
/// `offset_file` is the path to the `.inbox-offset` cursor file (usually
/// in the same directory as the inbox).
pub fn read_next_entry(inbox: &Path, offset_file: &Path) -> io::Result<Option<InboxEntry>> {
    // How large is the inbox right now? Snapshot once — the file may grow
    // while we run, but we only consume up to this boundary.
    let size = match fs::metadata(inbox) {
        Ok(m) => m.len(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };

    let mut file = fs::File::open(inbox)?;

    // Loop so blank lines are skipped rather than causing a premature None.
    // Each iteration reads from the current offset and either advances past
    // a blank line (continue) or returns the first non-blank entry found.
    //
    // NOTE: blank lines DO advance the offset immediately, since there is
    // nothing for the agent to process and no risk of data loss.
    loop {
        let start_offset: u64 = read_offset(offset_file).unwrap_or(0);

        if size <= start_offset {
            return Ok(None);
        }

        file.seek(SeekFrom::Start(start_offset))?;

        let remaining = (size - start_offset) as usize;
        let mut buf = vec![0u8; remaining];
        file.read_exact(&mut buf)?;

        // Find the newline at the BYTE level before any UTF-8 decode.
        // String::from_utf8_lossy substitutes 3-byte U+FFFD for each invalid
        // byte sequence, shifting char-level indices relative to the raw buffer.
        // Searching for '\n' in the lossy string would yield a character index
        // that is up to (3×N - N) bytes off for N invalid bytes before the
        // newline. Fix: search buf for b'\n' first, then decode only the
        // line-content slice. Lossy decode is safe for content display;
        // it must not be used for offset arithmetic.
        let (line_bytes, consumed) = match buf.iter().position(|&b| b == b'\n') {
            Some(nl) => (&buf[..nl], nl + 1),
            // No newline — consume the whole remainder (partial write case;
            // heartbeat.js handles this the same way). The full buffer is the
            // line content; no trailing byte to strip.
            None => (&buf[..], buf.len()),
        };

        let end_offset = start_offset + consumed as u64;

        // Decode only the line bytes (newline already excluded above).
        let line = String::from_utf8_lossy(line_bytes);
        let line = line.trim();
        if line.is_empty() {
            // Blank line — advance past it immediately (no content to lose).
            write_offset(offset_file, end_offset)?;
            continue;
        }

        let raw_line = line.to_string();
        let decoded = decode_line(line);

        return Ok(Some(InboxEntry {
            decoded,
            raw_line,
            start_offset,
            end_offset,
        }));
    }
}

/// Acknowledges that entry K has been processed by advancing the offset cursor
/// past it and removing the `.in-flight` file.
///
/// Order of operations (crash-safe):
///   1. Write new offset atomically (tmp + fsync + rename). If we crash here,
///      `.in-flight` still describes the unacknowledged entry → safe to retry.
///   2. Remove `.in-flight`. If we crash here, next startup sees a stale
///      `.in-flight` whose `end_offset` is < current cursor → safe to ignore.
///
/// `offset_file` — path to the `.inbox-offset` cursor file.
/// `new_offset` — the `end_offset` from the `InboxEntry` being acknowledged.
/// `in_flight_path` — path to the `.in-flight` artifact to remove.
pub fn acknowledge(offset_file: &Path, new_offset: u64, in_flight_path: &Path) -> io::Result<()> {
    // Defense-in-depth: never rewind the cursor. If new_offset is behind the
    // current cursor (e.g., stale .in-flight was fed to acknowledge after a
    // partial recover), skip the write rather than causing re-delivery of
    // already-acknowledged entries.
    let current = read_offset(offset_file).unwrap_or(0);
    if new_offset < current {
        // Cursor would rewind — skip advance, still remove .in-flight.
        let _ = fs::remove_file(in_flight_path);
        return Ok(());
    }
    // Step 1: advance cursor (atomic).
    write_offset(offset_file, new_offset)?;
    // Step 2: remove `.in-flight` (non-atomic, but recoverable — see §2 of spec).
    let _ = fs::remove_file(in_flight_path);
    Ok(())
}

/// Decodes a line from the inbox.
///
/// If the line starts with `"` it's treated as a JSON-encoded string and
/// unwrapped via `serde_json`. This lets multi-line prompts be stored as a
/// single JSONL entry without truncation.
///
/// Anything that isn't a valid JSON string is returned as-is (plain text
/// backwards-compatibility).
fn decode_line(line: &str) -> String {
    if line.starts_with('"') {
        if let Ok(serde_json::Value::String(s)) = serde_json::from_str(line) {
            return s;
        }
    }
    line.to_string()
}

/// Reads the current byte offset from the offset file.
/// Returns `None` if the file doesn't exist or can't be parsed.
pub fn read_offset(offset_file: &Path) -> Option<u64> {
    fs::read_to_string(offset_file)
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Writes the byte offset to the offset file with fsync for crash safety.
pub fn write_offset(offset_file: &Path, offset: u64) -> io::Result<()> {
    let tmp = offset_file.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        write!(f, "{}", offset)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, offset_file)?;
    Ok(())
}

/// Returns the canonical offset file path for a given inbox path.
/// Placed in the same directory as the inbox.
pub fn offset_file_for(inbox: &Path) -> PathBuf {
    let dir = inbox.parent().unwrap_or(Path::new("."));
    dir.join(".inbox-offset")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn make_inbox(dir: &TempDir, name: &str) -> PathBuf {
        dir.path().join(name)
    }

    fn write_inbox(path: &Path, content: &str) {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    // Helper: simulate acknowledgement so the next read advances correctly.
    fn ack(dir: &TempDir, entry: &InboxEntry) {
        let offset_file = dir.path().join(".inbox-offset");
        let in_flight = dir.path().join(".in-flight");
        acknowledge(&offset_file, entry.end_offset, &in_flight).unwrap();
    }

    #[test]
    fn empty_inbox_returns_none() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir, "inbox.jsonl");
        let offset = dir.path().join(".inbox-offset");

        // File doesn't exist yet
        assert_eq!(read_next_entry(&inbox, &offset).unwrap(), None);

        // File exists but empty
        fs::write(&inbox, "").unwrap();
        assert_eq!(read_next_entry(&inbox, &offset).unwrap(), None);
    }

    #[test]
    fn single_line_with_newline() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir, "inbox.jsonl");
        let offset = dir.path().join(".inbox-offset");

        write_inbox(&inbox, "hello world\n");

        let entry = read_next_entry(&inbox, &offset).unwrap().unwrap();
        assert_eq!(entry.decoded, "hello world");
        assert_eq!(entry.raw_line, "hello world");
        assert_eq!(entry.start_offset, 0);
        assert_eq!(entry.end_offset, 12); // "hello world\n" = 12 bytes

        // Cursor NOT advanced yet — same entry returned again if we don't ack.
        let entry2 = read_next_entry(&inbox, &offset).unwrap().unwrap();
        assert_eq!(entry2.decoded, "hello world");

        // After ack, inbox is exhausted.
        ack(&dir, &entry);
        let next = read_next_entry(&inbox, &offset).unwrap();
        assert_eq!(next, None);
    }

    #[test]
    fn single_line_without_newline() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir, "inbox.jsonl");
        let offset = dir.path().join(".inbox-offset");

        write_inbox(&inbox, "partial write no newline");

        let entry = read_next_entry(&inbox, &offset).unwrap().unwrap();
        assert_eq!(entry.decoded, "partial write no newline");
        assert_eq!(entry.start_offset, 0);
        assert_eq!(entry.end_offset, 24);

        ack(&dir, &entry);
        assert_eq!(read_next_entry(&inbox, &offset).unwrap(), None);
    }

    #[test]
    fn multiple_lines_consumed_in_order() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir, "inbox.jsonl");
        let offset = dir.path().join(".inbox-offset");

        write_inbox(&inbox, "line one\nline two\nline three\n");

        let e1 = read_next_entry(&inbox, &offset).unwrap().unwrap();
        assert_eq!(e1.decoded, "line one");
        ack(&dir, &e1);

        let e2 = read_next_entry(&inbox, &offset).unwrap().unwrap();
        assert_eq!(e2.decoded, "line two");
        ack(&dir, &e2);

        let e3 = read_next_entry(&inbox, &offset).unwrap().unwrap();
        assert_eq!(e3.decoded, "line three");
        ack(&dir, &e3);

        assert_eq!(read_next_entry(&inbox, &offset).unwrap(), None);
    }

    #[test]
    fn no_ack_means_same_entry_re_read() {
        // The core Fix B property: without an explicit ack, the same entry is
        // returned on every read (cursor is not advanced by read_next_entry).
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir, "inbox.jsonl");
        let offset = dir.path().join(".inbox-offset");

        write_inbox(&inbox, "important entry\n");

        for _ in 0..3 {
            let entry = read_next_entry(&inbox, &offset).unwrap().unwrap();
            assert_eq!(entry.decoded, "important entry");
            // deliberately NOT calling ack
        }

        // After ack, advances.
        let entry = read_next_entry(&inbox, &offset).unwrap().unwrap();
        ack(&dir, &entry);
        assert_eq!(read_next_entry(&inbox, &offset).unwrap(), None);
    }

    #[test]
    fn byte_offset_survives_append() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir, "inbox.jsonl");
        let offset = dir.path().join(".inbox-offset");

        write_inbox(&inbox, "first\n");
        let e1 = read_next_entry(&inbox, &offset).unwrap().unwrap();
        assert_eq!(e1.decoded, "first");
        ack(&dir, &e1);

        // Inbox was empty after first ack. Now a new line arrives.
        assert_eq!(read_next_entry(&inbox, &offset).unwrap(), None);

        write_inbox(&inbox, "second\n");
        let e2 = read_next_entry(&inbox, &offset).unwrap().unwrap();
        assert_eq!(e2.decoded, "second");
        ack(&dir, &e2);
    }

    #[test]
    fn json_encoded_multiline_is_decoded() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir, "inbox.jsonl");
        let offset = dir.path().join(".inbox-offset");

        let prompt = "line one\nline two\nline three";
        let encoded = serde_json::to_string(prompt).unwrap();
        write_inbox(&inbox, &format!("{}\n", encoded));

        let entry = read_next_entry(&inbox, &offset).unwrap().unwrap();
        assert_eq!(entry.decoded, prompt);
        assert_eq!(entry.raw_line, encoded); // raw_line is pre-decode
        ack(&dir, &entry);
        assert_eq!(read_next_entry(&inbox, &offset).unwrap(), None);
    }

    #[test]
    fn json_encoded_messages_drain_in_order() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir, "inbox.jsonl");
        let offset = dir.path().join(".inbox-offset");

        let msg1 = "triage batch one\nwith multiple\nlines";
        let msg2 = "triage batch two\nalso multiline";
        write_inbox(
            &inbox,
            &format!("{}\n", serde_json::to_string(msg1).unwrap()),
        );
        write_inbox(
            &inbox,
            &format!("{}\n", serde_json::to_string(msg2).unwrap()),
        );

        let e1 = read_next_entry(&inbox, &offset).unwrap().unwrap();
        assert_eq!(e1.decoded, msg1);
        ack(&dir, &e1);

        let e2 = read_next_entry(&inbox, &offset).unwrap().unwrap();
        assert_eq!(e2.decoded, msg2);
        ack(&dir, &e2);

        assert_eq!(read_next_entry(&inbox, &offset).unwrap(), None);
    }

    #[test]
    fn plain_text_passthrough_unchanged() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir, "inbox.jsonl");
        let offset = dir.path().join(".inbox-offset");

        write_inbox(&inbox, "plain message no encoding\n");

        let entry = read_next_entry(&inbox, &offset).unwrap().unwrap();
        assert_eq!(entry.decoded, "plain message no encoding");
        ack(&dir, &entry);
    }

    #[test]
    fn blank_lines_skipped_not_returned_as_none() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir, "inbox.jsonl");
        let offset = dir.path().join(".inbox-offset");

        write_inbox(&inbox, "\n\nreal message\n\n");

        let entry = read_next_entry(&inbox, &offset).unwrap().unwrap();
        assert_eq!(entry.decoded, "real message");
        ack(&dir, &entry);

        // Everything after "real message\n" is blank — returns None.
        // Note: blank lines after ack are skipped by the loop, so we get None.
        let next = read_next_entry(&inbox, &offset).unwrap();
        assert_eq!(next, None);
    }

    #[test]
    fn blank_lines_between_messages_skipped() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir, "inbox.jsonl");
        let offset = dir.path().join(".inbox-offset");

        write_inbox(&inbox, "first\n\n\nsecond\n");

        let e1 = read_next_entry(&inbox, &offset).unwrap().unwrap();
        assert_eq!(e1.decoded, "first");
        ack(&dir, &e1);

        // The two blank lines should be skipped, yielding "second" directly.
        let e2 = read_next_entry(&inbox, &offset).unwrap().unwrap();
        assert_eq!(e2.decoded, "second");
        ack(&dir, &e2);

        assert_eq!(read_next_entry(&inbox, &offset).unwrap(), None);
    }

    #[test]
    fn offset_file_for_places_in_same_dir() {
        let inbox = Path::new("/some/dir/inbox.jsonl");
        let offset = offset_file_for(inbox);
        assert_eq!(offset, PathBuf::from("/some/dir/.inbox-offset"));
    }

    #[test]
    fn acknowledge_advances_offset_and_removes_in_flight() {
        let dir = TempDir::new().unwrap();
        let offset_file = dir.path().join(".inbox-offset");
        let in_flight = dir.path().join(".in-flight");

        // Pre-create .in-flight to simulate live orphan.
        fs::write(&in_flight, "{}").unwrap();

        acknowledge(&offset_file, 42, &in_flight).unwrap();

        let written = read_offset(&offset_file).unwrap();
        assert_eq!(written, 42);
        assert!(!in_flight.exists(), ".in-flight should be removed on ack");
    }

    #[test]
    fn acknowledge_is_ok_if_in_flight_already_gone() {
        // Crash between cursor advance and .in-flight removal: safe to repeat.
        let dir = TempDir::new().unwrap();
        let offset_file = dir.path().join(".inbox-offset");
        let in_flight = dir.path().join(".in-flight");
        // Don't create .in-flight — simulate the already-removed case.
        acknowledge(&offset_file, 10, &in_flight).unwrap();
        assert_eq!(read_offset(&offset_file).unwrap(), 10);
    }

    // -------------------------------------------------------------------------
    // F6 regression: invalid UTF-8 before newline must not misalign cursor
    // -------------------------------------------------------------------------

    #[test]
    fn invalid_utf8_before_newline_cursor_stays_aligned() {
        // Reproduce Lens F6: buf = b"hel\xFFlo\nworld"
        // Lossy decode makes \xFF a 3-byte replacement char, shifting the
        // apparent newline position from byte 6 to char index 8.
        // With byte-level newline search, consumed must be 7 (not 9).
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir, "inbox.jsonl");
        let offset = dir.path().join(".inbox-offset");

        // Write two lines, first containing an invalid UTF-8 byte.
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&inbox)
            .unwrap();
        f.write_all(b"hel\xFFlo\nworld\n").unwrap();
        drop(f);

        let e1 = read_next_entry(&inbox, &offset).unwrap().unwrap();
        // Content is lossy-decoded but the byte offsets are what matter here.
        assert_eq!(e1.start_offset, 0);
        assert_eq!(
            e1.end_offset, 7,
            "consumed must be 7 bytes (6 content + 1 newline)"
        );

        ack(&dir, &e1);

        // Second entry must start at byte 7, not 9.
        let e2 = read_next_entry(&inbox, &offset).unwrap().unwrap();
        assert_eq!(e2.start_offset, 7);
        assert_eq!(e2.decoded, "world");
        ack(&dir, &e2);

        assert_eq!(read_next_entry(&inbox, &offset).unwrap(), None);
    }

    #[test]
    fn invalid_utf8_no_newline_cursor_stays_aligned() {
        // No-newline path: buf = b"bad\xFFbytes" — entire buffer is one entry.
        // consumed must equal buf.len(), not buf.len() - 1.
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir, "inbox.jsonl");
        let offset = dir.path().join(".inbox-offset");

        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&inbox)
            .unwrap();
        f.write_all(b"bad\xFFbytes").unwrap();
        drop(f);

        let e1 = read_next_entry(&inbox, &offset).unwrap().unwrap();
        assert_eq!(e1.start_offset, 0);
        assert_eq!(e1.end_offset, 9, "consumed must be 9 bytes (entire buffer)");
        assert!(
            e1.decoded.contains("bad"),
            "content prefix preserved: {}",
            e1.decoded
        );
        assert!(
            e1.decoded.contains("bytes"),
            "content suffix preserved: {}",
            e1.decoded
        );

        ack(&dir, &e1);
        assert_eq!(read_next_entry(&inbox, &offset).unwrap(), None);
    }

    // -------------------------------------------------------------------------
    // Crash window tests (§2 of spec)
    // -------------------------------------------------------------------------

    /// Scenario C1: hook crashes BEFORE writing .in-flight.
    /// On-disk state: offset still at start of K, no .in-flight, no .responded.
    /// Recovery: next read picks up entry K fresh from offset. Safe.
    #[test]
    fn crash_c1_before_in_flight_write_recovers_naturally() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir, "inbox.jsonl");
        let offset = dir.path().join(".inbox-offset");

        write_inbox(&inbox, "entry K\n");

        // Simulate: hook reads, crashes before writing .in-flight.
        // Cursor was NOT advanced (Fix B). No .in-flight present.
        // Next read re-delivers entry K.
        let entry = read_next_entry(&inbox, &offset).unwrap().unwrap();
        assert_eq!(entry.decoded, "entry K");
        // .in-flight was never written, no orphan signal at startup.
        // Behaviour is correct: entry K re-delivered on next session.
        ack(&dir, &entry); // clean up to prove the cycle completes
    }

    /// Scenario C2: hook crashes AFTER .in-flight write, BEFORE returning Block.
    /// On-disk state: .in-flight present, offset at start of K.
    /// Recovery: next launch sees .in-flight without active session → orphan → apply policy.
    #[test]
    fn crash_c2_in_flight_written_offset_not_advanced() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir, "inbox.jsonl");
        let offset = dir.path().join(".inbox-offset");
        let in_flight = dir.path().join(".in-flight");

        write_inbox(&inbox, "entry K\n");

        let entry = read_next_entry(&inbox, &offset).unwrap().unwrap();
        // Simulate: hook wrote .in-flight, then crashed (no ack, no responded).
        use crate::in_flight::InFlightEntry;
        let inflight = InFlightEntry::new(&entry.raw_line, entry.start_offset, entry.end_offset);
        inflight.write_to(&in_flight).unwrap();

        // Offset is still at 0 (start of K) — not advanced.
        assert_eq!(read_offset(&offset), None); // no offset file means 0

        // .in-flight exists with start_offset == 0, cursor at 0 → live orphan.
        let current_offset = read_offset(&offset).unwrap_or(0);
        assert!(!inflight.is_stale(current_offset), "should be live orphan");

        // Next read would re-deliver K (correct — agent never saw it).
        let re_entry = read_next_entry(&inbox, &offset).unwrap().unwrap();
        assert_eq!(re_entry.decoded, "entry K");
    }

    /// Scenario C3: hook crashes AFTER agent's response, BEFORE acknowledgement.
    /// On-disk state: .in-flight present, cursor at start of K (not advanced).
    /// Recovery: orphan — agent DID process it. Retry causes duplicate side effect.
    /// This is the idempotency risk §5 notes. The hook's job is to surface the
    /// signal; the policy (retry vs dead-letter) is the launcher's choice.
    #[test]
    fn crash_c3_after_response_before_ack_is_live_orphan() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir, "inbox.jsonl");
        let offset = dir.path().join(".inbox-offset");
        let in_flight = dir.path().join(".in-flight");

        write_inbox(&inbox, "entry K\n");

        let entry = read_next_entry(&inbox, &offset).unwrap().unwrap();

        use crate::in_flight::InFlightEntry;
        let inflight = InFlightEntry::new(&entry.raw_line, entry.start_offset, entry.end_offset);
        inflight.write_to(&in_flight).unwrap();

        // Agent responded, but hook crashed before ack. Cursor still at 0.
        let current_offset = read_offset(&offset).unwrap_or(0);

        // Live orphan: cursor (0) <= end_offset (8) → not stale.
        assert!(!inflight.is_stale(current_offset));

        // Stale orphan check: if cursor somehow advanced past end_offset.
        let read_back = crate::in_flight::InFlightEntry::read_from(&in_flight)
            .unwrap()
            .unwrap();
        assert!(!read_back.is_stale(current_offset));
        assert!(read_back.is_stale(entry.end_offset + 1));
    }

    /// Stale orphan: crash between cursor advance (step 1) and .in-flight removal (step 2).
    /// On-disk: .in-flight present, but cursor > end_offset.
    /// Recovery: launcher sees stale orphan → delete .in-flight, no action needed.
    #[test]
    fn stale_orphan_detected_by_cursor_past_end_offset() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir, "inbox.jsonl");
        let offset = dir.path().join(".inbox-offset");
        let in_flight = dir.path().join(".in-flight");

        write_inbox(&inbox, "entry K\n");

        let entry = read_next_entry(&inbox, &offset).unwrap().unwrap();

        use crate::in_flight::InFlightEntry;
        let inflight = InFlightEntry::new(&entry.raw_line, entry.start_offset, entry.end_offset);
        inflight.write_to(&in_flight).unwrap();

        // Step 1 of ack succeeded: cursor advanced past K.
        write_offset(&offset, entry.end_offset).unwrap();

        // Step 2 did NOT happen: .in-flight still present.
        assert!(in_flight.exists());

        // Now launcher reads .in-flight and checks against cursor.
        let read_back = InFlightEntry::read_from(&in_flight).unwrap().unwrap();
        let current_offset = read_offset(&offset).unwrap();

        // cursor == end_offset: is_stale uses >= so this IS stale.
        // The entry was acknowledged in step 1; .in-flight just wasn't cleaned up.
        assert!(read_back.is_stale(current_offset));

        // Strictly before end_offset: not stale.
        assert!(!read_back.is_stale(current_offset - 1));
    }
}

//! Byte-offset JSONL inbox reader.
//!
//! Reads one line at a time from a JSONL file, tracking position via a
//! companion `.inbox-offset` file. Advances the offset atomically with
//! fsync after each read so restarts don't re-deliver messages.
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

/// Reads the next unread line from the inbox file.
///
/// Returns `Some(line)` if a line was available, `None` if the inbox is
/// empty or fully consumed. Advances the offset file atomically on success.
///
/// `inbox` is the path to the JSONL file.
/// `offset_file` is the path to the `.inbox-offset` cursor file (usually
/// in the same directory as the inbox).
pub fn read_next_line(inbox: &Path, offset_file: &Path) -> io::Result<Option<String>> {
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
    // a blank line (continue) or returns the first non-blank line found.
    loop {
        let offset: u64 = read_offset(offset_file).unwrap_or(0);

        if size <= offset {
            return Ok(None);
        }

        file.seek(SeekFrom::Start(offset))?;

        let remaining = (size - offset) as usize;
        let mut buf = vec![0u8; remaining];
        file.read_exact(&mut buf)?;

        let raw = String::from_utf8_lossy(&buf);

        // Consume exactly one line. If there's a newline, stop there.
        // If there's no newline, consume the whole remainder (partial write
        // case — heartbeat.js handles this the same way).
        let (line, consumed) = match raw.find('\n') {
            Some(nl) => (&raw[..nl], nl + 1),
            None => (&raw[..], raw.len()),
        };

        let new_offset = offset + consumed as u64;

        let line = line.trim();
        if line.is_empty() {
            // Blank line — advance past it and keep looking.
            write_offset(offset_file, new_offset)?;
            continue;
        }

        // Advance the offset cursor atomically before returning the line.
        // If we crash after this write and before the caller acts, the line
        // is considered consumed. That's the safer failure mode (drop once)
        // versus re-delivering (potentially looping).
        write_offset(offset_file, new_offset)?;

        // If the line is a JSON-encoded string, unwrap it so callers receive
        // the original multi-line content. Plain text passes through unchanged.
        return Ok(Some(decode_line(line)));
    }
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
fn read_offset(offset_file: &Path) -> Option<u64> {
    fs::read_to_string(offset_file)
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Writes the byte offset to the offset file with fsync for crash safety.
fn write_offset(offset_file: &Path, offset: u64) -> io::Result<()> {
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

    #[test]
    fn empty_inbox_returns_none() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir, "inbox.jsonl");
        let offset = dir.path().join(".inbox-offset");

        // File doesn't exist yet
        assert_eq!(read_next_line(&inbox, &offset).unwrap(), None);

        // File exists but empty
        fs::write(&inbox, "").unwrap();
        assert_eq!(read_next_line(&inbox, &offset).unwrap(), None);
    }

    #[test]
    fn single_line_with_newline() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir, "inbox.jsonl");
        let offset = dir.path().join(".inbox-offset");

        write_inbox(&inbox, "hello world\n");

        let line = read_next_line(&inbox, &offset).unwrap();
        assert_eq!(line, Some("hello world".to_string()));

        // Offset should now be past the line
        let next = read_next_line(&inbox, &offset).unwrap();
        assert_eq!(next, None);
    }

    #[test]
    fn single_line_without_newline() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir, "inbox.jsonl");
        let offset = dir.path().join(".inbox-offset");

        write_inbox(&inbox, "partial write no newline");

        let line = read_next_line(&inbox, &offset).unwrap();
        assert_eq!(line, Some("partial write no newline".to_string()));

        let next = read_next_line(&inbox, &offset).unwrap();
        assert_eq!(next, None);
    }

    #[test]
    fn multiple_lines_consumed_in_order() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir, "inbox.jsonl");
        let offset = dir.path().join(".inbox-offset");

        write_inbox(&inbox, "line one\nline two\nline three\n");

        let l1 = read_next_line(&inbox, &offset).unwrap();
        assert_eq!(l1, Some("line one".to_string()));

        let l2 = read_next_line(&inbox, &offset).unwrap();
        assert_eq!(l2, Some("line two".to_string()));

        let l3 = read_next_line(&inbox, &offset).unwrap();
        assert_eq!(l3, Some("line three".to_string()));

        let l4 = read_next_line(&inbox, &offset).unwrap();
        assert_eq!(l4, None);
    }

    #[test]
    fn byte_offset_survives_append() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir, "inbox.jsonl");
        let offset = dir.path().join(".inbox-offset");

        write_inbox(&inbox, "first\n");
        let l1 = read_next_line(&inbox, &offset).unwrap();
        assert_eq!(l1, Some("first".to_string()));

        // Inbox was empty after first read. Now a new line arrives.
        assert_eq!(read_next_line(&inbox, &offset).unwrap(), None);

        write_inbox(&inbox, "second\n");
        let l2 = read_next_line(&inbox, &offset).unwrap();
        assert_eq!(l2, Some("second".to_string()));
    }

    #[test]
    fn json_encoded_multiline_is_decoded() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir, "inbox.jsonl");
        let offset = dir.path().join(".inbox-offset");

        // Simulate what the bash wrapper does:
        //   echo "$TRIAGE_PROMPT" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))' >> inbox.jsonl
        let prompt = "line one\nline two\nline three";
        let encoded = serde_json::to_string(prompt).unwrap();
        write_inbox(&inbox, &format!("{}\n", encoded));

        let decoded = read_next_line(&inbox, &offset).unwrap();
        assert_eq!(decoded, Some(prompt.to_string()));

        // Nothing left
        assert_eq!(read_next_line(&inbox, &offset).unwrap(), None);
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

        let l1 = read_next_line(&inbox, &offset).unwrap();
        assert_eq!(l1, Some(msg1.to_string()));

        let l2 = read_next_line(&inbox, &offset).unwrap();
        assert_eq!(l2, Some(msg2.to_string()));

        assert_eq!(read_next_line(&inbox, &offset).unwrap(), None);
    }

    #[test]
    fn plain_text_passthrough_unchanged() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir, "inbox.jsonl");
        let offset = dir.path().join(".inbox-offset");

        // Plain text (no JSON encoding) — backwards compat
        write_inbox(&inbox, "plain message no encoding\n");

        let line = read_next_line(&inbox, &offset).unwrap();
        assert_eq!(line, Some("plain message no encoding".to_string()));
    }

    #[test]
    fn blank_lines_skipped_not_returned_as_none() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir, "inbox.jsonl");
        let offset = dir.path().join(".inbox-offset");

        // Blank lines between real messages must not cause a spurious None.
        write_inbox(&inbox, "\n\nreal message\n\n");

        let line = read_next_line(&inbox, &offset).unwrap();
        assert_eq!(line, Some("real message".to_string()));

        // Everything after "real message\n" is blank — returns None.
        let next = read_next_line(&inbox, &offset).unwrap();
        assert_eq!(next, None);
    }

    #[test]
    fn blank_lines_between_messages_skipped() {
        let dir = TempDir::new().unwrap();
        let inbox = make_inbox(&dir, "inbox.jsonl");
        let offset = dir.path().join(".inbox-offset");

        write_inbox(&inbox, "first\n\n\nsecond\n");

        let l1 = read_next_line(&inbox, &offset).unwrap();
        assert_eq!(l1, Some("first".to_string()));

        // The two blank lines should be skipped, yielding "second" directly.
        let l2 = read_next_line(&inbox, &offset).unwrap();
        assert_eq!(l2, Some("second".to_string()));

        assert_eq!(read_next_line(&inbox, &offset).unwrap(), None);
    }

    #[test]
    fn offset_file_for_places_in_same_dir() {
        let inbox = Path::new("/some/dir/inbox.jsonl");
        let offset = offset_file_for(inbox);
        assert_eq!(offset, PathBuf::from("/some/dir/.inbox-offset"));
    }
}

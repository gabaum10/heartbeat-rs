//! `.in-flight` artifact: the on-disk sentinel for mid-delivery entries.
//!
//! Written by the hook when an entry is delivered to the agent. Cleared by
//! the hook when the agent acknowledges (next hook tick). Persists across
//! launcher restarts so crash recovery can distinguish stale orphans from
//! live ones.
//!
//! Schema (single-line JSON):
//! ```json
//! {
//!   "entry_id": "<sha256 of raw_line>",
//!   "start_offset": 0,
//!   "end_offset": 4096,
//!   "raw_line": "<the JSONL line as it appeared in inbox.jsonl, NOT decoded>",
//!   "delivered_at": "2026-05-15T14:23:11Z"
//! }
//! ```
//!
//! `entry_id` is SHA256(raw_line) hex-encoded. Same raw bytes → same ID across
//! retries, which lets downstream consumers deduplicate idempotently.
//!
//! `start_offset` / `end_offset` let the launcher distinguish:
//!   - stale orphan: cursor > end_offset (already acknowledged in a prior step)
//!   - live orphan:  cursor == start_offset (never acknowledged)

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{HeartbeatError, Result};

use hex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The `.in-flight` artifact written to disk while an entry is being processed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InFlightEntry {
    /// SHA256 of `raw_line`, hex-encoded. Stable across retries of the same line.
    pub entry_id: String,
    /// Byte offset of the start of this entry in inbox.jsonl.
    pub start_offset: u64,
    /// Byte offset of the first byte AFTER this entry (i.e. start + len(raw_line + newline)).
    pub end_offset: u64,
    /// The raw JSONL line as it appeared in the inbox, undecoded.
    pub raw_line: String,
    /// ISO 8601 UTC timestamp when the entry was delivered to the agent.
    pub delivered_at: String,
}

impl InFlightEntry {
    /// Construct a new `InFlightEntry` for the given raw inbox line.
    ///
    /// `raw_line` — the exact bytes from the inbox (before JSON-decoding).
    /// `start_offset` — byte position of the first byte of this line.
    /// `end_offset` — byte position of the first byte after this line
    ///                (i.e., start_offset + len(raw_line) + 1 for the newline).
    pub fn new(raw_line: &str, start_offset: u64, end_offset: u64) -> Self {
        InFlightEntry {
            entry_id: sha256_hex(raw_line),
            start_offset,
            end_offset,
            raw_line: raw_line.to_string(),
            delivered_at: utc_now_iso8601(),
        }
    }

    /// Write this entry to `path` atomically (tmp + fsync + rename).
    pub fn write_to(&self, path: &Path) -> Result<()> {
        let tmp = path.with_extension("tmp");
        let write_err = |e: io::Error| HeartbeatError::InFlightWrite {
            path: path.to_owned(),
            source: e,
        };
        {
            let mut f = fs::File::create(&tmp).map_err(write_err)?;
            // serde_json::to_string only fails on non-serializable types; our
            // struct derives Serialize so this is infallible in practice.
            // Map it to an IO error anyway for the rare edge case.
            let json = serde_json::to_string(self)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
                .map_err(write_err)?;
            f.write_all(json.as_bytes()).map_err(write_err)?;
            f.sync_all().map_err(write_err)?;
        }
        fs::rename(&tmp, path).map_err(write_err)?;
        Ok(())
    }

    /// Read and parse an `.in-flight` file. Returns `Ok(None)` if the file
    /// does not exist. Returns `Err(InFlightCorrupt)` for JSON parse errors.
    /// Returns `Err(InFlightRead)` for other IO errors.
    pub fn read_from(path: &Path) -> Result<Option<Self>> {
        match fs::read_to_string(path) {
            Ok(s) => {
                let entry: InFlightEntry =
                    serde_json::from_str(&s).map_err(|e| HeartbeatError::InFlightCorrupt {
                        path: path.to_owned(),
                        source: e,
                    })?;
                Ok(Some(entry))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(HeartbeatError::InFlightRead {
                path: path.to_owned(),
                source: e,
            }),
        }
    }

    /// Returns `true` if this in-flight entry is stale — meaning the offset
    /// cursor has already reached or passed this entry's end in a prior
    /// acknowledgement step (crash between cursor advance and `.in-flight`
    /// removal).
    ///
    /// When `current_offset >= end_offset`, the entry is fully past the
    /// cursor: it was acknowledged in step 1 but `.in-flight` removal was
    /// interrupted. Safe to delete without applying orphan policy.
    ///
    /// Uses `>=` to match `recover`'s stale check (recover.rs).
    pub fn is_stale(&self, current_offset: u64) -> bool {
        current_offset >= self.end_offset
    }
}

/// Returns the canonical `.in-flight` file path for a given inbox path.
pub fn in_flight_file_for(inbox: &Path) -> PathBuf {
    let dir = inbox.parent().unwrap_or(Path::new("."));
    dir.join(".in-flight")
}

/// SHA256 of `s`, returned as a lowercase hex string.
pub fn sha256_hex(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    hex::encode(hasher.finalize())
}

/// Current UTC time formatted as ISO 8601 (seconds precision).
/// Falls back to epoch if the system clock is unavailable.
fn utc_now_iso8601() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Format: YYYY-MM-DDTHH:MM:SSZ — hand-rolled to avoid a chrono dependency.
    let s = secs;
    let seconds = s % 60;
    let minutes = (s / 60) % 60;
    let hours = (s / 3600) % 24;
    let days_since_epoch = s / 86400;

    // Gregorian calendar computation from day count.
    let (year, month, day) = days_since_epoch_to_ymd(days_since_epoch);

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month, day, hours, minutes, seconds
    )
}

/// Convert days since Unix epoch (1970-01-01) to (year, month, day).
fn days_since_epoch_to_ymd(days: u64) -> (u64, u64, u64) {
    // Algorithm: civil calendar from Howard Hinnant's date algorithms.
    let z = days + 719468;
    let era = z / 146097;
    let doe = z % 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn entry_id_is_sha256_of_raw_line() {
        let e = InFlightEntry::new("hello world", 0, 12);
        let expected = sha256_hex("hello world");
        assert_eq!(e.entry_id, expected);
    }

    #[test]
    fn entry_id_stable_for_same_raw_line() {
        let e1 = InFlightEntry::new("same line", 0, 10);
        let e2 = InFlightEntry::new("same line", 0, 10);
        assert_eq!(e1.entry_id, e2.entry_id);
    }

    #[test]
    fn entry_id_differs_for_different_raw_lines() {
        let e1 = InFlightEntry::new("line A", 0, 7);
        let e2 = InFlightEntry::new("line B", 0, 7);
        assert_ne!(e1.entry_id, e2.entry_id);
    }

    #[test]
    fn round_trip_write_read() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".in-flight");

        let original = InFlightEntry::new("test entry line", 0, 16);
        original.write_to(&path).unwrap();

        let read_back = InFlightEntry::read_from(&path).unwrap().unwrap();
        assert_eq!(original.entry_id, read_back.entry_id);
        assert_eq!(original.start_offset, read_back.start_offset);
        assert_eq!(original.end_offset, read_back.end_offset);
        assert_eq!(original.raw_line, read_back.raw_line);
    }

    #[test]
    fn read_from_missing_file_returns_none() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".in-flight");
        let result = InFlightEntry::read_from(&path).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn is_stale_when_cursor_past_end_offset() {
        let e = InFlightEntry::new("line", 10, 15);
        assert!(e.is_stale(16));
        assert!(e.is_stale(100));
    }

    #[test]
    fn is_not_stale_when_cursor_before_end_offset() {
        let e = InFlightEntry::new("line", 10, 15);
        // cursor strictly before end_offset → live orphan
        assert!(!e.is_stale(14));
        assert!(!e.is_stale(10)); // cursor at start = live
        assert!(!e.is_stale(0)); // cursor before start = live
    }

    #[test]
    fn is_stale_when_cursor_at_end_offset() {
        // cursor == end_offset: entry is past the cursor (byte position after
        // the entry's last byte). Treat as stale — already acknowledged in
        // step 1 of the ack sequence.
        let e = InFlightEntry::new("line", 10, 15);
        assert!(e.is_stale(15));
    }

    #[test]
    fn in_flight_file_for_places_in_same_dir() {
        let inbox = Path::new("/some/dir/inbox.jsonl");
        let path = in_flight_file_for(inbox);
        assert_eq!(path, PathBuf::from("/some/dir/.in-flight"));
    }

    #[test]
    fn utc_timestamp_format_is_iso8601() {
        let ts = utc_now_iso8601();
        // YYYY-MM-DDTHH:MM:SSZ
        assert!(ts.ends_with('Z'), "timestamp must end with Z: {}", ts);
        assert_eq!(ts.len(), 20, "timestamp must be 20 chars: {}", ts);
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[7..8], "-");
        assert_eq!(&ts[10..11], "T");
    }

    #[test]
    fn write_is_atomic_via_tmp_rename() {
        // After write, the .in-flight file must exist and the .tmp must not.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".in-flight");
        let tmp = path.with_extension("tmp");

        let entry = InFlightEntry::new("atomic test", 0, 12);
        entry.write_to(&path).unwrap();

        assert!(path.exists(), ".in-flight must exist after write");
        assert!(
            !tmp.exists(),
            ".in-flight.tmp must be cleaned up after rename"
        );
    }

    // -------------------------------------------------------------------------
    // NEW: error-path tests for typed HeartbeatError variants (lil-grabby §9)
    // -------------------------------------------------------------------------

    /// Test 4 (§9.4): corrupt .in-flight JSON → read_from →
    /// Err(HeartbeatError::InFlightCorrupt { path, source }) with correct path.
    #[test]
    fn corrupt_in_flight_json_read_from_returns_in_flight_corrupt() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".in-flight");

        // Write invalid JSON to .in-flight.
        fs::write(&path, "{not valid json at all").unwrap();

        let result = InFlightEntry::read_from(&path);
        match result {
            Err(crate::error::HeartbeatError::InFlightCorrupt {
                path: err_path,
                source: _,
            }) => {
                assert_eq!(
                    err_path, path,
                    "InFlightCorrupt path must be the .in-flight file path"
                );
            }
            other => panic!(
                "expected Err(HeartbeatError::InFlightCorrupt), got {:?}",
                other
            ),
        }
    }

    /// Extra: truncated/empty file → read_from → Err(InFlightCorrupt).
    /// An empty file is valid UTF-8 but invalid JSON — same variant.
    #[test]
    fn empty_in_flight_file_returns_in_flight_corrupt() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".in-flight");

        fs::write(&path, "").unwrap();

        let result = InFlightEntry::read_from(&path);
        match result {
            Err(crate::error::HeartbeatError::InFlightCorrupt { .. }) => {}
            other => panic!(
                "expected Err(HeartbeatError::InFlightCorrupt) for empty file, got {:?}",
                other
            ),
        }
    }
}

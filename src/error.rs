//! Error types for the heartbeat-rs library.
//!
//! Library functions return `Result<T, HeartbeatError>`. The binary
//! translates errors into exit codes and stderr messages. Discriminated
//! variants only — no catch-all `Io(io::Error)`, because the entire point
//! of a typed error is matchability.

use std::io;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum HeartbeatError {
    /// IO error while reading the inbox.
    #[error("inbox read error: {0}")]
    InboxRead(#[source] io::Error),

    /// Offset file exists but its content is not a valid u64.
    /// `content` is the trimmed string we tried to parse.
    #[error("offset file corrupt at {path}: expected numeric offset, got {content:?}")]
    OffsetCorrupt { path: PathBuf, content: String },

    /// IO error while reading the offset file (other than `NotFound`).
    #[error("offset file read error at {path}: {source}")]
    OffsetRead {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// IO error while writing the offset file.
    #[error("offset file write error at {path}: {source}")]
    OffsetWrite {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// `.in-flight` file exists but its JSON is corrupt.
    #[error(".in-flight corrupt at {path}: {source}")]
    InFlightCorrupt {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    /// IO error while reading the `.in-flight` file (other than `NotFound`).
    #[error(".in-flight read error at {path}: {source}")]
    InFlightRead {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// IO error while writing the `.in-flight` file.
    #[error(".in-flight write error at {path}: {source}")]
    InFlightWrite {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// `.responded` present without `.in-flight` — recovery state inconsistent.
    #[error(
        "inconsistent state: .responded present without .in-flight at {io_dir}; \
         run `heartbeat-stop recover`"
    )]
    InconsistentState { io_dir: PathBuf },

    /// IO error while writing the dead-letter file during orphan recovery.
    #[error("dead-letter write error at {path}: {source}")]
    DeadLetterWrite {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Library-wide result alias.
pub type Result<T> = std::result::Result<T, HeartbeatError>;

//! heartbeat-rs: Stop hook and PTY launcher for autonomous Claude Code agent loops.
//!
//! This library provides the core primitives used by the `heartbeat-stop` stop hook
//! binary, and the PTY spawn layer used by the `heartbeat-launch` binary (feature-gated
//! behind `--features launch`).

pub mod error;
pub use error::{HeartbeatError, Result};

pub mod hook;
pub mod in_flight;
pub mod inbox;
#[cfg(feature = "launch")]
pub mod pty;
pub mod recover;

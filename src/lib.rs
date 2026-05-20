//! heartbeat-rs: Stop hook implementation for autonomous Claude Code agent loops.
//!
//! This library provides the core primitives used by the `heartbeat-stop` binary,
//! and the PTY spawn layer used by the `heartbeat-launch` binary.

pub mod error;
pub use error::{HeartbeatError, Result};

pub mod hook;
pub mod in_flight;
pub mod inbox;
#[cfg(feature = "launch")]
pub mod pty;
pub mod recover;

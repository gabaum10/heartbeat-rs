//! PTY allocation and child spawning for heartbeat-launch.
//!
//! Provides a thin wrapper around `portable-pty` that:
//! - Allocates a PTY with configurable dimensions
//! - Spawns an arbitrary command inside it
//! - Streams child stdout to the caller via a background thread
//! - Polls for child exit with a configurable timeout
//!
//! No inbox, no settings.json, no handshake. The consumer handles all of that.

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Result of a PTY session.
#[derive(Debug)]
pub struct RunResult {
    /// Exit code from the child process.
    pub exit_code: u32,
    /// Whether the session timed out before the child exited.
    pub timed_out: bool,
}

/// Errors from PTY operations.
#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    #[error("failed to open PTY: {0}")]
    Open(anyhow::Error),
    #[error("failed to spawn command: {0}")]
    Spawn(anyhow::Error),
    #[error("failed to clone PTY reader: {0}")]
    Reader(anyhow::Error),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("timeout: child did not exit within {0}s")]
    Timeout(u64),
}

/// Allocate a PTY, spawn `argv` inside it, stream stdout to the current
/// process's stdout, and poll until the child exits or `timeout_secs` elapses.
///
/// `timeout_secs == 0` means no timeout.
pub fn run(argv: &[String], cwd: &Path, timeout_secs: u64) -> Result<RunResult, PtyError> {
    let pty_system = native_pty_system();

    let pair = pty_system
        .openpty(PtySize {
            rows: 50,
            cols: 200,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(PtyError::Open)?;

    let mut cmd = CommandBuilder::new(&argv[0]);
    if argv.len() > 1 {
        cmd.args(&argv[1..]);
    }
    cmd.cwd(cwd);

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(PtyError::Spawn)?;

    // Drop slave so the master sees EOF when the child exits.
    drop(pair.slave);

    // Clone the killer before moving master into the reader thread.
    let mut killer = child.clone_killer();

    // Clone a reader from the master and spin a background thread to forward
    // output. The thread owns the reader; we join it after the child exits.
    let mut reader = pair.master.try_clone_reader().map_err(PtyError::Reader)?;

    // Shared flag: main thread signals reader thread to stop on timeout.
    let stop = Arc::new(Mutex::new(false));
    let stop_reader = Arc::clone(&stop);

    let reader_thread = thread::spawn(move || {
        let stdout = io::stdout();
        let mut buf = [0u8; 4096];
        loop {
            // Check stop flag before blocking read.
            {
                if *stop_reader.lock().unwrap() {
                    break;
                }
            }
            match reader.read(&mut buf) {
                Ok(0) => break, // EOF: slave side closed
                Ok(n) => {
                    let mut out = stdout.lock();
                    // Best-effort: if stdout is broken (consumer closed pipe), stop.
                    if out.write_all(&buf[..n]).is_err() {
                        break;
                    }
                    let _ = out.flush();
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break, // PTY closed or child exited
            }
        }
    });

    // Poll loop: check child exit, enforce timeout.
    let poll_interval = Duration::from_millis(100);
    let deadline = if timeout_secs > 0 {
        Some(Instant::now() + Duration::from_secs(timeout_secs))
    } else {
        None
    };

    let exit_code;
    let timed_out;

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                exit_code = status.exit_code();
                timed_out = false;
                break;
            }
            Ok(None) => {
                // Still running.
                if let Some(dl) = deadline {
                    if Instant::now() >= dl {
                        // Signal stop to reader thread, kill child.
                        *stop.lock().unwrap() = true;
                        let _ = killer.kill();
                        // Give it a moment to die, then claim the exit code.
                        thread::sleep(Duration::from_millis(500));
                        exit_code = match child.try_wait() {
                            Ok(Some(s)) => s.exit_code(),
                            _ => 1,
                        };
                        timed_out = true;
                        break;
                    }
                }
                thread::sleep(poll_interval);
            }
            Err(e) => return Err(PtyError::Io(e)),
        }
    }

    // Signal reader thread to finish and join.
    *stop.lock().unwrap() = true;
    // Drop master to unblock any pending read.
    drop(pair.master);
    let _ = reader_thread.join();

    if timed_out {
        return Err(PtyError::Timeout(timeout_secs));
    }

    Ok(RunResult {
        exit_code,
        timed_out: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp() -> PathBuf {
        std::env::temp_dir()
    }

    /// Spawn `echo hello` inside a PTY, capture stdout, verify output and exit code.
    #[test]
    fn echo_hello_exit_zero() {
        // Redirect PTY output to a temp file so we can inspect it.
        // We can't easily capture stdout from within the same process in a
        // unit test, so we verify exit code here and test output capture in
        // the integration test.
        let result = run(&["echo".to_string(), "hello".to_string()], &tmp(), 10)
            .expect("run should succeed");
        assert_eq!(result.exit_code, 0, "echo should exit 0");
        assert!(!result.timed_out);
    }

    /// A command that exits non-zero propagates the exit code.
    #[test]
    fn nonzero_exit_code_propagated() {
        // `false` always exits 1.
        let result = run(&["false".to_string()], &tmp(), 10).expect("run should succeed");
        assert_ne!(result.exit_code, 0, "false should exit non-zero");
    }

    /// Timeout fires and returns PtyError::Timeout.
    #[test]
    fn timeout_fires() {
        // Sleep for 60s but give it only 1s timeout.
        let err = run(
            &["sleep".to_string(), "60".to_string()],
            &tmp(),
            1,
        )
        .expect_err("should time out");
        match err {
            PtyError::Timeout(secs) => assert_eq!(secs, 1),
            other => panic!("expected Timeout, got {other:?}"),
        }
    }
}

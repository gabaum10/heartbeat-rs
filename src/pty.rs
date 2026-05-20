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
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Result of a PTY session.
#[derive(Debug)]
pub struct RunResult {
    /// Exit code from the child process.
    pub exit_code: u32,
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

/// Shut down the reader thread cleanly.
///
/// Sets the stop flag, drops the PTY master (which causes EOF on the cloned
/// reader, unblocking any pending `read()`), then waits up to 2 seconds for
/// the thread to finish. If the thread is still alive after the deadline it
/// is abandoned rather than blocking forever — this handles the known ConPTY
/// behaviour on Windows where the pipe handle may not deliver EOF even after
/// the child exits.
fn join_reader(
    handle: JoinHandle<()>,
    stop: &Arc<Mutex<bool>>,
    master: Box<dyn portable_pty::MasterPty + Send>,
) {
    // Signal best-effort early exit inside the read loop.
    if let Ok(mut g) = stop.lock() {
        *g = true;
    }
    // Dropping master sends EOF to the cloned reader, which is the primary
    // mechanism for unblocking the thread's blocking read() call.
    drop(master);

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if handle.is_finished() {
            let _ = handle.join();
            break;
        }
        if Instant::now() > deadline {
            // Thread hung (known ConPTY issue on Windows). Abandon it.
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
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
            // Real shutdown comes from drop(pair.master) causing EOF on the
            // reader; this flag is a best-effort early exit on timeout.
            if stop_reader.lock().map_or(false, |g| *g) {
                break;
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

    let exit_code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.exit_code(),
            Ok(None) => {
                // Still running.
                if let Some(dl) = deadline {
                    if Instant::now() >= dl {
                        // Timed out. Send SIGHUP via the cloned killer, wait briefly,
                        // then escalate to a hard kill if the child is still alive.
                        // Escalation matters on Unix when the child catches SIGHUP.
                        let _ = killer.kill(); // SIGHUP
                        thread::sleep(Duration::from_millis(500));
                        if matches!(child.try_wait(), Ok(None)) {
                            // Still alive after SIGHUP — force kill.
                            let _ = child.kill();
                            thread::sleep(Duration::from_millis(200));
                        }
                        // Clean up reader thread and return Err.
                        join_reader(reader_thread, &stop, pair.master);
                        return Err(PtyError::Timeout(timeout_secs));
                    }
                }
                thread::sleep(poll_interval);
            }
            Err(e) => return Err(PtyError::Io(e)),
        }
    };

    // Normal exit: shut down reader thread.
    join_reader(reader_thread, &stop, pair.master);

    Ok(RunResult { exit_code })
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

//! PTY allocation and child spawning for heartbeat-launch.
//!
//! Provides a thin wrapper around `portable-pty` that:
//! - Allocates a PTY with configurable dimensions
//! - Spawns an arbitrary command inside it
//! - Streams child stdout to the caller via a background thread
//! - Polls for child exit with a configurable timeout
//!
//! No inbox, no settings.json, no handshake. The consumer handles all of that.

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[cfg(unix)]
use libc;

/// Result of a PTY session.
#[derive(Debug)]
pub struct RunResult {
    /// Exit code from the child process.
    pub exit_code: u32,
}

/// Errors from PTY operations.
#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    /// PTY pair could not be allocated. The inner error is from `portable-pty`
    /// and typically indicates the OS refused to open a new PTY device (e.g.
    /// `/dev/ptmx` unavailable or `/dev/pts` not mounted).
    #[error("failed to open PTY: {0}")]
    Open(anyhow::Error),

    /// The command could not be spawned inside the PTY slave. Common causes:
    /// executable not found on PATH, permission denied, or the slave fd was
    /// already closed before `spawn_command` was called.
    #[error("failed to spawn command: {0}")]
    Spawn(anyhow::Error),

    /// The PTY master could not be cloned into a read-only handle for the
    /// background reader thread. This is fatal: without a reader the child's
    /// stdout would block once the PTY buffer fills.
    #[error("failed to clone PTY reader: {0}")]
    Reader(anyhow::Error),

    /// An unexpected I/O error occurred while polling the child process status.
    /// Normal child exit and PTY EOF are handled without surfacing this variant.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    /// The child did not exit within `timeout_secs` seconds. The child has
    /// already been killed (SIGKILL on Unix, `TerminateProcess` on Windows) by
    /// the time this error is returned. The caller should treat this the same
    /// as the `timeout(1)` utility: exit code 124 by convention.
    #[error("timeout: child did not exit within {0}s")]
    Timeout(u64),
}

/// Shut down the reader thread cleanly.
///
/// Waits up to 2 seconds for the reader thread to finish draining remaining
/// PTY output, then drops the PTY master. Dropping master before the reader
/// has finished would close the fd and cause the background read() to see an
/// error mid-drain, truncating any output buffered after the child exits. By
/// waiting first we let the reader observe the natural EOF from the child
/// closing the slave side.
///
/// If the thread is still alive after the deadline it is abandoned — this
/// handles the known ConPTY behaviour on Windows where the pipe handle may not
/// deliver EOF even after the child exits.
fn join_reader(
    handle: JoinHandle<()>,
    stop: &Arc<Mutex<bool>>,
    master: Box<dyn portable_pty::MasterPty + Send>,
) {
    // Give the reader thread a chance to drain remaining output before we
    // close the master fd. The thread exits naturally when it sees EOF (Ok(0))
    // from the slave closing after the child exits.
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if handle.is_finished() {
            let _ = handle.join();
            // Reader finished on its own: now safe to drop master.
            drop(master);
            return;
        }
        if Instant::now() > deadline {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    // Deadline elapsed without the reader finishing. Signal early exit and
    // drop master to unblock any stuck read() (handles ConPTY on Windows).
    if let Ok(mut g) = stop.lock() {
        *g = true;
    }
    drop(master);
    // Thread abandoned — do not join to avoid blocking indefinitely.
}

/// Write `/exit\n` to the PTY master to ask an interactive child (e.g. Claude
/// Code) to exit cleanly, then delete the signal file.
///
/// Best-effort: errors are ignored so the caller continues polling for child
/// exit regardless.
fn send_exit_command(master: &dyn MasterPty, signal_path: &Path) {
    if let Ok(mut writer) = master.take_writer() {
        let _ = writer.write_all(b"/exit\n");
        let _ = writer.flush();
    }
    let _ = std::fs::remove_file(signal_path);
}

/// Allocate a PTY, spawn `argv` inside it, stream stdout to the current
/// process's stdout, and poll until the child exits or `timeout_secs` elapses.
///
/// `timeout_secs == 0` means no timeout.
///
/// `exit_signal` — optional path to a signal file. When the file appears
/// during the poll loop (written by `heartbeat-stop` when it decides Approve),
/// `/exit\n` is written to the PTY master and the file is deleted. The poll
/// loop then continues waiting for the child to exit normally.
pub fn run(
    argv: &[String],
    cwd: &Path,
    timeout_secs: u64,
    exit_signal: Option<&Path>,
) -> Result<RunResult, PtyError> {
    let pty_system = native_pty_system();

    let pair = pty_system
        .openpty(PtySize {
            rows: 50,
            // Wide enough to suppress Claude Code's line-wrap reformatting,
            // which kicks in at narrower column counts and inserts spurious
            // newlines/indentation into the PTY output stream.
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

    let mut child = pair.slave.spawn_command(cmd).map_err(PtyError::Spawn)?;

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
            if stop_reader.lock().is_ok_and(|g| *g) {
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

    // Delete any stale signal file left over from a previous crash before
    // entering the poll loop. Without this, a file orphaned by a prior
    // abnormal exit would trigger an immediate /exit on the very first poll
    // tick, poisoning the new session before the child has done any work.
    if let Some(sig) = exit_signal {
        let _ = std::fs::remove_file(sig);
    }

    // Poll loop: check child exit, enforce timeout.
    // timeout_secs == 0 means no timeout: deadline is None and the timeout
    // branch inside the loop is never entered.
    let poll_interval = Duration::from_millis(100);
    let deadline = if timeout_secs > 0 {
        Some(Instant::now() + Duration::from_secs(timeout_secs))
    } else {
        None
    };

    // Track whether we have already sent the exit command so we only do it once.
    let mut exit_sent = false;

    let exit_code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.exit_code(),
            Ok(None) => {
                // Still running.
                if let Some(dl) = deadline {
                    if Instant::now() >= dl {
                        // Timed out. Kill the entire process group so that
                        // grandchild processes (forks of the command) are also
                        // reaped. On Unix we use killpg(pgid, SIGKILL) which
                        // delivers SIGKILL to every member of the process
                        // group. On other platforms we fall back to
                        // portable-pty's kill() which only reaches the direct
                        // child process.
                        #[cfg(unix)]
                        {
                            // process_group_leader() reads the foreground pgid
                            // from the PTY master via tcgetpgrp(). If the child
                            // set up its own process group (typical for shells)
                            // this covers all descendants in that group.
                            if let Some(pgid) = pair.master.process_group_leader() {
                                // SAFETY: pgid is a valid process group id
                                // returned by the OS. A negative pgid to kill(2)
                                // means "process group"; killpg(pgid, sig) is
                                // equivalent to kill(-pgid, sig).
                                unsafe {
                                    libc::killpg(pgid, libc::SIGKILL);
                                }
                            }
                        }
                        let _ = killer.kill(); // belt-and-suspenders / Windows fallback
                        thread::sleep(Duration::from_millis(500));
                        // Clean up reader thread and return Err.
                        join_reader(reader_thread, &stop, pair.master);
                        return Err(PtyError::Timeout(timeout_secs));
                    }
                }

                // Check exit signal file. When heartbeat-stop decides Approve
                // it touches this file; we write /exit\n to the PTY master and
                // delete the file so the child receives the exit command once.
                if !exit_sent {
                    if let Some(sig) = exit_signal {
                        if sig.exists() {
                            send_exit_command(pair.master.as_ref(), sig);
                            exit_sent = true;
                        }
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
    #[cfg(unix)]
    #[test]
    fn echo_hello_exit_zero() {
        // Redirect PTY output to a temp file so we can inspect it.
        // We can't easily capture stdout from within the same process in a
        // unit test, so we verify exit code here and test output capture in
        // the integration test.
        let result = run(&["echo".to_string(), "hello".to_string()], &tmp(), 10, None)
            .expect("run should succeed");
        assert_eq!(result.exit_code, 0, "echo should exit 0");
    }

    /// A command that exits non-zero propagates the exit code.
    #[cfg(unix)]
    #[test]
    fn nonzero_exit_code_propagated() {
        // `false` always exits 1.
        let result = run(&["false".to_string()], &tmp(), 10, None).expect("run should succeed");
        assert_ne!(result.exit_code, 0, "false should exit non-zero");
    }

    /// Timeout fires and returns PtyError::Timeout.
    #[cfg(unix)]
    #[test]
    fn timeout_fires() {
        // Sleep for 60s but give it only 1s timeout.
        let err = run(&["sleep".to_string(), "60".to_string()], &tmp(), 1, None)
            .expect_err("should time out");
        match err {
            PtyError::Timeout(secs) => assert_eq!(secs, 1),
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    /// Signal file triggers /exit: create the signal file while a long-running
    /// command is inside the PTY and verify the child exits within a reasonable
    /// deadline.
    ///
    /// We spawn `sh -c 'read line'` which blocks waiting for stdin input.
    /// A background thread creates the signal file after a short delay.
    /// heartbeat-launch's poll loop detects the file, writes `/exit\n` to the
    /// PTY master, and the shell receives it on stdin — causing it to exit.
    #[cfg(unix)]
    #[test]
    fn exit_signal_triggers_child_exit() {
        use std::fs;

        let signal_path = tmp().join("test-exit-signal-trigger.tmp");
        // Ensure clean state.
        let _ = fs::remove_file(&signal_path);

        let signal_path_clone = signal_path.clone();
        let writer_thread = thread::spawn(move || {
            // Give the PTY poll loop time to start before creating the file.
            thread::sleep(Duration::from_millis(300));
            fs::write(&signal_path_clone, b"").expect("write signal file");
        });

        // `read line` blocks on stdin until it receives a line.
        // When /exit\n is written to the PTY master the shell reads it,
        // processes it as the value for `line`, and returns 0.
        let result = run(
            &["sh".to_string(), "-c".to_string(), "read line".to_string()],
            &tmp(),
            10, // generous timeout so the test doesn't hang on slow CI
            Some(&signal_path),
        )
        .expect("run should succeed");

        writer_thread.join().expect("writer thread panicked");

        assert_eq!(result.exit_code, 0, "child should exit 0 after signal");
        // Signal file should have been deleted by send_exit_command.
        assert!(
            !signal_path.exists(),
            "signal file should be deleted after /exit is sent"
        );
    }

    /// Stale signal file at startup: a pre-existing signal file is deleted
    /// before the poll loop begins, preventing orphan-file poisoning where a
    /// crash on a previous run leaves the file behind and the next invocation
    /// immediately exits.
    #[cfg(unix)]
    #[test]
    fn stale_signal_file_deleted_before_poll() {
        use std::fs;

        let signal_path = tmp().join("test-exit-signal-stale.tmp");
        // Create the stale file BEFORE calling run().
        fs::write(&signal_path, b"").expect("write stale signal file");
        assert!(
            signal_path.exists(),
            "precondition: stale file should exist"
        );

        // `echo hello` exits immediately. If the stale signal file caused an
        // immediate /exit, the child would still exit 0 — but the important
        // thing is that the file was deleted during startup, not during the
        // first poll tick, so we also check that it's gone after run().
        let result = run(
            &["echo".to_string(), "hello".to_string()],
            &tmp(),
            10,
            Some(&signal_path),
        )
        .expect("run should succeed");

        assert_eq!(result.exit_code, 0);
        assert!(
            !signal_path.exists(),
            "stale signal file should be deleted by run() before poll loop"
        );
    }
}

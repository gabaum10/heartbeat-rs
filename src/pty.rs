//! PTY allocation and child spawning for heartbeat-launch.
//!
//! Provides a thin wrapper around `portable-pty` that:
//! - Allocates a PTY with configurable dimensions
//! - Spawns an arbitrary command inside it
//! - Streams child stdout to the caller via a background thread
//! - Polls for child exit with a configurable timeout
//! - Detects output idle periods and injects keepalive input to unstick stalled sessions
//!
//! No inbox, no settings.json, no handshake. The consumer handles all of that.

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[cfg(unix)]
use libc;

/// Configuration for idle detection and keepalive injection.
///
/// When the PTY produces no output for `timeout_secs` seconds, a keepalive
/// sequence is injected: ESC (to cancel any stalled generation) followed by
/// the `prompt` text and a newline. Retried up to `max_retries` times before
/// giving up and killing the child.
///
/// `timeout_secs == 0` disables idle detection entirely.
#[derive(Debug, Clone)]
pub struct IdleConfig {
    /// Seconds of output silence before triggering a keepalive. 0 = disabled.
    pub timeout_secs: u64,
    /// Text to inject after ESC when idle is detected.
    pub prompt: String,
    /// Maximum number of keepalive injections before killing the child.
    pub max_retries: u32,
}

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

    /// The child produced no output for the idle timeout period and did not
    /// recover after the maximum number of keepalive injections. The inner
    /// value is the idle timeout in seconds (not the session timeout).
    /// The child has already been killed by the time this is returned.
    #[error("idle exhausted: no output for {0}s after maximum keepalive retries")]
    IdleExhausted(u64),
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

// ---------------------------------------------------------------------------
// Shared PTY spawn helper
// ---------------------------------------------------------------------------

/// Output of the shared PTY spawn step.
struct PtySpawn {
    /// PTY master — used for `take_writer()`, `process_group_leader()`, and
    /// passed to `join_reader()` at teardown.
    master: Box<dyn portable_pty::MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send>,
    killer: Box<dyn portable_pty::ChildKiller + Send>,
    reader: Box<dyn Read + Send>,
}

/// Allocate a PTY, build the command, spawn it, and return the raw pieces.
///
/// Drops the slave side immediately so the master sees EOF when the child exits.
/// The reader is a clone of the master's read handle; it does NOT consume the
/// master — the caller still holds `master` for `take_writer()` and
/// `process_group_leader()`.
fn spawn_pty_child(argv: &[String], cwd: &Path) -> Result<PtySpawn, PtyError> {
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

    let child = pair.slave.spawn_command(cmd).map_err(PtyError::Spawn)?;

    // Drop slave so the master sees EOF when the child exits.
    drop(pair.slave);

    let killer = child.clone_killer();
    let reader = pair.master.try_clone_reader().map_err(PtyError::Reader)?;
    let master = pair.master;

    Ok(PtySpawn {
        master,
        child,
        killer,
        reader,
    })
}

// ---------------------------------------------------------------------------
// Shared reader thread helpers
// ---------------------------------------------------------------------------

/// Spawn a basic reader thread: forwards PTY output to stdout and stamps
/// `last_output` on every successful read.
///
/// Returns the join handle plus the shared stop flag and last-output timestamp
/// already cloned for the caller's use in the poll loop.
fn spawn_basic_reader(
    mut reader: Box<dyn Read + Send>,
    stop_reader: Arc<Mutex<bool>>,
    last_output_reader: Arc<Mutex<Instant>>,
) -> JoinHandle<()> {
    thread::spawn(move || {
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
                    // Stamp activity timestamp before forwarding output.
                    if let Ok(mut ts) = last_output_reader.lock() {
                        *ts = Instant::now();
                    }
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
    })
}

/// Spawn a sentinel-aware reader thread: forwards PTY output to stdout, stamps
/// `last_output`, marks `seen_output`, and appends ANSI-stripped bytes to
/// `sentinel_buf` for the queue controller to scan.
#[cfg(feature = "launch")]
fn spawn_sentinel_reader(
    mut reader: Box<dyn Read + Send>,
    stop_reader: Arc<Mutex<bool>>,
    last_output_reader: Arc<Mutex<Instant>>,
    seen_output_reader: Arc<Mutex<bool>>,
    sentinel_buf_reader: Arc<Mutex<Vec<u8>>>,
    sentinel_len: usize,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let stdout = io::stdout();
        let mut buf = [0u8; 4096];
        loop {
            if stop_reader.lock().is_ok_and(|g| *g) {
                break;
            }
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    // Update activity timestamp.
                    if let Ok(mut ts) = last_output_reader.lock() {
                        *ts = Instant::now();
                    }
                    // Mark that we've seen at least one byte of output.
                    if let Ok(mut seen) = seen_output_reader.lock() {
                        *seen = true;
                    }

                    let raw_chunk = &buf[..n];

                    // Forward raw bytes to stdout (user sees unmodified output).
                    let mut out = stdout.lock();
                    if out.write_all(raw_chunk).is_err() {
                        break;
                    }
                    let _ = out.flush();

                    // Append ANSI-stripped bytes to the sentinel buffer.
                    // strip() returns a new Vec; we append its contents.
                    let clean = strip_ansi_escapes::strip(raw_chunk);
                    if let Ok(mut sbuf) = sentinel_buf_reader.lock() {
                        sbuf.extend_from_slice(&clean);
                        // Keep the most recent 8 KB plus one extra sentinel_len
                        // so a match can never be split across a drain boundary.
                        let keep = 8192 + sentinel_len;
                        if sbuf.len() > keep * 2 {
                            let drain = sbuf.len() - keep;
                            sbuf.drain(..drain);
                        }
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Shared idle detection helper
// ---------------------------------------------------------------------------

/// Mutable state for the idle detection logic.
struct IdleState {
    timeout: u64,
    prompt: String,
    max_retries: u32,
    retry_count: u32,
    last_keepalive: Option<Instant>,
}

const KEEPALIVE_GRACE_SECS: u64 = 5;

impl IdleState {
    fn from_config(idle: Option<&IdleConfig>) -> Self {
        IdleState {
            timeout: idle.map(|c| c.timeout_secs).unwrap_or(0),
            prompt: idle
                .map(|c| c.prompt.clone())
                .unwrap_or_else(|| "Continue".to_string()),
            max_retries: idle.map(|c| c.max_retries).unwrap_or(3),
            retry_count: 0,
            last_keepalive: None,
        }
    }
}

/// Result of a single idle-detection tick.
enum IdleTick {
    /// Child should be killed; return `PtyError::IdleExhausted`.
    Exhausted,
    /// Idle detected; keepalive injected (or attempted).
    KeepaliveInjected,
    /// Output resumed after stall; retry counter reset.
    Recovered,
    /// Nothing notable.
    Ok,
}

/// Run one idle-detection tick.
///
/// `skip` — when true the check is a no-op (used during WaitingForBoot in
/// queue mode to suppress spurious keepalives during the startup silence window).
fn tick_idle(
    state: &mut IdleState,
    last_output: &Arc<Mutex<Instant>>,
    pty_writer: &mut Option<Box<dyn Write + Send>>,
    skip: bool,
) -> IdleTick {
    if state.timeout == 0 || skip {
        return IdleTick::Ok;
    }

    let silent_secs = last_output
        .lock()
        .map(|ts| ts.elapsed())
        .unwrap_or(Duration::ZERO);

    if silent_secs >= Duration::from_secs(state.timeout) {
        if state.retry_count >= state.max_retries {
            return IdleTick::Exhausted;
        }

        state.retry_count += 1;
        eprintln!(
            "heartbeat-launch: idle detected ({:.0}s silent) — injecting keepalive (attempt {}/{})",
            silent_secs.as_secs_f64(),
            state.retry_count,
            state.max_retries,
        );

        if let Some(ref mut w) = pty_writer {
            // Send ESC to cancel any stalled generation.
            let _ = w.write_all(b"\x1b");
            let _ = w.flush();
            // Brief pause to let the model process the cancel.
            thread::sleep(Duration::from_millis(500));
            // Inject the keepalive prompt.
            let _ = w.write_all(state.prompt.as_bytes());
            let _ = w.write_all(b"\n");
            let _ = w.flush();
        }

        // Record when we last injected so the grace period below
        // can suppress the echo-triggered counter reset.
        state.last_keepalive = Some(Instant::now());

        // Reset the activity timestamp so the idle timer restarts
        // from now rather than immediately firing again.
        if let Ok(mut ts) = last_output.lock() {
            *ts = Instant::now();
        }

        IdleTick::KeepaliveInjected
    } else if state.retry_count > 0 {
        // Output is flowing again after a previous stall — but only credit it
        // as genuine recovery if we are outside the grace window after the last
        // keepalive injection. Within the grace window the output is just the
        // PTY echoing the injected bytes back, not real recovery.
        let past_grace = state
            .last_keepalive
            .map(|t| t.elapsed() >= Duration::from_secs(KEEPALIVE_GRACE_SECS))
            .unwrap_or(true);
        if past_grace {
            eprintln!(
                "heartbeat-launch: output resumed — resetting idle retry counter (was {})",
                state.retry_count
            );
            state.retry_count = 0;
            state.last_keepalive = None;
            IdleTick::Recovered
        } else {
            IdleTick::Ok
        }
    } else {
        IdleTick::Ok
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Allocate a PTY, spawn `argv` inside it, stream stdout to the current
/// process's stdout, and poll until the child exits or `timeout_secs` elapses.
///
/// `timeout_secs == 0` means no timeout.
///
/// `exit_signal` — optional path to a signal file. When the file appears
/// during the poll loop (written by `heartbeat-stop` when it decides Approve),
/// `/exit\n` is written to the PTY master and the file is deleted. The poll
/// loop then continues waiting for the child to exit normally.
///
/// `idle` — optional idle detection config. When `idle.timeout_secs > 0` and
/// the PTY produces no output for that many seconds, ESC followed by
/// `idle.prompt` and a newline is injected to unstick a stalled session. After
/// `idle.max_retries` injections without recovery, the child is killed.
pub fn run(
    argv: &[String],
    cwd: &Path,
    timeout_secs: u64,
    exit_signal: Option<&Path>,
    idle: Option<&IdleConfig>,
) -> Result<RunResult, PtyError> {
    let PtySpawn {
        master,
        mut child,
        mut killer,
        reader,
    } = spawn_pty_child(argv, cwd)?;

    // Shared flag: main thread signals reader thread to stop on timeout.
    let stop = Arc::new(Mutex::new(false));

    // Shared timestamp: reader thread updates this on every successful read.
    // The poll loop reads it to detect idle periods.
    let last_output: Arc<Mutex<Instant>> = Arc::new(Mutex::new(Instant::now()));

    let reader_thread = spawn_basic_reader(reader, Arc::clone(&stop), Arc::clone(&last_output));

    // Delete any stale signal file left over from a previous crash before
    // entering the poll loop. Without this, a file orphaned by a prior
    // abnormal exit would trigger an immediate /exit on the very first poll
    // tick, poisoning the new session before the child has done any work.
    if let Some(sig) = exit_signal {
        let _ = std::fs::remove_file(sig);
    }

    // Acquire the PTY master writer once here. We hold it for the full poll
    // loop lifetime so it can be used for both keepalive injection and the
    // exit command. take_writer() errors on a second call, so this must be
    // the only call site.
    //
    // If take_writer() fails (unlikely on Unix), keepalive and exit injection
    // degrade gracefully to no-ops; the poll loop continues unaffected.
    let mut pty_writer: Option<Box<dyn Write + Send>> = master.take_writer().ok();

    // Poll loop: check child exit, enforce timeout.
    // timeout_secs == 0 means no timeout: deadline is None and the timeout
    // branch inside the loop is never entered.
    let poll_interval = Duration::from_millis(100);
    let deadline = if timeout_secs > 0 {
        Some(Instant::now() + Duration::from_secs(timeout_secs))
    } else {
        None
    };

    // Idle detection state. Only active when idle config is provided and
    // idle.timeout_secs > 0.
    let mut idle_state = IdleState::from_config(idle);

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
                            if let Some(pgid) = master.process_group_leader() {
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
                        join_reader(reader_thread, &stop, master);
                        return Err(PtyError::Timeout(timeout_secs));
                    }
                }

                // Check exit signal file. When heartbeat-stop decides Approve
                // it touches this file; we SIGTERM the process group (with a
                // 2-second grace period) then SIGKILL if it hasn't exited.
                if !exit_sent {
                    if let Some(sig) = exit_signal {
                        if sig.exists() {
                            let _ = std::fs::remove_file(sig);
                            eprintln!("heartbeat-launch: exit signal detected, terminating child");

                            #[cfg(unix)]
                            {
                                if let Some(pgid) = master.process_group_leader() {
                                    // SAFETY: pgid is a valid process group id
                                    // returned by the OS.
                                    unsafe {
                                        libc::killpg(pgid, libc::SIGTERM);
                                    }
                                }
                            }

                            // Grace period: wait up to 2s for clean shutdown.
                            let term_deadline = Instant::now() + Duration::from_secs(2);
                            while Instant::now() < term_deadline {
                                if let Ok(Some(_)) = child.try_wait() {
                                    break;
                                }
                                thread::sleep(Duration::from_millis(50));
                            }

                            // Force kill if still alive after grace period.
                            if child.try_wait().map(|s| s.is_none()).unwrap_or(true) {
                                eprintln!(
                                    "heartbeat-launch: child did not exit on SIGTERM, sending SIGKILL"
                                );
                                #[cfg(unix)]
                                {
                                    if let Some(pgid) = master.process_group_leader() {
                                        unsafe {
                                            libc::killpg(pgid, libc::SIGKILL);
                                        }
                                    }
                                }
                                let _ = killer.kill();
                            }

                            exit_sent = true;
                        }
                    }
                }

                // Idle detection: if output has been silent for longer than
                // idle_timeout, inject ESC + keepalive prompt to unstick the
                // stalled generation. After max_idle_retries injections without
                // recovery, give up and kill the child.
                if let IdleTick::Exhausted =
                    tick_idle(&mut idle_state, &last_output, &mut pty_writer, false)
                {
                    eprintln!(
                        "heartbeat-launch: idle timeout fired {} time(s) without recovery — killing child",
                        idle_state.retry_count
                    );
                    #[cfg(unix)]
                    {
                        if let Some(pgid) = master.process_group_leader() {
                            unsafe {
                                libc::killpg(pgid, libc::SIGKILL);
                            }
                        }
                    }
                    let _ = killer.kill();
                    thread::sleep(Duration::from_millis(500));
                    join_reader(reader_thread, &stop, master);
                    return Err(PtyError::IdleExhausted(idle_state.timeout));
                }

                thread::sleep(poll_interval);
            }
            Err(e) => return Err(PtyError::Io(e)),
        }
    };

    // Normal exit: shut down reader thread.
    join_reader(reader_thread, &stop, master);

    // If the child was killed via exit-signal, treat as clean exit (0)
    // regardless of the signal-death code the OS reports.
    let exit_code = if exit_sent { 0 } else { exit_code };

    Ok(RunResult { exit_code })
}

// ---------------------------------------------------------------------------
// Queue mode
// ---------------------------------------------------------------------------

/// Configuration for queue-based input injection.
///
/// When queue mode is active, `run_with_queue` loads entries from a JSONL file
/// and injects them one at a time into the PTY, waiting for a sentinel string
/// between each injection.
#[derive(Debug, Clone)]
pub struct QueueConfig {
    /// Path to the JSONL queue file. Each line is one entry.
    pub queue_path: std::path::PathBuf,
    /// Sentinel pattern to detect in PTY output between entries.
    pub sentinel: String,
    /// Seconds of output silence after first output burst before injecting
    /// the first entry. Gives Claude time to load context files.
    pub boot_delay_secs: u64,
    /// Format string for each injected entry.
    ///
    /// Placeholders: `{index}` (1-based), `{total}`, `{content}` (the raw
    /// queue line).  Used verbatim with a trailing newline appended.
    pub entry_template: String,
    /// Message sent to the PTY after all entries are consumed, before the
    /// controller transitions to Done.
    pub done_message: String,
}

/// Queue controller states.
#[derive(Debug, Clone, PartialEq)]
enum QueueState {
    /// Waiting for the initial boot silence period to expire.
    WaitingForBoot,
    /// Entry injected; waiting for sentinel in PTY output.
    WaitForSentinel,
    /// Sentinel found (or boot ready); inject the next entry.
    InjectNext,
    /// All entries consumed; send the final exit prompt.
    SendExit,
    /// Done — poll loop should terminate.
    Done,
}

/// Allocate a PTY, spawn `argv` inside it, stream stdout to the current
/// process's stdout, and drive a queue of entries through the session.
///
/// After the child produces `boot_delay_secs` seconds of output silence (post
/// first output), the first queue entry is injected. After each sentinel is
/// detected in the PTY output, the next entry is injected.  When all entries
/// are consumed the `done_message` from `QueueConfig` is sent.
///
/// All existing safety mechanisms (idle detection, keepalive, timeout, exit
/// signal) apply unchanged inside queue mode.  Idle detection is suppressed
/// during the WaitingForBoot phase to avoid spurious keepalives during the
/// startup silence window.
#[cfg(feature = "launch")]
pub fn run_with_queue(
    argv: &[String],
    cwd: &Path,
    timeout_secs: u64,
    exit_signal: Option<&Path>,
    idle: Option<&IdleConfig>,
    queue: &QueueConfig,
) -> Result<RunResult, PtyError> {
    use std::fs;

    // Load and validate the queue file up front so we fail fast.
    let queue_raw = fs::read_to_string(&queue.queue_path).map_err(|e| {
        PtyError::Io(io::Error::new(
            io::ErrorKind::NotFound,
            format!("queue file {}: {e}", queue.queue_path.display()),
        ))
    })?;
    let entries: Vec<String> = queue_raw
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect();
    let total = entries.len();
    eprintln!(
        "heartbeat-launch: queue mode — {total} entries from {}",
        queue.queue_path.display()
    );

    let PtySpawn {
        master,
        mut child,
        mut killer,
        reader,
    } = spawn_pty_child(argv, cwd)?;

    // Shared stop flag.
    let stop = Arc::new(Mutex::new(false));

    // Shared last-output timestamp (reused for boot detection).
    let last_output: Arc<Mutex<Instant>> = Arc::new(Mutex::new(Instant::now()));

    // Shared sentinel buffer: reader appends ANSI-stripped output; poll loop
    // scans for the sentinel string.  Capped at 16 KB; older bytes are dropped.
    let sentinel_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::with_capacity(8192)));

    // Track whether we have seen any output at all (used for boot detection).
    let seen_output: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));

    let reader_thread = spawn_sentinel_reader(
        reader,
        Arc::clone(&stop),
        Arc::clone(&last_output),
        Arc::clone(&seen_output),
        Arc::clone(&sentinel_buf),
        queue.sentinel.len(),
    );

    // Delete stale signal file.
    if let Some(sig) = exit_signal {
        let _ = fs::remove_file(sig);
    }

    let mut pty_writer: Option<Box<dyn Write + Send>> = master.take_writer().ok();

    let poll_interval = Duration::from_millis(100);
    let deadline = if timeout_secs > 0 {
        Some(Instant::now() + Duration::from_secs(timeout_secs))
    } else {
        None
    };

    // Idle detection state. Only active when idle config is provided and
    // idle.timeout_secs > 0.  Suppressed during WaitingForBoot.
    let mut idle_state = IdleState::from_config(idle);
    let mut exit_sent = false;

    // -----------------------------------------------------------------
    // Queue state machine
    // -----------------------------------------------------------------
    let mut queue_state = QueueState::WaitingForBoot;
    let mut queue_index: usize = 0;
    // Timestamp of last sentinel match (used for 500ms post-sentinel delay).
    let mut sentinel_matched_at: Option<Instant> = None;
    let boot_delay = Duration::from_secs(queue.boot_delay_secs);

    let exit_code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.exit_code(),
            Ok(None) => {
                // --- Timeout ---
                if let Some(dl) = deadline {
                    if Instant::now() >= dl {
                        #[cfg(unix)]
                        {
                            if let Some(pgid) = master.process_group_leader() {
                                unsafe {
                                    libc::killpg(pgid, libc::SIGKILL);
                                }
                            }
                        }
                        let _ = killer.kill();
                        thread::sleep(Duration::from_millis(500));
                        join_reader(reader_thread, &stop, master);
                        return Err(PtyError::Timeout(timeout_secs));
                    }
                }

                // --- Exit signal ---
                if !exit_sent {
                    if let Some(sig) = exit_signal {
                        if sig.exists() {
                            let _ = std::fs::remove_file(sig);
                            eprintln!("heartbeat-launch: exit signal detected, terminating child");

                            #[cfg(unix)]
                            {
                                if let Some(pgid) = master.process_group_leader() {
                                    // SAFETY: pgid is a valid process group id
                                    // returned by the OS.
                                    unsafe {
                                        libc::killpg(pgid, libc::SIGTERM);
                                    }
                                }
                            }

                            // Grace period: wait up to 2s for clean shutdown.
                            let term_deadline = Instant::now() + Duration::from_secs(2);
                            while Instant::now() < term_deadline {
                                if let Ok(Some(_)) = child.try_wait() {
                                    break;
                                }
                                thread::sleep(Duration::from_millis(50));
                            }

                            // Force kill if still alive after grace period.
                            if child.try_wait().map(|s| s.is_none()).unwrap_or(true) {
                                eprintln!(
                                    "heartbeat-launch: child did not exit on SIGTERM, sending SIGKILL"
                                );
                                #[cfg(unix)]
                                {
                                    if let Some(pgid) = master.process_group_leader() {
                                        unsafe {
                                            libc::killpg(pgid, libc::SIGKILL);
                                        }
                                    }
                                }
                                let _ = killer.kill();
                            }

                            exit_sent = true;
                        }
                    }
                }

                // --- Idle detection ---
                // Suppressed during WaitingForBoot: the child hasn't received
                // any work yet and we don't want spurious keepalive injections
                // during the startup silence window.
                let skip_idle = queue_state == QueueState::WaitingForBoot;
                if let IdleTick::Exhausted =
                    tick_idle(&mut idle_state, &last_output, &mut pty_writer, skip_idle)
                {
                    eprintln!(
                        "heartbeat-launch: idle timeout fired {} time(s) without recovery — killing child",
                        idle_state.retry_count
                    );
                    #[cfg(unix)]
                    {
                        if let Some(pgid) = master.process_group_leader() {
                            unsafe {
                                libc::killpg(pgid, libc::SIGKILL);
                            }
                        }
                    }
                    let _ = killer.kill();
                    thread::sleep(Duration::from_millis(500));
                    join_reader(reader_thread, &stop, master);
                    return Err(PtyError::IdleExhausted(idle_state.timeout));
                }

                // --- Queue state machine ---
                match queue_state {
                    QueueState::WaitingForBoot => {
                        // Wait until we've seen some output AND been silent for
                        // boot_delay_secs.
                        let has_output = seen_output.lock().map(|g| *g).unwrap_or(false);
                        if has_output {
                            let silent = last_output
                                .lock()
                                .map(|ts| ts.elapsed())
                                .unwrap_or(Duration::ZERO);
                            if silent >= boot_delay {
                                eprintln!(
                                    "heartbeat-launch: boot complete ({:.1}s silence) — ready to inject",
                                    silent.as_secs_f64()
                                );
                                queue_state = QueueState::InjectNext;
                                // Fall through immediately to InjectNext this tick.
                            }
                        }
                        // If we haven't fallen through, just sleep and loop.
                        if queue_state == QueueState::WaitingForBoot {
                            thread::sleep(poll_interval);
                            continue;
                        }
                    }
                    QueueState::WaitForSentinel => {
                        // Scan sentinel buffer for the sentinel string.
                        let found = {
                            let buf = sentinel_buf.lock().unwrap_or_else(|e| e.into_inner());
                            let text = String::from_utf8_lossy(&buf);
                            text.contains(queue.sentinel.as_str())
                        };

                        if found {
                            // Clear buffer.
                            if let Ok(mut buf) = sentinel_buf.lock() {
                                buf.clear();
                            }
                            eprintln!(
                                "heartbeat-launch: sentinel {:?} detected after entry {}",
                                queue.sentinel, queue_index,
                            );
                            // Record the match time; we'll wait 500ms before injecting.
                            sentinel_matched_at = Some(Instant::now());
                            queue_index += 1;
                            queue_state = QueueState::InjectNext;
                            // Fall through to InjectNext this tick (no continue).
                        } else {
                            thread::sleep(poll_interval);
                            continue;
                        }
                    }
                    QueueState::InjectNext => {}
                    QueueState::SendExit | QueueState::Done => {}
                }
                // Second match for states that fall through or are already here.
                match queue_state {
                    QueueState::InjectNext => {
                        // Honour the 500ms post-sentinel delay when one applies.
                        if let Some(matched_at) = sentinel_matched_at {
                            let elapsed = matched_at.elapsed();
                            let delay = Duration::from_millis(500);
                            if elapsed < delay {
                                thread::sleep(delay - elapsed);
                            }
                            sentinel_matched_at = None;
                        }

                        if queue_index < total {
                            let entry = &entries[queue_index];
                            let msg = queue
                                .entry_template
                                .replace("{index}", &(queue_index + 1).to_string())
                                .replace("{total}", &total.to_string())
                                .replace("{content}", entry);
                            eprintln!(
                                "heartbeat-launch: injecting entry {}/{total}",
                                queue_index + 1,
                            );
                            if let Some(ref mut w) = pty_writer {
                                let _ = w.write_all(msg.as_bytes());
                                let _ = w.write_all(b"\n");
                                let _ = w.flush();
                            }
                            queue_state = QueueState::WaitForSentinel;
                        } else {
                            // All entries consumed.
                            queue_state = QueueState::SendExit;
                        }
                    }
                    QueueState::SendExit => {
                        eprintln!(
                            "heartbeat-launch: all {total} entries consumed — sending exit prompt"
                        );
                        if let Some(ref mut w) = pty_writer {
                            let _ = w.write_all(queue.done_message.as_bytes());
                            let _ = w.write_all(b"\n");
                            let _ = w.flush();
                        }
                        queue_state = QueueState::Done;
                    }
                    _ => {}
                }

                thread::sleep(poll_interval);
            }
            Err(e) => return Err(PtyError::Io(e)),
        }
    };

    join_reader(reader_thread, &stop, master);

    // If the child was killed via exit-signal, treat as clean exit (0)
    // regardless of the signal-death code the OS reports.
    let exit_code = if exit_sent { 0 } else { exit_code };

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
        let result = run(
            &["echo".to_string(), "hello".to_string()],
            &tmp(),
            10,
            None,
            None,
        )
        .expect("run should succeed");
        assert_eq!(result.exit_code, 0, "echo should exit 0");
    }

    /// A command that exits non-zero propagates the exit code.
    #[cfg(unix)]
    #[test]
    fn nonzero_exit_code_propagated() {
        // `false` always exits 1.
        let result =
            run(&["false".to_string()], &tmp(), 10, None, None).expect("run should succeed");
        assert_ne!(result.exit_code, 0, "false should exit non-zero");
    }

    /// Timeout fires and returns PtyError::Timeout.
    #[cfg(unix)]
    #[test]
    fn timeout_fires() {
        // Sleep for 60s but give it only 1s timeout.
        let err = run(
            &["sleep".to_string(), "60".to_string()],
            &tmp(),
            1,
            None,
            None,
        )
        .expect_err("should time out");
        match err {
            PtyError::Timeout(secs) => assert_eq!(secs, 1),
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    /// Signal file triggers SIGTERM: create the signal file while a long-running
    /// command is inside the PTY and verify the child exits within a reasonable
    /// deadline.
    ///
    /// We spawn `sh -c 'read line'` which blocks waiting for stdin input.
    /// A background thread creates the signal file after a short delay.
    /// heartbeat-launch's poll loop detects the file, sends SIGTERM to the
    /// child process group, and the shell is killed — exit-signal shutdown returns
    /// exit code 0 (clean, intentional termination).
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

        // `read line` blocks on stdin until it receives input. The poll loop
        // detects the signal file and sends SIGTERM; exit-signal triggered exit
        // returns 0 — clean, intentional shutdown.
        let result = run(
            &["sh".to_string(), "-c".to_string(), "read line".to_string()],
            &tmp(),
            10, // generous timeout so the test doesn't hang on slow CI
            Some(&signal_path),
            None,
        )
        .expect("run should succeed");

        writer_thread.join().expect("writer thread panicked");

        assert_eq!(
            result.exit_code, 0,
            "signal-file triggered exit should return exit code 0 (clean shutdown)"
        );
        // Signal file should have been consumed when the exit was triggered.
        assert!(
            !signal_path.exists(),
            "signal file should be deleted after SIGTERM is sent"
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
            None,
        )
        .expect("run should succeed");

        assert_eq!(result.exit_code, 0);
        assert!(
            !signal_path.exists(),
            "stale signal file should be deleted by run() before poll loop"
        );
    }
}

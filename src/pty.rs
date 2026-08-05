//! PTY allocation and child spawning for heartbeat-launch.
//!
//! Provides a thin wrapper around `portable-pty` that:
//! - Allocates a PTY with configurable dimensions
//! - Spawns an arbitrary command inside it
//! - Streams child stdout to the caller via a background thread
//! - Polls for child exit with a configurable timeout
//! - Detects output idle periods and injects keepalive keystrokes (not prompts) to unstick stalled sessions
//! - Optionally tees raw child output to a per-run evidence log for post-mortem diagnosis of stalls
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

    // Forward the full parent environment into the child.
    //
    // portable-pty's CommandBuilder::new() already calls get_base_env()
    // internally, which captures std::env::vars_os() at construction time, so
    // the parent env is inherited without this call. The explicit loop here
    // re-inserts every parent var as an explicit (non-base-env) entry, making
    // the forwarding intent visible in the source and guarding against future
    // portable-pty behaviour changes that might alter or remove that implicit
    // capture (e.g. a version that defaults to env-clear for sandboxing).
    //
    // Practical effect: vars set on the heartbeat-launch invocation — such as
    // ANTHROPIC_BASE_URL for proxy experiments — propagate into the PTY child
    // without needing any special wiring in the caller.
    //
    // The denylist env_remove() calls below still strip session-identity vars
    // from whatever the final environment contains.
    for (key, val) in std::env::vars_os() {
        cmd.env(key, val);
    }

    // Strip parent session-identity env vars so a headless claude spawned here
    // never inherits the launching session's identity.  This plugs the bleed
    // vector surfaced in the CHILD_SESSION/CC-2.1.175 transcript suppression
    // incident where Vale and groove fired EXPOSED from a live session.
    //
    // DELIBERATE denylist posture (not env_clear/allowlist): the child
    // legitimately needs PATH, HOME, SOREN_HOME, and the per-agent vars that
    // hook.rs sets.  An allowlist would be brittle as that set grows.
    //
    // NOT stripped: CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS,
    // CLAUDE_STREAM_IDLE_TIMEOUT_MS, CLAUDE_EFFORT — these are
    // feature/runtime config, not session identity.
    //
    // SECURITY SURFACE: denylist-vs-allowlist posture is a security judgment;
    // Rift should ratify or override if the threat model changes.
    for key in &[
        "CLAUDE_CODE_SESSION_ID",
        "CLAUDE_CODE_CHILD_SESSION",
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_CODE_EXECPATH",
        "CLAUDECODE",
        "AI_AGENT",
    ] {
        cmd.env_remove(key);
    }

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

/// Cap on total bytes retained across the per-run PTY evidence log's two
/// files (see `RingLog`). This is evidence capture for post-mortem debugging
/// of stalls, not a feature — the cap keeps a runaway/looping child from
/// filling disk. `PTY_LOG_MAX_BYTES` (10 MiB) comfortably covers a stalled
/// session's worth of transcript at typical CLI output rates. Referenced from
/// the `--pty-log-dir` flag help (src/launch.rs) and this module's doc
/// comments — update prose at both sites if this changes materially.
const PTY_LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// A size-bounded PTY evidence log that rotates instead of truncating, so the
/// MOST RECENT output survives the cap rather than being dropped.
///
/// For a stalled or killed session, the bytes immediately before the
/// stall/kill are the diagnostic payload — a naive "stop writing once we hit
/// the cap" implementation keeps the *first* `PTY_LOG_MAX_BYTES` and silently
/// discards everything after, which is exactly backwards for this feature's
/// stated purpose (Lens, W536 round 2 review).
///
/// Two files on disk at any time: the active file at `path` (at most
/// `PTY_LOG_MAX_BYTES / 2` bytes) and the previous chunk at `path` with a
/// `.1` suffix appended to the existing extension (e.g.
/// `heartbeat-launch-pty-123-456.log.1`). Once the active file reaches half
/// the cap it is rotated to the `.1` path (replacing whatever was there
/// before) and a fresh active file is opened. Reading `<path>.1` followed by
/// `<path>` reconstructs up to `PTY_LOG_MAX_BYTES` of the most recent output
/// — the *head* of the oldest retained chunk is what eventually gets
/// dropped, not the tail.
struct RingLog {
    path: std::path::PathBuf,
    rotated_path: std::path::PathBuf,
    file: std::fs::File,
    bytes_written: u64,
    cap_half: u64,
}

impl RingLog {
    /// Open a per-run PTY evidence log inside `dir`, named
    /// `heartbeat-launch-pty-<unix_millis>-<pid>.log` — timestamped and
    /// PID-suffixed so concurrent/rapid-fire runs never collide. Creates
    /// `dir` if it doesn't exist.
    ///
    /// Returns `None` (rather than propagating an error) on any failure:
    /// this is best-effort evidence capture, not a feature the session's
    /// success should depend on. A warning is printed to stderr so the
    /// operator knows logging is off for this run.
    fn open(dir: &Path) -> Option<Self> {
        Self::open_with_cap_half(dir, PTY_LOG_MAX_BYTES / 2)
    }

    /// Same as `open`, but with an explicit rotation threshold instead of
    /// deriving it from `PTY_LOG_MAX_BYTES`. `open` is the production entry
    /// point; this exists so tests can exercise rotation with a tiny
    /// threshold instead of writing multi-megabyte fixtures.
    fn open_with_cap_half(dir: &Path, cap_half: u64) -> Option<Self> {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!(
                "heartbeat-launch: warning: could not create PTY log dir {}: {e}",
                dir.display()
            );
            return None;
        }
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let path = dir.join(format!(
            "heartbeat-launch-pty-{millis}-{}.log",
            std::process::id()
        ));
        let mut rotated_name = path.as_os_str().to_owned();
        rotated_name.push(".1");
        let rotated_path = std::path::PathBuf::from(rotated_name);

        match std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
        {
            Ok(file) => {
                eprintln!(
                    "heartbeat-launch: PTY output logging to {} (rotates to {} at half-cap; \
                     together they hold the most recent {PTY_LOG_MAX_BYTES} bytes)",
                    path.display(),
                    rotated_path.display(),
                );
                Some(RingLog {
                    path,
                    rotated_path,
                    file,
                    bytes_written: 0,
                    cap_half,
                })
            }
            Err(e) => {
                eprintln!(
                    "heartbeat-launch: warning: could not open PTY log {}: {e}",
                    path.display()
                );
                None
            }
        }
    }

    /// Write `data`, rotating once the active file passes half the cap.
    /// Returns `Err` on unrecoverable write failure (e.g. disk full) so the
    /// caller can stop trying for the rest of the run.
    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        self.file.write_all(data)?;
        self.bytes_written += data.len() as u64;
        if self.bytes_written >= self.cap_half {
            self.rotate();
        }
        Ok(())
    }

    /// Move the filled active file to the `.1` path and start a fresh one.
    ///
    /// Best-effort: if the rename or reopen fails (e.g. a platform that
    /// disallows renaming an open file), keep writing into the current file
    /// past `cap_half` rather than losing data outright — this degrades to
    /// "one oversized recent chunk" instead of losing the tail, which is
    /// still strictly better than the head-only cap this replaces.
    fn rotate(&mut self) {
        if std::fs::rename(&self.path, &self.rotated_path).is_err() {
            return;
        }
        match std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.path)
        {
            Ok(f) => {
                self.file = f;
                self.bytes_written = 0;
            }
            Err(_) => {
                // Couldn't reopen the active path after rotating — fall back
                // to appending to the rotated file so we keep a live fd.
                if let Ok(f) = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&self.rotated_path)
                {
                    self.file = f;
                }
            }
        }
    }
}

/// Spawn a basic reader thread: forwards PTY output to stdout, stamps
/// `last_output` on every successful read, and (if `pty_log` is provided) tees
/// the raw bytes to that rotating evidence log — capture so a stalled/killed
/// session is diagnosable after the fact (Tarn, W536: stalls otherwise leave
/// no corpse).
///
/// Returns the join handle plus the shared stop flag and last-output timestamp
/// already cloned for the caller's use in the poll loop.
fn spawn_basic_reader(
    mut reader: Box<dyn Read + Send>,
    stop_reader: Arc<Mutex<bool>>,
    last_output_reader: Arc<Mutex<Instant>>,
    mut pty_log: Option<RingLog>,
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

                    // Drain to stdout FIRST. This is the contract this thread
                    // exists for — keeping the PTY buffer drained so the
                    // child never blocks on write(). The evidence log below
                    // is best-effort and secondary: if it's slow (a full
                    // disk, a wedged network mount, --pty-log-dir pointed
                    // somewhere unfortunate), it must not be the thing that
                    // makes this stall-diagnosis instrument manufacture a
                    // stall by delaying the next read() (Lens, W536 round 2).
                    let mut out = stdout.lock();
                    // Best-effort: if stdout is broken (consumer closed pipe), stop.
                    if out.write_all(&buf[..n]).is_err() {
                        break;
                    }
                    let _ = out.flush();
                    drop(out);

                    // Tee to the evidence log after stdout. Independent of
                    // the stdout write above for error propagation — a full
                    // log cap or a log write failure never stops the session
                    // — but ordered after it for latency, per the comment
                    // above.
                    if let Some(ref mut log) = pty_log {
                        if log.write(&buf[..n]).is_err() {
                            // Log write failed (disk full, etc.) — stop
                            // trying for the rest of the session.
                            pty_log = None;
                        }
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break, // PTY closed or child exited
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

/// How long after a keepalive injection a read is still presumed to be the
/// PTY echoing the injected bytes back, rather than genuine child output.
///
/// This is bounded by tty line-discipline latency — milliseconds, since the
/// ESC-ESC + prompt + CR sequence is echoed by the kernel line discipline,
/// not generated by the (possibly stalled) child — plus headroom. It is
/// deliberately NOT sized to how long a genuine response takes to stream:
/// making this value large turns the recovery check in `tick_idle` into a
/// "the child must still be producing output N seconds after the keepalive"
/// bar, and a healthy session whose reply streams for less than that long
/// (a normal quick response) would never be credited with recovery — four
/// such gaps in one session reaches `IdleExhausted` and gets SIGKILLed with
/// every keepalive having actually worked. (Lens, W536 review of PR #15:
/// measured exactly this with the previous 20s "grace" constant repurposed
/// as this comparison's threshold — reads landing at injection+8s, well
/// after echo settles but short of 20s, were never credited.) Lowering this
/// value makes recovery *easier* to credit; raising it makes healthy
/// sessions more likely to be killed. Keep it close to the echo's real
/// latency, not the size of an expected reply.
const KEEPALIVE_ECHO_SETTLE_SECS: u64 = 2;

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
/// Returns `IdleTick::Ok` immediately when idle detection is disabled
/// (`state.timeout == 0`). Otherwise it checks how long the PTY has been silent
/// and injects a keepalive, reports recovery, or reports exhaustion accordingly.
fn tick_idle(
    state: &mut IdleState,
    last_output: &Arc<Mutex<Instant>>,
    pty_writer: &mut Option<Box<dyn Write + Send>>,
) -> IdleTick {
    if state.timeout == 0 {
        return IdleTick::Ok;
    }

    // Snapshot the actual last-read timestamp (not just its elapsed duration)
    // once here — the recovery branch below needs the raw value, not just how
    // long ago it was, to tell genuine post-grace output apart from the
    // injection's own echo.
    let last_output_ts = last_output
        .lock()
        .map(|ts| *ts)
        .unwrap_or_else(|_| Instant::now());
    let silent_secs = last_output_ts.elapsed();

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
            // Send ESC-ESC to cancel any stalled generation. Single ESC can
            // be consumed as a sequence prefix; double ESC reliably cancels.
            let _ = w.write_all(b"\x1b\x1b");
            let _ = w.flush();
            // Brief pause to let the model process the cancel.
            thread::sleep(Duration::from_millis(50));
            // Inject the keepalive prompt. CR submits; LF only inserts a
            // newline in the multi-line editor and never submits.
            let _ = w.write_all(state.prompt.as_bytes());
            let _ = w.write_all(b"\r");
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
        // as genuine recovery if the MOST RECENT recorded read (last_output_ts,
        // snapshotted above) landed at or after the echo-settle deadline. This
        // is deliberately NOT `last_keepalive.elapsed() >= N` (wall-clock "now"
        // vs. the keepalive time) — that check is a pure function of how long
        // the poll loop has been running and goes true regardless of whether
        // the child ever produced another byte, which was the original W536
        // defect: the constant used for this comparison (formerly 20s, sized
        // as a "grace window") was always less than any realistic idle
        // timeout, so it silently zeroed the retry counter every cycle and
        // made `IdleTick::Exhausted` unreachable.
        //
        // The injection branch above resets `last_output` to `Instant::now()`
        // at the moment of injection, so if the child produces nothing more
        // than the injection's own echo (which lands within a tick or two —
        // see `KEEPALIVE_ECHO_SETTLE_SECS`), `last_output_ts` never advances
        // past the deadline and this check correctly stays false forever —
        // the outer branch's silent_secs will grow again and drive the next
        // real retry after a full idle_timeout of continued silence. A read
        // landing at or after the deadline (genuine output continuing past
        // the echo-settling window, however briefly) satisfies it — the
        // check only needs to rule out the echo, not demand sustained output.
        let past_echo_settle = state
            .last_keepalive
            .map(|kt| last_output_ts >= kt + Duration::from_secs(KEEPALIVE_ECHO_SETTLE_SECS))
            // last_keepalive is always Some here: it's set in the same branch
            // that increments retry_count, and Recovered clears both
            // together, so retry_count > 0 implies last_keepalive.is_some().
            // If that invariant ever breaks, default to NOT crediting
            // recovery (conservative — the failure direction this fix exists
            // to eliminate is over-eager recovery, not under-eager).
            .unwrap_or(false);
        if past_echo_settle {
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
/// the signal file is deleted and the child's process group is terminated:
/// SIGTERM first, then SIGKILL after a short grace period if it has not exited.
///
/// `idle` — optional idle detection config. When `idle.timeout_secs > 0` and
/// the PTY produces no output for that many seconds, ESC-ESC followed by
/// the `idle` keepalive text and a carriage return is sent to unstick a stalled session. After
/// `idle.max_retries` injections without recovery, the child is killed.
///
/// `pty_log_dir` — optional directory for a per-run evidence log of the raw
/// PTY child output (tee'd alongside the normal stdout stream, capped at
/// `PTY_LOG_MAX_BYTES`). Disabled when `None` (the default: this is opt-in
/// evidence capture, not on-by-default behaviour). Naming and enabling this is
/// the caller's concern — e.g. Fen's triage harness passes its own log dir.
pub fn run(
    argv: &[String],
    cwd: &Path,
    timeout_secs: u64,
    exit_signal: Option<&Path>,
    idle: Option<&IdleConfig>,
    pty_log_dir: Option<&Path>,
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

    let pty_log = pty_log_dir.and_then(RingLog::open);
    let reader_thread =
        spawn_basic_reader(reader, Arc::clone(&stop), Arc::clone(&last_output), pty_log);

    // Delete any stale signal file left over from a previous crash before
    // entering the poll loop. Without this, a file orphaned by a prior
    // abnormal exit would trigger an immediate termination on the very first
    // poll tick, killing the new session before the child has done any work.
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
                    tick_idle(&mut idle_state, &last_output, &mut pty_writer)
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

    let exit_code = if exit_sent { 0 } else { exit_code };
    Ok(RunResult { exit_code })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

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
            run(&["false".to_string()], &tmp(), 10, None, None, None).expect("run should succeed");
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
    /// child process group, and the shell is killed — causing it to exit with
    /// a non-zero (signal-death) exit code.
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
        // detects the signal file and sends SIGTERM; the shell dies, but
        // exit_sent overrides the code to 0 — clean exit is clean exit.
        let result = run(
            &["sh".to_string(), "-c".to_string(), "read line".to_string()],
            &tmp(),
            10, // generous timeout so the test doesn't hang on slow CI
            Some(&signal_path),
            None,
            None,
        )
        .expect("run should succeed");

        writer_thread.join().expect("writer thread panicked");

        assert_eq!(
            result.exit_code, 0,
            "child killed via exit-signal should return exit code 0"
        );
        // Signal file should have been consumed when the exit was triggered.
        assert!(
            !signal_path.exists(),
            "signal file should be deleted after SIGTERM is sent"
        );
    }

    /// Regression test for the dead-`--max-idle-retries` defect (Tarn, W536).
    ///
    /// `tick_idle`'s recovery branch must only credit `IdleTick::Recovered`
    /// (and reset `retry_count`) when a GENUINE read landed after the
    /// echo-settle deadline — not merely because wall-clock time has passed
    /// since the keepalive was sent. The pre-fix implementation checked
    /// `last_keepalive.elapsed() >= <constant>`, which is a function of
    /// `Instant::now()` alone and becomes true regardless of whether the
    /// child ever produced another byte. Since that constant (originally 20,
    /// sized as a "grace window") was always less than any realistic idle
    /// timeout, this silently reset the counter on every cycle, making
    /// `IdleTick::Exhausted` unreachable.
    ///
    /// This test simulates: a keepalive was sent 25s ago, the PTY's last
    /// recorded read is 24s ago — i.e. only the injection's own echo landed,
    /// one second after injection, and nothing since. No real child output
    /// ever arrived. The counter must NOT reset.
    #[test]
    fn idle_recovery_requires_genuine_output_past_echo_settle() {
        let mut state = IdleState {
            timeout: 1000, // large: keeps us in the recovery-check branch, not the trigger branch
            prompt: "Continue".to_string(),
            max_retries: 3,
            retry_count: 1, // one keepalive already sent this cycle
            last_keepalive: Some(Instant::now() - Duration::from_secs(25)),
        };
        // Only the echo of the injected bytes arrived, one second after
        // injection — well within the echo-settle window, and nothing since.
        let last_output = Arc::new(Mutex::new(Instant::now() - Duration::from_secs(24)));
        let mut writer: Option<Box<dyn Write + Send>> = None;

        let tick = tick_idle(&mut state, &last_output, &mut writer);

        assert_eq!(
            state.retry_count, 1,
            "retry counter must not reset without a genuine read past the echo-settle deadline"
        );
        assert!(
            !matches!(tick, IdleTick::Recovered),
            "must not report Recovered without genuine post-settle output"
        );
    }

    /// Companion to the regression test above: genuine output that lands AT
    /// OR AFTER the echo-settle deadline must still be credited as real
    /// recovery. The fix must close the false-positive hole without also
    /// disabling the legitimate recovery path.
    #[test]
    fn idle_recovery_credited_for_genuine_post_settle_output() {
        let mut state = IdleState {
            timeout: 1000,
            prompt: "Continue".to_string(),
            max_retries: 3,
            retry_count: 1,
            last_keepalive: Some(Instant::now() - Duration::from_secs(25)),
        };
        // A real read landed 3s ago — well after the echo-settle deadline
        // relative to the keepalive sent 25s ago.
        let last_output = Arc::new(Mutex::new(Instant::now() - Duration::from_secs(3)));
        let mut writer: Option<Box<dyn Write + Send>> = None;

        let tick = tick_idle(&mut state, &last_output, &mut writer);

        assert_eq!(
            state.retry_count, 0,
            "genuine output past the echo-settle deadline must reset the retry counter"
        );
        assert!(matches!(tick, IdleTick::Recovered));
    }

    /// Regression test for Lens's IMPORTANT finding on PR #15 (W536 round 2):
    /// with the old `KEEPALIVE_GRACE_SECS` (20s) reused as the recovery-check
    /// threshold, a genuine reply that streamed output and then legitimately
    /// went quiet — but finished streaming before the 20s mark — was never
    /// credited as recovery. Lens measured this directly: a session whose
    /// keepalive worked every time (response ran injection+0s to
    /// injection+8s) still reached `IdleExhausted` and got SIGKILLed after
    /// enough idle cycles, because the 20s "grace" had silently become a
    /// "must still be talking 20s later" bar instead of an echo filter.
    ///
    /// This test simulates exactly that: a keepalive was sent 25s ago, and
    /// the last genuine read landed at injection+8s (17s ago) — well past
    /// echo-settle, but well short of the old 20s mark. Must be credited.
    ///
    /// Fails-first: red against the `KEEPALIVE_GRACE_SECS = 20` comparison
    /// (deadline at 25s-ago+20s = 5s-ago; 17s-ago is not >= 5s-ago), green
    /// against `KEEPALIVE_ECHO_SETTLE_SECS = 2` (deadline at 25s-ago+2s =
    /// 23s-ago; 17s-ago >= 23s-ago is true).
    #[test]
    fn idle_recovery_credited_for_response_shorter_than_old_grace_window() {
        let mut state = IdleState {
            timeout: 1000,
            prompt: "Continue".to_string(),
            max_retries: 3,
            retry_count: 1,
            last_keepalive: Some(Instant::now() - Duration::from_secs(25)),
        };
        // Genuine response streamed from injection to roughly injection+8s,
        // then the session went quiet again — a normal, healthy short reply.
        let last_output = Arc::new(Mutex::new(Instant::now() - Duration::from_secs(17)));
        let mut writer: Option<Box<dyn Write + Send>> = None;

        let tick = tick_idle(&mut state, &last_output, &mut writer);

        assert_eq!(
            state.retry_count, 0,
            "a genuine reply that streamed past echo-settle, even if it ended \
             well before the old 20s grace mark, must be credited as recovery — \
             a healthy session must not be driven toward IdleExhausted"
        );
        assert!(matches!(tick, IdleTick::Recovered));
    }

    /// Pins the inclusive `>=` boundary in the echo-settle comparison (Lens,
    /// notable finding: nothing previously constructed the exact edge, so a
    /// future `>=` -> `>` typo would leave the rest of the suite green).
    /// Constructs the last read to land at EXACTLY `last_keepalive +
    /// KEEPALIVE_ECHO_SETTLE_SECS` using `Instant` arithmetic (no sleeping,
    /// no flakiness) and asserts it counts as recovery, matching the
    /// docstring's "at or after."
    #[test]
    fn idle_recovery_boundary_is_inclusive_at_echo_settle_deadline() {
        let last_keepalive = Instant::now() - Duration::from_secs(30);
        let mut state = IdleState {
            timeout: 1000,
            prompt: "Continue".to_string(),
            max_retries: 3,
            retry_count: 1,
            last_keepalive: Some(last_keepalive),
        };
        let last_output = Arc::new(Mutex::new(
            last_keepalive + Duration::from_secs(KEEPALIVE_ECHO_SETTLE_SECS),
        ));
        let mut writer: Option<Box<dyn Write + Send>> = None;

        let tick = tick_idle(&mut state, &last_output, &mut writer);

        assert_eq!(
            state.retry_count, 0,
            "a read landing exactly at the echo-settle deadline must be credited (inclusive boundary)"
        );
        assert!(matches!(tick, IdleTick::Recovered));
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
        // immediate termination, the child would still exit 0 — but the
        // important thing is that the file was deleted during startup, not
        // during the first poll tick, so we also check that it's gone after run().
        let result = run(
            &["echo".to_string(), "hello".to_string()],
            &tmp(),
            10,
            Some(&signal_path),
            None,
            None,
        )
        .expect("run should succeed");

        assert_eq!(result.exit_code, 0);
        assert!(
            !signal_path.exists(),
            "stale signal file should be deleted by run() before poll loop"
        );
    }

    /// Regression test for Lens's NOTABLE finding on PR #15 (W536 round 2):
    /// the original cap implementation kept the FIRST `PTY_LOG_MAX_BYTES` of
    /// output and dropped everything after — exactly backwards for a
    /// stall/kill corpse log, where the bytes right before the event are the
    /// diagnostic payload. `RingLog` rotates instead, so the most recent
    /// output survives.
    ///
    /// Uses `open_with_cap_half` with a tiny threshold (48 bytes) instead of
    /// the real 5 MiB half-cap, so the test proves the rotation logic without
    /// writing multi-megabyte fixtures.
    #[test]
    fn ring_log_rotation_keeps_most_recent_bytes_drops_oldest_head() {
        let dir = TempDir::new().expect("create temp dir");
        let mut log = RingLog::open_with_cap_half(dir.path(), 48).expect("open ring log");

        // Seven 16-byte chunks (verified below). cap_half=48 means a
        // rotation fires after every third chunk (16 * 3 = 48).
        let chunk = |n: u8| -> Vec<u8> {
            let s = format!("chunk{n}-aaaaaaaaa"); // "chunkN-" (7) + 9 'a's = 16 bytes
            assert_eq!(s.len(), 16, "test fixture chunk must be exactly 16 bytes");
            s.into_bytes()
        };

        log.write(&chunk(1)).expect("write chunk1"); // active: 16 bytes
        log.write(&chunk(2)).expect("write chunk2"); // active: 32 bytes
        log.write(&chunk(3)).expect("write chunk3"); // active hits 48 -> rotates:
                                                     // rotated_path = chunk1+2+3, active fresh
        log.write(&chunk(4)).expect("write chunk4"); // active: 16 bytes
        log.write(&chunk(5)).expect("write chunk5"); // active: 32 bytes
        log.write(&chunk(6)).expect("write chunk6"); // active hits 48 -> rotates:
                                                     // rotated_path = chunk4+5+6 (overwrites 1+2+3), active fresh
        log.write(&chunk(7))
            .expect("write chunk7 (partial, no rotation)"); // active: 16 bytes, no rotation yet

        // Exactly two files on disk: the active file and the one rotation.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read log dir")
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(
            entries.len(),
            2,
            "expected active + one rotated file, found: {:?}",
            entries.iter().map(|e| e.path()).collect::<Vec<_>>()
        );

        let active = std::fs::read_to_string(&log.path).expect("read active log");
        let rotated = std::fs::read_to_string(&log.rotated_path).expect("read rotated log");
        let combined = format!("{rotated}{active}");

        for oldest in ["chunk1-", "chunk2-", "chunk3-"] {
            assert!(
                !combined.contains(oldest),
                "{oldest} is from the oldest rotated-out chunk and must be dropped, \
                 got combined content: {combined:?}"
            );
        }
        for newest in ["chunk4-", "chunk5-", "chunk6-", "chunk7-"] {
            assert!(
                combined.contains(newest),
                "{newest} is recent output and must survive the cap, \
                 got combined content: {combined:?}"
            );
        }
        assert!(
            active.contains("chunk7-"),
            "the active file specifically must hold the very latest write, got: {active:?}"
        );
    }

    /// Regression test for Lens's NOTABLE finding on PR #15 (W536 round 3):
    /// when `rotate()`'s rename fails, `bytes_written` is never reset, so
    /// every subsequent `write()` calls `rotate()` again, which retries the
    /// same doomed rename forever — one failing syscall per read on the
    /// drain thread — while the active file grows without bound, silently
    /// voiding the disk-fill guard `PTY_LOG_MAX_BYTES`/`cap_half` is supposed
    /// to provide.
    ///
    /// Reproduces Lens's method: chmod the log directory read-only after
    /// opening, so `rename` (which needs write permission on the containing
    /// directory to move an entry out of it) fails with EACCES while the
    /// already-open file descriptor stays valid and further writes keep
    /// succeeding.
    ///
    /// Expected to FAIL against the current `rotate()`: the loop below
    /// writes 200 * 16 = 3200 bytes against a 64-byte `cap_half`, and the
    /// active file's size should track all of it once rotation is broken.
    #[cfg(unix)]
    #[test]
    fn ring_log_stops_growing_when_rotation_fails() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().expect("create temp dir");
        let mut log = RingLog::open_with_cap_half(dir.path(), 64).expect("open ring log");

        // Remove write permission on the log directory so the rename inside
        // rotate() fails with EACCES. The already-open fd on the active file
        // stays valid; only future directory-entry operations (rename,
        // create) are blocked.
        let mut perms = std::fs::metadata(dir.path())
            .expect("stat log dir")
            .permissions();
        perms.set_mode(0o555);
        std::fs::set_permissions(dir.path(), perms).expect("chmod log dir read-only");

        // Write well past cap_half in a loop. Every write should succeed
        // (writing must never surface the rotation failure as an error to
        // the caller), but the active file must not grow without bound.
        for _ in 0..200 {
            log.write(b"0123456789012345")
                .expect("write must not error even when rotation is broken");
        }

        // Restore permissions before reading metadata / TempDir cleanup —
        // otherwise TempDir's own Drop can fail to remove a read-only dir.
        let restore = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(dir.path(), restore).expect("restore log dir perms for cleanup");

        let size = std::fs::metadata(&log.path)
            .expect("stat active log file")
            .len();
        assert!(
            size <= 64,
            "the cap must hold even when rotation fails — active file grew to \
             {size} bytes against cap_half=64 (wrote 3200 bytes total)"
        );
    }
}

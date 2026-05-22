//! heartbeat-launch: Launch a command inside a PTY.
//!
//! Designed to give Claude Code interactive mode by allocating a real PTY,
//! so CC's isTTY check succeeds and it runs in `cli` (not `sdk-cli`) mode.
//!
//! Usage:
//!   heartbeat-launch [--cwd <dir>] [--timeout <secs>] -- claude --model opus "Read CLAUDE.md"
//!
//! Everything after `--` is argv passed directly to the child process.
//! No inbox management, no settings.json generation, no handshake.
//! The consumer handles all of that.

use clap::Parser;
use heartbeat_rs::pty::{self, IdleConfig, QueueConfig};
use std::path::PathBuf;
use std::process;

#[derive(Parser)]
#[command(name = "heartbeat-launch")]
#[command(about = "Launch a command inside a PTY. Designed to give Claude Code interactive mode.")]
#[command(
    long_about = "Allocates a PTY via portable-pty (Unix PTY + Windows ConPTY), spawns the \
                  given command inside it, and forwards stdout to the current process. \
                  Polls for child exit with a configurable timeout and exits with the \
                  child's exit code.\n\n\
                  Everything after `--` is the command and its arguments. The consumer \
                  is responsible for inbox setup, settings.json, and stop hook wiring."
)]
struct Cli {
    /// Working directory for the child process.
    #[arg(long, default_value = ".")]
    cwd: String,

    /// Timeout in seconds (0 = no timeout).
    #[arg(long, default_value = "3600")]
    timeout: u64,

    /// Optional path to an exit signal file.
    ///
    /// When heartbeat-stop decides the session should end (Approve), it
    /// touches this file. heartbeat-launch detects the file in its poll loop,
    /// writes `/exit\n` to the PTY master, and deletes the file. The child
    /// then receives the command and exits normally.
    ///
    /// Must match the `--signal-file` value passed to heartbeat-stop.
    /// If omitted, no signal-file coordination is performed.
    #[arg(long)]
    exit_signal: Option<PathBuf>,

    /// Idle detection timeout in seconds (0 = disabled, default).
    ///
    /// If the PTY produces no output for this many seconds, a keepalive
    /// sequence is injected: ESC (to cancel any stalled generation) followed
    /// by --idle-prompt and a newline. This unsticks sessions where the
    /// Anthropic API stream has hung mid-generation.
    #[arg(long, default_value = "0")]
    idle_timeout: u64,

    /// Text to inject after ESC when idle is detected.
    ///
    /// Only used when --idle-timeout > 0. Sent as plain text followed by a
    /// newline to the PTY master after the ESC cancel byte.
    #[arg(long, default_value = "Continue")]
    idle_prompt: String,

    /// Maximum keepalive injections before giving up and killing the child.
    ///
    /// If the session remains idle after this many injections, the child is
    /// killed and heartbeat-launch exits 125 (distinct from --timeout's 124).
    /// Only used when --idle-timeout > 0.
    #[arg(long, default_value = "3")]
    max_idle_retries: u32,

    /// Path to a JSONL queue file.  Enables queue mode.
    ///
    /// When set, each line of the file is treated as one queue entry.  The
    /// controller injects entries one at a time, waiting for --queue-sentinel
    /// between each injection.
    #[arg(long)]
    queue: Option<PathBuf>,

    /// Sentinel string to detect in PTY output between queue entries.
    ///
    /// Only used when --queue is set.
    #[arg(long, default_value = "ENTRY_DONE")]
    queue_sentinel: String,

    /// Seconds of output silence after boot before injecting the first entry.
    ///
    /// Gives Claude time to load context files before the first entry arrives.
    /// Only used when --queue is set.
    #[arg(long, default_value = "5")]
    queue_boot_delay: u64,

    /// Format string for each injected queue entry.
    ///
    /// Placeholders: {index} (1-based position), {total} (total entries),
    /// {content} (the raw queue line).  A trailing newline is appended
    /// automatically.  Only used when --queue is set.
    #[arg(long, default_value = "Entry {index} of {total}:\n{content}\n\nProcess this entry. Output the result, then ENTRY_DONE.")]
    queue_entry_template: String,

    /// Message sent to the PTY after all queue entries are consumed.
    ///
    /// A trailing newline is appended automatically.
    /// Only used when --queue is set.
    #[arg(long, default_value = "All entries processed. Output QUEUE_COMPLETE with a summary.")]
    queue_done_message: String,

    /// Command and arguments to run inside the PTY.
    /// Pass everything after `--`.
    #[arg(trailing_var_arg = true, required = true)]
    cmd: Vec<String>,
}

fn main() {
    let cli = Cli::parse();

    let cwd = PathBuf::from(&cli.cwd);
    if !cwd.exists() {
        eprintln!(
            "heartbeat-launch: working directory does not exist: {}",
            cwd.display()
        );
        process::exit(1);
    }

    let cwd = match cwd.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "heartbeat-launch: cannot resolve working directory {}: {e}",
                cli.cwd
            );
            process::exit(1);
        }
    };

    if cli.cmd.is_empty() {
        eprintln!("heartbeat-launch: no command specified");
        process::exit(1);
    }

    if cli.timeout > 0 {
        eprintln!(
            "heartbeat-launch: spawning {:?} in {} with {}s timeout",
            cli.cmd,
            cwd.display(),
            cli.timeout
        );
    } else {
        eprintln!(
            "heartbeat-launch: spawning {:?} in {} (no timeout)",
            cli.cmd,
            cwd.display()
        );
    }

    let idle_cfg = if cli.idle_timeout > 0 {
        Some(IdleConfig {
            timeout_secs: cli.idle_timeout,
            prompt: cli.idle_prompt.clone(),
            max_retries: cli.max_idle_retries,
        })
    } else {
        None
    };

    let result = if let Some(queue_path) = cli.queue {
        let queue_cfg = QueueConfig {
            queue_path,
            sentinel: cli.queue_sentinel,
            boot_delay_secs: cli.queue_boot_delay,
            entry_template: cli.queue_entry_template,
            done_message: cli.queue_done_message,
        };
        eprintln!(
            "heartbeat-launch: queue mode enabled — file={}, sentinel={:?}, boot_delay={}s",
            queue_cfg.queue_path.display(),
            queue_cfg.sentinel,
            queue_cfg.boot_delay_secs,
        );
        pty::run_with_queue(
            &cli.cmd,
            &cwd,
            cli.timeout,
            cli.exit_signal.as_deref(),
            idle_cfg.as_ref(),
            &queue_cfg,
        )
    } else {
        pty::run(&cli.cmd, &cwd, cli.timeout, cli.exit_signal.as_deref(), idle_cfg.as_ref())
    };

    match result {
        Ok(result) => {
            // Cap exit code at 123 to avoid colliding with 124 (Timeout),
            // 125 (IdleExhausted), and the signal-death range (126-127 on
            // POSIX shells).  Values above 123 from a process exit() call are
            // technically valid but unusual; saturation avoids silent i32
            // wrapping on values that exceed i32::MAX (portable-pty: u32).
            let code = result.exit_code.min(123) as i32;
            process::exit(code);
        }
        Err(pty::PtyError::Timeout(secs)) => {
            eprintln!("heartbeat-launch: timeout after {secs}s — child killed");
            process::exit(124); // same convention as `timeout(1)` on Linux
        }
        Err(pty::PtyError::IdleExhausted(secs)) => {
            eprintln!("heartbeat-launch: idle exhausted — no output for {secs}s after maximum keepalive retries, child killed");
            process::exit(125);
        }
        Err(e) => {
            eprintln!("heartbeat-launch: error: {e}");
            process::exit(1);
        }
    }
}

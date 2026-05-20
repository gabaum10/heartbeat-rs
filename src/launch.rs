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
use heartbeat_rs::pty;
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

    match pty::run(&cli.cmd, &cwd, cli.timeout, cli.exit_signal.as_deref()) {
        Ok(result) => {
            // Cap exit code at 125 to avoid overlapping with the timeout
            // convention (124) and signal-death range (126-127 on POSIX shells).
            // Values above 125 from a process exit() call are technically valid
            // but unusual; saturation avoids silent i32 wrapping on values that
            // exceed i32::MAX (which portable-pty represents as u32).
            let code = result.exit_code.min(125) as i32;
            process::exit(code);
        }
        Err(pty::PtyError::Timeout(secs)) => {
            eprintln!("heartbeat-launch: timeout after {secs}s — child killed");
            process::exit(124); // same convention as `timeout(1)` on Linux
        }
        Err(e) => {
            eprintln!("heartbeat-launch: error: {e}");
            process::exit(1);
        }
    }
}

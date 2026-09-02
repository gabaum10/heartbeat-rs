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
use heartbeat_rs::pty::{self, IdleConfig};
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
    /// deletes it, and terminates the child's process group (SIGTERM then
    /// SIGKILL after a short grace period), ending the session.
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

    let result = pty::run(
        &cli.cmd,
        &cwd,
        cli.timeout,
        cli.exit_signal.as_deref(),
        idle_cfg.as_ref(),
    );

    match result {
        Ok(result) => {
            // Forward the child's real exit code unflattened. Reserving a
            // numeric band (the old 123 clamp) can't actually separate
            // heartbeat's own outcomes from the child's: the child is free
            // to exit with any code in that same space (e.g. a script that
            // legitimately exits 124), so a code-based reservation was
            // never sound — only ever approximate. What *is* sound: every
            // heartbeat-decided outcome (Timeout, IdleExhausted, the
            // generic Err arm below) already writes a distinct
            // heartbeat-authored stderr line before it exits. A plain
            // forward here does the same, so presence/absence of a
            // "heartbeat-launch:" stderr line — not the numeric value —
            // is what tells a reader which kind of exit they're looking
            // at. The forwarded value is the child's real code EXCEPT on
            // the `--exit-signal` path, where pty.rs synthesizes 0 once
            // the exit signal has been sent, regardless of how the child
            // actually died.
            //
            // u32 -> i32: exit_code is portable-pty's u32. On Unix, a
            // child that exits normally has already been through the
            // kernel's own truncation to 0..=255 before we ever see it
            // (portable-pty's `ExitStatus::from<std::process::ExitStatus>`
            // reads `status.code()`, which is `Some` only for a normal
            // exit and truncated via WEXITSTATUS in that case). A child
            // that dies by signal has no WEXITSTATUS value at all —
            // `status.code()` is `None`, and portable-pty maps that to a
            // literal 1 (`unwrap_or(1)`), not a kernel-truncated code. So
            // this channel cannot distinguish "child exited 1" from
            // "child was killed by a signal"; both arrive here as 1. On
            // Windows exit_code is GetExitCodeProcess's raw DWORD and can
            // exceed i32::MAX; try_from surfaces that instead of silently
            // wrapping it.
            let code = match i32::try_from(result.exit_code) {
                Ok(code) => code,
                Err(_) => {
                    eprintln!(
                        "heartbeat-launch: child exit code {} exceeds i32::MAX, capping to i32::MAX",
                        result.exit_code
                    );
                    i32::MAX
                }
            };
            eprintln!("heartbeat-launch: child exited with code {code}");
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

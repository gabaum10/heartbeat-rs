//! heartbeat-stop: Claude Code stop hook for autonomous agent loops.
//!
//! Reads from a JSONL inbox at a byte offset. Outputs a block decision to keep
//! a Claude Code session alive, or nothing to let it end.
//!
//! Usage:
//!   heartbeat-stop --inbox /path/to/inbox.jsonl --mode drain
//!   heartbeat-stop --inbox /path/to/inbox.jsonl --mode persist
//!   heartbeat-stop recover --inbox /path/to/inbox.jsonl --on-orphan deadletter

use clap::{Parser, Subcommand, ValueEnum};
use heartbeat_rs::hook;
use heartbeat_rs::recover::{self, OrphanPolicy};
use std::io::Write;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "heartbeat-stop",
    about = "Claude Code stop hook for autonomous agent loops",
    long_about = "Reads from a JSONL inbox at a byte offset and outputs a block/approve \
                  decision. Used as a Stop hook in .claude/settings.json to keep a \
                  Claude Code session alive while messages are queued.\n\n\
                  Also provides `recover` subcommand for launcher-side orphan recovery."
)]
struct Args {
    /// Subcommand. If absent, runs the stop hook (default behaviour).
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to the JSONL inbox file.
    /// Lines are plain text prompts, one per line.
    #[arg(long, global = true)]
    inbox: Option<PathBuf>,

    /// Operating mode.
    /// drain: approve stop when inbox is empty (session ends).
    /// persist: send idle ticks when empty (session stays alive).
    #[arg(long, default_value = "drain")]
    mode: CliMode,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run orphan recovery for the inbox. Call this BEFORE truncating the inbox
    /// or resetting the offset at the start of each launcher cycle.
    ///
    /// Detects any `.in-flight` artifact from a prior crashed session and
    /// applies the configured policy. Returns exit code 0 on success.
    Recover {
        /// What to do with an orphaned in-flight entry.
        ///
        /// retry     — Re-deliver the orphan as the first entry of the next
        ///             session. Use when agent-side work is idempotent.
        ///             Risk: duplicate side effects if agent already processed it.
        ///
        /// deadletter — Move orphan to .dead-letter.jsonl, advance cursor.
        ///             Use when duplicate side effects are unacceptable.
        ///             Requires operator attention to drain the dead-letter file.
        ///             This is the default.
        ///
        /// drop      — Delete .in-flight and advance cursor. Accept the loss.
        ///             Use when an upstream retry mechanism (e.g., IMAP re-fetch)
        ///             covers re-delivery anyway.
        #[arg(long, default_value = "deadletter")]
        on_orphan: CliOrphanPolicy,
    },
}

#[derive(Debug, Clone, ValueEnum)]
enum CliMode {
    /// Exit session when inbox is drained (timer-triggered or fresh-per-event).
    Drain,
    /// Send idle ticks when inbox is empty (persistent supervisor).
    Persist,
}

#[derive(Debug, Clone, ValueEnum)]
enum CliOrphanPolicy {
    /// Re-deliver orphan as the first entry of the next session.
    Retry,
    /// Move orphan to .dead-letter.jsonl and advance cursor (default).
    Deadletter,
    /// Drop orphan and advance cursor (use when upstream retry covers it).
    Drop,
}

impl From<CliMode> for hook::Mode {
    fn from(m: CliMode) -> Self {
        match m {
            CliMode::Drain => hook::Mode::Drain,
            CliMode::Persist => hook::Mode::Persist,
        }
    }
}

impl From<CliOrphanPolicy> for OrphanPolicy {
    fn from(p: CliOrphanPolicy) -> Self {
        match p {
            CliOrphanPolicy::Retry => OrphanPolicy::Retry,
            CliOrphanPolicy::Deadletter => OrphanPolicy::DeadLetter,
            CliOrphanPolicy::Drop => OrphanPolicy::Drop,
        }
    }
}

fn main() {
    let args = Args::parse();

    match args.command {
        Some(Command::Recover { on_orphan }) => {
            let inbox = match args.inbox {
                Some(p) => p,
                None => {
                    eprintln!("heartbeat-stop recover: --inbox is required");
                    std::process::exit(1);
                }
            };

            let policy = OrphanPolicy::from(on_orphan);
            match recover::recover(&inbox, policy) {
                Ok(outcome) => {
                    eprintln!("heartbeat-stop recover: {outcome:?}");
                }
                Err(e) => {
                    eprintln!("heartbeat-stop recover: {e}");
                    std::process::exit(1);
                }
            }
            std::process::exit(0);
        }

        None => {
            // Default: run the stop hook state machine.
            let inbox = match args.inbox {
                Some(p) => p,
                None => {
                    eprintln!("heartbeat-stop: --inbox is required");
                    std::process::exit(1);
                }
            };

            let mode = hook::Mode::from(args.mode);
            let decision = match hook::run(&inbox, &mode) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("heartbeat-stop: error: {e}");
                    // Fail-open: approve the stop rather than blocking indefinitely.
                    hook::Decision::Approve
                }
            };

            let output = hook::serialize(&decision);
            if !output.is_empty() {
                if let Err(e) = std::io::stdout().write_all(output.as_bytes()) {
                    if e.kind() == std::io::ErrorKind::BrokenPipe {
                        std::process::exit(0);
                    }
                    eprintln!("heartbeat-stop: fatal: stdout write failed: {e}");
                    std::process::exit(1);
                }
            }
            // Exit 0 in all cases. Claude Code reads stdout for the decision;
            // non-zero exit codes from hooks may be treated as errors.
            std::process::exit(0);
        }
    }
}

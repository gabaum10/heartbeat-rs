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
    version,
    about = "Claude Code stop hook for autonomous agent loops",
    long_about = "Reads from a JSONL inbox at a byte offset and outputs a block/approve \
                  decision. Used as a Stop hook in .claude/settings.json to keep a \
                  Claude Code session alive while the inbox has undelivered messages.\n\n\
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

    /// Seconds to sleep between idle ticks in persist mode. 0 disables sleeping
    /// (useful for testing). Default: 2.
    /// Note: ensure your hook `timeout` in .claude/settings.json is larger than
    /// this value, or Claude Code will kill the hook before the sleep completes.
    #[arg(long, default_value = "2", value_parser = clap::value_parser!(u64))]
    idle_interval: u64,

    /// Optional path to a signal file used for PTY exit coordination.
    ///
    /// When the hook decides Approve (session should end), it touches this
    /// file before printing the empty approve output. heartbeat-launch polls
    /// for the file and terminates the child's process group (SIGTERM then
    /// SIGKILL) when it appears, ending the session.
    ///
    /// Must match the `--exit-signal` value passed to heartbeat-launch.
    /// If omitted, no signal-file coordination is performed.
    #[arg(long)]
    signal_file: Option<PathBuf>,
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

/// Consecutive fail-open passes (under idle keepalive) allowed to leave the
/// signal file untouched before the next one touches it and ends the run. Each
/// skipped pass costs one keepalive interval, so this caps a persistent error
/// at that many extra keepalive cycles. Matches heartbeat-launch's default
/// `--max-idle-retries`.
const MAX_SKIPPED_SIGNAL_TOUCHES: u32 = 3;

/// Counter file for consecutive skipped touches, next to the inbox.
const SKIP_COUNT_FILE: &str = ".fail-open-skips";

/// True when this fail-open pass may skip the signal touch: fewer than
/// `MAX_SKIPPED_SIGNAL_TOUCHES` consecutive skips are recorded, and recording
/// this one succeeded. Any doubt (count unwritable, bound reached) returns
/// false so the caller falls back to today's touch-and-end behaviour.
fn skip_touch_within_bound(io_dir: &std::path::Path) -> bool {
    let path = io_dir.join(SKIP_COUNT_FILE);
    let count: u32 = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    if count >= MAX_SKIPPED_SIGNAL_TOUCHES {
        eprintln!(
            "heartbeat-stop: fail-open skipped the signal touch {count} times in a row; touching it to end the run"
        );
        return false;
    }
    if let Err(e) = std::fs::write(&path, (count + 1).to_string()) {
        eprintln!(
            "heartbeat-stop: warning: could not record skip count at {}: {e}; touching signal file",
            path.display()
        );
        return false;
    }
    eprintln!(
        "heartbeat-stop: idle keepalive configured: leaving signal file untouched ({}/{MAX_SKIPPED_SIGNAL_TOUCHES}); next Stop re-delivers",
        count + 1
    );
    true
}

/// A pass that did not error ends any run of consecutive skips.
fn clear_skip_count(io_dir: &std::path::Path) {
    let _ = std::fs::remove_file(io_dir.join(SKIP_COUNT_FILE));
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

            let mode = hook::Mode::from(args.mode.clone());
            if matches!(args.mode, CliMode::Persist) && args.idle_interval > 0 {
                eprintln!(
                    "heartbeat-stop: idle sleep {}s — ensure hook timeout > {}s",
                    args.idle_interval, args.idle_interval
                );
            }
            let (decision, hook_errored) = match hook::run(&inbox, &mode, args.idle_interval) {
                Ok(d) => (d, false),
                Err(e) => {
                    eprintln!("heartbeat-stop: error: {e}");
                    // Fail-open: approve the stop rather than blocking indefinitely.
                    (hook::Decision::Approve, true)
                }
            };

            // Under idle keepalive (heartbeat-launch --idle-timeout > 0 sets
            // IDLE_KEEPALIVE_ENV for the child), a fail-open Approve must not
            // touch the signal file: touching it ends the run, and on a queued
            // soren-headless run that silently drops the rest of the queue.
            // Leaving it untouched lets the launcher's keepalive start a new
            // turn, whose Stop re-delivers. Bounded by MAX_SKIPPED_SIGNAL_TOUCHES
            // consecutive skips so a persistent error still ends the run.
            // Without keepalive nothing would ever re-fire the hook, so that
            // path is unchanged: Approve plus signal touch.
            let keepalive = std::env::var(hook::IDLE_KEEPALIVE_ENV).is_ok_and(|v| v == "1");
            let io_dir = inbox.parent().unwrap_or(std::path::Path::new("."));
            let skip_signal_touch = if keepalive && hook_errored {
                skip_touch_within_bound(io_dir)
            } else {
                if keepalive {
                    clear_skip_count(io_dir);
                }
                false
            };

            // Signal-file coordination: when the decision is Approve, touch the
            // signal file (if configured) so heartbeat-launch knows to terminate
            // the child's process group. Must happen before we print the approve
            // output so the launcher sees the file before Claude Code exits.
            if matches!(decision, hook::Decision::Approve) && !skip_signal_touch {
                if let Some(ref sig) = args.signal_file {
                    if let Err(e) = std::fs::OpenOptions::new()
                        .create(true)
                        .truncate(false)
                        .write(true)
                        .open(sig)
                    {
                        eprintln!(
                            "heartbeat-stop: warning: could not touch signal file {}: {e}",
                            sig.display()
                        );
                    }
                }
            }

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

//! heartbeat-stop: Claude Code stop hook for autonomous agent loops.
//!
//! Reads from a JSONL inbox at a byte offset. Outputs a block decision to keep
//! a Claude Code session alive, or nothing to let it end.
//!
//! Usage:
//!   heartbeat-stop --inbox /path/to/inbox.jsonl --mode drain
//!   heartbeat-stop --inbox /path/to/inbox.jsonl --mode persist

use clap::{Parser, ValueEnum};
use heartbeat_rs::hook;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "heartbeat-stop",
    about = "Claude Code stop hook for autonomous agent loops",
    long_about = "Reads from a JSONL inbox at a byte offset and outputs a block/approve \
                  decision. Used as a Stop hook in .claude/settings.json to keep a \
                  Claude Code session alive while messages are queued."
)]
struct Args {
    /// Path to the JSONL inbox file.
    /// Lines are plain text prompts, one per line.
    #[arg(long)]
    inbox: PathBuf,

    /// Operating mode.
    /// drain: approve stop when inbox is empty (session ends).
    /// persist: send idle ticks when empty (session stays alive).
    #[arg(long, default_value = "drain")]
    mode: CliMode,
}

#[derive(Debug, Clone, ValueEnum)]
enum CliMode {
    /// Exit session when inbox is drained (timer-triggered or fresh-per-event).
    Drain,
    /// Send idle ticks when inbox is empty (persistent supervisor).
    Persist,
}

impl From<CliMode> for hook::Mode {
    fn from(m: CliMode) -> Self {
        match m {
            CliMode::Drain => hook::Mode::Drain,
            CliMode::Persist => hook::Mode::Persist,
        }
    }
}

fn main() {
    let args = Args::parse();
    let mode = hook::Mode::from(args.mode);

    let decision = match hook::run(&args.inbox, &mode) {
        Ok(d) => d,
        Err(e) => {
            // IO errors: log to stderr so they appear in the hook's error stream.
            // Approve the stop — safer than blocking indefinitely on a broken inbox.
            eprintln!("heartbeat-stop: error reading inbox: {}", e);
            hook::Decision::Approve
        }
    };

    let output = hook::serialize(&decision);
    if !output.is_empty() {
        print!("{}", output);
    }
    // Exit 0 in all cases. Claude Code reads stdout for the decision;
    // non-zero exit codes from hooks may be treated as errors.
    std::process::exit(0);
}

//! Integration tests for the `heartbeat-stop` binary.
//!
//! These tests exercise the CLI surface directly via subprocess invocation:
//! argument parsing, exit codes, stdout/stderr discipline, and end-to-end
//! multi-invocation cycles. Unit tests cover the library internals; these
//! tests catch regressions in the binary's contract.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

/// Path to the compiled binary. `cargo test` sets CARGO_BIN_EXE_heartbeat-stop.
fn binary() -> PathBuf {
    // env! would work at compile time; we use env::var for the integration
    // test context where the cargo test harness sets the var at runtime.
    // Fallback: build path relative to workspace root.
    std::env::var("CARGO_BIN_EXE_heartbeat-stop")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let mut p = std::env::current_exe().unwrap();
            // Strip the test binary name, go up to target/debug/deps, then to target/debug.
            p.pop(); // deps
            p.pop(); // debug
            p.push("heartbeat-stop");
            p
        })
}

fn run_hook(inbox: &Path, mode: &str) -> Output {
    Command::new(binary())
        .arg("--inbox")
        .arg(inbox)
        .arg("--mode")
        .arg(mode)
        .output()
        .expect("failed to run heartbeat-stop")
}

fn run_recover(inbox: &Path, policy: &str) -> Output {
    Command::new(binary())
        .arg("recover")
        .arg("--inbox")
        .arg(inbox)
        .arg("--on-orphan")
        .arg(policy)
        .output()
        .expect("failed to run heartbeat-stop recover")
}

fn write_line(inbox: &Path, line: &str) {
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(inbox)
        .unwrap();
    writeln!(f, "{}", line).unwrap();
}

fn inbox_path(dir: &TempDir) -> PathBuf {
    dir.path().join("inbox.jsonl")
}

// ---------------------------------------------------------------------------
// (a) Deliver → respond → ack happy path
// ---------------------------------------------------------------------------

#[test]
fn happy_path_deliver_ack_exit_codes_and_stdout() {
    let dir = TempDir::new().unwrap();
    let inbox = inbox_path(&dir);

    write_line(&inbox, "triage please");

    // Tick 1: no .responded, entry present → Block. Exit 0, non-empty stdout.
    let out1 = run_hook(&inbox, "drain");
    assert_eq!(out1.status.code(), Some(0), "hook must exit 0");
    let stdout1 = String::from_utf8_lossy(&out1.stdout);
    assert!(!stdout1.is_empty(), "deliver tick must produce non-empty stdout");
    let parsed: serde_json::Value = serde_json::from_str(&stdout1)
        .expect("deliver tick stdout must be valid JSON");
    assert_eq!(parsed["decision"], "block");
    assert_eq!(parsed["reason"], "triage please");

    // .responded must exist; .in-flight must exist.
    assert!(dir.path().join(".responded").exists(), ".responded must be written");
    assert!(dir.path().join(".in-flight").exists(), ".in-flight must be written");

    // Cursor must NOT have advanced on delivery (Fix B).
    // If the offset file doesn't exist, cursor is implicitly 0 — also correct.
    let offset_path = dir.path().join(".inbox-offset");
    let cursor: u64 = fs::read_to_string(&offset_path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    assert_eq!(cursor, 0, "cursor must not advance on delivery");

    // Tick 2: .responded present, inbox empty after ack → Approve.
    // Empty stdout = approve.
    let out2 = run_hook(&inbox, "drain");
    assert_eq!(out2.status.code(), Some(0), "hook must exit 0 on approve");
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    assert!(stdout2.is_empty(), "approve tick must produce empty stdout");

    // .responded and .in-flight must be gone.
    assert!(!dir.path().join(".responded").exists(), ".responded must be removed after ack");
    assert!(!dir.path().join(".in-flight").exists(), ".in-flight must be removed after ack");

    // Cursor must have advanced past the entry.
    let cursor_after: u64 = fs::read_to_string(&offset_path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(cursor_after > 0, "cursor must advance after ack");
}

// ---------------------------------------------------------------------------
// (b) recover with corrupt .in-flight returns exit code 1
// ---------------------------------------------------------------------------

#[test]
fn recover_corrupt_in_flight_exits_nonzero() {
    let dir = TempDir::new().unwrap();
    let inbox = inbox_path(&dir);

    write_line(&inbox, "something");
    fs::write(dir.path().join(".in-flight"), "{bad json").unwrap();

    let out = run_recover(&inbox, "deadletter");
    assert_ne!(
        out.status.code(),
        Some(0),
        "recover must exit non-zero on corrupt .in-flight"
    );
}

// ---------------------------------------------------------------------------
// (c) recover --on-orphan retry + followup hook re-delivers entry K
// ---------------------------------------------------------------------------

#[test]
fn recover_retry_then_hook_redelivers_entry_k() {
    let dir = TempDir::new().unwrap();
    let inbox = inbox_path(&dir);

    write_line(&inbox, "entry K");
    write_line(&inbox, "entry K+1");

    // Tick 1: deliver K.
    let d1 = run_hook(&inbox, "drain");
    assert_eq!(d1.status.code(), Some(0));
    let stdout1 = String::from_utf8_lossy(&d1.stdout);
    let parsed1: serde_json::Value = serde_json::from_str(&stdout1).unwrap();
    assert_eq!(parsed1["reason"], "entry K");

    // Simulate session crash: .responded and .in-flight present, cursor unmoved.
    assert!(dir.path().join(".in-flight").exists());

    // recover --on-orphan retry.
    let rec = run_recover(&inbox, "retry");
    assert_eq!(rec.status.code(), Some(0), "recover must exit 0 on success");

    // .in-flight removed, .responded still present (recover doesn't touch it).
    assert!(!dir.path().join(".in-flight").exists());

    // New session: hook runs with .responded still from crash. This is the
    // inconsistent state (F12) — .responded without .in-flight → error exit.
    // The launcher must clean up .responded before launching the next session.
    // Simulate the launcher cleanup:
    let _ = fs::remove_file(dir.path().join(".responded"));

    // Now hook runs cleanly: no .responded, cursor at 0, K is first entry.
    let d2 = run_hook(&inbox, "drain");
    assert_eq!(d2.status.code(), Some(0));
    let stdout2 = String::from_utf8_lossy(&d2.stdout);
    let parsed2: serde_json::Value = serde_json::from_str(&stdout2).unwrap();
    assert_eq!(
        parsed2["reason"], "entry K",
        "hook must re-deliver entry K after retry recovery"
    );
}

// ---------------------------------------------------------------------------
// (d) --inbox missing triggers non-zero exit and stderr message
// ---------------------------------------------------------------------------

#[test]
fn missing_inbox_flag_exits_nonzero() {
    let out = Command::new(binary())
        .arg("--mode")
        .arg("drain")
        .output()
        .expect("failed to run heartbeat-stop");
    // clap will exit non-zero when a required arg is missing.
    assert_ne!(out.status.code(), Some(0), "missing --inbox must exit non-zero");
}

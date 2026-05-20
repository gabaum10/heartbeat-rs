//! Integration tests for the `heartbeat-launch` binary.
//!
//! These tests exercise the binary's CLI contract: argv passthrough, exit code
//! forwarding, cwd handling, and timeout behavior. They spawn the compiled
//! binary as a subprocess, just like a real consumer would.

use std::path::PathBuf;
use std::process::Command;

fn binary() -> PathBuf {
    std::env::var("CARGO_BIN_EXE_heartbeat-launch")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let mut p = std::env::current_exe().unwrap();
            p.pop(); // deps
            p.pop(); // debug
            p.push("heartbeat-launch");
            p
        })
}

// ---------------------------------------------------------------------------
// (a) echo hello: output contains "hello", exit code 0
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn echo_hello_output_and_exit_zero() {
    let out = Command::new(binary())
        .arg("--timeout")
        .arg("10")
        .arg("--")
        .arg("echo")
        .arg("hello")
        .output()
        .expect("failed to run heartbeat-launch");

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("hello"),
        "stdout should contain 'hello', got: {stdout:?}"
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "echo should exit 0, got: {:?}",
        out.status.code()
    );
}

// ---------------------------------------------------------------------------
// (b) `false` exits non-zero; heartbeat-launch forwards the exit code
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn exit_code_forwarded() {
    let out = Command::new(binary())
        .arg("--timeout")
        .arg("10")
        .arg("--")
        .arg("false")
        .output()
        .expect("failed to run heartbeat-launch");

    assert_ne!(
        out.status.code(),
        Some(0),
        "`false` should produce non-zero exit code"
    );
}

// ---------------------------------------------------------------------------
// (c) --cwd sets working directory; `pwd` output matches
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn cwd_flag_sets_working_directory() {
    let tmp = std::env::temp_dir();
    let out = Command::new(binary())
        .arg("--cwd")
        .arg(tmp.to_str().unwrap())
        .arg("--timeout")
        .arg("10")
        .arg("--")
        .arg("pwd")
        .output()
        .expect("failed to run heartbeat-launch");

    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    // pwd output may include trailing newline and ANSI sequences from PTY;
    // check that the canonical temp dir path appears somewhere in output.
    let canonical_tmp = tmp.canonicalize().unwrap();
    assert!(
        stdout.contains(canonical_tmp.to_str().unwrap()),
        "stdout should contain {}, got: {stdout:?}",
        canonical_tmp.display()
    );
}

// ---------------------------------------------------------------------------
// (d) timeout fires: exit code 124
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn timeout_exits_124() {
    let out = Command::new(binary())
        .arg("--timeout")
        .arg("1")
        .arg("--")
        .arg("sleep")
        .arg("60")
        .output()
        .expect("failed to run heartbeat-launch");

    assert_eq!(
        out.status.code(),
        Some(124),
        "timeout should exit 124 (timeout(1) convention), got: {:?}",
        out.status.code()
    );
}

// ---------------------------------------------------------------------------
// (e) missing command: exits non-zero
// ---------------------------------------------------------------------------

#[test]
fn missing_command_exits_nonzero() {
    let out = Command::new(binary())
        .output()
        .expect("failed to run heartbeat-launch");

    assert_ne!(
        out.status.code(),
        Some(0),
        "missing command should exit non-zero"
    );
}

// ---------------------------------------------------------------------------
// (f) --timeout 0 means no timeout: a fast command completes normally
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn timeout_zero_means_no_timeout() {
    // echo exits immediately; with --timeout 0 (no timeout) it should succeed.
    let out = Command::new(binary())
        .arg("--timeout")
        .arg("0")
        .arg("--")
        .arg("echo")
        .arg("hello")
        .output()
        .expect("failed to run heartbeat-launch");

    assert_eq!(
        out.status.code(),
        Some(0),
        "--timeout 0 should not kill immediately; echo should exit 0, got: {:?}",
        out.status.code()
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("hello"),
        "stdout should contain 'hello', got: {stdout:?}"
    );
}

// ---------------------------------------------------------------------------
// (g) PTY contract: child sees a real TTY on stdout (isTTY = true)
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn tty_is_allocated() {
    let out = Command::new(binary())
        .args(["--", "sh", "-c", "[ -t 1 ] && echo tty || echo notty"])
        .output()
        .expect("failed to run heartbeat-launch");

    let stdout = String::from_utf8_lossy(&out.stdout);
    // Trim whitespace and ANSI sequences that PTY may append; check for exact
    // "tty" rather than stdout.contains("tty"), which would also match "notty".
    assert_eq!(
        stdout.trim(),
        "tty",
        "expected stdout to be exactly 'tty' (child should see a TTY), got: {stdout:?}"
    );
}

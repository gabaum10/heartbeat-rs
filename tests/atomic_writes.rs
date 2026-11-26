//! Regression: concurrent writers to the same target must not share a temp
//! path (gabaum10/soren#69 item 1). The old fixed `<file>.tmp` name made two
//! writers truncate/rename each other's temp, exposing empty or missing
//! targets to readers.

use heartbeat_rs::in_flight::InFlightEntry;
use heartbeat_rs::inbox::{read_offset, write_offset};
use std::fs;
use std::sync::{Arc, Barrier};
use std::thread;
use tempfile::TempDir;

const WRITERS: usize = 8;
const ROUNDS: usize = 300;

#[test]
fn concurrent_write_offset_never_corrupts_or_loses_target() {
    let dir = TempDir::new().unwrap();
    let offset_file = dir.path().join(".inbox-offset");
    write_offset(&offset_file, 0).unwrap();

    let barrier = Arc::new(Barrier::new(WRITERS + 1));
    let handles: Vec<_> = (0..WRITERS)
        .map(|w| {
            let (path, barrier) = (offset_file.clone(), barrier.clone());
            thread::spawn(move || {
                barrier.wait();
                for r in 0..ROUNDS {
                    write_offset(&path, (w * ROUNDS + r) as u64 + 1).unwrap();
                }
            })
        })
        .collect();

    // Reader: the target must always exist and parse, mid-storm.
    barrier.wait();
    for _ in 0..ROUNDS * 4 {
        let v = read_offset(&offset_file)
            .expect("target must never be corrupt/empty mid-write")
            .expect("target must never be missing mid-write");
        assert!(v <= (WRITERS * ROUNDS) as u64);
    }
    for h in handles {
        h.join().unwrap();
    }

    assert_no_leftover_temps(dir.path(), &[".inbox-offset"]);
}

#[test]
fn concurrent_in_flight_writes_never_corrupt_or_lose_target() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join(".in-flight");
    InFlightEntry::new("seed", 0, 5).write_to(&path).unwrap();

    let barrier = Arc::new(Barrier::new(WRITERS + 1));
    let handles: Vec<_> = (0..WRITERS)
        .map(|w| {
            let (path, barrier) = (path.clone(), barrier.clone());
            thread::spawn(move || {
                barrier.wait();
                for r in 0..ROUNDS {
                    let line = format!("writer {w} round {r}");
                    InFlightEntry::new(&line, 0, line.len() as u64 + 1)
                        .write_to(&path)
                        .unwrap();
                }
            })
        })
        .collect();

    barrier.wait();
    for _ in 0..ROUNDS * 4 {
        let e = InFlightEntry::read_from(&path)
            .expect("target must never be corrupt/empty mid-write")
            .expect("target must never be missing mid-write");
        assert_eq!(e.end_offset, e.raw_line.len() as u64 + 1, "torn entry");
    }
    for h in handles {
        h.join().unwrap();
    }

    assert_no_leftover_temps(dir.path(), &[".in-flight"]);
}

/// Two separate processes (the actual failure mode: hook vs launcher)
/// hammering the same target. Re-execs this test binary as the child.
#[test]
fn concurrent_processes_writing_same_offset_file() {
    if let Ok(path) = std::env::var("HB_ATOMIC_CHILD_TARGET") {
        let path = std::path::PathBuf::from(path);
        for r in 0..ROUNDS {
            write_offset(&path, r as u64 + 1).unwrap();
        }
        return;
    }

    let dir = TempDir::new().unwrap();
    let target = dir.path().join(".inbox-offset");
    write_offset(&target, 0).unwrap();

    let exe = std::env::current_exe().unwrap();
    let children: Vec<_> = (0..4)
        .map(|_| {
            std::process::Command::new(&exe)
                .args([
                    "--exact",
                    "concurrent_processes_writing_same_offset_file",
                    "--nocapture",
                ])
                .env("HB_ATOMIC_CHILD_TARGET", &target)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();

    for _ in 0..ROUNDS * 4 {
        read_offset(&target)
            .expect("target must never be corrupt/empty mid-write")
            .expect("target must never be missing mid-write");
    }
    for mut c in children {
        assert!(c.wait().unwrap().success(), "child writer failed");
    }
    assert_no_leftover_temps(dir.path(), &[".inbox-offset"]);
}

fn assert_no_leftover_temps(dir: &std::path::Path, allowed: &[&str]) {
    let extra: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| !allowed.contains(&n.as_str()))
        .collect();
    assert!(extra.is_empty(), "leaked temp files: {extra:?}");
}

//! Crash-safe replace-by-rename writes with a per-writer temp name.
//!
//! Every on-disk cursor and sentinel (`.inbox-offset`, `.in-flight`) is
//! written temp-then-rename. A fixed temp name (`<file>.tmp`) is shared by
//! every process touching the same directory: two writers truncate and
//! rename each other's temp, so a reader can see an empty or half-written
//! target. The temp here is unique per writer (pid plus a process-wide
//! counter) and created with `create_new`, so no two writers can ever hold
//! the same temp path, and it lives in the target's directory so the rename
//! stays atomic.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NONCE: AtomicU64 = AtomicU64::new(0);

/// Temp path for `target`: same directory, `.<name>.<pid>.<n>.tmp`.
pub(crate) fn unique_tmp_path(target: &Path) -> PathBuf {
    let name = target.file_name().unwrap_or_default();
    // Hidden targets (".inbox-offset") already carry the dot; don't double it.
    let mut tmp_name = OsString::new();
    if !name.to_string_lossy().starts_with('.') {
        tmp_name.push(".");
    }
    tmp_name.push(name);
    tmp_name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        NONCE.fetch_add(1, Ordering::Relaxed)
    ));
    target.with_file_name(tmp_name)
}

/// Writes `contents` to `target` via unique temp + fsync + rename.
///
/// On any failure the temp file is removed; `target` is left untouched.
pub(crate) fn write_atomic(target: &Path, contents: &[u8]) -> io::Result<()> {
    let (tmp, mut f) = create_unique_tmp(target)?;
    let result = (|| {
        f.write_all(contents)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// `create_new` guarantees exclusivity even against a stale leftover or a
/// pid-reusing process; on collision, take the next nonce.
fn create_unique_tmp(target: &Path) -> io::Result<(PathBuf, fs::File)> {
    loop {
        let tmp = unique_tmp_path(target);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(f) => return Ok((tmp, f)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use tempfile::TempDir;

    #[test]
    fn tmp_paths_are_unique_and_in_target_dir() {
        let target = Path::new("/some/dir/.inbox-offset");
        let paths: HashSet<PathBuf> = (0..1000).map(|_| unique_tmp_path(target)).collect();
        assert_eq!(paths.len(), 1000, "every call must yield a distinct temp");
        for p in &paths {
            assert_eq!(p.parent(), target.parent(), "temp must share the directory");
            assert_ne!(
                p,
                &target.with_extension("tmp"),
                "must not be the old fixed name"
            );
        }
    }

    #[test]
    fn tmp_name_does_not_double_the_leading_dot() {
        let hidden = unique_tmp_path(Path::new("/d/.inbox-offset"));
        let name = hidden.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with(".inbox-offset."), "got {name}");
        assert!(!name.starts_with(".."), "got {name}");

        let plain = unique_tmp_path(Path::new("/d/plain"));
        let name = plain.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with(".plain."), "got {name}");
    }

    #[test]
    fn failed_rename_removes_the_temp() {
        let dir = TempDir::new().unwrap();
        // Target is a non-empty directory: rename(file -> dir) fails.
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("child"), "x").unwrap();

        assert!(write_atomic(&target, b"data").is_err());

        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n != "target")
            .collect();
        assert!(leftovers.is_empty(), "temp leaked: {:?}", leftovers);
    }
}

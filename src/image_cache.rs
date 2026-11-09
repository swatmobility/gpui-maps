//! The story image cache's disk side (spec §4.3): one file per fetched
//! URL, named by a hash (so no URL and no secret reaches the file system),
//! written through a uniquely named temp file and an atomic `persist`, and
//! kept under [`MAX_IMAGE_CACHE_BYTES`] by a least-recently-used sweep
//! after every write.
//!
//! It lives outside `src/story` on purpose: recency is a file's mtime, and
//! touching it on a hit reads the wall clock, which `story_clock_scan`
//! bans in every story file. What it decides -- which file to delete
//! when the cache is over its cap -- never decides a frame.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing))]

use std::io::Write;
use std::path::Path;

/// The image cache's size cap (512 MiB).
pub const MAX_IMAGE_CACHE_BYTES: u64 = 512 * 1024 * 1024;

/// The cached bytes for `key` in `dir`, marking the entry used (its mtime
/// is set to now) so the sweep keeps it longest.
pub(crate) fn read(dir: &Path, key: &str) -> Option<Vec<u8>> {
    let path = dir.join(key);
    let bytes = std::fs::read(&path).ok()?;
    if let Ok(file) = std::fs::File::options().write(true).open(&path) {
        let _ = file.set_modified(std::time::SystemTime::now());
    }
    Some(bytes)
}

/// Store `bytes` as `key` in `dir`: a `NamedTempFile` in `dir` (a name of
/// its own, so two writers -- in one process or two -- never share one)
/// persisted over the entry in one rename, then a sweep to `cap`.
///
/// # Errors
/// The folder cannot be made or written.
pub(crate) fn write(dir: &Path, key: &str, bytes: &[u8], cap: u64) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut temp = tempfile::Builder::new().prefix(".partial-").tempfile_in(dir)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(dir.join(key)).map_err(|e| e.error)?;
    sweep(dir, cap);
    Ok(())
}

/// Drop `key` from `dir` (a copy that no longer decodes).
pub(crate) fn evict(dir: &Path, key: &str) {
    let _ = std::fs::remove_file(dir.join(key));
}

/// A temp file older than this is an orphan (its writer died) and is
/// deleted by the sweep.
const ORPHAN_AGE: std::time::Duration = std::time::Duration::from_secs(3600);

/// Delete the least recently used entries until the rest fit in `cap`.
/// Temp files (`.partial-*`) are not counted; one older than an hour is an
/// orphan and is deleted, a younger one may still be being written.
pub(crate) fn sweep(dir: &Path, cap: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let now = std::time::SystemTime::now();
    let entries: Vec<std::fs::DirEntry> = entries.filter_map(Result::ok).collect();
    for e in entries.iter().filter(|e| e.file_name().to_string_lossy().starts_with(".partial-")) {
        let old = e.metadata().and_then(|m| m.modified()).is_ok_and(|t| now.duration_since(t).is_ok_and(|age| age > ORPHAN_AGE));
        if old {
            let _ = std::fs::remove_file(e.path());
        }
    }
    let mut files: Vec<(std::time::SystemTime, u64, std::path::PathBuf)> = entries
        .iter()
        .filter(|e| !e.file_name().to_string_lossy().starts_with(".partial-"))
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            meta.is_file().then(|| (meta.modified().unwrap_or(std::time::UNIX_EPOCH), meta.len(), e.path()))
        })
        .collect();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    // Oldest first; ties by name, so the order never depends on the listing.
    files.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.2.cmp(&b.2)));
    for (_, len, path) in files {
        if total <= cap {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cache_keeps_the_most_recently_used_entries_under_its_cap() {
        let dir = tempfile::tempdir().unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        for (i, key) in ["a", "b", "c"].iter().enumerate() {
            write(dir.path(), key, &[0u8; 100], u64::MAX).unwrap();
            let f = std::fs::File::options().write(true).open(dir.path().join(key)).unwrap();
            f.set_modified(old + std::time::Duration::from_secs(i as u64 * 60)).unwrap();
        }
        // `a` is the oldest, but reading it makes it the newest.
        assert_eq!(read(dir.path(), "a").unwrap().len(), 100);
        write(dir.path(), "d", &[0u8; 100], 250).unwrap();
        let mut left: Vec<String> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, ["a", "d"], "b and c were least recently used");
        evict(dir.path(), "a");
        assert!(read(dir.path(), "a").is_none());
    }

    /// Review F8: an orphaned temp file (its writer died over an hour ago)
    /// is swept; a fresh one, perhaps still being written, is kept.
    #[test]
    fn the_sweep_removes_orphaned_temp_files_only() {
        let dir = tempfile::tempdir().unwrap();
        for name in [".partial-old", ".partial-new"] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        let f = std::fs::File::options().write(true).open(dir.path().join(".partial-old")).unwrap();
        f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(7200)).unwrap();
        sweep(dir.path(), u64::MAX);
        assert!(!dir.path().join(".partial-old").exists());
        assert!(dir.path().join(".partial-new").exists());
    }
}

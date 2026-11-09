//! Tile bodies on disk, so a relaunch draws the city it drew last time
//! without the network, and a commute's tiles are fetched once a week rather
//! than once a launch.
//!
//! One file per tile URL under the host's cache directory (iOS: the app's
//! `Library/Caches`, which the system may purge -- a cache, never data).
//! Files are named by a hash of the URL, not by z/x/y, so a directory
//! listing is not a map of where the user has been. Bounded by bytes: past
//! the cap the oldest files (by when they were fetched)
//! go until three quarters remain. Stale past `max_age`: a body that old
//! is fetched again (OpenFreeMap rebuilds its planet weekly).
//!
//! Called from the fetch workers; every method takes `&self` and the files
//! are written to a temporary name and renamed, so a reader never sees half
//! a tile.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

/// What a host gets by default: enough for a city at every zoom.
pub const DEFAULT_MAX_BYTES: u64 = 200 * 1024 * 1024;
/// OpenFreeMap's planet rebuild cadence.
pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);

#[derive(Debug)]
pub struct DiskCache {
    dir: PathBuf,
    max_bytes: u64,
    max_age: Duration,
    /// Bytes on disk, as last counted plus what was written since.
    bytes: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
}

/// FNV-1a, 64-bit: stable across runs and builds (std's hasher is not).
fn fnv1a(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

impl DiskCache {
    /// Opens (creating) the cache in `dir`.
    pub fn open(dir: impl AsRef<Path>, max_bytes: u64, max_age: Duration) -> std::io::Result<DiskCache> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;
        let cache =
            DiskCache { dir, max_bytes, max_age, bytes: AtomicU64::new(0), hits: AtomicU64::new(0), misses: AtomicU64::new(0) };
        cache.bytes.store(cache.entries().iter().map(|(_, len, _)| len).sum(), Ordering::Relaxed);
        Ok(cache)
    }

    fn path(&self, url: &str) -> PathBuf {
        self.dir.join(format!("{:016x}.tile", fnv1a(url)))
    }

    /// `url`'s body, when one is held and younger than `max_age`.
    pub fn get(&self, url: &str) -> Option<Vec<u8>> {
        let path = self.path(url);
        let fresh = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age < self.max_age);
        if !fresh {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let body = std::fs::read(&path).ok();
        if body.is_some() {
            self.hits.fetch_add(1, Ordering::Relaxed);
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
        }
        body
    }

    /// Stores `body` for `url`; past the byte cap, evicts the oldest files.
    /// Failure is silent: a cache that cannot write is a slower map, not a
    /// broken one.
    pub fn put(&self, url: &str, body: &[u8]) {
        let path = self.path(url);
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        if std::fs::write(&tmp, body).is_err() || std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        let total = self.bytes.fetch_add(body.len() as u64, Ordering::Relaxed) + body.len() as u64;
        if total > self.max_bytes {
            self.prune();
        }
    }

    /// Every cached file: path, length, modification time.
    fn entries(&self) -> Vec<(PathBuf, u64, SystemTime)> {
        let Ok(dir) = std::fs::read_dir(&self.dir) else { return Vec::new() };
        dir.filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == "tile"))
            .filter_map(|e| {
                let meta = e.metadata().ok()?;
                Some((e.path(), meta.len(), meta.modified().ok()?))
            })
            .collect()
    }

    /// Deletes the oldest files until three quarters of the cap remain.
    pub fn prune(&self) {
        let mut entries = self.entries();
        entries.sort_by_key(|(_, _, modified)| *modified);
        let mut total: u64 = entries.iter().map(|(_, len, _)| len).sum();
        let target = self.max_bytes / 4 * 3;
        for (path, len, _) in entries {
            if total <= target {
                break;
            }
            if std::fs::remove_file(&path).is_ok() {
                total -= len;
            }
        }
        self.bytes.store(total, Ordering::Relaxed);
    }

    /// `(bytes held, hits, misses)` since opening.
    pub fn stats(&self) -> (u64, u64, u64) {
        (self.bytes.load(Ordering::Relaxed), self.hits.load(Ordering::Relaxed), self.misses.load(Ordering::Relaxed))
    }
}

//! An opt-in, HTTP-semantics disk cache for [`super::http::HttpTileSource`]
//! ([`super::http::HttpTileSource::with_disk_cache`]).
//!
//! # Off unless the developer turns it on
//!
//! The engine never writes a tile to disk on its own. WHERE bytes may be
//! written, and WHETHER a provider's terms allow it at all, are the
//! application's decisions: OpenFreeMap, Natural Earth and Sentinel-2
//! cloudless send year- or week-long `max-age`s and invite caching, while
//! Mapbox's terms restrict how its tiles may be stored. **Caching a
//! provider's tiles is the developer's responsibility under that
//! provider's terms** -- this module only does what the response headers
//! say once it has been switched on.
//!
//! # What it honours
//!
//! A private (single-application) cache, RFC 9111 in the parts tiles use:
//!
//! - `Cache-Control: max-age=N` (less the response's `Age`), else
//!   `Expires - Date`, else 10% of `Date - Last-Modified` capped at a day
//!   (RFC 9111 §4.2.2's heuristic), is how long an entry is served without
//!   asking.
//! - `no-store` is never written, and drops any entry already held.
//! - `private` is never written either. Strictly a private cache may keep
//!   it, but this cache's key drops the URL's credentials (below), so two
//!   tokens share one entry -- and a `private` answer is the one a
//!   server says belongs to ONE requester.
//! - `no-cache` is written but revalidated on every use.
//! - A stale entry is revalidated with `If-None-Match` (its `ETag`) and
//!   `If-Modified-Since` (its `Last-Modified`). A `304` refreshes it and
//!   serves the held bytes; a `200` replaces it; a `404` drops it.
//! - A stale entry is served when the revalidation cannot reach the server,
//!   the server answers 5xx or 429, or a 200's body fails to arrive (RFC
//!   5861's `stale-if-error`, applied by default) -- for at most
//!   [`DiskCache::max_stale`] past its freshness (24 h by default), and
//!   never for an entry stored with `must-revalidate` or
//!   `proxy-revalidate`. A cached map keeps drawing through an outage.
//! - Lifetimes are capped at 2^31 s (68 years); a `max-age` or `Expires`
//!   past that, or an HTTP date outside 1970..=9999, cannot overflow.
//! - An answer with neither a lifetime nor a validator is not written:
//!   it could never be served without asking again in full.
//!
//! # Keys and the credential rule
//!
//! An entry's file is named by the SHA-256 of the tile URL after
//! [`crate::sprite::redact_url_credentials`] (an `access_token=`, `key=`,
//! `token=`, `password=` value or a `user:pass@` authority becomes
//! `[REDACTED]`). The file holds the validators, the times and the body --
//! never the URL. So no token reaches a path or a file, and a rotated
//! token keeps its cache.
//!
//! **Two consequences of that key** (review; no code change, by design):
//!
//! - Two tokens share one entry. That is right for a public tileset, where
//!   every token sees the same bytes. For a PRIVATE tileset (per-account
//!   data behind the token), isolation between tokens rests on the server
//!   answering `Cache-Control: private`, which is never written. **Do not
//!   enable the cache for a private tileset unless its server marks its
//!   tiles `private`.**
//! - The redaction matches any parameter ending in `key=` or `token=`
//!   (`monkey=`, `pagetoken=`): two URLs differing only in such a value
//!   collide on one key. Rare for tile URLs, but a template that varies a
//!   non-credential `...key=` parameter should not be cached.
//!
//! # Writes, eviction, other processes
//!
//! Every write goes to `.<key>.<pid>.<n>.tmp` beside the entry (created
//! with `create_new`, so it never writes through someone else's file) and
//! is renamed over it, so a reader -- this process or another sharing the
//! directory -- sees the old entry or the new one, never half of one.
//! Entries are evicted least-recently-used once the directory passes
//! `max_bytes`, down to 90% of it. One index per directory is shared by
//! every source in the process that names it; another process's writes
//! are counted the next time the directory is scanned (process start), so
//! two processes sharing one directory may hold it a little over budget
//! between scans. Leftover temp files older than an hour are swept on
//! that scan.
//!
//! Each entry's own read and write is `std::fs` inside the async fetch:
//! one local tile per call, and the fetch already runs off the frame
//! thread. The directory scan (once per directory per process) and the
//! eviction unlinks, which can touch thousands of files, run on
//! `spawn_blocking` and never under the index lock. An entry whose body
//! length does not match the length recorded beside it is a miss.
//!
//! Native only: the browser build has the browser's HTTP cache.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Where, and how much: the opt-in for
/// [`super::http::HttpTileSource::with_disk_cache`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskCache {
    /// Created on first use. Several sources may share one directory: their
    /// keys are their full (redacted) URLs, so they never collide. Give it a
    /// directory only this user can write.
    pub dir: PathBuf,
    /// The directory's budget, in bytes of entry files.
    pub max_bytes: u64,
    /// How long past its freshness an entry may still be served when the
    /// server cannot answer ([`DiskCache::DEFAULT_MAX_STALE`] from
    /// [`DiskCache::new`]). `Duration::ZERO` never serves stale.
    pub max_stale: Duration,
}

impl DiskCache {
    /// 24 hours.
    pub const DEFAULT_MAX_STALE: Duration = Duration::from_secs(24 * 60 * 60);

    /// `dir`, held to `max_bytes`, serving stale for at most
    /// [`Self::DEFAULT_MAX_STALE`].
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>, max_bytes: u64) -> Self {
        Self { dir: dir.into(), max_bytes, max_stale: Self::DEFAULT_MAX_STALE }
    }

    /// This cache with another stale-if-error bound.
    #[must_use]
    pub fn with_max_stale(mut self, max_stale: Duration) -> Self {
        self.max_stale = max_stale;
        self
    }
}

/// The longest lifetime an entry is given, whatever the server says: 2^31
/// seconds (68 years), far past any real `max-age` and far short of any
/// overflow.
pub const MAX_LIFETIME: Duration = Duration::from_secs(1 << 31);

/// The longest a `Last-Modified` heuristic may make an entry fresh.
const HEURISTIC_MAX: Duration = Duration::from_secs(24 * 60 * 60);
/// Temp files older than this are a crashed writer's, and are swept.
const STALE_TEMP: Duration = Duration::from_secs(60 * 60);
const MAGIC: &[u8; 6] = b"GMTC1\n";
const ENTRY_EXT: &str = "tile";

/// What a response's headers allow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Policy {
    /// `no-store` or `private`: never written, and an entry held is dropped.
    pub(crate) store: bool,
    /// How long from now the entry is served without asking.
    pub(crate) fresh_for: Duration,
    pub(crate) etag: Option<String>,
    pub(crate) last_modified: Option<String>,
    /// `must-revalidate` / `proxy-revalidate`: never served stale.
    pub(crate) must_revalidate: bool,
}

impl Policy {
    /// From a response's headers, `now` being when it arrived.
    pub(crate) fn from_headers(headers: &reqwest::header::HeaderMap, now: SystemTime) -> Self {
        let get = |name: reqwest::header::HeaderName| headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
        let cache_control = get(reqwest::header::CACHE_CONTROL).unwrap_or_default();
        let mut no_store = false;
        let mut no_cache = false;
        let mut must_revalidate = false;
        let mut max_age = None;
        for directive in cache_control.split(',') {
            let directive = directive.trim();
            let (name, value) = directive.split_once('=').map_or((directive, None), |(n, v)| (n.trim(), Some(v.trim().trim_matches('"'))));
            match name.to_ascii_lowercase().as_str() {
                "no-store" | "private" => no_store = true,
                "no-cache" => no_cache = true,
                "must-revalidate" | "proxy-revalidate" => must_revalidate = true,
                // Unparseable (or past u64) reads as the cap, not as absent.
                "max-age" => {
                    max_age = value.map(|v| {
                        if !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) {
                            v.parse().unwrap_or(u64::MAX)
                        } else {
                            0
                        }
                    });
                }
                _ => {}
            }
        }
        let date = get(reqwest::header::DATE).as_deref().and_then(parse_http_date);
        let last_modified = get(reqwest::header::LAST_MODIFIED);
        let lifetime = if no_cache {
            Duration::ZERO
        } else if let Some(secs) = max_age {
            Duration::from_secs(secs)
        } else if let Some(expires) = get(reqwest::header::EXPIRES) {
            // An unparseable `Expires` (often "0") means already expired.
            parse_http_date(&expires).and_then(|e| e.duration_since(date.unwrap_or(now)).ok()).unwrap_or(Duration::ZERO)
        } else if let (Some(date), Some(lm)) = (date, last_modified.as_deref().and_then(parse_http_date)) {
            date.duration_since(lm).map(|age| (age / 10).min(HEURISTIC_MAX)).unwrap_or(Duration::ZERO)
        } else {
            Duration::ZERO
        };
        let age = get(reqwest::header::AGE).and_then(|a| a.trim().parse::<u64>().ok()).map_or(Duration::ZERO, Duration::from_secs);
        Self {
            store: !no_store,
            fresh_for: lifetime.min(MAX_LIFETIME).saturating_sub(age),
            etag: get(reqwest::header::ETAG),
            last_modified,
            must_revalidate,
        }
    }

    /// Worth writing: allowed, and either servable for a while or
    /// revalidatable.
    fn worth_storing(&self) -> bool {
        self.store && (!self.fresh_for.is_zero() || self.etag.is_some() || self.last_modified.is_some())
    }
}

/// One entry as read from (or written to) its file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Meta {
    /// Unix seconds until which the entry is served without asking.
    fresh_until: u64,
    /// The lifetime it was given, reused when a `304` states none.
    lifetime: u64,
    etag: Option<String>,
    last_modified: Option<String>,
    #[serde(default)]
    must_revalidate: bool,
    /// The body's length, checked on read: a file cut short is a miss.
    body_len: u64,
}

pub(crate) struct Entry {
    meta: Meta,
    pub(crate) body: Vec<u8>,
}

impl Entry {
    pub(crate) fn is_fresh(&self, now: SystemTime) -> bool {
        unix_secs(now) < self.meta.fresh_until
    }

    /// The conditional headers that revalidate this entry.
    pub(crate) fn validators(&self) -> Vec<(reqwest::header::HeaderName, String)> {
        let mut out = Vec::new();
        if let Some(etag) = &self.meta.etag {
            out.push((reqwest::header::IF_NONE_MATCH, etag.clone()));
        }
        if let Some(lm) = &self.meta.last_modified {
            out.push((reqwest::header::IF_MODIFIED_SINCE, lm.clone()));
        }
        out
    }

    pub(crate) fn can_revalidate(&self) -> bool {
        self.meta.etag.is_some() || self.meta.last_modified.is_some()
    }

    /// Whether this (stale) entry may stand in for an answer the server
    /// could not give: not `must-revalidate`, and no more than `max_stale`
    /// past its freshness.
    pub(crate) fn can_serve_stale(&self, now: SystemTime, max_stale: Duration) -> bool {
        !self.meta.must_revalidate && unix_secs(now) <= self.meta.fresh_until.saturating_add(max_stale.as_secs())
    }
}

/// What this process knows of one directory.
#[derive(Default)]
struct Index {
    /// Key -> (file bytes, last use).
    entries: HashMap<String, (u64, SystemTime)>,
    total: u64,
}

impl Index {
    fn insert(&mut self, key: &str, bytes: u64, used: SystemTime) {
        if let Some((old, _)) = self.entries.insert(key.to_owned(), (bytes, used)) {
            self.total = self.total.saturating_sub(old);
        }
        self.total = self.total.saturating_add(bytes);
    }

    fn remove(&mut self, key: &str) {
        if let Some((old, _)) = self.entries.remove(key) {
            self.total = self.total.saturating_sub(old);
        }
    }
}

/// One directory's index and whether it has been scanned yet.
#[derive(Default)]
struct Shared {
    scanned: tokio::sync::OnceCell<()>,
    index: Mutex<Index>,
}

/// One index per directory, shared by every source in the process.
fn shared_for(dir: &Path) -> Arc<Shared> {
    static INDEXES: OnceLock<Mutex<HashMap<PathBuf, Arc<Shared>>>> = OnceLock::new();
    let mut map = INDEXES.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    map.entry(dir.to_path_buf()).or_default().clone()
}

/// A [`DiskCache`] at work: the store behind one source.
pub(crate) struct DiskCacheStore {
    config: DiskCache,
    shared: Arc<Shared>,
}

impl std::fmt::Debug for DiskCacheStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskCacheStore").field("config", &self.config).finish_non_exhaustive()
    }
}

impl DiskCacheStore {
    pub(crate) fn new(config: DiskCache) -> Self {
        let shared = shared_for(&config.dir);
        Self { config, shared }
    }

    pub(crate) fn max_stale(&self) -> Duration {
        self.config.max_stale
    }

    /// The key for `url`: SHA-256 of the URL with its credentials
    /// redacted, as hex. Never the URL itself.
    pub(crate) fn key_for(url: &str) -> String {
        use sha2::Digest;
        let redacted = crate::sprite::redact_url_credentials(url);
        let digest = sha2::Sha256::digest(redacted.as_bytes());
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn path_for(&self, key: &str) -> PathBuf {
        entry_path(&self.config.dir, key)
    }

    fn index(&self) -> std::sync::MutexGuard<'_, Index> {
        self.shared.index.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Counts what another run left in the directory, once per directory
    /// per process: the walk runs on `spawn_blocking`, and the index lock
    /// is taken only to merge its result.
    async fn ensure_scanned(&self) {
        self.shared
            .scanned
            .get_or_init(|| async {
                let dir = self.config.dir.clone();
                let found = tokio::task::spawn_blocking(move || scan(&dir)).await.unwrap_or_default();
                let mut index = self.index();
                for (key, bytes, used) in found {
                    // An entry this process already wrote is newer.
                    if !index.entries.contains_key(&key) {
                        index.insert(&key, bytes, used);
                    }
                }
            })
            .await;
    }

    /// The entry for `key`, if one is held and whole. Marks it used.
    pub(crate) async fn read(&self, key: &str) -> Option<Entry> {
        self.ensure_scanned().await;
        let path = self.path_for(key);
        let entry = std::fs::read(&path).ok().and_then(|bytes| decode(&bytes));
        let now = SystemTime::now();
        match &entry {
            Some(_) => {
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                self.index().insert(key, size, now);
                // The LRU order survives a restart through the file's mtime.
                if let Ok(file) = std::fs::File::open(&path) {
                    let _ = file.set_modified(now);
                }
            }
            None => {
                // Gone (another process evicted it), torn or corrupt: a miss.
                self.index().remove(key);
                let _ = std::fs::remove_file(&path);
            }
        }
        entry
    }

    /// Writes `body` under `key` as `policy` allows, or drops the entry
    /// when it allows none. `previous_lifetime`: a `304`'s fallback when it
    /// states no lifetime of its own.
    pub(crate) async fn write(&self, key: &str, body: &[u8], policy: &Policy, previous_lifetime: Option<Duration>) {
        if !policy.worth_storing() {
            self.remove(key).await;
            return;
        }
        self.ensure_scanned().await;
        let now = SystemTime::now();
        let lifetime = if policy.fresh_for.is_zero() { previous_lifetime.unwrap_or(Duration::ZERO) } else { policy.fresh_for };
        let lifetime = lifetime.min(MAX_LIFETIME);
        let meta = Meta {
            fresh_until: unix_secs(now).saturating_add(lifetime.as_secs()),
            lifetime: lifetime.as_secs(),
            etag: policy.etag.clone(),
            last_modified: policy.last_modified.clone(),
            must_revalidate: policy.must_revalidate,
            body_len: body.len() as u64,
        };
        let bytes = encode(&meta, body);
        let size = bytes.len() as u64;
        if size > self.config.max_bytes {
            self.remove(key).await;
            return;
        }
        if std::fs::create_dir_all(&self.config.dir).is_err() || !write_atomically(&self.config.dir, key, &bytes) {
            return;
        }
        let victims = {
            let mut index = self.index();
            index.insert(key, size, now);
            if index.total > self.config.max_bytes { choose_victims(&mut index, self.config.max_bytes, key) } else { Vec::new() }
        };
        if !victims.is_empty() {
            let dir = self.config.dir.clone();
            let _ = tokio::task::spawn_blocking(move || {
                for key in victims {
                    let _ = std::fs::remove_file(entry_path(&dir, &key));
                }
            })
            .await;
        }
    }

    /// Re-dates a held entry after a `304`.
    pub(crate) async fn refresh(&self, key: &str, entry: &Entry, policy: &Policy) {
        let merged = Policy {
            store: policy.store,
            fresh_for: policy.fresh_for,
            etag: policy.etag.clone().or_else(|| entry.meta.etag.clone()),
            last_modified: policy.last_modified.clone().or_else(|| entry.meta.last_modified.clone()),
            must_revalidate: policy.must_revalidate || entry.meta.must_revalidate,
        };
        self.write(key, &entry.body, &merged, Some(Duration::from_secs(entry.meta.lifetime))).await;
    }

    pub(crate) async fn remove(&self, key: &str) {
        self.index().remove(key);
        let _ = std::fs::remove_file(self.path_for(key));
    }
}

fn entry_path(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("{key}.{ENTRY_EXT}"))
}

/// Writes `bytes` to a fresh temp file (`create_new`: never through a file
/// that is already there) and renames it over `key`'s entry.
fn write_atomically(dir: &Path, key: &str, bytes: &[u8]) -> bool {
    use std::io::Write as _;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    for _ in 0..4 {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!(".{key}.{}.{n}.tmp", std::process::id()));
        let mut file = match std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(file) => file,
            // A crashed run's leftover with this pid and counter: next name.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return false,
        };
        let written = file.write_all(bytes).is_ok();
        drop(file);
        if written && std::fs::rename(&tmp, entry_path(dir, key)).is_ok() {
            return true;
        }
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    false
}

/// Least recently used first, down to 90% of `max_bytes`, sparing `keep`:
/// removed from the index here, under its lock; their files are unlinked by
/// the caller after the lock is released.
fn choose_victims(index: &mut Index, max_bytes: u64, keep: &str) -> Vec<String> {
    let target = max_bytes / 10 * 9;
    let mut order: Vec<(SystemTime, String)> =
        index.entries.iter().filter(|(k, _)| k.as_str() != keep).map(|(k, (_, used))| (*used, k.clone())).collect();
    order.sort();
    let mut victims = Vec::new();
    for (_, key) in order {
        if index.total <= target {
            break;
        }
        index.remove(&key);
        victims.push(key);
    }
    victims
}

/// What is in `dir` (another run's entries), sweeping dead temps.
fn scan(dir: &Path) -> Vec<(String, u64, SystemTime)> {
    let Ok(read) = std::fs::read_dir(dir) else { return Vec::new() };
    let now = SystemTime::now();
    let mut found = Vec::new();
    for item in read.flatten() {
        let Ok(meta) = item.metadata() else { continue };
        let name = item.file_name().to_string_lossy().into_owned();
        let modified = meta.modified().unwrap_or(UNIX_EPOCH);
        if name.starts_with('.') && name.ends_with(".tmp") {
            if now.duration_since(modified).is_ok_and(|age| age > STALE_TEMP) {
                let _ = std::fs::remove_file(item.path());
            }
            continue;
        }
        if let Some(key) = name.strip_suffix(&format!(".{ENTRY_EXT}")) {
            found.push((key.to_owned(), meta.len(), modified));
        }
    }
    found
}

fn encode(meta: &Meta, body: &[u8]) -> Vec<u8> {
    let meta = serde_json::to_vec(meta).unwrap_or_default();
    let mut out = Vec::with_capacity(MAGIC.len() + 4 + meta.len() + body.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(meta.len() as u32).to_le_bytes());
    out.extend_from_slice(&meta);
    out.extend_from_slice(body);
    out
}

fn decode(bytes: &[u8]) -> Option<Entry> {
    let rest = bytes.strip_prefix(MAGIC.as_slice())?;
    let len = u32::from_le_bytes(rest.get(..4)?.try_into().ok()?) as usize;
    let meta: Meta = serde_json::from_slice(rest.get(4..4usize.checked_add(len)?)?).ok()?;
    let body = &rest[4 + len..];
    (body.len() as u64 == meta.body_len).then(|| Entry { meta, body: body.to_vec() })
}

fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// An IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`), the one form RFC 9110
/// obliges a server to send. Anything else is `None`, which every caller
/// reads as "no date".
pub(crate) fn parse_http_date(text: &str) -> Option<SystemTime> {
    let mut parts = text.split_whitespace();
    let _weekday = parts.next()?;
    let day: u64 = parts.next()?.parse().ok()?;
    let month_name = parts.next()?;
    let month = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"]
        .iter()
        .position(|m| *m == month_name)? as u64
        + 1;
    let year: i64 = parts.next()?.parse().ok()?;
    let mut hms = parts.next()?.split(':').map(|p| p.parse::<u64>().ok());
    let (h, m, s) = (hms.next()??, hms.next()??, hms.next()??);
    if parts.next()? != "GMT" || !(1970..=9999).contains(&year) || !(1..=31).contains(&day) || h > 23 || m > 59 || s > 60 {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + (h * 3600 + m * 60 + s) as i64;
    UNIX_EPOCH.checked_add(Duration::from_secs(u64::try_from(secs).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue};

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn parses_an_imf_fixdate() {
        let t = parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT").unwrap();
        assert_eq!(unix_secs(t), 784_111_777);
        assert_eq!(unix_secs(parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT").unwrap()), 0);
        assert!(parse_http_date("0").is_none());
        assert!(parse_http_date("Fri, 31 Dec 9999 23:59:59 GMT").is_some());
        for absurd in ["Sat, 01 Jan 10000 00:00:00 GMT", "Fri, 31 Dec 1969 23:59:59 GMT", "Mon, 01 Jan 99999999999999999 00:00:00 GMT", "Mon, 01 Jan -5 00:00:00 GMT"] {
            assert!(parse_http_date(absurd).is_none(), "{absurd}");
        }
        assert!(parse_http_date("Sunday, 06-Nov-94 08:49:37 GMT").is_none());
    }

    #[test]
    fn the_policy_reads_what_tile_servers_send() {
        let now = SystemTime::now();
        // OpenFreeMap, as measured 2026-10-01.
        let p = Policy::from_headers(
            &headers(&[("cache-control", "public, max-age=315360000"), ("etag", "W/\"6a9d71e8-9fb48\""), ("age", "1654238")]),
            now,
        );
        assert!(p.store && p.fresh_for == Duration::from_secs(315_360_000 - 1_654_238));
        assert_eq!(p.etag.as_deref(), Some("W/\"6a9d71e8-9fb48\""));
        for cc in ["no-store", "private, max-age=60", "max-age=60, no-store"] {
            assert!(!Policy::from_headers(&headers(&[("cache-control", cc)]), now).store, "{cc}");
        }
        let p = Policy::from_headers(&headers(&[("cache-control", "no-cache, max-age=600"), ("etag", "\"x\"")]), now);
        assert!(p.store && p.fresh_for.is_zero() && p.worth_storing());
        // Expires - Date, and the Last-Modified heuristic capped at a day.
        let p = Policy::from_headers(
            &headers(&[("date", "Thu, 01 Oct 2026 01:25:03 GMT"), ("expires", "Thu, 08 Oct 2026 01:25:03 GMT")]),
            now,
        );
        assert_eq!(p.fresh_for, Duration::from_secs(7 * 86_400));
        let p = Policy::from_headers(
            &headers(&[("date", "Thu, 01 Oct 2026 01:25:03 GMT"), ("last-modified", "Tue, 01 Aug 2017 09:58:11 GMT")]),
            now,
        );
        assert_eq!(p.fresh_for, HEURISTIC_MAX);
        // Absurd lifetimes are capped, never overflowed.
        for h in [
            headers(&[("cache-control", &format!("max-age={}", u64::MAX))]),
            headers(&[("cache-control", "max-age=99999999999999999999999999")]),
            headers(&[("date", "Thu, 01 Oct 2026 01:25:03 GMT"), ("expires", "Fri, 31 Dec 9999 23:59:59 GMT")]),
            headers(&[("expires", "Sat, 01 Jan 10000 00:00:00 GMT")]),
        ] {
            let p = Policy::from_headers(&h, now);
            assert!(p.fresh_for <= MAX_LIFETIME, "{h:?}: {:?}", p.fresh_for);
        }
        let p = Policy::from_headers(&headers(&[("cache-control", "max-age=60, must-revalidate")]), now);
        assert!(p.must_revalidate);
        assert!(Policy::from_headers(&headers(&[("cache-control", "proxy-revalidate")]), now).must_revalidate);
        // Nothing to go on: not worth a file.
        assert!(!Policy::from_headers(&HeaderMap::new(), now).worth_storing());
    }

    #[test]
    fn an_entry_round_trips_and_a_torn_one_is_a_miss() {
        let meta = Meta { fresh_until: 5, lifetime: 7, etag: Some("\"e\"".into()), last_modified: None, must_revalidate: false, body_len: 4 };
        let bytes = encode(&meta, b"body");
        let back = decode(&bytes).unwrap();
        assert_eq!((back.meta, back.body), (meta, b"body".to_vec()));
        assert!(decode(&bytes[..8]).is_none());
        assert!(decode(&bytes[..bytes.len() - 1]).is_none(), "a body cut short is a miss");
        let mut long = bytes.clone();
        long.push(0);
        assert!(decode(&long).is_none(), "a body with bytes appended is a miss");
        assert!(decode(b"not a cache entry").is_none());
    }

    #[test]
    fn the_key_drops_credentials_and_is_never_the_url() {
        let a = DiskCacheStore::key_for("https://api.example/v4/t/1/2/3.mvt?access_token=SECRET_A");
        let b = DiskCacheStore::key_for("https://api.example/v4/t/1/2/3.mvt?access_token=SECRET_B");
        let c = DiskCacheStore::key_for("https://api.example/v4/t/1/2/4.mvt?access_token=SECRET_A");
        assert_eq!(a, b, "a rotated token keeps its cache");
        assert_ne!(a, c);
        assert!(a.len() == 64 && a.chars().all(|ch| ch.is_ascii_hexdigit()));
    }
}

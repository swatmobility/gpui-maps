//! An HTTP cache on disk for catalog documents, revalidated with ETag and
//! Last-Modified.
//!
//! The crate has no HTTP client: a host hands in a [`Transport`] (blocking,
//! one request per call). [`HttpCache::get`] answers, in order:
//!
//! 1. the memory copy, then the disk copy, while younger than `max_age`
//!    ([`Origin::Fresh`]);
//! 2. a conditional request (`If-None-Match` / `If-Modified-Since` from
//!    the stored copy): `304` refreshes the copy ([`Origin::Revalidated`]),
//!    `200` replaces it ([`Origin::Network`]);
//! 3. on a network failure (or with no transport: offline), the stored copy
//!    however old ([`Origin::Stale`]), then the recorded pack
//!    ([`Origin::Pack`]).
//!
//! At most [`MAX_CONCURRENT`] requests are in flight at once, over every
//! thread sharing the cache.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Requests in flight at once, per cache.
pub const MAX_CONCURRENT: usize = 4;

/// A conditional request's validators.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Conditional {
    pub if_none_match: Option<String>,
    pub if_modified_since: Option<String>,
}

/// What a transport answered.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub body: Vec<u8>,
}

/// One blocking GET. The host's implementation sets the User-Agent and the
/// timeouts; it never follows a redirect to another host silently.
pub trait Transport: Send + Sync {
    fn get(&self, url: &str, conditional: &Conditional) -> Result<Response, String>;
}

/// Where an answer came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Fresh,
    Revalidated,
    Network,
    /// The network failed (or there is none); an older copy answered.
    Stale,
    Pack,
}

impl Origin {
    /// Whether the answer was checked against the server just now.
    pub fn is_live(self) -> bool {
        matches!(self, Origin::Fresh | Origin::Revalidated | Origin::Network)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    pub bytes: Arc<Vec<u8>>,
    pub origin: Origin,
}

/// A stored copy's validators.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct Meta {
    url: String,
    etag: Option<String>,
    last_modified: Option<String>,
    /// Seconds since the epoch when it was last confirmed.
    checked_s: u64,
}

/// Answers from a recorded pack, by URL.
pub type PackLookup = Arc<dyn Fn(&str) -> Option<Vec<u8>> + Send + Sync>;

/// The in-memory layer: a URL's metadata and bytes.
type MemEntries = Mutex<HashMap<String, (Meta, Arc<Vec<u8>>)>>;

pub struct HttpCache {
    dir: Option<PathBuf>,
    transport: Option<Arc<dyn Transport>>,
    pack: Option<PackLookup>,
    max_age: Duration,
    mem: MemEntries,
    in_flight: (Mutex<usize>, Condvar),
    /// Requests sent (for tests and the status chip).
    sent: Mutex<usize>,
    /// Hosts the transport may reach; empty is any.
    allowed_hosts: Vec<String>,
}

impl HttpCache {
    /// `dir`: where copies are kept (`None`: memory only). `transport`:
    /// `None` is offline.
    pub fn new(dir: Option<PathBuf>, transport: Option<Arc<dyn Transport>>, max_age: Duration) -> HttpCache {
        HttpCache { dir, transport, pack: None, max_age, mem: Mutex::new(HashMap::new()), in_flight: (Mutex::new(0), Condvar::new()), sent: Mutex::new(0), allowed_hosts: Vec::new() }
    }

    /// Answers from a recorded pack when nothing else can.
    pub fn with_pack(mut self, pack: PackLookup) -> HttpCache {
        self.pack = Some(pack);
        self
    }

    /// Only these hosts reach the transport; any other URL is answered
    /// from the copies and the pack only.
    pub fn with_allowed_hosts(mut self, hosts: &[&str]) -> HttpCache {
        self.allowed_hosts = hosts.iter().map(|h| h.to_ascii_lowercase()).collect();
        self
    }

    pub fn is_online(&self) -> bool {
        self.transport.is_some()
    }

    pub fn requests_sent(&self) -> usize {
        *self.sent.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn may_reach(&self, url: &str) -> bool {
        if self.allowed_hosts.is_empty() {
            return true;
        }
        crate::url::host_of(url).is_some_and(|h| self.allowed_hosts.contains(&h))
    }

    /// The document at `url`. A path (no scheme) is read from disk.
    pub fn get(&self, url: &str) -> Result<Fetched, String> {
        if !url.contains("://") {
            return std::fs::read(url).map(|b| Fetched { bytes: Arc::new(b), origin: Origin::Fresh }).map_err(|e| format!("{url}: {e}"));
        }
        let now = now_s();
        let stored = self.stored(url);
        if let Some((meta, bytes)) = &stored {
            if now.saturating_sub(meta.checked_s) < self.max_age.as_secs() {
                return Ok(Fetched { bytes: bytes.clone(), origin: Origin::Fresh });
            }
        }
        let net_error = match (&self.transport, self.may_reach(url)) {
            (Some(t), true) => {
                let cond = stored.as_ref().map(|(m, _)| Conditional { if_none_match: m.etag.clone(), if_modified_since: m.last_modified.clone() }).unwrap_or_default();
                let answer = self.limited(|| t.get(url, &cond));
                match answer {
                    Ok(r) if r.status == 304 && stored.is_some() => {
                        let (mut meta, bytes) = stored.clone().expect("checked");
                        meta.checked_s = now;
                        if r.etag.is_some() {
                            meta.etag = r.etag;
                        }
                        self.store(url, meta, bytes.clone(), false);
                        return Ok(Fetched { bytes, origin: Origin::Revalidated });
                    }
                    Ok(r) if (200..300).contains(&r.status) => {
                        let bytes = Arc::new(r.body);
                        let meta = Meta { url: url.to_string(), etag: r.etag, last_modified: r.last_modified, checked_s: now };
                        self.store(url, meta, bytes.clone(), true);
                        return Ok(Fetched { bytes, origin: Origin::Network });
                    }
                    Ok(r) => format!("HTTP {}", r.status),
                    Err(e) => e,
                }
            }
            (Some(_), false) => "host not allowed".to_string(),
            (None, _) => "offline".to_string(),
        };
        if let Some((_, bytes)) = stored {
            return Ok(Fetched { bytes, origin: Origin::Stale });
        }
        if let Some(b) = self.pack.as_ref().and_then(|p| p(url)) {
            return Ok(Fetched { bytes: Arc::new(b), origin: Origin::Pack });
        }
        Err(format!("{url}: {net_error}"))
    }

    fn limited<T>(&self, f: impl FnOnce() -> T) -> T {
        let (lock, cv) = &self.in_flight;
        {
            let mut n = lock.lock().unwrap_or_else(|e| e.into_inner());
            while *n >= MAX_CONCURRENT {
                n = cv.wait(n).unwrap_or_else(|e| e.into_inner());
            }
            *n += 1;
        }
        *self.sent.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        let out = f();
        {
            let mut n = lock.lock().unwrap_or_else(|e| e.into_inner());
            *n -= 1;
        }
        cv.notify_one();
        out
    }

    fn stored(&self, url: &str) -> Option<(Meta, Arc<Vec<u8>>)> {
        if let Some(hit) = self.mem.lock().unwrap_or_else(|e| e.into_inner()).get(url) {
            return Some(hit.clone());
        }
        let dir = self.dir.as_ref()?;
        let (body, meta) = paths(dir, url);
        let meta: Meta = serde_json::from_slice(&std::fs::read(meta).ok()?).ok()?;
        if meta.url != url {
            return None; // a hash collision
        }
        let bytes = Arc::new(std::fs::read(body).ok()?);
        self.mem.lock().unwrap_or_else(|e| e.into_inner()).insert(url.to_string(), (meta.clone(), bytes.clone()));
        Some((meta, bytes))
    }

    fn store(&self, url: &str, meta: Meta, bytes: Arc<Vec<u8>>, body_changed: bool) {
        if let Some(dir) = &self.dir {
            let (body, meta_path) = paths(dir, url);
            let _ = std::fs::create_dir_all(dir);
            if body_changed {
                let _ = write_atomic(&body, &bytes);
            }
            if let Ok(m) = serde_json::to_vec(&meta) {
                let _ = write_atomic(&meta_path, &m);
            }
        }
        self.mem.lock().unwrap_or_else(|e| e.into_inner()).insert(url.to_string(), (meta, bytes));
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(tmp, path)
}

/// `(body, meta)` file paths of `url` under `dir`.
fn paths(dir: &Path, url: &str) -> (PathBuf, PathBuf) {
    let h = fnv1a(url.as_bytes());
    (dir.join(format!("{h:016x}.body")), dir.join(format!("{h:016x}.meta.json")))
}

/// FNV-1a 64: stable across Rust releases (std's hasher is not).
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn now_s() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A server with one document and an ETag.
    struct Server {
        etag: Mutex<String>,
        body: Mutex<Vec<u8>>,
        hits: AtomicUsize,
        conditional_hits: AtomicUsize,
        down: std::sync::atomic::AtomicBool,
    }

    impl Transport for Server {
        fn get(&self, _url: &str, c: &Conditional) -> Result<Response, String> {
            if self.down.load(Ordering::SeqCst) {
                return Err("connection refused".into());
            }
            self.hits.fetch_add(1, Ordering::SeqCst);
            let etag = self.etag.lock().unwrap().clone();
            if c.if_none_match.as_deref() == Some(etag.as_str()) {
                self.conditional_hits.fetch_add(1, Ordering::SeqCst);
                return Ok(Response { status: 304, etag: Some(etag), ..Default::default() });
            }
            Ok(Response { status: 200, etag: Some(etag), last_modified: Some("Sat, 01 Aug 2026 20:56:42 GMT".into()), body: self.body.lock().unwrap().clone() })
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("gpui-map-stac-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    const URL: &str = "https://data.source.coop/cholmes/portolan-nl/catalog.json";

    fn server() -> Arc<Server> {
        Arc::new(Server { etag: Mutex::new("\"v1\"".into()), body: Mutex::new(b"{\"v\":1}".to_vec()), hits: AtomicUsize::new(0), conditional_hits: AtomicUsize::new(0), down: Default::default() })
    }

    #[test]
    fn a_stale_copy_is_revalidated_with_its_etag_and_a_304_keeps_it() {
        let dir = scratch("etag");
        let s = server();
        let cache = HttpCache::new(Some(dir.clone()), Some(s.clone()), Duration::ZERO);
        let a = cache.get(URL).unwrap();
        assert_eq!(a.origin, Origin::Network);
        // A second cache over the same dir: the copy is on disk.
        let cache2 = HttpCache::new(Some(dir.clone()), Some(s.clone()), Duration::ZERO);
        let b = cache2.get(URL).unwrap();
        assert_eq!(b.origin, Origin::Revalidated);
        assert_eq!(b.bytes, a.bytes);
        assert_eq!(s.conditional_hits.load(Ordering::SeqCst), 1);
        // The server changes: a 200 replaces the copy.
        *s.etag.lock().unwrap() = "\"v2\"".into();
        *s.body.lock().unwrap() = b"{\"v\":2}".to_vec();
        let c = cache2.get(URL).unwrap();
        assert_eq!((c.origin, c.bytes.as_slice()), (Origin::Network, &b"{\"v\":2}"[..]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_fresh_copy_sends_nothing_and_a_failure_serves_the_old_copy_then_the_pack() {
        let dir = scratch("fresh");
        let s = server();
        let cache = HttpCache::new(Some(dir.clone()), Some(s.clone()), Duration::from_secs(3600));
        cache.get(URL).unwrap();
        assert_eq!(cache.get(URL).unwrap().origin, Origin::Fresh);
        assert_eq!(s.hits.load(Ordering::SeqCst), 1);

        let stale = HttpCache::new(Some(dir.clone()), Some(s.clone()), Duration::ZERO);
        s.down.store(true, Ordering::SeqCst);
        assert_eq!(stale.get(URL).unwrap().origin, Origin::Stale);

        let packed = HttpCache::new(None, None, Duration::ZERO).with_pack(Arc::new(|u: &str| (u == URL).then(|| b"{\"pack\":1}".to_vec())));
        let p = packed.get(URL).unwrap();
        assert_eq!(p.origin, Origin::Pack);
        assert!(packed.get("https://data.source.coop/elsewhere.json").unwrap_err().contains("offline"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_host_outside_the_allowlist_never_reaches_the_transport() {
        let s = server();
        let cache = HttpCache::new(None, Some(s.clone()), Duration::ZERO).with_allowed_hosts(&["data.source.coop"]);
        assert!(cache.get("https://example.org/catalog.json").unwrap_err().contains("host not allowed"));
        assert_eq!(s.hits.load(Ordering::SeqCst), 0);
        cache.get(URL).unwrap();
        assert_eq!(s.hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn no_more_than_four_requests_are_in_flight() {
        struct Slow {
            now: AtomicUsize,
            peak: AtomicUsize,
        }
        impl Transport for Slow {
            fn get(&self, _: &str, _: &Conditional) -> Result<Response, String> {
                let n = self.now.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(n, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(20));
                self.now.fetch_sub(1, Ordering::SeqCst);
                Ok(Response { status: 200, body: b"{}".to_vec(), ..Default::default() })
            }
        }
        let t = Arc::new(Slow { now: AtomicUsize::new(0), peak: AtomicUsize::new(0) });
        let cache = Arc::new(HttpCache::new(None, Some(t.clone()), Duration::ZERO));
        let threads: Vec<_> = (0..12)
            .map(|i| {
                let c = cache.clone();
                std::thread::spawn(move || c.get(&format!("https://h.org/{i}.json")).unwrap())
            })
            .collect();
        for th in threads {
            th.join().unwrap();
        }
        assert!(t.peak.load(Ordering::SeqCst) <= MAX_CONCURRENT, "peak {}", t.peak.load(Ordering::SeqCst));
        assert_eq!(cache.requests_sent(), 12);
    }
}

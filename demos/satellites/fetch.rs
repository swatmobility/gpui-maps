//! CelesTrak GP data: one polite download per group per two hours, cached
//! on disk (spec 2026-09-29-satellites-design.md R16).
//!
//! CelesTrak asks its users to be gentle: its GP data updates about every
//! two hours, it answers `403` to a client that asks again before the data
//! changed, and it blocks an address that keeps hammering it. So:
//!
//! - **One request per group per [`MIN_INTERVAL_S`].** A success or a
//!   failure both push `next_allowed_unix` two hours out; nothing retries
//!   inside that window, whatever the outcome.
//! - **Stop on anything but a 200.** The cached body stays as it was; the
//!   status and the first 120 characters of the answer are kept in the
//!   meta for the status line.
//! - **Latch after [`LATCH_AFTER`] errors in a row.** No request is made
//!   again until the user asks ([`Fetcher::retry`]), and even then the two
//!   hour rule stands.
//! - **One fetcher at a time**, per cache directory: a `.lock` file holding
//!   its creation time, broken when older than [`STALE_LOCK_S`].
//! - **Atomic writes.** The body and the meta are written to `*.tmp`,
//!   `fsync`ed and renamed, so a crash never leaves half a file.
//!
//! No retry, no parallel requests, and tests never contact CelesTrak (they
//! run against a local stub). Native only: the browser shell has no file
//! system and its own fetch. The request is reqwest's on the desktop and
//! `ureq`'s under `celestrak_ureq` (the iOS Studio app, `studio-mobile`,
//! which includes this file by `#[path]`); the policy is the same file.
//!
//! Data: US Space Force GP element sets, via CelesTrak
//! (<https://celestrak.org>). Credit "Orbits: CelesTrak / US Space Force".
use std::io::Write as _;
use std::path::{Path, PathBuf};

use gpui_maps::orbit::omm::{parse_omm, MAX_BODY_BYTES};

/// The CelesTrak groups the demo may download. `starlink` is deliberately
/// not one of them (it is inside `active`).
pub const GROUPS: [&str; 8] = ["active", "stations", "gps-ops", "geo", "galileo", "glo-ops", "beidou", SATCAT];
/// The SATCAT (catalogue) records of the active objects -- each object's
/// OWNER code, for colour-by-country. A separate CelesTrak endpoint
/// ([`satcat_url`]) under the same fetch policy, cached as `satcat.json`.
pub const SATCAT: &str = "satcat";
/// Whether `group` is one of [`GROUPS`]. Every [`Fetcher`] entry point
/// checks it before a group name reaches a cache path or a URL, so a name
/// like `../x` can neither read nor write outside the cache directory.
#[must_use]
pub fn is_known_group(group: &str) -> bool {
    GROUPS.contains(&group)
}

/// The SATCAT records URL beside a GP `base_url`
/// (`.../NORAD/elements/gp.php` -> `.../satcat/records.php`), for the
/// active objects as JSON. A base that is not CelesTrak's (a test stub)
/// is asked on the same path with the same query.
#[must_use]
pub fn satcat_url(base_url: &str) -> String {
    let base = base_url.replace("/NORAD/elements/gp.php", "/satcat/records.php");
    format!("{base}?GROUP=active&FORMAT=json")
}

/// A fetched body's `(usable records, skipped)`, or why it is refused:
/// OMM JSON for the GP groups, SATCAT JSON for [`SATCAT`].
fn validate(group: &str, body: &[u8]) -> Result<(usize, usize), String> {
    if group == SATCAT {
        return super::model::parse_satcat(body).map(|owners| (owners.len(), 0)).map_err(|e| format!("not SATCAT JSON: {e}"));
    }
    parse_omm(body).map(|r| (r.records.len(), r.skipped)).map_err(|e| format!("not OMM JSON: {e:?}"))
}

/// The error a [`Fetcher`] entry point gives for a group not in [`GROUPS`].
fn unknown_group(group: &str) -> String {
    format!("unknown group {group:?} (not one of {GROUPS:?})")
}
/// Seconds between two requests for one group, whatever the last outcome.
pub const MIN_INTERVAL_S: f64 = 7_200.0;
/// The SATCAT's own minimum interval: 24 h. Owners change rarely and the
/// body is ~5.7 MB.
pub const SATCAT_MIN_INTERVAL_S: f64 = 86_400.0;

/// A group's minimum refetch interval: [`SATCAT_MIN_INTERVAL_S`] for the
/// SATCAT, [`MIN_INTERVAL_S`] for every GP group.
#[must_use]
pub fn min_interval_s(group: &str) -> f64 {
    if group == SATCAT { SATCAT_MIN_INTERVAL_S } else { MIN_INTERVAL_S }
}
/// Errors in a row after which no request is made until [`Fetcher::retry`].
pub const LATCH_AFTER: u32 = 3;
/// A `.lock` older than this is a crashed fetcher's and is broken.
pub const STALE_LOCK_S: f64 = 120.0;
/// The whole request, connect to last byte.
pub const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

const BASE_URL: &str = "https://celestrak.org/NORAD/elements/gp.php";

/// The version the User-Agent names: the engine crate's.
#[cfg(not(feature = "celestrak_ureq"))]
const UA_VERSION: &str = gpui_maps::agent::CRATE_VERSION;
#[cfg(feature = "celestrak_ureq")]
const UA_VERSION: &str = env!("CARGO_PKG_VERSION");

/// What the cache knows about one group: `{group}.meta.json`.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Meta {
    /// When the cached body was fetched (or seeded), Unix seconds.
    pub fetched_unix: Option<f64>,
    /// The last answer's HTTP status; `None` for a transport error or a
    /// seeded body.
    pub status: Option<u16>,
    /// The cached body's size.
    pub bytes: u64,
    /// Records `parse_omm` accepted in the cached body.
    pub records: usize,
    /// No request before this, Unix seconds.
    pub next_allowed_unix: f64,
    pub consecutive_errors: u32,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// A 200 with a valid OMM body, now cached.
    Fetched { records: usize, bytes: u64 },
    /// Inside the two hour window: no request made.
    NotDue { next_allowed_unix: f64 },
    /// CelesTrak answered, but not with a 200.
    Refused { status: u16, body_head: String },
    /// A transport error, a timeout, or a 200 that is not OMM JSON.
    Failed(String),
    /// [`LATCH_AFTER`] errors in a row: no request until [`Fetcher::retry`].
    Latched,
    /// Another fetcher holds the cache directory's lock.
    Locked,
}

pub struct Fetcher {
    pub cache_dir: PathBuf,
    pub base_url: String,
    pub timeout: std::time::Duration,
    pub user_agent: String,
}

/// `$HOME/.cache/gpui-map-data/satellites` (the system temp dir without a
/// `HOME`). Never inside the repository.
pub fn default_cache_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map_or_else(std::env::temp_dir, PathBuf::from)
        .join(".cache/gpui-map-data/satellites")
}

/// `*.tmp`, `fsync`, rename: a reader sees the old file or the new one.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// The first 120 characters of `body`, control characters removed.
fn head_of(body: &[u8]) -> String {
    String::from_utf8_lossy(body).chars().filter(|c| !c.is_control()).take(120).collect()
}

/// Removes the `.lock` on drop, on every path out of `refresh`.
struct LockGuard(PathBuf);

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The request's result, before it is folded into the meta.
enum Answer {
    Ok(Vec<u8>),
    Status(u16, Vec<u8>),
    Transport(String),
}

impl Fetcher {
    /// The real CelesTrak GP endpoint, [`TIMEOUT`], and our User-Agent.
    pub fn celestrak(cache_dir: PathBuf) -> Self {
        Self {
            cache_dir,
            base_url: BASE_URL.to_string(),
            timeout: TIMEOUT,
            user_agent: format!("gpui-map-satellites/{UA_VERSION} (+https://github.com/swatmobility/gpui-maps)"),
        }
    }

    /// `{group}.json` in the cache; `None` for a group not in [`GROUPS`].
    fn body_path(&self, group: &str) -> Option<PathBuf> {
        is_known_group(group).then(|| self.cache_dir.join(format!("{group}.json")))
    }

    /// `{group}.meta.json` in the cache; `None` for a group not in [`GROUPS`].
    fn meta_path(&self, group: &str) -> Option<PathBuf> {
        is_known_group(group).then(|| self.cache_dir.join(format!("{group}.meta.json")))
    }

    /// The group's meta; the default (never fetched) when there is none or
    /// it does not parse, or the group is not one of [`GROUPS`].
    pub fn meta(&self, group: &str) -> Meta {
        self.meta_path(group).and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    fn write_meta(&self, group: &str, meta: &Meta) -> std::io::Result<()> {
        let json = serde_json::to_vec_pretty(meta).expect("Meta serialises");
        let path = self.meta_path(group).ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, unknown_group(group)))?;
        write_atomic(&path, &json)
    }

    /// [`Self::write_meta`], logging a failure (the meta was already
    /// persisted once before the request, so the backoff survives it).
    fn write_meta_logged(&self, group: &str, meta: &Meta) {
        if let Err(e) = self.write_meta(group, meta) {
            eprintln!("satellites: could not write {group} meta: {e}");
        }
    }

    /// The cached body, as last fetched or seeded; `None` for a group not
    /// in [`GROUPS`].
    pub fn cached(&self, group: &str) -> Option<Vec<u8>> {
        std::fs::read(self.body_path(group)?).ok()
    }

    /// Clears the latch only: the next [`Self::refresh`] may request again,
    /// but not before `next_allowed_unix`.
    pub fn retry(&self, group: &str) {
        let mut meta = self.meta(group);
        if meta.consecutive_errors > 0 {
            meta.consecutive_errors = 0;
            self.write_meta_logged(group, &meta);
        }
    }

    /// Records `body` as fetched at `fetched_unix` without a request (a
    /// body fetched earlier by other means): the two hour rule then runs
    /// from `fetched_unix`. The body must be OMM JSON with records.
    ///
    /// Never rolls the cache back: under the lock, a no-op (with a message)
    /// when the cache already holds a body fetched at or after
    /// `fetched_unix`, or when the group has errors on record (seeding
    /// never clears the error stop); `next_allowed_unix` only ever moves
    /// later. `WouldBlock` when another fetcher holds the lock.
    pub fn seed(&self, group: &str, body: &[u8], fetched_unix: f64) -> std::io::Result<()> {
        let Some(body_path) = self.body_path(group) else {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, unknown_group(group)));
        };
        let report = parse_omm(body).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("not OMM JSON: {e:?}")))?;
        if report.records.is_empty() {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "no usable records"));
        }
        std::fs::create_dir_all(&self.cache_dir)?;
        let Some(_lock) = self.lock(wall_unix()) else {
            return Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, "another fetcher holds the cache lock"));
        };
        let existing = self.meta(group);
        if existing.fetched_unix.is_some_and(|f| f >= fetched_unix) {
            eprintln!("satellites: not seeding {group}: the cache is from {:?}, not older than the seed ({fetched_unix})", existing.fetched_unix);
            return Ok(());
        }
        if existing.consecutive_errors > 0 {
            eprintln!("satellites: not seeding {group}: {} errors on record; retry first", existing.consecutive_errors);
            return Ok(());
        }
        write_atomic(&body_path, body)?;
        self.write_meta(
            group,
            &Meta {
                fetched_unix: Some(fetched_unix),
                status: None,
                bytes: body.len() as u64,
                records: report.records.len(),
                next_allowed_unix: existing.next_allowed_unix.max(fetched_unix + min_interval_s(group)),
                consecutive_errors: 0,
                last_error: None,
            },
        )
    }

    /// Takes the cache directory's lock, breaking a stale one once.
    ///
    /// The lock holds `now_unix` as text. Its age is `now_unix` minus that
    /// stamp, or, when the stamp does not parse (a crash between creating
    /// and stamping it), the file's own mtime age by the wall clock.
    ///
    /// A stale lock is claimed by an atomic rename to a name of our own, so
    /// of several processes breaking it at once only one wins; the winner
    /// re-checks that what it claimed really was stale (a live lock created
    /// in between is put back with `hard_link`, which never overwrites) and
    /// then competes for a fresh `create_new` like anyone else.
    fn lock(&self, now_unix: f64) -> Option<LockGuard> {
        let path = self.cache_dir.join(".lock");
        let create = || std::fs::OpenOptions::new().write(true).create_new(true).open(&path);
        let file = match create() {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let age = lock_age_s(&path, now_unix)?;
                if age <= STALE_LOCK_S {
                    return None;
                }
                let claimed = self.cache_dir.join(format!(".lock.stale-{}-{:?}", std::process::id(), std::thread::current().id()));
                std::fs::rename(&path, &claimed).ok()?;
                if lock_age_s(&claimed, now_unix).is_none_or(|a| a <= STALE_LOCK_S) {
                    // We took a live lock someone made after our look.
                    let _ = std::fs::hard_link(&claimed, &path);
                    let _ = std::fs::remove_file(&claimed);
                    return None;
                }
                let _ = std::fs::remove_file(&claimed);
                eprintln!("satellites: broke a stale lock from {age:.0} s ago");
                create().ok()?
            }
            Err(e) => {
                eprintln!("satellites: cannot lock {}: {e}", path.display());
                return None;
            }
        };
        let guard = LockGuard(path);
        let mut file = file;
        let _ = write!(file, "{now_unix}");
        let _ = file.sync_all();
        Some(guard)
    }

    /// The request's URL for `group`.
    fn url(&self, group: &str) -> String {
        if group == SATCAT { satcat_url(&self.base_url) } else { format!("{}?GROUP={group}&FORMAT=json", self.base_url) }
    }

    /// GET `{base_url}?GROUP={group}&FORMAT=json`, once, no retry, on
    /// `ureq` (the phone has no tokio runtime): the same caps as the
    /// reqwest request below.
    #[cfg(feature = "celestrak_ureq")]
    fn request(&self, group: &str) -> Answer {
        use std::io::Read as _;
        let mut config = ureq::Agent::config_builder()
            .timeout_global(Some(self.timeout))
            .user_agent(self.user_agent.as_str())
            .http_status_as_error(false);
        if self.base_url.starts_with("http://127.0.0.1") {
            // The tests' local stub: never through an HTTP(S)_PROXY.
            config = config.proxy(None);
        }
        let agent = ureq::Agent::new_with_config(config.build());
        let describe = |e: ureq::Error| if matches!(e, ureq::Error::Timeout(_)) { format!("timed out: {e}") } else { e.to_string() };
        let mut response = match agent.get(&self.url(group)).call() {
            Ok(r) => r,
            Err(e) => return Answer::Transport(describe(e)),
        };
        let status = response.status().as_u16();
        let cap = if status == 200 { MAX_BODY_BYTES as u64 + 1 } else { 4096 };
        let mut body = Vec::new();
        if let Err(e) = response.body_mut().with_config().limit(cap).reader().read_to_end(&mut body) {
            // The limit is the cap, not an error: a body past it is kept to
            // the cap, as the reqwest reader keeps it.
            if body.len() as u64 != cap {
                return Answer::Transport(e.to_string());
            }
        }
        if status == 200 { Answer::Ok(body) } else { Answer::Status(status, body) }
    }

    /// GET `{base_url}?GROUP={group}&FORMAT=json`, once, no retry.
    #[cfg(not(feature = "celestrak_ureq"))]
    fn request(&self, group: &str) -> Answer {
        let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(r) => r,
            Err(e) => return Answer::Transport(format!("no runtime: {e}")),
        };
        runtime.block_on(async {
            let mut builder = reqwest::Client::builder().timeout(self.timeout).user_agent(&self.user_agent);
            if self.base_url.starts_with("http://127.0.0.1") {
                // The tests' local stub: never through an HTTP(S)_PROXY.
                builder = builder.no_proxy();
            }
            let client = match builder.build() {
                Ok(c) => c,
                Err(e) => return Answer::Transport(e.to_string()),
            };
            let describe = |e: reqwest::Error| if e.is_timeout() { format!("timed out: {e}") } else { e.to_string() };
            let url = self.url(group);
            let mut response = match client.get(&url).send().await {
                Ok(r) => r,
                Err(e) => return Answer::Transport(describe(e)),
            };
            let status = response.status().as_u16();
            // A 200 is read up to one byte past the parse cap (so an over-cap
            // body is refused by `parse_omm`, not truncated into a valid
            // one); anything else only as far as the head the meta keeps.
            let cap = if status == 200 { MAX_BODY_BYTES as usize + 1 } else { 4096 };
            let mut body = Vec::new();
            loop {
                match response.chunk().await {
                    Ok(Some(chunk)) => {
                        body.extend_from_slice(&chunk[..chunk.len().min(cap - body.len())]);
                        if body.len() >= cap {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => return Answer::Transport(describe(e)),
                }
            }
            if status == 200 { Answer::Ok(body) } else { Answer::Status(status, body) }
        })
    }

    /// One refresh of `group` at `now_unix` under the policy in the module
    /// doc: at most one request, and none at all unless it is due.
    /// A group not in [`GROUPS`] is `Failed` before anything is read,
    /// written or requested.
    pub fn refresh(&self, group: &str, now_unix: f64) -> Outcome {
        let Some(body_path) = self.body_path(group) else { return Outcome::Failed(unknown_group(group)) };
        let meta = self.meta(group);
        if meta.consecutive_errors >= LATCH_AFTER {
            return Outcome::Latched;
        }
        let since_fetch_due = meta.fetched_unix.map_or(f64::NEG_INFINITY, |f| f + min_interval_s(group));
        if now_unix < meta.next_allowed_unix || now_unix < since_fetch_due {
            return Outcome::NotDue { next_allowed_unix: meta.next_allowed_unix.max(since_fetch_due) };
        }
        if let Err(e) = std::fs::create_dir_all(&self.cache_dir) {
            return Outcome::Failed(format!("cache dir {}: {e}", self.cache_dir.display()));
        }
        let Some(_lock) = self.lock(now_unix) else { return Outcome::Locked };
        // Under the lock, decide again: another fetcher may have fetched
        // (or failed) between our first look and taking the lock.
        let mut meta = self.meta(group);
        if meta.consecutive_errors >= LATCH_AFTER {
            return Outcome::Latched;
        }
        let since_fetch_due = meta.fetched_unix.map_or(f64::NEG_INFINITY, |f| f + min_interval_s(group));
        if now_unix < meta.next_allowed_unix || now_unix < since_fetch_due {
            return Outcome::NotDue { next_allowed_unix: meta.next_allowed_unix.max(since_fetch_due) };
        }

        // Whatever happens next, the next request waits two hours -- and
        // that is on disk BEFORE the request, so a kill mid-download (or a
        // failed write afterwards) cannot make the next launch due at once.
        // No request when it cannot be recorded.
        meta.next_allowed_unix = now_unix + min_interval_s(group);
        if let Err(e) = self.write_meta(group, &meta) {
            return Outcome::Failed(format!("cannot record the attempt, not requesting: {e}"));
        }
        let outcome = match self.request(group) {
            Answer::Ok(body) => match validate(group, &body) {
                Ok((0, skipped)) => {
                    meta.status = Some(200);
                    Outcome::Failed(format!("no usable records ({skipped} skipped)"))
                }
                Ok((records, _)) => match write_atomic(&body_path, &body) {
                    Ok(()) => {
                        let bytes = body.len() as u64;
                        meta = Meta {
                            fetched_unix: Some(now_unix),
                            status: Some(200),
                            bytes,
                            records,
                            next_allowed_unix: now_unix + min_interval_s(group),
                            consecutive_errors: 0,
                            last_error: None,
                        };
                        Outcome::Fetched { records, bytes }
                    }
                    Err(e) => {
                        meta.status = Some(200);
                        Outcome::Failed(format!("cannot write the body: {e}"))
                    }
                },
                Err(e) => {
                    meta.status = Some(200);
                    Outcome::Failed(e)
                }
            },
            Answer::Status(status, body) => {
                meta.status = Some(status);
                Outcome::Refused { status, body_head: head_of(&body) }
            }
            Answer::Transport(e) => {
                meta.status = None;
                Outcome::Failed(e)
            }
        };
        match &outcome {
            Outcome::Fetched { .. } => {}
            Outcome::Refused { status, body_head } => {
                meta.consecutive_errors += 1;
                meta.last_error = Some(format!("HTTP {status}: {body_head}"));
            }
            Outcome::Failed(e) => {
                meta.consecutive_errors += 1;
                meta.last_error = Some(head_of(e.as_bytes()));
            }
            Outcome::NotDue { .. } | Outcome::Latched | Outcome::Locked => unreachable!("decided before the request"),
        }
        self.write_meta_logged(group, &meta);
        outcome
    }
}

/// Wall-clock Unix seconds.
fn wall_unix() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

/// A lock file's age, seconds: `now_unix` minus its stamp, or its mtime age
/// by the wall clock when the stamp does not parse. `None` when it is gone.
fn lock_age_s(path: &Path, now_unix: f64) -> Option<f64> {
    match std::fs::read_to_string(path) {
        Ok(s) => match s.trim().parse::<f64>() {
            Ok(stamp) => Some(now_unix - stamp),
            Err(_) => {
                let modified = std::fs::metadata(path).ok()?.modified().ok()?;
                Some(std::time::SystemTime::now().duration_since(modified).map_or(0.0, |d| d.as_secs_f64()))
            }
        },
        Err(_) => None,
    }
}

/// "25 min ago" under 2 h, "3 h ago" under 48 h, "4 d ago" after.
fn age(seconds: f64) -> String {
    let s = seconds.max(0.0);
    if s < 7_200.0 {
        format!("{:.0} min ago", (s / 60.0).round())
    } else if s < 48.0 * 3_600.0 {
        format!("{:.0} h ago", (s / 3_600.0).round())
    } else {
        format!("{:.0} d ago", (s / 86_400.0).round())
    }
}

/// The panel's one-line data status for `group` at `now_unix`.
pub fn status_line(_group: &str, meta: &Meta, now_unix: f64) -> String {
    if meta.consecutive_errors >= LATCH_AFTER {
        return format!("CelesTrak: stopped after {} errors. Retry?", meta.consecutive_errors);
    }
    let Some(fetched) = meta.fetched_unix else { return "No orbital data yet".to_string() };
    let ago = age(now_unix - fetched);
    if meta.consecutive_errors == 0 {
        return format!("CelesTrak: updated {ago}");
    }
    match meta.status {
        Some(200) => format!("CelesTrak sent no orbital data: using data from {ago}"),
        Some(status) => format!("CelesTrak refused ({status}): using data from {ago}"),
        None => format!("CelesTrak unreachable: using data from {ago}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };

    /// A local HTTP stub: answers each connection with the next scripted (status, body, delay_ms), records
    /// every request head, and counts hits. The real CelesTrak is never contacted by any test.
    struct Stub {
        url: String,
        hits: Arc<AtomicUsize>,
        heads: Arc<Mutex<Vec<String>>>,
    }
    fn stub(script: Vec<(u16, &'static str, u64)>) -> Stub {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/gp.php", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let heads = Arc::new(Mutex::new(Vec::new()));
        let (h, hd) = (hits.clone(), heads.clone());
        std::thread::spawn(move || {
            let mut script = script.into_iter().cycle();
            for conn in listener.incoming().take(64) {
                let Ok(mut conn) = conn else { break };
                let mut buf = [0u8; 4096];
                let n = conn.read(&mut buf).unwrap_or(0);
                hd.lock().unwrap().push(String::from_utf8_lossy(&buf[..n]).into_owned());
                h.fetch_add(1, Ordering::SeqCst);
                let (status, body, delay) = script.next().unwrap();
                std::thread::sleep(std::time::Duration::from_millis(delay));
                let _ = write!(
                    conn,
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        Stub { url, hits, heads }
    }
    const BODY: &str = r#"[{"OBJECT_NAME":"ISS (ZARYA)","OBJECT_ID":"1998-067A","EPOCH":"2026-09-28T11:09:16.121952","MEAN_MOTION":15.48680135,"ECCENTRICITY":0.00071596,"INCLINATION":51.6312,"RA_OF_ASC_NODE":1,"ARG_OF_PERICENTER":2,"MEAN_ANOMALY":3,"NORAD_CAT_ID":25544,"BSTAR":0.0001,"MEAN_MOTION_DOT":0.0001,"MEAN_MOTION_DDOT":0}]"#;
    /// `tempfile` is already a dev-dependency, so it stands in for the
    /// brief's hand-written `tempdir_path::Dir`.
    fn fetcher(url: &str) -> (Fetcher, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (
            Fetcher {
                cache_dir: dir.path().to_path_buf(),
                base_url: url.to_string(),
                timeout: std::time::Duration::from_millis(300),
                user_agent: "gpui-map-satellites/test".into(),
            },
            dir,
        )
    }
    const T0: f64 = 1_790_000_000.0;

    #[test]
    fn the_stub_is_hit_once_per_two_hours_of_simulated_time() {
        let s = stub(vec![(200, BODY, 0)]);
        let (f, _d) = fetcher(&s.url);
        let mut t = T0;
        while t < T0 + 6.0 * 3600.0 {
            let _ = f.refresh("stations", t);
            t += 300.0;
        }
        assert_eq!(s.hits.load(Ordering::SeqCst), 3, "attempts every 5 min for 6 h must reach the server 3 times");
        assert_eq!(f.meta("stations").records, 1);
    }

    /// Final review M4: the Fetcher guards its own paths. A group name
    /// shaped like a path traversal (or any name not in `GROUPS`) reads
    /// nothing, writes nothing -- in the cache or beside it -- and reaches
    /// no server.
    #[test]
    fn a_group_outside_groups_touches_no_path_and_no_server() {
        let s = stub(vec![(200, BODY, 0)]);
        let (f, d) = fetcher(&s.url);
        let outside = d.path().join("escaped");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("x.json"), BODY).unwrap();
        std::fs::create_dir_all(d.path().join("cache")).unwrap();
        let f = Fetcher { cache_dir: d.path().join("cache"), ..f };
        for group in ["../escaped/x", "..", "stations/../../escaped/x", "/tmp/x", "Stations", ""] {
            let out = f.refresh(group, T0);
            assert!(matches!(&out, Outcome::Failed(e) if e.contains("unknown group")), "{group:?}: {out:?}");
            assert!(f.cached(group).is_none(), "{group:?} read a body from outside the cache");
            assert_eq!(f.meta(group), Meta::default());
            assert_eq!(f.seed(group, BODY.as_bytes(), T0).unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
            f.retry(group);
        }
        assert_eq!(s.hits.load(Ordering::SeqCst), 0, "no request for an unknown group");
        assert_eq!(std::fs::read_dir(d.path().join("cache")).unwrap().count(), 0, "nothing written in the cache");
        let beside: Vec<_> = std::fs::read_dir(&outside).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(beside, vec![std::ffi::OsString::from("x.json")], "nothing written beside it");
        assert!(GROUPS.iter().all(|g| is_known_group(g)));
    }

    #[test]
    fn the_request_asks_for_json_with_our_user_agent() {
        let s = stub(vec![(200, BODY, 0)]);
        let (f, _d) = fetcher(&s.url);
        assert!(matches!(f.refresh("gps-ops", T0), Outcome::Fetched { records: 1, .. }));
        let head = s.heads.lock().unwrap()[0].clone();
        assert!(head.contains("GROUP=gps-ops") && head.contains("FORMAT=json"), "{head}");
        assert!(head.to_ascii_lowercase().contains("user-agent: gpui-map-satellites/"), "{head}");
    }

    #[test]
    fn a_403_keeps_the_cache_and_waits_two_hours() {
        let s = stub(vec![(200, BODY, 0), (403, "GP data has not updated since your last successful download", 0)]);
        let (f, _d) = fetcher(&s.url);
        f.refresh("active", T0);
        let before = f.cached("active").unwrap();
        let out = f.refresh("active", T0 + 7_200.0);
        assert!(matches!(out, Outcome::Refused { status: 403, .. }), "{out:?}");
        assert_eq!(f.cached("active").unwrap(), before, "the cached body survives a refusal");
        let m = f.meta("active");
        assert_eq!((m.consecutive_errors, m.next_allowed_unix), (1, T0 + 2.0 * 7_200.0));
        assert!(matches!(f.refresh("active", T0 + 7_300.0), Outcome::NotDue { .. }));
        assert_eq!(s.hits.load(Ordering::SeqCst), 2);
        assert!(status_line("active", &m, T0 + 3.0 * 3600.0).contains("CelesTrak refused (403)"));
    }

    #[test]
    fn a_timeout_is_an_error_and_no_retry_inside_the_window() {
        let s = stub(vec![(200, BODY, 1_000)]);
        let (f, _d) = fetcher(&s.url);
        assert!(matches!(f.refresh("geo", T0), Outcome::Failed(_)));
        assert!(matches!(f.refresh("geo", T0 + 60.0), Outcome::NotDue { .. }));
        assert_eq!(s.hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn three_errors_latch_until_retry_and_retry_still_waits() {
        let s = stub(vec![(500, "oops", 0)]);
        let (f, _d) = fetcher(&s.url);
        for k in 0..3 {
            f.refresh("active", T0 + f64::from(k) * 7_200.0);
        }
        assert!(matches!(f.refresh("active", T0 + 6.0 * 3600.0), Outcome::Latched));
        assert_eq!(s.hits.load(Ordering::SeqCst), 3, "latched: no fourth request");
        f.retry("active");
        assert!(matches!(f.refresh("active", T0 + 5.0 * 3600.0 - 1.0), Outcome::NotDue { .. }), "retry does not skip the 2 h rule");
        f.refresh("active", T0 + 6.0 * 3600.0);
        assert_eq!(s.hits.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn a_stale_lock_older_than_two_minutes_is_broken() {
        let s = stub(vec![(200, BODY, 0)]);
        let (f, _d) = fetcher(&s.url);
        std::fs::write(f.cache_dir.join(".lock"), format!("{}", T0 - 30.0)).unwrap();
        assert!(matches!(f.refresh("stations", T0), Outcome::Locked), "a live lock is respected");
        std::fs::write(f.cache_dir.join(".lock"), format!("{}", T0 - 121.0)).unwrap();
        assert!(matches!(f.refresh("stations", T0), Outcome::Fetched { .. }), "a stale lock is broken");
        assert!(!f.cache_dir.join(".lock").exists(), "the lock is released after the fetch");
    }

    #[test]
    fn a_200_that_is_not_omm_is_an_error_and_keeps_the_cache() {
        let s = stub(vec![(200, BODY, 0), (200, "No GP data found", 0)]);
        let (f, _d) = fetcher(&s.url);
        f.refresh("stations", T0);
        assert!(matches!(f.refresh("stations", T0 + 7_200.0), Outcome::Failed(_)));
        assert_eq!(f.cached("stations").unwrap(), BODY.as_bytes());
    }

    #[test]
    fn seeding_records_a_fetch_without_a_request() {
        let s = stub(vec![(200, BODY, 0)]);
        let (f, _d) = fetcher(&s.url);
        f.seed("stations", BODY.as_bytes(), T0).unwrap();
        assert!(matches!(f.refresh("stations", T0 + 60.0), Outcome::NotDue { .. }));
        assert_eq!(s.hits.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn the_status_line_rounds_the_age_and_names_each_state() {
        let ok = Meta { fetched_unix: Some(T0), status: Some(200), next_allowed_unix: T0 + MIN_INTERVAL_S, ..Default::default() };
        assert_eq!(status_line("active", &ok, T0 + 25.0 * 60.0), "CelesTrak: updated 25 min ago");
        assert_eq!(status_line("active", &ok, T0 + 5.0 * 86_400.0), "CelesTrak: updated 5 d ago");
        let down = Meta { status: None, consecutive_errors: 1, ..ok.clone() };
        assert_eq!(status_line("active", &down, T0 + 3.0 * 3600.0), "CelesTrak unreachable: using data from 3 h ago");
        let latched = Meta { consecutive_errors: LATCH_AFTER, ..ok };
        assert_eq!(status_line("active", &latched, T0), "CelesTrak: stopped after 3 errors. Retry?");
        assert_eq!(status_line("active", &Meta::default(), T0), "No orbital data yet");
    }

    #[test]
    fn a_fetch_leaves_no_tmp_and_the_meta_round_trips() {
        let s = stub(vec![(200, BODY, 0)]);
        let (f, _d) = fetcher(&s.url);
        f.refresh("stations", T0);
        let names: Vec<String> = std::fs::read_dir(&f.cache_dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        assert!(names.iter().all(|n| !n.ends_with(".tmp")), "{names:?}");
        let m = f.meta("stations");
        assert_eq!((m.fetched_unix, m.status, m.bytes, m.records), (Some(T0), Some(200), BODY.len() as u64, 1));
    }

    /// A crash between writing `*.tmp` and the rename leaves half-written
    /// temp files next to the good ones: readers never see them, and the
    /// next fetch writes over them.
    #[test]
    fn a_crash_mid_write_leaves_the_old_files_readable() {
        let s = stub(vec![(200, BODY, 0)]);
        let (f, _d) = fetcher(&s.url);
        f.refresh("stations", T0);
        let meta = f.meta("stations");
        std::fs::write(f.cache_dir.join("stations.json.tmp"), &BODY.as_bytes()[..40]).unwrap();
        std::fs::write(f.cache_dir.join("stations.meta.json.tmp"), b"{\"fetched_unix\": 1").unwrap();
        assert_eq!(f.cached("stations").unwrap(), BODY.as_bytes(), "the half body is never read");
        assert_eq!(f.meta("stations"), meta, "nor the half meta");
        assert!(matches!(f.refresh("stations", T0 + 60.0), Outcome::NotDue { .. }), "the backoff survived");
        assert!(matches!(f.refresh("stations", T0 + 7_200.0), Outcome::Fetched { .. }));
        assert!(!f.cache_dir.join("stations.json.tmp").exists() && !f.cache_dir.join("stations.meta.json.tmp").exists());
    }

    /// A local stub answering each connection with the next raw response
    /// (cycled), then closing.
    fn raw_stub(script: Vec<Vec<u8>>) -> (String, Arc<AtomicUsize>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/gp.php", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        std::thread::spawn(move || {
            let mut script = script.into_iter().cycle();
            for conn in listener.incoming().take(64) {
                let Ok(mut conn) = conn else { break };
                let mut buf = [0u8; 4096];
                let _ = conn.read(&mut buf);
                h.fetch_add(1, Ordering::SeqCst);
                let _ = conn.write_all(&script.next().unwrap());
            }
        });
        (url, hits)
    }

    fn response(declared_len: usize, body: &[u8]) -> Vec<u8> {
        let mut r = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {declared_len}\r\nconnection: close\r\n\r\n").into_bytes();
        r.extend_from_slice(body);
        r
    }

    /// Review F8: a body cut short -- the connection closing before the
    /// declared length, or a complete answer holding truncated JSON -- is
    /// never cached, and a good cache survives it.
    #[test]
    fn a_partial_body_is_never_cached() {
        let half = &BODY.as_bytes()[..BODY.len() / 2];
        let (url, hits) = raw_stub(vec![response(BODY.len(), half), response(half.len(), half)]);
        let (f, _d) = fetcher(&url);
        assert!(matches!(f.refresh("stations", T0), Outcome::Failed(_)), "closed early");
        assert!(matches!(f.refresh("stations", T0 + 7_200.0), Outcome::Failed(_)), "truncated JSON");
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        assert_eq!(f.cached("stations"), None, "nothing cached from either");
        assert_eq!(f.meta("stations").consecutive_errors, 2);

        let (url, _) = raw_stub(vec![response(BODY.len(), BODY.as_bytes()), response(BODY.len(), half)]);
        let (g, _d2) = fetcher(&url);
        assert!(matches!(g.refresh("stations", T0), Outcome::Fetched { .. }));
        assert!(matches!(g.refresh("stations", T0 + 7_200.0), Outcome::Failed(_)));
        assert_eq!(g.cached("stations").unwrap(), BODY.as_bytes(), "the good body survives");
    }

    /// Review F4: a 200 with no usable record (`[]`, or every record
    /// skipped) keeps the old cache and counts as an error.
    #[test]
    fn a_200_with_no_usable_records_keeps_the_cache() {
        let s = stub(vec![(200, BODY, 0), (200, "[]", 0), (200, r#"[{"OBJECT_NAME":"X"}]"#, 0)]);
        let (f, _d) = fetcher(&s.url);
        f.refresh("active", T0);
        let out = f.refresh("active", T0 + 7_200.0);
        assert!(matches!(&out, Outcome::Failed(e) if e.contains("no usable records")), "{out:?}");
        let out = f.refresh("active", T0 + 14_400.0);
        assert!(matches!(&out, Outcome::Failed(e) if e.contains("no usable records")), "{out:?}");
        assert_eq!(f.cached("active").unwrap(), BODY.as_bytes());
        let m = f.meta("active");
        assert_eq!((m.records, m.consecutive_errors, m.fetched_unix), (1, 2, Some(T0)));
        assert!(status_line("active", &m, T0 + 4.0 * 3600.0).starts_with("CelesTrak sent no orbital data"));
    }

    /// Review F1: two fetchers on one cache directory at once -- threads
    /// here, processes in life -- make one request between them.
    #[test]
    fn two_fetchers_at_once_make_one_request() {
        let s = stub(vec![(200, BODY, 200)]);
        let dir = tempfile::tempdir().unwrap();
        let make = || Fetcher {
            cache_dir: dir.path().to_path_buf(),
            base_url: s.url.clone(),
            timeout: std::time::Duration::from_secs(5),
            user_agent: "gpui-map-satellites/test".into(),
        };
        for round in 0..3 {
            let t = T0 + f64::from(round) * 7_200.0;
            let fetchers: Vec<Fetcher> = (0..4).map(|_| make()).collect();
            let outcomes: Vec<Outcome> = std::thread::scope(|sc| {
                let handles: Vec<_> = fetchers.iter().map(|f| sc.spawn(move || f.refresh("stations", t))).collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });
            assert_eq!(outcomes.iter().filter(|o| matches!(o, Outcome::Fetched { .. })).count(), 1, "round {round}: {outcomes:?}");
            assert_eq!(s.hits.load(Ordering::SeqCst), round as usize + 1, "round {round}: {outcomes:?}");
        }
    }

    /// Review F1: several fetchers finding the same stale lock at once --
    /// only one of them breaks it and requests.
    #[test]
    fn a_stale_lock_is_broken_by_one_fetcher_only() {
        let s = stub(vec![(200, BODY, 200)]);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".lock"), format!("{}", T0 - 500.0)).unwrap();
        let fetchers: Vec<Fetcher> = (0..6)
            .map(|_| Fetcher {
                cache_dir: dir.path().to_path_buf(),
                base_url: s.url.clone(),
                timeout: std::time::Duration::from_secs(5),
                user_agent: "gpui-map-satellites/test".into(),
            })
            .collect();
        let outcomes: Vec<Outcome> = std::thread::scope(|sc| {
            let handles: Vec<_> = fetchers.iter().map(|f| sc.spawn(move || f.refresh("stations", T0))).collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(s.hits.load(Ordering::SeqCst), 1, "{outcomes:?}");
        assert!(!dir.path().join(".lock").exists(), "released");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path()).unwrap().filter_map(|e| e.ok()?.file_name().into_string().ok()).filter(|n| n.starts_with(".lock")).collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    /// Review F3: a lock whose stamp does not parse (a crash between
    /// creating and stamping it) expires by its mtime, not never.
    #[test]
    fn an_unstamped_lock_expires_by_its_mtime() {
        let s = stub(vec![(200, BODY, 0)]);
        let (f, _d) = fetcher(&s.url);
        let lock = f.cache_dir.join(".lock");
        std::fs::write(&lock, b"").unwrap();
        assert!(matches!(f.refresh("stations", T0), Outcome::Locked), "a fresh unstamped lock is live");
        let file = std::fs::OpenOptions::new().write(true).open(&lock).unwrap();
        file.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(121)).unwrap();
        drop(file);
        assert!(matches!(f.refresh("stations", T0), Outcome::Fetched { .. }), "an old unstamped lock is stale");
        assert_eq!(s.hits.load(Ordering::SeqCst), 1);
    }

    /// Review F5: the backoff is on disk before the request goes out, and
    /// no request is made when it cannot be written.
    #[test]
    fn the_backoff_is_recorded_before_the_request() {
        let s = stub(vec![(200, BODY, 400)]);
        let (f, _d) = fetcher(&s.url);
        let f = Fetcher { timeout: std::time::Duration::from_secs(5), ..f };
        std::thread::scope(|sc| {
            let h = sc.spawn(|| f.refresh("active", T0));
            std::thread::sleep(std::time::Duration::from_millis(200));
            // Mid-request (a kill now would keep this): already backed off.
            assert_eq!(f.meta("active").next_allowed_unix, T0 + MIN_INTERVAL_S);
            assert!(matches!(h.join().unwrap(), Outcome::Fetched { .. }));
        });

        let s = stub(vec![(200, BODY, 0)]);
        let (g, _d2) = fetcher(&s.url);
        // The meta's temp path is a directory: the attempt cannot be written.
        std::fs::create_dir_all(g.cache_dir.join("geo.meta.json.tmp")).unwrap();
        let out = g.refresh("geo", T0);
        assert!(matches!(&out, Outcome::Failed(e) if e.contains("cannot record the attempt")), "{out:?}");
        assert_eq!(s.hits.load(Ordering::SeqCst), 0, "no request it could not record");
    }

    /// Review F2: seeding never rolls a newer cache back, never moves the
    /// next request earlier, and never clears the error stop.
    #[test]
    fn seeding_after_a_fetch_changes_nothing() {
        let s = stub(vec![(200, BODY, 0), (500, "oops", 0)]);
        let (f, _d) = fetcher(&s.url);
        assert!(matches!(f.refresh("stations", T0), Outcome::Fetched { .. }));
        let before = f.meta("stations");
        const OLDER: &str = r#"[{"OBJECT_NAME":"OLD","OBJECT_ID":"1998-067A","EPOCH":"2026-09-20T11:09:16.121952","MEAN_MOTION":15.48680135,"ECCENTRICITY":0.00071596,"INCLINATION":51.6312,"RA_OF_ASC_NODE":1,"ARG_OF_PERICENTER":2,"MEAN_ANOMALY":3,"NORAD_CAT_ID":25544,"BSTAR":0.0001,"MEAN_MOTION_DOT":0.0001,"MEAN_MOTION_DDOT":0}]"#;
        f.seed("stations", OLDER.as_bytes(), T0 - 4.0 * 3600.0).unwrap();
        assert_eq!(f.meta("stations"), before);
        assert_eq!(f.cached("stations").unwrap(), BODY.as_bytes());
        assert!(matches!(f.refresh("stations", T0 + 60.0), Outcome::NotDue { .. }));
        assert_eq!(s.hits.load(Ordering::SeqCst), 1);
        // An error on record: a (newer) seed does not clear it either.
        assert!(matches!(f.refresh("stations", T0 + 7_200.0), Outcome::Refused { status: 500, .. }));
        f.seed("stations", OLDER.as_bytes(), T0 + 7_300.0).unwrap();
        let m = f.meta("stations");
        assert_eq!((m.consecutive_errors, m.fetched_unix), (1, Some(T0)));
        // A seed never moves the next request earlier.
        let (g, _d2) = fetcher(&s.url);
        g.seed("stations", BODY.as_bytes(), T0).unwrap();
        g.seed("stations", BODY.as_bytes(), T0 + 10.0).unwrap();
        assert_eq!(g.meta("stations").next_allowed_unix, T0 + 10.0 + MIN_INTERVAL_S);
    }

    #[test]
    fn the_satcat_waits_a_day_and_lives_at_its_own_url() {
        assert_eq!(min_interval_s(SATCAT), 86_400.0);
        assert_eq!(min_interval_s("active"), MIN_INTERVAL_S);
        assert_eq!(satcat_url(BASE_URL), "https://celestrak.org/satcat/records.php?GROUP=active&FORMAT=json");
        assert!(is_known_group(SATCAT));
    }

}

//! Tile bytes, off the main thread. Also the sprite sheet's two one-time
//! requests (`Job::Sprite`/`Fetched::Sprite*`), the same pool and the same
//! [`Fetcher::drain`] a tile uses -- see `app::MapApp::create_renderer`.
//!
//! # Why not `reqwest` + `tokio`
//!
//! The parent crate's `tile_source::http` is `reqwest` on a tokio
//! `rt-multi-thread` runtime, and both would compile for iOS. Neither earns
//! its place here. What this spike needs of the network is: issue up to a
//! few dozen independent GETs, never block the frame, and hand the bytes
//! back. That is a bounded pool of blocking sockets -- the shape a thread
//! pool has natively -- and wrapping it in a work-stealing async executor
//! adds a runtime, a reactor thread, and `Send + 'static` constraints on
//! every future for no behaviour the frame loop can observe.
//!
//! So: `ureq` (blocking, pure-Rust TLS through `rustls` + `ring`, bundled
//! webpki roots) on [`WORKERS`] `std::thread`s taking jobs from a shared
//! priority queue (`fetch_queue`; see [`Fetcher`]), results back over an
//! `mpsc`. `ureq`'s default `platform-verifier` is deliberately OFF -- it
//! links the Security framework through a second objc stack, and a public
//! CDN over a bundled root store does not need the system trust store.
//!
//! # `WORKERS` is a real policy decision, not a magic number
//!
//! The browser port never had to choose one: it issued a `fetch` per tile
//! and let Chrome's six-connections-per-host limit do the rationing by
//! accident (`web/src/app.rs`, `request_missing_tiles`). Here nothing
//! rations it for us. Eight is the same order as that accidental limit, and
//! it matters on a phone for a reason it did not in a tab: every worker
//! blocked on a socket is a thread whose stack is resident.

use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::geo::TileId;
use crate::sprite::redact_access_token;

/// Concurrent in-flight tile requests. See the module doc comment.
const WORKERS: usize = 8;

/// The longest any one request may take, connect to last body byte. ureq's
/// defaults set NO timeout at all, so a socket that stalled on weak
/// cellular (a dead NAT mapping, a half-open TCP connection) held its
/// worker forever -- and with all [`WORKERS`] stalled, every fetch in the
/// app stopped, which is a blank globe. Generous on purpose: a 200 KB
/// vector tile at a very poor 64 kbit/s is ~25 s, and a timed-out tile is
/// retried (see `fetch_policy`), so too short costs a retry, too long costs
/// a worker.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);
/// Connecting is the part that stalls silently when the radio has no
/// usable path; it is also the cheap part, so it gets a shorter leash.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Which of this crate's two tile grids a job or result is for.
///
/// Round-tripped through [`Job`] and [`Fetched`] rather than left for the
/// caller to infer from which of its own sets contains the `TileId`: the
/// vector source and an imagery source cover the same `(z, x, y)` grid, so
/// the SAME id can be in flight for both at once (a raster prefetch of a
/// tile whose vector twin is also loading), and a byte-format guess keyed
/// on tile id alone would have no way to tell which request it belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum TileKind {
    Vector,
    Raster,
}

/// Which of the two one-time sprite-sheet requests a sprite job or result
/// is for. Not folded into [`TileKind`]: a sprite request carries no
/// [`TileId`] -- there is no tile-grid position for a sprite sheet -- and
/// there are exactly two of these ever in flight per app launch (see
/// `app::MapApp::create_renderer`), not one per visible tile.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SpriteResource {
    /// The manifest -- `<base>@2x.json`.
    Manifest,
    /// The packed atlas PNG -- `<base>@2x.png`.
    Image,
}

/// One finished request. The `bool` is `is_prefetch`, carried through
/// untouched so the frame loop can tell a coarse-cover byte from a
/// visible-cover one without a second map lookup.
pub enum Fetched {
    /// Bytes, still encoded (MVT for [`TileKind::Vector`], PNG/JPEG for
    /// [`TileKind::Raster`]) -- decoding is the caller's, because both
    /// `mvt::decode` and `raster::decode_tile` are engine work and this
    /// module knows nothing about the engine.
    Body(TileId, TileKind, bool, Vec<u8>),
    /// The source genuinely has no data here (HTTP 404, 410 or 204). Distinct from
    /// [`Fetched::Failed`] because it must be remembered forever rather
    /// than retried.
    Missing(TileId, TileKind),
    /// Anything else: a network error, a timeout, a truncated body, or a
    /// status other than 404/410/204. Retried with backoff by the caller
    /// (`fetch_policy::RetryTable`), never remembered as missing. The
    /// `String` is already redacted -- a Mapbox tile URL carries
    /// `access_token=...` exactly as a sprite URL does.
    Failed(TileId, TileKind, TileFailure),
    /// The sprite manifest or image bytes, undecoded -- `app.rs`'s
    /// `try_install_mapbox_sprites` parses/slices/packs both once they have
    /// both arrived (order is not guaranteed: the two are queued together
    /// but drain independently).
    SpriteBody(SpriteResource, Vec<u8>),
    /// A 404 on a sprite request is NOT remembered the way a tile's is --
    /// there is nothing to retry (this fires at most twice, ever) and
    /// nothing to redraw around: the bundled sheet `app.rs` installs at
    /// startup stays live, and the map already has icons.
    SpriteMissing(SpriteResource),
    /// Same redaction as a tile URL never needs: a Mapbox sprite URL
    /// carries `access_token=...`, and this string is the one `app.rs`
    /// prints on failure.
    SpriteFailed(SpriteResource, String),
}

/// Why a tile request failed, and when the server asked to be retried.
#[derive(Debug, Clone, PartialEq)]
pub struct TileFailure {
    pub message: String,
    /// The response's `Retry-After`, in milliseconds, if it had a usable
    /// one (delta-seconds form; see `fetch_policy::parse_retry_after_ms`).
    pub retry_after_ms: Option<f64>,
}

impl TileFailure {
    pub fn new(message: impl Into<String>) -> Self {
        TileFailure { message: message.into(), retry_after_ms: None }
    }
}

impl std::fmt::Display for TileFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)?;
        if let Some(ms) = self.retry_after_ms {
            write!(f, " (retry-after {:.0} s)", ms / 1000.0)?;
        }
        Ok(())
    }
}

enum Job {
    Tile { tile: TileId, kind: TileKind, is_prefetch: bool, url: String },
    /// One of the two sprite requests. No `TileId`/`is_prefetch`: neither
    /// means anything for a sheet that is not part of the tile grid.
    Sprite { resource: SpriteResource, url: String },
}

/// What a queued tile job carries besides its key.
struct TileJob {
    is_prefetch: bool,
    url: String,
}

/// A tile job's identity in the queue: the same `(z, x, y)` can be wanted
/// as vector AND as imagery at once (see [`TileKind`]).
pub type TileKey = (TileId, TileKind);

/// Everything the workers share, behind one mutex.
struct Shared {
    /// Pending and running tile jobs, by priority. See `fetch_queue`.
    queue: crate::fetch_queue::FetchQueue<TileKey, TileJob>,
    /// The two one-time sprite requests. Always served before any tile: there
    /// are at most two per launch, and the map's icons wait on them.
    sprites: std::collections::VecDeque<(SpriteResource, String)>,
    /// Set by `Drop`: every worker returns at its next look at the queue.
    closed: bool,
}

/// A fixed pool of blocking HTTP workers, the priority queue they take jobs
/// from, and the channel their results come back on.
///
/// # Not a FIFO any more
///
/// This was an `mpsc` job channel: strictly first-in first-out, unbounded,
/// and never cancelled. A tilted pan queued hundreds of tiles per camera
/// step, and every one stayed ahead of the next step's tiles, so the
/// foreground waited behind tiles the camera had already left (measured in
/// `fetch_queue`'s harness). Now a worker takes the most urgent PENDING job
/// ([`crate::fetch_queue::FetchQueue::pop`]), and the frame loop hands the
/// fetcher its whole wanted set every frame ([`Fetcher::retarget`]) so a
/// pending job the camera left is dropped before it ever reaches a socket.
/// A job already running is left to finish: ureq cannot abort a blocking
/// request cheaply, and the frame loop decides what its bytes are worth.
pub struct Fetcher {
    shared: Arc<(Mutex<Shared>, Condvar)>,
    done: Receiver<Fetched>,
    /// Called by a worker right after it pushes onto `done`, so the app can
    /// wake its (now `ControlFlow::Wait`) main loop the instant a tile
    /// arrives rather than finding out only on the next frame it happened
    /// to draw for some other reason. `None` until `set_wake` is called --
    /// see that method's own doc for why the setter, not the constructor.
    wake: Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>>,
    /// Tile bodies on disk (`crate::disk_cache`), when the host gave a
    /// directory: read before the network, written after it.
    cache: Arc<Mutex<Option<Arc<crate::disk_cache::DiskCache>>>>,
}

impl Fetcher {
    pub fn new() -> Fetcher {
        let (done_tx, done_rx) = mpsc::channel::<Fetched>();
        let shared = Arc::new((
            Mutex::new(Shared {
                queue: crate::fetch_queue::FetchQueue::new(),
                sprites: std::collections::VecDeque::new(),
                closed: false,
            }),
            Condvar::new(),
        ));
        let wake: Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>> = Arc::new(Mutex::new(None));
        let cache: Arc<Mutex<Option<Arc<crate::disk_cache::DiskCache>>>> = Arc::new(Mutex::new(None));

        for i in 0..WORKERS {
            let shared = shared.clone();
            let done_tx = done_tx.clone();
            let wake = wake.clone();
            let cache = cache.clone();
            // One `Agent` per worker rather than one shared: an `Agent` owns
            // a connection pool, and a per-worker pool keeps a warm TLS
            // session per thread with no contention on the pool's mutex.
            let agent = agent();
            std::thread::Builder::new()
                .name(format!("tile-fetch-{i}"))
                // 512 KiB, not the 2 MiB default. Eight of these are
                // resident for the app's whole life and none of them
                // recurses; the deepest thing on this stack is rustls'
                // handshake.
                .stack_size(512 * 1024)
                .spawn(move || loop {
                    let Some(job) = next_job(&shared) else { return };
                    let cache = cache.lock().ok().and_then(|c| c.clone());
                    let result = fetch_cached(&agent, &job, cache.as_deref());
                    // Retired BEFORE the result is sent: the frame loop
                    // clears its in-flight mark on receipt and may ask for
                    // the same tile again at once, which `push` refuses
                    // while the key is still running.
                    if let Job::Tile { tile, kind, .. } = &job {
                        if let Ok(mut guard) = shared.0.lock() {
                            guard.queue.finish(&(*tile, *kind));
                        }
                    }
                    if done_tx.send(result).is_err() {
                        return;
                    }
                    // Best-effort: a wake that never lands because the event
                    // loop already went away costs nothing, and a poisoned
                    // mutex (a wake panicking, which none of them do) must
                    // not take a fetch worker down with it.
                    if let Ok(guard) = wake.lock() {
                        if let Some(wake) = guard.as_ref() {
                            wake();
                        }
                    }
                })
                .expect("spawning a tile-fetch worker");
        }
        Fetcher { shared, done: done_rx, wake, cache }
    }

    /// Lets every worker wake the app's main loop the instant it finishes a
    /// request. A setter rather than a constructor argument: the
    /// `EventLoopProxy` this is built from does not exist until
    /// `EventLoop::new` has run, and `Fetcher::new` is called from
    /// `MapApp::new` -- before that -- so tests and any other caller that
    /// never runs a real event loop can go on constructing a `Fetcher` (and
    /// a `MapApp`) with no proxy to give it. A request queued before this is
    /// called still completes and still lands in `done`; only the wake is
    /// missed, and the very next redraw (a touch, a resize, an animation
    /// frame) drains it exactly as it always did.
    /// Keeps tile bodies in `cache` from now on (`None`: network only).
    pub fn set_disk_cache(&self, cache: Option<Arc<crate::disk_cache::DiskCache>>) {
        *self.cache.lock().expect("fetcher cache mutex poisoned") = cache;
    }

    pub fn set_wake(&self, wake: impl Fn() + Send + Sync + 'static) {
        *self.wake.lock().expect("fetcher wake mutex poisoned") = Some(Arc::new(wake));
    }

    /// Queues a tile at `priority`. Never blocks. Returns `false`, and
    /// queues nothing, when the same `(tile, kind)` is already pending or
    /// running -- the caller's in-flight mark is then already right, since
    /// that job's answer is still coming.
    pub fn request(
        &self,
        tile: TileId,
        kind: TileKind,
        priority: crate::fetch_queue::Priority,
        is_prefetch: bool,
        url: String,
    ) -> bool {
        let (lock, cvar) = &*self.shared;
        let queued = lock
            .lock()
            .expect("tile job queue poisoned")
            .queue
            .push((tile, kind), priority, TileJob { is_prefetch, url });
        if queued {
            cvar.notify_one();
        }
        queued
    }

    /// Re-prioritises every pending tile job by this frame's `wanted` set and
    /// drops every pending job it does not contain; returns the dropped keys
    /// so the caller clears their in-flight marks (they may be asked for
    /// again when the camera comes back). Running jobs are untouched. A
    /// dropped job is NOT a failure: it never reached the network, and its
    /// retry state is left exactly as it was.
    pub fn retarget(&self, wanted: &crate::fetch_queue::Wanted<TileKey>) -> Vec<TileKey> {
        self.shared.0.lock().expect("tile job queue poisoned").queue.retarget(wanted)
    }

    /// `(pending, running, pending jobs dropped as stale so far)`, for the
    /// status line.
    pub fn queue_stats(&self) -> (usize, usize, u64) {
        let guard = self.shared.0.lock().expect("tile job queue poisoned");
        (guard.queue.pending_len(), guard.queue.running_len(), guard.queue.stale_dropped_total())
    }

    /// Queues one of the two one-time sprite-sheet requests. Same pool,
    /// same non-blocking [`Self::drain`] as a tile fetch -- this is the one
    /// HTTP path this crate has, tiles and sprites alike -- but served ahead
    /// of every tile, and never dropped by [`Self::retarget`].
    /// Called exactly twice per app launch, from `app::MapApp::create_renderer`,
    /// when the live style names a `"sprite"` this crate has a token for.
    pub fn request_sprite(&self, resource: SpriteResource, url: String) {
        let (lock, cvar) = &*self.shared;
        lock.lock().expect("tile job queue poisoned").sprites.push_back((resource, url));
        cvar.notify_one();
    }

    /// Everything that has arrived since the last call. Non-blocking, so a
    /// frame with no completed request costs one atomic load.
    pub fn drain(&self) -> Vec<Fetched> {
        let mut out = Vec::new();
        loop {
            match self.done.try_recv() {
                Ok(f) => out.push(f),
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => return out,
            }
        }
    }
}

impl Drop for Fetcher {
    /// Tells every worker to go away. A worker mid-request finishes it
    /// first, then finds `closed` (or a dead `done` channel) and returns.
    fn drop(&mut self) {
        let (lock, cvar) = &*self.shared;
        if let Ok(mut guard) = lock.lock() {
            guard.closed = true;
        }
        cvar.notify_all();
    }
}

/// Blocks until there is a job -- a sprite first, else the most urgent
/// pending tile -- and takes it. `None` once the fetcher is dropped.
fn next_job(shared: &(Mutex<Shared>, Condvar)) -> Option<Job> {
    let (lock, cvar) = shared;
    let mut guard = lock.lock().ok()?;
    loop {
        if guard.closed {
            return None;
        }
        if let Some((resource, url)) = guard.sprites.pop_front() {
            return Some(Job::Sprite { resource, url });
        }
        if let Some(((tile, kind), job)) = guard.queue.pop() {
            return Some(Job::Tile { tile, kind, is_prefetch: job.is_prefetch, url: job.url });
        }
        guard = cvar.wait(guard).ok()?;
    }
}

impl Default for Fetcher {
    fn default() -> Self {
        Fetcher::new()
    }
}

/// One worker's HTTP agent: ureq's defaults plus a finite timeout (see
/// [`REQUEST_TIMEOUT`]) and with non-2xx statuses returned as responses
/// rather than errors, so a 429's `Retry-After` header can be read.
fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .http_status_as_error(false)
        .build()
        .into()
}

/// What a tile response's status means, as [`Fetched`]. `Ok(())` is a
/// data-bearing success whose body the caller reads; anything else is the
/// finished answer. Split out so it is testable without a socket.
fn classify_tile_status(
    tile: TileId,
    kind: TileKind,
    status: u16,
    retry_after: Option<&str>,
) -> Result<(), Fetched> {
    if (200..300).contains(&status) && status != 204 {
        return Ok(());
    }
    // 404 (OpenFreeMap's answer for a tile with no data), 410 and 204 are
    // absence and are remembered; everything else is retried. See
    // `fetch_policy::classify_status`.
    Err(match crate::fetch_policy::classify_status(status, retry_after) {
        crate::fetch_policy::StatusVerdict::Absent => Fetched::Missing(tile, kind),
        crate::fetch_policy::StatusVerdict::Retry { retry_after_ms } => {
            Fetched::Failed(tile, kind, TileFailure { message: format!("HTTP {status}"), retry_after_ms })
        }
    })
}

/// [`fetch`] behind the disk cache: a fresh cached body answers without the
/// network, and a fetched one is kept. Sprites are not cached.
fn fetch_cached(agent: &ureq::Agent, job: &Job, cache: Option<&crate::disk_cache::DiskCache>) -> Fetched {
    let (Some(cache), Job::Tile { tile, kind, is_prefetch, url }) = (cache, job) else {
        return fetch(agent, job);
    };
    if let Some(body) = cache.get(url) {
        return Fetched::Body(*tile, *kind, *is_prefetch, body);
    }
    let fetched = fetch(agent, job);
    if let Fetched::Body(_, _, _, body) = &fetched {
        cache.put(url, body);
    }
    fetched
}

fn fetch(agent: &ureq::Agent, job: &Job) -> Fetched {
    match job {
        // A tile of a PMTiles archive: its byte range, read by the archive's
        // own directories (`pmtiles_http`), not a GET of the URL itself.
        Job::Tile { tile, kind, is_prefetch, url } if url.starts_with(crate::pmtiles_http::PREFIX) => match crate::pmtiles_http::fetch_tile(agent, url) {
            Ok(Some(bytes)) => Fetched::Body(*tile, *kind, *is_prefetch, bytes),
            Ok(None) => Fetched::Missing(*tile, *kind),
            Err(err) => Fetched::Failed(*tile, *kind, TileFailure::new(err)),
        },
        // A tile of a Cloud-Optimised GeoTIFF: rendered by the desktop's
        // COG reader from the internal tiles it covers (`cog_http`).
        #[cfg(feature = "cog_ureq")]
        Job::Tile { tile, kind, is_prefetch, url } if url.starts_with(crate::cog_http::PREFIX) => match crate::cog_http::fetch_tile(url) {
            Ok(Some(bytes)) => Fetched::Body(*tile, *kind, *is_prefetch, bytes),
            Ok(None) => Fetched::Missing(*tile, *kind),
            Err(err) => Fetched::Failed(*tile, *kind, TileFailure::new(err)),
        },
        Job::Tile { tile, kind, is_prefetch, url } => match agent.get(url).call() {
            Ok(mut response) => {
                let retry_after = response.headers().get("retry-after").and_then(|v| v.to_str().ok());
                if let Err(done) = classify_tile_status(*tile, *kind, response.status().as_u16(), retry_after) {
                    return done;
                }
                match response.body_mut().read_to_vec() {
                    Ok(bytes) => Fetched::Body(*tile, *kind, *is_prefetch, bytes),
                    Err(err) => Fetched::Failed(
                        *tile,
                        *kind,
                        TileFailure::new(redact_access_token(&format!("body: {err}"))),
                    ),
                }
            }
            // A network-level failure: DNS, connect, TLS, reset, timeout.
            // All transient from here -- retried with backoff.
            Err(err) => Fetched::Failed(*tile, *kind, TileFailure::new(redact_access_token(&err.to_string()))),
        },
        Job::Sprite { resource, url } => match agent.get(url).call() {
            // The agent returns non-2xx as a response (see [`agent`]), so
            // the sprite path checks the status itself.
            Ok(response) if response.status().as_u16() == 404 => Fetched::SpriteMissing(*resource),
            Ok(response) if !response.status().is_success() => {
                Fetched::SpriteFailed(*resource, format!("HTTP {}", response.status().as_u16()))
            }
            Ok(mut response) => match response.body_mut().read_to_vec() {
                Ok(bytes) => Fetched::SpriteBody(*resource, bytes),
                Err(err) => Fetched::SpriteFailed(*resource, format!("body: {err}")),
            },
            // A Mapbox sprite URL carries `access_token=...`, and ureq's
            // error `Display` embeds the request URL the same way
            // `reqwest`'s does (see `sprite::fetch_bytes`'s own doc
            // comment on why it redacts) -- this string's only consumer is
            // a `println!` in `app.rs`, so it is redacted here, before it
            // ever reaches `Fetched`.
            Err(err) => Fetched::SpriteFailed(*resource, redact_access_token(&err.to_string())),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TILE: TileId = TileId { z: 3, x: 1, y: 2 };

    #[test]
    fn a_404_is_missing_and_a_429_carries_its_retry_after() {
        assert!(classify_tile_status(TILE, TileKind::Vector, 200, None).is_ok());
        assert!(matches!(
            classify_tile_status(TILE, TileKind::Vector, 404, None),
            Err(Fetched::Missing(TILE, TileKind::Vector))
        ));
        assert!(matches!(
            classify_tile_status(TILE, TileKind::Raster, 204, None),
            Err(Fetched::Missing(TILE, TileKind::Raster))
        ));
        match classify_tile_status(TILE, TileKind::Vector, 429, Some("12")) {
            Err(Fetched::Failed(TILE, TileKind::Vector, failure)) => {
                assert_eq!(failure.retry_after_ms, Some(12_000.0));
                assert_eq!(failure.message, "HTTP 429");
            }
            _ => panic!("a 429 must be a retryable failure"),
        }
        assert!(matches!(
            classify_tile_status(TILE, TileKind::Vector, 503, None),
            Err(Fetched::Failed(TILE, TileKind::Vector, _))
        ));
    }

    fn shared() -> (Mutex<Shared>, Condvar) {
        (
            Mutex::new(Shared {
                queue: crate::fetch_queue::FetchQueue::new(),
                sprites: std::collections::VecDeque::new(),
                closed: false,
            }),
            Condvar::new(),
        )
    }

    fn visible(rank: u32) -> crate::fetch_queue::Priority {
        crate::fetch_queue::Priority { class: crate::fetch_queue::FetchClass::Visible, rank }
    }

    /// What a worker takes next: a sprite before any tile, then the most
    /// urgent tile -- not the oldest, which is what the `mpsc` FIFO served.
    /// Driven on the shared state directly, so no worker thread and no
    /// socket is involved.
    #[test]
    fn a_worker_takes_a_sprite_first_then_the_most_urgent_tile() {
        let s = shared();
        let far = TileId { z: 14, x: 1, y: 1 };
        let near = TileId { z: 14, x: 2, y: 2 };
        {
            let mut g = s.0.lock().unwrap();
            g.queue.push((far, TileKind::Vector), visible(900), TileJob { is_prefetch: false, url: "far".into() });
            g.queue.push((near, TileKind::Vector), visible(1), TileJob { is_prefetch: false, url: "near".into() });
            g.sprites.push_back((SpriteResource::Manifest, "sprite".into()));
        }
        assert!(matches!(next_job(&s), Some(Job::Sprite { resource: SpriteResource::Manifest, .. })));
        assert!(matches!(next_job(&s), Some(Job::Tile { tile, .. }) if tile == near));
        assert!(matches!(next_job(&s), Some(Job::Tile { tile, .. }) if tile == far));
        s.0.lock().unwrap().closed = true;
        assert!(next_job(&s).is_none(), "a closed fetcher lets its workers go");
    }

    /// The same `(z, x, y)` is two independent jobs for the two grids, and
    /// neither can be queued twice.
    #[test]
    fn vector_and_imagery_twins_are_separate_jobs_and_never_duplicated() {
        let s = shared();
        let mut g = s.0.lock().unwrap();
        let job = || TileJob { is_prefetch: false, url: String::new() };
        assert!(g.queue.push((TILE, TileKind::Vector), visible(0), job()));
        assert!(g.queue.push((TILE, TileKind::Raster), visible(0), job()));
        assert!(!g.queue.push((TILE, TileKind::Vector), visible(0), job()));
        assert_eq!(g.queue.pending_len(), 2);
    }
}

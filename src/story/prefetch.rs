//! S4.1: every frame's tiles known before the first frame renders, fetched
//! ahead in the order frames first need them, and a per-frame wait with a
//! deadline, so a story frame is captured only once its tiles are there
//! (or its misses are named).
//!
//! - [`plan`] is a pure function of the frames' [`CaptureRequest`]s: each
//!   frame's cover plus its dot-mask ring, each cover tile's ancestors up
//!   to [`STAND_IN_LEVELS`] up (the stand-ins), the order tiles are first
//!   needed in, and the last frame that needs each one.
//! - [`Prefetcher::ensure`] keeps the window `[n, n + lookahead]` queued,
//!   at most `concurrency` fetches in flight, and blocks until frame `n`'s
//!   tiles are resident, absent or failed, or until the deadline: the tile
//!   timeout, or the settle timeout on a settle frame.
//! - [`Prefetcher::evict_through`] drops tiles no later frame needs, so
//!   memory follows the look-ahead window, not the story length.
//!
//! **Stand-ins (spec §4.3, R7).** A tile that times out or fails is drawn
//! as its nearest resident ancestor by `capture::draw_order`, coarse and
//! not a hole. That needs an ancestor to BE resident, so the plan
//! schedules each cover tile's parent chain, [`STAND_IN_LEVELS`] deep, at
//! lower priority than exact tiles: after the exact tiles of the frame
//! that first needs them. A frame whose exact tile failed or is absent
//! also waits (within its deadline) for that tile's stand-ins, so which
//! ancestor draws does not depend on fetch timing.
//!
//! **Order.** Fetches start strictly in queue order: a fetch is spawned
//! only when a slot is free, by `ensure` or by the completion that freed
//! it. (A semaphore acquired inside each spawned task would bound the
//! count too, but the tasks reach the semaphore in whatever order the
//! workers poll them, so the first-needed order would be lost.)
//!
//! **Bounded fetches.** Each fetch is capped at
//! [`PrefetchOptions::fetch_timeout`]; past it the tile is `Failed` and
//! its slot is free, so a source that never answers cannot stall the
//! pool. The slot is released by a guard, so a fetch that panics frees
//! it too. Dropping the [`Prefetcher`] aborts every queued and in-flight
//! fetch and frees the store.
//!
//! **Runtime.** `ensure` blocks its thread while the fetches run on `rt`.
//! `rt` must be a multi-thread runtime, or one driven by some other
//! thread: a current-thread runtime driven by the caller would never run
//! a fetch, and every frame would time out.
//!
//! **Clock.** Planning is a pure function of story time. `Instant` appears
//! only in `ensure`, where it measures the wait; what a frame waits for
//! never depends on it.
//!
//! **Late arrivals.** A tile that misses its frame's deadline keeps its
//! fetch in flight (up to the fetch timeout); if it lands, the next frame
//! that needs it uses it. A failed tile is not retried: it is named
//! `Failed` on every frame that needs it.
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)
)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::capture::{CaptureRequest, TileStore};
use crate::geo::TileId;
use crate::tile_source::TileSource;

/// The default total cap on one story fetch ([`PrefetchOptions::fetch_timeout`]).
/// The HTTP source itself has no total timeout (only connect and
/// read-idle ones, for the live map's sake); a story render bounds each
/// fetch here so a trickling server cannot hold a slot for the whole run.
pub const DEFAULT_FETCH_TIMEOUT: Duration = Duration::from_secs(20);

/// How many ancestor levels above each cover tile the plan schedules as
/// stand-ins.
pub const STAND_IN_LEVELS: u8 = 2;

/// `tile`'s ancestors, nearest first, at most [`STAND_IN_LEVELS`] up and
/// never above z0.
pub fn stand_ins(tile: TileId) -> impl Iterator<Item = TileId> {
    (1..=STAND_IN_LEVELS.min(tile.z)).map(move |up| TileId { z: tile.z - up, x: tile.x >> up, y: tile.y >> up })
}

/// The tiles one frame draws: its cover, then any dot-mask ring tiles not
/// already in it, each once; and the stand-ins for them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FrameNeed {
    /// The exact tiles: what the frame waits for.
    pub tiles: Vec<TileId>,
    /// The cover tiles' ancestors ([`stand_ins`]) not already exact tiles
    /// of this frame, each once: fetched at lower priority, drawn only in
    /// place of a missing exact tile.
    pub stand_ins: Vec<TileId>,
}

/// Every frame's tiles, planned before the first frame renders.
#[derive(Clone, Debug, Default)]
pub struct Plan {
    /// One per frame, in frame order.
    pub needs: Vec<FrameNeed>,
    /// Every distinct tile once (exact and stand-in), with the first frame
    /// that needs it, sorted by that frame, then exact tiles before
    /// stand-ins, then position in its [`FrameNeed`].
    pub order: Vec<(TileId, usize /* first frame */)>,
    /// The last frame that needs each tile, as exact tile or stand-in.
    pub last_needed: HashMap<TileId, usize>,
    /// The largest single frame's exact-tile count.
    pub max_cover: usize,
    /// Distinct tiles over the whole story (`order.len()`).
    pub total_tiles: usize,
}

/// Plan the tiles of every frame: `r.cover()` ∪ `r.mask_ring()` per
/// request, in frame order, and the cover's stand-ins. Pure: the same
/// requests give the same plan.
pub fn plan(requests: impl Iterator<Item = CaptureRequest>) -> Plan {
    let mut out = Plan::default();
    let mut seen = HashSet::new();
    for (frame, request) in requests.enumerate() {
        let cover = request.cover();
        let mut in_frame = HashSet::new();
        let tiles: Vec<TileId> =
            cover.iter().copied().chain(request.mask_ring()).filter(|t| in_frame.insert(*t)).collect();
        let stand: Vec<TileId> = cover.iter().flat_map(|t| stand_ins(*t)).filter(|t| in_frame.insert(*t)).collect();
        for &tile in tiles.iter().chain(&stand) {
            if seen.insert(tile) {
                out.order.push((tile, frame));
            }
            out.last_needed.insert(tile, frame);
        }
        out.max_cover = out.max_cover.max(tiles.len());
        out.needs.push(FrameNeed { tiles, stand_ins: stand });
    }
    out.total_tiles = out.order.len();
    out
}

/// How the prefetcher fetches and waits.
#[derive(Clone, Debug)]
pub struct PrefetchOptions {
    /// Fetches in flight at once (at least one is always allowed).
    pub concurrency: usize,
    /// Frames past the current one whose tiles are fetched ahead.
    pub lookahead_frames: usize,
    /// How long an ordinary frame waits for its tiles.
    pub tile_timeout: Duration,
    /// How long a settle frame waits for its tiles.
    pub settle_timeout: Duration,
    /// The longest one fetch may take before its tile is `Failed` and its
    /// slot freed.
    pub fetch_timeout: Duration,
    /// A frame with any missing tile is an error, not a warning.
    pub strict: bool,
    /// The raw-tile disk cache ([`crate::capture::fetch_one`]).
    pub cache_dir: PathBuf,
    /// When set, a wait checks it every [`CANCEL_POLL`] and stops with
    /// `cancelled at frame n (waiting on k tiles)`.
    pub cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
}

/// How often a frame's wait looks at [`PrefetchOptions::cancel`].
pub const CANCEL_POLL: Duration = Duration::from_millis(50);

impl PrefetchOptions {
    /// The defaults at `fps`: 16 in flight, 3 s of look-ahead, a 10 s tile
    /// timeout, a 60 s settle timeout, a 20 s total fetch cap
    /// ([`DEFAULT_FETCH_TIMEOUT`]), not strict.
    #[must_use]
    pub fn new(fps: f64, cache_dir: PathBuf) -> Self {
        let lookahead = if fps.is_finite() && fps > 0.0 { (3.0 * fps).ceil() as usize } else { 0 };
        Self {
            concurrency: 16,
            lookahead_frames: lookahead,
            tile_timeout: Duration::from_secs(10),
            settle_timeout: Duration::from_secs(60),
            fetch_timeout: DEFAULT_FETCH_TIMEOUT,
            strict: false,
            cache_dir,
            cancel: None,
        }
    }
}

/// Why a tile a frame needs is not there.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub enum MissReason {
    /// Still in flight at the frame's deadline.
    Timeout,
    /// The fetch or the decode failed, with its message.
    Failed(String),
}

/// What one [`Prefetcher::ensure`] found.
#[derive(Clone, Debug, PartialEq)]
pub struct WaitOutcome {
    /// Tiles the frame needs that are neither resident nor absent.
    pub missing: Vec<(TileId, MissReason)>,
    /// How long `ensure` blocked.
    pub waited: Duration,
}

#[derive(Default)]
struct State {
    store: TileStore,
    absent: HashSet<TileId>,
    failed: HashMap<TileId, String>,
    /// In-flight fetches, with the handle that aborts each.
    in_flight: HashMap<TileId, tokio::task::AbortHandle>,
    /// Released, not yet started, in fetch order.
    queue: VecDeque<TileId>,
    queued: HashSet<TileId>,
    /// Tiles whose `last_needed` is below this are no longer wanted.
    evicted_below: usize,
    /// Set by `Prefetcher::drop`: nothing starts or lands any more.
    closed: bool,
}

impl State {
    fn known(&self, tile: &TileId) -> bool {
        self.store.contains_key(tile)
            || self.absent.contains(tile)
            || self.failed.contains_key(tile)
            || self.in_flight.contains_key(tile)
            || self.queued.contains(tile)
    }

    fn resolved(&self, tile: &TileId) -> bool {
        self.store.contains_key(tile) || self.absent.contains(tile) || self.failed.contains_key(tile)
    }

    fn enqueue_back(&mut self, tile: TileId) {
        if !self.known(&tile) {
            self.queue.push_back(tile);
            self.queued.insert(tile);
        }
    }
}

struct Inner {
    source: Arc<dyn TileSource>,
    plan: Arc<Plan>,
    cache_dir: PathBuf,
    concurrency: usize,
    fetch_timeout: Duration,
    rt: tokio::runtime::Handle,
    state: Mutex<State>,
    arrived: Condvar,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wanted(&self, st: &State, tile: &TileId) -> bool {
        self.plan.last_needed.get(tile).is_some_and(|&last| last >= st.evicted_below)
    }
}

/// Frees a fetch's slot however its task ends: with a result, by a panic
/// inside the fetch, or by an abort. Without a result the tile is
/// `Failed`, so no frame waits on a fetch that no longer exists.
///
/// **Inert until armed (F9).** The task arms it on its first poll. A task
/// the runtime drops without ever polling it -- a spawn onto a runtime
/// that is shutting down drops the future inline, on the spawning thread,
/// which holds the state lock -- must not lock again, so an unarmed slot's
/// drop does nothing. `pump` sees such a task already finished right
/// after its spawn and records the tile itself.
struct Slot {
    inner: Arc<Inner>,
    tile: TileId,
    armed: bool,
    result: Option<anyhow::Result<Option<crate::mvt::DecodedTile>>>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut st = self.inner.lock();
        st.in_flight.remove(&self.tile);
        if st.closed {
            return;
        }
        if self.inner.wanted(&st, &self.tile) {
            match self.result.take() {
                Some(Ok(Some(decoded))) => {
                    st.store.insert(self.tile, decoded);
                }
                Some(Ok(None)) => {
                    st.absent.insert(self.tile);
                }
                Some(Err(e)) => {
                    // A source's error may carry its URL: never its credentials.
                    st.failed.insert(self.tile, crate::sprite::redact_url_credentials(&format!("{e:#}")));
                }
                None => {
                    st.failed.insert(
                        self.tile,
                        "the fetch task ended without an answer (it panicked, or the runtime shut down)".to_string(),
                    );
                }
            }
        }
        pump(&self.inner, &mut st);
        drop(st);
        self.inner.arrived.notify_all();
    }
}

/// Start queued fetches, in queue order, while a slot is free.
fn pump(inner: &Arc<Inner>, st: &mut State) {
    while !st.closed && st.in_flight.len() < inner.concurrency {
        let Some(tile) = st.queue.pop_front() else { break };
        st.queued.remove(&tile);
        if !inner.wanted(st, &tile) || st.known(&tile) {
            continue;
        }
        let mut slot = Slot { inner: Arc::clone(inner), tile, armed: false, result: None };
        let handle = inner.rt.spawn(async move {
            let _unarmed = std::mem::replace(&mut slot.armed, true);
            let cap = slot.inner.fetch_timeout;
            let fetched =
                tokio::time::timeout(cap, crate::capture::fetch_one(slot.inner.source.as_ref(), tile, &slot.inner.cache_dir))
                    .await
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("no answer in {cap:?}")));
            // Read by `Slot::drop`, which rustc's unused-assignment lint
            // cannot see; `replace` says the same without the warning.
            let _none = slot.result.replace(fetched.and_then(|bytes| bytes.map(crate::mvt::decode).transpose()));
            // `slot` drops here: the result lands and the slot is freed.
        });
        // Finished already, while this thread holds the lock that any
        // armed slot needs to finish: the runtime dropped it unpolled
        // (it is shut down). Name the tile rather than leave a slot that
        // will never be freed.
        if handle.is_finished() {
            st.failed.insert(tile, "the tokio runtime is shut down".to_string());
            continue;
        }
        st.in_flight.insert(tile, handle.abort_handle());
    }
}

/// Read access to the decoded tiles, for the render. Holds the lock:
/// completions wait until it is dropped, so hold it for one frame only.
///
/// **Never hold it across a runtime shutdown** (dropping or shutting down
/// the tokio runtime the prefetcher fetches on). A shutdown drops every
/// in-flight task on the shutting-down thread; an armed [`Slot`] locks the
/// state in its `Drop`, and this guard holds that lock, so the shutdown
/// would wait forever on a lock its own thread holds. Drop the guard
/// first (end the frame), then the runtime.
pub struct TileStoreGuard<'a>(MutexGuard<'a, State>);

impl Deref for TileStoreGuard<'_> {
    type Target = TileStore;
    fn deref(&self) -> &TileStore {
        &self.0.store
    }
}

/// The look-ahead tile fetcher over a [`Plan`]. See the module docs.
pub struct Prefetcher {
    inner: Arc<Inner>,
    opts: PrefetchOptions,
    /// Entries of `plan.order` before this index have been released.
    released: usize,
    /// The frame the last `ensure` was for: a later call for an earlier
    /// frame (a second range, a still) re-walks the order from the start
    /// so that frame's look-ahead window is queued again.
    last_n: Option<usize>,
}

impl Prefetcher {
    /// A prefetcher that fetches through `source` on `rt`, caching raw
    /// tiles under `opts.cache_dir`. Nothing is fetched until the first
    /// [`ensure`](Self::ensure).
    ///
    /// `rt` must be a multi-thread runtime, or one driven by a thread
    /// other than the one calling `ensure` (which blocks): see the module
    /// docs.
    pub fn new(source: Arc<dyn TileSource>, plan: Arc<Plan>, opts: PrefetchOptions, rt: tokio::runtime::Handle) -> Self {
        // A cache directory that cannot be made fails each fetch's write,
        // so every tile is named `Failed` with the reason; nothing hides.
        let _ = std::fs::create_dir_all(&opts.cache_dir);
        let inner = Arc::new(Inner {
            source,
            plan,
            cache_dir: opts.cache_dir.clone(),
            concurrency: opts.concurrency.max(1),
            fetch_timeout: opts.fetch_timeout,
            rt,
            state: Mutex::new(State::default()),
            arrived: Condvar::new(),
        });
        Self { inner, opts, released: 0, last_n: None }
    }

    /// Frame n: keep the window [n, n+L] in flight; block until need(n) ⊆ store ∪ absent, or the deadline.
    ///
    /// The window starts at `n`, not at frame 0: a first call at a late
    /// frame (a still, a frame range) fetches nothing an earlier frame
    /// alone needs. The deadline is the tile timeout, or the settle
    /// timeout when `settle`. A failed tile stops the wait for itself at
    /// once, then the wait covers its stand-ins (and an absent tile's) so
    /// the render has a settled ancestor to draw.
    ///
    /// # Errors
    /// Only when a tile is missing and the options are `strict` or this
    /// is a settle frame: `frame {n}: tiles missing: z/x/y (reason), ...`.
    /// Also for an `n` past the plan's last frame.
    pub fn ensure(&mut self, n: usize, settle: bool) -> anyhow::Result<WaitOutcome> {
        let settle_timeout = self.opts.settle_timeout;
        self.ensure_with(n, settle.then_some(settle_timeout))
    }

    /// [`ensure`](Self::ensure) with the settle point's own timeout:
    /// `Some(timeout)` makes frame `n` a settle frame waiting up to
    /// `timeout` (a script's `settle.timeout_s`), `None` an ordinary one
    /// waiting up to the tile timeout.
    ///
    /// # Errors
    /// As [`ensure`](Self::ensure), and `cancelled at frame n (waiting
    /// on k tiles)` when [`PrefetchOptions::cancel`] is set during the
    /// wait (seen within [`CANCEL_POLL`]).
    pub fn ensure_with(&mut self, n: usize, settle_timeout: Option<Duration>) -> anyhow::Result<WaitOutcome> {
        let settle = settle_timeout.is_some();
        let start = Instant::now();
        let plan = Arc::clone(&self.inner.plan);
        let need = plan
            .needs
            .get(n)
            .ok_or_else(|| anyhow::anyhow!("frame {n} is past the plan's {} frames", plan.needs.len()))?;
        let horizon = n.saturating_add(self.opts.lookahead_frames);
        if self.last_n.is_some_and(|last| n < last) {
            self.released = 0;
        }
        self.last_n = Some(n);
        let mut st = self.inner.lock();
        // Frame n and everything after it is wanted again, even after an
        // `evict_through` past n.
        st.evicted_below = st.evicted_below.min(n);
        // Frame n's own tiles first, exact before stand-ins. Monotonic use
        // never queues anything here: the window released them already.
        for tile in need.tiles.iter().chain(&need.stand_ins).rev() {
            if !st.known(tile) {
                st.queue.push_front(*tile);
                st.queued.insert(*tile);
            }
        }
        while let Some(&(tile, first)) = plan.order.get(self.released) {
            if first > horizon {
                break;
            }
            self.released += 1;
            // Only tiles some frame from n on still needs (F1).
            if plan.last_needed.get(&tile).is_some_and(|&last| last >= n) && self.inner.wanted(&st, &tile) {
                st.enqueue_back(tile);
            }
        }
        pump(&self.inner, &mut st);

        let timeout = settle_timeout.unwrap_or(self.opts.tile_timeout);
        // `Instant + Duration` panics on overflow (R1): a timeout too long
        // to add is no deadline at all (the wait ends on its tiles or a
        // cancel), never a panic.
        let deadline = start.checked_add(timeout);
        let cancel = self.opts.cancel.clone();
        let cancelled = || cancel.as_ref().is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed));
        loop {
            let exact_done = need.tiles.iter().all(|t| st.resolved(t));
            // The stand-ins of an exact tile that is settled but not
            // drawn -- failed, or absent (`draw_order` stands an ancestor
            // in for an absent tile too) -- settled nearest first:
            // `draw_order` draws the NEAREST resident ancestor, so the
            // wait ends at the first resident one only once every nearer
            // one is settled (absent or failed). Which ancestor draws then
            // never depends on fetch timing (F10). Only planned stand-ins
            // count: a mask-ring tile has none, and an unplanned ancestor
            // is never fetched.
            let stand_ins_done = need.tiles.iter().filter(|t| st.failed.contains_key(t) || st.absent.contains(t)).all(|t| {
                for a in stand_ins(*t).filter(|a| plan.last_needed.contains_key(a)) {
                    if st.store.contains_key(&a) {
                        return true;
                    }
                    if !st.resolved(&a) {
                        return false;
                    }
                }
                true
            });
            let now = Instant::now();
            if (exact_done && stand_ins_done) || deadline.is_some_and(|d| now >= d) {
                break;
            }
            if cancelled() {
                let waiting = need.tiles.iter().filter(|t| !st.resolved(t)).count();
                anyhow::bail!("cancelled at frame {n} (waiting on {waiting} tiles)");
            }
            // No deadline: wake every poll period (a cancel may come).
            let left = deadline.map_or(CANCEL_POLL, |d| d - now);
            let slice = if cancel.is_some() { left.min(CANCEL_POLL) } else { left };
            st = self.inner.arrived.wait_timeout(st, slice).unwrap_or_else(PoisonError::into_inner).0;
        }
        let missing: Vec<(TileId, MissReason)> = need
            .tiles
            .iter()
            .filter(|t| !st.store.contains_key(t) && !st.absent.contains(t))
            .map(|t| (*t, st.failed.get(t).map_or(MissReason::Timeout, |m| MissReason::Failed(m.clone()))))
            .collect();
        drop(st);
        let waited = start.elapsed();
        if !missing.is_empty() && (self.opts.strict || settle) {
            let listed: Vec<String> = missing
                .iter()
                .map(|(t, r)| match r {
                    MissReason::Timeout => format!("{}/{}/{} (timeout)", t.z, t.x, t.y),
                    MissReason::Failed(m) => format!("{}/{}/{} (failed: {m})", t.z, t.x, t.y),
                })
                .collect();
            let kind = if settle { " at a settle point" } else { "" };
            anyhow::bail!("frame {n}: tiles missing{kind} after {waited:?}: {}", listed.join(", "));
        }
        Ok(WaitOutcome { missing, waited })
    }

    /// Read access to the decoded tiles for the render.
    pub fn store(&self) -> TileStoreGuard<'_> {
        TileStoreGuard(self.inner.lock())
    }

    /// Drop tiles whose last_needed < n: resident, absent, failed and
    /// queued ones. A fetch still in flight for one is discarded when it
    /// lands.
    pub fn evict_through(&mut self, n: usize) {
        let inner = &self.inner;
        let mut st = inner.lock();
        st.evicted_below = st.evicted_below.max(n);
        let below = st.evicted_below;
        let keep = |t: &TileId| inner.plan.last_needed.get(t).is_some_and(|&last| last >= below);
        st.store.retain(|t, _| keep(t));
        st.absent.retain(|t| keep(t));
        st.failed.retain(|t, _| keep(t));
        st.queue.retain(|t| keep(t));
        st.queued.retain(|t| keep(t));
    }

    /// Decoded tiles held now.
    pub fn resident(&self) -> usize {
        self.inner.lock().store.len()
    }

    /// Fetches in flight now.
    pub fn in_flight(&self) -> usize {
        self.inner.lock().in_flight.len()
    }
}

impl Drop for Prefetcher {
    /// Stop: nothing queued starts, every in-flight fetch is aborted, and
    /// the decoded tiles are freed now rather than when the last task
    /// lets go of the shared state.
    fn drop(&mut self) {
        let mut st = self.inner.lock();
        st.closed = true;
        st.queue.clear();
        st.queued.clear();
        let handles: Vec<tokio::task::AbortHandle> = st.in_flight.values().cloned().collect();
        st.store = TileStore::new();
        st.absent.clear();
        st.failed.clear();
        // Abort outside the lock: an aborted task's `Slot` locks it to
        // free its slot (and, being closed, does nothing else).
        drop(st);
        for handle in handles {
            handle.abort();
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::story::fixture::FakeSource;

    fn requests(n: usize) -> Vec<CaptureRequest> {
        (0..n).map(|i| CaptureRequest { center_lon: 103.80 + 0.002 * i as f64, center_lat: 1.30, zoom: 14.2, width: 480, height: 270, flat: true, ..CaptureRequest::default() }).collect()
    }
    fn run(fake: FakeSource, n: usize, opts: impl FnOnce(&mut PrefetchOptions)) -> (Vec<anyhow::Result<WaitOutcome>>, Arc<FakeSource>, usize) {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut o = PrefetchOptions { concurrency: 8, lookahead_frames: 5, tile_timeout: Duration::from_millis(200), settle_timeout: Duration::from_secs(2), fetch_timeout: Duration::from_secs(60), strict: false, cache_dir: dir.path().to_path_buf(), cancel: None };
        opts(&mut o);
        let plan = Arc::new(plan(requests(n).into_iter()));
        let fake = Arc::new(fake);
        let mut p = Prefetcher::new(fake.clone(), plan.clone(), o, rt.handle().clone());
        let mut out = Vec::new();
        let mut max_resident = 0;
        for i in 0..n { out.push(p.ensure(i, false)); max_resident = max_resident.max(p.resident()); p.evict_through(i); }
        (out, fake, max_resident)
    }
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap()
    }
    /// Polls `done` for up to 10 s; the bound only guards a hang.
    fn eventually(done: impl Fn() -> bool) -> bool {
        let until = Instant::now() + Duration::from_secs(10);
        while Instant::now() < until {
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        done()
    }

    #[test] fn a_all_tiles_arrive_and_nothing_is_missing() {
        let (out, _, _) = run(FakeSource::fixture(Duration::from_millis(50)), 20, |_| {});
        assert!(out.iter().all(|o| o.as_ref().unwrap().missing.is_empty()));
        // Deviation from the brief's 60 ms (ruling C1), widened again for
        // load (F5): `run` renders nothing between frames, so a tile
        // first needed at n + 5 is released at frame n and needed ~0 ms
        // later; that frame waits its 50 ms fetch plus a 5-7 ms debug
        // decode (62 and 68 ms seen idle). The bound is "about one fetch,
        // never the 200 ms deadline", with room for a loaded machine; the
        // look-ahead proof is `the_lookahead_hides_fetch_latency_behind_render_time`.
        let waits: Vec<u128> = out.iter().map(|o| o.as_ref().unwrap().waited.as_millis()).collect();
        assert!(out[10..].iter().all(|o| o.as_ref().unwrap().waited < Duration::from_millis(150)), "the look-ahead should have filled: {waits:?}");
    }
    #[test] fn b_a_tile_that_never_arrives_is_named_on_exactly_the_frames_that_need_it() {
        let reqs = requests(20);
        let victim = reqs[5].cover()[0];
        let needing: Vec<usize> = reqs.iter().enumerate().filter(|(_, r)| r.cover().contains(&victim)).map(|(i, _)| i).collect();
        let (out, _, _) = run(FakeSource { never: [victim].into(), ..FakeSource::fixture(Duration::ZERO) }, 20, |_| {});
        for (i, o) in out.iter().enumerate() {
            let named = o.as_ref().unwrap().missing.iter().any(|(t, r)| *t == victim && *r == MissReason::Timeout);
            assert_eq!(named, needing.contains(&i), "frame {i}");
        }
    }
    #[test] fn c_strict_turns_a_timeout_into_an_error_naming_frame_and_tile() {
        let victim = requests(1)[0].cover()[0];
        let (out, _, _) = run(FakeSource { never: [victim].into(), ..FakeSource::fixture(Duration::ZERO) }, 1, |o| o.strict = true);
        let msg = format!("{:#}", out[0].as_ref().unwrap_err());
        assert!(msg.contains("frame 0") && msg.contains(&format!("{}/{}/{}", victim.z, victim.x, victim.y)), "{msg}");
    }
    #[test] fn d_an_absent_tile_is_not_missing_and_not_waited_for() {
        let victim = requests(1)[0].cover()[0];
        // F5: a 5 s deadline, and "not waited for" is "returned well
        // before it" -- a relative bound a loaded machine cannot break.
        let (out, _, _) = run(FakeSource { absent: [victim].into(), ..FakeSource::fixture(Duration::ZERO) }, 1, |o| o.tile_timeout = Duration::from_secs(5));
        let o = out[0].as_ref().unwrap();
        assert!(o.missing.is_empty() && o.waited < Duration::from_millis(2500), "{o:?}");
    }
    #[test] fn e_a_settle_frame_waits_past_the_tile_timeout() {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let reqs = requests(1);
        let slow = reqs[0].cover()[0];
        let fake = Arc::new(FakeSource { slow: [(slow, Duration::from_millis(600))].into(), ..FakeSource::fixture(Duration::ZERO) });
        // F5: settle timeout 10 s (the brief's 2 s) so load cannot fail it.
        let mut p = Prefetcher::new(fake, Arc::new(plan(reqs.into_iter())), PrefetchOptions { concurrency: 4, lookahead_frames: 1, tile_timeout: Duration::from_millis(200), settle_timeout: Duration::from_secs(10), fetch_timeout: Duration::from_secs(60), strict: false, cache_dir: dir.path().into(), cancel: None }, rt.handle().clone());
        let o = p.ensure(0, true).unwrap();
        assert!(o.missing.is_empty() && o.waited >= Duration::from_millis(500));
    }
    #[test] fn f_resident_tiles_stay_within_the_lookahead_union_plus_ten_percent() {
        let reqs = requests(40);
        let pl = plan(reqs.clone().into_iter());
        // The union counts each frame's stand-ins too (ruling F3: they
        // are planned and resident like exact tiles).
        let window_max = (0..40).map(|n| { let mut s = HashSet::new(); for need in &pl.needs[n..(n + 6).min(40)] { s.extend(need.tiles.iter().chain(&need.stand_ins).copied()); } s.len() }).max().unwrap();
        let (_, _, max_resident) = run(FakeSource::fixture(Duration::ZERO), 40, |_| {});
        assert!(max_resident as f64 <= window_max as f64 * 1.1 + 1.0, "{max_resident} vs {window_max}");
    }
    #[test] fn g_fetches_go_out_in_first_needed_order() {
        let (_, fake, _) = run(FakeSource::fixture(Duration::ZERO), 20, |o| o.concurrency = 1);
        let log = fake.log.lock().unwrap().clone();
        let pl = plan(requests(20).into_iter());
        let first: HashMap<TileId, usize> = pl.order.iter().copied().collect();
        assert!(log.windows(2).all(|w| first[&w[0]] <= first[&w[1]]), "{log:?}");
    }

    // ---- Beyond the brief's seven. ----

    /// `g` holds only frame order, and at its slow pan most tiles share a
    /// first frame (a `pop_back` queue passes it). One request at a time,
    /// over a fast pan with a long look-ahead, the log must follow
    /// `plan.order` exactly: a subsequence of it (a queued tile no later
    /// frame needs is skipped), in its order, holding every exact tile.
    #[test]
    fn one_at_a_time_the_request_log_follows_the_plan_order_exactly() {
        let reqs: Vec<CaptureRequest> = (0..20).map(|i| CaptureRequest { center_lon: 103.80 + 0.02 * i as f64, ..requests(1)[0].clone() }).collect();
        let pl = Arc::new(plan(reqs.into_iter()));
        assert!(pl.order.iter().any(|(_, f)| *f > 0), "tiles enter after frame 0");
        let rt = runtime();
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeSource::fixture(Duration::ZERO));
        let mut o = PrefetchOptions::new(30.0, dir.path().into());
        o.concurrency = 1;
        o.lookahead_frames = 10;
        let mut p = Prefetcher::new(fake.clone(), pl.clone(), o, rt.handle().clone());
        for i in 0..20 {
            assert!(p.ensure(i, false).unwrap().missing.is_empty());
            p.evict_through(i);
        }
        let position: HashMap<TileId, usize> = pl.order.iter().enumerate().map(|(i, (t, _))| (*t, i)).collect();
        let log = fake.log.lock().unwrap().clone();
        assert!(log.windows(2).all(|w| position[&w[0]] < position[&w[1]]), "{log:?}");
        for need in &pl.needs {
            assert!(need.tiles.iter().all(|t| log.contains(t)));
        }
    }

    /// 30 ms of render per frame and 50 ms fetches: five frames of
    /// look-ahead (150 ms of lead) hide the fetches, none does not. F5:
    /// relative -- the look-ahead run waits far less in total than the
    /// control -- rather than a millisecond bound per frame.
    #[test]
    fn the_lookahead_hides_fetch_latency_behind_render_time() {
        let waits = |lookahead: usize| {
            let rt = runtime();
            let dir = tempfile::tempdir().unwrap();
            let pl = Arc::new(plan(requests(20).into_iter()));
            let mut o = PrefetchOptions::new(30.0, dir.path().into());
            o.lookahead_frames = lookahead;
            let mut p = Prefetcher::new(Arc::new(FakeSource::fixture(Duration::from_millis(50))), pl, o, rt.handle().clone());
            (0..20)
                .map(|i| {
                    let o = p.ensure(i, false).unwrap();
                    assert!(o.missing.is_empty());
                    std::thread::sleep(Duration::from_millis(30)); // the render
                    p.evict_through(i);
                    o.waited
                })
                .collect::<Vec<_>>()
        };
        let ahead: Duration = waits(5)[1..].iter().sum();
        let none: Duration = waits(0)[1..].iter().sum();
        assert!(none >= Duration::from_millis(45), "the control must wait on new tiles: {none:?}");
        assert!(ahead * 3 < none, "look-ahead {ahead:?} vs none {none:?}");
    }

    /// The plan: exact tiles are the cover; stand-ins are the cover's
    /// ancestors up to two levels, minus exact tiles; order holds each
    /// tile once at its first frame, exact before stand-ins within a
    /// frame; last_needed is the maximum frame; the counts add up; and
    /// planning twice gives the same plan (pure).
    #[test]
    fn the_plan_is_first_needed_ordered_and_pure() {
        let reqs = requests(30);
        let pl = plan(reqs.clone().into_iter());
        assert_eq!(pl.needs.len(), 30);
        assert_eq!(pl.total_tiles, pl.order.len());
        let distinct: HashSet<TileId> = pl.needs.iter().flat_map(|n| n.tiles.iter().chain(&n.stand_ins).copied()).collect();
        assert_eq!(distinct.len(), pl.total_tiles);
        assert!(pl.order.windows(2).all(|w| w[0].1 <= w[1].1));
        for (i, r) in reqs.iter().enumerate() {
            let need = &pl.needs[i];
            assert_eq!(need.tiles, r.cover(), "no dots: the exact tiles are the cover");
            let mut want: Vec<TileId> = Vec::new();
            for a in r.cover().into_iter().flat_map(stand_ins) {
                if !want.contains(&a) && !need.tiles.contains(&a) { want.push(a); }
            }
            assert_eq!(need.stand_ins, want);
            assert!(need.stand_ins.iter().all(|a| a.z + 2 >= 14 && a.z < 14), "{:?}", need.stand_ins);
            for t in need.tiles.iter().chain(&need.stand_ins) {
                assert!(pl.order.iter().find(|(o, _)| o == t).unwrap().1 <= i);
                assert!(pl.last_needed[t] >= i);
            }
            // Exact before stand-ins among the tiles first needed here.
            let here: Vec<TileId> = pl.order.iter().filter(|(_, f)| *f == i).map(|(t, _)| *t).collect();
            let last_exact = here.iter().rposition(|t| need.tiles.contains(t));
            let first_stand = here.iter().position(|t| need.stand_ins.contains(t));
            if let (Some(e), Some(s)) = (last_exact, first_stand) { assert!(e < s, "frame {i}: {here:?}"); }
        }
        assert!(pl.needs[0].stand_ins.len() >= 2, "z13 and z12 parents: {:?}", pl.needs[0].stand_ins);
        assert_eq!(pl.max_cover, pl.needs.iter().map(|n| n.tiles.len()).max().unwrap());
        let again = plan(reqs.into_iter());
        assert_eq!((again.needs, again.order), (pl.needs, pl.order));
        assert_eq!(stand_ins(TileId { z: 1, x: 1, y: 0 }).collect::<Vec<_>>(), vec![TileId { z: 0, x: 0, y: 0 }]);
        assert_eq!(stand_ins(TileId { z: 0, x: 0, y: 0 }).count(), 0);
    }

    /// A failed tile is named `Failed` with the source's message well
    /// before the deadline; a settle frame turns it into an error.
    #[test]
    fn a_failed_tile_is_named_failed_without_waiting_and_fails_a_settle_frame() {
        let victim = requests(1)[0].cover()[0];
        let (out, _, _) = run(FakeSource { fail: [victim].into(), ..FakeSource::fixture(Duration::ZERO) }, 1, |o| o.tile_timeout = Duration::from_secs(5));
        let o = out[0].as_ref().unwrap();
        assert!(o.waited < Duration::from_millis(2500), "{:?}", o.waited);
        assert!(matches!(&o.missing[..], [(t, MissReason::Failed(m))] if *t == victim && m.contains("scripted failure")), "{:?}", o.missing);

        let rt = runtime();
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeSource { fail: [victim].into(), ..FakeSource::fixture(Duration::ZERO) });
        let mut p = Prefetcher::new(fake, Arc::new(plan(requests(1).into_iter())), PrefetchOptions::new(30.0, dir.path().into()), rt.handle().clone());
        let msg = format!("{:#}", p.ensure(0, true).unwrap_err());
        assert!(msg.contains("frame 0") && msg.contains("settle") && msg.contains("failed: scripted failure"), "{msg}");
    }

    /// A tile that times out keeps its fetch in flight, and the next frame
    /// that needs it uses the late arrival without a second request.
    ///
    /// The late tile is HELD until frame 0 has given up on it, not merely
    /// slow: with a 300 ms delay against a 50 ms tile timeout, a loaded CI
    /// runner timed out frame 0's other, instant tile as well. Held, the
    /// late tile cannot land early, and the 1 s timeout is the instant
    /// tiles' margin, not a race.
    #[test]
    fn a_late_arrival_is_used_by_the_next_frame_that_needs_it() {
        let reqs = requests(2);
        let late = reqs[0].cover()[0];
        assert!(reqs[1].cover().contains(&late), "both frames need the tile");
        let rt = runtime();
        let dir = tempfile::tempdir().unwrap();
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let fake = Arc::new(FakeSource { held: [(late, gate.clone())].into(), ..FakeSource::fixture(Duration::ZERO) });
        let mut o = PrefetchOptions::new(30.0, dir.path().into());
        o.tile_timeout = Duration::from_secs(1);
        let mut p = Prefetcher::new(fake.clone(), Arc::new(plan(reqs.into_iter())), o, rt.handle().clone());
        let first = p.ensure(0, false).unwrap();
        assert_eq!(first.missing, vec![(late, MissReason::Timeout)]);
        gate.add_permits(1);
        // The settle deadline (60 s) only bounds a hang.
        let second = p.ensure(1, true).unwrap();
        assert!(second.missing.is_empty(), "{:?}", second.missing);
        assert!(p.store().contains_key(&late));
        assert_eq!(fake.log.lock().unwrap().iter().filter(|t| **t == late).count(), 1, "one request, not a retry");
    }

    /// The disk cache: a second prefetcher over the same cache directory
    /// asks the source for nothing, and the tiles it serves decode equal.
    #[test]
    fn a_warm_cache_dir_serves_every_tile_without_the_source() {
        let rt = runtime();
        let dir = tempfile::tempdir().unwrap();
        let pl = Arc::new(plan(requests(5).into_iter()));
        let pass = || {
            let fake = Arc::new(FakeSource::fixture(Duration::ZERO));
            let mut p = Prefetcher::new(fake.clone(), pl.clone(), PrefetchOptions::new(30.0, dir.path().into()), rt.handle().clone());
            for i in 0..5 {
                assert!(p.ensure(i, false).unwrap().missing.is_empty());
            }
            // Stand-ins are not waited for: let them land.
            assert!(eventually(|| p.resident() == pl.total_tiles));
            let hashes: HashMap<TileId, u64> = p.store().iter().map(|(t, d)| (*t, d.data_hash)).collect();
            let asked = fake.log.lock().unwrap().len();
            (asked, hashes)
        };
        let (cold, a) = pass();
        let (warm, b) = pass();
        assert_eq!(cold, pl.total_tiles);
        assert_eq!(warm, 0, "every tile from the cache");
        assert_eq!(a, b);
    }

    /// Eviction drops exactly the tiles no frame from `n` on needs.
    #[test]
    fn evict_through_drops_exactly_the_tiles_no_later_frame_needs() {
        let rt = runtime();
        let dir = tempfile::tempdir().unwrap();
        let pl = Arc::new(plan(requests(30).into_iter()));
        let mut o = PrefetchOptions::new(30.0, dir.path().into());
        o.lookahead_frames = 29;
        let mut p = Prefetcher::new(Arc::new(FakeSource::fixture(Duration::ZERO)), pl.clone(), o, rt.handle().clone());
        for i in 0..30 {
            p.ensure(i, false).unwrap();
        }
        assert!(eventually(|| p.resident() == pl.total_tiles));
        p.evict_through(20);
        let kept: HashSet<TileId> = p.store().keys().copied().collect();
        let want: HashSet<TileId> = pl.last_needed.iter().filter(|(_, &l)| l >= 20).map(|(t, _)| *t).collect();
        assert_eq!(kept, want);
        assert!(want.len() < pl.total_tiles, "the test evicts something");
    }

    /// Going back to an evicted frame re-fetches its tiles rather than
    /// reporting them missing.
    #[test]
    fn a_frame_before_the_eviction_point_is_fetched_again() {
        let rt = runtime();
        let dir = tempfile::tempdir().unwrap();
        let pl = Arc::new(plan(requests(30).into_iter()));
        let mut p = Prefetcher::new(Arc::new(FakeSource::fixture(Duration::ZERO)), pl, PrefetchOptions::new(30.0, dir.path().into()), rt.handle().clone());
        for i in 0..30 {
            p.ensure(i, false).unwrap();
            p.evict_through(i);
        }
        let o = p.ensure(0, false).unwrap();
        assert!(o.missing.is_empty(), "{:?}", o.missing);
    }

    /// F1: a fresh prefetcher's window starts at the first frame asked
    /// for. A still at frame 20 (`--at`) fetches only tiles some frame
    /// from 20 on needs, and nothing past the window.
    #[test]
    fn a_still_at_a_late_frame_fetches_only_its_window() {
        let reqs: Vec<CaptureRequest> = (0..40).map(|i| CaptureRequest { center_lon: 103.80 + 0.02 * i as f64, ..requests(1)[0].clone() }).collect();
        let pl = Arc::new(plan(reqs.into_iter()));
        let rt = runtime();
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeSource::fixture(Duration::ZERO));
        let mut o = PrefetchOptions::new(30.0, dir.path().into());
        o.concurrency = 1;
        o.lookahead_frames = 3;
        let mut p = Prefetcher::new(fake.clone(), pl.clone(), o, rt.handle().clone());
        assert!(p.ensure(20, false).unwrap().missing.is_empty());
        assert!(eventually(|| p.in_flight() == 0 && p.inner.lock().queue.is_empty()));
        let first: HashMap<TileId, usize> = pl.order.iter().copied().collect();
        let log = fake.log.lock().unwrap().clone();
        assert!(!log.is_empty());
        for t in &log {
            assert!(pl.last_needed[t] >= 20, "{t:?} is needed only before frame 20");
            assert!(first[t] <= 23, "{t:?} is past the window");
        }
        assert!(pl.order.iter().any(|(t, f)| *f < 20 && pl.last_needed[t] < 20), "the test has an earlier-only tile to skip");
    }

    /// F2: a source that never answers, on more tiles than there are
    /// slots, cannot stall the pool: each fetch is cut at the fetch
    /// timeout, named `Failed`, and its slot goes to the next tile.
    #[test]
    fn a_hung_source_frees_its_slots_at_the_fetch_timeout() {
        let need = plan(requests(1).into_iter()).needs[0].tiles.clone();
        assert!(need.len() >= 3, "{need:?}");
        let hung: HashSet<TileId> = need[..2].iter().copied().collect();
        let rt = runtime();
        let dir = tempfile::tempdir().unwrap();
        let mut o = PrefetchOptions::new(30.0, dir.path().into());
        o.concurrency = 1;
        o.fetch_timeout = Duration::from_millis(100);
        let mut p = Prefetcher::new(Arc::new(FakeSource { never: hung.clone(), ..FakeSource::fixture(Duration::ZERO) }), Arc::new(plan(requests(1).into_iter())), o, rt.handle().clone());
        let out = p.ensure(0, false).unwrap();
        let failed: HashSet<TileId> = out.missing.iter().filter(|(_, r)| matches!(r, MissReason::Failed(m) if m.contains("no answer"))).map(|(t, _)| *t).collect();
        assert_eq!(failed, hung, "{:?}", out.missing);
        assert!(need[2..].iter().all(|t| p.store().contains_key(t)), "the tiles behind the hung ones arrived");
        assert!(eventually(|| p.in_flight() == 0));
    }

    /// F2: a fetch that panics frees its slot and names its tile, and the
    /// tiles behind it (one slot) still arrive.
    #[test]
    fn a_panicking_fetch_frees_its_slot() {
        let need = plan(requests(1).into_iter()).needs[0].tiles.clone();
        let rt = runtime();
        let dir = tempfile::tempdir().unwrap();
        let mut o = PrefetchOptions::new(30.0, dir.path().into());
        o.concurrency = 1;
        let fake = FakeSource { panic: [need[0]].into(), ..FakeSource::fixture(Duration::ZERO) };
        let mut p = Prefetcher::new(Arc::new(fake), Arc::new(plan(requests(1).into_iter())), o, rt.handle().clone());
        let out = p.ensure(0, false).unwrap();
        assert!(matches!(&out.missing[..], [(t, MissReason::Failed(m))] if *t == need[0] && m.contains("panicked")), "{:?}", out.missing);
        assert!(need[1..].iter().all(|t| p.store().contains_key(t)));
        assert!(eventually(|| p.in_flight() == 0));
    }

    /// F2: dropping the prefetcher stops fetching (no new request after
    /// the drop), aborts what is in flight -- a hung fetch included -- and
    /// lets the shared state go.
    #[test]
    fn dropping_the_prefetcher_stops_fetching_and_frees_the_state() {
        let pl = Arc::new(plan(requests(20).into_iter()));
        let hung = pl.needs[0].tiles[0];
        let rt = runtime();
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeSource { never: [hung].into(), ..FakeSource::fixture(Duration::from_millis(20)) });
        let mut o = PrefetchOptions::new(30.0, dir.path().into());
        o.concurrency = 2;
        o.lookahead_frames = 19;
        o.tile_timeout = Duration::from_millis(30);
        let mut p = Prefetcher::new(fake.clone(), pl.clone(), o, rt.handle().clone());
        let _ = p.ensure(0, false).unwrap();
        let weak = Arc::downgrade(&p.inner);
        let queued = p.inner.lock().queue.len();
        assert!(queued + p.in_flight() > 1, "work was still pending at the drop");
        drop(p);
        assert!(eventually(|| weak.upgrade().is_none()), "a task still holds the shared state");
        let after = fake.log.lock().unwrap().len();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(fake.log.lock().unwrap().len(), after, "a request went out after the drop");
        assert!(after < pl.total_tiles, "the drop stopped the rest");
    }

    /// F3 (spec §4.3, §8.4(b)): an exact tile that fails still draws --
    /// as its planned ancestor, clipped to its box -- so the frame has no
    /// more clear-colour pixels than the all-present frame. The control
    /// fails the ancestors too and gets the hole.
    #[test]
    fn a_failed_tile_draws_its_ancestor_not_a_hole() {
        let _refused = crate::symbol_pass::LINE_LABELS_REFUSED_TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let request = CaptureRequest { clear: crate::capture::Clear::SENTINEL, backdrop: false, ..requests(1)[0].clone() };
        let victim = request.cover()[0];
        let style = crate::style_spec::parse_and_compile(&std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/liberty-style.json")).unwrap()).unwrap();
        let mut session = crate::capture::CaptureSession::new_blocking(None).expect("device");
        let rt = runtime();
        let mut frame = |fail: HashSet<TileId>| {
            let dir = tempfile::tempdir().unwrap();
            let fake = FakeSource { fail: fail.clone(), ..FakeSource::fixture(Duration::ZERO) };
            let mut p = Prefetcher::new(Arc::new(fake), Arc::new(plan(std::iter::once(request.clone()))), PrefetchOptions::new(30.0, dir.path().into()), rt.handle().clone());
            let out = p.ensure(0, false).unwrap();
            let want: Vec<TileId> = request.cover().into_iter().filter(|t| fail.contains(t)).collect();
            assert_eq!(out.missing.iter().map(|(t, _)| *t).collect::<Vec<_>>(), want);
            let capture = session.render(&request, &p.store(), &style).unwrap();
            let holes = capture.count_matching([255, 0, 255, 255], 2);
            (capture.tiles_missing.clone(), holes)
        };
        // The all-present baseline is 2 833 sentinel px, not 0, and that
        // is not a hole (F12, dumped and looked at): the capture never
        // draws a style's `background` layer as tile geometry --
        // `Clear::Style` paints it as the clear colour, and
        // `Clear::Sentinel` replaces it. Land where the fixture tile has no
        // landcover polygon (a strip along the bottom of this view and a
        // corner at the top right; roads and buildings are drawn across
        // it, `tiles_missing` is empty) therefore shows the sentinel. So
        // "no hole" is measured against that baseline: the stand-in frame
        // is near it (3 348), a real hole is a whole tile more (51 255).
        let (base_missing, base) = frame(HashSet::new());
        assert!(base_missing.is_empty());
        let (missing, holes) = frame([victim].into());
        let (control_missing, control) = frame(std::iter::once(victim).chain(stand_ins(victim)).collect());
        assert!(missing.is_empty(), "the ancestor stood in: {missing:?}");
        assert_eq!(control_missing, vec![victim], "control: nothing stood in");
        let hole = control.saturating_sub(base);
        assert!(hole > 20_000, "the control's hole is a tile's worth of pixels: {control} vs {base}");
        assert!(holes.saturating_sub(base) * 10 < hole, "stand-in {holes} vs baseline {base} vs hole {control}");
    }

    /// F9: the runtime dropped BEFORE the prefetcher, with work queued
    /// and in flight, then an `ensure` after the shutdown and the drop:
    /// every step returns (a spawn onto the closed runtime drops its task
    /// inline under the state lock, which must not deadlock). Run on a
    /// helper thread joined with a deadline, so a regression fails rather
    /// than hangs the suite.
    #[test]
    fn a_runtime_shut_down_first_never_deadlocks_the_prefetcher() {
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = runtime();
            let dir = tempfile::tempdir().unwrap();
            let mut o = PrefetchOptions::new(30.0, dir.path().into());
            o.concurrency = 1;
            o.lookahead_frames = 19;
            o.tile_timeout = Duration::from_millis(100);
            let fake = Arc::new(FakeSource::fixture(Duration::from_millis(50)));
            let mut p = Prefetcher::new(fake, Arc::new(plan(requests(20).into_iter())), o, rt.handle().clone());
            let _ = p.ensure(0, false).unwrap();
            let queued = p.inner.lock().queue.len();
            assert!(queued > 0, "work was queued at the shutdown");
            drop(rt);
            // After the shutdown: returns, and names what it could not get.
            let after = p.ensure(5, false).unwrap();
            drop(p);
            done.send(after).unwrap();
        });
        let after = finished.recv_timeout(Duration::from_secs(30)).expect("the prefetcher deadlocked after the runtime shut down");
        assert!(after.missing.iter().all(|(_, r)| matches!(r, MissReason::Failed(_) | MissReason::Timeout)), "{after:?}");
    }

    /// F10: an absent exact tile draws its stand-in ancestor
    /// (`draw_order`), so the frame waits for the stand-ins of absent
    /// tiles too, and the pixels never depend on when they landed: the
    /// same frame rendered with slow stand-ins and with instant ones is
    /// bit-identical.
    #[test]
    fn an_absent_tiles_stand_in_is_waited_for_so_the_pixels_do_not_depend_on_timing() {
        let _refused = crate::symbol_pass::LINE_LABELS_REFUSED_TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let request = CaptureRequest { clear: crate::capture::Clear::SENTINEL, backdrop: false, ..requests(1)[0].clone() };
        let victim = request.cover()[0];
        let style = crate::style_spec::parse_and_compile(&std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/liberty-style.json")).unwrap()).unwrap();
        let mut session = crate::capture::CaptureSession::new_blocking(None).expect("device");
        let rt = runtime();
        let mut frame = |stand_in_delay: Duration| {
            let dir = tempfile::tempdir().unwrap();
            // Only the PARENT is slow: the grandparent lands first, and a
            // wait that ended at "any stand-in resident" would draw it
            // instead of the parent the fast run draws.
            let slow: HashMap<TileId, Duration> = stand_ins(victim).take(1).map(|a| (a, stand_in_delay)).collect();
            let fake = FakeSource { absent: [victim].into(), slow, ..FakeSource::fixture(Duration::ZERO) };
            let mut p = Prefetcher::new(Arc::new(fake), Arc::new(plan(std::iter::once(request.clone()))), PrefetchOptions::new(30.0, dir.path().into()), rt.handle().clone());
            let out = p.ensure(0, false).unwrap();
            assert!(out.missing.is_empty(), "absent is not missing: {:?}", out.missing);
            let hash = session.render(&request, &p.store(), &style).unwrap().sha256_hex();
            (hash, out.waited)
        };
        let (slow_hash, slow_wait) = frame(Duration::from_millis(300));
        let (fast_hash, _) = frame(Duration::ZERO);
        assert!(slow_wait >= Duration::from_millis(250), "the frame waited for the absent tile's stand-ins: {slow_wait:?}");
        assert_eq!(slow_hash, fast_hash);
    }

    /// R1: a settle timeout no `Instant` can add (`Duration::MAX`) is no
    /// deadline, not a panic: the frame's tiles land and the wait ends.
    #[test]
    fn an_unaddable_settle_timeout_waits_for_the_tiles_without_panicking() {
        let rt = runtime();
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeSource::fixture(Duration::from_millis(20)));
        let mut p = Prefetcher::new(fake, Arc::new(plan(requests(2).into_iter())), PrefetchOptions::new(30.0, dir.path().into()), rt.handle().clone());
        let out = p.ensure_with(0, Some(Duration::MAX)).unwrap();
        assert!(out.missing.is_empty());
    }

    #[test]
    fn a_frame_past_the_plan_is_an_error_not_a_panic() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut p = Prefetcher::new(Arc::new(FakeSource::fixture(Duration::ZERO)), Arc::new(plan(requests(2).into_iter())), PrefetchOptions::new(30.0, dir.path().into()), rt.handle().clone());
        assert!(p.ensure(2, false).is_err());
    }

    #[test]
    fn default_options_follow_the_brief() {
        let o = PrefetchOptions::new(30.0, PathBuf::from("x"));
        assert_eq!((o.concurrency, o.lookahead_frames, o.tile_timeout, o.settle_timeout, o.fetch_timeout, o.strict), (16, 90, Duration::from_secs(10), Duration::from_secs(60), Duration::from_secs(20), false));
    }
}

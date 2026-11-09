//! Which pending tile request a fetch worker takes next, and which pending
//! requests the camera no longer wants.
//!
//! # The defect this exists to fix
//!
//! The phone shell's fetcher was an unbounded, strictly FIFO `mpsc` channel
//! feeding eight blocking workers, and nothing was ever taken back out of
//! it. Each frame the shell queued the cover tiles it did not yet have,
//! nearest the eye first -- but only the NEW ones. Every job queued from an
//! earlier camera position stayed ahead of them, and each was already
//! marked in flight, so it was never re-requested at a better place in the
//! line either. At 60 degrees of pitch a cover is up to
//! `cover_plan::MAX_COVER_TILES` (512) tiles, so a pan at street zoom left
//! hundreds of far-row tiles from positions the camera had already left
//! sitting between the workers and the ground under the camera. The
//! foreground showed the coarse stand-in (giant motorway strokes, huge park
//! polygons) until the stale backlog drained. The browser shell had the same
//! property one layer down: one `fetch` per tile, never aborted, rationed
//! only by the browser's own per-host connection limit.
//!
//! # The rule, and where it comes from
//!
//! MapLibre GL JS (BSD-3) does not let a tile the camera left keep its
//! place in the network queue. In `src/tile/tile_manager.ts`,
//! `update` -> `_cleanUpVectorTiles`/`_cleanUpRasterTiles` removes every
//! tile that is not in the retained set, and `_removeTile` ABORTS any that
//! has no data yet (`tile.aborted = true; this._abortTile(tile)`, which in
//! `src/source/vector_tile_source.ts` `abortTile` is
//! `tile.abortController.abort()`). Requests are rationed, not unbounded:
//! `src/util/image_request.ts` holds a queue capped at
//! `config.MAX_PARALLEL_IMAGE_REQUESTS` (16) in flight, and an aborted
//! queued request is taken out of that queue before it ever starts.
//!
//! This module is that rule as a pure data structure:
//!
//! - [`FetchQueue::pop`] hands a worker the highest-priority PENDING job --
//!   lowest [`Priority`], i.e. the class first ([`FetchClass`]: visible
//!   cover, then coarse prefetch, then next-zoom prefetch), then nearest the
//!   eye within the class -- not the oldest one.
//! - [`FetchQueue::retarget`], called once per frame with the frame's whole
//!   [`Wanted`] set, re-prioritises every pending job the camera still wants
//!   and DROPS every one it does not. The dropped keys are returned so the
//!   shell can clear its own in-flight mark and ask again later if the camera
//!   comes back. A dropped job is not a failure: it never touched the network.
//! - A job already handed to a worker ("running") is never dropped. A
//!   blocking HTTP request cannot be aborted cheaply, and its bytes are
//!   still worth decoding; [`FetchQueue::finish`] retires it when the worker
//!   is done.
//! - [`FetchQueue::push`] refuses a key that is already pending or running,
//!   so a tile can never be in the queue twice.
//! - [`Wanted::triage`] splits bodies that ARRIVED (a running request the
//!   camera left still delivers) into ones worth decoding, most urgent first,
//!   and stale ones the shell discards undecoded; [`may_start_another`] is the
//!   per-frame decode budget's rule. On the phone, decoding every arrival was
//!   the other half of the defect: after a pan and a zoom-out the device log
//!   showed 1,526 requests in flight, `decoded` at 1,323, 9 fps with a 5.3 s
//!   stall, and the 12-tile z7 cover all HOLE for over 10 s.
//!
//! # Measured
//!
//! The harness in this file's tests models a phone shell (8 workers) over the
//! real `cover_plan` covers at Times Square, pitch 60, z16.3 (tiles z14):
//!
//! | scenario                                   | master      | this module |
//! |--------------------------------------------|-------------|-------------|
//! | sideways pan 4 s, 150 ms: to foreground    | 16,867 ms   | 17 ms       |
//! | backward pan 4 s, 150 ms: to foreground    | 16,567 ms   | 17 ms       |
//! | pan + zoom-out to z7.9, 60 ms: to no holes | 24,167 ms   | 333 ms      |
//! | same: in flight when the motion stopped    | 2,756       | 19          |
//! | same: stale decodes                        | 2,931       | 0           |
//! | same: peak `decoded`                       | 2,717       | 260         |
//! | same: most decode work in one frame        | 64 ms       | 8 ms        |
//!
//! # Time is not a parameter at all
//!
//! Nothing here reads a clock (`tests/wasm_portability.rs` would refuse it
//! for the browser build anyway): the queue orders by priority and by
//! insertion sequence only. Retry timing stays in `fetch_policy`.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;

/// Why a tile is wanted, most urgent first. The derive order IS the
/// priority order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FetchClass {
    /// In the current draw cover (vector or imagery).
    Visible,
    /// The coarse prefetch cover a few zooms up -- stand-ins, `Optional` in
    /// maplibre-native's `TileNecessity` sense.
    Coarse,
    /// The next integer zoom's cover, fetched ahead of a zoom-in.
    NextZoom,
}

/// A pending job's place in line: class first, then `rank` within the class
/// (lower is sooner). Derived `Ord` compares `class` before `rank`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Priority {
    pub class: FetchClass,
    pub rank: u32,
}

/// The rank a list's LAST entry would get. [`Wanted::add`] spreads each list
/// over `0..RANK_SCALE` by relative position, so two lists added under the
/// same class (a vector cover and an imagery cover at a different zoom, of
/// different lengths) interleave by how near the eye each entry is within
/// its own list instead of one list starving the other.
pub const RANK_SCALE: u32 = 1 << 20;

/// One frame's wanted set, priority-ordered. Build it with [`Wanted::add`]
/// in class order; the first add of a key wins, so a tile in both the
/// visible cover and a prefetch cover keeps its visible priority.
#[derive(Debug)]
pub struct Wanted<K> {
    map: HashMap<K, Priority>,
}

impl<K: Copy + Eq + Hash> Default for Wanted<K> {
    fn default() -> Self {
        Wanted { map: HashMap::new() }
    }
}

impl<K: Copy + Eq + Hash> Wanted<K> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `keys`, already in fetch order (nearest the eye first), under
    /// `class`. A key already present keeps its earlier priority.
    pub fn add(&mut self, class: FetchClass, keys: &[K]) {
        let len = keys.len().max(1) as u64;
        for (i, &key) in keys.iter().enumerate() {
            let rank = (i as u64 * u64::from(RANK_SCALE) / len) as u32;
            self.map.entry(key).or_insert(Priority { class, rank });
        }
    }

    pub fn get(&self, key: &K) -> Option<Priority> {
        self.map.get(key).copied()
    }

    pub fn contains(&self, key: &K) -> bool {
        self.map.contains_key(key)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Every key, most urgent first (ties in an unspecified but stable-per-
    /// call order). What a shell walks to issue new requests.
    pub fn in_priority_order(&self) -> Vec<(K, Priority)> {
        let mut out: Vec<(K, Priority)> = self.map.iter().map(|(k, p)| (*k, *p)).collect();
        out.sort_by_key(|(_, p)| *p);
        out
    }

    /// Splits results that have ARRIVED but not been decoded into the ones
    /// still worth decoding, most urgent first, and the stale ones: neither
    /// in this wanted set nor kept by `also_keep` (a shell's retained
    /// stand-ins). A stale result is discarded undecoded -- its request was
    /// already running when the camera left, so the bytes came anyway, but
    /// decoding them is frame-thread work for a tile nothing will draw, and
    /// holding them is memory. Kept results with no wanted priority (only
    /// `also_keep` matched) go last.
    pub fn triage<T>(&self, arrived: Vec<(K, T)>, also_keep: impl Fn(&K) -> bool) -> Triage<K, T> {
        let mut keep: Vec<(Option<Priority>, K, T)> = Vec::with_capacity(arrived.len());
        let mut stale = Vec::new();
        for (key, item) in arrived {
            match self.get(&key) {
                Some(p) => keep.push((Some(p), key, item)),
                None if also_keep(&key) => keep.push((None, key, item)),
                None => stale.push((key, item)),
            }
        }
        // `None` sorts first in `Option`'s own order; this wants it last.
        keep.sort_by_key(|(p, _, _)| (p.is_none(), *p));
        Triage { in_order: keep.into_iter().map(|(_, k, t)| (k, t)).collect(), stale }
    }
}

/// [`Wanted::triage`]'s answer.
#[derive(Debug)]
pub struct Triage<K, T> {
    /// Decode these, in this order, as far as the frame's budget goes.
    pub in_order: Vec<(K, T)>,
    /// Discard these undecoded, and clear their in-flight marks.
    pub stale: Vec<(K, T)>,
}

/// Whether a frame may start decoding one more arrived tile: always the
/// first (so a tile slower than the whole budget still makes progress), and
/// after that only while `elapsed_ms` is under `budget_ms`. The rule every
/// budgeted loop in the shells already follows for the bake.
#[must_use]
pub fn may_start_another(done: usize, elapsed_ms: f64, budget_ms: f64) -> bool {
    done == 0 || elapsed_ms < budget_ms
}

struct Pending<J> {
    priority: Priority,
    /// Insertion order, the tie-break within one priority.
    seq: u64,
    job: J,
}

/// Pending and running fetch jobs keyed by `K` (a shell's tile identity, e.g.
/// `(TileId, TileKind)`), with a job payload `J` (a URL, say). See the
/// module doc for the rules.
pub struct FetchQueue<K, J> {
    pending: HashMap<K, Pending<J>>,
    running: HashSet<K>,
    seq: u64,
    stale_dropped: u64,
}

impl<K: Copy + Eq + Hash, J> Default for FetchQueue<K, J> {
    fn default() -> Self {
        FetchQueue { pending: HashMap::new(), running: HashSet::new(), seq: 0, stale_dropped: 0 }
    }
}

impl<K: Copy + Eq + Hash, J> FetchQueue<K, J> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues `job` for `key` at `priority`. Returns `false`, and queues
    /// nothing, when `key` is already pending or running -- a tile is never
    /// requested twice at once.
    pub fn push(&mut self, key: K, priority: Priority, job: J) -> bool {
        if self.running.contains(&key) || self.pending.contains_key(&key) {
            return false;
        }
        self.seq += 1;
        self.pending.insert(key, Pending { priority, seq: self.seq, job });
        true
    }

    /// Re-prioritises every pending job by `wanted`, and drops every pending
    /// job `wanted` does not contain. Returns the dropped keys, so the caller
    /// can clear its in-flight mark for each (letting the camera ask again
    /// later). Running jobs are never touched.
    pub fn retarget(&mut self, wanted: &Wanted<K>) -> Vec<K> {
        let mut dropped = Vec::new();
        self.pending.retain(|key, p| match wanted.get(key) {
            Some(priority) => {
                p.priority = priority;
                true
            }
            None => {
                dropped.push(*key);
                false
            }
        });
        self.stale_dropped += dropped.len() as u64;
        dropped
    }

    /// Takes the most urgent pending job (lowest priority, then oldest) and
    /// marks it running. `None` when nothing is pending.
    ///
    /// A linear scan: a pending set is at most a few covers (~1-2 k), and a
    /// pop happens once per completed request, not per frame.
    pub fn pop(&mut self) -> Option<(K, J)> {
        let key = *self.pending.iter().min_by_key(|(_, p)| (p.priority, p.seq))?.0;
        let p = self.pending.remove(&key)?;
        self.running.insert(key);
        Some((key, p.job))
    }

    /// A running job's request finished (whatever it answered). Returns
    /// whether `key` was running.
    pub fn finish(&mut self, key: &K) -> bool {
        self.running.remove(key)
    }

    pub fn is_pending(&self, key: &K) -> bool {
        self.pending.contains_key(key)
    }

    pub fn is_running(&self, key: &K) -> bool {
        self.running.contains(key)
    }

    /// Jobs waiting for a worker.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Jobs a worker has taken and not yet finished.
    pub fn running_len(&self) -> usize {
        self.running.len()
    }

    /// Every pending job [`Self::retarget`] has dropped since the queue was
    /// made, for the status line.
    pub fn stale_dropped_total(&self) -> u64 {
        self.stale_dropped
    }

    /// Drops everything pending (a style or source swap) and returns the
    /// keys, as [`Self::retarget`] with an empty wanted set would, but
    /// without counting them as stale.
    pub fn clear_pending(&mut self) -> Vec<K> {
        self.pending.drain().map(|(k, _)| k).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vis(rank: u32) -> Priority {
        Priority { class: FetchClass::Visible, rank }
    }

    #[test]
    fn a_worker_takes_the_most_urgent_pending_job_not_the_oldest() {
        let mut q: FetchQueue<u32, &str> = FetchQueue::new();
        assert!(q.push(1, vis(30), "far"));
        assert!(q.push(2, vis(10), "near"));
        assert!(q.push(3, vis(20), "mid"));
        assert_eq!(q.pop(), Some((2, "near")));
        assert_eq!(q.pop(), Some((3, "mid")));
        assert_eq!(q.pop(), Some((1, "far")));
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn equal_priorities_are_served_oldest_first() {
        let mut q: FetchQueue<u32, ()> = FetchQueue::new();
        for k in [5, 3, 9] {
            q.push(k, vis(0), ());
        }
        let order: Vec<u32> = std::iter::from_fn(|| q.pop().map(|(k, _)| k)).collect();
        assert_eq!(order, vec![5, 3, 9]);
    }

    #[test]
    fn classes_are_served_visible_then_coarse_then_next_zoom_whatever_the_rank() {
        let mut q: FetchQueue<u32, ()> = FetchQueue::new();
        q.push(1, Priority { class: FetchClass::NextZoom, rank: 0 }, ());
        q.push(2, Priority { class: FetchClass::Coarse, rank: 0 }, ());
        q.push(3, Priority { class: FetchClass::Visible, rank: RANK_SCALE - 1 }, ());
        let order: Vec<u32> = std::iter::from_fn(|| q.pop().map(|(k, _)| k)).collect();
        assert_eq!(order, vec![3, 2, 1]);

        let mut w: Wanted<u32> = Wanted::new();
        w.add(FetchClass::Visible, &[10, 11]);
        w.add(FetchClass::Coarse, &[11, 12]);
        w.add(FetchClass::NextZoom, &[13]);
        let classes: Vec<FetchClass> = w.in_priority_order().iter().map(|(_, p)| p.class).collect();
        assert_eq!(classes, vec![FetchClass::Visible, FetchClass::Visible, FetchClass::Coarse, FetchClass::NextZoom]);
        assert_eq!(w.get(&11).unwrap().class, FetchClass::Visible, "the first add of a key wins");
    }

    #[test]
    fn retarget_drops_unwanted_pending_jobs_and_hands_their_keys_back() {
        let mut q: FetchQueue<u32, ()> = FetchQueue::new();
        for k in 1..=4 {
            q.push(k, vis(k), ());
        }
        let mut w = Wanted::new();
        w.add(FetchClass::Visible, &[2, 4]);
        let mut dropped = q.retarget(&w);
        dropped.sort_unstable();
        assert_eq!(dropped, vec![1, 3], "the caller clears its in-flight mark for exactly these");
        assert_eq!(q.pending_len(), 2);
        assert_eq!(q.stale_dropped_total(), 2);
        assert!(!q.is_pending(&1) && !q.is_pending(&3));
        // Un-marked: the camera coming back can queue them again.
        assert!(q.push(1, vis(0), ()));
    }

    #[test]
    fn retarget_reorders_what_is_still_wanted() {
        let mut q: FetchQueue<u32, ()> = FetchQueue::new();
        q.push(1, vis(0), ());
        q.push(2, vis(1), ());
        let mut w = Wanted::new();
        w.add(FetchClass::Visible, &[2, 1]); // the camera turned round
        assert!(q.retarget(&w).is_empty());
        assert_eq!(q.pop().map(|(k, _)| k), Some(2));
    }

    #[test]
    fn a_running_job_is_never_dropped_and_never_requested_twice() {
        let mut q: FetchQueue<u32, ()> = FetchQueue::new();
        q.push(7, vis(0), ());
        assert_eq!(q.pop().map(|(k, _)| k), Some(7));
        assert!(!q.push(7, vis(0), ()), "running: no second request");
        assert!(q.retarget(&Wanted::new()).is_empty(), "running jobs are not dropped");
        assert!(q.is_running(&7));
        assert!(q.finish(&7));
        assert!(q.push(7, vis(0), ()), "finished: may be asked for again");
        assert!(!q.push(7, vis(0), ()), "pending: no second request");
        assert_eq!(q.pending_len(), 1);
    }

    #[test]
    fn two_lists_in_one_class_interleave_by_relative_position() {
        let mut w: Wanted<u32> = Wanted::new();
        w.add(FetchClass::Visible, &[1, 2, 3]); // vector cover: ranks 0, 1/3, 2/3
        w.add(FetchClass::Visible, &[100, 200]); // imagery cover: ranks 0, 1/2
        let order: Vec<u32> = w.in_priority_order().iter().map(|(k, _)| *k).collect();
        // 1 and 100 share rank 0; the rest interleave rather than queueing
        // the whole imagery cover behind the whole vector one.
        assert!(order[..2].contains(&1) && order[..2].contains(&100), "{order:?}");
        assert_eq!(&order[2..], &[2, 200, 3], "{order:?}");
    }

    // -- the pan harness ---------------------------------------------------
    //
    // A deterministic discrete-event model of one phone shell through one
    // camera path. Each frame: arrivals are collected, the camera moves, the
    // request pass runs, decoded tiles are baked and evicted, and idle fetch
    // workers take the next job, each request taking `latency_ms`. The frame
    // lasts `FRAME_MS` or, when its decode work is longer, that work plus
    // `FRAME_BASE_MS` -- which is how a decode flood becomes a stall. It
    // drives the real `cover_plan::flat_cover`/`nearest_first` the shells
    // use, under two shell behaviours:
    //
    // - `Master`: `ios/src/app.rs` + `ios/src/net.rs` at e012b1c. An `mpsc`
    //   FIFO: each frame appends the new tiles, nearest first; a queued tile
    //   is in flight and never re-queued or removed. Every arrival is decoded
    //   in `collect_fetched`, whatever it is. Eviction runs only inside
    //   `bake_some`, i.e. only on a frame that had something to bake.
    // - `Fixed`: this module. Priority pop, drop-unwanted-pending, stale
    //   arrivals discarded undecoded, decode under `DECODE_BUDGET_MS`
    //   nearest-first, eviction every frame.

    use crate::geo::TileId;
    use crate::render::cover_plan::{flat_cover, nearest_first};
    use crate::render::Camera;

    const WORKERS: usize = 8;
    const FRAME_MS: f64 = 1000.0 / 60.0;
    const FRAME_BASE_MS: f64 = 6.0;
    const SOURCE_MAX_ZOOM: u8 = 14;
    const PREFETCH_ZOOM_DELTA: u8 = 4;
    const MAX_GPU_TILES: usize = 96;
    const MAX_DECODED_TILES: usize = 64;
    /// The shells' decode budget; see `DECODE_BUDGET_MS` in each.
    const DECODE_BUDGET_MS: f64 = 4.0;
    /// The foreground: the cover's nearest-the-eye tiles, the bottom band of
    /// the screen at 60 degrees.
    const FOREGROUND: usize = 6;

    #[derive(Clone, Copy, PartialEq, Debug)]
    enum Shell {
        Master,
        Fixed,
    }

    #[derive(Clone, Copy, Debug)]
    struct Link {
        latency_ms: f64,
        /// Frame-thread `mvt::decode` cost per tile.
        decode_ms: f64,
    }

    #[derive(Debug, Default)]
    struct Report {
        /// Longest pending queue (not counting running requests).
        max_queue: usize,
        /// The shell's `inflight` set when the motion stopped (the device
        /// log's `inflight`).
        inflight_at_stop: usize,
        completed: usize,
        /// Fetch completions for tiles not wanted when they completed.
        stale_completions: usize,
        /// Decodes of tiles not wanted when decoded.
        stale_decodes: usize,
        max_decodes_per_frame: usize,
        max_frame_ms: f64,
        max_decoded: usize,
        /// Motion stop -> every foreground tile has its OWN data.
        foreground_ms: f64,
        /// Motion stop -> no cover tile is a hole (own data or an ancestor's).
        no_holes_ms: f64,
        dropped_pending: u64,
        max_cover: usize,
        max_coarse: usize,
    }

    fn cam(lat: f64, lon: f64, zoom: f64) -> Camera {
        Camera {
            center_lon: lon,
            center_lat: lat,
            zoom,
            pitch: 60f32.to_radians(),
            bearing: 0.0,
            // An iPhone 15 Pro: 393x852 points (the camera is in points).
            viewport_width_px: 393.0,
            viewport_height_px: 852.0,
            fov_y: Camera::DEFAULT_FOV_Y,
        }
    }

    /// The shells' `cover_at`: the globe's cover when it has one, else the
    /// flat frustum cover.
    fn cover_of(camera: &Camera, z: u8) -> Vec<TileId> {
        let globe = crate::render::globe::GlobeCover::new(&crate::render::globe::globe_frame(camera));
        crate::render::cover_plan::tile_cover(z, &globe, || flat_cover(camera, z).tiles)
    }

    fn has_data_or_ancestor(t: TileId, decoded: &HashSet<TileId>) -> bool {
        let mut t = t;
        loop {
            if decoded.contains(&t) {
                return true;
            }
            if t.z == 0 {
                return false;
            }
            t = TileId { z: t.z - 1, x: t.x / 2, y: t.y / 2 };
        }
    }

    /// Runs `path` (camera at time `t` ms; motion ends at `stop_ms`) and
    /// then holds still until everything wanted is decoded, or 60 s.
    fn simulate(shell: Shell, link: Link, stop_ms: f64, path: &dyn Fn(f64) -> Camera) -> Report {
        let mut decoded: HashSet<TileId> = HashSet::new();
        let mut meshed: HashSet<TileId> = HashSet::new();
        let mut inflight: HashSet<TileId> = HashSet::new();
        let mut arrived: Vec<(TileId, ())> = Vec::new();
        let mut fifo: std::collections::VecDeque<TileId> = Default::default();
        let mut queue: FetchQueue<TileId, ()> = FetchQueue::new();
        let mut workers: Vec<Option<(f64, TileId)>> = vec![None; WORKERS];
        let mut r = Report { foreground_ms: f64::INFINITY, no_holes_ms: f64::INFINITY, ..Report::default() };
        let mut now: f64 = 0.0;
        let mut stopped = false;
        type Covers = ((f64, f64, f64), Vec<TileId>, Vec<TileId>);
        let mut memo: Option<Covers> = None;
        loop {
            let camera = path(now.min(stop_ms));
            let z = (camera.zoom.floor() as u8).min(SOURCE_MAX_ZOOM);
            // Covers are the harness's whole cost; a still camera reuses them.
            let at = (camera.center_lat, camera.center_lon, camera.zoom);
            if memo.as_ref().is_none_or(|(k, _, _)| *k != at) {
                let cover = nearest_first(&cover_of(&camera, z), &camera);
                let coarse = nearest_first(&cover_of(&camera, z.saturating_sub(PREFETCH_ZOOM_DELTA)), &camera);
                memo = Some((at, cover, coarse));
            }
            let (_, cover, coarse) = memo.as_ref().expect("just filled");
            let (cover, coarse) = (cover.clone(), coarse.clone());
            r.max_cover = r.max_cover.max(cover.len());
            r.max_coarse = r.max_coarse.max(coarse.len());
            let mut wanted = Wanted::new();
            wanted.add(FetchClass::Visible, &cover);
            wanted.add(FetchClass::Coarse, &coarse);

            // collect_fetched
            for w in workers.iter_mut() {
                if let Some((done, t)) = *w {
                    if done <= now {
                        *w = None;
                        queue.finish(&t);
                        r.completed += 1;
                        if !wanted.contains(&t) {
                            r.stale_completions += 1;
                        }
                        arrived.push((t, ()));
                    }
                }
            }
            let mut decodes = 0usize;
            match shell {
                Shell::Master => {
                    for (t, ()) in arrived.drain(..) {
                        inflight.remove(&t);
                        decoded.insert(t);
                        decodes += 1;
                        if !wanted.contains(&t) {
                            r.stale_decodes += 1;
                        }
                    }
                }
                Shell::Fixed => {
                    let triage = wanted.triage(std::mem::take(&mut arrived), |_| false);
                    for (t, ()) in triage.stale {
                        inflight.remove(&t);
                    }
                    let mut spent = 0.0;
                    for (t, ()) in triage.in_order {
                        if may_start_another(decodes, spent, DECODE_BUDGET_MS) {
                            inflight.remove(&t);
                            decoded.insert(t);
                            decodes += 1;
                            spent += link.decode_ms;
                        } else {
                            arrived.push((t, ()));
                        }
                    }
                }
            }
            r.max_decodes_per_frame = r.max_decodes_per_frame.max(decodes);

            // request_missing_tiles
            match shell {
                Shell::Master => {
                    for &t in cover.iter().chain(coarse.iter()) {
                        if !decoded.contains(&t) && inflight.insert(t) {
                            fifo.push_back(t);
                        }
                    }
                }
                Shell::Fixed => {
                    for t in queue.retarget(&wanted) {
                        inflight.remove(&t);
                    }
                    for (t, p) in wanted.in_priority_order() {
                        if !decoded.contains(&t) && inflight.insert(t) {
                            queue.push(t, p, ());
                        }
                    }
                }
            }

            // bake_some (instantaneous here) + evict_discard_pile.
            let mut baked = false;
            for &t in cover.iter().chain(coarse.iter()) {
                if decoded.contains(&t) && meshed.insert(t) {
                    baked = true;
                }
            }
            if baked || shell == Shell::Fixed {
                let keep: HashSet<TileId> = cover.iter().chain(coarse.iter()).copied().collect();
                if meshed.len() > MAX_GPU_TILES {
                    meshed.retain(|t| keep.contains(t));
                }
                if decoded.len() > MAX_DECODED_TILES {
                    decoded.retain(|t| keep.contains(t) || meshed.contains(t));
                }
            }
            r.max_decoded = r.max_decoded.max(decoded.len());

            // Idle workers take the next job.
            for w in workers.iter_mut() {
                if w.is_none() {
                    let next = match shell {
                        Shell::Master => fifo.pop_front(),
                        Shell::Fixed => queue.pop().map(|(k, _)| k),
                    };
                    if let Some(t) = next {
                        *w = Some((now + link.latency_ms, t));
                    }
                }
            }
            let pending = match shell {
                Shell::Master => fifo.len(),
                Shell::Fixed => queue.pending_len(),
            };
            r.max_queue = r.max_queue.max(pending);

            if now >= stop_ms {
                if !stopped {
                    stopped = true;
                    r.inflight_at_stop = inflight.len();
                }
                let since = now - stop_ms;
                if r.foreground_ms.is_infinite() && cover.iter().take(FOREGROUND).all(|t| decoded.contains(t)) {
                    r.foreground_ms = since;
                }
                if r.no_holes_ms.is_infinite() && cover.iter().all(|t| has_data_or_ancestor(*t, &decoded)) {
                    r.no_holes_ms = since;
                }
                let idle = workers.iter().all(Option::is_none) && pending == 0 && arrived.is_empty();
                if (r.foreground_ms.is_finite() && r.no_holes_ms.is_finite() && idle) || since > 60_000.0 {
                    break;
                }
            }
            let frame_ms = FRAME_MS.max(FRAME_BASE_MS + decodes as f64 * link.decode_ms);
            r.max_frame_ms = r.max_frame_ms.max(frame_ms);
            now += frame_ms;
        }
        r.dropped_pending = queue.stale_dropped_total();
        r
    }

    const TIMES_SQUARE: (f64, f64) = (40.7580, -73.9855);
    const STREET_ZOOM: f64 = 16.3;

    /// Degrees of longitude one screen width (393 pt) spans at `zoom`.
    fn screen_width_deg(zoom: f64) -> f64 {
        393.0 * 360.0 / (512.0 * 2f64.powf(zoom))
    }

    /// A brisk pan: one screen width per second, in `(north, east)` unit
    /// direction.
    fn pan(dir: (f64, f64)) -> impl Fn(f64) -> Camera {
        move |t| {
            let d = screen_width_deg(STREET_ZOOM) * t / 1000.0;
            let lat = TIMES_SQUARE.0 + dir.0 * d * TIMES_SQUARE.0.to_radians().cos();
            cam(lat, TIMES_SQUARE.1 + dir.1 * d, STREET_ZOOM)
        }
    }

    /// The device log's sequence: the same sideways pan at street zoom for
    /// 4 s, then a 2 s zoom-out to z7.9 (tiles z7), then still.
    fn pan_then_zoom_out(t: f64) -> Camera {
        let panned = pan((0.0, 1.0))(t.min(4000.0));
        let f = ((t - 4000.0) / 2000.0).clamp(0.0, 1.0);
        cam(panned.center_lat, panned.center_lon, STREET_ZOOM + (7.9 - STREET_ZOOM) * f)
    }

    /// A 150 ms tile round trip; ~1 ms decode, the figure `ios/src/app.rs`'s
    /// `collect_fetched` doc gives.
    const SLOW_LINK: Link = Link { latency_ms: 150.0, decode_ms: 1.0 };
    /// The device log's case: a fast link (60 ms) and dense Manhattan z14
    /// tiles. 8 ms per decode is derived from the log itself: at 9.3 fps
    /// (~107 ms frames) `decoded` rose by ~93 tiles/s, i.e. ~10 decodes per
    /// frame, so at most ~10 ms each.
    const DEVICE_LINK: Link = Link { latency_ms: 60.0, decode_ms: 8.0 };

    fn report(name: &str, before: &Report, after: &Report) {
        eprintln!("[fetch_queue harness] {name}\n  master: {before:?}\n  fixed:  {after:?}");
    }

    /// Pans at street zoom, pitch 60: the foreground must arrive first.
    ///
    /// Thresholds, justified. Time to foreground after the pan stops must at
    /// least HALVE: with the fix the foreground is first in line, one or two
    /// fetch rounds (150-300 ms), whereas behind a stale FIFO it waits for
    /// whatever backlog is ahead of it; a 2x cut cannot come from the
    /// frame-granular model's own rounding. Stale completions must fall to a
    /// quarter: with the fix only a request already RUNNING when its tile
    /// left the cover (at most `WORKERS` per camera step) completes stale.
    #[test]
    fn a_pan_at_60_degrees_reaches_the_foreground_first() {
        for (name, dir) in [("sideways pan", (0.0, 1.0)), ("backward pan", (-1.0, 0.0)), ("forward pan", (1.0, 0.0))] {
            let path = pan(dir);
            let before = simulate(Shell::Master, SLOW_LINK, 4000.0, &path);
            let after = simulate(Shell::Fixed, SLOW_LINK, 4000.0, &path);
            report(name, &before, &after);
            assert!(
                after.foreground_ms * 2.0 <= before.foreground_ms.max(FRAME_MS * 2.0),
                "{name}: foreground {} ms (fixed) vs {} ms (master)",
                after.foreground_ms,
                before.foreground_ms
            );
            assert!(
                after.stale_completions * 4 <= before.stale_completions,
                "{name}: stale completions {} (fixed) vs {} (master)",
                after.stale_completions,
                before.stale_completions
            );
        }
    }

    /// The device log: pan at street zoom, then zoom out to z7.9. Master
    /// left 1,526 requests in flight, the 12-tile z7 cover all HOLE for over
    /// 10 s, 9 fps with a 5.3 s stall, and `decoded` at 1,323.
    ///
    /// Thresholds, justified:
    /// - No holes within 1 s of the zoom-out ending. The z7 cover plus its
    ///   z3 coarse cover is ~15 tiles: two 60 ms fetch rounds after the <= 8
    ///   requests still running drain, then decodes at one 8 ms tile per
    ///   frame -- ~400 ms nominal. 1 s is 2.5x that and 10x under the device.
    /// - Zero stale decodes: a stale arrival is discarded before decode.
    /// - Per-frame decode work bounded: at most `DECODE_BUDGET_MS` plus one
    ///   tile (the budget is checked between tiles, never inside one).
    /// - `decoded` bounded by the keep set, not by the backlog: under the
    ///   cover cap plus the coarse cover plus `MAX_DECODED_TILES`.
    #[test]
    fn a_street_pan_then_zoom_out_fills_the_low_zoom_cover_promptly() {
        fn check(r: &Report) -> Vec<String> {
            let mut broken = Vec::new();
            if r.no_holes_ms > 1000.0 {
                broken.push(format!("holes for {:.0} ms after the zoom-out", r.no_holes_ms));
            }
            if r.stale_decodes > 0 {
                broken.push(format!("{} stale decodes", r.stale_decodes));
            }
            let per_frame_ms = r.max_decodes_per_frame as f64 * DEVICE_LINK.decode_ms;
            if per_frame_ms > DECODE_BUDGET_MS + DEVICE_LINK.decode_ms {
                broken.push(format!("{per_frame_ms} ms of decode in one frame"));
            }
            // The keep set -- cover and coarse cover, each capped at
            // `MAX_COVER_TILES` -- plus the discard pile.
            if r.max_decoded > 2 * crate::render::cover_plan::MAX_COVER_TILES + MAX_DECODED_TILES {
                broken.push(format!("decoded peaked at {}", r.max_decoded));
            }
            broken
        }
        let before = simulate(Shell::Master, DEVICE_LINK, 6000.0, &pan_then_zoom_out);
        let after = simulate(Shell::Fixed, DEVICE_LINK, 6000.0, &pan_then_zoom_out);
        report("street pan then zoom out to z7.9", &before, &after);
        let (red, green) = (check(&before), check(&after));
        eprintln!("[fetch_queue harness] master breaks: {red:?}");
        // The `decoded` peak no longer catches the master QUEUE: that peak
        // came from 512-tile covers at pitch 60, and the distance LOD
        // (`cover_plan::flat_cover`) now bounds the covers both shells
        // simulate here to a few dozen tiles. The other three are the
        // queue's own failures and must still catch it.
        assert_eq!(red.len(), 3, "every queue threshold must catch master's behaviour: {red:?}");
        assert!(!red.iter().any(|b| b.starts_with("decoded peaked")), "{red:?}");
        assert!(green.is_empty(), "the fix breaks: {green:?}");
    }
}

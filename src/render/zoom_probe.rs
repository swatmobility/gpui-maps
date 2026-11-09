//! Zoom-continuity probe for tiled overlay sources (`probe` feature only).
//!
//! A per-frame log of what the overlay and the basemap drew for each cover
//! tile (own tile, ancestor stand-in, descendant stand-in, nothing), the
//! life of every overlay tile bake (requested -> started -> baked -> landed
//! -> uploaded), cache evictions of tiles the cover still wanted, and each
//! frame's CPU time split by paint phase. Written for
//! `docs/measurements/2026-09-26-overlay-zoom-continuity.md` and driven by
//! `demos/us_block_groups.rs --zoom-sweep`.
//!
//! Off until [`start`] is called, so an ordinary `probe` build pays one
//! relaxed load per hook. Nothing here changes what `paint` does: the
//! classification below re-derives the draw picks from the same cache
//! queries `paint` makes (`contains`), in the same order (own, ancestor at
//! any depth, then a complete descendant set two levels down) for the
//! basemap; the overlay's classification is handed in by `paint` itself.

use crate::geo::TileId;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};


static ON: AtomicBool = AtomicBool::new(false);

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(State::default()))
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    if !ON.load(Relaxed) {
        return None;
    }
    let mut s = state().lock().ok()?;
    Some(f(&mut s))
}

/// How a cover tile was drawn this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drawn {
    Own,
    /// Ancestor this many levels coarser.
    Ancestor(u8),
    Descendants,
    /// The tile's cut is known empty: nothing to draw, and correctly so.
    Empty,
    Hole,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CoverCounts {
    pub own: u32,
    pub ancestor: u32,
    pub descendants: u32,
    pub empty: u32,
    pub hole: u32,
    /// Sum of ancestor zoom distances (mean = this / ancestor).
    pub ancestor_levels: u32,
}

#[derive(Debug, Clone, Default)]
pub struct FrameRow {
    pub t_ms: f64,
    pub zoom: f64,
    pub segment: usize,
    pub cpu_ms: f64,
    pub phase_ms: Vec<(&'static str, f64)>,
    pub gpu_ms_last_sample: f64,
    pub overlay: CoverCounts,
    pub basemap: CoverCounts,
    pub overlay_uploads: u32,
    pub overlay_upload_bytes: u64,
    pub overlay_upload_ms: f64,
    pub bakes_queued: u32,
    pub bakes_running: u32,
    pub landed_waiting: u32,
    pub evicted_in_cover: u32,
    pub evicted_prev_drawn: u32,
    pub cache_bytes: u64,
    pub cache_budget: u64,
    pub cache_entries: u32,
    /// Main-thread work charged to this frame that ran OUTSIDE `paint`
    /// (tile installs from entity updates, etc.), by name, in ms.
    pub extra_ms: Vec<(&'static str, f64)>,
    /// Per-frame event counts by name (plan outcomes, sync symbol bakes...).
    pub counts: Vec<(&'static str, u32)>,
}

#[derive(Debug, Clone, Default)]
pub struct BakeRow {
    pub tile: Option<TileId>,
    pub segment: usize,
    pub requested_ms: f64,
    pub started_ms: Option<f64>,
    pub baked_ms: Option<f64>,
    pub landed_ms: Option<f64>,
    pub uploaded_ms: Option<f64>,
    /// Nearest-first rank in the cover it was requested from.
    pub rank_at_request: usize,
    /// Nearest-first rank in the CURRENT cover when the bake started;
    /// `None` = the camera had already left this tile.
    pub rank_at_start: Option<usize>,
    pub in_cover_at_upload: Option<bool>,
    pub superseded: bool,
    pub empty: bool,
    pub discarded: bool,
    pub rebake: bool,
    /// Dropped from the bake queue before it started: its tile left the
    /// wanted set.
    pub dropped: bool,
}

#[derive(Default)]
struct Pending {
    phase_ns: HashMap<&'static str, u64>,
    overlay: CoverCounts,
    basemap: CoverCounts,
    overlay_uploads: u32,
    overlay_upload_bytes: u64,
    overlay_upload_ns: u64,
    evicted_in_cover: u32,
    evicted_prev_drawn: u32,
    cache_bytes: u64,
    cache_budget: u64,
    cache_entries: u32,
    extra_ns: HashMap<&'static str, u64>,
    counts: HashMap<&'static str, u32>,
}

#[derive(Default)]
struct State {
    t0: Option<crate::timing::Instant>,
    zoom: f64,
    segment: usize,
    segments: Vec<(String, f64)>,
    gpu_ms_last: f64,
    cur: Pending,
    frames: Vec<FrameRow>,
    bakes: Vec<BakeRow>,
    open: HashMap<TileId, usize>,
    baked_ever: HashSet<TileId>,
    wanted_rank: HashMap<TileId, usize>,
    /// Overlay tiles last frame's draw found in the GPU cache.
    cached_prev: HashSet<TileId>,
    /// Per overlay display tile: (segment it entered the cover in,
    /// stand-in frames, hole frames) until its own tile draws.
    waiting: HashMap<TileId, (usize, u32, u32)>,
    /// (segment, stand-in frames, hole frames) of each display tile that
    /// reached its own tile.
    resolved: Vec<(usize, u32, u32)>,
    /// Display tiles that left the cover before their own tile drew.
    left_unresolved: Vec<(usize, u32, u32)>,
    basemap_waiting: HashMap<TileId, (usize, u32, u32)>,
    basemap_resolved: Vec<(usize, u32, u32)>,
}

impl State {
    fn now_ms(&self) -> f64 {
        self.t0.map_or(0.0, |t| t.elapsed().as_secs_f64() * 1e3)
    }
}

/// Starts (or restarts) recording. `label` names the first segment.
pub fn start(label: &str) {
    let mut s = state().lock().expect("zoom probe lock");
    *s = State::default();
    s.t0 = Some(crate::timing::Instant::now());
    s.segments.push((label.to_string(), 0.0));
    ON.store(true, Relaxed);
}

/// Opens a new segment of the run (e.g. "zoom in", "settle z12").
pub fn mark(label: &str) {
    with(|s| {
        let t = s.now_ms();
        s.segments.push((label.to_string(), t));
        s.segment = s.segments.len() - 1;
    });
}

/// The camera zoom the host just set, for the frame log.
pub fn note_zoom(zoom: f64) {
    with(|s| s.zoom = zoom);
}

pub fn stop() {
    ON.store(false, Relaxed);
}

/// Every segment's `(label, start ms)` and every recorded frame, for a
/// caller that grades them rather than prints them -- the census demo's
/// `gate:` lines, which `scripts/perf-gate.sh` scores. A copy: the log
/// keeps recording (or stays stopped) exactly as before.
pub fn snapshot() -> (Vec<(String, f64)>, Vec<FrameRow>) {
    let s = state().lock().expect("zoom probe lock");
    (s.segments.clone(), s.frames.clone())
}

pub(crate) fn note_phase(name: &'static str, nanos: u64) {
    with(|s| *s.cur.phase_ns.entry(name).or_default() += nanos);
}

/// Main-thread work that ran outside `paint` (between two frames), by name.
pub(crate) fn note_extra(name: &'static str, nanos: u64) {
    with(|s| *s.cur.extra_ns.entry(name).or_default() += nanos);
}

/// One occurrence of a named event in this frame.
pub(crate) fn note_count(name: &'static str) {
    with(|s| *s.cur.counts.entry(name).or_default() += 1);
}

pub(crate) fn note_gpu(nanos: u64) {
    with(|s| s.gpu_ms_last = nanos as f64 / 1e6);
}

/// `ensure_overlay_tiles_requested`'s nearest-first overlay tile list for
/// this frame (after overzoom mapping), in order.
pub fn overlay_wanted(tiles: &[TileId]) {
    with(|s| {
        s.wanted_rank.clear();
        for (i, t) in tiles.iter().enumerate() {
            s.wanted_rank.entry(*t).or_insert(i);
        }
    });
}

pub fn bake_requested(tile: TileId, rank: usize) {
    with(|s| {
        let rebake = !s.baked_ever.insert(tile);
        let row = BakeRow {
            tile: Some(tile),
            segment: s.segment,
            requested_ms: s.now_ms(),
            rank_at_request: rank,
            rebake,
            ..Default::default()
        };
        s.bakes.push(row);
        let i = s.bakes.len() - 1;
        s.open.insert(tile, i);
    });
}

pub fn bake_started(tile: TileId) {
    with(|s| {
        let t = s.now_ms();
        let rank = s.wanted_rank.get(&tile).copied();
        if let Some(&i) = s.open.get(&tile) {
            s.bakes[i].started_ms = Some(t);
            s.bakes[i].rank_at_start = rank;
        }
    });
}

pub fn bake_finished(tile: TileId, produced: bool, superseded: bool) {
    with(|s| {
        let t = s.now_ms();
        if let Some(&i) = s.open.get(&tile) {
            let b = &mut s.bakes[i];
            if b.started_ms.is_none() {
                b.started_ms = Some(t);
            }
            b.baked_ms = Some(t);
            b.superseded = superseded;
            b.empty = !produced && !superseded;
        }
    });
}

/// The bake queue dropped a waiting bake whose tile left the wanted set.
pub fn bake_dropped(tile: TileId) {
    with(|s| {
        if let Some(i) = s.open.remove(&tile) {
            s.bakes[i].dropped = true;
        }
    });
}

/// The bake's result reached the UI thread; `accepted` is `accept_landed`.
pub fn bake_landed(tile: TileId, accepted: bool) {
    with(|s| {
        let t = s.now_ms();
        if let Some(&i) = s.open.get(&tile) {
            s.bakes[i].landed_ms = Some(t);
            s.bakes[i].discarded = !accepted;
            if !accepted || s.bakes[i].empty {
                s.open.remove(&tile);
            }
        }
    });
}

pub fn overlay_uploaded(tile: TileId, bytes: u64, nanos: u64) {
    with(|s| {
        let t = s.now_ms();
        s.cur.overlay_uploads += 1;
        s.cur.overlay_upload_bytes += bytes;
        s.cur.overlay_upload_ns += nanos;
        let in_cover = s.wanted_rank.contains_key(&tile);
        if let Some(i) = s.open.remove(&tile) {
            s.bakes[i].uploaded_ms = Some(t);
            s.bakes[i].in_cover_at_upload = Some(in_cover);
        }
    });
}

fn classify(
    tile: TileId,
    cached: &dyn Fn(TileId) -> bool,
    empty: &dyn Fn(TileId) -> bool,
) -> Drawn {
    if empty(tile) {
        return Drawn::Empty;
    }
    if cached(tile) {
        return Drawn::Own;
    }
    let ancestor = || {
        let mut c = tile;
        while c.z > 0 {
            c = TileId { z: c.z - 1, x: c.x / 2, y: c.y / 2 };
            if cached(c) {
                return Some(tile.z - c.z);
            }
        }
        None
    };
    let kids = |t: TileId| -> [TileId; 4] {
        [(0, 0), (1, 0), (0, 1), (1, 1)].map(|(dx, dy)| TileId { z: t.z + 1, x: t.x * 2 + dx, y: t.y * 2 + dy })
    };
    let descendants_full = || {
        let c = kids(tile);
        if c.iter().all(|&k| cached(k)) {
            return true;
        }
        if c.iter().any(|&k| cached(k)) {
            return false;
        }
        c.iter().all(|&k| kids(k).iter().all(|&g| cached(g)))
    };
    if descendants_full() {
        return Drawn::Descendants;
    }
    if let Some(d) = ancestor() {
        return Drawn::Ancestor(d);
    }
    Drawn::Hole
}

fn count(c: &mut CoverCounts, d: Drawn) {
    match d {
        Drawn::Own => c.own += 1,
        Drawn::Ancestor(l) => {
            c.ancestor += 1;
            c.ancestor_levels += u32::from(l);
        }
        Drawn::Descendants => c.descendants += 1,
        Drawn::Empty => c.empty += 1,
        Drawn::Hole => c.hole += 1,
    }
}

fn track(
    waiting: &mut HashMap<TileId, (usize, u32, u32)>,
    resolved: &mut Vec<(usize, u32, u32)>,
    left: Option<&mut Vec<(usize, u32, u32)>>,
    segment: usize,
    now: &[(TileId, Drawn)],
) {
    let present: HashSet<TileId> = now.iter().map(|(t, _)| *t).collect();
    let gone: Vec<TileId> = waiting.keys().filter(|t| !present.contains(t)).copied().collect();
    let mut left = left;
    for t in gone {
        let w = waiting.remove(&t).expect("key from the map");
        if let Some(left) = left.as_deref_mut() {
            left.push(w);
        }
    }
    for &(t, d) in now {
        match d {
            Drawn::Own | Drawn::Empty => {
                if let Some(w) = waiting.remove(&t) {
                    resolved.push(w);
                }
            }
            Drawn::Hole => waiting.entry(t).or_insert((segment, 0, 0)).2 += 1,
            _ => waiting.entry(t).or_insert((segment, 0, 0)).1 += 1,
        }
    }
}

/// One overlay source's draw decision for this frame: `wanted` is
/// `wanted_overlay_tiles(visible_and_culled, max_zoom)` as `(display,
/// overlay tile, what paint drew for it)` -- the classification is paint's
/// own (`overlay_tile_picks`), not re-derived here; `cached` is the GPU
/// cache.
pub fn overlay_cover(
    wanted: &[(TileId, TileId, Drawn)],
    cached: &dyn Fn(TileId) -> bool,
    cache_bytes: u64,
    cache_budget: u64,
    cache_entries: u32,
) {
    if !ON.load(Relaxed) {
        return;
    }
    let mut picks = Vec::with_capacity(wanted.len());
    let mut seen = HashSet::new();
    let mut cached_now = HashSet::new();
    let mut wanted_now = HashSet::new();
    for &(display, tile, d) in wanted {
        wanted_now.insert(tile);
        if d == Drawn::Own {
            cached_now.insert(tile);
        }
        if seen.insert(display) {
            picks.push((display, d));
        }
    }
    with(|s| {
        for &(_, d) in &picks {
            count(&mut s.cur.overlay, d);
        }
        // Tiles drawn as their own last frame and no longer cached now:
        // the LRU let go of something it had just drawn.
        let evicted: Vec<TileId> = s.cached_prev.iter().filter(|t| !cached(**t)).copied().collect();
        s.cur.evicted_prev_drawn += evicted.len() as u32;
        s.cur.evicted_in_cover += evicted.iter().filter(|t| wanted_now.contains(t)).count() as u32;
        s.cached_prev = cached_now;
        s.cur.cache_bytes = cache_bytes;
        s.cur.cache_budget = cache_budget;
        s.cur.cache_entries = cache_entries;
        let seg = s.segment;
        let State { waiting, resolved, left_unresolved, .. } = &mut *s;
        track(waiting, resolved, Some(left_unresolved), seg, &picks);
    });
}

/// The basemap's pass 1 over `visible_and_culled`, classified the way
/// pass 1 picks (complete descendants first, then an ancestor).
pub fn basemap_cover(visible: &[TileId], cached: &dyn Fn(TileId) -> bool) {
    if !ON.load(Relaxed) {
        return;
    }
    let picks: Vec<(TileId, Drawn)> = visible.iter().map(|&t| (t, classify(t, cached, &|_| false))).collect();
    with(|s| {
        for &(_, d) in &picks {
            count(&mut s.cur.basemap, d);
        }
        let seg = s.segment;
        let State { basemap_waiting, basemap_resolved, .. } = &mut *s;
        track(basemap_waiting, basemap_resolved, None, seg, &picks);
    });
}

/// Closes the frame: `nanos` is `FrameTimer`'s `paint` duration.
pub(crate) fn frame_end(nanos: u64) {
    with(|s| {
        let cur = std::mem::take(&mut s.cur);
        let running = s
            .open
            .values()
            .filter(|&&i| s.bakes[i].started_ms.is_some() && s.bakes[i].baked_ms.is_none())
            .count() as u32;
        let queued = s.open.values().filter(|&&i| s.bakes[i].started_ms.is_none()).count() as u32;
        let landed_waiting = s
            .open
            .values()
            .filter(|&&i| s.bakes[i].landed_ms.is_some() && s.bakes[i].uploaded_ms.is_none())
            .count() as u32;
        let mut phase_ms: Vec<(&'static str, f64)> =
            cur.phase_ns.iter().map(|(k, v)| (*k, *v as f64 / 1e6)).collect();
        phase_ms.sort_by(|a, b| b.1.total_cmp(&a.1));
        let mut extra_ms: Vec<(&'static str, f64)> =
            cur.extra_ns.iter().map(|(k, v)| (*k, *v as f64 / 1e6)).collect();
        extra_ms.sort_by(|a, b| b.1.total_cmp(&a.1));
        let mut counts: Vec<(&'static str, u32)> = cur.counts.iter().map(|(k, v)| (*k, *v)).collect();
        counts.sort();
        let row = FrameRow {
            extra_ms,
            counts,
            t_ms: s.now_ms(),
            zoom: s.zoom,
            segment: s.segment,
            cpu_ms: nanos as f64 / 1e6,
            phase_ms,
            gpu_ms_last_sample: s.gpu_ms_last,
            overlay: cur.overlay,
            basemap: cur.basemap,
            overlay_uploads: cur.overlay_uploads,
            overlay_upload_bytes: cur.overlay_upload_bytes,
            overlay_upload_ms: cur.overlay_upload_ns as f64 / 1e6,
            bakes_queued: queued,
            bakes_running: running,
            landed_waiting,
            evicted_in_cover: cur.evicted_in_cover,
            evicted_prev_drawn: cur.evicted_prev_drawn,
            cache_bytes: cur.cache_bytes,
            cache_budget: cur.cache_budget,
            cache_entries: cur.cache_entries,
        };
        s.frames.push(row);
    });
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f64::total_cmp);
    v[((v.len() - 1) as f64 * p).round() as usize]
}

fn dist(v: &[u32]) -> String {
    let mut f: Vec<f64> = v.iter().map(|&x| f64::from(x)).collect();
    if f.is_empty() {
        return "-".into();
    }
    format!("n {} p50 {:.0} p95 {:.0} max {:.0}", f.len(), pct(&mut f, 0.5), pct(&mut f, 0.95), pct(&mut f, 1.0))
}

fn ms(v: &mut [f64]) -> String {
    if v.is_empty() {
        return "-".into();
    }
    let n = v.len();
    format!("n {n} p50 {:.0} p95 {:.0} max {:.0} ms", pct(v, 0.5), pct(v, 0.95), pct(v, 1.0))
}

/// The per-segment summary as text (markdown-ish, one block per segment).
pub fn report() -> String {
    let s = state().lock().expect("zoom probe lock");
    let mut out = String::new();
    use std::fmt::Write;
    for (seg, (label, seg_start)) in s.segments.iter().enumerate() {
        let frames: Vec<&FrameRow> = s.frames.iter().filter(|f| f.segment == seg).collect();
        if frames.is_empty() {
            continue;
        }
        let n = frames.len();
        let dur = frames.last().map_or(0.0, |f| f.t_ms) - seg_start;
        let sum = |f: &dyn Fn(&FrameRow) -> u32| frames.iter().map(|r| f(r)).sum::<u32>();
        let maxu = |f: &dyn Fn(&FrameRow) -> u32| frames.iter().map(|r| f(r)).max().unwrap_or(0);
        let hole_frames = frames.iter().filter(|f| f.overlay.hole > 0).count();
        let bm_hole_frames = frames.iter().filter(|f| f.basemap.hole > 0).count();
        let _ = writeln!(out, "## [{seg}] {label}  ({n} frames, {:.0} ms, {:.1} fps)", dur, n as f64 / (dur / 1e3).max(1e-9));
        let _ = writeln!(
            out,
            "overlay: hole tile-frames {} (frames with >=1 hole {hole_frames}, max holes/frame {}) | ancestor tile-frames {} (mean {:.1} levels) | descendant tile-frames {} | own {} | empty {}",
            sum(&|f| f.overlay.hole),
            maxu(&|f| f.overlay.hole),
            sum(&|f| f.overlay.ancestor),
            f64::from(sum(&|f| f.overlay.ancestor_levels)) / f64::from(sum(&|f| f.overlay.ancestor).max(1)),
            sum(&|f| f.overlay.descendants),
            sum(&|f| f.overlay.own),
            sum(&|f| f.overlay.empty),
        );
        let _ = writeln!(
            out,
            "basemap: hole tile-frames {} (frames with >=1 hole {bm_hole_frames}, max {}) | ancestor tile-frames {} (mean {:.1} levels) | descendant tile-frames {} | own {}",
            sum(&|f| f.basemap.hole),
            maxu(&|f| f.basemap.hole),
            sum(&|f| f.basemap.ancestor),
            f64::from(sum(&|f| f.basemap.ancestor_levels)) / f64::from(sum(&|f| f.basemap.ancestor).max(1)),
            sum(&|f| f.basemap.descendants),
            sum(&|f| f.basemap.own),
        );
        // Time to full detail: first frame in the segment with no overlay
        // stand-in or hole, and the same for the basemap.
        let full = |pick: &dyn Fn(&FrameRow) -> bool| {
            frames.iter().find(|f| pick(f)).map_or("never".to_string(), |f| format!("{:.0} ms", f.t_ms - seg_start))
        };
        let _ = writeln!(
            out,
            "time to full detail: overlay {} | basemap {}",
            full(&|f| f.overlay.hole == 0 && f.overlay.ancestor == 0 && f.overlay.descendants == 0 && f.overlay.own + f.overlay.empty > 0),
            full(&|f| f.basemap.hole == 0 && f.basemap.ancestor == 0 && f.basemap.descendants == 0 && f.basemap.own > 0),
        );
        let wait = |v: &[(usize, u32, u32)], which: usize| -> Vec<u32> {
            v.iter().filter(|w| w.0 == seg).map(|w| if which == 1 { w.1 + w.2 } else { w.2 }).collect()
        };
        let _ = writeln!(
            out,
            "overlay display tiles entering here, frames before own tile: stand-in+hole {} | hole only {} | left cover unresolved {}",
            dist(&wait(&s.resolved, 1)),
            dist(&wait(&s.resolved, 2)),
            s.left_unresolved.iter().filter(|w| w.0 == seg).count()
        );
        let _ = writeln!(out, "basemap display tiles, frames before own tile: {}", dist(&wait(&s.basemap_resolved, 1)));
        let bakes: Vec<&BakeRow> = s.bakes.iter().filter(|b| b.segment == seg).collect();
        let mut q: Vec<f64> = bakes.iter().filter_map(|b| Some(b.started_ms? - b.requested_ms)).collect();
        let mut run: Vec<f64> =
            bakes.iter().filter(|b| !b.superseded).filter_map(|b| Some(b.baked_ms? - b.started_ms?)).collect();
        let mut up: Vec<f64> = bakes.iter().filter_map(|b| Some(b.uploaded_ms? - b.landed_ms?)).collect();
        let mut total: Vec<f64> = bakes.iter().filter_map(|b| Some(b.uploaded_ms? - b.requested_ms)).collect();
        // A bake cancelled at its first poll did no work: not stale.
        let stale_start =
            bakes.iter().filter(|b| b.started_ms.is_some() && b.rank_at_start.is_none() && !(b.superseded && b.baked_ms == b.started_ms)).count();
        let stale_up = bakes.iter().filter(|b| b.in_cover_at_upload == Some(false)).count();
        let _ = writeln!(
            out,
            "bakes requested {} (rebakes {}) | started {} | stale at start (camera left) {stale_start} | superseded {} | empty {} | discarded {} | uploaded {} (not in cover at upload {stale_up}) | dropped unstarted {} | never started by end {}",
            bakes.len(),
            bakes.iter().filter(|b| b.rebake).count(),
            bakes.iter().filter(|b| b.started_ms.is_some()).count(),
            bakes.iter().filter(|b| b.superseded).count(),
            bakes.iter().filter(|b| b.empty).count(),
            bakes.iter().filter(|b| b.discarded).count(),
            bakes.iter().filter(|b| b.uploaded_ms.is_some()).count(),
            bakes.iter().filter(|b| b.dropped).count(),
            bakes.iter().filter(|b| b.started_ms.is_none() && !b.dropped).count(),
        );
        let _ = writeln!(out, "  queue wait {} | bake {} | land->upload {} | request->upload {}", ms(&mut q), ms(&mut run), ms(&mut up), ms(&mut total));
        let never: Vec<String> = bakes
            .iter()
            .filter(|b| b.started_ms.is_none() && !b.dropped)
            .filter_map(|b| b.tile)
            .map(|t| {
                let later = s.bakes.iter().filter(|o| o.tile == Some(t) && o.requested_ms > 0.0).count();
                format!("{}/{}/{}(rows {later})", t.z, t.x, t.y)
            })
            .collect();
        if !never.is_empty() {
            let _ = writeln!(out, "  never started: {}", never.join(" "));
        }
        let mut rank_start: Vec<f64> = bakes.iter().filter_map(|b| b.rank_at_start.map(|r| r as f64)).collect();
        let mut rank_req: Vec<f64> = bakes.iter().map(|b| b.rank_at_request as f64).collect();
        let _ = writeln!(
            out,
            "  nearest-first rank: at request p50 {:.0} | at start (still in cover) p50 {:.0} p95 {:.0}",
            pct(&mut rank_req, 0.5),
            pct(&mut rank_start, 0.5),
            pct(&mut rank_start, 0.95)
        );
        let _ = writeln!(
            out,
            "  in flight max: queued {} running {} landed-not-uploaded {}",
            maxu(&|f| f.bakes_queued),
            maxu(&|f| f.bakes_running),
            maxu(&|f| f.landed_waiting)
        );
        let _ = writeln!(
            out,
            "cache: evicted while in cover {} | evicted after being drawn last frame {} | bytes max {:.1} MB of {:.0} MB | entries max {}",
            sum(&|f| f.evicted_in_cover),
            sum(&|f| f.evicted_prev_drawn),
            frames.iter().map(|f| f.cache_bytes).max().unwrap_or(0) as f64 / 1e6,
            frames.iter().map(|f| f.cache_budget).max().unwrap_or(0) as f64 / 1e6,
            maxu(&|f| f.cache_entries),
        );
        let mut cpu: Vec<f64> = frames.iter().map(|f| f.cpu_ms).collect();
        let mut gpu: Vec<f64> = frames.iter().map(|f| f.gpu_ms_last_sample).collect();
        let spikes: Vec<&&FrameRow> = frames.iter().filter(|f| f.cpu_ms > 16.7).collect();
        let mut cause: HashMap<&'static str, (u32, f64)> = HashMap::new();
        for f in &spikes {
            let top = f.phase_ms.first().map_or("?", |p| p.0);
            let e = cause.entry(top).or_default();
            e.0 += 1;
            e.1 = e.1.max(f.cpu_ms);
        }
        let mut cause: Vec<_> = cause.into_iter().collect();
        cause.sort_by_key(|a| std::cmp::Reverse(a.1 .0));
        let _ = writeln!(
            out,
            "frame: cpu p50 {:.1} p95 {:.1} max {:.1} ms | gpu (last sample) p50 {:.1} max {:.1} ms | >16.7 ms cpu: {} frames, top phase: {}",
            pct(&mut cpu.clone(), 0.5),
            pct(&mut cpu.clone(), 0.95),
            pct(&mut cpu, 1.0),
            pct(&mut gpu.clone(), 0.5),
            pct(&mut gpu, 1.0),
            spikes.len(),
            cause.iter().map(|(k, (n, m))| format!("{k} x{n} (max {m:.0} ms)")).collect::<Vec<_>>().join(", ")
        );
        let up_frames: Vec<&&FrameRow> = frames.iter().filter(|f| f.overlay_uploads > 0).collect();
        let _ = writeln!(
            out,
            "overlay upload frames {}: tiles/frame max {} | bytes/frame max {:.1} MB | upload ms/frame max {:.1}",
            up_frames.len(),
            up_frames.iter().map(|f| f.overlay_uploads).max().unwrap_or(0),
            up_frames.iter().map(|f| f.overlay_upload_bytes).max().unwrap_or(0) as f64 / 1e6,
            up_frames.iter().map(|f| f.overlay_upload_ms).fold(0.0, f64::max),
        );
        let _ = writeln!(out);
    }
    out
}

/// Every frame as CSV (one header row).
pub fn frames_csv() -> String {
    let s = state().lock().expect("zoom probe lock");
    let mut out = String::from(
        "t_ms,segment,zoom,cpu_ms,gpu_ms,top_phase,top_phase_ms,ov_own,ov_anc,ov_desc,ov_empty,ov_hole,bm_own,bm_anc,bm_desc,bm_hole,uploads,upload_bytes,upload_ms,queued,running,landed_waiting,evicted_in_cover,cache_bytes,cache_entries,phases,extra,counts\n",
    );
    use std::fmt::Write;
    for f in &s.frames {
        let (p, pm) = f.phase_ms.first().copied().unwrap_or(("-", 0.0));
        let _ = writeln!(
            out,
            "{:.1},{},{:.3},{:.2},{:.2},{p},{pm:.2},{},{},{},{},{},{},{},{},{},{},{},{:.2},{},{},{},{},{},{},{},{},{}",
            f.t_ms,
            f.segment,
            f.zoom,
            f.cpu_ms,
            f.gpu_ms_last_sample,
            f.overlay.own,
            f.overlay.ancestor,
            f.overlay.descendants,
            f.overlay.empty,
            f.overlay.hole,
            f.basemap.own,
            f.basemap.ancestor,
            f.basemap.descendants,
            f.basemap.hole,
            f.overlay_uploads,
            f.overlay_upload_bytes,
            f.overlay_upload_ms,
            f.bakes_queued,
            f.bakes_running,
            f.landed_waiting,
            f.evicted_in_cover,
            f.cache_bytes,
            f.cache_entries,
            f.phase_ms.iter().map(|(k, v)| format!("{k}={v:.2}")).collect::<Vec<_>>().join(";"),
            f.extra_ms.iter().map(|(k, v)| format!("{k}={v:.2}")).collect::<Vec<_>>().join(";"),
            f.counts.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(";"),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_follows_the_basemap_pick_order() {
        let t = TileId { z: 5, x: 10, y: 12 };
        let parent = TileId { z: 4, x: 5, y: 6 };
        assert_eq!(classify(t, &|c| c == t, &|_| false), Drawn::Own);
        assert_eq!(classify(t, &|c| c == parent, &|_| false), Drawn::Ancestor(1));
        assert_eq!(classify(t, &|_| false, &|_| false), Drawn::Hole);
        assert_eq!(classify(t, &|c| c.z == 6 || c == parent, &|_| false), Drawn::Descendants);
    }
}

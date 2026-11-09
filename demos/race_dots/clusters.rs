//! Cluster mode's data (spec §5): the donut clusters of
//! `race-clusters-2020.pmtiles` (`tools/race-build export-clusters`, the
//! `CLT1` tiles of `overlay::clusters_tile`), one zoom's tiles at a time.
//!
//! - [`ClusterArchive`] opens the archive (its `race_clusters` metadata
//!   checked) and names the byte range of a tile.
//! - [`decode_tile`] decodes one tile for the zoom it was asked at and
//!   **refuses every record whose `expansion_zoom` is not above that zoom**
//!   (Task 12's L4: the tile does not know its own zoom, so a click on such
//!   a record would zoom out or stay put). A refused record is counted,
//!   never drawn, never a panic.
//! - [`ClusterTiles`] is the sans-IO bookkeeping: the tiles wanted at the
//!   view's tile zoom (`floor(zoom)` clamped to 3..=11), what is in flight,
//!   what landed, and the records to draw. The drawn set changes only
//!   when every wanted tile has landed, so a zoom step swaps one zoom's
//!   clusters for the next in one frame instead of showing a half-loaded
//!   mix of the two (each zoom's tiles hold all 331 M people, so any
//!   mixture would count people twice or not at all).
//!   A tile whose FETCH failed is asked for again after a bounded
//!   back-off ([`retry_after`]: 1 s doubling to 30 s); one whose bytes
//!   failed to decode is not (the same bytes would fail again).
//! - [`drawable`] / [`labels_geojson`] turn the records into the engine's
//!   donuts (at most `MAX_DONUTS`, the rest counted as truncated) and the
//!   count labels' point source.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui_maps::geo::TileId;
use gpui_maps::overlay::clusters_tile::{self, ClusterRecord, DOTS_ZOOM, MAX_ZOOM, MIN_ZOOM};
use gpui_maps::overlay::dotgrid::pmtiles_dir::{zxy_to_tile_id, PmEntry};
use gpui_maps::overlay::dotgrid::pmtiles_source::{find_entry, ArchiveOpener, OpenedArchive};
use gpui_maps::overlay::dotgrid::{GridError, GridRequest};
use gpui_maps::render::donut::{Donut, MAX_DONUTS};
use std::time::{Duration, Instant};

use super::walker;

/// The donut layer's id on the map.
pub const DONUT_LAYER: &str = "race-clusters";
/// The count labels' overlay source.
pub const LABEL_SOURCE: &str = "race-cluster-labels";

/// The archive `tools/race-build export-clusters` writes by default.
pub fn default_pmtiles() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".cache/gpui-map-data/census2020/race-clusters-2020.pmtiles")
}

/// An opened cluster archive: where it is and its tile directory.
pub struct ClusterArchive {
    entries: Vec<PmEntry>,
    location: String,
    /// The archive metadata's credit, shown on the panel's About card.
    pub attribution: String,
}

impl ClusterArchive {
    /// The byte range of `tile`, or [`GridRequest::Absent`] for a tile the
    /// archive has no entry for (no people there) or a zoom it does not cut.
    pub fn request(&self, tile: TileId) -> GridRequest {
        if !(MIN_ZOOM..=MAX_ZOOM).contains(&tile.z) {
            return GridRequest::Absent;
        }
        match find_entry(&self.entries, zxy_to_tile_id(tile.z, tile.x, tile.y)) {
            Some(e) => GridRequest::Range { location: self.location.clone(), offset: e.offset, length: u64::from(e.length) },
            None => GridRequest::Absent,
        }
    }
}

impl ClusterArchive {
    /// An empty archive carrying `attribution`, for the panel's tests.
    #[cfg(test)]
    pub fn for_test(attribution: &str) -> Arc<Self> {
        Arc::new(Self { entries: Vec::new(), location: String::new(), attribution: attribution.to_string() })
    }
}

/// Opens the cluster archive at `path` (native file reads).
pub fn open_clusters(path: &Path) -> Result<Arc<ClusterArchive>, String> {
    if !path.exists() {
        return Err(format!(
            "no cluster archive at {} -- build it with `cargo run --release --manifest-path tools/race-build/Cargo.toml -- export-clusters`",
            path.display()
        ));
    }
    let execute = gpui_maps::overlay::dotgrid::fetch_native::execute;
    let open = || -> Result<Arc<ClusterArchive>, GridError> {
        let mut opener = ArchiveOpener::new(path.to_string_lossy().to_string(), clusters_tile::parse_cluster_meta);
        let first = execute(&opener.first_request())?;
        let requests = opener.after_first(&first)?;
        let bodies = requests.iter().map(execute).collect::<Result<Vec<_>, _>>()?;
        let OpenedArchive { meta, entries, location } = opener.finish_archive(&bodies)?;
        Ok(Arc::new(ClusterArchive { entries, location, attribution: meta.attribution }))
    };
    open().map_err(|e| format!("{}: {e}", path.display()))
}

/// The cluster tile zoom for a camera at `zoom`: `floor(zoom)`, clamped
/// to the archive's 3..=11.
pub fn tile_zoom(zoom: f64) -> u8 {
    (zoom.floor().max(0.0) as u32).clamp(u32::from(MIN_ZOOM), u32::from(MAX_ZOOM)) as u8
}

/// One decoded tile: the records to draw, and how many were refused.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Decoded {
    pub records: Vec<ClusterRecord>,
    /// Records whose `expansion_zoom` was not above the tile's zoom.
    pub refused: usize,
}

/// Whether `r`, from a tile at zoom `z`, is sound: its expansion zoom must
/// be above `z` (a click zooms IN) and at most [`DOTS_ZOOM`].
pub fn record_fits_zoom(r: &ClusterRecord, z: u8) -> bool {
    r.expansion_zoom > z && r.expansion_zoom <= DOTS_ZOOM
}

/// Decodes `tile`'s bytes (empty: no people there) and refuses every
/// record that does not fit the tile's zoom ([`record_fits_zoom`]).
pub fn decode_tile(tile: TileId, body: &[u8]) -> Result<Decoded, GridError> {
    if body.is_empty() {
        return Ok(Decoded::default());
    }
    let mut records = clusters_tile::decode(body)?;
    let before = records.len();
    records.retain(|r| record_fits_zoom(r, tile.z));
    Ok(Decoded { refused: before - records.len(), records })
}

/// One tile to fetch: `request` through the shell's fetcher, then
/// [`decode_tile`], then [`ClusterTiles::land`].
#[derive(Debug, Clone)]
pub struct Fetch {
    pub tile: TileId,
    pub request: GridRequest,
}

/// Why a tile has no records.
#[derive(Debug, Clone, PartialEq)]
pub struct TileError {
    /// Worth asking again: the fetch failed, not the bytes.
    pub retry: bool,
    pub why: String,
}

/// Fetches and decodes `fetch` with `execute` (off the UI thread).
pub fn run_fetch(fetch: &Fetch, execute: impl Fn(&GridRequest) -> Result<Vec<u8>, GridError>) -> Result<Decoded, TileError> {
    let name = |e: GridError| format!("{}/{}/{}: {e}", fetch.tile.z, fetch.tile.x, fetch.tile.y);
    let bytes = execute(&fetch.request).map_err(|e| TileError { retry: true, why: name(e) })?;
    decode_tile(fetch.tile, &bytes).map_err(|e| TileError { retry: false, why: name(e) })
}

/// How long after its `failures`-th failed fetch (1-based) a tile is
/// asked for again: 1 s, doubling, at most [`MAX_RETRY_AFTER`].
pub fn retry_after(failures: u32) -> Duration {
    Duration::from_secs(1u64 << failures.saturating_sub(1).min(5)).min(MAX_RETRY_AFTER)
}

/// The longest back-off.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);

/// Tiles fetched at once, at most.
pub const MAX_IN_FLIGHT: usize = 8;
/// Tiles one view loads, at most.
pub const MAX_TILES: usize = 96;
/// Landed tiles kept, at most (the least recently wanted beyond it go).
/// The densest real tile holds 92 records, so this is well under 2 MB.
pub const MAX_CACHED_TILES: usize = 256;

#[derive(Debug, Clone)]
enum Slot {
    Loading,
    Ready { records: Arc<Vec<ClusterRecord>>, used: u64 },
    /// `retry_at`: `None` for bytes that failed to decode (never asked
    /// again), else when a wanted tile may be fetched again.
    Failed { used: u64, retry_at: Option<Instant> },
}

/// The sans-IO tile bookkeeping of cluster mode. See the module doc.
pub struct ClusterTiles {
    archive: Arc<ClusterArchive>,
    slots: HashMap<TileId, Slot>,
    wanted: Vec<TileId>,
    in_flight: usize,
    clock: u64,
    drawn: Vec<ClusterRecord>,
    /// Bumped whenever `drawn` changes.
    generation: u64,
    /// Tiles that failed to fetch or decode, and the last reason.
    pub errors: u64,
    pub last_error: Option<String>,
    /// Records refused by [`decode_tile`], over this panel's life.
    pub refused: u64,
    /// Failed fetches in a row, per tile (cleared when one lands).
    failures: HashMap<TileId, u32>,
}

impl ClusterTiles {
    /// The cluster archive's credit (its metadata `attribution`).
    pub fn attribution(&self) -> &str {
        &self.archive.attribution
    }

    pub fn new(archive: Arc<ClusterArchive>) -> Self {
        Self {
            archive,
            slots: HashMap::new(),
            wanted: Vec::new(),
            in_flight: 0,
            clock: 0,
            drawn: Vec::new(),
            generation: 0,
            errors: 0,
            last_error: None,
            refused: 0,
            failures: HashMap::new(),
        }
    }

    /// Wants `tiles` (deduplicated, at most [`MAX_TILES`]); returns the
    /// fetches to start now, failed fetches whose back-off has run out by
    /// `now` included.
    pub fn want(&mut self, tiles: Vec<TileId>, now: Instant) -> Vec<Fetch> {
        self.clock += 1;
        let mut seen = std::collections::HashSet::new();
        let mut tiles = tiles;
        tiles.retain(|t| seen.insert(*t));
        tiles.truncate(MAX_TILES);
        self.wanted = tiles;
        let mut fetches = Vec::new();
        for &tile in &self.wanted {
            let due = matches!(self.slots.get(&tile), Some(Slot::Failed { retry_at: Some(at), .. }) if *at <= now);
            match self.slots.get_mut(&tile) {
                Some(Slot::Ready { used, .. } | Slot::Failed { used, .. }) if !due => *used = self.clock,
                Some(Slot::Loading) => {}
                _ if self.in_flight < MAX_IN_FLIGHT => {
                    self.slots.insert(tile, Slot::Loading);
                    self.in_flight += 1;
                    fetches.push(Fetch { tile, request: self.archive.request(tile) });
                }
                _ => {}
            }
        }
        self.evict();
        self.refresh_drawn();
        fetches
    }

    /// Wants nothing (the mode is off): nothing is drawn.
    pub fn clear_wanted(&mut self) {
        self.wanted.clear();
        if !self.drawn.is_empty() {
            self.drawn.clear();
            self.generation += 1;
        }
    }

    /// Whether a wanted tile's back-off has run out by `now` (the panel
    /// then plans again, so [`Self::want`] re-asks it).
    pub fn retry_due(&self, now: Instant) -> bool {
        self.wanted.iter().any(|t| matches!(self.slots.get(t), Some(Slot::Failed { retry_at: Some(at), .. }) if *at <= now))
    }

    /// A fetch came back at `now`.
    pub fn land(&mut self, tile: TileId, result: Result<Decoded, TileError>, now: Instant) {
        self.in_flight = self.in_flight.saturating_sub(1);
        let slot = match result {
            Ok(d) => {
                if d.refused > 0 {
                    self.refused += d.refused as u64;
                    eprintln!("race_dots: cluster tile {}/{}/{}: {} records refused (expansion zoom not above z{})", tile.z, tile.x, tile.y, d.refused, tile.z);
                }
                self.failures.remove(&tile);
                Slot::Ready { records: Arc::new(d.records), used: self.clock }
            }
            Err(e) => {
                self.errors += 1;
                eprintln!("race_dots: cluster tile {}", e.why);
                self.last_error = Some(e.why);
                let retry_at = e.retry.then(|| {
                    let n = self.failures.entry(tile).or_insert(0);
                    *n += 1;
                    now + retry_after(*n)
                });
                Slot::Failed { used: self.clock, retry_at }
            }
        };
        self.slots.insert(tile, slot);
        self.refresh_drawn();
    }

    /// Every wanted tile has landed (ready or failed).
    pub fn settled(&self) -> bool {
        self.wanted.iter().all(|t| matches!(self.slots.get(t), Some(Slot::Ready { .. } | Slot::Failed { .. })))
    }

    /// Some wanted tile has not been asked for yet (the in-flight cap).
    pub fn has_unrequested(&self) -> bool {
        self.wanted.iter().any(|t| !self.slots.contains_key(t))
    }

    /// The records to draw.
    pub fn records(&self) -> &[ClusterRecord] {
        &self.drawn
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    fn refresh_drawn(&mut self) {
        if !self.settled() {
            return;
        }
        let mut drawn = Vec::new();
        for t in &self.wanted {
            if let Some(Slot::Ready { records, .. }) = self.slots.get(t) {
                drawn.extend_from_slice(records);
            }
        }
        if drawn != self.drawn {
            self.drawn = drawn;
            self.generation += 1;
        }
    }

    fn evict(&mut self) {
        if self.slots.len() <= MAX_CACHED_TILES {
            return;
        }
        let wanted: std::collections::HashSet<TileId> = self.wanted.iter().copied().collect();
        let mut old: Vec<(u64, TileId)> = self
            .slots
            .iter()
            .filter(|(t, _)| !wanted.contains(t))
            .filter_map(|(t, s)| match s {
                Slot::Ready { used, .. } | Slot::Failed { used, .. } => Some((*used, *t)),
                Slot::Loading => None,
            })
            .collect();
        old.sort_unstable_by_key(|(used, _)| *used);
        let excess = self.slots.len() - MAX_CACHED_TILES;
        for (_, t) in old.into_iter().take(excess) {
            self.slots.remove(&t);
            self.failures.remove(&t);
        }
    }
}

/// What the map is given: the donuts, the record behind each (the same
/// order: `donut_at`'s index is an index here), and how many records the
/// engine's cap left out.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Drawable {
    pub donuts: Vec<Donut>,
    pub records: Vec<ClusterRecord>,
    /// Records past `MAX_DONUTS` (the smallest), not drawn and not labelled.
    pub truncated: usize,
}

/// `records` as donuts, largest first (so a small donut is drawn, and
/// hit, over a large neighbour): Walker's size on the population, the
/// legend's hidden groups taken out of the shares, at `alpha`. A record
/// with no visible people is left out (it would draw nothing); past
/// `MAX_DONUTS` the smallest are dropped here, and counted, rather than
/// silently by `MapView::set_donuts`.
pub fn drawable(records: &[ClusterRecord], mask: u8, alpha: f32) -> Drawable {
    let visible = |r: &ClusterRecord| (0..8).any(|g| mask & (1 << g) != 0 && r.counts[g] > 0);
    let mut kept: Vec<ClusterRecord> = records.iter().filter(|r| visible(r)).copied().collect();
    kept.sort_by_key(|r| std::cmp::Reverse(r.pop));
    let truncated = kept.len().saturating_sub(MAX_DONUTS);
    kept.truncate(MAX_DONUTS);
    let donuts = kept
        .iter()
        .map(|r| {
            let counts = std::array::from_fn(|g| if mask & (1 << g) != 0 { r.counts[g] } else { 0 });
            Donut { lon: r.lon, lat: r.lat, radius_px: walker::donut_size(r.pop) / 2.0, counts, alpha }
        })
        .collect();
    Drawable { donuts, records: kept, truncated }
}

/// The count labels as a point source: one label per cluster (not per
/// leaf block, as Walker labels only clusters), `count_label(pop)` in 12
/// px `#263443` at `alpha`, centred on its donut, never dropped for
/// collision (the donuts themselves are spaced a cluster radius apart). A
/// cluster whose groups are all hidden has no donut and no label. Pass
/// [`Drawable::records`], so a truncated record has no label either.
///
/// With groups hidden in the legend the label (like the donut's size)
/// still counts EVERYONE in the cluster, while the ring shows only the
/// visible groups' shares: "12k" beside a mostly hidden ring is the
/// cluster's population, not the visible part of it (the About text says
/// so).
pub fn labels_geojson(records: &[ClusterRecord], mask: u8, alpha: f32) -> String {
    let features: Vec<serde_json::Value> = records
        .iter()
        .filter(|r| !r.leaf && alpha > 0.0 && (0..8).any(|g| mask & (1 << g) != 0 && r.counts[g] > 0))
        .map(|r| {
            serde_json::json!({
                "type": "Feature",
                "geometry": {"type": "Point", "coordinates": [r.lon, r.lat]},
                "properties": {
                    "circle-radius": 0,
                    "text-field": walker::count_label(r.pop),
                    "text-size": walker::COUNT_LABEL_SIZE,
                    "text-color": label_color(alpha),
                    "text-allow-overlap": true,
                },
            })
        })
        .collect();
    serde_json::json!({"type": "FeatureCollection", "features": features}).to_string()
}

/// [`walker::COUNT_LABEL_COLOR`] (`#263443`) at `alpha`, as `#rrggbbaa`
/// (the overlay's `text-color` takes no `rgba()`).
pub fn label_color(alpha: f32) -> String {
    format!("{}{:02x}", walker::COUNT_LABEL_COLOR, (alpha.clamp(0.0, 1.0) * 255.0).round() as u8)
}

/// Alpha to 1/100: the donuts and labels are re-sent only when this
/// changes.
pub fn quantise_alpha(alpha: f32) -> f32 {
    (alpha.clamp(0.0, 1.0) * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(pop: u32, expansion_zoom: u8, leaf: bool) -> ClusterRecord {
        ClusterRecord {
            lon: -73.9,
            lat: 40.7,
            blocks: if leaf { 1 } else { 5 },
            pop,
            counts: [pop / 2, pop - pop / 2, 0, 0, 0, 0, 0, 0],
            expansion_zoom,
            leaf,
        }
    }

    #[test]
    fn tile_zoom_is_the_floor_clamped_to_the_archive() {
        assert_eq!(tile_zoom(2.0), 3);
        assert_eq!(tile_zoom(3.5), 3);
        assert_eq!(tile_zoom(8.99), 8);
        assert_eq!(tile_zoom(11.8), 11);
        assert_eq!(tile_zoom(14.0), 11);
    }

    /// Task 12's L4, the source's half: a record whose expansion zoom is
    /// not above the requested tile's zoom is refused and counted, the
    /// rest kept -- and no panic on any of it.
    #[test]
    fn records_whose_expansion_zoom_is_not_above_the_tile_zoom_are_refused() {
        let tile = TileId { z: 8, x: 75, y: 96 };
        let body = clusters_tile::encode(&[rec(900, 9, false), rec(800, 8, false), rec(700, 3, false), rec(40, 12, true), rec(600, 12, false)]);
        let d = decode_tile(tile, &body).expect("a sound tile");
        assert_eq!(d.refused, 2);
        assert_eq!(d.records.iter().map(|r| r.pop).collect::<Vec<_>>(), vec![900, 40, 600]);
        // The same bytes asked for at z11 keep only what zooms past 11.
        let d11 = decode_tile(TileId { z: 11, x: 0, y: 0 }, &body).expect("a sound tile");
        assert_eq!((d11.refused, d11.records.len()), (3, 2));
        // A z12 request (none are cut) refuses everything.
        assert_eq!(decode_tile(TileId { z: 12, x: 0, y: 0 }, &body).expect("sound").records.len(), 0);
        // No bytes: no people; garbage: an error, not a panic.
        assert_eq!(decode_tile(tile, &[]).expect("empty"), Decoded::default());
        assert!(decode_tile(tile, b"not a tile").is_err());
    }

    #[test]
    fn landing_a_tile_counts_its_refused_records() {
        let archive = Arc::new(ClusterArchive { entries: Vec::new(), location: "mem".into(), attribution: String::new() });
        let mut tiles = ClusterTiles::new(archive);
        let t = TileId { z: 5, x: 9, y: 12 };
        let now = Instant::now();
        let fetches = tiles.want(vec![t, t], now);
        assert_eq!(fetches.len(), 1, "deduplicated");
        assert!(matches!(fetches[0].request, GridRequest::Absent), "no entry: absent");
        assert!(!tiles.settled());
        let body = clusters_tile::encode(&[rec(900, 9, false), rec(800, 5, false)]);
        tiles.land(t, run_fetch(&fetches[0], |_| Ok(body.clone())), now);
        assert!(tiles.settled());
        assert_eq!((tiles.refused, tiles.records().len()), (1, 1));
    }

    #[test]
    fn the_drawn_set_swaps_only_when_every_wanted_tile_landed() {
        let archive = Arc::new(ClusterArchive { entries: Vec::new(), location: "mem".into(), attribution: String::new() });
        let mut tiles = ClusterTiles::new(archive);
        let now = Instant::now();
        let a = TileId { z: 4, x: 3, y: 5 };
        tiles.want(vec![a], now);
        tiles.land(a, Ok(Decoded { records: vec![rec(100, 6, false)], refused: 0 }), now);
        let g = tiles.generation();
        assert_eq!(tiles.records().len(), 1);
        // The next zoom's two tiles: the old set stays until both land.
        let (c, d) = (TileId { z: 5, x: 6, y: 10 }, TileId { z: 5, x: 7, y: 10 });
        tiles.want(vec![c, d], now);
        tiles.land(c, Ok(Decoded { records: vec![rec(60, 7, false)], refused: 0 }), now);
        assert_eq!((tiles.records().len(), tiles.generation()), (1, g));
        tiles.land(d, Err(TileError { retry: true, why: "5/7/10: io".into() }), now);
        assert_eq!(tiles.records().iter().map(|r| r.pop).collect::<Vec<_>>(), vec![60]);
        assert_eq!(tiles.errors, 1);
        assert!(tiles.generation() > g);
        tiles.clear_wanted();
        assert!(tiles.records().is_empty());
    }

    #[test]
    fn the_archives_credit_is_kept_for_the_panel() {
        let archive = Arc::new(ClusterArchive { entries: Vec::new(), location: "mem".into(), attribution: "U.S. Census Bureau".into() });
        assert_eq!(ClusterTiles::new(archive).attribution(), "U.S. Census Bureau");
    }

    fn archive() -> Arc<ClusterArchive> {
        Arc::new(ClusterArchive { entries: Vec::new(), location: "mem".into(), attribution: String::new() })
    }

    fn tile(i: u32) -> TileId {
        TileId { z: 11, x: i, y: 7 }
    }

    #[test]
    fn donuts_are_walkers_sizes_largest_first_with_hidden_groups_out() {
        let records = [rec(50, 12, true), rec(3_000, 9, false), rec(120, 12, true)];
        let d = drawable(&records, 0b1111_1110, 0.5);
        assert_eq!(d.records.iter().map(|r| r.pop).collect::<Vec<_>>(), vec![3_000, 120, 50]);
        assert_eq!(d.donuts.iter().map(|d| d.radius_px).collect::<Vec<_>>(), vec![11.0, 7.5, 6.0]);
        assert_eq!(d.donuts[0].counts[0], 0, "group 0 hidden");
        assert_eq!(d.donuts[0].counts[1], 1_500);
        assert!(d.donuts.iter().all(|d| d.alpha == 0.5));
        assert_eq!(d.truncated, 0);
        // Every group a record has hidden: no donut for it.
        assert!(drawable(&records, 0b1111_1100, 1.0).donuts.is_empty());
    }

    /// Review L3: past the engine's cap the smallest records are dropped
    /// here and counted, and they get no label.
    #[test]
    fn past_the_cap_the_smallest_are_truncated_and_counted() {
        let records: Vec<ClusterRecord> = (0..MAX_DONUTS as u32 + 5).map(|i| rec(1_000 + i, 9, false)).collect();
        let d = drawable(&records, 0xFF, 1.0);
        assert_eq!((d.donuts.len(), d.records.len(), d.truncated), (MAX_DONUTS, MAX_DONUTS, 5));
        assert_eq!(d.records.last().map(|r| r.pop), Some(1_005), "the five smallest went");
        let labels: serde_json::Value = serde_json::from_str(&labels_geojson(&d.records, 0xFF, 1.0)).unwrap();
        assert_eq!(labels["features"].as_array().unwrap().len(), MAX_DONUTS);
    }

    /// Review L4: a failed fetch is asked for again after its back-off,
    /// which doubles and is bounded; a decode failure is not.
    #[test]
    fn a_failed_fetch_is_retried_after_a_bounded_backoff() {
        assert_eq!(retry_after(1), Duration::from_secs(1));
        assert_eq!(retry_after(2), Duration::from_secs(2));
        assert_eq!(retry_after(5), Duration::from_secs(16));
        assert_eq!(retry_after(6), MAX_RETRY_AFTER);
        assert_eq!(retry_after(u32::MAX), MAX_RETRY_AFTER);
        let mut tiles = ClusterTiles::new(archive());
        let t0 = Instant::now();
        let (t, bad) = (tile(1), tile(2));
        assert_eq!(tiles.want(vec![t, bad], t0).len(), 2);
        tiles.land(t, Err(TileError { retry: true, why: "io".into() }), t0);
        tiles.land(bad, Err(TileError { retry: false, why: "corrupt".into() }), t0);
        // Not before its back-off...
        assert!(!tiles.retry_due(t0 + Duration::from_millis(999)));
        assert!(tiles.want(vec![t, bad], t0 + Duration::from_millis(999)).is_empty());
        // ...then once, and the corrupt tile never.
        assert!(tiles.retry_due(t0 + Duration::from_secs(1)));
        let again = tiles.want(vec![t, bad], t0 + Duration::from_secs(1));
        assert_eq!(again.iter().map(|f| f.tile).collect::<Vec<_>>(), vec![t]);
        // A second failure waits 2 s; a success clears the count.
        let t1 = t0 + Duration::from_secs(1);
        tiles.land(t, Err(TileError { retry: true, why: "io".into() }), t1);
        assert!(!tiles.retry_due(t1 + Duration::from_millis(1_999)));
        assert!(tiles.retry_due(t1 + Duration::from_secs(2)));
        assert_eq!(tiles.want(vec![t], t1 + Duration::from_secs(2)).len(), 1);
        tiles.land(t, Ok(Decoded::default()), t1 + Duration::from_secs(2));
        assert!(tiles.failures.is_empty());
        assert_eq!(tiles.errors, 3);
    }

    /// Review L5: at most `MAX_IN_FLIGHT` fetches at once, the rest asked
    /// as fetches land.
    #[test]
    fn fetches_are_capped_in_flight() {
        let mut tiles = ClusterTiles::new(archive());
        let now = Instant::now();
        let wanted: Vec<TileId> = (0..20).map(tile).collect();
        let first = tiles.want(wanted.clone(), now);
        assert_eq!(first.len(), MAX_IN_FLIGHT);
        assert!(tiles.has_unrequested());
        assert!(tiles.want(wanted.clone(), now).is_empty(), "nothing more while 8 are out");
        for f in &first[..3] {
            tiles.land(f.tile, Ok(Decoded::default()), now);
        }
        assert_eq!(tiles.want(wanted.clone(), now).len(), 3);
        let mut out = MAX_IN_FLIGHT;
        let mut landed = 3;
        while landed < 20 {
            // Land whatever is loading, then ask again.
            let loading: Vec<TileId> = wanted.iter().copied().filter(|t| matches!(tiles.slots.get(t), Some(Slot::Loading))).collect();
            assert!(loading.len() <= MAX_IN_FLIGHT);
            for t in loading {
                tiles.land(t, Ok(Decoded::default()), now);
                landed += 1;
            }
            out = tiles.want(wanted.clone(), now).len();
        }
        assert_eq!(out, 0);
        assert!(!tiles.has_unrequested() && tiles.settled());
    }

    /// Review L5: the cache holds at most `MAX_CACHED_TILES`, evicting the
    /// least recently wanted landed tiles, never a wanted or loading one.
    #[test]
    fn the_cache_evicts_old_unwanted_tiles_only() {
        let mut tiles = ClusterTiles::new(archive());
        let now = Instant::now();
        // 40 views of 8 tiles each: 320 distinct tiles, all landed.
        for v in 0..40u32 {
            let view: Vec<TileId> = (0..8).map(|i| tile(v * 8 + i)).collect();
            for f in tiles.want(view.clone(), now) {
                tiles.land(f.tile, Ok(Decoded::default()), now);
            }
            assert!(tiles.slots.len() <= MAX_CACHED_TILES + MAX_IN_FLIGHT, "{} slots", tiles.slots.len());
        }
        // One more want evicts down to the cap; the latest view survives.
        let last: Vec<TileId> = (0..8).map(|i| tile(39 * 8 + i)).collect();
        tiles.want(last.clone(), now);
        assert!(tiles.slots.len() <= MAX_CACHED_TILES);
        assert!(last.iter().all(|t| tiles.slots.contains_key(t)));
        assert!(!tiles.slots.contains_key(&tile(0)), "the oldest went first");
        // A loading tile outlives eviction even when unwanted.
        let fresh: Vec<TileId> = (0..8).map(|i| tile(1_000 + i)).collect();
        assert_eq!(tiles.want(fresh.clone(), now).len(), 8);
        for v in 0..40u32 {
            let view: Vec<TileId> = (0..1).map(|i| tile(2_000 + v + i)).collect();
            let _ = tiles.want(view, now);
        }
        assert!(fresh.iter().all(|t| matches!(tiles.slots.get(t), Some(Slot::Loading))));
    }

    #[test]
    fn labels_are_the_clusters_counts_in_walkers_ink() {
        let records = [rec(50, 12, true), rec(12_400, 9, false)];
        let json: serde_json::Value = serde_json::from_str(&labels_geojson(&records, 0xFF, 0.7)).unwrap();
        let features = json["features"].as_array().unwrap();
        assert_eq!(features.len(), 1, "leaves carry no label");
        assert_eq!(features[0]["properties"]["text-field"], "12k");
        assert_eq!(features[0]["properties"]["text-size"], 12.0);
        assert_eq!(features[0]["properties"]["text-color"], "#263443b3");
        assert_eq!(label_color(1.0), "#263443ff");
        // Faded out, or every group hidden: no labels.
        assert!(labels_geojson(&records, 0xFF, 0.0).contains("\"features\":[]"));
        assert!(labels_geojson(&records, 0b1111_1100, 1.0).contains("\"features\":[]"));
    }
}

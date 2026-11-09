//! Hexagon mode's data (spec §4.2): the H3 hexagons of `race-hex-2020.pmtiles`
//! (or ClickHouse's `race_hex_2020`), one overlay source per resolution,
//! each loaded with the hex tiles in view and the ring around them while
//! the camera is inside that resolution's window ([`walker::HEX_WINDOWS`])
//! or within [`PREFETCH_ZOOM`] of it.
//!
//! - [`HexTiles`] is the sans-IO tile bookkeeping: what each resolution
//!   wants for this camera, what is in flight, what landed, and -- per
//!   resolution -- the records to draw (the wanted tiles that are ready).
//!   Each tile's records are drawn **unclipped**: a hexagon belongs to the
//!   tile holding its centre and may overhang it by up to one hexagon, so
//!   clipping would cut the edge hexagons (T10's carry). Hexagons under 10
//!   people never reach here -- the archive folded them into its per-tile
//!   hidden counts, which are never drawn.
//! - [`hex_paint`] is a resolution's paint: the modal group's palette
//!   colour, `#D9D9D9` on a tie and the `rgba(45,62,78,0.45)` outline. A
//!   group toggled off in the legend keeps its colour at alpha 0, so a
//!   toggle changes class colours, never class membership:
//!   `MapView::update_source_paint`'s table-only path.
//! - [`stack_opacities`] is each resolution's zoom cross-fade, applied per
//!   frame as `MapView::set_source_opacity` (a uniform that scales fill and
//!   outline alike), never as paint: see [`PaintKey`] for the blink a
//!   per-frame paint update caused. Composited so a cross-fade never dims
//!   the stack, the outgoing resolution held until the incoming one
//!   covers the view.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use gpui_maps::geo::TileId;
use gpui_maps::overlay::dotgrid::GridRequest;
use gpui_maps::overlay::hexes::{hex_tile_zoom, HexRecord, HexSource, Modal};
use gpui_maps::overlay::{FeatureId, FeatureInput, FeaturePaint, GeometryInput, PropertyColumn};
use gpui_maps::style::Rgba;

use super::walker;

/// Where the hexagons come from (C17).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HexSourceSpec {
    /// `race-hex-2020.pmtiles` (`tools/race-build export-hex`).
    Pmtiles(PathBuf),
    /// A ClickHouse HTTP endpoint holding `gpui_map_dots.race_hex_2020`
    /// (native, `--features "clickhouse-demo h3"`).
    ClickHouse(String),
}

impl HexSourceSpec {
    /// The archive `tools/race-build export-hex` writes by default.
    pub fn default_pmtiles() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_default();
        PathBuf::from(home).join(".cache/gpui-map-data/census2020/race-hex-2020.pmtiles")
    }
}

/// Opens `spec`'s hex source.
pub fn open_hexes(spec: &HexSourceSpec) -> Result<Arc<dyn HexSource>, String> {
    match spec {
        HexSourceSpec::Pmtiles(path) => {
            if !path.exists() {
                return Err(format!(
                    "no hex archive at {} -- build it with `cargo run --release --manifest-path tools/race-build/Cargo.toml -- export-hex`",
                    path.display()
                ));
            }
            let source = gpui_maps::overlay::dotgrid::fetch_native::open_pmtiles_hex(&path.to_string_lossy())
                .map_err(|e| format!("{}: {e}", path.display()))?;
            Ok(Arc::new(source))
        }
        #[cfg(all(feature = "clickhouse-demo", feature = "h3"))]
        HexSourceSpec::ClickHouse(url) => Ok(Arc::new(gpui_maps::overlay::hexes::source::ClickHouseHexSource::new(url))),
        #[cfg(not(all(feature = "clickhouse-demo", feature = "h3")))]
        HexSourceSpec::ClickHouse(url) => Err(format!("hexagons from ClickHouse ({url}) need --features \"clickhouse-demo h3\"")),
    }
}

/// The resolutions hexagon mode draws, coarsest first: the overlay sources
/// are created in this order so a finer resolution draws over a coarser one
/// through every cross-fade.
pub const RESOLUTIONS: [u8; 5] = [4, 5, 6, 7, 8];

/// Resolution `res`'s overlay source (`race-hex-r4` .. `race-hex-r8`).
pub fn source_name(res: u8) -> String {
    format!("race-hex-r{res}")
}

/// Resolution `res` from an overlay source's name.
pub fn res_of_source(name: &str) -> Option<u8> {
    name.strip_prefix("race-hex-r").and_then(|r| r.parse().ok()).filter(|r| RESOLUTIONS.contains(r))
}

/// Whether `(lon, lat)` is inside a hexagon's `boundary` (its ring, not
/// closed): an even-odd ray cast, after a box test.
pub fn boundary_contains(boundary: &[[f64; 2]], lon: f64, lat: f64) -> bool {
    if boundary.len() < 3 {
        return false;
    }
    let (mut w, mut s, mut e, mut n) = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
    for p in boundary {
        w = w.min(p[0]);
        e = e.max(p[0]);
        s = s.min(p[1]);
        n = n.max(p[1]);
    }
    if lon < w || lon > e || lat < s || lat > n {
        return false;
    }
    let mut inside = false;
    let mut j = boundary.len() - 1;
    for i in 0..boundary.len() {
        let (a, b) = (boundary[i], boundary[j]);
        if (a[1] > lat) != (b[1] > lat) && lon < (b[0] - a[0]) * (lat - a[1]) / (b[1] - a[1]) + a[0] {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// The hexagon under `(lon, lat)` among the drawn resolutions, as
/// `(res, index into its records)`: `layers` is `(res, opacity, records)`;
/// the most opaque resolution answers first (what the eye sees through a
/// cross-fade; a tie goes to the finer), and a hexagon whose largest group
/// the legend hides is not there to hover. The hover's own hit test: the
/// panel's records, so it answers the same in a test window as on screen.
pub fn hex_at(layers: &[(u8, f32, &[HexRecord])], lon: f64, lat: f64, mask: u8) -> Option<(u8, usize)> {
    let mut order: Vec<&(u8, f32, &[HexRecord])> = layers.iter().filter(|l| l.1 > 0.0).collect();
    order.sort_by(|a, b| b.1.total_cmp(&a.1).then(b.0.cmp(&a.0)));
    order.into_iter().find_map(|&(res, _, records)| {
        records.iter().position(|r| modal_shown(r.modal(), mask) && boundary_contains(&r.boundary, lon, lat)).map(|i| (res, i))
    })
}

/// The feature property the paint matches on.
pub const MODAL_KEY: &str = "modal";
const GROUP_KEYS: [&str; 8] = ["g0", "g1", "g2", "g3", "g4", "g5", "g6", "g7"];
const TIE_KEY: &str = "tie";

/// A record's [`MODAL_KEY`] value: its largest group (`g0`..`g7`, legend
/// order) or `tie`.
pub fn modal_key(rec: &HexRecord) -> &'static str {
    match rec.modal() {
        Modal::Group(g) => GROUP_KEYS.get(usize::from(g)).copied().unwrap_or("other"),
        Modal::Tie => TIE_KEY,
    }
}

/// `rgba(r,g,b,a)`, the colour `c` at alpha `a`.
pub fn css(c: Rgba, a: f32) -> String {
    let u = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u8;
    format!("rgba({},{},{},{})", u(c.r), u(c.g), u(c.b), a.clamp(0.0, 1.0))
}

/// Whether a record with modal `m` is drawn under the legend's `mask`
/// (bit `g` set: group `g` on). A tie is always drawn: the tie key is not a
/// toggle.
pub fn modal_shown(m: Modal, mask: u8) -> bool {
    match m {
        Modal::Group(g) => mask & (1 << g) != 0,
        Modal::Tie => true,
    }
}

/// A hexagon's fill colour (opaque): its modal group's palette colour, or
/// the tie grey.
pub fn fill_of(m: Modal) -> Rgba {
    match m {
        Modal::Group(g) if usize::from(g) < 8 => walker::color(usize::from(g)),
        Modal::Group(_) => walker::HEX_FALLBACK_COLOR,
        Modal::Tie => walker::HEX_TIE_COLOR,
    }
}

/// What a resolution's declared paint is built from: its `fill-opacity`
/// (per mille) and the legend's mask. When it moves for a source that
/// holds records, the source is repainted ([`SourceWrite::Repaint`]).
///
/// The `fill-opacity` is always 1: the zoom cross-fade is NOT paint (see
/// [`stack_opacities`]). A paint update to a source whose re-push is still
/// building its tile set takes `update_source_paint`'s Replan path, which
/// re-stamps the source and restarts that build; a cross-fade repainting
/// every frame of a zoom (or of a globe drag, whose zoom follows the
/// latitude) kept the pushed hexagons from ever landing, froze the old
/// picture at its old opacity, and blanked a resolution at its window's
/// end until the zoom stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaintKey {
    pub fill_opacity_milli: u16,
    pub mask: u8,
}

impl PaintKey {
    /// Resolution `res`'s paint at `zoom` under `mask`: the mask alone.
    pub fn at(_res: u8, _zoom: f64, mask: u8) -> Self {
        Self { fill_opacity_milli: 1000, mask }
    }

    /// The paint itself.
    pub fn paint(self) -> serde_json::Map<String, serde_json::Value> {
        hex_paint(f32::from(self.fill_opacity_milli) / 1000.0, self.mask)
    }
}

/// What a stack of translucent layers shows: `1 - prod(1 - a)`, the
/// combined alpha of `opacities` drawn over each other.
pub fn composite(opacities: &[f32]) -> f32 {
    1.0 - opacities.iter().fold(1.0, |rest, a| rest * (1.0 - a.clamp(0.0, 1.0)))
}

/// Each resolution's drawn opacity at `zoom` (per frame,
/// `MapView::set_source_opacity`: a uniform -- no re-bake, no re-stamp),
/// [`RESOLUTIONS`] order. `ready(res)` is how far the resolution draws
/// all the ground in view: 1 once it does ([`HexTiles::covered`]), 0
/// while it does not, eased between ([`ease_toward`]) so a resolution
/// finishing loading fades its partner over [`FADE`] rather than popping.
///
/// Walker's stops fade the two resolutions of a cross-fade linearly so
/// they SUM to 0.9. But they are drawn over each other, not added: at the
/// midpoint two layers at 0.45 show `1 - 0.55^2 = 0.70`, a dip from the
/// 0.9 plateau on either side -- every zoom through a cross-fade dimmed
/// the map and brightened it again. So, MapLibre's way, the finer
/// resolution (drawn on top: the sources are created coarsest first)
/// fades IN at its stops, and the coarser one under it is held at what
/// keeps their stack at the pair's sum `T`: `(T - a_hi) / (1 - a_hi)`,
/// which is `T` while the finer one is clear and 0 once it is at `T`.
/// And:
/// - while the finer one does not cover the view yet, the coarser one is
///   held whole (`T`), so loading never uncovers ground it drew;
/// - while the coarser one does not cover the view yet (a zoom out into
///   it), the finer one is held whole instead.
///
/// Outside a cross-fade a resolution draws at its own stops (r8's fade
/// into the dots above z12 included).
pub fn stack_opacities(zoom: f64, ready: impl Fn(u8) -> f32) -> [f32; 5] {
    let mut out = RESOLUTIONS.map(|r| walker::hex_opacity(r, zoom));
    for i in 0..RESOLUTIONS.len() - 1 {
        let (wlo, whi) = (walker::hex_opacity(RESOLUTIONS[i], zoom), walker::hex_opacity(RESOLUTIONS[i + 1], zoom));
        if wlo <= 0.0 || whi <= 0.0 {
            continue;
        }
        let total = (wlo + whi).min(1.0);
        let under = if whi >= 1.0 { 0.0 } else { ((total - whi) / (1.0 - whi)).clamp(0.0, 1.0) };
        let (rlo, rhi) = (ready(RESOLUTIONS[i]).clamp(0.0, 1.0), ready(RESOLUTIONS[i + 1]).clamp(0.0, 1.0));
        out[i] = total + (under - total) * rhi;
        out[i + 1] = total + (whi - total) * rlo;
    }
    out
}

/// How long a resolution's readiness ([`stack_opacities`]) takes to go
/// from 0 to 1: MapLibre's `fadeDuration`, 300 ms.
pub const FADE: std::time::Duration = std::time::Duration::from_millis(300);

/// `current` moved toward `target` by at most `dt / FADE` (the whole
/// 0..1 range takes [`FADE`]).
pub fn ease_toward(current: f32, target: f32, dt: std::time::Duration) -> f32 {
    let max = dt.as_secs_f32() / FADE.as_secs_f32();
    current + (target - current).clamp(-max, max)
}

/// A write to one resolution's overlay source this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceWrite {
    /// Its drawn tiles changed: re-push its records, the paint declared
    /// first (`declare_source_paint`, `set_source_columns`).
    Push(PaintKey),
    /// Only its paint moved: `MapView::update_source_paint`.
    Repaint(PaintKey),
}

/// This frame's source writes: a push for each resolution in `changed`
/// ([`HexTiles::take_changed`]), and a repaint for each other one whose
/// [`PaintKey`] at `zoom` and `mask` is not the one it was last painted
/// with (`painted`) while it holds records.
pub fn source_writes(changed: &[u8], painted: &[Option<PaintKey>; 5], zoom: f64, mask: u8, has_records: impl Fn(u8) -> bool) -> Vec<(u8, SourceWrite)> {
    let mut writes = Vec::new();
    for (i, &res) in RESOLUTIONS.iter().enumerate() {
        let key = PaintKey::at(res, zoom, mask);
        if changed.contains(&res) {
            writes.push((res, SourceWrite::Push(key)));
        } else if painted[i] != Some(key) && has_records(res) {
            writes.push((res, SourceWrite::Repaint(key)));
        }
    }
    writes
}

/// One resolution's paint at cross-fade `opacity` under the legend's
/// `mask`. See the module doc.
pub fn hex_paint(opacity: f32, mask: u8) -> serde_json::Map<String, serde_json::Value> {
    use serde_json::{json, Value};
    let mut fill = vec![json!("match"), json!(["get", MODAL_KEY])];
    let mut outline = fill.clone();
    for (g, key) in GROUP_KEYS.iter().enumerate() {
        let on = if mask & (1 << g) != 0 { 1.0 } else { 0.0 };
        fill.extend([json!(key), json!(css(walker::color(g), on))]);
        outline.extend([json!(key), json!(css(walker::HEX_OUTLINE, walker::HEX_OUTLINE.a * on))]);
    }
    fill.extend([json!(TIE_KEY), json!(css(walker::HEX_TIE_COLOR, 1.0)), json!(css(walker::HEX_FALLBACK_COLOR, 1.0))]);
    outline.extend([json!(TIE_KEY), json!(css(walker::HEX_OUTLINE, walker::HEX_OUTLINE.a)), json!(css(walker::HEX_OUTLINE, walker::HEX_OUTLINE.a))]);
    let mut paint = serde_json::Map::new();
    paint.insert("fill-color".into(), Value::Array(fill));
    paint.insert("fill-outline-color".into(), Value::Array(outline));
    paint.insert("fill-opacity".into(), json!(quantise_opacity(opacity)));
    paint
}

/// Opacity to 1/1000: the paint is re-sent only when this changes, and a
/// cross-fade over one zoom then takes at most 900 distinct steps.
pub fn quantise_opacity(opacity: f32) -> f64 {
    (f64::from(opacity.clamp(0.0, 1.0)) * 1000.0).round() / 1000.0
}

/// `records`' boundaries as closed rings (the first vertex repeated), one
/// per record, in record order.
pub fn closed_rings(records: &[HexRecord]) -> Vec<Vec<[f64; 2]>> {
    records
        .iter()
        .map(|r| {
            let mut ring = r.boundary.clone();
            if let Some(&first) = ring.first() {
                ring.push(first);
            }
            ring
        })
        .collect()
}

/// Pushes `records` to overlay source `name` through `push`, which gets
/// the features (feature `i` is `records[i]`, id `i`: `FeatureRef::index`
/// finds the record behind a click) and the [`MODAL_KEY`] column.
pub fn with_features<R>(records: &[HexRecord], push: impl FnOnce(&[FeatureInput<'_>], &[(&str, PropertyColumn<'_>)]) -> R) -> R {
    let rings = closed_rings(records);
    let ring_refs: Vec<&[[f64; 2]]> = rings.iter().map(Vec::as_slice).collect();
    let polygons: Vec<&[&[[f64; 2]]]> = ring_refs.iter().map(std::slice::from_ref).collect();
    let features: Vec<FeatureInput<'_>> = polygons
        .iter()
        .enumerate()
        .map(|(i, p)| FeatureInput {
            id: Some(FeatureId::Number(i as f64)),
            geometry: GeometryInput::Polygon(std::slice::from_ref(p)),
            paint: FeaturePaint::default(),
        })
        .collect();
    let keys: Vec<Option<&str>> = records.iter().map(|r| Some(modal_key(r))).collect();
    push(&features, &[(MODAL_KEY, PropertyColumn::Str(&keys))])
}

/// The headless capture's one overlay: every drawn resolution's hexagons
/// as a GeoJSON FeatureCollection, coarsest first, each carrying its own
/// resolved colours (`fc`, `oc`: the capture draws one source, so the
/// per-resolution opacity is baked into the colours), and the paint that
/// reads them.
pub fn capture_overlay(layers: &[(f32, &[HexRecord])], mask: u8) -> (String, String) {
    let mut features = Vec::new();
    for &(opacity, records) in layers {
        for r in records {
            let m = r.modal();
            let on = if modal_shown(m, mask) { opacity } else { 0.0 };
            let mut ring: Vec<[f64; 2]> = r.boundary.clone();
            if let Some(&first) = ring.first() {
                ring.push(first);
            }
            features.push(serde_json::json!({
                "type": "Feature",
                "geometry": {"type": "Polygon", "coordinates": [ring]},
                "properties": {"fc": css(fill_of(m), on), "oc": css(walker::HEX_OUTLINE, walker::HEX_OUTLINE.a * on)},
            }));
        }
    }
    let collection = serde_json::json!({"type": "FeatureCollection", "features": features});
    let paint = serde_json::json!({"fill-color": ["get", "fc"], "fill-outline-color": ["get", "oc"]});
    (collection.to_string(), paint.to_string())
}

// ---------------------------------------------------------------- tiles

/// Hex tiles fetched at once, at most.
pub const MAX_IN_FLIGHT: usize = 8;
/// Tiles one resolution loads for a view, at most (a pitched view can ask
/// for many more; the nearest are what the cover lists first).
pub const MAX_TILES_PER_RES: usize = 96;
/// Landed tiles (decoded or failed) kept, at most: the least recently
/// wanted beyond it go. A count, not bytes: the largest real tile is 4,105
/// records (~0.75 MB decoded), so ~190 MB at worst.
pub const MAX_CACHED_TILES: usize = 256;
/// A tile whose FETCH failed (a transient HTTP error from ClickHouse, say)
/// is asked for again after this; one that failed to DECODE never is.
pub const RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(5);

/// Why a tile has no records.
#[derive(Debug, Clone, PartialEq)]
pub struct TileError {
    /// Worth asking again ([`RETRY_AFTER`]): the fetch failed, not the bytes.
    pub retry: bool,
    pub why: String,
}

#[derive(Debug, Clone)]
enum Slot {
    Loading,
    Ready { records: Arc<Vec<HexRecord>>, used: u64 },
    Failed { used: u64, retry_at: Option<std::time::Instant> },
}

/// One resolution's wanted tiles and what it draws.
#[derive(Debug, Default)]
struct ResLayer {
    /// The view's tiles, then the ring around them.
    wanted: Vec<TileId>,
    /// How many of `wanted` are the view's own (the rest is the ring).
    in_view: usize,
    drawn: Vec<TileId>,
    records: Vec<HexRecord>,
}

/// How far outside its window (in zoom) a resolution is already loaded:
/// MapLibre loads a source's tiles for the zoom it is about to show only
/// once that zoom is reached, and a cross-fade starting before the next
/// resolution has landed was a fade of an empty layer -- so the
/// resolutions next to the current one are prefetched. Half a zoom is
/// about 0.5 s of a wheel zoom; a fetch from ClickHouse is 100-400 ms.
pub const PREFETCH_ZOOM: f64 = 0.5;

/// Whether resolution `res` loads at `zoom`: its window
/// ([`walker::HEX_WINDOWS`]) widened by [`PREFETCH_ZOOM`] each way.
pub fn wanted_at(res: u8, zoom: f64) -> bool {
    walker::HEX_WINDOWS
        .iter()
        .find(|w| w.0 == res)
        .is_some_and(|&(_, min, max, _)| (min - PREFETCH_ZOOM..=max + PREFETCH_ZOOM).contains(&zoom))
}

/// `view` (deduplicated, its order kept) followed by the ring of tiles
/// around it at the same zoom, nearest the view's first tile first, and
/// how many of the result are `view`'s. The ring is what a drag reveals
/// next: fetched before it is in view, it is drawn the frame it enters
/// instead of popping in a fetch later.
pub fn with_ring(view: Vec<TileId>) -> (Vec<TileId>, usize) {
    let mut seen = std::collections::HashSet::new();
    let mut tiles: Vec<TileId> = view.into_iter().filter(|t| seen.insert(*t)).collect();
    let in_view = tiles.len();
    let Some(&first) = tiles.first() else { return (tiles, 0) };
    let mut ring = Vec::new();
    for t in tiles.clone() {
        let n = 1i64 << t.z;
        for (dx, dy) in [(-1i64, -1i64), (0, -1), (1, -1), (-1, 0), (1, 0), (-1, 1), (0, 1), (1, 1)] {
            let y = i64::from(t.y) + dy;
            if !(0..n).contains(&y) {
                continue;
            }
            let x = (i64::from(t.x) + dx).rem_euclid(n);
            let tile = TileId { z: t.z, x: x as u32, y: y as u32 };
            if seen.insert(tile) {
                ring.push(tile);
            }
        }
    }
    let d = |t: &TileId| (i64::from(t.x) - i64::from(first.x)).abs().max((i64::from(t.y) - i64::from(first.y)).abs());
    ring.sort_by_key(d);
    tiles.extend(ring);
    (tiles, in_view)
}

/// One tile to fetch: `request` through the shell's fetcher, then
/// [`HexTiles::land`] with `source.decode(res, tile, bytes)`.
#[derive(Debug, Clone)]
pub struct Fetch {
    pub res: u8,
    pub tile: TileId,
    pub request: GridRequest,
}

/// The sans-IO tile bookkeeping of hexagon mode. See the module doc.
pub struct HexTiles {
    source: Arc<dyn HexSource>,
    slots: HashMap<(u8, TileId), Slot>,
    layers: [ResLayer; 5],
    in_flight: usize,
    clock: u64,
    /// Tiles that failed to fetch or decode, and the last reason.
    pub errors: u64,
    pub last_error: Option<String>,
}

fn slot_of(res: u8) -> Option<usize> {
    RESOLUTIONS.iter().position(|&r| r == res)
}

impl HexTiles {
    pub fn new(source: Arc<dyn HexSource>) -> Self {
        Self {
            source,
            slots: HashMap::new(),
            layers: Default::default(),
            in_flight: 0,
            clock: 0,
            errors: 0,
            last_error: None,
        }
    }

    pub fn source(&self) -> &Arc<dyn HexSource> {
        &self.source
    }

    /// Sets what every resolution wants at `zoom`: while `zoom` is within
    /// [`PREFETCH_ZOOM`] of its window ([`wanted_at`]), the tiles
    /// `cover(tz)` lists at its tile zoom, then the ring of tiles around
    /// them ([`with_ring`]); nothing otherwise. Returns the fetches to
    /// start now (including failed fetches whose retry time has come by
    /// `now`), the view's own tiles first.
    pub fn want(&mut self, zoom: f64, now: std::time::Instant, cover: &mut dyn FnMut(u8) -> Vec<TileId>) -> Vec<Fetch> {
        self.clock += 1;
        for (i, &res) in RESOLUTIONS.iter().enumerate() {
            let wanted = match hex_tile_zoom(res) {
                Some(tz) if wanted_at(res, zoom) => {
                    let tiles: Vec<TileId> = cover(tz).into_iter().filter(|t| t.z == tz).collect();
                    let (mut tiles, view) = with_ring(tiles);
                    tiles.truncate(MAX_TILES_PER_RES);
                    self.layers[i].in_view = view.min(tiles.len());
                    tiles
                }
                _ => {
                    self.layers[i].in_view = 0;
                    Vec::new()
                }
            };
            self.layers[i].wanted = wanted;
        }
        self.leave_all_but_wanted();
        // The view's tiles of the resolutions drawn now, then those of the
        // prefetched ones, then every ring: what is on screen first.
        let mut order: Vec<(u8, u8, TileId)> = Vec::new();
        for (i, &res) in RESOLUTIONS.iter().enumerate() {
            let shown = walker::hex_opacity(res, zoom) > 0.0;
            let layer = &self.layers[i];
            for (k, &tile) in layer.wanted.iter().enumerate() {
                let rank = match (k < layer.in_view, shown) {
                    (true, true) => 0,
                    (true, false) => 1,
                    (false, _) => 2,
                };
                order.push((rank, res, tile));
            }
        }
        order.sort_by_key(|&(rank, _, _)| rank);
        let mut fetches = Vec::new();
        for (_, res, tile) in order {
            let due = matches!(self.slots.get(&(res, tile)), Some(Slot::Failed { retry_at: Some(at), .. }) if *at <= now);
            match self.slots.get_mut(&(res, tile)) {
                Some(Slot::Ready { used, .. } | Slot::Failed { used, .. }) if !due => *used = self.clock,
                Some(Slot::Loading) | Some(_) if !due => {}
                _ if self.in_flight < MAX_IN_FLIGHT => {
                    self.slots.insert((res, tile), Slot::Loading);
                    self.in_flight += 1;
                    fetches.push(Fetch { res, tile, request: self.source.request(res, tile) });
                }
                _ => {}
            }
        }
        fetches
    }

    /// [`Self::want`] now.
    #[cfg(test)]
    pub fn want_now(&mut self, zoom: f64, cover: &mut dyn FnMut(u8) -> Vec<TileId>) -> Vec<Fetch> {
        self.want(zoom, std::time::Instant::now(), cover)
    }

    /// Nothing wanted (hexagon mode left): every resolution draws nothing
    /// at the next [`Self::take_changed`].
    pub fn clear_wanted(&mut self) {
        for layer in &mut self.layers {
            layer.wanted.clear();
        }
    }

    /// Evicts the least recently wanted ready tiles past
    /// [`MAX_CACHED_TILES`], never a wanted one.
    fn leave_all_but_wanted(&mut self) {
        let ready = self.slots.values().filter(|s| !matches!(s, Slot::Loading)).count();
        if ready <= MAX_CACHED_TILES {
            return;
        }
        let wanted: std::collections::HashSet<(u8, TileId)> =
            RESOLUTIONS.iter().zip(&self.layers).flat_map(|(&r, l)| l.wanted.iter().map(move |&t| (r, t))).collect();
        let mut old: Vec<((u8, TileId), u64)> = self
            .slots
            .iter()
            .filter_map(|(k, s)| match s {
                Slot::Ready { used, .. } | Slot::Failed { used, .. } if !wanted.contains(k) => Some((*k, *used)),
                _ => None,
            })
            .collect();
        old.sort_by_key(|&(_, used)| used);
        for (k, _) in old.into_iter().take(ready - MAX_CACHED_TILES) {
            self.slots.remove(&k);
        }
    }

    /// A fetch's outcome: the decoded records, or why not.
    pub fn land(&mut self, res: u8, tile: TileId, result: Result<Vec<HexRecord>, TileError>) {
        self.land_at(res, tile, result, std::time::Instant::now());
    }

    /// [`Self::land`] at `now` (the retry clock starts here).
    pub fn land_at(&mut self, res: u8, tile: TileId, result: Result<Vec<HexRecord>, TileError>, now: std::time::Instant) {
        self.in_flight = self.in_flight.saturating_sub(1);
        let slot = match result {
            Ok(records) => Slot::Ready { records: Arc::new(records), used: self.clock },
            Err(e) => {
                self.errors += 1;
                self.last_error = Some(e.why);
                Slot::Failed { used: self.clock, retry_at: e.retry.then(|| now + RETRY_AFTER) }
            }
        };
        self.slots.insert((res, tile), slot);
    }

    /// Whether a wanted tile's fetch is due for a retry at `now` (the
    /// caller then re-plans even with a still camera).
    pub fn retry_due(&self, now: std::time::Instant) -> bool {
        RESOLUTIONS.iter().zip(&self.layers).any(|(&r, l)| {
            l.wanted.iter().any(|t| matches!(self.slots.get(&(r, *t)), Some(Slot::Failed { retry_at: Some(at), .. }) if *at <= now))
        })
    }

    /// Resolution `res`'s push failed: it draws nothing it can answer a
    /// click for, and the next [`Self::take_changed`] rebuilds it.
    pub fn invalidate(&mut self, res: u8) {
        if let Some(i) = slot_of(res) {
            self.layers[i].drawn.clear();
            self.layers[i].records.clear();
        }
    }

    /// The resolutions whose drawn tiles changed since the last call --
    /// each now draws its wanted tiles that are ready, in cover order --
    /// with their records rebuilt.
    pub fn take_changed(&mut self) -> Vec<u8> {
        let mut changed = Vec::new();
        for (i, &res) in RESOLUTIONS.iter().enumerate() {
            let layer = &self.layers[i];
            let drawn: Vec<TileId> = layer
                .wanted
                .iter()
                .copied()
                .filter(|t| matches!(self.slots.get(&(res, *t)), Some(Slot::Ready { .. })))
                .collect();
            if drawn == layer.drawn {
                continue;
            }
            let mut records = Vec::new();
            for t in &drawn {
                if let Some(Slot::Ready { records: r, .. }) = self.slots.get(&(res, *t)) {
                    records.extend(r.iter().cloned());
                }
            }
            let layer = &mut self.layers[i];
            layer.drawn = drawn;
            layer.records = records;
            changed.push(res);
        }
        changed
    }

    /// Resolution `res`'s drawn records (feature `i` of its source is
    /// record `i`).
    pub fn records(&self, res: u8) -> &[HexRecord] {
        slot_of(res).map_or(&[], |i| &self.layers[i].records)
    }

    /// Resolution `res`'s drawn tiles (read by this file's tests).
    #[cfg(test)]
    pub fn drawn_tiles(&self, res: u8) -> &[TileId] {
        slot_of(res).map_or(&[], |i| &self.layers[i].drawn)
    }

    /// Whether resolution `res` draws all the ground in view: every one of
    /// the view's wanted tiles has landed (ready, or failed) and the ready
    /// ones are all drawn ([`Self::take_changed`] has pushed them). Not
    /// wanted at all: `false`.
    pub fn covered(&self, res: u8) -> bool {
        let Some(i) = slot_of(res) else { return false };
        let layer = &self.layers[i];
        let view = &layer.wanted[..layer.in_view.min(layer.wanted.len())];
        !view.is_empty()
            && view.iter().all(|t| match self.slots.get(&(res, *t)) {
                Some(Slot::Ready { .. }) => layer.drawn.contains(t),
                Some(Slot::Failed { .. }) => true,
                _ => false,
            })
    }

    /// Whether every wanted tile has landed (ready, or failed).
    pub fn settled(&self) -> bool {
        RESOLUTIONS
            .iter()
            .zip(&self.layers)
            .all(|(&r, l)| l.wanted.iter().all(|t| matches!(self.slots.get(&(r, *t)), Some(Slot::Ready { .. } | Slot::Failed { .. }))))
    }

    /// Fetches running now (read by this file's tests).
    #[cfg(test)]
    pub fn in_flight(&self) -> usize {
        self.in_flight
    }
}

/// Runs `fetch` through `execute` and decodes it: [`HexTiles::land`]'s
/// argument. A failed fetch is retryable; a failed decode is not.
pub fn run_fetch(
    source: &dyn HexSource,
    fetch: &Fetch,
    execute: impl Fn(&GridRequest) -> Result<Vec<u8>, gpui_maps::overlay::dotgrid::GridError>,
) -> Result<Vec<HexRecord>, TileError> {
    let at = format!("r{} {}/{}/{}", fetch.res, fetch.tile.z, fetch.tile.x, fetch.tile.y);
    let bytes = execute(&fetch.request).map_err(|e| TileError { retry: true, why: format!("{at}: {e}") })?;
    source.decode(fetch.res, fetch.tile, &bytes).map_err(|e| TileError { retry: false, why: format!("{at}: {e}") })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_maps::overlay::dotgrid::GridError;
    use std::sync::Mutex;

    /// Serves every tile with one record per tile whose modal group is
    /// `tile.x % 8`, and counts requests.
    struct Fake {
        asked: Mutex<Vec<(u8, TileId)>>,
    }

    fn rec(res: u8, counts: [u32; 8]) -> HexRecord {
        HexRecord { res, h3: 1, counts, boundary: vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.5, 1.5], [0.0, 1.0], [-0.5, 0.5]] }
    }

    #[test]
    fn the_hover_hit_test_finds_the_hexagon_under_the_point() {
        let ring = rec(6, [0; 8]).boundary;
        assert!(boundary_contains(&ring, 0.5, 0.5));
        assert!(boundary_contains(&ring, 0.5, 1.2), "the top point");
        assert!(!boundary_contains(&ring, 1.2, 0.5));
        assert!(!boundary_contains(&ring, 0.9, 1.4), "inside the box, outside the hexagon");
        assert!(!boundary_contains(&ring[..2], 0.5, 0.0));
        let white = rec(6, [0, 9, 0, 0, 0, 0, 0, 0]);
        let mut shifted = rec(7, [9, 0, 0, 0, 0, 0, 0, 0]);
        shifted.boundary.iter_mut().for_each(|p| p[0] += 0.25);
        let mut far = rec(6, [0; 8]);
        far.boundary.iter_mut().for_each(|p| p[0] += 10.0);
        let r6 = [white.clone(), far];
        let r7 = [shifted];
        // The more opaque resolution answers through a cross-fade.
        assert_eq!(hex_at(&[(6, 0.8, &r6), (7, 0.2, &r7)], 0.5, 0.5, 0xFF), Some((6, 0)));
        assert_eq!(hex_at(&[(6, 0.2, &r6), (7, 0.8, &r7)], 0.5, 0.5, 0xFF), Some((7, 0)));
        assert_eq!(hex_at(&[(6, 0.5, &r6), (7, 0.5, &r7)], 0.5, 0.5, 0xFF), Some((7, 0)), "a tie: the finer");
        assert_eq!(hex_at(&[(6, 1.0, &r6), (7, 0.0, &r7)], 1.1, 0.5, 0xFF), None, "r7 is not drawn");
        // White hidden: r6's hexagon is not there, r7's (Hispanic) is.
        assert_eq!(hex_at(&[(6, 1.0, &r6), (7, 0.5, &r7)], 0.5, 0.5, !(1u8 << 1)), Some((7, 0)));
        assert_eq!(hex_at(&[(6, 1.0, &r6)], 5.0, 5.0, 0xFF), None);
    }

    impl HexSource for Fake {
        fn request(&self, res: u8, tile: TileId) -> GridRequest {
            self.asked.lock().unwrap().push((res, tile));
            GridRequest::Range { location: format!("{res}"), offset: u64::from(tile.x), length: u64::from(tile.y) }
        }
        fn decode(&self, res: u8, tile: TileId, _: &[u8]) -> Result<Vec<HexRecord>, GridError> {
            let mut c = [0; 8];
            c[(tile.x % 8) as usize] = 10;
            Ok(vec![rec(res, c)])
        }
    }

    fn fake() -> (Arc<Fake>, HexTiles) {
        let f = Arc::new(Fake { asked: Mutex::new(Vec::new()) });
        (Arc::clone(&f), HexTiles::new(f))
    }

    fn row(z: u8, n: u32) -> Vec<TileId> {
        (0..n).map(|x| TileId { z, x, y: 0 }).collect()
    }

    fn land_all(tiles: &mut HexTiles, fetches: &[Fetch]) {
        let src = Arc::clone(tiles.source());
        for f in fetches {
            let r = run_fetch(src.as_ref(), f, |_| Ok(Vec::new()));
            tiles.land(f.res, f.tile, r);
        }
    }

    /// Wants `cover` at `zoom` and lands every fetch until nothing more is
    /// asked for: the view, its ring and the prefetched resolutions.
    fn settle(tiles: &mut HexTiles, zoom: f64, cover: &dyn Fn(u8) -> Vec<TileId>) {
        loop {
            let more = tiles.want_now(zoom, &mut |z| cover(z));
            if more.is_empty() {
                break;
            }
            land_all(tiles, &more);
        }
    }

    /// The tiles of `res`'s view in `cover` at its tile zoom.
    fn view(res: u8, cover: &dyn Fn(u8) -> Vec<TileId>) -> Vec<TileId> {
        cover(hex_tile_zoom(res).unwrap())
    }

    #[test]
    fn a_resolution_loads_only_inside_its_window_and_its_prefetch_margin() {
        let (_, mut tiles) = fake();
        let cover = |z: u8| row(z, 2);
        // z5: r4 (fading out) and r5 (fading in) -- the view's tiles first,
        // then their rings; r6's window starts at 6.5, its prefetch at 6.0.
        let fetches = tiles.want_now(5.0, &mut |z| cover(z));
        let res: Vec<u8> = fetches.iter().map(|f| f.res).collect();
        assert_eq!(&res[..4], &[4, 4, 5, 5], "{res:?}");
        assert!(res.iter().all(|r| matches!(r, 4 | 5)), "{res:?}");
        assert!(fetches.iter().all(|f| Some(f.tile.z) == hex_tile_zoom(f.res)));
        land_all(&mut tiles, &fetches);
        settle(&mut tiles, 5.0, &cover);
        assert_eq!(tiles.take_changed(), vec![4, 5]);
        assert!(view(4, &cover).iter().all(|t| tiles.drawn_tiles(4).contains(t)), "the view's tiles are drawn");
        assert!(tiles.records(4).len() > 2, "and the ring's");
        assert_eq!(tiles.records(6).len(), 0);
        assert!(tiles.covered(4) && tiles.covered(5) && !tiles.covered(6));
        assert!(tiles.take_changed().is_empty(), "nothing changed since");
        // z6.2: r4 is past its window and its margin and draws nothing
        // again; r5 is cached; r6 is prefetched before its fade starts.
        let fetches = tiles.want_now(6.2, &mut |z| cover(z));
        assert!(!fetches.is_empty() && fetches.iter().all(|f| f.res == 6), "{fetches:?}");
        assert_eq!(tiles.take_changed(), vec![4]);
        assert!(tiles.records(4).is_empty());
        assert!(!tiles.covered(4) && tiles.covered(5) && !tiles.covered(6));
    }

    #[test]
    fn records_are_drawn_whole_in_cover_order_and_in_flight_is_bounded() {
        let (_, mut tiles) = fake();
        let cover = |z: u8| row(z, 20);
        let fetches = tiles.want_now(11.0, &mut |z| cover(z));
        // z11: r7 (fading out) and r8 (fading in) -- 40 in view, 8 in flight,
        // the view's first.
        assert_eq!(fetches.len(), MAX_IN_FLIGHT);
        assert!(fetches.iter().all(|f| f.tile.y == 0 && f.tile.x < 20), "{fetches:?}");
        assert_eq!(tiles.in_flight(), MAX_IN_FLIGHT);
        land_all(&mut tiles, &fetches);
        assert_eq!(tiles.in_flight(), 0);
        assert!(!tiles.settled());
        settle(&mut tiles, 11.0, &cover);
        assert!(tiles.settled());
        tiles.take_changed();
        assert_eq!(&tiles.drawn_tiles(8)[..20], row(10, 20).as_slice(), "the view first, in cover order");
        // One record per tile, its boundary untouched (no clipping).
        assert_eq!(tiles.records(8).len(), tiles.drawn_tiles(8).len());
        assert!(tiles.records(8).iter().all(|r| r.boundary.len() == 6 && r.boundary[3] == [0.5, 1.5]));
    }

    #[test]
    fn a_tile_that_failed_to_decode_counts_and_is_not_drawn_or_retried() {
        let (f, mut tiles) = fake();
        let t0 = std::time::Instant::now();
        let fetches = tiles.want(3.5, t0, &mut |z| row(z, 1));
        let (bad, ring) = fetches.split_first().expect("the view's tile first");
        assert_eq!((bad.res, bad.tile), (4, TileId { z: 3, x: 0, y: 0 }));
        tiles.land_at(4, bad.tile, Err(TileError { retry: false, why: "boom".into() }), t0);
        land_all(&mut tiles, ring);
        assert_eq!((tiles.errors, tiles.last_error.as_deref()), (1, Some("boom")));
        let later = t0 + RETRY_AFTER * 10;
        assert!(!tiles.retry_due(later));
        assert!(tiles.want(3.5, later, &mut |z| row(z, 1)).is_empty());
        assert!(tiles.settled());
        tiles.take_changed();
        assert!(!tiles.drawn_tiles(4).contains(&bad.tile), "not drawn");
        assert!(tiles.covered(4), "a failed tile does not hold the cross-fade forever");
        let asked = f.asked.lock().unwrap().clone();
        assert_eq!(asked.iter().filter(|(_, t)| *t == bad.tile).count(), 1, "asked once");
    }

    #[test]
    fn a_tile_whose_fetch_failed_is_retried_after_the_backoff() {
        let (f, mut tiles) = fake();
        let t0 = std::time::Instant::now();
        let fetches = tiles.want(3.5, t0, &mut |z| row(z, 1));
        let (bad, ring) = fetches.split_first().expect("the view's tile first");
        tiles.land_at(4, bad.tile, Err(TileError { retry: true, why: "503".into() }), t0);
        land_all(&mut tiles, ring);
        assert!(!tiles.retry_due(t0 + RETRY_AFTER / 2));
        assert!(tiles.want(3.5, t0 + RETRY_AFTER / 2, &mut |z| row(z, 1)).is_empty(), "not before the backoff");
        assert!(tiles.retry_due(t0 + RETRY_AFTER));
        tiles.take_changed();
        let again = tiles.want(3.5, t0 + RETRY_AFTER, &mut |z| row(z, 1));
        assert_eq!(again.len(), 1);
        land_all(&mut tiles, &again);
        assert_eq!(tiles.take_changed(), vec![4]);
        assert!(tiles.drawn_tiles(4).contains(&bad.tile));
        let asked = f.asked.lock().unwrap().clone();
        assert_eq!(asked.iter().filter(|(_, t)| *t == bad.tile).count(), 2);
    }

    #[test]
    fn an_invalidated_resolution_draws_nothing_and_is_rebuilt() {
        let (_, mut tiles) = fake();
        settle(&mut tiles, 9.0, &|z| row(z, 1));
        tiles.take_changed();
        let n = tiles.records(7).len();
        assert!(n > 0);
        tiles.invalidate(7);
        assert!(tiles.records(7).is_empty());
        assert_eq!(tiles.take_changed(), vec![7]);
        assert_eq!(tiles.records(7).len(), n);
    }

    #[test]
    fn leaving_hexagon_mode_clears_every_resolution() {
        let (_, mut tiles) = fake();
        settle(&mut tiles, 9.0, &|z| row(z, 1));
        // z9: r6 (fading out) and r7 (fading in); r8 prefetched (10.0).
        assert_eq!(tiles.take_changed(), vec![6, 7]);
        tiles.clear_wanted();
        assert_eq!(tiles.take_changed(), vec![6, 7]);
        assert!(RESOLUTIONS.iter().all(|&r| tiles.records(r).is_empty()));
    }

    #[test]
    fn the_cache_evicts_old_unwanted_tiles_only() {
        let (f, mut tiles) = fake();
        for step in 0..40u32 {
            // A pan: r8 only (z12.2, past r7's prefetch margin), 10 new tiles a step.
            let cover = move |z: u8| (0..10).map(|i| TileId { z, x: step * 10 + i, y: 0 }).collect::<Vec<_>>();
            settle(&mut tiles, 12.2, &cover);
            tiles.take_changed();
            assert!(cover(10).iter().all(|t| tiles.drawn_tiles(8).contains(t)));
        }
        let ready = tiles.slots.values().filter(|s| matches!(s, Slot::Ready { .. })).count();
        assert!(ready <= MAX_CACHED_TILES + MAX_TILES_PER_RES, "{ready}");
        // Review L6: no tile was ever asked for twice -- a wanted tile
        // evicted mid-pan would have been re-requested.
        let asked = f.asked.lock().unwrap().clone();
        let unique: std::collections::HashSet<_> = asked.iter().collect();
        assert_eq!(unique.len(), asked.len(), "a tile was fetched twice");
        // And a pan back to the start re-fetches the evicted old tiles.
        let back = |z: u8| (0..10).map(|i| TileId { z, x: i, y: 0 }).collect::<Vec<_>>();
        let before = asked.len();
        settle(&mut tiles, 12.2, &back);
        assert!(f.asked.lock().unwrap().len() > before, "the evicted start is fetched again");
        assert_eq!(tiles.take_changed(), vec![8]);
        assert!(back(10).iter().all(|t| tiles.drawn_tiles(8).contains(t)));
    }

    #[test]
    fn modal_keys_and_colours() {
        assert_eq!(modal_key(&rec(8, [1, 9, 0, 0, 0, 0, 0, 0])), "g1");
        assert_eq!(modal_key(&rec(8, [9, 9, 0, 0, 0, 0, 0, 0])), "tie");
        assert_eq!(fill_of(Modal::Tie), walker::HEX_TIE_COLOR);
        assert_eq!(fill_of(Modal::Group(2)), walker::color(2));
        assert_eq!(css(walker::HEX_OUTLINE, 0.45), "rgba(45,62,78,0.45)");
        assert!(modal_shown(Modal::Tie, 0));
        assert!(!modal_shown(Modal::Group(3), 0b1111_0111));
        assert_eq!(res_of_source(&source_name(7)), Some(7));
        assert_eq!(res_of_source("race-hex-r9"), None);
        assert_eq!(res_of_source("race"), None);
    }

    #[test]
    fn the_paint_hides_toggled_groups_at_alpha_zero_and_carries_the_opacity() {
        let p = hex_paint(0.4567, 0b1111_1101);
        let fill = p["fill-color"].as_array().unwrap();
        let find = |arr: &Vec<serde_json::Value>, key: &str| {
            let i = arr.iter().position(|v| v == key).unwrap();
            arr[i + 1].as_str().unwrap().to_string()
        };
        assert_eq!(find(fill, "g1"), css(walker::color(1), 0.0));
        assert_eq!(find(fill, "g0"), css(walker::color(0), 1.0));
        assert_eq!(find(fill, "tie"), "rgba(217,217,217,1)");
        assert_eq!(fill.last().unwrap(), "rgba(153,153,153,1)");
        let outline = p["fill-outline-color"].as_array().unwrap();
        assert_eq!(find(outline, "g0"), "rgba(45,62,78,0.45)");
        assert_eq!(find(outline, "g1"), "rgba(45,62,78,0)");
        assert_eq!(p["fill-opacity"], 0.457);
    }

    /// The paint compiles in the engine and the features push, so the
    /// window's per-resolution sources accept what this module builds.
    #[test]
    fn the_engine_accepts_the_paint_and_the_features() {
        let mut store = gpui_maps::overlay::OverlayStore::new();
        let name = source_name(8);
        store.declare_source_paint(&name, &hex_paint(0.9, 0xFF)).expect("the hex paint compiles");
        let records = vec![rec(8, [10, 0, 0, 0, 0, 0, 0, 0]), rec(8, [5, 5, 0, 0, 0, 0, 0, 0])];
        with_features(&records, |features, columns| store.set_source_columns(&name, features, columns, 11))
            .expect("hexagons push");
        store.declare_source_paint(&name, &hex_paint(0.0, 0)).expect("zero opacity and all groups off compile");
        let (geojson, paint) = capture_overlay(&[(0.45, &records)], 0xFF);
        assert!(geojson.contains("rgba(230,159,0,0.45)") && geojson.contains("rgba(217,217,217,0.45)"), "{geojson}");
        let paint: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&paint).unwrap();
        store.declare_source_paint("capture", &paint).expect("the capture paint compiles");
        store.set_source("capture", &geojson, 11).expect("the capture overlay parses");
    }

    /// The blink sweep: the panel's own per-frame decisions
    /// ([`HexTiles::want`] with its prefetch, [`HexTiles::take_changed`]
    /// coalesced as `drive_hexes` does, [`stack_opacities`] over readiness
    /// eased by [`ease_toward`]) over a scripted camera, every tile's hexagons
    /// covering it, each fetch landing after a seeded 100-400 ms (a hex
    /// tile from ClickHouse), against `MapView`'s rule for a tiled source:
    /// a push installs `BUILD` frames later, the old picture drawn until
    /// then (`compose_cover_quadrants`).
    ///
    /// Measured on a grid of points over the middle of the view, each
    /// point's drawn alpha the composite of the resolutions whose
    /// installed tiles hold it: every frame, at least 95% of the points
    /// must show at least 95% of the stack's plateau (Walker's opacities
    /// summed: 0.9, or r8's own fade into the dots). Less is ground gone
    /// missing, or the whole stack dimmed -- the two halves of the blink.
    mod sweep {
        use super::*;
        use gpui_maps::render::Camera;
        use std::collections::HashSet;

        const FRAME_MS: u64 = 16;
        const BUILD: u64 = 3;

        struct Every;
        impl HexSource for Every {
            fn request(&self, res: u8, tile: TileId) -> GridRequest {
                GridRequest::Range { location: format!("{res}"), offset: u64::from(tile.x), length: u64::from(tile.y) }
            }
            fn decode(&self, res: u8, _: TileId, _: &[u8]) -> Result<Vec<HexRecord>, GridError> {
                Ok(vec![rec(res, [10, 0, 0, 0, 0, 0, 0, 0])])
            }
        }

        fn cover(camera: &Camera, tz: u8) -> Vec<TileId> {
            let frame = gpui_maps::render::globe::globe_frame(camera).with_globe_enabled(true);
            let globe = gpui_maps::render::globe::GlobeCover::new(&frame);
            gpui_maps::render::cover_plan::tile_cover(tz, &globe, || gpui_maps::render::cover_plan::flat_cover(camera, tz).tiles)
                .into_iter()
                .filter(|t| t.z == tz)
                .collect()
        }

        fn cam(lon: f64, lat: f64, zoom: f64) -> Camera {
            Camera { center_lon: lon, center_lat: lat, zoom, pitch: 0.0, bearing: 0.0, viewport_width_px: 900.0, viewport_height_px: 700.0, fov_y: Camera::DEFAULT_FOV_Y }
        }

        /// The ground points sampled: a 16 x 12 grid over the middle 60%
        /// of the view, as world fractions.
        fn points(c: &Camera) -> Vec<(f64, f64)> {
            let world = f64::from(gpui_maps::render::WORLD_SIZE);
            let mut out = Vec::new();
            for j in 0..12 {
                for i in 0..16 {
                    let x = c.viewport_width_px * (0.2 + 0.6 * i as f32 / 15.0);
                    let y = c.viewport_height_px * (0.2 + 0.6 * j as f32 / 11.0);
                    if let Some(p) = gpui_maps::render::unproject_to_ground_f64(c, x, y) {
                        out.push((p.x / world, p.y / world));
                    }
                }
            }
            out
        }

        fn tile_at(tz: u8, (x, y): (f64, f64)) -> TileId {
            let n = f64::from(1u32 << tz);
            let clamp = |v: f64| (v * n).floor().clamp(0.0, n - 1.0) as u32;
            TileId { z: tz, x: clamp(x.rem_euclid(1.0)), y: clamp(y) }
        }

        /// A seeded 100-400 ms per fetch.
        struct Latency(u64);
        impl Latency {
            fn next_ms(&mut self) -> u64 {
                self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                100 + (self.0 >> 33) % 301
            }
        }

        /// Runs `cams` (parked on the first until settled) and returns the
        /// worst frame: `(frame, zoom, share of points at >= 95% of the
        /// plateau, the mean drawn alpha over the plateau)`.
        fn worst(cams: &[Camera], seed: u64) -> (usize, f64, f64, f64) {
            let mut script = vec![cams[0]; 150];
            script.extend_from_slice(cams);
            let mut tiles = HexTiles::new(Arc::new(Every));
            let mut latency = Latency(seed);
            let t0 = std::time::Instant::now();
            let mut pending: Vec<(u64, Fetch)> = Vec::new();
            // Per resolution: installed tiles, and a build in flight.
            let mut installed: [HashSet<TileId>; 5] = Default::default();
            let mut building: [Option<(HashSet<TileId>, u64)>; 5] = Default::default();
            let mut ready = [0.0f32; 5];
            let mut pushed_at: Option<u64> = None;
            let mut last: Option<Camera> = None;
            let mut worst = (0usize, 0.0f64, f64::INFINITY, f64::INFINITY);
            for (f, c) in script.iter().enumerate() {
                let frame = f as u64;
                let now = t0 + std::time::Duration::from_millis(FRAME_MS * frame);
                let (due, rest): (Vec<_>, Vec<_>) = pending.into_iter().partition(|(at, _)| *at <= frame);
                pending = rest;
                let landed = !due.is_empty();
                for (_, fetch) in due {
                    let r = run_fetch(tiles.source().as_ref(), &fetch, |_| Ok(Vec::new()));
                    tiles.land_at(fetch.res, fetch.tile, r, now);
                }
                if last != Some(*c) || landed {
                    for fetch in tiles.want(c.zoom, now, &mut |tz| cover(c, tz)) {
                        let at = frame + latency.next_ms().div_ceil(FRAME_MS);
                        pending.push((at, fetch));
                    }
                }
                last = Some(*c);
                let push_now = tiles.settled() || pushed_at.is_none_or(|p| (frame - p) * FRAME_MS >= 250);
                let changed = if push_now { tiles.take_changed() } else { Vec::new() };
                if !changed.is_empty() {
                    pushed_at = Some(frame);
                }
                for res in changed {
                    let i = slot_of(res).unwrap();
                    let drawn: HashSet<TileId> = tiles.drawn_tiles(res).iter().copied().collect();
                    if drawn.is_empty() {
                        (installed[i], building[i]) = (drawn, None);
                    } else {
                        building[i] = Some((drawn, frame));
                    }
                }
                for i in 0..5 {
                    if building[i].as_ref().is_some_and(|b| frame - b.1 >= BUILD) {
                        installed[i] = building[i].take().unwrap().0;
                    }
                }
                for (i, &res) in RESOLUTIONS.iter().enumerate() {
                    let target = if tiles.covered(res) { 1.0 } else { 0.0 };
                    ready[i] = ease_toward(ready[i], target, std::time::Duration::from_millis(FRAME_MS));
                }
                let drawn_opacity = stack_opacities(c.zoom, |r| ready[slot_of(r).unwrap()]);
                if f < 150 {
                    continue;
                }
                // What the stack shows loaded: Walker's stops summed.
                let plateau = f64::from(RESOLUTIONS.iter().map(|&r| walker::hex_opacity(r, c.zoom)).sum::<f32>().min(1.0));
                if plateau < 0.05 {
                    continue;
                }
                let pts = points(c);
                let alphas: Vec<f64> = pts
                    .iter()
                    .map(|&p| {
                        let ops: Vec<f32> = RESOLUTIONS
                            .iter()
                            .enumerate()
                            .filter(|&(i, &res)| installed[i].contains(&tile_at(hex_tile_zoom(res).unwrap(), p)))
                            .map(|(i, _)| drawn_opacity[i])
                            .collect();
                        f64::from(composite(&ops))
                    })
                    .collect();
                let shown = alphas.iter().filter(|&&a| a >= 0.95 * plateau).count() as f64 / alphas.len() as f64;
                let mean = alphas.iter().sum::<f64>() / alphas.len() as f64 / plateau;
                if shown < worst.2 || (shown == worst.2 && mean < worst.3) {
                    worst = (f, c.zoom, shown, mean);
                }
            }
            worst
        }

        fn assert_no_blink(what: &str, cams: &[Camera]) {
            for seed in [1u64, 2, 3] {
                let (f, z, shown, mean) = worst(cams, seed);
                assert!(
                    shown >= 0.95,
                    "{what} (latency seed {seed}): frame {f} at z{z:.3} shows {shown:.3} of the ground at its plateau (mean {mean:.3})"
                );
            }
        }

        const AT: (f64, f64) = (-95.37, 29.76);

        fn ease(t: f64) -> f64 {
            t * t * (3.0 - 2.0 * t)
        }

        /// z3 -> z12 -> z3 over 8 s each way, through every cross-fade.
        #[test]
        fn a_zoom_sweep_never_blinks() {
            let leg = |a: f64, b: f64, n: usize| (0..=n).map(move |i| cam(AT.0, AT.1, a + (b - a) * ease(i as f64 / n as f64)));
            let cams: Vec<Camera> = leg(3.0, 12.0, 480).chain(leg(12.0, 12.0, 60)).chain(leg(12.0, 3.0, 480)).collect();
            assert_no_blink("zoom sweep", &cams);
        }

        /// A globe drag at z4.8 (inside the r4/r5 cross-fade) and z7: the
        /// centre moves 40 degrees east and 15 north and back, the zoom
        /// moving with the latitude as MapLibre's globe drag moves it.
        #[test]
        fn a_globe_rotation_never_blinks() {
            for z0 in [4.8, 7.0] {
                let adj = |lat: f64| (lat.to_radians().cos() / AT.1.to_radians().cos()).log2();
                let cams: Vec<Camera> = (0..=300)
                    .map(|i| {
                        let t = i as f64 / 300.0;
                        let lat = AT.1 + 15.0 * (t * std::f64::consts::TAU).sin();
                        cam(AT.0 + 40.0 * t, lat, z0 + adj(lat))
                    })
                    .collect();
                assert_no_blink(&format!("globe drag at z{z0}"), &cams);
            }
        }

        /// A flat pan at z8 (r6 alone) and z10.8 (r7 -> r8): two screen
        /// widths east.
        #[test]
        fn a_pan_never_blinks() {
            for z in [8.0, 10.8] {
                let span = 900.0 / 256.0 / 2f64.powf(z) * 360.0 * 2.0;
                let cams: Vec<Camera> = (0..=240).map(|i| cam(AT.0 + span * i as f64 / 240.0, AT.1, z)).collect();
                assert_no_blink(&format!("pan at z{z}"), &cams);
            }
        }
    }

    /// The stack's combined alpha, every resolution loaded, follows
    /// Walker's summed stops at every zoom: no dip mid-fade (two 0.45
    /// layers composite to 0.70, not 0.9), no overshoot.
    #[test]
    fn the_composited_stack_never_dips_through_a_cross_fade() {
        let mut z = 3.0;
        while z <= 12.75 {
            let walker_sum: f32 = RESOLUTIONS.iter().map(|&r| walker::hex_opacity(r, z)).sum();
            let ops = stack_opacities(z, |_| 1.0);
            let shown = composite(&ops);
            assert!((shown - walker_sum.min(1.0)).abs() < 1e-3, "z{z:.2}: shows {shown:.3}, Walker's stops sum to {walker_sum:.3}");
            z += 0.01;
        }
        let naive = composite(&RESOLUTIONS.map(|r| walker::hex_opacity(r, 5.0)));
        assert!(naive < 0.75, "the dip this replaces: {naive}");
        // The incoming finer level not landed: the coarser is held whole.
        let ready = |not: u8| move |r: u8| if r == not { 0.0 } else { 1.0 };
        let held = stack_opacities(5.0, ready(5));
        assert_eq!((held[0], held[1]), (0.9, 0.45));
        // The coarser one not landed (a zoom out into it): the finer is.
        assert_eq!(stack_opacities(5.0, ready(4))[1], 0.9);
        // Readiness eases over FADE, so the hand-over fades.
        assert!((ease_toward(0.0, 1.0, FADE / 2) - 0.5).abs() < 1e-6);
        assert_eq!(ease_toward(0.5, 0.0, FADE * 2), 0.0);
    }

    #[test]
    fn the_ring_follows_the_view_and_wraps_the_antimeridian() {
        let (tiles, view) = with_ring(vec![TileId { z: 3, x: 0, y: 3 }, TileId { z: 3, x: 0, y: 3 }]);
        assert_eq!(view, 1, "deduplicated");
        assert_eq!(tiles.len(), 9);
        assert!(tiles.contains(&TileId { z: 3, x: 7, y: 2 }), "x wraps: {tiles:?}");
        let (tiles, _) = with_ring(vec![TileId { z: 2, x: 1, y: 0 }]);
        assert_eq!(tiles.len(), 6, "no row above the top");
    }

    #[test]
    fn rings_are_closed() {
        let rings = closed_rings(&[rec(5, [10, 0, 0, 0, 0, 0, 0, 0])]);
        assert_eq!(rings[0].len(), 7);
        assert_eq!(rings[0][0], rings[0][6]);
    }
}

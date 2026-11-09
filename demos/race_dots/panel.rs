//! The race map as one gpui entity, [`RaceMapPanel`]: the map with the dot
//! layer and (hexagon mode) the H3 hexagons, the legend (top left), the view
//! switcher and status line (top right), the About panel (bottom right) and
//! the hexagon popup; in cluster mode the donuts, their count labels and
//! the click that zooms into one. It takes its data from a
//! [`RaceSources`] and its host from a [`PanelHost`], not from CLI flags:
//! the gallery embeds it as the `race-map` panel
//! (`kit/src/panels/race_dots.rs`, which includes this file for both
//! crates), and `demos/race_dots.rs` is only its window and its
//! measurement flags. Every pointer position it uses is panel-relative
//! (the map's own pixels), whatever the panel's place in the window.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    div, px, App, AppContext, Context, Entity, FontWeight, InteractiveElement, IntoElement, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, Window,
};
use gpui_maps::map_view::{MapView, SourcePaintPath};
use gpui_maps::overlay::dotgrid::layer::DotStats;
use gpui_maps::overlay::hexes::{HexRecord, Modal};
use gpui_maps::style::Rgba;
use gpui_maps::overlay::dotgrid::{fetch_native, GridCountSource, GridTileKey};
use gpui_maps::tile_source::http::HttpTileSource;
use gpui_maps::tile_source::TileSource;

use super::clusters::{self, ClusterTiles, DONUT_LAYER, LABEL_SOURCE};
use super::hexes::{self, HexSourceSpec, HexTiles, RESOLUTIONS};
use super::states::{self, SortKey};
use super::{about, walker};
use gpui_maps::overlay::clusters_tile::ClusterRecord;

/// The dot layer's id on the map.
pub const LAYER: &str = "race";

/// Where the race map's data comes from (C17): the dots, the hexagons and
/// the clusters.
#[derive(Debug, Clone)]
pub struct RaceSources {
    /// The race-dots PMTiles archive (`tools/race-build`).
    pub dots: PathBuf,
    /// The hexagons (`race-build export-hex`'s archive, or ClickHouse).
    /// `None`: hexagon mode is not offered.
    pub hexes: Option<HexSourceSpec>,
    /// The cluster archive (`race-build export-clusters`). `None`: cluster
    /// mode is not offered.
    pub clusters: Option<PathBuf>,
}

impl RaceSources {
    /// The archive `tools/race-build` writes by default.
    pub fn default_dots() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_default();
        PathBuf::from(home).join(".cache/gpui-map-data/census2020/race-dots-2020.pmtiles")
    }
}

/// The view switcher's choices (Walker's radio group).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Dots,
    Hexagons,
    Clusters,
    /// A pie per state over the state outlines, and the states chart.
    States,
}

impl Mode {
    pub const ALL: [Mode; 4] = [Mode::Dots, Mode::Hexagons, Mode::Clusters, Mode::States];

    pub fn label(self) -> &'static str {
        match self {
            Mode::Dots => "Dots",
            Mode::Hexagons => "Hexagons",
            Mode::Clusters => "Clusters",
            Mode::States => "States",
        }
    }

    /// Whether this build can draw the mode: all four (T14 added
    /// clusters). A mode still needs its source: `mode_available`.
    pub fn enabled(self) -> bool {
        matches!(self, Mode::Dots | Mode::Hexagons | Mode::Clusters | Mode::States)
    }
}

/// Walker's hook: whether the dots (their legend and status text) are the
/// active layer -- always in dot mode, from `legendSwitch` z12.375 in
/// hexagon mode, and from z12 in cluster mode.
pub fn dots_active(mode: Mode, zoom: f64) -> bool {
    match mode {
        Mode::Dots => true,
        Mode::Hexagons => zoom >= walker::LEGEND_SWITCH_HEX,
        Mode::Clusters => zoom >= walker::CLUSTER_DOTS_FROM,
        Mode::States => false,
    }
}

const ONE_DOT: &str = "One dot per person";
const ZOOM_IN: &str = "Zoom in to see every dot";
/// Dot density has no per-object identity: nothing per dot to hover.
pub const DOTS_AGGREGATED: &str = "aggregated; no per-dot details";

/// Walker's status line at `zoom` in `mode`.
pub fn status_text(mode: Mode, zoom: f64) -> &'static str {
    if dots_active(mode, zoom) {
        return if zoom >= walker::ONE_DOT_PER_PERSON_ZOOM { ONE_DOT } else { ZOOM_IN };
    }
    match mode {
        Mode::Hexagons => "Hex colours show the largest group, not population density. Minimum: 10 people.",
        Mode::Clusters => "Click a donut to zoom in",
        Mode::States => "Pie area: population. Click a state for its groups",
        Mode::Dots => unreachable!("dots are always active in dot mode"),
    }
}

/// The ppd at and under which each dot is one person: 1, to float noise.
/// The controller lands exactly on 1 (`PpdController`'s floor), so this is
/// "the ppd really is 1", not a rounding of 1.004 (review F1 ruling).
pub const ONE_PPD: f32 = 1.0001;

/// The status line the panel shows: [`status_text`], except that while the
/// dots are active the layer's real ppd decides the dots' text -- "One dot
/// per person" once it reads 1.00, whatever the zoom, and never before --
/// so the line cannot claim one dot per person while each dot still stands
/// for several (Walker's z13 rule is his tiles'; ours is the ppd
/// controller's, which reaches 1 by view population and the 2x drawn-dots
/// bound, not by zoom: over Manhattan that is past z13). `ppd` is `None`
/// until dots are drawn: "Zoom in to see every dot" until they say
/// otherwise -- never a claim from a fixed zoom.
pub fn status_line(mode: Mode, zoom: f64, ppd: Option<f32>) -> &'static str {
    if !dots_active(mode, zoom) {
        return status_text(mode, zoom);
    }
    match ppd {
        Some(ppd) if ppd <= ONE_PPD => ONE_DOT,
        _ => ZOOM_IN,
    }
}

/// The legend's toggles: a group is drawn while it is on. All on by default.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Legend {
    /// Bit `g` set: group `g` is toggled off.
    off: u8,
}

impl Legend {
    /// Flips group `g` (0..8).
    pub fn toggle(&mut self, g: usize) {
        assert!(g < 8, "8 groups, got {g}");
        self.off ^= 1 << g;
    }

    pub fn is_on(&self, g: usize) -> bool {
        self.off & (1 << g) == 0
    }

    /// Every group on again.
    pub fn show_all(&mut self) {
        self.off = 0;
    }

    /// `DotStyle::category_mask`: bit `g` set while group `g` is drawn.
    pub fn mask(&self) -> u8 {
        !self.off
    }
}

/// "1,234,567".
pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// A head count: exact below a million, then "1.29 M".
pub fn people_text(people: f64) -> String {
    if people >= 1e6 {
        format!("{:.2} M", people / 1e6)
    } else {
        thousands(people.round().max(0.0) as u64)
    }
}

/// What the layer draws from: a pyramid level, or the block parts (T7's
/// part tiles, `GridTileKey::PART_LEVEL`), with the regions the part cap
/// sent back to cells.
pub fn level_text(stats: &DotStats) -> String {
    if stats.level == GridTileKey::PART_LEVEL {
        if stats.part_fallback_tiles > 0 {
            format!("parts, {} regions as cells (part cap)", stats.part_fallback_tiles)
        } else {
            "parts".to_string()
        }
    } else {
        format!("level {}", stats.level)
    }
}

/// The HUD line: people and dots in view, the ppd, and the level.
pub fn hud_text(stats: Option<&DotStats>, note: Option<&str>) -> String {
    if let Some(why) = note {
        return format!("no dots: {why}");
    }
    match stats {
        Some(s) if s.dots_drawn > 0 => format!(
            "{} people in view · {} dots · 1 dot = {:.2} people · {}",
            people_text(s.people_in_view),
            thousands(s.dots_in_view),
            s.ppd.max(1.0),
            level_text(s)
        ),
        Some(s) if s.last_error.is_some() => format!("no dots: {}", s.last_error.unwrap_or("error")),
        _ => "loading dots…".to_string(),
    }
}

// ---------------------------------------------------------------- popup

/// A hexagon's popup (Walker's): the total, whether its largest group is
/// shared, and each group's share in legend order, with its colour.
#[derive(Debug, Clone, PartialEq)]
pub struct HexPopup {
    pub total: u32,
    pub tie: bool,
    pub rows: [(&'static str, f32 /* percent */, Rgba); 8],
}

impl HexPopup {
    /// The popup of a hexagon with these 8 group counts.
    pub fn of(counts: &[u32; 8]) -> Self {
        let total = counts.iter().fold(0u32, |a, &c| a.saturating_add(c));
        // The one tie rule (spec §4.1), `HexRecord::modal`'s -- not a copy.
        let tie = modal_of(counts) == Modal::Tie;
        let rows = std::array::from_fn(|g| {
            let pct = if total == 0 { 0.0 } else { (100.0 * f64::from(counts[g]) / f64::from(total)) as f32 };
            (walker::GROUPS[g].label, pct, walker::color(g))
        });
        Self { total, tie, rows }
    }

    pub fn of_record(rec: &HexRecord) -> Self {
        Self::of(&rec.counts)
    }

    /// "N people in this hexagon".
    pub fn title(&self) -> String {
        format!("{} people in this hexagon", thousands(u64::from(self.total)))
    }

    /// The hexagon's largest group, or a tie (from its shares, which order
    /// the groups as its counts do).
    pub fn modal(&self) -> Modal {
        if self.tie {
            return Modal::Tie;
        }
        let g = (0..8).max_by(|&a, &b| self.rows[a].1.total_cmp(&self.rows[b].1)).unwrap_or(0);
        Modal::Group(g as u8)
    }
}

/// `HexRecord::modal` of 8 counts.
fn modal_of(counts: &[u32; 8]) -> Modal {
    HexRecord { res: 0, h3: 0, counts: *counts, boundary: Vec::new() }.modal()
}

/// The popup's state: open on a hexagon, anchored at the clicked ground
/// point `(lon, lat)` (it follows the map), or closed. Walker's closes on
/// zoom start, on Escape and on a mode change; ours also closes when the
/// legend hides the hexagon's group.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PopupState {
    open: Option<(HexPopup, (f64, f64))>,
}

impl PopupState {
    /// Open at the origin; tests only (the panel opens at the click).
    #[cfg(test)]
    pub fn open(popup: HexPopup) -> Self {
        Self::open_at(popup, (0.0, 0.0))
    }

    /// Open, anchored at `lon_lat`, the ground point the click landed on.
    pub fn open_at(popup: HexPopup, lon_lat: (f64, f64)) -> Self {
        Self { open: Some((popup, lon_lat)) }
    }

    /// The legend's mask changed: a popup on a hexagon it now hides closes.
    pub fn on_mask(&mut self, mask: u8) {
        if self.open.as_ref().is_some_and(|(p, _)| !hexes::modal_shown(p.modal(), mask)) {
            self.close();
        }
    }

    pub fn close(&mut self) {
        self.open = None;
    }

    pub fn is_closed(&self) -> bool {
        self.open.is_none()
    }

    pub fn on_zoom_start(&mut self) {
        self.close();
    }

    /// Any mode switch closes it (the popup is a hexagon's).
    pub fn on_mode_change(&mut self, _to: Mode) {
        self.close();
    }

    pub fn shown(&self) -> Option<(&HexPopup, (f64, f64))> {
        self.open.as_ref().map(|(p, at)| (p, *at))
    }
}

/// How far (logical px) the pointer may move between press and release for
/// the press to still be a click: a press that turns into a pan never opens
/// the popup.
pub const CLICK_SLOP_PX: f32 = 4.0;

/// Click-on-release for the engine's `FeatureEvent::Click`, which fires on
/// the PRESS (its semantics are other consumers' too, so they stay): the
/// press records where it was, the engine's hit arms the gate, and only a
/// release within [`CLICK_SLOP_PX`] of the press fires it.
#[derive(Debug, Clone, PartialEq)]
pub struct ClickGate<T> {
    press: Option<(f32, f32)>,
    hit: Option<T>,
}

impl<T> Default for ClickGate<T> {
    fn default() -> Self {
        Self { press: None, hit: None }
    }
}

impl<T> ClickGate<T> {
    /// A button went down at `at` (any hit from an earlier press is gone).
    pub fn press(&mut self, at: (f32, f32)) {
        self.press = Some(at);
        self.hit = None;
    }

    /// The engine hit something on the current press.
    pub fn hit(&mut self, hit: T) {
        if self.press.is_some() {
            self.hit = Some(hit);
        }
    }

    /// The button came up at `at`: the hit, if this was a click.
    pub fn release(&mut self, at: (f32, f32)) -> Option<T> {
        let press = self.press.take();
        let hit = self.hit.take();
        let (px, py) = press?;
        ((at.0 - px).hypot(at.1 - py) <= CLICK_SLOP_PX).then_some(hit).flatten()
    }
}

/// Where `(lon, lat)` is drawn in the map's own logical pixels on
/// `camera`'s frame, the globe included -- the engine's
/// `project_world_to_screen_on_frame` (crate-private), rebuilt from its
/// public parts: the flat view-projection, blended toward the sphere by the
/// frame's globeness (`GlobeAnchors`), the horizontal flip `shader.wgsl`
/// applies, then NDC to pixels. `None` behind the planet or the eye.
pub fn lon_lat_to_screen(camera: &gpui_maps::render::Camera, globe_enabled: bool, lon: f64, lat: f64) -> Option<(f32, f32)> {
    use gpui_maps::render::globe::{globe_frame, GlobeAnchors};
    let ws = f64::from(gpui_maps::render::WORLD_SIZE);
    let (tx, ty) = gpui_maps::geo::lon_lat_to_tile_xy(lon, lat, 0.0);
    let world = (tx * ws, ty * ws);
    let flat = camera.view_projection().as_dmat4() * glam::DVec4::new(world.0, world.1, 0.0, 1.0);
    let anchors = GlobeAnchors::new(&globe_frame(camera).with_globe_enabled(globe_enabled));
    let clip = match anchors.for_tile(gpui_maps::geo::TileId { z: 0, x: 0, y: 0 }, ws) {
        None => flat,
        Some(tile) => {
            let sphere = tile.sphere(world);
            if tile.occluded(sphere) {
                return None;
            }
            let mut flipped = flat;
            flipped.x = -flipped.x;
            let mut clip = tile.project(flipped, sphere);
            clip.x = -clip.x;
            clip
        }
    };
    if clip.w <= 0.0 {
        return None;
    }
    let (nx, ny) = (-clip.x / clip.w, clip.y / clip.w);
    Some((((nx + 1.0) * 0.5 * f64::from(camera.viewport_width_px)) as f32, ((1.0 - ny) * 0.5 * f64::from(camera.viewport_height_px)) as f32))
}

/// The popup's anchor on screen: [`lon_lat_to_screen`], `None` when the
/// point is off the map's viewport (the popup hides until it is back).
pub fn popup_anchor(camera: &gpui_maps::render::Camera, globe_enabled: bool, lon_lat: (f64, f64)) -> Option<(f32, f32)> {
    let (x, y) = lon_lat_to_screen(camera, globe_enabled, lon_lat.0, lon_lat.1)?;
    ((0.0..=camera.viewport_width_px).contains(&x) && (0.0..=camera.viewport_height_px).contains(&y)).then_some((x, y))
}

/// The HUD line in hexagon mode while the hexagons are the active layer:
/// the resolutions drawn with their opacity and hexagon counts, and the
/// errors. `layers` is `(res, opacity, hexagons drawn)`.
pub fn hex_hud_text(layers: &[(u8, f32, usize)], loading: bool, errors: u64, note: Option<&str>) -> String {
    if let Some(why) = note {
        return format!("no hexagons: {why}");
    }
    let shown: Vec<String> = layers
        .iter()
        .filter(|l| l.1 > 0.0)
        .map(|&(res, op, n)| format!("r{res} {} hexagons at {:.0}%", thousands(n as u64), f64::from(op) * 100.0))
        .collect();
    let mut line = if shown.is_empty() { "no hexagons in view".to_string() } else { shown.join(" · ") };
    if loading {
        line.push_str(" · loading…");
    }
    if errors > 0 {
        line.push_str(&format!(" · {errors} tile errors"));
    }
    line
}

// ---------------------------------------------------------------- data

/// Opens the race-dots archive, refusing a dot archive that is not a race
/// one (a GHSL grid would draw density colours, not groups).
pub fn open_dots(path: &Path) -> Result<Arc<dyn GridCountSource>, String> {
    if !path.exists() {
        return Err(format!(
            "no race archive at {} -- build it with `cargo run --release --manifest-path tools/race-build/Cargo.toml`",
            path.display()
        ));
    }
    let source = fetch_native::open_pmtiles_grid(&path.to_string_lossy()).map_err(|e| format!("{}: {e}", path.display()))?;
    if source.race().is_none() {
        return Err(format!("{} is a dot archive but not a race one (no \"race\" metadata)", path.display()));
    }
    Ok(Arc::new(source))
}

/// The basemap under the dots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basemap {
    /// Mapbox Light v11 (`demos/mapbox-light-style.json`): Walker's grey
    /// look (spec §9). Needs `MAPBOX_ACCESS_TOKEN`.
    MapboxLight,
    /// OpenFreeMap Positron: keyless, also pale grey.
    Positron,
    /// No tiles and no network: the dots alone on a pale ground.
    None,
}

impl Basemap {
    /// Mapbox Light with a token, else Positron.
    pub fn from_env() -> Self {
        if mapbox_token().is_some() {
            Basemap::MapboxLight
        } else {
            Basemap::Positron
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Basemap::MapboxLight => "Mapbox Light",
            Basemap::Positron => "OpenFreeMap Positron",
            Basemap::None => "none",
        }
    }
}

fn mapbox_token() -> Option<String> {
    crate::kit::env_mapbox_token()
}

/// A basemap's compiled style, its tiles and (Mapbox) its sprite URL.
pub struct BasemapSpec {
    pub style: gpui_maps::style::Style,
    pub tiles: Arc<dyn TileSource>,
    /// The resolved sprite URL (with the token); never printed.
    pub sprite_url: Option<String>,
}

/// Builds `which`. Mapbox Light without a token panics: the caller chose it.
pub fn basemap(which: Basemap) -> BasemapSpec {
    basemap_with_token(which, mapbox_token().as_deref())
}

/// [`basemap`] with the Mapbox token handed in (the gallery reads it from
/// the environment or `~/.config/gpui-map/mapbox.env`, not only the
/// environment). Mapbox Light without one panics: the caller chose it.
pub fn basemap_with_token(which: Basemap, token: Option<&str>) -> BasemapSpec {
    match which {
        Basemap::MapboxLight => {
            let token = token.expect("Mapbox Light needs MAPBOX_ACCESS_TOKEN").to_string();
            let bytes = crate::kit::asset_bytes("demos/mapbox-light-style.json").expect("vendored Mapbox Light style");
            let style = gpui_maps::style_spec::parse_and_compile(&bytes).expect("Mapbox Light must compile");
            let url = gpui_maps::basemap::mapbox_composite_tile_url(&bytes, &token).expect("a mapbox:// composite source");
            let tiles = HttpTileSource::new(
                url,
                16,
                concat!(
                    r#"<a href="https://www.mapbox.com/about/maps" target="_blank">&copy; Mapbox</a> "#,
                    r#"<a href="https://www.openstreetmap.org/copyright/" target="_blank">&copy; OpenStreetMap</a>"#,
                ),
            );
            let sprite_url = style.sprite.clone().and_then(|s| gpui_maps::sprite::resolve_sprite_url(&s, Some(&token)).ok());
            BasemapSpec { style, tiles: Arc::new(tiles), sprite_url }
        }
        Basemap::Positron => {
            let bytes = crate::kit::asset_bytes("demos/positron-style.json").expect("vendored Positron style");
            let style = gpui_maps::style_spec::parse_and_compile(&bytes).expect("Positron must compile");
            let tiles = HttpTileSource::new(
                "https://tiles.openfreemap.org/planet/20260802_080001_pt/{z}/{x}/{y}.pbf",
                14,
                concat!(
                    r#"<a href="https://openfreemap.org" target="_blank">OpenFreeMap</a> "#,
                    r#"<a href="https://www.openmaptiles.org/" target="_blank">&copy; OpenMapTiles</a> "#,
                    r#"Data from <a href="https://www.openstreetmap.org/copyright" target="_blank">OpenStreetMap</a>"#,
                ),
            );
            BasemapSpec { style, tiles: Arc::new(tiles), sprite_url: None }
        }
        Basemap::None => BasemapSpec {
            style: gpui_maps::style::Style { layers: Vec::new(), sprite: None, sky: Default::default() },
            tiles: Arc::new(gpui_maps::tile_source::EmptyTileSource),
            sprite_url: None,
        },
    }
}

/// The tokio runtime the panel's map fetches on: one per process, shared by
/// every panel (the gallery opens one at a time).
pub fn runtime_handle() -> tokio::runtime::Handle {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME
        .get_or_init(|| tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("a tokio runtime"))
        .handle()
        .clone()
}

// ---------------------------------------------------------------- panel

/// How the panel opens: the standalone demo fills one from its flags; the
/// gallery takes the defaults (Walker's start, the environment's basemap).
#[derive(Debug, Clone, Copy)]
pub struct PanelOptions {
    pub basemap: Basemap,
    /// Centre lon, lat and zoom.
    pub at: (f64, f64, f64),
    /// `DotStyle::budget_dots` override (`--budget`).
    pub budget: Option<u32>,
    /// The map's size before the first layout.
    pub size: (f32, f32),
    /// Groups toggled off at the start (`--hide`).
    pub legend: Legend,
    /// The view at the start (`--mode`).
    pub mode: Mode,
}

impl Default for PanelOptions {
    fn default() -> Self {
        Self { basemap: Basemap::from_env(), at: walker::START, budget: None, size: (1280.0, 800.0), legend: Legend::default(), mode: Mode::Dots }
    }
}

/// Who hosts the panel: its own window ([`PanelHost::standalone`]) or the
/// gallery (`kit/src/panels/race_dots.rs`' `build`).
#[derive(Clone)]
pub struct PanelHost {
    /// The tokio runtime the map (and the sprite fetch) run on.
    pub runtime: tokio::runtime::Handle,
    /// The Mapbox token for [`Basemap::MapboxLight`]. Never printed.
    pub mapbox_token: Option<String>,
    /// The window-title prefix the demo keys publish; `None` in the
    /// gallery, which owns its window's title.
    pub title: Option<String>,
    /// Raised by the gallery when it switches away: sources that finish
    /// opening after that are never attached.
    pub cancel: Arc<AtomicBool>,
    /// Whether the in-map "About this map" card starts open: in its own
    /// window, yes; in the gallery, which has its own About pane, no.
    pub about_open: bool,
}

impl PanelHost {
    /// The standalone window: the process-wide runtime, the environment's
    /// token, the "Race dots" title.
    pub fn standalone() -> Self {
        Self { runtime: runtime_handle(), mapbox_token: mapbox_token(), title: Some("Race dots".to_string()), cancel: Arc::default(), about_open: true }
    }
}

/// The three sources, opened off the UI thread by [`open_sources`]: each is
/// a file read (the dots' header and root directory, the hexagons', the
/// clusters' whole directory), which must not stall the first frame.
pub struct OpenedSources {
    pub dots: Result<Arc<dyn GridCountSource>, String>,
    /// `None`: no hex source was given.
    pub hexes: Option<Result<Arc<dyn gpui_maps::overlay::hexes::HexSource>, String>>,
    /// `None`: no cluster archive was given.
    pub clusters: Option<Result<Arc<clusters::ClusterArchive>, String>>,
}

/// Opens every source `sources` names. Blocking: run it on a background
/// thread ([`RaceMapPanel::new`] does).
pub fn open_sources(sources: &RaceSources) -> OpenedSources {
    OpenedSources {
        dots: open_dots(&sources.dots),
        hexes: sources.hexes.as_ref().map(hexes::open_hexes),
        clusters: sources.clusters.as_deref().map(clusters::open_clusters),
    }
}

/// `text` for a person to read: the home directory as `~` (a card never
/// prints `/Users/<name>/...` in full).
fn tilde_text(text: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => text.replace(&home, "~"),
        _ => text.to_string(),
    }
}

/// Why a mode's radio is greyed out, short: an open error's first clause
/// (its build command is the gallery's banner's and the README's), with
/// `~` for the home directory.
pub fn unavailable_reason(note: Option<&str>) -> String {
    let why = note.unwrap_or("no source");
    let why = why.split(" -- ").next().unwrap_or(why);
    tilde_text(why.trim())
}

/// Whether the pointer should be the pointing hand: over a hexagon that
/// takes a click (hexagon mode, its resolution drawn, its group shown), or
/// over a donut in cluster mode -- what `on_feature_click` and
/// `on_donut_click` act on, and nothing else.
pub fn pointing_hand(mode: Mode, hex_clickable: bool, over_donut: bool) -> bool {
    match mode {
        Mode::Hexagons => hex_clickable,
        Mode::Clusters | Mode::States => over_donut,
        Mode::Dots => false,
    }
}

pub struct RaceMapPanel {
    /// People by group, linked to the legend's mask and the hexagons
    /// (the shared `linked_chart` component).
    pub chart: Entity<crate::linked_chart::LinkedChart>,
    /// What the chart's totals were last counted for: the mode, the
    /// hexagons loaded and the zoom (to 1/10).
    chart_key: (Mode, usize, i64),
    map: Entity<MapView>,
    keys: crate::shell::DemoKeys,
    mode: Mode,
    legend: Legend,
    about_open: bool,
    stats: Option<DotStats>,
    zoom: f64,
    /// Why there are no dots (the archive did not open), shown in the HUD.
    note: Option<String>,
    basemap: Basemap,
    /// The map has had the keyboard once ([`Render::render`]).
    focused_once: bool,
    /// The archive's parts/cells crossover zoom, when it opened.
    crossover: Option<f64>,
    /// Hexagon mode's tiles (`None`: no hex source, or it did not open).
    hexes: Option<HexTiles>,
    /// Why there are no hexagons, shown in the HUD.
    hex_note: Option<String>,
    /// Per resolution (`RESOLUTIONS` order), the `PaintKey`
    /// its source's paint was last given.
    hex_paint: [Option<hexes::PaintKey>; 5],
    /// The camera the hex tiles were last wanted for, and whether a tile
    /// landed since (either one re-plans).
    hex_camera: Option<gpui_maps::render::Camera>,
    hex_landed: bool,
    /// Hex paint updates by the path the engine took (table-only, rebake,
    /// replan): a zoom step should be table-only.
    pub hex_paint_paths: [u64; 3],
    popup: PopupState,
    /// Press/release gating of the engine's (press-time) hexagon hits:
    /// `(source, feature index, ground lon/lat)`.
    click: ClickGate<(String, usize, (f64, f64))>,
    /// When the hex sources were last pushed while tiles were still
    /// loading (pushes are coalesced to one per `HEX_PUSH_EVERY` then).
    hex_pushed_at: Option<std::time::Instant>,
    /// When the hex cross-fade last stepped (`hexes::ease_toward`'s
    /// clock), and each resolution's eased readiness
    /// (`hexes::stack_opacities`).
    hex_faded_at: Option<std::time::Instant>,
    hex_ready: [f32; 5],
    /// Cluster mode's tiles (`None`: no cluster archive, or it did not open).
    clusters: Option<ClusterTiles>,
    /// Why there are no clusters, shown in the HUD.
    cluster_note: Option<String>,
    /// The camera the cluster tiles were last wanted for, and whether a
    /// tile landed since.
    cluster_camera: Option<gpui_maps::render::Camera>,
    cluster_landed: bool,
    /// What the donuts and labels were last given: the tiles' generation,
    /// the legend mask and the (quantised) fade.
    cluster_shown: Option<(u64, u8, f32)>,
    /// The records behind the donuts, in the donuts' order (`donut_at`'s
    /// index is an index here).
    cluster_drawn: Vec<ClusterRecord>,
    /// Records the donut cap left out of the last push (review L3).
    cluster_truncated: usize,
    /// Press/release gating of a click on a donut.
    donut_click: ClickGate<()>,
    /// The panel's origin in the window (logical px), recorded each paint:
    /// `donut_at` takes the map's own pixels, and the map fills the panel.
    origin: std::rc::Rc<std::cell::Cell<(f32, f32)>>,
    /// The map-local point the last donut click asked `donut_at` about.
    pub last_donut_query: Option<(f32, f32)>,
    /// The map-local point the last pointer move asked `donut_at` about.
    pub last_donut_hover: Option<(f32, f32)>,
    /// Where the last donut click that hit a donut eased the camera to.
    pub last_donut_target: Option<(f64, f64, f64)>,
    /// The hexagon under the pointer (hexagon mode): the panel's own hit
    /// test against its records ([`hexes::hex_at`]), run at most once per
    /// frame at the latest pointer, and again when the camera settles
    /// somewhere new. Its id is its index, as `hexes::with_features` gives
    /// it, so it names the drawn feature the highlight lights.
    hover_hex: Option<gpui_maps::overlay::FeatureRef>,
    /// The pointer over the map, map-local; `None` off the map.
    hex_pointer: Option<(f32, f32)>,
    /// A pointer move (or a settled camera, or a push) since the last hit
    /// test: the next render runs one.
    hex_pick_pending: bool,
    /// The camera the last hit test saw.
    hex_pick_camera: Option<gpui_maps::render::Camera>,
    /// Hit tests run (each pointer move does not run one).
    pub hex_picks: usize,
    /// The hexagon the click popup is about, lit while nothing is hovered.
    pinned_hex: Option<gpui_maps::overlay::FeatureRef>,
    /// What the panel last asked the map to highlight, and when: a
    /// hexagon whose source was still baking draws no highlight, and is
    /// asked for again (`HIGHLIGHT_RETRY`) until it does.
    lit_hex: Option<gpui_maps::overlay::FeatureRef>,
    lit_requested_at: Option<std::time::Instant>,
    /// A donut is under the pointer (cluster mode).
    over_donut: bool,
    /// The sources are still opening (off the UI thread).
    loading: bool,
    /// The mode to switch to once the sources are open (`--mode`).
    pending_mode: Option<Mode>,
    /// `DotStyle::budget_dots` override, kept for the dots' attach.
    budget: Option<u32>,
    /// States mode: the selected state (an index into `states::rows`),
    /// `None` for the United States.
    selected_state: Option<usize>,
    /// The states chart's order.
    sort: SortKey,
    /// The group the chart's share sort reads (the last group row clicked).
    share_group: usize,
    /// What the pies and outlines were last given: the legend mask, the
    /// quantised zoom scale and the selection; `None`: not drawn.
    states_shown: Option<(u8, f32, Option<usize>)>,
    /// The rows behind the pies, in the pies' order (`donut_at`'s index is
    /// an index here).
    states_drawn: Vec<usize>,
    /// The pies' total labels last handed to the map: `(fips, text)`, in
    /// the pies' order (empty outside States mode).
    state_labels: Vec<(u8, String)>,
    /// Where the last state selection eased the camera to.
    pub last_state_target: Option<(f64, f64, f64)>,
}

/// The most a donut click zooms to (spec §5.2: `expansion_zoom`, capped).
pub const DONUT_CLICK_MAX_ZOOM: f64 = 14.0;
/// How long a donut click's ease takes.
pub const DONUT_EASE: Duration = Duration::from_millis(600);
/// How long a state selection's ease takes.
pub const STATE_EASE: Duration = Duration::from_millis(900);

/// Where a click on a donut of `record` eases to: its centre, at its
/// expansion zoom capped at [`DONUT_CLICK_MAX_ZOOM`].
pub fn donut_click_target(record: &ClusterRecord) -> (f64, f64, f64) {
    (record.lon, record.lat, f64::from(record.expansion_zoom).min(DONUT_CLICK_MAX_ZOOM))
}

/// What [`cluster_hud_text`] reports.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ClusterHud {
    /// Donuts given to the map, after the cap.
    pub donuts: usize,
    /// Records the cap left out.
    pub truncated: usize,
    pub tile_zoom: u8,
    pub loading: bool,
    pub errors: u64,
    pub refused: u64,
}

/// The HUD line in cluster mode while the donuts are the active layer.
pub fn cluster_hud_text(h: ClusterHud, note: Option<&str>) -> String {
    let ClusterHud { donuts, truncated, tile_zoom, loading, errors, refused } = h;
    if let Some(why) = note {
        return format!("no clusters: {why}");
    }
    let mut line = format!("{} donuts · z{tile_zoom} clusters", thousands(donuts as u64));
    if truncated > 0 {
        line.push_str(&format!(" · {} truncated", thousands(truncated as u64)));
    }
    if loading {
        line.push_str(" · loading…");
    }
    if errors > 0 {
        line.push_str(&format!(" · {errors} tile errors"));
    }
    if refused > 0 {
        line.push_str(&format!(" · {refused} corrupt records refused"));
    }
    line
}

/// The HUD line in States mode: the pies drawn and the selection.
pub fn states_hud_text(selected: Option<usize>, pies: usize) -> String {
    match selected {
        Some(i) => {
            let r = &states::rows()[i];
            format!("{} · {} people · {pies} state pies", r.name, thousands(r.total()))
        }
        None => format!("United States · {} people · {pies} state pies", thousands(walker::US_TOTAL)),
    }
}

/// How long a requested hexagon highlight the map could not draw yet (its
/// source still baking) waits before it is asked for again.
const HIGHLIGHT_RETRY: Duration = Duration::from_millis(250);

/// While hex tiles are still landing, a resolution is re-pushed at most
/// this often (review L1: each push re-plans the whole resolution on the UI
/// thread); once the view has settled it is pushed at once.
const HEX_PUSH_EVERY: Duration = Duration::from_millis(250);

impl RaceMapPanel {
    /// The standalone window's panel: [`Self::new`] with
    /// [`PanelHost::standalone`].
    pub fn with_options(window: &mut Window, cx: &mut Context<Self>, sources: RaceSources, options: PanelOptions) -> Self {
        Self::new(window, cx, sources, options, PanelHost::standalone())
    }

    /// The panel, hosted by `host`. Nothing here blocks: the basemap's
    /// sprite sheet is fetched on `host.runtime` and the three archives are
    /// opened on the background executor; the map draws its basemap at
    /// once, the HUD says "loading dots…", and the dots, hexagons and
    /// clusters are attached ([`Self::attach`]) when they have opened --
    /// unless `host.cancel` was raised (the gallery switched away) or the
    /// panel is gone by then.
    pub fn new(_window: &mut Window, cx: &mut Context<Self>, sources: RaceSources, options: PanelOptions, host: PanelHost) -> Self {
        let spec = basemap_with_token(options.basemap, host.mapbox_token.as_deref());
        let sprite_url = spec.sprite_url.clone();
        let (lon, lat, zoom) = options.at;
        let (w, h) = options.size;
        let runtime = host.runtime.clone();
        let map = cx.new(|_| {
            let mut view = MapView::new(lon, lat, zoom, f64::from(w), f64::from(h), spec.style, spec.tiles, runtime.clone());
            view.set_chrome_above_map(true);
            // Walker's `minZoom` / `maxZoom`: every input clamps to it.
            view.set_zoom_range(walker::MIN_ZOOM, walker::MAX_ZOOM);
            view
        });
        // Off the UI thread (the panel used to block its first frame on
        // it); a failure costs icons only and is logged, redacted, by kit.
        if let Some(url) = sprite_url {
            crate::kit::spawn_sprites(map.downgrade(), url, None, host.runtime.clone(), cx);
        }
        // The engine's Click fires on the press: it arms the click gate, and
        // the release (`render`'s capture-phase mouse up) opens the popup.
        // The hexagon hover is the panel's own (`resolve_hex_hover`).
        cx.subscribe(&map, |this: &mut Self, _map, event: &gpui_maps::overlay::FeatureEvent, _cx| {
            if let gpui_maps::overlay::FeatureEvent::Click { feature, lon_lat } = event {
                this.click.hit((feature.source.to_string(), feature.index, *lon_lat));
            }
        })
        .detach();
        // The popup follows its hexagon: re-projected whenever the map
        // changes, not only on the 30 Hz tick.
        cx.observe(&map, |this: &mut Self, _map, cx| {
            if !this.popup.is_closed() {
                cx.notify();
            }
        })
        .detach();
        let keys = crate::shell::DemoKeys::with_satellite_titled(&map, host.title.clone(), None, cx);
        // The HUD, the status line, the hex tiles and the cross-fades
        // follow the map at 30 Hz.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_millis(33)).await;
            if this.update(cx, |panel, cx| panel.tick(cx)).is_err() {
                return;
            }
        })
        .detach();
        // The archives, off the UI thread.
        let opened = cx.background_executor().spawn(async move { open_sources(&sources) });
        let cancel = host.cancel.clone();
        cx.spawn(async move |this, cx| {
            let opened = opened.await;
            if cancel.load(Ordering::SeqCst) {
                return;
            }
            this.update(cx, |panel, cx| panel.attach(opened, cx)).ok();
        })
        .detach();
        let chart = group_chart(&map, cx.entity().downgrade(), cx);
        Self {
            chart,
            chart_key: (Mode::Dots, 0, 0),
            map,
            keys,
            mode: Mode::Dots,
            legend: options.legend,
            about_open: host.about_open,
            stats: None,
            zoom,
            note: None,
            basemap: options.basemap,
            focused_once: false,
            crossover: None,
            hexes: None,
            hex_note: None,
            hex_paint: [None; 5],
            hex_camera: None,
            hex_landed: false,
            hex_paint_paths: [0; 3],
            popup: PopupState::default(),
            click: ClickGate::default(),
            hex_pushed_at: None,
            hex_faded_at: None,
            hex_ready: [0.0; 5],
            clusters: None,
            cluster_note: None,
            cluster_camera: None,
            cluster_landed: false,
            cluster_shown: None,
            cluster_drawn: Vec::new(),
            cluster_truncated: 0,
            donut_click: ClickGate::default(),
            origin: std::rc::Rc::new(std::cell::Cell::new((0.0, 0.0))),
            last_donut_query: None,
            last_donut_hover: None,
            last_donut_target: None,
            hover_hex: None,
            hex_pointer: None,
            hex_pick_pending: false,
            hex_pick_camera: None,
            hex_picks: 0,
            pinned_hex: None,
            lit_hex: None,
            lit_requested_at: None,
            over_donut: false,
            loading: true,
            pending_mode: Some(options.mode),
            budget: options.budget,
            selected_state: None,
            sort: SortKey::Population,
            share_group: 0,
            states_shown: None,
            states_drawn: Vec::new(),
            state_labels: Vec::new(),
            last_state_target: None,
        }
    }

    /// Hands the opened sources to the map: the dots as the dot layer (the
    /// legend's mask as it is now), one empty overlay source per hexagon
    /// resolution, the cluster tiles; then the mode asked for at the start.
    /// A source that did not open is its mode's note, shown in the HUD and
    /// beside its greyed-out radio.
    fn attach(&mut self, opened: OpenedSources, cx: &mut Context<Self>) {
        self.loading = false;
        let mask = self.legend.mask();
        match opened.dots {
            Ok(source) => {
                self.crossover = source.race().map(|r| r.crossover);
                let mut style = walker::dot_style(self.budget);
                style.category_mask = mask;
                self.map.update(cx, |view, cx| view.set_dot_density_source(LAYER, source, style, cx));
            }
            Err(why) => {
                eprintln!("race_dots: {why}");
                self.note = Some(why);
            }
        }
        // One overlay source per hexagon resolution, created coarsest
        // first so a finer one draws over a coarser one through each
        // cross-fade (a name keeps its place in draw order across pushes),
        // under the basemap's labels as the dots are.
        match opened.hexes {
            Some(Ok(source)) => {
                self.map.update(cx, |view, cx| {
                    // The hex archive's own credit, collected with the
                    // dots' (the engine drops it when it is the same).
                    view.set_source_credit("race-hexes", source.attribution());
                    for res in RESOLUTIONS {
                        let name = hexes::source_name(res);
                        view.declare_source_paint(&name, &hexes::hex_paint(0.0, mask)).expect("the hex paint compiles");
                        hexes::with_features(&[], |f, c| view.set_source_columns(&name, f, c, cx)).expect("an empty source");
                        view.set_source_below_labels(&name, true);
                    }
                });
                self.hexes = Some(HexTiles::new(source));
            }
            Some(Err(why)) => {
                eprintln!("race_dots: {why}");
                self.hex_note = Some(why);
            }
            None => self.hex_note = Some("no hex source given".to_string()),
        }
        match opened.clusters {
            Some(Ok(archive)) => {
                // The archive's own credit as a source credit, collected
                // with the dots' -- not a hand-made "Clusters: ..." line
                // appended after it: "U.S. Census Bureau" (the dots) is
                // inside "U.S. Census Bureau, 2020 P.L. 94-171" (the
                // clusters), so the map states the holder once, in full.
                self.map.update(cx, |view, cx| {
                    view.set_source_credit("race-clusters", Some(&archive.attribution));
                    cx.notify();
                });
                self.clusters = Some(ClusterTiles::new(archive));
            }
            Some(Err(why)) => {
                eprintln!("race_dots: {why}");
                self.cluster_note = Some(why);
            }
            None => self.cluster_note = Some("no cluster archive given".to_string()),
        }
        let mode = self.pending_mode.take().unwrap_or(self.mode);
        self.set_mode(mode, cx);
        cx.notify();
    }

    /// Whether the in-map About card is expanded.
    pub fn about_expanded(&self) -> bool {
        self.about_open
    }

    /// The sources have opened (or failed to): the map's attribution line
    /// now carries the dot archive's credit.
    pub fn loaded(&self) -> bool {
        !self.loading
    }

    /// Why each greyed-out mode is greyed out, for the switcher: nothing
    /// while the sources are still opening.
    pub fn unavailable_lines(&self) -> Vec<String> {
        if self.loading {
            return Vec::new();
        }
        let mut lines = Vec::new();
        if self.hexes.is_none() {
            lines.push(format!("Hexagons off: {}", unavailable_reason(self.hex_note.as_deref())));
        }
        if self.clusters.is_none() {
            lines.push(format!("Clusters off: {}", unavailable_reason(self.cluster_note.as_deref())));
        }
        lines
    }

    /// The hovered hexagon's largest group, when that hexagon is drawn
    /// (its resolution visible, its group on): `None` for a tie.
    fn hovered_hex_group(&self) -> Option<Option<u8>> {
        let f = self.hover_hex.as_ref()?;
        let res = hexes::res_of_source(&f.source)?;
        let rec = self.hexes.as_ref().and_then(|t| t.records(res).get(f.index))?;
        (walker::hex_opacity(res, self.zoom) > 0.0 && hexes::modal_shown(rec.modal(), self.legend.mask())).then(|| match rec.modal() {
            gpui_maps::overlay::hexes::Modal::Group(g) => Some(g),
            gpui_maps::overlay::hexes::Modal::Tie => None,
        })
    }

    /// The chart follows the map: the hovered hexagon's largest group's
    /// bar lights (the shared popup's `follow_popup`, from the engine's
    /// own hover events -- the hexagons keep Walker's click popup).
    fn sync_chart_hover(&mut self, cx: &mut Context<Self>) {
        let slot = if self.mode == Mode::Hexagons { self.hovered_hex_group().flatten().map(usize::from) } else { None };
        self.chart.update(cx, |c, cx| c.set_map_hover(slot, cx));
    }

    /// Whether the pointer is the pointing hand now ([`pointing_hand`]).
    pub fn pointer(&self) -> bool {
        let hex_clickable = self.hovered_hex_group().is_some();
        pointing_hand(self.mode, hex_clickable, self.over_donut)
    }

    /// A pointer move over the map at window point `at`: in cluster mode,
    /// whether a donut is under it (map-local, as the click asks).
    fn on_map_move(&mut self, at: (f32, f32), cx: &mut Context<Self>) {
        let (ox, oy) = self.origin.get();
        let local = (at.0 - ox, at.1 - oy);
        if self.mode == Mode::Hexagons {
            // Hexagon hover: the pointer is kept and the hit test waits for
            // the next render -- one per frame, however many moves arrive
            // -- which also moves the card after the pointer.
            self.hex_pointer = Some(local);
            self.hex_pick_pending = true;
            cx.notify();
        }
        let over = matches!(self.mode, Mode::Clusters | Mode::States) && {
            self.last_donut_hover = Some(local);
            let hit = self.map.read(cx).donut_at(local.0, local.1);
            hit.is_some_and(|(layer, i)| match self.mode {
                Mode::States => layer == states::PIE_LAYER && i < self.states_drawn.len(),
                _ => layer == DONUT_LAYER && i < self.cluster_drawn.len(),
            })
        };
        if over != self.over_donut {
            self.over_donut = over;
            cx.notify();
        }
    }

    /// The pointer left the map (onto a card, or out of the panel): no
    /// hexagon is hovered.
    fn on_map_leave(&mut self, cx: &mut Context<Self>) {
        self.hex_pointer = None;
        self.hex_pick_pending = false;
        if self.hover_hex.take().is_some() {
            self.sync_chart_hover(cx);
            self.sync_hex_highlight(cx);
            cx.notify();
        }
    }

    /// The hexagon under `local` (map-local px) now, as the feature the
    /// map draws it as.
    fn hex_under(&self, local: (f32, f32), cx: &App) -> Option<gpui_maps::overlay::FeatureRef> {
        let tiles = self.hexes.as_ref()?;
        let (camera, globe) = {
            let m = self.map.read(cx);
            (*m.camera(), m.globe_enabled())
        };
        let ws = f64::from(gpui_maps::render::WORLD_SIZE);
        let w = gpui_maps::render::globe::unproject_to_world_on_globe(&camera, globe, local.0, local.1)?;
        let (lon, lat) = gpui_maps::geo::tile_xy_to_lon_lat(w.x / ws, w.y / ws, 0.0);
        let layers: Vec<(u8, f32, &[HexRecord])> = RESOLUTIONS.iter().map(|&r| (r, walker::hex_opacity(r, self.zoom), tiles.records(r))).collect();
        let (res, index) = hexes::hex_at(&layers, lon, lat, self.legend.mask())?;
        Some(gpui_maps::overlay::FeatureRef {
            source: hexes::source_name(res).into(),
            index,
            id: Some(gpui_maps::overlay::FeatureId::Number(index as f64)),
        })
    }

    /// Runs the pending hexagon hit test (from `render`: at most once a
    /// frame): the hovered hexagon, its bar in the chart, its highlight.
    /// While the camera moves nothing is hovered -- the hexagon would
    /// slide out from under the pointer a frame later -- and the test runs
    /// again once it settles.
    fn resolve_hex_hover(&mut self, cx: &mut Context<Self>) {
        if !self.hex_pick_pending {
            return;
        }
        let (camera, moving) = {
            let m = self.map.read(cx);
            (*m.camera(), m.camera_in_motion())
        };
        let hover = match self.hex_pointer {
            Some(at) if self.mode == Mode::Hexagons && !moving => {
                self.hex_pick_pending = false;
                self.hex_pick_camera = Some(camera);
                self.hex_picks += 1;
                self.hex_under(at, cx)
            }
            Some(_) if self.mode == Mode::Hexagons => None,
            _ => {
                self.hex_pick_pending = false;
                None
            }
        };
        if hover != self.hover_hex {
            self.hover_hex = hover;
            self.sync_chart_hover(cx);
        }
        self.sync_hex_highlight(cx);
    }

    /// What the map should light: the hovered hexagon, else the one whose
    /// popup is open, else nothing.
    fn hex_highlight_wanted(&self) -> Option<gpui_maps::overlay::FeatureRef> {
        let hovered = self.hovered_hex_group().is_some().then(|| self.hover_hex.clone()).flatten();
        let pinned = (self.mode == Mode::Hexagons && !self.popup.is_closed()).then(|| self.pinned_hex.clone()).flatten();
        hovered.or(pinned)
    }

    /// The map lights [`Self::hex_highlight_wanted`] -- asked only when
    /// that changes (the engine draws it as one small highlight mesh; the
    /// hexagon sources are not touched).
    fn sync_hex_highlight(&mut self, cx: &mut Context<Self>) {
        let want = self.hex_highlight_wanted();
        if want == self.lit_hex {
            return;
        }
        self.request_hex_highlight(want, cx);
    }

    fn request_hex_highlight(&mut self, want: Option<gpui_maps::overlay::FeatureRef>, cx: &mut Context<Self>) {
        self.lit_requested_at = Some(std::time::Instant::now());
        self.lit_hex = want.clone();
        self.map.update(cx, |view, cx| {
            view.set_highlight(want.map(gpui_maps::map_view::MapObject::Feature), crate::popup::default_highlight(), cx);
        });
    }

    /// The hexagon the map is asked to light now.
    pub fn lit_hexagon(&self) -> Option<&gpui_maps::overlay::FeatureRef> {
        self.lit_hex.as_ref()
    }

    /// The hovered hexagon, when it is drawn and shown.
    pub fn hovered_hexagon(&self) -> Option<&gpui_maps::overlay::FeatureRef> {
        self.hovered_hex_group().and(self.hover_hex.as_ref())
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Whether `mode` can be chosen now: hexagons and clusters need their
    /// sources.
    pub fn mode_available(&self, mode: Mode) -> bool {
        mode.enabled()
            && match mode {
                Mode::Dots | Mode::States => true,
                Mode::Hexagons => self.hexes.is_some(),
                Mode::Clusters => self.clusters.is_some(),
            }
    }

    /// Donuts given to the map now (on screen or not), after the cap.
    pub fn donuts_drawn(&self) -> usize {
        self.cluster_drawn.len()
    }

    /// Records the donut cap left out of the last push.
    pub fn donuts_truncated(&self) -> usize {
        self.cluster_truncated
    }

    /// Cluster records refused as corrupt, over the panel's life.
    pub fn clusters_refused(&self) -> u64 {
        self.clusters.as_ref().map_or(0, |t| t.refused)
    }

    /// A click (released within the slop) at window point `at` in cluster
    /// mode: the donut under it eases the camera to its expansion zoom.
    fn on_donut_click(&mut self, at: (f32, f32), cx: &mut Context<Self>) {
        if !matches!(self.mode, Mode::Clusters | Mode::States) {
            return;
        }
        let (ox, oy) = self.origin.get();
        let local = (at.0 - ox, at.1 - oy);
        self.last_donut_query = Some(local);
        let hit = self.map.read(cx).donut_at(local.0, local.1);
        if self.mode == Mode::States {
            // A state's pie selects the state.
            if let Some(i) = hit.filter(|(layer, _)| layer == states::PIE_LAYER).and_then(|(_, i)| self.states_drawn.get(i).copied()) {
                self.select_state(Some(i), cx);
            }
            return;
        }
        let Some(record) = hit.filter(|(layer, _)| layer == DONUT_LAYER).and_then(|(_, i)| self.cluster_drawn.get(i).copied()) else { return };
        let (lon, lat, zoom) = donut_click_target(&record);
        self.last_donut_target = Some((lon, lat, zoom));
        self.map.update(cx, |view, cx| {
            view.ease_to(gpui_maps::camera_control::CameraTarget { center: Some((lon, lat)), zoom: Some(zoom), ..Default::default() }, DONUT_EASE);
            cx.notify();
        });
    }

    /// Plans the cluster tiles for the camera, starts their fetches, and
    /// hands the drawn set to the map as donuts and count labels whenever
    /// it, the legend or the fade changed; the dots' fade in cluster mode.
    fn drive_clusters(&mut self, cx: &mut Context<Self>) {
        let Some(tiles) = self.clusters.as_mut() else { return };
        let (camera, globe_on) = {
            let m = self.map.read(cx);
            (*m.camera(), m.globe_enabled())
        };
        let zoom = camera.zoom;
        let alpha = clusters::quantise_alpha(walker::cluster_alpha(zoom));
        let active = self.mode == Mode::Clusters && alpha > 0.0;
        let now = std::time::Instant::now();
        if !active {
            tiles.clear_wanted();
            self.cluster_camera = None;
        } else if self.cluster_camera != Some(camera) || self.cluster_landed || tiles.has_unrequested() || tiles.retry_due(now) {
            self.cluster_camera = Some(camera);
            self.cluster_landed = false;
            let tz = clusters::tile_zoom(zoom);
            let frame = gpui_maps::render::globe::globe_frame(&camera).with_globe_enabled(globe_on);
            let globe = gpui_maps::render::globe::GlobeCover::new(&frame);
            let cover: Vec<gpui_maps::geo::TileId> =
                gpui_maps::render::cover_plan::tile_cover(tz, &globe, || gpui_maps::render::cover_plan::flat_cover(&camera, tz).tiles)
                    .into_iter()
                    .filter(|t| t.z == tz)
                    .collect();
            for fetch in tiles.want(cover, now) {
                cx.spawn(async move |this, cx| {
                    let f = fetch.clone();
                    let result = cx
                        .background_executor()
                        .spawn(async move { clusters::run_fetch(&f, gpui_maps::overlay::dotgrid::fetch_native::execute) })
                        .await;
                    this.update(cx, |panel, cx| {
                        if let Some(t) = panel.clusters.as_mut() {
                            t.land(fetch.tile, result, std::time::Instant::now());
                        }
                        panel.cluster_landed = true;
                        cx.notify();
                    })
                    .ok();
                })
                .detach();
            }
        }
        let mask = self.legend.mask();
        let shown = (tiles.generation(), mask, if active { alpha } else { 0.0 });
        if self.cluster_shown != Some(shown) {
            self.cluster_shown = Some(shown);
            let records: Vec<ClusterRecord> = if active { tiles.records().to_vec() } else { Vec::new() };
            let clusters::Drawable { donuts, records, truncated } = clusters::drawable(&records, mask, shown.2);
            let labels = clusters::labels_geojson(&records, mask, shown.2);
            self.cluster_drawn = records;
            self.cluster_truncated = truncated;
            self.map.update(cx, |view, cx| {
                if donuts.is_empty() {
                    view.remove_donuts(DONUT_LAYER, cx);
                } else {
                    view.set_donuts(DONUT_LAYER, donuts, gpui_maps::overlay::dotgrid::layer::RACE_PALETTE, cx);
                }
                if let Err(e) = view.set_source(LABEL_SOURCE, &labels, cx) {
                    eprintln!("race_dots: cluster labels refused: {e:?}");
                }
            });
        }
        if self.mode == Mode::Clusters {
            let fade = walker::dot_opacity_in_cluster_mode(zoom);
            self.map.update(cx, |m, cx| m.set_dot_opacity(LAYER, fade, cx));
        }
    }

    /// Hexagons drawn now, over every resolution.
    pub fn hexagons_drawn(&self) -> usize {
        self.hexes.as_ref().map_or(0, |t| RESOLUTIONS.iter().map(|&r| t.records(r).len()).sum())
    }

    /// `(res, opacity, hexagons drawn)` per resolution at the current zoom.
    fn hex_layers(&self) -> Vec<(u8, f32, usize)> {
        RESOLUTIONS
            .iter()
            .map(|&r| (r, walker::hex_opacity(r, self.zoom), self.hexes.as_ref().map_or(0, |t| t.records(r).len())))
            .collect()
    }

    /// A click on overlay feature `index` of source `source`: a hexagon
    /// that is drawn (its resolution visible, its group on) opens its popup.
    fn on_feature_click(&mut self, source: &str, index: usize, lon_lat: (f64, f64), cx: &mut Context<Self>) {
        // Gated on the resolution's own opacity only (review L2): r8 still
        // draws, and still takes clicks, from legendSwitch to z12.75.
        if self.mode != Mode::Hexagons {
            return;
        }
        // The hexagon lit under the pointer is the one a click pins (the
        // engine's own hit may pick another resolution mid cross-fade).
        let hovered = self.hovered_hexagon().cloned();
        let (source, index) = hovered.as_ref().map_or((source, index), |f| (&*f.source, f.index));
        let Some(res) = hexes::res_of_source(source) else { return };
        let Some(rec) = self.hexes.as_ref().and_then(|t| t.records(res).get(index)) else { return };
        if walker::hex_opacity(res, self.zoom) <= 0.0 || !hexes::modal_shown(rec.modal(), self.legend.mask()) {
            return;
        }
        self.popup = PopupState::open_at(HexPopup::of_record(rec), lon_lat);
        self.pinned_hex = Some(gpui_maps::overlay::FeatureRef {
            source: source.into(),
            index,
            id: Some(gpui_maps::overlay::FeatureId::Number(index as f64)),
        });
        self.sync_hex_highlight(cx);
        cx.notify();
    }

    /// Plans the hex tiles for the camera, starts their fetches, pushes the
    /// resolutions whose tiles changed, and brings each resolution's paint
    /// (its cross-fade opacity, the legend) and the dots' fade up to date.
    fn drive_hexes(&mut self, cx: &mut Context<Self>) {
        let Some(tiles) = self.hexes.as_mut() else { return };
        let (camera, globe_on) = {
            let m = self.map.read(cx);
            (*m.camera(), m.globe_enabled())
        };
        let zoom = camera.zoom;
        let retry = tiles.retry_due(std::time::Instant::now());
        if self.mode == Mode::Hexagons && (self.hex_camera != Some(camera) || self.hex_landed || retry) {
            self.hex_camera = Some(camera);
            self.hex_landed = false;
            let frame = gpui_maps::render::globe::globe_frame(&camera).with_globe_enabled(globe_on);
            let globe = gpui_maps::render::globe::GlobeCover::new(&frame);
            let fetches = tiles.want(zoom, std::time::Instant::now(), &mut |tz| {
                gpui_maps::render::cover_plan::tile_cover(tz, &globe, || gpui_maps::render::cover_plan::flat_cover(&camera, tz).tiles)
            });
            for fetch in fetches {
                let source = Arc::clone(tiles.source());
                cx.spawn(async move |this, cx| {
                    let f = fetch.clone();
                    let result = cx
                        .background_executor()
                        .spawn(async move { hexes::run_fetch(source.as_ref(), &f, gpui_maps::overlay::dotgrid::fetch_native::execute) })
                        .await;
                    this.update(cx, |panel, cx| {
                        if let Some(t) = panel.hexes.as_mut() {
                            t.land(fetch.res, fetch.tile, result);
                        }
                        panel.hex_landed = true;
                        cx.notify();
                    })
                    .ok();
                })
                .detach();
            }
        }
        let mask = self.legend.mask();
        // Coalesced while loading (review L1); at once when settled.
        let now = std::time::Instant::now();
        let push_now = tiles.settled() || self.hex_pushed_at.is_none_or(|t| now.duration_since(t) >= HEX_PUSH_EVERY);
        let changed = if push_now { tiles.take_changed() } else { Vec::new() };
        if !changed.is_empty() {
            self.hex_pushed_at = Some(now);
            // A pushed resolution's indexes name other hexagons now: the
            // hover is tested again, and the popup's hexagon is no longer
            // known by index (its card keeps its numbers, unlit).
            let pushed = |f: &gpui_maps::overlay::FeatureRef| hexes::res_of_source(&f.source).is_some_and(|r| changed.contains(&r));
            if self.hex_pointer.is_some() {
                self.hex_pick_pending = true;
            }
            if self.pinned_hex.as_ref().is_some_and(pushed) {
                self.pinned_hex = None;
            }
        }
        let map = self.map.clone();
        let writes = hexes::source_writes(&changed, &self.hex_paint, zoom, mask, |r| !tiles.records(r).is_empty());
        for (res, write) in writes {
            let i = RESOLUTIONS.iter().position(|&r| r == res).expect("a drawn resolution");
            let records = tiles.records(res);
            match write {
                hexes::SourceWrite::Push(key) => {
                    // Declared first: a declaration governs the next push.
                    let pushed = map.update(cx, |view, cx| {
                        let name = hexes::source_name(res);
                        view.declare_source_paint(&name, &key.paint()).expect("the hex paint compiles");
                        if let Err(e) = hexes::with_features(records, |f, c| view.set_source_columns(&name, f, c, cx)) {
                            eprintln!("race_dots: r{res} hexagons refused: {e:?}");
                            return false;
                        }
                        // Table-only repaints from the first legend toggle.
                        view.tile_source_for_paint_updates(&name, cx);
                        true
                    });
                    if pushed {
                        self.hex_paint[i] = Some(key);
                    } else {
                        // Review N3: the engine kept the old features, so no
                        // record may answer a click on them; re-push next tick.
                        tiles.invalidate(res);
                        self.hex_paint[i] = None;
                    }
                }
                hexes::SourceWrite::Repaint(key) => {
                    let path = map.update(cx, |view, cx| view.update_source_paint(&hexes::source_name(res), &key.paint(), cx));
                    match path {
                        Ok(SourcePaintPath::TableOnly) => self.hex_paint_paths[0] += 1,
                        Ok(SourcePaintPath::Rebake) => self.hex_paint_paths[1] += 1,
                        Ok(SourcePaintPath::Replan) => self.hex_paint_paths[2] += 1,
                        Err(e) => eprintln!("race_dots: r{res} hex paint refused: {e:?}"),
                    }
                    self.hex_paint[i] = Some(key);
                }
            }
        }
        // The cross-fade: a per-frame multiplier, never a re-bake --
        // composited so the stack never dims mid-fade, and the outgoing
        // resolution held until the incoming one covers the view (each
        // resolution's readiness eased over `hexes::FADE`).
        let dt = self.hex_faded_at.map_or(hexes::FADE, |t| now.duration_since(t));
        self.hex_faded_at = Some(now);
        for (i, &res) in RESOLUTIONS.iter().enumerate() {
            let target = if tiles.covered(res) { 1.0 } else { 0.0 };
            self.hex_ready[i] = hexes::ease_toward(self.hex_ready[i], target, dt);
        }
        let ready = self.hex_ready;
        let opacities = hexes::stack_opacities(zoom, |r| RESOLUTIONS.iter().position(|&x| x == r).map_or(0.0, |i| ready[i]));
        for (i, &res) in RESOLUTIONS.iter().enumerate() {
            let name = hexes::source_name(res);
            if map.read(cx).source_line_fx(&name).opacity != opacities[i] {
                map.update(cx, |view, cx| view.set_source_opacity(&name, opacities[i], cx));
            }
        }
        if self.mode == Mode::Hexagons {
            let fade = walker::dot_opacity_in_hex_mode(zoom);
            self.map.update(cx, |m, cx| m.set_dot_opacity(LAYER, fade, cx));
        }
    }

    pub fn map(&self) -> &Entity<MapView> {
        &self.map
    }

    /// The archive's parts/cells crossover zoom (`None` without an archive).
    pub fn crossover(&self) -> Option<f64> {
        self.crossover
    }

    /// Reads the layer's stats and the zoom (the zoom range is the map's
    /// own: `MapView::set_zoom_range`), drives the hexagons, and closes the
    /// popup when a zoom starts.
    fn tick(&mut self, cx: &mut Context<Self>) {
        self.drive_hexes(cx);
        self.drive_clusters(cx);
        self.drive_states(cx);
        let key = (self.mode, self.hexagons_drawn(), (self.zoom * 10.0).round() as i64);
        if key != self.chart_key {
            self.chart_key = key;
            self.sync_chart(cx);
        }
        // A zoom, mode or legend change can hide the hovered hexagon.
        self.sync_chart_hover(cx);
        // The camera settled somewhere new under a still pointer: test
        // again (and, while it moves, drop the hover).
        if self.hex_pointer.is_some() && self.mode == Mode::Hexagons {
            let (camera, moving) = {
                let m = self.map.read(cx);
                (*m.camera(), m.camera_in_motion())
            };
            if (moving && self.hover_hex.is_some()) || (!moving && self.hex_pick_camera != Some(camera)) {
                self.hex_pick_pending = true;
                cx.notify();
            }
        }
        self.sync_hex_highlight(cx);
        let undrawn = self.lit_hex.is_some() && self.map.read(cx).highlight().is_none();
        if undrawn && self.lit_requested_at.is_none_or(|t| t.elapsed() >= HIGHLIGHT_RETRY) {
            self.request_hex_highlight(self.lit_hex.clone(), cx);
        }
        let (stats, zoom) = {
            let map = self.map.read(cx);
            (map.dot_stats(LAYER), map.camera().zoom)
        };
        if (zoom - self.zoom).abs() > 1e-4 {
            self.popup.on_zoom_start();
        }
        if stats != self.stats || (zoom - self.zoom).abs() > 1e-4 {
            self.stats = stats;
            self.zoom = zoom;
            cx.notify();
        }
    }

    fn toggle_group(&mut self, g: usize, cx: &mut Context<Self>) {
        self.legend.toggle(g);
        self.apply_mask(cx);
    }

    /// The chart follows the legend (either may change the mask) and the
    /// hexagons loaded.
    fn sync_chart(&mut self, cx: &mut Context<Self>) {
        let on = !self.legend.off;
        let selection = if on == 0xFF {
            crate::linked_chart::Selection::All
        } else {
            crate::linked_chart::Selection::Bars((0..8).filter(|g| on & (1 << g) != 0).collect())
        };
        let hex = (self.mode == Mode::Hexagons).then(|| {
            let tiles = self.hexes.as_ref()?;
            let res = *RESOLUTIONS.iter().max_by(|a, b| walker::hex_opacity(**a, self.zoom).total_cmp(&walker::hex_opacity(**b, self.zoom)))?;
            let mut totals = vec![0.0; 8];
            for rec in tiles.records(res) {
                for (g, n) in rec.counts.iter().enumerate().take(8) {
                    totals[g] += f64::from(*n);
                }
            }
            totals.iter().any(|v| *v > 0.0).then_some(totals)
        });
        let (totals, unit) = match hex.flatten() {
            Some(t) => (t, HEX_UNIT),
            None => (national_totals(), NATIONAL_UNIT),
        };
        self.chart.update(cx, |c, cx| {
            c.selection = selection;
            c.spec.unit = unit.to_string();
            c.set_totals(totals, cx);
        });
    }

    fn show_all(&mut self, cx: &mut Context<Self>) {
        self.legend.show_all();
        self.apply_mask(cx);
    }

    /// The legend's toggles to the dots' mask and (the next tick) the
    /// hexagons' paint; a popup on a hexagon just hidden closes.
    fn apply_mask(&mut self, cx: &mut Context<Self>) {
        self.sync_chart(cx);
        let mask = self.legend.mask();
        self.map.update(cx, |m, cx| m.set_dot_category_mask(LAYER, mask, cx));
        self.popup.on_mask(mask);
        // A hidden group's hexagon is no longer there to hover or light.
        self.sync_chart_hover(cx);
        self.sync_hex_highlight(cx);
        self.drive_hexes(cx);
        self.drive_clusters(cx);
        self.drive_states(cx);
        cx.notify();
    }

    /// Switches the view: hexagon mode wants the hex tiles and keeps the
    /// dots to z12 and up, fading in under the r8 hexagons; dot mode drops
    /// the hexagons and gives the dots Walker's whole range and opacity.
    pub fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        if !self.mode_available(mode) {
            return;
        }
        let changed = mode != self.mode;
        self.mode = mode;
        if changed {
            self.popup.on_mode_change(mode);
            self.over_donut = false;
            // The hover is a hexagon's: tested again on the next move or
            // render in hexagon mode, gone in the others.
            self.hover_hex = None;
            self.pinned_hex = None;
            self.hex_pick_pending = self.hex_pointer.is_some();
            self.sync_hex_highlight(cx);
        }
        match mode {
            Mode::Hexagons => {
                let fade = walker::dot_opacity_in_hex_mode(self.zoom);
                self.map.update(cx, |m, cx| {
                    m.set_dot_min_zoom(LAYER, walker::DOT_FADE_IN_HEX[0].0, cx);
                    m.set_dot_opacity(LAYER, fade, cx);
                });
                self.hex_camera = None;
            }
            Mode::Clusters => {
                if let Some(t) = self.hexes.as_mut() {
                    t.clear_wanted();
                }
                // Walker's `setLayerZoomRange('people', 12, 24)`, fading in
                // under the donuts' fade-out.
                let fade = walker::dot_opacity_in_cluster_mode(self.zoom);
                self.map.update(cx, |m, cx| {
                    m.set_dot_min_zoom(LAYER, walker::dots_min_zoom(Mode::Clusters), cx);
                    m.set_dot_opacity(LAYER, fade, cx);
                });
                self.cluster_camera = None;
            }
            Mode::Dots => {
                if let Some(t) = self.hexes.as_mut() {
                    t.clear_wanted();
                }
                self.map.update(cx, |m, cx| {
                    m.set_dot_min_zoom(LAYER, walker::dots_min_zoom(Mode::Dots), cx);
                    m.set_dot_opacity(LAYER, walker::DOT_OPACITY, cx);
                });
            }
            Mode::States => {
                if let Some(t) = self.hexes.as_mut() {
                    t.clear_wanted();
                }
                // No dots under the pies: the layer plans nothing.
                self.map.update(cx, |m, cx| m.set_dot_min_zoom(LAYER, walker::dots_min_zoom(Mode::States), cx));
            }
        }
        self.drive_hexes(cx);
        self.drive_clusters(cx);
        self.drive_states(cx);
        cx.notify();
    }

    /// Pushes the state pies and outlines when the mode, the legend, the
    /// zoom scale or the selection changed; takes them off outside States
    /// mode.
    fn drive_states(&mut self, cx: &mut Context<Self>) {
        let zoom = self.map.read(cx).camera().zoom;
        let want = (self.mode == Mode::States).then(|| (self.legend.mask(), states::quantised_scale(zoom), self.selected_state));
        if want == self.states_shown {
            return;
        }
        let outlines_changed = want.map(|w| w.2) != self.states_shown.map(|s| s.2) || want.is_some() != self.states_shown.is_some();
        self.states_shown = want;
        match want {
            None => {
                self.states_drawn.clear();
                self.state_labels.clear();
                self.map.update(cx, |view, cx| {
                    view.remove_donuts(states::PIE_LAYER, cx);
                    view.remove_source(states::OUTLINE_SOURCE, cx);
                    view.remove_source(states::LABEL_SOURCE, cx);
                });
            }
            Some((mask, scale, selected)) => {
                let pies = states::pies(mask, scale, selected);
                // Each pie's total in its centre, as every cluster donut
                // carries its count (the user: "why states don't contain
                // total number").
                let labels = states::labels_geojson(&pies);
                self.state_labels = pies
                    .order
                    .iter()
                    .map(|&i| {
                        let r = &states::rows()[i];
                        (r.fips, walker::count_label(u32::try_from(r.total()).unwrap_or(u32::MAX)))
                    })
                    .collect();
                self.states_drawn = pies.order;
                let fips = selected.map(|i| states::rows()[i].fips);
                self.map.update(cx, |view, cx| {
                    if outlines_changed {
                        match view.set_source(states::OUTLINE_SOURCE, &states::outlines_geojson(fips), cx) {
                            Ok(_) => view.set_source_below_labels(states::OUTLINE_SOURCE, true),
                            Err(e) => eprintln!("race_dots: state outlines refused: {e:?}"),
                        }
                    }
                    if pies.donuts.is_empty() {
                        view.remove_donuts(states::PIE_LAYER, cx);
                    } else {
                        view.set_donuts(states::PIE_LAYER, pies.donuts, gpui_maps::overlay::dotgrid::layer::RACE_PALETTE, cx);
                    }
                    if let Err(e) = view.set_source(states::LABEL_SOURCE, &labels, cx) {
                        eprintln!("race_dots: state labels refused: {e:?}");
                    }
                });
            }
        }
    }

    /// Selects state `i` (the camera eases to fit it) or, with `None`, the
    /// United States (back to the start view).
    pub fn select_state(&mut self, i: Option<usize>, cx: &mut Context<Self>) {
        self.selected_state = i;
        let target = match i {
            Some(i) => {
                let c = *self.map.read(cx).camera();
                states::fit(i, f64::from(c.viewport_width_px), f64::from(c.viewport_height_px))
            }
            None => walker::START,
        };
        self.last_state_target = Some(target);
        let (lon, lat, zoom) = target;
        self.map.update(cx, |view, cx| {
            view.ease_to(gpui_maps::camera_control::CameraTarget { center: Some((lon, lat)), zoom: Some(zoom), ..Default::default() }, STATE_EASE);
            cx.notify();
        });
        self.drive_states(cx);
        cx.notify();
    }

    /// The pies' total labels given to the map now: `(fips, text)`.
    pub fn state_labels(&self) -> &[(u8, String)] {
        &self.state_labels
    }

    /// State pies given to the map now.
    pub fn state_pies_drawn(&self) -> usize {
        self.states_drawn.len()
    }

    /// The selected state, `None` for the United States.
    pub fn selected_state(&self) -> Option<usize> {
        self.selected_state
    }

    /// The chart's order now.
    pub fn sort(&self) -> SortKey {
        self.sort
    }

    fn on_key(&mut self, event: &gpui::KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.key == "escape" && !self.popup.is_closed() {
            self.popup.close();
            cx.notify();
            return;
        }
        let map = self.map.clone();
        self.keys.on_key(&map, event, window, cx);
    }

    /// The status line now.
    pub fn status(&self) -> &'static str {
        let ppd = self.stats.filter(|s| s.dots_drawn > 0).map(|s| s.ppd);
        status_line(self.mode, self.zoom, ppd)
    }

    fn legend_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = walker::GROUPS.iter().enumerate().map(|(g, group)| {
            let on = self.legend.is_on(g);
            let c = walker::color(g);
            let colour = gpui::Rgba { r: c.r, g: c.g, b: c.b, a: 1.0 };
            // The swatch keeps its size and border either way: only its fill
            // and the text colour say "hidden".
            let swatch = div().flex_shrink_0().size(px(11.0)).rounded_full().border_2().border_color(colour);
            let swatch = if on { swatch.bg(colour) } else { swatch };
            div()
                .id(SharedString::from(format!("race-legend-{g}")))
                .debug_selector(move || format!("race-legend-{g}"))
                .h(px(LEGEND_ROW_H))
                .flex()
                .items_center()
                .gap(px(8.0))
                .px(px(4.0))
                .rounded(px(4.0))
                .cursor_pointer()
                .hover(|s| s.bg(crate::popup::ink_a(0x0000_000d)))
                .text_color(if on { crate::popup::ink(0x1f2937) } else { crate::popup::ink(0x9ca3af) })
                .on_click(cx.listener(move |this, _: &gpui::ClickEvent, _, cx| this.toggle_group(g, cx)))
                .child(swatch)
                .child(div().flex_1().min_w(px(0.0)).truncate().child(group.label))
                .child(numeric(NUM_W).text_color(crate::popup::ink(0x6b7280)).child(people_text(group.people as f64)))
        });
        // Hexagon mode's tie key, until the legend hands over to the dots:
        // its slot is always there (transparent when not shown), so the
        // card keeps its height across modes and zooms.
        let show_tie = self.mode == Mode::Hexagons && !dots_active(self.mode, self.zoom);
        let tie = {
            let c = walker::HEX_TIE_COLOR;
            let colour = gpui::Rgba { r: c.r, g: c.g, b: c.b, a: 1.0 };
            div()
                .id("race-legend-tie")
                .h(px(LEGEND_ROW_H))
                .flex()
                .items_center()
                .gap(px(8.0))
                .px(px(4.0))
                .text_color(crate::popup::ink(0x1f2937))
                .opacity(if show_tie { 1.0 } else { 0.0 })
                .child(div().flex_shrink_0().size(px(11.0)).border_1().border_color(crate::popup::ink(0xb0b4ba)).bg(colour))
                .child(div().flex_1().truncate().child(walker::HEX_TIE_LABEL))
        };
        // "Show all groups": its slot is always there; with every group on
        // it is transparent and takes no click.
        let some_hidden = self.legend.mask() != 0xFF;
        let mut all = div()
            .id("race-legend-all")
            .h(px(16.0))
            .text_size(px(11.5))
            .text_color(crate::popup::ink(0x1d4ed8))
            .opacity(if some_hidden { 1.0 } else { 0.0 })
            .child("Show all groups");
        if some_hidden {
            all = all.cursor_pointer().on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| this.show_all(cx)));
        }
        div()
            .id("race-legend")
            .debug_selector(|| "race-legend".into())
            .flex_shrink_0()
            .w(px(LEFT_W))
            .p(px(10.0))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .bg(crate::popup::ink_a(0xffff_fff0))
            .rounded(px(6.0))
            .shadow_md()
            .occlude()
            .text_size(px(12.5))
            .child(
                div()
                    .text_size(px(14.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(crate::popup::ink(0x1f2937))
                    .pb(px(4.0))
                    .child("Race and ethnicity, 2020"),
            )
            .children(rows)
            .child(tie)
            .child(
                div()
                    .pt(px(4.0))
                    .text_color(crate::popup::ink(0x6b7280))
                    .text_size(px(11.5))
                    .child(format!("{} people. Click a group to hide or show it.", thousands(walker::US_TOTAL))),
            )
            .child(all)
    }

    fn switcher_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let radios = Mode::ALL.iter().map(|&mode| {
            let selected = mode == self.mode;
            let enabled = self.mode_available(mode);
            // The inner dot sits in a fixed 12 px ring, and the label never
            // changes weight: selecting a mode moves nothing.
            let dot = div()
                .flex_shrink_0()
                .size(px(12.0))
                .rounded_full()
                .border_1()
                .border_color(if enabled { crate::popup::ink(0x374151) } else { crate::popup::ink(0xc4c8cf) })
                .flex()
                .items_center()
                .justify_center()
                .child(div().size(px(6.0)).rounded_full().bg(crate::popup::ink(0x1f2937)).opacity(if selected { 1.0 } else { 0.0 }));
            let mut row = div()
                .id(SharedString::from(format!("race-mode-{}", mode.label())))
                .flex()
                .items_center()
                .gap(px(5.0))
                .text_color(if enabled { crate::popup::ink(0x1f2937) } else { crate::popup::ink(0xa1a6ae) })
                .child(dot)
                .child(mode.label());
            if enabled {
                row = row.cursor_pointer().on_click(cx.listener(move |this, _: &gpui::ClickEvent, _, cx| this.set_mode(mode, cx)));
            }
            row
        });
        // Every line has a fixed height (and is cut, not wrapped, past it):
        // the HUD's counts, the status and the greyed-mode reasons change
        // length as the map moves and the modes switch, the card does not.
        div()
            .id("race-switcher")
            .debug_selector(|| "race-switcher".into())
            .absolute()
            .top(px(60.0)) // one 12 px inset below the compass (12 + 36 + 12)
            .right(px(12.0))
            .w(px(SWITCHER_W))
            .p(px(10.0))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .bg(crate::popup::ink_a(0xffff_fff0))
            .rounded(px(6.0))
            .shadow_md()
            .occlude()
            .text_size(px(13.0))
            .child(div().h(px(18.0)).flex().items_center().gap(px(14.0)).children(radios))
            .child(div().h(px(14.0)).text_size(px(11.0)).text_color(crate::popup::ink(0x9ca3af)).truncate().child(self.unavailable_lines().join("; ")))
            .child(
                div()
                    .h(px(34.0))
                    .line_height(px(17.0))
                    .overflow_hidden()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(crate::popup::ink(0x1f2937))
                    .child(self.status()),
            )
            .child(div().h(px(30.0)).line_height(px(15.0)).overflow_hidden().text_size(px(11.5)).text_color(crate::popup::ink(0x6b7280)).child(self.hud()))
            .child(div().h(px(14.0)).text_size(px(11.0)).text_color(crate::popup::ink(0x9ca3af)).child(self.zoom_line()))
    }

    /// The legend's last line: the zoom, and while dots are drawn, that a
    /// dot has no details of its own to hover (hexagons, donuts and states
    /// do).
    pub fn zoom_line(&self) -> String {
        if dots_active(self.mode, self.zoom) {
            format!("z{:.2} · {DOTS_AGGREGATED}", self.zoom)
        } else {
            format!("z{:.2}", self.zoom)
        }
    }

    /// States mode's chart, under the legend: the selection's groups (the
    /// United States, or the state clicked) as share rows, the sort chips,
    /// and a 100% stacked bar per state. Every part has a fixed size; the
    /// list scrolls.
    fn states_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mask = self.legend.mask();
        let rows = states::rows();
        let (counts, total) = match self.selected_state {
            Some(i) => (states::counts64(&rows[i]), rows[i].total()),
            None => (states::us_counts(), walker::US_TOTAL),
        };
        let shares = states::shares(&counts, mask);
        // Breadcrumb: "United States" goes back once a state is selected.
        let mut us = div()
            .id("race-states-us")
            .debug_selector(|| "race-states-us".into())
            .flex_shrink_0()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(if self.selected_state.is_some() { crate::popup::ink(0x1d4ed8) } else { crate::popup::ink(0x1f2937) })
            .child("United States");
        if self.selected_state.is_some() {
            us = us.cursor_pointer().on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| this.select_state(None, cx)));
        }
        let crumb = div()
            .h(px(18.0))
            .flex()
            .items_center()
            .gap(px(4.0))
            .text_size(px(13.0))
            .child(us)
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(crate::popup::ink(0x1f2937))
                    .child(self.selected_state.map_or(String::new(), |i| format!("› {}", rows[i].name))),
            );
        let shown = (0..8).filter(|&g| mask & (1 << g) != 0).count();
        let sub = div()
            .h(px(14.0))
            .text_size(px(11.0))
            .text_color(crate::popup::ink(0x6b7280))
            .truncate()
            .child(format!("{} people · shares of the {shown} groups shown", thousands(total)));
        let group_rows = walker::GROUPS.iter().enumerate().map(|(g, group)| {
            let on = mask & (1 << g) != 0;
            let sorting = self.sort == SortKey::Share(g);
            share_row(group.label, on.then_some(shares[g]), walker::color(g))
                .id(SharedString::from(format!("race-states-group-{g}")))
                .debug_selector(move || format!("race-states-group-{g}"))
                .rounded(px(3.0))
                .px(px(2.0))
                .when(sorting, |r| r.bg(crate::popup::ink_a(0x1d4e_d814)))
                .cursor_pointer()
                .hover(|s| s.bg(crate::popup::ink_a(0x0000_000d)))
                .on_click(cx.listener(move |this, _: &gpui::ClickEvent, _, cx| {
                    this.share_group = g;
                    this.sort = SortKey::Share(g);
                    cx.notify();
                }))
        });
        let chip = |id: &'static str, label: String, w: f32, key: SortKey| {
            let on = self.sort == key;
            div()
                .id(id)
                .debug_selector(move || id.to_string())
                .flex_shrink_0()
                .w(px(w))
                .h(px(20.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(10.0))
                .border_1()
                .border_color(if on { crate::popup::ink(0x1f2937) } else { crate::popup::ink(0xc4c8cf) })
                .bg(if on { crate::popup::ink(0x1f2937) } else { crate::popup::ink(0xffffff) })
                .text_color(if on { crate::popup::ink(0xffffff) } else { crate::popup::ink(0x374151) })
                .text_size(px(11.0))
                .overflow_hidden()
                .whitespace_nowrap()
                .cursor_pointer()
                .on_click(cx.listener(move |this, _: &gpui::ClickEvent, _, cx| {
                    this.sort = key;
                    cx.notify();
                }))
                .child(label)
        };
        let chips = div()
            .h(px(20.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(div().flex_shrink_0().text_size(px(11.0)).text_color(crate::popup::ink(0x6b7280)).child("Sort"))
            .child(chip("race-states-sort-name", "Name".into(), CHIP_W[0], SortKey::Name))
            .child(chip("race-states-sort-pop", "Population".into(), CHIP_W[1], SortKey::Population))
            .child(chip("race-states-sort-share", format!("Share: {}", SHORT_LABELS[self.share_group]), CHIP_W[2], SortKey::Share(self.share_group)));
        let order = states::sorted(self.sort, mask);
        let bars = order.into_iter().map(|i| {
            let r = &rows[i];
            let s = states::shares(&states::counts64(r), mask);
            let segments = (0..8).filter(|&g| s[g] > 0.0).map(|g| {
                let c = walker::color(g);
                div().flex_shrink_0().h_full().w(gpui::relative(s[g] / 100.0)).bg(gpui::Rgba { r: c.r, g: c.g, b: c.b, a: 1.0 })
            });
            let selected = self.selected_state == Some(i);
            div()
                .id(SharedString::from(format!("race-states-row-{}", r.abbr)))
                .debug_selector(move || format!("race-states-row-{i}"))
                .flex_shrink_0()
                .h(px(STATE_ROW_H))
                .flex()
                .items_center()
                .gap(px(6.0))
                .px(px(2.0))
                .rounded(px(3.0))
                .when(selected, |d| d.bg(crate::popup::ink_a(0x1d4e_d822)))
                .cursor_pointer()
                .hover(|d| d.bg(crate::popup::ink_a(0x0000_000d)))
                .on_click(cx.listener(move |this, _: &gpui::ClickEvent, _, cx| this.select_state(Some(i), cx)))
                .child(div().flex_shrink_0().w(px(22.0)).text_color(crate::popup::ink(0x374151)).child(r.abbr))
                .child(div().flex_1().h(px(9.0)).flex().overflow_hidden().rounded(px(2.0)).bg(crate::popup::ink_a(0x0000_0012)).children(segments))
                .child(numeric(NUM_W).text_color(crate::popup::ink(0x6b7280)).child(people_text(r.total() as f64)))
        });
        div()
            .id("race-states")
            .debug_selector(|| "race-states".into())
            .w(px(LEFT_W))
            .flex_1()
            .min_h(px(0.0))
            .p(px(10.0))
            .flex()
            .flex_col()
            .gap(px(3.0))
            .bg(crate::popup::ink_a(0xffff_fff0))
            .rounded(px(6.0))
            .shadow_md()
            .occlude()
            .text_size(px(11.5))
            .child(crumb)
            .child(sub)
            .children(group_rows)
            .child(div().h(px(4.0)))
            .child(chips)
            .child(div().id("race-states-list").flex_1().min_h(px(0.0)).overflow_y_scroll().flex().flex_col().children(bars))
    }

    /// The HUD line: the hexagons' while they are the active layer, the
    /// dots' otherwise.
    pub fn hud(&self) -> String {
        if self.mode == Mode::States {
            return states_hud_text(self.selected_state, self.states_drawn.len());
        }
        if self.mode == Mode::Hexagons && !dots_active(self.mode, self.zoom) {
            let (loading, errors) = self.hexes.as_ref().map_or((false, 0), |t| (!t.settled(), t.errors));
            return hex_hud_text(&self.hex_layers(), loading, errors, self.hex_note.as_deref());
        }
        if self.mode == Mode::Clusters && !dots_active(self.mode, self.zoom) {
            let (loading, errors, refused) = self.clusters.as_ref().map_or((false, 0, 0), |t| (!t.settled(), t.errors, t.refused));
            let hud = ClusterHud {
                donuts: self.cluster_drawn.len(),
                truncated: self.cluster_truncated,
                tile_zoom: clusters::tile_zoom(self.zoom),
                loading,
                errors,
                refused,
            };
            return cluster_hud_text(hud, self.cluster_note.as_deref());
        }
        hud_text(self.stats.as_ref(), self.note.as_deref())
    }

    /// The hexagon popup (Walker's): the total, a tie note, and the 8
    /// groups' shares with a bar each, beside its hexagon's clicked ground
    /// point, re-projected on every render so it follows the map; hidden
    /// while that point is off screen. The map fills the panel from its
    /// origin, so the map's pixels are the panel's -- inside the gallery
    /// too, whatever the panel's place in the window.
    fn popup_card(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let (popup, lon_lat) = self.popup.shown()?;
        const W: f32 = 270.0;
        const H: f32 = 250.0;
        let (camera, globe_on) = {
            let m = self.map.read(cx);
            (*m.camera(), m.globe_enabled())
        };
        let (x, y) = popup_anchor(&camera, globe_on, lon_lat)?;
        let (vw, vh) = (camera.viewport_width_px, camera.viewport_height_px);
        let left = (x + 14.0).min(vw - W - 8.0).max(8.0);
        let top = (y + 14.0).min(vh - H - 8.0).max(8.0);
        let rows = popup.rows.iter().map(|&(label, pct, c)| share_row(label, Some(pct), c));
        let close = div()
            .id("race-popup-close")
            .cursor_pointer()
            .text_color(crate::popup::ink(0x6b7280))
            .on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                this.popup.close();
                cx.notify();
            }))
            .child("×");
        Some(
            div()
                .id("race-hex-popup")
                .debug_selector(|| "race-hex-popup".to_string())
                .absolute()
                .left(px(left))
                .top(px(top))
                .w(px(W))
                .p(px(10.0))
                .flex()
                .flex_col()
                .gap(px(3.0))
                .bg(crate::popup::ink_a(0xffff_fffa))
                .rounded(px(6.0))
                .shadow_md()
                .occlude()
                .text_size(px(11.5))
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_size(px(13.0))
                        .text_color(crate::popup::ink(0x1f2937))
                        .child(popup.title())
                        .child(close),
                )
                .children(popup.tie.then(|| div().text_color(crate::popup::ink(0x6b7280)).child("No single largest group (tie)")))
                .children(rows),
        )
    }

    /// The hovered hexagon's readout: the click popup's numbers (the total,
    /// a tie note, the 8 shares), beside the pointer and following it,
    /// placed as every map page's hover card is (`popup::place`: flipped
    /// off the edges, never under the pointer). It takes no pointer events
    /// (no `occlude`), so it never steals the hover it shows.
    fn hex_hover_card(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        const W: f32 = 270.0;
        const H: f32 = 236.0;
        if self.mode != Mode::Hexagons {
            return None;
        }
        let f = self.hovered_hexagon()?;
        let res = hexes::res_of_source(&f.source)?;
        let rec = self.hexes.as_ref()?.records(res).get(f.index)?;
        let at = self.hex_pointer?;
        let popup = HexPopup::of_record(rec);
        let camera = *self.map.read(cx).camera();
        let (left, top) = crate::popup::place(at, (W, H), (camera.viewport_width_px, camera.viewport_height_px));
        let rows = popup.rows.iter().map(|&(label, pct, c)| share_row(label, Some(pct), c));
        Some(
            div()
                .id("race-hex-hover")
                .debug_selector(|| "race-hex-hover".to_string())
                .absolute()
                .left(px(left))
                .top(px(top))
                .w(px(W))
                .p(px(10.0))
                .flex()
                .flex_col()
                .gap(px(3.0))
                .bg(crate::popup::ink_a(0xffff_fffa))
                .rounded(px(6.0))
                .shadow_md()
                .text_size(px(11.5))
                .child(div().font_weight(FontWeight::SEMIBOLD).text_size(px(13.0)).text_color(crate::popup::ink(0x1f2937)).child(popup.title()))
                .children(popup.tie.then(|| div().text_color(crate::popup::ink(0x6b7280)).child("No single largest group (tie)")))
                .children(rows),
        )
    }

    /// The hovered hexagon's readout title, when one shows (tests).
    pub fn hex_hover_title(&self) -> Option<String> {
        let f = self.hovered_hexagon()?;
        let rec = self.hexes.as_ref()?.records(hexes::res_of_source(&f.source)?).get(f.index)?;
        (self.mode == Mode::Hexagons && self.hex_pointer.is_some()).then(|| HexPopup::of_record(rec).title())
    }

    fn about_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let header = div()
            .id("race-about-toggle")
            .flex()
            .justify_between()
            .cursor_pointer()
            .font_weight(FontWeight::SEMIBOLD)
            .text_size(px(13.0))
            .text_color(crate::popup::ink(0x1f2937))
            .on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                this.about_open = !this.about_open;
                cx.notify();
            }))
            .child("About this map")
            .child(if self.about_open { "▾" } else { "▸" });
        // Bottom-right, and ABOVE the engine's SWAT mark's band (12..30 px up
        // from the map's bottom edge -- `BrandMark::footprint`), so on a map
        // narrow enough for the two to share columns it still cannot cover
        // it: `the_about_card_keeps_clear_of_the_brand_mark` pins it.
        let mut card = div()
            .id("race-about")
            .debug_selector(|| "race-about".into())
            .absolute()
            .bottom(px(34.0))
            .right(px(12.0))
            .w(px(360.0))
            .p(px(10.0))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .bg(crate::popup::ink_a(0xffff_fff0))
            .rounded(px(6.0))
            .shadow_md()
            .occlude()
            .text_size(px(11.5))
            .text_color(crate::popup::ink(0x374151))
            .child(header);
        if self.about_open {
            card = card
                .children(about::ABOUT.iter().map(|p| div().child(*p)))
                .child(
                    div()
                        .id("race-about-walker")
                        .cursor_pointer()
                        .text_color(crate::popup::ink(0x1d4ed8))
                        .on_click(|_: &gpui::ClickEvent, _, cx: &mut App| cx.open_url(about::WALKER_URL))
                        .child(about::INSPIRED_BY),
                )
                .child(div().text_color(crate::popup::ink(0x6b7280)).child(format!("Basemap: {}.", self.basemap_credit(cx))))
                .children(cluster_credit_line(self.clusters.as_ref().map(ClusterTiles::attribution)).map(|line| div().text_color(crate::popup::ink(0x6b7280)).child(line)));
        }
        card
    }

    /// The basemap the map draws NOW, from its sources' own credits -- a
    /// host can swap the basemap or turn imagery on under the panel (the
    /// gallery's map options), so the panel's opening pick is not enough.
    fn basemap_credit(&self, cx: &App) -> String {
        let map = self.map.read(cx);
        basemap_line(map.tile_source().attribution(), map.raster_source().and_then(|r| r.attribution()))
    }
}

/// The left column's width: the legend and the states chart.
pub const LEFT_W: f32 = 300.0;
/// The view switcher's width (fixed: its HUD text changes as the map moves).
pub const SWITCHER_W: f32 = 360.0;
/// A legend row's height.
const LEGEND_ROW_H: f32 = 19.0;
/// A states chart row's height.
const STATE_ROW_H: f32 = 14.0;
/// A people-count column's width ("331.4M", "39.5M", "577k").
const NUM_W: f32 = 52.0;
/// A share row's percentage column and bar widths.
const PCT_W: f32 = 46.0;
const BAR_W: f32 = 80.0;
/// The sort chips' widths: Name, Population, Share.
const CHIP_W: [f32; 3] = [46.0, 70.0, 124.0];
/// The share sort chip's group names.
pub const SHORT_LABELS: [&str; 8] = ["Hispanic", "White", "Black", "AIAN", "Asian", "NHPI", "Other", "Two or more"];

/// A fixed-width, right-aligned numeric column: a count or a percentage
/// changing its digits never moves what is beside it.
fn numeric(w: f32) -> gpui::Div {
    div().flex_shrink_0().w(px(w)).flex().justify_end()
}

/// One group's share as the popup and the states chart show it: label,
/// percentage (fixed width) and bar (fixed width). `None`: the group is
/// hidden in the legend -- greyed, "hidden", no bar.
fn share_row(label: &'static str, pct: Option<f32>, c: Rgba) -> gpui::Div {
    let colour = gpui::Rgba { r: c.r, g: c.g, b: c.b, a: 1.0 };
    let p = pct.unwrap_or(0.0);
    div()
        .h(px(15.0))
        .flex()
        .items_center()
        .gap(px(6.0))
        .child(div().flex_1().min_w(px(0.0)).truncate().text_color(if pct.is_some() { crate::popup::ink(0x374151) } else { crate::popup::ink(0xa1a6ae) }).child(label))
        .child(
            numeric(PCT_W)
                .text_color(if pct.is_some() { crate::popup::ink(0x1f2937) } else { crate::popup::ink(0xa1a6ae) })
                .child(pct.map_or_else(|| "hidden".to_string(), walker::percent_label)),
        )
        .child(
            div()
                .flex_shrink_0()
                .w(px(BAR_W))
                .h(px(8.0))
                .rounded(px(2.0))
                .bg(crate::popup::ink_a(0x0000_0012))
                .child(div().h_full().rounded(px(2.0)).w(px(BAR_W * (p / 100.0).clamp(0.0, 1.0))).bg(colour)),
        )
}

/// The About card's "Basemap:" text from the vector source's credit and,
/// with imagery on, the imagery's: tags stripped (`kit::strip_tags`), the
/// entity and Mapbox's "Improve this map" link text tidied away.
fn basemap_line(vector: Option<&str>, imagery: Option<&str>) -> String {
    let tidy = |html: &str| {
        crate::kit::strip_tags(html).replace("&copy;", "©").replace("Improve this map", "").split_whitespace().collect::<Vec<_>>().join(" ")
    };
    match (vector.map(tidy), imagery.map(tidy)) {
        (Some(v), Some(i)) => format!("{v}; imagery {i}"),
        (Some(v), None) => v,
        (None, Some(i)) => format!("imagery {i}"),
        (None, None) => "none".to_string(),
    }
}

/// The cluster archive's own credit, as the About card's line: its
/// metadata `attribution` (final review #7: read, then dropped, before).
/// The engine's attribution line credits the dot archive only, so a
/// cluster credit is shown here. `None` without clusters or a credit.
fn cluster_credit_line(credit: Option<&str>) -> Option<String> {
    credit.filter(|c| !c.trim().is_empty()).map(|c| format!("Clusters: {c}."))
}

impl RaceMapPanel {
    /// The map, in a box pinned to the panel's top-left corner (review H1:
    /// an absolute child with no insets takes its block-layout static
    /// position, after the in-flow map). The box's first child records its
    /// origin -- the map's -- for `donut_at`; its capture-phase press arms
    /// the donut click only when the map itself is under the pointer
    /// (review M2): the cards `.occlude()`, so a press on one leaves this
    /// box un-hovered, exactly as the engine's hexagon hit is gated.
    fn map_area(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("race-map-area")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .capture_any_mouse_down(cx.listener(|this, e: &gpui::MouseDownEvent, _, _| {
                let at = (f32::from(e.position.x), f32::from(e.position.y));
                this.donut_click.press(at);
                this.donut_click.hit(());
            }))
            // The pointing hand over a donut: asked in the map's own
            // pixels, as the click is; off the map (a card, outside the
            // panel) no donut is under the pointer.
            .on_mouse_move(cx.listener(|this, e: &gpui::MouseMoveEvent, _, cx| {
                this.on_map_move((f32::from(e.position.x), f32::from(e.position.y)), cx);
            }))
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !*hovered && this.over_donut {
                    this.over_donut = false;
                    cx.notify();
                }
                if !*hovered {
                    this.on_map_leave(cx);
                }
            }))
            .when(self.pointer(), |area| area.cursor_pointer())
            .child(origin_probe(self.origin.clone()))
            .child(self.map_child())
    }

    /// The map. In a test window a stand-in laid out and hit-tested as
    /// `MapView`'s element is (in flow, 100% x 100%, a `Normal` hitbox):
    /// a test window has no platform layer for the real one.
    fn map_child(&self) -> gpui::AnyElement {
        #[cfg(not(test))]
        return self.map.clone().into_any_element();
        #[cfg(test)]
        return div().size_full().on_mouse_down(gpui::MouseButton::Left, |_, _, _| {}).into_any_element();
    }
}

impl Render for RaceMapPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The cards in the app's theme, as the shared chart and popup.
        crate::popup::sync_ink(cx);
        // The keyboard goes to the map ONCE, on the first render that has
        // its handle (review F6): refocusing on every render would steal it
        // from any sibling input in the gallery, as `tick` re-renders at
        // up to 30 Hz.
        if !self.focused_once {
            if let Some(handle) = self.map.read(cx).focus_handle() {
                window.focus(&handle, cx);
                self.focused_once = true;
            }
        }
        // A story preview moves the camera before the map renders.
        let preview = crate::shell::preview_layer(&self.map, window, cx);
        let legend = self.legend_card(cx);
        let states_card = (self.mode == Mode::States).then(|| self.states_card(cx));
        let switcher = self.switcher_card(cx);
        let about = self.about_card(cx);
        // The hexagon hover's hit test: at most once per frame, at the
        // latest pointer.
        self.resolve_hex_hover(cx);
        let hover_card = self.hex_hover_card(cx);
        // The pinned popup steps aside while a hover card shows.
        let popup = if hover_card.is_some() { None } else { self.popup_card(cx) };
        // NO background on the root: the map draws into its own
        // CAMetalLayer BEHIND gpui's window content, so an opaque root
        // covers it completely (the user saw no map and no dots; headless
        // captures and draw counters cannot see it). Pinned by
        // `the_root_paints_no_background_over_the_map`.
        let chart = self.chart.clone();
        div()
            .id("race-dots-root")
            .size_full()
            .relative()
            .on_key_down(cx.listener(Self::on_key))
            // Click-on-release for hexagons (review R1): capture phase, so
            // the map's own drag handling cannot hide either edge. Window
            // coordinates are fine: only the press-release distance counts.
            .capture_any_mouse_down(cx.listener(|this, e: &gpui::MouseDownEvent, _, _| {
                let at = (f32::from(e.position.x), f32::from(e.position.y));
                this.click.press(at);
                // A press anywhere drops an armed donut click; only a press
                // on the map (`map_area`, below) arms it again.
                this.donut_click.press(at);
            }))
            .capture_any_mouse_up(cx.listener(|this, e: &gpui::MouseUpEvent, _, cx| {
                let at = (f32::from(e.position.x), f32::from(e.position.y));
                if let Some((source, index, lon_lat)) = this.click.release(at) {
                    this.on_feature_click(&source, index, lon_lat, cx);
                }
                if this.donut_click.release(at).is_some() {
                    this.on_donut_click(at, cx);
                }
            }))
            .child(self.map_area(cx))
            .child(
                // The left column: the legend, and under it (States mode)
                // the states chart, down to the About card's bottom line:
                // `the_left_column_keeps_clear_of_the_brand_mark` pins it.
                div()
                    .id("race-left")
                    .debug_selector(|| "race-left".into())
                    .absolute()
                    .top(px(12.0))
                    .left(px(12.0))
                    .bottom(px(34.0))
                    .w(px(LEFT_W))
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(legend)
                    .children(states_card)
                    // Hexagon mode's own distribution (in the other modes
                    // the legend already lists the national totals).
                    .when(self.mode == Mode::Hexagons, |c| c.child(chart)),
            )
            .child(switcher)
            .child(about)
            .children(popup)
            .children(hover_card)
            .children(preview)
    }
}

/// The groups' chart: people per group -- in the hexagons loaded, in
/// hexagon mode; the 2020 national totals otherwise. A click keeps the
/// chosen groups (the legend's own mask: dots, hexagons, donuts and pies);
/// a hover lights the hexagons whose largest group it is.
fn group_chart(map: &Entity<MapView>, panel: gpui::WeakEntity<RaceMapPanel>, cx: &mut Context<RaceMapPanel>) -> Entity<crate::linked_chart::LinkedChart> {
    use crate::linked_chart::{Category, ChartSpec, Kind, Link, LinkedChart, Selection};
    let categories: Vec<Category> = walker::GROUPS
        .iter()
        .enumerate()
        .map(|(g, group)| {
            let c = walker::color(g);
            let rgb = ((c.r * 255.0).round() as u32) << 16 | ((c.g * 255.0).round() as u32) << 8 | (c.b * 255.0).round() as u32;
            Category::new(gpui_maps::overlay::TagValue::String(format!("g{g}")), short_label(group.label)).color(rgb)
        })
        .collect();
    let apply = move |selection: &Selection, cx: &mut App| {
        let on: u8 = match selection {
            Selection::Bars(set) => set.iter().fold(0u8, |m, &g| m | (1 << g)),
            _ => 0xFF,
        };
        // Deferred: the chart is mid-update here, and the mask change
        // updates the chart back (`sync_chart`).
        let panel = panel.clone();
        cx.defer(move |cx| {
            let _ = panel.update(cx, |p, cx| {
                p.legend.off = !on;
                p.apply_mask(cx);
            });
        });
    };
    let spec = ChartSpec {
        title: "People by group".into(),
        unit: NATIONAL_UNIT.into(),
        noun: "people".into(),
        fmt: crate::popup::NumFmt::Compact,
        hint: Some("Click bars to keep their groups.".into()),
    };
    let sources: Vec<String> = RESOLUTIONS.iter().map(|&r| hexes::source_name(r)).collect();
    cx.new(|cx| {
        let mut c = LinkedChart::with_rows(map, &sources[0], hexes::MODAL_KEY, Kind::Bars { categories }, spec, Link::Callback(std::rc::Rc::new(apply)), Vec::new(), cx);
        c.also_light(&sources[1..]);
        c.docked = true;
        c.set_totals(national_totals(), cx);
        c
    })
}

const NATIONAL_UNIT: &str = "2020 Census, United States (P.L. 94-171).";
const HEX_UNIT: &str = "In the hexagons loaded (the most visible resolution); a hover lights the hexagons it leads.";

fn national_totals() -> Vec<f64> {
    walker::GROUPS.iter().map(|g| g.people as f64).collect()
}

/// A legend label short enough for a bar row.
fn short_label(label: &str) -> &str {
    match label {
        "Hispanic or Latino (any race)" => "Hispanic / Latino",
        "American Indian / Alaska Native" => "Am. Indian / AK Native",
        "Native Hawaiian / Pacific Islander" => "NH / Pacific Islander",
        other => other,
    }
}

/// An empty, unpainted element pinned over the map that records the
/// map's window origin at each prepaint (a click arrives in window
/// pixels). Pinned with explicit insets: see `RaceMapPanel::map_area`.
fn origin_probe(origin: std::rc::Rc<std::cell::Cell<(f32, f32)>>) -> impl IntoElement {
    gpui::canvas(
        move |bounds, _, _| origin.set((f32::from(bounds.origin.x), f32::from(bounds.origin.y))),
        |_, _, _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The card's basemap line follows the live sources: the vector
    /// source's credit, tags stripped, plus the imagery's when a host turned
    /// imagery on (the gallery's map options).
    #[test]
    fn the_basemap_line_reads_the_live_sources_credit() {
        let ofm = concat!(
            r#"<a href="https://openfreemap.org">OpenFreeMap</a> "#,
            r#"<a href="https://www.openmaptiles.org/">&copy; OpenMapTiles</a> "#,
            r#"Data from <a href="https://www.openstreetmap.org/copyright">OpenStreetMap</a>"#,
        );
        assert_eq!(basemap_line(Some(ofm), None), "OpenFreeMap © OpenMapTiles Data from OpenStreetMap");
        let mapbox = r#"<a href="x">&copy; Mapbox</a> <a href="y">&copy; OpenStreetMap</a> <a href="z">Improve this map</a>"#;
        assert_eq!(basemap_line(Some(mapbox), None), "© Mapbox © OpenStreetMap");
        let line = basemap_line(Some(ofm), Some(crate::kit::S2CLOUDLESS_2016_ATTRIBUTION));
        assert!(
            line.ends_with("; imagery EOxCloudless https://cloudless.eox.at by EOX IT Services GmbH (Contains modified Copernicus Sentinel data 2016 & 2017)"),
            "{line}"
        );
        assert!(!line.contains('<'), "{line}");
        assert_eq!(basemap_line(None, None), "none");
    }

    /// The map is a CAMetalLayer under gpui's content: any `.bg(` on the
    /// panel root (from its `div()` to the map child) hides it. Checked on
    /// the source, since neither a gpui test window nor a headless capture
    /// draws the map's layer.
    #[test]
    fn the_root_paints_no_background_over_the_map() {
        // From the top of `render` (review F5): a background set before the
        // id, or on a wrapper around the root, is caught too.
        let src = include_str!("panel.rs");
        let start = src.find("impl Render for RaceMapPanel").expect("the render impl");
        let end = start + src[start..].find(".child(self.map_area(cx))").expect("the map child");
        let root = &src[start..end];
        assert!(!root.contains(".bg("), "the panel root sets a background, which covers the map:\n{root}");
        // Nor the map's own box, the probe or the stand-in.
        let area = &src[src.find("fn map_area").expect("map_area")..src.find("impl Render for RaceMapPanel").expect("render")];
        assert!(!area.contains(".bg("), "the map's box sets a background:\n{area}");
    }

    #[test]
    fn status_text_follows_walker() {
        assert_eq!(status_text(Mode::Dots, 13.0), "One dot per person");
        assert_eq!(status_text(Mode::Dots, 12.9), "Zoom in to see every dot");
        assert_eq!(status_text(Mode::Hexagons, 8.0), "Hex colours show the largest group, not population density. Minimum: 10 people.");
        // Hexagon mode hands over to the dots' text at legendSwitch 12.375; "One dot per person" still needs z >= 13.
        assert_eq!(status_text(Mode::Hexagons, 12.4), "Zoom in to see every dot");
        assert_eq!(status_text(Mode::Hexagons, 13.0), "One dot per person");
        assert_eq!(status_text(Mode::Clusters, 8.0), "Click a donut to zoom in");
        assert_eq!(status_text(Mode::Clusters, 12.0), "Zoom in to see every dot"); // clusters hand over at z12
    }

    #[test]
    fn legend_toggles_build_the_mask() {
        let mut l = Legend::default();
        l.toggle(1);
        assert_eq!(l.mask(), 0b1111_1101);
    }

    #[test]
    fn the_about_text_states_the_total_and_the_caveats() {
        let t = about::ABOUT.join(" ");
        for s in ["331,449,281", "not at home addresses", "differential privacy", "U.S. Census Bureau", "Kyle Walker"] {
            assert!(t.contains(s), "{s}");
        }
    }

    #[test]
    fn the_about_text_names_the_simulation_and_the_dc_match() {
        let t = about::ABOUT.join(" ");
        for s in ["tie", "fewer than 10 people", "compressed scale", "click one to zoom", "still include everyone"] {
            assert!(t.contains(s), "{s}");
        }
        for s in ["simulated", "not a residence", "P.L. 94-171", "TIGER/Line", "identical to the DHC table P9", "noise"] {
            assert!(t.contains(s), "{s}");
        }
        assert!(about::INSPIRED_BY.contains("Kyle Walker"));
        assert!(about::WALKER_URL.starts_with("https://walker-data.com/"));
    }

    #[test]
    fn the_real_ppd_decides_the_dots_status() {
        // No claim from a fixed zoom: until dots are drawn, zoom in.
        assert_eq!(status_line(Mode::Dots, 13.2, None), "Zoom in to see every dot");
        assert_eq!(status_line(Mode::Dots, 14.0, None), "Zoom in to see every dot");
        // Past z13 but each dot still stands for 1.4 people: not yet.
        assert_eq!(status_line(Mode::Dots, 13.2, Some(1.4)), "Zoom in to see every dot");
        // Reads 1.00 at z12.9: yes.
        assert_eq!(status_line(Mode::Dots, 12.9, Some(1.0)), "One dot per person");
        // 1.004 is not one dot per person, though it prints as 1.00.
        assert_eq!(status_line(Mode::Dots, 13.5, Some(1.004)), "Zoom in to see every dot");
        // A mode whose dots are not active keeps its own text.
        assert_eq!(status_line(Mode::Clusters, 8.0, Some(1.0)), "Click a donut to zoom in");
    }

    #[test]
    fn legend_toggles_back_and_show_all() {
        let mut l = Legend::default();
        assert_eq!(l.mask(), 0xFF);
        l.toggle(7);
        l.toggle(0);
        assert_eq!(l.mask(), 0b0111_1110);
        assert!(!l.is_on(7) && l.is_on(3));
        l.toggle(7);
        assert_eq!(l.mask(), 0b1111_1110);
        l.show_all();
        assert_eq!(l.mask(), 0xFF);
    }

    /// A host window that draws the panel 900x700 at window (100, 50), as
    /// the gallery embeds it, around a real `MapView` with no basemap.
    mod window {
        use super::super::*;
        use gpui::{Modifiers, MouseButton, TestAppContext, VisualTestContext};

        pub struct Host {
            pub panel: Entity<RaceMapPanel>,
            pub at: (f32, f32),
        }

        impl Render for Host {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div().size_full().pl(px(self.at.0)).pt(px(self.at.1)).child(div().w(px(900.0)).h(px(700.0)).child(self.panel.clone()))
            }
        }

        pub fn host(cx: &mut TestAppContext) -> (Entity<Host>, &mut VisualTestContext) {
            host_at(cx, (100.0, 50.0))
        }

        /// The panel 900x700 at window `at`, as the gallery places it right
        /// of its sidebar and under its title bar.
        pub fn host_at(cx: &mut TestAppContext, at: (f32, f32)) -> (Entity<Host>, &mut VisualTestContext) {
            let (host, cx) = cx.add_window_view(|window, cx| {
                let sources = RaceSources { dots: PathBuf::from("/nonexistent/race.pmtiles"), hexes: None, clusters: None };
                let options = PanelOptions { basemap: Basemap::None, size: (900.0, 700.0), ..PanelOptions::default() };
                let panel = cx.new(|cx| {
                    let mut p = RaceMapPanel::with_options(window, cx, sources, options);
                    // No archive in a test: the mode is set directly, as
                    // `set_mode` would with one (and kept when the missing
                    // archives' notes are attached).
                    p.mode = Mode::Clusters;
                    p.pending_mode = None;
                    p
                });
                Host { panel, at }
            });
            draw(cx);
            (host, cx)
        }

        pub fn draw(cx: &mut VisualTestContext) {
            cx.update(|window, cx| {
                window.refresh();
                let _ = window.draw(cx);
            });
            cx.run_until_parked();
        }

        /// A press and release at window `(x, y)`, a frame after each.
        pub fn click(cx: &mut VisualTestContext, x: f32, y: f32) {
            let at = gpui::point(px(x), px(y));
            cx.simulate_mouse_move(at, None, Modifiers::default());
            draw(cx);
            cx.simulate_mouse_down(at, MouseButton::Left, Modifiers::default());
            draw(cx);
            cx.simulate_mouse_up(at, MouseButton::Left, Modifiers::default());
            draw(cx);
        }

        pub fn query(cx: &mut VisualTestContext, host: &Entity<Host>) -> Option<(f32, f32)> {
            let panel = host.read_with(cx, |h, _| h.panel.clone());
            panel.read_with(cx, |p, _| p.last_donut_query)
        }

        /// A pointer move to window `(x, y)`, and a frame.
        pub fn hover(cx: &mut VisualTestContext, x: f32, y: f32) {
            cx.simulate_mouse_move(gpui::point(px(x), px(y)), None, Modifiers::default());
            draw(cx);
        }

        /// One 20 px donut at the map's centre (the camera's), backed by
        /// one drawn cluster record that eases to z9, as `drive_clusters`
        /// would push it.
        pub fn put_donut_at_centre(cx: &mut VisualTestContext, host: &Entity<Host>) {
            let panel = host.read_with(cx, |h, _| h.panel.clone());
            panel.update(cx, |p, cx| {
                let (lon, lat) = {
                    let c = p.map.read(cx).camera();
                    (c.center_lon, c.center_lat)
                };
                let counts = [10, 20, 30, 0, 5, 0, 0, 35];
                p.cluster_drawn = vec![ClusterRecord { lon, lat, blocks: 40, pop: 100, counts, expansion_zoom: 9, leaf: false }];
                p.map.update(cx, |view, cx| {
                    let donut = gpui_maps::render::donut::Donut { lon, lat, radius_px: 20.0, counts, alpha: 1.0 };
                    view.set_donuts(DONUT_LAYER, vec![donut], gpui_maps::overlay::dotgrid::layer::RACE_PALETTE, cx);
                });
            });
            draw(cx);
        }
    }

    /// The About card is the race panel's one card anchored to the map's
    /// bottom edge. It sits bottom-RIGHT, 360 px wide, so it shares the SWAT
    /// mark's columns only on a map narrower than ~463 px, but it must clear
    /// the mark's band at any width: a native map draws below gpui, and a
    /// card over the mark hides it. Laid out by gpui (opened, its tallest)
    /// and measured with `debug_bounds` against `BrandMark::footprint`.
    #[test]
    fn the_about_card_keeps_clear_of_the_brand_mark() {
        let mut app = gpui::TestAppContext::single();
        let at = (220.0, 40.0);
        let (host, cx) = window::host_at(&mut app, at);
        let panel = host.read_with(cx, |h, _| h.panel.clone());
        for open in [false, true] {
            panel.update(cx, |p, cx| {
                p.about_open = open;
                cx.notify();
            });
            window::draw(cx);
            let b = cx.debug_bounds("race-about").expect("the About card is drawn");
            let (y, h) = (f32::from(b.origin.y) - at.1, f32::from(b.size.height));
            let (_, my, _, _) = gpui_maps::brand_mark::BrandMark::default().footprint(700.0);
            assert!(h > 0.0);
            assert!(y + h <= my, "About (open: {open}) reaches down to {} over the mark's top at {my}", y + h);
        }
    }

    /// The gallery's layout (the user's live test): the panel at window
    /// (220, 40), right of the sidebar and under the title bar. A click on
    /// the donut at the map's centre -- window (220 + 450, 40 + 350) --
    /// asks `donut_at` about map point (450, 350) and hits it; a click at
    /// window (450, 350), which a window-relative mapping would have taken
    /// for the centre, hits nothing; and a click on bare map is asked about
    /// at the window point less the panel's origin.
    #[test]
    fn at_the_gallery_offset_a_click_hits_the_donut_at_its_map_local_point() {
        let mut app = gpui::TestAppContext::single();
        let (host, cx) = window::host_at(&mut app, (220.0, 40.0));
        window::put_donut_at_centre(cx, &host);
        let panel = host.read_with(cx, |h, _| h.panel.clone());
        window::click(cx, 450.0, 350.0);
        assert_eq!(panel.read_with(cx, |p, _| p.last_donut_target), None, "window (450, 350) is not the map's centre");
        window::click(cx, 220.0 + 300.0, 40.0 + 550.0);
        assert_eq!(window::query(cx, &host), Some((300.0, 550.0)), "the window point, less the panel's origin");
        assert_eq!(panel.read_with(cx, |p, _| p.last_donut_target), None, "no donut there");
        window::click(cx, 220.0 + 450.0, 40.0 + 350.0);
        assert_eq!(window::query(cx, &host), Some((450.0, 350.0)));
        let (lon, lat) = panel.read_with(cx, |p, _| (p.cluster_drawn[0].lon, p.cluster_drawn[0].lat));
        assert_eq!(panel.read_with(cx, |p, _| p.last_donut_target), Some((lon, lat, 9.0)), "the donut's click eased to its expansion zoom");
    }

    /// The pointing hand over a donut, at the gallery offset: a move onto
    /// it (map-local, as the click) turns it on, a move off it or onto a
    /// card turns it off; outside cluster mode a donut is not clickable.
    #[test]
    fn at_the_gallery_offset_the_pointer_is_a_hand_over_a_donut_only() {
        let mut app = gpui::TestAppContext::single();
        let (host, cx) = window::host_at(&mut app, (220.0, 40.0));
        window::put_donut_at_centre(cx, &host);
        let panel = host.read_with(cx, |h, _| h.panel.clone());
        let pointer = |cx: &mut gpui::VisualTestContext| panel.read_with(cx, |p, _| p.pointer());
        window::hover(cx, 220.0 + 455.0, 40.0 + 345.0);
        assert_eq!(panel.read_with(cx, |p, _| p.last_donut_hover), Some((455.0, 345.0)));
        assert!(pointer(cx), "over the donut");
        window::hover(cx, 220.0 + 300.0, 40.0 + 600.0);
        assert!(!pointer(cx), "off the donut");
        window::hover(cx, 220.0 + 450.0, 40.0 + 350.0);
        assert!(pointer(cx));
        // Onto the legend card (it occludes the map): the hand goes.
        window::hover(cx, 220.0 + 150.0, 40.0 + 40.0);
        assert!(!pointer(cx), "a card is not a donut");
        window::hover(cx, 220.0 + 450.0, 40.0 + 350.0);
        assert!(pointer(cx));
        panel.update(cx, |p, _| p.mode = Mode::Dots);
        assert!(!pointer(cx), "dot mode clicks no donut");
    }

    /// A small hexagon (radius `r` degrees) round `(lon, lat)` with these
    /// counts.
    fn hex_around(res: u8, lon: f64, lat: f64, r: f64, counts: [u32; 8]) -> HexRecord {
        let boundary = (0..6)
            .map(|k| {
                let a = std::f64::consts::PI / 3.0 * f64::from(k);
                [lon + r * a.cos(), lat + r * a.sin()]
            })
            .collect();
        HexRecord { res, h3: u64::from(res) << 8 | 1, counts, boundary }
    }

    /// Hexagon mode at z8 over Houston, the panel at the gallery offset
    /// (220, 40), r6 loaded with two hexagons round map points (400, 300)
    /// (White largest, 100 people) and (500, 300) (Black largest, 50).
    fn hover_fixture(app: &mut gpui::TestAppContext) -> (Entity<RaceMapPanel>, &mut gpui::VisualTestContext) {
        let (host, cx) = window::host_at(app, (220.0, 40.0));
        let panel = host.read_with(cx, |h, _| h.panel.clone());
        panel.update(cx, |p, cx| {
            p.map.update(cx, |m, _| m.set_camera(-95.37, 29.76, 8.0));
            let ground = |p: &RaceMapPanel, x: f32, y: f32, cx: &App| {
                let m = p.map.read(cx);
                let ws = f64::from(gpui_maps::render::WORLD_SIZE);
                let w = gpui_maps::render::globe::unproject_to_world_on_globe(m.camera(), m.globe_enabled(), x, y).expect("ground");
                gpui_maps::geo::tile_xy_to_lon_lat(w.x / ws, w.y / ws, 0.0)
            };
            let a = ground(p, 400.0, 300.0, cx);
            let b = ground(p, 500.0, 300.0, cx);
            let records = vec![hex_around(6, a.0, a.1, 0.05, [5, 80, 5, 0, 5, 0, 0, 5]), hex_around(6, b.0, b.1, 0.05, [5, 10, 30, 0, 5, 0, 0, 0])];
            struct Two(Vec<HexRecord>);
            impl gpui_maps::overlay::hexes::HexSource for Two {
                fn request(&self, res: u8, tile: gpui_maps::geo::TileId) -> gpui_maps::overlay::dotgrid::GridRequest {
                    gpui_maps::overlay::dotgrid::GridRequest::Range { location: format!("{res}"), offset: u64::from(tile.x), length: 1 }
                }
                fn decode(&self, _: u8, _: gpui_maps::geo::TileId, _: &[u8]) -> Result<Vec<HexRecord>, gpui_maps::overlay::dotgrid::GridError> {
                    Ok(self.0.clone())
                }
            }
            let mut tiles = HexTiles::new(Arc::new(Two(records.clone())));
            for f in tiles.want_now(8.0, &mut |z| vec![gpui_maps::geo::TileId { z, x: 0, y: 0 }]) {
                let mine = if f.res == 6 { records.clone() } else { Vec::new() };
                tiles.land(f.res, f.tile, Ok(mine));
            }
            tiles.take_changed();
            assert_eq!(tiles.records(6).len(), 2);
            p.hexes = Some(tiles);
            // Planned for this camera: a legend toggle's `drive_hexes`
            // keeps the fixture's tiles rather than re-planning the cover.
            p.hex_camera = Some(*p.map.read(cx).camera());
            p.mode = Mode::Hexagons;
            p.zoom = 8.0;
            assert!(walker::hex_opacity(6, 8.0) > 0.0, "r6 draws at z8");
        });
        window::draw(cx);
        (panel, cx)
    }

    fn hovered(cx: &mut gpui::VisualTestContext, panel: &Entity<RaceMapPanel>) -> (Option<usize>, Option<usize>, Option<String>) {
        panel.read_with(cx, |p, _| (p.hovered_hexagon().map(|f| f.index), p.lit_hexagon().map(|f| f.index), p.hex_hover_title()))
    }

    /// The user: "on hexagon it's better to have hover interaction with
    /// hexagon highlight instead of onclick". Real pointer moves (through
    /// gpui's dispatch, at the gallery offset) over one hexagon light it
    /// and show its readout beside the pointer; onto the other, both
    /// follow; onto open ground, a card, or off the map, both clear.
    #[test]
    fn hovering_a_hexagon_lights_it_and_shows_its_numbers() {
        let mut app = gpui::TestAppContext::single();
        let (panel, cx) = hover_fixture(&mut app);
        let at = (220.0, 40.0);
        assert_eq!(hovered(cx, &panel), (None, None, None));
        assert!(cx.debug_bounds("race-hex-hover").is_none());
        window::hover(cx, at.0 + 400.0, at.1 + 300.0);
        assert_eq!(hovered(cx, &panel), (Some(0), Some(0), Some("100 people in this hexagon".into())));
        let lit = panel.read_with(cx, |p, _| p.lit_hexagon().cloned()).unwrap();
        assert_eq!((&*lit.source, lit.id.clone()), ("race-hex-r6", Some(gpui_maps::overlay::FeatureId::Number(0.0))), "the drawn feature's own id");
        let card = cx.debug_bounds("race-hex-hover").expect("the readout shows on hover");
        // Beside the pointer (popup::place: GAP right and below), in the window.
        assert!((f32::from(card.origin.x) - (at.0 + 400.0 + crate::popup::GAP)).abs() < 1.0, "{card:?}");
        assert!((f32::from(card.origin.y) - (at.1 + 300.0 + crate::popup::GAP)).abs() < 1.0, "{card:?}");
        assert!(!card.contains(&gpui::point(px(at.0 + 400.0), px(at.1 + 300.0))), "never under the pointer");
        assert!(panel.read_with(cx, |p, _| p.pointer()), "still clickable: a click pins it");
        assert_eq!(panel.read_with(cx, |p, cx| p.chart.read(cx).map_hover), Some(1), "White's bar lights");
        // Within the same hexagon the card follows the pointer.
        window::hover(cx, at.0 + 405.0, at.1 + 302.0);
        assert_eq!(hovered(cx, &panel).0, Some(0));
        let moved = cx.debug_bounds("race-hex-hover").unwrap();
        assert!((f32::from(moved.origin.x) - f32::from(card.origin.x) - 5.0).abs() < 1.0, "{moved:?}");
        // The other hexagon.
        window::hover(cx, at.0 + 500.0, at.1 + 300.0);
        assert_eq!(hovered(cx, &panel), (Some(1), Some(1), Some("50 people in this hexagon".into())));
        assert_eq!(panel.read_with(cx, |p, cx| p.chart.read(cx).map_hover), Some(2), "Black's bar lights");
        // Open ground between them.
        window::hover(cx, at.0 + 450.0, at.1 + 300.0);
        assert_eq!(hovered(cx, &panel), (None, None, None));
        assert!(cx.debug_bounds("race-hex-hover").is_none(), "the readout clears on leave");
        // Back on, then onto the legend card (it occludes the map).
        window::hover(cx, at.0 + 400.0, at.1 + 300.0);
        assert_eq!(hovered(cx, &panel).1, Some(0));
        window::hover(cx, at.0 + 60.0, at.1 + 40.0);
        assert_eq!(hovered(cx, &panel), (None, None, None), "a card is not the map");
        // Back on, then out of the panel altogether.
        window::hover(cx, at.0 + 400.0, at.1 + 300.0);
        assert_eq!(hovered(cx, &panel).1, Some(0));
        window::hover(cx, 5.0, 5.0);
        assert_eq!(hovered(cx, &panel), (None, None, None), "off the map");
    }

    /// Many pointer moves between two frames run ONE hit test, at the
    /// latest pointer -- never one per move, and the hexagon sources are
    /// never re-pushed for a hover.
    #[test]
    fn the_hover_hit_test_runs_once_a_frame_at_the_latest_pointer() {
        let mut app = gpui::TestAppContext::single();
        let (panel, cx) = hover_fixture(&mut app);
        let at = (220.0, 40.0);
        let picks = |cx: &mut gpui::VisualTestContext| panel.read_with(cx, |p, _| p.hex_picks);
        let before = picks(cx);
        // Five moves dispatched between two frames, as a fast pointer
        // delivers them (`simulate_mouse_move` draws a frame after each).
        cx.update(|window, cx| {
            for x in [350.0, 420.0, 450.0, 490.0, 500.0] {
                let event = gpui::MouseMoveEvent { position: gpui::point(px(at.0 + x), px(at.1 + 300.0)), pressed_button: None, modifiers: gpui::Modifiers::default() };
                window.dispatch_event(gpui::PlatformInput::MouseMove(event), cx);
            }
        });
        window::draw(cx);
        let ran = picks(cx) - before;
        assert_eq!(ran, 1, "five moves, one frame, one hit test");
        assert_eq!(hovered(cx, &panel).0, Some(1), "at the latest pointer");
        window::draw(cx);
        assert_eq!(picks(cx), before + ran, "a frame with no move tests nothing");
        let pushes = panel.read_with(cx, |p, cx| p.map.read(cx).source_generation(&hexes::source_name(6)));
        window::hover(cx, at.0 + 400.0, at.1 + 300.0);
        assert_eq!(panel.read_with(cx, |p, cx| p.map.read(cx).source_generation(&hexes::source_name(6))), pushes, "a hover re-pushes no source");
    }

    /// Hiding the hovered hexagon's largest group in the legend clears the
    /// hover, its light and its card; a click still pins a card, lit once
    /// the pointer leaves; a mode switch clears both.
    #[test]
    fn the_legend_click_and_mode_keep_the_hover_honest() {
        let mut app = gpui::TestAppContext::single();
        let (panel, cx) = hover_fixture(&mut app);
        let at = (220.0, 40.0);
        window::hover(cx, at.0 + 400.0, at.1 + 300.0);
        assert_eq!(hovered(cx, &panel).1, Some(0));
        panel.update(cx, |p, cx| p.toggle_group(1, cx));
        window::draw(cx);
        assert_eq!(hovered(cx, &panel), (None, None, None), "White hidden: its hexagon is gone");
        assert!(cx.debug_bounds("race-hex-hover").is_none());
        panel.update(cx, |p, cx| p.toggle_group(1, cx));
        window::hover(cx, at.0 + 401.0, at.1 + 300.0);
        assert_eq!(hovered(cx, &panel).1, Some(0), "shown again, hovered again");
        // A click pins the hovered hexagon's card (the engine's press-time
        // hit stands in for a real one: a test window has no engine).
        panel.update(cx, |p, cx| p.on_feature_click(&hexes::source_name(6), 0, (-95.0, 29.0), cx));
        assert_eq!(panel.read_with(cx, |p, _| p.pinned_hex.as_ref().map(|f| f.index)), Some(0));
        window::draw(cx);
        assert!(cx.debug_bounds("race-hex-hover").is_some() && cx.debug_bounds("race-hex-popup").is_none(), "the hover card, not both");
        window::hover(cx, at.0 + 500.0, at.1 + 300.0);
        assert_eq!(hovered(cx, &panel).1, Some(1), "the hover wins the light");
        window::hover(cx, at.0 + 450.0, at.1 + 300.0);
        assert_eq!(hovered(cx, &panel).1, Some(0), "off every hexagon: the pinned one is lit");
        assert!(cx.debug_bounds("race-hex-popup").is_some(), "its card is back");
        panel.update(cx, |p, cx| p.set_mode(Mode::Dots, cx));
        window::draw(cx);
        assert_eq!(hovered(cx, &panel), (None, None, None), "dot mode lights no hexagon");
    }

    /// The hexagon popup at the gallery offset (220, 40): placed from its
    /// anchor in the MAP's pixels (14 px right and below it), so in the
    /// window it sits at the panel's origin plus that -- and clamped to
    /// the map's own edges, not the window's.
    #[test]
    fn at_the_gallery_offset_the_hex_popup_sits_beside_its_anchor() {
        let mut app = gpui::TestAppContext::single();
        let (host, cx) = window::host_at(&mut app, (220.0, 40.0));
        let panel = host.read_with(cx, |h, _| h.panel.clone());
        // Houston at z10: every pixel of the map is on the ground (at
        // Walker's z3.5 start the corners are space around the globe).
        panel.update(cx, |p, cx| p.map.update(cx, |m, _| m.set_camera(-95.37, 29.76, 10.0)));
        let (camera, globe) = panel.read_with(cx, |p, cx| (*p.map.read(cx).camera(), p.map.read(cx).globe_enabled()));
        assert_eq!((camera.viewport_width_px, camera.viewport_height_px), (900.0, 700.0));
        let ground = |x: f32, y: f32| {
            let ws = f64::from(gpui_maps::render::WORLD_SIZE);
            let w = gpui_maps::render::globe::unproject_to_world_on_globe(&camera, globe, x, y).expect("on the ground");
            gpui_maps::geo::tile_xy_to_lon_lat(w.x / ws, w.y / ws, 0.0)
        };
        let mut place = |lon_lat: (f64, f64)| {
            panel.update(cx, |p, cx| {
                p.mode = Mode::Hexagons;
                p.popup = PopupState::open_at(hex_fixture(), lon_lat);
                cx.notify();
            });
            window::draw(cx);
            let b = cx.debug_bounds("race-hex-popup").expect("the popup is drawn");
            (f32::from(b.origin.x), f32::from(b.origin.y))
        };
        // Anchored at map (300, 200): the card's corner at map (314, 214).
        let (x, y) = place(ground(300.0, 200.0));
        assert!((x - (220.0 + 314.0)).abs() < 1.0 && (y - (40.0 + 214.0)).abs() < 1.0, "({x}, {y})");
        // Anchored near the map's bottom-right: clamped 8 px inside the
        // MAP's edges (900 - 270 - 8, 700 - 250 - 8), offset by the panel.
        let (x, y) = place(ground(880.0, 690.0));
        assert!((x - (220.0 + 622.0)).abs() < 1.0 && (y - (40.0 + 442.0)).abs() < 1.0, "({x}, {y})");
    }

    /// Serves one r6 record (White the largest group) for every tile.
    struct OneHex;
    impl gpui_maps::overlay::hexes::HexSource for OneHex {
        fn request(&self, res: u8, tile: gpui_maps::geo::TileId) -> gpui_maps::overlay::dotgrid::GridRequest {
            gpui_maps::overlay::dotgrid::GridRequest::Range { location: format!("{res}"), offset: u64::from(tile.x), length: 1 }
        }
        fn decode(&self, res: u8, _: gpui_maps::geo::TileId, _: &[u8]) -> Result<Vec<HexRecord>, gpui_maps::overlay::dotgrid::GridError> {
            Ok(vec![hex_record(res)])
        }
    }

    fn hex_record(res: u8) -> HexRecord {
        HexRecord { res, h3: 1, counts: [3, 90, 5, 0, 1, 0, 0, 1], boundary: vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.5, 1.5], [0.0, 1.0], [-0.5, 0.5]] }
    }

    /// Review L2: the positive case. A loaded r6 hexagon at z8 (r6's
    /// plateau) under the pointer is the pointing hand; hiding its largest
    /// group, zooming out of r6's window, leaving hexagon mode, or a
    /// feature index with no record behind it, is not.
    #[test]
    fn the_pointing_hand_shows_over_a_loaded_clickable_hexagon() {
        use gpui_maps::overlay::FeatureRef;
        let mut app = gpui::TestAppContext::single();
        let (host, cx) = window::host_at(&mut app, (220.0, 40.0));
        let panel = host.read_with(cx, |h, _| h.panel.clone());
        panel.update(cx, |p, cx| {
            p.map.update(cx, |m, _| m.set_camera(-95.37, 29.76, 8.0));
            let mut tiles = HexTiles::new(Arc::new(OneHex));
            let fetches = tiles.want_now(8.0, &mut |z| vec![gpui_maps::geo::TileId { z, x: 0, y: 0 }]);
            assert!(fetches.iter().any(|f| f.res == 6), "r6 is wanted at z8");
            for f in fetches {
                tiles.land(f.res, f.tile, Ok(vec![hex_record(f.res)]));
            }
            tiles.take_changed();
            assert_eq!(tiles.records(6).len(), 1);
            p.hexes = Some(tiles);
            p.mode = Mode::Hexagons;
            p.zoom = 8.0;
        });
        let hex = |i: usize| FeatureRef { source: hexes::source_name(6).into(), index: i, id: None };
        panel.update(cx, |p, cx| {
            p.hover_hex = Some(hex(0));
            p.sync_chart_hover(cx);
        });
        let pointer = |cx: &mut gpui::VisualTestContext| panel.read_with(cx, |p, _| p.pointer());
        assert!(pointer(cx), "over a drawn, shown r6 hexagon");
        let chart = panel.read_with(cx, |p, _| p.chart.clone());
        assert_eq!(chart.read_with(cx, |c, _| c.map_hover), Some(1), "its largest group's bar (White) lights");
        panel.update(cx, |p, cx| {
            p.legend.toggle(1);
            p.sync_chart_hover(cx);
        });
        assert!(!pointer(cx), "its largest group (White) hidden: its click does nothing");
        assert_eq!(chart.read_with(cx, |c, _| c.map_hover), None, "a hidden hexagon lights no bar");
        panel.update(cx, |p, _| p.legend.toggle(1));
        assert!(pointer(cx));
        panel.update(cx, |p, _| p.zoom = 11.0);
        assert!(!pointer(cx), "r6 is not drawn at z11");
        panel.update(cx, |p, _| p.zoom = 8.0);
        panel.update(cx, |p, _| p.mode = Mode::Dots);
        assert!(!pointer(cx), "dot mode clicks no hexagon");
        panel.update(cx, |p, _| {
            p.mode = Mode::Hexagons;
            p.hover_hex = Some(hex(5));
        });
        assert!(!pointer(cx), "no record behind index 5");
    }

    /// Review L1: the cluster archive's credit reaches the map's own
    /// attribution line when the clusters attach -- shown with the About
    /// card collapsed -- and is still the About card's line.
    #[test]
    fn the_cluster_archives_credit_is_on_the_attribution_line() {
        let mut app = gpui::TestAppContext::single();
        let (host, cx) = window::host_at(&mut app, (220.0, 40.0));
        let panel = host.read_with(cx, |h, _| h.panel.clone());
        let archive = clusters::ClusterArchive::for_test("U.S. Census Bureau, 2020 P.L. 94-171");
        panel.update(cx, |p, cx| {
            p.about_open = false;
            p.pending_mode = None;
            p.attach(OpenedSources { dots: Err("no dots".into()), hexes: None, clusters: Some(Ok(archive)) }, cx);
        });
        let line = panel.read_with(cx, |p, cx| {
            p.map.read(cx).frame_attribution().map(|c| gpui_maps::tile_source::attribution_text(&c)).unwrap_or_default()
        });
        assert!(line.contains("U.S. Census Bureau, 2020 P.L. 94-171"), "{line}");
        assert_eq!(line.matches("Census Bureau").count(), 1, "the holder once, not once per archive: {line}");
        let about = panel.read_with(cx, |p, _| cluster_credit_line(p.clusters.as_ref().map(ClusterTiles::attribution)));
        assert_eq!(about.as_deref(), Some("Clusters: U.S. Census Bureau, 2020 P.L. 94-171."));
    }

    #[test]
    fn the_pointing_hand_is_for_what_a_click_acts_on() {
        assert!(pointing_hand(Mode::Hexagons, true, false));
        assert!(!pointing_hand(Mode::Hexagons, false, true), "a donut is not clickable in hexagon mode");
        assert!(pointing_hand(Mode::Clusters, false, true));
        assert!(!pointing_hand(Mode::Clusters, true, false), "nor a hexagon in cluster mode");
        assert!(!pointing_hand(Mode::Dots, true, true));
    }

    /// Nothing is opened on the UI thread: the panel exists, loading, the
    /// moment it is built; the notes land once the background has run.
    #[test]
    fn the_archives_open_off_the_ui_thread_and_greyed_modes_say_why() {
        let mut app = gpui::TestAppContext::single();
        let cx = app.add_empty_window();
        let panel = cx.update(|window, cx| {
            let sources = RaceSources {
                dots: PathBuf::from("/nonexistent/race-dots.pmtiles"),
                hexes: Some(HexSourceSpec::Pmtiles(PathBuf::from("/nonexistent/race-hex.pmtiles"))),
                clusters: Some(PathBuf::from("/nonexistent/race-clusters.pmtiles")),
            };
            let options = PanelOptions { basemap: Basemap::None, ..PanelOptions::default() };
            cx.new(|cx| RaceMapPanel::with_options(window, cx, sources, options))
        });
        assert!(!panel.read_with(cx, |p, _| p.loaded()), "the build returned before any archive was opened");
        assert!(panel.read_with(cx, |p, _| p.unavailable_lines().is_empty()), "no reason while loading");
        cx.run_until_parked();
        assert!(panel.read_with(cx, |p, _| p.loaded()));
        let lines = panel.read_with(cx, |p, _| p.unavailable_lines());
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].starts_with("Hexagons off: no hex archive at /nonexistent/race-hex.pmtiles"), "{lines:?}");
        assert!(lines[1].starts_with("Clusters off: no cluster archive at"), "{lines:?}");
        assert!(lines.iter().all(|l| !l.contains("cargo")), "the command is the banner's, not the switcher's: {lines:?}");
        assert_eq!(panel.read_with(cx, |p, _| p.hud()), "no dots: no race archive at /nonexistent/race-dots.pmtiles -- build it with `cargo run --release --manifest-path tools/race-build/Cargo.toml`");
    }

    #[test]
    fn a_reason_is_short_and_tilded() {
        let home = std::env::var("HOME").unwrap_or_default();
        if !home.is_empty() {
            let note = format!("no hex archive at {home}/.cache/x.pmtiles -- build it with `cargo run ...`");
            assert_eq!(unavailable_reason(Some(&note)), "no hex archive at ~/.cache/x.pmtiles");
        }
        assert_eq!(unavailable_reason(None), "no source");
    }

    /// Review H1: a click at a known window point reaches `donut_at` in the
    /// map's own pixels, with the panel embedded away from the window's
    /// origin.
    #[test]
    fn a_donut_click_asks_donut_at_in_map_pixels() {
        let mut app = gpui::TestAppContext::single();
        let (host, cx) = window::host(&mut app);
        // Clear of every card: below the legend, left of the About panel.
        window::click(cx, 400.0, 600.0);
        assert_eq!(window::query(cx, &host), Some((300.0, 550.0)));
    }

    /// Review M2: a click on a card (the legend) over the map never asks
    /// for a donut; a click on the map still does.
    #[test]
    fn a_click_on_a_card_is_not_a_donut_click() {
        let mut app = gpui::TestAppContext::single();
        let (host, cx) = window::host(&mut app);
        // The legend: 300 px wide from panel (12, 12).
        window::click(cx, 100.0 + 150.0, 50.0 + 40.0);
        assert_eq!(window::query(cx, &host), None, "a legend click went through to the map");
        window::click(cx, 400.0, 600.0);
        assert_eq!(window::query(cx, &host), Some((300.0, 550.0)));
    }

    /// The left column (legend, and the states chart in States mode) runs
    /// down the map's left edge, the SWAT mark's corner: in every mode it
    /// ends above the mark's top.
    #[test]
    fn the_left_column_keeps_clear_of_the_brand_mark() {
        let mut app = gpui::TestAppContext::single();
        let at = (220.0, 40.0);
        let (host, cx) = window::host_at(&mut app, at);
        let panel = host.read_with(cx, |h, _| h.panel.clone());
        for mode in Mode::ALL {
            panel.update(cx, |p, cx| {
                p.mode = mode;
                cx.notify();
            });
            window::draw(cx);
            let b = cx.debug_bounds("race-left").expect("the left column is drawn");
            let (y, h) = (f32::from(b.origin.y) - at.1, f32::from(b.size.height));
            let (_, my, _, _) = gpui_maps::brand_mark::BrandMark::default().footprint(700.0);
            assert!(h > 0.0);
            assert!(y + h <= my, "{mode:?}: the left column reaches down to {} over the mark's top at {my}", y + h);
        }
    }

    /// Every card's bounds, by its debug selector (`None`: not drawn).
    fn card_bounds(cx: &mut gpui::VisualTestContext) -> Vec<(&'static str, Option<gpui::Bounds<gpui::Pixels>>)> {
        ["race-legend", "race-switcher", "race-about", "race-states"].into_iter().map(|s| (s, cx.debug_bounds(s))).collect()
    }

    /// The user's ask: pressing a button never moves or resizes a card.
    /// Every toggle, show-all, mode switch, a HUD and status of any
    /// length, a state selection and every sort, each followed by a frame:
    /// the legend, the switcher and the About card keep their bounds to
    /// the pixel throughout, and the states chart keeps its own while it
    /// is shown.
    #[test]
    fn no_button_moves_or_resizes_a_card() {
        let mut app = gpui::TestAppContext::single();
        let (host, cx) = window::host_at(&mut app, (220.0, 40.0));
        let panel = host.read_with(cx, |h, _| h.panel.clone());
        let step = |cx: &mut gpui::VisualTestContext, f: &dyn Fn(&mut RaceMapPanel, &mut Context<RaceMapPanel>)| {
            panel.update(cx, |p, cx| {
                f(p, cx);
                cx.notify();
            });
            window::draw(cx);
        };
        step(cx, &|p, _| p.mode = Mode::Dots);
        let base = card_bounds(cx);
        assert!(base[0].1.is_some() && base[1].1.is_some() && base[3].1.is_none(), "{base:?}");
        let same = |cx: &mut gpui::VisualTestContext, what: &str| {
            let now = card_bounds(cx);
            for ((name, b0), (_, b1)) in base.iter().zip(&now) {
                if *name != "race-states" {
                    assert_eq!(b0, b1, "{name} moved or resized after {what}");
                }
            }
        };
        for g in 0..8 {
            step(cx, &|p, cx| p.toggle_group(g, cx));
            same(cx, &format!("hiding group {g}"));
        }
        step(cx, &|p, cx| p.show_all(cx));
        same(cx, "show all");
        step(cx, &|p, cx| p.toggle_group(3, cx));
        same(cx, "hiding one group (Show all appears)");
        step(cx, &|p, cx| p.show_all(cx));
        for mode in Mode::ALL {
            // Hexagons and clusters have no archive in a test: set directly.
            step(cx, &|p, cx| {
                p.mode = mode;
                p.drive_states(cx);
            });
            same(cx, &format!("{mode:?} mode"));
        }
        // Hexagon mode's tie key and status, and a long HUD.
        step(cx, &|p, _| {
            p.mode = Mode::Hexagons;
            p.zoom = 5.0;
            p.hex_note = Some("a very long reason why there are no hexagons today, which goes on and on past any card width".into());
        });
        same(cx, "the hexagon status and a long HUD");
        step(cx, &|p, _| p.note = Some("x".repeat(400)));
        same(cx, "a 400-character dots note");
        step(cx, &|p, cx| {
            p.mode = Mode::States;
            p.drive_states(cx);
        });
        same(cx, "States mode");
        let chart = cx.debug_bounds("race-states").expect("the states chart in States mode");
        assert!(f32::from(chart.size.height) > 100.0, "{chart:?}");
        let chart_same = |cx: &mut gpui::VisualTestContext, what: &str| {
            same(cx, what);
            assert_eq!(cx.debug_bounds("race-states"), Some(chart), "the states chart moved or resized after {what}");
        };
        step(cx, &|p, cx| p.select_state(Some(4), cx));
        chart_same(cx, "selecting a state");
        for key in [SortKey::Name, SortKey::Share(0), SortKey::Share(7), SortKey::Population] {
            step(cx, &|p, _| {
                p.sort = key;
                if let SortKey::Share(g) = key {
                    p.share_group = g;
                }
            });
            chart_same(cx, &format!("sorting by {key:?}"));
        }
        step(cx, &|p, cx| p.toggle_group(1, cx));
        chart_same(cx, "hiding a group in States mode");
        step(cx, &|p, cx| p.select_state(None, cx));
        chart_same(cx, "back to the United States");
    }

    /// The chart's rows and the pies both select a state: a click on a
    /// chart row selects its state and eases to fit it; a click on a pie
    /// on the map (map-local, at the gallery offset) selects the pie's
    /// state; "United States" goes back to the start view.
    #[test]
    fn a_chart_row_or_a_pie_selects_a_state() {
        let mut app = gpui::TestAppContext::single();
        let at = (220.0, 40.0);
        let (host, cx) = window::host_at(&mut app, at);
        let panel = host.read_with(cx, |h, _| h.panel.clone());
        panel.update(cx, |p, cx| {
            p.mode = Mode::States;
            p.sort = SortKey::Population;
            p.drive_states(cx);
            cx.notify();
        });
        window::draw(cx);
        assert_eq!(panel.read_with(cx, |p, _| p.state_pies_drawn()), 51);
        // California tops the population sort: its row is the list's first.
        let ca = states::by_fips(6).unwrap();
        let sel: &'static str = Box::leak(format!("race-states-row-{ca}").into_boxed_str());
        let row = cx.debug_bounds(sel).expect("California's row is drawn");
        let c = row.center();
        window::click(cx, f32::from(c.x), f32::from(c.y));
        assert_eq!(panel.read_with(cx, |p, _| p.selected_state()), Some(ca));
        let (lon, _, zoom) = panel.read_with(cx, |p, _| p.last_state_target).expect("eased");
        assert!((-125.0..-114.0).contains(&lon) && zoom > 4.0, "{lon} z{zoom}");
        // Back to the US.
        panel.update(cx, |p, cx| p.select_state(None, cx));
        assert_eq!(panel.read_with(cx, |p, _| (p.selected_state(), p.last_state_target)), (None, Some(walker::START)));
        // Kansas's pie, where the map draws it (the camera has not moved:
        // a test window does not animate).
        let ks = states::by_fips(20).unwrap();
        let (x, y) = panel.read_with(cx, |p, cx| {
            let m = p.map.read(cx);
            let a = states::rows()[ks].anchor;
            lon_lat_to_screen(m.camera(), m.globe_enabled(), a.0, a.1).expect("Kansas is on screen")
        });
        window::click(cx, at.0 + x, at.1 + y);
        assert_eq!(panel.read_with(cx, |p, _| p.selected_state()), Some(ks), "Kansas's pie at ({x}, {y})");
    }

    /// The user: "why states don't contain total number" -- every state
    /// pie's centre carries its population, as every cluster donut does,
    /// and keeps it through real clicks on the chart (a state row, the
    /// breadcrumb, the sort pills, a group row) and on the legend (hiding
    /// a group keeps the totals; hiding all takes pies and labels off).
    #[test]
    fn every_state_pie_keeps_its_total_through_chart_and_legend_clicks() {
        let mut app = gpui::TestAppContext::single();
        let (host, cx) = window::host_at(&mut app, (220.0, 40.0));
        let panel = host.read_with(cx, |h, _| h.panel.clone());
        panel.update(cx, |p, cx| {
            p.mode = Mode::States;
            p.drive_states(cx);
            cx.notify();
        });
        window::draw(cx);
        let expected: Vec<(u8, String)> = {
            let mut v: Vec<(u8, String)> = states::rows().iter().map(|r| (r.fips, walker::count_label(r.total() as u32))).collect();
            v.sort();
            v
        };
        let check = |cx: &mut gpui::VisualTestContext, what: &str| {
            let (mut labels, pies, on_map) = panel.read_with(cx, |p, cx| {
                (p.state_labels().to_vec(), p.state_pies_drawn(), p.map.read(cx).overlay_source_order().iter().any(|s| s == states::LABEL_SOURCE))
            });
            assert!(on_map, "the label source is on the map after {what}");
            assert_eq!(labels.len(), pies, "one label per pie after {what}");
            labels.sort();
            assert_eq!(labels, expected, "every state's total after {what}");
        };
        check(cx, "opening States mode");
        let sc = states::by_fips(45).unwrap();
        assert!(panel.read_with(cx, |p, _| p.state_labels().contains(&(45, "5.1M".into()))), "South Carolina's 5,118,425");
        let click_id = |cx: &mut gpui::VisualTestContext, id: &str| {
            let b = cx.debug_bounds(Box::leak(id.to_string().into_boxed_str())).unwrap_or_else(|| panic!("{id} is drawn"));
            let c = b.center();
            window::click(cx, f32::from(c.x), f32::from(c.y));
        };
        // Sort by the share of Black (South Carolina is near the top), and
        // click its row.
        click_id(cx, "race-states-group-2");
        assert_eq!(panel.read_with(cx, |p, _| p.sort()), SortKey::Share(2));
        check(cx, "a group row (share sort)");
        click_id(cx, &format!("race-states-row-{sc}"));
        assert_eq!(panel.read_with(cx, |p, _| p.selected_state()), Some(sc));
        check(cx, "a state row");
        click_id(cx, "race-states-sort-name");
        check(cx, "the Name pill");
        click_id(cx, "race-states-sort-pop");
        check(cx, "the Population pill");
        click_id(cx, "race-states-us");
        assert_eq!(panel.read_with(cx, |p, _| p.selected_state()), None);
        check(cx, "the United States breadcrumb");
        // The legend: hiding White keeps every pie and its total.
        click_id(cx, "race-legend-1");
        assert_eq!(panel.read_with(cx, |p, _| p.legend.mask()), !(1u8 << 1));
        check(cx, "hiding White in the legend");
        panel.update(cx, |p, cx| {
            for g in [0, 2, 3, 4, 5, 6, 7] {
                p.toggle_group(g, cx);
            }
        });
        window::draw(cx);
        assert_eq!(panel.read_with(cx, |p, _| (p.state_pies_drawn(), p.state_labels().len())), (0, 0), "nothing shown: no pies, no labels");
        panel.update(cx, |p, cx| p.show_all(cx));
        check(cx, "show all");
        // Leaving States mode takes the labels off with the pies.
        panel.update(cx, |p, cx| {
            p.mode = Mode::Dots;
            p.drive_states(cx);
        });
        assert!(panel.read_with(cx, |p, cx| p.state_labels().is_empty() && !p.map.read(cx).overlay_source_order().iter().any(|s| s == states::LABEL_SOURCE)));
    }

    #[test]
    fn the_states_hud_names_the_selection() {
        assert_eq!(states_hud_text(None, 51), "United States · 331,449,281 people · 51 state pies");
        let ri = states::by_fips(44).unwrap();
        assert!(states_hud_text(Some(ri), 51).starts_with("Rhode Island · 1,097,379 people"));
        assert_eq!(status_text(Mode::States, 4.0), "Pie area: population. Click a state for its groups");
        assert!(!dots_active(Mode::States, 14.0));
        assert!(pointing_hand(Mode::States, false, true));
    }

    #[test]
    fn every_mode_is_offered() {
        assert_eq!(Mode::ALL.iter().filter(|m| m.enabled()).collect::<Vec<_>>(), vec![&Mode::Dots, &Mode::Hexagons, &Mode::Clusters, &Mode::States]);
    }

    #[test]
    fn a_donut_click_eases_to_its_expansion_zoom_capped_at_fourteen() {
        let r = ClusterRecord { lon: -73.9, lat: 40.7, blocks: 12, pop: 9000, counts: [1, 2, 3, 4, 5, 6, 7, 8964], expansion_zoom: 9, leaf: false };
        assert_eq!(donut_click_target(&r), (-73.9, 40.7, 9.0));
        assert_eq!(donut_click_target(&ClusterRecord { expansion_zoom: 12, leaf: true, blocks: 1, ..r }).2, 12.0);
        assert_eq!(donut_click_target(&ClusterRecord { expansion_zoom: 200, ..r }).2, DONUT_CLICK_MAX_ZOOM);
    }

    #[test]
    fn the_cluster_hud_names_donuts_zoom_and_trouble() {
        let h = ClusterHud { donuts: 1234, tile_zoom: 8, ..ClusterHud::default() };
        assert_eq!(cluster_hud_text(h, None), "1,234 donuts · z8 clusters");
        let h = ClusterHud { donuts: 8192, truncated: 1500, tile_zoom: 11, loading: true, errors: 2, refused: 5 };
        assert_eq!(cluster_hud_text(h, None), "8,192 donuts · z11 clusters · 1,500 truncated · loading… · 2 tile errors · 5 corrupt records refused");
        assert_eq!(cluster_hud_text(ClusterHud::default(), Some("no archive")), "no clusters: no archive");
    }

    /// Final review #7: the cluster archive's credit reaches the About
    /// card; no clusters, or a blank credit, adds no line.
    #[test]
    fn the_cluster_archives_credit_is_shown() {
        assert_eq!(cluster_credit_line(Some("U.S. Census Bureau")).as_deref(), Some("Clusters: U.S. Census Bureau."));
        assert_eq!(cluster_credit_line(Some("  ")), None);
        assert_eq!(cluster_credit_line(None), None);
    }

    fn hex_fixture() -> HexPopup {
        HexPopup::of(&[3, 90, 5, 0, 1, 0, 0, 1])
    }

    #[test]
    fn the_popup_closes_on_zoom_start_and_mode_change() {
        let mut p = PopupState::open(hex_fixture());
        p.on_zoom_start();
        assert!(p.is_closed());
        let mut p = PopupState::open(hex_fixture());
        p.on_mode_change(Mode::Dots);
        assert!(p.is_closed());
    }

    /// `close()` is what Escape (`on_key`) and the popup's × call; the key
    /// wiring itself needs a window (owed to the controller's click run).
    #[test]
    fn close_closes_the_popup_and_closed_is_the_default() {
        assert!(PopupState::default().is_closed());
        let mut p = PopupState::open(hex_fixture());
        assert!(!p.is_closed());
        p.close();
        assert!(p.is_closed());
    }

    #[test]
    fn a_press_opens_nothing_until_a_release_without_a_drag() {
        let mut g: ClickGate<u32> = ClickGate::default();
        // A click: press, the engine's hit, release 3 px away.
        g.press((100.0, 100.0));
        g.hit(7);
        assert_eq!(g.release((103.0, 100.0)), Some(7));
        // A pan: the same press and hit, released 5 px away.
        g.press((100.0, 100.0));
        g.hit(7);
        assert_eq!(g.release((104.0, 103.0)), None, "5 px is a drag");
        // Exactly at the slop still clicks.
        g.press((0.0, 0.0));
        g.hit(1);
        assert_eq!(g.release((CLICK_SLOP_PX, 0.0)), Some(1));
        // A press on empty map (no hit), and a hit with no press.
        g.press((0.0, 0.0));
        assert_eq!(g.release((0.0, 0.0)), None);
        g.hit(2);
        assert_eq!(g.release((0.0, 0.0)), None);
        // A new press drops the previous press's hit.
        g.press((0.0, 0.0));
        g.hit(3);
        g.press((50.0, 50.0));
        assert_eq!(g.release((50.0, 50.0)), None);
        // A release consumes the gate: a second release fires nothing.
        g.press((0.0, 0.0));
        g.hit(4);
        assert_eq!(g.release((0.0, 0.0)), Some(4));
        assert_eq!(g.release((0.0, 0.0)), None);
    }

    fn camera(lon: f64, lat: f64, zoom: f64) -> gpui_maps::render::Camera {
        gpui_maps::render::Camera { center_lon: lon, center_lat: lat, zoom, pitch: 0.0, bearing: 0.0, viewport_width_px: 1280.0, viewport_height_px: 800.0, fov_y: gpui_maps::render::Camera::DEFAULT_FOV_Y }
    }

    /// The popup's projection is the inverse of the engine's own pick
    /// (`unproject_to_world_on_globe`, what hover and click use): a pixel
    /// taken to the ground and back lands on itself, flat and on the globe,
    /// panned and pitched.
    #[test]
    fn the_popup_projection_round_trips_the_engines_pick() {
        let ws = f64::from(gpui_maps::render::WORLD_SIZE);
        let mut cases = vec![
            (camera(-95.37, 29.76, 10.0), false),
            (camera(-95.37, 29.76, 10.0), true),
            (camera(-98.0, 39.0, 4.0), true), // on the globe
            (camera(-73.97, 40.78, 11.6), true), // inside the globe->flat blend
        ];
        let mut pitched = camera(-73.97, 40.78, 12.5);
        pitched.pitch = 0.6;
        pitched.bearing = 0.4;
        cases.push((pitched, true));
        for (cam, globe) in cases {
            for &(x, y) in &[(640.0f32, 400.0f32), (100.0, 80.0), (1200.0, 700.0), (900.0, 300.0)] {
                let world = gpui_maps::render::globe::unproject_to_world_on_globe(&cam, globe, x, y).expect("the pixel is on the ground");
                let (lon, lat) = gpui_maps::geo::tile_xy_to_lon_lat(world.x / ws, world.y / ws, 0.0);
                let (sx, sy) = lon_lat_to_screen(&cam, globe, lon, lat).expect("a visible point projects");
                assert!((sx - x).abs() < 1.0 && (sy - y).abs() < 1.0, "z{} globe {globe}: ({x}, {y}) came back as ({sx}, {sy})", cam.zoom);
            }
        }
    }

    #[test]
    fn the_popup_hides_off_screen_and_behind_the_planet() {
        let cam = camera(-95.37, 29.76, 10.0);
        // The camera's centre is the viewport's centre.
        let (x, y) = popup_anchor(&cam, true, (-95.37, 29.76)).expect("on screen");
        assert!((x - 640.0).abs() < 0.5 && (y - 400.0).abs() < 0.5, "({x}, {y})");
        // Dallas is off a z10 Houston view: hidden, though it projects.
        assert!(lon_lat_to_screen(&cam, true, -96.8, 32.78).is_some());
        assert_eq!(popup_anchor(&cam, true, (-96.8, 32.78)), None);
        // From a z3 globe over the US, Beijing is on the far side.
        let globe = camera(-98.0, 39.0, 3.0);
        assert_eq!(lon_lat_to_screen(&globe, true, 116.4, 39.9), None);
        assert!(popup_anchor(&globe, true, (-98.0, 39.0)).is_some());
    }

    #[test]
    fn hiding_a_hexagons_group_closes_its_popup() {
        let mut p = PopupState::open(hex_fixture()); // White is the largest group (1)
        assert_eq!(hex_fixture().modal(), Modal::Group(1));
        p.on_mask(0b1111_1110); // Hispanic off: not this hexagon's
        assert!(!p.is_closed());
        p.on_mask(0b1111_1101); // White off
        assert!(p.is_closed());
        let mut tie = PopupState::open(HexPopup::of(&[20, 20, 0, 0, 0, 0, 0, 0]));
        tie.on_mask(0); // the tie key is not a toggle
        assert!(!tie.is_closed());
    }

    #[test]
    fn the_popup_gives_every_groups_share() {
        let p = hex_fixture();
        assert_eq!(p.total, 100);
        assert!(!p.tie);
        assert_eq!(p.rows.map(|r| r.0), walker::GROUPS.map(|g| g.label));
        assert_eq!(p.rows[1].1, 90.0);
        assert_eq!(p.rows[3].1, 0.0);
        assert_eq!(p.rows[4].2, walker::color(4));
        assert_eq!(p.title(), "100 people in this hexagon");
        let t = HexPopup::of(&[20, 20, 0, 0, 0, 0, 0, 0]);
        assert!(t.tie);
        assert_eq!(t.title(), "40 people in this hexagon");
        assert_eq!(HexPopup::of(&[1_234_567, 0, 0, 0, 0, 0, 0, 0]).title(), "1,234,567 people in this hexagon");
    }

    #[test]
    fn the_status_line_in_hexagon_mode_hands_over_at_legend_switch() {
        let hex = "Hex colours show the largest group, not population density. Minimum: 10 people.";
        assert_eq!(status_line(Mode::Hexagons, 12.3, Some(1.0)), hex);
        assert_eq!(status_line(Mode::Hexagons, 12.375, None), "Zoom in to see every dot");
        assert!(!dots_active(Mode::Hexagons, walker::LEGEND_SWITCH_HEX - 1e-9));
        assert!(dots_active(Mode::Hexagons, walker::LEGEND_SWITCH_HEX));
    }

    #[test]
    fn the_hud_says_parts_for_the_part_level() {
        let mut s = DotStats { level: GridTileKey::PART_LEVEL, dots_drawn: 5, ppd: 1.0, people_in_view: 1234.0, dots_in_view: 1234, ..Default::default() };
        assert_eq!(level_text(&s), "parts");
        s.part_fallback_tiles = 3;
        assert_eq!(level_text(&s), "parts, 3 regions as cells (part cap)");
        assert_eq!(hud_text(Some(&s), None), "1,234 people in view · 1,234 dots · 1 dot = 1.00 people · parts, 3 regions as cells (part cap)");
        s.level = 4;
        assert_eq!(level_text(&s), "level 4");
        assert_eq!(hud_text(None, Some("no archive")), "no dots: no archive");
        assert_eq!(hud_text(None, None), "loading dots…");
    }
}

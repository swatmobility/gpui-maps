//! The satellites demo as one gpui entity, [`SatellitesPanel`]: the globe
//! with every catalogued object as a keyframed altitude layer, the
//! crossfilter column (top left: type and country chips, altitude and
//! inclination charts, every count under the other filters), "Show GEO" /
//! "Show LEO" / "About" (top right), the hover card, the pinned card of a selected object with "Follow", the ISS
//! with its halo, ring and ground track, and the time bar (bottom). Every
//! decision is `model.rs`'s; this file is wiring and layout.
//!
//! It takes its data from a [`DataSource`] and its host from a
//! [`PanelHost`]: the gallery embeds it as the `satellites` panel
//! (`kit/src/panels/satellites.rs`, which includes this file for both
//! crates), and `demos/satellites.rs` is its window and its flags. Every
//! pointer position it uses is panel-relative (the map's own pixels),
//! whatever the panel's place in the window.
//!
//! **Rings and ground tracks are the host's to rebuild** (Task 7 review,
//! Minor 6): `orbit_ring_teme` and `ground_track` are sampled once, at the
//! scene time they are built for, while the dot is re-propagated every
//! keyframe, so J2 precession walks a LEO dot off its ring by ~24 km an hour
//! at 1x and by degrees within minutes at 1000x. The panel rebuilds a ring
//! when its object changes and whenever the scene time has moved an eighth
//! of the object's period either way since it was built, and a ground track
//! every twentieth ([`model::ring_due`]): a scrub, a reverse run and a
//! 1000x run are all "the scene moved". At 1x that is every ~12 min for the
//! ISS ring (~64 us to rebuild) and every ~5 min for its track; at 1000x
//! every 0.7 s and 0.3 s of wall time. A GEO track (~4,300 samples of the
//! deep-space integrator, ~10 ms) is due every ~72 wall minutes at 1x.
//!
//! **The fetch uses the wall clock**, never the scene clock:
//! `Fetcher::refresh(now_unix)` is handed `SystemTime::now()`, so a scene
//! scrubbed a day ahead cannot make a request due early (Task 8 carry).

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use gpui::prelude::FluentBuilder as _;
use gpui::{
    div, px, relative, rgb, rgba, App, AppContext, Context, Entity, FontWeight, InteractiveElement, IntoElement, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, Window,
};
use gpui_maps::camera_control::{MAX_ZOOM, SPACE_MIN_ZOOM};
use gpui_maps::map_view::{AltitudePositions, MapView, SceneClock};
use gpui_maps::orbit::catalog::ISS_NORAD;
use gpui_maps::orbit::propagate::{self, CatalogSource};
use gpui_maps::render::altitude::{AltitudeLine, RefFrame};
use gpui_maps::tile_source::http::HttpTileSource;
use gpui_maps::tile_source::TileSource;

use super::about::{self, AboutFacts};
use super::charts;
use super::model::{self, Bodies, Chips, Loaded, Selection, GROUPS, GROUP_COUNT};
use gpui_maps::render::altitude_pipeline::AltitudeLook;

/// The dots' altitude layer id.
pub const LAYER: &str = "sats";
/// The ISS's ring: always shown (thin) unless another object is selected.
pub const RING_ISS: &str = "sat-ring-iss";
/// The selected object's ring, on top of the others.
pub const RING_SELECTED: &str = "sat-ring-selected";
/// The hovered object's ring, faint.
pub const RING_HOVER: &str = "sat-ring-hover";
/// Width (logical px) and alpha of each ring (spec §4.2).
pub const RING_STYLE: [(&str, f32, f32); 3] = [(RING_ISS, 1.0, 0.5), (RING_SELECTED, 1.5, 0.7), (RING_HOVER, 1.0, 0.35)];
/// The ground track's overlay sources: `sat-track-0`, `sat-track-1`, ...
fn track_source(k: usize) -> String {
    format!("sat-track-{k}")
}

/// Samples per ring.
pub const RING_SAMPLES: usize = 256;
/// The credit appended to the basemap's on the map's attribution line.
pub const DATA_CREDIT: &str = "Orbits: CelesTrak / US Space Force";
/// The credit when the synthetic set is drawn.
pub const SYNTHETIC_CREDIT: &str = "Orbits: SYNTHETIC (seeded, not real)";
/// Where the time bar starts, px from the map's left edge: right of the
/// SWAT mark's footprint (6 + 87 px wide at its default size) and a gap.
/// Pinned by `the_time_bar_keeps_clear_of_the_brand_mark`.
pub const TIME_BAR_LEFT: f32 = 104.0;
/// The time bar's bottom offset: above the mark's band (12..30 px up) and
/// the attribution line, so neither is covered at any map width.
pub const TIME_BAR_BOTTOM: f32 = 30.0;
/// The opening camera: the LEO swarm filling the view, GEO's ring past it.
pub const START: (f64, f64, f64) = (10.0, 20.0, 0.0);

/// Where the orbits come from.
#[derive(Clone, Debug, PartialEq)]
pub enum DataSource {
    /// The CelesTrak cache (`fetch.rs`); `fetch` lets the panel refresh the
    /// enabled groups once on open, under the two hour rule.
    Cache { dir: PathBuf, fetch: bool },
    /// One OMM JSON file (`--omm`), classified by whatever group lists
    /// `groups_dir` holds; never fetched.
    Omm { file: PathBuf, groups_dir: Option<PathBuf> },
    /// The seeded synthetic set.
    Synthetic,
}

impl DataSource {
    /// The default cache, refreshed once on open.
    #[must_use]
    pub fn default_cache() -> Self {
        Self::Cache { dir: default_cache_dir(), fetch: true }
    }
}

/// `~/.cache/gpui-map-data/satellites` (`fetch::default_cache_dir`).
#[must_use]
pub fn default_cache_dir() -> PathBuf {
    std::env::var_os("HOME").map_or_else(std::env::temp_dir, PathBuf::from).join(".cache/gpui-map-data/satellites")
}

/// The basemap under the globe (`demos/README.md`): Mapbox Light with a
/// token, OpenFreeMap Positron without one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basemap {
    MapboxLight,
    Positron,
    /// No tiles and no network (tests).
    None,
}

impl Basemap {
    /// Mapbox Light with a token, else Positron.
    #[must_use]
    pub fn for_token(token: Option<&str>) -> Self {
        if token.is_some_and(|t| !t.is_empty()) {
            Basemap::MapboxLight
        } else {
            Basemap::Positron
        }
    }

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Basemap::MapboxLight => "Mapbox Light",
            Basemap::Positron => "OpenFreeMap Positron",
            Basemap::None => "none",
        }
    }
}

/// `MAPBOX_ACCESS_TOKEN` from the environment. Never printed.
#[must_use]
pub fn mapbox_token() -> Option<String> {
    crate::kit::env_mapbox_token()
}

/// A basemap's compiled style, its tiles and (Mapbox) its sprite URL.
pub struct BasemapSpec {
    pub style: gpui_maps::style::Style,
    pub tiles: Arc<dyn TileSource>,
    /// The resolved sprite URL (with the token); never printed.
    pub sprite_url: Option<String>,
    /// "Earth: real": the imagery source and its tile size, for
    /// `MapView::set_raster_source`; `None` for a vector-only style.
    pub raster: Option<(Arc<dyn TileSource>, u32)>,
}

/// Builds `which`, as `race_dots` does. Mapbox Light without a token
/// panics: the caller chose it.
#[must_use]
pub fn basemap_spec(which: Basemap, token: Option<&str>) -> BasemapSpec {
    basemap_spec_night(which, token, false)
}

/// [`basemap_spec`], turned night-dark ([`model::darken_style_json`]) when
/// `night`: what the Glow look draws over.
#[must_use]
pub fn basemap_spec_night(which: Basemap, token: Option<&str>, night: bool) -> BasemapSpec {
    basemap_spec_earth(which, token, if night { model::Earth::Night } else { model::Earth::Map })
}

/// [`basemap_spec`] for an "Earth" choice: the basemap as it is
/// ([`model::Earth::Map`]), in night colours ([`model::Earth::Night`]),
/// or Natural Earth imagery under its borders ([`model::Earth::Real`],
/// with [`BasemapSpec::raster`] set).
#[must_use]
pub fn basemap_spec_earth(which: Basemap, token: Option<&str>, earth: model::Earth) -> BasemapSpec {
    let read = |name: &str, what: &str| {
        let bytes = crate::kit::asset_bytes(name).unwrap_or_else(|_| panic!("vendored {what} style"));
        let turned = match earth {
            // Streets at its own brightness: the dots draw "over" on it.
            model::Earth::Map | model::Earth::Streets => None,
            model::Earth::Night => model::darken_style_json(&bytes),
            model::Earth::Real => model::real_earth_style_json(&bytes),
        };
        turned.unwrap_or(bytes)
    };
    let raster = (earth == model::Earth::Real && which != Basemap::None).then(|| {
        let source: Arc<dyn TileSource> = Arc::new(HttpTileSource::new(model::NATURAL_EARTH_URL, 6, model::NATURAL_EARTH_ATTRIBUTION));
        (source, 512)
    });
    // "Earth: streets": Mapbox Streets with a token, else Liberty on the
    // same OpenFreeMap tiles Positron draws.
    let streets = earth == model::Earth::Streets;
    match which {
        Basemap::MapboxLight => {
            let token = token.expect("Mapbox Light needs MAPBOX_ACCESS_TOKEN").to_string();
            let bytes = if streets { read("examples/mapbox-streets-style.json", "Mapbox Streets") } else { read("demos/mapbox-light-style.json", "Mapbox Light") };
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
            BasemapSpec { style, tiles: Arc::new(tiles), sprite_url, raster }
        }
        Basemap::Positron => {
            let bytes = if streets { read("examples/liberty-style.json", "Liberty") } else { read("demos/positron-style.json", "Positron") };
            let style = gpui_maps::style_spec::parse_and_compile(&bytes).expect("the OpenFreeMap style must compile");
            let sprite_url = if streets { style.sprite.clone() } else { None };
            let tiles = HttpTileSource::new(
                "https://tiles.openfreemap.org/planet/20260802_080001_pt/{z}/{x}/{y}.pbf",
                14,
                concat!(
                    r#"<a href="https://openfreemap.org" target="_blank">OpenFreeMap</a> "#,
                    r#"<a href="https://www.openmaptiles.org/" target="_blank">&copy; OpenMapTiles</a> "#,
                    r#"Data from <a href="https://www.openstreetmap.org/copyright" target="_blank">OpenStreetMap</a>"#,
                ),
            );
            BasemapSpec { style, tiles: Arc::new(tiles), sprite_url, raster }
        }
        Basemap::None => BasemapSpec {
            style: gpui_maps::style::Style { layers: Vec::new(), sprite: None, sky: Default::default() },
            tiles: Arc::new(gpui_maps::tile_source::EmptyTileSource),
            sprite_url: None,
            raster: None,
        },
    }
}

/// The tokio runtime the panel's map fetches on: one per process.
pub fn runtime_handle() -> tokio::runtime::Handle {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME
        .get_or_init(|| tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("a tokio runtime"))
        .handle()
        .clone()
}

/// Wall-clock Unix seconds: the only clock the fetch and the scrubber's
/// "now" read.
#[must_use]
pub fn wall_unix() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

/// How the panel opens.
#[derive(Debug, Clone)]
pub struct PanelOptions {
    pub basemap: Basemap,
    pub data: DataSource,
    /// Centre lon, lat and zoom.
    pub at: (f64, f64, f64),
    /// The scene time to open at (`--at`); `None` is now.
    pub scene_unix: Option<f64>,
    /// The clock's rate (`--rate`); 1 is real time.
    pub rate: f64,
    /// The map's size before the first layout.
    pub size: (f32, f32),
    /// Which groups' chips start on, in `model::GROUPS` order; all by
    /// default (`satellites --hide-all` starts with none, the perf gate's
    /// A/B control).
    pub groups: [bool; GROUP_COUNT],
    /// The dots' and rings' look (`--look`); glowing by default.
    pub look: AltitudeLook,
    /// Motion trails behind the dots ("Trails"); on by default here, the
    /// space scene being the one place they belong (the engine's are off).
    pub trails: bool,
    /// The planet's look ("Earth"); `None`: real imagery with the Glow
    /// look, the plain map with Flat.
    pub earth: Option<model::Earth>,
}

/// **The plain-defaults exception** (user rule 2026-09-29: every map opens
/// plain): this panel opens with glowing dots, trails and real Earth
/// imagery because the space scene is the demo; each is a toggle here, and
/// the engine itself defaults to none of them (`AltitudeLook::Flat`, no
/// `TrailStyle`, no raster source). Stated on the About card too
/// (`about::DEFAULTS`).
impl Default for PanelOptions {
    fn default() -> Self {
        Self {
            basemap: Basemap::for_token(mapbox_token().as_deref()),
            data: DataSource::default_cache(),
            at: START,
            scene_unix: None,
            rate: 1.0,
            size: (1280.0, 800.0),
            groups: Chips::default().enabled,
            look: AltitudeLook::Glow,
            trails: true,
            earth: None,
        }
    }
}

/// Who hosts the panel: its own window ([`PanelHost::standalone`]) or the
/// gallery (`kit/src/panels/satellites.rs`' `build`).
#[derive(Clone)]
pub struct PanelHost {
    pub runtime: tokio::runtime::Handle,
    /// For [`Basemap::MapboxLight`]. Never printed.
    pub mapbox_token: Option<String>,
    /// The window-title prefix the demo keys publish; `None` in the gallery.
    pub title: Option<String>,
    /// Raised by the gallery when it switches away: a catalogue or a fetch
    /// that lands after that is never attached.
    pub cancel: Arc<AtomicBool>,
    /// Whether the About card starts open.
    pub about_open: bool,
}

impl PanelHost {
    /// The standalone window.
    #[must_use]
    pub fn standalone() -> Self {
        Self { runtime: runtime_handle(), mapbox_token: mapbox_token(), title: Some("Satellites".to_string()), cancel: Arc::default(), about_open: false }
    }
}

/// What a load read, off the UI thread.
pub struct LoadResult {
    pub loaded: Option<Loaded>,
    /// When the cached `active` body (else the newest group body) was fetched.
    pub fetched_unix: Option<f64>,
    /// The fetch status line for the About/status area; `None` when there is
    /// no cache (a file or synthetic).
    pub status: Option<String>,
    /// Some cached group is latched after repeated errors (offer "Retry").
    pub latched: bool,
    /// Read + parse + catalogue build, ms.
    pub took_ms: f64,
}

/// Reads and builds the catalogue for `data`. Blocking: run it on a
/// background thread ([`SatellitesPanel::new`] does).
#[must_use]
pub fn load(data: &DataSource) -> LoadResult {
    let started = Instant::now();
    let read = |dir: &std::path::Path, group: &str| std::fs::read(dir.join(format!("{group}.json"))).ok();
    let from_dir = |dir: &std::path::Path, active: Option<Vec<u8>>| Bodies {
        active,
        stations: read(dir, "stations"),
        gps: read(dir, "gps-ops"),
        geo: read(dir, "geo"),
        galileo: read(dir, "galileo"),
        glonass: read(dir, "glo-ops"),
        beidou: read(dir, "beidou"),
        satcat: read(dir, "satcat"),
    };
    let (loaded, fetched_unix, status, latched) = match data {
        DataSource::Synthetic => (Some(model::synthetic_loaded()), None, None, false),
        DataSource::Omm { file, groups_dir } => {
            let body = std::fs::read(file).ok();
            let bodies = match groups_dir {
                Some(dir) => from_dir(dir, body),
                None => Bodies { active: body, ..Bodies::default() },
            };
            (model::catalog_from_bodies(&bodies), None, None, false)
        }
        DataSource::Cache { dir, .. } => {
            let loaded = model::catalog_from_bodies(&from_dir(dir, read(dir, "active")));
            let (fetched, status, latched) = cache_status(dir);
            (loaded, fetched, status, latched)
        }
    };
    LoadResult { loaded, fetched_unix, status, latched, took_ms: started.elapsed().as_secs_f64() * 1e3 }
}

/// The cache's fetch time, status line and latch, from its metas.
#[cfg(not(target_arch = "wasm32"))]
fn cache_status(dir: &std::path::Path) -> (Option<f64>, Option<String>, bool) {
    use super::fetch::{self, Fetcher, LATCH_AFTER};
    let fetcher = Fetcher::celestrak(dir.to_path_buf());
    let metas: Vec<(&str, fetch::Meta)> = fetch::GROUPS.iter().map(|g| (*g, fetcher.meta(g))).collect();
    let latched = metas.iter().any(|(_, m)| m.consecutive_errors >= LATCH_AFTER);
    // `active` speaks for the cache when it is there; else the group with
    // the newest body.
    let (group, meta) = metas
        .iter()
        .find(|(g, m)| *g == "active" && m.fetched_unix.is_some())
        .or_else(|| metas.iter().filter(|(_, m)| m.fetched_unix.is_some()).max_by(|a, b| a.1.fetched_unix.partial_cmp(&b.1.fetched_unix).unwrap_or(std::cmp::Ordering::Equal)))
        .or_else(|| metas.iter().find(|(_, m)| m.consecutive_errors > 0))
        .cloned()
        .unwrap_or(("active", fetch::Meta::default()));
    (meta.fetched_unix, Some(fetch::status_line(group, &meta, wall_unix())), latched)
}

#[cfg(target_arch = "wasm32")]
fn cache_status(_dir: &std::path::Path) -> (Option<f64>, Option<String>, bool) {
    (None, None, false)
}

/// One refresh of every group the enabled chips need, at the WALL clock:
/// `(any body was fetched, the outcomes)`. Blocking: at most one request
/// per group (each up to `fetch::TIMEOUT`), none unless due.
#[cfg(not(target_arch = "wasm32"))]
pub fn refresh_groups(dir: &std::path::Path, groups: &[&'static str]) -> (bool, Vec<String>) {
    let fetcher = super::fetch::Fetcher::celestrak(dir.to_path_buf());
    let mut fetched = false;
    let mut outcomes = Vec::new();
    for group in groups {
        let outcome = fetcher.refresh(group, wall_unix());
        fetched |= matches!(outcome, super::fetch::Outcome::Fetched { .. });
        outcomes.push(format!("{group}: {outcome:?}"));
    }
    (fetched, outcomes)
}

/// Clears the error latch of every group (the "Retry" button); the two
/// hour rule still stands.
#[cfg(not(target_arch = "wasm32"))]
fn retry_groups(dir: &std::path::Path) {
    let fetcher = super::fetch::Fetcher::celestrak(dir.to_path_buf());
    for group in super::fetch::GROUPS {
        fetcher.retry(group);
    }
}

/// A ring (or track) as drawn: its object and the scene time it was built for.
type Built = Option<(usize, f64)>;

pub struct SatellitesPanel {
    /// The filter column's own view (see [`FilterColumn`]), made on the
    /// first render.
    column: Option<Entity<FilterColumn>>,
    /// The [`ColumnKey`] the column was last built (or told to rebuild)
    /// at.
    column_drawn: Option<ColumnKey>,
    map: Entity<MapView>,
    keys: crate::shell::DemoKeys,
    data: DataSource,
    cancel: Arc<AtomicBool>,
    basemap: Basemap,
    loaded: Option<Loaded>,
    loading: bool,
    opened: Instant,
    /// Read + parse + build of the last load, ms.
    pub load_ms: Option<f64>,
    /// When the first dots were drawn (the ISS, or object 0, has a state).
    pub first_dots_ms: Option<f64>,
    facts: AboutFacts,
    status: Option<String>,
    latched: bool,
    fetching: bool,
    chips: Chips,
    look: AltitudeLook,
    trails: bool,
    earth: model::Earth,
    /// What the dots are coloured and chipped by.
    colour_by: model::ColourBy,
    /// The country chips (colour-by-country), in `model::COUNTRIES` order.
    country_enabled: [bool; model::COUNTRY_COUNT],
    /// The orbit-altitude filter, km (an orbit is in when its
    /// [perigee, apogee] overlaps it), and HEO-only.
    alt_km: (f64, f64),
    heo_only: bool,
    /// The inclination filter, degrees.
    incl_deg: (f64, f64),
    /// The "Distributions" card is open.
    dist_open: bool,
    /// Owners (the SATCAT) have been needed -- country colouring, or a
    /// card's owner line -- so the SATCAT is fetched (once, lazily, under
    /// its 24 h rule) when it is not cached.
    pub owners_wanted: bool,
    /// The crossfilter counts (chips, charts, the status line), recounted
    /// only when a filter, the catalogue or the colouring changes.
    cross: Option<model::CrossCounts>,
    /// How many times the crossfilter was counted (tests).
    pub recounts: u32,
    /// Which chart handle a drag moves: `(altitude chart, low handle)`.
    brushing: Option<(bool, bool)>,
    /// The buttons card's window bounds, recorded each paint: the filter
    /// column's width keeps clear of it.
    buttons_bounds: Rc<Cell<(f32, f32, f32, f32)>>,
    /// The two histograms' `(x, width)` in the window, recorded each paint.
    alt_bounds: Rc<Cell<(f32, f32)>>,
    incl_bounds: Rc<Cell<(f32, f32)>>,
    /// For re-building the basemap when "Earth" changes. Never printed.
    mapbox_token: Option<String>,
    selection: Selection,
    halo: Option<usize>,
    clock: SceneClock,
    /// The rate "play" resumes at (signed; never 0).
    play_rate: f64,
    about_open: bool,
    rings: [Built; 3],
    track: Built,
    /// How many `sat-track-N` overlay sources the track is drawn in.
    track_sources: usize,
    /// The scene second last printed on the time bar.
    last_second: i64,
    /// Ticks that asked for a repaint (the idle test counts them).
    pub repaints: u64,
    /// The centre follow last put the camera at.
    follow_set: Option<(f64, f64)>,
    /// The camera at the last tick: `(lon, lat, zoom, pitch, bearing)`.
    last_camera: Option<(f64, f64, f64, f32, f32)>,
    /// Where a press on the map went down (window px).
    press: Option<(f32, f32)>,
    /// The map's (the panel's) origin in the window, recorded each paint.
    origin: Rc<Cell<(f32, f32)>>,
    /// The scrubber's `(x, width)` in the window, recorded each paint.
    scrub_bounds: Rc<Cell<(f32, f32)>>,
    scrubbing: bool,
    focused_once: bool,
    /// The map-local point the last pointer move asked `altitude_point_at` about.
    pub last_query: Option<(f32, f32)>,
}

impl SatellitesPanel {
    /// The standalone window's panel.
    pub fn with_options(window: &mut Window, cx: &mut Context<Self>, options: PanelOptions) -> Self {
        Self::new(window, cx, options, PanelHost::standalone())
    }

    /// The panel, hosted by `host`. Nothing here blocks: the catalogue is
    /// read and built on the background executor, then (a cache with
    /// `fetch`) each enabled group is refreshed once, off the UI thread,
    /// under the two hour rule; a fetched body reloads the catalogue.
    pub fn new(_window: &mut Window, cx: &mut Context<Self>, options: PanelOptions, host: PanelHost) -> Self {
        let earth = options.earth.unwrap_or(if options.look == AltitudeLook::Glow { model::Earth::Real } else { model::Earth::Map });
        let spec = basemap_spec_earth(options.basemap, host.mapbox_token.as_deref(), earth);
        let raster = spec.raster.clone();
        let sprite_url = spec.sprite_url.clone();
        let (lon, lat, zoom) = options.at;
        let (w, h) = options.size;
        let runtime = host.runtime.clone();
        let clock = SceneClock {
            anchor_scene_unix: options.scene_unix.unwrap_or_else(wall_unix),
            anchor_wall: Instant::now().into(),
            rate: options.rate,
        };
        let map = cx.new(|_| {
            let mut view = MapView::new(lon, lat, zoom.max(gpui_maps::camera_control::MIN_ZOOM), f64::from(w), f64::from(h), spec.style, spec.tiles, runtime.clone());
            view.set_chrome_above_map(true);
            // Opt into the space floor (Task 4 ruling), so "Show GEO" can
            // reach the shell and a pinch can back out to it.
            view.set_zoom_range(SPACE_MIN_ZOOM, MAX_ZOOM);
            if zoom < gpui_maps::camera_control::MIN_ZOOM {
                view.set_camera(lon, lat, zoom);
            }
            view.set_globe_enabled(true);
            view.set_data_credit(Some(DATA_CREDIT.to_string()));
            view.set_scene_clock(Some(clock));
            // The space scene opts into the space look (star map, clouds,
            // sun shading, rim); every other map stays plain. `L` in the
            // gallery turns it off.
            view.set_space_look(gpui_maps::render::space_look::SpaceLook::ON);
            if let Some((source, size)) = raster {
                view.set_raster_source(source, size);
            }
            view
        });
        if let Some(url) = sprite_url {
            crate::kit::spawn_sprites(map.downgrade(), url, None, host.runtime.clone(), cx);
        }
        let keys = crate::shell::DemoKeys::with_satellite_titled(&map, host.title.clone(), None, cx);
        // Follow, the rings' cadence, the hover card and the time label at
        // 30 Hz; 4 Hz when paused and idle (model::tick_interval).
        cx.spawn(async move |this, cx| {
            let mut wait = model::TICK_ACTIVE;
            loop {
                cx.background_executor().timer(wait).await;
                match this.update(cx, |panel, cx| panel.tick(cx)) {
                    Ok(next) => wait = next,
                    Err(_) => return,
                }
            }
        })
        .detach();
        // Every state change the column draws from ends in a `cx.notify()`
        // of this panel (a map frame does not notify it). Here, in the
        // effect flush before the draw, the column is told to rebuild when
        // what it draws from changed -- and only then: the clock's tick,
        // hover, follow, fetch and scrub notify the panel too, and leave
        // the column as it was.
        cx.observe_self(|this, cx| {
            let key = this.column_key();
            if this.column_drawn != Some(key) {
                this.column_drawn = Some(key);
                if let Some(column) = &this.column {
                    column.update(cx, |_, cx| cx.notify());
                }
            }
        })
        .detach();
        let mut panel = Self {
            column: None,
            column_drawn: None,
            map,
            keys,
            data: options.data.clone(),
            cancel: host.cancel.clone(),
            basemap: options.basemap,
            loaded: None,
            loading: true,
            opened: Instant::now(),
            load_ms: None,
            first_dots_ms: None,
            facts: AboutFacts::default(),
            status: None,
            latched: false,
            fetching: false,
            chips: Chips { enabled: options.groups, ..Chips::default() },
            look: options.look,
            trails: options.trails,
            earth,
            colour_by: model::ColourBy::Group,
            country_enabled: [true; model::COUNTRY_COUNT],
            alt_km: (0.0, f64::INFINITY),
            heo_only: false,
            incl_deg: (0.0, 180.0),
            dist_open: false,
            owners_wanted: false,
            cross: None,
            recounts: 0,
            brushing: None,
            buttons_bounds: Rc::new(Cell::new((0.0, 0.0, 0.0, 0.0))),
            alt_bounds: Rc::new(Cell::new((0.0, 0.0))),
            incl_bounds: Rc::new(Cell::new((0.0, 0.0))),
            mapbox_token: host.mapbox_token.clone(),
            selection: Selection::default(),
            halo: None,
            clock,
            play_rate: if options.rate == 0.0 { 1.0 } else { options.rate },
            about_open: host.about_open,
            rings: [None; 3],
            track: None,
            track_sources: 0,
            last_second: 0,
            repaints: 0,
            follow_set: None,
            last_camera: None,
            press: None,
            origin: Rc::new(Cell::new((0.0, 0.0))),
            scrub_bounds: Rc::new(Cell::new((0.0, 0.0))),
            scrubbing: false,
            focused_once: false,
            last_query: None,
        };
        panel.start_load(true, cx);
        panel
    }

    /// Reads the catalogue off the UI thread and attaches it; then, when
    /// `then_fetch` and the source is a fetching cache, refreshes once.
    fn start_load(&mut self, then_fetch: bool, cx: &mut Context<Self>) {
        self.loading = true;
        let data = self.data.clone();
        let task = cx.background_executor().spawn(async move { load(&data) });
        let cancel = self.cancel.clone();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            if cancel.load(Ordering::SeqCst) {
                return;
            }
            this.update(cx, |panel, cx| {
                panel.attach(result, cx);
                if then_fetch {
                    panel.start_fetch(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// One refresh of the enabled groups (a fetching cache only), off the
    /// UI thread; a fetched body reloads the catalogue.
    #[cfg(not(target_arch = "wasm32"))]
    fn start_fetch(&mut self, cx: &mut Context<Self>) {
        let DataSource::Cache { dir, fetch: true } = &self.data else { return };
        let (dir, groups) = (dir.clone(), model::celestrak_groups(self.chips.enabled));
        if groups.is_empty() || self.fetching {
            return;
        }
        self.fetching = true;
        let task = cx.background_executor().spawn(async move { refresh_groups(&dir, &groups) });
        let cancel = self.cancel.clone();
        cx.spawn(async move |this, cx| {
            let (fetched, outcomes) = task.await;
            if cancel.load(Ordering::SeqCst) {
                return;
            }
            eprintln!("satellites: refresh: {}", outcomes.join("; "));
            this.update(cx, |panel, cx| {
                panel.fetching = false;
                if fetched {
                    // A new body: parse and upload it.
                    panel.start_load(false, cx);
                } else {
                    // Nothing new (the common reopen inside two hours): only
                    // the status line and the latch follow the metas the
                    // refresh wrote -- no second parse, no re-upload.
                    panel.refresh_status(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    #[cfg(target_arch = "wasm32")]
    fn start_fetch(&mut self, _cx: &mut Context<Self>) {}

    /// Re-reads the cache's metas (off the UI thread) for the status line
    /// and the latch.
    fn refresh_status(&mut self, cx: &mut Context<Self>) {
        let DataSource::Cache { dir, .. } = &self.data else { return };
        let dir = dir.clone();
        let task = cx.background_executor().spawn(async move { cache_status(&dir) });
        cx.spawn(async move |this, cx| {
            let (_, status, latched) = task.await;
            this.update(cx, |panel, cx| {
                panel.status = status;
                panel.latched = latched;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Hands a loaded catalogue to the map: the keyframed layer (Inertial,
    /// TEME), its style, the chips' mask, the ISS halo; keeps the selection
    /// by NORAD id across a reload.
    fn attach(&mut self, result: LoadResult, cx: &mut Context<Self>) {
        self.loading = false;
        self.load_ms = Some(result.took_ms);
        self.status = result.status;
        self.latched = result.latched;
        let Some(loaded) = result.loaded else {
            // A reload that found nothing keeps what is drawn.
            if self.loaded.is_none() {
                self.map.update(cx, |m, cx| m.remove_altitude(LAYER, cx));
            }
            cx.notify();
            return;
        };
        eprintln!(
            "satellites: {} objects {:?} [stations, starlink, gps, geo, other, galileo, glonass, beidou] built in {:.0} ms, {:.0} ms after open{}",
            loaded.catalog.objects.len(),
            loaded.catalog.group_counts(),
            result.took_ms,
            self.opened.elapsed().as_secs_f64() * 1e3,
            if loaded.synthetic { " (SYNTHETIC)" } else { "" }
        );
        if let Some(old) = self.loaded.as_ref() {
            let (old, new) = (old.catalog.clone(), loaded.catalog.clone());
            self.selection.remap(|i| old.objects.get(i).map(|o| o.record.norad_id), |n| new.index_of(n));
        }
        let catalog = loaded.catalog.clone();
        self.chips.counts = catalog.group_counts();
        self.chips.downloaded = loaded.downloaded;
        self.halo = catalog.index_of(ISS_NORAD);
        self.facts = AboutFacts {
            fetched_unix: result.fetched_unix,
            epochs: catalog.epoch_stats_unix(),
            skipped: loaded.skipped,
            synthetic: loaded.synthetic,
        };
        let style = self.style_of(&loaded);
        let mask = model::filter_mask(&loaded.stats, &self.filters());
        let synthetic = loaded.synthetic;
        self.map.update(cx, |m, cx| {
            let source = Arc::new(CatalogSource(catalog.clone()));
            m.set_altitude_points(LAYER, AltitudePositions::Keyframed { frame: RefFrame::Inertial, source }, style, cx);
            m.set_altitude_visibility(LAYER, &mask);
            m.set_data_credit(Some(if synthetic { SYNTHETIC_CREDIT } else { DATA_CREDIT }.to_string()));
        });
        self.loaded = Some(loaded);
        self.recount();
        // The country chips are always on show: their owners are wanted
        // from the first catalogue (the SATCAT's own 24 h rule holds).
        self.want_owners(cx);
        self.selection.on_mask_changed(&mask);
        // Every ring and track is rebuilt for the new catalogue.
        self.rings = [None; 3];
        self.track = None;
        self.apply_selection(cx);
        cx.notify();
    }

    /// Whether a catalogue is attached.
    pub fn loaded(&self) -> bool {
        self.loaded.is_some()
    }

    /// The map.
    pub fn map(&self) -> &Entity<MapView> {
        &self.map
    }

    /// The chips as they are now.
    pub fn chips(&self) -> Chips {
        self.chips
    }

    /// The planet's look now.
    pub fn earth(&self) -> model::Earth {
        self.earth
    }

    /// Switches the planet between real imagery, the map and the night
    /// map: a new style, and the imagery source set or cleared with it
    /// (the style first, so no frame mounts imagery without its layer).
    pub fn set_earth(&mut self, earth: model::Earth, cx: &mut Context<Self>) {
        self.earth = earth;
        let spec = basemap_spec_earth(self.basemap, self.mapbox_token.as_deref(), earth);
        let restyle = self.loaded.as_ref().map(|l| self.style_of(l));
        let sprite = spec.sprite_url.clone();
        self.map.update(cx, |m, cx| {
            m.set_style(spec.style);
            match spec.raster {
                Some((source, size)) => m.set_raster_source(source, size),
                None => m.clear_raster_source(),
            }
            if let Some(style) = restyle {
                m.set_altitude_style(LAYER, style);
            }
            cx.notify();
        });
        if let Some(url) = sprite {
            crate::kit::spawn_sprites(self.map.downgrade(), url, self.mapbox_token.clone(), runtime_handle(), cx);
        }
        // The rings and the track take the new Earth's look and colours.
        self.rings = [None; 3];
        self.track = None;
        self.refresh_lines(cx);
        cx.notify();
    }

    /// Whether the dots draw motion trails.
    pub fn trails(&self) -> bool {
        self.trails
    }

    /// Turns the trails on or off: a new style for the same layer (the
    /// positions, keyframes and mask are kept).
    pub fn set_trails(&mut self, on: bool, cx: &mut Context<Self>) {
        self.trails = on;
        if let Some(style) = self.loaded.as_ref().map(|l| self.style_of(l)) {
            self.map.update(cx, |m, cx| {
                m.set_altitude_style(LAYER, style);
                cx.notify();
            });
        }
        cx.notify();
    }

    /// The selection as it is now.
    pub fn selection(&self) -> Selection {
        self.selection
    }

    /// The scene time now, Unix seconds.
    pub fn scene_unix(&self) -> f64 {
        self.clock.at(Instant::now())
    }

    /// Whether the ISS's own ring is on: it is drawn, its group shown, and
    /// no other object is selected.
    fn iss_ring_on(&self) -> Option<usize> {
        let iss = self.halo?;
        let f = self.filters();
        let shown = self.loaded.as_ref()?.stats.get(iss).is_some_and(|s| f.passes(s));
        (shown && self.selection.selected.is_none()).then_some(iss)
    }

    /// Pushes the emphasis and brings every ring and track up to date.
    fn apply_selection(&mut self, cx: &mut Context<Self>) {
        if self.selection.hovered.is_some() || self.selection.selected.is_some() {
            self.want_owners(cx);
        }
        let Selection { hovered, selected, .. } = self.selection;
        let halo = self.halo;
        self.map.update(cx, |m, cx| {
            m.set_altitude_emphasis(LAYER, hovered, selected, halo);
            cx.notify();
        });
        self.refresh_lines(cx);
    }

    /// The rings and the ground track this frame wants, rebuilt when their
    /// object changed or [`model::ring_due`] says the scene has moved.
    fn refresh_lines(&mut self, cx: &mut Context<Self>) {
        let Some(catalog) = self.loaded.as_ref().map(|l| l.catalog.clone()) else { return };
        let scene = self.scene_unix();
        let Selection { hovered, selected, .. } = self.selection;
        let iss = self.iss_ring_on();
        let hover = hovered.filter(|h| Some(*h) != selected && Some(*h) != iss);
        let want = [iss, selected, hover];
        // Rings and the track in the scene's look: Flat ("over") on a
        // light Earth, where a screen-blended glow would wash out.
        let (asked, earth) = (self.look, self.earth);
        let look = model::scene_look(asked, earth);
        for (k, (id, width, alpha)) in RING_STYLE.into_iter().enumerate() {
            let fresh = match (want[k], self.rings[k]) {
                (None, None) => true,
                (Some(i), Some((j, t))) => i == j && !model::ring_due(t, scene, model::period_s(&catalog.objects[i]), model::RING_FRACTION),
                _ => false,
            };
            if fresh {
                continue;
            }
            let lines = want[k]
                .and_then(|i| catalog.objects.get(i))
                .map(|obj| vec![AltitudeLine { points: propagate::orbit_ring_teme(obj, model::ring_start(obj, scene), RING_SAMPLES), color: model::scene_rgba(asked, earth, obj.group, alpha), closed: true }])
                .unwrap_or_default();
            self.map.update(cx, |m, cx| {
                if lines.is_empty() {
                    m.remove_altitude(id, cx);
                } else {
                    m.set_altitude_lines_look(id, look, cx);
                    m.set_altitude_lines(id, RefFrame::Inertial, width, lines, cx);
                }
            });
            self.rings[k] = want[k].map(|i| (i, scene));
        }
        // The ground track: the selected object's, else the ISS's.
        let want = selected.or(iss);
        let fresh = match (want, self.track) {
            (None, None) => true,
            (Some(i), Some((j, t))) => i == j && !model::ring_due(t, scene, model::period_s(&catalog.objects[i]), model::TRACK_FRACTION),
            _ => false,
        };
        if !fresh {
            // One overlay source for the whole track, or none: the engine's
            // grid reaches every longitude from one origin.
            let geojsons: Vec<String> = want
                .and_then(|i| catalog.objects.get(i))
                .map(|obj| {
                    let colour = model::scene_hex(asked, earth, obj.group);
                    model::track_geojson(look, &propagate::ground_track(obj, scene), &colour)
                })
                .into_iter()
                .collect();
            let (old, new) = (self.track_sources, geojsons.len());
            self.map.update(cx, |m, cx| {
                for (k, json) in geojsons.iter().enumerate() {
                    if let Err(e) = m.set_source(&track_source(k), json, cx) {
                        eprintln!("satellites: ground track: {e:?}");
                    }
                }
                for k in new..old {
                    m.remove_source(&track_source(k), cx);
                }
            });
            self.track_sources = new;
            self.track = want.map(|i| (i, scene));
        }
    }

    /// 30 Hz: follow, a moving camera hides the hover, the rings' cadence,
    /// the first-dots stamp, and a repaint for the time label. Returns how
    /// long to sleep before the next tick ([`model::tick_interval`]).
    fn tick(&mut self, cx: &mut Context<Self>) -> Duration {
        if self.loaded.is_none() {
            return model::TICK_ACTIVE;
        }
        let (camera, viewport_h) = {
            let m = self.map.read(cx);
            (*m.camera(), m.viewport_px().1)
        };
        let now = (camera.center_lon, camera.center_lat, camera.zoom, camera.pitch, camera.bearing);
        if self.first_dots_ms.is_none() {
            let probe = self.halo.unwrap_or(0);
            if self.map.read(cx).altitude_point_state(LAYER, probe).is_some() {
                let ms = self.opened.elapsed().as_secs_f64() * 1e3;
                self.first_dots_ms = Some(ms);
                eprintln!("satellites: first dots: {ms:.0} ms after open");
            }
        }
        let mut changed = false;
        if self.selection.following {
            if let Some(set) = self.follow_set {
                if model::moved_by_user(set, (camera.center_lon, camera.center_lat)) {
                    self.selection.on_user_camera_move();
                    self.follow_set = None;
                    changed = true;
                }
            }
        }
        if self.selection.following {
            let state = self.selection.selected.and_then(|i| self.map.read(cx).altitude_point_state(LAYER, i));
            match state {
                Some(s) => {
                    let zoom = camera.zoom.min(model::follow_zoom_cap(s.alt_m, s.lat, viewport_h));
                    let set = self.map.update(cx, |m, cx| {
                        m.set_camera(s.lon, s.lat, zoom);
                        cx.notify();
                        let c = m.camera();
                        (c.center_lon, c.center_lat)
                    });
                    self.follow_set = Some(set);
                }
                None => {
                    // Decayed, hidden, or not drawn yet: follow ends.
                    self.selection.following = false;
                    self.follow_set = None;
                    changed = true;
                }
            }
        } else if self.last_camera.is_some_and(|c| c != now) && self.selection.hovered.is_some() {
            // The hover card and ring hide while the camera moves.
            self.selection.hovered = None;
            changed = true;
        }
        let camera_moved = self.last_camera.is_some_and(|c| c != now);
        self.last_camera = Some(now);
        if changed {
            self.apply_selection(cx);
        } else {
            self.refresh_lines(cx);
        }
        // Repaint only for what moves (model::tick_repaints): the printed
        // second, a hover card following its dot on a running clock, follow,
        // a fetch or a scrub. A paused panel asks for nothing, whatever is
        // hovered or selected; the moving dots repaint the map by themselves.
        let second = self.scene_unix().floor() as i64;
        let label_changed = second != self.last_second;
        self.last_second = second;
        let s = self.selection;
        if model::tick_repaints(changed, self.clock.rate, s.hovered.is_some(), s.following, self.fetching || self.scrubbing, label_changed) {
            self.repaints += 1;
            cx.notify();
        }
        // Settled: the first dots are stamped (so the cold-load figure is
        // not a slow tick late), or they never will be soon (a flat map
        // draws no dots until it is zoomed out to the globe).
        let settled = self.first_dots_ms.is_some() || self.opened.elapsed() > Duration::from_secs(10);
        model::tick_interval(self.clock.rate, s.following, self.fetching || self.scrubbing, camera_moved, settled)
    }

    fn toggle_group(&mut self, g: usize, cx: &mut Context<Self>) {
        let mut enabled = self.chips.enabled;
        enabled[g] = !enabled[g];
        self.set_groups(enabled, cx);
    }

    /// Shows exactly the `enabled` groups: one mask push, no re-propagation.
    pub fn set_groups(&mut self, enabled: [bool; GROUP_COUNT], cx: &mut Context<Self>) {
        self.chips.enabled = enabled;
        self.push_mask(cx);
    }

    /// Every filter as it stands: the group chips, the country chips, the
    /// orbit-altitude and inclination ranges.
    pub fn filters(&self) -> model::Filters {
        model::Filters { groups: self.chips.enabled, countries: self.country_enabled, alt_km: self.alt_km, incl_deg: self.incl_deg, heo_only: self.heo_only }
    }

    /// The one mask every filter makes, pushed to the map (flag bits, no
    /// re-propagation); a hidden hovered or selected object is dropped.
    /// The mask and the crossfilter counts come from one call to
    /// [`model::cross_counts`], so a brush drag walks the catalogue once
    /// per event, not twice (the mask used to come from a separate
    /// [`model::filter_mask`] pass, then [`Self::recount`] a second).
    fn push_mask(&mut self, cx: &mut Context<Self>) {
        let cross = self.loaded.as_ref().map(|loaded| {
            let (n, class_of) = self.class_of();
            model::cross_counts(&loaded.stats, &self.filters(), n, class_of)
        });
        if let Some(cross) = &cross {
            self.map.update(cx, |m, cx| {
                m.set_altitude_visibility(LAYER, &cross.mask);
                cx.notify();
            });
            self.selection.on_mask_changed(&cross.mask);
            self.apply_selection(cx);
            self.recounts += 1;
        }
        self.cross = cross;
        cx.notify();
    }

    /// The dots' style for `loaded` in this panel's look, trails, Earth and
    /// colouring.
    fn style_of(&self, loaded: &Loaded) -> gpui_maps::render::altitude_pipeline::AltitudeStyle {
        match self.colour_by {
            model::ColourBy::Group => model::style_for_earth(&loaded.catalog, self.look, self.trails, self.earth),
            model::ColourBy::Country => {
                let countries: Vec<model::Country> = loaded.stats.iter().map(|s| s.country).collect();
                model::style_by_country(&loaded.catalog, &countries, self.look, self.trails, self.earth)
            }
        }
    }

    /// The owners are needed now: the first time, when the loaded
    /// catalogue has none and this panel may fetch, ask CelesTrak for the
    /// SATCAT alone (the fetch policy's 24 h rule for it still holds), and
    /// reload when it lands.
    fn want_owners(&mut self, cx: &mut Context<Self>) {
        if self.owners_wanted {
            return;
        }
        self.owners_wanted = true;
        if self.loaded.as_ref().is_some_and(|l| l.has_owners() || l.synthetic) {
            return;
        }
        self.fetch_satcat(cx);
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn fetch_satcat(&mut self, cx: &mut Context<Self>) {
        let DataSource::Cache { dir, fetch: true } = &self.data else { return };
        let dir = dir.clone();
        let task = cx.background_executor().spawn(async move { refresh_groups(&dir, &[super::fetch::SATCAT]) });
        let cancel = self.cancel.clone();
        cx.spawn(async move |this, cx| {
            let (fetched, outcomes) = task.await;
            if cancel.load(Ordering::SeqCst) {
                return;
            }
            eprintln!("satellites: owners: {}", outcomes.join("; "));
            if fetched {
                this.update(cx, |panel, cx| panel.start_load(false, cx)).ok();
            }
        })
        .detach();
    }

    #[cfg(target_arch = "wasm32")]
    fn fetch_satcat(&mut self, _cx: &mut Context<Self>) {}

    /// Colours the dots, the chips' swatches and the chart stacks by type
    /// or by owner country. A palette only: the filters and the mask are
    /// untouched (the charts restack, so the counts are recounted once).
    pub fn set_colour_by(&mut self, by: model::ColourBy, cx: &mut Context<Self>) {
        self.colour_by = by;
        if by == model::ColourBy::Country {
            self.want_owners(cx);
        }
        self.recount();
        if let Some(style) = self.loaded.as_ref().map(|l| self.style_of(l)) {
            self.map.update(cx, |m, cx| {
                m.set_altitude_style(LAYER, style);
                cx.notify();
            });
        }
        cx.notify();
    }

    /// Shows exactly the `enabled` owner classes.
    pub fn set_countries(&mut self, enabled: [bool; model::COUNTRY_COUNT], cx: &mut Context<Self>) {
        self.country_enabled = enabled;
        self.push_mask(cx);
    }

    /// The orbit-altitude filter: `alt_km` (km) and HEO-only.
    pub fn set_altitude(&mut self, alt_km: (f64, f64), heo_only: bool, cx: &mut Context<Self>) {
        self.alt_km = (alt_km.0.min(alt_km.1), alt_km.0.max(alt_km.1));
        self.heo_only = heo_only;
        self.push_mask(cx);
    }

    /// The inclination filter, degrees.
    pub fn set_inclination(&mut self, incl: (f64, f64), cx: &mut Context<Self>) {
        self.incl_deg = (incl.0.clamp(0.0, 180.0).min(incl.1.clamp(0.0, 180.0)), incl.0.clamp(0.0, 180.0).max(incl.1.clamp(0.0, 180.0)));
        self.push_mask(cx);
    }

    /// A chip's click: toggles group or owner class `i` in `dim`, leaving
    /// every other dimension as it is.
    pub fn toggle_chip(&mut self, dim: ChipDim, i: usize, cx: &mut Context<Self>) {
        match dim {
            ChipDim::Type => {
                if i < GROUP_COUNT {
                    self.toggle_group(i, cx);
                }
            }
            ChipDim::Country => {
                let mut e = self.country_enabled;
                if let Some(v) = e.get_mut(i) {
                    *v = !*v;
                }
                self.set_countries(e, cx);
            }
        }
    }

    /// A dimension's "all" / "none".
    pub fn set_all(&mut self, dim: ChipDim, on: bool, cx: &mut Context<Self>) {
        match dim {
            ChipDim::Type => self.set_groups([on; GROUP_COUNT], cx),
            ChipDim::Country => self.set_countries([on; model::COUNTRY_COUNT], cx),
        }
    }

    /// "reset filters": every dimension open again, one mask push.
    pub fn reset_filters(&mut self, cx: &mut Context<Self>) {
        self.chips.enabled = [true; GROUP_COUNT];
        self.country_enabled = [true; model::COUNTRY_COUNT];
        self.alt_km = (0.0, f64::INFINITY);
        self.heo_only = false;
        self.incl_deg = (0.0, 180.0);
        self.push_mask(cx);
    }

    /// The crossfilter counts as last counted (`None` before a catalogue).
    pub fn cross(&self) -> Option<&model::CrossCounts> {
        self.cross.as_ref()
    }

    /// Recounts the crossfilter: on a filter change, a new catalogue or a
    /// new colouring -- never per frame or per repaint.
    fn recount(&mut self) {
        let Some(loaded) = self.loaded.as_ref() else {
            self.cross = None;
            return;
        };
        let (n, class_of) = self.class_of();
        self.cross = Some(model::cross_counts(&loaded.stats, &self.filters(), n, class_of));
        self.recounts += 1;
    }

    /// The dimension the dots are coloured by.
    fn colour_dim(&self) -> ChipDim {
        match self.colour_by {
            model::ColourBy::Group => ChipDim::Type,
            model::ColourBy::Country => ChipDim::Country,
        }
    }

    /// The panel's colours now (they follow the Earth).
    pub fn theme(&self) -> model::Theme {
        self.earth.theme()
    }

    /// One dimension's chips as drawn: crossfilter counts (what each holds
    /// under every OTHER dimension's filter), on/off, greyed at zero, the
    /// palette's swatch for the colour dimension and a neutral one for the
    /// other.
    pub fn chip_views(&self, dim: ChipDim) -> Vec<ChipView> {
        let palette = self.colour_dim() == dim;
        let theme = self.theme();
        let neutral = rgb(theme.neutral);
        let to = |c: gpui_maps::style::Rgba| gpui::Rgba { r: c.r, g: c.g, b: c.b, a: 1.0 };
        match dim {
            ChipDim::Type => GROUPS
                .into_iter()
                .map(|g| {
                    let i = g as usize;
                    let count = self.chips.downloaded[i].then(|| self.cross.as_ref().map_or(0, |x| x.groups[i]));
                    ChipView {
                        index: i,
                        name: model::group_name(g),
                        count,
                        max_count: self.cross.as_ref().map(|x| x.group_totals[i]),
                        on: self.chips.enabled[i],
                        greyed: count.unwrap_or(0) == 0,
                        colour: if palette { to(model::scene_rgba(self.look, self.earth, g, 1.0)) } else { neutral },
                    }
                })
                .collect(),
            ChipDim::Country => model::COUNTRIES
                .into_iter()
                .map(|c| {
                    let i = c as usize;
                    let count = self.cross.as_ref().map(|x| x.countries[i]);
                    ChipView {
                        index: i,
                        name: model::country_name(c),
                        count,
                        max_count: self.cross.as_ref().map(|x| x.country_totals[i]),
                        on: self.country_enabled[i],
                        greyed: count.unwrap_or(0) == 0,
                        colour: if palette { to(model::country_scene_rgba(c, self.earth, 1.0)) } else { neutral },
                    }
                })
                .collect(),
        }
    }

    /// The loaded catalogue's per-object stats, if any.
    pub fn stats(&self) -> Option<&[model::ObjStats]> {
        self.loaded.as_ref().map(|l| l.stats.as_slice())
    }

    /// Re-anchors the clock at `scene_unix`, now, at `rate`.
    fn set_clock(&mut self, scene_unix: f64, rate: f64, cx: &mut Context<Self>) {
        self.clock = SceneClock { anchor_scene_unix: scene_unix, anchor_wall: Instant::now().into(), rate };
        let clock = self.clock;
        self.map.update(cx, |m, cx| {
            m.set_scene_clock(Some(clock));
            cx.notify();
        });
        self.refresh_lines(cx);
        cx.notify();
    }

    pub fn set_rate(&mut self, rate: f64, cx: &mut Context<Self>) {
        if rate != 0.0 {
            self.play_rate = rate;
        }
        let scene = self.scene_unix();
        self.set_clock(scene, rate, cx);
    }

    fn scrub_at(&mut self, x: f32, cx: &mut Context<Self>) {
        let (x0, w) = self.scrub_bounds.get();
        if w <= 0.0 {
            return;
        }
        let scene = model::scrub_to_unix(wall_unix(), (x - x0) / w);
        let rate = self.clock.rate;
        self.set_clock(scene, rate, cx);
    }

    /// "Now": wall time at 1x.
    fn now(&mut self, cx: &mut Context<Self>) {
        self.play_rate = 1.0;
        self.set_clock(wall_unix(), 1.0, cx);
    }

    fn use_synthetic(&mut self, cx: &mut Context<Self>) {
        self.data = DataSource::Synthetic;
        self.start_load(false, cx);
        cx.notify();
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn retry(&mut self, cx: &mut Context<Self>) {
        if let DataSource::Cache { dir, .. } = &self.data {
            retry_groups(dir);
            self.latched = false;
            self.start_fetch(cx);
        }
        cx.notify();
    }

    #[cfg(target_arch = "wasm32")]
    fn retry(&mut self, _cx: &mut Context<Self>) {}

    /// A pointer move over the map, window px: hover.
    fn on_map_move(&mut self, at: (f32, f32), dragging: bool, cx: &mut Context<Self>) {
        if dragging || self.loaded.is_none() {
            return;
        }
        let (ox, oy) = self.origin.get();
        let local = (at.0 - ox, at.1 - oy);
        self.last_query = Some(local);
        let hit = self.map.read(cx).altitude_point_at(local.0, local.1).filter(|(id, _)| id == LAYER).map(|(_, i)| i);
        if hit != self.selection.hovered {
            self.selection.hovered = hit;
            self.apply_selection(cx);
            cx.notify();
        }
    }

    /// A press released within the click slop on the map: select or deselect.
    fn on_map_click(&mut self, at: (f32, f32), cx: &mut Context<Self>) {
        if self.loaded.is_none() {
            return;
        }
        let (ox, oy) = self.origin.get();
        let local = (at.0 - ox, at.1 - oy);
        self.last_query = Some(local);
        let hit = self.map.read(cx).altitude_point_at(local.0, local.1).filter(|(id, _)| id == LAYER).map(|(_, i)| i);
        self.selection.on_click(hit);
        self.follow_set = None;
        self.apply_selection(cx);
        cx.notify();
    }

    fn on_key(&mut self, event: &gpui::KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.key == "escape" && self.selection.selected.is_some() {
            self.selection.on_click(None);
            self.apply_selection(cx);
            cx.notify();
            return;
        }
        let map = self.map.clone();
        self.keys.on_key(&map, event, window, cx);
    }

    fn about_facts(&self) -> AboutFacts {
        self.facts
    }
}

// ---------------------------------------------------------------- layout

/// The hover card's width, and its height as laid out with its usual lines.
const HOVER_CARD_W: f32 = 250.0;
const HOVER_CARD_H: f32 = 150.0;

/// The filter column's widest and narrowest (it is as wide as the map
/// leaves beside the buttons card, within these).
const COL_MAX_W: f32 = 332.0;
const COL_MIN_W: f32 = 240.0;
/// The column's inset from the map's top-left corner.
const COL_INSET: f32 = 12.0;
/// The bottom band the filter column keeps out of: the time bar
/// (`TIME_BAR_BOTTOM` + ~36), the status row above it (~24), the gaps.
const DIST_BOTTOM_BAND_PX: f32 = TIME_BAR_BOTTOM + 36.0 + 24.0 + 16.0;
/// The histograms' height.
const HIST_H: f32 = 40.0;

/// A filter dimension drawn as chips.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChipDim {
    Type,
    Country,
}

impl ChipDim {
    fn prefix(self) -> &'static str {
        match self {
            ChipDim::Type => "sat-type",
            ChipDim::Country => "sat-country",
        }
    }
}

/// One chip as drawn.
#[derive(Clone, Debug, PartialEq)]
pub struct ChipView {
    /// `group as usize` or `country as usize`.
    pub index: usize,
    pub name: &'static str,
    /// Its crossfilter count; `None` when its list was never downloaded.
    pub count: Option<u32>,
    /// The most it can ever count (its total with no filter), which sizes
    /// the count's slot once: a press elsewhere never changes its width.
    pub max_count: Option<u32>,
    pub on: bool,
    /// Nothing matches it under the other filters: drawn greyed, still
    /// clickable (turning it on shows nothing until another filter opens).
    pub greyed: bool,
    pub colour: gpui::Rgba,
}

fn with_alpha(c: gpui::Rgba, a: f32) -> gpui::Rgba {
    gpui::Rgba { a, ..c }
}

/// Text in a slot as wide as the widest of `current` and `alternatives`
/// (each digit tabular): the others are laid out zero-high and transparent
/// beneath it, so the slot's width is fixed whatever `current` becomes,
/// and a press that changes the text never moves its neighbours.
fn fixed_text(current: impl Into<SharedString>, alternatives: impl IntoIterator<Item = SharedString>, right: bool) -> gpui::Div {
    let tnum = gpui::FontFeatures(std::sync::Arc::new(vec![("tnum".to_string(), 1)]));
    div()
        .flex()
        .flex_col()
        .font_features(tnum)
        .when(right, |d| d.items_end())
        .when(!right, |d| d.items_center())
        .child(div().whitespace_nowrap().child(current.into()))
        .children(alternatives.into_iter().map(|a| div().h(px(0.0)).overflow_hidden().whitespace_nowrap().text_color(gpui::transparent_black()).child(a)))
}

/// [`button`] whose label changes on a press ("Trails on" / "Trails
/// off"): as wide as its widest label, so the press changes colour only.
fn toggle_button(t: &model::Theme, id: impl Into<SharedString>, label: &'static str, labels: &[&'static str], active: bool) -> gpui::Stateful<gpui::Div> {
    let id: SharedString = id.into();
    div()
        .id(id)
        .px(px(7.0))
        .py(px(2.0))
        .rounded(px(4.0))
        .cursor_pointer()
        .text_color(if active { rgb(t.button_on_ink) } else { rgb(t.ink) })
        .when(active, |b| b.bg(rgb(t.button_on)))
        .hover(|s| s.opacity(0.85))
        .child(fixed_text(label, labels.iter().map(|&l| SharedString::from(l)), false))
}

/// A small text button in the theme.
fn button(t: &model::Theme, id: impl Into<SharedString>, label: impl Into<SharedString>, active: bool) -> gpui::Stateful<gpui::Div> {
    let id: SharedString = id.into();
    div()
        .id(id)
        .px(px(7.0))
        .py(px(2.0))
        .rounded(px(4.0))
        .cursor_pointer()
        .text_color(if active { rgb(t.button_on_ink) } else { rgb(t.ink) })
        .when(active, |b| b.bg(rgb(t.button_on)))
        .hover(|s| s.opacity(0.85))
        .child(label.into())
}

/// A small-caps section label (upper case, small, spaced by weight).
fn caps(t: &model::Theme, text: &str) -> gpui::Div {
    div().text_size(px(9.0)).font_weight(FontWeight::SEMIBOLD).text_color(rgb(t.label)).child(text.to_uppercase())
}

/// A theme card: the fill, the corner, the shadow. Every card but the
/// floating hover card also `.occlude()`s the map.
fn card(t: &model::Theme, id: &'static str) -> gpui::Stateful<gpui::Div> {
    div().id(id).debug_selector(move || id.into()).bg(rgba(t.card_a)).rounded(px(6.0)).shadow_md()
}

impl SatellitesPanel {
    /// The filter column's width: what the map leaves between the left
    /// margin and the buttons card (measured last paint; ~210 px before
    /// the first), within [`COL_MIN_W`]..[`COL_MAX_W`].
    fn column_width(&self, cx: &App) -> f32 {
        let (map_w, _) = self.map.read(cx).viewport_px();
        let buttons_w = match self.buttons_bounds.get().2 {
            w if w > 0.0 => w,
            _ => 210.0,
        };
        (map_w - buttons_w - 3.0 * COL_INSET).clamp(COL_MIN_W, COL_MAX_W)
    }

    /// One chip: swatch (filled when on, a ring when off), name, count.
    fn chip(&self, t: &model::Theme, dim: ChipDim, v: ChipView, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let id = format!("{}-{}", dim.prefix(), v.index);
        let count_id = format!("{id}-count");
        let c = v.colour;
        let swatch = div().size(px(7.0)).rounded_full().border_1().border_color(c);
        let swatch = if v.on { swatch.bg(c) } else { swatch };
        let count = v.count.map_or_else(|| "n/a".to_string(), |n| model::thousands(n as usize));
        // The slot fits the chip's unfiltered total (a count never exceeds
        // it); "n/a" (never downloaded) is shown alone, so needs no room.
        let widest = [SharedString::from(model::thousands(v.max_count.unwrap_or(0).max(v.count.unwrap_or(0)) as usize))];
        let i = v.index;
        let hover = rgba(t.hover_a);
        div()
            .id(SharedString::from(id.clone()))
            .debug_selector(move || id.clone())
            .flex()
            .flex_none()
            .items_center()
            .gap(px(4.0))
            .h(px(18.0))
            .px(px(5.0))
            .rounded(px(3.0))
            .border_1()
            .border_color(if v.on { with_alpha(c, 0.6) } else { rgba(t.rule_a) })
            .when(v.on, |d| d.bg(with_alpha(c, if t.dark { 0.12 } else { 0.10 })))
            .when(v.greyed, |d| d.opacity(0.4))
            .text_size(px(10.5))
            .cursor_pointer()
            .hover(move |s| s.border_color(hover))
            .child(swatch)
            .child(div().text_color(if v.on { rgb(t.ink) } else { rgb(t.off) }).child(v.name))
            .child(fixed_text(count, widest, true).debug_selector(move || count_id.clone()).text_size(px(9.5)).text_color(rgb(t.muted)))
            .on_click(cx.listener(move |this, _: &gpui::ClickEvent, _, cx| this.toggle_chip(dim, i, cx)))
    }

    /// A dimension's heading: its small-caps name and "all" / "none".
    fn dim_heading(&self, t: &model::Theme, dim: ChipDim, name: &str, cx: &mut Context<Self>) -> gpui::Div {
        let link = |which: &'static str, on: bool| {
            let id = format!("{}-{which}", dim.prefix());
            div()
                .id(SharedString::from(id.clone()))
                .debug_selector(move || id.clone())
                .cursor_pointer()
                .hover(|s| s.opacity(0.7))
                .child(which.to_uppercase())
                .on_click(cx.listener(move |this, _: &gpui::ClickEvent, _, cx| this.set_all(dim, on, cx)))
        };
        div()
            .flex()
            .items_center()
            .justify_between()
            .pt(px(3.0))
            .child(caps(t, name))
            .child(div().flex().gap(px(8.0)).text_size(px(9.0)).text_color(rgb(t.accent)).child(link("all", true)).child(link("none", false)))
    }

    /// Every filter at once: the status line ("N of M shown", the active
    /// filters, "reset filters"), the type chips, the country chips, and
    /// the view controls ("Colour by", trails, Earth, the charts).
    fn filter_card(&self, t: &model::Theme, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let f = self.filters();
        let active = model::filters_active(&f);
        let head = match self.cross.as_ref() {
            Some(x) => model::shown_line(x.shown as usize, x.total as usize),
            None if self.loading => "Loading the catalogue…".to_string(),
            None => "No catalogue".to_string(),
        };
        let summary_line = model::filter_summary_line(&f);
        let status = div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(8.0))
            .child(div().debug_selector(|| "sat-visible-count".into()).text_size(px(12.5)).font_weight(FontWeight::SEMIBOLD).text_color(rgb(t.ink)).child(head))
            .child(
                div()
                    .id("sat-reset")
                    .debug_selector(|| "sat-reset".into())
                    .flex_none()
                    .text_size(px(9.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(if active { rgb(t.accent) } else { rgb(t.off) })
                    .when(active, |d| d.cursor_pointer().hover(|s| s.opacity(0.7)))
                    .on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| this.reset_filters(cx)))
                    .child("RESET FILTERS"),
            );
        // Always one line, filtered or not: a row that appeared on the
        // first press pushed every chip below it down.
        let summary = div()
            .debug_selector(|| "sat-filter-summary".into())
            .h(px(13.0))
            .w_full()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .text_size(px(10.0))
            .text_color(if active { rgb(t.ink) } else { rgb(t.muted) })
            .child(summary_line);
        let row = |chips: Vec<gpui::Stateful<gpui::Div>>, id: &'static str| div().debug_selector(move || id.into()).flex().flex_wrap().gap(px(4.0)).children(chips);
        let types: Vec<_> = self.chip_views(ChipDim::Type).into_iter().map(|v| self.chip(t, ChipDim::Type, v, cx)).collect();
        let countries: Vec<_> = self.chip_views(ChipDim::Country).into_iter().map(|v| self.chip(t, ChipDim::Country, v, cx)).collect();
        let no_owners = self.loaded.as_ref().is_some_and(|l| !l.has_owners() && !l.synthetic);
        let by_country = self.colour_by == model::ColourBy::Country;
        let seg = |id: &'static str, label: &'static str, on: bool, by: model::ColourBy| {
            button(t, id, label, on).debug_selector(move || id.into()).on_click(cx.listener(move |this, _: &gpui::ClickEvent, _, cx| this.set_colour_by(by, cx)))
        };
        let trails = self.trails;
        let controls = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(4.0))
            .pt(px(6.0))
            .border_t_1()
            .border_color(rgba(t.rule_a))
            .text_size(px(11.0))
            .child(caps(t, "Colour by").mr(px(2.0)))
            .child(seg("sat-colour-type", "type", !by_country, model::ColourBy::Group))
            .child(seg("sat-colour-country", "country", by_country, model::ColourBy::Country))
            .child(div().w(px(1.0)).h(px(12.0)).mx(px(3.0)).bg(rgba(t.rule_a)))
            .child(toggle_button(t, "sat-trails", if trails { "Trails on" } else { "Trails off" }, &["Trails on", "Trails off"], trails).debug_selector(|| "sat-trails".into()).on_click(cx.listener(move |this, _: &gpui::ClickEvent, _, cx| this.set_trails(!trails, cx))))
            .child(toggle_button(t, "sat-earth", self.earth.label(), &model::Earth::ALL.map(model::Earth::label), false).debug_selector(|| "sat-earth".into()).on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| this.set_earth(this.earth.next(), cx))))
            .child(
                toggle_button(t, "sat-dist-toggle", if self.dist_open { "Charts ▴" } else { "Charts ▾" }, &["Charts ▴", "Charts ▾"], self.dist_open)
                    .debug_selector(|| "sat-dist-toggle".into())
                    .on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                        this.dist_open = !this.dist_open;
                        cx.notify();
                    })),
            );
        card(t, "sat-filters")
            .occlude()
            .flex_none()
            .w_full()
            .p(px(10.0))
            .flex()
            .flex_col()
            .gap(px(3.0))
            .child(status)
            .child(summary)
            .child(self.dim_heading(t, ChipDim::Type, "Type", cx))
            .child(row(types, "sat-type-row"))
            .child(self.dim_heading(t, ChipDim::Country, "Country", cx))
            .children(no_owners.then(|| div().text_size(px(9.5)).text_color(rgb(t.muted)).child("Owners not loaded yet: every object counts as \"Other\".")))
            .child(row(countries, "sat-country-row"))
            .child(controls)
    }

    /// The class colours of the current colouring (by class index), as the
    /// scene draws them over this Earth.
    fn class_colours(&self) -> Vec<gpui::Rgba> {
        let to = |c: gpui_maps::style::Rgba| gpui::Rgba { r: c.r, g: c.g, b: c.b, a: 1.0 };
        match self.colour_by {
            model::ColourBy::Group => (0..GROUP_COUNT)
                .map(|i| to(model::scene_rgba(self.look, self.earth, GROUPS.into_iter().find(|g| *g as usize == i).expect("every index"), 1.0)))
                .collect(),
            model::ColourBy::Country => model::COUNTRIES.iter().map(|&c| to(model::country_scene_rgba(c, self.earth, 1.0))).collect(),
        }
    }

    /// The current colouring's class of one object, and how many classes.
    fn class_of(&self) -> (usize, fn(&model::ObjStats) -> usize) {
        match self.colour_by {
            model::ColourBy::Group => (GROUP_COUNT, |s| s.group as usize),
            model::ColourBy::Country => (model::COUNTRY_COUNT, |s| s.country as usize),
        }
    }

    /// A drag on one of the two histograms: `x` in window px moves the
    /// nearer handle (or, mid-drag, the one being dragged).
    fn brush_at(&mut self, altitude: bool, x: f32, start: bool, cx: &mut Context<Self>) {
        let bounds = if altitude { self.alt_bounds.get() } else { self.incl_bounds.get() };
        let f = charts::frac_at(bounds, x);
        let sel = if altitude { self.alt_sel() } else { self.incl_sel() };
        let which = if start { None } else { self.brushing.filter(|b| b.0 == altitude).map(|b| b.1) };
        let (sel, low) = model::brush(sel, f, which);
        self.brushing = Some((altitude, low));
        if altitude {
            self.set_altitude((model::frac_to_alt(sel.0), model::frac_to_alt(sel.1)), false, cx);
        } else {
            self.set_inclination((f64::from(sel.0) * 180.0, f64::from(sel.1) * 180.0), cx);
        }
    }

    /// The altitude range as chart fractions (open ends at the edges).
    fn alt_sel(&self) -> (f32, f32) {
        (if self.alt_km.0 <= 0.0 { 0.0 } else { model::alt_to_frac(self.alt_km.0) }, if self.alt_km.1.is_finite() { model::alt_to_frac(self.alt_km.1) } else { 1.0 })
    }

    fn incl_sel(&self) -> (f32, f32) {
        ((self.incl_deg.0 / 180.0) as f32, (self.incl_deg.1 / 180.0) as f32)
    }

    /// "Charts" (collapsible): the orbit-altitude histogram with its
    /// presets and the inclination histogram, each showing what every
    /// OTHER filter lets through (dimmed) with its own range's part bright,
    /// brushed; and one thin bar per colour class under all the other
    /// filters (click: that class's chip). Everything reads the cached
    /// crossfilter: nothing here counts.
    fn dist_card(&self, t: &model::Theme, width: f32, cx: &mut Context<Self>) -> Option<gpui::Stateful<gpui::Div>> {
        if !self.dist_open {
            return None;
        }
        let x = self.cross.as_ref()?;
        let colours = self.class_colours();
        let alt_chart = charts::Stacked { bins: x.alt.clone(), inside: x.alt_in.clone(), colours: colours.clone() };
        let incl_chart = charts::Stacked { bins: x.incl.clone(), inside: x.incl_in.clone(), colours };
        let chart_w = width - 20.0;
        let axis = |l: &str, r: &str| div().flex().justify_between().text_size(px(9.0)).text_color(rgb(t.muted)).child(l.to_string()).child(r.to_string());
        let current = model::AltPreset::of(&self.filters());
        let presets = model::AltPreset::ALL.into_iter().map(|p| {
            let on = current == Some(p);
            div()
                .id(SharedString::from(format!("sat-alt-{}", p.label())))
                .px(px(5.0))
                .rounded(px(3.0))
                .text_size(px(9.5))
                .cursor_pointer()
                .text_color(if on { rgb(t.on_accent) } else { rgb(t.muted) })
                .when(on, |b| b.bg(rgb(t.accent)))
                .on_click(cx.listener(move |this, _: &gpui::ClickEvent, _, cx| {
                    let (range, heo) = p.range();
                    this.set_altitude(range, heo, cx);
                }))
                .child(p.label())
        });
        let handle = rgba(t.handle_a);
        let hist = |id: &'static str, altitude: bool, chart: charts::Stacked, sel: (f32, f32), bounds: Rc<Cell<(f32, f32)>>| {
            div()
                .id(id)
                .debug_selector(move || id.into())
                .cursor_crosshair()
                .child(charts::stacked_histogram(chart_w, HIST_H, chart, sel, bounds, handle))
                .on_mouse_down(gpui::MouseButton::Left, cx.listener(move |this, e: &gpui::MouseDownEvent, _, cx| this.brush_at(altitude, f32::from(e.position.x), true, cx)))
                .on_mouse_move(cx.listener(move |this, e: &gpui::MouseMoveEvent, _, cx| {
                    if e.dragging() && this.brushing.is_some_and(|b| b.0 == altitude) {
                        this.brush_at(altitude, f32::from(e.position.x), false, cx);
                    }
                }))
                .on_mouse_up(gpui::MouseButton::Left, cx.listener(|this, _: &gpui::MouseUpEvent, _, _| this.brushing = None))
        };
        let dim = self.colour_dim();
        let views = self.chip_views(dim);
        let peak = views.iter().filter_map(|v| v.count).max().unwrap_or(0).max(1);
        let bar_w = chart_w - 120.0;
        let bars = views.into_iter().map(|v| {
            let i = v.index;
            let count = v.count.unwrap_or(0);
            let w = (count as f32 / peak as f32 * bar_w).max(if count > 0 { 1.0 } else { 0.0 });
            div()
                .id(SharedString::from(format!("sat-bar-{i}")))
                .debug_selector(move || format!("sat-bar-{i}"))
                .flex()
                .items_center()
                .gap(px(6.0))
                .h(px(12.0))
                .cursor_pointer()
                .when(v.greyed, |d| d.opacity(0.4))
                .on_click(cx.listener(move |this, _: &gpui::ClickEvent, _, cx| this.toggle_chip(dim, i, cx)))
                .child(div().w(px(58.0)).text_size(px(9.5)).text_color(if v.on { rgb(t.ink) } else { rgb(t.off) }).child(v.name))
                .child(div().w(px(bar_w)).child(div().w(px(w)).h(px(4.0)).rounded(px(1.0)).bg(with_alpha(v.colour, if v.on { 0.95 } else { 0.3 }))))
                .child(div().text_size(px(9.0)).text_color(rgb(t.muted)).child(model::thousands(count as usize)))
        });
        Some(
            card(t, "sat-dist")
                .occlude()
                .flex_none()
                .w_full()
                .p(px(10.0))
                .flex()
                .flex_col()
                .gap(px(3.0))
                .child(div().flex().items_center().justify_between().child(caps(t, "Orbit altitude")).child(div().flex().gap(px(1.0)).children(presets)))
                .child(div().text_size(px(8.5)).text_color(rgb(t.muted)).child("perigee–apogee overlap · bins: mean altitude"))
                .child(hist("sat-alt-hist", true, alt_chart, self.alt_sel(), self.alt_bounds.clone()))
                .child(axis("150 km", "60,000 km"))
                .child(caps(t, "Inclination").pt(px(4.0)))
                .child(hist("sat-incl-hist", false, incl_chart, self.incl_sel(), self.incl_bounds.clone()))
                .child(axis("0°", "180°"))
                .child(caps(t, if dim == ChipDim::Type { "By type · other filters" } else { "By country · other filters" }).pt(px(4.0)))
                .children(bars),
        )
    }

    /// The left column: the filter card and, open, the charts; scrolls
    /// rather than grow into the status row, the time bar and the mark.
    fn filter_column(&self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let t = self.theme();
        let (width, max_h) = self.column_box(cx);
        let filters = self.filter_card(&t, cx);
        let dist = self.dist_card(&t, width, cx);
        div()
            .id("sat-left")
            .debug_selector(|| "sat-left".into())
            .w(px(width))
            .max_h(px(max_h))
            .overflow_y_scroll()
            .occlude()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(filters)
            .children(dist)
    }

    /// How many times the filter column has been built (0 before the
    /// first render).
    pub fn column_builds(&self, cx: &App) -> usize {
        self.column.as_ref().map_or(0, |c| c.read(cx).builds)
    }

    /// Everything the filter column draws from that is this panel's own
    /// state (its box -- width, greatest height, window position -- is in
    /// gpui's own cache key).
    fn column_key(&self) -> ColumnKey {
        ColumnKey {
            filters: self.filters(),
            recounts: self.recounts,
            chips: self.chips,
            dist_open: self.dist_open,
            colour_by: self.colour_by,
            trails: self.trails,
            earth: self.earth,
            look: self.look,
            loading: self.loading,
            catalogue: self.loaded.as_ref().map(|l| (l.has_owners(), l.synthetic)),
        }
    }

    /// The filter column's width and its greatest height.
    fn column_box(&self, cx: &App) -> (f32, f32) {
        let (_, map_h) = self.map.read(cx).viewport_px();
        (self.column_width(cx), (map_h - COL_INSET - DIST_BOTTOM_BAND_PX - 6.0).max(120.0))
    }

    /// The left column, placed: an unpainted box at the column's inset,
    /// as wide as the column and as tall as it may grow, holding the
    /// [`FilterColumn`] view, `.cached()`: last frame's layout and paint,
    /// reused until the column is notified -- by this panel's observer when
    /// a [`ColumnKey`] input changes, or by gpui for its own hover/scroll.
    ///
    /// A map frame marks the map's ancestors dirty, this panel included,
    /// and redraws the window; without the cache every such frame rebuilt
    /// every chip and chart here, the bulk of a frame's gpui work. The
    /// column's hover and scroll notify the column's own view, which gpui
    /// then rebuilds regardless of the cache.
    fn placed_column(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let (width, max_h) = self.column_box(cx);
        let view = match &self.column {
            Some(view) => view.clone(),
            None => {
                let panel = cx.entity().downgrade();
                let view = cx.new(|_| FilterColumn { panel, builds: 0 });
                self.column = Some(view.clone());
                view
            }
        };
        // The observer above has normally seen any change already, and
        // notified the column, which gpui then rebuilds although it is
        // embedded cached. A change it could not see (a notify from inside
        // a draw, which runs no observers) is caught here, and that frame
        // builds the column uncached.
        let key = self.column_key();
        let missed = self.column_drawn != Some(key);
        self.column_drawn = Some(key);
        let view = gpui::AnyView::from(view);
        let view = if missed { view.into_any_element() } else { view.cached(gpui::StyleRefinement::default().size_full()).into_any_element() };
        div().absolute().top(px(COL_INSET)).left(px(COL_INSET)).w(px(width)).h(px(max_h)).child(view)
    }

    fn buttons_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme();
        card(&t, "sat-buttons")
            .occlude()
            .child(bounds_probe(self.buttons_bounds.clone()))
            .absolute()
            .top(px(12.0))
            .right(px(12.0))
            .p(px(6.0))
            .flex()
            .gap(px(4.0))
            .text_size(px(12.0))
            .child(button(&t, "sat-show-geo", "Show GEO", false).on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                this.map.update(cx, |m, cx| {
                    m.zoom_to_fit_shell(model::GEO_RADIUS_M, 0.1);
                    cx.notify();
                });
            })))
            .child(button(&t, "sat-show-leo", "Show LEO", false).on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                this.map.update(cx, |m, cx| {
                    m.zoom_to_fit_shell(model::LEO_RADIUS_M, 0.1);
                    cx.notify();
                });
            })))
            .child(button(&t, "sat-about-toggle", "About", self.about_open).on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                this.about_open = !this.about_open;
                cx.notify();
            })))
    }

    fn about_card(&self) -> Option<impl IntoElement> {
        let t = self.theme();
        self.about_open.then(|| {
            card(&t, "sat-about")
                .occlude()
                .absolute()
                .top(px(52.0))
                .right(px(12.0))
                .w(px(360.0))
                .p(px(10.0))
                .flex()
                .flex_col()
                .gap(px(6.0))
                .text_size(px(11.5))
                .text_color(rgb(t.muted))
                .child(div().font_weight(FontWeight::SEMIBOLD).text_size(px(13.0)).text_color(rgb(t.ink)).child("About this map"))
                .children(about::about_lines(&self.about_facts()).into_iter().map(|l| div().child(l)))
                .child(
                    div()
                        .id("sat-about-celestrak")
                        .cursor_pointer()
                        .text_color(rgb(t.accent))
                        .on_click(|_: &gpui::ClickEvent, _, cx: &mut App| cx.open_url(about::CELESTRAK_URL))
                        .child("celestrak.org"),
                )
                .child(div().child(match self.earth {
                    model::Earth::Real => format!("Earth: Natural Earth II shaded relief (public domain), dimmed, under {} borders.", self.basemap.name()),
                    model::Earth::Night => format!("Basemap: {}, in night colours.", self.basemap.name()),
                    model::Earth::Streets => format!("Earth: {}.", if self.basemap == Basemap::MapboxLight { "Mapbox Streets" } else { "OpenFreeMap Liberty" }),
                    model::Earth::Map => format!("Basemap: {}.", self.basemap.name()),
                }))
                .child(div().child(about::DEFAULTS))
        })
    }

    /// The card of object `i`, `pinned` (with Follow and close) or floating.
    fn object_card(&self, i: usize, pinned: bool, cx: &mut Context<Self>) -> Option<gpui::Stateful<gpui::Div>> {
        let t = self.theme();
        let catalog = self.loaded.as_ref()?.catalog.clone();
        let obj = catalog.objects.get(i)?;
        let state = self.map.read(cx).altitude_point_state(LAYER, i);
        let mut card_text = model::card_for(obj, state.as_ref(), self.scene_unix());
        let owner = self.loaded.as_ref().and_then(|l| l.owners.get(i).cloned().flatten());
        card_text.lines.insert(1, model::owner_line(owner.as_deref()));
        let c = model::scene_rgba(self.look, self.earth, obj.group, 1.0);
        let mut el = card(&t, if pinned { "sat-selected-card" } else { "sat-hover-card" })
            .absolute()
            .w(px(HOVER_CARD_W))
            .p(px(9.0))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .border_l_4()
            .border_color(gpui::Rgba { r: c.r, g: c.g, b: c.b, a: 1.0 })
            .text_size(px(11.5))
            .text_color(rgb(t.muted))
            .child(div().font_weight(FontWeight::SEMIBOLD).text_size(px(12.5)).text_color(rgb(t.ink)).child(card_text.title.clone()))
            .children(card_text.lines.iter().map(|l| div().child(l.clone())))
            .children(card_text.attached.map(|a| div().text_color(rgb(t.label)).child(a)))
            .children(card_text.stale.then(|| div().text_color(rgb(t.warn)).child("Elements over 3 days from the scene: kilometres of error")));
        if pinned {
            let following = self.selection.following;
            el = el.occlude().child(
                div()
                    .flex()
                    .gap(px(6.0))
                    .pt(px(4.0))
                    .child(button(&t, "sat-follow", if following { "Following" } else { "Follow" }, following).on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                        this.selection.following = !this.selection.following;
                        this.follow_set = None;
                        cx.notify();
                    })))
                    .child(button(&t, "sat-deselect", "Close", false).on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                        this.selection.on_click(None);
                        this.apply_selection(cx);
                        cx.notify();
                    }))),
            );
        }
        Some(el)
    }

    fn cards(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let mut out = Vec::new();
        if let Some(i) = self.selection.selected {
            if let Some(card) = self.object_card(i, true, cx) {
                // Right of the filter column, clamped so the card's right
                // edge never runs off the map (below ~610 px map width the
                // column and a full-width card no longer both fit beside
                // it); the buttons card sits higher (top 12) than this
                // card (top 64), so the clamp cannot make them overlap.
                let (map_w, _) = self.map.read(cx).viewport_px();
                let left = (COL_INSET + self.column_width(cx) + 8.0).min(map_w - HOVER_CARD_W - 12.0).max(COL_INSET);
                out.push(card.top(px(64.0)).left(px(left)).into_any_element());
            }
        }
        if let Some(i) = self.selection.hovered.filter(|h| Some(*h) != self.selection.selected) {
            let (at, (w, h)) = {
                let m = self.map.read(cx);
                (m.altitude_point_screen(LAYER, i), m.viewport_px())
            };
            if let (Some((x, y)), Some(card)) = (at, self.object_card(i, false, cx)) {
                // Beside the dot, flipped to its other side near the right
                // or bottom edge so the card stays on the map.
                let left = if x + 14.0 + HOVER_CARD_W > w { x - 14.0 - HOVER_CARD_W } else { x + 14.0 };
                let top = if y + 14.0 + HOVER_CARD_H > h { y - 14.0 - HOVER_CARD_H } else { y + 14.0 };
                out.push(card.top(px(top.max(0.0))).left(px(left.max(0.0))).into_any_element());
            }
        }
        out
    }

    fn no_data_card(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let t = self.theme();
        (!self.loading && self.loaded.is_none()).then(|| {
            card(&t, "sat-no-data")
                .occlude()
                .absolute()
                .top(px(120.0))
                .left(px(12.0))
                .w(px(340.0))
                .p(px(12.0))
                .flex()
                .flex_col()
                .gap(px(8.0))
                .text_size(px(12.0))
                .text_color(rgb(t.muted))
                .child(div().font_weight(FontWeight::SEMIBOLD).text_size(px(13.0)).text_color(rgb(t.ink)).child("No orbital data yet"))
                .child(div().child(self.status.clone().unwrap_or_else(|| "No cached CelesTrak data and no file given.".to_string())))
                .child(
                    button(&t, "sat-synthetic-button", "Use synthetic data", true)
                        .debug_selector(|| "sat-synthetic-button".into())
                        .on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| this.use_synthetic(cx))),
                )
        })
    }

    /// The fetch status, the staleness chip and the synthetic label, just
    /// above the time bar (the shown count heads the filter card).
    fn status_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme();
        let scene = self.scene_unix();
        let stale = self.facts.epochs.and_then(|(_, _, newest)| model::staleness_chip(newest, scene));
        let mut row = div()
            .id("sat-status")
            .debug_selector(|| "sat-status".into())
            .absolute()
            .left(px(TIME_BAR_LEFT))
            .bottom(px(self.time_bar_bottom(cx) + 40.0))
            .flex()
            .gap(px(6.0))
            .text_size(px(11.5));
        let pill = |text: String, bg: u32, fg: u32| div().px(px(7.0)).py(px(2.0)).rounded(px(4.0)).bg(rgba(bg)).text_color(rgb(fg)).child(text);
        if self.facts.synthetic {
            row = row.child(pill("SYNTHETIC".to_string(), t.synthetic_bg_a, t.synthetic_ink));
        }
        if self.loading {
            row = row.child(pill("Loading the catalogue…".to_string(), t.card_a, t.muted));
        } else if self.fetching {
            row = row.child(pill("Asking CelesTrak…".to_string(), t.card_a, t.muted));
        }
        if let Some(status) = self.status.clone().filter(|_| self.loaded.is_some()) {
            row = row.child(pill(status, t.card_a, t.muted));
        }
        if self.latched {
            row = row.child(button(&t, "sat-retry", "Retry", true).occlude().on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| this.retry(cx))));
        }
        if let Some(text) = stale {
            row = row.child(pill(text, t.warn_bg_a, t.warn));
        }
        row
    }

    /// The time bar's bottom offset: [`TIME_BAR_BOTTOM`], or above the
    /// credit plate when its stacked credits (basemap, imagery, orbits,
    /// stars, clouds) stand taller than that (`MapView::attribution_bounds`)
    /// -- the bar spans the map's width, so it would cover them.
    fn time_bar_bottom(&self, cx: &App) -> f32 {
        let m = self.map.read(cx);
        let (_, map_h) = m.viewport_px();
        let plate = m.attribution_bounds().map_or(0.0, |(_, y, _, _)| map_h - y + 4.0);
        TIME_BAR_BOTTOM.max(plate)
    }

    fn time_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme();
        let rate = self.clock.rate;
        let playing = rate != 0.0;
        let backwards = if playing { rate < 0.0 } else { self.play_rate < 0.0 };
        let speed = if playing { rate.abs() } else { self.play_rate.abs() };
        let speeds = model::SPEEDS[1..].iter().map(|&s| {
            let active = (speed - s).abs() < 1e-9;
            button(&t, SharedString::from(format!("sat-speed-{s}")), format!("{s}×"), active).on_click(cx.listener(move |this, _: &gpui::ClickEvent, _, cx| {
                let sign = if this.play_rate < 0.0 { -1.0 } else { 1.0 };
                this.set_rate(sign * s, cx);
            }))
        });
        let scene = self.scene_unix();
        let fraction = model::scrub_fraction(wall_unix(), scene);
        let probe_bounds = self.scrub_bounds.clone();
        let scrubber = div()
            .id("sat-scrub")
            .relative()
            .flex_1()
            .min_w(px(80.0))
            .h(px(18.0))
            .cursor_pointer()
            .child(
                gpui::canvas(
                    move |bounds, _, _| probe_bounds.set((f32::from(bounds.origin.x), f32::from(bounds.size.width))),
                    |_, _, _, _| {},
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
            .child(div().absolute().top(px(8.0)).left_0().right_0().h(px(2.0)).rounded(px(1.0)).bg(rgb(t.track)))
            .child(div().absolute().top(px(3.0)).left(relative(0.5)).w(px(1.0)).h(px(12.0)).bg(rgb(t.label)))
            .child(div().absolute().top(px(3.0)).left(relative(fraction)).ml(px(-6.0)).size(px(12.0)).rounded_full().bg(rgb(t.thumb)))
            .on_mouse_down(gpui::MouseButton::Left, cx.listener(|this, e: &gpui::MouseDownEvent, _, cx| {
                this.scrubbing = true;
                this.scrub_at(f32::from(e.position.x), cx);
            }))
            .on_mouse_move(cx.listener(|this, e: &gpui::MouseMoveEvent, _, cx| {
                if this.scrubbing && e.dragging() {
                    this.scrub_at(f32::from(e.position.x), cx);
                }
            }));
        // The id is spelled here, not only inside `card`, for the gallery's
        // brand-mark scan (`every_bottom_anchored_panel_card_is_accounted_for`).
        card(&t, "sat-time-bar")
            .id("sat-time-bar")
            .occlude()
            .absolute()
            .left(px(TIME_BAR_LEFT))
            .right(px(12.0))
            .bottom(px(self.time_bar_bottom(cx)))
            .p(px(6.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .text_size(px(12.0))
            .child(button(&t, "sat-play", if playing { "Pause" } else { "Play" }, false).on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                let rate = if this.clock.rate == 0.0 { this.play_rate } else { 0.0 };
                this.set_rate(rate, cx);
            })))
            .child(button(&t, "sat-reverse", if backwards { "◀ back" } else { "▶ fwd" }, backwards).on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                this.play_rate = -this.play_rate;
                if this.clock.rate != 0.0 {
                    let rate = this.play_rate;
                    this.set_rate(rate, cx);
                }
                cx.notify();
            })))
            .children(speeds)
            .child(scrubber)
            .child(div().min_w(px(170.0)).text_color(rgb(t.ink)).child(model::format_utc(scene)))
            .child(button(&t, "sat-now", "Now", false).on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| this.now(cx))))
    }

    /// The map, in a box pinned to the panel's top-left corner. Its first
    /// child records its origin (the map's) for `altitude_point_at`; its
    /// capture-phase press arms a click only when the map itself is under
    /// the pointer (every card `.occlude()`s).
    fn map_area(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("sat-map-area")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .capture_any_mouse_down(cx.listener(|this, e: &gpui::MouseDownEvent, _, _| {
                this.press = Some((f32::from(e.position.x), f32::from(e.position.y)));
            }))
            .on_mouse_move(cx.listener(|this, e: &gpui::MouseMoveEvent, _, cx| {
                this.on_map_move((f32::from(e.position.x), f32::from(e.position.y)), e.pressed_button.is_some(), cx);
            }))
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !*hovered && this.selection.hovered.is_some() {
                    this.selection.hovered = None;
                    this.apply_selection(cx);
                    cx.notify();
                }
            }))
            .when(self.selection.hovered.is_some(), |area| area.cursor_pointer())
            .child(origin_probe(self.origin.clone()))
            .child(self.map_child())
    }

    /// The map. In a test window a stand-in laid out and hit-tested as
    /// `MapView`'s element is: a test window has no platform layer for it.
    fn map_child(&self) -> gpui::AnyElement {
        #[cfg(not(test))]
        return self.map.clone().into_any_element();
        #[cfg(test)]
        return div().size_full().on_mouse_down(gpui::MouseButton::Left, |_, _, _| {}).into_any_element();
    }
}

impl Render for SatellitesPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focused_once {
            if let Some(handle) = self.map.read(cx).focus_handle() {
                window.focus(&handle, cx);
                self.focused_once = true;
            }
        }
        let column = self.placed_column(cx);
        let buttons = self.buttons_card(cx);
        let about = self.about_card();
        let cards = self.cards(cx);
        let no_data = self.no_data_card(cx);
        let status = self.status_row(cx);
        let bar = self.time_bar(cx);
        // NO background on the root: the map draws into its own
        // CAMetalLayer BEHIND gpui's content, so an opaque root covers it
        // (captures and draw counters cannot see that). Pinned by
        // `the_root_paints_no_background_over_the_map`.
        div()
            .id("sat-root")
            .size_full()
            .relative()
            .on_key_down(cx.listener(Self::on_key))
            .capture_any_mouse_up(cx.listener(|this, e: &gpui::MouseUpEvent, _, cx| {
                this.scrubbing = false;
                let at = (f32::from(e.position.x), f32::from(e.position.y));
                if let Some(down) = this.press.take() {
                    if model::is_click(down, at) {
                        this.on_map_click(at, cx);
                    }
                }
            }))
            .child(self.map_area(cx))
            .child(column)
            .child(buttons)
            .children(about)
            .children(cards)
            .children(no_data)
            .child(status)
            .child(bar)
    }
}

/// What the filter column shows, as [`SatellitesPanel::column_key`] reads
/// it: a change rebuilds the column, anything else reuses it.
#[derive(Clone, Copy, PartialEq)]
struct ColumnKey {
    filters: model::Filters,
    /// The crossfilter counts (every chip's and chart's number) change
    /// only on a recount.
    recounts: u32,
    chips: Chips,
    dist_open: bool,
    colour_by: model::ColourBy,
    trails: bool,
    earth: model::Earth,
    look: AltitudeLook,
    loading: bool,
    /// `(has_owners, synthetic)` of the loaded catalogue.
    catalogue: Option<(bool, bool)>,
}

/// The panel's filter column as a view of its own, so the window can
/// reuse it across map frames: see [`SatellitesPanel::placed_column`]. Its
/// content is still built by the panel (`filter_column`), whose listeners
/// it carries.
pub struct FilterColumn {
    panel: gpui::WeakEntity<SatellitesPanel>,
    /// How many times it has been built (a reused frame never calls
    /// `render`), for the redraw tests.
    builds: usize,
}

impl Render for FilterColumn {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.builds += 1;
        match self.panel.upgrade() {
            Some(panel) => panel.update(cx, |p, cx| p.filter_column(cx)).into_any_element(),
            None => div().into_any_element(),
        }
    }
}

/// An unpainted element pinned over the map that records the map's window
/// origin at each prepaint (pointer events arrive in window pixels).
/// Records its parent's window bounds `(x, y, w, h)` each paint.
fn bounds_probe(cell: Rc<Cell<(f32, f32, f32, f32)>>) -> impl IntoElement {
    gpui::canvas(
        move |b, _, _| cell.set((f32::from(b.origin.x), f32::from(b.origin.y), f32::from(b.size.width), f32::from(b.size.height))),
        |_, _, _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

fn origin_probe(origin: Rc<Cell<(f32, f32)>>) -> impl IntoElement {
    gpui::canvas(move |bounds, _, _| origin.set((f32::from(bounds.origin.x), f32::from(bounds.origin.y))), |_, _, _, _| {})
        .absolute()
        .top_0()
        .left_0()
        .size_full()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_maps::orbit::catalog::Group;

    /// The map is a CAMetalLayer under gpui's content: any `.bg(` on the
    /// panel root (from `render` to the map child) or the map's box hides it.
    #[test]
    fn the_root_paints_no_background_over_the_map() {
        let src = include_str!("panel.rs");
        let needle = [".", "bg", "("].concat();
        let start = src.find(&["impl Render for ", "SatellitesPanel"].concat()).expect("the render impl");
        let end = start + src[start..].find(".child(self.map_area(cx))").expect("the map child");
        assert!(!src[start..end].contains(&needle), "the panel root sets a background, which covers the map");
        let area = &src[src.find(&["fn ", "map_area("].concat()).expect("map_area")..src.find(&["fn ", "map_child("].concat()).expect("map_child")];
        assert!(!area.contains(&needle), "the map's box sets a background");
    }

    /// A host window that draws the panel 900x700 at window `at`, as the
    /// gallery embeds it, around a real `MapView` with no basemap.
    mod window {
        use super::super::*;
        use gpui::{Modifiers, MouseButton, TestAppContext, VisualTestContext};

        pub struct Host {
            pub panel: Entity<SatellitesPanel>,
        }

        pub struct HostView {
            pub host: Host,
            pub at: (f32, f32),
            pub size: (f32, f32),
        }

        impl Render for HostView {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div().size_full().pl(px(self.at.0)).pt(px(self.at.1)).child(div().w(px(self.size.0)).h(px(self.size.1)).child(self.host.panel.clone()))
            }
        }

        /// Where the test panel's data comes from.
        pub struct Sources(pub DataSource);

        impl Sources {
            /// No cache directory and no fetch: never the network.
            pub fn empty_cache_no_fetch() -> Self {
                Self(DataSource::Cache { dir: PathBuf::from("/nonexistent/gpui-map-satellites-cache"), fetch: false })
            }

            /// A cache holding only the committed `stations` fixture.
            pub fn stations_cache(dir: &std::path::Path) -> Self {
                std::fs::create_dir_all(dir).unwrap();
                std::fs::write(dir.join("stations.json"), include_bytes!("../../tests/fixtures/orbit/stations.json")).unwrap();
                Self(DataSource::Cache { dir: dir.to_path_buf(), fetch: false })
            }
        }

        pub fn host_at(cx: &mut TestAppContext, at: (f32, f32)) -> (Entity<HostView>, &mut VisualTestContext) {
            host_at_with(cx, at, Sources::empty_cache_no_fetch())
        }

        pub fn host_at_with(cx: &mut TestAppContext, at: (f32, f32), sources: Sources) -> (Entity<HostView>, &mut VisualTestContext) {
            host_sized(cx, at, (900.0, 700.0), sources)
        }

        /// The panel at `size` (map px), at window offset `at`.
        pub fn host_sized(cx: &mut TestAppContext, at: (f32, f32), size: (f32, f32), sources: Sources) -> (Entity<HostView>, &mut VisualTestContext) {
            let (host, cx) = cx.add_window_view(|window, cx| {
                let options = PanelOptions { basemap: Basemap::None, data: sources.0, size, ..PanelOptions::default() };
                let panel = cx.new(|cx| SatellitesPanel::with_options(window, cx, options));
                HostView { host: Host { panel }, at, size }
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

        /// A pointer move to window `(x, y)`, and a frame.
        pub fn hover(cx: &mut VisualTestContext, x: f32, y: f32) {
            cx.simulate_mouse_move(gpui::point(px(x), px(y)), None, Modifiers::default());
            draw(cx);
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

        pub fn panel(cx: &mut VisualTestContext, host: &Entity<HostView>) -> Entity<SatellitesPanel> {
            host.read_with(cx, |h, _| h.host.panel.clone())
        }
    }

    /// The time bar is the panel's card anchored to the map's bottom edge:
    /// it starts right of the SWAT mark's footprint, and its bottom is above
    /// the mark's band, at the gallery's offset.
    #[test]
    fn the_time_bar_keeps_clear_of_the_brand_mark() {
        let mut app = gpui::TestAppContext::single();
        let at = (220.0, 40.0);
        let (_host, cx) = window::host_at(&mut app, at);
        window::draw(cx);
        let b = cx.debug_bounds("sat-time-bar").expect("the time bar is drawn");
        let (mx, my, mw, _mh) = gpui_maps::brand_mark::BrandMark::default().footprint(700.0);
        assert!(f32::from(b.origin.x) - at.0 >= mx + mw, "the time bar starts at {} over the mark's right edge {}", f32::from(b.origin.x) - at.0, mx + mw);
        let bottom = f32::from(b.origin.y) + f32::from(b.size.height) - at.1;
        assert!(bottom <= my, "the time bar reaches down to {bottom}, over the mark's top at {my}");
        assert!(f32::from(b.size.width) > 300.0, "the bar spans the map");
        // The status row above it starts at the same left edge.
        let s = cx.debug_bounds("sat-status").expect("the status row is laid out");
        assert!(f32::from(s.origin.x) - at.0 >= mx + mw && f32::from(s.origin.y) + f32::from(s.size.height) - at.1 <= my);
    }

    #[test]
    fn the_panel_asks_for_the_space_zoom_floor_and_credits_celestrak() {
        let mut app = gpui::TestAppContext::single();
        let (host, cx) = window::host_at(&mut app, (0.0, 0.0));
        let (range, credit) = host.read_with(cx, |h, cx| {
            let m = h.host.panel.read(cx).map.read(cx);
            (m.zoom_range(), m.data_credit().map(str::to_string))
        });
        assert_eq!(range.0, gpui_maps::camera_control::SPACE_MIN_ZOOM);
        assert!(credit.unwrap_or_default().contains("Orbits: CelesTrak / US Space Force"));
    }

    #[test]
    fn with_no_cache_and_no_network_the_panel_offers_synthetic_data() {
        let mut app = gpui::TestAppContext::single();
        let (host, cx) = window::host_at_with(&mut app, (0.0, 0.0), window::Sources::empty_cache_no_fetch());
        window::draw(cx);
        assert!(cx.debug_bounds("sat-no-data").is_some() && cx.debug_bounds("sat-synthetic-button").is_some());
        // The button loads the seeded set, labelled SYNTHETIC.
        let b = cx.debug_bounds("sat-synthetic-button").unwrap();
        window::click(cx, f32::from(b.origin.x) + 4.0, f32::from(b.origin.y) + 4.0);
        window::draw(cx);
        let panel = window::panel(cx, &host);
        let (loaded, chips, credit) = panel.read_with(cx, |p, cx| (p.loaded(), p.chips(), p.map.read(cx).data_credit().map(str::to_string)));
        assert!(loaded, "the synthetic set attached");
        assert!(chips.counts.iter().sum::<usize>() > 1_000 && chips.downloaded == [true; GROUP_COUNT], "{chips:?}");
        assert!(credit.unwrap_or_default().contains("SYNTHETIC"));
    }

    /// A cache with only the stations list: the stations chip counts 11,
    /// the other chips say "not downloaded", the ISS carries the halo and
    /// its ring and track are on.
    #[test]
    fn a_stations_only_cache_draws_the_stations_with_the_iss_ring() {
        let dir = std::env::temp_dir().join(format!("gpui-map-sat-panel-test-{}", std::process::id()));
        let mut app = gpui::TestAppContext::single();
        let (host, cx) = window::host_at_with(&mut app, (0.0, 0.0), window::Sources::stations_cache(&dir));
        window::draw(cx);
        let panel = window::panel(cx, &host);
        let (chips, halo, rings, track) = panel.read_with(cx, |p, _| (p.chips(), p.halo, p.rings, p.track));
        assert_eq!(chips.label(Group::Stations), "Stations 11");
        assert_eq!(chips.label(Group::Gps), "GPS: not downloaded");
        assert_eq!(chips.label(Group::Starlink), "Starlink: not downloaded");
        assert!(halo.is_some(), "the ISS is drawn with its halo");
        assert_eq!(rings[0].map(|r| r.0), halo, "the ISS ring is on");
        assert_eq!(track.map(|t| t.0), halo, "the ISS track is on");
        assert_eq!(panel.read_with(cx, |p, _| p.track_sources), 1, "a 1.5-orbit track is one source");
        // Hiding the stations drops the ISS ring and track.
        panel.update(cx, |p, cx| p.toggle_group(Group::Stations as usize, cx));
        let (rings, track) = panel.read_with(cx, |p, _| (p.rings, p.track));
        assert!(rings[0].is_none() && track.is_none());
        assert_eq!(panel.read_with(cx, |p, _| p.track_sources), 0, "every track source removed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The gallery's layout: the panel at window (220, 40). A pointer at
    /// window (220 + 450, 40 + 350) asks `altitude_point_at` about map
    /// point (450, 350), not the window point.
    #[test]
    fn at_the_gallery_offset_the_pointer_is_asked_about_in_map_pixels() {
        let dir = std::env::temp_dir().join(format!("gpui-map-sat-panel-offset-{}", std::process::id()));
        let mut app = gpui::TestAppContext::single();
        let (host, cx) = window::host_at_with(&mut app, (220.0, 40.0), window::Sources::stations_cache(&dir));
        window::hover(cx, 670.0, 390.0);
        let panel = window::panel(cx, &host);
        assert_eq!(panel.read_with(cx, |p, _| p.last_query), Some((450.0, 350.0)));
        // A click on bare map deselects, asked about at the same map point.
        panel.update(cx, |p, _| p.selection.selected = Some(0));
        window::click(cx, 320.0, 500.0);
        assert_eq!(panel.read_with(cx, |p, _| (p.last_query, p.selection.selected)), (Some((100.0, 460.0)), None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review F3: a paused panel with a hover and a selection requests no
    /// repaints from its tick; a running one with a hover does. Final
    /// review M2: and its timer slows to [`model::TICK_IDLE`].
    #[test]
    fn a_paused_panel_with_a_selection_is_idle() {
        let dir = std::env::temp_dir().join(format!("gpui-map-sat-panel-idle-{}", std::process::id()));
        let mut app = gpui::TestAppContext::single();
        let (host, cx) = window::host_at_with(&mut app, (0.0, 0.0), window::Sources::stations_cache(&dir));
        let panel = window::panel(cx, &host);
        panel.update(cx, |p, cx| {
            p.set_rate(0.0, cx);
            p.selection.selected = Some(0);
            p.selection.hovered = Some(1);
            p.first_dots_ms = Some(0.0); // headless: nothing paints dots
            p.tick(cx); // settles the printed second and the camera
            p.repaints = 0;
            for _ in 0..30 {
                assert_eq!(p.tick(cx), model::TICK_IDLE, "a paused, idle panel ticks slowly");
            }
        });
        assert_eq!(panel.read_with(cx, |p, _| p.repaints), 0, "a paused panel repainted");
        panel.update(cx, |p, cx| {
            p.set_rate(60.0, cx);
            assert_eq!(p.tick(cx), model::TICK_ACTIVE, "running: 30 Hz again");
        });
        assert!(panel.read_with(cx, |p, _| p.repaints) > 0, "the positive control: running with a hover repaints");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The group chips (Starlink's among them) set the mask (no reload),
    /// drop a hovered or selected object of a hidden group, and put the
    /// count on the status line; "Trails" restyles the layer;
    /// paused with a filter on, the panel still asks for no frames.
    #[test]
    fn the_starlink_chip_and_the_trails_toggle() {
        let mut app = gpui::TestAppContext::single();
        let (host, cx) = window::host_at_with(&mut app, (0.0, 0.0), window::Sources::empty_cache_no_fetch());
        let panel = window::panel(cx, &host);
        panel.update(cx, |p, cx| p.use_synthetic(cx));
        window::draw(cx);
        assert!(panel.read_with(cx, |p, _| p.loaded()), "the synthetic set attached");
        let (counts, starlink_obj, other_obj) = panel.read_with(cx, |p, _| {
            let cat = &p.loaded.as_ref().unwrap().catalog;
            let find = |starlink: bool| cat.objects.iter().position(|o| (o.group == Group::Starlink) == starlink).unwrap();
            (cat.group_counts(), find(true), find(false))
        });
        let starlink = counts[Group::Starlink as usize];
        let total: usize = counts.iter().sum();
        assert!(panel.read_with(cx, |p, _| p.trails()), "trails default on in the panel");
        panel.update(cx, |p, cx| {
            p.selection.hovered = Some(other_obj);
            p.selection.selected = Some(other_obj);
            p.set_groups(model::only(Group::Starlink), cx);
        });
        let (shown, sel) = panel.read_with(cx, |p, _| (p.cross().map(|x| x.shown as usize), p.selection));
        assert_eq!(shown, Some(starlink));
        assert_eq!((sel.hovered, sel.selected), (None, None), "a hidden object is neither hovered nor selected");
        panel.update(cx, |p, cx| {
            p.set_groups([true; GROUP_COUNT], cx);
            p.selection.hovered = Some(starlink_obj);
            p.toggle_group(Group::Starlink as usize, cx); // the Starlink chip, off
        });
        let (shown, sel) = panel.read_with(cx, |p, _| (p.cross().map(|x| x.shown as usize), p.selection));
        assert_eq!(shown, Some(total - starlink));
        assert_eq!(sel.hovered, None);
        panel.update(cx, |p, cx| p.set_groups([true; GROUP_COUNT], cx));
        assert_eq!(panel.read_with(cx, |p, _| p.cross().map(|x| x.shown as usize)), Some(total));
        panel.update(cx, |p, cx| p.set_trails(false, cx));
        assert!(!panel.read_with(cx, |p, _| p.trails()));
        panel.update(cx, |p, cx| p.set_trails(true, cx));
        assert!(panel.read_with(cx, |p, _| p.trails()));
        // "Earth" cycles real -> streets -> map -> night -> real.
        let start = panel.read_with(cx, |p, _| p.earth());
        assert_eq!(start, model::Earth::Real, "glow opens on the real Earth");
        assert_eq!(panel.read_with(cx, |p, _| p.theme()), model::Theme::DARK, "the real Earth: the dark instrument theme");
        for want in [model::Earth::Streets, model::Earth::Map, model::Earth::Night, model::Earth::Real] {
            panel.update(cx, |p, cx| p.set_earth(p.earth().next(), cx));
            assert_eq!(panel.read_with(cx, |p, _| p.earth()), want);
            // The theme follows the Earth; every card reads it.
            let theme = panel.read_with(cx, |p, _| p.theme());
            assert_eq!(theme, if want.is_light() { model::Theme::LIGHT } else { model::Theme::DARK }, "{want:?}");
            window::draw(cx);
            assert!(cx.debug_bounds("sat-filters").is_some() && cx.debug_bounds("sat-time-bar").is_some());
        }
        // Paused, filtered, with trails on: idle.
        panel.update(cx, |p, cx| {
            p.set_rate(0.0, cx);
            p.set_groups(model::only(Group::Starlink), cx);
            p.first_dots_ms = Some(0.0);
            p.tick(cx);
            p.repaints = 0;
            for _ in 0..30 {
                assert_eq!(p.tick(cx), model::TICK_IDLE);
            }
        });
        assert_eq!(panel.read_with(cx, |p, _| p.repaints), 0, "a paused, filtered panel repainted");
    }

    /// A rect `(x0, y0, x1, y1)` in map px of a laid-out element.
    fn rect(cx: &mut gpui::VisualTestContext, at: (f32, f32), id: &str) -> Option<(f32, f32, f32, f32)> {
        cx.debug_bounds(leak(id)).map(|b| {
            let x = f32::from(b.origin.x) - at.0;
            let y = f32::from(b.origin.y) - at.1;
            (x, y, x + f32::from(b.size.width), y + f32::from(b.size.height))
        })
    }

    /// `debug_bounds` takes a `&'static str`; the chip ids are built.
    fn leak(id: &str) -> &'static str {
        Box::leak(id.to_string().into_boxed_str())
    }

    fn apart(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> bool {
        a.2 <= b.0 || b.2 <= a.0 || a.3 <= b.1 || b.3 <= a.1
    }

    /// The filter column at 480x480, 640x420, 760x560 and 900x700, at the
    /// gallery's offset, charts closed and open, coloured by type and by
    /// country: every type and country chip is laid out inside the filter
    /// card, on rows that wrap without overlap; the column stays inside
    /// the map and clear of the buttons, the status row, the time bar, the
    /// brand mark, the compass and the attribution, scrolling rather than
    /// growing; a bar click toggles its chip. Review F3: at 480 px (below
    /// the ~610 px the naive `left = column + 8` used to run the pinned
    /// card off the right edge) the pinned card also stays on the map and
    /// clear of the buttons card.
    #[test]
    fn the_filter_column_fits_wraps_and_keeps_clear_at_every_size() {
        let at = (220.0, 40.0);
        for size in [(480.0, 480.0), (640.0, 420.0), (760.0, 560.0), (900.0, 700.0)] {
            let mut app = gpui::TestAppContext::single();
            let (host, cx) = window::host_sized(&mut app, at, size, window::Sources::empty_cache_no_fetch());
            let panel = window::panel(cx, &host);
            panel.update(cx, |p, cx| p.use_synthetic(cx));
            window::draw(cx);
            window::draw(cx);
            for (dist, by) in [(false, model::ColourBy::Group), (true, model::ColourBy::Group), (true, model::ColourBy::Country), (false, model::ColourBy::Country)] {
                panel.update(cx, |p, cx| {
                    p.dist_open = dist;
                    p.set_colour_by(by, cx);
                });
                window::draw(cx);
                window::draw(cx);
                let what = format!("{size:?} charts {dist} {by:?}");
                let left = rect(cx, at, "sat-left").expect("the column");
                let card = rect(cx, at, "sat-filters").expect("the filter card");
                assert!(left.0 >= 0.0 && left.1 >= 0.0 && left.2 <= size.0 && left.3 <= size.1, "{what}: column inside the map {left:?}");
                assert!(card.0 >= left.0 - 0.5 && card.2 <= left.2 + 0.5, "{what}: the card is as wide as the column");
                let mut chips = Vec::new();
                for dim in ["sat-type", "sat-country"] {
                    for i in 0..8 {
                        let r = rect(cx, at, &format!("{dim}-{i}")).unwrap_or_else(|| panic!("{what}: {dim}-{i} laid out"));
                        assert!(r.0 >= card.0 && r.2 <= card.2, "{what}: {dim}-{i} {r:?} wraps inside the card {card:?}");
                        assert!(r.3 - r.1 <= 20.0, "{what}: {dim}-{i} is one line high");
                        chips.push((format!("{dim}-{i}"), r));
                    }
                }
                for (i, (a, ra)) in chips.iter().enumerate() {
                    for (b, rb) in &chips[i + 1..] {
                        assert!(apart(*ra, *rb), "{what}: {a} {ra:?} overlaps {b} {rb:?}");
                    }
                }
                if !dist {
                    assert!(card.3 <= left.3 + 0.5, "{what}: the filter card fits without scrolling ({} > {})", card.3, left.3);
                }
                let buttons = rect(cx, at, "sat-buttons").expect("buttons");
                let bar = rect(cx, at, "sat-time-bar").expect("time bar");
                let status = rect(cx, at, "sat-status").expect("status");
                let mark = {
                    let (x, y, w, h) = gpui_maps::brand_mark::BrandMark::default().footprint(size.1);
                    (x, y, x + w, y + h)
                };
                let (cx0, cy0, cs) = gpui_maps::compass::compass_rect(size.0, 0.0);
                let compass = (cx0, cy0, cx0 + cs, cy0 + cs);
                let credit = (size.0 * 0.4, size.1 - TIME_BAR_BOTTOM + 4.0, size.0, size.1);
                for (other, o) in [("buttons", buttons), ("time bar", bar), ("status", status), ("brand mark", mark), ("compass", compass), ("attribution", credit)] {
                    assert!(apart(left, o), "{what}: the column {left:?} overlaps the {other} {o:?}");
                }
                if dist {
                    assert!(rect(cx, at, "sat-alt-hist").is_some() && rect(cx, at, "sat-incl-hist").is_some(), "{what}: both charts");
                    let n = if by == model::ColourBy::Group { GROUP_COUNT } else { model::COUNTRY_COUNT };
                    assert!(rect(cx, at, &format!("sat-bar-{}", n - 1)).is_some(), "{what}: one bar per class");
                }
            }
            // Review F3: the pinned card (a selected object) stays on the
            // map and clear of the buttons card, even at 480 px where the
            // column crowds the right edge.
            panel.update(cx, |p, _| p.selection.selected = Some(0));
            window::draw(cx);
            if let Some(pin) = rect(cx, at, "sat-selected-card") {
                let buttons = rect(cx, at, "sat-buttons").expect("buttons");
                assert!(pin.0 >= 0.0 && pin.2 <= size.0, "{size:?}: the pinned card stays on the map {pin:?}");
                assert!(apart(pin, buttons), "{size:?}: the pinned card {pin:?} overlaps the buttons card {buttons:?}");
            }
            panel.update(cx, |p, _| p.selection.selected = None);
            // A bar click toggles its chip (the charts open, by type).
            panel.update(cx, |p, cx| {
                p.dist_open = true;
                p.set_colour_by(model::ColourBy::Group, cx);
            });
            window::draw(cx);
            let gps = Group::Gps as usize;
            let b = rect(cx, at, "sat-bar-2").expect("the GPS bar");
            let left = rect(cx, at, "sat-left").unwrap();
            if b.1 + 8.0 <= left.3 {
                window::click(cx, at.0 + b.0 + 4.0, at.1 + b.1 + 4.0);
            } else {
                panel.update(cx, |p, cx| p.toggle_chip(ChipDim::Type, gps, cx));
            }
            assert!(!panel.read_with(cx, |p, _| p.chips().enabled[gps]), "{size:?}: the bar turned GPS off");
        }
    }

    /// Stats with every owner class present (the synthetic set has no
    /// SATCAT): object `i` is owned by class `i mod 8`.
    fn spread_owners(panel: &Entity<SatellitesPanel>, cx: &mut gpui::VisualTestContext) {
        panel.update(cx, |p, cx| {
            if let Some(l) = p.loaded.as_mut() {
                for (i, s) in l.stats.iter_mut().enumerate() {
                    s.country = model::COUNTRIES[i % model::COUNTRY_COUNT];
                }
            }
            p.push_mask(cx);
        });
    }

    /// Pressing a control never changes the filter card's layout (user
    /// report: deselecting Starlink inserted a summary line and shrank the
    /// country chips): the card, every chip, every link and every button
    /// keep the exact bounds they had before the press, through real
    /// clicks on each -- a type chip, a country chip, ALL / NONE, RESET
    /// FILTERS, both COLOUR BY toggles, Trails, Earth and Charts.
    #[test]
    fn pressing_a_filter_control_never_moves_the_layout() {
        let at = (220.0, 40.0);
        let mut app = gpui::TestAppContext::single();
        let (host, cx) = window::host_sized(&mut app, at, (900.0, 700.0), window::Sources::empty_cache_no_fetch());
        let panel = window::panel(cx, &host);
        panel.update(cx, |p, cx| p.use_synthetic(cx));
        window::draw(cx);
        spread_owners(&panel, cx);
        window::draw(cx);
        let mut ids: Vec<String> = ["sat-filters", "sat-filter-summary", "sat-reset", "sat-type-row", "sat-country-row", "sat-type-all", "sat-type-none", "sat-country-all", "sat-country-none", "sat-colour-type", "sat-colour-country", "sat-trails", "sat-earth", "sat-dist-toggle"]
            .into_iter()
            .map(String::from)
            .collect();
        for g in GROUPS {
            ids.push(format!("sat-type-{}", g as usize));
            ids.push(format!("sat-type-{}-count", g as usize));
        }
        for c in model::COUNTRIES {
            ids.push(format!("sat-country-{}", c as usize));
            ids.push(format!("sat-country-{}-count", c as usize));
        }
        let measure = |cx: &mut gpui::VisualTestContext| -> Vec<(String, (f32, f32, f32, f32))> {
            ids.iter().map(|id| (id.clone(), rect(cx, at, id).unwrap_or_else(|| panic!("{id} is drawn")))).collect()
        };
        let before = measure(cx);
        let summary = |cx: &mut gpui::VisualTestContext| panel.read_with(cx, |p, _| model::filter_summary_line(&p.filters()));
        assert_eq!(summary(cx), model::NO_FILTER_SUMMARY, "unfiltered, the line is reserved with neutral text");
        let starlink = format!("sat-type-{}", Group::Starlink as usize);
        let presses = [
            starlink.as_str(),
            "sat-country-0",
            "sat-type-all",
            "sat-country-none",
            "sat-country-all",
            "sat-type-none",
            "sat-reset",
            starlink.as_str(),
            "sat-colour-country",
            "sat-colour-type",
            "sat-trails",
            "sat-trails",
            "sat-earth",
            "sat-dist-toggle",
            "sat-dist-toggle",
            "sat-reset",
        ];
        let mut saw_filtered = false;
        for id in presses {
            let b = cx.debug_bounds(leak(id)).unwrap_or_else(|| panic!("{id} drawn"));
            let state = panel.read_with(cx, |p, _| (p.filters(), p.colour_by, p.trails, p.earth, p.dist_open));
            window::click(cx, f32::from(b.origin.x) + f32::from(b.size.width) / 2.0, f32::from(b.origin.y) + f32::from(b.size.height) / 2.0);
            window::draw(cx);
            let after = panel.read_with(cx, |p, _| (p.filters(), p.colour_by, p.trails, p.earth, p.dist_open));
            // "reset filters" with nothing filtered does nothing, by design.
            if !(id == "sat-reset" && !model::filters_active(&state.0)) {
                assert_ne!(after, state, "pressing {id} changed nothing: the click missed");
            }
            saw_filtered |= model::filters_active(&after.0);
            let now = measure(cx);
            for ((name, was), (_, is)) in before.iter().zip(&now) {
                assert_eq!(was, is, "pressing {id} moved or resized {name}");
            }
        }
        assert!(saw_filtered, "some press filtered the set");
    }

    /// The crossfilter in the panel, driven through its chips at the
    /// gallery's offset: a chip's count is its class under every OTHER
    /// dimension; a zero chip is greyed and still clickable; a brush on the
    /// altitude chart moves the chip counts and the inclination chart;
    /// "Colour by" changes the palette only; a filtered-out hovered or
    /// selected object is dropped; a repaint counts nothing.
    #[test]
    fn the_chips_brush_and_colour_drive_one_crossfilter() {
        let at = (220.0, 40.0);
        let mut app = gpui::TestAppContext::single();
        let (host, cx) = window::host_sized(&mut app, at, (900.0, 700.0), window::Sources::empty_cache_no_fetch());
        let panel = window::panel(cx, &host);
        panel.update(cx, |p, cx| p.use_synthetic(cx));
        window::draw(cx);
        spread_owners(&panel, cx);
        window::draw(cx);
        let click = |cx: &mut gpui::VisualTestContext, id: &str| {
            let b = cx.debug_bounds(leak(id)).unwrap_or_else(|| panic!("{id} drawn"));
            window::click(cx, f32::from(b.origin.x) + 3.0, f32::from(b.origin.y) + 3.0);
        };
        // China only, through "none" and the China chip.
        click(cx, "sat-country-none");
        click(cx, "sat-country-1");
        let (f, x, stats) = panel.read_with(cx, |p, _| (p.filters(), p.cross().cloned().unwrap(), p.stats().unwrap().to_vec()));
        assert_eq!(f.countries.iter().filter(|&&c| c).count(), 1);
        assert!(f.countries[model::Country::China as usize]);
        for g in GROUPS {
            let want = stats.iter().filter(|s| s.group == g && f.pass_country(s) && f.pass_alt(s) && f.pass_incl(s)).count();
            assert_eq!(x.groups[g as usize] as usize, want, "{g:?} under China only");
        }
        for c in model::COUNTRIES {
            let want = stats.iter().filter(|s| s.country == c).count();
            assert_eq!(x.countries[c as usize] as usize, want, "{c:?}: its own dimension does not narrow it");
        }
        assert_eq!(x.shown as usize, stats.iter().filter(|s| s.country == model::Country::China).count());
        assert!(cx.debug_bounds("sat-filter-summary").is_some(), "the active filters are summarised");
        // A hovered and a selected Starlink object, then the Starlink chip
        // off: both dropped.
        let starlink = stats.iter().position(|s| s.group == Group::Starlink && s.country == model::Country::China).expect("a Chinese Starlink in the spread");
        panel.update(cx, |p, _| {
            p.selection.hovered = Some(starlink);
            p.selection.selected = Some(starlink);
        });
        click(cx, &format!("sat-type-{}", Group::Starlink as usize));
        let sel = panel.read_with(cx, |p, _| p.selection);
        assert_eq!((sel.hovered, sel.selected), (None, None), "a filtered-out object is neither hovered nor selected");
        click(cx, "sat-reset");
        assert_eq!(panel.read_with(cx, |p, _| p.filters()), model::Filters::default(), "reset filters");
        // GEO only: Starlink counts 0 -> greyed, and still clickable.
        panel.update(cx, |p, cx| {
            let (r, heo) = model::AltPreset::Geo.range();
            p.set_altitude(r, heo, cx);
        });
        window::draw(cx);
        let starlink_chip = panel.read_with(cx, |p, _| p.chip_views(ChipDim::Type).into_iter().find(|v| v.index == Group::Starlink as usize).unwrap());
        assert!(starlink_chip.greyed && starlink_chip.on && starlink_chip.count == Some(0), "{starlink_chip:?}");
        assert!(panel.read_with(cx, |p, _| p.chip_views(ChipDim::Type).iter().any(|v| !v.greyed)), "the GEO chip is not greyed");
        click(cx, &format!("sat-type-{}", Group::Starlink as usize));
        assert!(!panel.read_with(cx, |p, _| p.chips().enabled[Group::Starlink as usize]), "a greyed chip still toggles");
        click(cx, &format!("sat-type-{}", Group::Starlink as usize));
        click(cx, "sat-reset");
        // Brush: a press on the altitude chart at 2,000 km moves the nearer
        // (low) handle there: LEO leaves, the counts and the inclination
        // chart follow, the altitude chart keeps its context.
        panel.update(cx, |p, cx| {
            p.dist_open = true;
            cx.notify();
        });
        window::draw(cx);
        window::draw(cx);
        let before = panel.read_with(cx, |p, _| p.cross().cloned().unwrap());
        let recounts = panel.read_with(cx, |p, _| p.recounts);
        let h = cx.debug_bounds("sat-alt-hist").expect("the altitude chart");
        let (hx, hw) = panel.read_with(cx, |p, _| p.alt_bounds.get());
        assert!((hx - f32::from(h.origin.x)).abs() < 1.0 && hw > 100.0, "the chart's bounds are recorded in window px");
        let y = f32::from(h.origin.y) + 10.0;
        let xw = hx + hw * model::alt_to_frac(2_000.0);
        cx.simulate_mouse_move(gpui::point(px(xw), px(y)), None, gpui::Modifiers::default());
        cx.simulate_mouse_down(gpui::point(px(xw), px(y)), gpui::MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_up(gpui::point(px(xw), px(y)), gpui::MouseButton::Left, gpui::Modifiers::default());
        window::draw(cx);
        let (alt_km, after) = panel.read_with(cx, |p, _| (p.alt_km, p.cross().cloned().unwrap()));
        assert!((alt_km.0 - 2_000.0).abs() < 150.0 && alt_km.1.is_infinite(), "brushed to {alt_km:?}");
        assert_eq!(after.groups[Group::Starlink as usize], 0, "LEO left the Starlink chip");
        assert!(after.groups[Group::Gps as usize] > 0 && after.groups[Group::Gps as usize] == before.groups[Group::Gps as usize]);
        assert!(after.shown < before.shown);
        assert_eq!(after.alt, before.alt, "the brushed chart keeps its context");
        assert_ne!(after.incl, before.incl, "the inclination chart follows the brush");
        assert!(panel.read_with(cx, |p, _| p.recounts) > recounts);
        // "Colour by: country": the palette only.
        let (f0, mask0) = panel.read_with(cx, |p, _| (p.filters(), model::filter_mask(p.stats().unwrap(), &p.filters())));
        click(cx, "sat-colour-country");
        let (f1, mask1, by_country) = panel.read_with(cx, |p, _| (p.filters(), model::filter_mask(p.stats().unwrap(), &p.filters()), p.cross().cloned().unwrap()));
        assert_eq!(panel.read_with(cx, |p, _| p.colour_by), model::ColourBy::Country);
        assert_eq!((f0, &mask0), (f1, &mask1), "colour-by never changes the filters or the mask");
        assert_eq!((by_country.groups, by_country.countries, by_country.shown), (after.groups, after.countries, after.shown));
        assert_eq!(by_country.alt[0].len(), model::COUNTRY_COUNT, "the stacks are by country now");
        let swatches = panel.read_with(cx, |p, _| (p.chip_views(ChipDim::Type)[0].colour, p.chip_views(ChipDim::Country)[0].colour));
        assert_eq!(swatches.0, rgb(panel.read_with(cx, |p, _| p.theme().neutral)), "the type chips go neutral");
        assert_ne!(swatches.1, swatches.0, "the country chips carry the palette");
        // A repaint recounts nothing.
        let recounts = panel.read_with(cx, |p, _| p.recounts);
        for _ in 0..3 {
            window::draw(cx);
        }
        assert_eq!(panel.read_with(cx, |p, _| p.recounts), recounts, "a repaint recounts nothing");
    }

    /// The altitude and inclination filters and the country chips all go
    /// into the one mask the map holds; a hovered or selected object they
    /// hide is dropped (so it is neither hovered nor hit), and a paused,
    /// filtered panel asks for no frames.
    #[test]
    fn the_orbit_filters_and_country_chips_mask_the_dots() {
        let mut app = gpui::TestAppContext::single();
        let (host, cx) = window::host_at_with(&mut app, (0.0, 0.0), window::Sources::empty_cache_no_fetch());
        let panel = window::panel(cx, &host);
        panel.update(cx, |p, cx| p.use_synthetic(cx));
        window::draw(cx);
        let (leo, geo) = panel.read_with(cx, |p, _| {
            let st = p.stats().unwrap();
            (st.iter().position(|s| s.apogee_km < 2_000.0).unwrap(), st.iter().position(|s| s.perigee_km > 35_000.0 && s.apogee_km < 36_500.0).unwrap())
        });
        panel.update(cx, |p, cx| {
            p.selection.hovered = Some(leo);
            p.selection.selected = Some(geo);
            let (r, heo) = model::AltPreset::Geo.range();
            p.set_altitude(r, heo, cx);
        });
        let sel = panel.read_with(cx, |p, _| p.selection);
        assert_eq!((sel.hovered, sel.selected), (None, Some(geo)), "the LEO object is filtered out, the GEO one kept");
        let shown = panel.read_with(cx, |p, _| p.stats().unwrap().iter().filter(|s| p.filters().passes(s)).count());
        let geo_n = panel.read_with(cx, |p, _| p.stats().unwrap().iter().filter(|s| model::overlaps(35_586.0, 35_986.0, s.perigee_km, s.apogee_km)).count());
        assert_eq!(shown, geo_n);
        panel.update(cx, |p, cx| p.set_inclination((30.0, 40.0), cx));
        assert_eq!(panel.read_with(cx, |p, _| p.selection.selected), None, "a GEO satellite is not inclined 30-40 degrees");
        panel.update(cx, |p, cx| {
            p.set_altitude((0.0, f64::INFINITY), false, cx);
            p.set_inclination((0.0, 180.0), cx);
            let mut c = [false; model::COUNTRY_COUNT];
            c[model::Country::Other as usize] = true;
            p.set_countries(c, cx);
        });
        let all_other = panel.read_with(cx, |p, _| p.stats().unwrap().iter().filter(|s| p.filters().passes(s)).all(|s| s.country == model::Country::Other));
        assert!(all_other);
        panel.update(cx, |p, cx| {
            p.set_rate(0.0, cx);
            p.first_dots_ms = Some(0.0);
            p.tick(cx);
            p.repaints = 0;
            let recounts = p.recounts;
            for _ in 0..20 {
                assert_eq!(p.tick(cx), model::TICK_IDLE);
            }
            assert_eq!(p.recounts, recounts, "an idle tick counts nothing");
        });
        assert_eq!(panel.read_with(cx, |p, _| p.repaints), 0, "a paused, filtered panel repainted");
    }

    /// Review F4: the country chips are always on show, so `attach` wants
    /// the SATCAT owner catalogue from the very first catalogue -- not
    /// only once the panel is switched to "Colour by: country" (the old
    /// lazy behaviour). `owners_wanted` is a once-per-panel latch: a
    /// second attach (a reload) does not ask again.
    #[test]
    fn attach_wants_the_satcat_owner_catalogue() {
        let mut app = gpui::TestAppContext::single();
        let (host, cx) = window::host_at(&mut app, (0.0, 0.0));
        let panel = window::panel(cx, &host);
        assert!(!panel.read_with(cx, |p, _| p.owners_wanted), "no catalogue attached yet");
        panel.update(cx, |p, cx| p.use_synthetic(cx));
        window::draw(cx);
        assert!(
            panel.read_with(cx, |p, _| p.owners_wanted),
            "the owners are wanted from the first attach, before any colour-by-country click"
        );
        // A second attach (a reload of the same synthetic set) leaves the
        // latch as it is.
        panel.update(cx, |p, cx| p.use_synthetic(cx));
        window::draw(cx);
        assert!(panel.read_with(cx, |p, _| p.owners_wanted));
    }

    /// The clock: pause keeps the scene still, a speed runs it, reverse
    /// runs it backwards, "Now" is wall time at 1x.
    #[test]
    fn the_time_bar_drives_the_scene_clock() {
        let mut app = gpui::TestAppContext::single();
        let (host, cx) = window::host_at(&mut app, (0.0, 0.0));
        let panel = window::panel(cx, &host);
        let rate = |cx: &mut gpui::VisualTestContext| panel.read_with(cx, |p, cx| (p.clock.rate, p.map.read(cx).scene_clock().map(|c| c.rate)));
        panel.update(cx, |p, cx| p.set_rate(0.0, cx));
        assert_eq!(rate(cx), (0.0, Some(0.0)));
        panel.update(cx, |p, cx| p.set_rate(-1000.0, cx));
        assert_eq!(rate(cx), (-1000.0, Some(-1000.0)));
        panel.update(cx, |p, cx| p.now(cx));
        assert_eq!(rate(cx), (1.0, Some(1.0)));
        let scene = panel.read_with(cx, |p, _| p.scene_unix());
        assert!((scene - wall_unix()).abs() < 5.0);
    }
}

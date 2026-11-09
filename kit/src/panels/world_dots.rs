//! Every person on Earth as a dot: the windowed body of
//! `demos/world_dots.rs` -- where the dots come from, the basemap under
//! them, and the window root (`DemoRoot`: the map, the demo keys, the live
//! epoch keys and the counter over the map). Moved here unchanged (gallery
//! Task 13) so the gallery can host it as a panel (Task 14);
//! `demos/world_dots.rs` keeps its `Args`, `main`, every headless mode
//! (`--measure`, `--capture`, `--diag`, ...), the `*_task`s and
//! `windowed`, and reaches everything below through
//! `#[path = "../kit/src/panels/world_dots.rs"] mod world_dots_panel;`.
//! This file is the ONE copy: the demo keeps none of its own.
//!
//! The gallery's `world-dots` panel ([`build`], Task 14) is built from the
//! same pieces: the basemap ([`routes_open`], keyless), the
//! dot style ([`dot_style`]), the root ([`DemoRoot::embedded`]) and the
//! counter line ([`counter_line`]).
//!
//! Crate paths: `crate::shell` is `kit/src/shell.rs` and
//! `crate::kit` is `kit/src/kit.rs`, which every includer declares
//! at its crate root.
//!
//! `#![allow(dead_code)]`: this file is included by two crates that each
//! use a different half of it -- the demo its headless modes' helpers
//! (`PRESETS`, `open_source`, `StyleArgs`, `table_arcsec`, ...), the
//! gallery its panel entry points ([`build`], [`requirements`],
//! [`CREDITS`]) -- so every item is dead in one of them. The same
//! file-level allow `kit/src/panels/big_points.rs` carries for the same
//! reason; `cargo test` compiles both crates and runs this file's tests in
//! each.
//!
//! **The one `#[path]` inside a panel:** the synthetic world
//! (`demos/world_dots/synthetic.rs`) is included from here, not from the
//! demo, so each crate that reaches it (the demo now, the gallery later)
//! includes it exactly once, through this file. `synthetic.rs` is private
//! to this demo and is included nowhere else. Every other panel reaches
//! shared code through `crate::` paths its includer declares.
#![allow(dead_code)]

#[path = "../../../demos/world_dots/synthetic.rs"]
pub mod synthetic;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::prelude::FluentBuilder as _;
use gpui::{div, px, App, AppContext as _, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render, Styled, WeakEntity, Window};
use gpui_maps::map_view::MapView;
#[cfg(feature = "clickhouse-demo")]
use gpui_maps::overlay::dotgrid::clickhouse::ClickHouseGridSource;
use gpui_maps::overlay::dotgrid::fetch_native;
use gpui_maps::overlay::dotgrid::layer::{DotRamp, DotStyle};
use gpui_maps::overlay::dotgrid::GridCountSource;
use gpui_maps::sprite::redact_url_credentials;
use gpui_maps::tile_source::http::HttpTileSource;
use gpui_maps::tile_source::TileSource;

use crate::{kit, shell};

/// What [`style_of`] reads: the `--budget`, `--shrink`, `--fade`,
/// `--relocate`, `--k` and `--epochs` flags (`demos/world_dots.rs`'s
/// `parse_args` fills one in its `Args`).
#[derive(Clone, Copy, Debug, Default)]
pub struct StyleArgs {
    /// `--budget N`: `DotStyle::budget_dots`.
    pub budget: Option<u32>,
    /// `--shrink F`: `DotStyle::density_shrink` (default 0.3; 0 draws
    /// every dot at the zoom's radius, as before the density shrink).
    pub shrink: Option<f32>,
    /// `--fade G`: `DotStyle::overlap_gamma` (0 = no overlap fade).
    pub fade: Option<f32>,
    /// `--relocate on|off`: `DotStyle::relocate` (default on).
    pub relocate: Option<bool>,
    /// `--k N`: `DotStyle::mask_k`, candidates per dot while relocating.
    pub k: Option<u32>,
    /// `--epochs A,B`: live epochs, `DotStyle::{epoch, epoch_b}`.
    pub epochs: Option<(u16, u16)>,
}

pub const LAYER: &str = "population";
pub const W: u32 = 1920;
pub const H: u32 = 1080;

#[derive(Clone, Copy, Debug)]
pub struct Preset {
    pub name: &'static str,
    pub lon: f64,
    pub lat: f64,
    pub zoom: f64,
}

pub const PRESETS: [Preset; 4] = [
    Preset { name: "world_z1", lon: 0.0, lat: 20.0, zoom: 1.0 },
    Preset { name: "india_z5", lon: 78.9, lat: 22.0, zoom: 5.0 },
    Preset { name: "dhaka_z10", lon: 90.4125, lat: 23.8103, zoom: 10.0 },
    Preset { name: "dhaka_z15", lon: 90.4125, lat: 23.8103, zoom: 15.0 },
];

/// Where the Gallery page opens: Eurasia, India and East Africa facing
/// (the user's pick, 2026-10-09), the zoom set by [`home_zoom`] so the
/// globe fills [`HOME_GLOBE_FRACTION`] of the map's height. Not a
/// `PRESETS` entry: those are the perf gate's measured views, `world_z1`
/// among them.
pub const GALLERY_HOME: Preset = Preset { name: "gallery_home", lon: 60.0, lat: 38.0, zoom: 1.4 };

/// The globe's diameter over the map's height at [`GALLERY_HOME`].
pub const HOME_GLOBE_FRACTION: f64 = 0.98;

/// The zoom at which the globe, centred at `lat`, is `fraction` of `h`
/// pixels across: its radius is `WORLD_SIZE * 2^z / (2 pi cos lat)`
/// (`render::globe::globe_radius_world`). [`GALLERY_HOME`]'s zoom when the
/// map has no size yet.
#[must_use]
pub fn home_zoom(h: f64, lat: f64, fraction: f64) -> f64 {
    if h < 1.0 {
        return GALLERY_HOME.zoom;
    }
    let world = f64::from(gpui_maps::render::WORLD_SIZE);
    let diameter = h * fraction;
    (diameter * std::f64::consts::PI * lat.to_radians().cos() / world).log2().clamp(0.0, 4.0)
}

pub fn preset(at: &str) -> Preset {
    match at {
        "world" => PRESETS[0],
        "india" => PRESETS[1],
        "dhaka" => PRESETS[2],
        "dhaka15" => PRESETS[3],
        "paris15" => Preset { name: "paris_z15", lon: 2.3522, lat: 48.8566, zoom: 15.0 },
        "amsterdam13" => Preset { name: "amsterdam_z13", lon: 4.9041, lat: 52.3676, zoom: 13.0 },
        "cairo14" => Preset { name: "cairo_z14", lon: 31.2357, lat: 30.0444, zoom: 14.0 },
        "sahel12" => Preset { name: "sahel_z12", lon: 3.0, lat: 14.0, zoom: 12.0 },
        "nizhny14" => Preset { name: "nizhny_z14", lon: 43.99, lat: 56.31, zoom: 14.0 },
        // The user's street view (2026-09-27).
        "nizhny16" => Preset { name: "nizhny_z16.5", lon: 43.995, lat: 56.318, zoom: 16.5 },
        other => panic!("--at takes world|india|dhaka|dhaka15|paris15|amsterdam13|cairo14|sahel12|nizhny14|nizhny16, got {other}"),
    }
}

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

/// The saved connection the gallery's `world-dots-clickhouse` reads
/// through: Studio resolves it on its Connections page
/// ([`kit::PanelEnv::clickhouse`]) and seeds it at
/// `http://localhost:8123` when no connection has the name.
pub const CLICKHOUSE_CONNECTION: &str = "Local ClickHouse";

/// The table the gallery's ClickHouse page reads, in `gpui_map_dots`.
pub const CLICKHOUSE_TABLE: &str = "ghs_cells_30ss";

/// The script that loads [`CLICKHOUSE_TABLE`] (named for the agent and the
/// docs; the missing-data card shows no script).
pub const CLICKHOUSE_LOAD: &str = "scripts/load-ghsl-epochs.sh";

/// The standalone demo's `--clickhouse` with no URL (never the gallery,
/// which reads [`CLICKHOUSE_CONNECTION`]): `CLICKHOUSE_URL`, else a local
/// server.
pub fn default_clickhouse_url() -> String {
    std::env::var("CLICKHOUSE_URL").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| "http://localhost:8123".to_string())
}

pub fn default_grid() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    format!("{home}/.cache/gpui-map-data/ghsl/E2025_30ss.pmtiles")
}

/// The command that downloads the GHSL 2025 30″ raster. A macro, not a
/// `const`, so [`FETCH_GHSL`] can `concat!` it: the one copy of the text.
macro_rules! fetch_ghsl_raster {
    () => {
        "scripts/fetch-ghsl.sh 2025 30ss"
    };
}
/// The start of the command that builds the archive from that raster,
/// up to its `--out`, for [`FETCH_GHSL`] and [`build_ghsl_archive`].
macro_rules! build_ghsl_archive_to {
    () => {
        "cargo run --release --manifest-path tools/dotgrid-build/Cargo.toml -- --tiff ~/.cache/gpui-map-data/ghsl/GHS_POP_E2025_GLOBE_R2023A_4326_30ss_V1_0.tif --arcsec 30 --epoch 2025 --out "
    };
}

/// The dot source, and whether it is the synthetic stand-in.
pub fn open_source(grid_arg: Option<&str>, synthetic: bool) -> (Arc<dyn GridCountSource>, bool) {
    if synthetic {
        return (Arc::new(synthetic::source()), true);
    }
    let location = grid_arg.map_or_else(default_grid, str::to_string);
    let is_url = location.starts_with("http://") || location.starts_with("https://");
    if !is_url && !Path::new(&location).exists() {
        println!("no grid at {location}. To fetch and build the GHSL 30″ world (~461 MB download, ~102 MB archive):");
        println!("  {}", fetch_ghsl_raster!());
        println!("  {}", build_ghsl_archive(&location));
        println!("drawing the SYNTHETIC world instead");
        return (Arc::new(synthetic::source()), true);
    }
    (open_grid(&location).unwrap_or_else(|why| panic!("{why}")), false)
}

/// Opens the archive at `location` (a path or an http(s) URL), blocking,
/// and prints `grid: <location>` with credentials redacted. `Err` is
/// [`grid_open_failure`]'s sentence: the standalone panics with it, the
/// gallery panel shows it on the counter.
pub fn open_grid(location: &str) -> Result<Arc<dyn GridCountSource>, String> {
    let source = fetch_native::open_pmtiles_grid(location).map_err(|e| grid_open_failure(location, &e))?;
    println!("grid: {}", redact_url_credentials(location));
    Ok(Arc::new(source))
}

/// The command that builds the archive at `out` from the fetched raster.
pub fn build_ghsl_archive(out: &str) -> String {
    format!("{}{out}", build_ghsl_archive_to!())
}

/// Why `--grid` did not open, with any credential in the location (an
/// access token, a password, `user:pass@`) redacted.
pub fn grid_open_failure(location: &str, e: &gpui_maps::overlay::dotgrid::GridError) -> String {
    redact_url_credentials(&format!("{location}: {e}"))
}

/// Where the dots come from: a source, or none and the reason (shown on
/// screen instead of the counter).
pub struct DotSource {
    pub source: Option<Arc<dyn GridCountSource>>,
    pub synthetic: bool,
    pub note: Option<String>,
}

/// `--clickhouse` when given, else the archive (or the synthetic world).
pub fn pick_source(clickhouse: Option<&str>, table: &str, grid: Option<&str>, synthetic: bool) -> DotSource {
    if let Some(url) = clickhouse {
        return clickhouse_source(url, Vec::new(), table);
    }
    let (source, synthetic) = open_source(grid, synthetic);
    DotSource { source: Some(source), synthetic, note: None }
}

/// Arc-seconds of a loader table's cells, from its name (`ghs_cells_30ss` -> 30).
#[cfg_attr(not(feature = "clickhouse-demo"), allow(dead_code))]
pub fn table_arcsec(table: &str) -> f64 {
    table
        .strip_prefix("ghs_cells_")
        .and_then(|t| t.strip_suffix("ss"))
        .and_then(|t| t.parse().ok())
        .unwrap_or_else(|| panic!("--table takes ghs_cells_30ss|ghs_cells_3ss, got {table}"))
}

/// `url` without its query string or `user:pass@` (either may carry
/// credentials), for the screen and the log.
#[cfg_attr(not(feature = "clickhouse-demo"), allow(dead_code))]
pub fn display_url(url: &str) -> String {
    let url = url.split('?').next().unwrap_or(url);
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let (authority, path) = rest.split_once('/').map_or((rest, ""), |(a, p)| (a, p));
            let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
            if path.is_empty() {
                format!("{scheme}://{host}")
            } else {
                format!("{scheme}://{host}/{path}")
            }
        }
        None => url.to_string(),
    }
}

/// The dot source a `connect` result gives the demo: the source, or none
/// and the sentence the screen shows instead. Pure, so the "server down at
/// startup" path is tested without a window.
#[cfg(feature = "clickhouse-demo")]
pub fn choose_source(
    connected: Result<ClickHouseGridSource, gpui_maps::overlay::dotgrid::GridError>,
    url: &str,
    table: &str,
) -> (Option<Arc<dyn GridCountSource>>, Option<String>) {
    match connected {
        Ok(source) => (Some(Arc::new(source)), None),
        Err(e) => (None, Some(format!("ClickHouse {} ({table}) unavailable: {e}", display_url(url)))),
    }
}

/// `headers`: a saved connection's auth ([`kit::ClickHouseEndpoint`]),
/// empty for the standalone demo's URL.
#[cfg(feature = "clickhouse-demo")]
pub fn clickhouse_source(url: &str, headers: Vec<(String, String)>, table: &str) -> DotSource {
    let started = Instant::now();
    // 90°N: the loader's snapped lattice (MG-2's ruling), as in the archive.
    let (source, note) = choose_source(ClickHouseGridSource::connect_with_headers(url, headers, table, table_arcsec(table), 90.0), url, table);
    match (&source, &note) {
        (Some(s), _) => println!(
            "grid: ClickHouse {} gpui_map_dots.{table} ({} levels, epochs {:?}), descriptor in {:.0} ms",
            display_url(url),
            s.descriptor().levels,
            s.descriptor().epochs,
            started.elapsed().as_secs_f64() * 1e3
        ),
        (None, Some(n)) => println!("no dots: {n}; drawing the basemap without them"),
        (None, None) => {}
    }
    DotSource { source, synthetic: false, note }
}

#[cfg(not(feature = "clickhouse-demo"))]
pub fn clickhouse_source(_url: &str, _headers: Vec<(String, String)>, _table: &str) -> DotSource {
    unreachable!("parse_args exits on --clickhouse without clickhouse-demo")
}

pub fn dot_style(budget: Option<u32>) -> DotStyle {
    let mut style = DotStyle::default();
    if let Some(b) = budget {
        style.budget_dots = b;
    }
    style
}

/// [`dot_style`] with every [`StyleArgs`] flag applied: `--shrink`,
/// `--fade`, `--relocate`, `--k` and `--epochs` (`--budget` goes through
/// [`dot_style`] itself).
pub fn style_of(args: &StyleArgs) -> DotStyle {
    let mut style = dot_style(args.budget);
    if let Some(shrink) = args.shrink {
        style.density_shrink = shrink;
    }
    if let Some(gamma) = args.fade {
        style.overlap_gamma = gamma;
    }
    if let Some(relocate) = args.relocate {
        style.relocate = relocate;
    }
    if let Some(k) = args.k {
        style.mask_k = k;
    }
    if let Some((a, b)) = args.epochs {
        style.epoch = a;
        style.epoch_b = Some(b);
    }
    style
}

/// `worldview`: `Some("IN"|"US"|"CN"|"JP"|...)` runs `style::set_worldview`
/// over the vendored style's JSON before it compiles (`--worldview`);
/// `None` draws the style as vendored (`US`).
pub fn mapbox_streets(worldview: Option<&str>) -> Option<(gpui_maps::style::Style, Arc<dyn TileSource>, Option<String>, String)> {
    let token = crate::kit::env_mapbox_token()?;
    // Not in the open library tree (scripts/fetch-mapbox-styles.sh).
    if !crate::kit::mapbox_style_bundled("examples/mapbox-streets-style.json") {
        eprintln!("world_dots: examples/mapbox-streets-style.json is not in this checkout; no Mapbox Streets basemap");
        return None;
    }
    Some(mapbox_streets_for(token, worldview))
}

/// [`mapbox_streets`] with the token handed in rather than read from the
/// environment: the gallery passes `PanelEnv::mapbox_token`, which also
/// comes from `~/.config/gpui-map/mapbox.env`.
pub fn mapbox_streets_for(
    token: String,
    worldview: Option<&str>,
) -> (gpui_maps::style::Style, Arc<dyn TileSource>, Option<String>, String) {
    let bytes = crate::kit::asset_bytes("examples/mapbox-streets-style.json").expect("vendored Mapbox Streets style");
    let bytes = match worldview {
        Some(wv) => {
            let mut json: serde_json::Value = serde_json::from_slice(&bytes).expect("vendored Mapbox Streets style is valid JSON");
            let changed = gpui_maps::style::set_worldview(&mut json, wv);
            println!("worldview: {wv} ({changed} layers changed)");
            serde_json::to_vec(&json).expect("serialise the rewritten style")
        }
        None => bytes,
    };
    let style = gpui_maps::style_spec::parse_and_compile(&bytes).expect("Mapbox Streets must compile");
    let url = gpui_maps::basemap::mapbox_composite_tile_url(&bytes, &token).expect("a mapbox:// composite source");
    let source = HttpTileSource::new(
        url,
        16,
        concat!(
            r#"<a href="https://www.mapbox.com/about/maps" target="_blank">&copy; Mapbox</a> "#,
            r#"<a href="https://www.openstreetmap.org/copyright/" target="_blank">&copy; OpenStreetMap</a>"#,
        ),
    );
    let sprite = style.sprite.clone();
    (style, Arc::new(source), sprite, token)
}

/// The page's basemap: SWAT Routes Dark (open), the keyless look-alike
/// over OpenFreeMap's planet tiles, with or without a Mapbox token: the
/// dots read best on it. Studio's style chooser opens ticked on it
/// (`map_options::OPENS_ON`) and can switch to any other.
pub fn routes_open() -> crate::kit::KeylessBasemap {
    crate::kit::keyless_basemap(crate::kit::KeylessStyle::OpenSwatRoutesDark).expect("the vendored SWAT Routes Dark (open) style compiles")
}

/// The window root: the map, focus, the demo keys and the counter over it.
pub struct DemoRoot {
    pub map: Entity<MapView>,
    pub counter: String,
    pub label: Option<&'static str>,
    pub keys: shell::DemoKeys,
    /// False only in a gpui test window, which has no platform window for
    /// the map's Metal layer: the map is held (and keyed) but not drawn,
    /// and the root takes the keyboard itself (`focus`) since there is no
    /// map node to focus.
    pub show_map: bool,
    pub focus: gpui::FocusHandle,
    /// The source's epochs, ascending (`[`/`]` step epoch B through them).
    pub epochs: Vec<u16>,
    /// The epoch slider (the shared `time_filter` card), for a source with
    /// more than one epoch.
    pub time: Option<Entity<crate::time_filter::TimeFilter>>,
    /// Bumped by every `space`: an older animation sees it and stops.
    pub animation: u64,
    /// Why there are no dots, when the source failed after the root was
    /// built (the gallery opens the archive off the UI thread); the
    /// counter shows it ([`counter_task`]). The standalone window keeps its
    /// note in `windowed` and leaves this `None`.
    pub note: Option<String>,
    /// When the map takes the keyboard: every frame standalone, once (and
    /// on a click in the map) in the gallery, whose sidebar must keep it.
    pub focus_policy: shell::FocusPolicy,
    pub focused_once: bool,
}

impl DemoRoot {
    /// The standalone window's root: the demo keys publish the window
    /// title, and the map holds the keyboard every frame.
    pub fn new(map: Entity<MapView>, counter: String, label: Option<&'static str>, show_map: bool, cx: &mut Context<Self>) -> Self {
        let keys = shell::DemoKeys::new(&map, "World dots", cx);
        Self::with_keys(map, counter, label, show_map, keys, shell::FocusPolicy::EveryFrame, cx)
    }

    /// The gallery's root, as `shell::DemoShell::embedded` is: no window
    /// title (the gallery owns it) and the map focused once.
    pub fn embedded(map: Entity<MapView>, counter: String, label: Option<&'static str>, cx: &mut Context<Self>) -> Self {
        let keys = shell::DemoKeys::with_satellite_titled(&map, None, None, cx);
        Self::with_keys(map, counter, label, true, keys, shell::FocusPolicy::Once, cx)
    }

    fn with_keys(
        map: Entity<MapView>,
        counter: String,
        label: Option<&'static str>,
        show_map: bool,
        keys: shell::DemoKeys,
        focus_policy: shell::FocusPolicy,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            map,
            counter,
            label,
            keys,
            show_map,
            focus: cx.focus_handle(),
            epochs: Vec::new(),
            time: None,
            animation: 0,
            note: None,
            focus_policy,
            focused_once: false,
        }
    }

    /// The live-epoch keys ([`EpochKey`]) first, then `t`, `g` and `s`
    /// (`shell::DemoKeys::on_key`), for keys the map did not consume.
    fn on_key(&mut self, event: &gpui::KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(key) = EpochKey::of(event) {
            self.epoch_key(key, cx);
            cx.notify();
            cx.stop_propagation();
            return;
        }
        let map = self.map.clone();
        self.keys.on_key(&map, event, window, cx);
    }

    /// Live epochs (Task 15). Without an `epoch_b` (no `--epochs`), each
    /// key says so on stdout and does nothing else.
    fn epoch_key(&mut self, key: EpochKey, cx: &mut Context<Self>) {
        let Some(style) = self.map.read(cx).dot_style(LAYER) else { return };
        let Some(b) = style.epoch_b else {
            println!("epochs: this layer draws one epoch ({}); start with --epochs A,B", style.epoch);
            return;
        };
        match key {
            EpochKey::Difference => {
                let ramp = if style.ramp == DotRamp::Difference { DotStyle::default().ramp } else { DotRamp::Difference };
                self.map.update(cx, |m, cx| m.set_dot_ramp(LAYER, ramp, cx));
                println!("epochs: {} ramp", if ramp == DotRamp::Difference { "difference" } else { "density" });
            }
            EpochKey::Step(dir) => match step_epoch(&self.epochs, style.epoch, b, dir) {
                Some(next) => {
                    self.animation += 1;
                    self.map.update(cx, |m, cx| {
                        m.set_dot_epochs(LAYER, style.epoch, Some(next), cx);
                        m.set_dot_epoch_mix(LAYER, 0.0, cx);
                    });
                    println!("epochs: {} -> {next}", style.epoch);
                }
                None => println!("epochs: no loaded epoch {} {b} (loaded: {:?})", if dir < 0 { "before" } else { "after" }, self.epochs),
            },
            EpochKey::Animate => {
                // Toward B from nearer A, back toward A from nearer B.
                let (from, to) = if style.epoch_mix < 0.5 { (style.epoch_mix, 1.0) } else { (style.epoch_mix, 0.0) };
                self.animation += 1;
                let generation = self.animation;
                let map = self.map.clone();
                cx.spawn(async move |this, cx| {
                    let started = Instant::now();
                    loop {
                        cx.background_executor().timer(Duration::from_millis(16)).await;
                        if this.read_with(cx, |r, _| r.animation != generation).unwrap_or(true) {
                            return;
                        }
                        let t = mix_at(started.elapsed(), from, to);
                        map.update(cx, |m, cx| m.set_dot_epoch_mix(LAYER, t, cx));
                        if t == to {
                            return;
                        }
                    }
                })
                .detach();
            }
        }
    }
}

/// A live-epoch key: `[` / `]` step epoch B, `space` animates, `shift+D`
/// toggles the difference ramp. Unmodified otherwise (Command, Control,
/// Option and Function belong to the host and the map); plain `d` is the
/// engine's tile-debug overlay, which the map consumes first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpochKey {
    Step(i32),
    Animate,
    Difference,
}

impl EpochKey {
    pub fn of(event: &gpui::KeyDownEvent) -> Option<Self> {
        let m = &event.keystroke.modifiers;
        if m.platform || m.control || m.alt || m.function {
            return None;
        }
        match (event.keystroke.key.as_str(), m.shift) {
            ("[", false) => Some(Self::Step(-1)),
            ("]", false) => Some(Self::Step(1)),
            ("space", false) => Some(Self::Animate),
            ("d", true) => Some(Self::Difference),
            _ => None,
        }
    }
}

/// One A -> B animation's length.
pub const EPOCH_ANIMATION: Duration = Duration::from_secs(2);

/// The mix `elapsed` into an animation from `from` to `to` over
/// [`EPOCH_ANIMATION`], smoothstep-eased; exactly `to` at the end.
pub fn mix_at(elapsed: Duration, from: f32, to: f32) -> f32 {
    let u = (elapsed.as_secs_f32() / EPOCH_ANIMATION.as_secs_f32()).min(1.0);
    if u >= 1.0 {
        return to;
    }
    from + (to - from) * u * u * (3.0 - 2.0 * u)
}

/// Epoch B after one `[` (`dir` -1) or `]` (+1): the loaded epoch before
/// or after `b`, skipping `a`. `None` past either end.
pub fn step_epoch(epochs: &[u16], a: u16, b: u16, dir: i32) -> Option<u16> {
    let mut candidates: Vec<u16> = epochs.iter().copied().filter(|&e| e != a).collect();
    if dir < 0 {
        candidates.reverse();
        candidates.into_iter().find(|&e| e < b)
    } else {
        candidates.into_iter().find(|&e| e > b)
    }
}

/// The counter's epoch line: `1975 -> 2030 at 40 %`, and the ramp when it
/// is the difference one. Empty for a single epoch.
pub fn epoch_text(style: Option<&DotStyle>) -> String {
    match style {
        Some(s) if s.epoch_b.is_some() => format!(
            " · {} → {} at {:.0} %{}",
            s.epoch,
            s.epoch_b.unwrap_or(s.epoch),
            s.epoch_mix * 100.0,
            if s.ramp == DotRamp::Difference { " (difference)" } else { "" }
        ),
        _ => String::new(),
    }
}

impl Render for DemoRoot {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let handle = self.map.read(cx).focus_handle();
        if self.focus_policy.should_focus(handle.is_some(), self.focused_once) {
            if let Some(handle) = handle {
                window.focus(&handle, cx);
                self.focused_once = true;
            }
        }
        // A story preview (`POST /story/preview`) moves the camera before
        // the map renders; its captions go over the map.
        let preview = shell::preview_layer(&self.map, window, cx);
        // The app's theme, as the epoch card below and the other pages'
        // cards follow it: white on the dark globe read as a hole in it.
        // No theme installed (a bare test window): dark, the page's look.
        let dark = cx.try_global::<gpui_component::Theme>().is_none_or(|t| t.mode.is_dark());
        let (bg, fg, muted, edge, alert) = if dark {
            (gpui::rgba(0x1d20_25f2), gpui::rgb(0xe8eaed), gpui::rgb(0xa0a6ad), gpui::rgba(0xffff_ff14), gpui::rgb(0xff6b6b))
        } else {
            (gpui::rgba(0xffff_fff2), gpui::rgb(0x1f2933), gpui::rgb(0x5f6b76), gpui::rgba(0x0000_0014), gpui::rgb(0xb00020))
        };
        let mut card = div()
            .id("world-dots-counter")
            .debug_selector(|| "world-dots-counter".to_string())
            .absolute()
            .top(px(12.0))
            .left(px(12.0))
            .px(px(12.0))
            .py(px(8.0))
            .rounded(px(8.0))
            .bg(bg)
            .border_1()
            .border_color(edge)
            .text_color(fg)
            .text_size(px(14.0))
            .child(self.counter.clone())
            // Dot density has no per-object identity: say so rather than
            // fake a hover card.
            .child(div().text_size(px(12.0)).text_color(muted).child(AGGREGATED_NOTE));
        if let Some(label) = self.label {
            card = card.child(div().text_color(alert).child(label));
        }
        // Weak, for `shell::FocusedMap`'s reason: a listener in the last
        // drawn frame must not keep a switched-away panel's map alive.
        let click_map = self.map.downgrade();
        let root = div()
            .id("world-dots-root")
            .size_full()
            .relative()
            .on_key_down(cx.listener(Self::on_key))
            // The gallery's `Once` policy's other half: a click in the map
            // takes the keyboard back from the sidebar. (Standalone, the
            // map already holds it every frame.)
            .when(self.focus_policy == shell::FocusPolicy::Once, |root| {
                root.on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
                    if let Some(handle) = click_map.upgrade().and_then(|map| map.read(cx).focus_handle()) {
                        window.focus(&handle, cx);
                    }
                })
            });
        if self.show_map {
            // Keys reach this root on their way up from the focused map
            // (`kit/src/shell.rs`' module doc: the map answers
            // first). No `track_focus` here: a click would take the
            // keyboard from the map.
            root.child(self.map.clone()).child(card).children(preview).children(self.time.clone())
        } else {
            window.focus(&self.focus, cx);
            root.track_focus(&self.focus).child(card)
        }
    }
}

/// The legend's word on hovering: these dots are aggregated, there is
/// nothing per dot to show.
pub const AGGREGATED_NOTE: &str = "Aggregated; no per-dot details";

/// A head count for the counter: exact below a million, then "1.29 M"
/// and "1.57 B".
pub fn people_text(people: f64) -> String {
    if people >= 1e9 {
        format!("{:.2} B", people / 1e9)
    } else if people >= 1e6 {
        format!("{:.2} M", people / 1e6)
    } else {
        thousands(people.round().max(0.0) as u64)
    }
}

pub fn counter_text(stats: Option<gpui_maps::overlay::dotgrid::layer::DotStats>, no_source: Option<&str>) -> String {
    if let Some(why) = no_source {
        return format!("no dots: {why}");
    }
    match stats {
        Some(s) if s.dots_drawn > 0 => {
            // What the VIEW shows: the people whose cells are centred on
            // screen and their dots at the view's ppd. Not the drawn
            // tiles' whole population -- they reach far off screen (a
            // Nizhny Novgorod view read 9.5 M people's dots for a city of
            // 1.3 M).
            format!(
                "{} people in view · {} dots · 1 dot = {} people",
                people_text(s.people_in_view),
                thousands(s.dots_in_view),
                thousands(s.ppd.round().max(1.0) as u64)
            )
        }
        Some(s) if s.last_error.is_some() => format!("no dots: {}", s.last_error.unwrap_or("error")),
        _ => "loading dots…".to_string(),
    }
}

/// The counter's whole line for `view`: [`counter_text`] plus the epoch
/// part ([`epoch_text`]). `note` is why there are no dots, if there are
/// none. The standalone window's counter loop and the gallery's
/// [`counter_task`] both draw this.
pub fn counter_line(view: &MapView, note: Option<&str>) -> String {
    counter_text(view.dot_stats(LAYER), note) + &epoch_text(view.dot_style(LAYER).as_ref())
}

/// The label over the synthetic world, on screen wherever it is drawn.
pub const SYNTHETIC_LABEL: &str = "synthetic population (procedural, not GHSL)";

// ------------------------------------------------------------ the gallery

pub const ID: &str = "world-dots";
#[cfg(feature = "clickhouse-demo")]
pub const ID_CLICKHOUSE: &str = "world-dots-clickhouse";

/// The two commands that fetch the GHSL 2025 30″ raster and build the
/// archive [`default_grid`] names: the missing-data card and the fallback
/// banner print them verbatim.
pub const FETCH_GHSL: &str =
    concat!(fetch_ghsl_raster!(), " && ", build_ghsl_archive_to!(), "~/.cache/gpui-map-data/ghsl/E2025_30ss.pmtiles");

/// The dots, and the basemap under them: SWAT Routes Dark (open) over
/// OpenFreeMap (OpenMapTiles, OpenStreetMap). A Mapbox style picked in the
/// chooser adds its own credit (`StyleChoice::credits`).
pub const CREDITS: &[kit::Credit] = &[kit::CREDIT_GHSL, kit::CREDIT_OSM_OPENFREEMAP];

/// Network for the basemap and the GHSL archive at [`default_grid`] (the
/// synthetic world without it, `Status::Fallback`). No Mapbox token: the
/// basemap is keyless ([`routes_open`]).
pub fn requirements(_env: &kit::PanelEnv) -> Vec<kit::Requirement> {
    requirements_at(Path::new(&default_grid()))
}

/// [`requirements`] with the archive at `path`: a test names a path that
/// is not there rather than depending on the real `$HOME`.
pub fn requirements_at(path: &Path) -> Vec<kit::Requirement> {
    vec![
        kit::Requirement::Network,
        kit::Requirement::File {
            path: path.to_path_buf(),
            what: "GHSL 30″ world archive",
            fetch: FETCH_GHSL,
            size: "~461 MB download, ~102 MB archive",
            fallback: Some("synthetic world"),
            then: None,
        },
    ]
}

/// The ClickHouse entry's needs: the same basemap, and the saved
/// connection [`CLICKHOUSE_CONNECTION`] answering with the cells loaded.
/// The gallery probes it (`kit::probe_clickhouse`) before building; with
/// no such connection the page is `Missing` and points at the
/// Connections page.
#[cfg(feature = "clickhouse-demo")]
pub fn requirements_clickhouse(env: &kit::PanelEnv) -> Vec<kit::Requirement> {
    let (connection, url) = match &env.clickhouse {
        Some(e) => (e.connection.clone(), Some(e.url.clone())),
        None => (CLICKHOUSE_CONNECTION.to_string(), None),
    };
    vec![kit::Requirement::Network, kit::Requirement::ClickHouse { connection, url, load: CLICKHOUSE_LOAD }]
}

/// Where the gallery panel's dots come from.
enum Dots {
    Synthetic,
    /// A local archive (or URL), opened off the UI thread.
    Archive(String),
    /// `gpui_map_dots.<table>` on the saved connection's server, connected
    /// off the UI thread.
    #[cfg(feature = "clickhouse-demo")]
    ClickHouse { endpoint: kit::ClickHouseEndpoint, table: &'static str },
}

/// The GHSL archive at [`default_grid`] when it is there, else the
/// synthetic world (labelled so on screen; `requirements` has already put
/// the fetch commands in the gallery's banner).
pub fn build(env: &kit::PanelEnv, window: &mut Window, cx: &mut App) -> anyhow::Result<kit::Built> {
    let grid = default_grid();
    if Path::new(&grid).exists() {
        build_with(env, Dots::Archive(grid), cx)
    } else {
        build_synthetic(env, window, cx)
    }
}

/// The synthetic world, whatever is on disk.
pub fn build_synthetic(env: &kit::PanelEnv, _window: &mut Window, cx: &mut App) -> anyhow::Result<kit::Built> {
    build_with(env, Dots::Synthetic, cx)
}

/// The dots from ClickHouse (`ghs_cells_30ss`) through the saved
/// connection the host resolved, at the default epoch. No connection: an
/// error (the gallery shows the missing-data card before ever building).
#[cfg(feature = "clickhouse-demo")]
pub fn build_clickhouse(env: &kit::PanelEnv, _window: &mut Window, cx: &mut App) -> anyhow::Result<kit::Built> {
    let endpoint = env.clickhouse.clone().ok_or_else(|| anyhow::anyhow!(kit::no_clickhouse_connection(CLICKHOUSE_CONNECTION)))?;
    build_with(env, Dots::ClickHouse { endpoint, table: CLICKHOUSE_TABLE }, cx)
}

/// The panel: the basemap at once, at the `world` preset, and the dots
/// when their source arrives. Opening a source never panics here (unlike
/// the standalone [`open_source`]): a failure is the counter's
/// "no dots: ..." line, credentials redacted.
fn build_with(env: &kit::PanelEnv, dots: Dots, cx: &mut App) -> anyhow::Result<kit::Built> {
    let (w, h) = env.viewport;
    let at = Preset { zoom: home_zoom(h, GALLERY_HOME.lat, HOME_GLOBE_FRACTION), ..GALLERY_HOME };
    // Keyless whether or not a token is present: Mapbox is a chooser pick.
    let basemap = routes_open();
    let runtime = env.runtime.clone();
    let map = cx.new(|_| {
        let mut view = MapView::new(at.lon, at.lat, at.zoom, w, h, basemap.style, basemap.source, runtime.clone());
        view.set_style_document(basemap.document);
        view.set_chrome_above_map(true);
        view
    });
    if let Some(sprite) = basemap.sprite {
        kit::spawn_sprites(map.downgrade(), sprite, None, runtime, cx);
    }
    let style = dot_style(None);
    let label = matches!(dots, Dots::Synthetic).then_some(SYNTHETIC_LABEL);
    let counter = counter_text(None, None);
    let root = cx.new(|cx| match env.host.title() {
        Some(title) => {
            let keys = shell::DemoKeys::new(&map, title, cx);
            DemoRoot::with_keys(map.clone(), counter, label, true, keys, shell::FocusPolicy::EveryFrame, cx)
        }
        None => DemoRoot::embedded(map.clone(), counter, label, cx),
    });
    // `None` for the synthetic world, attached at once; else how to open
    // the source, which runs on the background executor.
    type Open = Box<dyn FnOnce() -> Result<Arc<dyn GridCountSource>, String> + Send>;
    #[cfg(feature = "clickhouse-demo")]
    let totals_from: Option<kit::ClickHouseEndpoint> = match &dots {
        Dots::ClickHouse { endpoint, .. } => Some(endpoint.clone()),
        _ => None,
    };
    #[cfg(not(feature = "clickhouse-demo"))]
    let totals_from: Option<kit::ClickHouseEndpoint> = None;
    let open: Option<Open> = match dots {
        Dots::Synthetic => {
            attach(&root, &map, Arc::new(synthetic::source()), style.clone(), None, cx);
            None
        }
        // Each opener prints its own failure: `clickhouse_source` already
        // says "no dots: ..." on a failed connect.
        Dots::Archive(location) => Some(Box::new(move || {
            open_grid(&location).inspect_err(|why| println!("no dots: {why}"))
        })),
        #[cfg(feature = "clickhouse-demo")]
        Dots::ClickHouse { endpoint, table } => Some(Box::new(move || {
            let DotSource { source, note, .. } = clickhouse_source(&endpoint.url, endpoint.headers.clone(), table);
            source.ok_or_else(|| note.unwrap_or_else(|| "ClickHouse gave no source".to_string()))
        })),
    };
    if let Some(open) = open {
        let opened = cx.background_executor().spawn(async move { open() });
        let (root_weak, map_weak, cancel) = (root.downgrade(), map.downgrade(), env.cancel.clone());
        cx.spawn(async move |cx| {
            let result = opened.await;
            // Switched away while opening: the panel is gone, or going.
            if cancel.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            cx.update(|cx| {
                let (Some(root), Some(map)) = (root_weak.upgrade(), map_weak.upgrade()) else { return };
                match result {
                    Ok(source) => attach(&root, &map, source, style, totals_from, cx),
                    Err(why) => {
                        root.update(cx, |r, cx| {
                            r.note = Some(why);
                            cx.notify();
                        });
                    }
                }
            });
        })
        .detach();
    }
    counter_task(root.downgrade(), cx);
    Ok(kit::Built { view: root.into(), map: Some(map.downgrade()), start: (at.lon, at.lat, at.zoom) })
}

/// Hands `source` to the map and its epochs to the root's `[`/`]` keys.
fn attach(root: &Entity<DemoRoot>, map: &Entity<MapView>, source: Arc<dyn GridCountSource>, style: DotStyle, totals_from: Option<kit::ClickHouseEndpoint>, cx: &mut App) {
    let epochs = source.descriptor().epochs.clone();
    root.update(cx, |r, _| r.epochs = epochs.clone());
    map.update(cx, |view, cx| view.set_dot_density_source(LAYER, source, style.clone(), cx));
    if epochs.len() > 1 {
        let card = epoch_card(map, &epochs, style.epoch, totals_from, cx);
        root.update(cx, |r, cx| {
            r.time = Some(card);
            cx.notify();
        });
    }
}

/// World population per epoch, `(epoch, people)`, filled in when the
/// server answers ([`fetch_epoch_totals`]).
type Totals = std::rc::Rc<std::cell::RefCell<Vec<(f64, f64)>>>;

/// The epoch slider: one bar per epoch (the world's population, once the
/// totals land), a head that rests on an epoch and glides between them
/// while playing -- `set_dot_epochs(a, b)` + `set_dot_epoch_mix(t)`, the
/// `[` / `]` / space keys' own calls, so a step reuses the pair's tiles.
fn epoch_card(map: &Entity<MapView>, epochs: &[u16], at: u16, totals_from: Option<kit::ClickHouseEndpoint>, cx: &mut App) -> Entity<crate::time_filter::TimeFilter> {
    use crate::time_filter::{between, Counter, TimeFilter, TimeFormat, TimeTarget, WindowMode};
    let stops: Vec<f64> = epochs.iter().map(|&e| f64::from(e)).collect();
    let step = stops.windows(2).map(|w| w[1] - w[0]).fold(f64::INFINITY, f64::min).max(1.0);
    let domain = [stops[0] - step / 2.0, stops[stops.len() - 1] + step / 2.0];
    let weak = map.downgrade();
    let pair_stops = stops.clone();
    let apply = move |w: [f64; 2], cx: &mut App| {
        let Some((a, b, t)) = epoch_pair(&pair_stops, w[1]) else { return };
        let _ = weak.update(cx, |m, cx| {
            m.set_dot_epochs(LAYER, a as u16, Some(b as u16), cx);
            m.set_dot_epoch_mix(LAYER, t as f32, cx);
        });
    };
    let totals: Totals = Totals::default();
    let read = totals.clone();
    let counter = Counter::Text(std::rc::Rc::new(move |w: [f64; 2]| {
        let t = read.borrow();
        let people = (t.len() > 1).then(|| {
            let years: Vec<f64> = t.iter().map(|p| p.0).collect();
            let (a, b, f) = between(&years, w[1]).unwrap_or((years[0], years[0], 0.0));
            let at = |y: f64| t.iter().find(|p| p.0 == y).map_or(0.0, |p| p.1);
            at(a) + (at(b) - at(a)) * f
        });
        match people {
            Some(p) => format!("{:.2} billion people", p / 1e9),
            None => "GHSL population grid".to_string(),
        }
    }));
    let head = f64::from(at).clamp(stops[0], stops[stops.len() - 1]);
    let hist = vec![0.0; stops.len()];
    let card = cx.new(|cx| {
        let mut tf = TimeFilter::new(
            map.downgrade(),
            TimeTarget::Custom(std::rc::Rc::new(apply)),
            WindowMode::Stops(stops.clone()),
            "Epoch",
            domain,
            [head, head],
            hist,
            counter,
            TimeFormat::plain(|y| format!("{y:.0}")),
            cx,
        );
        tf.playback.speed = 0.5;
        tf.caption("World population by GHSL epoch (1975-2030; 2025 and 2030 are projections). Drag or Play to move through time.")
    });
    fetch_epoch_totals(card.downgrade(), totals, totals_from, cx);
    card
}

/// The epoch pair and mix for a head at year `v`: `(a, b, t)` with the
/// dots `t` of the way from `a` to `b`; at the last epoch, the pair
/// before it fully mixed.
pub fn epoch_pair(stops: &[f64], v: f64) -> Option<(f64, f64, f64)> {
    let (a, b, t) = crate::time_filter::between(stops, v)?;
    if a != b {
        return Some((a, b, t));
    }
    let i = stops.iter().position(|&s| s == a)?;
    Some(match (i.checked_sub(1), stops.get(i + 1)) {
        (_, Some(&next)) => (a, next, 0.0),
        (Some(prev), None) => (stops[prev], a, 1.0),
        (None, None) => (a, a, 0.0),
    })
}

/// World population per epoch from the ClickHouse cells (the coarsest
/// level, well under a second), into the card's bars. Only with the
/// `clickhouse-demo` feature; without it the bars stay empty.
fn fetch_epoch_totals(card: WeakEntity<crate::time_filter::TimeFilter>, totals: Totals, from: Option<kit::ClickHouseEndpoint>, cx: &mut App) {
    #[cfg(feature = "clickhouse-demo")]
    {
        // The same saved connection the dots read through; none (an
        // archive with several epochs), no bars.
        let Some(endpoint) = from else { return };
        let (url, headers) = (endpoint.url, endpoint.headers);
        let query = "SELECT epoch, sum(pop) FROM gpui_map_dots.ghs_cells_30ss WHERE level = 6 GROUP BY epoch ORDER BY epoch FORMAT TSV";
        let task = cx.background_spawn(async move {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().ok()?;
            let text = rt.block_on(async {
                let client = reqwest::Client::builder().timeout(Duration::from_secs(20)).build().ok()?;
                let mut post = client.post(&url).body(query);
                for (k, v) in &headers {
                    post = post.header(k.as_str(), v.as_str());
                }
                post.send().await.ok()?.text().await.ok()
            })?;
            let rows: Vec<(f64, f64)> = text
                .lines()
                .filter_map(|l| {
                    let (e, p) = l.split_once('\t')?;
                    Some((e.trim().parse().ok()?, p.trim().parse().ok()?))
                })
                .collect();
            (!rows.is_empty()).then_some(rows)
        });
        cx.spawn(async move |cx| {
            let Some(rows) = task.await else {
                eprintln!("world-dots: no epoch totals from ClickHouse");
                return;
            };
            *totals.borrow_mut() = rows.clone();
            let _ = card.update(cx, |tf, cx| {
                let stops = match &tf.mode {
                    crate::time_filter::WindowMode::Stops(s) => s.clone(),
                    _ => Vec::new(),
                };
                tf.hist = stops.iter().map(|y| rows.iter().find(|r| r.0 == *y).map_or(0.0, |r| r.1)).collect();
                cx.notify();
            });
        })
        .detach();
    }
    #[cfg(not(feature = "clickhouse-demo"))]
    {
        let _ = (card, totals, from, cx);
    }
}

/// Refreshes the gallery panel's counter four times a second from
/// [`counter_line`], until the root is gone (the gallery switched away).
fn counter_task(root: WeakEntity<DemoRoot>, cx: &mut App) {
    cx.spawn(async move |cx| loop {
        cx.background_executor().timer(Duration::from_millis(250)).await;
        let updated = root.update(cx, |r, cx| {
            let text = counter_line(r.map.read(cx), r.note.as_deref());
            if r.counter != text {
                r.counter = text;
                cx.notify();
            }
        });
        if updated.is_err() {
            return;
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    // No `use super::*` (gallery test modules name what they use).

    /// The home view's globe is 98% of the map's height whatever the
    /// window: the zoom comes from the height, through the engine's own
    /// globe radius.
    #[test]
    fn the_home_globe_fills_most_of_the_map_height() {
        for h in [600.0_f64, 788.0, 1100.0] {
            let z = super::home_zoom(h, super::GALLERY_HOME.lat, super::HOME_GLOBE_FRACTION);
            let camera = gpui_maps::render::Camera {
                center_lon: super::GALLERY_HOME.lon,
                center_lat: super::GALLERY_HOME.lat,
                zoom: z,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: 1400.0,
                viewport_height_px: h as f32,
                fov_y: gpui_maps::render::Camera::DEFAULT_FOV_Y,
            };
            let diameter = 2.0 * gpui_maps::render::globe::globe_radius_world(&camera) * 2f64.powf(z);
            assert!((diameter / h - super::HOME_GLOBE_FRACTION).abs() < 0.01, "h {h}: z {z:.2}, globe {diameter:.0} px");
        }
        assert_eq!(super::home_zoom(0.0, 38.0, super::HOME_GLOBE_FRACTION), super::GALLERY_HOME.zoom, "no size yet");
    }
    use std::sync::Arc;
    #[cfg(feature = "clickhouse-demo")]
    use std::time::Instant;
    use std::time::Duration;

    use gpui_maps::map_view::MapView;
    #[cfg(feature = "clickhouse-demo")]
    use gpui_maps::overlay::dotgrid::clickhouse::ClickHouseGridSource;
    use gpui_maps::overlay::dotgrid::layer::{DotRamp, DotStyle};
    use gpui_maps::overlay::dotgrid::{fetch_native, grid, GridTile};

    #[cfg(feature = "clickhouse-demo")]
    use super::choose_source;
    use super::{counter_text, grid_open_failure, mix_at, people_text, step_epoch, DemoRoot, EPOCH_ANIMATION, LAYER};

    /// The window -> `MapView` chain must set no opaque background: the
    /// map draws in a CAMetalLayer behind gpui, so one would hide it (the
    /// bug `race_dots` shipped). `DemoRoot`'s root div holds the map; the
    /// file's one background is the counter card's, which sits OVER the
    /// map as the root's second child. The needle is built at runtime so
    /// this assertion does not match itself via `include_str!`.
    #[test]
    fn the_panel_root_sets_no_opaque_background() {
        let needle = [".", "bg", "("].concat();
        let source = include_str!("world_dots.rs");
        let statement = |start: &str| {
            let at = source.find(start).unwrap_or_else(|| panic!("{start} is in world_dots.rs"));
            &source[at..at + source[at..].find(';').expect("the statement ends")]
        };
        assert!(!statement("let root = div()").contains(&needle), "the root div must not set a background");
        assert!(statement("let mut card = div()").contains(&needle), "the counter card keeps its own background");
        assert_eq!(source.matches(&needle).count(), 1, "the counter card's is the one background in world_dots.rs");
    }

    /// Review focus 5: ClickHouse down at startup -- `connect` fails fast,
    /// the demo gets no dot source (the window then opens with the
    /// basemap alone) and a sentence saying why, which the counter shows.
    #[test]
    #[cfg(feature = "clickhouse-demo")]
    fn an_unreachable_server_surfaces_an_error_and_no_dots() {
        let url = "http://admin:secret1@127.0.0.1:1?password=secret2";
        let started = Instant::now();
        let connected = ClickHouseGridSource::connect(url, "ghs_cells_30ss", 30.0, 90.0);
        let (source, note) = choose_source(connected, url, "ghs_cells_30ss");
        assert!(started.elapsed() < Duration::from_secs(2), "took {:?}", started.elapsed());
        assert!(source.is_none(), "no dot source");
        let note = note.expect("a reason");
        assert!(note.starts_with("ClickHouse http://127.0.0.1:1 (ghs_cells_30ss) unavailable: "), "{note}");
        assert!(!note.contains("secret"), "credentials stay off screen: {note}");
        assert_eq!(counter_text(None, Some(&note)), format!("no dots: {note}"));
    }

    /// Final review I1: `--grid` with credentials in it never reaches the
    /// printed location or the open-failure panic.
    #[test]
    fn a_grid_location_is_shown_without_credentials() {
        let location = "https://user:SECRET@h.example/w.pmtiles?access_token=SECRET&password=SECRET";
        let Err(err) = fetch_native::open_pmtiles_grid("http://user:SECRET@127.0.0.1:9/w.pmtiles?password=SECRET") else {
            panic!("nothing listens on port 9")
        };
        let text = grid_open_failure(location, &err);
        assert!(!text.contains("SECRET"), "a credential leaked: {text}");
        assert!(text.contains("h.example/w.pmtiles"), "{text}");
    }

    /// The defect (a Nizhny Novgorod view): the counter read
    /// "1,350,908 dots drawn · 1 dot = 7 people" -- the drawn tiles'
    /// dots, 9.5 M people's worth, for a city of 1.3 M. It reads the
    /// people and dots in VIEW.
    #[test]
    fn the_counter_reads_the_people_and_dots_in_view() {
        let stats = gpui_maps::overlay::dotgrid::layer::DotStats {
            dots_drawn: 1_350_908,
            pop_drawn: 9.5e6,
            people_in_view: 1_289_376.0,
            dots_in_view: 184_197,
            ppd: 7.0,
            ..Default::default()
        };
        assert_eq!(counter_text(Some(stats), None), "1.29 M people in view · 184,197 dots · 1 dot = 7 people");
        for (people, text) in [(912.0, "912"), (48_210.0, "48,210"), (1.567e9, "1.57 B")] {
            assert_eq!(people_text(people), text);
        }
    }

    /// The defect: `T` (day/night terminator) and `G` (globe or
    /// flat) did nothing in world_dots -- they are the demo shell's keys
    /// (`kit/src/shell.rs`), and this demo's root had none. Keys go
    /// through gpui's own dispatch here (`simulate_keystrokes`). The map
    /// entity is held but not drawn: a gpui test window has no platform
    /// window for its Metal layer, so nothing is focused and the key's path
    /// is the root alone -- the root's own listener is what is tested.
    #[test]
    fn g_flips_the_projection_and_t_the_terminator() {
        use gpui::{AppContext as _, TestAppContext};
        let mut app = TestAppContext::single();
        let rt = tokio::runtime::Runtime::new().expect("a runtime");
        let handle = rt.handle().clone();
        let (root, cx) = app.add_window_view(|_, cx| {
            let map = cx.new(|_| {
                MapView::new(
                    43.95,
                    56.30,
                    9.5,
                    800.0,
                    600.0,
                    gpui_maps::style::Style { layers: vec![], sprite: None, sky: Default::default() },
                    Arc::new(gpui_maps::tile_source::EmptyTileSource),
                    handle,
                )
            });
            DemoRoot::new(map, "loading dots…".to_string(), None, false, cx)
        });
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        let map = root.read_with(cx, |root, _| root.map.clone());
        let globe = |cx: &mut gpui::VisualTestContext| map.read_with(cx, |m, _| m.globe_enabled());
        let night = |cx: &mut gpui::VisualTestContext| map.read_with(cx, |m, _| m.space().atmosphere.night_brightness_clamped());
        let (globe0, night0) = (globe(cx), night(cx));
        cx.simulate_keystrokes("g");
        assert_eq!(globe(cx), !globe0, "g flips the projection");
        cx.simulate_keystrokes("g");
        assert_eq!(globe(cx), globe0, "and back");
        cx.simulate_keystrokes("t");
        assert_ne!(night(cx), night0, "t toggles the terminator");
        cx.simulate_keystrokes("t");
        assert_eq!(night(cx), night0, "and back");
        cx.simulate_keystrokes("shift-g");
        assert_eq!(globe(cx), globe0, "shift+g is not the demo's key");
        drop(rt);
    }

    /// Task 15's keys, through gpui's dispatch as above: `]` / `[` step
    /// epoch B through the source's epochs (never onto A, never past the
    /// ends), `shift+D` toggles the difference ramp, `space` starts an
    /// animation, and plain `d` is not the demo's (the map's tile debug).
    #[test]
    fn the_epoch_keys_step_b_toggle_the_ramp_and_animate() {
        use gpui::{AppContext as _, TestAppContext};
        use gpui_maps::overlay::dotgrid::{GridDescriptor, MemoryGridSource};
        let mut app = TestAppContext::single();
        let rt = tokio::runtime::Runtime::new().expect("a runtime");
        let handle = rt.handle().clone();
        let epochs = vec![1975, 1980, 2025, 2030];
        let d = GridDescriptor {
            base_arcsec: 30.0,
            origin_lat: 90.0,
            origin_lon: -180.0,
            levels: 7,
            max_data_zoom: grid::base_data_zoom(30.0),
            epochs: epochs.clone(),
            attribution: String::new(),
        };
        let (root, cx) = app.add_window_view(|_, cx| {
            let map = cx.new(|cx| {
                let mut m = MapView::new(
                    78.9,
                    22.0,
                    5.0,
                    800.0,
                    600.0,
                    gpui_maps::style::Style { layers: vec![], sprite: None, sky: Default::default() },
                    Arc::new(gpui_maps::tile_source::EmptyTileSource),
                    handle,
                );
                let style = DotStyle { epoch: 1975, epoch_b: Some(2025), ..DotStyle::default() };
                m.set_dot_density_source(LAYER, Arc::new(MemoryGridSource::new(d, |_| GridTile::default())), style, cx);
                m
            });
            DemoRoot { epochs, ..DemoRoot::new(map, "loading dots…".to_string(), None, false, cx) }
        });
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        let map = root.read_with(cx, |root, _| root.map.clone());
        let style = |cx: &mut gpui::VisualTestContext| map.read_with(cx, |m, _| m.dot_style(LAYER).unwrap());
        let b = |cx: &mut gpui::VisualTestContext| style(cx).epoch_b;
        cx.simulate_keystrokes("]");
        assert_eq!(b(cx), Some(2030));
        cx.simulate_keystrokes("]");
        assert_eq!(b(cx), Some(2030), "past the last epoch");
        cx.simulate_keystrokes("[ [");
        assert_eq!(b(cx), Some(1980));
        cx.simulate_keystrokes("[");
        assert_eq!(b(cx), Some(1980), "never onto epoch A");
        assert_eq!(style(cx).epoch, 1975);
        cx.simulate_keystrokes("shift-d");
        assert_eq!(style(cx).ramp, DotRamp::Difference);
        cx.simulate_keystrokes("d");
        assert_eq!(style(cx).ramp, DotRamp::Difference, "plain d is the map's tile debug");
        cx.simulate_keystrokes("shift-d");
        assert_eq!(style(cx).ramp, DotStyle::default().ramp);
        let before = root.read_with(cx, |r, _| r.animation);
        cx.simulate_keystrokes("space");
        assert_eq!(root.read_with(cx, |r, _| r.animation), before + 1, "space starts an animation");
        drop(rt);
    }

    /// Task 14: no archive is a fallback (the synthetic world), never a
    /// block, and the card/banner carry both fetch commands.
    #[test]
    fn a_missing_archive_falls_back_to_the_synthetic_world_with_the_fetch_commands() {
        let reqs = super::requirements_at(std::path::Path::new("/nonexistent/E2025_30ss.pmtiles"));
        let r = reqs.iter().find(|r| matches!(r, crate::kit::Requirement::File { .. })).unwrap();
        match crate::kit::check(r, &|_| None, &|p| p.exists()) {
            crate::kit::Status::Fallback { why } => assert!(why.contains("synthetic")),
            other => panic!("{other:?}"),
        }
        if let crate::kit::Requirement::File { fetch, .. } = r {
            assert!(fetch.contains("scripts/fetch-ghsl.sh 2025 30ss") && fetch.contains("tools/dotgrid-build"));
        }
    }

    /// The gallery's synthetic panel: the dots are attached at build (no
    /// source to open), labelled synthetic on screen, the root is the
    /// embedded one (focus once, no window title), and the map and view
    /// both go away with the `Built` the gallery drops on a switch.
    #[test]
    fn the_synthetic_panel_attaches_labelled_dots_and_releases_them() {
        use gpui::{Entity, TestAppContext};
        let mut app = TestAppContext::single();
        let rt = tokio::runtime::Runtime::new().expect("a runtime");
        let env = crate::kit::PanelEnv {
            host: crate::kit::Host::Gallery,
            runtime: rt.handle().clone(),
            mapbox_token: None,
            viewport: (800.0, 600.0),
            cancel: Default::default(),
            clickhouse: None,
        };
        let cx = app.add_empty_window();
        let built = cx.update(|window, cx| super::build_synthetic(&env, window, cx)).expect("builds with no token and no archive");
        let map = built.map.clone().expect("a map");
        assert!(map.read_with(cx, |m, _| m.dot_style(LAYER)).expect("alive").is_some(), "the dots are attached at once");
        let root: Entity<DemoRoot> = built.view.clone().downcast().expect("the root is DemoRoot");
        root.read_with(cx, |r, _| {
            assert_eq!(r.label, Some(super::SYNTHETIC_LABEL));
            assert_eq!(r.epochs, vec![2025]);
            assert_eq!(r.focus_policy, crate::shell::FocusPolicy::Once);
        });
        let weak_root = root.downgrade();
        drop(root);
        drop(built);
        cx.update(|_, _| {});
        cx.run_until_parked();
        assert!(weak_root.upgrade().is_none(), "the root is released with its Built");
        assert!(map.upgrade().is_none(), "the map is released with its Built");
        drop(rt);
    }

    /// An archive that will not open is the counter's "no dots: ..." line
    /// (credentials redacted), never the standalone `open_source`'s panic;
    /// the basemap stays up.
    #[test]
    fn an_archive_that_fails_to_open_is_a_note_not_a_panic() {
        use gpui::{Entity, TestAppContext};
        let mut app = TestAppContext::single();
        let rt = tokio::runtime::Runtime::new().expect("a runtime");
        let env = crate::kit::PanelEnv {
            host: crate::kit::Host::Gallery,
            runtime: rt.handle().clone(),
            mapbox_token: None,
            viewport: (800.0, 600.0),
            cancel: Default::default(),
            clickhouse: None,
        };
        let cx = app.add_empty_window();
        let location = "http://user:SECRET@127.0.0.1:9/w.pmtiles".to_string();
        let built = cx.update(|_, cx| super::build_with(&env, super::Dots::Archive(location), cx)).expect("the basemap builds");
        let root: Entity<DemoRoot> = built.view.clone().downcast().expect("the root is DemoRoot");
        cx.run_until_parked();
        let note = root.read_with(cx, |r, _| r.note.clone()).expect("the failure is on screen");
        assert!(note.contains("127.0.0.1:9/w.pmtiles") && !note.contains("SECRET"), "{note}");
        assert_eq!(root.read_with(cx, |r, _| r.label), None, "not labelled synthetic");
        drop(built);
        drop(rt);
    }

    /// Review finding 2: the one copy of the GHSL commands still reads
    /// exactly what the standalone printed and the card showed before.
    #[test]
    fn the_ghsl_commands_are_one_copy_with_the_old_text() {
        let build = "cargo run --release --manifest-path tools/dotgrid-build/Cargo.toml -- --tiff ~/.cache/gpui-map-data/ghsl/GHS_POP_E2025_GLOBE_R2023A_4326_30ss_V1_0.tif --arcsec 30 --epoch 2025 --out ";
        assert_eq!(super::build_ghsl_archive("/x/E.pmtiles"), format!("{build}/x/E.pmtiles"));
        assert_eq!(super::FETCH_GHSL, format!("scripts/fetch-ghsl.sh 2025 30ss && {build}~/.cache/gpui-map-data/ghsl/E2025_30ss.pmtiles"));
    }

    #[test]
    fn an_epoch_animation_eases_from_a_to_b_in_two_seconds() {
        assert_eq!(mix_at(Duration::ZERO, 0.0, 1.0), 0.0);
        assert_eq!(mix_at(Duration::from_secs(1), 0.0, 1.0), 0.5);
        assert_eq!(mix_at(EPOCH_ANIMATION, 0.0, 1.0), 1.0);
        assert_eq!(mix_at(Duration::from_secs(9), 1.0, 0.0), 0.0, "and back");
        assert_eq!(step_epoch(&[1975, 1980, 2030], 1975, 1980, -1), None);
        assert_eq!(step_epoch(&[1975, 1980, 2030], 1980, 1975, 1), Some(2030), "skips A");
    }

    /// The epoch slider's head to the dots' epoch pair and mix: between two
    /// epochs it blends them; on the last it is the pair before, fully
    /// mixed.
    #[test]
    fn the_epoch_head_maps_to_a_pair_and_a_mix() {
        let stops = [1975.0, 1980.0, 1985.0];
        assert_eq!(super::epoch_pair(&stops, 1977.5), Some((1975.0, 1980.0, 0.5)));
        assert_eq!(super::epoch_pair(&stops, 1975.0), Some((1975.0, 1980.0, 0.0)));
        assert_eq!(super::epoch_pair(&stops, 1985.0), Some((1980.0, 1985.0, 1.0)));
        assert_eq!(super::epoch_pair(&[], 1980.0), None);
    }
}

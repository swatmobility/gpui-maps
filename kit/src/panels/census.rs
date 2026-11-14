//! US Census block groups by median household income: the windowed body
//! of `demos/us_block_groups.rs` -- the loaded block groups and their
//! push, the basemaps, and the window root (`DemoRoot`: the map, the hover
//! tooltip, the income slider, the histograms and the Breakdowns panel).
//! Moved here unchanged (gallery Task 11) so the gallery can host it as a
//! panel (Task 12); `demos/us_block_groups.rs` keeps its `Args`,
//! `parse_args`, `main`, `load` (it reads `Args`), every headless mode
//! (`--measure`, the gate rows, the probe reports), the `*_task`s and
//! `windowed`, and reaches everything below through
//! `#[path = "../kit/src/panels/census.rs"] mod census;`.
//! This file is the ONE copy: the demo keeps none of its own.
//!
//! What the browser shell shares -- the pack, the ramp, the slider maths,
//! the histograms, the hover words, the breakdowns -- lives in
//! `gpui_maps::census` and is imported here, never copied.
//!
//! Crate paths: `crate::shell` is `kit/src/shell.rs`, which every
//! includer declares at its crate root.
//!
//! `#![allow(dead_code)]`: this file is included by two crates that each
//! use a different half of it -- the demo its headless modes' helpers,
//! the gallery its panel entry points -- so items are dead in one of them.
//! The same file-level allow `kit/src/panels/world_dots.rs` carries.
//!
//! **The two `#[path]`s inside a panel:** the Breakdowns files
//! (`demos/us_block_groups/{breakdowns,panel}.rs`) are included from here,
//! not from the demo, so each crate that reaches them includes them
//! exactly once, through this file. `panel.rs` names `DemoRoot` and the
//! ramp helpers as `super::`, which is this module.
#![allow(dead_code)]

#[path = "../../../demos/us_block_groups/breakdowns.rs"]
pub mod breakdowns;

#[path = "../../../demos/us_block_groups/panel.rs"]
pub mod panel;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{px, App, AppContext, Context, Entity, IntoElement, Render, Window};
use gpui_maps::census::hist::{bin_span, income_bin, Histogram, HIST_OUT_ALPHA, HIST_OVERALL, HIST_OVERALL_OUT};
use gpui_maps::census::hover::thousands;
use gpui_maps::census::pack;
use gpui_maps::census::slider::{self, Handle};
use gpui_maps::census::view::{self, in_view_indices_indexed, ViewIndex, ViewItem};
use gpui_maps::census::{names::*, ramp::*};
#[cfg(test)]
use gpui_maps::census::hist::{hist_bins, HIST_CLASSES_PER_BIN};
use gpui_maps::census::source_state;
#[cfg(test)]
use gpui_maps::census::START;
use gpui_maps::map_view::MapView;
use gpui_maps::overlay::{FeatureId, FeatureInput, FeaturePaint, GeometryInput, PropertyColumn};
use gpui_maps::tile_source::http::HttpTileSource;
use gpui_maps::tile_source::TileSource;

use crate::{kit, shell};

/// The single national source (`--one-source`, `--geojson`). Per-state
/// sources are named by `census::source_name` (`block-groups-06`), the
/// web page's names, and read back by `census::source_state`.
pub const FILL_SOURCE: &str = "block-groups";

/// A state's source name from its zero-padded FIPS string (`"06"`).
pub fn state_source(fips: &str) -> String {
    gpui_maps::census::source_name(fips.parse().expect("a state FIPS is a number 1..=99"))
}

// How many states and metros the in-view breakdown keeps, by population
// in view: one number for both shells.
use gpui_maps::census::panel_json::IN_VIEW_GROUPS;

/// Height of the histogram strip under the slider.
pub const HIST_HEIGHT: f32 = 34.0;

/// The viridis colour of `income` (`census::ramp::viridis_rgba`).
pub fn viridis_at(income: f64) -> gpui::Rgba {
    let [r, g, b, a] = viridis_rgba(income);
    gpui::Rgba { r, g, b, a }
}

/// This state's block groups as `census::view::view_items` needs them: a
/// `pack::PackRow` per feature (only `.income` is read) and its rings, from
/// the same flat `BlockGroups` arrays `load_shapefiles_cancellable` built.
///
/// `bg.income: Option<f64>` round-trips through `PackRow.income:
/// Option<u32>` (`i as u32`, undone by `view::view_item_of`'s `f64::from`)
/// -- exact for the whole-dollar values `join_income`'s ACS CSV always
/// carries (`income_round_trips_through_pack_row_for_whole_dollar_incomes`
/// pins it), but silently lossy for a fractional income, should a future
/// source ever have one.
pub fn view_items(data: &BlockGroups) -> Vec<ViewItem> {
    let rows: Vec<pack::PackRow> = data
        .features
        .iter()
        .map(|bg| pack::PackRow { geoid: bg.geoid as u64, income: bg.income.map(|i| i as u32), population: None, aland_km2: 0.0 })
        .collect();
    view::view_items(&rows, |i| {
        let bg = &data.features[i];
        data.rings[bg.rings.0..bg.rings.1].iter().map(|&(r0, r1)| data.coords[r0..r1].to_vec()).collect()
    })
}

pub const ALL_STATES: &[&str] = &[
    "01", "02", "04", "05", "06", "08", "09", "10", "11", "12", "13", "15", "16", "17", "18", "19", "20", "21", "22",
    "23", "24", "25", "26", "27", "28", "29", "30", "31", "32", "33", "34", "35", "36", "37", "38", "39", "40", "41",
    "42", "44", "45", "46", "47", "48", "49", "50", "51", "53", "54", "55", "56", "60", "66", "69", "72", "78",
];

/// Which basemap to draw the choropleth over. Both are pale greys -- the
/// point is that the income ramp is the only saturated colour on screen,
/// the way Kyle Walker's mapgl reference reads. `None` (the default) picks
/// [`Basemap::Mapbox`] when `MAPBOX_ACCESS_TOKEN` is set and
/// [`Basemap::Positron`] otherwise; `--basemap` overrides that pick either
/// way.
#[derive(Clone, Copy, PartialEq)]
pub enum Basemap {
    /// Mapbox Light v11 (`demos/mapbox-light-style.json`, vendored from
    /// `mapbox://styles/mapbox/light-v11`). Needs `MAPBOX_ACCESS_TOKEN`.
    Mapbox,
    /// OpenFreeMap Positron (`demos/positron-style.json`, vendored from
    /// `https://tiles.openfreemap.org/styles/positron`). Keyless.
    Positron,
}

/// Every block group, flattened: one coordinate array, rings and polygons
/// as index ranges into it, so 70 M coordinates are one allocation rather
/// than 700 k.
#[derive(Default)]
pub struct BlockGroups {
    pub coords: Vec<[f64; 2]>,
    pub rings: Vec<(usize, usize)>,
    pub polygons: Vec<(usize, usize)>,
    pub features: Vec<Bg>,
    /// `(fips, first feature, one past the last)`, in load order.
    pub states: Vec<(String, usize, usize)>,
    /// `Some` when these came from the simplified pack
    /// ([`load_pack_cancellable`]): the tables the shapefile path reads
    /// from other files under the data directory instead.
    pub pack: Option<PackTables>,
}

/// What a pack carries beyond the geometry and the per-feature rows: the
/// county names and the CBSA table (the shapefile path reads both from
/// disk), and the simplification it was built with.
pub struct PackTables {
    pub path: PathBuf,
    /// The Douglas-Peucker tolerance the pack was simplified at, metres.
    pub tolerance_m: f32,
    /// `names::parse_county_names`' input, as the browser reads it.
    pub county_text: String,
    /// `Breakdowns::from_texts`' CBSA table.
    pub cbsa_text: String,
}

pub struct Bg {
    pub geoid: f64,
    /// Median household income, `None` where the Census has no estimate or
    /// the block group is not in the ACS table (the island areas).
    pub income: Option<f64>,
    /// `ALAND`: land area in square metres, for the hover tooltip.
    pub aland_m2: f64,
    /// Total population: the pack carries it per block group; the
    /// shapefile path leaves it `None` and the breakdowns read
    /// `acs/population_bg_2023.csv` instead.
    pub population: Option<f64>,
    pub polygons: (usize, usize),
    pub rings: (usize, usize),
}

/// `tl_2023_{fips}_bg.shp` under `dir/shp`: where one state's block
/// groups are read from.
pub fn shapefile_path(dir: &std::path::Path, fips: &str) -> PathBuf {
    dir.join("shp").join(format!("tl_2023_{fips}_bg.shp"))
}

/// Every state's block groups, then the income join, stopping between
/// states when `cancel` is raised:
/// `cancel` is read before each state's shapefile is opened and once more
/// before the income join, and `None` is returned the moment it is set.
/// `progress(done, total)` follows each state. A state whose shapefile is
/// not on disk is said so and skipped (the standalone checks first and
/// panics, `demos/us_block_groups.rs`'s `load`); any other read error
/// panics as before.
pub fn load_shapefiles_cancellable(
    dir: &std::path::Path,
    states: &[String],
    cancel: &AtomicBool,
    progress: &dyn Fn(usize, usize),
) -> Option<BlockGroups> {
    use shapefile::dbase::FieldValue;
    use shapefile::PolygonRing;
    let mut out = BlockGroups::default();
    let total = states.len();
    for (done, fips) in states.iter().enumerate() {
        if cancel.load(Ordering::SeqCst) {
            println!("census load: cancelled after {done} of {total} states");
            return None;
        }
        let first = out.features.len();
        let path = shapefile_path(dir, fips);
        if !path.exists() {
            println!("{}: not found -- run demos/data/fetch_us_block_groups.sh; skipping state {fips}", path.display());
            progress(done + 1, total);
            continue;
        }
        let mut reader = shapefile::Reader::from_path(&path).unwrap_or_else(|err| {
            panic!("{}: {err} -- run demos/data/fetch_us_block_groups.sh first", path.display())
        });
        for item in reader.iter_shapes_and_records_as::<shapefile::Polygon, shapefile::dbase::Record>() {
            let (shape, record) = item.expect("a readable block group record");
            let number = |key: &str| match record.get(key) {
                Some(FieldValue::Numeric(Some(v))) => *v,
                Some(FieldValue::Character(Some(s))) => s.trim().parse().unwrap_or(0.0),
                _ => 0.0,
            };
            let (p0, r0) = (out.polygons.len(), out.rings.len());
            // TIGER writes each outer ring followed by its own holes, so a
            // hole belongs to the outer ring before it.
            for ring in shape.rings() {
                let start = out.coords.len();
                out.coords.extend(ring.points().iter().map(|p| [p.x, p.y]));
                if matches!(ring, PolygonRing::Outer(_)) || out.polygons.len() == p0 {
                    out.polygons.push((out.rings.len(), out.rings.len()));
                }
                out.rings.push((start, out.coords.len()));
                out.polygons.last_mut().expect("pushed above").1 = out.rings.len();
            }
            out.features.push(Bg {
                geoid: number("GEOID"),
                income: None,
                aland_m2: number("ALAND"),
                population: None,
                polygons: (p0, out.polygons.len()),
                rings: (r0, out.rings.len()),
            });
        }
        out.states.push((fips.clone(), first, out.features.len()));
        progress(done + 1, total);
    }
    if cancel.load(Ordering::SeqCst) {
        println!("census load: cancelled after {total} of {total} states, before the income join");
        return None;
    }
    join_income(dir, &mut out);
    Some(out)
}

/// Joins `acs/income_bg_2023.csv` (`GEOID,income`, written by
/// `fetch_us_block_groups.sh`) onto the loaded block groups by GEOID and
/// prints the coverage. A missing CSV leaves every block group grey.
pub fn join_income(dir: &std::path::Path, out: &mut BlockGroups) {
    let path = dir.join("acs").join("income_bg_2023.csv");
    let Ok(text) = std::fs::read_to_string(&path) else {
        println!("income join: {} missing -- run demos/data/fetch_us_block_groups.sh; every block group draws grey", path.display());
        return;
    };
    // GEOID -> Some(income), or None for a row whose estimate is a sentinel.
    let table: HashMap<u64, Option<f64>> = text
        .lines()
        .skip(1)
        .filter_map(|line| {
            let (geoid, income) = line.split_once(',')?;
            Some((geoid.parse().ok()?, income.trim().parse().ok()))
        })
        .collect();
    let (mut matched, mut no_estimate, mut not_in_table) = (0usize, 0usize, 0usize);
    for bg in &mut out.features {
        match table.get(&(bg.geoid as u64)) {
            Some(Some(income)) => {
                bg.income = Some(*income);
                matched += 1;
            }
            Some(None) => no_estimate += 1,
            None => not_in_table += 1,
        }
    }
    println!(
        "income join: {matched} of {} block groups have an income; {} missing ({no_estimate} no estimate, {not_in_table} not in the ACS table)",
        out.features.len(),
        no_estimate + not_in_table
    );
}

/// Loads `names/2023_Gaz_counties_national.txt` (written by
/// `fetch_us_block_groups.sh`'s county-names step): the Census's 2023
/// county gazetteer, tab-delimited `USPS GEOID ANSICODE NAME ...`, GEOID
/// the 5-digit state+county FIPS. Chosen over the older
/// `codes2020/national_county2020.txt` the Census also publishes: that
/// file still lists Connecticut's eight pre-2022 counties (FIPS
/// 09001-09015), not the 09110-09190 planning regions TIGER2023 actually
/// ships, while the 2023 gazetteer has both those and Puerto Rico's 78
/// municipios already in this exact GEOID+NAME shape -- one file, no
/// STATEFP+COUNTYFP concatenation to get wrong. A missing file returns an
/// empty map rather than panicking; every lookup then falls back to
/// `county_label`'s `"County NNN"`.
pub fn load_county_names(dir: &std::path::Path) -> HashMap<u32, String> {
    let path = dir.join("names").join("2023_Gaz_counties_national.txt");
    let Ok(text) = std::fs::read_to_string(&path) else {
        println!(
            "county names: {} missing -- run demos/data/fetch_us_block_groups.sh; popups fall back to \"County NNN\"",
            path.display()
        );
        return HashMap::new();
    };
    parse_county_names(&text)
}

/// The `income` property as ONE typed column -- `None` where the Census has
/// no estimate -- handed to `set_source_columns`, so no per-feature map is
/// ever built (B9: columnar overlay properties).
pub fn income_column(data: &BlockGroups) -> Vec<Option<f64>> {
    data.features.iter().map(|bg| bg.income).collect()
}

pub fn paint_object(json: &str) -> serde_json::Map<String, serde_json::Value> {
    serde_json::from_str(json).expect("the demo's paint JSON is valid")
}

/// Declares the sources' paint and pushes the block groups. The ONE code
/// path the window and `--measure` share.
///
/// One fill source PER STATE by default, each outlined with
/// `fill-outline-color`. `--one-source` pushes the whole country as ONE
/// source, which the chunked overlay upload draws (it used to draw nothing
/// past 4 GiB). Returns the synchronous push time, `None` for the outline
/// (it costs no push of its own now), and how many sources were pushed.
pub fn push(
    view: &mut MapView,
    data: &BlockGroups,
    outline: bool,
    one_source: bool,
    cx: &mut Context<MapView>,
) -> (Duration, Option<Duration>, u64) {
    let started = Instant::now();
    let incomes = income_column(data);
    let properties_time = started.elapsed();
    let rings: Vec<&[[f64; 2]]> = data.rings.iter().map(|&(a, b)| &data.coords[a..b]).collect();
    let polygons: Vec<&[&[[f64; 2]]]> = data.polygons.iter().map(|&(a, b)| &rings[a..b]).collect();
    let groups: Vec<(String, usize, usize)> = if one_source {
        vec![(FILL_SOURCE.to_string(), 0, data.features.len())]
    } else {
        data.states.iter().map(|(fips, a, b)| (state_source(fips), *a, *b)).collect()
    };
    println!("properties build (demo side, before the push): {:.1} ms", properties_time.as_secs_f64() * 1e3);
    let mut fill_time = Duration::ZERO;
    let outline_time: Option<Duration> = None;
    // The hairline is the fill's own `fill-outline-color`: a line list over
    // the fill's vertices (MapLibre's `fill_bucket`), not a second source
    // tessellated as 0.5 px lines -- which cost 3 vertices per coordinate,
    // ~19 GB nationally, and could not draw as one source at all.
    let fill_paint = fill_paint_json(outline);

    for (name, first, last) in &groups {
        view.declare_source_paint(name, &paint_object(&fill_paint)).expect("fill paint must be accepted");
        // Basemap place names and road shields over the choropleth, as
        // Kyle Walker's reference inserts its fill before the style's label
        // layers (`beforeId`). Below the labels is the engine's default;
        // said out loud here because it is the point of the picture.
        view.set_source_below_labels(name, true);
        let fills: Vec<FeatureInput<'_>> = data.features[*first..*last]
            .iter()
            .map(|bg| FeatureInput {
                id: Some(FeatureId::Number(bg.geoid)),
                geometry: GeometryInput::Polygon(&polygons[bg.polygons.0..bg.polygons.1]),
                paint: FeaturePaint::default(),
            })
            .collect();
        let columns = [("income", PropertyColumn::Number(&incomes[*first..*last]))];
        if std::env::var_os("US_BG_MAP_DOOR").is_some() {
            // The unchanged per-feature-map door, for comparison: the maps
            // are built here (timed with the push), the columns inside it.
            let started = Instant::now();
            let maps = income_maps(&incomes[*first..*last]);
            let data_fills: Vec<gpui_maps::overlay::DataFeatureInput<'_>> = fills
                .iter()
                .zip(&maps)
                .map(|(feature, properties)| gpui_maps::overlay::DataFeatureInput {
                    feature: feature.clone(),
                    properties: Arc::clone(properties),
                })
                .collect();
            view.set_source_data_from(name, &data_fills, cx).expect("block groups must be accepted");
            fill_time += started.elapsed();
            continue;
        }
        let started = Instant::now();
        view.set_source_columns(name, &fills, &columns, cx).expect("block groups must be accepted");
        fill_time += started.elapsed();
    }
    // The slider repaints every source on every drag step, and a state
    // under the engine's tiling threshold (DC, Delaware, the territories)
    // would otherwise be one whole-source mesh with its colours baked in:
    // a replan per step, which the pump then waits on. Tiled now, every
    // source's drag step is a table rewrite.
    for (name, _, _) in &groups {
        view.tile_source_for_paint_updates(name, cx);
    }
    (fill_time, outline_time, groups.len() as u64)
}

/// `US_BG_MAP_DOOR=1`: one property map PER FEATURE, the shape a caller who
/// does not share maps hands to `set_source_data_from`.
pub fn income_maps(incomes: &[Option<f64>]) -> Vec<Arc<gpui_maps::overlay::FeatureProperties>> {
    incomes
        .iter()
        .map(|income| match income {
            Some(income) => Arc::new(HashMap::from([(
                "income".to_string(),
                gpui_maps::overlay::TagValue::Number(*income),
            )])),
            None => gpui_maps::overlay::no_properties(),
        })
        .collect()
}

/// `--geojson FILE`: the string door. Fill only -- an overlay polygon draws
/// no outline, and deriving one would mean parsing the file here. The
/// converted GeoJSON carries no `income`, so every feature takes the grey
/// no-data arm.
pub fn push_geojson(view: &mut MapView, text: &str, cx: &mut Context<MapView>) -> Duration {
    view.declare_source_paint(FILL_SOURCE, &paint_object(&fill_paint_json(false))).expect("fill paint must be accepted");
    let started = Instant::now();
    view.set_source(FILL_SOURCE, text, cx).expect("the GeoJSON must be accepted");
    started.elapsed()
}

pub enum Payload {
    Shapes(BlockGroups),
    GeoJson(String),
}

pub fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("a tokio runtime")
}

/// `1` or `true` (case insensitive) turns `GPUI_MAP_HOVER_DEBUG` on;
/// anything else, including `0`, unset or empty, is off -- being SET is
/// not enough, so `GPUI_MAP_HOVER_DEBUG=0` (e.g. left over in a shell from
/// disabling something else) must not turn this on.
fn hover_debug_value(v: Option<&str>) -> bool {
    matches!(v.map(|v| v.trim().to_ascii_lowercase()).as_deref(), Some("1") | Some("true"))
}

/// Whether `GPUI_MAP_HOVER_DEBUG` is on (read once). See [`hover_debug_value`].
fn hover_debug() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| hover_debug_value(std::env::var("GPUI_MAP_HOVER_DEBUG").ok().as_deref()))
}

/// The window root: the map, focus (see `examples/mapbox_streets_demo.rs`'s
/// `FocusedMap`, which this used to be before it grew a legend), and the
/// legend card layered over the map as ordinary gpui elements -- app-side,
/// not an engine feature. `MapView::set_chrome_above_map` is what makes
/// that layering possible: without it the map's own GPU layer draws OVER
/// gpui's, and the card would be composited but invisible.
///
/// The map, and the one thing a host owes it -- focus: the library never
/// takes focus, and the map's handle exists only after its first prepaint,
/// so the root focuses it on every render (a no-op once it has focus).
pub struct DemoRoot {
    pub map: Entity<MapView>,
    /// GEOID -> (income, land area m^2), for the hover tooltip. Empty for
    /// `--geojson`, whose features carry no properties the demo reads.
    pub lookup: Arc<HashMap<u64, (Option<f64>, f64)>>,
    /// 5-digit state+county FIPS -> county (or county-equivalent) name,
    /// for the hover tooltip's title line. See `load_county_names`; empty
    /// if the names file was never fetched, in which case the tooltip
    /// falls back to `"County NNN"`.
    pub counties: Arc<HashMap<u32, String>>,
    /// The hover card, its highlight and click-to-pin: the shared Studio
    /// component (`popup.rs`), its card built by [`census_card`] from
    /// [`tooltip_lines`] -- the browser popup's words.
    pub popup: Entity<crate::popup::MapPopup>,
    /// [`Self::applied`], shared with the card's formatter: a block group
    /// the range hides gets no card.
    pub applied_shared: std::rc::Rc<std::cell::Cell<IncomeRange>>,
    /// This panel's own box in window coordinates, `(x, y, w, h)`,
    /// recorded at each prepaint by [`area_probe`]. Every pointer event
    /// arrives in WINDOW pixels, and the panel starts at the window's
    /// origin only standalone: in the gallery the sidebar and title bar
    /// offset it. The slider, the histogram hover, the tooltip and
    /// `fly_to_bounds` all map through this, never a fixed origin.
    pub area: std::rc::Rc<std::cell::Cell<(f32, f32, f32, f32)>>,
    /// Every fill source the push created (one per state, or one).
    pub sources: Arc<[String]>,
    /// Whether the sources were declared with `fill-outline-color`.
    pub outline: bool,
    /// The slider's selection, as the user last set it.
    pub wanted: IncomeRange,
    /// The selection the map is drawing (or re-baking towards).
    pub applied: IncomeRange,
    /// A coalescing retry is scheduled: at most one paint update is ever in
    /// flight, and the latest selection wins.
    pub retry_scheduled: bool,
    /// Which handle a drag holds.
    pub dragging: Option<Handle>,
    /// The legend's measured layout, from the last render: the slider's
    /// mouse handlers map a window x back to an income through it.
    pub layout: LegendLayout,
    /// Paint-update timings, for `--slider-sweep`.
    pub stats: SliderStats,
    /// The measured layout has been printed once.
    pub layout_printed: bool,
    /// All block groups' incomes, binned once at load (the grey bars).
    pub overall: Arc<Histogram>,
    /// The block groups in view, recounted each time the camera settles
    /// (the coloured bars); `None` until the first count lands.
    pub in_view: Option<Histogram>,
    /// Per-state and per-metro income breakdowns, built once on the
    /// background executor after the window opens (`start_breakdowns`);
    /// `None` until then, or when the fetch script's tables are missing.
    pub breakdowns: Option<Arc<breakdowns::Breakdowns>>,
    /// The breakdowns panel, its own (cached) view: see `panel::PanelView`.
    pub panel_view: Entity<panel::PanelView>,
    /// The state the panel is pointing at (every other state dims), as
    /// wanted and as the map last drew it -- coalesced by `pump` with the
    /// range.
    pub focus_wanted: Option<u32>,
    pub focus_applied: Option<u32>,
    /// The bar under the pointer, for the counts line.
    pub hist_hover: Option<usize>,
    /// The bar of the block group hovered ON THE MAP (the popup's pick):
    /// the histogram lights it.
    pub map_hist_hover: Option<usize>,
    /// A brush drawn on the histogram: the stop it started at. The drag
    /// sets the slider's range (the histogram and the slider share an
    /// axis), so a brush is a slider move.
    pub hist_brush: Option<usize>,
    /// The bar the pointer previews: the map shows only its block groups
    /// until the pointer leaves (a paint-table update, as a slider step).
    pub preview: Option<IncomeRange>,
    /// The histogram strip's window box at its last paint (for the probe).
    pub hist_bounds: std::rc::Rc<std::cell::Cell<(f32, f32, f32, f32)>>,
    /// The window's key-owning shell (t/g/s), drawn in the map's place;
    /// `None` draws the bare map.
    pub shell: Option<Entity<shell::DemoShell>>,
    /// When the root gives the map the keyboard: every frame standalone
    /// ([`DemoRoot::new`]), once in the gallery ([`CensusPanel`]).
    pub focus_policy: shell::FocusPolicy,
    pub focused_once: bool,
}

/// What each paint update cost, and which engine path it took.
#[derive(Default)]
pub struct SliderStats {
    /// Synchronous `update_source_paint` time, summed over the sources.
    pub sync: Vec<Duration>,
    pub table_only: usize,
    pub rebake: usize,
    pub replan: usize,
}

impl DemoRoot {
    pub fn new(
        map: Entity<MapView>,
        lookup: Arc<HashMap<u64, (Option<f64>, f64)>>,
        counties: Arc<HashMap<u32, String>>,
        sources: Arc<[String]>,
        outline: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let lookup_for_hist = Arc::clone(&lookup);
        let root = cx.weak_entity();
        let panel_view = cx.new(|_| panel::PanelView::new(root, false));
        let applied_shared = std::rc::Rc::new(std::cell::Cell::new(IncomeRange::full()));
        let popup = {
            let (lookup, counties, applied) = (Arc::clone(&lookup), Arc::clone(&counties), applied_shared.clone());
            let config = crate::popup::PopupConfig::new()
                .only_listed()
                .highlight(|_| gpui_maps::overlay::HighlightStyle {
                    fill_color: Some(gpui_maps::style::Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.18 }),
                    ..gpui_maps::overlay::HighlightStyle::default()
                });
            let config = crate::popup::PopupConfig {
                fallback: Some(crate::popup::Formatter::Custom(std::rc::Rc::new(move |pick| {
                    let gpui_maps::map_view::MapObject::Feature(feature) = &pick.object else { return None };
                    let Some(FeatureId::Number(n)) = feature.id else { return None };
                    let geoid = n as u64;
                    let row = lookup.get(&geoid).copied();
                    // A hidden block group is still hit-tested by the engine
                    // (it is drawn, at alpha 0): no card for it.
                    if row.is_some_and(|(income, _)| !applied.get().contains(income)) {
                        return None;
                    }
                    Some(census_card(geoid, row, &counties))
                }))),
                ..config
            };
            cx.new(|cx| crate::popup::MapPopup::new(&map, config, cx))
        };
        // A block group hovered on the map lights its income bar.
        {
            let lookup = Arc::clone(&lookup);
            cx.observe(&popup, move |this: &mut Self, popup, cx| {
                let bin = popup.read(cx).hovered_pick().or(popup.read(cx).pinned_pick()).and_then(|pick| {
                    let gpui_maps::map_view::MapObject::Feature(feature) = &pick.object else { return None };
                    let Some(FeatureId::Number(n)) = feature.id else { return None };
                    lookup.get(&(n as u64)).and_then(|(income, _)| income.map(income_bin))
                });
                if bin != this.map_hist_hover {
                    this.map_hist_hover = bin;
                    cx.notify();
                }
            })
            .detach();
        }
        start_hist_probe(cx.weak_entity(), cx);
        Self {
            map,
            lookup,
            counties,
            popup,
            applied_shared,
            area: std::rc::Rc::new(std::cell::Cell::new((0.0, 0.0, 0.0, 0.0))),
            sources,
            outline,
            wanted: IncomeRange::full(),
            applied: IncomeRange::full(),
            retry_scheduled: false,
            dragging: None,
            layout: LegendLayout::default(),
            stats: SliderStats::default(),
            layout_printed: false,
            overall: Arc::new({
                let started = Instant::now();
                let h = Histogram::of_incomes(lookup_for_hist.values().map(|(income, _)| *income));
                if !lookup_for_hist.is_empty() {
                    println!(
                        "histogram (all): {} block groups binned in {:.2} ms",
                        h.total + h.no_data,
                        started.elapsed().as_secs_f64() * 1e3
                    );
                }
                h
            }),
            in_view: None,
            breakdowns: None,
            panel_view,
            focus_wanted: None,
            focus_applied: None,
            hist_hover: None,
            map_hist_hover: None,
            hist_brush: None,
            preview: None,
            hist_bounds: std::rc::Rc::new(std::cell::Cell::new((0.0, 0.0, 0.0, 0.0))),
            shell: None,
            focus_policy: shell::FocusPolicy::EveryFrame,
            focused_once: false,
        }
    }

    /// Recounts the in-view histogram whenever the camera has held still
    /// for one poll (~120 ms) at a pose not yet counted -- on the
    /// background executor, since a full-US count projects 242 k boxes.
    /// The same pass groups the visible set into the in-view state and
    /// metro breakdowns once `breakdowns` has landed (a pose counted
    /// before that is counted again when it does).
    pub fn start_in_view(&mut self, items: Arc<[ViewItem]>, cx: &mut Context<Self>) {
        // Weak: a strong handle here kept the map (and its GPU buffers)
        // alive up to one 120 ms poll after the gallery closed the panel,
        // past the leak gate's close sample (gallery Task 12's first
        // `--cycle census,basic` run: `closed_map_alive_at_build 1`).
        let map = self.map.downgrade();
        // The boxes' coarse grid, once: each recount tests only the cells
        // the camera might see (`view::in_view_indices_indexed`, exact).
        let index = Arc::new(ViewIndex::build(&items));
        cx.spawn(async move |this, cx| {
            let mut counted = None;
            let mut last = None;
            loop {
                cx.background_executor().timer(Duration::from_millis(120)).await;
                let Ok(pose) = map.read_with(cx, |view, _| (*view.camera(), view.globe_enabled())) else { return };
                // The regroup runs only while the In view tab is on screen;
                // opening it changes `groups` and so recounts.
                let Ok(groups) = this.read_with(cx, |this, cx| {
                    this.breakdowns.clone().filter(|_| this.panel_view.read(cx).shows_in_view())
                }) else {
                    return;
                };
                let stable = last == Some(pose);
                last = Some(pose);
                if !stable || counted == Some((pose, groups.is_some())) || pose.0.viewport_width_px < 1.0 {
                    continue;
                }
                counted = Some((pose, groups.is_some()));
                let (n, items_bg, index_bg) = (items.len(), Arc::clone(&items), Arc::clone(&index));
                let (h, by_group, took, took_groups) = cx
                    .background_executor()
                    .spawn(async move {
                        let started = Instant::now();
                        let visible = in_view_indices_indexed(&index_bg, &items_bg, &pose.0, pose.1);
                        let h = Histogram::of_bins(visible.iter().map(|&i| items_bg[i as usize].bin));
                        let took = started.elapsed();
                        let by_group = groups.map(|b| {
                            let v = b.in_view(&visible, IN_VIEW_GROUPS);
                            let top = v.metros.first().map(|m| b.metros[m.group].name.clone());
                            (v, top)
                        });
                        (h, by_group, took, started.elapsed() - took)
                    })
                    .await;
                println!(
                    "histogram (in view): {} of {} block groups at z{:.2} in {:.1} ms (background)",
                    h.total + h.no_data,
                    n,
                    pose.0.zoom,
                    took.as_secs_f64() * 1e3
                );
                let by_group = by_group.map(|(v, top)| {
                    println!(
                        "breakdowns (in view): {} states, {} metros (top {IN_VIEW_GROUPS}) in {:.1} ms (background){}",
                        v.states.len(),
                        v.metros.len(),
                        took_groups.as_secs_f64() * 1e3,
                        top.map(|name| format!("; most populous metro in view: {name}")).unwrap_or_default()
                    );
                    v
                });
                if this
                    .update(cx, |this, cx| {
                        this.in_view = Some(h);
                        if let Some(v) = by_group {
                            this.panel_view.update(cx, |p, cx| {
                                p.in_view = Some(v);
                                cx.notify();
                            });
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
        })
        .detach();
    }

    /// Builds the state and metro breakdowns on the background executor:
    /// `features` are `(GEOID, income)` in the push's feature order (the
    /// in-view items' order), joined to the population and metro tables the
    /// fetch script writes under `dir`.
    pub fn start_breakdowns(
        &mut self,
        dir: PathBuf,
        features: Vec<(u64, Option<f64>)>,
        boxes: Option<Arc<[ViewItem]>>,
        cx: &mut Context<Self>,
    ) {
        let n = features.len();
        self.start_breakdowns_with(move || breakdowns::load(&dir, features.into_iter()), n, boxes, cx);
    }

    /// [`Self::start_breakdowns`] for pack-loaded block groups: `rows`
    /// carry their own population (the pack's), joined to the pack's CBSA
    /// table -- the browser's `Breakdowns::from_texts` call, with no file
    /// read.
    pub fn start_breakdowns_from_pack(
        &mut self,
        rows: Vec<breakdowns::Row>,
        cbsa_text: String,
        boxes: Option<Arc<[ViewItem]>>,
        cx: &mut Context<Self>,
    ) {
        let n = rows.len();
        self.start_breakdowns_with(
            move || breakdowns::Breakdowns::from_texts(&rows, None, &cbsa_text, breakdowns::BinEdges::default()),
            n,
            boxes,
            cx,
        );
    }

    /// Runs `build` (`n` block groups) on the background executor, sets
    /// each group's fly-to bounds from `boxes`, and hands the result to the
    /// panel -- or its error.
    fn start_breakdowns_with(
        &mut self,
        build: impl FnOnce() -> Result<breakdowns::Breakdowns, String> + Send + 'static,
        n: usize,
        boxes: Option<Arc<[ViewItem]>>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let built = cx
                .background_executor()
                .spawn(async move {
                    let started = Instant::now();
                    build().map(|mut b| {
                        // Each state's and metro's extent, for the panel's
                        // fly-to: the in-view boxes, same feature order.
                        if let Some(items) = boxes {
                            let boxes: Vec<[f32; 4]> = items.iter().map(|i| i.bbox).collect();
                            b.set_bounds(&boxes);
                        }
                        (b, n, started.elapsed())
                    })
                })
                .await;
            let b = match built {
                Ok((b, n, took)) => {
                    println!(
                        "breakdowns: {n} block groups into {} states and {} metros in {:.1} ms (background)",
                        b.states.len(),
                        b.metros.len(),
                        took.as_secs_f64() * 1e3
                    );
                    b
                }
                Err(err) => {
                    println!("breakdowns: none -- {err}");
                    let _ = this.update(cx, |this, cx| this.set_breakdowns_error(err, cx));
                    return;
                }
            };
            let b = Arc::new(b);
            let _ = this.update(cx, |this, cx| {
                this.breakdowns = Some(Arc::clone(&b));
                this.panel_view.update(cx, |p, cx| {
                    p.breakdowns = Some(b);
                    cx.notify();
                });
            });
        })
        .detach();
    }

    pub fn settled(&self, cx: &App) -> bool {
        let view = self.map.read(cx);
        self.sources.iter().all(|name| view.source_paint_settled(name))
    }

    /// Selects `range` and brings the map to it -- now if nothing is in
    /// flight, otherwise as soon as the in-flight update has settled.
    pub fn request_range(&mut self, range: IncomeRange, cx: &mut Context<Self>) {
        if range != self.wanted {
            self.wanted = range;
            self.panel_view.update(cx, |p, cx| {
                p.range = range;
                cx.notify();
            });
            cx.notify();
        }
        self.pump(cx);
    }

    /// The panel says why it has nothing to show, instead of "Loading".
    pub fn set_breakdowns_error(&mut self, err: String, cx: &mut Context<Self>) {
        self.panel_view.update(cx, |p, cx| {
            p.error = Some(err);
            cx.notify();
        });
    }

    /// One paint update, if one is owed and none is in flight. A
    /// table-only update settles on the frame it is made in; a re-bake
    /// holds the next one back until its picture is whole.
    fn pump(&mut self, cx: &mut Context<Self>) {
        let target = self.target();
        if self.retry_scheduled || (target == self.applied && self.focus_wanted == self.focus_applied) {
            return;
        }
        if !self.settled(cx) {
            self.retry_scheduled = true;
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(Duration::from_millis(4)).await;
                this.update(cx, |this, cx| {
                    this.retry_scheduled = false;
                    this.pump(cx);
                })
                .ok();
            })
            .detach();
            return;
        }
        let json = target.paint_json(self.outline);
        let paint = paint_object(&json);
        if self.focus_wanted.is_none() && self.focus_applied.is_none() {
            self.apply_paint(&paint, cx);
        } else {
            // Only the sources whose look changed: all of them for a new
            // range, else the ones entering or leaving the dim.
            let dimmed = paint_object(&dimmed_paint_json(&json));
            let dim = |focus: Option<u32>, name: &str| focus.is_some() && source_state(name) != focus;
            let range_moved = target != self.applied;
            let sources = Arc::clone(&self.sources);
            let updates: Vec<(&str, &serde_json::Map<String, serde_json::Value>)> = sources
                .iter()
                .filter(|name| range_moved || dim(self.focus_wanted, name) != dim(self.focus_applied, name))
                .map(|name| (name.as_str(), if dim(self.focus_wanted, name) { &dimmed } else { &paint }))
                .collect();
            self.apply_paints(&updates, cx);
        }
        self.applied = target;
        self.focus_applied = self.focus_wanted;
        // A range that now hides the hovered (or pinned) block group closes
        // its card and highlight.
        if self.applied_shared.get() != self.applied {
            self.applied_shared.set(self.applied);
            self.popup.update(cx, |p, cx| p.refresh(cx));
        }
    }

    /// Points the map at one state (`Some(fips)`: every other state dims)
    /// or none. Only with per-state sources -- the national one has no
    /// state to single out.
    fn set_state_focus(&mut self, state: Option<u32>, cx: &mut Context<Self>) {
        if self.sources.len() < 2 || state == self.focus_wanted {
            return;
        }
        self.focus_wanted = state;
        self.pump(cx);
    }

    /// Eases the camera to frame `bounds` in the part of the window the
    /// panel does not cover.
    /// `covered`: how much of the window's right side the panel takes.
    pub fn fly_to_bounds(
        &mut self,
        bounds: breakdowns::MercatorBounds,
        max_zoom: f64,
        size: gpui::Size<gpui::Pixels>,
        covered: f32,
        cx: &mut Context<Self>,
    ) {
        // The panel's own size once it has been laid out (the gallery
        // gives it less than the window); `size` until then.
        let size = self.area_size().unwrap_or(size);
        let (w, h) = (f64::from(f32::from(size.width) - covered), f64::from(f32::from(size.height)));
        let (lon, lat, zoom) = bounds.fit(w, h, 40.0, max_zoom);
        self.map.update(cx, |view, cx| {
            view.ease_to(
                gpui_maps::camera_control::CameraTarget {
                    center: Some((lon, lat)),
                    zoom: Some(zoom),
                    anchor_offset_px: (-covered / 2.0, 0.0),
                    ..Default::default()
                },
                Duration::from_millis(900),
            );
            cx.notify();
        });
    }

    /// `update_source_paint` on every source, timed and counted.
    pub fn apply_paint(&mut self, paint: &serde_json::Map<String, serde_json::Value>, cx: &mut Context<Self>) {
        let sources = Arc::clone(&self.sources);
        let updates: Vec<_> = sources.iter().map(|name| (name.as_str(), paint)).collect();
        self.apply_paints(&updates, cx);
    }

    /// `update_source_paint` on each `(source, paint)`, timed and counted.
    fn apply_paints(&mut self, updates: &[(&str, &serde_json::Map<String, serde_json::Value>)], cx: &mut Context<Self>) {
        use gpui_maps::map_view::SourcePaintPath;
        if updates.is_empty() {
            return;
        }
        let started = Instant::now();
        let paths: Vec<SourcePaintPath> = self.map.update(cx, |view, cx| {
            updates
                .iter()
                .map(|(name, paint)| view.update_source_paint(name, paint, cx).expect("the demo's paint is valid"))
                .collect()
        });
        self.stats.sync.push(started.elapsed());
        if paths.iter().all(|p| *p == SourcePaintPath::TableOnly) {
            self.stats.table_only += 1;
        } else {
            if paths.contains(&SourcePaintPath::Replan) {
                self.stats.replan += 1;
            } else {
                self.stats.rebake += 1;
            }
        }
    }

    /// The panel's window origin, `(x, y)` (see [`Self::area`]).
    pub fn origin(&self) -> (f32, f32) {
        let (x, y, _, _) = self.area.get();
        (x, y)
    }

    /// The panel's laid-out size, `None` before its first prepaint.
    pub fn area_size(&self) -> Option<gpui::Size<gpui::Pixels>> {
        let (_, _, w, h) = self.area.get();
        (w > 0.0 && h > 0.0).then(|| gpui::size(gpui::px(w), gpui::px(h)))
    }

    /// A press at window-x `x` on the slider -- see [`slider_grab`]; on
    /// the bare track it jumps the nearer knob there.
    fn press_at(&mut self, x: f32, cx: &mut Context<Self>) {
        self.dragging = slider_grab(SliderAxis::of(&self.layout, self.origin().0), self.wanted, x);
        if self.dragging.is_none() {
            self.drag_to(x, cx);
        }
    }

    /// The pointer at window-x `x` with the button held -- see [`slider_drag`].
    fn drag_to(&mut self, x: f32, cx: &mut Context<Self>) {
        let (held, range) = slider_drag(SliderAxis::of(&self.layout, self.origin().0), self.wanted, self.dragging, x);
        self.dragging = held;
        self.request_range(range, cx);
    }

    /// What the map should draw: the hovered bar's preview, else the
    /// slider's range.
    pub fn target(&self) -> IncomeRange {
        self.preview.unwrap_or(self.wanted)
    }

    /// A press on the histogram at window-x `x`: a brush starts there.
    pub fn hist_press(&mut self, x: f32, cx: &mut Context<Self>) {
        let stop = SliderAxis::of(&self.layout, self.origin().0).stop_at(x);
        self.hist_brush = Some(stop);
        self.preview = None;
        self.request_range(brush_range(stop, stop), cx);
    }

    /// The pointer at window-x `x` with a histogram brush held.
    pub fn hist_brush_to(&mut self, x: f32, cx: &mut Context<Self>) {
        let Some(anchor) = self.hist_brush else { return };
        let stop = SliderAxis::of(&self.layout, self.origin().0).stop_at(x);
        self.request_range(brush_range(anchor, stop), cx);
    }

    /// The bar under the pointer changed: preview it on the map (none: the
    /// slider's range again). Not while a brush or a knob is held.
    pub fn set_hist_hover(&mut self, bar: Option<usize>, cx: &mut Context<Self>) {
        if bar == self.hist_hover {
            return;
        }
        self.hist_hover = bar;
        let preview = if self.hist_brush.is_some() || self.dragging.is_some() { None } else { bar.map(bar_range) };
        if preview != self.preview {
            self.preview = preview;
            self.pump(cx);
        }
        cx.notify();
    }
}

/// The slider range a histogram brush from stop `a` to stop `b` selects
/// (either order; at least one stop wide).
pub fn brush_range(a: usize, b: usize) -> IncomeRange {
    let last = slider_stops().len() - 1;
    let (lo, hi) = (a.min(b), a.max(b));
    if hi > lo {
        IncomeRange { lo, hi }
    } else if lo < last {
        IncomeRange { lo, hi: lo + 1 }
    } else {
        IncomeRange { lo: last - 1, hi: last }
    }
}

/// The slider range nearest histogram bar `k`'s incomes.
pub fn bar_range(k: usize) -> IncomeRange {
    let (a, b) = bin_span(k);
    brush_range(slider::stop_at(income_fraction(a)), slider::stop_at(income_fraction(b)))
}

/// `GPUI_MAP_STUDIO_PROBE_BRUSH=a,b` (fractions of the histogram's width):
/// a brush on the income histogram, dispatched through the window once the
/// histogram has painted; `GPUI_MAP_STUDIO_PROBE_CHART_HOVER=f` then the
/// pointer over fraction `f` of it. For screenshots; bounded at 60 s.
fn start_hist_probe(root: gpui::WeakEntity<DemoRoot>, cx: &mut Context<DemoRoot>) {
    let pair = |name: &str| -> Option<(f32, Option<f32>)> {
        let v = std::env::var(name).ok()?;
        let mut it = v.split(',').map(|t| t.trim().parse::<f32>().ok());
        let a = it.next()??;
        Some((a, it.next().flatten()))
    };
    let brush = pair("GPUI_MAP_STUDIO_PROBE_BRUSH");
    let hover = pair("GPUI_MAP_STUDIO_PROBE_CHART_HOVER").map(|(a, _)| a);
    if brush.is_none() && hover.is_none() {
        return;
    }
    cx.spawn(async move |_, cx| {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            cx.background_executor().timer(Duration::from_millis(500)).await;
            if Instant::now() > deadline {
                eprintln!("census probe: no histogram after 60 s");
                return;
            }
            let Ok((x, y, w, h)) = root.update(cx, |r, _| r.hist_bounds.get()) else { return };
            if w <= 1.0 {
                continue;
            }
            cx.background_executor().timer(Duration::from_millis(3000)).await;
            let Some(handle) = cx.update(|cx| cx.active_window().or_else(|| cx.windows().first().copied())) else { return };
            let at = |f: f32| gpui::point(gpui::px(x + w * f), gpui::px(y + h * 0.5));
            let (b, m) = (gpui::MouseButton::Left, gpui::Modifiers::default());
            if let Some((a, to)) = brush {
                let _ = handle.update(cx, |_, window, cx| {
                    window.dispatch_event(gpui::PlatformInput::MouseDown(gpui::MouseDownEvent { button: b, position: at(a), modifiers: m, click_count: 1, first_mouse: false }), cx);
                    window.refresh();
                });
                cx.background_executor().timer(Duration::from_millis(150)).await;
                let end = at(to.unwrap_or(a));
                let _ = handle.update(cx, |_, window, cx| {
                    window.dispatch_event(gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent { position: end, pressed_button: Some(b), modifiers: m }), cx);
                    window.dispatch_event(gpui::PlatformInput::MouseUp(gpui::MouseUpEvent { button: b, position: end, modifiers: m, click_count: 1 }), cx);
                });
                let _ = root.update(cx, |r, _| eprintln!("census probe: brushed to {:?}", r.wanted));
            }
            if let Some(f) = hover {
                cx.background_executor().timer(Duration::from_millis(300)).await;
                let _ = handle.update(cx, |_, window, cx| {
                    window.dispatch_event(gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent { position: at(f), pressed_button: None, modifiers: m }), cx);
                });
                let _ = root.update(cx, |r, _| eprintln!("census probe: hover bar {:?}, preview {:?}", r.hist_hover, r.preview));
            }
            return;
        }
    })
    .detach();
}

/// The slider's axis in window x: where the knob at each stop sits, and
/// the stop nearest a pointer.
#[derive(Clone, Copy, Debug)]
pub struct SliderAxis {
    /// Window x of the bar's left end (stop 0's knob centre).
    left: f32,
    /// The bar's width.
    bar: f32,
}

impl SliderAxis {
    /// `origin_x`: the panel's own window x ([`DemoRoot::origin`]); 0
    /// standalone, the sidebar's width in the gallery.
    fn of(layout: &LegendLayout, origin_x: f32) -> Self {
        Self { left: origin_x + LEGEND_ORIGIN.0 + LEGEND_PAD, bar: layout.bar }
    }

    /// Window x of the knob at stop `i`.
    fn knob_x(self, i: usize) -> f32 {
        self.left + income_fraction(slider_stops()[i]) as f32 * self.bar
    }

    /// The stop nearest window-x `x`, clamped into the slider.
    fn stop_at(self, x: f32) -> usize {
        slider::stop_at(self.frac(x))
    }

    /// Window-x `x` as a fraction along the bar (unclamped).
    fn frac(self, x: f32) -> f64 {
        f64::from((x - self.left) / self.bar.max(1.0))
    }

    /// The bar as `census::slider` measures it: [`KNOB`] and one pixel as
    /// fractions of its width.
    fn track(self) -> slider::Track {
        slider::Track::of_px(f64::from(self.bar))
    }
}

/// What a press at window-x `x` on the slider holds (`census::slider::grab_on`,
/// shared with the browser's slider).
pub fn slider_grab(axis: SliderAxis, range: IncomeRange, x: f32) -> Option<Handle> {
    slider::grab_on(axis.track(), range, axis.frac(x))
}

/// The pointer at window-x `x` with the button held (`census::slider::drag_on`).
pub fn slider_drag(axis: SliderAxis, range: IncomeRange, held: Option<Handle>, x: f32) -> (Option<Handle>, IncomeRange) {
    slider::drag_on(axis.track(), range, held, axis.frac(x))
}

/// Whether `DemoRoot` gives the map the keyboard this render. Standalone
/// (`EveryFrame`): always. Gallery (`Once`): once, and only if nothing else
/// holds focus -- a census mounted after a long load must not take the
/// keyboard from the sidebar the user moved to meanwhile (the embedded
/// shell already focused the map once while the panel was loading).
pub fn takes_focus(policy: shell::FocusPolicy, handle_exists: bool, focused_once: bool, something_focused: bool) -> bool {
    policy.should_focus(handle_exists, focused_once) && (policy == shell::FocusPolicy::EveryFrame || !something_focused)
}

impl Render for DemoRoot {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The cards in the app's theme, as the shared popup and charts.
        crate::popup::sync_ink(cx);
        // Standalone: every render. The gallery (`focus_policy` `Once`):
        // the first render the handle exists, so the sidebar keeps the
        // keyboard after that; a click in the map (the shell's) takes it.
        let handle = self.map.read(cx).focus_handle();
        if takes_focus(self.focus_policy, handle.is_some(), self.focused_once, window.focused(cx).is_some()) {
            if let Some(handle) = handle {
                window.focus(&handle, cx);
                self.focused_once = true;
            }
        }
        use gpui::prelude::*;
        let preview = if self.shell.is_none() { shell::preview_layer(&self.map, window, cx) } else { None };
        // The breakdowns panel: a cached view, so a render of the root
        // that is not about it (a tooltip following the cursor) reuses its
        // last layout and paint.
        self.panel_view.update(cx, |p, cx| p.sync_column(cx));
        let panel_element = gpui::AnyView::from(self.panel_view.clone())
            .cached(gpui::StyleRefinement::default().absolute().top_0().left_0().size_full());
        self.layout = LegendLayout::measure(window);
        if !self.layout_printed {
            self.layout_printed = true;
            println!(
                "legend: card {:.0} px (bar {:.0}), scale factor {}; tick labels {:?} px wide, left edges {:?}",
                self.layout.card_width(),
                self.layout.bar,
                window.scale_factor(),
                self.layout.tick_width.map(|w| (w * 10.0).round() / 10.0),
                self.layout.tick_left.map(|l| (l * 10.0).round() / 10.0),
            );
        }
        // A held knob follows the pointer ANYWHERE -- over the card, the
        // map, or past the window's edge, where macOS keeps delivering a
        // drag -- so its move and release listeners are window-wide, not
        // hitbox-gated (the way `MapView::paint` carries a pan). A
        // hitbox-gated `on_mouse_move` stops at the window edge and leaves
        // the knob short of a fast drag.
        let entity = cx.entity().downgrade();
        let drag_listeners = (self.dragging.is_some() || self.hist_brush.is_some()).then(|| {
            gpui::canvas(
                |_, _, _| {},
                move |_, _, window, _| {
                    let moved = entity.clone();
                    window.on_mouse_event(move |event: &gpui::MouseMoveEvent, phase, _window, cx| {
                        if phase != gpui::DispatchPhase::Bubble {
                            return;
                        }
                        let _ = moved.update(cx, |this, cx| {
                            if this.hist_brush.is_some() {
                                if event.pressed_button == Some(gpui::MouseButton::Left) {
                                    this.hist_brush_to(f32::from(event.position.x), cx);
                                } else {
                                    this.hist_brush = None;
                                    cx.notify();
                                }
                                return;
                            }
                            if this.dragging.is_none() {
                                return;
                            }
                            if event.pressed_button == Some(gpui::MouseButton::Left) {
                                this.drag_to(f32::from(event.position.x), cx);
                            } else {
                                this.dragging = None;
                                cx.notify();
                            }
                        });
                    });
                    let released = entity.clone();
                    window.on_mouse_event(move |event: &gpui::MouseUpEvent, phase, _window, cx| {
                        if phase == gpui::DispatchPhase::Bubble && event.button == gpui::MouseButton::Left {
                            let _ = released.update(cx, |this, cx| {
                                this.dragging = None;
                                this.hist_brush = None;
                                cx.notify();
                            });
                        }
                    });
                },
            )
            .absolute()
            .size_0()
        });
        gpui::div()
            .relative()
            .size_full()
            .child(area_probe(self.area.clone()))
            .map(|root| {
                // A gpui test window has no platform window for the map's
                // Metal layer, so the slider tests draw a stand-in that
                // hit-tests like the map (a `Normal` hitbox, pressed ->
                // counted) -- see `tests::slider`.
                #[cfg(test)]
                return root.child(tests::slider::map_stand_in());
                #[cfg(not(test))]
                match &self.shell {
                    Some(shell) => root.child(shell.clone()),
                    None => root.child(self.map.clone()),
                }
            })
            // The hover / click layer, over the map and under the legend
            // and the Breakdowns panel (which occlude it).
            .child(crate::popup::input_layer(&self.popup, cx))
            // A bare map (no shell, whose own render runs the hook) takes
            // a story preview here instead.
            .children(preview)
            .child(
                legend(
                    &self.layout,
                    self.wanted,
                    cx.listener(|this, event: &gpui::MouseDownEvent, _window, cx| {
                        this.press_at(f32::from(event.position.x), cx);
                        cx.notify();
                    }),
                    Histograms {
                        overall: Arc::clone(&self.overall),
                        in_view: self.in_view.clone(),
                        hover: self.hist_hover.or(self.map_hist_hover),
                        bounds: self.hist_bounds.clone(),
                    },
                    cx.listener(|this, event: &gpui::MouseDownEvent, _window, cx| {
                        this.hist_press(f32::from(event.position.x), cx);
                        cx.stop_propagation();
                    }),
                    cx.listener(|this, event: &gpui::MouseMoveEvent, _window, cx| {
                        let local = f32::from(event.position.x) - this.origin().0;
                        let x = local - LEGEND_ORIGIN.0 - LEGEND_PAD;
                        let k = income_bin(this.layout.income_at(local));
                        let next = (0.0..=this.layout.bar).contains(&x).then_some(k);
                        this.set_hist_hover(next, cx);
                    }),
                    cx.listener(|this, hovered: &bool, _window, cx| {
                        if !*hovered && this.hist_hover.is_some() {
                            this.set_hist_hover(None, cx);
                        }
                    }),
                ),
            )
            .child(panel_element)
            .child(self.popup.clone())
            .children(drag_listeners)
    }
}

/// An empty, unpainted element over the whole panel that records the
/// panel's window box at each prepaint (pointer events arrive in window
/// pixels) -- `race_dots`' `origin_probe`, with the size too.
fn area_probe(area: std::rc::Rc<std::cell::Cell<(f32, f32, f32, f32)>>) -> impl IntoElement {
    use gpui::prelude::*;
    gpui::canvas(
        move |bounds, _, _| {
            area.set((
                f32::from(bounds.origin.x),
                f32::from(bounds.origin.y),
                f32::from(bounds.size.width),
                f32::from(bounds.size.height),
            ))
        },
        |_, _, _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// The hover card's five lines, every one from `census` -- the words the
/// browser's popup prints (`census::hover::popup_json`), so the two shells
/// cannot drift. `row` is `(income, ALAND in m²)`.
pub fn tooltip_lines(geoid: u64, row: Option<(Option<f64>, f64)>, counties: &HashMap<u32, String>) -> [String; 5] {
    use gpui_maps::census::hover::{area_line, geoid_line, income_line};
    let (income, aland_m2) = row.unwrap_or((None, f64::NAN));
    [
        popup_title(geoid, counties),
        popup_subtitle(geoid),
        income_line(income),
        area_line(aland_m2.is_finite().then_some((aland_m2 / 1e6) as f32)),
        geoid_line(geoid),
    ]
}

/// The hover card (the shared `popup` component's): the browser popup's
/// five lines ([`tooltip_lines`]), the last three as label / value rows
/// -- `"Median household income: $87,250"` reads `Median household
/// income | $87,250`, the same words.
pub fn census_card(geoid: u64, row: Option<(Option<f64>, f64)>, counties: &HashMap<u32, String>) -> crate::popup::Card {
    let [title, subtitle, income, area, geoid_line] = tooltip_lines(geoid, row, counties);
    let split = |line: &str, label: &str| match line.split_once(": ") {
        Some((k, v)) => (k.to_string(), v.to_string()),
        None => (label.to_string(), line.to_string()),
    };
    let geoid = match geoid_line.split_once(' ') {
        Some((k, v)) => (k.to_string(), v.to_string()),
        None => ("GEOID".to_string(), geoid_line),
    };
    crate::popup::Card {
        title,
        subtitle: Some(subtitle),
        rows: vec![split(&income, "Median household income"), split(&area, "Land area"), geoid],
        ..Default::default()
    }
}

/// The slider knobs' diameter.
pub const KNOB: f32 = slider::KNOB_PX as f32;

/// The card's inner padding, and the least clear space between two tick
/// labels.
pub const LEGEND_PAD: f32 = 12.0;
pub const LEGEND_LABEL_GAP: f32 = 12.0;
/// The card's top-left corner in the window.
pub const LEGEND_ORIGIN: (f32, f32) = (16.0, 16.0);
/// The captions the card can show, at their widest (every digit an 8),
/// measured instead of the live one so the card does not change width
/// under a drag.
pub const LEGEND_WIDEST_CAPTIONS: [&str; 4] = [
    "Showing $888.8K – $888.8K · no data hidden",
    "All incomes, and no data (grey)",
    // The histogram's hover line, and its key (plus two 8 px swatches and
    // three 4 px gaps, `LEGEND_SWATCHES`).
    "$888.8K – $888.8K: 888,888 in view · 888,888 all",
    "in view 888,888all 888,888 · each to its own peak",
];
/// What the histogram key's swatches and gaps add to its measured text.
pub const LEGEND_SWATCHES: f32 = 2.0 * 8.0 + 3.0 * 4.0;
/// The narrowest the bar is drawn, so a short set of labels does not
/// shrink the card below the title.
pub const LEGEND_MIN_BAR: f32 = 206.0;

/// The legend card's geometry, from measured text: the bar is as wide as
/// it has to be for every tick label to sit centred under its stop (the two
/// end labels flush with the bar's ends, so they cannot overflow the card)
/// with at least [`LEGEND_LABEL_GAP`] between neighbours.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LegendLayout {
    /// The bar's (and slider's) width; the card is this plus two paddings.
    bar: f32,
    /// Each tick label's left edge, relative to the bar's left edge.
    tick_left: [f32; 5],
    /// Each tick label's measured width.
    tick_width: [f32; 5],
}

impl LegendLayout {
    /// `widths` are the tick labels' measured widths, `min_bar` the least
    /// bar width anything else in the card (title, caption) needs.
    fn solve(widths: [f32; 5], min_bar: f32) -> Self {
        let left_of = |bar: f32, i: usize| {
            let f = LEGEND_TICKS[i].1;
            if i == 0 {
                0.0
            } else if i == 4 {
                bar - widths[4]
            } else {
                f * bar - widths[i] / 2.0
            }
        };
        // Each neighbour pair's gap is linear in the bar width:
        // left(i+1) - left(i) - w(i) >= GAP  <=>  bar >= (GAP + ...) / df.
        // Every label's left edge is `f * bar` minus a constant (0 for the
        // first, its width for the last, half its width between), so each
        // gap grows by `df * bar`.
        let mut bar = min_bar;
        for i in 0..4 {
            let df = LEGEND_TICKS[i + 1].1 - LEGEND_TICKS[i].1;
            let at_zero = left_of(0.0, i + 1) - left_of(0.0, i) - widths[i];
            bar = bar.max((LEGEND_LABEL_GAP - at_zero) / df);
        }
        let bar = bar.ceil();
        Self { bar, tick_left: std::array::from_fn(|i| left_of(bar, i)), tick_width: widths }
    }

    /// Measures the tick labels (and the widest caption) with gpui's own
    /// text system at the legend's `text_xs` size, and solves.
    fn measure(window: &Window) -> Self {
        let size = window.rem_size() * 0.75;
        let font = window.text_style().font();
        let width = |text: &str| -> f32 {
            let run = gpui::TextRun {
                len: text.len(),
                font: font.clone(),
                color: gpui::black(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            f32::from(window.text_system().shape_line(text.to_string().into(), size, &[run], None).width)
        };
        let widths = LEGEND_TICKS.map(|(label, _)| width(label));
        let caption = LEGEND_WIDEST_CAPTIONS
            .iter()
            .enumerate()
            .map(|(i, c)| width(c) + if i == 3 { LEGEND_SWATCHES } else { 0.0 })
            .fold(0.0, f32::max);
        Self::solve(widths, LEGEND_MIN_BAR.max(caption))
    }

    fn card_width(&self) -> f32 {
        self.bar + 2.0 * LEGEND_PAD
    }

    /// The income under panel-x `x` (window x less [`DemoRoot::origin`]),
    /// clamped to the ramp.
    fn income_at(&self, x: f32) -> f64 {
        let (first, last) = (VIRIDIS_STOPS[0].0, VIRIDIS_STOPS[VIRIDIS_STOPS.len() - 1].0);
        let t = f64::from(((x - LEGEND_ORIGIN.0 - LEGEND_PAD) / self.bar.max(1.0)).clamp(0.0, 1.0));
        first + t * (last - first)
    }
}

/// The legend card: white, rounded, a subtle shadow, top-left over the map
/// -- Kyle Walker's mapgl reference's own legend, redrawn in gpui elements
/// rather than carried by the engine -- with a two-handle income range
/// slider under the ramp. `.occlude()` stops the map underneath from also
/// receiving drags/scrolls that land on the card.
pub fn legend(
    layout: &LegendLayout,
    range: IncomeRange,
    on_slider_down: impl Fn(&gpui::MouseDownEvent, &mut Window, &mut App) + 'static,
    hist: Histograms,
    on_hist_down: impl Fn(&gpui::MouseDownEvent, &mut Window, &mut App) + 'static,
    on_hist_move: impl Fn(&gpui::MouseMoveEvent, &mut Window, &mut App) + 'static,
    on_hist_hover: impl Fn(&bool, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    use gpui::prelude::*;
    use gpui::{linear_color_stop, linear_gradient, px, rgb};

    let stops = slider_stops();
    let (lo_x, hi_x) =
        (income_fraction(stops[range.lo]) as f32 * layout.bar, income_fraction(stops[range.hi]) as f32 * layout.bar);

    // One flex_1 segment per adjacent pair of viridis stops: gpui's
    // linear_gradient takes only two colour stops, so a continuous 9-stop
    // ramp is 8 equal-width two-colour gradients placed edge to edge. The
    // segments share exact colours at every boundary, so the seams don't
    // show. Outside the selection the bar is washed out, as the map is.
    let veil = |left: f32, width: f32| {
        gpui::div().absolute().top_0().h_full().left(px(left)).w(px(width.max(0.0))).bg(crate::popup::ink_a(0xffffffb8))
    };
    let bar = gpui::div()
        .relative()
        .w(px(layout.bar))
        .h(px(10.0))
        .child(
            gpui::div().flex().flex_row().size_full().rounded(px(2.0)).overflow_hidden().children(
                VIRIDIS_STOPS.windows(2).map(|pair| {
                    let (from, to) = (pair[0].1, pair[1].1);
                    gpui::div()
                        .flex_1()
                        .h_full()
                        .bg(linear_gradient(90.0, linear_color_stop(rgb(from), 0.0), linear_color_stop(rgb(to), 1.0)))
                }),
            ),
        )
        .child(veil(0.0, lo_x))
        .child(veil(hi_x, layout.bar - hi_x));

    // The slider: a thin track, the selection on it, two round handles.
    // Mouse-down on a knob grabs it, between the knobs grabs the band, and
    // elsewhere on the track jumps the nearer handle (the root's and the
    // card's mouse-move handlers carry the drag, see `DemoRoot::render`).
    // The slider's box is the bar's width plus a knob's width, so a knob
    // sitting on either end -- both do at the full span, the default -- is
    // pressable across its whole face, not only its inner half.
    let knob = |x: f32| {
        gpui::div()
            .absolute()
            .top(px(1.0))
            .left(px(x))
            .size(px(KNOB))
            .rounded_full()
            .bg(crate::popup::ink(0xffffff))
            .border_2()
            .border_color(crate::popup::ink(0x1f2937))
            .shadow_sm()
    };
    let slider = gpui::div()
        .id("income-slider")
        .relative()
        .ml(px(-KNOB / 2.0))
        .w(px(layout.bar + KNOB))
        .h(px(16.0))
        .cursor_pointer()
        .debug_selector(|| "income-slider".into())
        .on_mouse_down(gpui::MouseButton::Left, on_slider_down)
        .child(gpui::div().absolute().top(px(7.0)).left(px(KNOB / 2.0)).w(px(layout.bar)).h(px(2.0)).rounded_full().bg(crate::popup::ink(0xd1d5db)))
        .child(gpui::div().absolute().top(px(6.0)).left(px(KNOB / 2.0 + lo_x)).w(px(hi_x - lo_x)).h(px(4.0)).bg(crate::popup::ink(0x1f2937)))
        // The band between the knobs' faces drags as a whole
        // (`slider_grab`): an open hand over it. Only a cursor -- the
        // press is the slider's.
        .children((hi_x - lo_x > KNOB).then(|| {
            gpui::div()
                .absolute()
                .top_0()
                .h_full()
                .left(px(lo_x + KNOB))
                .w(px(hi_x - lo_x - KNOB))
                .cursor(gpui::CursorStyle::OpenHand)
        }))
        .child(knob(lo_x))
        .child(knob(hi_x));

    let histogram = histogram_strip(layout, range, &hist)
        .id("income-histogram")
        .debug_selector(|| "income-histogram".into())
        .cursor_crosshair()
        .on_mouse_down(gpui::MouseButton::Left, on_hist_down)
        .on_mouse_move(on_hist_move)
        .on_hover(on_hist_hover);
    let hist_line = hist_caption(&hist);

    let tick_row = gpui::div().relative().w(px(layout.bar)).h(px(16.0)).children(LEGEND_TICKS.iter().enumerate().map(
        |(i, (label, _))| {
            gpui::div()
                .absolute()
                .top_0()
                .left(px(layout.tick_left[i]))
                .text_xs()
                .text_color(crate::popup::ink(0x4b5563))
                .whitespace_nowrap()
                .child(*label)
        },
    ));

    let caption = range.caption();

    gpui::div()
        .id("legend")
        .absolute()
        .top(px(LEGEND_ORIGIN.1))
        .left(px(LEGEND_ORIGIN.0))
        .w(px(layout.card_width()))
        .p(px(LEGEND_PAD))
        .flex()
        .flex_col()
        .gap(px(8.0))
        .bg(crate::popup::ink(0xffffff))
        .rounded(px(6.0))
        .shadow_md()
        .occlude()
        .child(gpui::div().text_sm().font_weight(gpui::FontWeight::SEMIBOLD).text_color(crate::popup::ink(0x1f2937)).child(
            "Median household income",
        ))
        .child(gpui::div().flex().flex_col().gap(px(4.0)).child(bar).child(slider).child(histogram))
        .child(tick_row)
        .child(hist_line)
        .child(
            gpui::div()
                .w_full()
                .flex()
                .justify_center()
                .text_xs()
                .text_color(crate::popup::ink(0x6b7280))
                .whitespace_nowrap()
                .child(caption),
        )
}

/// What the legend's histogram strip draws.
pub struct Histograms {
    overall: Arc<Histogram>,
    in_view: Option<Histogram>,
    hover: Option<usize>,
    /// Where the strip painted, for the screenshot probe.
    bounds: std::rc::Rc<std::cell::Cell<(f32, f32, f32, f32)>>,
}

/// Two histograms on the slider's axis, painted as quads (166 bars, no
/// element per bar): all block groups in grey behind, those in view in the
/// ramp's colour in front, each scaled to its own peak. Bars outside the
/// slider's selection dim; a bar cut by a handle is dimmed only past it.
pub fn histogram_strip(layout: &LegendLayout, range: IncomeRange, hist: &Histograms) -> gpui::Div {
    use gpui::prelude::*;
    let bar = layout.bar;
    let stops = slider_stops();
    let (sel_lo, sel_hi) =
        (income_fraction(stops[range.lo]) as f32 * bar, income_fraction(stops[range.hi]) as f32 * bar);
    let overall = Arc::clone(&hist.overall);
    let in_view = hist.in_view.clone();
    let hover = hist.hover;
    let probe = hist.bounds.clone();
    let dark = crate::popup::ink_is_dark();
    gpui::div().relative().w(px(bar)).h(px(HIST_HEIGHT)).child(
        gpui::canvas(
            |_, _, _| {},
            move |bounds, _, window, _| {
                probe.set((f32::from(bounds.origin.x), f32::from(bounds.origin.y), f32::from(bounds.size.width), f32::from(bounds.size.height)));
                let (ox, bottom) = (bounds.origin.x, bounds.origin.y + bounds.size.height);
                // One bar: `[x0, x1)` along the axis, `h` tall, split where
                // the selection's edges cut it.
                let mut quad = |x0: f32, x1: f32, h: f32, inside: gpui::Rgba, outside: gpui::Rgba| {
                    if h <= 0.0 || x1 <= x0 {
                        return;
                    }
                    let cuts = [x0, sel_lo.clamp(x0, x1), sel_hi.clamp(x0, x1), x1];
                    for (i, w) in cuts.windows(2).enumerate() {
                        if w[1] > w[0] {
                            let color = if i == 1 { inside } else { outside };
                            window.paint_quad(gpui::fill(
                                gpui::Bounds::new(
                                    gpui::point(ox + px(w[0]), bottom - px(h)),
                                    gpui::size(px(w[1] - w[0]), px(h)),
                                ),
                                color,
                            ));
                        }
                    }
                };
                let x_of = |k: usize| {
                    let (a, b) = bin_span(k);
                    (income_fraction(a) as f32 * bar, income_fraction(b) as f32 * bar)
                };
                let height = |count: u32, peak: u32| {
                    if count == 0 || peak == 0 { 0.0 } else { (count as f32 / peak as f32 * HIST_HEIGHT).max(1.0) }
                };
                // The hovered bar's whole column, clearly (the review found
                // an 8% tint faint), widened to at least 3 px so a narrow
                // bin still reads, and capped in the charts' hover accent.
                let hovered = hover.map(|k| {
                    let (x0, x1) = x_of(k);
                    let mid = (x0 + x1) / 2.0;
                    (x0.min(mid - 1.5), x1.max(mid + 1.5))
                });
                if let Some((x0, x1)) = hovered {
                    let tint = if dark { gpui::Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.22 } } else { gpui::Rgba { r: 0.12, g: 0.16, b: 0.22, a: 0.22 } };
                    quad(x0, x1, HIST_HEIGHT, tint, tint);
                }
                let peak = overall.peak();
                for (k, &count) in overall.counts.iter().enumerate() {
                    let (x0, x1) = x_of(k);
                    quad(x0, x1 - 0.5, height(count, peak), gpui::rgb(HIST_OVERALL), gpui::rgb(HIST_OVERALL_OUT));
                }
                if let Some(in_view) = &in_view {
                    let peak = in_view.peak();
                    for (k, &count) in in_view.counts.iter().enumerate() {
                        let (x0, x1) = x_of(k);
                        let (a, b) = bin_span(k);
                        let color = viridis_at((a + b) / 2.0);
                        // Narrower than the grey bar behind it, so both read.
                        let inset = ((x1 - x0) * 0.2).min(0.8);
                        quad(
                            x0 + inset,
                            x1 - 0.5 - inset,
                            height(count, peak),
                            color,
                            gpui::Rgba { a: HIST_OUT_ALPHA, ..color },
                        );
                    }
                }
                if let Some((x0, x1)) = hovered {
                    let cap = gpui::Rgba { r: 1.0, g: 0.77, b: 0.0, a: 1.0 };
                    window.paint_quad(gpui::fill(gpui::Bounds::new(gpui::point(ox + px(x0), bottom - px(HIST_HEIGHT)), gpui::size(px(x1 - x0), px(3.0))), cap));
                }
            },
        )
        .size_full(),
    )
}

/// The line under the ticks: under the pointer, one bar's income span and
/// both its counts; otherwise which bars are which, their totals, and how
/// they are scaled.
pub fn hist_caption(hist: &Histograms) -> gpui::Div {
    use gpui::prelude::*;
    let in_view_total = hist.in_view.as_ref().map_or_else(|| "…".to_string(), |h| thousands(u64::from(h.total)));
    let swatch = |color: gpui::Rgba| gpui::div().w(px(8.0)).h(px(8.0)).rounded(px(1.0)).bg(color);
    let text = |t: String| gpui::div().child(t);
    let row = gpui::div()
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .gap(px(4.0))
        .text_xs()
        .text_color(crate::popup::ink(0x6b7280))
        .whitespace_nowrap();
    match hist.hover {
        Some(k) => {
            let (a, b) = bin_span(k);
            let in_view = hist.in_view.as_ref().map_or_else(|| "…".to_string(), |h| thousands(u64::from(h.counts[k])));
            row.child(text(format!(
                "{} – {}: {in_view} in view · {} all",
                dollars_k(a),
                dollars_k(b),
                thousands(u64::from(hist.overall.counts[k]))
            )))
        }
        None => row
            .child(swatch(viridis_at(126_250.0)))
            .child(text(format!("in view {in_view_total}")))
            .child(swatch(gpui::rgb(HIST_OVERALL)))
            .child(text(format!("all {} · each to its own peak", thousands(u64::from(hist.overall.total))))),
    }
}


/// Mapbox Light v11 (`demos/mapbox-light-style.json`, needs
/// `MAPBOX_ACCESS_TOKEN`), or OpenFreeMap Positron (`demos/positron-style.json`,
/// keyless) -- both pale greys, so the income ramp is the only saturated
/// colour on the map, as in Kyle Walker's mapgl reference. `pick` overrides
/// the token-presence default either way; `--basemap mapbox` with no token
/// set panics with a clear message rather than silently falling back. The
/// token is never printed.
pub fn basemap(pick: Option<Basemap>) -> (gpui_maps::style::Style, Arc<dyn TileSource>) {
    basemap_with(pick, crate::kit::env_mapbox_token())
}

/// [`basemap`] with the token handed in: the gallery's comes from
/// `kit::PanelEnv::mapbox_token` (the environment or `mapbox.env`).
pub fn basemap_with(pick: Option<Basemap>, token: Option<String>) -> (gpui_maps::style::Style, Arc<dyn TileSource>) {
    let mut choice = pick.unwrap_or(if token.is_some() { Basemap::Mapbox } else { Basemap::Positron });
    // Mapbox Light is not in the open library tree: draw Positron instead of
    // panicking (scripts/fetch-mapbox-styles.sh fetches the original).
    if choice == Basemap::Mapbox && !crate::kit::mapbox_style_bundled("demos/mapbox-light-style.json") {
        eprintln!("census: demos/mapbox-light-style.json is not in this checkout; drawing Positron");
        choice = Basemap::Positron;
    }

    if choice == Basemap::Mapbox {
        let token = token.expect("--basemap mapbox needs MAPBOX_ACCESS_TOKEN set (a Mapbox public 'pk.' token)");
        let bytes = crate::kit::asset_bytes("demos/mapbox-light-style.json").expect("vendored Mapbox Light style");
        let mut json: serde_json::Value = serde_json::from_slice(&bytes).expect("the vendored style is JSON");
        readable_place_labels(&mut json);
        let bytes = serde_json::to_vec(&json).expect("a Value serialises");
        let style = gpui_maps::style_spec::parse_and_compile(&bytes).expect("Mapbox Light must compile");
        let tilesets = json["sources"]["composite"]["url"]
            .as_str()
            .and_then(|u| u.strip_prefix("mapbox://"))
            .expect("a mapbox:// composite source")
            .to_string();
        let url = format!("https://api.mapbox.com/v4/{tilesets}/{{z}}/{{x}}/{{y}}.mvt?access_token={token}");
        println!("basemap: Mapbox Light");
        let source = HttpTileSource::new(
            url,
            16,
            concat!(
                r#"<a href="https://www.mapbox.com/about/maps" target="_blank">&copy; Mapbox</a> "#,
                r#"<a href="https://www.openstreetmap.org/copyright/" target="_blank">&copy; OpenStreetMap</a>"#,
            ),
        );
        return (style, Arc::new(source));
    }

    let bytes = crate::kit::asset_bytes("demos/positron-style.json").expect("vendored Positron style");
    let style = gpui_maps::style_spec::parse_and_compile(&bytes).expect("Positron must compile");
    println!("basemap: OpenFreeMap Positron");
    let source = HttpTileSource::new(
        "https://tiles.openfreemap.org/planet/20260802_080001_pt/{z}/{x}/{y}.pbf",
        14,
        concat!(
            r#"<a href="https://openfreemap.org" target="_blank">OpenFreeMap</a> "#,
            r#"<a href="https://www.openmaptiles.org/" target="_blank">&copy; OpenMapTiles</a> "#,
            r#"Data from <a href="https://www.openstreetmap.org/copyright" target="_blank">OpenStreetMap</a>"#,
        ),
    );
    (style, Arc::new(source))
}

/// The JSON [`basemap_with`] compiles for `pick` (Mapbox Light with its
/// place labels already darkened, or Positron), for `crate::data_labels`.
pub fn basemap_document(pick: Basemap) -> Option<serde_json::Value> {
    let rel = if pick == Basemap::Mapbox { "demos/mapbox-light-style.json" } else { "demos/positron-style.json" };
    let mut json: serde_json::Value = serde_json::from_slice(&crate::kit::asset_bytes(rel).ok()?).ok()?;
    if pick == Basemap::Mapbox {
        readable_place_labels(&mut json);
    }
    Some(json)
}

/// The imagery `s` switches to: the vendored imagery style and Mapbox
/// satellite (`@2x`, 512 px) with a token, else Sentinel-2 cloudless
/// (EOX, the 2016 layer, CC BY 4.0), the same pair `examples/basic_map.rs` defaults to.
/// The imagery style's labels name OpenMapTiles layers, so over the Mapbox
/// basemap's tiles only the imagery (and the choropleth) draws.
pub fn satellite() -> (gpui_maps::style::Style, Option<(Arc<dyn TileSource>, u32, &'static str)>) {
    satellite_with(crate::kit::env_mapbox_token())
}

/// [`satellite`] with the token handed in, as [`basemap_with`].
pub fn satellite_with(token: Option<String>) -> (gpui_maps::style::Style, Option<(Arc<dyn TileSource>, u32, &'static str)>) {
    let bytes = crate::kit::asset_bytes("examples/satellite-style.json").expect("vendored satellite style");
    let style = gpui_maps::style_spec::parse_and_compile(&bytes).expect("the satellite style compiles");
    let source: (Arc<dyn TileSource>, u32, &'static str) = match token {
        Some(token) => (
            Arc::new(HttpTileSource::new(
                format!("https://api.mapbox.com/v4/mapbox.satellite/{{z}}/{{x}}/{{y}}@2x.jpg?access_token={token}"),
                22,
                r#"<a href="https://www.mapbox.com/about/maps" target="_blank">&copy; Mapbox</a> <a href="https://www.maxar.com/" target="_blank">&copy; Maxar</a>"#,
            )),
            512,
            "mapbox-satellite",
        ),
        None => (
            Arc::new(HttpTileSource::new(
                crate::kit::S2CLOUDLESS_2016_URL,
                14,
                crate::kit::S2CLOUDLESS_2016_ATTRIBUTION,
            )),
            256,
            "s2cloudless",
        ),
    };
    (style, Some(source))
}

// ------------------------------------------------------------ the gallery

/// What a load reads: which states, from where, and how the window opens.
/// The standalone builds one from its `Args`; the gallery takes
/// [`Options::default`].
#[derive(Clone)]
pub struct Options {
    /// Zero-padded state FIPS codes, in load order.
    pub states: Vec<String>,
    /// The fetch script's output directory (`shp/`, `acs/`, `names/`, ...).
    pub data: PathBuf,
    /// Declare the sources with `fill-outline-color` (the hairline).
    pub outline: bool,
    /// `None` picks by the token, as [`basemap`] does.
    pub basemap: Option<Basemap>,
    /// `None` opens at `census::START`.
    pub camera: Option<(f64, f64, f64)>,
    /// The browser's simplified `BGPK` pack (`src/census/pack.rs`,
    /// `tools/census-pack`): read instead of the shapefiles when it is on
    /// disk and decodes, else the shapefiles under `data` are read. `None`
    /// reads the shapefiles always -- the standalone's choice, the
    /// unsimplified 242k-polygon benchmark.
    pub pack: Option<PathBuf>,
}

impl Default for Options {
    /// Every state, from [`default_pack`] when it is there, else from
    /// [`default_data`], outlined.
    fn default() -> Self {
        Self {
            states: ALL_STATES.iter().map(|s| s.to_string()).collect(),
            data: default_data(),
            outline: true,
            basemap: None,
            camera: None,
            pack: Some(default_pack()),
        }
    }
}

/// `US_BG_PACK`, else `~/.cache/gpui-map-data/census-web/us_bg_2023.bgpk`:
/// where `tools/census-pack` writes the browser's pack.
pub fn default_pack() -> PathBuf {
    std::env::var_os("US_BG_PACK").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".cache/gpui-map-data/census-web/us_bg_2023.bgpk")
    })
}

/// `US_BG_CACHE`, else `~/.cache/gpui-map/demos/us-block-groups`: where
/// `demos/data/fetch_us_block_groups.sh` writes.
pub fn default_data() -> PathBuf {
    std::env::var_os("US_BG_CACHE").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".cache/gpui-map/demos/us-block-groups")
    })
}

/// THE load: every state in `opts.states` read from `opts.data`, then the
/// income join, stopping between states (and before the join) once
/// `cancel` is raised, which returns `None`. `progress(done, total)`
/// follows each state. The standalone's `load` is this with a flag nobody
/// raises; the gallery's [`CensusPanel`] raises it on a switch away.
pub fn load_cancellable(opts: &Options, cancel: &AtomicBool, progress: &dyn Fn(usize, usize)) -> Option<Payload> {
    load_cancellable_noting(opts, cancel, progress, &|_| {})
}

/// [`load_cancellable`], saying which source it reads through `note`: the
/// pack when `opts.pack` is on disk and decodes, else the shapefiles --
/// with why, when a pack was asked for. A pack that fails part-way (a
/// truncated or foreign file) falls back to the shapefiles; a cancel
/// during the pack stops the load as it does between shapefiles.
pub fn load_cancellable_noting(
    opts: &Options,
    cancel: &AtomicBool,
    progress: &dyn Fn(usize, usize),
    note: &dyn Fn(String),
) -> Option<Payload> {
    let started = Instant::now();
    match opts.pack.as_deref() {
        Some(path) if path.exists() => {
            note(PACK_NOTE.to_string());
            match load_pack_cancellable(path, &opts.states, cancel, progress) {
                Ok(Some(d)) => {
                    print_read(&format!("read {} states from {}", d.states.len(), path.display()), &d, started.elapsed());
                    return Some(Payload::Shapes(d));
                }
                Ok(None) => return None,
                Err(err) => {
                    println!("census pack {}: {err} -- reading the shapefiles instead", path.display());
                    note(format!("The pack did not load ({err}): reading the unsimplified TIGER shapefiles instead"));
                }
            }
        }
        Some(path) => {
            println!("census pack {}: not found -- reading the shapefiles (see tools/census-pack)", path.display());
            note(format!("No pack at {}: reading the unsimplified TIGER shapefiles", path.display()));
        }
        None => {}
    }
    let started = Instant::now();
    let d = load_shapefiles_cancellable(&opts.data, &opts.states, cancel, progress)?;
    print_read(&format!("read {} states", opts.states.len()), &d, started.elapsed());
    Some(Payload::Shapes(d))
}

/// The loading card's source line while the pack is read.
pub const PACK_NOTE: &str = "From the simplified pack (TIGER/Line 2023, simplified)";

/// The load's one summary line: `{what}: N features, ... in T ms`.
fn print_read(what: &str, d: &BlockGroups, elapsed: Duration) {
    let max = d
        .features
        .iter()
        .map(|f| d.rings[f.rings.0..f.rings.1].iter().map(|r| r.1 - r.0).sum::<usize>())
        .max()
        .unwrap_or(0);
    println!(
        "{what}: {} features, {} rings, {} input coordinates (max {max} per feature) in {:.1} ms",
        d.features.len(),
        d.rings.len(),
        d.coords.len(),
        elapsed.as_secs_f64() * 1e3
    );
}

/// Every state in `states` from the `BGPK` pack at `path`
/// (`gpui_maps::census::pack`'s bounded decoders, the browser's), into the
/// same flat [`BlockGroups`] the shapefile path builds -- so the push, the
/// hover table, the histograms, the in-view index and the breakdowns take
/// it unchanged. Each row's income, population and land area come from the
/// pack's own tables, and the county names and CBSA table ride along in
/// [`BlockGroups::pack`].
///
/// No pack file may be larger than this before it is read into memory: the
/// shipped US block-group pack is ~41 MB, and the file is user-built or
/// points wherever `US_BG_PACK` says, so it is untrusted the way a
/// downloaded file is.
const MAX_PACK_BYTES: u64 = 256 * 1024 * 1024;

/// `Ok(None)` once `cancel` is raised: it is read before the tables and
/// before each state. `progress(done, total)` follows each state. `Err`
/// for a file that cannot be read or does not decode, or is over
/// [`MAX_PACK_BYTES`].
pub fn load_pack_cancellable(
    path: &std::path::Path,
    states: &[String],
    cancel: &AtomicBool,
    progress: &dyn Fn(usize, usize),
) -> Result<Option<BlockGroups>, String> {
    let len = std::fs::metadata(path).map_err(|err| format!("{}: {err}", path.display()))?.len();
    if len > MAX_PACK_BYTES {
        return Err(format!("{} is {len} bytes, over the {MAX_PACK_BYTES}-byte limit", path.display()));
    }
    let bytes = std::fs::read(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let header = pack::parse_header(&bytes).map_err(|err| err.to_string())?;
    let section = |offset: u64, len: u32, what: &str| -> Result<&[u8], String> {
        usize::try_from(offset)
            .ok()
            .and_then(|start| Some(start..start.checked_add(len as usize)?))
            .and_then(|range| bytes.get(range))
            .ok_or_else(|| format!("{what} runs past the end of the file"))
    };
    if cancel.load(Ordering::SeqCst) {
        println!("census load: cancelled before the pack's tables");
        return Ok(None);
    }
    let tables = pack::decode_tables(&header, section(header.tables_offset, header.tables_len, "the tables")?)
        .map_err(|err| err.to_string())?;
    let wanted: Vec<&pack::DirRow> =
        header.states.iter().filter(|row| states.iter().any(|s| s.parse::<u8>() == Ok(row.fips))).collect();
    let total = wanted.len();
    let mut out = BlockGroups::default();
    for (done, row) in wanted.into_iter().enumerate() {
        if cancel.load(Ordering::SeqCst) {
            println!("census load: cancelled after {done} of {total} states (pack)");
            return Ok(None);
        }
        let rows = (row.first_feature as usize)
            .checked_add(row.feature_count as usize)
            .and_then(|end| tables.rows.get(row.first_feature as usize..end))
            .ok_or_else(|| format!("state {:02}: its rows are past the table", row.fips))?;
        let geo = pack::decode_state(row, section(row.offset, row.len, "a state section")?)
            .map_err(|err| format!("state {:02}: {err}", row.fips))?;
        if geo.features.len() != rows.len() {
            return Err(format!("state {:02}: {} features, {} rows", row.fips, geo.features.len(), rows.len()));
        }
        let first = out.features.len();
        let (c0, r0, p0) = (out.coords.len(), out.rings.len(), out.polygons.len());
        out.coords.extend_from_slice(&geo.coords);
        out.rings.extend(geo.rings.iter().map(|&(a, b)| (c0 + a as usize, c0 + b as usize)));
        out.polygons.extend(geo.polygons.iter().map(|&(a, b)| (r0 + a as usize, r0 + b as usize)));
        for (&(a, b), r) in geo.features.iter().zip(rows) {
            let polygons = (p0 + a as usize, p0 + b as usize);
            // A feature's polygons are consecutive, and so are their rings.
            let rings = match out.polygons.get(polygons.0..polygons.1) {
                Some([one, .., last]) => (one.0, last.1),
                Some([one]) => *one,
                _ => (out.rings.len(), out.rings.len()),
            };
            out.features.push(Bg {
                geoid: r.geoid as f64,
                income: r.income.map(f64::from),
                aland_m2: f64::from(r.aland_km2) * 1e6,
                population: r.population.map(f64::from),
                polygons,
                rings,
            });
        }
        out.states.push((format!("{:02}", row.fips), first, out.features.len()));
        progress(done + 1, total);
    }
    let income = out.features.iter().filter(|bg| bg.income.is_some()).count();
    println!(
        "income (pack): {income} of {} block groups have an income; simplified at {} m",
        out.features.len(),
        header.tolerance_m
    );
    out.pack = Some(PackTables {
        path: path.to_path_buf(),
        tolerance_m: header.tolerance_m,
        county_text: tables.county_text,
        cbsa_text: tables.cbsa_text,
    });
    Ok(Some(out))
}

/// GEOID -> (income, land area m^2): the hover card's table.
pub fn lookup_of(data: &BlockGroups) -> HashMap<u64, (Option<f64>, f64)> {
    data.features.iter().map(|bg| (bg.geoid as u64, (bg.income, bg.aland_m2))).collect()
}

/// `(GEOID, income)` per block group, for `DemoRoot::start_breakdowns`.
pub fn breakdown_rows_of(data: &BlockGroups) -> Vec<(u64, Option<f64>)> {
    data.features.iter().map(|bg| (bg.geoid as u64, bg.income)).collect()
}

/// Everything the window needs from a load, computed off the UI thread:
/// the block groups (pushed, then dropped), the histogram boxes, the
/// county names, the hover table and the breakdown rows.
pub struct Prepared {
    pub data: BlockGroups,
    pub view_items: Arc<[ViewItem]>,
    pub counties: Arc<HashMap<u32, String>>,
    pub lookup: Arc<HashMap<u64, (Option<f64>, f64)>>,
    pub breakdown_rows: Vec<(u64, Option<f64>)>,
    /// Where the breakdowns' population and metro tables come from.
    pub breakdown_tables: BreakdownTables,
    /// The pack's simplification tolerance (metres) when the geometry came
    /// from it; `None` for the unsimplified shapefiles.
    pub simplified_m: Option<f32>,
}

/// The census credit the map shows (`MapView::set_data_credit`), naming
/// the simplification when the geometry came from the pack.
pub fn map_credit(simplified_m: Option<f32>) -> String {
    match simplified_m {
        Some(m) => format!("US Census Bureau: TIGER/Line 2023 (simplified in the pack, {m} m), ACS 2023 5-year"),
        None => "US Census Bureau: TIGER/Line 2023, ACS 2023 5-year".to_string(),
    }
}

/// The breakdowns' tables: files under the data directory (the shapefile
/// path), or the pack's own (each row's population, the CBSA table).
pub enum BreakdownTables {
    Dir(PathBuf),
    Pack { rows: Vec<breakdowns::Row>, cbsa_text: String },
}

/// [`Prepared`] from the loaded block groups: the same pieces `windowed`
/// builds before and inside its window, in one place off the UI thread.
///
/// Pack-loaded block groups (`data.pack`) take the county names and the
/// breakdowns' tables from the pack; the shapefile path reads them under
/// `dir`.
pub fn prepare(mut data: BlockGroups, dir: &std::path::Path) -> Prepared {
    let started = Instant::now();
    let view_items: Arc<[ViewItem]> = view_items(&data).into();
    println!("histogram boxes: {} block groups in {:.1} ms", view_items.len(), started.elapsed().as_secs_f64() * 1e3);
    let simplified_m = data.pack.as_ref().map(|t| t.tolerance_m);
    let (counties, breakdown_tables) = match data.pack.take() {
        Some(tables) => {
            let rows = data
                .features
                .iter()
                .map(|bg| breakdowns::Row { geoid: bg.geoid as u64, income: bg.income, population: bg.population })
                .collect();
            (parse_county_names(&tables.county_text), BreakdownTables::Pack { rows, cbsa_text: tables.cbsa_text })
        }
        None => (load_county_names(dir), BreakdownTables::Dir(dir.to_path_buf())),
    };
    Prepared {
        view_items,
        counties: Arc::new(counties),
        lookup: Arc::new(lookup_of(&data)),
        breakdown_rows: breakdown_rows_of(&data),
        breakdown_tables,
        simplified_m,
        data,
    }
}

/// Drops `value` on a thread of its own: a loaded census is ~4 GB, and
/// freeing it on the UI thread would stall the next panel's first frames.
pub fn drop_off_ui_thread<T: Send + 'static>(value: T) {
    std::thread::Builder::new()
        .name("census-drop".into())
        .spawn(move || drop(value))
        .expect("a thread to drop the census on");
}

pub const ID: &str = "census";

/// The Census's two datasets, public domain (17 U.S.C. 105).
pub const CREDITS: &[kit::Credit] = &[
    kit::Credit {
        changes: "simplified (shared arcs) when the gallery reads tools/census-pack's pack, and the map's credit then says so; unsimplified from the shapefiles otherwise; joined to income by GEOID",
        ..kit::CREDIT_CENSUS_TIGER
    },
    kit::CREDIT_CENSUS_ACS,
    kit::Credit { what: "Basemap tiles, when drawn (Mapbox Light, with a token)", ..kit::CREDIT_MAPBOX },
    kit::Credit { what: "Basemap tiles, when drawn (Positron, without a token)", ..kit::CREDIT_OSM_OPENFREEMAP },
    kit::Credit { what: "Satellite imagery, when drawn (S, without a token)", ..kit::CREDIT_S2CLOUDLESS },
];

/// Network for the basemap, a Mapbox token for Mapbox Light (Positron
/// without one), and the fetched data at [`default_data`].
pub fn requirements(_env: &kit::PanelEnv) -> Vec<kit::Requirement> {
    studio_requirements(&default_data(), &default_pack())
}

/// [`requirements_for`], unless the pack at `pack` is on disk: then the
/// pack is the one file the panel needs (it carries the geometry, the
/// incomes, the populations, the county names and the metro table).
pub fn requirements_with(data: &std::path::Path, pack: Option<&std::path::Path>) -> Vec<kit::Requirement> {
    let Some(pack) = pack.filter(|p| p.exists()) else { return requirements_for(data) };
    pack_requirements(pack)
}

/// The gallery's needs: the pack, which the missing-data card DOWNLOADS
/// (`kit::datapack`) -- unless the shapefiles are already fetched and the
/// pack is not, when the panel reads those instead. Never a script for
/// the user to run.
pub fn studio_requirements(data: &std::path::Path, pack: &std::path::Path) -> Vec<kit::Requirement> {
    if !pack.exists() && data.join("acs/income_bg_2023.csv").exists() {
        return requirements_for(data);
    }
    pack_requirements(pack)
}

fn pack_requirements(pack: &std::path::Path) -> Vec<kit::Requirement> {
    vec![
        kit::Requirement::Network,
        kit::Requirement::MapboxToken { fallback: Some("OpenFreeMap Positron") },
        kit::Requirement::File {
            path: pack.to_path_buf(),
            what: "US block groups, simplified pack (TIGER/Line 2023 + ACS 2023)",
            fetch: PACK_BUILD,
            size: "~41 MB",
            fallback: None,
            then: None,
        },
    ]
}

/// [`requirements`] with the data at `data`: the file checked is the
/// income table the fetch script writes last, and without it there is no
/// fallback -- the card shows the fetch script.
pub fn requirements_for(data: &std::path::Path) -> Vec<kit::Requirement> {
    vec![
        kit::Requirement::Network,
        kit::Requirement::MapboxToken { fallback: Some("OpenFreeMap Positron") },
        kit::Requirement::File {
            path: data.join("acs/income_bg_2023.csv"),
            what: "US block groups (TIGER/Line 2023 + ACS 2023)",
            fetch: "demos/data/fetch_us_block_groups.sh",
            size: "~1.1 GB download; ~4.7 GB RAM while open",
            fallback: None,
            // The pack the panel reads first (021f127): built from these
            // shapefiles, it opens in under a second instead of ~50 s.
            then: Some(PACK_BUILD),
        },
    ]
}

/// The command that builds the pack [`default_pack`] names, from the
/// shapefiles -- the one `web/run.sh` prints.
pub const PACK_BUILD: &str =
    "cargo run --release --manifest-path tools/census-pack/Cargo.toml -- --out ~/.cache/gpui-map-data/census-web/us_bg_2023.bgpk";

/// What the gallery panel shows.
pub enum PanelState {
    /// The basemap, and a line saying how far the load has got.
    Loading { done: usize, total: usize },
    /// The standalone's own root, built by the same [`DemoRoot::new`].
    Ready(Entity<DemoRoot>),
    /// The basemap, and why there is no census.
    Failed(String),
}

/// The gallery's census panel: the basemap at once, the block groups when
/// the background load lands. The map and its shell are built here and
/// handed to the [`DemoRoot`] on [`PanelState::Ready`], so the map the
/// gallery holds (`kit::Built::map`) is the one the census draws on.
pub struct CensusPanel {
    pub map: Entity<MapView>,
    pub shell: Entity<shell::DemoShell>,
    pub state: PanelState,
    /// Which source the load reads, and why when it is not the pack
    /// (`load_cancellable_noting`'s note), for the loading card.
    pub note: Option<String>,
    /// The basemap labels over the choropleth (`crate::data_labels`).
    _labels: crate::data_labels::DataLabels,
}

impl CensusPanel {
    /// The loading line: `Reading block groups: 3/56 states`.
    pub fn loading_text(done: usize, total: usize) -> String {
        format!("Reading block groups: {done}/{total} states")
    }
}

/// Whether `view` is a census panel still reading its block groups:
/// `--cycle` waits for this to clear (bounded) before a census panel's
/// dwell, so the close it judges is a loaded census, not a cancelled load.
pub fn is_loading(view: &gpui::AnyView, cx: &App) -> bool {
    view.clone()
        .downcast::<CensusPanel>()
        .is_ok_and(|panel| matches!(panel.read(cx).state, PanelState::Loading { .. }))
}

impl Render for CensusPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        use gpui::prelude::*;
        let root = gpui::div().relative().size_full();
        match &self.state {
            PanelState::Ready(census) => root.child(census.clone()),
            PanelState::Loading { done, total } => {
                let lines = self.note.iter().cloned().chain([Self::loading_text(*done, *total)]).collect();
                root.child(self.shell.clone()).child(status_card(lines, Some(loading_fraction(*done, *total))))
            }
            PanelState::Failed(why) => root.child(self.shell.clone()).child(status_card(vec![why.clone()], None)),
        }
    }
}

/// How far the load is, `0..=1`, for the loading card's bar.
pub fn loading_fraction(done: usize, total: usize) -> f32 {
    if total == 0 {
        0.0
    } else {
        (done as f32 / total as f32).clamp(0.0, 1.0)
    }
}

/// Width of the loading card's progress bar.
const PROGRESS_WIDTH: f32 = 280.0;

/// A card centred over the map: the load's source and progress (a bar,
/// `progress` of the way along), or why it failed. The pill is the
/// panel's one card, drawn OVER the map; the bar's track and fill sit
/// inside it.
fn status_card(lines: Vec<String>, progress: Option<f32>) -> gpui::Div {
    use gpui::prelude::*;
    gpui::div().absolute().inset_0().flex().items_center().justify_center().child(
        gpui::div()
            .px_4()
            .py_2()
            .rounded_md()
            .bg(gpui::rgba(0xffffffff))
            .text_color(gpui::rgb(0x1f2933))
            .text_sm()
            .flex()
            .flex_col()
            .gap_1()
            .children(lines.into_iter().map(|line| gpui::div().child(line)))
            .children(progress.map(|frac| {
                gpui::div()
                    .w(px(PROGRESS_WIDTH))
                    .h(px(4.0))
                    .rounded_full()
                    .bg(gpui::rgba(0x1f29332e))
                    .child(gpui::div().h_full().w(px(PROGRESS_WIDTH * frac)).rounded_full().bg(gpui::rgb(0x3b528b)))
            })),
    )
}

/// The gallery entry: every state, from [`default_data`].
pub fn build(env: &kit::PanelEnv, _window: &mut Window, cx: &mut App) -> anyhow::Result<kit::Built> {
    let opts = Options::default();
    let (outline, for_panel) = (opts.outline, opts.clone());
    let total = opts.states.len();
    let note: Arc<std::sync::Mutex<Option<String>>> = Default::default();
    let noted = Arc::clone(&note);
    let load = move |cancel: &AtomicBool, progress: &dyn Fn(usize, usize)| -> Result<Option<Prepared>, String> {
        let say = |line: String| {
            if let Ok(mut n) = noted.lock() {
                *n = Some(line);
            }
        };
        let loaded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| load_cancellable_noting(&opts, cancel, progress, &say)))
            .map_err(|panic| {
                panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "the census load panicked".to_string())
            })?;
        let Some(Payload::Shapes(data)) = loaded else { return Ok(None) };
        if data.features.is_empty() {
            return Err(format!(
                "no block groups under {} -- run demos/data/fetch_us_block_groups.sh",
                opts.data.join("shp").display()
            ));
        }
        let prepared = prepare(data, &opts.data);
        // Switched away while preparing: dropped here, off the UI thread.
        if cancel.load(Ordering::SeqCst) {
            println!("census load: cancelled after the read, before the window");
            return Ok(None);
        }
        Ok(Some(prepared))
    };
    let mount = move |panel: &Entity<CensusPanel>, prepared: Prepared, cx: &mut App| mount(panel, prepared, outline, cx);
    build_with(env, for_panel, total, note, load, mount, cx)
}

/// The panel with its basemap, and a load run on the background executor:
/// progress arrives over a channel, polled every 100 ms, and the result is
/// mounted with `mount` -- unless the gallery switched away meanwhile
/// (`env.cancel`, or the panel is gone), when it is dropped on a thread of
/// its own ([`drop_off_ui_thread`]) and no census appears. `load` and
/// `mount` are parameters so a test can stand in a slow fake.
pub fn build_with<T: Send + 'static>(
    env: &kit::PanelEnv,
    opts: Options,
    total: usize,
    note: Arc<std::sync::Mutex<Option<String>>>,
    load: impl FnOnce(&AtomicBool, &dyn Fn(usize, usize)) -> Result<Option<T>, String> + Send + 'static,
    mount: impl FnOnce(&Entity<CensusPanel>, T, &mut App) + 'static,
    cx: &mut App,
) -> anyhow::Result<kit::Built> {
    let token = env.mapbox_token.clone();
    let pick = opts.basemap.unwrap_or(if token.is_some() { Basemap::Mapbox } else { Basemap::Positron });
    let (style, source) = basemap_with(Some(pick), token.clone());
    let (imagery_style, satellite) = satellite_with(token);
    let start = opts.camera.unwrap_or(gpui_maps::census::START);
    let (w, h) = env.viewport;
    let runtime = env.runtime.clone();
    let map = cx.new(|_| {
        let mut view = MapView::new(start.0, start.1, start.2, w, h, style, source, runtime);
        view.set_chrome_above_map(true);
        view.set_skip_unchanged_frames(std::env::var_os("US_BG_REDRAW_ALWAYS").is_none());
        view
    });
    // The block groups fill the country: labels over the data, before the
    // first paint and before the satellite toggle keeps the vector style.
    let labels = crate::data_labels::DataLabels::attach(&map, basemap_document(pick), true, cx);
    let vector_style = map.read(cx).style().clone();
    let sat = satellite.map(|(source, tile_size, name)| {
        shell::Satellite::new(source, tile_size, name, false).with_styles(imagery_style, vector_style)
    });
    let shell = cx.new(|cx| match env.host.title() {
        Some(title) => shell::DemoShell::with_satellite(map.clone(), title.to_string(), sat, cx),
        None => shell::DemoShell::embedded(map.clone(), sat, cx),
    });
    let panel = cx.new(|_| CensusPanel { map: map.clone(), shell, state: PanelState::Loading { done: 0, total }, note: None, _labels: labels });

    let (tx, rx) = std::sync::mpsc::channel::<(usize, usize)>();
    let cancel = env.cancel.clone();
    let loading = {
        let cancel = cancel.clone();
        cx.background_executor().spawn(async move {
            load(&cancel, &|done, total| {
                let _ = tx.send((done, total));
            })
        })
    };
    // Progress and the source note: until the panel is gone or has left
    // `Loading`.
    let weak = panel.downgrade();
    cx.spawn(async move |cx| loop {
        cx.background_executor().timer(Duration::from_millis(100)).await;
        let latest = rx.try_iter().last();
        let line = note.lock().ok().and_then(|n| n.clone());
        let updated = weak.update(cx, |p, cx| {
            let PanelState::Loading { done, total } = &mut p.state else { return false };
            let mut changed = false;
            if let Some(latest) = latest.filter(|&l| l != (*done, *total)) {
                (*done, *total) = latest;
                changed = true;
            }
            if p.note != line {
                p.note = line;
                changed = true;
            }
            if changed {
                cx.notify();
            }
            true
        });
        if !matches!(updated, Ok(true)) {
            return;
        }
    })
    .detach();
    // Completion.
    let weak = panel.downgrade();
    cx.spawn(async move |cx| {
        let result = loading.await;
        let panel = weak.upgrade();
        match result {
            Ok(None) => {}
            Ok(Some(loaded)) => match panel {
                Some(panel) if !cancel.load(Ordering::SeqCst) => {
                    cx.update(|cx| mount(&panel, loaded, cx));
                }
                _ => {
                    println!("census load: finished after the panel closed; dropping it off the UI thread");
                    drop_off_ui_thread(loaded);
                }
            },
            Err(why) => {
                if let Some(panel) = panel {
                    println!("census load failed: {why}");
                    panel.update(cx, |p, cx| {
                        p.state = PanelState::Failed(format!("No census: {why}"));
                        cx.notify();
                    });
                }
            }
        }
    })
    .detach();
    Ok(kit::Built { view: panel.into(), map: Some(map.downgrade()), start })
}

/// Pushes the loaded block groups into the panel's map and puts the
/// standalone's root over it, the same calls `windowed` makes: `push`,
/// `DemoRoot::new` with the shell, the breakdowns and the in-view count.
fn mount(panel: &Entity<CensusPanel>, prepared: Prepared, outline: bool, cx: &mut App) {
    let Prepared { data, view_items, counties, lookup, breakdown_rows, breakdown_tables, simplified_m } = prepared;
    let (map, shell) = panel.read_with(cx, |p, _| (p.map.clone(), p.shell.clone()));
    map.update(cx, |view, cx| {
        let (fill, line, _) = push(view, &data, outline, false, cx);
        println!(
            "push (sync): fill {:.1} ms{}",
            fill.as_secs_f64() * 1e3,
            line.map(|d| format!(", outline {:.1} ms", d.as_secs_f64() * 1e3)).unwrap_or_default()
        );
        view.set_data_credit(Some(map_credit(simplified_m)));
    });
    let sources: Arc<[String]> = data.states.iter().map(|(fips, _, _)| state_source(fips)).collect();
    drop_off_ui_thread(data);
    let root = cx.new(|cx| {
        let mut root = DemoRoot::new(map, lookup, counties, sources, outline, cx);
        root.shell = Some(shell);
        root.focus_policy = shell::FocusPolicy::Once;
        let boxes = Some(Arc::clone(&view_items));
        match breakdown_tables {
            BreakdownTables::Dir(dir) => root.start_breakdowns(dir, breakdown_rows, boxes, cx),
            BreakdownTables::Pack { rows, cbsa_text } => root.start_breakdowns_from_pack(rows, cbsa_text, boxes, cx),
        }
        root.start_in_view(view_items, cx);
        root
    });
    panel.update(cx, |p, cx| {
        p.state = PanelState::Ready(root);
        cx.notify();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The window -> `MapView` chain must set no opaque background: the
    /// map draws in a CAMetalLayer behind gpui, so one would hide it (the
    /// bug `race_dots` shipped). `DemoRoot::render` builds the root div
    /// that holds the map and sets no background anywhere; the backgrounds
    /// in this file are the cards drawn OVER the map as the root's later
    /// children (the legend, the tooltip), built by their own functions.
    /// The needle is built at runtime so this assertion does not match
    /// itself via `include_str!`.
    #[test]
    fn the_panel_root_sets_no_opaque_background() {
        let needle = [".", "bg", "("].concat();
        let source = include_str!("census.rs");
        let start = source.find("impl Render for DemoRoot").expect("DemoRoot renders in census.rs");
        let render = &source[start..start + source[start..].find("\n}\n").expect("the impl ends")];
        assert!(render.contains("gpui::div()\n            .relative()\n            .size_full()"), "the root div is where this test looks");
        assert!(!render.contains(&needle), "DemoRoot's render, and so its root div, must not set a background");
    }

    /// Review F5: a mounted census takes the keyboard only if nothing else
    /// holds it; standalone it still takes it every frame.
    #[test]
    fn a_mounted_census_leaves_the_sidebar_its_focus() {
        use shell::FocusPolicy::{EveryFrame, Once};
        assert!(!takes_focus(Once, true, false, true), "the sidebar has focus: keep it there");
        assert!(takes_focus(Once, true, false, false), "nothing focused: the map takes it once");
        assert!(!takes_focus(Once, true, true, false), "once");
        assert!(!takes_focus(Once, false, false, false), "no handle yet");
        assert!(takes_focus(EveryFrame, true, true, true), "standalone: every frame, as before");
    }

    /// Gallery Task 12: a raised cancel stops the load between states,
    /// and nothing after it is read.
    #[test]
    fn load_stops_between_states_when_cancelled() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let dir = tempfile::tempdir().unwrap(); // no shapefiles: each state is "not found" and skipped
        let cancel = AtomicBool::new(false);
        let seen = AtomicUsize::new(0);
        let opts = Options { states: vec!["44".into(), "11".into(), "06".into()], data: dir.path().into(), pack: None, ..Options::default() };
        let r = load_cancellable(&opts, &cancel, &|done, _| {
            seen.store(done, Ordering::SeqCst);
            if done == 1 {
                cancel.store(true, Ordering::SeqCst);
            }
        });
        assert!(r.is_none(), "a cancelled load returns nothing");
        assert_eq!(seen.load(Ordering::SeqCst), 1, "no state after the cancel is read");
    }

    /// Never cancelled, every state is visited (missing ones skipped) and
    /// the load returns, empty: the gallery turns that into its "run the
    /// fetch script" failure rather than an empty map.
    #[test]
    fn an_uncancelled_load_visits_every_state() {
        let dir = tempfile::tempdir().unwrap();
        let seen = std::sync::Mutex::new(Vec::new());
        let opts = Options { states: vec!["44".into(), "11".into()], data: dir.path().into(), pack: None, ..Options::default() };
        let r = load_cancellable(&opts, &AtomicBool::new(false), &|done, total| seen.lock().unwrap().push((done, total)));
        let Some(Payload::Shapes(data)) = r else { panic!("an uncancelled load returns its block groups") };
        assert!(data.features.is_empty() && data.states.is_empty());
        assert_eq!(*seen.lock().unwrap(), vec![(1, 2), (2, 2)]);
    }

    /// A two-state pack (DC one square; RI a square and a square with a
    /// hole), written by `pack::encode` -- the browser's encoder -- to
    /// `dir/test.bgpk`, with a gazetteer-shaped county table and one metro.
    fn write_test_pack(dir: &std::path::Path) -> PathBuf {
        use gpui_maps::census::pack::{encode, EncodeState, PackRow};
        let square = |lon: f64, lat: f64, d: f64| vec![[lon, lat], [lon + d, lat], [lon + d, lat + d], [lon, lat + d], [lon, lat]];
        let ri = EncodeState {
            fips: 44,
            features: vec![
                vec![vec![square(-71.5, 41.8, 0.01)]],
                vec![vec![square(-71.49, 41.8, 0.01), square(-71.487, 41.803, 0.002)]],
            ],
        };
        let dc = EncodeState { fips: 11, features: vec![vec![vec![square(-77.03, 38.9, 0.005)]]] };
        let rows = vec![
            PackRow { geoid: 110010001011, income: Some(250_001), population: Some(1_200), aland_km2: 0.2 },
            PackRow { geoid: 440010301001, income: None, population: Some(0), aland_km2: 1.5 },
            PackRow { geoid: 440010301002, income: Some(64_000), population: Some(900), aland_km2: 0.9 },
        ];
        let counties = "USPS\tGEOID\tANSICODE\tNAME\nRI\t44001\t01219777\tBristol County\nDC\t11001\t01702382\tDistrict of Columbia\n";
        let cbsa = "44001\t39300\tProvidence-Warwick, RI-MA\n";
        let path = dir.join("test.bgpk");
        std::fs::write(&path, encode(&[dc, ri], &rows, counties, cbsa, 5.0)).unwrap();
        path
    }

    /// Options reading only `pack`, with no shapefiles under `data`.
    fn pack_options(data: &std::path::Path, pack: PathBuf, states: &[&str]) -> Options {
        Options { states: states.iter().map(|s| s.to_string()).collect(), data: data.into(), pack: Some(pack), ..Options::default() }
    }

    /// Pack parity: a pack read through `load_cancellable` is the same
    /// `Payload::Shapes` the shapefile path returns, and every code path
    /// the panel runs on it -- the push's per-state sources, the hover
    /// table and county names, the histogram boxes and the in-view index,
    /// the breakdowns and their fly-to bounds, the map credit -- takes it.
    #[test]
    fn a_pack_loads_through_load_cancellable_and_feeds_every_panel_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_pack(dir.path());
        let seen = std::sync::Mutex::new(Vec::new());
        let notes = std::sync::Mutex::new(Vec::new());
        let opts = pack_options(dir.path(), path, &["11", "44"]);
        let r = load_cancellable_noting(&opts, &AtomicBool::new(false), &|d, t| seen.lock().unwrap().push((d, t)), &|n| {
            notes.lock().unwrap().push(n)
        });
        let Some(Payload::Shapes(data)) = r else { panic!("the pack loads as block groups") };
        assert_eq!(*seen.lock().unwrap(), vec![(1, 2), (2, 2)], "progress per state");
        assert_eq!(*notes.lock().unwrap(), vec![PACK_NOTE.to_string()]);
        assert!(PACK_NOTE.contains("simplified"));

        // The flat arrays, as the shapefile path lays them out.
        assert_eq!(data.states, vec![("11".to_string(), 0, 1), ("44".to_string(), 1, 3)]);
        assert_eq!(data.features.len(), 3);
        let holed = &data.features[2];
        assert_eq!((holed.polygons.1 - holed.polygons.0, holed.rings.1 - holed.rings.0), (1, 2), "an outer ring and its hole");
        assert_eq!(data.polygons[holed.polygons.0], holed.rings, "the polygon's rings are the feature's");
        for bg in &data.features {
            for &(a, b) in &data.rings[bg.rings.0..bg.rings.1] {
                assert!(b > a && b <= data.coords.len());
                assert_eq!(data.coords[a], data.coords[b - 1], "closed ring");
            }
        }
        let first = data.coords[data.rings[holed.rings.0].0];
        assert!((first[0] - -71.49).abs() < 1e-5 && (first[1] - 41.8).abs() < 1e-5, "{first:?}");
        assert_eq!(data.features.iter().map(|bg| bg.income).collect::<Vec<_>>(), vec![Some(250_001.0), None, Some(64_000.0)]);
        assert_eq!(data.features[0].geoid, 110010001011.0);
        assert!((data.features[1].aland_m2 - 1.5e6).abs() < 1.0);
        let tables = data.pack.as_ref().expect("the pack's tables ride along");
        assert_eq!(tables.tolerance_m, 5.0);

        // The push's sources, one per state.
        let sources: Vec<String> = data.states.iter().map(|(fips, _, _)| state_source(fips)).collect();
        assert_eq!(sources, vec!["block-groups-11", "block-groups-44"]);
        assert_eq!(income_column(&data), vec![Some(250_001.0), None, Some(64_000.0)]);

        // Everything `mount` hands `DemoRoot`.
        let prepared = prepare(data, dir.path());
        assert_eq!(prepared.simplified_m, Some(5.0));
        assert!(map_credit(prepared.simplified_m).contains("simplified"));
        assert!(!map_credit(None).contains("simplified"));
        assert_eq!(prepared.counties.get(&44001).map(String::as_str), Some("Bristol County"), "names from the pack");
        let lines = tooltip_lines(440010301002, prepared.lookup.get(&440010301002).copied(), &prepared.counties);
        assert!(lines[0].contains("Bristol County"), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("64,000")), "{lines:?}");
        assert_eq!(prepared.view_items.len(), 3);
        assert_eq!(usize::from(prepared.view_items[0].bin), income_bin(250_001.0), "the histogram bar of the income");
        let BreakdownTables::Pack { rows, cbsa_text } = &prepared.breakdown_tables else {
            panic!("the breakdowns read the pack's tables, not the data directory")
        };
        assert_eq!(rows.iter().map(|r| r.population).collect::<Vec<_>>(), vec![Some(1_200.0), Some(0.0), Some(900.0)]);
        let mut b = breakdowns::Breakdowns::from_texts(rows, None, cbsa_text, breakdowns::BinEdges::default()).expect("breakdowns");
        assert_eq!(b.states.iter().map(|g| g.code).collect::<Vec<_>>().len(), 2);
        assert_eq!(b.metros.iter().map(|g| g.code).collect::<Vec<_>>(), vec![39300]);
        b.set_bounds(&prepared.view_items.iter().map(|i| i.bbox).collect::<Vec<_>>());
        assert!(b.states.iter().all(|g| g.bounds.is_some()), "every state can be flown to");

        // The in-view recount over the index: a camera on Rhode Island
        // sees its two block groups, not DC's.
        let index = ViewIndex::build(&prepared.view_items);
        let camera = gpui_maps::render::Camera {
            center_lon: -71.49,
            center_lat: 41.805,
            zoom: 12.0,
            pitch: 0.0,
            bearing: 0.0,
            viewport_width_px: 800.0,
            viewport_height_px: 600.0,
            fov_y: gpui_maps::render::Camera::DEFAULT_FOV_Y,
        };
        let mut visible = in_view_indices_indexed(&index, &prepared.view_items, &camera, true);
        visible.sort_unstable();
        assert_eq!(visible, vec![1, 2]);
        let in_view = b.in_view(&visible, IN_VIEW_GROUPS);
        assert_eq!(in_view.block_groups, 2);
        assert_eq!(in_view.states.len(), 1, "Rhode Island alone");
        assert_eq!(in_view.metros.len(), 1, "Providence");
    }

    /// A raised cancel stops a pack load between states, as it does the
    /// shapefiles.
    #[test]
    fn a_pack_load_stops_between_states_when_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_pack(dir.path());
        let cancel = AtomicBool::new(false);
        let seen = std::sync::Mutex::new(Vec::new());
        let r = load_cancellable(&pack_options(dir.path(), path.clone(), &["11", "44"]), &cancel, &|done, total| {
            seen.lock().unwrap().push((done, total));
            cancel.store(true, Ordering::SeqCst);
        });
        assert!(r.is_none(), "a cancelled load returns nothing");
        assert_eq!(*seen.lock().unwrap(), vec![(1, 2)], "no state after the cancel is decoded");
        // Raised before it starts: not even the tables.
        let r = load_cancellable(&pack_options(dir.path(), path, &["11", "44"]), &AtomicBool::new(true), &|_, _| panic!("no state"));
        assert!(r.is_none());
    }

    /// Only the states asked for are read from a pack.
    #[test]
    fn a_pack_load_reads_only_the_states_asked_for() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_pack(dir.path());
        let r = load_cancellable(&pack_options(dir.path(), path, &["44"]), &AtomicBool::new(false), &|_, _| {});
        let Some(Payload::Shapes(data)) = r else { panic!("loads") };
        assert_eq!(data.states, vec![("44".to_string(), 0, 2)]);
        assert_eq!(data.features[0].geoid, 440010301001.0);
    }

    /// A pack that is not there, or does not decode, falls back to the
    /// shapefiles, and the note says which and why.
    #[test]
    fn a_missing_or_broken_pack_falls_back_to_the_shapefiles_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let broken = dir.path().join("broken.bgpk");
        std::fs::write(&broken, b"PMTiles, not a pack").unwrap();
        for (pack, says) in [(dir.path().join("absent.bgpk"), "No pack at"), (broken, "The pack did not load")] {
            let notes = std::sync::Mutex::new(Vec::new());
            let opts = pack_options(dir.path(), pack, &["44"]);
            let r = load_cancellable_noting(&opts, &AtomicBool::new(false), &|_, _| {}, &|n| notes.lock().unwrap().push(n));
            let Some(Payload::Shapes(data)) = r else { panic!("the shapefile path returns") };
            assert!(data.pack.is_none() && data.features.is_empty(), "no shapefiles in the temp dir either");
            // The card's last word is the fallback (a broken pack was
            // announced as the pack first).
            let notes = notes.lock().unwrap();
            let last = notes.last().expect("a note");
            assert!(last.starts_with(says) && last.contains("shapefiles"), "{notes:?}");
        }
        // Prepared from the shapefile path, the breakdowns read the data dir.
        let prepared = prepare(BlockGroups::default(), dir.path());
        assert!(matches!(prepared.breakdown_tables, BreakdownTables::Dir(_)) && prepared.simplified_m.is_none());
    }

    /// With the pack on disk the panel needs only it; without, the
    /// shapefile data as before.
    #[test]
    fn the_requirement_is_the_pack_when_it_is_there() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_pack(dir.path());
        let file = |reqs: Vec<crate::kit::Requirement>| {
            reqs.into_iter()
                .find_map(|r| match r {
                    crate::kit::Requirement::File { path, .. } => Some(path),
                    _ => None,
                })
                .unwrap()
        };
        assert_eq!(file(requirements_with(dir.path(), Some(&path))), path);
        let absent = dir.path().join("absent.bgpk");
        assert_eq!(file(requirements_with(dir.path(), Some(&absent))), dir.path().join("acs/income_bg_2023.csv"));
        assert_eq!(file(requirements_with(dir.path(), None)), dir.path().join("acs/income_bg_2023.csv"));
    }

    /// M5: `census-pack` bails with "--out FILE is required" without it --
    /// the pack requirement's fetch command must be the one that works
    /// ([`PACK_BUILD`], the same one `web/run.sh` prints), not a shorter
    /// copy missing the flag.
    #[test]
    fn the_pack_requirements_fetch_command_includes_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_pack(dir.path());
        let fetch = requirements_with(dir.path(), Some(&path))
            .into_iter()
            .find_map(|r| match r {
                crate::kit::Requirement::File { fetch, .. } => Some(fetch),
                _ => None,
            })
            .unwrap();
        assert!(fetch.contains("--out"), "{fetch}");
        assert_eq!(fetch, PACK_BUILD, "one copy of the command, not a second that can drift");
    }

    /// M4: the pack file is read off disk unbounded before this fix --
    /// a huge or corrupt `US_BG_PACK` would be read whole into memory.
    /// `load_pack_cancellable` must refuse it by its on-disk size before
    /// `std::fs::read` ever runs.
    #[test]
    fn a_pack_over_the_size_limit_is_refused_before_it_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.bgpk");
        // A sparse file: its declared length is over the limit without
        // actually writing that many bytes to disk.
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(MAX_PACK_BYTES + 1).unwrap();
        let cancel = AtomicBool::new(false);
        let Err(err) = load_pack_cancellable(&path, &["44".to_string()], &cancel, &|_, _| {}) else {
            panic!("an over-the-limit pack must be refused, not read")
        };
        assert!(err.contains("over the"), "{err}");
    }

    /// `GPUI_MAP_HOVER_DEBUG=0` must not turn debug output on -- only `1`
    /// or `true` (any case) does; unset, empty or anything else is off.
    #[test]
    fn hover_debug_is_on_only_for_1_or_true() {
        assert!(hover_debug_value(Some("1")));
        assert!(hover_debug_value(Some("true")));
        assert!(hover_debug_value(Some("TRUE")));
        assert!(hover_debug_value(Some("True")));
        assert!(!hover_debug_value(Some("0")));
        assert!(!hover_debug_value(Some("")));
        assert!(!hover_debug_value(Some("yes")));
        assert!(!hover_debug_value(Some("2")));
        assert!(!hover_debug_value(None));
    }

    /// The loading card's bar: a fraction, empty with no total.
    #[test]
    fn the_loading_bar_is_the_fraction_done() {
        assert_eq!(loading_fraction(0, 56), 0.0);
        assert_eq!(loading_fraction(28, 56), 0.5);
        assert_eq!(loading_fraction(56, 56), 1.0);
        assert_eq!(loading_fraction(3, 0), 0.0);
        assert_eq!(loading_fraction(9, 4), 1.0);
    }

    /// Real data, both sources: the pack and the shapefiles hold the same
    /// block groups with the same income join, and the pack loads in a
    /// fraction of the time. `cargo test --release --example studio --
    /// --ignored pack_and_shapefiles --nocapture`.
    #[test]
    #[ignore = "reads ~1.1 GB of shapefiles and the 41 MB pack"]
    fn pack_and_shapefiles_hold_the_same_block_groups() {
        let pack = default_pack();
        assert!(pack.exists(), "{}", pack.display());
        let never = AtomicBool::new(false);
        let t = Instant::now();
        let Some(Payload::Shapes(p)) = load_cancellable(&Options::default(), &never, &|_, _| {}) else { panic!("pack") };
        let pack_load = t.elapsed();
        assert!(p.pack.is_some(), "read the pack");
        let t = Instant::now();
        let prepared = prepare(p, &default_data());
        let pack_prepare = t.elapsed();
        let t = Instant::now();
        let Some(Payload::Shapes(s)) = load_cancellable(&Options { pack: None, ..Options::default() }, &never, &|_, _| {}) else {
            panic!("shapefiles")
        };
        let shp_load = t.elapsed();
        println!(
            "pack: load {:.1} ms + prepare {:.1} ms; shapefiles: load {:.1} ms",
            pack_load.as_secs_f64() * 1e3,
            pack_prepare.as_secs_f64() * 1e3,
            shp_load.as_secs_f64() * 1e3
        );
        assert_eq!(prepared.view_items.len(), s.features.len(), "block groups");
        let pack_rows: HashMap<u64, Option<f64>> = prepared.breakdown_rows.iter().copied().collect();
        assert_eq!(pack_rows.len(), s.features.len(), "unique GEOIDs");
        let (mut same, mut differ) = (0usize, 0usize);
        for bg in &s.features {
            match pack_rows.get(&(bg.geoid as u64)) {
                Some(income) if *income == bg.income => same += 1,
                _ => differ += 1,
            }
        }
        let with_income = |it: &mut dyn Iterator<Item = Option<f64>>| it.filter(Option::is_some).count();
        println!(
            "incomes: pack {} / shapefiles {} with an income; {same} agree, {differ} differ",
            with_income(&mut pack_rows.values().copied()),
            with_income(&mut s.features.iter().map(|bg| bg.income))
        );
        assert_eq!(differ, 0);
    }

    /// Real data: the synchronous push of the pack's block groups (what
    /// `mount` runs on the UI thread), headless. `cargo test --release
    /// --example studio -- --ignored pack_push --nocapture`.
    #[test]
    #[ignore = "reads the 41 MB pack"]
    fn pack_push_time() {
        let never = AtomicBool::new(false);
        let Some(Payload::Shapes(p)) = load_cancellable(&Options::default(), &never, &|_, _| {}) else { panic!("pack") };
        assert!(p.pack.is_some(), "read the pack");
        let mut app = gpui::TestAppContext::single();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let cx = app.add_empty_window();
        let map = cx.update(|_, cx| {
            let (style, source) = basemap_with(Some(Basemap::Positron), None);
            cx.new(|_| MapView::new(-98.0, 39.0, 3.5, 1200.0, 800.0, style, source, rt.handle().clone()))
        });
        let started = Instant::now();
        let (fill, _, n) = cx.update(|_, cx| map.update(cx, |view, cx| push(view, &p, true, false, cx)));
        println!(
            "push (sync): {n} sources, fill {:.1} ms, wall {:.1} ms",
            fill.as_secs_f64() * 1e3,
            started.elapsed().as_secs_f64() * 1e3
        );
        drop(map);
        drop(rt);
    }

    #[test]
    fn census_requirement_names_the_fetch_script_and_size() {
        let env_path = std::path::PathBuf::from("/nonexistent");
        let reqs = requirements_for(&env_path);
        let file = reqs
            .iter()
            .find_map(|r| match r {
                crate::kit::Requirement::File { fetch, size, .. } => Some((*fetch, *size)),
                _ => None,
            })
            .unwrap();
        assert!(file.0.contains("demos/data/fetch_us_block_groups.sh"));
        assert!(file.1.contains("1.1 GB"));
        // The pack is what the panel reads first; the card says how to
        // build it, from these shapefiles.
        let then = reqs.iter().find_map(|r| match r {
            crate::kit::Requirement::File { then, .. } => *then,
            _ => None,
        });
        assert!(then.is_some_and(|t| t.contains("tools/census-pack") && t.contains("us_bg_2023.bgpk")), "{then:?}");
        // No fallback: without the data the gallery shows the card.
        let r = reqs.iter().find(|r| matches!(r, crate::kit::Requirement::File { .. })).unwrap();
        assert!(matches!(crate::kit::check(r, &|_| None, &|p| p.exists()), crate::kit::Status::Missing { .. }));
    }

    /// The gallery side of the window -> `MapView` chain (Task 11 review):
    /// `CensusPanel`'s render sets no background, and hands the map to
    /// either the shell (loading, failed) or `DemoRoot` (ready), neither of
    /// whose renders sets one (`DemoRoot`'s is pinned above). The status
    /// card, drawn OVER the map, is the gallery section's one background.
    #[test]
    fn the_gallery_chain_down_to_the_map_sets_no_background() {
        let needle = [".", "bg", "("].concat();
        let block = |source: &'static str, start: &str| -> &'static str {
            let at = source.find(start).unwrap_or_else(|| panic!("{start}"));
            &source[at..at + source[at..].find("\n}\n").expect("the impl ends")]
        };
        let source = include_str!("census.rs");
        let panel = block(source, &["impl Render for ", "CensusPanel"].concat());
        let root = "gpui::div().relative().size_full()";
        assert!(panel.contains(root), "the panel's root div is where this test looks");
        assert!(!panel.contains(&needle), "CensusPanel's render must not set a background");
        assert!(panel.replacen(root, &[root, ".bg(gpui::black())"].concat(), 1).contains(&needle), "the pin bites");
        assert!(panel.contains("root.child(self.shell.clone())") && panel.contains("root.child(census.clone())"));
        let shell = block(include_str!("../shell.rs"), &["impl Render for ", "DemoShell"].concat());
        assert!(shell.contains("self.map") && !shell.contains(&needle), "the shell holds the map and sets no background");
        let start = source.find(&["// ------------------------------------------------------------ ", "the gallery"].concat()).unwrap();
        let end = source.find(&["#[cfg(test)]\n", "mod tests"].concat()).unwrap();
        // The status card's pill, and its progress bar's track and fill
        // inside the pill: nothing else in the gallery section.
        assert_eq!(source[start..end].matches(&needle).count(), 3, "the status card's are the only backgrounds");
        let card = &source[source.find(&["fn ", "status_card("].concat()).unwrap()..end];
        let card = &card[..card.find("\n}\n").unwrap()];
        assert_eq!(card.matches(&needle).count(), 3, "all three are the card's");
        // Review F6: and it is the inner pill's, not the full-panel
        // overlay's -- `absolute().inset_0()` covers the whole map.
        let overlay_of = |source: &str| -> String {
            let card = &source[source.find(&["fn ", "status_card("].concat()).expect("the card")..];
            let from = card.find("inset_0()").expect("the overlay");
            let to = from + card[from..].find("gpui::div()").expect("the pill");
            card[from..to].to_string()
        };
        assert!(!overlay_of(source).contains(&needle), "the status card's full-panel overlay must not set a background");
        let moved = source.replacen("inset_0()", &["inset_0()", ".bg(gpui::white())"].concat(), 1);
        assert!(overlay_of(&moved).contains(&needle), "the pin bites");
    }

    /// A gallery panel over a fake load, for the switch-away tests.
    fn env(rt: &tokio::runtime::Runtime) -> crate::kit::PanelEnv {
        crate::kit::PanelEnv {
            host: crate::kit::Host::Gallery,
            runtime: rt.handle().clone(),
            mapbox_token: None,
            viewport: (800.0, 600.0),
            cancel: Default::default(),
            clickhouse: None,
        }
    }

    /// Records the thread it was dropped on.
    struct DropProbe(Arc<std::sync::Mutex<Option<std::thread::ThreadId>>>);
    impl Drop for DropProbe {
        fn drop(&mut self) {
            *self.0.lock().unwrap() = Some(std::thread::current().id());
        }
    }

    /// Switched away 3 states into a 5-state load (the flag raised, the
    /// panel dropped, as `PanelSlot::switch` does): the load stops at the
    /// next state, nothing is mounted, and the panel and its map go.
    #[test]
    fn a_switch_away_during_the_load_stops_it_and_no_census_appears() {
        use std::sync::atomic::AtomicUsize;
        let mut app = gpui::TestAppContext::single();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let env = env(&rt);
        let (mounted, read) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let cx = app.add_empty_window();
        let (m, r, flag) = (mounted.clone(), read.clone(), env.cancel.clone());
        let built = cx
            .update(|_, cx| {
                build_with(
                    &env,
                    Options::default(),
                    5,
                    Default::default(),
                    move |cancel, progress| {
                        for done in 1..=5 {
                            if cancel.load(Ordering::SeqCst) {
                                return Ok(None);
                            }
                            r.store(done, Ordering::SeqCst);
                            progress(done, 5);
                            if done == 3 {
                                flag.store(true, Ordering::SeqCst); // the switch, mid-load
                            }
                        }
                        Ok(Some(()))
                    },
                    move |_, (), _| {
                        m.fetch_add(1, Ordering::SeqCst);
                    },
                    cx,
                )
            })
            .expect("the basemap builds");
        let (view, map) = (built.view.downgrade(), built.map.clone().unwrap());
        let panel: Entity<CensusPanel> = built.view.clone().downcast().expect("CensusPanel");
        let shell = panel.read_with(cx, |p, _| p.shell.downgrade());
        drop(panel);
        drop(built);
        // The effect flush that releases dropped entities, as the gallery's
        // next frame would.
        cx.update(|_, _| {});
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(250));
        cx.run_until_parked();
        assert_eq!(read.load(Ordering::SeqCst), 3, "no state after the switch is read");
        assert_eq!(mounted.load(Ordering::SeqCst), 0, "no census appears");
        assert!(view.upgrade().is_none() && shell.upgrade().is_none(), "the panel and its shell are released");
        assert!(map.upgrade().is_none(), "the map is released");
        drop(rt);
    }

    /// A load that finishes after the switch (it never looked at the flag)
    /// is not mounted, and its payload is dropped on a thread of its own.
    #[test]
    fn a_load_that_lands_after_the_switch_is_dropped_off_the_ui_thread() {
        use std::sync::atomic::AtomicUsize;
        let mut app = gpui::TestAppContext::single();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let env = env(&rt);
        let mounted = Arc::new(AtomicUsize::new(0));
        let dropped_on = Arc::new(std::sync::Mutex::new(None));
        let cx = app.add_empty_window();
        let (m, probe) = (mounted.clone(), DropProbe(dropped_on.clone()));
        let built = cx
            .update(|_, cx| {
                build_with(&env, Options::default(), 1, Default::default(), move |_, _| Ok(Some(probe)), move |_, _: DropProbe, _| {
                    m.fetch_add(1, Ordering::SeqCst);
                }, cx)
            })
            .expect("the basemap builds");
        env.cancel.store(true, Ordering::SeqCst);
        drop(built);
        cx.run_until_parked();
        let deadline = Instant::now() + Duration::from_secs(5);
        while dropped_on.lock().unwrap().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let on = dropped_on.lock().unwrap().expect("the payload was dropped");
        assert_ne!(on, std::thread::current().id(), "not on the UI thread");
        assert_eq!(mounted.load(Ordering::SeqCst), 0, "no census appears");
        drop(rt);
    }

    /// Review F7: the completion's own cancel check, alone. The panel is
    /// still alive (as for the frame after a switch, while the drawn frame
    /// still references it) but its flag is raised: the result must not
    /// mount, and is dropped off the UI thread.
    #[test]
    fn a_raised_cancel_with_the_panel_still_alive_does_not_mount() {
        use std::sync::atomic::AtomicUsize;
        let mut app = gpui::TestAppContext::single();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let env = env(&rt);
        let mounted = Arc::new(AtomicUsize::new(0));
        let dropped_on = Arc::new(std::sync::Mutex::new(None));
        let cx = app.add_empty_window();
        let (m, probe) = (mounted.clone(), DropProbe(dropped_on.clone()));
        let built = cx
            .update(|_, cx| {
                build_with(&env, Options::default(), 1, Default::default(), move |_, _| Ok(Some(probe)), move |_, _: DropProbe, _| {
                    m.fetch_add(1, Ordering::SeqCst);
                }, cx)
            })
            .expect("the basemap builds");
        env.cancel.store(true, Ordering::SeqCst);
        cx.run_until_parked();
        let deadline = Instant::now() + Duration::from_secs(5);
        while dropped_on.lock().unwrap().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let on = dropped_on.lock().unwrap().expect("the payload was dropped");
        assert_ne!(on, std::thread::current().id(), "not on the UI thread");
        assert_eq!(mounted.load(Ordering::SeqCst), 0, "a cancelled panel mounts nothing, even while alive");
        assert!(built.view.clone().downcast::<CensusPanel>().is_ok(), "the panel was alive throughout");
        drop(built);
        drop(rt);
    }

    /// A mounted census (the real `mount`: push, `DemoRoot`, breakdowns,
    /// the in-view poll) releases its map the moment the gallery drops the
    /// panel -- no task may hold it strongly, or the leak gate's close
    /// sample sees it alive (the first `--cycle census,basic` run did).
    #[test]
    fn a_mounted_census_releases_its_map_with_the_panel() {
        let mut app = gpui::TestAppContext::single();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let env = env(&rt);
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().to_path_buf();
        let cx = app.add_empty_window();
        let built = cx
            .update(|_, cx| {
                let prepared = prepare(BlockGroups::default(), &data_dir);
                build_with(
                    &env,
                    Options::default(),
                    1,
                    Default::default(),
                    move |_, _| Ok(Some(prepared)),
                    move |panel, prepared, cx| mount(panel, prepared, true, cx),
                    cx,
                )
            })
            .expect("the basemap builds");
        let panel: Entity<CensusPanel> = built.view.clone().downcast().expect("CensusPanel");
        cx.run_until_parked();
        let root = panel.read_with(cx, |p, _| match &p.state {
            PanelState::Ready(root) => root.downgrade(),
            _ => panic!("mounted"),
        });
        cx.executor().advance_clock(Duration::from_millis(500));
        cx.run_until_parked();
        let map = built.map.clone().unwrap();
        drop(panel);
        drop(built);
        cx.update(|_, _| {});
        assert!(root.upgrade().is_none(), "the census root is released");
        assert!(map.upgrade().is_none(), "the map is released at once, not on the next poll");
        drop(rt);
    }

    /// The positive control: left open, the load's progress reaches the
    /// loading line and the result is mounted; a failed load says why.
    #[test]
    fn an_open_panel_shows_progress_then_mounts_or_says_why() {
        use std::sync::atomic::AtomicUsize;
        let mut app = gpui::TestAppContext::single();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let env = env(&rt);
        let mounted = Arc::new(AtomicUsize::new(0));
        let cx = app.add_empty_window();
        let m = mounted.clone();
        let built = cx
            .update(|_, cx| {
                build_with(
                    &env,
                    Options::default(),
                    2,
                    Default::default(),
                    |_, progress| {
                        progress(1, 2);
                        progress(2, 2);
                        Ok(Some(()))
                    },
                    move |_, (), _| {
                        m.fetch_add(1, Ordering::SeqCst);
                    },
                    cx,
                )
            })
            .expect("the basemap builds");
        let panel: Entity<CensusPanel> = built.view.clone().downcast().expect("the view is CensusPanel");
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(250));
        cx.run_until_parked();
        assert_eq!(mounted.load(Ordering::SeqCst), 1, "mounted once");
        panel.read_with(cx, |p, _| match p.state {
            PanelState::Loading { done, total } => assert_eq!((done, total), (2, 2)),
            _ => panic!("the fake mount leaves the state alone"),
        });
        assert_eq!(CensusPanel::loading_text(2, 2), "Reading block groups: 2/2 states");

        let built_failed = cx
            .update(|_, cx| build_with(&env, Options::default(), 1, Default::default(), |_, _| Err::<Option<()>, _>("no shapefiles".into()), |_, (), _| {}, cx))
            .expect("the basemap builds");
        let failed: Entity<CensusPanel> = built_failed.view.clone().downcast().expect("CensusPanel");
        cx.run_until_parked();
        failed.read_with(cx, |p, _| match &p.state {
            PanelState::Failed(why) => assert!(why.contains("no shapefiles"), "{why}"),
            _ => panic!("a failed load says so"),
        });
        drop((built, built_failed));
        drop(rt);
    }

    /// The panel's state hover dims every other state by colour alone:
    /// each opaque colour gains an alpha, nothing else in the expression
    /// moves, and the result still parses.
    #[test]
    fn a_dimmed_paint_changes_colours_only() {
        for range in [IncomeRange::full(), IncomeRange { lo: 40, hi: 120 }] {
            for outline in [false, true] {
                let json = range.paint_json(outline);
                let dim = dimmed_paint_json(&json);
                assert!(!dim.contains("\"#"), "{dim}");
                assert!(dim.contains("\"rgba(68,1,84,0.28)\""), "{dim}");
                assert_eq!(dim.contains("rgba(158,158,158,0.28)"), range.is_full(), "no-data grey dims only where it shows");
                assert!(!dim.contains(OUTLINE_COLOR));
                assert_eq!(dim.contains(&format!("\"{}\"", dimmed_outline())), outline, "the hairline dims with the fill");
                // Same shape: strip every colour string and compare.
                let shape = |s: &str| s.split('"').enumerate().filter(|(i, _)| i % 2 == 0).map(|(_, p)| p).collect::<String>();
                assert_eq!(shape(&dim), shape(&json));
                let _ = paint_object(&dim);
            }
        }
    }

    #[test]
    fn a_per_state_source_names_its_state() {
        assert_eq!(source_state("block-groups-06"), Some(6));
        assert_eq!(source_state("block-groups-72"), Some(72));
        assert_eq!(source_state("block-groups"), None);
    }

    /// `view_items`' `pack::PackRow` adapter round-trips a `Bg.income:
    /// Option<f64>` through `PackRow.income: Option<u32>` (`view_items`'s
    /// `i as u32`, undone by `view::view_item_of`'s `f64::from`). Every
    /// ACS 2023 median household income `join_income` reads is a whole
    /// dollar amount, so this must be exact -- were it not (a fractional
    /// income, or one past `u32::MAX`), the block group would silently
    /// count in the wrong histogram bar.
    #[test]
    fn income_round_trips_through_pack_row_for_whole_dollar_incomes() {
        for income in [0.0, 1.0, 2_500.0, 87_250.0, 126_250.0, 250_000.0, 999_999.0] {
            assert_eq!(f64::from(income as u32), income, "{income}");
        }
    }

    /// I1: the native card and the browser popup print the same five
    /// lines for the same block group, a real income and a missing one.
    #[test]
    fn the_native_card_and_the_browser_popup_say_the_same_words() {
        use gpui_maps::census::hover::{popup_json, HoverInfo};
        let counties = HashMap::from([(6037, "Los Angeles County".to_string())]);
        for income in [Some(87_250u32), Some(0), None] {
            // ALAND as native reads it (m²) and as the pack stores it (f32 km²).
            let aland_m2 = 421_987.0;
            let native = tooltip_lines(60_375_432_011, Some((income.map(f64::from), aland_m2)), &counties);
            let info = HoverInfo { geoid: 60_375_432_011, income, aland_km2: (aland_m2 / 1e6) as f32 };
            let web: serde_json::Value = serde_json::from_str(&popup_json(&info, &counties)).unwrap();
            let web = ["title", "subtitle", "income", "aland", "geoid_line"].map(|k| web[k].as_str().unwrap().to_string());
            assert_eq!(native, web, "income {income:?}");
        }
    }

    /// The shared card keeps the five lines' words, as rows.
    #[test]
    fn the_popup_card_keeps_the_tooltip_words() {
        let counties = HashMap::from([(6037, "Los Angeles County".to_string())]);
        let card = census_card(60_375_432_011, Some((Some(87_250.0), 421_987.0)), &counties);
        assert_eq!(card.title, "Los Angeles County, California");
        assert_eq!(card.subtitle.as_deref(), Some("Tract 5432.01 · Block group 1"));
        assert_eq!(card.rows[0], ("Median household income".to_string(), "$87,250".to_string()));
        assert_eq!(card.rows[1], ("Land area".to_string(), "0.42 km²".to_string()));
        assert_eq!(card.rows[2], ("GEOID".to_string(), "060375432011".to_string()));
        let none = census_card(60_375_432_011, Some((None, 421_987.0)), &counties);
        assert_eq!(none.rows[0].1, "No estimate");
    }

    #[test]
    fn a_missing_names_file_loads_an_empty_table_without_panicking() {
        let dir = std::env::temp_dir().join("gpui-map-test-missing-names-dir-does-not-exist");
        let table = load_county_names(&dir);
        assert!(table.is_empty());
    }

    #[test]
    fn the_names_file_format_parses_into_the_table() {
        let dir = std::env::temp_dir().join(format!("gpui-map-test-names-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("names")).expect("temp dir");
        std::fs::write(
            dir.join("names").join("2023_Gaz_counties_national.txt"),
            "USPS\tGEOID\tANSICODE\tNAME\tALAND\tAWATER\tALAND_SQMI\tAWATER_SQMI\tINTPTLAT\tINTPTLONG\n\
             CA\t06037\t00277283\tLos Angeles County\t1\t2\t3\t4\t5\t6\n\
             CT\t09110\t02830244\tCapitol Planning Region\t1\t2\t3\t4\t5\t6\n",
        )
        .expect("temp file");
        let table = load_county_names(&dir);
        assert_eq!(table.get(&6037).map(String::as_str), Some("Los Angeles County"));
        assert_eq!(table.get(&9110).map(String::as_str), Some("Capitol Planning Region"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_slider_stops_are_the_ramp_ends_and_every_step_boundary() {
        let stops = slider_stops();
        assert_eq!(stops.len(), 250, "249 classes on viridis's nine even stops, 250 handle positions");
        assert_eq!((stops[0], stops[249]), (2_500.0, 250_000.0));
        assert!(stops.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn a_handle_position_maps_to_an_income_and_back() {
        let stops = slider_stops();
        for (i, &income) in stops.iter().enumerate() {
            assert_eq!(IncomeRange::nearest_stop(income), i);
        }
        assert_eq!(IncomeRange::nearest_stop(-1e9), 0);
        assert_eq!(IncomeRange::nearest_stop(1e9), 249);
        // Window x across the bar maps onto the ramp's span, clamped.
        let layout = LegendLayout { bar: 300.0, ..Default::default() };
        let left = LEGEND_ORIGIN.0 + LEGEND_PAD;
        assert_eq!(layout.income_at(left), 2_500.0);
        assert_eq!(layout.income_at(left + 300.0), 250_000.0);
        assert_eq!(layout.income_at(left + 150.0), 126_250.0);
        assert_eq!(layout.income_at(0.0), 2_500.0);
        assert_eq!(layout.income_at(5_000.0), 250_000.0);
    }

    #[test]
    fn a_range_selects_exactly_its_incomes_and_hides_no_data() {
        let stops = slider_stops();
        let full = IncomeRange::full();
        assert!(full.contains(None) && full.contains(Some(0.0)) && full.contains(Some(1e9)));
        assert_eq!(full.bounds(), (None, None));
        let r = IncomeRange { lo: 10, hi: 50 };
        assert_eq!(r.bounds(), (Some(stops[10]), Some(stops[50])));
        assert!(!r.contains(None), "no data is hidden while a range is set");
        assert!(r.contains(Some(stops[10])) && r.contains(Some(stops[50] - 0.01)));
        assert!(!r.contains(Some(stops[10] - 0.01)) && !r.contains(Some(stops[50])));
        // The open ends: the lowest handle keeps the Census's "2,500-".
        assert!(IncomeRange { lo: 0, hi: 5 }.contains(Some(2_499.0)));
        assert!(IncomeRange { lo: 200, hi: 249 }.contains(Some(250_001.0)));
        assert_eq!(IncomeRange { lo: 0, hi: 249 }.label(), "$2.5K – $250K");
    }

    /// The demo's mirror of the engine's quantisation, checked against the
    /// engine: one feature just inside each end of every band, pushed to a
    /// small (untiled, so merged-plan) source. The engine's op count is
    /// its class count, and it must be exactly one per band plus no-data --
    /// at the full span and under a snapped range alike. An unsnapped edge
    /// splits a band, which is what the table-only path must refuse.
    #[test]
    fn a_snapped_range_keeps_every_engine_paint_class() {
        let stops = slider_stops();
        let mut incomes: Vec<Option<f64>> = Vec::new();
        for band in stops.windows(2) {
            incomes.push(Some((band[0] + 1.0).ceil()));
            incomes.push(Some((band[1] - 1.0).floor()));
        }
        incomes.push(Some(1_000.0));
        incomes.push(Some(400_000.0));
        incomes.push(None);
        let squares: Vec<Vec<[f64; 2]>> = (0..incomes.len())
            .map(|i| {
                let x = (i % 40) as f64 * 0.1;
                let y = (i / 40) as f64 * 0.1;
                vec![[x, y], [x + 0.05, y], [x + 0.05, y + 0.05], [x, y + 0.05], [x, y]]
            })
            .collect();
        let rings: Vec<[&[[f64; 2]]; 1]> = squares.iter().map(|s| [s.as_slice()]).collect();
        let polygons: Vec<[&[&[[f64; 2]]]; 1]> = rings.iter().map(|r| [r.as_slice()]).collect();
        let classes = |json: &str| {
            let mut store = gpui_maps::overlay::OverlayStore::new();
            store.declare_source_paint("bg", &paint_object(json)).unwrap();
            let features: Vec<FeatureInput<'_>> = polygons
                .iter()
                .enumerate()
                .map(|(i, polygon)| FeatureInput {
                    id: Some(FeatureId::Number(i as f64)),
                    geometry: GeometryInput::Polygon(polygon),
                    paint: FeaturePaint::default(),
                })
                .collect();
            let columns = [("income", PropertyColumn::Number(&incomes))];
            store.set_source_columns("bg", &features, &columns, 4).unwrap();
            store.source_op_count("bg").unwrap()
        };
        assert_eq!(classes(&IncomeRange::full().paint_json(true)), 250, "249 ramp classes + no-data");
        for range in [IncomeRange { lo: 1, hi: 40 }, IncomeRange { lo: 100, hi: 249 }, IncomeRange { lo: 0, hi: 3 }] {
            assert_eq!(classes(&range.paint_json(true)), 250, "{range:?} keeps every class");
        }
        let split = range_paint_json(true, Some((stops[20] + stops[21]) / 2.0), None);
        assert_eq!(classes(&split), 251, "an unsnapped edge splits a band");
    }

    fn check_fits(layout: &LegendLayout) {
        let w = layout.tick_width;
        let l = layout.tick_left;
        assert_eq!(l[0], 0.0, "the first label is flush with the bar's start");
        assert!((l[4] + w[4] - layout.bar).abs() < 1e-3, "the last is flush with its end");
        for i in 1..4 {
            let centre = l[i] + w[i] / 2.0;
            assert!((centre - LEGEND_TICKS[i].1 * layout.bar).abs() < 1e-3, "label {i} centred under its stop");
        }
        for i in 0..4 {
            assert!(l[i + 1] - (l[i] + w[i]) >= LEGEND_LABEL_GAP - 1e-3, "labels {i} and {} are too close", i + 1);
        }
        // Everything sits inside the card's padding: the labels span the
        // bar exactly, and the card is the bar plus a pad either side.
        assert_eq!(layout.card_width(), layout.bar + 2.0 * LEGEND_PAD);
    }

    #[test]
    fn every_tick_label_fits_the_card_with_padding() {
        // Widths as text_xs (12 px) gives them in the system font, then as
        // a wider font would, then the degenerate case.
        for widths in [
            [29.0, 45.5, 52.4, 52.1, 32.3],
            [44.0, 72.0, 84.0, 84.0, 48.0],
            [80.0, 120.0, 140.0, 140.0, 80.0],
            [0.0; 5],
        ] {
            let layout = LegendLayout::solve(widths, LEGEND_MIN_BAR);
            check_fits(&layout);
            assert!(layout.bar >= LEGEND_MIN_BAR);
        }
        // 2x: gpui lays out in logical pixels, so the widths are the same;
        // rounding each to the device's half-logical-pixel grid still fits.
        let at_2x = [29.0f32, 45.5, 52.4, 52.1, 32.3].map(|w| (w * 2.0).ceil() / 2.0);
        check_fits(&LegendLayout::solve(at_2x, LEGEND_MIN_BAR));
    }

    #[test]
    fn every_histogram_bar_sits_under_the_incomes_it_counts() {
        let stops = slider_stops();
        assert_eq!(hist_bins() * HIST_CLASSES_PER_BIN, stops.len() - 1);
        // Bar edges are handle positions, contiguous, spanning the ramp.
        assert_eq!(bin_span(0).0, stops[0]);
        assert_eq!(bin_span(hist_bins() - 1).1, stops[stops.len() - 1]);
        for k in 1..hist_bins() {
            assert_eq!(bin_span(k - 1).1, bin_span(k).0);
        }
        // Any income: its bar's span holds it, so the bar is drawn under
        // the x the slider maps that income to.
        let layout = LegendLayout { bar: 308.0, ..Default::default() };
        let mut income = 2_500.0;
        while income < 250_000.0 {
            let (a, b) = bin_span(income_bin(income));
            assert!(a <= income && income < b, "{income} in [{a}, {b})");
            let x = income_fraction(income) as f32 * layout.bar;
            assert!(income_fraction(a) as f32 * layout.bar <= x && x <= income_fraction(b) as f32 * layout.bar);
            income += 97.3;
        }
        // The clamps: the Census's "2,500-" and "250,000+" land in the end bars.
        assert_eq!(income_bin(0.0), 0);
        assert_eq!(income_bin(250_001.0), hist_bins() - 1);
        // A range whose handles sit on bar edges selects exactly its bars.
        let range = IncomeRange { lo: 30, hi: 150 };
        for k in 0..hist_bins() {
            let (a, b) = bin_span(k);
            let inside = range.contains(Some(a)) && range.contains(Some((a + b) / 2.0));
            assert_eq!(inside, (10..50).contains(&k), "bar {k}");
        }
    }

    /// The slider's press-and-drag geometry, pure: a 300 px bar, about
    /// 1.2 px a stop, so a knob covers about 12 stops.
    mod slider_geometry {
        use super::super::*;

        const AXIS: SliderAxis = SliderAxis { left: 28.0, bar: 300.0 };
        const LAST: usize = 249;

        /// A press at `from` then a move through `path`, the way
        /// `DemoRoot::press_at` and `drag_to` drive it: each range the
        /// drag makes, in order.
        fn press_drag(range: IncomeRange, from: f32, path: &[f32]) -> Vec<IncomeRange> {
            let mut held = slider_grab(AXIS, range, from);
            let mut range = range;
            let mut out = Vec::new();
            if held.is_none() {
                (held, range) = slider_drag(AXIS, range, held, from);
                out.push(range);
            }
            for &x in path {
                (held, range) = slider_drag(AXIS, range, held, x);
                out.push(range);
            }
            out
        }

        fn band_mid(r: IncomeRange) -> f32 {
            (AXIS.knob_x(r.lo) + AXIS.knob_x(r.hi)) / 2.0
        }

        #[test]
        fn a_band_drag_moves_both_handles_and_keeps_the_width() {
            let r = IncomeRange { lo: 60, hi: 120 };
            let x = band_mid(r);
            let steps = press_drag(r, x, &[x + 30.0, x - 50.0]);
            for s in &steps {
                assert_eq!(s.hi - s.lo, 60, "the width holds: {steps:?}");
            }
            assert!(steps[0].lo > 60, "right moves the band right: {steps:?}");
            assert!(steps[1].lo < 60, "left moves it left: {steps:?}");
        }

        #[test]
        fn a_band_drag_clamps_at_each_end_without_shrinking() {
            let r = IncomeRange { lo: 60, hi: 120 };
            let x = band_mid(r);
            let steps = press_drag(r, x, &[-1000.0, x, 5000.0, x]);
            assert_eq!(steps[0], IncomeRange { lo: 0, hi: 60 }, "pinned at the low end");
            assert_eq!(steps[1], r, "back to the press point, back to the start");
            assert_eq!(steps[2], IncomeRange { lo: LAST - 60, hi: LAST }, "pinned at the high end");
            assert_eq!(steps[3], r);
        }

        /// Every pointer x, however fine, moves both knobs by the one
        /// whole number of stops the pointer has crossed.
        #[test]
        fn a_band_drag_snaps_to_stops() {
            let r = IncomeRange { lo: 100, hi: 140 };
            let x = band_mid(r);
            let path: Vec<f32> = (1..200).map(|k| x + k as f32 * 0.37).collect();
            let steps = press_drag(r, x, &path);
            for (s, &px) in steps.iter().zip(&path) {
                let moved = AXIS.stop_at(px) as isize - AXIS.stop_at(x) as isize;
                let lo = (100 + moved).clamp(0, (LAST - 40) as isize) as usize;
                assert_eq!(*s, IncomeRange { lo, hi: lo + 40 }, "at x {px}");
            }
            assert!(steps.last().unwrap().lo > 150, "the sweep crossed many stops: {:?}", steps.last());
        }

        /// A knob's whole face is the knob, even the half over the band;
        /// one pixel past it is the band.
        #[test]
        fn a_press_on_a_handle_still_grabs_that_handle() {
            let r = IncomeRange { lo: 60, hi: 180 };
            let (lo_x, hi_x) = (AXIS.knob_x(r.lo), AXIS.knob_x(r.hi));
            for x in [lo_x, lo_x - KNOB / 2.0 + 0.5, lo_x + KNOB / 2.0 - 0.5] {
                assert!(matches!(slider_grab(AXIS, r, x), Some(Handle::Lo)), "x {x}");
                let s = press_drag(r, x, &[x + 20.0])[0];
                assert!(s.lo > 60 && s.hi == 180, "the low knob alone: {s:?}");
            }
            for x in [hi_x, hi_x + KNOB / 2.0 - 0.5, hi_x - KNOB / 2.0 + 0.5] {
                assert!(matches!(slider_grab(AXIS, r, x), Some(Handle::Hi)), "x {x}");
                let s = press_drag(r, x, &[x - 20.0])[0];
                assert!(s.hi < 180 && s.lo == 60, "the high knob alone: {s:?}");
            }
            let x = lo_x + KNOB / 2.0 + 1.0;
            let s = press_drag(r, x, &[x + 20.0]);
            assert!(s[0].lo > 60 && s[0].hi > 180 && s[0].hi - s[0].lo == 120, "just past the knob, the band: {s:?}");
        }

        /// Knobs a little more than a knob apart leave a sliver of band
        /// between them: each knob's face still grabs that knob, the
        /// sliver the band. Knobs closer than that leave no band, and the
        /// overlap still goes to the drag direction.
        #[test]
        fn a_narrow_band_still_lets_you_pick_a_handle() {
            let lo = 100;
            let hi = (lo + 1..=LAST).find(|&h| AXIS.knob_x(h) - AXIS.knob_x(lo) > KNOB + 3.0).unwrap();
            let r = IncomeRange { lo, hi };
            let (lo_x, hi_x) = (AXIS.knob_x(lo), AXIS.knob_x(hi));
            let s = press_drag(r, lo_x + KNOB / 2.0 - 0.5, &[lo_x - 20.0])[0];
            assert!(s.lo < lo && s.hi == hi, "the low knob's inner edge: {s:?}");
            let s = press_drag(r, hi_x - KNOB / 2.0 + 0.5, &[hi_x + 20.0])[0];
            assert!(s.hi > hi && s.lo == lo, "the high knob's inner edge: {s:?}");
            let x = band_mid(r);
            let s = press_drag(r, x, &[x + 20.0])[0];
            assert!(s.lo > lo && s.hi - s.lo == hi - lo, "the sliver between them, the band: {s:?}");
            // Overlapping knobs: no band at all.
            let r = IncomeRange { lo: 100, hi: 103 };
            let x = band_mid(r);
            assert!(matches!(slider_grab(AXIS, r, x), Some(Handle::Either(_))));
            let s = press_drag(r, x, &[x - 20.0]);
            assert!(s[0].lo < 100 && s[0].hi == 103, "{s:?}");
        }
    }

    /// The slider driven through gpui's own dispatch -- a `TestAppContext`
    /// window at scale factor 2, `simulate_mouse_*` -- with the map drawn
    /// as a stand-in that hit-tests like it (see `DemoRoot::render`).
    pub(super) mod slider {
        use super::super::*;
        use gpui::{prelude::*, Modifiers, MouseButton, TestAppContext, VisualTestContext};
        use std::cell::Cell;

        thread_local! {
            /// Left presses the map (stand-in) received.
            pub static MAP_PRESSES: Cell<usize> = const { Cell::new(0) };
        }

        /// The map's hitbox as `MapView::prepaint` inserts it (`Normal`),
        /// and its mouse-down as `MapView::paint` gates it (on hover).
        pub fn map_stand_in() -> gpui::Div {
            gpui::div()
                .absolute()
                .size_full()
                .on_mouse_down(MouseButton::Left, |_, _, _| MAP_PRESSES.with(|n| n.set(n.get() + 1)))
        }

        pub fn window(cx: &mut TestAppContext) -> (Entity<DemoRoot>, &mut VisualTestContext) {
            MAP_PRESSES.with(|n| n.set(0));
            let rt = runtime();
            let handle = rt.handle().clone();
            std::mem::forget(rt);
            let (root, cx) = cx.add_window_view(|_, cx| {
                let map = cx.new(|_| {
                    MapView::new(
                        START.0,
                        START.1,
                        START.2,
                        800.0,
                        600.0,
                        gpui_maps::style::Style { layers: vec![], sprite: None, sky: Default::default() },
                        Arc::new(gpui_maps::tile_source::EmptyTileSource),
                        handle,
                    )
                });
                DemoRoot::new(map, Arc::new(HashMap::new()), Arc::new(HashMap::new()), Arc::from(Vec::<String>::new()), false, cx)
            });
            draw(cx);
            (root, cx)
        }

        pub fn draw(cx: &mut VisualTestContext) {
            cx.update(|window, cx| {
                window.refresh();
                let _ = window.draw(cx);
            });
            cx.run_until_parked();
        }

        /// The slider's bounds in the last frame.
        pub fn slider(cx: &mut VisualTestContext) -> gpui::Bounds<gpui::Pixels> {
            cx.debug_bounds("income-slider").expect("the slider was drawn")
        }

        /// The window x a handle at stop `i` is drawn at.
        pub fn knob_x(cx: &mut VisualTestContext, root: &Entity<DemoRoot>, i: usize) -> f32 {
            let left = f32::from(slider(cx).origin.x) + KNOB / 2.0;
            let bar = root.read_with(cx, |r, _| r.layout.bar);
            left + income_fraction(slider_stops()[i]) as f32 * bar
        }

        pub fn mid_y(cx: &mut VisualTestContext) -> f32 {
            let b = slider(cx);
            f32::from(b.origin.y) + f32::from(b.size.height) / 2.0
        }

        pub fn at(x: f32, y: f32) -> gpui::Point<gpui::Pixels> {
            gpui::point(px(x), px(y))
        }

        /// Press at `from`, move through `path` with the button held,
        /// release at the last point; a frame drawn after each event, as
        /// the real window would.
        pub fn drag(cx: &mut VisualTestContext, from: (f32, f32), path: &[(f32, f32)]) {
            cx.simulate_mouse_move(at(from.0, from.1), None, Modifiers::default());
            cx.simulate_mouse_down(at(from.0, from.1), MouseButton::Left, Modifiers::default());
            draw(cx);
            for &(x, y) in path {
                cx.simulate_mouse_move(at(x, y), Some(MouseButton::Left), Modifiers::default());
                draw(cx);
            }
            let end = path.last().copied().unwrap_or(from);
            cx.simulate_mouse_up(at(end.0, end.1), MouseButton::Left, Modifiers::default());
            draw(cx);
        }

        pub fn wanted(cx: &mut VisualTestContext, root: &Entity<DemoRoot>) -> IncomeRange {
            root.read_with(cx, |r, _| r.wanted)
        }

        pub fn set(cx: &mut VisualTestContext, root: &Entity<DemoRoot>, lo: usize, hi: usize) {
            root.update(cx, |r, _| {
                r.wanted = IncomeRange { lo, hi };
                r.applied = r.wanted;
            });
            draw(cx);
        }

        /// Hosts a `DemoRoot` at a window offset, the way the gallery
        /// does (the sidebar to its left, the title bar above).
        struct Offset {
            root: Entity<DemoRoot>,
        }

        impl Render for Offset {
            fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
                gpui::div().size_full().child(
                    gpui::div()
                        .absolute()
                        .left(px(OFFSET.0))
                        .top(px(OFFSET.1))
                        .w(px(800.0))
                        .h(px(600.0))
                        .child(self.root.clone()),
                )
            }
        }

        const OFFSET: (f32, f32) = (220.0, 40.0);

        /// [`window`], with the `DemoRoot` at [`OFFSET`] in the window.
        pub fn offset_window(cx: &mut TestAppContext) -> (Entity<DemoRoot>, &mut VisualTestContext) {
            MAP_PRESSES.with(|n| n.set(0));
            let rt = runtime();
            let handle = rt.handle().clone();
            std::mem::forget(rt);
            let (host, cx) = cx.add_window_view(|_, cx| {
                let map = cx.new(|_| {
                    MapView::new(
                        START.0,
                        START.1,
                        START.2,
                        800.0,
                        600.0,
                        gpui_maps::style::Style { layers: vec![], sprite: None, sky: Default::default() },
                        Arc::new(gpui_maps::tile_source::EmptyTileSource),
                        handle,
                    )
                });
                let root = cx.new(|cx| {
                    DemoRoot::new(map, Arc::new(HashMap::new()), Arc::new(HashMap::new()), Arc::from(Vec::<String>::new()), false, cx)
                });
                Offset { root }
            });
            draw(cx);
            draw(cx);
            let root = host.read_with(cx, |h, _| h.root.clone());
            (root, cx)
        }

        /// The user's gallery bug: the slider mapped window x through the
        /// fixed `LEGEND_ORIGIN`, so with the panel offset by the sidebar
        /// the knob never followed the cursor. A press and drag at known
        /// window points must land on the stop drawn under them.
        #[test]
        fn at_a_window_offset_a_drag_lands_the_knob_under_the_cursor() {
            let mut app = TestAppContext::single();
            let (root, cx) = offset_window(&mut app);
            let b = slider(cx);
            assert_eq!(f32::from(b.origin.x) + KNOB / 2.0, OFFSET.0 + LEGEND_ORIGIN.0 + LEGEND_PAD, "drawn at the offset");
            set(cx, &root, 60, 190);
            let y = mid_y(cx);
            let (from, to) = (knob_x(cx, &root, 60), knob_x(cx, &root, 100));
            drag(cx, (from, y), &[((from + to) / 2.0, y), (to, y)]);
            let lo = wanted(cx, &root).lo;
            assert!(lo.abs_diff(100) <= 1, "the low knob follows the cursor to stop 100: {:?}", wanted(cx, &root));
            assert_eq!(wanted(cx, &root).hi, 190);
            // A press on the bare track jumps the nearer knob there.
            set(cx, &root, 60, 190);
            let x = knob_x(cx, &root, 30);
            drag(cx, (x, y), &[]);
            assert!(wanted(cx, &root).lo.abs_diff(30) <= 1, "{:?}", wanted(cx, &root));
            assert_eq!(MAP_PRESSES.with(Cell::get), 0, "a press on the card never reaches the map");
        }

        /// The histogram hover at the same offset names the bar under the
        /// cursor.
        #[test]
        fn at_a_window_offset_histogram_hover_names_the_bar_under_the_cursor() {
            let mut app = TestAppContext::single();
            let (root, cx) = offset_window(&mut app);
            let h = cx.debug_bounds("income-histogram").expect("the histogram was drawn");
            let bar = root.read_with(cx, |r, _| r.layout.bar);
            let income = 100_000.0;
            let x = f32::from(h.origin.x) + income_fraction(income) as f32 * bar;
            let y = f32::from(h.origin.y) + HIST_HEIGHT / 2.0;
            cx.simulate_mouse_move(at(x, y), None, Modifiers::default());
            draw(cx);
            assert_eq!(root.read_with(cx, |r, _| r.hist_hover), Some(income_bin(income)));
        }

        #[test]
        fn the_window_is_hidpi_and_the_slider_sits_where_the_layout_says() {
            let mut app = TestAppContext::single();
            let (root, cx) = window(&mut app);
            assert_eq!(cx.update(|window, _| window.scale_factor()), 2.0);
            let b = slider(cx);
            assert_eq!(f32::from(b.origin.x) + KNOB / 2.0, LEGEND_ORIGIN.0 + LEGEND_PAD);
            assert_eq!(f32::from(b.size.width), root.read_with(cx, |r, _| r.layout.bar) + KNOB);
        }

        /// The histogram strip is the bar's own axis: same left edge, same
        /// width, just under the slider; hovering a bar names that bar.
        #[test]
        fn the_histogram_sits_on_the_slider_axis_and_hover_names_its_bar() {
            let mut app = TestAppContext::single();
            let (root, cx) = window(&mut app);
            let (s, h) = (slider(cx), cx.debug_bounds("income-histogram").expect("the histogram was drawn"));
            let bar = root.read_with(cx, |r, _| r.layout.bar);
            assert_eq!(f32::from(h.origin.x), f32::from(s.origin.x) + KNOB / 2.0);
            assert_eq!(f32::from(h.size.width), bar);
            assert!(h.origin.y >= s.origin.y + s.size.height, "under the slider");
            let income = 100_000.0;
            let x = f32::from(h.origin.x) + income_fraction(income) as f32 * bar;
            let y = f32::from(h.origin.y) + HIST_HEIGHT / 2.0;
            cx.simulate_mouse_move(at(x, y), None, Modifiers::default());
            draw(cx);
            assert_eq!(root.read_with(cx, |r, _| r.hist_hover), Some(income_bin(income)));
            cx.simulate_mouse_move(at(x, y + 300.0), None, Modifiers::default());
            draw(cx);
            assert_eq!(root.read_with(cx, |r, _| r.hist_hover), None);
        }

        #[test]
        fn a_press_on_a_knob_centre_then_a_drag_moves_it() {
            let mut app = TestAppContext::single();
            let (root, cx) = window(&mut app);
            let (x, y) = (knob_x(cx, &root, 0), mid_y(cx));
            drag(cx, (x, y), &[(x + 30.0, y), (x + 60.0, y)]);
            assert!(wanted(cx, &root).lo > 0, "{:?}", wanted(cx, &root));
            assert_eq!(MAP_PRESSES.with(Cell::get), 0, "a press on the card never reaches the map");
        }

        /// The default selection is the full span: both knobs sit on the
        /// slider's ends, half of each hanging past the slider's box.
        #[test]
        fn grabbing_the_low_knob_by_its_outer_half_drags_it() {
            let mut app = TestAppContext::single();
            let (root, cx) = window(&mut app);
            let (x, y) = (knob_x(cx, &root, 0), mid_y(cx));
            drag(cx, (x - 5.0, y), &[(x + 30.0, y), (x + 60.0, y)]);
            assert!(wanted(cx, &root).lo > 0, "{:?}", wanted(cx, &root));
        }

        #[test]
        fn grabbing_the_high_knob_by_its_outer_half_drags_it() {
            let mut app = TestAppContext::single();
            let (root, cx) = window(&mut app);
            let (x, y) = (knob_x(cx, &root, 249), mid_y(cx));
            drag(cx, (x + 5.0, y), &[(x - 30.0, y), (x - 60.0, y)]);
            assert!(wanted(cx, &root).hi < 249, "{:?}", wanted(cx, &root));
        }

        #[test]
        fn grabbing_a_knob_by_its_top_or_bottom_edge_drags_it() {
            let mut app = TestAppContext::single();
            let (root, cx) = window(&mut app);
            set(cx, &root, 60, 190);
            let b = slider(cx);
            let x = knob_x(cx, &root, 60);
            for y in [f32::from(b.origin.y) + 2.0, f32::from(b.origin.y) + 14.0] {
                set(cx, &root, 60, 190);
                drag(cx, (x, y), &[(x - 20.0, y), (x - 40.0, y)]);
                assert!(wanted(cx, &root).lo < 60, "y {y}: {:?}", wanted(cx, &root));
            }
        }

        /// Knobs a few pixels apart overlap: a press on the pair must drag
        /// whichever way the pointer then goes.
        #[test]
        fn overlapping_knobs_follow_the_drag_direction() {
            let mut app = TestAppContext::single();
            let (root, cx) = window(&mut app);
            let y = mid_y(cx);
            // Pressed right of both centres, dragged left: the low knob.
            set(cx, &root, 100, 103);
            let x = knob_x(cx, &root, 103) + 2.0;
            drag(cx, (x, y), &[(x - 20.0, y), (x - 40.0, y)]);
            let r = wanted(cx, &root);
            assert!(r.lo < 100, "dragging left off the pair moves the low knob: {r:?}");
            // Pressed left of both, dragged right: the high knob.
            set(cx, &root, 100, 103);
            let x = knob_x(cx, &root, 100) - 2.0;
            drag(cx, (x, y), &[(x + 20.0, y), (x + 40.0, y)]);
            let r = wanted(cx, &root);
            assert!(r.hi > 103, "dragging right off the pair moves the high knob: {r:?}");
        }

        /// Fast drags leave the card between two events: over the map, and
        /// past the window's edge (macOS keeps delivering a drag there).
        #[test]
        fn a_drag_past_the_card_and_out_of_the_window_reaches_the_end() {
            let mut app = TestAppContext::single();
            let (root, cx) = window(&mut app);
            let y = mid_y(cx);
            set(cx, &root, 120, 200);
            let x = knob_x(cx, &root, 120);
            drag(cx, (x, y), &[(x - 30.0, y), (-40.0, y)]);
            assert_eq!(wanted(cx, &root).lo, 0, "released left of the window");
            set(cx, &root, 120, 200);
            drag(cx, (x, y), &[(x - 30.0, y), (4.0, y + 300.0)]);
            assert_eq!(wanted(cx, &root).lo, 0, "released over the map");
            assert_eq!(MAP_PRESSES.with(Cell::get), 0);
        }

        #[test]
        fn a_release_anywhere_ends_the_drag() {
            let mut app = TestAppContext::single();
            let (root, cx) = window(&mut app);
            let y = mid_y(cx);
            set(cx, &root, 120, 200);
            let x = knob_x(cx, &root, 120);
            drag(cx, (x, y), &[(x - 30.0, y), (-40.0, y)]);
            let r = wanted(cx, &root);
            cx.simulate_mouse_move(at(x + 50.0, y), None, Modifiers::default());
            draw(cx);
            assert_eq!(wanted(cx, &root), r, "a hover after the release moves nothing");
            assert_eq!(root.read_with(cx, |r, _| r.dragging), None);
        }

        /// The band between the knobs drags as a whole, through the
        /// window-wide listeners, past the window's edge to the end.
        #[test]
        fn a_press_between_the_knobs_drags_the_whole_band() {
            let mut app = TestAppContext::single();
            let (root, cx) = window(&mut app);
            let y = mid_y(cx);
            set(cx, &root, 80, 140);
            let x = (knob_x(cx, &root, 80) + knob_x(cx, &root, 140)) / 2.0;
            drag(cx, (x, y), &[(x + 20.0, y), (x + 40.0, y)]);
            let r = wanted(cx, &root);
            assert!(r.lo > 80 && r.hi - r.lo == 60, "{r:?}");
            set(cx, &root, 80, 140);
            drag(cx, (x, y), &[(x - 30.0, y + 200.0), (-40.0, y)]);
            assert_eq!(wanted(cx, &root), IncomeRange { lo: 0, hi: 60 });
            assert_eq!(root.read_with(cx, |r, _| r.dragging), None);
            assert_eq!(MAP_PRESSES.with(Cell::get), 0);
        }

        #[test]
        fn a_drag_while_a_paint_update_is_in_flight_still_moves_the_knob() {
            let mut app = TestAppContext::single();
            let (root, cx) = window(&mut app);
            let y = mid_y(cx);
            set(cx, &root, 120, 200);
            root.update(cx, |r, _| r.retry_scheduled = true);
            let x = knob_x(cx, &root, 120);
            drag(cx, (x, y), &[(x - 30.0, y), (x - 60.0, y)]);
            assert!(wanted(cx, &root).lo < 100, "{:?}", wanted(cx, &root));
        }
    }

    /// A histogram brush is a slider range, at least one stop wide, in
    /// either direction; a bar's preview covers its incomes.
    #[test]
    fn a_histogram_brush_is_a_slider_range() {
        use gpui_maps::census::ramp::IncomeRange;
        assert_eq!(super::brush_range(40, 10), IncomeRange { lo: 10, hi: 40 });
        assert_eq!(super::brush_range(10, 10), IncomeRange { lo: 10, hi: 11 });
        let last = gpui_maps::census::ramp::slider_stops().len() - 1;
        assert_eq!(super::brush_range(last, last), IncomeRange { lo: last - 1, hi: last });
        let r = super::bar_range(gpui_maps::census::hist::income_bin(100_000.0));
        let stops = gpui_maps::census::ramp::slider_stops();
        assert!(stops[r.lo] <= 100_000.0 + 1.0 && stops[r.hi] >= 100_000.0 - 1.0, "{r:?}");
    }
}

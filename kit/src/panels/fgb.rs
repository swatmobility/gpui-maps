//! FlatGeobuf as an overlay source: the bundled world cities
//! (`tests/fixtures/fgb/world_cities.fgb`, 46 points, read from bytes)
//! coloured by a bool column and sized by a number column, and the US
//! counties streamed by viewport from the FlatGeobuf project's own demo
//! file (3,221 polygons, 14 MB, over HTTP range requests). Shared by
//! `demos/fgb.rs` (windowed, optionally on any path or URL) and the
//! gallery (`flatgeobuf`).
//!
//! The counties file is the reference implementation's test data
//! (BSD-2-Clause, served with CORS and `Range` from its GitHub Pages
//! site) and is in NAD83 (EPSG:4269), a geographic datum within a couple of
//! metres of WGS84 -- so it loads with `FgbOptions::accept_any_crs`, and
//! the card shows the warning that produces. Over 8 MiB, it streams: the
//! card follows the loaded count as the camera settles on new views.
//!
//! `GPUI_MAP_FGB` (a path or an http(s) URL) loads that file instead, in
//! one plain colour.
//!
//! `#![allow(dead_code)]`: included by two crates that each use a
//! different half of it.
#![allow(dead_code)]

use gpui::{App, AppContext as _, Context, Entity, IntoElement, Render, SharedString, WeakEntity, Window};
use gpui_maps::map_view::{FgbInput, MapView};
use gpui_maps::overlay::fgb::{FgbOptions, FgbReport};
use gpui_maps::overlay::FeaturePaint;
use gpui_maps::style::Rgba;

use crate::kit;
use crate::shell::{FocusPolicy, FocusedMap};

pub const ID: &str = "flatgeobuf";
pub const CITIES: &str = "fgb-cities";
pub const COUNTIES: &str = "fgb-counties";
/// Over the US north-east at z5.5: a few hundred of the counties in view.
pub const START: (f64, f64, f64) = (-78.0, 40.5, 5.5);

/// The bundled sample: the cities CSV written as FlatGeobuf (see the
/// fixture's licence note).
pub const CITIES_FGB: &[u8] = include_bytes!("../../../tests/fixtures/fgb/world_cities.fgb");

/// The FlatGeobuf project's demo file: US counties, NAD83, packed index.
pub const COUNTIES_URL: &str = "https://flatgeobuf.org/test/data/UScounties.fgb";

/// Network: the basemap and the counties.
pub fn requirements(_env: &kit::PanelEnv) -> Vec<kit::Requirement> {
    vec![kit::Requirement::Network]
}

pub const CREDIT_CITIES: kit::Credit = kit::Credit {
    what: "Cities",
    holder: "Natural Earth (naturalearthdata.com), populated places",
    dataset: "46 world cities from Natural Earth 1:10m populated places v5.1.2, written as FlatGeobuf from tests/fixtures/world_cities.csv",
    licence: kit::Licence::PublicDomain,
    url: "https://www.naturalearthdata.com/downloads/10m-cultural-vectors/10m-populated-places/",
    changes: "46 cities kept; positions rounded to 0.01 degree",
};

pub const CREDIT_COUNTIES: kit::Credit = kit::Credit {
    what: "US counties",
    holder: "FlatGeobuf project (Björn Harrtell and contributors)",
    dataset: "UScounties.fgb, the reference implementation's test data",
    licence: kit::Licence::Bsd2,
    url: "https://flatgeobuf.org/test/data/UScounties.fgb",
    changes: "streamed by viewport; drawn as WGS84 (the file is NAD83)",
};

pub const CREDITS: &[kit::Credit] = &[CREDIT_CITIES, CREDIT_COUNTIES, kit::CREDIT_OSM_OPENFREEMAP];

/// A path or an http(s) URL as an input.
pub fn input_for(location: &str) -> FgbInput {
    if location.starts_with("https://") || location.starts_with("http://") {
        FgbInput::Url(location.to_string())
    } else {
        FgbInput::Path(location.into())
    }
}

/// `GPUI_MAP_FGB`, when set.
pub fn input_from_env() -> Option<FgbInput> {
    let location = std::env::var("GPUI_MAP_FGB").ok().filter(|s| !s.trim().is_empty())?;
    Some(input_for(&location))
}

/// Capitals orange, other cities blue (a `case` on the bool column),
/// radius by population (an `interpolate` on the Long column).
pub fn cities_paint() -> serde_json::Map<String, serde_json::Value> {
    let json = serde_json::json!({
        "circle-color": ["case", ["==", ["get", "capital"], true], "#e4572e", "#2e86ab"],
        "circle-radius": ["interpolate", ["linear"], ["get", "population"], 100_000, 4, 40_000_000, 14],
    });
    json.as_object().expect("an object").clone()
}

fn counties_paint() -> FeaturePaint {
    FeaturePaint {
        fill_color: Rgba { r: 0.33, g: 0.42, b: 0.78, a: 0.22 },
        line_color: Rgba { r: 0.2, g: 0.27, b: 0.6, a: 0.9 },
        ..FeaturePaint::default()
    }
}

// ---------------------------------------------------------------------------
// The counties' colours: map colouring by default, one colour per state on
// the card's toggle. The file carries no number to choropleth (name, state,
// FIPS), and one flat colour hid every county line.
// ---------------------------------------------------------------------------

/// Soft qualitative colours for the counties (ColorBrewer Set3's first
/// six): only there to tell neighbours apart.
pub const COUNTY_PALETTE: [&str; 6] = ["#8dd3c7", "#ffffb3", "#bebada", "#fb8072", "#80b1d3", "#fdb462"];
/// One hue per state (ColorBrewer Set2's first six), states graph-coloured
/// so neighbouring states differ too.
pub const STATE_PALETTE: [&str; 6] = ["#66c2a5", "#fc8d62", "#8da0cb", "#e78ac3", "#a6d854", "#ffd92f"];
/// Kept under every colour: the fills are 0.7 opaque with a white edge.
pub const COUNTY_FILL_OPACITY: f64 = 0.7;
pub const COUNTY_EDGE: &str = "rgba(255, 255, 255, 0.9)";

/// How the counties are coloured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CountyColouring {
    /// Map colouring: no two neighbouring counties share a colour.
    Neighbours,
    /// One colour per state, neighbouring states apart.
    ByState,
}

impl CountyColouring {
    /// The legend card's note.
    pub fn note(self) -> &'static str {
        match self {
            CountyColouring::Neighbours => "Colours only tell neighbours apart",
            CountyColouring::ByState => "One colour per state",
        }
    }
}

/// Each county's colour index both ways, keyed by the file's own `FIPS`
/// and `STATE` values (what the paint's `match` reads).
#[derive(Clone, Debug)]
pub struct CountyColours {
    /// `(FIPS value, colour index)`, one per county with a FIPS.
    pub county: Vec<(serde_json::Value, u8)>,
    /// `(STATE value, colour index)`, one per state.
    pub state: Vec<(serde_json::Value, u8)>,
    /// The counties' neighbour lists (for the test and the log).
    pub adjacency: Vec<Vec<u32>>,
    pub county_colours: Vec<u8>,
    pub state_colours: Vec<u8>,
    pub state_of: Vec<u32>,
}

/// The whole counties file, cached once for the colouring (the map itself
/// streams it by viewport): `~/.cache/gpui-map-data/fgb/UScounties.fgb`.
fn counties_cache() -> Option<std::path::PathBuf> {
    Some(std::path::PathBuf::from(std::env::var_os("HOME")?).join(".cache/gpui-map-data/fgb/UScounties.fgb"))
}

/// The counties file's bytes: the cache, or one download into it (the
/// project User-Agent, nothing else sent).
pub async fn counties_bytes() -> anyhow::Result<Vec<u8>> {
    let path = counties_cache().ok_or_else(|| anyhow::anyhow!("no $HOME to cache the counties under"))?;
    if let Ok(bytes) = std::fs::read(&path) {
        return Ok(bytes);
    }
    let client = reqwest::Client::builder().user_agent(gpui_maps::tile_source::http::TILE_USER_AGENT).build()?;
    let bytes = client.get(COUNTIES_URL).send().await?.error_for_status()?.bytes().await?.to_vec();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("part-{}", std::process::id()));
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, &path)?;
    Ok(bytes)
}

/// A column's value at row `i`, as the paint's `match` compares it.
fn column_value(fgb: &gpui_maps::overlay::fgb::FgbSource, name: &str, i: usize) -> Option<serde_json::Value> {
    use gpui_maps::overlay::csv::CsvValues;
    match fgb.column(name)? {
        CsvValues::Str(s) => s.get(i).map(|v| serde_json::Value::from(v.to_string())),
        CsvValues::Number(n) => n.get(i).copied().flatten().and_then(serde_json::Number::from_f64).map(serde_json::Value::Number),
        CsvValues::Bool(_) => None,
    }
}

/// Reads the counties and colours them (blocking: a whole-file read and the
/// adjacency pass, a fraction of a second; run it off the UI thread).
pub fn county_colours(bytes: &[u8]) -> anyhow::Result<CountyColours> {
    let fgb = gpui_maps::overlay::fgb::FgbSource::from_bytes(bytes, &FgbOptions { accept_any_crs: true, ..FgbOptions::default() })?;
    let polygons: Vec<crate::map_colouring::Polygons> = (0..fgb.len())
        .map(|i| match fgb.geometry(i) {
            Some(gpui_maps::overlay::OverlayGeometry::Polygon(p)) => p,
            _ => Vec::new(),
        })
        .collect();
    let adjacency = crate::map_colouring::adjacency(&polygons);
    let county_colours = crate::map_colouring::colour(&adjacency);
    let mut states: Vec<serde_json::Value> = Vec::new();
    let mut state_of = Vec::with_capacity(fgb.len());
    for i in 0..fgb.len() {
        let s = column_value(&fgb, "STATE", i).unwrap_or(serde_json::Value::Null);
        let g = states.iter().position(|v| *v == s).unwrap_or_else(|| {
            states.push(s.clone());
            states.len() - 1
        });
        state_of.push(g as u32);
    }
    let state_graph = crate::map_colouring::group_adjacency(&adjacency, &state_of, states.len());
    let state_colours = crate::map_colouring::colour(&state_graph);
    let county = (0..fgb.len()).filter_map(|i| Some((column_value(&fgb, "FIPS", i)?, county_colours[i]))).collect();
    let state = states.into_iter().zip(state_colours.iter().copied()).filter(|(v, _)| !v.is_null()).collect();
    Ok(CountyColours { county, state, adjacency, county_colours, state_colours, state_of })
}

/// The counties' declared paint for `mode`: a `match` on `FIPS` (or
/// `STATE`) into the palette, 0.7 opaque, a white edge. A paint update
/// (`MapView::update_source_paint`), never a re-push.
pub fn county_paint(colours: &CountyColours, mode: CountyColouring) -> serde_json::Map<String, serde_json::Value> {
    use serde_json::{json, Value};
    let (key, rows, palette): (&str, &[(Value, u8)], &[&str; 6]) = match mode {
        CountyColouring::Neighbours => ("FIPS", &colours.county, &COUNTY_PALETTE),
        CountyColouring::ByState => ("STATE", &colours.state, &STATE_PALETTE),
    };
    let mut expr = vec![json!("match"), json!(["get", key])];
    for (c, colour) in palette.iter().enumerate() {
        // An index past the palette would wrap onto a neighbour's colour;
        // it falls to the default instead (never seen on this file: the
        // test checks the counties fit in six).
        let labels: Vec<Value> = rows.iter().filter(|(_, i)| usize::from(*i) == c).map(|(v, _)| v.clone()).collect();
        if !labels.is_empty() {
            expr.push(Value::Array(labels));
            expr.push(json!(colour));
        }
    }
    expr.push(json!("#cccccc"));
    let paint = json!({"fill-color": expr, "fill-opacity": COUNTY_FILL_OPACITY, "fill-outline-color": COUNTY_EDGE});
    paint.as_object().expect("an object").clone()
}

fn plain_paint() -> FeaturePaint {
    FeaturePaint {
        circle_radius: 2.5,
        circle_color: Rgba { r: 0.18, g: 0.53, b: 0.67, a: 0.9 },
        fill_color: Rgba { r: 0.18, g: 0.53, b: 0.67, a: 0.3 },
        line_color: Rgba { r: 0.1, g: 0.35, b: 0.45, a: 0.9 },
        ..FeaturePaint::default()
    }
}

/// One source's card line.
struct Line {
    source: &'static str,
    label: SharedString,
    status: SharedString,
    report: Option<FgbReport>,
}

pub fn build(env: &kit::PanelEnv, window: &mut Window, cx: &mut App) -> anyhow::Result<kit::Built> {
    build_with(env, input_from_env(), window, cx)
}

/// The contiguous United States, roughly -- deliberately loose (it also
/// covers a sliver of Mexico and the Maritimes) since this is a fallback
/// framing, not a precise one. [`COUNTIES_URL`]'s own header states an
/// envelope of roughly -179.15 .. 179.78: not a real 359-degree span, but
/// the Aleutian Islands (part of Alaska, which this file also carries)
/// crossing the antimeridian, which `kit::coverage_from_bounds` correctly
/// refuses to frame (see its own doc comment). The counties the default
/// panel actually draws are overwhelmingly in the contiguous 48, so THAT
/// is the sensible default to fall back to -- not [`START`]'s single fixed
/// point, which only ever showed the eastern half of them on screen.
const CONTIGUOUS_US: [f64; 4] = [-125.0, 24.0, -66.0, 50.0];

/// Where to open the camera: the primary dataset's own header bbox,
/// framed to `(w, h)` the way the MBTiles/PMTiles panels frame an
/// archive's `coverage()`.
///
/// One blocking range request for `input`'s header (`FgbRemote::open`
/// fetches 16 KiB at most, never the body); a local path blocks on a
/// bounded read of that one file. With no `input`, the US counties file
/// (`COUNTIES_URL`) is always what the default panel draws, so its
/// envelope -- not the bundled cities' -- is what frames the camera: the
/// counties are the dataset that was landing off-screen.
fn opening_camera(input: Option<&FgbInput>, w: f64, h: f64) -> (f64, f64, f64) {
    // The SAME options `build_with` loads this same input with below: a
    // custom `input` gets plain `FgbOptions::default()` there (an
    // unexpected CRS is a load error the card reports, not something this
    // peek should paper over by reading it anyway), while the default
    // counties file is always loaded with `accept_any_crs` (it is stated
    // NAD83, not WGS84, and the panel accepts that). Mismatched options
    // here would park the camera on a header the real load then refuses.
    let counties_options = FgbOptions { accept_any_crs: true, ..FgbOptions::default() };
    let custom_options = FgbOptions::default();
    let envelope = match input {
        Some(FgbInput::Url(url)) => gpui_maps::overlay::fgb::FgbRemote::open(url, &custom_options).ok().and_then(|r| r.header().envelope),
        Some(FgbInput::Path(path)) => {
            gpui_maps::overlay::fgb::FgbSource::from_path(path, &custom_options).ok().and_then(|s| s.header().envelope)
        }
        Some(FgbInput::Bytes(bytes)) => {
            gpui_maps::overlay::fgb::FgbSource::from_bytes(bytes, &custom_options).ok().and_then(|s| s.header().envelope)
        }
        None => gpui_maps::overlay::fgb::FgbRemote::open(COUNTIES_URL, &counties_options).ok().and_then(|r| r.header().envelope),
    };
    // No `input` (the default counties+cities panel) falls back to a
    // fixed box framing where its primary dataset actually lives
    // (`CONTIGUOUS_US`) rather than [`START`]'s single point -- the
    // counties' own stated envelope is unusable (the antimeridian
    // crossing above), but "mostly CONUS" is still true of the file and
    // worth falling back to on purpose, not by accident.
    let fallback = if input.is_none() { kit::coverage_from_bounds(CONTIGUOUS_US[0], CONTIGUOUS_US[1], CONTIGUOUS_US[2], CONTIGUOUS_US[3]) } else { None };
    match envelope.and_then(|[min_lon, min_lat, max_lon, max_lat]| kit::coverage_from_bounds(min_lon, min_lat, max_lon, max_lat)).or(fallback) {
        Some(coverage) => coverage.opening_camera(w, h),
        None => START,
    }
}

/// The page's basemap (keyless OpenFreeMap Positron): its Explorer sample
/// opens on the same one (`samples::tests::every_sample_opens_on_its_gallery_twins_basemap`).
pub const STYLE: &str = "demos/positron-style.json";

/// `input` `None` draws the bundled cities and the remote counties.
pub fn build_with(env: &kit::PanelEnv, input: Option<FgbInput>, window: &mut Window, cx: &mut App) -> anyhow::Result<kit::Built> {
    // Positron; `GPUI_MAP_FGB_BASEMAP=dark` puts the counties on
    // OpenFreeMap Dark (a capture switch for the labels' dark-ground form).
    let style_file = if std::env::var("GPUI_MAP_FGB_BASEMAP").as_deref() == Ok("dark") { "demos/openfreemap-dark-style.json" } else { STYLE };
    let style = kit::compile_style(style_file)?;
    let source = kit::openfreemap_planet();
    let (w, h) = env.viewport;
    let host = env.host.clone();
    let runtime = env.runtime.clone();
    let start = opening_camera(input.as_ref(), w, h);
    let map = cx.new(|_| {
        let mut view = MapView::new(start.0, start.1, start.2, w, h, style, source, runtime);
        if let Some(title) = host.title() {
            view.set_window_title_prefix(title.to_string());
        }
        // Always, not only in the gallery: the card is a gpui element over
        // the map, and without this the map's Metal layer composites above
        // it -- a standalone window would show the map and no card.
        view.set_chrome_above_map(true);
        view
    });
    // Hover a city or a county for its card; a file of your own gets the
    // default property table.
    let config = if input.is_none() {
        crate::popup::PopupConfig::new().source(CITIES, crate::popup::world_city_card()).custom(COUNTIES, |pick| county_card(pick.properties.as_deref()))
    } else {
        crate::popup::PopupConfig::new()
    };
    let popup = crate::popup::install(&map, config, window, cx);
    let bundled = input.is_none();
    let mut loads = Vec::new();
    match input {
        Some(input) => {
            let label: SharedString = match &input {
                FgbInput::Url(url) => url.clone().into(),
                FgbInput::Path(path) => path.display().to_string().into(),
                FgbInput::Bytes(_) => "bytes".into(),
            };
            let task = map.update(cx, |view, cx| view.load_source_fgb(CITIES, input, FgbOptions::default(), plain_paint(), cx));
            loads.push((CITIES, label, task));
        }
        None => {
            map.update(cx, |view, _| view.declare_source_paint(CITIES, &cities_paint())).map_err(|e| anyhow::anyhow!("{e:?}"))?;
            map.update(cx, |view, _| {
                view.set_source_credit(CITIES, Some("Cities: gpui-maps sample"));
                view.set_source_credit(COUNTIES, Some("US counties: FlatGeobuf project (BSD-2-Clause)"));
            });
            let options = FgbOptions { accept_any_crs: true, ..FgbOptions::default() };
            let counties = map.update(cx, |view, cx| {
                view.load_source_fgb(COUNTIES, FgbInput::Url(COUNTIES_URL.into()), options, counties_paint(), cx)
            });
            loads.push((COUNTIES, "US counties (flatgeobuf.org, by range request)".into(), counties));
            let cities = map.update(cx, |view, cx| {
                view.load_source_fgb(CITIES, FgbInput::Bytes(CITIES_FGB.to_vec()), FgbOptions::default(), FeaturePaint::default(), cx)
            });
            loads.push((CITIES, "world_cities.fgb (bundled)".into(), cities));
        }
    }
    // The bundled cities' population, linked both ways, docked beside the
    // map (the shared `linked_chart` component).
    let chart = bundled.then(|| crate::linked_chart::city_population_chart(&map, CITIES, &popup, cx));
    // The counties fill the country: basemap labels over them.
    let labels = bundled.then(|| crate::data_labels::DataLabels::attach(&map, crate::data_labels::document(style_file).ok(), true, cx));
    let runtime = env.runtime.clone();
    let focus = if env.host.is_gallery() { FocusPolicy::Once } else { FocusPolicy::EveryFrame };
    let inner = cx.new(|_| FocusedMap::new(map.clone(), focus));
    let weak_map = map.downgrade();
    let root = cx.new(|cx: &mut Context<FgbRoot>| {
        let mut lines = Vec::new();
        for (source, label, task) in loads {
            lines.push(Line { source, label, status: "loading...".into(), report: None });
            cx.spawn(async move |this, cx| {
                let started = std::time::Instant::now();
                let result = task.await;
                let ms = started.elapsed().as_secs_f64() * 1e3;
                match &result {
                    Ok(report) => eprintln!("flatgeobuf {source}: {report} ({ms:.0} ms, {} requests, {} bytes)", report.requests, report.bytes_read),
                    Err(e) => eprintln!("flatgeobuf {source}: {e}"),
                }
                let _ = this.update(cx, |root, cx| {
                    let landed = result.is_ok();
                    if let Some(line) = root.lines.iter_mut().find(|l| l.source == source) {
                        match result {
                            Ok(report) => {
                                line.status = format!("{report} in {ms:.0} ms").into();
                                line.report = Some(report);
                            }
                            Err(e) => line.status = format!("failed: {e}").into(),
                        }
                    }
                    // The colours can be ready before the streamed counties
                    // land (the whole file is cached, the stream is not):
                    // their paint update was then refused, so apply it now.
                    if landed && source == COUNTIES && root.colours.is_some() {
                        root.set_colouring(root.colouring, cx);
                    }
                    cx.notify();
                });
            })
            .detach();
        }
        // The counties' colours: the whole file read once (cached) and
        // graph-coloured off the UI thread, then a paint update -- the
        // streamed geometry is never pushed again.
        if bundled {
            let mode = if std::env::var("GPUI_MAP_FGB_COLOURING").as_deref() == Ok("state") { CountyColouring::ByState } else { CountyColouring::Neighbours };
            let task = runtime.spawn(async move {
                let bytes = counties_bytes().await?;
                tokio::task::spawn_blocking(move || {
                    let started = std::time::Instant::now();
                    county_colours(&bytes).map(|c| (c, started.elapsed()))
                })
                .await?
            });
            cx.spawn(async move |this, cx| {
                let result = task.await.map_err(|e| anyhow::anyhow!("{e}")).and_then(|r| r);
                let _ = this.update(cx, |root: &mut FgbRoot, cx| {
                    match result {
                        Ok((colours, took)) => {
                            let used = |c: &[u8]| c.iter().max().map_or(0, |m| usize::from(*m) + 1);
                            eprintln!(
                                "flatgeobuf counties: {} counties, {} neighbour pairs, {} colours; {} states, {} colours; coloured in {:.0} ms",
                                colours.county.len(),
                                colours.adjacency.iter().map(Vec::len).sum::<usize>() / 2,
                                used(&colours.county_colours),
                                colours.state.len(),
                                used(&colours.state_colours),
                                took.as_secs_f64() * 1e3
                            );
                            root.colours = Some(std::sync::Arc::new(colours));
                            root.set_colouring(mode, cx);
                        }
                        Err(e) => eprintln!("flatgeobuf counties: not coloured ({e:#}); one flat colour"),
                    }
                });
            })
            .detach();
        }
        // A streamed source's loaded count moves as the camera settles:
        // the card re-reads it twice a second.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(std::time::Duration::from_millis(500)).await;
            if this.update(cx, |_, cx| cx.notify()).is_err() {
                break;
            }
        })
        .detach();
        FgbRoot { inner, popup, chart, map: weak_map, lines, colours: None, colouring: CountyColouring::Neighbours, _labels: labels }
    });
    Ok(kit::Built { view: root.into(), map: Some(map.downgrade()), start })
}

/// The map and a small card: per source, its features, what loaded, and
/// its extent.
pub struct FgbRoot {
    inner: Entity<FocusedMap>,
    popup: Entity<crate::popup::MapPopup>,
    /// The cities' population chart (the bundled data only).
    pub chart: Option<Entity<crate::linked_chart::LinkedChart>>,
    map: WeakEntity<MapView>,
    lines: Vec<Line>,
    /// The counties' colour indices, once computed (the default panel only).
    colours: Option<std::sync::Arc<CountyColours>>,
    colouring: CountyColouring,
    /// Basemap labels over the counties (`crate::data_labels`).
    _labels: Option<crate::data_labels::DataLabels>,
}

impl FgbRoot {
    /// Each source's status line: "loading..." until its load lands.
    pub fn statuses(&self) -> Vec<String> {
        self.lines.iter().map(|l| l.status.to_string()).collect()
    }

    /// Recolours the counties: a paint update on the streamed source, no
    /// re-push. A no-op until the colours are computed.
    pub fn set_colouring(&mut self, mode: CountyColouring, cx: &mut Context<Self>) {
        self.colouring = mode;
        let (Some(colours), Some(map)) = (self.colours.clone(), self.map.upgrade()) else { return };
        let paint = county_paint(&colours, mode);
        map.update(cx, |m, cx| {
            if let Err(e) = m.update_source_paint(COUNTIES, &paint, cx) {
                eprintln!("flatgeobuf counties: paint refused: {e:?}");
            }
            cx.notify();
        });
        cx.notify();
    }
}

/// A US county (flatgeobuf.org's `UScounties.fgb`): "Kings County",
/// its state, and its FIPS code.
pub fn county_card(props: Option<&gpui_maps::overlay::FeatureProperties>) -> Option<crate::popup::Card> {
    let props = props?;
    let text = |k: &str| match props.get(k) {
        Some(gpui_maps::overlay::TagValue::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Some(gpui_maps::overlay::TagValue::Number(n)) => Some(format!("{n}")),
        _ => None,
    };
    let name = text("NAME")?;
    let title = match text("LSAD") {
        Some(kind) if !name.ends_with(&kind) && kind.chars().any(char::is_alphabetic) => format!("{name} {kind}"),
        _ => name,
    };
    let mut rows = Vec::new();
    if let Some(state) = text("STATE") {
        rows.push(("State".to_string(), state));
    }
    if let Some(fips) = text("FIPS") {
        rows.push(("FIPS".to_string(), fips));
    }
    Some(crate::popup::Card { title, subtitle: Some("US county".into()), rows, ..Default::default() })
}

fn bbox_text(b: [f64; 4]) -> String {
    format!("bbox {:.2}, {:.2} .. {:.2}, {:.2}", b[0], b[1], b[2], b[3])
}

impl Render for FgbRoot {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui::{InteractiveElement as _, ParentElement as _, Styled as _};
        let map = self.map.upgrade();
        let mut card = gpui::div()
            .absolute()
            .top(gpui::px(12.0))
            .left(gpui::px(12.0))
            .max_w(gpui::px(440.0))
            .px(gpui::px(10.0))
            .py(gpui::px(6.0))
            .rounded(gpui::px(6.0))
            .bg(gpui::rgba(0xffffffcc))
            .text_color(gpui::rgb(0x1f2933))
            .text_xs()
            .flex()
            .flex_col()
            .gap(gpui::px(2.0))
            // The card swallows the mouse: a drag on it must not also pan
            // the map beneath (see `MapView::set_chrome_above_map`).
            .id("fgb-card")
            .occlude();
        for line in &self.lines {
            card = card.child(gpui::div().font_weight(gpui::FontWeight::SEMIBOLD).child(format!("FlatGeobuf: {}", line.label)));
            let Some(report) = &line.report else {
                card = card.child(gpui::div().child(line.status.clone()));
                continue;
            };
            let stream = map.as_ref().and_then(|m| m.read(cx).fgb_stream_stats(line.source));
            let counts = match stream {
                Some(s) => format!(
                    "{} features | {} loaded, streaming ({} queries, {} requests, {:.1} of {:.1} MB)",
                    s.features_total,
                    s.features_loaded,
                    s.queries,
                    s.requests,
                    s.bytes_read as f64 / 1e6,
                    s.file_bytes as f64 / 1e6
                ),
                None => format!("{} features | {} loaded", report.features_total, report.features_loaded),
            };
            card = card.child(gpui::div().child(counts));
            if let Some(b) = report.bbox {
                card = card.child(gpui::div().child(format!("{} | CRS {}", bbox_text(b), report.crs)));
            }
            for warning in &report.warnings {
                card = card.child(gpui::div().text_color(gpui::rgb(0x8a5a00)).child(warning.clone()));
            }
        }
        // The counties' colouring: what the colours mean, and the toggle.
        if self.colours.is_some() {
            use gpui::StatefulInteractiveElement as _;
            let mode = self.colouring;
            let (next, label) = match mode {
                CountyColouring::Neighbours => (CountyColouring::ByState, "By state"),
                CountyColouring::ByState => (CountyColouring::Neighbours, "By neighbours"),
            };
            card = card.child(
                gpui::div()
                    .flex()
                    .items_center()
                    .gap(gpui::px(8.0))
                    .child(gpui::div().child(format!("Counties: {}", mode.note())))
                    .child(
                        gpui::div()
                            .id("fgb-colouring-toggle")
                            .debug_selector(|| "fgb-colouring-toggle".to_string())
                            .px(gpui::px(6.0))
                            .rounded(gpui::px(4.0))
                            .border_1()
                            .border_color(gpui::rgba(0x1f293340))
                            .cursor_pointer()
                            .child(label)
                            .on_click(cx.listener(move |root: &mut FgbRoot, _, _, cx| root.set_colouring(next, cx))),
                    ),
            );
        }
        let map_area = gpui::div()
            .relative()
            .size_full()
            .child(self.inner.clone())
            .child(crate::popup::input_layer(&self.popup, cx))
            .child(card)
            .child(self.popup.clone());
        match &self.chart {
            Some(chart) => crate::linked_chart::over_map(map_area, vec![gpui::IntoElement::into_any_element(chart.clone())]),
            None => map_area,
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_bundled_cities_read_with_typed_columns() {
        let fgb = gpui_maps::overlay::fgb::FgbSource::from_bytes(super::CITIES_FGB, &Default::default()).unwrap();
        assert_eq!(fgb.len(), 46);
        assert_eq!(fgb.report().features_total, 46);
        assert!(fgb.report().warnings.is_empty(), "{}", fgb.report());
        use gpui_maps::overlay::fgb::FgbColumnType::{Bool, Long, String};
        let types: Vec<_> = fgb.report().columns.iter().map(|(n, t)| (n.as_str(), *t)).collect();
        assert_eq!(types, [("name", String), ("country", String), ("population", Long), ("capital", Bool)]);
    }

    /// The real counties file, when cached (`counties_bytes` caches it on
    /// the page's first open; skipped otherwise -- no network in a test):
    /// no two neighbouring counties, and no two neighbouring states, share
    /// a colour, both fit the six-colour palettes, and both paints compile.
    #[test]
    fn no_two_neighbouring_counties_share_a_colour() {
        let Some(path) = super::counties_cache() else { return };
        let Ok(bytes) = std::fs::read(&path) else {
            eprintln!("skipped: {} is not cached", path.display());
            return;
        };
        let c = super::county_colours(&bytes).unwrap();
        assert!(c.county.len() > 3000, "{} counties", c.county.len());
        let pairs: usize = c.adjacency.iter().map(Vec::len).sum::<usize>() / 2;
        assert!(pairs > 8000, "a real adjacency graph, not an empty one: {pairs} pairs");
        assert!(crate::map_colouring::is_proper(&c.adjacency, &c.county_colours));
        assert!(c.county_colours.iter().all(|&i| usize::from(i) < super::COUNTY_PALETTE.len()));
        let states = crate::map_colouring::group_adjacency(&c.adjacency, &c.state_of, c.state_colours.len());
        assert!(crate::map_colouring::is_proper(&states, &c.state_colours));
        assert!(c.state_colours.iter().all(|&i| usize::from(i) < super::STATE_PALETTE.len()));
        for mode in [super::CountyColouring::Neighbours, super::CountyColouring::ByState] {
            let mut store = gpui_maps::overlay::OverlayStore::new();
            store.declare_source_paint(super::COUNTIES, &super::county_paint(&c, mode)).expect("compiles");
        }
    }

    #[test]
    fn the_cities_paint_compiles() {
        let mut store = gpui_maps::overlay::OverlayStore::new();
        store.declare_source_paint(super::CITIES, &super::cities_paint()).expect("compiles");
    }

    /// The panel in a test window, on a file input (no network): once the
    /// off-thread load lands the card leaves "loading..." and the map holds
    /// the source.
    #[test]
    fn the_card_leaves_loading_and_the_map_holds_the_source() {
        use gpui::TestAppContext;
        let dir = std::env::temp_dir().join(format!("gpui-maps-fgb-panel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cities.fgb");
        std::fs::write(&path, super::CITIES_FGB).unwrap();
        let rt = tokio::runtime::Runtime::new().expect("a runtime");
        let env = crate::kit::PanelEnv::from_process(crate::kit::Host::Gallery, rt.handle().clone(), (800.0, 600.0));
        let mut app = TestAppContext::single();
        let cx = app.add_empty_window();
        let input = Some(gpui_maps::map_view::FgbInput::Path(path.clone()));
        let built = cx.update(|window, cx| super::build_with(&env, input, window, cx)).expect("builds");
        let root = built.view.clone().downcast::<super::FgbRoot>().unwrap_or_else(|_| panic!("a FgbRoot"));
        let map = built.map.as_ref().and_then(|m| m.upgrade()).expect("a map");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            cx.run_until_parked();
            let statuses = cx.update(|_, cx| root.read(cx).statuses());
            if statuses.iter().all(|s| s != "loading...") {
                assert!(statuses[0].starts_with("loaded 46 of 46 features"), "{statuses:?}");
                break;
            }
            assert!(std::time::Instant::now() < deadline, "the card never left loading");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        cx.update(|_, cx| {
            let map = map.read(cx);
            assert!(!map.style().layers.is_empty(), "the map has its basemap style");
        });
        env.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A standalone window (`cargo run --example fgb`) draws the card
    /// above the map too: without `set_chrome_above_map(true)` the map's
    /// Metal layer composites over every gpui element and the card is hidden.
    #[test]
    fn a_standalone_window_puts_the_card_above_the_map() {
        use gpui::TestAppContext;
        let dir = std::env::temp_dir().join(format!("gpui-maps-fgb-standalone-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cities.fgb");
        std::fs::write(&path, super::CITIES_FGB).unwrap();
        let rt = tokio::runtime::Runtime::new().expect("a runtime");
        let host = crate::kit::Host::Standalone { title: "fgb".into() };
        let env = crate::kit::PanelEnv::from_process(host, rt.handle().clone(), (800.0, 600.0));
        let mut app = TestAppContext::single();
        let cx = app.add_empty_window();
        let built = cx.update(|window, cx| super::build_with(&env, Some(gpui_maps::map_view::FgbInput::Path(path)), window, cx)).expect("builds");
        let map = built.map.as_ref().and_then(|m| m.upgrade()).expect("a map");
        cx.update(|_, cx| assert!(map.read(cx).chrome_above_map(), "the standalone map hides the card"));
        env.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

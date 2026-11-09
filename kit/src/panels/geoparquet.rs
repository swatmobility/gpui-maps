//! GeoParquet as an overlay source: the bundled world cities
//! (`tests/fixtures/geoparquet/world_cities.parquet`, 46 points in 3 row
//! groups, read from bytes) coloured by a bool column and sized by a number
//! column, over the GeoParquet specification's own 1.1 example file (five
//! Natural Earth countries, WKB polygons with a bbox covering column).
//! Shared by `demos/geoparquet.rs` (windowed, optionally on any path or
//! URL) and the gallery (`geoparquet`).
//!
//! `GPUI_MAP_GEOPARQUET` (a path or an http(s) URL) loads that file instead,
//! streamed by viewport when it is large. `GPUI_MAP_GEOPARQUET=overture`
//! resolves the newest Overture Maps release (`stac.overturemaps.org`'s
//! `latest`, then the public S3 bucket's listing -- no credentials, `Range`
//! honoured) and streams the first file of its places theme (CDLA
//! Permissive 2.0; ~700 MB, zstd, 256 row groups) over Mexico City: only
//! the row groups in view are fetched. There is no fixed Overture URL to
//! bundle: each release replaces the file names and the bucket keeps only
//! the last two releases, which is why it is resolved and opt-in.
//!
//! `#![allow(dead_code)]`: included by two crates that each use a
//! different half of it.
#![allow(dead_code)]

use gpui::{App, AppContext as _, Context, Entity, IntoElement, Render, SharedString, WeakEntity, Window};
use gpui_maps::map_view::{GeoParquetInput, MapView};
use gpui_maps::overlay::geoparquet::{GeoParquetOptions, GeoParquetReport, GEOPARQUET_USER_AGENT};
use gpui_maps::overlay::FeaturePaint;
use gpui_maps::style::Rgba;

use crate::kit;
use crate::shell::{FocusPolicy, FocusedMap};

pub const ID: &str = "geoparquet";
pub const CITIES: &str = "gpq-cities";
pub const COUNTRIES: &str = "gpq-countries";
pub const FILE: &str = "gpq-file";
/// The Atlantic at z1.6: the five countries and most of the cities.
pub const START: (f64, f64, f64) = (-40.0, 25.0, 1.6);
/// Mexico City at z12, for the Overture places.
pub const OVERTURE_START: (f64, f64, f64) = (-99.15, 19.42, 12.0);

/// The bundled samples (see the fixtures' licence note).
pub const CITIES_PARQUET: &[u8] = include_bytes!("../../../tests/fixtures/geoparquet/world_cities.parquet");
pub const COUNTRIES_PARQUET: &[u8] = include_bytes!("../../../tests/fixtures/geoparquet/naturalearth_countries.parquet");

/// Overture's release catalogue and public bucket.
pub const OVERTURE_STAC: &str = "https://stac.overturemaps.org/catalog.json";
pub const OVERTURE_BUCKET: &str = "https://overturemaps-us-west-2.s3.us-west-2.amazonaws.com";

/// Network: the basemap (and Overture, when asked).
pub fn requirements(_env: &kit::PanelEnv) -> Vec<kit::Requirement> {
    vec![kit::Requirement::Network]
}

pub const CREDIT_CITIES: kit::Credit = kit::Credit {
    what: "Cities",
    holder: "Natural Earth (naturalearthdata.com), populated places",
    dataset: "46 world cities from Natural Earth 1:10m populated places v5.1.2, written as GeoParquet from tests/fixtures/world_cities.csv",
    licence: kit::Licence::PublicDomain,
    url: "https://www.naturalearthdata.com/downloads/10m-cultural-vectors/10m-populated-places/",
    changes: "46 cities kept; positions rounded to 0.01 degree",
};

pub const CREDIT_COUNTRIES: kit::Credit = kit::Credit {
    what: "Countries",
    holder: "Natural Earth; the GeoParquet specification's example file (OGC)",
    dataset: "examples/example.parquet at geoparquet v1.1.0: five 1:110m admin-0 countries",
    licence: kit::Licence::PublicDomain,
    url: "https://github.com/opengeospatial/geoparquet/tree/v1.1.0/examples",
    changes: "",
};

pub const CREDIT_OVERTURE: kit::Credit = kit::Credit {
    what: "Places (GPUI_MAP_GEOPARQUET=overture only)",
    holder: "Overture Maps Foundation",
    dataset: "Overture places theme, the newest release",
    licence: kit::Licence::CdlaPermissive2,
    url: "https://docs.overturemaps.org/attribution/",
    changes: "streamed by viewport; names.primary and confidence only",
};

pub const CREDITS: &[kit::Credit] = &[CREDIT_CITIES, CREDIT_COUNTRIES, CREDIT_OVERTURE, kit::CREDIT_OSM_OPENFREEMAP];

/// What `GPUI_MAP_GEOPARQUET` (or the demo's argument) asks for.
#[derive(Debug, Clone)]
pub enum Choice {
    File(GeoParquetInput),
    Overture,
}

/// A path, an http(s) URL, or `overture`.
pub fn choice_for(location: &str) -> Choice {
    if location.eq_ignore_ascii_case("overture") {
        Choice::Overture
    } else if location.starts_with("https://") || location.starts_with("http://") {
        Choice::File(GeoParquetInput::Url(location.to_string()))
    } else {
        Choice::File(GeoParquetInput::Path(location.into()))
    }
}

/// `GPUI_MAP_GEOPARQUET`, when set.
pub fn choice_from_env() -> Option<Choice> {
    let location = std::env::var("GPUI_MAP_GEOPARQUET").ok().filter(|s| !s.trim().is_empty())?;
    Some(choice_for(location.trim()))
}

/// The first places file of the newest Overture release: the STAC
/// catalogue's `latest`, then one S3 listing (no credentials).
pub async fn overture_places_url() -> Result<String, String> {
    let client = reqwest::Client::builder()
        .user_agent(GEOPARQUET_USER_AGENT)
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())?;
    let get = async |url: String| -> Result<String, String> {
        let response = client.get(&url).send().await.map_err(|e| e.without_url().to_string())?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status().as_u16()));
        }
        response.text().await.map_err(|e| e.without_url().to_string())
    };
    let catalog: serde_json::Value = serde_json::from_str(&get(OVERTURE_STAC.into()).await?).map_err(|e| e.to_string())?;
    let latest = catalog.get("latest").and_then(|v| v.as_str()).ok_or("no `latest` in the Overture catalogue")?.to_string();
    let prefix = format!("release/{latest}/theme=places/type=place/");
    let listing = get(format!("{OVERTURE_BUCKET}/?list-type=2&max-keys=5&prefix={prefix}")).await?;
    let key = listing
        .split("<Key>")
        .skip(1)
        .filter_map(|k| k.split("</Key>").next())
        .find(|k| k.ends_with(".parquet"))
        .ok_or("no places file in the listing")?;
    Ok(format!("{OVERTURE_BUCKET}/{key}"))
}

/// Capitals orange, other cities blue, radius by population.
pub fn cities_paint() -> serde_json::Map<String, serde_json::Value> {
    let json = serde_json::json!({
        "circle-color": ["case", ["==", ["get", "capital"], true], "#e4572e", "#2e86ab"],
        "circle-radius": ["interpolate", ["linear"], ["get", "population"], 100_000, 4, 40_000_000, 14],
    });
    json.as_object().expect("an object").clone()
}

/// Overture places: colour by confidence.
pub fn places_paint() -> serde_json::Map<String, serde_json::Value> {
    let json = serde_json::json!({
        "circle-color": ["interpolate", ["linear"], ["get", "confidence"], 0.3, "#c9d6df", 0.95, "#8e2c8f"],
        "circle-radius": 2.5,
    });
    json.as_object().expect("an object").clone()
}

fn countries_paint() -> FeaturePaint {
    FeaturePaint {
        fill_color: Rgba { r: 0.36, g: 0.6, b: 0.42, a: 0.28 },
        line_color: Rgba { r: 0.2, g: 0.42, b: 0.27, a: 0.9 },
        ..FeaturePaint::default()
    }
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
    report: Option<GeoParquetReport>,
}

type Load = gpui::Task<Result<GeoParquetReport, String>>;

pub fn build(env: &kit::PanelEnv, window: &mut Window, cx: &mut App) -> anyhow::Result<kit::Built> {
    build_with(env, choice_from_env(), window, cx)
}

/// Where to open the camera: the file's own `geo` metadata bbox, framed to
/// `(w, h)` the way the MBTiles/PMTiles panels frame an archive's
/// `coverage()`. [`OVERTURE_START`] is unchanged -- Overture's URL is
/// resolved asynchronously, after the window has already opened, so there
/// is no header to read yet. [`START`] is the fallback for everything else:
/// a metadata-less file, or a read that failed (the real load below
/// reports that failure on the card; the camera falls back silently since
/// it is cosmetic either way).
///
/// With no `choice`, the default panel draws BOTH bundled files, so the
/// camera frames their union -- same reasoning as the FlatGeobuf panel's
/// cities-and-counties default, EXCEPT that here the union is always
/// rejected and [`START`] (the Atlantic, z1.6) is the real answer: the
/// GeoParquet 1.1 spec's own example file states its `bbox` as the whole
/// world (`-180,-90..180,83.6`), and checked against its actual rows (`SELECT
/// name, ST_Envelope(geometry) ...`) its five countries are Fiji (literally
/// xmin -180/xmax 180, the spec's own antimeridian test case), Tanzania,
/// Western Sahara, Canada and the USA (Alaska takes this one past -171,
/// the same antimeridian issue as the FlatGeobuf counties) -- five points
/// scattered across every inhabited continent, not a `CONTIGUOUS_US`-style
/// single region this panel could usefully fall back to. The bundled
/// cities are similarly a deliberately even spread, Honolulu to Auckland.
/// A whole-world view is the correct "framed on its data" answer for a
/// panel whose default data IS the whole world.
///
/// Parses the bundled files a second time, synchronously -- already
/// -resident bytes, microseconds -- and blocks on one metadata-only read
/// for a custom path or URL ([`gpui_maps::overlay::geoparquet::GeoParquetRemote::open`]
/// fetches the footer, never the row groups).
fn opening_camera(choice: Option<&Choice>, w: f64, h: f64) -> (f64, f64, f64) {
    let options = GeoParquetOptions::default();
    let bbox_of_bytes = |bytes: &[u8]| gpui_maps::overlay::geoparquet::GeoParquetSource::from_bytes(bytes, &options).ok().and_then(|s| s.metadata().bbox);
    let bbox = match choice {
        Some(Choice::Overture) => None,
        Some(Choice::File(GeoParquetInput::Url(url))) => {
            gpui_maps::overlay::geoparquet::GeoParquetRemote::open(url, &options).ok().and_then(|r| r.metadata().bbox)
        }
        Some(Choice::File(GeoParquetInput::Path(path))) => {
            gpui_maps::overlay::geoparquet::GeoParquetSource::from_path(path, &options).ok().and_then(|s| s.metadata().bbox)
        }
        Some(Choice::File(GeoParquetInput::Bytes(bytes))) => bbox_of_bytes(bytes),
        None => kit::union_bbox(bbox_of_bytes(CITIES_PARQUET), bbox_of_bytes(COUNTRIES_PARQUET)),
    };
    match bbox.and_then(|[min_lon, min_lat, max_lon, max_lat]| kit::coverage_from_bounds(min_lon, min_lat, max_lon, max_lat)) {
        Some(coverage) => coverage.opening_camera(w, h),
        None if matches!(choice, Some(Choice::Overture)) => OVERTURE_START,
        None => START,
    }
}

/// The page's basemap (keyless OpenFreeMap Positron): its Explorer sample
/// opens on the same one (`samples::tests::every_sample_opens_on_its_gallery_twins_basemap`).
pub const STYLE: &str = "demos/positron-style.json";

/// `choice` `None` draws the bundled cities and countries.
pub fn build_with(env: &kit::PanelEnv, choice: Option<Choice>, window: &mut Window, cx: &mut App) -> anyhow::Result<kit::Built> {
    let style = kit::compile_style(STYLE)?;
    let source = kit::openfreemap_planet();
    let (w, h) = env.viewport;
    let host = env.host.clone();
    let runtime = env.runtime.clone();
    let start = opening_camera(choice.as_ref(), w, h);
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
    // Hover a place for its card: the bundled cities and countries their
    // own cards, Overture places their name and confidence, a file of your
    // own the default property table.
    let config = match &choice {
        None => crate::popup::PopupConfig::new()
            .source(CITIES, crate::popup::world_city_card())
            .source(COUNTRIES, crate::popup::natural_earth_country_card()),
        Some(Choice::Overture) => crate::popup::PopupConfig::new().source(
            FILE,
            crate::popup::CardSpec::new()
                .title("names.primary")
                .title_or("Unnamed place")
                .subtitle("Overture place")
                .field("confidence", "Confidence")
                .num(crate::popup::NumFmt::Fraction(0)),
        ),
        Some(Choice::File(_)) => crate::popup::PopupConfig::new(),
    };
    let popup = crate::popup::install(&map, config, window, cx);
    let bundled = choice.is_none();
    let mut loads: Vec<(&'static str, SharedString, Load)> = Vec::new();
    let err = |r: Result<GeoParquetReport, gpui_maps::overlay::geoparquet::GeoParquetError>| r.map_err(|e| e.to_string());
    match choice {
        Some(Choice::File(input)) => {
            let label: SharedString = match &input {
                GeoParquetInput::Url(url) => url.clone().into(),
                GeoParquetInput::Path(path) => path.display().to_string().into(),
                GeoParquetInput::Bytes(_) => "bytes".into(),
            };
            let task = map.update(cx, |view, cx| view.load_source_geoparquet(FILE, input, GeoParquetOptions::default(), plain_paint(), cx));
            loads.push((FILE, label, cx.background_spawn(async move { err(task.await) })));
        }
        Some(Choice::Overture) => {
            map.update(cx, |view, _| view.declare_source_paint(FILE, &places_paint())).map_err(|e| anyhow::anyhow!("{e:?}"))?;
            map.update(cx, |view, _| view.set_source_credit(FILE, Some("Places: Overture Maps Foundation (CDLA Permissive 2.0)")));
            let resolve = env.runtime.spawn(overture_places_url());
            let weak = map.downgrade();
            let task = cx.spawn(async move |cx| {
                let url = resolve.await.map_err(|e| e.to_string())??;
                eprintln!("geoparquet: Overture places at {url}");
                let options = GeoParquetOptions::default().with_columns(["names.primary", "confidence"]);
                let load = weak
                    .update(cx, |view, cx| view.load_source_geoparquet(FILE, GeoParquetInput::Url(url), options, FeaturePaint::default(), cx))
                    .map_err(|_| "the map closed".to_string())?;
                err(load.await)
            });
            loads.push((FILE, "Overture places, newest release (S3, by range request)".into(), task));
        }
        None => {
            map.update(cx, |view, _| view.declare_source_paint(CITIES, &cities_paint())).map_err(|e| anyhow::anyhow!("{e:?}"))?;
            map.update(cx, |view, _| {
                view.set_source_credit(CITIES, Some("Cities: gpui-maps sample"));
                view.set_source_credit(COUNTRIES, Some("Countries: Natural Earth (public domain)"));
            });
            let countries = map.update(cx, |view, cx| {
                view.load_source_geoparquet(
                    COUNTRIES,
                    GeoParquetInput::Bytes(COUNTRIES_PARQUET.to_vec()),
                    GeoParquetOptions::default(),
                    countries_paint(),
                    cx,
                )
            });
            loads.push((COUNTRIES, "naturalearth_countries.parquet (bundled, the spec's example)".into(), cx.background_spawn(async move { err(countries.await) })));
            let cities = map.update(cx, |view, cx| {
                view.load_source_geoparquet(
                    CITIES,
                    GeoParquetInput::Bytes(CITIES_PARQUET.to_vec()),
                    GeoParquetOptions::default(),
                    FeaturePaint::default(),
                    cx,
                )
            });
            loads.push((CITIES, "world_cities.parquet (bundled)".into(), cx.background_spawn(async move { err(cities.await) })));
        }
    }
    // The bundled cities' population, linked both ways, docked beside the
    // map (the bundled countries file is the spec's five-row example: too
    // few rows to chart).
    let chart = bundled.then(|| crate::linked_chart::city_population_chart(&map, CITIES, &popup, cx));
    let focus = if env.host.is_gallery() { FocusPolicy::Once } else { FocusPolicy::EveryFrame };
    let inner = cx.new(|_| FocusedMap::new(map.clone(), focus));
    let weak_map = map.downgrade();
    let root = cx.new(|cx: &mut Context<GeoParquetRoot>| {
        let mut lines = Vec::new();
        for (source, label, task) in loads {
            lines.push(Line { source, label, status: "loading...".into(), report: None });
            cx.spawn(async move |this, cx| {
                let started = std::time::Instant::now();
                let result = task.await;
                let ms = started.elapsed().as_secs_f64() * 1e3;
                match &result {
                    Ok(report) => eprintln!("geoparquet {source}: {report} ({ms:.0} ms, {} requests, {} bytes)", report.requests, report.bytes_read),
                    Err(e) => eprintln!("geoparquet {source}: {e}"),
                }
                let _ = this.update(cx, |root, cx| {
                    if let Some(line) = root.lines.iter_mut().find(|l| l.source == source) {
                        match result {
                            Ok(report) => {
                                line.status = format!("{report} in {ms:.0} ms").into();
                                line.report = Some(report);
                            }
                            Err(e) => line.status = format!("failed: {e}").into(),
                        }
                    }
                    cx.notify();
                });
            })
            .detach();
        }
        // A streamed source's counts move as the camera settles: the card
        // re-reads them twice a second.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(std::time::Duration::from_millis(500)).await;
            if this.update(cx, |_, cx| cx.notify()).is_err() {
                break;
            }
        })
        .detach();
        GeoParquetRoot { inner, popup, chart, map: weak_map, lines }
    });
    Ok(kit::Built { view: root.into(), map: Some(map.downgrade()), start })
}

/// The map and a small card: per source, rows and row groups read and
/// skipped, bytes fetched, and its extent.
pub struct GeoParquetRoot {
    inner: Entity<FocusedMap>,
    popup: Entity<crate::popup::MapPopup>,
    /// The cities' population chart (the bundled data only).
    pub chart: Option<Entity<crate::linked_chart::LinkedChart>>,
    map: WeakEntity<MapView>,
    lines: Vec<Line>,
}

impl GeoParquetRoot {
    /// Each source's status line: "loading..." until its load lands.
    pub fn statuses(&self) -> Vec<String> {
        self.lines.iter().map(|l| l.status.to_string()).collect()
    }
}

fn mb(bytes: u64) -> f64 {
    bytes as f64 / 1e6
}

impl Render for GeoParquetRoot {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui::{InteractiveElement as _, ParentElement as _, Styled as _};
        let map = self.map.upgrade();
        let mut card = gpui::div()
            .absolute()
            .top(gpui::px(12.0))
            .left(gpui::px(12.0))
            .max_w(gpui::px(460.0))
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
            .id("geoparquet-card")
            .occlude();
        for line in &self.lines {
            card = card.child(gpui::div().font_weight(gpui::FontWeight::SEMIBOLD).child(format!("GeoParquet: {}", line.label)));
            let Some(report) = &line.report else {
                card = card.child(gpui::div().child(line.status.clone()));
                continue;
            };
            let stream = map.as_ref().and_then(|m| m.read(cx).geoparquet_stream_stats(line.source));
            let counts = match stream {
                Some(s) => vec![
                    format!("{} of {} rows loaded, streaming ({} queries)", s.rows_loaded, s.rows_total, s.queries),
                    format!(
                        "row groups: {} read, {} skipped of {} | {:.1} of {:.1} MB fetched in {} requests",
                        s.row_groups_read,
                        s.row_groups_skipped,
                        s.row_groups_total,
                        mb(s.bytes_read),
                        mb(s.file_bytes),
                        s.requests
                    ),
                ],
                None => vec![
                    format!("{} of {} rows loaded ({} outside the box)", report.rows_loaded, report.rows_total, report.rows_filtered),
                    format!(
                        "row groups: {} read, {} skipped of {} | {:.2} MB fetched",
                        report.row_groups_read,
                        report.row_groups_skipped,
                        report.row_groups_total,
                        mb(report.bytes_read)
                    ),
                ],
            };
            for c in counts {
                card = card.child(gpui::div().child(c));
            }
            card = card.child(gpui::div().child(format!(
                "GeoParquet {} | {} | CRS {} | pushdown {:?}",
                report.version,
                report.encoding.name(),
                report.crs,
                report.pushdown
            )));
            for warning in &report.warnings {
                card = card.child(gpui::div().text_color(gpui::rgb(0x8a5a00)).child(warning.clone()));
            }
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
    use gpui_maps::overlay::geoparquet::{GeoParquetOptions, GeoParquetSource};

    #[test]
    fn the_bundled_files_read_with_typed_columns() {
        let cities = GeoParquetSource::from_bytes(super::CITIES_PARQUET, &GeoParquetOptions::default()).unwrap();
        assert_eq!(cities.len(), 46);
        assert!(cities.report().warnings.is_empty(), "{}", cities.report());
        let names: Vec<&str> = cities.report().columns.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["name", "country", "population", "capital"]);
        let countries = GeoParquetSource::from_bytes(super::COUNTRIES_PARQUET, &GeoParquetOptions::default()).unwrap();
        assert_eq!(countries.len(), 5);
    }

    #[test]
    fn the_paints_compile() {
        let mut store = gpui_maps::overlay::OverlayStore::new();
        store.declare_source_paint(super::CITIES, &super::cities_paint()).expect("compiles");
        store.declare_source_paint(super::FILE, &super::places_paint()).expect("compiles");
    }

    #[test]
    fn overture_is_opt_in_by_name() {
        assert!(matches!(super::choice_for("overture"), super::Choice::Overture));
        assert!(matches!(super::choice_for("/tmp/x.parquet"), super::Choice::File(_)));
    }

    /// The panel in a test window, on a file input (no network): once the
    /// off-thread load lands the card leaves "loading...".
    #[test]
    fn the_card_leaves_loading_and_the_map_holds_the_source() {
        use gpui::TestAppContext;
        let dir = std::env::temp_dir().join(format!("gpui-maps-gpq-panel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cities.parquet");
        std::fs::write(&path, super::CITIES_PARQUET).unwrap();
        let rt = tokio::runtime::Runtime::new().expect("a runtime");
        let env = crate::kit::PanelEnv::from_process(crate::kit::Host::Gallery, rt.handle().clone(), (800.0, 600.0));
        let mut app = TestAppContext::single();
        let cx = app.add_empty_window();
        let choice = Some(super::Choice::File(gpui_maps::map_view::GeoParquetInput::Path(path.clone())));
        let built = cx.update(|window, cx| super::build_with(&env, choice, window, cx)).expect("builds");
        let root = built.view.clone().downcast::<super::GeoParquetRoot>().unwrap_or_else(|_| panic!("a GeoParquetRoot"));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            cx.run_until_parked();
            let statuses = cx.update(|_, cx| root.read(cx).statuses());
            if statuses.iter().all(|s| s != "loading...") {
                assert!(statuses[0].starts_with("loaded 46 of 46 rows"), "{statuses:?}");
                break;
            }
            assert!(std::time::Instant::now() < deadline, "the card never left loading");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        env.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A standalone window (`cargo run --example geoparquet`) draws the card
    /// above the map too: without `set_chrome_above_map(true)` the map's
    /// Metal layer composites over every gpui element and the card is hidden.
    #[test]
    fn a_standalone_window_puts_the_card_above_the_map() {
        use gpui::TestAppContext;
        let dir = std::env::temp_dir().join(format!("gpui-maps-gpq-standalone-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cities.parquet");
        std::fs::write(&path, super::CITIES_PARQUET).unwrap();
        let rt = tokio::runtime::Runtime::new().expect("a runtime");
        let host = crate::kit::Host::Standalone { title: "geoparquet".into() };
        let env = crate::kit::PanelEnv::from_process(host, rt.handle().clone(), (800.0, 600.0));
        let mut app = TestAppContext::single();
        let cx = app.add_empty_window();
        let built = cx.update(|window, cx| super::build_with(&env, Some(super::Choice::File(gpui_maps::map_view::GeoParquetInput::Path(path))), window, cx)).expect("builds");
        let map = built.map.as_ref().and_then(|m| m.upgrade()).expect("a map");
        cx.update(|_, cx| assert!(map.read(cx).chrome_above_map(), "the standalone map hides the card"));
        env.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

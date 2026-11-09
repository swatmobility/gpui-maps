//! A CSV as an overlay source: 46 world cities from a bundled CSV, drawn as
//! circles coloured by a bool column (`capital`) and sized by a number
//! column (`population`) -- both types inferred by `overlay::csv`. Shared
//! by `demos/csv_points.rs` (windowed, optionally on any CSV path or URL)
//! and the gallery (`csv-points`).
//!
//! `GPUI_MAP_CSV` (a path or an http(s) URL) loads that CSV instead of the
//! bundled one, in the gallery as in the standalone window; such a file
//! draws in one plain colour, since its columns are unknown.
//!
//! The load runs off the UI thread (`MapView::load_source_csv`); the card
//! shows its parse report (rows loaded, skipped and why).
//!
//! `#![allow(dead_code)]`: included by two crates that each use a
//! different half of it.
#![allow(dead_code)]


use gpui::{App, AppContext as _, Context, Entity, IntoElement, Render, SharedString, Window};
use gpui_maps::map_view::{CsvInput, MapView};
use gpui_maps::overlay::csv::CsvOptions;
use gpui_maps::overlay::FeaturePaint;
use gpui_maps::style::Rgba;

use crate::kit;
use crate::shell::{FocusPolicy, FocusedMap};

pub const ID: &str = "csv-points";
pub const SOURCE: &str = "csv";
pub const START: (f64, f64, f64) = (15.0, 22.0, 1.4);

/// The bundled sample: the same file the engine's end-to-end capture test
/// draws (`src/capture/csv_e2e_tests.rs`).
pub const CITIES: &[u8] = include_bytes!("../../../tests/fixtures/world_cities.csv");

/// Network, for the basemap only: the cities are compiled in.
pub fn requirements(_env: &kit::PanelEnv) -> Vec<kit::Requirement> {
    vec![kit::Requirement::Network]
}

pub const CREDIT_CITIES: kit::Credit = kit::Credit {
    what: "Cities",
    holder: "Natural Earth (naturalearthdata.com), populated places",
    dataset: "46 world cities from Natural Earth 1:10m populated places v5.1.2: position and pop_max, capital from adm0cap",
    licence: kit::Licence::PublicDomain,
    url: "https://www.naturalearthdata.com/downloads/10m-cultural-vectors/10m-populated-places/",
    changes: "46 cities kept; positions rounded to 0.01 degree",
};

pub const CREDITS: &[kit::Credit] = &[CREDIT_CITIES, kit::CREDIT_OSM_OPENFREEMAP];

/// Where the CSV comes from: `GPUI_MAP_CSV` (path or URL), else the
/// bundled cities.
pub fn input_from_env() -> Option<CsvInput> {
    let location = std::env::var("GPUI_MAP_CSV").ok().filter(|s| !s.trim().is_empty())?;
    Some(if location.starts_with("https://") || location.starts_with("http://") {
        CsvInput::Url(location)
    } else {
        CsvInput::Path(location.into())
    })
}

/// The cities' paint: capitals orange, other cities blue (a `case` on the
/// bool column), radius by population (an `interpolate` on the number
/// column).
pub fn cities_paint() -> serde_json::Map<String, serde_json::Value> {
    let json = serde_json::json!({
        "circle-color": ["case", ["==", ["get", "capital"], true], "#e4572e", "#2e86ab"],
        "circle-radius": ["interpolate", ["linear"], ["get", "population"], 100_000, 4, 40_000_000, 14],
    });
    json.as_object().expect("an object").clone()
}

fn plain_paint() -> FeaturePaint {
    FeaturePaint { circle_radius: 2.5, circle_color: Rgba { r: 0.18, g: 0.53, b: 0.67, a: 0.9 }, ..FeaturePaint::default() }
}

pub fn build(env: &kit::PanelEnv, window: &mut Window, cx: &mut App) -> anyhow::Result<kit::Built> {
    build_with(env, input_from_env(), window, cx)
}

/// The bounds of every `[lon, lat]` point, as `[min_lon, min_lat, max_lon,
/// max_lat]`. Zero points leaves the `INFINITY`/`NEG_INFINITY` seed in
/// place, which is not finite -- `kit::coverage_from_bounds` rejects it as
/// unusable the same way it rejects any other degenerate box, rather than
/// this function needing its own `Option`.
fn bounds_of(points: &[[f64; 2]]) -> [f64; 4] {
    let mut b = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
    for [lon, lat] in points {
        b[0] = b[0].min(*lon);
        b[1] = b[1].min(*lat);
        b[2] = b[2].max(*lon);
        b[3] = b[3].max(*lat);
    }
    b
}

/// Where to open the camera: the CSV's own computed bounds, framed to
/// `(w, h)` the way the MBTiles/PMTiles panels frame an archive's
/// `coverage()` -- [`START`] is only the fallback for a row-less file, a
/// geometry column `points()` cannot read back as plain positions (WKT or
/// GeoJSON), or a read that failed outright (the real load below reports
/// that failure on the card; this falls back silently since the camera is
/// cosmetic either way).
///
/// With no custom input, the bundled sample also hits that fallback --
/// checked (`SELECT name, lat, lon ... ORDER BY lon`), not assumed: its 46
/// cities are a deliberately even spread from Honolulu (-157.86) to
/// Auckland (174.76), not a false antimeridian reading of a clustered
/// dataset the way the FlatGeobuf counties' header is. There is no single
/// region to fall back to for data that is the whole world by design, so
/// [`START`]'s existing whole-world framing (unlike the FlatGeobuf panel's
/// old single fixed point, which hid most of its data) already is "open on
/// its data" for this one.
///
/// Parses `input` a second time, synchronously -- the bundled cities are
/// already-resident bytes (microseconds), and a custom path or URL pays
/// one more read of a file [`MapView::load_source_csv`] is about to read
/// again anyway. Cheap enough not to warrant threading the parsed
/// [`gpui_maps::overlay::csv::CsvSource`] through the async loader instead.
fn opening_camera(input: &CsvInput, w: f64, h: f64) -> (f64, f64, f64) {
    let options = CsvOptions::default();
    let parsed = match input {
        CsvInput::Url(url) => gpui_maps::overlay::csv::CsvSource::from_url(url, &options).ok(),
        CsvInput::Path(path) => gpui_maps::overlay::csv::CsvSource::from_path(path, &options).ok(),
        CsvInput::Bytes(bytes) => gpui_maps::overlay::csv::CsvSource::from_bytes(bytes, &options).ok(),
    };
    let bbox = parsed.as_ref().and_then(|s| s.points()).map(bounds_of);
    match bbox.and_then(|[min_lon, min_lat, max_lon, max_lat]| kit::coverage_from_bounds(min_lon, min_lat, max_lon, max_lat)) {
        Some(coverage) => coverage.opening_camera(w, h),
        None => START,
    }
}

/// The page's basemap (keyless OpenFreeMap Positron): its Explorer sample
/// opens on the same one (`samples::tests::every_sample_opens_on_its_gallery_twins_basemap`).
pub const STYLE: &str = "demos/positron-style.json";

/// `input` `None` draws the bundled cities with [`cities_paint`].
pub fn build_with(env: &kit::PanelEnv, input: Option<CsvInput>, window: &mut Window, cx: &mut App) -> anyhow::Result<kit::Built> {
    let style = kit::compile_style(STYLE)?;
    let source = kit::openfreemap_planet();
    let (w, h) = env.viewport;
    let host = env.host.clone();
    let runtime = env.runtime.clone();
    let bundled = input.is_none();
    let (label, input, paint): (SharedString, CsvInput, FeaturePaint) = match input {
        Some(input) => {
            let label: SharedString = match &input {
                CsvInput::Url(url) => url.clone().into(),
                CsvInput::Path(path) => path.display().to_string().into(),
                CsvInput::Bytes(_) => "bytes".into(),
            };
            (label, input, plain_paint())
        }
        None => ("world_cities.csv (bundled)".into(), CsvInput::Bytes(CITIES.to_vec()), FeaturePaint::default()),
    };
    let start = opening_camera(&input, w, h);
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
    if bundled {
        map.update(cx, |view, _| view.declare_source_paint(SOURCE, &cities_paint())).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        map.update(cx, |view, _| view.set_source_credit(SOURCE, Some("Cities: gpui-maps sample")));
    }
    let task = map.update(cx, |view, cx| view.load_source_csv(SOURCE, input, CsvOptions::default(), paint, cx));
    // Hover a city for its card; the bundled file gets the cities card,
    // any other CSV the default property table.
    let config = if bundled { crate::popup::PopupConfig::new().source(SOURCE, crate::popup::world_city_card()) } else { crate::popup::PopupConfig::new() };
    let popup = crate::popup::install(&map, config, window, cx);
    // The bundled cities' population, linked both ways (brush -> the GPU
    // range filter; hover a bin -> its cities light; hover a city -> its
    // bin lights), docked beside the map.
    let chart = bundled.then(|| crate::linked_chart::city_population_chart(&map, SOURCE, &popup, cx));
    let focus = if env.host.is_gallery() { FocusPolicy::Once } else { FocusPolicy::EveryFrame };
    let inner = cx.new(|_| FocusedMap::new(map.clone(), focus));
    let root = cx.new(|cx: &mut Context<CsvRoot>| {
        cx.spawn(async move |this, cx| {
            let started = std::time::Instant::now();
            let result = task.await;
            let ms = started.elapsed().as_secs_f64() * 1e3;
            let status: SharedString = match &result {
                Ok(report) => format!("{report} in {ms:.0} ms").into(),
                Err(e) => format!("failed: {e}").into(),
            };
            match &result {
                Ok(report) => eprintln!("csv-points: {report} ({ms:.0} ms, columns {:?})", report.columns),
                Err(e) => eprintln!("csv-points: {e}"),
            }
            let _ = this.update(cx, |root, cx| {
                root.status = status;
                cx.notify();
            });
        })
        .detach();
        CsvRoot { inner, popup, chart, source: label, status: "loading...".into() }
    });
    Ok(kit::Built { view: root.into(), map: Some(map.downgrade()), start })
}

/// The map and a small card naming the CSV and its parse report.
pub struct CsvRoot {
    inner: Entity<FocusedMap>,
    popup: Entity<crate::popup::MapPopup>,
    /// The population chart (the bundled cities only).
    pub chart: Option<Entity<crate::linked_chart::LinkedChart>>,
    source: SharedString,
    status: SharedString,
}

impl CsvRoot {
    /// The card's report line: "loading..." until the load lands.
    pub fn status(&self) -> &str {
        &self.status
    }
}

impl Render for CsvRoot {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui::{InteractiveElement as _, ParentElement as _, Styled as _};
        // The card is translucent and occludes (as the COG panel's card);
        // the root itself paints nothing.
        let card = gpui::div()
            .absolute()
            .top(gpui::px(12.0))
            .left(gpui::px(12.0))
            .max_w(gpui::px(380.0))
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
            .id("csv-card")
            .occlude()
            .child(gpui::div().child(format!("CSV source: {}", self.source)))
            .child(gpui::div().child(self.status.clone()));
        let map_area = gpui::div()
            .relative()
            .size_full()
            .child(self.inner.clone())
            .child(crate::popup::input_layer(&self.popup, cx))
            .child(card)
            .child(self.popup.clone());
        match &self.chart {
            Some(chart) => crate::linked_chart::over_map(map_area, vec![chart.clone().into_any_element()]),
            None => map_area,
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_bundled_cities_parse_with_typed_columns() {
        let csv = gpui_maps::overlay::csv::CsvSource::from_bytes(super::CITIES, &Default::default()).unwrap();
        assert_eq!(csv.report().rows_skipped, 0, "{}", csv.report());
        assert_eq!(csv.len(), 46);
        use gpui_maps::overlay::csv::CsvColumnType::{Bool, Number, String};
        let types: Vec<_> = csv.report().columns.iter().map(|(n, t)| (n.as_str(), *t)).collect();
        assert_eq!(types, [("name", String), ("country", String), ("population", Number), ("capital", Bool)]);
    }

    /// The panel in a test window: once the off-thread load lands the
    /// card leaves "loading...", reports the rows, and the map holds the
    /// source and a style.
    #[test]
    fn the_card_leaves_loading_and_the_map_holds_the_source() {
        use gpui::TestAppContext;
        let rt = tokio::runtime::Runtime::new().expect("a runtime");
        let env = crate::kit::PanelEnv::from_process(crate::kit::Host::Gallery, rt.handle().clone(), (800.0, 600.0));
        let mut app = TestAppContext::single();
        let cx = app.add_empty_window();
        let built = cx.update(|window, cx| super::build_with(&env, None, window, cx)).expect("builds");
        let root = built.view.clone().downcast::<super::CsvRoot>().unwrap_or_else(|_| panic!("a CsvRoot"));
        let map = built.map.as_ref().and_then(|m| m.upgrade()).expect("a map");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            cx.run_until_parked();
            let status = cx.update(|_, cx| root.read(cx).status().to_string());
            if status != "loading..." {
                assert!(status.starts_with("loaded 46 of 46 rows"), "{status}");
                break;
            }
            assert!(std::time::Instant::now() < deadline, "the card never left loading");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        cx.update(|_, cx| {
            let map = map.read(cx);
            assert!(!map.style().layers.is_empty(), "the map has its basemap style");
            assert!(map.attributions().iter().any(|c| c.text.contains("gpui-maps sample")), "the source credit");
        });
        env.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    #[test]
    fn the_cities_paint_compiles() {
        let mut store = gpui_maps::overlay::OverlayStore::new();
        store.declare_source_paint(super::SOURCE, &super::cities_paint()).expect("compiles");
    }

    /// A standalone window (`cargo run --example csv_points`) draws the card
    /// above the map too: without `set_chrome_above_map(true)` the map's
    /// Metal layer composites over every gpui element and the card is hidden.
    #[test]
    fn a_standalone_window_puts_the_card_above_the_map() {
        use gpui::TestAppContext;
        let rt = tokio::runtime::Runtime::new().expect("a runtime");
        let host = crate::kit::Host::Standalone { title: "csv_points".into() };
        let env = crate::kit::PanelEnv::from_process(host, rt.handle().clone(), (800.0, 600.0));
        let mut app = TestAppContext::single();
        let cx = app.add_empty_window();
        let built = cx.update(|window, cx| super::build_with(&env, None, window, cx)).expect("builds");
        let map = built.map.as_ref().and_then(|m| m.upgrade()).expect("a map");
        cx.update(|_, cx| assert!(map.read(cx).chrome_above_map(), "the standalone map hides the card"));
        env.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

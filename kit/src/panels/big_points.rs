//! Millions of synthetic points as one overlay source: clusters at low
//! zoom, every point at high zoom. Shared by `demos/big_points.rs` (which
//! keeps its own `Args`, `main`, `--measure` and `--zoom-sweep`) and the
//! gallery (`big-points`).
//!
//! The data drawn here is SYNTHETIC and seeded -- see [`generate`] -- so
//! nothing is fetched or committed for the points themselves; only the
//! basemap underneath is network-dependent.
//!
//! [`generate`], [`cluster_options`], [`point_paint`], [`cluster_paint`]
//! and [`biggest_city`] are `pub`: this file is the ONE copy, and
//! `demos/big_points.rs`'s `measure` (its own perf-gate path) calls these
//! same functions rather than keeping a second copy that could drift from
//! what the gallery and the standalone window actually draw. `demos/big_points.rs`
//! already declares `#[path = "../kit/src/panels/big_points.rs"] mod panel;`
//! at its crate root to reach `Options`/`build_with`, so `measure`, in the
//! same crate root, reaches `panel::generate` and friends the same way.
#![allow(dead_code)]

use std::time::{Duration, Instant};

use gpui::{App, AppContext as _, Context, Entity, IntoElement, Render, Window};
use gpui_maps::map_view::MapView;
use gpui_maps::overlay::{ClusterMode, ClusterOptions, ClusterProperty, FeaturePaint, PropertyColumn};
use gpui_maps::style::Rgba;

use crate::kit;
use crate::shell::{FocusPolicy, FocusedMap};

pub const ID: &str = "big-points";
pub const SOURCE: &str = "points";
pub const START: (f64, f64, f64) = (-96.0, 38.5, 3.7);

/// Network for the basemap only: Graphite is keyless, token or not.
pub fn requirements(_env: &kit::PanelEnv) -> Vec<kit::Requirement> {
    vec![kit::Requirement::Network]
}

/// The points themselves, plus the keyless basemap under them.
pub const CREDITS: &[kit::Credit] = &[kit::CREDIT_SYNTHETIC, kit::CREDIT_OSM_OPENFREEMAP];

/// What the gallery (and `demos/big_points.rs`'s `windowed`) build against:
/// `points`/`seed` decide [`generate`]'s dataset, `cluster` and `mode`
/// decide [`cluster_options`]. Mirrors `demos/big_points.rs`'s `Args`
/// minus the flags that never reach this panel (`--measure`,
/// `--zoom-sweep`, `--quit-after`, `--basemap`).
#[derive(Clone)]
pub struct Options {
    pub points: usize,
    pub seed: u64,
    pub cluster: bool,
    pub mode: ClusterMode,
}

impl Default for Options {
    fn default() -> Self {
        Self { points: 2_000_000, seed: 1, cluster: true, mode: ClusterMode::Radius }
    }
}

/// splitmix64 -> uniform in [0, 1). Verbatim from `demos/big_points.rs`.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
    }

    fn gauss(&mut self) -> f64 {
        let (u, v) = (self.next().max(1e-12), self.next());
        (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
    }
}

/// The dataset: positions and the two property columns. No `biggest`
/// field (only `--zoom-sweep` and `measure`'s cameras need the biggest
/// city's centre, and [`biggest_city`] gets it far cheaper than a full
/// generation).
pub struct Data {
    pub lon_lat: Vec<[f64; 2]>,
    pub category: Vec<Option<f64>>,
    pub value: Vec<Option<f64>>,
}

/// The dataset generator: 40 US "cities" (Gaussian cores whose sizes
/// follow a Zipf curve) plus a sparse rural spread over the contiguous 48,
/// each point carrying a `category` (0-4) and a `value`. Deterministic in
/// `(n, seed)` -- the same seed always draws the same points, which is
/// what makes `--measure`'s `gate: points.*` figures comparable run to
/// run.
pub fn generate(n: usize, seed: u64) -> Data {
    let mut r = Rng(seed);
    let cities: Vec<(f64, f64, f64)> = (0..40)
        .map(|i| {
            let lon = -122.0 + r.next() * 50.0;
            let lat = 27.0 + r.next() * 20.0;
            let sigma = 0.04 + 0.25 / (1.0 + i as f64).sqrt();
            (lon, lat, sigma)
        })
        .collect();
    let weights: Vec<f64> = (0..cities.len()).map(|i| 1.0 / (1.0 + i as f64)).collect();
    let total: f64 = weights.iter().sum();
    let mut cumulative = Vec::with_capacity(weights.len());
    let mut acc = 0.0;
    for w in &weights {
        acc += w / total;
        cumulative.push(acc);
    }
    let mut lon_lat = Vec::with_capacity(n);
    let mut category = Vec::with_capacity(n);
    let mut value = Vec::with_capacity(n);
    for _ in 0..n {
        if r.next() < 0.1 {
            lon_lat.push([-124.0 + r.next() * 57.0, 25.0 + r.next() * 24.0]);
        } else {
            let pick = r.next();
            let c = cumulative.iter().position(|&c| pick <= c).unwrap_or(cities.len() - 1);
            let (lon, lat, sigma) = cities[c];
            let s = if r.next() < 0.6 { sigma * 0.3 } else { sigma };
            lon_lat.push([lon + r.gauss() * s / lat.to_radians().cos(), lat + r.gauss() * s]);
        }
        category.push(Some((r.next() * 5.0).floor()));
        value.push(Some((-r.next().max(1e-12).ln() * 100.0).round()));
    }
    Data { lon_lat, category, value }
}

/// City 0's centre alone, the first two `Rng` draws for `seed` -- exactly
/// what [`generate`]'s `cities[0]` lon/lat would be, without generating
/// any of the `n` points. `demos/big_points.rs`'s `--zoom-sweep` and
/// `measure`'s cameras both need only this.
pub fn biggest_city(seed: u64) -> (f64, f64) {
    let mut r = Rng(seed);
    let lon = -122.0 + r.next() * 50.0;
    let lat = 27.0 + r.next() * 20.0;
    (lon, lat)
}

/// Builds the clustering options a point source is declared with, taking
/// a pre-built [`ClusterMode`] (the caller picks radius/grid/H3).
pub fn cluster_options(cluster: bool, mode: ClusterMode) -> ClusterOptions {
    if cluster {
        ClusterOptions {
            properties: vec![ClusterProperty::sum("value_sum", "value"), ClusterProperty::max("value_max", "value")],
            mode,
            ..ClusterOptions::default()
        }
    } else {
        ClusterOptions::points_only()
    }
}

/// The green ramp, by points per cluster on a LOG scale: one stop per
/// decade, each the colour the "8+ billion people" page's Viridis ramp
/// (`DotRamp::Viridis`) has at 0.50, 0.58 ... 0.90 -- its green stretch,
/// the part that reads on Graphite, so the two pages share one look.
/// Viridis' dark
/// purple-blue start is left out (on a #1b1f26 basemap it is a hole), and
/// so is its yellow end: a capture with it had the biggest clusters read
/// as yellow discs, where the 8B page reads teal-green with a few bright
/// cores. The legend draws the same stops.
pub const RAMP: [(f64, u32); 6] =
    [(1.0, 0x21918c), (10.0, 0x1fa287), (100.0, 0x2db27d), (1_000.0, 0x52c569), (10_000.0, 0x86d549), (100_000.0, 0xbddf26)];

/// Circle radius (logical px) at each [`RAMP`] stop: also one step per
/// decade, so size, like colour, is log-scaled.
pub const RADII: [f32; 6] = [6.0, 8.5, 11.0, 14.5, 18.5, 23.0];

/// Every circle's alpha: translucent enough that a big cluster lets the
/// basemap's roads and coast through, as the 8B page's dots do.
pub const ALPHA: f32 = 0.8;

/// The cluster labels' ink: near-white with the Graphite basemap's own
/// place-label halo (`#070708`, a touch wider than its 1 px), so a count
/// reads on the brightest lime as on the darkest teal, in the basemap's
/// label voice.
pub const LABEL_COLOR: u32 = 0xf4f5f7;
pub const LABEL_HALO: u32 = 0x070708;

fn rgba(rgb: u32, a: f32) -> Rgba {
    let c = |shift: u32| ((rgb >> shift) & 0xff) as f32 / 255.0;
    Rgba { r: c(16), g: c(8), b: c(0), a }
}

fn css(rgb: u32, a: f32) -> String {
    format!("rgba({}, {}, {}, {a})", (rgb >> 16) & 0xff, (rgb >> 8) & 0xff, rgb & 0xff)
}

/// The unclustered points' own paint: [`RAMP`]'s first stop (a point is
/// a "cluster" of one), 2.5 px -- the 8B page's dot at its closest zoom,
/// which is where single points show here. Its `text-field` is the
/// clusters' count: a cluster takes this paint, and on a single point the
/// token is empty, which is no label at all.
pub fn point_paint() -> FeaturePaint {
    FeaturePaint {
        circle_radius: 2.5,
        circle_color: rgba(RAMP[0].1, ALPHA),
        text_field: Some("{point_count_abbreviated}".into()),
        text_size: 11.0,
        text_color: rgba(LABEL_COLOR, 1.0),
        text_halo_color: rgba(LABEL_HALO, 1.0),
        text_halo_width: 1.2,
        ..FeaturePaint::default()
    }
}

/// Clusters coloured and sized by `point_count` on a log scale: a linear
/// `interpolate` between one stop per decade is linear in log10(count)
/// within each decade ([`RAMP`], [`RADII`]). Points keep their own paint
/// (the expressions have no answer for a row without `point_count`).
pub fn cluster_paint() -> serde_json::Map<String, serde_json::Value> {
    let mut color = vec![serde_json::json!("interpolate"), serde_json::json!(["linear"]), serde_json::json!(["get", "point_count"])];
    let mut radius = color.clone();
    for ((count, rgb), r) in RAMP.iter().zip(RADII) {
        color.extend([serde_json::json!(count), serde_json::json!(css(*rgb, ALPHA))]);
        radius.extend([serde_json::json!(count), serde_json::json!(r)]);
    }
    let json = serde_json::json!({ "circle-color": color, "circle-radius": radius });
    json.as_object().expect("an object").clone()
}

/// Graphite (SWAT Routes Dark's look on open data), the "8+ billion
/// people" page's basemap, keyless with or without a Mapbox token.
fn basemap() -> kit::KeylessBasemap {
    kit::keyless_basemap(kit::KeylessStyle::OpenSwatRoutesDark).expect("the vendored Graphite style compiles")
}

pub fn build(env: &kit::PanelEnv, window: &mut Window, cx: &mut App) -> anyhow::Result<kit::Built> {
    build_with(env, Options::default(), window, cx)
}

pub fn build_with(env: &kit::PanelEnv, opts: Options, window: &mut Window, cx: &mut App) -> anyhow::Result<kit::Built> {
    let started = Instant::now();
    let data = generate(opts.points, opts.seed);
    println!("generate: {:.0} ms", started.elapsed().as_secs_f64() * 1e3);
    let basemap = basemap();
    let (w, h) = env.viewport;
    let runtime = env.runtime.clone();
    let map = cx.new(|_| {
        let mut view = MapView::new(START.0, START.1, START.2, w, h, basemap.style, basemap.source, runtime.clone());
        view.set_style_document(basemap.document);
        view
    });
    if let Some(sprite) = basemap.sprite {
        kit::spawn_sprites(map.downgrade(), sprite, None, runtime, cx);
    }
    // Always: the hover card is a gpui element over the map, and without
    // this the map's Metal layer composites above it.
    map.update(cx, |view, _cx| view.set_chrome_above_map(true));
    map.update(cx, |view, cx| {
        view.set_source_cluster_options(SOURCE, Some(cluster_options(opts.cluster, opts.mode)), cx).expect("valid options");
        view.declare_source_paint(SOURCE, &cluster_paint()).expect("valid paint");
        let started = Instant::now();
        view.set_source_points(
            SOURCE,
            &data.lon_lat,
            None,
            &point_paint(),
            &[("category", PropertyColumn::Number(&data.category)), ("value", PropertyColumn::Number(&data.value))],
            cx,
        )
        .expect("a valid push");
        println!("push (sync): {:.1} ms", started.elapsed().as_secs_f64() * 1e3);
    });
    // Hover: a card for the cluster or point under the pointer (and the
    // pointing hand); a click on a cluster eases in to where it splits, on
    // a point pins its card. The shared Studio component (`popup.rs`).
    let popup = crate::popup::install(&map, popup_config(), window, cx);
    // Points by category, linked both ways: a click keeps one category
    // (the GPU range filter), a hover lights its points, a hovered point
    // lights its bar.
    let chart = category_chart(&map, &popup, &data, opts.cluster, cx);
    let focus = if env.host.is_gallery() { FocusPolicy::Once } else { FocusPolicy::EveryFrame };
    let inner = cx.new(|_| FocusedMap::new(map.clone(), focus));
    let root = cx.new(|_| PointsRoot { inner, popup, chart });
    Ok(kit::Built { view: root.into(), map: Some(map.downgrade()), start: START })
}

/// The five synthetic categories.
pub const CATEGORIES: [&str; 5] = ["Category 0", "Category 1", "Category 2", "Category 3", "Category 4"];

/// The category chart's bars: the ramp's mid green ([`RAMP`]'s 1,000
/// stop), not the chart's default blue, which clashed with the green
/// clusters.
pub const CHART_GREEN: u32 = RAMP[3].1;

/// The category chart. Clusters have no category (a cluster's points mix
/// them), so a category filter draws the points themselves: the source
/// is re-declared unclustered while one is chosen (re-indexed off the UI
/// thread; Supercluster has no filter either), then the window is the GPU
/// range filter on `category` -- a uniform write per click after that.
/// "Show all" brings the clusters back.
fn category_chart(
    map: &Entity<MapView>,
    popup: &Entity<crate::popup::MapPopup>,
    data: &Data,
    cluster: bool,
    cx: &mut App,
) -> Entity<crate::linked_chart::LinkedChart> {
    use crate::linked_chart::{Category, ChartSpec, Kind, Link, LinkedChart, Selection};
    map.update(cx, |view, cx| view.set_source_filter_field(SOURCE, Some("category"), cx));
    let categories = CATEGORIES.iter().enumerate().map(|(i, c)| Category::new(gpui_maps::overlay::TagValue::Number(i as f64), c).color(CHART_GREEN)).collect();
    let keys = data.category.iter().map(|c| c.map(gpui_maps::overlay::TagValue::Number)).collect();
    let weak = map.downgrade();
    let mode = cluster_options(cluster, ClusterMode::Radius).mode;
    let apply = move |selection: &Selection, cx: &mut App| {
        let chosen = match selection {
            Selection::Bars(set) => set.first().copied(),
            _ => None,
        };
        let _ = weak.update(cx, |view, cx| {
            match chosen {
                Some(i) => {
                    let _ = view.set_source_cluster_options(SOURCE, Some(cluster_options(false, mode.clone())), cx);
                    view.set_source_filter_range(SOURCE, Some([i as f64, i as f64]), cx);
                }
                None => {
                    view.set_source_filter_range(SOURCE, None, cx);
                    let _ = view.set_source_cluster_options(SOURCE, Some(cluster_options(cluster, mode.clone())), cx);
                }
            }
        });
    };
    let spec = ChartSpec {
        title: "Points by category".into(),
        unit: "Synthetic points. Click a bar to show only its points (unclustered).".into(),
        noun: "points".into(),
        fmt: crate::popup::NumFmt::Compact,
        hint: None,
    };
    cx.new(|cx| {
        let mut c = LinkedChart::with_rows(map, SOURCE, "category", Kind::Bars { categories }, spec, Link::Callback(std::rc::Rc::new(apply)), keys, cx)
            .docked()
            .single_select();
        c.follow_popup(popup, cx);
        c
    })
}

/// Where a click on cluster `id` eases to: its expansion zoom (the zoom at
/// which it splits into its children), but always at least one level in.
/// At the opening z3.7 the biggest clusters split at z4 -- a 0.3-level
/// ease reads as "nothing happened".
pub fn cluster_click_zoom(expansion: Option<u8>, zoom: f64) -> f64 {
    expansion.map_or(zoom + 2.0, f64::from).max(zoom + 1.0).min(20.0)
}

fn number(props: &gpui_maps::overlay::FeatureProperties, key: &str) -> Option<f64> {
    match props.get(key) {
        Some(gpui_maps::overlay::TagValue::Number(n)) => Some(*n),
        _ => None,
    }
}

/// The card: a cluster says how many points it holds and what their
/// values sum to; a point says its category and value.
pub fn card(props: Option<&gpui_maps::overlay::FeatureProperties>) -> Option<crate::popup::Card> {
    use crate::popup::{number as fmt, Card, NumFmt};
    let props = props?;
    if let Some(count) = number(props, "point_count") {
        let mut rows = vec![("Points".to_string(), fmt(count, NumFmt::Int))];
        if let Some(sum) = number(props, "value_sum") {
            rows.push(("Value, total".into(), fmt(sum, NumFmt::Compact)));
        }
        if let Some(max) = number(props, "value_max") {
            rows.push(("Value, largest".into(), fmt(max, NumFmt::Int)));
        }
        return Some(Card {
            title: format!("Cluster of {} points", fmt(count, NumFmt::Compact)),
            subtitle: Some("Synthetic points, clustered".into()),
            rows,
            hint: Some("Click to zoom in until it splits".into()),
            ..Card::default()
        });
    }
    let category = number(props, "category")?;
    let mut rows = vec![("Category".to_string(), format!("{}", category as i64))];
    if let Some(v) = number(props, "value") {
        rows.push(("Value".into(), fmt(v, NumFmt::Int)));
    }
    Some(Card { title: "Synthetic point".into(), subtitle: Some(format!("Category {}", category as i64)), rows, hint: Some("Click to pin".into()), ..Card::default() })
}

fn popup_config() -> crate::popup::PopupConfig {
    use crate::popup::{ClickOutcome, PopupConfig};
    // cursor: a map popup's click callback, not an element (the map's pick sets the hand)
    PopupConfig::new().only_listed().custom(SOURCE, |pick| card(pick.properties.as_deref())).on_click(|pick, lon_lat, map, cx| {
        let cluster_id = pick.properties.as_deref().and_then(|p| number(p, "cluster_id"));
        let (Some(id), Some(lon_lat)) = (cluster_id, lon_lat) else { return ClickOutcome::Pin };
        map.update(cx, |view, cx| {
            let zoom = cluster_click_zoom(view.get_cluster_expansion_zoom(SOURCE, id as u32), view.camera().zoom);
            view.ease_to(
                gpui_maps::camera_control::CameraTarget { center: Some(lon_lat), zoom: Some(zoom), ..Default::default() },
                Duration::from_millis(500),
            );
            cx.notify();
        });
        ClickOutcome::Handled
    })
}

/// The panel's root: the focused map, the hover / popup layer, and the
/// legend card saying what is on screen.
pub struct PointsRoot {
    inner: Entity<FocusedMap>,
    popup: Entity<crate::popup::MapPopup>,
    pub chart: Entity<crate::linked_chart::LinkedChart>,
}

/// The legend's lines: what is drawn, what the circles mean, and what a
/// click does. [`HINT`]`[0]` is the headline, `[1]` and `[2]` sit above
/// and below the ramp, `[3]` is the footnote.
pub const HINT: [&str; 4] = [
    "2,000,000 synthetic points, clustered on the fly as you pan and zoom",
    "Each circle is a cluster; its size and colour = how many points it holds",
    "Points per cluster, log scale",
    "Hover for details; click a cluster to zoom in until it splits",
];

/// The ramp's tick labels, one per [`RAMP`] stop.
pub const TICKS: [&str; 6] = ["1", "10", "100", "1k", "10k", "100k"];

/// The legend: the "8+ billion people" page's counter card -- same
/// corner, palette, border and type sizes -- holding the headline, the
/// green ramp with a circle per decade sized as on the map, and what a
/// click does. Follows the app theme as that card does; dark without one
/// (the page is dark).
fn legend(dark: bool) -> gpui::Div {
    use gpui::{linear_color_stop, linear_gradient, px, rgb, ParentElement as _, Styled as _};
    let (bg, fg, muted, edge) = if dark {
        (gpui::rgba(0x1d20_25f2), rgb(0xe8eaed), rgb(0xa0a6ad), gpui::rgba(0xffff_ff14))
    } else {
        (gpui::rgba(0xffff_fff2), rgb(0x1f2933), rgb(0x5f6b76), gpui::rgba(0x0000_0014))
    };
    const BAR_W: f32 = 228.0;
    let rgba_of = |rgb: u32| gpui::Rgba { a: ALPHA, ..gpui::rgb(rgb) };
    // gpui's linear_gradient takes two stops: one segment per decade,
    // edge to edge, sharing colours at every seam (as census' bar does).
    let bar = gpui::div().flex().flex_row().w(px(BAR_W)).h(px(8.0)).rounded(px(2.0)).overflow_hidden().children(RAMP.windows(2).map(|pair| {
        gpui::div().flex_1().h_full().bg(linear_gradient(90.0, linear_color_stop(rgb(pair[0].1), 0.0), linear_color_stop(rgb(pair[1].1), 1.0)))
    }));
    // A circle per stop, at its map radius, centred over its tick and
    // standing on a common baseline.
    let slot = BAR_W / (RAMP.len() - 1) as f32;
    let tallest = RADII[RADII.len() - 1] * 2.0;
    let circles = gpui::div().relative().w(px(BAR_W)).h(px(tallest)).children(RAMP.iter().zip(RADII).enumerate().map(|(i, ((_, c), r))| {
        let r = r.min(slot * 0.9);
        gpui::div()
            .absolute()
            .left(px(i as f32 * slot - r))
            .top(px(tallest - r * 2.0))
            .size(px(r * 2.0))
            .rounded_full()
            .bg(rgba_of(*c))
    }));
    let ticks = gpui::div().relative().w(px(BAR_W)).h(px(14.0)).text_size(px(11.0)).text_color(muted).children(TICKS.iter().enumerate().map(|(i, t)| {
        gpui::div().absolute().top_0().left(px(i as f32 * slot - 16.0)).w(px(32.0)).flex().justify_center().child(*t)
    }));
    gpui::div()
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
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(HINT[0])
        .child(gpui::div().text_size(px(12.0)).text_color(muted).child(HINT[1]))
        // Inset by the widest circle's half, so the end circles sit inside the card.
        .child(gpui::div().px(px(16.0)).pt(px(4.0)).flex().flex_col().gap(px(4.0)).child(circles).child(bar).child(ticks))
        .child(gpui::div().text_size(px(12.0)).text_color(muted).child(HINT[2]))
        .child(gpui::div().text_size(px(12.0)).text_color(muted).child(HINT[3]))
}

impl Render for PointsRoot {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui::{ParentElement as _, Styled as _};
        let dark = cx.try_global::<gpui_component::Theme>().is_none_or(|t| t.mode.is_dark());
        let map_area = gpui::div()
            .relative()
            .size_full()
            .child(self.inner.clone())
            .child(crate::popup::input_layer(&self.popup, cx))
            .child(legend(dark))
            .child(self.popup.clone());
        crate::linked_chart::over_map(map_area, vec![gpui::IntoElement::into_any_element(self.chart.clone())])
    }
}

#[cfg(test)]
mod tests {
    use super::{card, cluster_click_zoom, HINT};

    /// The user's "clicking a cluster does nothing": at the opening z3.7 the
    /// biggest clusters' expansion zoom is z4, a 0.3-level ease nobody sees.
    #[test]
    fn a_cluster_click_always_zooms_in_at_least_one_level() {
        assert_eq!(cluster_click_zoom(Some(4), 3.7), 4.7, "z4 from z3.7 would be invisible");
        assert_eq!(cluster_click_zoom(Some(9), 3.7), 9.0, "a deeper split is followed all the way");
        assert_eq!(cluster_click_zoom(None, 5.0), 7.0);
        assert_eq!(cluster_click_zoom(Some(22), 19.5), 20.0);
    }

    /// The hint says what is drawn, what a click does and what the circles
    /// mean -- the user's "unclear what big points demo is".
    #[test]
    fn the_hint_says_what_it_shows_what_a_click_does_and_what_the_circles_mean() {
        let text = HINT.join(" ");
        assert!(text.contains("2,000,000 synthetic points") && text.contains("clustered on the fly"), "{text}");
        assert!(text.contains("size and colour") && text.contains("how many points"), "{text}");
        assert!(text.contains("click a cluster to zoom in"), "{text}");
    }

    fn props(pairs: &[(&str, f64)]) -> gpui_maps::overlay::FeatureProperties {
        pairs.iter().map(|(k, v)| ((*k).to_string(), gpui_maps::overlay::TagValue::Number(*v))).collect()
    }

    /// A cluster's card says how many points it holds and what a click
    /// does; a point's says its category and value (it printed them to
    /// stdout before).
    #[test]
    fn the_card_names_a_cluster_or_a_point() {
        let c = card(Some(&props(&[("point_count", 12_345.0), ("cluster_id", 7.0), ("value_sum", 1_234_567.0), ("value_max", 980.0)]))).unwrap();
        assert_eq!(c.title, "Cluster of 12.3k points");
        assert_eq!(c.rows[0], ("Points".to_string(), "12,345".to_string()));
        assert_eq!(c.rows[1].1, "1.2M");
        assert!(c.hint.unwrap().contains("zoom in"));
        let p = card(Some(&props(&[("category", 3.0), ("value", 117.0)]))).unwrap();
        assert_eq!(p.subtitle.as_deref(), Some("Category 3"));
        assert_eq!(p.rows[1], ("Value".to_string(), "117".to_string()));
        assert_eq!(card(None), None);
    }

    #[test]
    fn the_panel_root_sets_no_opaque_background() {
        // An opaque background call anywhere on the window -> MapView chain
        // hides the map, which draws in a CAMetalLayer behind gpui -- the
        // bug `race_dots` shipped. Every background in this file is the
        // legend's, which sits OVER the map as a sibling of it. The needle
        // is built at runtime (never written as one contiguous literal) so
        // this very assertion does not match itself via `include_str!`.
        let needle = [".", "bg", "("].concat();
        let source = include_str!("big_points.rs");
        let block = |start: &str, end: &str| {
            let at = source.find(start).unwrap_or_else(|| panic!("{start} is in big_points.rs"));
            &source[at..at + source[at..].find(end).expect("the block ends")]
        };
        let legend = block("fn legend(", "\nimpl Render for PointsRoot");
        assert!(!block("impl Render for PointsRoot", "\n}\n").contains(&needle), "the root must not set a background");
        assert_eq!(source.matches(&needle).count(), legend.matches(&needle).count(), "the legend's are the only backgrounds");
    }

    /// Clusters are green on Graphite, as on the 8B page. The ramp is
    /// Viridis' green half, the same hex
    /// the 8B page's `DotRamp::Viridis` ends on, one stop per decade (a log
    /// scale), and the colour and radius expressions read those stops.
    #[test]
    fn clusters_are_the_8b_green_ramp_on_a_log_scale() {
        use super::{cluster_paint, point_paint, RADII, RAMP};
        // The bottom stop is the 8B ramp's own mid-point colour (its third
        // stop), and the top stays green: more green than red.
        let viridis = gpui_maps::overlay::dotgrid::layer::DotRamp::Viridis.stops();
        let mid = viridis[2];
        let bottom = super::rgba(RAMP[0].1, 1.0);
        assert!((bottom.r - mid.r).abs() < 1e-3 && (bottom.g - mid.g).abs() < 1e-3 && (bottom.b - mid.b).abs() < 1e-3, "Viridis' own teal");
        let top = super::rgba(RAMP[RAMP.len() - 1].1, 1.0);
        assert!(top.g > top.r + 0.1, "green, not yellow, at the top");
        for pair in RAMP.windows(2) {
            assert_eq!(pair[1].0 / pair[0].0, 10.0, "one stop per decade: linear between them is log10");
            let g = |c: u32| (c >> 8) & 0xff;
            assert!(g(pair[1].1) > g(pair[0].1), "brighter green as the count grows");
        }
        assert!(RADII.windows(2).all(|r| r[1] > r[0]));
        let paint = cluster_paint();
        for key in ["circle-color", "circle-radius"] {
            let expr = paint[key].as_array().unwrap();
            assert_eq!(expr[0], "interpolate", "{key}");
            assert_eq!(expr[2], serde_json::json!(["get", "point_count"]), "{key}");
            assert_eq!(expr.len(), 3 + 2 * RAMP.len(), "{key}");
        }
        let p = point_paint();
        assert_eq!(p.text_field.as_deref(), Some("{point_count_abbreviated}"), "clusters say their count");
        assert!(p.text_color.r > 0.9 && p.text_color.g > 0.9 && p.text_color.b > 0.9, "near-white labels");
        assert_eq!(p.circle_color, super::rgba(RAMP[0].1, super::ALPHA), "a single point is a cluster of one");
    }

    /// "Points by category" draws its bars in the ramp's mid green, not
    /// the chart's default blue, and the dark card leaves it as it is.
    #[test]
    fn the_category_bars_are_the_ramps_mid_green() {
        use super::{CHART_GREEN, RAMP};
        assert_eq!(CHART_GREEN, RAMP[RAMP.len() / 2].1);
        let ch = |c: u32, s: u32| (c >> s) & 0xff;
        assert!(ch(CHART_GREEN, 8) > ch(CHART_GREEN, 16) + 40 && ch(CHART_GREEN, 8) > ch(CHART_GREEN, 0) + 40, "green, not blue: {CHART_GREEN:06x}");
        assert_eq!(crate::linked_chart::readable_on(CHART_GREEN, true), CHART_GREEN, "reads on the dark card unchanged");
    }

    /// Graphite under it, keyless: no Mapbox credit, no token requirement.
    #[test]
    fn the_basemap_is_keyless_graphite() {
        assert!(!super::CREDITS.iter().any(|c| c.licence == crate::kit::Licence::MapboxTos));
        assert_eq!(crate::kit::KeylessStyle::OpenSwatRoutesDark.label(), "Graphite");
    }
}

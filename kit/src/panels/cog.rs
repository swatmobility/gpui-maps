//! A Cloud-Optimised GeoTIFF as the map's raster layer:
//! [`gpui_maps::tile_source::cog::CogSource`] through `set_raster_source`,
//! drawn OVER a Liberty basemap (`MapView::set_raster_above_basemap`), so
//! the map shows outside the file's bounds and over its nodata, and under
//! Liberty's labels. Shared by `demos/cog.rs` (windowed)
//! and the Studio ("Cloud-Optimized GeoTIFF", under Data formats).
//!
//! The bundled file is `tests/fixtures/cog/rainier_dem.tif` (~220 KB): Mount
//! Rainier from USGS 3DEP's 1 arc-second DEM (public domain), resampled to
//! EPSG:3857 and written by `tile_source::cog::write` (`demos/cog.rs
//! --make-fixture`), drawn with the terrain ramp and a hillshade.
//!
//! The card's button streams the whole USGS 3DEP 1 arc-second tile it came
//! from (`n47w122`, 3612 x 3612 float32, LZW, ~55 MB, EPSG:4269) straight
//! off the public bucket by range request -- only the internal tiles in
//! view are read. `GPUI_MAP_COG` (a path or an http(s) URL) opens any
//! other COG instead.
//!
//! `#![allow(dead_code)]`: included by two crates that each use a
//! different half of it.
#![allow(dead_code)]

use std::sync::Arc;

use gpui::{App, AppContext as _, Context, Entity, IntoElement, Render, SharedString, WeakEntity, Window};
use gpui_maps::map_view::MapView;
use gpui_maps::tile_source::cog::{CogOptions, CogRamp, CogRendering, CogSource};
use gpui_maps::tile_source::TileSource;

use crate::kit;
use crate::shell::{FocusPolicy, FocusedMap};

pub const ID: &str = "cog";

/// The bundled DEM.
pub const RAINIER_TIF: &[u8] = include_bytes!("../../../tests/fixtures/cog/rainier_dem.tif");

/// USGS 3DEP 1 arc-second, the 1 x 1 degree tile north-west of 47N 122W.
pub const USGS_RAINIER_URL: &str = "https://prd-tnm.s3.amazonaws.com/StagedProducts/Elevation/1/TIFF/current/n47w122/USGS_1_n47w122.tif";

/// The variable a path or URL is read from.
pub const PATH_VAR: &str = "GPUI_MAP_COG";

pub const CREDIT_USGS: kit::Credit = kit::Credit {
    what: "Elevation",
    holder: "U.S. Geological Survey, 3D Elevation Program",
    dataset: "USGS 3DEP 1 arc-second DEM, tile n47w122 (bundled: resampled to EPSG:3857, 384 px)",
    licence: kit::Licence::PublicDomain,
    url: "https://www.usgs.gov/3d-elevation-program",
    changes: "bundled copy resampled and cropped; drawn with a terrain ramp and hillshade",
};

pub const CREDITS: &[kit::Credit] = &[CREDIT_USGS, kit::CREDIT_OSM_OPENFREEMAP];

/// The credit drawn on the map.
pub const USGS_ATTRIBUTION: &str = "Elevation: USGS 3DEP (public domain)";

/// Network: the labels' basemap (and the remote COG, when asked).
pub fn requirements(_env: &kit::PanelEnv) -> Vec<kit::Requirement> {
    vec![kit::Requirement::Network]
}

/// The DEM's options: terrain ramp over Rainier's range, hillshaded.
pub fn dem_options() -> CogOptions {
    CogOptions::default()
        .with_ramp(CogRamp::Terrain)
        .with_range(200.0, 4300.0)
        .with_hillshade(1.5)
        .with_attribution(USGS_ATTRIBUTION)
}

/// What is shown.
#[derive(Debug, Clone)]
pub enum Choice {
    Bundled,
    Usgs,
    Location(String),
}

impl Choice {
    fn label(&self) -> String {
        match self {
            Choice::Bundled => "rainier_dem.tif (bundled, 384 x 384 int16)".into(),
            Choice::Usgs => "USGS 3DEP n47w122 (S3, by range request)".into(),
            Choice::Location(l) => l.clone(),
        }
    }
}

/// `GPUI_MAP_COG`, when set: `usgs` or a path/URL.
pub fn choice_from_env() -> Option<Choice> {
    let v = std::env::var(PATH_VAR).ok().filter(|s| !s.trim().is_empty())?;
    Some(choice_for(v.trim()))
}

pub fn choice_for(location: &str) -> Choice {
    if location.eq_ignore_ascii_case("usgs") {
        Choice::Usgs
    } else {
        Choice::Location(location.to_string())
    }
}

/// Opens `choice` (blocking for a URL: call off the UI thread).
pub fn open(choice: &Choice) -> Result<CogSource, String> {
    let result = match choice {
        Choice::Bundled => CogSource::from_bytes(RAINIER_TIF.to_vec(), dem_options()),
        Choice::Usgs => CogSource::open_blocking(USGS_RAINIER_URL, dem_options()),
        Choice::Location(l) => CogSource::open_blocking(l, CogOptions::default().with_hillshade(1.0)),
    };
    result.map_err(|e| e.to_string())
}

/// Liberty with its one raster layer (the Natural Earth relief, faded to
/// 0.1 by zoom 6 and gone past 7) made the COG's: opaque at every zoom.
/// The engine draws the raster source wherever the panel puts it, at the
/// opacity the style's raster layer gives it.
pub fn cog_style() -> anyhow::Result<gpui_maps::style::Style> {
    let mut doc: serde_json::Value = serde_json::from_slice(&kit::asset_bytes("examples/liberty-style.json")?)?;
    let layers = doc["layers"].as_array_mut().ok_or_else(|| anyhow::anyhow!("liberty has no layers"))?;
    let raster = layers
        .iter_mut()
        .find(|l| l["type"] == "raster")
        .ok_or_else(|| anyhow::anyhow!("liberty has no raster layer"))?;
    let source = raster["source"].clone();
    *raster = serde_json::json!({ "id": "cog", "type": "raster", "source": source, "paint": { "raster-opacity": 1.0 } });
    gpui_maps::style_spec::parse_and_compile(&serde_json::to_vec(&doc)?)
}

pub fn build(env: &kit::PanelEnv, window: &mut Window, cx: &mut App) -> anyhow::Result<kit::Built> {
    build_with(env, choice_from_env().unwrap_or(Choice::Bundled), window, cx)
}

pub fn build_with(env: &kit::PanelEnv, choice: Choice, _window: &mut Window, cx: &mut App) -> anyhow::Result<kit::Built> {
    let style = cog_style()?;
    let sprite = style.sprite.clone();
    let basemap = kit::openfreemap_planet();
    let (w, h) = env.viewport;
    // The bundled file opens synchronously (it is bytes), so the camera
    // frames it; a remote choice starts on Rainier and loads in the
    // background.
    let bundled = CogSource::from_bytes(RAINIER_TIF.to_vec(), dem_options()).map_err(|e| anyhow::anyhow!("the bundled COG: {e}"))?;
    let start = bundled.opening_camera(w, h);
    let host = env.host.clone();
    let runtime = env.runtime.clone();
    let first = matches!(choice, Choice::Bundled).then(|| bundled.clone());
    let map = cx.new(|_| {
        let mut view = MapView::new(start.0, start.1, start.2, w, h, style, basemap, runtime);
        if let Some(cog) = &first {
            view.set_raster_source(Arc::new(cog.clone()) as Arc<dyn TileSource>, cog.tile_size());
        }
        if let Some(title) = host.title() {
            view.set_window_title_prefix(title.to_string());
        }
        // Always, not only in the gallery: the card (and its button) are
        // gpui elements over the map, and without this the map's Metal
        // layer composites above them -- a standalone window would show
        // the map and no card.
        view.set_chrome_above_map(true);
        // Over the basemap's fills, not under them: the COG's tiles are
        // transparent outside its bounds and at nodata, and the map shows
        // through there.
        view.set_raster_above_basemap(true);
        view
    });
    if let Some(sprite) = sprite {
        kit::spawn_sprites(map.downgrade(), sprite, None, env.runtime.clone(), cx);
    }
    let focus = if env.host.is_gallery() { FocusPolicy::Once } else { FocusPolicy::EveryFrame };
    let inner = cx.new(|_| FocusedMap::new(map.clone(), focus));
    let weak_map = map.downgrade();
    let runtime = env.runtime.clone();
    let root = cx.new(|cx: &mut Context<CogRoot>| {
        let mut root = CogRoot { inner, map: weak_map, runtime, label: choice.label().into(), source: first, status: None, opened_ms: None };
        if !matches!(choice, Choice::Bundled) {
            root.switch(choice, cx);
        }
        // The counters move as tiles land: the card re-reads them twice a
        // second.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(std::time::Duration::from_millis(500)).await;
            if this.update(cx, |_, cx| cx.notify()).is_err() {
                break;
            }
        })
        .detach();
        root
    });
    Ok(kit::Built { view: root.into(), map: Some(map.downgrade()), start })
}

/// The map and a card: what the file says about itself, and what reading
/// it has cost so far.
pub struct CogRoot {
    inner: Entity<FocusedMap>,
    map: WeakEntity<MapView>,
    runtime: tokio::runtime::Handle,
    label: SharedString,
    source: Option<CogSource>,
    status: Option<SharedString>,
    opened_ms: Option<f64>,
}

impl CogRoot {
    /// Opens `choice` off the UI thread and puts it on the map.
    pub fn switch(&mut self, choice: Choice, cx: &mut Context<Self>) {
        self.label = choice.label().into();
        self.status = Some("opening...".into());
        let job = self.runtime.spawn_blocking(move || {
            let started = std::time::Instant::now();
            open(&choice).map(|c| (c, started.elapsed().as_secs_f64() * 1e3))
        });
        cx.spawn(async move |this, cx| {
            let result = job.await.map_err(|e| e.to_string()).and_then(|r| r);
            let _ = this.update(cx, |root, cx| {
                match result {
                    Ok((cog, ms)) => {
                        eprintln!("cog: {} opened in {ms:.0} ms: {}", root.label, cog.metadata().summary());
                        if let Some(map) = root.map.upgrade() {
                            let source = Arc::new(cog.clone()) as Arc<dyn TileSource>;
                            map.update(cx, |m, cx| {
                                m.set_raster_source(source, cog.tile_size());
                                cx.notify();
                            });
                        }
                        root.source = Some(cog);
                        root.opened_ms = Some(ms);
                        root.status = None;
                    }
                    Err(e) => {
                        eprintln!("cog: {}: {e}", root.label);
                        root.status = Some(format!("failed: {e}").into());
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The card's lines (for tests and logs).
    pub fn lines(&self) -> Vec<String> {
        let mut lines = vec![format!("Cloud GeoTIFF: {}", self.label)];
        if let Some(status) = &self.status {
            lines.push(status.to_string());
        }
        let Some(cog) = &self.source else { return lines };
        let m = cog.metadata();
        lines.push(m.summary());
        lines.push(format!("bounds {:.3}, {:.3} .. {:.3}, {:.3}  max zoom {}", m.bounds[0], m.bounds[1], m.bounds[2], m.bounds[3], m.max_zoom));
        let levels: Vec<String> = m.overviews.iter().map(|(w, _)| w.to_string()).collect();
        lines.push(format!("overviews {}", if levels.is_empty() { "none".into() } else { levels.join(", ") }));
        lines.push(match &m.rendering {
            CogRendering::Rgb { alpha } => format!("drawn as RGB{}", if *alpha { "A" } else { "" }),
            CogRendering::Ramp { band, min, max } => format!(
                "band {band} through a ramp, {min:.0} .. {max:.0}{}",
                m.nodata.map(|n| format!(", nodata {n}")).unwrap_or_default()
            ),
        });
        let opened = self.opened_ms.map(|ms| format!(" in {ms:.0} ms")).unwrap_or_default();
        lines.push(format!("header: {:.1} KB in {} request(s){opened}", m.header_bytes as f64 / 1e3, m.header_requests));
        let s = cog.stats();
        lines.push(format!(
            "{} map tiles | {} requests, {:.2} MB | {} tiles decoded, {} cache hits",
            s.map_tiles,
            s.requests,
            s.bytes_read as f64 / 1e6,
            s.tiles_decoded,
            s.cache_hits
        ));
        for w in &m.warnings {
            lines.push(w.clone());
        }
        lines
    }
}

impl Render for CogRoot {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui::{InteractiveElement as _, ParentElement as _, StatefulInteractiveElement as _, Styled as _};
        let lines = self.lines();
        let card = gpui::div()
            .absolute()
            .top(gpui::px(12.0))
            .left(gpui::px(12.0))
            .max_w(gpui::px(460.0))
            .px(gpui::px(10.0))
            .py(gpui::px(6.0))
            .rounded(gpui::px(6.0))
            .bg(gpui::rgba(0xffffffdd))
            .text_color(gpui::rgb(0x1f2933))
            .text_xs()
            .flex()
            .flex_col()
            .gap(gpui::px(2.0));
        // The card swallows the mouse: a click on its button must not also
        // drag the map beneath (see `MapView::set_chrome_above_map`).
        let mut card = card.id("cog-card").occlude();
        for (i, line) in lines.into_iter().enumerate() {
            let row = gpui::div().child(line);
            card = card.child(if i == 0 { row.font_weight(gpui::FontWeight::SEMIBOLD) } else { row });
        }
        let streaming = self.label.starts_with("USGS");
        let button = |id: &'static str, text: &'static str| {
            gpui::div()
                .id(id)
                .mt(gpui::px(4.0))
                .px(gpui::px(8.0))
                .py(gpui::px(3.0))
                .rounded(gpui::px(4.0))
                .bg(gpui::rgb(0x1f6feb))
                .text_color(gpui::rgb(0xffffff))
                .cursor_pointer()
                .child(text)
        };
        card = card.child(if streaming {
            button("cog-bundled", "Show the bundled file").cursor_pointer().on_click(cx.listener(|root, _, _, cx| root.switch(Choice::Bundled, cx)))
        } else {
            button("cog-usgs", "Stream USGS 3DEP n47w122 (55 MB, by range request)").cursor_pointer().on_click(cx.listener(|root, _, _, cx| root.switch(Choice::Usgs, cx)))
        });
        gpui::div().relative().size_full().child(self.inner.clone()).child(card)
    }
}

#[cfg(test)]
mod tests {
    use gpui_maps::tile_source::cog::CogCrs;

    #[test]
    fn the_bundled_dem_opens_in_web_mercator_with_overviews() {
        let cog = super::open(&super::Choice::Bundled).expect("opens");
        let m = cog.metadata();
        assert_eq!(m.crs, CogCrs::WebMercator { epsg: 3857 });
        assert!(!m.overviews.is_empty());
        assert!(super::RAINIER_TIF.len() < 300_000, "the fixture stays under 300 KB");
        // Rainier's summit, ~-121.76, 46.85, is inside.
        assert!(m.bounds[0] < -121.76 && m.bounds[2] > -121.76 && m.bounds[1] < 46.85 && m.bounds[3] > 46.85, "{:?}", m.bounds);
    }

    #[test]
    fn usgs_is_opt_in_by_name() {
        assert!(matches!(super::choice_for("usgs"), super::Choice::Usgs));
        assert!(matches!(super::choice_for("/tmp/x.tif"), super::Choice::Location(_)));
    }

    /// A standalone window (`cargo run --example cog`) draws the card
    /// above the map too: without `set_chrome_above_map(true)` the map's
    /// Metal layer composites over every gpui element and the card is hidden.
    #[test]
    fn a_standalone_window_puts_the_card_above_the_map() {
        use gpui::TestAppContext;
        let rt = tokio::runtime::Runtime::new().expect("a runtime");
        let host = crate::kit::Host::Standalone { title: "cog".into() };
        let env = crate::kit::PanelEnv::from_process(host, rt.handle().clone(), (800.0, 600.0));
        let mut app = TestAppContext::single();
        let cx = app.add_empty_window();
        let built = cx.update(|window, cx| super::build_with(&env, super::Choice::Bundled, window, cx)).expect("builds");
        let map = built.map.as_ref().and_then(|m| m.upgrade()).expect("a map");
        cx.update(|_, cx| assert!(map.read(cx).chrome_above_map(), "the standalone map hides the card"));
        env.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// The COG draws OVER a vector basemap, so the map shows outside its
    /// bounds and over its nodata (both transparent in the tile): the
    /// panel's style has land, water and roads, the raster is set above
    /// the basemap, and the style draws it opaque at every zoom the COG
    /// reaches (Liberty's own relief layer fades out at zoom 7).
    #[test]
    fn the_cog_draws_over_a_vector_basemap_at_every_zoom() {
        use gpui::TestAppContext;
        use gpui_maps::style::StyleLayer;
        let rt = tokio::runtime::Runtime::new().expect("a runtime");
        let host = crate::kit::Host::Standalone { title: "cog".into() };
        let env = crate::kit::PanelEnv::from_process(host, rt.handle().clone(), (800.0, 600.0));
        let mut app = TestAppContext::single();
        let cx = app.add_empty_window();
        let built = cx.update(|window, cx| super::build_with(&env, super::Choice::Bundled, window, cx)).expect("builds");
        let map = built.map.as_ref().and_then(|m| m.upgrade()).expect("a map");
        cx.update(|_, cx| {
            let map = map.read(cx);
            assert!(map.raster_above_basemap(), "the COG is drawn under the basemap's fills");
            let layers = &map.style().layers;
            assert!(layers.iter().any(|l| matches!(l, StyleLayer::Fill { .. })), "the basemap has no ground to show");
            for zoom in [0u8, 7, 10, 12, 16] {
                let o = gpui_maps::render::raster_plan::raster_opacity(zoom, layers);
                assert_eq!(o.opacity, Some(1.0), "z{zoom}: {o:?}");
            }
        });
        env.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

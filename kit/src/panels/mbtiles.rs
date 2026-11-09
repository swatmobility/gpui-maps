//! An MBTiles file as the basemap: [`gpui_maps::tile_source::mbtiles::MbtilesSource`],
//! the SQLite sibling of `PmtilesSource`, drawn with the vendored Liberty
//! style and the committed sprite sheet. Shared by `demos/mbtiles.rs` and
//! the Studio (`mbtiles`, under Data formats).
//!
//! The bundled sample is written at build time into a temporary file from
//! the three real OpenFreeMap z14 tiles already committed under
//! `benches/fixtures/` (Singapore, Shinjuku, Manhattan; ODbL), gzipped as
//! real writers store them, with the metadata an MBTiles writer would put
//! next to them. The map opens on Shinjuku; the other two tiles are the
//! rest of the file. Nothing is downloaded and no binary is committed.
//!
//! `GPUI_MAP_MBTILES=/path/to/area.mbtiles` opens that file instead (any
//! OpenMapTiles-schema vector file draws; a raster file goes on as imagery
//! over the same style). The card shows what the file says about itself:
//! name, format, zooms, bounds, layers and its credit.
//!
//! `#![allow(dead_code)]`: included by two crates that each use a
//! different half of it.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{App, AppContext as _, Context, Entity, IntoElement, Render, SharedString, Window};
use gpui_maps::geo::TileId;
use gpui_maps::map_view::MapView;
use gpui_maps::sprite::SpriteAtlas;
use gpui_maps::tile_source::mbtiles::{MbtilesMetadata, MbtilesSource};
use gpui_maps::tile_source::{EmptyTileSource, TileSource};

use crate::kit;
use crate::shell::{FocusPolicy, FocusedMap};

pub const ID: &str = "mbtiles";

/// The variable a local `.mbtiles` path is read from.
pub const PATH_VAR: &str = "GPUI_MAP_MBTILES";

/// The three committed tiles: `(label, tile, bytes)`.
pub const SAMPLE_TILES: &[(&str, TileId, &[u8])] = &[
    ("Shinjuku", TileId { z: 14, x: 14549, y: 6451 }, include_bytes!("../../../benches/fixtures/shinjuku_z14.pbf")),
    ("Singapore", TileId { z: 14, x: 12918, y: 8134 }, include_bytes!("../../../benches/fixtures/singapore_z14.pbf")),
    ("Manhattan", TileId { z: 14, x: 4824, y: 6157 }, include_bytes!("../../../benches/fixtures/manhattan_z14.pbf")),
];

/// OpenFreeMap's sprite sheet, fetched once into the sprite cache: its
/// licence is unverified, so it is not in the tree.
const OFM_SPRITE: &str = "https://tiles.openfreemap.org/sprites/ofm_f384/ofm";

/// Nothing: the sample is compiled in, the style is vendored, glyphs come
/// from the system font book, and the sprite sheet is optional (cached).
pub fn requirements(_env: &kit::PanelEnv) -> Vec<kit::Requirement> {
    Vec::new()
}

/// The committed tiles (ODbL, OpenFreeMap) and sprite sheet.
pub const CREDITS: &[kit::Credit] = &[kit::CREDIT_OSM_OPENFREEMAP, kit::CREDIT_OFM_SPRITES_UNVERIFIED];

fn gzip(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write as _;
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(bytes).expect("gzip into a Vec");
    gz.finish().expect("gzip into a Vec")
}

/// Writes an MBTiles 1.3 file: `metadata`, `tiles` (rows TMS-flipped) and
/// the unique tile index, in one transaction. `tiles` holds XYZ ids and the
/// blob as it is to be stored.
pub fn write_mbtiles(path: &Path, metadata: &[(&str, String)], tiles: impl IntoIterator<Item = (TileId, Vec<u8>)>) -> anyhow::Result<()> {
    let mut conn = rusqlite::Connection::open(path)?;
    conn.execute_batch(
        "PRAGMA journal_mode = OFF; PRAGMA synchronous = OFF;
         CREATE TABLE metadata (name text, value text);
         CREATE TABLE tiles (zoom_level integer, tile_column integer, tile_row integer, tile_data blob);
         CREATE UNIQUE INDEX tile_index ON tiles (zoom_level, tile_column, tile_row);",
    )?;
    let tx = conn.transaction()?;
    for (name, value) in metadata {
        tx.execute("INSERT INTO metadata VALUES (?1, ?2)", (name, value))?;
    }
    {
        let mut insert = tx.prepare("INSERT INTO tiles VALUES (?1, ?2, ?3, ?4)")?;
        for (tile, bytes) in tiles {
            insert.execute((tile.z, tile.x, (1u32 << tile.z) - 1 - tile.y, bytes))?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// The bundled sample, written to `path` (~0.6 MB).
pub fn write_sample(path: &Path) -> anyhow::Result<()> {
    let bounds = SAMPLE_TILES.iter().map(|(_, t, _)| gpui_maps::geo::tile_bounds_lonlat(*t)).fold(
        (180.0f64, 90.0f64, -180.0f64, -90.0f64),
        |(w, s, e, n), b| (w.min(b.min_lon), s.min(b.min_lat), e.max(b.max_lon), n.max(b.max_lat)),
    );
    let shinjuku = gpui_maps::geo::tile_bounds_lonlat(SAMPLE_TILES[0].1);
    let metadata = [
        ("name", "Three cities (gpui-maps sample)".to_string()),
        ("format", "pbf".to_string()),
        ("type", "baselayer".to_string()),
        ("description", "Three OpenFreeMap z14 tiles: Shinjuku, Singapore, Manhattan".to_string()),
        ("minzoom", "14".to_string()),
        ("maxzoom", "14".to_string()),
        ("bounds", format!("{:.4},{:.4},{:.4},{:.4}", bounds.0, bounds.1, bounds.2, bounds.3)),
        (
            "center",
            format!("{:.5},{:.5},14", (shinjuku.min_lon + shinjuku.max_lon) / 2.0, (shinjuku.min_lat + shinjuku.max_lat) / 2.0),
        ),
        (
            "attribution",
            r#"<a href="https://openfreemap.org">OpenFreeMap</a> <a href="https://www.openmaptiles.org/">&copy; OpenMapTiles</a> Data from <a href="https://www.openstreetmap.org/copyright">OpenStreetMap</a>"#.to_string(),
        ),
        (
            "json",
            r#"{"vector_layers":[{"id":"building","minzoom":13},{"id":"transportation","minzoom":4},{"id":"water","minzoom":0},{"id":"landuse","minzoom":4},{"id":"poi","minzoom":12}]}"#.to_string(),
        ),
    ];
    write_mbtiles(path, &metadata, SAMPLE_TILES.iter().map(|(_, t, b)| (*t, gzip(b))))
}

fn cached_sprites() -> Option<Arc<SpriteAtlas>> {
    gpui_maps::cached_sprite_atlas(OFM_SPRITE, false)
        .map(Arc::new)
        .inspect_err(|err| eprintln!("mbtiles: no sprite sheet (not cached and the fetch failed: {err:#}); drawing without icons"))
        .ok()
}

/// `GPUI_MAP_MBTILES`, when set and non-empty.
pub fn path_from_env() -> Option<PathBuf> {
    std::env::var(PATH_VAR).ok().filter(|p| !p.trim().is_empty()).map(PathBuf::from)
}

pub fn build(env: &kit::PanelEnv, window: &mut Window, cx: &mut App) -> anyhow::Result<kit::Built> {
    build_with(env, path_from_env(), window, cx)
}

/// `path` `None` writes and opens the bundled sample.
pub fn build_with(env: &kit::PanelEnv, path: Option<PathBuf>, _window: &mut Window, cx: &mut App) -> anyhow::Result<kit::Built> {
    let (path, label, sample) = match path {
        Some(path) => {
            let label = kit::tilde(&path);
            (path, label, None)
        }
        None => {
            let file = tempfile::Builder::new().prefix("gpui-map-sample-").suffix(".mbtiles").tempfile()?.into_temp_path();
            write_sample(&file)?;
            (file.to_path_buf(), "three_cities.mbtiles (bundled)".to_string(), Some(file))
        }
    };
    let started = std::time::Instant::now();
    let source = MbtilesSource::open_blocking(&path)?;
    let open_ms = started.elapsed().as_secs_f64() * 1e3;
    let metadata = source.metadata().clone();
    let (lon, lat, zoom) = {
        let (w, h) = env.viewport;
        source.coverage().opening_camera(w, h)
    };
    eprintln!(
        "mbtiles: {label}: {} {} z{}-{} opened in {open_ms:.1} ms; camera {lon:.4},{lat:.4} z{zoom:.1}",
        metadata.name.as_deref().unwrap_or("(unnamed)"),
        metadata.format.as_str(),
        metadata.min_zoom,
        metadata.max_zoom,
    );
    let credit = source.attribution().map(str::to_string);

    let style = kit::compile_style("examples/liberty-style.json")?;
    let source: Arc<dyn TileSource> = Arc::new(source);
    // Vector tiles are the basemap; raster tiles go on as imagery over the
    // style's own background, through the same `TileSource`.
    let (basemap, raster): (Arc<dyn TileSource>, Option<Arc<dyn TileSource>>) =
        if metadata.format.is_vector() { (source, None) } else { (Arc::new(EmptyTileSource), Some(source)) };
    let sprites = cached_sprites();
    let (w, h) = env.viewport;
    let host = env.host.clone();
    let runtime = env.runtime.clone();
    let map = cx.new(|_| {
        let mut view = MapView::new(lon, lat, zoom, w, h, style, basemap, runtime);
        if let Some(sprites) = sprites {
            view.set_sprites(sprites);
        }
        if let Some(raster) = raster {
            view.set_raster_source(raster, 256);
        }
        if let Some(title) = host.title() {
            view.set_window_title_prefix(title.to_string());
        }
        // Always, not only in the gallery: the card is a gpui element over
        // the map, and without this the map's Metal layer composites above
        // it -- a standalone window would show the map and no card.
        view.set_chrome_above_map(true);
        view
    });
    let focus = if env.host.is_gallery() { FocusPolicy::Once } else { FocusPolicy::EveryFrame };
    let inner = cx.new(|_| FocusedMap::new(map.clone(), focus));
    let lines = card_lines(&label, &metadata, credit.as_deref(), open_ms);
    let root = cx.new(|_: &mut Context<MbtilesRoot>| MbtilesRoot { inner, lines, _sample: sample });
    Ok(kit::Built { view: root.into(), map: Some(map.downgrade()), start: (lon, lat, zoom) })
}

/// The card's lines: what the file says about itself.
pub fn card_lines(label: &str, m: &MbtilesMetadata, credit: Option<&str>, open_ms: f64) -> Vec<SharedString> {
    let mut lines: Vec<SharedString> = vec![
        format!("MBTiles: {label}").into(),
        format!("name: {}", m.name.as_deref().unwrap_or("(none)")).into(),
        format!("format: {}  zooms: {}-{}  opened in {open_ms:.1} ms", m.format.as_str(), m.min_zoom, m.max_zoom).into(),
    ];
    lines.push(match m.bounds {
        Some(b) => format!("bounds: {:.4}, {:.4} .. {:.4}, {:.4}", b.min_lon, b.min_lat, b.max_lon, b.max_lat).into(),
        None => "bounds: (none stated: the world)".into(),
    });
    if !m.vector_layers.is_empty() {
        let shown: Vec<&str> = m.vector_layers.iter().take(6).map(String::as_str).collect();
        let more = m.vector_layers.len().saturating_sub(shown.len());
        let tail = if more > 0 { format!(" +{more}") } else { String::new() };
        lines.push(format!("layers: {}{tail}", shown.join(", ")).into());
    }
    lines.push(format!("credit: {}", credit.unwrap_or("(none stated)")).into());
    lines
}

/// The map and a card with the file's metadata. Holds the bundled sample's
/// temporary file, which is deleted when the panel closes.
pub struct MbtilesRoot {
    inner: Entity<FocusedMap>,
    lines: Vec<SharedString>,
    _sample: Option<tempfile::TempPath>,
}

impl MbtilesRoot {
    pub fn lines(&self) -> &[SharedString] {
        &self.lines
    }
}

impl Render for MbtilesRoot {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        use gpui::{InteractiveElement as _, ParentElement as _, Styled as _};
        // Translucent and occluding, as the COG panel's card; the
        // root itself paints nothing.
        let card = gpui::div()
            .absolute()
            .top(gpui::px(12.0))
            .left(gpui::px(12.0))
            .max_w(gpui::px(420.0))
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
            .id("mbtiles-card")
            .occlude()
            .children(self.lines.iter().map(|line| gpui::div().child(line.clone())));
        gpui::div().relative().size_full().child(self.inner.clone()).child(card)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sample_is_small_and_reads_back_every_tile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.mbtiles");
        write_sample(&path).unwrap();
        let len = std::fs::metadata(&path).unwrap().len();
        assert!(len < 1_000_000, "sample is {len} bytes");
        let source = MbtilesSource::open_blocking(&path).unwrap();
        let m = source.metadata();
        assert_eq!((m.min_zoom, m.max_zoom, m.format.is_vector()), (14, 14, true));
        assert!(source.attribution().is_some_and(|c| c.contains("OpenStreetMap")));
        for (name, tile, bytes) in SAMPLE_TILES {
            let got = pollster::block_on(source.fetch(*tile)).unwrap();
            assert_eq!(got.as_deref(), Some(*bytes), "{name}");
        }
        let (lon, lat, zoom) = source.coverage().opening_camera(800.0, 600.0);
        let at = gpui_maps::geo::tile_id_at(lon, lat, 14);
        assert_eq!((at, zoom), (SAMPLE_TILES[0].1, 14.0), "opens on Shinjuku");
    }

    /// The panel in a test window: the card names the sample and its
    /// metadata, and the map credits the file's own attribution.
    #[test]
    fn the_panel_shows_the_metadata_and_credits_the_file() {
        use gpui::TestAppContext;
        let rt = tokio::runtime::Runtime::new().expect("a runtime");
        let env = crate::kit::PanelEnv::from_process(crate::kit::Host::Gallery, rt.handle().clone(), (800.0, 600.0));
        let mut app = TestAppContext::single();
        let cx = app.add_empty_window();
        let built = cx.update(|window, cx| build_with(&env, None, window, cx)).expect("builds");
        let root = built.view.clone().downcast::<MbtilesRoot>().unwrap_or_else(|_| panic!("an MbtilesRoot"));
        let map = built.map.as_ref().and_then(|m| m.upgrade()).expect("a map");
        cx.update(|_, cx| {
            let lines: Vec<String> = root.read(cx).lines().iter().map(|l| l.to_string()).collect();
            assert!(lines[0].contains("bundled"), "{lines:?}");
            assert!(lines.iter().any(|l| l.contains("name: Three cities")), "{lines:?}");
            assert!(lines.iter().any(|l| l.starts_with("format: pbf  zooms: 14-14")), "{lines:?}");
            assert!(lines.iter().any(|l| l.starts_with("bounds: ")), "{lines:?}");
            let map = map.read(cx);
            assert!(map.attributions().iter().any(|c| c.text.contains("OpenStreetMap")), "the file's credit");
        });
        env.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// A standalone window (`cargo run --example mbtiles`) draws the card
    /// above the map too: without `set_chrome_above_map(true)` the map's
    /// Metal layer composites over every gpui element and the card is hidden.
    #[test]
    fn a_standalone_window_puts_the_card_above_the_map() {
        use gpui::TestAppContext;
        let rt = tokio::runtime::Runtime::new().expect("a runtime");
        let host = crate::kit::Host::Standalone { title: "mbtiles".into() };
        let env = crate::kit::PanelEnv::from_process(host, rt.handle().clone(), (800.0, 600.0));
        let mut app = TestAppContext::single();
        let cx = app.add_empty_window();
        let built = cx.update(|window, cx| super::build_with(&env, None, window, cx)).expect("builds");
        let map = built.map.as_ref().and_then(|m| m.upgrade()).expect("a map");
        cx.update(|_, cx| assert!(map.read(cx).chrome_above_map(), "the standalone map hides the card"));
        env.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

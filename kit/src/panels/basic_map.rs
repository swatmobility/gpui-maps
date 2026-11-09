//! OpenFreeMap Liberty with an optional imagery basemap. Shared by
//! `examples/basic_map.rs` and the gallery (`basic`). The environment
//! variables, the imagery choice and the credits are documented on
//! `examples/basic_map.rs`'s module doc.
//!
//! `#![allow(dead_code)]`: this file is included by two crates (the
//! standalone example and the gallery) that each use a different half of
//! it, so some items are dead in one of them.
#![allow(dead_code)]

use std::sync::Arc;

use gpui::{App, AppContext as _, Window};
use gpui_maps::map_view::MapView;
use gpui_maps::tile_source::http::HttpTileSource;
use gpui_maps::tile_source::TileSource;

use crate::{kit, shell};

pub const ID: &str = "basic";

/// What the gallery checks before opening this panel. `build` never needs a
/// token -- with none it falls back to Sentinel-2 cloudless imagery (see
/// [`default_satellite_source`]) -- so a missing token is a [`kit::Status::Fallback`],
/// not a [`kit::Status::Missing`], and the panel switches to with a banner
/// rather than the missing-data card.
pub fn requirements(_env: &kit::PanelEnv) -> Vec<kit::Requirement> {
    vec![kit::Requirement::Network, kit::Requirement::MapboxToken { fallback: Some("Sentinel-2 cloudless imagery") }]
}

/// Every dataset this panel can draw: OpenFreeMap's vector basemap, Natural
/// Earth's public-domain relief, EOX's CC BY 4.0 (2016) satellite imagery, and
/// Mapbox's (only credited when a token switches imagery to it).
pub const CREDITS: &[kit::Credit] =
    &[kit::CREDIT_OSM_OPENFREEMAP, kit::CREDIT_NATURAL_EARTH, kit::CREDIT_S2CLOUDLESS, kit::CREDIT_MAPBOX];

/// The style file the panel opens with: `examples/satellite-style.json`
/// for `GPUI_MAP_STYLE=satellite`, Liberty otherwise.
pub fn style_file() -> &'static str {
    if satellite_style() {
        "examples/satellite-style.json"
    } else {
        "examples/liberty-style.json"
    }
}

fn satellite_style() -> bool {
    matches!(std::env::var("GPUI_MAP_STYLE").as_deref(), Ok("satellite"))
}

/// The standalone example's up-front check of everything `build` would
/// otherwise panic on: both styles compile, `GPUI_MAP_RASTER` names a known
/// source, and `mapbox-satellite` has a token. Called from `main`, before
/// `Application::run`.
pub fn preflight(env: &kit::PanelEnv) -> anyhow::Result<()> {
    kit::compile_style("examples/satellite-style.json")?;
    kit::compile_style("examples/liberty-style.json")?;
    match std::env::var("GPUI_MAP_RASTER").as_deref() {
        Err(_) | Ok("none" | "natural-earth" | "s2cloudless") => Ok(()),
        Ok("mapbox-satellite") if env.mapbox_token.is_some() => Ok(()),
        Ok("mapbox-satellite") => {
            anyhow::bail!("GPUI_MAP_RASTER=mapbox-satellite needs GPUI_MAP_ALLOW_MAPBOX=1 and MAPBOX_ACCESS_TOKEN (a public `pk.` token)")
        }
        Ok(other) => anyhow::bail!(
            "GPUI_MAP_RASTER={other:?} is not one of: mapbox-satellite, s2cloudless, natural-earth, none"
        ),
    }
}

pub fn build(env: &kit::PanelEnv, _window: &mut Window, cx: &mut App) -> anyhow::Result<kit::Built> {
    // Which vendored style to open with. Unset is Liberty; `satellite` is
    // the imagery-only document. BOTH are compiled, always, because `S`
    // switches between them rather than merely adding and removing a
    // raster layer -- see `shell::Satellite::styles`.
    let satellite_style = satellite_style();
    let imagery_style = kit::compile_style("examples/satellite-style.json")?;
    let vector_style = kit::compile_style("examples/liberty-style.json")?;
    eprintln!("style: {} (S switches)", style_file());
    let style = if satellite_style { imagery_style.clone() } else { vector_style.clone() };
    // The imagery basemap, defaulted PER STYLE: the imagery style wants
    // Mapbox satellite (s2cloudless without a token), Liberty wants the
    // Natural Earth relief its own raster layer names and caps at zoom 7.
    let strict = !env.host.is_gallery();
    let token = env.mapbox_token.as_deref();
    let raster_default = if satellite_style { default_satellite_source(token.is_some()) } else { "natural-earth" };
    let raster_name = std::env::var("GPUI_MAP_RASTER").unwrap_or_else(|_| raster_default.to_string());
    let satellite = raster_source(&raster_name, token, strict);
    // ON from the first frame for the imagery style, whose whole point is
    // the imagery; OFF for Liberty, where `s` asks for it.
    let on_at_launch = satellite.is_some() && satellite_style;
    if let Some((_, _, name)) = satellite.as_ref() {
        eprintln!(
            "imagery: {name} ({})",
            if on_at_launch { "on -- press S to switch off" } else { "off -- press S" }
        );
    }
    let sprite = style.sprite.clone();
    let (lon, lat, zoom) = camera_from_env();
    let (w, h) = env.viewport;
    let source = kit::openfreemap_planet();
    let runtime = env.runtime.clone();
    let gallery = env.host.is_gallery();
    // Chrome always draws above the map inside the gallery (its native
    // layer sits below gpui's own paint, see `main.rs`'s `AppRoot`), and
    // can be forced on standalone too (`GPUI_MAP_CHROME_ABOVE=1`) to
    // measure that cost on its own -- `scripts/gallery-parity.sh`'s Step 7
    // uses this to split "chrome above the map" from the gallery's own
    // overhead when a parity run disagrees.
    let chrome_above = gallery || std::env::var("GPUI_MAP_CHROME_ABOVE").as_deref() == Ok("1");
    // The JSON the launch style was compiled from, kept on the view for
    // `MapView::scene_snapshot` (the agent's `capture.image` of the current
    // scene renders a story from it).
    let document: Option<serde_json::Value> =
        kit::asset_bytes(style_file()).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let map = cx.new(|_| {
        let mut view = MapView::new(lon, lat, zoom, w, h, style, source, runtime);
        if let Some(document) = document {
            view.set_style_document(document);
        }
        view.set_basemap_name(Some("openfreemap"));
        if chrome_above {
            view.set_chrome_above_map(true);
        }
        view
    });
    // The basemap's icons: route shields, one-way arrows, POI markers.
    // OpenFreeMap's sheet needs no key. Fetched off the UI thread; see
    // `kit::spawn_sprites`.
    if let Some(sprite) = sprite {
        kit::spawn_sprites(map.downgrade(), sprite, None, env.runtime.clone(), cx);
    }
    let sat = satellite.map(|(source, tile_size, name)| {
        shell::Satellite::new(source, tile_size, name, on_at_launch).with_styles(imagery_style, vector_style)
    });
    let root = match &env.host {
        kit::Host::Standalone { title } => {
            let title = title.clone();
            cx.new(|cx| shell::DemoShell::with_satellite(map.clone(), title, sat, cx))
        }
        kit::Host::Gallery => cx.new(|cx| shell::DemoShell::embedded(map.clone(), sat, cx)),
    };
    Ok(kit::Built { view: root.into(), map: Some(map.downgrade()), start: (lon, lat, zoom) })
}

/// The camera this example opens at: Singapore, zoom 12 -- roads, water,
/// land and landuse detail are dense here (10k+ transportation features in
/// a single z12 tile) -- unless `GPUI_MAP_LON`/`LAT`/`ZOOM` override it.
///
/// The override is the same one `mapbox_standard_dark.rs` carries and
/// exists for the same reason: a camera that has to be driven to a
/// specific zoom (the flat Mercator raster path above zoom 12, say)
/// without recompiling. The one reading of those variables is
/// [`kit::camera_from_env`].
fn camera_from_env() -> (f64, f64, f64) {
    kit::camera_from_env((103.8198, 1.3521, 12.0))
}

/// Natural Earth II shaded relief, served by OpenFreeMap -- the very
/// source `examples/liberty-style.json`'s `ne2_shaded` names.
///
/// **Public domain**, so a frame rendered from it can be committed as
/// evidence. Capped at z6 by the tileset.
pub const NATURAL_EARTH: &str = "https://tiles.openfreemap.org/natural_earth/ne2sr/{z}/{x}/{y}.png";
pub const NATURAL_EARTH_ATTRIBUTION: &str =
    r#"<a href="https://www.naturalearthdata.com/" target="_blank">Natural Earth</a>"#;

/// Sentinel-2 cloudless by EOX, the **2016** layer, **CC BY 4.0** (every later layer is CC BY-NC-SA, which a commercial product cannot ship) -- the satellite basemap
/// this example exists for, and an openly-licensed one that needs no
/// token.
///
/// Note `{z}/{y}/{x}`, not `{z}/{x}/{y}`: this is a WMTS endpoint and WMTS
/// orders its path row-then-column. `HttpTileSource::url_for` substitutes
/// the three names wherever they appear, so the template expresses it
/// directly and no y-flip is needed.
const S2_CLOUDLESS: &str = kit::S2CLOUDLESS_2016_URL;

/// The credit CC BY 4.0 requires, EOX's wording verbatim (see
/// `kit::S2CLOUDLESS_2016_ATTRIBUTION`), and the reason
/// `MapView::frame_attribution` exists: this line has to be on screen
/// ALONGSIDE OpenFreeMap's, not instead of it.
const S2_CLOUDLESS_ATTRIBUTION: &str = kit::S2CLOUDLESS_2016_ATTRIBUTION;

const MAPBOX_ATTRIBUTION: &str = concat!(
    r#"<a href="https://www.mapbox.com/about/maps" target="_blank">&copy; Mapbox</a> "#,
    r#"<a href="https://www.openstreetmap.org/copyright/" target="_blank">&copy; OpenStreetMap</a>"#,
);

/// Which imagery source the satellite style opens with: Mapbox satellite
/// when a token is present, s2cloudless when it is not.
///
/// **The fallback is not an error path.** The whole point of an openly
/// licensed default is that a machine with no credentials still gets a
/// working satellite map, so the absence of a token is announced once and
/// the app carries on. Failing to start would turn a licence convenience
/// into a hard dependency.
///
/// Only asks whether a token exists (`kit::PanelEnv::mapbox_token`, which
/// reads the environment and then `~/.config/gpui-map/mapbox.env`). The
/// value goes straight into `HttpTileSource` in `raster_source` -- it is
/// never printed, logged or written.
fn default_satellite_source(has_token: bool) -> &'static str {
    if has_token {
        return "mapbox-satellite";
    }
    eprintln!(
        "gpui-maps: no Mapbox token (MAPBOX_ACCESS_TOKEN is not set, and \
         ~/.config/gpui-map/mapbox.env has none or was refused) -- falling back to Sentinel-2 cloudless \
         (EOX, the 2016 layer, CC BY 4.0), which needs no token. Set either for Mapbox satellite, \
         which resolves far more at city zoom, or pick one explicitly with GPUI_MAP_RASTER."
    );
    "s2cloudless"
}

/// The imagery source `GPUI_MAP_RASTER` names, its tile size, and its
/// display name -- or `None` for `none`.
///
/// The same three sources `examples/globe_capture.rs` offers, declared
/// here rather than shared with it because that harness is a binary of its
/// own with no library surface between them, and two copies of four
/// constants beat an `examples/common/` module that exists to hold four
/// constants.
///
/// The tile SIZE is per source and is checked, not assumed:
/// `raster::decode_tile` refuses a tile of any other size rather than
/// stretching it (see its doc comment). 512 for Natural Earth, 256 for
/// both satellite sources.
///
/// `strict` is the standalone example: a bad `GPUI_MAP_RASTER` or a
/// `mapbox-satellite` with no token panics there, as it always did. Inside
/// the gallery (`strict == false`) the same cases print a line and fall
/// back to no imagery (or s2cloudless), because one panel must not take
/// the whole gallery down.
fn raster_source(
    name: &str,
    token: Option<&str>,
    strict: bool,
) -> Option<(Arc<dyn TileSource>, u32, &'static str)> {
    match name {
        "none" => None,
        "natural-earth" => Some((
            Arc::new(kit::with_tile_cache(
                HttpTileSource::new(NATURAL_EARTH, 6, NATURAL_EARTH_ATTRIBUTION),
                kit::Licence::PublicDomain,
            )),
            512,
            "natural-earth",
        )),
        "s2cloudless" => Some((
            // The cache is keyed on the full URL, so the 2016 layer's
            // entries can never be served for another year's.
            Arc::new(kit::with_tile_cache(
                HttpTileSource::new(S2_CLOUDLESS, 14, S2_CLOUDLESS_ATTRIBUTION),
                kit::Licence::CcBy4,
            )),
            256,
            "s2cloudless",
        )),
        "mapbox-satellite" => {
            // Eyeballing only. Mapbox imagery is licensed and a frame
            // rendered from it is NOT committable evidence -- which is why
            // the two openly-licensed sources above exist and this one has
            // to be asked for by name.
            //
            // The token is read here and never printed: it goes straight
            // into `HttpTileSource`, which is the only thing that ever
            // sees the assembled url.
            let Some(token) = token else {
                if strict {
                    panic!("GPUI_MAP_RASTER=mapbox-satellite needs GPUI_MAP_ALLOW_MAPBOX=1 and MAPBOX_ACCESS_TOKEN (a public `pk.` token)")
                }
                return raster_source(default_satellite_source(false), None, strict);
            };
            // `@2x`, and it is not cosmetic: the plain endpoint serves
            // 256px tiles, which this engine's 512-logical-pixel grid
            // stretches to half the linear resolution. `@2x` returns
            // 512x512 for the same tile, the same request count and the
            // same tile count -- double the linear resolution for
            // nothing. Declared as 512 below, which is what it decodes
            // to; `raster::decode_tile` checks that rather than trusting
            // it.
            let url = format!(
                "https://api.mapbox.com/v4/mapbox.satellite/{{z}}/{{x}}/{{y}}@2x.jpg?access_token={token}"
            );
            Some((
                // Not cached unless GPUI_MAP_TILE_CACHE=on: these bytes
                // are licensed data, and caching them is the developer's
                // call under Mapbox's terms. Never under the working tree
                // either way.
                Arc::new(kit::with_tile_cache(HttpTileSource::new(url, 22, MAPBOX_ATTRIBUTION), kit::Licence::MapboxTos)),
                512,
                "mapbox-satellite",
            ))
        }
        other if strict => panic!(
            "GPUI_MAP_RASTER={other:?} is not one of: mapbox-satellite, s2cloudless, \
             natural-earth, none"
        ),
        other => {
            eprintln!(
                "gpui-maps: GPUI_MAP_RASTER={other:?} is not one of: mapbox-satellite, s2cloudless, \
                 natural-earth, none -- no imagery"
            );
            None
        }
    }
}

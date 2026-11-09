//! The styles this build carries, and the order the switcher cycles them.
//!
//! Both are EMBEDDED rather than fetched or bundled, for the reason
//! `app::MapApp::new` already gave for carrying one: a bundle resource would
//! mean `CFBundleGetMainBundle` + a `.app`-relative path, which is real UIKit
//! FFI for no new knowledge, and 210 KB inside the binary costs nothing. A
//! shipping app would put them in the bundle (or fetch them) -- both are a
//! path change here and nothing else, because the engine takes `&[u8]`.
//!
//! # Why these three and not the other five
//!
//! The constraint is not taste, it is SCHEMA. `app::TILE_URL` serves
//! OpenMapTiles, and a style whose `source-layer` names come from a different
//! vocabulary compiles perfectly and then draws nothing: `style::match_tile`
//! matches by name, finds no layer called `road` in a tile that calls it
//! `transportation`, and emits zero paint ops.
//!
//! ```text
//!                                     schema      layers  alive  dead
//! liberty-style.json                  OMT            111    109     0
//! styles-open/open-streets-style.json OMT            111    109     0
//! mapbox-style.json (now out of tree)  OMT             52     51     0
//! swat-biz-light-style.json           Streets v8     123     11   111
//! swat-routes-dark-style.json         Streets v8      50      5    44
//! mapbox-standard-dark-style.json     Streets v8      88     11    76
//! mapbox-streets-style.json           Streets v8     134     11   122
//! mapbox-standard-dark-reference-*    an `imports:` stub, 0 layers
//! ```
//!
//! Exactly these bind against OpenFreeMap (open-streets is Liberty's layers
//! recoloured, so it binds as Liberty does). Another OMT style used to sit
//! here -- 101 layers, 100 alive -- and left with the demo it belonged to
//! when that was split into its own repository; nothing about the schema
//! argument changed with it.
//!
//! # The Mapbox-schema set
//!
//! The four Streets v8 files needed Mapbox TILES as well as a token, which
//! was a `TILE_URL` change and a different job -- that job is now done (see
//! `app::mapbox_tile_url_template` and `app::TileSchema`). [`MAPBOX_STYLES`]
//! is that set's own cycle: styles whose `source-layer` names come from the
//! Mapbox `composite` vector source, not OpenMapTiles. It is used only when
//! `MAPBOX_ACCESS_TOKEN` was baked in at build time (`option_env!` in
//! `app.rs`) and the `composite` source URL could be derived from the
//! vendored fixture; otherwise the app falls back to [`STYLES`] and says so
//! in the startup log. `app::TileSchema` is what picks between the two sets
//! and their matching tile source -- a style from one set is never compiled
//! against the other set's tile URL.
//!
//! # What the switcher is FOR
//!
//! Not "pick the best one for the user" -- that guess was made once already
//! and it is the guess this file exists to stop making. The two differ in
//! exactly the dimension the complaint was about (saturation), and a phone in
//! the hand comparing them settles it in ten seconds where a paint-op table
//! cannot.

use std::sync::OnceLock;

/// One entry in the cycle: a name for the log and the compiled bytes.
///
/// No sprite sheet field, deliberately -- and no longer because the field
/// would be inert. Every one of these styles names a `sprite` URL and a
/// `glyphs` template. `glyphs` stays unread: text is shaped locally, from
/// `app::EMBEDDED_FONT`, not from a fetched SDF glyph PBF -- no shell in
/// this repository reads a style's `glyphs` field, native and the browser
/// included. `sprite` is now READ, just not through this struct: every
/// style in [`STYLES`] draws icons from `app::EMBEDDED_SPRITE_JSON`/
/// `EMBEDDED_SPRITE_PNG`, the bundled OMT-vocabulary sheet installed
/// unconditionally in `app::MapApp::create_renderer` regardless of what
/// (if anything) a given style's own `"sprite"` field names -- both entries
/// here have `icon-image` expressions in the OMT vocabulary
/// (`default-N`/`road_N`) the bundled sheet covers. [`MAPBOX_STYLES`]'
/// single entry additionally fetches its OWN sprite sheet at runtime, over
/// `net::Fetcher`, because its `"sprite"` names Mapbox art the bundled
/// sheet does not carry -- see `app::MapApp::try_install_mapbox_sprites`.
pub struct StyleChoice {
    /// Printed to the device log on every switch. Short enough to read at a
    /// glance in a `devicectl` stream that is also carrying touch and frame
    /// lines.
    pub name: &'static str,
    pub json: &'static [u8],
    /// One line on what makes this one different, printed with the name so
    /// the log says what the eye is being asked to compare.
    pub note: &'static str,
}

/// Cycled in order by a three-finger touch; index 0 is what launches.
///
/// `open-streets` leads (2026-10, Mapbox off by default): our own
/// Mapbox Streets look-alike over the OpenMapTiles vocabulary, built from
/// OpenFreeMap Liberty (`examples/styles-open/README.md`). Liberty follows
/// because it is the one the user named. (`mapbox-style.json`, a
/// Mapbox-authored OMT style, sat between them until it left the public
/// tree.)
pub const STYLES: &[StyleChoice] = &[
    StyleChoice {
        name: "open-streets",
        json: include_bytes!("../../examples/styles-open/open-streets-style.json"),
        note: "Streets (open): our Mapbox Streets look-alike, OpenFreeMap Liberty recoloured; the keyless default",
    },
    StyleChoice {
        name: "liberty",
        json: LIBERTY,
        note: "OSM Liberty; pale by design -- #f8f4f0 ground, near-white landuse, white roads",
    },
    // The dark set, the same three `gpui_maps::basemaps` offers natively
    // (`Basemap::SwatDark`, `OpenDark`, `Dark`), on the same OpenFreeMap
    // planet source -- what a transit app wants under its own lines.
    StyleChoice {
        name: "swat-dark",
        json: include_bytes!("../../examples/styles-open/open-swat-routes-dark-style.json"),
        note: "Graphite: the dark base transit routes are drawn over",
    },
    StyleChoice {
        name: "open-dark",
        json: include_bytes!("../../examples/styles-open/open-dark-style.json"),
        note: "Dark Grey (open): our dark look-alike over OpenMapTiles",
    },
    // The desktop Gallery's "Light" card (`kit/src/panels/open_basemaps.rs`):
    // Graphite's light twin, a calm base for data.
    StyleChoice {
        name: "open-light",
        json: include_bytes!("../../examples/styles-open/open-light-style.json"),
        note: "Light (open): Graphite's light twin, low saturation",
    },
    // The desktop Gallery's "Terrain" card: shaded relief (the style's own
    // raster source, OpenFreeMap's Natural Earth relief) under a light base.
    StyleChoice {
        name: "open-terrain",
        json: include_bytes!("../../examples/styles-open/open-terrain-style.json"),
        note: "Terrain (open): shaded relief under a light base",
    },
    // The desktop Gallery's "Satellite" card: labels and boundaries only,
    // over the imagery the host sets (`MapApp::set_raster_override`; the
    // style names no tiles).
    StyleChoice {
        name: "satellite",
        json: include_bytes!("../../examples/satellite-style.json"),
        note: "Satellite: place labels over the host's imagery",
    },
    // The desktop Gallery's "PMTiles (remote)" card: Protomaps' Florence
    // demo archive under the vendored Protomaps basemap style, read by
    // range requests (`style_source`, `pmtiles_http`).
    StyleChoice {
        name: "pmtiles-florence",
        json: include_bytes!("../../examples/protomaps-basemap-style.json"),
        note: "PMTiles: Protomaps' Florence archive by range requests",
    },
    // Gallery cards whose map is a basemap with the host's raster OVER
    // its fills ([`CARD_RASTERS`]): the COG card (`kit/src/panels/cog.rs`
    // `cog_style`, Liberty) and two Kepler samples (`examples/panels/kepler.rs`,
    // the dark basemap).
    StyleChoice { name: LIBERTY_COG, json: LIBERTY, note: "Liberty with a COG over its fills (the Gallery's COG card)" },
    StyleChoice { name: DARK_LUCERNE, json: DARK_MATTER, note: "Dark Matter under a historic raster PMTiles map (the Lake Lucerne card)" },
    StyleChoice { name: DARK_NCEP, json: DARK_MATTER, note: "Dark Matter under NOAA's radar WMS (the NCEP card)" },
    StyleChoice {
        name: "dark-matter",
        json: DARK_MATTER,
        note: "OpenFreeMap Dark (Dark Matter)",
    },
];

const LIBERTY: &[u8] = include_bytes!("../../examples/liberty-style.json");
const DARK_MATTER: &[u8] = include_bytes!("../../demos/openfreemap-dark-style.json");

/// The COG card's map: Liberty, its raster layer the host's COG.
pub const LIBERTY_COG: &str = "liberty-cog";
/// The Lake Lucerne card's map: the dark basemap under the archive's tiles.
pub const DARK_LUCERNE: &str = "dark-lucerne";
/// The NCEP radar card's map: the dark basemap under the WMS, at 0.8.
pub const DARK_NCEP: &str = "dark-ncep";

/// The styles that draw the host's raster over the basemap, and the
/// opacity of the one raster layer each draws it with: the desktop card's
/// (`cog_style`'s opaque layer; Kepler's layer opacity).
pub const CARD_RASTERS: &[(&str, f32)] = &[(LIBERTY_COG, 1.0), (DARK_LUCERNE, 1.0), (DARK_NCEP, 0.8)];

impl StyleChoice {
    /// The style document this choice compiles: [`Self::json`], except a
    /// [`CARD_RASTERS`] style, whose raster layers are replaced by one that
    /// draws the host's imagery at the card's opacity at every zoom (as
    /// the desktop's `cog::cog_style` replaces Liberty's faded relief).
    /// Built once per style.
    pub fn style_json(&self) -> &'static [u8] {
        let Some(&(_, opacity)) = CARD_RASTERS.iter().find(|(n, _)| *n == self.name) else { return self.json };
        static PATCHED: OnceLock<std::sync::Mutex<std::collections::HashMap<&'static str, &'static [u8]>>> = OnceLock::new();
        let mut map = PATCHED.get_or_init(Default::default).lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let json = self.json;
        map.entry(self.name).or_insert_with(|| card_raster_style(json, opacity).map_or(json, |v| &*Box::leak(v.into_boxed_slice())))
    }
}

/// `json` with its raster layers replaced by one, `card-raster`, over a
/// raster source with no tiles of its own (the host sets them), drawn at
/// `opacity` at every zoom.
fn card_raster_style(json: &[u8], opacity: f32) -> Option<Vec<u8>> {
    let mut doc: serde_json::Value = serde_json::from_slice(json).ok()?;
    doc.get_mut("sources")?.as_object_mut()?.insert("card-raster".into(), serde_json::json!({ "type": "raster", "tiles": [], "tileSize": 256 }));
    let layers = doc.get_mut("layers")?.as_array_mut()?;
    layers.retain(|l| l["type"] != "raster");
    // Its place in the list does not matter to the drawing (the raster is
    // drawn over the basemap's tiles, [`raster_above_basemap`]); first, as
    // a style editor would show the backmost layer.
    let at = usize::from(layers.first().is_some_and(|l| l["type"] == "background"));
    layers.insert(at, serde_json::json!({ "id": "card-raster", "type": "raster", "source": "card-raster", "paint": { "raster-opacity": opacity } }));
    serde_json::to_vec(&doc).ok()
}

/// Whether style `name` draws the host's raster OVER the basemap's fills
/// and lines (and under the labels, buildings and overlays) rather than
/// under every vector layer: the desktop's `set_raster_above_basemap`,
/// which these cards turn on so the map shows outside the data and
/// through its transparent texels.
pub fn raster_above_basemap(name: &str) -> bool {
    CARD_RASTERS.iter().any(|(n, _)| *n == name)
}

/// The Mapbox-schema cycle: only reachable when `GPUI_MAP_ALLOW_MAPBOX=1`
/// (Mapbox is off by default) and `MAPBOX_ACCESS_TOKEN` was
/// present at build time (see `app::TileSchema::resolve`). One entry today
/// -- `mapbox-streets-style.json`, 134 layers over the real `composite`
/// source -- which is the desktop reference (`examples/mapbox_standard_dark.rs`
/// with `GPUI_MAP_STYLE=streets`) and the style this task exists to match.
/// A single-entry cycle is a deliberate, harmless degenerate case of the
/// same switcher: three fingers still recompiles and reclears, it just
/// lands back on index 0.
pub const MAPBOX_STYLES: &[StyleChoice] = &[StyleChoice {
    name: "mapbox-streets-v8",
    // Staged by `build.rs` from `$MAPBOX_FIXTURES`; EMPTY when that is unset,
    // which makes `TileSchema::resolve` fall back to OpenFreeMap.
    json: include_bytes!(concat!(env!("OUT_DIR"), "/mapbox-streets-style.json")),
    note: "Streets v8 over the real Mapbox composite source -- the desktop reference style",
}];

/// A style whose vector tiles do not come from the schema's own source
/// (OpenFreeMap's planet): its tile template, the source's highest zoom
/// until the archive says, and the credit the basemap owes instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StyleSource {
    pub tiles: &'static str,
    /// The archive the tiles come from (its header gives the true ceiling).
    pub archive: &'static str,
    pub max_zoom: u8,
    pub credit: &'static str,
}

/// The desktop's `tile_source::pmtiles::DEMO_ARCHIVE_URL` (the gallery's
/// `pmtiles-remote` card always opens it).
pub const DEMO_ARCHIVE_URL: &str = "https://pmtiles.io/protomaps(vector)ODbL_firenze.pmtiles";

/// [`StyleSource`] of style `name`, if it has its own.
pub fn style_source(name: &str) -> Option<StyleSource> {
    match name {
        "pmtiles-florence" => Some(StyleSource {
            tiles: concat!("pmtiles+", "https://pmtiles.io/protomaps(vector)ODbL_firenze.pmtiles", "/{z}/{x}/{y}"),
            archive: DEMO_ARCHIVE_URL,
            max_zoom: 15,
            credit: "Protomaps \u{a9} OpenStreetMap contributors",
        }),
        _ => None,
    }
}

//! The browser half: an interactive, multi-tile map.
//!
//! Everything here is plumbing -- fetch, canvas, surface, events, one draw
//! call per frame. The map itself is four engine calls, and they are the same
//! four a native `MapView` makes:
//!
//! ```text
//! style_spec::parse_and_compile(bytes)      -> style::Style
//! mvt::decode(bytes)                        -> mvt::DecodedTile
//! style::match_tile(&tile, &style, zoom)    -> style::PaintPlan
//! render::tessellate_plan(&plan, 4096, ...) -> render::TileMesh
//! ```
//!
//! and the draw is `render::gpu::TilePipeline`, running the crate's own
//! `src/render/shader.wgsl`. No triangle on the page was produced by anything
//! in this file.
//!
//! # What came from `map_view.rs`, and what did not
//!
//! `map_view.rs` is 13,566 lines and is not coming to the browser. The
//! interaction below therefore needed a decision per piece. The full survey
//! is in the spec; the short version, marked at each site as `[REUSED]`,
//! `[LIFTED]` or `[NOT PORTED]`:
//!
//! * the tile cover, mercator, the projection, the per-tile model matrix,
//!   the unprojection under the cursor -- all already in `geo`/`render`,
//!   used unchanged;
//! * the camera -- pan, zoom ease, rotate, tilt, north snap, `ease_to` --
//!   is `crate::camera_control::CameraController`, the one native and iOS
//!   drive too; this shell only turns DOM input into `CameraIntent`s;
//! * the eight tile caches and the off-thread bake -- deliberately NOT
//!   brought over, the honest size of what phase 2 has left to do.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

use crate::camera_control::{CameraController, CameraIntent, CameraTarget, RotateSamples, MAX_ZOOM, MIN_ZOOM};
use crate::geo::TileId;
use crate::mvt::DecodedTile;
use crate::render::gpu::RasterDraws;
use crate::render::icon_pipeline::{pack_icon_atlas, IconInstance, IconPipeline, IconRect};
use crate::render::raster_mesh::RasterMeshKey;
use crate::render::raster_pipeline::{RasterBuffers, RasterPipeline};
use crate::render::{alloc::TrackedBuffer, Camera, GpuContext, TileBuffers, TilePipeline};
use crate::style::{Lerp, Style};
use crate::touch_gesture::{pointer_distance, TwoFingerGesture};

// `?census=<pack>`: the US census income choropleth and its hover popup.
mod census_page;
mod markers_api;

/// The MVT extent every `PaintOp`'s geometry is in, and the divisor
/// `tile_model_matrix` uses to scale a tile back to world units. OpenFreeMap
/// encodes at 4096, the OpenMapTiles default.
const TILE_EXTENT: f32 = 4096.0;

/// [`TILE_EXTENT`], as the `u32` `render::raster_mesh::build` takes --
/// native's `MVT_EXTENT` (`src/mvt.rs`). A raster tile's square is cut in
/// the same coordinate space the vector fills are, so the two must share
/// one extent; kept as its own named constant rather than an inline `as
/// u32` cast at the one call site so the two stay visibly the same number.
const RASTER_MESH_EXTENT: u32 = 4096;

/// The default tile endpoint: OpenFreeMap's planet build, public and
/// keyless -- the same source `examples/basic_map.rs` draws from natively,
/// through the one shared constant ([`crate::basemap::OPENFREEMAP_PLANET_TILE_URL`])
/// so the two cannot drift onto different dated paths.
///
/// Overridable with `?tiles=<template>` -- so the SAME wasm binary can be
/// pointed at a self-hosted OMT server, a Mapbox composite endpoint, or any
/// other `{z}/{x}/{y}` tile source and compared without a rebuild. That is
/// also how this page is meant to be pointed at a private or licensed
/// corpus for local development: never bake such a source in as the
/// default, since this repository -- and this default -- are public.
const TILE_URL: &str = crate::basemap::OPENFREEMAP_PLANET_TILE_URL;

/// The ACTIVE source's zoom ceiling -- [`TILE_URL`]'s by default, or
/// whatever `?tiles=` repointed [`tile_template`] to.
///
/// This used to be a `const SOURCE_MAX_ZOOM: u8 = 14`, which is right for
/// every OpenMapTiles-schema source (OpenFreeMap's planet build included)
/// and silently wrong for any `?tiles=` override that names a deeper one: a
/// Mapbox composite endpoint (`?tiles=https://api.mapbox.com/v4/...`) goes
/// two levels deeper, to z16, and its style thins dense layers out of
/// anything shallower -- a build pinned to 14 never fetches the tile that
/// has them. See `crate::source_zoom`'s module doc for the measurement (a
/// Manhattan `building`-layer comparison across z13-z16) and the report
/// this closes.
///
/// Read fresh every time rather than cached, because `?tiles=` is read
/// once before startup (see [`tile_template`]'s own doc) but nothing here
/// should have to remember that fact -- a future runtime source switch (the
/// style dropdown does not do this today; see `index.html`'s own note)
/// picks this up for free.
fn source_max_zoom() -> u8 {
    crate::source_zoom::max_zoom_for_tile_url(&tile_template())
}

/// Served by the local static server beside `index.html` -- it is
/// `examples/styles-open/open-streets-style.json` ("Streets (open)": our
/// Mapbox Streets look-alike, OpenFreeMap Liberty recoloured, with
/// OpenFreeMap's own sprite), copied verbatim by `build.sh`. It replaced
/// `examples/mapbox-style.json` as the keyless default in 2026-10 (Mapbox
/// is off by default); that one is still in the picker. Fetched
/// rather than `include_str!`d deliberately: embedding it would have hidden
/// the question of where a browser gets its style from.
///
/// Mapbox Streets' idiom over the OpenMapTiles schema, which is exactly what
/// the default endpoint ([`TILE_URL`]) emits (`water`, `landcover`,
/// `landuse`, `transportation`, `transportation_name`, `building`, `place`,
/// `boundary`, `waterway`). Overridable with `?style=<url>`.
const STYLE_URL: &str = "./open-streets-style.json";

/// `examples/mapbox-streets-style.json`, the style native's
/// `examples/mapbox_streets_demo.rs` and iOS's `MAPBOX_STYLES` draw -- over
/// the real Mapbox `composite` source rather than OpenMapTiles tiles.
/// `build.sh` copies it into `dist/` ONLY when `MAPBOX_ACCESS_TOKEN` is set
/// for the build, and it is the default style exactly when a token is
/// available (see [`mapbox_token`] and `crate::basemap::startup_style_url`).
const MAPBOX_STYLE_URL: &str = "./mapbox-streets-style.json";

/// Baked in at BUILD time, the way `ios/src/app.rs` bakes its own:
/// `option_env!` resolves against the compiler's environment, so the value
/// exists only in the `dist/` wasm a `MAPBOX_ACCESS_TOKEN="$TOK"
/// ./web/build.sh` produced -- `dist/` is gitignored, and no tracked file
/// ever carries it. `None` for every build that does not export it, which
/// is the public build, whose behaviour is then exactly what it was.
///
/// A `pk.` token is a PUBLIC client token by Mapbox's design, but a dist
/// built with one hands it to everyone who can load the page. Serve such a
/// dist only where that is acceptable.
const BAKED_MAPBOX_ACCESS_TOKEN: Option<&str> = option_env!("MAPBOX_ACCESS_TOKEN");

/// The build-time opt-in, `GPUI_MAP_ALLOW_MAPBOX=1` -- the same switch
/// `examples/common/kit.rs::MAPBOX_ALLOW_VAR` reads at run time natively.
const MAPBOX_ALLOWED: bool = match option_env!("GPUI_MAP_ALLOW_MAPBOX") {
    Some(v) => matches!(v.as_bytes(), b"1"),
    None => false,
};

/// The Mapbox token in force: a run-time `?access_token=` (for local
/// testing against a tokenless dist, or to try another token), else the
/// build-time one. Blank is none (`crate::basemap::usable_token`). Never
/// logged: every string that could carry it goes through [`log`] or
/// [`set_text`], which redact it.
///
/// Gated: Mapbox is OFF (2026-10, a usage spike on the key) unless the build
/// was made with `GPUI_MAP_ALLOW_MAPBOX=1` ([`MAPBOX_ALLOWED`]). Without it
/// neither the baked token nor `?access_token=` is used, and the page draws
/// its keyless OpenFreeMap basemap. `web/build.sh` bakes no token without
/// the opt-in either.
fn mapbox_token() -> Option<String> {
    if !MAPBOX_ALLOWED {
        return None;
    }
    let query = query_param("access_token");
    crate::basemap::usable_token(query.as_deref())
        .or_else(|| crate::basemap::usable_token(BAKED_MAPBOX_ACCESS_TOKEN))
        .map(str::to_string)
}

/// The FALLBACK sprite sheet's BASE url -- `sprite::sprite_urls`' convention,
/// so the two files actually fetched are `<base>@2x.json` and
/// `<base>@2x.png`. Reached by [`sprite_base`] only when the loaded style's
/// own `"sprite"` field cannot be used as-is; see that function for the full
/// resolution order. Overridable with `?sprite=<base>`, and `?sprite=none`
/// turns icons off entirely (which is what this page did before it had a
/// sheet, and is the A arm of an icon cost measurement).
///
/// # Why a style's `"sprite"` is not simply used verbatim
///
/// `examples/mapbox-style.json` declares `"sprite":
/// "mapbox://sprites/mapbox/streets-v12"`. Resolving THAT needs a Mapbox
/// access token on every request, and the art behind it is Mapbox's, which
/// this repository -- a PUBLIC one -- may neither redistribute nor bake a
/// credential into. So the sheet this page falls back to for a `mapbox://`
/// reference (or no `"sprite"` field at all) is OURS:
/// `web/sprites/build_sprites.py` generates it from route-shield geometry
/// written here plus the CC0 maki icon set, under the icon NAMES the style
/// asks for (`jp-national-route-2`, `oneway-small`, `suitcase`, ...).
/// Names are not the art. See `web/sprites/README.md`.
///
/// The consequence worth stating plainly: this is the same trade the fonts
/// took. Vendored, offline, no key, no third-party request on the critical
/// path -- and the shields are our drawing of a Japanese route marker
/// rather than Mapbox's.
///
/// A style whose `"sprite"` is already a plain `http(s)://` URL -- Liberty's
/// `https://tiles.openfreemap.org/sprites/ofm_f384/ofm` -- carries none of
/// that problem: it needs no token and this repository is redistributing
/// nothing of Mapbox's. `sprite_base` uses it directly, through the same
/// `sprite::resolve_sprite_url` the native build calls in
/// `examples/basic_map.rs`, so this fallback is never reached for Liberty.
const SPRITE_URL: &str = "./japan-sprite";

/// Where the camera opens: Tokyo Station. A dense city centre rather than
/// an arbitrary point in the ocean, so the default view has roads,
/// buildings and labels to show on first paint. `?lon=`/`?lat=`/`?z=` move
/// it.
const DEFAULT_CENTER_LON: f64 = 139.7671;
const DEFAULT_CENTER_LAT: f64 = 35.6812;
/// z14, [`TILE_URL`]'s own real-data ceiling -- see [`source_max_zoom`] and
/// `crate::source_zoom`'s module doc. A dense city centre is at its busiest
/// exactly at that ceiling: below it the generalised mid-zoom tiles carry a
/// dozen roads, at it, far more.
const DEFAULT_ZOOM: f64 = 14.0;

/// One credit line, and the language it is written in.
///
/// `lang` is not decoration: a browser picks glyph forms for the shared CJK
/// ideographs off the element's language, the same unification
/// `font_fallback::han_unification` performs for the map's own labels. An
/// untagged Japanese line in a `zh` locale renders in the wrong forms.
pub struct CreditLine {
    pub text: &'static str,
    /// BCP-47, or `None` for the document's own language.
    pub lang: Option<&'static str>,
}

/// The attribution a given tile source REQUIRES, which is a condition of
/// using it and not page furniture.
///
/// # Why this is a function of the source and not a constant
///
/// It was a constant -- two lines naming one commercial rightsholder --
/// baked into both `app.rs` and `index.html`. `?tiles=` can repoint this
/// build at any endpoint, and the moment it does, a constant credit is not
/// merely stale but false in both directions at once: pointed at OpenFreeMap
/// it asserted a commercial licence over OpenStreetMap data, and it dropped
/// the ODbL attribution that data actually requires. A page that may be
/// shown to someone else cannot carry a credit chosen by whoever compiled it
/// rather than by whatever is on screen.
///
/// The desktop already works this way -- `MapView::frame_attribution`
/// composes the credit from the sources actually in use, one line per
/// source, so a basemap plus an overlay plus terrain owes three. This is
/// the browser's smaller version of the same rule: one vector source, whose
/// credit is looked up from the template in force.
///
/// An unrecognised `?tiles=` falls through to [`UNKNOWN_SOURCE_CREDIT`],
/// which claims nothing on anyone's behalf. Saying "this build does not
/// know" is honest; naming a rightsholder this build cannot verify is not.
///
/// # Why a `?tiles=` override falls through to "unknown" rather than a guess
///
/// There was once a table entry here for a private, machine-local tile
/// stack this repository never shipped or described -- a credit table in a
/// PUBLIC repository should not carry a rightsholder's name for data that
/// repository neither distributes nor can check. It was removed rather than
/// updated when that stack's own demo moved out of this repository, and
/// nothing replaced it: an arbitrary `?tiles=` override can point anywhere,
/// and the honest default for a source this function does not recognise is
/// [`UNKNOWN_SOURCE_CREDIT`], not a guess. [`TILE_URL`] itself is always
/// recognised -- it names OpenFreeMap, so [`OPENFREEMAP_CREDIT`] is what a
/// build with no override prints.
pub fn attribution_for(template: &str) -> &'static [CreditLine] {
    // Matched on the HOST plus enough of the path to identify the corpus,
    // not on the whole template: OpenFreeMap's URL carries a dated build id
    // (`.../planet/20260802_080001_pt/...`) that changes every week.
    if template.contains("tiles.openfreemap.org") {
        OPENFREEMAP_CREDIT
    } else if template.contains("api.mapbox.com") {
        MAPBOX_CREDIT
    } else {
        UNKNOWN_SOURCE_CREDIT
    }
}

/// OpenFreeMap: OpenStreetMap data under the ODbL, served by OpenFreeMap.
/// Two credits because there are two parties -- the surveyors and the host
/// -- and the ODbL names the first of them specifically.
pub const OPENFREEMAP_CREDIT: &[CreditLine] = &[
    CreditLine { text: "Map data \u{a9} OpenStreetMap contributors (ODbL)", lang: None },
    CreditLine { text: "Tiles \u{a9} OpenFreeMap", lang: None },
];

/// Mapbox's `composite` source: Mapbox's own credit and OpenStreetMap's, as
/// the TileJSON states them and `examples/mapbox_streets_demo.rs` shows
/// natively. Text only, as there: Mapbox's terms also ask for the wordmark,
/// an image, which neither shell draws yet.
pub const MAPBOX_CREDIT: &[CreditLine] = &[
    CreditLine { text: "\u{a9} Mapbox \u{a9} OpenStreetMap contributors", lang: None },
    CreditLine { text: "Improve this map: www.mapbox.com/contribute", lang: None },
];

/// What a `?tiles=` override with no entry above gets: a statement that the
/// credit is unknown, rather than someone else's.
pub const UNKNOWN_SOURCE_CREDIT: &[CreditLine] = &[CreditLine {
    text: "Map data from the tile source shown under \u{201c}source\u{201d} \u{2014} \
           this build carries no credit for it. Check its licence before redistributing.",
    lang: None,
}];

/// `[REUSED]` from `map_view::ZOOM_LEVELS_PER_SCROLL_LINE`. 0.8, not 1.0,
/// matching Mapbox GL JS and Google Maps; a whole level per line reads as
/// twitchy.
const ZOOM_LEVELS_PER_SCROLL_LINE: f64 = 0.8;

// The camera's zoom range is `crate::camera_control::MIN_ZOOM..=MAX_ZOOM`,
// shared with native and iOS -- see those constants' doc comments. The
// ceiling is past every ceiling [`source_max_zoom`] reports (14 for the
// default corpus, 16 for a Mapbox composite `?tiles=` override), so the
// top few levels are OVERZOOM: the ceiling's own tiles magnified, with the
// style still resolved at the camera's own zoom bucket. That is exactly
// what native does, and why `display_zoom` is deliberately not clamped to
// the source ceiling -- see its doc comment.

/// A touch that lasts longer than this, measured `pointerdown` to
/// `pointerup`, is a press-and-hold rather than a tap and cannot start (or
/// continue) a double-tap sequence.
const TAP_MAX_DURATION_MS: f64 = 300.0;
/// A touch whose finger travelled further than this many CSS pixels between
/// its `pointerdown` and `pointerup` is a (possibly very short) drag, not a
/// tap.
const TAP_MAX_MOVEMENT_PX: f32 = 12.0;
/// Two taps count as a double-tap only if the second lands within this many
/// milliseconds of the first ending.
const DOUBLE_TAP_MS: f64 = 350.0;
/// ...and within this many CSS pixels of it.
const DOUBLE_TAP_PX: f32 = 40.0;

/// How many milliseconds of tessellation one frame may spend.
///
/// Crossing an integer zoom re-bakes every visible tile (their `PaintPlan`s
/// were resolved at the old zoom), so without a ration a zoom step is a
/// third-of-a-second freeze. Native avoids this entirely by baking on a tokio
/// worker; a browser main thread has no worker to hand it to, so the work is
/// spread over frames instead. See the spec -- this is the single biggest
/// thing phase 2 inherits.
///
/// # Why a millisecond budget and not a tile count
///
/// This was `BAKES_PER_FRAME: usize = 2`, justified by an ESTIMATE of "~15 ms
/// for a dense tile". Measured over the sixteen-tile Singapore cover, five
/// page loads: the MEAN bake is 7.1-8.1 ms and the WORST single bake is
/// 20.7-26.4 ms. Both halves of that matter, and they point opposite ways.
///
/// A count of two costs 15 ms on a mean tile -- a frame, near enough -- and
/// 41-53 ms on two dense ones, which is three dropped frames. Lowering the
/// count to one would fix the freeze and halve the fill rate for every cheap
/// tile that never needed the protection. No single count serves a
/// distribution that wide.
///
/// A time budget serves both, because it is a budget against the thing
/// actually being protected. Cheap tiles run three or four to a frame; a
/// dense one runs alone. The budget is checked BEFORE each tile and the first
/// is always taken, so a single tile costing more than the whole budget still
/// makes progress -- the frame it overruns is unavoidable, and it is one
/// frame instead of the two or three a pair of them would cost.
///
/// 12, so that the mean case still fits two tiles and the fill rate is
/// unchanged from the count of two it replaces. It is not 16.7: the frame
/// also has to plan, draw, and submit.
///
/// Measured, five page loads each, as the worst SINGLE frame's bake spend:
///
///     count of 2       40.3-44.2 ms      full cover 309-496 ms
///     12 ms budget     24.5-32.7 ms      full cover 305-431 ms
///
/// A 28% cut in the worst frame with the fill rate unchanged and neither
/// range overlapping. The residual overrun is inherent to checking a budget
/// before doing the work: a 5 ms tile passes the check and a 26 ms tile may
/// follow it. Bounding that too would mean predicting a tile's cost before
/// tessellating it, which nothing here can do.
const BAKE_BUDGET_MS_PER_FRAME: f64 = 12.0;

/// The caller overlays' share of a frame: index builds, point views and
/// overlay tile bakes run inline until this is spent (at least one bake a
/// frame). A third of a 60 Hz frame, beside the basemap's own bake budget;
/// the step past it is Web Workers -- see the overlays spec.
const OVERLAY_BUDGET_MS_PER_FRAME: f64 = 6.0;

/// How often the hidden-tab pump wakes, in milliseconds. See
/// [`start_hidden_pump`] -- and note that browsers clamp a background
/// `setTimeout` to about one second whatever is asked for, so this is a
/// floor rather than a period.
const HIDDEN_PUMP_INTERVAL_MS: i32 = 250;

/// Upper bound on retained GPU tiles, so a long pan does not grow the buffer
/// set without limit. Native has eight separately-bounded caches; this is one
/// crude cap, which is all a spike needs.
///
/// It is now a bound on the DISCARD PILE, not on what is drawing: eviction
/// never touches the retained set (cover + stand-ins + coarse cover). That is
/// the `_tiles`/`_cache` split all three reference engines make -- see
/// `docs/superpowers/specs/2026-08-30-tile-retention-reference.md` §5A.
const MAX_GPU_TILES: usize = 96;

/// Upper bound on retained DECODED tiles -- the `mvt::DecodedTile` a mesh is
/// baked from, which is a much larger object than the mesh and was bounded by
/// nothing before this constant existed.
///
/// 64, against [`MAX_GPU_TILES`]'s 96, and deliberately the SMALLER of the
/// two: a decoded tile is only needed to bake or re-bake, while a mesh is
/// needed to draw. Keeping more geometry than meshes would be paying the
/// larger cost for the rarer need. The cost of getting it wrong in this
/// direction is one refetch of a 150 KB tile from a server on loopback.
///
/// Neither bound can drop a tile that is retained or resident -- see the
/// eviction in `bake_some` -- so both are bounds on the discard pile, and a
/// cover larger than either is still drawn in full.
const MAX_DECODED_TILES: usize = 64;

/// Upper bound on GPU-resident imagery tiles. One crude cap, the same shape
/// as [`MAX_GPU_TILES`] and for the same reason -- native's `raster_tiles` is
/// a byte-BUDGETED cache (`RASTER_DECODED_BUDGET_BYTES` and friends,
/// `map_view.rs`), and a page whose imagery has never existed before this
/// task gets the spike-level version of that, not the full budget
/// accounting. A raster tile costs `tile_size^2 * 4` bytes of texture --
/// 256KB at 256px -- so 48 tiles is 12MB, comfortably inside a phone's
/// texture budget.
const MAX_RASTER_TILES: usize = 48;

/// How many zoom levels below the tile cover the coarse prefetch cover sits.
///
/// Ported from maplibre-native's `DEFAULT_PREFETCH_ZOOM_DELTA`
/// (`include/mln/util/constants.hpp:51`), driven by `TilePyramid::update`
/// (`tile_pyramid.cpp:127-141, 209-219`): a SECOND tile cover, four levels
/// coarser, fetched and retained but never drawn on its own account. It exists
/// so that when a visible tile has not arrived yet the ancestor walk has
/// something to magnify -- proactively, at a quiet moment, rather than issuing
/// the request at the instant the hole appears, when the network is already
/// saturated with the tiles the user is waiting for.
const DEFAULT_PREFETCH_ZOOM_DELTA: u8 = 4;

/// How far below an uncovered tile `covering_descendants` looks. 3, matching
/// both MapLibre engines (`tile_manager.ts:95`, `maxOverzooming = 3`); native
/// here uses 2 (`map_view.rs:5288`).
const MAX_DESCENDANT_DEPTH: u8 = 3;

/// Which of the four retention behaviours is active.
///
/// One build, four arms, so an A/B is a page reload with `?mode=N` rather than
/// a rebuild -- the wasm is byte-identical across arms and cannot be the
/// difference.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    /// `?mode=0` -- exactly the behaviour before this round: the draw list is
    /// the cover, and only cover tiles whose mesh was baked at the CURRENT
    /// zoom bucket are in it.
    Baseline,
    /// `?mode=1` -- a cover tile's own mesh covers even when it was baked at
    /// another zoom bucket. Nothing else changes.
    StaleSelf,
    /// `?mode=2` -- adds descendant and ancestor stand-ins, uploads them, and
    /// makes eviction respect the retained set.
    StandIns,
    /// `?mode=3` (the default) -- adds the coarse prefetch cover.
    Prefetch,
}

impl Mode {
    fn from_index(i: u32) -> Mode {
        match i {
            0 => Mode::Baseline,
            1 => Mode::StaleSelf,
            2 => Mode::StandIns,
            _ => Mode::Prefetch,
        }
    }
    fn index(self) -> u32 {
        match self {
            Mode::Baseline => 0,
            Mode::StaleSelf => 1,
            Mode::StandIns => 2,
            Mode::Prefetch => 3,
        }
    }
    /// A mesh baked at another zoom bucket still counts as coverage.
    /// `hasData()` is deliberately loose in all three reference engines
    /// (`tile.ts:555`): coverage is never surrendered for staleness.
    fn stale_meshes_cover(self) -> bool {
        self != Mode::Baseline
    }
    fn stand_ins(self) -> bool {
        matches!(self, Mode::StandIns | Mode::Prefetch)
    }
    fn prefetch_delta(self) -> u8 {
        if self == Mode::Prefetch {
            DEFAULT_PREFETCH_ZOOM_DELTA
        } else {
            0
        }
    }
}

/// A `baked_zoom` no camera can produce, used to mark every resident mesh
/// as stale in one pass.
///
/// [`map_set_style`] sets it on every GPU tile: `bake_some` already re-bakes
/// anything whose `baked_zoom` is not the display zoom, so invalidating a
/// style is exactly the same operation as crossing a zoom bucket and needs
/// no second code path. Dropping the meshes instead would blank the map for
/// the second or two the re-bake takes; this way the old picture stays up
/// and is replaced tile by tile, which is what every GL engine does on
/// `setStyle`.
const REBAKE_SENTINEL: u8 = u8::MAX;

fn tile_parent(t: TileId) -> Option<TileId> {
    (t.z > 0).then(|| TileId { z: t.z - 1, x: t.x / 2, y: t.y / 2 })
}

/// The `4^depth` descendants of `t` at `t.z + depth`.
fn tile_descendants(t: TileId, depth: u8) -> Vec<TileId> {
    let mut level = vec![t];
    for _ in 0..depth {
        let mut next = Vec::with_capacity(level.len() * 4);
        for p in level {
            for dy in 0..2 {
                for dx in 0..2 {
                    next.push(TileId { z: p.z + 1, x: p.x * 2 + dx, y: p.y * 2 + dy });
                }
            }
        }
        level = next;
    }
    level
}

/// The coarsest COMPLETE generation of descendants of `tile` that `has_mesh`
/// accepts, or `None`.
///
/// Complete, not partial, and deliberately: drawing 1 of 4 children leaves the
/// other three quarters a bare hole, which is worse than magnifying an
/// ancestor. That gate is `descendants_fully_cover` on native and
/// `_areDescendentsComplete` in maplibre-gl-js (`tile_manager.ts:427-436`).
/// Coarsest first, not finest, matching `tile_manager.ts:388-393` -- fewest,
/// largest stand-ins.
fn covering_descendants(tile: TileId, source_max_zoom: u8, has_mesh: impl Fn(TileId) -> bool) -> Option<Vec<TileId>> {
    // Nothing is ever fetched below the source's own ceiling, so looking there
    // is 4 + 16 + 64 guaranteed misses per uncovered tile.
    let max_depth = MAX_DESCENDANT_DEPTH.min(source_max_zoom.saturating_sub(tile.z));
    for depth in 1..=max_depth {
        let gen = tile_descendants(tile, depth);
        if gen.iter().all(|d| has_mesh(*d)) {
            return Some(gen);
        }
    }
    None
}

/// What one frame decided: what to draw, what to keep, and what it cost.
#[derive(Default)]
struct FramePlan {
    /// Coarse-first, so the ideal tiles paint over their own stand-ins. All
    /// geometry is coplanar at z=0 and the pipeline is `LessEqual`, so draw
    /// order IS the tiebreaker (`gpu.rs:216`) -- exactly the two-pass shape
    /// native uses (`map_view.rs:6356` then `:6425`).
    draws: Vec<TileId>,
    /// This frame's basemap cover, nearest the eye first: what the caller
    /// overlays are baked and picked against (`overlay::host`).
    cover: Vec<TileId>,
    /// Everything eviction must not touch: the cover, every stand-in chosen,
    /// every ancestor walked on the way to one, and the coarse cover.
    retained: HashSet<TileId>,
    cover_len: usize,
    /// The cover's `(finest, coarsest)` zoom -- two values under the
    /// distance LOD (`cover_plan::flat_cover`), one at pitch 0. For the
    /// status line's `tiles z..`.
    cover_zooms: Option<(u8, u8)>,
    /// Cover tiles that drew their own mesh at the current zoom bucket.
    fresh: u32,
    /// Cover tiles that drew their own mesh, baked at ANOTHER zoom bucket.
    /// Under `Mode::Baseline` these are holes instead.
    stale: u32,
    descendant_stand_ins: u32,
    ancestor_stand_ins: u32,
    /// Cover tiles that drew NOTHING -- no mesh of their own, no covering
    /// descendants, no resident ancestor.
    holes: u32,
    /// Of `holes`: the tile HAS a mesh of its own, baked at another zoom
    /// bucket, and the draw filter threw it away.
    ///
    /// This is a browser-only, self-inflicted hole: native keeps drawing the
    /// stale mesh until the re-bake lands (`gpu_meshes` is keyed by tile, not
    /// by bake zoom, and `map_view.rs:6425` draws whatever is in it). It is
    /// counted in EVERY arm, so one run tells the three causes apart.
    holes_stale_mesh: u32,
    /// Of `holes`: no ancestor of this tile has been decoded at any level, so
    /// there was never anything to stand in with. The "never LOADED, not
    /// evicted" half of the native diagnosis.
    holes_never_loaded: u32,
    /// Of `holes`: an ancestor IS decoded but has no GPU mesh -- it was
    /// evicted, or has not been baked yet. The "evicted / unbaked" half.
    holes_ancestor_unbaked: u32,
}

/// Everything the HUD reports and the sweep driver reads back.
#[derive(Default, Clone, Copy)]
struct Metrics {
    frames: u64,
    hole_frames: u64,
    holes: u64,
    holes_stale_mesh: u64,
    holes_never_loaded: u64,
    holes_ancestor_unbaked: u64,
    stale: u64,
    descendant_stand_ins: u64,
    ancestor_stand_ins: u64,
    requests_cover: u64,
    requests_prefetch: u64,
    bakes_cover: u64,
    bakes_prefetch: u64,
    bytes_cover: u64,
    bytes_prefetch: u64,
    /// Milliseconds spent inside `match_tile` + `tessellate_plan`, summed
    /// over every bake.
    ///
    /// This is the scarce resource `BAKE_BUDGET_MS_PER_FRAME` rations, and
    /// until it was measured the ration was set against an ESTIMATE -- the
    /// "~15 ms for a dense tile" in that constant's doc comment. It is also
    /// the only startup cost that is purely this crate's own CPU, so it is
    /// the number an optimiser-level change moves and the network does not.
    bake_ms: f64,
    /// The single most expensive bake seen. Kept apart from the mean because
    /// the ration is a budget against the WORST tile in a frame as much as
    /// the average one: a mean of 7.7 ms with a worst case of 7.7 and a mean
    /// of 7.7 with a worst case of 26 want different rations, and this crate
    /// turned out to be the second.
    bake_ms_max: f64,
    /// The most tessellation any ONE call to `bake_some` spent -- i.e. the
    /// worst single frame's bake cost, which is the number the per-frame
    /// ration exists to hold down and the only one that corresponds to a
    /// visible stutter. `bake_ms_max` is per TILE and can only be lowered by
    /// making the tessellator faster; this one is per FRAME and is exactly
    /// what the ration controls.
    bake_ms_frame_max: f64,
}

impl Metrics {
    fn record(&mut self, plan: &FramePlan) {
        self.frames += 1;
        if plan.holes > 0 {
            self.hole_frames += 1;
        }
        self.holes += plan.holes as u64;
        self.holes_stale_mesh += plan.holes_stale_mesh as u64;
        self.holes_never_loaded += plan.holes_never_loaded as u64;
        self.holes_ancestor_unbaked += plan.holes_ancestor_unbaked as u64;
        self.stale += plan.stale as u64;
        self.descendant_stand_ins += plan.descendant_stand_ins as u64;
        self.ancestor_stand_ins += plan.ancestor_stand_ins as u64;
    }

    fn to_json(self, mode: Mode) -> String {
        format!(
            "{{\"mode\":{},\"frames\":{},\"hole_frames\":{},\"holes\":{},\
             \"holes_stale_mesh\":{},\"holes_never_loaded\":{},\
             \"holes_ancestor_unbaked\":{},\"stale\":{},\
             \"descendant_stand_ins\":{},\"ancestor_stand_ins\":{},\
             \"requests_cover\":{},\"requests_prefetch\":{},\
             \"bakes_cover\":{},\"bakes_prefetch\":{},\
             \"bytes_cover\":{},\"bytes_prefetch\":{},\"bake_ms\":{:.1},\
             \"bake_ms_max\":{:.1},\"bake_ms_frame_max\":{:.1}}}",
            mode.index(),
            self.frames,
            self.hole_frames,
            self.holes,
            self.holes_stale_mesh,
            self.holes_never_loaded,
            self.holes_ancestor_unbaked,
            self.stale,
            self.descendant_stand_ins,
            self.ancestor_stand_ins,
            self.requests_cover,
            self.requests_prefetch,
            self.bakes_cover,
            self.bakes_prefetch,
            self.bytes_cover,
            self.bytes_prefetch,
            self.bake_ms,
            self.bake_ms_max,
            self.bake_ms_frame_max,
        )
    }
}

/// The startup timeline, in `performance.now()`'s time origin.
///
/// Every mark is milliseconds since NAVIGATION, not since wasm entry, so the
/// list reads as a partition of page load rather than a set of unrelated
/// stopwatches. That distinction is not pedantic: the fetch, compile and
/// instantiate of a 5 MB `.wasm` all happen before the first line of this
/// crate runs, and no stopwatch started inside `run` can see any of it. The
/// old "page load -> first triangle" number was started at `run`'s first
/// line and was therefore not measuring page load at all.
///
/// The two counters logged alongside it -- frames elapsed, and whether the
/// document was hidden -- are here because an elapsed time ALONE cannot tell
/// a slow renderer from a browser that was not running the frame loop.
/// Chrome throttles `requestAnimationFrame` to a few hertz in a hidden or
/// occluded tab and stops it entirely in some, and since every tile request
/// and every bake in this app is driven from that callback, such a tab spends
/// its first ten seconds having issued no requests at all. Measured: 7568 ms
/// to first triangle over 19 FRAMES in a background tab, against 105-162 ms
/// over the same handful of frames in a foreground one. Same binary, same
/// network, 50x apart -- and indistinguishable without the frame count.
#[derive(Default)]
struct Startup {
    /// `(label, milliseconds since navigation)`, in the order reached.
    marks: Vec<(&'static str, f64)>,
}

impl Startup {
    fn mark(&mut self, label: &'static str) {
        self.marks.push((label, now_ms()));
    }

    /// `mark`, but only for a stage that is reached repeatedly and is only
    /// interesting the first time -- the first fetch dispatched, the first
    /// tile decoded, the first bake.
    fn mark_once(&mut self, label: &'static str) {
        if !self.marks.iter().any(|(l, _)| *l == label) {
            self.mark(label);
        }
    }

    /// The timeline as `label +delta (absolute)`. The delta is from the
    /// PREVIOUS mark, and the first one is measured from zero -- i.e. from
    /// navigation -- so it reports the wasm load nobody else can.
    fn report(&self) -> String {
        let mut out = String::new();
        let mut prev = 0.0;
        for (label, t) in &self.marks {
            out.push_str(&format!("{label} +{:.0} ({:.0})  ", t - prev, t));
            prev = *t;
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Pan inertia -- flick to fling, and glide to a stop.
//
// `[LIFTED]` from `map_view.rs:200-390`, which is itself a faithful port of
// MapLibre GL JS's `src/ui/handler_inertia.ts`. Every constant below is
// MapLibre's, and the reason to copy rather than tune is that the user is
// comparing this against native, which is comparing itself against MapLibre:
// a hand-tuned approximation feels subtly wrong in a way that is hard to
// describe and easy to notice.
//
// The only substantive change from native is the clock. `Instant`/`Duration`
// become `f64` milliseconds taken from `Event::time_stamp()` -- the event's
// OWN timestamp, in the same time origin as `performance.now()`, not the wall
// clock at handling time. That distinction matters: the browser coalesces
// pointer motion into roughly one event per frame, so reading the clock when
// the handler runs would compress a burst of motion into an instant and
// inflate the measured velocity.
// ---------------------------------------------------------------------------

/// How far back the drag-sample buffer keeps history. Samples older than this
/// are dropped on every recorded move, so the buffer stays a few entries long
/// however long the drag runs, and a drag that stalls before release ages out
/// its own history and flings nothing. MapLibre's buffer `cutoff`,
/// `handler_inertia.ts`.
const INERTIA_BUFFER_CUTOFF_MS: f64 = 160.0;

/// How much of the retained history release velocity is measured over.
/// SHORTER than the buffer on purpose: a drag that curves or slows at the end
/// must fling along its FINAL direction, not the average of the whole buffer.
/// (MapLibre measures across its entire buffer; this is native's one
/// deliberate deviation from it, kept here so the two agree.)
const INERTIA_VELOCITY_WINDOW_MS: f64 = 60.0;

/// MapLibre's `linearity`: the fraction of the measured drag speed the glide
/// starts at, and the knob that trades glide distance against glide time.
const INERTIA_LINEARITY: f64 = 0.3;

/// Deceleration in px/s^2 and the speed ceiling in px/s, from
/// `handler_inertia.ts`'s pan-specific inertia options. The cap is what stops
/// a violent flick from throwing the map several screens away.
const PAN_INERTIA_DECELERATION: f64 = 2500.0;
const PAN_INERTIA_MAX_SPEED: f64 = 1400.0;

/// Glides shorter than this are dropped entirely. Half a pixel is invisible,
/// but it would still hold the frame loop open for a few frames after a
/// release -- and a user who parked the cursor before letting go expects the
/// map to stop dead.
const MIN_PAN_INERTIA_OFFSET_PX: f64 = 0.5;

/// MapLibre's inertia easing curve, `bezier(0, 0, 0.3, 1)`: leaves the origin
/// at drag speed and flattens into a stop.
///
/// This reuses the ENGINE's own `cubic-bezier` solver rather than carrying a
/// second copy of the same WebKit `UnitBezier` math -- exactly what native
/// does, and one more line item for the portability survey: the curve
/// evaluation was already portable, only the driving was not.
fn pan_inertia_easing() -> crate::style_spec::expression::Interpolation {
    crate::style_spec::expression::Interpolation::CubicBezier { x1: 0.0, y1: 0.0, x2: 0.3, y2: 1.0 }
}

/// One (time, cursor position) pair recorded during an in-progress drag.
#[derive(Clone, Copy, Debug)]
struct DragSample {
    at_ms: f64,
    x: f32,
    y: f32,
}

/// A post-release pan glide in flight.
///
/// `offset_*` is the TOTAL pixel distance the glide will cover, in the sign
/// convention `pan_by_pixels` takes, and `applied_*` is how much of that has
/// already been handed to it. Tracking the applied total, rather than
/// integrating a per-frame velocity, is what keeps the glide from drifting:
/// each frame applies the difference between where the curve says it should
/// be and where it already is, so a long or short frame self-corrects instead
/// of accumulating error.
#[derive(Clone, Copy, Debug)]
struct PanInertia {
    started_ms: f64,
    duration_ms: f64,
    offset_x: f64,
    offset_y: f64,
    applied_x: f64,
    applied_y: f64,
}

/// Drops drag samples older than `INERTIA_BUFFER_CUTOFF_MS` relative to `now`.
fn drain_drag_samples(samples: &mut std::collections::VecDeque<DragSample>, now_ms: f64) {
    while samples.front().is_some_and(|s| (now_ms - s.at_ms).max(0.0) > INERTIA_BUFFER_CUTOFF_MS) {
        samples.pop_front();
    }
}

/// Picks the sample pair release velocity is measured across: the newest
/// sample, and the oldest one still inside `INERTIA_VELOCITY_WINDOW_MS`.
///
/// Always returns at least the last two samples when there are two. A slow
/// event stream (or one long frame stall) can leave the only two samples
/// further apart than the velocity window, and collapsing to a single sample
/// there would silently kill the fling on exactly the machines that need it
/// most.
fn drag_velocity_window(samples: &[DragSample], released_ms: f64) -> Option<(DragSample, DragSample)> {
    if samples.len() < 2 {
        return None;
    }
    let last = *samples.last()?;
    let widest = samples.len() - 2;
    let index = samples
        .iter()
        .position(|s| (released_ms - s.at_ms).max(0.0) <= INERTIA_VELOCITY_WINDOW_MS)
        .unwrap_or(widest)
        .min(widest);
    Some((samples[index], last))
}

/// MapLibre's `calculateEasing`, one axis. Returns the total pixel offset the
/// glide covers on that axis and how long it takes:
///
/// ```text
/// speed    = clamp(amount * linearity / window, -maxSpeed, maxSpeed)
/// duration = |speed| / (deceleration * linearity)
/// offset   = speed * duration / 2
/// ```
///
/// The `offset` line is the area under a linear ramp from `speed` to zero
/// over `duration` -- constant deceleration. The curve applied per frame is
/// the bezier, not that ramp, but the total distance is what the deceleration
/// model dictates.
fn pan_inertia_axis(amount_px: f64, window_secs: f64) -> (f64, f64) {
    let speed = (amount_px * INERTIA_LINEARITY / window_secs)
        .clamp(-PAN_INERTIA_MAX_SPEED, PAN_INERTIA_MAX_SPEED);
    let duration_secs = speed.abs() / (PAN_INERTIA_DECELERATION * INERTIA_LINEARITY);
    (speed * (duration_secs / 2.0), duration_secs)
}

/// The whole velocity -> glide computation, as a pure function of numbers.
/// Returns the total offset per axis and the glide duration in MILLISECONDS,
/// or `None` when there should be no glide at all.
///
/// Both axes share the LONGER of the two computed durations (MapLibre takes
/// the longest across all its deltas), so a diagonal fling is one motion that
/// stops once rather than one that snaps to an axis partway through. The
/// per-axis offsets are deliberately not rescaled to match: the slower axis
/// simply covers its own distance more gradually.
fn pan_inertia_curve(amount_x: f64, amount_y: f64, window_secs: f64) -> Option<(f64, f64, f64)> {
    // A zero-length window would divide by zero and give infinite speed.
    // (MapLibre guards it with a `+ 1e-15` epsilon, which merely turns the
    // infinity into a huge finite number; rejecting the pair is honest.)
    if window_secs <= 0.0 || !window_secs.is_finite() {
        return None;
    }
    let (offset_x, duration_x) = pan_inertia_axis(amount_x, window_secs);
    let (offset_y, duration_y) = pan_inertia_axis(amount_y, window_secs);
    if offset_x.hypot(offset_y) < MIN_PAN_INERTIA_OFFSET_PX {
        return None;
    }
    let duration_secs = duration_x.max(duration_y);
    if duration_secs <= 0.0 {
        return None;
    }
    Some((offset_x, offset_y, duration_secs * 1000.0))
}

/// Builds the glide a release at `released_ms` should start. `None` when the
/// gesture was too short, too slow, or too sparse to fling.
fn pan_inertia_from_samples(samples: &[DragSample], released_ms: f64) -> Option<PanInertia> {
    let (first, last) = drag_velocity_window(samples, released_ms)?;
    let window_secs = (last.at_ms - first.at_ms).max(0.0) / 1000.0;
    let amount_x = (last.x - first.x) as f64;
    let amount_y = (last.y - first.y) as f64;
    let (offset_x, offset_y, duration_ms) = pan_inertia_curve(amount_x, amount_y, window_secs)?;
    Some(PanInertia {
        started_ms: released_ms,
        duration_ms,
        offset_x,
        offset_y,
        applied_x: 0.0,
        applied_y: 0.0,
    })
}

/// One frame of a glide: the pixel delta to hand `pan_by_pixels`, and whether
/// this was the last frame.
///
/// The final frame applies exactly the un-applied remainder rather than
/// `offset * eased(1.0)`, so the glide lands precisely on `offset` with no
/// sub-pixel residual left behind by the bezier solver's tolerance.
fn pan_inertia_step(inertia: &PanInertia, now_ms: f64) -> ((f64, f64), bool) {
    let elapsed = (now_ms - inertia.started_ms).max(0.0);
    let t = if inertia.duration_ms > 0.0 {
        (elapsed / inertia.duration_ms).clamp(0.0, 1.0)
    } else {
        1.0
    };
    if t >= 1.0 {
        return ((inertia.offset_x - inertia.applied_x, inertia.offset_y - inertia.applied_y), true);
    }
    // `t_at(t, 0.0, 1.0)` makes the style-spec interpolation's zoom-bracket
    // arguments degenerate into a plain 0..1 fraction, which is exactly the
    // CSS easing evaluation this wants.
    let eased = pan_inertia_easing().t_at(t, 0.0, 1.0);
    (
        (inertia.offset_x * eased - inertia.applied_x, inertia.offset_y * eased - inertia.applied_y),
        false,
    )
}

/// The clear colour a style asks for.
///
/// Resolved through the engine against an EMPTY tile rather than read out of
/// the JSON: a `background` layer's `background-color` can be an expression,
/// and `match_tile` is the thing that evaluates one. A style with no
/// background layer gets white, which is the spec's own default.
fn style_background(style: &Style) -> wgpu::Color {
    crate::style::match_tile(&DecodedTile { data_hash: 0, layers: vec![] }, style, 14)
        .ops
        .iter()
        .find_map(|op| match op {
            crate::style::PaintOp::Background { color } => Some(TilePipeline::clear_color(color)),
            _ => None,
        })
        .unwrap_or(wgpu::Color::WHITE)
}

/// The style's `background-color` as the zoom-STOP PAIR the shader blends,
/// for the integer zoom bucket the tiles are resolved at.
///
/// The pair rather than one lerped colour, for the reason
/// `render::space::backdrop_plan` argues: the ocean sphere bakes both ends
/// into its vertices and lets `shader.wgsl` blend them by `blend_t`, so a
/// zoom-expression background does not re-tessellate 4,225 vertices on every
/// frame of a zoom. `None` for a style with no `background` layer, which
/// `space_plan` reads as fully transparent -- and which is therefore pure
/// space at full globeness rather than a transparent hole.
///
/// Read off `style.layers` directly, unlike [`style_background`] above, which
/// goes through `match_tile`: `match_tile` resolves `value_at`, a single
/// colour, and the pair is exactly what it throws away.
fn background_pair(style: &Style, zoom: u8) -> Option<(crate::style::Rgba, crate::style::Rgba)> {
    style.layers.iter().find_map(|layer| match layer {
        crate::style::StyleLayer::Background { color, .. } => Some(color.value_at_pair(zoom)),
        _ => None,
    })
}

/// What this page's [`render::space::Space`] starts as.
///
/// **Stars and the terminator are OFF, as natively** -- the 2026-09-29
/// ruling ("on all maps, default state: no terminator, plain maps"). The
/// page used to default `?stars=` to 1.0 because a globe had no silhouette
/// without the backdrop; `space::draws_backdrop` no longer depends on the
/// stars, so the black space behind the planet stays. `?stars=1` and
/// `?night=0.25` bring either back.
///
/// `?stars=`, `?atmosphere=` and `?night=` are the browser-reachable version
/// of `GPUI_MAP_STARS`/`GPUI_MAP_ATMOSPHERE`/`GPUI_MAP_NIGHT`. Anything that
/// does not parse as a finite `f32` leaves the field at its default;
/// `Space::star_alpha` and `Atmosphere::blend`/`night_brightness_clamped`
/// clamp regardless, so a bad value cannot reach a shader.
fn space_default() -> crate::render::space::Space {
    let defaults = crate::render::atmosphere::Atmosphere::default();
    crate::render::space::Space {
        star_intensity: num_param("stars").map(|v| v as f32).unwrap_or(0.0),
        atmosphere: crate::render::atmosphere::Atmosphere {
            intensity: num_param("atmosphere").map(|v| v as f32).unwrap_or(defaults.intensity),
            night_brightness: num_param("night")
                .map(|v| v as f32)
                .unwrap_or(defaults.night_brightness),
            // `?sunlon=&sunlat=` pins the subsolar point, so a capture of
            // the day side does not depend on the wall clock it ran at.
            // Both or neither: one alone leaves `Sun::Now`.
            sun: match (num_param("sunlon"), num_param("sunlat")) {
                (Some(lon_deg), Some(lat_deg)) => crate::render::atmosphere::Sun::Map { lon_deg, lat_deg },
                // A census page is daylit unless `?sun=now` (the ledger's
                // ruling: a night-shaded choropleth hides the data).
                _ => census_page::census_param()
                    .and_then(|_| crate::census::census_sun(query_param("sun").as_deref(), false))
                    .unwrap_or(defaults.sun),
            },
        },
        ..crate::render::space::Space::default()
    }
}

/// What this page's [`render::space_look::SpaceLook`] starts as: OFF, as
/// natively, unless `?look=space` -- then [`SpaceLook::ON`], the tuned
/// defaults `globe_capture --space-look` draws with.
///
/// [`SpaceLook::ON`]: crate::render::space_look::SpaceLook::ON
fn space_look_default() -> crate::render::space_look::SpaceLook {
    if query_param("look").is_some_and(|v| v == "space") {
        crate::render::space_look::SpaceLook::ON
    } else {
        crate::render::space_look::SpaceLook::default()
    }
}

fn now_ms() -> f64 {
    web_sys::window().and_then(|w| w.performance()).map(|p| p.now()).unwrap_or(0.0)
}

/// One `?name=value` off the page's own URL, percent-decoded.
///
/// Hand-rolled rather than `URLSearchParams`, which would mean another
/// `web-sys` feature and another few kilobytes of glue for eight lines of
/// string splitting. `decodeURIComponent` is reached through `js_sys` --
/// already a dependency -- because a tile template contains `{` and `}`,
/// which a browser percent-encodes when it normalises the address bar.
fn query_param(name: &str) -> Option<String> {
    let search = web_sys::window()?.location().search().ok()?;
    let prefix = format!("{name}=");
    let raw = search.trim_start_matches('?').split('&').find_map(|kv| kv.strip_prefix(&prefix))?;
    let decoded = js_sys::decode_uri_component(raw).ok().and_then(|v| v.as_string());
    Some(decoded.unwrap_or_else(|| raw.to_string()))
}

thread_local! {
    /// The tile template in force, [`TILE_URL`] unless `?tiles=` overrode it.
    ///
    /// A `thread_local` rather than a `MapApp` field because it is
    /// configuration read once at startup and never written again, and
    /// putting it on `MapApp` would mean `request_missing_tiles` had to hold
    /// the `RefCell` borrow across the `spawn_local` boundary to read it.
    static TILE_TEMPLATE: RefCell<String> = RefCell::new(TILE_URL.to_string());

    /// The schema [`TILE_TEMPLATE`] speaks, set with it at startup. The
    /// style picker reads it: a style of the OTHER schema cannot be swapped
    /// in over these tiles (see [`map_set_style`]).
    static TILE_SCHEMA: std::cell::Cell<crate::basemap::TileSchema> =
        const { std::cell::Cell::new(crate::basemap::TileSchema::Omt) };
}

fn tile_template() -> String {
    TILE_TEMPLATE.with(|t| t.borrow().clone())
}

/// `{z}/{x}/{y}` filled in with `tile`'s coordinates.
fn fill_template(template: &str, tile: TileId) -> String {
    template
        .replace("{z}", &tile.z.to_string())
        .replace("{x}", &tile.x.to_string())
        .replace("{y}", &tile.y.to_string())
}

/// Which of the page's two tile grids a request is for: the vector source
/// and an imagery source share the `(z, x, y)` grid, so the same id can be
/// wanted for both at once.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum TileKind {
    Vector,
    Raster,
}

/// A request's identity in [`crate::fetch_queue::FetchQueue`].
type TileKey = (TileId, TileKind);

/// Tile requests on the wire at once. The iOS shell's `net::WORKERS`, and
/// the same order as the six-per-host limit an HTTP/1.1 browser applied by
/// accident before there was a queue; MapLibre's own
/// `MAX_PARALLEL_IMAGE_REQUESTS` is 16. Low enough that the next request
/// sent is chosen by THIS frame's priorities, not queued inside the browser
/// where nothing can reorder or cancel it.
const MAX_CONCURRENT_FETCHES: usize = 8;

/// Main-thread time the frame loop may spend decoding arrived tiles, checked
/// between tiles (the first is unconditional). See `MapApp::decode_arrived`.
const DECODE_BUDGET_MS_PER_FRAME: f64 = 4.0;

/// [`query_param`] parsed as a finite `f64`. A malformed or infinite value
/// is `None` rather than a panic or a NaN camera -- the same rule the native
/// demos' `parse_center` applies to their environment variables.
fn num_param(name: &str) -> Option<f64> {
    query_param(name)?.trim().parse::<f64>().ok().filter(|v| v.is_finite())
}

/// The page's own bottom overlays -- the attribution card and the readout --
/// as a bottom inset, in CSS pixels, so the engine-drawn ETA strip sits above
/// them rather than under them. The attribution is a licence condition, so it
/// is the chrome that moves. This is the browser's "safe area": the same
/// input the phone fills from its home indicator.
fn page_overlay_insets(viewport_height: f32) -> crate::nav::host::SafeInsets {
    let document = web_sys::window().and_then(|w| w.document());
    let bottom = ["credit", "hud"]
        .iter()
        .filter_map(|id| document.as_ref()?.get_element_by_id(id)?.dyn_into::<web_sys::HtmlElement>().ok())
        .filter(|el| el.offset_height() > 0)
        .map(|el| viewport_height - el.offset_top() as f32)
        .fold(0.0_f32, f32::max);
    crate::nav::host::SafeInsets { bottom: bottom.clamp(0.0, viewport_height / 2.0), ..Default::default() }
}

/// The page's corner chrome as label obstacles, in the canvas's physical
/// pixels: the credit's card (`#credit-lines`) and the SWAT mark
/// (`#swatmark`), each as laid out right now, padded as a symbol box is
/// (`symbol_pass::chrome_obstacle`). A hidden or absent element (zero
/// size) is no obstacle.
fn dom_chrome_obstacles(scale_factor: f32) -> Vec<crate::collision::CollisionBox> {
    let Some(document) = web_sys::window().and_then(|w| w.document()) else { return Vec::new() };
    let origin = document.get_element_by_id("map").map(|c| c.get_bounding_client_rect());
    let (ox, oy) = origin.map_or((0.0, 0.0), |r| (r.left() as f32, r.top() as f32));
    ["credit-lines", "swatmark"]
        .iter()
        .filter_map(|id| document.get_element_by_id(id))
        .map(|el| el.get_bounding_client_rect())
        .filter(|r| r.width() > 0.0 && r.height() > 0.0)
        .map(|r| {
            crate::symbol_pass::chrome_obstacle(
                (r.left() as f32 - ox) * scale_factor,
                (r.top() as f32 - oy) * scale_factor,
                r.width() as f32 * scale_factor,
                r.height() as f32 * scale_factor,
                scale_factor,
            )
        })
        .collect()
}

/// Hides the brand card while a trip runs and brings it back after: it
/// sits where the banner draws, and unlike the attribution it is not
/// required. Only called on a change, so a page without `?nav=` never
/// touches it.
fn set_brand_card_hidden(hidden: bool) {
    if let Some(el) = web_sys::window().and_then(|w| w.document()).and_then(|d| d.get_element_by_id("brand")) {
        let _ = if hidden { el.set_attribute("hidden", "") } else { el.remove_attribute("hidden") };
    }
}

/// This frame's inputs for `NavHost`: `performance.now()`, `Date.now()` and
/// the page's own UTC offset, the camera's logical viewport, and the page's
/// overlays as insets. A free function over the camera so it can run while
/// other `MapApp` fields are borrowed.
fn nav_frame_of(camera: &crate::render::Camera) -> crate::nav::host::NavFrame {
    crate::nav::host::NavFrame {
        now_ms: now_ms(),
        wall: crate::nav::host::WallClock {
            unix_ms: js_sys::Date::now(),
            // `getTimezoneOffset` is UTC minus local, in minutes.
            utc_offset_min: -(js_sys::Date::new_0().get_timezone_offset() as i32),
        },
        viewport: (camera.viewport_width_px, camera.viewport_height_px),
        insets: page_overlay_insets(camera.viewport_height_px),
    }
}

/// `NavHost::take_voice` -> `window.speechSynthesis` (spec: `docs/
/// superpowers/specs/2026-09-25-voice-guidance-design.md`, "Shells", web
/// row). `Stop` cancels whatever is playing; `Speak` cancels, then speaks,
/// so a new prompt always interrupts one still in progress -- the same rule
/// every shell's `speak(text)` follows.
///
/// Two things must never turn a spoken instruction into a panic:
/// - **Autoplay.** A browser blocks audio until the page has had a user
///   gesture (`a.voice_gesture_seen`, set on the first `pointerdown` or
///   `keydown` -- see `install_input_handlers`, and note the Simulate
///   button counts because it is a `pointerdown` on this same canvas, not a
///   separate element). Before that, a `Speak` utterance is dropped, not
///   queued: `NavHost` already decided this was the prompt to say, and by
///   the time a gesture arrives it would be stale. `Stop` is never gated --
///   cancelling is not audio, and nothing could be playing this early
///   anyway, since every earlier `Speak` was dropped the same way.
/// - **A missing API.** `Window::speech_synthesis` is a `Result` because
///   some engines (and every headless one without `--enable-speech-api`)
///   have no `speechSynthesis` at all; logged once, not every frame.
fn handle_voice_command(a: &mut MapApp, command: crate::nav::host::VoiceCommand) {
    let Some(window) = web_sys::window() else { return };
    let synth = match window.speech_synthesis() {
        Ok(synth) => synth,
        Err(_) => {
            if !a.voice_no_synth_logged {
                a.voice_no_synth_logged = true;
                log("[voice] speechSynthesis is not available");
            }
            return;
        }
    };
    match command {
        crate::nav::host::VoiceCommand::Stop => synth.cancel(),
        crate::nav::host::VoiceCommand::Speak(utterance) => {
            if !a.voice_gesture_seen {
                log(&format!("[voice] dropped (no gesture yet): {}", utterance.text));
                return;
            }
            let Ok(spoken) = web_sys::SpeechSynthesisUtterance::new_with_text(&utterance.text) else { return };
            if a.voice_choice.is_none() {
                // `voiceschanged` may not have fired yet; the list may be in.
                pick_web_voice(a, &synth);
            }
            match &a.voice_choice {
                Some(voice) => {
                    spoken.set_voice(Some(voice));
                    spoken.set_lang(&voice.lang());
                }
                None => spoken.set_lang("en-US"),
            }
            spoken.set_rate(0.95);
            spoken.set_pitch(1.0);
            synth.cancel();
            synth.speak(&spoken);
        }
    }
}

/// `speechSynthesis.getVoices()` through the shared `pick_voice`, into
/// `a.voice_choice`. Without it Chrome reads `lang = "en-US"` with its
/// default voice, which on macOS can be a compact or novelty one -- the
/// "metallic" voice. The list loads asynchronously (empty until
/// `voiceschanged`), so this runs on that event and again before a prompt
/// while nothing is picked. Logs the name once per new pick.
fn pick_web_voice(a: &mut MapApp, synth: &web_sys::SpeechSynthesis) {
    let voices: Vec<web_sys::SpeechSynthesisVoice> =
        synth.get_voices().iter().filter_map(|v| v.dyn_into::<web_sys::SpeechSynthesisVoice>().ok()).collect();
    let infos: Vec<crate::nav::host::VoiceInfo> = voices
        .iter()
        .map(|v| crate::nav::host::VoiceInfo { name: v.name(), lang: v.lang(), quality: None, local: v.local_service() })
        .collect();
    let Some(index) = crate::nav::host::pick_voice(&infos) else { return };
    let name = &infos[index].name;
    if a.voice_logged.as_deref() != Some(name.as_str()) {
        log(&format!("[voice] using: {name}"));
        a.voice_logged = Some(name.clone());
    }
    a.voice_choice = Some(voices[index].clone());
}

/// One route request through `./swat-route` (the page's own server,
/// `web/serve.py`, adds the key), answered into `NavHost::route_fetched`.
fn fetch_nav_route(app: &Rc<RefCell<MapApp>>, request: crate::nav::host::RouteRequest) {
    let app = app.clone();
    wasm_bindgen_futures::spawn_local(async move {
        let result = fetch_route_body(&request.url).await;
        let mut guard = app.borrow_mut();
        let a = &mut *guard;
        let frame = nav_frame_of(a.camera_control.camera());
        match result {
            RouteFetch::NoProxy(status) => a.nav.routing_unavailable(
                request.id,
                &format!("HTTP {status}: the page is not served by web/serve.py"),
                &frame,
            ),
            RouteFetch::Answer(result) => a.nav.route_fetched(request.id, result, &a.camera_control, &frame),
        }
        a.needs_redraw = true;
    });
}

/// What a route request came back with.
enum RouteFetch {
    /// A body (JSON, whatever the status: OSRM and `serve.py`'s own "no key"
    /// answer report errors as JSON with a `code`), or a transport error.
    Answer(Result<Vec<u8>, String>),
    /// A non-2xx that is not JSON: a plain static server with no
    /// `/swat-route/` proxy -- live routing is off, not a failed route.
    NoProxy(u16),
}

async fn fetch_route_body(url: &str) -> RouteFetch {
    let window = match web_sys::window() {
        Some(window) => window,
        None => return RouteFetch::Answer(Err("no window".to_string())),
    };
    let response: web_sys::Response = match wasm_bindgen_futures::JsFuture::from(window.fetch_with_str(url)).await {
        Ok(value) => match value.dyn_into() {
            Ok(response) => response,
            Err(_) => return RouteFetch::Answer(Err("not a Response".to_string())),
        },
        Err(e) => return RouteFetch::Answer(Err(format!("{e:?}"))),
    };
    let is_json = response.headers().get("content-type").ok().flatten().is_some_and(|t| t.contains("json"));
    if !response.ok() && !is_json {
        return RouteFetch::NoProxy(response.status());
    }
    RouteFetch::Answer(route_body(response).await)
}

async fn route_body(response: web_sys::Response) -> Result<Vec<u8>, String> {
    let buffer = wasm_bindgen_futures::JsFuture::from(response.array_buffer().map_err(|e| format!("{e:?}"))?)
        .await
        .map_err(|e| format!("{e:?}"))?;
    Ok(js_sys::Uint8Array::new(&buffer).to_vec())
}

/// A still, short right-click at `(x, y)`: route there from the camera
/// centre (`NavHost::request_route_at`); the frame loop sends it.
fn destination_click(a: &mut MapApp, x: f32, y: f32) {
    let camera = *a.camera_control.camera();
    let frame = nav_frame_of(&camera);
    if let Some(request) = a.nav.request_route_at(&camera, true, x, y, &frame) {
        a.nav_outbox.push(request);
    }
    a.needs_redraw = true;
}

/// `?nav=sim&route=<name>...`: fetch the named fixture from `./routes/`
/// (copied there by `build.sh`) and hand it to `NavHost`. `?nav=button`
/// offers the engine-drawn Simulate button instead. A name outside
/// `valid_route_name` is refused before it reaches a URL. A page without
/// `nav=` returns at the first line and touches nothing.
async fn load_nav_demo(app: Rc<RefCell<MapApp>>) {
    let pairs: Vec<(String, String)> = ["nav", "route", "speed", "nav_at", "nav_paused", "offroute"]
        .iter()
        .filter_map(|key| query_param(key).map(|value| (key.to_string(), value)))
        .collect();
    let Some(request) = crate::nav::host::DemoRequest::from_query(&pairs) else { return };
    if let Err(err) = start_nav_demo(app, request).await {
        log(&format!("nav: {}", err.as_string().unwrap_or_default()));
    }
}

/// Fetch a demo request's route fixture and hand it to `NavHost`, starting
/// the drive when asked: the half of [`load_nav_demo`] that
/// [`map_nav_start`] shares.
async fn start_nav_demo(app: Rc<RefCell<MapApp>>, request: crate::nav::host::DemoRequest) -> Result<(), JsValue> {
    if !crate::nav::host::valid_route_name(&request.route) {
        return Err(JsValue::from_str(&format!("refusing route name {:?}", request.route)));
    }
    let bytes = fetch_bytes(&format!("./routes/{}.json", request.route))
        .await
        .map_err(|err| JsValue::from_str(&format!("route fixture fetch failed: {err:?}")))?
        .ok_or_else(|| JsValue::from_str(&format!("no route fixture named {:?}", request.route)))?;
    let mut guard = app.try_borrow_mut().map_err(|_| JsValue::from_str("map busy"))?;
    let a = &mut *guard;
    a.nav.set_demo_route(std::sync::Arc::from(bytes), request.options);
    if request.show_button {
        a.nav.set_show_simulate(true);
    }
    a.needs_redraw = true;
    if request.autostart {
        let frame = nav_frame_of(a.camera_control.camera());
        a.nav.start_simulation(&a.camera_control, &frame).map_err(|err| JsValue::from_str(&err.to_string()))?;
    }
    Ok(())
}

/// Start the demo drive on `./routes/<route>.json` at `speed` (1 to 8), as
/// `?nav=sim&route=&speed=` does at load: a page can switch one running
/// map into the turn-by-turn demo without reloading the engine. A demo
/// already running is put away first.
#[wasm_bindgen]
pub async fn map_nav_start(route: String, speed: f64) -> Result<(), JsValue> {
    let app = APP.with(|slot| slot.borrow().clone()).ok_or_else(|| JsValue::from_str("no map"))?;
    let pairs = [("nav", "sim".to_string()), ("route", route), ("speed", speed.to_string())]
        .map(|(k, v)| (k.to_string(), v));
    let request = crate::nav::host::DemoRequest::from_query(&pairs).ok_or_else(|| JsValue::from_str("bad request"))?;
    with_app(|a| a.nav.clear_demo());
    start_nav_demo(app, request).await
}

/// End the demo drive and put it away (`NavHost::clear_demo`), so the map
/// can move on to another demo with no navigation chrome left on it.
#[wasm_bindgen]
pub fn map_nav_end() {
    with_app(|a| {
        a.nav.clear_demo();
        a.needs_redraw = true;
    });
}

/// The imagery source this page fetches from, resolved ONCE at load time and
/// carried on `MapApp` (see [`resolve_raster_config`]) -- for the raster half
/// of the frame.
///
/// Two sources, in priority order, and BOTH are read -- this was the bug: an
/// earlier version of this struct read only the first of them, so a style
/// that declared its own raster source (`examples/liberty-style.json`'s
/// `ne2_shaded`, Natural Earth relief) compiled a correct opacity curve
/// through `render::raster_plan` and then had nothing to fetch, because nothing
/// here ever looked at the style's `"sources"` object to find out where its
/// bytes live.
///
///  1. **`?imagery=`.** A consumer's own token/source, named at LOAD TIME --
///     this crate is a PUBLIC repo target (see `web/src/lib.rs`'s module
///     doc): no Mapbox art is vendored here and no access token is ever
///     baked into a build script or a committed file. The same shape
///     `?tiles=` already overrides [`TILE_URL`] with -- see
///     `examples/globe_capture.rs` for the native twin of this same rule
///     (`MAPBOX_ACCESS_TOKEN`, read at runtime, never printed, never
///     committed). `?imagery_tile_size=`/`?imagery_max_zoom=` override this
///     arm's own 256/22 defaults.
///  2. **The style's own raster source**, with `?imagery=style` (since
///     2026-09-29; before, whenever (1) was absent -- the plain-default
///     ruling turned imagery off unless asked for): the first
///     `StyleLayer::Raster` layer's `source` name, resolved against
///     `parse_style_raster_sources`' reading of the raw style JSON's
///     `"sources"` object for its `tiles[0]`/`tileSize`/`maxzoom`. This is
///     what makes `liberty-style.json` show Natural Earth relief with
///     `?imagery=style` -- exactly as it does when a native consumer wires
///     the same style's raster layer to imagery (the demos' `S`).
///
/// Empty (no imagery at all) with no `?imagery=`, the default -- `tile_size`/
/// `max_zoom` at their arm-2 defaults (256, matching
/// `examples/satellite-style.json`'s own `tileSize`, deliberately not 512 --
/// see that file's own note and this crate's raster task notes: declaring
/// 512 for a 256px source is exactly the mismatch that made imagery soft
/// before `raster_cover_zoom` existed; and 22, Mapbox's own satellite
/// ceiling) in that case too, though `template` being empty means neither
/// number is ever read.
#[derive(Clone)]
struct RasterConfig {
    /// `{z}`/`{x}`/`{y}` template, or empty for "no imagery source".
    template: String,
    tile_size: u32,
    max_zoom: u8,
}

/// One raster SOURCE's declared shape, straight off the style JSON's own
/// `"sources"` object: `tiles[0]`, `tileSize`, `maxzoom`.
///
/// `style_spec` never keeps this. Confirmed by grep, not assumed: no file
/// under `src/style_spec/` parses a `"sources"` object at all, and
/// `StyleLayer::Raster` carries only the source's NAME (`source: String`),
/// never resolved further -- see that variant's own doc comment in
/// `src/style.rs`. Parsed here instead, off the SAME bytes
/// `style_spec::parse_and_compile` already consumed in [`run`], so a style
/// that declares its own imagery needs no `?imagery=` override to draw it.
struct StyleRasterSource {
    tiles: String,
    tile_size: u32,
    max_zoom: u8,
}

/// Every raster-typed entry in the style JSON's `"sources"` object, keyed by
/// source name.
///
/// A parse failure, or a missing/malformed field on one entry, is skipped
/// rather than failing the whole page: a style whose imagery source is
/// declared badly should lose its imagery, not its labels and roads. Reads
/// the raw bytes directly with `serde_json` (already a dependency, for
/// `style_spec::lut`'s colour-theme parsing) rather than through
/// `style::Style`, which never keeps this object at all.
fn parse_style_raster_sources(style_bytes: &[u8]) -> HashMap<String, StyleRasterSource> {
    let mut out = HashMap::new();
    let Ok(root) = serde_json::from_slice::<serde_json::Value>(style_bytes) else { return out };
    let Some(sources) = root.get("sources").and_then(serde_json::Value::as_object) else {
        return out;
    };
    for (name, source) in sources {
        if source.get("type").and_then(serde_json::Value::as_str) != Some("raster") {
            continue;
        }
        let Some(tiles) = source
            .get("tiles")
            .and_then(serde_json::Value::as_array)
            .and_then(|a| a.first())
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        // `raster::DEFAULT_TILE_SIZE`'s own default (512) when the style
        // does not declare `tileSize` at all.
        let tile_size =
            source.get("tileSize").and_then(serde_json::Value::as_u64).map_or(512, |v| v as u32);
        let max_zoom =
            source.get("maxzoom").and_then(serde_json::Value::as_u64).map_or(22, |v| v as u8);
        out.insert(name.clone(), StyleRasterSource { tiles: tiles.to_string(), tile_size, max_zoom });
    }
    out
}

/// [`RasterConfig`]'s own resolution order -- see that struct's doc comment
/// for the two arms and why both must be read. `style` is the already-
/// compiled `Style` (for its layers' `raster_source()` names); `style_bytes`
/// is the SAME bytes `style_spec::parse_and_compile` was handed, re-parsed
/// here as raw JSON because the compiled `Style` throws the `"sources"`
/// object away.
///
/// Only the FIRST `StyleLayer::Raster` layer's source is resolved -- the
/// engine assumes one raster source per map throughout (see
/// `map_view.rs`'s own note on `MapView::raster_source` being a single
/// `Option`, not a registry), so a style declaring more than one raster
/// layer is not a case native itself handles either.
fn resolve_raster_config(style: &Style, style_bytes: &[u8]) -> RasterConfig {
    let query_template = query_param("imagery").unwrap_or_default();
    // The plain default (2026-09-29): no imagery unless asked for. The
    // style's own raster source (arm 2) is `?imagery=style`, as native
    // wires it only on `S` -- so Liberty opens as the same vector map on
    // every shell.
    if query_template.is_empty() {
        return RasterConfig { template: String::new(), tile_size: 256, max_zoom: 22 };
    }
    if query_template != "style" {
        return RasterConfig {
            template: query_template,
            tile_size: num_param("imagery_tile_size").map_or(256, |v| v as u32),
            max_zoom: num_param("imagery_max_zoom").map_or(22, |v| v as u8),
        };
    }
    let sources = parse_style_raster_sources(style_bytes);
    style
        .layers
        .iter()
        .find_map(|layer| sources.get(layer.raster_source()?))
        .map(|s| RasterConfig { template: s.tiles.clone(), tile_size: s.tile_size, max_zoom: s.max_zoom })
        .unwrap_or_else(|| RasterConfig { template: String::new(), tile_size: 256, max_zoom: 22 })
}

/// Every log line is redacted: a tile or sprite URL can carry the Mapbox
/// token, and this is the one door they all leave through.
fn log(msg: &str) {
    let msg = &crate::sprite::redact_access_token(msg);
    web_sys::console::log_1(&JsValue::from_str(msg));
    if let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id("log"))
    {
        el.set_text_content(Some(&format!("{}{}\n", el.text_content().unwrap_or_default(), msg)));
    }
}

/// Overwrites the one-line status readout, rather than appending to the log.
fn status(msg: &str) {
    set_text("status", msg);
}

/// `element.textContent = msg`, for an element that may not exist.
///
/// Every readout on the page goes through here, and none of them is
/// required to be present: the page is markup this crate does not own, and
/// a build that renders into a page missing an element should draw a map
/// with one fewer label rather than fail to draw at all.
///
/// Redacted like [`log`]: `#source` shows the tile template in force.
fn set_text(id: &str, msg: &str) {
    let msg = &crate::sprite::redact_access_token(msg);
    if let Some(el) =
        web_sys::window().and_then(|w| w.document()).and_then(|d| d.get_element_by_id(id))
    {
        el.set_text_content(Some(msg));
    }
}

/// Renders the credit the tile source in force requires, into the page's
/// `#credit-lines` container.
///
/// # Why the markup no longer carries the text
///
/// Because `?tiles=` decides which credit is correct, and the markup is
/// written before anyone has typed a URL. `index.html` used to hold one
/// source's two credit lines literally, and pointing the page at OpenFreeMap
/// left them there -- a commercial licence asserted over OpenStreetMap data,
/// and the ODbL credit that data requires missing. So the container is empty
/// in the markup and this fills it, once, from [`attribution_for`].
///
/// One `<p>` per line, `textContent` rather than `innerHTML`: every string
/// here is a `&'static str` from this file, but a credit is the one element
/// on the page whose contents a future change is most likely to want to
/// interpolate a URL into, and the safe habit is cheaper than the audit.
///
/// A missing container is reported and not fatal -- the same rule
/// [`set_text`] follows, for the same reason -- but it IS a defect: a map
/// drawing without its credit is a licence breach. `build.sh` fails the
/// build when the container is absent, which is where this is meant to be
/// caught.
fn render_attribution(template: &str) {
    CREDIT_TEMPLATE.with(|t| *t.borrow_mut() = template.to_string());
    let Some(document) = web_sys::window().and_then(|w| w.document()) else { return };
    let Some(container) = document.get_element_by_id("credit-lines") else {
        log("ATTRIBUTION: the page has no #credit-lines -- the map is drawing WITHOUT its credit");
        return;
    };
    // Clears whatever is there, so a second call cannot stack two sources'
    // credits on top of each other.
    container.set_text_content(Some(""));
    // The engine's one collection rule (`attribution::collect`, as native's
    // `MapView::attributions`): basemap first, then each data layer's, each
    // holder once -- a data layer re-stating the basemap's OpenStreetMap
    // credit is not printed twice.
    use crate::attribution::{Credit, CreditRole};
    let basemap = attribution_for(template);
    let extras = EXTRA_CREDITS.with(|credits| credits.borrow().clone());
    // The space look's assets, while drawn (`set_look_credits`): native's
    // `CreditRole::Asset`, so they sort after every data credit.
    let look = LOOK_CREDITS.with(|credits| credits.borrow().clone());
    let credits = crate::attribution::collect(
        basemap
            .iter()
            .map(|line| Credit::new(CreditRole::Basemap, line.text))
            .chain(extras.into_iter().map(|text| Credit::new(CreditRole::Overlay, text)))
            .chain(look.into_iter().map(|text| Credit::new(CreditRole::Asset, text))),
    );
    for credit in &credits {
        let Ok(p) = document.create_element("p") else { continue };
        p.set_text_content(Some(&credit.text));
        if let Some(lang) = basemap.iter().find(|l| l.text == credit.text).and_then(|l| l.lang) {
            let _ = p.set_attribute("lang", lang);
        }
        let _ = container.append_child(&p);
        // In the console as well as on the page: the credit is the answer to
        // "whose data is this", and a capture of the console is the form
        // that question usually gets asked in.
        log(&format!("attribution: {}", credit.text));
    }
    apply_credit_compact();
}

/// The credit card's compact toggle, as native's plate
/// (`attribution::CompactState`, after MapLibre's compact attribution): a
/// card taller than `attribution::COMPACT_HEIGHT_FRACTION` of the window is
/// compact -- shown in full until the map is first moved, then collapsed
/// to the "i" button (`#credit-toggle`), which opens and closes it.
fn apply_credit_compact() {
    let Some(document) = web_sys::window().and_then(|w| w.document()) else { return };
    let (Some(credit), Some(lines), Some(toggle)) = (
        document.get_element_by_id("credit"),
        document.get_element_by_id("credit-lines").and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok()),
        document.get_element_by_id("credit-toggle"),
    ) else {
        return;
    };
    wire_credit_toggle(&toggle);
    // Measured expanded: a collapsed card's lines are not laid out.
    let _ = credit.set_attribute("class", "layer");
    let height = lines.offset_height() as f32;
    let window_height = web_sys::window().and_then(|w| w.inner_height().ok()).and_then(|h| h.as_f64()).unwrap_or(0.0) as f32;
    let compact = crate::attribution::needs_compact(height, window_height);
    CREDIT_COMPACT.with(|c| c.borrow_mut().1 = compact);
    let collapsed = CREDIT_COMPACT.with(|c| c.borrow().0.collapsed(compact));
    let class = match (compact, collapsed) {
        (true, true) => "layer compact collapsed",
        (true, false) => "layer compact",
        _ => "layer",
    };
    let _ = credit.set_attribute("class", class);
    if compact {
        let _ = toggle.remove_attribute("hidden");
        let _ = toggle.set_attribute("aria-expanded", if collapsed { "false" } else { "true" });
    } else {
        let _ = toggle.set_attribute("hidden", "");
    }
}

/// The map moved: a compact card collapses, the first time.
fn credit_map_moved() {
    let changed = CREDIT_COMPACT.with(|c| {
        let mut c = c.borrow_mut();
        let compact = c.1;
        c.0.map_moved(compact)
    });
    if changed {
        apply_credit_compact();
    }
}

/// `#credit-toggle`'s click, wired once.
fn wire_credit_toggle(toggle: &web_sys::Element) {
    if CREDIT_TOGGLE_WIRED.with(|w| w.replace(true)) {
        return;
    }
    let on_click = Closure::<dyn FnMut()>::new(move || {
        CREDIT_COMPACT.with(|c| c.borrow_mut().0.toggle());
        apply_credit_compact();
    });
    let _ = toggle.add_event_listener_with_callback("click", on_click.as_ref().unchecked_ref());
    on_click.forget();
}

thread_local! {
    /// Credits of data layers drawn over the basemap, in the order added.
    static EXTRA_CREDITS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    /// The basemap template [`render_attribution`] last drew, so a new
    /// data credit can redraw the line.
    static CREDIT_TEMPLATE: RefCell<String> = const { RefCell::new(String::new()) };
    /// The credit card's compact toggle, and whether the card is compact.
    static CREDIT_COMPACT: RefCell<(crate::attribution::CompactState, bool)> =
        RefCell::new((crate::attribution::CompactState::default(), false));
    /// Whether `#credit-toggle`'s click is wired.
    static CREDIT_TOGGLE_WIRED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The space look's credits the last frame drew (`look_credits`).
    static LOOK_CREDITS: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
}

/// The credit an `?imagery=` template's source requires, for the one source
/// this page is pointed at in public: EOX's Sentinel-2 cloudless, CC BY 4.0,
/// the launch site's globe. Any other template is the caller's to credit,
/// as a `?tiles=` source is.
fn imagery_credit(template: &str) -> Option<&'static str> {
    template
        .contains("tiles.maps.eox.at/wmts/1.0.0/s2cloudless")
        .then_some("Imagery: Sentinel-2 cloudless 2016 by EOX IT Services GmbH (CC BY 4.0)")
}

/// The space look's credits for this frame (`space_look::look_credits`):
/// the credit line is redrawn only when they change -- the look switching
/// on, its textures landing, the clouds fading out past z5 -- not per frame.
fn set_look_credits(credits: Vec<&'static str>) {
    let changed = LOOK_CREDITS.with(|held| {
        let mut held = held.borrow_mut();
        let changed = *held != credits;
        if changed {
            *held = credits;
        }
        changed
    });
    if changed {
        let template = CREDIT_TEMPLATE.with(|t| t.borrow().clone());
        render_attribution(&template);
    }
}

/// Adds a data layer's credit under the basemap's (once).
fn add_credit(text: &str) {
    let added = EXTRA_CREDITS.with(|credits| {
        let mut credits = credits.borrow_mut();
        let new = !credits.iter().any(|c| c == text);
        if new {
            credits.push(text.to_string());
        }
        new
    });
    if added {
        log(&format!("attribution: {text}"));
        let template = CREDIT_TEMPLATE.with(|t| t.borrow().clone());
        render_attribution(&template);
    }
}

/// The boot card's one line of prose -- "compiling the style", "16 tiles" --
/// so that the first second of the page says what it is doing instead of
/// showing an empty canvas.
///
/// This is the half of "starts fast" that is not a number. The wasm module
/// alone is ~4.7 MB and takes a few hundred milliseconds to fetch, compile
/// and instantiate before a single line of this crate runs; the tiles then
/// arrive over another few hundred. A blank canvas for that whole window
/// reads as broken, and it read as broken to a previous round of this work
/// -- see `start_hidden_pump`, where a stuck "starting..." was diagnosed as
/// a frozen renderer when it was a backgrounded tab.
fn boot_stage(msg: &str) {
    set_text("bootstage", msg);
}

/// Dismisses the boot card. Idempotent -- adding a class that is already
/// there is a no-op, and the CSS transition only runs on the edge.
fn boot_done() {
    if let Some(el) =
        web_sys::window().and_then(|w| w.document()).and_then(|d| d.get_element_by_id("boot"))
    {
        let _ = el.set_attribute("class", "gone");
    }
}

/// `fetch(url)` -> bytes. This is the whole answer to "where does the data
/// come from once `std::fs` is gone": the engine never opened a file in the
/// first place -- `mvt::decode` and `style_spec::parse_and_compile` take
/// `Vec<u8>`/`&[u8]` -- so the I/O layer is the only thing that changed, and
/// it is these twenty lines. `Ok(None)` is a 404, which OpenFreeMap returns
/// for a tile with no data and which must not be retried forever.
async fn fetch_bytes(url: &str) -> Result<Option<Vec<u8>>, JsValue> {
    let window = web_sys::window().ok_or_else(|| JsValue::from_str("no window"))?;
    let response: web_sys::Response =
        wasm_bindgen_futures::JsFuture::from(window.fetch_with_str(url)).await?.dyn_into()?;
    if response.status() == 404 {
        return Ok(None);
    }
    if !response.ok() {
        return Err(JsValue::from_str(&crate::sprite::redact_access_token(&format!(
            "{} returned HTTP {}",
            url,
            response.status()
        ))));
    }
    let buffer = wasm_bindgen_futures::JsFuture::from(response.array_buffer()?).await?;
    Ok(Some(js_sys::Uint8Array::new(&buffer).to_vec()))
}

/// What one tile request came back with. [`fetch_bytes`]'s tile-only twin:
/// a tile must tell ABSENCE (remembered for the session) from a transient
/// failure (retried with backoff, see `crate::fetch_policy`), and
/// `fetch_bytes` folds every non-404 into one `Err`.
enum TileFetch {
    Body(Vec<u8>),
    /// 404/410/204 -- see `fetch_policy::classify_status`.
    Absent,
    /// A network error or a retryable status. `message` never contains the
    /// URL: a tile URL can carry an access token.
    Failed { message: String, retry_after_ms: Option<f64> },
}

async fn fetch_tile(url: &str) -> TileFetch {
    let failed = |message: String| TileFetch::Failed { message, retry_after_ms: None };
    let Some(window) = web_sys::window() else { return failed("no window".into()) };
    let response: web_sys::Response =
        match wasm_bindgen_futures::JsFuture::from(window.fetch_with_str(url)).await.and_then(|r| r.dyn_into()) {
            Ok(response) => response,
            // A `fetch` rejection is a network error (offline, reset, CORS
            // on an error page) -- transient by definition.
            Err(_) => return failed("network error".into()),
        };
    let status = response.status();
    if !((200..300).contains(&status) && status != 204) {
        // Readable cross-origin only when the server lists it in
        // `Access-Control-Expose-Headers`; `None` falls back to the backoff.
        let retry_after = response.headers().get("retry-after").ok().flatten();
        return match crate::fetch_policy::classify_status(status, retry_after.as_deref()) {
            crate::fetch_policy::StatusVerdict::Absent => TileFetch::Absent,
            crate::fetch_policy::StatusVerdict::Retry { retry_after_ms } => {
                TileFetch::Failed { message: format!("HTTP {status}"), retry_after_ms }
            }
        };
    }
    let buffer = match response.array_buffer() {
        Ok(promise) => wasm_bindgen_futures::JsFuture::from(promise).await,
        Err(err) => Err(err),
    };
    match buffer {
        Ok(buffer) => TileFetch::Body(js_sys::Uint8Array::new(&buffer).to_vec()),
        Err(_) => failed("body cut off".into()),
    }
}

/// The faces this page ships, fetched at startup and handed to `fontdb`.
///
/// # Why these files and not a system font
///
/// Because a browser has no font book to enumerate. `fontdb`'s
/// `load_system_fonts` gates its directory scan on `target_os` and no arm
/// matches `wasm32-unknown-unknown`: it compiles, returns, and leaves the
/// database EMPTY. Every label then shapes against nothing and draws
/// nothing, with no error on any channel -- which is exactly what this page
/// did until these two files existed.
///
/// # Why a subset, and how big a one
///
/// This subset was cut for an earlier build of this page whose corpus was
/// Japan and nothing else, so what it would ever need to draw was known
/// rather than guessed: a Japan map-feature table held every string that
/// renderer could ever label, and across all 132,207 distinct ones there
/// were exactly **2,836 distinct codepoints** -- 2,612 CJK ideographs, 86
/// katakana, 71 hiragana, 31 fullwidth forms (`国道２０２号線` is 7.3% of
/// all drawn characters), 30 ASCII, and a handful of punctuation. No Ext A,
/// no Ext B, no Hangul, no emoji, no Cyrillic. The default corpus is wider
/// now ([`TILE_URL`], OpenFreeMap's planet), and the Latin pair below is
/// what covers the gap that opened -- Japanese text still shapes from this
/// subset, byte for byte.
///
/// Noto Sans JP subset to exactly those codepoints, layout features off:
///
/// ```text
///                                    raw        gzip -9    brotli -q11
///   Noto Sans JP Regular, full   4,533,028    3,882,739    3,352,941
///   ... the same, subset           691,816      616,616      556,164
///   Noto Sans JP Bold, subset      710,232      633,194      572,979
///   (for scale) the wasm module  4,747,382    1,464,295    1,045,263
/// ```
///
/// For contrast, the SDF glyph PBFs a tile server of ours serves for
/// the same coverage are **51 MB**. The subset is 1.1% of that, with
/// strictly better coverage -- it keeps U+FA11, the one Compatibility
/// Ideograph a 256-aligned range cut drops.
///
/// # Why fetched rather than `include_bytes!`
///
/// Because the wasm module has to be fully downloaded and compiled before
/// ANY of it runs. Embedding 1.4 MB would put the whole font on the
/// critical path to the first triangle, in exchange for nothing: there is
/// no label to draw before there is a tile to draw it on. Fetched, the two
/// faces arrive alongside the tiles and labels appear on the first frame
/// after they land. They also cache independently of the module, so a
/// rebuild does not re-download them.
///
/// # The Latin pair, and why the Japan subset alone was not enough
///
/// `japan-codepoints.txt` is the codepoints that occur in
/// that table, and Japanese place names contain **no ASCII
/// capitals**: all 26 of `U+0041`-`U+005A` are absent from both Japan
/// faces, along with lowercase `b d g h k m q v z`. That was invisible on
/// the corpus the subset was cut for and is ruinous anywhere else on
/// OpenStreetMap -- which is everywhere the default corpus now draws, not
/// only a `?tiles=` override -- "Singapore" drew as "in apore", and every
/// expressway shield (AYE, ECP, CTE, PIE, ...) drew as an empty plate.
/// `sprites/README.md` had already recorded the gap for the sprite sheet's
/// shield text and said fixing it "is a font change, not
/// a sprite one". This is that change.
///
/// Noto Sans (Latin) Regular and Bold, subset to Basic Latin, Latin-1
/// Supplement, Latin Extended-A and -B, Latin Extended Additional (the
/// Vietnamese alphabet), the combining diacritics and the handful of
/// punctuation marks place names actually use -- 911 codepoints,
/// **70,976 B + 70,876 B = 142 KB**, or +10% on the 1,402,048 B the two
/// Japan faces already cost. See `assets/fonts/README.md` for why Extended-A
/// alone was not enough. Both
/// weights, because the styles ask for Regular and Bold.
///
/// # ADDED to the Japan pair, never substituted for it
///
/// Order matters for reading this list, not for resolution:
/// [`FONT_FAMILY`] is what every `text-font` stack resolves to, and it
/// stays `Noto Sans JP`. So Japanese, the digits and the lowercase Latin
/// the Japan subset already covers all keep coming from the Japan faces,
/// byte for byte as before; the Latin faces are reached only per-glyph, for
/// a codepoint the requested family cannot draw.
///
/// That path already existed and needed nothing: `font_fallback`'s
/// `script_fallback` is deliberately EMPTY for Latin, which hands the
/// codepoint to `COMMON_FALLBACK` -- and `COMMON_FALLBACK` already names
/// `"Noto Sans"` (`font_fallback.rs:144`) for exactly this, the
/// "Linux, and the usual web-served set" entry. Loading these two faces
/// into the same `fontdb::Database` is the whole wiring; there is no attrs
/// or family-list change, and a mixed word like "Singapore" is shaped as
/// one run with two faces in it.
///
/// # Licence
///
/// SIL Open Font License 1.1 for both pairs, shipped beside the fonts as
/// `dist/LICENSE-OFL.txt` -- `build.sh` copies it and fails the build if it
/// is missing, because a font redistributed without its licence is a
/// licence breach and not a missing file.
///
/// The two pairs have DIFFERENT copyright holders, and the notice carries
/// both: Noto Sans JP is Adobe's (it is built from Source Han Sans, hence
/// the Reserved Font Name "Source"), and Noto Sans is Google's with no
/// reserved name. Both lines are taken from the fonts' own `name` table.
const FONT_URLS: &[&str] = &[
    "./NotoSansJP-Regular.japan-subset.otf",
    "./NotoSansJP-Bold.japan-subset.otf",
    "./NotoSans-Regular.latin-subset.ttf",
    "./NotoSans-Bold.latin-subset.ttf",
];

/// The family every `text-font` stack falls back to here.
///
/// The vendored styles ask for `Noto Sans Regular`/`Bold`/`Italic`, which
/// are the names of Mapbox GLYPH PBFs rather than of any installed face --
/// no font database anywhere indexes a family called "Noto Sans Regular".
/// `font_fallback::resolve_font_stack` handles that by keeping the WEIGHT
/// from the stack's first entry and taking the family from a default; on
/// macOS that default is a real system face, and here it has to be the one
/// face this page shipped. See `font_fallback::resolve_font_stack_with_default`.
const FONT_FAMILY: &str = "Noto Sans JP";

/// Fetches [`FONT_URLS`], builds the text renderer, and installs it on the
/// live map.
///
/// Spawned, never awaited: see [`Labels`]. A failure here is logged and
/// leaves `labels` as `None`, which draws a map with no text rather than no
/// map -- the same degradation the native build takes when a face is
/// missing.
async fn load_font(app: Rc<RefCell<MapApp>>, format: wgpu::TextureFormat) {
    let started = now_ms();
    let mut db = glyphon::fontdb::Database::new();
    let mut bytes_total = 0usize;
    for url in FONT_URLS {
        match fetch_bytes(url).await {
            Ok(Some(bytes)) => {
                bytes_total += bytes.len();
                // `load_font_data` PARSES: a truncated or misserved file is
                // rejected here and simply adds no face, which would leave
                // the database empty in exactly the way this whole function
                // exists to prevent. The face count is checked below.
                db.load_font_data(bytes);
            }
            Ok(None) => log(&format!("font {url}: 404 -- labels will not draw")),
            Err(err) => log(&format!("font {url}: {err:?} -- labels will not draw")),
        }
    }
    if db.is_empty() {
        log("no font face parsed: the map will draw without labels");
        return;
    }
    let families: Vec<String> = db
        .faces()
        .flat_map(|face| face.families.iter().map(|(name, _)| name.clone()))
        .collect();

    // `navigator.language`, not a hardcoded locale, and it is not
    // decoration: the locale is the ONLY input Han unification has (see
    // `font_fallback::han_unification`). A machine reporting `ja-JP` gets
    // Japanese glyph forms for the shared CJK ideographs; one reporting
    // `zh-TW` gets traditional ones. Reading it here is what keeps this from
    // being a Japan special case in code as well as in data.
    let locale = web_sys::window()
        .map(|w| w.navigator().language().unwrap_or_default())
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| "ja-JP".to_string());
    let font_system = crate::font_fallback::new_font_system_from_db(locale.clone(), db);

    let mut app_mut = app.borrow_mut();
    let device = &app_mut.ctx.device;
    let queue = &app_mut.ctx.queue;
    let swash_cache = glyphon::SwashCache::new();
    let cache = glyphon::Cache::new(device);
    let viewport = glyphon::Viewport::new(device, &cache);
    // `ColorMode::Web`, not glyphon's `Accurate` default, for the same
    // reason this page's surface is `Unorm`: `Accurate` linearises the label
    // colour in glyphon's vertex shader on the assumption that an `Srgb`
    // target will re-encode it, and there is no encode to pair with. This is
    // `MapView::ensure_gpu`'s choice, restated because the two call sites
    // are in different crates and must not drift.
    let mut atlas = glyphon::TextAtlas::with_color_mode(device, queue, &cache, format, glyphon::ColorMode::Web);
    let renderer =
        glyphon::TextRenderer::new(&mut atlas, device, wgpu::MultisampleState::default(), None);
    log(&format!(
        "fonts: {} B over {} file(s) in {:.0} ms, {} face name(s) [{}], locale {locale}",
        bytes_total,
        FONT_URLS.len(),
        now_ms() - started,
        families.len(),
        families.join(", "),
    ));
    app_mut.labels = Some(Labels {
        font_system,
        swash_cache,
        cache,
        viewport,
        atlas,
        renderer,
        shape_cache: crate::symbol_pass::LabelShapeCache::with_default_family(FONT_FAMILY),
        // Degenerate until the first frame resets it to the real padded
        // viewport -- `MapView`'s own `collision_index` starts the same way.
        collision: crate::collision::CollisionIndex::new(crate::collision::CollisionBox {
            left: 0.0,
            top: 0.0,
            right: 0.0,
            bottom: 0.0,
        }),
        glyph_atlas: crate::render::GlyphAtlas::new(),
        glyph_atlas_bind_group: None,
        glyph_atlas_generation: None,
        census: LabelCensus::default(),
        texts: Vec::new(),
        families,
    });
    app_mut.needs_redraw = true;
    app_mut.startup.mark_once("font ready");
}

/// The sprite sheet, once it has landed: everything the icon half of the
/// symbol pass needs and nothing else.
///
/// `MapApp::icons` is `Option<Icons>` for the same reason `labels` is
/// `Option<Labels>`: the sheet arrives over the network after the first
/// triangle, and a page that never gets one must still draw a map. `None`
/// here is not a browser special case -- it is bit-for-bit what the native
/// build looks like before `MapView::set_sprites` is called, right down to
/// `icon_atlas_size` being `(0, 0)` and every icon-bearing feature drawing
/// its text and skipping its icon.
struct Icons {
    pipeline: IconPipeline,
    /// The bind group over the packed atlas TEXTURE. Built once, at load;
    /// the texture itself is owned by the bind group and nothing else holds
    /// it, which is why this field and not a `wgpu::Texture` is what is
    /// kept.
    bind_group: wgpu::BindGroup,
    /// Where each named sprite sits in the PACKED atlas -- `pack_icon_atlas`'
    /// output, not the sheet's own manifest rects. The two differ: the
    /// sheet is packed however its generator laid it out, and the atlas is
    /// repacked 1024 wide with a one-texel gutter.
    rects: HashMap<String, IconRect>,
    /// `rects` inverted: packed atlas origin -> sprite name. Built once, at
    /// load, so that an instrument can name the sprite behind a drawn quad
    /// WITHOUT the drawing path carrying a `String` it does not need.
    ///
    /// An `IconInstance` deliberately carries no name -- it is a screen rect
    /// and a UV rect, and adding an owned name to it would mean one
    /// allocation per icon per frame on a path shared with the native
    /// build. But `uv_min` is exactly `(rect.x / atlas_w, rect.y / atlas_h)`
    /// (`symbol_pass.rs`), so multiplying back and rounding recovers the
    /// origin, and the origin is unique per sprite.
    by_origin: HashMap<(u32, u32), String>,
    atlas_size: (u32, u32),
    /// How long `IconPipeline::render` took on the last frame that had any
    /// icons, and the running max/sum -- the `pass_ms` of the icon half.
    /// Read back by [`map_icons_json`]. See [`LabelCensus`] for why a
    /// separate number from the label pass's is worth having: both sit
    /// inside one `Fifo` frame that is pinned at 16.7 ms regardless.
    draw_ms: f64,
    draw_ms_max: f64,
    draw_ms_sum: f64,
    draw_frames: u32,
    /// Quads drawn on the last frame, and the sprite names behind them, in
    /// draw order -- see [`ICON_NAME_DUMP`]. `names` is empty until an
    /// instrument switches the dump on.
    drawn: usize,
    names: Vec<String>,
    /// Sprite names the STYLE asked for that this sheet does not carry, in
    /// first-seen order, capped. A shield that draws as nothing has exactly
    /// two causes -- the expression resolved to a name, and the sheet had
    /// no such name -- and this separates them without a screenshot.
    missing: Vec<String>,
    /// Total entries in the sheet, and the sheet's own pixel dimensions
    /// before repacking. Logged once, reported by [`map_icons_json`].
    sheet_entries: usize,
    sheet_bytes: usize,
}

/// Cap on [`Icons::missing`]: a data-driven `icon-image` over a POI layer
/// can name hundreds of distinct classes, and this list is a diagnostic
/// rather than a census.
const MAX_MISSING_ICON_NAMES: usize = 64;

thread_local! {
    /// Off until an instrument asks, exactly like [`LABEL_TEXT_DUMP`]: a
    /// frame-time measurement must not be paying to collect a list nobody
    /// reads. Latched on by the first [`map_icons_json`] call, so its first
    /// answer carries counts with an empty `names` and the next frame fills
    /// it.
    static ICON_NAME_DUMP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The sprite base URL this page will actually fetch.
///
/// Thin glue over `sprite::resolve_sprite_base` -- the whole resolution
/// rule lives there, pure and unit-tested without a browser, because it is
/// exactly the function this page and (via `resolve_sprite_url`) native's
/// `examples/basic_map.rs` need to agree on. All this does is read the two
/// query params and hand them over: `?sprite=<base>` overrides everything
/// (and `?sprite=none` is icons-off), otherwise `style_sprite` --
/// `Style::sprite`, the loaded style's own `"sprite"` field -- is resolved
/// against an optional `?token=` (an opt-in, run-time-only escape hatch for
/// a local `mapbox://` demo; never read from a committed file), falling
/// back to [`SPRITE_URL`] exactly where `resolve_sprite_base` says to.
///
/// Over the Mapbox `composite` source the token in force ([`mapbox_token`])
/// is passed too, so Mapbox Streets draws Mapbox's own sheet -- the one
/// native's `examples/mapbox_streets_demo.rs` fetches through the same
/// `resolve_sprite_url`. An OMT style keeps today's behaviour: our sheet,
/// unless `?token=` asks otherwise.
fn sprite_base(style_sprite: Option<&str>) -> Option<String> {
    let token = query_param("token").or_else(|| {
        (TILE_SCHEMA.with(|s| s.get()) == crate::basemap::TileSchema::MapboxComposite)
            .then(mapbox_token)
            .flatten()
    });
    crate::sprite::resolve_sprite_base(
        query_param("sprite").as_deref(),
        token.as_deref(),
        style_sprite,
        SPRITE_URL,
    )
}

/// Fetches the sprite sheet, packs it into one GPU atlas, and installs it
/// on the live map.
///
/// Spawned, never awaited, same as [`load_font`] and for the same reason:
/// there is no icon to draw before there is a road under it, and putting a
/// second network round trip in front of the first triangle buys nothing.
///
/// # This is `sprite::fetch_sprite_atlas`, minus `reqwest`
///
/// Every step below the fetch is the SHIPPED code:
/// `sprite::sprite_urls` picks the two URLs, `sprite::parse_sprite_manifest`
/// reads the index, `sprite::slice_atlas` cuts the sheet into named images,
/// and `render::icon_pipeline::pack_icon_atlas` repacks those into the
/// texture the shader samples. `fetch_sprite_atlas` itself is
/// `cfg(not(target_arch = "wasm32"))` only because `reqwest` has no wasm
/// build -- the four functions it calls all cross untouched, which is why
/// this function is a dozen lines and not a port.
///
/// A failure at any step costs icons and nothing else: it is logged,
/// `app.icons` stays `None`, and every label still draws.
async fn load_sprites(app: Rc<RefCell<MapApp>>, format: wgpu::TextureFormat) {
    // Read before the `base` match so the borrow does not need to live
    // across the `await`s below it.
    let style_sprite = app.borrow().style.sprite.clone();
    let Some(base) = sprite_base(style_sprite.as_deref()) else {
        log("sprites: ?sprite=none -- icons off, labels unaffected");
        return;
    };
    let started = now_ms();
    let (json_url, png_url) = crate::sprite::sprite_urls(&base);
    let json = match fetch_bytes(&json_url).await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            log(&format!("sprites: {json_url} 404 -- icons will not draw"));
            return;
        }
        Err(err) => {
            log(&format!("sprites: {json_url} {err:?} -- icons will not draw"));
            return;
        }
    };
    let png = match fetch_bytes(&png_url).await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            log(&format!("sprites: {png_url} 404 -- icons will not draw"));
            return;
        }
        Err(err) => {
            log(&format!("sprites: {png_url} {err:?} -- icons will not draw"));
            return;
        }
    };
    let sheet_bytes = json.len() + png.len();
    let manifest = match crate::sprite::parse_sprite_manifest(&json) {
        Ok(manifest) => manifest,
        Err(err) => {
            log(&format!("sprites: {json_url} is not a sprite manifest: {err:#}"));
            return;
        }
    };
    let sheet_entries = manifest.len();
    // `slice_atlas` decodes the PNG and cuts one `RgbaImage` per manifest
    // entry. `image` is compiled here with `default-features = false,
    // features = ["png"]` -- which is exactly the decoder a sprite sheet
    // needs and none of the ones it does not.
    let sprites = match crate::sprite::slice_atlas(&png, &manifest) {
        Ok(sprites) => sprites,
        Err(err) => {
            log(&format!("sprites: {png_url} does not match its manifest: {err:#}"));
            return;
        }
    };
    let decoded_ms = now_ms() - started;
    let names: Vec<String> = {
        // Sorted, so the packed atlas is the same texture on every load and
        // a screenshot diff is not defeated by `HashMap` iteration order.
        let mut names: Vec<String> = sprites.names().cloned().collect();
        names.sort();
        names
    };
    let (atlas_image, rects) = pack_icon_atlas(&sprites, &names);
    let atlas_size = atlas_image.dimensions();
    let by_origin: HashMap<(u32, u32), String> =
        rects.iter().map(|(name, rect)| ((rect.x, rect.y), name.clone())).collect();

    let mut app_mut = app.borrow_mut();
    let pipeline = IconPipeline::new(&app_mut.ctx, format);
    let bind_group = pipeline.upload_atlas(&app_mut.ctx, &atlas_image);
    log(&format!(
        "sprites: {sheet_entries} entries, {sheet_bytes} B over 2 files, decoded in \
         {decoded_ms:.0} ms, packed to {}x{} ({:.1} MB on the GPU), total {:.0} ms",
        atlas_size.0,
        atlas_size.1,
        (atlas_size.0 as f64 * atlas_size.1 as f64 * 4.0) / 1e6,
        now_ms() - started,
    ));
    app_mut.icons = Some(Icons {
        pipeline,
        bind_group,
        rects,
        by_origin,
        atlas_size,
        draw_ms: 0.0,
        draw_ms_max: 0.0,
        draw_ms_sum: 0.0,
        draw_frames: 0,
        drawn: 0,
        names: Vec::new(),
        missing: Vec::new(),
        sheet_entries,
        sheet_bytes,
    });
    app_mut.needs_redraw = true;
    app_mut.startup.mark_once("sprites ready");
}

/// The space look's star map, next to the page. `build.sh` copies it into
/// `dist/space/` under `space_assets::STARMAP`'s file name, checked against
/// that pin by `scripts/fetch-space-assets.sh`.
const SPACE_STARMAP_URL: &str = "./space/starmap_2020_3840.jpg";
/// The space look's cloud map; `space_assets::CLOUDS`, as above.
const SPACE_CLOUDS_URL: &str = "./space/cloud_combined_2048.jpg";

/// Fetches and decodes the space look's two textures (`?look=space`), and
/// hands them to the next frame to upload.
///
/// Native's `space_assets::load_cached_textures`, minus the disk: the files
/// are static next to the page, verified at build time rather than here
/// (this crate has no `sha2`). Spawned, never awaited: until they land the
/// look draws its shading and rim alone, as native's does while its
/// background load runs. A texture that fails is logged and left `None`,
/// and that half is not drawn -- the plain sky, never a broken frame.
async fn load_space_textures(app: Rc<RefCell<MapApp>>) {
    if !app.borrow().space_look.enabled {
        return;
    }
    let started = now_ms();
    let fetch = |url: &'static str| async move {
        let bytes = match fetch_bytes(url).await {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                log(&format!("space look: {url} 404 -- that half is not drawn"));
                return None;
            }
            Err(err) => {
                log(&format!("space look: {url} {err:?} -- that half is not drawn"));
                return None;
            }
        };
        match image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg) {
            Ok(image) => {
                let image = image.to_rgba8();
                let (width, height) = image.dimensions();
                Some(crate::render::space_look::EquirectImage { width, height, rgba: image.into_raw() })
            }
            Err(err) => {
                log(&format!("space look: {url} is not a JPEG: {err} -- that half is not drawn"));
                None
            }
        }
    };
    let textures = crate::render::space_look::SpaceTextures {
        starmap: fetch(SPACE_STARMAP_URL).await,
        clouds: fetch(SPACE_CLOUDS_URL).await,
    };
    let size = |image: &Option<crate::render::space_look::EquirectImage>| {
        image.as_ref().map_or("missing".into(), |i| format!("{}x{}", i.width, i.height))
    };
    log(&format!(
        "space look: star map {}, clouds {}, fetched and decoded in {:.0} ms",
        size(&textures.starmap),
        size(&textures.clouds),
        now_ms() - started
    ));
    let mut app_mut = app.borrow_mut();
    app_mut.space_textures_generation += 1;
    app_mut.space_textures = Some(textures);
    app_mut.needs_redraw = true;
}

/// The backing store's size in physical pixels, and the css-to-physical
/// scale that produced it.
///
/// `devicePixelRatio` is what you want; `max_texture_dimension_2d` is what
/// you may have. WebGL2's downlevel limits cap that at 2048, which a
/// 1100 CSS px window at dpr 2 already exceeds -- and `Surface::configure`
/// treats an over-sized surface as a VALIDATION ERROR, i.e. a wgpu panic,
/// not a soft fallback. (That is not hypothetical: `?gl` panicked outright
/// at exactly this window size before this clamp existed.)
///
/// Both axes take the SAME scale, deliberately: a per-axis clamp would fit
/// the limit and reintroduce the stretch this whole change is about. Going
/// soft is the acceptable degradation; going anamorphic is not.
fn backing_store_size(css_width: f32, css_height: f32, dpr: f32, max_dim: u32) -> (u32, u32, f32) {
    let max_dim = max_dim as f32;
    let scale = dpr.min(max_dim / css_width).min(max_dim / css_height).max(0.1);
    let width = ((css_width * scale).round() as u32).max(1);
    let height = ((css_height * scale).round() as u32).max(1);
    (width, height, scale)
}

/// One tile's GPU residency: its buffers, its model uniform, and the zoom its
/// `PaintPlan` was resolved at.
///
/// `baked_zoom` is the whole reason this is not just a `TileBuffers`. Paint is
/// resolved per integer zoom bucket (`style::match_tile`'s `zoom` argument),
/// so crossing a bucket makes every mesh stale even though the geometry did
/// not move. Native calls this `bake_zoom` and re-bakes lazily in `plan_for`;
/// the same check is `baked_zoom != display_zoom` below.
struct GpuTile {
    baked_zoom: u8,
    buffers: TileBuffers,
    model_buffer: TrackedBuffer,
    model_bind_group: wgpu::BindGroup,
    /// Every label and icon this tile resolved at `baked_zoom`, from
    /// `style::match_tile_symbols` -- the symbol-layer twin of the
    /// `PaintPlan` the mesh above was tessellated from.
    ///
    /// Baked HERE, beside the mesh, rather than lazily on the read that
    /// needs it (which is what `MapView::symbol_plan_for` does natively):
    /// `baked_zoom` already gates the mesh, both plans go stale on exactly
    /// the same zoom-bucket crossing, and a browser has no worker thread to
    /// defer the second bake to anyway. One `Rc` so a frame can hold it
    /// while the map mutates around it.
    symbols: Rc<crate::style::SymbolPlan>,
    /// The plan's `fill-extrusion` roofs and walls
    /// (`render::extrusion::tessellate_extrusions`), uploaded in the same
    /// bake as `buffers` -- `MapView`'s `CachedTile::extrusion`. `None` when
    /// the tile has no buildings (`create_extrusion_buffers` never builds an
    /// empty upload). A tile with ONLY buildings still gets its (empty) flat
    /// buffers here: this shell never skips an empty flat mesh, so the
    /// `draws_anything` gate `MapView` needed is already the behaviour.
    extrusion: Option<crate::render::extrusion_gpu::ExtrusionGpu>,
}

/// One decoded imagery tile's GPU residency: its texture, and the two bind
/// groups the raster pass draws it with. The browser's twin of
/// `map_view.rs`'s `HeldRasterTile` -- same shape, minus the byte-budget
/// cache it lives in there: this page bounds `MapApp::raster_tiles` with
/// [`MAX_RASTER_TILES`] instead, the same crude cap `MAX_GPU_TILES` already
/// is for the vector meshes.
struct HeldRasterTile {
    texture: crate::render::raster_pipeline::RasterTexture,
    /// Which shared square (`render::raster_mesh`) this tile draws with --
    /// looked up in `MapApp::raster_squares`, not held here, for the same
    /// reason native's does not hold its own copy either.
    mesh_key: RasterMeshKey,
    model_buffer: TrackedBuffer,
    model_bind_group: wgpu::BindGroup,
    paint_bind_group: wgpu::BindGroup,
    /// Held only so the buffer the bind group points at stays alive; never
    /// read directly. Native calls the same field `_paint_buffer` for the
    /// same reason.
    _paint_buffer: TrackedBuffer,
    /// The `raster-opacity` `paint_bind_group` was built with, so a style
    /// whose resolved opacity actually moves rewrites the bind group and a
    /// constant one (every raster style this repo carries) never does.
    paint_opacity: f32,
}

/// The ocean sphere's GPU residency -- `render::space::backdrop_mesh` for one
/// background-colour PAIR, plus the model uniform that places it.
///
/// The same shape `map_view.rs`'s `Backdrop` has, and held for the same
/// reason: the mesh is 4,225 vertices and depends on nothing but the colour
/// pair, so a camera move rewrites the 192-byte uniform and leaves the
/// buffers alone. `color` is the rebuild key -- the PAIR, not a lerped
/// colour, because a zoom-expression `background-color` changes its lerped
/// value on every frame of a zoom and would re-tessellate on every one of
/// them. See `render::space::backdrop_plan`.
struct Backdrop {
    color: (crate::style::Rgba, crate::style::Rgba),
    buffers: TileBuffers,
    model_buffer: TrackedBuffer,
    model_bind_group: wgpu::BindGroup,
}

/// What one frame's labels cost and how many survived, so the browser's
/// answer can be held against the native build's.
///
/// Every field is a count over ONE frame except `shaped_total`, which is
/// cumulative over the session -- it is `LabelShapeCache::shape_count`, and
/// the thing worth knowing about it is that it stops climbing once the view
/// settles. A number that keeps climbing on a still map means the cache is
/// missing every frame, which is the difference between 8 ms and 30.
#[derive(Debug, Clone, Copy, Default)]
struct LabelCensus {
    /// `SymbolFeature`s offered, over every drawn tile.
    features: usize,
    /// Caller-overlay features that offered a label or icon on screen this
    /// frame (`symbol_pass::collect_overlay_candidates`): counted into
    /// `candidates` (or bypassed) with the basemap's.
    overlay_symbols: usize,
    /// Of those, the ones that projected on screen with something to draw.
    candidates: usize,
    /// Of those, the ones collision placed.
    placed: usize,
    /// Placed symbols that carry text (the rest are icon-only).
    labels: usize,
    /// Icon quads this frame drew. Zero before the sprite sheet lands, and
    /// zero forever under `?sprite=none`. Kept beside `labels` rather than
    /// on `Icons` because the two are answers to the same question -- what
    /// did the symbol pass put on screen -- and a placed symbol can carry
    /// one, the other, or both.
    icons: usize,
    /// `glyphon::TextArea`s handed to `prepare` -- one per label plus its
    /// halo copies, so always >= `labels` where any halo is set.
    text_areas: usize,
    /// Cumulative real shaping operations. See the type comment.
    shaped_total: usize,
    /// What this frame's whole label pass cost on the CPU, in
    /// milliseconds -- projection, shaping, collision, the halo fan-out and
    /// glyphon's `prepare` (which is where glyph rasterisation and atlas
    /// upload happen), but not the GPU's own draw.
    ///
    /// Measured because the frame INTERVAL cannot answer the question: the
    /// surface is `PresentMode::Fifo`, so every frame is 16.7 ms whether the
    /// work took 2 ms or 12, and a label pass could double in cost without
    /// moving a single percentile until it went over budget. This is the
    /// number that says how much of the budget the text is using.
    ///
    /// `performance.now()` is coarsened to 100 us without cross-origin
    /// isolation, which is why the running max and mean below are kept
    /// rather than one sample: a 1-3 ms quantity read at 0.1 ms resolution
    /// is fine in aggregate and noisy alone.
    pass_ms: f64,
    /// The largest `pass_ms` since the counters were last reset.
    pass_ms_max: f64,
    /// Frames the label pass has run, so a caller can take a mean.
    pass_frames: u64,
    /// `pass_ms` summed over `pass_frames`.
    pass_ms_sum: f64,
}

/// The text renderer and everything it needs, or nothing at all until the
/// font arrives.
///
/// # Why this is `Option` and not built in `run`
///
/// Because the font is a network fetch and the map is not waiting for it.
/// `MapApp` draws its first triangle at ~400 ms; the subset face is ~692 KB
/// and lands later. Making the whole app await it would put the font on the
/// critical path to FIRST PAINT, in exchange for nothing -- there is no
/// label to draw before there is a tile to draw it on. So the map starts,
/// tiles fill in, and labels appear on the first frame after the bytes land.
///
/// The fields are `MapView`'s `GpuState` fields, one for one, with the same
/// reasons: `cache` is dead but must outlive `viewport` and `atlas`; the
/// atlas is `ColorMode::Web` because the surface is `Unorm`; `collision` is
/// RETAINED across frames and `reset` in place.
struct Labels {
    font_system: glyphon::FontSystem,
    swash_cache: glyphon::SwashCache,
    #[allow(dead_code)]
    cache: glyphon::Cache,
    viewport: glyphon::Viewport,
    atlas: glyphon::TextAtlas,
    renderer: glyphon::TextRenderer,
    shape_cache: crate::symbol_pass::LabelShapeCache,
    collision: crate::collision::CollisionIndex,
    /// Where a line-placed street name rasters its glyphs -- `map_view.rs`'s
    /// `GpuState::glyph_atlas`, one per process/page rather than one per
    /// frame, for the same reason: rasterisation is paid once and the
    /// resident bitmaps are kept. Sampled by `icon_pipeline`'s own pipeline
    /// (a second batch, its own bind group -- see `draw_labels`), not by
    /// glyphon: this atlas and glyphon's `atlas` field above are two
    /// entirely separate textures for two entirely separate label paths.
    glyph_atlas: crate::render::GlyphAtlas,
    /// Built from `glyph_atlas`'s pixels once something has actually
    /// rastered into it, and rebuilt whenever `glyph_atlas.generation()`
    /// moves past `glyph_atlas_generation` -- exactly `GpuState`'s own pair
    /// of fields, and for the same reason: a growing atlas has to be
    /// re-uploaded before a quad addressing its new pixels can draw them.
    glyph_atlas_bind_group: Option<wgpu::BindGroup>,
    glyph_atlas_generation: Option<u64>,
    census: LabelCensus,
    /// The text of every label the last frame drew, in draw order -- but
    /// only once [`LABEL_TEXT_DUMP`] has been switched on, and empty until
    /// then. This is the list a native-vs-browser comparison holds against
    /// `MapView`'s.
    ///
    /// Off by default and latched on by the first [`map_labels_json`] call,
    /// because a frame-time measurement must not be paying for an
    /// instrument nobody is reading -- the same rule `with_app`'s
    /// `try_borrow_mut` follows. The first read after switching it on
    /// therefore returns the counts with an EMPTY list; the next frame
    /// fills it.
    texts: Vec<String>,
    /// The face names the loaded font actually declares, logged once. A
    /// misnamed family is otherwise indistinguishable from a font that
    /// failed to parse: both draw nothing.
    families: Vec<String>,
}

/// One drawn frame's CPU cost, split by pass.
///
/// # Why a per-frame RECORD and not a running mean
///
/// Because the question is a distribution, and two traps have already been
/// walked into on this page by answering it with an average.
///
/// The first: a frame-rate meter measured a DEAD map at a flawless p50 of
/// 8.3 ms, because `requestAnimationFrame` keeps firing at 120 Hz on a page
/// whose renderer has stopped. `interval_ms` alone cannot tell a fast map
/// from a frozen one -- which is why it is recorded beside the pass times
/// and never instead of them, and why [`map_frame_samples_json`] reports the
/// map's own frame counter with it.
///
/// The second: the surface is `PresentMode::Fifo`. Every frame therefore
/// waits for vsync, so `interval_ms` sits at 16.7 ms whether the work took
/// 2 ms or 12, and a pass could double in cost without moving a single
/// percentile until it went over budget. `cpu_ms` and its parts are the
/// numbers that move.
#[derive(Clone, Copy, Default)]
struct FrameSample {
    /// `requestAnimationFrame` delta -- the presented interval, vsync and
    /// all. Only useful for catching frames that MISSED a vsync.
    interval_ms: f32,
    /// Everything `draw` does: uniforms, encode, both passes, submit,
    /// present.
    cpu_ms: f32,
    /// The basemap: camera and model uniform writes, plus the tile render
    /// pass's encode.
    tile_ms: f32,
    /// `draw_labels` end to end -- project, shape, place, glyphon's
    /// `prepare`, and the encode of both the icon and the text draw.
    symbol_ms: f32,
    /// Of `symbol_ms`, the part that is `IconPipeline::render`. Zero with
    /// no sheet, and the number that answers "what do shields cost".
    icon_ms: f32,
    /// `queue.submit` plus `queue.present`.
    submit_ms: f32,
    /// Tiles drawn, so a percentile can be read against how much was on
    /// screen rather than in the abstract.
    drawn: u16,
    /// Icon quads and text areas this frame put up.
    icons: u16,
    labels: u16,
}

/// How many [`FrameSample`]s are kept before the oldest is dropped. 8192 at
/// 60 Hz is a little over two minutes -- longer than any single leg of a
/// measurement run, and 8192 * 24 B is 196 KB, which does not move the heap
/// number the same run is reporting.
const MAX_FRAME_SAMPLES: usize = 8192;

struct MapApp {
    ctx: GpuContext,
    /// Kept so a resize can re-read the CSS box and rewrite the backing
    /// store. The `<canvas>` is the only thing that knows its own size.
    canvas: web_sys::HtmlCanvasElement,
    surface: wgpu::Surface<'static>,
    /// Kept whole rather than rebuilt per resize: `format`, `view_formats`,
    /// `alpha_mode` and `present_mode` were chosen once, against this
    /// adapter's capabilities, and a resize must change only `width`/`height`.
    surface_config: wgpu::SurfaceConfiguration,
    pipeline: TilePipeline,
    depth_view: wgpu::TextureView,
    /// One uniform per `fill-extrusion` layer of the frame's
    /// `render::frame_plan::extrusion_plan`, rewritten every frame --
    /// `MapView`'s `extrusion_params`.
    extrusion_params: crate::render::extrusion_gpu::ExtrusionParamsGpu,
    /// The surface format the pipeline was built against, and the format the
    /// per-frame view is created in. Non-sRGB -- see the surface
    /// configuration in `run` for why that is not an oversight.
    render_format: wgpu::TextureFormat,
    camera_buffer: TrackedBuffer,
    camera_bind_group: wgpu::BindGroup,

    style: Rc<Style>,
    /// Resolved ONCE in `run`, by [`resolve_raster_config`] -- `?imagery=`
    /// if given, else the style's own declared raster source, else empty.
    /// See [`RasterConfig`]'s own doc comment for why both arms have to be
    /// read.
    raster_config: RasterConfig,
    /// The camera and every ease steering it -- see `crate::camera_control`;
    /// this shell never writes a camera field itself.
    camera_control: CameraController,
    /// The reach of this frame's display-zoom flat cover
    /// (`render::cover_plan::FlatCover::reach`), set by `cover()`. `None`
    /// when the globe covered the frame or nothing was cut. Read by the
    /// horizon fade.
    last_cover_reach: std::cell::Cell<Option<f64>>,
    /// The display cover's `FlatCover::building_reach`: where buildings
    /// fade out (`frame_plan::extrusion_frame_depth`).
    last_building_reach: std::cell::Cell<Option<f64>>,
    /// The backing store's ACTUAL css-to-physical scale, which is the ratio
    /// the shader's antialiasing band must use. Equal to `devicePixelRatio`
    /// except when `apply_resize`'s texture-limit clamp bites.
    dpr: f32,
    /// The `devicePixelRatio` `apply_resize` last read. Kept apart from `dpr`
    /// precisely so the clamped case does not read as "changed" forever.
    dpr_seen: f32,
    /// The bearing `sync_compass_dom` last wrote to the DOM compass, so a
    /// still map costs one float comparison a frame rather than a style
    /// write. `None` before the first sync, which always differs from any
    /// real bearing.
    compass_shown_bearing: Option<f32>,
    /// The SWAT mark's ink for this frame, decided in `draw` against what
    /// is behind the mark (`brand_mark::resolve_for_frame`, native's one
    /// call), and the ink `sync_brand_mark_dom` last wrote to `#swatmark`.
    brand_variant: Option<crate::brand_mark::ResolvedVariant>,
    brand_shown: Option<crate::brand_mark::ResolvedVariant>,
    background: wgpu::Color,

    /// WHAT THE GLOBE SITS ON. `MapView`'s `space` field, on this page's own
    /// app struct -- the space colour, the star intensity and the limb glow.
    /// See [`space_default`] for why the star gate opens here and not
    /// natively. Read through `render::frame_plan::space_plan`, never
    /// through `Space::draws_stars` directly -- see `tests/space_plan_parity.rs`.
    space: crate::render::space::Space,
    /// Built on the first frame that actually draws stars and kept: 1.24 MB
    /// of static vertices for all 9,096 catalogue stars plus a WGSL module
    /// the browser has to compile. `None` until then, so `?stars=0` costs a
    /// single `f32` comparison a frame and nothing else. `GpuState::star_pipeline`
    /// is the native twin.
    star_pipeline: Option<crate::render::space::StarPipeline>,
    /// The ocean sphere, rebuilt only when the background colour PAIR
    /// changes -- see [`Backdrop`].
    backdrop: Option<Backdrop>,
    /// The limb glow. Three generated vertices and a 112-byte uniform, so
    /// the lazy build is about the shader module rather than about memory.
    atmosphere_pipeline: Option<crate::render::atmosphere::AtmospherePipeline>,
    /// The sky above the limb (`render::sky`), built the first frame
    /// `SpacePlan::draws_sky` is true. Native's `GpuState::sky_pipeline`.
    sky_pipeline: Option<crate::render::sky::SkyPipeline>,
    /// The opt-in space look (`render::space_look`): `?look=space` turns it
    /// on, as native's `MapView::set_space_look` does. Off by default, and
    /// then nothing below is fetched or built. See [`space_look_default`].
    space_look: crate::render::space_look::SpaceLook,
    /// `?look_time=UNIX`: the instant the look's sky and point stars are
    /// turned to (`globe_capture --look-time`). `None` follows the wall
    /// clock, as native's `read_scene_clock` does with no scene clock.
    look_time: Option<f64>,
    /// Textures [`load_space_textures`] fetched and decoded, waiting for
    /// the next frame to upload them; taken there, so the GPU holds the only
    /// copy afterwards (native's `SpaceTextureLoad::Pending`).
    space_textures: Option<crate::render::space_look::SpaceTextures>,
    /// Bumped per installed texture set; a pipeline of another generation
    /// is rebuilt. 0 is the 1x1 stand-in drawn until the textures land.
    space_textures_generation: u64,
    /// The look's two passes, built the first frame the look draws.
    /// Native's `GpuState::space_look_pipeline`.
    space_look_pipeline: Option<crate::render::space_look::SpaceLookPipeline>,

    /// Decoded MVT, kept so a zoom-bucket crossing can re-bake without
    /// re-fetching. Native's `decoded_cache`, minus the bound.
    decoded: HashMap<TileId, Rc<DecodedTile>>,
    gpu_tiles: HashMap<TileId, GpuTile>,
    inflight: HashSet<TileId>,
    /// Requests waiting to be sent, and the ones on the wire, by priority.
    /// The payload is `(is_prefetch, url)`. See [`Self::request_missing_tiles`].
    fetch_queue: crate::fetch_queue::FetchQueue<TileKey, (bool, String)>,
    /// Tile bodies that arrived but are not decoded yet -- see
    /// [`Self::decode_arrived`]. Their tiles stay in `inflight` /
    /// `raster_inflight` until decoded or discarded.
    arrived: Vec<(TileKey, (bool, Vec<u8>))>,
    /// The last request pass's wanted set: what `decode_arrived` keeps.
    wanted: crate::fetch_queue::Wanted<TileKey>,
    /// Bodies discarded undecoded because the camera had left their tile.
    stale_results_dropped: u64,
    /// Tiles the source has no data for (404/410/204). Without this a 404 is
    /// re-requested every frame forever. Only absence goes here; a transient
    /// failure goes into `retries`, because one timeout used to blank a tile
    /// for the rest of the session.
    ///
    /// Bounded (`raster_retention::AbsentTiles`, native's `absent`): it
    /// grew by one entry per absent tile ever seen.
    missing: crate::raster_retention::AbsentTiles,
    /// Backoff state for vector tiles whose last request failed transiently.
    /// No wake is needed here: the rAF loop calls `request_missing_tiles`
    /// every tick anyway and draws only when a tile lands.
    retries: crate::fetch_policy::RetryTable,

    /// THE IMAGERY BASEMAP. Five fields mirroring `decoded`/`gpu_tiles`/
    /// `inflight`/`missing` above one for one, plus the pipeline and the
    /// shared-square cache -- native's `raster_decoded`/`raster_tiles`/
    /// `raster_pending`/`raster_absent`/`raster_pipeline`/`raster_squares`.
    /// `raster_decoded` is bounded by native's own policy
    /// (`raster_retention::DecodedImagery`: a byte-budgeted LRU that never
    /// evicts the frame's cover, its floor or z0); it was a `HashMap` that
    /// kept every imagery tile the session had ever decoded. The device
    /// side keeps `MAX_RASTER_TILES`, this page's one crude cap. Empty and
    /// free on every page with no imagery source configured -- see
    /// [`RasterConfig`] and [`resolve_raster_config`].
    raster_decoded: crate::raster_retention::DecodedImagery<Rc<image::RgbaImage>>,
    raster_tiles: HashMap<TileId, HeldRasterTile>,
    raster_inflight: HashSet<TileId>,
    raster_missing: crate::raster_retention::AbsentTiles,
    /// `retries`' imagery twin.
    raster_retries: crate::fetch_policy::RetryTable,
    /// Built on the first frame that actually draws imagery and kept, like
    /// `star_pipeline` and `atmosphere_pipeline`: one shader module and one
    /// pipeline object, which a page with no `?imagery=` template never
    /// pays for.
    raster_pipeline: Option<RasterPipeline>,
    /// One square per granularity, shared by every imagery tile at that
    /// zoom -- `render::raster_mesh`'s whole purpose, the same cache
    /// `GpuState::raster_squares` is natively.
    raster_squares: HashMap<RasterMeshKey, RasterBuffers>,

    /// The text renderer, once its font has landed. See [`Labels`].
    labels: Option<Labels>,
    /// The per-frame allowance of new glyph rasters `prepare` may do -- see
    /// `render::text_glyph_budget`'s own doc comment. `map_view.rs`'s
    /// `GpuState` and `ios/src/app.rs`'s `Renderer` carry the identical
    /// field, driven the identical way.
    text_glyph_budget: crate::render::text_glyph_budget::TextGlyphBudget<glyphon::cosmic_text::CacheKey>,
    /// Fade-in state for labels, icons and line-label glyphs -- see
    /// `render::label_fade`'s own doc comment. Clocked off `now_ms() /
    /// 1000.0` rather than an `Instant::now()` origin the way the
    /// native/iOS shells are: `std::time::Instant` panics on
    /// `wasm32-unknown-unknown`, and `performance.now()` is already a
    /// monotonic seconds-capable clock with no epoch to subtract off.
    label_fade: crate::render::label_fade::LabelFade,
    /// Caps how often the fade alone sets `needs_redraw` -- see
    /// `render::label_fade::FadeRedrawScheduler`'s own doc comment.
    fade_redraw_gate: crate::render::label_fade::FadeRedrawScheduler,
    /// When a fading frame's redraw request was turned down by
    /// `fade_redraw_gate` (a display ticking faster than its cap), the
    /// `now_ms() / 1000.0` at which `start_frame_loop` sets `needs_redraw`
    /// instead. `requestAnimationFrame` re-arms every tick, but the gate is
    /// only consulted inside `draw`, and `draw` only runs on `needs_redraw`:
    /// without this deadline a turned-down request left every fading label
    /// frozen part-way to opaque on a still map (place names ghosted under
    /// data layers). The iOS/native shells keep the same deadline.
    fade_wake_at_s: Option<f64>,

    /// Turn-by-turn navigation; everything it decides is `crate::nav`'s,
    /// reached only through `nav::host`
    /// (`tests/navigation_is_computed_in_nav.rs`).
    nav: crate::nav::host::NavHost,
    /// The route line's mesh while a trip is active. `None` -- never
    /// created -- on a page without `?nav=`.
    nav_route: Option<crate::render::nav_layer::RouteMeshGpu>,
    /// Caller overlay sources -- GeoJSON polygons on the tiled compact path,
    /// clustered points as instanced circles -- driven INLINE by the
    /// engine's `overlay::host::ShellOverlays` under
    /// [`OVERLAY_BUDGET_MS_PER_FRAME`] (the spec's "Web threading"). Filled
    /// by `?overlay=`, `?points=` and [`map_add_geojson`].
    overlays: crate::overlay::host::ShellOverlays,
    /// `?census=<pack>`: the block groups' rows, county names and load
    /// progress (`census_page`). `None` on any other page.
    census: Option<census_page::CensusState>,
    /// Population dots (Task 16), from `?grid=` (a PMTiles archive, by
    /// `Range`) or `?clickhouse=` (the server's read-only proxy): the
    /// engine's `DotLayers`, planned every frame and fed by
    /// [`drive_dot_jobs`], and their GPU half. Empty on a page with neither.
    dots: crate::overlay::host::DotLayers,
    dots_gpu: crate::overlay::host::DotLayersGpu,
    /// ms since navigation when the dot source was asked for, and when the
    /// first dot was drawn: MG-24's "first dots" is the second.
    dots_asked_ms: Option<f64>,
    dots_first_ms: Option<f64>,
    /// The chrome pass, created on the first frame that has chrome.
    nav_chrome: Option<crate::render::nav_layer::NavChromeGpu>,
    /// Whether [`set_brand_card_hidden`] last hid the brand card.
    nav_brand_hidden: bool,
    /// Autoplay policy: `true` once the page has seen its first
    /// `pointerdown` or `keydown` (the Simulate tap counts, since it lands
    /// on the canvas as a `pointerdown`). Until then an utterance is
    /// dropped, not queued -- see `handle_voice_command`.
    voice_gesture_seen: bool,
    /// `speechSynthesis` was missing, logged once so a browser without it
    /// does not spam the console every frame.
    voice_no_synth_logged: bool,
    /// The voice `pick_web_voice` chose, once the browser has listed any.
    voice_choice: Option<web_sys::SpeechSynthesisVoice>,
    /// The name last logged as `[voice] using:`, so it is logged once.
    voice_logged: Option<String>,

    /// The icon renderer, once its sprite sheet has landed. See [`Icons`].
    icons: Option<Icons>,

    /// Which retention behaviour is active. See [`Mode`].
    mode: Mode,
    /// Set by `map_freeze` -- the frame loop does nothing at all while it is
    /// true. Instrumentation, so a capture can name its own frame.
    frozen: bool,
    /// `?trace=1`: one `console.log` line per rAF tick describing what the
    /// frame drew, by bake bucket -- the instrument the zoom-crossing
    /// twitch was measured with. `None` when off; otherwise last frame's
    /// label texts, so the line can report how many appeared and vanished.
    trace: Option<HashSet<String>>,
    trace_frame: u64,
    metrics: Metrics,
    /// Last frame's retained set, which is what eviction is allowed to spare.
    /// One frame of lag is harmless -- the set only ever grows by a tile that
    /// was just baked, and a tile that was just baked is not an eviction
    /// candidate.
    retained: HashSet<TileId>,

    drag_from: Option<(f32, f32)>,
    /// (time, position) samples from the drag in progress, oldest first,
    /// trimmed to `INERTIA_BUFFER_CUTOFF_MS` on every recorded move.
    drag_samples: std::collections::VecDeque<DragSample>,
    /// The post-release glide in flight, if any. See [`PanInertia`].
    pan_inertia: Option<PanInertia>,
    /// Every pointer (mouse, touch or pen) currently down on the canvas, by
    /// `PointerEvent.pointerId`, holding its last known CSS-pixel position.
    ///
    /// This is the single source of truth `install_input_handlers` reads to
    /// tell a one-finger drag from a two-finger pinch, and re-baselining a
    /// gesture when the pointer count changes falls out of it for free: a
    /// pan reads its one entry, a pinch reads its two, and the map is kept
    /// current on every `pointermove` regardless of which mode is active, so
    /// whichever mode a pointer count change switches TO always starts from
    /// each finger's true current position rather than a stale one.
    active_pointers: HashMap<i32, (f32, f32)>,
    /// The mouse pointer driving a right-drag / Ctrl+drag orbit, and where
    /// it was last seen. MapLibre's `DragRotateHandler` + `DragPitchHandler`.
    orbit: Option<(i32, (f32, f32))>,
    /// Where and when the orbit (right) button went down: a still, short
    /// press is a destination click (`nav::host::is_destination_click`).
    orbit_down: Option<((f32, f32), f64)>,
    /// The in-progress Shift+wheel tilt/rotate: its axis lock and the
    /// event time it last moved, so a pause starts a fresh axis choice.
    /// Native's `MapView::shift_scroll`, through the same
    /// `camera_control::shift_scroll_intent`.
    shift_scroll: Option<(crate::camera_control::ScrollAxisLock, f64)>,
    /// The trackpad pinch in progress: Chrome/Firefox ctrl-wheel or
    /// Safari's `gesture*` events. See `crate::trackpad_pinch`.
    browser_pinch: crate::trackpad_pinch::BrowserPinch,
    /// Route requests `NavHost` asked for, fetched by the frame loop (which
    /// holds the `Rc` a fetch needs to answer into).
    nav_outbox: Vec<crate::nav::host::RouteRequest>,
    /// The two-pointer gesture in progress, if two touches are down.
    pinch: Option<TwoFingerGesture>,
    /// Bearing deltas of the gesture in progress, for rotation inertia.
    rotate_samples: RotateSamples,
    /// `now_ms` of the last `advance`, for the eases' `dt`.
    last_advance_ms: Option<f64>,
    /// Set on a touch `pointerdown` that is the first (and so far only)
    /// finger down -- `(pointer_id, down_time_ms, down_x, down_y)` -- and
    /// consumed on that same pointer's `pointerup` to decide whether the
    /// touch was short and stationary enough to count as a tap at all.
    /// Cleared the moment a second finger lands (no tap is in progress once
    /// a gesture is no longer single-finger) and on `pointercancel`.
    touch_tap_start: Option<(i32, f64, f32, f32)>,
    /// The most recently recognised tap -- `(time_ms, x, y)` -- consumed (and
    /// turned into a zoom-in) by a second tap landing within
    /// [`DOUBLE_TAP_MS`]/[`DOUBLE_TAP_PX`] of it.
    last_tap: Option<(f64, f32, f32)>,
    /// Set by every resize signal, consumed by the frame loop. THIS is the
    /// coalescing: a drag of the window edge fires hundreds of events, and
    /// reconfiguring a surface and reallocating a depth texture per event
    /// thrashes badly -- but the flag only records THAT the size changed, and
    /// `apply_resize` reads the live CSS box when it runs, so the final size
    /// is always the one applied. A queue of sizes could go stale; a flag
    /// cannot.
    resize_pending: bool,
    needs_redraw: bool,
    /// [`map_set_labels_on_data`]: the paint every basemap label draws
    /// with while the page has data fills under the labels.
    label_paint: Option<crate::style::LabelPaint>,
    /// Which labels sit over the page's polygons, memoised per label.
    label_cover: crate::overlay::host::LabelCover,
    /// Whether a [`start_hidden_pump`] chain is currently scheduled. The
    /// chain stops itself the moment the document becomes visible, and the
    /// `visibilitychange` handler starts a new one when it goes hidden again,
    /// so without this a tab toggled twice would carry two chains.
    pump_running: bool,
    first_draw_logged: bool,
    /// `requestAnimationFrame` timestamps from the last second, so the
    /// readout can report a frame RATE rather than a frame count.
    ///
    /// A rolling one-second window rather than a smoothed inter-frame delta:
    /// the number that matters here is "did panning hold 60", and a window
    /// answers that directly while an EMA of deltas hides exactly the
    /// single long frame a bake overrun causes. Read back by
    /// [`map_fps`] so a scripted pan can assert on it.
    frame_stamps: std::collections::VecDeque<f64>,
    /// The last [`MAX_FRAME_SAMPLES`] drawn frames, oldest first. Drained by
    /// [`map_frame_samples_json`]. See [`FrameSample`].
    frame_samples: std::collections::VecDeque<FrameSample>,
    /// Samples dropped because the ring was full since it was last drained.
    /// Reported, so a run that overflowed cannot be mistaken for one that
    /// did not.
    frame_samples_dropped: u32,
    /// The rAF delta of the frame in progress, handed from
    /// `record_frame_stamp` (which sees it) to `draw` (which records it).
    last_interval_ms: f32,
    /// Whether the "full cover" line has been logged. A separate flag from
    /// `first_draw_logged` because the two mark genuinely different events:
    /// the first triangle is ONE tile of sixteen drawing, which is the
    /// earliest moment the page stops looking broken, and the full cover is
    /// the moment it stops looking incomplete. Reporting only the first was
    /// flattering by roughly a factor of ten.
    full_cover_logged: bool,
    started_at: f64,
    /// See [`Startup`]. Carried on the app rather than dropped at the end of
    /// `run` because half the interesting stages -- the first fetch, the
    /// first decode, the first bake -- happen after `run` has returned.
    startup: Startup,
}

impl MapApp {
    /// `[REUSED]` -- `map_view::display_zoom`, which is one line there too.
    /// Deliberately NOT clamped to the source ceiling: it selects layers by
    /// `minzoom`/`maxzoom` and resolves every zoom-dependent paint value, and
    /// Mapbox/MapLibre resolve style against the MAP's zoom, never the tile's.
    /// Frames presented in the last second. `0.0` until a second has
    /// elapsed, which is honest -- a rate cannot be reported from one
    /// sample -- and is why the readout prints a dash for it.
    fn fps(&self) -> f64 {
        match (self.frame_stamps.front(), self.frame_stamps.back()) {
            (Some(&first), Some(&last)) if last > first => {
                (self.frame_stamps.len() - 1) as f64 * 1000.0 / (last - first)
            }
            _ => 0.0,
        }
    }

    /// Records one `requestAnimationFrame` tick and drops anything older
    /// than the window.
    fn record_frame_stamp(&mut self, now: f64) {
        // The interval is taken HERE, at the top of the rAF callback, and
        // carried to `draw`: `draw` runs after a resize, a tile request and
        // a bake, so reading the clock there would measure the callback's
        // own progress rather than the presented interval.
        self.last_interval_ms = self.frame_stamps.back().map_or(0.0, |&prev| (now - prev) as f32);
        self.frame_stamps.push_back(now);
        while self.frame_stamps.front().is_some_and(|&t| now - t > 1000.0) {
            self.frame_stamps.pop_front();
        }
    }

    fn display_zoom(&self) -> u8 {
        self.camera_control.camera().zoom.floor().clamp(0.0, u8::MAX as f64) as u8
    }

    fn tile_zoom(&self) -> u8 {
        self.display_zoom().min(source_max_zoom())
    }

    /// The globe's cover for THIS call, or `GlobeCover::FLAT` above its
    /// band. Built fresh here rather than threaded in from `draw()`'s own
    /// `globe_frame` (line ~2878): `cover()`/`prefetch_cover()` are read from
    /// `plan_frame()`, a separate call outside `draw()`'s borrow, so there is
    /// no frame value to share -- the same shape as native's
    /// `MapView::cover_progress`, which builds its own for the identical
    /// reason (see that method's doc comment in `map_view.rs`).
    fn globe_cover(&self) -> crate::render::globe::GlobeCover {
        crate::render::globe::GlobeCover::new(&crate::render::globe::globe_frame(self.camera_control.camera()))
    }

    /// `render::cover_plan::flat_cover`, the one pitch-aware,
    /// frustum-trimmed planar cover every shell shares, routed through
    /// `render::cover_plan::tile_cover` so the GLOBE gets first say. Records
    /// the flat cover's `reach` in `last_cover_reach` for the horizon fade.
    ///
    /// This used to call `geo::visible_tiles` directly and unconditionally,
    /// which is exactly the defect `cover_plan` exists to close: a flat
    /// Mercator bbox cannot cover a sphere showing a hemisphere, so half the
    /// globe stayed the unrendered Liberty background colour.
    fn cover(&self) -> Vec<TileId> {
        let tile_zoom = self.tile_zoom();
        let camera = self.camera_control.camera();
        let mut reach = None;
        // The building floor (the far-building fix), shared: while buildings
        // draw, the near ground stays on tiles that carry building data.
        let request = crate::render::frame_plan::building_cover_request(&self.style.layers, camera.zoom, tile_zoom);
        let mut building_reach = None;
        let tiles = crate::render::cover_plan::tile_cover(tile_zoom, &self.globe_cover(), || {
            let cover = crate::render::cover_plan::flat_cover(camera, request);
            reach = cover.reach;
            building_reach = cover.building_reach;
            cover.tiles
        });
        self.last_cover_reach.set(reach);
        self.last_building_reach.set(building_reach);
        tiles
    }

    /// The coarse prefetch cover: the same viewport, covered at
    /// `tile_zoom - prefetch_zoom_delta`.
    ///
    /// `tile_pyramid.cpp:127-141`. `flat_cover` works in world units, so
    /// the same frustum asked at a zoom four levels down selects tiles 16x
    /// wider on each side, which is why the coarse cover is a handful of
    /// tiles at a flat viewport while the visible cover scales with area.
    /// That asymmetry is the whole reason this is cheap.
    fn prefetch_cover(&self) -> Vec<TileId> {
        let delta = self.mode.prefetch_delta();
        if delta == 0 {
            return Vec::new();
        }
        let tile_zoom = self.tile_zoom();
        let coarse = tile_zoom.saturating_sub(delta);
        if coarse >= tile_zoom {
            return Vec::new();
        }
        // Same fix as `cover()`, same reason: the coarse cover shares
        // `tile_zoom`'s flat formula, so it shares its defect above the
        // globe's band too. Routed through `cover_plan::tile_cover` so the
        // sphere gets first say here as well -- an ancestor stand-in fetched
        // from the flat function's hemisphere would be exactly as wrong as
        // the visible cover was.
        // Its distance LOD is shifted down with it
        // (`cover_plan::CoverZoom::coarser`): centred on the camera's own
        // zoom, the coarse cover would stay at `coarse` 2^delta times
        // farther out than the display cover keeps its zoom -- the
        // 512-tile prefetch backlog at 60 degrees.
        let camera = self.camera_control.camera();
        let request = crate::render::cover_plan::CoverZoom::coarser(camera, tile_zoom, coarse);
        crate::render::cover_plan::tile_cover(coarse, &self.globe_cover(), || {
            crate::render::cover_plan::flat_cover(camera, request).tiles
        })
    }

    /// THE IMAGERY DECISION for this frame -- whether it draws, at what
    /// opacity, and at what cover zoom -- through the one function every
    /// shell shares, `render::raster_plan::raster_plan`. Never re-derived
    /// here: see `tests/raster_plan_parity.rs`, which fails the build the
    /// moment this file (or any other shell) walks `StyleLayer::Raster`
    /// or computes a cover zoom on its own instead of reading this.
    fn raster_plan(&self) -> crate::render::raster_plan::RasterPlan {
        let config = &self.raster_config;
        crate::render::raster_plan::raster_plan(
            !config.template.is_empty(),
            self.display_zoom(),
            self.camera_control.camera().zoom,
            config.tile_size,
            self.dpr,
            config.max_zoom,
            &self.style.layers,
        )
    }

    /// The imagery and absence caches' occupancy (`map_memory_json`).
    fn raster_counts(&self) -> RasterCounts {
        RasterCounts {
            decoded_tiles: self.raster_decoded.len(),
            decoded_bytes: self.raster_decoded.held_bytes(),
            gpu_tiles: self.raster_tiles.len(),
            absent_tiles: self.missing.len() + self.raster_missing.len(),
        }
    }

    /// The imagery cover this frame wants, empty when [`Self::raster_plan`]
    /// says nothing draws, and where it was cut (the flat cover's `reach`,
    /// for `cover_plan::frame_fog`; `None` when nothing draws or nothing
    /// was cut). Routed through `cover_plan::tile_cover` exactly as
    /// [`Self::cover`] is, so the globe gets first say over imagery too.
    fn raster_cover(&self) -> (Vec<TileId>, Option<f64>) {
        let plan = self.raster_plan();
        if plan.opacity.is_none() {
            return (Vec::new(), None);
        }
        let mut reach = None;
        let tiles = crate::render::cover_plan::tile_cover(plan.cover_zoom, &self.globe_cover(), || {
            let cover = crate::render::cover_plan::flat_cover(self.camera_control.camera(), plan.cover_request());
            reach = cover.reach;
            cover.tiles
        });
        (tiles, reach)
    }

    /// Decides, in ONE traversal, what is drawn, what is kept, and where the
    /// holes are.
    ///
    /// maplibre-native's shape: `retain` and `renderedTiles` are populated by
    /// two callbacks handed to the same algorithm
    /// (`tile_pyramid.cpp:158-228`), so the upload set and the draw set cannot
    /// drift apart. Deciding visibility first and hunting for stand-ins in a
    /// later pass is exactly how a stand-in ends up drawn but never uploaded.
    fn plan_frame(&self) -> FramePlan {
        let display_zoom = self.display_zoom();
        let cover = self.cover();
        let mut plan = FramePlan {
            cover: cover.clone(),
            cover_len: cover.len(),
            cover_zooms: crate::render::cover_plan::zoom_span(&cover),
            ..Default::default()
        };

        let has_mesh = |t: TileId| match self.gpu_tiles.get(&t) {
            Some(g) => self.mode.stale_meshes_cover() || g.baked_zoom == display_zoom,
            None => false,
        };
        let source_max_zoom = source_max_zoom();

        // Pass 1 -- the stand-ins, coarse-first so the ideal tiles below paint
        // over them. Native's `map_view.rs:6356`.
        let mut queued: HashSet<TileId> = HashSet::new();
        let mut stand_ins: Vec<TileId> = Vec::new();
        for &tile in &cover {
            plan.retained.insert(tile);
            if has_mesh(tile) {
                continue;
            }
            if !self.mode.stand_ins() {
                plan.holes += 1;
                self.classify_hole(tile, &mut plan);
                continue;
            }
            // Finer tiles first: their geometry needs no magnification, so
            // they will not visibly jump when the real tile lands.
            if let Some(descendants) = covering_descendants(tile, source_max_zoom, has_mesh) {
                plan.descendant_stand_ins += 1;
                for d in descendants {
                    plan.retained.insert(d);
                    if queued.insert(d) {
                        stand_ins.push(d);
                    }
                }
                continue;
            }
            // Then up. EVERY ancestor walked is retained, not just the one
            // that ends up drawn (`update_renderables.hpp:122,127`;
            // `source_cache.ts:786`) -- a partially-loaded intermediate level
            // is what makes the next zoom-out step already covered.
            let mut current = tile;
            let mut found = None;
            while let Some(parent) = tile_parent(current) {
                plan.retained.insert(parent);
                if has_mesh(parent) {
                    found = Some(parent);
                    break;
                }
                current = parent;
            }
            match found {
                Some(ancestor) => {
                    plan.ancestor_stand_ins += 1;
                    if queued.insert(ancestor) {
                        stand_ins.push(ancestor);
                    }
                }
                None => {
                    plan.holes += 1;
                    self.classify_hole(tile, &mut plan);
                }
            }
        }

        // Coarse-first within the stand-in set too: a z9 ancestor must not
        // paint over a z12 one that is standing in for a smaller area.
        stand_ins.sort_by_key(|t| t.z);
        plan.draws = stand_ins;

        // Pass 2 -- every cover tile that has something of its own, on top.
        for &tile in &cover {
            if let Some(g) = self.gpu_tiles.get(&tile) {
                if self.mode.stale_meshes_cover() || g.baked_zoom == display_zoom {
                    if g.baked_zoom == display_zoom {
                        plan.fresh += 1;
                    } else {
                        plan.stale += 1;
                    }
                    plan.draws.push(tile);
                }
            }
        }

        // The coarse cover is RETAINED and never drawn on its own account --
        // the no-op `renderTile` callback at `tile_pyramid.cpp:214`. It only
        // reaches the screen by being selected as an ancestor stand-in above,
        // which is the entire point of loading it early.
        for t in self.prefetch_cover() {
            plan.retained.insert(t);
        }
        plan
    }

    /// One `?trace=1` line: the frame's camera, how many tiles it drew at
    /// each bake bucket, the stand-ins, the label churn against the last
    /// drawn frame, and where the tick's milliseconds went.
    fn trace_line(&mut self, plan: &FramePlan, drew: bool, decode_ms: f64, bake_ms: f64, draw_ms: f64) {
        self.trace_frame += 1;
        let mut buckets: std::collections::BTreeMap<u8, u32> = Default::default();
        for t in &plan.draws {
            if let Some(g) = self.gpu_tiles.get(t) {
                *buckets.entry(g.baked_zoom).or_default() += 1;
            }
        }
        let buckets: Vec<String> = buckets.iter().map(|(z, n)| format!("\"{z}\":{n}")).collect();
        let (labels, added, removed) = match (self.labels.as_ref(), drew) {
            (Some(l), true) => {
                let now: HashSet<String> = l.texts.iter().cloned().collect();
                let prev = self.trace.as_ref().unwrap();
                let added = now.difference(prev).count();
                let removed = prev.difference(&now).count();
                let n = now.len();
                self.trace = Some(now);
                (n, added, removed)
            }
            _ => (self.trace.as_ref().map_or(0, |p| p.len()), 0, 0),
        };
        let cam = self.camera_control.camera();
        log(&format!(
            "TRACE {{\"f\":{},\"t\":{:.1},\"iv\":{:.1},\"z\":{:.4},\"pitch\":{:.1},\"dz\":{},\"drew\":{},\
             \"draws\":{},\"buckets\":{{{}}},\"fresh\":{},\"stale\":{},\"desc\":{},\"anc\":{},\"holes\":{},\
             \"labels\":{},\"l_add\":{},\"l_rm\":{},\"decode_ms\":{:.2},\"bake_ms\":{:.2},\"draw_ms\":{:.2}}}",
            self.trace_frame,
            now_ms(),
            self.last_interval_ms,
            cam.zoom,
            cam.pitch.to_degrees(),
            self.display_zoom(),
            drew,
            plan.draws.len(),
            buckets.join(","),
            plan.fresh,
            plan.stale,
            plan.descendant_stand_ins,
            plan.ancestor_stand_ins,
            plan.holes,
            labels,
            added,
            removed,
            decode_ms,
            bake_ms,
            draw_ms,
        ));
    }

    /// Which KIND of hole this is -- the distinction that made the native fix
    /// correct rather than lucky. "No ancestor loaded" cannot be fixed by any
    /// cache bound or retention policy; "ancestor loaded but not on the GPU"
    /// can.
    fn classify_hole(&self, tile: TileId, plan: &mut FramePlan) {
        // Checked first, and it is the cheapest cause to fix: the geometry is
        // already on the GPU and was discarded for being a zoom bucket behind.
        if self.gpu_tiles.contains_key(&tile) {
            plan.holes_stale_mesh += 1;
            return;
        }
        let mut current = tile;
        while let Some(parent) = tile_parent(current) {
            if self.decoded.contains_key(&parent) {
                plan.holes_ancestor_unbaked += 1;
                return;
            }
            current = parent;
        }
        plan.holes_never_loaded += 1;
    }

    /// One drag step through the controller, grabbed at `anchor` (the
    /// pointer's position before the move, CSS pixels). Bearing-aware for
    /// free: this shell no longer does its own screen-to-tile arithmetic.
    fn pan_by_pixels(&mut self, dx: f32, dy: f32, anchor: (f32, f32)) {
        self.camera_control.apply(CameraIntent::PanBy { dx_px: dx, dy_px: dy, anchor_px: anchor });
    }

    /// Zoom by `levels` about `(x, y)`: eased, like native, now that the
    /// frame loop advances the controller.
    fn zoom_about(&mut self, levels: f64, x: f32, y: f32, immediate: bool) {
        self.camera_control.apply(CameraIntent::ZoomAbout { levels, anchor_px: (x, y), immediate });
    }

    /// The two touch pointers in a STABLE order (by pointer id). A
    /// `HashMap`'s iteration order would swap the pair between events and
    /// read as a half-turn twist.
    fn pointer_pair(&self) -> Option<[(f32, f32); 2]> {
        let mut ids: Vec<i32> = self.active_pointers.keys().copied().collect();
        ids.sort_unstable();
        match ids.as_slice() {
            [a, b] => Some([self.active_pointers[a], self.active_pointers[b]]),
            _ => None,
        }
    }

    /// `[LIFTED]` -- `map_view::advance_pan_inertia`, verbatim modulo the
    /// clock. Returns whether the glide is still running.
    ///
    /// This drives `pan_by_pixels` directly rather than steering a target and
    /// chasing it, so the curve applied here is exactly the curve the user
    /// sees. It also runs BEFORE `request_missing_tiles` in the frame loop,
    /// for the same reason `apply_resize` does: a fling that moved the camera
    /// after the cover was computed would spend the whole glide asking for
    /// the tiles it wanted one frame ago, and would outrun the tile set
    /// exactly the way the round-3 blink did.
    fn advance_pan_inertia(&mut self, now_ms: f64) -> bool {
        let Some(inertia) = self.pan_inertia else { return false };
        let ((dx, dy), finished) = pan_inertia_step(&inertia, now_ms);
        let camera = self.camera_control.camera();
        let centre = (camera.viewport_width_px / 2.0, camera.viewport_height_px / 2.0);
        self.pan_by_pixels(dx as f32, dy as f32, centre);
        self.needs_redraw = true;
        if finished {
            self.pan_inertia = None;
            return false;
        }
        // Accumulate the amount ASKED for, not the f32 `pan_by_pixels`
        // received: the running total is what the final frame subtracts from
        // `offset_*` to land exactly, and it has to be in the same units the
        // curve is computed in.
        self.pan_inertia = Some(PanInertia {
            applied_x: inertia.applied_x + dx,
            applied_y: inertia.applied_y + dy,
            ..inertia
        });
        true
    }

    /// This frame's wanted set, most urgent first: the vector cover and the
    /// imagery cover (both `Visible`, interleaved by nearness within each),
    /// then the coarse prefetch cover. Every list nearest-the-eye first --
    /// the fetch order only; `cover()` is drawn in its own order.
    fn wanted_tiles(&self) -> crate::fetch_queue::Wanted<TileKey> {
        use crate::fetch_queue::FetchClass;
        let camera = self.camera_control.camera();
        let keys = |tiles: &[TileId], kind: TileKind| -> Vec<TileKey> {
            crate::render::cover_plan::nearest_first(tiles, camera).into_iter().map(|t| (t, kind)).collect()
        };
        let cover = keys(&self.cover(), TileKind::Vector);
        let raster = if self.raster_config.template.is_empty() {
            Vec::new()
        } else {
            keys(&self.raster_cover().0, TileKind::Raster)
        };
        let coarse = keys(&self.prefetch_cover(), TileKind::Vector);
        let mut wanted = crate::fetch_queue::Wanted::new();
        wanted.add(FetchClass::Visible, &cover);
        wanted.add(FetchClass::Visible, &raster);
        wanted.add(FetchClass::Coarse, &coarse);
        wanted
    }

    /// The request pass, vector and imagery together: retarget the pending
    /// queue to this frame's wanted set, queue what is missing, and start as
    /// many fetches as [`MAX_CONCURRENT_FETCHES`] allows.
    ///
    /// # Why there is a queue at all
    ///
    /// This used to be one `fetch` per missing tile, straight onto the task
    /// queue, never aborted: the browser's own per-host connection limit did
    /// the rationing by accident, in FIFO order, and a request for a tile the
    /// camera had left stayed ahead of the tiles it now wanted. On HTTP/2 there
    /// was not even that limit -- every tile of a 512-tile tilted cover went
    /// out at once and shared the bandwidth with the foreground. Now
    /// requests wait in `fetch_queue` ([`crate::fetch_queue::FetchQueue`]),
    /// at most [`MAX_CONCURRENT_FETCHES`] are on the wire, the most urgent
    /// goes next, and a pending one the camera left is dropped before it is
    /// ever sent -- MapLibre's `_removeTile` -> `abortTile` rule. Requests
    /// already sent are left to finish rather than aborted: at most eight of
    /// them, and [`Self::decode_arrived`] discards their bytes if stale.
    fn request_missing_tiles(app: &Rc<RefCell<MapApp>>) {
        let now = now_ms();
        {
            let mut guard = app.borrow_mut();
            let a = &mut *guard;
            let wanted = a.wanted_tiles();
            for (tile, kind) in a.fetch_queue.retarget(&wanted) {
                match kind {
                    TileKind::Vector => a.inflight.remove(&tile),
                    TileKind::Raster => a.raster_inflight.remove(&tile),
                };
            }
            let template = tile_template();
            for ((tile, kind), priority) in wanted.in_priority_order() {
                let is_prefetch = priority.class != crate::fetch_queue::FetchClass::Visible;
                let url = match kind {
                    TileKind::Vector => {
                        if a.decoded.contains_key(&tile)
                            || a.inflight.contains(&tile)
                            || a.missing.contains(&tile)
                            || !a.retries.may_request(tile, now)
                        {
                            continue;
                        }
                        a.inflight.insert(tile);
                        a.retries.on_requested(tile);
                        if is_prefetch {
                            a.metrics.requests_prefetch += 1;
                        } else {
                            a.metrics.requests_cover += 1;
                        }
                        fill_template(&template, tile)
                    }
                    TileKind::Raster => {
                        if a.raster_decoded.contains(&tile)
                            || a.raster_inflight.contains(&tile)
                            || a.raster_missing.contains(&tile)
                            || !a.raster_retries.may_request(tile, now)
                        {
                            continue;
                        }
                        a.raster_inflight.insert(tile);
                        a.raster_retries.on_requested(tile);
                        fill_template(&a.raster_config.template, tile)
                    }
                };
                a.fetch_queue.push((tile, kind), priority, (is_prefetch, url));
            }
            a.wanted = wanted;
        }
        MapApp::start_fetches(app);
    }

    /// Sends queued requests, most urgent first, until
    /// [`MAX_CONCURRENT_FETCHES`] are on the wire. Called by the request pass
    /// and by every completion, so a finished fetch is replaced at once
    /// rather than on the next frame.
    fn start_fetches(app: &Rc<RefCell<MapApp>>) {
        loop {
            let next = {
                let mut a = app.borrow_mut();
                if a.fetch_queue.running_len() >= MAX_CONCURRENT_FETCHES {
                    return;
                }
                a.fetch_queue.pop()
            };
            let Some(((tile, kind), (is_prefetch, url))) = next else { return };
            if kind == TileKind::Vector {
                app.borrow_mut().startup.mark_once("first fetch sent");
            }
            let app = app.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let result = fetch_tile(&url).await;
                {
                    let mut a = app.borrow_mut();
                    a.fetch_queue.finish(&(tile, kind));
                    match result {
                        // Buffered, not decoded: `decode_arrived` decodes
                        // under the frame's budget, and discards it undecoded
                        // if the camera has left the tile by then.
                        TileFetch::Body(bytes) => {
                            a.arrived.push(((tile, kind), (is_prefetch, bytes)));
                            a.needs_redraw = true;
                        }
                        // 404: the source genuinely has nothing here. Remember it.
                        TileFetch::Absent => match kind {
                            TileKind::Vector => {
                                a.retries.on_settled(tile);
                                a.missing.insert(tile);
                                a.inflight.remove(&tile);
                            }
                            TileKind::Raster => {
                                a.raster_retries.on_settled(tile);
                                a.raster_missing.insert(tile);
                                a.raster_inflight.remove(&tile);
                            }
                        },
                        TileFetch::Failed { message, retry_after_ms } => {
                            let now = now_ms();
                            let (delay, what) = match kind {
                                TileKind::Vector => {
                                    a.inflight.remove(&tile);
                                    (a.retries.on_failure(tile, now, retry_after_ms), "fetch")
                                }
                                TileKind::Raster => {
                                    a.raster_inflight.remove(&tile);
                                    (a.raster_retries.on_failure(tile, now, retry_after_ms), "imagery fetch")
                                }
                            };
                            log(&format!("{what} {tile:?} failed, retry in {:.1} s: {message}", delay / 1000.0));
                        }
                    }
                }
                MapApp::start_fetches(&app);
            });
        }
    }

    /// Decodes buffered tile bodies, most urgent first, until `budget_ms` is
    /// spent; the rest wait for the next frame (keeping their in-flight
    /// marks). A body for a tile in neither this frame's wanted set nor the
    /// retained set is stale and is discarded undecoded, its in-flight mark
    /// cleared. Returns whether anything was decoded.
    ///
    /// Decoding used to run inside each fetch's completion, unbudgeted, for
    /// whatever tile it was: after a tilted pan, a burst of stale arrivals
    /// could decode dozens of tiles between two frames.
    fn decode_arrived(&mut self, budget_ms: f64) -> bool {
        if self.arrived.is_empty() {
            return false;
        }
        let retained = &self.retained;
        let triage = self
            .wanted
            .triage(std::mem::take(&mut self.arrived), |(t, kind)| *kind == TileKind::Vector && retained.contains(t));
        for ((tile, kind), _) in triage.stale {
            match kind {
                TileKind::Vector => self.inflight.remove(&tile),
                TileKind::Raster => self.raster_inflight.remove(&tile),
            };
            self.stale_results_dropped += 1;
        }
        let started = now_ms();
        let mut done = 0;
        for ((tile, kind), (is_prefetch, bytes)) in triage.in_order {
            if !crate::fetch_queue::may_start_another(done, now_ms() - started, budget_ms) {
                self.arrived.push(((tile, kind), (is_prefetch, bytes)));
                continue;
            }
            done += 1;
            match kind {
                TileKind::Vector => {
                    self.inflight.remove(&tile);
                    if is_prefetch {
                        self.metrics.bytes_prefetch += bytes.len() as u64;
                    } else {
                        self.metrics.bytes_cover += bytes.len() as u64;
                    }
                    match crate::mvt::decode(bytes) {
                        Ok(decoded) => {
                            self.startup.mark_once("first tile decoded");
                            self.retries.on_settled(tile);
                            self.decoded.insert(tile, Rc::new(decoded));
                        }
                        // Usually a body cut off mid-transfer: retried.
                        Err(err) => {
                            let delay = self.retries.on_failure(tile, now_ms(), None);
                            log(&format!("decode {tile:?} failed, retry in {:.1} s: {err:#}", delay / 1000.0));
                        }
                    }
                }
                TileKind::Raster => {
                    self.raster_inflight.remove(&tile);
                    match crate::raster::decode_tile_at(&bytes, self.raster_config.tile_size, tile) {
                        Ok(image) => {
                            self.raster_retries.on_settled(tile);
                            self.raster_decoded.insert(tile, Rc::new(image));
                        }
                        Err(err) => {
                            // Never logs the URL or the bytes -- a tile URL
                            // can carry an access token, and `decode_tile`'s
                            // own error already omits both. See `src/raster.rs`.
                            // Retried, like the vector decode: usually a
                            // truncated body.
                            let delay = self.raster_retries.on_failure(tile, now_ms(), None);
                            log(&format!("imagery {tile:?} decode failed, retry in {:.1} s: {err:#}", delay / 1000.0));
                        }
                    }
                }
            }
        }
        done > 0
    }

    /// Bakes tiles in strict priority order until `budget_ms` of tessellation
    /// has been spent. Returns whether anything was baked (i.e. whether more
    /// frames are needed to finish the set).
    ///
    /// # The priority order IS the browser's `TileNecessity`
    ///
    /// maplibre-native's `TileNecessity::Optional`
    /// (`update_renderables.hpp:117-128`) exists so that a stand-in request
    /// never competes with the tile the user is actually waiting for. Our
    /// scarce resource is not bandwidth, it is the 7-26 ms of `match_tile` +
    /// `tessellate_plan` that `BAKE_BUDGET_MS_PER_FRAME` rations on the main
    /// thread -- there is no worker to hand it to. So the same idea applies to
    /// the bake budget instead:
    ///
    /// 1. cover tiles with NO mesh at all -- a hole on screen right now;
    /// 2. cover tiles whose mesh was baked at another zoom bucket -- visible,
    ///    but stylistically a bucket behind;
    /// 3. the coarse prefetch cover -- `Optional` in every sense. It gets the
    ///    leftovers, which in practice means the frames after the camera
    ///    settles, which is exactly the quiet moment the prefetch is for.
    ///
    /// Without (3) coming last, the coarse cover would steal bake slots from
    /// the tiles under the cursor and make the symptom worse -- which is what
    /// naive on-demand ancestor requests measured on native.
    /// `budget_ms` is normally [`BAKE_BUDGET_MS_PER_FRAME`]. The hidden-tab
    /// pump passes `f64::INFINITY`: the ration exists to keep a tessellation
    /// from overrunning a 16.7 ms frame, and a hidden tab has no frame to
    /// protect.
    fn bake_some(&mut self, budget_ms: f64) -> bool {
        let display_zoom = self.display_zoom();
        let cover = self.cover();
        let coarse = self.prefetch_cover();
        let camera = self.camera_control.camera();

        // Each class nearest-the-eye first (`render::cover_plan::bake_order`),
        // not `cover()`'s own row-major `(x, y)` draw order: at 60 degrees of
        // pitch that draw order put the far rows first, so the ground under
        // the camera was the LAST thing baked. `cover()`'s draw list itself
        // is untouched -- `bake_order` only reorders which of its tiles get
        // a bake slot this frame.
        let todo: Vec<(TileId, bool)> = crate::render::cover_plan::bake_order(
            &cover,
            &coarse,
            |t| self.gpu_tiles.get(&t).map(|g| g.baked_zoom),
            |t| self.decoded.contains_key(&t),
            display_zoom,
            camera,
        );
        if todo.is_empty() {
            // Still evict: stale tiles used to pile into `decoded` on frames
            // with nothing to bake, and the cap below never ran for them.
            self.evict_discard_pile(cover);
            return false;
        }
        self.startup.mark_once("first bake starts");
        let frame_started = now_ms();
        for (index, (tile, is_prefetch)) in todo.into_iter().enumerate() {
            // Checked BEFORE the tile rather than after it, and never for the
            // first one. A tile whose own cost exceeds the whole budget would
            // otherwise never be baked at all, and the priority order puts
            // exactly the tile the user is waiting for at index 0.
            if index > 0 && now_ms() - frame_started >= budget_ms {
                break;
            }
            if is_prefetch {
                self.metrics.bakes_prefetch += 1;
            } else {
                self.metrics.bakes_cover += 1;
            }
            let decoded = self.decoded[&tile].clone();
            // The engine, in two calls. Exactly what `MapView::bake_tile` runs
            // on a tokio worker -- here it runs on the frame, because a
            // browser main thread has no worker to hand it to.
            //
            // Timed, because these two lines ARE the frame budget this
            // function rations, and the ration was chosen against an estimate
            // rather than a measurement. `performance.now()` is coarsened to
            // 100 us by cross-origin isolation rules, which is well under the
            // per-tile cost and irrelevant to a sum over twenty of them.
            let bake_started = now_ms();
            let plan = crate::style::match_tile(&decoded, &self.style, display_zoom);
            // `Subdivision::for_tile`, exactly as `MapView::bake_tile` does
            // (`map_view.rs:1772` and its five siblings). Bake-time, so it
            // has to be decided here even on a frame whose camera is above
            // the globe band: the mesh outlives the zoom it was cut at, and
            // an un-subdivided one cannot be repaired by the vertex shader.
            // `Subdivision::for_tile` is also what closes the poles, which
            // is geometry no flat tile carries at all.
            let mesh = crate::render::tessellate_plan(
                &plan,
                4096,
                crate::render::mesh::COORD_SCALE,
                crate::render::subdivide::Subdivision::for_tile(tile),
            );
            // The symbol half of the same bake, and charged to the same
            // budget: `match_tile_symbols` walks the same layers over the
            // same features, and a `symbol-placement: line` layer walks the
            // geometry again to space its anchors. Leaving it out of the
            // timing would make the budget this function rations describe a
            // frame it no longer measures.
            //
            // `tile_units_per_px` converts `symbol-spacing` from pixels into
            // the tile's own 0..4096 space and so depends on how far the
            // bake zoom is above the tile's own -- `MapView::symbol_plan_for`
            // computes it exactly this way.
            let symbols = Rc::new(crate::style::match_tile_symbols(
                &decoded,
                &self.style,
                display_zoom,
                crate::symbol_line::tile_units_per_px(
                    crate::symbol_line::symbol_spacing_zoom(display_zoom, self.tile_zoom(), tile.z),
                    tile.z,
                ),
            ));
            // The buildings, from the same plan and charged to the same
            // budget: `MapView::bake_tile`'s `tessellate_extrusions`, and
            // the upload beside the flat buffers below.
            let extrusion_mesh = crate::render::extrusion::tessellate_extrusions(&plan);
            let bake_cost = now_ms() - bake_started;
            self.metrics.bake_ms += bake_cost;
            self.metrics.bake_ms_max = self.metrics.bake_ms_max.max(bake_cost);

            let extrusion = self.pipeline.create_extrusion_buffers(&self.ctx, &extrusion_mesh);
            match self.gpu_tiles.remove(&tile) {
                // Re-bake at a new zoom: the uniform and bind group are still
                // good, and `install_mesh` grows the buffers only if it must.
                Some(existing) => {
                    let buffers =
                        self.pipeline.install_basemap_mesh(&self.ctx, Some(existing.buffers), &mesh);
                    self.gpu_tiles.insert(
                        tile,
                        GpuTile {
                            baked_zoom: display_zoom,
                            buffers,
                            model_buffer: existing.model_buffer,
                            model_bind_group: existing.model_bind_group,
                            symbols,
                            extrusion,
                        },
                    );
                }
                None => {
                    let (model_buffer, model_bind_group) = self.pipeline.create_model_bind_group(
                        &self.ctx,
                        glam::Mat4::IDENTITY.to_cols_array_2d(),
                        0.0,
                        1.0,
                        // A placeholder, like the IDENTITY matrix above it:
                        // `draw` rewrites this whole uniform through
                        // `write_model_uniform` before the tile is ever
                        // drawn. `FLAT` rather than a real upload because
                        // there is no frame here to build one from -- this
                        // runs in the bake loop, not the draw loop. Same
                        // choice as `map_view.rs:15238`.
                        crate::render::GlobeParams::FLAT,
                    );
                    let buffers = self.pipeline.create_basemap_tile_buffers(&self.ctx, &mesh);
                    self.gpu_tiles.insert(
                        tile,
                        GpuTile { baked_zoom: display_zoom, buffers, model_buffer, model_bind_group, symbols, extrusion },
                    );
                }
            }
        }

        self.metrics.bake_ms_frame_max =
            self.metrics.bake_ms_frame_max.max(now_ms() - frame_started);
        self.evict_discard_pile(cover);
        true
    }

    /// Eviction for [`Self::bake_some`], run on every call whether or not
    /// anything was baked.
    fn evict_discard_pile(&mut self, cover: Vec<TileId>) {

        // Eviction, bounded by the DISCARD PILE and never reaching the
        // retained set.
        //
        // The bound used to be `keep = cover`, which is precisely the shape
        // all three reference engines say is wrong: a stand-in is by
        // definition not in the cover, so a cover-keyed retain is guaranteed
        // to throw away exactly the tile that is standing in for a hole
        // (reference §5A, §2). `self.retained` is last frame's cover PLUS
        // every stand-in chosen, every ancestor walked toward one, and the
        // coarse cover. Under `Mode::Baseline`/`StaleSelf` that set is just
        // the cover, so the old behaviour is preserved exactly.
        let mut keep: HashSet<TileId> = cover.into_iter().collect();
        keep.extend(self.retained.iter().copied());
        if self.gpu_tiles.len() > MAX_GPU_TILES {
            self.gpu_tiles.retain(|t, _| keep.contains(t));
        }
        // AND THE DECODED TILES, which nothing bounded until now.
        //
        // `MAX_GPU_TILES` bounds the meshes. `self.decoded` -- the `mvt`
        // output every mesh is baked FROM -- was bounded by nothing at all,
        // and it is the larger of the two: a Tokyo z14 tile carries ~2,100
        // road and ~2,800 building features, and holding one costs about a
        // megabyte of wasm linear memory.
        //
        // Measured before this bound existed, over a scripted tour of twelve
        // z14 stops across Kanto followed by sixty drags: `decoded` reached
        // 390 tiles and never fell, and the wasm heap climbed 263 -> 468 MB
        // and stayed there. It is not a leak in the usual sense -- every
        // entry is reachable and would be reused if the camera came back --
        // but wasm linear memory only ever GROWS (there is no counterpart to
        // `memory.grow`), so an unbounded cache in a browser is a one-way
        // ratchet on the tab's footprint in a way it is not natively.
        //
        // The same `keep` set as the meshes, and a bound stated in the same
        // terms, so the two caches cannot disagree about what is retained: a
        // decoded tile whose mesh is still resident is never dropped, or the
        // next re-bake at a new zoom bucket would have nothing to bake from
        // and would have to refetch a tile that is already on screen.
        if self.decoded.len() > MAX_DECODED_TILES {
            self.decoded.retain(|t, _| keep.contains(t) || self.gpu_tiles.contains_key(t));
        }
    }

    /// Re-sizes the three things that must move together, and the reason
    /// this function exists at all.
    ///
    /// A map is a WINDOW ONTO A FIXED-SCALE WORLD, not an image scaled to
    /// fit: a wider window must reveal more ground at the same
    /// metres-per-pixel, never stretch the ground it already had. Getting
    /// that right needs all three of
    ///
    ///   1. the canvas backing store -- `css_size * devicePixelRatio`;
    ///   2. the wgpu surface (and the depth attachment, which must match the
    ///      colour attachment's size or the render pass is invalid);
    ///   3. **the camera's viewport, in CSS pixels**.
    ///
    /// (3) is the one that is easy to miss, and missing it is exactly the
    /// stretch. `Camera::distance` is `viewport_height_px / (2 tan(fovy/2) *
    /// 2^zoom)` and `view_projection_f64`'s aspect is
    /// `viewport_width_px / viewport_height_px`, so a stale viewport keeps
    /// the OLD aspect and the OLD scale while the same NDC cube is stretched
    /// across more pixels. Note that the camera is parameterised by explicit
    /// dimensions and not by an aspect ratio -- an aspect ratio alone cannot
    /// tell "wider window, same scale, more world" from "same world,
    /// stretched", and this is why the engine's own camera needed no change.
    ///
    /// The viewport also feeds `cover()` -> `cover_plan::tile_cover`, so the
    /// tile set widens with the window on its own: the frame loop's
    /// `request_missing_tiles`/`bake_some` run against the new cover on the
    /// very next frame, with no resize-specific tile code here.
    fn apply_resize(&mut self) {
        let Some(window) = web_sys::window() else { return };
        // `client_width`/`client_height` are the CSS box, which is what the
        // camera wants; `0` happens while the page is hidden or mid-layout,
        // and a zero viewport makes the projection degenerate.
        let css_width = self.canvas.client_width().max(1) as f32;
        let css_height = self.canvas.client_height().max(1) as f32;
        let dpr = (window.device_pixel_ratio() as f32).clamp(0.5, 8.0);
        // Recorded BEFORE the early return below, so that a ratio change the
        // clamp happens to absorb is still counted as seen -- otherwise the
        // frame loop's poll would call back in here every single frame.
        self.dpr_seen = dpr;

        let (width, height, scale) = backing_store_size(
            css_width,
            css_height,
            dpr,
            self.ctx.device.limits().max_texture_dimension_2d,
        );

        if width == self.surface_config.width
            && height == self.surface_config.height
            && css_width == self.camera_control.camera().viewport_width_px
            && css_height == self.camera_control.camera().viewport_height_px
        {
            return;
        }

        // 1. the backing store.
        self.canvas.set_width(width);
        self.canvas.set_height(height);
        // 2. the surface, and the depth attachment that has to match it.
        self.surface_config.width = width;
        self.surface_config.height = height;
        self.surface.configure(&self.ctx.device, &self.surface_config);
        self.depth_view = self.pipeline.create_depth_view(&self.ctx, width, height);
        // 3. the camera. Everything above is pixels; this is the map.
        self.camera_control.set_viewport(css_width, css_height);
        // The shader's analytic antialiasing band is in physical pixels, so
        // it follows the ACTUAL backing-store ratio, which is `scale` and not
        // `dpr` whenever the clamp above bit.
        self.dpr = scale;
        self.needs_redraw = true;
    }

    /// Draws exactly `plan.draws`, in exactly that order.
    ///
    /// The list already contains the stand-ins, coarse first, and the ideal
    /// tiles after them; this function makes no coverage decision of its own.
    /// That is the point -- the upload set and the draw set are the same
    /// vector, which is the invariant `render_tile_source.cpp:40-44` keeps and
    /// the one whose absence made an evicted stand-in unrecoverable on native.
    /// Logs, once, the moment every tile in the cover is drawing its own
    /// freshly-baked mesh.
    ///
    /// "First triangle" is a real milestone but it is not "the map is up": it
    /// fires when ONE tile of the sixteen has been baked, which on a fast
    /// connection happens while the other fifteen are still in flight. The
    /// number a user would recognise as load time is this one, and it runs
    /// roughly an order of magnitude longer -- so measuring only the first
    /// triangle would have hidden every improvement and every regression in
    /// the fifteen tiles after it.
    ///
    /// `plan.fresh == plan.cover_len` is the same predicate `map_is_settled`
    /// uses, restated here rather than shared because that one is an
    /// instrumentation export the sweep driver polls and this one is a
    /// one-shot on the frame path.
    fn log_full_cover(&mut self, plan: &FramePlan) {
        if self.full_cover_logged {
            return;
        }
        // The boot card's progress line, refreshed every frame until the
        // cover completes. `fresh`, not `decoded.len()`: what the user is
        // waiting for is tiles ON SCREEN at this zoom, and a decoded tile
        // that has not been baked yet is not one of them.
        if plan.cover_len > 0 {
            boot_stage(&format!("{} of {} tiles", plan.fresh, plan.cover_len));
        }
        if plan.cover_len == 0 || plan.fresh as usize != plan.cover_len {
            return;
        }
        self.full_cover_logged = true;
        self.startup.mark_once("full cover");
        // The boot card goes away when the map is COMPLETE, not when it
        // first has a triangle. Dismissing it at first triangle would show
        // the user a map with holes in it and call that "loaded"; this is
        // the moment every tile of the cover is on screen at the current
        // zoom. `document_hidden()` is not consulted -- if the tab is
        // hidden the transition simply does not run until it is shown, which
        // is the correct behaviour and not one this crate has to arrange.
        boot_done();
        // The frame count is on this line for the same reason it is on the
        // first-triangle one, plus a second: reached from the hidden-tab pump
        // it is legitimately ZERO. Every tile is decoded and baked and not one
        // has been drawn, which is the correct outcome for a tab nobody is
        // looking at and would be an alarming one for a tab somebody is.
        log(&format!(
            "page load -> full cover: {:.0} ms ({} tiles) over {} frames, \
             {} bakes costing {:.0} ms of CPU ({:.1} ms/tile, worst tile {:.1}, \
             worst frame {:.1}){}",
            now_ms(),
            plan.cover_len,
            self.metrics.frames,
            self.metrics.bakes_cover + self.metrics.bakes_prefetch,
            self.metrics.bake_ms,
            self.metrics.bake_ms
                / (self.metrics.bakes_cover + self.metrics.bakes_prefetch).max(1) as f64,
            self.metrics.bake_ms_max,
            self.metrics.bake_ms_frame_max,
            if document_hidden() { ", document HIDDEN -- baked but not drawn" } else { "" },
        ));
        log(&format!("startup: {}", self.startup.report()));
        log(&format!(
            "memory at full cover: {}",
            memory_json(self.decoded.len(), self.gpu_tiles.len(), self.raster_counts())
        ));
    }

    /// One frame's labels: project, shape, place, and draw.
    ///
    /// # The shape of this function is not its own
    ///
    /// Every step below is `symbol_pass`, which is `map_view.rs`'s label
    /// code moved to a file a browser can include. `basemap_candidate` does
    /// the projection, culling, shaping, positioning and boxing for one
    /// feature; `place_visible_symbols` runs the whole list through the
    /// collision index; `sort_symbols_for_drawing` puts the survivors in
    /// style-layer order; `text_areas` synthesises the halo. What is written
    /// HERE is only the loop around them and the pass they draw into --
    /// which is the same claim `render::mesh` makes about the triangles.
    ///
    /// Two things differ from native, both deliberate and both visible in
    /// the counters:
    ///
    /// * **A `mapbox://`-sprited style draws OUR sheet, not Mapbox's.** The
    ///   pass itself is the shared one -- `symbol_pass` builds the
    ///   `IconInstance`s and this function draws them, in the same pass and
    ///   the same order as `map_view.rs` -- but for `examples/mapbox-style.
    ///   json`, whose `"sprite"` needs a token this repository will not
    ///   bake in, the sheet behind the names is the one `web/sprites/`
    ///   generates rather than Mapbox's. See [`SPRITE_URL`] and
    ///   [`sprite_base`]. A style whose `"sprite"` is already a plain
    ///   `http(s)://` URL (Liberty's OpenFreeMap sheet) draws from THAT
    ///   sheet instead, same as native. Before a sheet lands (and forever
    ///   under `?sprite=none`) `icon_atlas_size` is `(0, 0)` and every
    ///   icon-bearing feature draws its text and skips its icon, which is
    ///   what the native build does before `set_sprites` is called.
    /// * **No attribution plate.** The credits are DOM over the canvas
    ///   (`dom_chrome_obstacles` keeps labels clear of them). The caller
    ///   overlays' own labels ARE placed here, through the loop native runs
    ///   (`symbol_pass::collect_overlay_candidates`).
    fn draw_labels(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        plan: &FramePlan,
        globe_frame: &crate::render::globe::GlobeFrame,
        light: crate::render::SymbolLight,
        fog: crate::render::HorizonFog,
    ) {
        let Some(labels) = self.labels.as_mut() else { return };
        let scale_factor = self.dpr;
        let (physical_width, physical_height) =
            (self.surface_config.width as f32, self.surface_config.height as f32);
        // glyphon works entirely in PHYSICAL pixels -- `TextArea::left` is
        // already-physical and its `scale` stays 1.0 -- while the camera and
        // `AnchorProjection::screen` are in LOGICAL ones. The
        // resolution comes off `surface_config` rather than off
        // `viewport_*_px * dpr` so it can never drift from the surface that
        // was actually configured, including when `backing_store_size`'s
        // texture-limit clamp bit.
        labels.viewport.update(
            &self.ctx.queue,
            glyphon::Resolution { width: self.surface_config.width, height: self.surface_config.height },
        );

        let pass_started = now_ms();
        // The running totals survive the frame; the per-frame counts do not.
        let mut census =
            LabelCensus { pass_ms_max: labels.census.pass_ms_max, pass_frames: labels.census.pass_frames,
                          pass_ms_sum: labels.census.pass_ms_sum, ..LabelCensus::default() };
        let mut candidates: Vec<crate::symbol_pass::SymbolCandidate> = Vec::new();
        let mut bypassed: Vec<crate::symbol_pass::DrawnSymbol> = Vec::new();
        // The sprite sheet's packed rects, or an empty map when no sheet has
        // landed (or `?sprite=none` refused one). `icon_atlas_size` of
        // `(0, 0)` is the signal `symbol_pass` reads: every icon-bearing
        // feature then draws its text and skips its icon, which is what the
        // native build does before `set_sprites` is called. Named rather
        // than inlined so the two states are visible side by side.
        let no_rects: HashMap<String, IconRect> = HashMap::new();
        let (icon_rects, icon_atlas_size) = match self.icons.as_ref() {
            Some(icons) => (&icons.rects, icons.atlas_size),
            None => (&no_rects, (0, 0)),
        };
        // Labels over data: the on-data paint for the labels over one of
        // the page's polygons (memoised per label), the style's elsewhere.
        let area_cover = if self.label_paint.is_some() { self.overlays.area_cover() } else { Default::default() };
        let label_cover = &self.label_cover;
        let over_data = |tile: TileId, anchor: (i32, i32)| label_cover.covers(&area_cover, tile, anchor);
        let frame = crate::symbol_pass::SymbolFrame {
            occlusion: None,
            label_paint: self.label_paint.map(|paint| crate::symbol_pass::LabelsOnData {
                paint,
                over_data: (!area_cover.is_empty()).then_some(&over_data as &dyn Fn(TileId, (i32, i32)) -> bool),
            }),
            camera: self.camera_control.camera(),
            style_layers: &self.style.layers,
            scale_factor,
            size_scale: scale_factor,
            collision_padding: crate::symbol_pass::SYMBOL_COLLISION_PADDING_LOGICAL_PX * scale_factor,
            size_blend_t: self.camera_control.camera().zoom.fract() as f32,
            icon_rects,
            icon_atlas_size,
            // The browser build's own globe, narrowed from the SAME
            // `globe_frame` value `draw` built its tile uniforms from rather
            // than from a second call -- the two frontends place labels
            // through one `symbol_pass`, so they must agree about the sphere
            // as well as about the shaping, and two `globe_frame` calls in
            // one frame are free to disagree about where it is.
            globe: crate::render::globe::GlobeAnchors::new(globe_frame),
            // LIT, from the same sun the ground under these glyphs was lit
            // from -- `draw` resolves it once and hands it down. The pair has
            // to agree: labels dimmed over an undimmed night hemisphere, or
            // the reverse, is the very mismatch `render::SymbolLight` exists
            // to remove. It is `SymbolLight::UNLIT` in everything but name
            // on a flat frame, where `space_transition` is 0.
            light,
            // THIS FRAME's fade, the value `draw` wrote into the camera
            // uniform -- a label past its end is not placed.
            fog,
        };
        // `plan.draws`, which is the cover plus whatever stand-ins are
        // filling holes. A stand-in's symbols are its OWN tile's -- style
        // resolved at the display zoom, line spacing laid out at its own zoom
        // (`symbol_line::symbol_spacing_zoom`) -- so a z11 tile standing in
        // under a z13 camera contributes z11 labels, which is the right answer
        // and the same one its triangles give. The same holds for a far,
        // coarse cover tile under the distance LOD.
        let visible_plans: Vec<(TileId, Rc<crate::style::SymbolPlan>)> = plan
            .draws
            .iter()
            .filter_map(|tile| self.gpu_tiles.get(tile).map(|gpu| (*tile, gpu.symbols.clone())))
            .collect();
        // A real `GlyphSink`, not `None` -- this used to be the one place
        // this page did less than `map_view.rs` does. With `None`, a
        // line-placed street name was still shaped, still placed along its
        // road and still given a collision box (every `census` number was
        // already a drawing frame's number, which is exactly why
        // `symbol_pass::label_census` passes `None` on purpose -- see its
        // doc comment), but `LineLabelPlacement::quads` came back empty, so
        // the label reserved its space and drew nothing. `labels.glyph_atlas`
        // and `labels.swash_cache` are the same two resources
        // `map_view.rs`'s `GpuState` pairs for exactly this call.
        let mut glyph_sink =
            crate::symbol_pass::GlyphSink { atlas: &mut labels.glyph_atlas, swash: &mut labels.swash_cache };
        census.features = crate::symbol_pass::collect_candidates(
            &frame,
            visible_plans.iter().map(|(tile, plan)| (*tile, &**plan)),
            &mut labels.font_system,
            &mut labels.shape_cache,
            Some(&mut glyph_sink),
            &mut candidates,
            &mut bypassed,
        );
        // The caller overlays' own labels and icons (a point layer's
        // `text-field`): `symbol_pass::collect_overlay_candidates`, the loop
        // `map_view.rs` runs, so they are placed with the overlay's layer
        // index -- before every basemap label, drawn after them -- and
        // collide with the basemap's labels as on native. Same sort keys
        // (`ShellOverlays::symbol_sources`). No icon geometry on this page:
        // an `icon-text-fit` sprite draws unstretched.
        let symbol_sources = self.overlays.symbol_sources();
        if !symbol_sources.is_empty() {
            let camera = self.camera_control.camera();
            let projector = crate::render::globe::WorldProjector::new(camera, &frame.globe);
            let no_geometry: HashMap<String, crate::sprite::IconGeometry> = HashMap::new();
            let overlay_frame = crate::symbol_pass::OverlaySymbolFrame {
                projector: &projector,
                zoom: camera.zoom,
                viewport: (camera.viewport_width_px, camera.viewport_height_px),
                scale_factor,
                collision_padding: frame.collision_padding,
                icon_rects,
                icon_atlas_size,
                icon_geometry: &no_geometry,
            };
            census.overlay_symbols = crate::symbol_pass::collect_overlay_candidates(
                &overlay_frame,
                symbol_sources
                    .iter()
                    .flat_map(|(base, features)| features.iter().map(move |f| ((base + f.index) as f32, f))),
                &mut labels.font_system,
                &mut labels.shape_cache,
                &mut candidates,
                &mut bypassed,
            );
        }
        census.candidates = candidates.len();

        labels.collision.reset(crate::symbol_pass::symbol_collision_bounds(
            self.camera_control.camera().viewport_width_px,
            self.camera_control.camera().viewport_height_px,
            scale_factor,
        ));
        // The remaining route's screen path, so no label sits on the line.
        // Empty with no trip.
        for route_box in self.nav.collision_boxes(self.camera_control.camera(), scale_factor) {
            labels.collision.insert_obstacle(route_box);
        }
        // The credit card and the SWAT mark, which this page draws as DOM
        // over the canvas: no label is placed under either (native keeps
        // clear of its own plate and mark the same way, `corner_chrome`).
        for chrome_box in dom_chrome_obstacles(scale_factor) {
            labels.collision.insert_obstacle(chrome_box);
        }
        // Place, merge the bypassed symbols in, sort into style-layer order,
        // clip a label behind a later icon, and split the survivors into the
        // three lists the draw code below wants -- shared with `map_view.rs`
        // via `render::label_plan::label_draw_plan`, so this page's split
        // cannot silently stop matching native's again. See that module's
        // doc comment for the defect it replaces.
        let crate::render::label_plan::LabelDrawPlan {
            mut icon_instances,
            mut symbol_text,
            mut glyph_instances,
            symbols_placed,
            overlay_sort_keys: _,
        } = crate::render::label_plan::label_draw_plan(candidates, bypassed, &mut labels.collision);
        census.placed = symbols_placed;
        census.labels = symbol_text.len();
        // See `Labels::texts`: off unless an instrument asked for it.
        if LABEL_TEXT_DUMP.with(|f| f.get()) {
            labels.texts.clear();
            labels.texts.extend(symbol_text.iter().map(|((key, ..), _)| key.0.clone()));
        }

        // New glyph rasters for glyphon, under a per-frame allowance -- see
        // `render::text_glyph_budget`. Before the fade below, so a deferred
        // label's fade starts on the frame its text first draws. `[FROM
        // NATIVE]` -- `map_view.rs`'s identical block.
        let text_deferred = self.text_glyph_budget.admit(
            &mut symbol_text,
            crate::render::text_glyph_budget::NEW_GLYPHS_PER_FRAME,
            |((key, ..), _)| labels.shape_cache.get(key).map(|shaped| shaped.glyph_keys.as_slice()),
        );
        if text_deferred > 0 {
            self.needs_redraw = true;
        }
        // Fade-in for whatever arrived since last frame, keyed by content
        // and nearest position -- see `render::label_fade`. Applied to the
        // map's own symbols only. `[FROM NATIVE]` -- `map_view.rs`'s
        // identical block, capped through the same `FadeRedrawScheduler` --
        // see that type's own doc comment, and `MapApp::fade_wake_at_s`'s
        // for the delayed wake a turned-down request is owed.
        {
            use crate::render::label_fade::fade_id;
            let now_s = now_ms() / 1000.0;
            let fade = &mut self.label_fade;
            fade.begin_frame(now_s, 0.1 * physical_width.max(physical_height));
            for ((key, left, top, color, _, halo_color, _), _) in symbol_text.iter_mut() {
                let opacity = fade.opacity(fade_id(0, &(&key.0, &key.4)), *left, *top);
                color.a *= opacity;
                halo_color.a *= opacity;
            }
            for (kind, quads) in [(1u8, &mut icon_instances), (2u8, &mut glyph_instances)] {
                for quad in quads.iter_mut() {
                    let id = fade_id(
                        kind,
                        &[quad.uv_min.0.to_bits(), quad.uv_min.1.to_bits(), quad.uv_max.0.to_bits(), quad.uv_max.1.to_bits()],
                    );
                    quad.opacity *= fade.opacity(id, quad.screen_x, quad.screen_y);
                }
            }
            let animating = fade.end_frame();
            use crate::render::label_fade::FadeFollowUp;
            self.fade_wake_at_s = match self.fade_redraw_gate.follow_up(now_s, animating) {
                FadeFollowUp::Idle => None,
                FadeFollowUp::RedrawNow => {
                    self.needs_redraw = true;
                    None
                }
                FadeFollowUp::WakeAt(at) => Some(at),
            };
        }

        // The glyph atlas's texture, and the UVs that address it, settled
        // together -- exactly `map_view.rs`'s own pair of steps, in the same
        // order, for the same reason (see that file's comment on this same
        // sequence). The atlas grew while `collect_candidates` rastered new
        // glyphs above, so it has to be (re-)uploaded before a quad can
        // sample it, and every quad's texel UVs have to be divided by the
        // size that upload establishes.
        //
        // Gated on `self.icons`, not on `labels`: `IconPipeline` is the one
        // pipeline both atlases draw through (see `render_batches`' own doc
        // comment), and on this page -- as on native -- it does not exist
        // until a sprite sheet has been set, because that is the only place
        // either build constructs one. A page with fonts but no sprites yet
        // draws no line-placed labels either, for one frame, same as native.
        if !glyph_instances.is_empty() {
            if let Some(icons) = self.icons.as_ref() {
                let generation = labels.glyph_atlas.generation();
                if labels.glyph_atlas_bind_group.is_none()
                    || labels.glyph_atlas_generation != Some(generation)
                {
                    labels.glyph_atlas_bind_group =
                        Some(icons.pipeline.upload_atlas(&self.ctx, labels.glyph_atlas.rgba()));
                    labels.glyph_atlas_generation = Some(generation);
                }
            }
            let (_, atlas_width, atlas_height) = labels.glyph_atlas.image();
            crate::symbol_pass::normalise_glyph_uvs(&mut glyph_instances, atlas_width, atlas_height);
            // No sprite sheet yet, so no `IconPipeline` and nothing to
            // sample: drop the quads rather than draw them against no bind
            // group at all.
            if labels.glyph_atlas_bind_group.is_none() {
                glyph_instances.clear();
            }
        }

        // Text is PREPARED before the pass opens, because `prepare` can fail
        // -- `PrepareError::AtlasFull` once enough labels share one atlas in
        // a dense area -- and a failed prepare must skip this frame's text
        // without taking down the icons, which are already encoded and have
        // nothing to do with glyphon.
        let text_prepared = if symbol_text.is_empty() {
            false
        } else {
            let areas = crate::symbol_pass::text_areas(
                &symbol_text,
                &labels.shape_cache,
                physical_width,
                physical_height,
            );
            census.text_areas = areas.len();
            match labels.renderer.prepare(
                &self.ctx.device,
                &self.ctx.queue,
                &mut labels.font_system,
                &mut labels.atlas,
                &labels.viewport,
                areas,
                &mut labels.swash_cache,
            ) {
                Ok(()) => {
                    // What this frame's `text_glyph_budget.admit` let through
                    // really did land in the atlas -- `[FROM NATIVE]`.
                    self.text_glyph_budget.confirm();
                    true
                }
                Err(err) => {
                    log(&format!("glyphon prepare: {err:?}"));
                    // Nothing admitted this frame is known-cached after all.
                    self.text_glyph_budget.discard();
                    false
                }
            }
        };

        // ONE pass for both halves, opened only if there is something to put
        // in it -- and it is the same single `LoadOp::Load` pass the labels
        // already used, not a second one stacked on top. `map_view.rs`'s
        // `symbol-overlay-pass` makes exactly this choice for exactly this
        // reason; the two frontends must not differ on how many passes a
        // frame costs, or a pass-timing comparison between them measures the
        // difference in bookkeeping.
        // A frame with no icons must report a zero, not the last frame that
        // had some -- `FrameSample::icon_ms` is read every frame and would
        // otherwise smear one busy frame's cost across the still ones after
        // it.
        if icon_instances.is_empty() {
            if let Some(icons) = self.icons.as_mut() {
                icons.draw_ms = 0.0;
                icons.drawn = 0;
            }
        }
        if text_prepared || !icon_instances.is_empty() || !glyph_instances.is_empty() {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("symbols"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            // Icons BEFORE text, at the same screen position: a route number
            // has to land on top of its shield, not under it, and this pass
            // has no depth test -- draw order is the only thing deciding
            // who is on top. Same order, same reason, as `map_view.rs`.
            //
            // Line-placed text rides in the SAME call, as a second batch:
            // one pipeline, one vertex buffer, two `draw`s with two bind
            // groups -- `IconPipeline::render_batches`' own doc comment
            // explains why a second `render` call here would clobber the
            // first's vertices. The glyphs draw SECOND, so a street name
            // sits over a road arrow rather than under it, matching the
            // order `glyphon`-drawn labels have always had against icons.
            if !icon_instances.is_empty() || !glyph_instances.is_empty() {
                if let Some(icons) = self.icons.as_mut() {
                    let icon_started = now_ms();
                    let mut batches: Vec<(&wgpu::BindGroup, &[IconInstance])> = Vec::new();
                    if !icon_instances.is_empty() {
                        batches.push((&icons.bind_group, &icon_instances));
                    }
                    if let Some(glyph_atlas) = labels.glyph_atlas_bind_group.as_ref() {
                        if !glyph_instances.is_empty() {
                            batches.push((glyph_atlas, &glyph_instances));
                        }
                    }
                    icons.pipeline.render_batches(
                        &self.ctx,
                        &mut pass,
                        &batches,
                        physical_width,
                        physical_height,
                    );
                    icons.draw_ms = now_ms() - icon_started;
                    icons.draw_ms_max = icons.draw_ms_max.max(icons.draw_ms);
                    icons.draw_ms_sum += icons.draw_ms;
                    icons.draw_frames += 1;
                    icons.drawn = icon_instances.len();
                }
            }
            if text_prepared {
                if let Err(err) = labels.renderer.render(&labels.atlas, &labels.viewport, &mut pass) {
                    log(&format!("glyphon render: {err:?}"));
                }
            }
        }
        census.icons = icon_instances.len();

        // Instrument-only, and off unless [`map_icons_json`] switched it on:
        // which sprite names this frame's features ASKED for that the sheet
        // does not carry. A shield that draws as nothing has exactly two
        // causes -- the expression resolved to no name at all, or it
        // resolved to a name the sheet lacks -- and only the second one
        // leaves a trace anywhere. Walking every visible feature is O(features)
        // and stays off the drawing path entirely.
        if ICON_NAME_DUMP.with(|f| f.get()) {
            let mut wanted: Vec<String> = Vec::new();
            for (_, plan) in &visible_plans {
                for feature in &plan.features {
                    let Some(name) = feature.icon.as_deref() else { continue };
                    if !wanted.iter().any(|seen| seen == name) {
                        wanted.push(name.to_string());
                    }
                }
            }
            if let Some(icons) = self.icons.as_mut() {
                icons.missing = wanted
                    .into_iter()
                    .filter(|name| !icons.rects.contains_key(name))
                    .take(MAX_MISSING_ICON_NAMES)
                    .collect();
                let (atlas_w, atlas_h) = (icons.atlas_size.0 as f32, icons.atlas_size.1 as f32);
                icons.names = icon_instances
                    .iter()
                    .map(|instance| {
                        let origin = (
                            (instance.uv_min.0 * atlas_w).round() as u32,
                            (instance.uv_min.1 * atlas_h).round() as u32,
                        );
                        icons
                            .by_origin
                            .get(&origin)
                            .cloned()
                            .unwrap_or_else(|| format!("?@{},{}", origin.0, origin.1))
                    })
                    .collect();
            }
        }

        labels.atlas.trim();
        // Retains exactly what this frame drew and nothing else, capped --
        // otherwise a long pan grows one `glyphon::Buffer` per label ever
        // seen and never gives one back.
        labels.shape_cache.end_frame();
        census.shaped_total = labels.shape_cache.shape_count;
        census.pass_ms = now_ms() - pass_started;
        census.pass_ms_max = census.pass_ms_max.max(census.pass_ms);
        census.pass_frames += 1;
        census.pass_ms_sum += census.pass_ms;
        labels.census = census;
    }

    /// Plans the dot layers for this camera (`DotLayers::frame`): which
    /// count tiles are wanted, the view's people per dot, eviction. Every
    /// tick, drawn or not, like `plan_frame`; the jobs it makes are handed
    /// out by [`drive_dot_jobs`].
    fn plan_dots(&mut self) {
        if self.dots.is_empty() {
            return;
        }
        let camera = *self.camera_control.camera();
        let cover = crate::render::globe::GlobeCover::new(&crate::render::globe::globe_frame(&camera));
        self.dots.frame(&camera, &cover, crate::timing::Instant::now());
    }

    fn draw(&mut self, plan: &FramePlan) {
        let draw_started = now_ms();
        // `setMarkers` labels follow their points, on the camera this frame
        // draws with -- not the one before the tick stepped it.
        markers_api::place_labels(self);
        self.pipeline.write_camera_uniform(
            &self.ctx,
            &self.camera_buffer,
            self.camera_control.camera().view_projection().to_cols_array_2d(),
            self.camera_control.camera().zoom as f32,
            self.dpr,
        );
        // `circle-pitch-scale: viewport`'s centre distance (`vs_circle`), as
        // `MapView::paint` writes it.
        self.pipeline.write_circle_frame(&self.ctx, &self.camera_buffer, self.camera_control.camera().distance() as f32);
        // This frame's globe, built ONCE and asked per tile -- the same shape
        // `MapView::paint` uses (`map_view.rs:8807`), and from the same
        // `globe_frame` the label pass narrows for its anchors, so the
        // triangles and the labels on top of them cannot disagree about where
        // the sphere is. Above `GLOBE_ZOOM_MAX` (z6) `transition` is 0.0 and
        // `shader.wgsl` takes its flat early-out, so this costs one matrix
        // build per frame and changes no pixel.
        //
        // LIT, from `self.space`, exactly as `MapView::paint` is -- see
        // [`space_default`] for why this page has a `Space` to light it from.
        let globe_frame = crate::render::globe::globe_frame(self.camera_control.camera());
        let space_transition = globe_frame.space_transition;
        // THE SUN, ONCE -- the same reason `map_view.rs` resolves it once:
        // `Sun::Now` reads a clock, the subsolar point moves 0.0042 degrees a
        // second, and two unsynchronised reads in one frame would light the
        // ground and the air from two different instants.
        let frame_atmosphere = crate::render::atmosphere::Atmosphere {
            // Daylight while the terminator is off -- native's one call.
            sun: crate::render::atmosphere::frame_sun_for(&self.space.atmosphere, None),
            ..self.space.atmosphere
        };
        let sun_direction =
            frame_atmosphere.sun.direction_sphere(&globe_frame.globe_view.inverse());
        // The night floor, raised to 1.0 by the SPACE fade rather than the
        // projection's -- see `map_view.rs` for the algebra that makes this
        // exact rather than approximate.
        let night_floor = crate::render::atmosphere::night_floor(frame_atmosphere.night_brightness_clamped(), globe_frame.space_transition);
        // The symbol pass's half of the same sun, built from the same three
        // values `GlobeUpload::new` is handed below so a glyph and the
        // triangles under it cannot be lit from two instants.
        // `night_brightness_clamped()` rather than `night_floor`:
        // `SymbolLight::new` folds the space fade in itself, differently,
        // because a glyph's floor is raised.
        let symbol_light = crate::render::SymbolLight::new(
            sun_direction,
            frame_atmosphere.night_brightness_clamped(),
            globe_frame.space_transition,
        );
        let globe = crate::render::globe::GlobeUpload::new(
            &globe_frame,
            self.camera_control.camera(),
            TILE_EXTENT,
            sun_direction,
            night_floor,
        );
        // The route line's mesh, rebuilt when the layer changes. With no
        // trip it is dropped (or was never made) and nothing is uploaded.
        // After `globe`, which projects it onto the same sphere as the tiles.
        crate::render::nav_layer::sync_route(
            &mut self.nav_route,
            &self.ctx,
            &self.pipeline,
            self.nav.route_layer(),
            self.camera_control.camera(),
            &globe,
        );
        // Caller overlays: upload what landed, pick each cover tile's draw
        // (own, clipped ancestor, descendants), write this frame's uniforms.
        // Projected on the same globe as the tiles.
        self.overlays.prepare(&self.ctx, &self.pipeline, self.camera_control.camera(), &globe, &plan.cover);
        // Population dots: upload the tiles that landed (or were rebuilt),
        // write this frame's uniforms, report dots drawn. No dot mask: the
        // relocation mask is rendered from the basemap's mask lists, which
        // this page's tiles do not carry yet, so the page's layer is
        // `relocate: false` (`load_dot_source`) and dots sit uniformly in
        // their cell -- the dasymetric plan's DP-7 web half is still owed.
        self.dots_gpu.sync(&mut self.dots, &self.ctx, &self.pipeline, self.camera_control.camera(), &globe, None);

        // ---- WHAT THE GLOBE SITS ON ------------------------------------
        //
        // One plan, read at every gate below -- the clear colour, the
        // backdrop, the stars and the `SpaceDraw` this frame builds -- rather
        // than each recomputing the same three-way decision against
        // `self.space` and `frame_atmosphere` for itself. See
        // `render::frame_plan`: this is the call that used to be missing
        // here, which is the whole reason the browser never drew a star.
        //
        // Here, above the `draws` list, for the native file's borrow reason
        // as much as for the draw order: `draws` below holds immutable
        // borrows of `self.gpu_tiles`, and building a star pipeline or
        // retessellating the backdrop wants `&mut self`.
        //
        // The PAIR, not a lerped colour -- see `background_pair`.
        let background_pair = background_pair(&self.style, self.camera_control.camera().zoom.floor() as u8);
        let background_blend_t = self.camera_control.camera().zoom.fract() as f32;
        let background_color =
            background_pair.map(|(from, to)| from.lerp(&to, background_blend_t));
        self.brand_variant = Some(crate::brand_mark::resolve_for_frame(
            crate::brand_mark::BrandMarkVariant::Auto,
            background_color,
            &self.space,
            space_transition,
        ));
        // The space look draws the point stars too, at its own intensity --
        // native's `look_space`, read by the plan and the star write below.
        let look_space = if self.space_look.enabled {
            crate::render::space::Space {
                star_intensity: self.space.star_intensity.max(self.space_look.star_intensity),
                ..self.space
            }
        } else {
            self.space
        };
        // The look's clock: `?look_time=`, else the wall clock (native's
        // `read_scene_clock` with no scene clock). Read once per frame.
        let look_sidereal = if self.space_look.enabled {
            crate::render::space_look::sidereal_for(Some(self.look_time.unwrap_or_else(|| js_sys::Date::now() / 1000.0)))
        } else {
            0.0
        };
        let space_plan = crate::render::frame_plan::space_plan(
            background_color,
            &look_space,
            &frame_atmosphere,
            &self.style.sky.at_zoom(self.camera_control.camera().zoom),
            space_transition,
            // Does any pixel look above the limb? A fact of the geometry;
            // the plan decides what follows from it (`render::sky`).
            crate::render::sky::sees_above_horizon(self.camera_control.camera(), &globe_frame),
        );
        // `space_plan` returns `background_color` UNCHANGED at
        // `transition == 0.0`; the flat branch is kept separate anyway so
        // that a flat frame's clear is computed by the same expression it
        // was before this page had a backdrop, not merely by one equal to
        // it. Unlike native, the fallback is `self.background` rather than
        // `TRANSPARENT`: `style_background` already defaults a style with no
        // background layer to white, and that default is this page's.
        // A frame whose sky draws clears to space even above z6 -- see
        // `frame_plan::space_plan`.
        let clear_color = if space_transition > 0.0 || space_plan.draws_sky {
            TilePipeline::clear_color(&space_plan.clear_color)
        } else {
            background_color.map_or(self.background, |c| TilePipeline::clear_color(&c))
        };
        // THE HORIZON FADE, decided by `cover_plan::frame_fog` from this
        // frame's cover cuts -- the vector one (`last_cover_reach`, set by
        // this frame's cover) and the imagery one, from the raster cover
        // taken HERE and reused by the raster draw below -- and the colour
        // the flat frame clears to, and written into the camera uniform
        // AFTER `write_camera_uniform` (which leaves it off).
        // `HorizonFog::NONE` below 50 degrees and on every globe frame, so
        // those frames draw byte-for-byte what they drew before.
        let (raster_cover, raster_reach) = self.raster_cover();
        // The decoded imagery this frame must not evict: the cover, its
        // floor and z0 -- native's `raster_working_set`, one definition.
        self.raster_decoded.set_cover(&raster_cover);
        let fog = crate::render::cover_plan::frame_fog(
            self.camera_control.camera(),
            self.last_cover_reach.get(),
            raster_reach,
            // The colour this frame ACTUALLY clears to: a style with no
            // background layer clears to `self.background`, and must fade
            // toward it rather than not at all.
            Some(crate::render::cover_plan::fog_background(background_color, self.background)),
            space_transition,
        );
        self.pipeline.write_horizon_fog(&self.ctx, &self.camera_buffer, fog);

        if space_plan.draws_backdrop {
            // The ocean sphere: the background layer re-expressed as a
            // sphere BEHIND the tiles, which is what gives the planet an
            // edge. Without it Liberty's background colour is the clear
            // colour and fills the whole viewport, and the silhouette simply
            // is not there.
            let transparent = crate::style::Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 };
            let ocean = background_pair.unwrap_or((transparent, transparent));
            let tile = crate::render::space::BACKDROP_TILE;
            let matrix = crate::render::combined_tile_matrix(self.camera_control.camera(), tile, TILE_EXTENT)
                .to_cols_array_2d();
            if self.backdrop.as_ref().is_none_or(|held| held.color != ocean) {
                let mesh =
                    crate::render::space::backdrop_mesh(TILE_EXTENT as u32, ocean.0, ocean.1);
                let buffers = self.pipeline.create_tile_buffers(&self.ctx, &mesh);
                let (model_buffer, model_bind_group) = self.pipeline.create_model_bind_group(
                    &self.ctx,
                    matrix,
                    background_blend_t,
                    0.0,
                    globe.for_tile(tile),
                );
                self.backdrop =
                    Some(Backdrop { color: ocean, buffers, model_buffer, model_bind_group });
            } else if let Some(held) = self.backdrop.as_ref() {
                // One `combined_tile_matrix` and one 192-byte memcpy for ONE
                // tile, whether or not the camera moved -- cheaper than
                // tracking whether it was needed.
                self.pipeline.write_model_uniform(
                    &self.ctx,
                    &held.model_buffer,
                    matrix,
                    background_blend_t,
                    0.0,
                    globe.for_tile(tile),
                );
            }
        }

        if space_plan.draws_stars {
            // Built on the first frame that draws stars and kept: 1.24 MB of
            // static vertices for all 9,096 catalogue stars, which a
            // consumer that never asks for stars never pays. See
            // `MapApp::star_pipeline`.
            let stars = self.star_pipeline.get_or_insert_with(|| {
                crate::render::space::StarPipeline::new(
                    &self.ctx,
                    self.render_format,
                    self.pipeline.depth_format(),
                )
            });
            if self.space_look.enabled {
                stars.write_sky_turned(&self.ctx, self.camera_control.camera(), &look_space, look_sidereal);
            } else {
                stars.write_sky(&self.ctx, self.camera_control.camera(), &self.space);
            }
        }

        // The space look, as native's `MapView::paint`: until the textures
        // are in (or if they never arrive) it is its shading and rim alone,
        // on a 1x1 stand-in (generation 0). A set that landed is uploaded
        // ONCE and its decoded RGBA dropped here.
        let draws_look = self.space_look.draws(space_transition);
        if draws_look {
            if let Some(textures) = self.space_textures.take() {
                self.space_look_pipeline = Some(crate::render::space_look::SpaceLookPipeline::new(
                    &self.ctx,
                    self.render_format,
                    self.pipeline.depth_format(),
                    &textures,
                    self.space_textures_generation,
                ));
            }
            let look = self.space_look_pipeline.get_or_insert_with(|| {
                crate::render::space_look::SpaceLookPipeline::new(
                    &self.ctx,
                    self.render_format,
                    self.pipeline.depth_format(),
                    &crate::render::space_look::SpaceTextures::default(),
                    0,
                )
            });
            look.write(
                &self.ctx,
                self.camera_control.camera(),
                &globe_frame,
                &self.space_look,
                frame_atmosphere.sun,
                look_sidereal,
            );
        }
        // Its credits, only for what this frame draws (native's
        // `MapView::attributions`): the star map while the sky pass draws,
        // the clouds until they fade out by z5.
        let mut look_credits = self.space_look_pipeline.as_ref().map_or_else(Vec::new, |look| {
            crate::render::space_look::look_credits(
                look.has_starmap(),
                look.has_clouds(),
                &self.space_look,
                space_transition,
                self.camera_control.camera().zoom,
            )
        });
        // The `?imagery=` source's credit, while this frame draws it.
        if self.raster_plan().opacity.is_some() {
            if let Some(credit) = imagery_credit(&self.raster_config.template) {
                look_credits.insert(0, credit);
            }
        }
        set_look_credits(look_credits);

        if space_plan.draws_sky {
            // The style's `sky` above the limb -- see `render::sky`.
            let sky = self.sky_pipeline.get_or_insert_with(|| {
                crate::render::sky::SkyPipeline::new(&self.ctx, self.render_format, self.pipeline.depth_format())
            });
            sky.write(&self.ctx, self.camera_control.camera(), &globe_frame, &space_plan.sky, &frame_atmosphere, space_plan.ground);
        }

        if space_plan.draws_atmosphere {
            // Built on the first globe frame and kept. Three generated
            // vertices and a 112-byte uniform, so the lazy build is about
            // the shader module rather than about memory -- see
            // `MapApp::atmosphere_pipeline`.
            let atmosphere = self.atmosphere_pipeline.get_or_insert_with(|| {
                crate::render::atmosphere::AtmospherePipeline::new(
                    &self.ctx,
                    self.render_format,
                    self.pipeline.depth_format(),
                )
            });
            // From the SAME `globe_frame` the tile uniforms came from, not a
            // second one built here: the glow has to sit on the limb the
            // tiles actually drew.
            // The intensity the PLAN decided: `Atmosphere::intensity` times
            // the style's `atmosphere-blend`.
            let air = crate::render::atmosphere::Atmosphere {
                intensity: space_plan.atmosphere_intensity,
                ..frame_atmosphere
            };
            atmosphere.write(&self.ctx, self.camera_control.camera(), &globe_frame, &air);
        }
        // Every tile in the draw list, not only the cover -- a stand-in's
        // model matrix is its OWN tile's, which is the whole of the "no new
        // tile math" claim: `combined_tile_matrix` places and scales a z9 tile
        // under a z13 camera with no help.
        //
        // Blended against the tile's OWN bake zoom, not `zoom.fract()`: after
        // an integer crossing most of the cover is a bucket stale for tens of
        // frames (11 of 12 tiles for ~10 frames at pitch 60), and `fract()`
        // drew each of those at the wrong end of its paint pair -- z15
        // widths at z16.03 -- then jumped when its re-bake landed. That was
        // the measured zoom twitch. A `REBAKE_SENTINEL` mesh (style swap)
        // was baked at a bucket this frame no longer knows; it keeps the
        // old `fract()` behaviour, i.e. is treated as baked at this bucket.
        let display_zoom = self.display_zoom();
        for tile in &plan.draws {
            let Some(gpu) = self.gpu_tiles.get(tile) else { continue };
            let baked_zoom = if gpu.baked_zoom == REBAKE_SENTINEL { display_zoom } else { gpu.baked_zoom };
            self.pipeline.write_model_uniform(
                &self.ctx,
                &gpu.model_buffer,
                crate::render::combined_tile_matrix(self.camera_control.camera(), *tile, TILE_EXTENT)
                    .to_cols_array_2d(),
                crate::render::tile_blend_t_for_baked_zoom(self.camera_control.camera().zoom, f64::from(baked_zoom)),
                crate::render::dash_scale(self.camera_control.camera(), *tile, TILE_EXTENT),
                // The stand-in's OWN id, for the same reason the matrix uses
                // it: the geometry in those buffers is the stand-in's, so the
                // Mercator coords that place it on the sphere are too.
                globe.for_tile(*tile),
            );
        }

        // ---- THE IMAGERY BASEMAP ---------------------------------------
        //
        // Resolved and uploaded HERE, before `draws` below takes its
        // immutable borrows of `self.gpu_tiles` -- the same borrow-timing
        // reason `MapView::paint` resolves its own imagery before taking
        // `gpu_meshes`. Empty and free on every page with no `?imagery=`
        // template: `raster_plan.opacity` is `None`, `raster_draw_order`
        // never grows, and no pipeline is ever built.
        //
        // **Simplified from native in one respect, and it is a documented
        // one, not a silent gap**: no ancestor stand-ins. `MapView::paint`
        // walks up the pyramid from every missing tile so a pan shows the
        // last-known coarse imagery instead of the clear colour underneath;
        // this page draws only tiles it has actually decoded. A page with
        // no imagery source pays nothing either way, and a page WITH one
        // sees a plain gap fill in rather than a blurred ancestor while a
        // tile is in flight -- a real behavioural difference from native,
        // acceptable because pixel parity is a NATIVE requirement (see
        // `a_flat_frame_is_pixel_identical_through_the_space_aware_path`),
        // not a cross-shell one; the design document's own non-goal list
        // says shells are not required to be identical, only to share the
        // DECISION -- which this does, through `Self::raster_plan`.
        let raster_plan = self.raster_plan();
        let mut raster_draw_order: Vec<TileId> = Vec::new();
        if let Some(opacity) = raster_plan.opacity {
            let cover = raster_cover;
            let mut tiles: Vec<TileId> =
                cover.iter().copied().filter(|t| self.raster_decoded.contains(t)).collect();
            // Deterministic draw order, exactly as native's raster path
            // sorts -- see `map_view.rs`'s own comment on why a `HashMap`
            // walk order cannot be allowed to decide what overlaps what at
            // the limb.
            tiles.sort_by_key(|t| (t.z, t.y, t.x));

            if !tiles.is_empty() {
                if self.raster_pipeline.is_none() {
                    self.raster_pipeline = Some(RasterPipeline::new(
                        &self.ctx,
                        self.render_format,
                        &self.pipeline,
                    ));
                }
                let raster_pipeline = self.raster_pipeline.as_ref().expect("just built if missing");
                let blend_t = self.camera_control.camera().zoom.fract() as f32;
                for tile in tiles {
                    let mesh_key = RasterMeshKey::for_tile(tile);
                    if !self.raster_squares.contains_key(&mesh_key) {
                        let mesh = crate::render::raster_mesh::build(
                            mesh_key,
                            RASTER_MESH_EXTENT,
                            crate::render::mesh::COORD_SCALE,
                        );
                        self.raster_squares
                            .insert(mesh_key, raster_pipeline.create_mesh_buffers(&self.ctx, &mesh));
                    }
                    // The SAME per-tile model uniform the vector tile above
                    // gets: same `combined_tile_matrix`, same `GlobeParams`,
                    // same blend factor -- so the imagery and the fills over
                    // it cannot disagree about where the tile is.
                    let matrix = crate::render::combined_tile_matrix(self.camera_control.camera(), tile, TILE_EXTENT)
                        .to_cols_array_2d();
                    let dash_scale = crate::render::dash_scale(self.camera_control.camera(), tile, TILE_EXTENT);
                    let globe_params = globe.for_tile(tile);

                    if let Some(held) = self.raster_tiles.get_mut(&tile) {
                        // Already uploaded. A camera move is a 32-byte queue
                        // write, not a rebuild.
                        self.pipeline.write_model_uniform(
                            &self.ctx,
                            &held.model_buffer,
                            matrix,
                            blend_t,
                            dash_scale,
                            globe_params,
                        );
                        if held.paint_opacity != opacity {
                            let (buffer, bind_group) = raster_pipeline.create_paint_bind_group(
                                &self.ctx,
                                &held.texture,
                                opacity,
                            );
                            held.paint_bind_group = bind_group;
                            held._paint_buffer = buffer;
                            held.paint_opacity = opacity;
                        }
                    } else {
                        let Some(image) = self.raster_decoded.get(&tile) else { continue };
                        let texture = raster_pipeline.upload_tile(&self.ctx, image);
                        let (paint_buffer, paint_bind_group) =
                            raster_pipeline.create_paint_bind_group(&self.ctx, &texture, opacity);
                        let (model_buffer, model_bind_group) = self.pipeline.create_model_bind_group(
                            &self.ctx,
                            matrix,
                            blend_t,
                            dash_scale,
                            globe_params,
                        );
                        self.raster_tiles.insert(
                            tile,
                            HeldRasterTile {
                                texture,
                                mesh_key,
                                model_buffer,
                                model_bind_group,
                                paint_bind_group,
                                _paint_buffer: paint_buffer,
                                paint_opacity: opacity,
                            },
                        );
                    }
                    raster_draw_order.push(tile);
                }
                // The crude cap: once over budget, keep only what THIS frame
                // actually drew. A `HashMap` has no insertion order to evict
                // by, so unlike `bake_some`'s `retain` (which spares a
                // recently-retained set across a frame of lag) this drops
                // everything outside the immediate draw list the moment the
                // cap is crossed -- acceptable for a page that had no
                // imagery cache at all before this task.
                if self.raster_tiles.len() > MAX_RASTER_TILES {
                    let keep: HashSet<TileId> = raster_draw_order.iter().copied().collect();
                    self.raster_tiles.retain(|t, _| keep.contains(t));
                }
            }
        }

        // The frame's buildings: decided by `render::frame_plan`, the one
        // definition every shell obeys (`MapView::paint` makes this exact
        // call), and written into the per-layer uniforms now, while `self`
        // is still mutable. Translucency on: a layer with opacity < 1 draws
        // a depth prepass then a stencilled colour pass (the stencil is
        // cleared once, when the tile pass opens).
        let extrusion_plan = {
            let camera = self.camera_control.camera();
            crate::render::frame_plan::extrusion_plan(
                &self.style.layers,
                camera.zoom,
                camera.center_lat,
                f64::from(camera.bearing),
                crate::render::frame_plan::ExtrusionOptions {
                    translucency: true,
                    // The fade before the building cut. The look stays
                    // MapLibre's here: `ExtrusionLook::ENHANCED` is native
                    // only for now, so no haze colour is needed.
                    depth: crate::render::frame_plan::extrusion_frame_depth(
                        camera,
                        self.last_building_reach.get(),
                        crate::style::Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 },
                    ),
                    ..Default::default()
                },
            )
        };
        if !extrusion_plan.layers.is_empty() {
            let params: Vec<_> = extrusion_plan.layers.iter().map(|l| l.params).collect();
            self.extrusion_params.write(&self.ctx, self.pipeline.extrusion(), &params);
        }

        // The flat list and the building list, selected in one pass over
        // the same draw order (`cover_plan::split_basemap_and_building_tiles`).
        // The shells do NOT clip stand-ins as `MapView` does (R21): a
        // stand-in draws flat only and carries no buildings, which appear
        // when the tile itself lands (Task 12 review, Ruling F1).
        let (flat_tiles, building_tiles) = crate::render::cover_plan::split_basemap_and_building_tiles(
            &plan.draws,
            &plan.cover,
            |t| self.gpu_tiles.get(&t),
        );
        let draws: Vec<(&TileBuffers, &wgpu::BindGroup)> =
            flat_tiles.iter().map(|g| (&g.buffers, &g.model_bind_group)).collect();
        let extrusion_tiles: Vec<(&crate::render::extrusion_gpu::ExtrusionGpu, &wgpu::BindGroup)> = building_tiles
            .iter()
            .filter_map(|g| g.extrusion.as_ref().map(|gpu| (gpu, &g.model_bind_group)))
            .collect();

        // WHAT IS DRAWN BEHIND THE TILES -- and, for the atmosphere, in
        // front of them. All three were decided above, where `&mut self` was
        // still available; this only reads them, so it travels beside the
        // `draws` list it is submitted with. The three `if`s are the same
        // gates the blocks above used, deliberately: delete one there and
        // the other stops drawing too.
        let space = crate::render::SpaceDraw {
            stars: if space_plan.draws_stars { self.star_pipeline.as_ref() } else { None },
            sky: if space_plan.draws_sky { self.sky_pipeline.as_ref() } else { None },
            backdrop: if space_plan.draws_backdrop {
                self.backdrop.as_ref().map(|held| (&held.buffers, &held.model_bind_group))
            } else {
                None
            },
            atmosphere: if space_plan.draws_atmosphere {
                self.atmosphere_pipeline.as_ref()
            } else {
                None
            },
            // The space look's sky first and its globe dressing after the
            // tiles (`render::gpu` keeps that order), gated as natively.
            look: if draws_look { self.space_look_pipeline.as_ref() } else { None },
        };

        // WHAT IS DRAWN UNDER THE TILES: the imagery, in the order the
        // upload loop above decided. Assembled through `iter()` rather than
        // a lookup, the same reason native's `held_raster` is: a lookup
        // would need `&mut self`, unreachable once `draws` above has taken
        // its immutable borrows. `raster_squares.get` can legitimately miss
        // (a tile evicted by the cap right above) -- `filter_map` drops it,
        // same as native drops a tile its own byte budget evicted mid-loop.
        let raster_squares = &self.raster_squares;
        let held_raster = &self.raster_tiles;
        let raster_draws_vec: Vec<(&RasterBuffers, &wgpu::BindGroup, &wgpu::BindGroup)> =
            raster_draw_order
                .iter()
                .filter_map(|tile| {
                    let held = held_raster.get(tile)?;
                    let square = raster_squares.get(&held.mesh_key)?;
                    Some((square, &held.model_bind_group, &held.paint_bind_group))
                })
                .collect();
        let raster = self
            .raster_pipeline
            .as_ref()
            .filter(|_| !raster_draws_vec.is_empty())
            .map(|pipeline| RasterDraws { pipeline, draws: &raster_draws_vec, after_tile_draws: 0 });

        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t)
            | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            other => {
                log(&format!("get_current_texture: {other:?}"));
                return;
            }
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor {
            format: Some(self.render_format),
            ..Default::default()
        });
        let mut encoder = self
            .ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("map") });
        // `render_with_space_and_raster`, not `render_with_space`: the
        // imagery draws between the ocean sphere and the vector tiles, in
        // the pass they already share. `raster` is `None` on every frame
        // with no imagery, and that arm issues no command at all -- see
        // `render::gpu::TilePipeline::render_with_space_and_raster`'s doc
        // comment for why it exists beside the old entry point rather than
        // replacing it, and native's `MapView::paint` for the identical
        // call.
        // The engine's tile pass takes `TileDraw`s: the basemap's 88-byte
        // tiles, then the caller overlays (compact fills, hairlines,
        // instanced circles -- `ShellOverlays::push_draws`, native's order),
        // then the route line, last, over every layer -- native's order.
        let mut draws: Vec<crate::render::TileDraw<'_>> = draws.iter().map(|&draw| draw.into()).collect();
        // Sources over the buildings (every source, on a frame that draws
        // any, unless one was kept under them) go in a second list, drawn
        // in its own pass after the building draws below.
        let mut overlays_over_buildings = Vec::new();
        self.overlays.push_draws_split(&mut draws, &mut overlays_over_buildings, !extrusion_plan.steps.is_empty());
        // Population dots over the overlays, under the route and the labels:
        // native's order (`MapView::paint`).
        self.dots_gpu.push_draws(globe_frame.transition == 0.0, &mut draws);
        if let Some(draw) = self.nav_route.as_ref().and_then(|route| route.draw()) {
            draws.push(draw.into());
        }
        // The buildings, after EVERY other push -- basemap, overlays, dots,
        // route -- so their walls cover what lies behind them while the
        // label pass below still draws over them: native's order.
        if !extrusion_plan.steps.is_empty() {
            crate::render::extrusion_gpu::push_extrusion_draws(
                &extrusion_plan,
                &extrusion_tiles,
                &self.extrusion_params,
                &mut draws,
            );
        }
        self.pipeline.render_with_space_and_raster(
            &mut encoder,
            &view,
            &self.depth_view,
            clear_color,
            &self.camera_bind_group,
            &space,
            raster,
            &draws,
            // `timestamp_writes`. `None`, and it is not a stub: the engine
            // gained this so a native demo could put GPU pass timings on
            // stderr, and it is driven by `wgpu::Features::TIMESTAMP_QUERY`,
            // which no browser exposes on either backend. Asking for a query
            // set the device does not have is a validation error, not a
            // degraded reading -- so the browser's answer to "how long did
            // the pass take" is the frame rate in the corner, which is what
            // `map_fps` reports.
            None,
        );
        // Overlay sources lifted over the buildings, in their own pass with
        // the extrusions' depth cleared: native's `MapView::paint` order.
        // Opens nothing when there are none.
        self.pipeline.render_overlay_above_buildings(
            &mut encoder,
            &view,
            &self.depth_view,
            &self.camera_bind_group,
            &overlays_over_buildings,
        );
        let drawn = draws.len() + overlays_over_buildings.len();
        drop(overlays_over_buildings);
        // `draws` holds immutable borrows of `self`, and `draw_labels`
        // below takes `&mut self`. (`space` borrows too, but it has no
        // `Drop`, so its borrow already ends at its last use.)
        drop(draws);

        // ---- the symbol pass: labels, on top of the tiles just drawn -----
        //
        // A SECOND render pass on the same encoder, `LoadOp::Load` so it
        // draws over the basemap, and no depth attachment -- overlap between
        // labels is resolved BEFORE anything is drawn, by the collision
        // index, so nothing that reaches the pass can overlap anything else
        // in it. This is `MapView::paint`'s structure exactly, and every
        // decision inside it is `symbol_pass`'s, shared with that file.
        let tile_ms = now_ms() - draw_started;
        self.draw_labels(&mut encoder, &view, plan, &globe_frame, symbol_light, fog);
        // The navigation chrome, in its own pass over everything. No scene,
        // no pass, and no `NavChromeGpu` is ever created without a trip.
        if self.nav.is_active() {
            let frame = nav_frame_of(self.camera_control.camera());
            let scene = self.nav.scene(self.camera_control.camera(), &frame);
            if !scene.is_empty() {
                let chrome = self
                    .nav_chrome
                    .get_or_insert_with(|| crate::render::nav_layer::NavChromeGpu::new(&self.ctx, self.render_format));
                chrome.render(
                    &self.ctx,
                    &mut encoder,
                    &view,
                    &scene,
                    self.labels.as_mut().map(|labels| &mut labels.font_system),
                    (self.surface_config.width, self.surface_config.height),
                    self.dpr,
                );
            }
        }
        let symbol_ms = now_ms() - draw_started - tile_ms;

        let submit_started = now_ms();
        self.ctx.queue.submit(std::iter::once(encoder.finish()));
        self.ctx.queue.present(frame);
        let submit_ms = now_ms() - submit_started;

        // The frame's cost, split. Pushed unconditionally -- an instrument
        // that only records while something is watching cannot answer a
        // question asked afterwards, and 24 bytes a frame is affordable in a
        // way that a `String` a frame would not be.
        let sample = FrameSample {
            interval_ms: self.last_interval_ms,
            cpu_ms: (now_ms() - draw_started) as f32,
            tile_ms: tile_ms as f32,
            symbol_ms: symbol_ms as f32,
            icon_ms: self.icons.as_ref().map_or(0.0, |icons| icons.draw_ms as f32),
            submit_ms: submit_ms as f32,
            drawn: drawn.min(u16::MAX as usize) as u16,
            icons: self.labels.as_ref().map_or(0, |l| l.census.icons.min(u16::MAX as usize) as u16),
            labels: self.labels.as_ref().map_or(0, |l| l.census.labels.min(u16::MAX as usize) as u16),
        };
        if self.frame_samples.len() >= MAX_FRAME_SAMPLES {
            self.frame_samples.pop_front();
            self.frame_samples_dropped += 1;
        }
        self.frame_samples.push_back(sample);

        if !self.first_draw_logged && drawn > 0 {
            self.first_draw_logged = true;
            self.startup.mark_once("first triangle");
            // FOUR numbers, not one, because one was actively misleading.
            //
            // The old line reported `now - started_at` and CALLED it "page
            // load", but `started_at` is `run`'s first instruction -- so the
            // module fetch, compile and instantiate were silently outside the
            // measurement. The absolute `performance.now()` is the real
            // page-load figure and is reported first.
            //
            // The frame count and the visibility flag are what turn an
            // anomalous reading into a diagnosis. A hidden or occluded tab
            // gets `requestAnimationFrame` at a few hertz, and since the
            // first tile request and every bake are driven from that
            // callback, such a tab reports seconds here having done nothing
            // wrong. Ten seconds over 19 frames and ten seconds over 600 are
            // two completely different bugs; without the count they are the
            // same number.
            let hidden = web_sys::window()
                .and_then(|w| w.document())
                .map(|d| d.hidden())
                .unwrap_or(false);
            log(&format!(
                "page load -> first triangle: {:.0} ms ({:.0} ms after wasm entry)                  over {} frames, document {}",
                now_ms(),
                now_ms() - self.started_at,
                self.metrics.frames,
                if hidden { "HIDDEN (rAF is throttled -- this number is not a renderer cost)" } else { "visible" },
            ));
        }
        // Two readouts, deliberately, from one set of counters.
        //
        // `#readout` is the one a person reads: where the camera is, how much
        // is on screen, and whether the frame rate is holding. `#status` is
        // the measurement HUD this spike was built around -- every retention
        // counter, every arm -- and it is behind the debug panel because a
        // page that opens on four lines of `holes_ancestor_unbaked` is a
        // instrument, not a map. Neither is derived from the other: the debug
        // line keeps its exact previous format so a measurement taken before
        // this change is still comparable with one taken after.
        let fps = self.fps();
        set_text(
            "readout",
            &format!(
                "z {:.2}   {:.4}, {:.4}   {} tiles   {}",
                self.camera_control.camera().zoom,
                self.camera_control.camera().center_lat,
                self.camera_control.camera().center_lon,
                drawn,
                if fps > 0.0 { format!("{fps:.0} fps") } else { "-- fps".to_string() },
            ),
        );
        status(&format!(
            "z {:.2} (tiles z{})  |  {:.4}, {:.4}  |  {} drawn, {} decoded, \
             {} in flight, {} empty\n\
             mode {:?}  |  cover {}: {} fresh, {} stale, {} desc, {} anc, {} HOLE  \
             |  totals: {} holes over {} frames ({} stale mesh, {} never loaded, {} unbaked)  \
             |  requests {}+{} coarse  |  bakes {}+{} coarse  \
             |  queue {} pending {} running {} arrived, stale {} dropped {} discarded",
            self.camera_control.camera().zoom,
            crate::render::cover_plan::zoom_span_label(plan.cover_zooms, self.tile_zoom()),
            self.camera_control.camera().center_lat,
            self.camera_control.camera().center_lon,
            drawn,
            self.decoded.len(),
            self.inflight.len(),
            self.missing.len(),
            self.mode,
            plan.cover_len,
            plan.fresh,
            plan.stale,
            plan.descendant_stand_ins,
            plan.ancestor_stand_ins,
            plan.holes,
            self.metrics.holes,
            self.metrics.frames,
            self.metrics.holes_stale_mesh,
            self.metrics.holes_never_loaded,
            self.metrics.holes_ancestor_unbaked,
            self.metrics.requests_cover,
            self.metrics.requests_prefetch,
            self.metrics.bakes_cover,
            self.metrics.bakes_prefetch,
            self.fetch_queue.pending_len(),
            self.fetch_queue.running_len(),
            self.arrived.len(),
            self.fetch_queue.stale_dropped_total(),
            self.stale_results_dropped,
        ));
    }

    /// Mirrors the camera's bearing onto the DOM compass (`#compass` in
    /// `index.html`): hidden at north, needle turned by
    /// `compass::needle_rotation_rad` otherwise. Only when the bearing
    /// changed -- a style write per frame on a still map is layout work for
    /// nothing.
    /// Shows the SWAT mark (`#swatmark`, bottom-left above the credit) in
    /// the ink `draw` chose, writing `src` only when the ink changes. The
    /// same two vendored PNGs native draws, as data URIs.
    fn sync_brand_mark_dom(&mut self) {
        let Some(variant) = self.brand_variant else { return };
        if self.brand_shown == Some(variant) {
            return;
        }
        let Some(img) = web_sys::window().and_then(|w| w.document()).and_then(|d| d.get_element_by_id("swatmark"))
        else {
            return;
        };
        use base64::Engine as _;
        let src = format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(variant.png()));
        let _ = img.set_attribute("src", &src);
        let _ = img.remove_attribute("hidden");
        self.brand_shown = Some(variant);
    }

    fn sync_compass_dom(&mut self) {
        let bearing = self.camera_control.camera().bearing;
        if self.compass_shown_bearing == Some(bearing) {
            return;
        }
        self.compass_shown_bearing = Some(bearing);
        let Some(button) = web_sys::window().and_then(|w| w.document()).and_then(|d| d.get_element_by_id("compass"))
        else {
            return;
        };
        if crate::compass::compass_visible(bearing) {
            let _ = button.remove_attribute("hidden");
        } else {
            let _ = button.set_attribute("hidden", "");
        }
        if let Ok(button) = button.dyn_into::<web_sys::HtmlElement>() {
            let _ = button.style().set_property("--needle", &format!("{}rad", crate::compass::needle_rotation_rad(bearing)));
        }
    }
}

// ---------------------------------------------------------------------------
// The measurement surface.
//
// A handle to the live `MapApp`, so a scripted zoom sweep can drive the camera
// on the map's own terms and read the counters back without a screenshot in
// the loop. Everything below is instrumentation; none of it is on the drawing
// path.
// ---------------------------------------------------------------------------

thread_local! {
    static APP: RefCell<Option<Rc<RefCell<MapApp>>>> = const { RefCell::new(None) };
}

/// The dot layer's id on this page (one source at a time).
const DOT_LAYER: &str = "pop";

/// Where the page's own server proxies ClickHouse (`web/serve.py`).
const CLICKHOUSE_PROXY: &str = "./clickhouse";

/// `?grid=<archive>` or `?clickhouse=<table>` (with `?dot_epoch=` and
/// `?dot_budget=`): opens the dot source and adds it as [`DOT_LAYER`].
/// Every decision about the query string and the replies is in
/// `overlay::host::shell_fetch` (tested there; this crate has no runner).
///
/// - `?grid=./grid/E2025_30ss.pmtiles`: a `dotgrid` PMTiles archive, read
///   by `Range` -- `PmtilesOpener`'s two steps (the header and root
///   directory in one read, then the metadata and leaf directories), then
///   one range per count tile. `serve.py` serves `/grid/` with `206`s.
/// - `?clickhouse=ghs_cells_30ss`: `gpui_map_dots.<table>` through
///   `serve.py`'s `/clickhouse/` proxy, by parameters (never SQL).
async fn load_dot_source(app: Rc<RefCell<MapApp>>) {
    use crate::overlay::host::shell_fetch::{self, DotSourceParam};
    use crate::overlay::host::{GridCountSource, Platform};
    let param = match shell_fetch::dot_source_param(query_param("grid").as_deref(), query_param("clickhouse").as_deref()) {
        Ok(Some(param)) => param,
        Ok(None) => return,
        Err(why) => {
            log(&format!("dots: {why}"));
            status(&format!("dots: {why}"));
            return;
        }
    };
    app.borrow_mut().dots_asked_ms = Some(now_ms());
    let opened: Result<std::sync::Arc<dyn GridCountSource>, crate::overlay::host::GridError> = match &param {
        DotSourceParam::Pmtiles(url) => open_dot_archive(url).await.map(|s| std::sync::Arc::new(s) as _),
        DotSourceParam::ClickHouse { table } => open_dot_proxy(table).await.map(|s| std::sync::Arc::new(s) as _),
    };
    let source = match opened {
        Ok(source) => source,
        Err(err) => {
            log(&format!("dots: could not open {param:?}: {err}"));
            status(&format!("dots: {err}"));
            return;
        }
    };
    let descriptor = source.descriptor().clone();
    let Some(epoch) = shell_fetch::pick_epoch(&descriptor.epochs, num_param("dot_epoch")) else {
        log("dots: the source has no epoch");
        return;
    };
    // A race archive (`?grid=./grid/race-dots-2020.pmtiles`, race plan
    // MG-R15) draws Walker's categorical dots; anything else the density
    // ramp. `?dot_budget=` overrides either budget.
    let race = source.race().is_some();
    let asked_budget = num_param("dot_budget");
    let style = shell_fetch::shell_dot_style(
        Platform::Web,
        race,
        epoch,
        asked_budget.map(|b| shell_fetch::web_dot_budget(Some(b))),
    );
    log(&format!(
        "dots: {param:?} open at {:.0} ms: {}{}″ grid, {} levels, epochs {:?}, drawing {epoch}, budget {} dots",
        now_ms(),
        if race { "race archive, " } else { "" },
        descriptor.base_arcsec,
        descriptor.levels,
        descriptor.epochs,
        style.budget_dots
    ));
    {
        let mut a = app.borrow_mut();
        a.dots.set_source(DOT_LAYER, source, style);
        a.needs_redraw = true;
    }
    for credit in app.borrow().dots.attributions() {
        add_credit(&credit);
    }
}

/// Opens a `dotgrid` PMTiles archive by `Range` (`fetch_native::open_pmtiles_grid`'s
/// browser twin: the same `PmtilesOpener` steps, fetched with [`fetch_grid`]).
async fn open_dot_archive(url: &str) -> Result<crate::overlay::host::PmtilesGridSource, crate::overlay::host::GridError> {
    let mut opener = crate::overlay::host::PmtilesOpener::new(url);
    let first = fetch_grid(&opener.first_request()).await?;
    let requests = opener.after_first(&first)?;
    let mut bodies = Vec::with_capacity(requests.len());
    for request in &requests {
        bodies.push(fetch_grid(request).await?);
    }
    opener.finish(&bodies)
}

/// Reads the proxy's descriptor for `table` and makes its source.
async fn open_dot_proxy(table: &str) -> Result<crate::overlay::host::ClickHouseProxySource, crate::overlay::host::GridError> {
    use crate::overlay::host::{ClickHouseProxySource, GridRequest};
    let reply = fetch_grid(&GridRequest::Get { url: ClickHouseProxySource::descriptor_url(CLICKHOUSE_PROXY, table) }).await?;
    ClickHouseProxySource::from_descriptor_reply(CLICKHOUSE_PROXY, table, &reply)
}

/// Executes one count-tile request with `fetch`: a byte range (a `Range`
/// header, `206` required -- anything else is aborted before its body is
/// read) or the proxy's whole-body GET (with `shell_fetch::PROXY_HEADER`).
/// Every rule is `shell_fetch`'s: native's deadline
/// (`REQUEST_TIMEOUT_MS`, after which the fetch is aborted and lands as
/// `shell_fetch::timed_out`, a source error that frees its slot and backs
/// off), and a body bounded as it streams in (`BodyCap`).
async fn fetch_grid(req: &crate::overlay::host::GridRequest) -> Result<Vec<u8>, crate::overlay::host::GridError> {
    use crate::overlay::host::shell_fetch;
    use crate::overlay::host::{GridError, GridRequest};
    let Some((url, headers)) = shell_fetch::browser_fetch(req) else {
        return match req {
            GridRequest::Absent => Ok(Vec::new()),
            _ => Err(GridError::Server("a browser never sends SQL; use the /clickhouse/ proxy".to_string())),
        };
    };
    let window = web_sys::window().ok_or_else(|| GridError::Server("no window".to_string()))?;
    let abort = web_sys::AbortController::new()
        .map_err(|_| GridError::Server("fetch failed: no AbortController".to_string()))?;
    // The deadline: past it, abort, and whatever step was waiting fails
    // as `timed_out` -- never the URL in the text (it may carry a token).
    let timed_out = Rc::new(std::cell::Cell::new(false));
    // `Closure::once`, held here across the await and dropped only after
    // `clear_timeout`: the common path (the fetch lands first) never calls
    // it, and `once_into_js` would leak it and its captures on every fetch.
    // Dropping it is safe on both paths: cleared, the browser can no longer
    // call it; fired, it already ran (to completion, in its own task,
    // before this future resumed) and the drop frees what is left.
    let fire = {
        let (abort, timed_out) = (abort.clone(), timed_out.clone());
        Closure::once(move || {
            timed_out.set(true);
            abort.abort();
        })
    };
    let timer = window
        .set_timeout_with_callback_and_timeout_and_arguments_0(
            fire.as_ref().unchecked_ref(),
            shell_fetch::REQUEST_TIMEOUT_MS as i32,
        )
        .ok();
    let result = fetch_grid_within(&window, req, &url, &headers, &abort, &timed_out).await;
    if let Some(handle) = timer {
        window.clear_timeout_with_handle(handle);
    }
    drop(fire);
    result
}

async fn fetch_grid_within(
    window: &web_sys::Window,
    req: &crate::overlay::host::GridRequest,
    url: &str,
    headers: &[(String, String)],
    abort: &web_sys::AbortController,
    timed_out: &Rc<std::cell::Cell<bool>>,
) -> Result<Vec<u8>, crate::overlay::host::GridError> {
    use crate::overlay::host::shell_fetch::{self, BodyCap, ReplyStatus};
    use crate::overlay::host::{GridError, GridRequest};
    let failed = |e: JsValue| {
        if timed_out.get() {
            shell_fetch::timed_out()
        } else {
            GridError::Server(format!("fetch failed: {}", e.as_string().unwrap_or_else(|| "a network error".into())))
        }
    };
    let init = web_sys::RequestInit::new();
    init.set_method("GET");
    init.set_signal(Some(&abort.signal()));
    let request_headers = web_sys::Headers::new().map_err(failed)?;
    for (name, value) in headers {
        request_headers.set(name, value).map_err(failed)?;
    }
    init.set_headers(&request_headers);
    let request = web_sys::Request::new_with_str_and_init(url, &init).map_err(failed)?;
    let response: web_sys::Response = wasm_bindgen_futures::JsFuture::from(window.fetch_with_request(&request))
        .await
        .map_err(failed)?
        .dyn_into()
        .map_err(failed)?;
    let status = response.status();
    let reply_headers = response.headers();
    let code = reply_headers.get("x-clickhouse-exception-code").ok().flatten();
    let content_length = reply_headers.get("content-length").ok().flatten().and_then(|v| v.trim().parse::<u64>().ok());
    let is_range = matches!(req, GridRequest::Range { .. });
    let rule = if is_range { shell_fetch::range_status(status) } else { shell_fetch::get_status(status, code.as_deref()) };
    match rule {
        ReplyStatus::Refuse(err) => {
            abort.abort();
            Err(err)
        }
        ReplyStatus::ReadErrorText => {
            // Only a small, declared error body is read; a big one is not
            // streamed in to be cut to 512 bytes.
            let text = if shell_fetch::read_error_text(content_length) {
                match response.text() {
                    Ok(p) => wasm_bindgen_futures::JsFuture::from(p).await.ok().and_then(|t| t.as_string()).unwrap_or_default(),
                    Err(_) => String::new(),
                }
            } else {
                abort.abort();
                String::new()
            };
            Err(shell_fetch::error_reply(status, code.as_deref(), &text))
        }
        ReplyStatus::ReadBody => {
            let mut cap = BodyCap::for_request(req);
            if let Err(err) = cap.declared(content_length) {
                abort.abort();
                return Err(err);
            }
            let Some(stream) = response.body() else { return Ok(Vec::new()) };
            let reader: web_sys::ReadableStreamDefaultReader = stream.get_reader().dyn_into().map_err(|o: js_sys::Object| failed(o.into()))?;
            let mut body = Vec::new();
            loop {
                let chunk = wasm_bindgen_futures::JsFuture::from(reader.read()).await.map_err(failed)?;
                let done = js_sys::Reflect::get(&chunk, &JsValue::from_str("done")).ok().and_then(|d| d.as_bool()).unwrap_or(true);
                if done {
                    break;
                }
                let value = js_sys::Reflect::get(&chunk, &JsValue::from_str("value")).map_err(failed)?;
                let bytes = js_sys::Uint8Array::new(&value);
                if let Err(err) = cap.take(bytes.length() as usize) {
                    abort.abort();
                    return Err(err);
                }
                let at = body.len();
                body.resize(at + bytes.length() as usize, 0);
                bytes.copy_to(&mut body[at..]);
            }
            match req {
                GridRequest::Range { length, .. } => shell_fetch::range_body(body, *length),
                _ => shell_fetch::get_body(body),
            }
        }
    }
}

/// Hands out every dot job the layers have now (native's
/// `MapView::drive_dot_sources`; `DotLayers` caps the requests in flight).
/// A fetch runs as a future; its decode and block build run when it lands,
/// on this thread (the page has no worker). A rebuild has no IO and runs
/// inline. Called from the frame loop with no borrow held.
fn drive_dot_jobs(app: &Rc<RefCell<MapApp>>) {
    let jobs: Vec<crate::overlay::host::DotJob> = {
        let Ok(mut a) = app.try_borrow_mut() else { return };
        if a.dots.is_empty() {
            return;
        }
        std::iter::from_fn(|| a.dots.next_job()).collect()
    };
    for job in jobs {
        match job.request() {
            None => {
                let landed = job.run(|_| Ok(Vec::new()));
                let mut a = app.borrow_mut();
                if a.dots.land(landed) {
                    a.needs_redraw = true;
                }
            }
            Some(request) => {
                let app = app.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    let fetched = fetch_grid(&request).await;
                    let landed = job.run(move |_| fetched);
                    let mut a = app.borrow_mut();
                    if a.dots.land(landed) {
                        a.needs_redraw = true;
                    }
                });
            }
        }
    }
}

/// The dot layer's counters, for MG-24's harness: `null` with no layer.
/// `first_dots_ms` / `asked_ms` are ms since navigation; `stale_dropped`
/// counts results for a replaced source.
#[wasm_bindgen]
pub fn map_dot_stats_json() -> String {
    with_app(|a| {
        let Some(s) = a.dots.stats(DOT_LAYER) else { return "null".to_string() };
        let budget = a.dots.style(DOT_LAYER).map_or(0, |st| st.budget_dots);
        let opt = |v: Option<f64>| v.map_or("null".to_string(), |v| format!("{v:.1}"));
        format!(
            "{{\"dots_drawn\":{},\"budget\":{budget},\"ppd\":{},\"level\":{},\"tiles_ready\":{},\"tiles_wanted\":{},\
             \"standin_tiles\":{},\"tiles_drawn\":{},\"gpu_bytes\":{},\"source_errors\":{},\"last_error\":{},\
             \"asked_ms\":{},\"first_dots_ms\":{},\"tiles_held\":{},\"stale_dropped\":{}}}",
            s.dots_drawn,
            s.ppd,
            s.level,
            s.tiles_ready,
            s.tiles_wanted,
            s.standin_tiles,
            s.tiles_drawn,
            s.gpu_bytes,
            s.source_errors,
            s.last_error.map_or("null".to_string(), |e| format!("\"{e}\"")),
            opt(a.dots_asked_ms),
            opt(a.dots_first_ms),
            a.dots.tiles_held(DOT_LAYER),
            a.dots.stale_dropped(),
        )
    })
    .unwrap_or_else(|| "null".into())
}

/// `?overlay=` and `?points=`: the caller-overlay demo.
///
/// - `?overlay=sample` draws the engine's seeded sample polygons
///   (`overlay::host::sample_blocks_geojson`, the same data the phone's demo
///   draws) around the start centre; `?overlay=<url>` fetches a GeoJSON
///   FeatureCollection into source `overlay`, painted by `?overlay_paint=`
///   (a style-spec paint object as JSON) when given.
/// - `?points=<n>` adds `n` seeded points around the centre as a clustered
///   source.
async fn load_overlay_demo(app: Rc<RefCell<MapApp>>) {
    let overlay = query_param("overlay");
    let points = query_param("points").and_then(|n| n.parse::<usize>().ok()).unwrap_or(0);
    if overlay.is_none() && points == 0 {
        return;
    }
    let (lon, lat, zoom) = {
        let a = app.borrow();
        let camera = a.camera_control.camera();
        (camera.center_lon, camera.center_lat, a.display_zoom())
    };
    match overlay.as_deref() {
        None => {}
        Some("sample") => {
            let mut a = app.borrow_mut();
            let result = a.overlays.load_demo(lon, lat, 48, 32, points, zoom);
            log(&format!("overlay: sample polygons + {points} points around ({lon:.4}, {lat:.4}): {result:?}"));
            a.needs_redraw = true;
            return;
        }
        Some(url) => match fetch_bytes(url).await {
            Ok(Some(bytes)) => {
                let text = String::from_utf8_lossy(&bytes);
                let mut a = app.borrow_mut();
                if let Some(paint) = query_param("overlay_paint")
                    .and_then(|p| serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&p).ok())
                {
                    if let Err(err) = a.overlays.declare_source_paint("overlay", &paint) {
                        log(&format!("overlay: paint refused: {err}"));
                    }
                }
                let result = a.overlays.set_source("overlay", &text, zoom);
                log(&format!("overlay: {} bytes from {url}: {result:?}", bytes.len()));
                a.needs_redraw = true;
            }
            Ok(None) => log(&format!("overlay: {url} returned 404")),
            Err(err) => log(&format!("overlay: fetch failed: {err:?}")),
        },
    }
    if points > 0 {
        let mut a = app.borrow_mut();
        let result = a
            .overlays
            .declare_source_paint(crate::overlay::host::DEMO_POINT_SOURCE, &crate::overlay::host::demo_cluster_paint())
            .and_then(|()| {
                a.overlays.set_points(
                    crate::overlay::host::DEMO_POINT_SOURCE,
                    &crate::overlay::host::sample_points(lon, lat, points, 0.3, 11),
                    &crate::overlay::host::FeaturePaint {
                        circle_radius: 5.0,
                        circle_color: crate::style::Rgba { r: 0.11, g: 0.42, b: 0.78, a: 0.9 },
                        ..crate::overlay::host::FeaturePaint::default()
                    },
                    crate::overlay::host::ClusterOptions::default(),
                )
            });
        log(&format!("overlay: {points} clustered points: {result:?}"));
        a.needs_redraw = true;
    }
}

/// Labels over data, from JS: `true` while the page draws data fills under
/// the basemap labels, so every label takes the engine's on-data paint
/// (`style::LabelPaint::ON_DATA`: dark text in an opaque light halo)
/// instead of the style's own; `false` for the style's own again. A host
/// reading a Studio document decides with [`map_labels_on_data_for_doc`],
/// the rule every shell shares.
#[wasm_bindgen]
pub fn map_set_labels_on_data(on: bool) {
    with_app(|a| {
        let paint = on.then_some(crate::style::LabelPaint::ON_DATA);
        if a.label_paint != paint {
            a.label_paint = paint;
            a.needs_redraw = true;
        }
    });
}

/// Whether the Studio document `doc_json` (a `.mapdoc.json` body) draws
/// data fills under the labels: `swat_maps_doc::dashboard::labels_on_data`,
/// the rule desktop and phone apply too. `false` for a body that does not
/// parse as a document.
#[wasm_bindgen]
pub fn map_labels_on_data_for_doc(doc_json: String) -> bool {
    serde_json::from_str::<swat_maps_doc::doc::Doc>(&doc_json).is_ok_and(|doc| swat_maps_doc::dashboard::labels_on_data(&doc))
}

/// Adds (or replaces) a GeoJSON FeatureCollection as overlay source `name`,
/// from JS: `map_add_geojson("parcels", text, '{"fill-color": "#e11d48"}')`.
/// `paint_json` is a style-spec paint object (empty for the features' own
/// paint). Polygons and lines draw through the tile index.
#[wasm_bindgen]
pub fn map_add_geojson(name: String, geojson: String, paint_json: String) -> Result<(), JsValue> {
    with_app(|a| {
        if !paint_json.trim().is_empty() {
            let paint: serde_json::Map<String, serde_json::Value> =
                serde_json::from_str(&paint_json).map_err(|e| JsValue::from_str(&format!("paint: {e}")))?;
            a.overlays.declare_source_paint(&name, &paint).map_err(|e| JsValue::from_str(&e.to_string()))?;
        }
        let zoom = a.display_zoom();
        a.overlays.set_source(&name, &geojson, zoom).map_err(|e| JsValue::from_str(&e.to_string()))?;
        a.needs_redraw = true;
        Ok(())
    })
    .unwrap_or_else(|| Err(JsValue::from_str("map not ready")))
}

/// Adds (or replaces) overlay source `name` from CSV text, from JS:
/// `map_add_csv("stations", text, '{"circle-color": "#e11d48", "circle-radius": 6}')`.
/// The geometry is a lat/lon column pair (or a WKT / GeoJSON geometry
/// column), found by `overlay::csv`'s header rules. Points take the point
/// path and draw as circles -- which a GeoJSON source through
/// [`map_add_geojson`] does not, as that path tiles lines and polygons --
/// and anything else the tiled path. `paint_json` as [`map_add_geojson`]:
/// a style-spec paint object whose colours may be expressions over the
/// rows' columns.
#[wasm_bindgen]
pub fn map_add_csv(name: String, csv: String, paint_json: String) -> Result<(), JsValue> {
    let source = crate::overlay::host::CsvSource::from_bytes(csv.as_bytes(), &crate::overlay::host::CsvOptions::default())
        .map_err(|e| JsValue::from_str(&format!("csv: {e}")))?;
    with_app(|a| add_csv_source(a, &name, &source, &paint_json)).unwrap_or_else(|| Err(JsValue::from_str("map not ready")))
}

/// [`map_add_csv`] from a URL the browser fetches (CORS applies; the
/// overlay CSV reader's size cap and timeout too). Resolves once the
/// source is in.
#[wasm_bindgen]
pub async fn map_load_source_csv(name: String, url: String, paint_json: String) -> Result<(), JsValue> {
    let source = crate::overlay::host::CsvSource::from_url(&url, &crate::overlay::host::CsvOptions::default())
        .await
        .map_err(|e| JsValue::from_str(&format!("csv: {e}")))?;
    with_app(|a| add_csv_source(a, &name, &source, &paint_json)).unwrap_or_else(|| Err(JsValue::from_str("map not ready")))
}

/// Adds (or replaces) overlay source `name` from a FlatGeobuf URL the
/// browser fetches with `Range` requests (CORS must allow `Range`): the
/// whole file in this version; a viewport-streamed variant follows. Points
/// take the point path, lines and polygons the tiled one. `paint_json` as
/// [`map_add_geojson`].
#[wasm_bindgen]
pub async fn map_load_source_fgb(name: String, url: String, paint_json: String) -> Result<(), JsValue> {
    let source = crate::overlay::host::FgbSource::from_url(&url, &crate::overlay::host::FgbOptions::default())
        .await
        .map_err(|e| JsValue::from_str(&format!("fgb: {e}")))?;
    with_app(|a| {
        if !paint_json.trim().is_empty() {
            let paint: serde_json::Map<String, serde_json::Value> =
                serde_json::from_str(&paint_json).map_err(|e| JsValue::from_str(&format!("paint: {e}")))?;
            a.overlays.declare_source_paint(&name, &paint).map_err(|e| JsValue::from_str(&e.to_string()))?;
        }
        let zoom = a.display_zoom();
        a.overlays
            .set_source_fgb(&name, &source, &crate::overlay::host::FeaturePaint::default(), None, zoom)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        a.needs_redraw = true;
        Ok(())
    })
    .unwrap_or_else(|| Err(JsValue::from_str("map not ready")))
}

/// Adds (or replaces) overlay source `name` from a GeoParquet URL the
/// browser reads with `Range` requests (CORS must allow `Range`): the
/// footer, then the column chunks of every row group, so the whole file in
/// this version. A zstd-compressed file is refused (this shell links no C).
/// Points take the point path, lines and polygons the tiled one, as
/// [`map_load_source_fgb`]; `paint_json` as [`map_add_geojson`].
#[cfg(feature = "geoparquet")]
#[wasm_bindgen]
pub async fn map_load_source_geoparquet(name: String, url: String, paint_json: String) -> Result<(), JsValue> {
    let source = crate::overlay::host::GeoParquetSource::from_url(
        &url,
        &crate::overlay::host::GeoParquetOptions::default(),
    )
    .await
    .map_err(|e| JsValue::from_str(&format!("geoparquet: {e}")))?;
    with_app(|a| {
        if !paint_json.trim().is_empty() {
            let paint: serde_json::Map<String, serde_json::Value> =
                serde_json::from_str(&paint_json).map_err(|e| JsValue::from_str(&format!("paint: {e}")))?;
            a.overlays.declare_source_paint(&name, &paint).map_err(|e| JsValue::from_str(&e.to_string()))?;
        }
        let zoom = a.display_zoom();
        a.overlays
            .set_source_geoparquet(&name, &source, &crate::overlay::host::FeaturePaint::default(), None, zoom)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        a.needs_redraw = true;
        Ok(())
    })
    .unwrap_or_else(|| Err(JsValue::from_str("map not ready")))
}

fn add_csv_source(a: &mut MapApp, name: &str, source: &crate::overlay::host::CsvSource, paint_json: &str) -> Result<(), JsValue> {
    if !paint_json.trim().is_empty() {
        let paint: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(paint_json).map_err(|e| JsValue::from_str(&format!("paint: {e}")))?;
        a.overlays.declare_source_paint(name, &paint).map_err(|e| JsValue::from_str(&e.to_string()))?;
    }
    let zoom = a.display_zoom();
    a.overlays
        .set_source_csv(name, source, &crate::overlay::host::FeaturePaint::default(), None, zoom)
        .map_err(|e| JsValue::from_str(&e.to_string()))?;
    a.needs_redraw = true;
    Ok(())
}

/// Removes overlay source `name`. `true` if there was one.
#[wasm_bindgen]
pub fn map_remove_source(name: String) -> bool {
    with_app(|a| {
        a.needs_redraw = true;
        a.overlays.remove_source(&name)
    })
    .unwrap_or(false)
}

/// `{"sources": [...], "tile_draws": n, "circle_layers": n}`: what the
/// overlays drew in the last frame, for a capture script to check.
#[wasm_bindgen]
pub fn map_overlay_stats_json() -> String {
    with_app(|a| {
        let (tile_draws, circle_layers) = a.overlays.stats();
        let names: Vec<String> = a.overlays.source_names().iter().map(|n| n.to_string()).collect();
        serde_json::json!({"sources": names, "tile_draws": tile_draws, "circle_layers": circle_layers}).to_string()
    })
    .unwrap_or_default()
}

/// Runs `f` against the live map, or returns `None` if there is no map yet
/// or the map is already borrowed.
///
/// `try_borrow_mut`, NOT `borrow_mut`, and the difference is the whole point
/// of this comment. Every caller below is an instrument -- a counter read, a
/// camera nudge from a button, a scripted probe -- and a `RefCell` that is
/// already borrowed unwinds as a wasm `unreachable`, which does not raise a
/// catchable JS exception: it kills the frame loop, and the map freezes for
/// good one call after some tool asked it a question.
///
/// That happened twice while this page was being measured. Once for a real
/// re-entrancy bug of this crate's own (`log_full_cover` logging a memory
/// line through `with_app` while the frame loop held the borrow -- fixed at
/// the source, see [`memory_json`]). Once from OUTSIDE: a CDP
/// `Runtime.evaluate` calling [`map_fps`] landed while an input handler was
/// on the stack, which nothing in this crate can prevent and nothing in this
/// crate did wrong.
///
/// An instrument that can stop the thing it measures is not an instrument.
/// A refused read returns `None` and the caller reports a zero or a
/// no-op; the count of refusals is on `BORROW_REFUSED` so this degrades
/// visibly rather than silently.
fn with_app<R>(f: impl FnOnce(&mut MapApp) -> R) -> Option<R> {
    // The `Rc` is CLONED out of the slot rather than borrowed through it:
    // `slot.borrow()`'s guard would have to outlive the `try_borrow_mut`
    // guard taken from inside it, which it cannot. One refcount bump.
    let app = APP.with(|slot| slot.borrow().clone())?;
    // Bound to a `let` rather than returned straight out of the `match`: a
    // tail-position `match` keeps its scrutinee's temporary -- the
    // `RefMut` -- alive past the end of `app`, which does not borrow-check.
    let out = match app.try_borrow_mut() {
        Ok(mut a) => Some(f(&mut a)),
        Err(_) => {
            BORROW_REFUSED.with(|n| n.set(n.get() + 1));
            None
        }
    };
    out
}

thread_local! {
    /// How many [`with_app`] calls were refused because the map was already
    /// borrowed. Reported by [`map_metrics_json`]; a non-zero value means
    /// some instrument asked at a moment it could not be answered, not that
    /// the map is broken.
    static BORROW_REFUSED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// The accumulated counters, as JSON. Read by the sweep driver.
/// How many instrument reads were refused because the map was mid-borrow.
/// See [`with_app`].
#[wasm_bindgen]
pub fn map_borrow_refused() -> u32 {
    BORROW_REFUSED.with(|n| n.get())
}

thread_local! {
    /// Whether the draw path records the text of every label it draws. See
    /// [`Labels::texts`]; latched on by [`map_labels_json`].
    static LABEL_TEXT_DUMP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The last frame's label census, and -- from the frame after the first
/// call -- the text of every label drawn.
///
/// This is the number a browser-vs-native comparison is made on. Both
/// builds run the same `symbol_pass`, so at the same camera and the same
/// viewport `features`, `candidates`, `placed` and `labels` should agree,
/// and the `texts` lists should be the same strings. Where they do NOT
/// agree, the documented reason left on `draw_labels` is overlay features,
/// which this page has none of.
///
/// The OTHER documented reason used to be that this page fetched no sprite
/// sheet, so an icon collision box that would evict a label natively was
/// absent here. That is no longer true -- see [`load_sprites`] -- and it is
/// worth naming, because it means a label census taken before icons landed
/// is NOT comparable with one taken after: a shield now competes for space
/// and some labels lose. Compare like for like, or compare against
/// `?sprite=none`.
///
/// `"null"` when the font has not landed yet, which is distinct from a
/// census of zeroes (font present, nothing on screen to label).
#[wasm_bindgen]
pub fn map_labels_json() -> String {
    LABEL_TEXT_DUMP.with(|f| f.set(true));
    with_app(|a| {
        let Some(labels) = a.labels.as_ref() else { return "null".to_string() };
        let c = labels.census;
        let texts: Vec<String> =
            labels.texts.iter().map(|t| serde_json::Value::String(t.clone()).to_string()).collect();
        format!(
            "{{\"features\":{},\"overlay_symbols\":{},\"candidates\":{},\"placed\":{},\"labels\":{},\
             \"text_areas\":{},\"shaped_total\":{},\"pass_ms\":{:.2},\"pass_ms_max\":{:.2},\
             \"pass_frames\":{},\"pass_ms_mean\":{:.3},\"families\":{},\"texts\":[{}]}}",
            c.features,
            c.overlay_symbols,
            c.candidates,
            c.placed,
            c.labels,
            c.text_areas,
            c.shaped_total,
            c.pass_ms,
            c.pass_ms_max,
            c.pass_frames,
            if c.pass_frames > 0 { c.pass_ms_sum / c.pass_frames as f64 } else { 0.0 },
            serde_json::Value::Array(
                labels.families.iter().cloned().map(serde_json::Value::String).collect()
            ),
            texts.join(","),
        )
    })
    .unwrap_or_else(|| "null".into())
}

/// The last frame's icon census, and -- from the frame after the first call
/// -- the sprite NAME behind every quad drawn, in draw order, plus every
/// name the visible features asked for that the sheet does not carry.
///
/// This is the shield-specific half of the comparison [`map_labels_json`]
/// makes for text. "The shields render" is not a claim a screenshot can
/// settle on its own: a blank box and the wrong badge look equally fine at
/// z12 on a laptop panel. The `names` list settles it -- at a Tokyo camera
/// it must contain `jp-national-route-*`, `jp-expressway-*` and
/// `jp-prefectural-road-*`, in the proportions the tile data's `network`
/// values imply -- and `missing` catches the other failure mode, a style
/// expression resolving to a sprite this sheet never had.
///
/// `"null"` when no sheet has landed, which is distinct from a census of
/// zeroes (sheet present, nothing icon-bearing on screen).
#[wasm_bindgen]
pub fn map_icons_json() -> String {
    ICON_NAME_DUMP.with(|f| f.set(true));
    with_app(|a| {
        let Some(icons) = a.icons.as_ref() else { return "null".to_string() };
        let json_names = |names: &[String]| {
            serde_json::Value::Array(names.iter().cloned().map(serde_json::Value::String).collect())
                .to_string()
        };
        format!(
            "{{\"sheet_entries\":{},\"sheet_bytes\":{},\"atlas_w\":{},\"atlas_h\":{},\
             \"packed\":{},\"drawn\":{},\"draw_ms\":{:.3},\"draw_ms_max\":{:.3},\
             \"draw_frames\":{},\"draw_ms_mean\":{:.4},\"names\":{},\"missing\":{}}}",
            icons.sheet_entries,
            icons.sheet_bytes,
            icons.atlas_size.0,
            icons.atlas_size.1,
            icons.rects.len(),
            icons.drawn,
            icons.draw_ms,
            icons.draw_ms_max,
            icons.draw_frames,
            if icons.draw_frames > 0 { icons.draw_ms_sum / icons.draw_frames as f64 } else { 0.0 },
            json_names(&icons.names),
            json_names(&icons.missing),
        )
    })
    .unwrap_or_else(|| "null".into())
}

/// Every drawn frame's per-pass CPU cost since the last call, as one JSON
/// object of parallel arrays, and the ring is CLEARED by reading it.
///
/// Arrays rather than an array of objects because the whole point is to hand
/// a few thousand samples to a percentile function without paying for a few
/// thousand JSON objects to do it, and because a `Float32Array`-shaped
/// answer is what the driver actually wants.
///
/// `frames` is the map's OWN drawn-frame counter, reported alongside, and it
/// is the guard against the trap this instrument exists because of: a page
/// whose renderer has stopped still fires `requestAnimationFrame`, so a
/// flawless `interval_ms` distribution with a `frames` that did not move is
/// a dead map, not a fast one. `dropped` is non-zero if the run was longer
/// than the ring.
#[wasm_bindgen]
pub fn map_frame_samples_json() -> String {
    with_app(|a| {
        let n = a.frame_samples.len();
        let mut interval = String::with_capacity(n * 6);
        let (mut cpu, mut tile, mut symbol, mut icon, mut submit) =
            (String::new(), String::new(), String::new(), String::new(), String::new());
        let (mut drawn, mut icons, mut labels) = (String::new(), String::new(), String::new());
        for (i, s) in a.frame_samples.iter().enumerate() {
            let sep = if i == 0 { "" } else { "," };
            interval.push_str(&format!("{sep}{:.2}", s.interval_ms));
            cpu.push_str(&format!("{sep}{:.3}", s.cpu_ms));
            tile.push_str(&format!("{sep}{:.3}", s.tile_ms));
            symbol.push_str(&format!("{sep}{:.3}", s.symbol_ms));
            icon.push_str(&format!("{sep}{:.3}", s.icon_ms));
            submit.push_str(&format!("{sep}{:.3}", s.submit_ms));
            drawn.push_str(&format!("{sep}{}", s.drawn));
            icons.push_str(&format!("{sep}{}", s.icons));
            labels.push_str(&format!("{sep}{}", s.labels));
        }
        let dropped = a.frame_samples_dropped;
        let frames = a.metrics.frames;
        a.frame_samples.clear();
        a.frame_samples_dropped = 0;
        format!(
            "{{\"n\":{n},\"dropped\":{dropped},\"frames\":{frames},\"interval_ms\":[{interval}],\
             \"cpu_ms\":[{cpu}],\"tile_ms\":[{tile}],\"symbol_ms\":[{symbol}],\
             \"icon_ms\":[{icon}],\"submit_ms\":[{submit}],\"drawn\":[{drawn}],\
             \"icons\":[{icons}],\"labels\":[{labels}]}}"
        )
    })
    .unwrap_or_else(|| "null".into())
}

#[wasm_bindgen]
pub fn map_metrics_json() -> String {
    with_app(|a| a.metrics.to_json(a.mode)).unwrap_or_else(|| "null".into())
}

/// Zeroes the counters. Called at the top of a sweep, after the initial cover
/// has settled, so the page-load fill is not charged to the sweep.
#[wasm_bindgen]
pub fn map_metrics_reset() {
    with_app(|a| a.metrics = Metrics::default());
}

/// Switches retention behaviour without a reload. `?mode=N` does the same at
/// startup, and is what the A/B arms use -- a reload also resets the tile
/// state, which a mid-run switch does not.
#[wasm_bindgen]
pub fn map_set_mode(mode: u32) {
    with_app(|a| {
        a.mode = Mode::from_index(mode);
        a.needs_redraw = true;
    });
}

/// Places the camera exactly, with no anchoring and no easing -- so a scripted
/// sweep is deterministic in what it LOADS, not merely in where it ends up.
#[wasm_bindgen]
pub fn map_set_camera(lon: f64, lat: f64, zoom: f64) {
    with_app(|a| {
        a.camera_control.jump_to(CameraTarget { center: Some((lon, lat)), zoom: Some(zoom), ..CameraTarget::default() });
        a.pan_inertia = None;
        a.needs_redraw = true;
    });
}

/// `map_set_camera` with pitch and bearing, in DEGREES. No easing. A
/// missing (NaN) or non-finite argument keeps that field's current value.
#[wasm_bindgen]
pub fn map_jump_to(lon: f64, lat: f64, zoom: f64, pitch_deg: f64, bearing_deg: f64) {
    with_app(|a| {
        a.camera_control.jump_to(CameraTarget {
            center: Some((lon, lat)),
            zoom: Some(zoom),
            pitch: crate::camera_control::radians_from_degrees(pitch_deg),
            bearing: crate::camera_control::radians_from_degrees(bearing_deg),
            ..CameraTarget::default()
        });
        a.pan_inertia = None;
        a.needs_redraw = true;
    });
}

/// MapLibre's `easeTo`, in degrees and milliseconds. Any input cancels it.
/// A missing argument arrives as NaN: the controller ignores a non-finite
/// field, and a NaN, negative, infinite or over-long duration is a jump.
#[wasm_bindgen]
pub fn map_ease_to(lon: f64, lat: f64, zoom: f64, pitch_deg: f64, bearing_deg: f64, duration_ms: f64) {
    with_app(|a| {
        a.camera_control.ease_to(
            CameraTarget {
                center: Some((lon, lat)),
                zoom: Some(zoom),
                pitch: crate::camera_control::radians_from_degrees(pitch_deg),
                bearing: crate::camera_control::radians_from_degrees(bearing_deg),
                ..CameraTarget::default()
            },
            crate::camera_control::ease_duration_from_ms(duration_ms),
        );
        a.pan_inertia = None;
        a.needs_redraw = true;
    });
}

/// The compass button's action (Task 10).
#[wasm_bindgen]
pub fn map_reset_north() {
    with_app(|a| {
        a.camera_control.apply(CameraIntent::ResetNorth);
        a.needs_redraw = true;
    });
}

/// The glide currently in flight, or `null`. Instrumentation: it lets a
/// scripted flick check the offset and duration MapLibre's formulas produced
/// against the same numbers worked by hand, instead of eyeballing the motion.
#[wasm_bindgen]
pub fn map_pan_inertia_json() -> String {
    with_app(|a| {
        a.pan_inertia
            .map(|i| {
                format!(
                    "{{\"offset_x\":{},\"offset_y\":{},\"duration_ms\":{},\
                     \"applied_x\":{},\"applied_y\":{}}}",
                    i.offset_x, i.offset_y, i.duration_ms, i.applied_x, i.applied_y
                )
            })
            .unwrap_or_else(|| "null".into())
    })
    .unwrap_or_else(|| "null".into())
}

/// The camera, as JSON. Instrumentation.
/// What this page is holding, in bytes.
///
/// Three numbers, because "how much memory" has three different answers here
/// and reporting one of them alone is how a browser build gets accused of a
/// leak it does not have:
///
/// * `wasm_heap_bytes` -- the wasm linear memory, i.e. every Rust allocation
///   this crate has made, read straight from `memory.size()`. It is the
///   closest thing to native's RSS, and note that it only ever GROWS:
///   `memory.grow` has no counterpart, so freeing a decoded tile returns the
///   bytes to Rust's allocator and NOT to the browser. A flat number here
///   after a long pan means the allocator is reusing, which is the good
///   outcome and not a stuck reading.
/// * `gpu_buffer_bytes_live` -- vertex and index buffers currently held,
///   counted by the engine's own `TrackedBuffer`, which is the same counter
///   the native memory work reads. This is NOT in the wasm heap; it is on
///   the GPU, and `MAX_GPU_TILES` is the thing that bounds it.
/// * `js_heap_bytes` -- V8's own heap, via the non-standard
///   `performance.memory`. Reported as 0 where the browser does not expose
///   it. It is small here and is included only so the three add up to
///   something a task manager can be compared against.
///
/// `rss_bytes()` is deliberately not among them: it is `0` on any target
/// that is not macOS, and a zero that looks like a measurement is worse than
/// an absent one.
#[wasm_bindgen]
pub fn map_memory_json() -> String {
    let (decoded, gpu_tiles, raster) = with_app(|a| (a.decoded.len(), a.gpu_tiles.len(), a.raster_counts()))
        .unwrap_or((0, 0, RasterCounts::default()));
    memory_json(decoded, gpu_tiles, raster)
}

/// [`map_memory_json`] with the two `MapApp` counts passed IN rather than
/// borrowed.
///
/// The split is not cosmetic. `log_full_cover` runs inside the frame loop's
/// `app.borrow_mut()`, so a memory line logged from there through
/// `with_app` re-enters the same `RefCell` and panics -- `already
/// borrowed`, unwound as a wasm `unreachable`, which kills the frame loop
/// dead one frame after the map finished loading. It did exactly that, once.
/// Anything called from inside the frame loop takes this entry point.
fn memory_json(decoded: usize, gpu_tiles: usize, raster: RasterCounts) -> String {
    let wasm_heap = core::arch::wasm32::memory_size(0) as u64 * 65536;
    let stats = crate::render::allocation_stats();
    let js_heap = web_sys::window()
        .and_then(|w| js_sys::Reflect::get(&w, &JsValue::from_str("performance")).ok())
        .and_then(|p| js_sys::Reflect::get(&p, &JsValue::from_str("memory")).ok())
        .and_then(|m| js_sys::Reflect::get(&m, &JsValue::from_str("usedJSHeapSize")).ok())
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0) as u64;
    format!(
        "{{\"wasm_heap_bytes\":{wasm_heap},\"gpu_buffer_bytes_live\":{},\
         \"gpu_buffers_live\":{},\"js_heap_bytes\":{js_heap},\
         \"decoded_tiles\":{decoded},\"gpu_tiles\":{gpu_tiles},\
         \"mesh_installs_total\":{},\"draw_calls_last_frame\":{},\
         \"raster_decoded_tiles\":{},\"raster_decoded_bytes\":{},\
         \"raster_gpu_tiles\":{},\"absent_tiles\":{}}}",
        stats.gpu_buffer_bytes_live,
        stats.gpu_buffers_live,
        stats.mesh_installs_total,
        stats.draw_calls_last_frame,
        raster.decoded_tiles,
        raster.decoded_bytes,
        raster.gpu_tiles,
        raster.absent_tiles,
    )
}

/// The imagery and absence caches' occupancy, for [`memory_json`]: what a
/// long-pan measurement reads to see that each is bounded.
#[derive(Clone, Copy, Default)]
struct RasterCounts {
    decoded_tiles: usize,
    decoded_bytes: usize,
    gpu_tiles: usize,
    absent_tiles: usize,
}

/// Replaces the live style, without a reload and without refetching a
/// 4.5 MB wasm module.
///
/// The alternative -- putting the style in the query string and reloading --
/// costs a module fetch, a compile, an adapter request and a full tile
/// cover, i.e. about a second and a quarter, to change some colours. This
/// costs one 64 KB fetch and a re-bake, and it keeps the camera exactly
/// where the user left it, which is the whole reason to compare two styles
/// in the first place.
///
/// The re-bake is [`REBAKE_SENTINEL`]: `bake_some` is already the code that
/// re-tessellates a stale mesh, so the swap does not add a second baking
/// path. Meshes outside the cover keep the OLD style until they next enter
/// it, so a stand-in can briefly be drawn in the previous palette during the
/// changeover.
#[wasm_bindgen]
pub async fn map_set_style(url: String) -> Result<(), JsValue> {
    let bytes = fetch_bytes(&url).await?.ok_or_else(|| JsValue::from_str("style 404"))?;
    // A style of the other schema needs the other TILES, and everything
    // decoded, baked and in flight belongs to these. Rather than a second
    // teardown path, reload with `?style=` and the camera: startup already
    // pairs a style with its source (`crate::basemap::tile_template_for_style`,
    // which also decides what a `?tiles=` is the source for). Two styles of
    // one schema -- all a tokenless page offers -- swap in place as before.
    if crate::basemap::style_schema(&bytes) != TILE_SCHEMA.with(|s| s.get()) {
        return reload_with_style(&url);
    }
    let compiled = crate::style_spec::parse_and_compile(&bytes)
        .map_err(|e| JsValue::from_str(&format!("style compile failed: {e:#}")))?;
    let background = style_background(&compiled);
    let layers = compiled.layers.len();
    // `Option::take` because `with_app` takes an `FnOnce` this closure cannot
    // prove it is -- moving a non-`Copy` value into it directly does not
    // borrow-check.
    let mut compiled = Some(compiled);
    with_app(|a| {
        a.style = Rc::new(compiled.take().expect("with_app runs its closure once"));
        a.background = background;
        for gpu in a.gpu_tiles.values_mut() {
            gpu.baked_zoom = REBAKE_SENTINEL;
        }
        a.needs_redraw = true;
    })
    .ok_or_else(|| JsValue::from_str("no map"))?;
    log(&format!("style swapped: {url} ({layers} layers, {} B)", bytes.len()));
    Ok(())
}

/// Reloads the page on `style`, keeping the camera and every other query
/// parameter. See [`map_set_style`] for why a reload.
fn reload_with_style(style: &str) -> Result<(), JsValue> {
    let window = web_sys::window().ok_or_else(|| JsValue::from_str("no window"))?;
    let camera = with_app(|a| {
        let c = a.camera_control.camera();
        (c.center_lon, c.center_lat, c.zoom, c.pitch.to_degrees(), c.bearing.to_degrees())
    });
    let replaced = ["style", "lon", "lat", "z", "pitch", "bearing"];
    let search = window.location().search().unwrap_or_default();
    let mut params: Vec<String> = search
        .trim_start_matches('?')
        .split('&')
        .filter(|kv| !kv.is_empty())
        .filter(|kv| !replaced.iter().any(|name| kv.split('=').next() == Some(*name)))
        .map(str::to_string)
        .collect();
    params.push(format!("style={}", String::from(js_sys::encode_uri_component(style))));
    if let Some((lon, lat, zoom, pitch, bearing)) = camera {
        params.push(format!("lon={lon:.6}&lat={lat:.6}&z={zoom:.3}&pitch={pitch:.1}&bearing={bearing:.1}"));
    }
    log(&format!("style {style} speaks the other tile schema -- reloading on it"));
    window.location().set_search(&format!("?{}", params.join("&")))
}

/// With a Mapbox token, the style picker offers the real Mapbox Streets
/// first. The open look-alike is already labelled "Streets (open)"
/// in the shipped markup, so this only reaffirms it (in case a future
/// default ever changes the static label back); without a token the
/// markup is left exactly as shipped.
fn offer_mapbox_streets() {
    let Some(document) = web_sys::window().and_then(|w| w.document()) else { return };
    let Some(pick) = document.get_element_by_id("stylepick") else { return };
    if let Ok(Some(imitation)) = pick.query_selector(&format!("option[value=\"{STYLE_URL}\"]")) {
        imitation.set_text_content(Some("Streets (open)"));
    }
    let Ok(option) = document.create_element("option") else { return };
    let _ = option.set_attribute("value", MAPBOX_STYLE_URL);
    option.set_text_content(Some("Mapbox Streets"));
    let first = pick.first_child();
    let _ = pick.insert_before(&option, first.as_ref());
}

/// Shows `url` as the picker's current choice, when it is one of them.
fn select_style_option(url: &str) {
    if let Some(pick) = web_sys::window().and_then(|w| w.document()).and_then(|d| d.get_element_by_id("stylepick")) {
        let _ = js_sys::Reflect::set(&pick, &JsValue::from_str("value"), &JsValue::from_str(url));
    }
}

/// Frames presented in the last second. `0` before a second has elapsed.
///
/// Exported so a scripted pan can assert on the frame rate instead of a
/// human watching the corner of the screen -- which matters more here than
/// it would natively, because a hidden or occluded tab throttles
/// `requestAnimationFrame` to a couple of hertz and would otherwise look
/// exactly like a renderer that had fallen over.
#[wasm_bindgen]
pub fn map_fps() -> f64 {
    with_app(|a| a.fps()).unwrap_or(0.0)
}

/// The startup timeline as `label +delta (absolute)` -- the same string the
/// log prints, reachable without scraping the log element.
#[wasm_bindgen]
pub fn map_startup_report() -> String {
    with_app(|a| a.startup.report()).unwrap_or_default()
}

#[wasm_bindgen]
pub fn map_camera_json() -> String {
    with_app(|a| {
        let c = a.camera_control.camera();
        format!(
            "{{\"lon\":{},\"lat\":{},\"zoom\":{},\"pitch\":{},\"bearing\":{}}}",
            c.center_lon,
            c.center_lat,
            c.zoom,
            c.pitch.to_degrees(),
            c.bearing.to_degrees()
        )
    })
    .unwrap_or_else(|| "null".into())
}

/// Stops the frame loop where it stands, so a screenshot shows the frame the
/// script chose rather than whatever the map had caught up to by the time the
/// capture landed. A frozen page keeps its last presented surface contents.
#[wasm_bindgen]
pub fn map_freeze(frozen: bool) {
    with_app(|a| {
        a.frozen = frozen;
        a.needs_redraw = true;
    });
}

/// True once every cover tile is drawing its own mesh at the current zoom
/// bucket -- i.e. the map has fully settled. The sweep waits on this rather
/// than on a wall-clock timeout, so a slow network lengthens the run instead of
/// silently corrupting it.
#[wasm_bindgen]
pub fn map_is_settled() -> bool {
    with_app(|a| {
        let plan = a.plan_frame();
        plan.fresh as usize == plan.cover_len
    })
    .unwrap_or(false)
}

#[wasm_bindgen(start)]
pub fn main() {
    console_error_panic_hook::set_once();
    wasm_bindgen_futures::spawn_local(async {
        if let Err(err) = run().await {
            log(&format!("FAILED: {err:?}"));
            // On the boot card as well as in the console. The card is what
            // is in front of the user at the moment a startup failure
            // happens, and "loading the engine..." forever is the single
            // least informative thing this page can say -- it is exactly the
            // symptom a throttled background tab produces, which is how one
            // was misdiagnosed as a dead renderer before.
            boot_stage(&format!("{err:?}"));
        }
    });
}

async fn run() -> Result<(), JsValue> {
    let t0 = now_ms();
    let mut startup = Startup::default();
    // The first mark's delta is measured from zero, so it reports the ONE
    // stage this crate cannot instrument from the inside: the browser
    // fetching, compiling and instantiating the module.
    startup.mark("wasm ready");
    boot_stage("fetching the style");
    let window = web_sys::window().ok_or_else(|| JsValue::from_str("no window"))?;
    let document = window.document().ok_or_else(|| JsValue::from_str("no document"))?;

    // ---- 1. The style, over the network. ---------------------------------
    // `?tiles=` and `?style=` are read before anything is fetched, so the
    // whole page -- style, tiles, and the endpoint the chrome reports -- is
    // repointable without a rebuild.
    //
    // The style and the tile source are chosen TOGETHER, by
    // `crate::basemap` -- the pairing iOS makes: with a Mapbox token,
    // Mapbox Streets over the `composite` source; without one, exactly
    // what this page did before.
    let token = mapbox_token();
    let query_tiles = query_param("tiles");
    let query_style = query_param("style");
    // `?census=`: the census demo's own pale basemap (Mapbox Light with a
    // token, Positron without) unless `?style=`/`?tiles=` chose one.
    let census_page = census_page::census_param().is_some();
    let query_style = query_style.or_else(|| {
        census_page
            .then(|| crate::census::census_style_url(None, query_tiles.as_deref(), token.is_some()))
            .flatten()
            .map(str::to_string)
    });
    if token.is_some() {
        offer_mapbox_streets();
    }
    let (style_url, style_bytes) = {
        let mut url =
            crate::basemap::startup_style_url(token.as_deref(), query_style.as_deref(), STYLE_URL, MAPBOX_STYLE_URL);
        let mut bytes = fetch_bytes(&url).await?;
        // A token with no Mapbox style beside it: a tokenless dist given
        // `?access_token=`. Fall back to the page's own default rather than
        // fail to start.
        if bytes.is_none() && url == MAPBOX_STYLE_URL && query_style.is_none() {
            log(&format!("{MAPBOX_STYLE_URL} is not in this dist -- falling back to {STYLE_URL}"));
            url = STYLE_URL.to_string();
            bytes = fetch_bytes(&url).await?;
        }
        let bytes = bytes.ok_or_else(|| JsValue::from_str("style 404"))?;
        // The census demo darkens and haloes the basemap's place names over
        // its ramp, as native does, before the style is compiled.
        (url, if census_page { crate::census::census_style_bytes(&bytes) } else { bytes })
    };
    let (schema, template) = match crate::basemap::tile_template_for_style(
        &style_bytes,
        token.as_deref(),
        query_tiles.as_deref(),
        TILE_URL,
    ) {
        Ok(pair) => pair,
        Err(err) => {
            // A Mapbox-schema style with neither a token nor `?tiles=`:
            // draw it over the default tiles as this page always did, and
            // say why it looks empty.
            log(&format!("{style_url}: {err:#} -- drawing it over {TILE_URL}, which it will not match"));
            (crate::basemap::TileSchema::Omt, TILE_URL.to_string())
        }
    };
    log(&format!(
        "basemap: {style_url} over {} ({:?}, source ceiling z{}){}",
        template,
        schema,
        crate::source_zoom::max_zoom_for_tile_url(&template),
        match (token.is_some(), BAKED_MAPBOX_ACCESS_TOKEN.is_some()) {
            (false, _) if !MAPBOX_ALLOWED => " -- Mapbox is turned off in this build (GPUI_MAP_ALLOW_MAPBOX=1 at build time turns it on)",
            (false, _) => " -- no Mapbox token in this build",
            (true, true) => " -- Mapbox token baked in at build time",
            (true, false) => " -- Mapbox token from ?access_token=",
        }
    ));
    TILE_TEMPLATE.with(|t| *t.borrow_mut() = template);
    TILE_SCHEMA.with(|s| s.set(schema));
    select_style_option(&style_url);
    startup.mark("style fetched");
    let style = crate::style_spec::parse_and_compile(&style_bytes)
        .map_err(|e| JsValue::from_str(&format!("style compile failed: {e:#}")))?;
    startup.mark("style compiled");
    boot_stage("starting the GPU");
    log(&format!(
        "style: {} B fetched, {} layers compiled in {:.0} ms",
        style_bytes.len(),
        style.layers.len(),
        now_ms() - t0
    ));
    // Resolved from the SAME bytes just compiled above, re-read as raw JSON
    // for the `"sources"` object `style::Style` throws away -- see
    // `resolve_raster_config`'s own doc comment. Never logs `template`
    // itself: a `?imagery=` override can carry a consumer's own token.
    let raster_config = resolve_raster_config(&style, &style_bytes);
    log(&format!(
        "imagery: {}",
        if raster_config.template.is_empty() {
            "none (no ?imagery= override and the style declares no raster source)".to_string()
        } else {
            format!(
                "enabled, {}px tiles, source ceiling z{}",
                raster_config.tile_size, raster_config.max_zoom
            )
        }
    ));

    // ---- 2. The canvas. --------------------------------------------------
    let canvas: web_sys::HtmlCanvasElement = document
        .get_element_by_id("map")
        .ok_or_else(|| JsValue::from_str("no #map canvas"))?
        .dyn_into()?;

    // SIZE THE BACKING STORE IN PHYSICAL PIXELS, THE CAMERA IN CSS PIXELS.
    //
    // A `<canvas>` whose `width`/`height` attributes are left at their CSS
    // size renders at 1x and is then upscaled by the compositor to the
    // display's device ratio -- on a 2x Retina panel a half-resolution image
    // stretched to full size, which is exactly what "not very sharp" looks
    // like. The backing store must be `css_size * devicePixelRatio`.
    //
    // The CAMERA must NOT follow it. `Camera`'s viewport is in the same units
    // as `zoom` (512 logical px per tile at z0) and as `Vertex::half_width_px`,
    // so feeding it physical pixels would show half the ground at twice the
    // scale and leave every line the wrong thickness. Same split MapLibre
    // makes: CSS pixels for the transform, `devicePixelRatio` for the
    // framebuffer -- and, here, for the shader's antialiasing band, which is
    // the only reason `device_pixel_ratio` is in the camera uniform at all.
    // There is no MSAA to lean on instead: every pipeline in `gpu.rs` is
    // `sample_count: 1`, natively too, and stroke edges are feathered
    // analytically in `shader.wgsl`.
    let dpr = (window.device_pixel_ratio() as f32).clamp(0.5, 8.0);
    let css_width = canvas.client_width().max(1) as f32;
    let css_height = canvas.client_height().max(1) as f32;
    // The backing store itself is sized further down, once the device's
    // `max_texture_dimension_2d` is known -- see `backing_store_size`.

    // ---- 3. The GPU. -----------------------------------------------------
    // `?gl` forces the WebGL2 path so the fallback can be exercised on a
    // browser that HAS WebGPU. Without it there is no way to find out whether
    // the fallback works until you meet a browser that needs it.
    let search = window.location().search().unwrap_or_default();
    let force_gl = search.contains("gl");
    // `?mode=N` picks the retention arm at startup. Deliberately a query
    // parameter and not a rebuild: the same wasm binary serves all four arms,
    // so nothing about the build can be the difference between them.
    let mode = search
        .split(['?', '&'])
        .find_map(|kv| kv.strip_prefix("mode="))
        .and_then(|v| v.parse::<u32>().ok())
        .map(Mode::from_index)
        .unwrap_or(Mode::Prefetch);
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: if force_gl { wgpu::Backends::GL } else { wgpu::Backends::all() },
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let surface = instance
        .create_surface(wgpu::SurfaceTarget::Canvas(canvas.clone()))
        .map_err(|e| JsValue::from_str(&format!("create_surface: {e}")))?;
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            ..Default::default()
        })
        .await
        .map_err(|e| JsValue::from_str(&format!("no adapter: {e}")))?;
    startup.mark("adapter");
    let info = adapter.get_info();

    // `GpuContext::new` is the engine's own device setup, not a browser
    // variant of it -- and it is now the only branch there is. It derives its
    // `required_limits` from the adapter, so WebGL2's zero for every compute
    // limit is no longer measured against the WebGPU baseline, and
    // `unmet_shader_needs` refuses by name and by number if the backend is
    // short on a limit the shaders really consume. The
    // `downlevel_webgl2_defaults()` branch that used to stand here was that
    // same policy hand-rolled outside the engine, because the engine did not
    // yet have it.
    let ctx = GpuContext::new(&adapter)
        .await
        .map_err(|e| JsValue::from_str(&format!("device: {e:#}")))?;
    startup.mark("device");
    boot_stage("building the pipeline");

    let (width, height, backing_scale) = backing_store_size(
        css_width,
        css_height,
        dpr,
        ctx.device.limits().max_texture_dimension_2d,
    );
    canvas.set_width(width);
    canvas.set_height(height);

    let caps = surface.get_capabilities(&adapter);
    // THE ENGINE REQUIRES A NON-sRGB RENDER TARGET, AND BOTH BROWSER BACKENDS
    // CAN GIVE IT ONE -- BUT ONLY ONE OF THEM OFFERS IT FIRST.
    //
    // This block said the opposite until the engine moved underneath it, and
    // the inversion is worth stating because the symptom is subtle enough to
    // live in a build for a while. `shader.wgsl` used to decode every vertex
    // colour to linear and `clear_color` used to decode the background, both
    // on the premise that the target's automatic encode-on-write would put
    // it back; so the browser, whose canvas formats are UNORM, needed an sRGB
    // VIEW to supply that missing encode.
    //
    // `cdd7a79` removed both halves natively -- translucent layers were
    // compositing in LINEAR light, which is not the space Mapbox GL JS blends
    // in, so a half-transparent layer over the reference's ground came out a
    // different colour from the reference. The shader now writes the authored
    // sRGB value and the target stores it: no transfer function on either
    // side, and blending happens in sRGB where the reference does it.
    //
    // The sRGB view was then the only encode left in the chain, applied to a
    // value that was already encoded -- `gpu.rs`'s
    // `solid_fill_midtone_color_round_trips_without_a_stray_gamma_encode`
    // names this exact failure and the exact arithmetic. Measured on this
    // page before the fix: the `land` background, authored `#ece7e4`, read
    // back off the canvas as `#f6f4f3`, which is `srgb_encode` of those three
    // channels to the byte. Every midtone in the document was that much too
    // light, and the map read as washed out rather than as broken -- so
    // nothing shouted.
    //
    // What each backend offers, in order (Chrome 152, macOS):
    //
    //   WebGPU  [Bgra8Unorm, Rgba8Unorm, Rgba16Float]      -- head is UNORM
    //   WebGL2  [Rgba8UnormSrgb, Rgba8Unorm, Rgba16Float]  -- head is sRGB
    //
    // so `caps.formats[0]` is right on one backend and wrong on the other,
    // which is why this searches rather than indexes.
    let format = caps
        .formats
        .iter()
        .copied()
        .find(|f| !f.is_srgb())
        // No non-sRGB format at all: take the head and say so. The map will
        // be a gamma out, and a line in the log is a better outcome than a
        // panic on a backend nobody here has seen.
        .unwrap_or_else(|| {
            log("WARNING: no non-sRGB surface format offered; colours will be one encode too light");
            caps.formats[0]
        });
    let view_formats = vec![];
    // Held onto (in `MapApp::surface_config`) so `apply_resize` can change
    // `width`/`height` and nothing else.
    let surface_config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format,
        color_space: wgpu::SurfaceColorSpace::Auto,
        width,
        height,
        present_mode: wgpu::PresentMode::Fifo,
        desired_maximum_frame_latency: 2,
        alpha_mode: caps.alpha_modes[0],
        view_formats,
    };
    surface.configure(&ctx.device, &surface_config);
    log(&format!(
        "gpu: {:?} ({}) | canvas {css_width}x{css_height} CSS px at dpr {dpr} \
         -> {width}x{height} (x{backing_scale}) | {format:?} (no sRGB view: the \
         shader writes authored sRGB and the target stores it)",
        info.backend, info.name
    ));

    startup.mark("surface configured");

    // `TilePipeline::new` is a shader-module creation and one render
    // pipeline. On the WebGPU backend that hands `shader.wgsl` to the
    // browser's own compiler, which is not free and is not on any budget this
    // crate controls -- hence its own mark.
    let pipeline = TilePipeline::new(&ctx, format);
    startup.mark("pipeline");
    let depth_view = pipeline.create_depth_view(&ctx, width, height);
    let (camera_buffer, camera_bind_group) = pipeline.create_camera_bind_group(
        &ctx,
        glam::Mat4::IDENTITY.to_cols_array_2d(),
        0.0,
        backing_scale,
    );

    let background = style_background(&style);
    let census_camera = census_page.then(|| {
        query_param("camera").as_deref().and_then(crate::census::parse_camera).unwrap_or(crate::census::START)
    });

    let app = Rc::new(RefCell::new(MapApp {
        ctx,
        canvas: canvas.clone(),
        surface,
        surface_config,
        pipeline,
        depth_view,
        extrusion_params: crate::render::extrusion_gpu::ExtrusionParamsGpu::new(),
        render_format: format,
        camera_buffer,
        camera_bind_group,
        style: Rc::new(style),
        raster_config,
        last_cover_reach: std::cell::Cell::new(None),
            last_building_reach: std::cell::Cell::new(None),
        camera_control: {
            let mut control = CameraController::new(
                Camera {
                    // Tokyo Station, z14 -- see `DEFAULT_CENTER_LON` and
                    // `DEFAULT_ZOOM`. `?lon=`/`?lat=`/`?z=` move it, which is how a
                    // second region is inspected without a rebuild. A census
                    // page opens on the lower 48, or at `?camera=lon,lat,zoom`.
                    center_lon: crate::camera_control::wrap_lon(
                        census_camera.map(|c| c.0).or_else(|| num_param("lon")).unwrap_or(DEFAULT_CENTER_LON),
                    ),
                    center_lat: census_camera.map(|c| c.1).or_else(|| num_param("lat")).unwrap_or(DEFAULT_CENTER_LAT),
                    zoom: census_camera
                        .map(|c| c.2)
                        .or_else(|| num_param("z"))
                        .unwrap_or(DEFAULT_ZOOM)
                        .clamp(MIN_ZOOM, MAX_ZOOM),
                    pitch: 0.0,
                    bearing: 0.0,
                    viewport_width_px: css_width,
                    viewport_height_px: css_height,
                    fov_y: Camera::DEFAULT_FOV_Y,
                },
                MIN_ZOOM,
                MAX_ZOOM,
            );
            // `?pitch=` and `?bearing=`, in DEGREES, like the numbers a
            // person types: what a scripted or headless capture needs to
            // open a tilted, rotated frame without driving input.
            control.jump_to(CameraTarget {
                pitch: num_param("pitch").and_then(crate::camera_control::radians_from_degrees),
                bearing: num_param("bearing").and_then(crate::camera_control::radians_from_degrees),
                ..CameraTarget::default()
            });
            control
        },
        orbit: None,
        orbit_down: None,
        shift_scroll: None,
        browser_pinch: Default::default(),
        nav_outbox: Vec::new(),
        pinch: None,
        rotate_samples: RotateSamples::default(),
        last_advance_ms: None,
        dpr: backing_scale,
        dpr_seen: dpr,
        compass_shown_bearing: None,
        brand_variant: None,
        brand_shown: None,
        background,
        space: space_default(),
        // Both pipelines and the backdrop are built on the first frame that
        // needs them, not here: a page opened at z14 never reaches globe
        // range and should pay for neither.
        star_pipeline: None,
        backdrop: None,
        atmosphere_pipeline: None,
        sky_pipeline: None,
        space_look: space_look_default(),
        look_time: num_param("look_time"),
        space_textures: None,
        space_textures_generation: 0,
        space_look_pipeline: None,
        decoded: HashMap::new(),
        gpu_tiles: HashMap::new(),
        inflight: HashSet::new(),
        fetch_queue: crate::fetch_queue::FetchQueue::new(),
        arrived: Vec::new(),
        wanted: crate::fetch_queue::Wanted::new(),
        stale_results_dropped: 0,
        missing: crate::raster_retention::AbsentTiles::new(),
        retries: crate::fetch_policy::RetryTable::new(),
        raster_decoded: crate::raster_retention::DecodedImagery::new(),
        raster_tiles: HashMap::new(),
        raster_inflight: HashSet::new(),
        raster_missing: crate::raster_retention::AbsentTiles::new(),
        raster_retries: crate::fetch_policy::RetryTable::new(),
        raster_pipeline: None,
        raster_squares: HashMap::new(),
        // Filled in by `load_font` below, which is spawned rather than
        // awaited -- see [`Labels`].
        labels: None,
        text_glyph_budget: Default::default(),
        label_fade: Default::default(),
        fade_redraw_gate: Default::default(),
        fade_wake_at_s: None,
        nav: {
            // Live routing goes through `web/serve.py`'s `/swat-route/`,
            // which holds the key; the page never sees one.
            let mut host = crate::nav::host::NavHost::new();
            host.set_routing(Ok(crate::nav::host::RoutingConfig::keyless("./swat-route", None)));
            host
        },
        nav_route: None,
        overlays: crate::overlay::host::ShellOverlays::new(),
        census: None,
        dots: crate::overlay::host::DotLayers::new(),
        dots_gpu: crate::overlay::host::DotLayersGpu::default(),
        dots_asked_ms: None,
        dots_first_ms: None,
        nav_chrome: None,
        nav_brand_hidden: false,
        voice_gesture_seen: false,
        voice_no_synth_logged: false,
        voice_choice: None,
        voice_logged: None,
        // ...and by `load_sprites`, spawned beside it for the same reason.
        icons: None,
        mode,
        frozen: false,
        trace: query_param("trace").filter(|v| v == "1").map(|_| {
            LABEL_TEXT_DUMP.with(|f| f.set(true));
            HashSet::new()
        }),
        trace_frame: 0,
        metrics: Metrics::default(),
        retained: HashSet::new(),
        drag_from: None,
        drag_samples: std::collections::VecDeque::new(),
        pan_inertia: None,
        active_pointers: HashMap::new(),
        touch_tap_start: None,
        last_tap: None,
        // True from the start, not just after an event: the measurement above
        // happens before the log element below the canvas has any lines in
        // it, so the flex layout moves under us within the first few frames.
        // The first `apply_resize` re-measures and corrects it.
        resize_pending: true,
        needs_redraw: true,
        label_paint: None,
        label_cover: Default::default(),
        pump_running: false,
        frame_stamps: std::collections::VecDeque::new(),
        frame_samples: std::collections::VecDeque::new(),
        frame_samples_dropped: 0,
        last_interval_ms: 0.0,
        first_draw_logged: false,
        full_cover_logged: false,
        started_at: t0,
        startup,
    }));

    APP.with(|slot| *slot.borrow_mut() = Some(app.clone()));
    log(&format!("retention mode: {mode:?} (?mode=0|1|2|3 selects)"));

    app.borrow_mut().startup.mark("app built");
    boot_stage("requesting tiles");
    // The endpoint, on the page rather than only in the console: "where did
    // these pixels come from" is the first question anyone asks of a map,
    // and with `?tiles=` able to repoint the build it is a question the
    // markup cannot answer on its own.
    set_text("source", &tile_template());
    render_attribution(&tile_template());

    // Spawned, not awaited: the map draws its first triangle at ~400 ms and
    // the faces are ~1.4 MB. Awaiting them here would put the whole font on
    // the critical path to first paint for no gain -- there is no label to
    // draw before there is a tile under it. See `load_font`.
    wasm_bindgen_futures::spawn_local(load_font(app.clone(), format));
    // The sheet is ~100 KB against the fonts' 1.4 MB and lands well before
    // them, but it is spawned rather than awaited for the same reason: an
    // icon has nothing to sit on until a road is drawn. See `load_sprites`.
    wasm_bindgen_futures::spawn_local(load_sprites(app.clone(), format));
    // `?look=space`: the space look's two textures. Returns at once, having
    // touched nothing, on a page without it.
    wasm_bindgen_futures::spawn_local(load_space_textures(app.clone()));
    // `?nav=sim&route=<fixture>`: a simulated drive. Returns at once, having
    // touched nothing, on a page without `nav=`.
    wasm_bindgen_futures::spawn_local(load_nav_demo(app.clone()));
    // `?overlay=<url>|sample` and `?points=<n>`: caller overlays. Returns at
    // once, having touched nothing, on a page without either.
    wasm_bindgen_futures::spawn_local(load_overlay_demo(app.clone()));
    // `?census=<pack>`: the census income choropleth. Returns at once,
    // having touched nothing, on a page without it.
    wasm_bindgen_futures::spawn_local(census_page::load(Rc::clone(&app)));
    // `?grid=<archive>` or `?clickhouse=<table>`: population dots. Returns
    // at once, having touched nothing, on a page with neither.
    wasm_bindgen_futures::spawn_local(load_dot_source(app.clone()));

    install_input_handlers(&canvas, &app)?;
    install_resize_handlers(&canvas, &app)?;
    install_visibility_handler(&app)?;
    start_frame_loop(&app);
    // The load-time case the `visibilitychange` handler above cannot catch:
    // a tab that was ALREADY hidden when the page loaded never fires that
    // event, and is precisely the tab whose frame loop will not run.
    if document_hidden() {
        start_hidden_pump(&app);
    }
    Ok(())
}

/// The CSS box's two change signals, routed to one flag.
///
/// * a **`ResizeObserver` on the canvas** -- the canvas is a flex child, so
///   it also changes size when the log below it grows a line, which no
///   window-level event reports. It also fires once on `observe`, which is
///   what corrects the pre-layout measurement taken in `run`.
/// * **`window.onresize`** -- a belt for the observer, and the event that
///   actually arrives while a window edge is being dragged.
///
/// The THIRD case, a `devicePixelRatio` change at an unchanged CSS size --
/// dragging the window from a Retina panel to an external monitor -- has no
/// event here on purpose; see the frame loop.
fn install_resize_handlers(
    canvas: &web_sys::HtmlCanvasElement,
    app: &Rc<RefCell<MapApp>>,
) -> Result<(), JsValue> {
    let window = web_sys::window().ok_or_else(|| JsValue::from_str("no window"))?;

    {
        let app = app.clone();
        // The observer hands its callback (entries, observer); ignoring both
        // is deliberate -- `apply_resize` re-measures from the DOM rather
        // than trusting a size captured when the event was queued.
        let on_observe = Closure::<dyn FnMut()>::new(move || {
            app.borrow_mut().resize_pending = true;
        });
        let observer = web_sys::ResizeObserver::new(on_observe.as_ref().unchecked_ref())?;
        observer.observe(canvas);
        // Both leak deliberately, for the page's lifetime: a dropped
        // `ResizeObserver` stops observing, and a dropped `Closure` leaves
        // the JS side calling into a freed table slot.
        on_observe.forget();
        std::mem::forget(observer);
    }
    {
        let app = app.clone();
        let on_resize = Closure::<dyn FnMut()>::new(move || {
            app.borrow_mut().resize_pending = true;
            // A resized window may now fit the credit, or no longer.
            apply_credit_compact();
        });
        window.add_event_listener_with_callback("resize", on_resize.as_ref().unchecked_ref())?;
        on_resize.forget();
    }
    Ok(())
}

/// Drag to pan, pinch to zoom, double-tap to zoom in, wheel to zoom toward
/// the cursor, double-click to zoom in -- the native demos' interaction
/// model, on browser events instead of gpui ones, PLUS the touch input the
/// native demos never had to add for themselves (`gpui`'s own window gives
/// native that for free; the browser gives this shell nothing until it
/// listens).
///
/// This is the part `map_view.rs` could contribute nothing to. Its handlers
/// (`window.on_mouse_event(|event: &gpui::MouseDownEvent, ...|)`, four of
/// them) are gpui types end to end -- `MouseDownEvent`, `ScrollWheelEvent`,
/// `ScrollDelta::Lines`/`Pixels`, phase dispatch, `cx.notify()`. What they
/// call INTO is portable and reused above; the plumbing is not, and would not
/// be worth porting even if it could be. Phase 2 should expect to write this
/// layer per host, not to move it.
///
/// # Pointer Events, not `touchstart`/`touchmove`/`touchend`
///
/// `mousedown`/`mousemove`/`mouseup` are replaced here by
/// `pointerdown`/`pointermove`/`pointerup`/`pointercancel` rather than
/// having a second, touch-specific set of listeners alongside them.
/// `PointerEvent` unifies mouse, touch and pen behind one `pointerId`, which
/// is exactly what letting a two-finger pinch and a one-finger mouse drag
/// share `MapApp::active_pointers` needs -- iOS Safari never sends `wheel`
/// for a touch gesture, so without Pointer Events this shell would need a
/// parallel touch state machine duplicating the mouse one, not an addition
/// to it. Each pointer calls `set_pointer_capture` on `pointerdown`, so a
/// drag (or a pinch finger) that leaves the canvas is still tracked --
/// which is also why, unlike the old mouse handler, there is no
/// `pointerleave`/`mouseleave` listener here: capture delivers `pointerup`
/// to the canvas regardless of where the pointer is released.
///
/// # Rotation and tilt
///
/// Wired, following MapLibre GL JS's defaults so the map behaves the way web
/// users expect: RIGHT-drag or CTRL+left-drag rotates (horizontal) and tilts
/// (vertical) at once (`generateMouseRotationHandler` /
/// `generateMousePitchHandler`, `src/ui/handler/mouse.ts`), and two touches
/// pinch, twist and tilt through `touch_gesture::TwoFingerGesture`. What
/// this section used to say -- that rotation was left out because this
/// file's `pan_by_pixels` ignored bearing -- stopped being true when every
/// pan here started going through `camera_control`.
fn install_input_handlers(
    canvas: &web_sys::HtmlCanvasElement,
    app: &Rc<RefCell<MapApp>>,
) -> Result<(), JsValue> {
    canvas.style().set_property("cursor", "grab")?;

    {
        let app = app.clone();
        let canvas2 = canvas.clone();
        let on_pointer_down =
            Closure::<dyn FnMut(web_sys::PointerEvent)>::new(move |e: web_sys::PointerEvent| {
                let id = e.pointer_id();
                let (x, y) = (e.offset_x() as f32, e.offset_y() as f32);
                let is_touch = e.pointer_type() == "touch";
                let is_mouse = e.pointer_type() == "mouse";
                // MapLibre's `checkCorrectEvent` for rotate+pitch: the right
                // button, or the left with Ctrl held.
                let orbit_button = is_mouse && (e.button() == 2 || (e.button() == 0 && e.ctrl_key()));
                let now = e.time_stamp();
                // So a drag (or a pinch finger) that leaves the canvas is
                // still tracked -- see this function's own doc comment.
                let _ = canvas2.set_pointer_capture(id);

                let mut a = app.borrow_mut();
                // Browsers gate audio behind a user gesture; this is the
                // first one, and it includes the engine-drawn Simulate
                // button -- a tap on it is a `pointerdown` on this same
                // canvas, not a separate DOM button. See `handle_voice_command`.
                a.voice_gesture_seen = true;
                // A press on the navigation chrome (Recenter, End, pause, speed) is
                // the chrome's, not the start of a pan. Its `pointerup`
                // still applies `GestureEnded`, which the follow camera does
                // not count as input, so it keeps following.
                if a.nav.is_active() {
                    let a = &mut *a;
                    let frame = nav_frame_of(a.camera_control.camera());
                    if a.nav.tap(x, y, &mut a.camera_control, &frame) {
                        a.needs_redraw = true;
                        return;
                    }
                }
                // Grabbing the map stops it dead, the way catching a
                // spinning wheel does: an in-flight glide must not keep
                // sliding the camera out from under a NEW gesture, whatever
                // its pointer count.
                a.pan_inertia = None;
                a.drag_samples.clear();
                if a.active_pointers.is_empty() && a.orbit.is_none() {
                    a.camera_control.apply(CameraIntent::GestureBegan);
                    a.rotate_samples.clear();
                }
                if orbit_button {
                    a.orbit = Some((id, (x, y)));
                    a.orbit_down = Some(((x, y), now));
                    drop(a);
                    let _ = canvas2.style().set_property("cursor", "move");
                    return;
                }

                credit_map_moved();
                a.active_pointers.insert(id, (x, y));
                if a.active_pointers.len() == 1 {
                    a.drag_from = Some((x, y));
                    a.touch_tap_start = if is_touch { Some((id, now, x, y)) } else { None };
                } else {
                    // A second (or further) finger just landed: this is no
                    // longer a one-finger drag, and no longer a tap in
                    // progress either. Exactly two begin a two-finger
                    // gesture, measured from where both are now.
                    a.drag_from = None;
                    a.touch_tap_start = None;
                    a.pinch = a.pointer_pair().map(|[p, q]| TwoFingerGesture::begin(p, q));
                }
                drop(a);
                let _ = canvas2.style().set_property("cursor", "grabbing");
            });
        canvas.add_event_listener_with_callback(
            "pointerdown",
            on_pointer_down.as_ref().unchecked_ref(),
        )?;
        on_pointer_down.forget();
    }
    {
        let app = app.clone();
        let on_pointer_move =
            Closure::<dyn FnMut(web_sys::PointerEvent)>::new(move |e: web_sys::PointerEvent| {
                let id = e.pointer_id();
                let (x, y) = (e.offset_x() as f32, e.offset_y() as f32);
                let mut a = app.borrow_mut();
                if let Some((orbit_id, (px, py))) = a.orbit {
                    if orbit_id == id {
                        let rotate = crate::camera_control::rotate_radians_for_drag_px(x - px);
                        if rotate != 0.0 {
                            a.camera_control.apply(CameraIntent::RotateBy { radians: rotate, anchor_px: None });
                            a.rotate_samples.push(e.time_stamp(), rotate);
                        }
                        let tilt = crate::camera_control::tilt_radians_for_drag_px(y - py);
                        if tilt != 0.0 {
                            a.camera_control.apply(CameraIntent::TiltBy { radians: tilt });
                        }
                        a.orbit = Some((id, (x, y)));
                        a.needs_redraw = true;
                        return;
                    }
                }
                if !a.active_pointers.contains_key(&id) {
                    // A move for a pointer that never went down on this
                    // canvas (a hovering mouse with no button held fires
                    // `pointermove` too) -- nothing to do.
                    return;
                }
                match a.active_pointers.len() {
                    1 => {
                        let Some((px, py)) = a.drag_from else { return };
                        a.pan_by_pixels(x - px, y - py, (px, py));
                        a.drag_from = Some((x, y));
                        a.active_pointers.insert(id, (x, y));
                        // Record for the release fling, using the EVENT's
                        // own timestamp rather than the clock at handling
                        // time. Chrome coalesces pointer motion into
                        // roughly one event per frame, so reading the clock
                        // here would compress a burst of real motion into
                        // an instant and inflate the measured velocity.
                        // `Event::time_stamp` shares `performance.now`'s
                        // time origin.
                        let at_ms = e.time_stamp();
                        a.drag_samples.push_back(DragSample { at_ms, x, y });
                        drain_drag_samples(&mut a.drag_samples, at_ms);
                        a.needs_redraw = true;
                    }
                    2 => {
                        // Two touches: `TwoFingerGesture` decides pinch-and-
                        // twist versus tilt, and `two_finger_intents` turns
                        // its answer into intents -- the same helper iOS uses.
                        // The other finger's position comes from
                        // `active_pointers`, kept current by ITS OWN moves.
                        let Some(old) = a.pointer_pair() else { return };
                        a.active_pointers.insert(id, (x, y));
                        let Some(new) = a.pointer_pair() else { return };
                        let at_ms = e.time_stamp();
                        if let Some(mut gesture) = a.pinch {
                            let step = gesture.update(old, new, at_ms);
                            a.pinch = Some(gesture);
                            for intent in crate::camera_control::two_finger_intents(&step) {
                                a.camera_control.apply(intent);
                            }
                            if step.rotate_radians != 0.0 {
                                a.rotate_samples.push(at_ms, step.rotate_radians);
                            }
                            a.needs_redraw = true;
                        }
                    }
                    _ => {
                        // Three or more pointers: no gesture is defined for
                        // this, but positions are still kept current so
                        // dropping back to one or two re-baselines from
                        // where the remaining fingers truly are.
                        a.active_pointers.insert(id, (x, y));
                    }
                }
            });
        canvas.add_event_listener_with_callback(
            "pointermove",
            on_pointer_move.as_ref().unchecked_ref(),
        )?;
        on_pointer_move.forget();
    }
    {
        let app = app.clone();
        let canvas2 = canvas.clone();
        let on_pointer_up =
            Closure::<dyn FnMut(web_sys::PointerEvent)>::new(move |e: web_sys::PointerEvent| {
                let id = e.pointer_id();
                let (x, y) = (e.offset_x() as f32, e.offset_y() as f32);
                let is_touch = e.pointer_type() == "touch";
                let at_ms = e.time_stamp();
                let mut a = app.borrow_mut();
                release_pointer(&mut a, &canvas2, id, x, y, at_ms, is_touch, false);
            });
        canvas
            .add_event_listener_with_callback("pointerup", on_pointer_up.as_ref().unchecked_ref())?;
        on_pointer_up.forget();
    }
    {
        let app = app.clone();
        let canvas2 = canvas.clone();
        // iOS fires this -- rather than `pointerup` -- when the system
        // takes the gesture away mid-touch (e.g. a Control Center or
        // app-switcher swipe starting from the edge). It must reset state
        // cleanly without being mistaken for a completed drag (no fling) or
        // a tap (no double-tap credit).
        let on_pointer_cancel =
            Closure::<dyn FnMut(web_sys::PointerEvent)>::new(move |e: web_sys::PointerEvent| {
                let id = e.pointer_id();
                let (x, y) = (e.offset_x() as f32, e.offset_y() as f32);
                let at_ms = e.time_stamp();
                let mut a = app.borrow_mut();
                release_pointer(&mut a, &canvas2, id, x, y, at_ms, false, true);
            });
        canvas.add_event_listener_with_callback(
            "pointercancel",
            on_pointer_cancel.as_ref().unchecked_ref(),
        )?;
        on_pointer_cancel.forget();
    }
    {
        let app = app.clone();
        let on_wheel =
            Closure::<dyn FnMut(web_sys::WheelEvent)>::new(move |e: web_sys::WheelEvent| {
                // Or the page scrolls under the map.
                e.prevent_default();
                // gpui hands `map_view` a delta already tagged as LINES or
                // PIXELS and it branches on the variant; the DOM says which
                // through `deltaMode`. 0 = pixels (trackpads, and Chrome's own
                // wheel emulation), 1 = lines. 100 px per line is Chrome's own
                // wheel step, so a physical notch lands on ~1 line either way.
                let lines = match e.delta_mode() {
                    web_sys::WheelEvent::DOM_DELTA_LINE => e.delta_y(),
                    _ => e.delta_y() / 100.0,
                };
                let mut a = app.borrow_mut();
                // A trackpad pinch: Chrome, Edge and Firefox report it as a
                // wheel with `ctrlKey` set and a small pixel `deltaY`. It is
                // applied AT ONCE, 1:1 with the fingers (`trackpad_pinch`),
                // not through the eased `zoom_about`: each eased call
                // re-targets from the DRAWN zoom, so every event but the last
                // in a frame, and the undelivered part of the ease, was lost
                // -- the pinch felt slow and laggy. `preventDefault` above
                // also keeps the browser from page-zooming.
                let line_mode = e.delta_mode() == web_sys::WheelEvent::DOM_DELTA_LINE;
                if let Some(levels) = a.browser_pinch.wheel(e.ctrl_key(), e.delta_y(), line_mode) {
                    if levels != 0.0 {
                        let anchor_px = (e.offset_x() as f32, e.offset_y() as f32);
                        a.camera_control.apply(CameraIntent::PinchZoomBy { levels, anchor_px });
                        a.needs_redraw = true;
                    }
                    drop(a);
                    credit_map_moved();
                    return;
                }
                // Shift+wheel tilts (vertical) and rotates (horizontal), as
                // on native. NOT Ctrl: a browser reports a trackpad pinch as
                // a wheel event with `ctrlKey` set, and that is the pinch
                // above.
                if e.shift_key() {
                    use crate::camera_control::{shift_scroll_intent, ScrollTravel};
                    // The DOM's signs are the negation of AppKit's (gpui's
                    // web platform negates them the same way).
                    let (dx, dy) = (-e.delta_x() as f32, -e.delta_y() as f32);
                    let travel = if e.delta_mode() == web_sys::WheelEvent::DOM_DELTA_LINE {
                        ScrollTravel::Lines { dx, dy }
                    } else {
                        ScrollTravel::Pixels { dx, dy }
                    };
                    let now = e.time_stamp();
                    let mut lock = match a.shift_scroll {
                        Some((lock, last)) if now - last <= 250.0 => lock,
                        _ => Default::default(),
                    };
                    if let Some(intent) = shift_scroll_intent(travel, &mut lock) {
                        a.camera_control.apply(intent);
                    }
                    a.shift_scroll = Some((lock, now));
                    a.needs_redraw = true;
                    drop(a);
                    credit_map_moved();
                    return;
                }
                a.shift_scroll = None;
                // Pixel deltas are a trackpad's continuous stream: the fast
                // ease. A line delta is a wheel notch: the discrete one.
                let pixels = e.delta_mode() != web_sys::WheelEvent::DOM_DELTA_LINE;
                a.zoom_about(-lines * ZOOM_LEVELS_PER_SCROLL_LINE, e.offset_x() as f32, e.offset_y() as f32, pixels);
                a.needs_redraw = true;
                drop(a);
                credit_map_moved();
            });
        canvas.add_event_listener_with_callback("wheel", on_wheel.as_ref().unchecked_ref())?;
        on_wheel.forget();
    }
    // Safari's trackpad pinch: WebKit's non-standard `gesturestart` /
    // `gesturechange` / `gestureend`, whose `scale` is cumulative since the
    // start. `web_sys` has no `GestureEvent`, so the fields are read by name.
    // `preventDefault` on all three is what stops Safari page-zooming.
    for kind in ["gesturestart", "gesturechange", "gestureend"] {
        let app = app.clone();
        let canvas2 = canvas.clone();
        let on_gesture = Closure::<dyn FnMut(web_sys::Event)>::new(move |e: web_sys::Event| {
            e.prevent_default();
            let field = |name: &str| js_sys::Reflect::get(&e, &JsValue::from_str(name)).ok().and_then(|v| v.as_f64());
            let mut a = app.borrow_mut();
            match kind {
                "gesturestart" => a.browser_pinch.gesture_start(),
                "gestureend" => a.browser_pinch.gesture_end(),
                _ => {
                    let Some(scale) = field("scale") else { return };
                    let levels = a.browser_pinch.gesture_change(scale);
                    if levels == 0.0 {
                        return;
                    }
                    let rect = canvas2.get_bounding_client_rect();
                    let anchor_px = match (field("clientX"), field("clientY")) {
                        (Some(x), Some(y)) => ((x - rect.left()) as f32, (y - rect.top()) as f32),
                        _ => ((rect.width() / 2.0) as f32, (rect.height() / 2.0) as f32),
                    };
                    a.camera_control.apply(CameraIntent::PinchZoomBy { levels, anchor_px });
                    a.needs_redraw = true;
                    drop(a);
                    credit_map_moved();
                }
            }
        });
        canvas.add_event_listener_with_callback(kind, on_gesture.as_ref().unchecked_ref())?;
        on_gesture.forget();
    }
    {
        let app = app.clone();
        let on_dbl =
            Closure::<dyn FnMut(web_sys::MouseEvent)>::new(move |e: web_sys::MouseEvent| {
                let mut a = app.borrow_mut();
                // One whole zoom level.
                a.zoom_about(1.0, e.offset_x() as f32, e.offset_y() as f32, false);
                a.needs_redraw = true;
            });
        canvas.add_event_listener_with_callback("dblclick", on_dbl.as_ref().unchecked_ref())?;
        on_dbl.forget();
    }
    {
        // A right-drag orbits, so the browser's context menu must not open
        // on its release -- MapLibre's `mouse.ts` suppresses it for the same
        // reason ("we don't allow it with rotation").
        let on_context_menu = Closure::<dyn FnMut(web_sys::MouseEvent)>::new(move |e: web_sys::MouseEvent| {
            e.prevent_default();
        });
        canvas.add_event_listener_with_callback("contextmenu", on_context_menu.as_ref().unchecked_ref())?;
        on_context_menu.forget();
    }
    {
        // The other half of the autoplay gesture -- see the `pointerdown`
        // handler above and `handle_voice_command`. On `window`, not the canvas:
        // a key can be typed with focus anywhere on the page.
        //
        // Also native's drive keys, G / Shift+G / P: `NavHost::key` decides
        // what each does, and refuses a key typed into a field or held with
        // Ctrl/Cmd/Alt, or G on a page with no `?nav=`.
        let app = app.clone();
        let on_keydown = Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |e: web_sys::KeyboardEvent| {
            let mut guard = app.borrow_mut();
            let a = &mut *guard;
            a.voice_gesture_seen = true;
            let target = e.target().and_then(|t| t.dyn_into::<web_sys::HtmlElement>().ok());
            let tag = target.as_ref().map(|t| t.tag_name());
            let key = e.key();
            let press = crate::nav::host::KeyPress {
                key: &key,
                shift: e.shift_key(),
                other_modifier: e.ctrl_key() || e.meta_key() || e.alt_key(),
                repeat: e.repeat(),
                target_tag: tag.as_deref(),
                target_editable: target.as_ref().is_some_and(|t| t.is_content_editable()),
            };
            let frame = nav_frame_of(a.camera_control.camera());
            if a.nav.key(&press, &a.camera_control, &frame) {
                a.needs_redraw = true;
            }
        });
        web_sys::window()
            .unwrap()
            .add_event_listener_with_callback("keydown", on_keydown.as_ref().unchecked_ref())?;
        on_keydown.forget();
    }
    // The voice list arrives asynchronously: pick now (Safari lists at
    // once) and again whenever it changes (Chrome lists after load).
    if let Ok(synth) = web_sys::window().unwrap().speech_synthesis() {
        if let Ok(mut a) = app.try_borrow_mut() {
            pick_web_voice(&mut a, &synth);
        }
        let app = app.clone();
        let listed = synth.clone();
        let on_voices = Closure::<dyn FnMut()>::new(move || {
            if let Ok(mut a) = app.try_borrow_mut() {
                pick_web_voice(&mut a, &listed);
            }
        });
        synth.add_event_listener_with_callback("voiceschanged", on_voices.as_ref().unchecked_ref())?;
        on_voices.forget();
    }
    Ok(())
}

/// Shared by the `pointerup` and `pointercancel` listeners in
/// `install_input_handlers`: removes `id` from `active_pointers` and settles
/// whatever gesture that pointer was part of.
///
/// `is_cancel` distinguishes the two callers. A `pointerup` may end in a
/// fling (a plain one-finger drag releasing) or a recognised tap; a
/// `pointercancel` -- iOS's escape hatch for a gesture the OS itself takes
/// over -- must never produce either, since the touch it describes was
/// interrupted rather than completed.
fn release_pointer(
    a: &mut MapApp,
    canvas: &web_sys::HtmlCanvasElement,
    id: i32,
    x: f32,
    y: f32,
    at_ms: f64,
    is_touch: bool,
    is_cancel: bool,
) {
    if a.orbit.is_some_and(|(orbit_id, _)| orbit_id == id) {
        a.orbit = None;
        // A still, short right press (not an orbit drag) picks a destination.
        if let Some((down, down_ms)) = a.orbit_down.take() {
            if !is_cancel && crate::nav::host::is_destination_click(down, (x, y), at_ms - down_ms) {
                destination_click(a, x, y);
            }
        }
    }
    // Captured BEFORE removal: true only if this pointer was the sole one
    // down and mid-drag, i.e. a plain one-finger pan rather than the tail of
    // a pinch.
    let was_single_drag = a.drag_from.is_some() && a.active_pointers.len() == 1;
    a.active_pointers.remove(&id);
    // Two touches became one or none: the two-finger gesture is over. Two
    // from three: a new pair, measured from where it is now.
    a.pinch = a.pointer_pair().map(|[p, q]| TwoFingerGesture::begin(p, q));
    if a.active_pointers.is_empty() && a.orbit.is_none() {
        let rotate_velocity = if is_cancel { 0.0 } else { a.rotate_samples.velocity(at_ms) };
        a.rotate_samples.clear();
        a.camera_control.apply(CameraIntent::GestureEnded { rotate_velocity });
        a.needs_redraw = true;
    }

    if !is_cancel && was_single_drag {
        // Let go mid-motion and the map keeps gliding, easing to a stop.
        // Drained FIRST, so a drag that stalled before release ages its own
        // history out and correctly flings nothing.
        drain_drag_samples(&mut a.drag_samples, at_ms);
        let samples: Vec<DragSample> = a.drag_samples.iter().copied().collect();
        a.pan_inertia = pan_inertia_from_samples(&samples, at_ms);
    } else {
        // Either cancelled, or this pointer was one of a pinch: neither
        // measures a one-finger fling.
        a.pan_inertia = None;
    }
    a.drag_samples.clear();
    a.drag_from = None;

    // Re-baseline rather than falling back to a stale anchor: whichever
    // pointer(s) remain down keep panning/pinching from where they truly
    // are right now, not from state left over by the finger that just
    // lifted.
    if a.active_pointers.len() == 1 {
        if let Some((_, &pos)) = a.active_pointers.iter().next() {
            a.drag_from = Some(pos);
        }
    }

    if is_cancel {
        a.touch_tap_start = None;
        let _ = canvas.style().set_property("cursor", "grab");
        return;
    }
    a.needs_redraw = true;

    // Double-tap: only for the pointer that both STARTED and ENDED a short,
    // near-stationary single-finger touch (never for mouse -- desktop's
    // double-click zoom is the existing, untouched `dblclick` listener).
    if is_touch {
        if let Some((tap_id, start_ms, sx, sy)) = a.touch_tap_start {
            if tap_id == id {
                let dt = at_ms - start_ms;
                let moved = pointer_distance((sx, sy), (x, y));
                if dt <= TAP_MAX_DURATION_MS && moved <= TAP_MAX_MOVEMENT_PX as f64 {
                    match a.last_tap {
                        Some((last_ms, lx, ly))
                            if at_ms - last_ms <= DOUBLE_TAP_MS
                                && pointer_distance((lx, ly), (x, y)) <= DOUBLE_TAP_PX as f64 =>
                        {
                            // One whole zoom level, mirroring the `dblclick`
                            // listener's own amount.
                            a.zoom_about(1.0, x, y, false);
                            a.needs_redraw = true;
                            a.last_tap = None;
                        }
                        _ => a.last_tap = Some((at_ms, x, y)),
                    }
                } else {
                    // Moved too far or held too long to be a tap at all --
                    // breaks any double-tap sequence in progress.
                    a.last_tap = None;
                }
            }
        }
        a.touch_tap_start = None;
    }

    let _ = canvas.style().set_property("cursor", "grab");
}

/// Whether the document is currently hidden -- backgrounded tab, minimised
/// window, or (on macOS) a window another window completely covers.
fn document_hidden() -> bool {
    web_sys::window().and_then(|w| w.document()).map(|d| d.hidden()).unwrap_or(false)
}

/// Fetching and baking, driven by a timer instead of by the frame callback,
/// for exactly as long as the document is hidden.
///
/// # The bug this exists for
///
/// Everything this app does after `run` returns is driven from
/// `requestAnimationFrame`: the tile request, the bake, the draw. Chrome
/// throttles rAF hard in a hidden or occluded tab, and in the common case of
/// a tab opened in the background and not looked at, does not run it AT ALL.
/// Measured, twice, on a 180-second timeout: a background tab finished its
/// style fetch and its GPU setup within 60 ms and then sat at "starting…" for
/// three minutes, with four resource entries on the page and not one of them
/// a tile. In a merely occluded tab rAF ran at about 2.5 Hz, which put first
/// triangle at 7568 ms over NINETEEN frames.
///
/// That is not a slow renderer, and it is why the log line now carries the
/// frame count. But it is still a bad outcome: the map begins loading from
/// nothing at the instant the user switches to the tab, which is the one
/// moment they are watching.
///
/// # What it does, and what it deliberately does not
///
/// It runs the two halves of the loop that have a LASTING effect -- issue the
/// requests, bake what has arrived -- and not the third. It never draws: a
/// hidden surface has no frame worth presenting, `get_current_texture` on one
/// is not a thing to lean on, and the pixels would be discarded anyway. The
/// visible frame loop redraws on its first tick, off `needs_redraw`.
///
/// It bakes with NO ration. `BAKE_BUDGET_MS_PER_FRAME` is 12 because a bake
/// costs 7-26 ms and a frame is 16.7; a hidden tab has no frame to fill, and
/// rationing against ticks the browser clamps to one second apart would spend
/// twenty seconds doing 150 ms of work. The work is bounded by the tile cover
/// and terminates.
///
/// The chain stops itself as soon as the document is visible, so a foreground
/// tab carries no timer at all -- `install_visibility_handler` starts a fresh
/// one if it is ever hidden again.
fn start_hidden_pump(app: &Rc<RefCell<MapApp>>) {
    if app.borrow().pump_running {
        return;
    }
    app.borrow_mut().pump_running = true;

    // Installed ONCE, into a `thread_local` that outlives every tick, and
    // never dropped. A hide that arrives later re-arms this same closure
    // rather than building a second one.
    //
    // This replaces an `Rc<RefCell<Option<Closure>>>` the closure itself
    // held the only handle to, cleared with `*holder2.borrow_mut() = None`
    // on the visible branch. That is a use-after-free, and not a subtle
    // one: the assignment drops the `Closure`, dropping the `Closure` drops
    // the environment it captured, the environment held the last `Rc` to
    // the very cell being assigned through -- so the allocation is freed
    // while the `RefMut` guard is live, and the guard's `Drop` then writes
    // its borrow flag into freed memory. The closure's own captured state
    // is gone underneath it at the same moment. It survived only because
    // the next statement is `return`.
    //
    // Nothing here has to be freed for the cycle's sake either: `.forget()`
    // is what every other handler on this page does (see
    // `install_input_handlers` and `install_resize_handlers`), for the same
    // reason -- a page-lifetime callback outliving the page is not a leak.
    let app2 = app.clone();
    let pump = Closure::<dyn FnMut()>::new(move || {
        if !document_hidden() {
            // Visible: the frame loop has the wheel back. Stop re-arming;
            // the closure stays installed for the next time the tab hides.
            app2.borrow_mut().pump_running = false;
            return;
        }
        if !app2.borrow().frozen {
            app2.borrow_mut().startup.mark_once("hidden pump");
            MapApp::request_missing_tiles(&app2);
            let mut a = app2.borrow_mut();
            a.decode_arrived(f64::INFINITY);
            if a.bake_some(f64::INFINITY) {
                // So the first VISIBLE frame paints what the pump baked
                // rather than waiting for the next thing to change.
                a.needs_redraw = true;
            }
            // The pump reports the milestone it reaches, because otherwise
            // nothing does: `log_full_cover` is otherwise only reached from
            // the frame loop, and the frame loop is exactly what is not
            // running. Without this a hidden tab could load and bake its
            // whole cover in under a second and still show no evidence of
            // having done anything at all.
            let plan = a.plan_frame();
            {
                let a = &mut *a;
                let budget = std::time::Duration::from_secs_f64(OVERLAY_BUDGET_MS_PER_FRAME / 1e3);
                let update_started = now_ms();
                let busy = a.overlays.update(a.camera_control.camera(), &plan.cover, budget);
                let update_ms = now_ms() - update_started;
                if busy {
                    a.needs_redraw = true;
                }
                census_page::after_overlays(a, busy, update_ms);
            }
            // Population dots: plan (the fetches go out below, with no
            // borrow held), so a tab hidden at load has its dots too.
            a.plan_dots();
            a.log_full_cover(&plan);
        }
        drive_dot_jobs(&app2);
        schedule_hidden_pump(HIDDEN_PUMP_INTERVAL_MS);
    });
    // Replacing whatever was installed drops the previous `Closure`, which
    // is safe for the one reason the old arrangement was not: this function
    // is never reached from INSIDE the pump. Its only two callers are `run`
    // and the `visibilitychange` handler, and `pump_running` -- cleared by
    // a tick that has already decided to return -- keeps a second install
    // from landing while a tick is on the stack.
    PUMP.with(|slot| *slot.borrow_mut() = Some(pump));
    // Zero, not `HIDDEN_PUMP_INTERVAL_MS`: a tab that is hidden at load must
    // not wait a quarter second before sending its first tile request, and
    // this is the whole point of the pump.
    schedule_hidden_pump(0);
}

thread_local! {
    /// The hidden-tab pump's callback, for the page's lifetime.
    ///
    /// A `thread_local` and not an `Rc` the closure captures a handle to:
    /// the closure must be reachable from inside itself in order to re-arm
    /// the chain, and the only two ways to arrange that are a reference
    /// cycle the closure breaks by dropping itself -- which is the
    /// use-after-free described in [`start_hidden_pump`] -- or an owner
    /// that is not the closure. This is the owner that is not the closure.
    static PUMP: RefCell<Option<Closure<dyn FnMut()>>> = const { RefCell::new(None) };
}

/// Asks the browser to run the pump again in `delay_ms`.
///
/// A no-op before [`start_hidden_pump`] has installed one, which is the
/// correct reading of "wake the pump" on a page that has no pump.
fn schedule_hidden_pump(delay_ms: i32) {
    PUMP.with(|slot| {
        // The `Ref` is held only across `set_timeout`, which cannot re-enter
        // this cell: `setTimeout` schedules, it does not call.
        if let (Some(w), Some(cb)) = (web_sys::window(), slot.borrow().as_ref()) {
            let _ = w.set_timeout_with_callback_and_timeout_and_arguments_0(
                cb.as_ref().unchecked_ref(),
                delay_ms,
            );
        }
    });
}

/// Starts the pump whenever the document goes hidden, and asks for one draw
/// when it comes back.
///
/// The redraw matters: the pump may have baked several tiles while hidden,
/// and the frame loop only draws when `needs_redraw` or a bake says so.
/// Without this the first visible frame would present the pre-hidden picture.
fn install_visibility_handler(app: &Rc<RefCell<MapApp>>) -> Result<(), JsValue> {
    let document = web_sys::window()
        .and_then(|w| w.document())
        .ok_or_else(|| JsValue::from_str("no document"))?;
    let app = app.clone();
    let cb = Closure::<dyn FnMut()>::new(move || {
        if document_hidden() {
            start_hidden_pump(&app);
        } else {
            app.borrow_mut().needs_redraw = true;
        }
    });
    document
        .add_event_listener_with_callback("visibilitychange", cb.as_ref().unchecked_ref())?;
    cb.forget();
    Ok(())
}

/// A plain `requestAnimationFrame` loop. It asks for tiles, spends at most
/// `BAKE_BUDGET_MS_PER_FRAME` tessellating, and redraws only when something
/// changed -- so a still map costs one comparison per frame rather than a
/// full pass.
///
/// An OCCLUDED or backgrounded window throttles `requestAnimationFrame` to
/// zero, so NOTHING here runs at all -- no fetch, no bake, no draw, and no
/// error either. That used to be simply a caveat to anyone driving this from
/// automation, and it was also a real user-facing bug: a tab opened in the
/// background loaded nothing whatsoever until it was looked at.
/// [`start_hidden_pump`] is the answer to it, and takes over the fetch and
/// bake halves of this loop for as long as the document is hidden.
fn start_frame_loop(app: &Rc<RefCell<MapApp>>) {
    let holder: Rc<RefCell<Option<Closure<dyn FnMut()>>>> = Rc::new(RefCell::new(None));
    let holder2 = holder.clone();
    let app = app.clone();
    *holder.borrow_mut() = Some(Closure::<dyn FnMut()>::new(move || {
        // Resize first, and before the tile request: `apply_resize` widens
        // the camera's viewport, and `request_missing_tiles` immediately
        // below asks `cover()` -- which reads that viewport -- what is now
        // visible. Doing it in the other order costs a frame of blank edge.
        if app.borrow().frozen {
            if let Some(cb) = holder2.borrow().as_ref() {
                let _ =
                    web_sys::window().unwrap().request_animation_frame(cb.as_ref().unchecked_ref());
            }
            return;
        }
        {
            let mut a = app.borrow_mut();
            let now = now_ms();
            a.record_frame_stamp(now);
            a.startup.mark_once("first frame");
            // `devicePixelRatio` is polled rather than listened for. It is a
            // plain number, so reading it forces no layout -- unlike the CSS
            // box, which is why THAT is event-driven -- and the alternative
            // is a re-arming `(resolution: Ndppx)` media query, which is
            // both fiddlier and, as it turns out, not exercisable: Chrome's
            // own `Emulation.setDeviceMetricsOverride` changes the ratio
            // without ever firing that query's `change` event (confirmed
            // against a plain JS listener, so it is the emulation, not us).
            // A signal that cannot be tested is a signal that will be wrong.
            //
            // Compared against `dpr_seen`, not `dpr`: when the texture-limit
            // clamp bites they differ permanently, and comparing against the
            // applied scale would re-enter `apply_resize` every frame.
            let dpr_now =
                web_sys::window().map(|w| w.device_pixel_ratio() as f32).unwrap_or(a.dpr_seen);
            if a.resize_pending || dpr_now != a.dpr_seen {
                a.resize_pending = false;
                a.apply_resize();
            }
        }
        {
            // The camera's eases, rotation inertia and `ease_to`. A frame is
            // drawn while this says something is still moving, and not after.
            let mut a = app.borrow_mut();
            // Navigation first, so a follow camera's `jump_to` lands before
            // the eases run -- native `paint`'s and iOS `advance_motion`'s
            // order.
            if a.nav.is_active() {
                let a = &mut *a;
                let frame = nav_frame_of(a.camera_control.camera());
                let tick = a.nav.advance(&mut a.camera_control, &frame);
                a.needs_redraw |= tick.redraw;
                a.nav_outbox.extend(tick.fetch);
                if let Some(command) = a.nav.take_voice() {
                    handle_voice_command(a, command);
                }
            }
            if a.nav.trip_active() != a.nav_brand_hidden {
                a.nav_brand_hidden = a.nav.trip_active();
                set_brand_card_hidden(a.nav_brand_hidden);
            }
            let now = now_ms();
            let dt_ms = a.last_advance_ms.replace(now).map_or(0.0, |last| (now - last).max(0.0));
            if a.camera_control.advance(std::time::Duration::from_secs_f64(dt_ms / 1000.0)) {
                a.needs_redraw = true;
            }
        }
        // Route requests `NavHost` asked for (a reroute; a destination
        // click queues here too), sent once the borrow above is released.
        let outbox = std::mem::take(&mut app.borrow_mut().nav_outbox);
        for request in outbox {
            fetch_nav_route(&app, request);
        }
        // The glide, AFTER the camera's eases -- the order native
        // (`MapView::paint`) and iOS (`advance_motion`) run them in, so a
        // fling and a mid-glide zoom compose the same way on every shell --
        // and BEFORE the tile request below reads `cover()`. Unlike a zoom
        // ease this takes absolute time rather than a per-frame delta: the
        // glide is a fixed-duration curve, so a long stall should find it
        // finished, not replay it in slow motion.
        app.borrow_mut().advance_pan_inertia(now_ms());
        MapApp::request_missing_tiles(&app);
        {
            let mut a = app.borrow_mut();
            // Not for a wake -- this loop ticks regardless -- but so the
            // tables forget long-unwanted tiles and stay bounded.
            let now = now_ms();
            a.retries.park_overdue(now);
            a.raster_retries.park_overdue(now);
            let decode_started = now_ms();
            if a.decode_arrived(DECODE_BUDGET_MS_PER_FRAME) {
                a.needs_redraw = true;
            }
            let bake_started = now_ms();
            let baked = a.bake_some(BAKE_BUDGET_MS_PER_FRAME);
            let bake_ms = now_ms() - bake_started;
            let decode_ms = bake_started - decode_started;
            // Planned AFTER the bake, so the counters describe the frame that
            // is about to be presented rather than the one before it. And
            // planned on EVERY frame, drawn or not: a still map with a hole in
            // it is still a frame with a hole in it, and skipping the analysis
            // whenever `needs_redraw` is false would quietly undercount
            // exactly the frames the user complains about.
            let plan = a.plan_frame();
            // Caller overlays: index builds, point views and tile bakes,
            // inline under their own share of the frame.
            {
                let a = &mut *a;
                let budget = std::time::Duration::from_secs_f64(OVERLAY_BUDGET_MS_PER_FRAME / 1e3);
                let update_started = now_ms();
                let busy = a.overlays.update(a.camera_control.camera(), &plan.cover, budget);
                let update_ms = now_ms() - update_started;
                if busy {
                    a.needs_redraw = true;
                }
                census_page::after_overlays(a, busy, update_ms);
            }
            // Population dots: which count tiles this frame wants, the
            // view's people per dot, eviction. The fetches go out at the
            // end of the tick, once this borrow is released (`drive_dot_jobs`).
            a.plan_dots();
            a.log_full_cover(&plan);
            a.metrics.record(&plan);
            a.retained = plan.retained.clone();
            // A label fade the redraw cap turned down last frame: due now.
            if a.fade_wake_at_s.is_some_and(|at| now_ms() / 1000.0 >= at) {
                a.fade_wake_at_s = None;
                a.needs_redraw = true;
            }
            let drew = baked || a.needs_redraw;
            let draw_started = now_ms();
            if drew {
                a.needs_redraw = false;
                a.draw(&plan);
                census_page::after_draw(&mut a);
                a.sync_compass_dom();
                a.sync_brand_mark_dom();
                if a.dots_first_ms.is_none() && a.dots.stats(DOT_LAYER).is_some_and(|s| s.dots_drawn > 0) {
                    a.dots_first_ms = Some(now_ms());
                    a.startup.mark_once("first dots");
                    log(&format!("dots: first dots at {:.0} ms", now_ms()));
                }
            }
            if a.trace.is_some() {
                let draw_ms = now_ms() - draw_started;
                a.trace_line(&plan, drew, decode_ms, bake_ms, draw_ms);
            }
        }
        drive_dot_jobs(&app);
        if let Some(cb) = holder2.borrow().as_ref() {
            let _ =
                web_sys::window().unwrap().request_animation_frame(cb.as_ref().unchecked_ref());
        }
    }));
    let _ = web_sys::window()
        .unwrap()
        .request_animation_frame(holder.borrow().as_ref().unwrap().as_ref().unchecked_ref());
}

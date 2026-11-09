//! The iOS half: an interactive, multi-tile vector map, on a UIKit window.
//!
//! Everything here is plumbing -- window, surface, touches, fetch, one draw
//! call per frame. The map itself is four engine calls, and they are the
//! same four a native `MapView` makes and the same four the wasm spike
//! makes:
//!
//! ```text
//! style_spec::parse_and_compile(bytes)      -> style::Style
//! mvt::decode(bytes)                        -> mvt::DecodedTile
//! style::match_tile(&tile, &style, zoom)    -> style::PaintPlan
//! render::tessellate_plan(&plan, 4096, ...) -> render::TileMesh
//! ```
//!
//! and the draw is `render::gpu::TilePipeline`, running the crate's own
//! `src/render/shader.wgsl`. No triangle on this screen was produced by
//! anything in this file.
//!
//! # What came from where
//!
//! This is the wasm spike's `web/src/app.rs` with the browser shell taken
//! out and a UIKit one put in. The parts that are the SAME code, marked
//! `[FROM WASM]` at each site, are everything that decides what a map does:
//! the tile cover and the coarse prefetch cover, the stand-in search, the
//! retained-set eviction, the bake budget, `pan_by_pixels`,
//! `center_anchored_at`, and MapLibre's pan inertia. The parts that are new
//! are the parts a host owns and cannot share: the event loop, the surface,
//! the network, and the gestures.
//!
//! Two shell differences are worth naming, because they are not
//! translations of the browser code -- they are things a phone has and a tab
//! did not:
//!
//! * **Threads.** `fetch` was a promise on the main thread's task queue;
//!   here it is [`crate::net::Fetcher`], eight blocking workers with the
//!   results collected once per frame. The BAKE is still on the main
//!   thread, budgeted by `BAKES_PER_FRAME`, which is the single biggest
//!   thing left undone -- see that constant.
//! * **Pinch.** There is no wheel. Two-finger pinch replaces
//!   `zoom_towards_point`'s scroll-line input with a distance RATIO, which
//!   is a more direct expression of the same anchored-zoom arithmetic: the
//!   `scale_factor` the browser derived from a line count arrives here
//!   measured off the user's fingers. The same two fingers also carry
//!   ROTATION, which no mouse gesture in the browser shell could express:
//!   see [`ROTATION_DEADZONE_RADIANS`] and `MapApp::rotate_by`, and
//!   `ios/ROTATION.md` for the whole design.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

// The winit shell -- the standalone demo app's event loop, window and
// `main` -- is a child module so it reaches `MapApp`'s private state the way
// this file does, and is behind `winit-shell` so a host that brings its own
// UIKit view (`host`, and the `embed-ios/` crate on top of it) links no
// winit at all.
#[cfg(feature = "winit-shell")]
#[path = "app_winit.rs"]
mod winit_shell;
#[cfg(feature = "winit-shell")]
pub use winit_shell::main;
// The embedding door: a host-owned `CAMetalLayer`, host-driven frames and
// touches, overlays by stable id. See its module doc.
#[path = "app_host.rs"]
pub mod host;
// A second vector source over the basemap, with its own style layers: the
// desktop's `MapView::add_vector_overlay`, on this shell's threads.
#[path = "app_vector_overlay.rs"]
pub mod vector_overlay;

/// A touch's phase, as UIKit reports it (`UITouch.Phase`, and winit's
/// `TouchPhase`, which is the same four states).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchPhase {
    Started,
    Moved,
    Ended,
    Cancelled,
}

/// The drawable as the host last described it: the layer's size in
/// PHYSICAL pixels, the points-to-pixels scale, and the status-bar and
/// home-indicator bands in points. The winit shell reads these off its
/// window (`winit_shell::metrics_of`); an embedding host passes them in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SurfaceMetrics {
    pub width_px: u32,
    pub height_px: u32,
    pub scale: f64,
    pub top_inset_pt: f32,
    pub bottom_inset_pt: f32,
}

impl Default for SurfaceMetrics {
    fn default() -> Self {
        Self { width_px: 1, height_px: 1, scale: 1.0, top_inset_pt: 0.0, bottom_inset_pt: 0.0 }
    }
}

use crate::camera_control::{CameraController, CameraIntent, RotateSamples, MAX_ZOOM, MIN_ZOOM};
use crate::geo::TileId;
use crate::mvt::DecodedTile;
use crate::net::{Fetched, Fetcher, SpriteResource, TileKey, TileKind};
use crate::render::{alloc::TrackedBuffer, Camera, GpuContext, SpaceDraw, TileBuffers, TilePipeline};
use crate::style::{Lerp, Style};
use crate::touch_gesture::TwoFingerGesture;

/// `{z}/{x}/{y}` filled in with `tile`'s coordinates, and
/// `{bbox-epsg-3857}` (MapLibre's WMS convention) with its extent in Web
/// Mercator metres, `minx,miny,maxx,maxy` -- the desktop's
/// `tile_source::wms::tile_bbox_3857`.
fn fill_template(template: &str, tile: TileId) -> String {
    let mut url = template
        .replace("{z}", &tile.z.to_string())
        .replace("{x}", &tile.x.to_string())
        .replace("{y}", &tile.y.to_string());
    if url.contains("{bbox-epsg-3857}") {
        const HALF: f64 = std::f64::consts::PI * 6_378_137.0;
        let size = 2.0 * HALF / f64::from(1u32 << tile.z.min(31));
        let minx = -HALF + f64::from(tile.x) * size;
        let maxy = HALF - f64::from(tile.y) * size;
        url = url.replace("{bbox-epsg-3857}", &format!("{},{},{},{}", minx, maxy - size, minx + size, maxy));
    }
    url
}

/// The MVT extent every `PaintOp`'s geometry is in, and the divisor
/// `tile_model_matrix` uses to scale a tile back to world units. OpenFreeMap
/// encodes at 4096, the OpenMapTiles default.
const TILE_EXTENT: f32 = 4096.0;

/// The committed route fixtures, compiled in: a phone has no `tests/` to
/// read. Key already stripped (see `tests/fixtures/routes/`). The first is
/// the fallback for a name that matches none of them.
const DEMO_ROUTES: [(&str, &[u8]); 3] = [
    ("manila", include_bytes!("../../tests/fixtures/routes/manila.json")),
    (
        "singapore-bugis-marina-bay",
        include_bytes!("../../tests/fixtures/routes/singapore-bugis-marina-bay.json"),
    ),
    (
        "singapore-seletar-rotary-uturn",
        include_bytes!("../../tests/fixtures/routes/singapore-seletar-rotary-uturn.json"),
    ),
];

/// The fixture a demo request names, or the first one.
#[cfg_attr(not(feature = "winit-shell"), allow(dead_code))]
fn demo_route_bytes(name: &str) -> &'static [u8] {
    DEMO_ROUTES.iter().find(|(known, _)| *known == name).map_or(DEMO_ROUTES[0].1, |(_, bytes)| *bytes)
}

/// This frame's inputs for `NavHost`, with the phone's safe area: the status
/// bar and Dynamic Island on top, the home indicator below, both in points.
fn nav_frame_of(now_ms: f64, camera: &Camera, top_inset_pt: f32, bottom_inset_pt: f32) -> crate::nav::host::NavFrame {
    let unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64() * 1000.0);
    crate::nav::host::NavFrame {
        now_ms,
        wall: crate::nav::host::WallClock {
            unix_ms,
            utc_offset_min: chrono::Local::now().offset().local_minus_utc() / 60,
        },
        viewport: (camera.viewport_width_px, camera.viewport_height_px),
        insets: crate::nav::host::SafeInsets { top: top_inset_pt, bottom: bottom_inset_pt, left: 0.0, right: 0.0 },
    }
}

/// OpenFreeMap's planet tiles: no API key and no token. The one shared
/// constant ([`crate::basemap::OPENFREEMAP_PLANET_TILE_URL`]), so this, the
/// wasm spike and `examples/basic_map.rs` cannot drift onto different dated
/// paths. Used whenever [`TileSchema::resolve`] falls back to OMT -- see
/// that function and [`crate::styles`] for when and why.
const TILE_URL: &str = crate::basemap::OPENFREEMAP_PLANET_TILE_URL;

/// Baked into the binary at BUILD time, never read at runtime and never
/// committed: `option_env!` resolves against `rustc`'s own environment at
/// compile time, so the value exists only in whatever process compiled this
/// crate and in the resulting binary -- not in any tracked file. `None` on
/// every `cargo build`/`cargo test` invocation that does not export it
/// first, which is the state this repository is in and stays in; only
/// `ios/run-device.sh`, invoked as
/// `MAPBOX_ACCESS_TOKEN="$TOK" ... ./run-device.sh`, sets it.
///
/// See [`TileSchema::resolve`] for what happens with it.
///
/// Gated: Mapbox is OFF (2026-10, a usage spike on the key) unless the build
/// also had `GPUI_MAP_ALLOW_MAPBOX=1` -- the same opt-in
/// `examples/common/kit.rs::MAPBOX_ALLOW_VAR` reads natively. Without it
/// this is `None` whatever was exported, and the app draws OpenFreeMap.
const MAPBOX_ACCESS_TOKEN: Option<&str> = if MAPBOX_ALLOWED { option_env!("MAPBOX_ACCESS_TOKEN") } else { None };

/// The build-time opt-in, `GPUI_MAP_ALLOW_MAPBOX=1`.
const MAPBOX_ALLOWED: bool = match option_env!("GPUI_MAP_ALLOW_MAPBOX") {
    Some(v) => matches!(v.as_bytes(), b"1"),
    None => false,
};

/// Which schema this build's tile source and style are paired against.
///
/// This is the whole of what keeps a style and a tile source from being
/// mismatched (see [`crate::styles`]'s module doc): the two are chosen
/// TOGETHER, by [`TileSchema::resolve`], from exactly one of two fixed
/// pairings -- there is no code path that can compile an OMT style and then
/// pick the Mapbox tile URL, or vice versa, because both halves come off
/// the SAME `TileSchema` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TileSchema {
    /// OpenFreeMap planet tiles ([`TILE_URL`]), OpenMapTiles vocabulary --
    /// [`crate::styles::STYLES`]. Today's default, and the fallback when no
    /// usable token was baked in at build time.
    Omt,
    /// Mapbox's `composite` vector source, Streets-v8 vocabulary --
    /// [`crate::styles::MAPBOX_STYLES`]. Only reachable with a token.
    MapboxComposite,
}

impl TileSchema {
    /// This build's cycle of styles -- never the other schema's.
    fn styles(self) -> &'static [crate::styles::StyleChoice] {
        match self {
            TileSchema::Omt => crate::styles::STYLES,
            TileSchema::MapboxComposite => crate::styles::MAPBOX_STYLES,
        }
    }

    /// The real-data zoom ceiling of THIS schema's tile source --
    /// `crate::source_zoom`'s two named constants, picked structurally
    /// rather than guessed from a URL, because this build already knows
    /// which schema it is without reading one back out of `tile_url`.
    ///
    /// A method rather than a cached field on `MapApp`, deliberately: the
    /// old `SOURCE_MAX_ZOOM` this replaces was a single hardcoded 14 read
    /// by every zoom-selection site, which was the whole defect (see
    /// `crate::source_zoom`'s module doc) -- New York drew no buildings
    /// because the Mapbox composite source was never asked past z14, where
    /// its own style's `building` layer (`minzoom: 15`) has nothing to
    /// draw. Every call site below reads `self.schema.source_max_zoom()`
    /// fresh, so there is nothing here for a future source switch to leave
    /// stale.
    fn source_max_zoom(self) -> u8 {
        match self {
            TileSchema::Omt => crate::source_zoom::OPENMAPTILES_MAX_ZOOM,
            TileSchema::MapboxComposite => crate::source_zoom::MAPBOX_COMPOSITE_MAX_ZOOM,
        }
    }

    /// Picks the schema (and, alongside it, the `{z}/{x}/{y}` tile URL
    /// template) once at startup, and logs the decision -- WITHOUT ever
    /// printing the token itself, on any path.
    ///
    /// Three outcomes, all logged:
    /// * no token baked in -> `Omt`, OpenFreeMap, today's behaviour;
    /// * a token baked in, but `examples/mapbox-streets-style.json`'s
    ///   `sources.composite.url` cannot be read (a corrupt or edited
    ///   fixture) -> `Omt`, with a loud warning, because falling back to a
    ///   style this app can actually pair correctly beats compiling a
    ///   Mapbox style against a tile source it was never given;
    /// * a token baked in and the composite URL resolves -> `MapboxComposite`.
    fn resolve() -> (TileSchema, String) {
        let Some(token) = MAPBOX_ACCESS_TOKEN.map(str::trim).filter(|t| !t.is_empty()) else {
            crate::mlog!(
                "[map] Mapbox off at build time (needs GPUI_MAP_ALLOW_MAPBOX=1 and MAPBOX_ACCESS_TOKEN) -- \
                 falling back to OpenFreeMap tiles and the OpenMapTiles-schema style set"
            );
            return (TileSchema::Omt, TILE_URL.to_string());
        };
        match mapbox_tile_url_template(token) {
            Ok(url) => {
                crate::mlog!("[map] MAPBOX_ACCESS_TOKEN set at build time -- using Mapbox composite tiles");
                (TileSchema::MapboxComposite, url)
            }
            Err(e) => {
                crate::mlog!(
                    "[map] MAPBOX_ACCESS_TOKEN set, but the composite tile URL could not be \
                     derived ({e:#}) -- falling back to OpenFreeMap tiles and the \
                     OpenMapTiles-schema style set"
                );
                (TileSchema::Omt, TILE_URL.to_string())
            }
        }
    }
}

/// The host's own basemap archive and the credit it owes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BasemapArchive {
    pub archive: String,
    pub credit: String,
    /// Outside the archive's bounds, draw the schema's online tiles (the
    /// archive's region in full detail, the rest of the world online).
    /// `false`: the archive alone, blank outside it.
    pub online_outside: bool,
}

impl BasemapArchive {
    /// The credit an OpenMapTiles archive built from OpenStreetMap owes.
    pub const OMT_CREDIT: &'static str = "\u{a9} OpenMapTiles \u{a9} OpenStreetMap contributors";
}

/// Whether `tile` comes from the online tiles under an archive with
/// `bounds` that allows them: when it is not wholly inside them (the
/// archive's copy is half empty) and its online fetch has not just failed.
pub fn tile_from_online(tile: TileId, bounds: crate::geo::LonLatBounds, online_failed: bool) -> bool {
    let t = crate::geo::tile_bounds_lonlat(tile);
    let inside = t.min_lon >= bounds.min_lon && t.max_lon <= bounds.max_lon && t.min_lat >= bounds.min_lat && t.max_lat <= bounds.max_lat;
    !inside && !online_failed
}

/// [`MapApp::active_archive`] over fields, for a caller that holds another
/// field of the app mutably (the frame's renderer).
fn active_archive(source: Option<crate::styles::StyleSource>, schema: TileSchema, archive: &Option<BasemapArchive>) -> Option<&BasemapArchive> {
    match (source, schema) {
        (None, TileSchema::Omt) => archive.as_ref(),
        _ => None,
    }
}

/// Every credit this build's map owes, collected by the engine's one rule
/// (`attribution::collect`, as native's `MapView::attributions`): the
/// basemap's (the schema in force: OpenFreeMap's own credit, or Mapbox's
/// as its TileJSON states it), the imagery's when `GPUI_MAP_IMAGERY` is
/// set (its credit is not known to this build, and the plate says so
/// rather than name a rightsholder it cannot verify -- the browser's
/// `UNKNOWN_SOURCE_CREDIT` rule), and each dot archive's own.
fn ios_credits(
    schema: TileSchema,
    source: Option<crate::styles::StyleSource>,
    archive: Option<&BasemapArchive>,
    raster: &RasterConfig,
    dots: &crate::overlay::host::DotLayers,
    extra: &[String],
    data: Option<&str>,
) -> Vec<crate::attribution::Credit> {
    use crate::attribution::{Credit, CreditRole};
    let basemap = match (source, archive, schema) {
        (Some(s), _, _) => s.credit,
        // Online tiles outside the archive: OpenFreeMap's credit, which
        // already names OpenMapTiles and OpenStreetMap.
        (None, Some(a), TileSchema::Omt) if a.online_outside => "OpenFreeMap \u{a9} OpenMapTiles Data from OpenStreetMap",
        (None, Some(a), _) => a.credit.as_str(),
        (None, None, TileSchema::Omt) => "OpenFreeMap \u{a9} OpenMapTiles Data from OpenStreetMap",
        (None, None, TileSchema::MapboxComposite) => "\u{a9} Mapbox \u{a9} OpenStreetMap",
    };
    // The host's imagery states its own credit; the environment's does not
    // (and the plate says so); a style's own raster source (OpenFreeMap's
    // shaded relief) is covered by the basemap's credit unless it names one.
    let imagery = match (&raster.credit, raster.template.is_empty()) {
        (Some(c), false) => Some(Credit::new(CreditRole::Imagery, c.clone())),
        (None, false) if raster.template == raster_config_env().template => Some(Credit::new(CreditRole::Imagery, "Imagery: GPUI_MAP_IMAGERY (credit not stated by this build)")),
        _ => None,
    };
    // A custom archive credit is still owed while the online tiles draw
    // around it; the default one is inside OpenFreeMap's.
    let archive_credit = archive.filter(|a| a.online_outside && source.is_none() && a.credit != BasemapArchive::OMT_CREDIT).map(|a| Credit::new(CreditRole::Basemap, a.credit.clone()));
    crate::attribution::collect(
        std::iter::once(Credit::new(CreditRole::Basemap, basemap))
            .chain(archive_credit)
            .chain(imagery)
            .chain(dots.attributions().into_iter().map(|c| Credit::new(CreditRole::Overlay, c)))
            .chain(extra.iter().map(|c| Credit::new(CreditRole::Overlay, c.clone())))
            // The host's data credit (the satellites' "Orbits: CelesTrak /
            // US Space Force"), as native's `MapView::set_data_credit`.
            .chain(data.map(|c| Credit::new(CreditRole::Overlay, c))),
    )
}

/// Derives the Mapbox `composite` source's raw-tile-API URL template from
/// the vendored `mapbox-streets-style.json`'s `sources.composite.url`,
/// through `crate::basemap::mapbox_composite_tile_url` -- the derivation
/// the browser shell uses too, so the two cannot build it differently.
///
/// Never logs `token` or the URL this returns (which embeds it) -- the one
/// caller ([`TileSchema::resolve`]) only ever logs whether this succeeded.
fn mapbox_tile_url_template(token: &str) -> anyhow::Result<String> {
    // The one derivation, shared with the browser shell -- see
    // `crate::basemap::mapbox_composite_tile_url`.
    crate::basemap::mapbox_composite_tile_url(crate::styles::MAPBOX_STYLES[0].json, token)
}

// The styles this build carries live in [`crate::styles`], one per entry of
// [`crate::styles::STYLES`] (OpenMapTiles schema) or
// [`crate::styles::MAPBOX_STYLES`] (Mapbox composite schema, see
// [`TileSchema`]), all EMBEDDED with `include_bytes!` rather than fetched
// or bundled. That module carries the reasoning for the sets; what belongs
// here is only what a reader of the FRAME loop needs:
//
// * [`MapApp::style`] is whichever of them is live, recompiled in place by
//   [`MapApp::cycle_style`] when three fingers land. There is no restart and
//   no refetch -- `decoded` survives a switch untouched, and `bake_some`
//   rebuilds every mesh from the MVT bytes already in hand.
// * `sprite` and `glyphs` in both documents are INERT on this platform,
//   and not because a token is missing. This shell has no symbol pass at
//   all: nothing here calls `style::match_tile_symbols`, so an `icon-image`
//   resolves to a name no code ever looks up, and `sprite::resolve_sprite_url`
//   is never reached from iOS on any style. Repointing a `sprite:` URL would
//   therefore change nothing on device; see `ios/STYLE-SWITCHER.md` for what
//   actually stands between this build and a drawn icon.
// * No labels either, for a different reason one layer down.
//   `font_fallback`'s `new_font_system` calls fontdb's `load_system_fonts`,
//   which has no iOS branch and takes the LINUX path to directories that do
//   not exist in an app sandbox (see `lib.rs`). Every symbol layer in every
//   style here is inert on device, which is why they were compared on
//   fills and lines alone.

// The camera's zoom range is `crate::camera_control::MIN_ZOOM..=MAX_ZOOM`,
// shared with native and the browser -- see those constants' doc comments.
// The ceiling is past every source's ceiling this build knows of --
// `TileSchema::source_max_zoom` is 14 (OMT) or 16 (Mapbox composite) -- so
// the top few levels are OVERZOOM: the ceiling's own tiles magnified, with
// the style still resolved at the camera's own zoom bucket. That is
// exactly what native does.

/// How long one frame may spend tessellating, and how many tiles it may take
/// in the attempt.
///
/// The budget is the real bound; the count is a guard, so a frame facing a
/// hundred trivial tiles cannot spend the whole of it on allocator churn.
/// That is the reverse of what stood here, which was `BAKES_PER_FRAME = 2`
/// and nothing else -- `[FROM WASM]`, where the browser rationed the bake by
/// COUNT because it had no clock it trusted.
///
/// A count is the wrong unit, and this shell now has the numbers to say why:
/// `match_tile` + `tessellate_plan` is ~15 ms for a dense city tile and well
/// under 1 ms for an empty ocean one, so "two tiles" is anything between
/// 0.2 ms and 30 ms of a frame that the display wants delivered in 8.33.
/// Thirty milliseconds is three and a half missed frames, and it lands
/// exactly when the user is doing the thing that provokes it -- crossing an
/// integer zoom, which re-bakes every visible tile at once.
///
/// Organic Maps meters the same work the same way, and its constant is the
/// direct ancestor of this one (`libs/drape_frontend/frontend_renderer.cpp`):
///
/// ```text
/// double constexpr kVSyncInterval = 0.06;
/// // Metal/Vulkan rendering is fast, so we can decrease sync inverval.
/// double constexpr kVSyncIntervalMetalVulkan = 0.03;
/// ```
///
/// checked as `syncInterval - elapsed <= 0` between units of background work.
/// Theirs is 30 ms because it is spent on a dedicated render thread between
/// two 60 Hz frames; this one is spent on the frame itself, after the present
/// (see `MapApp::frame`), inside the slack before the next drawable frees.
/// At 120 Hz that slack is one 8.33 ms period less the ~1 ms the rest of the
/// frame measures, so six is the honest number here rather than thirty.
///
/// The budget is checked BETWEEN tiles and never inside one: a tessellation
/// in progress cannot be suspended, so a single dense tile still overruns.
/// That is the remaining stutter and the count above is not what fixes it --
/// moving the bake off-thread is, and it stays the first thing phase 2 should
/// do. The `PaintPlan` and `TileMesh` have to cross a thread boundary and
/// `TileBuffers` cannot, because the upload must come back to the queue.
const BAKE_BUDGET_MS: f64 = 6.0;

/// Frame-thread time `decode_arrived` may spend on `mvt::decode` /
/// `raster::decode_tile` per frame, checked between tiles (the first is
/// unconditional, so progress never halts). A typical tile decodes in ~1 ms,
/// so this is ~4 tiles a frame -- 240/s at 60 Hz, above what eight workers
/// fetch on any real link, so the buffer does not grow while the camera is
/// still; a dense Manhattan z14 tile (~8-10 ms on device) gets one a frame.
/// Before this budget a frame decoded EVERY arrival, and after a tilted pan
/// that was hundreds of stale tiles and a multi-second stall.
const DECODE_BUDGET_MS: f64 = 4.0;
const BAKES_PER_FRAME: usize = 8;

/// Upper bound on retained GPU tiles, so a long pan does not grow the buffer
/// set without limit. A bound on the DISCARD PILE, not on what is drawing:
/// eviction never touches the retained set (cover + stand-ins + coarse
/// cover).
const MAX_GPU_TILES: usize = 96;

/// Upper bound on the decoded-MVT discard pile -- `web/src/app.rs`'s
/// `MAX_DECODED_TILES`, same value, same reason: a decoded tile is only
/// needed to bake or re-bake, a mesh to draw, so this is the smaller of the
/// two. Like [`MAX_GPU_TILES`] it never drops a retained or resident tile.
const MAX_DECODED_TILES: usize = 64;

/// The crude cap on `MapApp::raster_tiles` -- `web/src/app.rs`'s
/// `MAX_RASTER_TILES`, same value, same reason: unlike `MAX_GPU_TILES`'s
/// `retain`, which spares whatever `bake_some` last touched, this is a
/// `HashMap` with no insertion order to evict by, so it drops everything
/// outside the immediate draw list the moment the cap is crossed. Free on
/// every launch that never sets `GPUI_MAP_IMAGERY`.
const MAX_RASTER_TILES: usize = 48;

/// How many zoom levels below the tile cover the coarse prefetch cover sits.
/// maplibre-native's `DEFAULT_PREFETCH_ZOOM_DELTA`.
const PREFETCH_ZOOM_DELTA: u8 = 4;

/// How far below an uncovered tile `covering_descendants` looks. 3, matching
/// both MapLibre engines.
const MAX_DESCENDANT_DEPTH: u8 = 3;

// ---------------------------------------------------------------------------
// Gesture constants. There is no browser equivalent for any of these: a
// mouse has a wheel and a double-click, and a touchscreen has neither.
// ---------------------------------------------------------------------------

/// A touch that lifts within this long, having moved less than
/// [`TAP_SLOP_PT`], is a tap rather than the end of a drag.
const TAP_MAX_MS: f64 = 300.0;
const TAP_SLOP_PT: f64 = 12.0;
/// Two taps closer together than this in time and space are a double tap.
/// 300 ms is UIKit's own double-tap window.
const DOUBLE_TAP_MAX_MS: f64 = 300.0;
const DOUBLE_TAP_SLOP_PT: f64 = 44.0;
/// A double tap zooms in by exactly one level, which is what every map app
/// on the platform does.
const DOUBLE_TAP_ZOOM: f64 = 1.0;

/// How long a one-finger drag segment that took over from a dropped pinch
/// must have been running, at release, before its velocity window is
/// trusted to fling. See `post_pinch_drag_is_trustworthy` for why this
/// exists and why it is scoped to just that case.
const MIN_POST_PINCH_DRAG_MS: f64 = 50.0;

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

/// `[FROM WASM]` -- the coarsest COMPLETE generation of descendants of
/// `tile` that `has_mesh` accepts, or `None`. Complete, not partial:
/// drawing 1 of 4 children leaves the other three quarters a bare hole,
/// which is worse than magnifying an ancestor.
fn covering_descendants(tile: TileId, source_max_zoom: u8, has_mesh: impl Fn(TileId) -> bool) -> Option<Vec<TileId>> {
    let max_depth = MAX_DESCENDANT_DEPTH.min(source_max_zoom.saturating_sub(tile.z));
    for depth in 1..=max_depth {
        let gen = tile_descendants(tile, depth);
        if gen.iter().all(|d| has_mesh(*d)) {
            return Some(gen);
        }
    }
    None
}

/// The caller overlays' share of a frame: index builds, point views and
/// overlay tile bakes run inline until this is spent (at least one bake a
/// frame). This shell has threads, but its run loop is single-threaded
/// winit; a worker pool behind `OverlayHost::next_bake`/`land_bake` is the
/// follow-up (see the overlays spec).
const OVERLAY_BUDGET_PER_FRAME: std::time::Duration = std::time::Duration::from_millis(4);

/// `[FROM WASM]` -- what one frame decided: what to draw, what to keep, and
/// what it cost.
#[derive(Default)]
struct FramePlan {
    /// Coarse-first, so the ideal tiles paint over their own stand-ins. All
    /// geometry is coplanar at z=0 and the pipeline is `LessEqual`, so draw
    /// order IS the tiebreaker.
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
    fresh: u32,
    stale: u32,
    descendant_stand_ins: u32,
    ancestor_stand_ins: u32,
    holes: u32,
}

// ---------------------------------------------------------------------------
// Frame timing.
//
// The status line further down prints a cumulative frame COUNT, which
// answers "is it drawing" and nothing else. A count cannot tell 60 Hz from
// 120 Hz without a stopwatch, and it hides one 30 ms frame inside a second's
// worth of good ones. What matters on a ProMotion phone is the DISTRIBUTION
// of the interval between presents, and how long a finger waits for one.
//
// Both are measured rather than inferred, because the thing they were added
// to test -- `CADisableMinimumFrameDurationOnPhone` -- is an Info.plist key
// with no API to read back and no error if it is absent, misspelled or
// ignored. The only honest confirmation that it took is a measured 8.3 ms
// period where there had been a 16.7 ms one.
// ---------------------------------------------------------------------------

/// One second's worth of millisecond samples, summarised and then dropped.
///
/// A `Vec` rather than a running mean, because the mean is the least
/// interesting number here: a loop that misses one present in ten still
/// averages 108 fps, and only the tail says so.
#[derive(Default)]
struct MsWindow(Vec<f64>);

impl MsWindow {
    fn push(&mut self, ms: f64) {
        self.0.push(ms);
    }

    /// `(n, mean, p50, p95, max)`, or `None` if nothing was sampled.
    ///
    /// Sorts in place, and every caller calls [`MsWindow::clear`]
    /// immediately after, so the order it destroys is never read again. At
    /// 120 Hz the window holds about 120 `f64`s and the sort is free.
    fn summary(&mut self) -> Option<(usize, f64, f64, f64, f64)> {
        if self.0.is_empty() {
            return None;
        }
        self.0.sort_by(f64::total_cmp);
        let n = self.0.len();
        let mean = self.0.iter().sum::<f64>() / n as f64;
        // Nearest-rank, so every quantile printed is a sample that really
        // happened rather than an interpolation between two that did.
        let at = |q: f64| self.0[(((n as f64) * q).ceil() as usize).clamp(1, n) - 1];
        Some((n, mean, at(0.50), at(0.95), self.0[n - 1]))
    }

    fn clear(&mut self) {
        self.0.clear();
    }

    /// The window sorted into the refresh periods a 120 Hz panel can
    /// actually produce: `(120, 90, 60, 40 or slower, stalled)`.
    ///
    /// A mean and a p50 cannot tell "the display is running at 60" from "the
    /// display is running at 120 and we are missing every other frame" --
    /// both land near 16.7 ms, and they have completely different fixes.
    /// Bucketing does tell them apart: the first is all 60s, the second is a
    /// mix of 8.3s and 25s. Boundaries sit midway between the nominal
    /// periods (8.33, 11.11, 16.67, 25.0 ms), so jitter of a millisecond or
    /// two does not migrate a sample into the next bucket.
    fn refresh_buckets(&self) -> (usize, usize, usize, usize, usize) {
        let mut b = (0, 0, 0, 0, 0);
        for &ms in &self.0 {
            match ms {
                _ if ms < 9.7 => b.0 += 1,
                _ if ms < 13.9 => b.1 += 1,
                _ if ms < 20.8 => b.2 += 1,
                _ if ms < 100.0 => b.3 += 1,
                _ => b.4 += 1,
            }
        }
        b
    }
}

// ---------------------------------------------------------------------------
// Pan inertia -- flick to fling, and glide to a stop.
//
// The SAMPLING here is still `[FROM WASM]` -- the ring buffer, the cutoff and
// the velocity window came from `map_view.rs:200-390`, itself a port of
// MapLibre GL JS's `src/ui/handler_inertia.ts`. The CURVE is no longer
// MapLibre's; it is Organic Maps'
// (`libs/drape_frontend/kinetic_scroller.cpp`), and the reason is a user
// report: "not so flying as omim".
//
// # Why the curve had to change and not just its numbers
//
// MapLibre brakes at a CONSTANT deceleration, which is a linear ramp of
// speed to zero -- a car stopping. Organic Maps decays EXPONENTIALLY,
// `1 - e^(-4t)` over a fixed 1.5 s -- a puck on ice. The two have different
// shapes and no choice of constants makes one the other: MapLibre's glide
// distance is quadratic in the release speed, `L*v^2 / (2*D)`, while Organic
// Maps' is linear, `0.4*v`. Fitting one to the other at one flick speed
// mismatches it at every other.
//
// The distances, computed from both papers' own formulas, for a flick at
// zoom 13 (this shell's start zoom), in logical points on a 430 x 932 pt
// screen:
//
// ```text
//   flick   MapLibre           Organic Maps
//   pt/s    dist     time      dist     time
//     300      5 pt  0.12 s     120 pt  1.5 s
//     600     22 pt  0.24 s     240 pt  1.5 s
//     800     38 pt  0.32 s     320 pt  1.5 s
//    1600    154 pt  0.64 s     640 pt  1.5 s
//    3200    614 pt  1.28 s    1280 pt  1.5 s
//    4800   1307 pt  1.87 s    1360 pt  1.5 s   <- both at their ceiling
// ```
//
// An ordinary flick is 600-800 pt/s. MapLibre moves the map by a tenth of
// the screen's width and stops in a third of a second; Organic Maps carries
// it three quarters of a screen over a second and a half. That is the whole
// of "not so flying", and it is an eight- to elevenfold difference, not a
// tuning nuance.
//
// # What is NOT taken from Organic Maps
//
// Their velocity comes off the map CENTRE's pixel displacement over a 50 ms
// window (`KineticScroller::Update`, `kTimeWindowSec`); this shell keeps its
// own finger-sample buffer and its 60 ms window. Two reasons. The measured
// quantity is the same -- a one-finger drag moves the centre exactly as far
// as the finger, in the opposite direction -- so switching would change
// nothing but the plumbing. And the 60 ms window is load-bearing elsewhere:
// `post_pinch_drag_is_trustworthy` and its regression tests are written
// against these samples (see `ios/PINCH-FLING-FIX.md`), and a curve change
// is not the moment to disturb the guard that stops a released pinch
// flinging the map.
//
// That guard matters MORE now, not less. The old ceiling was 1400 pt/s; the
// new one reaches 4600. An artefactual velocity window that used to produce
// a 344 pt fling would now produce nearly a thousand. The gate is what stops
// it, it is unchanged, and its test is unchanged.
//
// The clock is still the one deviation from the browser version, and it
// still changes back toward native: `Event::time_stamp()` becomes elapsed
// milliseconds off an `Instant`. The browser needed the event's own
// timestamp because Chrome coalesces pointer motion into one event per
// frame; UIKit delivers touch moves as they arrive, so reading the clock in
// the handler is honest here.
// ---------------------------------------------------------------------------

/// How far back the drag-sample buffer keeps history. MapLibre's `cutoff`.
const INERTIA_BUFFER_CUTOFF_MS: f64 = 160.0;
/// How much of the retained history release velocity is measured over.
/// SHORTER than the buffer on purpose: a drag that curves or slows at the
/// end must fling along its FINAL direction.
const INERTIA_VELOCITY_WINDOW_MS: f64 = 60.0;

/// How long a fling runs, and how sharply it decays inside that time.
/// Organic Maps' `kKineticDuration` and `kKineticFadeoff`.
///
/// The duration is FIXED: a gentle flick and a hard one both take 1.5 s.
/// That reads as wrong and is not -- the decay does the work. The travelled
/// fraction is `1 - e^(-4t)`, so the glide is 63% done at 0.375 s, 86% at
/// 0.75 s, and 98.2% by the time the clock runs out; the last 1.8% is taken
/// up in one step (see `pan_inertia_step`'s `t >= 1.0` arm), exactly as
/// their animation system does when `IsFinished` clamps `GetT` to 1.
/// A weak fling therefore LOOKS short because it covers little ground, not
/// because it is cut off early.
const KINETIC_DURATION_SECS: f64 = 1.5;
const KINETIC_FADEOFF: f64 = 4.0;

/// The fraction of one second's travel at the release velocity that the
/// glide actually covers. Organic Maps' `kKineticAcceleration`, and the
/// whole of their distance law: `CreateKineticAnimation` builds `offset` as
/// the ground covered in one second at `velocity`, then keeps 0.4 of it.
const KINETIC_ACCELERATION: f64 = 0.4;

/// Below this release speed there is no fling at all. Organic Maps'
/// `kInstantVelocityThreshold`.
///
/// Their constant is 200 "generic pixels per second, should multiply on
/// visual scale", and on an @3x iPhone `GetVisualScale()` is 3 while touch
/// coordinates are physical pixels -- so `200 * vs` physical px/s is 200
/// LOGICAL pt/s, which is the unit this shell works in. Every Organic Maps
/// speed below converts the same way and needs no scale factor here.
///
/// This floor is new: MapLibre had none, and dropped a weak fling only
/// afterwards, via [`MIN_PAN_INERTIA_OFFSET_PX`], once the arithmetic had
/// already produced a few pixels of glide. A floor on the INPUT is the
/// better shape -- it says "that was a stop, not a throw" -- and it is a
/// second line of defence for the released-pinch artefact.
const INSTANT_VELOCITY_THRESHOLD_PT_S: f64 = 200.0;

/// The release-speed ceiling, LERPed by zoom. Organic Maps'
/// `kKineticMaxSpeedStart`/`End` and `CalculateKineticMaxSpeed`.
///
/// The ceiling is LOWER at world view and HIGHER at street level, which is
/// the opposite of the intuition that a wide view should fling further, and
/// it is right. Velocity is measured in PIXELS and the glide is spent in
/// pixels, so a 5000 pt/s fling at z2 would cross an ocean per flick while
/// the same fling at z17 crosses a few streets. Their own comment on
/// `GetDirectionAndVelocity` says why the pixel space is deliberate: "We
/// need the same reaction on different zoom levels, and should calculate
/// velocity on pixel space."
///
/// ```text
/// double CalculateKineticMaxSpeed(ScreenBase const & modelView)
/// {
///   double const lerpCoef = 1.0 - GetNormalizedZoomLevel(modelView.GetScale());
///   return kKineticMaxSpeedStart * lerpCoef + kKineticMaxSpeedEnd * (1.0 - lerpCoef);
/// }
/// ```
///
/// with `GetNormalizedZoomLevel` (`visual_params.cpp`) being
/// `clamp((zoom - 1) / (21 - 1), 0, 1)` -- so the coefficient RISES with
/// zoom and the ceiling rises with it: 1000 pt/s at z1, 3400 at z13, 4800 at
/// this shell's [`MAX_ZOOM`] of 20.
const KINETIC_MAX_SPEED_START_PT_S: f64 = 1000.0;
const KINETIC_MAX_SPEED_END_PT_S: f64 = 5000.0;
/// The denominator of Organic Maps' zoom normalisation:
/// `scales::GetUpperStyleScale() + 1`, i.e. 20 + 1. Kept as THEIR number and
/// not swapped for this shell's [`MAX_ZOOM`], so the curve a user feels at a
/// given zoom is the curve they feel at that zoom.
const KINETIC_ZOOM_NORMALISATION_MAX: f64 = 21.0;
const KINETIC_ZOOM_NORMALISATION_MIN: f64 = 1.0;

/// Glides shorter than this are dropped entirely.
const MIN_PAN_INERTIA_OFFSET_PX: f64 = 0.5;

/// Organic Maps' `CalculateKineticMaxSpeed`, in logical points per second.
fn kinetic_max_speed(zoom: f64) -> f64 {
    let normalised = ((zoom - KINETIC_ZOOM_NORMALISATION_MIN)
        / (KINETIC_ZOOM_NORMALISATION_MAX - KINETIC_ZOOM_NORMALISATION_MIN))
        .clamp(0.0, 1.0);
    KINETIC_MAX_SPEED_START_PT_S * (1.0 - normalised) + KINETIC_MAX_SPEED_END_PT_S * normalised
}

#[derive(Clone, Copy, Debug)]
struct DragSample {
    at_ms: f64,
    x: f32,
    y: f32,
}

/// A post-release pan glide in flight. `offset_*` is the TOTAL pixel
/// distance the glide will cover and `applied_*` is how much of it has
/// already been handed to `pan_by_pixels`; tracking the applied total,
/// rather than integrating a per-frame velocity, is what keeps the glide
/// from drifting when a frame runs long.
#[derive(Clone, Copy, Debug)]
struct PanInertia {
    started_ms: f64,
    duration_ms: f64,
    offset_x: f64,
    offset_y: f64,
    applied_x: f64,
    applied_y: f64,
}

fn drain_drag_samples(samples: &mut std::collections::VecDeque<DragSample>, now_ms: f64) {
    while samples.front().is_some_and(|s| (now_ms - s.at_ms).max(0.0) > INERTIA_BUFFER_CUTOFF_MS) {
        samples.pop_front();
    }
}

/// Picks the sample pair release velocity is measured across: the newest
/// sample, and the oldest one still inside `INERTIA_VELOCITY_WINDOW_MS`.
fn drag_velocity_window(
    samples: &[DragSample],
    released_ms: f64,
) -> Option<(DragSample, DragSample)> {
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

/// The whole velocity -> glide computation, as a pure function of numbers:
/// Organic Maps' `KineticScroller::GetDirectionAndVelocity` and
/// `CreateKineticAnimation`, with their mercator round trip removed.
///
/// They convert a pixel velocity to a mercator offset and back
/// (`PtoG(GtoP(center) + dir * velocity) - center`) because their animation
/// drives the map plane in mercator. This shell's glide is applied through
/// `pan_by_pixels`, so the offset stays in pixels and the round trip is not
/// merely unnecessary but harmful -- it would bake in the zoom at the moment
/// of release, and a fling that outlives a pinch would then travel the wrong
/// distance.
///
/// A single vector, not two independent axes. MapLibre's code ran the
/// arithmetic per axis and then reconciled the two durations by taking the
/// longer, which quietly bends a diagonal fling: the shorter axis finishes
/// its own easing early and then holds while the other catches up. Organic
/// Maps normalises the direction and scales one magnitude, so a diagonal
/// fling travels in a straight line. That is the better shape and it is free.
fn pan_inertia_curve(
    amount_x: f64,
    amount_y: f64,
    window_secs: f64,
    zoom: f64,
) -> Option<(f64, f64, f64)> {
    if window_secs <= 0.0 || !window_secs.is_finite() {
        return None;
    }
    let travelled = amount_x.hypot(amount_y);
    if travelled <= 0.0 {
        return None;
    }
    // "Most touch filtrations happen here" -- their comment, and the reason
    // the floor is checked BEFORE the ceiling: a release too slow to be a
    // throw must produce nothing, not a clamped something.
    let velocity = travelled / window_secs;
    if velocity < INSTANT_VELOCITY_THRESHOLD_PT_S {
        return None;
    }
    let velocity = velocity.min(kinetic_max_speed(zoom));
    let glide = KINETIC_ACCELERATION * velocity;
    let offset_x = amount_x / travelled * glide;
    let offset_y = amount_y / travelled * glide;
    if offset_x.hypot(offset_y) < MIN_PAN_INERTIA_OFFSET_PX {
        return None;
    }
    Some((offset_x, offset_y, KINETIC_DURATION_SECS * 1000.0))
}

fn pan_inertia_from_samples(
    samples: &[DragSample],
    released_ms: f64,
    zoom: f64,
) -> Option<PanInertia> {
    let (first, last) = drag_velocity_window(samples, released_ms)?;
    let window_secs = (last.at_ms - first.at_ms).max(0.0) / 1000.0;
    let amount_x = (last.x - first.x) as f64;
    let amount_y = (last.y - first.y) as f64;
    let (offset_x, offset_y, duration_ms) =
        pan_inertia_curve(amount_x, amount_y, window_secs, zoom)?;
    Some(PanInertia {
        started_ms: released_ms,
        duration_ms,
        offset_x,
        offset_y,
        applied_x: 0.0,
        applied_y: 0.0,
    })
}

/// Whether a release's velocity window is trustworthy enough to fling from,
/// for a one-finger drag segment that took over from a dropped pinch --
/// `false` for the first [`MIN_POST_PINCH_DRAG_MS`] of that segment's life.
///
/// # The bug this guards
///
/// `begin_drag` restarts pan-drag sampling from whichever finger survives a
/// pinch, at THAT finger's current position (see its own doc comment). If
/// the two lifts are staggered by even a few milliseconds -- the ordinary
/// case, not an edge case -- the surviving finger is still travelling
/// RADIALLY, at pinch speed, when its neighbour lets go. `on_touch` samples
/// that motion as a pan, `drag_velocity_window` is satisfied by as few as
/// two samples a couple of milliseconds apart, and `pan_inertia_axis`
/// divides by that near-zero window and clamps straight to
/// `PAN_INERTIA_MAX_SPEED` -- a full-speed fling in whatever direction the
/// finger happened to be moving when it was let go. That is exactly what
/// was reported: a pinch that "sometimes activates acceleration into one
/// direction like inertial mechanism."
///
/// # Why this is not a port of MapLibre, and what it takes instead
///
/// MapLibre GL JS's `TouchPanHandler` (`src/ui/handler/touch_pan.ts`)
/// carries no equivalent bug, but for two reasons that do not transfer to
/// this engine:
///
/// 1. It tracks a CENTROID -- the average delta of every touch currently
///    down -- continuously across a touch-count change, rather than
///    restarting from one anchor finger. A symmetric pinch averages to
///    near-zero pan, so little to nothing is recorded while both fingers
///    are still down; this engine tracks one anchor finger, not a
///    centroid (see `pinch_state`'s own two-finger midpoint, which is a
///    separate code path), so there is no such averaging to fall back on.
/// 2. Its `HandlerInertia` buffer (`handler_inertia.ts`) is never cleared
///    on a touch-count change either, only on the final release, AND the
///    browser coalesces `touchmove` to one event per animation frame --
///    so its shortest possible velocity window is a frame wide (~16ms).
///    UIKit delivers touch moves as they arrive (this file's inertia
///    section preamble), which is exactly the deviation that makes a
///    several-times-shorter, artefactual window possible here.
///
/// Porting MapLibre's centroid tracking would still leave the
/// frame-coalescing floor missing, so it would not by itself close this
/// gap on this platform. A hard rule -- any gesture that was ever
/// multi-touch never flings -- would close it, but it also kills "pinch,
/// lift one finger, keep dragging, flick", a real gesture every touch map
/// app supports. This threshold is the narrower cut: it blocks only a
/// velocity window measured in the first instants after a pinch drops to
/// one finger, which is exactly when the sample is most likely to be
/// residual pinch motion rather than an intentional drag. A genuine
/// continued drag clears 50ms almost immediately and flings exactly as
/// before; a fresh one-finger touch (never part of a pinch) is not gated
/// at all, so an ordinary quick flick is untouched.
fn post_pinch_drag_is_trustworthy(segment_started_ms: f64, released_ms: f64) -> bool {
    released_ms - segment_started_ms >= MIN_POST_PINCH_DRAG_MS
}

/// One frame of a glide: the pixel delta to hand `pan_by_pixels`, and
/// whether this was the last frame. The final frame applies exactly the
/// un-applied remainder, so the glide lands precisely on `offset`.
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
    // Organic Maps' `KineticScrollAnimation::GetProperty`:
    //
    //     value = m_endPos - m_direction * exp(-kKineticFadeoff * GetT());
    //
    // stated as a travelled FRACTION rather than as a position, because this
    // shell accumulates offsets through `pan_by_pixels` and has no absolute
    // end position to subtract from. The two are the same curve: their
    // `endPos - direction * e^(-4t)` is `startPos + direction * (1 - e^(-4t))`.
    //
    // At `t == 1` this is 0.9817, not 1 -- the animation lands 1.8% short and
    // their system snaps it to `GetTargetProperty`'s `m_endPos`. The branch
    // above is that snap, and it is why the residual is taken in one step
    // rather than smeared into the curve.
    let eased = 1.0 - (-KINETIC_FADEOFF * t).exp();
    (
        (inertia.offset_x * eased - inertia.applied_x, inertia.offset_y * eased - inertia.applied_y),
        false,
    )
}

/// The backing store's size in physical pixels, and the logical-to-physical
/// scale that produced it.
///
/// `[FROM WASM]`, where it existed because WebGL2's downlevel
/// `max_texture_dimension_2d` of 2048 is smaller than a Retina browser
/// window. Metal's is 16384 and no phone comes close, so this clamp never
/// bites on iOS -- it is kept because the arithmetic is the same and a
/// silent `Surface::configure` validation panic is not a failure mode worth
/// re-earning on a future device.
fn backing_store_size(
    logical_width: f32,
    logical_height: f32,
    scale_factor: f32,
    max_dim: u32,
) -> (u32, u32, f32) {
    let max_dim = max_dim as f32;
    let scale =
        scale_factor.min(max_dim / logical_width).min(max_dim / logical_height).max(0.1);
    let width = ((logical_width * scale).round() as u32).max(1);
    let height = ((logical_height * scale).round() as u32).max(1);
    (width, height, scale)
}


/// One tile's GPU residency: its buffers, its model uniform, and the zoom
/// its `PaintPlan` was resolved at.
///
/// `baked_zoom` is the whole reason this is not just a `TileBuffers`. Paint
/// is resolved per integer zoom bucket (`style::match_tile`'s `zoom`
/// argument), so crossing a bucket makes every mesh stale even though the
/// geometry did not move.
struct GpuTile {
    baked_zoom: u8,
    buffers: TileBuffers,
    model_buffer: TrackedBuffer,
    model_bind_group: wgpu::BindGroup,
    /// This tile's symbol plan, baked in the SAME `bake_some` pass as
    /// `buffers` and at the same `baked_zoom` -- `web/src/app.rs`'s
    /// `GpuTile::symbols`, an `Rc` there and an `Arc` here for the same
    /// reason every other shared engine value on this crate is an `Arc`:
    /// no thread boundary needs it, but `style::Style` (which this frame's
    /// `symbol_pass::SymbolFrame` also borrows from) already commits this
    /// crate to `Arc` over `Rc`.
    symbols: Arc<crate::style::SymbolPlan>,
    /// The plan's `fill-extrusion` roofs and walls
    /// (`render::extrusion::tessellate_extrusions`), uploaded in the same
    /// bake as `buffers` -- `web/src/app.rs`'s `GpuTile::extrusion`, itself
    /// `MapView`'s `CachedTile::extrusion`. `None` when the tile has no
    /// buildings. A buildings-only tile still gets its (empty) flat buffers:
    /// this shell never skips an empty flat mesh, so `MapView`'s
    /// `draws_anything` gate is already the behaviour here.
    extrusion: Option<crate::render::extrusion_gpu::ExtrusionGpu>,
}

/// One decoded imagery tile's GPU residency: its texture, and the two bind
/// groups the raster pass draws it with. `[FROM WASM]` -- `web/src/app.rs`'s
/// `HeldRasterTile`, itself `map_view.rs`'s, minus the byte-budget cache it
/// lives in there: this crate bounds `MapApp::raster_tiles` with
/// [`MAX_RASTER_TILES`] instead, the same crude cap `MAX_GPU_TILES` already
/// is for the vector meshes.
struct HeldRasterTile {
    texture: crate::render::raster_pipeline::RasterTexture,
    /// Which shared square (`render::raster_mesh`) this tile draws with --
    /// looked up in `MapApp::raster_squares`, not held here.
    mesh_key: crate::render::raster_mesh::RasterMeshKey,
    model_buffer: TrackedBuffer,
    model_bind_group: wgpu::BindGroup,
    paint_bind_group: wgpu::BindGroup,
    /// Held only so the buffer the bind group points at stays alive; never
    /// read directly. Native and the browser both call the same field
    /// `_paint_buffer` for the same reason.
    _paint_buffer: TrackedBuffer,
    /// The `raster-opacity` `paint_bind_group` was built with, so a style
    /// whose resolved opacity actually moves rewrites the bind group and a
    /// constant one (every style this crate cycles) never does.
    paint_opacity: f32,
}

/// The ocean sphere's GPU residency: `render::space::backdrop_mesh` for one
/// background-colour PAIR, plus the model uniform that places it.
///
/// `[FROM WASM]` -- `web/src/app.rs`'s `Backdrop`, which is itself
/// `map_view.rs`'s own `Backdrop` shape. `color` is the rebuild key -- the
/// PAIR, not a lerped colour, so a zoom-expression `background-color` moves
/// the model uniform every frame of a zoom rather than re-tessellating
/// 4,225 vertices on every one of them.
struct Backdrop {
    color: (crate::style::Rgba, crate::style::Rgba),
    buffers: TileBuffers,
    model_buffer: TrackedBuffer,
    model_bind_group: wgpu::BindGroup,
}

/// A single Latin subset face, `include_bytes!`-embedded rather than
/// fetched -- the same 71 KB OFL asset `web/src/app.rs`'s `load_font` fetches
/// over the network at runtime (`FONT_URLS`), baked into this binary
/// instead because glyph shaping needs it before there is a style, a
/// schema, or a token to resolve a fetch against -- unlike the bundled
/// sprite sheet below, which is also embedded but is upgraded over the
/// network for one style (see [`EMBEDDED_SPRITE_JSON`]). See `ios/src/lib.rs`'s
/// `font_fallback` comment for why `new_font_system`'s system-font scan is
/// the wrong tool here regardless of how the bytes arrive: `fontdb` has no
/// iOS branch and would come back empty.
///
/// Deliberately ONE face, not the four `FONT_URLS` carries. `Noto Sans
/// Bold`/`Noto Sans Italic`/`Noto Sans JP` -- every `text-font` the OMT
/// styles (`styles.rs`) name -- fall back to this one through
/// `font_fallback::resolve_font_stack_with_default`'s installed-family
/// check, which is the same degradation a style asking for a face neither
/// build shipped already gets. `mapbox-streets-v8` (also cycled by
/// `styles.rs`) exercises the identical path for a different reason: its
/// `text-font` never names Noto Sans at all -- it is Mapbox's own
/// commercial `DIN Pro`, with `Arial Unicode MS` as a coverage tail (see
/// `font_fallback::COVERAGE_ONLY_FAMILIES`) -- so every one of its labels
/// falls through to [`FONT_FAMILY`] exactly the way an unshipped Noto Sans
/// weight does; `font_fallback`'s
/// `every_compiled_mapbox_streets_symbol_layer_resolves_to_the_embedded_face`
/// pins it against the real vendored style. CJK text draws as tofu on this
/// build; nothing these styles name reaches a script this one Latin subset
/// does not cover.
pub(crate) const EMBEDDED_FONT: &[u8] = include_bytes!("../../assets/fonts/NotoSans-Regular.latin-subset.ttf");

/// The sprite sheet every OMT-schema style's `icon-image` names resolve
/// against (`default-N`, `road_N`, the one-way arrow, ...) -- Maki icons
/// repacked for OpenMapTiles' shield/POI vocabulary, already vendored at
/// `ios/assets/japan-sprite@2x.{json,png}` (see that directory's own
/// `PROVENANCE.md`/`LICENSE-MAKI.txt`) but never loaded until this task.
/// `include_bytes!`-embedded for the same reason [`EMBEDDED_FONT`] is: it
/// has to be there before there is a window, let alone a network. See
/// [`MapApp::create_renderer`]'s own doc for how this is packed and
/// uploaded, and for the ONE style ([`crate::styles::MAPBOX_STYLES`]'
/// `mapbox-streets-style.json`) whose real Mapbox art replaces it at
/// runtime instead, over `net::Fetcher`, when a token was baked in.
const EMBEDDED_SPRITE_JSON: &[u8] = include_bytes!("../assets/japan-sprite@2x.json");
/// The packed atlas [`EMBEDDED_SPRITE_JSON`] indexes. See that constant's
/// own doc comment.
const EMBEDDED_SPRITE_PNG: &[u8] = include_bytes!("../assets/japan-sprite@2x.png");

/// Parses and slices [`EMBEDDED_SPRITE_JSON`]/[`EMBEDDED_SPRITE_PNG`] into a
/// [`crate::sprite::SpriteAtlas`], the same two calls
/// `sprite::fetch_sprite_atlas` makes on native/the browser's fetched
/// bytes (`parse_sprite_manifest` then `slice_atlas` -- no second parser).
/// Bytes already in the binary, so this never fails on anything but a
/// corrupt asset; a failure here costs icons and nothing else -- the map
/// draws with an empty atlas exactly as it did before this task, and says
/// so on stderr rather than panicking.
fn load_embedded_sprites() -> crate::sprite::SpriteAtlas {
    match crate::sprite::parse_sprite_manifest(EMBEDDED_SPRITE_JSON)
        .and_then(|manifest| crate::sprite::slice_atlas(EMBEDDED_SPRITE_PNG, &manifest))
    {
        Ok(atlas) => atlas,
        Err(err) => {
            crate::mlog!("[map] bundled sprite sheet failed to parse, icons will not draw: {err:#}");
            crate::sprite::SpriteAtlas::new()
        }
    }
}

/// This crate's whole label pass, gathered into one struct the way
/// `web/src/app.rs`'s `Labels` is: a `glyphon` text renderer for point
/// labels, plus the SDF glyph atlas and the `IconPipeline` that draws its
/// quads (and, since this task, the real icon sprite sheet's quads too --
/// see [`Renderer::icon_atlas_bind_group`]) -- native's `GpuState` fields
/// of the same names, one frame's worth at a time.
///
/// Built once in `Renderer::new`, not lazily behind an `Option` the way
/// `web/src/app.rs`'s `Labels` is: the font is `include_bytes!` rather
/// than fetched, so there is no "still loading" state to represent, and
/// unlike `star_pipeline`/`backdrop`/`atmosphere_pipeline` above (which
/// wait for a globe frame that may never come), a label is possible from
/// the very first tile.
struct Labels {
    font_system: glyphon::FontSystem,
    swash_cache: glyphon::SwashCache,
    /// Kept alive alongside `viewport`/`atlas`, which both borrowed it at
    /// construction -- `map_view.rs`'s `GpuState` keeps the same four
    /// glyphon values together for the same reason.
    _cache: glyphon::Cache,
    viewport: glyphon::Viewport,
    atlas: glyphon::TextAtlas,
    text_renderer: glyphon::TextRenderer,
    shape_cache: crate::symbol_pass::LabelShapeCache,
    /// Reset every frame to this frame's padded viewport bounds
    /// (`symbol_pass::symbol_collision_bounds`) before `place_visible_symbols`
    /// runs -- degenerate (`0,0,0,0`) until then, the same starting box
    /// `web/src/app.rs`'s own `collision` field uses.
    collision: crate::collision::CollisionIndex,
    /// The SDF text-glyph atlas: line-placed labels rasterise into this
    /// through `symbol_pass::GlyphSink`, and `glyph_atlas_bind_group` below
    /// is this atlas's pixels, uploaded through `IconPipeline` -- the same
    /// pipeline an icon sprite sheet would upload through, since one
    /// shader samples both (see `render::icon_pipeline`'s own doc comment).
    glyph_atlas: crate::render::GlyphAtlas,
    glyph_atlas_bind_group: Option<wgpu::BindGroup>,
    glyph_atlas_generation: Option<u64>,
}

impl Labels {
    /// Builds the font system from [`EMBEDDED_FONT`] and every other piece
    /// of GPU-resident label state, once, at `Renderer::new` time.
    fn new(ctx: &GpuContext, color_format: wgpu::TextureFormat) -> Labels {
        let mut db = glyphon::fontdb::Database::new();
        db.load_font_data(EMBEDDED_FONT.to_vec());
        // No locale query on iOS the way `web/src/app.rs` reads
        // `navigator.language` -- `sys_locale` (already a `font_fallback`
        // dependency, for the Han-unification path) is the portable
        // equivalent, and a fixed fallback matches what
        // `examples/basic_map.rs` does when no locale is available.
        let locale = sys_locale::get_locale().unwrap_or_else(|| "en-US".to_string());
        // Every other script from the phone's own fonts (`system_fonts`).
        let fonts = crate::system_fonts::load(&mut db, &locale);
        crate::log::emit_or_defer(format!(
            "[map] system fonts: {} files, {} faces in {:.0} ms; scripts with none: {:?}",
            fonts.files, fonts.faces, fonts.millis, fonts.missing_scripts
        ));
        let font_system = crate::font_fallback::new_font_system_from_db(locale, db);
        let swash_cache = glyphon::SwashCache::new();
        let cache = glyphon::Cache::new(&ctx.device);
        let viewport = glyphon::Viewport::new(&ctx.device, &cache);
        // `ColorMode::Web`, matching `map_view.rs`'s and `web/src/app.rs`'s
        // own choice on an `Srgb` render target view over a `Unorm`
        // surface -- see either file's comment on why `Accurate` (glyphon's
        // default) double-applies the sRGB curve there.
        let mut atlas = glyphon::TextAtlas::with_color_mode(
            &ctx.device,
            &ctx.queue,
            &cache,
            color_format,
            glyphon::ColorMode::Web,
        );
        let text_renderer =
            glyphon::TextRenderer::new(&mut atlas, &ctx.device, wgpu::MultisampleState::default(), None);
        Labels {
            font_system,
            swash_cache,
            _cache: cache,
            viewport,
            atlas,
            text_renderer,
            shape_cache: crate::symbol_pass::LabelShapeCache::with_default_family(FONT_FAMILY),
            collision: crate::collision::CollisionIndex::new(crate::collision::CollisionBox {
                left: 0.0,
                top: 0.0,
                right: 0.0,
                bottom: 0.0,
            }),
            glyph_atlas: crate::render::GlyphAtlas::new(),
            glyph_atlas_bind_group: None,
            glyph_atlas_generation: None,
        }
    }
}

/// What every `text-font` stack in this crate's styles resolves to when the
/// stack's own faces are not installed -- `web/src/app.rs`'s `FONT_FAMILY`,
/// same value, same reason: it is the name [`EMBEDDED_FONT`]'s own face
/// table carries.
pub(crate) const FONT_FAMILY: &str = "Noto Sans";

/// One finger currently on the glass, in LOGICAL points.
#[derive(Clone, Copy, Debug)]
struct ActiveTouch {
    id: u64,
    x: f64,
    y: f64,
    /// Where it landed and when, so a lift can be classified as a tap.
    start_x: f64,
    start_y: f64,
    start_ms: f64,
    /// Set once the finger travels past [`TAP_SLOP_PT`]; a touch that has
    /// moved is never a tap, however briefly it was down.
    moved: bool,
}

/// Everything the GPU side needs, created in `resumed` once there is a
/// window to attach to. Split out from [`MapApp`] because on iOS the app can
/// be resumed after a suspend with the surface gone, and the map state
/// (camera, tiles, style) must outlive that.
struct Renderer {
    /// The winit shell's window, held so the surface's `'static` lifetime
    /// is honest. An embedding host owns its layer and holds it itself.
    #[cfg(feature = "winit-shell")]
    window: Option<Arc<winit::window::Window>>,
    ctx: GpuContext,
    surface: wgpu::Surface<'static>,
    /// Kept whole rather than rebuilt per resize: `format`, `view_formats`,
    /// `alpha_mode` and `present_mode` were chosen once against this
    /// adapter's capabilities, and a resize must change only width/height.
    surface_config: wgpu::SurfaceConfiguration,
    pipeline: TilePipeline,
    depth_view: wgpu::TextureView,
    /// One uniform per `fill-extrusion` layer of the frame's
    /// `render::frame_plan::extrusion_plan`, rewritten every frame --
    /// `MapView`'s `extrusion_params`.
    extrusion_params: crate::render::extrusion_gpu::ExtrusionParamsGpu,
    /// The UNORM sibling of whatever the backend prefers (when offered) --
    /// never an `*Srgb` view. See `create_renderer`'s surface-format comment.
    render_format: wgpu::TextureFormat,
    /// The route line's mesh while a trip is active (`render::nav_layer`);
    /// `None` otherwise, so a non-navigating frame makes no route GPU call.
    nav_route: Option<crate::render::nav_layer::RouteMeshGpu>,
    /// The chrome pass's pipelines, built on the first frame with chrome.
    nav_chrome: Option<crate::render::nav_layer::NavChromeGpu>,
    camera_buffer: TrackedBuffer,
    camera_bind_group: wgpu::BindGroup,
    /// The process's standing request for 120 Hz. Held, not leaked: it is
    /// live for exactly as long as there is a surface to draw into, and
    /// dropping it hands the display's cadence back to Core Animation. See
    /// `display_link.rs` for why a `CADisplayLink` is what asks.
    ///
    /// `None` on any screen that cannot do better than 60, where the request
    /// would be granted at 60 anyway and the link would be pure overhead.
    ///
    /// Paused and resumed by `sync_activity_state` rather than held live for
    /// the surface's whole life: an idle `CADisplayLink` still fires its
    /// (empty) callback every vsync, which is a real wake of the run loop
    /// for a frame that is never going to be drawn.
    frame_rate_hint: Option<crate::display_link::FrameRateHint>,

    // -- what the globe sits on ------------------------------------------
    //
    // Device-bound, like everything else above, so they live here and not
    // on `MapApp` -- and lazy, like `GpuState`'s native twins: a launch that
    // never reaches globe range must pay for none of the three.
    /// Built on the first frame with `SpacePlan::draws_stars`: 1.24 MB of
    /// static vertices for all 9,096 catalogue stars. See
    /// `render::space::StarPipeline`.
    star_pipeline: Option<crate::render::space::StarPipeline>,
    /// The altitude layers' dots and rings, built on the first frame that
    /// has any (no depth: the planet hides a dot analytically).
    altitude_pipeline: Option<crate::render::altitude_pipeline::AltitudePipeline>,
    /// The ocean sphere -- the style's background layer re-expressed as
    /// geometry behind the tiles. Rebuilt only when the background colour
    /// PAIR changes; see [`Backdrop`].
    backdrop: Option<Backdrop>,
    /// The limb glow. Three generated vertices and a 112-byte uniform.
    atmosphere_pipeline: Option<crate::render::atmosphere::AtmospherePipeline>,
    /// The sky above the limb (`render::sky`), built the first frame
    /// `SpacePlan::draws_sky` is true.
    sky_pipeline: Option<crate::render::sky::SkyPipeline>,

    // -- labels -----------------------------------------------------------
    /// Built unconditionally, like `pipeline` above -- see `Labels`' own
    /// doc comment for why this is not an `Option` the way the three space
    /// fields above are.
    labels: Labels,
    /// The one pipeline that draws BOTH the icon sprite atlas
    /// ([`Self::icon_atlas_bind_group`]) and `labels.glyph_atlas`'s SDF
    /// quads (see `render::icon_pipeline`'s own doc comment on why one
    /// shader serves both). Built unconditionally for the same reason
    /// `pipeline: TilePipeline` is: the PIPELINE object needs no atlas to
    /// exist, and the glyph half needs it from the first label onward.
    icon_pipeline: crate::render::icon_pipeline::IconPipeline,
    /// The packed icon sprite sheet's GPU texture, bound and ready to
    /// sample through [`Self::icon_pipeline`]. Built at [`MapApp::create_renderer`]
    /// time from [`load_embedded_sprites`] -- never `None` the way native's
    /// `GpuState::icon_atlas_bind_group` is, because this crate has a real
    /// sheet (the bundled one, at worst) before the first frame rather than
    /// waiting on `set_sprites`. `try_install_mapbox_sprites` REPLACES all
    /// three of these fields together, atomically from a caller's view,
    /// once the ONE Mapbox-schema style's own art has both arrived over
    /// `net::Fetcher` -- see that method's own doc comment.
    icon_atlas_bind_group: wgpu::BindGroup,
    /// The compass needle's artwork, uploaded once at renderer creation --
    /// unlike native's lazy `GpuState::compass_atlas`, since this crate
    /// builds the whole renderer eagerly and there is no earlier moment a
    /// lazy upload would save. See `crate::compass`.
    compass_bind_group: wgpu::BindGroup,
    /// The SWAT mark's two inks, uploaded once at renderer creation for the
    /// compass's reason, indexed by `brand_mark::ResolvedVariant::index`,
    /// each with its artwork's pixel size. `None` for an ink whose vendored
    /// PNG failed to decode (it cannot: the bytes are `include_bytes!`), so
    /// a decorative mark never takes the map down. Native's
    /// `GpuState::brand_mark_atlases`, eager.
    brand_mark_atlases: Vec<Option<(wgpu::BindGroup, (u32, u32))>>,
    /// The credit plate behind the attribution text, as native draws it.
    plate_pipeline: crate::render::plate_pipeline::PlatePipeline,
    /// The compact plate's toggle (`attribution::CompactState`): a tap on
    /// the plate or the "i" button flips it; the first pan collapses it.
    attribution_compact: crate::attribution::CompactState,
    /// The plate as last drawn, in LOGICAL points, and whether compact.
    attribution_hit: Option<((f32, f32, f32, f32), bool)>,
    /// Where each named sprite sits in [`Self::icon_atlas_bind_group`] --
    /// `map_view.rs`'s `GpuState::icon_rects`, same shape.
    icon_rects: HashMap<String, crate::render::icon_pipeline::IconRect>,
    /// [`Self::icon_atlas_bind_group`]'s pixel dimensions -- what
    /// `symbol_pass::collect_candidates` divides a packed rect's texel
    /// coordinates by to get a normalised UV. `(0, 0)` never happens here
    /// (see this field's own struct doc); a genuinely empty sheet still
    /// packs to a real, if icon-less, texture (`pack_icon_atlas`'s own
    /// `.max(1)`).
    icon_atlas_size: (u32, u32),
    /// How many icon quads the last `draw_labels` call actually built --
    /// the measurable half of "does this build draw shields now": before
    /// this task it was zero on every frame, unconditionally (the
    /// `debug_assert!` this task removed). Read once a second by
    /// `MapApp::frame`'s own `[map]` status line rather than printed here
    /// per frame, which would be 120 lines a second of the same number.
    icon_instances_drawn: usize,
    /// The per-frame allowance of new glyph rasters `prepare` may do -- see
    /// `render::text_glyph_budget`'s own doc comment. `map_view.rs`'s
    /// `GpuState` carries the identical field, driven the identical way.
    text_glyph_budget: crate::render::text_glyph_budget::TextGlyphBudget<glyphon::cosmic_text::CacheKey>,
    /// Fade-in state for labels, icons and line-label glyphs -- see
    /// `render::label_fade`'s own doc comment.
    label_fade: crate::render::label_fade::LabelFade,
    /// The fade's clock origin.
    label_fade_origin: Instant,
    /// Caps how often the fade alone wakes another frame -- see
    /// `render::label_fade::FadeRedrawScheduler`'s own doc comment.
    fade_redraw_gate: crate::render::label_fade::FadeRedrawScheduler,
    /// Set by `draw_labels` from `LabelFade::end_frame`'s (gated) result, so
    /// `MapApp::frame` -- which owns `request_redraw` and has no access to
    /// `Renderer`'s own borrow by the time it decides whether to ask for
    /// another frame -- can fold it into the same redraw predicate `moving`/
    /// `baked`/`decoding` already feed.
    label_fade_wants_redraw: bool,
    /// When the fade is still animating but `fade_redraw_gate` said not yet
    /// (see `render::label_fade::FadeRedrawScheduler`), the wall-clock
    /// instant to wake the `ControlFlow::Wait` loop at instead of dropping
    /// the request outright -- an on-demand loop that simply skipped it
    /// would leave the fade frozen below full opacity until some UNRELATED
    /// event happened to redraw again. `MapApp::about_to_wait` folds this
    /// into its own `ControlFlow::WaitUntil` alongside a retry backoff.
    fade_wake_at: Option<Instant>,
    // Imagery's own GPU state (`raster_pipeline`/`raster_squares`/
    // `raster_tiles`) lives on `MapApp`, beside `gpu_tiles`, not here --
    // see `MapApp`'s own field doc for why.
}

impl Renderer {
    /// Symbol overlay pass: point labels (`glyphon`) and line-placed glyph
    /// quads (`IconPipeline`'s SDF branch, over `labels.glyph_atlas`), drawn
    /// into a SECOND render pass on the SAME command encoder as the tile/
    /// raster pass just before it -- `LoadOp::Load`, not `Clear`, so it
    /// draws on top of what is already there. `[FROM WASM/NATIVE]` --
    /// `web/src/app.rs`'s `draw_labels`, itself `map_view.rs`'s
    /// `symbol-overlay-pass`, minus overlay features and the attribution
    /// plate (neither exists in this crate, same as the browser build).
    ///
    /// A method on `Renderer`, not on `MapApp`: `draw`'s `renderer` is
    /// already an exclusive borrow of `self.renderer`, and a method taking
    /// `&mut self: MapApp` here would need the whole of `self` again, which
    /// the borrow checker cannot reconcile with `renderer` still live --
    /// the same reason the imagery upload block in `draw` is inline rather
    /// than a `MapApp` method. `gpu_tiles`/`camera`/`style`/`dpr` are
    /// therefore explicit parameters: shared borrows of OTHER fields of
    /// `self`, which is not a conflict.
    #[allow(clippy::too_many_arguments)]
    fn draw_labels(
        &mut self,
        credit: Option<&str>,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        gpu_tiles: &HashMap<TileId, GpuTile>,
        plan: &FramePlan,
        camera: &Camera,
        style: &Style,
        dpr: f32,
        top_inset_pt: f32,
        globe_frame: &crate::render::globe::GlobeFrame,
        light: crate::render::SymbolLight,
        fog: crate::render::HorizonFog,
        route_boxes: &[crate::collision::CollisionBox],
        brand: Option<crate::brand_mark::ResolvedVariant>,
        bottom_inset_pt: f32,
        overlay_plans: &[(TileId, Arc<crate::style::SymbolPlan>)],
        rtl: bool,
        label_paint: Option<crate::symbol_pass::LabelsOnData<'_>>,
    ) {
        let scale_factor = dpr;
        let (physical_width, physical_height) =
            (self.surface_config.width as f32, self.surface_config.height as f32);
        // glyphon works entirely in PHYSICAL pixels; the camera and
        // `AnchorProjection::screen` are in LOGICAL ones -- see
        // `map_view.rs`'s own comment on this same split.
        self.labels.viewport.update(
            &self.ctx.queue,
            glyphon::Resolution { width: self.surface_config.width, height: self.surface_config.height },
        );

        let mut candidates: Vec<crate::symbol_pass::SymbolCandidate> = Vec::new();
        let mut bypassed: Vec<crate::symbol_pass::DrawnSymbol> = Vec::new();
        // The real sprite sheet -- the bundled one at worst, Mapbox's own
        // once `try_install_mapbox_sprites` lands it -- not the permanent
        // "before `set_sprites`" empty state this crate used to start in.
        // See `Renderer::icon_atlas_bind_group`'s own doc comment.
        let frame = crate::symbol_pass::SymbolFrame {
            occlusion: None,
            label_paint,
            camera,
            style_layers: &style.layers,
            scale_factor,
            size_scale: scale_factor,
            collision_padding: crate::symbol_pass::SYMBOL_COLLISION_PADDING_LOGICAL_PX * scale_factor,
            size_blend_t: camera.zoom.fract() as f32,
            icon_rects: &self.icon_rects,
            icon_atlas_size: self.icon_atlas_size,
            globe: crate::render::globe::GlobeAnchors::new(globe_frame),
            light,
            // THIS FRAME's fade, the value `draw` wrote into the camera
            // uniform -- a label past its end is not placed.
            fog,
        };
        // `plan.draws`, which is the cover plus whatever stand-ins are
        // filling holes -- a stand-in's symbols are its OWN tile's, baked
        // at its own zoom, same as its triangles.
        let visible_plans: Vec<(TileId, Arc<crate::style::SymbolPlan>)> = plan
            .draws
            .iter()
            .filter_map(|tile| gpu_tiles.get(tile).map(|gpu| (*tile, gpu.symbols.clone())))
            // An embedding host's point-layer labels (`host::Host`), as
            // tile-anchored plans through the same candidate collection and
            // the same collision index as the basemap's.
            .chain(overlay_plans.iter().cloned())
            .collect();
        // A REAL `GlyphSink`, not `None` -- `None` is the defect
        // `tests/label_plan_parity.rs` exists to catch (see that test's own
        // doc comment): every line-placed label would still be measured,
        // placed and collision-boxed, and then rastered nowhere.
        let mut glyph_sink = crate::symbol_pass::GlyphSink {
            atlas: &mut self.labels.glyph_atlas,
            swash: &mut self.labels.swash_cache,
        };
        crate::symbol_pass::collect_candidates(
            &frame,
            visible_plans.iter().map(|(tile, plan)| (*tile, &**plan)),
            &mut self.labels.font_system,
            &mut self.labels.shape_cache,
            Some(&mut glyph_sink),
            &mut candidates,
            &mut bypassed,
        );

        self.labels.collision.reset(crate::symbol_pass::symbol_collision_bounds(
            camera.viewport_width_px,
            camera.viewport_height_px,
            scale_factor,
        ));
        // The route's screen path, in first: a label that would sit on the
        // line is rejected the way one that would sit on another label is.
        // Empty with no trip.
        for route_box in route_boxes.iter().copied() {
            self.labels.collision.insert_obstacle(route_box);
        }
        // The credit's plate, bottom-right above the home indicator: native's
        // one layout (`symbol_pass::attribution_plate_layout`) -- every
        // credit the map owes, stacked, never clipped, and compact (an "i"
        // button after the first pan) when too tall for the screen.
        let corner_height = physical_height - bottom_inset_pt * dpr;
        let credit_layout = credit.filter(|c| !c.is_empty()).map(|credit| {
            let size = crate::symbol_pass::ATTRIBUTION_TEXT_SIZE_PX * dpr;
            let face = crate::symbol_pass::chrome_text_face();
            let justify = crate::overlay::host::TextJustify::Center;
            let shaping = std::cell::RefCell::new((&mut self.labels.font_system, &mut self.labels.shape_cache));
            let measure = |text: &str| {
                let mut guard = shaping.borrow_mut();
                let (font_system, shape_cache) = &mut *guard;
                shape_cache.get_or_shape(font_system, text, size, None, justify, &face).measured_width
            };
            crate::symbol_pass::attribution_plate_layout(credit, physical_width, corner_height, dpr, self.attribution_compact, measure)
        });
        self.attribution_hit = credit_layout
            .as_ref()
            .map(|l| ((l.left / dpr, l.top / dpr, l.width / dpr, l.height / dpr), l.compact));
        let credit_rect = credit_layout.as_ref().map(|l| (l.left, l.top, l.width, l.height));
        if let Some((x, y, w, h)) = credit_rect {
            self.labels.collision.insert_obstacle(crate::symbol_pass::chrome_obstacle(x, y, w, h, scale_factor));
        }
        // The SWAT mark's footprint (the quad drawn below, lifted clear of
        // the plate as native's is), so no label is placed under it.
        let brand_quad = |dimensions: (u32, u32)| {
            let mut q = crate::brand_mark::instance(&crate::brand_mark::BrandMark::default(), dimensions, corner_height, dpr);
            if let Some(rect) = credit_rect {
                crate::brand_mark::lift_clear_of_plate(&mut q, rect, crate::chrome::BOTTOM_ROW_INSET_PX * dpr);
            }
            q
        };
        if brand.is_some() {
            let q = brand_quad(crate::brand_mark::artwork_size());
            self.labels
                .collision
                .insert_obstacle(crate::symbol_pass::chrome_obstacle(q.screen_x, q.screen_y, q.width, q.height, scale_factor));
        }
        // Place, merge the bypassed symbols in, sort into style-layer
        // order, clip a label behind a later icon, and split the survivors
        // into the three lists the draw code below wants -- shared with
        // native and the browser via `render::label_plan::label_draw_plan`.
        let crate::render::label_plan::LabelDrawPlan {
            mut icon_instances,
            mut symbol_text,
            mut glyph_instances,
            symbols_placed: _,
            overlay_sort_keys: _,
        } = crate::render::label_plan::label_draw_plan(candidates, bypassed, &mut self.labels.collision);
        self.icon_instances_drawn = icon_instances.len();

        // New glyph rasters for glyphon, under a per-frame allowance -- see
        // `render::text_glyph_budget`. Before the fade below, so a deferred
        // label's fade starts on the frame its text first draws. `[FROM
        // NATIVE]` -- `map_view.rs`'s identical block.
        let text_deferred = {
            let shape_cache = &self.labels.shape_cache;
            self.text_glyph_budget.admit(
                &mut symbol_text,
                crate::render::text_glyph_budget::NEW_GLYPHS_PER_FRAME,
                |((key, ..), _)| shape_cache.get(key).map(|shaped| shaped.glyph_keys.as_slice()),
            )
        };
        let mut label_fade_wants_redraw = text_deferred > 0;
        // Fade-in for whatever arrived since last frame, keyed by content
        // and nearest position -- see `render::label_fade`. Applied to the
        // map's own symbols only. `[FROM NATIVE]` -- `map_view.rs`'s
        // identical block, capped through the same
        // `FadeRedrawScheduler` so a ProMotion display's 120 Hz cadence does
        // not redraw the fade twice as often as it needs (see that type's
        // own doc comment).
        {
            use crate::render::label_fade::fade_id;
            let now_s = self.label_fade_origin.elapsed().as_secs_f64();
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
            match self.fade_redraw_gate.follow_up(now_s, animating) {
                // Nothing left to animate -- stop asking entirely.
                FadeFollowUp::Idle => self.fade_wake_at = None,
                FadeFollowUp::RedrawNow => {
                    label_fade_wants_redraw = true;
                    self.fade_wake_at = None;
                }
                // The gate said not yet: an on-demand `ControlFlow::Wait`
                // loop has nothing else that would wake it at the right
                // moment, so ask for exactly one wake at the cap's own
                // interval rather than dropping the request and leaving the
                // fade frozen below 1. `MapApp::about_to_wait` reads this.
                FadeFollowUp::WakeAt(_) => {
                    self.fade_wake_at = Some(
                        Instant::now() + Duration::from_secs_f64(crate::render::label_fade::MIN_REDRAW_INTERVAL_SECONDS),
                    );
                }
            }
        }
        self.label_fade_wants_redraw = label_fade_wants_redraw;

        // The compass: shown only off north (Deviation 5), drawn as a
        // third `IconInstance` batch through this same pipeline -- this
        // crate has no chrome to attach a native control to (see
        // `ios/ROTATION.md`), so it draws through the engine's existing
        // overlay path, same as the brand mark/attribution do on the other
        // two shells.
        let compass = crate::compass::compass_visible(camera.bearing)
            .then(|| crate::compass::compass_instance_dir(camera.bearing, camera.viewport_width_px, top_inset_pt, rtl, dpr));
        // The SWAT mark, bottom-left, through the same pipeline (native's
        // `brand_mark::instance`, fed a viewport cut short by the home
        // indicator so the mark sits above it rather than under it).
        let brand = brand.and_then(|variant| {
            let (bind_group, dimensions) = self.brand_mark_atlases.get(variant.index())?.as_ref()?;
            Some((bind_group, brand_quad(*dimensions)))
        });
        // The credit's lines join the text AFTER the fade (a licence credit
        // never fades in) and draw over every label, unplaced.
        let credit_plate = credit_layout.map(|layout| {
            let size = crate::symbol_pass::ATTRIBUTION_TEXT_SIZE_PX * dpr;
            let face = crate::symbol_pass::chrome_text_face();
            let justify = crate::overlay::host::TextJustify::Center;
            // The light pill (`attribution::AttributionTone::Light`): this
            // shell does not decide the tone from its style yet.
            let palette = crate::attribution::AttributionTone::Light.palette();
            let rgba = |[r, g, b, a]: [f32; 4]| crate::style::Rgba { r, g, b, a };
            let mut push = |line: &str, x: f32, y: f32, color: crate::style::Rgba| {
                symbol_text.push((
                    (crate::symbol_pass::LabelShapeCache::key(line, size, None, justify, &face), x, y, color, 0.0, color, 0.0),
                    None,
                ));
            };
            if !layout.collapsed {
                for (line, x, y) in &layout.lines {
                    push(line, *x, *y, rgba(palette.text));
                }
            }
            push(crate::symbol_pass::ATTRIBUTION_COMPACT_GLYPH, layout.button_glyph.0, layout.button_glyph.1, rgba(palette.glyph));
            let (bx, by, bd) = layout.button;
            [
                crate::render::plate_pipeline::PlateInstance {
                    screen_x: layout.left,
                    screen_y: layout.top,
                    width: layout.width,
                    height: layout.height,
                    radius_px: layout.radius,
                    color: palette.plate,
                },
                crate::render::plate_pipeline::PlateInstance {
                    screen_x: bx,
                    screen_y: by,
                    width: bd,
                    height: bd,
                    radius_px: bd / 2.0,
                    color: palette.button,
                },
            ]
        });

        // The glyph atlas's texture, and the UVs that address it, settled
        // together -- the atlas grew while `collect_candidates` rastered
        // new glyphs above, so it has to be (re-)uploaded before a quad can
        // sample it, and every quad's texel UVs have to be divided by the
        // size that upload establishes. Unlike native/the browser this is
        // NOT gated on a sprite `IconPipeline` existing first: this crate's
        // `icon_pipeline` is unconditional (see `Renderer`'s own field
        // doc), so the glyph atlas can always upload through it.
        if !glyph_instances.is_empty() {
            let generation = self.labels.glyph_atlas.generation();
            if self.labels.glyph_atlas_bind_group.is_none()
                || self.labels.glyph_atlas_generation != Some(generation)
            {
                self.labels.glyph_atlas_bind_group =
                    Some(self.icon_pipeline.upload_atlas(&self.ctx, self.labels.glyph_atlas.rgba()));
                self.labels.glyph_atlas_generation = Some(generation);
            }
            let (_, atlas_width, atlas_height) = self.labels.glyph_atlas.image();
            crate::symbol_pass::normalise_glyph_uvs(&mut glyph_instances, atlas_width, atlas_height);
        }

        // Text `prepare()` happens before the pass below is opened -- it
        // only touches the device/queue/atlas, not the pass itself, same
        // ordering `map_view.rs` uses and for the same reason: `prepare`
        // can fail on a real, reachable condition (`PrepareError::AtlasFull`
        // once enough labels share one atlas), and this skips just this
        // frame's text rather than taking the whole present down.
        let text_prepared = if symbol_text.is_empty() {
            false
        } else {
            let text_areas = crate::symbol_pass::text_areas(
                &symbol_text,
                &self.labels.shape_cache,
                physical_width,
                physical_height,
            );
            match self.labels.text_renderer.prepare(
                &self.ctx.device,
                &self.ctx.queue,
                &mut self.labels.font_system,
                &mut self.labels.atlas,
                &self.labels.viewport,
                text_areas,
                &mut self.labels.swash_cache,
            ) {
                Ok(()) => {
                    // What this frame's `text_glyph_budget.admit` let through
                    // really did land in the atlas -- `[FROM NATIVE]`.
                    self.text_glyph_budget.confirm();
                    true
                }
                Err(err) => {
                    crate::mlog!("[map] symbol text prepare failed, skipping this frame's labels: {err:#}");
                    // Nothing admitted this frame is known-cached after all.
                    self.text_glyph_budget.discard();
                    false
                }
            }
        };

        // ONE pass for both halves -- glyphs then text -- opened only if
        // there is something to put in it, and not a THIRD pass stacked on
        // the tile pass and the imagery pass before it: this draws into
        // the SAME `LoadOp::Load` pass `map_view.rs`'s
        // `symbol-overlay-pass` uses for exactly this reason.
        if !icon_instances.is_empty() || !glyph_instances.is_empty() || compass.is_some() || brand.is_some() || text_prepared || credit_plate.is_some() {
            let mut symbol_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("symbol-overlay-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            // Icon sprite quads (road shields, POI markers, one-way arrows)
            // draw BEFORE line-placed glyph quads, matching Mapbox's own
            // default composition and `map_view.rs`'s same ordering. Both
            // ride in the SAME call, as two batches through the one shared
            // `IconPipeline` (see that pipeline's own doc comment on why
            // one shader serves both, and `render_batches`' own doc on why
            // this has to be one call rather than two: it keeps a single
            // vertex buffer, and a second call would overwrite the first's
            // quads before either draw ran).
            if !icon_instances.is_empty() || !glyph_instances.is_empty() || compass.is_some() || brand.is_some() {
                let mut batches: Vec<(&wgpu::BindGroup, &[crate::render::icon_pipeline::IconInstance])> = Vec::new();
                if !icon_instances.is_empty() {
                    batches.push((&self.icon_atlas_bind_group, &icon_instances));
                }
                if let Some(atlas) = self.labels.glyph_atlas_bind_group.as_ref() {
                    batches.push((atlas, &glyph_instances));
                }
                if let Some(compass) = compass.as_ref() {
                    batches.push((&self.compass_bind_group, std::slice::from_ref(compass)));
                }
                if let Some((bind_group, mark)) = brand.as_ref() {
                    batches.push((*bind_group, std::slice::from_ref(mark)));
                }
                self.icon_pipeline.render_batches(
                    &self.ctx,
                    &mut symbol_pass,
                    &batches,
                    physical_width,
                    physical_height,
                );
            }

            // The credit plate under its text (and over every icon), as
            // native draws it.
            if let Some(plates) = credit_plate.as_ref() {
                self.plate_pipeline.render(&self.ctx, &mut symbol_pass, plates, physical_width, physical_height);
            }
            // Text draws SECOND, so a street name sits over a road arrow
            // rather than under it -- the order `glyphon`-drawn labels
            // have always had against icons.
            if text_prepared {
                if let Err(err) =
                    self.labels.text_renderer.render(&self.labels.atlas, &self.labels.viewport, &mut symbol_pass)
                {
                    crate::mlog!("[map] symbol text render failed, skipping this frame's labels: {err:#}");
                }
            }
        }
        self.labels.atlas.trim();
    }
}

struct MapApp {
    renderer: Option<Renderer>,
    fetcher: Fetcher,
    started: Instant,
    /// Test seam: when `Some`, `now_ms` returns it instead of the wall
    /// clock, so a test can stamp a gesture's events with chosen times.
    /// Test builds only; production always reads `started`.
    #[cfg(test)]
    now_override: Option<f64>,

    /// The Mapbox sprite manifest's raw bytes, once
    /// `Fetched::SpriteBody(SpriteResource::Manifest, _)` has arrived --
    /// `None` until then, and forever if no fetch was ever queued (the
    /// `TileSchema::Omt` build) or it failed. Paired with
    /// `mapbox_sprite_png` below by `try_install_mapbox_sprites`, which is
    /// what actually consumes both; this field only accumulates.
    mapbox_sprite_json: Option<Vec<u8>>,
    /// [`Self::mapbox_sprite_json`]'s twin: the packed atlas PNG's raw
    /// bytes.
    mapbox_sprite_png: Option<Vec<u8>>,

    style: Style,
    /// Chosen once at startup ([`TileSchema::resolve`]) and never changed
    /// afterwards: which style cycle [`Self::style_index`] indexes into
    /// ([`TileSchema::styles`]) and which tile URL template
    /// [`Self::tile_url`] holds. Three fingers cycles WITHIN this schema's
    /// own set; it never crosses to the other one.
    schema: TileSchema,
    /// This build's `{z}/{x}/{y}` tile URL template -- OpenFreeMap's fixed
    /// string for [`TileSchema::Omt`], or the Mapbox composite URL (token
    /// baked in) [`TileSchema::resolve`] derived for
    /// [`TileSchema::MapboxComposite`]. A field rather than the old bare
    /// [`TILE_URL`] constant because the Mapbox case cannot be a `const`: it
    /// embeds a token read at startup, not known at compile time as a
    /// literal.
    tile_url: String,
    /// The schema's own tile template; a style with its own source
    /// (`styles::style_source`) swaps `tile_url` away from it.
    schema_tile_url: String,
    /// The style in force's own source, if it has one.
    style_source: Option<crate::styles::StyleSource>,
    /// The host's own OpenMapTiles archive for the basemap (an offline
    /// region the app downloaded), in place of the schema's tiles; see
    /// [`MapApp::set_basemap_archive`].
    basemap_archive: Option<BasemapArchive>,
    /// The archive's bounds from its header, when it lets the online tiles
    /// draw outside them (`BasemapArchive::online_outside`).
    archive_bounds: Option<crate::geo::LonLatBounds>,
    /// Vector tiles asked of the online source under an archive (see
    /// [`Self::vector_url`]).
    online_inflight: HashSet<TileId>,
    /// Tiles the online source failed for under an archive (no network):
    /// they come from the archive instead, half empty or not.
    online_failed: HashSet<TileId>,
    /// Set the first time [`log_schema_bind_once`] runs, so the schema-bind
    /// check prints exactly once per style (at the compile that produced
    /// the live `style`, against the first tile decoded afterwards) rather
    /// than once per tile.
    schema_checked: bool,
    /// Which entry of [`TileSchema::styles`] `style` was compiled from.
    /// Kept because the compiled `Style` has no memory of where it came from
    /// -- it carries layers, not a filename -- and the switcher has to know
    /// what "next" means.
    style_index: usize,
    /// Set when a third finger lands and the style switches; cleared when the
    /// last finger lifts.
    ///
    /// Its whole job is to make the switch cost NOTHING else. Three fingers
    /// arriving on the glass are three `Started` events, and the first two of
    /// them have already begun a drag and then a pinch by the time the third
    /// is seen -- so without this the map lurches and spins under the hand
    /// that was only asking for a different palette. While it is set,
    /// `Moved` updates each finger's position (so the tap/slop bookkeeping
    /// stays honest) and applies nothing, and the release path skips the
    /// fling. See [`MapApp::cycle_style`].
    multi_touch_consumed: bool,
    /// The camera and every ease steering it; see `crate::camera_control`.
    /// This shell never writes a camera field itself --
    /// `tests/camera_is_written_by_the_controller.rs`.
    camera_control: CameraController,
    /// The reach of this frame's display-zoom flat cover
    /// (`render::cover_plan::FlatCover::reach`), set by `cover()`. `None`
    /// when the globe covered the frame or nothing was cut. Read by the
    /// horizon fade.
    last_cover_reach: std::cell::Cell<Option<f64>>,
    /// The display cover's `FlatCover::building_reach`: where buildings
    /// fade out (`frame_plan::extrusion_frame_depth`).
    last_building_reach: std::cell::Cell<Option<f64>>,
    /// The backing store's ACTUAL logical-to-physical scale, which is the
    /// ratio the shader's antialiasing band must use.
    dpr: f32,
    /// The status bar / Dynamic Island's height in points, so the compass
    /// (this crate's one piece of chrome -- see `ios/ROTATION.md`) sits
    /// below it rather than under it. Set by `apply_resize`, which is the
    /// one place that can compare `inner_position` (the safe area's origin)
    /// against `outer_position` (the window's) -- see that call site's own
    /// comment. `0.0` until the first resize, which undershoots by at most
    /// one frame on launch.
    top_inset_pt: f32,
    /// The home indicator's band, for the chrome's bottom strip. Set by
    /// `apply_resize` beside `top_inset_pt`, from the same inner/outer split.
    bottom_inset_pt: f32,
    /// Turn-by-turn navigation; everything it decides lives in `crate::nav`.
    nav: crate::nav::host::NavHost,
    /// Speaks what `nav` hands out each frame; see `crate::speech`.
    speech: crate::speech::SpeechOutput,
    /// `--nav-sim`: start the drive on the first frame (it needs a camera).
    nav_autostart: bool,
    /// Voice prompts `nav` has handed out, said aloud or not: how a host
    /// test sees guidance went out (`host::Host::nav_prompts`).
    nav_prompts: u64,
    /// `false`: prompts are counted, never said (a host's tests on a Mac
    /// must not talk). See `host::Host::nav_set_speaker_on`.
    speaker_on: bool,
    /// Added to the monotonic clock: a host test marches a simulated drive
    /// forward without waiting it out (`host::Host::advance_clock_ms`).
    clock_skew_ms: f64,
    background: wgpu::Color,

    /// WHAT THE GLOBE SITS ON. `MapView`'s `space` field, on this app's own
    /// struct: the space colour, the star intensity and the limb glow. See
    /// [`space_default`]. The GPU residency the three booleans this drives
    /// gate -- the star pipeline, the backdrop and the atmosphere pipeline
    /// -- lives on [`Renderer`] instead, beside `ctx` and `pipeline`, for
    /// the same reason `gpu_tiles` does not: they are device-bound, and
    /// `Renderer` is what does not survive a suspend that drops the
    /// surface. `MapView`'s `GpuState` holds the native twins of the same
    /// three fields for the same reason.
    space: crate::render::space::Space,

    /// Decoded MVT, kept so a zoom-bucket crossing can re-bake without
    /// re-fetching. Native's `decoded_cache`; bounded by
    /// [`MAX_DECODED_TILES`] in `evict_discard_pile`.
    decoded: HashMap<TileId, DecodedTile>,
    gpu_tiles: HashMap<TileId, GpuTile>,
    inflight: HashSet<TileId>,
    /// Tile bodies drained from the fetcher but not decoded yet -- see
    /// [`Self::decode_arrived`]. Their tiles stay in `inflight` /
    /// `raster_pending` until decoded or discarded.
    arrived: Vec<(TileKey, (bool, Vec<u8>))>,
    /// The last request pass's wanted set: what `decode_arrived` keeps.
    wanted: crate::fetch_queue::Wanted<TileKey>,
    /// Bodies discarded undecoded because the camera had left their tile.
    stale_results_dropped: u64,
    /// Tiles the source has no data for (404/410/204). Without this a 404 is
    /// re-requested every frame forever. ONLY absence goes here: a transient
    /// failure goes into `retries` instead, because putting it here is what
    /// used to blank a tile for the whole session after one timeout.
    /// Bounded (`raster_retention::AbsentTiles`, native's `absent`).
    missing: crate::raster_retention::AbsentTiles,
    /// Backoff state for vector tiles whose last request failed transiently.
    /// See `fetch_policy`.
    retries: crate::fetch_policy::RetryTable,
    /// Last frame's retained set, which is what eviction is allowed to
    /// spare.
    retained: HashSet<TileId>,

    // -- imagery ----------------------------------------------------------
    //
    // `web/src/app.rs`'s `raster_decoded`/`raster_inflight`/`raster_missing`
    // fields, same shape, and bounded by native's own policy:
    // `raster_retention::DecodedImagery` is a byte-budgeted LRU that never
    // evicts the frame's cover, its floor or z0 (it was a `HashMap` that
    // kept every imagery tile the session had ever decoded).
    // `raster_pending`, not `raster_inflight`: this
    // crate's `net::Fetcher` already calls the vector twin of this set
    // `inflight`, and the two fields sit one screen apart, so a name that
    // reads differently at a glance is worth more here than matching the
    // browser's spelling.
    raster_decoded: crate::raster_retention::DecodedImagery<Arc<image::RgbaImage>>,
    raster_pending: HashSet<TileId>,
    raster_missing: crate::raster_retention::AbsentTiles,
    /// `retries`' imagery twin.
    raster_retries: crate::fetch_policy::RetryTable,
    /// `None` until the first raster tile lands -- `map_view.rs`'s and
    /// `web/src/app.rs`'s own `raster_pipeline` field, same reason: built
    /// lazily so a launch with no `GPUI_MAP_IMAGERY` set (see
    /// `raster_config`) never pays for a pipeline it never draws with.
    /// Lives beside `gpu_tiles`, not on [`Renderer`], for the same reason
    /// `gpu_tiles` itself does: `draw`'s borrow of `renderer` and its reads
    /// of `self.gpu_tiles`/`self.raster_tiles` are disjoint FIELD borrows
    /// either way, so nothing forces a choice here, and matching where
    /// `gpu_tiles` already lives keeps every per-tile GPU cache in one
    /// place -- native's `GpuState` and the browser's `MapApp` both do the
    /// same.
    raster_pipeline: Option<crate::render::raster_pipeline::RasterPipeline>,
    /// One shared square mesh per raster cover zoom -- `render::raster_mesh`'s
    /// whole purpose is that every tile at one zoom shares one buffer pair.
    raster_squares: HashMap<crate::render::raster_mesh::RasterMeshKey, crate::render::raster_pipeline::RasterBuffers>,
    /// One decoded, uploaded imagery tile's GPU residency, bounded by
    /// [`MAX_RASTER_TILES`] -- see [`HeldRasterTile`].
    raster_tiles: HashMap<TileId, HeldRasterTile>,
    /// The imagery in force ([`effective_raster`]), kept as a field so a
    /// frame reads it beside its other borrows.
    raster: RasterConfig,
    /// The host's imagery (`set_raster_override`), over the style's own.
    raster_override: Option<RasterConfig>,

    // -- gesture state --------------------------------------------------
    touches: Vec<ActiveTouch>,
    /// Where the one-finger drag was last seen, in logical points.
    drag_from: Option<(f64, f64)>,
    drag_samples: std::collections::VecDeque<DragSample>,
    pan_inertia: Option<PanInertia>,
    /// When the CURRENT one-finger drag segment began -- i.e. the `now_ms`
    /// of the last `begin_drag` call -- and whether that segment took over
    /// from a dropped pinch rather than starting at a fresh touch-down. See
    /// `post_pinch_drag_is_trustworthy`.
    drag_segment_started_ms: f64,
    drag_segment_from_pinch: bool,
    /// The two-finger gesture in progress: `Some` exactly while one is.
    /// It decides pinch-and-rotate versus tilt (`crate::touch_gesture`).
    pinch: Option<TwoFingerGesture>,
    /// The two fingers' positions at the last update, in points: the `old`
    /// half of `TwoFingerGesture::update`. Re-seeded, without resetting the
    /// gesture, when the finger count changes above two.
    pinch_points: Option<[(f32, f32); 2]>,
    /// Bearing deltas of the gesture in progress, for its release velocity.
    rotate_samples: RotateSamples,
    /// `now_ms` of the last `advance`, for the camera eases' `dt`.
    last_advance_ms: Option<f64>,
    /// Where and when the last tap lifted, for double-tap detection.
    last_tap: Option<(f64, f64, f64)>,

    resize_pending: bool,
    /// Set whenever something has happened that a frame has not yet drawn
    /// -- a touch, a resize, a fling still gliding, a bake backlog still
    /// draining, or a tile that landed on a background thread and woke the
    /// loop through `net::Fetcher::set_wake` -- and cleared the instant
    /// `about_to_wait` acts on it.
    ///
    /// This is the redraw predicate: the whole reason `about_to_wait` no
    /// longer draws unconditionally. `ControlFlow::Wait` plus every trigger
    /// below calling `window.request_redraw()` should already be enough on
    /// its own to stop the loop waking with nothing to do, but this flag is
    /// checked too, on purpose -- see `about_to_wait`'s own comment for why
    /// a second, cheap gate is worth having on top of trusting winit's wake
    /// bookkeeping alone.
    needs_redraw: bool,
    /// Caller overlay sources -- GeoJSON polygons on the tiled compact path,
    /// clustered points as instanced circles -- driven inline by the
    /// engine's `overlay::host::ShellOverlays` under
    /// [`OVERLAY_BUDGET_PER_FRAME`]. Filled by the overlay demo toggle
    /// (`--overlay-demo` / `GPUI_MAP_OVERLAY=demo`).
    overlays: crate::overlay::host::ShellOverlays,
    /// Credits the host's vector overlays owe (`host::Host::
    /// add_vector_overlay`), drawn on the credit plate after the basemap's.
    extra_credits: Vec<String>,
    /// Arcs, extruded overlays (3D hexbins, 3D GeoJSON), trips and
    /// heatmaps: the engine's renderers in `MapView::paint`'s order (see
    /// `overlay::shell_layers`). Extrusions draw in the tile pass after the
    /// buildings; the rest in a pass of their own under the labels.
    pub(crate) shell_layers: crate::overlay::host::ShellLayers,
    /// Population dots (Task 17): the engine's `DotLayers`, planned every
    /// frame and fed by [`crate::dots::DotWorker`] from a PMTiles archive
    /// in the app's `Documents` folder (`crate::dots::open_phone_source`);
    /// empty, and never touched, when no archive was copied.
    dots: crate::overlay::host::DotLayers,
    /// The dots' GPU half; belongs to the renderer's device, so it is
    /// dropped with it (`resumed`).
    dots_gpu: crate::overlay::host::DotLayersGpu,
    dot_worker: Option<crate::dots::DotWorker>,
    /// When a failed count tile's backoff ends: the one wake the loop
    /// needs for it (native's `dot_retry_pending`).
    dot_retry_at: Option<Instant>,
    /// Objects above the globe (the satellites card): the desktop's
    /// `MapView` altitude state, verbatim (`render::altitude_layers`),
    /// advanced and drawn by [`MapApp::draw`] on a globe frame.
    altitude: crate::render::altitude_layers::AltitudeLayers,
    /// What instant the altitude layers show and how fast it runs; `None`
    /// is real time.
    scene_clock: Option<crate::render::altitude_layers::SceneClock>,
    /// When the moving dots next owe a frame (they ask only once they have
    /// moved a visible amount: `AltitudeLayers::dot_motion`).
    altitude_wake_at: Option<Instant>,
    /// Dots the last frame drew, and how long its altitude advance +
    /// prepare took (ms): the satellites' perf line.
    altitude_points_drawn: usize,
    altitude_cpu_ms: f64,
    /// The host's data credit, joined to the basemap's.
    data_credit: Option<String>,
    /// When the dot source was set, and whether its first dots have been
    /// logged ("first dots" in MG-25's log).
    dots_set_at: Option<Instant>,
    dots_first_logged: bool,
    /// The overlay demo toggle: load the sample on the first frame, which
    /// has a camera to centre it on.
    overlay_demo: bool,
    /// Mirrors what `sync_activity_state` last told UIKit, so a frame with
    /// nothing new to report does not repeat an Objective-C message it has
    /// already sent. Not read for anything else -- the source of truth for
    /// whether the idle timer SHOULD be off is always recomputed fresh from
    /// `touches`/`pan_inertia`, never trusted from this cache.
    idle_timer_disabled: bool,
    frames: u64,
    first_draw_logged: bool,
    /// A test's request to read the next drawn frame back
    /// (`host::Host::capture_frame`), and the frame read: physical width,
    /// height and RGBA8 rows.
    pub(crate) capture_requested: bool,
    pub(crate) captured: Option<(u32, u32, Vec<u8>)>,
    /// When the first vector tile body arrived, ms after launch: the
    /// network's share of launch -> first triangle.
    first_body_ms: Option<f64>,
    last_status_ms: f64,

    // -- frame timing ---------------------------------------------------
    /// The `now_ms` of the previous `frame()` entry. `frame()` is the only
    /// place that sees every present, so it is the only place the interval
    /// between them can be measured without a second clock.
    last_frame_ms: Option<f64>,
    frame_intervals: MsWindow,
    /// Arrival time of the OLDEST touch event not yet answered by a
    /// present, and the window of how long that wait turned out to be.
    ///
    /// Written with `get_or_insert`, never plain assignment: several moves
    /// can land between two presents, and the finger is waiting on the
    /// FIRST of them. Charging from the last would report the queue as
    /// empty when it is at its fullest.
    input_pending_ms: Option<f64>,
    input_latency: MsWindow,

    // -- the host ---------------------------------------------------------
    /// The drawable's size, scale and insets, as the host last set them
    /// ([`Self::set_surface_metrics`]); read by `apply_resize`.
    surface_metrics: SurfaceMetrics,
    /// Called by `request_redraw`: the winit shell's `window.request_redraw`,
    /// or an embedding host's `on_needs_frame`. `None` in tests.
    redraw_hook: Option<Box<dyn Fn()>>,
    /// Whether frames should toggle `UIApplication.idleTimerDisabled` while
    /// a gesture or a glide is live. The standalone app does; an embedding
    /// host owns its own idle policy.
    manage_idle_timer: bool,
    /// Whether a tap (past the chrome) records where it landed in
    /// [`Self::tapped_at`], for the embedding host to hit-test its overlays.
    /// The standalone demo has nothing to report to.
    pick_on_tap: bool,
    /// Where the last such tap landed, in points; taken by the host after
    /// each touch.
    tapped_at: Option<(f64, f64)>,
    /// An embedding host's point-layer labels, built at each push
    /// (`host::Host::flush`) and fed to the label pass beside the basemap's.
    overlay_label_plans: Vec<(TileId, Arc<crate::style::SymbolPlan>)>,
    /// Draw `overlays` in their own pass over the 3D buildings (depth
    /// cleared) rather than in the tile pass under them. The embedding host
    /// turns it on.
    overlays_above_buildings: bool,
    /// Draw the basemap's `fill-extrusion` layers flat: footprints at the
    /// ground in their own colour, no walls. A render-time switch; the
    /// style is not recompiled.
    buildings_flat: bool,
    /// Labels over data (`host::Host::set_labels_on_data`): the paint every
    /// basemap label draws with while the host draws data fills under the
    /// labels. A render-time switch, like `buildings_flat`.
    label_paint: Option<crate::style::LabelPaint>,
    /// Which labels sit over the host's polygons, memoised per label.
    label_cover: crate::overlay::host::LabelCover,
    /// The host's layout direction: right to left mirrors the chrome that
    /// has a side (the compass, top-left). The SWAT mark stays bottom-left
    /// and the credits bottom-right, as MapLibre keeps them.
    chrome_rtl: bool,
    /// The host draws the credits itself (a collapsible ⓘ that also
    /// carries the data's credits): the engine draws no credit plate. The
    /// host owes every credit `attributions` lists, visible on a tap.
    credits_by_host: bool,
}

/// A frame's `fill-extrusion` plan: `style`'s extruded layers at `camera`,
/// flattened (every roof at the ground) when `flat`. A free function so the
/// frame can call it while it holds the renderer.
fn frame_extrusion_plan(
    style: &Style,
    camera: &Camera,
    building_reach: Option<f64>,
    flat: bool,
) -> crate::render::frame_plan::ExtrusionPlan {
    crate::render::frame_plan::extrusion_plan(
        &style.layers,
        camera.zoom,
        camera.center_lat,
        f64::from(camera.bearing),
        crate::render::frame_plan::ExtrusionOptions {
            translucency: true,
            // `host::Host::set_buildings_3d(false)`: every roof at
            // the ground, so the footprints keep the city's texture
            // and nothing stands over a route.
            flatten: flat,
            // The fade before the building cut. The look stays
            // MapLibre's here: `ExtrusionLook::ENHANCED` is native
            // only for now, so no haze colour is needed.
            depth: crate::render::frame_plan::extrusion_frame_depth(
                camera,
                building_reach,
                crate::style::Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 },
            ),
            ..Default::default()
        },
    )
}

/// Compiles one entry of a [`TileSchema`]'s own style cycle
/// ([`TileSchema::styles`]) and derives the clear colour that goes with it.
///
/// A free function, not a method, because it is called once before `MapApp`
/// exists (`new`) and once per switch on a `MapApp` that already does
/// (`cycle_style`) -- and because it is pure: bytes in, style and colour out,
/// no GPU, no camera, no clock. That is the same property that let this engine
/// cross to a browser and now to a phone.
///
/// The background is resolved through the ENGINE against an empty tile, not
/// read out of the JSON: `background-color` can be a zoom expression, and
/// `match_tile` is the only thing that knows how to evaluate one.
///
/// Takes the STYLE SET explicitly, rather than reaching for
/// `crate::styles::STYLES` itself, so a caller can never compile an index
/// against the wrong schema's cycle by accident -- see [`TileSchema`].
fn compile_style(styles: &[crate::styles::StyleChoice], index: usize) -> anyhow::Result<(Style, wgpu::Color)> {
    let choice = &styles[index];
    let style = crate::style_spec::parse_and_compile(choice.style_json())
        .map_err(|e| anyhow::anyhow!("style {:?} compile failed: {e:#}", choice.name))?;
    let background = crate::style::match_tile(&DecodedTile { data_hash: 0, layers: vec![] }, &style, 14)
        .ops
        .iter()
        .find_map(|op| match op {
            crate::style::PaintOp::Background { color } => Some(TilePipeline::clear_color(color)),
            _ => None,
        })
        .unwrap_or(wgpu::Color::WHITE);
    Ok((style, background))
}

/// Logs the compiled/skipped layer counts `style_spec` itself recorded for
/// the compile that just finished (`crate::style_spec::compile_diagnostics`,
/// last-compile-wins and process-global -- see that function's own doc
/// comment), and warns loudly when the skip share is high enough to be a
/// schema mismatch rather than ordinary unsupported constructs.
///
/// This is a COMPILE-time count: it says how many of the style's own JSON
/// layers `parse_and_compile` could turn into a `StyleLayer` at all, which
/// does not depend on which tiles are in play. It cannot by itself catch
/// "right layer types, wrong vocabulary" (a Streets-v8 style compiles the
/// same whether the tiles underneath are Mapbox's or OpenFreeMap's) -- that
/// half is [`log_schema_bind_once`], run against the first tile actually
/// decoded. Printing both is what `TileSchema::resolve` pairing styles and
/// tile sources correctly should make unnecessary in practice, and what
/// the task's own instrumentation requirement asks for regardless.
fn log_compile_diagnostics(schema: TileSchema, choice: &crate::styles::StyleChoice, style: &Style) {
    let diagnostics = crate::style_spec::compile_diagnostics();
    let skipped =
        diagnostics.iter().filter(|d| d.kind == crate::style_spec::DiagnosticKind::SkippedLayer).count();
    let compiled = style.layers.len();
    crate::mlog!(
        "[map] style {:?} ({schema:?} schema): {compiled} StyleLayers compiled, {skipped} JSON layers skipped at compile time",
        choice.name,
    );
    let total = compiled + skipped;
    if total > 0 {
        let skip_pct = 100.0 * skipped as f64 / total as f64;
        // Half or more of a basemap's declared layers failing to compile at
        // all is not this crate's normal unsupported-property noise (a few
        // percent, historically) -- it is the shape a style/schema mismatch
        // would leave if one ever slipped past `TileSchema`'s pairing.
        if skip_pct >= 50.0 {
            crate::mlog!(
                "[map] WARNING: {skipped}/{total} layers ({skip_pct:.0}%) skipped at compile \
                 time for {:?} -- this looks like a style/schema mismatch, not ordinary \
                 unsupported properties",
                choice.name,
            );
        }
    }
}

/// The RUNTIME half of the schema check `log_compile_diagnostics` cannot
/// do: of the layers that DID compile, how many actually bind to a
/// `source-layer` present in a real decoded tile, versus how many name one
/// that never shows up because the tile came from the wrong vocabulary.
/// This is exactly the `layers alive dead` measurement `crate::styles`'
/// module doc tabulates by hand for each vendored fixture (e.g. Streets v8
/// over OMT tiles: 134 layers, 11 alive, 122 dead) -- computed here, live,
/// against whatever tile this build's own `TileSchema` actually fetched.
///
/// Called once, against the first vector tile decoded after a style
/// compiles (initial launch or a switch) -- see `MapApp::schema_checked`.
/// A single tile is enough: source-layer NAMES are static per source, not
/// per tile, so whether `"road"` exists in this tile says the same thing
/// about `"road"` in every other tile from the same source.
fn log_schema_bind_once(style: &Style, tile: &DecodedTile) {
    let mut named = 0usize;
    let mut alive = 0usize;
    // Every `(id, kind, source-layer)` that missed -- named, not merely
    // counted, because a bare count cannot tell a WARNING-worthy schema
    // mismatch apart from a colour-bearing fill dying for a real,
    // zoom-legitimate reason (see this function's own doc comment and the
    // defect this was added to diagnose: which of the dead layers are fills
    // that should be carrying colour).
    let mut dead_layers: Vec<(&str, &'static str, &str)> = Vec::new();
    for layer in &style.layers {
        let source_layer = match layer {
            crate::style::StyleLayer::Fill { source_layer, .. }
            | crate::style::StyleLayer::Line { source_layer, .. }
            | crate::style::StyleLayer::Symbol { source_layer, .. } => source_layer,
            // Every other variant (`Background`, and the imagery source's
            // own layer) names no vector source-layer and cannot be dead in
            // this sense -- excluded from the ratio rather than counted as
            // an automatic match. A wildcard, deliberately, rather than
            // naming the imagery variant: `tests/raster_plan_parity.rs`'s
            // `no_shell_walks_style_layer_raster_to_resolve_opacity_itself`
            // bans this shell from destructuring it outside `raster_plan`,
            // and this check has no opinion on raster opacity at all.
            _ => continue,
        };
        named += 1;
        if tile.layers.iter().any(|l| &l.name == source_layer) {
            alive += 1;
        } else {
            dead_layers.push((layer.id(), layer.kind(), source_layer));
        }
    }
    let dead = named - alive;
    crate::mlog!(
        "[map] schema bind check (richest z10+ tile, {} source-layers in it): {alive} alive, {dead} dead of {named} vector layers",
        tile.layers.len(),
    );
    if !dead_layers.is_empty() {
        crate::mlog!("[map] dead layers (compiled, but their source-layer is absent from this tile):");
        for (id, kind, source_layer) in &dead_layers {
            crate::mlog!("[map]   id={id:<40} kind={kind:<9} source-layer={source_layer}");
        }
    }
    if named > 0 && dead * 2 > named {
        crate::mlog!(
            "[map] WARNING: {dead}/{named} compiled vector layers never matched a source-layer \
             in a z10+ tile with {} of them -- style and tile source disagree on vocabulary (this is the \
             122-of-134-skipped failure mode `ios/src/styles.rs` documents)",
            tile.layers.len(),
        );
    }
}

/// What this app's [`crate::render::space::Space`] starts as.
///
/// `[FROM NATIVE]` -- `map_view::space_default`, unchanged down to the
/// variable names: `std::env::var` compiles and runs the same way here as
/// it does on the desktop (it simply returns `Err` in an app sandbox with
/// nothing set), so the three knobs stay available for a device run
/// launched from Xcode with a scheme environment variable.
///
/// Stars default OFF, as native: the 2026-09-29 ruling ("on all maps,
/// default state: no terminator, plain maps") supersedes the 2026-09-24
/// report that turned them on here. `GPUI_MAP_STARS=1` (a scheme
/// environment variable) still turns them on. The terminator follows the
/// engine's own default, now off (`Atmosphere::default().night_brightness`
/// is 1.0); `GPUI_MAP_NIGHT=0.25` turns it on.
fn space_default() -> crate::render::space::Space {
    let star_intensity = std::env::var("GPUI_MAP_STARS")
        .ok()
        .and_then(|value| value.trim().parse::<f32>().ok())
        .filter(|value| value.is_finite())
        .unwrap_or(0.0);
    let atmosphere_intensity = std::env::var("GPUI_MAP_ATMOSPHERE")
        .ok()
        .and_then(|value| value.trim().parse::<f32>().ok())
        .filter(|value| value.is_finite())
        .unwrap_or(1.0);
    let night_brightness = std::env::var("GPUI_MAP_NIGHT")
        .ok()
        .and_then(|value| value.trim().parse::<f32>().ok())
        .filter(|value| value.is_finite())
        .unwrap_or_else(|| crate::render::atmosphere::Atmosphere::default().night_brightness);
    crate::render::space::Space {
        star_intensity,
        atmosphere: crate::render::atmosphere::Atmosphere {
            intensity: atmosphere_intensity,
            night_brightness,
            ..crate::render::atmosphere::Atmosphere::default()
        },
        ..crate::render::space::Space::default()
    }
}

/// This build's imagery source, read once from the environment -- the same
/// pattern [`space_default`] uses (`GPUI_MAP_STARS` etc.) and, like it,
/// OFF by default: no template ships with this repository (this crate must
/// never commit tile imagery or a token), so a launch that sets none of
/// `GPUI_MAP_IMAGERY`/`_TILE_SIZE`/`_MAX_ZOOM` fetches nothing and draws
/// exactly as it did before this task. `[FROM WASM]` -- `web/src/app.rs`'s
/// `raster_config`/`RasterConfig`, an environment variable in place of a
/// `?imagery=` query param because there is no query string here. Defaults
/// match that file's: 256 is `examples/satellite-style.json`'s own
/// `tileSize` (see that file's note on why not 512), and 22 is Mapbox's own
/// satellite ceiling.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RasterConfig {
    /// `{z}`/`{x}`/`{y}` template, or empty for "no imagery source".
    pub template: String,
    pub tile_size: u32,
    pub max_zoom: u8,
    /// The imagery's credit, when the source states one.
    pub credit: Option<String>,
}

fn raster_config_env() -> RasterConfig {
    RasterConfig {
        credit: None,
        template: std::env::var("GPUI_MAP_IMAGERY").unwrap_or_default(),
        tile_size: std::env::var("GPUI_MAP_IMAGERY_TILE_SIZE")
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok())
            .unwrap_or(256),
        max_zoom: std::env::var("GPUI_MAP_IMAGERY_MAX_ZOOM")
            .ok()
            .and_then(|value| value.trim().parse::<u8>().ok())
            .unwrap_or(22),
    }
}

/// The style's own raster source, as the desktop's `MapView` draws a
/// style's raster layers (`examples/styles-open/open-terrain-style.json`'s
/// shaded relief, Liberty's): the first `raster` source with a tile
/// template that a `raster` layer draws. Its `tileSize` (MapLibre's default
/// 512) and `maxzoom` (22) as declared. `None` for a style with none, or
/// one whose raster source names no tiles (the satellite style: its
/// imagery is the host's to choose, [`MapApp::set_raster_override`]).
pub fn style_raster(json: &[u8]) -> Option<RasterConfig> {
    let v: serde_json::Value = serde_json::from_slice(json).ok()?;
    let sources = v.get("sources")?.as_object()?;
    let drawn: HashSet<&str> = v.get("layers")?.as_array()?.iter().filter(|l| l.get("type").and_then(|t| t.as_str()) == Some("raster")).filter_map(|l| l.get("source").and_then(|s| s.as_str())).collect();
    sources.iter().filter(|(name, _)| drawn.contains(name.as_str())).find_map(|(_, src)| {
        if src.get("type").and_then(|t| t.as_str()) != Some("raster") {
            return None;
        }
        let template = src.get("tiles")?.as_array()?.first()?.as_str()?.to_string();
        Some(RasterConfig {
            template,
            tile_size: src.get("tileSize").and_then(|t| t.as_u64()).unwrap_or(512) as u32,
            max_zoom: src.get("maxzoom").and_then(|t| t.as_u64()).unwrap_or(22).min(24) as u8,
            credit: src.get("attribution").and_then(|a| a.as_str()).map(str::to_string),
        })
    })
}

/// The imagery this frame draws from: the host's override, else the
/// `GPUI_MAP_IMAGERY` environment, else the style's own raster source.
fn effective_raster(over: Option<&RasterConfig>, style_json: &[u8]) -> RasterConfig {
    if let Some(o) = over {
        return o.clone();
    }
    let env = raster_config_env();
    if !env.template.is_empty() {
        return env;
    }
    style_raster(style_json).unwrap_or(env)
}

/// The style's `background-color` as the zoom-STOP PAIR the shader blends,
/// for the integer zoom bucket the tiles are resolved at.
///
/// `[FROM WASM]` -- `web/src/app.rs`'s `background_pair`, read off
/// `style.layers` directly rather than through `compile_style`'s
/// `match_tile`, which resolves `value_at` -- a single already-lerped
/// colour -- and throws the pair away. See `render::space::backdrop_plan`
/// for why the pair, not the lerp, is what the backdrop mesh is baked from.
fn background_pair(style: &Style, zoom: u8) -> Option<(crate::style::Rgba, crate::style::Rgba)> {
    style.layers.iter().find_map(|layer| match layer {
        crate::style::StyleLayer::Background { color, .. } => Some(color.value_at_pair(zoom)),
        _ => None,
    })
}

impl MapApp {
    fn new() -> anyhow::Result<MapApp> {
        let started = Instant::now();
        // Picked BEFORE any style compiles -- see `TileSchema::resolve`.
        // Never logs a token.
        let (schema, tile_url) = TileSchema::resolve();
        let styles = schema.styles();
        // Compiled BEFORE there is a window -- see `compile_style`.
        let (style, background) = compile_style(styles, 0)?;
        crate::mlog!(
            "[map] style 1/{}: {:?}, {} B, {} layers compiled in {:.0} ms -- {}",
            styles.len(),
            styles[0].name,
            styles[0].json.len(),
            style.layers.len(),
            started.elapsed().as_secs_f64() * 1000.0,
            styles[0].note,
        );
        log_compile_diagnostics(schema, &styles[0], &style);
        crate::mlog!("[map] three fingers on the glass cycles to the next style");

        Ok(MapApp {
            renderer: None,
            fetcher: Fetcher::new(),
            started,
            #[cfg(test)]
            now_override: None,
            mapbox_sprite_json: None,
            mapbox_sprite_png: None,
            style,
            schema,
            schema_tile_url: tile_url.clone(),
            style_source: None,
            basemap_archive: None,
            archive_bounds: None,
            online_inflight: HashSet::new(),
            online_failed: HashSet::new(),
            tile_url,
            schema_checked: false,
            style_index: 0,
            multi_touch_consumed: false,
            last_cover_reach: std::cell::Cell::new(None),
            last_building_reach: std::cell::Cell::new(None),
            camera_control: CameraController::new(Camera {
                // Singapore, Marina Bay. Anywhere would do; this one is
                // dense, and it is where the wasm spike looks, so the two
                // screenshots are comparable.
                center_lon: 103.8555,
                center_lat: 1.2895,
                zoom: 13.0,
                pitch: 0.0,
                bearing: 0.0,
                // Corrected by the first `apply_resize`, before any draw.
                viewport_width_px: 1.0,
                viewport_height_px: 1.0,
                fov_y: Camera::DEFAULT_FOV_Y,
            }, MIN_ZOOM, MAX_ZOOM),
            dpr: 1.0,
            // Corrected by the first `apply_resize`, before any draw --
            // same as `viewport_width_px`/`viewport_height_px` above.
            top_inset_pt: 0.0,
            bottom_inset_pt: 0.0,
            nav: crate::nav::host::NavHost::new(),
            speech: crate::speech::SpeechOutput::default(),
            nav_autostart: false,
            nav_prompts: 0,
            speaker_on: true,
            clock_skew_ms: 0.0,
            background,
            space: space_default(),
            decoded: HashMap::new(),
            gpu_tiles: HashMap::new(),
            inflight: HashSet::new(),
            arrived: Vec::new(),
            wanted: crate::fetch_queue::Wanted::new(),
            stale_results_dropped: 0,
            missing: crate::raster_retention::AbsentTiles::new(),
            retries: crate::fetch_policy::RetryTable::new(),
            retained: HashSet::new(),
            raster_decoded: crate::raster_retention::DecodedImagery::new(),
            raster_pending: HashSet::new(),
            raster_missing: crate::raster_retention::AbsentTiles::new(),
            raster_retries: crate::fetch_policy::RetryTable::new(),
            raster_pipeline: None,
            raster_squares: HashMap::new(),
            raster_tiles: HashMap::new(),
            raster: effective_raster(None, styles[0].style_json()),
            raster_override: None,
            touches: Vec::new(),
            drag_from: None,
            drag_samples: std::collections::VecDeque::new(),
            pan_inertia: None,
            drag_segment_started_ms: 0.0,
            drag_segment_from_pinch: false,
            pinch: None,
            pinch_points: None,
            rotate_samples: RotateSamples::default(),
            last_advance_ms: None,
            last_tap: None,
            resize_pending: true,
            needs_redraw: true,
            overlays: crate::overlay::host::ShellOverlays::new(),
            extra_credits: Vec::new(),
            shell_layers: crate::overlay::host::ShellLayers::new(),
            dots: crate::overlay::host::DotLayers::new(),
            dots_gpu: crate::overlay::host::DotLayersGpu::default(),
            dot_worker: None,
            dot_retry_at: None,
            altitude: Default::default(),
            scene_clock: None,
            altitude_wake_at: None,
            altitude_points_drawn: 0,
            altitude_cpu_ms: 0.0,
            data_credit: None,
            dots_set_at: None,
            dots_first_logged: false,
            overlay_demo: false,
            idle_timer_disabled: false,
            frames: 0,
            first_draw_logged: false,
            capture_requested: false,
            captured: None,
            first_body_ms: None,
            last_status_ms: 0.0,
            last_frame_ms: None,
            frame_intervals: MsWindow::default(),
            input_pending_ms: None,
            input_latency: MsWindow::default(),
            surface_metrics: SurfaceMetrics::default(),
            redraw_hook: None,
            manage_idle_timer: true,
            pick_on_tap: false,
            tapped_at: None,
            overlay_label_plans: Vec::new(),
            overlays_above_buildings: false,
            buildings_flat: false,
            label_paint: None,
            label_cover: Default::default(),
            chrome_rtl: false,
            credits_by_host: false,
        })
    }

    fn now_ms(&self) -> f64 {
        #[cfg(test)]
        if let Some(now) = self.now_override {
            return now;
        }
        self.started.elapsed().as_secs_f64() * 1000.0 + self.clock_skew_ms
    }

    /// Runs [`log_schema_bind_once`] exactly once for the currently live
    /// `style`, against whatever decoded tile is available first.
    ///
    /// Called from three places, because "the first tile" means something
    /// different depending on how the live style got here: [`Self::new`]
    /// (nothing decoded yet -- a no-op, the real check comes from
    /// `collect_fetched`'s call once a tile lands), `collect_fetched` (the
    /// launch case: a tile has just finished decoding), and
    /// [`Self::cycle_style`] (the switch case: `decoded` is NOT cleared on
    /// a style switch -- see that method's own doc comment -- so a tile is
    /// very likely ALREADY sitting there and no new fetch will ever arrive
    /// to trigger the `collect_fetched` path).
    fn maybe_log_schema_bind(&mut self) {
        if self.schema_checked {
            return;
        }
        // The richest tile decoded at street-ish zoom, not merely the first:
        // the first to land is often a coarse prefetch or an all-sea tile
        // carrying one or two source-layers, against which a correct style
        // reads as "105/106 dead" (seen on a phone, open-streets over
        // Singapore). Deferred until such a tile exists.
        let Some((_, tile)) = self
            .decoded
            .iter()
            .filter(|(id, tile)| id.z >= 10 && tile.layers.len() >= 4)
            .max_by_key(|(_, tile)| tile.layers.len())
        else {
            return;
        };
        log_schema_bind_once(&self.style, tile);
        self.schema_checked = true;
    }

    /// Advances to the next entry of the ACTIVE schema's own cycle
    /// ([`TileSchema::styles`], never the other schema's), in place, with
    /// no restart and no refetch.
    ///
    /// # Why this is cheap, and where the cost actually is
    ///
    /// A style swap looks like it should be expensive and is not, because of
    /// where the engine draws its own seam. `decoded` holds MVT bytes, which
    /// know nothing about any style; `gpu_tiles` holds tessellated meshes,
    /// which are made of the previous style's colours and widths and are
    /// therefore all stale at once. So the swap is: recompile, recolour the
    /// clear, drop every mesh. Nothing leaves the device and nothing comes
    /// back over the network -- `bake_some` rebuilds each dropped mesh from
    /// the bytes already in `decoded`, at [`BAKES_PER_FRAME`] tiles and
    /// [`BAKE_BUDGET_MS`] per frame, exactly as it does after a zoom-bucket
    /// crossing. A full cover is ~12 tiles, so the map is whole again within
    /// two frames of the finger lifting.
    ///
    /// Dropping the `GpuTile`s drops their buffers with them, which is the
    /// one thing that must not be forgotten: `TrackedBuffer` accounts for GPU
    /// memory on `Drop`, and leaving a stale mesh keyed by tile would leave
    /// the allocator's books believing the old style is still resident while
    /// the new one is baked beside it.
    ///
    /// A style that fails to compile leaves the live one alone. There is no
    /// reason for a bad JSON in the binary to take the map down mid-gesture,
    /// and a switcher that can strand the user on a blank screen is worse
    /// than no switcher.
    fn cycle_style(&mut self) {
        // `self.schema` never changes here -- cycling stays WITHIN whichever
        // schema's set `TileSchema::resolve` picked at launch (see
        // `MapApp::schema`'s doc comment); there is no gesture that crosses
        // to the other tile source.
        let next = (self.style_index + 1) % self.schema.styles().len();
        self.select_style(next);
    }

    /// Compiles and installs entry `next` of this schema's style set; on a
    /// compile failure logs and keeps the current style. `false` then, or
    /// when `next` is out of range.
    fn select_style(&mut self, next: usize) -> bool {
        let styles = self.schema.styles();
        let Some(choice) = styles.get(next) else { return false };
        let started = Instant::now();
        let (style, background) = match compile_style(styles, next) {
            Ok(pair) => pair,
            Err(e) => {
                crate::mlog!("[style] {:?} did not compile, staying put: {e:#}", choice.name);
                return false;
            }
        };
        self.style = style;
        self.background = background;
        self.style_index = next;
        self.refresh_raster();
        self.refresh_source();
        // A new live style needs its own bind check against the next tile.
        self.schema_checked = false;
        // Every mesh on the GPU was tessellated with the previous style's
        // paint; none of it survives. `decoded` deliberately does NOT.
        let dropped = self.gpu_tiles.len();
        self.gpu_tiles.clear();
        crate::mlog!(
            "[style] {}/{} {:?}: {} layers in {:.0} ms, {} meshes dropped, {} tiles kept decoded -- {}",
            next + 1,
            styles.len(),
            choice.name,
            self.style.layers.len(),
            started.elapsed().as_secs_f64() * 1000.0,
            dropped,
            self.decoded.len(),
            choice.note,
        );
        log_compile_diagnostics(self.schema, choice, &self.style);
        // `decoded` was NOT cleared above, so a bind check can very likely
        // run right now instead of waiting on a fetch that will never come.
        self.maybe_log_schema_bind();
        true
    }

    /// Re-resolves the imagery in force; a change of source drops every
    /// imagery tile of the old one (decoded, on the GPU, pending, absent).
    fn refresh_raster(&mut self) {
        let next = effective_raster(self.raster_override.as_ref(), self.schema.styles()[self.style_index].style_json());
        if next == self.raster {
            return;
        }
        self.raster = next;
        self.raster_decoded = crate::raster_retention::DecodedImagery::new();
        self.raster_pending.clear();
        self.raster_missing = crate::raster_retention::AbsentTiles::new();
        self.raster_retries = crate::fetch_policy::RetryTable::new();
        self.raster_tiles.clear();
        self.needs_redraw = true;
    }

    /// Re-resolves where the style's vector tiles come from: the schema's
    /// source, or the style's own (a PMTiles archive). A change of source
    /// drops every vector tile of the old one -- the two are different
    /// schemas, so no tile of one may draw under the other's style.
    fn refresh_source(&mut self) {
        let src = crate::styles::style_source(self.schema.styles()[self.style_index].name);
        self.style_source = src;
        let url = match (src, self.active_archive()) {
            (Some(s), _) => s.tiles.to_string(),
            (None, Some(a)) => format!("{}{}/{{z}}/{{x}}/{{y}}", crate::pmtiles_http::PREFIX, a.archive),
            (None, None) => self.schema_tile_url.clone(),
        };
        if url == self.tile_url {
            return;
        }
        self.tile_url = url;
        self.drop_vector_tiles();
    }

    /// Drops every vector tile, for a change of where they come from.
    fn drop_vector_tiles(&mut self) {
        self.online_inflight.clear();
        self.online_failed.clear();
        self.decoded.clear();
        self.gpu_tiles.clear();
        self.inflight.clear();
        self.retained.clear();
        self.missing = crate::raster_retention::AbsentTiles::new();
        self.retries = crate::fetch_policy::RetryTable::new();
        self.needs_redraw = true;
    }

    /// The vector source's highest zoom: the archive's own once it is open,
    /// else what the style's source declares, else the schema's.
    fn source_max_zoom(&self) -> u8 {
        match (self.style_source, self.active_archive()) {
            (Some(s), _) => crate::pmtiles_http::max_zoom(s.archive).unwrap_or(s.max_zoom),
            (None, Some(a)) => crate::pmtiles_http::max_zoom(&a.archive).unwrap_or(self.schema.source_max_zoom()),
            (None, None) => self.schema.source_max_zoom(),
        }
    }

    /// The host's archive, when it is what the basemap draws from: only
    /// under an OpenMapTiles style without a source of its own -- the
    /// archive is OpenMapTiles, and any other style would draw nothing.
    fn active_archive(&self) -> Option<&BasemapArchive> {
        active_archive(self.style_source, self.schema, &self.basemap_archive)
    }

    /// Draws the basemap from `archive` (`file://<path>`, an absolute path
    /// or an `https://` URL of an OpenMapTiles PMTiles archive) instead of
    /// the schema's online tiles; `None` returns to them. The style, the
    /// camera and the overlays stay; the old source's tiles are dropped.
    /// Outside the archive's bounds the map is blank: an archive is a
    /// closed set of tiles.
    ///
    /// With `online_outside`, a tile not wholly inside the archive's bounds
    /// comes from the online tiles instead -- the archive holds only its
    /// region, so its border tiles are half empty -- and from the archive
    /// again when the online fetch fails (no network).
    pub fn set_basemap_archive(&mut self, archive: Option<BasemapArchive>) {
        if self.basemap_archive == archive {
            return;
        }
        self.archive_bounds = archive.as_ref().filter(|a| a.online_outside).and_then(|a| match crate::pmtiles_http::bounds(&a.archive) {
            Ok(b) => Some(b),
            Err(err) => {
                crate::mlog!("[map] archive bounds: {}; drawing the archive alone", crate::log::scrub(&err));
                None
            }
        });
        self.basemap_archive = archive;
        let url = self.tile_url.clone();
        self.refresh_source();
        // Same archive URL, other `online_outside`: tiles move source too.
        if self.tile_url == url {
            self.drop_vector_tiles();
        }
    }

    /// Where one vector tile comes from: the source in force
    /// (`tile_url`), or the online tiles for a tile outside an archive that
    /// allows them (see [`Self::set_basemap_archive`]). `true` when online.
    fn vector_url(&self, tile: TileId) -> (String, bool) {
        let online = match (self.active_archive(), self.archive_bounds) {
            (Some(a), Some(b)) if a.online_outside => tile_from_online(tile, b, self.online_failed.contains(&tile)),
            _ => false,
        };
        let template = if online { &self.schema_tile_url } else { &self.tile_url };
        (fill_template(template, tile), online)
    }

    /// The host's imagery source (a satellite basemap's tiles, a COG's or a
    /// WMS's rendered as `{z}/{x}/{y}`), drawn under the style's `raster`
    /// layers as the desktop draws a raster source; `None` returns to the
    /// style's own.
    pub fn set_raster_override(&mut self, raster: Option<RasterConfig>) {
        self.raster_override = raster;
        self.refresh_raster();
    }

    /// `[FROM WASM]` -- `map_view::display_zoom`. Deliberately NOT clamped
    /// to the source ceiling: it selects layers by `minzoom`/`maxzoom` and
    /// resolves every zoom-dependent paint value, and Mapbox/MapLibre
    /// resolve style against the MAP's zoom, never the tile's.
    fn display_zoom(&self) -> u8 {
        self.camera_control.camera().zoom.floor().clamp(0.0, u8::MAX as f64) as u8
    }

    fn tile_zoom(&self) -> u8 {
        self.display_zoom().min(self.source_max_zoom())
    }

    /// `[FROM WASM]` -- `render::cover_plan::flat_cover`, the one
    /// pitch-aware, frustum-trimmed planar cover every shell shares, at the
    /// tile zoom, through `render::cover_plan::tile_cover` so the GLOBE gets
    /// first say. Records the flat cover's `reach` in `last_cover_reach` for
    /// the horizon fade.
    fn cover(&self) -> Vec<TileId> {
        let z = self.tile_zoom();
        let camera = self.camera_control.camera();
        let mut reach = None;
        // The building floor (the far-building fix), shared: while buildings
        // draw, the near ground stays on tiles that carry building data.
        let request = crate::render::frame_plan::building_cover_request(&self.style.layers, camera.zoom, z);
        let mut building_reach = None;
        let tiles = crate::render::cover_plan::tile_cover(z, &self.globe_cover(), || {
            let cover = crate::render::cover_plan::flat_cover(camera, request);
            reach = cover.reach;
            building_reach = cover.building_reach;
            cover.tiles
        });
        self.last_cover_reach.set(reach);
        self.last_building_reach.set(building_reach);
        tiles
    }

    /// The globe's cover for THIS call, or `GlobeCover::FLAT` above its band.
    fn globe_cover(&self) -> crate::render::globe::GlobeCover {
        crate::render::globe::GlobeCover::new(&crate::render::globe::globe_frame(self.camera_control.camera()))
    }

    /// Any zoom's cover, without the reach bookkeeping -- the coarse
    /// prefetch cover asks at `z` below the tile zoom, with its distance
    /// LOD shifted down by the same amount (`cover_plan::CoverZoom::coarser`).
    ///
    /// Routed through `render::cover_plan::tile_cover` so the GLOBE gets
    /// first say -- this used to call `geo::visible_tiles` directly and
    /// unconditionally, which is the same blank-hemisphere bug
    /// `web/src/app.rs`'s `cover()` had: a flat Mercator bbox cannot cover a
    /// sphere showing a hemisphere.
    fn cover_at(&self, request: crate::render::cover_plan::CoverZoom) -> Vec<TileId> {
        crate::render::cover_plan::tile_cover(request.zoom, &self.globe_cover(), || {
            crate::render::cover_plan::flat_cover(self.camera_control.camera(), request).tiles
        })
    }

    /// The coarse prefetch cover: the same viewport, covered at
    /// `tile_zoom - PREFETCH_ZOOM_DELTA`. Retained and never drawn on its
    /// own account -- it reaches the screen only by being selected as an
    /// ancestor stand-in, which is the entire point of loading it early.
    fn prefetch_cover(&self) -> Vec<TileId> {
        let tile_zoom = self.tile_zoom();
        let coarse = tile_zoom.saturating_sub(PREFETCH_ZOOM_DELTA);
        if coarse >= tile_zoom {
            return Vec::new();
        }
        self.cover_at(crate::render::cover_plan::CoverZoom::coarser(self.camera_control.camera(), tile_zoom, coarse))
    }

    /// THE IMAGERY DECISION for this frame -- whether it draws, at what
    /// opacity, and at what cover zoom -- through the one function every
    /// imagery-drawing shell shares, `render::raster_plan::raster_plan`.
    /// Never re-derived here: see `tests/raster_plan_parity.rs`, which
    /// fails the build the moment this file (or any other shell) walks
    /// `StyleLayer::Raster` or computes a cover zoom on its own instead of
    /// reading this. `[FROM WASM]` -- `web/src/app.rs`'s `raster_plan`,
    /// unchanged in shape; `self.raster` is this crate's `?imagery=`
    /// equivalent (an environment variable, since there is no query
    /// string -- see that function's own doc comment).
    fn raster_plan(&self) -> crate::render::raster_plan::RasterPlan {
        let config = &self.raster;
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

    /// The imagery cover this frame wants, empty when `plan.opacity` is
    /// `None`. Routed through `cover_plan::tile_cover` exactly as
    /// `cover_at` is, so the globe gets first say over imagery too --
    /// `[FROM WASM]`, `web/src/app.rs`'s `raster_cover`.
    ///
    /// Returns the cover's cut too (the flat cover's `reach`, `None` when
    /// nothing draws or nothing was cut), for `cover_plan::frame_fog`.
    fn raster_cover(&self, plan: &crate::render::raster_plan::RasterPlan) -> (Vec<TileId>, Option<f64>) {
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

    /// `[FROM WASM]` -- decides, in ONE traversal, what is drawn, what is
    /// kept, and where the holes are. Deciding visibility first and hunting
    /// for stand-ins in a later pass is exactly how a stand-in ends up drawn
    /// but never uploaded.
    fn plan_frame(&self) -> FramePlan {
        let display_zoom = self.display_zoom();
        let cover = self.cover();
        let mut plan = FramePlan {
            cover: cover.clone(),
            cover_len: cover.len(),
            cover_zooms: crate::render::cover_plan::zoom_span(&cover),
            ..Default::default()
        };

        // A mesh baked at another zoom bucket still counts as coverage:
        // `hasData()` is deliberately loose in all three reference engines.
        let has_mesh = |t: TileId| self.gpu_tiles.contains_key(&t);
        let source_max_zoom = self.source_max_zoom();

        // Pass 1 -- the stand-ins, coarse-first so the ideal tiles below
        // paint over them.
        let mut queued: HashSet<TileId> = HashSet::new();
        let mut stand_ins: Vec<TileId> = Vec::new();
        for &tile in &cover {
            plan.retained.insert(tile);
            if has_mesh(tile) {
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
            // that ends up drawn -- a partially-loaded intermediate level is
            // what makes the next zoom-out step already covered.
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
                None => plan.holes += 1,
            }
        }

        // Coarse-first within the stand-in set too: a z9 ancestor must not
        // paint over a z12 one standing in for a smaller area.
        stand_ins.sort_by_key(|t| t.z);
        plan.draws = stand_ins;

        // Pass 2 -- every cover tile that has something of its own, on top.
        for &tile in &cover {
            if let Some(g) = self.gpu_tiles.get(&tile) {
                if g.baked_zoom == display_zoom {
                    plan.fresh += 1;
                } else {
                    plan.stale += 1;
                }
                plan.draws.push(tile);
            }
        }

        for t in self.prefetch_cover() {
            plan.retained.insert(t);
        }
        plan
    }

    /// The camera as it is right now, mid-ease included.
    fn camera(&self) -> &Camera {
        self.camera_control.camera()
    }

    /// One drag step through the controller, grabbed at `anchor` (the
    /// finger's position before the move, in points).
    fn pan_by_pixels(&mut self, dx: f32, dy: f32, anchor: (f32, f32)) {
        // A compact credit plate collapses on the first pan (MapLibre's
        // `_updateCompactMinimize`, on drag).
        if let Some(renderer) = self.renderer.as_mut() {
            if let Some((_, compact)) = renderer.attribution_hit {
                renderer.attribution_compact.map_moved(compact);
            }
        }
        self.camera_control.apply(CameraIntent::PanBy { dx_px: dx, dy_px: dy, anchor_px: anchor });
    }

    /// `[FROM WASM]` -- `map_view::advance_pan_inertia`. Drives
    /// `pan_by_pixels` directly rather than steering a target and chasing
    /// it, so the curve applied here is exactly the curve the user sees.
    fn advance_pan_inertia(&mut self, now_ms: f64) {
        let Some(inertia) = self.pan_inertia else { return };
        let ((dx, dy), finished) = pan_inertia_step(&inertia, now_ms);
        let centre = (self.camera().viewport_width_px / 2.0, self.camera().viewport_height_px / 2.0);
        self.pan_by_pixels(dx as f32, dy as f32, centre);
        if finished {
            self.pan_inertia = None;
            return;
        }
        // Accumulate the amount ASKED for, not the f32 `pan_by_pixels`
        // received: the running total is what the final frame subtracts from
        // `offset_*` to land exactly.
        self.pan_inertia = Some(PanInertia {
            applied_x: inertia.applied_x + dx,
            applied_y: inertia.applied_y + dy,
            ..inertia
        });
    }

    // -- the network ----------------------------------------------------

    /// This frame's wanted set, most urgent first: the vector cover and the
    /// imagery cover (both [`FetchClass::Visible`], interleaved by nearness
    /// within each), then the coarse prefetch cover. Every list is
    /// nearest-the-eye first -- the fetch order only; `cover()` is drawn in
    /// its own order.
    ///
    /// [`FetchClass::Visible`]: crate::fetch_queue::FetchClass::Visible
    fn wanted_tiles(&self) -> crate::fetch_queue::Wanted<TileKey> {
        use crate::fetch_queue::FetchClass;
        let camera = self.camera_control.camera();
        let key = |kind| move |t: TileId| (t, kind);
        let cover: Vec<TileKey> = crate::render::cover_plan::nearest_first(&self.cover(), camera)
            .into_iter()
            .map(key(TileKind::Vector))
            .collect();
        let raster: Vec<TileKey> = if self.raster.template.is_empty() {
            Vec::new()
        } else {
            let (tiles, _) = self.raster_cover(&self.raster_plan());
            crate::render::cover_plan::nearest_first(&tiles, camera).into_iter().map(key(TileKind::Raster)).collect()
        };
        let coarse: Vec<TileKey> = crate::render::cover_plan::nearest_first(&self.prefetch_cover(), camera)
            .into_iter()
            .map(key(TileKind::Vector))
            .collect();
        let mut wanted = crate::fetch_queue::Wanted::new();
        wanted.add(FetchClass::Visible, &cover);
        wanted.add(FetchClass::Visible, &raster);
        wanted.add(FetchClass::Coarse, &coarse);
        wanted
    }

    /// The request pass, vector and imagery together.
    ///
    /// First the fetcher is RETARGETED to this frame's wanted set
    /// ([`Self::wanted_tiles`]): every pending job the camera no longer wants
    /// is dropped and its in-flight mark cleared, so it is asked for again if
    /// the camera comes back, and every one it still wants is re-ranked. Then
    /// every wanted tile that is not decoded, in flight, known absent or
    /// backing off is queued at its priority. Workers take the most urgent
    /// pending job, not the oldest -- see `net::Fetcher`'s own doc for the
    /// FIFO this replaced.
    ///
    /// The wanted set is kept (`self.wanted`) for [`Self::decode_arrived`],
    /// which uses it to discard stale bytes undecoded.
    fn request_missing_tiles(&mut self) {
        let wanted = self.wanted_tiles();
        let raster_template = self.raster.template.clone();
        for (tile, kind) in self.fetcher.retarget(&wanted) {
            match kind {
                TileKind::Vector => self.inflight.remove(&tile) | self.online_inflight.remove(&tile),
                TileKind::Raster => self.raster_pending.remove(&tile),
            };
        }
        for ((tile, kind), priority) in wanted.in_priority_order() {
            let is_prefetch = priority.class != crate::fetch_queue::FetchClass::Visible;
            match kind {
                TileKind::Vector => {
                    if !self.should_request_vector(tile) {
                        continue;
                    }
                    let (url, online) = self.vector_url(tile);
                    if online {
                        self.online_inflight.insert(tile);
                    }
                    // `false` means the fetcher already holds this key (its
                    // answer is still coming), so the in-flight mark is
                    // right either way.
                    self.fetcher.request(tile, kind, priority, is_prefetch, url);
                    self.inflight.insert(tile);
                    self.retries.on_requested(tile);
                }
                TileKind::Raster => {
                    if !self.should_request_raster(tile) {
                        continue;
                    }
                    let url = fill_template(&raster_template, tile);
                    self.fetcher.request(tile, kind, priority, false, url);
                    self.raster_pending.insert(tile);
                    self.raster_retries.on_requested(tile);
                }
            }
        }
        self.wanted = wanted;
    }

    /// Whether `tile` is worth asking the vector source for right now: not
    /// decoded, not in flight, not known absent, and not still waiting out
    /// the backoff from a transient failure.
    fn should_request_vector(&self, tile: TileId) -> bool {
        !self.decoded.contains_key(&tile)
            && !self.inflight.contains(&tile)
            && !self.missing.contains(&tile)
            && self.retries.may_request(tile, self.now_ms())
    }

    /// The imagery twin of [`Self::should_request_vector`].
    fn should_request_raster(&self, tile: TileId) -> bool {
        !self.raster_decoded.contains(&tile)
            && !self.raster_pending.contains(&tile)
            && !self.raster_missing.contains(&tile)
            && self.raster_retries.may_request(tile, self.now_ms())
    }

    /// After both request passes: parks every retry whose backoff expired
    /// but that neither pass asked for (the camera moved on), so
    /// [`Self::next_retry_wake_ms`] is always in the future once a frame has
    /// run and a retry never keeps the loop spinning. Called unconditionally,
    /// including when the request pass wanted no imagery.
    fn park_overdue_retries(&mut self) {
        let now = self.now_ms();
        self.retries.park_overdue(now);
        self.raster_retries.park_overdue(now);
    }

    /// Whether a retry's backoff has expired, so the loop owes a frame to
    /// re-request it.
    fn retry_is_due(&self) -> bool {
        self.next_retry_wake_ms().is_some_and(|due| due <= self.now_ms())
    }

    /// When the loop must next wake for a retry, on the `now_ms` clock.
    fn next_retry_wake_ms(&self) -> Option<f64> {
        match (self.retries.next_wake_ms(), self.raster_retries.next_wake_ms()) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Whether `Renderer::fade_wake_at`'s deadline has arrived -- the same
    /// shape as `retry_is_due`, for the same reason: a `ControlFlow::WaitUntil`
    /// wake is not itself a reason to draw, only a promise that THIS is what
    /// to check once it fires.
    fn fade_wake_is_due(&self) -> bool {
        self.renderer.as_ref().is_some_and(|r| r.fade_wake_at.is_some_and(|at| Instant::now() >= at))
    }

    /// Collects whatever the workers finished. Absences, failures and sprite
    /// bytes are handled at once (they are cheap); tile BODIES are only
    /// buffered, for [`Self::decode_arrived`] to decode under a budget once
    /// this frame's request pass has said what is still wanted.
    ///
    /// This used to decode every body the moment it was drained, whatever it
    /// was. After a street-zoom pan and a zoom-out the device log had 1,526
    /// requests outstanding, and every stale z14 tile that came back was
    /// decoded on this thread: `decoded` climbed 393 -> 1,323, the frame rate
    /// fell to 9 fps with a 5.3 s stall, and the 12 tiles actually wanted sat
    /// all HOLE for over 10 s.
    fn collect_fetched(&mut self) {
        for fetched in self.fetcher.drain() {
            match fetched {
                Fetched::Body(tile, kind, is_prefetch, bytes) => {
                    if self.first_body_ms.is_none() {
                        self.first_body_ms = Some(self.now_ms());
                    }
                    self.arrived.push(((tile, kind), (is_prefetch, bytes)));
                }
                other => self.handle_fetched(other),
            }
        }
    }

    /// Decodes buffered tile bodies, most urgent first, until
    /// [`DECODE_BUDGET_MS`] is spent; the rest wait for the next frame (and
    /// keep their in-flight marks, so they are not asked for twice). A body
    /// for a tile that is in neither this frame's wanted set nor the retained
    /// set is STALE -- its request was already running when the camera left
    /// -- and is discarded undecoded, its in-flight mark cleared so the
    /// camera coming back asks for it again. Returns whether bodies remain,
    /// which is work only another frame can drain.
    ///
    /// Decoding stays on this thread rather than moving to the fetch workers:
    /// every engine call runs on one thread in this shell, and the browser
    /// shares the same budgeted policy because it has no other thread. The
    /// budget is what bounds a frame; the stale discard is what keeps the
    /// work it rations worth doing.
    fn decode_arrived(&mut self) -> bool {
        if self.arrived.is_empty() {
            return false;
        }
        let retained = &self.retained;
        let triage = self
            .wanted
            .triage(std::mem::take(&mut self.arrived), |(t, kind)| *kind == TileKind::Vector && retained.contains(t));
        for ((tile, kind), _) in triage.stale {
            match kind {
                TileKind::Vector => self.inflight.remove(&tile) | self.online_inflight.remove(&tile),
                TileKind::Raster => self.raster_pending.remove(&tile),
            };
            self.stale_results_dropped += 1;
        }
        let started = Instant::now();
        let mut done = 0;
        for ((tile, kind), (is_prefetch, bytes)) in triage.in_order {
            let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
            if crate::fetch_queue::may_start_another(done, elapsed_ms, DECODE_BUDGET_MS) {
                self.handle_fetched(Fetched::Body(tile, kind, is_prefetch, bytes));
                done += 1;
            } else {
                self.arrived.push(((tile, kind), (is_prefetch, bytes)));
            }
        }
        !self.arrived.is_empty()
    }

    /// One finished request -- [`Self::collect_fetched`]'s body, split out
    /// so a test can hand it a result without a network.
    fn handle_fetched(&mut self, fetched: Fetched) {
        {
            match fetched {
                // A tile asked of a source no longer in force
                // (`refresh_source` emptied the in-flight set) is dropped.
                Fetched::Body(tile, TileKind::Vector, _, _) if !self.inflight.contains(&tile) => {}
                Fetched::Body(tile, TileKind::Vector, _is_prefetch, bytes) => {
                    self.inflight.remove(&tile);
                    if self.online_inflight.remove(&tile) && !self.online_failed.is_empty() {
                        // The network is back: the tiles drawn from the
                        // archive in its absence are fetched online again.
                        for t in std::mem::take(&mut self.online_failed) {
                            self.decoded.remove(&t);
                        }
                    }
                    match crate::mvt::decode(bytes) {
                        Ok(decoded) => {
                            self.retries.on_settled(tile);
                            // A body for a baked tile is a new copy (an
                            // archive tile fetched online again): rebake it.
                            self.gpu_tiles.remove(&tile);
                            self.decoded.insert(tile, decoded);
                            self.maybe_log_schema_bind();
                        }
                        Err(err) => {
                            // Most often a body truncated by a dropped
                            // connection, which a retry fixes. A genuinely
                            // corrupt tile costs one request per backoff cap.
                            let delay = self.retries.on_failure(tile, self.now_ms(), None);
                            crate::mlog!("[map] decode {} failed, retry in {:.1} s: {}", crate::log::tile(tile), delay / 1000.0, crate::log::scrub(&format!("{err:#}")));
                        }
                    }
                }
                Fetched::Missing(tile, TileKind::Vector) => {
                    self.inflight.remove(&tile);
                    self.online_inflight.remove(&tile);
                    self.retries.on_settled(tile);
                    self.missing.insert(tile);
                }
                Fetched::Failed(tile, TileKind::Vector, _) if self.online_inflight.remove(&tile) => {
                    // Offline outside the archive: ask the archive now.
                    self.inflight.remove(&tile);
                    self.online_failed.insert(tile);
                    self.needs_redraw = true;
                }
                Fetched::Failed(tile, TileKind::Vector, err) => {
                    self.inflight.remove(&tile);
                    let delay = self.retries.on_failure(tile, self.now_ms(), err.retry_after_ms);
                    crate::mlog!("[map] fetch {} failed, retry in {:.1} s: {}", crate::log::tile(tile), delay / 1000.0, crate::log::scrub(&err.to_string()));
                }
                // A tile asked of an imagery source no longer in force
                // (`reset_raster` emptied the pending set) is dropped.
                Fetched::Body(tile, TileKind::Raster, _, _) if !self.raster_pending.contains(&tile) => {}
                Fetched::Body(tile, TileKind::Raster, _is_prefetch, bytes) => {
                    self.raster_pending.remove(&tile);
                    let tile_size = self.raster.tile_size;
                    match crate::raster::decode_tile_at(&bytes, tile_size, tile) {
                        Ok(image) => {
                            self.raster_retries.on_settled(tile);
                            self.raster_decoded.insert(tile, Arc::new(image));
                        }
                        Err(err) => {
                            // Never logs the bytes -- see `raster.rs`'s own
                            // `decode_tile` doc comment on why a raster
                            // decode error stays terse. Retried, like the
                            // vector decode above: usually a truncated body.
                            let delay = self.raster_retries.on_failure(tile, self.now_ms(), None);
                            crate::mlog!(
                                "[map] raster decode {} failed, retry in {:.1} s: {}",
                                crate::log::tile(tile),
                                delay / 1000.0,
                                crate::log::scrub(&format!("{err:#}"))
                            );
                        }
                    }
                }
                Fetched::Missing(tile, TileKind::Raster) => {
                    self.raster_pending.remove(&tile);
                    self.raster_retries.on_settled(tile);
                    self.raster_missing.insert(tile);
                }
                Fetched::Failed(tile, TileKind::Raster, err) => {
                    self.raster_pending.remove(&tile);
                    let delay = self.raster_retries.on_failure(tile, self.now_ms(), err.retry_after_ms);
                    crate::mlog!("[map] raster fetch {} failed, retry in {:.1} s: {}", crate::log::tile(tile), delay / 1000.0, crate::log::scrub(&err.to_string()));
                }
                Fetched::SpriteBody(resource, bytes) => {
                    match resource {
                        SpriteResource::Manifest => self.mapbox_sprite_json = Some(bytes),
                        SpriteResource::Image => self.mapbox_sprite_png = Some(bytes),
                    }
                    self.try_install_mapbox_sprites();
                }
                Fetched::SpriteMissing(resource) => {
                    crate::mlog!("[map] sprites: mapbox {resource:?} 404 -- keeping the bundled sheet");
                }
                Fetched::SpriteFailed(resource, err) => {
                    crate::mlog!(
                        "[map] sprites: mapbox {resource:?} fetch failed, keeping the bundled sheet: {err}"
                    );
                }
            }
        }
    }

    /// Once BOTH of the Mapbox sprite sheet's two requests have landed
    /// (see `create_renderer`'s own doc comment on when they are queued),
    /// parses the manifest, slices the atlas -- `sprite::parse_sprite_manifest`
    /// then `sprite::slice_atlas`, the same two calls `load_embedded_sprites`
    /// makes on the bundled bytes, no second parser -- repacks it through
    /// `render::icon_pipeline::pack_icon_atlas` and REPLACES the bundled
    /// sheet's three `Renderer` fields with it.
    ///
    /// A no-op until both bytes are in hand (returns immediately if either
    /// is still `None`), and a no-op forever after either request fails:
    /// once consumed with `Option::take`, a field is never refilled --
    /// there is no retry, and none is needed, since the bundled sheet it
    /// leaves in place already draws every OMT-vocabulary icon.
    ///
    /// Any failure here -- a bad manifest, a PNG that does not match it, no
    /// live `Renderer` yet (unreachable in practice: both requests are
    /// queued from inside `create_renderer`, after the `Renderer` this
    /// method would update already exists) -- costs icons and nothing
    /// else: it is logged, and the bundled sheet stays exactly as it was.
    fn try_install_mapbox_sprites(&mut self) {
        if self.mapbox_sprite_json.is_none() || self.mapbox_sprite_png.is_none() {
            return;
        }
        let json = self.mapbox_sprite_json.take().expect("checked Some above");
        let png = self.mapbox_sprite_png.take().expect("checked Some above");
        let manifest = match crate::sprite::parse_sprite_manifest(&json) {
            Ok(manifest) => manifest,
            Err(err) => {
                crate::mlog!("[map] sprites: mapbox manifest invalid, keeping the bundled sheet: {err:#}");
                return;
            }
        };
        let atlas = match crate::sprite::slice_atlas(&png, &manifest) {
            Ok(atlas) => atlas,
            Err(err) => {
                crate::mlog!(
                    "[map] sprites: mapbox sheet does not match its manifest, keeping the bundled one: {err:#}"
                );
                return;
            }
        };
        let Some(renderer) = self.renderer.as_mut() else {
            crate::mlog!("[map] sprites: mapbox sheet landed with no renderer yet, dropping it");
            return;
        };
        let names: Vec<String> = atlas.names().cloned().collect();
        let (atlas_image, icon_rects) = crate::render::icon_pipeline::pack_icon_atlas(&atlas, &names);
        let icon_atlas_size = atlas_image.dimensions();
        let icon_atlas_bind_group = renderer.icon_pipeline.upload_atlas(&renderer.ctx, &atlas_image);
        crate::mlog!(
            "[map] sprites: mapbox sheet installed, {} entries, packed to {}x{} (replacing the bundled one)",
            names.len(),
            icon_atlas_size.0,
            icon_atlas_size.1,
        );
        renderer.icon_atlas_bind_group = icon_atlas_bind_group;
        renderer.icon_rects = icon_rects;
        renderer.icon_atlas_size = icon_atlas_size;
        self.needs_redraw = true;
    }

    // -- the bake -------------------------------------------------------

    /// `[FROM WASM]` -- bakes at most `BAKES_PER_FRAME` tiles, in strict
    /// priority order: cover tiles with no mesh at all (a hole on screen
    /// right now), then cover tiles a zoom bucket behind, then the coarse
    /// prefetch cover, which is `Optional` in every sense and gets the
    /// leftovers. Without that ordering the coarse cover steals bake slots
    /// from the tiles under the finger and makes the symptom worse.
    ///
    /// Within each class the candidates are nearest-the-eye first
    /// (`render::cover_plan::bake_order`), not `cover()`'s own row-major
    /// `(x, y)` draw order: at 60 degrees of pitch that draw order put the
    /// far rows first, so the ground under the camera was the LAST thing
    /// baked. `cover()`'s draw list itself is untouched -- `bake_order`
    /// only reorders which of its tiles get a bake slot this frame.
    fn bake_some(&mut self) -> bool {
        let Some(renderer) = self.renderer.as_ref() else { return false };
        let display_zoom = self.display_zoom();
        let cover = self.cover();
        let coarse = self.prefetch_cover();
        let camera = self.camera_control.camera();

        let todo: Vec<TileId> = crate::render::cover_plan::bake_order(
            &cover,
            &coarse,
            |t| self.gpu_tiles.get(&t).map(|g| g.baked_zoom),
            |t| self.decoded.contains_key(&t),
            display_zoom,
            camera,
        )
        .into_iter()
        .map(|(t, _)| t)
        .take(BAKES_PER_FRAME)
        .collect();
        if todo.is_empty() {
            return false;
        }
        let budget_from = Instant::now();
        for (n, tile) in todo.into_iter().enumerate() {
            // Between tiles, never inside one -- see `BAKE_BUDGET_MS`. The
            // first tile is unconditional, so the budget can stall progress
            // to a crawl but never to a halt, however slow a single tile is.
            if n > 0 && budget_from.elapsed().as_secs_f64() * 1000.0 >= BAKE_BUDGET_MS {
                break;
            }
            let decoded = &self.decoded[&tile];
            // The engine, in two calls. Exactly what `MapView::bake_tile`
            // runs on a tokio worker -- here it runs on the frame, after the
            // present, under a clock; see [`BAKE_BUDGET_MS`].
            let plan = crate::style::match_tile(decoded, &self.style, display_zoom);
            let mesh = crate::render::tessellate_plan(
                &plan,
                4096,
                crate::render::mesh::COORD_SCALE,
                crate::render::subdivide::Subdivision::for_tile(tile),
            );
            // The symbol half of the same bake, charged to the same budget
            // -- `web/src/app.rs`'s `bake_some`, same call, same reason:
            // `match_tile_symbols` walks the same layers over the same
            // features, and leaving it out of the timing would make
            // `BAKE_BUDGET_MS` describe a frame this function no longer
            // measures. `tile_units_per_px` converts `symbol-spacing` from
            // pixels into the tile's own 0..4096 space and depends on how
            // far the bake zoom is above the tile's own.
            let symbols = Arc::new(crate::style::match_tile_symbols(
                decoded,
                &self.style,
                display_zoom,
                crate::symbol_line::tile_units_per_px(
                    crate::symbol_line::symbol_spacing_zoom(display_zoom, self.tile_zoom(), tile.z),
                    tile.z,
                ),
            ));
            // The buildings, from the same plan and in the same budget:
            // `MapView::bake_tile`'s `tessellate_extrusions`, uploaded
            // beside the flat buffers.
            let extrusion = renderer.pipeline.create_extrusion_buffers(
                &renderer.ctx,
                &crate::render::extrusion::tessellate_extrusions(&plan),
            );

            match self.gpu_tiles.remove(&tile) {
                // Re-bake at a new zoom: the uniform and bind group are
                // still good, and `install_mesh` grows the buffers only if
                // it must.
                Some(existing) => {
                    let buffers = renderer.pipeline.install_basemap_mesh(
                        &renderer.ctx,
                        Some(existing.buffers),
                        &mesh,
                    );
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
                    let (model_buffer, model_bind_group) =
                        renderer.pipeline.create_model_bind_group(
                            &renderer.ctx,
                            glam::Mat4::IDENTITY.to_cols_array_2d(),
                            0.0,
                            1.0,
                            // A placeholder, not a decision: `draw` below
                            // rewrites this uniform -- matrix, blend, dash
                            // scale AND globe params together -- before this
                            // tile is ever submitted for a draw, exactly as
                            // the identity matrix two lines up is a
                            // placeholder. `GlobeParams::FLAT` is what
                            // `map_view.rs`'s own test-fixture bake uses for
                            // the same reason.
                            crate::render::GlobeParams::FLAT,
                        );
                    let buffers = renderer.pipeline.create_basemap_tile_buffers(&renderer.ctx, &mesh);
                    self.gpu_tiles.insert(
                        tile,
                        GpuTile {
                            baked_zoom: display_zoom,
                            buffers,
                            model_buffer,
                            model_bind_group,
                            symbols,
                            extrusion,
                        },
                    );
                }
            }
        }

        true
    }

    /// Eviction, bounded by the DISCARD PILE and never reaching the
    /// retained set. A stand-in is by definition not in the cover, so a
    /// cover-keyed retain is guaranteed to throw away exactly the tile that
    /// is standing in for a hole.
    ///
    /// AND THE DECODED TILES, which nothing bounded until the final fix
    /// wave: `decoded` only ever grew, so panning at 60 degrees -- a cover
    /// of up to 512 tiles -- ratcheted it up until jetsam killed the app.
    /// `web/src/app.rs`'s bound, verbatim: the same `keep` set as the
    /// meshes, and a decoded tile whose mesh is still resident is never
    /// dropped, or the next re-bake would have nothing to bake from.
    fn evict_discard_pile(&mut self, cover: Vec<TileId>) {
        let mut keep: HashSet<TileId> = cover.into_iter().collect();
        keep.extend(self.retained.iter().copied());
        if self.gpu_tiles.len() > MAX_GPU_TILES {
            self.gpu_tiles.retain(|t, _| keep.contains(t));
        }
        if self.decoded.len() > MAX_DECODED_TILES {
            self.decoded.retain(|t, _| keep.contains(t) || self.gpu_tiles.contains_key(t));
        }
    }

    // -- the surface ----------------------------------------------------

    /// Re-sizes the three things that must move together.
    ///
    /// `[FROM WASM]`, and the split it exists to enforce is the same one on
    /// every host: the BACKING STORE is sized in physical pixels, the CAMERA
    /// in logical ones. `Camera`'s viewport is in the same units as `zoom`
    /// (512 logical px per tile at z0) and as `Vertex::half_width_px`, so
    /// feeding it physical pixels would show a quarter of the ground at
    /// three times the scale on a 3x phone and leave every line the wrong
    /// thickness. The engine's own camera needed no change for this,
    /// because it is parameterised by explicit dimensions rather than by an
    /// aspect ratio -- an aspect ratio alone cannot tell "taller window,
    /// same scale, more world" from "same world, stretched".
    fn apply_resize(&mut self) {
        let metrics = self.surface_metrics;
        let Some(renderer) = self.renderer.as_mut() else { return };
        let physical = (metrics.width_px.max(1), metrics.height_px.max(1));
        let scale_factor = (metrics.scale as f32).clamp(0.5, 8.0);
        let logical_width = (physical.0 as f32) / scale_factor;
        let logical_height = (physical.1 as f32) / scale_factor;

        let (width, height, scale) = backing_store_size(
            logical_width,
            logical_height,
            scale_factor,
            renderer.ctx.device.limits().max_texture_dimension_2d,
        );
        // The insets ride along on every resize, whether or not the size
        // moved: rotating a phone changes them without changing the area.
        self.top_inset_pt = metrics.top_inset_pt.max(0.0);
        self.bottom_inset_pt = metrics.bottom_inset_pt.max(0.0);
        // The pixel ratio too, BEFORE the early return below. An embedding
        // host sizes the camera before the renderer exists and the renderer
        // is created at the final size, so this pass can find nothing to
        // reconfigure -- and returning first left `dpr` at its 1.0 start:
        // every label laid out in points and drawn into a 3x target, a third
        // the size in the top-left third (iPhone 15 Pro Max, rev 74f97b73).
        self.dpr = scale;
        if width == renderer.surface_config.width
            && height == renderer.surface_config.height
            && logical_width == self.camera_control.camera().viewport_width_px
            && logical_height == self.camera_control.camera().viewport_height_px
        {
            return;
        }
        // 1. the surface, and 2. the depth attachment that has to match it.
        renderer.surface_config.width = width;
        renderer.surface_config.height = height;
        renderer.surface.configure(&renderer.ctx.device, &renderer.surface_config);
        renderer.depth_view = renderer.pipeline.create_depth_view(&renderer.ctx, width, height);
        // 3. the camera. Everything above is pixels; this is the map.
        self.camera_control.set_viewport(logical_width, logical_height);
        // The invariant this whole function exists to keep, stated out loud
        // and checked every time it runs. `width`/`height` are what the
        // surface was just configured to; `physical` is the layer's real
        // pixel size. Equal means one drawable pixel is one screen pixel and
        // the projection's aspect is the screen's. Unequal means Core
        // Animation is SCALING the drawable to fit the layer -- which is
        // silent, is not an error anywhere, and is exactly the 12.3%
        // vertical stretch this replaced.
        if (width, height) != (physical.0, physical.1) {
            crate::mlog!(
                "[map] WRONG: surface {width}x{height} != layer {}x{} -- the drawable is \
                 being scaled to fit",
                physical.0, physical.1
            );
        }
        crate::mlog!(
            "[map] surface {width}x{height} px = layer {}x{} px ({logical_width}x{logical_height} \
             pt at x{scale}, aspect {:.4})",
            physical.0,
            physical.1,
            physical.0 as f64 / physical.1 as f64,
        );
    }

    /// Draws exactly `plan.draws`, in exactly that order. This function
    /// makes no coverage decision of its own -- the upload set and the draw
    /// set are the same vector, which is the invariant whose absence made an
    /// evicted stand-in unrecoverable on native.
    fn draw(&mut self, plan: &FramePlan) {
        // Computed before `renderer` below takes its exclusive borrow of
        // `self.renderer`: both are METHODS (`&self`), and a method call
        // needs the whole of `self` for its receiver, which the borrow
        // checker cannot reconcile with an outstanding field-specific
        // `&mut` on `self.renderer` -- unlike the plain field reads
        // (`self.camera_control`, `self.style`, ...) the rest of this function
        // freely interleaves with `renderer`'s borrow below.
        let raster_plan = self.raster_plan();
        let (raster_cover, raster_reach) = self.raster_cover(&raster_plan);
        // The decoded imagery this frame must not evict: the cover, its
        // floor and z0 -- native's `raster_working_set`, one definition.
        self.raster_decoded.set_cover(&raster_cover);
        // The navigation chrome and the label-collision boxes for THIS
        // frame's camera. Both are empty with no trip, and then no pass,
        // pipeline or buffer below exists.
        let nav_scene = if self.nav.is_active() {
            let frame =
                nav_frame_of(self.now_ms(), self.camera_control.camera(), self.top_inset_pt, self.bottom_inset_pt);
            self.nav.scene(self.camera_control.camera(), &frame)
        } else {
            crate::nav::host::ChromeScene::default()
        };
        let nav_boxes = self.nav.collision_boxes(self.camera_control.camera(), self.dpr);

        let Some(renderer) = self.renderer.as_mut() else { return };
        renderer.pipeline.write_camera_uniform(
            &renderer.ctx,
            &renderer.camera_buffer,
            self.camera_control.camera().view_projection().to_cols_array_2d(),
            self.camera_control.camera().zoom as f32,
            self.dpr,
        );
        // `circle-pitch-scale: viewport`'s centre distance (`vs_circle`), as
        // `MapView::paint` writes it.
        renderer.pipeline.write_circle_frame(&renderer.ctx, &renderer.camera_buffer, self.camera_control.camera().distance() as f32);

        // ---- what the globe sits on: `[FROM WASM]`, `web/src/app.rs`'s own
        // block, itself `map_view.rs`'s -- computed once, here, ABOVE the
        // per-tile loop, because the per-tile model uniform now carries a
        // `GlobeParams` this frame's `globe` decides and every reader below
        // (the clear colour, the backdrop, the stars, the atmosphere, and
        // every tile's own uniform) has to agree with it.
        //
        // `with_globe_enabled` is not called here: this crate has no
        // `set_globe_enabled` switch (see `ios/ROTATION.md`'s twin question
        // for pinch) -- the globe is simply on whenever the camera's zoom
        // says so.
        let globe_frame = crate::render::globe::globe_frame(self.camera_control.camera());
        // THE SUN, ONCE. `Sun::Now` reads the wall clock and the subsolar
        // point moves 0.0042 degrees a second, so the ground (the tile
        // uniforms below) and the air (the atmosphere's own uniform) must
        // not read it twice in the same frame -- see `map_view.rs`'s
        // identical `frame_atmosphere`.
        let frame_atmosphere = crate::render::atmosphere::Atmosphere {
            // Daylight while the terminator is off -- native's one call.
            sun: crate::render::atmosphere::frame_sun_for(&self.space.atmosphere, self.scene_clock.map(|c| c.at(Instant::now()))),
            ..self.space.atmosphere
        };
        let sun_direction =
            frame_atmosphere.sun.direction_sphere(&globe_frame.globe_view.inverse());
        // The night floor, raised to 1.0 by the SPACE fade rather than the
        // projection's -- `map_view.rs` has the algebra that makes this
        // exact rather than approximate.
        let night_floor = crate::render::atmosphere::night_floor(frame_atmosphere.night_brightness_clamped(), globe_frame.space_transition);
        // The symbol pass's half of the same sun, built from the same three
        // values `GlobeUpload::new` is handed below so a glyph and the
        // triangles under it cannot be lit from two instants --
        // `night_brightness_clamped()` rather than `night_floor`:
        // `SymbolLight::new` folds the space fade in itself, differently,
        // because a glyph's floor is raised. `[FROM WASM]`.
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
        let space_transition = globe.space_transition();

        // The PAIR, not a lerped colour -- see `render::space::backdrop_plan`
        // for why the backdrop mesh is baked from the stop pair rather than
        // re-tessellated every frame of a zoom.
        let background_pair = background_pair(&self.style, self.camera_control.camera().zoom.floor() as u8);
        let background_blend_t = self.camera_control.camera().zoom.fract() as f32;
        let background_color =
            background_pair.map(|(from, to)| from.lerp(&to, background_blend_t));

        // ONE PLACE decides whether the stars, the backdrop and the
        // atmosphere draw, and what the frame clears to -- see
        // `render::frame_plan`. `plan` (this function's parameter) is the
        // TILE plan; `space_plan` (this local) is the space plan, and the
        // two are unrelated types that happen to share a common English
        // word, hence the qualified name below.
        // The mark's ink against what is behind it this frame -- native's
        // one call, `brand_mark::resolve_for_frame`.
        let brand_variant = crate::brand_mark::resolve_for_frame(
            crate::brand_mark::BrandMarkVariant::Auto,
            background_color,
            &self.space,
            space_transition,
        );
        let space_plan = crate::render::frame_plan::space_plan(
            background_color,
            &self.space,
            &frame_atmosphere,
            &self.style.sky.at_zoom(self.camera_control.camera().zoom),
            space_transition,
            // Does any pixel look above the limb? A fact of the geometry;
            // the plan decides what follows from it (`render::sky`).
            crate::render::sky::sees_above_horizon(self.camera_control.camera(), &globe_frame),
        );
        // A frame whose sky draws clears to space even above z6 -- see
        // `frame_plan::space_plan`.
        let clear_color = if space_transition > 0.0 || space_plan.draws_sky {
            TilePipeline::clear_color(&space_plan.clear_color)
        } else {
            // The pre-existing line, unchanged and kept as its own branch
            // rather than folded into `space_clear_color`: a flat frame's
            // clear colour is not merely equal to what it was before this
            // task, it is computed by the same expression. Unlike native,
            // the fallback is `self.background` rather than `TRANSPARENT`:
            // `compile_style` already defaults a style with no background
            // layer to white, and that default is this app's.
            background_color.map_or(self.background, |c| TilePipeline::clear_color(&c))
        };
        // THE HORIZON FADE, decided by `cover_plan::frame_fog` from this
        // frame's cover cuts -- the vector one (`last_cover_reach`, set by
        // this frame's cover) and the imagery one (`raster_reach`, from the
        // raster cover above) -- and the colour the flat frame clears to,
        // and written into the camera uniform AFTER `write_camera_uniform`
        // (which leaves it off). `HorizonFog::NONE` below 50 degrees and on
        // every globe frame, so those frames draw byte-for-byte what they
        // drew before.
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
        renderer.pipeline.write_horizon_fog(&renderer.ctx, &renderer.camera_buffer, fog);

        if space_plan.draws_backdrop {
            // The ocean sphere: the background layer re-expressed as a
            // sphere BEHIND the tiles. Without it the clear colour above
            // fills the whole viewport and the planet has no visible edge.
            let transparent = crate::style::Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 };
            let ocean = background_pair.unwrap_or((transparent, transparent));
            let tile = crate::render::space::BACKDROP_TILE;
            let matrix = crate::render::combined_tile_matrix(self.camera_control.camera(), tile, TILE_EXTENT)
                .to_cols_array_2d();
            if renderer.backdrop.as_ref().is_none_or(|held| held.color != ocean) {
                let mesh =
                    crate::render::space::backdrop_mesh(TILE_EXTENT as u32, ocean.0, ocean.1);
                let buffers = renderer.pipeline.create_tile_buffers(&renderer.ctx, &mesh);
                let (model_buffer, model_bind_group) = renderer.pipeline.create_model_bind_group(
                    &renderer.ctx,
                    matrix,
                    background_blend_t,
                    0.0,
                    globe.for_tile(tile),
                );
                renderer.backdrop =
                    Some(Backdrop { color: ocean, buffers, model_buffer, model_bind_group });
            } else if let Some(held) = renderer.backdrop.as_ref() {
                // One `combined_tile_matrix` and one 192-byte memcpy for ONE
                // tile, whether or not the camera moved -- cheaper than
                // tracking whether it was needed. See `map_view.rs`.
                renderer.pipeline.write_model_uniform(
                    &renderer.ctx,
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
            // static vertices for all 9,096 catalogue stars, which a launch
            // that never asks for them never pays. See `map_view.rs:10517`.
            let stars = renderer.star_pipeline.get_or_insert_with(|| {
                crate::render::space::StarPipeline::new(
                    &renderer.ctx,
                    renderer.render_format,
                    renderer.pipeline.depth_format(),
                )
            });
            stars.write_sky(&renderer.ctx, self.camera_control.camera(), &self.space);
        }

        if space_plan.draws_sky {
            // The style's `sky` above the limb -- see `render::sky`.
            let sky = renderer.sky_pipeline.get_or_insert_with(|| {
                crate::render::sky::SkyPipeline::new(&renderer.ctx, renderer.render_format, renderer.pipeline.depth_format())
            });
            sky.write(&renderer.ctx, self.camera_control.camera(), &globe_frame, &space_plan.sky, &frame_atmosphere, space_plan.ground);
        }

        if space_plan.draws_atmosphere {
            // Built on the first globe frame and kept. Three generated
            // vertices and a 112-byte uniform.
            let atmosphere = renderer.atmosphere_pipeline.get_or_insert_with(|| {
                crate::render::atmosphere::AtmospherePipeline::new(
                    &renderer.ctx,
                    renderer.render_format,
                    renderer.pipeline.depth_format(),
                )
            });
            // `frame_atmosphere`, not `self.space.atmosphere`: the sun was
            // resolved once above, and this is that same value.
            // The intensity the PLAN decided: `Atmosphere::intensity` times
            // the style's `atmosphere-blend`.
            let air = crate::render::atmosphere::Atmosphere {
                intensity: space_plan.atmosphere_intensity,
                ..frame_atmosphere
            };
            atmosphere.write(&renderer.ctx, self.camera_control.camera(), &globe_frame, &air);
        }

        // Every tile in the draw list, not only the cover -- a stand-in's
        // model matrix is its OWN tile's, which is the whole of the "no new
        // tile math" claim: `combined_tile_matrix` places and scales a z9
        // tile under a z13 camera with no help.
        //
        // Blended against the tile's OWN bake zoom, not `zoom.fract()`: a
        // mesh still waiting for its re-bake after an integer crossing would
        // otherwise draw at the wrong end of its paint pair (z15 widths at
        // z16.03) and jump when the re-bake lands -- the browser's measured
        // zoom twitch; see `tests/stale_mesh_blend_parity.rs`.
        for tile in &plan.draws {
            let Some(gpu) = self.gpu_tiles.get(tile) else { continue };
            renderer.pipeline.write_model_uniform(
                &renderer.ctx,
                &gpu.model_buffer,
                crate::render::combined_tile_matrix(self.camera_control.camera(), *tile, TILE_EXTENT)
                    .to_cols_array_2d(),
                crate::render::tile_blend_t_for_baked_zoom(self.camera_control.camera().zoom, f64::from(gpu.baked_zoom)),
                crate::render::dash_scale(self.camera_control.camera(), *tile, TILE_EXTENT),
                globe.for_tile(*tile),
            );
        }

        // ---- THE IMAGERY BASEMAP -------------------------------------------
        //
        // Resolved and uploaded HERE, before `draws` below takes its
        // immutable borrows of `self.gpu_tiles` -- `web/src/app.rs`'s own
        // reason for the same placement. Empty and free on every launch
        // with no `GPUI_MAP_IMAGERY` set: `raster_plan.opacity` is `None`,
        // `raster_draw_order` never grows, and no pipeline is ever built.
        //
        // **Simplified from native in the same one respect the browser
        // build is, and for the same documented reason**: no ancestor
        // stand-ins. This crate draws only imagery it has actually
        // decoded; a tile in flight leaves a plain gap rather than a
        // blurred ancestor. Pixel parity is a NATIVE requirement (see
        // `a_flat_frame_is_pixel_identical_through_the_space_aware_path`),
        // not a cross-shell one -- the design's own non-goal list says
        // shells share the DECISION (`raster_plan`), not the pixels.
        let mut raster_draw_order: Vec<TileId> = Vec::new();
        if let Some(opacity) = raster_plan.opacity {
            let mut tiles: Vec<TileId> =
                raster_cover.iter().copied().filter(|t| self.raster_decoded.contains(t)).collect();
            // Deterministic draw order -- native's own comment on why a
            // `HashMap` walk order cannot decide what overlaps at the limb.
            tiles.sort_by_key(|t| (t.z, t.y, t.x));

            if !tiles.is_empty() {
                if self.raster_pipeline.is_none() {
                    self.raster_pipeline = Some(crate::render::raster_pipeline::RasterPipeline::new(
                        &renderer.ctx,
                        renderer.render_format,
                        &renderer.pipeline,
                    ));
                }
                let raster_pipeline = self.raster_pipeline.as_ref().expect("just built if missing");
                let blend_t = self.camera_control.camera().zoom.fract() as f32;
                for tile in tiles {
                    let mesh_key = crate::render::raster_mesh::RasterMeshKey::for_tile(tile);
                    self.raster_squares.entry(mesh_key).or_insert_with(|| {
                        let mesh = crate::render::raster_mesh::build(
                            mesh_key,
                            TILE_EXTENT as u32,
                            crate::render::mesh::COORD_SCALE,
                        );
                        raster_pipeline.create_mesh_buffers(&renderer.ctx, &mesh)
                    });
                    // The SAME per-tile model uniform the vector tile above
                    // gets: same `combined_tile_matrix`, same `GlobeParams`,
                    // same blend factor, so the imagery and the fills over
                    // it cannot disagree about where the tile is.
                    let matrix = crate::render::combined_tile_matrix(self.camera_control.camera(), tile, TILE_EXTENT)
                        .to_cols_array_2d();
                    let dash_scale = crate::render::dash_scale(self.camera_control.camera(), tile, TILE_EXTENT);
                    let globe_params = globe.for_tile(tile);

                    if let Some(held) = self.raster_tiles.get_mut(&tile) {
                        renderer.pipeline.write_model_uniform(
                            &renderer.ctx,
                            &held.model_buffer,
                            matrix,
                            blend_t,
                            dash_scale,
                            globe_params,
                        );
                        if held.paint_opacity != opacity {
                            let (buffer, bind_group) = raster_pipeline.create_paint_bind_group(
                                &renderer.ctx,
                                &held.texture,
                                opacity,
                            );
                            held.paint_bind_group = bind_group;
                            held._paint_buffer = buffer;
                            held.paint_opacity = opacity;
                        }
                    } else {
                        let Some(image) = self.raster_decoded.get(&tile) else { continue };
                        let texture = raster_pipeline.upload_tile(&renderer.ctx, image);
                        let (paint_buffer, paint_bind_group) =
                            raster_pipeline.create_paint_bind_group(&renderer.ctx, &texture, opacity);
                        let (model_buffer, model_bind_group) = renderer.pipeline.create_model_bind_group(
                            &renderer.ctx,
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
                if self.raster_tiles.len() > MAX_RASTER_TILES {
                    let keep: HashSet<TileId> = raster_draw_order.iter().copied().collect();
                    self.raster_tiles.retain(|t, _| keep.contains(t));
                }
            }
        }

        // The route line's mesh follows the layer; `None` drops it when the
        // trip ends. See `render::nav_layer`.
        crate::render::nav_layer::sync_route(
            &mut renderer.nav_route,
            &renderer.ctx,
            &renderer.pipeline,
            self.nav.route_layer(),
            self.camera_control.camera(),
            &globe,
        );
        // Caller overlays: upload what landed, pick each cover tile's draw,
        // write this frame's uniforms -- on the same globe as the tiles.
        self.overlays.prepare(&renderer.ctx, &renderer.pipeline, self.camera_control.camera(), &globe, &plan.cover);
        // Population dots: upload the tiles that landed (or were rebuilt),
        // write this frame's uniforms. No dot mask: this shell's basemap
        // carries no mask lists yet, so its layer is `relocate: false`
        // (`shell_fetch::shell_dot_style(Platform::Ios, ..)`, density or race).
        if !self.dots.is_empty() {
            self.dots_gpu.sync(&mut self.dots, &renderer.ctx, &renderer.pipeline, self.camera_control.camera(), &globe, None);
        }
        // Arcs, extruded overlays, trips and heatmaps: uploads and this
        // frame's uniforms, against the same globe as the tiles.
        if !self.shell_layers.is_empty() || self.shell_layers.draws_overlay() {
            let camera = self.camera_control.camera();
            let light = crate::overlay::host::style_light(&self.style.layers, camera.zoom);
            self.shell_layers.prepare(&crate::overlay::host::ShellFrame {
                ctx: &renderer.ctx,
                tile_pipeline: &renderer.pipeline,
                color_format: renderer.render_format,
                camera,
                globe_frame: &globe_frame,
                globe: &globe,
                light: &light,
                px_scale: self.dpr,
                physical: (renderer.surface_config.width, renderer.surface_config.height),
            });
        }
        // The frame's buildings: decided by `render::frame_plan`, the one
        // definition every shell obeys (`MapView::paint` makes this exact
        // call), and written into the per-layer uniforms now, while
        // `renderer` is still mutable. Translucency on: a layer with
        // opacity < 1 draws a depth prepass then a stencilled colour pass
        // (the stencil is cleared once, when the tile pass opens).
        let extrusion_plan = frame_extrusion_plan(
            &self.style,
            self.camera_control.camera(),
            self.last_building_reach.get(),
            self.buildings_flat,
        );
        if !extrusion_plan.layers.is_empty() {
            let params: Vec<_> = extrusion_plan.layers.iter().map(|l| l.params).collect();
            renderer.extrusion_params.write(&renderer.ctx, renderer.pipeline.extrusion(), &params);
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

        // WHAT IS DRAWN UNDER THE TILES: the imagery, in the order the
        // upload loop above decided -- `web/src/app.rs`'s own comment on
        // why this is assembled through `iter()` rather than a lookup: a
        // lookup would need `&mut self`, unreachable once `draws` above has
        // taken its immutable borrows. `raster_squares.get` can
        // legitimately miss (a tile the cap above evicted mid-loop);
        // `filter_map` drops it, same as native and the browser do.
        let raster_squares = &self.raster_squares;
        let held_raster = &self.raster_tiles;
        let raster_draws_vec: Vec<(
            &crate::render::raster_pipeline::RasterBuffers,
            &wgpu::BindGroup,
            &wgpu::BindGroup,
        )> = raster_draw_order
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
            .map(|pipeline| crate::render::gpu::RasterDraws {
                pipeline,
                draws: &raster_draws_vec,
                // Over the basemap's own draws (all of `draws` so far) when
                // the style says so -- the COG card; else under them all.
                after_tile_draws: if crate::styles::raster_above_basemap(self.schema.styles()[self.style_index].name) { draws.len() } else { 0 },
            });

        // WHAT IS DRAWN BEHIND THE TILES, and nothing at all on a flat map.
        // Both halves were decided above, where `&mut renderer` was still
        // available; this only reads them, so it can sit beside the
        // `draws` list it is submitted with. The three gates are exactly
        // `space_plan`'s three booleans -- deliberately, so that a shell
        // deciding one of these for itself again is the one drift
        // `tests/space_plan_parity.rs` exists to catch.
        let space = SpaceDraw {
            stars: if space_plan.draws_stars { renderer.star_pipeline.as_ref() } else { None },
            sky: if space_plan.draws_sky { renderer.sky_pipeline.as_ref() } else { None },
            backdrop: if space_plan.draws_backdrop {
                renderer.backdrop.as_ref().map(|held| (&held.buffers, &held.model_bind_group))
            } else {
                None
            },
            atmosphere: if space_plan.draws_atmosphere {
                renderer.atmosphere_pipeline.as_ref()
            } else {
                None
            },
            // The space look is native-only for now: the textures are fetched
            // by `space_assets`, which this shell does not link.
            look: None,
        };

        let frame = match renderer.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            other => {
                crate::mlog!("[map] get_current_texture: {other:?}");
                return;
            }
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor {
            format: Some(renderer.render_format),
            ..Default::default()
        });
        let mut encoder = renderer
            .ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("map") });
        // `render_with_space_and_raster`, not `render_with_space`: `render_with_space`
        // delegates here with `raster: None`, so the only difference on a
        // frame with no imagery source is that this call site names the
        // `None` itself -- `raster` is `None` above whenever
        // `raster_draws_vec` is empty, which is every launch that never
        // sets `GPUI_MAP_IMAGERY`.
        // The engine's tile pass takes `TileDraw`s: the basemap's 88-byte
        // tiles, then the caller overlays (compact fills, hairlines,
        // instanced circles -- native's order), then, last, the route over
        // every layer and still under the symbol pass's labels.
        let mut draws: Vec<crate::render::TileDraw<'_>> = draws.iter().map(|&draw| draw.into()).collect();
        // The host's overlays: in the tile pass, where the buildings drawn
        // after them can hide them (the standalone demo), or in their own
        // pass over the buildings, depth cleared -- the engine's
        // `render_overlay_above_buildings`, the desktop's
        // `set_source_above_buildings` pass -- so a route under a tower
        // draws whole (an embedding transit app; `host::Host`).
        // Either way a source is over the buildings by default on a frame
        // that draws any (`render::overlay_order::overlay_pass`); the host
        // flag lifts the ones kept under them too.
        let mut above_buildings: Vec<crate::render::TileDraw<'_>> = Vec::new();
        if self.overlays_above_buildings {
            self.overlays.push_draws(&mut above_buildings);
        } else {
            let lift = !extrusion_plan.steps.is_empty() && !self.shell_layers.draws_tile_pass();
            self.overlays.push_draws_split(&mut draws, &mut above_buildings, lift);
        }
        // Population dots over the overlays, under the route and the
        // labels: native's order (`MapView::paint`).
        self.dots_gpu.push_draws(globe_frame.transition == 0.0, &mut draws);
        if let Some(draw) = renderer.nav_route.as_ref().and_then(|route| route.draw()) {
            draws.push(draw.into());
        }
        // The buildings, after EVERY other push -- basemap, overlays, dots,
        // route -- so their walls cover what lies behind them while the
        // label pass still draws over them: native's order.
        if !extrusion_plan.steps.is_empty() {
            crate::render::extrusion_gpu::push_extrusion_draws(
                &extrusion_plan,
                &extrusion_tiles,
                &renderer.extrusion_params,
                &mut draws,
            );
        }
        // The overlay extrusions after the buildings: the same pipelines
        // and depth buffer, so a column and a building occlude each other
        // (`MapView::paint`'s order).
        self.shell_layers.push_tile_draws(&mut draws);
        renderer.pipeline.render_with_space_and_raster(
            &mut encoder,
            &view,
            &renderer.depth_view,
            clear_color,
            &renderer.camera_bind_group,
            &space,
            raster,
            &draws,
            // `timestamp_writes`: `None`, deliberately, and NOT because the
            // hardware refuses.
            //
            // It is worth being precise, because the engine is not. The doc
            // on `PassTimer` says "iOS and wasm both have adapters that do
            // not offer it", and on this phone that is false: the
            // `[map] gpu:` line above prints `TIMESTAMP_QUERY: yes` on the
            // A17 Pro, and it prints the DEVICE's features, not the
            // adapter's -- so `GpuContext::new`'s `adapter.features() &
            // Features::TIMESTAMP_QUERY` really did carry the feature
            // through. iOS Metal supports counter sampling at stage
            // boundaries; the engine's assumption is an untested one, and
            // this is the line that tests it.
            //
            // `None` is still right here, for the mundane reason: nothing in
            // this spike CONSUMES a GPU pass duration. `PassTimer` is not in
            // `render/mod.rs`' re-export list, there is no `gpu-stats`
            // reporting path, and a `Some(...)` would need a query set, a
            // resolve buffer, a mappable readback and the two-frame
            // resolve-on-the-NEXT-encoder dance `PassTimer` documents -- all
            // to feed a number nothing prints. If GPU timing is ever wanted
            // on device, the feature is there for it, and this is the one
            // argument that changes.
            None,
        );
        renderer.pipeline.render_overlay_above_buildings(
            &mut encoder,
            &view,
            &renderer.depth_view,
            &renderer.camera_bind_group,
            &above_buildings,
        );
        let drawn = draws.len() + above_buildings.len();
        drop(above_buildings);
        drop(draws);
        drop(raster_draws_vec);
        // Heatmaps, arcs and trips: their off-screen sums, then one pass
        // over the tiles and under the labels (the desktop's symbol pass,
        // before its icons and text).
        let physical = (renderer.surface_config.width, renderer.surface_config.height);
        self.shell_layers.encode_offscreen(&renderer.ctx, &mut encoder, physical);
        self.shell_layers.render_overlay_pass(&mut encoder, &view);

        // Altitude layers (satellites): only on a globe frame
        // (`AltitudeFrame::new` is `None` at transition 0), from THIS frame's
        // `globe_frame`, advanced to this frame's scene time -- native's
        // `MapView::paint` block. Over the tiles and the shell layers, under
        // the labels and the chrome (the symbol pass below).
        self.altitude_points_drawn = 0;
        if !self.altitude.is_empty() || self.altitude.has_keyframed() {
            let started = Instant::now();
            let scene = scene_time_of(self.scene_clock);
            let rate = scene_rate_of(self.scene_clock);
            if self.altitude.has_keyframed() {
                self.altitude.advance(scene, rate);
            }
            let has_altitude = !self.altitude.is_empty()
                && match crate::render::altitude_pipeline::AltitudeFrame::new(
                    self.camera_control.camera(),
                    &globe_frame,
                    glam::DMat3::IDENTITY,
                    0.0,
                    60.0,
                    1.0,
                ) {
                    Some(mut frame) => {
                        frame.px_scale = self.dpr;
                        frame.viewport = (physical.0 as f32, physical.1 as f32);
                        let pipeline = renderer.altitude_pipeline.get_or_insert_with(|| {
                            crate::render::altitude_pipeline::AltitudePipeline::new(&renderer.ctx, renderer.render_format, None)
                        });
                        self.altitude.prepare(pipeline, &renderer.ctx, &frame, scene)
                    }
                    None => {
                        self.altitude.not_drawn();
                        false
                    }
                };
            if has_altitude {
                if let Some(pipeline) = renderer.altitude_pipeline.as_ref() {
                    self.altitude_points_drawn = pipeline.points_drawn();
                    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("altitude-pass"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &view,
                            depth_slice: None,
                            resolve_target: None,
                            ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                        })],
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                        multiview_mask: None,
                    });
                    self.altitude.draw(pipeline, &mut pass);
                }
            }
            // The next frame the dots owe: at once while one is due within
            // a frame (a hold, or a dot that moved a visible amount), else a
            // wake when it is (native's `altitude_redraw_wait`).
            let dots_wait = if globe_frame.transition > 0.0 { altitude_redraw_after_s(&self.altitude, rate) } else { None };
            self.altitude_wake_at = dots_wait.map(|w| Instant::now() + Duration::from_secs_f64(w.clamp(0.0, 3_600.0)));
            self.altitude_cpu_ms = started.elapsed().as_secs_f64() * 1e3;
        } else {
            self.altitude_wake_at = None;
        }

        // Symbol overlay: point labels and line-placed glyph quads, into a
        // second pass on this same encoder. A method on `Renderer` (see its
        // own doc comment for why), so `self.gpu_tiles`/`self.camera_control`/
        // `self.style`/`self.dpr` are passed as explicit shared borrows --
        // `renderer` is already an exclusive borrow of `self.renderer`, and
        // those four are OTHER fields of `self`, not a conflict. `symbol_light`
        // is the one computed above, alongside `globe`/`night_floor`, from
        // the SAME sun -- not a second one.
        let credit = if self.credits_by_host { None } else { crate::attribution::join(&ios_credits(self.schema, self.style_source, active_archive(self.style_source, self.schema, &self.basemap_archive), &self.raster, &self.dots, &self.extra_credits, self.data_credit.as_deref())) };
        // Labels over data: the on-data paint for the labels over one of
        // the host's polygons (memoised per label), the style's elsewhere.
        let area_cover = if self.label_paint.is_some() { self.overlays.area_cover() } else { Default::default() };
        let label_cover = &self.label_cover;
        let over_data = |tile: TileId, anchor: (i32, i32)| label_cover.covers(&area_cover, tile, anchor);
        let labels_on_data = self.label_paint.map(|paint| crate::symbol_pass::LabelsOnData {
            paint,
            over_data: (!area_cover.is_empty()).then_some(&over_data as &dyn Fn(TileId, (i32, i32)) -> bool),
        });
        renderer.draw_labels(
            credit.as_deref(),
            &mut encoder,
            &view,
            &self.gpu_tiles,
            plan,
            self.camera_control.camera(),
            &self.style,
            self.dpr,
            self.top_inset_pt,
            &globe_frame,
            symbol_light,
            fog,
            &nav_boxes,
            Some(brand_variant),
            self.bottom_inset_pt,
            &self.overlay_label_plans,
            self.chrome_rtl,
            labels_on_data,
        );
        // The navigation chrome, in its own pass over everything. No pass,
        // no pipelines and no buffers exist while there is nothing to show.
        if !nav_scene.is_empty() {
            let chrome = renderer
                .nav_chrome
                .get_or_insert_with(|| crate::render::nav_layer::NavChromeGpu::new(&renderer.ctx, renderer.render_format));
            chrome.render(
                &renderer.ctx,
                &mut encoder,
                &view,
                &nav_scene,
                Some(&mut renderer.labels.font_system),
                (renderer.surface_config.width, renderer.surface_config.height),
                self.dpr,
            );
        }

        // A test's readback: the drawn frame, copied before the present --
        // the engine's `render::frame_grab`, as `MapView::grab_next_frame`.
        let grab = if std::mem::take(&mut self.capture_requested) {
            crate::render::frame_grab::encode(&renderer.ctx, &mut encoder, &frame.texture)
        } else {
            None
        };
        renderer.ctx.queue.submit(std::iter::once(encoder.finish()));
        renderer.ctx.queue.present(frame);
        if let Some(Ok(f)) = grab.map(crate::render::frame_grab::FrameGrab::read) {
            self.captured = Some((f.width, f.height, f.rgba));
        }

        if !self.first_draw_logged && drawn > 0 {
            self.first_draw_logged = true;
            crate::mlog!(
                "[map] launch -> first triangle: {:.0} ms ({})",
                self.now_ms(),
                match self.first_body_ms {
                    Some(ms) => format!("first tile body at {ms:.0} ms"),
                    // The first draw was the host's overlays (they draw
                    // before any tile lands) or tiles already held.
                    None => "before any tile body: overlays or held tiles".to_string(),
                },
            );
        }
    }

    /// One frame: collect, resize, glide, request, plan, draw -- and THEN
    /// bake.
    ///
    /// The bake used to run before the plan, which is the natural reading
    /// order and the wrong execution order. A tessellation is up to ~15 ms
    /// (see [`BAKE_BUDGET_MS`]) and the present is what the user's finger is
    /// waiting for, so putting the bake first delays every frame during a
    /// tile load by the cost of work that could not possibly appear in it:
    /// `plan_frame` has already chosen what to draw by the time the mesh
    /// lands, so a tile baked at the top of frame N is drawn no sooner than
    /// N+1 either way. Baking first buys nothing and costs a present.
    ///
    /// After the present, the same work is free twice over. The GPU is busy
    /// with the command buffer just submitted, and the loop's next act is to
    /// block in `get_current_texture` until a drawable frees -- so the bake
    /// runs in slack that would otherwise be spent waiting.
    ///
    /// This is Organic Maps' frame order, and theirs is the same argument:
    /// `FrontendRenderer::RenderFrame` is `ProcessEvents` -> `PrepareScene`
    /// -> `RenderScene` -> `UpdateScene`, with `UpdateScene` -- the call that
    /// tells the tile pipeline the viewport moved -- deliberately last. The
    /// frame you see always reflects the touch that arrived microseconds ago,
    /// drawn with whatever geometry is already resident, and the data
    /// pipeline hears about it afterwards.
    fn frame(&mut self) {
        if self.renderer.is_none() {
            return;
        }
        // Measured at the TOP, before any work: `about_to_wait` is entered
        // once per pass of the run loop and the previous pass ended inside
        // `get_current_texture`, so this difference is the present-to-
        // present period the display actually granted -- not the CPU time
        // one frame's work took.
        let frame_started = self.now_ms();
        if let Some(previous) = self.last_frame_ms.replace(frame_started) {
            self.frame_intervals.push(frame_started - previous);
        }
        if self.resize_pending {
            self.resize_pending = false;
            self.apply_resize();
        }
        self.collect_fetched();
        // The glide BEFORE the tile request below reads `cover()`: a fling
        // that moved the camera after the cover was computed would spend the
        // whole glide asking for the tiles it wanted one frame ago.
        let now = self.now_ms();
        let moving = self.advance_motion(now);
        // Checked here, after the glide has had its chance to finish this
        // frame and before anything below reads `touches`/`pan_inertia`
        // again: this is the one place a frame ALWAYS runs while either is
        // true, so it is the one place that is guaranteed to notice the
        // instant both go false and re-arm the idle timer / pause the
        // display link promptly rather than a frame late.
        self.sync_activity_state();
        self.request_missing_tiles();
        self.park_overdue_retries();
        let plan = self.plan_frame();
        self.retained = plan.retained.clone();
        self.frames += 1;
        if std::mem::take(&mut self.overlay_demo) {
            let camera = self.camera_control.camera();
            let (lon, lat) = (camera.center_lon, camera.center_lat);
            let zoom = self.display_zoom();
            let result = self.overlays.load_demo(lon, lat, 48, 32, 3_000, zoom);
            crate::mlog!("[overlay] demo: sample polygons + 3000 clustered points at ({}): {result:?}", crate::log::position(lat, lon));
        }
        // Caller overlays: index builds, point views and tile bakes, inline
        // under their own share of the frame. Before `draw`, which uploads
        // what landed.
        let overlay_work = self.overlays.update(self.camera_control.camera(), &plan.cover, OVERLAY_BUDGET_PER_FRAME);
        self.drive_dots();
        self.draw(&plan);
        // Charged at the far side of `draw`, whose last act is
        // `queue.present`. On Metal that is `presentDrawable` + `commit`,
        // so this measures touch-to-SUBMIT. Photon is later by the GPU's
        // own pass time plus up to one refresh period of scanout; the
        // `[perf]` line says so rather than pretending otherwise.
        if let Some(arrived_at) = self.input_pending_ms.take() {
            self.input_latency.push(self.now_ms() - arrived_at);
        }
        // After the present and after the latency is charged, so that neither
        // the picture nor the measurement of it waits on the tessellator.
        // See this function's own doc.
        // Decode after the present too, for the same reason as the bake:
        // `plan_frame` draws meshes, never decoded tiles, so a tile decoded
        // before the draw appears no sooner than one decoded after it.
        let decoding = self.decode_arrived();
        let baked = self.bake_some();
        // Every frame, not only a frame that baked: stale arrivals used to
        // pile into `decoded` on frames with nothing to bake, so the cap
        // never ran while the backlog drained (1,323 decoded on device).
        let cover = self.cover();
        self.evict_discard_pile(cover);

        // The redraw predicate's other half -- see `needs_redraw`'s own
        // doc. `sync_activity_state` above already asked "is a gesture or a
        // fling live"; this asks the complementary question `paint()` asks
        // on native (`still_animating || still_gliding || still_orienting`,
        // this shell's own version of it): is there work QUEUED that only
        // another frame can drain. `pan_inertia` still being `Some` covers
        // the fling itself; `baked` covers a bake backlog `BAKES_PER_FRAME`
        // did not finish this pass -- if `bake_some` had anything to do at
        // all, ask for one more frame and let IT find out whether the
        // backlog is actually empty now. A tile that has not been
        // requested yet, or has been requested but not yet answered, is
        // NOT here: that is what `net::Fetcher`'s wake is for, not a poll.
        // `moving` is `advance_motion`'s answer: the fling, or
        // `CameraController::advance`, which reaches `false` in finite time
        // for every motion (`every_motion_settles_in_finite_time`).
        //
        // `label_fade_wants_redraw` is `draw_labels`' own answer, set inside
        // `self.draw(&plan)` above and already capped by
        // `FadeRedrawScheduler` -- so a still-fading label asks for at most
        // ~60 fps of extra frames here, not the display's own max.
        let fade_wants_redraw = self.renderer.as_ref().is_some_and(|r| r.label_fade_wants_redraw);
        // Moving satellites due within a frame ask for the next one now,
        // not through a wake (which would lose the display link's vsync).
        let altitude_due = self.altitude_wake_at.is_some_and(|at| at <= Instant::now() + Duration::from_millis(16));
        if altitude_due {
            self.altitude_wake_at = None;
        }
        if moving || baked || decoding || fade_wants_redraw || overlay_work || altitude_due {
            self.request_redraw();
        }

        if now - self.last_status_ms > 1000.0 {
            self.last_status_ms = now;
            let (at120, at90, at60, slow, stalled) = self.frame_intervals.refresh_buckets();
            if let Some((n, mean, p50, p95, max)) = self.frame_intervals.summary() {
                crate::mlog!(
                    "[perf] {:.1} fps | present interval ms: mean {mean:.2} p50 {p50:.2} \
                     p95 {p95:.2} max {max:.2} over {n} frames | 120Hz {at120} 90Hz {at90} \
                     60Hz {at60} slower {slow} stalled {stalled}",
                    1000.0 / mean,
                );
            }
            self.frame_intervals.clear();
            // Absent from the log means nobody touched the screen this
            // second, which is a different statement from "zero latency"
            // and has to look different.
            if let Some((n, mean, p50, p95, max)) = self.input_latency.summary() {
                crate::mlog!(
                    "[perf] touch -> present submit ms: mean {mean:.2} p50 {p50:.2} \
                     p95 {p95:.2} max {max:.2} over {n} batches (photon adds GPU time \
                     + up to one refresh period)"
                );
            }
            self.input_latency.clear();
            self.log_dots();
            if self.altitude.has_keyframed() {
                crate::mlog!(
                    "[sats] {} dots drawn | altitude advance+prepare {:.2} ms | scene rate {}",
                    self.altitude_points_drawn,
                    self.altitude_cpu_ms,
                    self.scene_rate(),
                );
            }
            let (queue_pending, queue_running, queue_dropped) = self.fetcher.queue_stats();
            crate::mlog!(
                "[map] z {:.2} (tiles z{}) @ {} brg {:.1} pit {:.1} | cover {}: {} fresh, {} stale, \
                 {} desc, {} anc, {} HOLE | {} gpu, {} decoded, {} inflight, {} empty, {} retrying \
                 ({} failed) | queue {} pending {} running {} arrived, stale {} dropped {} discarded \
                 | {} icons | {} frames",
                self.camera_control.camera().zoom,
                crate::render::cover_plan::zoom_span_label(plan.cover_zooms, self.tile_zoom()),
                crate::log::position(self.camera_control.camera().center_lat, self.camera_control.camera().center_lon),
                // Degrees, signed: the controller keeps bearing in [-180, 180).
                self.camera_control.camera().bearing.to_degrees(),
                self.camera_control.camera().pitch.to_degrees(),
                plan.cover_len,
                plan.fresh,
                plan.stale,
                plan.descendant_stand_ins,
                plan.ancestor_stand_ins,
                plan.holes,
                self.gpu_tiles.len(),
                self.decoded.len(),
                self.inflight.len(),
                self.missing.len(),
                // Tiles backing off after a transient failure, vector and
                // imagery together, and every such failure this session. A
                // device log with a growing `failed` and a non-zero
                // `retrying` is the weak-network case, visibly handled.
                self.retries.retrying() + self.raster_retries.retrying(),
                self.retries.failures_total() + self.raster_retries.failures_total(),
                // The fetch queue: jobs waiting for a worker, jobs on one,
                // bodies waiting for a decode slot; then pending jobs dropped
                // because the camera left them, and bodies discarded
                // undecoded for the same reason. A `pending` that stays in the
                // hundreds while the camera is still is the old FIFO backlog.
                queue_pending,
                queue_running,
                self.arrived.len(),
                queue_dropped,
                self.stale_results_dropped,
                // The last frame's icon quad count -- zero on every frame
                // before this task (the `debug_assert!` it removed), and
                // the number this task's own device-log verification reads
                // to say whether shields are actually drawing.
                self.renderer.as_ref().map_or(0, |r| r.icon_instances_drawn),
                self.frames,
            );
        }
    }

    /// This frame's dot work, before `draw`: lands what the workers
    /// finished, plans the layers for this camera (`DotLayers::frame`),
    /// arms the retry wake, and hands out every job the layers have now
    /// (`DotLayers` caps a source's requests in flight). Nothing with no
    /// dot source.
    fn drive_dots(&mut self) {
        if self.dots.is_empty() {
            return;
        }
        if let Some(worker) = &self.dot_worker {
            for landed in worker.drain() {
                self.dots.land(landed);
            }
        }
        let now = crate::timing::Instant::now();
        let camera = *self.camera_control.camera();
        let cover = crate::render::globe::GlobeCover::new(&crate::render::globe::globe_frame(&camera));
        let work = self.dots.frame(&camera, &cover, now);
        // A failed key is asked for again after a backoff, but nothing
        // redraws a phone sitting still: one wake when it ends.
        if self.dot_retry_at.is_none() {
            if let Some(wait) = self.dots.next_retry(now) {
                self.dot_retry_at = Some(Instant::now() + wait + Duration::from_millis(50));
            }
        }
        if work {
            if let Some(worker) = &self.dot_worker {
                while let Some(job) = self.dots.next_job() {
                    worker.submit(job);
                }
            }
        }
    }

    /// The once-a-second `[dots]` line MG-25 reads: dots drawn against the
    /// budget, the view's people per dot and level, tiles, errors.
    fn log_dots(&mut self) {
        let Some(s) = self.dots.stats(crate::dots::DOT_LAYER) else { return };
        let budget = self.dots.style(crate::dots::DOT_LAYER).map_or(0, |st| st.budget_dots);
        if !self.dots_first_logged && s.dots_drawn > 0 {
            self.dots_first_logged = true;
            if let Some(set_at) = self.dots_set_at {
                crate::mlog!("[dots] first dots within {:.0} ms of the source being set", set_at.elapsed().as_secs_f64() * 1000.0);
            }
        }
        crate::mlog!(
            "[dots] {} drawn / {budget} budget | ppd {:.1} level {} | tiles {} ready {} wanted {} stand-in {} drawn | \
             {} held | gpu {:.1} MB | {} source errors{}",
            s.dots_drawn,
            s.ppd,
            s.level,
            s.tiles_ready,
            s.tiles_wanted,
            s.standin_tiles,
            s.tiles_drawn,
            self.dots.tiles_held(crate::dots::DOT_LAYER),
            s.gpu_bytes as f64 / (1024.0 * 1024.0),
            s.source_errors,
            s.last_error.map(|e| format!(" (last: {e})")).unwrap_or_default(),
        );
    }

    /// This frame's share of every motion: the camera's eases (zoom,
    /// orientation, rotation inertia) and then the fling. Returns whether
    /// either is still moving, which is `frame()`'s whole redraw predicate
    /// apart from the bake backlog -- split out so the heat fix's contract
    /// can be tested without a GPU (`a_settled_camera_stops_asking_for_frames`).
    fn advance_motion(&mut self, now_ms: f64) -> bool {
        // Navigation before the camera's eases, as on native and the web:
        // its follow camera `jump_to`s this frame's pose. An idle host is
        // never called, and a paused or arrived trip asks for no frame.
        let nav_moving = if self.nav.is_active() || self.nav_autostart {
            let frame = nav_frame_of(now_ms, self.camera_control.camera(), self.top_inset_pt, self.bottom_inset_pt);
            if std::mem::take(&mut self.nav_autostart) {
                match self.nav.start_simulation(&self.camera_control, &frame) {
                    Ok(()) => crate::mlog!("[nav] drive started"),
                    Err(err) => crate::mlog!("[nav] {err}"),
                }
            }
            self.nav.advance(&mut self.camera_control, &frame).redraw
        } else {
            false
        };
        // Voice guidance: what to say is `nav`'s; this only says it, through
        // the speaker shared with native (`crate::speech`), which activates
        // the ducking audio session for a prompt and releases it once quiet.
        let voice = self.nav.take_voice();
        if matches!(voice, Some(crate::nav::host::VoiceCommand::Speak(_))) {
            self.nav_prompts += 1;
        }
        if self.speaker_on {
            self.speech.frame(voice);
        }
        // `advance` clamps a long gap itself (`camera_control::MAX_ADVANCE_STEP`).
        let dt_ms = self.last_advance_ms.replace(now_ms).map_or(0.0, |last| (now_ms - last).max(0.0));
        let camera_moving = self.camera_control.advance(std::time::Duration::from_secs_f64(dt_ms / 1000.0));
        self.advance_pan_inertia(now_ms);
        camera_moving || self.pan_inertia.is_some() || nav_moving
    }

    /// Marks a frame as owed and, if the surface already exists, wakes the
    /// (`ControlFlow::Wait`) run loop to go draw it. The single call site
    /// every redraw trigger in this file goes through -- a touch, a resize,
    /// the network wake, and `frame()`'s own tail when a fling or a bake
    /// backlog means the picture is not done changing yet.
    ///
    /// A no-op before the first `resumed()` (no window to ask winit to
    /// redraw) beyond setting the flag, which is harmless: `resumed()` sets
    /// it again itself once the renderer exists.
    fn request_redraw(&mut self) {
        self.needs_redraw = true;
        if self.renderer.is_some() {
            if let Some(hook) = self.redraw_hook.as_ref() {
                hook();
            }
        }
    }

    /// The one condition both idle-time knobs this shell owns are scoped to:
    /// a finger is on the glass, or a fling this app started is still
    /// gliding. Neither the display link nor the idle timer earns its keep
    /// for a map that is fully at rest, however many tiles just happened to
    /// land -- a bake backlog draining, or a tile decode, is not "the user
    /// is moving something", it is "the picture is catching up", and it
    /// wants a frame, not 120 Hz and a phone that will not sleep.
    ///
    /// A no-op, both in the objc sense and in the syscall sense, on every
    /// call where the answer has not changed since the last one -- the
    /// common case, since this runs every frame a gesture or a fling is
    /// live and would otherwise resend the same message every single frame
    /// of a multi-second pan.
    fn sync_activity_state(&mut self) {
        let active = !self.touches.is_empty() || self.pan_inertia.is_some() || !self.camera_control.is_settled();
        if active == self.idle_timer_disabled {
            return;
        }
        let Some(mtm) = objc2::MainThreadMarker::new() else { return };
        // An embedding host owns the idle timer: only the bookkeeping moves.
        if !self.manage_idle_timer || crate::display_link::set_idle_timer_disabled(mtm, active) {
            self.idle_timer_disabled = active;
        }
        if let Some(renderer) = self.renderer.as_ref() {
            if let Some(hint) = renderer.frame_rate_hint.as_ref() {
                hint.set_paused(!active);
            }
        }
    }

    // -- gestures -------------------------------------------------------
    //
    // This is the part `map_view.rs` could contribute nothing to, exactly as
    // the browser found: its handlers are gpui types end to end
    // (`MouseDownEvent`, `ScrollWheelEvent`, `ScrollDelta::Lines`). What
    // they call INTO is portable and reused above; the plumbing is not, and
    // phase 2 should expect to write this layer per host.

    fn touch_index(&self, id: u64) -> Option<usize> {
        self.touches.iter().position(|t| t.id == id)
    }

    /// The first two fingers, in points.
    fn pinch_pair(&self) -> Option<[(f32, f32); 2]> {
        let (a, b) = (self.touches.first()?, self.touches.get(1)?);
        Some([(a.x as f32, a.y as f32), (b.x as f32, b.y as f32)])
    }

    /// Begins (or re-seeds) the two-finger gesture from the fingers that
    /// are down now.
    ///
    /// `fresh` distinguishes the two callers. A gesture STARTING (`true`)
    /// begins a new `TwoFingerGesture`, which zeroes the rotation deadzone
    /// and decides afresh between pinch-and-rotate and tilt. A gesture whose
    /// finger COUNT changed above two (`false`) -- a third finger landing,
    /// or one of three lifting -- only re-seeds the reference pair, so the
    /// next update's delta is measured against the new pair instead of
    /// jumping by the difference between two different pairs of fingers.
    /// Its rotation stays engaged: from the hand's point of view that is
    /// still one continuous twist, and making the user re-earn the deadzone
    /// mid-gesture would read as the map sticking.
    fn begin_pinch(&mut self, fresh: bool) {
        self.pinch_points = self.pinch_pair();
        if fresh {
            self.pinch = self.pinch_points.map(|[a, b]| TwoFingerGesture::begin(a, b));
        }
    }

    /// Ends the two-finger gesture. The rotation state dies with it, which
    /// is what makes the deadzone per-gesture.
    fn end_pinch(&mut self) {
        self.pinch = None;
        self.pinch_points = None;
    }

    /// Starts (or restarts) the one-finger drag from wherever the remaining
    /// finger is. Called on the first touch down and again whenever a pinch
    /// drops back to one finger -- without the restart, the surviving finger
    /// would pan by the whole distance from wherever the drag last sampled.
    ///
    /// `from_pinch` is `true` only for the second case, and it is what lets
    /// `post_pinch_drag_is_trustworthy` tell a survivor of a dropped pinch
    /// apart from a fresh one-finger touch-down at release time -- see that
    /// function for why the distinction matters.
    fn begin_drag(&mut self, now_ms: f64, from_pinch: bool) {
        self.drag_from = self.touches.first().map(|t| (t.x, t.y));
        self.drag_samples.clear();
        if let Some((x, y)) = self.drag_from {
            self.drag_samples.push_back(DragSample { at_ms: now_ms, x: x as f32, y: y as f32 });
        }
        self.drag_segment_started_ms = now_ms;
        self.drag_segment_from_pinch = from_pinch;
    }

    fn on_touch(&mut self, phase: TouchPhase, id: u64, x: f64, y: f64) {
        let now = self.now_ms();
        // The instant a finger's intent entered the process. See
        // `input_pending_ms` for why this is `get_or_insert` and not `=`.
        self.input_pending_ms.get_or_insert(now);
        // Everything but `Moved`, which is one line per frame of a drag.
        // Kept rather than deleted: a gesture that does nothing is
        // indistinguishable from a gesture that never arrived, and on a
        // simulator driven by synthesised host events that distinction is
        // the whole debugging loop.
        if phase != TouchPhase::Moved {
            crate::mlog!("[touch] {phase:?} id={id} at {} ({} down)", crate::log::point(x, y), self.touches.len());
        }
        match phase {
            TouchPhase::Started => {
                self.touches.push(ActiveTouch {
                    id,
                    x,
                    y,
                    start_x: x,
                    start_y: y,
                    start_ms: now,
                    moved: false,
                });
                // Putting a finger down stops a glide dead, the way catching
                // a spinning wheel does.
                self.pan_inertia = None;
                if self.touches.len() == 1 {
                    self.begin_drag(now, false);
                    self.end_pinch();
                    // A new gesture: catches any rotation inertia still
                    // spinning, and forgets whether the last one rotated.
                    self.camera_control.apply(CameraIntent::GestureBegan);
                    self.rotate_samples.clear();
                } else if self.touches.len() >= 3 {
                    // Three fingers is the style switcher, and it fires on
                    // the third finger's DOWN rather than on a lift.
                    //
                    // Down, because a tap needs a timeout to be recognised
                    // and this gesture has to be unambiguous the instant it
                    // is made: the two fingers already on the glass are
                    // mid-pinch, and holding the decision open for
                    // `TAP_MAX_MS` means 300 ms of the map zooming and
                    // spinning before it turns out the user wanted neither.
                    // Firing immediately and then consuming the rest of the
                    // gesture (`multi_touch_consumed`) is the only ordering
                    // in which the map does not move at all.
                    //
                    // Three, because one and two are taken -- pan and
                    // pinch/rotate -- and because nothing in `ios/ROTATION.md`
                    // or `ios/PINCH-FLING-FIX.md` reads a third contact: the
                    // pinch is built from `touches[0]` and `touches[1]` only
                    // (see `pinch_pair`), so a third finger was previously
                    // inert rather than meaningful, and giving it a meaning
                    // takes nothing away.
                    if !self.multi_touch_consumed {
                        self.multi_touch_consumed = true;
                        self.end_pinch();
                        self.drag_from = None;
                        self.drag_samples.clear();
                        self.cycle_style();
                    }
                } else {
                    self.drag_from = None;
                    self.drag_samples.clear();
                    // Two fingers down is a gesture starting; a third
                    // landing on an existing one is not.
                    self.begin_pinch(self.touches.len() == 2);
                }
            }
            TouchPhase::Moved => {
                let Some(i) = self.touch_index(id) else { return };
                self.touches[i].x = x;
                self.touches[i].y = y;
                if (x - self.touches[i].start_x).hypot(y - self.touches[i].start_y) > TAP_SLOP_PT {
                    self.touches[i].moved = true;
                }
                // The position and the slop above are still recorded -- only
                // the camera is spared. A gesture that already spent itself
                // on a style switch must not also pan, pinch or rotate, and
                // that stays true for the whole time the hand is down, not
                // just until it drops back to two fingers.
                if self.multi_touch_consumed {
                    return;
                }
                if self.touches.len() >= 2 {
                    self.update_pinch(now);
                } else if let Some((px, py)) = self.drag_from {
                    self.pan_by_pixels((x - px) as f32, (y - py) as f32, (px as f32, py as f32));
                    self.drag_from = Some((x, y));
                    self.drag_samples.push_back(DragSample {
                        at_ms: now,
                        x: x as f32,
                        y: y as f32,
                    });
                    drain_drag_samples(&mut self.drag_samples, now);
                }
            }
            TouchPhase::Ended | TouchPhase::Cancelled => {
                let Some(i) = self.touch_index(id) else { return };
                let touch = self.touches.remove(i);
                let was_last = self.touches.is_empty();
                if self.multi_touch_consumed {
                    // A hand comes off the glass one finger at a time, and
                    // the last of them is a short, still contact that reads
                    // exactly like a tap -- so the switch would be followed
                    // by a phantom double-tap zoom. `last_tap` is cleared
                    // rather than merely bypassed for the same reason: a tap
                    // recorded BEFORE the three fingers landed must not pair
                    // with the next real one across a style switch.
                    if was_last {
                        self.multi_touch_consumed = false;
                        self.drag_from = None;
                        self.drag_samples.clear();
                        self.last_tap = None;
                        // No throw: this gesture spent itself on a style
                        // switch.
                        self.rotate_samples.clear();
                        self.camera_control.apply(CameraIntent::GestureEnded { rotate_velocity: 0.0 });
                        self.end_pinch();
                    }
                    return;
                }
                if was_last {
                    let tapped = phase == TouchPhase::Ended
                        && !touch.moved
                        && now - touch.start_ms <= TAP_MAX_MS;
                    if tapped {
                        self.on_tap(touch.x, touch.y, now);
                        self.drag_samples.clear();
                    } else {
                        // Let go mid-motion and the map keeps gliding.
                        // Drained FIRST, so a drag that stalled before
                        // release ages its own history out and correctly
                        // flings nothing.
                        drain_drag_samples(&mut self.drag_samples, now);
                        // A drag segment that just took over from a dropped
                        // pinch does not get to fling until it has been
                        // running a moment on its own -- see
                        // `post_pinch_drag_is_trustworthy`. A drag that
                        // started at a fresh touch-down is never gated here.
                        let trustworthy = !self.drag_segment_from_pinch
                            || post_pinch_drag_is_trustworthy(self.drag_segment_started_ms, now);
                        let samples: Vec<DragSample> = self.drag_samples.iter().copied().collect();
                        // The camera's zoom, because the speed ceiling
                        // depends on it -- see `kinetic_max_speed`. Read at
                        // RELEASE and then fixed: a glide that outlives a
                        // later zoom keeps the ceiling it was thrown under,
                        // which is what makes the throw and its result feel
                        // like one motion.
                        self.pan_inertia = if trustworthy {
                            pan_inertia_from_samples(&samples, now, self.camera_control.camera().zoom)
                        } else {
                            None
                        };
                        // Logged because a fling is the one gesture whose
                        // result is entirely off-screen arithmetic: the
                        // distance and the ceiling that clamped it cannot be
                        // read off a moving map, and "not so flying" was the
                        // report this curve exists to answer.
                        if let Some(f) = self.pan_inertia {
                            crate::mlog!(
                                "[touch] FLING {:.0} pt over {:.0} ms (ceiling {:.0} pt/s at \
                                 z{:.2})",
                                f.offset_x.hypot(f.offset_y),
                                f.duration_ms,
                                kinetic_max_speed(self.camera_control.camera().zoom),
                                self.camera_control.camera().zoom,
                            );
                        }
                        self.drag_samples.clear();
                    }
                    self.drag_from = None;
                    // The release: rotation inertia, then the north snap,
                    // both applied to where the throw lands (never live --
                    // see `ios/ROTATION.md`). A cancelled touch throws
                    // nothing.
                    let rotate_velocity =
                        if phase == TouchPhase::Ended { self.rotate_samples.velocity(now) } else { 0.0 };
                    self.rotate_samples.clear();
                    self.camera_control.apply(CameraIntent::GestureEnded { rotate_velocity });
                    self.end_pinch();
                } else if self.touches.len() == 1 {
                    self.end_pinch();
                    self.begin_drag(now, true);
                } else {
                    self.begin_pinch(false);
                }
            }
        }
    }

    /// Two fingers moved. `TwoFingerGesture` decides once whether this is a
    /// pinch-and-rotate or a tilt (see `touch_gesture`), and each part of its
    /// answer becomes one intent -- about the same midpoint, from the same
    /// sample, so a hand doing all three at once does not fight itself.
    fn update_pinch(&mut self, now_ms: f64) {
        let Some(new) = self.pinch_pair() else { return };
        let Some(old) = self.pinch_points.replace(new) else { return };
        let Some(gesture) = self.pinch.as_mut() else { return };
        let was_rotating = gesture.is_rotating();
        let step = gesture.update(old, new, now_ms);
        if !was_rotating && gesture.is_rotating() {
            crate::mlog!("[touch] rotation engaged");
        }
        // While a trip follows the puck, a pinch is a zoom about the puck
        // (the follow camera keeps the puck where it rides, at the user's
        // new distance). Its centroid drift is not a pan: UIKit and winit
        // hand the two fingers over one at a time, so every pinch step is
        // two half-step pans, and a pan frees the camera -- the user's
        // "pinch while following keeps following" broke on the phone.
        let following = self.nav.state().is_some_and(|s| s.camera_mode == crate::nav::host::CameraMode::Following);
        for intent in crate::camera_control::two_finger_intents(&step) {
            if following && matches!(intent, CameraIntent::PanBy { .. } | CameraIntent::TiltBy { .. }) {
                continue;
            }
            self.camera_control.apply(intent);
        }
        if step.rotate_radians != 0.0 {
            self.rotate_samples.push(now_ms, step.rotate_radians);
        }
    }

    /// A second tap inside the double-tap window zooms in one level about
    /// the tapped point. A single tap does nothing -- it is the first half
    /// of a possible double.
    fn on_tap(&mut self, x: f64, y: f64, now_ms: f64) {
        // The navigation chrome first: a button acts, a panel swallows the
        // press, and neither becomes half of a double tap.
        if self.nav.is_active() {
            let frame = nav_frame_of(now_ms, self.camera_control.camera(), self.top_inset_pt, self.bottom_inset_pt);
            if self.nav.tap(x as f32, y as f32, &mut self.camera_control, &frame) {
                crate::mlog!("[touch] navigation chrome");
                self.last_tap = None;
                self.request_redraw();
                return;
            }
        }
        let camera = *self.camera_control.camera();
        if crate::compass::compass_visible(camera.bearing)
            && crate::compass::compass_hit_dir(x as f32, y as f32, camera.viewport_width_px, self.top_inset_pt, self.chrome_rtl)
        {
            crate::mlog!("[touch] compass -> north");
            self.camera_control.apply(CameraIntent::ResetNorth);
            self.last_tap = None;
            self.request_redraw();
            return;
        }
        // A compact credit plate: a tap toggles it (expanded <-> "i").
        if let Some(renderer) = self.renderer.as_mut() {
            if let Some(((px, py, pw, ph), true)) = renderer.attribution_hit {
                let (tx, ty) = (x as f32, y as f32);
                if tx >= px && tx <= px + pw && ty >= py && ty <= py + ph {
                    renderer.attribution_compact.toggle();
                    crate::mlog!("[touch] attribution toggled");
                    self.last_tap = None;
                    self.request_redraw();
                    return;
                }
            }
        }
        // An embedding host hit-tests its overlays where the finger lifted
        // (`host::Host::touch`). Recorded before the double-tap check, so the
        // first tap of a double still reports.
        if self.pick_on_tap {
            self.tapped_at = Some((x, y));
        }
        if let Some((px, py, pms)) = self.last_tap {
            let dt = now_ms - pms;
            let dist = (x - px).hypot(y - py);
            if dt <= DOUBLE_TAP_MAX_MS && dist <= DOUBLE_TAP_SLOP_PT {
                crate::mlog!("[touch] DOUBLE TAP ({dt:.0} ms, {dist:.0} pt apart) -> zoom in");
                self.camera_control.apply(CameraIntent::ZoomAbout {
                    levels: DOUBLE_TAP_ZOOM,
                    anchor_px: (x as f32, y as f32),
                    immediate: false,
                });
                self.last_tap = None;
                return;
            }
            crate::mlog!("[touch] tap, but not a double ({dt:.0} ms, {dist:.0} pt apart)");
        }
        self.last_tap = Some((x, y, now_ms));
    }
}

/// The scene time now under `clock`, Unix seconds: wall UTC without one.
fn scene_time_of(clock: Option<crate::render::altitude_layers::SceneClock>) -> f64 {
    match clock {
        Some(clock) => clock.at(Instant::now()),
        None => crate::render::altitude_layers::SceneClock::realtime().anchor_scene_unix,
    }
}

/// The scene clock's rate: 1 (real time) without one.
fn scene_rate_of(clock: Option<crate::render::altitude_layers::SceneClock>) -> f64 {
    clock.map_or(1.0, |c| c.rate)
}

/// Wall seconds until the altitude layers need their next frame --
/// native's `MapView::altitude_redraw_after_s`, on the same state: now
/// while a layer holds for a keyframe, else once the fastest shown dot has
/// moved a visible amount (or a keyframe event is due); `None` while
/// nothing moves.
fn altitude_redraw_after_s(altitude: &crate::render::altitude_layers::AltitudeLayers, rate: f64) -> Option<f64> {
    if !crate::render::altitude_layers::wants_frames(rate, altitude.has_shown_keyframed(), altitude.waiting) {
        return None;
    }
    if altitude.holding {
        return Some(0.0);
    }
    crate::render::altitude_pipeline::dot_redraw_after_s(rate, altitude.dot_motion)
}

impl MapApp {
    /// This frame's scene time, Unix seconds: the clock's, or wall UTC.
    fn scene_time(&self) -> f64 {
        scene_time_of(self.scene_clock)
    }

    /// The scene clock's rate: 1 (real time) when there is no clock.
    fn scene_rate(&self) -> f64 {
        scene_rate_of(self.scene_clock)
    }

    /// One pass of the frame loop: draws a frame if anything owes one, and
    /// says when the loop must next wake on its own -- a retry's backoff, a
    /// label fade's next step, a dot tile's retry -- or `None` when only an
    /// event (a touch, a resize, a tile landing) should wake it. The winit
    /// shell's `about_to_wait` and an embedding host's `render` both run
    /// exactly this.
    fn pump(&mut self) -> Option<Instant> {
        if self.retry_is_due() {
            self.needs_redraw = true;
        }
        // The other half of `fade_wake_at`: a `ControlFlow::WaitUntil` wakes
        // this run loop at the deadline regardless, but `about_to_wait` only
        // actually draws when SOMETHING says it should -- without this, the
        // wake would fire and find neither a retry due nor `needs_redraw`
        // set, skip `frame()`, and go straight back to sleep with the fade
        // still short of 1.
        if self.fade_wake_is_due() {
            self.needs_redraw = true;
        }
        if self.dot_retry_at.is_some_and(|at| Instant::now() >= at) {
            self.dot_retry_at = None;
            self.needs_redraw = true;
        }
        if self.altitude_wake_at.is_some_and(|at| Instant::now() >= at) {
            self.altitude_wake_at = None;
            self.needs_redraw = true;
        }
        if self.needs_redraw {
            self.needs_redraw = false;
            self.frame();
        }
        // A tile backing off after a transient failure needs exactly one
        // wake, when its backoff expires -- not a spinning loop. `frame()`
        // parks every expired retry it did not re-request, so this deadline
        // is in the future whenever a frame just ran; `Wait` again once
        // nothing is backing off.
        //
        // The fade's own deadline (`Renderer::fade_wake_at`) joins the same
        // `WaitUntil`, the earlier of the two winning: a still-fading label
        // needs its own wake exactly the way a backing-off tile does, and
        // for the same reason -- `self.frame()` just above dropped its
        // redraw request (see `FadeRedrawScheduler`) rather than granting it
        // immediately, so nothing else in this loop will wake it at the
        // right moment otherwise.
        let retry_wake_at = self
            .next_retry_wake_ms()
            .map(|due_ms| self.started + Duration::from_secs_f64(due_ms.max(0.0) / 1000.0));
        let fade_wake_at = self.renderer.as_ref().and_then(|r| r.fade_wake_at);
        let retry_wake_at = match (retry_wake_at, self.dot_retry_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let retry_wake_at = match (retry_wake_at, self.altitude_wake_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        match (retry_wake_at, fade_wake_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// This frame's `fill-extrusion` plan; see [`frame_extrusion_plan`].
    fn frame_extrusion_plan(&self) -> crate::render::frame_plan::ExtrusionPlan {
        frame_extrusion_plan(&self.style, self.camera_control.camera(), self.last_building_reach.get(), self.buildings_flat)
    }

    /// Installs a freshly created renderer: a new device, so the overlays'
    /// GPU half (tile caches, circle layers) that belonged to the old one is
    /// dropped and re-requested from the CPU state on the next frame.
    fn install_renderer(&mut self, renderer: Renderer) {
        self.overlays.drop_gpu();
        self.shell_layers.drop_gpu();
        self.dots_gpu = crate::overlay::host::DotLayersGpu::default();
        self.renderer = Some(renderer);
        self.resize_pending = true;
        self.needs_redraw = true;
    }

    /// Builds the renderer on `surface`, sized from [`Self::surface_metrics`]
    /// (set it first). `screen_hz` is the display's ceiling when this shell
    /// should ask Core Animation for it through its own `CADisplayLink`
    /// (the winit shell); `None` when the host drives frames itself.
    fn create_renderer(
        &self,
        instance: &wgpu::Instance,
        surface: wgpu::Surface<'static>,
        screen_hz: Option<f32>,
    ) -> anyhow::Result<Renderer> {
        // Stage timings for the launch -> first triangle budget: counts and
        // milliseconds only.
        let built = Instant::now();
        let mut stages: Vec<(&str, f64)> = Vec::new();
        let mut mark = |name: &'static str, since: &mut Instant| {
            stages.push((name, since.elapsed().as_secs_f64() * 1000.0));
            *since = Instant::now();
        };
        let mut since = Instant::now();
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .map_err(|e| anyhow::anyhow!("no wgpu adapter: {e}"))?;
        let info = adapter.get_info();

        // The engine's own device setup, called unmodified -- and it is now
        // the only branch there is. `GpuContext::new` derives its
        // `required_limits` from the adapter, so iOS Metal's 15 inter-stage
        // shader variables are no longer measured against the WebGPU
        // baseline's 16, and `unmet_shader_needs` refuses by name and by
        // number if this adapter is ever short on a limit the shaders really
        // consume. The enumeration that used to stand here was that same
        // policy hand-rolled outside the engine, because the engine did not
        // yet have it.
        let ctx = pollster::block_on(GpuContext::new(&adapter))?;
        mark("adapter+device", &mut since);

        let metrics = self.surface_metrics;
        let physical = (metrics.width_px.max(1), metrics.height_px.max(1));
        let scale_factor = (metrics.scale as f32).clamp(0.5, 8.0);
        let (width, height, backing_scale) = backing_store_size(
            (physical.0 as f32) / scale_factor,
            (physical.1 as f32) / scale_factor,
            scale_factor,
            ctx.device.limits().max_texture_dimension_2d,
        );

        let caps = surface.get_capabilities(&adapter);
        // THE ENGINE REQUIRES A NON-sRGB RENDER TARGET -- no `*Srgb` format,
        // no `*Srgb` view, no hardware encode-on-write.
        //
        // Match `map_view.rs`: configure the surface UNORM and render
        // through a view of the same format. Every shader writes its
        // authored sRGB value unchanged -- `shader.wgsl` has no
        // `srgb_to_linear_rgb` on the fragment path and
        // `TilePipeline::clear_color` decodes nothing -- so an sRGB view
        // would encode already-sRGB colour a SECOND time and the whole map
        // comes out pale and low-contrast, one encode too light (a map "in
        // a fog"), which is what it did on a phone while desktop looked
        // right. Translucent layers blend in sRGB, the space the reference
        // (Mapbox GL JS, MapLibre Native) blends in.
        //
        // Guarded rather than assumed: `remove_srgb_suffix` is a mapping,
        // not a capability check, and a backend may offer only the sRGB
        // variant -- configuring with a format it does not offer fails.
        let preferred = caps.formats[0];
        let linear = preferred.remove_srgb_suffix();
        let format = if caps.formats.contains(&linear) { linear } else { preferred };
        let view_formats: Vec<wgpu::TextureFormat> = vec![];
        crate::mlog!(
            "[map] surface {format:?} (preferred {preferred:?}, offered {:?}) -- blending in sRGB like map_view",
            caps.formats
        );
        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width,
            height,
            present_mode: wgpu::PresentMode::Fifo,
            // TWO -- wgpu's default, kept, but kept on evidence rather than
            // on inertia. wgpu-hal's Metal backend sets
            // `maximumDrawableCount` to this plus one, so this is a
            // triple-buffered layer, and a frame can be up to three refresh
            // periods behind the finger that asked for it.
            //
            // Halving that queue is the obvious latency win and it is what
            // Organic Maps effectively does -- `MetalBaseContext::Present`
            // (`libs/drape/metal/metal_base_context.mm`) ends every frame
            // with `[commandBuffer waitUntilCompleted]`, collapsing the
            // in-flight depth to ONE whatever the drawable count is, which
            // is a large part of why it feels immediate at only 60 Hz.
            //
            // It was tried here, on device, and it LOST. Same build, same
            // phone, same first second after launch:
            //
            // It was tried here, on device, and it LOST. Same phone, same
            // display link, the first second after launch in each case,
            // counted by refresh period:
            //
            // ```text
            //                          p50       120Hz  90Hz  60Hz  slower
            // latency 1 (2 drawables)  25.05 ms      3     0     0      18
            // latency 2 (3 drawables)  16.44 ms     10     3    13       6
            // latency 2 (3 drawables)  16.57 ms     16     1    12       6
            // ```
            //
            // The reason is `BAKES_PER_FRAME` and what it says about this
            // shell: tessellation is still on the main thread at roughly
            // 15 ms for a dense tile, so a frame that bakes blows an 8.3 ms
            // budget outright. Three drawables give that overrun a frame of
            // slack to hide in; two give it none, the loop blocks in
            // `nextDrawable`, and the eighteen samples in the `slower`
            // column are what that costs -- far more than the queue depth
            // saved.
            //
            // So this number is not the fix. It becomes the fix the moment
            // the bake moves off the main thread, and that is the honest
            // ordering: shorten the frame first, then shorten the queue.
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes[0],
            view_formats,
        };
        surface.configure(&ctx.device, &surface_config);

        // `-[UIScreen maximumFramesPerSecond]`, which winit surfaces as the
        // monitor's refresh rate. This is the arbiter of the whole 120 Hz
        // question: if the SCREEN says 60 on a ProMotion phone, no plist key,
        // no display link and no swapchain setting will get more out of it,
        // and the cap is Low Power Mode or Settings -> Accessibility ->
        // Motion -> Limit Frame Rate rather than anything in this bundle.
        // Read rather than assumed, because those causes are
        // indistinguishable from a measured interval alone.
        // And having read it, ASK for it. `Info.plist`'s
        // `CADisableMinimumFrameDurationOnPhone` only lifts the ceiling; a
        // process that never states a cadence is given the iPhone default of
        // 60 regardless. `display_link.rs` has the measurements and the
        // reasoning; the floor of 60 is there so a frame this app cannot
        // deliver steps the display down rather than stalling it.
        let mtm = objc2::MainThreadMarker::new();
        let frame_rate_hint = screen_hz
            .zip(mtm)
            .and_then(|(hz, mtm)| crate::display_link::FrameRateHint::request(mtm, 60.0, hz));
        // Starts PAUSED: a `CADisplayLink` is unpaused by default, and
        // nothing is gesturing or gliding yet at launch. `sync_activity_state`
        // only sends UIKit a message when the active/inactive state actually
        // CHANGES (see its own doc), so it must start in agreement with
        // `MapApp::idle_timer_disabled`'s own initial `false` -- both
        // "inactive" -- or the first frame at rest would leave the link
        // firing every vsync forever, having never been told to stop.
        if let Some(hint) = frame_rate_hint.as_ref() {
            hint.set_paused(true);
        }
        // Not cosmetic, and not only a convenience for the user: a phone that
        // auto-locks mid-run keeps this process alive and keeps it logging
        // while the drawables stop coming, which reads in the log as a
        // performance regression that is really a dark screen.
        //
        // Left alone here rather than forced on: `set_idle_timer_disabled`
        // is now called by `sync_activity_state`, on the same condition as
        // the display link's pause, so the idle timer starts (and stays)
        // exactly as UIKit's own default has it until the first touch.
        let idle_timer_scoped = mtm.is_some();

        crate::mlog!(
            "[map] gpu: {:?} ({}) | {}x{} px at x{scale_factor} | {format:?} (preferred \
             {preferred:?}; rendered through the same format) | screen max {} | asked for {} | idle timer: scoped to \
             touch/fling ({idle_timer_scoped}) | frame latency {} -> {} drawables | \
             TIMESTAMP_QUERY: {}",
            info.backend,
            info.name,
            physical.0,
            physical.1,
            match screen_hz {
                Some(hz) => format!("{hz:.0} Hz"),
                None => "unknown".to_string(),
            },
            // What Core Animation says it granted, which is not always what
            // was asked for and is never what was assumed.
            match frame_rate_hint.as_ref().map(|h| h.granted()) {
                Some(r) => format!("{:.0}-{:.0} Hz (prefer {:.0})", r.minimum, r.maximum, r.preferred),
                None => "nothing (no display link)".to_string(),
            },
            // Printed so a `[perf]` log identifies which swapchain depth
            // produced it. wgpu-hal's Metal backend sets
            // `maximumDrawableCount` to this + 1 and clamps the request to
            // 1..=2 on iOS 11.2+, so the arithmetic below is exact rather
            // than a guess.
            surface_config.desired_maximum_frame_latency,
            surface_config.desired_maximum_frame_latency + 1,
            // Printed rather than assumed, because it is the whole
            // justification for the `None` this file passes to
            // `TilePipeline::render`. `GpuContext::new` requests the feature
            // only when the adapter offers it, so the DEVICE's answer -- not
            // the adapter's -- is what a timestamp write would be validated
            // against.
            if ctx.device.features().contains(wgpu::Features::TIMESTAMP_QUERY) {
                "yes"
            } else {
                "no"
            }
        );

        let pipeline = TilePipeline::new(&ctx, format);
        let depth_view = pipeline.create_depth_view(&ctx, width, height);
        let (camera_buffer, camera_bind_group) = pipeline.create_camera_bind_group(
            &ctx,
            glam::Mat4::IDENTITY.to_cols_array_2d(),
            0.0,
            backing_scale,
        );
        mark("surface+tile pipeline", &mut since);
        let labels = Labels::new(&ctx, format);
        mark("labels (fonts, text atlas)", &mut since);
        let icon_pipeline = crate::render::icon_pipeline::IconPipeline::new(&ctx, format);
        let plate_pipeline = crate::render::plate_pipeline::PlatePipeline::new(&ctx, format);

        // The bundled sprite sheet, packed and uploaded before the first
        // frame -- see `EMBEDDED_SPRITE_JSON`'s own doc comment. This
        // always yields a real texture: a corrupt asset still produces an
        // atlas (`load_embedded_sprites`' own `SpriteAtlas::new()`
        // fallback), and `pack_icon_atlas` packs an empty one to a real,
        // if icon-less, texture rather than a zero-sized one.
        mark("icon+plate pipelines", &mut since);
        let sprites = load_embedded_sprites();
        let sprite_names: Vec<String> = sprites.names().cloned().collect();
        let (icon_atlas_image, icon_rects) =
            crate::render::icon_pipeline::pack_icon_atlas(&sprites, &sprite_names);
        let icon_atlas_size = icon_atlas_image.dimensions();
        let icon_atlas_bind_group = icon_pipeline.upload_atlas(&ctx, &icon_atlas_image);
        crate::mlog!(
            "[map] sprites: bundled sheet, {} entries, packed to {}x{}",
            sprite_names.len(),
            icon_atlas_size.0,
            icon_atlas_size.1,
        );
        // The compass, unconditionally at startup (unlike native's lazy
        // upload): this crate has no `MapView` embedder deciding whether a
        // map exists yet, so there is no earlier moment building it would
        // pre-empt, and it is one 128x128 texture.
        let compass_bind_group =
            icon_pipeline.upload_atlas(&ctx, &crate::compass::compass_artwork(crate::compass::COMPASS_ARTWORK_PX));
        let brand_mark_atlases = [crate::brand_mark::ResolvedVariant::OnLight, crate::brand_mark::ResolvedVariant::OnDark]
            .into_iter()
            .map(|variant| match crate::brand_mark::decode(variant.png()) {
                Ok(artwork) => {
                    let dimensions = artwork.dimensions();
                    Some((icon_pipeline.upload_atlas(&ctx, &artwork), dimensions))
                }
                Err(err) => {
                    crate::mlog!("[map] brand mark failed to decode, not drawing it: {err:#}");
                    None
                }
            })
            .collect::<Vec<_>>();
        debug_assert_eq!(brand_mark_atlases.len(), crate::brand_mark::RESOLVED_VARIANTS);

        // The ONE style that asks for Mapbox's own art rather than the
        // OMT-vocabulary shields the bundled sheet covers --
        // `styles::MAPBOX_STYLES`' `mapbox-streets-style.json`, whose
        // `road-number-shield`/`road-exit-shield` layers name Mapbox's own
        // sprite names, absent from the bundled sheet. Only reachable with
        // a token baked in: `TileSchema::resolve` never returns
        // `MapboxComposite` otherwise, so this never fires in a build with
        // no token. The fetch carries the SAME token this build already
        // bakes into its Mapbox TILE url -- the same licensing position:
        // nothing is redistributed, only fetched at runtime with the
        // user's own key (see this task's own notes). Queued here, before
        // the first frame, but never awaited and never blocking it:
        // `try_install_mapbox_sprites` (called from `collect_fetched`,
        // every frame) picks the bytes up whenever both land, and the
        // bundled sheet above already covers every icon until then -- and
        // forever, if a token is missing, `resolve_sprite_url` refuses the
        // reference, or either request 404s or fails outright.
        if self.schema == TileSchema::MapboxComposite {
            if let Some(sprite) = self.style.sprite.as_deref() {
                match crate::sprite::resolve_sprite_url(sprite, MAPBOX_ACCESS_TOKEN) {
                    Ok(base) => {
                        let (json_url, png_url) = crate::sprite::sprite_urls(&base);
                        self.fetcher.request_sprite(SpriteResource::Manifest, json_url);
                        self.fetcher.request_sprite(SpriteResource::Image, png_url);
                        crate::mlog!("[map] sprites: fetching mapbox's own sheet for {sprite:?}");
                    }
                    Err(err) => {
                        crate::mlog!(
                            "[map] sprites: {sprite:?} not resolved, keeping the bundled sheet: {err:#}"
                        );
                    }
                }
            }
        }

        mark("sprites, compass, brand mark", &mut since);
        crate::log::emit_or_defer(format!(
            "[map] renderer built in {:.0} ms: {}",
            built.elapsed().as_secs_f64() * 1000.0,
            stages.iter().map(|(name, ms)| format!("{name} {ms:.0}")).collect::<Vec<_>>().join(", "),
        ));
        Ok(Renderer {
            #[cfg(feature = "winit-shell")]
            window: None,
            ctx,
            surface,
            surface_config,
            pipeline,
            depth_view,
            extrusion_params: crate::render::extrusion_gpu::ExtrusionParamsGpu::new(),
            render_format: format,
            nav_route: None,
            nav_chrome: None,
            camera_buffer,
            camera_bind_group,
            frame_rate_hint,
            // Built lazily, on the first frame that actually needs each --
            // see this struct's own field docs.
            star_pipeline: None,
            altitude_pipeline: None,
            backdrop: None,
            atmosphere_pipeline: None,
            sky_pipeline: None,
            labels,
            icon_pipeline,
            icon_atlas_bind_group,
            compass_bind_group,
            brand_mark_atlases,
            plate_pipeline,
            attribution_compact: crate::attribution::CompactState::default(),
            attribution_hit: None,
            icon_rects,
            icon_atlas_size,
            icon_instances_drawn: 0,
            text_glyph_budget: Default::default(),
            label_fade: Default::default(),
            label_fade_origin: Instant::now(),
            fade_redraw_gate: Default::default(),
            label_fade_wants_redraw: false,
            fade_wake_at: None,
        })
    }
}

/// The kinetic curve, worked against Organic Maps' own formulas rather than
/// against this implementation's output.
///
/// Every expected value below is computed here from the constants -- never
/// pasted from a run -- so a change to `KINETIC_ACCELERATION`, the ceiling
/// lerp or the fadeoff makes these fail rather than quietly agree with
/// whatever the code now does.
#[cfg(test)]
mod kinetic_inertia_tests {
    use super::*;

    /// Two samples `window_ms` apart travelling `dx, dy`, i.e. a straight
    /// drag at a known speed, released the instant the last sample landed.
    fn flick(dx: f64, dy: f64, window_ms: f64, zoom: f64) -> Option<PanInertia> {
        let samples = [
            DragSample { at_ms: 1000.0, x: 0.0, y: 0.0 },
            DragSample { at_ms: 1000.0 + window_ms, x: dx as f32, y: dy as f32 },
        ];
        pan_inertia_from_samples(&samples, 1000.0 + window_ms, zoom)
    }

    /// A flick at `speed` points per second, expressed over a window short
    /// enough to sit inside [`INERTIA_VELOCITY_WINDOW_MS`].
    fn flick_at(speed: f64, zoom: f64) -> Option<PanInertia> {
        let window_ms = 50.0;
        flick(speed * window_ms / 1000.0, 0.0, window_ms, zoom)
    }

    #[test]
    fn the_speed_ceiling_rises_with_zoom_the_way_organic_maps_lerps_it() {
        // `clamp((zoom - 1) / (21 - 1), 0, 1)`, then lerp 1000 -> 5000.
        for zoom in [1.0, 6.0, 13.0, 19.0] {
            let n = (zoom - 1.0) / 20.0;
            let expected = 1000.0 * (1.0 - n) + 5000.0 * n;
            assert!((kinetic_max_speed(zoom) - expected).abs() < 1e-9, "at z{zoom}");
        }
        // Clamped at both ends, so a camera outside their zoom range cannot
        // extrapolate the ceiling past its own endpoints.
        assert_eq!(kinetic_max_speed(0.0), KINETIC_MAX_SPEED_START_PT_S);
        assert_eq!(kinetic_max_speed(99.0), KINETIC_MAX_SPEED_END_PT_S);
    }

    #[test]
    fn an_uncapped_flick_glides_four_tenths_of_one_seconds_travel() {
        // 800 pt/s is an ordinary flick and is under the ceiling at z13
        // (3400 pt/s), so no clamp is in play and the distance law is bare.
        let inertia = flick_at(800.0, 13.0).expect("800 pt/s must fling");
        let expected = KINETIC_ACCELERATION * 800.0;
        assert!((inertia.offset_x - expected).abs() < 1e-6, "{inertia:?} vs {expected}");
        assert_eq!(inertia.duration_ms, KINETIC_DURATION_SECS * 1000.0);
        // And that is the number the user was missing: eight times what
        // MapLibre's `linearity * v^2 / (2 * deceleration)` gave for the same
        // flick, which was 38.4 pt.
        assert!(expected > 300.0 && expected < 340.0);
    }

    #[test]
    fn the_same_flick_travels_further_at_street_zoom_than_at_world_zoom() {
        // Fast enough to be clamped at BOTH zooms, so what is being measured
        // is the ceiling and nothing else.
        let fast = 6000.0;
        let world = flick_at(fast, 2.0).expect("must fling at z2");
        let street = flick_at(fast, 17.0).expect("must fling at z17");
        assert!(
            (world.offset_x - KINETIC_ACCELERATION * kinetic_max_speed(2.0)).abs() < 1e-6,
            "{world:?}"
        );
        assert!(
            (street.offset_x - KINETIC_ACCELERATION * kinetic_max_speed(17.0)).abs() < 1e-6,
            "{street:?}"
        );
        assert!(
            street.offset_x > world.offset_x * 3.0,
            "street {} should be far past world {}",
            street.offset_x,
            world.offset_x
        );
    }

    #[test]
    fn a_release_slower_than_the_instant_threshold_does_not_fling_at_all() {
        assert!(flick_at(INSTANT_VELOCITY_THRESHOLD_PT_S - 1.0, 13.0).is_none());
        assert!(flick_at(INSTANT_VELOCITY_THRESHOLD_PT_S + 1.0, 13.0).is_some());
    }

    #[test]
    fn a_diagonal_flick_travels_in_a_straight_line() {
        // The bug this guards is the one the per-axis formula had: with two
        // independent eases reconciled by taking the longer duration, the
        // shorter axis finishes early and the path bends. Here the offset
        // must stay parallel to the drag, and the step must stay parallel to
        // the offset at every point along it.
        let inertia = flick(30.0, 40.0, 50.0, 13.0).expect("a 1000 pt/s diagonal must fling");
        assert!((inertia.offset_x / inertia.offset_y - 30.0 / 40.0).abs() < 1e-9);
        for t in [0.05, 0.25, 0.5, 0.9] {
            let ((dx, dy), _) = pan_inertia_step(&inertia, 1050.0 + t * inertia.duration_ms);
            assert!((dx / dy - 30.0 / 40.0).abs() < 1e-9, "bent at t={t}");
        }
    }

    #[test]
    fn the_glide_decays_exponentially_and_is_ahead_of_a_linear_brake_throughout() {
        let inertia = flick_at(1000.0, 13.0).expect("must fling");
        let total = inertia.offset_x;
        let mut applied = 0.0;
        for t in [0.1, 0.25, 0.5, 0.75, 0.99] {
            let stepped = PanInertia { applied_x: applied, ..inertia };
            let ((dx, _), finished) =
                pan_inertia_step(&stepped, inertia.started_ms + t * inertia.duration_ms);
            applied += dx;
            assert!(!finished, "t={t} must still be running");
            // Their curve: travelled fraction is `1 - e^(-4t)`.
            let expected = total * (1.0 - (-KINETIC_FADEOFF * t).exp());
            assert!((applied - expected).abs() < 1e-6, "t={t}: {applied} vs {expected}");
            // A constant-deceleration brake covers `2t - t^2` of its
            // distance by time t. Exponential decay is ahead of it the whole
            // way for the first two thirds, which is the "launches fast" feel
            // wanted here; by t=0.75 the brake has caught up because it is
            // still moving while this one has all but stopped.
            if t <= 0.5 {
                let linear_brake = total * (2.0 * t - t * t);
                assert!(applied > linear_brake, "t={t}: {applied} vs brake {linear_brake}");
            }
        }
        // And it lands exactly, with the 1.8% residual taken in one step.
        let stepped = PanInertia { applied_x: applied, ..inertia };
        let ((dx, _), finished) =
            pan_inertia_step(&stepped, inertia.started_ms + inertia.duration_ms);
        assert!(finished);
        assert!((applied + dx - total).abs() < 1e-9);
        assert!(dx > 0.0 && dx < total * 0.02, "the residual should be under 2%: {dx}");
    }
}

#[cfg(test)]
mod pinch_release_tests {
    use super::*;
    use std::thread::sleep;
    use std::time::Duration;

    // ---- `post_pinch_drag_is_trustworthy`, the pure gate ----------------
    //
    // Hand-worked against the constant, the same way the inertia curve
    // tests above are worked against Organic Maps' formulas, so a change to
    // `MIN_POST_PINCH_DRAG_MS` cannot silently make these agree with a bug.

    #[test]
    fn a_post_pinch_drag_shorter_than_the_threshold_is_not_trustworthy() {
        assert!(!post_pinch_drag_is_trustworthy(100.0, 100.0 + MIN_POST_PINCH_DRAG_MS - 1.0));
    }

    #[test]
    fn a_post_pinch_drag_at_or_past_the_threshold_is_trustworthy() {
        assert!(post_pinch_drag_is_trustworthy(100.0, 100.0 + MIN_POST_PINCH_DRAG_MS));
        assert!(post_pinch_drag_is_trustworthy(100.0, 100.0 + MIN_POST_PINCH_DRAG_MS + 200.0));
    }

    // ---- the end-to-end regression --------------------------------------
    //
    // THE PINCH regression below is driven through `on_touch` with the REAL
    // clock (`MapApp::now_ms` reads an `Instant`) rather than a mocked one:
    // the bug it guards only exists because UIKit delivers touch moves as
    // fast as they arrive, and a mocked clock would launder that away. The
    // `sleep` calls are what stand in for that arrival cadence. (The twist
    // tests further down DO mock it, through `now_override` in
    // `drive_twist_timed`, because what they grade is the fling's
    // velocity over a known span, which a real clock cannot give exactly.)

    /// Two touches spreading apart -- id 1 moving left, id 2 moving right --
    /// exactly as a real pinch reports it.
    fn drive_pinch(app: &mut MapApp, steps: u32, step_ms: u64) {
        app.on_touch(TouchPhase::Started, 1, 150.0, 300.0);
        app.on_touch(TouchPhase::Started, 2, 250.0, 300.0);
        for i in 1..=steps {
            let t = i as f64;
            app.on_touch(TouchPhase::Moved, 1, 150.0 - 5.0 * t, 300.0);
            app.on_touch(TouchPhase::Moved, 2, 250.0 + 5.0 * t, 300.0);
            sleep(Duration::from_millis(step_ms));
        }
    }

    #[test]
    fn a_pinch_dropped_to_one_finger_and_released_within_milliseconds_does_not_fling() {
        // The bug, reproduced then fixed: finger 1 lifts mid-pinch; finger 2
        // -- still travelling at pinch speed -- is sampled a couple more
        // times a couple of milliseconds apart, then lifts too. Before this
        // fix that was a fling (see `PINCH-FLING-FIX.md` for the measured
        // repro: 3 samples, a 5ms window, a 344px offset); the whole point
        // of `post_pinch_drag_is_trustworthy` is that it no longer is.
        let mut app = MapApp::new().expect("style compiles without a GPU");
        drive_pinch(&mut app, 10, 3);
        app.on_touch(TouchPhase::Ended, 1, 100.0, 300.0);
        sleep(Duration::from_millis(2));
        app.on_touch(TouchPhase::Moved, 2, 306.0, 300.0);
        sleep(Duration::from_millis(2));
        app.on_touch(TouchPhase::Moved, 2, 312.0, 300.0);
        sleep(Duration::from_millis(2));
        app.on_touch(TouchPhase::Ended, 2, 315.0, 300.0);

        assert!(
            app.pan_inertia.is_none(),
            "a release milliseconds after a pinch drops to one finger must not fling: {:?}",
            app.pan_inertia
        );
    }

    #[test]
    fn pinch_lift_one_finger_keep_dragging_then_flick_still_flings() {
        // The gesture the threshold exists to PRESERVE: a pinch, one finger
        // lifts, and the user keeps dragging with the other for a real beat
        // -- comfortably past `MIN_POST_PINCH_DRAG_MS` -- before releasing.
        // This must fling exactly as an ordinary one-finger flick does.
        let mut app = MapApp::new().expect("style compiles without a GPU");
        drive_pinch(&mut app, 10, 3);
        app.on_touch(TouchPhase::Ended, 1, 100.0, 300.0);
        for i in 1..=8 {
            app.on_touch(TouchPhase::Moved, 2, 300.0 + 6.0 * i as f64, 300.0);
            sleep(Duration::from_millis(MIN_POST_PINCH_DRAG_MS as u64 / 4));
        }
        app.on_touch(TouchPhase::Ended, 2, 400.0, 300.0);

        assert!(
            app.pan_inertia.is_some(),
            "a genuine continued drag well past the threshold must still fling"
        );
    }

    #[test]
    fn a_fresh_one_finger_flick_that_never_touched_a_pinch_is_not_gated() {
        // No pinch was ever involved, so `drag_segment_from_pinch` is false
        // and the threshold does not apply at all -- even a very quick
        // flick must fling, exactly as before this fix.
        let mut app = MapApp::new().expect("style compiles without a GPU");
        app.on_touch(TouchPhase::Started, 1, 100.0, 300.0);
        for i in 1..=3 {
            app.on_touch(TouchPhase::Moved, 1, 100.0 + 20.0 * i as f64, 300.0);
            sleep(Duration::from_millis(3));
        }
        app.on_touch(TouchPhase::Ended, 1, 165.0, 300.0);

        assert!(app.pan_inertia.is_some(), "an ordinary quick one-finger flick must still fling");
    }
}

#[cfg(test)]
mod rotation_tests {
    use super::*;
    use crate::camera_control::{CameraIntent, CameraTarget};
    use crate::touch_gesture::{MIN_TWIST_SPAN_PX, TWIST_DEADZONE_RADIANS};

    /// Runs the camera's eases to rest, as the frame loop would.
    pub(super) fn settle(app: &mut MapApp) {
        for _ in 0..2_000 {
            if !app.camera_control.advance(std::time::Duration::from_millis(16)) {
                return;
            }
        }
        panic!("the camera never settled: {:?}", app.camera());
    }

    /// `to - from`, brought into (-pi, pi]: the deleted shell helper, kept
    /// here because the direction test measures a screen angle with it.
    fn signed_angle_delta(from: f64, to: f64) -> f64 {
        let tau = std::f64::consts::TAU;
        let d = (to - from).rem_euclid(tau);
        if d > std::f64::consts::PI {
            d - tau
        } else {
            d
        }
    }

    /// The Simulator's own two-finger gesture (Option+drag) pivots a fixed
    /// pair of synthetic touches about the pointer and always ends both
    /// together -- see `PINCH-FLING-FIX.md`, which found the same limit. It
    /// can show a twist rotating, but it cannot hold the span constant
    /// while turning, cannot twist by an exact number of degrees, and
    /// cannot put the fingers 30 pt apart. Everything below therefore
    /// drives `on_touch` directly, which is the same rigorous stand-in the
    /// fling fix used, at the same level.
    ///
    /// No clock is mocked and no `sleep` appears here. Rotation itself is a
    /// function of where the fingers are, not when they got there; the one
    /// thing that reads the clock is the release velocity. These events land
    /// microseconds apart on the real clock, well inside
    /// `MIN_ROTATE_SAMPLE_SPAN_MS`, so a twist driven here throws nothing --
    /// the same answer a hand that stops before lifting gets.
    fn app_on_a_phone() -> MapApp {
        let mut app = MapApp::new().expect("style compiles without a GPU");
        // What `apply_resize` sets from a real window, in logical points
        // (an iPhone 14/15-sized screen). `MapApp::new` starts at 1x1, at
        // which every unproject in this module would be meaningless.
        app.camera_control.set_viewport(390.0, 844.0);
        app
    }

    /// The centre of that screen, in points.
    const SCREEN_MID: (f64, f64) = (195.0, 422.0);

    /// Two fingers diametrically opposite on a circle of radius `radius`
    /// about `about`, turned from screen angle `from` to `to` in `steps`
    /// equal steps. Increasing angle is a visually CLOCKWISE turn (screen y
    /// points down).
    ///
    /// The midpoint and the span are constant by construction, so this
    /// gesture pans by nothing and zooms by nothing: the only thing it can
    /// change is the bearing.
    fn drive_twist(
        app: &mut MapApp,
        about: (f64, f64),
        radius: f64,
        from: f64,
        to: f64,
        steps: u32,
    ) {
        let at = |phi: f64, half: f64| {
            (about.0 + radius * (phi + half).cos(), about.1 + radius * (phi + half).sin())
        };
        let (a, b) = (at(from, 0.0), at(from, std::f64::consts::PI));
        app.on_touch(TouchPhase::Started, 1, a.0, a.1);
        app.on_touch(TouchPhase::Started, 2, b.0, b.1);
        for i in 1..=steps {
            let phi = from + (to - from) * i as f64 / steps as f64;
            let (a, b) = (at(phi, 0.0), at(phi, std::f64::consts::PI));
            app.on_touch(TouchPhase::Moved, 1, a.0, a.1);
            app.on_touch(TouchPhase::Moved, 2, b.0, b.1);
        }
    }

    /// The band of rotation a twist of `total` radians is allowed to apply,
    /// worked from [`TWIST_DEADZONE_RADIANS`] rather than hard-coded, so
    /// that changing the constant changes the expectation instead of
    /// silently invalidating the test.
    ///
    /// Rotation engages on the first SAMPLE whose net twist reaches the
    /// deadzone, and that sample's own delta is applied (the deadzone is
    /// consumed, never repaid -- see `TwoFingerGesture::update`). So the applied
    /// rotation is the total less the deadzone, plus at most the one sample
    /// that crossed it. `step` bounds that sample: note that `drive_twist`'s
    /// nominal step is an upper bound on it, because winit delivers each
    /// finger's move as its own event -- exactly as UIKit hands them over --
    /// so the pair's angle actually advances in half-steps.
    ///
    /// For the current 10-degree deadzone, a 40-degree twist in 2-degree
    /// steps must apply between 30 and 32 degrees; it applies 30.
    ///
    /// The low end carries 1e-4 rad (0.006 deg) of slack, because the
    /// bearing is accumulated in the engine's `f32` across every sample of
    /// the gesture; the measured shortfall over a 40-sample twist is about
    /// 3e-6 rad. That is far below anything a hand or an eye can tell, and
    /// still an order of magnitude tighter than a missed or doubled
    /// deadzone.
    fn expected_rotation_band(total: f64, step: f64) -> (f64, f64) {
        let least = total.abs() - TWIST_DEADZONE_RADIANS;
        (least - 1e-4, least + step.abs())
    }

    #[track_caller]
    fn assert_rotation_in_band(applied: f64, total: f64, step: f64) {
        let (low, high) = expected_rotation_band(total, step);
        assert!(
            applied.signum() == total.signum(),
            "a twist of {:.1} deg turned the map {:.1} deg -- the wrong way",
            degrees(total),
            degrees(applied)
        );
        assert!(
            (low..=high).contains(&applied.abs()),
            "a {:.1} deg twist past a {:.1} deg deadzone must apply {:.2}..{:.2} deg, \
             applied {:.2}",
            degrees(total),
            degrees(TWIST_DEADZONE_RADIANS),
            degrees(low),
            degrees(high),
            degrees(applied.abs())
        );
    }

    fn degrees(radians: f64) -> f64 {
        radians.to_degrees()
    }

    // ---- the gesture, driven through `on_touch` -------------------------

    #[test]
    fn a_deliberate_twist_rotates_the_map() {
        let mut app = app_on_a_phone();
        let total = 40f64.to_radians();
        drive_twist(&mut app, SCREEN_MID, 90.0, 0.0, total, 20);

        // Fingers turned clockwise on screen, so the map turned clockwise
        // with them, which is a NEGATIVE bearing (bearing grows the other
        // way -- see `TwoFingerStep::rotate_radians`).
        let applied = app.camera().bearing as f64;
        assert_rotation_in_band(applied, -total, total / 20.0);
        assert!(app.pinch.is_some_and(|g| g.is_rotating()), "the gesture must have latched into rotating");
    }

    #[test]
    fn the_map_turns_the_same_way_the_fingers_do() {
        // The direction check, done through the engine's own projection
        // rather than by reading the bearing back: a sign error in
        // `TwoFingerGesture` or in the controller's anchored rotate would
        // leave the magnitude right and turn the map the wrong way.
        let mut app = app_on_a_phone();
        let probe = (SCREEN_MID.0, SCREEN_MID.1 - 100.0); // 100 pt "north" of centre
        let ground =
            crate::render::unproject_to_ground_f64(app.camera(), probe.0 as f32, probe.1 as f32)
                .expect("a pitch-0 camera has ground under every pixel");

        let total = 40f64.to_radians();
        drive_twist(&mut app, SCREEN_MID, 90.0, 0.0, total, 20);

        let (x, y) = crate::render::project_world_to_screen_f64(app.camera(), (ground.x, ground.y))
            .expect("still in front of the camera");
        // It must have swung about the anchor the same way the fingers did
        // -- clockwise, i.e. towards a LARGER screen angle -- by the twist
        // less the deadzone, keeping its distance from the anchor.
        let start = (probe.1 - SCREEN_MID.1).atan2(probe.0 - SCREEN_MID.0);
        let (dx, dy) = (x as f64 - SCREEN_MID.0, y as f64 - SCREEN_MID.1);
        let swung = signed_angle_delta(start, dy.atan2(dx));
        assert_rotation_in_band(swung, total, total / 20.0);
        assert!(
            (dx.hypot(dy) - 100.0).abs() < 0.05,
            "a rotation must not move the point towards or away from its anchor: {:.3} pt",
            dx.hypot(dy)
        );
    }

    #[test]
    fn the_ground_under_the_pinch_midpoint_stays_there() {
        // The anchor property, at a midpoint deliberately NOWHERE NEAR the
        // screen centre -- rotating about the centre would pass a test run
        // at the centre and fail every real gesture.
        let mut app = app_on_a_phone();
        let anchor = (120.0, 260.0);
        let held =
            crate::render::unproject_to_ground_f64(app.camera(), anchor.0 as f32, anchor.1 as f32)
                .expect("ground under the anchor");

        drive_twist(&mut app, anchor, 90.0, 0.0, 70f64.to_radians(), 35);
        assert!(app.pinch.is_some_and(|g| g.is_rotating()));

        let (x, y) = crate::render::project_world_to_screen_f64(app.camera(), (held.x, held.y))
            .expect("still in front of the camera");
        let slip = (x as f64 - anchor.0).hypot(y as f64 - anchor.1);
        println!("[test] anchor slip over a 70 deg twist: {slip:.6} pt");
        assert!(slip < 0.05, "the held ground slipped {slip:.4} pt out from under the fingers");
    }

    #[test]
    fn a_straight_pinch_with_incidental_rotation_does_not_rotate() {
        // A real zoom: the fingers spread from 100 pt apart to 240, drifting
        // 5 degrees on the way -- less than the deadzone, and about as much
        // as a hand does without meaning to. The zoom must happen; the
        // bearing must not move at all.
        let mut app = app_on_a_phone();
        let zoom_before = app.camera().zoom;
        let mid = SCREEN_MID;
        let at = |phi: f64, half: f64, r: f64| {
            (mid.0 + r * (phi + half).cos(), mid.1 + r * (phi + half).sin())
        };
        let (a, b) = (at(0.0, 0.0, 50.0), at(0.0, std::f64::consts::PI, 50.0));
        app.on_touch(TouchPhase::Started, 1, a.0, a.1);
        app.on_touch(TouchPhase::Started, 2, b.0, b.1);
        for i in 1..=20 {
            let t = i as f64 / 20.0;
            let (phi, r) = (5f64.to_radians() * t, 50.0 + 70.0 * t);
            let (a, b) = (at(phi, 0.0, r), at(phi, std::f64::consts::PI, r));
            app.on_touch(TouchPhase::Moved, 1, a.0, a.1);
            app.on_touch(TouchPhase::Moved, 2, b.0, b.1);
        }

        assert_eq!(
            app.camera().bearing, 0.0,
            "an incidental {:.1} deg twist (deadzone is {:.1}) must leave the map exactly at north",
            5.0,
            degrees(TWIST_DEADZONE_RADIANS)
        );
        assert!(!app.pinch.is_some_and(|g| g.is_rotating()));
        settle(&mut app);
        assert!(
            app.camera().zoom > zoom_before + 1.0,
            "the pinch itself must still zoom: {zoom_before} -> {}",
            app.camera().zoom
        );
    }

    #[test]
    fn fingers_too_close_together_never_rotate() {
        // 30 pt apart -- under `MIN_TWIST_SPAN_PX` -- twisted a quarter
        // turn. At that separation the angle is contact-patch noise, so
        // however much of it there is, it must not turn the map.
        let mut app = app_on_a_phone();
        assert!(30.0 < MIN_TWIST_SPAN_PX);
        drive_twist(&mut app, SCREEN_MID, 15.0, 0.0, std::f64::consts::FRAC_PI_2, 20);
        assert_eq!(app.camera().bearing, 0.0);
        assert!(!app.pinch.is_some_and(|g| g.is_rotating()));
    }

    #[test]
    fn rotation_stays_engaged_once_the_deadzone_is_cleared() {
        // Twist well past the deadzone, then turn all the way back through
        // the starting angle. If engagement were re-tested per frame off the
        // net rotation, the map would stop dead as the net fell back below
        // ten degrees. It must keep tracking the fingers.
        let mut app = app_on_a_phone();
        drive_twist(&mut app, SCREEN_MID, 90.0, 0.0, 40f64.to_radians(), 20);
        assert!(app.pinch.is_some_and(|g| g.is_rotating()));
        let after_first_leg = app.camera().bearing as f64;
        // Same gesture, fingers now turning the other way, back past the
        // start to -40 degrees.
        for i in 1..=40 {
            let phi = 40f64.to_radians() - 80f64.to_radians() * i as f64 / 40.0;
            let at = |half: f64| {
                (SCREEN_MID.0 + 90.0 * (phi + half).cos(), SCREEN_MID.1 + 90.0 * (phi + half).sin())
            };
            let (a, b) = (at(0.0), at(std::f64::consts::PI));
            app.on_touch(TouchPhase::Moved, 1, a.0, a.1);
            app.on_touch(TouchPhase::Moved, 2, b.0, b.1);
        }
        // Every one of those 80 degrees must have landed on top of where the
        // first leg left the map -- no second deadzone, and no stall as the
        // net twist fell back through ten degrees. (The return leg also
        // passes straight through north on its way, which is the subject of
        // `north_is_not_a_detent_during_the_gesture`.)
        let applied = degrees(app.camera().bearing as f64);
        let expect = degrees(after_first_leg + 80f64.to_radians());
        assert!((applied - expect).abs() < 1e-2, "expected {expect:.2} deg, got {applied:.2}");
        assert!(
            app.pinch.is_some_and(|g| g.is_rotating()),
            "engagement must survive turning back through the start"
        );
    }

    /// `drive_twist` on a synthetic clock: the events are stamped evenly
    /// from `start_ms` to `start_ms + span_ms` through `now_override`, and
    /// the clock is left at the last stamp.
    fn drive_twist_timed(app: &mut MapApp, to: f64, steps: u32, start_ms: f64, span_ms: f64) {
        let at = |phi: f64, half: f64| (SCREEN_MID.0 + 90.0 * (phi + half).cos(), SCREEN_MID.1 + 90.0 * (phi + half).sin());
        let events = 2 * steps + 2;
        let mut event = 0;
        let mut stamp = |app: &mut MapApp| {
            app.now_override = Some(start_ms + span_ms * f64::from(event) / f64::from(events - 1));
            event += 1;
        };
        let (a, b) = (at(0.0, 0.0), at(0.0, std::f64::consts::PI));
        stamp(app);
        app.on_touch(TouchPhase::Started, 1, a.0, a.1);
        stamp(app);
        app.on_touch(TouchPhase::Started, 2, b.0, b.1);
        for i in 1..=steps {
            let phi = to * f64::from(i) / f64::from(steps);
            let (a, b) = (at(phi, 0.0), at(phi, std::f64::consts::PI));
            stamp(app);
            app.on_touch(TouchPhase::Moved, 1, a.0, a.1);
            stamp(app);
            app.on_touch(TouchPhase::Moved, 2, b.0, b.1);
        }
    }

    #[test]
    fn a_twist_released_near_north_settles_on_north_after_the_release() {
        let mut app = app_on_a_phone();
        // The whole twist inside half a millisecond: one frame's events,
        // which is what a hand that stops before lifting leaves in the
        // velocity window.
        drive_twist_timed(&mut app, 15f64.to_radians(), 15, 1_000.0, 0.5);
        let during = app.camera().bearing;
        assert!(during != 0.0 && during.abs() < 7f32.to_radians(), "~5 degrees land after the deadzone: {during}");
        assert_eq!(app.rotate_samples.velocity(app.now_ms()), 0.0, "a same-frame twist must not throw");
        app.on_touch(TouchPhase::Ended, 1, 195.0, 332.0);
        app.on_touch(TouchPhase::Ended, 2, 195.0, 512.0);
        settle(&mut app);
        assert_eq!(app.camera().bearing, 0.0, "released inside the 7-degree window, it settles ON north");
    }

    #[test]
    fn a_twist_spread_over_forty_milliseconds_is_thrown_on_release() {
        // The mirror of the test above, so the clock seam cannot pass by
        // making every velocity zero: the same kind of twist over 40 ms of
        // synthetic time is a flick, and the map keeps turning after it.
        let mut app = app_on_a_phone();
        drive_twist_timed(&mut app, 40f64.to_radians(), 20, 1_000.0, 40.0);
        let released = app.camera().bearing;
        let v = app.rotate_samples.velocity(app.now_ms());
        assert!(v < 0.0, "a clockwise flick throws a negative bearing velocity: {v}");
        app.on_touch(TouchPhase::Ended, 1, 195.0, 332.0);
        app.on_touch(TouchPhase::Ended, 2, 195.0, 512.0);
        assert!(app.camera_control.advance(std::time::Duration::from_millis(16)), "the throw is still turning");
        assert!(app.camera().bearing < released, "the first frame after release keeps turning the same way");
        settle(&mut app);
        let thrown = released - app.camera().bearing;
        assert!(thrown > 5f32.to_radians(), "the throw added {:.1} deg", thrown.to_degrees());
    }

    #[test]
    fn north_is_not_a_detent_during_the_gesture() {
        // Native's rule (map_view.rs:845-855): a snap that fires mid-gesture
        // makes the map fight the fingers. The bearing must pass THROUGH
        // north while the fingers are down.
        let mut app = app_on_a_phone();
        drive_twist(&mut app, SCREEN_MID, 90.0, 0.0, 40f64.to_radians(), 20);
        let mut crossed_near_north = false;
        for i in 1..=60 {
            let phi = 40f64.to_radians() - 60f64.to_radians() * i as f64 / 60.0;
            let at = |half: f64| (SCREEN_MID.0 + 90.0 * (phi + half).cos(), SCREEN_MID.1 + 90.0 * (phi + half).sin());
            let (a, b) = (at(0.0), at(std::f64::consts::PI));
            app.on_touch(TouchPhase::Moved, 1, a.0, a.1);
            app.on_touch(TouchPhase::Moved, 2, b.0, b.1);
            let brg = app.camera().bearing;
            if brg != 0.0 && brg.abs() < 5f32.to_radians() {
                crossed_near_north = true;
            }
        }
        assert!(crossed_near_north, "the bearing must take values inside the snap window, not be held at 0");
    }

    #[test]
    fn a_two_finger_vertical_drag_tilts_the_map() {
        let mut app = app_on_a_phone();
        app.on_touch(TouchPhase::Started, 1, 145.0, 500.0);
        app.on_touch(TouchPhase::Started, 2, 245.0, 500.0);
        for i in 1..=20 {
            let y = 500.0 - 5.0 * i as f64; // both fingers UP
            app.on_touch(TouchPhase::Moved, 1, 145.0, y);
            app.on_touch(TouchPhase::Moved, 2, 245.0, y);
        }
        assert!(app.camera().pitch > 0.5, "a 100 pt upward two-finger drag tilts into 3D: {}", app.camera().pitch);
        assert_eq!(app.camera().bearing, 0.0, "and does not rotate");
    }

    #[test]
    fn a_settled_camera_stops_asking_for_frames() {
        // The heat fix's contract (2026-09-23): `frame()` requests another
        // frame only while something is moving. Here, only the camera is.
        let mut app = app_on_a_phone();
        app.camera_control.apply(CameraIntent::ResetNorthAndPitch);
        app.camera_control.jump_to(CameraTarget { bearing: Some(1.0), ..CameraTarget::default() });
        app.camera_control.apply(CameraIntent::ResetNorth);
        assert!(!app.camera_control.is_settled());
        // Driven through `advance_motion`, the frame loop's own redraw
        // decision, on a synthetic 60 Hz clock (a GPU-less `frame()` returns
        // before it gets there).
        let mut now = 1_000.0;
        let mut frames = 0;
        while app.advance_motion(now) {
            now += 16.0;
            frames += 1;
            assert!(frames < 1_000, "the frame loop never stopped asking: {:?}", app.camera());
        }
        assert!(frames > 1, "a 57-degree ease must take more than one frame: {frames}");
        assert_eq!(app.camera().bearing, 0.0, "it stopped because it arrived");
        assert!(app.camera_control.is_settled());
        assert!(!app.advance_motion(now + 16.0), "and a settled camera stays quiet");
        assert!(!app.advance_motion(now + 5_000.0), "however long the gap");
    }

    #[test]
    fn a_rotated_map_pans_where_the_finger_goes() {
        // iOS's old `pan_by_pixels` predated rotation and read screen deltas
        // as world deltas. On a map turned 45 degrees that sends the drag off at 45
        // degrees to the finger. The ground under the finger must stay under
        // it -- which is also, incidentally, what makes a fling on a rotated
        // map glide in the direction it was thrown.
        let mut app = app_on_a_phone();
        app.camera_control.jump_to(CameraTarget { bearing: Some(45f32.to_radians()), ..CameraTarget::default() });
        let (from, to) = ((100.0, 600.0), (260.0, 300.0));
        let held = crate::render::unproject_to_ground_f64(app.camera(), from.0 as f32, from.1 as f32)
            .expect("ground under the finger");
        app.on_touch(TouchPhase::Started, 1, from.0, from.1);
        app.on_touch(TouchPhase::Moved, 1, to.0, to.1);

        let (x, y) = crate::render::project_world_to_screen_f64(app.camera(), (held.x, held.y))
            .expect("still in front of the camera");
        let slip = (x as f64 - to.0).hypot(y as f64 - to.1);
        println!("[test] finger slip over a 340 pt drag on a 45 deg map: {slip:.6} pt");
        assert!(slip < 0.5, "the ground under the finger slipped {slip:.3} pt on a rotated map");
    }

    #[test]
    fn a_twist_that_releases_both_fingers_does_not_fling() {
        // The pinch-fling gate, re-checked with rotation in the mix: a twist
        // is not a throw, and lifting out of one must not glide.
        let mut app = app_on_a_phone();
        drive_twist(&mut app, SCREEN_MID, 90.0, 0.0, 60f64.to_radians(), 30);
        app.on_touch(TouchPhase::Ended, 1, 240.0, 500.0);
        app.on_touch(TouchPhase::Ended, 2, 150.0, 344.0);
        assert!(app.pan_inertia.is_none(), "{:?}", app.pan_inertia);
        assert!(app.pinch.is_none(), "the gesture's rotation state must die with it");
    }
}

/// The phone's navigation wiring, without a GPU: launch requests, the safe
/// area, the Simulate control and the frame loop's redraw decision.
#[cfg(test)]
mod nav_tests {
    use super::*;

    /// An iPhone 16 Pro in points: 62 pt of status bar and Dynamic Island,
    /// 34 pt of home indicator (`drawable_size`'s doc).
    fn app_on_a_phone() -> MapApp {
        let mut app = MapApp::new().expect("style compiles without a GPU");
        app.camera_control.set_viewport(402.0, 874.0);
        app.top_inset_pt = 62.0;
        app.bottom_inset_pt = 34.0;
        app
    }

    #[test]
    fn every_demo_route_is_found_by_name_and_an_unknown_one_falls_back() {
        for (name, bytes) in DEMO_ROUTES {
            assert!(!bytes.is_empty(), "{name}");
            assert!(demo_route_bytes(name) == bytes, "{name}");
            let mut app = app_on_a_phone();
            app.nav.set_demo_route(Arc::from(bytes), crate::nav::host::SimOptions::default());
            let frame = nav_frame_of(0.0, app.camera(), app.top_inset_pt, app.bottom_inset_pt);
            assert_eq!(app.nav.start_simulation(&app.camera_control, &frame), Ok(()), "{name} must drive");
        }
        assert!(demo_route_bytes("../../etc/passwd") == DEMO_ROUTES[0].1);
        assert!(demo_route_bytes("singapore-seletar-rotary-uturn") != DEMO_ROUTES[0].1);
    }

    #[test]
    fn the_frame_carries_the_status_bar_and_the_home_indicator() {
        let app = app_on_a_phone();
        let frame = nav_frame_of(5.0, app.camera(), app.top_inset_pt, app.bottom_inset_pt);
        assert_eq!(frame.viewport, (402.0, 874.0));
        assert_eq!((frame.insets.top, frame.insets.bottom), (62.0, 34.0));
        assert_eq!((frame.insets.left, frame.insets.right), (0.0, 0.0));
        assert!(frame.wall.unix_ms > 0.0);
    }

    #[test]
    fn no_request_means_no_navigation_and_no_frames() {
        let mut app = app_on_a_phone();
        assert!(!app.nav.is_active());
        assert!(!app.advance_motion(1_000.0));
        assert!(!app.advance_motion(1_016.0));
    }

    #[test]
    fn the_simulate_button_sits_above_the_home_indicator_and_starts_the_drive() {
        let mut app = app_on_a_phone();
        app.nav.set_demo_route(Arc::from(DEMO_ROUTES[0].1), crate::nav::host::SimOptions::default());
        app.nav.set_show_simulate(true);
        // Offered, but idle: it asks for no frames of its own.
        assert!(app.nav.is_active() && !app.nav.trip_active());
        assert!(!app.advance_motion(1_000.0), "an idle Simulate button must let the display link rest");

        let frame = nav_frame_of(1_000.0, app.camera(), app.top_inset_pt, app.bottom_inset_pt);
        let scene = app.nav.scene(app.camera(), &frame);
        let button = scene.buttons.first().expect("the Simulate button").rect;
        assert!(button.y + button.h <= 874.0 - 34.0, "clear of the home indicator: {button:?}");

        app.on_tap(f64::from(button.x + button.w / 2.0), f64::from(button.y + button.h / 2.0), 1_000.0);
        assert!(app.nav.trip_active(), "a tap on Simulate starts the drive");
        assert!(app.last_tap.is_none(), "and is not the first half of a double tap");
        assert!(app.advance_motion(1_016.0), "a drive asks for frames");
    }

    #[test]
    fn autostart_drives_on_the_first_frame_and_a_paused_drive_rests() {
        let mut app = app_on_a_phone();
        app.nav.set_demo_route(Arc::from(DEMO_ROUTES[0].1), crate::nav::host::SimOptions::default());
        app.nav_autostart = true;
        assert!(app.advance_motion(1_000.0));
        assert!(app.nav.trip_active() && !app.nav_autostart, "started once, on the first frame");
        app.nav.toggle_pause();
        let mut now = 1_000.0;
        let mut frames = 0;
        while app.advance_motion(now) {
            now += 16.0;
            frames += 1;
            assert!(frames < 1_000, "a paused drive never stopped asking for frames");
        }
        assert!(!app.advance_motion(now + 5_000.0), "and stays quiet");
    }
}

#[cfg(test)]
mod style_switch_tests {
    use super::rotation_tests::settle;
    use super::*;
    use crate::camera_control::CameraTarget;

    /// The same stand-in `rotation_tests` uses, for the same reason: the
    /// Simulator cannot synthesise a third contact at all, so the gesture is
    /// driven through `on_touch` directly. Nothing here is timed -- a
    /// three-finger switch is a function of how many fingers are down, not
    /// of when they arrived -- so no clock is mocked and nothing sleeps.
    fn app_on_a_phone() -> MapApp {
        let mut app = MapApp::new().expect("style compiles without a GPU");
        app.camera_control.set_viewport(390.0, 844.0);
        app
    }

    /// Three fingers down and three fingers up, all still, spread out the way
    /// a hand actually lands -- not on top of each other.
    fn three_finger_touch(app: &mut MapApp) {
        app.on_touch(TouchPhase::Started, 1, 120.0, 400.0);
        app.on_touch(TouchPhase::Started, 2, 195.0, 400.0);
        app.on_touch(TouchPhase::Started, 3, 270.0, 400.0);
        app.on_touch(TouchPhase::Ended, 1, 120.0, 400.0);
        app.on_touch(TouchPhase::Ended, 2, 195.0, 400.0);
        app.on_touch(TouchPhase::Ended, 3, 270.0, 400.0);
    }

    #[test]
    fn every_vendored_style_in_the_cycle_compiles_to_layers() {
        // The switcher's failure mode is a style that parses to nothing and
        // leaves a blank screen, which on a phone is indistinguishable from
        // a crashed renderer. Checked here rather than trusted, for all
        // three, because two of them have never been compiled on this
        // platform at all.
        for (i, choice) in crate::styles::STYLES.iter().enumerate() {
            let (style, _) = compile_style(crate::styles::STYLES, i)
                .unwrap_or_else(|e| panic!("style {:?} must compile: {e:#}", choice.name));
            assert!(
                style.layers.len() > 20,
                "style {:?} compiled to only {} layers, which is not a basemap",
                choice.name,
                style.layers.len()
            );
        }
    }

    #[test]
    fn every_mapbox_schema_style_in_the_cycle_compiles_to_layers() {
        // The Mapbox-schema cycle's own version of the test above -- one
        // entry today (`mapbox-streets-style.json`), but iterated rather
        // than indexed by hand so a second entry is covered automatically.
        // The style is Mapbox's and staged from `MAPBOX_FIXTURES` at build
        // time (see `build.rs`); without it there is nothing to check.
        if crate::styles::MAPBOX_STYLES[0].json.is_empty() {
            eprintln!("skipped: MAPBOX_FIXTURES was unset when this test binary was built");
            return;
        }
        for (i, choice) in crate::styles::MAPBOX_STYLES.iter().enumerate() {
            let (style, _) = compile_style(crate::styles::MAPBOX_STYLES, i)
                .unwrap_or_else(|e| panic!("style {:?} must compile: {e:#}", choice.name));
            assert!(
                style.layers.len() > 20,
                "style {:?} compiled to only {} layers, which is not a basemap",
                choice.name,
                style.layers.len()
            );
        }
    }

    #[test]
    fn mapbox_tile_url_template_bakes_the_token_and_keeps_the_placeholders() {
        // A fake token (never a real one in a test), just to check the
        // shape: the real `composite` tileset ids from the vendored
        // fixture, Mapbox's v4 endpoint, the token as a query param, and
        // the `{z}/{x}/{y}` placeholders left LITERAL for
        // `request_missing_tiles`'s own `.replace` calls to fill in.
        if crate::styles::MAPBOX_STYLES[0].json.is_empty() {
            assert!(mapbox_tile_url_template("pk.test-token-not-real").is_err(), "no staged style, no URL");
            return;
        }
        let url = mapbox_tile_url_template("pk.test-token-not-real").expect("vendored fixture must parse");
        assert!(url.starts_with("https://api.mapbox.com/v4/"), "unexpected URL shape: {url}");
        assert!(url.contains("{z}/{x}/{y}.mvt"), "placeholders must stay literal: {url}");
        assert!(url.ends_with("access_token=pk.test-token-not-real"), "token must be the query param: {url}");
    }

    #[test]
    fn tile_schema_falls_back_to_omt_without_a_build_time_token() {
        // `MAPBOX_ACCESS_TOKEN` is `option_env!`, resolved once when this
        // test binary was compiled -- and nothing in this repository's own
        // `cargo test` invocation exports it (only `run-device.sh` does),
        // so this is the real, not simulated, no-token path.
        let (schema, tile_url) = TileSchema::resolve();
        assert_eq!(schema, TileSchema::Omt, "no token baked in -- must fall back to OMT");
        assert_eq!(tile_url, TILE_URL, "the OMT fallback must use OpenFreeMap's own template");
    }

    /// FAILS on master: `SOURCE_MAX_ZOOM` was hardcoded to 14 everywhere,
    /// so a Mapbox composite source (real ceiling z16) was never asked past
    /// z14 -- exactly where `examples/mapbox-streets-style.json`'s
    /// `building` layer (`minzoom: 15`) has nothing to draw. New York's
    /// missing buildings, reproduced at the zoom-selection layer: at 15.5
    /// the tile zoom must be 15, not 14, and at 17 it must be 16 (the
    /// ceiling), not 14.
    ///
    /// `app.schema` is set directly rather than through `TileSchema::resolve`
    /// (which needs a build-time token this test binary never has) --
    /// a private field, reachable because this module is `super::*` inside
    /// the same crate, and the same seam `now_override` already uses for a
    /// value production only ever sets once, at construction.
    #[test]
    fn tile_zoom_caps_a_mapbox_composite_source_to_its_own_ceiling_not_omts() {
        let mut app = app_on_a_phone();
        app.schema = TileSchema::MapboxComposite;

        app.camera_control.jump_to(CameraTarget { zoom: Some(15.5), ..CameraTarget::default() });
        assert_eq!(app.tile_zoom(), 15, "z15.5 on a Mapbox composite source must ask for z15, not z14");

        app.camera_control.jump_to(CameraTarget { zoom: Some(17.0), ..CameraTarget::default() });
        assert_eq!(app.tile_zoom(), 16, "z17 on a Mapbox composite source must clamp to its own z16 ceiling, not z14");
    }

    /// The other half of the same fix: an OpenMapTiles source must stay
    /// capped at 14 -- the fix is a per-source ceiling, not a blanket bump
    /// to 16 that would ask OpenFreeMap for tiles it does not have.
    #[test]
    fn tile_zoom_still_caps_an_omt_source_at_fourteen() {
        let mut app = app_on_a_phone();
        assert_eq!(app.schema, TileSchema::Omt, "the default build (no token) is OMT");

        app.camera_control.jump_to(CameraTarget { zoom: Some(15.5), ..CameraTarget::default() });
        assert_eq!(app.tile_zoom(), 14, "z15.5 on an OMT source must still clamp to z14");

        app.camera_control.jump_to(CameraTarget { zoom: Some(17.0), ..CameraTarget::default() });
        assert_eq!(app.tile_zoom(), 14, "z17 on an OMT source must still clamp to z14");
    }

    /// The property that makes this a per-source decision rather than a
    /// value latched once at startup: `tile_zoom()` reads
    /// `self.schema.source_max_zoom()` fresh every call, so a schema change
    /// is visible on the very next read with nothing else to invalidate --
    /// unlike the constant it replaces, which could not have updated at
    /// all.
    #[test]
    fn switching_the_active_schema_updates_the_tile_zoom_cap_live() {
        let mut app = app_on_a_phone();
        app.camera_control.jump_to(CameraTarget { zoom: Some(17.0), ..CameraTarget::default() });
        assert_eq!(app.tile_zoom(), 14, "starts on OMT: capped at 14");

        app.schema = TileSchema::MapboxComposite;
        assert_eq!(app.tile_zoom(), 16, "the same camera zoom, after switching schema, caps at the new source's 16");

        app.schema = TileSchema::Omt;
        assert_eq!(app.tile_zoom(), 14, "switching back drops the cap to 14 again");
    }

    #[test]
    fn three_fingers_advance_one_style_and_wrap_at_the_end() {
        let mut app = app_on_a_phone();
        assert_eq!(app.style_index, 0);
        for expected in 1..=crate::styles::STYLES.len() {
            three_finger_touch(&mut app);
            assert_eq!(
                app.style_index,
                expected % crate::styles::STYLES.len(),
                "the {expected}th switch landed on the wrong style"
            );
        }
    }

    #[test]
    fn a_switch_changes_the_compiled_style_and_the_clear_colour() {
        // Not just the index: the two things a frame actually reads. A
        // switcher that advanced a counter and drew the old style would pass
        // the test above and fail on the glass.
        let mut app = app_on_a_phone();
        let before_layers = app.style.layers.len();
        let before_background = app.background;
        three_finger_touch(&mut app);
        assert_ne!(
            (app.style.layers.len(), colour_bits(app.background)),
            (before_layers, colour_bits(before_background)),
            "the live style and its clear colour are unchanged after a switch"
        );
    }

    /// `wgpu::Color` is four `f64`s and is not `Eq`; comparing bit patterns
    /// is exact and is all this needs -- two styles' backgrounds differing
    /// by less than an ULP would be a coincidence worth failing on anyway.
    fn colour_bits(c: wgpu::Color) -> (u64, u64, u64, u64) {
        (c.r.to_bits(), c.g.to_bits(), c.b.to_bits(), c.a.to_bits())
    }

    /// Panning at 60 degrees fed `decoded` up to 512 tiles a view and
    /// nothing ever removed one. Past [`MAX_DECODED_TILES`] the discard
    /// pile goes, and only it: every cover and retained tile stays.
    #[test]
    fn the_decoded_tiles_are_bounded_to_the_cover_and_the_retained_set() {
        let mut app = app_on_a_phone();
        let tile = |x: u32| TileId { z: 16, x, y: 32000 };
        for x in 0..200 {
            app.decoded.insert(tile(x), DecodedTile { data_hash: 0, layers: vec![] });
        }
        app.retained = (190..200).map(tile).collect();
        let cover: Vec<TileId> = (0..10).map(tile).collect();
        app.evict_discard_pile(cover.clone());
        assert_eq!(app.decoded.len(), 20, "cover (10) + retained (10) survive, the other 180 go");
        assert!(cover.iter().chain(app.retained.iter()).all(|t| app.decoded.contains_key(t)));

        // Under the bound nothing is touched, discard pile or not.
        let mut app = app_on_a_phone();
        for x in 0..MAX_DECODED_TILES as u32 {
            app.decoded.insert(tile(x), DecodedTile { data_hash: 0, layers: vec![] });
        }
        app.evict_discard_pile(Vec::new());
        assert_eq!(app.decoded.len(), MAX_DECODED_TILES);
    }

    #[test]
    fn a_style_switch_keeps_every_decoded_tile() {
        // The property that makes the switch free: MVT bytes know nothing
        // about a style, so a swap must never provoke a refetch. If this
        // ever fails, every switch costs a full cover over the network.
        let mut app = app_on_a_phone();
        app.decoded.insert(TileId { z: 13, x: 6459, y: 4066 }, DecodedTile { data_hash: 0, layers: vec![] });
        app.decoded.insert(TileId { z: 13, x: 6460, y: 4066 }, DecodedTile { data_hash: 0, layers: vec![] });
        three_finger_touch(&mut app);
        assert_eq!(app.decoded.len(), 2, "a style switch must not evict decoded tiles");
    }

    #[test]
    fn the_hand_that_switches_the_style_does_not_also_move_the_map() {
        // The whole reason the switch fires on the third finger's DOWN and
        // then latches. Two fingers are already pinching by the time the
        // third lands, and a hand settling onto glass is never perfectly
        // still -- so without the latch this gesture pans, zooms and rotates
        // on its way to changing a palette.
        let mut app = app_on_a_phone();
        let before = (app.camera().center_lon, app.camera().center_lat, app.camera().zoom, app.camera().bearing);

        app.on_touch(TouchPhase::Started, 1, 120.0, 400.0);
        app.on_touch(TouchPhase::Started, 2, 195.0, 400.0);
        app.on_touch(TouchPhase::Started, 3, 270.0, 400.0);
        // The wobble: all three fingers drift, and the outer pair both
        // spreads and twists -- a pinch-plus-rotate if anything were reading
        // it.
        for i in 1..=6 {
            let d = 4.0 * i as f64;
            app.on_touch(TouchPhase::Moved, 1, 120.0 - d, 400.0 - d);
            app.on_touch(TouchPhase::Moved, 2, 195.0, 400.0 + d);
            app.on_touch(TouchPhase::Moved, 3, 270.0 + d, 400.0 + d);
        }
        app.on_touch(TouchPhase::Ended, 1, 96.0, 376.0);
        app.on_touch(TouchPhase::Ended, 2, 195.0, 424.0);
        app.on_touch(TouchPhase::Ended, 3, 294.0, 424.0);
        // Anything the gesture asked for would land in an ease, not at once.
        settle(&mut app);

        assert_eq!(app.style_index, 1, "the switch itself must still have happened");
        assert_eq!(
            (app.camera().center_lon, app.camera().center_lat, app.camera().zoom, app.camera().bearing),
            before,
            "a three-finger switch moved the camera"
        );
        assert!(app.pan_inertia.is_none(), "a three-finger switch must not fling");
        assert!(!app.multi_touch_consumed, "the latch must clear when the last finger lifts");
    }

    #[test]
    fn the_finger_that_leaves_a_style_switch_is_not_half_of_a_double_tap() {
        // A hand lifts one finger at a time, and the last one off is a
        // short, still contact that the tap classifier would otherwise
        // accept. Paired with a real tap a moment later it zooms the map --
        // a switch followed by a mystery zoom.
        let mut app = app_on_a_phone();
        three_finger_touch(&mut app);
        let zoom_after_switch = app.camera().zoom;
        app.on_touch(TouchPhase::Started, 9, 195.0, 400.0);
        app.on_touch(TouchPhase::Ended, 9, 195.0, 400.0);
        // A double tap EASES its zoom in, so reading the zoom straight after
        // the tap would pass whether or not it zoomed (ruling F5).
        settle(&mut app);
        assert_eq!(app.camera().zoom, zoom_after_switch, "a single tap after a switch zoomed the map");
    }

    #[test]
    fn one_and_two_finger_gestures_still_work_after_a_switch() {
        // The latch is per-gesture, not sticky. A pan immediately after a
        // style switch must pan.
        let mut app = app_on_a_phone();
        three_finger_touch(&mut app);
        let before_lon = app.camera().center_lon;
        app.on_touch(TouchPhase::Started, 1, 195.0, 400.0);
        app.on_touch(TouchPhase::Moved, 1, 145.0, 400.0);
        app.on_touch(TouchPhase::Ended, 1, 145.0, 400.0);
        assert_ne!(app.camera().center_lon, before_lon, "the map would not pan after a style switch");
    }
}

#[cfg(test)]
mod fetch_retry_tests {
    use super::*;

    const TILE: TileId = TileId { z: 7, x: 90, y: 40 };

    /// A GPU-less, network-free app. `handle_fetched` and
    /// `should_request_vector` are the two halves of the fetch loop that
    /// need neither, so nothing here ever reaches `net::Fetcher`'s workers.
    fn app_at(now_ms: f64) -> MapApp {
        let mut app = MapApp::new().expect("style compiles without a GPU");
        app.now_override = Some(now_ms);
        app
    }

    #[test]
    fn a_transient_failure_is_retried_once_its_backoff_expires() {
        // The field report: a tile that failed ONCE on weak cellular (a
        // timeout, a reset, a 429) was never asked for again, and the map
        // stayed blank there for the rest of the session.
        let mut app = app_at(1_000.0);
        app.inflight.insert(TILE);
        app.handle_fetched(Fetched::Failed(TILE, TileKind::Vector, crate::net::TileFailure::new("timeout: global")));
        assert!(!app.inflight.contains(&TILE));
        assert!(!app.should_request_vector(TILE), "still backing off right after the failure");
        app.now_override = Some(1_000.0 + crate::fetch_policy::MAX_BACKOFF_MS);
        assert!(app.should_request_vector(TILE), "a transient failure must be retried, not blacklisted");
    }

    #[test]
    fn a_404_is_never_asked_for_again() {
        let mut app = app_at(1_000.0);
        app.inflight.insert(TILE);
        app.handle_fetched(Fetched::Missing(TILE, TileKind::Vector));
        app.now_override = Some(1_000.0 + 10.0 * crate::fetch_policy::MAX_BACKOFF_MS);
        assert!(!app.should_request_vector(TILE));
        assert_eq!(app.next_retry_wake_ms(), None, "absence never wakes the loop");
    }

    #[test]
    fn a_pending_retry_wakes_the_loop_once_and_does_not_keep_it_spinning() {
        // The heat fix's contract, for retries: a tile backing off asks for
        // ONE wake when it comes due. If the camera no longer wants it by
        // then, the frame that wake ran parks it and the loop goes back to
        // `ControlFlow::Wait`.
        let mut app = app_at(1_000.0);
        app.inflight.insert(TILE);
        let failure = crate::net::TileFailure { message: "HTTP 429".into(), retry_after_ms: Some(5_000.0) };
        app.handle_fetched(Fetched::Failed(TILE, TileKind::Vector, failure));
        assert!(!app.retry_is_due(), "nothing is owed while the backoff runs");
        assert_eq!(app.next_retry_wake_ms(), Some(6_000.0), "Retry-After sets the wake");
        app.now_override = Some(6_000.0);
        assert!(app.retry_is_due(), "the backoff expired: one frame is owed");
        // That frame's request pass did not ask for TILE (the camera moved).
        app.park_overdue_retries();
        assert!(!app.retry_is_due());
        assert_eq!(app.next_retry_wake_ms(), None, "and the loop goes back to Wait");
        // Coming back into view still re-requests it straight away.
        assert!(app.should_request_vector(TILE));
    }

    #[test]
    fn a_truncated_body_is_retried_and_a_success_clears_the_retry() {
        let mut app = app_at(1_000.0);
        app.inflight.insert(TILE);
        // Not an MVT: what a connection cut mid-body leaves behind.
        app.handle_fetched(Fetched::Body(TILE, TileKind::Vector, false, vec![0x1a, 0xff, 0xff]));
        assert!(!app.missing.contains(&TILE), "a bad body is not absence");
        assert_eq!(app.retries.len(), 1);
        app.now_override = Some(1_000.0 + crate::fetch_policy::MAX_BACKOFF_MS);
        assert!(app.should_request_vector(TILE));
        app.inflight.insert(TILE);
        app.retries.on_requested(TILE);
        app.handle_fetched(Fetched::Body(TILE, TileKind::Vector, false, Vec::new()));
        assert!(app.retries.is_empty(), "a decoded tile has nothing left to retry");
        assert!(app.decoded.contains_key(&TILE));
    }

    /// The device log's second half: after a pan and a zoom-out, bytes for
    /// tiles the camera had left kept arriving and were ALL decoded on the
    /// frame thread. A body whose tile is neither wanted nor retained is now
    /// discarded undecoded, and its in-flight mark cleared so the camera
    /// coming back asks for it again.
    #[test]
    fn a_body_for_a_tile_the_camera_left_is_discarded_undecoded() {
        let mut app = app_at(1_000.0);
        app.inflight.insert(TILE);
        app.raster_pending.insert(TILE);
        // This frame's request pass wanted nothing at TILE.
        app.wanted = crate::fetch_queue::Wanted::new();
        app.arrived.push(((TILE, TileKind::Vector), (false, Vec::new())));
        app.arrived.push(((TILE, TileKind::Raster), (false, Vec::new())));
        assert!(!app.decode_arrived(), "nothing left for another frame");
        assert!(!app.decoded.contains_key(&TILE), "a stale body is never decoded");
        assert!(!app.inflight.contains(&TILE) && !app.raster_pending.contains(&TILE));
        assert_eq!(app.stale_results_dropped, 2);
        assert!(app.should_request_vector(TILE), "coming back into view asks again");
        assert!(app.retries.is_empty(), "a discard is not a failure");
    }

    /// A body the camera still wants is decoded -- nearest first -- and so
    /// is one only the retained set (a stand-in) still holds.
    #[test]
    fn a_wanted_or_retained_body_is_decoded() {
        let mut app = app_at(1_000.0);
        let stand_in = TileId { z: 6, x: 45, y: 20 };
        let mut wanted = crate::fetch_queue::Wanted::new();
        wanted.add(crate::fetch_queue::FetchClass::Visible, &[(TILE, TileKind::Vector)]);
        app.wanted = wanted;
        app.retained.insert(stand_in);
        for t in [TILE, stand_in] {
            app.inflight.insert(t);
            app.arrived.push(((t, TileKind::Vector), (false, Vec::new())));
        }
        app.decode_arrived();
        // Two empty tiles decode in microseconds: both inside one budget.
        assert!(app.arrived.is_empty());
        assert!(app.decoded.contains_key(&TILE) && app.decoded.contains_key(&stand_in));
        assert!(app.inflight.is_empty());
        assert_eq!(app.stale_results_dropped, 0);
    }

    /// `collect_fetched`'s split: a body is buffered for `decode_arrived`,
    /// everything else is handled on the spot.
    #[test]
    fn arrivals_are_buffered_and_absences_handled_at_once() {
        let mut app = app_at(1_000.0);
        app.inflight.insert(TILE);
        app.handle_fetched(Fetched::Missing(TILE, TileKind::Vector));
        assert!(app.missing.contains(&TILE));
        assert!(app.arrived.is_empty(), "handle_fetched never buffers");
    }

    #[test]
    fn a_failed_imagery_tile_is_retried_too() {
        let mut app = app_at(1_000.0);
        app.raster_pending.insert(TILE);
        app.handle_fetched(Fetched::Failed(TILE, TileKind::Raster, crate::net::TileFailure::new("HTTP 503")));
        assert!(!app.raster_missing.contains(&TILE));
        assert!(!app.should_request_raster(TILE));
        app.now_override = Some(1_000.0 + crate::fetch_policy::MAX_BACKOFF_MS);
        assert!(app.should_request_raster(TILE));
    }
}

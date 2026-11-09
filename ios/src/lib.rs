//! `gpui-maps`' rendering engine, on iOS.
//!
//! # How this crate reaches the engine
//!
//! By SOURCE, not by dependency -- the technique the wasm spike established
//! (`web/src/lib.rs`), unchanged. Every `#[path]` below names a file in
//! `../../src/`: the shipped crate's own, unmodified. Nothing here is a
//! copy, a fork, or a reimplementation. Change `src/render/mesh.rs` and this
//! app's triangles change with it.
//!
//! The alternative -- `gpui-maps = { path = ".." }` with `gpui` behind a
//! feature -- fails for iOS the same way it failed for wasm, and for a
//! longer list of reasons: `gpui` itself, `tokio`'s `rt-multi-thread`,
//! `reqwest`, `pmtiles`' `mmap-async-tokio`, `glyphon`, and the whole
//! `objc2-app-kit` stack, which is macOS-only and is a hard compile error on
//! `aarch64-apple-ios-sim`. That is a workspace split, not a feature flag.
//!
//! The module set below started as exactly the closure the tessellator
//! needed, found by following `crate::` references outward from
//! `render::mesh`:
//!
//! ```text
//! render::mesh   -> render::run_split, style
//! style          -> style_spec, symbol_line, mvt, collision
//! style_spec     -> style, mvt, collision
//! symbol_line    -> collision, geo
//! render::gpu    -> render::{alloc, camera, mesh}, style, geo
//! render::camera -> geo, overlay::geom
//! ```
//!
//! It has grown twice since: once for the globe/space/atmosphere stack
//! (see `render/mod.rs`'s own comment), and once for labels and imagery,
//! which is what pulled `sprite` and `symbol_pass` in -- `render::label_plan`
//! names `symbol_pass`, and `render::glyph_atlas`/`render::icon_pipeline`
//! both name `sprite` (see `render/mod.rs` for the per-module reasoning).
//! `map_view`, `agent`, `tile_source` and the rest of `overlay` are still
//! NOT in it, and none of them is missed.
//!
//! # What the engine needed for iOS
//!
//! Almost nothing. `git diff master -- src/` was 0 bytes through the wasm
//! and tessellator spikes; the label/imagery task added exactly one
//! feature-gated `cfg`, widening `sprite.rs`'s `fetch_sprite_atlas`/
//! `fetch_bytes` from `not(target_arch = "wasm32")` to `all(not(target_arch
//! = "wasm32"), feature = "sprite_http_fetch")` -- those two functions are
//! the only thing in `sprite.rs` that names `reqwest`, which this crate
//! does not depend on (see `net.rs`), and this crate's own `Cargo.toml`
//! leaves the feature off (see that manifest's own comment on why a
//! feature, not `target_os = "ios"`). The two macOS-only blocks in
//! `render/alloc.rs`
//! (`rss_bytes`, `heap_in_use_bytes`) are already `#[cfg(target_os =
//! "macos")]`-gated and compile away to `0` here; nothing else in the
//! closure names a platform.

// The engine, verbatim. `lib.rs` is a mod-rs file, so a non-inline `#[path]`
// resolves against this directory -- `ios/src/` -- which is why every path
// starts `../../src/`.
// The Mapbox-fixture test macros, first so the vendored modules' tests below
// see them (they are not in the public tree; see that file).
#[cfg(test)]
#[macro_use]
#[path = "../../src/mapbox_fixtures.rs"]
mod mapbox_fixtures;
// The credit collection and compact rule, shared with native and iOS.
#[path = "../../src/attribution.rs"]
pub mod attribution;
#[path = "../../src/collision.rs"]
pub mod collision;
// Not on the tessellator's path, but on `style.rs`' and `style_spec`'s: a
// compiled symbol layer's `text-font` is a `font_fallback::FontStack`, and a
// layer that declares none gets `font_fallback::default_font_stack()`. Those
// two items are all the closure reaches -- everything else in the file (the
// cosmic-text `Fallback` table, `new_font_system`, `resolve_font_stack`) is
// compiled and unused here, because a FILE is the smallest unit a `#[path]`
// include can take.
//
// This is also where labels turned out to need a THIRD item:
// `new_font_system_from_db`, not `new_font_system`. `new_font_system` calls
// `fontdb::Database::load_system_fonts`, and fontdb has no iOS branch -- iOS
// is `unix` and not `macos`, so it takes the LINUX one
// (`/usr/share/fonts`, `/usr/local/share/fonts`, `$HOME/.fonts`), none of
// which exist in an app sandbox, and the database comes back EMPTY. That is
// the same shape of defect this module's own doc comment describes in
// cosmic-text's fallback table (picked by `cfg`, wrong on iOS), one layer
// further down. `ios/src/app.rs` avoids it the way `web/src/app.rs` does --
// `db.load_font_data` on bytes it already has, then
// `new_font_system_from_db` -- except the bytes are `include_bytes!` at
// compile time (`assets/fonts/NotoSans-Regular.latin-subset.ttf`, the same OFL
// asset the browser build fetches at runtime) rather than fetched, because
// this crate's shell has no async fetch path for anything but tile bytes
// (see `net.rs`) and a 71 KB subset costs nothing baked in.
#[path = "../../src/font_fallback.rs"]
pub mod font_fallback;
#[path = "../../src/geo.rs"]
pub mod geo;
#[path = "../../src/mvt.rs"]
pub mod mvt;
#[path = "../../src/projection.rs"]
pub mod projection;
#[path = "../../src/style.rs"]
pub mod style;
// A `mod.rs` reached by `#[path]` keeps its own directory for children, so
// `style_spec`'s five submodules resolve out of `src/style_spec/` with no
// further help.
#[path = "../../src/style_spec/mod.rs"]
pub mod style_spec;
#[path = "../../src/symbol_line.rs"]
pub mod symbol_line;
// `render::alloc`, included by this crate's own `render` list, takes its
// clock from here. iOS has a working `std::time::Instant` and does not need
// the wasm arm; it needs the MODULE, because the file it includes names it.
#[path = "../../src/timing.rs"]
pub mod timing;
// Added for this crate's label wiring: `render::glyph_atlas` and
// `render::icon_pipeline` both name `crate::sprite` (`sdf_from_mask`, and
// `SpriteAtlas`/`SpriteRect`/`slice_atlas` for `pack_icon_atlas`), and
// `symbol_pass::label_census` (unreached from here, but compiled -- a
// `#[path]` include takes the whole FILE) names it too. `sprite.rs`'s own
// `fetch_sprite_atlas`/`fetch_bytes` are `reqwest`-based and used to be the
// reason this crate could not take the file at all; they are now
// `#[cfg(all(not(target_arch = "wasm32"), feature = "sprite_http_fetch"))]`,
// and this crate's own `Cargo.toml` leaves that feature off (declared, not
// in any `default`) -- so this build never compiles them, whether it
// targets `aarch64-apple-ios` for real or, for a fast host-side sanity
// check, this Mac's own target. See that cfg's own comment in `sprite.rs`
// for why a feature and not `target_os = "ios"`. This crate builds a real
// sprite ATLAS now: the bundled `assets/japan-sprite@2x.{json,png}` sheet,
// unconditionally (`app::load_embedded_sprites`), and -- for the one style
// whose own `"sprite"` names Mapbox art the bundled sheet doesn't carry --
// Mapbox's own sheet, fetched at runtime over `net::Fetcher` and installed
// in its place (`app::MapApp::try_install_mapbox_sprites`). `sprite_urls`
// and `redact_access_token`, both `pub(crate)`, are what that fetch and its
// error path reuse from this file rather than re-deriving; they are no
// longer unused here. `#[allow(dead_code)]` stays for what genuinely is:
// `resolve_sprite_base` (the browser's own query-string resolution, which
// this crate has no query string to read) and `fetch_sprite_atlas`/
// `fetch_bytes`, which simply do not exist on this build at all (see the
// `cfg` above).
#[allow(dead_code)]
#[path = "../../src/sprite.rs"]
pub mod sprite;
// The place/merge/sort/clip/split step `render::label_plan` orchestrates,
// and `collect_candidates`, which this crate's `app.rs` calls to build one
// frame's `SymbolCandidate`/`DrawnSymbol` lists -- shared verbatim with
// native and the browser, per this file's own module doc. `#[allow(dead_code)]`:
// this 3,000+ line file also carries `label_census` (a debug-only
// instrumentation pass no shell calls from its drawing path) and the
// overlay-feature variant of candidate collection, neither of which this
// crate's minimal wiring reaches.
#[allow(dead_code)]
#[path = "../../src/symbol_pass.rs"]
pub mod symbol_pass;
// Imagery decode: `crate::raster::decode_tile`, the browser's own choice
// (`web/src/app.rs`'s `request_missing_raster_tiles`) over `image`'s
// generic `load_from_memory` for a byte-budget reason that applies here
// too. Zero network code of its own -- bytes in (from `net::Fetcher`,
// this crate's existing ureq pool), an `image::RgbaImage` out.
// `#[allow(dead_code)]`: this crate calls `decode_tile` and never the
// lower-level `decode` it wraps.
#[allow(dead_code)]
#[path = "../../src/raster.rs"]
pub mod raster;
// Imagery retention -- the working set and byte budget native's
// `raster_decoded` keeps by -- verbatim, so this shell's imagery cache is
// bounded by the same definition rather than a copy of it.
#[path = "../../src/raster_retention.rs"]
pub mod raster_retention;
// Two-pointer gesture math -- pinch, twist, and the pinch-or-tilt decision --
// shared with the browser shell, which has included it since the web touch
// work. Names nothing but floats and tuples.
#[path = "../../src/touch_gesture.rs"]
pub mod touch_gesture;
// The camera's rules, shared with native and the browser. See the module's
// own doc; `app.rs` moves onto it in the 3D camera plan's Task 6.
#[path = "../../src/camera_control.rs"]
pub mod camera_control;
// The story's shared, pure half, as the browser includes it: the script
// (S1), the camera path (S2) with its `fly` and `tracks` siblings, and the
// timeline (S3). No clock, no gpui, no wgpu; `style_cache.rs`, which
// compiles, stays native. An inline module, because `camera_path.rs`
// names its siblings as `super::...`. Inside an inline module a child's
// `#[path]` resolves from `ios/src/story/`, a directory that does not
// exist; `#[path = "."]` on the inline module points its children back at
// `ios/src/`, so they use the same `../../` as every include above.
#[allow(dead_code)]
#[path = "."]
pub mod story {
    #[path = "../../src/story/tiers.rs"]
    pub mod tiers;
    #[path = "../../src/story/script.rs"]
    pub mod script;
    #[path = "../../src/story/tracks.rs"]
    pub mod tracks;
    #[path = "../../src/story/camera_path.rs"]
    pub mod camera_path;
    #[path = "../../src/story/chapters.rs"]
    pub mod chapters;
    #[path = "../../src/story/design.rs"]
    pub mod design;
    #[path = "../../src/story/captions.rs"]
    pub mod captions;
    #[path = "../../src/story/fly.rs"]
    pub mod fly;
    #[path = "../../src/story/timeline.rs"]
    pub mod timeline;
    #[path = "../../src/story/effects.rs"]
    pub mod effects;
    #[path = "../../src/story/overlay_layers.rs"]
    pub mod overlay_layers;
    #[path = "../../src/story/panel.rs"]
    pub mod panel;
    #[path = "../../src/story/layout.rs"]
    pub mod layout;
    #[path = "../../src/story/callout.rs"]
    pub mod callout;
    #[path = "../../src/story/furniture.rs"]
    pub mod furniture;
    // The design system's card: its layout and its timing (spec §3, R6).
    #[path = "../../src/story/card_layout.rs"]
    pub mod card_layout;
    #[path = "../../src/story/frame_layout.rs"]
    pub mod frame_layout;
    #[path = "../../src/story/cards.rs"]
    pub mod cards;
}
// The corner chrome's insets (top corners, bottom row); see its module doc.
#[path = "../../src/chrome.rs"]
pub mod chrome;
// The compass button's rules; see its module doc.
#[path = "../../src/compass.rs"]
pub mod compass;
// The SWAT mark, bottom-left on every map (the 2026-09-29 plain-default
// ruling); drawn through the icon pipeline like the compass.
#[path = "../../src/brand_mark.rs"]
pub mod brand_mark;
// Turn-by-turn navigation; see the module doc.
#[path = "../../src/nav/mod.rs"]
pub mod nav;
// The turn-by-turn speaker (`AVSpeechSynthesizer` + the ducking audio
// session), shared with native macOS; see its module doc. Behind `speech`
// (on by default): it links AVFAudio, which an embedding app that draws a
// map and never navigates must not be made to link.
#[cfg(feature = "speech")]
#[path = "../../src/speech.rs"]
pub mod speech;
/// Silent stand-in for the speaker when `speech` is off: the same two items
/// the shell calls, saying nothing.
#[cfg(not(feature = "speech"))]
pub mod speech {
    #[derive(Default)]
    pub struct SpeechOutput {
        _silent: (),
    }

    impl SpeechOutput {
        pub fn frame(&mut self, _command: Option<crate::nav::host::VoiceCommand>) {}
    }
}
// Absent-or-retry for a tile request that brought no data, and the backoff
// between retries; shared with the browser. Clock-free: time is a parameter.
#[path = "../../src/fetch_policy.rs"]
pub mod fetch_policy;
// Which pending tile request a worker takes next, and which the camera no
// longer wants; shared with the browser. Clock-free.
#[path = "../../src/fetch_queue.rs"]
pub mod fetch_queue;
// The active vector source's real-data zoom ceiling, from its schema/URL;
// shared with the browser so the two cannot answer the question two
// different ways. See the module doc.
#[path = "../../src/source_zoom.rs"]
pub mod source_zoom;
// Which tile source and style a shell starts on, chosen together, and the
// one derivation of the Mapbox `composite` tile URL; shared with native.
// Names only `serde_json`, `anyhow` and (in its tests) `sprite`.
#[path = "../../src/basemap.rs"]
pub mod basemap;

// These two are the only files this crate adds to the engine's own tree, and
// both are module lists rather than logic -- see their doc comments.
// The whole overlay module, verbatim: the app draws caller overlays through
// `overlay::host::ShellOverlays`, built on the same `OverlayHost`/`OverlayGpu`
// `map_view` drives. `dead_code`: the consumer-facing doors this app does
// not call (live diffs, highlights, column editors) are alive natively.
#[allow(dead_code, unused_imports)]
#[path = "../../src/overlay/mod.rs"]
pub mod overlay;
// Flow bundling (`flow::od`, edge-path and density bundling, places):
// pure CPU, no platform. Studio for iOS bundles OD flows with it exactly as
// the desktop Explorer does (`gpui-map-agent-kit/src/flow_glow.rs`).
#[allow(dead_code)]
#[path = "../../src/flow/mod.rs"]
pub mod flow;
// `overlay::host`'s tile cache: `TileCache` alone, not the native-only
// fetch half of `tile_source`.
#[allow(dead_code)]
#[path = "../../src/tile_source/cache.rs"]
pub mod tile_cache;
pub mod tile_source {
    pub use crate::tile_cache::TileCache;
    #[cfg(feature = "cog_ureq")]
    pub use crate::cog;
}
// Cloud-Optimised GeoTIFF, verbatim (`tile_source::cog` above), as `web/`
// includes it: the format core is shared and `fetch_ureq` reads by this
// crate's `ureq`. At the crate root because a nested `#[path]` would
// resolve through `ios/src/tile_source/`, which does not exist.
#[cfg(feature = "cog_ureq")]
#[allow(dead_code)]
#[path = "../../src/tile_source/cog/mod.rs"]
pub mod cog;
// Byte-range coalescing and bounded concurrency, verbatim: the COG,
// GeoParquet and FlatGeobuf readers' shared request planner.
#[allow(dead_code)]
#[path = "../../src/range_fetch.rs"]
pub mod range_fetch;
pub mod render;
// Satellites: CelesTrak OMM parsing, the drawn catalogue and SGP4
// propagation, verbatim (behind `orbits`, on the pure-Rust `sgp4`).
#[cfg(feature = "orbits")]
#[allow(dead_code)]
#[path = "../../src/orbit/mod.rs"]
pub mod orbit;
/// The desktop's `gpui_maps::map_view` altitude names, so code shared with
/// the desktop by `#[path]` (`demos/satellites/model.rs`) resolves here.
pub mod map_view {
    pub use crate::render::altitude_layers::{AltitudePointState, AltitudePositions, SceneClock};
}

/// The shell's log: on, with positions, for the standalone demo; OFF and
/// position-free for an embedding app (`app::host::Host`), where the camera
/// follows a passenger and a coordinate in the device log is their location.
///
/// Every shell line goes through [`mlog!`]. Lines that would carry a place --
/// the camera centre, a tile's z/x/y, a touch point, a URL (tile URLs carry
/// z/x/y) -- format it through [`log::position`], [`log::tile`],
/// [`log::point`] or [`log::scrub`], which print it only while
/// [`log::positions`] is on. The embed turns that off for good.
pub mod log {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    // On for the standalone demo, off for an embed (no winit shell).
    static ENABLED: AtomicBool = AtomicBool::new(cfg!(feature = "winit-shell"));
    static POSITIONS: AtomicBool = AtomicBool::new(cfg!(feature = "winit-shell"));
    /// A test's capture of every emitted line.
    static CAPTURE: Mutex<Option<Vec<String>>> = Mutex::new(None);

    /// Lines worth reading even when logging is turned on only after they
    /// happened -- the renderer's build timings, which an embedding app's
    /// `MapEngine::new` produces before it can call `set_debug_logging`.
    /// The last few, emitted when logging is next turned on.
    static DEFERRED: Mutex<Vec<String>> = Mutex::new(Vec::new());
    const DEFERRED_CAP: usize = 8;

    pub fn set_enabled(on: bool) {
        ENABLED.store(on, Ordering::Relaxed);
        if on {
            let held = std::mem::take(&mut *DEFERRED.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
            for line in held {
                emit(line);
            }
        }
    }

    /// `line` now if logging is on, else held for when it is turned on.
    pub fn emit_or_defer(line: String) {
        if enabled() {
            emit(line);
            return;
        }
        let mut held = DEFERRED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if held.len() == DEFERRED_CAP {
            held.remove(0);
        }
        held.push(line);
    }

    pub fn enabled() -> bool {
        ENABLED.load(Ordering::Relaxed)
    }

    pub fn set_positions(on: bool) {
        POSITIONS.store(on, Ordering::Relaxed);
    }

    pub fn positions() -> bool {
        POSITIONS.load(Ordering::Relaxed)
    }

    /// Starts (`true`) or stops capturing emitted lines; stopping returns
    /// what was captured. For tests.
    pub fn capture(on: bool) -> Vec<String> {
        let mut capture = CAPTURE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let captured = capture.take().unwrap_or_default();
        if on {
            *capture = Some(Vec::new());
        }
        captured
    }

    #[doc(hidden)]
    pub fn emit(line: String) {
        if let Some(lines) = CAPTURE.lock().unwrap_or_else(std::sync::PoisonError::into_inner).as_mut() {
            lines.push(line.clone());
        }
        println!("{line}");
    }

    /// `lat,lon`, or `-` with positions off.
    pub fn position(lat: f64, lon: f64) -> String {
        if positions() {
            format!("{lat:.4},{lon:.4}")
        } else {
            "-".into()
        }
    }

    /// A tile's `z/x/y`, or only its zoom with positions off.
    pub fn tile(tile: crate::geo::TileId) -> String {
        if positions() {
            format!("{}/{}/{}", tile.z, tile.x, tile.y)
        } else {
            format!("z{}", tile.z)
        }
    }

    /// A screen point, or `-` with positions off.
    pub fn point(x: f64, y: f64) -> String {
        if positions() {
            format!("{x:.0},{y:.0}")
        } else {
            "-".into()
        }
    }

    /// `text` with every URL replaced by `<url>` while positions are off.
    pub fn scrub(text: &str) -> String {
        if positions() {
            return text.to_string();
        }
        text.split(' ')
            .map(|word| if word.contains("://") { "<url>" } else { word })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// `println!`, through the shell's [`log`] switch.
#[macro_export]
macro_rules! mlog {
    ($($arg:tt)*) => {
        if $crate::log::enabled() {
            $crate::log::emit(format!($($arg)*));
        }
    };
}

pub mod app;
pub mod disk_cache;
pub mod display_link;
pub mod dots;
pub mod net;
pub mod pmtiles_http;
#[cfg(feature = "cog_ureq")]
pub mod cog_http;
pub mod styles;
pub mod system_fonts;

// First, so its test macros are in scope in every module below.
#[cfg(test)]
#[macro_use]
mod mapbox_fixtures;
pub mod agent;
pub mod attribution;
/// Which tile source and style a shell starts on, chosen together, and the
/// one derivation of the Mapbox `composite` tile URL -- shared by native,
/// iOS and the browser. See the module doc.
pub mod basemap;
/// Keyless basemaps over OpenFreeMap: `basemaps::load(Basemap::StreetsLight)`.
pub mod basemaps;
pub mod brand_mark;
/// The US census income demo's shared logic: the `BGPK` pack the browser
/// reads, the income ramp, the popup's names and the hover rule. Shared by
/// the native demo and the browser shell (`#[path]`), tested here.
#[doc(hidden)]
pub mod census;
// A real frame, headless, to a file: the thing `agent.rs`' deferred
// `map.capture` was never able to give an investigation. Native-only by
// construction (it reads tiles off disk and writes a PNG), and named by
// neither `web/src/lib.rs` nor `ios/src/lib.rs`, whose module lists are the
// tessellator's closure and nothing else.
pub mod capture;
pub mod collision;
pub mod font_fallback;
pub mod symbol_pass;
pub mod geo;
/// The story image cache's disk side (`story::media`).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod image_cache;
pub mod map_view;
/// Off macOS: the map's frames shown as images in gpui's scene.
#[cfg(not(target_os = "macos"))]
pub(crate) mod offscreen_presenter;
pub mod mvt;
pub mod projection;
/// Satellites: CelesTrak OMM parsing, the drawn catalogue and SGP4
/// propagation (spec 2026-09-29-satellites-design.md). Behind `orbits`.
#[cfg(feature = "orbits")]
pub mod orbit;
/// glTF / GLB files as model-layer meshes (`--features models`).
#[cfg(feature = "models")]
pub mod model_gltf;
/// Origin-destination flows: trip rows aggregated into corridors on a
/// hexagonal lattice, Edge-Path Bundling, and load banding. Renderer-agnostic.
pub mod flow;
pub mod overlay;
/// Raster tiles: PNG/JPEG bytes into RGBA pixels. The imagery counterpart
/// of `mvt`, and like it a pure decode -- no network, no device -- which
/// is what lets its tests assert on exact pixel values.
pub mod raster;
/// Imagery retention -- the working set and byte budget every shell's
/// decoded-imagery cache obeys. `#[path]`-included by the browser and phone
/// shells; see the module doc.
pub mod raster_retention;
/// Byte-range reads as few, concurrent requests (COG, GeoParquet,
/// FlatGeobuf). See the module doc.
pub mod range_fetch;
pub mod render;
/// The zoom ceiling a vector tile source's real data goes to, as a function
/// of which source is active rather than a shell constant -- shared by
/// native, iOS and the browser for the same reason `camera_control` is:
/// gpui-free and clock-free, so the browser shell (which cannot run tests)
/// shares the same tested logic instead of guessing its own. See the module
/// doc for the defect this exists to close.
pub mod source_zoom;
// The space look's textures: fetched on first use into
// `~/.cache/gpui-map/space/`, verified by pinned sha256. Native only.
pub mod space_assets;
pub mod sprite;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod sprite_cache;
#[cfg(not(target_arch = "wasm32"))]
pub use sprite_cache::cached_sprite_atlas;
pub mod style;
pub mod style_spec;
pub mod story;
pub mod symbol_line;
pub mod tile_source;
/// Pure gesture math (pinch distance/midpoint/zoom-delta) for the browser
/// shell's multitouch input. Lives here rather than in `web/src/app.rs`
/// purely so `cargo test` actually runs its unit tests -- see the module's
/// own doc comment.
pub mod touch_gesture;
/// Trackpad pinch (native magnify, browser ctrl-wheel, Safari gesture
/// events) as zoom levels that track the fingers 1:1. Shared with `web/`.
pub mod trackpad_pinch;
/// Every rule that changes the camera -- limits, wrapping, north snap,
/// easing, inertia, programmatic moves -- shared by native, iOS and the
/// browser so that no shell can decide one of them for itself. gpui-free
/// and clock-free for the same reason as `touch_gesture`: the browser
/// shell cannot run tests, so this is where they run.
pub mod camera_control;
/// The corner chrome's insets: `CORNER_INSET_PX` for the top corners (the
/// compass, the navigation chrome, cards) and `BOTTOM_ROW_INSET_PX` for the
/// bottom row the brand mark and the attribution plate share, on one centre
/// line. gpui-free, so every shell compiles it by `#[path]`.
pub mod chrome;
/// The compass button's rules: geometry, visibility, needle rotation,
/// artwork and hit test, decided once here so no shell computes any of
/// them. gpui-free, like `camera_control`, so the browser shell (which
/// cannot run tests) shares the same tested logic.
pub mod compass;
/// Turn-by-turn navigation over Ferrostar: route parsing, turn text, the
/// trip session and reroute policy, the simulated drive, the follow camera,
/// the route layer and the chrome's layout. gpui-free, clock-free and
/// I/O-free, so the browser and phone shells compile the same files. See the
/// module doc.
pub mod nav;
/// The turn-by-turn speaker: `AVSpeechSynthesizer` on Apple platforms, a
/// no-op elsewhere. Shared with the iOS shell by `#[path]`. Not navigation
/// (that is `nav::voice`), so it lives outside `src/nav`.
pub mod speech;
/// Absent-or-retry for a tile request that brought back no data, and the
/// capped exponential backoff between retries. Clock-free (time is a
/// parameter) so the browser and phone shells share it and it is tested
/// here. See the module doc.
pub mod fetch_policy;
/// Which pending tile request a fetch worker takes next, and which pending
/// requests the camera no longer wants: a priority queue with
/// drop-on-retarget, shared by the phone and browser shells. See the
/// module doc.
pub mod fetch_queue;
/// The one monotonic clock. `std::time::Instant::now()` compiles for
/// `wasm32-unknown-unknown` and panics when it is called, so every engine
/// module a browser or a phone compiles goes through here instead.
pub mod timing;

// The `geocode` feature is a RESERVED NAME with nothing behind it: the crate
// it used to enable now lives in its own repository (see the feature's
// comment in `Cargo.toml` for the wiring, which is two lines).
//
// This is here rather than nowhere because the alternative is silent. Cargo
// has no spelling for "optional dependency, default off, source absent", so
// the feature is declared empty -- and an empty feature ACCEPTS
// `--features geocode`, links nothing, and leaves the caller believing they
// have address resolution. A caller who asks for a geocoder and gets a map
// that cannot geocode should be told so by the compiler, at the moment they
// ask, and not discover it at the first query.
#[cfg(feature = "geocode")]
compile_error!(
    "the `geocode` feature is reserved but not wired up: the `geocode` crate was split out of \
     this repository and is no longer a path dependency. To use it, add it back as a git or \
     path dependency and restore `geocode = [\"dep:geocode\"]` -- see the `geocode` feature's \
     comment in Cargo.toml. A default build does not need this feature; nothing in `src/` \
     references the crate."
);

//! `gpui-maps`' rendering engine, in a browser.
//!
//! # How this crate reaches the engine
//!
//! By SOURCE, not by dependency. Every `#[path]` below names a file in
//! `../../src/` -- the shipped crate's own, unmodified. Nothing here is a
//! copy, a fork, or a reimplementation: change `src/render/mesh.rs` and this
//! page's triangles change with it.
//!
//! The alternative -- `gpui-maps = { path = ".." }` with `gpui` behind a
//! feature -- was measured and rejected for this phase. Making `gpui`
//! optional is genuinely small (two files use it as code: `map_view.rs` and
//! `render/macos_layer.rs`), but `gpui` is not the only
//! thing in that `Cargo.toml` that cannot cross: `tokio`'s
//! `rt-multi-thread`, `reqwest`, `pmtiles`' `mmap-async-tokio`, `glyphon`,
//! `image` and the whole `objc2` stack would all still have to become
//! optional, and every one of them is load-bearing for the native build.
//! That is phase 2's job, and it is a workspace split, not a feature flag.
//!
//! The module set below is exactly the closure the tessellator needs, found
//! by following `crate::` references outward from `render::mesh`:
//!
//! ```text
//! render::mesh -> render::run_split, style
//! style        -> style_spec, symbol_line, mvt, collision
//! style_spec   -> style, mvt, collision
//! symbol_line  -> collision, geo
//! render::gpu  -> render::{alloc, camera, mesh}, style, geo
//! render::camera -> geo, overlay::geom
//! symbol_pass  -> collision, font_fallback, geo, mvt, style,
//!                 overlay::paint, render::{camera, icon_pipeline}
//! ```
//!
//! `symbol_pass` -- the label half -- joined that closure when it moved out
//! of `map_view.rs`. It is why this page has text: everything between a
//! baked `style::SymbolFeature` and a `glyphon::TextArea` is now in a file
//! a browser can include, and `app::draw_labels` calls it rather than
//! reimplementing it.
//!
//! `map_view`, `agent`, `tile_source` and the rest of `overlay` are NOT in
//! it, and none of them is missed.

// The engine, verbatim. `lib.rs` is a mod-rs file, so a non-inline `#[path]`
// resolves against this directory -- `web/src/` -- which is why every path
// starts `../../src/`.
//
// # Why three of these carry `#[allow(dead_code)]`
//
// Because this crate includes whole engine FILES and uses a subset of each.
// `plate_pipeline` draws the native attribution plate, `debug_pipeline` the
// tile overlay, `icon_fit` the nine-patch fitter, most of `alloc`'s writers
// feed `map_view`'s counters -- none of that is reachable from a page whose
// entry point is `app.rs`, and all of it is perfectly alive in the shipped
// crate. Measured: 50 `dead_code` warnings, 17 from `symbol_pass`, 1 from
// `sprite`, 32 from under `render` (`alloc` 15, `icon_fit` 6,
// `plate_pipeline` 5, `debug_pipeline` 3, and one each in `mesh`,
// `glyph_atlas` and `camera`), and ZERO from this crate's own `lib.rs`,
// `app.rs` or `overlay/mod.rs` -- so the allow goes on the three module
// declarations that account for all 50 rather than on the crate, and the
// hand-written files stay under the lint.
//
// Getting to zero warnings is not tidiness: `scripts/check-wasm.sh` runs
// with `-D warnings`, so a NEW warning -- an unused import from a renamed
// re-export, say -- is a build failure rather than line 51 of 50.
// The credit collection and compact rule, shared with native and iOS.
#[path = "../../src/attribution.rs"]
pub mod attribution;
#[path = "../../src/collision.rs"]
pub mod collision;
#[path = "../../src/geo.rs"]
pub mod geo;
#[path = "../../src/mvt.rs"]
pub mod mvt;
#[path = "../../src/projection.rs"]
pub mod projection;
// The raster half of the tessellator's closure that this crate never used to
// need: bytes-to-`image::RgbaImage` decode for imagery tiles. Named directly
// here, alongside `mvt` -- not reached through `render/mod.rs`'s `#[path]`
// below -- because it names only `image`, nothing under `render`, and
// `app::request_missing_raster_tiles` calls `crate::raster::decode_tile`
// exactly the way it calls `crate::mvt::decode`.
#[path = "../../src/raster.rs"]
pub mod raster;
// Imagery retention -- the working set and byte budget native's
// `raster_decoded` keeps by -- verbatim, so this shell's imagery cache is
// bounded by the same definition rather than a copy of it.
#[path = "../../src/raster_retention.rs"]
pub mod raster_retention;
#[path = "../../src/style.rs"]
pub mod style;
// A `mod.rs` reached by `#[path]` keeps its own directory for children, so
// `style_spec`'s five submodules resolve out of `src/style_spec/` with no
// further help.
#[path = "../../src/style_spec/mod.rs"]
pub mod style_spec;
#[path = "../../src/symbol_line.rs"]
pub mod symbol_line;
// The one monotonic clock, and the reason this page renders at all.
// `std::time::Instant::now()` compiles for this target and PANICS when it
// is called -- see `src/timing.rs` for what that cost -- so `symbol_pass`
// and `render::alloc`, both included below, reach the clock through here.
#[path = "../../src/timing.rs"]
pub mod timing;
// Pinch distance/midpoint/zoom-delta math, used by `app::install_input_handlers`.
// Names nothing but floats and tuples, so it needed no `#[allow(dead_code)]`
// treatment and no wasm-specific rewrite to cross.
#[path = "../../src/touch_gesture.rs"]
pub mod touch_gesture;
// Trackpad pinch (ctrl-wheel, Safari gesture events) as zoom levels.
#[path = "../../src/trackpad_pinch.rs"]
pub mod trackpad_pinch;
// The camera's rules, shared with native and iOS: see the module's own doc.
// Names `render::Camera`, `geo` and `style_spec`'s easing, all already here.
#[path = "../../src/camera_control.rs"]
pub mod camera_control;
// The story's shared, pure half: the script (types, parse, structural
// validation, S1), the camera path (pose as a function of story time,
// S2) and the timeline (scene state as a function of story time, S3;
// `style_cache.rs`, which compiles, stays native). Reads no clock, no gpui, no wgpu; #[allow(dead_code)] like its
// neighbours, since the player itself is native-only. An inline module,
// because `camera_path.rs` names its siblings as `super::script` and
// `super::tracks`. Inside an inline module a child's `#[path]` resolves
// from `web/src/story/`, a directory that does not exist, so `../` out of
// it fails. `#[path = "."]` on the inline module points its children back
// at `web/src/`, so they use the same `../../` as every include above --
// and `tests/wasm_portability.rs`, which parses these lines, finds them.
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
// Turn-by-turn navigation: the route, the session, the chrome's layout. A
// `mod.rs` reached by `#[path]` keeps its own directory for children, so
// every `pub mod` in it resolves out of `src/nav/`.
#[path = "../../src/nav/mod.rs"]
pub mod nav;
// Absent-or-retry for a tile request that brought no data, and the backoff
// between retries; shared with iOS. Clock-free: time is a parameter.
#[path = "../../src/fetch_policy.rs"]
pub mod fetch_policy;
// Which pending tile request goes out next, and which the camera no longer
// wants; shared with iOS. Clock-free.
#[path = "../../src/fetch_queue.rs"]
pub mod fetch_queue;
// The census demo's shared logic (`?census=`): the `BGPK` pack decoder, the
// income ramp, the popup's words and the hover rule, tested in the engine
// crate and driven by `app::census_page`.
#[path = "../../src/census/mod.rs"]
pub mod census;
// The active vector source's real-data zoom ceiling, from its schema/URL;
// shared with iOS so the two cannot answer the question two different ways.
// See the module doc.
#[path = "../../src/source_zoom.rs"]
pub mod source_zoom;
// Which tile source and style a shell starts on, chosen together, and the
// one derivation of the Mapbox `composite` tile URL; shared with native.
// Names only `serde_json`, `anyhow` and (in its tests) `sprite`.
#[path = "../../src/basemap.rs"]
pub mod basemap;

#[allow(dead_code)] // native-only halves of this file; see the note above
#[path = "../../src/symbol_pass.rs"]
pub mod symbol_pass;
// The SWAT mark, bottom-left on every map (the 2026-09-29 plain-default
// ruling). Here it is a DOM <img>, like the compass: `App::sync_brand_mark_dom`.
#[path = "../../src/brand_mark.rs"]
#[allow(dead_code)] // the GPU-quad half (decode, instance, atlas index) is native/iOS-only
pub mod brand_mark;
// `symbol_pass` reaches all three of these. `sprite` is pure `HashMap`
// arithmetic; `render::icon_pipeline` (which owns `IconRect`,
// `IconInstance` and `SdfPaint`) and `overlay::paint` (which owns
// `TextJustify`, half of a `LabelKey`) name nothing but `wgpu`, `image`,
// `serde_json` and this crate. The earlier note claiming the icon pipeline
// "needs the sprite atlas and glyphon" was wrong on both counts -- it names
// neither -- and cost this page its labels for a release.
#[allow(dead_code)] // native-only halves of this file; see the note above
#[path = "../../src/sprite.rs"]
pub mod sprite;
// `font_fallback` is the SHIPPED file now, not a restatement of two of its
// items. What used to make it uncrossable was `db.load_system_fonts()`, and
// that is a `new_font_system` problem rather than a file problem: the rest
// of it -- `MapLabelFallback`, `script_fallback_families`,
// `resolve_font_stack`, `ResolvedFace` -- is the per-script fallback table
// this page needs MORE than the native build does, because a browser's font
// set is one face rather than a thousand.
#[path = "../../src/font_fallback.rs"]
pub mod font_fallback;
// The whole overlay module, verbatim: the page draws caller GeoJSON and
// clustered points through `overlay::host::ShellOverlays`, built on the same
// `OverlayHost`/`OverlayGpu` `map_view` drives. A `mod.rs` reached by
// `#[path]` keeps its own directory for children. `dead_code`: the page
// drives the host, not every consumer-facing door of the store beside it
// (`live`, `hit`'s highlight half, the column editors) -- all alive natively.
#[allow(dead_code, unused_imports)]
#[path = "../../src/overlay/mod.rs"]
pub mod overlay;
// The overlay tile cache's `TileCache`, and nothing else of `tile_source`
// (whose HTTP and PMTiles halves are native-only). `overlay::host` names it
// as `crate::tile_source::TileCache`, so the module is re-created here with
// that one item.
#[allow(dead_code)]
#[path = "../../src/tile_source/cache.rs"]
pub mod tile_cache;
pub mod tile_source {
    pub use crate::tile_cache::TileCache;
    pub use crate::cog;
}
// Cloud-Optimised GeoTIFF, verbatim (`tile_source::cog` above): the format
// core is shared and `fetch_web` reads by `fetch` range requests. At the
// crate root because a nested `#[path]` would resolve through
// `web/src/tile_source/`, which does not exist.
#[path = "../../src/tile_source/cog/mod.rs"]
pub mod cog;
// Byte-range coalescing and bounded concurrency, verbatim: the COG,
// GeoParquet and FlatGeobuf readers' shared request planner.
#[allow(dead_code)]
#[path = "../../src/range_fetch.rs"]
pub mod range_fetch;

/// Opens the COG at `url` by range request and renders map tile
/// `z/x/y` (512 px): `"<summary> | <opaque pixels> px drawn | <stats>"`.
/// The browser's own check that a COG range-reads and decodes here, from
/// the console: `await wasm.cogTile(url, 10, 165, 360)`.
#[wasm_bindgen::prelude::wasm_bindgen(js_name = cogTile)]
pub async fn cog_tile(url: String, z: u8, x: u32, y: u32) -> Result<String, wasm_bindgen::JsValue> {
    use tile_source::cog::{CogOptions, CogSource};
    let fail = |e: tile_source::cog::CogError| wasm_bindgen::JsValue::from_str(&e.to_string());
    let cog = CogSource::open(&url, CogOptions::default().with_hillshade(1.0)).await.map_err(fail)?;
    let image = cog.render_tile(geo::TileId { z, x, y }).await.map_err(fail)?;
    let drawn = image.map_or(0, |i| i.pixels().filter(|p| p.0[3] > 0).count());
    Ok(format!("{} | {drawn} px drawn | {:?}", cog.metadata().summary(), cog.stats()))
}
// `render` is NOT a shim, and that is the point of this line.
//
// It used to be `web/src/render/mod.rs`: a hand-written 56-line file that
// `#[path]`-included the same shipped sources but restated the module list
// and the re-export list in its own words. Two lists, nothing proving they
// agreed, and every engine change that added a `render::` item broke this
// build silently -- four times (the JS SDK spike found 18 errors, globe
// Task 2 left 23, globe Task 6 held it at 23, and `29453cd` shipped with
// 17). Nine of those 17 were nothing but names missing from the restated
// list: `vec_block_bytes`, `heap_block_bytes`, `hashmap_table_bytes`,
// `GlyphKey`, `GlyphAtlas`, `glyph_font_scale`.
//
// So there is one list now, and it is the engine's. Everything in
// `src/render/` compiles for `wasm32-unknown-unknown` unchanged --
// `debug_pipeline`, `debug_tiles`, `coverage`, `glyph_atlas` and
// `icon_fit`, all of which the old shim dropped, name only `wgpu`,
// `serde_json`, `crate::geo`, `crate::sprite`, `crate::overlay::geom` and
// each other. The one file that genuinely cannot cross, `macos_layer`, was
// already `#[cfg(target_os = "macos")]` in that list and needed no help:
// `wasm32-unknown-unknown` has `target_os = "unknown"`.
//
// Carrying the five extra modules costs the bundle 163 BYTES. Measured, both
// arms built by `web/build.sh` on the same commit -- the shared list gives a
// `gpui_maps_web_bg.wasm` of 7,021,593 B, a control arm restating the old
// shim's smaller module set (repaired so it compiles) gives 7,021,430 B.
// 0.002%, which is what `lto = true` plus `--gc-sections` does to code
// nothing reaches: the modules are compiled and then thrown away. So the
// trade the old shim was making -- a hand-maintained list, to keep unreached
// code out of the bundle -- was not buying anything.
//
// A `mod.rs` reached by `#[path]` keeps its own directory for children, so
// every `pub mod` inside it resolves out of `src/render/` -- the same
// mechanism `style_spec` above relies on.
#[allow(dead_code)] // native-only halves of this tree; see the note above
#[path = "../../src/render/mod.rs"]
pub mod render;

pub mod app;

//! The engine's `render` module, minus everything that cannot reach iOS.
//!
//! Every `#[path]` below points at the SHIPPED source in `../../../src/` --
//! this file exists only to choose which of its submodules exist here, and
//! to forward the names `gpu.rs`, `mesh.rs`, `style.rs` and `mvt.rs` reach
//! for by their `crate::render::` prefix. Nothing here is a copy, a fork or
//! a reimplementation.
//!
//! # Why the re-exports below are globs, and used to be a hand-written list
//!
//! Because the hand-written list drifted, and the drift was invisible.
//! Measured at `29453cd` (v0.6.2): `cd ios && cargo check --target
//! aarch64-apple-ios` gave ELEVEN errors, and all eleven were one cause --
//! the engine's `render/mod.rs` had gained `vec_block_bytes`,
//! `heap_block_bytes` and `hashmap_table_bytes`, this file had not, and
//! `src/mvt.rs`, `src/style.rs` and `src/render/mesh.rs` all name them
//! through `crate::render::`. The browser build had the same three names
//! missing from ITS restatement on the same day, for the same reason: a
//! second hand-maintained list, and nothing that reads both.
//!
//! A glob per included module ends that whole class. `pub use alloc::*`
//! forwards every item `alloc.rs` exposes, the `pub(crate)` ones included
//! -- verified rather than assumed: `crate::render::note_paint_entered` and
//! `crate::render::pitch_scale_factor_at_ground` are both `pub(crate)` in
//! the engine and both resolve through these globs. So an item added to a
//! module this file already includes arrives here with nothing to remember,
//! and a name the engine RENAMES arrives renamed, next to the call sites
//! that renamed with it.
//!
//! The browser crate solved the same problem by deleting its shim and
//! `#[path]`-including `src/render/mod.rs` whole. That does not reach here,
//! for the reason `macos_layer` (AppKit) and `coverage`/`debug_pipeline`/
//! `debug_tiles` (native's own debug overlays) give in the `DROPPED:` lines
//! below -- but the list of what does not cross is now shorter than it was.
//! `glyph_atlas` and `icon_pipeline` used to sit beside them: both name
//! `crate::sprite`, and `sprite.rs` used to reach `reqwest` on everything
//! that is not wasm -- a dependency this crate exists to avoid (see
//! `net.rs`, which does its HTTP on `ureq` and threads instead). `sprite.rs`
//! now excludes iOS from that same `cfg` alongside wasm (see its own doc
//! comment), which is what let this crate's label wiring bring in
//! `glyph_atlas`, `icon_pipeline`, `label_plan` and (at the crate root)
//! `symbol_pass` and `sprite` itself -- see `ios/src/lib.rs`. A glob keeps
//! the one-list property for every module that crosses, without buying a
//! dependency this crate does not use or porting the debug-only passes that
//! have never drawn a pixel on a phone.
//!
//! # What a glob does not cover, and what guards it
//!
//! A new FILE. If the engine grows `src/render/foo.rs` and something in
//! this crate's closure calls `crate::render::foo::bar`, the `mod` list
//! below has to grow too. That half is guarded twice:
//!
//!   * `tests/ios_shell_stays_in_step.rs`, in the shipped crate, fails
//!     under a plain `cargo test` the moment the engine's module list and
//!     this one disagree in a way the `DROPPED:` lines below do not account
//!     for. It needs no iOS toolchain.
//!   * `scripts/check-ios.sh`, from the repository root, is the only
//!     command anywhere that actually COMPILES this crate -- `ios/` is its
//!     own cargo workspace, so `cargo check`/`test`/`clippy` from the root
//!     never have.
//!
//! # The modules this crate does not take
//!
//! The lines below are read by that test, one per engine `render` submodule
//! that is deliberately absent. Keep the `DROPPED: <name> -- <reason>`
//! shape; the reason is the point of the line.
//!
//! DROPPED: coverage -- the tile-coverage debug overlay; nothing in this crate's closure parses a coverage string.
//! DROPPED: debug_pipeline -- the debug-line pass; this crate draws no debug geometry.
//! DROPPED: debug_tiles -- the tile-status debug vocabulary, which only `map_view` drives, and `map_view` is not in the closure.
//! DROPPED: mask_timer -- the dot mask pass's GPU timer, a `probe`-feature diagnostic (`world_dots`); the iOS shell builds without `probe`.
//! DROPPED: zoom_probe -- the `probe`-feature zoom-continuity log; its hooks live in `map_view`, which is not in the closure.
//! DROPPED: macos_layer -- AppKit's `NSView`, and gpui. Dropped because it is *AppKit*, not because it is Metal: `gpu.rs` runs on Metal here exactly as it does on the desktop.
//! DROPPED: marker_pipeline -- the story-marker disc pass; only `map_view` and `capture` (story overlays) draw story markers, and neither is in the closure.
//!
//! `glyph_atlas`, `icon_pipeline` and `label_plan` were dropped here on a
//! `crate::sprite` reasoning that no longer holds -- see the module comment
//! above and `sprite.rs`'s own doc comment on why its `cfg` now excludes
//! iOS -- and are gone from the list above, not renamed into it. Labels
//! draw on this crate as of this task; see `ios/src/app.rs`'s `Labels` and
//! its `label_draw_plan` call site.
//!
//! `raster_mesh` and `raster_pipeline` were dropped here on the same
//! reasoning until an earlier task and are gone from the list above, not
//! renamed into it: `gpu.rs`'s `RasterDraws` (added by the globe/raster
//! merge, after the reasoning below was written) names
//! `crate::render::raster_pipeline` UNCONDITIONALLY -- not behind the
//! `SpaceDraw` this task adds -- so the type has to resolve here whether or
//! not this crate ever builds one, and the old DROPPED reason no longer
//! held regardless: `raster_pipeline.rs` names `image::RgbaImage` but never
//! `crate::raster`, and `image` is already this crate's own dependency
//! (`style_spec::lut`'s base64 PNG decode). Verified by grep, not assumed.
//! This crate now builds a `RasterPipeline` too -- see `ios/src/app.rs`'s
//! `raster_plan` call site.
//!
//! # Why the `allow(dead_code)`
//!
//! A `#[path]` include takes a whole FILE -- the smallest unit there is --
//! so `alloc.rs` arrives complete, including the fifteen counters only
//! `map_view` ever writes. Those are dead here by construction and will
//! stay dead. The attribute is scoped to exactly the engine modules that
//! carry such items -- `alloc`, `camera`, `mesh`, `raster_mesh`,
//! `raster_pipeline`, `glyph_atlas`, `icon_pipeline`, `solar`, `space` and
//! `subdivide` as of this writing, each for its own reason (see that
//! module's own `#[path]` comment) -- so the crate sits at zero warnings
//! and `scripts/check-ios.sh` can run with `-D warnings`. The shell's own
//! files (`app.rs`, `net.rs`, `styles.rs`, `display_link.rs`) are not
//! covered by it and get the lint as normal.
#[allow(dead_code)]
#[path = "../../../src/render/alloc.rs"]
pub mod alloc;
// The globe/space/atmosphere stack this crate now draws with. `atmosphere`,
// `globe`, `solar`, `space` and `subdivide` are what `gpu.rs`'s `SpaceDraw`
// and `Subdivision`-keyed tessellation already name unconditionally -- they
// were never optional here, only unreached: `ios/src/app.rs` had no call
// site for any of them (see `app.rs`'s own module doc). They cross to this
// crate's closure exactly as the module comment above says the globe merge
// crossed to wasm: `Camera`, `TileId`, `TileMesh`/`Vertex`, `Lerp`/`Rgba` are
// all already here, and none of the five names `crate::sprite`, `map_view`
// or anything else outside this crate's reach.
#[path = "../../../src/render/atmosphere.rs"]
pub mod atmosphere;
// `atmosphere` takes its eye from `altitude::eye_sphere`; the rest of the
// module (satellite maths) has no caller in this shell yet.
#[allow(dead_code)]
#[path = "../../../src/render/altitude.rs"]
pub mod altitude;
#[allow(dead_code)]
#[path = "../../../src/render/camera.rs"]
mod camera;
#[path = "../../../src/render/globe.rs"]
pub mod globe;
// The 8-byte tile vertex and paint table `gpu.rs`'s compact pipelines
// are built over. Nothing here bakes compact meshes yet.
#[allow(dead_code)]
#[path = "../../../src/render/compact.rs"]
pub mod compact;
// The instance `gpu.rs`'s circle pipeline is built over. The shell draws no
// overlay sources yet, so nothing here makes circles.
#[allow(dead_code)]
#[path = "../../../src/render/circles.rs"]
pub mod circles;
// `fill-extrusion` tessellation, which `app.rs` bakes beside each tile's
// flat mesh. The allow covers the helpers only native and tests call.
#[allow(dead_code)]
#[path = "../../../src/render/extrusion.rs"]
pub mod extrusion;
// The extrusion pipeline `gpu.rs`'s `TilePipeline` builds and
// `TileDraw::Extrusion` names; `app.rs` pushes its draws. The allow covers
// the helpers only native and tests call.
#[allow(dead_code)]
#[path = "../../../src/render/extrusion_gpu.rs"]
pub mod extrusion_gpu;
// The donut-cluster pipeline (`DonutPipeline`, `donut_shader.wgsl`). Only
// `map_view` draws donuts, which is not in this closure; included so the
// shell compiles the pipeline and shader it will draw with, hence the allow.
#[allow(dead_code)]
#[path = "../../../src/render/donut.rs"]
pub mod donut;
// The altitude layer's point pipeline (`AltitudePipeline`,
// `altitude_shader.wgsl`): dots above the globe (satellites), drawn by
// `app.rs` over `altitude_layers` below, as `map_view` draws it natively
// (spec 2026-09-29-satellites §2.8).
#[allow(dead_code)]
#[path = "../../../src/render/altitude_pipeline.rs"]
pub mod altitude_pipeline;
// The altitude layers' state (layers, keyframe workers, scene clock, hit
// test) the desktop's `MapView` drives, driven here by `app.rs`'s frame:
// the Studio app's satellites. `dead_code`: the line layers and the
// queries this shell does not call yet.
#[allow(dead_code)]
#[path = "../../../src/render/altitude_layers.rs"]
pub mod altitude_layers;
// Origin-destination arcs (`ArcPipeline`, `arc_shader.wgsl`):
// `overlay::arcs` (included whole with `overlay`) builds `ArcSet`s from
// it, and `overlay::shell_layers` draws them in the app's overlay pass.
#[allow(dead_code)]
#[path = "../../../src/render/arcs.rs"]
pub mod arcs;
// The arcs' CPU hit test: `overlay::shell_layers` picks arcs with it on a
// tap. It used to be DROPPED here; the shell draws and picks arcs now.
#[allow(dead_code)]
#[path = "../../../src/render/arc_pick.rs"]
pub mod arc_pick;
// Animated trips (`TripPipeline`, `trips_shader.wgsl`): `overlay::trips`
// builds `TripSet`s from it, and `overlay::shell_layers` draws them (the
// app's overlay pass, after the arcs).
#[allow(dead_code)]
#[path = "../../../src/render/trips.rs"]
pub mod trips;
// One frame read back from the surface (`MapView::grab_next_frame`'s
// copy): `host::Host::capture_frame`, what the host's pixel tests read.
#[path = "../../../src/render/frame_grab.rs"]
pub mod frame_grab;
// Screen-space heatmaps (`HeatmapPipeline`, `heatmap_shader.wgsl`), drawn by
// `overlay::shell_layers` under the arcs.
#[allow(dead_code)]
#[path = "../../../src/render/heatmap.rs"]
pub mod heatmap;
// Extruded overlay polygons (3D hexbins, 3D GeoJSON) on the tile pass's
// extrusion pipeline: `overlay::shell_layers` uploads them. It used to be
// DROPPED here; the shell draws them after the basemap's buildings now.
#[path = "../../../src/render/overlay_extrusion.rs"]
pub mod overlay_extrusion;
// 3D models (`ModelPipeline`, `model_shader.wgsl`): `gpu.rs`'s
// `TileDraw::Models` names it. Only `map_view` draws them today; included
// so it compiles here and `check-ios` builds the pipeline under Metal's
// limits.
#[allow(dead_code)]
#[path = "../../../src/render/model.rs"]
pub mod model;
// The world dot-density overlay's GPU data. `overlay::dotgrid` (included
// whole with `overlay`) builds `DotBlock`s from it; this shell does not
// draw dots yet (Phase 4 of the world dot-density plan), hence the allow.
#[allow(dead_code)]
#[path = "../../../src/render/dots.rs"]
pub mod dots;
// The world-anchored dot mask `gpu.rs`'s `encode_dot_mask` renders into.
// Same allow, for the same reason: this shell draws no dots yet.
#[allow(dead_code)]
#[path = "../../../src/render/dot_mask.rs"]
pub mod dot_mask;
#[path = "../../../src/render/gpu.rs"]
pub mod gpu;
#[allow(dead_code)]
#[path = "../../../src/render/mesh.rs"]
pub mod mesh;
#[path = "../../../src/render/run_split.rs"]
pub(crate) mod run_split;
// `mesh.rs`'s bad-stroke-normal diagnostic dump
// (`GPUI_MAP_DUMP_BAD_NORMALS`). Its own file-writing half is
// `cfg(not(target_arch = "wasm32"))` and this crate targets a real OS, so
// it dumps for real here too -- unlike `run_split` above, nothing here is
// unreached.
#[path = "../../../src/render/bad_normal_dump.rs"]
pub(crate) mod bad_normal_dump;
// `gpu.rs`'s `RasterDraws` names both of these unconditionally -- see this
// file's module comment for why the DROPPED reasoning that used to cover
// them no longer holds. This crate now builds a `RasterPipeline` for real
// (see `ios/src/app.rs`), but `#[allow(dead_code)]` stays: a `#[path]`
// include takes the whole FILE, and each still carries items (test-only
// helpers, alternate constructors) this crate's own call graph does not
// reach, same as `alloc`/`camera`/`mesh`.
#[allow(dead_code)]
#[path = "../../../src/render/raster_mesh.rs"]
pub mod raster_mesh;
#[allow(dead_code)]
#[path = "../../../src/render/raster_pipeline.rs"]
pub mod raster_pipeline;
// Labels: the SDF text-glyph atlas (rasterises a shaped glyph into the same
// signed-distance encoding `pack_icon_atlas` uses for icons, via
// `crate::sprite::sdf_from_mask` -- see `sprite.rs`'s doc comment for why
// that no longer drags in `reqwest`) and the pipeline that draws both an
// icon sprite sheet and this atlas through one shader (`IconInstance`,
// `IconRect`, `pack_icon_atlas`, `render_batches`). This crate now packs a
// REAL icon sprite sheet too -- the bundled one always, Mapbox's own for
// one style (see `app.rs`'s `load_embedded_sprites`/
// `try_install_mapbox_sprites`) -- so `pack_icon_atlas` and the sprite half
// of `IconInstance` are live, same as the glyph half. `#[allow(dead_code)]`
// stays for the same whole-FILE reason as `raster_mesh`/`raster_pipeline`
// above: a `#[path]` include still takes items this crate's own call graph
// does not reach (test-only constructors and the like).
#[allow(dead_code)]
#[path = "../../../src/render/glyph_atlas.rs"]
pub(crate) mod glyph_atlas;
#[allow(dead_code)]
#[path = "../../../src/render/icon_pipeline.rs"]
pub(crate) mod icon_pipeline;
// The nine-patch fitter `icon-text-fit` stretches overlay icons with. Named
// by `symbol_pass::overlay_symbol_parts` (included above with
// `symbol_pass`), which is where the overlay loop's icon quads are built.
#[allow(dead_code)]
#[path = "../../../src/render/icon_fit.rs"]
pub(crate) mod icon_fit;
// The chrome's rounded panels (`nav_layer::NavChromeGpu`). It used to be
// DROPPED because nothing here drew an attribution plate; the chrome does.
#[allow(dead_code)]
#[path = "../../../src/render/plate_pipeline.rs"]
pub(crate) mod plate_pipeline;
// Navigation's route mesh and chrome pass; see its module doc.
#[path = "../../../src/render/nav_layer.rs"]
pub mod nav_layer;
#[allow(dead_code)]
#[path = "../../../src/render/solar.rs"]
pub mod solar;
#[allow(dead_code)]
#[path = "../../../src/render/space.rs"]
pub mod space;
#[allow(dead_code)]
#[path = "../../../src/render/sky.rs"]
pub mod sky;
// The opt-in space look: `gpu.rs`'s `SpaceDraw` names its pipeline. This
// shell does not draw it yet (no texture fetcher), hence dead code.
#[allow(dead_code)]
#[path = "../../../src/render/space_look.rs"]
pub mod space_look;
#[allow(dead_code)]
#[path = "../../../src/render/subdivide.rs"]
pub mod subdivide;
// One place decides what a frame's space pass contains, for every target --
// see its own module doc. Zero `gpui`/`wgpu`/DOM/UIKit references, so it
// costs this crate nothing to include and nothing to keep in step.
#[path = "../../../src/render/frame_plan.rs"]
pub mod frame_plan;
// Which tiles a frame's cover contains -- the fix for the globe showing a
// blank hemisphere when a shell asks a flat function for a spherical
// answer. Zero `gpui`/`wgpu`/DOM/UIKit references, same as `frame_plan`
// above.
#[path = "../../../src/render/cover_plan.rs"]
pub mod cover_plan;
// Which pass each overlay source draws in (`ShellOverlays::push_draws_split`):
// over or under the buildings and the labels. Zero `gpui`/`wgpu`/DOM/UIKit
// references, same as `frame_plan` above.
#[path = "../../../src/render/overlay_order.rs"]
pub mod overlay_order;
// Whether one frame's imagery draws, at what opacity, and at what cover
// zoom -- the raster counterpart of `frame_plan` above, and zero
// `gpui`/`wgpu`/DOM/UIKit references for the same reason: it names only
// `crate::style` and `crate::geo`, both already in this crate's closure
// (this crate already draws vector tiles, which need `style` regardless).
// `ios/src/app.rs` calls it every frame, sharing the one decision every
// other shell does rather than re-deriving `raster_cover_zoom`'s
// tile-size/scale-factor correction or the style's raster-layer zoom gate
// on its own.
#[path = "../../../src/render/raster_plan.rs"]
pub mod raster_plan;
// The place/merge/sort/clip/split step every label-drawing shell now shares
// -- turns `symbol_pass::collect_candidates`' output into the three
// draw-ready lists `ios/src/app.rs`'s symbol pass draws from. Zero
// `gpui`/`wgpu`/DOM/UIKit references, same as `frame_plan`/`cover_plan`/
// `raster_plan` above; unlike `glyph_atlas`/`icon_pipeline`, nothing in this
// file is unused here, so it carries no `#[allow(dead_code)]`.
#[path = "../../../src/render/label_plan.rs"]
pub(crate) mod label_plan;
// Label fade-in and the glyphon glyph-raster allowance -- pure state, no
// `gpui`/`wgpu`/DOM/UIKit references, same as `label_plan` above. Wired into
// `ios/src/app.rs`'s `Renderer::draw_labels` the same way `map_view.rs`
// wires them: `text_glyph_budget::TextGlyphBudget::admit` right after
// `label_draw_plan`, then `label_fade::LabelFade` over the three lists it
// produced.
#[path = "../../../src/render/label_fade.rs"]
pub(crate) mod label_fade;
#[path = "../../../src/render/text_glyph_budget.rs"]
pub(crate) mod text_glyph_budget;

// One glob per included module, deliberately. See this file's module
// comment for why these are not a hand-written list any more.
//
// `alloc.rs`'s `frame_ms_percentile`/`frame_stats`/`reset_frame_stats`/
// `FrameTimer` are still absent, and still not missing: the engine gates
// them behind the crate's `probe` feature, which this crate declares and
// leaves off.
pub use alloc::*;
pub use atmosphere::*;
pub use camera::*;
pub use globe::*;
pub use gpu::*;
pub use mesh::*;
pub use raster_mesh::*;
pub use raster_pipeline::*;
// `glyph_atlas`, `icon_pipeline` and `label_plan` are `pub(crate) mod`, not
// `pub mod` (see their own `#[path]` lines) -- every item a `#[path]`
// include pulls out of them is `pub(crate)` in the shipped source, so a
// `pub use` here would claim a visibility the items do not have and warn
// on it (`-D warnings` in `scripts/check-ios.sh` turns that into a build
// failure). `pub(crate) use` matches what is actually there, the same way
// `run_split` above takes no re-export line at all.
pub(crate) use glyph_atlas::*;
// The shipped `src/render/mod.rs` re-exports this one RENAMED
// (`font_scale as glyph_font_scale`) because `mesh.rs`/`gpu.rs` already
// have their own `font_scale`-shaped names; `symbol_pass.rs` and
// `map_view.rs` both call `crate::render::glyph_font_scale`, never
// `crate::render::font_scale`. This file's glob does not rename anything,
// so the alias needs its own line, next to the glob it rides beside.
pub(crate) use glyph_atlas::font_scale as glyph_font_scale;
pub(crate) use icon_pipeline::*;
pub(crate) use plate_pipeline::*;
pub use solar::*;
pub use space::*;
pub use subdivide::*;
pub use frame_plan::*;
pub use cover_plan::*;
pub use raster_plan::*;
// No re-export line, the same way `run_split` above takes none: this
// crate's own `app.rs` calls `crate::render::label_plan::label_draw_plan`
// by its full path (matching `map_view.rs`'s and `web/src/app.rs`'s own
// call sites), so `label_plan::*` would sit here unused and `-D warnings`
// (`scripts/check-ios.sh`) would refuse it. Checked by actually building,
// not assumed: the crate warned on this exact line until it was removed.
//
// `label_fade` and `text_glyph_budget` take no re-export line for the same
// reason: `app.rs`'s `Renderer::draw_labels` names both by their full path
// (`crate::render::label_fade::{LabelFade, FadeRedrawScheduler, fade_id,
// MIN_REDRAW_INTERVAL_SECONDS}`, `crate::render::text_glyph_budget::{TextGlyphBudget,
// NEW_GLYPHS_PER_FRAME}`), matching `map_view.rs`'s own call sites.

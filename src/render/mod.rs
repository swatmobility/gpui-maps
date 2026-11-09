//! The renderer's module list -- and, for the browser build, the only one.
//!
//! `web/src/lib.rs` `#[path]`-includes THIS file rather than restating it.
//! It used to restate it, in a 56-line `web/src/render/mod.rs`, and the
//! restatement drifted every time an item was added here: nine of the
//! seventeen errors the browser build carried at `29453cd` were nothing but
//! names this list has and that one had lost (`vec_block_bytes`,
//! `heap_block_bytes`, `hashmap_table_bytes`, `GlyphKey`, `GlyphAtlas`,
//! `glyph_font_scale`). So: add an item here and the browser gets it; there
//! is no second list to remember.
//!
//! What that costs is the handful of `#[cfg_attr(target_arch = "wasm32",
//! allow(unused_imports))]` attributes below. Each one marks a `pub(crate)`
//! re-export whose only consumer is `map_view.rs`, which is not in the
//! browser crate's module closure (see `web/src/lib.rs`'s own comment for
//! what is). They are `cfg_attr`, not a bare `allow`, so an unused import
//! in the NATIVE build is still a warning -- the browser build is the only
//! one told to expect these.
//!
//! `macos_layer` is the one module that genuinely cannot cross, and it
//! needed nothing: `#[cfg(target_os = "macos")]` already excludes
//! `wasm32-unknown-unknown`, whose `target_os` is `"unknown"`. Everything
//! else here -- `debug_pipeline`, `debug_tiles`, `coverage`, `glyph_atlas`,
//! `icon_fit` included, all five of which the old shim dropped -- compiles
//! for wasm unchanged.
//!
//! That survived the globe merge without an exception. `atmosphere`,
//! `globe`, `solar`, `space` and `subdivide` -- six modules, two WGSL
//! shaders and a 45 KB star catalogue, none of which the old shim knew
//! about -- all cross untouched too, `solar` by way of the one
//! `cfg(target_arch = "wasm32")` clock read it already carried. The three
//! errors the merge did produce were arity, not portability:
//! `tessellate_plan`, `create_model_bind_group` and `write_model_uniform`
//! each grew a parameter and `web/src/app.rs` still called the old shape.
//! That is the class of break only a compiler finds, and only this list
//! being shared puts it in front of one.
//!
//! `scripts/check-wasm.sh`, from the repository root, is what proves that
//! is still true.
//!
//! # `ios/src/render/mod.rs` is still a second copy, and still drifting
//!
//! Measured at `29453cd`, the same day as the browser build:
//! `cd ios && cargo check --target aarch64-apple-ios` gives ELEVEN errors,
//! and all eleven are the same three names -- `vec_block_bytes`,
//! `heap_block_bytes`, `hashmap_table_bytes`. Identical cause, identical
//! silence.
//!
//! It is not fixed here because this one line does not reach it. The iOS
//! crate's module closure has no `sprite` (see `ios/src/lib.rs`), and
//! `render::glyph_atlas` and `render::icon_pipeline` both name
//! `crate::sprite` -- so including this list whole would pull the label and
//! sprite halves into a build that has never drawn either. That is a port,
//! not a repair, and it cannot be confirmed without a device. Left as a
//! known, named break rather than a guessed-at fix.
pub mod alloc;
// The bad-stroke-normal diagnostic dump (`GPUI_MAP_DUMP_BAD_NORMALS`); see
// the module doc. The dump itself never runs on wasm32 (no filesystem);
// the module still compiles there so its callers need no `cfg`.
pub(crate) mod bad_normal_dump;
// Zoom-continuity log for tiled overlays; see the module doc.
#[cfg(feature = "probe")]
pub mod zoom_probe;
// The glow: a Rayleigh/Mie scattering integral run full-screen over the
// globe, gated on the same transition everything else in this plan is. `pub`
// because `Atmosphere` and `Sun` are consumer-facing settings, reached
// through `Space` and `MapView::set_space` -- there being no `light` in this
// crate for them to live on. See its module docs for the two-link provenance
// chain.
pub mod atmosphere;
// Points above the globe (satellites): the ECEF/TEME to sphere frame, the
// planet's occlusion of a point, the CPU projection and keyframe Hermite.
// Pure maths, so it crosses to every shell; `atmosphere` takes its eye from it.
pub mod altitude;
// The altitude layer's instanced point pipeline over `altitude`'s maths:
// dots above the globe, hidden by the planet analytically. See its module docs.
pub mod altitude_pipeline;
// The altitude layers' state over the pipeline -- layers, keyframe workers,
// scene clock, hit test -- without gpui, so every shell drives the same one.
pub mod altitude_layers;
// Origin-destination arcs: one instance per arc, the strip generated in the
// vertex stage (deck.gl's ArcLayer). Flat and globe. See its module docs.
pub mod arcs;
// Which arc is under the cursor: a grid over the chords, built once per
// set, and the exact test against the drawn polyline. See its module docs.
pub mod arc_pick;
// Animated trips: per-vertex timestamps, the scene clock and the trail as
// a uniform (deck.gl's TripsLayer). Flat and globe. See its module docs.
pub mod trips;
// 3D models: one triangle mesh instanced at lon/lat/altitude with a
// heading, tinted per instance, depth-tested in the tile pass. See its
// module docs.
pub mod model;
// Heatmaps: screen-space kernel density summed into a float target, then
// coloured over the frame's maximum (deck.gl's HeatmapLayer). See its
// module docs.
pub mod heatmap;
mod camera;
// The 8-byte tile vertex (`i16x2` position, `u16` paint index, `u16`
// flags) and the paint table `vs_compact` reads. See its module docs.
pub mod compact;
// Instanced circles: one 16-byte instance per overlay dot. See its module docs.
pub mod circles;
// `fill-extrusion` roofs and walls: the 12-byte extrusion vertex and the
// per-feature height/base/colour table. See its module docs.
pub mod extrusion;
// The `fill-extrusion` pipeline, its per-chunk buffers and per-layer
// uniforms; `TileDraw::Extrusion` draws through it. See its module docs.
pub mod extrusion_gpu;
// Overlay extrusion's GPU half: `overlay::extrude`'s synthetic-tile meshes
// uploaded and drawn through the extrusion pipeline. See its module docs.
pub mod overlay_extrusion;
// Donut clusters: an instanced SDF ring cut into group sectors, anchored at
// CPU-projected points like icons. `pub` because `Donut` is the argument of
// `MapView::set_donuts`. See its module docs.
pub mod donut;
// The world dot-density overlay's GPU data: `DotBlock` (one texel of a
// tile's block texture), the `vs_dot` uniform `DotParams` and the density
// ramp. The pipeline that draws them is `TilePipeline`'s dot pipeline
// (`gpu.rs`, `TileDraw::Dots`). See its module docs.
pub mod dots;
// The world-anchored dot mask: an `R8Uint` texture of basemap ranks on a
// texel grid fixed to the world at the camera's integer zoom, which
// `vs_dot` reads to relocate a dot inside its cell. See its module docs.
pub mod dot_mask;
#[cfg(feature = "probe")]
pub mod mask_timer;
pub(crate) mod debug_pipeline;
pub mod debug_tiles;
pub mod coverage;
// The globe projection's camera-level maths -- matrices, the horizon clipping
// plane, the Mercator<->sphere transition scalar. `pub` because a consumer
// driving the camera needs to be able to ask what globeness a zoom implies;
// deliberately contains no rendering at all (see its module docs).
pub mod globe;
pub mod gpu;
// The glyph atlas for line-placed text; re-exported below, beside
// `icon_pipeline`'s items, now that `symbol_pass`'s per-glyph quad builder
// draws from it.
pub(crate) mod glyph_atlas;
pub(crate) mod icon_fit;
pub(crate) mod icon_pipeline;
pub mod mesh;
// The square a raster tile wears its texture on -- one per granularity,
// shared by every tile at that zoom, and cut by the same
// `subdivide::granularity_for_zoom` the fills are cut by so the two land
// on the same sphere. `pub` for the same reason `subdivide` is: a caller
// outside `render` computes the key from a `TileId`.
pub mod raster_mesh;
// The pass that draws raster tiles: one textured square per tile, into the
// same render pass the fills go into, immediately before them. `pub`
// because `capture` and `map_view` both construct one.
pub mod raster_pipeline;
pub(crate) mod plate_pipeline;
// Story markers: discs with a stroke and a blur (spec §6.4), capture-only.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(crate) mod marker_pipeline;
pub(crate) mod run_split;
// Bake-time subdivision: the grid split that makes the globe's limb a circle
// instead of a polygon, and the pole caps that close the sphere past
// Mercator's +/-85.051 degrees. `pub` because `Subdivision::for_tile` is what
// a caller outside `render` (`map_view`'s bake) has to compute.
pub mod subdivide;
// What the globe sits ON: the space colour outside the silhouette, the
// background layer re-expressed as an ocean sphere behind the tiles, and a
// star field of real stars, fixed to the ground. `pub` because `Space` is a
// consumer-facing setting (`MapView::set_space`) and because the star gate
// is off by default, so a caller has to be able to name it.
pub mod space;
// The sky above the globe's limb: MapLibre's style-level `sky`, drawn
// behind the ground and faded to space by the globe transition.
pub mod sky;
// The opt-in space look: a star-map sky, clouds, sun shading and a rim.
// Takes decoded textures; fetching them is `crate::space_assets` (native).
pub mod space_look;
pub mod frame_plan;
// Which side of the symbol pass each overlay source draws on -- the
// `beforeId` decision, shared by `MapView::paint` and `capture`.
pub mod overlay_order;
// The orchestration around `symbol_pass`'s label placement -- what one
// frame's placed symbols become as three draw-ready instance lists, in
// draw order. `pub(crate)`, not `pub`: unlike `frame_plan::SpacePlan` this
// has no consumer-facing constructor, only shell call sites inside this
// crate (and the browser/iOS crates, which reach it through this same
// file). See its own module doc for the defect it exists to prevent.
pub(crate) mod label_plan;
// Label fade-in and the glyphon glyph-raster allowance: what keeps a
// budgeted label from popping in, and the budget itself. Pure state, no
// GPU, so every shell can reach them through this file.
pub(crate) mod label_fade;
pub(crate) mod text_glyph_budget;
// Navigation's route mesh (and, from the chrome task on, its chrome pass):
// the GPU half of `crate::nav`. `pub` because every shell holds one.
pub mod nav_layer;

// Which tiles one frame needs: `GlobeCover` when the frame is spherical,
// a flat bbox only when it is not. Shared because all THREE shells asked a
// flat mercator function for a spherical answer, which is why the browser
// drew a globe with a blank hemisphere.
pub mod cover_plan;
// Whether one frame's imagery draws, at what opacity, and at what cover
// zoom -- the raster counterpart of `frame_plan`. `pub` for the same reason
// `frame_plan` is: `RasterPlan` and `raster_plan` are consumer-facing (every
// shell, including the browser build reaching this file by `#[path]`, calls
// this to decide its raster pass).
pub mod raster_plan;
// Where the sun IS, for an instant -- the subsolar point from a UTC
// timestamp. `pub` because `Sun::at_utc` is a consumer-facing constructor
// and a consumer with its own clock needs to be able to name the module the
// arithmetic is argued in. Pure arithmetic over `std` and nothing else, so
// it crosses to `wasm32-unknown-unknown` untouched apart from the one
// `cfg`-gated clock read `now_unix_seconds` carries.
pub mod solar;
// A frame as drawn, read back on request (`MapView::grab_next_frame`).
pub mod frame_grab;
#[cfg(target_os = "macos")]
pub mod macos_layer;

pub use camera::{
    combined_overlay_matrix, combined_tile_matrix, dash_scale, overlay_dash_scale,
    pitch_aware_tile_bbox, tile_blend_t_for_baked_zoom,
    tile_in_frustum, tile_model_matrix, unproject_to_ground, unproject_to_ground_f64, Camera,
    WORLD_SIZE,
};
// `pub(crate)` (not `pub use` above) because `project_clip_to_screen` itself
// is only `pub(crate)` -- this makes it reachable crate-wide as
// `crate::render::project_clip_to_screen` without claiming a visibility the
// underlying item doesn't have. (`project_world_to_screen`, its world-space
// sibling, is `#[cfg(test)]` and used only from `camera.rs`'s own tests.)
//
// Used by `map_view::project_symbol_anchor`, the shared anchor-projection
// helper both the text and icon symbol-drawing loops call.
//
// `project_world_to_screen_f64` rides along for the same reason: it is
// `pub(crate)` on `camera` and reached crate-wide as
// `crate::render::project_world_to_screen_f64` -- the symbol loop's
// world-space anchor projection, used for overlay features, which have no
// tile to be local to.
// Of the three, only `project_clip_to_screen` is reached from the browser
// crate; the two world-space ones are overlay projections and overlay lives
// in `map_view`. See this file's module comment.
#[cfg_attr(target_arch = "wasm32", allow(unused_imports))]
pub(crate) use camera::{project_clip_to_screen, project_world_to_screen_f64_with_matrix};
// `render::camera` is a private module and `camera_control` sits outside `render`, so its
// `fit_shell_zoom` (and this file's own test module) reach the FOV constant through here.
pub(crate) use camera::FOV_Y_RADIANS_F64;
// The plain form: `WorldProjector` (globe.rs) passes its matrix in via
// `_with_matrix`; `MapView::nav_puck_screen` and tests use this one.
#[cfg_attr(target_arch = "wasm32", allow(unused_imports))]
pub(crate) use camera::project_world_to_screen_f64;
// `pub(crate)` for the same reason, and deliberately NOT in the public list
// above: it exists for `MapView::feature_at`'s pixel->world tolerance, whose
// whole consumer (`overlay::hit`) is itself `pub(crate)`. Exporting a camera
// query nothing outside the crate can use the answer for would be a
// versioning promise bought for nothing.
#[cfg_attr(target_arch = "wasm32", allow(unused_imports))] // map_view-only; see this file's module comment
pub(crate) use camera::pitch_scale_factor_at_ground;
// The dot layer ranks its part tiles nearest the eye (`DotLayers::frame`).
pub(crate) use camera::eye_ground_point;
pub use alloc::{allocation_stats, heap_in_use_bytes, rss_bytes, AllocationStats, TrackedBuffer};
// The footprint model. `pub` and ungated alongside the heap counter it is
// graded against, and for the same reason: the per-tile cache budgets are
// denominated in this unit, so a consumer sizing those budgets for its own
// platform has to be able to compute the same number the cache will.
pub use alloc::{hashmap_table_bytes, heap_block_bytes, vec_block_bytes};
// The paint census is `pub` and ungated for the same reason it is not
// behind `probe`: "did this window ever draw at all" has to be answerable
// from outside the process, in every build. See `alloc`'s own section
// comment for the measured bug that argument comes from.
pub use alloc::{note_process_start, paint_census, paints_skipped_unchanged, PaintCensus, PaintSkip};
#[cfg_attr(target_arch = "wasm32", allow(unused_imports))] // map_view-only; see this file's module comment
pub(crate) use alloc::{note_paint_entered, note_paint_skipped, note_paint_submitted};
#[cfg_attr(target_arch = "wasm32", allow(unused_imports))] // map_view-only; see this file's module comment
pub(crate) use alloc::note_overlay_mesh_install;
// `paint_retessellations_total`'s writer. `pub(crate)` and ungated: the
// counter it feeds is part of the shipped contract (see its field doc), but
// `MapView::plan_for` is the only thing that may ever write it.
#[cfg_attr(target_arch = "wasm32", allow(unused_imports))] // map_view-only; see this file's module comment
pub(crate) use alloc::note_paint_retessellation;
#[cfg(feature = "probe")]
pub use alloc::{
    frame_ms_percentile, note_ancestor_stand_in, note_descendant_stand_in, note_frame_nanos,
    note_retention_bound, note_tile_fetch_bytes, note_tile_fetch_issued, note_uncovered_ancestors,
    note_visible_tile_without_stand_in, reset_frame_stats, FrameTimer,
};
#[cfg(feature = "probe")]
pub use alloc::{frame_log_start, frame_log_take, FrameSample};
#[cfg(feature = "probe")]
pub(crate) use alloc::note_surface_acquire_nanos;
// The paint-phase split, beside the frame timer it apportions and gated the
// same way. `PhaseLap` is the writer (`paint` holds one), the five
// accessors are the readers, and `PaintPhase`/`PaintPhases` are the shape
// both halves agree on -- see `alloc`'s paint-phase section.
//
// `paint_phase_window_over` and `paint_phases_window_max` are out here for
// the same reason the other readers are: a consumer grading a hitch needs
// the RATE, and the max is n=1 (see their docs). `PAINT_PHASE_SAMPLES` is
// the window the count is out of, so it travels with them.
#[cfg(feature = "probe")]
pub use alloc::{
    note_paint_phase, paint_phase_samples, paint_phase_window_over, paint_phases_max,
    paint_phases_mean, paint_phases_p50, paint_phases_window_max, reset_paint_phases, PaintPhase,
    PaintPhases, PhaseLap, PAINT_PHASE_COUNT, PAINT_PHASE_SAMPLES,
};
pub use gpu::{
    CompactBuffers, DotGroupGpu, DotTileGpu, GlobeParams, GpuContext, HorizonFog, PaintTableGpu, PassTimer, SpaceDraw, TileBuffers, TileClip, TileDraw,
    TilePipeline, LINE_FX_NONE,
};
// The globe's backdrop and star field. `Space` is the consumer-facing
// setting; `StarPipeline` is what `map_view` builds the first frame a
// non-zero `star_intensity` is drawn.
pub use space::{Space, StarPipeline};
pub use atmosphere::{dark_label_colors, Atmosphere, AtmospherePipeline, Sun, SymbolLight, SymbolShade};
#[cfg_attr(target_arch = "wasm32", allow(unused_imports))] // map_view-only; see this file's module comment
pub(crate) use icon_pipeline::{pack_icon_atlas, IconInstance, IconPipeline, IconRect, SdfPaint};
// The glyph atlas line-placed text samples. `font_scale` is renamed on the
// way out: at the call site it sits next to `symbol_pass::icon_font_scale`,
// its sprite-side twin, and one unqualified `font_scale` between the two
// would be the easiest possible thing to reach for by mistake -- and the
// mistake draws something, just at the wrong size with a halo threshold
// computed against the wrong scale.
pub(crate) use glyph_atlas::{font_scale as glyph_font_scale, GlyphAtlas, GlyphKey};
// Re-exported for `sprite`'s own tests, which assert that the SDF ENCODING
// `sdf_from_mask` writes is the exact inverse of the DECODING these three
// drive in `icon_shader.wgsl`. Two halves of one convention, in two
// modules, with nothing but a test to hold them together.
#[cfg(test)]
pub(crate) use icon_pipeline::{SDF_EDGE, SDF_HALO_BASE, SDF_PX};
pub use mesh::{
    can_splice_changed_ops,
    coalesced_vertex_spans, rewrite_paint_attributes, rewrite_paint_attributes_for_ops,
    splice_changed_ops,
    tessellate_overlay_plan, tessellate_plan, tessellate_plan_with_ranges, tessellate_plan_with_ranges_cancellable,
    tessellate_plan_with_ranges_reusing,
    OpSpan, TileMesh, Vertex,
};
#[cfg_attr(target_arch = "wasm32", allow(unused_imports))] // map_view-only; see this file's module comment
pub(crate) use plate_pipeline::{PlateInstance, PlatePipeline};
#[cfg_attr(target_arch = "wasm32", allow(unused_imports))] // map_view-only; see this file's module comment
pub(crate) use debug_pipeline::DebugPipeline;
// The tile-status debug overlay's vocabulary, `pub` because a host app
// embedding `MapView` needs the same names to drive
// `MapView::set_debug_tile_overlay` and to read `tile_debug_statuses`.
pub use debug_tiles::{TileDataState, TileDebugStatus, TileDrawSource};
// The import-coverage overlay's vocabulary -- `pub` for the same reason:
// a host application loads the generated document and hands the cells to
// `MapView::set_coverage_cells`.
pub use coverage::{parse_coverage, CoverageCell, CoverageStatus};
#[cfg(target_os = "macos")]
pub use macos_layer::MapLayer;

/// Guards the one-list property this file's module comment claims, and the
/// `#[path]` includes that depend on it.
///
/// These run under a plain `cargo test` and need no wasm toolchain. They do
/// NOT replace `scripts/check-wasm.sh` -- nothing but a compiler catches a
/// changed function signature, which was two of the seventeen errors
/// `29453cd` carried. What they catch is cheaper and earlier: the browser
/// build quietly going back to a second, hand-maintained copy of the list
/// above, and an engine file being renamed out from under a `#[path]` that
/// still names the old one.
#[cfg(test)]
mod browser_build_shares_this_list {
    use std::path::{Path, PathBuf};

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    /// `web/` is absent from a packaged `.crate` and present in every
    /// checkout. Skipping when it is absent is the only way these tests can
    /// be honest in both places -- and the skip is narrow: it is keyed on the
    /// directory not existing at all, not on any file inside it being
    /// missing, so deleting `web/src/lib.rs` still fails rather than skips.
    fn web_dir() -> Option<PathBuf> {
        let dir = repo_root().join("web");
        dir.is_dir().then_some(dir)
    }

    /// The browser crate must reach this module list, not restate it.
    ///
    /// It restated it in `web/src/render/mod.rs` for four releases. Nine of
    /// the seventeen errors at `29453cd` were names that restatement had
    /// lost; see this module's own comment for which.
    #[test]
    fn the_browser_crate_includes_this_file_rather_than_restating_it() {
        let Some(web) = web_dir() else { return };
        let lib = web.join("src/lib.rs");
        let source = std::fs::read_to_string(&lib)
            .unwrap_or_else(|err| panic!("cannot read {}: {err}", lib.display()));
        assert!(
            source.contains(r#"#[path = "../../src/render/mod.rs"]"#),
            "{} must `#[path]`-include `src/render/mod.rs` -- the engine's module list is \
             the only one there is. A second copy of it is what broke the browser build \
             four times; see `src/render/mod.rs`'s module comment.",
            lib.display(),
        );
        let shim = web.join("src/render/mod.rs");
        assert!(
            !shim.exists(),
            "{} is back. That file was a hand-maintained restatement of `src/render/mod.rs` \
             and drifted from it every time an item was added here. Whatever it was brought \
             back for, put it behind a `cfg` in `src/render/mod.rs` instead, so there stays \
             one list.",
            shim.display(),
        );
    }

    /// Every `#[path]` in the browser crate must name a file that exists.
    ///
    /// A renamed or moved engine file breaks the browser build and nothing
    /// native notices -- the wasm check is the only thing that compiles those
    /// includes, and it is a separate command. This is the half of that
    /// failure a plain `cargo test` can catch, and it costs two file reads.
    #[test]
    fn every_path_include_in_the_browser_crate_names_a_file_that_exists() {
        let Some(web) = web_dir() else { return };
        // The one hand-written module file that carries `#[path]` attributes
        // (`overlay` is the engine's own `src/overlay/mod.rs` now, included
        // whole). `app.rs` has none -- it is this crate's own code.
        let mut checked = 0usize;
        for relative in ["src/lib.rs"] {
            let file = web.join(relative);
            let source = std::fs::read_to_string(&file)
                .unwrap_or_else(|err| panic!("cannot read {}: {err}", file.display()));
            // The directory a non-inline `#[path]` resolves against is the
            // one holding the mod-rs file that declares it -- the same rule
            // the includes themselves are written to.
            let base = file.parent().expect("a file has a parent").to_path_buf();
            for line in source.lines() {
                let line = line.trim();
                let Some(rest) = line.strip_prefix(r#"#[path = ""#) else { continue };
                let Some(target) = rest.strip_suffix(r#""]"#) else { continue };
                let resolved = normalise(&base.join(target));
                // A `#[path]` on an INLINE module (`#[path = "."] pub mod
                // story { ... }`) names the directory its children resolve
                // from, not a file. Only `"."` is supported: this loop
                // resolves those children against `base`, which is right
                // only when the inline module's directory IS `base`.
                assert!(
                    target.ends_with(".rs") || target == ".",
                    "{}: `#[path = \"{target}\"]` on an inline module must be \".\" -- this test resolves its \
                     children against the file's own directory",
                    file.display(),
                );
                let exists = if target.ends_with(".rs") { resolved.is_file() } else { resolved.is_dir() };
                assert!(
                    exists,
                    "{}: `#[path = \"{target}\"]` resolves to {}, which does not exist. \
                     An engine file was renamed or moved and the include did not follow; \
                     the browser build is broken. `./scripts/check-wasm.sh` shows the rest.",
                    file.display(),
                    resolved.display(),
                );
                checked += 1;
            }
        }
        // A test that silently checked nothing would be the same class of bug
        // as the one it guards against.
        assert!(
            checked >= 10,
            "expected the browser crate to carry at least ten `#[path]` includes, found \
             {checked} -- either the parser above stopped matching or the crate stopped \
             reaching the engine by source",
        );
    }

    /// `Path::canonicalize` would do this, but it also resolves symlinks and
    /// fails outright on a missing file -- and a missing file is exactly what
    /// the caller wants to report on rather than panic over.
    fn normalise(path: &Path) -> PathBuf {
        let mut out = PathBuf::new();
        for part in path.components() {
            match part {
                std::path::Component::ParentDir => {
                    out.pop();
                }
                std::path::Component::CurDir => {}
                other => out.push(other),
            }
        }
        out
    }
}

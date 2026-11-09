//! What one frame's imagery/raster pass draws: whether it draws at all,
//! at what opacity, and at what zoom its cover is taken -- the raster
//! counterpart of `frame_plan::space_plan` and `cover_plan::tile_cover`,
//! extracted the same way and for the same reason.
//!
//! `src/map_view.rs` had this decision written out as three private
//! methods (`raster_opacity_for_frame`, `raster_tiles_for_frame`,
//! `raster_cover_zoom`). `web/src/app.rs` had none of it at all: the
//! browser has never had a raster pipeline, so satellite/relief imagery
//! set through a style's `"type": "raster"` source simply could not
//! appear there, no matter how the style was written. That is a
//! different KIND of gap than the space, cover and label extractions
//! closed -- those moved a call that one shell already made so every
//! shell made it; this one gives two shells (web today, iOS not yet
//! wired -- see `ios/src/render/mod.rs`'s own note) a decision to call
//! that native already had, so that adding the orchestration around it
//! (fetch, decode, upload, draw) is the only thing left shell-specific.
//!
//! Deliberately free of `gpui`, `wgpu`, DOM and UIKit, like every module
//! in this family: plain data in
//! (`camera_zoom`/`tile_size`/`scale_factor`/`source_max_zoom`/`layers`),
//! plain data out ([`RasterPlan`]). Fetching bytes, decoding them,
//! uploading a texture and drawing a square all stay in each shell --
//! `src/render/raster_pipeline.rs` and `src/render/raster_mesh.rs` are
//! the shared GPU-facing halves of that, already reachable from every
//! target because `src/render/mod.rs` declares them unconditionally.

use crate::style::StyleLayer;

/// What this frame's raster/imagery pass contains: nothing, or a texture
/// cover at a resolved opacity and zoom.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RasterPlan {
    /// `Some(opacity)` -- already resolved at the display zoom and
    /// clamped to `[0, 1]` -- when imagery draws this frame; `None` when
    /// it does not: no raster source at all, or a raster layer present
    /// whose zoom range excludes this frame's display zoom.
    pub opacity: Option<f32>,
    /// The zoom this frame's imagery cover should be requested at. Only
    /// meaningful when `opacity` is `Some`; left at `0` otherwise, since
    /// nothing asks for a cover when nothing will draw.
    pub cover_zoom: u8,
    /// The continuous zoom `cover_zoom` was rounded from (the camera's zoom
    /// plus `raster_cover_zoom`'s tile-size/scale-factor correction): the
    /// centre of the imagery cover's distance LOD. See
    /// [`RasterPlan::cover_request`].
    pub cover_center_zoom: f64,
    /// Whether a raster source is set and the style declares **no**
    /// raster layer at all -- `raster_opacity_for_frame`'s fourth case.
    /// `opacity` is still `Some(1.0)` in this case (the style-spec
    /// default, so a consumer who forgot the layer still sees imagery
    /// rather than nothing), and this flag is the caller's cue to print
    /// its own one-time notice. The notice's one-shot bookkeeping is a
    /// `Cell<bool>` on each shell's own state, not here -- a plan is
    /// recomputed every frame and must stay side-effect-free.
    pub declared_layerless: bool,
}

impl RasterPlan {
    /// Nothing to draw: no raster source, or a raster layer that excludes
    /// this frame's zoom.
    pub const NONE: RasterPlan =
        RasterPlan { opacity: None, cover_zoom: 0, cover_center_zoom: 0.0, declared_layerless: false };

    /// What to hand `cover_plan::flat_cover` for this frame's imagery: tiles
    /// at `cover_zoom` near the eye, the distance LOD centred on
    /// `cover_center_zoom` and ROUNDED, as `raster_cover_zoom` rounds (and
    /// as MapLibre's raster sources set `roundZoom`).
    #[must_use]
    pub fn cover_request(&self) -> crate::render::cover_plan::CoverZoom {
        crate::render::cover_plan::CoverZoom {
            zoom: self.cover_zoom,
            center_zoom: Some(self.cover_center_zoom),
            round: true,
            min_zoom: 0,
            building_floor: None,
            cutoff: None,
        }
    }
}

/// The single definition of whether this frame's imagery draws, at what
/// opacity, and at what cover zoom.
///
/// `has_source` gates everything else, character for character
/// `raster_opacity_for_frame`'s own first line (`self.raster_source.as_ref()?`)
/// -- `false` short-circuits to [`RasterPlan::NONE`] without reading
/// `layers` or computing a cover zoom at all, which is what keeps this
/// free for every map with no raster source.
///
/// `display_zoom` is the shell's already-resolved display zoom (native's
/// `MapView::display_zoom`, the browser's `MapApp::display_zoom` --
/// both `camera.zoom.floor()` clamped into `u8`); `camera_zoom`,
/// `tile_size` and `scale_factor` are [`raster_cover_zoom`]'s own
/// arguments, unchanged; `source_max_zoom` is the raster source's own
/// ceiling; `layers` is the style's layer list, walked exactly as
/// `raster_opacity_for_frame` walked `self.style.layers`.
#[must_use]
pub fn raster_plan(
    has_source: bool,
    display_zoom: u8,
    camera_zoom: f64,
    tile_size: u32,
    scale_factor: f32,
    source_max_zoom: u8,
    layers: &[StyleLayer],
) -> RasterPlan {
    if !has_source {
        return RasterPlan::NONE;
    }
    let RasterOpacity { opacity, declared_layerless } = raster_opacity(display_zoom, layers);
    let (cover_zoom, cover_center_zoom) = match opacity {
        Some(_) => {
            let request = raster_cover_request(camera_zoom, tile_size, scale_factor, source_max_zoom);
            (request.zoom, request.center_zoom.unwrap_or(camera_zoom))
        }
        None => (0, 0.0),
    };
    RasterPlan { opacity, cover_zoom, cover_center_zoom, declared_layerless }
}

/// [`raster_plan`]'s opacity half, on its own -- kept separate because
/// `MapView::raster_opacity_for_frame` is asked twice a frame (once
/// through `raster_tiles_for_frame`, once directly in `paint`) and
/// neither call has a cover zoom to spend: only [`raster_plan`], which a
/// cover computation actually needs, pays for [`raster_cover_zoom`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RasterOpacity {
    /// See [`RasterPlan::opacity`].
    pub opacity: Option<f32>,
    /// See [`RasterPlan::declared_layerless`].
    pub declared_layerless: bool,
}

/// The `raster-opacity` this frame draws imagery with, given a raster
/// source IS set -- the caller has already answered that question (see
/// [`raster_plan`]'s `has_source` gate).
///
/// Three outcomes, verbatim `raster_opacity_for_frame`'s (`src/map_view.rs`):
///
///  - **A `StyleLayer::Raster` whose zoom range includes `display_zoom`.**
///    That layer's `raster-opacity`, resolved at `display_zoom` exactly as
///    every other zoom-dependent paint value is, and clamped to `[0, 1]`.
///  - **A raster layer present, but this zoom is outside every one of
///    them.** `None` -- a style like `examples/liberty-style.json`, whose
///    Natural Earth relief stops at zoom 6, must not have the engine draw
///    it past that just because a source happens to be set.
///  - **No raster layer declared at all.** `Some(1.0)` -- the style-spec
///    default -- with `declared_layerless: true` so the caller can print
///    its one-time notice. The alternative (refuse to draw) fails
///    silently in the direction nobody can diagnose.
///
/// The zoom gate is `zoom < min_zoom || zoom > max_zoom`, character for
/// character `resolve_plan`'s own gate for every other layer kind.
/// `style::layer_zoom_fade`'s one-level appear/disappear ramp is
/// deliberately NOT applied -- see `raster_opacity_for_frame`'s doc
/// comment for why a raster layer has no vertex-colour pair to ride it on.
#[must_use]
pub fn raster_opacity(display_zoom: u8, layers: &[StyleLayer]) -> RasterOpacity {
    let mut declared_any = false;
    for layer in layers {
        let StyleLayer::Raster { opacity, min_zoom, max_zoom, .. } = layer else { continue };
        declared_any = true;
        if min_zoom.is_some_and(|min| display_zoom < min) || max_zoom.is_some_and(|max| display_zoom > max) {
            continue;
        }
        return RasterOpacity {
            opacity: Some(opacity.value_at(display_zoom).clamp(0.0, 1.0)),
            declared_layerless: false,
        };
    }
    if declared_any {
        // Every raster layer in the style says "not at this zoom".
        return RasterOpacity { opacity: None, declared_layerless: false };
    }
    RasterOpacity { opacity: Some(1.0), declared_layerless: true }
}

/// The imagery cover zoom for a camera at `camera_zoom`, given a source
/// whose tiles are `tile_size` px square and whose ceiling is `max_zoom`.
///
/// Verbatim `raster_cover_zoom` (`src/map_view.rs`), moved rather than
/// rewritten. Two corrections on top of the camera's own zoom, both
/// folded into one `round`:
///
///  - **Tile size.** A 256px image stretched over a 512px slot is half
///    the linear resolution. `log2(512 / tile_size)` is the correction,
///    and it is exactly `coveringZoomLevel`'s
///    `scaleZoom(transform.tileSize / options.tileSize)`
///    (`covering_tiles.ts:163-169`, MapLibre GL JS, BSD-3).
///  - **Device pixel ratio.** A 512-logical-pixel slot is `512 * scale`
///    PHYSICAL pixels, and the surface is configured in physical pixels.
///
/// **`round`, not `floor`.** `visible_tiles` floors, and floor is right
/// for vector geometry, which re-tessellates at any scale. An image
/// has fixed pixels and magnifying it is detail that cannot come back,
/// which is why raster keeps a cover zoom of its own rather than sharing
/// `visible_tiles`' vector one -- MapLibre's `roundZoom`, set for its
/// raster source and not its vector one (`covering_tiles.ts:164`). **Do
/// not unify this with the vector cover's `floor`** -- see this crate's
/// `docs/superpowers/specs/2026-09-22-frame-core-extraction-design.md`
/// and the raster task's own hard constraints for why the asymmetry is
/// deliberate, not an oversight waiting to be cleaned up.
#[must_use]
pub fn raster_cover_zoom(camera_zoom: f64, tile_size: u32, scale_factor: f32, max_zoom: u8) -> u8 {
    if tile_size == 0 {
        return 0;
    }
    let scale = if scale_factor.is_finite() && scale_factor > 0.0 { f64::from(scale_factor) } else { 1.0 };
    let correction = (crate::geo::TILE_SIZE * scale / f64::from(tile_size)).log2();
    let ideal = (camera_zoom + correction).round();
    ideal.clamp(0.0, f64::from(max_zoom)) as u8
}

/// [`raster_cover_zoom`] as a `cover_plan::flat_cover` request: the same
/// zoom, plus the unrounded centre the imagery cover's distance LOD is
/// taken about (`camera_zoom` plus the same correction), rounded per tile.
/// For a shell that has no [`RasterPlan`] (native's
/// `raster_tiles_and_reach_for_frame`).
#[must_use]
pub fn raster_cover_request(
    camera_zoom: f64,
    tile_size: u32,
    scale_factor: f32,
    max_zoom: u8,
) -> crate::render::cover_plan::CoverZoom {
    let zoom = raster_cover_zoom(camera_zoom, tile_size, scale_factor, max_zoom);
    let scale = if scale_factor.is_finite() && scale_factor > 0.0 { f64::from(scale_factor) } else { 1.0 };
    let correction = if tile_size == 0 { 0.0 } else { (crate::geo::TILE_SIZE * scale / f64::from(tile_size)).log2() };
    crate::render::cover_plan::CoverZoom { zoom, center_zoom: Some(camera_zoom + correction), round: true, min_zoom: 0, building_floor: None, cutoff: None }
}

/// How many texels of a raster tile land on one physical screen pixel at
/// this camera. **1.0 is correct**; 2.0 is a 2x magnification, 0.5 is
/// downsampling. Verbatim `raster_texels_per_physical_pixel`
/// (`src/map_view.rs`), moved -- the debug instrument
/// [`raster_cover_zoom`] exists to keep close to 1.0, kept beside it so a
/// shell that wants the same diagnostic (see `MapView::paint`'s "imagery
/// scale" notice) does not have to re-derive the arithmetic it checks.
#[must_use]
pub fn raster_texels_per_physical_pixel(
    camera_zoom: f64,
    cover_zoom: u8,
    tile_size: u32,
    scale_factor: f32,
) -> f64 {
    let scale = if scale_factor.is_finite() && scale_factor > 0.0 { f64::from(scale_factor) } else { 1.0 };
    let physical_span = crate::geo::TILE_SIZE * scale * 2f64.powf(camera_zoom - f64::from(cover_zoom));
    if physical_span <= 0.0 {
        return 0.0;
    }
    f64::from(tile_size) / physical_span
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::ZoomValue;

    fn raster_layer(min_zoom: Option<u8>, max_zoom: Option<u8>, opacity: f32) -> StyleLayer {
        StyleLayer::Raster {
            id: "imagery".to_string(),
            source: "satellite".to_string(),
            opacity: ZoomValue::constant(opacity),
            min_zoom,
            max_zoom,
        }
    }

    /// No raster source at all: `RasterPlan::NONE`, and nothing else is
    /// even read -- this is the free case every map with no imagery pays.
    #[test]
    fn no_source_is_none_regardless_of_the_style() {
        let layers = [raster_layer(None, None, 1.0)];
        let plan = raster_plan(false, 10, 10.0, 256, 1.0, 22, &layers);
        assert_eq!(plan, RasterPlan::NONE);
    }

    /// A source and a matching layer: the layer's own opacity, and a
    /// cover zoom actually computed (not the `NONE` default of 0).
    #[test]
    fn a_source_with_a_matching_layer_draws_at_its_opacity() {
        let layers = [raster_layer(Some(0), Some(20), 0.75)];
        let plan = raster_plan(true, 10, 10.0, 256, 1.0, 22, &layers);
        assert_eq!(plan.opacity, Some(0.75));
        assert!(!plan.declared_layerless);
        // `tile_size == TILE_SIZE / 2` (256 vs 512) asks for one zoom
        // level deeper -- see `raster_cover_zoom`'s own doc comment.
        assert_eq!(plan.cover_zoom, 11);
    }

    /// The zoom gate: a raster layer present, but this frame's zoom is
    /// outside every one of them -- `None`, not the style-spec default.
    #[test]
    fn a_layer_present_but_out_of_zoom_range_draws_nothing() {
        let layers = [raster_layer(Some(0), Some(6), 1.0)];
        let plan = raster_plan(true, 14, 14.0, 256, 1.0, 22, &layers);
        assert_eq!(plan.opacity, None);
        assert_eq!(plan.cover_zoom, 0, "no cover is asked for when nothing will draw");
        assert!(!plan.declared_layerless, "the layer WAS declared; this is not the layerless case");
    }

    /// A raster source with no raster layer at all: the style-spec
    /// default opacity, and the flag that tells the caller to print its
    /// one-time notice.
    #[test]
    fn a_source_with_no_raster_layer_draws_at_the_default_opacity_and_flags_it() {
        let layers: [StyleLayer; 0] = [];
        let plan = raster_plan(true, 10, 10.0, 512, 1.0, 22, &layers);
        assert_eq!(plan.opacity, Some(1.0));
        assert!(plan.declared_layerless);
    }

    /// `raster_plan` is a pure function of its inputs: two calls with the
    /// same arguments return the same `RasterPlan`, at every combination
    /// a frame can actually be at -- the same property
    /// `space_plan_is_a_pure_function_of_its_inputs` pins for `space_plan`.
    #[test]
    fn raster_plan_is_a_pure_function_of_its_inputs() {
        let layers = [raster_layer(Some(0), Some(20), 0.5)];
        for zoom in [0u8, 6, 11, 20] {
            let a = raster_plan(true, zoom, f64::from(zoom), 256, 2.0, 22, &layers);
            let b = raster_plan(true, zoom, f64::from(zoom), 256, 2.0, 22, &layers);
            assert_eq!(a, b, "raster_plan returned different values for identical inputs at zoom {zoom}");
        }
    }

    /// `raster_cover_zoom` rounds; the vector cover floors. Pinned here so
    /// the two cannot be quietly unified -- see this module's own doc
    /// comment and the raster task's hard constraints.
    #[test]
    fn raster_cover_zoom_rounds_rather_than_floors() {
        // camera zoom 12.9 at the vector tile size (512): floor would give
        // 12, but raster wants the nearest integer zoom, which is 13.
        assert_eq!(raster_cover_zoom(12.9, 512, 1.0, 22), 13);
        assert_eq!(raster_cover_zoom(12.4, 512, 1.0, 22), 12);
    }

    /// `raster_cover_zoom` clamps to the source's own ceiling -- imagery
    /// above it is magnified rather than requested at a level that does
    /// not exist.
    #[test]
    fn raster_cover_zoom_clamps_to_the_source_ceiling() {
        assert_eq!(raster_cover_zoom(25.0, 512, 1.0, 14), 14);
    }

    /// A tile size of zero cannot happen through any real configuration,
    /// but must not divide-by-zero or panic either -- the honest answer
    /// is "no zoom is correct", zoom 0.
    #[test]
    fn raster_cover_zoom_of_a_zero_tile_size_does_not_panic() {
        assert_eq!(raster_cover_zoom(10.0, 0, 1.0, 22), 0);
    }

    /// The instrument [`raster_cover_zoom`] is built to keep near 1.0: a
    /// cover taken exactly at the camera's own zoom, at the vector tile
    /// size, samples one texel per physical pixel.
    #[test]
    fn texels_per_physical_pixel_is_one_at_the_cameras_own_zoom_and_the_vector_tile_size() {
        let ratio = raster_texels_per_physical_pixel(10.0, 10, 512, 1.0);
        assert!((ratio - 1.0).abs() < 1e-9, "expected 1.0, got {ratio}");
    }
}

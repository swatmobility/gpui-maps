//! Placing, shaping and drawing a map's labels -- the half of the symbol
//! pass that has nothing to do with a window.
//!
//! # Why this is its own file
//!
//! Every function below used to live in `map_view.rs`, between the camera
//! helpers and gpui's `Element` impl, and each of them is plain arithmetic
//! over `wgpu`, `glyphon` and `crate::collision`. Not one touches `gpui`,
//! `Window`, `App`, `tokio`, `reqwest` or `objc2`. That was already true
//! when they were in `map_view.rs`; it was just not USABLE, because the
//! file around them cannot compile for `wasm32-unknown-unknown` and a
//! `#[path]` include takes the whole file or none of it.
//!
//! So the browser build (`web/src/lib.rs`) includes THIS file and gets the
//! same shaping, the same `text-anchor` arithmetic, the same halo
//! approximation and the same collision plumbing the native build runs --
//! not a second implementation of them. A label that moves on macOS moves
//! in Chrome, because it is one `text_area_origin` and not two.
//!
//! # What is here, in the order the frame uses it
//!
//! 1. **Project and cull.** [`AnchorProjection`] takes a `SymbolFeature`'s
//!    tile-space anchor to a logical screen pixel -- through the tile's own
//!    matrix on a flat map, and over the sphere on the globe;
//!    [`SYMBOL_CULL_MARGIN_PX`] says how far outside the viewport a symbol
//!    may sit and still be considered, and [`AnchorProjection::occluded`]
//!    says whether the planet is in front of it.
//! 2. **Shape and measure.** [`LabelShapeCache`] holds one `glyphon::Buffer`
//!    per (text, size, wrap width, justification, face) and re-shapes only
//!    on a miss; [`measure_shaped_block`] reads the block's width, height
//!    and first baseline back out of it.
//! 3. **Position.** [`text_area_origin`] turns the anchor, the layer's
//!    `text-offset`/`text-anchor` and the measured block into the physical
//!    top-left `glyphon::TextArea` wants, snapped to whole pixels.
//! 4. **Box and place.** [`text_collision_box`]/[`icon_collision_box`] feed
//!    `collision::symbol_box`, and [`place_visible_symbols`] runs the whole
//!    candidate list through a `CollisionIndex`. One thing is decided there
//!    that geometry cannot decide: [`CrossLayerTextVeto`] drops a point
//!    label that merely repeats one another LAYER already placed at the same
//!    place, which is what stops a city-state writing its name twice.
//! 5. **Order and clip.** [`sort_symbols_for_drawing`] puts the survivors in
//!    style-layer order and [`clip_labels_behind_later_icons`] takes the
//!    covered part of a label away from it.
//! 6. **Halo.** [`halo_offsets`] synthesises an outline out of repeated
//!    draws, because cosmic-text has no outline primitive.
//!
//! # The other text path: line placement
//!
//! Steps 2-3 and 6 above describe a POINT label, which is drawn by
//! `glyphon::TextArea`. A `symbol-placement: line` label whose
//! `layout.text-rotation-alignment` is `map` does not go that way, and
//! cannot: a `TextArea` has a position and no rotation, so the per-glyph
//! angle `symbol_line` computes could never reach it, and a street name
//! laid out along its road was then drawn horizontally across it.
//! [`line_glyph_quads`] draws one ROTATED QUAD per glyph instead, out of
//! `render::GlyphAtlas`, through `IconPipeline`'s existing SDF branch --
//! which is also where the halo those layers ask for comes from, as a real
//! distance threshold rather than [`halo_offsets`]' four offset copies.
//! The two paths are mutually exclusive per symbol; see [`GlyphRunDraw`].
//!
//! `text-rotation-alignment: viewport` is the third case, and it is line
//! placement that takes the POINT path: the road's direction is not an
//! input, so the label is one horizontal block at the line anchor. Every
//! `highway-name-*` layer is `map` and every `highway-shield-*` layer is
//! `viewport`, which is why the shields are the ones that care. See
//! [`text_rotation_alignment`].
//!
//! [`basemap_candidate`] is steps 1-4 for one `style::SymbolFeature`, and
//! [`collect_candidates`] is the per-tile loop around it that both
//! frontends call.
//!
//! # Where the tests are
//!
//! Still in `map_view.rs`'s `mod tests`, which reaches them through that
//! file's `use crate::symbol_pass::*`. They were not moved with the code
//! deliberately: moving 1,300 lines of assertions in the same change that
//! moved the code they assert on would have made the diff unreviewable, and
//! they exercise these functions exactly as well from there. [`label_census`]
//! is the end-to-end check that this file behaves the same in both builds --
//! `examples/label_parity.rs` runs it natively and the browser's
//! `map_labels_json()` prints the same fields.

use std::collections::{HashMap, HashSet};

use crate::collision::{place_symbols_vetoed, CollisionBox, CollisionIndex, PlacementCandidate};
use crate::geo::TileId;
use crate::mvt::MVT_EXTENT;
use crate::render::{Camera, IconInstance, IconRect};
use crate::style::{self, TextAnchor};

/// One tile's anchor projection for one frame: the tile-to-clip matrix the
/// flat map has always used, and -- on the globe -- the sphere half beside
/// it.
///
/// # Why this is a value and not two arguments
///
/// Both halves are loop-invariant over a tile's features, and both are
/// expensive to rebuild: `combined_tile_matrix` is an f64 perspective +
/// look-at construction, and the globe half would otherwise re-derive the
/// frame's globe matrix and horizon plane. Holding them in one value built
/// once per tile is what stops a per-feature loop rebuilding either -- the
/// same argument `render::globe::GlobeUpload` carries for the GPU's half,
/// and the same defect (`globe_transform.ts:284-295`, both sub-transforms
/// rebuilt per tile per frame) it exists to keep out.
///
/// It also means a caller cannot *accidentally* project flat on a globe
/// frame: there is no `&glam::Mat4` to pass any more, and every construction
/// site has to say which `GlobeAnchors` it is projecting under. That is why
/// [`Self::new`] takes the globe rather than defaulting to flat.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AnchorProjection {
    /// The tile the anchors are in: what label occlusion places them by.
    tile: TileId,
    /// Tile-local (pre-scaled x1000) MVT coordinates to clip space --
    /// `render::combined_tile_matrix`, unchanged, and the ONLY thing
    /// consulted when the globe is off.
    tile_to_clip: glam::Mat4,
    /// `None` whenever `transition == 0.0`, which is every frame above
    /// `render::globe::PROJECTION_BLEND_ZOOM_MAX`. That `None` is what makes the flat
    /// path bit-identical rather than merely intended to be unchanged: the
    /// globe arm is not entered, not multiplied by zero.
    globe: Option<crate::render::globe::TileAnchors>,
}

impl AnchorProjection {
    /// This frame's projection for one tile.
    #[must_use]
    pub(crate) fn new(
        camera: &Camera,
        tile: TileId,
        globe: &crate::render::globe::GlobeAnchors,
    ) -> Self {
        Self {
            tile,
            tile_to_clip: crate::render::combined_tile_matrix(camera, tile, MVT_EXTENT as f32),
            globe: globe.for_tile(tile, f64::from(MVT_EXTENT)),
        }
    }

    /// This frame's projection for one tile with the globe off -- what every
    /// frame above the globe band gets, and what a test that is not about the
    /// globe should use.
    ///
    /// `#[cfg(test)]` because production never spells the flat case out: a
    /// frame passes the `GlobeAnchors` it has, and above the band that value
    /// IS `FLAT` already. Leaving it compiled into the shipped library would
    /// be one more dead-code warning in a build that carries 23 and counts
    /// them.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn flat(camera: &Camera, tile: TileId) -> Self {
        Self::new(camera, tile, &crate::render::globe::GlobeAnchors::FLAT)
    }

    /// Projects a symbol's tile-local anchor point to a logical window pixel,
    /// through the shared clip-to-screen mapping (`project_clip_to_screen`,
    /// which returns `None` for a point behind the near plane).
    ///
    /// On the globe the anchor goes to the sphere first
    /// (`globe::TileAnchors::sphere`) and the two clip positions are blended
    /// by the frame's globeness, exactly as `shader.wgsl`'s
    /// `interpolate_projection` blends the vertex beneath it -- so a label
    /// travels with its own feature through the whole transition instead of
    /// snapping onto the sphere at one end.
    ///
    /// **Says nothing about the far side.** A point behind the planet
    /// projects to a perfectly ordinary pixel, which is the entire reason a
    /// city on the far hemisphere used to be drawn: see [`Self::occluded`].
    /// # The horizontal flip, which is NOT applied to both halves
    ///
    /// `shader.wgsl`'s `vs_main` writes `clip.x = -clip.x` **before** the
    /// blend, on the Mercator position alone: the flip undoes a property of
    /// `Camera::eye_target_up`'s basis (world +x lands on NDC -x there), and
    /// `globe_frame` builds its matrix from scratch instead of reusing that
    /// basis, so the sphere's +x is already screen-right. Flipping the
    /// blended result would mirror the globe east for west.
    ///
    /// `project_clip_to_screen`, though, applies the flip itself, because
    /// every caller it had before this one handed it an *unflipped* Mercator
    /// clip. So the globe arm hands it `-clip.x` and lets it flip that back
    /// -- rather than growing a second copy of the NDC-to-window mapping
    /// here, which is the arithmetic most worth having exactly one of.
    ///
    /// The first draft of this function blended the unflipped flat clip and
    /// let `project_clip_to_screen` flip the lot, and every label came out
    /// mirrored about the central meridian -- a defect invisible in any test
    /// whose prediction was written the same way, which is why
    /// `render::gpu`'s
    /// `a_symbol_anchor_lands_on_the_tile_that_owns_it_on_the_globe` reads
    /// real rendered pixels instead.
    #[must_use]
    pub(crate) fn screen(&self, camera: &Camera, anchor: geo_types::Point<i32>) -> Option<(f32, f32)> {
        self.screen_at(camera, f64::from(anchor.x()), f64::from(anchor.y()))
    }

    /// [`Self::screen`] for a FRACTIONAL tile position -- the screen glyph
    /// walk's unrounded anchor and its one-unit near-plane probe. For an
    /// integer position it is bit-identical to `screen`: an `i32` reaches
    /// `f32` by the same rounding whether or not it passes through `f64`.
    #[must_use]
    pub(crate) fn screen_at(&self, camera: &Camera, x: f64, y: f64) -> Option<(f32, f32)> {
        let local = glam::Vec4::new(x as f32 * 1000.0, y as f32 * 1000.0, 0.0, 1.0);
        let flat_clip = self.tile_to_clip * local;
        let Some(globe) = &self.globe else {
            return crate::render::project_clip_to_screen(camera, flat_clip);
        };
        let sphere = globe.sphere((x, y));
        let mut flipped = flat_clip.as_dvec4();
        flipped.x = -flipped.x;
        let mut clip = globe.project(flipped, sphere);
        clip.x = -clip.x;
        crate::render::project_clip_to_screen(camera, clip.as_vec4())
    }

    /// How much bigger (> 1) or smaller (< 1) a symbol at `anchor` draws
    /// than one at the look-at point: see [`symbol_perspective_ratio`]. The
    /// anchor's clip `w` through `tile_to_clip` is MapLibre's
    /// `projectedPoint.w`, in the same world units as `Camera::distance`
    /// (its `cameraToCenterDistance`).
    ///
    /// Exactly `1.0` at pitch 0 by a BRANCH, not by arithmetic. Every
    /// anchor's depth equals the centre's there in exact maths but not in
    /// this f32 matrix, and an unpitched frame's labels, boxes and
    /// shape-cache keys must not move by one ULP. Also `1.0` on a globe
    /// frame, whose depth is not the flat matrix's.
    #[must_use]
    pub(crate) fn perspective_ratio(&self, camera: &Camera, anchor: geo_types::Point<i32>) -> f32 {
        if camera.pitch == 0.0 || self.globe.is_some() {
            return 1.0;
        }
        let local = glam::Vec4::new(anchor.x() as f32 * 1000.0, anchor.y() as f32 * 1000.0, 0.0, 1.0);
        perspective_ratio_at_depth(camera.distance() as f32, (self.tile_to_clip * local).w)
    }

    /// This anchor's clip `w` through the FLAT matrix -- the depth the
    /// horizon fade is a function of (`shader.wgsl`'s `fog_depth`), in the
    /// same world units as `Camera::distance`. On a globe frame it is
    /// still the depth the shaders fade by (their `fog_depth` is the flat
    /// clip `w`), which is all [`horizon_visibility`] asks of it.
    #[must_use]
    pub(crate) fn depth(&self, anchor: geo_types::Point<i32>) -> f32 {
        let local = glam::Vec4::new(anchor.x() as f32 * 1000.0, anchor.y() as f32 * 1000.0, 0.0, 1.0);
        (self.tile_to_clip * local).w
    }

    /// Whether the planet is between the camera and this anchor.
    ///
    /// Always `false` on a flat map -- there is nothing to hide behind, and
    /// `mercator_transform.ts:854`'s `isLocationOccluded` returns a hard
    /// `false` for the same reason.
    ///
    /// Needs no matrix, so it is a separate call from [`Self::screen`]: see
    /// `globe::TileAnchors::occluded`.
    #[must_use]
    pub(crate) fn occluded(&self, anchor: geo_types::Point<i32>) -> bool {
        self.occluded_at(f64::from(anchor.x()), f64::from(anchor.y()))
    }

    /// [`Self::occluded`] for a fractional tile position.
    #[must_use]
    pub(crate) fn occluded_at(&self, x: f64, y: f64) -> bool {
        self.globe.is_some_and(|globe| globe.occluded(globe.sphere((x, y))))
    }

    /// This anchor's point on the unit sphere, or `None` on a flat frame.
    ///
    /// The one thing `render::SymbolLight` needs, and the reason it is a
    /// method here rather than a field on the light: the light is per FRAME
    /// and the normal is per ANCHOR, and this type is the only thing in the
    /// symbol pass that knows how to get from a tile's MVT units to the
    /// sphere. Costs one `exp`, a `sin` and a `cos` — the same call
    /// [`Self::occluded`] already makes, and deliberately not shared with
    /// it: a symbol that is occluded never reaches the shading, and one
    /// that is not would otherwise carry a `DVec3` through a `Skipped`
    /// return for nothing.
    #[must_use]
    pub(crate) fn sphere(&self, anchor: geo_types::Point<i32>) -> Option<glam::DVec3> {
        self.globe.map(|globe| globe.sphere((f64::from(anchor.x()), f64::from(anchor.y()))))
    }
}

/// Perspective ratio steps per unit: see [`symbol_perspective_ratio`].
pub(crate) const PERSPECTIVE_RATIO_STEPS: f32 = 16.0;

/// MapLibre's symbol perspective ratio for a viewport-aligned label,
/// `clamp(0.5 + 0.5 * camera_to_center / camera_to_anchor, 0, 4)`
/// (MapLibre GL JS, BSD-3-Clause, (c) MapLibre contributors:
/// `src/shaders/glsl/symbol_sdf.vertex.glsl:91-108`, the
/// `!u_pitch_with_map` arm, and `src/symbol/projection.ts:205-207`'s
/// `getPerspectiveRatio`), quantised to 1/[`PERSPECTIVE_RATIO_STEPS`] so
/// that a moving camera does not re-shape every label on every frame (the
/// shaped-text cache is keyed by size). MapLibre needs no quantising
/// because it scales in the vertex shader; glyphon shapes on the CPU.
///
/// Only the viewport-pitched arm exists here: `text-pitch-alignment` is not
/// parsed, so no label is pitched with the map.
pub(crate) fn symbol_perspective_ratio(camera_to_center: f32, camera_to_anchor: f32) -> f32 {
    let ratio = (0.5 + 0.5 * camera_to_center / camera_to_anchor).clamp(0.0, 4.0);
    (ratio * PERSPECTIVE_RATIO_STEPS).round() / PERSPECTIVE_RATIO_STEPS
}

/// [`symbol_perspective_ratio`] for an anchor at clip depth `depth` (its
/// clip `w`), or exactly `1.0` when there is no ratio to take: an anchor
/// behind the eye (`w <= 0`; `project_clip_to_screen` returns `None` for
/// such a point, so a caller that projected first never gets here with
/// one, but the ratio does not rely on that) or a degenerate matrix (`w`
/// NaN or infinite).
pub(crate) fn perspective_ratio_at_depth(camera_to_center: f32, depth: f32) -> f32 {
    if !depth.is_finite() || depth <= 0.0 {
        return 1.0;
    }
    symbol_perspective_ratio(camera_to_center, depth)
}

/// The clockwise rotation, in radians, for a symbol at `anchor_screen`.
///
/// Both arguments are in y-down PHYSICAL screen pixels, the same space
/// `icon_shader.wgsl` rotates in, so no sign correction belongs anywhere
/// between here and the GPU. A `Viewport`-aligned symbol ignores the line's
/// direction and keeps only the style's own `icon-rotate` -- which is how a
/// shield stays upright on a curving road.
pub(crate) fn symbol_angle_rad(
    anchor_screen: (f32, f32),
    forward_screen: Option<(f32, f32)>,
    icon_rotate_deg: f32,
    alignment: crate::style::RotationAlignment,
) -> f32 {
    let along_line = match (forward_screen, alignment) {
        (Some(forward), crate::style::RotationAlignment::Map) => {
            (forward.1 - anchor_screen.1).atan2(forward.0 - anchor_screen.0)
        }
        _ => 0.0,
    };
    along_line + icon_rotate_deg.to_radians()
}

/// One line-placed GLYPH's rotation, for both the quad [`line_glyph_quads`]
/// draws and the collision box [`glyph_run_collision_box`] reserves for it
/// -- the two must agree, or the box a neighbour collides against does not
/// match what actually draws.
///
/// Deliberately NOT `symbol_angle_rad((anchor_x, anchor_y),
/// anchors.screen(camera, placement.forward), ...)`, even though that reads
/// as the obvious shape here: `placement.at` was once rounded to the nearest
/// tile unit (it no longer is -- see [`crate::symbol_line::GlyphAlong`] --
/// but a short segment still makes `at`-to-`forward` a poor lever), and taking the angle
/// from a rounded point to a nearby `forward` vertex turns that rounding
/// into a visible per-glyph rotation error whenever `at` lands close to
/// `forward` -- consecutive glyphs in the same or adjacent short segments
/// then tilt at measurably different angles even though the line itself is
/// straight. This is exactly the "Beach Road" defect a real device showed:
/// individual glyphs rotated along a curve the road does not have.
///
/// MapLibre does not have this failure mode because it never derives the
/// angle from the interpolated point at all: `placeGlyphAlongLine`
/// (`src/symbol/projection.ts:915` in `maplibre-gl-js`, BSD-3-Clause) computes
/// `segmentAngle = angle + Math.atan2(currentVertex.y - previousVertex.y,
/// currentVertex.x - previousVertex.x)` -- the segment's own two RAW
/// endpoint vertices, constant for every glyph placed within that segment.
/// `placement.segment_start`/`placement.segment_end` are that same pair
/// (see [`crate::symbol_line::LineAnchor::segment_start`]), so this takes
/// the angle from THEM instead of from `at`/`forward`.
///
/// `placement.at` is still what the glyph's quad (and its collision box) is
/// CENTRED on -- only the rotation changes source. `None` -- unrotated,
/// matching every other "could not project a rotation reference" fallback
/// in this file -- only when the segment's own start fails to project,
/// which needs a pitched camera and a segment endpoint behind the near
/// plane; such a glyph is at the very edge of the screen regardless.
fn line_glyph_angle(
    ctx: &SymbolFrame<'_>,
    anchors: &AnchorProjection,
    placement: &crate::symbol_line::GlyphAlong,
    alignment: crate::style::RotationAlignment,
) -> f32 {
    let Some(start) = anchors.screen(ctx.camera, placement.segment_start) else { return 0.0 };
    let end = anchors.screen(ctx.camera, placement.segment_end);
    symbol_angle_rad(start, end, 0.0, alignment)
}

/// Whether an OVERLAY symbol skips collision placement -- both halves of
/// it, together.
///
/// Either flag is enough, and it decides the whole billboard. Honouring the
/// icon and text flags independently is what a style engine does, and it
/// produced the one artifact a stop pin must never show: an icon that
/// bypassed collision while its label lost the placement contest left an
/// EMPTY BADGE, a pin marking a stop with no number on it.
///
/// The basemap's own rule is different (`symbol_bypasses_collision`) and
/// deliberately so: there, a text-bearing feature is excluded from the
/// bypass entirely, because every vendored layer that sets the property has
/// `text-field: null` and the bypass path can only carry icons.
pub(crate) fn overlay_bypasses_collision(icon_allow_overlap: bool, text_allow_overlap: bool) -> bool {
    icon_allow_overlap || text_allow_overlap
}

/// One LINE-PLACED overlay icon (`MapView::declare_source_symbols`): its
/// quad and collision box, centred on the anchor's screen position
/// (logical px) and turned `rotation_rad` clockwise -- the projected
/// direction of the line, from [`symbol_angle_rad`] with
/// `RotationAlignment::Map`, as a basemap one-way arrow is. `None` when the
/// sheet does not carry the sprite, or `icon_size` is 0.
///
/// An SDF sprite is drawn in the overlay's default icon colours: the v1
/// layout has no `icon-color`.
pub(crate) fn line_symbol_icon(
    rect: &IconRect,
    screen: (f32, f32),
    rotation_rad: f32,
    icon_size: f32,
    scale_factor: f32,
    icon_atlas_size: (u32, u32),
) -> Option<(IconInstance, CollisionBox)> {
    if icon_size <= 0.0 || icon_atlas_size.0 == 0 || icon_atlas_size.1 == 0 {
        return None;
    }
    let (logical_width, logical_height) = icon_logical_size(rect, icon_size);
    let (width, height) = (logical_width * scale_factor, logical_height * scale_factor);
    let (centre_x, centre_y) = (screen.0 * scale_factor, screen.1 * scale_factor);
    let (atlas_w, atlas_h) = (icon_atlas_size.0 as f32, icon_atlas_size.1 as f32);
    let defaults = crate::overlay::FeaturePaint::default();
    let sdf = sdf_paint_for(
        rect,
        icon_font_scale(rect, scale_factor),
        scale_factor,
        defaults.icon_color,
        defaults.icon_halo_color,
        defaults.icon_halo_width,
    );
    let icon = IconInstance {
        screen_x: centre_x - width / 2.0,
        screen_y: centre_y - height / 2.0,
        width,
        height,
        uv_min: (rect.x as f32 / atlas_w, rect.y as f32 / atlas_h),
        uv_max: ((rect.x + rect.width) as f32 / atlas_w, (rect.y + rect.height) as f32 / atlas_h),
        rotation_rad,
        sdf,
        opacity: 1.0,
    };
    Some((icon, icon_collision_box(width, height, rotation_rad, centre_x, centre_y)))
}

/// Whether a symbol should skip ordinary collision placement and be drawn
/// straight into `bypassed_icons` instead, given the style's own
/// `bypass_collision` flag and whether this particular feature resolved a
/// drawable label.
///
/// `bypass_collision` compiles from `icon-allow-overlap` &&
/// `icon-ignore-placement`, which are ICON-scoped style properties -- every
/// layer that sets them in both vendored styles has `text-field: null`. A
/// feature that also carries text is therefore always routed through the
/// ordinary candidate path instead, even when `bypass_collision` is set:
/// bypassing here would force a LABEL onto the screen using a flag that
/// never meant to promise that, and (the bug an earlier version of this
/// function's call site had) skipping the candidate push for such a feature
/// silently dropped its text entirely whenever it had no icon to carry it
/// out through `bypassed_icons`.
pub(crate) fn symbol_bypasses_collision(bypass_collision: bool, has_text: bool) -> bool {
    bypass_collision && !has_text
}

/// The `(width, height)` an icon occupies in LOGICAL points, given its
/// packed atlas rect and the style's resolved `icon-size` factor.
///
/// `rect.width`/`height` are the sprite sheet's own pixels, which are NOT
/// logical points: this crate fetches the `@2x` sheet (see
/// `sprite::sprite_urls`), whose entries declare `"pixelRatio": 2` and
/// store a 16-point icon as 32x32 pixels. Dividing the ratio back out is
/// what makes those 32 texels land on the 32 physical pixels a 16-point
/// icon covers at a 2x window scale factor -- 1:1, so `FilterMode::Linear`
/// is effectively an exact copy. Skipping the division (the original bug)
/// treated 32 as 32 points and stretched the icon over 64 physical pixels.
///
/// `pixel_ratio` arrives from a manifest fetched over the network, so a
/// zero, negative, or non-finite value is reachable without a bug on our
/// side; those fall back to 1x rather than pushing an infinite or NaN quad
/// size into the vertex buffer, which would corrupt the whole icon draw
/// instead of just mis-sizing one icon.
/// The `icon-size` multiplier to draw with THIS frame: the feature's baked
/// bracket walked by `t`, the camera's position inside the zoom bucket the
/// plan was baked at (`MapView::tile_blend_t`, the very interpolant the
/// paint side's `_from`/`_to` blend already uses).
///
/// Real Mapbox sizes a symbol at the map's CONTINUOUS zoom every frame --
/// `symbol_size.ts`'s `evaluateSizeForZoom` interpolates between the stops
/// covering `[tileZoom, tileZoom + 1]` and `draw_symbol.ts` feeds the result
/// to the shader -- so an icon grows smoothly as the camera moves through a
/// zoom level. Resolving at the bucket's integer floor alone froze the icon
/// for a whole level and then stepped it, and since these curves grow with
/// zoom, left it undersized everywhere but the floor: for Mapbox Standard's
/// `road` layer (`["interpolate", ["exponential", 1.5], ["zoom"], 14, 0.122,
/// 18, 0.969, 20, 1]`) that is 0.617 instead of 0.938 just below z18, a
/// third too small.
///
/// A constant or `step` `icon-size` bakes both ends equal (see
/// `style::LayoutValue::resolve_pair`), so this is exactly a no-op on the
/// expressions real Mapbox also refuses to interpolate.
pub(crate) fn blended_icon_size(from: f32, to: f32, t: f32) -> f32 {
    from + (to - from) * t
}

pub(crate) fn icon_logical_size(rect: &IconRect, icon_size: f32) -> (f32, f32) {
    let pixel_ratio = if rect.pixel_ratio.is_finite() && rect.pixel_ratio > 0.0 { rect.pixel_ratio } else { 1.0 };
    (
        rect.width as f32 / pixel_ratio * icon_size,
        rect.height as f32 / pixel_ratio * icon_size,
    )
}

/// The colours to draw an icon quad with, or `None` when the sprite is a
/// plain RGBA image that carries its own.
///
/// The style spec's own rule, not a simplification of it: `icon-color`,
/// `icon-halo-color` and `icon-halo-width` apply ONLY to an SDF sprite. A
/// normal sprite ignores all three, which is why the manifest's `sdf` flag
/// -- and not the presence of a colour in the style -- is what decides
/// here.
///
/// `physical_height` is the quad's drawn height, so `font_scale` comes out
/// in physical pixels per atlas texel and carries the window's scale factor
/// with it; see `SdfPaint::font_scale` for why that must NOT be applied a
/// second time.
pub(crate) fn sdf_paint_for(
    rect: &IconRect,
    font_scale: f32,
    scale_factor: f32,
    color: style::Rgba,
    halo_color: style::Rgba,
    halo_width_pt: f32,
) -> Option<crate::render::SdfPaint> {
    if !rect.sdf {
        return None;
    }
    Some(crate::render::SdfPaint {
        fill: [color.r, color.g, color.b, color.a],
        halo: [halo_color.r, halo_color.g, halo_color.b, halo_color.a],
        halo_width: halo_width_pt * scale_factor,
        font_scale,
    })
}

/// Physical pixels per atlas texel for a sprite drawn at its authored size
/// -- `SdfPaint::font_scale`.
///
/// Taken from the sprite's own `pixelRatio` and the window's scale factor
/// rather than from the quad being drawn, which matters once an icon is
/// STRETCHED: a stretched band's quad is longer than its source without the
/// distance field having changed, and measuring the scale from it would
/// widen that patch's border while leaving its neighbours' alone. The
/// unstretched parts set the scale; the bands sit in flat regions where the
/// difference does not show.
pub(crate) fn icon_font_scale(rect: &IconRect, scale_factor: f32) -> f32 {
    let ratio = if rect.pixel_ratio.is_finite() && rect.pixel_ratio > 0.0 { rect.pixel_ratio } else { 1.0 };
    scale_factor / ratio
}

/// The text an overlay symbol actually labels itself with: `text-field`,
/// unless it resolved to the empty string, which is no label at all.
///
/// MapLibre's semantics (`symbol_layout.ts`: `shapeText` returns `false`
/// for text with no positioned glyphs, so `shapedText` is undefined; the
/// feature is still added with its icon, and `fitIconToText` only runs
/// when there IS a default horizontal shaping). An empty label therefore
/// leaves the icon drawn at its own size, unfitted, with no text box to
/// collide or to clip -- which is what a cluster-label paint such as
/// `"{point_count_abbreviated}"` resolves to on every single, unclustered
/// point.
pub(crate) fn overlay_label_text(paint: &crate::overlay::FeaturePaint) -> Option<&str> {
    paint.text_field.as_deref().filter(|text| !text.is_empty())
}

/// One overlay symbol's two halves -- its icon quads and its label -- each
/// with its collision box. What the overlay loop in `MapView::paint` builds
/// per feature before it is placed; see [`overlay_symbol_parts`].
pub(crate) struct OverlaySymbolParts {
    pub(crate) icons: Vec<IconInstance>,
    pub(crate) icon_box: Option<CollisionBox>,
    pub(crate) text: Option<LabelDraw>,
    pub(crate) text_box: Option<CollisionBox>,
}

/// Builds one overlay feature's icon quads (stretched by `icon-text-fit`,
/// SDF-coloured where the sprite is a distance field) and its label draw,
/// at the feature's projected anchor `(screen_x, screen_y)` in LOGICAL
/// pixels. Everything is returned in physical pixels.
///
/// Extracted from `MapView::paint`'s overlay loop so the overlay path can
/// be driven -- and its quads rendered and read back -- without a window.
/// Placement (collision, bypass) stays in the loop.
#[allow(clippy::too_many_arguments)]
pub(crate) fn overlay_symbol_parts(
    paint: &crate::overlay::FeaturePaint,
    screen_x: f32,
    screen_y: f32,
    scale_factor: f32,
    icon_rects: &HashMap<String, IconRect>,
    icon_atlas_size: (u32, u32),
    icon_geometry: &HashMap<String, crate::sprite::IconGeometry>,
    font_system: &mut glyphon::FontSystem,
    shape_cache: &mut LabelShapeCache,
) -> OverlaySymbolParts {
    // Measured BEFORE the icon, because `icon-text-fit` sizes the
    // icon from the label rather than the other way round. The
    // shaping is cached, so asking early costs nothing.
    let physical_text_size = paint.text_size * scale_factor;
    let label_text = overlay_label_text(paint);
    let measured_label = label_text.map(|text| {
        let shaped = shape_cache.get_or_shape(
            font_system,
            text,
            physical_text_size,
            None,
            paint.text_justify,
            &chrome_text_face(),
        );
        (shaped.measured_width, shaped.measured_height, shaped.baseline)
    });
    // The badge is SIZED to the line box (or block of line boxes,
    // for a multi-line label) and the label is CENTRED by its ink.
    // Those are two different heights on purpose.
    //
    // Mapbox sizes from the line box -- `fitIconToText` takes
    // `shapedText.bottom - shapedText.top`, which for one line is
    // `ONE_EM(24) * lineHeight(1.2) * fontSize/24`, the line height
    // exactly, and for `n` lines is `n` line boxes -- exactly what
    // `measured_label`'s own `measured_height` (from
    // `measure_shaped_block`, real glyphon layout, not a per-line
    // constant) already gives the `fitted_size` call below. Sizing
    // to the ink instead makes the badge too short and turns a
    // two-digit pin into a wide, flat rectangle where mapbox-gl's is
    // nearly square -- and sizing a MULTI-line label to only one
    // line box corrupts it the same way (the actual bug this
    // multi-line-aware height fixes: a taller label used to overflow
    // a badge sized for its first line alone).
    //
    // Positioning is the opposite: a line box is not centred on the
    // glyphs inside it (the baseline sits about 0.81 of the way
    // down), so centring the BOX leaves the number riding high.
    let ink_height = physical_text_size * LABEL_CAP_HEIGHT_RATIO;

    let mut icon_draws: Vec<IconInstance> = Vec::new();
    let mut icon_box: Option<CollisionBox> = None;
    // The icon's own content box, AFTER stretching, as
    // `(left, top, right, bottom)` in physical pixels. `None` when
    // `icon-text-fit` is off, and the label then falls back to
    // hanging off the feature's point with its `text-offset`.
    let mut fitted_text_box: Option<(f32, f32, f32, f32)> = None;
    if let Some(icon_name) = &paint.icon_image {
        if icon_atlas_size.0 > 0 {
            if let Some(rect) = icon_rects.get(icon_name).copied() {
                // Image texels per PHYSICAL pixel. Working in
                // physical pixels throughout means the patches
                // `icon_fit` hands back are already in the units
                // `IconInstance` wants, with no second conversion to
                // get subtly wrong.
                let ratio = if rect.pixel_ratio.is_finite() && rect.pixel_ratio > 0.0 {
                    rect.pixel_ratio
                } else {
                    1.0
                };
                let texels_per_physical = ratio / scale_factor;
                let geometry = icon_geometry.get(icon_name);
                let fit = paint.icon_text_fit;
                // Padding arrives as Mapbox's [top, right, bottom,
                // left] and `fitted_size` wants [left, top, right,
                // bottom] -- the same order `content` uses. Getting
                // this wrong is invisible for symmetric padding and
                // silently wrong for any other.
                let [pad_top, pad_right, pad_bottom, pad_left] =
                    paint.icon_text_fit_padding;
                let padding = [
                    pad_left * scale_factor,
                    pad_top * scale_factor,
                    pad_right * scale_factor,
                    pad_bottom * scale_factor,
                ];
                let (physical_width, physical_height) = match measured_label {
                    Some(label) if fit != crate::overlay::paint::TextFit::None => {
                        // `label.1` is the REAL shaped block height
                        // (`measure_shaped_block`, already multi-
                        // line-aware -- see its own doc comment: for
                        // one line it reduces to exactly `fit_height`
                        // below, so no existing single-line badge
                        // moves). Using the hardcoded one-line
                        // `fit_height` constant here instead sized
                        // every badge for one line regardless of how
                        // many the label actually shaped to, which is
                        // what corrupted a multi-line label instead
                        // of drawing a taller badge for it.
                        crate::render::icon_fit::fitted_size(
                            (rect.width, rect.height),
                            texels_per_physical,
                            geometry.and_then(|g| g.content),
                            (label.0, label.1),
                            padding,
                            fit.fits_width(),
                            fit.fits_height(),
                        )
                    }
                    // `icon_size` is 1.0: an overlay icon draws at
                    // the sprite's native logical size.
                    _ => {
                        let (w, h) = icon_logical_size(&rect, 1.0);
                        (w * scale_factor, h * scale_factor)
                    }
                };
                // `text-offset` moves the ANCHOR POINT itself (ems
                // of the label's own font size -- the same
                // convention `text_area_origin` uses for the
                // no-icon-fit path, and the reason this multiplies
                // by `physical_text_size` rather than the icon's
                // own dimensions), same as real Mapbox GL/MapLibre:
                // `icon-text-fit` GROWS the icon to wrap wherever
                // the text lands, so the text's own offset has to
                // move the icon too, not just be applied a second
                // time to text drawn inside a box that never
                // moved. Previously missing here entirely -- a
                // `StopLabelPin` callout with a real `text-offset`
                // (anchored `bottom` at the SAME point as the stop
                // pin beneath it, both `icon-anchor: bottom`) drew
                // its badge right on top of the pin instead of
                // floating above it, live-verified on staging.
                // `icon-anchor` then shifts the icon's CENTRE away
                // from that (offset) point on top: a teardrop pin
                // wants `bottom` -- its tip on the point, its head
                // above -- where a symmetric badge wants the
                // default `center`. Applied to the FITTED size, so
                // a badge that grew still rests its tip on the
                // point rather than floating further off it.
                let (anchor_dx, anchor_dy) = paint.icon_anchor.centre_offset();
                let offset_x = paint.text_offset.0 * physical_text_size;
                let offset_y = paint.text_offset.1 * physical_text_size;
                let centre_x = screen_x * scale_factor + offset_x + anchor_dx * physical_width;
                let centre_y = screen_y * scale_factor + offset_y + anchor_dy * physical_height;
                let physical_left = centre_x - physical_width / 2.0;
                let physical_top = centre_y - physical_height / 2.0;
                let atlas_w = icon_atlas_size.0 as f32;
                let atlas_h = icon_atlas_size.1 as f32;
                // An overlay feature has no forward reference to
                // align along, so this cannot come from
                // `symbol_angle_rad` the way a basemap symbol's does
                // -- it comes from the caller, who is the only party
                // that knows a vehicle's heading. The `.to_radians()`
                // is the same conversion `symbol_angle_rad` applies to
                // the basemap's `icon_rotate_deg`; see
                // `FeaturePaint::icon_rotate_deg` for why the field is
                // in degrees.
                let rotation_rad = paint.icon_rotate_deg.to_radians();

                let empty: &[(u32, u32)] = &[];
                let columns = crate::render::icon_fit::axis_segments(
                    rect.width,
                    geometry.map_or(empty, |g| &g.stretch_x),
                    physical_width,
                    texels_per_physical,
                );
                let rows = crate::render::icon_fit::axis_segments(
                    rect.height,
                    geometry.map_or(empty, |g| &g.stretch_y),
                    physical_height,
                    texels_per_physical,
                );
                // The content box has to be located through the
                // SAME segments the icon was drawn with -- the
                // stretch bands move it, so neither the authored
                // box nor a fixed fraction of the fitted icon finds
                // it. See `icon_fit::map_along`.
                if let (Some(content), true) =
                    (geometry.and_then(|g| g.content), fit != crate::overlay::paint::TextFit::None)
                {
                    let left = crate::render::icon_fit::map_along(&columns, content[0] as f32);
                    let right = crate::render::icon_fit::map_along(&columns, content[2] as f32);
                    let top = crate::render::icon_fit::map_along(&rows, content[1] as f32);
                    let bottom = crate::render::icon_fit::map_along(&rows, content[3] as f32);
                    fitted_text_box = Some((
                        physical_left + left,
                        physical_top + top,
                        physical_left + right,
                        physical_top + bottom,
                    ));
                }

                let sdf = sdf_paint_for(
                    &rect,
                    icon_font_scale(&rect, scale_factor),
                    scale_factor,
                    paint.icon_color,
                    paint.icon_halo_color,
                    paint.icon_halo_width,
                );
                for patch in crate::render::icon_fit::patches(&columns, &rows) {
                    let (src_x, src_y, src_w, src_h) = patch.src;
                    let (dst_x, dst_y, dst_w, dst_h) = patch.dst;
                    icon_draws.push(IconInstance {
                        screen_x: physical_left + dst_x,
                        screen_y: physical_top + dst_y,
                        width: dst_w,
                        height: dst_h,
                        uv_min: (
                            (rect.x as f32 + src_x) / atlas_w,
                            (rect.y as f32 + src_y) / atlas_h,
                        ),
                        uv_max: (
                            (rect.x as f32 + src_x + src_w) / atlas_w,
                            (rect.y as f32 + src_y + src_h) / atlas_h,
                        ),
                        rotation_rad,
                        sdf,
                        // The overlay carries no `icon-opacity`: a
                        // caller's own feature paint has
                        // `icon_color`'s alpha for that, and
                        // nothing routes a style paint property here.
                        opacity: 1.0,
                    });
                }
                // The box follows the icon, not the feature's
                // point: an anchored icon occupies the space it is
                // DRAWN in, and a collision box left behind at the
                // anchor would reserve the wrong region -- letting
                // a label overlap the pin's head while guarding
                // empty ground under its tip.
                icon_box = Some(icon_collision_box(
                    physical_width,
                    physical_height,
                    rotation_rad,
                    centre_x,
                    centre_y,
                ));
            }
        }
    }

    let mut text_draw: Option<LabelDraw> = None;
    let mut text_box: Option<CollisionBox> = None;
    if let (Some(text), Some((measured_width, measured_height, label_baseline))) =
        (label_text, measured_label)
    {
        let (left, top) = match fitted_text_box {
            Some(fitted) => fitted_text_position(
                fitted,
                physical_text_size,
                ink_height,
                measured_width,
                measured_height,
                label_baseline,
            ),
            // Centred, always: an overlay marker's paint is this
            // crate's own small struct (`overlay::MarkerPaint`), not
            // a compiled style layer, and it has no `text-anchor` to
            // read. Written as the explicit style-spec default
            // rather than left implicit so that adding one later is
            // a change at this line, not a hunt for where the anchor
            // went missing.
            None => text_area_origin(
                screen_x,
                screen_y,
                scale_factor,
                LabelAnchoring { offset_em: paint.text_offset, anchor: paint.text_anchor },
                physical_text_size,
                measured_width,
                measured_height,
            ),
        };
        // The halo is scaled by `scale_factor` for the same reason
        // the text size is: `halo_offsets` works in PHYSICAL pixels,
        // so an unscaled width would draw a retina halo at half the
        // thickness a caller asked for. This mirrors what the
        // basemap loop does with its own style-driven halo width.
        text_draw = Some((
            LabelShapeCache::key(text, physical_text_size, None, paint.text_justify, &chrome_text_face()),
            left,
            top,
            paint.text_color,
            paint.text_halo_width * scale_factor,
            paint.text_halo_color,
            // A caller's overlay `FeaturePaint` has no halo blur to
            // read; its halo is the hard edge it has always been.
            0.0,
        ));
        text_box = Some(text_collision_box(left, top, measured_width, measured_height));
    }
    OverlaySymbolParts { icons: icon_draws, icon_box, text: text_draw, text_box }
}

/// The `TextArea` inputs pass 1 of the symbol loop records for one label:
/// `(shaped-buffer key, left, top, color, halo width, halo color, halo
/// blur)`, with
/// `left`/`top` already in physical pixels. Named because both the draw list
/// and `SymbolCandidate` carry it.
pub(crate) type LabelDraw = (LabelKey, f32, f32, style::Rgba, f32, style::Rgba, f32);

/// A LINE-placed label, drawn as one rotated quad per glyph out of
/// `render::GlyphAtlas` instead of as a `glyphon::TextArea`.
///
/// The two paths are mutually exclusive per symbol: a `SymbolCandidate`
/// carries either a [`LabelDraw`] or one of these, never both. `TextArea`
/// has a position and no rotation, so the angle
/// `symbol_line::place_glyphs_along` computes for each glyph cannot reach
/// it -- which is exactly why a street name used to be laid out along its
/// road and then drawn horizontally.
///
/// The text rides along with the quads because `LabelCensus::texts` is the
/// native-vs-browser parity instrument and a street name that stopped
/// travelling as a `LabelDraw` would silently drop out of it. It costs the
/// same one `String` per placed label that `LabelKey` already costs.
pub(crate) struct GlyphRunDraw {
    /// What these quads spell.
    pub(crate) text: String,
    /// One entry per glyph WITH INK, in reading order -- a space rasters to
    /// nothing and gets no quad (see `GlyphAtlas::get_or_raster`), so this
    /// is shorter than the label's glyph count whenever it has spaces.
    pub(crate) quads: Vec<IconInstance>,
}

/// One symbol that survived pass 1's projection and cull, offered to
/// collision placement.
///
/// It carries BOTH halves of the symbol's drawing -- the label and the icon
/// quad -- precisely so `place_visible_symbols` can drop them together. An
/// earlier shape of this code appended to the text list and the icon list
/// independently, which made it easy to filter one and forget the other and
/// leave a city dot on screen with no name attached.
pub(crate) struct SymbolCandidate {
    pub(crate) text: Option<LabelDraw>,
    /// This symbol's label as per-glyph rotated quads, for LINE placement.
    /// `None` for everything else, and never `Some` at the same time as
    /// [`Self::text`] -- see [`GlyphRunDraw`].
    pub(crate) glyphs: Option<GlyphRunDraw>,
    /// This symbol's own icon and label boxes, kept SEPARATE from
    /// `placement.bounds` (which is their union, padded, for collision).
    ///
    /// `hide_labels_behind_later_icons` needs the two apart: it asks
    /// whether a LATER symbol's opaque icon covers THIS symbol's label,
    /// which the union cannot answer.
    pub(crate) icon_box: Option<CollisionBox>,
    pub(crate) text_box: Option<CollisionBox>,
    /// EVERY quad this symbol's icon draws as. One for an ordinary icon;
    /// several for one stretched around its label, where each is a patch of
    /// the sprite (see `render::icon_fit`). A `Vec` rather than an `Option`
    /// so collision still drops the whole symbol -- text and every patch --
    /// as one unit.
    pub(crate) icons: Vec<IconInstance>,
    pub(crate) placement: PlacementCandidate,
    /// `overlay::paint::FeaturePaint::text_clip_behind_icons` -- `true` for
    /// every basemap symbol (compiled style layers carry no such property)
    /// and for an overlay feature that did not opt out. See that field's own
    /// doc comment for why an overlay feature ever would.
    pub(crate) text_clip_behind_icons: bool,
    /// Where this symbol's FEATURE sits on screen, in LOGICAL pixels -- the
    /// projected anchor, before `text-offset`, `text-anchor` or an icon
    /// moved the drawing off it. Read only by [`CrossLayerTextVeto`]; `None`
    /// means "not eligible for duplicate suppression".
    ///
    /// The anchor and not `text_box`'s centre because the question is
    /// whether two features are at the same PLACE, and two layers labelling
    /// one place hang their text off it differently (`label_country_*`
    /// centres on the point, `label_city_capital` sits above it).
    ///
    /// `None` for an OVERLAY symbol (two of a consumer's features carrying
    /// one string are two features), for a LINE-placed label (a street name
    /// repeats along its road by design), and for a symbol with no text.
    pub(crate) dedupe_anchor: Option<(f32, f32)>,
}

/// How near two point labels spelling the same thing have to be, anchor to
/// anchor, before the later one is read as a SECOND NAME FOR ONE PLACE:
/// **32 logical pixels**.
///
/// Measured on the case it exists for. OpenMapTiles' `place` layer carries
/// Singapore as `class=country` and as `class=city, capital=2` ~8 km apart,
/// which on screen doubles every zoom level: 0.8 px at z4, 13.6 px at z8 and
/// 27.1 px at z8.99, the last fraction before `label_country_*`'s `maxzoom`
/// of 9. The radius has to reach 27.1 or the rule quits exactly where the
/// doubling is most obvious.
///
/// What it must NOT reach is a city inside a same-named state: New York
/// city against New York state measure 86 px apart at z5 and 171 px at z6
/// (the state layer's whole band), and move apart from there. A city-state
/// is the one shape where the two anchors converge.
///
/// A SCREEN radius, not a ground one: 32 px is 156 km at z5 and 76 m at
/// z16, so the rule is permissive where two nodes land on one pixel and all
/// but inert once the camera is close enough to tell them apart.
pub(crate) const DUPLICATE_TEXT_RADIUS_LOGICAL_PX: f32 = 32.0;

/// Drops a point label that merely repeats one a DIFFERENT style layer has
/// already placed within [`DUPLICATE_TEXT_RADIUS_LOGICAL_PX`] of the same
/// anchor.
///
/// # The case
///
/// A city-state. OpenMapTiles carries Singapore as a country AND as a
/// capital city at essentially one point (and as an `island`, which
/// `label_other` draws from z8), and the styles draw each from its own
/// layer, so between z3 and z9 the map wrote "Singapore" two or three times
/// one above the other. Collision cannot cure it: the labels do not
/// overlap. They are correct answers to "what is here"; the defect is that
/// all of them are drawn.
///
/// # A deliberate divergence from the references
///
/// Neither MapLibre GL JS, MapLibre Native nor Organic Maps suppresses a
/// label because another LAYER drew that text. MapLibre's text-keyed dedupe
/// is per layer (`pauseable_placement.ts:10-27`, `placement.cpp:200-207`)
/// and exists for one symbol seen in a parent and a child tile; Organic Maps
/// keys on `FeatureID`. So this rule is ours, written as narrowly as the
/// case allows:
///
/// 1. **Across layers only.** Within one layer there is no stable way to
///    pick the survivor (no `symbol-sort-key`, so offer order -- tile walk
///    order -- decides), and a rule that swaps survivors as you pan is worse
///    than the doubled label.
/// 2. **Point labels on the basemap only** -- see
///    [`SymbolCandidate::dedupe_anchor`].
/// 3. **Byte-identical text.** No case folding, no similarity: "Singapore"
///    against "Singapore\n新加坡" are different labels and both draw.
///
/// # Which one survives
///
/// The one placement already ranks higher: the veto is asked in
/// `(layer_index DESC, sort_key ASC)` order, so the STYLE decides -- for the
/// OpenMapTiles styles, the country label. Explicitly not the larger glyph
/// run (the capital label is bigger at z4 and smaller from z6, so a size
/// rule would swap survivors mid-zoom). Above the country layers' `maxzoom`
/// the country label is not offered and the capital draws alone; the switch
/// is one-way and comes from the style's own zoom range, so it does not
/// flicker.
///
/// Only a label that actually PLACED suppresses another: one that lost its
/// own slot to collision leaves its duplicate free to draw.
struct CrossLayerTextVeto<'a> {
    /// One entry per candidate, in the caller's order: `(text, x, y,
    /// layer_index)`, or `None` for a candidate that can neither be
    /// suppressed nor suppress.
    entries: Vec<Option<(&'a str, f32, f32, usize)>>,
    /// The placed entries so far, bucketed by text: one hash and an almost
    /// always empty bucket per candidate, not a scan of every placed label.
    placed_by_text: HashMap<&'a str, Vec<(f32, f32, usize)>>,
}

impl<'a> CrossLayerTextVeto<'a> {
    fn new(candidates: &'a [SymbolCandidate]) -> Self {
        let entries = candidates
            .iter()
            .map(|candidate| {
                let (x, y) = candidate.dedupe_anchor?;
                let (key, ..) = candidate.text.as_ref()?;
                // An empty label is not a duplicate of the next empty one.
                if key.0.is_empty() {
                    return None;
                }
                Some((key.0.as_str(), x, y, candidate.placement.layer_index))
            })
            .collect();
        Self { entries, placed_by_text: HashMap::new() }
    }
}

impl crate::collision::PlacementVeto for CrossLayerTextVeto<'_> {
    fn rejects(&mut self, candidate: usize) -> bool {
        let Some((text, x, y, layer_index)) = self.entries[candidate] else {
            return false;
        };
        let Some(placed) = self.placed_by_text.get(text) else {
            return false;
        };
        let r2 = DUPLICATE_TEXT_RADIUS_LOGICAL_PX * DUPLICATE_TEXT_RADIUS_LOGICAL_PX;
        placed.iter().any(|&(px, py, placed_layer)| {
            // Narrowing 1: a same-layer twin is left alone.
            placed_layer != layer_index && (px - x).powi(2) + (py - y).powi(2) <= r2
        })
    }

    fn accepted(&mut self, candidate: usize) {
        if let Some((text, x, y, layer_index)) = self.entries[candidate] {
            self.placed_by_text.entry(text).or_default().push((x, y, layer_index));
        }
    }
}

/// Runs collision placement over one frame's symbol candidates and returns
/// the text and icon draw lists with every REJECTED symbol removed from
/// both.
///
/// Sits between pass 1 (project, cull, shape, measure) and pass 2 (build
/// `TextArea`s and `prepare`) of the symbol loop. Rejecting here rather than
/// at draw time means a dropped label costs no `TextArea`, no halo copies
/// and no atlas space -- so collision avoidance is a net REDUCTION in
/// per-frame work, not an addition to it.
///
/// Priority order and the greedy fill itself live in `crate::collision`;
/// this function is only the plumbing that keeps the two output lists in
/// step.
///
/// `index` is the CALLER's -- retained across frames on `GpuState` in
/// production (see `GpuState::collision_index`) and reset for exactly this
/// call so allocation-pooling discipline lives at the call site, not
/// duplicated into every place that needs a grid. The caller is expected to
/// have already called `index.reset(bounds)` (or handed in a fresh
/// `CollisionIndex::new(bounds)`); this function only places into it. That
/// precondition is checked on entry, because forgetting it fails SILENTLY:
/// last frame's boxes stay in the grid, this frame's labels collide with
/// ghosts, and the map simply draws fewer labels each frame with no panic
/// and no test failure to point at the cause. A `debug_assert` turns the
/// worst class of bug this signature admits into an immediate, named one.
///
/// The check is on PLACEMENTS, not on the index being empty: obstacles the
/// caller seeded this frame after `reset` (`CollisionIndex::insert_obstacle`,
/// the navigation route line) are meant to be there. Checking `is_empty`
/// instead aborted every debug build that drew a route with labels on.
pub(crate) fn place_visible_symbols(candidates: Vec<SymbolCandidate>, index: &mut CollisionIndex) -> Vec<DrawnSymbol> {
    debug_assert!(
        index.placement_count() == 0,
        "place_visible_symbols requires a cleared index: call index.reset(bounds) (see \
         symbol_collision_bounds) before this, or placements from the previous frame will \
         silently reject this frame's labels"
    );
    let placements: Vec<PlacementCandidate> = candidates.iter().map(|c| c.placement).collect();
    let placed = place_symbols_vetoed(index, &placements, &mut CrossLayerTextVeto::new(&candidates));
    let mut out = Vec::new();
    for (candidate, placed) in candidates.into_iter().zip(placed) {
        if !placed {
            continue;
        }
        out.push(DrawnSymbol {
            order: (candidate.placement.layer_index, candidate.placement.sort_key),
            icons: candidate.icons,
            icon_box: candidate.icon_box,
            text: candidate.text,
            glyphs: candidate.glyphs,
            text_box: candidate.text_box,
            text_clip: None,
            text_clip_behind_icons: candidate.text_clip_behind_icons,
        });
    }
    out
}

/// One symbol that survived placement (or bypassed it), with its halves
/// still attached to each other.
///
/// They stay together this far because the last thing done to them --
/// `hide_labels_behind_later_icons` -- is a question about the RELATION
/// between one symbol's label and another symbol's icon. Splitting them
/// into two lists at placement time, which is what this code did before,
/// throws away exactly the pairing that question needs.
pub(crate) struct DrawnSymbol {
    /// `(layer_index, symbol-sort-key)`: the draw order, ascending.
    pub(crate) order: (usize, f32),
    pub(crate) icons: Vec<IconInstance>,
    pub(crate) icon_box: Option<CollisionBox>,
    pub(crate) text: Option<LabelDraw>,
    /// See `SymbolCandidate`'s own field of the same name.
    ///
    /// Not touched by `clip_labels_behind_later_icons`: `text_clip` is a
    /// `glyphon::TextArea` bounds rectangle, and these quads do not go
    /// through glyphon. A line-placed street name is therefore not clipped
    /// where a later billboard covers it -- acceptable, and arguably
    /// right: the artifact that clipping exists to cure is a badge's own
    /// number floating over the pin in front of it, which a road name is
    /// not.
    pub(crate) glyphs: Option<GlyphRunDraw>,
    pub(crate) text_box: Option<CollisionBox>,
    /// The part of `text_box` no later billboard covers, once
    /// `clip_labels_behind_later_icons` has run. `None` before then, and
    /// for a symbol with no label.
    pub(crate) text_clip: Option<CollisionBox>,
    /// See `SymbolCandidate`'s own field of the same name.
    pub(crate) text_clip_behind_icons: bool,
}

/// Clips each label to the part of it a later billboard does NOT cover.
///
/// Icons and text are two pipelines, drawn one after the other, so every
/// label lands above every icon. Where symbols cannot overlap that is
/// invisible; where they are allowed to -- `icon-allow-overlap`, which stop
/// pins need or they vanish as the camera pulls back -- it shows as a
/// stop's number floating on top of the pin that is in front of it.
///
/// A pin and its number are one billboard, and a billboard that goes behind
/// another goes behind ALL of it: the part of it still in view keeps its
/// number, the part behind loses it. Hiding the whole label instead was
/// tried twice -- on any overlap, then on the label's centre -- and both
/// read as a badge sitting in plain view with nothing written on it, which
/// is the artifact this is meant to cure rather than a milder form of it.
///
/// The GPU cannot settle this for us. A depth buffer would, and glyphon
/// even takes a depth per glyph (`prepare_with_depth`), but it reads that
/// depth from the glyph's METADATA, which is baked into the shaped buffer
/// -- and buffers are cached by `(text, size)`, so every symbol showing
/// "10" shares one. Distinct depths would mean re-shaping per symbol, the
/// 4000x cost `LabelShapeCache` exists to avoid. What glyphon DOES offer
/// per text area is a clip rectangle, which is enough.
///
/// A rectangle cannot express "everything except that badge", so each
/// covering icon shrinks the clip along whichever edge loses the least
/// text. That is EXACT for the case that actually happens -- one neighbour
/// overlapping from one side -- and conservative when badges close in from
/// several, where it trims a little more than strictly necessary rather
/// than letting text through on top of a badge.
///
/// `symbols` must already be in draw order (ascending), which is what makes
/// "later" mean "in front".
///
/// A symbol whose own `text_clip_behind_icons` is `false` keeps its full
/// `text_box` regardless of what draws later -- see that field's own doc
/// comment (`overlay::paint::FeaturePaint`) for the badge-sized-around-its-
/// own-multi-line-label case this exists for. It opts a symbol's TEXT out of
/// being clipped; the symbol's own icon still blocks a neighbour that has
/// not opted out, unchanged.
pub(crate) fn clip_labels_behind_later_icons(symbols: &mut [DrawnSymbol]) {
    for i in 0..symbols.len() {
        let Some(text_box) = symbols[i].text_box else { continue };
        if symbols[i].text.is_none() {
            continue;
        }
        if !symbols[i].text_clip_behind_icons {
            symbols[i].text_clip = Some(text_box);
            continue;
        }
        let mut clip = text_box;
        for later in &symbols[i + 1..] {
            let Some(icon) = later.icon_box else { continue };
            if !icon.intersects(&clip) {
                continue;
            }
            clip = shrink_away_from(clip, icon);
            if clip.right <= clip.left || clip.bottom <= clip.top {
                break;
            }
        }
        symbols[i].text_clip = Some(clip);
    }
}

/// Trims `clip` back to the side of `blocker` that keeps the most of it.
///
/// The four candidates are the four ways one rectangle can be pushed out of
/// another's way. Whichever leaves the largest area wins; if none does --
/// `blocker` covers `clip` outright -- the result is empty, and an empty
/// clip draws nothing, which is correct for a billboard entirely behind
/// another.
pub(crate) fn shrink_away_from(clip: CollisionBox, blocker: CollisionBox) -> CollisionBox {
    let mut best = CollisionBox { left: 0.0, top: 0.0, right: 0.0, bottom: 0.0 };
    let mut best_area = 0.0;
    let candidates = [
        CollisionBox { right: clip.right.min(blocker.left), ..clip },
        CollisionBox { left: clip.left.max(blocker.right), ..clip },
        CollisionBox { bottom: clip.bottom.min(blocker.top), ..clip },
        CollisionBox { top: clip.top.max(blocker.bottom), ..clip },
    ];
    for candidate in candidates {
        let width = candidate.right - candidate.left;
        let height = candidate.bottom - candidate.top;
        if width <= 0.0 || height <= 0.0 {
            continue;
        }
        let area = width * height;
        if area > best_area {
            best_area = area;
            best = candidate;
        }
    }
    best
}

/// Puts symbols in Mapbox's own draw order: ascending `(layer_index,
/// symbol-sort-key)`, so an earlier layer draws underneath a later one and,
/// within a layer, the style spec's rule holds -- "features with lower sort
/// keys are drawn and placed first", i.e. lowest at the bottom.
///
/// Ordering the list IS the drawing order, because `IconPipeline::render`
/// issues one draw call and blending within a draw call follows primitive
/// order. That is what makes two overlapping SDF pins resolve cleanly:
/// each symbol contributes its halo and its fill adjacently, so the later
/// pin lays down both over the earlier one instead of its border blending
/// into the earlier one's face. Batching by pass -- every halo, then every
/// fill -- is precisely what leaves mapbox-gl unable to do this
/// (mapbox/mapbox-gl-js#11504).
///
/// It is also what makes "later" meaningful to
/// `hide_labels_behind_later_icons`, which runs immediately after.
///
/// `sort_by` is stable, so symbols that tie keep the order the symbol loop
/// produced them in, which is itself deterministic (tiles in visit order,
/// features in plan order). Nothing here depends on that being any
/// PARTICULAR order -- only on it being the same one every frame, so a
/// static map does not shimmer.
pub(crate) fn sort_symbols_for_drawing(symbols: &mut [DrawnSymbol]) {
    symbols.sort_by(|a, b| a.order.0.cmp(&b.order.0).then_with(|| a.order.1.total_cmp(&b.order.1)));
}

/// How far beyond the viewport, in LOGICAL pixels, a symbol's projected
/// anchor may fall and still be considered by the symbol loop.
///
/// It does double duty, and the two duties want the same number for the same
/// reason. It is the cull margin, so a label whose anchor is just off screen
/// but whose glyphs reach back onto it still draws. And it is the amount the
/// collision index's grid extends past the viewport
/// (`symbol_collision_bounds`), so labels enter and leave PLACEMENT off
/// screen rather than at the edge -- without that, panning would make a
/// label at the border win a slot the moment its anchor crossed the edge and
/// then shove an established neighbour out, in full view. This is exactly
/// MapLibre's `viewportPadding` in `collision_index.ts`, which is also 100,
/// and it is deliberately the same constant here rather than a second one
/// that happens to match.
pub(crate) const SYMBOL_CULL_MARGIN_PX: f32 = 100.0;

/// The line height every label buffer is shaped with, as a multiple of its
/// font size -- the `glyphon::Metrics` line height in
/// `LabelShapeCache::get_or_shape`.
///
/// A shaped label's real height is `ShapedLabel::measured_height`, read back
/// off the layout runs rather than computed from this; but the two agree
/// exactly (one line box per line), which is what lets the fallbacks here
/// and in `attribution_origin` -- both single-line by construction -- use
/// the ratio directly.
pub(crate) const LABEL_LINE_HEIGHT_RATIO: f32 = 1.2;

/// The height of a digit or capital, as a fraction of the font size.
///
/// This is the band a viewer actually SEES, and it is a good deal shorter
/// than the line box that contains it: the line box carries the ascender's
/// headroom above and the descender's depth below, neither of which a run
/// of digits uses. A badge sized and centred on the line box therefore
/// comes out too tall and top-heavy -- measured at 14px of white above the
/// digits against 10px below, on a badge whose interior was 41px for 17px
/// of ink.
///
/// 0.72 is the usual value for a UI sans (Inter, this crate's label face,
/// is 0.727). It is an approximation of the run's real ink box, which
/// glyphon does not expose: a run with a descender -- "Depot" has one --
/// sits marginally lower than perfectly centred, which the badge's own
/// padding absorbs.
pub(crate) const LABEL_CAP_HEIGHT_RATIO: f32 = 0.72;

/// The gap kept clear around every symbol's collision box, in LOGICAL
/// pixels, before it is scaled to physical.
///
/// This is Mapbox's `text-padding`, whose style-spec default is 2, applied
/// as a constant: the property itself is deliberately NOT compiled. No
/// vendored layer overrides it, and the value's job is legibility (two
/// labels that technically don't overlap but sit one pixel apart still read
/// as one smear), which is a renderer concern more than a style one.
pub(crate) const SYMBOL_COLLISION_PADDING_LOGICAL_PX: f32 = 2.0;

/// A piece of screen chrome (the credit's plate, the brand mark) at
/// `(x, y)`, `w`x`h` physical pixels, as a label obstacle: padded by
/// [`SYMBOL_COLLISION_PADDING_LOGICAL_PX`] as a symbol's own box is, so a
/// label keeps the same gap from the chrome as from another label. Shared
/// by native (`map_view::CornerChrome::obstacles`) and the browser shell.
pub(crate) fn chrome_obstacle(x: f32, y: f32, w: f32, h: f32, scale_factor: f32) -> crate::collision::CollisionBox {
    let pad = SYMBOL_COLLISION_PADDING_LOGICAL_PX * scale_factor;
    crate::collision::CollisionBox { left: x - pad, top: y - pad, right: x + w + pad, bottom: y + h + pad }
}

/// [`chrome_obstacle`] for a `(left, top, right, bottom)` rectangle. To the
/// float the same box as `chrome_obstacle(x, y, w, h)` for the rectangle
/// `(x, y, x + w, y + h)`: `x + w + pad` is `right + pad`.
pub(crate) fn chrome_obstacle_rect((left, top, right, bottom): (f32, f32, f32, f32), scale_factor: f32) -> crate::collision::CollisionBox {
    let pad = SYMBOL_COLLISION_PADDING_LOGICAL_PX * scale_factor;
    crate::collision::CollisionBox { left: left - pad, top: top - pad, right: right + pad, bottom: bottom + pad }
}

/// The region the frame's collision grid covers, in PHYSICAL pixels: the
/// viewport grown by `SYMBOL_CULL_MARGIN_PX` LOGICAL pixels on every side.
///
/// The margin is in logical pixels because that is the space the symbol
/// loop's cull test works in (`AnchorProjection::screen` returns
/// logical coordinates), while the grid itself is physical because every
/// collision box is -- so the margin is scaled on its way in and the two
/// stay exactly the same 100 logical pixels.
pub(crate) fn symbol_collision_bounds(viewport_width_px: f32, viewport_height_px: f32, scale_factor: f32) -> CollisionBox {
    CollisionBox {
        left: -SYMBOL_CULL_MARGIN_PX * scale_factor,
        top: -SYMBOL_CULL_MARGIN_PX * scale_factor,
        right: (viewport_width_px + SYMBOL_CULL_MARGIN_PX) * scale_factor,
        bottom: (viewport_height_px + SYMBOL_CULL_MARGIN_PX) * scale_factor,
    }
}

/// The text half of a symbol's collision box, in PHYSICAL pixels.
///
/// `left`/`top` are `text_area_origin`'s output verbatim -- the very origin
/// the glyphs will be rasterized from -- and the extent is the size shaping
/// measured for the whole block (`ShapedLabel::measured_width` /
/// `measured_height`). So the box is what the label will actually cover, not
/// an estimate of it.
///
/// The height must be the BLOCK's, not one line's. This box is handed
/// straight back to glyphon as the label's `TextBounds`
/// (`clip_labels_behind_later_icons` gives every labelled symbol its own box
/// as a clip, whether or not anything covers it), so a box that assumes one
/// line does not merely under-reserve collision space -- it slices the
/// second line off on screen. That was visible over Myanmar, where Liberty's
/// `concat(name:latin, "\n", name:nonlatin)` makes two-line labels the norm
/// and every one of them showed a sliver of cut-off Burmese.
pub(crate) fn text_collision_box(left: f32, top: f32, measured_width: f32, measured_height: f32) -> CollisionBox {
    CollisionBox::from_origin_size(left, top, measured_width, measured_height)
}

/// The icon half of a symbol's collision box, in PHYSICAL pixels, centered
/// on `(anchor_px_x, anchor_px_y)` -- the same anchor the drawn quad is
/// centered on.
///
/// Takes part in placement with the ROTATED quad's axis-aligned bounding
/// box, not the drawn quad's own `width`/`height`: those describe the quad
/// BEFORE the vertex shader's rotation (see `IconInstance::rotation_rad`'s
/// doc comment), so a collision box built from them would under-estimate a
/// rotated icon's screen footprint -- and for a non-square sprite at a steep
/// angle, be the wrong shape entirely (a 32x8 arrow rotated 90 degrees still
/// reserving 32x8, when what actually covers the screen is 8x32).
///
/// Standard axis-aligned-bounding-box-of-a-rotated-rectangle formula:
/// `w' = |w cos θ| + |h sin θ|`, `h' = |w sin θ| + |h cos θ|`. At `θ = 0`
/// this reduces to `(w, h)` exactly, so point symbols (whose `rotation_rad`
/// is always `0.0`) are unaffected.
pub(crate) fn icon_collision_box(width: f32, height: f32, rotation_rad: f32, anchor_px_x: f32, anchor_px_y: f32) -> CollisionBox {
    let (sin_r, cos_r) = rotation_rad.sin_cos();
    let rotated_width = (width * cos_r).abs() + (height * sin_r).abs();
    let rotated_height = (width * sin_r).abs() + (height * cos_r).abs();
    CollisionBox::from_origin_size(
        anchor_px_x - rotated_width / 2.0,
        anchor_px_y - rotated_height / 2.0,
        rotated_width,
        rotated_height,
    )
}

/// The two `layout` properties that place a label relative to the point it
/// was anchored at, travelling together because they only mean anything
/// together: `text-offset` moves the point, `text-anchor` says which part
/// of the text goes on it. Neither is a substitute for the other -- Liberty's
/// `poi_transit` needs both (`[0.9, 0]` and `left`) to put its label beside
/// its icon rather than on top of it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct LabelAnchoring {
    /// `layout.text-offset`, in EMS of the label's own font size.
    pub(crate) offset_em: (f32, f32),
    pub(crate) anchor: TextAnchor,
}

impl LabelAnchoring {
    /// The style-spec default anchor with a given offset -- for callers
    /// that have a `text-offset` but genuinely have no `text-anchor` to
    /// read (the overlay's own marker paint), and for tests whose subject
    /// is something other than the anchor.
    pub(crate) fn centred(offset_em: (f32, f32)) -> Self {
        LabelAnchoring { offset_em, anchor: TextAnchor::Center }
    }
}

/// The final `TextArea` `(left, top)` for one label, in PHYSICAL pixels,
/// SNAPPED to whole physical pixels.
///
/// `screen_x`/`screen_y` are `AnchorProjection::screen`'s LOGICAL-pixel output;
/// `measured_width`/`measured_height` are the size shaping measured for the
/// whole block (recorded once into `ShapedLabel`), already in the same
/// physical space the buffer was shaped in.
///
/// `anchoring` carries the two INDEPENDENT layout properties that decide
/// where the block goes relative to `screen_x`/`screen_y`, and the order
/// matters: `text-offset` moves the anchor POINT (in ems -- this project's
/// established convention, hence the multiply by the buffer's own physical
/// font size), and only then does `text-anchor` decide WHICH PART OF THE
/// TEXT lands on the point it moved to. Under the style-spec default of
/// `center` half of each extent comes back off, so the block straddles the
/// point rather than being top-left anchored; under `left` nothing comes
/// off the x and the run starts at the point; under `right` the whole
/// width does. See `TextAnchor`.
///
/// The two are grouped into a struct rather than passed as two more
/// parameters because this function was already at clippy's
/// seven-argument threshold, and `too_many_arguments` is a fair complaint
/// here rather than one to silence.
///
/// The rounding is the point of this helper. cosmic-text rasterizes a glyph
/// run relative to the `TextArea` origin it is given, so an origin landing
/// between two physical pixels puts every vertical stem and every baseline in
/// the run between two pixels too, and the antialiaser splits each one across
/// both -- which is precisely the "soft and light" look that separates these
/// labels from native macOS text (macOS snaps text to whole device pixels).
/// The projection hands us arbitrary floats, so without this the origin is
/// fractional essentially always.
///
/// It is deliberately the SUM that is rounded, not the terms: rounding the
/// anchor, the offset and the anchoring term separately would accumulate up
/// to half a pixel of error each and could shift a label a whole pixel off
/// its anchor. That stays true now that the third term is a fraction of the
/// extent rather than always half of it -- `text_area_origin_still_rounds_
/// only_the_sum_once_an_anchor_term_is_in_it` pins it.
pub(crate) fn text_area_origin(
    screen_x: f32,
    screen_y: f32,
    scale_factor: f32,
    anchoring: LabelAnchoring,
    physical_text_size: f32,
    measured_width: f32,
    measured_height: f32,
) -> (f32, f32) {
    let left = screen_x * scale_factor + anchoring.offset_em.0 * physical_text_size
        - measured_width * anchoring.anchor.horizontal_fraction();
    // Half the SHAPED BLOCK's height, not half the font size. A buffer is
    // shaped with `Metrics::new(size, size * LABEL_LINE_HEIGHT_RATIO)`, so
    // one line box is 1.2x the font size tall -- and `measured_height` is
    // exactly that for a one-line label, which is why this is the same
    // number `text_collision_box` sizes the box with. Subtracting
    // `size / 2.0` centred a box 1.2x taller than it was assumed to be,
    // leaving every label 0.1 * size too LOW: measured against real shaped
    // metrics at size 18 (baseline 17.60 in a 21.60 line box), an all-caps
    // run's visible band centred 2.3px below its anchor instead of 0.5px.
    // On a route shield, where the text must sit inside a 28px badge, that
    // reads as the number sagging.
    //
    // For a label with more than one line the same argument applies to the
    // BLOCK: `measured_height` is `n` line boxes, and centring on it is
    // what puts a two-line place name straddling its anchor rather than
    // hanging below it. Centring on one line instead would push the second
    // line a whole line box down and out of the collision box that becomes
    // its clip.
    //
    // What the halving is now a CASE OF, rather than the only behaviour:
    // `text-anchor: center` takes half of each extent, and the other eight
    // keywords take none of it or all of it, per axis
    // (`TextAnchor::horizontal_fraction`/`vertical_fraction`). A `center`
    // fraction is exactly `0.5`, and multiplying by 0.5 is the same f32 as
    // dividing by 2.0 -- both exact, both powers of two -- so every label
    // that was centred before this property was read lands on the very same
    // pixel it did.
    let top = screen_y * scale_factor + anchoring.offset_em.1 * physical_text_size
        - measured_height * anchoring.anchor.vertical_fraction();
    (left.round(), top.round())
}

/// Where a label's `TextArea` origin goes inside the box an `icon-text-fit`
/// badge grew to hold it -- `fitted_text_box`, `(left, top, right, bottom)`
/// in physical pixels.
///
/// Horizontally always centred on `measured_width`, and not offset by
/// padding a second time: `fitted_size` already grew the icon by the label
/// PLUS the padding, so adding it again here shifted the label off centre by
/// exactly that much -- a bug this line used to have, and it showed as a
/// wide left margin.
///
/// Vertically, ONE line is centred on its INK, not its line box: the box
/// grew to the line box's height, and centring on that instead of the ink
/// leaves the glyphs riding high, because a line's baseline sits about 0.81
/// of the way down it. `ink_height` (the font's cap-height at this size) is
/// what makes that centring line up with what a reader's eye treats as "the
/// text", not the invisible leading above and below it.
///
/// MORE than one line is centred on the whole BLOCK instead
/// (`measured_height`, exactly what `fitted_size` grew the box to hold), the
/// same generalisation `text_area_origin` already makes for its own
/// no-icon-fit path. Re-using the one-line ink formula for `n` lines centres
/// only the FIRST line's cap band -- anchoring the whole block near the top
/// of a box with nothing to balance it below, and every line past the first
/// pushed down and out of the badge that grew to hold it. Confirmed live: a
/// three-line Stop Details callout carried a wide empty margin above its
/// first line while its third line hung out below the badge entirely --
/// undamaged (text always draws over icons, `map_view.rs`'s own render-pass
/// ordering), just outside the box that was supposed to hold it.
pub(crate) fn fitted_text_position(
    fitted_text_box: (f32, f32, f32, f32),
    physical_text_size: f32,
    ink_height: f32,
    measured_width: f32,
    measured_height: f32,
    label_baseline: f32,
) -> (f32, f32) {
    let (box_left, box_top, box_right, box_bottom) = fitted_text_box;
    let one_line = physical_text_size * LABEL_LINE_HEIGHT_RATIO;
    let top = if measured_height > one_line + 0.5 {
        box_top + ((box_bottom - box_top) - measured_height) / 2.0
    } else {
        let centre_y = (box_top + box_bottom) / 2.0;
        centre_y + ink_height / 2.0 - label_baseline
    };
    (box_left + ((box_right - box_left) - measured_width) / 2.0, top)
}

/// Whether an overlay symbol draws at all at `camera_zoom` --
/// `overlay::paint::FeaturePaint::min_zoom`'s own check, extracted so it is
/// testable without a real camera or GPU context. `None` always draws,
/// matching a Mapbox layer with no `minzoom`.
pub(crate) fn symbol_visible_at_zoom(min_zoom: Option<f32>, camera_zoom: f64) -> bool {
    match min_zoom {
        Some(min_zoom) => camera_zoom >= min_zoom as f64,
        None => true,
    }
}

/// The `(dx, dy)` PHYSICAL-pixel offsets at which one label's halo copies are
/// drawn, relative to its (already pixel-snapped) origin -- empty when the
/// label has no halo.
///
/// glyphon/cosmic-text have no text-outline primitive, so a halo is faked by
/// drawing the same shaped buffer four more times in the halo color, one step
/// out in each cardinal direction, underneath the real glyph. That
/// approximation only works if those copies land on whole pixels: at a
/// fractional offset each copy is itself smeared over two pixel columns (or
/// rows), and four smeared copies compound coverage at every edge into a
/// smudge rather than an outline. Mapbox `text-halo-width` values are
/// commonly 0.5-1.25, so the raw physical offset is fractional far more often
/// than not.
///
/// Hence: round to whole pixels, but floor at 1 whole pixel whenever a halo
/// was requested at all. Without the floor a sub-half-pixel width would round
/// to zero and stack all four copies exactly on the crisp centre glyph --
/// no outline, just four extra alpha passes fattening the glyph in place.
/// A width of zero (the common case: most layers declare no halo) still
/// yields no copies at all, so unhaloed labels stay a single crisp run.
///
/// Non-finite widths are reachable without a bug on our side (style-driven,
/// then multiplied by a window-supplied scale factor) and are treated as "no
/// halo": a NaN or infinite `TextArea.left` would corrupt the entire prepared
/// text batch, not just the one label.
///
/// This remains an approximation -- a real SDF halo is tracked separately.
/// Text size of the source attribution credit, in LOGICAL pixels (scaled by
/// the display's scale factor before shaping, like every other label).
///
/// MapLibre GL JS draws its own attribution control at 12 CSS px. This is
/// smaller because the plate collapses to an "i" button only when it is
/// too tall for the map (`attribution_plate_layout`), so it is usually all
/// on screen, and because it must not read as part of the map's own
/// labelling.
pub(crate) const ATTRIBUTION_TEXT_SIZE_PX: f32 = 10.0;


/// The light pill's palette (`attribution::AttributionTone::Light`): what
/// a shell that does not know its map's tone draws (iOS), and the colours
/// below.
#[cfg(test)]
const LIGHT_ATTRIBUTION: crate::attribution::AttributionPalette = crate::attribution::AttributionTone::Light.palette();

/// The credit's ink on the light pill: `#333`, MapLibre's
/// `.maplibregl-ctrl-attrib a` (`rgb(0 0 0 / 75%)`) on its white compact
/// pill. A map whose background is dark draws the dark pill instead
/// (`attribution::AttributionTone`, chosen by `MapView::paint` and the
/// capture from the style's background).
///
/// History worth keeping: this credit used to be dark text in a faked white
/// halo (four offset glyph copies), which at 10 px swallowed the strokes and
/// read embossed; a plate replaced the halo, and the plate's alpha had to be
/// re-derived when the surface moved from `*Srgb` to `Unorm` (the blend runs
/// on sRGB-encoded values -- `plate_shader.wgsl`). The pill is now opaque on
/// light maps and 90% on dark ones, so its contrast no longer depends on the
/// map under it; `the_attribution_credit_clears_wcag_aa_over_the_darkest_possible_basemap`
/// still pins the ratio for both tones.
#[cfg(test)]
pub(crate) const ATTRIBUTION_TEXT_COLOR: style::Rgba =
    style::Rgba { r: LIGHT_ATTRIBUTION.text[0], g: LIGHT_ATTRIBUTION.text[1], b: LIGHT_ATTRIBUTION.text[2], a: LIGHT_ATTRIBUTION.text[3] };

/// Room between the plate's edge and the text it holds, in LOGICAL pixels.
/// Wider horizontally than vertically -- "comfortable" padding around a
/// short single line of text reads as a pill-ish control, not a tight box
/// traced around the glyphs.
///
/// **The vertical 1.5** makes the plate 15 px, five
/// `brand_mark::GRID_UNIT_PX`, with the text centred in it (~4 px of plate
/// above the caps, ~4 below the baseline). It was chosen when the plate and
/// the mark shared a FOOT, to put the credit's baseline on the wordmark's;
/// since 2026-10-08 the bottom row shares a CENTRE line instead
/// (`chrome::BOTTOM_ROW_HEIGHT_PX`: the plate's foot 10.5 up, the mark's 9),
/// which puts the credit's baseline ~2 px above the wordmark's -- the
/// centre line is what reads as one row at this size. MapLibre's own plate
/// is no looser: `line-height: 20px` on 12 px type and no vertical padding.
/// `map_view`'s `the_mark_and_the_credit_share_one_centre_line` measures it
/// through the shaped credit at 1x and 2x.
pub(crate) const ATTRIBUTION_PLATE_PADDING_X_PX: f32 = 6.0;
pub(crate) const ATTRIBUTION_PLATE_PADDING_Y_PX: f32 = 1.5;

/// Where the attribution plate's top-left corner sits, in PHYSICAL pixels,
/// clamped so it never scrolls off either edge -- the same clamping this
/// function did back when it placed the bare (halo-only) text directly, just
/// generalised to an explicit `content_height` instead of deriving one line
/// box's height from a font size internally (the plate's height already has
/// its own padding baked in, so there is no single "font size" to derive it
/// from any more).
///
/// Bottom-right, the corner every map engine puts its attribution control
/// in, measured from the plate's own width so its right edge lands `padding`
/// in from the viewport edge rather than at a guessed offset, and its
/// bottom `physical_padding_bottom` up (the bottom row's centre line puts
/// the plate's foot higher than its side inset: `chrome::bottom_row_foot_px`).
///
/// Both coordinates are clamped at zero. Content wider than the window is --
/// a narrow window, or a source that states a long licence -- then starts at
/// the left edge and is cut off on the right, which still shows the
/// beginning of the credit. Letting `left` go negative would scroll the
/// beginning off-screen instead and show only its tail.
pub(crate) fn attribution_origin(
    physical_viewport_width: f32,
    physical_viewport_height: f32,
    content_width: f32,
    content_height: f32,
    physical_padding_right: f32,
    physical_padding_bottom: f32,
) -> (f32, f32) {
    let left = physical_viewport_width - content_width - physical_padding_right;
    let top = physical_viewport_height - content_height - physical_padding_bottom;
    (left.max(0.0).round(), top.max(0.0).round())
}

/// The widest the attribution plate may run before its credit stacks, as a
/// fraction of the frame's width (map-stories spec §5.4).
pub(crate) const ATTRIBUTION_STACK_FRACTION: f32 = 0.6;

/// The credit `text` as the lines of a stacked plate: split on `" | "`
/// (MapLibre's separator, and `MapView::frame_attribution`'s), then packed
/// greedily so no line is wider than `max_width_px` by `measure`. A credit
/// that fits is one line, unchanged. A single credit that fits on a line
/// is never broken; one that is wider than a whole line on its own is
/// broken at its spaces as a last resort, so the plate never runs off the
/// frame (a licence credit that is clipped is not a credit). Only a single
/// WORD wider than the maximum can still exceed it.
pub(crate) fn stacked_attribution_lines(text: &str, max_width_px: f32, measure: impl Fn(&str) -> f32) -> Vec<String> {
    if measure(text) <= max_width_px {
        return vec![text.to_string()];
    }
    // Tokens with the separator that joins each to the one before it: a
    // whole credit where it fits, its words where it does not.
    let mut tokens: Vec<(Option<&str>, &str)> = Vec::new();
    for part in text.split(" | ") {
        if measure(part) <= max_width_px {
            tokens.push((Some(" | "), part));
        } else {
            for (i, word) in part.split(' ').filter(|w| !w.is_empty()).enumerate() {
                // `None` for the first word: a broken credit starts its own
                // line, so it reads as one credit rather than trailing a
                // " | " at the end of the line before.
                tokens.push((if i == 0 { None } else { Some(" ") }, word));
            }
        }
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    for (separator, token) in tokens {
        if current.is_empty() {
            current = token.to_string();
            continue;
        }
        let candidate = separator.map(|separator| format!("{current}{separator}{token}"));
        if let Some(candidate) = candidate.filter(|c| measure(c) <= max_width_px) {
            current = candidate;
        } else {
            lines.push(std::mem::replace(&mut current, token.to_string()));
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// The attribution plate laid out, in PHYSICAL pixels: the plate's box and
/// each text line's top-left. One layout for every shell -- native
/// (`map_view::attribution_chrome`), the capture's chrome pass and iOS --
/// so a credit wraps, compacts and clears the brand mark the same way on
/// all of them.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AttributionPlate {
    pub(crate) left: f32,
    pub(crate) top: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
    pub(crate) radius: f32,
    /// Each line's text and top-left. The collapsed button's one line is
    /// [`ATTRIBUTION_COMPACT_GLYPH`].
    pub(crate) lines: Vec<(String, f32, f32)>,
    /// Whether the expanded plate is too tall for the map
    /// (`attribution::needs_compact`): a press toggles it.
    pub(crate) compact: bool,
    /// Whether it draws as the collapsed button.
    pub(crate) collapsed: bool,
    /// The round (i) button inside the pill's right end, on its bottom
    /// line: `(left, top, diameter)`, and where its
    /// [`ATTRIBUTION_COMPACT_GLYPH`] draws (top-left). Collapsed, the
    /// button is all there is, inset in the round plate.
    pub(crate) button: (f32, f32, f32),
    pub(crate) button_glyph: (f32, f32),
}

/// The collapsed plate's one glyph: an "i" in a round plate, MapLibre's
/// compact attribution button (`maplibregl-ctrl-attrib-button`).
pub(crate) const ATTRIBUTION_COMPACT_GLYPH: &str = "i";

/// Lays the credit `credit` (credits joined by `attribution::SEPARATOR`)
/// out bottom-right of a `physical_viewport_width` x
/// `physical_viewport_height` corner.
///
/// Expanded, a credit no wider than [`ATTRIBUTION_STACK_FRACTION`] of the
/// width is one line; a wider one stacks ([`stacked_attribution_lines`]),
/// right-aligned in a plate as wide as the widest line -- never clipped.
/// When that plate is taller than `attribution::COMPACT_HEIGHT_FRACTION`
/// of the corner it is **compact**, and `state` decides whether it draws
/// collapsed: a round button one line tall holding
/// [`ATTRIBUTION_COMPACT_GLYPH`].
pub(crate) fn attribution_plate_layout(
    credit: &str,
    physical_viewport_width: f32,
    physical_viewport_height: f32,
    scale_factor: f32,
    state: crate::attribution::CompactState,
    measure: impl Fn(&str) -> f32,
) -> AttributionPlate {
    let physical_text_size = ATTRIBUTION_TEXT_SIZE_PX * scale_factor;
    let pad_x = ATTRIBUTION_PLATE_PADDING_X_PX * scale_factor;
    let pad_y = ATTRIBUTION_PLATE_PADDING_Y_PX * scale_factor;
    let line_height = physical_text_size * LABEL_LINE_HEIGHT_RATIO;
    // The bottom row (`chrome::BOTTOM_ROW_INSET_PX`): the plate stands the
    // row's inset in from the right, and its ONE-line height is centred on
    // the row's centre line, the brand mark's middle -- so a stacked plate
    // keeps its bottom line there and grows up, and the compact button
    // (one line tall) sits on it too.
    let padding = crate::chrome::BOTTOM_ROW_INSET_PX * scale_factor;
    let one_line_height = line_height + pad_y * 2.0;
    let foot = crate::chrome::bottom_row_foot_px(one_line_height / scale_factor) * scale_factor;
    let max_text_width = physical_viewport_width * ATTRIBUTION_STACK_FRACTION - pad_x * 2.0;
    let lines = stacked_attribution_lines(credit, max_text_width, &measure);
    let plate_height = line_height * lines.len() as f32 + pad_y * 2.0;
    let logical_width = physical_viewport_width / scale_factor.max(f32::MIN_POSITIVE);
    let compact = crate::attribution::compact_plate(plate_height, physical_viewport_height, lines.len(), logical_width);
    let narrow = crate::attribution::is_narrow(logical_width);
    // The (i) button: a circle one line box across, centred in a line's
    // band of the pill (so `pad_y` of pill rings it), its "i" centred in it
    // -- MapLibre's `.maplibregl-ctrl-attrib-button`, at this plate's scale.
    let button_size = line_height;
    let glyph_width = measure(ATTRIBUTION_COMPACT_GLYPH);
    let button_at = |left: f32, top: f32| {
        ((left, top, button_size), (left + ((button_size - glyph_width) / 2.0).max(0.0), top))
    };
    if state.collapsed_on(compact, narrow) {
        let size = one_line_height;
        let (left, top) = attribution_origin(physical_viewport_width, physical_viewport_height, size, size, padding, foot);
        let (button, button_glyph) = button_at(left + pad_y, top + pad_y);
        return AttributionPlate {
            left,
            top,
            width: size,
            height: size,
            radius: size / 2.0,
            lines: vec![(ATTRIBUTION_COMPACT_GLYPH.to_string(), button_glyph.0, button_glyph.1)],
            compact,
            collapsed: true,
            button,
            button_glyph,
        };
    }
    let widths: Vec<f32> = lines.iter().map(|line| measure(line)).collect();
    let widest = widths.iter().copied().fold(0.0_f32, f32::max);
    // The pill: the credits `pad_x` in from its left end, then half that
    // before the button, which stands `pad_y` in from the right end (as
    // from the top and bottom) -- MapLibre's compact
    // `padding: 2px 28px 2px 8px` around a 24 px button, on this grid.
    let plate_width = pad_x + widest + pad_x / 2.0 + button_size + pad_y;
    let (left, top) =
        attribution_origin(physical_viewport_width, physical_viewport_height, plate_width, plate_height, padding, foot);
    // Inside the plate, offset by its own inner padding -- not re-clamped:
    // the plate was clamped to the viewport, and the text only falls
    // further inside it.
    let lines = lines
        .into_iter()
        .zip(widths)
        .enumerate()
        .map(|(row, (line, width))| (line, left + pad_x + (widest - width), top + pad_y + line_height * row as f32))
        .collect();
    let (button, button_glyph) = button_at(left + plate_width - pad_y - button_size, top + plate_height - pad_y - line_height);
    AttributionPlate {
        left,
        top,
        width: plate_width,
        height: plate_height,
        // A pill: ends as round as one line is tall (a stacked plate keeps
        // those corners and grows up).
        radius: one_line_height / 2.0,
        lines,
        compact,
        collapsed: false,
        button,
        button_glyph,
    }
}

/// Mapbox's own conversion from a `text-halo-blur` in points to the width of
/// the smoothstep band its glyph shader fades the halo out over.
///
/// `symbol_sdf.fragment.glsl` computes
/// `gamma = (u_halo_blur * 1.19 / SDF_PX + EDGE_GAMMA) / fontScale` and then
/// `smoothstep(buff - gamma, buff + gamma, dist)`, with `SDF_PX = 8`. Since
/// one SDF unit is `SDF_PX` pixels, `gamma` is `1.19 * blur` PIXELS, and the
/// fade runs from `halo_width - gamma` out to `halo_width + gamma`. The
/// `EDGE_GAMMA` term is the constant antialiasing gamma every edge gets and
/// is not part of the blur, so it is not reproduced here -- this path's
/// edges are antialiased by the rasteriser instead.
pub(crate) const HALO_BLUR_PIXELS_PER_POINT: f32 = 1.19;

/// How many rings a blurred halo is allowed to fade over.
///
/// Each ring is four more whole copies of the label's glyph run, so the cost
/// is linear in this and a style is free to ask for a blur of 20. The real
/// styles ask for 0.5 or 1 (`examples/mapbox-style.json`'s thirteen), which
/// is two or three rings; eight is far past anything that reads as a halo
/// rather than a glow, and past it the ramp is simply sampled more coarsely.
pub(crate) const HALO_MAX_RINGS: usize = 8;

/// The hard-edge halo radius, in whole physical pixels, from which its
/// cross of copies gets four diagonal copies too ([`halo_offsets`]).
pub(crate) const HALO_DIAGONALS_FROM_PX: f32 = 3.0;

/// The offset copies that fake one label's halo, and the source alpha each
/// is drawn at -- **outermost first**, which is the order they must be
/// composited in (see below).
///
/// # Why copies at all
///
/// glyphon/cosmic-text have no text-outline primitive and no signed distance
/// field for text (unlike the icon path, which has a real one). So the halo
/// is the same shaped buffer drawn again, offset, in the halo colour, under
/// the real-colour glyph -- a dilation by a plus-shaped structuring element.
///
/// # Whole pixels, and at least one
///
/// At a FRACTIONAL offset each copy is itself blurred across two pixel
/// columns/rows and four of them compound coverage at every edge -- a smudge.
/// Mapbox halo widths are commonly 0.5-1.25pt, so the raw physical offset is
/// very often fractional. And an offset that rounds to zero would stack all
/// four copies exactly on the crisp centre glyph: no outline at all, just
/// four extra passes fattening the glyph in place. So the radii are whole
/// pixels, with a floor of one whenever a halo was actually asked for.
///
/// # `text-halo-blur`
///
/// With `halo_blur` at zero -- the style-spec default, and what most layers
/// leave it at -- this is exactly one ring at full alpha and nothing here
/// changes what a halo looks like.
///
/// A non-zero blur is Mapbox's smoothstep fade, sampled at whole-pixel radii:
/// coverage is 1 inside `halo_width - gamma`, 0 outside `halo_width + gamma`,
/// and smoothstep between (see [`HALO_BLUR_PIXELS_PER_POINT`]). One ring per
/// radius, each carrying that radius's coverage.
///
/// The alphas returned are NOT those coverages. The copies are alpha-blended
/// over each other with no depth test, so a pixel inside ring `i` is painted
/// by every ring from the outermost inwards, and painting three copies at
/// 0.5 does not leave 0.5 behind -- it leaves 0.875. So the rings are
/// emitted outermost first and each is given the source alpha that lands the
/// COMPOSITE on its own coverage: `s = (a_i - a_out) / (1 - a_out)`, where
/// `a_out` is what the rings already painted left there. That is exact for a
/// fully opaque halo colour; for a translucent one the caller's own
/// multiplication by `halo_color.a` makes it an approximation, in the same
/// direction and by less than the ring quantisation itself.
pub(crate) fn halo_offsets(halo_width: f32, halo_blur: f32) -> Vec<(f32, f32, f32)> {
    if !halo_width.is_finite() || halo_width <= 0.0 {
        return Vec::new();
    }
    let ring = |radius: f32, alpha: f32| {
        [(-radius, 0.0, alpha), (radius, 0.0, alpha), (0.0, -radius, alpha), (0.0, radius, alpha)]
    };
    // A wide hard edge is a cross with gaps between its arms: at 3 or more
    // physical pixels the four copies stand clear of a glyph's strokes and
    // read as four ghost glyphs -- the "doubled" agent labels of the user's
    // 2026-10-12 delivery-sim screenshot (2 pt at 2x = a 4 px cross). The
    // diagonals close it into an octagon. Two pixels and under (every
    // 1 pt halo at 2x) stays the plain cross it always was.
    let hard_edge = || {
        let radius = halo_width.round().max(1.0);
        let mut copies = ring(radius, 1.0).to_vec();
        if radius >= HALO_DIAGONALS_FROM_PX {
            let d = (radius * std::f32::consts::FRAC_1_SQRT_2).round().max(1.0);
            copies.extend_from_slice(&[(-d, -d, 1.0), (d, -d, 1.0), (-d, d, 1.0), (d, d, 1.0)]);
        }
        copies
    };
    if !halo_blur.is_finite() || halo_blur <= 0.0 {
        return hard_edge();
    }
    let gamma = halo_blur * HALO_BLUR_PIXELS_PER_POINT;
    let outermost = (halo_width + gamma).round().max(1.0);
    let rings = (outermost as usize).min(HALO_MAX_RINGS);
    // Coverage at each whole-pixel radius, innermost first.
    let coverage: Vec<f32> = (1..=rings)
        .map(|r| {
            let radius = r as f32;
            let t = ((radius - (halo_width - gamma)) / (2.0 * gamma)).clamp(0.0, 1.0);
            // 1 - smoothstep: full inside the band, gone outside it.
            1.0 - t * t * (3.0 - 2.0 * t)
        })
        .collect();
    // A blur wide enough to have faded everything away by the first whole
    // pixel would draw no halo at all, which is not what asking for one
    // means. Fall back to the hard edge rather than silently dropping it.
    if coverage.iter().all(|a| *a <= 0.0) {
        return hard_edge();
    }
    let mut offsets = Vec::with_capacity(rings * 4);
    let mut already = 0.0f32;
    for index in (0..rings).rev() {
        let target = coverage[index];
        let source = if already >= 1.0 { 0.0 } else { ((target - already) / (1.0 - already)).clamp(0.0, 1.0) };
        if source > 0.0 {
            offsets.extend_from_slice(&ring(index as f32 + 1.0, source));
            already = target;
        }
    }
    offsets
}

/// How far out the outermost halo copy reaches, in physical pixels -- what a
/// label's clip rectangle has to be grown by so the halo is not shaved off
/// its own edges.
pub(crate) fn halo_reach(halo_width: f32, halo_blur: f32) -> f32 {
    halo_offsets(halo_width, halo_blur).iter().map(|(dx, dy, _)| dx.abs().max(dy.abs())).fold(0.0, f32::max)
}


/// The face the engine's own chrome -- the attribution credit -- and a
/// caller's overlay features are shaped in.
///
/// Neither reads a `text-font`: the credit has no style layer at all, and an
/// overlay's `overlay::paint::FeaturePaint` carries no font property. Both
/// keep the Medium-weight SF Pro Text they have always used; see
/// `font_fallback::ResolvedFace::chrome`.
///
/// A BASEMAP label does not come through here. It is shaped in the face its
/// layer's `layout.text-font` asks for, resolved by
/// `font_fallback::resolve_font_stack` against what this machine actually
/// has installed -- which is the whole point of that function, and why this
/// is no longer one constant for every label on the map.
pub(crate) fn chrome_text_face() -> crate::font_fallback::ResolvedFace {
    crate::font_fallback::ResolvedFace::chrome()
}

/// How many shaped labels `LabelShapeCache` keeps before it starts throwing
/// away the ones the current frame didn't draw.
///
/// The cache is keyed by `(text, physical_text_size)`, and panning feeds it a
/// steady stream of new place names, so without a bound it would grow for as
/// long as the session lasts. 1024 is comfortably more than a dense viewport
/// holds (a screenful of labels is order-100s), so in steady state nothing is
/// ever evicted and no label is ever re-shaped -- the cap only bites after a
/// long pan has accumulated names that are now far off-screen.
pub(crate) const LABEL_SHAPE_CACHE_MAX_ENTRIES: usize = 1024;

/// A shaped label: the `glyphon::Buffer` the text renderer draws from, plus
/// the SIZE that shaping measured for it.
///
/// The size is stored rather than re-derived per frame because the only way
/// to get it back out of a `Buffer` is to walk `layout_runs()`, and the whole
/// point of this cache is that the per-frame path does no layout work at all.
/// It is in the same physical-pixel space as the `Metrics` the buffer was
/// shaped with, which is exactly what `text_area_origin` expects.
pub(crate) struct ShapedLabel {
    pub(crate) buffer: glyphon::Buffer,
    /// The widest line's width, in physical pixels -- the maximum
    /// `LayoutRun::line_w` over every run, not the first run's.
    pub(crate) measured_width: f32,
    /// The whole text block's height, in physical pixels: the bottom of
    /// the last layout run (`line_top + line_height`), which for a
    /// single-line label is exactly one line box
    /// (`physical_text_size * LABEL_LINE_HEIGHT_RATIO`) and for an
    /// `n`-line one is `n` of them.
    ///
    /// Taken from the runs' own geometry rather than counting `\n`s and
    /// multiplying, because the runs are the ground truth for what
    /// glyphon will draw: cosmic-text may give a run its own
    /// `line_height`, and it lays out a trailing newline as a real (if
    /// empty) extra line. Counting separators would have to guess at
    /// both.
    pub(crate) measured_height: f32,
    /// Every distinct glyph image glyphon will look up to draw this label at
    /// a whole-pixel origin: its atlas cache keys, deduplicated.
    ///
    /// `text_areas` puts every label (and every halo copy, offset a whole
    /// pixel) on a whole-pixel origin, so the sub-pixel bins these keys
    /// carry depend only on the shaping, not on where the label lands. That
    /// is what lets `render::text_glyph_budget` tell, before `prepare`,
    /// whether drawing a label will rasterise anything.
    pub(crate) glyph_keys: Vec<glyphon::cosmic_text::CacheKey>,
    /// Distance from the buffer's top to the run's BASELINE, in physical
    /// pixels -- glyphon's `LayoutRun::line_y`.
    ///
    /// Recorded because a line box is not centred on the text inside it.
    /// cosmic-text puts the baseline about 0.81 of the way down (measured:
    /// 17.60 in a 21.60 box at size 18), so centring the BOX leaves the
    /// visible glyphs sitting high -- which is exactly what made the
    /// numbers in a fitted badge look top-heavy. Anything that needs the
    /// ink centred rather than the box has to work from this.
    pub(crate) baseline: f32,
    /// What shaping this label cost, in ms. Most labels shape in ~0.03 ms;
    /// one that needs a font-fallback walk (`"Rabat\nⵔⴱⴰⵟ الرباط"`: Tifinagh
    /// and Arabic under a Latin face) costs 2-3 ms EVERY time it is shaped,
    /// and the census zoom sweep re-shaped the same handful of those on
    /// every return to z3 -- 4-7 ms of `symbol_collect` per frame. See
    /// [`LabelShapeCache::end_frame`], which keeps these past the trim.
    pub(crate) cost_ms: f32,
    /// [`LabelShapeCache`]'s frame counter when this label was last drawn.
    pub(crate) last_used: u64,
}

/// Shaping at or above this cost marks a label as expensive to rebuild, and
/// the trim keeps it while it is among the [`EXPENSIVE_SHAPES_KEPT`] most
/// recently drawn such labels. See [`ShapedLabel::cost_ms`].
pub(crate) const EXPENSIVE_SHAPE_MS: f32 = 0.5;

/// How many expensive labels not drawn this frame survive the trim. Small
/// next to [`LABEL_SHAPE_CACHE_MAX_ENTRIES`]: fallback-heavy labels are a
/// few per screen, and this bounds what they can hold.
pub(crate) const EXPENSIVE_SHAPES_KEPT: usize = 256;

/// The cache key: the label's text, its physical text size, and the wrap
/// width it was shaped against -- the two `f32`s as raw bits, since `f32` is
/// not `Hash`/`Eq`.
///
/// The wrap width is part of the key because two layers can hand this cache
/// the same name at the same size and want different line breaks:
/// `poi_transit` asks for 9 em while a layer that declares no
/// `text-max-width` gets the spec's 10, and `None` (no wrapping at all) is a
/// third case again. Serving one from another's entry would render the label
/// at the wrong width for as long as it stayed cached.
///
/// The last member is the run's [`crate::font_fallback::TextStyle`] (spec
/// §8): tracking and OpenType features shape a different buffer, so a
/// tabular stat and the same text unstyled are two entries. Map labels
/// pass `TextStyle::default()` (through [`LabelShapeCache::key`] and
/// [`LabelShapeCache::get_or_shape`]), which shapes exactly as before.
/// The style is normalised by `TextStyle::for_text` first, so an Arabic
/// run's tracked and untracked keys are one.
pub(crate) type LabelKey = (String, u32, Option<u32>, crate::overlay::paint::TextJustify, crate::font_fallback::ResolvedFace, crate::font_fallback::TextStyle);

/// The three numbers `ShapedLabel` carries, read off an already-shaped
/// buffer: the widest line's width, the whole block's height, and the first
/// run's baseline. `one_line` is the buffer's own line height, used only as
/// the fallback for a buffer with no runs at all.
///
/// Measured across EVERY layout run, not just the first. Liberty's
/// text-field for a feature carrying `name:nonlatin` is
/// `concat(name:latin, "\n", name:nonlatin)` and the literal newline
/// survives compilation, so a large share of the world's place labels shape
/// into TWO runs -- and now that `text-max-width` wraps, any long name does.
/// Taking the first run's `line_w` as the label's width, and one line box as
/// its height, described a label that does not exist: the collision box
/// built from those numbers becomes the `TextBounds` glyphon clips to, and
/// the second line was drawn and then sliced away -- visibly, as a sliver of
/// cut-off Burmese under every place name over Myanmar.
///
/// Width is the WIDEST line, since the block is positioned by its widest
/// extent. Height comes from the runs' own geometry -- the bottom of the
/// last one -- rather than counting newlines and multiplying by the line
/// height, because the runs are the ground truth for what glyphon will draw:
/// cosmic-text may give a run its own `line_height`, and a trailing newline
/// lays out as a real (if empty) extra line (pinned by
/// `a_trailing_newline_really_does_report_a_second_empty_line`). For one
/// line `line_top` is 0 and `line_height` is the `Metrics` line height, so
/// this reduces to exactly the `physical_text_size * LABEL_LINE_HEIGHT_RATIO`
/// the one-line path always used -- no existing label moves.
pub(crate) fn measure_shaped_block(buffer: &glyphon::Buffer, one_line: f32) -> (f32, f32, f32) {
    let mut measured_width = 0.0f32;
    let mut block_bottom = 0.0f32;
    // `line_y` is already relative to the buffer's own origin, the same
    // origin a `TextArea`'s `top` addresses. It is the FIRST run's baseline:
    // the only consumer is the badge-fitting path, which centres a
    // single-line number's ink.
    let mut baseline = None;
    for run in buffer.layout_runs() {
        measured_width = measured_width.max(run.line_w);
        block_bottom = block_bottom.max(run.line_top + run.line_height);
        baseline.get_or_insert(run.line_y);
    }
    // A buffer with no runs at all is not reachable through `set_text` (even
    // the empty string lays out one), but falling back to one line keeps a
    // zero-height collision box -- which collides with nothing and clips
    // everything away -- off the map if it ever became reachable.
    let measured_height = if block_bottom > 0.0 { block_bottom } else { one_line };
    (measured_width, measured_height, baseline.unwrap_or(one_line))
}

/// The distinct glyph-atlas keys `buffer` draws with, at a whole-pixel
/// origin and scale 1 -- see [`ShapedLabel::glyph_keys`].
pub(crate) fn shaped_glyph_keys(buffer: &glyphon::Buffer) -> Vec<glyphon::cosmic_text::CacheKey> {
    let mut keys: Vec<glyphon::cosmic_text::CacheKey> = Vec::new();
    for run in buffer.layout_runs() {
        for glyph in run.glyphs.iter() {
            let key = glyph.physical((0.0, 0.0), 1.0).cache_key;
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
    }
    keys
}

/// Shaped `glyphon::Buffer`s, reused across frames.
///
/// Shaping is by far the most expensive thing the symbol overlay pass does,
/// and the paint loop runs it once per visible label per FRAME for text that
/// almost never changes. Measured at a 30-frame average with identical
/// strings, re-shaping every frame cost 28 ms/frame at 50 labels and 216
/// ms/frame at 400 (dev profile; 1.3 ms and 6.5 ms in release) against a
/// 16.67 ms 60fps budget, while reusing already-shaped buffers costs
/// 0.003-0.028 ms/frame -- roughly 4000x cheaper.
///
/// The key is `(text, physical_text_size, wrap_width)`. `physical_text_size` is
/// `feature.text_size * scale_factor`, so the key re-shapes correctly both
/// when the style's text size changes (zooming across a `text-size` stop) and
/// when the window moves to a display with a different scale factor. The
/// resolved FACE is part of the key too, and has to be: since the style
/// compiler started reading `layout.text-font`, two layers can ask for the
/// same string at the same size in different faces (Liberty sets its water
/// labels in `Noto Sans Italic` and its places in `Noto Sans Regular`), and
/// a key without the face would hand the second layer the first one's
/// shaped buffer. `wrap_width` earns its place in the key for the reason
/// spelled out on `LabelKey`.
pub(crate) struct LabelShapeCache {
    pub(crate) entries: HashMap<LabelKey, ShapedLabel>,
    /// Keys touched by the frame in progress -- what `end_frame` retains by.
    pub(crate) used_this_frame: HashSet<LabelKey>,
    /// Every family name the font database indexes, ASCII-lowercased --
    /// `font_fallback::resolve_font_stack`'s "is this installed?" predicate.
    ///
    /// Built once, on the first label of the session, because `fontdb` has
    /// no family index: answering the question means walking every face on
    /// the machine, and there are thousands. `None` until then; it is not
    /// built in `new()` because the cache is constructed without a
    /// `FontSystem` in hand (and several tests construct one with no GPU at
    /// all).
    pub(crate) installed_families: Option<std::collections::HashSet<String>>,
    /// `text-font` stack -> the face it resolved to, so the walk over the
    /// stack and the installed-set lookups happen once per LAYER rather than
    /// once per label per frame. Keyed by the `Arc`'s contents, not its
    /// address: two layers that declare the same stack share one entry.
    pub(crate) resolved_faces: HashMap<crate::font_fallback::FontStack, crate::font_fallback::ResolvedFace>,
    /// How many real shaping operations have happened over this cache's
    /// lifetime. Exists so tests can assert that a repeated label shapes
    /// exactly once, and so the browser build can hold its own number
    /// against the native one; the paint path never reads it.
    pub(crate) shape_count: usize,
    /// Wall time, in milliseconds, spent inside the `or_insert_with` MISS
    /// branch of `get_or_shape` -- i.e. real glyphon shaping, excluding the
    /// hit path.
    ///
    /// Exists because `label_census` builds a FRESH cache per call while the
    /// live frame holds one on `GpuState` across frames, so the census's total
    /// charges every frame for shaping a real frame pays once. Without this
    /// split the census's headline figure reads as a per-frame cost and is
    /// not one.
    pub(crate) shape_ms: f64,
    /// The family a `text-font` stack falls back to when the machine has
    /// none of the faces it names -- see
    /// `font_fallback::resolve_font_stack_with_default`.
    ///
    /// `DEFAULT_LABEL_FAMILY` on a machine with a font book, because that
    /// face is really installed there. A browser has only what the page
    /// shipped it, so the browser build names that face instead.
    pub(crate) default_family: std::sync::Arc<str>,
    /// Frames ended so far: the clock `ShapedLabel::last_used` reads.
    pub(crate) frame: u64,
    /// Shapes of a key this cache had shaped before and then trimmed
    /// (`end_frame`): the re-shapes `story.card.reshapes_after_first`
    /// counts (spec §14.3). A hit never shapes, so a key shaped twice was
    /// evicted in between.
    pub(crate) reshape_count: usize,
    /// Hashes of the keys `end_frame` trimmed ([`label_key_hash`]), what
    /// [`Self::reshape_count`] is read against. Bounded: cleared past
    /// [`EVICTED_KEYS_KEPT`], so a long session undercounts rather than
    /// grows.
    evicted: HashSet<u64>,
}

/// How many trimmed keys [`LabelShapeCache`] remembers for its re-shape
/// count before it forgets them all.
const EVICTED_KEYS_KEPT: usize = 1 << 16;

/// Empties `evicted` once it holds [`EVICTED_KEYS_KEPT`] hashes.
fn forget_if_full(evicted: &mut HashSet<u64>) {
    if evicted.len() >= EVICTED_KEYS_KEPT {
        evicted.clear();
    }
}

/// A [`LabelKey`]'s hash for the trimmed-key set: `DefaultHasher::new()`
/// has fixed keys, so the same key hashes the same in one process.
fn label_key_hash(key: &LabelKey) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    h.finish()
}

impl LabelShapeCache {
    pub(crate) fn new() -> Self {
        Self::with_default_family(crate::font_fallback::DEFAULT_LABEL_FAMILY)
    }

    /// [`LabelShapeCache::new`] for a build whose font set does not contain
    /// `font_fallback::DEFAULT_LABEL_FAMILY`. See [`Self::default_family`].
    pub(crate) fn with_default_family(default_family: &str) -> Self {
        Self {
            entries: HashMap::new(),
            used_this_frame: HashSet::new(),
            installed_families: None,
            resolved_faces: HashMap::new(),
            shape_count: 0,
            shape_ms: 0.0,
            default_family: default_family.into(),
            frame: 0,
            reshape_count: 0,
            evicted: HashSet::new(),
        }
    }

    pub(crate) fn key(
        text: &str,
        physical_text_size: f32,
        wrap_width: Option<f32>,
        justify: crate::overlay::paint::TextJustify,
        face: &crate::font_fallback::ResolvedFace,
    ) -> LabelKey {
        Self::key_styled(text, physical_text_size, wrap_width, justify, face, crate::font_fallback::TextStyle::default())
    }

    /// [`Self::key`] for a run shaped with `style` (spec §8): the key
    /// [`Self::get_or_shape_styled`] files it under.
    pub(crate) fn key_styled(
        text: &str,
        physical_text_size: f32,
        wrap_width: Option<f32>,
        justify: crate::overlay::paint::TextJustify,
        face: &crate::font_fallback::ResolvedFace,
        style: crate::font_fallback::TextStyle,
    ) -> LabelKey {
        (text.to_string(), physical_text_size.to_bits(), wrap_width.map(f32::to_bits), justify, face.clone(), style.for_text(text))
    }

    /// The face a layer's `layout.text-font` resolves to on THIS machine.
    ///
    /// Memoised per stack: the walk is cheap but the installed-family set it
    /// consults is not, and the paint loop would otherwise redo both for
    /// every label of every layer on every frame.
    pub(crate) fn face_for(
        &mut self,
        font_system: &glyphon::FontSystem,
        stack: &crate::font_fallback::FontStack,
    ) -> crate::font_fallback::ResolvedFace {
        if let Some(face) = self.resolved_faces.get(stack) {
            return face.clone();
        }
        let installed = self
            .installed_families
            .get_or_insert_with(|| crate::font_fallback::installed_families(font_system.db()));
        let face = crate::font_fallback::resolve_font_stack_with_default(
            stack,
            |name| installed.contains(&name.to_ascii_lowercase()),
            &self.default_family,
        );
        self.resolved_faces.insert(stack.clone(), face.clone());
        face
    }

    /// Returns the shaped label for `(text, physical_text_size,
    /// wrap_width)`, shaping it only if it isn't cached yet, and marks it as
    /// used by this frame.
    ///
    /// `wrap_width` is `layout.text-max-width` converted out of ems and into
    /// PHYSICAL PIXELS by the caller (`max_width_em * physical_text_size`),
    /// or `None` for text the property does not apply to -- line-placed
    /// labels and the attribution credit. It is the width past which the
    /// text breaks onto a new line; it is not a clip, and a word too long to
    /// fit still overflows it (see
    /// `a_single_word_wider_than_the_wrap_width_overflows_instead_of_breaking_or_vanishing`).
    ///
    /// # Line alignment, and `text-justify`
    ///
    /// Once a label has more than one line, the lines have to be aligned
    /// against each other -- `justify` (part of `LabelKey`, so a `center`
    /// and a `left` shape of the same text/size/wrap are two different
    /// cache entries, never one overwriting the other) picks which.
    /// Defaults to `Center` at the `FeaturePaint`/basemap-symbol layer,
    /// matching the style spec's own default for `layout.text-justify` --
    /// cosmic-text's own default is left, so doing nothing here would have
    /// silently picked a behaviour the spec does not ask for.
    ///
    /// This is INDEPENDENT of `layout.text-anchor`, which the caller applies
    /// through `text_area_origin`: the anchor positions the whole block
    /// relative to the label's anchor point, while justification aligns the
    /// lines within that block. A left-anchored block whose lines are
    /// centred on each other is exactly right per the spec, even though the
    /// combination looks unusual -- the block's LEFT EDGE lands on the
    /// anchor point, and the short lines sit indented inside it.
    ///
    /// The centring is done in a SECOND shaping pass, at the width the first
    /// pass measured, rather than by aligning inside `wrap_width` directly.
    /// cosmic-text aligns within the buffer's own `width_opt`, so centring a
    /// 42px label inside a 180px wrap box slides it 69px right of where the
    /// caller placed it -- measured. Re-shaping at the measured width puts
    /// the block's left edge back on the buffer's origin, which is the
    /// origin `text_area_origin` and the collision box both address. The
    /// line breaks survive the re-shape because cosmic-text fits a line
    /// while it is `<=` the width, and the width it is given is the widest
    /// line it just produced. The extra pass only ever runs on a cache MISS
    /// of a MULTI-LINE label; a one-liner needs no alignment at all and is
    /// left strictly alone.
    pub(crate) fn get_or_shape(
        &mut self,
        font_system: &mut glyphon::FontSystem,
        text: &str,
        physical_text_size: f32,
        wrap_width: Option<f32>,
        justify: crate::overlay::paint::TextJustify,
        face: &crate::font_fallback::ResolvedFace,
    ) -> &ShapedLabel {
        self.get_or_shape_styled(font_system, text, physical_text_size, wrap_width, justify, face, crate::font_fallback::TextStyle::default())
    }

    /// [`Self::get_or_shape`] with a [`crate::font_fallback::TextStyle`]
    /// (spec §8): the story chrome's weight, tracking and `tnum`/`lnum`.
    /// The style is part of the key, so a styled and an unstyled shape of
    /// one text never share an entry.
    ///
    /// Tracking is added by cosmic-text after EVERY glyph, the last one
    /// too; the measured width drops that trailing step, so a run is as
    /// wide as its ink and a ten-glyph run grows by nine steps. The
    /// buffer itself is untouched -- the glyphs sit where they were
    /// shaped.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn get_or_shape_styled(
        &mut self,
        font_system: &mut glyphon::FontSystem,
        text: &str,
        physical_text_size: f32,
        wrap_width: Option<f32>,
        justify: crate::overlay::paint::TextJustify,
        face: &crate::font_fallback::ResolvedFace,
        style: crate::font_fallback::TextStyle,
    ) -> &ShapedLabel {
        let key = Self::key_styled(text, physical_text_size, wrap_width, justify, face, style);
        let style = key.5;
        self.used_this_frame.insert(key.clone());
        // Split borrow: `entry`'s closure needs `shape_count` mutably while
        // `entries` is already mutably borrowed, which `&mut self` alone
        // would not allow.
        let Self { entries, shape_count, shape_ms, frame, reshape_count, evicted, .. } = self;
        let vacant = match entries.entry(key) {
            std::collections::hash_map::Entry::Occupied(hit) => {
                let entry = hit.into_mut();
                entry.last_used = *frame;
                return entry;
            }
            std::collections::hash_map::Entry::Vacant(vacant) => vacant,
        };
        if !evicted.is_empty() && evicted.contains(&label_key_hash(vacant.key())) {
            *reshape_count += 1;
        }
        let entry = vacant.insert({
            *shape_count += 1;
            let t_shape = crate::timing::Instant::now();
            let one_line = physical_text_size * LABEL_LINE_HEIGHT_RATIO;
            let mut buffer = glyphon::Buffer::new(font_system, glyphon::Metrics::new(physical_text_size, one_line));
            // `Wrap::Word` rather than cosmic-text's own default of
            // `WordOrGlyph`, which breaks a word too long to fit into
            // stacked mid-word fragments (measured: four of them for the
            // long-form Welsh village name at 9 em). mapbox-gl-js's
            // `determineLineBreaks` only ever breaks at a potential break
            // point and lets such a word overflow, and a place name sliced
            // mid-word reads as a rendering fault in a way an over-wide one
            // does not. With `width_opt` at `None` the wrap mode is inert,
            // so this is set unconditionally.
            buffer.set_wrap(glyphon::Wrap::Word);
            buffer.set_size(wrap_width, None);
            buffer.set_text(text, &style.apply(face.attrs()), glyphon::Shaping::Advanced, None);
            buffer.shape_until_scroll(font_system, false);

            let (mut measured_width, mut measured_height, mut baseline) = measure_shaped_block(&buffer, one_line);

            // The second pass, and ONLY for a block that actually has lines
            // to align against each other. See this function's doc comment
            // for why it re-shapes at the measured width instead of
            // aligning inside `wrap_width`.
            if buffer.layout_runs().count() > 1 {
                let align = match justify {
                    crate::overlay::paint::TextJustify::Center => glyphon::cosmic_text::Align::Center,
                    crate::overlay::paint::TextJustify::Left => glyphon::cosmic_text::Align::Left,
                    crate::overlay::paint::TextJustify::Right => glyphon::cosmic_text::Align::Right,
                };
                for line in &mut buffer.lines {
                    line.set_align(Some(align));
                }
                buffer.set_size(Some(measured_width), None);
                buffer.shape_until_scroll(font_system, false);
                (measured_width, measured_height, baseline) = measure_shaped_block(&buffer, one_line);
            }

            // The trailing tracking step (see this function's doc): every
            // line carries one, so the widest line loses one. Untracked,
            // this is exactly 0 and the width is untouched.
            let trailing = style.letter_spacing_em() * physical_text_size;
            if trailing != 0.0 && measured_width > 0.0 {
                measured_width = (measured_width - trailing).max(0.0);
            }

            let cost_ms = t_shape.elapsed().as_secs_f64() * 1000.0;
            *shape_ms += cost_ms;
            let glyph_keys = shaped_glyph_keys(&buffer);
            ShapedLabel { buffer, measured_width, measured_height, baseline, glyph_keys, cost_ms: cost_ms as f32, last_used: 0 }
        });
        entry.last_used = *frame;
        entry
    }

    /// Marks an already-shaped entry as drawn this frame (so `end_frame`
    /// keeps it) without shaping: what the live story chrome, built once
    /// per story frame and drawn on many, does each frame. `false` when
    /// the entry is gone.
    pub(crate) fn touch(&mut self, key: &LabelKey) -> bool {
        if self.entries.contains_key(key) {
            self.used_this_frame.insert(key.clone());
            true
        } else {
            false
        }
    }

    /// Looks up an already-ensured entry without touching the frame's used
    /// set -- the shared-borrow second pass of the paint loop, which builds
    /// `TextArea`s from keys the mutable first pass already shaped.
    pub(crate) fn get(&self, key: &LabelKey) -> Option<&ShapedLabel> {
        self.entries.get(key)
    }

    /// Ends the frame: bounds the cache, then clears the touched set.
    ///
    /// Over the cap, the trim keeps what this frame drew AND the
    /// [`EXPENSIVE_SHAPES_KEPT`] most recently drawn labels that cost at
    /// least [`EXPENSIVE_SHAPE_MS`] to shape: re-shaping those on the next
    /// visit is what spent 4-7 ms per frame on every return to z3 in the
    /// census zoom sweep. MapLibre never reshapes a label it has laid out
    /// for a tile it still holds; this is the nearest the per-frame cache
    /// gets to that.
    pub(crate) fn end_frame(&mut self) {
        self.end_frame_with_caps(LABEL_SHAPE_CACHE_MAX_ENTRIES, EXPENSIVE_SHAPES_KEPT);
    }

    /// [`Self::end_frame`] with both bounds explicit.
    pub(crate) fn end_frame_with_caps(&mut self, cap: usize, expensive_kept: usize) {
        if self.entries.len() > cap && expensive_kept > 0 {
            let used = &self.used_this_frame;
            let mut expensive: Vec<(u64, &LabelKey)> = self
                .entries
                .iter()
                .filter(|(key, label)| !used.contains(*key) && label.cost_ms >= EXPENSIVE_SHAPE_MS)
                .map(|(key, label)| (label.last_used, key))
                .collect();
            // The newest `expensive_kept` survive, by key and not by a
            // `last_used` cut: `last_used` is a frame number, many labels
            // share one, and a cut at a shared frame kept every label of
            // that frame -- more than `expensive_kept`. Which of a tied
            // frame's labels stay is arbitrary; how many is not.
            expensive.sort_unstable_by(|a, b| b.0.cmp(&a.0));
            let kept: HashSet<LabelKey> =
                expensive.into_iter().take(expensive_kept).map(|(_, key)| key.clone()).collect();
            let Self { entries, used_this_frame: used, evicted, .. } = self;
            forget_if_full(evicted);
            entries.retain(|key, _| {
                let keep = used.contains(key) || kept.contains(key);
                if !keep {
                    evicted.insert(label_key_hash(key));
                }
                keep
            });
            self.used_this_frame.clear();
            self.frame += 1;
            return;
        }
        self.end_frame_with_cap(cap);
    }

    /// `end_frame` with an explicit cap, so the trim is testable without
    /// shaping a thousand distinct strings to cross the real threshold.
    ///
    /// Deliberately not an LRU: `LruCache::get` takes `&mut self`, which
    /// cannot be called while the paint loop holds shared references to the
    /// cached buffers to build its `TextArea`s. Retaining exactly what the
    /// frame drew is both cheaper and a better fit for the access pattern
    /// (every visible label is touched every frame).
    pub(crate) fn end_frame_with_cap(&mut self, cap: usize) {
        if self.entries.len() > cap {
            let Self { entries, used_this_frame: used, evicted, .. } = self;
            forget_if_full(evicted);
            entries.retain(|key, _| {
                let keep = used.contains(key);
                if !keep {
                    evicted.insert(label_key_hash(key));
                }
                keep
            });
        }
        self.used_this_frame.clear();
        self.frame += 1;
    }
}


/// How many line-placed labels `symbol_line::place_glyphs_along` has
/// REFUSED over this process's life -- the label did not fit inside its
/// polyline at its anchor, or the line turned harder under it than
/// [`symbol_line::MAX_GLYPH_ANGLE_RAD`].
///
/// Dropping such a label is what MapLibre does (`get_anchors.ts` returns no
/// anchor at all), and it is the right answer: a run that overhangs the end
/// of its road loses its last glyphs silently, and one bent around a
/// hairpin overlaps itself. But a silent drop is how "the street names
/// stopped appearing" becomes an unfalsifiable report, so the refusals are
/// counted.
///
/// Process-wide and monotonic, like `render::alloc`'s own counters, because
/// the drop happens per feature per frame inside `basemap_candidate` and
/// there is no per-frame object down there to hang it on.
static LINE_LABELS_REFUSED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Held by every test that reads a DELTA of [`LINE_LABELS_REFUSED`] and by
/// every test that runs line labels through [`collect_candidates`] beside
/// them: the counter is process-wide and the test harness is not.
#[cfg(test)]
pub(crate) static LINE_LABELS_REFUSED_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// [`LINE_LABELS_REFUSED`]'s current value.
pub fn line_labels_refused() -> u64 {
    LINE_LABELS_REFUSED.load(std::sync::atomic::Ordering::Relaxed)
}

/// One shaped glyph of a line-placed label, as the quad builder needs it.
///
/// Copied out of `run.glyphs` rather than borrowed because the buffer it
/// comes from lives in `LabelShapeCache`, and rastering into the glyph
/// atlas needs `&mut FontSystem` -- which the shaped-buffer borrow and the
/// atlas call cannot both hold across.
#[derive(Debug, Clone, Copy)]
struct LineGlyph {
    key: crate::render::GlyphKey,
    /// This glyph's advance, in PHYSICAL pixels -- cosmic-text's
    /// `LayoutGlyph::w`. The buffer was shaped with `physical_text_size` as
    /// its `Metrics` size (see `LabelShapeCache::get_or_shape`), so every
    /// length off it is already physical.
    advance_px: f32,
    /// The glyph's own positioning offsets inside the run, in physical
    /// pixels -- cosmic-text's `x_offset`/`y_offset`, which glyphon adds to
    /// the pen position before the bitmap's own bearing. NOT scaled by
    /// `font_scale`: these are display-size quantities, where a
    /// `GlyphRect`'s bearing is in atlas texels.
    offset_px: (f32, f32),
}

/// The glyphs of a line-placed label, in reading order, off an
/// already-shaped buffer.
///
/// Only the FIRST layout run, and deliberately. A line-placed label is one
/// line by construction -- `text-max-width` is `None` under line placement
/// (see `style::SymbolFeature::text_max_width_em`), so nothing wraps -- and
/// a `text-field` that concatenated a literal newline would otherwise have
/// its second line laid along the same stretch of road, directly on top of
/// its first.
///
/// This is the per-glyph data the measurement walk in
/// [`measure_shaped_block`] has always iterated past: it reads `line_w` and
/// `line_top` off each run and never looks at `run.glyphs`.
fn line_glyphs(buffer: &glyphon::Buffer) -> Vec<LineGlyph> {
    let Some(run) = buffer.layout_runs().next() else { return Vec::new() };
    run.glyphs
        .iter()
        .map(|glyph| LineGlyph {
            key: crate::render::GlyphKey::for_glyph(glyph),
            advance_px: glyph.w,
            offset_px: (glyph.x_offset, glyph.y_offset),
        })
        .collect()
}

/// The two mutable resources rastering a glyph needs beyond the
/// `FontSystem` the symbol pass already carries.
///
/// A struct so [`basemap_candidate`] and [`collect_candidates`] gain ONE
/// argument rather than two, and so the pairing is named: the swash cache
/// must be the caller's own (`map_view`'s `glyphon_swash_cache`, the one
/// glyphon's `prepare` is already given), or the rasterisation work and the
/// resident bitmaps are both paid for twice.
///
/// Passed as an `Option`, and `None` is a real case rather than a
/// convenience: it means "decide where this label goes, but do not raster
/// it". [`label_census`] is that caller. See [`line_glyph_quads`] for
/// exactly what the two cases share.
pub(crate) struct GlyphSink<'a> {
    pub(crate) atlas: &'a mut crate::render::GlyphAtlas,
    pub(crate) swash: &'a mut glyphon::SwashCache,
}

/// Whether this run would read RIGHT TO LEFT on screen -- MapLibre's
/// `needsFlipping`, `src/symbol/projection.ts`:
///
/// ```text
/// if (writingMode === WritingMode.vertical ? firstPoint.y < lastPoint.y
///                                          : firstPoint.x > lastPoint.x) {
///     return {needsFlipping: true};
/// }
/// ```
///
/// Only the horizontal arm: this crate has no vertical writing mode, so
/// `writingMode` is always horizontal and the comparison is on x.
///
/// PROJECTED positions, not tile-space ones. The two differ under camera
/// rotation and pitch, and it is the screen the reader is looking at -- a
/// road digitised west-to-east reads backwards the moment the camera turns
/// past 90 degrees, and a rule written on the tile's own coordinates would
/// never notice.
///
/// `false` when either end fails to project (behind the near plane under
/// pitch), which leaves the run unflipped -- the same fallback
/// `symbol_angle_rad` takes for a forward reference it cannot project, and
/// for the same reason: such a label is at the very edge of the screen.
/// `false` for a single glyph, where `first` and `last` are the same point
/// and the strict `>` settles it.
fn reads_right_to_left(
    ctx: &SymbolFrame<'_>,
    anchors: &AnchorProjection,
    placements: &[crate::symbol_line::GlyphAlong],
) -> bool {
    let (Some(first), Some(last)) = (placements.first(), placements.last()) else { return false };
    let (Some(first), Some(last)) = (
        anchors.screen_at(ctx.camera, first.at.0, first.at.1),
        anchors.screen_at(ctx.camera, last.at.0, last.at.1),
    ) else {
        return false;
    };
    first.0 > last.0
}

/// `layout.text-rotation-alignment` for the layer a symbol feature came
/// from.
///
/// `text_rotation_alignment`, NOT `rotation_alignment`: the two compile
/// into separate fields and a style is free to set only one of them
/// (Liberty's `road-number-shield` sets both to `viewport`, its
/// `highway-shield-*` layers only the text one). Reading the icon field
/// here would rotate street names by whatever the shields asked for.
///
/// `Map` is the fallback for a layer index that resolves to nothing (or to
/// a non-symbol layer), which cannot happen -- the index came off a feature
/// this very frame's `style_layers` baked. It matches the value
/// `style_spec`'s `auto` resolution gives a LINE-placed layer, which is the
/// only placement that reaches the caller's gate.
///
/// Its value decides WHICH PATH a line-placed label takes, not merely how
/// its quads are angled -- see [`basemap_candidate`]'s gate and
/// [`line_glyph_quads`].
fn text_rotation_alignment(
    ctx: &SymbolFrame<'_>,
    layer_index: usize,
) -> crate::style::RotationAlignment {
    ctx.style_layers.get(layer_index).map_or(crate::style::RotationAlignment::Map, |layer| {
        match layer {
            style::StyleLayer::Symbol { text_rotation_alignment, .. } => *text_rotation_alignment,
            _ => crate::style::RotationAlignment::Map,
        }
    })
}

/// One rotated quad per inked glyph of a line-placed label, or `None` when
/// the label cannot be laid along its line at this anchor.
///
/// # This is the path for text that FOLLOWS the line, and only that
///
/// `alignment` is the layer's `layout.text-rotation-alignment`, read once
/// by the caller ([`text_rotation_alignment`]) and used here for the quad
/// angles and for [`glyph_run_collision_box`]. In practice it is always
/// [`crate::style::RotationAlignment::Map`], because the caller's gate
/// sends a `Viewport` label down the single-block path instead and it never
/// reaches this function -- which is what MapLibre does, where
/// `placeGlyphsAlongLine` is reached only for line-following text. It is a
/// parameter rather than a lookup so that the gate and the geometry cannot
/// come to different conclusions about the same layer: there is ONE read.
///
/// Distributing the glyphs along the polyline while holding each of them
/// level -- which is what ignoring `alignment` here amounts to -- is not a
/// compromise between the two modes. On a north-south road it stacks the
/// label one upright letter per row, and `BKE` in a motorway shield comes
/// out spelled downwards.
///
/// # What the caller has to get right, and what happens if it doesn't
///
/// A `GlyphRect`'s `width`, `height` and both halves of its `bearing` are
/// in ATLAS TEXELS at `glyph_atlas::RASTER_SIZE_PX`, not in display pixels
/// -- that is the point of a size-independent field. All four are scaled
/// here by `glyph_font_scale(physical_text_size)`, and THAT SAME VALUE goes
/// into `SdfPaint::font_scale`, because the shader divides the halo's width
/// and the edge's gamma by it to convert both back into field units
/// (`icon_pipeline::sdf_thresholds`). Scaling the quad by one number and
/// telling the shader another draws something -- at the wrong size, with a
/// halo threshold computed against a scale the quad does not have. It does
/// not look like a failure.
///
/// # Geometry
///
/// A run that would read RIGHT TO LEFT on screen is re-placed against the
/// reversed line first -- see [`reads_right_to_left`] and the keep-upright
/// block below -- so everything after that point is working with a run that
/// reads correctly.
///
/// [`line_glyph_placements`] returns each glyph's position ON THE LINE, at
/// the middle of its own advance, in physical pixels, and its angle -- the
/// screen walk on a pitched or globe frame, the tile walk on an unpitched
/// flat one. Everything else is built in a LOCAL frame whose +x runs along the line and whose +y runs
/// across it (y-down, the same sense as the screen), and then rotated into
/// place by the projected angle:
///
/// * `-advance_px / 2` walks back from the advance's middle to the glyph's
///   pen position.
/// * `offset_px` is cosmic-text's own per-glyph placement, then
///   `bearing * scale` is the bitmap's offset from the pen -- `left` to the
///   right, `top` ABOVE the baseline, hence the minus on y. Without the
///   bearing every glyph sits on its own baseline, and a "y" rides up level
///   with an "S".
/// * `+ half_cap` drops the baseline by half a cap height so the run's INK
///   is centred on the road rather than sitting entirely above it.
/// * `+ width / 2, + height / 2` converts the bitmap's top-left into the
///   quad CENTRE, which is the point `icon_shader.wgsl` rotates about.
///
/// # UVs come out in TEXELS
///
/// Every returned quad's `uv_min`/`uv_max` is in atlas TEXELS, not the
/// 0..1 the shader samples. [`normalise_glyph_uvs`] finishes them; nothing
/// else may draw them first.
///
/// # The collision box comes back with them
///
/// [`LineLabelPlacement::bounds`] is the union of the per-glyph boxes, and
/// it is computed on BOTH the drawing and the no-atlas paths -- see that
/// field, and [`glyph_run_collision_box`] for why it is built out of
/// advances rather than out of the `GlyphRect`s this function draws from.
#[allow(clippy::too_many_arguments)]
fn line_glyph_quads(
    ctx: &SymbolFrame<'_>,
    anchors: &AnchorProjection,
    feature: &style::SymbolFeature,
    line: &geo_types::LineString<i32>,
    glyphs: &[LineGlyph],
    physical_text_size: f32,
    // `text-halo-width` in physical pixels, from the caller, which has
    // already put this symbol's perspective ratio into it -- so a street
    // name's halo shrinks with its glyphs, as a point label's does.
    halo_width: f32,
    tile_units_per_physical_px: f64,
    alignment: crate::style::RotationAlignment,
    // `shade` -- the day/night multipliers for this feature's ANCHOR -- is
    // resolved once by `basemap_candidate` and passed in rather than
    // recomputed per glyph: a label is one thing and lights as one thing,
    // and a run whose letters each dotted their own normal against the sun
    // would develop a gradient along a road lying east-west at dusk.
    shade: crate::render::SymbolShade,
    font_system: &mut glyphon::FontSystem,
    sink: Option<&mut GlyphSink<'_>>,
) -> Option<LineLabelPlacement> {
    if glyphs.is_empty() {
        return None;
    }
    let advances_px: Vec<f32> = glyphs.iter().map(|glyph| glyph.advance_px).collect();
    let placements = line_glyph_placements(
        ctx,
        anchors,
        line,
        feature.line_centre_distance,
        &advances_px,
        tile_units_per_physical_px,
        alignment,
    )?;
    // The union of the per-glyph boxes -- the whole point of this task, and
    // computed here, ONCE, for both the drawing and the measuring caller.
    // See [`glyph_run_collision_box`].
    let bounds = glyph_run_collision_box(glyphs, &placements, physical_text_size);

    // No atlas: this caller is measuring PLACEMENT, not drawing. Everything
    // above this line has already run -- the advance conversion, the
    // does-it-fit test, the max-angle rejection and now the collision box --
    // so the refusal count, the set of surviving labels AND the space each
    // one reserves are exactly a drawing caller's. All that is skipped is
    // the rasterisation, which produces no number the census reports and
    // which a live frame pays once per glyph for the process's life
    // (`MapView` holds ONE `GlyphAtlas`), not once per frame. See
    // [`GlyphSink`].
    let Some(sink) = sink else { return Some(LineLabelPlacement { quads: Vec::new(), bounds }) };
    // Over this frame's rastering allowance with glyphs still to make: the
    // label waits for a later frame, whole. See `GlyphAtlas::admits`.
    if !sink.atlas.admits(glyphs.iter().map(|glyph| glyph.key)) {
        return None;
    }

    // Physical pixels per atlas texel -- the ONE scale, used for the quad
    // and handed to the shader. See this function's doc comment.
    let font_scale = crate::render::glyph_font_scale(physical_text_size);
    let half_cap = physical_text_size * LABEL_CAP_HEIGHT_RATIO / 2.0;
    // One label, one light: resolved once for the whole run. A DARK glyph
    // on the night side comes back as its night pair with `text-opacity`
    // folded in and `opacity` 1.0 -- see `render::SymbolShade::sdf_colors`.
    // A label over data keeps the on-data paint as it is: its ground is
    // the data and its own light halo, not the night side of the basemap,
    // so the night pair (light text in a dark halo) would undo it.
    let on_data = ctx.on_data_paint(anchors, feature);
    let (text_color, text_halo_color) = text_colors(on_data, feature);
    let (ink, halo, opacity) = match on_data {
        Some(_) => (text_color, text_halo_color, feature.text_opacity),
        None => shade.sdf_colors(text_color, text_halo_color, feature.text_opacity),
    };
    let mut quads: Vec<IconInstance> = Vec::with_capacity(glyphs.len());
    for (glyph, placement) in glyphs.iter().zip(&placements) {
        // `None` is a glyph this atlas will not draw -- every space in
        // every label, plus colour emoji and a full atlas. The rest of the
        // run still draws; the gap the space leaves is the gap it should
        // leave, since its advance was still spent above.
        let Some(rect) = sink.atlas.get_or_raster(sink.swash, font_system, glyph.key) else { continue };
        // `None` is a glyph whose position did not project (the tile walk
        // only); it is skipped AFTER the raster, as it always was, so the
        // atlas fills in the same order either way.
        let Some(placement) = placement else { continue };
        let (centre_x0, centre_y0) = placement.centre;
        let angle = placement.angle;

        let width = rect.width as f32 * font_scale;
        let height = rect.height as f32 * font_scale;
        let local_x = -glyph.advance_px / 2.0 + glyph.offset_px.0 + rect.bearing.0 * font_scale + width / 2.0;
        let local_y = half_cap + glyph.offset_px.1 - rect.bearing.1 * font_scale + height / 2.0;
        let (sin, cos) = (angle.sin(), angle.cos());
        let centre_x = centre_x0 + cos * local_x - sin * local_y;
        let centre_y = centre_y0 + sin * local_x + cos * local_y;

        quads.push(IconInstance {
            screen_x: centre_x - width / 2.0,
            screen_y: centre_y - height / 2.0,
            width,
            height,
            // ATLAS TEXELS, not 0..1 -- [`normalise_glyph_uvs`] divides
            // them once the frame's atlas has stopped growing. See that
            // function for why the division cannot happen here.
            uv_min: (rect.x as f32, rect.y as f32),
            uv_max: ((rect.x + rect.width) as f32, (rect.y + rect.height) as f32),
            rotation_rad: angle,
            // ALWAYS `Some`: this atlas holds a distance field, so the
            // quad has no colour of its own and the halo the five
            // `highway-name-*` layers ask for comes from the SDF branch's
            // second pass (`icon_pipeline::push_quad`). Colours go in
            // unfaded and `opacity` carries `text-opacity`, the way an
            // icon's does -- fading both would apply it twice.
            //
            // The NIGHT is the one thing applied to them here, and it is
            // the same `shade` the glyphon path applies to a point label --
            // a street name bent along its road must cross the terminator
            // with the city name beside it, not a frame later and not at a
            // different brightness. RGB takes `ink`, the halo's alpha takes
            // `halo_alpha`; see `render::SymbolLight`.
            sdf: Some(crate::render::SdfPaint {
                fill: [ink.r, ink.g, ink.b, ink.a],
                halo: [halo.r, halo.g, halo.b, halo.a],
                halo_width,
                font_scale,
            }),
            opacity,
        });
    }
    // An empty list is NOT a refusal: it is a label every glyph of which
    // the atlas would not draw (all spaces, all colour emoji, or a full
    // atlas). The label is still placed and still counted, exactly as it is
    // on the no-sink path above -- it simply draws nothing.
    Some(LineLabelPlacement { quads, bounds })
}

/// Where each glyph of a line-placed label lands on the screen -- the ONE
/// placement both [`line_glyph_quads`]'s quads and
/// [`glyph_run_collision_box`] are built from, so drawing, collision and
/// `label_census` cannot disagree.
///
/// `None` is a refusal and is counted in [`LINE_LABELS_REFUSED`]. An
/// element is `None` only on the tile walk, for a glyph whose tile position
/// did not project; such a glyph is skipped by both consumers.
pub(crate) fn line_glyph_placements(
    ctx: &SymbolFrame<'_>,
    anchors: &AnchorProjection,
    line: &geo_types::LineString<i32>,
    centre_distance: f64,
    advances_px: &[f32],
    tile_units_per_physical_px: f64,
    alignment: crate::style::RotationAlignment,
) -> Option<Vec<Option<crate::symbol_line::ScreenGlyph>>> {
    if advances_px.is_empty() || !tile_units_per_physical_px.is_finite() || tile_units_per_physical_px <= 0.0 {
        return None;
    }
    // AN UNPITCHED FLAT FRAME KEEPS THE TILE WALK, BY A BRANCH. There the
    // projection is a similarity, so the two walks are the same placement
    // (to f32 noise: the tile walk no longer snaps a glyph to a whole tile
    // unit, see `symbol_line::GlyphAlong`), and the tile walk keeps its own
    // keep-upright, refusal and occlusion rules unchanged. The same
    // precedent as `AnchorProjection::perspective_ratio`.
    if ctx.camera.pitch == 0.0 && anchors.globe.is_none() {
        let units = tile_units_per_physical_px;
        return tile_walk_glyphs(ctx, anchors, line, centre_distance, advances_px, units, alignment);
    }
    let placed = screen_walk_glyphs(ctx, anchors, line, centre_distance, advances_px, alignment);
    if placed.is_none() {
        LINE_LABELS_REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    placed.map(|glyphs| glyphs.into_iter().map(Some).collect())
}

/// The SCREEN-space glyph walk every pitched or globe frame takes: the
/// polyline is projected outward from the anchor and the advances -- which
/// are already screen lengths, the shaping size carrying the anchor's
/// perspective ratio -- are laid along the projected line
/// (`symbol_line::place_glyphs_along_screen_upright`, MapLibre's
/// `placeGlyphsAlongLine` for viewport-pitched text).
///
/// `None` when the anchor does not project, the run does not fit its
/// projected stretch, the tile line bends harder than
/// `symbol_line::MAX_GLYPH_ANGLE_RAD` under it, or any tile vertex of the
/// segments the glyphs sit on is behind the planet. That last test is on
/// segment ENDPOINTS rather than on the glyphs' own points (which have no
/// tile position on this walk), so near the limb it refuses a little
/// earlier than the tile walk did; it is still MapLibre-conservative -- see
/// the horizon comment in [`tile_walk_glyphs`].
fn screen_walk_glyphs(
    ctx: &SymbolFrame<'_>,
    anchors: &AnchorProjection,
    line: &geo_types::LineString<i32>,
    centre_distance: f64,
    advances_px: &[f32],
    alignment: crate::style::RotationAlignment,
) -> Option<Vec<crate::symbol_line::ScreenGlyph>> {
    let (segment, anchor_tile) = crate::symbol_line::point_at_distance(line, centre_distance)?;
    let scale = f64::from(ctx.scale_factor);
    let project = |(x, y): (f64, f64)| {
        anchors.screen_at(ctx.camera, x, y).map(|(sx, sy)| (f64::from(sx) * scale, f64::from(sy) * scale))
    };
    let anchor_screen = project(anchor_tile)?;
    let advances: Vec<f64> = advances_px.iter().map(|advance| f64::from(*advance)).collect();
    let run: f64 = advances.iter().sum();
    let projected =
        crate::symbol_line::project_line_around(line, segment, anchor_tile, anchor_screen, run / 2.0, project)?;
    let mut placed = crate::symbol_line::place_glyphs_along_screen_upright(
        &projected,
        line,
        &advances,
        crate::symbol_line::MAX_GLYPH_ANGLE_RAD,
    )?;

    // The planet in the way: refused whole, as the tile walk refuses it.
    if anchors.occluded_at(anchor_tile.0, anchor_tile.1)
        || projected.segments.iter().any(|&seg| {
            line.0[seg..=seg + 1].iter().any(|vertex| anchors.occluded_at(f64::from(vertex.x), f64::from(vertex.y)))
        })
    {
        return None;
    }
    if !matches!(alignment, crate::style::RotationAlignment::Map) {
        for glyph in &mut placed {
            glyph.angle = 0.0;
        }
    }
    Some(placed)
}

/// The TILE-space glyph walk: advances converted to tile units through one
/// flat constant, walked along the tile polyline, each glyph projected.
fn tile_walk_glyphs(
    ctx: &SymbolFrame<'_>,
    anchors: &AnchorProjection,
    line: &geo_types::LineString<i32>,
    centre_distance: f64,
    advances_px: &[f32],
    tile_units_per_physical_px: f64,
    alignment: crate::style::RotationAlignment,
) -> Option<Vec<Option<crate::symbol_line::ScreenGlyph>>> {
    // `place_glyphs_along` walks the polyline in TILE units, so every
    // advance has to leave physical-pixel space on the way in. The
    // conversion is the frame's own, not the bake zoom's: the plan was
    // baked at an integer zoom but the camera moves continuously inside
    // that bucket, and a glyph's QUAD is drawn at the screen size the
    // camera implies. Spacing the glyphs by the bake zoom's scale instead
    // would let the letters overlap (or gap) by up to 2x by the end of a
    // bucket.
    let advances: Vec<f64> =
        advances_px.iter().map(|advance| *advance as f64 * tile_units_per_physical_px).collect();
    let placements = crate::symbol_line::place_glyphs_along(
        line,
        centre_distance,
        &advances,
        crate::symbol_line::MAX_GLYPH_ANGLE_RAD,
    );
    let Some(mut placements) = placements else {
        LINE_LABELS_REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return None;
    };
    // KEEP UPRIGHT. A road's polyline runs whichever way it was digitised,
    // and half of them were digitised east to west -- so half of every
    // street name would be laid out along the line and come out reading
    // backwards, upside down. MapLibre's rule, `src/symbol/projection.ts`:
    //
    //     if (writingMode === WritingMode.vertical ? firstPoint.y < lastPoint.y
    //                                              : firstPoint.x > lastPoint.x) {
    //         return {needsFlipping: true};
    //     }
    //
    // and the shape around it (`projection.ts:299-326`): place unflipped;
    // if that needs flipping, place again flipped; if the FLIPPED attempt
    // has `notEnoughRoom` either, `hideGlyphs` -- draw nothing rather than
    // a bad label.
    //
    // Two details that are easy to get wrong. The test is on PROJECTED
    // screen positions, not on tile-space geometry: a rotated or pitched
    // camera can turn a run that reads left-to-right in the tile into one
    // that reads right-to-left on screen, and it is the screen that the
    // reader is looking at. And the flip is not a reordering -- the same
    // run placed against the REVERSED line also turns every `forward`
    // around, which is what puts the glyphs the right way up rather than
    // merely putting them back in reading order.
    //
    // `layout.text-keep-upright` is NOT compiled, deliberately. The
    // style-spec default is `true` and no style this repository carries --
    // `examples/liberty-style.json` included -- sets it on any layer, so
    // the property is dead work until one asks for `false`.
    // This is the `true` branch, unconditionally.
    if reads_right_to_left(ctx, anchors, &placements) {
        let total = crate::symbol_line::line_length(line);
        // The same anchor addressed from the other end. `centre_distance`
        // is measured from the part's START to the anchor point (see
        // `symbol_line::PartAnchor::centre_distance`), so the reversed
        // line's own measure of it is the complement -- pinned by
        // `a_run_placed_against_the_reversed_line_is_the_same_run_backwards`.
        let reversed = geo_types::LineString(line.0.iter().rev().copied().collect());
        let Some(flipped) = crate::symbol_line::place_glyphs_along(
            &reversed,
            total - centre_distance,
            &advances,
            crate::symbol_line::MAX_GLYPH_ANGLE_RAD,
        ) else {
            // MapLibre's `hideGlyphs`: the run reads backwards and will not
            // fit the other way round either, so nothing is drawn. Counted
            // as a refusal like any other drop -- it IS one.
            LINE_LABELS_REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return None;
        };
        placements = flipped;
    }
    // A RUN THAT CROSSES THE HORIZON IS NOT DRAWN AT ALL, and this is the
    // one place this task deliberately does less than the reference.
    //
    // MapLibre slices the projected path to its longest continuous
    // unoccluded segment and re-places the glyphs along THAT
    // (`pathSlicedToLongestUnoccluded`, `symbol/projection.ts:978-995`), so a
    // street crossing the limb keeps the half of its name that is still on
    // the near side. Doing the same here means clipping the polyline in TILE
    // space and re-running `place_glyphs_along` against a shortened line
    // with a re-solved `line_centre_distance`, which is a second placement
    // pass and its own set of refusal cases.
    //
    // What is NOT acceptable is the middle option -- dropping the occluded
    // glyphs and keeping the rest -- because `place_glyphs_along` spaced the
    // survivors for a complete word, so the result reads as a misspelling
    // rather than as a clipped label. Refusing the whole run is the
    // conservative end of the same choice, it is the behaviour this function
    // already has for every other placement it cannot honour (see
    // `hideGlyphs` above), and at z0-z5 -- the only zooms with a horizon --
    // there are very few line-placed labels to refuse.
    //
    // Counted as a refusal, like the others, so `line_labels_refused` still
    // adds up over a frame.
    if placements.iter().any(|placement| anchors.occluded_at(placement.at.0, placement.at.1)) {
        LINE_LABELS_REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return None;
    }
    Some(
        placements
            .iter()
            .map(|placement| {
                // UNROUNDED: a glyph snapped to a whole tile unit is off
                // its baseline by up to half of one, 6 physical px at z19
                // on a z14 tile at 3x -- see `symbol_line::GlyphAlong`.
                let (anchor_x, anchor_y) = anchors.screen_at(ctx.camera, placement.at.0, placement.at.1)?;
                Some(crate::symbol_line::ScreenGlyph {
                    centre: (anchor_x * ctx.scale_factor, anchor_y * ctx.scale_factor),
                    angle: line_glyph_angle(ctx, anchors, placement, alignment),
                })
            })
            .collect(),
    )
}

/// What one line-placed label came out of [`line_glyph_quads`] as: the
/// quads to draw, and the screen space to reserve for them.
///
/// The two travel together because they are two readings of one placement.
/// An earlier shape of this code returned only the quads and let
/// `basemap_candidate` build the box from `text_area_origin` -- i.e. from
/// where the label WOULD have gone had it been drawn horizontally at its
/// anchor -- which is how a street name bent around a corner came to
/// reserve a wide flat rectangle it did not occupy.
struct LineLabelPlacement {
    /// One rotated quad per inked glyph. Empty on the no-atlas path, and
    /// empty for a label the atlas draws nothing of; neither is a refusal.
    quads: Vec<IconInstance>,
    /// The union of this run's per-glyph boxes, in PHYSICAL pixels --
    /// [`glyph_run_collision_box`]'s output. `None` when no glyph of the
    /// run projected, which leaves `basemap_candidate` to fall back to the
    /// straight-run box.
    bounds: Option<CollisionBox>,
}

/// The union AABB of a line-placed run's per-glyph boxes, in PHYSICAL
/// pixels -- what a curved label actually covers, rather than what a
/// straight one at the same anchor would have.
///
/// # This is the FIRST CUT, and deliberately so
///
/// One axis-aligned rectangle around a bent run is strictly better than the
/// straight-run box it replaces (a run laid down a near-vertical street now
/// reserves a tall thin box instead of a wide flat one) and strictly worse
/// than what MapLibre does, which is a CHAIN of circles along the line
/// (`CollisionFeature`'s `bboxifyLabel`). A label bent into a deep V still
/// reserves the whole triangle here, including the empty middle. That is a
/// later plan; see `docs/` and this task's report.
///
/// # Why ADVANCES and not `GlyphRect`s
///
/// This function never touches the glyph atlas, and it must not: its result
/// takes part in COLLISION, and collision has to come out the same for
/// `label_census` -- which deliberately rasters nothing (see [`GlyphSink`])
/// -- as it does for a drawing frame. A box built from the inked
/// `GlyphRect` would exist only on the drawing path, and the census, which
/// is this project's placement instrument, would silently stop describing
/// the frame it models.
///
/// So each glyph contributes its LAYOUT box: its own advance wide (the
/// glyph sits at the middle of its advance -- `place_glyphs_along`'s
/// contract) and one line box tall, `physical_text_size *
/// LABEL_LINE_HEIGHT_RATIO`, centred across the line. That is the same
/// width and the same height the straight-run box was built from
/// (`ShapedLabel::measured_width` is the sum of these advances,
/// `measured_height` is one line box for a one-line label, and a
/// line-placed label is one line by construction), so at a bearing of zero
/// this reduces to very nearly the old box and the change is purely the
/// rotation and the bend.
///
/// A consequence worth stating: the box is tighter than the old one but not
/// tight around the INK. A run of lowercase letters reserves full line
/// boxes, exactly as a point label does.
///
/// # Geometry
///
/// Each glyph's box is the axis-aligned bounding box of its own ROTATED
/// layout rectangle, which is precisely what [`icon_collision_box`] already
/// computes (`w' = |w cos t| + |h sin t|`), taken about the glyph's own
/// projected anchor. The angle is [`symbol_angle_rad`]'s, off the same two
/// projected points the quad is built from, so the box turns with the quad
/// under camera rotation and pitch without any bearing arithmetic.
///
/// Glyphs that fail to project are skipped, as they are in the quad loop.
/// `None` when every one of them does, which means the run is behind the
/// near plane.
fn glyph_run_collision_box(
    glyphs: &[LineGlyph],
    placements: &[Option<crate::symbol_line::ScreenGlyph>],
    physical_text_size: f32,
) -> Option<CollisionBox> {
    // The BLOCK height a one-line label measures, which is what the
    // straight-run box used and what `text_area_origin` centres on.
    let line_box = physical_text_size * LABEL_LINE_HEIGHT_RATIO;
    let mut union: Option<CollisionBox> = None;
    for (glyph, placement) in glyphs.iter().zip(placements) {
        let Some(placement) = placement else { continue };
        let box_for_glyph = icon_collision_box(
            glyph.advance_px,
            line_box,
            placement.angle,
            placement.centre.0,
            placement.centre.1,
        );
        union = Some(match union {
            Some(so_far) => so_far.union(box_for_glyph),
            None => box_for_glyph,
        });
    }
    union
}

/// Turns the ATLAS-TEXEL uv rects [`line_glyph_quads`] emits into the 0..1
/// the shader samples, against the glyph atlas's size for THIS frame.
///
/// # Why this is a second pass and not a division at the source
///
/// Because the divisor is not known at the source. `GlyphAtlas` is filled
/// INCREMENTALLY -- a glyph at a time, as the symbol pass asks for it --
/// and it grows downwards in power-of-two steps as it fills. So the atlas
/// is 256 rows tall while the first tile's street names are being built and
/// 512 by the time the last tile's are, while the texture that gets
/// uploaded is ONE snapshot at the final height. Dividing per glyph would
/// give every quad built before a growth step a `v` twice as large as the
/// texture it samples: the whole label draws, from the wrong place in the
/// atlas, which is a smear of neighbouring glyphs rather than a blank.
///
/// Called once, beside the upload that establishes the dimensions, so the
/// texture's size and the UVs' divisor are the same two numbers on adjacent
/// lines.
///
/// A zero-sized atlas (nothing ever rastered) leaves the quads alone: there
/// is nothing to sample and no caller has quads to normalise in that state.
pub(crate) fn normalise_glyph_uvs(instances: &mut [IconInstance], atlas_width: u32, atlas_height: u32) {
    if atlas_width == 0 || atlas_height == 0 {
        return;
    }
    let (w, h) = (atlas_width as f32, atlas_height as f32);
    for instance in instances {
        instance.uv_min = (instance.uv_min.0 / w, instance.uv_min.1 / h);
        instance.uv_max = (instance.uv_max.0 / w, instance.uv_max.1 / h);
    }
}

/// Tile units per PHYSICAL pixel for a tile at `tile_z`, with the camera at
/// `camera_zoom`.
///
/// `symbol_line::tile_units_per_px` with two differences, both of which the
/// glyph walk needs and the anchor walk does not:
///
/// * a CONTINUOUS zoom, not the integer bake zoom. `symbol-spacing` is
///   baked once per zoom bucket and may drift inside it (that function's
///   own doc says so), but a glyph's quad is drawn at the size the camera
///   implies THIS FRAME, so spacing the glyphs at the bucket's scale would
///   let the letters overlap or gap by up to 2x.
/// * physical rather than logical pixels, because the advances it converts
///   come off a buffer shaped at `physical_text_size`.
///
/// Only the unpitched flat frame's tile walk uses it: that frame's
/// projection has one scale everywhere. A pitched frame has none, which is
/// why [`screen_walk_glyphs`] spaces glyphs on the projected line instead.
pub(crate) fn tile_units_per_physical_px(camera_zoom: f64, tile_z: u8, scale_factor: f32) -> f64 {
    let scale_factor = if scale_factor.is_finite() && scale_factor > 0.0 { scale_factor as f64 } else { 1.0 };
    MVT_EXTENT as f64
        / (crate::geo::TILE_SIZE * 2.0f64.powf(camera_zoom - tile_z as f64) * scale_factor)
}

/// What one `style::SymbolFeature` turned into.
///
/// Three outcomes, not two: a feature can produce nothing at all (no text
/// and no atlas-backed icon, off screen, or behind the near plane), and
/// such a feature must not consume a placement slot -- see the
/// `collision::symbol_box` call in [`basemap_candidate`].
pub(crate) enum SymbolOutcome {
    /// Offer this to the collision index.
    Candidate(SymbolCandidate),
    /// Draw this unconditionally, after placement -- it neither competes
    /// for a slot nor blocks one. See [`symbol_bypasses_collision`].
    Bypassed(DrawnSymbol),
    /// Nothing to draw and nothing to place.
    Skipped,
}

/// Everything [`basemap_candidate`] needs that is the same for every feature
/// of every tile in one frame.
///
/// A struct rather than eight arguments because the call is inside a hot
/// double loop and every one of these is loop-invariant: naming them once
/// makes it obvious at the call site that nothing per-feature has been
/// smuggled in.
pub(crate) struct SymbolFrame<'a> {
    pub(crate) camera: &'a Camera,
    /// The compiled style's layers, read ONLY for
    /// `RotationAlignment` -- `SymbolFeature` carries everything else it
    /// needs, and the layer lookup is a miss-tolerant `.get`.
    pub(crate) style_layers: &'a [style::StyleLayer],
    /// Physical pixels per logical pixel: `Window::scale_factor()` on the
    /// native build, `devicePixelRatio` in a browser. Places every symbol.
    pub(crate) scale_factor: f32,
    /// Physical pixels per layout pixel of a symbol's SIZE -- its text,
    /// halo, icon and `icon-text-fit` padding (times the perspective
    /// ratio); its position stays in [`Self::scale_factor`]. Equal to it
    /// everywhere but a live map under a story frame, whose basemap labels
    /// are sized at the story's ratio (`MapView::paint`,
    /// `live_story_ratio`) so the preview's labels are the export's (spec
    /// §13, ruling R11-b).
    pub(crate) size_scale: f32,
    /// [`SYMBOL_COLLISION_PADDING_LOGICAL_PX`] already scaled to physical.
    pub(crate) collision_padding: f32,
    /// Where the camera sits inside the zoom bucket the plans were baked
    /// at -- see [`blended_icon_size`].
    pub(crate) size_blend_t: f32,
    pub(crate) icon_rects: &'a HashMap<String, IconRect>,
    /// `(0, 0)` when no sprite atlas has been packed, which is the normal
    /// state of a build that never called `set_sprites` -- every
    /// icon-bearing feature then draws its text and skips its icon.
    pub(crate) icon_atlas_size: (u32, u32),
    /// This frame's globe, built ONCE (from `paint`'s one `globe_frame`
    /// call) and asked one tile at a time by [`AnchorProjection::new`].
    ///
    /// `GlobeAnchors::FLAT` above the globe band, and then every symbol in
    /// the frame takes the path it took before the globe existed. It is a
    /// field rather than something derived from `camera` here precisely so
    /// that it CANNOT be derived here: a second `globe_frame` call per frame
    /// is the per-frame redundancy `render::globe`'s module docs exist to
    /// keep out, and `paint_calls_globe_frame_exactly_once_and_before_its_tile_loops`
    /// counts the call sites.
    pub(crate) globe: crate::render::globe::GlobeAnchors,
    /// **The day/night terminator, for the text rather than the ground.**
    ///
    /// Built once per frame from the SAME resolved sun `GlobeUpload::new`
    /// is handed, beside [`Self::globe`] and for the same reason: the
    /// subsolar point moves 0.0042 degrees a second, and a second clock
    /// read inside one frame would light a label from a different instant
    /// from the coastline under it.
    ///
    /// `render::SymbolLight::UNLIT` — both multipliers a literal `1.0` — is
    /// what a flat frame, a census and a consumer with no atmosphere get,
    /// and it is what makes "labels off the globe are unchanged" structural.
    pub(crate) light: crate::render::SymbolLight,
    /// THIS FRAME's horizon fade, the very value the shell wrote into the
    /// camera uniform (`cover_plan::frame_fog`). A symbol whose anchor is
    /// at or past `fog.end` is not placed -- it would float over pure
    /// background, the ground under it faded away -- and one inside the
    /// band fades with the ground. `HorizonFog::NONE` (`end == 0`) below 50
    /// degrees of pitch and on every globe frame, where it changes nothing.
    pub(crate) fog: crate::render::HorizonFog,
    /// **Buildings in front of a label** (`ExtrusionOverrides::label_occlusion`):
    /// a symbol (`extrusion::OcclusionLayers`: every symbol layer by
    /// default -- icons, shields, house numbers, arrows, road and place
    /// names; line-placed ones by their anchor) whose anchor a building
    /// hides from the eye is not placed, so it neither draws nor takes a
    /// visible label's space; on a live map it fades out over
    /// `extrusion::OCCLUSION_FADE_SECONDS` first. MapLibre draws every
    /// symbol over its extrusions; Mapbox GL JS v3 hides symbols behind
    /// its 3D buildings, which is what this follows. `None` -- every frame
    /// without buildings or with the option off -- places exactly as before.
    pub(crate) occlusion: Option<&'a crate::render::extrusion::BuildingOcclusion>,
    /// **Labels over data** (`MapView::set_basemap_label_paint`): the text
    /// colour and halo every basemap label draws with instead of its
    /// style's, while a host has data fills under the labels. `None` --
    /// every map without such data -- draws each label as its style says.
    pub(crate) label_paint: Option<LabelsOnData<'a>>,
}

/// [`SymbolFrame::label_paint`]: the on-data paint, and which labels take it.
#[derive(Clone, Copy)]
pub(crate) struct LabelsOnData<'a> {
    pub(crate) paint: crate::style::LabelPaint,
    /// Whether the label anchored at `(tile, anchor)` (tile extent units)
    /// sits over a data fill, so it takes [`Self::paint`]; one off the
    /// data keeps its style's. `None`: every label takes it.
    pub(crate) over_data: Option<&'a dyn Fn(TileId, (i32, i32)) -> bool>,
}

impl SymbolFrame<'_> {
    /// The paint `feature`'s text draws with this frame: the on-data
    /// paint for a label over a data fill, `None` (its style's) otherwise.
    fn on_data_paint(&self, anchors: &AnchorProjection, feature: &style::SymbolFeature) -> Option<crate::style::LabelPaint> {
        let on = self.label_paint?;
        match on.over_data {
            Some(over) => over(anchors.tile, (feature.anchor.x(), feature.anchor.y())).then_some(on.paint),
            None => Some(on.paint),
        }
    }

    fn icon_atlas_width(&self) -> u32 {
        self.icon_atlas_size.0
    }

    fn icon_atlas_height(&self) -> u32 {
        self.icon_atlas_size.1
    }
}

/// `feature`'s text colour and halo colour under `paint` (the on-data
/// paint, when the label takes it) or its style's own.
fn text_colors(paint: Option<crate::style::LabelPaint>, feature: &style::SymbolFeature) -> (crate::style::Rgba, crate::style::Rgba) {
    match paint {
        Some(paint) => (paint.text_color, paint.halo_color),
        None => (feature.text_color, feature.text_halo_color),
    }
}

/// `feature`'s halo width and blur in layout pixels (see [`text_colors`]).
fn text_halo(paint: Option<crate::style::LabelPaint>, feature: &style::SymbolFeature) -> (f32, f32) {
    match paint {
        Some(paint) => (paint.halo_width, paint.halo_blur),
        None => (feature.text_halo_width, feature.text_halo_blur),
    }
}

/// How much of a symbol at `anchor` the horizon fade leaves visible:
/// `1 - smoothstep(fog.start, fog.end, depth)`, exactly the ground's own
/// fade inverted, or `None` -- do not place it -- when the anchor is at or
/// past `fog.end`. `Some(1.0)` whenever the fade is off (`end == 0`),
/// without computing a depth.
///
/// A globe frame's fade is off (`cover_plan::frame_fog`) except an eye
/// camera's distance fog (`cover_plan::eye_fog`), which a high eye keeps
/// below z12, where the projection already blends toward the sphere. The
/// shaders fade by the FLAT clip `w` there too (`fog_depth`, taken before
/// `project_to_sphere`), so the label takes the same flat depth and fades
/// with the ground under it instead of floating crisp over the fog.
pub(crate) fn horizon_visibility(
    fog: crate::render::HorizonFog,
    anchors: &AnchorProjection,
    anchor: geo_types::Point<i32>,
) -> Option<f32> {
    if fog.end <= 0.0 {
        return Some(1.0);
    }
    let depth = anchors.depth(anchor);
    // `!(<)` rather than `>=`, so a NaN depth is refused too.
    if !(depth < fog.end) {
        return None;
    }
    let t = ((depth - fog.start) / (fog.end - fog.start)).clamp(0.0, 1.0);
    Some(1.0 - t * t * (3.0 - 2.0 * t))
}

/// Projects, shapes, positions and boxes ONE basemap symbol -- everything
/// between a baked `style::SymbolFeature` and the collision index.
///
/// This is the function the native and browser frontends have in common,
/// and it is the reason `symbol_pass.rs` exists as a file: the arithmetic
/// that decides where a street name lands is written once. Both callers are
/// the same two-deep loop -- tiles outside, `symbol_plan.features` inside,
/// with `combined_tile_matrix` hoisted to the tile.
///
/// It shapes as a side effect: a cache miss in `shape_cache` allocates and
/// shapes a `glyphon::Buffer`, and the returned `SymbolCandidate` carries
/// only the KEY to it. Pass two -- building `glyphon::TextArea`s -- looks
/// the buffers back up under a shared borrow, which is why the key and not
/// the buffer travels. See [`LabelShapeCache`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn basemap_candidate(
    ctx: &SymbolFrame<'_>,
    anchors: &AnchorProjection,
    tile_z: u8,
    feature: &style::SymbolFeature,
    font_system: &mut glyphon::FontSystem,
    shape_cache: &mut LabelShapeCache,
    mut glyphs: Option<&mut GlyphSink<'_>>,
) -> SymbolOutcome {
    if feature.text.is_none() && feature.icon.is_none() {
        return SymbolOutcome::Skipped;
    }
    let Some((screen_x, screen_y)) = anchors.screen(ctx.camera, feature.anchor) else {
        return SymbolOutcome::Skipped;
    };
    if screen_x < -SYMBOL_CULL_MARGIN_PX
        || screen_y < -SYMBOL_CULL_MARGIN_PX
        || screen_x > ctx.camera.viewport_width_px + SYMBOL_CULL_MARGIN_PX
        || screen_y > ctx.camera.viewport_height_px + SYMBOL_CULL_MARGIN_PX
    {
        return SymbolOutcome::Skipped;
    }
    // THE PLANET IS IN THE WAY. A feature on the far hemisphere projects to
    // a perfectly ordinary pixel -- often one well inside the viewport, over
    // the middle of the globe -- so the cull test above cannot see it, and
    // Sydney was drawn over the Atlantic. The far side is rejected by an
    // analytic plane test and nothing else: no depth buffer, no backface
    // cull. See `AnchorProjection::occluded`, and
    // `vertical_perspective_transform.ts:428-430` for the test itself.
    //
    // Rejected HERE rather than at placement, which is one step earlier than
    // MapLibre (`collision_index.ts:166` marks the box unplaceable, and
    // `placement.ts:831-833` refuses it even under `overlap: always`). The
    // effect is the same and it is the effect that matters: an occluded
    // symbol neither draws nor reserves space, so it cannot evict a label
    // that IS visible. Doing it earlier only saves the shaping.
    //
    // After the cull, not before it, because the cull already holds the
    // screen position this function had to compute anyway, while the plane
    // test is work that only a globe frame does at all.
    if anchors.occluded(feature.anchor) {
        return SymbolOutcome::Skipped;
    }
    // PAST THE FOG, NOTHING. The horizon fade takes the ground to the
    // background colour by `fog.end`, so a label anchored there would float
    // over nothing; inside the band it fades with the ground under it.
    // `fog.end == 0` (every frame below 50 degrees, every globe frame)
    // skips this entirely, so those frames place and draw exactly as they
    // did.
    let faded_by_fog;
    let feature = match horizon_visibility(ctx.fog, anchors, feature.anchor) {
        None => return SymbolOutcome::Skipped,
        Some(visibility) if visibility < 1.0 => {
            faded_by_fog = style::SymbolFeature {
                text_opacity: feature.text_opacity * visibility,
                icon_opacity: feature.icon_opacity * visibility,
                ..feature.clone()
            };
            &faded_by_fog
        }
        Some(_) => feature,
    };
    // BEHIND A BUILDING, NOTHING -- for every symbol, point- or line-placed:
    // a bus stop, a shield, a house number, a one-way arrow or a street
    // name whose anchor a building hides from the eye. See
    // `SymbolFrame::occlusion`; before shaping, like the two gates above.
    // Mid-fade (a live map, `BuildingOcclusion::label_visibility`) the
    // symbol is placed with its opacity scaled, as the fog does above.
    let faded_by_building;
    let feature = match ctx.occlusion {
        Some(occlusion) => {
            let visibility =
                occlusion.label_visibility(anchors.tile, [feature.anchor.x(), feature.anchor.y()], feature.layer_index);
            if !(visibility > 0.0) {
                return SymbolOutcome::Skipped;
            }
            if visibility < 1.0 {
                faded_by_building = style::SymbolFeature {
                    text_opacity: feature.text_opacity * visibility,
                    icon_opacity: feature.icon_opacity * visibility,
                    ..feature.clone()
                };
                &faded_by_building
            } else {
                feature
            }
        }
        None => feature,
    };

    // **THE NIGHT SIDE OF THE PLANET IS DARK, AND THE LABELS ON IT ARE NOT
    // PASTED ON GLASS IN FRONT OF IT.**
    //
    // `shader.wgsl` multiplies the ground by the sun and nothing multiplied
    // the text over it, so at the default night brightness the night
    // hemisphere ran at a quarter while every label on it stayed full
    // white. This is the other half of that terminator: the SAME smoothstep
    // over the SAME band against the SAME `dot(normal, sun)`, with a raised
    // floor so a night label is dimmer rather than gone. See
    // `render::SymbolLight` for the floors and the contrast they were
    // measured to.
    //
    // Resolved ONCE per feature, above the icon and the text, because both
    // halves take it and a second `sphere()` call is an `exp`, a `sin` and
    // a `cos` for an answer we already have. On a flat frame
    // `anchors.sphere` is `None` and `shade` is `SymbolShade::UNSHADED`,
    // whose two multipliers are literal `1.0`s -- so every colour below is
    // bit-identical to the colour that reached it before this existed.
    let shade = ctx.light.shade(anchors.sphere(feature.anchor));

    // PERSPECTIVE. A symbol's size, in physical pixels per logical one:
    // the display's scale factor times MapLibre's perspective ratio, so a
    // street name toward the horizon draws -- and collides -- smaller than
    // one under the camera. Positions stay in `ctx.scale_factor`: the ratio
    // sizes a symbol about its anchor, it does not move the anchor. The
    // size is `ctx.size_scale`, the scale factor but under a live story
    // frame (`SymbolFrame::size_scale`).
    let symbol_scale = ctx.size_scale * anchors.perspective_ratio(ctx.camera, feature.anchor);

    // The icon quad and its collision box, or `(None, None)`.
    let mut icon_draw: Option<IconInstance> = None;
    let mut icon_box: Option<CollisionBox> = None;
    // The icon's native physical size and rotation, for `icon-text-fit`
    // below, which resizes it once the label is measured.
    let mut icon_native: Option<(f32, f32, f32)> = None;
    if let Some(icon_name) = &feature.icon {
        // `ctx.icon_atlas_width() == 0` means no sprite atlas has been
        // packed yet at all (see `icon_atlas_size`'s doc
        // comment) -- distinct from "this particular icon name
        // isn't in the atlas" (a data-driven `icon-image` this
        // fetched sheet doesn't happen to contain), which
        // `icon_rects.get` below handles by simply skipping the
        // feature's icon (its text, if any, still draws).
        if ctx.icon_atlas_width() > 0 {
            if let Some(rect) = ctx.icon_rects.get(icon_name).copied() {
                // Icons render at their native size scaled by
                // `icon-size` -- taken continuously across the
                // zoom bucket, not snapped to its floor; see
                // `blended_icon_size` -- in
                // LOGICAL points (the same convention
                // `feature.text_size` already uses) and then
                // scaled to physical the same way -- see
                // `icon_logical_size` for why the rect's atlas
                // pixels are not themselves logical points.
                // Centered on the anchor, matching Mapbox's
                // default `icon-anchor: center` (same reasoning
                // as the text-centering fix above).
                let icon_size = blended_icon_size(
                    feature.icon_size_from,
                    feature.icon_size_to,
                    ctx.size_blend_t,
                );
                let (logical_width, logical_height) = icon_logical_size(&rect, icon_size);
                let physical_width = logical_width * symbol_scale;
                let physical_height = logical_height * symbol_scale;
                let physical_left = screen_x * ctx.scale_factor - physical_width / 2.0;
                let physical_top = screen_y * ctx.scale_factor - physical_height / 2.0;
                let atlas_w = ctx.icon_atlas_width() as f32;
                let atlas_h = ctx.icon_atlas_height() as f32;

                // Consumed only by icon-bearing features (the
                // quad's rotation and its collision box below),
                // so computed here rather than above for every
                // symbol -- including up to 120Hz for every
                // text-only feature of every visible tile, which
                // never reads it. Projected through the SAME
                // per-tile matrix as `feature.anchor` above, so
                // both points share one perspective projection.
                // The anchor already passed the cull check
                // above, but the forward reference is a separate
                // tile-space point a bit further along the line
                // -- under pitch it can still land behind the
                // near plane and project to `None`.
                // `symbol_angle_rad` falls back to an unrotated
                // icon in that case, which is fine: such a
                // symbol is right at the screen's edge anyway.
                let forward_screen = feature.forward_ref.and_then(|forward| {
                    anchors.screen(ctx.camera, forward)
                });
                let rotation_rad = symbol_angle_rad(
                    (screen_x, screen_y),
                    forward_screen,
                    feature.icon_rotate_deg,
                    ctx.style_layers.get(feature.layer_index).map_or(
                        crate::style::RotationAlignment::Viewport,
                        |layer| match layer {
                            style::StyleLayer::Symbol { rotation_alignment, .. } => *rotation_alignment,
                            _ => crate::style::RotationAlignment::Viewport,
                        },
                    ),
                );

                // `paint.icon-opacity`. A zero one emits NO
                // quad -- an invisible sprite should not cost
                // vertices, and Mapbox's own `draw_symbol.ts`
                // skips the icon pass outright on
                // `icon-opacity == 0` -- but the collision box
                // below is still built, deliberately. See the
                // note on `icon_box` for why.
                if feature.icon_opacity > 0.0 {
                    // A DARK SDF icon on the night side turns into its night
                    // pair, as a dark glyph does; a raster sprite's colour
                    // is its texel and is never touched, so no icon is
                    // crushed toward black (`SymbolShade::icon_colors`).
                    let (icon_fill, icon_halo, icon_opacity) = if rect.sdf {
                        shade.icon_colors(
                            feature.icon_color,
                            feature.icon_halo_color,
                            feature.icon_opacity,
                        )
                    } else {
                        (feature.icon_color, feature.icon_halo_color, feature.icon_opacity)
                    };
                    icon_draw = Some(IconInstance {
                        screen_x: physical_left,
                        screen_y: physical_top,
                        width: physical_width,
                        height: physical_height,
                        uv_min: (rect.x as f32 / atlas_w, rect.y as f32 / atlas_h),
                        uv_max: (
                            (rect.x + rect.width) as f32 / atlas_w,
                            (rect.y + rect.height) as f32 / atlas_h,
                        ),
                        rotation_rad,
                        sdf: sdf_paint_for(
                            &rect,
                            icon_font_scale(&rect, symbol_scale),
                            symbol_scale,
                            icon_fill,
                            icon_halo,
                            feature.icon_halo_width,
                        ),
                        opacity: icon_opacity,
                    });
                }
                // Built whether or not the icon draws. An
                // invisible symbol still occupies its placement
                // slot in mapbox-gl-js -- `placement.ts` reads
                // only `symbol-z-offset` off `paint` and has no
                // idea the icon is transparent -- so the labels
                // it evicts there are evicted here too, and the
                // ones that survive land where the reference
                // puts them. Dropping the box instead would
                // silently RE-PLACE every label around a hidden
                // POI, which is a bigger visible difference
                // than the hidden icon ever was.
                icon_box = Some(icon_collision_box(
                    physical_width,
                    physical_height,
                    rotation_rad,
                    screen_x * ctx.scale_factor,
                    screen_y * ctx.scale_factor,
                ));
                icon_native = Some((physical_width, physical_height, rotation_rad));
            }
        }
    }

    // The label half. `None` for an icon-only feature -- which
    // then goes on to be placed on its icon box alone, rather
    // than (as an earlier `continue` here did) skipping
    // placement entirely and drawing unconditionally.
    let mut text_draw: Option<LabelDraw> = None;
    // The label as per-glyph rotated quads, for LINE placement only. See
    // `GlyphRunDraw`, and the gate further down for why `feature.line` is
    // the test rather than anything read off the style layer.
    let mut glyph_draw: Option<GlyphRunDraw> = None;
    let mut text_box: Option<CollisionBox> = None;
    // The union of the per-glyph boxes, for a label that reached the LINE
    // path and was placed along its road. `None` for everything else, and
    // it is the difference between the two that decides which box goes into
    // `text_box` below. See `glyph_run_collision_box`.
    let mut glyph_run_box: Option<CollisionBox> = None;
    // Set when this feature went down the LINE path and
    // `place_glyphs_along` REFUSED it -- the run did not fit inside its
    // polyline at this anchor, or the line turned harder than
    // `MAX_GLYPH_ANGLE_RAD` under it. Such a label draws nothing, so it
    // must reserve nothing; see where `text_box` is decided.
    let mut line_label_refused = false;
    // A point label's box, `(left, top, width, height)` physical pixels:
    // what `icon-text-fit` wraps the icon around.
    let mut point_text_rect: Option<(f32, f32, f32, f32)> = None;
    if let Some(text) = &feature.text {
        // `layout.text-rotation-alignment`, read ONCE per feature and used
        // for two things that must not disagree: the gate below, and the
        // angle every glyph quad is drawn at.
        //
        // `Map` means the text FOLLOWS the road, which is the per-glyph
        // curved path. `Viewport` means the road's direction is not an
        // input at all -- one horizontal block at the anchor, which is the
        // same path point placement takes. MapLibre draws the line exactly
        // there: `placeGlyphsAlongLine` is reached only for text that
        // follows the line.
        //
        // Getting this wrong is not subtle, and it shipped: distributing
        // the glyphs along the polyline while holding each one level put
        // the three letters of a `BKE` motorway shield in a vertical stack
        // down a north-south expressway. Every `highway-name-*` layer in
        // both vendored styles is `map`, so nothing but the SHIELDS --
        // Liberty's `highway-shield-non-us`, `highway-shield-us-interstate`
        // and `road_shield_us`, all `viewport`, all line-placed from z11 --
        // could show it.
        let text_alignment = text_rotation_alignment(ctx, feature.layer_index);
        let follows_line = matches!(text_alignment, crate::style::RotationAlignment::Map);
        let physical_text_size = feature.text_size * symbol_scale;
        // `layout.text-max-width`, out of ems and into the
        // physical pixels shaping works in: the property is a
        // multiple of the FONT SIZE, so the same `9` is 90px on
        // a 10px label and 180px on a 20px one, and the display's
        // scale factor is already inside `physical_text_size`.
        // `None` -- a line-placed label, which does not wrap --
        // stays `None`.
        let wrap_width = feature.text_max_width_em.map(|ems| ems * physical_text_size);
        // Shapes only on a cache miss -- a label that was already on
        // screen last frame costs a hash lookup here instead of a
        // full `Buffer` allocation + shape (see `LabelShapeCache` for
        // the measured difference). The borrow of the returned entry
        // ends on this line so the cache is free to be borrowed again
        // for the next feature.
        // The face this layer's `layout.text-font` asks for,
        // resolved against what this machine has installed.
        // Memoised per stack, so this is a hash lookup for every
        // label after the first of its layer.
        let t_face = crate::timing::Instant::now();
        let face = shape_cache.face_for(font_system, &feature.text_font);
        collect_timing::add(&collect_timing::FACE_NS, t_face);
        let t_shape = crate::timing::Instant::now();
        let shaped_before = shape_cache.shape_count;
        let (measured_width, measured_height, line_glyph_run) = {
            // Basemap symbol layers have no `text-justify` hook
            // yet -- see `get_or_shape`'s own doc comment.
            let shaped = shape_cache.get_or_shape(
                font_system,
                text,
                physical_text_size,
                wrap_width,
                crate::overlay::paint::TextJustify::Center,
                &face,
            );
            // The per-glyph half of the same shaping, collected ONLY for a
            // label that will actually be laid along its road -- `run.glyphs`
            // is data nothing else in this crate reads, and copying it for
            // every point label would be a `Vec` per POI per frame for
            // nothing. Copied out (rather than the buffer borrowed onwards)
            // because rastering into the glyph atlas needs `&mut
            // FontSystem`, which cannot be taken while `shape_cache`'s
            // returned reference is alive.
            //
            // `Some` here IS the gate further down: the two conditions are
            // written once, and a `None` run falls through to the
            // single-block path. See that match.
            let line_glyph_run = if feature.line.is_some() && follows_line {
                Some(line_glyphs(&shaped.buffer))
            } else {
                None
            };
            (shaped.measured_width, shaped.measured_height, line_glyph_run)
        };
        collect_timing::add(&collect_timing::SHAPE_NS, t_shape);
        if shape_cache.shape_count != shaped_before {
            if let Some(sink) = glyphs.as_deref_mut() {
                sink.atlas.charge(t_shape.elapsed());
            }
        }
        let (left, top) = text_area_origin(
            screen_x,
            screen_y,
            ctx.scale_factor,
            LabelAnchoring { offset_em: feature.text_offset, anchor: feature.text_anchor },
            physical_text_size,
            measured_width,
            measured_height,
        );
        let on_data = ctx.on_data_paint(anchors, feature);
        let (halo_width, halo_blur) = text_halo(on_data, feature);
        let halo_width = halo_width * symbol_scale;
        // `paint.text-opacity`, on the label's own colour AND on
        // its halo's -- Mapbox hands one `opacity` attribute to
        // the glyph shader and it multiplies both
        // (`program_configuration.ts` maps `text-opacity` to the
        // `opacity` attribute, which `symbol_sdf.fragment.glsl`
        // applies after the halo/fill choice, not before it).
        //
        // A zero one emits no `TextArea` at all, saving the
        // whole glyph upload for a label nobody can see -- but
        // NOT the collision box built just below, for the reason
        // the icon box carries: mapbox-gl-js's placement pass
        // never reads the property, so an invisible label
        // competes for its slot there and has to here.
        if feature.text_opacity > 0.0 {
            // Text that FOLLOWS ITS ROAD takes the per-glyph path and the
            // `TextArea` path is skipped entirely for it -- the two are
            // mutually exclusive per symbol (see `GlyphRunDraw`), because
            // drawing both would put a horizontal copy of the street name
            // across its own rotated one.
            //
            // Two conditions, both necessary, and `line_glyph_run` above
            // carries them:
            //
            // * `feature.line`, which the bake sets on line placement and
            //   only on line placement (see `style::SymbolFeature::line`),
            //   so POINT-placed text cannot reach this branch however the
            //   layer is otherwise written; and
            // * `text-rotation-alignment: map`, because `viewport` asks for
            //   the road's direction to be ignored ENTIRELY. Such a label
            //   falls through to the `_` arm -- the same single horizontal
            //   block a point label draws, at the line anchor -- which is
            //   the only thing that puts `BKE` inside its shield instead of
            //   spelling it down the road.
            match (&feature.line, line_glyph_run) {
                (Some(line), Some(run)) => {
                    let t_line = crate::timing::Instant::now();
                    let placed = line_glyph_quads(
                        ctx,
                        anchors,
                        feature,
                        line,
                        &run,
                        physical_text_size,
                        halo_width,
                        tile_units_per_physical_px(ctx.camera.zoom, tile_z, ctx.scale_factor),
                        text_alignment,
                        shade,
                        font_system,
                        glyphs,
                    );
                    collect_timing::add(&collect_timing::LINE_NS, t_line);
                    if let Some(placed) = placed {
                        glyph_run_box = placed.bounds;
                        glyph_draw = Some(GlyphRunDraw { text: text.clone(), quads: placed.quads });
                    } else {
                        line_label_refused = true;
                    }
                }
                _ => {
                    // `shade.ink` gives up brightness the night no longer
                    // needs and `shade.halo` gives up weight it is no
                    // longer doing anything with; `text-opacity` multiplies
                    // the alpha of both, exactly as it did before. See
                    // `render::SymbolShade` -- and note that on a DAY-side
                    // label both accessors return their argument, so this
                    // is the expression it was.
                    //
                    // A DARK glyph on the night side is the exception: it
                    // turns into its light-on-dark night pair instead
                    // (`render::dark_label_colors`), because no dimming
                    // makes black text readable on a dark ground.
                    //
                    // A label over data keeps the on-data paint (see
                    // `line_glyph_quads`): no night pair for it.
                    let (text_color, text_halo_color) = text_colors(on_data, feature);
                    let (text_fill, text_halo) = match on_data {
                        Some(_) => {
                            let o = |c: crate::style::Rgba| crate::style::Rgba { a: c.a * feature.text_opacity, ..c };
                            (o(text_color), o(text_halo_color))
                        }
                        None => shade.label_colors(text_color, text_halo_color, feature.text_opacity),
                    };
                    text_draw = Some((
                        LabelShapeCache::key(
                            text,
                            physical_text_size,
                            wrap_width,
                            crate::overlay::paint::TextJustify::Center,
                            &face,
                        ),
                        left,
                        top,
                        text_fill,
                        halo_width,
                        text_halo,
                        halo_blur * symbol_scale,
                    ));
                }
            }
        }
        // The box is built from the very numbers the glyphs will be
        // drawn with -- `text_area_origin`'s origin and the size
        // shaping measured for the whole block, however many lines
        // it turned out to have -- so it is what the label covers,
        // not an estimate.
        //
        // Unless the label went down a road, in which case the glyphs are
        // NOT at that origin and are not in a horizontal row: the union of
        // where they actually landed replaces it
        // (`glyph_run_collision_box`).
        //
        // And a line label `place_glyphs_along` REFUSED reserves NOTHING.
        // It is the same defect as the box above, one step earlier: the run
        // does not fit inside its polyline at this anchor, so no glyph will
        // be drawn anywhere -- and a feature that draws nothing must not
        // take a collision slot off one that does. It used to contribute
        // the straight-run box, which is up to 24 invisible labels
        // suppressing visible neighbours at z14 on the Japan corpus.
        //
        // `None` here and no icon means `symbol_box` returns `None` just
        // below and the whole feature is `Skipped`, which is the right
        // outcome and is exactly what a data-driven `icon-image` naming a
        // missing sprite already gets. A line-placed feature that DOES have
        // an icon keeps it: `symbol_box(None, icon_box, _)` still places
        // the icon on its own box, so a shield whose name would not fit
        // still draws.
        //
        // The straight-run box remains the fallback for the two cases that
        // really do draw a horizontal block at that origin: a point label,
        // and a line label held at `text-opacity: 0` -- which never enters
        // the branch above at all, so it is not known to be refused and
        // keeps the conservative box.
        text_box = match glyph_run_box {
            Some(union) => Some(union),
            None if line_label_refused => None,
            None => {
                point_text_rect = Some((left, top, measured_width, measured_height));
                Some(text_collision_box(left, top, measured_width, measured_height))
            }
        };
    }

    // `icon-text-fit`: the icon wraps the label, Mapbox's `fitIconToText`
    // (MapLibre `src/symbol/shaping.ts`, BSD-3-Clause): on a fitted axis
    // it spans the text's box plus `icon-text-fit-padding`
    // (`[top, right, bottom, left]`, layout pixels scaled with the
    // symbol); on the other it keeps its own size, centred on the text.
    // `icon-anchor` is not respected under a fit, as in Mapbox. The whole
    // sprite is stretched: Mapbox Streets' fitted sprite (`intersection`,
    // the Tokyo intersection-name plate) is a plain rectangle with no
    // `stretchX`/`content`, which is exactly what Mapbox stretches whole.
    // A sprite WITH stretch bands is stretched whole too here -- its
    // corners distort, but it wraps its label, where at native size it
    // covered part of the name and none of its ends.
    let fit = ctx.style_layers.get(feature.layer_index).and_then(|layer| match layer {
        style::StyleLayer::Symbol { icon_text_fit, .. } if icon_text_fit.fit != crate::overlay::paint::TextFit::None => {
            Some(*icon_text_fit)
        }
        _ => None,
    });
    if let (Some(fit), Some((native_w, native_h, rotation_rad)), Some((left, top, width, height))) =
        (fit, icon_native, point_text_rect)
    {
        let [pad_top, pad_right, pad_bottom, pad_left] = fit.padding.map(|p| p * symbol_scale);
        let (x0, x1) = if fit.fit.fits_width() {
            (left - pad_left, left + width + pad_right)
        } else {
            let mid = left + width / 2.0;
            (mid - native_w / 2.0, mid + native_w / 2.0)
        };
        let (y0, y1) = if fit.fit.fits_height() {
            (top - pad_top, top + height + pad_bottom)
        } else {
            let mid = top + height / 2.0;
            (mid - native_h / 2.0, mid + native_h / 2.0)
        };
        if let Some(icon) = icon_draw.as_mut() {
            icon.screen_x = x0;
            icon.screen_y = y0;
            icon.width = x1 - x0;
            icon.height = y1 - y0;
        }
        icon_box = Some(icon_collision_box(x1 - x0, y1 - y0, rotation_rad, (x0 + x1) / 2.0, (y0 + y1) / 2.0));
    }

    // A feature that resolved neither a drawable label nor an
    // atlas-backed icon (e.g. a data-driven `icon-image` naming
    // a sprite this sheet doesn't carry, on a layer with no
    // text) must not consume a placement slot.
    let Some(bounds) =
        crate::collision::symbol_box(text_box, icon_box, ctx.collision_padding)
    else {
        return SymbolOutcome::Skipped;
    };
    // See `symbol_bypasses_collision` for why a text-bearing
    // feature is excluded even when `bypass_collision` is set.
    //
    // `feature.text`, NOT `text_draw`: a label held at
    // `text-opacity: 0` still HAS text, and letting it slip into
    // the bypass path would hand it the very promise the flag
    // never made (see that function) -- and would do so only
    // while it happened to be invisible, which is the worst
    // possible time for placement to change shape.
    if symbol_bypasses_collision(feature.bypass_collision, feature.text.is_some()) {
        return match icon_draw {
            Some(icon) => SymbolOutcome::Bypassed(DrawnSymbol {
                order: (feature.layer_index, feature.sort_key),
                icons: vec![icon],
                icon_box,
                text: None,
                glyphs: None,
                text_box: None,
                text_clip: None,
                // Compiled style layers carry no such property.
                text_clip_behind_icons: true,
            }),
            None => SymbolOutcome::Skipped,
        };
    }
    // Offered for cross-layer duplicate suppression only as a point label:
    // a line-placed one repeats along its road by design.
    let dedupe_anchor = (text_draw.is_some() && glyph_draw.is_none()).then_some((screen_x, screen_y));
    SymbolOutcome::Candidate(SymbolCandidate {
        dedupe_anchor,
        text: text_draw,
        glyphs: glyph_draw,
        icon_box,
        text_box,
        // Basemap icons are never stretched: `icon-text-fit` is
        // an overlay property so far, and no vendored layer
        // this crate compiles uses it.
        icons: icon_draw.into_iter().collect(),
        // Compiled style layers carry no such property.
        text_clip_behind_icons: true,
        placement: PlacementCandidate {
            // The style-layer index UNSHIFTED. The overlay sits
            // at `usize::MAX` rather than at 0, so no index needs
            // to be reserved at the bottom and every basemap
            // symbol keeps exactly the rank its style layer gave
            // it -- the same value the bypass path above puts in
            // `DrawnSymbol::order`.
            layer_index: feature.layer_index,
            sort_key: feature.sort_key,
            bounds,
        },
    })
}


/// The side, in texels, [`grow_text_atlas`] grows glyphon's mask atlas to.
///
/// glyphon starts its atlas at 256x256 and, when a new glyph does not fit,
/// EVICTS the least recently used glyph not drawn this frame before it
/// considers growing. The atlas therefore stays the size of one frame's
/// working set: on the census zoom sweep it sat at 512 for most of the run
/// and evicted 5,028 glyphs, every one of which was rasterised again on
/// the way back (7,765 rasters for ~2,700 distinct glyph images). 2048 is
/// 4 MB of `R8Unorm` and holds every glyph a z3-z12 sweep shows.
pub(crate) const TEXT_ATLAS_SIDE: u32 = 2048;

/// Grows glyphon's mask atlas to `side` (clamped to the device limit) once,
/// so that its LRU eviction -- and the re-raster of every evicted glyph when
/// it comes back -- stops being the steady state. See [`TEXT_ATLAS_SIDE`].
///
/// glyphon has no API to size an atlas, and grows only when a glyph does
/// not fit and every cached glyph was drawn this frame. So this prepares one
/// throwaway text area whose glyphs, at large sizes, cover more than a
/// quarter of `side`^2 -- the only way past the eviction-first rule. The
/// throwaway glyphs are ordinary LRU entries afterwards: the first to go if
/// the atlas ever fills. The renderer's vertices are overwritten by the next
/// `prepare`; the caller must not `render` before one.
///
/// Costs one-off rasters of a few hundred large glyphs (~10-30 ms), once per
/// atlas, on the first frame that has text to draw.
#[allow(clippy::too_many_arguments)]
pub(crate) fn grow_text_atlas(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    font_system: &mut glyphon::FontSystem,
    atlas: &mut glyphon::TextAtlas,
    viewport: &glyphon::Viewport,
    renderer: &mut glyphon::TextRenderer,
    swash: &mut glyphon::SwashCache,
    side: u32,
) {
    let side = side.min(device.limits().max_texture_dimension_2d);
    const SAMPLE: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789@#%&WM";
    // One buffer per size: the area a glyph covers grows with the square of
    // the size, so a handful of large sizes reach a quarter of `side`^2
    // with few glyphs. Sizes are scaled to `side` so a smaller device
    // limit asks for proportionally less.
    let scale = side as f32 / 2048.0;
    let buffers: Vec<glyphon::Buffer> = [160.0f32, 200.0]
        .iter()
        .map(|size| {
            let size = (size * scale).max(8.0);
            let mut buffer = glyphon::Buffer::new(font_system, glyphon::Metrics::new(size, size * 1.25));
            buffer.set_size(None, None);
            buffer.set_text(SAMPLE, &glyphon::Attrs::new(), glyphon::Shaping::Basic, None);
            buffer.shape_until_scroll(font_system, false);
            buffer
        })
        .collect();
    let bounds = glyphon::TextBounds { left: 0, top: 0, right: 1, bottom: 1 };
    let areas = buffers.iter().map(|buffer| glyphon::TextArea {
        buffer,
        left: 0.0,
        top: 0.0,
        scale: 1.0,
        bounds,
        default_color: glyphon::Color::rgba(0, 0, 0, 0),
        custom_glyphs: &[],
    });
    // A failure (the device limit reached first) leaves the atlas as large
    // as it got, which is all this can do anyway.
    let _ = renderer.prepare(device, queue, font_system, atlas, viewport, areas, swash);
    atlas.trim();
}

/// A fingerprint of everything [`text_areas`] builds from, so a frame whose
/// labels are identical to the last prepared frame's can reuse glyphon's
/// prepared vertices instead of calling `prepare` again (measured at
/// ~1 ms for a dense, still z12 frame of 1,680 areas / 13 k glyphs).
pub(crate) fn text_signature(
    symbol_text: &[(LabelDraw, Option<CollisionBox>)],
    physical_viewport_width: f32,
    physical_viewport_height: f32,
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    physical_viewport_width.to_bits().hash(&mut hasher);
    physical_viewport_height.to_bits().hash(&mut hasher);
    symbol_text.len().hash(&mut hasher);
    for ((key, left, top, color, halo_width, halo_color, halo_blur), clip) in symbol_text {
        key.hash(&mut hasher);
        for value in [
            *left, *top, color.r, color.g, color.b, color.a, *halo_width, halo_color.r, halo_color.g, halo_color.b,
            halo_color.a, *halo_blur,
        ] {
            value.to_bits().hash(&mut hasher);
        }
        match clip {
            Some(clip) => {
                for value in [clip.left, clip.top, clip.right, clip.bottom] {
                    value.to_bits().hash(&mut hasher);
                }
            }
            None => 0u8.hash(&mut hasher),
        }
    }
    hasher.finish()
}

/// Every `glyphon::TextArea` one frame's labels draw, halo copies included.
///
/// # Why this is a function and not four lines at the call site
///
/// Because the halo is a fiction. cosmic-text has no outline primitive, so
/// an outlined label is the SAME shaped buffer drawn several times in the
/// halo colour, offset a whole physical pixel out, before the real-colour
/// draw goes on top ([`halo_offsets`] decides how many and how faded). Get
/// the order or the alphas wrong and every label on the map is subtly
/// wrong, which is not a thing to reimplement per frontend.
///
/// The returned areas borrow `shape_cache` -- pass TWO of the split
/// described on [`LabelShapeCache`]: pass one shaped the buffers under a
/// `&mut` borrow and kept only their keys, and this resolves the keys back
/// under a shared borrow, so `TextRenderer::prepare` can still take `&mut`
/// borrows of everything else.
///
/// A key that is somehow not in the cache is SKIPPED rather than panicked
/// on. It cannot happen -- pass one ensured every key and nothing trims the
/// cache until `end_frame` -- but losing one label beats losing the frame.
pub(crate) fn text_areas<'a>(
    symbol_text: &'a [(LabelDraw, Option<CollisionBox>)],
    shape_cache: &'a LabelShapeCache,
    physical_viewport_width: f32,
    physical_viewport_height: f32,
) -> Vec<glyphon::TextArea<'a>> {
    let bounds = glyphon::TextBounds {
        left: 0,
        top: 0,
        right: physical_viewport_width as i32,
        bottom: physical_viewport_height as i32,
    };
    // Local helper rather than a closure: a closure capturing
    // `bounds` and taking `&'a glyphon::Buffer` (with `'a` tied to
    // `label_shape_cache`, populated above) hits Rust's usual trouble
    // inferring a higher-ranked lifetime for closure arguments; a
    // plain fn sidesteps that.
    fn make_area<'a>(
        buffer: &'a glyphon::Buffer,
        left: f32,
        top: f32,
        color: crate::style::Rgba,
        bounds: glyphon::TextBounds,
    ) -> glyphon::TextArea<'a> {
        glyphon::TextArea {
            buffer,
            left,
            top,
            scale: 1.0,
            bounds,
            default_color: glyphon::Color::rgba(
                (color.r * 255.0) as u8,
                (color.g * 255.0) as u8,
                (color.b * 255.0) as u8,
                (color.a * 255.0) as u8,
            ),
            custom_glyphs: &[],
        }
    }

    // Halo approximation: glyphon/cosmic-text have no native text
    // outline primitive, so each feature with a halo draws its SAME
    // shaped buffer again in the halo color, offset a whole physical
    // pixel out in each cardinal direction -- once per ring, and one
    // ring unless `text-halo-blur` asks for a fade (see
    // `halo_offsets`). Those copies come BEFORE the real-color draw:
    // there is no depth test, so later entries in `text_areas` win
    // on overlap, and the rings themselves must run outermost first
    // for their alphas to composite to the coverage they encode. A
    // feature with no halo contributes just the one real-color area.
    //
    // Pass TWO of the two-pass split described at `symbol_text`: a
    // shared borrow of just the cache field (so `prepare` below can
    // still take `&mut` borrows of the other `gpu` fields), resolving
    // each key back to the buffer pass one shaped.
        symbol_text
        .iter()
        .flat_map(|((key, left, top, color, halo_width, halo_color, halo_blur), clip)| {
            // Cannot actually miss -- pass one ensured every one of
            // these keys and nothing trims the cache until
            // `end_frame` at the bottom of this function -- but
            // silently skipping one label beats panicking a frame.
            let Some(shaped) = shape_cache.get(key) else { return Vec::new() };
            // The label's own clip, where a later billboard covers
            // part of it, and the whole viewport where nothing
            // does. glyphon clips a text area to this rectangle,
            // which is what puts the covered part of a number
            // BEHIND the badge in front of it instead of on top.
            //
            // Grown by the halo width first: the halo copies are
            // drawn a whole pixel out in each direction, and
            // clipping them to the bare text box would shave the
            // outline off the edges of every label on the map.
            let clipped = match clip {
                Some(box_) => {
                    let grown = box_.padded(halo_reach(*halo_width, *halo_blur));
                    glyphon::TextBounds {
                        left: grown.left.floor() as i32,
                        top: grown.top.floor() as i32,
                        right: grown.right.ceil() as i32,
                        bottom: grown.bottom.ceil() as i32,
                    }
                }
                None => bounds,
            };
            let offsets = halo_offsets(*halo_width, *halo_blur);
            let mut areas = Vec::with_capacity(offsets.len() + 1);
            for (dx, dy, alpha) in offsets {
                // `alpha` is the ring's own source alpha -- see
                // `halo_offsets` for why it is not its coverage.
                let faded = crate::style::Rgba { a: halo_color.a * alpha, ..*halo_color };
                areas.push(make_area(&shaped.buffer, left + dx, top + dy, faded, clipped));
            }
            areas.push(make_area(&shaped.buffer, *left, *top, *color, clipped));
            areas
        })
        .collect()
}

/// Walks every visible tile's symbol plan and offers each feature to
/// [`basemap_candidate`], appending to the two lists the collision pass
/// takes.
///
/// The loop is three lines and it is here anyway, because there are three
/// callers -- `map_view::paint`, the browser's `app::draw_labels`, and
/// [`label_census`] -- and a fourth copy of "which plan, which matrix, which
/// bucket" is exactly how two frontends start disagreeing about what a map
/// says. `combined_tile_matrix` is hoisted to the tile, which is the only
/// thing the loop is really for.
///
/// Returns how many features were offered, which is the denominator every
/// other count in a census is read against.
#[allow(clippy::too_many_arguments)]
/// Loads every face a style's symbol layers name, once, so the first label
/// set in each does not pay for it mid-gesture.
///
/// The first shape in a face cosmic-text has not opened yet reads and
/// parses the font file: measured at 11-33 ms for the first label in each
/// of SF Pro Text 400, 500 and 700 on the census zoom sweep
/// (2026-09-26), against ~0.03 ms for every later label. A face a style
/// uses only from z10 (italic water names, bold road shields) paid that in
/// the first frame that reached z10. MapLibre's equivalent is its glyph
/// manager requesting each stack's ranges as the style loads.
///
/// Shapes a short sample into a throwaway buffer: nothing is cached in
/// `shape_cache` but the resolved faces, so what is placed and drawn is
/// unchanged. Returns how many distinct faces it warmed.
pub(crate) fn prewarm_fonts<'a>(
    stacks: impl Iterator<Item = &'a crate::font_fallback::FontStack>,
    font_system: &mut glyphon::FontSystem,
    shape_cache: &mut LabelShapeCache,
) -> usize {
    let mut warmed = HashSet::new();
    for stack in stacks {
        let face = shape_cache.face_for(font_system, stack);
        if !warmed.insert(face.clone()) {
            continue;
        }
        let mut buffer = glyphon::Buffer::new(font_system, glyphon::Metrics::new(16.0, 20.0));
        buffer.set_text("Ag 0", &face.attrs(), glyphon::Shaping::Advanced, None);
        buffer.shape_until_scroll(font_system, false);
    }
    warmed.len()
}

/// Where `collect_candidates`' time goes, summed since the last
/// [`collect_timing::take`]: resolving font stacks, shaping (with the
/// line-glyph walk), and line placement (which includes glyph rastering).
/// Relaxed atomics, so the web build pays two clock reads per label and
/// nothing else; read only by the native `probe` build.
pub(crate) mod collect_timing {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    pub(crate) static FACE_NS: AtomicU64 = AtomicU64::new(0);
    pub(crate) static SHAPE_NS: AtomicU64 = AtomicU64::new(0);
    pub(crate) static LINE_NS: AtomicU64 = AtomicU64::new(0);
    pub(crate) fn add(counter: &AtomicU64, since: crate::timing::Instant) {
        counter.fetch_add(since.elapsed().as_nanos() as u64, Relaxed);
    }
    /// (face, shape, line) nanoseconds since the last call, and resets them.
    #[allow(dead_code)]
    pub(crate) fn take() -> (u64, u64, u64) {
        (FACE_NS.swap(0, Relaxed), SHAPE_NS.swap(0, Relaxed), LINE_NS.swap(0, Relaxed))
    }
}

pub(crate) fn collect_candidates<'a>(
    frame: &SymbolFrame<'_>,
    plans: impl Iterator<Item = (TileId, &'a style::SymbolPlan)>,
    font_system: &mut glyphon::FontSystem,
    shape_cache: &mut LabelShapeCache,
    mut glyphs: Option<&mut GlyphSink<'_>>,
    candidates: &mut Vec<SymbolCandidate>,
    bypassed: &mut Vec<DrawnSymbol>,
) -> usize {
    let mut features = 0;
    for (tile, plan) in plans {
        let anchors = AnchorProjection::new(frame.camera, tile, &frame.globe);
        for feature in &plan.features {
            features += 1;
            // Reborrowed per feature rather than moved: `Option<&mut _>`
            // is not `Copy`, and the sink has to survive the whole walk.
            let sink = glyphs.as_deref_mut();
            match basemap_candidate(frame, &anchors, tile.z, feature, font_system, shape_cache, sink) {
                SymbolOutcome::Candidate(candidate) => candidates.push(candidate),
                SymbolOutcome::Bypassed(symbol) => bypassed.push(symbol),
                SymbolOutcome::Skipped => {}
            }
        }
    }
    features
}

/// What one camera's worth of labels came to: the counts, and the text of
/// every label that survived placement, in draw order.
///
/// Public because [`label_census`] is: this is the shape a
/// native-vs-browser comparison is made on, and the browser's
/// `map_labels_json()` prints the same field names.
#[derive(Debug, Clone, Default)]
pub struct LabelCensus {
    /// `SymbolFeature`s offered, over every plan given.
    pub features: usize,
    /// Of those, the ones that projected on screen with something to draw.
    pub candidates: usize,
    /// Of those, the ones collision placed (icon-only symbols included).
    pub placed: usize,
    /// The text of every placed label, in draw order. Shorter than
    /// `placed` by however many placed symbols were icon-only.
    pub texts: Vec<String>,
    /// Real shaping operations this call performed. It builds its own
    /// cache, so this is "how many distinct (text, size, face) there were",
    /// not a per-frame cost.
    pub shaped: usize,
    /// The SPRITE NAME behind every icon quad placed, in draw order --
    /// empty unless [`label_census`] was given a sprite atlas.
    ///
    /// This is the shield half of the comparison. "The right shield draws"
    /// cannot be settled by counting: a route badge and a blank box are both
    /// one quad, and a `jp-national-route-2` drawn where a
    /// `jp-expressway-2` belongs is one quad too. The name is the only thing
    /// that distinguishes them, and the browser's `map_icons_json()` prints
    /// the same list under the same key.
    pub icons: Vec<String>,
    /// Sprite names the placed features ASKED for that the atlas does not
    /// carry, deduplicated, sorted. Non-empty means a style expression
    /// resolved to a name the sheet is missing -- the one failure mode that
    /// otherwise leaves no trace at all, because `basemap_candidate` skips
    /// such an icon silently and keeps the feature's text.
    pub missing_icons: Vec<String>,
    /// Line-placed labels `symbol_line::place_glyphs_along` REFUSED during
    /// this call: the label was longer than the polyline it was anchored
    /// on, or the line turned harder under it than
    /// `symbol_line::MAX_GLYPH_ANGLE_RAD`.
    ///
    /// Dropping them is what MapLibre does, and it is right (see
    /// `place_glyphs_along`'s doc) -- but a silent drop makes "the street
    /// names stopped appearing" unanswerable, so the refusals are on the
    /// record. Counted as a delta of the process-wide
    /// [`line_labels_refused`] counter across this call.
    pub line_labels_refused: u64,
    /// Where this call's wall time went, by phase, in milliseconds.
    ///
    /// Exists because a whole-pass figure is not actionable. `label_census`
    /// was reported at 16.9 ms for 2,063 offered symbols at z16, and a
    /// collision-index restructure against that number bought exactly nothing
    /// -- because at most 550 boxes ever enter the index, and the cost was
    /// never there. A single total invites that mistake; a split forecloses
    /// it.
    pub phases: CensusPhases,
}

/// `label_census`'s wall time, split by phase. Milliseconds.
///
/// The two that matter are [`Self::collect_ms`] (project, shape, build
/// candidates) and [`Self::place_ms`] (the collision grid). They are reported
/// separately and not summed into a ratio here: a ratio hides which one grew.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CensusPhases {
    /// Packing the sprite sheet into one atlas image. Zero when
    /// `label_census` was given no sprites, which is how the stress suite
    /// calls it.
    pub atlas_ms: f64,
    /// Walking every plan's features for icon names the atlas lacks. An
    /// instrument-only phase -- a live frame never asks this -- so it is
    /// reported to be SUBTRACTED from any comparison with a real frame, not
    /// to be optimised.
    pub missing_icons_ms: f64,
    /// `collect_candidates`: projecting every anchor, shaping every distinct
    /// label, and building the candidate list. Glyph shaping lives here.
    pub collect_ms: f64,
    /// Of [`Self::collect_ms`], the part spent in real glyphon shaping (cache
    /// MISSES only).
    ///
    /// **The live frame does not pay this every frame.** `MapView` holds one
    /// `LabelShapeCache` on its `GpuState` for the process's life, while
    /// `label_census` builds a fresh one per call -- so this is the cost of a
    /// first frame showing entirely new labels, and a steady-state pan pays
    /// only `collect_ms - shape_ms` plus `place_ms`. Subtract it before
    /// comparing any census figure to a frame budget.
    pub shape_ms: f64,
    /// `place_visible_symbols`: the greedy collision pass over the candidate
    /// boxes. This is the phase a collision-index change moves.
    pub place_ms: f64,
    /// Draw-order sort plus `clip_labels_behind_later_icons`.
    pub order_ms: f64,
    /// Naming each placed icon quad back to its sprite. Instrument-only,
    /// like `missing_icons_ms`.
    pub naming_ms: f64,
}

/// Runs the whole label pass -- project, shape, place, order -- and reports
/// what it placed, with no GPU and no window anywhere in the path.
///
/// # Why this exists
///
/// Because "the browser build draws the same labels as the native one" is a
/// claim, and a claim needs a number. Shaping needs a `FontSystem` and
/// nothing else; collision needs no device at all. So the same code the
/// frame loop runs can be run headless, against the same tiles, and its
/// answer diffed against the browser's `map_labels_json()`.
///
/// It builds a fresh `LabelShapeCache` and a fresh `CollisionIndex` per
/// call, which is what makes it a measurement rather than a frame: a live
/// map retains both, and `shaped` here is therefore the DISTINCT label
/// count rather than a per-frame shaping cost.
///
/// # `sprites` is not optional decoration
///
/// Pass `None` and `icon_atlas_size` is `(0, 0)`: no icon draws, and -- the
/// part that is easy to miss -- an icon-only feature then contributes NO
/// COLLISION BOX, so labels survive that a sheet-bearing build would have
/// evicted. A census taken without sprites is therefore not comparable with
/// one taken with them. Pass `Some` and this packs the atlas exactly the way
/// both frontends do (`pack_icon_atlas`, over the atlas's own sorted names),
/// so the rects, and hence the boxes, and hence the survivors, are the same.
///
/// It builds its own atlas rather than taking one already packed because
/// there is no GPU here to have packed one: `pack_icon_atlas` is pure
/// arithmetic over `image::RgbaImage`s and needs no device.
///
/// # One place this does NOT mirror the frame path: glyph rastering
///
/// Line-placed labels are PLACED here exactly as a frame places them -- the
/// advance conversion, the does-it-fit test, the max-angle rejection and
/// the keep-upright flip all run, so `texts` and `line_labels_refused` are
/// a drawing caller's numbers. What does not run is the RASTERING: this
/// passes `None` for `line_glyph_quads`' [`GlyphSink`], so no glyph reaches
/// `render::GlyphAtlas` and no quad is built.
///
/// That is deliberate, and it is the one asymmetry in this function worth
/// knowing about before trusting a timing from it. Two reasons:
///
/// * The census reports no number rastering moves. It counts what was
///   offered, what projected, what placed and what the labels say, and
///   every one of those is settled before a glyph is touched.
/// * It would make the timings a fiction. This function builds everything
///   fresh per call, so a warm loop of ten calls would run the swash raster
///   and `sprite::sdf_from_mask`'s distance transform
///   ten times over, where a live `MapView` holds ONE `GlyphAtlas` for the
///   process's life and pays each glyph once, ever. Measured on the Japan
///   corpus with rastering left in: `symbol_collision.z14.census_p50` went
///   from 4.8 ms to 12.1 ms and z16's from 17.0 ms to 35.5 ms -- two graded
///   metrics turned into hard failures by work the frame this models does
///   not repeat.
///
/// The consequence to hold onto: a glyph-atlas change cannot be graded from
/// here. Its cost is a first-sighting cost in the frame, and it has to be
/// measured in the frame.
pub fn label_census<'a>(
    camera: &Camera,
    style_layers: &[style::StyleLayer],
    plans: impl Iterator<Item = (TileId, &'a style::SymbolPlan)>,
    scale_factor: f32,
    font_system: &mut glyphon::FontSystem,
    default_family: &str,
    sprites: Option<&crate::sprite::SpriteAtlas>,
) -> LabelCensus {
    // Sorted, the same way `app::load_sprites` sorts: `pack_icon_atlas`
    // lays sprites out in the order it is given, and a `HashMap` iteration
    // order would put the same sheet at different origins on two runs --
    // harmless for drawing, fatal for a name-by-origin lookup diffed
    // between two processes.
    let t_atlas = crate::timing::Instant::now();
    let (packed_atlas, icon_rects) = match sprites {
        Some(sprites) => {
            let mut names: Vec<String> = sprites.names().cloned().collect();
            names.sort();
            let (image, rects) = crate::render::pack_icon_atlas(sprites, &names);
            (Some(image), rects)
        }
        None => (None, HashMap::new()),
    };
    let atlas_ms = t_atlas.elapsed().as_secs_f64() * 1000.0;
    let icon_atlas_size = packed_atlas.as_ref().map_or((0, 0), |image| image.dimensions());
    // `rects` inverted, so a placed `IconInstance` -- which carries a UV
    // rect and no name -- can be named. Same trick, same reason, as
    // `app::Icons::by_origin`: the drawing path must not carry a `String`
    // for an instrument's benefit.
    let by_origin: HashMap<(u32, u32), &str> =
        icon_rects.iter().map(|(name, rect)| ((rect.x, rect.y), name.as_str())).collect();
    let frame = SymbolFrame {
        occlusion: None,
        label_paint: None,
        camera,
        style_layers,
        scale_factor,
        size_scale: scale_factor,
        collision_padding: SYMBOL_COLLISION_PADDING_LOGICAL_PX * scale_factor,
        size_blend_t: camera.zoom.fract() as f32,
        icon_rects: &icon_rects,
        icon_atlas_size,
        // The census answers "what does this camera show", so it has to see
        // the same globe a frame at that camera would: below z6 the anchors
        // are on the sphere and the far hemisphere is not labelled. One
        // camera-level build per census, which is the same arrangement
        // `MapView::cover_progress` has and for the same reason -- a census
        // is asked from outside a frame and has no frame value to borrow.
        globe: crate::render::globe::GlobeAnchors::new(&crate::render::globe::globe_frame(camera)),
        // UNLIT, and that is not a gap. A census answers WHICH labels this
        // camera places and how many; the terminator changes a label's
        // colour and nothing else -- not its box, not its placement, not
        // its survival. Lighting it here would mean reading a clock inside
        // an instrument, which is the one thing a census must not do if two
        // runs a minute apart are to be comparable.
        light: crate::render::SymbolLight::UNLIT,
        // OFF. The fade's end comes from the cover's cut, and a census is
        // handed plans rather than a cover (nor the source's max zoom that
        // decides the cover), so it has no cut to fade at. Below 50 degrees
        // -- every graded census -- the frame's fade is off too; above it a
        // census counts the labels past the fade that a frame drops.
        fog: crate::render::HorizonFog::NONE,
    };
    let mut shape_cache = LabelShapeCache::with_default_family(default_family);
    let mut candidates = Vec::new();
    let mut bypassed = Vec::new();
    // Collected rather than streamed, because the list is walked TWICE: once
    // through the pass itself, and once for `missing_icons`. The second walk
    // is the only reason -- a live frame streams its plans and never asks
    // this question.
    let plans: Vec<(TileId, &style::SymbolPlan)> = plans.collect();
    let t_missing = crate::timing::Instant::now();
    let missing_icons = {
        let mut missing: Vec<String> = plans
            .iter()
            .flat_map(|(_, plan)| plan.features.iter())
            .filter_map(|feature| feature.icon.as_deref())
            .filter(|name| !icon_rects.contains_key(*name))
            .map(str::to_string)
            .collect();
        missing.sort();
        missing.dedup();
        missing
    };
    let missing_icons_ms = t_missing.elapsed().as_secs_f64() * 1000.0;
    // `line_labels_refused` is process-wide and monotonic, so the census
    // reports its own DELTA across the walk below. Single-threaded by
    // construction here; a concurrent census elsewhere in the process would
    // see the two attributed to whichever finished last, which is a
    // limitation of an instrument rather than of the counter.
    let refused_before = line_labels_refused();
    let t_collect = crate::timing::Instant::now();
    let features = collect_candidates(
        &frame,
        plans.into_iter(),
        font_system,
        &mut shape_cache,
        // NO glyph atlas, deliberately -- see `GlyphSink` and
        // `line_glyph_quads`. Line-placed labels are still placed here,
        // still refused here on the same two tests, and still counted in
        // `texts` and `line_labels_refused`; what they do not do is
        // RASTER. Two reasons, and the second is the one that matters.
        //
        // The census reports no number that rasterisation moves: it counts
        // what was offered, what projected, what placed, and what the
        // labels say, and every one of those is settled before a glyph is
        // touched. And it would make the timings a fiction: the census
        // builds everything fresh per call, so a warm loop of ten calls
        // would run the swash raster and the distance
        // transform ten times over, where a live `MapView` holds ONE
        // `GlyphAtlas` for the process's life and pays each glyph once,
        // ever. Measured on the Japan corpus: rastering inside the census
        // moved `symbol_collision.z14.census_p50` from 4.8 ms to 12.1 ms
        // and z16's from 17.0 ms to 35.5 ms, for work the frame it is
        // meant to model does not repeat.
        None,
        &mut candidates,
        &mut bypassed,
    );
    let collect_ms = t_collect.elapsed().as_secs_f64() * 1000.0;
    let line_labels_refused = line_labels_refused() - refused_before;
    let candidate_count = candidates.len();
    let mut index = CollisionIndex::new(symbol_collision_bounds(
        camera.viewport_width_px,
        camera.viewport_height_px,
        scale_factor,
    ));
    // The index build is inside `place_ms` deliberately: allocating the grid
    // is part of what a collision-index change alters, and attributing it
    // elsewhere would let a restructure that merely moved the allocation look
    // like a win.
    let t_place = crate::timing::Instant::now();
    let mut drawn = place_visible_symbols(candidates, &mut index);
    let place_ms = t_place.elapsed().as_secs_f64() * 1000.0;
    let t_order = crate::timing::Instant::now();
    drawn.extend(bypassed);
    sort_symbols_for_drawing(&mut drawn);
    clip_labels_behind_later_icons(&mut drawn);
    let order_ms = t_order.elapsed().as_secs_f64() * 1000.0;
    let t_naming = crate::timing::Instant::now();
    let (atlas_w, atlas_h) = (icon_atlas_size.0 as f32, icon_atlas_size.1 as f32);
    let icons: Vec<String> = drawn
        .iter()
        .flat_map(|symbol| symbol.icons.iter())
        .map(|instance| {
            let origin = (
                (instance.uv_min.0 * atlas_w).round() as u32,
                (instance.uv_min.1 * atlas_h).round() as u32,
            );
            by_origin
                .get(&origin)
                .map_or_else(|| format!("?@{},{}", origin.0, origin.1), |name| (*name).to_string())
        })
        .collect();
    let naming_ms = t_naming.elapsed().as_secs_f64() * 1000.0;
    LabelCensus {
        features,
        candidates: candidate_count,
        placed: drawn.len(),
        // BOTH label paths. A line-placed street name travels as per-glyph
        // quads rather than as a `LabelDraw` (see `GlyphRunDraw`), and
        // reading only `symbol.text` would have every street name silently
        // vanish from the one instrument that compares this build's labels
        // with the browser's.
        texts: drawn
            .into_iter()
            .filter_map(|symbol| match (symbol.text, symbol.glyphs) {
                (Some((key, ..)), _) => Some(key.0),
                (None, Some(run)) => Some(run.text),
                (None, None) => None,
            })
            .collect(),
        shaped: shape_cache.shape_count,
        icons,
        missing_icons,
        line_labels_refused,
        phases: CensusPhases {
            atlas_ms,
            missing_icons_ms,
            collect_ms,
            shape_ms: shape_cache.shape_ms,
            place_ms,
            order_ms,
            naming_ms,
        },
    }
}

/// The globe half of the symbol pass.
///
/// # Why these are here and not in `map_view.rs` with the rest
///
/// This file's module comment says the symbol tests live in `map_view.rs`'s
/// `mod tests`, and that is still true of every test about shaping,
/// anchoring, boxes and placement. These cannot join them: they need a
/// `render::globe::GlobeFrame`, and
/// `render::globe::paint_calls_globe_frame_exactly_once_and_before_its_tile_loops`
/// counts the textual `globe_frame(` call sites in `map_view.rs` -- test
/// module included -- and refuses a third. That count is the guard against
/// MapLibre's per-tile transform rebuild reappearing, so the tests move
/// rather than the guard loosening.
#[cfg(test)]
mod globe_tests {
    use super::*;
    use crate::render::globe::{globe_frame, GlobeAnchors};

    /// A globe camera looking straight down at `lon`/`lat`, at a zoom inside
    /// the globe band. Pitch and bearing are zero so [`pinhole_screen`] can
    /// stay arithmetic rather than becoming a second matrix chain.
    fn globe_camera(lon: f64, lat: f64, zoom: f64, width: f32, height: f32) -> Camera {
        Camera {
            center_lon: lon,
            center_lat: lat,
            zoom,
            pitch: 0.0,
            bearing: 0.0,
            viewport_width_px: width,
            viewport_height_px: height,
            fov_y: Camera::DEFAULT_FOV_Y,
        }
    }

    /// The longitude and latitude, in radians, of one tile-local MVT anchor,
    /// by the DIRECT inverse-Gudermannian formula.
    ///
    /// Deliberately not `globe::mercator_y_to_sin_cos_lat`: that function is
    /// the half-angle substitution the shader uses, and a test that predicted
    /// a pixel through it would be checking the projection against itself.
    /// The two agree to 1e-12 by
    /// `the_half_angle_substitution_matches_the_direct_latitude_formula`, so
    /// using the direct form here costs nothing and keeps the prediction
    /// independent.
    fn anchor_lon_lat_radians(tile: TileId, anchor: geo_types::Point<i32>) -> (f64, f64) {
        let tiles = f64::from(1u32 << tile.z);
        let mercator_x = (f64::from(tile.x) + f64::from(anchor.x()) / f64::from(MVT_EXTENT)) / tiles;
        let mercator_y = (f64::from(tile.y) + f64::from(anchor.y()) / f64::from(MVT_EXTENT)) / tiles;
        let lon = mercator_x * 2.0 * std::f64::consts::PI - std::f64::consts::PI;
        let lat = 2.0 * (std::f64::consts::PI - mercator_y * 2.0 * std::f64::consts::PI).exp().atan()
            - std::f64::consts::FRAC_PI_2;
        (lon, lat)
    }

    /// Where a lon/lat on the sphere lands on screen, **from pinhole geometry
    /// alone** -- no projection matrix, no near plane, no far plane.
    ///
    /// The same discipline
    /// `a_named_star_lands_where_the_camera_puts_it_and_is_dark_20_px_away`
    /// (Task 8) used, and for the same reason: a prediction made with the
    /// renderer's own matrix cannot tell a correct projection from a CPU and
    /// a GPU agreeing about one shared mistake. Everything here is written
    /// out:
    ///
    /// - the sphere point is `(sin lon cos lat, sin lat, cos lon cos lat)`,
    ///   which is where `globe::mercator_to_sphere` puts lon 0 on `+z` and
    ///   north on `+y`;
    /// - the camera's own lon/lat spins the planet by `Rx(lat0) * Ry(-lon0)`
    ///   and the eye sits `radius + distance` back along `+z`;
    /// - `radius` is `WORLD_SIZE / 2PI / cos(lat0)` and `distance` is
    ///   `height / (2 tan(fov/2) 2^zoom)`, both re-derived rather than read
    ///   off `globe_radius_world`/`Camera::distance`;
    /// - and the pinhole itself is `k = (height/2) / tan(fov/2)`, with
    ///   `col = centre_x + k * v.x / |v.z|` and
    ///   `row = centre_y - k * v.y / |v.z|`. The PLUS on x is the whole
    ///   sign question this projection turns on: `shader.wgsl` applies its
    ///   horizontal flip to the Mercator half only (`vs_main`, `clip.x =
    ///   -clip.x`, before the blend), because that flip exists to undo
    ///   `Camera::eye_target_up`'s basis and `globe_frame` does not use that
    ///   basis -- the sphere's +x IS screen right. The minus on y is the
    ///   ordinary y-down window convention, which both halves share.
    ///
    /// Valid only at pitch 0 and bearing 0, which is why [`globe_camera`]
    /// fixes both.
    fn pinhole_screen(camera: &Camera, lon: f64, lat: f64) -> (f64, f64) {
        let fov_half_tan = camera.tan_half_fov();
        let radius = f64::from(crate::render::WORLD_SIZE)
            / (2.0 * std::f64::consts::PI)
            / camera.center_lat.to_radians().cos();
        let distance =
            f64::from(camera.viewport_height_px) / (2.0 * fov_half_tan * 2f64.powf(camera.zoom));
        let (px, py, pz) = (lon.sin() * lat.cos(), lat.sin(), lon.cos() * lat.cos());
        // Ry(-lon0), then Rx(lat0) -- the planet turning under a fixed eye.
        let (lon0, lat0) = (camera.center_lon.to_radians(), camera.center_lat.to_radians());
        let (rx, ry, rz) = (px * lon0.cos() - pz * lon0.sin(), py, px * lon0.sin() + pz * lon0.cos());
        let (rx, ry, rz) = (rx, ry * lat0.cos() - rz * lat0.sin(), ry * lat0.sin() + rz * lat0.cos());
        // Scale to the sphere's world radius, then back the eye off by the
        // centre distance plus one radius.
        let (vx, vy, vz) = (rx * radius, ry * radius, rz * radius - (radius + distance));
        let k = f64::from(camera.viewport_height_px) / 2.0 / fov_half_tan;
        (
            f64::from(camera.viewport_width_px) / 2.0 + k * vx / -vz,
            f64::from(camera.viewport_height_px) / 2.0 - k * vy / -vz,
        )
    }

    /// A point-placed label with nothing surprising on it.
    pub(super) fn point_label(anchor: geo_types::Point<i32>, text: &str) -> style::SymbolFeature {
        style::SymbolFeature {
            anchor,
            text: Some(text.to_string()),
            icon: None,
            text_size: 14.0,
            text_offset: (0.0, 0.0),
            text_anchor: TextAnchor::Center,
            text_max_width_em: None,
            icon_size_from: 1.0,
            icon_size_to: 1.0,
            text_color: style::Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 },
            text_halo_color: style::Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 },
            text_halo_width: 0.0,
            text_halo_blur: 0.0,
            text_font: crate::font_fallback::default_font_stack(),
            text_opacity: 1.0,
            icon_opacity: 1.0,
            icon_color: style::Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 },
            icon_halo_color: style::Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 },
            icon_halo_width: 0.0,
            layer_index: 0,
            sort_key: 0.0,
            forward_ref: None,
            line: None,
            line_centre_distance: 0.0,
            icon_rotate_deg: 0.0,
            bypass_collision: false,
        }
    }

    /// Runs the real pass -- `collect_candidates` then
    /// `place_visible_symbols` -- over one tile's features and hands back
    /// what survived, in draw order.
    fn place(
        camera: &Camera,
        globe: GlobeAnchors,
        tile: TileId,
        features: Vec<style::SymbolFeature>,
    ) -> Vec<DrawnSymbol> {
        place_with_fog(camera, globe, tile, features, crate::render::HorizonFog::NONE)
    }

    /// [`place`] under a horizon fade.
    fn place_with_fog(
        camera: &Camera,
        globe: GlobeAnchors,
        tile: TileId,
        features: Vec<style::SymbolFeature>,
        fog: crate::render::HorizonFog,
    ) -> Vec<DrawnSymbol> {
        let candidates = candidates_for(camera, globe, tile, features, fog);
        let mut index = CollisionIndex::new(symbol_collision_bounds(
            camera.viewport_width_px,
            camera.viewport_height_px,
            1.0,
        ));
        place_visible_symbols(candidates, &mut index)
    }

    /// The real `collect_candidates` over one tile's features, at scale
    /// factor 1.0 -- the half of [`place_with_fog`] before the index, so a
    /// test can seed the index itself.
    pub(super) fn candidates_for(
        camera: &Camera,
        globe: GlobeAnchors,
        tile: TileId,
        features: Vec<style::SymbolFeature>,
        fog: crate::render::HorizonFog,
    ) -> Vec<SymbolCandidate> {
        let mut font_system = crate::font_fallback::new_font_system();
        let mut shape_cache = LabelShapeCache::new();
        let icon_rects: HashMap<String, IconRect> = HashMap::new();
        let frame = SymbolFrame {
            occlusion: None,
            label_paint: None,
            camera,
            style_layers: &[],
            scale_factor: 1.0,
            size_scale: 1.0,
            collision_padding: SYMBOL_COLLISION_PADDING_LOGICAL_PX,
            size_blend_t: 0.0,
            icon_rects: &icon_rects,
            icon_atlas_size: (0, 0),
            globe,
            light: crate::render::SymbolLight::UNLIT,
            fog,
        };
        let plan = style::SymbolPlan { features };
        let mut candidates = Vec::new();
        let mut bypassed = Vec::new();
        collect_candidates(
            &frame,
            std::iter::once((tile, &plan)),
            &mut font_system,
            &mut shape_cache,
            None,
            &mut candidates,
            &mut bypassed,
        );
        candidates
    }

    /// The camera the placement tests share: the whole planet on screen at
    /// `transition == 1.0` exactly (`transition_for_zoom` is a hard 1.0 at
    /// and below `PROJECTION_BLEND_ZOOM_MIN = 11`), at lon/lat 0, pitch 0,
    /// bearing 0.
    ///
    /// **2048x512, and the aspect ratio is load-bearing.** The camera's
    /// distance -- and so the horizon, at `acos(R / (R + d))` = 53.46
    /// degrees here -- depends on the viewport HEIGHT alone
    /// (`Camera::distance`), while how much longitude the FLAT map shows
    /// depends on its width. A square viewport at this zoom shows about 45
    /// degrees of longitude either side of centre, which is INSIDE the
    /// horizon, so no point could be both occluded on the globe and on
    /// screen when flat -- and
    /// `a_feature_on_the_far_hemisphere_is_not_placed_although_its_pixel_is_on_screen`
    /// needs exactly such a point or it cannot tell occlusion from the cull.
    /// Four to one gives 90 degrees either side and 37 degrees of room
    /// between the two thresholds.
    pub(super) fn planet_camera() -> Camera {
        globe_camera(0.0, 0.0, 3.0, 2048.0, 512.0)
    }

    /// The flat path must not move by one ULP, and the globe path must move
    /// a very long way -- both halves, in one test, because either alone
    /// grades nothing.
    ///
    /// Task 8 recorded the failure mode this is written against: its own
    /// flat-identity test proved nothing twice, once because its fixture was
    /// the same colour as the thing it should not have drawn. The analogue
    /// here is an `AnchorProjection` whose globe arm is never taken -- every
    /// flat assertion would still pass, and so would every test about
    /// placement, because a flat projection is a perfectly ordinary pixel.
    /// So the contrast is asserted in the same test as the identity.
    #[test]
    fn a_flat_frame_projects_an_anchor_bit_identically_and_a_globe_frame_does_not() {
        let tile = TileId { z: 3, x: 5, y: 4 };
        let anchor = geo_types::Point::new(2730, 1000);

        // Above the band: `transition_for_zoom(13) == 0.0`, so `GlobeAnchors`
        // built from a real frame IS `FLAT`, and both must equal the matrix
        // multiply this crate did before the globe existed.
        //
        // z13, not z8. The projection blend runs over z11-z12
        // (`render::globe::PROJECTION_BLEND_ZOOM_MIN`, MapLibre's own band),
        // because that is the only place the sphere and Mercator agree
        // closely enough for a mix of them to be harmless; z8 is inside the
        // pure-globe half now. The space effects still fade out by z6 --
        // that is `space_transition_for_zoom`, a different number.
        let flat_camera = globe_camera(0.0, 0.0, 13.0, 2048.0, 512.0);
        let expected = {
            let tile_to_clip =
                crate::render::combined_tile_matrix(&flat_camera, tile, MVT_EXTENT as f32);
            let local =
                glam::Vec4::new(anchor.x() as f32 * 1000.0, anchor.y() as f32 * 1000.0, 0.0, 1.0);
            crate::render::project_clip_to_screen(&flat_camera, tile_to_clip * local)
        };
        let from_frame = GlobeAnchors::new(&globe_frame(&flat_camera));
        assert!(
            !from_frame.is_globe(),
            "z13 is above PROJECTION_BLEND_ZOOM_MAX, so the globe must be off"
        );
        for (name, anchors) in [
            ("GlobeAnchors::FLAT", AnchorProjection::flat(&flat_camera, tile)),
            ("a real z8 frame", AnchorProjection::new(&flat_camera, tile, &from_frame)),
        ] {
            assert_eq!(
                anchors.screen(&flat_camera, anchor),
                expected,
                "{name} must project the flat anchor to the very same pixel, to the bit"
            );
            assert!(!anchors.occluded(anchor), "{name} has no planet to hide behind");
        }

        // And inside the band the globe arm really is taken. Without this
        // half, deleting `AnchorProjection`'s globe branch outright would
        // leave the assertions above green.
        let camera = planet_camera();
        let globe = GlobeAnchors::new(&globe_frame(&camera));
        assert!(globe.is_globe(), "z3 is far below the blend band, so transition is exactly 1.0");
        let curved = AnchorProjection::new(&camera, tile, &globe)
            .screen(&camera, anchor)
            .expect("the anchor is in front of the near plane");
        let flat = AnchorProjection::flat(&camera, tile)
            .screen(&camera, anchor)
            .expect("the anchor is in front of the near plane");
        let moved = ((curved.0 - flat.0).powi(2) + (curved.1 - flat.1).powi(2)).sqrt();
        eprintln!("globe anchor {curved:?} vs flat anchor {flat:?}: {moved:.1} px apart");
        assert!(
            moved > 100.0,
            "at full transition the sphere must put this anchor somewhere else entirely; it \
             moved only {moved:.1} px, which is what a globe arm that never runs looks like"
        );
    }

    /// A label lands where the planet's own geometry says its feature is.
    ///
    /// The expected pixel comes from [`pinhole_screen`] -- no matrix, no clip
    /// volume -- and is compared against what the REAL pass produced:
    /// `collect_candidates` -> `place_visible_symbols` -> the placed label's
    /// own collision box. A centred label's box is centred on its anchor
    /// (`text_area_origin` with `TextAnchor::Center` subtracts half of each
    /// measured extent), so the box's centre IS the projected anchor, up to
    /// the whole-pixel `round()` that function ends with.
    ///
    /// **Tolerance: 1.0 px**, of which 0.5 px is that rounding and the rest
    /// is the f32 narrowing at `project_clip_to_screen`. The measured error
    /// is printed, and it is far inside.
    #[test]
    fn a_label_lands_where_pinhole_geometry_puts_its_feature_on_the_sphere() {
        let camera = planet_camera();
        let globe = GlobeAnchors::new(&globe_frame(&camera));
        // lon 22.5, lat -21.94 -- a point on the near hemisphere, 31.1
        // degrees of arc from the camera's axis (the horizon is at 53.5),
        // off BOTH axes and in a different quadrant from the camera centre,
        // so that a swapped or sign-flipped component could not land on the
        // right pixel by symmetry.
        let tile = TileId { z: 2, x: 2, y: 2 };
        let anchor = geo_types::Point::new(1024, 1024);
        let (lon, lat) = anchor_lon_lat_radians(tile, anchor);

        let placed = place(&camera, globe, tile, vec![point_label(anchor, "Somewhere")]);
        assert_eq!(placed.len(), 1, "the near-side label must be placed");
        let text_box = placed[0].text_box.expect("a text label has a text box");
        let centre = (
            f64::from(text_box.left + text_box.right) / 2.0,
            f64::from(text_box.top + text_box.bottom) / 2.0,
        );

        let expected = pinhole_screen(&camera, lon, lat);
        let error = ((centre.0 - expected.0).powi(2) + (centre.1 - expected.1).powi(2)).sqrt();
        eprintln!(
            "label at lon {:.4} lat {:.4}: box centre ({:.3}, {:.3}), pinhole ({:.3}, {:.3}), \
             error {error:.3} px",
            lon.to_degrees(),
            lat.to_degrees(),
            centre.0,
            centre.1,
            expected.0,
            expected.1
        );
        assert!(
            error < 1.0,
            "the label is {error:.3} px from where pinhole geometry puts its feature; the \
             tolerance is 1.0 px, of which 0.5 is text_area_origin's own round()"
        );
    }

    /// The far hemisphere is not labelled -- and the test can tell that from
    /// "it was off screen anyway", which is the whole difficulty.
    ///
    /// The feature chosen is 75 degrees of arc from the camera's axis, past
    /// the 64.9-degree horizon this camera has, and its globe projection
    /// lands **well inside the viewport**: over the middle of the drawn
    /// planet, which is exactly how Sydney came to be drawn over the
    /// Atlantic. So the assertion is not "it is gone" but "it is gone even
    /// though its pixel is on screen", and the flat arm of the same camera
    /// places it to prove the pass is otherwise willing.
    #[test]
    fn a_feature_on_the_far_hemisphere_is_not_placed_although_its_pixel_is_on_screen() {
        let camera = planet_camera();
        let globe = GlobeAnchors::new(&globe_frame(&camera));
        let tile = TileId { z: 3, x: 5, y: 4 };
        let anchor = geo_types::Point::new(2730, 0);
        let (lon, lat) = anchor_lon_lat_radians(tile, anchor);
        assert!(lat.abs() < 1e-12, "the fixture is on the equator: lat {lat}");

        let anchors = AnchorProjection::new(&camera, tile, &globe);
        let on_screen = anchors
            .screen(&camera, anchor)
            .expect("a far-side point is still in front of the near plane");
        eprintln!(
            "far-side feature at lon {:.2}: globe pixel {on_screen:?} in a {}x{} viewport",
            lon.to_degrees(),
            camera.viewport_width_px,
            camera.viewport_height_px
        );
        assert!(
            on_screen.0 > 0.0
                && on_screen.1 > 0.0
                && on_screen.0 < camera.viewport_width_px
                && on_screen.1 < camera.viewport_height_px,
            "the fixture must project INSIDE the viewport, or the cull would be doing the work \
             this test attributes to occlusion; it projected to {on_screen:?}"
        );
        assert!(anchors.occluded(anchor), "75 degrees of arc is past this camera's horizon");

        assert!(
            place(&camera, globe, tile, vec![point_label(anchor, "Faraway")]).is_empty(),
            "a feature behind the planet must not be placed at all"
        );
        // Transition 0.0 at the SAME camera, tile and anchor: the only
        // variable is the globeness, and the label comes back.
        // `GlobeAnchors::FLAT` is exactly what every frame above z6 carries.
        let flat = place(&camera, GlobeAnchors::FLAT, tile, vec![point_label(anchor, "Faraway")]);
        assert_eq!(
            flat.len(),
            1,
            "at transition 0.0 the same feature must still be placed -- occlusion must not have \
             leaked into the flat path"
        );
    }


    /// A street name whose glyphs would run off the edge of the world draws
    /// nothing -- and one that stays on the near side still draws.
    ///
    /// Both runs are on the equator of tile `2/2/2`, laid west to east, at a
    /// camera whose horizon sits at longitude 53.46. The control ends at
    /// longitude 48.7; the crossing run is CENTRED on the horizon, so about
    /// half its glyphs are behind the planet.
    ///
    /// The whole run is refused rather than sliced, which is where this task
    /// deliberately stops short of `pathSlicedToLongestUnoccluded`
    /// (`symbol/projection.ts:978-995`) -- see `line_glyph_quads`, which
    /// carries the argument.
    #[test]
    fn a_line_placed_label_crossing_the_horizon_draws_nothing_and_one_beside_it_draws() {
        let camera = planet_camera();
        let globe = GlobeAnchors::new(&globe_frame(&camera));
        let tile = TileId { z: 2, x: 2, y: 2 };
        // Tile-local x for a longitude, on tile 2/2/*: the tile spans
        // longitude 0..90 in 4096 units.
        let at_lon = |lon: f64| (lon / 90.0 * 4096.0).round() as i32;
        let horizon_x = at_lon(53.46);
        let road = |from: i32, to: i32, centre: f64| {
            let mut feature = point_label(geo_types::Point::new(from + (to - from) / 2, 0), "Equator Road");
            feature.line = Some(std::sync::Arc::new(geo_types::LineString(vec![
                geo_types::Coord { x: from, y: 0 },
                geo_types::Coord { x: to, y: 0 },
            ])));
            feature.line_centre_distance = centre;
            feature
        };

        // The control: longitude 0..48.7, wholly inside the horizon. Long
        // enough ON SCREEN for the run: glyphs are walked along the
        // projected line, and toward the limb the globe foreshortens a
        // road hard -- longitude 30..48.7 is 54 px here, shorter than the
        // 89 px label, which the tile walk used to squeeze onto it.
        let near = road(at_lon(0.0), at_lon(48.7), f64::from(at_lon(48.7) - at_lon(0.0)) / 2.0);
        let placed = place(&camera, globe, tile, vec![near]);
        assert_eq!(
            placed.len(),
            1,
            "a line-placed label wholly on the near side must still be placed on the globe"
        );
        assert!(
            placed[0].glyphs.is_some(),
            "and it must have gone down the per-glyph line path, not the horizontal-block one"
        );

        // The crossing: longitude 30..75, with the run centred on the
        // horizon at longitude 53.46.
        let crossing = road(
            at_lon(30.0),
            at_lon(75.0),
            f64::from(horizon_x - at_lon(30.0)),
        );
        let _refused_lock = crate::symbol_pass::LINE_LABELS_REFUSED_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let refused_before = line_labels_refused();
        assert!(
            place(&camera, globe, tile, vec![crossing]).is_empty(),
            "a run straddling the horizon must draw nothing rather than spelling half a word \
             or scattering glyphs into space"
        );
        assert_eq!(
            line_labels_refused() - refused_before,
            1,
            "and the refusal must be counted like every other one"
        );

        // The same crossing run at transition 0.0 is placed -- so the
        // refusal is the horizon and not the geometry.
        let crossing = road(
            at_lon(30.0),
            at_lon(75.0),
            f64::from(horizon_x - at_lon(30.0)),
        );
        assert_eq!(
            place(&camera, GlobeAnchors::FLAT, tile, vec![crossing]).len(),
            1,
            "with the globe off the very same run must be placed -- occlusion must not have \
             leaked into the flat path"
        );
    }

    /// A globeness change re-places; it must not re-SHAPE.
    ///
    /// The plan's third divergence rests on this: MapLibre forces a full
    /// re-placement on any globeness change (`style.ts:1843-1852`), which for
    /// them means re-shaping too, while `LabelShapeCache` here is keyed on
    /// `(text, size, wrap, justification, face)` and a transition touches
    /// none of them. Measured rather than assumed, as the plan demands -- and
    /// the isolation is deliberate: the SAME camera, the SAME features, the
    /// SAME cache, with only the `GlobeAnchors` swapped. Changing the zoom
    /// instead would change `text_size` as well and the measurement would not
    /// be about the transition at all.
    #[test]
    fn a_globe_transition_shapes_no_glyphs_a_second_time() {
        let camera = planet_camera();
        let globe = GlobeAnchors::new(&globe_frame(&camera));
        // Anchors spread over about 10 degrees of the near hemisphere, so
        // every one of them is on screen under BOTH projections -- a fixture
        // that projected off screen would be culled before it could be
        // shaped, and the measurement would be of nothing.
        let tile = TileId { z: 2, x: 2, y: 2 };
        let features: Vec<style::SymbolFeature> = (0..24)
            .map(|i| {
                point_label(
                    geo_types::Point::new(900 + i * 20, 400 + i * 20),
                    &format!("Label {i}"),
                )
            })
            .collect();
        let plan = style::SymbolPlan { features };

        let mut font_system = crate::font_fallback::new_font_system();
        let mut shape_cache = LabelShapeCache::new();
        let icon_rects: HashMap<String, IconRect> = HashMap::new();
        let mut walk = |globe: GlobeAnchors| {
            let frame = SymbolFrame {
                occlusion: None,
                label_paint: None,
                camera: &camera,
                style_layers: &[],
                scale_factor: 1.0,
                size_scale: 1.0,
                collision_padding: SYMBOL_COLLISION_PADDING_LOGICAL_PX,
                size_blend_t: 0.0,
                icon_rects: &icon_rects,
                icon_atlas_size: (0, 0),
                globe,
                light: crate::render::SymbolLight::UNLIT,
                fog: crate::render::HorizonFog::NONE,
            };
            let (shaped_before, ms_before) = (shape_cache.shape_count, shape_cache.shape_ms);
            let mut candidates = Vec::new();
            let mut bypassed = Vec::new();
            let t = crate::timing::Instant::now();
            collect_candidates(
                &frame,
                std::iter::once((tile, &plan)),
                &mut font_system,
                &mut shape_cache,
                None,
                &mut candidates,
                &mut bypassed,
            );
            let collect_ms = t.elapsed().as_secs_f64() * 1000.0;
            (
                candidates.len(),
                shape_cache.shape_count - shaped_before,
                shape_cache.shape_ms - ms_before,
                collect_ms,
            )
        };

        let (flat_candidates, flat_shaped, flat_shape_ms, flat_ms) = walk(GlobeAnchors::FLAT);
        let (globe_candidates, globe_shaped, globe_shape_ms, globe_ms) = walk(globe);
        let (again_candidates, again_shaped, again_shape_ms, again_ms) = walk(GlobeAnchors::FLAT);
        eprintln!(
            "transition 0 -> 1 -> 0 over {} features, one shape cache:\n  \
             flat   {flat_candidates:>3} candidates, {flat_shaped:>2} shaped, \
             {flat_shape_ms:.3} ms shaping, {flat_ms:.3} ms collect\n  \
             globe  {globe_candidates:>3} candidates, {globe_shaped:>2} shaped, \
             {globe_shape_ms:.3} ms shaping, {globe_ms:.3} ms collect\n  \
             flat   {again_candidates:>3} candidates, {again_shaped:>2} shaped, \
             {again_shape_ms:.3} ms shaping, {again_ms:.3} ms collect",
            plan.features.len()
        );
        assert!(
            flat_shaped > 0,
            "the first walk must actually shape something, or this proves nothing"
        );
        assert_eq!(
            globe_shaped, 0,
            "crossing into the globe re-shaped {globe_shaped} labels; the cache is keyed on \
             (text, size, wrap, justification, face) and a transition changes none of them, so \
             something is invalidating it"
        );
        assert_eq!(globe_shape_ms, 0.0, "no shaping means no shaping time");
        assert_eq!(again_shaped, 0, "crossing back out re-shaped {again_shaped} labels");
    }

    /// **THE NIGHT-SIDE LABEL FIX, GRADED ON PIXELS.**
    ///
    /// The defect: `shader.wgsl` shades the ground by the sun and nothing
    /// shaded the text over it, so at the default night brightness the
    /// night hemisphere ran at a quarter while every label on it stayed
    /// full white. The fix is `render::SymbolLight`, applied in
    /// `basemap_candidate`.
    ///
    /// **Both halves are asserted here, in one test, because either alone
    /// grades nothing.** A global dimming would satisfy "the night label is
    /// darker" perfectly; what rules it out is the DAY label in the same
    /// frame, drawn from the same feature, through the same pass, coming
    /// back byte-identical. That is the same discipline
    /// `a_flat_frame_projects_an_anchor_bit_identically_and_a_globe_frame_does_not`
    /// holds one file over.
    ///
    /// Nothing here is a stand-in: the labels go through
    /// `collect_candidates` → `place_visible_symbols` → `text_areas`, which
    /// is `MapView::paint`'s own chain, and they are drawn by glyphon into
    /// an `Rgba8Unorm` target with the same `ColorMode::Web` atlas
    /// production builds. The only thing written here is the fixture and
    /// the sampling.
    #[test]
    fn a_label_on_the_night_side_is_dimmed_to_its_floor_and_one_on_the_day_side_is_not() {
        // Two labels, the same text, at the same LONGITUDE, one either
        // side of the terminator -- the same tile, the same
        // `AnchorProjection` and the same shaping cache serve both, so any
        // difference between them is the light and can be nothing else.
        //
        // A z0 tile, so one tile holds the whole world and the two anchors
        // are two points in it.
        let tile = TileId { z: 0, x: 0, y: 0 };
        let camera = globe_camera(0.0, 0.0, 1.5, 1024.0, 1024.0);
        let globe_frame = globe_frame(&camera);

        // **The terminator straight across the middle of the disc**, which
        // is what `examples/globe_capture.rs --lit` uses and for the same
        // reason: it is clock-free, so this test says the same thing at
        // every hour, and it puts a wide margin of lit and unlit surface on
        // screen at once. Tilt 90 is the sun on the screen's horizon and
        // bearing 0 puts it toward the top, so the northern half of the
        // visible cap is day and the southern half is night.
        //
        // A MAP-anchored sun would not do: the visible cap of a globe
        // camera is about 53 degrees of arc from its centre, while the
        // terminator sits 90 degrees from the subsolar point, so a sun
        // anchored under the camera leaves no night on screen at all --
        // which is how the first version of this fixture placed one label
        // instead of two.
        let sun = crate::render::atmosphere::Sun::Viewport { tilt_deg: 90.0, bearing_deg: 0.0 }
            .direction_sphere(&globe_frame.globe_view.inverse());
        let night = crate::render::SymbolLight::new(
            sun,
            crate::render::Atmosphere::TERMINATOR_NIGHT_BRIGHTNESS,
            globe_frame.space_transition,
        );

        // A latitude's Mercator y, in this z0 tile's MVT units.
        let at_lat = |degrees: f64| {
            let phi = degrees.to_radians();
            let mercator_y = 0.5
                - (std::f64::consts::FRAC_PI_4 + phi / 2.0).tan().ln()
                    / (2.0 * std::f64::consts::PI);
            geo_types::Point::new(
                (MVT_EXTENT / 2) as i32,
                (mercator_y * f64::from(MVT_EXTENT)) as i32,
            )
        };
        // 30 degrees either side of the terminator: ten band-widths clear
        // of it (the band is 3 degrees), and well inside the visible cap.
        let day_anchor = at_lat(30.0);
        let night_anchor = at_lat(-30.0);

        // The fixture's own claim about which side each anchor is on,
        // checked against the light before any pixel is read -- otherwise a
        // fixture that put both labels in daylight would "prove" the day
        // side is unchanged and prove nothing else.
        let anchors = AnchorProjection::new(&camera, tile, &GlobeAnchors::new(&globe_frame));
        assert_eq!(
            night.shade(anchors.sphere(day_anchor)),
            crate::render::SymbolShade::UNSHADED,
            "the fixture's DAY anchor is not in daylight"
        );
        let white = style::Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };
        let floor = night.shade(anchors.sphere(night_anchor)).ink(white).r;
        assert!(
            (floor - 0.70).abs() < 1e-6,
            "the fixture's NIGHT anchor is not at the full night floor: white ink there \
             comes out at {floor}, not the 0.70 `render::SymbolLight` documents"
        );

        let render = |light: crate::render::SymbolLight| {
            pollster::block_on(render_two_labels(
                &camera,
                &globe_frame,
                tile,
                day_anchor,
                night_anchor,
                light,
                point_label,
                wgpu::Color::BLACK,
            ))
        };
        let lit = render(night);
        let unlit = render(crate::render::SymbolLight::UNLIT);

        // A frame with no ink in it would pass every ratio below, so the
        // ink is counted before it is measured -- `prove a run painted`.
        assert!(
            unlit.day.ink_pixels > 50 && unlit.night.ink_pixels > 50,
            "the fixture drew no glyphs: day {} night {} ink pixels",
            unlit.day.ink_pixels,
            unlit.night.ink_pixels
        );

        // --- THE DAY SIDE IS UNTOUCHED, BYTE FOR BYTE ---
        // Counted rather than `assert_eq!`ed on the vectors: a failure here
        // should report HOW MANY bytes moved, not print twelve thousand of
        // them.
        let day_moved =
            lit.day.bytes.iter().zip(&unlit.day.bytes).filter(|(a, b)| a != b).count();
        assert_eq!(
            day_moved,
            0,
            "{day_moved} of {} bytes of the DAY label changed -- a night-side fix that \
             moves the day side is a global dimming wearing a terminator",
            lit.day.bytes.len()
        );

        // --- THE NIGHT SIDE IS DIMMER, AND BY THE DESIGNED AMOUNT ---
        let night_moved =
            lit.night.bytes.iter().zip(&unlit.night.bytes).filter(|(a, b)| a != b).count();
        assert!(
            night_moved > 0,
            "the NIGHT label did not move at all -- the fix is not reaching the pass"
        );
        assert!(
            unlit.night.ink >= 250.0,
            "the fixture's unlit night label should be near-white ink; it is {}",
            unlit.night.ink
        );
        let ratio = lit.night.ink / unlit.night.ink;
        assert!(
            (0.66..=0.74).contains(&ratio),
            "night-side ink came out at {ratio:.3} of the day value. The band is 0.66 to \
             0.74 around the 0.70 `render::LABEL_NIGHT_DIM` is calibrated to -- see \
             `render::SymbolLight` for why 0.70 and not the ground's own 0.25 (which \
             would be 1.05:1 against the night land, i.e. invisible)."
        );
        // And in absolute terms: #B3B3B3, give or take the rasteriser.
        assert!(
            (172.0..=185.0).contains(&lit.night.ink),
            "night-side ink drew at {}, not the ~179 (#B3B3B3) the docs quote",
            lit.night.ink
        );
    }

    /// Mapbox Streets' `state-label`: black glyph, white halo 1 px wide,
    /// `text-opacity` 0.5 -- the grey-on-grey night label that was reported.
    fn streets_state_label(anchor: geo_types::Point<i32>, text: &str) -> style::SymbolFeature {
        style::SymbolFeature {
            text_color: style::Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 },
            text_halo_color: style::Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 },
            text_halo_width: 1.0,
            text_opacity: 0.5,
            ..point_label(anchor, text)
        }
    }

    /// **A dark-on-light label on the night side reads at 4.5:1 or better,
    /// and the same label on the day side is byte-identical to an unlit
    /// one.** Drawn by glyphon through the real pass onto the reported
    /// frame's night land (`#43423E`), and graded on the pixels.
    #[test]
    fn a_dark_on_light_label_on_the_night_side_keeps_readable_contrast() {
        let tile = TileId { z: 0, x: 0, y: 0 };
        let camera = globe_camera(0.0, 0.0, 1.5, 1024.0, 1024.0);
        let globe_frame = globe_frame(&camera);
        let sun = crate::render::atmosphere::Sun::Viewport { tilt_deg: 90.0, bearing_deg: 0.0 }
            .direction_sphere(&globe_frame.globe_view.inverse());
        let night = crate::render::SymbolLight::new(
            sun,
            crate::render::Atmosphere::TERMINATOR_NIGHT_BRIGHTNESS,
            globe_frame.space_transition,
        );
        let at_lat = |degrees: f64| {
            let phi = degrees.to_radians();
            let mercator_y = 0.5
                - (std::f64::consts::FRAC_PI_4 + phi / 2.0).tan().ln()
                    / (2.0 * std::f64::consts::PI);
            geo_types::Point::new(
                (MVT_EXTENT / 2) as i32,
                (mercator_y * f64::from(MVT_EXTENT)) as i32,
            )
        };
        let (day_anchor, night_anchor) = (at_lat(30.0), at_lat(-30.0));
        let land = wgpu::Color {
            r: f64::from(0x43) / 255.0,
            g: f64::from(0x42) / 255.0,
            b: f64::from(0x3E) / 255.0,
            a: 1.0,
        };
        let render = |light: crate::render::SymbolLight| {
            pollster::block_on(render_two_labels(
                &camera,
                &globe_frame,
                tile,
                day_anchor,
                night_anchor,
                light,
                streets_state_label,
                land,
            ))
        };
        let lit = render(night);
        let unlit = render(crate::render::SymbolLight::UNLIT);

        let day_moved =
            lit.day.bytes.iter().zip(&unlit.day.bytes).filter(|(a, b)| a != b).count();
        assert_eq!(day_moved, 0, "{day_moved} bytes of the DAY label changed");
        assert!(
            unlit.night.contrast > 2.0,
            "the fixture drew no label to grade: unlit contrast {:.2}",
            unlit.night.contrast
        );
        assert!(
            lit.night.contrast >= 4.5,
            "the night-side state label reads at {:.2}:1 against its halo; it needs 4.5:1 \
             (unlit, the same label on the same ground reads {:.2}:1)",
            lit.night.contrast,
            unlit.night.contrast
        );
    }

    /// What [`sample_label`] measured out of one label's box.
    struct LabelInk {
        /// The mean of the brightest 32 pixels -- the glyph's own ink,
        /// rather than the antialiased skirt or the ground around it.
        ink: f32,
        /// How many pixels in the box are brighter than the ground, i.e.
        /// how much glyph there is. A zero here invalidates `ink`.
        ink_pixels: usize,
        /// Every byte of the box, for the day side's byte-identity claim.
        bytes: Vec<u8>,
        /// WCAG contrast between the box's darkest and brightest pixels --
        /// the glyph against its halo. See [`sample_label`].
        contrast: f32,
    }

    struct TwoLabels {
        day: LabelInk,
        night: LabelInk,
    }

    /// Renders the two fixture labels into an offscreen `Rgba8Unorm` target
    /// through the real pass, and hands back a box of pixels around each.
    ///
    /// The clear is `clear`: BLACK for the ink test, so the glyph is the
    /// only bright thing in its box and "the brightest 32 pixels" is the
    /// ink by construction; the night land for the contrast test. No tiles
    /// are drawn: this test is about the label's own colour, and a ground
    /// under it would only add a second variable to subtract.
    #[allow(clippy::too_many_arguments)] // a test fixture: two anchors, a light, a label, a ground
    async fn render_two_labels(
        camera: &Camera,
        globe_frame: &crate::render::globe::GlobeFrame,
        tile: TileId,
        day_anchor: geo_types::Point<i32>,
        night_anchor: geo_types::Point<i32>,
        light: crate::render::SymbolLight,
        label: fn(geo_types::Point<i32>, &str) -> style::SymbolFeature,
        clear: wgpu::Color,
    ) -> TwoLabels {
        let (width, height) =
            (camera.viewport_width_px as u32, camera.viewport_height_px as u32);
        let ctx = crate::render::GpuContext::new_headless().await.expect("headless device");
        let format = wgpu::TextureFormat::Rgba8Unorm;

        let mut font_system = crate::font_fallback::new_font_system();
        let mut shape_cache = LabelShapeCache::new();
        let icon_rects: HashMap<String, IconRect> = HashMap::new();
        let frame = SymbolFrame {
            occlusion: None,
            label_paint: None,
            camera,
            style_layers: &[],
            scale_factor: 1.0,
            size_scale: 1.0,
            collision_padding: SYMBOL_COLLISION_PADDING_LOGICAL_PX,
            size_blend_t: 0.0,
            icon_rects: &icon_rects,
            icon_atlas_size: (0, 0),
            globe: GlobeAnchors::new(globe_frame),
            light,
            fog: crate::render::HorizonFog::NONE,
        };
        // The same text both times: a difference between the two boxes
        // cannot then be a difference in shaping, hinting or coverage.
        let plan = style::SymbolPlan {
            features: vec![label(day_anchor, "NOON"), label(night_anchor, "NOON")],
        };
        let mut candidates = Vec::new();
        let mut bypassed = Vec::new();
        collect_candidates(
            &frame,
            std::iter::once((tile, &plan)),
            &mut font_system,
            &mut shape_cache,
            None,
            &mut candidates,
            &mut bypassed,
        );
        let mut index = CollisionIndex::new(symbol_collision_bounds(
            camera.viewport_width_px,
            camera.viewport_height_px,
            1.0,
        ));
        let mut drawn = place_visible_symbols(candidates, &mut index);
        drawn.extend(bypassed);
        sort_symbols_for_drawing(&mut drawn);
        let symbol_text: Vec<(LabelDraw, Option<CollisionBox>)> = drawn
            .into_iter()
            .filter_map(|symbol| symbol.text.map(|text| (text, symbol.text_clip)))
            .collect();
        assert_eq!(
            symbol_text.len(),
            2,
            "both fixture labels must be placed; {} were",
            symbol_text.len()
        );
        // Which box is which: the anchors differ in longitude, so they
        // differ in screen x. Read off the placed labels rather than
        // predicted, so a change to `text_area_origin` cannot silently move
        // the sampling window off the glyphs.
        let mut origins: Vec<(f32, f32)> =
            symbol_text.iter().map(|((_, left, top, ..), _)| (*left, *top)).collect();
        origins.sort_by(|a, b| a.1.total_cmp(&b.1));
        // The sun is toward the top of the screen, so the northern label is
        // the day one and it has the SMALLER y.
        let (day_origin, night_origin) = (origins[0], origins[1]);

        let glyphon_cache = glyphon::Cache::new(&ctx.device);
        let mut viewport = glyphon::Viewport::new(&ctx.device, &glyphon_cache);
        let mut atlas = glyphon::TextAtlas::with_color_mode(
            &ctx.device,
            &ctx.queue,
            &glyphon_cache,
            format,
            glyphon::ColorMode::Web,
        );
        let mut renderer = glyphon::TextRenderer::new(
            &mut atlas,
            &ctx.device,
            wgpu::MultisampleState::default(),
            None,
        );
        let mut swash = glyphon::SwashCache::new();
        viewport.update(&ctx.queue, glyphon::Resolution { width, height });
        let areas = text_areas(&symbol_text, &shape_cache, width as f32, height as f32);
        renderer
            .prepare(
                &ctx.device,
                &ctx.queue,
                &mut font_system,
                &mut atlas,
                &viewport,
                areas,
                &mut swash,
            )
            .expect("glyphon prepare");

        let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("night-label-test"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder =
            ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("night-label-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            renderer.render(&atlas, &viewport, &mut pass).expect("glyphon render");
        }

        let bytes_per_row = (width * 4).next_multiple_of(256);
        // Through `TrackedBuffer`, not `create_buffer`: every allocation in
        // this crate is counted, and
        // `every_buffer_creation_in_the_crate_goes_through_the_tracker`
        // greps for the ones that are not.
        let readback = crate::render::TrackedBuffer::new(&ctx.device, &wgpu::BufferDescriptor {
            label: Some("night-label-readback"),
            size: u64::from(bytes_per_row) * u64::from(height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        },
        );
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        );
        ctx.queue.submit(Some(encoder.finish()));
        let slice = readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        ctx.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
        rx.recv().expect("readback").expect("readback");
        let padded = slice.get_mapped_range().expect("mapped");
        let row_bytes = (width * 4) as usize;
        let mut rgba = Vec::with_capacity(row_bytes * height as usize);
        for row in 0..height as usize {
            let start = row * bytes_per_row as usize;
            rgba.extend_from_slice(&padded[start..start + row_bytes]);
        }
        drop(padded);
        readback.unmap();

        TwoLabels {
            day: sample_label(&rgba, width, height, day_origin),
            night: sample_label(&rgba, width, height, night_origin),
        }
    }

    /// The pixels of one label, from its placed origin.
    ///
    /// A generous box -- 96 x 32 from the origin -- because the origin is
    /// the text AREA's top-left and the glyphs sit inside it at whatever
    /// the shaped ascent puts them.
    fn sample_label(rgba: &[u8], width: u32, height: u32, origin: (f32, f32)) -> LabelInk {
        let (left, top) = (origin.0.max(0.0) as u32, origin.1.max(0.0) as u32);
        let (right, bottom) = ((left + 96).min(width), (top + 32).min(height));
        let mut bytes = Vec::new();
        let mut luminance = Vec::new();
        for y in top..bottom {
            for x in left..right {
                let i = ((y * width + x) * 4) as usize;
                bytes.extend_from_slice(&rgba[i..i + 4]);
                luminance.push(
                    (f32::from(rgba[i]) + f32::from(rgba[i + 1]) + f32::from(rgba[i + 2])) / 3.0,
                );
            }
        }
        luminance.sort_by(f32::total_cmp);
        let ink_pixels = luminance.iter().filter(|value| **value > 8.0).count();
        let brightest: f32 = luminance.iter().rev().take(32).sum();

        // WCAG relative luminance of every pixel, and the ratio between the
        // mean of the 32 darkest and the 32 brightest: the glyph core and
        // its halo ring, whichever way round the label is drawn. Symmetric
        // on purpose, so the same number grades dark-on-light and
        // light-on-dark.
        let decode = |c: u8| {
            let c = f32::from(c) / 255.0;
            if c <= 0.040_45 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
        };
        let mut relative: Vec<f32> = bytes
            .as_chunks::<4>().0.iter()
            .map(|p| 0.2126 * decode(p[0]) + 0.7152 * decode(p[1]) + 0.0722 * decode(p[2]))
            .collect();
        relative.sort_by(f32::total_cmp);
        let darkest = relative.iter().take(32).sum::<f32>() / 32.0;
        let lightest = relative.iter().rev().take(32).sum::<f32>() / 32.0;
        let contrast = (lightest + 0.05) / (darkest + 0.05);
        LabelInk { ink: brightest / 32.0, ink_pixels, bytes, contrast }
    }

    // ---- Perspective-scaled labels (MapLibre's perspective ratio) ----

    #[test]
    fn the_perspective_ratio_is_maplibres_clamped_and_quantised() {
        assert_eq!(symbol_perspective_ratio(10.0, 10.0), 1.0);
        assert_eq!(symbol_perspective_ratio(10.0, 20.0), 0.75, "twice as far: 0.5 + 0.5 / 2");
        assert_eq!(symbol_perspective_ratio(10.0, 5.0), 1.5, "half as far: 0.5 + 0.5 * 2");
        assert_eq!(symbol_perspective_ratio(10.0, 0.1), 4.0, "clamped at MapLibre's 4.0");
        let q = symbol_perspective_ratio(10.0, 13.0);
        assert_eq!(q * PERSPECTIVE_RATIO_STEPS, (q * PERSPECTIVE_RATIO_STEPS).round(), "on a 1/16 step: {q}");
    }

    /// A z16 camera centred on the middle of one tile, and that tile.
    fn tilted_tile_camera(pitch: f32) -> (Camera, TileId) {
        let (x, y) = crate::geo::lon_lat_to_tile_xy(103.8198, 1.3521, 16.0);
        let tile = TileId { z: 16, x: x.floor() as u32, y: y.floor() as u32 };
        let (lon, lat) = crate::geo::tile_xy_to_lon_lat(x.floor() + 0.5, y.floor() + 0.5, 16.0);
        let camera = Camera {
            center_lon: lon,
            center_lat: lat,
            zoom: 16.0,
            pitch,
            bearing: 0.0,
            viewport_width_px: 1024.0,
            viewport_height_px: 768.0,
            fov_y: Camera::DEFAULT_FOV_Y,
        };
        (camera, tile)
    }

    #[test]
    fn a_flat_frame_scales_no_label() {
        let (camera, tile) = tilted_tile_camera(0.0);
        let anchors = AnchorProjection::flat(&camera, tile);
        for anchor in [geo_types::Point::new(2048, 448), geo_types::Point::new(2048, 3648)] {
            assert_eq!(anchors.perspective_ratio(&camera, anchor), 1.0);
        }
    }

    #[test]
    fn a_distant_label_draws_and_collides_smaller_than_a_near_one() {
        // 1600 MVT units is 200 centre-pixels at z16: one label that far
        // north (up the screen, farther from the eye), one that far south.
        let (camera, tile) = tilted_tile_camera(0.8);
        let far = point_label(geo_types::Point::new(2048, 448), "Far");
        let near = point_label(geo_types::Point::new(2048, 3648), "Far"); // same text: same shaped size at pitch 0
        let placed = place(&camera, GlobeAnchors::FLAT, tile, vec![far, near]);
        assert_eq!(placed.len(), 2, "both labels place: they are 200+ px apart");
        let height = |s: &DrawnSymbol| s.text_box.map(|b| b.bottom - b.top).expect("a text box");
        let (a, b) = (height(&placed[0]), height(&placed[1]));
        let (far_h, near_h) = (a.min(b), a.max(b));
        assert!(far_h < near_h * 0.95, "the far box ({far_h}) must be smaller than the near one ({near_h})");

        // And the same pair on a flat camera: identical boxes.
        let (flat, tile) = tilted_tile_camera(0.0);
        let placed = place(&flat, GlobeAnchors::FLAT, tile, vec![
            point_label(geo_types::Point::new(2048, 448), "Far"),
            point_label(geo_types::Point::new(2048, 3648), "Far"),
        ]);
        // The shaped size is bit-identical. The box HEIGHTS are compared to
        // a tolerance, not to the bit: a box is stored as edges, so its
        // height is `(top + h) - top` at two different absolute rows (about
        // 184 and 584 px here), and that subtraction alone differs in the
        // last few ULPs (16.800003 against 16.799988) with no ratio in play.
        let size = |s: &DrawnSymbol| s.text.as_ref().map(|t| t.0 .1).expect("a drawn label");
        assert_eq!(size(&placed[0]), size(&placed[1]), "a flat frame shapes both at one size");
        assert!(
            (height(&placed[0]) - height(&placed[1])).abs() < 1e-3,
            "flat boxes {} and {}",
            height(&placed[0]),
            height(&placed[1])
        );
    }

    /// The anchors the pitch-0 identity tests spread over one tile: a 5x5
    /// grid, corners and edges included.
    fn anchor_grid() -> Vec<geo_types::Point<i32>> {
        (0..5).flat_map(|i| (0..5).map(move |j| geo_types::Point::new(i * 1024, j * 1024))).collect()
    }

    /// `true` at pitch 0 whatever the bearing, the zoom's fraction or the
    /// anchor: the ratio is a BRANCH there, so no ULP of matrix depth can
    /// leak into a flat frame.
    #[test]
    fn a_flat_frame_scales_no_label_at_any_bearing_zoom_or_anchor() {
        for bearing in [0.0_f32, 0.7, -2.9, std::f32::consts::PI] {
            for zoom in [3.0_f64, 12.37, 16.0, 19.9] {
                let (mut camera, tile) = tilted_tile_camera(0.0);
                camera.bearing = bearing;
                camera.zoom = zoom;
                let anchors = AnchorProjection::flat(&camera, tile);
                for anchor in anchor_grid() {
                    assert_eq!(
                        anchors.perspective_ratio(&camera, anchor).to_bits(),
                        1.0_f32.to_bits(),
                        "bearing {bearing} zoom {zoom} anchor {anchor:?}"
                    );
                }
            }
        }
    }

    /// R11 (spec §13, ruling R11-b): a live window under a story frame
    /// sizes the basemap's labels as the export does. In a 400x711 window
    /// at 2x (800x1422 physical, story ratio 800/1080 -- neither 1 nor the
    /// backing scale), the live frame (placed at the backing scale, sized
    /// at `MapView`'s `live_symbol_size_scale`) and the capture of the same
    /// frame (ratio-sized logical viewport, `pixel_ratio` = the ratio)
    /// shape every label at the same physical glyph size, halo and blur,
    /// measure it the same, and put it at the same physical pixel (within
    /// the 1 px the origin snap allows). Without the story frame the live
    /// label is sized at the backing scale, as before.
    #[test]
    fn a_live_story_window_sizes_basemap_labels_as_the_export() {
        let (logical, scale) = ((400.0_f32, 711.0_f32), 2.0_f32);
        let physical = (logical.0 * scale, logical.1 * scale);
        let ratio = crate::map_view::live_story_ratio(physical);
        assert!((ratio - 800.0 / 1080.0).abs() < 1e-6, "{ratio}");
        assert_eq!(crate::map_view::live_symbol_size_scale(true, physical, scale), ratio);
        assert_eq!(crate::map_view::live_symbol_size_scale(false, physical, scale), scale, "no story: the backing scale");
        let (base, tile) = tilted_tile_camera(0.0);
        // The capture's camera: the frame in its own logical units at the pose zoom.
        let capture_camera = Camera { viewport_width_px: physical.0 / ratio, viewport_height_px: physical.1 / ratio, ..base };
        // The live view's: the window's logical size at the story's zoom offset.
        let live_camera = Camera {
            viewport_width_px: logical.0,
            viewport_height_px: logical.1,
            zoom: base.zoom + crate::story::live_zoom_offset(logical),
            ..base
        };
        let texts = ["A", "Orchard Road", "Bukit Timah", "Marina", "Tanjong Pagar"];
        let features: Vec<style::SymbolFeature> = anchor_grid()
            .into_iter()
            .enumerate()
            .map(|(i, anchor)| {
                let mut label = point_label(anchor, texts[i % texts.len()]);
                label.text_size = 11.0 + (i % 4) as f32 * 1.5;
                label.text_halo_width = 1.25;
                label.text_halo_blur = 0.5;
                label
            })
            .collect();
        let mut font_system = crate::font_fallback::new_font_system();
        let mut shape_cache = LabelShapeCache::new();
        let icon_rects: HashMap<String, IconRect> = HashMap::new();
        fn frame_of<'a>(camera: &'a Camera, icon_rects: &'a HashMap<String, IconRect>, scale_factor: f32, size_scale: f32, collision_padding: f32) -> SymbolFrame<'a> {
            SymbolFrame {
                occlusion: None,
                label_paint: None,
                camera,
                style_layers: &[],
                scale_factor,
                size_scale,
                collision_padding,
                size_blend_t: 0.0,
                icon_rects,
                icon_atlas_size: (0, 0),
                globe: GlobeAnchors::FLAT,
                light: crate::render::SymbolLight::UNLIT,
                fog: crate::render::HorizonFog::NONE,
            }
        }
        // `MapView::paint`'s size scale and padding, and the capture's
        // (`CaptureSession`: both at its pixel ratio).
        let mv = |story: bool| (crate::map_view::live_symbol_size_scale(story, physical, scale), crate::map_view::live_symbol_collision_padding(story, physical, scale));
        let live = frame_of(&live_camera, &icon_rects, scale, mv(true).0, mv(true).1);
        let plain = frame_of(&live_camera, &icon_rects, scale, mv(false).0, mv(false).1);
        let capture = frame_of(&capture_camera, &icon_rects, ratio, ratio, SYMBOL_COLLISION_PADDING_LOGICAL_PX * ratio);
        let mut place = |frame: &SymbolFrame<'_>, camera: &Camera, feature: &style::SymbolFeature| {
            let anchors = AnchorProjection::flat(camera, tile);
            match basemap_candidate(frame, &anchors, tile.z, feature, &mut font_system, &mut shape_cache, None) {
                SymbolOutcome::Candidate(c) => c.text.clone().zip(c.text_box).map(|(t, b)| (t, b, c.placement.bounds)),
                _ => None,
            }
        };
        let mut compared = 0;
        // The collision boxes: the label's own and the padded union collision
        // places by, the same size live as in the export, at the same place
        // within the 1 px the origin snap allows.
        let same_box = |a: crate::collision::CollisionBox, b: crate::collision::CollisionBox, what: &str, text: &str| {
            let (wa, ha, wb, hb) = (a.right - a.left, a.bottom - a.top, b.right - b.left, b.bottom - b.top);
            assert!((wa - wb).abs() < 1e-3 && (ha - hb).abs() < 1e-3, "{text}: {what} {wa}x{ha} live vs {wb}x{hb} export");
            assert!((a.left - b.left).abs() <= 1.0 && (a.top - b.top).abs() <= 1.0, "{text}: {what} at {a:?} vs {b:?}");
        };
        for feature in &features {
            let (Some((l, lbox, lbounds)), Some((c, cbox, cbounds))) = (place(&live, &live_camera, feature), place(&capture, &capture_camera, feature)) else { continue };
            let text = feature.text.as_deref().unwrap_or_default();
            same_box(lbox, cbox, "text box", text);
            same_box(lbounds, cbounds, "padded collision box", text);
            assert_eq!(l.0 .1, c.0 .1, "{text}: the physical glyph size (shape key)");
            assert_eq!(f32::from_bits(c.0 .1), feature.text_size * ratio, "{text}: the export's size");
            assert!((l.1 - c.1).abs() <= 1.0 && (l.2 - c.2).abs() <= 1.0, "{text}: at ({}, {}) live vs ({}, {}) export", l.1, l.2, c.1, c.2);
            assert_eq!((l.4.to_bits(), l.6.to_bits()), (c.4.to_bits(), c.6.to_bits()), "{text}: halo width and blur");
            let (p, ..) = place(&plain, &live_camera, feature).expect("placed without a story too");
            assert_eq!(f32::from_bits(p.0 .1), feature.text_size * scale, "{text}: no story frame, the backing scale");
            compared += 1;
        }
        assert!(compared >= 10, "{compared} labels compared");
    }

    /// **PITCH 0 IS UNCHANGED.** Every placed label's shape-cache size, halo,
    /// origin and collision box equal what the pre-perspective formulas give
    /// -- `text_size * scale_factor`, `text_halo_width * scale_factor`, the
    /// origin `text_area_origin` puts at the anchor's pixel, and a box the
    /// size shaping measured at that size -- to the bit, at a HiDPI scale
    /// factor and a rotated bearing so neither can hide a multiply by one.
    ///
    /// Predicted rather than hard-coded: the measured widths come from the
    /// machine's installed fonts, so a golden number would pass here and
    /// fail on the next machine.
    #[test]
    fn at_pitch_0_every_label_is_sized_and_placed_as_before_perspective() {
        let scale_factor = 2.0_f32;
        let (mut camera, tile) = tilted_tile_camera(0.0);
        camera.bearing = 0.7;
        let texts = ["A", "Orchard Road", "Bukit Timah", "Marina", "Tanjong Pagar"];
        let features: Vec<style::SymbolFeature> = anchor_grid()
            .into_iter()
            .enumerate()
            .map(|(i, anchor)| {
                let mut label = point_label(anchor, texts[i % texts.len()]);
                label.text_size = 11.0 + (i % 4) as f32 * 1.5;
                label.text_halo_width = 1.25;
                label.text_halo_blur = 0.5;
                label.text_offset = (0.0, 0.3);
                label
            })
            .collect();
        let mut font_system = crate::font_fallback::new_font_system();
        let mut shape_cache = LabelShapeCache::new();
        let icon_rects: HashMap<String, IconRect> = HashMap::new();
        let frame = SymbolFrame {
            occlusion: None,
            label_paint: None,
            camera: &camera,
            style_layers: &[],
            scale_factor,
            size_scale: scale_factor,
            collision_padding: SYMBOL_COLLISION_PADDING_LOGICAL_PX * scale_factor,
            size_blend_t: 0.0,
            icon_rects: &icon_rects,
            icon_atlas_size: (0, 0),
            globe: GlobeAnchors::FLAT,
            light: crate::render::SymbolLight::UNLIT,
            fog: crate::render::HorizonFog::NONE,
        };
        let anchors = AnchorProjection::flat(&camera, tile);
        let mut checked = 0;
        for feature in &features {
            let SymbolOutcome::Candidate(candidate) = basemap_candidate(
                &frame,
                &anchors,
                tile.z,
                feature,
                &mut font_system,
                &mut shape_cache,
                None,
            ) else {
                continue;
            };
            let (screen_x, screen_y) = anchors.screen(&camera, feature.anchor).expect("on screen");
            let size = feature.text_size * scale_factor;
            let face = shape_cache.face_for(&font_system, &feature.text_font);
            let text = feature.text.as_deref().expect("a label");
            let shaped = shape_cache.get_or_shape(
                &mut font_system,
                text,
                size,
                None,
                crate::overlay::paint::TextJustify::Center,
                &face,
            );
            let (width, height) = (shaped.measured_width, shaped.measured_height);
            let (left, top) = text_area_origin(
                screen_x,
                screen_y,
                scale_factor,
                LabelAnchoring { offset_em: feature.text_offset, anchor: feature.text_anchor },
                size,
                width,
                height,
            );
            let draw = candidate.text.as_ref().expect("a drawn label");
            assert_eq!(draw.0 .1, size.to_bits(), "{text}: shape-cache size");
            assert_eq!((draw.1.to_bits(), draw.2.to_bits()), (left.to_bits(), top.to_bits()), "{text}: origin");
            assert_eq!(draw.4.to_bits(), (feature.text_halo_width * scale_factor).to_bits(), "{text}: halo");
            assert_eq!(draw.6.to_bits(), (feature.text_halo_blur * scale_factor).to_bits(), "{text}: blur");
            assert_eq!(
                candidate.text_box,
                Some(text_collision_box(left, top, width, height)),
                "{text}: collision box"
            );
            checked += 1;
        }
        assert!(checked >= 9, "only {checked} labels reached the check; the fixture proves nothing");
    }

    /// **NO CHURN.** A still, tilted camera re-shapes nothing on its second
    /// frame -- the ratio is a pure function of the frame, so each label
    /// lands on the same 1/16 step and the same shape-cache key -- while its
    /// first frame did shape labels at more than one size, so the ratio was
    /// actually in play.
    #[test]
    fn a_still_tilted_camera_shapes_nothing_on_its_second_frame() {
        let (camera, tile) = tilted_tile_camera(0.9);
        let features: Vec<style::SymbolFeature> =
            anchor_grid().into_iter().map(|anchor| point_label(anchor, "Same")).collect();
        let plan = style::SymbolPlan { features };
        let mut font_system = crate::font_fallback::new_font_system();
        let mut shape_cache = LabelShapeCache::new();
        let icon_rects: HashMap<String, IconRect> = HashMap::new();
        let frame = SymbolFrame {
            occlusion: None,
            label_paint: None,
            camera: &camera,
            style_layers: &[],
            scale_factor: 1.0,
            size_scale: 1.0,
            collision_padding: SYMBOL_COLLISION_PADDING_LOGICAL_PX,
            size_blend_t: 0.0,
            icon_rects: &icon_rects,
            icon_atlas_size: (0, 0),
            globe: GlobeAnchors::FLAT,
            light: crate::render::SymbolLight::UNLIT,
            fog: crate::render::HorizonFog::NONE,
        };
        let mut walk = |shape_cache: &mut LabelShapeCache| {
            let before = shape_cache.shape_count;
            let (mut candidates, mut bypassed) = (Vec::new(), Vec::new());
            collect_candidates(
                &frame,
                std::iter::once((tile, &plan)),
                &mut font_system,
                shape_cache,
                None,
                &mut candidates,
                &mut bypassed,
            );
            let keys: Vec<LabelKey> =
                candidates.iter().filter_map(|c| c.text.as_ref().map(|t| t.0.clone())).collect();
            (shape_cache.shape_count - before, keys)
        };
        let (first_shaped, first_keys) = walk(&mut shape_cache);
        let (second_shaped, second_keys) = walk(&mut shape_cache);
        let sizes: std::collections::BTreeSet<u32> = first_keys.iter().map(|key| key.1).collect();
        assert!(sizes.len() > 1, "one text at one style size shaped at {} sizes; want > 1", sizes.len());
        assert_eq!(first_shaped, sizes.len(), "one shape per distinct quantised size");
        assert_eq!(second_shaped, 0, "a still camera re-shaped {second_shaped} labels");
        assert_eq!(first_keys, second_keys, "a still camera moved a label to another size");
    }

    /// **LABELS OVER DATA.** With an on-data paint on the frame
    /// (`MapView::set_basemap_label_paint`) every basemap label draws in
    /// it -- text colour, halo colour, width and blur -- and without one
    /// in its style's own: a point-only map is untouched.
    #[test]
    fn an_on_data_paint_replaces_every_labels_style_paint() {
        let (camera, tile) = tilted_tile_camera(0.0);
        let features: Vec<style::SymbolFeature> =
            anchor_grid().into_iter().map(|anchor| point_label(anchor, "Kyoto")).collect();
        let plan = style::SymbolPlan { features };
        let mut font_system = crate::font_fallback::new_font_system();
        let mut shape_cache = LabelShapeCache::new();
        let icon_rects: HashMap<String, IconRect> = HashMap::new();
        let mut draws = |label_paint: Option<LabelsOnData<'_>>| {
            let frame = SymbolFrame {
                occlusion: None,
                label_paint,
                camera: &camera,
                style_layers: &[],
                scale_factor: 1.0,
                size_scale: 1.0,
                collision_padding: SYMBOL_COLLISION_PADDING_LOGICAL_PX,
                size_blend_t: 0.0,
                icon_rects: &icon_rects,
                icon_atlas_size: (0, 0),
                globe: GlobeAnchors::FLAT,
                light: crate::render::SymbolLight::UNLIT,
                fog: crate::render::HorizonFog::NONE,
            };
            let (mut candidates, mut bypassed) = (Vec::new(), Vec::new());
            collect_candidates(
                &frame,
                std::iter::once((tile, &plan)),
                &mut font_system,
                &mut shape_cache,
                None,
                &mut candidates,
                &mut bypassed,
            );
            candidates.into_iter().filter_map(|c| c.text).collect::<Vec<_>>()
        };
        let style_feature = &plan.features[0];
        let own = draws(None);
        assert!(!own.is_empty(), "the fixture drew no label");
        for d in &own {
            assert_eq!((d.3, d.5), (style_feature.text_color, style_feature.text_halo_color), "no paint: the style's own");
            assert_eq!(d.4, style_feature.text_halo_width);
        }
        let on = style::LabelPaint::ON_DATA;
        let on_data = draws(Some(LabelsOnData { paint: on, over_data: None }));
        assert_eq!(on_data.len(), own.len(), "the paint changes no placement");
        for d in &on_data {
            assert_eq!((d.3, d.5), (on.text_color, on.halo_color), "dark text, light halo");
            assert_eq!((d.4, d.6), (on.halo_width, on.halo_blur), "the halo's width and blur");
        }
        // Per label: only the labels over data take it; the rest keep the
        // style's paint (a label on the bare dark basemap stays light).
        let west = |_: TileId, anchor: (i32, i32)| anchor.0 < 2048;
        let split = draws(Some(LabelsOnData { paint: on, over_data: Some(&west) }));
        assert_eq!(split.len(), own.len());
        let over = split.iter().filter(|d| d.3 == on.text_color).count();
        let off = split.iter().filter(|d| d.3 == style_feature.text_color && d.5 == style_feature.text_halo_color).count();
        assert!(over > 0 && off > 0, "{over} labels over data, {off} off it: the fixture must have both");
        assert_eq!(over + off, split.len());
    }

    /// **NO OSCILLATION.** As the camera tilts smoothly, a far anchor's
    /// quantised ratio only ever steps DOWN (and a near one's only up): it
    /// changes step a bounded number of times and never flips back and
    /// forth between two neighbouring steps.
    #[test]
    fn a_tilting_camera_steps_each_ratio_one_way_only() {
        let (far, near) = (geo_types::Point::new(2048, 0), geo_types::Point::new(2048, 4096));
        let mut previous: Option<(f32, f32)> = None;
        let mut steps = 0;
        for i in 0..=2000 {
            let (camera, tile) = tilted_tile_camera(1.0 * i as f32 / 2000.0);
            let anchors = AnchorProjection::flat(&camera, tile);
            let now = (anchors.perspective_ratio(&camera, far), anchors.perspective_ratio(&camera, near));
            // The look-at point is the tile's centre: its clip `w` IS
            // `Camera::distance`, so the units agree or this is not 1.
            let centre = anchors.perspective_ratio(&camera, geo_types::Point::new(2048, 2048));
            assert_eq!(centre, 1.0, "the look-at anchor at pitch {}", camera.pitch);
            if let Some(before) = previous {
                assert!(now.0 <= before.0, "far ratio went back up at step {i}: {before:?} -> {now:?}");
                assert!(now.1 >= before.1, "near ratio went back down at step {i}: {before:?} -> {now:?}");
                steps += usize::from(now != before);
            }
            previous = Some(now);
        }
        assert!(steps > 0, "the ratio never changed across 0..1 rad of pitch");
        assert!(steps < 40, "{steps} step changes over 2000 frames of tilt");
    }

    /// **THE DEPTH GUARD.** A clip depth of zero, below zero, NaN or
    /// infinite has no perspective ratio: exactly `1.0`, never a clamped
    /// `4.0`, a `0.0` or a NaN size.
    #[test]
    fn a_depth_with_no_ratio_scales_no_label() {
        for depth in [0.0_f32, -0.0, -1.0, -1e-30, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(
                perspective_ratio_at_depth(10.0, depth).to_bits(),
                1.0_f32.to_bits(),
                "depth {depth}"
            );
        }
        assert_eq!(perspective_ratio_at_depth(10.0, 20.0), 0.75, "a real depth still takes the ratio");
    }

    /// The same guard reached through a REAL camera: an anchor in a tile far
    /// behind a tilted eye has a negative clip `w`, and its ratio is 1.0.
    #[test]
    fn an_anchor_behind_a_tilted_eye_scales_no_label() {
        let (camera, tile) = tilted_tile_camera(1.0);
        let anchor = geo_types::Point::new(2048, 2048);
        let behind = TileId { y: tile.y + 16, ..tile };
        let anchors = AnchorProjection::flat(&camera, behind);
        let local = glam::Vec4::new(2048.0 * 1000.0, 2048.0 * 1000.0, 0.0, 1.0);
        let w = (anchors.tile_to_clip * local).w;
        assert!(w < 0.0, "the fixture must be behind the eye: w = {w}");
        assert_eq!(anchors.perspective_ratio(&camera, anchor).to_bits(), 1.0_f32.to_bits());
    }

    // ---- Line labels walked in SCREEN space (final fix, item 1) ----

    /// The display scale the line-label tests run at: 2, so a mix-up
    /// between logical and physical pixels cannot hide.
    const LINE_SCALE: f32 = 2.0;

    /// 60 degrees of pitch, the controller's maximum, in radians.
    const SIXTY_DEGREES: f32 = std::f32::consts::FRAC_PI_3;

    /// A realistic run: physical advances of a 14 px label at 2x, varied the
    /// way real letters are.
    const LINE_ADVANCES: [f32; 12] = [18.0, 16.0, 20.0, 15.0, 12.0, 8.0, 16.0, 10.0, 18.0, 17.0, 20.0, 16.0];

    /// The camera the item-1 crops were taken at: pitch 60, bearing 90, z16,
    /// centred on the middle of one tile.
    fn pitched_line_camera(pitch: f32, bearing: f32) -> (Camera, TileId) {
        let (mut camera, tile) = tilted_tile_camera(pitch);
        camera.bearing = bearing;
        (camera, tile)
    }

    /// [`line_glyph_placements`] on a flat frame at [`LINE_SCALE`] -- the
    /// placement the quads, the collision box and the census all read.
    fn line_placements(
        camera: &Camera,
        tile: TileId,
        line: &geo_types::LineString<i32>,
        centre_distance: f64,
        advances: &[f32],
    ) -> Option<Vec<Option<crate::symbol_line::ScreenGlyph>>> {
        let icon_rects: HashMap<String, IconRect> = HashMap::new();
        let frame = SymbolFrame {
            occlusion: None,
            label_paint: None,
            camera,
            style_layers: &[],
            scale_factor: LINE_SCALE,
            size_scale: LINE_SCALE,
            collision_padding: 0.0,
            size_blend_t: 0.0,
            icon_rects: &icon_rects,
            icon_atlas_size: (0, 0),
            globe: GlobeAnchors::FLAT,
            light: crate::render::SymbolLight::UNLIT,
            fog: crate::render::HorizonFog::NONE,
        };
        let anchors = AnchorProjection::flat(camera, tile);
        line_glyph_placements(
            &frame,
            &anchors,
            line,
            centre_distance,
            advances,
            tile_units_per_physical_px(camera.zoom, tile.z, LINE_SCALE),
            crate::style::RotationAlignment::Map,
        )
    }

    fn every_glyph(placed: Vec<Option<crate::symbol_line::ScreenGlyph>>) -> Vec<crate::symbol_line::ScreenGlyph> {
        placed.into_iter().map(|glyph| glyph.expect("every glyph of a pitched run projects")).collect()
    }

    /// **THE LETTERS OF A PITCHED STREET NAME ARE ONE ADVANCE APART, NEAR
    /// AND FAR.** The glyph size already carries the anchor's perspective
    /// ratio, so its advances are screen lengths; walking them along the
    /// tile line through a flat constant and then projecting each glyph
    /// compressed the far gaps ("RpublicBulevard") and stretched the near
    /// ones ("R o c h o r Road"). Consecutive centres must sit the mean of
    /// their two advances apart, to half a pixel, on a road that recedes
    /// and on one that crosses the view, both near the eye and far from it.
    #[test]
    fn pitched_line_glyphs_are_spaced_by_their_advances_near_and_far() {
        let (camera, tile) = pitched_line_camera(SIXTY_DEGREES, std::f32::consts::FRAC_PI_2);
        let anchors = AnchorProjection::flat(&camera, tile);
        let mut ratios = Vec::new();
        let mut failures = Vec::new();
        for offset in [-1600, 1600] {
            let receding = geo_types::LineString::from(vec![(-30000, 2048), (34000, 2048)]);
            let x = 2048 + offset;
            let crossing = geo_types::LineString::from(vec![(x, -30000), (x, 34000)]);
            ratios.push(anchors.perspective_ratio(&camera, geo_types::Point::new(x, 2048)));
            for (name, line, centre) in [
                ("receding", &receding, f64::from(x + 30000)),
                ("crossing", &crossing, 32048.0),
            ] {
                let placed = every_glyph(
                    line_placements(&camera, tile, line, centre, &LINE_ADVANCES)
                        .unwrap_or_else(|| panic!("the {name} run at offset {offset} places")),
                );
                // Worst gap per run, so a failure names every case at once.
                let worst = placed
                    .windows(2)
                    .enumerate()
                    .map(|(i, pair)| {
                        let gap = (pair[1].centre.0 - pair[0].centre.0).hypot(pair[1].centre.1 - pair[0].centre.1);
                        let expected = (LINE_ADVANCES[i] + LINE_ADVANCES[i + 1]) / 2.0;
                        (i, gap, expected)
                    })
                    .max_by(|a, b| (a.1 - a.2).abs().total_cmp(&(b.1 - b.2).abs()))
                    .expect("a 12-glyph run has gaps");
                if (worst.1 - worst.2).abs() > 0.5 {
                    failures.push(format!(
                        "{name} road, offset {offset}: glyphs {} and {} are {} px apart, their advances say {}",
                        worst.0,
                        worst.0 + 1,
                        worst.1,
                        worst.2
                    ));
                }
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
        // The fixture must really have a near and a far side.
        let (low, high) = (ratios[0].min(ratios[1]), ratios[0].max(ratios[1]));
        assert!(low < 0.95 && high > 1.05, "one anchor near, one far: ratios {ratios:?}");
    }

    /// A road running from the screen to behind the eye: the vertex behind
    /// the near plane does not project, and the walk synthesises the
    /// stretch's end from the direction the segment leaves the screen.
    /// Whatever the anchor, the result is finite glyphs or a refusal --
    /// never a panic, never a NaN quad.
    #[test]
    fn a_line_crossing_the_near_plane_places_finite_glyphs_or_refuses() {
        let (camera, tile) = pitched_line_camera(SIXTY_DEGREES, std::f32::consts::FRAC_PI_2);
        let mut placed_any = 0;
        for far in [-400_000, 400_000] {
            let line = geo_types::LineString::from(vec![(2048, 2048), (2048 + far, 2048), (2048 + far, 9000)]);
            for step in 0..200 {
                let centre = f64::from(step) * 37.0;
                if let Some(placed) = line_placements(&camera, tile, &line, centre, &LINE_ADVANCES) {
                    for glyph in placed.into_iter().flatten() {
                        assert!(
                            glyph.centre.0.is_finite() && glyph.centre.1.is_finite() && glyph.angle.is_finite(),
                            "far {far}, centre {centre}: {glyph:?}"
                        );
                    }
                    placed_any += 1;
                }
            }
        }
        assert!(placed_any > 0, "some anchor near the visible vertex must still place");
    }

    /// Keep-upright on the screen walk: a road digitised EAST TO WEST reads
    /// left to right at bearing 0 and at bearing 180, and so does one
    /// digitised west to east.
    #[test]
    fn a_pitched_road_reads_left_to_right_whichever_way_it_was_digitised() {
        for bearing in [0.0, std::f32::consts::PI] {
            let (camera, tile) = pitched_line_camera(SIXTY_DEGREES, bearing);
            for (name, line) in [
                ("east to west", geo_types::LineString::from(vec![(34000, 2048), (-30000, 2048)])),
                ("west to east", geo_types::LineString::from(vec![(-30000, 2048), (34000, 2048)])),
            ] {
                let placed = line_placements(&camera, tile, &line, 32000.0, &LINE_ADVANCES).expect("places");
                let placed = every_glyph(placed);
                let (first, last) = (placed[0].centre.0, placed[placed.len() - 1].centre.0);
                assert!(first < last, "{name} at bearing {bearing}: first glyph x {first}, last {last}");
            }
        }
    }

    /// **AN OVERZOOMED FLAT FRAME'S STREET NAME IS STRAIGHT AND EVENLY
    /// SPACED.** The "Be a c h Rd" defect: at pitch 0 the tile walk used to
    /// snap every glyph centre to a whole tile unit, and one tile unit is
    /// `512 * 2^(zoom - tile.z) / 4096` logical px -- 4 at z19 on a z14
    /// tile, 12 physical px on a 3x phone. On a straight, slanted road the
    /// glyph centres must sit on the line to half a physical pixel, and
    /// consecutive centres must be the mean of their two advances apart to
    /// half a pixel, at z19 on z14 (32x overzoom) and z20 on z16 (16x).
    #[test]
    fn an_overzoomed_unpitched_road_name_is_straight_and_evenly_spaced() {
        const PHONE_SCALE: f32 = 3.0;
        let advances: Vec<f32> = LINE_ADVANCES.iter().map(|a| a * 1.5).collect();
        let icon_rects: HashMap<String, IconRect> = HashMap::new();
        let mut failures = Vec::new();
        for (zoom, tile_z) in [(19.0_f64, 14_u8), (20.0, 16)] {
            let (x, y) = crate::geo::lon_lat_to_tile_xy(103.8540, 1.2975, f64::from(tile_z));
            let tile = TileId { z: tile_z, x: x.floor() as u32, y: y.floor() as u32 };
            // The camera looks at tile unit (1516, 2499) exactly.
            let (fx, fy) = (1516.0 / 4096.0, 2499.0 / 4096.0);
            let (lon, lat) = crate::geo::tile_xy_to_lon_lat(x.floor() + fx, y.floor() + fy, f64::from(tile_z));
            let camera = Camera {
                center_lon: lon,
                center_lat: lat,
                zoom,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: 430.0,
                viewport_height_px: 932.0,
                fov_y: Camera::DEFAULT_FOV_Y,
            };
            let frame = SymbolFrame {
                occlusion: None,
                label_paint: None,
                camera: &camera,
                style_layers: &[],
                scale_factor: PHONE_SCALE,
                size_scale: PHONE_SCALE,
                collision_padding: 0.0,
                size_blend_t: 0.0,
                icon_rects: &icon_rects,
                icon_atlas_size: (0, 0),
                globe: GlobeAnchors::FLAT,
                light: crate::render::SymbolLight::UNLIT,
                fog: crate::render::HorizonFog::NONE,
            };
            let anchors = AnchorProjection::flat(&camera, tile);
            let units = tile_units_per_physical_px(camera.zoom, tile.z, PHONE_SCALE);
            // Slanted (about 16 degrees) so a rounded centre falls off the
            // line, and a horizontal one so the spacing alone is visible.
            for (name, a, b) in [("slanted", (916, 2326), (2116, 2672)), ("horizontal", (916, 2499), (2116, 2499))] {
                let line = geo_types::LineString::from(vec![a, b]);
                for shift in 0..8 {
                    // Sub-unit anchor offsets, so the test does not pass by
                    // a lucky alignment with the tile grid.
                    let centre = crate::symbol_line::line_length(&line) / 2.0 + f64::from(shift) * 0.13;
                    let placed = line_glyph_placements(
                        &frame,
                        &anchors,
                        &line,
                        centre,
                        &advances,
                        units,
                        crate::style::RotationAlignment::Map,
                    )
                    .expect("a straight road through the view places");
                    let placed: Vec<_> = placed.into_iter().map(|g| g.expect("projects")).collect();
                    let (sa, sb) = (
                        anchors.screen(&camera, geo_types::Point::new(a.0, a.1)).expect("projects"),
                        anchors.screen(&camera, geo_types::Point::new(b.0, b.1)).expect("projects"),
                    );
                    let s = f64::from(PHONE_SCALE);
                    let (ax, ay) = (f64::from(sa.0) * s, f64::from(sa.1) * s);
                    let (dx, dy) = (f64::from(sb.0) * s - ax, f64::from(sb.1) * s - ay);
                    let len = dx.hypot(dy);
                    let offsets: Vec<f64> = placed
                        .iter()
                        .map(|g| ((f64::from(g.centre.0) - ax) * dy - (f64::from(g.centre.1) - ay) * dx) / len)
                        .collect();
                    let spread = offsets.iter().copied().fold(f64::MIN, f64::max)
                        - offsets.iter().copied().fold(f64::MAX, f64::min);
                    let spacing_errors: Vec<f64> = placed
                        .windows(2)
                        .enumerate()
                        .map(|(i, pair)| {
                            let gap = f64::from(pair[1].centre.0 - pair[0].centre.0)
                                .hypot(f64::from(pair[1].centre.1 - pair[0].centre.1));
                            gap - f64::from(advances[i] + advances[i + 1]) / 2.0
                        })
                        .collect();
                    let worst_spacing = spacing_errors.iter().map(|e| e.abs()).fold(0.0, f64::max);
                    if shift == 0 {
                        eprintln!(
                            "z{zoom} on z{tile_z} {name}: perpendicular offsets {offsets:.2?} (spread {spread:.2}), \
                             spacing errors {spacing_errors:.2?}"
                        );
                    }
                    if spread >= 0.5 || worst_spacing > 0.5 {
                        failures.push(format!(
                            "z{zoom} on z{tile_z} {name} shift {shift}: baseline spread {spread:.2} px, \
                             worst spacing error {worst_spacing:.2} px"
                        ));
                    }
                }
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    /// **PITCH 0 DOES NOT MOVE.** An unpitched flat frame takes the tile
    /// walk by a branch, so its placements are the tile walk's to the bit
    /// -- every quad and collision box is built from them. And the screen
    /// walk, had it been taken, lands on the same points to f32 noise: the
    /// tile walk no longer snaps a glyph to a whole tile unit
    /// (`symbol_line::GlyphAlong`), so the two walks are the same placement
    /// there, which is why the branch costs nothing. (Before that fix they
    /// agreed only to the rounding, 0.2 px here and 6 px at z19 on z14.)
    #[test]
    fn an_unpitched_frame_places_line_glyphs_bit_identically_to_the_tile_walk() {
        let icon_rects: HashMap<String, IconRect> = HashMap::new();
        for bearing in [0.0_f32, 0.7, -2.9, std::f32::consts::PI] {
            let (camera, tile) = pitched_line_camera(0.0, bearing);
            let frame = SymbolFrame {
                occlusion: None,
                label_paint: None,
                camera: &camera,
                style_layers: &[],
                scale_factor: LINE_SCALE,
                size_scale: LINE_SCALE,
                collision_padding: 0.0,
                size_blend_t: 0.0,
                icon_rects: &icon_rects,
                icon_atlas_size: (0, 0),
                globe: GlobeAnchors::FLAT,
                light: crate::render::SymbolLight::UNLIT,
                fog: crate::render::HorizonFog::NONE,
            };
            let anchors = AnchorProjection::flat(&camera, tile);
            let units = tile_units_per_physical_px(camera.zoom, tile.z, LINE_SCALE);
            let map = crate::style::RotationAlignment::Map;
            for line in [
                geo_types::LineString::from(vec![(0, 2048), (4096, 2048)]),
                geo_types::LineString::from(vec![(4096, 300), (300, 3900)]),
                geo_types::LineString::from(vec![(500, 500), (2048, 1500), (3600, 1700)]),
            ] {
                let centre = crate::symbol_line::line_length(&line) / 2.0;
                let chosen = line_glyph_placements(&frame, &anchors, &line, centre, &LINE_ADVANCES, units, map)
                    .expect("places");
                let tile_walk = tile_walk_glyphs(&frame, &anchors, &line, centre, &LINE_ADVANCES, units, map)
                    .expect("places");
                let bits = |glyphs: &[Option<crate::symbol_line::ScreenGlyph>]| -> Vec<[u32; 3]> {
                    glyphs
                        .iter()
                        .map(|g| g.expect("projects"))
                        .map(|g| [g.centre.0.to_bits(), g.centre.1.to_bits(), g.angle.to_bits()])
                        .collect()
                };
                assert_eq!(bits(&chosen), bits(&tile_walk), "bearing {bearing}: pitch 0 is the tile walk, bit for bit");

                let screen = screen_walk_glyphs(&frame, &anchors, &line, centre, &LINE_ADVANCES, map)
                    .expect("the screen walk places it too");
                for (a, b) in screen.iter().zip(tile_walk.iter().map(|g| g.expect("projects"))) {
                    let moved = (a.centre.0 - b.centre.0).hypot(a.centre.1 - b.centre.1);
                    assert!(moved < 0.02, "bearing {bearing}: the walks differ by {moved} px, more than f32 noise");
                    assert!((a.angle - b.angle).abs() < 1e-3, "bearing {bearing}: angles {} vs {}", a.angle, b.angle);
                }
            }
        }
    }

    // ---- Symbols and the horizon fade (final fix, item 5) ----

    /// A label anchored at or past the fade's end floats over pure
    /// background, so it is not placed; one nearer the eye than the band is
    /// untouched; and with the fade off both place, as before.
    #[test]
    fn a_label_past_the_fog_end_is_not_placed_and_one_before_the_band_is_untouched() {
        let (camera, tile) = tilted_tile_camera(SIXTY_DEGREES);
        let anchors = AnchorProjection::flat(&camera, tile);
        let (far, near) = (geo_types::Point::new(2048, 448), geo_types::Point::new(2048, 3648));
        let (far_w, near_w) = (anchors.depth(far), anchors.depth(near));
        assert!(far_w > near_w, "the fixture's north anchor is the far one: {far_w} vs {near_w}");
        let end = (far_w + near_w) / 2.0;
        let fog = crate::render::HorizonFog { start: near_w + (end - near_w) / 2.0, end, color: [1.0; 4] };
        let features = || vec![point_label(far, "Far"), point_label(near, "Near")];

        let fogged = place_with_fog(&camera, GlobeAnchors::FLAT, tile, features(), fog);
        assert_eq!(fogged.len(), 1, "only the near label places");
        let label = fogged[0].text.as_ref().expect("a drawn label");
        assert_eq!(label.3.a.to_bits(), 1.0_f32.to_bits(), "in front of the band: unfaded");

        assert_eq!(place(&camera, GlobeAnchors::FLAT, tile, features()).len(), 2, "no fade, both place");
    }

    /// Inside the band a label fades with the ground under it: its opacity
    /// is `1 - smoothstep(start, end, depth)`, the shader's own fade
    /// inverted.
    #[test]
    fn a_label_inside_the_fog_band_fades_with_the_ground() {
        let (camera, tile) = tilted_tile_camera(SIXTY_DEGREES);
        let anchors = AnchorProjection::flat(&camera, tile);
        let anchor = geo_types::Point::new(2048, 1200);
        let w = anchors.depth(anchor);
        let fog = crate::render::HorizonFog { start: w * 0.9, end: w * 1.05, color: [1.0; 4] };
        let placed = place_with_fog(&camera, GlobeAnchors::FLAT, tile, vec![point_label(anchor, "Band")], fog);
        assert_eq!(placed.len(), 1);
        let alpha = placed[0].text.as_ref().expect("a drawn label").3.a;
        let t = ((w - fog.start) / (fog.end - fog.start)).clamp(0.0, 1.0);
        let expected = 1.0 - t * t * (3.0 - 2.0 * t);
        assert!(alpha > 0.0 && alpha < 1.0, "a label inside the band is partly faded: {alpha}");
        assert!((alpha - expected).abs() < 1e-5, "{alpha} vs {expected}");
    }
}

/// The route line as a label obstacle (`NavHost::collision_boxes`), seeded
/// into the index after `reset` and before `place_visible_symbols` -- the
/// order every shell uses. These run in the debug profile the phone ships,
/// where `place_visible_symbols`' stale-placement guard is live.
#[cfg(test)]
mod route_obstacle_tests {
    use super::globe_tests::{candidates_for, planet_camera, point_label};
    use super::*;
    use crate::render::globe::{globe_frame, GlobeAnchors};

    /// A near-side point label that the globe tests already prove places,
    /// and the index its frame would hand to `place_visible_symbols`.
    fn somewhere() -> (Vec<SymbolCandidate>, CollisionIndex) {
        let camera = planet_camera();
        let globe = GlobeAnchors::new(&globe_frame(&camera));
        let candidates = candidates_for(
            &camera,
            globe,
            TileId { z: 2, x: 2, y: 2 },
            vec![point_label(geo_types::Point::new(1024, 1024), "Somewhere")],
            crate::render::HorizonFog::NONE,
        );
        let index = CollisionIndex::new(symbol_collision_bounds(
            camera.viewport_width_px,
            camera.viewport_height_px,
            1.0,
        ));
        (candidates, index)
    }

    /// The crash: iOS, pressing Simulate. A route box seeded this frame is
    /// not a placement left over from the last one.
    #[test]
    fn route_obstacles_seeded_after_reset_do_not_trip_the_stale_placement_guard() {
        let (candidates, mut index) = somewhere();
        index.reset(symbol_collision_bounds(2048.0, 512.0, 1.0));
        index.insert_obstacle(CollisionBox { left: 0.0, top: 0.0, right: 10.0, bottom: 10.0 });
        let placed = place_visible_symbols(candidates, &mut index);
        assert_eq!(placed.len(), 1, "an obstacle far from the label leaves it placed");
    }

    /// The guard stays alive: a label placed last frame and never `reset`
    /// away is still caught, obstacles or not.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "place_visible_symbols requires a cleared index")]
    fn a_placement_left_over_from_the_previous_frame_still_trips_the_guard() {
        let (candidates, mut index) = somewhere();
        index.insert_obstacle(CollisionBox { left: 0.0, top: 0.0, right: 10.0, bottom: 10.0 });
        assert!(index.place(CollisionBox { left: 100.0, top: 100.0, right: 140.0, bottom: 120.0 }));
        let _ = place_visible_symbols(candidates, &mut index);
    }

    /// An obstacle is an obstacle: a label on it is rejected, and the same
    /// label with the obstacle elsewhere is placed.
    #[test]
    fn a_label_on_a_seeded_route_obstacle_is_rejected_and_one_clear_of_it_is_placed() {
        let (candidates, mut index) = somewhere();
        let label_box = place_visible_symbols(candidates, &mut index)[0].text_box.expect("a text box");

        let (candidates, mut index) = somewhere();
        let centre = ((label_box.left + label_box.right) / 2.0, (label_box.top + label_box.bottom) / 2.0);
        index.insert_obstacle(CollisionBox {
            left: centre.0 - 2.0,
            top: centre.1 - 2.0,
            right: centre.0 + 2.0,
            bottom: centre.1 + 2.0,
        });
        assert!(place_visible_symbols(candidates, &mut index).is_empty(), "the route runs through the label");

        let (candidates, mut index) = somewhere();
        index.insert_obstacle(CollisionBox {
            left: label_box.right + 50.0,
            top: label_box.top,
            right: label_box.right + 60.0,
            bottom: label_box.bottom,
        });
        assert_eq!(place_visible_symbols(candidates, &mut index).len(), 1, "the route is 50 px to the right");
    }

    /// A navigation frame with labels, end to end in debug: a real
    /// simulated drive over the Manila fixture, its follow camera, its
    /// route boxes seeded the way every shell seeds them, and a grid of
    /// labels over the tile under the camera through `label_draw_plan` --
    /// the shared call the three shells make. This is the frame that
    /// aborted on the phone.
    #[test]
    fn a_nav_frame_with_labels_places_them_off_the_route_and_does_not_panic() {
        use crate::nav::host::{NavFrame, NavHost, SafeInsets, SimOptions, WallClock};
        const MANILA: &[u8] = include_bytes!("../tests/fixtures/routes/manila.json");
        let start = Camera {
            center_lon: 120.98,
            center_lat: 14.59,
            zoom: 16.0,
            pitch: 0.0,
            bearing: 0.0,
            viewport_width_px: 400.0,
            viewport_height_px: 400.0,
            fov_y: Camera::DEFAULT_FOV_Y,
        };
        let mut host = NavHost::new();
        host.set_demo_route(
            std::sync::Arc::from(MANILA),
            SimOptions { start_at_m: Some(700.0), paused: true, ..Default::default() },
        );
        let mut control = crate::camera_control::CameraController::new(
            start,
            crate::camera_control::MIN_ZOOM,
            crate::camera_control::MAX_ZOOM,
        );
        let frame_at = |now_ms: f64| NavFrame {
            now_ms,
            wall: WallClock { unix_ms: 1_773_755_400_000.0 + now_ms, utc_offset_min: 480 },
            viewport: (400.0, 400.0),
            insets: SafeInsets::default(),
        };
        host.start_simulation(&control, &frame_at(0.0)).expect("the Manila fixture drives");
        let _ = host.advance(&mut control, &frame_at(0.0));
        let _ = host.advance(&mut control, &frame_at(1000.0));
        let camera = *control.camera();
        let route = host.collision_boxes(&camera, 1.0);
        assert!(!route.is_empty(), "the drive has a route on screen");

        // The 3x3 block of z16 tiles around the one under the camera -- the
        // tilted follow view shows more than one -- each with a dense 32x32
        // grid of short labels.
        let n = f64::from(1u32 << 16);
        let x = ((camera.center_lon + 180.0) / 360.0 * n).floor() as u32;
        let lat = camera.center_lat.to_radians();
        let y = ((1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0 * n).floor() as u32;
        let features: Vec<_> = (0..1024)
            .map(|i| point_label(geo_types::Point::new(64 + (i % 32) * 128, 64 + (i / 32) * 128), &format!("L{i}")))
            .collect();
        let candidates = || {
            let mut all = Vec::new();
            for tile_y in y - 1..=y + 1 {
                for tile_x in x - 1..=x + 1 {
                    all.extend(candidates_for(
                        &camera,
                        GlobeAnchors::FLAT,
                        TileId { z: 16, x: tile_x, y: tile_y },
                        features.clone(),
                        crate::render::HorizonFog::NONE,
                    ));
                }
            }
            all
        };
        let bounds = symbol_collision_bounds(camera.viewport_width_px, camera.viewport_height_px, 1.0);
        let on_route = |drawn: &[DrawnSymbol]| {
            drawn
                .iter()
                .filter(|d| d.text_box.is_some_and(|t| route.iter().any(|r| r.intersects(&t))))
                .count()
        };

        // The control: the same labels with no route are free to sit on it,
        // so the assertion below is not vacuous.
        let mut index = CollisionIndex::new(bounds);
        let without_route = place_visible_symbols(candidates(), &mut index);
        assert!(on_route(&without_route) > 0, "the label grid must cover the route for this to test anything");

        // As every shell does it: the retained index is `reset`, the route
        // seeded, then the shared plan runs -- which is where the phone
        // aborted.
        let seed = |index: &mut CollisionIndex| {
            index.reset(bounds);
            for route_box in route.iter().copied() {
                index.insert_obstacle(route_box);
            }
        };
        let mut index = CollisionIndex::new(bounds);
        let _ = place_visible_symbols(candidates(), &mut index); // last frame's placements
        seed(&mut index);
        let plan = crate::render::label_plan::label_draw_plan(candidates(), Vec::new(), &mut index);
        assert!(plan.symbols_placed > 0, "labels still draw during a drive");

        seed(&mut index);
        let with_route = place_visible_symbols(candidates(), &mut index);
        assert_eq!(with_route.len(), plan.symbols_placed, "the plan places what placement places");
        assert_eq!(on_route(&with_route), 0, "no placed label sits on the route");
    }
}

#[cfg(test)]
mod stacked_attribution_tests {
    use super::*;

    /// One character = one pixel, so the widths are the lengths.
    fn chars(text: &str) -> f32 {
        text.chars().count() as f32
    }

    #[test]
    fn a_credit_that_fits_is_one_line_unchanged() {
        assert_eq!(stacked_attribution_lines("A | B", 5.0, chars), vec!["A | B".to_string()]);
    }

    #[test]
    fn a_wide_credit_stacks_on_the_separator_greedily() {
        let lines = stacked_attribution_lines("aaaa | bb | cc | dddddd", 9.0, chars);
        assert_eq!(lines, vec!["aaaa | bb", "cc", "dddddd"]);
        assert!(lines.iter().all(|l| chars(l) <= 9.0));
    }

    #[test]
    fn a_credit_that_fits_a_line_is_never_broken_mid_credit() {
        assert_eq!(stacked_attribution_lines("ab cd | ef gh", 6.0, chars), vec!["ab cd", "ef gh"]);
    }

    #[test]
    fn a_single_credit_wider_than_a_line_breaks_at_its_spaces() {
        let lines = stacked_attribution_lines("x | aaa bbb ccc ddd | y", 7.0, chars);
        assert_eq!(lines, vec!["x", "aaa bbb", "ccc ddd", "y"]);
        assert!(lines.iter().all(|l| chars(l) <= 7.0));
        // Only a single word wider than the maximum can exceed it.
        assert_eq!(stacked_attribution_lines("abcdefghij | k", 4.0, chars), vec!["abcdefghij", "k"]);
    }
}

/// "Bus stops are visible through buildings": every kind of symbol whose
/// anchor a tower hides is not placed -- icon, route shield, street name,
/// house number, one-way arrow -- and the same symbol in front of the tower
/// is. On a live map the hide is a fade, not a pop.
#[cfg(test)]
mod building_occlusion_tests {
    use super::globe_tests::point_label;
    use super::*;
    use crate::render::extrusion::{BuildingOcclusion, OcclusionLayers, OcclusionMemory, OCCLUSION_FADE_SECONDS};
    use std::sync::Arc;

    const TILE: TileId = TileId { z: 14, x: 8192, y: 8192 };
    /// 300 m behind a 200 m tower, and in front of it (the eye is south).
    const BEHIND: (i32, i32) = (2048, 1500);
    const IN_FRONT: (i32, i32) = (2048, 3000);

    fn camera() -> Camera {
        Camera {
            center_lon: 0.011,
            center_lat: -0.011,
            zoom: 16.0,
            pitch: 60f32.to_radians(),
            bearing: 0.0,
            viewport_width_px: 1024.0,
            viewport_height_px: 1024.0,
            fov_y: Camera::DEFAULT_FOV_Y,
        }
    }

    fn tower() -> Arc<crate::render::extrusion::ExtrusionMesh> {
        let grey = style::Rgba { r: 0.5, g: 0.5, b: 0.5, a: 1.0 };
        let row = style::ExtrusionRow {
            color_from: grey,
            color_to: grey,
            base_from: 0.0,
            height_from: 200.0,
            base_to: 0.0,
            height_to: 200.0,
        };
        let ring = geo_types::LineString::from(vec![(1800, 2000), (2300, 2000), (2300, 2600), (1800, 2600), (1800, 2000)]);
        let geometry = Arc::new(geo_types::Geometry::Polygon(geo_types::Polygon::new(ring, vec![])));
        let plan = style::PaintPlan { ops: vec![style::PaintOp::Extrusion { group: 0, row, geometry }] };
        Arc::new(crate::render::extrusion::tessellate_extrusions(&plan))
    }

    fn streets() -> Option<style::Style> {
        Some(crate::style_spec::parse_and_compile(&crate::mapbox_fixtures::read("mapbox-streets-style.json")?).unwrap())
    }

    fn layer(style: &style::Style, id: &str) -> usize {
        style.layers.iter().position(|l| l.id() == id).unwrap_or_else(|| panic!("{id}"))
    }

    /// One symbol of each kind the bug report saw through a building, on
    /// Mapbox Streets' own layers, anchored at `at`.
    fn kinds(style: &style::Style, at: (i32, i32)) -> Vec<(&'static str, style::SymbolFeature)> {
        let anchor = geo_types::Point::new(at.0, at.1);
        let ahead = Some(geo_types::Point::new(at.0 + 64, at.1));
        let with = |id: &str, text: Option<&str>, icon: Option<&str>| style::SymbolFeature {
            text: text.map(str::to_string),
            icon: icon.map(str::to_string),
            layer_index: layer(style, id),
            ..point_label(anchor, "")
        };
        vec![
            ("icon (bus stop)", with("transit-label", None, Some("bus"))),
            ("shield", style::SymbolFeature { forward_ref: ahead, ..with("road-number-shield", Some("20"), Some("shield")) }),
            (
                "text (street name)",
                // By its forward reference alone: the glyph walk along a
                // `line` needs a glyph sink this test does not build, and
                // the occlusion gate runs before either.
                style::SymbolFeature { forward_ref: ahead, ..with("road-label", Some("Meiji-dori"), None) },
            ),
            ("house number", with("building-number-label", Some("12"), None)),
            ("one-way arrow", style::SymbolFeature { forward_ref: ahead, ..with("road-oneway-arrow-blue", None, Some("oneway")) }),
        ]
    }

    fn outcome(style: &style::Style, occlusion: &BuildingOcclusion, feature: &style::SymbolFeature) -> SymbolOutcome {
        let camera = camera();
        let mut icon_rects: HashMap<String, IconRect> = HashMap::new();
        for name in ["bus", "shield", "oneway"] {
            icon_rects.insert(name.to_string(), IconRect { x: 0, y: 0, width: 16, height: 16, pixel_ratio: 1.0, sdf: false });
        }
        let frame = SymbolFrame {
            occlusion: Some(occlusion),
            label_paint: None,
            camera: &camera,
            style_layers: &style.layers,
            scale_factor: 1.0,
            size_scale: 1.0,
            collision_padding: SYMBOL_COLLISION_PADDING_LOGICAL_PX,
            size_blend_t: 0.0,
            icon_rects: &icon_rects,
            icon_atlas_size: (64, 64),
            globe: crate::render::globe::GlobeAnchors::FLAT,
            light: crate::render::SymbolLight::UNLIT,
            fog: crate::render::HorizonFog::NONE,
        };
        let anchors = AnchorProjection::new(&camera, TILE, &frame.globe);
        let mut font_system = crate::font_fallback::new_font_system();
        let mut shape_cache = LabelShapeCache::new();
        basemap_candidate(&frame, &anchors, TILE.z, feature, &mut font_system, &mut shape_cache, None)
    }

    fn occlusion(style: &style::Style, memory: Option<OcclusionMemory>) -> BuildingOcclusion {
        let m2w = crate::render::WORLD_SIZE / 40_075_016.686;
        BuildingOcclusion::new(&camera(), m2w, vec![(TILE, tower())], &style.layers, OcclusionLayers::default(), memory)
    }

    #[test]
    fn every_symbol_kind_behind_a_tower_is_hidden_and_in_front_of_it_is_placed() {
        let style = or_skip!(streets());
        let occlusion = occlusion(&style, None);
        for ((kind, behind), (_, in_front)) in kinds(&style, BEHIND).into_iter().zip(kinds(&style, IN_FRONT)) {
            assert!(
                !matches!(outcome(&style, &occlusion, &in_front), SymbolOutcome::Skipped),
                "{kind}: in front of the tower it must be placed (the control)"
            );
            assert!(
                matches!(outcome(&style, &occlusion, &behind), SymbolOutcome::Skipped),
                "{kind}: behind the tower it must not be placed"
            );
        }
    }

    /// "Changi Road drawn on top of the bus": a 3D model's box hides every
    /// symbol kind under it and behind it, exactly -- one beside it or in
    /// front of it (between it and the eye) is placed. The bus stands
    /// across the view at the tile's middle, the eye due south.
    #[test]
    fn a_label_under_or_behind_a_3d_bus_is_hidden_and_one_beside_or_in_front_is_placed() {
        use crate::render::extrusion::ModelOccluder;
        let style = or_skip!(streets());
        let size = f64::from(crate::render::WORLD_SIZE) / f64::from(1u32 << TILE.z);
        let at = |x: i32, y: i32| glam::DVec2::new((f64::from(TILE.x) + f64::from(x) / 4096.0) * size, (f64::from(TILE.y) + f64::from(y) / 4096.0) * size);
        let m2w = f64::from(crate::render::WORLD_SIZE) / 40_075_016.686;
        // 12 m long east-west, 2.5 m wide, 0.3 to 3.3 m up (an MVT unit at
        // z14 is 0.6 m).
        let bus = ModelOccluder { center: at(2048, 2300), forward: glam::DVec2::X, half: glam::DVec2::new(6.0, 1.25) * m2w, z: (0.3 * m2w, 3.3 * m2w) };
        let occlusion = BuildingOcclusion::new(&camera(), m2w as f32, Vec::new(), &style.layers, OcclusionLayers::default(), None).with_model_occluders(vec![bus]);
        for (where_, anchor, hidden) in [
            ("under the bus", (2048, 2300), true),
            ("just behind it", (2048, 2296), true),
            ("behind its far end", (2056, 2296), true),
            ("in front of it", (2048, 2310), false),
            ("beside it", (2080, 2300), false),
            ("far behind it, the ray passing over its roof", (2048, 2200), false),
        ] {
            assert_eq!(occlusion.hides(TILE, [anchor.0, anchor.1]), hidden, "{where_}");
            for (kind, feature) in kinds(&style, anchor) {
                let skipped = matches!(outcome(&style, &occlusion, &feature), SymbolOutcome::Skipped);
                assert_eq!(skipped, hidden, "{kind} {where_}");
            }
        }
        // With no model, nothing is hidden (and nothing is tested).
        let none = BuildingOcclusion::new(&camera(), m2w as f32, Vec::new(), &style.layers, OcclusionLayers::default(), None);
        assert!(!none.hides(TILE, [2048, 2300]));
    }

    /// A live map does not pop a label out: once the hysteresis flips it,
    /// its opacity falls over `OCCLUSION_FADE_SECONDS` (a frame's step is
    /// at most 1/30 s), drawn at partial alpha until it reaches zero.
    #[test]
    fn a_label_that_goes_behind_a_tower_fades_out_over_several_frames() {
        let style = or_skip!(streets());
        let house = layer(&style, "building-number-label");
        let key = [BEHIND.0, BEHIND.1];
        // First sight behind the tower decides at once: no fade from 1.
        let first = occlusion(&style, Some(OcclusionMemory::default())).label_visibility(TILE, key, house);
        assert_eq!(first, 0.0, "a label first seen behind a tower starts hidden");
        // A label settled VISIBLE that the tower now hides (the camera moved).
        let mut memory = OcclusionMemory::default();
        memory.force_visible_for_test(TILE, key, house);
        let mut curve = Vec::new();
        for _ in 0..40 {
            memory.age_for_test();
            let o = occlusion(&style, Some(memory));
            curve.push(o.label_visibility(TILE, key, house));
            memory = o.into_memory();
            if *curve.last().unwrap() == 0.0 {
                break;
            }
        }
        let fading = curve.iter().filter(|&&o| o > 0.0 && o < 1.0).count();
        assert!(fading + 1 >= (OCCLUSION_FADE_SECONDS * 30.0).round() as usize, "faded over frames, not popped: {curve:?}");
        assert!(curve.windows(2).all(|w| w[1] <= w[0]), "monotonic: {curve:?}");
        assert_eq!(*curve.last().unwrap(), 0.0, "and reaches hidden: {curve:?}");
        assert!(!memory.pending(), "settled hidden needs no more frames");
        // Mid-fade the symbol is placed with its opacity scaled, not skipped.
        let mut memory = OcclusionMemory::default();
        memory.force_visible_for_test(TILE, key, house);
        let feature = style::SymbolFeature { layer_index: house, ..point_label(geo_types::Point::new(BEHIND.0, BEHIND.1), "12") };
        let mut alphas = Vec::new();
        for _ in 0..40 {
            memory.age_for_test();
            let o = occlusion(&style, Some(memory));
            match outcome(&style, &o, &feature) {
                SymbolOutcome::Candidate(c) => alphas.push(c.text.map_or(-1.0, |t| t.3.a)),
                _ => break,
            }
            memory = o.into_memory();
        }
        assert!(alphas.iter().any(|&a| a > 0.0 && a < 1.0), "a fading label draws at partial alpha: {alphas:?}");
    }
}

/// The opaque white boxes over Tokyo intersection names ("Shinjuku 4",
/// "Nukibenten"): Mapbox Streets' `road-intersection` layer puts its name
/// on the `intersection` sprite -- a plain white 26x14-point rectangle with
/// no stretch metadata -- under `icon-text-fit: both`, padding
/// `[1, 2, 1, 2]`. Mapbox stretches the plate to the label
/// (`fitIconToText`); drawn at its native size it covered the middle of a
/// long name and nothing of the ends.
#[cfg(test)]
mod icon_text_fit_tests {
    use super::globe_tests::point_label;
    use super::*;

    #[test]
    fn an_intersection_plate_is_stretched_to_wrap_its_whole_name() {
        let style =
            crate::style_spec::parse_and_compile(&mapbox_fixture_or_skip!("mapbox-streets-style.json")).unwrap();
        let layer = style.layers.iter().position(|l| l.id() == "road-intersection").unwrap();
        let style::StyleLayer::Symbol { icon_text_fit, .. } = &style.layers[layer] else { unreachable!() };
        assert_eq!(icon_text_fit.fit, crate::overlay::paint::TextFit::Both, "the layer's icon-text-fit compiles");
        assert_eq!(icon_text_fit.padding, [1.0, 2.0, 1.0, 2.0]);
        let camera = Camera {
            center_lon: 0.011,
            center_lat: -0.011,
            zoom: 16.0,
            pitch: 0.0,
            bearing: 0.0,
            viewport_width_px: 1024.0,
            viewport_height_px: 1024.0,
            fov_y: Camera::DEFAULT_FOV_Y,
        };
        let mut icon_rects: HashMap<String, IconRect> = HashMap::new();
        // The real sprite@2x entry: 52x28 texels at pixelRatio 2.
        icon_rects.insert("intersection".into(), IconRect { x: 0, y: 0, width: 52, height: 28, pixel_ratio: 2.0, sdf: false });
        for scale_factor in [1.0f32, 2.0] {
            let frame = SymbolFrame {
                occlusion: None,
                label_paint: None,
                camera: &camera,
                style_layers: &style.layers,
                scale_factor,
                size_scale: scale_factor,
                collision_padding: SYMBOL_COLLISION_PADDING_LOGICAL_PX,
                size_blend_t: 0.0,
                icon_rects: &icon_rects,
                icon_atlas_size: (64, 64),
                globe: crate::render::globe::GlobeAnchors::FLAT,
                light: crate::render::SymbolLight::UNLIT,
                fog: crate::render::HorizonFog::NONE,
            };
            let tile = TileId { z: 14, x: 8192, y: 8192 };
            let anchors = AnchorProjection::new(&camera, tile, &frame.globe);
            let feature = style::SymbolFeature {
                icon: Some("intersection".into()),
                layer_index: layer,
                text_size: 12.0,
                ..point_label(geo_types::Point::new(2048, 2048), "Shinjuku 4-chome Minami")
            };
            let mut font_system = crate::font_fallback::new_font_system();
            let mut shape_cache = LabelShapeCache::new();
            let SymbolOutcome::Candidate(c) =
                basemap_candidate(&frame, &anchors, tile.z, &feature, &mut font_system, &mut shape_cache, None)
            else {
                panic!("the intersection name is placed")
            };
            let text = c.text_box.expect("a text box");
            let (left, top, right, bottom) = c.icons.iter().fold((f32::MAX, f32::MAX, f32::MIN, f32::MIN), |b, i| {
                (b.0.min(i.screen_x), b.1.min(i.screen_y), b.2.max(i.screen_x + i.width), b.3.max(i.screen_y + i.height))
            });
            assert!(!c.icons.is_empty(), "the plate draws");
            // `text_collision_box` pads nothing; the plate is the text's
            // box plus [top, right, bottom, left] = [1, 2, 1, 2] points.
            let (s, eps) = (scale_factor, 0.01);
            assert!((left - (text.left - 2.0 * s)).abs() < eps, "@{s}x left {left} vs text {}", text.left);
            assert!((right - (text.right + 2.0 * s)).abs() < eps, "@{s}x right {right} vs text {}", text.right);
            assert!((top - (text.top - 1.0 * s)).abs() < eps, "@{s}x top {top} vs text {}", text.top);
            assert!((bottom - (text.bottom + 1.0 * s)).abs() < eps, "@{s}x bottom {bottom} vs text {}", text.bottom);
            // The collision box is the plate's, so a neighbour cannot sit
            // on the part of it the native-size box left out.
            let icon_box = c.icon_box.expect("an icon box");
            assert!(icon_box.left <= left + eps && icon_box.right >= right - eps, "@{s}x {icon_box:?}");
        }
    }

    /// A layer without `icon-text-fit` keeps its icon at the sprite's own
    /// size, centred on the anchor -- unchanged.
    #[test]
    fn a_layer_without_icon_text_fit_draws_its_icon_at_native_size() {
        let style =
            crate::style_spec::parse_and_compile(&mapbox_fixture_or_skip!("mapbox-streets-style.json")).unwrap();
        let layer = style.layers.iter().position(|l| l.id() == "poi-label").unwrap();
        let style::StyleLayer::Symbol { icon_text_fit, .. } = &style.layers[layer] else { unreachable!() };
        assert_eq!(*icon_text_fit, style::IconTextFit::default());
    }
}

/// Where an overlay feature's label and icon hang ON SCREEN (logical
/// pixels): [`overlay_label_anchor`] through this frame's
/// `render::globe::WorldProjector` -- the SAME
/// projector the donut clusters and the navigation puck use, so a count
/// label lands on its donut's centre. `None` for a feature with no anchor,
/// or one whose anchor the planet (or the near plane) hides.
///
/// On a flat frame the projector is the flat f64 matrix, bit for bit what
/// this path always did. On the globe it used to be that same FLAT matrix
/// too: every overlay label on a z3-z11 globe was placed where the point
/// would sit on the Mercator plane, not on the sphere, so labels drifted
/// off their features towards the limb, and far-side labels -- never
/// tested against the horizon -- floated in space off the planet's edge.
pub(crate) fn overlay_symbol_screen_anchor(
    projector: &crate::render::globe::WorldProjector<'_>,
    geometry: &crate::overlay::geojson::OverlayGeometry,
) -> Option<(f32, f32)> {
    let (lon, lat) = overlay_label_anchor(geometry)?;
    projector.project(crate::overlay::geom::lon_lat_to_world(lon, lat))
}

/// Where an overlay feature's label and icon hang from, in `(lon, lat)`.
///
/// A point anchors on itself. A line anchors at the middle of its LONGEST
/// part, measured in lon/lat, so a route's number sits on the route rather
/// than at whichever end happened to be first in the array. A polygon
/// anchors at the centre of its exterior ring's extent -- the centre of its
/// bounding box -- which is cheap and adequate for a convex-ish service
/// area; a true pole of inaccessibility is deferred with the rest of the
/// label placement work.
///
/// The polygon arm is deliberately NOT the ring's first-and-last midpoint,
/// which is what a line's midpoint rule would give it. GeoJSON requires a
/// closed ring and the parser keeps the closing position, so for a ring
/// `first == last` and that midpoint is exactly the first vertex -- i.e.
/// whichever CORNER the author happened to write first, which put a service
/// area's name outside the area about as often as inside it.
pub(crate) fn overlay_label_anchor(geometry: &crate::overlay::geojson::OverlayGeometry) -> Option<(f64, f64)> {
    use crate::overlay::geojson::OverlayGeometry;
    let segment_length = |a: (f64, f64), b: (f64, f64)| -> f64 {
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        (dx * dx + dy * dy).sqrt()
    };
    // The point half way ALONG the path, not the midpoint of the chord
    // between its first and last vertices. For any line that bends those
    // are different points, and the chord midpoint is not on the line at
    // all: the example's AYE expressway bends southward, so its chord
    // midpoint sat about 330 m north of the road -- roughly 35 px at zoom
    // 13, which is exactly how far off its label was reported to be.
    let point_at_half_length = |points: &[(f64, f64)]| -> Option<(f64, f64)> {
        let total: f64 = points.windows(2).map(|pair| segment_length(pair[0], pair[1])).sum();
        // A path of zero length is every vertex in the same place, so any
        // of them is the answer.
        if total <= 0.0 {
            return points.first().copied();
        }
        let half = total / 2.0;
        let mut travelled = 0.0;
        for pair in points.windows(2) {
            let segment = segment_length(pair[0], pair[1]);
            if travelled + segment >= half {
                let t = (half - travelled) / segment;
                return Some((
                    pair[0].0 + (pair[1].0 - pair[0].0) * t,
                    pair[0].1 + (pair[1].1 - pair[0].1) * t,
                ));
            }
            travelled += segment;
        }
        // Unreachable while `total > 0`: the running sum reaches `half` at
        // or before the final segment. Answering with the last vertex
        // rather than `None` keeps a floating-point edge case from
        // silently dropping a label.
        points.last().copied()
    };
    let bounding_box_center = |points: &[(f64, f64)]| -> Option<(f64, f64)> {
        let (first_x, first_y) = *points.first()?;
        let (mut min_x, mut max_x, mut min_y, mut max_y) = (first_x, first_x, first_y, first_y);
        for &(x, y) in points {
            min_x = min_x.min(x);
            max_x = max_x.max(x);
            min_y = min_y.min(y);
            max_y = max_y.max(y);
        }
        Some(((min_x + max_x) / 2.0, (min_y + max_y) / 2.0))
    };
    let length = |points: &[(f64, f64)]| -> f64 {
        points
            .windows(2)
            .map(|pair| {
                let (dx, dy) = (pair[1].0 - pair[0].0, pair[1].1 - pair[0].1);
                (dx * dx + dy * dy).sqrt()
            })
            .sum()
    };
    match geometry {
        OverlayGeometry::Empty => None,
        OverlayGeometry::Point(points) => points.first().copied(),
        OverlayGeometry::Line(parts) => parts
            .iter()
            .filter(|part| part.len() >= 2)
            .max_by(|a, b| length(a).total_cmp(&length(b)))
            .and_then(|part| point_at_half_length(part)),
        OverlayGeometry::Polygon(polygons) => {
            polygons.first().and_then(|rings| rings.first()).and_then(|ring| bounding_box_center(ring))
        }
        // A tiled feature whose lon/lat now lives in the tile index: the
        // centre of the box it kept (the polygon arm's answer for a
        // one-polygon feature; a line's box centre rather than its
        // half-length point).
        OverlayGeometry::Indexed { bbox_lon_lat: (w, s, e, n), .. } => Some(((w + e) / 2.0, (s + n) / 2.0)),
    }
}

/// What one frame's overlay symbol pass reads besides the features:
/// [`collect_overlay_candidates`]'s inputs, shared by `MapView::paint` and
/// the browser shell's label pass so overlay text is placed, and collides
/// with the basemap's labels, the same way on both.
pub(crate) struct OverlaySymbolFrame<'a> {
    /// This frame's projector (flat or globe), as the basemap's labels use.
    pub(crate) projector: &'a crate::render::globe::WorldProjector<'a>,
    pub(crate) zoom: f64,
    /// The viewport in LOGICAL pixels: the cull rectangle, grown by
    /// [`SYMBOL_CULL_MARGIN_PX`].
    pub(crate) viewport: (f32, f32),
    pub(crate) scale_factor: f32,
    /// Physical pixels, as `SymbolFrame::collision_padding`.
    pub(crate) collision_padding: f32,
    pub(crate) icon_rects: &'a HashMap<String, IconRect>,
    pub(crate) icon_atlas_size: (u32, u32),
    pub(crate) icon_geometry: &'a HashMap<String, crate::sprite::IconGeometry>,
}

/// The overlay's point-anchored symbols (a feature's `text-field` and
/// `icon-image`) as placement candidates, each with its sort key: features
/// with neither, under their `min_zoom`, or off screen are skipped; a
/// feature that allows overlap (either flag) goes straight to `bypassed`,
/// icon and label together. Every candidate takes the overlay's layer
/// index ([`crate::render::label_plan::OVERLAY_LAYER_INDEX`]): placed
/// before every basemap label, so it wins, and drawn after them. Returns
/// how many features offered a symbol.
pub(crate) fn collect_overlay_candidates<'f>(
    frame: &OverlaySymbolFrame<'_>,
    features: impl IntoIterator<Item = (f32, &'f crate::overlay::OverlayFeature)>,
    font_system: &mut glyphon::FontSystem,
    shape_cache: &mut LabelShapeCache,
    candidates: &mut Vec<SymbolCandidate>,
    bypassed: &mut Vec<DrawnSymbol>,
) -> usize {
    let layer_index = crate::render::label_plan::OVERLAY_LAYER_INDEX;
    let mut offered = 0;
    for (sort_key, feature) in features {
        if feature.paint.text_field.is_none() && feature.paint.icon_image.is_none() {
            continue;
        }
        // `min_zoom`: skipped before the shaping, culling and collision, so
        // a zoomed-out view of hundreds of these costs nothing per label.
        if !symbol_visible_at_zoom(feature.paint.min_zoom, frame.zoom) {
            continue;
        }
        let Some((screen_x, screen_y)) = overlay_symbol_screen_anchor(frame.projector, &feature.geometry) else {
            continue;
        };
        if screen_x < -SYMBOL_CULL_MARGIN_PX
            || screen_y < -SYMBOL_CULL_MARGIN_PX
            || screen_x > frame.viewport.0 + SYMBOL_CULL_MARGIN_PX
            || screen_y > frame.viewport.1 + SYMBOL_CULL_MARGIN_PX
        {
            continue;
        }
        offered += 1;
        let OverlaySymbolParts { icons, icon_box, text, text_box } = overlay_symbol_parts(
            &feature.paint,
            screen_x,
            screen_y,
            frame.scale_factor,
            frame.icon_rects,
            frame.icon_atlas_size,
            frame.icon_geometry,
            font_system,
            shape_cache,
        );
        // A pin and its label are ONE symbol: either flag bypasses both
        // halves (`overlay_bypasses_collision`'s reasoning), so a bypassed
        // icon never stands with its label culled.
        if overlay_bypasses_collision(feature.paint.icon_allow_overlap, feature.paint.text_allow_overlap) {
            bypassed.push(DrawnSymbol {
                order: (layer_index, sort_key),
                icons,
                icon_box,
                text,
                // An overlay anchor is a point: never line-placed glyphs.
                glyphs: None,
                text_box,
                text_clip: None,
                text_clip_behind_icons: feature.paint.text_clip_behind_icons,
            });
            continue;
        }
        // A feature whose icon this sheet lacks and which has no text takes
        // no placement slot.
        let Some(bounds) = crate::collision::symbol_box(text_box, icon_box, frame.collision_padding) else {
            continue;
        };
        candidates.push(SymbolCandidate {
            text,
            glyphs: None,
            icon_box,
            text_box,
            icons,
            text_clip_behind_icons: feature.paint.text_clip_behind_icons,
            placement: PlacementCandidate { layer_index, sort_key, bounds },
            // A consumer's own data: two features with one string are two
            // features. See `SymbolCandidate::dedupe_anchor`.
            dedupe_anchor: None,
        });
    }
    offered
}

#[cfg(test)]
mod overlay_candidate_tests {
    use super::globe_tests::{candidates_for, point_label};
    use super::*;
    use crate::render::globe::{GlobeAnchors, WorldProjector};

    /// A flat camera centred on `tile`'s middle, at its zoom, and that
    /// middle's lon/lat: where a basemap label anchored at the tile's centre
    /// and an overlay point at the lon/lat land on the same pixel.
    fn centred(tile: TileId) -> (Camera, (f64, f64)) {
        let n = 2f64.powi(i32::from(tile.z));
        let lon = (f64::from(tile.x) + 0.5) / n * 360.0 - 180.0;
        let lat = (std::f64::consts::PI * (1.0 - 2.0 * (f64::from(tile.y) + 0.5) / n)).sinh().atan().to_degrees();
        let camera = Camera {
            center_lon: lon,
            center_lat: lat,
            zoom: f64::from(tile.z),
            pitch: 0.0,
            bearing: 0.0,
            viewport_width_px: 800.0,
            viewport_height_px: 600.0,
            fov_y: Camera::DEFAULT_FOV_Y,
        };
        (camera, (lon, lat))
    }

    fn overlay_point(index: usize, lon: f64, lat: f64, text: &str) -> crate::overlay::OverlayFeature {
        let paint = crate::overlay::FeaturePaint { text_field: Some(text.to_string()), ..Default::default() };
        crate::overlay::OverlayFeature {
            index,
            id: None,
            geometry: crate::overlay::geojson::OverlayGeometry::Point(vec![(lon, lat)]),
            paint,
        }
    }

    /// The overlay half of a frame, as `MapView::paint` and the browser
    /// shell's label pass both run it.
    fn overlay(camera: &Camera, features: &[crate::overlay::OverlayFeature]) -> (usize, Vec<SymbolCandidate>, Vec<DrawnSymbol>) {
        let mut font_system = crate::font_fallback::new_font_system();
        let mut shape_cache = LabelShapeCache::new();
        let icon_rects: HashMap<String, IconRect> = HashMap::new();
        let icon_geometry: HashMap<String, crate::sprite::IconGeometry> = HashMap::new();
        let projector = WorldProjector::new(camera, &GlobeAnchors::FLAT);
        let frame = OverlaySymbolFrame {
            projector: &projector,
            zoom: camera.zoom,
            viewport: (camera.viewport_width_px, camera.viewport_height_px),
            scale_factor: 1.0,
            collision_padding: SYMBOL_COLLISION_PADDING_LOGICAL_PX,
            icon_rects: &icon_rects,
            icon_atlas_size: (0, 0),
            icon_geometry: &icon_geometry,
        };
        let (mut candidates, mut bypassed) = (Vec::new(), Vec::new());
        let offered = collect_overlay_candidates(
            &frame,
            features.iter().map(|f| (f.index as f32, f)),
            &mut font_system,
            &mut shape_cache,
            &mut candidates,
            &mut bypassed,
        );
        (offered, candidates, bypassed)
    }

    fn index(camera: &Camera) -> CollisionIndex {
        CollisionIndex::new(symbol_collision_bounds(camera.viewport_width_px, camera.viewport_height_px, 1.0))
    }

    /// The web label pass's bug: overlay text was never offered, so a
    /// station name never drew there. Offered, it wins the spot a basemap
    /// label wanted (placed first) and draws over the basemap's (sorted
    /// last), as on native.
    #[test]
    fn an_overlay_label_wins_against_the_basemap_label_on_its_spot() {
        let tile = TileId { z: 14, x: 14552, y: 6451 };
        let (camera, (lon, lat)) = centred(tile);
        let centre = geo_types::Point::new(MVT_EXTENT as i32 / 2, MVT_EXTENT as i32 / 2);
        let mut candidates = candidates_for(&camera, GlobeAnchors::FLAT, tile, vec![point_label(centre, "Basemap place")], crate::render::HorizonFog::NONE);
        assert_eq!(candidates.len(), 1, "the basemap label is a candidate on its own");
        let (offered, overlay, bypassed) = overlay(&camera, &[overlay_point(0, lon, lat, "Shinjuku")]);
        assert_eq!((offered, overlay.len(), bypassed.len()), (1, 1, 0));
        let ob = overlay[0].placement.bounds;
        let bb = candidates[0].placement.bounds;
        assert!(ob.left < bb.right && bb.left < ob.right && ob.top < bb.bottom && bb.top < ob.bottom, "the fixture must overlap: {ob:?} vs {bb:?}");
        candidates.extend(overlay);
        let placed = place_visible_symbols(candidates, &mut index(&camera));
        assert_eq!(placed.len(), 1, "the two overlap: one of them goes");
        assert_eq!(placed[0].order.0, crate::render::label_plan::OVERLAY_LAYER_INDEX, "and the overlay's is the one kept");
    }

    /// Two overlay labels on one spot: the lower sort key places, the other
    /// is dropped -- overlay text collides with itself too.
    #[test]
    fn overlapping_overlay_labels_keep_one() {
        let tile = TileId { z: 14, x: 14552, y: 6451 };
        let (camera, (lon, lat)) = centred(tile);
        let (_, candidates, _) = overlay(&camera, &[overlay_point(0, lon, lat, "Shinjuku"), overlay_point(1, lon, lat, "Shinjuku-sanchome")]);
        assert_eq!(candidates.len(), 2);
        assert_eq!(place_visible_symbols(candidates, &mut index(&camera)).len(), 1);
    }

    /// `min-zoom` above the camera, a feature off screen, and one with no
    /// text or icon offer nothing; `text-allow-overlap` bypasses placement.
    #[test]
    fn the_gate_the_cull_and_the_bypass() {
        let tile = TileId { z: 14, x: 14552, y: 6451 };
        let (camera, (lon, lat)) = centred(tile);
        let mut gated = overlay_point(0, lon, lat, "Gated");
        gated.paint.min_zoom = Some(15.0);
        let far = overlay_point(1, lon + 1.0, lat, "Far away");
        let mut blank = overlay_point(2, lon, lat, "");
        blank.paint.text_field = None;
        let (offered, candidates, bypassed) = overlay(&camera, &[gated, far, blank]);
        assert_eq!((offered, candidates.len(), bypassed.len()), (0, 0, 0));
        let mut over = overlay_point(3, lon, lat, "Always");
        over.paint.text_allow_overlap = true;
        let (offered, candidates, bypassed) = overlay(&camera, &[over]);
        assert_eq!((offered, candidates.len(), bypassed.len()), (1, 0, 1));
        assert!(bypassed[0].text.is_some(), "the bypassed symbol keeps its label");
    }
}

#[cfg(test)]
mod eye_fog_label_tests {
    use super::*;
    use crate::render::globe::{globe_frame, GlobeAnchors};

    /// **Labels fade with an eye camera's fog below z12** (the Digital
    /// twin from 6-12 km up: the projection blends toward the sphere
    /// there, and the place names used to float crisp over the fogged
    /// ground). On a blended frame the label takes the flat clip `w` the
    /// shaders fade the ground by: clear near, fading in the band, gone
    /// past its end; and a frame with the fade off is untouched.
    #[test]
    fn a_label_fades_with_the_eye_fog_on_a_blended_globe_frame() {
        let fov = 46f64.to_radians();
        let t = crate::camera_control::eye_target_fov((-73.99, 40.747), 10_000.0, 0.35, 75f32.to_radians(), 887.0, fov);
        let (lon, lat) = t.center.unwrap();
        let camera = Camera { center_lon: lon, center_lat: lat, zoom: t.zoom.unwrap(), pitch: t.pitch.unwrap(), bearing: t.bearing.unwrap(), viewport_width_px: 1401.0, viewport_height_px: 887.0, fov_y: fov };
        assert!(crate::render::globe::transition_for_zoom(camera.zoom) > 0.0, "z{} is a blended frame", camera.zoom);
        let globe = GlobeAnchors::new(&globe_frame(&camera));
        let z = camera.zoom.floor() as u8;
        let (x, y) = crate::geo::lon_lat_to_tile_xy(lon, lat, f64::from(z));
        let tile = TileId { z, x: x as u32, y: y as u32 };
        let anchors = AnchorProjection::new(&camera, tile, &globe);
        assert!(anchors.globe.is_some(), "the anchors take the globe path");
        let anchor = geo_types::Point::new((MVT_EXTENT / 2) as i32, (MVT_EXTENT / 2) as i32);
        let depth = anchors.depth(anchor);
        let colour = [1.0, 1.0, 1.0, 1.0];
        let fog = |start: f32, end: f32| crate::render::HorizonFog { start, end, color: colour };
        assert_eq!(horizon_visibility(crate::render::HorizonFog::NONE, &anchors, anchor), Some(1.0));
        assert_eq!(horizon_visibility(fog(depth * 2.0, depth * 4.0), &anchors, anchor), Some(1.0), "nearer than the band: clear");
        let mid = horizon_visibility(fog(depth * 0.5, depth * 1.5), &anchors, anchor).expect("inside the band");
        assert!((mid - 0.5).abs() < 1e-3, "half way through the band: {mid}");
        assert_eq!(horizon_visibility(fog(depth * 0.25, depth * 0.5), &anchors, anchor), None, "past the fog: not placed");
    }
}

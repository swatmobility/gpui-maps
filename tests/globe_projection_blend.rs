//! **The projection blend must not move the planet.**
//!
//! `shader.wgsl`'s `interpolate_projection` mixes the clip-space image of a
//! vertex under the sphere with its image under Mercator, weighted by
//! `render::globe::transition_for_zoom(camera.zoom)`. A mix of two pictures
//! is only harmless where the two pictures are already in the same place —
//! and the zoom range over which the blend runs is the only thing that
//! decides whether they are.
//!
//! # The defect this file exists to keep out
//!
//! The blend used to run over z3–z6. At those zooms the sphere and Mercator
//! do not merely differ, they differ by **screen-widths**: at a lat-60
//! camera at z3.63 an ordinary ground vertex measured 36 px horizontally
//! and 38 px vertically from where the sphere puts it, and the pole cap —
//! whose apex vertices all share one sphere position but sit at every
//! longitude on the Mercator plane — was fanned from a point into a 158 px
//! arc. What the user saw was a lens, a gore, an egg: map content sliding
//! off a planet whose silhouette — drawn by the ray-marched atmosphere,
//! which never blends — stayed a correct circle. The measured symptom was
//! the covered centroid sliding 190 px at a **fixed** camera as the
//! transition fell below 1.0, with the silhouette's moment-fit fill ratio
//! falling from 0.991 to 0.935.
//!
//! MapLibre blends over z11–z12 (`projection_factory.ts:40-50` expands the
//! style value `"globe"` to `['interpolate', ['linear'], ['zoom'], 11,
//! 'vertical-perspective', 12, 'mercator']`), where the largest gap anywhere
//! in a 2048 px frame is about three pixels.
//!
//! # Why this test is out here and not in `src/render/globe.rs`
//!
//! So that putting the band back fails it, with nothing else touched.
//! Everything here is public API, and the zoom for each transition is found
//! by **bisecting `transition_for_zoom` itself** rather than read off the
//! band constants — so setting `PROJECTION_BLEND_ZOOM_MIN`/`_MAX` back to
//! 3 and 6 in `render::globe`, and changing not one other line, moves the
//! sampled cameras back down to z3–z4 and fails the assertions below by two
//! to three orders of magnitude (111 px against 8, 19 px against 1). A test
//! that read the constants would instead follow them wherever they went and
//! grade nothing.
//!
//! # Why 2048 px, and why a camera off the equator
//!
//! Every pixel test in this crate renders at 161 px, which is how a 190 px
//! drift survived the suite. And at `center_lat = 0` the drift is
//! antisymmetric about the frame centre, so the **centroid** of a symmetric
//! sample is stationary however broken the blend is — the centroid statement
//! only has teeth away from the equator. Both assertions are made at
//! lat 60, which is also where the user's own frames were.

use gpui_maps::geo::TileId;
use gpui_maps::render::globe::{globe_frame, transition_for_zoom, GlobeAnchors};
use gpui_maps::render::{combined_tile_matrix, tile_model_matrix, Camera, WORLD_SIZE};

/// The frame the assertions below are made at. See the module docs.
const FRAME_PX: f64 = 2048.0;

/// The transitions graded, spanning the whole band.
const TRANSITIONS: [f64; 5] = [1.0, 0.95, 0.9, 0.8, 0.5];

/// Worst single-vertex drift allowed, in pixels of a 2048 px frame.
///
/// The blend's own curvature term is real and cannot be zero for
/// `0 < t < 1`: it is the sphere's sagitta across the frame, weighted by
/// `1 - t`. At the reference's band it measures 0.72 px at t = 0.95 and
/// 5.26 px at t = 0.5 for this camera, so 8 px is a ceiling with room in
/// it rather than a transcription of today's numbers. The same measurement
/// at the old z3–z6 band is 111 px and about 970 px.
const MAX_VERTEX_DRIFT_PX: f64 = 8.0;

/// Worst drift of the sampled centroid — the statement the user's own
/// measurement was phrased in ("the covered centroid slides 190 px at a
/// fixed camera"). Measures 0.02 px at the reference's band, 19 px to
/// 113 px at the old one.
const MAX_CENTROID_DRIFT_PX: f64 = 1.0;

/// The zoom at which `transition_for_zoom` takes the value `t`, found by
/// bisection so this file never reads the band's constants. Returns `None`
/// for a `t` the function does not take.
fn zoom_for_transition(t: f64) -> Option<f64> {
    if (t - 1.0).abs() < f64::EPSILON {
        // The band's low end is a plateau, not a point: pick the zoom where
        // the function is still exactly 1.0 and about to leave it.
        let mut lo = 0.0_f64;
        let mut hi = 24.0_f64;
        for _ in 0..80 {
            let mid = 0.5 * (lo + hi);
            if f64::from(transition_for_zoom(mid)) >= 1.0 {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        return Some(lo);
    }
    let mut lo = 0.0_f64;
    let mut hi = 24.0_f64;
    for _ in 0..80 {
        let mid = 0.5 * (lo + hi);
        if f64::from(transition_for_zoom(mid)) > t {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let z = 0.5 * (lo + hi);
    ((f64::from(transition_for_zoom(z)) - t).abs() < 1e-4).then_some(z)
}

/// Clip space to framebuffer pixels, the rasterizer's own two lines: NDC `x`
/// runs left to right, NDC `y` runs **up** and the framebuffer's runs down.
fn to_screen(clip: glam::DVec4) -> Option<(f64, f64)> {
    (clip.w > 0.0).then(|| {
        (
            (clip.x / clip.w + 1.0) * 0.5 * FRAME_PX,
            (1.0 - clip.y / clip.w) * 0.5 * FRAME_PX,
        )
    })
}

#[test]
fn the_projection_blend_leaves_every_on_screen_vertex_where_the_sphere_put_it() {
    let extent = 4096.0_f32;

    for t in TRANSITIONS {
        let zoom = zoom_for_transition(t)
            .unwrap_or_else(|| panic!("transition_for_zoom never takes the value {t}"));
        let camera = Camera {
            center_lon: 90.0,
            center_lat: 60.0,
            zoom,
            pitch: 0.0,
            bearing: 0.0,
            viewport_width_px: FRAME_PX as f32,
            viewport_height_px: FRAME_PX as f32,
            fov_y: Camera::DEFAULT_FOV_Y,
        };
        let frame = globe_frame(&camera);
        let globe = GlobeAnchors::new(&frame);

        // **Sampled tile by tile at the camera's own level, not as one z0
        // tile, and the reason is f32.** The shader's flat arm is
        // `combined_tile_matrix` narrowed to f32 and multiplied by a lyon
        // coordinate that reaches `extent * COORD_SCALE` -- four million.
        // For a tile at the camera's level those two cancel inside the
        // tile, and the residue is 1e-5 px; for the z0 tile at z11 they do
        // not, and the residue is tens of pixels of pure narrowing noise,
        // which would be measured here as drift. Tiles at the camera's own
        // level are also what actually draws.
        let level = zoom.floor() as u8;
        let tiles_across = f64::from(1u32 << level);
        let (centre_x, centre_y) =
            gpui_maps::geo::lon_lat_to_tile_xy(camera.center_lon, camera.center_lat, 0.0);
        // Half the frame, in tiles of this level, plus one so the ring of
        // tiles the frame's edge lands in is included.
        let half_tiles = (FRAME_PX / 512.0) + 1.0;

        let per_tile = 12;
        let mut worst_drift = 0.0_f64;
        let mut worst_at = (0.0, 0.0);
        let mut sum_globe = (0.0_f64, 0.0_f64);
        let mut sum_blend = (0.0_f64, 0.0_f64);
        let mut sampled = 0_u32;

        let tx0 = ((centre_x * tiles_across - half_tiles).floor() as i64).max(0);
        let tx1 = ((centre_x * tiles_across + half_tiles).ceil() as i64)
            .min(tiles_across as i64 - 1);
        let ty0 = ((centre_y * tiles_across - half_tiles).floor() as i64).max(0);
        let ty1 = ((centre_y * tiles_across + half_tiles).ceil() as i64)
            .min(tiles_across as i64 - 1);

        for ty in ty0..=ty1 {
            for tx in tx0..=tx1 {
                let tile = TileId { z: level, x: tx as u32, y: ty as u32 };
                let anchors = globe
                    .for_tile(tile, 1.0)
                    .expect("a transition above zero is a globe frame");
                // THE SHADER'S OWN `model.matrix`, f32 narrowing included.
                let combined = combined_tile_matrix(&camera, tile, extent);
                // Lyon units per `[0, 1]` in-tile unit, read out of
                // production's own tile matrix rather than reconstructed
                // from `COORD_SCALE`, which is crate-private.
                //
                // From the matrix's x AXIS, not from the difference of two
                // transformed points: the scale is 3e-8 and the tile's world
                // origin is up to 256, so in f32 `origin + scale == origin`
                // and the difference is a hard zero.
                let model = tile_model_matrix(tile, extent);
                let world_per_lyon = f64::from(model.x_axis.x);
                let tile_world_size = f64::from(WORLD_SIZE) / tiles_across;
                let lyon_per_tile_unit = tile_world_size / world_per_lyon;

                for i in 0..=per_tile {
                    for j in 0..=per_tile {
                        let u = f64::from(i) / f64::from(per_tile);
                        let v = f64::from(j) / f64::from(per_tile);

                        // `TileAnchors::sphere` takes the anchor in the
                        // tile's own units, and `for_tile` was given an
                        // extent of 1.0, so `[0, 1]` in-tile IS the anchor.
                        let sphere = anchors.sphere((u, v));
                        // Where the sphere alone puts it: the pure-globe
                        // arm, which is what the frame draws at transition
                        // 1.0 and what the atmosphere's ray march draws at
                        // every transition.
                        let Some(globe_px) =
                            to_screen(frame.globe_view_proj * sphere.extend(1.0))
                        else {
                            continue;
                        };
                        if globe_px.0 < 0.0
                            || globe_px.0 > FRAME_PX
                            || globe_px.1 < 0.0
                            || globe_px.1 > FRAME_PX
                            || anchors.occluded(sphere)
                        {
                            continue;
                        }

                        let lyon = glam::Vec4::new(
                            (u * lyon_per_tile_unit) as f32,
                            (v * lyon_per_tile_unit) as f32,
                            0.0,
                            1.0,
                        );
                        let flat_clip = combined * lyon;
                        let blended = anchors.project(
                            glam::DVec4::new(
                                // NEGATED: `shader.wgsl`'s `clip.x = -clip.x`,
                                // which undoes `camera::eye_target_up`'s basis
                                // and applies to the MERCATOR arm only --
                                // `globe_frame` builds its own basis and
                                // already puts east at +x. Production's CPU
                                // twin does the same (`symbol_pass`'s
                                // `AnchorProjection::screen`); without it the
                                // flat arm is mirrored and the blend measures
                                // a reflection rather than a drift.
                                -f64::from(flat_clip.x),
                                f64::from(flat_clip.y),
                                f64::from(flat_clip.z),
                                f64::from(flat_clip.w),
                            ),
                            sphere,
                        );
                        let Some(blend_px) = to_screen(blended) else {
                            panic!(
                                "the blend put a front-facing vertex behind the eye at \
                                 transition {t} (zoom {zoom}), tile {tile:?} at ({u}, {v})"
                            );
                        };

                        let drift =
                            (blend_px.0 - globe_px.0).hypot(blend_px.1 - globe_px.1);
                        if drift > worst_drift {
                            worst_drift = drift;
                            worst_at = (u, v);
                        }
                        sum_globe.0 += globe_px.0;
                        sum_globe.1 += globe_px.1;
                        sum_blend.0 += blend_px.0;
                        sum_blend.1 += blend_px.1;
                        sampled += 1;
                    }
                }
            }
        }

        assert!(
            sampled > 1_000,
            "transition {t} (zoom {zoom}) sampled only {sampled} on-screen points -- the \
             tiles this test scans no longer cover the frame, so a passing result would \
             mean nothing"
        );

        assert!(
            worst_drift <= MAX_VERTEX_DRIFT_PX,
            "at transition {t} (zoom {zoom}), the blend moved a vertex {worst_drift:.1} px \
             off the sphere it is blending from, at in-tile {worst_at:?}, in a \
             {FRAME_PX} px frame. The blend is only harmless where the two projections \
             already agree; a drift this size means the band has been moved back down to \
             where they do not. See this file's module docs."
        );

        let n = f64::from(sampled);
        let centroid_drift = (sum_blend.0 / n - sum_globe.0 / n)
            .hypot(sum_blend.1 / n - sum_globe.1 / n);
        assert!(
            centroid_drift <= MAX_CENTROID_DRIFT_PX,
            "at transition {t} (zoom {zoom}), the centroid of the covered sphere slid \
             {centroid_drift:.1} px away from where the pure-globe projection puts it, at a \
             FIXED camera. That slide is the lens/gore/egg defect."
        );
    }
}

/// The blend's endpoints are exact, and the band is a band — the two facts
/// the test above cannot see, because it only ever looks inside the band.
#[test]
fn the_transition_is_one_where_the_planet_is_a_ball_and_zero_where_a_street_is() {
    assert_eq!(
        transition_for_zoom(0.0),
        1.0,
        "a whole-planet camera must be pure vertical perspective"
    );
    assert_eq!(
        transition_for_zoom(6.0),
        1.0,
        "z6 is a country, not a street: Mercator's poles are still at infinity relative to \
         the sphere's, so there must be no Mercator in the matrix yet"
    );
    assert_eq!(
        transition_for_zoom(24.0),
        0.0,
        "a street-corner camera must be pure Mercator, so `vs_main` returns before \
         `project_to_sphere` is ever evaluated"
    );

    let mut previous = 1.0_f32;
    let mut z = 0.0_f64;
    while z <= 24.0 {
        let t = transition_for_zoom(z);
        assert!(t <= previous, "globeness must not go backwards at z{z}");
        assert!((0.0..=1.0).contains(&t), "globeness out of range at z{z}: {t}");
        previous = t;
        z += 0.01;
    }

    // The band exists: there is a zoom where the blend is genuinely partial,
    // or the projection would snap and the snap would be visible.
    assert!(
        (0.0..1.0).contains(&transition_for_zoom(11.5)),
        "the band must not have collapsed to a step"
    );
}

/// **Moving the blend band up puts the globe's tile cover in charge at high
/// zoom, and it must still behave like a viewport there.**
///
/// `GlobeCover` replaces the flat `pitch_aware_tile_bbox` selection on every
/// frame with a non-zero transition, which now includes every zoom up to
/// `PROJECTION_BLEND_ZOOM_MAX`. The cover descends a quadtree from z0 and
/// culls against the horizon plus five frustum planes, so at z11 the answer
/// should be the handful of tiles the screen actually shows. A cover that
/// quietly fell back to a level's worth of tiles up here would not look
/// wrong — the extra tiles are off screen — it would just fetch thousands of
/// them, which is why this is asserted rather than assumed.
#[test]
fn the_globe_cover_is_still_a_viewport_at_the_top_of_the_band() {
    use gpui_maps::render::globe::GlobeCover;

    let camera = Camera {
        center_lon: 103.8198,
        center_lat: 1.3521,
        zoom: 11.9,
        pitch: 0.0,
        bearing: 0.0,
        viewport_width_px: 1024.0,
        viewport_height_px: 768.0,
        fov_y: Camera::DEFAULT_FOV_Y,
    };
    let frame = globe_frame(&camera);
    assert!(
        frame.transition > 0.0,
        "z11.9 is inside the blend band, so the globe cover is the one in charge"
    );
    let cover = GlobeCover::new(&frame)
        .cover(11)
        .expect("a frame with a non-zero transition has a globe cover");

    // 1024x768 at z11 is 4x3 tiles of 256 px, so ~12 with a ring of slack.
    assert!(
        (4..=64).contains(&cover.len()),
        "the z11 globe cover of a 1024x768 viewport was {} tiles -- it should be the \
         dozen the screen shows, not a level",
        cover.len()
    );

    // And it contains the tile directly under the camera.
    let (cx, cy) = gpui_maps::geo::lon_lat_to_tile_xy(camera.center_lon, camera.center_lat, 11.0);
    let centre = TileId { z: 11, x: cx as u32, y: cy as u32 };
    assert!(
        cover.contains(&centre),
        "the globe cover left out the tile under the camera: {centre:?} not in {cover:?}"
    );
}

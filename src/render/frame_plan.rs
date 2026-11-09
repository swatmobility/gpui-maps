//! What a frame's space pass CONTAINS, decided once for every target.
//!
//! Three shells -- `src/map_view.rs`, `web/src/app.rs`, `ios/src/app.rs` --
//! each used to compute these gates itself. That is why the browser drew no
//! stars for months: its copy simply never called them. The gates live here
//! now; a shell may only obey.
//!
//! This module is deliberately free of `gpui`, `wgpu`, DOM and UIKit. The
//! `SpaceDraw` a shell builds from this plan holds GPU handles and stays
//! platform-local -- the DECISION does not.

use crate::render::atmosphere::Atmosphere;
use crate::render::space::{self, Space};
use crate::style::{Rgba, SkyFrame};

/// Plain values: everything a shell needs to build its space pass.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpacePlan {
    pub clear_color: Rgba,
    pub draws_stars: bool,
    pub draws_backdrop: bool,
    pub draws_atmosphere: bool,
    /// Whether the sky pass (`render::sky::SkyPipeline`) is submitted: the
    /// style's `sky` above the limb, faded toward space by the transition,
    /// on any frame where some pixel looks above the limb. False on every
    /// frame that sees none -- every pitch-0 frame, every Mercator one.
    pub draws_sky: bool,
    /// The style's `sky` at this frame's zoom -- what the sky pass is
    /// written from.
    pub sky: SkyFrame,
    /// What the sky pass paints where a ray hits the planet: the style's
    /// background colour (transparent for a style with none), handed to
    /// `SkyPipeline::write`. A frame whose sky draws clears to SPACE, so
    /// this -- not the clear -- is the colour of bare land under the tiles
    /// wherever there is no ocean sphere (every z6-z11 tilted frame).
    pub ground: Rgba,
    /// The intensity the scattering pass is written with:
    /// `Atmosphere::intensity` times the style's `atmosphere-blend`, as
    /// `draw_sky.ts`'s `drawAtmosphere` multiplies its pass by it. A shell
    /// writes `Atmosphere { intensity: plan.atmosphere_intensity, .. }`.
    pub atmosphere_intensity: f32,
}

/// The single definition of what a frame's space pass contains.
///
/// `sky` is the style's `sky` at this frame's zoom
/// (`style.sky.at_zoom(zoom)`), our defaults when the style has none.
/// `above_horizon` is `render::sky::sees_above_horizon(camera, &globe_frame)`
/// -- a fact about the frame's geometry, which every shell computes the same
/// way; the DECISION it feeds is made here.
///
/// **Whenever the sky draws, the frame clears to SPACE and the stars may
/// draw**, at any zoom -- not only inside the globe band. The sky pass is
/// full-screen and paints nothing on the night side (`render::sky`'s
/// "Night"), so what shows there is the clear colour and the stars: a clear
/// to the style background would put the land colour in the night sky at
/// z6.5, tilted. A frame that sees no sky is exactly what it was.
///
/// **The ground under that space clear is the sky pass's job**, through
/// [`SpacePlan::ground`]: below the limb it paints the background. Without
/// it every bare-land pixel of a z6-z11 tilted frame was the sky's colour or
/// black space with stars (Kansas and the Atacama, `render::sky`'s "The
/// ground below the limb").
pub fn space_plan(
    background: Option<Rgba>,
    space: &Space,
    atmosphere: &Atmosphere,
    sky: &SkyFrame,
    space_transition: f32,
    above_horizon: bool,
) -> SpacePlan {
    let atmosphere_intensity = atmosphere.intensity.clamp(0.0, 1.0) * sky.atmosphere_blend.clamp(0.0, 1.0);
    let air = Atmosphere { intensity: atmosphere_intensity, ..*atmosphere };
    let draws_sky = crate::render::sky::draws_sky(sky, space_transition, above_horizon);
    SpacePlan {
        clear_color: if draws_sky {
            space.color
        } else {
            space::space_clear_color(background, space, space_transition)
        },
        draws_stars: space.draws_stars(space_transition) || (draws_sky && space.star_alpha() > 0.0),
        draws_backdrop: space::draws_backdrop(space_transition),
        draws_atmosphere: air.draws(space_transition),
        draws_sky,
        sky: *sky,
        ground: background.unwrap_or(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }),
        atmosphere_intensity,
    }
}

/// **The ground observer's star field this frame** (`MapView::set_sky_stars`):
/// the intensity to draw it at, `0.0` for no draw. Only when the host
/// turned it `on`, on a flat frame (`space_transition <= 0`: the globe has
/// its own stars) whose sky pass draws (`plan.draws_sky`: something above
/// the horizon is on screen), and under a dark sky colour
/// (`space::sky_star_intensity`). Off, nothing changes in the plan, so the
/// frame issues no extra draw.
#[must_use]
pub fn frame_sky_stars(on: bool, plan: &SpacePlan, space_transition: f32) -> f32 {
    if !on || space_transition.is_nan() || space_transition > 0.0 || !plan.draws_sky {
        return 0.0;
    }
    space::sky_star_intensity(&plan.sky)
}

/// One extruded group's frame values: its `group` (the spans it draws,
/// `render::extrusion::GroupSpan::group`), its uniform, and whether it
/// takes the translucent two-pass path (a `Prepass` then a `Color` step).
#[derive(Debug, Clone, PartialEq)]
pub struct ExtrusionLayerFrame {
    pub group: u16,
    pub params: crate::render::extrusion::ExtrusionParams,
    pub translucent: bool,
}

/// One draw pass over every extruded tile: `layer_index` into
/// [`ExtrusionPlan::layers`] (and so into `ExtrusionParamsGpu`'s slots).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExtrusionStep {
    pub layer_index: usize,
    pub pass: crate::render::extrusion::ExtrusionPass,
}

/// What a frame's extrusions draw, decided once for every shell (the same
/// reason [`SpacePlan`] exists). Empty below
/// [`crate::style::EXTRUSION_MIN_ZOOM`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ExtrusionPlan {
    pub layers: Vec<ExtrusionLayerFrame>,
    pub steps: Vec<ExtrusionStep>,
    /// World units per metre at the camera's latitude (0 when flattened).
    pub meters_to_world: f32,
}

/// Debug and staging switches for [`extrusion_plan`].
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ExtrusionOptions {
    /// Honour `fill-extrusion-opacity` < 1 with the two-pass translucent
    /// draw. MapView and captures turn it on; off, a translucent layer
    /// draws opaque at opacity 1 (the tests' default, and a debug switch).
    pub translucency: bool,
    pub opacity_override: Option<f32>,
    /// Debug: meters_to_world = 0 (roofs at ground level). Used by the capture wall check.
    pub flatten: bool,
    /// Multiplies each layer's `fill-extrusion-opacity` at the frame's
    /// zoom (then clamped to `0..=1`), so a zoom fade survives: Streets'
    /// `15 -> 0, 15.5 -> 0.6` scaled by `1 / 0.6` fades to solid.
    /// `MapView::set_extrusion_overrides`. Ignored under `opacity_override`.
    pub opacity_scale: Option<f32>,
    /// Replaces the style's `light.anchor` for every extrusion layer.
    /// `MapView::set_extrusion_overrides`.
    pub light_anchor: Option<crate::style::LightAnchor>,
    /// Shading beyond MapLibre's; the default is MapLibre exactly.
    pub look: crate::render::extrusion::ExtrusionLook,
    /// This frame's view-depth bands and haze colour
    /// ([`extrusion_frame_depth`]); the default turns both bands off.
    pub depth: ExtrusionDepth,
}

/// One frame's depth-dependent extrusion inputs, in view depth (clip `w`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ExtrusionDepth {
    /// `[start, end]` of the distance haze ([`crate::render::extrusion::ExtrusionLook::haze`]).
    pub haze_band: Option<[f32; 2]>,
    /// What the haze blends toward: the frame's background, rgb.
    pub haze_color: [f32; 3],
    /// `[start, end]` over which buildings sink away before the cover's
    /// building data ends (`cover_plan::building_fade_band`).
    pub building_cut: Option<[f32; 2]>,
    /// The frame's logical viewport, `[width, height]`
    /// ([`crate::render::extrusion::ExtrusionParams::viewport`]).
    pub viewport_px: [f32; 2],
}

/// How far past the look-at point the haze reaches full strength, as a
/// multiple of the camera's distance to it.
pub const HAZE_END_DISTANCES: f64 = 4.0;

/// The frame's [`ExtrusionDepth`]: the haze from the look-at point's depth
/// to [`HAZE_END_DISTANCES`] times it (nothing nearer than the look-at
/// point is hazed), toward `background`; the building cut from
/// [`crate::render::cover_plan::FlatCover::building_reach`].
#[must_use]
pub fn extrusion_frame_depth(
    camera: &crate::render::Camera,
    building_reach: Option<f64>,
    background: crate::style::Rgba,
) -> ExtrusionDepth {
    let d = camera.distance();
    ExtrusionDepth {
        haze_band: d.is_finite().then_some([d as f32, (d * HAZE_END_DISTANCES) as f32]),
        haze_color: [background.r, background.g, background.b],
        building_cut: crate::render::cover_plan::building_fade_band(camera, building_reach),
        viewport_px: [camera.viewport_width_px, camera.viewport_height_px],
    }
}

/// Whether any `fill-extrusion` layer draws at `camera_zoom`: what turns on
/// the cover's building floor (`cover_plan::CoverZoom::building_floor`).
#[must_use]
pub fn extrusion_visible(layers: &[crate::style::StyleLayer], camera_zoom: f64) -> bool {
    layers.iter().any(|l| matches!(l, crate::style::StyleLayer::Extrusion { .. }))
        && !extrusion_plan(layers, camera_zoom, 0.0, 0.0, ExtrusionOptions { translucency: true, ..Default::default() })
            .steps
            .is_empty()
}

/// The display cover's request for a shell that knows no source minzoom
/// (web, iOS): `tile_zoom` with the default building floor while an
/// extrusion layer draws -- the far-building fix every shell shares.
/// `MapView` asks the same with its source's stated minzoom.
#[must_use]
pub fn building_cover_request(
    layers: &[crate::style::StyleLayer],
    camera_zoom: f64,
    tile_zoom: u8,
) -> crate::render::cover_plan::CoverZoom {
    let building_floor =
        extrusion_visible(layers, camera_zoom).then(|| crate::render::cover_plan::building_floor_for(None));
    crate::render::cover_plan::CoverZoom { building_floor, ..tile_zoom.into() }
}

/// Metres around the equator: `WORLD_SIZE` world units.
const EARTH_CIRCUMFERENCE_M: f64 = 40_075_016.686;

/// The single definition of what a frame's extrusions draw.
///
/// - **Zoom gate:** nothing below [`crate::style::EXTRUSION_MIN_ZOOM`]
///   (the Ruling: the boundary is exclusive -- the flat fallback draws
///   below 12, the extrusion from 12, never both). A layer draws while
///   `min_zoom <= floor(zoom) < max_zoom` (MapLibre's exclusive maxzoom).
/// - **Height scale:** `WORLD_SIZE / (circumference · cos(lat))`: Mercator
///   stretches a metre by `1 / cos(lat)`, and a building's height must
///   stretch with its footprint, or every building away from the equator
///   is too short (half height at 60°).
/// - **Light:** the style's light at this zoom (`ExtrusionLightTable::at`),
///   a raw MapLibre-magnitude vector turned by the bearing when
///   viewport-anchored ([`crate::render::extrusion::light_direction`]).
/// - **Opacity:** the layer's `fill-extrusion-opacity` at the fractional
///   zoom (`ZoomValue::value_at_zoom`, which follows a stop off the integer
///   grid exactly); alpha is opacity alone (Task 6 ruling).
/// - **Colour, height, base** are not evaluated here: the mesh table holds
///   each feature's `(floor z, floor z + 1)` pair and the shader mixes it by
///   the tile's `blend_t`, the same whole-zoom interpolation every flat fill
///   uses. A colour or height stop OFF the integer grid is therefore read at
///   its integer resample (no vendored style has one: Standard's colour and
///   height do not ramp on zoom).
///
/// One layer per distinct `group` (every colour case of one source layer
/// shares the group and so the same uniform), in style order. An opaque
/// layer is one `Opaque` step; a translucent one (opacity < 1 with
/// `translucency` on) is a depth-only `Prepass` step then a
/// `Color { stencil_ref }` step, MapLibre's two-pass translucent draw
/// (`draw_fill_extrusion.ts:27-47`).
#[must_use]
pub fn extrusion_plan(
    layers: &[crate::style::StyleLayer],
    camera_zoom: f64,
    center_lat_deg: f64,
    bearing_rad: f64,
    options: ExtrusionOptions,
) -> ExtrusionPlan {
    use crate::render::extrusion::{frame_translate, light_direction, ExtrusionParams, ExtrusionPass};
    if !camera_zoom.is_finite() || camera_zoom < f64::from(crate::style::EXTRUSION_MIN_ZOOM) {
        return ExtrusionPlan::default();
    }
    // BUILDINGS NEVER DRAW ON THE GLOBE, and this gate is why: the globe
    // transition ends at `PROJECTION_BLEND_ZOOM_MAX` (12), exactly where
    // extrusion starts, so every frame that extrudes is pure Mercator. The
    // extrusion shader, the building floor, the fade bands and label
    // occlusion all assume a flat frame. Pinned by the const assert below
    // and `buildings_never_draw_during_the_globe_transition`.
    const _: () = assert!(crate::render::globe::PROJECTION_BLEND_ZOOM_MAX <= crate::style::EXTRUSION_MIN_ZOOM as f64);
    let meters_to_world = if options.flatten {
        0.0
    } else {
        let cos = center_lat_deg.to_radians().cos().max(1e-6);
        (f64::from(crate::render::camera::WORLD_SIZE) / (EARTH_CIRCUMFERENCE_M * cos)) as f32
    };
    let zf = camera_zoom.floor().min(f64::from(u8::MAX)) as u8;
    let mut plan = ExtrusionPlan { meters_to_world, ..ExtrusionPlan::default() };
    for layer in layers {
        let crate::style::StyleLayer::Extrusion {
            group,
            opacity,
            vertical_gradient,
            translate,
            translate_anchor,
            light,
            min_zoom,
            max_zoom,
            ..
        } = layer
        else {
            continue;
        };
        if min_zoom.is_some_and(|mn| zf < mn) || max_zoom.is_some_and(|mx| zf >= mx) {
            continue;
        }
        if plan.layers.iter().any(|l| l.group == *group) {
            continue;
        }
        // At the FRACTIONAL zoom, off-grid stops included: Standard's
        // `15 -> 0, 15.3 -> a` fade reads 2a/3 at z15.2, as in MapLibre.
        let mut o = options
            .opacity_override
            .unwrap_or_else(|| opacity.value_at_zoom(camera_zoom) * options.opacity_scale.unwrap_or(1.0))
            .clamp(0.0, 1.0);
        // An invisible layer draws nothing at all -- no prepass either, whose
        // depth would hide later layers behind buildings no one can see.
        // MapLibre `draw_fill_extrusion.ts:20-22` returns early the same
        // way; Mapbox Standard's buildings reach opacity 0 at exactly z15.
        // (A NaN opacity survives `clamp`, so it is skipped explicitly.)
        if o.is_nan() || o <= 0.0 {
            continue;
        }
        let translucent = options.translucency && o < 1.0;
        if !options.translucency && o < 1.0 {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| eprintln!("extrusion: translucency off, a translucent layer drawn opaque"));
            o = 1.0;
        }
        let mut light = light.at(camera_zoom);
        if let Some(anchor) = options.light_anchor {
            light.anchor = anchor;
        }
        let params = ExtrusionParams {
            light_dir: light_direction(&light, bearing_rad),
            light_intensity: light.intensity,
            light_color: [light.color.r, light.color.g, light.color.b],
            vertical_gradient: if *vertical_gradient { 1.0 } else { 0.0 },
            // Pixels, literal-only from the compiler; a viewport anchor is
            // turned by +bearing (MapLibre `translatePosition`, util.ts:69).
            translate_px: frame_translate(*translate, *translate_anchor, bearing_rad),
            opacity: o,
            meters_to_world,
            ..ExtrusionParams::ZERO
        };
        let mut params = params;
        options.look.apply(&mut params);
        let haze = options.depth.haze_band.filter(|_| options.look.haze > 0.0).unwrap_or([0.0; 2]);
        let cut = options.depth.building_cut.unwrap_or([0.0; 2]);
        params.bands = [haze[0], haze[1], cut[0], cut[1]];
        let [r, g, b] = options.depth.haze_color;
        params.haze_color = [r, g, b, 1.0];
        let [vw, vh] = options.depth.viewport_px;
        params.viewport = [vw, vh, 0.0, 0.0];
        let layer_index = plan.layers.len();
        // The stencil reference comes from the layer index (Task 7 review
        // F5, which superseded the per-tile clear): the tile pass clears
        // the stencil to 0 ONCE, and `index % 255 + 1` is distinct for the
        // first 255 layers. Past that a reference would alias an earlier
        // layer's stamp and drop its pixels, so such a layer draws opaque
        // instead (no style comes near: Liberty has 1, Standard 2).
        let stenciled = translucent && layer_index < 255;
        debug_assert!(!translucent || stenciled, "more than 255 extrusion layers: stencil references would alias");
        if stenciled {
            plan.steps.push(ExtrusionStep { layer_index, pass: ExtrusionPass::Prepass });
            plan.steps.push(ExtrusionStep {
                layer_index,
                pass: ExtrusionPass::Color { stencil_ref: crate::render::extrusion::stencil_ref(layer_index) },
            });
        } else {
            plan.steps.push(ExtrusionStep { layer_index, pass: ExtrusionPass::Opaque });
        }
        plan.layers.push(ExtrusionLayerFrame { group: *group, params, translucent: stenciled });
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Sky stars: off adds nothing, and on draws only a dark sky.** Off,
    /// the intensity is zero whatever the frame, so `paint` leaves
    /// `draws_stars` as the plan had it and issues no star draw; on, a
    /// flat eye frame under the night race's sky draws them, a day sky
    /// (the style default) does not, a frame that sees no sky does not,
    /// and the globe (its own stars) does not.
    #[test]
    fn sky_stars_draw_only_when_asked_on_a_dark_flat_sky() {
        let night = SkyFrame {
            sky_color: Rgba { r: 0.016, g: 0.024, b: 0.051, a: 1.0 },
            horizon_color: Rgba { r: 0.149, g: 0.165, b: 0.227, a: 1.0 },
            ..SkyFrame::default()
        };
        let flat = |sky: SkyFrame, draws_sky: bool| SpacePlan {
            draws_sky,
            sky,
            ..space_plan(None, &Space::default(), &Atmosphere::default(), &sky, 0.0, false)
        };
        let lit = flat(night, true);
        assert!(!lit.draws_stars, "the plan itself draws no stars on a flat frame");
        assert_eq!(frame_sky_stars(false, &lit, 0.0), 0.0, "off: nothing");
        let on = frame_sky_stars(true, &lit, 0.0);
        assert!(on > 0.5 && on <= space::SKY_STAR_MAX_INTENSITY, "on under a night sky: {on}");
        assert_eq!(frame_sky_stars(true, &flat(SkyFrame::default(), true), 0.0), 0.0, "a day sky draws none");
        assert_eq!(frame_sky_stars(true, &flat(night, false), 0.0), 0.0, "no sky on screen, no stars");
        assert_eq!(frame_sky_stars(true, &lit, 0.4), 0.0, "the globe has its own");
        assert_eq!(frame_sky_stars(true, &lit, f32::NAN), 0.0);
    }

    #[test]
    fn the_shells_building_cover_request_floors_only_with_buildings() {
        assert_eq!(building_cover_request(&liberty_layers(), 16.0, 14).building_floor, Some(13));
        assert_eq!(building_cover_request(&liberty_layers(), 16.0, 14).zoom, 14);
        let flat: Vec<_> = liberty_layers().into_iter().filter(|l| !matches!(l, crate::style::StyleLayer::Extrusion { .. })).collect();
        assert_eq!(building_cover_request(&flat, 16.0, 14).building_floor, None);
        assert_eq!(building_cover_request(&liberty_layers(), 11.0, 11).building_floor, None, "below the buildings' zoom");
    }

    #[test]
    fn buildings_never_draw_during_the_globe_transition() {
        let layers = liberty_layers();
        let mut z = crate::render::globe::GLOBE_ZOOM_MIN;
        while z < 16.0 {
            let camera = crate::render::Camera {
                center_lon: 139.7,
                center_lat: 35.69,
                zoom: z,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: 800.0,
                viewport_height_px: 600.0,
                fov_y: crate::render::Camera::DEFAULT_FOV_Y,
            };
            let transition = crate::render::globe::globe_frame(&camera).transition;
            let steps = extrusion_plan(&layers, z, 35.69, 0.0, ExtrusionOptions::default()).steps.len();
            assert!(transition == 0.0 || steps == 0, "z{z}: globe transition {transition} with {steps} extrusion steps");
            z += 0.05;
        }
    }
    use crate::render::extrusion::ExtrusionPass;

    fn liberty_layers() -> Vec<crate::style::StyleLayer> {
        crate::style_spec::parse_and_compile(&std::fs::read("examples/liberty-style.json").unwrap()).unwrap().layers
    }

    #[test]
    fn liberty_at_z16_plans_one_extrusion_layer() {
        let plan = extrusion_plan(&liberty_layers(), 16.0, 35.69, 0.0, ExtrusionOptions::default());
        assert_eq!(plan.layers.len(), 1);
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].pass, ExtrusionPass::Opaque);
        // Translucency off (the default) -> drawn at opacity 1.
        assert_eq!(plan.layers[0].params.opacity, 1.0);
    }

    #[test]
    fn a_translucent_layer_gets_a_prepass_then_a_stencilled_colour_pass() {
        let plan = extrusion_plan(&liberty_layers(), 16.0, 35.69, 0.0, ExtrusionOptions { translucency: true, ..Default::default() });
        assert_eq!(plan.layers[0].params.opacity, 0.8);
        assert!(plan.layers[0].translucent);
        assert_eq!(
            plan.steps.iter().map(|s| s.pass).collect::<Vec<_>>(),
            [ExtrusionPass::Prepass, ExtrusionPass::Color { stencil_ref: 1 }]
        );
    }

    /// The stencil reference is the LAYER index's (`index % 255 + 1`, the
    /// Task 7 review F5 Ruling), not a running count of translucent
    /// layers: layer 2 gets 3, and the opaque layer 1 between them
    /// leaves 2 unused.
    #[test]
    fn two_translucent_layers_get_distinct_stencil_refs_and_an_opaque_one_gets_one_pass() {
        let style = crate::style_spec::parse_and_compile(br##"{"version":8,"sources":{"s":{"type":"vector","tiles":["x/{z}/{x}/{y}"]}},
            "layers":[
            {"id":"a","type":"fill-extrusion","source":"s","source-layer":"b","paint":{"fill-extrusion-color":"#f00","fill-extrusion-opacity":0.5}},
            {"id":"o","type":"fill-extrusion","source":"s","source-layer":"b","paint":{"fill-extrusion-color":"#0f0"}},
            {"id":"c","type":"fill-extrusion","source":"s","source-layer":"b","paint":{"fill-extrusion-color":"#00f","fill-extrusion-opacity":0.5}}]}"##).unwrap();
        let plan = extrusion_plan(&style.layers, 16.0, 0.0, 0.0, ExtrusionOptions { translucency: true, ..Default::default() });
        assert_eq!(
            plan.steps.iter().map(|s| (s.layer_index, s.pass)).collect::<Vec<_>>(),
            [
                (0, ExtrusionPass::Prepass),
                (0, ExtrusionPass::Color { stencil_ref: 1 }),
                (1, ExtrusionPass::Opaque),
                (2, ExtrusionPass::Prepass),
                (2, ExtrusionPass::Color { stencil_ref: 3 }),
            ]
        );
        assert_eq!(plan.layers.iter().map(|l| l.translucent).collect::<Vec<_>>(), [true, false, true]);
    }

    /// Task 8 review, finding 1: an opacity-0 layer is skipped ENTIRELY, as
    /// MapLibre `draw_fill_extrusion.ts:20-22` does -- no prepass whose depth
    /// would hide a later layer behind invisible buildings, and not drawn
    /// opaque when translucency is off either.
    #[test]
    fn an_opacity_0_layer_is_skipped_entirely_prepass_included() {
        let style = crate::style_spec::parse_and_compile(br##"{"version":8,"sources":{"s":{"type":"vector","tiles":["x/{z}/{x}/{y}"]}},
            "layers":[
            {"id":"gone","type":"fill-extrusion","source":"s","source-layer":"b","paint":{"fill-extrusion-color":"#f00",
                "fill-extrusion-opacity":["interpolate",["linear"],["zoom"],15,0,15.3,0.8]}},
            {"id":"kept","type":"fill-extrusion","source":"s","source-layer":"b","paint":{"fill-extrusion-color":"#0f0","fill-extrusion-opacity":0.5}}]}"##).unwrap();
        let kept = style.layers.iter().find_map(|l| match l {
            crate::style::StyleLayer::Extrusion { id, group, .. } if id == "kept" => Some(*group),
            _ => None,
        });
        for translucency in [true, false] {
            let plan = extrusion_plan(&style.layers, 15.0, 0.0, 0.0, ExtrusionOptions { translucency, ..Default::default() });
            assert_eq!(plan.layers.iter().map(|l| Some(l.group)).collect::<Vec<_>>(), [kept], "translucency {translucency}");
            let want: &[ExtrusionPass] =
                if translucency { &[ExtrusionPass::Prepass, ExtrusionPass::Color { stencil_ref: 1 }] } else { &[ExtrusionPass::Opaque] };
            assert_eq!(plan.steps.iter().map(|s| (s.layer_index, s.pass)).collect::<Vec<_>>(),
                want.iter().map(|&p| (0, p)).collect::<Vec<_>>());
        }
        // Just past 15 the layer is visible again, and it plans first (style
        // order), at the FRACTIONAL zoom's opacity: two thirds of the way up
        // `15 -> 0, 15.3 -> 0.8` is 0.533, as in MapLibre -- not the 0.16 a
        // whole-zoom resample (0 at 15, 0.8 at 16) would give.
        let plan = extrusion_plan(&style.layers, 15.2, 0.0, 0.0, ExtrusionOptions { translucency: true, ..Default::default() });
        assert_eq!(plan.layers.len(), 2);
        assert_ne!(Some(plan.layers[0].group), kept, "the fading layer plans first");
        let o = plan.layers[0].params.opacity;
        assert!((o - 0.8 * 2.0 / 3.0).abs() < 1e-4, "z15.2 opacity {o}");
        // An override of 0 is the same invisible layer.
        let plan = extrusion_plan(&style.layers, 16.0, 0.0, 0.0,
            ExtrusionOptions { translucency: true, opacity_override: Some(0.0), ..Default::default() });
        assert!(plan.steps.is_empty() && plan.layers.is_empty());
    }

    #[test]
    fn a_viewport_translate_is_rotated_by_the_bearing_and_a_map_translate_is_not() {
        let mut layers = liberty_layers();
        for l in &mut layers { if let crate::style::StyleLayer::Extrusion { translate, translate_anchor, .. } = l {
            *translate = [10.0, 0.0]; *translate_anchor = crate::style::TranslateAnchor::Map; } }
        let map = extrusion_plan(&layers, 16.0, 0.0, std::f64::consts::FRAC_PI_2, ExtrusionOptions::default());
        assert_eq!(map.layers[0].params.translate_px, [10.0, 0.0]);
        for l in &mut layers { if let crate::style::StyleLayer::Extrusion { translate_anchor, .. } = l {
            *translate_anchor = crate::style::TranslateAnchor::Viewport; } }
        let vp = extrusion_plan(&layers, 16.0, 0.0, std::f64::consts::FRAC_PI_2, ExtrusionOptions::default());
        let t = vp.layers[0].params.translate_px;
        assert!(t[0].abs() < 1e-4 && (t[1].abs() - 10.0).abs() < 1e-4, "{t:?}: rotated a quarter turn");
        // MapLibre `translatePosition` (util.ts:69): +bearing, so +x lands on +y.
        assert!(t[1] > 0.0, "{t:?}");
    }

    #[test]
    fn with_translucency_off_a_translucent_layer_draws_opaque_in_one_pass() {
        let plan = extrusion_plan(&liberty_layers(), 16.0, 35.69, 0.0, ExtrusionOptions::default());
        assert_eq!(plan.layers[0].params.opacity, 1.0);
        assert!(!plan.layers[0].translucent);
        assert_eq!(plan.steps.iter().map(|s| s.pass).collect::<Vec<_>>(), [ExtrusionPass::Opaque]);
    }

    #[test]
    fn below_zoom_12_the_plan_draws_no_extrusions() {
        let mut layers = liberty_layers();
        // Liberty's `building-3d` is minzoom 14, so these pass on the layer's
        // own range; the camera gate for a layer that reaches below 12 is
        // pinned by `the_z12_handover_is_exclusive` (minzoom 10).
        for z in [3.0, 11.0, 11.99] {
            assert!(extrusion_plan(&layers, z, 35.69, 0.0, ExtrusionOptions::default()).steps.is_empty(), "z{z}");
        }
        layers.clear();
        assert!(extrusion_plan(&layers, 16.0, 35.69, 0.0, ExtrusionOptions::default()).steps.is_empty());
    }

    #[test]
    fn meters_to_world_follows_the_camera_latitude() {
        let at = |lat| extrusion_plan(&liberty_layers(), 16.0, lat, 0.0, ExtrusionOptions::default()).meters_to_world;
        let equator = at(0.0);
        assert!((equator - crate::render::camera::WORLD_SIZE / 40_075_016.686).abs() < 1e-12);
        assert!((at(60.0) / equator - 2.0).abs() < 1e-4, "1 / cos 60° = 2");
    }

    #[test]
    fn flatten_and_opacity_override_reach_the_params() {
        let opts = ExtrusionOptions { flatten: true, opacity_override: Some(0.25), translucency: true, ..Default::default() };
        let plan = extrusion_plan(&liberty_layers(), 16.0, 35.69, 0.0, opts);
        assert_eq!(plan.layers[0].params.meters_to_world, 0.0);
        assert_eq!(plan.layers[0].params.opacity, 0.25);
    }

    /// The z12 Ruling, end to end through the compiler: a `fill-extrusion`
    /// whose `minzoom` reaches below 12 draws its flat footprint at zoom
    /// < 12 only and its extrusion at zoom >= 12 only -- no frame both.
    #[test]
    fn the_z12_handover_is_exclusive() {
        let style = serde_json::json!({
            "version": 8,
            "sources": {"s": {"type": "vector", "tiles": ["x/{z}/{x}/{y}"]}},
            "layers": [{"id": "b", "type": "fill-extrusion", "source": "s", "source-layer": "building",
                        "minzoom": 10, "paint": {"fill-extrusion-color": "#888", "fill-extrusion-height": 30}}]
        });
        let layers = crate::style_spec::parse_and_compile(&serde_json::to_vec(&style).unwrap()).unwrap().layers;
        let flat_max = layers
            .iter()
            .find_map(|l| match l {
                crate::style::StyleLayer::Fill { max_zoom, .. } => Some(*max_zoom),
                _ => None,
            })
            .expect("a flat fallback below 12");
        // The flat gate is inclusive (`zoom > max_zoom` skips): 11 is its
        // last bucket, so it draws for zoom < 12 and never at 12.
        assert_eq!(flat_max, Some(crate::style::EXTRUSION_MIN_ZOOM - 1));
        assert!(extrusion_plan(&layers, 11.99, 0.0, 0.0, ExtrusionOptions::default()).steps.is_empty());
        assert_eq!(extrusion_plan(&layers, 12.0, 0.0, 0.0, ExtrusionOptions::default()).steps.len(), 1);
    }

    #[test]
    fn a_viewport_light_turns_with_the_bearing_in_the_params() {
        let layers = liberty_layers();
        let a = extrusion_plan(&layers, 16.0, 35.69, 0.0, ExtrusionOptions::default()).layers[0].params.light_dir;
        let b = extrusion_plan(&layers, 16.0, 35.69, 1.0, ExtrusionOptions::default()).layers[0].params.light_dir;
        assert_ne!(a, b, "Liberty's light is viewport-anchored (the default)");
        let len = |v: [f32; 3]| (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
        assert!((len(a) - 1.15).abs() < 1e-4, "raw MapLibre magnitude: {}", len(a));
    }
}

#[cfg(test)]
mod space_tests {
    use super::*;
    use crate::render::space::Space;
    use crate::render::atmosphere::Atmosphere;
    use crate::style::SkyFrame;

    /// The three gates are the SAME three the shells used to each compute
    /// for themselves. This test is the reason they can stop.
    #[test]
    fn the_plan_reproduces_the_three_gates_the_shells_used_to_duplicate() {
        let space = Space { star_intensity: 1.0, ..Space::default() };
        let atmosphere = Atmosphere::default();

        // Fully in space: everything draws.
        let far = space_plan(None, &space, &atmosphere, &SkyFrame::default(), 1.0, true);
        assert!(far.draws_stars, "stars draw at transition 1.0");
        assert!(far.draws_backdrop, "backdrop draws at transition 1.0");

        // Fully planar: the space pass is off.
        let near = space_plan(None, &space, &atmosphere, &SkyFrame::default(), 0.0, false);
        assert!(!near.draws_stars, "stars do NOT draw at transition 0.0");
        assert!(!near.draws_backdrop, "backdrop does NOT draw at 0.0");
    }

    /// `star_intensity` defaults to 0.0 and that is OFF, not dim --
    /// `map_view.rs:3717` documents this as a lazy-allocation guard: the
    /// star draw and its 1.24 MB of buffers are never created until a
    /// non-zero intensity is actually drawn. The plan must preserve it.
    #[test]
    fn zero_star_intensity_is_off_even_fully_in_space() {
        let dark = Space { star_intensity: 0.0, ..Space::default() };
        let plan = space_plan(None, &dark, &Atmosphere::default(), &SkyFrame::default(), 1.0, true);
        assert!(!plan.draws_stars, "0.0 intensity must not allocate a star draw");
    }

    /// A shell must not have to know that the background is returned
    /// UNCHANGED below the transition -- see `map_view.rs:10424`.
    #[test]
    fn the_background_passes_through_unchanged_when_planar() {
        let bg = Rgba { r: 0.97, g: 0.96, b: 0.94, a: 1.0 };
        let plan = space_plan(Some(bg), &Space::default(), &Atmosphere::default(), &SkyFrame::default(), 0.0, false);
        assert_eq!(plan.clear_color, bg, "planar frames clear to the style background");
    }

    /// THE GREY SKY, as a plan: inside the globe band the clear colour is
    /// no longer a blend toward the style background, the sky pass draws
    /// (fading out toward z3), and the scattering pass is scaled by the
    /// style's `atmosphere-blend`.
    #[test]
    fn the_globe_band_draws_the_sky_over_space_not_the_background() {
        let bg = Rgba { r: 0xE7 as f32 / 255.0, g: 0xE3 as f32 / 255.0, b: 0xDF as f32 / 255.0, a: 1.0 };
        let space = Space::default();
        let atmosphere = Atmosphere::default();
        let sky = SkyFrame::default();
        let z5 = crate::render::globe::space_transition_for_zoom(5.0);
        assert!(z5 > 0.0 && z5 < 1.0, "z5 is inside the band: {z5}");
        let plan = space_plan(Some(bg), &space, &atmosphere, &sky, z5, true);
        assert_ne!(plan.clear_color, bg);
        assert_eq!(plan.clear_color, space.color, "above the limb the sky is drawn over SPACE");
        assert!(plan.draws_sky);
        assert!((plan.atmosphere_intensity - 0.8).abs() < 1e-6, "1.0 intensity x 0.8 atmosphere-blend");

        let z3 = crate::render::globe::space_transition_for_zoom(3.0);
        let far = space_plan(Some(bg), &space, &atmosphere, &sky, z3, true);
        assert!(!far.draws_sky, "at z3 the sky has faded to space");
        assert_eq!(far.clear_color, space.color);

        let flat = space_plan(Some(bg), &space, &atmosphere, &sky, 0.0, false);
        assert!(!flat.draws_sky && !flat.draws_atmosphere);
        assert_eq!(flat.clear_color, bg);

        let no_air = SkyFrame { atmosphere_blend: 0.0, ..sky };
        assert!(!space_plan(Some(bg), &space, &atmosphere, &no_air, 1.0, true).draws_atmosphere);

        // z6.5, tilted near the limit: no globe band, but the curved horizon
        // is on screen -- the sky draws over SPACE, and so may the stars.
        let stars = Space { star_intensity: 1.0, ..space };
        let tilted = space_plan(Some(bg), &stars, &atmosphere, &sky, 0.0, true);
        assert!(tilted.draws_sky, "a frame that sees above the limb draws the sky at any zoom");
        assert_eq!(tilted.clear_color, space.color, "the night sky above the limb is space, not the land colour");
        assert!(tilted.draws_stars, "and the stars behind it");
        assert!(!tilted.draws_backdrop && !tilted.draws_atmosphere, "nothing else of the space pass changes");
        assert_eq!(tilted.ground, bg, "the sky pass paints the ground below the limb in the background colour");
        assert_eq!(space_plan(None, &stars, &atmosphere, &sky, 0.0, true).ground.a, 0.0, "no background layer, no ground colour");
    }
}

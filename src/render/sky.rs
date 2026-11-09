//! The sky above the globe's limb: MapLibre's style-level `sky`, drawn.
//!
//! # The defect this module removes
//!
//! In the globe band (z3-z6) everything above the planet's limb used to be
//! the frame's clear colour, and that colour was
//! `lerp(style background, space colour, transition)`. Zooming in from space
//! it tended to the style's LAND colour: tilted at z5 over East Africa the
//! top third of the screen was flat `#E7E3DF`-ish grey. On a real iPhone,
//! moving closer to earth from space showed grey instead of blue or space.
//!
//! # What is ported, and from where
//!
//! MapLibre GL JS (BSD-3, Copyright (c) 2023 MapLibre contributors):
//!
//!  - the PROPERTIES and their defaults: the style spec's root `sky`
//!    (`maplibre-style-spec` `src/reference/v8.json`), carried as
//!    [`crate::style::Sky`] and parsed in `style_spec::compile_sky`;
//!  - the GRADIENT: `src/shaders/glsl/sky.fragment.glsl` -- sky colour,
//!    blending to the horizon colour over `sky-horizon-blend` of half the
//!    view by `pow(1 - blend / reach, 2)`. See `sky_shader.wgsl` for the one
//!    change a spherical horizon forces (an angle above the limb instead of
//!    a pixel distance above a line);
//!  - the FADE TO SPACE: `src/webgl/program/sky_program.ts` sets
//!    `u_sky_blend = projectionData.projectionTransition` and the shader
//!    mixes the sky toward transparent by it, so the sky is fully present on
//!    a flat map and fully gone on a globe. [`sky_alpha`] is that, with this
//!    crate's `space_transition` as the transition;
//!  - the ORDER: `painter.ts` draws the sky FIRST ("draw sky first to not
//!    overwrite symbols") and the atmosphere last. Here the sky is drawn
//!    after the stars and before the ocean sphere and the tiles -- with one
//!    change of ours: as it fades, its horizon colour runs toward its sky
//!    colour, so a white horizon over black space never reads as grey
//!    (see [`gradient`]). How much of the stars shows through it is ours
//!    too (see "Night" below);
//!  - `atmosphere-blend`: `draw_sky.ts`'s `drawAtmosphere` multiplies the
//!    scattering pass by it (`atmosphereBlend = sky.properties.get(
//!    'atmosphere-blend') * projectionData.projectionTransition`).
//!    `frame_plan::space_plan` folds it into the frame's atmosphere
//!    intensity.
//!
//! # What is NOT drawn, and why
//!
//! `fog-color`, `fog-ground-blend` and `horizon-fog-blend` are parsed and
//! carried but draw nothing. In the reference they are TERRAIN properties
//! -- the spec says "Requires 3D terrain", they are read only by
//! `terrain_program.ts`, and even there `u_fog_ground_blend_opacity` is
//! forced to 0 in globe mode. This crate has no terrain. Its own horizon
//! fade (`cover_plan::frame_fog`) is a different thing: it hides where the
//! flat cover was CUT, so it must end in the colour the frame is cleared to
//! (the style background), or the uncovered strip past the cut would show
//! as a band of a different colour. It is off on every globe frame, as
//! before.
//!
//! # The sky draws wherever the view looks above the limb
//!
//! Not only in the globe band. The projection is spherical up to z11, so a
//! frame tilted near `MAX_PITCH` at z6-z11 sees a curved horizon with sky
//! above it; the pass used to be gated on `space_transition > 0` and those
//! frames showed the flat style background there. [`sees_above_horizon`]
//! asks the geometry instead, and [`sky_alpha`] is 1 from z6 up. When the
//! sky draws the frame clears to the SPACE colour (`frame_plan`), so the
//! night sky is space and not the style background.
//!
//! # Flat frames get no sky, because they cannot see one
//!
//! On a pure-Mercator frame (z12 up) `camera_control::MAX_PITCH` and the
//! vertical FOV are both 60 degrees, so the top edge of the viewport is at
//! `pitch + fov/2 <= 90` degrees from the nadir: horizontal at the steepest
//! tilt, below the horizon otherwise, and the same for every column, so no
//! aspect ratio shows above-horizon screen area. A frame that sees no
//! above-limb pixel -- every pitch-0 frame, every low-pitch one -- submits
//! no pass and is byte-identical.
//!
//! # Night: ours -- black, with stars
//!
//! The reference's sky knows nothing of the sun. The user's rule for the
//! night side: "sky should be black stars not grey not blue". Three things,
//! all in `sky_shader.wgsl`, all mirrored here on the CPU so they can be
//! tested without a GPU:
//!
//!  1. **One terminator.** The sky over a point turns to night through
//!     [`atmosphere::terminator_lit`] -- the SAME function, width and
//!     position the ground's shade ([`atmosphere::night_shade`]) and the
//!     labels are built from. What night DOES differs, deliberately: the
//!     ground keeps a floor ([`atmosphere::night_floor`]) because a map has
//!     to stay readable, and that floor fades out by z6 because a street map
//!     is not dimmed; the sky above has no such duty and goes to SPACE at
//!     every zoom ([`sky_night_floor`] is 0 at the default
//!     `night_brightness`, and 1 -- no night -- with the terminator off).
//!     It used to multiply the white horizon by the raw `night_brightness`,
//!     which is grey (`#4A4A4A`-`#C0C0C0` on the user's screen).
//!  2. **A twilight band**: a faint lift of the horizon colour along the
//!     limb just past the terminator on the night side
//!     ([`TWILIGHT_REACH_SIN`], [`TWILIGHT_STRENGTH`]). Restrained on
//!     purpose: at the limb it adds at most 15% of the horizon colour, and
//!     it is gone about nine degrees into the night.
//!  3. **The stars hide behind a bright sky, and only a bright one.** The
//!     pass's ALPHA is its opacity to the stars drawn before it,
//!     [`star_occlusion`], not its coverage: with alpha == coverage a z4.5
//!     day sky (coverage 0.5) let half of every star through the blue. By
//!     night it is zero, so the stars show at full strength.
//!
//! [`atmosphere::terminator_lit`]: super::atmosphere::terminator_lit
//! [`atmosphere::night_shade`]: super::atmosphere::night_shade
//! [`atmosphere::night_floor`]: super::atmosphere::night_floor
//!
//! # The ground below the limb: ours
//!
//! A ray that analytically hits the planet is not sky: the pass paints it
//! the style's BACKGROUND colour (`SpacePlan::ground`), opaque, shaded by
//! the ground's own night floor ([`atmosphere::night_floor`]) through the
//! terminator at the point the ray hits -- exactly what `shader.wgsl` would
//! shade a background fill there with. The pass is drawn behind the ground,
//! so this only shows where no tile fill was drawn.
//!
//! That is most of a Mapbox Streets map: bare land is its `background`
//! layer and nothing else. Whenever this pass draws the frame clears to
//! SPACE, and from z6 up there is no ocean sphere under the tiles, so
//! before this every bare-land pixel of a tilted z6-z11 frame was the sky
//! pass's own below-limb colour -- the horizon colour where the ray's
//! nearest point to the centre happened to face the sun, black space and
//! stars where it did not. The user saw a dark curved "shadow" over Kansas
//! in daylight, black cities (translucent urban fills over black), and
//! black ground with stars in it over the Atacama. The analytic limb is
//! also where the tiles' horizon is (measured at z6.77 and z9.5), so the
//! ground this paints meets the tiles without a band.
//!
//! [`atmosphere::night_floor`]: super::atmosphere::night_floor
//!
//! # The default horizon is pale blue: ours
//!
//! A style with no `sky` gets [`crate::style::Sky::default`], which is the
//! spec's defaults with ONE change: `horizon-color` is
//! [`crate::style::HORIZON_COLOR_OURS`] (`#C8E0F8`) instead of `#FFFFFF`.
//! White blended into `#88C6FC` reads as grey haze at the limb (z5.5 day:
//! `#E4E8EC`, 7% saturation). The gradient's shape is the reference's; only
//! the default colour at its bottom moved. A style that declares a `sky`
//! gets the spec's white for anything it leaves out.
//!
//! # Its own pipeline
//!
//! One inter-stage slot (the ray), no vertex buffer, one 160-byte uniform --
//! nothing added to `shader.wgsl`, whose `VertexOutput` is what iOS's
//! 15-slot limit is spent on.

use super::atmosphere::Atmosphere;
use super::globe::GlobeFrame;
use super::Camera;
use crate::style::SkyFrame;

/// How much of the sky a frame of this globeness shows: `1 - transition`,
/// the reference's `mix(sky, transparent, projectionTransition)` -- all of it
/// from z6 up (`space_transition == 0`), none at full globe. NaN is none.
///
/// This is the sky's COVERAGE only. Whether the pass is submitted at all is
/// [`draws_sky`], which asks the geometry.
#[must_use]
pub fn sky_alpha(transition: f32) -> f32 {
    if transition.is_nan() {
        return 0.0;
    }
    (1.0 - transition).clamp(0.0, 1.0)
}

/// **Whether any pixel of this frame looks above the planet's limb.**
///
/// The sky used to be gated on `space_transition > 0`, on the premise that
/// only a globe-band frame can see above the horizon. That premise was
/// false: the PROJECTION stays spherical up to z11 (`globe::transition_for_zoom`),
/// so at z6.48 tilted near `MAX_PITCH` the curved horizon was on screen and
/// everything above it was the flat style background (`#F4F1EF`, the user's
/// browser screenshot). The limb of a sphere seen from an eye `d` radii out
/// is `asin(1 / d)` from the nadir, which is always less than the 90 degrees
/// the steepest flat view reaches.
///
/// So this asks the geometry: the eye and the four corner rays, in the SAME
/// sphere space `sky_shader.wgsl` works in. The rays whose angle from the
/// nadir is at most the limb's form a cone, whose section by the image plane
/// is convex, so if all four corners are inside it every pixel is: the
/// corners decide.
///
/// **False on a pure-Mercator frame** (`frame.transition == 0`, z12 up):
/// there the ground is a plane and `camera_control::MAX_PITCH + fov/2` is at
/// most horizontal -- see the module doc's "Flat frames".
#[must_use]
pub fn sees_above_horizon(camera: &Camera, frame: &GlobeFrame) -> bool {
    if frame.transition.is_nan() || frame.transition <= 0.0 {
        return false;
    }
    let inverse_view = frame.globe_view.inverse();
    let eye = inverse_view.transform_point3(glam::DVec3::ZERO);
    let d = eye.length();
    if !d.is_finite() || d <= 1.0 {
        return false;
    }
    let limb = (1.0 / d).asin();
    let nadir = -eye / d;
    let tan = camera.tan_half_fov();
    let aspect = f64::from(camera.viewport_width_px) / f64::from(camera.viewport_height_px).max(1.0);
    [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)].into_iter().any(|(x, y): (f64, f64)| {
        let ray = inverse_view.transform_vector3(glam::DVec3::new(x * tan * aspect, y * tan, -1.0)).normalize();
        ray.dot(nadir).clamp(-1.0, 1.0).acos() > limb
    })
}

/// Whether the sky pass is submitted this frame: some pixel looks above the
/// limb ([`sees_above_horizon`]), something of the sky is left
/// ([`sky_alpha`]), and the style's colours are not both transparent.
#[must_use]
pub fn draws_sky(sky: &SkyFrame, transition: f32, above_horizon: bool) -> bool {
    above_horizon && sky_alpha(transition) > 0.0 && (sky.sky_color.a > 0.0 || sky.horizon_color.a > 0.0)
}

/// The colour the sky pass paints along a ray `elevation` radians above the
/// limb, at coverage `alpha` ([`sky_alpha`]), BEFORE the night and the
/// coverage itself are applied -- `sky_shader.wgsl`'s gradient, on the CPU,
/// so it can be tested without a GPU.
///
/// At `alpha == 1` this is `sky.fragment.glsl` exactly. Below it the horizon
/// colour runs toward the sky colour (ours): faded by coverage alone, a white
/// horizon over black space is a mid-grey band halfway through the globe
/// band -- the colour this module exists to remove.
#[must_use]
pub fn gradient(sky: &SkyFrame, elevation: f32, alpha: f32) -> crate::style::Rgba {
    use crate::style::Lerp;
    let horizon = sky.horizon_color.lerp(&sky.sky_color, 1.0 - alpha.clamp(0.0, 1.0));
    let reach = horizon_angle(sky);
    if reach > 0.0 {
        let blend = elevation.max(0.0) / reach;
        if blend < 1.0 {
            return sky.sky_color.lerp(&horizon, (1.0 - blend).powi(2));
        }
    }
    sky.sky_color
}

/// `sky-horizon-blend` times half the DEFAULT vertical FOV, in radians:
/// a style angle, so a camera's own wider FOV does not stretch the band.
fn horizon_angle(sky: &SkyFrame) -> f32 {
    sky.sky_horizon_blend * (super::camera::FOV_Y_RADIANS_F64 / 2.0) as f32
}

/// `sky_shader.wgsl`'s `TWILIGHT_REACH_SIN`: how far past the terminator,
/// as a sine (about 9 degrees), the twilight band reaches.
pub const TWILIGHT_REACH_SIN: f32 = 0.157;
/// `sky_shader.wgsl`'s `TWILIGHT_STRENGTH`: the most of the horizon colour
/// the twilight band adds, at the limb.
pub const TWILIGHT_STRENGTH: f32 = 0.15;
/// `sky_shader.wgsl`'s `STAR_OCCLUSION_GAIN`: a day sky of this reciprocal
/// coverage or more hides the stars completely.
pub const STAR_OCCLUSION_GAIN: f32 = 4.0;

/// How much of the DAY sky is left on the night side, from the consumer's
/// `night_brightness` (clamped): `night_brightness^4`. **0.0039 at the
/// default 0.25** -- under one 8-bit step of `#88C6FC`, i.e. space -- and
/// exactly 1 at `night_brightness == 1`, the terminator switched off, so
/// that knob still means "no night" for the sky as it does for the ground.
/// Continuous between, so a consumer's subtle 0.9 night gets a subtle sky.
#[must_use]
pub fn sky_night_floor(night_brightness: f32) -> f32 {
    night_brightness.clamp(0.0, 1.0).powi(4)
}

/// How much of the day sky shows over a point whose sun is at
/// `cos_zenith`: `mix(floor, 1, terminator_lit(cos_zenith))`, with
/// `floor` [`sky_night_floor`]. `sky_shader.wgsl`'s `day`. The same
/// expression as the ground's [`super::atmosphere::night_shade`], with the
/// sky's floor in place of the ground's.
#[must_use]
pub fn day_weight(floor: f32, cos_zenith: f32) -> f32 {
    super::atmosphere::night_shade(floor, cos_zenith)
}

/// The sky pass's alpha -- its opacity to the stars behind it -- at
/// `coverage` (the colour's alpha times [`sky_alpha`]) and `day`
/// ([`day_weight`]). `STAR_OCCLUSION_GAIN` times the coverage by day, zero
/// at night, and zero whenever the coverage is. `sky_shader.wgsl`'s
/// `occlusion`.
#[must_use]
pub fn star_occlusion(coverage: f32, day: f32) -> f32 {
    (coverage * STAR_OCCLUSION_GAIN).min(1.0) * day
}

/// What `sky_shader.wgsl`'s `Sky` struct expects. 160 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct SkyUniform {
    inverse_view: [[f32; 4]; 4],
    camera_sphere: [f32; 3],
    alpha: f32,
    sun_dir: [f32; 3],
    tan_half_fov_y: f32,
    sky_color: [f32; 4],
    horizon_color: [f32; 4],
    aspect: f32,
    horizon_angle: f32,
    night: f32,
    ground_night: f32,
    ground_color: [f32; 4],
}

impl SkyUniform {
    pub(crate) fn new(
        camera: &Camera,
        frame: &GlobeFrame,
        sky: &SkyFrame,
        atmosphere: &Atmosphere,
        ground: crate::style::Rgba,
    ) -> Self {
        let inverse_view = frame.globe_view.inverse();
        let eye = inverse_view.transform_point3(glam::DVec3::ZERO);
        let sun = atmosphere.sun.direction_sphere(&inverse_view);
        let rgba = |c: crate::style::Rgba| [c.r, c.g, c.b, c.a.clamp(0.0, 1.0)];
        Self {
            inverse_view: inverse_view.as_mat4().to_cols_array_2d(),
            camera_sphere: [eye.x as f32, eye.y as f32, eye.z as f32],
            alpha: sky_alpha(frame.space_transition),
            sun_dir: [sun.x as f32, sun.y as f32, sun.z as f32],
            tan_half_fov_y: camera.tan_half_fov() as f32,
            sky_color: rgba(sky.sky_color),
            horizon_color: rgba(sky.horizon_color),
            aspect: camera.viewport_width_px / camera.viewport_height_px,
            horizon_angle: horizon_angle(sky),
            // Space by night, whatever the zoom -- see the module doc's
            // "Night".
            night: sky_night_floor(atmosphere.night_brightness_clamped()),
            // The ground's floor, the one every shell uploads to
            // `shader.wgsl` -- see "The ground below the limb".
            ground_night: super::atmosphere::night_floor(atmosphere.night_brightness_clamped(), frame.space_transition),
            ground_color: rgba(ground),
        }
    }
}

/// How far, in planet radii, the eye camera's stand-in planet puts the eye
/// above its surface ([`SkyUniform::flat`]): its limb then dips
/// `sqrt(2 * FLAT_EYE_ALTITUDE)` = 0.36 degrees below level, under the
/// fogged far edge of the cover (a cutoff of 600 m from 1.1 m up is 0.1
/// degrees down), and well clear of f32's resolution around 1.0.
pub const FLAT_EYE_ALTITUDE: f32 = 2e-5;

impl SkyUniform {
    /// **The eye camera's sky** (`camera_control::EyeCamera`): the same
    /// gradient on a flat frame, which has no globe frame to read. The
    /// shader's sphere is stood in for by a unit planet the eye sits
    /// [`FLAT_EYE_ALTITUDE`] above, in a frame whose `z` is the world's
    /// up, so "above the limb" is "above level"; the sun is overhead (the
    /// host's colours are the sky, night or day) and below the limb it
    /// paints the horizon colour -- the fog's, so the fogged ground and
    /// the sky meet without a seam.
    pub(crate) fn flat(camera: &Camera, sky: &SkyFrame) -> Self {
        let (eye, target, up) = camera.eye_target_up();
        let forward = (target - eye).normalize();
        let right = forward.cross(up).normalize();
        let up = right.cross(forward).normalize();
        // View space (x right, y up, looking down -z) to the world frame.
        let inverse_view = glam::DMat4::from_cols(right.extend(0.0), up.extend(0.0), (-forward).extend(0.0), glam::DVec4::W);
        let rgba = |c: crate::style::Rgba| [c.r, c.g, c.b, c.a.clamp(0.0, 1.0)];
        Self {
            inverse_view: inverse_view.as_mat4().to_cols_array_2d(),
            camera_sphere: [0.0, 0.0, 1.0 + FLAT_EYE_ALTITUDE],
            alpha: 1.0,
            sun_dir: [0.0, 0.0, 1.0],
            tan_half_fov_y: camera.tan_half_fov() as f32,
            sky_color: rgba(sky.sky_color),
            horizon_color: rgba(sky.horizon_color),
            aspect: camera.viewport_width_px / camera.viewport_height_px,
            horizon_angle: horizon_angle(sky),
            night: 1.0,
            ground_night: 1.0,
            ground_color: rgba(sky.horizon_color),
        }
    }
}

/// The full-screen sky pass: one pipeline, one uniform, three generated
/// vertices. Built lazily, the first frame that draws a sky.
pub struct SkyPipeline {
    pipeline: wgpu::RenderPipeline,
    uniform: super::alloc::TrackedBuffer,
    bind_group: wgpu::BindGroup,
}

impl SkyPipeline {
    /// `depth_format` is the tile pass's: this draws into the same pass.
    #[must_use]
    pub fn new(ctx: &super::GpuContext, color_format: wgpu::TextureFormat, depth_format: wgpu::TextureFormat) -> Self {
        let shader = ctx.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("gpui-maps sky shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("sky_shader.wgsl").into()),
        });
        let bgl = ctx.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("sky-bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let uniform = super::alloc::TrackedBuffer::new(
            &ctx.device,
            &wgpu::BufferDescriptor {
                label: Some("sky-uniform"),
                size: std::mem::size_of::<SkyUniform>() as wgpu::BufferAddress,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            },
        );
        let bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("sky-bind-group"),
            layout: &bgl,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: uniform.as_entire_binding() }],
        });
        let layout = ctx.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("sky-pipeline-layout"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let pipeline = ctx.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("sky-pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            // Drawn BEFORE anything that writes depth, so there is nothing to
            // test against; occlusion is draw order, as for the stars and the
            // ocean sphere. Writing nothing keeps the tiles' `Clear(1.0)`
            // intact for every later pass.
            depth_stencil: Some(wgpu::DepthStencilState {
                format: depth_format,
                depth_write_enabled: Some(false),
                depth_compare: Some(wgpu::CompareFunction::Always),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: color_format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        Self { pipeline, uniform, bind_group }
    }

    /// Rewrites the uniform: one `DMat4::inverse` and a 160-byte write.
    ///
    /// `ground` is what a ray that hits the planet paints: the style's
    /// background colour at this zoom, `SpacePlan::ground` -- see the module
    /// doc's "The ground below the limb".
    pub fn write(
        &self,
        ctx: &super::GpuContext,
        camera: &Camera,
        frame: &GlobeFrame,
        sky: &SkyFrame,
        atmosphere: &Atmosphere,
        ground: crate::style::Rgba,
    ) {
        ctx.queue.write_buffer(&self.uniform, 0, bytemuck::bytes_of(&SkyUniform::new(camera, frame, sky, atmosphere, ground)));
    }

    /// [`SkyUniform::flat`]: the eye camera's sky on a flat frame.
    pub fn write_flat(&self, ctx: &super::GpuContext, camera: &Camera, sky: &SkyFrame) {
        ctx.queue.write_buffer(&self.uniform, 0, bytemuck::bytes_of(&SkyUniform::flat(camera, sky)));
    }

    /// Draws into an already-open pass: after the stars, before the ocean
    /// sphere and the tiles.
    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::{Rgba, Sky};

    /// The eye camera's flat sky: the shader's "angle above the limb" is
    /// the angle above level within the stand-in's 0.36-degree dip, so the
    /// gradient starts at the horizon line -- 42% from the top at pitch 85
    /// -- whatever the bearing, and the screen's top edge is sky.
    #[test]
    fn the_flat_sky_puts_its_horizon_at_level() {
        let sky = SkyFrame { sky_color: Rgba { r: 0.0, g: 0.0, b: 0.1, a: 1.0 }, horizon_color: Rgba { r: 0.2, g: 0.2, b: 0.3, a: 1.0 }, ..Sky::default().at_zoom(20.0) };
        for bearing in [0.0f32, 1.0, 3.5] {
            let cam = Camera { center_lon: 103.86, center_lat: 1.29, zoom: 22.0, pitch: 85f32.to_radians(), bearing, viewport_width_px: 1024.0, viewport_height_px: 768.0, fov_y: Camera::DEFAULT_FOV_Y };
            let u = SkyUniform::flat(&cam, &sky);
            let inv = glam::Mat4::from_cols_array_2d(&u.inverse_view);
            let eye = glam::Vec3::from(u.camera_sphere);
            // The shader's elevation for the ray through NDC `y` (up).
            let elevation = |y: f32| {
                let ray = (inv * glam::Vec4::new(0.0, y * u.tan_half_fov_y, -1.0, 0.0)).truncate().normalize();
                let theta = ray.dot(-eye / eye.length()).clamp(-1.0, 1.0).acos();
                theta - (1.0 / eye.length()).asin()
            };
            // Level is 5 degrees above the centre: NDC tan(5)/tan(30).
            let level = 5f32.to_radians().tan() / u.tan_half_fov_y;
            assert!((elevation(level) - (2.0 * FLAT_EYE_ALTITUDE).sqrt()).abs() < 2e-3, "{}", elevation(level));
            assert!(elevation(level - 0.05) < 0.0 && elevation(1.0) > 0.4, "below level is ground, the top is sky");
            assert!(((1.0 - level) / 2.0 - 0.42).abs() < 0.01, "the horizon {:.3} from the top", (1.0 - level) / 2.0);
        }
    }

    fn camera(zoom: f64, pitch_deg: f32) -> Camera {
        Camera {
            center_lon: -90.0,
            center_lat: 29.0,
            zoom,
            pitch: pitch_deg.to_radians(),
            bearing: 0.0,
            viewport_width_px: 683.0,
            viewport_height_px: 529.0,
            fov_y: Camera::DEFAULT_FOV_Y,
        }
    }

    /// A frame that sees no above-limb pixel submits no pass, at any zoom:
    /// pitch 0 in and out of the band, a low tilt, and a Mercator frame at
    /// the steepest tilt.
    #[test]
    fn a_frame_that_sees_no_sky_submits_no_pass() {
        let sky = SkyFrame::default();
        for (zoom, pitch) in [(6.5, 0.0), (8.0, 20.0), (5.5, 0.0), (14.0, 60.0), (12.0, 60.0)] {
            let camera = camera(zoom, pitch);
            let frame = crate::render::globe::globe_frame(&camera);
            let above = sees_above_horizon(&camera, &frame);
            assert!(!above, "z{zoom} pitch {pitch}: no pixel looks above the limb");
            assert!(!draws_sky(&sky, frame.space_transition, above), "z{zoom} pitch {pitch}");
        }
        assert_eq!(sky_alpha(f32::NAN), 0.0);
    }

    /// **THE FLAT BACKGROUND ABOVE THE CURVED HORIZON.** The user's browser
    /// screenshot: z6.48, tilted near the limit, over the US Gulf coast, the
    /// Earth's curved horizon on screen and everything above it the style
    /// background `#F4F1EF`. The projection is spherical up to z11, so the
    /// limb dips below the horizontal; the sky must draw there -- and at
    /// full coverage, since `space_transition` is 0 from z6 up.
    #[test]
    fn the_sky_draws_above_the_curved_horizon_past_the_globe_band() {
        let sky = SkyFrame::default();
        for zoom in [6.0, 6.48, 7.0, 8.0, 10.0] {
            let camera = camera(zoom, 60.0);
            let frame = crate::render::globe::globe_frame(&camera);
            assert_eq!(frame.space_transition, 0.0, "z{zoom} is past the globe band");
            let above = sees_above_horizon(&camera, &frame);
            assert!(above, "z{zoom} at MAX_PITCH: the curved horizon is on screen, so sky is above it");
            assert!(draws_sky(&sky, frame.space_transition, above), "z{zoom}: the sky pass must draw");
            assert_eq!(sky_alpha(frame.space_transition), 1.0, "z{zoom}: the whole sky, not a fade");
        }
    }

    /// The reference's `mix(sky, transparent, projectionTransition)`: all
    /// sky from z6 up, none at full globe.
    #[test]
    fn the_sky_fades_out_toward_space_with_the_transition() {
        assert_eq!(sky_alpha(0.0), 1.0);
        assert!((sky_alpha(0.25) - 0.75).abs() < 1e-6);
        assert_eq!(sky_alpha(1.0), 0.0);
        let sky = SkyFrame::default();
        assert!(draws_sky(&sky, 0.5, true));
        assert!(!draws_sky(&sky, 0.5, false), "nothing above the limb, nothing drawn");
        assert!(!draws_sky(&sky, 1.0, true), "nothing of the sky is left at full globe");
        let clear = Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 };
        let transparent = SkyFrame { sky_color: clear, horizon_color: clear, ..sky };
        assert!(!draws_sky(&transparent, 0.5, true), "MapLibre's own 'no sky' draws nothing");
    }

    /// **One terminator.** The sky and the ground turn to night through the
    /// same ramp: at every sun angle across the terminator the sky's
    /// normalised night weight equals the ground's, at every zoom of the
    /// band -- only the DEPTH differs (the sky goes to space, the ground to
    /// its floor).
    #[test]
    fn the_sky_and_the_ground_turn_to_night_through_one_terminator() {
        let nb = crate::render::atmosphere::Atmosphere::TERMINATOR_NIGHT_BRIGHTNESS;
        let sky_floor = sky_night_floor(nb);
        assert!(sky_floor < 1.0 / 255.0, "the default night sky is space, not a dim day sky: {sky_floor}");
        assert_eq!(sky_night_floor(1.0), 1.0, "night_brightness 1.0 still switches the night off");
        for zoom in [3.2, 4.0, 4.5, 5.0, 5.5] {
            let t = crate::render::globe::space_transition_for_zoom(zoom);
            let ground_floor = crate::render::atmosphere::night_floor(nb, t);
            for i in -20..=20 {
                let cz = i as f32 * 0.01;
                let ground = crate::render::atmosphere::night_shade(ground_floor, cz);
                let sky = day_weight(sky_floor, cz);
                let ground_night = (1.0 - ground) / (1.0 - ground_floor);
                let sky_night = (1.0 - sky) / (1.0 - sky_floor);
                assert!(
                    (ground_night - sky_night).abs() < 1e-5,
                    "z{zoom} cos_zenith {cz}: the ground is {ground_night} of the way to night, the sky {sky_night}"
                );
            }
        }
    }

    /// By day the sky hides the stars as soon as it has a quarter of its
    /// coverage; by night it hides none; with no coverage it hides none.
    #[test]
    fn the_sky_hides_the_stars_by_day_and_only_by_day() {
        assert_eq!(star_occlusion(0.5, 1.0), 1.0, "z4.5 day: no star shows through the blue");
        assert_eq!(star_occlusion(1.0, 1.0), 1.0);
        assert_eq!(star_occlusion(0.5, 0.0), 0.0, "night: every star shows");
        assert_eq!(star_occlusion(0.0, 1.0), 0.0, "no sky: every star shows");
        assert!((star_occlusion(0.1, 1.0) - 0.4).abs() < 1e-6, "they come back continuously toward z3");
    }

    /// The Rust mirrors hold the shader's numbers.
    #[test]
    fn the_cpu_mirrors_carry_the_shaders_constants() {
        let source = include_str!("sky_shader.wgsl");
        let constant = |name: &str| -> f32 {
            let line = source
                .lines()
                .find(|l| l.trim_start().starts_with(&format!("const {name}")))
                .unwrap_or_else(|| panic!("sky_shader.wgsl declares {name}"));
            line.split('=').nth(1).unwrap().trim().trim_end_matches(';').parse().unwrap()
        };
        assert_eq!(constant("TWILIGHT_REACH_SIN"), TWILIGHT_REACH_SIN);
        assert_eq!(constant("TWILIGHT_STRENGTH"), TWILIGHT_STRENGTH);
        assert_eq!(constant("STAR_OCCLUSION_GAIN"), STAR_OCCLUSION_GAIN);
        assert_eq!(constant("TERMINATOR_HALF_WIDTH_SIN"), crate::render::atmosphere::TERMINATOR_HALF_WIDTH_SIN);
    }

    /// `sky.fragment.glsl`'s gradient: horizon colour at the limb, sky
    /// colour from `sky-horizon-blend` of half the FOV upward, and the
    /// squared falloff between.
    #[test]
    fn the_gradient_runs_from_the_horizon_colour_at_the_limb_to_the_sky_colour() {
        let sky = SkyFrame::default();
        assert_eq!(gradient(&sky, 0.0, 1.0), sky.horizon_color);
        assert_eq!(gradient(&sky, -0.1, 1.0), sky.horizon_color, "below the limb: horizon colour");
        let reach = horizon_angle(&sky);
        assert_eq!(gradient(&sky, reach, 1.0), sky.sky_color);
        assert_eq!(gradient(&sky, 2.0 * reach, 1.0), sky.sky_color);
        let half = gradient(&sky, 0.5 * reach, 1.0);
        let expected = {
            use crate::style::Lerp;
            sky.sky_color.lerp(&sky.horizon_color, 0.25)
        };
        assert!((half.b - expected.b).abs() < 1e-6 && (half.r - expected.r).abs() < 1e-6, "{half:?}");
        let flat = SkyFrame { sky_horizon_blend: 0.0, ..sky };
        assert_eq!(gradient(&flat, 0.0, 1.0), sky.sky_color, "sky-horizon-blend 0: sky colour only");
        // Half faded: the horizon is half way to the sky colour, so it is
        // BLUE over space rather than grey.
        let faded = gradient(&sky, 0.0, 0.5);
        assert!(faded.b > faded.r + 0.1, "{faded:?}");
    }

    #[test]
    fn the_uniform_is_160_bytes_and_a_multiple_of_16() {
        assert_eq!(std::mem::size_of::<SkyUniform>(), 160);
        assert_eq!(std::mem::offset_of!(SkyUniform, camera_sphere), 64);
        assert_eq!(std::mem::offset_of!(SkyUniform, sun_dir), 80);
        assert_eq!(std::mem::offset_of!(SkyUniform, sky_color), 96);
        assert_eq!(std::mem::offset_of!(SkyUniform, horizon_color), 112);
        assert_eq!(std::mem::offset_of!(SkyUniform, aspect), 128);
    }

    /// One inter-stage slot, no vertex input: the sky spends nothing of the
    /// tile shader's iOS budget and has its own of 15.
    #[test]
    fn the_sky_shader_has_one_varying_and_no_vertex_input() {
        let source = include_str!("sky_shader.wgsl");
        let output = source.split_once("struct VertexOutput").expect("declares VertexOutput").1;
        let output = output.split_once('}').expect("closed").0;
        assert_eq!(output.matches("@location(").count(), 1, "{output}");
        assert!(!source.contains("struct VertexInput"));
    }

    /// The flat-frame claim the module doc rests on: at the steepest tilt
    /// the top edge of a MERCATOR view is exactly horizontal, never above it.
    #[test]
    fn at_max_pitch_the_top_of_a_flat_view_is_the_horizon_not_the_sky() {
        let top = f64::from(crate::camera_control::MAX_PITCH) + super::super::camera::FOV_Y_RADIANS_F64 / 2.0;
        assert!(top <= std::f64::consts::FRAC_PI_2 + 1e-6, "top ray {} deg from nadir", top.to_degrees());
    }

    /// The spec's defaults are MapLibre's; a style with NO sky gets ours,
    /// which differ in the horizon colour alone.
    #[test]
    fn the_spec_sky_is_maplibres_and_ours_differs_only_in_the_horizon() {
        let spec = Sky::spec().at_zoom(5.0);
        assert_eq!(spec.sky_color, crate::style::SKY_COLOR_DEFAULT);
        assert_eq!(spec.horizon_color, Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 });
        assert_eq!(spec.fog_color, Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 });
        assert_eq!(
            (spec.fog_ground_blend, spec.horizon_fog_blend, spec.sky_horizon_blend, spec.atmosphere_blend),
            (0.5, 0.8, 0.8, 0.8)
        );
        let ours = Sky::default().at_zoom(5.0);
        assert_eq!(ours, SkyFrame { horizon_color: crate::style::HORIZON_COLOR_OURS, ..spec });
        let h = ours.horizon_color;
        assert!(h.b - h.r > 0.15, "our horizon is a pale BLUE, not white or grey: {h:?}");
    }
}

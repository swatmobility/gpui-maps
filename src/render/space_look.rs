//! **The space look**: the globe as a planet in space -- a star and Milky
//! Way sky behind it, white cloud swirls over it, and sun shading with a
//! thin blue rim. Opt-in, and OFF by default: every map still opens plain.
//!
//! # What it draws, and where in the frame
//!
//! Two full-screen passes from one shader (`space_look_shader.wgsl`), both
//! inside the tile pass, neither testing nor writing depth:
//!
//!  1. **The sky** ([`SpaceLookPipeline::draw_sky`]), FIRST -- before the
//!     BSC point stars, the style's sky, the ocean sphere and the tiles. An
//!     equirectangular star map looked up along each pixel's ray in RA/Dec,
//!     added to the space clear. The point stars draw over it for sparkle.
//!  2. **The globe dressing** ([`SpaceLookPipeline::draw_globe`]), after the
//!     tiles and before the limb glow: an analytic ray/sphere hit gives each
//!     pixel's ground point, where the clouds are sampled and the sun
//!     shading is applied, in one premultiplied blend.
//!
//! Both are ours. MapLibre GL JS has no star field, clouds or sphere
//! shading; nothing here comes from any Mapbox GL JS tree.
//!
//! # The frame: the BSC stars', turned by the sidereal angle
//!
//! The directions are `render::sky`'s sphere space -- the unrotated planet
//! frame the BSC catalogue already lives in (`space::sky_direction`), so a
//! texel of the star map and the catalogue star at the same RA/Dec land on
//! the same pixel. Under a scene clock both are turned by Greenwich mean
//! sidereal time together ([`super::space::sky_view_proj_turned`]), so the
//! sky is the real one for the instant the sun is computed for.
//!
//! # Assets: not in the repository
//!
//! The textures are NASA imagery fetched on first use into
//! `~/.cache/gpui-map/space/` and verified by pinned sha256
//! (`crate::space_assets`, native only). This module takes decoded RGBA
//! and never touches the network or the disk, so it compiles for the web
//! and iOS shells too; a shell with no textures gets the plain look (the
//! passes are simply not submitted, or sample a 1x1 black stand-in).
//!
//!  - **Stars + Milky Way**: NASA/Goddard Space Flight Center Scientific
//!    Visualization Studio, *Deep Star Maps 2020* (svs.gsfc.nasa.gov/4851),
//!    Gaia DR2 data by ESA/Gaia/DPAC. See [`STARMAP_CREDIT`].
//!  - **Clouds**: NASA Visible Earth, *Blue Marble: Clouds*
//!    (visibleearth.nasa.gov/images/57747), NASA Goddard Space Flight
//!    Center image by Reto Stöckli. See [`CLOUDS_CREDIT`].

use super::atmosphere::Sun;
use super::globe::GlobeFrame;
use super::Camera;

/// The on-map credit the star map owes while it is drawn.
///
/// The credit SVS lists for Deep Star Maps 2020, in full: "NASA/Goddard
/// Space Flight Center Scientific Visualization Studio. Gaia DR2:
/// ESA/Gaia/DPAC." (Commons' terms: use for any purpose "provided that the
/// copyright holder is properly attributed").
pub const STARMAP_CREDIT: &str =
    "Stars: NASA/Goddard Space Flight Center Scientific Visualization Studio. Gaia DR2: ESA/Gaia/DPAC";
/// The on-map credit the cloud layer owes while it is drawn.
pub const CLOUDS_CREDIT: &str = "Clouds: NASA Blue Marble (R. Stöckli, NASA GSFC)";

/// The space look's settings. [`Default`] is OFF -- a map opens plain.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpaceLook {
    /// The master switch. Off draws nothing and loads nothing.
    pub enabled: bool,
    /// Gain on the star-map texture; `0.0` skips the sky pass.
    pub milky_way: f32,
    /// Cloud opacity at globe zooms (faded out by [`look_fade`]).
    pub clouds: f32,
    /// How much of the sun shading applies (faded out by [`look_fade`]).
    pub shading: f32,
    /// Strength of the thin blue rim.
    pub rim: f32,
    /// The BSC point stars' intensity while the look is on (the larger of
    /// this and `Space::star_intensity` is drawn).
    pub star_intensity: f32,
}

// Not derivable, whatever clippy says: a derive would zero the tuned
// strengths that `..Self::ON` keeps for when the look is switched on.
#[allow(clippy::derivable_impls)]
impl Default for SpaceLook {
    fn default() -> Self {
        Self {
            enabled: false,
            ..Self::ON
        }
    }
}

impl SpaceLook {
    /// The look switched on at its tuned defaults.
    pub const ON: Self = Self {
        enabled: true,
        milky_way: 0.6,
        clouds: 0.95,
        shading: 1.0,
        rim: 0.8,
        star_intensity: 0.8,
    };

    /// Whether anything of the look draws on a frame of this globeness.
    #[must_use]
    pub fn draws(&self, transition: f32) -> bool {
        self.enabled && transition > 0.0
    }
}

/// The night side's floor under the look's shading: near black, as the
/// reference's far limb is.
pub const LOOK_NIGHT_FLOOR: f32 = 0.05;

/// The sun the look shades with when the frame's own sun is the flat
/// [`super::atmosphere::DAYLIGHT_SUN`] (terminator off): up and to the left
/// of the eye, so the ball reads as lit from one side and rolls to dark on
/// the lower right, as a planet photographed from space does.
pub const VIEWPORT_SUN: Sun = Sun::Viewport {
    tilt_deg: 55.0,
    bearing_deg: -40.0,
};

/// The old name of [`VIEWPORT_SUN`].
#[deprecated(since = "0.7.1", note = "renamed to `VIEWPORT_SUN`")]
pub const STUDIO_SUN: Sun = VIEWPORT_SUN;

/// The sun the look shades with: the frame's own, unless that is the
/// straight-on daylight sun of a terminator-off map, which lights nothing
/// directionally -- then [`VIEWPORT_SUN`].
#[must_use]
pub fn look_sun(frame_sun: Sun) -> Sun {
    if frame_sun == super::atmosphere::DAYLIGHT_SUN {
        VIEWPORT_SUN
    } else {
        frame_sun
    }
}

/// How much of the clouds, the shading and the rim a frame at `zoom` gets:
/// all of it to z3, none from z5, smooth between -- so a street map is
/// never clouded or dimmed.
#[must_use]
pub fn look_fade(zoom: f64) -> f32 {
    let t = ((zoom - 3.0) / 2.0).clamp(0.0, 1.0) as f32;
    1.0 - t * t * (3.0 - 2.0 * t)
}

/// An equirectangular RGBA8 image: longitude (or RA) across, latitude (or
/// declination) down, north at the top.
#[derive(Clone, PartialEq, Eq)]
pub struct EquirectImage {
    pub width: u32,
    pub height: u32,
    /// `width * height * 4` bytes, straight RGBA.
    pub rgba: Vec<u8>,
}

impl std::fmt::Debug for EquirectImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "EquirectImage({}x{})", self.width, self.height)
    }
}

impl EquirectImage {
    /// Halved by 2x2 box filtering until both sides are at most `max_side`
    /// -- what keeps a 3840-wide star map inside WebGL2's 2048 limit without
    /// an image crate.
    #[must_use]
    pub fn fit_to(mut self, max_side: u32) -> Self {
        let max_side = max_side.max(1);
        while self.width > max_side || self.height > max_side {
            let (w, h) = ((self.width / 2).max(1), (self.height / 2).max(1));
            let mut out = vec![0u8; (w * h * 4) as usize];
            for y in 0..h {
                for x in 0..w {
                    for c in 0..4 {
                        let px = |xx: u32, yy: u32| {
                            let (xx, yy) = (xx.min(self.width - 1), yy.min(self.height - 1));
                            u32::from(self.rgba[((yy * self.width + xx) * 4 + c) as usize])
                        };
                        let sum = px(2 * x, 2 * y)
                            + px(2 * x + 1, 2 * y)
                            + px(2 * x, 2 * y + 1)
                            + px(2 * x + 1, 2 * y + 1);
                        out[((y * w + x) * 4 + c) as usize] = ((sum + 2) / 4) as u8;
                    }
                }
            }
            self = Self {
                width: w,
                height: h,
                rgba: out,
            };
        }
        self
    }
}

/// The decoded textures a look can draw. Either may be missing (offline,
/// a failed checksum, a shell with no fetcher): that half is not drawn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpaceTextures {
    pub starmap: Option<EquirectImage>,
    pub clouds: Option<EquirectImage>,
}

/// The credits owed by a frame of globeness `transition` at `zoom`, for a
/// look whose pipeline holds a star map (`has_starmap`) and a cloud map
/// (`has_clouds`): only what that frame actually DRAWS. The star credit while
/// the sky pass draws; the cloud credit while the faded cloud opacity
/// ([`look_fade`]) is above zero. In the order they join the attribution.
#[must_use]
pub fn look_credits(
    has_starmap: bool,
    has_clouds: bool,
    look: &SpaceLook,
    transition: f32,
    zoom: f64,
) -> Vec<&'static str> {
    let mut out = Vec::new();
    if !look.draws(transition) {
        return out;
    }
    if has_starmap && look.milky_way > 0.0 {
        out.push(STARMAP_CREDIT);
    }
    if has_clouds && look.clouds.clamp(0.0, 1.0) * look_fade(zoom) > 0.0 {
        out.push(CLOUDS_CREDIT);
    }
    out
}

/// What `space_look_shader.wgsl`'s `Look` struct expects. 128 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct LookUniform {
    inverse_view: [[f32; 4]; 4],
    camera_sphere: [f32; 3],
    tan_half_fov_y: f32,
    sun_dir: [f32; 3],
    aspect: f32,
    milky_way: f32,
    clouds: f32,
    shading: f32,
    sidereal: f32,
    rim: f32,
    night: f32,
    _pad: [f32; 2],
}

impl LookUniform {
    pub(crate) fn new(
        camera: &Camera,
        frame: &GlobeFrame,
        look: &SpaceLook,
        sun: Sun,
        sidereal_rad: f64,
    ) -> Self {
        let inverse_view = frame.globe_view.inverse();
        let eye = inverse_view.transform_point3(glam::DVec3::ZERO);
        let sun = look_sun(sun).direction_sphere(&inverse_view);
        let fade = look_fade(camera.zoom);
        Self {
            inverse_view: inverse_view.as_mat4().to_cols_array_2d(),
            camera_sphere: [eye.x as f32, eye.y as f32, eye.z as f32],
            tan_half_fov_y: camera.tan_half_fov() as f32,
            sun_dir: [sun.x as f32, sun.y as f32, sun.z as f32],
            aspect: camera.viewport_width_px / camera.viewport_height_px.max(1.0),
            milky_way: look.milky_way.max(0.0),
            clouds: look.clouds.clamp(0.0, 1.0) * fade,
            shading: look.shading.clamp(0.0, 1.0) * fade,
            sidereal: sidereal_rad as f32,
            rim: look.rim.max(0.0) * fade,
            night: LOOK_NIGHT_FLOOR,
            _pad: [0.0; 2],
        }
    }
}

/// The two passes, their textures and their one uniform. Built lazily the
/// first frame the look draws.
pub struct SpaceLookPipeline {
    sky: wgpu::RenderPipeline,
    globe: wgpu::RenderPipeline,
    uniform: super::alloc::TrackedBuffer,
    bind_group: wgpu::BindGroup,
    has_starmap: bool,
    has_clouds: bool,
    /// Which texture set this pipeline was built from (the caller's counter
    /// or pointer); a different one rebuilds it. See [`Self::generation`].
    generation: u64,
    sky_on: std::cell::Cell<bool>,
    globe_on: std::cell::Cell<bool>,
    _textures: [wgpu::Texture; 2],
}

fn upload(ctx: &super::GpuContext, label: &str, image: Option<&EquirectImage>) -> wgpu::Texture {
    let max_side = ctx.device.limits().max_texture_dimension_2d;
    let black = EquirectImage {
        width: 1,
        height: 1,
        rgba: vec![0, 0, 0, 255],
    };
    let fitted;
    let image = match image {
        Some(image) if image.width > max_side || image.height > max_side => {
            fitted = image.clone().fit_to(max_side);
            &fitted
        }
        Some(image) => image,
        None => &black,
    };
    let size = wgpu::Extent3d {
        width: image.width,
        height: image.height,
        depth_or_array_layers: 1,
    };
    let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    ctx.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &image.rgba,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(4 * image.width),
            rows_per_image: Some(image.height),
        },
        size,
    );
    texture
}

impl SpaceLookPipeline {
    /// Builds both pipelines and uploads `textures` (each fitted to the
    /// device's texture limit; a missing one becomes a 1x1 black stand-in).
    #[must_use]
    pub fn new(
        ctx: &super::GpuContext,
        color_format: wgpu::TextureFormat,
        depth_format: wgpu::TextureFormat,
        textures: &SpaceTextures,
        generation: u64,
    ) -> Self {
        Self::from_source(ctx, include_str!("space_look_shader.wgsl"), color_format, depth_format, textures, generation)
    }

    /// [`Self::new`] from a given WGSL source (a test compiles a cache-cold
    /// copy to time the first-use compile).
    fn from_source(
        ctx: &super::GpuContext,
        source: &str,
        color_format: wgpu::TextureFormat,
        depth_format: wgpu::TextureFormat,
        textures: &SpaceTextures,
        generation: u64,
    ) -> Self {
        let shader = ctx
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("gpui-maps space look shader"),
                source: wgpu::ShaderSource::Wgsl(source.into()),
            });
        let texture_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let bgl = ctx
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("space-look-bgl"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    texture_entry(1),
                    texture_entry(2),
                    wgpu::BindGroupLayoutEntry {
                        binding: 3,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });
        let uniform = super::alloc::TrackedBuffer::new(
            &ctx.device,
            &wgpu::BufferDescriptor {
                label: Some("space-look-uniform"),
                size: std::mem::size_of::<LookUniform>() as wgpu::BufferAddress,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            },
        );
        let star_texture = upload(ctx, "space-look-starmap", textures.starmap.as_ref());
        let cloud_texture = upload(ctx, "space-look-clouds", textures.clouds.as_ref());
        let sampler = ctx.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("space-look-sampler"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let star_view = star_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let cloud_view = cloud_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("space-look-bind-group"),
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&star_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&cloud_view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });
        let layout = ctx
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("space-look-pipeline-layout"),
                bind_group_layouts: &[Some(&bgl)],
                immediate_size: 0,
            });
        let make = |label: &str, entry: &str| {
            ctx.device
                .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some(label),
                    layout: Some(&layout),
                    vertex: wgpu::VertexState {
                        module: &shader,
                        entry_point: Some("vs_main"),
                        compilation_options: wgpu::PipelineCompilationOptions::default(),
                        buffers: &[],
                    },
                    primitive: wgpu::PrimitiveState::default(),
                    // Like the sky and the stars: no depth test, no depth write;
                    // occlusion is draw order and the analytic sphere.
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
                        entry_point: Some(entry),
                        compilation_options: wgpu::PipelineCompilationOptions::default(),
                        targets: &[Some(wgpu::ColorTargetState {
                            format: color_format,
                            blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                            write_mask: wgpu::ColorWrites::ALL,
                        })],
                    }),
                    multiview_mask: None,
                    cache: None,
                })
        };
        let sky = make("space-look-sky", "fs_sky");
        let globe = make("space-look-globe", "fs_globe");
        Self {
            sky,
            globe,
            uniform,
            bind_group,
            has_starmap: textures.starmap.is_some(),
            has_clouds: textures.clouds.is_some(),
            generation,
            sky_on: std::cell::Cell::new(false),
            globe_on: std::cell::Cell::new(false),
            _textures: [star_texture, cloud_texture],
        }
    }

    /// The texture-set identity this pipeline was built with: a caller
    /// rebuilds when its own differs (identity, not mere presence, so a new
    /// texture set with the same halves present is still picked up).
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether a star map was uploaded (not the 1x1 stand-in).
    #[must_use]
    pub fn has_starmap(&self) -> bool {
        self.has_starmap
    }

    /// Whether a cloud map was uploaded.
    #[must_use]
    pub fn has_clouds(&self) -> bool {
        self.has_clouds
    }

    /// Rewrites the uniform and decides which of the two passes this frame
    /// submits: the sky when a star map is present and the look's gain is
    /// positive, the globe dressing while [`look_fade`] leaves anything of
    /// it. `sun` is the frame's resolved sun ([`look_sun`] picks the
    /// shading's), `sidereal_rad` the Greenwich sidereal angle.
    pub fn write(
        &self,
        ctx: &super::GpuContext,
        camera: &Camera,
        frame: &GlobeFrame,
        look: &SpaceLook,
        sun: Sun,
        sidereal_rad: f64,
    ) {
        let uniform = LookUniform::new(camera, frame, look, sun, sidereal_rad);
        let on = look.draws(frame.space_transition);
        self.sky_on
            .set(on && self.has_starmap && uniform.milky_way > 0.0);
        self.globe_on.set(
            on && (uniform.shading > 0.0
                || (self.has_clouds && uniform.clouds > 0.0)
                || uniform.rim > 0.0),
        );
        ctx.queue
            .write_buffer(&self.uniform, 0, bytemuck::bytes_of(&uniform));
    }

    /// The star-map sky, FIRST in the pass. Returns the draw calls issued.
    pub fn draw_sky(&self, pass: &mut wgpu::RenderPass<'_>) -> u64 {
        if !self.sky_on.get() {
            return 0;
        }
        pass.set_pipeline(&self.sky);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.draw(0..3, 0..1);
        1
    }

    /// Clouds, shading and rim, after the tiles. Returns the draw calls
    /// issued.
    pub fn draw_globe(&self, pass: &mut wgpu::RenderPass<'_>) -> u64 {
        if !self.globe_on.get() {
            return 0;
        }
        pass.set_pipeline(&self.globe);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.draw(0..3, 0..1);
        1
    }
}

/// What one frame of the look costs on the GPU, from [`measure_frame_cost`].
#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
pub struct FrameCost {
    /// A clear-only pass, ms/frame (median batch).
    pub clear_ms: f64,
    /// The same pass with the sky and globe passes, ms/frame (median batch).
    pub look_ms: f64,
    /// The sky pass alone, ms/frame.
    pub sky_ms: f64,
    /// The globe pass alone, ms/frame.
    pub globe_ms: f64,
    /// Look draw calls issued across every batch: the proof it painted.
    pub draws: u64,
}

/// **The perf gate's measurement of the look** (`look.*` rows, and the
/// `space_look_cost` investigation test): GPU ms/frame of one pass that
/// only clears a `width`x`height` target, against the same pass drawing
/// `look` as written for a camera at `zoom` (lat 20, lon 100, the viewport
/// sun). Four arms -- clear, both passes, sky, globe -- in INTERLEAVED
/// batches of `frames` submits with one device wait per batch (the
/// ground-shading test's method: a wait per frame times the round trip),
/// after one untimed batch each; `reps` batches per arm, the median kept.
#[doc(hidden)]
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub fn measure_frame_cost(
    ctx: &super::GpuContext,
    look: &SpaceLookPipeline,
    color: wgpu::TextureFormat,
    depth: wgpu::TextureFormat,
    (width, height): (u32, u32),
    zoom: f64,
    reps: usize,
    frames: u32,
) -> FrameCost {
    let target = |format, label| {
        ctx.device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            })
            .create_view(&wgpu::TextureViewDescriptor::default())
    };
    let color_view = target(color, "look-cost-color");
    let depth_view = target(depth, "look-cost-depth");
    let camera = Camera {
        center_lon: 100.0,
        center_lat: 20.0,
        zoom,
        pitch: 0.0,
        bearing: 0.0,
        viewport_width_px: width as f32,
        viewport_height_px: height as f32,
        fov_y: Camera::DEFAULT_FOV_Y,
    };
    let frame = super::globe::globe_frame(&camera);
    look.write(ctx, &camera, &frame, &SpaceLook::ON, VIEWPORT_SUN, 1.0);
    let mut draws = 0u64;
    // Arm 0 clear only, 1 sky + globe, 2 sky, 3 globe.
    let mut batch = |arm: usize, frames: u32| -> f64 {
        let t = crate::timing::Instant::now();
        for _ in 0..frames {
            let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("look-cost"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &color_view,
                        resolve_target: None,
                        depth_slice: None,
                        ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                    })],
                    depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                        view: &depth_view,
                        depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Discard }),
                        stencil_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(0), store: wgpu::StoreOp::Discard }),
                    }),
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                if matches!(arm, 1 | 2) {
                    draws += look.draw_sky(&mut pass);
                }
                if matches!(arm, 1 | 3) {
                    draws += look.draw_globe(&mut pass);
                }
            }
            ctx.queue.submit(Some(encoder.finish()));
        }
        let _ = ctx.device.poll(wgpu::PollType::wait_indefinitely());
        t.elapsed().as_secs_f64() * 1e3 / f64::from(frames)
    };
    for arm in 0..4 {
        batch(arm, frames.min(5));
    }
    let mut runs: Vec<Vec<_>> = (0..4).map(|_| Vec::with_capacity(reps)).collect();
    for _ in 0..reps.max(1) {
        for (arm, r) in runs.iter_mut().enumerate() {
            r.push(batch(arm, frames));
        }
    }
    let median = |r: &mut Vec<f64>| {
        r.sort_by(f64::total_cmp);
        r[r.len() / 2]
    };
    let m: Vec<f64> = runs.iter_mut().map(median).collect();
    FrameCost { clear_ms: m[0], look_ms: m[1], sky_ms: m[2], globe_ms: m[3], draws }
}

/// Greenwich mean sidereal angle for the look's sky at `unix_s`, radians;
/// `0.0` for a frame with no clock (the BSC stars' own ground-fixed frame).
#[must_use]
pub fn sidereal_for(unix_s: Option<f64>) -> f64 {
    unix_s
        .filter(|t| t.is_finite())
        .map_or(0.0, super::altitude::gmst_rad)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_look_is_off_by_default_and_draws_nothing_on_a_flat_frame() {
        let look = SpaceLook::default();
        assert!(!look.enabled);
        assert!(!look.draws(1.0));
        assert!(!SpaceLook::ON.draws(0.0), "a flat frame gets no look");
        assert!(SpaceLook::ON.draws(1.0));
    }

    #[test]
    fn clouds_and_shading_fade_out_by_zoom_five() {
        assert_eq!(look_fade(0.0), 1.0);
        assert_eq!(look_fade(3.0), 1.0);
        assert!((look_fade(4.0) - 0.5).abs() < 1e-6);
        assert_eq!(look_fade(5.0), 0.0);
        assert_eq!(look_fade(14.0), 0.0);
    }

    #[test]
    fn the_uniform_is_128_bytes() {
        assert_eq!(std::mem::size_of::<LookUniform>(), 128);
    }

    #[test]
    fn credits_join_only_for_what_is_drawn() {
        let on = SpaceLook::ON;
        assert_eq!(
            look_credits(true, true, &on, 1.0, 1.5),
            vec![STARMAP_CREDIT, CLOUDS_CREDIT]
        );
        assert!(
            look_credits(true, true, &SpaceLook::default(), 1.0, 1.5).is_empty(),
            "look off"
        );
        assert!(
            look_credits(true, true, &on, 0.0, 10.0).is_empty(),
            "a flat frame draws neither"
        );
        assert_eq!(
            look_credits(false, true, &on, 1.0, 1.5),
            vec![CLOUDS_CREDIT],
            "no star map loaded"
        );
        // z5.5 is still a (tilted) globe frame for the sky, but the clouds
        // have faded out: only the star credit.
        assert_eq!(
            look_credits(true, true, &on, 0.2, 5.5),
            vec![STARMAP_CREDIT]
        );
        let no_clouds = SpaceLook { clouds: 0.0, ..on };
        assert_eq!(
            look_credits(true, true, &no_clouds, 1.0, 1.5),
            vec![STARMAP_CREDIT]
        );
    }

    #[test]
    fn fit_to_halves_until_it_fits_and_averages() {
        let image = EquirectImage {
            width: 4,
            height: 2,
            rgba: (0..32).map(|i| (i * 8) as u8).collect(),
        };
        let fitted = image.fit_to(2);
        assert_eq!((fitted.width, fitted.height), (2, 1));
        // Top-left 2x2 block, channel 0: pixels (0,0),(1,0),(0,1),(1,1).
        let expect = (32 + 128 + 160 + 2) / 4; // (0,0) is 0
        assert_eq!(u32::from(fitted.rgba[0]), expect);
    }

    #[test]
    fn a_terminator_off_frame_is_shaded_by_the_studio_sun() {
        assert_eq!(look_sun(super::super::atmosphere::DAYLIGHT_SUN), VIEWPORT_SUN);
        let real = Sun::Map {
            lon_deg: 10.0,
            lat_deg: 5.0,
        };
        assert_eq!(look_sun(real), real);
    }

    #[test]
    fn no_clock_means_no_sidereal_turn() {
        assert_eq!(sidereal_for(None), 0.0);
        assert_eq!(sidereal_for(Some(f64::NAN)), 0.0);
        assert!(sidereal_for(Some(1_790_697_600.0)) > 0.0);
    }

    /// **What the look costs, measured** (the space-look lag
    /// investigation): GPU ms/frame of a clear-only pass against the same
    /// pass with the look's sky + globe passes, at 1x/2x/5K surfaces and
    /// z1.5-z6, with the REAL cached textures and with 1x1 stand-ins; and
    /// the CPU cost of building the pipeline (the first-use upload).
    /// Batches interleaved, one wait per batch (the ground-shading test's
    /// method). Needs `~/.cache/gpui-map/space/` populated.
    ///
    /// `cargo test --release --lib space_look_cost -- --ignored --nocapture`
    #[test]
    #[ignore = "timing: cargo test --release --lib space_look_cost -- --ignored --nocapture"]
    fn space_look_cost() {
        use crate::render::GpuContext;
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let t = crate::timing::Instant::now();
            let real = crate::space_assets::load_cached_textures();
            println!("space_look_cost: decode (background thread in the app) {:.1} ms", t.elapsed().as_secs_f64() * 1e3);
            assert!(real.starmap.is_some() && real.clouds.is_some(), "populate ~/.cache/gpui-map/space first");
            let color = wgpu::TextureFormat::Bgra8Unorm;
            let depth = wgpu::TextureFormat::Depth24PlusStencil8;
            // First-use build: shader, two pipelines, ~37 MB upload -- on the
            // frame thread in `paint`. Timed to the device idle.
            // Warm the device as the app's is by the time the look first
            // draws (the tile pipeline compiled), then compile a cache-COLD
            // copy of the look's shader (a nonce comment defeats Metal's
            // on-disk cache): the first-use stall on a warm device.
            let t = crate::timing::Instant::now();
            drop(crate::render::TilePipeline::new(&ctx, color));
            println!("space_look_cost: tile pipeline (device warm-up) {:.2} ms", t.elapsed().as_secs_f64() * 1e3);
            let nonce = (crate::render::solar::now_unix_seconds().unwrap_or(0.0) * 1e9) as u128;
            let cold = format!("{}\n// nonce {nonce}\n", include_str!("space_look_shader.wgsl"));
            let t = crate::timing::Instant::now();
            drop(SpaceLookPipeline::from_source(&ctx, &cold, color, depth, &SpaceTextures::default(), 0));
            println!("space_look_cost: new(1x1) COLD shader, warm device, cpu {:.2} ms", t.elapsed().as_secs_f64() * 1e3);
            let t = crate::timing::Instant::now();
            drop(SpaceLookPipeline::new(&ctx, color, depth, &SpaceTextures::default(), 0));
            println!("space_look_cost: new(1x1) shipped shader cpu {:.2} ms", t.elapsed().as_secs_f64() * 1e3);
            for (label, textures) in [("real", &real), ("real again", &real), ("stars only", &SpaceTextures { starmap: real.starmap.clone(), clouds: None })] {
                let t = crate::timing::Instant::now();
                let p = SpaceLookPipeline::new(&ctx, color, depth, textures, 1);
                let cpu = t.elapsed().as_secs_f64() * 1e3;
                let _ = ctx.device.poll(wgpu::PollType::wait_indefinitely());
                println!("space_look_cost: new({label}) cpu {cpu:.2} ms, to idle {:.2} ms", t.elapsed().as_secs_f64() * 1e3);
                drop(p);
            }
            let t = crate::timing::Instant::now();
            let stand_in = SpaceLookPipeline::new(&ctx, color, depth, &SpaceTextures::default(), 0);
            println!("space_look_cost: new(1x1) cpu {:.2} ms", t.elapsed().as_secs_f64() * 1e3);
            let real_look = SpaceLookPipeline::new(&ctx, color, depth, &real, 1);
            let mut draws = 0u64;
            for size in [(1728u32, 1117u32), (3456, 2234), (5120, 2880)] {
                for zoom in [1.5, 3.0, 4.5, 6.0] {
                    let r = measure_frame_cost(&ctx, &real_look, color, depth, size, zoom, 7, 20);
                    let s = measure_frame_cost(&ctx, &stand_in, color, depth, size, zoom, 7, 20);
                    draws += r.draws + s.draws;
                    println!(
                        "space_look_cost: {}x{} z{zoom}: clear {:.3} ms | +look real {:+.3} | +look 1x1 {:+.3} | +sky real {:+.3} | +globe real {:+.3} ms (sky_on {} globe_on {})",
                        size.0,
                        size.1,
                        r.clear_ms,
                        r.look_ms - r.clear_ms,
                        s.look_ms - s.clear_ms,
                        r.sky_ms - r.clear_ms,
                        r.globe_ms - r.clear_ms,
                        real_look.sky_on.get(),
                        real_look.globe_on.get(),
                    );
                }
            }
            // The per-frame CPU side: the uniform and its 128-byte write.
            let camera = Camera { center_lon: 100.0, center_lat: 20.0, zoom: 1.5, pitch: 0.0, bearing: 0.0, viewport_width_px: 3456.0, viewport_height_px: 2234.0, fov_y: Camera::DEFAULT_FOV_Y };
            let frame = crate::render::globe::globe_frame(&camera);
            let t = crate::timing::Instant::now();
            for i in 0..10_000 {
                real_look.write(&ctx, &camera, &frame, &SpaceLook::ON, VIEWPORT_SUN, f64::from(i) * 1e-4);
            }
            println!("space_look_cost: write() per frame {:.2} us", t.elapsed().as_secs_f64() * 1e6 / 10_000.0);
            assert!(draws > 0, "no look draw was issued: nothing was timed");
            println!("space_look_cost: look draws issued {draws}");
        });
    }

    /// The shader's RA convention agrees with the BSC catalogue's: a star
    /// at RA/Dec sits in `space::sky_direction(ra, dec)`, whose
    /// `atan2(x, z)` is the RA the shader reads back.
    #[test]
    fn the_star_map_frame_is_the_catalogue_frame() {
        for (ra, dec) in [(0.0, 0.0), (90.0, 10.0), (266.4, -28.9), (101.3, -16.7)] {
            let d = super::super::space::sky_direction(ra, dec);
            let back = d.x.atan2(d.z).to_degrees().rem_euclid(360.0);
            assert!((back - ra).abs() < 1e-9, "{ra}: {back}");
            assert!((d.y.asin().to_degrees() - dec).abs() < 1e-9);
        }
    }
}

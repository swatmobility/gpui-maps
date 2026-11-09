//! Navigation on the GPU: the route line as one more mesh in the tile pass,
//! and the chrome (banner, strip, buttons, puck) in a pass of its own over
//! everything else. Shared by all three shells; each owns one of each and
//! calls them from its frame. Neither exists until a trip does, which keeps
//! a frame without navigation identical to one from before it -- same
//! draws, same passes, same buffers.
//!
//! The route line is drawn exactly like an overlay source: tessellated at
//! `mesh::OVERLAY_COORD_SCALE` in its origin's local grid, placed with
//! `combined_overlay_matrix`, and projected onto the globe like the tiles
//! under it (`GlobeUpload::for_overlay`, cut on `Subdivision::OVERLAY`'s
//! grid by `render::tessellate_overlay_plan`). Navigation mostly runs at
//! z16, where the globe transition is zero and that path is the flat one
//! bit for bit, but an overview zoomed out past z12 is on the sphere.

use crate::nav::chrome::{ChromeGlyph, ChromeScene};
use crate::nav::icons;
use crate::nav::route_layer::RouteLayer;
use crate::render::alloc::TrackedBuffer;
use crate::render::globe::GlobeUpload;
use crate::render::{Camera, GlobeParams, GpuContext, IconInstance, IconPipeline, PlateInstance, PlatePipeline, TileBuffers, TilePipeline};

/// The route mesh and its model uniform.
pub struct RouteMeshGpu {
    buffers: Option<TileBuffers>,
    model_buffer: TrackedBuffer,
    model_bind_group: wgpu::BindGroup,
    generation: Option<u64>,
    origin_world: (f64, f64),
}

impl RouteMeshGpu {
    #[must_use]
    pub fn new(ctx: &GpuContext, pipeline: &TilePipeline) -> Self {
        let (model_buffer, model_bind_group) =
            pipeline.create_model_bind_group(ctx, glam::Mat4::IDENTITY.to_cols_array_2d(), 0.0, 0.0, GlobeParams::FLAT);
        Self { buffers: None, model_buffer, model_bind_group, generation: None, origin_world: (0.0, 0.0) }
    }

    /// Re-tessellates when `layer` changed and writes this frame's matrix.
    ///
    /// `globe` is the frame's `GlobeUpload`, the one its tiles were drawn
    /// with, so the line sits on the same sphere they do.
    pub fn sync(&mut self, ctx: &GpuContext, pipeline: &TilePipeline, layer: &RouteLayer, camera: &Camera, globe: &GlobeUpload) {
        if self.generation != Some(layer.generation()) {
            let mesh = crate::render::tessellate_overlay_plan(layer.plan(), layer.coord_scale());
            self.buffers = if mesh.indices.is_empty() { None } else { Some(pipeline.install_mesh(ctx, self.buffers.take(), &mesh)) };
            self.generation = Some(layer.generation());
            self.origin_world = layer.origin_world();
        }
        let matrix = crate::render::combined_overlay_matrix(camera, self.origin_world).to_cols_array_2d();
        pipeline.write_model_uniform(
            ctx,
            &self.model_buffer,
            matrix,
            0.0,
            crate::render::overlay_dash_scale(camera),
            globe.for_overlay(self.origin_world),
        );
    }

    /// The draw to append LAST to the tile pass's list: over every basemap
    /// tile, under the symbol pass's labels.
    #[must_use]
    pub fn draw(&self) -> Option<(&TileBuffers, &wgpu::BindGroup)> {
        self.buffers.as_ref().map(|b| (b, &self.model_bind_group))
    }
}

/// Keeps `slot` in step with `layer`: created on the first frame of a trip,
/// dropped when it ends.
pub fn sync_route(
    slot: &mut Option<RouteMeshGpu>,
    ctx: &GpuContext,
    pipeline: &TilePipeline,
    layer: Option<&RouteLayer>,
    camera: &Camera,
    globe: &GlobeUpload,
) {
    match layer {
        None => *slot = None,
        Some(layer) => slot.get_or_insert_with(|| RouteMeshGpu::new(ctx, pipeline)).sync(ctx, pipeline, layer, camera, globe),
    }
}

/// The chrome pass over the finished frame, in four layers: the puck, then
/// plates, then the chrome's own icons, then text. The puck is map content,
/// so a banner or strip it slides under covers it.
///
/// The puck has an `IconPipeline` of its own because a pipeline keeps ONE
/// vertex buffer and `queue.write_buffer` lands before any draw runs: a
/// second `render_batches` call on the same pipeline, after the plates,
/// would overwrite the puck's quads (see `IconPipeline::render_batches`).
pub struct NavChromeGpu {
    plates: PlatePipeline,
    icons: IconPipeline,
    icon_atlas: wgpu::BindGroup,
    puck: IconPipeline,
    puck_atlas: wgpu::BindGroup,
    _text_cache: glyphon::Cache,
    viewport: glyphon::Viewport,
    text_atlas: glyphon::TextAtlas,
    text: glyphon::TextRenderer,
    swash: glyphon::SwashCache,
    shapes: crate::symbol_pass::LabelShapeCache,
}

impl NavChromeGpu {
    /// `format` is the colour target's; `ColorMode::Web` like every other
    /// glyphon atlas in the engine (the targets are `Unorm`).
    #[must_use]
    pub fn new(ctx: &GpuContext, format: wgpu::TextureFormat) -> Self {
        let icons = IconPipeline::new(ctx, format);
        let atlas = icons::atlas();
        let icon_atlas = icons.upload_atlas(ctx, &atlas);
        let puck = IconPipeline::new(ctx, format);
        let puck_atlas = puck.upload_atlas(ctx, &atlas);
        let text_cache = glyphon::Cache::new(&ctx.device);
        let viewport = glyphon::Viewport::new(&ctx.device, &text_cache);
        let mut text_atlas = glyphon::TextAtlas::with_color_mode(&ctx.device, &ctx.queue, &text_cache, format, glyphon::ColorMode::Web);
        let text = glyphon::TextRenderer::new(&mut text_atlas, &ctx.device, wgpu::MultisampleState::default(), None);
        Self {
            plates: PlatePipeline::new(ctx, format),
            icons,
            icon_atlas,
            puck,
            puck_atlas,
            _text_cache: text_cache,
            viewport,
            text_atlas,
            text,
            swash: glyphon::SwashCache::new(),
            shapes: crate::symbol_pass::LabelShapeCache::new(),
        }
    }

    /// Draws `scene` over `view` in its own pass. `physical` is the target's
    /// size in pixels, `scale` the device pixel ratio. Without a
    /// `font_system` (the browser before its font arrives) the text is
    /// skipped and the rest still draws. An empty scene opens no pass.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        ctx: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        scene: &ChromeScene,
        font_system: Option<&mut glyphon::FontSystem>,
        physical: (u32, u32),
        scale: f32,
    ) {
        if scene.is_empty() {
            return;
        }
        let plates: Vec<PlateInstance> = scene
            .plates
            .iter()
            .map(|p| PlateInstance {
                screen_x: p.rect.x * scale,
                screen_y: p.rect.y * scale,
                width: p.rect.w * scale,
                height: p.rect.h * scale,
                radius_px: p.radius * scale,
                color: p.color,
            })
            .collect();
        let (puck_instances, icon_instances): (Vec<IconInstance>, Vec<IconInstance>) = scene
            .icons
            .iter()
            .map(|i| {
                let (uv_min, uv_max) = icons::uv(i.glyph);
                IconInstance {
                    screen_x: i.rect.x * scale,
                    screen_y: i.rect.y * scale,
                    width: i.rect.w * scale,
                    height: i.rect.h * scale,
                    uv_min,
                    uv_max,
                    rotation_rad: i.rotation_rad,
                    sdf: None,
                    opacity: 1.0,
                }
            })
            .zip(scene.icons.iter())
            .fold((Vec::new(), Vec::new()), |(mut puck, mut rest), (instance, icon)| {
                if matches!(icon.glyph, ChromeGlyph::Puck { .. }) { puck.push(instance) } else { rest.push(instance) }
                (puck, rest)
            });
        let text_prepared = match font_system {
            Some(font_system) if !scene.texts.is_empty() => self.prepare_text(ctx, font_system, scene, physical, scale),
            _ => false,
        };
        let (w, h) = (physical.0 as f32, physical.1 as f32);
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("nav-chrome-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            self.puck.render_batches(ctx, &mut pass, &[(&self.puck_atlas, puck_instances.as_slice())], w, h);
            self.plates.render(ctx, &mut pass, &plates, w, h);
            self.icons.render_batches(ctx, &mut pass, &[(&self.icon_atlas, icon_instances.as_slice())], w, h);
            if text_prepared {
                let _ = self.text.render(&self.text_atlas, &self.viewport, &mut pass);
            }
        }
        self.text_atlas.trim();
        self.shapes.end_frame();
    }

    fn prepare_text(&mut self, ctx: &GpuContext, font_system: &mut glyphon::FontSystem, scene: &ChromeScene, physical: (u32, u32), scale: f32) -> bool {
        let face = crate::symbol_pass::chrome_text_face();
        let justify = crate::overlay::paint::TextJustify::Left;
        let mut keyed = Vec::with_capacity(scene.texts.len());
        for text in &scene.texts {
            let size = text.size * scale;
            let wrap = text.wrap_width.map(|w| w * scale);
            self.shapes.get_or_shape(font_system, &text.text, size, wrap, justify, &face);
            keyed.push((crate::symbol_pass::LabelShapeCache::key(&text.text, size, wrap, justify, &face), text));
        }
        self.viewport.update(&ctx.queue, glyphon::Resolution { width: physical.0, height: physical.1 });
        let screen = glyphon::TextBounds { left: 0, top: 0, right: physical.0 as i32, bottom: physical.1 as i32 };
        let areas: Vec<glyphon::TextArea> = keyed
            .iter()
            .filter_map(|(key, text)| {
                let shaped = self.shapes.get(key)?;
                let [r, g, b, a] = text.color.map(|c| (c.clamp(0.0, 1.0) * 255.0).round() as u8);
                Some(glyphon::TextArea {
                    buffer: &shaped.buffer,
                    left: text.left * scale,
                    top: text.top * scale,
                    scale: 1.0,
                    // Clipped to its plate, so no line of it lands on the map.
                    bounds: text.clip.map_or(screen, |c| glyphon::TextBounds {
                        left: (c.x * scale).floor() as i32,
                        top: (c.y * scale).floor() as i32,
                        right: ((c.x + c.w) * scale).ceil() as i32,
                        bottom: ((c.y + c.h) * scale).ceil() as i32,
                    }),
                    default_color: glyphon::Color::rgba(r, g, b, a),
                    custom_glyphs: &[],
                })
            })
            .collect();
        self.text
            .prepare(&ctx.device, &ctx.queue, font_system, &mut self.text_atlas, &self.viewport, areas, &mut self.swash)
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nav::chrome::{ChromeIcon, ChromePlate, Rect};

    const SIZE: (u32, u32) = (128, 64);

    fn target(ctx: &GpuContext, size: (u32, u32)) -> wgpu::Texture {
        ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("nav-layer-test"),
            size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        })
    }

    /// Submits `encoder` and reads `texture` back as tightly packed RGBA
    /// (every width used here is a multiple of 64, so rows need no padding).
    fn read_back(ctx: &GpuContext, mut encoder: wgpu::CommandEncoder, texture: &wgpu::Texture, size: (u32, u32)) -> Vec<u8> {
        let bytes_per_row = size.0 * 4;
        // Through `TrackedBuffer`, not a raw buffer: every allocation in the
        // crate is counted, and
        // `every_buffer_creation_in_the_crate_goes_through_the_tracker`
        // scans for the ones that are not.
        let readback = TrackedBuffer::new(
            &ctx.device,
            &wgpu::BufferDescriptor {
                label: None,
                size: u64::from(bytes_per_row * size.1),
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            },
        );
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo {
                buffer: &*readback,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(bytes_per_row), rows_per_image: Some(size.1) },
            },
            wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
        );
        ctx.queue.submit(Some(encoder.finish()));
        let slice = readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| tx.send(result).unwrap());
        ctx.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        rx.recv().unwrap().unwrap();
        slice.get_mapped_range().expect("mapped after poll").to_vec()
    }

    fn render_scene(scene: &ChromeScene) -> Vec<u8> {
        let ctx = pollster::block_on(GpuContext::new_headless()).expect("headless device");
        let texture = target(&ctx, SIZE);
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            // Clear to transparent first: the chrome pass LOADS.
            let _clear = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }
        let mut chrome = NavChromeGpu::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
        chrome.render(&ctx, &mut encoder, &view, scene, None, SIZE, 1.0);
        read_back(&ctx, encoder, &texture, SIZE)
    }

    fn pixel(data: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
        let at = ((y * width + x) * 4) as usize;
        [data[at], data[at + 1], data[at + 2], data[at + 3]]
    }

    #[test]
    fn a_plate_and_an_icon_land_where_the_scene_puts_them() {
        let scene = ChromeScene {
            plates: vec![ChromePlate { rect: Rect { x: 8.0, y: 8.0, w: 48.0, h: 48.0 }, radius: 6.0, color: [1.0, 0.0, 0.0, 1.0] }],
            icons: vec![ChromeIcon { rect: Rect { x: 72.0, y: 8.0, w: 48.0, h: 48.0 }, glyph: ChromeGlyph::Puck { muted: false }, rotation_rad: 0.0 }],
            ..ChromeScene::default()
        };
        let data = render_scene(&scene);
        let inside = pixel(&data, SIZE.0, 32, 32);
        assert!(inside[0] > 240 && inside[1] < 16 && inside[3] > 240, "the plate is red: {inside:?}");
        assert_eq!(pixel(&data, SIZE.0, 2, 2)[3], 0, "outside the plate nothing is drawn");
        let puck = pixel(&data, SIZE.0, 96, 32);
        assert!(puck[3] > 200, "the puck icon drew: {puck:?}");
    }

    #[test]
    fn a_plate_covers_the_puck_but_not_the_chromes_own_icons() {
        // The puck goes FIRST in `scene.icons` (as `NavHost::scene` puts
        // it), which a single icon batch drew after every plate. Both icons
        // sit on an opaque red plate: the puck must be hidden under it, the
        // close glyph (white) must show over it.
        let plate = Rect { x: 0.0, y: 0.0, w: 128.0, h: 64.0 };
        let scene = ChromeScene {
            plates: vec![ChromePlate { rect: plate, radius: 0.0, color: [1.0, 0.0, 0.0, 1.0] }],
            icons: vec![
                ChromeIcon { rect: Rect { x: 8.0, y: 8.0, w: 48.0, h: 48.0 }, glyph: ChromeGlyph::Puck { muted: false }, rotation_rad: 0.0 },
                ChromeIcon { rect: Rect { x: 72.0, y: 8.0, w: 48.0, h: 48.0 }, glyph: ChromeGlyph::Close, rotation_rad: 0.0 },
            ],
            ..ChromeScene::default()
        };
        let data = render_scene(&scene);
        let under = pixel(&data, SIZE.0, 32, 32);
        assert!(under[0] > 240 && under[1] < 16 && under[2] < 16, "the plate hides the puck: {under:?}");
        let close = pixel(&data, SIZE.0, 96, 32);
        assert!(close[1] > 200 && close[2] > 200, "the chrome's own icon draws over its plate: {close:?}");
    }

    #[test]
    fn an_empty_scene_touches_no_pixel() {
        let data = render_scene(&ChromeScene::default());
        assert!(data.chunks(4).all(|p| p[3] == 0));
    }

    #[test]
    fn the_route_line_draws_blue_ahead_of_the_puck_and_grey_behind_it() {
        use crate::nav::format::WallClock;
        use crate::nav::route_layer::{DRIVEN_COLOR, REMAINING_COLOR};
        use crate::nav::route_source::parse_route;
        use crate::nav::simulate::SimOptions;
        use crate::nav::trip::{NavTrip, TripClock};

        const ROUTE_SIZE: (u32, u32) = (256, 256);
        let clock = TripClock { now_ms: 0.0, wall: WallClock { unix_ms: 0.0, utc_offset_min: 480 } };
        let mut trip = NavTrip::simulated(
            parse_route(include_bytes!("../../tests/fixtures/routes/manila.json")).unwrap(),
            SimOptions { start_at_m: Some(1_200.0), paused: true, ..SimOptions::default() },
            clock,
        )
        .unwrap();
        trip.advance(clock);
        let route = trip.session().route();
        let mut layer = RouteLayer::new(route, 0);
        layer.update(route, 0, trip.state());
        let puck = trip.state().puck.unwrap();
        let camera = Camera {
            center_lon: puck.lon,
            center_lat: puck.lat,
            zoom: 16.0,
            pitch: 0.0,
            bearing: 0.0,
            viewport_width_px: ROUTE_SIZE.0 as f32,
            viewport_height_px: ROUTE_SIZE.1 as f32,
            fov_y: Camera::DEFAULT_FOV_Y,
        };

        let ctx = pollster::block_on(GpuContext::new_headless()).expect("headless device");
        let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
        let (_camera_buffer, camera_bind_group) =
            pipeline.create_camera_bind_group(&ctx, camera.view_projection().to_cols_array_2d(), camera.zoom as f32, 1.0);
        let texture = target(&ctx, ROUTE_SIZE);
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let depth = pipeline.create_depth_view(&ctx, ROUTE_SIZE.0, ROUTE_SIZE.1);
        let mut slot = None;
        // z16: `transition` is 0, so this is the flat path the line always took.
        let globe = GlobeUpload::new(&crate::render::globe::globe_frame(&camera), &camera, 4096.0, glam::DVec3::ZERO, 1.0);
        sync_route(&mut slot, &ctx, &pipeline, Some(&layer), &camera, &globe);
        let mesh = slot.as_ref().expect("a trip creates the mesh");
        let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        let draws: Vec<_> = mesh.draw().into_iter().collect();
        pipeline.render(&mut encoder, &view, &depth, wgpu::Color::TRANSPARENT, &camera_bind_group, &draws, None);
        let data = read_back(&ctx, encoder, &texture, ROUTE_SIZE);

        let near = |got: [u8; 4], want: crate::style::Rgba| {
            [want.r, want.g, want.b].iter().zip(got).all(|(w, g)| (f32::from(g) - w * 255.0).abs() <= 20.0) && got[3] > 240
        };
        let sample = |at_m: f64| {
            let (x, y) = crate::render::project_world_to_screen_f64(&camera, layer.world_at(at_m)).expect("on screen");
            assert!((0.0..ROUTE_SIZE.0 as f32).contains(&x) && (0.0..ROUTE_SIZE.1 as f32).contains(&y), "({x}, {y})");
            pixel(&data, ROUTE_SIZE.0, x.round() as u32, y.round() as u32)
        };
        let ahead = sample(layer.split_m() + 40.0);
        assert!(near(ahead, REMAINING_COLOR), "40 m ahead of the puck is the remaining blue: {ahead:?}");
        let behind = sample(layer.split_m() - 40.0);
        assert!(near(behind, DRIVEN_COLOR), "40 m behind it is the driven grey: {behind:?}");
        assert_eq!(pixel(&data, ROUTE_SIZE.0, 2, 2)[3], 0, "off the line nothing is drawn");
        sync_route(&mut slot, &ctx, &pipeline, None, &camera, &globe);
        assert!(slot.is_none(), "the trip's end drops the mesh");
    }
}

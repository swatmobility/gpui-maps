//! The GPU half of overlay extrusion ([`crate::overlay::extrude`]): each
//! synthetic tile's mesh uploaded with
//! [`crate::render::TilePipeline::create_extrusion_buffers`], a model
//! uniform per tile, and one [`ExtrusionParamsGpu`] slot for the layer --
//! then drawn as `TileDraw::Extrusion`, through the basemap's own
//! extrusion pipelines, after the basemap's buildings in the tile pass.
//! `MapView::paint` and `capture` both draw through this.

use crate::overlay::extrude::{frame_params, overlay_stencil_ref, ExtrusionStyle, OverlayExtrusion};
use crate::render::extrusion::ExtrusionPass;
use crate::render::extrusion_gpu::{ExtrusionGpu, ExtrusionParamsGpu};
use crate::render::{Camera, GlobeParams, GpuContext, TileDraw, TilePipeline};

struct TileGpu {
    tile: crate::geo::TileId,
    gpu: ExtrusionGpu,
    model: crate::render::alloc::TrackedBuffer,
    model_bind_group: wgpu::BindGroup,
}

/// One overlay extrusion layer on the GPU.
pub struct OverlayExtrusionGpu {
    tiles: Vec<TileGpu>,
    params: ExtrusionParamsGpu,
    translucent: bool,
    /// [`OverlayExtrusion::facade_metres_per_unit`]: 0 draws no facade.
    facade_metres_per_unit: f32,
}

impl OverlayExtrusionGpu {
    /// Uploads every tile of `set`.
    #[must_use]
    pub fn upload(ctx: &GpuContext, pipeline: &TilePipeline, set: &OverlayExtrusion) -> Self {
        let tiles = set
            .tiles
            .iter()
            .filter_map(|t| {
                let gpu = pipeline.create_extrusion_buffers(ctx, &t.mesh)?;
                let (model, model_bind_group) = pipeline.create_model_bind_group(ctx, glam::Mat4::IDENTITY.to_cols_array_2d(), 0.0, 1.0, GlobeParams::FLAT);
                Some(TileGpu { tile: t.tile, gpu, model, model_bind_group })
            })
            .collect();
        Self { tiles, params: ExtrusionParamsGpu::new(), translucent: false, facade_metres_per_unit: set.facade_metres_per_unit() }
    }

    /// Bytes held on the device.
    #[must_use]
    pub fn device_bytes(&self) -> u64 {
        self.tiles.iter().map(|t| t.gpu.device_bytes()).sum()
    }

    /// This frame's uniforms: each tile's matrix for `camera` and, on a
    /// globe frame (`globe` with a transition above zero), the tile's
    /// globe uniform -- the same `GlobeUpload::for_tile` the basemap tile
    /// at that id gets, so the column stands where the ground under it is
    /// drawn -- and the layer's light, height scale and opacity. `None`
    /// (or a flat upload) draws Mercator exactly as before the globe arm.
    pub fn prepare(
        &mut self,
        ctx: &GpuContext,
        pipeline: &TilePipeline,
        camera: &Camera,
        globe: Option<&crate::render::globe::GlobeUpload>,
        style: &ExtrusionStyle,
        light: &crate::style::ExtrusionLight,
    ) {
        let globe = globe.filter(|g| g.transition() > 0.0);
        for t in &self.tiles {
            let m = crate::render::combined_tile_matrix(camera, t.tile, crate::render::extrusion::EXTENT as f32).to_cols_array_2d();
            let dash = crate::render::dash_scale(camera, t.tile, crate::render::extrusion::EXTENT as f32);
            let params = globe.map_or(GlobeParams::FLAT, |g| g.for_tile(t.tile));
            pipeline.write_model_uniform(ctx, &t.model, m, 0.0, dash, params);
        }
        let mut params = frame_params(style, light, camera.center_lat, f64::from(camera.bearing), [camera.viewport_width_px, camera.viewport_height_px]);
        if globe.is_some() {
            let night = params.globe_light[3];
            params.globe_light = crate::render::extrusion::globe_light(params.light_dir, camera.center_lon, camera.center_lat);
            params.globe_light[3] = night;
        }
        params.viewport[2] = self.facade_metres_per_unit;
        // An overlay's own haze (`ExtrusionLook::haze_color`): the band a
        // style layer gets (`frame_plan::extrusion_frame_depth`).
        if let Some(c) = style.look.haze_color {
            params.haze_color = [c.r, c.g, c.b, 1.0];
            let d = camera.distance();
            if let Some([start, end]) = style.look.haze_range_m.filter(|_| style.look.haze > 0.0) {
                params.bands[0] = start * params.meters_to_world;
                params.bands[1] = end.max(start + 1.0) * params.meters_to_world;
            } else if style.look.haze > 0.0 && d.is_finite() {
                params.bands[0] = d as f32;
                params.bands[1] = (d * crate::render::frame_plan::HAZE_END_DISTANCES) as f32;
            }
        }
        self.translucent = params.opacity < 1.0;
        self.params.write(ctx, pipeline.extrusion(), &[params]);
    }

    /// Appends the layer's draws: one opaque pass, or MapLibre's
    /// translucent pair (a depth prepass over every tile, then the
    /// stencilled colour pass) with `overlay_stencil_ref(layer)`.
    pub fn push_draws<'a>(&'a self, layer: usize, out: &mut Vec<TileDraw<'a>>) {
        let passes: &[ExtrusionPass] = if self.translucent {
            &[ExtrusionPass::Prepass, ExtrusionPass::Color { stencil_ref: overlay_stencil_ref(layer) }]
        } else {
            &[ExtrusionPass::Opaque]
        };
        for &pass in passes {
            for t in &self.tiles {
                for chunk in &t.gpu.chunks {
                    for span in chunk.spans.iter().filter(|s| s.index_count > 0) {
                        out.push(TileDraw::Extrusion {
                            chunk,
                            model: &t.model_bind_group,
                            params: self.params.bind_group(0),
                            first_index: span.first_index,
                            index_count: span.index_count,
                            pass,
                        });
                    }
                }
            }
        }
    }
}

use crate::render::{TileMesh, Vertex};

pub struct GpuContext {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    /// The `max_buffer_size` this device was actually GRANTED -- read back
    /// off the device rather than off the adapter, because those are not
    /// necessarily the same number and the difference is the whole point
    /// (see [`Self::new`]).
    ///
    /// Cached at construction because [`TilePipeline::install_mesh`] consults
    /// it on every install, and it cannot change for the life of the device:
    /// `request_device` grants "exactly the specified limits, and no better
    /// or worse".
    pub max_buffer_size: u64,
}

/// The limits to ask `request_device` for, given what the adapter OFFERS.
///
/// A pure function of `offered`, deliberately: it is the whole policy, it is
/// the thing that was wrong, and it is testable on a machine with no GPU.
///
/// # What was wrong
///
/// `wgpu::DeviceDescriptor::default()` asks for `wgpu::Limits::default()` --
/// the **WebGPU baseline**, the set a conforming implementation must offer.
/// That is not "what this adapter offers", and treating it as such was
/// simultaneously too greedy and too timid:
///
/// - **Too greedy.** The baseline's `max_inter_stage_shader_variables` is 16.
///   iOS Metal allows **15**, and `request_device` is rejected outright --
///   `Limit 'max_inter_stage_shader_variables' value 16 is better than
///   allowed 15` -- so the engine could not get a device on iOS **at all**.
///   `shader.wgsl` passes **11** (`VertexOutput` locations 0-10), well under
///   what iOS offers: the device was refused over headroom the
///   shaders never touch. WebGL2 fails the same way for its own reasons --
///   it reports **zero** for every compute limit in the baseline, and this
///   crate runs no compute.
/// - **Too timid.** The baseline's `max_buffer_size` is 256 MiB. The adapter
///   in an M4 Pro offers 4 GiB. The overlay bakes into one `TileMesh` and
///   therefore one vertex buffer, so the baseline was a hard 3,355,443-vertex
///   ceiling on everything drawn -- self-imposed at 1/16th of the hardware --
///   and crossing it killed the process (see [`TilePipeline::install_mesh`]).
///
/// One request fixes both, because both are the same mistake: **ask for what
/// the adapter has**, in every dimension, in both directions.
///
/// # Why the adapter's set verbatim, and not something cleverer
///
/// Two refinements were considered and rejected.
///
/// *Element-wise `min(baseline, offered)`* -- keeping the baseline as a
/// portability tripwire everywhere except `max_buffer_size` -- would need
/// this crate to enumerate all 57 fields of `wgpu::Limits` and re-audit them
/// on every wgpu bump, and it buys nothing the test suite does not already
/// buy: [`GpuContext::new_headless`] keeps `Limits::default()` **on purpose**,
/// so every test in this crate still runs against the portable floor. A 20th
/// vertex attribute still fails here exactly as it would on a weaker machine.
/// The tripwire lives in the test path, which is where it bites.
///
/// *Requesting zero for the compute, storage, ray-tracing and mesh-shader
/// limits* this crate never touches would be honest about what a render-only
/// crate needs, and wgpu does warn that asking for more than you need "may
/// cause performance to decrease". It was rejected as speculation with a real
/// downside: those limits are validation-only in practice, and pinning them
/// to zero would turn any future compute path into a device-creation puzzle
/// rather than a legible pipeline-creation error.
///
/// # The one thing this does NOT protect against
///
/// Following the adapter downwards means accepting a device that is below the
/// baseline in a dimension the shaders **do** use, and a device granting, say,
/// 4 inter-stage variables would break rendering in a way far worse than a
/// clean refusal. That is what [`unmet_shader_needs`] is for, and
/// [`GpuContext::new`] calls it before it calls `request_device`.
#[must_use]
pub fn required_limits(offered: &wgpu::Limits) -> wgpu::Limits {
    offered.clone()
}

/// Every limit in `granted` that falls short of what this crate's shaders and
/// pipelines actually consume, as human-readable lines -- empty when the
/// device can render.
///
/// The counterweight to [`required_limits`]' permissiveness. Following the
/// adapter is right, but only because the shortfall it might otherwise hide
/// is caught here, by name, before a device exists: a refusal reading
/// `max_vertex_attributes: needs 11, offers 4` is worth far more than a device
/// that silently draws the wrong thing, or nothing.
///
/// **Every number below is traced to the source that consumes it, and is the
/// real figure rather than a round one.** A minimum invented larger than the
/// truth refuses hardware that would have worked, which is the same class of
/// bug as the one [`required_limits`] exists to fix. `max_buffer_size` is
/// deliberately absent: no single value is "enough" for it, the ceiling is
/// wherever the hardware puts it, and [`TilePipeline::install_mesh`] already
/// refuses a mesh that will not fit without taking the process down.
/// What this crate's shaders and pipelines consume, as `(limit name, the
/// figure needed)` -- the `needed` column of [`unmet_shader_needs`], on its
/// own.
///
/// Exposed separately so a test can assert a declared figure against the thing
/// it is a figure FOR, rather than against another copy of the same literal.
/// Two of the thirteen are not constants at all but properties of a Rust type
/// or of a shader file, and both had, or could, silently drift: see
/// `the_model_uniform_is_16_byte_aligned_and_its_declared_need_matches_reality`
/// and `the_declared_inter_stage_need_matches_the_shader_it_is_declared_for`.
#[must_use]
pub fn declared_needs() -> Vec<(&'static str, u64)> {
    // The `needed` column is a property of this crate alone -- no entry reads
    // `granted` to decide what it needs -- so which limit set is passed here
    // cannot affect the answer.
    shader_needs(&wgpu::Limits::default())
        .iter()
        .map(|(name, needed, _)| (*name, *needed))
        .collect()
}

#[must_use]
pub fn unmet_shader_needs(granted: &wgpu::Limits) -> Vec<String> {
    shader_needs(granted)
        .iter()
        .filter(|(_, needed, offered)| offered < needed)
        .map(|(name, needed, offered)| format!("{name}: needs {needed}, offers {offered}"))
        .collect()
}

/// The single table behind both [`declared_needs`] and
/// [`unmet_shader_needs`], as `(name, needed, offered)`.
///
/// One table rather than two, so the name a shortfall is reported under and
/// the name a test looks a figure up by cannot disagree.
fn shader_needs(granted: &wgpu::Limits) -> [(&'static str, u64, u64); 13] {
    let vertex_stride = std::mem::size_of::<Vertex>() as u64;
    [
        // `shader.wgsl`'s `VertexOutput` fills locations 0-10 -- ELEVEN slots,
        // counted off the struct itself by
        // `the_declared_inter_stage_need_matches_the_shader_it_is_declared_for`,
        // which is what keeps this figure honest. It said 8 for as long as
        // `aa_inset_px` (location 8) had existed, because nothing checked it
        // and no platform this crate targets offers fewer than 15.
        //
        // The tenth is `globe_light`, the ground shading's sun. RAISED to 10
        // rather than the shading being packed into a spare component of an
        // existing slot: 10 of iOS Metal's 15 and of the WebGL2 fallback's
        // 15 leaves five, and `every_platform_this_crate_targets_clears_the_shader_needs`
        // is green with no edits. A need is only ever raised to the truth
        // here -- lowering one to make that test pass is how an iOS crash
        // shipped once.
        //
        // Still a literal rather than a scan of `include_str!("shader.wgsl")`
        // done here: this runs on the device-creation path, where a parse that
        // could fail would be a far worse failure mode than a figure a test
        // pins. The `size_of` below can be derived directly and therefore is.
        //
        // The ELEVENTH is `tile_position`, the stand-in clip's in-tile
        // coordinate (see `TileClip`). RAISED to 11 by the same rule the
        // tenth was: a need is only ever raised to the truth here. 11 of
        // iOS Metal's 15 and of the WebGL2 fallback's 15 leaves four, and
        // `every_platform_this_crate_targets_clears_the_shader_needs` is
        // green with no edits.
        //
        // The TWELFTH is `line_progress`, MapLibre's `lineMetrics` progress,
        // which `fs_main` trims against `model.line_fx`. RAISED to 12 by the
        // same rule: 12 of iOS Metal's 15 and of the WebGL2 fallback's 15
        // leaves three, and
        // `every_platform_this_crate_targets_clears_the_shader_needs` is
        // green with no edits.
        //
        // This is the limit iOS Metal caps at 15 and the baseline asks 16 for.
        ("max_inter_stage_shader_variables", 12, u64::from(granted.max_inter_stage_shader_variables)),
        // `TilePipeline::from_source`'s `ATTRS`: locations 0-11, so TWELVE
        // (RAISED from 11 with `line_progress` at location 11).
        //
        // **This said 9, and it was wrong** -- the same understatement Task
        // 8 found in the inter-stage figure, in the same table, for the same
        // reason: `gap_half_px` and `gap_half_px_to` were inserted at
        // locations 7 and 8 when `line-gap-width` casings landed, pushing
        // `line_distance` and `dash_runs` to 9 and 10, and nothing counted.
        // Nothing caught it because no platform this crate targets offers
        // fewer than 16, so a need of 9 and a need of 11 are both met
        // everywhere -- but an understated need is the direction that SHIPS
        // A CRASH: a device reporting exactly 9 would have been accepted at
        // device creation and then failed pipeline creation.
        //
        // Counted off the shader's own `VertexInput` by
        // `the_declared_vertex_attribute_need_matches_the_shader_it_is_declared_for`,
        // so it cannot drift again.
        ("max_vertex_attributes", 12, u64::from(granted.max_vertex_attributes)),
        // One vertex buffer per pipeline. The overlay circle pipeline is
        // instanced and still has one: its quad comes from
        // `@builtin(vertex_index)`, the buffer is the instances. The dot
        // pipeline has none: it pulls its blocks from a texture.
        ("max_vertex_buffers", 1, u64::from(granted.max_vertex_buffers)),
        // `size_of::<Vertex>()`, the widest vertex struct in the crate --
        // 92 bytes (88 before `line_progress`), against `PlateVertex`'s 44, `IconInstance`'s and
        // `space::StarVertex`'s 28.
        ("max_vertex_buffer_array_stride", vertex_stride, u64::from(granted.max_vertex_buffer_array_stride)),
        // The tile pipeline's `camera_bgl` + `model_bgl`; the icon pipeline's
        // `atlas_bgl` + `viewport_bgl`.
        // The overlay circle pipeline adds a third: `circle_bgl` (as the
        // compact basemap pipelines add `tile_table_bgl`).
        ("max_bind_groups", 3, u64::from(granted.max_bind_groups)),
        // Those two groups plus the one vertex buffer they are drawn with.
        // RAISED from 3 to the truth with the dot pipeline: the circle
        // pipeline's three groups and one instance buffer were already
        // four (the dot pipeline's three groups and no buffer are three).
        ("max_bind_groups_plus_vertex_buffers", 4, u64::from(granted.max_bind_groups_plus_vertex_buffers)),
        // `icon_pipeline`'s `atlas_bgl`: a texture and a sampler. The dot
        // pipeline's `dot_bgl` has six: the ramp's paint table, the
        // `DotParams` uniform, the `DotBlock` texture, the dot mask, and
        // the composition and shape textures. RAISED from 3 with
        // relocation, and from 4 with the race dots (WebGL2's downlevel
        // limit is 1000).
        ("max_bindings_per_bind_group", 6, u64::from(granted.max_bindings_per_bind_group)),
        // Both of the tile pipeline's uniforms are VERTEX-visible.
        // Plus `vs_circle`'s `CircleParams`: three in the circle pipeline.
        ("max_uniform_buffers_per_shader_stage", 3, u64::from(granted.max_uniform_buffers_per_shader_stage)),
        // The largest uniform this crate binds, DERIVED rather than written
        // down: `ModelUniform` grew from 80 to 192 bytes when the globe's
        // matrix, clipping plane, tile Mercator coords and transition joined
        // it, and a literal 80 would have quietly outlived that.
        // `CameraUniform` is 112 since the circle frame, and the star field's
        // `StarUniform` is 96 -- both listed here anyway, so that a future
        // field on either cannot grow past `ModelUniform` without this
        // figure following it.
        (
            "max_uniform_buffer_binding_size",
            (std::mem::size_of::<CameraUniform>() as u64)
                .max(std::mem::size_of::<ModelUniform>() as u64)
                .max(std::mem::size_of::<crate::render::space::StarUniform>() as u64),
            granted.max_uniform_buffer_binding_size,
        ),
        // `icon_shader.wgsl`'s `icon_texture` and `icon_sampler`; and, in
        // the VERTEX stage, `shader.wgsl`'s `paint_table`, which `vs_compact`
        // reads with `textureLoad` (no sampler). `vs_dot` reads FIVE in the
        // vertex stage: the ramp's paint table, the `DotBlock` texture, the
        // dot mask, and the race dots' composition and shape textures.
        // RAISED from 2 with relocation and from 3 with the race dots
        // (WebGL2 allows 16).
        ("max_sampled_textures_per_shader_stage", 5, u64::from(granted.max_sampled_textures_per_shader_stage)),
        ("max_samplers_per_shader_stage", 1, u64::from(granted.max_samplers_per_shader_stage)),
        // `render::dots::DOT_TEXTURE_WIDTH` (2048): every dot tile's block
        // texture is that wide (`TilePipeline::create_dot_tile`). RAISED
        // from `pack_icon_atlas`'s `ATLAS_WIDTH` (1024) with the dot layer;
        // WebGL2's downlevel limit is exactly 2048. The window's own
        // backing store imposes no minimum -- `backing_store_size` clamps
        // itself to whatever `max_texture_dimension_2d` turns out to be.
        (
            "max_texture_dimension_2d",
            crate::render::dots::DOT_TEXTURE_WIDTH as u64,
            u64::from(granted.max_texture_dimension_2d),
        ),
        // Every pass in this crate writes one colour attachment.
        ("max_color_attachments", 1, u64::from(granted.max_color_attachments)),
    ]
}

impl GpuContext {
    /// A windowless device, used by this module's own test and reused by
    /// `map_view` when it creates the real device (the setup is identical;
    /// only the surface differs).
    ///
    /// **Not `#[cfg(test)]` any more**, and the argument below is why that
    /// costs nothing. It was test-only until `capture` — the headless
    /// real-style frame grab (`src/capture.rs`) — needed a device from an
    /// ordinary binary. The reasoning for the baseline limits is about
    /// WHICH device this hands back, not about who is allowed to ask: a
    /// capture taken at the WebGPU baseline is the frame a weak conforming
    /// device draws, which is the more useful frame for a diagnostic to
    /// grade, and it keeps every pixel this crate measures — test or
    /// capture — measured against one set of limits.
    pub async fn new_headless() -> anyhow::Result<Self> {
        Ok(Self::new_headless_with_info().await?.0)
    }

    /// [`Self::new_headless`], and the adapter's info (for a capture
    /// manifest's device name).
    ///
    /// # Errors
    /// See [`Self::new_headless`].
    pub async fn new_headless_with_info() -> anyhow::Result<(Self, wgpu::AdapterInfo)> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                ..Default::default()
            })
            .await
            .map_err(|e| anyhow::anyhow!("no wgpu adapter: {e}"))?;
        // DELIBERATELY the WebGPU baseline, unlike `new` below -- this is the
        // one call site that does NOT follow the adapter, and it is the
        // portability tripwire for the whole crate.
        //
        // Every test in this crate builds its device here, so every test runs
        // against `Limits::default()`: the floor a weak conforming device
        // guarantees. A test suite running against this machine's 4 GiB
        // buffer ceiling and 31 vertex attributes would stop being able to
        // fail the way a weaker machine fails, and a 20th vertex attribute
        // would then validate here and break on a laptop with nothing able to
        // catch it. wgpu's default panicking uncaptured-error handler is left
        // in place for the same reason: in a test a validation error should be
        // loud, not a line of stderr.
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor::default())
            .await
            .map_err(|e| anyhow::anyhow!("wgpu device request failed: {e}"))?;
        let max_buffer_size = device.limits().max_buffer_size;
        Ok((Self { device, queue, max_buffer_size }, adapter.get_info()))
    }

    /// The real device: asks the adapter for exactly what the adapter offers,
    /// and refuses cleanly if that is not enough to render.
    ///
    /// See [`required_limits`] for why the WebGPU baseline this used to
    /// request was both too greedy (no device at all on iOS or WebGL2) and
    /// too timid (a 256 MiB buffer ceiling on hardware offering 4 GiB), and
    /// [`unmet_shader_needs`] for the check that makes following the adapter
    /// downwards safe.
    pub async fn new(adapter: &wgpu::Adapter) -> anyhow::Result<Self> {
        let required_limits = required_limits(&adapter.limits());

        // REFUSE BEFORE THE DEVICE EXISTS, NOT AFTER IT DRAWS WRONG.
        //
        // `request_device` grants "exactly the specified limits, and no
        // better or worse", so the set asked for above is the set that will
        // bind -- which makes this check on the REQUEST equivalent to a check
        // on the grant, and available one step earlier, where the error can
        // still be a `Result` rather than a broken picture.
        //
        // Every shortfall is listed, not just the first: one is a hardware
        // generation this crate could plausibly support with a smaller
        // `VertexOutput`, a dozen is a platform that was never going to work,
        // and the difference is worth knowing from a single run.
        let shortfalls = unmet_shader_needs(&required_limits);
        if !shortfalls.is_empty() {
            let info = adapter.get_info();
            anyhow::bail!(
                "this GPU cannot run gpui-map's shaders -- {} ({:?}, {:?}) is short on {} limit(s): {}",
                info.name,
                info.backend,
                info.device_type,
                shortfalls.len(),
                shortfalls.join("; "),
            );
        }

        // Timestamp queries if the adapter has them, silence if it does not.
        //
        // Intersecting with what the adapter OFFERS is the same discipline
        // `required_limits` follows one line up, and for the same reason: this
        // engine runs on macOS, iOS, and wasm, and asking for a feature a
        // device lacks does not degrade the picture, it refuses the device.
        // An `&` makes the request satisfiable by construction on every
        // adapter, so `PassTimer` is the only place that has to know whether
        // the feature actually arrived -- see [`PassTimer::new`].
        //
        // TIMESTAMP_QUERY alone, deliberately: it is what
        // `RenderPassDescriptor::timestamp_writes` needs. The neighbouring
        // `TIMESTAMP_QUERY_INSIDE_ENCODERS`/`_PASSES` buy arbitrary
        // `write_timestamp` calls, which nothing here wants, and they are
        // considerably less widely supported.
        let required_features = adapter.features() & wgpu::Features::TIMESTAMP_QUERY;

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                required_limits,
                required_features,
                ..Default::default()
            })
            .await
            .map_err(|e| anyhow::anyhow!("wgpu device request failed: {e}"))?;

        // A wgpu validation error is not worth killing a window for.
        //
        // wgpu's default uncaptured-error handler PANICS, and on the real
        // device that panic is raised inside the AppKit/Metal callback that
        // drives a frame -- a frame Rust cannot unwind out of, so the runtime
        // gives up on the unwind entirely ("fatal runtime error: failed to
        // initiate panic, error 3, aborting"). That is a SIGABRT: no
        // backtrace, no message the user sees, the window simply vanishes. A
        // handler that prints and returns loses the offending frame and keeps
        // the process.
        //
        // This is a BACKSTOP, not the fix for the oversized-mesh case.
        // `install_mesh` refuses such a mesh before wgpu ever sees it, and it
        // has to: `wgpu::util::create_buffer_init` follows a failed
        // `create_buffer` with `get_mapped_range_mut(..).expect(..)` on the
        // error buffer it got back, so for that one path a handler alone
        // would only trade wgpu's abort for `wgpu::util`'s.
        device.on_uncaptured_error(std::sync::Arc::new(|error| {
            eprintln!("gpui-maps: wgpu error (frame lost, renderer continuing): {error}");
        }));

        // Read back off the device rather than reused from `required_limits`:
        // this is the number wgpu validates against, and it is the one worth
        // trusting.
        let max_buffer_size = device.limits().max_buffer_size;
        Ok(Self { device, queue, max_buffer_size })
    }
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct CameraUniform {
    view_proj: [[f32; 4]; 4],
    zoom: f32,
    /// Physical pixels per logical pixel. `shader.wgsl` sizes the stroke's
    /// antialiasing band from this so the band is one PHYSICAL pixel wide on
    /// any display -- MapLibre passes `u_device_pixel_ratio` for the same
    /// purpose.
    device_pixel_ratio: f32,
    // `zoom` and `device_pixel_ratio` sit at byte offsets 64 and 68, and
    // WGSL gives the following `vec2<f32>` an 8-byte alignment, so it lands
    // at 72; the `vec4<f32>` after it is 16-byte aligned and 80 already is,
    // so it lands at 80 and the struct ends at 96 -- a multiple of the
    // 16-byte alignment `mat4x4<f32>` imposes, so there is no further
    // rounding. These fields therefore match the WGSL struct's true 96-byte
    // size exactly; get it wrong and wgpu's validator rejects the bind group
    // outright ("buffer bound with size N where the shader expects M")
    // rather than failing quietly.
    /// `shader.wgsl`'s `fog_range`: `(start, end)` depth of the horizon
    /// fade, `end == 0` off. In the 8 bytes that were `_pad`.
    fog_range: [f32; 2],
    /// `shader.wgsl`'s `fog_color`, at offset 80: a `vec4<f32>` is 16-byte
    /// aligned, and 80 already is.
    fog_color: [f32; 4],
    /// `shader.wgsl`'s `circle_frame`, at offset 96 (the struct ends at
    /// 112): `.x` is the camera's eye-to-centre distance in world units
    /// (`Camera::distance`, MapLibre's `cameraToCenterDistance`), which
    /// `vs_circle` divides a circle's clip `w` by for `circle-pitch-scale:
    /// viewport`. Zero until [`TilePipeline::write_circle_frame`] writes it,
    /// and a zero draws that scale as `map` (the spec's default) rather
    /// than dividing by it. The rest is padding.
    circle_frame: [f32; 4],
}

/// The horizon fade for one frame: `shader.wgsl`'s `Camera::fog_range` and
/// `fog_color`. Decided by `cover_plan::horizon_fog`; a shell only writes it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HorizonFog {
    pub start: f32,
    pub end: f32,
    pub color: [f32; 4],
}

impl HorizonFog {
    /// Off. What every frame below 50 degrees of pitch writes.
    pub const NONE: HorizonFog = HorizonFog { start: 0.0, end: 0.0, color: [0.0; 4] };
}

/// The globe half of a tile's model uniform: everything
/// `shader.wgsl`'s `project_to_sphere` / `interpolate_projection` need that
/// `ModelUniform`'s Mercator fields do not already carry.
///
/// A struct rather than five more positional parameters on
/// [`TilePipeline::write_model_uniform`], because four of the five are
/// meaningless apart (a globe matrix without the tile's Mercator coords
/// projects every tile onto the same patch of sphere) and because
/// [`Self::FLAT`] then names the "no globe" case once instead of at every
/// call site.
///
/// **All five fields are `f32` because this is the upload boundary.** The
/// matrix and plane are computed in `f64` by `render::globe::globe_frame` and
/// narrowed here, once -- the rule the rest of this crate's matrix chain
/// follows (see `Camera::view_projection`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GlobeParams {
    /// Unit-sphere position → clip space: `GlobeFrame::globe_view_proj`,
    /// narrowed. MapLibre's `u_projection_matrix` in globe mode.
    ///
    /// Per FRAME, not per tile -- every tile drawn in one frame gets the same
    /// matrix here. It rides in the per-tile uniform anyway because that is
    /// the only uniform a tile's bind group carries that is rewritten when the
    /// camera moves, and the alternative (a third bind group) would cost a
    /// `max_bind_groups` slot to save 64 bytes per drawn tile.
    pub globe_matrix: [[f32; 4]; 4],
    /// `(x, y, width, height)` of the tile in `[0, 1]` Mercator space:
    /// `render::globe::tile_mercator_coords`. MapLibre's
    /// `u_projection_tile_mercator_coords`
    /// (`_projection_globe.vertex.glsl:3`).
    pub tile_mercator_coords: [f32; 4],
    /// The horizon plane, `xyz` normal and `w` offset, in unit-sphere space:
    /// `GlobeFrame::clipping_plane`, narrowed. MapLibre's
    /// `u_projection_clipping_plane` (`:4`).
    pub clipping_plane: [f32; 4],
    /// Globeness: `0.0` = pure Mercator, `1.0` = pure globe.
    /// `render::globe::transition_for_zoom`. MapLibre's
    /// `u_projection_transition` (`:5`).
    pub transition: f32,
    /// Multiplier taking a vertex's `position` from lyon input units into the
    /// `[0, 1]` in-tile space `project_to_sphere` wants: `1 / (COORD_SCALE *
    /// extent)`.
    ///
    /// Carried separately rather than folded into
    /// `tile_mercator_coords.zw` -- which is what MapLibre does, its
    /// `getMercatorTileCoordinates` returning `1 / scale / EXTENT`
    /// (`transform_helper.ts:692-703`) -- because
    /// `render::globe::tile_mercator_coords` deliberately returns the tile's
    /// TRUE Mercator span and documents that the consumer must normalise its
    /// own in-tile coordinate. Our vertex positions are not in MVT extent
    /// units; they are in lyon units, MVT coords times `COORD_SCALE`.
    pub lyon_to_tile_unit: f32,
    /// `[0, 1]` in-tile units per logical screen pixel, at the camera's
    /// CONTINUOUS zoom: `1 / (tile_world_size * 2^zoom)`.
    ///
    /// The globe's stroke extrusion happens in tile space and is then
    /// projected through the sphere, because the sphere mapping is not affine
    /// and a direction cannot be pushed through it the way the flat path
    /// pushes a world-space direction through `camera.view_proj`. This is the
    /// scale that makes that extrusion come out at the requested pixel width.
    ///
    /// NOT `dash_scale`'s floored-zoom sibling: this one must be continuous,
    /// or every line on the globe would jump in thickness at each integer
    /// zoom. See `dash_scale`'s doc comment for why that one is floored.
    pub tile_units_per_px: f32,
    /// **Unit** direction from the planet's centre toward the sun, in the
    /// sphere space `shader.wgsl`'s `project_to_sphere` returns:
    /// `render::atmosphere::Sun::direction_sphere`.
    ///
    /// Per FRAME, like `globe_matrix` and for the same reason — it rides in
    /// the per-tile uniform because that is the uniform a tile's bind group
    /// already rewrites when the camera moves.
    ///
    /// Not normalised here: `Sun::direction_sphere` returns a unit vector
    /// (asserted in `a_map_anchored_sun_ignores_the_camera_and_a_viewport_anchored_one_does_not`)
    /// and `shader.wgsl` normalises the NORMAL rather than this, because
    /// the normal is the one that comes out of an `exp` in f32.
    pub sun_direction: [f32; 3],
    /// How bright the night side of the planet is, as a multiplier on a
    /// fill's own colour: `0.0` black, `1.0` no shading at all.
    /// `render::atmosphere::Atmosphere::night_brightness`.
    pub night_brightness: f32,
}

impl GlobeParams {
    /// No globe at all: `transition` zero, so `shader.wgsl` takes its flat
    /// early-out and never evaluates `project_to_sphere`.
    ///
    /// The other fields are zero and are genuinely unread in that path -- the
    /// shader's `transition <= 0.0` branch returns the Mercator position
    /// before touching any of them, and writes a zero `globe_light` varying
    /// so its fragment stage skips the ground shading too. Kept as a named
    /// constant so a call site that has no globe to pass says so, rather
    /// than spelling out seven zeroes and inviting the question of whether
    /// they mean something.
    ///
    /// **`night_brightness` is 0.0 here and not 1.0, deliberately.** 1.0
    /// would be the "no shading" value, so a shading leak onto a flat frame
    /// would be a NO-OP and no test could see it -- which is exactly the
    /// trap Task 8 fell into twice (a magenta backdrop and a written sky
    /// uniform are what it fixed them with). 0.0 makes a leak paint the
    /// whole flat basemap black, and
    /// `a_flat_frame_is_pixel_identical_however_the_sun_is_written` is
    /// graded against a fixture bright enough to show it.
    pub const FLAT: Self = Self {
        globe_matrix: [[0.0; 4]; 4],
        tile_mercator_coords: [0.0; 4],
        clipping_plane: [0.0; 4],
        transition: 0.0,
        lyon_to_tile_unit: 0.0,
        tile_units_per_px: 0.0,
        sun_direction: [0.0; 3],
        night_brightness: 0.0,
    };

    /// This uniform with ground shading turned off: a sun of zero length
    /// and a night side as bright as the day side.
    ///
    /// For the pixel tests that predate ground shading and are graded
    /// against the exact colours they were graded against before it —
    /// `subdivision_makes_the_globes_limb_a_circle`,
    /// `a_symbol_anchor_lands_on_the_tile_that_owns_it_on_the_globe` and
    /// their siblings, which read `(255, 0, 0)` and mean it. A test about a
    /// silhouette or an anchor should not have to know a sun exists; one
    /// about the sun asks for it.
    #[must_use]
    pub fn unlit(self) -> Self {
        Self { sun_direction: [0.0; 3], night_brightness: 1.0, ..self }
    }
}

/// **The part of a tile's own coordinate space a draw is allowed to
/// paint**, as a box in the same lyon units `mesh::Vertex::position`
/// carries (MVT coordinates times `mesh::COORD_SCALE`).
///
/// # Why a draw would want less than the whole tile
///
/// `MapView::paint` draws a cached ANCESTOR under any visible tile whose
/// own mesh has not arrived yet, so the frame has no hole where the tile
/// will be. One z2 ancestor covers four z3 tiles, so with one of the four
/// still loading the ancestor is drawn under the other THREE as well — and
/// a translucent fill (a `landcover` wash at `fill-opacity` 0.3, say) is
/// then composited twice over those three quadrants. Measured on the
/// reported camera with `capture`'s `--absent 1`: 1,700 doubled pixels
/// against a settled frame's 3, a factor of 567, in a patch bounded by the
/// ancestor's own tile box — which on the globe is a meridian or a
/// parallel, exactly the reported shape.
///
/// A stand-in only stands in for the tiles that are actually missing, so
/// this is the box of the one missing tile it was pulled in for, expressed
/// in the ANCESTOR's coordinates ([`Self::for_descendant`]).
///
/// # Why in TILE space, and not a scissor rectangle
///
/// A scissor is a rectangle in SCREEN space, and a tile's screen-space
/// region on the globe is not a rectangle: it is a spherical quadrilateral
/// whose edges bow, and near the limb it is barely even convex. A scissor
/// tight enough to exclude the ready siblings would cut into the missing
/// tile's own quadrant, and one loose enough to keep the quadrant whole
/// would still cover part of the siblings. The clip has to be applied in
/// the space the boundary is straight in, which is the tile's own — and
/// there the boundary is exactly the edge the ready sibling's geometry
/// starts at, so the two abut with neither a gap nor an overlap.
///
/// That is the same argument `style::clip_fill_geometry_to_tile_extent`
/// already makes for adjacent tiles' overlapping buffer geometry; this is
/// the same clip, at a sub-tile box, applied per fragment because the mesh
/// being clipped is a cached one that is also drawn unclipped elsewhere.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TileClip {
    /// `[min_x, min_y, max_x, max_y]`, lyon units, inclusive.
    bounds: [f32; 4],
}

impl TileClip {
    /// No clip at all: every fragment passes.
    ///
    /// **`1e30`, not `f32::MAX` and not an infinity**, and the exact value
    /// is load-bearing. The shader normalises a vertex's position into this
    /// box (`(p - min) / (max - min)`) and masks on the result being in
    /// `[0, 1]`; with `f32::MAX` at both ends that denominator overflows to
    /// infinity, and an infinity in a uniform is the kind of value a
    /// backend is free to be creative about. `1e30` leaves the span finite
    /// (`2e30`), makes the numerator round to exactly `1e30` for any real
    /// tile coordinate, and so puts EVERY vertex of an unclipped draw at
    /// exactly `0.5` — a mask of exactly `1.0`, an alpha multiplied by a
    /// bitwise one, and a frame **bit-identical** to one drawn before this
    /// field existed. That is what
    /// `a_flat_frame_is_pixel_identical_however_the_sun_is_written` and
    /// every other pixel test in `render::gpu` rest on.
    pub const WHOLE: Self = Self { bounds: [-1e30, -1e30, 1e30, 1e30] };

    /// The box `descendant` occupies inside `ancestor`, in `ancestor`'s own
    /// lyon coordinates.
    ///
    /// `descendant` must be at or below `ancestor` in the pyramid and
    /// inside it; anything else returns [`Self::WHOLE`], because the
    /// caller's fallback for "I cannot work out the sub-box" has to be the
    /// box that draws EVERYTHING rather than the one that draws nothing —
    /// a stand-in exists to stop a hole, and a clip that got its arithmetic
    /// wrong must fail back to the old behaviour, not to a black quadrant.
    #[must_use]
    pub fn for_descendant(ancestor: crate::geo::TileId, descendant: crate::geo::TileId) -> Self {
        let Some(depth) = descendant.z.checked_sub(ancestor.z) else { return Self::WHOLE };
        // 2^31 sub-tiles across is already past anything a tile pyramid
        // reaches, and `1u32 << 32` is a panic in debug and a wrap in
        // release -- neither of which may happen inside a fallback path.
        if depth >= 32 {
            return Self::WHOLE;
        }
        let span = 1u32 << depth;
        let (Some(base_x), Some(base_y)) =
            (ancestor.x.checked_mul(span), ancestor.y.checked_mul(span))
        else {
            return Self::WHOLE;
        };
        let (Some(local_x), Some(local_y)) =
            (descendant.x.checked_sub(base_x), descendant.y.checked_sub(base_y))
        else {
            return Self::WHOLE; // not actually a descendant
        };
        if local_x >= span || local_y >= span {
            return Self::WHOLE; // ditto, off the other side
        }
        // The full span of a tile's vertex coordinates: `GlobeUpload`'s
        // `lyon_to_tile_unit` is `1 / (COORD_SCALE * extent)`, so this is
        // its reciprocal, and a position divided by it is the `[0, 1]`
        // in-tile coordinate the shader calls `pos_in_tile`.
        let lyon_span = crate::render::mesh::COORD_SCALE * crate::mvt::MVT_EXTENT as f32;
        let cell = lyon_span / span as f32;
        Self {
            bounds: [
                local_x as f32 * cell,
                local_y as f32 * cell,
                (local_x + 1) as f32 * cell,
                (local_y + 1) as f32 * cell,
            ],
        }
    }

    /// Whether this clip lets every fragment through, i.e. whether a draw
    /// under it can reuse a tile's own cached, unclipped model uniform.
    #[must_use]
    pub fn is_whole(self) -> bool {
        self == Self::WHOLE
    }

    /// `[min_x, min_y, max_x, max_y]` in lyon units — the uniform's own
    /// field order, and the only thing the shader reads.
    #[must_use]
    pub fn bounds(self) -> [f32; 4] {
        self.bounds
    }
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ModelUniform {
    matrix: [[f32; 4]; 4],
    /// [`GlobeParams::globe_matrix`]. Second so the two `mat4x4<f32>`s sit at
    /// offsets 0 and 64, both already 16-byte aligned, with no padding between
    /// them.
    globe_matrix: [[f32; 4]; 4],
    /// [`GlobeParams::tile_mercator_coords`]. A `vec4<f32>` in WGSL, so it
    /// must land on a 16-byte boundary: 128, which it does.
    tile_mercator_coords: [f32; 4],
    /// [`GlobeParams::clipping_plane`], at offset 144.
    clipping_plane: [f32; 4],
    blend_t: f32,
    /// Pixels per lyon input unit at the camera's FLOORED zoom; see
    /// `camera::dash_scale` (the floor is deliberate -- it is MapLibre's
    /// `u_tileratio` behaviour). Consumed only by the dash mask, which
    /// crossfades between this scale and twice it to hide the doubling at each
    /// integer zoom -- see `shader.wgsl`'s `fs_main`.
    dash_scale: f32,
    /// [`GlobeParams::transition`].
    transition: f32,
    /// [`GlobeParams::lyon_to_tile_unit`].
    lyon_to_tile_unit: f32,
    /// [`GlobeParams::tile_units_per_px`].
    tile_units_per_px: f32,
    // Pads to offset 192, which is where WGSL puts the `vec3<f32>` below --
    // `vec3<f32>` has a 16-byte ALIGNMENT, so it cannot start at 180 and the
    // next multiple of 16 is 192. This pad is therefore not slack at the end
    // of the struct any more; it is a field's alignment, and a field added
    // above it moves the sun.
    //
    // 64 (matrix) + 64 (globe_matrix) + 16 (tile_mercator_coords)
    // + 16 (clipping_plane) = 160, then five f32 scalars = 180, pad to 192,
    // `sun_direction` 192..204, `night_brightness` 204..208, then
    // `clip_bounds` 208..224 -- **224 bytes**, a multiple of 16, with no
    // tail pad at all (which `bytemuck::Pod` requires). The `vec4` at the
    // end needed a multiple of 16 to start on and 208 already was one, so
    // it moved nothing above it; the sun is still at 192.
    //
    // Getting this wrong does NOT look like a uniform bug: it
    // shifts every field after the pad, so a matrix column arrives where a
    // scalar was expected and the GEOMETRY comes out corrupt. Asserted by
    // `the_model_uniform_is_16_byte_aligned_and_its_declared_need_matches_reality`.
    _pad: [f32; 3],
    /// [`GlobeParams::sun_direction`], at offset 192.
    sun_direction: [f32; 3],
    /// [`GlobeParams::night_brightness`], at offset 204.
    night_brightness: f32,
    /// [`TileClip::bounds`] — `[min_x, min_y, max_x, max_y]` in lyon units,
    /// a `vec4<f32>` in WGSL and therefore 16-byte aligned.
    ///
    /// **Appended deliberately, at offset 208.** A `vec4` needs a multiple
    /// of 16 and 208 already is one, so this lands immediately after
    /// `night_brightness` with no pad at all and, more to the point,
    /// **moves no field above it** — the sun stays at 192, which is what
    /// the offsets documented on `_pad` above assert. The struct is 224
    /// bytes, still a multiple of 16 and still with no tail pad, which is
    /// what `bytemuck::Pod` requires.
    clip_bounds: [f32; 4],
    /// `(keep_start, keep_end, opacity, metrics_on)` -- the per-draw line
    /// effects a story's `draw`/`show`/`hide`/`fade` drive without
    /// re-tessellating. [`LINE_FX_NONE`] everywhere except where
    /// [`TilePipeline::write_model_line_fx`] writes otherwise.
    ///
    /// **Appended at offset 224**, a multiple of 16, so it moves nothing
    /// above it; the struct becomes 240 bytes, still a multiple of 16 with
    /// no tail pad. `shader.wgsl` reads `.z` (opacity) in the vertex stage
    /// and `.x`, `.y`, `.w` (the trim) in `fs_main` -- the reason the model
    /// group is now `VERTEX | FRAGMENT`.
    ///
    /// `[x, y]` is the VISIBLE range of line progress, the complement of the
    /// style spec's `line-trim-offset` (which names the hidden range); a
    /// caller taking `line-trim-offset` must convert. `extrusion_shader.wgsl`
    /// ignores the field, so a fade does not reach 3D buildings.
    line_fx: [f32; 4],
}

/// The `line_fx` every draw gets unless a caller asks for a trim or a fade:
/// no trim (`0..1`), full opacity, metrics off. `shader.wgsl`'s opacity
/// multiply is exact at `1.0` and the trim test is skipped at `w == 0`, so
/// a draw with this value is bit-identical to one made before `line_fx`
/// existed.
pub const LINE_FX_NONE: [f32; 4] = [0.0, 1.0, 1.0, 0.0];

/// A kept range covering the whole line (`x <= 0`, `y >= 1`) trims
/// nothing, so it is written with the trim OFF (`w = 0`): no fragment is
/// then ever compared against its ends, and a "fully drawn" line is
/// exactly the untrimmed one, cap fragments that interpolate to
/// `1.0 + ulp` included. Every other value passes through unchanged; an
/// empty range (`y <= x`) is `shader.wgsl`'s to draw as nothing.
#[must_use]
pub fn normalise_line_fx(line_fx: [f32; 4]) -> [f32; 4] {
    let [x, y, z, w] = line_fx;
    if x <= 0.0 && y >= 1.0 {
        [x, y, z, 0.0]
    } else {
        [x, y, z, w]
    }
}

/// `circle_frame`'s byte offset inside the camera uniform, for
/// [`TilePipeline::write_circle_frame`].
const CIRCLE_FRAME_OFFSET: u64 = std::mem::offset_of!(CameraUniform, circle_frame) as u64;

/// `line_fx`'s byte offset inside the model uniform, for the partial write
/// in [`TilePipeline::write_model_line_fx`].
const LINE_FX_OFFSET: u64 = std::mem::offset_of!(ModelUniform, line_fx) as u64;

impl ModelUniform {
    /// The one place a [`GlobeParams`] is flattened into the uniform's own
    /// field order, so the two constructors below cannot drift apart in how
    /// they lay it out.
    fn new(
        matrix: [[f32; 4]; 4],
        blend_t: f32,
        dash_scale: f32,
        globe: GlobeParams,
        clip: TileClip,
        line_fx: [f32; 4],
    ) -> Self {
        Self {
            matrix,
            globe_matrix: globe.globe_matrix,
            tile_mercator_coords: globe.tile_mercator_coords,
            clipping_plane: globe.clipping_plane,
            blend_t,
            dash_scale,
            transition: globe.transition,
            lyon_to_tile_unit: globe.lyon_to_tile_unit,
            tile_units_per_px: globe.tile_units_per_px,
            _pad: [0.0; 3],
            sun_direction: globe.sun_direction,
            night_brightness: globe.night_brightness,
            clip_bounds: clip.bounds(),
            line_fx: normalise_line_fx(line_fx),
        }
    }
}

pub struct TileBuffers {
    vertex_buffer: crate::render::alloc::TrackedBuffer,
    index_buffer: crate::render::alloc::TrackedBuffer,
    index_count: u32,
    /// `Uint32` for every tile; `Uint16` for an overlay chunk of at most
    /// 65,536 vertices -- MapLibre's segment limit (`segment.ts:136`).
    index_format: wgpu::IndexFormat,
    /// A fill outline's LINE-LIST indices over THIS buffer's vertices, and
    /// their count, in `index_format`. `None` for everything without one.
    outline: Option<(crate::render::alloc::TrackedBuffer, u32)>,
    /// A basemap tile's fills on the 8-byte vertex
    /// ([`TilePipeline::install_basemap_mesh`]). When set, the buffers above
    /// hold only the triangles that must stay 88-byte, and the tile draws
    /// as `split.runs`, alternating the two bodies in op order.
    split: Option<Box<SplitBody>>,
    /// A basemap tile's dot-mask triangles
    /// ([`TilePipeline::install_basemap_mesh_masked`]): index lists over
    /// the buffers above and `split`'s compact body. `None` for everything
    /// else, and for a basemap tile with no ranked fill.
    pub(crate) mask: Option<MaskBuffers>,
}

/// A basemap tile's [`crate::render::compact::MaskIndexLists`] on the GPU:
/// per body, ONE index buffer holding the three lists back to back, and
/// each list's range of it (`[water, green, building]`, in indices). Each
/// body is `None` when its three lists are all empty.
///
/// `INDEX` usage only -- no `COPY_DST`, since a paint-only rewrite
/// ([`TilePipeline::write_vertex_buffer`]) never moves an index, and no
/// `COPY_SRC`: the mask is never read back (DP-8).
pub(crate) struct MaskBuffers {
    pub(crate) compact: Option<(crate::render::alloc::TrackedBuffer, [std::ops::Range<u32>; 3])>,
    pub(crate) legacy: Option<(crate::render::alloc::TrackedBuffer, [std::ops::Range<u32>; 3])>,
}

impl MaskBuffers {
    /// `lists` on the device, or `None` when every list is empty.
    fn new(ctx: &GpuContext, lists: &crate::render::compact::MaskIndexLists) -> Option<Self> {
        if lists.is_empty() {
            return None;
        }
        let body = |label: &'static str, three: &[Vec<u32>; 3]| {
            if three.iter().all(Vec::is_empty) {
                return None;
            }
            let mut indices = Vec::with_capacity(three.iter().map(Vec::len).sum());
            let ranges = std::array::from_fn(|k| {
                let start = indices.len() as u32;
                indices.extend_from_slice(&three[k]);
                start..indices.len() as u32
            });
            let buffer = crate::render::alloc::TrackedBuffer::init(
                &ctx.device,
                &wgpu::util::BufferInitDescriptor {
                    label: Some(label),
                    contents: bytemuck::cast_slice(&indices),
                    usage: wgpu::BufferUsages::INDEX,
                },
            );
            Some((buffer, ranges))
        };
        Some(Self { compact: body("mask-compact-indices", &lists.compact), legacy: body("mask-legacy-indices", &lists.legacy) })
    }

    /// Bytes held by the two buffers.
    #[must_use]
    fn device_bytes(&self) -> u64 {
        [&self.compact, &self.legacy].iter().filter_map(|b| b.as_ref()).map(|(buffer, _)| buffer.tracked_size()).sum()
    }
}

/// The compact half of a split basemap tile: its buffers, its paint table
/// and the table's group-2 bind group, and the draw order.
struct SplitBody {
    compact: CompactBuffers,
    /// The undashed strokes on the 16-byte `compact::CompactStrokeVertex`;
    /// `None` when the tile has none, or strokes stay 88-byte.
    strokes: Option<StrokeBuffers>,
    table: PaintTableGpu,
    table_group: wgpu::BindGroup,
    runs: Vec<crate::render::compact::TileRun>,
    /// Vertex counts of the bodies, which a paint-only rewrite
    /// ([`TilePipeline::write_vertex_buffer`]) must reproduce to write in place.
    legacy_vertices: usize,
    compact_vertices: usize,
    stroke_vertices: usize,
}

/// A split basemap tile's stroke body: 16-byte vertices and their
/// triangle list.
struct StrokeBuffers {
    vertices: crate::render::alloc::TrackedBuffer,
    indices: crate::render::alloc::TrackedBuffer,
    format: wgpu::IndexFormat,
}

impl TileBuffers {
    /// Device bytes of the compact half of a split basemap tile: its
    /// fill and stroke vertex and index buffers and its paint table. 0 when
    /// not split.
    #[must_use]
    pub fn compact_bytes(&self) -> u64 {
        self.split.as_ref().map_or(0, |s| {
            s.compact.device_bytes()
                + s.table.device_bytes()
                + s.strokes.as_ref().map_or(0, |b| b.vertices.tracked_size() + b.indices.tracked_size())
        })
    }

    /// Device bytes of the dot-mask index lists ([`MaskBuffers`]); 0
    /// without them.
    #[must_use]
    pub fn mask_bytes(&self) -> u64 {
        self.mask.as_ref().map_or(0, MaskBuffers::device_bytes)
    }

    /// Every device byte this tile's buffers hold: the 88-byte vertex,
    /// index and outline buffers' capacities, the compact half
    /// ([`Self::compact_bytes`]) and the mask lists ([`Self::mask_bytes`]).
    #[must_use]
    pub fn device_bytes(&self) -> u64 {
        self.vertex_capacity_bytes()
            + self.index_capacity_bytes()
            + self.outline_capacity_bytes()
            + self.compact_bytes()
            + self.mask_bytes()
    }

    /// Whether this tile's undashed strokes draw on the 16-byte vertex.
    #[must_use]
    pub fn has_compact_strokes(&self) -> bool {
        self.split.as_ref().is_some_and(|s| s.strokes.is_some())
    }

    /// Whether this tile's fills draw on the compact vertex.
    #[must_use]
    pub fn is_split(&self) -> bool {
        self.split.is_some()
    }

    /// Bytes the vertex buffer can hold -- its CAPACITY, not the size of
    /// the mesh currently in it. The two differ after a smaller mesh is
    /// installed into buffers grown for a larger one; see
    /// [`TilePipeline::install_mesh`].
    #[must_use]
    pub fn vertex_capacity_bytes(&self) -> u64 {
        self.vertex_buffer.tracked_size()
    }

    /// Bytes the index buffer can hold. Same capacity-not-size caveat as
    /// [`Self::vertex_capacity_bytes`]; `index_count` is what bounds the
    /// draw.
    #[must_use]
    pub fn index_capacity_bytes(&self) -> u64 {
        self.index_buffer.tracked_size()
    }

    /// Bytes the fill-outline index buffer can hold; 0 without one.
    #[must_use]
    pub fn outline_capacity_bytes(&self) -> u64 {
        self.outline.as_ref().map_or(0, |(buffer, _)| buffer.tracked_size())
    }
}

/// `queue.write_buffer(buffer, 0, bytes)`, with a `bytes.len()` that is
/// not a multiple of `wgpu::COPY_BUFFER_ALIGNMENT` padded with zeros up to
/// one. The aligned prefix is written straight from `bytes`, so only the
/// last word (at most 3 bytes of data) is copied. Nothing for empty
/// `bytes`: wgpu rejects a zero-length write. See [`fit_or_allocate`].
fn write_padded(ctx: &GpuContext, buffer: &wgpu::Buffer, bytes: &[u8]) {
    let align = wgpu::COPY_BUFFER_ALIGNMENT as usize;
    let whole = bytes.len() - bytes.len() % align;
    if whole > 0 {
        ctx.queue.write_buffer(buffer, 0, &bytes[..whole]);
    }
    if whole < bytes.len() {
        let mut tail = [0u8; 4];
        tail[..bytes.len() - whole].copy_from_slice(&bytes[whole..]);
        ctx.queue.write_buffer(buffer, whole as u64, &tail[..align]);
    }
}

/// Writes `bytes` into `existing` if it has the capacity, and allocates a
/// buffer sized to `bytes` if it does not (or if there is no `existing`).
///
/// The grow-and-keep half of [`TilePipeline::install_mesh`], factored out
/// because the vertex and index buffers grow independently: a mesh can need
/// more vertices without needing more indices, and growing the one that
/// fits would throw away capacity for nothing.
///
/// `max(needed, current)` is what the reallocation asks for, and this is
/// already that: the only way to reach the allocation is `current <
/// needed`, so `max` is `needed`. Never shrinking is the branch above, not
/// arithmetic here.
///
/// The empty-`bytes` guard is the same one `write_vertex_range` carries --
/// wgpu rejects a zero-length `write_buffer` -- and it is reachable: a tile
/// with no matching style layer tessellates to nothing.
///
/// # The write is padded to `COPY_BUFFER_ALIGNMENT`
///
/// `queue.write_buffer` rejects a size that is not a multiple of 4, and a
/// u16 overlay chunk with an ODD triangle count has `6 * n` index bytes.
/// The allocation below never noticed -- `create_buffer_init` pads -- but
/// the reuse branch did, and the uncaptured-error handler prints and
/// carries on, so the write was simply DROPPED: the chunk kept the
/// previous generation's indices while its vertices and index count were
/// the new ones, and a re-pushed source drew triangles between vertices
/// that were never meant to meet -- the kilometre-long translucent wedges
/// a consumer reported over their bus routes. The fitting buffer is
/// always big enough for the padded tail: its own size is a multiple of 4
/// (`TrackedBuffer::track` records `buffer.size()`, which `create_buffer_init`
/// rounded up), so it holds `bytes.len()` rounded up too.
/// `a_reused_chunk_with_an_odd_triangle_count_draws_its_own_triangles`
/// pins it.
fn fit_or_allocate(
    ctx: &GpuContext,
    existing: Option<crate::render::alloc::TrackedBuffer>,
    bytes: &[u8],
    label: &'static str,
    usage: wgpu::BufferUsages,
) -> crate::render::alloc::TrackedBuffer {
    if let Some(buffer) = existing {
        if buffer.tracked_size() >= (bytes.len() as u64).next_multiple_of(wgpu::COPY_BUFFER_ALIGNMENT) {
            write_padded(ctx, &buffer, bytes);
            return buffer;
        }
    }
    crate::render::alloc::TrackedBuffer::init(
        &ctx.device,
        &wgpu::util::BufferInitDescriptor { label: Some(label), contents: bytes, usage },
    )
}

/// Reports a mesh [`TilePipeline::install_mesh`] refused, at most once per
/// process.
///
/// Once, not every time, because installs are driven by bake completions, and
/// an animated overlay that has crossed the ceiling crosses it on every bake
/// -- which at demo frame rates is a scrolling wall of identical stderr. The
/// first one carries all the information the next thousand would.
fn note_mesh_too_large(vertex_bytes: u64, index_bytes: u64, max_buffer_size: u64) {
    static REPORTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if REPORTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    eprintln!(
        "gpui-maps: mesh too large for one GPU buffer, skipping it (this window keeps \
         running; nothing else is affected, and a smaller mesh will draw normally). \
         vertices {vertex_bytes} B, indices {index_bytes} B, max_buffer_size \
         {max_buffer_size} B. Reduce the overlay's coordinate count -- aggregate, \
         simplify, or filter. Reported once per process."
    );
}

/// What a [`TilePipeline`] has sent to the GPU for dot tiles, monotonic:
/// one map's own count (each `MapView` and each capture has its own
/// pipeline), unlike the process-global `render::allocation_stats`.
/// Counted INSIDE [`TilePipeline::create_dot_tile`],
/// [`TilePipeline::create_dot_bind_group`] and
/// [`TilePipeline::write_dot_params`], so no call site escapes it (review
/// N-3). Live epochs' claim (Task 15, MG-23) is graded here: moving
/// `epoch_mix` moves `params_writes` only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DotUploads {
    /// Block textures created and written: a tile's whole blocks.
    pub tile_uploads: u64,
    /// Dot groups created, each with a new `DotParams` buffer.
    pub groups_created: u64,
    /// `DotParams` rewritten in place: no allocation.
    pub params_writes: u64,
}

pub struct TilePipeline {
    pipeline: wgpu::RenderPipeline,
    /// The fill-outline twin of `pipeline`: `LineList`, `vs_outline`. See
    /// `TilePipeline::new` and `TileBuffers::outline`.
    outline: wgpu::RenderPipeline,
    /// The same shader, the same layouts and the same vertex format as
    /// `pipeline`, with **`depth_write_enabled` off** — for the globe's
    /// ocean-sphere backdrop (`render::space::backdrop_mesh`) and nothing
    /// else.
    ///
    /// # Why the backdrop cannot share `pipeline`
    ///
    /// The backdrop is a `z0` tile-covering fill and a drawn tile is a
    /// `z3`-ish one, and at the same point of the sphere they compute the
    /// same position by DIFFERENT arithmetic: `shader.wgsl`'s
    /// `project_to_sphere` evaluates `tile_mercator_coords.xy +
    /// tile_mercator_coords.zw * pos_in_tile`, which is `(0,0) + (1,1)*p`
    /// for the backdrop and `(x/8,y/8) + (1/8,1/8)*p` for the tile. Those
    /// agree mathematically and round differently in f32, so the two z
    /// values can differ by an ULP either way — and `depth_compare` is
    /// `LessEqual`, so wherever the backdrop's z landed the smaller one a
    /// tile fragment drawn afterwards would be REJECTED, leaving a speckle
    /// of ocean-coloured pixels scattered through the land.
    ///
    /// Writing no depth removes the failure rather than narrowing it: the
    /// depth buffer still holds its `Clear(1.0)` when the tiles arrive, so
    /// every tile fragment passes, and occlusion of the backdrop is draw
    /// ORDER (backdrop first) exactly as it is for the stars before it.
    /// MapLibre's own globe-only backdrop pass is `DepthMode.ReadOnly`
    /// (`draw_sky.ts:84`) for the same reason.
    backdrop: wgpu::RenderPipeline,
    /// The same shader and layouts again, writing **depth and no colour**
    /// (`ColorWrites::empty()`, `depth_write_enabled: true`) -- the ocean
    /// sphere drawn a second time, AFTER the tiles, purely to put the
    /// planet's surface into the depth buffer where no tile fill did.
    ///
    /// # Why the glow needed this
    ///
    /// `AtmospherePipeline` draws at the far plane and tests `LessEqual`,
    /// so it paints only where nothing nearer was drawn. That works
    /// wherever the ground wrote depth -- but the ground writes depth only
    /// where a FILL was drawn, and the ocean is not a fill: it is the
    /// backdrop above, whose whole purpose is to write no depth. Over
    /// water the buffer therefore still held the cleared 1.0, the glow's
    /// `LessEqual` passed, and the scattering integral composited over the
    /// sea.
    ///
    /// Measured on the real frame -- lon 15, lat 50, z3.5, 2048 px, 32
    /// tiles, the terminator lit -- as the largest channel difference the
    /// atmosphere pass makes, split by whether the control pixel is one the
    /// backdrop painted:
    ///
    /// ```text
    ///   r / limb        over a tile fill      over the backdrop
    ///   0.90 - 0.95     mean 0.00  max 0      mean  6.26  max  18
    ///   0.95 - 0.97     mean 0.00  max 0      mean 13.52  max  31
    ///   0.97 - 0.99     mean 0.00  max 0      mean 29.25  max 115
    /// ```
    ///
    /// **Exactly zero over land and all of it over water**, which is the
    /// whole diagnosis in one table: the depth fix in
    /// `AtmospherePipeline::new` already works perfectly wherever there IS
    /// depth, and the residual band is the water it never reached.
    ///
    /// # Why this does not bring back the hole-punching
    ///
    /// `backdrop` exists because the backdrop and a tile compute the same
    /// sphere point by different arithmetic and disagree by an ULP either
    /// way, so a depth-WRITING backdrop drawn FIRST rejected 56% of the
    /// tile fragments over it and speckled the land with ocean (the table
    /// on `backdrop`, and
    /// `the_ocean_sphere_does_not_punch_holes_in_the_tiles_over_it`).
    ///
    /// This pass cannot do that, for two independent reasons:
    ///
    ///  - it runs AFTER the tiles, so their colour is already in the
    ///    framebuffer, and
    ///  - it writes **no colour at all**, so there is no ocean pixel for it
    ///    to put anywhere.
    ///
    /// The ULP disagreement still happens -- on about half the land pixels
    /// this pass's `LessEqual` passes and overwrites the tile's depth with
    /// one an ULP away. That is harmless: the only later reader is the
    /// glow's test against the FAR plane, and a value an ULP from the
    /// ground's is just as far from 1.0 as the ground's was. The dither was
    /// never a depth problem; it was a problem about which COLOUR won, and
    /// this pass does not compete for colour.
    backdrop_depth: wgpu::RenderPipeline,
    /// `vs_compact` / `fs_main` over the 8-byte `compact::CompactVertex`,
    /// TriangleList: a tile's fills. Same blend, depth and multisample
    /// state as `pipeline`; see `TileDraw::CompactFill`.
    compact_fill: wgpu::RenderPipeline,
    /// The same shader and vertex format as `compact_fill`, `LineList`:
    /// the hairline over the fill's own vertices, with the outline's paint
    /// table bound. See `TileDraw::CompactOutline`.
    compact_outline: wgpu::RenderPipeline,
    /// `compact_fill` and `compact_outline` over a PLANAR buffer
    /// ([`TilePipeline::create_compact_buffers_planar`]): the same
    /// attributes, the same shader, from two streams.
    compact_fill_planar: wgpu::RenderPipeline,
    compact_outline_planar: wgpu::RenderPipeline,
    /// `vs_compact_tile` / `fs_main`, TriangleList: a split basemap tile's
    /// fills. The tile's own model group at group 1 and its paint table at
    /// group 2 (`tile_table_bgl`), so every model group a tile is drawn
    /// with -- its own, or a stand-in's clipped one -- serves both bodies.
    compact_tile: wgpu::RenderPipeline,
    /// `vs_compact_stroke` / `fs_main`, TriangleList: a split basemap
    /// tile's undashed strokes on the 16-byte vertex, under the same groups
    /// as `compact_tile`.
    compact_stroke: wgpu::RenderPipeline,
    tile_table_bgl: wgpu::BindGroupLayout,
    /// `vs_circle` / `fs_main`, TriangleStrip, instanced: one quad per
    /// `circles::CircleInstance`. The model group at 1 and `circle_bgl`
    /// (paint table + `CircleParams`) at 2. See `TileDraw::Circles`.
    circles: wgpu::RenderPipeline,
    circle_bgl: wgpu::BindGroupLayout,
    /// `vs_dot` / `fs_dot`, TriangleList, NO vertex buffer: one draw per
    /// dot tile, `DOT_VERTICES * B` (3B) vertices per `dots::DotBlock` of a
    /// bucket of size `B`, the blocks pulled from the tile's texture by
    /// `vertex_index`.
    /// `dot_bgl` (ramp, `DotParams`, blocks) at group 2. See `TileDraw::Dots`.
    dots: wgpu::RenderPipeline,
    /// `vs_dot_flat` / `fs_dot_flat`: the same draws on a frame whose
    /// `transition == 0` (MG-8). See `TileDraw::Dots`' `flat`.
    dots_flat: wgpu::RenderPipeline,
    dot_bgl: wgpu::BindGroupLayout,
    /// The dot mask pass ([`Self::encode_dot_mask`]): `vs_mask_compact` /
    /// `vs_mask_legacy` and `fs_mask` into an `R8Uint` target, on the
    /// mask-only layout (`mask_bgl`: one dynamic-offset [`MaskDraw`]).
    mask_compact: wgpu::RenderPipeline,
    mask_legacy: wgpu::RenderPipeline,
    /// `vs_mask_quad`: each drawn tile's footprint as `MaskRank::Land`, over
    /// the no-data clear and under the tile's ranked fills.
    mask_quad: wgpu::RenderPipeline,
    mask_bgl: wgpu::BindGroupLayout,
    /// The 1x1 `R8Uint` texel of rank `MaskRank::Land` a dot draw binds
    /// when there is no mask ([`Self::land_mask_view`]).
    land_mask_view: wgpu::TextureView,
    /// The 1x1 zero `Rgba32Uint` texel a dot draw binds at bindings 4 and
    /// 5 when its tile has no composition or shape texture.
    zero_texel_view: wgpu::TextureView,
    /// What this pipeline has sent for dot tiles ([`DotUploads`]),
    /// counted inside the calls that do it.
    dot_counts: [std::sync::atomic::AtomicU64; 3],
    camera_bgl: wgpu::BindGroupLayout,
    model_bgl: wgpu::BindGroupLayout,
    /// `model_bgl` plus the paint table at binding 1 (vertex-stage texture).
    compact_model_bgl: wgpu::BindGroupLayout,
    depth_format: wgpu::TextureFormat,
    /// The `fill-extrusion` pipeline: groups 0 and 1 are `camera_bgl` and
    /// `model_bgl`, and it draws into this pass's `depth_format`.
    extrusion: crate::render::extrusion_gpu::ExtrusionPipeline,
}

/// One draw of the dot mask pass: `shader.wgsl`'s `MaskDraw`, 32 bytes.
/// `ndc = pos_lyon * scale + offset`, y flipped; `rank` is what the draw
/// writes (`MaskRank as u32`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct MaskDraw {
    scale: [f32; 2],
    offset: [f32; 2],
    rank: u32,
    _pad: [u32; 3],
}

impl MaskDraw {
    /// The draw of a tile whose lyon origin is `tile_origin_unit_world`
    /// (`unit_world_per_tile_lyon` unit world per lyon unit, y growing
    /// south) into `grid`, writing `rank`. f64 throughout and relative to
    /// the mask origin, so the f32s are mask-local figures; the tile is
    /// taken in the world copy nearest the grid.
    fn new(grid: &crate::render::dot_mask::MaskGrid, tile_origin_unit_world: (f64, f64), unit_world_per_tile_lyon: f64, rank: u32) -> Self {
        let t = grid.texels_per_unit_world();
        let texels_per_lyon = unit_world_per_tile_lyon * t;
        let tile_texel = grid.tile_origin_texels(tile_origin_unit_world);
        let (w, h) = (f64::from(grid.size[0]), f64::from(grid.size[1]));
        Self {
            scale: [(2.0 * texels_per_lyon / w) as f32, (-2.0 * texels_per_lyon / h) as f32],
            offset: [(2.0 * tile_texel[0] / w - 1.0) as f32, (1.0 - 2.0 * tile_texel[1] / h) as f32],
            rank,
            _pad: [0; 3],
        }
    }
}

/// One basemap tile of a dot mask pass ([`TilePipeline::encode_dot_mask`]):
/// its buffers (whose mask index lists are drawn), where its lyon units
/// are in the world, and its side in unit world (`1 / 2^z`, for the cull).
pub struct MaskTileDraw<'a> {
    pub buffers: &'a TileBuffers,
    pub tile_origin_unit_world: (f64, f64),
    pub unit_world_per_tile_lyon: f64,
    pub tile_size_unit_world: f64,
}

impl<'a> MaskTileDraw<'a> {
    /// Basemap tile `tile`'s draw: its NW corner and side in unit world,
    /// and its lyon units (`MVT_EXTENT * COORD_SCALE` per tile side).
    #[must_use]
    pub fn for_tile(buffers: &'a TileBuffers, tile: crate::geo::TileId) -> Self {
        let n = f64::from(1u32 << tile.z);
        MaskTileDraw {
            buffers,
            tile_origin_unit_world: (f64::from(tile.x) / n, f64::from(tile.y) / n),
            unit_world_per_tile_lyon: 1.0 / (n * f64::from(crate::mvt::MVT_EXTENT) * f64::from(crate::render::mesh::COORD_SCALE)),
            tile_size_unit_world: 1.0 / n,
        }
    }
}

impl MaskTileDraw<'_> {
    /// Geometry past a tile's edge (the MVT buffer) this far, as a fraction
    /// of the tile, still counts for the cull: generous, since a wrong
    /// reject loses texels and a wrong accept costs one clipped draw.
    const CULL_MARGIN: f64 = 0.25;

    /// Whether any of the tile's triangles can land on `grid`.
    fn touches(&self, grid: &crate::render::dot_mask::MaskGrid) -> bool {
        let m = self.tile_size_unit_world * Self::CULL_MARGIN;
        let (x, y) = self.tile_origin_unit_world;
        let s = self.tile_size_unit_world;
        grid.touches([x - m, y - m, x + s + m, y + s + m])
    }
}

/// One compact draw unit on the GPU: 8-byte vertices, the fill's triangle
/// list and the hairline's line list over the same vertices.
pub struct CompactBuffers {
    vertices: crate::render::alloc::TrackedBuffer,
    fill: crate::render::alloc::TrackedBuffer,
    fill_format: wgpu::IndexFormat,
    fill_count: u32,
    outline: Option<crate::render::alloc::TrackedBuffer>,
    outline_format: wgpu::IndexFormat,
    outline_count: u32,
    /// `Some(byte offset)` when `vertices` is PLANAR: every vertex's `pos`
    /// (4 bytes each) from 0, then every vertex's `[paint, flags]` from
    /// this offset. The paint stream is contiguous, so a class change
    /// rewrites a feature's vertices' paint with one `write_buffer`
    /// ([`TilePipeline::write_compact_paint`]); the shader reads the same
    /// attributes either way.
    paint_offset: Option<u64>,
}

impl CompactBuffers {
    /// Bytes held by the three buffers.
    #[must_use]
    pub fn device_bytes(&self) -> u64 {
        self.vertices.tracked_size()
            + self.fill.tracked_size()
            + self.outline.as_ref().map_or(0, crate::render::alloc::TrackedBuffer::tracked_size)
    }

    /// Whether the vertices are planar (`paint_offset`).
    #[must_use]
    pub fn is_planar(&self) -> bool {
        self.paint_offset.is_some()
    }

    /// Whether there is a hairline to draw.
    #[must_use]
    pub fn has_outline(&self) -> bool {
        self.outline_count > 0
    }
}

/// A paint table on the GPU: an `Rgba32Float` texture, `textureLoad`ed by
/// `vs_compact`. Shared by every tile of a source.
pub struct PaintTableGpu {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    bytes: u64,
}

impl PaintTableGpu {
    /// Texture bytes.
    #[must_use]
    pub fn device_bytes(&self) -> u64 {
        self.bytes
    }
}

/// One draw in the tile pass. `Legacy` is the 88-byte vertex (and its own
/// A2 outline, if any); the two compact kinds draw a `CompactBuffers` with
/// a bind group from [`TilePipeline::create_compact_model_bind_group`] --
/// the fill's paint table for `CompactFill`, the outline's for
/// `CompactOutline`.
#[derive(Clone, Copy)]
pub enum TileDraw<'a> {
    Legacy(&'a TileBuffers, &'a wgpu::BindGroup),
    CompactFill(&'a CompactBuffers, &'a wgpu::BindGroup),
    CompactOutline(&'a CompactBuffers, &'a wgpu::BindGroup),
    /// Instanced circles: the instances, the model group (the overlay's own,
    /// off the circles' origin) and the circle group from
    /// [`TilePipeline::create_circle_bind_group`].
    Circles(&'a CircleBuffers, &'a wgpu::BindGroup, &'a wgpu::BindGroup),
    /// One tile of population dots: its dot group from
    /// [`TilePipeline::create_dot_bind_group`] (which holds the tile's block
    /// texture and the vertex count its params were laid out for), the
    /// model group (the overlay's, off the tile's NW corner), and whether
    /// the frame is flat (`transition == 0`), which draws `vs_dot_flat`.
    Dots(&'a DotGroupGpu, &'a wgpu::BindGroup, bool),
    /// One group's span of one extrusion chunk
    /// ([`crate::render::extrusion::GroupSpan`]): the chunk's buffers and
    /// table, the tile's model group, the layer's
    /// [`crate::render::extrusion_gpu::ExtrusionParamsGpu`] group, and which
    /// extrusion pipeline draws it.
    Extrusion {
        chunk: &'a crate::render::extrusion_gpu::ExtrusionBuffers,
        model: &'a wgpu::BindGroup,
        params: &'a wgpu::BindGroup,
        first_index: u32,
        index_count: u32,
        pass: crate::render::extrusion_gpu::ExtrusionPass,
    },
    /// Every model layer ([`crate::render::model::ModelPipeline`]), as
    /// prepared this frame: drawn in the tile pass so it shares the
    /// buildings' depth. Pushed after the extrusions.
    Models(&'a crate::render::model::ModelPipeline),
    /// The trips layers ([`crate::render::trips::TripPipeline`]), as
    /// prepared this frame, through their depth-pass pipelines (testing and
    /// writing no depth): pushed right before [`Self::Models`] on a frame
    /// that draws both ([`push_trips_and_models`]), so a 3D bus or car
    /// stands over its trail rather than under it.
    Trips(&'a crate::render::trips::TripPipeline),
}

/// The frame's 3D models, after the trips when there are any drawn this
/// frame: the ground layers (overlay lines, trails, stop glows, walkers)
/// first, then the meshes depth-tested against themselves and the
/// buildings, so nothing flat paints over a model standing on it --
/// MapLibre's order, where a 3D layer drawn after a line layer covers it
/// (`painter.ts` `getDepthModeFor3D`: `LEQUAL`, read-write). `trips` is
/// `None` when no trips layer draws, or when they draw in the symbol pass
/// as on a frame with no model.
pub fn push_trips_and_models<'a>(
    trips: Option<&'a crate::render::trips::TripPipeline>,
    models: &'a crate::render::model::ModelPipeline,
    draws: &mut Vec<TileDraw<'a>>,
) {
    if let Some(trips) = trips {
        draws.push(TileDraw::Trips(trips));
    }
    draws.push(TileDraw::Models(models));
}

impl TileDraw<'_> {
    /// Triangles this draw rasterises: a triangle list's index count / 3,
    /// two per instanced circle. A hairline outline draws lines, not
    /// triangles, and counts 0 here (it still counts as a draw call where
    /// one is counted). Population dots report 0: their vertex count is
    /// decided on the GPU.
    #[must_use]
    pub fn triangles(&self) -> u64 {
        match self {
            // The 88-byte body only: a split BASEMAP tile's compact half is
            // not counted (no overlay draw is ever split).
            TileDraw::Legacy(buffers, _) => u64::from(buffers.index_count) / 3,
            TileDraw::CompactFill(buffers, _) => u64::from(buffers.fill_count) / 3,
            TileDraw::CompactOutline(..) | TileDraw::Dots(..) => 0,
            TileDraw::Circles(buffers, _, _) => u64::from(buffers.count) * 2,
            TileDraw::Extrusion { index_count, .. } => u64::from(*index_count) / 3,
            TileDraw::Models(models) => models.stats().triangles,
            TileDraw::Trips(trips) => trips.stats().instances * 2,
        }
    }
}

/// One tile's dot blocks on the GPU (`render::dots`): every bucket's
/// `DotBlock`s in one `Rgba32Uint` texture, bucket after bucket (and, when
/// live, one texel of `pop_b` per block after them), drawn as ONE
/// non-instanced draw. See `shader.wgsl`'s `dot_blocks`.
///
/// **Dead blocks are skipped, not drawn.** Within a bucket the blocks are
/// sorted by `max(pop, pop_b) / first_dot`, descending: a block holds a
/// dot that can be alive at the view's `ppd` only while `first_dot <
/// max / ppd`, so the blocks that can draw anything are a PREFIX of each
/// bucket. [`Self::layout`] finds each prefix (a binary search over
/// `keys`) and lays the draw out over the prefixes only -- the slots a
/// tile carries between rebuilds cost nothing on the GPU, except for the
/// partly live block at each cell's edge. MG-8. (`needs_rebuild` allows
/// 2x, the controller's MG-8 ruling.)
///
/// **Overflow**: a tile holding more blocks than the texture can (`2048 x
/// max_texture_dimension_2d`, 4 M on WebGL2 -- above any budget) keeps
/// the blocks with the HIGHEST keys, i.e. drops the ones that die first as
/// `ppd` rises, counts them in [`crate::render::dots::dot_blocks_dropped_total`]
/// and says so once on stderr.
pub struct DotTileGpu {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
    /// The race dots' composition texture (binding 4), texel for texel
    /// with the blocks; `None` for a tile without one.
    comps: Option<(wgpu::Texture, wgpu::TextureView)>,
    /// The race dots' shape texture (binding 5), likewise.
    shapes: Option<(wgpu::Texture, wgpu::TextureView)>,
    bytes: u64,
    /// Per bucket, `max(pop, pop_b) / first_dot` of each block in texture
    /// order (descending; `inf` for `first_dot == 0`).
    keys: [Vec<f32>; 4],
    seg_block: [u32; 4],
    pop_b_offset: u32,
    live: bool,
    /// Blocks dropped because the tile held more than the texture can
    /// (`DOT_TEXTURE_WIDTH * max_texture_dimension_2d`); 0 in practice.
    pub dropped_blocks: u64,
}

impl DotTileGpu {
    /// Texture bytes.
    #[must_use]
    pub fn device_bytes(&self) -> u64 {
        self.bytes
    }

    /// Blocks of bucket `i` that can hold a live dot at view `ppd`: those
    /// with `first_dot < max / ppd`, i.e. key `> ppd`, a prefix. The key is
    /// compared with a 1e-5 margin, so an f32 rounding of the division can
    /// only ever draw one more dead block, never skip a live one.
    fn live_prefix(&self, i: usize, ppd: f32) -> usize {
        let limit = ppd * (1.0 - 1e-5);
        self.keys[i].partition_point(|&k| k > limit)
    }

    /// `params` with this tile's bucket layout for `params.ppd` and its
    /// live flag filled in, and the vertex count a draw with those params
    /// must issue -- returned TOGETHER, because the two are only right as a
    /// pair: `DotGroupGpu` stores both.
    #[must_use]
    ///
    /// A triangle tile's params must have the cell-only terms off (C12:
    /// the race style's job, which the shader does not branch on) --
    /// checked here, where every draw's params pass.
    pub fn layout(&self, params: crate::render::dots::DotParams) -> (crate::render::dots::DotParams, u32) {
        debug_assert!(
            params.shape != crate::render::dots::SHAPE_TRIANGLE || params.cell_terms_off(),
            "a triangle tile draws with relocation, the density shrink or the overlap fade on (C12): {params:?}"
        );
        let mut seg_vertex = [0u32; 4];
        let mut vertices = 0u64;
        for (i, slot) in seg_vertex.iter_mut().enumerate() {
            *slot = vertices as u32;
            vertices += u64::from(crate::render::dots::DOT_VERTICES)
                * u64::from(crate::render::dots::DOT_BUCKETS[i])
                * self.live_prefix(i, params.ppd) as u64;
        }
        let laid_out = crate::render::dots::DotParams {
            seg_vertex,
            seg_block: self.seg_block,
            pop_b_offset: self.pop_b_offset,
            live: u32::from(self.live),
            ..params
        };
        // At most 2048 x 2048 blocks of 64 dots, 3 vertices each: 8e8.
        (laid_out, vertices as u32)
    }
}

/// A dot tile's group 2 -- ramp, `DotParams` uniform, block texture, dot
/// mask -- the mask generation it was bound to, and the vertex count the
/// params in its uniform were laid out for. One per (tile, draw): the
/// count and the uniform's `seg_vertex` are written together by
/// [`TilePipeline::write_dot_params`], so two groups over the same tile at
/// different `ppd`s each draw their own layout. Write before encoding the
/// frame's draw: the uniform is read at submit, the count at encode.
pub struct DotGroupGpu {
    params: crate::render::alloc::TrackedBuffer,
    group: wgpu::BindGroup,
    vertices: std::sync::atomic::AtomicU32,
    /// `DotMaskGpu::generation` of the mask bound at binding 3, `None` for
    /// the 1x1 land texel: a group whose generation is not the live mask's
    /// is stale and must be recreated before it relocates against it.
    mask_generation: Option<u64>,
}

impl DotGroupGpu {
    /// The generation of the mask this group binds (`None`: no mask).
    #[must_use]
    pub fn mask_generation(&self) -> Option<u64> {
        self.mask_generation
    }

    /// Vertices this group's draw issues (3 per slot of each bucket's live
    /// prefix at the `ppd` last written).
    #[must_use]
    pub fn vertices(&self) -> u32 {
        self.vertices.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Bytes of the params uniform.
    #[must_use]
    pub fn device_bytes(&self) -> u64 {
        self.params.tracked_size()
    }
}

/// A layer of instanced circles on the GPU. See `render::circles`.
pub struct CircleBuffers {
    instances: crate::render::alloc::TrackedBuffer,
    count: u32,
}

impl CircleBuffers {
    /// Bytes held by the instance buffer.
    #[must_use]
    pub fn device_bytes(&self) -> u64 {
        self.instances.tracked_size()
    }

    /// Instances drawn.
    #[must_use]
    pub fn len(&self) -> u32 {
        self.count
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

impl<'a> From<(&'a TileBuffers, &'a wgpu::BindGroup)> for TileDraw<'a> {
    fn from((buffers, model): (&'a TileBuffers, &'a wgpu::BindGroup)) -> Self {
        Self::Legacy(buffers, model)
    }
}

/// Everything drawn BEHIND the basemap tiles, and nothing at all on a flat
/// map.
///
/// One parameter on [`TilePipeline::render_with_space`] rather than two,
/// because the two halves are not independent: a space clear colour without
/// the ocean sphere turns every untiled stretch of ocean into a hole, and
/// the ocean sphere without a space clear colour is invisible. See
/// `render::space`'s module docs for that argument in full.
///
/// [`Self::NONE`] is the flat map, and it is what every pre-existing call
/// site passes — `TilePipeline::render` delegates with it, so the flat
/// command stream is unchanged by construction rather than by inspection.
#[derive(Default)]
pub struct SpaceDraw<'a> {
    /// The star field, drawn FIRST so the backdrop and the tiles cover it.
    /// `None` when `Space::draws_stars` is false, which includes every
    /// flat frame and every default-configured consumer.
    pub stars: Option<&'a crate::render::space::StarPipeline>,
    /// The style's `sky` above the limb, drawn right after the stars and
    /// before the ocean sphere and the tiles -- MapLibre draws its sky first
    /// too (`painter.ts`). `None` when `SpacePlan::draws_sky` is false,
    /// which includes every flat frame. See `render::sky`.
    pub sky: Option<&'a crate::render::sky::SkyPipeline>,
    /// The ocean sphere: `render::space::backdrop_mesh` for the current
    /// background colour, with a model bind group carrying
    /// `render::space::BACKDROP_TILE`'s `GlobeParams`.
    pub backdrop: Option<(&'a TileBuffers, &'a wgpu::BindGroup)>,
    /// The limb glow, drawn LAST — over the tiles, not behind them. `None`
    /// when `render::atmosphere::Atmosphere::draws` is false, which includes
    /// every flat frame. Named on the same struct as the two things drawn
    /// behind the tiles although it is drawn in front of them, because the
    /// three share one gate shape and one reason to exist: none of them is a
    /// flat map's business.
    pub atmosphere: Option<&'a crate::render::atmosphere::AtmospherePipeline>,
    /// The opt-in space look (`render::space_look`): its star-map sky FIRST,
    /// before the point stars, and its clouds and shading after the tiles,
    /// before the limb glow. `None` unless a consumer turned the look on,
    /// so every default frame's command stream is unchanged.
    pub look: Option<&'a crate::render::space_look::SpaceLookPipeline>,
}

impl SpaceDraw<'_> {
    /// No backdrop, no stars and no atmosphere: the flat map, and every call
    /// site that predates the globe having anything behind it.
    pub const NONE: Self = Self { stars: None, sky: None, backdrop: None, atmosphere: None, look: None };
}

/// The raster basemap's draws for one frame: the pipeline, and one entry
/// per tile.
///
/// # Why this is its own parameter and not a fourth `SpaceDraw` field
///
/// `SpaceDraw` is the three things that are "not a flat map's business"
/// -- its own doc comment says so, and every one of them is gated on a
/// globe transition. A raster basemap is emphatically a flat map's
/// business; it is the *ground*. Filing it there would make that doc
/// comment false and would put raster behind the same mental gate as the
/// star field.
///
/// Each entry is `(square, model bind group, paint bind group)`:
///
///  - the **square** is shared by every tile at one zoom
///    (`render::raster_mesh`), so several entries commonly name one
///    buffer pair;
///  - the **model bind group** is the tile's own, the very same one the
///    vector draw for that tile binds -- see
///    `TilePipeline::shared_layouts`;
///  - the **paint bind group** is the tile's texture, the shared sampler
///    and this frame's resolved `raster-opacity`.
pub struct RasterDraws<'a> {
    pub pipeline: &'a crate::render::raster_pipeline::RasterPipeline,
    pub draws: &'a [(
        &'a crate::render::raster_pipeline::RasterBuffers,
        &'a wgpu::BindGroup,
        &'a wgpu::BindGroup,
    )],
    /// How many of the pass's tile draws go BEFORE the imagery. 0 -- every
    /// imagery basemap -- puts it under every vector layer; the count of
    /// basemap tile draws puts a data raster (a COG) over the basemap's
    /// fills and lines and under the overlays, the labels and the
    /// buildings, so the basemap shows wherever its texels are
    /// transparent. See `MapView::set_raster_above_basemap`.
    pub after_tile_draws: usize,
}

impl TilePipeline {
    pub fn new(ctx: &GpuContext, color_format: wgpu::TextureFormat) -> Self {
        Self::from_source(ctx, color_format, include_str!("shader.wgsl"))
    }

    /// [`Self::new`] from a caller-supplied shader source.
    ///
    /// **Exists for ONE reason and it is a measurement.** The ground shading
    /// this crate added to `fs_main` is four lines inside the shader that
    /// draws every tile, not a separate pass, so there is no draw to switch
    /// off and no uniform that can turn it into a no-op — the only A/B that
    /// isolates its cost is two pipelines from two shader sources. That is
    /// `the_ground_shading_costs_less_than_the_plans_half_millisecond`, and
    /// the same mechanism is what makes a shader-level sabotage check
    /// possible at all (`the_ground_shading_is_what_darkens_the_night_side`).
    ///
    /// `pub(crate)` and never called outside `#[cfg(test)]`; `new` is the
    /// only entry point a consumer has, and it is the one that pins the
    /// source to the shipped file.
    pub(crate) fn from_source(
        ctx: &GpuContext,
        color_format: wgpu::TextureFormat,
        source: &str,
    ) -> Self {
        let shader = ctx.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("gpui-maps tile shader"),
            source: wgpu::ShaderSource::Wgsl(crate::projection::specialise_wgsl(source).into_owned().into()),
        });

        let camera_bgl = ctx.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("camera-bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                count: None,
            }],
        });
        // VERTEX | FRAGMENT: `fs_main` reads `model.line_fx` for the line
        // trim. Every pipeline sharing this layout (raster, extrusion) sees
        // a superset of the visibility it needs, which wgpu accepts.
        let model_bgl = ctx.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("model-bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                count: None,
            }],
        });

        let layout = ctx.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("tile-pipeline-layout"),
            bind_group_layouts: &[Some(&camera_bgl), Some(&model_bgl)],
            immediate_size: 0,
        });

        // ORDER IS THE CONTRACT: `vertex_attr_array!` lays these out by
        // accumulating each format's size in declaration order, so this list
        // must mirror `render::mesh::Vertex`'s FIELD order exactly, not just
        // its field set. Locations 7/8 are `gap_half_px`/`gap_half_px_to`,
        // which sit between `half_width_px_to` and `line_distance` in the
        // struct; `line_distance` and `dash_runs` moved to 9/10 with them.
        // `line_progress` is location 11, appended LAST to `Vertex` so no
        // earlier offset moved.
        const ATTRS: [wgpu::VertexAttribute; 12] = wgpu::vertex_attr_array![
            0 => Float32x2, 1 => Float32x2, 2 => Float32x4, 3 => Float32, 4 => Float32,
            5 => Float32x4, 6 => Float32, 7 => Float32, 8 => Float32, 9 => Float32,
            10 => Float32x4, 11 => Float32
        ];
        let vertex_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Vertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &ATTRS,
        };

        let depth_format = wgpu::TextureFormat::Depth24PlusStencil8;

        // THREE pipelines off one shader, one layout and one vertex layout,
        // differing ONLY in `depth_write_enabled` and whether they write
        // colour -- see `backdrop` and `backdrop_depth` on this struct for
        // why the second and third exist.
        let build_with = |label: &'static str,
                          depth_write: bool,
                          color: wgpu::ColorWrites,
                          entry_point: &'static str,
                          topology: wgpu::PrimitiveTopology| {
            ctx.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some(entry_point),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: &[Some(vertex_layout.clone())],
                },
                primitive: wgpu::PrimitiveState { topology, ..wgpu::PrimitiveState::default() },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: depth_format,
                    depth_write_enabled: Some(depth_write),
                    // `LessEqual`, not `Less`: all v1-scope geometry sits at z=0 (no
                    // extrusion/terrain yet), so every fragment in a tile's
                    // `PaintPlan` is coplanar. `Less` would reject a same-depth
                    // fragment outright, silently discarding any style layer drawn
                    // after the first at the same pixel. `LessEqual` makes
                    // same-depth fragments use draw order ("last drawn wins") as
                    // the tiebreaker, while leaving real depth testing fully
                    // intact for any future geometry with actual elevation
                    // variation.
                    depth_compare: Some(wgpu::CompareFunction::LessEqual),
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
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: color,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let build = |label: &'static str, depth_write: bool, color: wgpu::ColorWrites| {
            build_with(label, depth_write, color, "vs_main", wgpu::PrimitiveTopology::TriangleList)
        };
        let pipeline = build("tile-pipeline", false, wgpu::ColorWrites::ALL);
        let backdrop = build("tile-backdrop-pipeline", false, wgpu::ColorWrites::ALL);
        let backdrop_depth =
            build("tile-backdrop-depth-pipeline", true, wgpu::ColorWrites::empty());
        // A fill's hairline outline: a LINE LIST over the fill's own
        // vertices, MapLibre's `fillOutline` program drawn with `gl.LINES`
        // (`draw_fill.ts:132-133`). `LineList` is core WebGPU and every wgpu
        // backend maps it natively -- Metal `MTLPrimitiveTypeLine`, Vulkan
        // `LINE_LIST`, and the GL backend (WebGL2 included) `GL_LINES` --
        // so there is no downlevel flag to check and no fallback to carry.
        // Hardware lines are one pixel wide everywhere, which is exactly
        // the outline's width in the reference. `vs_outline` reads the
        // outline colour the fill vertex carries in its stroke-only slots
        // and adds no inter-stage variable.
        let outline = build_with(
            "tile-fill-outline-pipeline",
            false,
            wgpu::ColorWrites::ALL,
            "vs_outline",
            wgpu::PrimitiveTopology::LineList,
        );

        // The compact pair: `vs_compact` over an 8-byte vertex, whose model
        // group adds the paint table. Same blend, depth (`LessEqual`, no
        // write) and multisample state as `pipeline`, so a compact fill
        // composites exactly as the legacy fill it replaces.
        let compact_model_bgl = ctx.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("compact-model-bgl"),
            entries: &[
                // The model uniform, `VERTEX | FRAGMENT` as in `model_bgl`:
                // the compact fills draw through `fs_main`, which reads
                // `model.line_fx`.
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        let compact_layout = ctx.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("compact-pipeline-layout"),
            bind_group_layouts: &[Some(&camera_bgl), Some(&compact_model_bgl)],
            immediate_size: 0,
        });
        let compact_vertex_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<crate::render::compact::CompactVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &crate::render::compact::COMPACT_ATTRS,
        };
        let tile_table_bgl = ctx.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("tile-paint-table-bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            }],
        });
        let compact_tile_layout = ctx.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("compact-tile-pipeline-layout"),
            bind_group_layouts: &[Some(&camera_bgl), Some(&model_bgl), Some(&tile_table_bgl)],
            immediate_size: 0,
        });
        let build_with_fragment = |label: &'static str,
                                   topology: wgpu::PrimitiveTopology,
                                   layout: &wgpu::PipelineLayout,
                                   entry_point: &'static str,
                                   vertex_layouts: &[Option<wgpu::VertexBufferLayout<'_>>],
                                   fragment_entry: &'static str| {
            ctx.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some(entry_point),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: vertex_layouts,
                },
                primitive: wgpu::PrimitiveState { topology, ..wgpu::PrimitiveState::default() },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: depth_format,
                    depth_write_enabled: Some(false),
                    depth_compare: Some(wgpu::CompareFunction::LessEqual),
                    stencil: wgpu::StencilState::default(),
                    bias: wgpu::DepthBiasState::default(),
                }),
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(fragment_entry),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: color_format,
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let build_compact_with = |label: &'static str,
                                  topology: wgpu::PrimitiveTopology,
                                  layout: &wgpu::PipelineLayout,
                                  entry_point: &'static str,
                                  vertex_layouts: &[Option<wgpu::VertexBufferLayout<'_>>]| {
            build_with_fragment(label, topology, layout, entry_point, vertex_layouts, "fs_main")
        };
        let build_compact = |label: &'static str, topology: wgpu::PrimitiveTopology| {
            build_compact_with(label, topology, &compact_layout, "vs_compact", &[Some(compact_vertex_layout.clone())])
        };
        let compact_fill = build_compact("tile-compact-fill-pipeline", wgpu::PrimitiveTopology::TriangleList);
        let compact_outline = build_compact("tile-compact-outline-pipeline", wgpu::PrimitiveTopology::LineList);
        // The same attributes from two streams of one buffer: positions,
        // then `[paint, flags]` (see `CompactBuffers::paint_offset`).
        let planar_layouts = [
            Some(wgpu::VertexBufferLayout {
                array_stride: 4,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &crate::render::compact::COMPACT_PLANAR_POS_ATTRS,
            }),
            Some(wgpu::VertexBufferLayout {
                array_stride: 4,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &crate::render::compact::COMPACT_PLANAR_PAINT_ATTRS,
            }),
        ];
        let compact_fill_planar = build_compact_with(
            "tile-compact-fill-planar-pipeline",
            wgpu::PrimitiveTopology::TriangleList,
            &compact_layout,
            "vs_compact",
            &planar_layouts,
        );
        let compact_outline_planar = build_compact_with(
            "tile-compact-outline-planar-pipeline",
            wgpu::PrimitiveTopology::LineList,
            &compact_layout,
            "vs_compact",
            &planar_layouts,
        );
        let compact_tile = build_compact_with(
            "tile-compact-basemap-pipeline",
            wgpu::PrimitiveTopology::TriangleList,
            &compact_tile_layout,
            "vs_compact_tile",
            &[Some(compact_vertex_layout.clone())],
        );
        let compact_stroke = build_compact_with(
            "tile-compact-stroke-pipeline",
            wgpu::PrimitiveTopology::TriangleList,
            &compact_tile_layout,
            "vs_compact_stroke",
            &[Some(wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<crate::render::compact::CompactStrokeVertex>() as wgpu::BufferAddress,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &crate::render::compact::COMPACT_STROKE_ATTRS,
            })],
        );

        // Instanced circles: the quad is `@builtin(vertex_index)`, so the
        // one vertex buffer is the instances (`step_mode: Instance`) and the
        // pipeline stays inside `max_vertex_buffers: 1`. WebGL2 has
        // instanced draws (and wgpu's GL backend emits `gl_VertexID`); only
        // a non-zero FIRST instance would need a downlevel flag, and every
        // draw starts at 0.
        let circle_bgl = ctx.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("circle-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let circle_layout = ctx.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("circle-pipeline-layout"),
            bind_group_layouts: &[Some(&camera_bgl), Some(&model_bgl), Some(&circle_bgl)],
            immediate_size: 0,
        });
        let circles = build_compact_with(
            "overlay-circle-pipeline",
            wgpu::PrimitiveTopology::TriangleStrip,
            &circle_layout,
            "vs_circle",
            &[Some(wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<crate::render::circles::CircleInstance>() as wgpu::BufferAddress,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &crate::render::circles::CIRCLE_ATTRS,
            })],
        );
        // Procedural population dots (`render::dots`): `vs_dot` pulls
        // each vertex's block from the tile's `DotBlock` texture by
        // `vertex_index` -- NO vertex buffer and NO instancing, which MG-8
        // measured as the dominant cost of a 6-vertex instance per block.
        // Group 2 is the circle group plus the block texture.
        let dot_bgl = ctx.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("dot-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Uint,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                // The dot mask (`render::dot_mask`, R8Uint), read by
                // `mask_rank` with `textureLoad` in the vertex stage.
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Uint,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                // The race dots' composition (4) and shape (5) textures,
                // `Rgba32Uint`, parallel to the block texture (C10).
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Uint,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 5,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Uint,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        let dot_layout = ctx.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("dot-pipeline-layout"),
            bind_group_layouts: &[Some(&camera_bgl), Some(&model_bgl), Some(&dot_bgl)],
            immediate_size: 0,
        });
        // A globe frame's dots: `vs_dot` / `fs_dot` (4 inter-stage
        // locations, not `fs_main`'s 11, and no `discard` for a clip box or
        // trim a dot never has).
        let dots = build_with_fragment(
            "overlay-dot-pipeline",
            wgpu::PrimitiveTopology::TriangleList,
            &dot_layout,
            "vs_dot",
            &[],
            "fs_dot",
        );
        // A flat frame's dots: the same slots through `vs_dot_flat` /
        // `fs_dot_flat` (3 inter-stage locations, not 11). MG-8.
        let dots_flat = build_with_fragment(
            "overlay-dot-flat-pipeline",
            wgpu::PrimitiveTopology::TriangleList,
            &dot_layout,
            "vs_dot_flat",
            &[],
            "fs_dot_flat",
        );

        // The dot mask pass: position only (location 0 of either vertex
        // layout, at offset 0), a per-draw uniform at a dynamic offset, no
        // depth, no blend, sample count 1 -- an integer target has no blend
        // and is never the MSAA target.
        let mask_bgl = ctx.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("dot-mask-bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: true,
                    min_binding_size: wgpu::BufferSize::new(std::mem::size_of::<MaskDraw>() as u64),
                },
                count: None,
            }],
        });
        let mask_layout = ctx.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("dot-mask-pipeline-layout"),
            bind_group_layouts: &[Some(&mask_bgl)],
            immediate_size: 0,
        });
        let build_mask = |label: &'static str, entry_point: &'static str, buffer: Option<wgpu::VertexBufferLayout<'_>>| {
            let buffers: Vec<Option<wgpu::VertexBufferLayout<'_>>> = buffer.into_iter().map(Some).collect();
            ctx.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&mask_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some(entry_point),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: &buffers,
                },
                primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleList, ..wgpu::PrimitiveState::default() },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_mask"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: wgpu::TextureFormat::R8Uint,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let mask_compact = build_mask(
            "dot-mask-compact-pipeline",
            "vs_mask_compact",
            Some(wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<crate::render::compact::CompactVertex>() as wgpu::BufferAddress,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &[wgpu::VertexAttribute { format: wgpu::VertexFormat::Sint16x2, offset: 0, shader_location: 0 }],
            }),
        );
        let mask_legacy = build_mask(
            "dot-mask-legacy-pipeline",
            "vs_mask_legacy",
            Some(wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<Vertex>() as wgpu::BufferAddress,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &[wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x2, offset: 0, shader_location: 0 }],
            }),
        );
        let mask_quad = build_mask("dot-mask-quad-pipeline", "vs_mask_quad", None);
        let land_mask = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("dot-mask-land"),
            size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Uint,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        ctx.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &land_mask,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &[crate::overlay::dotgrid::mask_class::MaskRank::Land as u8],
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: None, rows_per_image: None },
            wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
        );
        let land_mask_view = land_mask.create_view(&wgpu::TextureViewDescriptor::default());
        // A 1x1 zero `Rgba32Uint` texel: the composition and shape
        // bindings of a tile without them (world dots, C10). Never read:
        // the shader loads them only under their colour mode and shape.
        let zero_texel = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("dot-zero-texel"),
            size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba32Uint,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        ctx.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &zero_texel,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &[0u8; 16],
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: None, rows_per_image: None },
            wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
        );
        let zero_texel_view = zero_texel.create_view(&wgpu::TextureViewDescriptor::default());
        let extrusion =
            crate::render::extrusion_gpu::ExtrusionPipeline::new(ctx, color_format, &camera_bgl, &model_bgl, depth_format);

        Self {
            pipeline,
            backdrop,
            backdrop_depth,
            outline,
            compact_fill,
            compact_outline,
            compact_fill_planar,
            compact_outline_planar,
            compact_tile,
            compact_stroke,
            circles,
            circle_bgl,
            dots,
            dots_flat,
            dot_bgl,
            mask_compact,
            mask_legacy,
            mask_quad,
            mask_bgl,
            land_mask_view,
            zero_texel_view,
            dot_counts: Default::default(),
            tile_table_bgl,
            camera_bgl,
            model_bgl,
            compact_model_bgl,
            depth_format,
            extrusion,
        }
    }

    /// The `fill-extrusion` pipeline, built over this pipeline's camera and
    /// model layouts and depth format.
    #[must_use]
    pub fn extrusion(&self) -> &crate::render::extrusion_gpu::ExtrusionPipeline {
        &self.extrusion
    }

    /// Swaps in another extrusion pipeline: the front-face test's culling
    /// controls.
    #[cfg(test)]
    pub(crate) fn replace_extrusion_for_test(&mut self, extrusion: crate::render::extrusion_gpu::ExtrusionPipeline) {
        self.extrusion = extrusion;
    }

    /// Uploads a tile's extrusion mesh: per chunk a vertex buffer, an index
    /// buffer in the chunk's own format and an `Rgba32Float` table
    /// [`crate::render::extrusion::TABLE_WIDTH`] texels wide, padded with
    /// zeros to whole rows, with its group-2 bind group. `None` for an
    /// empty mesh: no zero-size buffer or texture is ever created.
    pub fn create_extrusion_buffers(
        &self,
        ctx: &GpuContext,
        mesh: &crate::render::extrusion::ExtrusionMesh,
    ) -> Option<crate::render::extrusion_gpu::ExtrusionGpu> {
        use crate::render::extrusion::TABLE_WIDTH;
        use crate::render::extrusion_gpu::{table_rows, ExtrusionBuffers, ExtrusionGpu};
        if mesh.is_empty() {
            return None;
        }
        crate::render::alloc::note_mesh_install();
        let init = |label: &'static str, contents: &[u8], usage: wgpu::BufferUsages| {
            crate::render::alloc::TrackedBuffer::init(
                &ctx.device,
                &wgpu::util::BufferInitDescriptor { label: Some(label), contents, usage },
            )
        };
        // A chunk of a non-empty mesh can still be empty (every chunk but
        // one of them is not, in practice); four bytes keep it bindable and
        // its spans draw nothing.
        let pad = |b: &[u8]| if b.is_empty() { vec![0u8; 4] } else { b.to_vec() };
        let chunks = mesh
            .chunks
            .iter()
            .map(|chunk| {
                let vertex = init(
                    "extrusion-vertices",
                    &pad(bytemuck::cast_slice(&chunk.vertices)),
                    wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                );
                let index = init("extrusion-indices", &pad(chunk.indices.bytes()), wgpu::BufferUsages::INDEX);
                let rows = table_rows(chunk.table.len());
                let size = wgpu::Extent3d { width: TABLE_WIDTH as u32, height: rows, depth_or_array_layers: 1 };
                let table = ctx.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("extrusion-table"),
                    size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba32Float,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                });
                Self::write_extrusion_texels(ctx, &table, &chunk.table);
                let view = table.create_view(&wgpu::TextureViewDescriptor::default());
                let table_bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("extrusion-table-bg"),
                    layout: self.extrusion.table_layout(),
                    entries: &[wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) }],
                });
                ExtrusionBuffers {
                    vertex,
                    index,
                    index_format: chunk.indices.format(),
                    table,
                    table_bind_group,
                    spans: chunk.spans.clone(),
                }
            })
            .collect();
        Some(ExtrusionGpu { chunks })
    }

    /// Rewrites each chunk's table in place from `mesh` (after
    /// `render::extrusion::rewrite_table`), as [`Self::write_paint_table`]
    /// does: every bind group keeps its texture. A chunk whose table no
    /// longer fits its texture's rows, or a chunk count that differs, is
    /// skipped -- that is a re-upload, never a rewrite.
    pub fn write_extrusion_table(
        &self,
        ctx: &GpuContext,
        gpu: &crate::render::extrusion_gpu::ExtrusionGpu,
        mesh: &crate::render::extrusion::ExtrusionMesh,
    ) {
        if gpu.chunks.len() != mesh.chunks.len() {
            return;
        }
        for (buffers, chunk) in gpu.chunks.iter().zip(&mesh.chunks) {
            if crate::render::extrusion_gpu::table_rows(chunk.table.len()) != buffers.table.size().height {
                continue;
            }
            Self::write_extrusion_texels(ctx, &buffers.table, &chunk.table);
        }
    }

    /// `table`, zero-padded to the texture's whole rows, into `texture`.
    fn write_extrusion_texels(ctx: &GpuContext, texture: &wgpu::Texture, table: &[[f32; 4]]) {
        use crate::render::extrusion::TABLE_WIDTH;
        let size = texture.size();
        let texels = crate::render::extrusion_gpu::padded_table(table, size.height);
        ctx.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(&texels),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(TABLE_WIDTH as u32 * 16),
                rows_per_image: Some(size.height),
            },
            size,
        );
    }

    /// Uploads a [`crate::render::compact::CompactMesh`]: fresh buffers
    /// sized to the mesh (no outline buffer without an outline).
    pub fn create_compact_buffers(&self, ctx: &GpuContext, mesh: &crate::render::compact::CompactMesh) -> CompactBuffers {
        crate::render::alloc::note_mesh_install();
        let init = |label: &'static str, contents: &[u8], usage: wgpu::BufferUsages| {
            crate::render::alloc::TrackedBuffer::init(
                &ctx.device,
                &wgpu::util::BufferInitDescriptor { label: Some(label), contents, usage },
            )
        };
        // Four bytes rather than zero keeps an empty mesh bindable; its
        // zero count means it is never drawn.
        let pad = |b: &[u8]| if b.is_empty() { vec![0u8; 4] } else { b.to_vec() };
        let vertices = init(
            "compact-vertices",
            &pad(bytemuck::cast_slice(&mesh.vertices)),
            wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        );
        let fill = init("compact-fill-indices", &pad(mesh.fill.bytes()), wgpu::BufferUsages::INDEX);
        let outline = (!mesh.outline.is_empty())
            .then(|| init("compact-outline-indices", mesh.outline.bytes(), wgpu::BufferUsages::INDEX));
        CompactBuffers {
            vertices,
            fill,
            fill_format: mesh.fill.format(),
            fill_count: mesh.fill.len() as u32,
            outline,
            outline_format: mesh.outline.format(),
            outline_count: mesh.outline.len() as u32,
            paint_offset: None,
        }
    }

    /// [`Self::create_compact_buffers`], with the vertices PLANAR (see
    /// `CompactBuffers::paint_offset`): the same bytes, positions first.
    pub fn create_compact_buffers_planar(
        &self,
        ctx: &GpuContext,
        mesh: &crate::render::compact::CompactMesh,
    ) -> CompactBuffers {
        let n = mesh.vertices.len();
        let mut bytes: Vec<u8> = Vec::with_capacity((8 * n).max(8));
        for v in &mesh.vertices {
            bytes.extend_from_slice(bytemuck::bytes_of(&v.pos));
        }
        for v in &mesh.vertices {
            bytes.extend_from_slice(bytemuck::bytes_of(&[v.paint, v.flags]));
        }
        if bytes.is_empty() {
            bytes.resize(8, 0);
        }
        let mut buffers = self.create_compact_buffers(ctx, &crate::render::compact::CompactMesh {
            vertices: Vec::new(),
            fill: mesh.fill.clone(),
            outline: mesh.outline.clone(),
        });
        buffers.vertices = crate::render::alloc::TrackedBuffer::init(
            &ctx.device,
            &wgpu::util::BufferInitDescriptor {
                label: Some("compact-vertices-planar"),
                contents: &bytes,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            },
        );
        buffers.paint_offset = Some(4 * n as u64);
        buffers
    }

    /// Rewrites vertices `first..first + paint.len()`'s `[paint, flags]`
    /// in a planar [`CompactBuffers`]. `false`, nothing written, for an
    /// interleaved one or a range past its vertices.
    pub fn write_compact_paint(&self, ctx: &GpuContext, buffers: &CompactBuffers, first: usize, paint: &[[u16; 2]]) -> bool {
        let Some(offset) = buffers.paint_offset else { return false };
        let end = offset + 4 * (first + paint.len()) as u64;
        if paint.is_empty() || end > 2 * offset {
            return paint.is_empty();
        }
        ctx.queue.write_buffer(&buffers.vertices, offset + 4 * first as u64, bytemuck::cast_slice(paint));
        true
    }

    /// Uploads a paint table as an `Rgba32Float` texture.
    pub fn create_paint_table(&self, ctx: &GpuContext, table: &crate::render::compact::PaintTable) -> PaintTableGpu {
        let size = wgpu::Extent3d { width: table.width, height: table.height, depth_or_array_layers: 1 };
        let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("paint-table"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba32Float,
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
            bytemuck::cast_slice(&table.texels),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(table.width * 16),
                rows_per_image: Some(table.height),
            },
            size,
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        PaintTableGpu { texture, view, bytes: u64::from(table.width) * u64::from(table.height) * 16 }
    }

    /// Rewrites an existing paint table in place -- the cheap half of a
    /// tiled source's paint update (`MapView::update_source_paint`): every
    /// bind group already built over `gpu` keeps pointing at the same
    /// texture, so no tile, mesh or bind group is touched. `false`, and
    /// nothing written, when `table` is a different size: a class count
    /// that changed rows is a re-bake, never a rewrite.
    pub fn write_paint_table(
        &self,
        ctx: &GpuContext,
        gpu: &PaintTableGpu,
        table: &crate::render::compact::PaintTable,
    ) -> bool {
        let size = gpu.texture.size();
        if size.width != table.width || size.height != table.height {
            return false;
        }
        ctx.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &gpu.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(&table.texels),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(table.width * 16),
                rows_per_image: Some(table.height),
            },
            size,
        );
        true
    }

    /// Uploads instanced circles.
    pub fn create_circle_buffers(&self, ctx: &GpuContext, instances: &[crate::render::circles::CircleInstance]) -> CircleBuffers {
        crate::render::alloc::note_mesh_install();
        let bytes: &[u8] = bytemuck::cast_slice(instances);
        let contents = if bytes.is_empty() { vec![0u8; 16] } else { bytes.to_vec() };
        let instances_buffer = crate::render::alloc::TrackedBuffer::init(
            &ctx.device,
            &wgpu::util::BufferInitDescriptor {
                label: Some("circle-instances"),
                contents: &contents,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            },
        );
        CircleBuffers { instances: instances_buffer, count: instances.len() as u32 }
    }

    /// A circle draw's group 2: its paint table and a `CircleParams`
    /// uniform (returned, to be rewritten per frame with
    /// [`Self::write_circle_params`]).
    pub fn create_circle_bind_group(
        &self,
        ctx: &GpuContext,
        table: &PaintTableGpu,
        params: crate::render::circles::CircleParams,
    ) -> (crate::render::alloc::TrackedBuffer, wgpu::BindGroup) {
        let buffer = crate::render::alloc::TrackedBuffer::init(&ctx.device, &wgpu::util::BufferInitDescriptor {
            label: Some("circle-params"),
            contents: bytemuck::bytes_of(&params),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("circle-bind-group"),
            layout: &self.circle_bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&table.view) },
                wgpu::BindGroupEntry { binding: 1, resource: buffer.as_entire_binding() },
            ],
        });
        (buffer, group)
    }

    /// What this pipeline has sent for dot tiles so far.
    #[must_use]
    pub fn dot_uploads(&self) -> DotUploads {
        let get = |i: usize| self.dot_counts[i].load(std::sync::atomic::Ordering::Relaxed);
        DotUploads { tile_uploads: get(0), groups_created: get(1), params_writes: get(2) }
    }

    /// Uploads one tile's bucketed dot blocks (`overlay::dotgrid::blocks::
    /// TileBlocks::{buckets, pop_b}`) as one `DotBlock` texture: bucket 1's
    /// blocks, then 4's, 16's and 64's, then -- when `pop_b` is non-empty
    /// (live mode) -- one texel per block whose `.x` is its `pop_b`.
    pub fn create_dot_tile(
        &self,
        ctx: &GpuContext,
        buckets: &[Vec<crate::render::dots::DotBlock>; 4],
        pop_b: &[Vec<f32>; 4],
    ) -> DotTileGpu {
        self.create_dot_tile_with(ctx, buckets, pop_b, &Default::default(), &Default::default())
    }

    /// [`Self::create_dot_tile`] with the race dots' per-block texels
    /// (`overlay::dotgrid::blocks::TileBlocks::{comps, shapes}`, parallel
    /// to `buckets`, each empty or complete): each goes into its own
    /// `Rgba32Uint` texture at the block's own texel index (group 2
    /// bindings 4 and 5, C10). Empty ones bind the pipeline's 1x1 zero
    /// texel instead.
    pub fn create_dot_tile_with(
        &self,
        ctx: &GpuContext,
        buckets: &[Vec<crate::render::dots::DotBlock>; 4],
        pop_b: &[Vec<f32>; 4],
        comps: &[Vec<[u32; 4]>; 4],
        shapes: &[Vec<[f32; 4]>; 4],
    ) -> DotTileGpu {
        let live = pop_b.iter().any(|b| !b.is_empty());
        let max_rows = ctx.device.limits().max_texture_dimension_2d;
        // Live mode doubles the texels a block needs.
        let capacity = u64::from(crate::render::dots::DOT_TEXTURE_WIDTH) * u64::from(max_rows) / if live { 2 } else { 1 };
        self.create_dot_tile_capped(ctx, buckets, pop_b, comps, shapes, capacity)
    }

    /// [`Self::create_dot_tile`] with the block capacity stated, so a test
    /// can overflow it without four million blocks.
    fn create_dot_tile_capped(
        &self,
        ctx: &GpuContext,
        buckets: &[Vec<crate::render::dots::DotBlock>; 4],
        pop_b: &[Vec<f32>; 4],
        comps: &[Vec<[u32; 4]>; 4],
        shapes: &[Vec<[f32; 4]>; 4],
        capacity: u64,
    ) -> DotTileGpu {
        use crate::render::dots::{DotBlock, DOT_TEXTURE_WIDTH};
        crate::render::alloc::note_mesh_install();
        self.dot_counts[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let live = pop_b.iter().any(|b| !b.is_empty());
        // Each bucket in descending key order (ties keep the builder's).
        let mut orders: [Vec<(f32, usize)>; 4] = Default::default();
        for (i, bucket) in buckets.iter().enumerate() {
            let top = |b: usize| if live { bucket[b].pop.max(pop_b[i][b]) } else { bucket[b].pop };
            let key = |b: usize| {
                if bucket[b].first_dot == 0 { f32::INFINITY } else { top(b) / bucket[b].first_dot as f32 }
            };
            orders[i] = (0..bucket.len()).map(|b| (key(b), b)).collect();
            orders[i].sort_by(|a, b| b.0.total_cmp(&a.0));
        }
        // Over capacity: keep the highest keys across ALL buckets -- the
        // blocks that die last as `ppd` rises -- which is a prefix of each
        // bucket's order.
        let total: u64 = orders.iter().map(|o| o.len() as u64).sum();
        let mut takes = orders.each_ref().map(Vec::len);
        if total > capacity {
            let mut all: Vec<(f32, usize)> =
                orders.iter().enumerate().flat_map(|(i, o)| o.iter().map(move |&(k, _)| (k, i))).collect();
            all.sort_by(|a, b| b.0.total_cmp(&a.0));
            takes = [0; 4];
            for &(_, i) in &all[..capacity as usize] {
                takes[i] += 1;
            }
            crate::render::dots::note_dot_blocks_dropped(total - capacity);
        }
        let mut texels: Vec<DotBlock> = Vec::new();
        let mut pops: Vec<f32> = Vec::new();
        let has_comps = comps.iter().any(|c| !c.is_empty());
        let has_shapes = shapes.iter().any(|c| !c.is_empty());
        // Each side vec is empty for the whole tile or parallel to every
        // bucket: the loop below indexes it by the bucket's own blocks.
        for i in 0..4 {
            debug_assert!(comps[i].len() == if has_comps { buckets[i].len() } else { 0 }, "comps[{i}] is not parallel to its bucket");
            debug_assert!(shapes[i].len() == if has_shapes { buckets[i].len() } else { 0 }, "shapes[{i}] is not parallel to its bucket");
        }
        let mut comp_texels: Vec<[u32; 4]> = Vec::new();
        let mut shape_texels: Vec<[f32; 4]> = Vec::new();
        let mut seg_block = [0u32; 4];
        let mut keys: [Vec<f32>; 4] = Default::default();
        for (i, order) in orders.iter().enumerate() {
            seg_block[i] = texels.len() as u32;
            let kept = &order[..takes[i]];
            texels.extend(kept.iter().map(|&(_, b)| buckets[i][b]));
            if live {
                pops.extend(kept.iter().map(|&(_, b)| pop_b[i][b]));
            }
            if has_comps {
                comp_texels.extend(kept.iter().map(|&(_, b)| comps[i][b]));
            }
            if has_shapes {
                shape_texels.extend(kept.iter().map(|&(_, b)| shapes[i][b]));
            }
            keys[i] = kept.iter().map(|&(k, _)| k).collect();
        }
        // One `Rgba32Uint` texture of `data`'s texels, DOT_TEXTURE_WIDTH
        // a row, as the block texture is laid out.
        let side_texture = |label: &str, data: &[u8]| -> (wgpu::Texture, wgpu::TextureView, u64) {
            let n = (data.len() / 16) as u32;
            let rows = n.div_ceil(DOT_TEXTURE_WIDTH).max(1);
            let mut padded = data.to_vec();
            padded.resize((rows * DOT_TEXTURE_WIDTH * 16) as usize, 0);
            let size = wgpu::Extent3d { width: DOT_TEXTURE_WIDTH, height: rows, depth_or_array_layers: 1 };
            let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba32Uint,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            ctx.queue.write_texture(
                wgpu::TexelCopyTextureInfo { texture: &texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                &padded,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(DOT_TEXTURE_WIDTH * 16), rows_per_image: Some(rows) },
                size,
            );
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            (texture, view, u64::from(rows) * u64::from(DOT_TEXTURE_WIDTH) * 16)
        };
        let mut side_bytes = 0u64;
        let comps_gpu = has_comps.then(|| {
            let (t, v, b) = side_texture("dot-comps", bytemuck::cast_slice(&comp_texels));
            side_bytes += b;
            (t, v)
        });
        let shapes_gpu = has_shapes.then(|| {
            let (t, v, b) = side_texture("dot-shapes", bytemuck::cast_slice(&shape_texels));
            side_bytes += b;
            (t, v)
        });
        let dropped = total - texels.len() as u64;
        let blocks = texels.len() as u32;
        let pop_b_offset = if live { blocks } else { 0 };
        texels.extend(pops.iter().map(|&p| DotBlock { origin: [p, 0.0], pop: 0.0, first_dot: 0 }));
        let rows = (texels.len() as u32).div_ceil(DOT_TEXTURE_WIDTH).max(1);
        texels.resize((rows * DOT_TEXTURE_WIDTH) as usize, DotBlock { origin: [0.0; 2], pop: 0.0, first_dot: 0 });
        let size = wgpu::Extent3d { width: DOT_TEXTURE_WIDTH, height: rows, depth_or_array_layers: 1 };
        let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("dot-blocks"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba32Uint,
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
            bytemuck::cast_slice(&texels),
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(DOT_TEXTURE_WIDTH * 16), rows_per_image: Some(rows) },
            size,
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        DotTileGpu {
            _texture: texture,
            view,
            comps: comps_gpu,
            shapes: shapes_gpu,
            bytes: u64::from(rows) * u64::from(DOT_TEXTURE_WIDTH) * 16 + side_bytes,
            keys,
            seg_block,
            pop_b_offset,
            live,
            dropped_blocks: dropped,
        }
    }

    /// Recreates the texture if `grid.size` changed; clears to MASK_CLEAR_RANK; draws every
    /// tile's green lists, then every tile's water, then every building (the last write wins,
    /// so building > water > green); sets the key; bumps the generation.
    ///
    /// A tile whose rect (with a quarter-tile margin for the MVT buffer)
    /// misses the grid is skipped. Returns the number of draws encoded.
    ///
    /// Each list is one indexed draw over the body its indices address (the
    /// split tile's compact vertices, or the 88-byte ones), with its
    /// [`MaskDraw`] at its own dynamic offset of one uniform buffer made for
    /// this pass: the pass runs only when the key changes, so the buffer is
    /// not kept.
    pub fn encode_dot_mask(
        &self,
        ctx: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        mask: &mut Option<crate::render::dot_mask::DotMaskGpu>,
        key: crate::render::dot_mask::MaskKey,
        tiles: &[MaskTileDraw<'_>],
    ) -> usize {
        self.encode_dot_mask_timed(ctx, encoder, mask, key, tiles, None)
    }

    /// [`Self::encode_dot_mask`] with the pass's `timestamp_writes`
    /// (`mask_timer::MaskPassTimer`, probe builds).
    pub fn encode_dot_mask_timed(
        &self,
        ctx: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        mask: &mut Option<crate::render::dot_mask::DotMaskGpu>,
        key: crate::render::dot_mask::MaskKey,
        tiles: &[MaskTileDraw<'_>],
        timestamp_writes: Option<wgpu::RenderPassTimestampWrites<'_>>,
    ) -> usize {
        use crate::overlay::dotgrid::mask_class::MaskRank;
        let grid = key.grid;
        if mask.as_ref().is_none_or(|m| m.size() != grid.size) {
            let generation = mask.as_ref().map_or(0, crate::render::dot_mask::DotMaskGpu::generation);
            *mask = Some(crate::render::dot_mask::DotMaskGpu::new(ctx, grid.size, generation));
        }
        let target = mask.as_mut().expect("just made");

        // `MaskBuffers`' lists are `[water, green, building]`; draw order is
        // green, water, building.
        const ORDER: [(usize, MaskRank); 3] = [(1, MaskRank::Green), (0, MaskRank::Water), (2, MaskRank::Building)];
        struct Draw<'b> {
            /// `None`: a tile footprint (`vs_mask_quad`, six vertices).
            compact: Option<bool>,
            vertices: &'b wgpu::Buffer,
            indices: &'b wgpu::Buffer,
            range: std::ops::Range<u32>,
            uniform: MaskDraw,
        }
        let mut draws: Vec<Draw<'_>> = Vec::new();
        // The texture clears to no data; every drawn tile's footprint is land
        // first, whether or not it carries ranked fills, so only texels no
        // tile covers stay no data (DP-5 ruling).
        for tile in tiles {
            if !tile.touches(&grid) {
                continue;
            }
            let uniform = MaskDraw::new(&grid, tile.tile_origin_unit_world, tile.tile_size_unit_world, MaskRank::Land as u32);
            let buffer = &tile.buffers.vertex_buffer;
            draws.push(Draw { compact: None, vertices: buffer, indices: buffer, range: 0..6, uniform });
        }
        for (list, rank) in ORDER {
            for tile in tiles {
                let Some(lists) = tile.buffers.mask.as_ref() else { continue };
                if !tile.touches(&grid) {
                    continue;
                }
                let uniform = MaskDraw::new(&grid, tile.tile_origin_unit_world, tile.unit_world_per_tile_lyon, rank as u32);
                if let (Some((indices, ranges)), Some(split)) = (&lists.compact, &tile.buffers.split) {
                    if !ranges[list].is_empty() {
                        draws.push(Draw { compact: Some(true), vertices: &split.compact.vertices, indices, range: ranges[list].clone(), uniform });
                    }
                }
                if let Some((indices, ranges)) = &lists.legacy {
                    if !ranges[list].is_empty() {
                        draws.push(Draw {
                            compact: Some(false),
                            vertices: &tile.buffers.vertex_buffer,
                            indices,
                            range: ranges[list].clone(),
                            uniform,
                        });
                    }
                }
            }
        }

        let stride = (std::mem::size_of::<MaskDraw>() as u64)
            .next_multiple_of(u64::from(ctx.device.limits().min_uniform_buffer_offset_alignment));
        let uniforms = (!draws.is_empty()).then(|| {
            let mut bytes = vec![0u8; (stride * draws.len() as u64) as usize];
            for (i, draw) in draws.iter().enumerate() {
                let at = i * stride as usize;
                bytes[at..at + std::mem::size_of::<MaskDraw>()].copy_from_slice(bytemuck::bytes_of(&draw.uniform));
            }
            let buffer = crate::render::alloc::TrackedBuffer::init(
                &ctx.device,
                &wgpu::util::BufferInitDescriptor { label: Some("dot-mask-draws"), contents: &bytes, usage: wgpu::BufferUsages::UNIFORM },
            );
            let group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("dot-mask-draws"),
                layout: &self.mask_bgl,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &buffer,
                        offset: 0,
                        size: wgpu::BufferSize::new(std::mem::size_of::<MaskDraw>() as u64),
                    }),
                }],
            });
            (buffer, group)
        });

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("dot-mask-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target.view(),
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: f64::from(crate::render::dot_mask::MASK_CLEAR_RANK),
                        g: 0.0,
                        b: 0.0,
                        a: 0.0,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        if let Some((_buffer, group)) = &uniforms {
            for (i, draw) in draws.iter().enumerate() {
                pass.set_bind_group(0, group, &[(i as u64 * stride) as u32]);
                let Some(compact) = draw.compact else {
                    pass.set_pipeline(&self.mask_quad);
                    pass.draw(draw.range.clone(), 0..1);
                    continue;
                };
                pass.set_pipeline(if compact { &self.mask_compact } else { &self.mask_legacy });
                pass.set_vertex_buffer(0, draw.vertices.slice(..));
                pass.set_index_buffer(draw.indices.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(draw.range.clone(), 0, 0..1);
            }
        }
        drop(pass);
        target.drawn(key);
        draws.len()
    }

    /// The 1x1 texel of rank 3 bound when a draw has no mask.
    #[must_use]
    pub fn land_mask_view(&self) -> &wgpu::TextureView {
        &self.land_mask_view
    }

    /// A dot tile's group 2: the paint table holding its density ramp, a
    /// `DotParams` uniform (`params` with the tile's bucket layout filled
    /// in by [`DotTileGpu::layout`], to be rewritten per frame with
    /// [`Self::write_dot_params`] as the view's `ppd` moves) and the tile's
    /// block texture -- with the vertex count those params need -- and the
    /// dot mask: `mask`'s texture, or [`Self::land_mask_view`] when `None`
    /// (then `params.mask_k` should be 0: every candidate reads land).
    pub fn create_dot_bind_group(
        &self,
        ctx: &GpuContext,
        ramp: &PaintTableGpu,
        tile: &DotTileGpu,
        mask: Option<&crate::render::dot_mask::DotMaskGpu>,
        params: crate::render::dots::DotParams,
    ) -> DotGroupGpu {
        let mask_view = mask.map_or(&self.land_mask_view, crate::render::dot_mask::DotMaskGpu::view);
        self.dot_counts[1].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (laid_out, vertices) = tile.layout(params);
        let buffer = crate::render::alloc::TrackedBuffer::init(&ctx.device, &wgpu::util::BufferInitDescriptor {
            label: Some("dot-params"),
            contents: bytemuck::bytes_of(&laid_out),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("dot-bind-group"),
            layout: &self.dot_bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&ramp.view) },
                wgpu::BindGroupEntry { binding: 1, resource: buffer.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&tile.view) },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(mask_view) },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::TextureView(tile.comps.as_ref().map_or(&self.zero_texel_view, |c| &c.1)),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: wgpu::BindingResource::TextureView(tile.shapes.as_ref().map_or(&self.zero_texel_view, |c| &c.1)),
                },
            ],
        });
        DotGroupGpu {
            params: buffer,
            group,
            vertices: std::sync::atomic::AtomicU32::new(vertices),
            mask_generation: mask.map(crate::render::dot_mask::DotMaskGpu::generation),
        }
    }

    /// Rewrites a dot group's `DotParams` (the view `ppd`, the live mix,
    /// the exclusion boxes) for `tile` -- the tile the group was created
    /// over -- and the group's vertex count with it.
    pub fn write_dot_params(
        &self,
        ctx: &GpuContext,
        dots: &DotGroupGpu,
        tile: &DotTileGpu,
        params: crate::render::dots::DotParams,
    ) {
        let (laid_out, vertices) = tile.layout(params);
        self.dot_counts[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        ctx.queue.write_buffer(&dots.params, 0, bytemuck::bytes_of(&laid_out));
        dots.vertices.store(vertices, std::sync::atomic::Ordering::Relaxed);
    }

    /// Draws one dot tile into an open pass whose groups 0 and 1 the caller
    /// has bound: its group 2, then ONE non-instanced draw of every
    /// bucket's live prefix (no vertex buffer, no `first_instance`). `flat`
    /// (the frame's `transition == 0`) draws `vs_dot_flat`. Returns the
    /// draw calls issued.
    pub fn draw_dot_tile(&self, pass: &mut wgpu::RenderPass<'_>, dots: &DotGroupGpu, flat: bool) -> u64 {
        pass.set_pipeline(if flat { &self.dots_flat } else { &self.dots });
        Self::encode_dot_tile(pass, dots)
    }

    fn encode_dot_tile(pass: &mut wgpu::RenderPass<'_>, dots: &DotGroupGpu) -> u64 {
        let vertices = dots.vertices();
        if vertices == 0 {
            return 0;
        }
        pass.set_bind_group(2, &dots.group, &[]);
        pass.draw(0..vertices, 0..1);
        1
    }

    /// Rewrites a circle group's exclusion boxes.
    pub fn write_circle_params(&self, ctx: &GpuContext, buffer: &wgpu::Buffer, params: crate::render::circles::CircleParams) {
        ctx.queue.write_buffer(buffer, 0, bytemuck::bytes_of(&params));
    }

    /// A compact draw's model group: `model` (a `ModelUniform` buffer,
    /// which other bind groups may share) and the paint table it reads.
    pub fn create_compact_model_bind_group(&self, ctx: &GpuContext, model: &wgpu::Buffer, table: &PaintTableGpu) -> wgpu::BindGroup {
        ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("compact-model-bind-group"),
            layout: &self.compact_model_bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: model.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&table.view) },
            ],
        })
    }

    /// A 88-byte-vertex draw's model group over an existing `model` buffer
    /// (e.g. one from [`Self::create_model_buffer_clipped`]).
    pub fn create_legacy_model_bind_group(&self, ctx: &GpuContext, model: &wgpu::Buffer) -> wgpu::BindGroup {
        ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("model-bind-group"),
            layout: &self.model_bgl,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: model.as_entire_binding() }],
        })
    }

    /// A model uniform buffer alone, for a caller that makes its bind
    /// groups itself (compact and legacy groups can share one buffer).
    #[allow(clippy::too_many_arguments)]
    pub fn create_model_buffer_clipped(&self, ctx: &GpuContext, matrix: [[f32; 4]; 4], blend_t: f32, dash_scale: f32, globe: GlobeParams, clip: TileClip) -> crate::render::alloc::TrackedBuffer {
        self.create_model_buffer_fx(ctx, matrix, blend_t, dash_scale, globe, clip, LINE_FX_NONE)
    }

    /// [`Self::create_model_buffer_clipped`] carrying `line_fx` (see
    /// [`Self::write_model_uniform_fx`]).
    #[allow(clippy::too_many_arguments)]
    pub fn create_model_buffer_fx(&self, ctx: &GpuContext, matrix: [[f32; 4]; 4], blend_t: f32, dash_scale: f32, globe: GlobeParams, clip: TileClip, line_fx: [f32; 4]) -> crate::render::alloc::TrackedBuffer {
        let uniform = ModelUniform::new(matrix, blend_t, dash_scale, globe, clip, line_fx);
        crate::render::alloc::TrackedBuffer::init(&ctx.device, &wgpu::util::BufferInitDescriptor {
            label: Some("model-uniform"),
            contents: bytemuck::bytes_of(&uniform),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        })
    }

    /// The depth format this pipeline's pass attaches. Read by
    /// `render::space::StarPipeline::new`, which draws into the SAME pass
    /// and so has to declare a matching format.
    #[must_use]
    pub fn depth_format(&self) -> wgpu::TextureFormat {
        self.depth_format
    }

    /// The camera and model bind group layouts, for a pipeline that draws
    /// into this pass and binds **this pipeline's own** camera and model
    /// bind groups.
    ///
    /// `render::raster_pipeline` is the caller, and the sharing is the
    /// point rather than a convenience. A raster tile needs exactly the
    /// per-tile uniform a fill tile needs -- the same
    /// `combined_tile_matrix`, the same `GlobeParams`, the same `blend_t`
    /// -- so it binds the same `ModelUniform` buffer
    /// `create_model_bind_group` already wrote, and reads it through a
    /// WGSL `Model` struct declared to match. Handing out the layout is
    /// what lets a second pipeline do that; building a second, identical
    /// layout would compile, and would then be one unnoticed edit away
    /// from the two structs disagreeing about a 224-byte uniform.
    #[must_use]
    pub fn shared_layouts(&self) -> (&wgpu::BindGroupLayout, &wgpu::BindGroupLayout) {
        (&self.camera_bgl, &self.model_bgl)
    }

    pub fn create_depth_view(&self, ctx: &GpuContext, width: u32, height: u32) -> wgpu::TextureView {
        let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("depth"),
            size: wgpu::Extent3d { width: width.max(1), height: height.max(1), depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.depth_format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        texture.create_view(&wgpu::TextureViewDescriptor::default())
    }

    /// Updates an existing camera uniform buffer's contents in place, without
    /// touching the bind group at all — `wgpu::Queue::write_buffer` is a
    /// plain memcpy into the buffer wgpu already allocated for it. Used every
    /// frame (the camera moves every frame) in place of recreating the buffer
    /// and bind group via `create_camera_bind_group`, which only needs to run
    /// once per `GpuState` lifetime.
    pub fn write_camera_uniform(&self, ctx: &GpuContext, buffer: &wgpu::Buffer, view_proj: [[f32; 4]; 4], zoom: f32, device_pixel_ratio: f32) {
        let uniform = CameraUniform { view_proj, zoom, device_pixel_ratio, fog_range: [0.0; 2], fog_color: [0.0; 4], circle_frame: [0.0; 4] };
        ctx.queue.write_buffer(buffer, 0, bytemuck::bytes_of(&uniform));
    }

    /// Writes the fade into an existing camera uniform, AFTER
    /// `write_camera_uniform` (which leaves it off). Bytes 72..96 only.
    pub fn write_horizon_fog(&self, ctx: &GpuContext, buffer: &wgpu::Buffer, fog: HorizonFog) {
        let bytes: [f32; 6] = [fog.start, fog.end, fog.color[0], fog.color[1], fog.color[2], fog.color[3]];
        ctx.queue.write_buffer(buffer, 72, bytemuck::cast_slice(&bytes));
    }

    /// Writes the circle frame into an existing camera uniform, AFTER
    /// `write_camera_uniform` (which leaves it zero): `camera_to_center` is
    /// `Camera::distance()`, what `circle-pitch-scale: viewport` divides by.
    /// Bytes 96..112 only.
    pub fn write_circle_frame(&self, ctx: &GpuContext, buffer: &wgpu::Buffer, camera_to_center: f32) {
        let bytes: [f32; 4] = [camera_to_center, 0.0, 0.0, 0.0];
        ctx.queue.write_buffer(buffer, CIRCLE_FRAME_OFFSET, bytemuck::cast_slice(&bytes));
    }

    /// Rewrites an existing model-uniform buffer's contents in place -- a
    /// plain memcpy into the buffer wgpu already allocated for it, mirroring
    /// `write_camera_uniform`'s shape. Used whenever the camera has changed
    /// since the last frame (see `MapView::paint`'s `camera_changed` check) in
    /// place of recreating the buffer/bind group via `create_model_bind_group`,
    /// which only needs to run once per tile's lifetime in the `gpu_meshes`
    /// cache.
    ///
    /// Writes [`TileClip::WHOLE`]: this rewrites a TILE's own cached
    /// uniform, and a tile drawn as itself paints its whole box. A clipped
    /// stand-in gets its own buffer from
    /// [`Self::create_model_bind_group_clipped`] instead, precisely so that
    /// this cached one is never the thing a clip is written into — the same
    /// cached mesh is also drawn unclipped, in the same frame, for the tile
    /// it belongs to.
    pub fn write_model_uniform(&self, ctx: &GpuContext, buffer: &wgpu::Buffer, matrix: [[f32; 4]; 4], blend_t: f32, dash_scale: f32, globe: GlobeParams) {
        self.write_model_uniform_fx(ctx, buffer, matrix, blend_t, dash_scale, globe, TileClip::WHOLE, LINE_FX_NONE);
    }

    /// The whole model uniform, `line_fx` INCLUDED -- the per-frame write
    /// for a draw that carries a trim or a fade (an overlay source's,
    /// `MapView::set_source_line_trim` / `set_source_opacity`). The value
    /// is part of the one write, so no later whole-uniform write in the
    /// frame can reset it, and no write order has to be remembered
    /// (story-content T5 review F3). `line_fx` is normalised
    /// ([`normalise_line_fx`]); [`LINE_FX_NONE`] makes this exactly
    /// [`Self::write_model_uniform_clipped`].
    #[allow(clippy::too_many_arguments)]
    pub fn write_model_uniform_fx(&self, ctx: &GpuContext, buffer: &wgpu::Buffer, matrix: [[f32; 4]; 4], blend_t: f32, dash_scale: f32, globe: GlobeParams, clip: TileClip, line_fx: [f32; 4]) {
        let uniform = ModelUniform::new(matrix, blend_t, dash_scale, globe, clip, line_fx);
        ctx.queue.write_buffer(buffer, 0, bytemuck::bytes_of(&uniform));
    }

    /// [`Self::write_model_uniform`] with a clip: a tiled overlay tile's
    /// OWN uniform, which always carries a clip -- its own box
    /// (`TileClip::for_descendant(tile, tile)`), so the 1024-unit buffer it
    /// shares with its neighbours is painted once, by whichever tile owns
    /// it. MapLibre's per-tile stencil, in effect.
    ///
    /// Only the native `MapView` overlay path calls this, hence
    /// `#[allow(dead_code)]` as on `install_overlay_chunk`.
    #[allow(dead_code)]
    #[allow(clippy::too_many_arguments)]
    pub fn write_model_uniform_clipped(&self, ctx: &GpuContext, buffer: &wgpu::Buffer, matrix: [[f32; 4]; 4], blend_t: f32, dash_scale: f32, globe: GlobeParams, clip: TileClip) {
        self.write_model_uniform_fx(ctx, buffer, matrix, blend_t, dash_scale, globe, clip, LINE_FX_NONE);
    }

    /// Writes only `line_fx` -- `(keep_start, keep_end, opacity,
    /// metrics_on)` -- into an existing model uniform, bytes 224..240.
    /// `[keep_start, keep_end]` is the VISIBLE range, the complement of
    /// `line-trim-offset`. A full range (`<= 0`, `>= 1`) is written with
    /// the trim OFF ([`normalise_line_fx`]).
    ///
    /// Every `create_model_*` and `write_model_uniform*` without `_fx`
    /// writes [`LINE_FX_NONE`], so a caller of THIS calls it after any
    /// whole-uniform write of the same buffer in the frame; the queue
    /// applies writes in order. A draw whose uniform is rewritten every
    /// frame should use [`Self::write_model_uniform_fx`] instead, which
    /// cannot be ordered wrong.
    pub fn write_model_line_fx(&self, ctx: &GpuContext, model: &wgpu::Buffer, line_fx: [f32; 4]) {
        ctx.queue.write_buffer(model, LINE_FX_OFFSET, bytemuck::cast_slice(&normalise_line_fx(line_fx)));
    }

    pub fn create_camera_bind_group(&self, ctx: &GpuContext, view_proj: [[f32; 4]; 4], zoom: f32, device_pixel_ratio: f32) -> (crate::render::alloc::TrackedBuffer, wgpu::BindGroup) {
        let uniform = CameraUniform { view_proj, zoom, device_pixel_ratio, fog_range: [0.0; 2], fog_color: [0.0; 4], circle_frame: [0.0; 4] };
        let buffer = crate::render::alloc::TrackedBuffer::init(&ctx.device, &wgpu::util::BufferInitDescriptor {
            label: Some("camera-uniform"),
            contents: bytemuck::bytes_of(&uniform),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("camera-bind-group"),
            layout: &self.camera_bgl,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: buffer.as_entire_binding() }],
        });
        (buffer, bind_group)
    }

    /// A model bind group for a draw that paints its tile's WHOLE box —
    /// every caller that is not an ancestor stand-in. Delegates to
    /// [`Self::create_model_bind_group_clipped`] with [`TileClip::WHOLE`],
    /// so the two cannot lay the uniform out differently.
    pub fn create_model_bind_group(&self, ctx: &GpuContext, matrix: [[f32; 4]; 4], blend_t: f32, dash_scale: f32, globe: GlobeParams) -> (crate::render::alloc::TrackedBuffer, wgpu::BindGroup) {
        self.create_model_bind_group_clipped(ctx, matrix, blend_t, dash_scale, globe, TileClip::WHOLE)
    }

    /// A model bind group whose draw may only paint inside `clip`, a box in
    /// the tile's own lyon coordinates — see [`TileClip`] for what that is
    /// for and why the clip is in tile space rather than a scissor.
    ///
    /// **Its own buffer, not a tile's cached one.** The mesh a clipped
    /// stand-in draws belongs to the ancestor tile, which may be drawn
    /// unclipped in the same frame and is certainly drawn unclipped in some
    /// later one; and one ancestor standing in for two missing descendants
    /// needs two different clips over the same buffers. So a clipped draw
    /// carries a uniform of its own, built per frame and dropped with the
    /// frame, exactly as the overlay's per-source uniforms are.
    pub fn create_model_bind_group_clipped(&self, ctx: &GpuContext, matrix: [[f32; 4]; 4], blend_t: f32, dash_scale: f32, globe: GlobeParams, clip: TileClip) -> (crate::render::alloc::TrackedBuffer, wgpu::BindGroup) {
        self.create_model_bind_group_fx(ctx, matrix, blend_t, dash_scale, globe, clip, LINE_FX_NONE)
    }

    /// [`Self::create_model_bind_group_clipped`] carrying `line_fx` (see
    /// [`Self::write_model_uniform_fx`]).
    #[allow(clippy::too_many_arguments)]
    pub fn create_model_bind_group_fx(&self, ctx: &GpuContext, matrix: [[f32; 4]; 4], blend_t: f32, dash_scale: f32, globe: GlobeParams, clip: TileClip, line_fx: [f32; 4]) -> (crate::render::alloc::TrackedBuffer, wgpu::BindGroup) {
        let uniform = ModelUniform::new(matrix, blend_t, dash_scale, globe, clip, line_fx);
        let buffer = crate::render::alloc::TrackedBuffer::init(&ctx.device, &wgpu::util::BufferInitDescriptor {
            label: Some("model-uniform"),
            contents: bytemuck::bytes_of(&uniform),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("model-bind-group"),
            layout: &self.model_bgl,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: buffer.as_entire_binding() }],
        });
        (buffer, bind_group)
    }

    /// A camera bind group at a device pixel ratio of 1, so a test's
    /// rendered output is in the same units as the coordinates it asserts
    /// on. Anything checking the stroke's antialiasing band specifically
    /// should call `create_camera_bind_group` directly with a real ratio.
    #[cfg(test)]
    fn test_camera_bind_group(&self, ctx: &GpuContext, view_proj: [[f32; 4]; 4], zoom: f32) -> wgpu::BindGroup {
        self.create_camera_bind_group(ctx, view_proj, zoom, 1.0).1
    }

    /// A model bind group with no globe, which is what every test predating
    /// the globe wants: `GlobeParams::FLAT` makes `shader.wgsl` take its
    /// Mercator early-out, so those tests render exactly what they rendered
    /// before the projection existed.
    #[cfg(test)]
    pub(crate) fn test_model_bind_group(&self, ctx: &GpuContext, matrix: [[f32; 4]; 4], blend_t: f32, dash_scale: f32) -> wgpu::BindGroup {
        self.create_model_bind_group(ctx, matrix, blend_t, dash_scale, GlobeParams::FLAT).1
    }

    /// Installs `mesh` into a FRESH pair of GPU buffers. Equivalent to
    /// [`Self::install_mesh`] with nothing to reuse, and the right call
    /// wherever there genuinely is nothing: a tile arriving in
    /// `MapView`'s `gpu_meshes` for the first time is a different tile from
    /// whatever the LRU last evicted, not the same one changing size.
    pub fn create_tile_buffers(&self, ctx: &GpuContext, mesh: &TileMesh) -> TileBuffers {
        self.install_mesh(ctx, None, mesh)
    }

    /// Uploads `mesh` into `existing` where it fits, and allocates only
    /// where it does not -- the grow-and-keep rule: capacity is a high-water
    /// mark that rises to whatever the largest mesh so far needed and never
    /// falls back.
    ///
    /// This is the single place a `TileMesh` -- a tile's or the overlay's --
    /// becomes GPU buffers, so it is the single place `mesh_installs_total`
    /// is counted. Note that an install is still counted when nothing is
    /// allocated: `mesh_installs_total` climbing while
    /// `gpu_buffers_created_total` stays flat is not a discrepancy, it is
    /// the whole point of the pair (see `render::alloc`).
    ///
    /// # Why this is not exact-size reuse
    ///
    /// Reusing only on an exact size match would be nearly useless for the
    /// case that motivates this. The overlay's vertex count moves with its
    /// feature set, so a caller pushing a live feed re-pushes a slightly
    /// different count almost every time and would miss an exact-match
    /// guard on almost every push -- reallocating a ~200 MB buffer per
    /// tick, which is the behaviour being fixed. A high-water mark converges
    /// to the largest set the caller pushes, usually within a handful of
    /// pushes, and then stops allocating entirely.
    ///
    /// The cost is holding capacity for the largest mesh ever installed even
    /// after a smaller one replaces it. That is the intended trade: the
    /// alternative is trading a fixed high-water mark for an unbounded
    /// allocation rate. Where the overlay stops being drawn at all,
    /// `MapView::paint` still drops the buffers outright rather than keeping
    /// ~200 MB for a mesh that is no longer on screen -- shrinking is a
    /// decision for the owner of the buffers, not for this function.
    ///
    /// # Stale bytes past the end
    ///
    /// A mesh smaller than the buffers it lands in leaves the previous
    /// mesh's bytes past its own end, in both buffers. Nothing draws them:
    /// `render` issues `draw_indexed(0..index_count)` and `index_count` is
    /// reset here on every install, so the leftover indices are never
    /// fetched and the leftover vertices are never referenced.
    pub fn install_mesh(
        &self,
        ctx: &GpuContext,
        existing: Option<TileBuffers>,
        mesh: &TileMesh,
    ) -> TileBuffers {
        crate::render::alloc::note_mesh_install();
        let (old_vertex, old_index) = match existing {
            Some(TileBuffers { vertex_buffer, index_buffer, .. }) => (Some(vertex_buffer), Some(index_buffer)),
            None => (None, None),
        };

        // REFUSE A MESH THAT CANNOT FIT, RATHER THAN LETTING WGPU KILL THE
        // PROCESS OVER IT.
        //
        // Asking the adapter for its own `max_buffer_size` (see
        // `required_limits`) moves this wall 16x on an M4 Pro; it does not
        // remove it, because the overlay still bakes into ONE buffer. What
        // this guard removes is the DEATH, and it has to sit here -- ahead of
        // the allocation -- because neither obvious alternative works:
        //
        //   - An uncaptured-error handler alone is not enough. wgpu returns an
        //     ERROR BUFFER from the failed `create_buffer`, and
        //     `create_buffer_init` immediately does
        //     `get_mapped_range_mut(..).expect(..)` on it, which panics in the
        //     unwind-hostile frame all over again.
        //   - Letting the error buffer through is worse than the abort. Its
        //     `.size()` is the size that was REQUESTED (`wgpu::Buffer` stores
        //     the descriptor's value whether or not the backend honoured it),
        //     so `TrackedBuffer` would cache the oversized figure and
        //     `fit_or_allocate`'s reuse branch would hand the same poisoned
        //     buffer back on every subsequent install FOREVER -- and binding
        //     it invalidates the whole render pass, taking the basemap down
        //     with the overlay. A permanently blank window that logs is not
        //     better than a crash; it is a crash you cannot see.
        //
        // So the mesh is dropped and empty buffers are installed instead.
        // `index_count: 0` makes `render` skip this entry outright (the
        // `continue` there), so nothing is bound, nothing is drawn, the device
        // stays valid, and the very next install of a mesh that DOES fit
        // repairs the picture. The basemap and every other tile are
        // untouched: only the offending mesh goes missing.
        let vertex_bytes = (mesh.vertices.len() * std::mem::size_of::<Vertex>()) as u64;
        let index_bytes = (mesh.indices.len() * std::mem::size_of::<u32>()) as u64;
        if vertex_bytes > ctx.max_buffer_size || index_bytes > ctx.max_buffer_size {
            note_mesh_too_large(vertex_bytes, index_bytes, ctx.max_buffer_size);
            return TileBuffers {
                vertex_buffer: fit_or_allocate(
                    ctx,
                    old_vertex,
                    &[],
                    "tile-vertices",
                    wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                ),
                index_buffer: fit_or_allocate(
                    ctx,
                    old_index,
                    &[],
                    "tile-indices",
                    wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
                ),
                index_count: 0,
                index_format: wgpu::IndexFormat::Uint32,
                outline: None,
                split: None,
                mask: None,
            };
        }

        let vertex_buffer = fit_or_allocate(
            ctx,
            old_vertex,
            bytemuck::cast_slice(&mesh.vertices),
            "tile-vertices",
            // `COPY_DST` (not just `VERTEX`) so `write_vertex_buffer`,
            // `write_vertex_range` and the reuse branch above can rewrite
            // this buffer's contents in place, instead of `MapView` having to
            // recreate it (and its bind group) every time -- mirrors why the
            // camera/model uniform buffers below carry `COPY_DST`.
            wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        );
        let index_buffer = fit_or_allocate(
            ctx,
            old_index,
            bytemuck::cast_slice(&mesh.indices),
            "tile-indices",
            // `COPY_DST` for the reuse branch only. Nothing rewrites indices
            // in place the way the paint-only path rewrites vertices -- a
            // paint change cannot alter topology -- but a reinstall can, and
            // without this flag every reinstall would have to reallocate.
            wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
        );
        TileBuffers {
            vertex_buffer,
            index_buffer,
            index_count: mesh.indices.len() as u32,
            index_format: wgpu::IndexFormat::Uint32,
            outline: None,
            split: None,
            mask: None,
        }
    }

    /// Uploads one overlay chunk -- a run of whole triangulation units, at
    /// most 65,536 vertices when `indices` is `U16` -- into its own vertex,
    /// index and (for a fill with `fill-outline-color`) outline buffers,
    /// reusing `existing`'s where they fit, exactly as [`Self::install_mesh`]
    /// does. Every index is already chunk-local.
    ///
    /// Only the native `MapView` overlay path calls this -- the iOS and
    /// browser shells have no GeoJSON overlays yet -- so it is
    /// `#[allow(dead_code)]` rather than `cfg`-gated, keeping `TilePipeline`
    /// one shape on every shell.
    #[allow(dead_code)]
    pub(crate) fn install_overlay_chunk(
        &self,
        ctx: &GpuContext,
        existing: Option<TileBuffers>,
        vertices: &[Vertex],
        indices: crate::render::mesh::ChunkIndices<'_>,
        outline: crate::render::mesh::ChunkIndices<'_>,
    ) -> TileBuffers {
        crate::render::alloc::note_mesh_install();
        let (old_vertex, old_index, old_outline) = match existing {
            Some(TileBuffers { vertex_buffer, index_buffer, outline, .. }) => {
                (Some(vertex_buffer), Some(index_buffer), outline.map(|(buffer, _)| buffer))
            }
            None => (None, None, None),
        };
        let usage_v = wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST;
        let usage_i = wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST;
        let vertex_buffer =
            fit_or_allocate(ctx, old_vertex, bytemuck::cast_slice(vertices), "overlay-vertices", usage_v);
        let index_buffer = fit_or_allocate(ctx, old_index, indices.bytes(), "overlay-indices", usage_i);
        let outline = (!outline.is_empty()).then(|| {
            (fit_or_allocate(ctx, old_outline, outline.bytes(), "overlay-outline-indices", usage_i), outline.len() as u32)
        });
        TileBuffers {
            vertex_buffer,
            index_buffer,
            index_count: indices.len() as u32,
            index_format: indices.format(),
            outline,
            split: None,
            mask: None,
        }
    }

    /// Rewrites an existing tile's vertex buffer contents in place -- a
    /// plain memcpy into the buffer wgpu already allocated for it, mirroring
    /// `write_camera_uniform`/`write_model_uniform`. Used when a zoom
    /// crossing changes a tile's paint values but not its geometry:
    /// `MapView::plan_for` rewrites the tile's cached `TileMesh`'s paint
    /// fields via `render::rewrite_paint_attributes`, and this just pushes
    /// the updated bytes, in place of re-tessellating and recreating the
    /// buffer/bind group via `create_tile_buffers`. `buffers`' index buffer
    /// is untouched -- vertex COUNT and topology are unchanged by a
    /// paint-only rewrite (`rewrite_paint_attributes` edits vertices in
    /// place; it never adds or removes any), so the existing index buffer
    /// and `buffers.index_count` both stay valid.
    ///
    /// Panics (via wgpu's validation) if `mesh.vertices`'s byte size differs
    /// from what `buffers.vertex_buffer` was created with -- callers must
    /// only pass a `mesh` whose vertex count matches the one `buffers` was
    /// built from, which the paint-only-rewrite contract above guarantees.
    ///
    /// A split basemap tile ([`Self::install_basemap_mesh`]) is re-split and
    /// its three parts rewritten in place: the 88-byte vertices, the compact
    /// vertices (whose paint indices may move) and the paint table. `false`,
    /// with nothing written, when the re-split no longer fits the tile's
    /// buffers -- a changed body size, or a paint table grown past its
    /// texture's rows (more than 256 distinct fill colour pairs, grown) --
    /// and the caller must reinstall the tile instead.
    pub fn write_vertex_buffer(&self, ctx: &GpuContext, buffers: &TileBuffers, mesh: &TileMesh) -> bool {
        let Some(split) = buffers.split.as_ref() else {
            ctx.queue.write_buffer(&buffers.vertex_buffer, 0, bytemuck::cast_slice(&mesh.vertices));
            return true;
        };
        let Some(fresh) = crate::render::compact::split_tile_mesh(mesh) else { return false };
        let table = fresh.table();
        let size = split.table.texture.size();
        if fresh.runs != split.runs
            || fresh.legacy.vertices.len() != split.legacy_vertices
            || fresh.compact.vertices.len() != split.compact_vertices
            || fresh.strokes.len() != split.stroke_vertices
            || table.width != size.width
            || table.height > size.height
        {
            return false;
        }
        if !fresh.legacy.vertices.is_empty() {
            ctx.queue.write_buffer(&buffers.vertex_buffer, 0, bytemuck::cast_slice(&fresh.legacy.vertices));
        }
        ctx.queue.write_buffer(&split.compact.vertices, 0, bytemuck::cast_slice(&fresh.compact.vertices));
        if let Some(strokes) = &split.strokes {
            // The paint offsets move with the fill entry count, so the
            // stroke vertices are rewritten with the table.
            ctx.queue.write_buffer(&strokes.vertices, 0, bytemuck::cast_slice(&fresh.strokes));
        }
        ctx.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &split.table.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(&table.texels),
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(table.width * 16), rows_per_image: Some(table.height) },
            wgpu::Extent3d { width: table.width, height: table.height, depth_or_array_layers: 1 },
        );
        true
    }

    /// [`Self::install_basemap_mesh`] into fresh buffers.
    pub fn create_basemap_tile_buffers(&self, ctx: &GpuContext, mesh: &TileMesh) -> TileBuffers {
        self.install_basemap_mesh(ctx, None, mesh)
    }

    /// A BASEMAP tile's [`Self::install_mesh`]: its fill triangles go on the
    /// 8-byte compact vertex and its undashed strokes on the 16-byte one,
    /// with a per-tile paint table, and only the rest (dashes, off-grid
    /// cuts) keeps the 88-byte vertex in the ordinary buffers
    /// (`render::compact::split_tile_mesh`). The tile then draws as
    /// alternating runs of the bodies, in op order, through the same
    /// `TileDraw::Legacy` and the same model bind group as before -- so a
    /// frame is pixel-identical to the unsplit tile.
    ///
    /// A tile with no fill or undashed stroke, or with
    /// `GPUI_MAP_BASEMAP_COMPACT=0`
    /// ([`crate::render::compact::basemap_compact_enabled`]), installs
    /// unsplit; `GPUI_MAP_BASEMAP_COMPACT_STROKES=0` keeps strokes 88-byte.
    /// `existing`'s 88-byte buffers are reused where they fit; the compact
    /// bodies are always fresh.
    pub fn install_basemap_mesh(&self, ctx: &GpuContext, existing: Option<TileBuffers>, mesh: &TileMesh) -> TileBuffers {
        if !crate::render::compact::basemap_compact_enabled() {
            return self.install_mesh(ctx, existing, mesh);
        }
        self.install_basemap_mesh_with(ctx, existing, mesh, crate::render::compact::basemap_compact_strokes_enabled())
    }

    /// [`Self::install_basemap_mesh`] with the compact split ON and the
    /// stroke body forced to `strokes`: reads no environment, for a caller
    /// (the story capture path) whose frames must not depend on it.
    pub fn install_basemap_mesh_with(
        &self,
        ctx: &GpuContext,
        existing: Option<TileBuffers>,
        mesh: &TileMesh,
        strokes: bool,
    ) -> TileBuffers {
        let Some(split) = crate::render::compact::split_tile_mesh_with(mesh, strokes) else {
            return self.install_mesh(ctx, existing, mesh);
        };
        self.install_split(ctx, existing, split)
    }

    /// [`Self::install_basemap_mesh`] into fresh buffers, with the dot
    /// mask's index lists for the plan's ranked fills (`ranks`, from
    /// `style::resolve_plan_with_mask`; `spans` align 1:1 with that plan's
    /// ops). The tile's own buffers and draws are exactly
    /// `install_basemap_mesh`'s -- the lists are extra `INDEX` buffers over
    /// the same vertices ([`MaskBuffers`]) and nothing else changes. Empty
    /// `ranks` install no mask at all.
    ///
    /// Always fresh buffers: every basemap install is (`MapView`'s mesh
    /// upload builds each tile's `TileBuffers` from scratch), so there is no
    /// `existing` to reuse.
    pub fn install_basemap_mesh_masked(
        &self,
        ctx: &GpuContext,
        mesh: &TileMesh,
        spans: &[crate::render::mesh::OpSpan],
        ranks: &crate::overlay::dotgrid::mask_class::MaskRanks,
    ) -> TileBuffers {
        if !crate::render::compact::basemap_compact_enabled() {
            return self.install_basemap_mesh_masked_with(ctx, mesh, spans, ranks, None);
        }
        let strokes = crate::render::compact::basemap_compact_strokes_enabled();
        self.install_basemap_mesh_masked_with(ctx, mesh, spans, ranks, Some(strokes))
    }

    /// [`Self::install_basemap_mesh_masked`] reading no environment:
    /// `compact_strokes` `None` installs unsplit (compact off), `Some(s)`
    /// splits with the stroke body forced to `s`.
    pub fn install_basemap_mesh_masked_with(
        &self,
        ctx: &GpuContext,
        mesh: &TileMesh,
        spans: &[crate::render::mesh::OpSpan],
        ranks: &crate::overlay::dotgrid::mask_class::MaskRanks,
        compact_strokes: Option<bool>,
    ) -> TileBuffers {
        // Ranks that do not name ops of THIS mesh's spans (a caller pairing
        // one plan's ranks with another plan's triangulation) would index
        // `spans` out of bounds on the frame thread. The tile installs
        // unmasked instead; debug builds say so loudly.
        let aligned = crate::render::compact::mask_ranks_align(mesh, spans, ranks);
        debug_assert!(aligned, "mask ranks do not align with the mesh's op spans");
        let empty = crate::overlay::dotgrid::mask_class::MaskRanks::default();
        let ranks = if aligned { ranks } else { &empty };
        let split = compact_strokes.and_then(|strokes| crate::render::compact::split_tile_mesh_with_mask(mesh, strokes, spans, ranks));
        let Some((split, lists)) = split else {
            return self.install_mesh_masked(ctx, mesh, spans, ranks);
        };
        let legacy_indices = split.legacy.indices.len();
        let mut buffers = self.install_split(ctx, None, split);
        // `install_mesh` refuses a body too large for one buffer and leaves
        // it empty; lists over vertices that were never uploaded would
        // index past the buffer, so the tile gets no mask.
        if buffers.index_count as usize == legacy_indices {
            buffers.mask = MaskBuffers::new(ctx, &lists);
        }
        buffers
    }

    /// The unsplit half of [`Self::install_basemap_mesh_masked`]: `mesh`
    /// whole on the 88-byte vertex, and the ranked ops' own indices as the
    /// mask's `legacy` lists.
    pub(crate) fn install_mesh_masked(
        &self,
        ctx: &GpuContext,
        mesh: &TileMesh,
        spans: &[crate::render::mesh::OpSpan],
        ranks: &crate::overlay::dotgrid::mask_class::MaskRanks,
    ) -> TileBuffers {
        let mut buffers = self.install_mesh(ctx, None, mesh);
        if buffers.index_count as usize == mesh.indices.len() {
            buffers.mask = MaskBuffers::new(ctx, &crate::render::compact::mask_lists_unsplit(mesh, spans, ranks));
        }
        buffers
    }

    /// Installs an already split tile ([`Self::install_basemap_mesh`]).
    pub(crate) fn install_split(
        &self,
        ctx: &GpuContext,
        existing: Option<TileBuffers>,
        split: crate::render::compact::SplitTileMesh,
    ) -> TileBuffers {
        let mut buffers = self.install_mesh(ctx, existing, &split.legacy);
        let compact = self.create_compact_buffers(ctx, &split.compact);
        let strokes = (!split.strokes.is_empty()).then(|| {
            let init = |label: &'static str, contents: &[u8], usage: wgpu::BufferUsages| {
                crate::render::alloc::TrackedBuffer::init(
                    &ctx.device,
                    &wgpu::util::BufferInitDescriptor { label: Some(label), contents, usage },
                )
            };
            StrokeBuffers {
                vertices: init(
                    "compact-stroke-vertices",
                    bytemuck::cast_slice(&split.strokes),
                    wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                ),
                indices: init("compact-stroke-indices", split.stroke_indices.bytes(), wgpu::BufferUsages::INDEX),
                format: split.stroke_indices.format(),
            }
        });
        let table = self.create_paint_table(ctx, &split.table());
        let table_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("tile-paint-table-bind-group"),
            layout: &self.tile_table_bgl,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&table.view) }],
        });
        buffers.split = Some(Box::new(SplitBody {
            compact,
            strokes,
            table,
            table_group,
            legacy_vertices: split.legacy.vertices.len(),
            compact_vertices: split.compact.vertices.len(),
            stroke_vertices: split.strokes.len(),
            runs: split.runs,
        }));
        buffers
    }

    /// Rewrites ONE RUN of an existing vertex buffer in place -- what a
    /// paint-only update of a few overlay features needs, in place of
    /// pushing the whole buffer.
    ///
    /// The whole-buffer form above is right for a zoom crossing, where
    /// every vertex is stale. It is exactly wrong for a feature recolour:
    /// on this crate's own fleet dataset the overlay's vertex buffer is
    /// 204 MB and one recoloured route is about 1 MB of it, and pushing the
    /// other 203 MB is the single largest cost in an update that otherwise
    /// touches nothing.
    ///
    /// `vertices` is a range of VERTEX indices into `mesh.vertices`; the
    /// byte offset comes from `mesh::vertex_byte_range`, which is tested on
    /// its own because an off-by-one here shifts positions and normals by a
    /// fraction of a vertex -- corrupt geometry, from a paint bug.
    ///
    /// Empty ranges are skipped rather than passed on: an op can tessellate
    /// to no vertices at all, and wgpu rejects a zero-length write.
    /// Writes `vertices` into `buffers` starting at vertex `first`: the
    /// released-mesh counterpart of `write_vertex_range`, for vertices that
    /// were re-derived rather than read from a retained mesh.
    pub fn write_vertices_at(
        &self,
        ctx: &GpuContext,
        buffers: &TileBuffers,
        first: usize,
        vertices: &[crate::render::Vertex],
    ) {
        if vertices.is_empty() {
            return;
        }
        let bytes = crate::render::mesh::vertex_byte_range(&(first..first + vertices.len()));
        ctx.queue.write_buffer(&buffers.vertex_buffer, bytes.start, bytemuck::cast_slice(vertices));
    }

    pub fn write_vertex_range(
        &self,
        ctx: &GpuContext,
        buffers: &TileBuffers,
        mesh: &TileMesh,
        vertices: std::ops::Range<usize>,
    ) {
        if vertices.is_empty() {
            return;
        }
        let bytes = crate::render::mesh::vertex_byte_range(&vertices);
        ctx.queue.write_buffer(
            &buffers.vertex_buffer,
            bytes.start,
            bytemuck::cast_slice(&mesh.vertices[vertices]),
        );
    }

    /// An sRGB-authored colour as a `wgpu::Color` for `render`'s clear
    /// value.
    ///
    /// Passed through unchanged, and that is the whole point: the render
    /// target is a UNORM format (see `map_view.rs`'s surface-format
    /// selection), so what is written is what is stored, and the clear value
    /// is written raw.
    ///
    /// This used to convert to linear, because the target used to be `*Srgb`
    /// and the hardware gamma-ENCODED whatever it was given -- a clear value
    /// is not a fragment and never passes through a shader, so it needed its
    /// own copy of the conversion `shader.wgsl` was doing. That was a live
    /// defect once, in the other direction: under the Mapbox Standard Dark
    /// document (`background-color` resolving to the sRGB triple
    /// `[0.096, 0.094667, 0.094]`, i.e. rgb(24,24,24)) every pixel of land
    /// not covered by another fill read back as rgb(87,87,86) -- exactly
    /// `srgb_encode` of those three channels -- while the roads drawn over
    /// it, which did go through the shader, were right.
    ///
    /// The pairing is what matters, not either half: a transfer function
    /// here without an encoding target, or a target that encodes without one
    /// here, is a background ~2.2 gamma out from everything drawn on it.
    /// Both halves left together.
    ///
    /// Alpha was never converted: it is a linear coverage fraction in both
    /// spaces.
    pub fn clear_color(color: &crate::style::Rgba) -> wgpu::Color {
        wgpu::Color { r: color.r as f64, g: color.g as f64, b: color.b as f64, a: color.a as f64 }
    }

    // Seven arguments, and `timestamp_writes` is the seventh. Bundling them
    // would only rename them: every one is an independent input the caller
    // computes separately, and there is exactly one production call site.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        color_view: &wgpu::TextureView,
        depth_view: &wgpu::TextureView,
        clear_color: wgpu::Color,
        camera_bind_group: &wgpu::BindGroup,
        draws: &[(&TileBuffers, &wgpu::BindGroup)],
        timestamp_writes: Option<wgpu::RenderPassTimestampWrites<'_>>,
    ) {
        // A DELEGATION, not a copy: whatever the tile pass does, it does
        // once. `SpaceDraw::NONE` issues no commands at all, so a flat
        // frame's command stream through here is byte-for-byte the stream
        // it was before the globe had a backdrop -- which is a property of
        // this one line rather than something the pixel tests have to
        // re-establish. (They do anyway:
        // `a_flat_frame_is_pixel_identical_through_the_space_aware_path`.)
        self.render_with_space(
            encoder,
            color_view,
            depth_view,
            clear_color,
            camera_bind_group,
            &SpaceDraw::NONE,
            draws,
            timestamp_writes,
        );
    }

    /// [`Self::render`] plus whatever sits behind the tiles on the globe.
    ///
    /// The pass draws in three ordered groups, and the order IS the
    /// occlusion model — there is no depth test doing this work, because
    /// `shader.wgsl`'s `globe_clipping_z` replaces z with a horizon test
    /// rather than a distance:
    ///
    ///  1. **the stars**, which must end up behind everything;
    ///  2. **the ocean sphere**, opaque, covering the whole silhouette disc
    ///     and so hiding every star inside it (`self.backdrop`, which
    ///     writes no depth — see that field);
    ///  3. **the tiles**, in the caller's order, exactly as before;
    ///  4. **the atmosphere**, over all of it.
    ///
    /// Four is last because the glow is *air in front of everything else*:
    /// the reference draws it after every layer, opaque and translucent both
    /// (`painter.ts:641-644`). Drawing it before the tiles would leave only
    /// the part outside the silhouette — the planet's own fills would paint
    /// over the haze that makes the limb read as atmosphere rather than as
    /// an outline. It is the LAST thing in the TILE pass and not the last
    /// thing in the frame: the symbol pass runs afterwards, so labels are
    /// drawn on top of the glow rather than through it, which is where we
    /// diverge from the reference and deliberately.
    #[allow(clippy::too_many_arguments)]
    pub fn render_with_space(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        color_view: &wgpu::TextureView,
        depth_view: &wgpu::TextureView,
        clear_color: wgpu::Color,
        camera_bind_group: &wgpu::BindGroup,
        space: &SpaceDraw<'_>,
        draws: &[(&TileBuffers, &wgpu::BindGroup)],
        timestamp_writes: Option<wgpu::RenderPassTimestampWrites<'_>>,
    ) {
        let draws: Vec<TileDraw<'_>> = draws.iter().map(|&pair| TileDraw::from(pair)).collect();
        self.render_with_space_and_raster(
            encoder,
            color_view,
            depth_view,
            clear_color,
            camera_bind_group,
            space,
            None,
            &draws,
            timestamp_writes,
        );
    }

    /// Issues `draws` into an open pass, in order, switching pipelines only
    /// when the next draw needs a different one. Returns the draw calls it
    /// actually issued (an empty buffer is skipped). Shared by the tile pass
    /// and [`Self::render_overlay_above_labels`], so an overlay draws the same
    /// way in either slot.
    fn encode_tile_draws(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
        camera_bind_group: &wgpu::BindGroup,
        draws: &[TileDraw<'_>],
    ) -> u64 {
        let mut draw_calls = 0u64;
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, camera_bind_group, &[]);
        let mut bound: *const wgpu::RenderPipeline = &self.pipeline;
        let mut use_pipeline = |pass: &mut wgpu::RenderPass<'_>, pipeline: &wgpu::RenderPipeline| {
            if !std::ptr::eq(bound, pipeline) {
                pass.set_pipeline(pipeline);
                bound = pipeline;
            }
        };
        for draw in draws {
            match *draw {
                TileDraw::Legacy(buffers, model_bind_group) if buffers.split.is_some() => {
                    // A split basemap tile: its runs in op order, each body
                    // under the SAME model group, so the pair composites
                    // exactly as the one 88-byte draw did.
                    let split = buffers.split.as_ref().expect("guarded");
                    for run in &split.runs {
                        if run.indices.is_empty() {
                            continue;
                        }
                        match run.body {
                            crate::render::compact::RunBody::Fill => {
                                use_pipeline(pass, &self.compact_tile);
                                pass.set_bind_group(1, model_bind_group, &[]);
                                pass.set_bind_group(2, &split.table_group, &[]);
                                pass.set_vertex_buffer(0, split.compact.vertices.slice(..));
                                pass.set_index_buffer(split.compact.fill.slice(..), split.compact.fill_format);
                            }
                            crate::render::compact::RunBody::Stroke => {
                                let Some(strokes) = &split.strokes else { continue };
                                use_pipeline(pass, &self.compact_stroke);
                                pass.set_bind_group(1, model_bind_group, &[]);
                                pass.set_bind_group(2, &split.table_group, &[]);
                                pass.set_vertex_buffer(0, strokes.vertices.slice(..));
                                pass.set_index_buffer(strokes.indices.slice(..), strokes.format);
                            }
                            crate::render::compact::RunBody::Legacy => {
                                use_pipeline(pass, &self.pipeline);
                                pass.set_bind_group(1, model_bind_group, &[]);
                                pass.set_vertex_buffer(0, buffers.vertex_buffer.slice(..));
                                pass.set_index_buffer(buffers.index_buffer.slice(..), buffers.index_format);
                            }
                        }
                        pass.draw_indexed(run.indices.clone(), 0, 0..1);
                        draw_calls += 1;
                    }
                }
                TileDraw::Legacy(buffers, model_bind_group) => {
                    if buffers.index_count == 0 {
                        continue; // e.g. an ocean tile with no matching style layer
                    }
                    use_pipeline(pass, &self.pipeline);
                    pass.set_bind_group(1, model_bind_group, &[]);
                    pass.set_vertex_buffer(0, buffers.vertex_buffer.slice(..));
                    pass.set_index_buffer(buffers.index_buffer.slice(..), buffers.index_format);
                    pass.draw_indexed(0..buffers.index_count, 0, 0..1);
                    draw_calls += 1;
                    // The hairline over the fill it outlines, straight after it --
                    // MapLibre draws a fill layer's outline right after its fill
                    // (`draw_fill.ts`). Same vertex buffer, same bind groups; only
                    // the pipeline and the index buffer change.
                    if let Some((outline, count)) = &buffers.outline {
                        use_pipeline(pass, &self.outline);
                        pass.set_index_buffer(outline.slice(..), buffers.index_format);
                        pass.draw_indexed(0..*count, 0, 0..1);
                        draw_calls += 1;
                    }
                }
                TileDraw::CompactFill(buffers, model_bind_group) => {
                    if buffers.fill_count == 0 {
                        continue;
                    }
                    pass.set_bind_group(1, model_bind_group, &[]);
                    match buffers.paint_offset {
                        Some(at) => {
                            use_pipeline(pass, &self.compact_fill_planar);
                            pass.set_vertex_buffer(0, buffers.vertices.slice(..at));
                            pass.set_vertex_buffer(1, buffers.vertices.slice(at..2 * at));
                        }
                        None => {
                            use_pipeline(pass, &self.compact_fill);
                            pass.set_vertex_buffer(0, buffers.vertices.slice(..));
                        }
                    }
                    pass.set_index_buffer(buffers.fill.slice(..), buffers.fill_format);
                    pass.draw_indexed(0..buffers.fill_count, 0, 0..1);
                    draw_calls += 1;
                }
                TileDraw::Circles(buffers, model_bind_group, circle_group) => {
                    if buffers.count == 0 {
                        continue;
                    }
                    use_pipeline(pass, &self.circles);
                    pass.set_bind_group(1, model_bind_group, &[]);
                    pass.set_bind_group(2, circle_group, &[]);
                    pass.set_vertex_buffer(0, buffers.instances.slice(..));
                    pass.draw(0..crate::render::circles::CIRCLE_CORNERS, 0..buffers.count);
                    draw_calls += 1;
                }
                TileDraw::Dots(dots, model_bind_group, flat) => {
                    if dots.vertices() == 0 {
                        continue;
                    }
                    use_pipeline(pass, if flat { &self.dots_flat } else { &self.dots });
                    pass.set_bind_group(1, model_bind_group, &[]);
                    draw_calls += Self::encode_dot_tile(pass, dots);
                }
                TileDraw::CompactOutline(buffers, model_bind_group) => {
                    let Some(outline) = &buffers.outline else { continue };
                    pass.set_bind_group(1, model_bind_group, &[]);
                    match buffers.paint_offset {
                        Some(at) => {
                            use_pipeline(pass, &self.compact_outline_planar);
                            pass.set_vertex_buffer(0, buffers.vertices.slice(..at));
                            pass.set_vertex_buffer(1, buffers.vertices.slice(at..2 * at));
                        }
                        None => {
                            use_pipeline(pass, &self.compact_outline);
                            pass.set_vertex_buffer(0, buffers.vertices.slice(..));
                        }
                    }
                    pass.set_index_buffer(outline.slice(..), buffers.outline_format);
                    pass.draw_indexed(0..buffers.outline_count, 0, 0..1);
                    draw_calls += 1;
                }
                TileDraw::Extrusion { chunk, model, params, first_index, index_count, pass: which } => {
                    if index_count == 0 {
                        continue;
                    }
                    // Groups 2 and 3 are left bound to extrusion groups
                    // afterwards: every other arm that reads group 2 sets it
                    // itself (split runs, circles, dots), and nothing else
                    // has a group 3.
                    use_pipeline(pass, self.extrusion.pipeline(which));
                    if let crate::render::extrusion_gpu::ExtrusionPass::Color { stencil_ref } = which {
                        // Dynamic state: the translucent pipeline reads it
                        // (NotEqual / Replace); every other pipeline in the
                        // pass ignores the stencil, so leaving it set is safe.
                        pass.set_stencil_reference(u32::from(stencil_ref));
                    }
                    pass.set_bind_group(1, model, &[]);
                    pass.set_bind_group(2, &chunk.table_bind_group, &[]);
                    pass.set_bind_group(3, params, &[]);
                    pass.set_vertex_buffer(0, chunk.vertex.slice(..));
                    pass.set_index_buffer(chunk.index.slice(..), chunk.index_format);
                    pass.draw_indexed(first_index..first_index + index_count, 0, 0..1);
                    draw_calls += 1;
                }
                TileDraw::Models(models) => {
                    let calls = models.draw(pass);
                    if calls > 0 {
                        // The models bound their own pipeline and group 0:
                        // put the tile pipeline's back for any draw after.
                        pass.set_pipeline(&self.pipeline);
                        use_pipeline(pass, &self.pipeline);
                        pass.set_bind_group(0, camera_bind_group, &[]);
                    }
                    draw_calls += calls;
                }
                TileDraw::Trips(trips) => {
                    // Counted by the trips themselves (`note_draw_calls`).
                    if trips.draw_in_depth_pass(pass) > 0 {
                        pass.set_pipeline(&self.pipeline);
                        use_pipeline(pass, &self.pipeline);
                        pass.set_bind_group(0, camera_bind_group, &[]);
                    }
                }
            }
        }

        // 3b. The ocean sphere AGAIN, depth only -- so the planet's surface
        //     is in the depth buffer over water as well as over land, and
        //     the glow below has something to be rejected by there. Only
        //     when there is a glow: with no atmosphere this pass has no
        //     reader, and a depth write nothing reads is a draw call spent
        //     on nothing. See `TilePipeline::backdrop_depth` for the
        //     measurement and for why writing depth here cannot bring back
        //     the speckling that `backdrop` exists to prevent.
        draw_calls
    }

    /// The overlay sources that sit ABOVE the basemap's labels
    /// (`render::overlay_order::LabelSlot::AboveLabels`), drawn in their own
    /// pass after the symbol pass: colour loaded, depth cleared (the tile
    /// pass discards its depth, and every overlay fragment is coplanar at
    /// z=0 with `LessEqual`, so a fresh depth buffer draws in list order
    /// exactly as the tile pass would). Nothing is opened when `draws` is
    /// empty, so a frame with no such source is unchanged.
    pub fn render_overlay_above_labels(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        color_view: &wgpu::TextureView,
        depth_view: &wgpu::TextureView,
        camera_bind_group: &wgpu::BindGroup,
        draws: &[TileDraw<'_>],
    ) {
        self.render_overlay_pass("overlay-above-labels-pass", encoder, color_view, depth_view, camera_bind_group, draws);
    }

    /// The overlay draws that sit ABOVE the 3D buildings and BELOW the
    /// labels (a story's `line`/`circle` layers, `above_buildings`), drawn
    /// in their own pass right after the tile pass and before the symbol
    /// pass: colour loaded, depth cleared, exactly as
    /// [`Self::render_overlay_above_labels`]. With the depth the
    /// extrusions wrote gone, a route under a wall or a roof draws whole
    /// instead of in dashes. Ground geometry in the tile pass is never
    /// depth-tested against anything but the extrusions (the tiles draw
    /// before `backdrop_depth`, and write none themselves), so nothing
    /// else changes. Nothing is opened when `draws` is empty.
    pub fn render_overlay_above_buildings(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        color_view: &wgpu::TextureView,
        depth_view: &wgpu::TextureView,
        camera_bind_group: &wgpu::BindGroup,
        draws: &[TileDraw<'_>],
    ) {
        self.render_overlay_pass("overlay-above-buildings-pass", encoder, color_view, depth_view, camera_bind_group, draws);
    }

    /// One overlay pass over the frame so far: colour loaded, depth (and
    /// stencil) cleared and discarded, `draws` in order.
    fn render_overlay_pass(
        &self,
        label: &str,
        encoder: &mut wgpu::CommandEncoder,
        color_view: &wgpu::TextureView,
        depth_view: &wgpu::TextureView,
        camera_bind_group: &wgpu::BindGroup,
        draws: &[TileDraw<'_>],
    ) {
        if draws.is_empty() {
            return;
        }
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some(label),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: color_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: depth_view,
                depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Discard }),
                // Nothing in this pass uses stencil: the extrusion translucent
                // pass that does runs in the tile pass. The attachment's
                // format has a stencil aspect all the same, so it is cleared
                // to 0 and discarded, the ops that cost nothing on a tiler.
                stencil_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(0), store: wgpu::StoreOp::Discard }),
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        let draw_calls = self.encode_tile_draws(&mut pass, camera_bind_group, draws);
        crate::render::alloc::note_draw_calls(draw_calls);
    }


    /// [`Self::render_with_space`], plus a raster basemap drawn between the
    /// ocean sphere and the vector tiles.
    ///
    /// # Why an added method rather than an added parameter
    ///
    /// `render_with_space` has seven call sites, four of them tests that
    /// have nothing to do with raster. Threading `None` through all of them
    /// would be seven edits to prove that nothing changed, and the proof
    /// would be by inspection. Delegating instead makes it structural: the
    /// old entry point passes `None` in exactly one place, so every
    /// pre-existing caller provably issues the command stream it issued
    /// before.
    ///
    /// # Where raster lands in the order, and why there
    ///
    /// Between the backdrop and the tiles -- step 2b of the five the
    /// [`Self::render_with_space`] docs enumerate. Above the ocean sphere
    /// because imagery is the ground and the sphere is what shows where no
    /// ground was drawn; below the vector tiles because every fill, line
    /// and label in the style is meant to sit ON the basemap.
    ///
    /// **Draw order is the whole ordering mechanism here.** Since
    /// `aa20836` nothing in this pass writes depth except
    /// `backdrop_depth`, so there is no depth test to express "raster is
    /// underneath" with, and `RasterPipeline` writes none either -- for a
    /// sharper version of the same reason, since a raster square and the
    /// fills over it are two different triangulations of the same pixels.
    /// See `render::raster_pipeline`'s module docs.
    #[allow(clippy::too_many_arguments)]
    pub fn render_with_space_and_raster(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        color_view: &wgpu::TextureView,
        depth_view: &wgpu::TextureView,
        clear_color: wgpu::Color,
        camera_bind_group: &wgpu::BindGroup,
        space: &SpaceDraw<'_>,
        raster: Option<RasterDraws<'_>>,
        draws: &[TileDraw<'_>],
        timestamp_writes: Option<wgpu::RenderPassTimestampWrites<'_>>,
    ) {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("tile-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: color_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(clear_color), store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: depth_view,
                // `Discard`, because NOTHING reads this buffer once the pass
                // ends, and it can be shown rather than believed:
                // `create_depth_view` gives the texture `RENDER_ATTACHMENT`
                // and nothing else, so wgpu would reject a bind group or a
                // copy that tried to read it; the symbol pass runs with
                // `depth_stencil_attachment: None`; and the next frame opens
                // with the `Clear` on the line above, so not even a later
                // frame depends on what is in here.
                //
                // Storing it was a full-screen depth-stencil writeback every
                // frame -- 23.8 MB of 32-bit depth alone at a 3024x1964
                // drawable (Metal backs Depth24PlusStencil8 with
                // Depth32Float_Stencil8, so the stencil adds ~5.9 MB), ~1.4
                // GB/s at 60 Hz -- on a tile-based GPU where the buffer
                // would otherwise never leave tile memory.
                depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Discard }),
                // Cleared to 0 each pass and never stored: only the extrusion
                // translucent pass reads it, within this pass (spec §4.3).
                stencil_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(0), store: wgpu::StoreOp::Discard }),
            }),
            timestamp_writes,
            occlusion_query_set: None,
            multiview_mask: None,
        });

        let mut draw_calls = 0u64;

        // 0. The space look's star-map sky, under the point stars.
        if let Some(look) = space.look {
            draw_calls += look.draw_sky(&mut pass);
        }

        // 1. The stars. Own pipeline, own bind group, own vertex format --
        //    so this is set up and torn down around the tile pipeline
        //    rather than interleaved with it, and a frame without stars
        //    issues nothing here.
        //    A ground observer's star field (`StarPipeline::over_sky`) waits
        //    for 1c instead.
        if let Some(stars) = space.stars.filter(|s| !s.over_sky()) {
            stars.draw(&mut pass);
            draw_calls += 1;
        }

        // 1b. The sky, over the stars by `1 - transition` of coverage and
        //     under everything else. Writes no depth.
        if let Some(sky) = space.sky {
            sky.draw(&mut pass);
            draw_calls += 1;
        }

        // 1c. The stars over a pitched ground view's sky gradient
        //     (`MapView::set_sky_stars`): above the horizon only, under
        //     every tile.
        if let Some(stars) = space.stars.filter(|s| s.over_sky()) {
            stars.draw(&mut pass);
            draw_calls += 1;
        }

        // 2. The ocean sphere, on the depth-write-disabled twin.
        if let Some((buffers, model_bind_group)) = space.backdrop {
            if buffers.index_count > 0 {
                pass.set_pipeline(&self.backdrop);
                pass.set_bind_group(0, camera_bind_group, &[]);
                pass.set_bind_group(1, model_bind_group, &[]);
                pass.set_vertex_buffer(0, buffers.vertex_buffer.slice(..));
                pass.set_index_buffer(buffers.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..buffers.index_count, 0, 0..1);
                draw_calls += 1;
            }
        }

        // 2b. The raster basemap, over the ocean sphere and under every
        //     vector layer. Its own pipeline and its own third bind group
        //     (texture + sampler + `raster-opacity`), so like the stars it
        //     is set up and torn down around the tile pipeline rather than
        //     interleaved with it. A frame with no raster source issues
        //     nothing here and the command stream is unchanged.
        //
        //     Unless the caller asked for it after the first
        //     `after_tile_draws` tile draws (a data raster over the
        //     basemap): those go first, then the raster, then the rest.
        //     0 -- every imagery basemap -- issues exactly the stream above.
        let split = raster.as_ref().map_or(0, |r| r.after_tile_draws.min(draws.len()));
        if split > 0 {
            draw_calls += self.encode_tile_draws(&mut pass, camera_bind_group, &draws[..split]);
        }
        if let Some(raster) = raster {
            draw_calls += raster.pipeline.draw(&mut pass, camera_bind_group, raster.draws);
        }

        // 3. The tiles, in the caller's order. The pipeline is switched
        //    only when the draw kind changes; the camera group is bound
        //    once, since every tile pipeline shares `camera_bgl`.
        draw_calls += self.encode_tile_draws(&mut pass, camera_bind_group, &draws[split..]);

        if let (Some((buffers, model_bind_group)), Some(_)) = (space.backdrop, space.atmosphere) {
            if buffers.index_count > 0 {
                pass.set_pipeline(&self.backdrop_depth);
                pass.set_bind_group(0, camera_bind_group, &[]);
                pass.set_bind_group(1, model_bind_group, &[]);
                pass.set_vertex_buffer(0, buffers.vertex_buffer.slice(..));
                pass.set_index_buffer(buffers.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..buffers.index_count, 0, 0..1);
                draw_calls += 1;
            }
        }

        // 3b. The space look's clouds, sun shading and rim: over the
        //     planet the tiles drew, under the limb glow.
        if let Some(look) = space.look {
            draw_calls += look.draw_globe(&mut pass);
        }

        // 4. The atmosphere, over everything the tile pass drew. Own
        //    pipeline, own bind group, no vertex buffer at all -- three
        //    generated vertices.
        if let Some(atmosphere) = space.atmosphere {
            atmosphere.draw(&mut pass);
            draw_calls += 1;
        }

        // Counted here rather than as `draws.len()` at the call site: the
        // `index_count == 0` skip above means those are not the same number,
        // and the one this loop issues is the one the driver sees.
        crate::render::alloc::note_draw_calls(draw_calls);
    }
}

/// Wall-clock GPU time for the two render passes, read back off the device
/// through a `TIMESTAMP_QUERY` query set.
///
/// # Why this exists
///
/// Every other timing in this crate is CPU-side: [`alloc::FrameTimer`] wraps
/// `MapView::paint`, which ends at `queue.submit` and therefore measures how
/// long it took to BUILD a frame, not how long the GPU took to draw it. Those
/// two numbers diverge in exactly the situations worth catching -- a
/// full-screen depth writeback, an overdrawn tile stack, a pitched camera
/// tripling the visible cover -- and until this type there was no number for
/// the second one at all.
///
/// # Degrading, not failing
///
/// [`Self::new`] returns `None` on any device that did not get
/// `TIMESTAMP_QUERY` (`GpuContext::new` asks only when the adapter offers it;
/// `new_headless`, which every test uses, deliberately never does). Every
/// caller then passes `None` for `timestamp_writes` and nothing else changes.
/// This engine runs on macOS, iOS and wasm and must start on all three.
///
/// # The resolve belongs to the NEXT frame's command buffer. Measured.
///
/// The obvious arrangement -- write the timestamps in the passes, then
/// `resolve_query_set` further down the SAME encoder -- reads the
/// end-of-pass slot back as a flat zero on Metal. Reproduced directly: one
/// tile pass writing slots 0 and 1, resolved on its own command buffer,
/// gives `[343977018418333, 0, 0, 0]`; the identical pass resolved from the
/// FOLLOWING command buffer gives `[343977018418333, 343977018478000, ...]`,
/// a 59.7 us pass. Metal samples the end-of-fragment counter as the encoder
/// retires, and a `resolveCounters` sitting in the same command buffer is
/// simply too early to see it.
///
/// So [`Self::begin_frame`] resolves the PREVIOUS frame's slots at the top
/// of this frame's encoder, before any pass has overwritten them. Queue
/// ordering is what makes that safe: the earlier command buffer has retired
/// before this one runs, so the counters are settled by the time the resolve
/// executes. A duration therefore arrives two frames after the frame it
/// describes, which is exactly what a diagnostic gauge wants and is why
/// nothing in the render path ever waits on one.
///
/// `reading` keeps that honest under back-pressure. While a readback is
/// outstanding the mappable buffer is owned by wgpu, so a frame that finds
/// the flag set skips resolving and lets the next frame overwrite the query
/// set instead. A slow GPU then samples fewer frames rather than stalling
/// the fast one, and double-mapping is impossible by construction rather
/// than by hoping the callback ran in time.
/// `GPUI_MAP_GPU_SPIKE_MS=<ms>`: one stderr line per sampled frame whose
/// tile pass took at least that long, with the symbol pass of the SAME
/// frame and the mesh installs (basemap tiles, overlay chunks) since the
/// previous sample. The discriminator for a worst frame: a spike the
/// symbol pass shares is the GPU as a whole (clocks, another process); a
/// tile-pass-only spike beside installs is an upload; neither is the
/// tile pass's own work.
fn note_gpu_spike(tile_nanos: u64, symbol_nanos: Option<u64>) {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    static THRESHOLD: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    static LAST_INSTALLS: AtomicU64 = AtomicU64::new(0);
    static SAMPLES: AtomicU64 = AtomicU64::new(0);
    let Some(threshold) = *THRESHOLD.get_or_init(|| {
        std::env::var("GPUI_MAP_GPU_SPIKE_MS").ok()?.parse::<f64>().ok().map(|ms| (ms * 1e6) as u64)
    }) else {
        return;
    };
    let sample = SAMPLES.fetch_add(1, Relaxed);
    let installs = crate::render::alloc::allocation_stats().mesh_installs_total;
    let since = installs.saturating_sub(LAST_INSTALLS.swap(installs, Relaxed));
    if tile_nanos >= threshold {
        eprintln!(
            "[gpu spike] sample {sample}: tile {:.2} ms, symbol {} | mesh installs since last sample {since}",
            tile_nanos as f64 / 1e6,
            symbol_nanos.map_or("-".to_string(), |n| format!("{:.2} ms", n as f64 / 1e6)),
        );
    }
}

pub struct PassTimer {
    query_set: wgpu::QuerySet,
    /// `QUERY_RESOLVE | COPY_SRC`. `resolve_query_set` cannot write into a
    /// mappable buffer, so the ticks land here first and are copied across.
    resolve: crate::render::alloc::TrackedBuffer,
    /// `COPY_DST | MAP_READ`. Behind an `Arc` only because the `map_async`
    /// callback is `'static` and has to unmap it; the tracker still sees
    /// exactly one buffer, released when the last handle goes.
    readback: std::sync::Arc<crate::render::alloc::TrackedBuffer>,
    /// Nanoseconds per timestamp tick, per `Queue::get_timestamp_period`.
    period_ns: f32,
    /// `Some(symbol_pass_ran)` while the query set holds a submitted frame's
    /// timestamps that no encoder has resolved yet. The flag travels with
    /// them because the symbol pass is conditional, and whether slots 2-3
    /// mean anything is a property of the frame that wrote them, not of the
    /// frame that resolves them.
    written: std::cell::Cell<Option<bool>>,
    reading: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl PassTimer {
    /// Query slots: tile-pass begin/end, then symbol-pass begin/end.
    const QUERIES: u32 = 4;
    /// One `u64` of ticks per query. Both buffers are rounded up to
    /// `QUERY_RESOLVE_BUFFER_ALIGNMENT` so the resolve destination is legal
    /// without any offset arithmetic.
    const BYTES: u64 = wgpu::QUERY_RESOLVE_BUFFER_ALIGNMENT;

    /// `None` unless the device actually got `TIMESTAMP_QUERY`.
    // Flagged on wasm32 only, where wgpu's buffers are `!Send`; natively
    // the readback `Arc` is `Send + Sync`.
    #[allow(clippy::arc_with_non_send_sync)]
    pub fn new(ctx: &GpuContext) -> Option<Self> {
        if !ctx.device.features().contains(wgpu::Features::TIMESTAMP_QUERY) {
            return None;
        }
        Some(Self {
            query_set: ctx.device.create_query_set(&wgpu::QuerySetDescriptor {
                label: Some("pass-timer"),
                ty: wgpu::QueryType::Timestamp,
                count: Self::QUERIES,
            }),
            resolve: crate::render::alloc::TrackedBuffer::new(&ctx.device, &wgpu::BufferDescriptor {
                label: Some("pass-timer-resolve"),
                size: Self::BYTES,
                usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            }),
            readback: std::sync::Arc::new(crate::render::alloc::TrackedBuffer::new(&ctx.device, &wgpu::BufferDescriptor {
                label: Some("pass-timer-readback"),
                size: Self::BYTES,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            })),
            period_ns: ctx.queue.get_timestamp_period(),
            written: std::cell::Cell::new(None),
            reading: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }

    /// `timestamp_writes` for the tile pass.
    pub fn tile_writes(&self) -> wgpu::RenderPassTimestampWrites<'_> {
        wgpu::RenderPassTimestampWrites {
            query_set: &self.query_set,
            beginning_of_pass_write_index: Some(0),
            end_of_pass_write_index: Some(1),
        }
    }

    /// `timestamp_writes` for the symbol overlay pass. That pass is opened
    /// only when there is something to draw into it, so a frame can write
    /// slots 0-1 and not 2-3 -- see [`Self::read_back`]'s `symbol_pass_ran`.
    pub fn symbol_writes(&self) -> wgpu::RenderPassTimestampWrites<'_> {
        wgpu::RenderPassTimestampWrites {
            query_set: &self.query_set,
            beginning_of_pass_write_index: Some(2),
            end_of_pass_write_index: Some(3),
        }
    }

    /// Queues the resolve + copy for the PREVIOUS frame's timestamps. Call
    /// FIRST on this frame's encoder, before any pass has begun -- the
    /// resolve has to run before this frame overwrites the slots, and it has
    /// to be on a later command buffer than the frame it reads (see the type
    /// doc). Returns that frame's `symbol_pass_ran`, to be handed straight to
    /// [`Self::read_back`] after the submit; `None` when there was nothing
    /// to resolve or a readback is still outstanding.
    #[must_use]
    pub fn begin_frame(&self, encoder: &mut wgpu::CommandEncoder) -> Option<bool> {
        if self.reading.load(std::sync::atomic::Ordering::Acquire) {
            return None;
        }
        // `take`, so a frame that is skipped here does not leave a stale
        // claim behind: `end_frame` is about to set the flag for the frame
        // whose timestamps will actually be in those slots.
        let symbol_pass_ran = self.written.take()?;
        encoder.resolve_query_set(&self.query_set, 0..Self::QUERIES, &self.resolve, 0);
        encoder.copy_buffer_to_buffer(&self.resolve, 0, &self.readback, 0, Self::BYTES);
        Some(symbol_pass_ran)
    }

    /// Records that this frame's passes have been encoded and what they
    /// wrote. Call after the last pass, before the submit.
    pub fn end_frame(&self, symbol_pass_ran: bool) {
        self.written.set(Some(symbol_pass_ran));
    }

    /// Maps the readback buffer and, when the GPU gets there, publishes the
    /// two durations to [`crate::render::alloc`]. Call AFTER the submit that
    /// carried [`Self::begin_frame`]'s copy, with that call's return value.
    pub fn read_back(&self, resolved: Option<bool>) {
        let Some(symbol_pass_ran) = resolved else {
            return;
        };
        self.reading.store(true, std::sync::atomic::Ordering::Release);
        let readback = std::sync::Arc::clone(&self.readback);
        let reading = std::sync::Arc::clone(&self.reading);
        let period_ns = self.period_ns;
        self.readback.map_async(wgpu::MapMode::Read, .., move |result| {
            if result.is_ok() {
                let view = readback.slice(..).get_mapped_range().expect("the range just mapped for reading");
                let ticks: [u64; Self::QUERIES as usize] = std::array::from_fn(|i| {
                    let at = i * 8;
                    u64::from_le_bytes(view[at..at + 8].try_into().expect("8 bytes per timestamp query"))
                });
                drop(view);
                readback.unmap();
                // `saturating_sub` because a driver is not obliged to make
                // these monotonic across a pass boundary, and a wrapped
                // subtraction would publish a ~584-year maximum that no
                // reader could ever reset.
                let nanos = |from: u64, to: u64| (to.saturating_sub(from) as f64 * f64::from(period_ns)) as u64;
                let (tile, symbol) = (nanos(ticks[0], ticks[1]), symbol_pass_ran.then(|| nanos(ticks[2], ticks[3])));
                note_gpu_spike(tile, symbol);
                crate::render::alloc::note_gpu_pass_nanos(tile, symbol);
            }
            reading.store(false, std::sync::atomic::Ordering::Release);
        });
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::render::mesh::Vertex;
    use crate::style::Rgba;
    use std::sync::Arc;

    /// A sun for the globe tests that are NOT about the sun: a unit
    /// direction, and a night side exactly as bright as the day side, which
    /// is what `night_brightness == 1.0` means.
    ///
    /// Every globe pixel test that predates ground shading grades an exact
    /// byte triple — `(255, 0, 0)` for a tile, `(79, 116, 153)` for the
    /// ocean, `(235, 235, 235)` for Spica — and a test about a silhouette,
    /// an anchor, a stroke width or a star position has no business
    /// knowing a sun exists. The two tests that ARE about it name their own.
    const UNLIT_SUN: glam::DVec3 = glam::DVec3::Z;
    const UNLIT_NIGHT: f32 = 1.0;

    fn triangle_mesh(color: [f32; 4]) -> crate::render::TileMesh {
        crate::render::TileMesh {
            vertices: vec![
                Vertex { position: [-500.0, -500.0], normal: [0.0, 0.0], color, is_stroke: 0.0, half_width_px: 0.0, color_to: color, half_width_px_to: 0.0, gap_half_px: 0.0, gap_half_px_to: 0.0, line_distance: 0.0, dash_runs: [0.0; 4], line_progress: -1.0 },
                Vertex { position: [500.0, -500.0], normal: [0.0, 0.0], color, is_stroke: 0.0, half_width_px: 0.0, color_to: color, half_width_px_to: 0.0, gap_half_px: 0.0, gap_half_px_to: 0.0, line_distance: 0.0, dash_runs: [0.0; 4], line_progress: -1.0 },
                Vertex { position: [0.0, 500.0], normal: [0.0, 0.0], color, is_stroke: 0.0, half_width_px: 0.0, color_to: color, half_width_px_to: 0.0, gap_half_px: 0.0, gap_half_px_to: 0.0, line_distance: 0.0, dash_runs: [0.0; 4], line_progress: -1.0 },
            ],
            indices: vec![0, 1, 2],
        }
    }

    /// An adapter shaped like iOS Metal: the WebGPU baseline in every
    /// dimension except the one that made the engine unable to get a device
    /// there at all.
    fn ios_shaped_limits() -> wgpu::Limits {
        wgpu::Limits {
            // The refusal, verbatim: "Limit
            // 'max_inter_stage_shader_variables' value 16 is better than
            // allowed 15".
            max_inter_stage_shader_variables: 15,
            // And the other direction, in the same adapter: far more than the
            // baseline's 256 MiB.
            max_buffer_size: 4 * 1024 * 1024 * 1024,
            ..wgpu::Limits::default()
        }
    }

    /// The regression this whole change exists for, and it needs no GPU: the
    /// baseline request is UNSATISFIABLE on an iOS-shaped adapter, and the
    /// adapter-derived one is satisfiable by construction.
    #[test]
    fn an_ios_shaped_adapter_can_be_asked_for_a_device_at_all() {
        let ios = ios_shaped_limits();
        assert!(
            !wgpu::Limits::default().check_limits(&ios),
            "the premise: `DeviceDescriptor::default()` asks for more than iOS Metal allows, \
             which is why `request_device` was rejected outright there"
        );
        assert!(
            required_limits(&ios).check_limits(&ios),
            "asking for what the adapter offers is satisfiable BY CONSTRUCTION -- there is no \
             adapter this request can be refused by"
        );
    }

    /// Follows the adapter in BOTH directions, which is the point: down where
    /// the baseline was too greedy, up where it was too timid. Pinned as a
    /// policy, not as this machine's numbers.
    #[test]
    fn required_limits_track_the_adapter_rather_than_the_webgpu_baseline() {
        let ios = ios_shaped_limits();
        let asked = required_limits(&ios);

        assert_eq!(
            asked.max_inter_stage_shader_variables, 15,
            "DOWN: the baseline's 16 is what iOS refuses, and `shader.wgsl` only fills 10"
        );
        assert_eq!(
            asked.max_buffer_size,
            4 * 1024 * 1024 * 1024,
            "UP: the baseline's 256 MiB was a self-imposed ceiling at 1/16th of the hardware"
        );
        assert_ne!(
            asked,
            wgpu::Limits::default(),
            "if this ever equals the baseline again, both halves of the defect are back"
        );
        assert_eq!(asked, ios, "the adapter's set, verbatim -- no better and no worse");
    }

    /// Following the adapter downwards is only safe because a shortfall in a
    /// dimension the shaders DO use is refused by name. A device granting 4
    /// inter-stage variables would render wrongly, which is worse than not
    /// starting.
    #[test]
    fn a_device_short_of_what_the_shaders_use_is_named_not_accepted() {
        let crippled = wgpu::Limits {
            max_inter_stage_shader_variables: 4,
            max_vertex_attributes: 4,
            ..wgpu::Limits::default()
        };
        let shortfalls = unmet_shader_needs(&crippled);
        assert_eq!(shortfalls.len(), 2, "every shortfall is reported, not just the first");
        // Built from `declared_needs` rather than spelled out, so this
        // assertion tests the REPORTING and not a second copy of the figure --
        // a literal here would have to be edited in lockstep with the one in
        // the table, which is the drift
        // `the_declared_inter_stage_need_matches_the_shader_it_is_declared_for`
        // exists to catch.
        let inter_stage = declared_needs()
            .into_iter()
            .find(|(name, _)| *name == "max_inter_stage_shader_variables")
            .expect("the limit is declared")
            .1;
        assert!(
            shortfalls
                .contains(&format!("max_inter_stage_shader_variables: needs {inter_stage}, offers 4")),
            "the error must name the limit and both numbers: {shortfalls:?}"
        );
        // ...and the same for the attribute count, DERIVED for the same
        // reason. This one was a literal `9` and it drifted exactly as that
        // comment predicted: the table's figure was corrected from 9 to the
        // 11 `VertexInput` actually declares, and this assertion was the
        // only thing that failed.
        let attributes = declared_needs()
            .into_iter()
            .find(|(name, _)| *name == "max_vertex_attributes")
            .expect("the limit is declared")
            .1;
        assert!(
            shortfalls.contains(&format!("max_vertex_attributes: needs {attributes}, offers 4")),
            "{shortfalls:?}"
        );
    }

    /// The other half of the check's correctness, and the easier half to get
    /// wrong: a minimum invented LARGER than the truth refuses hardware that
    /// would have worked. Every platform this crate targets must pass.
    #[test]
    fn every_platform_this_crate_targets_clears_the_shader_needs() {
        for (what, limits) in [
            ("the WebGPU baseline", wgpu::Limits::default()),
            ("iOS Metal", ios_shaped_limits()),
            // The wasm shell's fallback when the browser gives it WebGL2
            // rather than WebGPU -- 15 inter-stage variables, zero for every
            // compute and storage limit, a 255-byte vertex stride.
            ("WebGL2", wgpu::Limits::downlevel_webgl2_defaults()),
            ("the downlevel floor", wgpu::Limits::downlevel_defaults()),
        ] {
            assert!(
                unmet_shader_needs(&limits).is_empty(),
                "{what} must be able to run this crate's shaders, but is reported short on {:?}",
                unmet_shader_needs(&limits)
            );
        }
    }

    /// The headless proof of the iOS fix, on a REAL device rather than a
    /// limit set: this machine's adapter, capped at iOS Metal's 15
    /// inter-stage variables, asked for through `required_limits` and
    /// screened by `unmet_shader_needs` exactly as [`GpuContext::new`] does
    /// -- and every render pipeline the engine builds (the tile pipeline and
    /// everything `TilePipeline::new` builds with it, the altitude, donut and
    /// sky pipelines) compiles on it with no validation error. A shader that
    /// grew past 15 locations fails here, not on a phone.
    #[test]
    fn every_pipeline_builds_on_a_device_capped_at_ios_metals_15_inter_stage_variables() {
        pollster::block_on(async {
            let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
            let adapter = instance
                .request_adapter(&wgpu::RequestAdapterOptions { power_preference: wgpu::PowerPreference::LowPower, ..Default::default() })
                .await
                .expect("an adapter");
            let offered = wgpu::Limits { max_inter_stage_shader_variables: 15, ..adapter.limits() };
            let asked = required_limits(&offered);
            assert!(unmet_shader_needs(&asked).is_empty(), "{:?}", unmet_shader_needs(&asked));
            let (device, queue) = adapter
                .request_device(&wgpu::DeviceDescriptor { required_limits: asked, ..Default::default() })
                .await
                .expect("a device at iOS Metal's inter-stage limit");
            assert_eq!(device.limits().max_inter_stage_shader_variables, 15, "the cap is what the device enforces");
            let max_buffer_size = device.limits().max_buffer_size;
            let ctx = GpuContext { device, queue, max_buffer_size };
            let scope = ctx.device.push_error_scope(wgpu::ErrorFilter::Validation);
            let format = wgpu::TextureFormat::Rgba8Unorm;
            let _tiles = TilePipeline::new(&ctx, format);
            let _altitude = crate::render::altitude_pipeline::AltitudePipeline::new(&ctx, format, Some(wgpu::TextureFormat::Depth24Plus));
            let _donut = crate::render::donut::DonutPipeline::new(&ctx, format);
            let _sky = crate::render::sky::SkyPipeline::new(&ctx, format, wgpu::TextureFormat::Depth24PlusStencil8);
            let _model = crate::render::model::ModelPipeline::new(&ctx, format, Some(wgpu::TextureFormat::Depth24PlusStencil8));
            let error = scope.pop().await;
            assert!(error.is_none(), "validation error at 15 inter-stage variables: {error:?}");
        });
    }

    /// The declared need for `max_uniform_buffer_binding_size` must be
    /// COMPUTED from the type, not written as a literal.
    ///
    /// Two separate failures are guarded here, and the second is the one that
    /// actually bites.
    ///
    /// **The alignment.** WGSL requires a host-shareable struct containing a
    /// `mat4x4<f32>` to be a multiple of 16 bytes. Getting it wrong does not
    /// produce a uniform-shaped symptom: it shifts every field after the pad,
    /// so a matrix column arrives where a scalar was expected and the
    /// GEOMETRY is corrupt -- which looks nothing like the uniform bug that
    /// caused it. Same reasoning `vertex_byte_range` carries.
    ///
    /// **The literal.** A literal is how the declared need came to say 80
    /// after the struct grew: the number was correct when written and silently
    /// stopped being correct. A declared minimum smaller than the truth ships
    /// a crash on hardware that reports exactly the declared figure, and one
    /// larger refuses hardware that would have worked. Deriving it from
    /// `size_of` removes the only way it can drift.
    #[test]
    fn the_model_uniform_is_16_byte_aligned_and_its_declared_need_matches_reality() {
        assert_eq!(
            std::mem::size_of::<ModelUniform>() % 16,
            0,
            "ModelUniform is {} bytes, which is not a multiple of 16",
            std::mem::size_of::<ModelUniform>()
        );
        let needs = declared_needs();
        let declared = needs
            .iter()
            .find(|(name, _)| *name == "max_uniform_buffer_binding_size")
            .expect("the limit is declared");
        assert_eq!(
            declared.1,
            std::mem::size_of::<ModelUniform>() as u64,
            "the declared need must be computed from the type, not written as a literal \
             -- a literal is how it came to say 80 after the struct grew"
        );
    }

    /// `line_fx` is appended at offset 224, 16-byte aligned, after
    /// `clip_bounds` -- so nothing above it moved -- and the uniform is 240
    /// bytes. `LINE_FX_NONE` is the default every constructor writes.
    #[test]
    fn line_fx_is_appended_at_224_and_defaults_to_no_trim_full_opacity() {
        assert_eq!(std::mem::offset_of!(ModelUniform, clip_bounds), 208);
        assert_eq!(std::mem::offset_of!(ModelUniform, line_fx), 224);
        assert_eq!(LINE_FX_OFFSET, 224);
        assert_eq!(std::mem::size_of::<ModelUniform>(), 240);
        assert_eq!(LINE_FX_NONE, [0.0, 1.0, 1.0, 0.0]);
        let uniform = ModelUniform::new([[0.0; 4]; 4], 0.0, 1.0, GlobeParams::FLAT, TileClip::WHOLE, LINE_FX_NONE);
        assert_eq!(uniform.line_fx, LINE_FX_NONE);
        // The WGSL `Model` ends with the same field.
        let src = include_str!("shader.wgsl");
        assert!(src.contains("    clip_bounds: vec4<f32>,\n    /// `(keep_start"), "line_fx follows clip_bounds in shader.wgsl");
        assert!(src.contains("    line_fx: vec4<f32>,\n}"), "line_fx is the last field of Model");
    }

    /// F25: every entry point whose output is `VertexOutput`, and `fs_main`,
    /// through naga's GLSL ES 3.00 back end -- the WebGL2 path, where an
    /// entry point nobody listed is the one that breaks in the browser.
    #[test]
    fn the_tile_shader_is_valid_glsl_es_300() {
        crate::render::dots::assert_valid_glsl_es_300(
            include_str!("shader.wgsl"),
            &[
                ("vs_main", naga::ShaderStage::Vertex),
                ("vs_outline", naga::ShaderStage::Vertex),
                ("vs_compact", naga::ShaderStage::Vertex),
                ("vs_compact_tile", naga::ShaderStage::Vertex),
                ("vs_compact_stroke", naga::ShaderStage::Vertex),
                ("vs_circle", naga::ShaderStage::Vertex),
                ("vs_dot", naga::ShaderStage::Vertex),
                ("fs_main", naga::ShaderStage::Fragment),
                ("fs_dot", naga::ShaderStage::Fragment),
            ],
        );
    }

    /// F25: `vs_main` passes `line_progress` through (`vertex_main`); every
    /// other `VertexOutput` producer has no line metrics and writes `-1.0`,
    /// so its fragments are never trimmed. Text scan, in the idiom of
    /// `the_declared_inter_stage_need_matches_the_shader_it_is_declared_for`.
    #[test]
    fn only_vertex_main_passes_line_progress_through_and_every_other_entry_writes_minus_1() {
        let src = include_str!("shader.wgsl");
        let body = |name: &str| {
            let start = src.find(&format!("fn {name}(")).unwrap_or_else(|| panic!("{name} exists"));
            let end = start + src[start..].find("\n}\n").expect("closed");
            &src[start..end]
        };
        assert!(body("vertex_main").contains("out.line_progress = input.line_progress;"));
        assert!(body("vertex_main").contains("out.color.a = out.color.a * model.line_fx.z;"));
        assert!(body("compact_main").contains("out.line_progress = -1.0;"));
        // F1: the fade reaches the compact fills too (a tiled overlay's).
        assert!(body("compact_main").contains("out.color.a = out.color.a * model.line_fx.z;"));
        // F2/F5: an empty range cuts everything; an end is cut only inside the line.
        let fs_all = body("fs_main");
        assert!(fs_all.contains("model.line_fx.y <= model.line_fx.x ||"));
        assert!(fs_all.contains("(model.line_fx.x > 0.0 && input.line_progress < model.line_fx.x)"));
        assert!(fs_all.contains("(model.line_fx.y < 1.0 && input.line_progress > model.line_fx.y)"));
        for ep in ["vs_compact_stroke", "vs_circle"] {
            assert!(body(ep).contains("v.line_progress = -1.0;"), "{ep}");
        }
        // `vs_dot` writes `DotGlobeOutput`, which has no line progress for
        // `fs_dot` to trim on; it still takes the draw's fade.
        assert!(body("vs_dot").contains("color.a = color.a * model.line_fx.z;"));
        // vs_outline re-dresses a FILL vertex, whose progress is already -1.0.
        assert!(body("vs_outline").contains("return vertex_main(v);"));
        assert!(body("vs_main").contains("return vertex_main(input);"));
        // vs_compact and vs_compact_tile end in compact_main.
        for ep in ["vs_compact", "vs_compact_tile"] {
            assert!(body(ep).contains("return compact_main("), "{ep}");
        }
        // fs_main's first statement is the trim.
        let fs = body("fs_main");
        let first = fs.find("if (").expect("an if");
        assert!(fs[first..].starts_with("if (model.line_fx.w > 0.5 && input.line_progress >= 0.0"));
        assert!(!fs[..first].contains("var ") && !fs[..first].contains("let "), "the trim is fs_main's first statement");
    }

    /// The altitude layer's point and line stages (spec §2.9): two
    /// inter-stage vectors each, six and three instance attributes, all
    /// within the declared needs,
    /// so it builds on iOS Metal and WebGL2 wherever the tile shader does.
    #[test]
    fn the_altitude_shader_fits_inside_the_declared_needs() {
        let src = include_str!("altitude_shader.wgsl");
        let points = vertex_output_locations_of(src, "PointOut");
        let lines = vertex_output_locations_of(src, "LineOut");
        let trails = vertex_output_locations_of(src, "TrailOut");
        assert_eq!(points, vec![0, 1, 2], "points: color, uv_alpha_size, the Glow halo's gain (spec §2.9)");
        assert_eq!(lines, vec![0, 1], "lines: color, sphere_pos_edge (the width rides in the uniform)");
        assert_eq!(trails, vec![0, 1, 2], "trails: color, sphere_pos_edge, along");
        let declared = declared_needs().iter().find(|(n, _)| *n == "max_inter_stage_shader_variables").unwrap().1;
        assert!(points.len() as u64 <= declared && lines.len() as u64 <= declared && trails.len() as u64 <= declared);
        let declared_attrs = declared_needs().iter().find(|(n, _)| *n == "max_vertex_attributes").unwrap().1;
        for attrs in [crate::render::altitude_pipeline::POINT_ATTRS.len(), crate::render::altitude_pipeline::LINE_ATTRS.len()] {
            assert!(attrs as u64 <= declared_attrs, "{attrs} instance attributes");
        }
    }

    #[test]
    fn the_depth_attachment_carries_a_stencil_aspect() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            // Opened before the pipelines are built, so every depth-tested
            // pipeline's `depth_stencil` format is validated too. wgpu's
            // error handler logs and continues; the scope is what turns a
            // validation error into a failure here.
            let scope = ctx.device.push_error_scope(wgpu::ErrorFilter::Validation);
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            // Depth24PlusStencil8 is core in WebGPU and WebGL2 (no feature
            // flag); on Metal wgpu backs it with Depth32Float_Stencil8 on
            // Apple silicon and iOS, so those keep float depth. Spec §4.3, R7.
            assert_eq!(pipeline.depth_format(), wgpu::TextureFormat::Depth24PlusStencil8);

            // The format alone proves nothing about the device: attach a
            // texture of it the way both passes do -- depth AND stencil
            // cleared, both discarded -- bind the real depth-tested
            // pipeline in that pass, and require the device to accept both.
            // `set_pipeline` is where wgpu checks the pipeline's
            // `depth_stencil.format` against the attachment, so a pipeline
            // built with any other format fails here.
            let color = ctx.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("stencil-aspect-probe-color"),
                size: wgpu::Extent3d { width: 4, height: 4, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            });
            let color_view = color.create_view(&wgpu::TextureViewDescriptor::default());
            let depth = ctx.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("stencil-aspect-probe"),
                size: wgpu::Extent3d { width: 4, height: 4, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: pipeline.depth_format(),
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            });
            let view = depth.create_view(&wgpu::TextureViewDescriptor::default());
            let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("stencil-aspect-probe"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &color_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Discard },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &view,
                    depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Discard }),
                    stencil_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(0), store: wgpu::StoreOp::Discard }),
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&pipeline.pipeline);
            drop(pass);
            ctx.queue.submit([encoder.finish()]);
            let error = scope.pop().await;
            assert!(error.is_none(), "a depth-stencil attachment of the pipeline's format must validate: {error:?}");
        });
    }

    /// The declared `max_inter_stage_shader_variables` need must match the
    /// shader it is declared FOR, counted from the shader's own text.
    ///
    /// This is the same drift the test above catches for the uniform's size,
    /// in the one other declared need that is a property of a file rather than
    /// of a Rust type -- and it had already happened: the need said 8 with a
    /// comment reading "`VertexOutput` occupies locations 0-7", while
    /// `aa_inset_px` had since been added at location 8. Nothing caught it
    /// because no platform this crate targets offers fewer than 15, so the
    /// understatement was invisible. It would not have stayed invisible: a
    /// device reporting exactly 8 would have been accepted and then failed
    /// pipeline creation.
    ///
    /// wgpu validates BOTH halves of this (`wgpu-core-29.0.4`'s
    /// `validation.rs:1453-1498`): a vertex output's location must be
    /// `<= limit - 1`, and the COUNT of user-defined outputs must be
    /// `<= limit`. `@builtin(position)` deducts nothing
    /// (`validation/shader_io_deductions.rs:52`) and the deduction that does
    /// exist -- `PointListPrimitiveTopology` -- cannot apply, because this
    /// crate's pipelines are all `TriangleList`. So the need is exactly the
    /// number of `@location` slots in `VertexOutput`, and exactly the largest
    /// of them plus one.
    #[test]
    fn the_declared_inter_stage_need_matches_the_shader_it_is_declared_for() {
        let locations = vertex_output_locations(include_str!("shader.wgsl"));
        assert!(
            !locations.is_empty(),
            "failed to parse any @location out of shader.wgsl's VertexOutput -- \
             the parser, not the shader, is what to look at first"
        );
        let needs = declared_needs();
        let declared = needs
            .iter()
            .find(|(name, _)| *name == "max_inter_stage_shader_variables")
            .expect("the limit is declared");
        assert_eq!(
            declared.1,
            locations.len() as u64,
            "shader.wgsl's VertexOutput fills {} inter-stage slots ({locations:?}), \
             but the declared need says {}",
            locations.len(),
            declared.1
        );
        assert_eq!(
            declared.1,
            u64::from(locations.iter().copied().max().expect("non-empty")) + 1,
            "the locations must be contiguous from 0: wgpu bounds the largest INDEX by \
             `limit - 1` as well as the count by `limit`, so a gap makes one of the two \
             assertions here a lie ({locations:?})"
        );
    }

    /// **The stand-in clip's arithmetic, which is the whole fix on the CPU
    /// side.** Four children tile their parent exactly: adjacent boxes
    /// share an edge, the union spans the tile, and no two overlap.
    ///
    /// A box off by one cell would clip a stand-in to the WRONG quadrant --
    /// covering three tiles that are ready and leaving bare the one that is
    /// not, which is both halves of the bar failed at once and neither of
    /// them visible in a count.
    #[test]
    fn a_tile_clip_gives_each_child_its_own_quarter_of_its_parent() {
        let span = crate::render::mesh::COORD_SCALE * crate::mvt::MVT_EXTENT as f32;
        let parent = crate::geo::TileId { z: 2, x: 1, y: 2 };
        let children: Vec<crate::geo::TileId> = (0..2)
            .flat_map(|dy| {
                (0..2).map(move |dx| crate::geo::TileId {
                    z: 3,
                    x: parent.x * 2 + dx,
                    y: parent.y * 2 + dy,
                })
            })
            .collect();
        let boxes: Vec<[f32; 4]> =
            children.iter().map(|&c| TileClip::for_descendant(parent, c).bounds()).collect();

        let half = span / 2.0;
        assert_eq!(boxes[0], [0.0, 0.0, half, half], "the (0,0) child is the top-left quarter");
        assert_eq!(boxes[1], [half, 0.0, span, half]);
        assert_eq!(boxes[2], [0.0, half, half, span]);
        assert_eq!(boxes[3], [half, half, span, span]);

        // Disjoint interiors, pairwise, stated as the test rather than read
        // off the literals above -- `MapView::paint` now pushes the
        // ancestor once per missing tile, so an overlap here would be a
        // region composited twice by the very code that exists to stop it.
        for (i, a) in boxes.iter().enumerate() {
            for b in &boxes[i + 1..] {
                assert!(
                    a[2] <= b[0] || b[2] <= a[0] || a[3] <= b[1] || b[3] <= a[1],
                    "{a:?} and {b:?} overlap"
                );
            }
        }

        // Two levels down: one sixteenth, in the right place.
        let grandchild = crate::geo::TileId { z: 4, x: parent.x * 4 + 3, y: parent.y * 4 + 1 };
        let quarter = span / 4.0;
        assert_eq!(
            TileClip::for_descendant(parent, grandchild).bounds(),
            [3.0 * quarter, quarter, 4.0 * quarter, 2.0 * quarter]
        );

        // Anything that is not a descendant falls back to the box that
        // draws EVERYTHING, never the one that draws nothing -- a stand-in
        // exists to stop a hole, so a clip that cannot work out its sub-box
        // has to fail back to the old behaviour. See `TileClip::for_descendant`.
        for stranger in [
            crate::geo::TileId { z: 3, x: parent.x * 2 + 2, y: parent.y * 2 }, // next quad over
            crate::geo::TileId { z: 1, x: 0, y: 1 },                           // shallower
            crate::geo::TileId { z: 3, x: 0, y: 0 },                           // elsewhere entirely
        ] {
            assert!(
                TileClip::for_descendant(parent, stranger).is_whole(),
                "{stranger:?} is not a descendant of {parent:?}, so the clip must be WHOLE"
            );
        }
    }

    /// **The star shader must fit inside the needs the crate already
    /// declares**, or it is a second shader silently raising the bar for
    /// every platform.
    ///
    /// `shader_needs` declares 10 inter-stage variables, 11 vertex
    /// attributes, 1 vertex buffer and a uniform binding sized from the
    /// widest uniform struct — all of them for the TILE shader, which is
    /// the widest. `star_shader.wgsl` is narrower on every axis and this is
    /// the test that says so rather than a comment claiming it: two
    /// inter-stage slots, four attributes, one buffer, an 96-byte uniform.
    ///
    /// That matters because `every_platform_this_crate_targets_clears_the_shader_needs`
    /// only checks the DECLARED figures against each platform. A shader
    /// that quietly needed more than the declaration would be accepted at
    /// device creation and fail at pipeline creation — which on iOS, where
    /// `max_inter_stage_shader_variables` is 15 against the WebGPU
    /// baseline's 16, is exactly the crash this crate has already shipped
    /// once.
    #[test]
    fn the_star_shader_fits_inside_the_needs_the_tile_shader_already_declares() {
        let source = include_str!("star_shader.wgsl");
        let locations = vertex_output_locations(source);
        assert_eq!(
            locations.len(),
            2,
            "star_shader.wgsl's VertexOutput fills {locations:?}, not the two slots this \
             test expects -- if a slot was added deliberately, check it against the \
             declared need below rather than just widening this number"
        );
        let needs = declared_needs();
        let declared_inter_stage = needs
            .iter()
            .find(|(name, _)| *name == "max_inter_stage_shader_variables")
            .expect("the limit is declared")
            .1;
        assert!(
            locations.len() as u64 <= declared_inter_stage,
            "the star shader fills {} inter-stage slots but the crate declares a need of \
             only {declared_inter_stage}",
            locations.len()
        );
        assert!(
            u64::from(locations.iter().copied().max().expect("non-empty")) < declared_inter_stage,
            "wgpu bounds the largest inter-stage INDEX by `limit - 1` as well as the \
             count by `limit` ({locations:?})"
        );

        // Four attributes, from the one table the pipeline is built with --
        // read off the shader's own `VertexInput` rather than off
        // `STAR_VERTEX_ATTRS`, so the two cannot drift apart in COUNT.
        let input = source
            .split_once("struct VertexInput {")
            .expect("star_shader.wgsl declares a VertexInput")
            .1
            .split_once("\n}")
            .expect("the struct is closed")
            .0;
        let inputs = input.matches("@location(").count() as u64;
        let declared_attrs =
            needs.iter().find(|(name, _)| *name == "max_vertex_attributes").expect("declared").1;
        assert_eq!(inputs, 4, "the star vertex has four attributes");
        assert!(inputs <= declared_attrs);

        // And the uniform is inside the declared binding size, which is
        // derived from `max(CameraUniform, ModelUniform, StarUniform)` --
        // so this can only fail if the derivation stopped including it.
        let declared_uniform = needs
            .iter()
            .find(|(name, _)| *name == "max_uniform_buffer_binding_size")
            .expect("declared")
            .1;
        assert!(
            std::mem::size_of::<crate::render::space::StarUniform>() as u64 <= declared_uniform,
            "StarUniform is {} bytes against a declared need of {declared_uniform}",
            std::mem::size_of::<crate::render::space::StarUniform>()
        );
        assert_eq!(
            std::mem::size_of::<crate::render::space::StarUniform>() % 16,
            0,
            "a host-shareable WGSL uniform struct holding a mat4x4<f32> must be a \
             multiple of 16 bytes; StarUniform is {}",
            std::mem::size_of::<crate::render::space::StarUniform>()
        );
    }

    /// **The atmosphere shader fits inside the needs the tile shader
    /// already declares — so no platform limit moves for Task 9.**
    ///
    /// This is the most expensive shader in the crate and the one most
    /// likely to ask for something new, so the check is made against the
    /// SAME `declared_needs` table the device-creation path reads, not
    /// against a set of literals. Every figure below is a reason the
    /// declared set is unchanged rather than a coincidence:
    ///
    ///  - **one** inter-stage slot, against a declared 9 and iOS Metal's
    ///    15, because the ray is the only thing crossing the boundary;
    ///  - **zero** vertex attributes and zero vertex buffers — the
    ///    full-screen triangle comes out of `@builtin(vertex_index)` — so
    ///    the tight `max_vertex_attributes` (9 of 16) is untouched;
    ///  - **one** bind group holding **one** buffer, read by both stages,
    ///    against `max_uniform_buffers_per_shader_stage`'s declared 2;
    ///  - a **112-byte** uniform against `ModelUniform`'s 192, which is what
    ///    `max_uniform_buffer_binding_size` is derived from.
    ///
    /// What this does NOT check is the *instruction* cost — five primary and
    /// three secondary steps with two `exp`s each, full-screen. No limit in
    /// `wgpu::Limits` expresses that; what does is
    /// `the_full_screen_atmosphere_costs_less_than_the_plans_half_millisecond`,
    /// and the loops are constant-bounded so a WebGL2 driver that requires
    /// that of a `for` is satisfied.
    #[test]
    fn the_atmosphere_shader_fits_inside_the_needs_the_tile_shader_already_declares() {
        let source = include_str!("atmosphere_shader.wgsl");
        let locations = vertex_output_locations(source);
        assert_eq!(
            locations.len(),
            1,
            "atmosphere_shader.wgsl's VertexOutput fills {locations:?}, not the one slot \
             this test expects -- if a slot was added deliberately, check it against the \
             declared need below rather than just widening this number"
        );
        let needs = declared_needs();
        let declared_inter_stage = needs
            .iter()
            .find(|(name, _)| *name == "max_inter_stage_shader_variables")
            .expect("the limit is declared")
            .1;
        assert!(
            locations.len() as u64 <= declared_inter_stage,
            "the atmosphere shader fills {} inter-stage slots but the crate declares a \
             need of only {declared_inter_stage}",
            locations.len()
        );
        assert!(
            u64::from(locations.iter().copied().max().expect("non-empty")) < declared_inter_stage,
            "wgpu bounds the largest inter-stage INDEX by `limit - 1` as well as the \
             count by `limit` ({locations:?})"
        );

        // NO vertex input at all, read off the shader's own text: the
        // pipeline is built with `buffers: &[]`, and a `@location` appearing
        // in a vertex entry point would make that a validation error rather
        // than a limits question.
        assert!(
            !source.contains("struct VertexInput"),
            "the atmosphere pass takes no vertex buffer -- see `AtmospherePipeline`"
        );
        let vertex_stage = source
            .split_once("fn vs_main")
            .expect("atmosphere_shader.wgsl declares vs_main")
            .1;
        let signature = vertex_stage.split_once(')').expect("the signature is closed").0;
        assert!(
            !signature.contains("@location("),
            "vs_main takes {signature}, which reads a vertex attribute; the pipeline \
             binds no vertex buffer"
        );

        let declared_uniform = needs
            .iter()
            .find(|(name, _)| *name == "max_uniform_buffer_binding_size")
            .expect("declared")
            .1;
        let size = std::mem::size_of::<crate::render::atmosphere::AtmosphereUniform>() as u64;
        assert_eq!(size, 112, "AtmosphereUniform is {size} bytes, not the 112 the docs claim");
        assert!(
            size <= declared_uniform,
            "AtmosphereUniform is {size} bytes against a declared need of \
             {declared_uniform}. RAISE THE DECLARED NEED -- do not shrink the struct to \
             fit a number that was written for a different struct"
        );
        assert_eq!(size % 16, 0);

        // One bind group, one binding in it, read by both stages.
        assert_eq!(
            source.matches("@group(").count(),
            1,
            "the atmosphere pass binds one group; more would need \
             `max_bind_groups`, declared at 2, re-examined"
        );
    }

    /// **The declared `max_vertex_attributes` need must match the shader it
    /// is declared FOR**, counted off `shader.wgsl`'s own `VertexInput`.
    ///
    /// The third member of a family: the uniform's size is derived from its
    /// type, the inter-stage count is scanned from `VertexOutput`, and this
    /// is the one that was left. **It had already drifted**, the same way
    /// and for the same reason as the inter-stage figure Task 8 caught: the
    /// need said 9 with a comment reading "locations 0-8", while
    /// `line-gap-width` casings had inserted `gap_half_px` and
    /// `gap_half_px_to` at 7 and 8 and pushed `line_distance` and
    /// `dash_runs` to 9 and 10. Eleven attributes, declared as nine.
    ///
    /// Invisible, because no platform this crate targets offers fewer than
    /// 16 — and invisible in the dangerous direction: an understated need is
    /// accepted at device creation on hardware that then fails pipeline
    /// creation, which is the iOS crash this crate has already shipped once.
    ///
    /// `TilePipeline::from_source`'s `ATTRS` is asserted against the same
    /// count, so the pipeline's layout and the shader's input cannot
    /// disagree either.
    #[test]
    fn the_declared_vertex_attribute_need_matches_the_shader_it_is_declared_for() {
        let source = include_str!("shader.wgsl");
        let body = source
            .split_once("struct VertexInput {")
            .expect("shader.wgsl declares a VertexInput struct")
            .1;
        let body = body.split_once("\n}").expect("the struct is closed").0;
        let mut locations: Vec<u32> = body
            .match_indices("@location(")
            .filter_map(|(at, _)| {
                let rest = &body[at + "@location(".len()..];
                rest.split_once(')')?.0.trim().parse().ok()
            })
            .collect();
        locations.sort_unstable();
        assert!(!locations.is_empty(), "failed to parse any @location out of VertexInput");
        let declared = declared_needs()
            .into_iter()
            .find(|(name, _)| *name == "max_vertex_attributes")
            .expect("the limit is declared")
            .1;
        assert_eq!(
            declared,
            locations.len() as u64,
            "shader.wgsl's VertexInput declares {} vertex attributes ({locations:?}), but \
             the declared need says {declared}. RAISE the need to the truth -- lowering \
             one to make `every_platform_this_crate_targets_clears_the_shader_needs` pass \
             is how an iOS crash shipped",
            locations.len()
        );
        assert_eq!(
            locations,
            (0..locations.len() as u32).collect::<Vec<_>>(),
            "the attribute locations must be contiguous from 0: `vertex_attr_array!` lays \
             them out by accumulating each format's size in declaration order, so a gap \
             is a silently misaligned vertex rather than an error ({locations:?})"
        );
    }

    /// Every `@location(N)` declared inside `shader.wgsl`'s `VertexOutput`,
    /// which is the struct that crosses the vertex/fragment boundary.
    ///
    /// Deliberately a text scan rather than a naga parse: `naga` is not a
    /// direct dependency of this crate (it arrives through `wgpu` without its
    /// IR feature), and the thing being guarded is the number a HUMAN reads
    /// off the struct when updating the declared need. A scan of the same
    /// text the human reads is the closer check.
    fn vertex_output_locations(source: &str) -> Vec<u32> {
        vertex_output_locations_of(source, "VertexOutput")
    }

    /// [`vertex_output_locations`] for any struct: the `@location`s of
    /// `struct_name`'s body, empty when the shader has no such struct.
    fn vertex_output_locations_of(source: &str, struct_name: &str) -> Vec<u32> {
        let Some((_, body)) = source.split_once(&format!("struct {struct_name} {{")) else {
            return Vec::new();
        };
        let body = body.split_once("\n}").expect("the struct is closed").0;
        body.match_indices("@location(")
            .filter_map(|(at, _)| {
                let rest = &body[at + "@location(".len()..];
                rest.split_once(')')?.0.trim().parse().ok()
            })
            .collect()
    }

    /// The guard's whole promise is "the process is still here afterwards",
    /// so the test drives it the only way that proves that: install a mesh
    /// that cannot fit, then keep using the same pipeline and buffers.
    ///
    /// `max_buffer_size` is squeezed on the CONTEXT rather than by baking a
    /// real 256 MiB mesh -- the guard compares two numbers and does not care
    /// where the smaller one came from, and allocating a quarter-gigabyte of
    /// vertices to test an early return would be a slow way to learn nothing
    /// extra.
    #[test]
    fn a_mesh_too_large_for_one_buffer_is_skipped_rather_than_fatal() {
        pollster::block_on(async {
            let mut ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);

            // Below one triangle's 240 bytes of vertices, above zero.
            ctx.max_buffer_size = 128;

            let refused = pipeline.create_tile_buffers(&ctx, &triangle_mesh([1.0, 0.0, 0.0, 1.0]));
            assert_eq!(
                refused.index_count, 0,
                "a refused mesh must install a zero-length DRAW, which `render` skips outright"
            );

            // The device must still be usable -- this is the whole point. If
            // an error buffer had been created and cached, this next install
            // would either panic or silently reuse the poisoned buffer.
            ctx.max_buffer_size = u64::MAX;
            let repaired =
                pipeline.install_mesh(&ctx, Some(refused), &triangle_mesh([0.0, 1.0, 0.0, 1.0]));
            assert_eq!(
                repaired.index_count, 3,
                "the very next mesh that DOES fit must draw normally: the skip is per-install, \
                 not a latch"
            );
        });
    }

    /// Guards the direction of the fix on real hardware, not a specific
    /// number: what the device is GRANTED must be what the adapter OFFERS.
    /// Asserting this machine's 4 GiB would just re-pin this machine.
    #[test]
    fn the_real_device_is_granted_what_the_adapter_offers() {
        pollster::block_on(async {
            let instance =
                wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
            let Ok(adapter) = instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::LowPower,
                    ..Default::default()
                })
                .await
            else {
                return; // headless CI with no adapter: nothing to assert
            };
            let offered = adapter.limits();
            let ctx = GpuContext::new(&adapter).await.expect("device");
            let granted = ctx.device.limits();

            assert_eq!(
                granted.max_buffer_size, offered.max_buffer_size,
                "`Limits::default()`'s 256 MiB floor on hardware offering more is what made an \
                 oversized mesh fatal"
            );
            assert_eq!(
                granted.max_inter_stage_shader_variables, offered.max_inter_stage_shader_variables,
                "the dimension the baseline was too GREEDY in -- 16 asked, 15 allowed on iOS"
            );
            assert_eq!(
                granted, offered,
                "the whole adapter set, so no dimension can quietly go back to the baseline"
            );
            assert_eq!(
                ctx.max_buffer_size, granted.max_buffer_size,
                "the cached figure must be the one wgpu validates against"
            );
        });
    }

    /// The number is not arbitrary: 275,652,880 bytes is the exact
    /// `tile-vertices` buffer wgpu refused under `Limits::default()`, verbatim
    /// from the abort it caused.
    ///
    /// Reporting a raised limit and being able to USE it are different
    /// claims, and only the second one matters. So this allocates the buffer
    /// rather than reading a field back.
    #[test]
    fn a_buffer_that_used_to_abort_the_process_now_allocates() {
        pollster::block_on(async {
            const THE_BUFFER_THAT_ABORTED: u64 = 275_652_880;

            let instance =
                wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
            let Ok(adapter) = instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::LowPower,
                    ..Default::default()
                })
                .await
            else {
                return; // headless CI with no adapter
            };
            if adapter.limits().max_buffer_size < THE_BUFFER_THAT_ABORTED {
                return; // hardware genuinely smaller than the old floor's failure
            }
            let ctx = GpuContext::new(&adapter).await.expect("device");
            assert!(
                ctx.max_buffer_size >= THE_BUFFER_THAT_ABORTED,
                "the granted ceiling must clear the buffer that used to abort"
            );

            // The allocation itself is the assertion. Under the old
            // `Limits::default()` request this line was a SIGABRT, not a
            // failure -- there would have been no test result to read.
            // Through `TrackedBuffer`, both because the crate requires every
            // creation to go through it and because it is the path the
            // overlay actually takes.
            let buffer = crate::render::alloc::TrackedBuffer::new(
                &ctx.device,
                &wgpu::BufferDescriptor {
                    label: Some("the-buffer-that-aborted"),
                    size: THE_BUFFER_THAT_ABORTED,
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                },
            );
            assert_eq!(buffer.tracked_size(), THE_BUFFER_THAT_ABORTED);
            drop(buffer);
        });
    }

    #[test]
    fn renders_a_solid_triangle_to_an_offscreen_texture() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);

            let size = 64u32;
            let color_texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("test-color"),
                size: wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor::default());
            let depth_view = pipeline.create_depth_view(&ctx, size, size);

            let identity = glam::Mat4::IDENTITY.to_cols_array_2d();
            let camera_bind_group = pipeline.test_camera_bind_group(&ctx, identity, 0.0);
            let tile_buffers = pipeline.create_tile_buffers(&ctx, &triangle_mesh([0.0, 1.0, 0.0, 1.0]));
            let model_bind_group = pipeline.test_model_bind_group(&ctx, identity, 1.0, 0.0);

            let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            pipeline.render(
                &mut encoder,
                &color_view,
                &depth_view,
                wgpu::Color::TRANSPARENT,
                &camera_bind_group,
                &[(&tile_buffers, &model_bind_group)],
                None,
            );

            let bytes_per_row = (size * 4).next_multiple_of(256);
            let output_buffer = crate::render::alloc::TrackedBuffer::new(&ctx.device, &wgpu::BufferDescriptor {
                label: Some("readback"),
                size: (bytes_per_row * size) as u64,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo { texture: &color_texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                wgpu::TexelCopyBufferInfo {
                    buffer: &output_buffer,
                    layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(bytes_per_row), rows_per_image: Some(size) },
                },
                wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 1 },
            );
            ctx.queue.submit(Some(encoder.finish()));

            let slice = output_buffer.slice(..);
            let (tx, rx) = std::sync::mpsc::channel();
            slice.map_async(wgpu::MapMode::Read, move |result| { tx.send(result).unwrap(); });
            ctx.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
            rx.recv().unwrap().unwrap();

            let data = slice.get_mapped_range().expect("buffer should be mapped after poll");
            let center_pixel_offset = (size / 2 * bytes_per_row + (size / 2) * 4) as usize;
            let pixel = &data[center_pixel_offset..center_pixel_offset + 4];

            assert!(pixel[1] > 200, "expected green channel to be bright, got {pixel:?}");
            assert!(pixel[0] < 50, "expected red channel to be dark, got {pixel:?}");
        });
    }

    /// Regression test for a real double-sRGB-encoding bug found via a live
    /// side-by-side comparison against real MapLibre GL JS rendering the
    /// identical style against the identical tile data: every color in this
    /// renderer is authored in sRGB (parsed from CSS strings, or hand-written
    /// `Rgba` literals), but `shader.wgsl`'s `fs_main` used to pass that
    /// already-sRGB-encoded value straight through (`return input.color;`)
    /// into a `Srgb`-format render target. Writing to an `Srgb`-format target
    /// makes the GPU automatically gamma-*encode* whatever linear value the
    /// shader outputs -- so the already-encoded input got encoded a SECOND
    /// time, washing out every non-primary color (pure black/white/primaries
    /// are fixed points of the sRGB curve, so the earlier tests using them
    /// couldn't have caught this). The invariant outlived that fix
    /// and is if anything more direct now: the target is a UNORM format, so
    /// the shader writes the authored value and the target stores it, with
    /// no transfer function on either side.
    ///
    /// This test uses a non-trivial midtone (sRGB 0.5 in every channel,
    /// where a stray gamma conversion costs the most) rather than a
    /// primary, which is a fixed point of the sRGB curve and so could not
    /// catch one:
    ///   - correct pipeline: 0.5 in, 0.5 out (byte ~128).
    ///   - a stray ENCODE: srgb_encode(0.5) = 1.055*0.5^(1/2.4) - 0.055
    ///     ~= 0.735 (byte ~187) -- visibly lighter than authored, which is
    ///     exactly the "washed out" symptom that was observed.
    ///   - a stray DECODE, the mirror mistake and the one now available
    ///     (linearising in the shader with nothing left to re-encode it):
    ///     srgb_to_linear(0.5) ~= 0.214 (byte ~55), a map far too dark.
    #[test]
    fn solid_fill_midtone_color_round_trips_without_a_stray_gamma_encode() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);

            let identity = glam::Mat4::IDENTITY.to_cols_array_2d();
            let camera_bind_group = pipeline.test_camera_bind_group(&ctx, identity, 0.0);
            let data = render_triangle_for_test(&ctx, &pipeline, &camera_bind_group, [0.5, 0.5, 0.5, 1.0]).await;
            let pixel = &data[PROBE..PROBE + 4];

            // Expected ~128 (authored 0.5 round-tripping through a correct
            // single-encode pipeline). Allow generous slack for 8-bit
            // quantization, but tight enough that ~187 (the double-encoded
            // value) cannot pass.
            for (channel, value) in [("r", pixel[0]), ("g", pixel[1]), ("b", pixel[2])] {
                assert!(
                    (110..=145).contains(&value),
                    "expected authored sRGB 0.5 to round-trip to ~128 on channel {channel}, got {value} \
                     (pixel {pixel:?}) -- a value near 187 would indicate the color is still being \
                     double-sRGB-encoded"
                );
            }
        });
    }

    /// The side of `render_triangle_for_test`'s square target.
    const PROBE_SIZE: u32 = 64;
    /// The byte offset, in `render_triangle_for_test`'s readback, of the
    /// centre pixel both the gamma test and the horizon-fade test read.
    /// `PROBE_SIZE * 4` is 256, already a whole `COPY_BYTES_PER_ROW_ALIGNMENT`,
    /// so the rows are tightly packed.
    const PROBE: usize = ((PROBE_SIZE / 2) * PROBE_SIZE * 4 + (PROBE_SIZE / 2) * 4) as usize;

    /// The frame `solid_fill_midtone_color_round_trips_without_a_stray_gamma_encode`
    /// draws, factored out so the horizon-fade test draws the SAME one: a
    /// `PROBE_SIZE` square `Rgba8Unorm` target, `triangle_mesh(rgba)` under
    /// an identity model, cleared transparent, read back as RGBA bytes.
    async fn render_triangle_for_test(
        ctx: &GpuContext,
        pipeline: &TilePipeline,
        camera_bind_group: &wgpu::BindGroup,
        rgba: [f32; 4],
    ) -> Vec<u8> {
        let size = PROBE_SIZE;
        let color_texture = make_color_texture(ctx, size);
        let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let depth_view = pipeline.create_depth_view(ctx, size, size);

        let identity = glam::Mat4::IDENTITY.to_cols_array_2d();
        let tile_buffers = pipeline.create_tile_buffers(ctx, &triangle_mesh(rgba));
        let model_bind_group = pipeline.test_model_bind_group(ctx, identity, 1.0, 0.0);

        let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        pipeline.render(
            &mut encoder,
            &color_view,
            &depth_view,
            wgpu::Color::TRANSPARENT,
            camera_bind_group,
            &[(&tile_buffers, &model_bind_group)],
            None,
        );
        read_all_pixels(ctx, &color_texture, encoder, size).await
    }

    /// The fade reads the uniform: with a band that ends in front of the
    /// triangle, the fill comes out as the fog colour. And `HorizonFog::NONE`
    /// leaves the midtone exactly as the gamma test above sees it.
    #[test]
    fn the_horizon_fade_blends_toward_the_fog_colour_and_none_changes_nothing() {
        let ctx = pollster::block_on(GpuContext::new_headless()).expect("headless device");
        let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
        let identity = glam::Mat4::IDENTITY.to_cols_array_2d();
        let render_with = |fog: Option<HorizonFog>| -> Vec<u8> {
            // Same frame as `solid_fill_midtone_color_round_trips_without_a_stray_gamma_encode`:
            // its target, the identity camera, a 0.5-grey triangle.
            let (buffer, camera_bind_group) = pipeline.create_camera_bind_group(&ctx, identity, 0.0, 1.0);
            if let Some(fog) = fog {
                pipeline.write_horizon_fog(&ctx, &buffer, fog);
            }
            pollster::block_on(render_triangle_for_test(&ctx, &pipeline, &camera_bind_group, [0.5, 0.5, 0.5, 1.0]))
        };
        let untouched = render_with(None);
        let plain = render_with(Some(HorizonFog::NONE));
        assert_eq!(plain, untouched, "writing NONE must change no byte of the frame");
        // Identity matrix: clip w is exactly 1.0 for every vertex, so a
        // band from 0.0 to 0.5 has the triangle entirely past its end.
        let fogged = render_with(Some(HorizonFog { start: 0.0, end: 0.5, color: [1.0, 0.0, 0.0, 1.0] }));
        let rgb = |px: &[u8]| [px[PROBE], px[PROBE + 1], px[PROBE + 2]];
        let red = rgb(&fogged);
        assert!(red[0] >= 254 && red[1] <= 1 && red[2] <= 1, "past the band's end the fill IS the fog colour: {red:?}");
        // And a band wholly BEYOND the triangle leaves it as authored: the
        // fade is a function of depth, not simply "on".
        let beyond = render_with(Some(HorizonFog { start: 2.0, end: 3.0, color: [1.0, 0.0, 0.0, 1.0] }));
        assert_eq!(beyond, untouched, "in front of the band the fill is untouched");
    }

    /// **A RASTER OVER THE BASEMAP.** `RasterDraws::after_tile_draws` puts
    /// the imagery after that many tile draws instead of before them all:
    /// 0 (every basemap imagery source) leaves a red fill over an opaque
    /// grey raster, 1 puts the grey over the fill, and a TRANSPARENT texel
    /// drawn over the fill leaves the fill showing -- a data raster (a COG)
    /// with the basemap visible outside its bounds and over its nodata.
    #[test]
    fn a_raster_after_the_basemap_draws_over_its_fills_and_shows_them_where_transparent() {
        let ctx = pollster::block_on(GpuContext::new_headless()).expect("headless device");
        let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
        let raster = crate::render::raster_pipeline::RasterPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, &pipeline);
        let identity = glam::Mat4::IDENTITY.to_cols_array_2d();
        let tile = crate::geo::TileId { z: 16, x: 51661, y: 32532 };
        let square = raster.create_mesh_buffers(
            &ctx,
            &crate::render::raster_mesh::build(
                crate::render::raster_mesh::RasterMeshKey::for_tile(tile),
                4096,
                crate::render::mesh::COORD_SCALE,
            ),
        );
        let span = 4096.0 * crate::render::mesh::COORD_SCALE;
        let to_ndc = glam::Mat4::from_translation(glam::Vec3::new(-1.0, -1.0, 0.0))
            * glam::Mat4::from_scale(glam::Vec3::new(2.0 / span, 2.0 / span, 1.0));
        let raster_model = pipeline.test_model_bind_group(&ctx, to_ndc.to_cols_array_2d(), 0.0, 1.0);
        let fill = pipeline.create_tile_buffers(&ctx, &triangle_mesh([1.0, 0.0, 0.0, 1.0]));
        let fill_model = pipeline.test_model_bind_group(&ctx, identity, 1.0, 0.0);
        let (_camera_buffer, camera_bind_group) = pipeline.create_camera_bind_group(&ctx, identity, 0.0, 1.0);

        let render_with = |texel: [u8; 4], after_tile_draws: usize| -> [u8; 3] {
            let texture = raster.upload_tile(&ctx, &image::RgbaImage::from_pixel(4, 4, image::Rgba(texel)));
            let (_paint_buffer, paint) = raster.create_paint_bind_group(&ctx, &texture, 1.0);
            let color_texture = make_color_texture(&ctx, PROBE_SIZE);
            let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor::default());
            let depth_view = pipeline.create_depth_view(&ctx, PROBE_SIZE, PROBE_SIZE);
            let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            let raster_draws = [(&square, &raster_model, &paint)];
            pipeline.render_with_space_and_raster(
                &mut encoder,
                &color_view,
                &depth_view,
                wgpu::Color::TRANSPARENT,
                &camera_bind_group,
                &SpaceDraw::NONE,
                Some(RasterDraws { pipeline: &raster, draws: &raster_draws, after_tile_draws }),
                &[TileDraw::Legacy(&fill, &fill_model)],
                None,
            );
            let px = pollster::block_on(read_all_pixels(&ctx, &color_texture, encoder, PROBE_SIZE));
            [px[PROBE], px[PROBE + 1], px[PROBE + 2]]
        };
        let is_red = |c: [u8; 3]| c[0] >= 250 && c[1] <= 5 && c[2] <= 5;
        let is_grey = |c: [u8; 3]| c.iter().all(|v| (120..=136).contains(v));
        let under = render_with([128, 128, 128, 255], 0);
        assert!(is_red(under), "imagery under the basemap: the fill covers it: {under:?}");
        let over = render_with([128, 128, 128, 255], 1);
        assert!(is_grey(over), "a raster after the basemap covers the fill: {over:?}");
        let clear = render_with([0, 0, 0, 0], 1);
        assert!(is_red(clear), "a transparent texel over the basemap shows the fill: {clear:?}");
    }

    /// **IMAGERY FADES TOO.** The same three frames as the fill test above,
    /// drawn through `RasterPipeline`: a mid-grey raster square filling the
    /// target under an identity camera (so every vertex's clip `w` is 1.0).
    /// Before the fix `raster_shader.wgsl` declared `fog_range` and never
    /// read it, so the band that turns the fill red left imagery grey.
    #[test]
    fn the_horizon_fade_blends_raster_imagery_toward_the_fog_colour_and_none_changes_nothing() {
        let ctx = pollster::block_on(GpuContext::new_headless()).expect("headless device");
        let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
        let raster = crate::render::raster_pipeline::RasterPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, &pipeline);
        let identity = glam::Mat4::IDENTITY.to_cols_array_2d();
        let tile = crate::geo::TileId { z: 16, x: 51661, y: 32532 };
        let square = raster.create_mesh_buffers(
            &ctx,
            &crate::render::raster_mesh::build(
                crate::render::raster_mesh::RasterMeshKey::for_tile(tile),
                4096,
                crate::render::mesh::COORD_SCALE,
            ),
        );
        // The tile's lyon square onto the whole of NDC.
        let span = 4096.0 * crate::render::mesh::COORD_SCALE;
        let to_ndc = glam::Mat4::from_translation(glam::Vec3::new(-1.0, -1.0, 0.0))
            * glam::Mat4::from_scale(glam::Vec3::new(2.0 / span, 2.0 / span, 1.0));
        let model = pipeline.test_model_bind_group(&ctx, to_ndc.to_cols_array_2d(), 0.0, 1.0);
        let texture = raster.upload_tile(&ctx, &image::RgbaImage::from_pixel(4, 4, image::Rgba([128, 128, 128, 255])));
        let (_paint_buffer, paint) = raster.create_paint_bind_group(&ctx, &texture, 1.0);

        let render_with = |fog: Option<HorizonFog>| -> Vec<u8> {
            let (buffer, camera_bind_group) = pipeline.create_camera_bind_group(&ctx, identity, 0.0, 1.0);
            if let Some(fog) = fog {
                pipeline.write_horizon_fog(&ctx, &buffer, fog);
            }
            let color_texture = make_color_texture(&ctx, PROBE_SIZE);
            let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor::default());
            let depth_view = pipeline.create_depth_view(&ctx, PROBE_SIZE, PROBE_SIZE);
            let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            let draws = [(&square, &model, &paint)];
            pipeline.render_with_space_and_raster(
                &mut encoder,
                &color_view,
                &depth_view,
                wgpu::Color::TRANSPARENT,
                &camera_bind_group,
                &SpaceDraw::NONE,
                Some(RasterDraws { pipeline: &raster, draws: &draws, after_tile_draws: 0 }),
                &[],
                None,
            );
            pollster::block_on(read_all_pixels(&ctx, &color_texture, encoder, PROBE_SIZE))
        };
        let rgb = |px: &[u8]| [px[PROBE], px[PROBE + 1], px[PROBE + 2]];
        let untouched = render_with(None);
        let grey = rgb(&untouched);
        assert!(grey.iter().all(|c| (120..=136).contains(c)), "the fixture draws its grey: {grey:?}");
        assert_eq!(render_with(Some(HorizonFog::NONE)), untouched, "writing NONE must change no byte of the frame");
        let red = rgb(&render_with(Some(HorizonFog { start: 0.0, end: 0.5, color: [1.0, 0.0, 0.0, 1.0] })));
        assert!(red[0] >= 254 && red[1] <= 1 && red[2] <= 1, "past the band's end imagery IS the fog colour: {red:?}");
        let beyond = render_with(Some(HorizonFog { start: 2.0, end: 3.0, color: [1.0, 0.0, 0.0, 1.0] }));
        assert_eq!(beyond, untouched, "in front of the band imagery is untouched");
    }

    /// `write_horizon_fog` writes six floats at byte 72 and
    /// `write_circle_frame` four at 96, and `CameraUniform` is where
    /// `shader.wgsl`'s `Camera` says: `fog_range` at 72, `fog_color` at 80,
    /// `circle_frame` at 96, 112 bytes in all.
    #[test]
    fn the_camera_uniform_is_112_bytes_with_the_fade_and_circle_frame_where_the_shader_reads_them() {
        assert_eq!(std::mem::size_of::<CameraUniform>(), 112);
        assert_eq!(std::mem::offset_of!(CameraUniform, fog_range), 72);
        assert_eq!(std::mem::offset_of!(CameraUniform, fog_color), 80);
        assert_eq!(CIRCLE_FRAME_OFFSET, 96);
    }

    /// The clear value gets the same colour-space treatment a fragment
    /// does. `MapView` clears the tile pass with the style's `background`
    /// layer colour, and a clear never passes through `shader.wgsl`, so
    /// before `TilePipeline::clear_color` the background was the one colour
    /// in the whole renderer still being sRGB-encoded twice.
    ///
    /// Same midtone and same tolerance as
    /// `solid_fill_midtone_color_round_trips_without_a_stray_gamma_encode`
    /// above, deliberately: this is that test's missing
    /// half, on the path that draws nothing at all. ~187 is the
    /// double-encoded value the old code produced.
    #[test]
    fn a_cleared_background_round_trips_like_a_fragment_does() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);

            let size = 64u32;
            let color_texture = make_color_texture(&ctx, size);
            let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor::default());
            let depth_view = pipeline.create_depth_view(&ctx, size, size);
            let identity = glam::Mat4::IDENTITY.to_cols_array_2d();
            let camera_bind_group = pipeline.test_camera_bind_group(&ctx, identity, 0.0);

            let authored = crate::style::Rgba { r: 0.5, g: 0.5, b: 0.5, a: 1.0 };
            let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            // No draws at all: the clear IS the frame.
            pipeline.render(
                &mut encoder,
                &color_view,
                &depth_view,
                TilePipeline::clear_color(&authored),
                &camera_bind_group,
                &[],
                None,
            );

            let data = read_all_pixels(&ctx, &color_texture, encoder, size).await;
            let bytes_per_row = (size * 4).next_multiple_of(256);
            let offset = (size / 2 * bytes_per_row + (size / 2) * 4) as usize;
            let pixel = &data[offset..offset + 4];
            for (channel, value) in [("r", pixel[0]), ("g", pixel[1]), ("b", pixel[2])] {
                assert!(
                    (110..=145).contains(&value),
                    "expected an authored sRGB 0.5 background to clear to ~128 on channel {channel}, \
                     got {value} (pixel {pixel:?}) -- a value near 187 means the clear colour is \
                     still being double-sRGB-encoded"
                );
            }
        });
    }

    /// The arithmetic on its own, on the colour that exposed the defect
    /// this pairing exists to prevent: Mapbox Standard Dark's
    /// `background-color` under this fixture's night+monochrome config.
    ///
    /// Both halves of the pairing are pinned here, because getting either
    /// one alone wrong is the same visible bug in opposite directions. The
    /// clear value must go to the GPU as the authored sRGB triple, AND the
    /// target must be a format that stores it -- rgb(24,24,24), not the
    /// rgb(87,87,86) a `*Srgb` target would encode it into (the value that
    /// was actually measured off a real window capture when this function
    /// converted and the target encoded a second time).
    #[test]
    fn the_clear_colour_reaches_a_unorm_target_as_the_authored_srgb_triple() {
        let authored = crate::style::Rgba { r: 0.096, g: 0.094_666_67, b: 0.094, a: 1.0 };
        let clear = TilePipeline::clear_color(&authored);

        for (channel, cleared, authored) in
            [("r", clear.r, authored.r), ("g", clear.g, authored.g), ("b", clear.b, authored.b)]
        {
            assert_eq!(
                (cleared * 255.0).round() as i32,
                (authored * 255.0).round() as i32,
                "channel {channel} must reach a Unorm target unchanged",
            );
        }
        assert_eq!(clear.a, 1.0, "alpha is linear coverage in both spaces and must not be converted");

        // The half that is NOT in this function: had the clear value been
        // linearized here, an `*Srgb` target's encode-on-write would put it
        // back -- but this renderer's target does not encode, so the linear
        // value would be STORED and the ground would read rgb(24,24,24)
        // instead of... this. Stated as arithmetic so the number is on the
        // record rather than in a comment.
        let linearize = |c: f64| if c <= 0.040_45 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) };
        assert_eq!(
            (linearize(authored.r as f64) * 255.0).round() as i32,
            2,
            "linearizing without a target that re-encodes would clear to near-black",
        );
    }

    /// **The colour-space decision, measured.**
    ///
    /// `#99ddff` at 50% over an opaque `#ece7e4` -- both real values out of
    /// the vendored styles, and the pair the mismatch was originally caught
    /// on. What comes out depends entirely on the space the fixed-function
    /// blender works in:
    ///
    ///   * sRGB (a UNORM target: what this renderer, MapLibre GL JS, Mapbox
    ///     GL JS and all four MapLibre Native backends use):
    ///     `0.5*0x99 + 0.5*0xec = 194.5` -> **#c2e2f2**, byte for byte what
    ///     Mapbox GL JS produces.
    ///   * linear light (an `*Srgb` target, which decodes both operands,
    ///     blends, and re-encodes): 0.3185 and 0.8388 average to 0.5787,
    ///     encoding back to 200 -> **#c8e2f2**. Six units out on red, and up
    ///     to dE2000 4.24 across the values these styles actually use --
    ///     well past the ~2.3 at which a colour difference is visible.
    ///
    /// Note that green and blue agree to the byte between the two: this is
    /// not a difference a spot check on one channel would find, and no
    /// opaque colour can expose it at all. It takes a translucent layer over
    /// a specific ground, which is what this test is.
    #[test]
    fn a_translucent_layer_composites_in_srgb_the_way_the_reference_does() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);

            let size = 64u32;
            let color_texture = make_color_texture(&ctx, size);
            let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor::default());
            let depth_view = pipeline.create_depth_view(&ctx, size, size);
            let identity = glam::Mat4::IDENTITY.to_cols_array_2d();
            let camera_bind_group = pipeline.test_camera_bind_group(&ctx, identity, 0.0);

            // `#ece7e4`, the ground, opaque.
            let ground = triangle_mesh([236.0 / 255.0, 231.0 / 255.0, 228.0 / 255.0, 1.0]);
            // `#99ddff` at half alpha, over it.
            let over = triangle_mesh([153.0 / 255.0, 221.0 / 255.0, 1.0, 0.5]);
            let ground_buffers = pipeline.create_tile_buffers(&ctx, &ground);
            let over_buffers = pipeline.create_tile_buffers(&ctx, &over);
            let ground_model = pipeline.test_model_bind_group(&ctx, identity, 1.0, 0.0);
            let over_model = pipeline.test_model_bind_group(&ctx, identity, 1.0, 0.0);

            let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            pipeline.render(
                &mut encoder,
                &color_view,
                &depth_view,
                wgpu::Color::TRANSPARENT,
                &camera_bind_group,
                // Draw order is blend order: the ground first, the
                // translucent layer over it. Both sit at z=0 and the depth
                // test is `LessEqual`, so the second is not rejected.
                &[(&ground_buffers, &ground_model), (&over_buffers, &over_model)],
                None,
            );

            let data = read_all_pixels(&ctx, &color_texture, encoder, size).await;
            let bytes_per_row = (size * 4).next_multiple_of(256);
            let offset = (size / 2 * bytes_per_row + (size / 2) * 4) as usize;
            let pixel = &data[offset..offset + 4];

            // The alternative, as arithmetic rather than as a comment: what
            // the SAME two colours composite to when the blender decodes to
            // linear light first. It is 200, and this test's tolerance is
            // one -- so nothing that produces it can pass.
            let linear = |c: f64| if c <= 0.040_45 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) };
            let encode = |l: f64| if l <= 0.003_130_8 { 12.92 * l } else { 1.055 * l.powf(1.0 / 2.4) - 0.055 };
            let linear_light_red = encode((linear(153.0 / 255.0) + linear(236.0 / 255.0)) / 2.0);
            assert_eq!((linear_light_red * 255.0).round() as u8, 200, "the linear-light answer, for the record");

            // Mapbox GL JS's answer, to the byte, with one unit of slack for
            // the blender's own rounding.
            for (channel, value, expected) in [("r", pixel[0], 194u8), ("g", pixel[1], 226), ("b", pixel[2], 242)] {
                assert!(
                    value.abs_diff(expected) <= 1,
                    "channel {channel}: expected #c2e2f2 (Mapbox GL JS's own composite), got {pixel:?} -- \
                     a red near 200 means the blend is happening in linear light again",
                );
            }
        });
    }

    /// Direct proof that continuous blending reaches the screen, not just
    /// the vertex data model: a triangle whose `color`/`color_to` pair is
    /// pure red at the lower endpoint and pure blue at the upper one,
    /// rendered with `blend_t` set to exactly halfway (0.5, as `MapView::
    /// tile_blend_t` computes for a camera zoom exactly half a level into
    /// its display-zoom bucket), must read back as an even red/blue blend
    /// -- not pure red (which would mean the shader ignored `color_to`/
    /// `model.blend_t` entirely, the pre-fix "staircase" behavior).
    /// `blend_t` is a single value passed directly in the model uniform
    /// (see `blend_t_of_one_selects_the_upper_endpoint_of_the_baked_
    /// color_pair` below for the upper-endpoint case), not derived from
    /// `camera.zoom` in the shader, so the camera's own zoom here is
    /// incidental.
    #[test]
    fn fractional_zoom_continuously_blends_between_the_vertex_color_pair() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);

            let size = 64u32;
            let color_texture = make_color_texture(&ctx, size);
            let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor::default());
            let depth_view = pipeline.create_depth_view(&ctx, size, size);

            let identity = glam::Mat4::IDENTITY.to_cols_array_2d();
            let camera_bind_group = pipeline.test_camera_bind_group(&ctx, identity, 9.5);
            // blend_t = 0.5 -- exactly halfway between the baked pair's two
            // endpoints, matching what `MapView::tile_blend_t` (`camera.
            // zoom.fract()`) produces when the camera sits halfway through
            // the current display-zoom bucket.
            let model_bind_group = pipeline.test_model_bind_group(&ctx, identity, 0.5, 0.0);

            let mesh = crate::render::TileMesh {
                vertices: vec![
                    Vertex {
                        position: [-500.0, -500.0], normal: [0.0, 0.0],
                        color: [1.0, 0.0, 0.0, 1.0], is_stroke: 0.0, half_width_px: 0.0,
                        color_to: [0.0, 0.0, 1.0, 1.0], half_width_px_to: 0.0, gap_half_px: 0.0, gap_half_px_to: 0.0,
                        line_distance: 0.0, dash_runs: [0.0; 4], line_progress: -1.0,
                    },
                    Vertex {
                        position: [500.0, -500.0], normal: [0.0, 0.0],
                        color: [1.0, 0.0, 0.0, 1.0], is_stroke: 0.0, half_width_px: 0.0,
                        color_to: [0.0, 0.0, 1.0, 1.0], half_width_px_to: 0.0, gap_half_px: 0.0, gap_half_px_to: 0.0,
                        line_distance: 0.0, dash_runs: [0.0; 4], line_progress: -1.0,
                    },
                    Vertex {
                        position: [0.0, 500.0], normal: [0.0, 0.0],
                        color: [1.0, 0.0, 0.0, 1.0], is_stroke: 0.0, half_width_px: 0.0,
                        color_to: [0.0, 0.0, 1.0, 1.0], half_width_px_to: 0.0, gap_half_px: 0.0, gap_half_px_to: 0.0,
                        line_distance: 0.0, dash_runs: [0.0; 4], line_progress: -1.0,
                    },
                ],
                indices: vec![0, 1, 2],
            };
            let tile_buffers = pipeline.create_tile_buffers(&ctx, &mesh);

            let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            pipeline.render(
                &mut encoder,
                &color_view,
                &depth_view,
                wgpu::Color::TRANSPARENT,
                &camera_bind_group,
                &[(&tile_buffers, &model_bind_group)],
                None,
            );

            let data = read_all_pixels(&ctx, &color_texture, encoder, size).await;
            let bytes_per_row = (size * 4).next_multiple_of(256);
            let center_pixel_offset = (size / 2 * bytes_per_row + (size / 2) * 4) as usize;
            let pixel = &data[center_pixel_offset..center_pixel_offset + 4];

            // Pure red (pre-fix / buggy) would read ~255 red, ~0 blue.
            // An even 50/50 blend, after sRGB round-tripping (same
            // pipeline the midtone test above already validates), reads
            // roughly mid-range on both channels -- clearly distinct from
            // both pure-red and pure-blue.
            assert!(
                pixel[0] > 60 && pixel[0] < 200,
                "expected red channel to be a mid-blend, not saturated, got {pixel:?}"
            );
            assert!(
                pixel[2] > 60 && pixel[2] < 200,
                "expected blue channel to be a mid-blend, not zero, got {pixel:?}"
            );
        });
    }

    /// Endpoint counterpart to `fractional_zoom_continuously_blends_
    /// between_the_vertex_color_pair` above, which covers the MIDPOINT
    /// (`blend_t = 0.5`): a `blend_t` of exactly `1.0` is a value `MapView::
    /// tile_blend_t` (`camera.zoom.fract()`) never actually produces --
    /// `f64::fract()` returns `[0, 1)`, and right at the next integer
    /// display-zoom bucket it wraps to `0.0` instead (see
    /// `tile_blend_t(11.0) == 0.0` in map_view.rs). `1.0` is the supremum
    /// the interpolant approaches asymptotically within a bucket but never
    /// attains. It's fed directly here, bypassing `tile_blend_t`, to
    /// exercise the far endpoint of the shader's `mix` -- which must
    /// select the pair's UPPER endpoint (pure blue) in the shader, not
    /// something else (e.g. an off-by-one in `mix`'s argument order, or a
    /// value that got silently clamped/wrapped before reaching the
    /// uniform).
    ///
    /// Renamed and rewritten from `blend_t_clamps_instead_of_wrapping_
    /// when_camera_zoom_drifts_past_a_tiles_own_bracket`: that name and
    /// its doc described the retired per-tile `clamp(camera.zoom - tile.z,
    /// 0, 1)` formula and a "confirmed live bug" that formula fixed. Under
    /// display-zoom rebaking there is no stale bracket left to drift past
    /// (see `shader.wgsl`'s updated comment on `blend_t`), so kept rather
    /// than deleted -- endpoint coverage of `blend_t` reaching the shader
    /// is still useful on its own -- but renamed to describe what it
    /// verifies now, with the dead clamp-formula recomputation replaced
    /// by the literal value it always evaluated to.
    #[test]
    fn blend_t_of_one_selects_the_upper_endpoint_of_the_baked_color_pair() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);

            let size = 64u32;
            let color_texture = make_color_texture(&ctx, size);
            let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor::default());
            let depth_view = pipeline.create_depth_view(&ctx, size, size);

            let identity = glam::Mat4::IDENTITY.to_cols_array_2d();
            // camera.zoom doesn't feed into blend_t in the shader at all
            // (see shader.wgsl) -- it's passed directly here, at its
            // upper-bracket-endpoint value.
            let camera_bind_group = pipeline.test_camera_bind_group(&ctx, identity, 15.0);
            let model_bind_group = pipeline.test_model_bind_group(&ctx, identity, 1.0, 0.0);

            let mesh = crate::render::TileMesh {
                vertices: vec![
                    Vertex {
                        position: [-500.0, -500.0], normal: [0.0, 0.0],
                        color: [1.0, 0.0, 0.0, 1.0], is_stroke: 0.0, half_width_px: 0.0,
                        color_to: [0.0, 0.0, 1.0, 1.0], half_width_px_to: 0.0, gap_half_px: 0.0, gap_half_px_to: 0.0,
                        line_distance: 0.0, dash_runs: [0.0; 4], line_progress: -1.0,
                    },
                    Vertex {
                        position: [500.0, -500.0], normal: [0.0, 0.0],
                        color: [1.0, 0.0, 0.0, 1.0], is_stroke: 0.0, half_width_px: 0.0,
                        color_to: [0.0, 0.0, 1.0, 1.0], half_width_px_to: 0.0, gap_half_px: 0.0, gap_half_px_to: 0.0,
                        line_distance: 0.0, dash_runs: [0.0; 4], line_progress: -1.0,
                    },
                    Vertex {
                        position: [0.0, 500.0], normal: [0.0, 0.0],
                        color: [1.0, 0.0, 0.0, 1.0], is_stroke: 0.0, half_width_px: 0.0,
                        color_to: [0.0, 0.0, 1.0, 1.0], half_width_px_to: 0.0, gap_half_px: 0.0, gap_half_px_to: 0.0,
                        line_distance: 0.0, dash_runs: [0.0; 4], line_progress: -1.0,
                    },
                ],
                indices: vec![0, 1, 2],
            };
            let tile_buffers = pipeline.create_tile_buffers(&ctx, &mesh);

            let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            pipeline.render(
                &mut encoder,
                &color_view,
                &depth_view,
                wgpu::Color::TRANSPARENT,
                &camera_bind_group,
                &[(&tile_buffers, &model_bind_group)],
                None,
            );

            let data = read_all_pixels(&ctx, &color_texture, encoder, size).await;
            let bytes_per_row = (size * 4).next_multiple_of(256);
            let center_pixel_offset = (size / 2 * bytes_per_row + (size / 2) * 4) as usize;
            let pixel = &data[center_pixel_offset..center_pixel_offset + 4];

            // Selects the blue endpoint outright: red near 0, blue near 255
            // (allowing for sRGB round-trip quantization, same tolerance
            // pattern as the existing midtone round-trip test).
            assert!(pixel[0] < 30, "expected red channel near 0 (blue endpoint selected), got {pixel:?}");
            assert!(pixel[2] > 225, "expected blue channel near 255 (endpoint selected), got {pixel:?}");
        });
    }

    async fn read_all_pixels(
        ctx: &GpuContext,
        color_texture: &wgpu::Texture,
        encoder: wgpu::CommandEncoder,
        size: u32,
    ) -> Vec<u8> {
        let mut encoder = encoder;
        let bytes_per_row = (size * 4).next_multiple_of(256);
        let output_buffer = crate::render::alloc::TrackedBuffer::new(&ctx.device, &wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (bytes_per_row * size) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: color_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &output_buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(size),
                },
            },
            wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 1 },
        );
        ctx.queue.submit(Some(encoder.finish()));

        let slice = output_buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            tx.send(result).unwrap();
        });
        ctx.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        rx.recv().unwrap().unwrap();

        let data = slice.get_mapped_range().expect("buffer should be mapped after poll");
        let mut out = vec![0u8; (bytes_per_row * size) as usize];
        out.copy_from_slice(&data);
        out
    }

    /// Renders `draws` into a `size`-square target cleared to `clear`, with
    /// a camera bind group for `camera`.
    pub(crate) async fn render_tile_draws(
        ctx: &GpuContext,
        pipeline: &TilePipeline,
        camera: &crate::render::Camera,
        size: u32,
        clear: wgpu::Color,
        draws: &[TileDraw<'_>],
    ) -> Vec<u8> {
        let color_texture = make_color_texture(ctx, size);
        let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let depth_view = pipeline.create_depth_view(ctx, size, size);
        let camera_bind_group =
            pipeline.test_camera_bind_group(ctx, camera.view_projection().to_cols_array_2d(), camera.zoom as f32);
        let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        pipeline.render_with_space_and_raster(
            &mut encoder,
            &color_view,
            &depth_view,
            clear,
            &camera_bind_group,
            &SpaceDraw::NONE,
            None,
            draws,
            None,
        );
        read_all_pixels(ctx, &color_texture, encoder, size).await
    }

    /// A 3-op tile plan: an opaque fill with a hole, a translucent fill
    /// whose colour pair differs (so `blend_t` mixes), and a third fill
    /// straddling the tile edge into the buffer. Axis-aligned rings at
    /// integer MVT coordinates, so every vertex -- the subdivision's
    /// included -- is an integer and the compact path's i16 snap is exact.
    fn compact_test_fill_plan() -> crate::style::PaintPlan {
        let rect = |x0: i32, y0: i32, x1: i32, y1: i32| {
            geo_types::LineString::from(vec![(x0, y0), (x1, y0), (x1, y1), (x0, y1), (x0, y0)])
        };
        let rgba = |r, g, b, a| Rgba { r, g, b, a };
        let fill = |from: Rgba, to: Rgba, polygon: geo_types::Polygon<i32>| crate::style::PaintOp::Fill {
            color_from: from,
            color_to: to,
            geometry: std::sync::Arc::new(geo_types::Geometry::Polygon(polygon)),
        };
        crate::style::PaintPlan {
            ops: vec![
                fill(
                    rgba(0.2, 0.5, 0.3, 1.0),
                    rgba(0.2, 0.5, 0.3, 1.0),
                    geo_types::Polygon::new(rect(300, 300, 3700, 3500), vec![rect(1200, 1100, 2500, 2600)]),
                ),
                fill(rgba(0.9, 0.1, 0.1, 0.4), rgba(0.1, 0.1, 0.9, 0.7), geo_types::Polygon::new(rect(900, 700, 3100, 3900), vec![])),
                fill(rgba(1.0, 0.8, 0.0, 0.6), rgba(1.0, 0.8, 0.0, 0.6), geo_types::Polygon::new(rect(-700, 2000, 1500, 4600), vec![])),
            ],
        }
    }

    /// Every vendored style (`examples/*-style.json`, `demos/*-style.json`).
    fn vendored_styles() -> Vec<(String, crate::style::Style)> {
        let root = env!("CARGO_MANIFEST_DIR");
        let mut out = Vec::new();
        for dir in ["examples", "demos"] {
            let mut paths: Vec<_> = std::fs::read_dir(format!("{root}/{dir}"))
                .expect("style dir")
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.to_string_lossy().ends_with("-style.json"))
                .collect();
            paths.sort();
            for path in paths {
                let bytes = std::fs::read(&path).expect("style file");
                // A style whose layers come from an unresolved `imports`
                // entry (mapbox-standard-dark) does not compile alone; its
                // `-reference` twin is the resolved form and is compared.
                match crate::style_spec::parse_and_compile(&bytes) {
                    Ok(style) => out.push((path.file_name().unwrap().to_string_lossy().into_owned(), style)),
                    Err(e) => eprintln!("vendored_styles: skipping {}: {e}", path.display()),
                }
            }
        }
        out
    }

    /// **THE TASK 8 GATE, extended to strokes.** A basemap tile installed
    /// split (`install_basemap_mesh`: fills on the 8-byte vertex, undashed
    /// strokes on the 16-byte one, the rest on the 88-byte one, drawn as
    /// interleaved runs) must paint exactly what the unsplit 88-byte install
    /// paints -- for every vendored style over the real Singapore z14 tile,
    /// at four display zooms, flat and on the globe, whole and clipped to one
    /// quadrant as a stand-in is.
    ///
    /// 0 pixels may differ, flat and on the globe (the tile re-placed at z11,
    /// transition 0 / 0.7 / 1, and at z3 so the globe subdivision cuts it):
    /// the split converts only vertices whose position the compact vertices
    /// reproduce bit for bit, and a stroke keeps its f32 normal. A 0.1 %
    /// colour error in `vs_compact_tile`, or a run drawn out of order, fails
    /// it; so does a 0.1 % stroke-width error in the paint table, which the
    /// sabotage frames below prove on every run.
    #[test]
    fn a_split_basemap_tile_is_pixel_identical_to_the_88_byte_tile_for_every_vendored_style() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 384u32;
            let extent = crate::mvt::MVT_EXTENT;
            let bytes = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/benches/fixtures/singapore_z14.pbf"))
                .expect("vendored Singapore tile");
            let decoded = crate::mvt::decode(bytes).expect("fixture decodes");
            let real = crate::geo::TileId { z: 14, x: 12918, y: 8133 };
            // The same data re-placed where the globe is drawn: z11 is the last
            // zoom at full globe (`globe::transition_for_zoom`), z3 is cut by
            // the globe subdivision.
            let mid = crate::geo::TileId { z: 11, x: 1614, y: 1016 };
            let low = crate::geo::TileId { z: 3, x: 6, y: 3 };
            let (mut compared, mut split_tiles, mut stroke_tiles, mut painted) = (0usize, 0usize, 0usize, 0usize);
            let (mut legacy_bytes, mut split_bytes) = (0u64, 0u64);
            let (mut stroke_vertices, mut legacy_stroke_vertices, mut max_runs) = (0usize, 0usize, 0usize);
            let (mut sabotaged_frames, mut sabotaged_px) = (0usize, 0usize);
            for (name, style) in vendored_styles() {
                let matched = crate::style::match_tile_static(&decoded, &style);
                for display_zoom in [12u8, 14, 15, 17] {
                    let plan = crate::style::resolve_plan(&decoded, &style, &matched, display_zoom);
                    for (tile, transitions) in [
                        (real, &[0.0_f32][..]),
                        (mid, &[0.0_f32, 0.7, 1.0][..]),
                        (low, &[0.0_f32, 0.6, 1.0][..]),
                    ] {
                        let mesh = crate::render::tessellate_plan(
                            &plan,
                            extent,
                            crate::render::mesh::COORD_SCALE,
                            crate::render::subdivide::Subdivision::for_tile(tile),
                        );
                        if mesh.indices.is_empty() {
                            continue;
                        }
                        let Some(split) = crate::render::compact::split_tile_mesh_with(&mesh, true) else {
                            continue;
                        };
                        let legacy_buffers = pipeline.create_tile_buffers(&ctx, &mesh);
                        split_tiles += 1;
                        max_runs = max_runs.max(split.runs.len());
                        stroke_vertices += split.strokes.len();
                        legacy_stroke_vertices += split.legacy.vertices.iter().filter(|v| v.is_stroke > 0.5).count();
                        // The sabotage: every stroke 0.1 % wider in the table.
                        let sabotaged = (tile == real && !split.stroke_paints.is_empty()).then(|| {
                            let mut s = split.clone();
                            for p in &mut s.stroke_paints {
                                p.widths[0] *= 1.001;
                                p.widths[1] *= 1.001;
                            }
                            pipeline.install_split(&ctx, None, s)
                        });
                        let split_buffers = pipeline.install_split(&ctx, None, split);
                        if tile == real && display_zoom == 14 {
                            // Per style, the real tile at its own zoom: the
                            // 88-byte, fills-only and fills + strokes bytes.
                            let bytes = |b: &TileBuffers| {
                                b.vertex_capacity_bytes() + b.index_capacity_bytes() + b.compact_bytes()
                            };
                            let fills_only = crate::render::compact::split_tile_mesh_with(&mesh, false)
                                .map(|s| bytes(&pipeline.install_split(&ctx, None, s)));
                            eprintln!(
                                "singapore z14 {name}: {} vertices, 88-byte {} B, fills-only {:?} B, \
                                 fills + strokes {} B",
                                mesh.vertices.len(),
                                bytes(&legacy_buffers),
                                fills_only,
                                bytes(&split_buffers)
                            );
                        }
                        if split_buffers.has_compact_strokes() {
                            stroke_tiles += 1;
                        }
                        legacy_bytes += legacy_buffers.vertex_capacity_bytes() + legacy_buffers.index_capacity_bytes();
                        split_bytes += split_buffers.vertex_capacity_bytes()
                            + split_buffers.index_capacity_bytes()
                            + split_buffers.compact_bytes();
                        let (lon, lat) = crate::geo::tile_xy_to_lon_lat(
                            f64::from(tile.x) + 0.5,
                            f64::from(tile.y) + 0.5,
                            f64::from(tile.z),
                        );
                        let camera = crate::render::Camera {
                            center_lon: lon,
                            center_lat: lat,
                            zoom: f64::from(tile.z) + 0.3,
                            pitch: 0.0,
                            bearing: 0.0,
                            viewport_width_px: size as f32,
                            viewport_height_px: size as f32,
                            fov_y: crate::render::Camera::DEFAULT_FOV_Y,
                        };
                        let frame = crate::render::globe::globe_frame(&camera);
                        let sun = crate::render::atmosphere::Sun::Map { lon_deg: lon + 40.0, lat_deg: 10.0 }
                            .direction_sphere(&glam::DMat4::IDENTITY);
                        let tile_world_size = crate::render::WORLD_SIZE / 2f32.powi(i32::from(tile.z));
                        let matrix = crate::render::combined_tile_matrix(&camera, tile, extent as f32).to_cols_array_2d();
                        // A stand-in's clip: the one child quadrant it covers.
                        let quadrant = crate::geo::TileId { z: tile.z + 1, x: tile.x * 2 + 1, y: tile.y * 2 };
                        for &transition in transitions {
                            let params = GlobeParams {
                                globe_matrix: frame.globe_view_proj.as_mat4().to_cols_array_2d(),
                                tile_mercator_coords: crate::render::globe::tile_mercator_coords(tile),
                                clipping_plane: frame.clipping_plane.map(|c| c as f32),
                                transition,
                                lyon_to_tile_unit: 1.0 / (crate::render::mesh::COORD_SCALE * extent as f32),
                                tile_units_per_px: 1.0 / (tile_world_size * 2f32.powf(camera.zoom as f32)),
                                sun_direction: [sun.x as f32, sun.y as f32, sun.z as f32],
                                night_brightness: 0.25,
                            };
                            for clip in [TileClip::WHOLE, TileClip::for_descendant(tile, quadrant)] {
                                let (_model, group) =
                                    pipeline.create_model_bind_group_clipped(&ctx, matrix, 0.37, 0.5, params, clip);
                                let clear = wgpu::Color { r: 0.1, g: 0.2, b: 0.3, a: 1.0 };
                                let legacy = render_tile_draws(
                                    &ctx,
                                    &pipeline,
                                    &camera,
                                    size,
                                    clear,
                                    &[TileDraw::Legacy(&legacy_buffers, &group)],
                                )
                                .await;
                                let split = render_tile_draws(
                                    &ctx,
                                    &pipeline,
                                    &camera,
                                    size,
                                    clear,
                                    &[TileDraw::Legacy(&split_buffers, &group)],
                                )
                                .await;
                                let differing = legacy.chunks(4).zip(split.chunks(4)).filter(|(a, b)| a != b).count();
                                let delta = legacy.iter().zip(&split).map(|(a, b)| a.abs_diff(*b)).max().unwrap_or(0);
                                compared += 1;
                                painted = painted.max(legacy.chunks(4).filter(|p| p != &[26, 51, 77, 255]).count());
                                assert_eq!(
                                    differing, 0,
                                    "{name} z{display_zoom} tile {tile:?} transition {transition} clip {clip:?}: \
                                     {differing} px differ by up to {delta}"
                                );
                                if let Some(sabotaged) = &sabotaged {
                                    let bad = render_tile_draws(
                                        &ctx,
                                        &pipeline,
                                        &camera,
                                        size,
                                        clear,
                                        &[TileDraw::Legacy(sabotaged, &group)],
                                    )
                                    .await;
                                    sabotaged_frames += 1;
                                    sabotaged_px += legacy.chunks(4).zip(bad.chunks(4)).filter(|(a, b)| a != b).count();
                                }
                            }
                        }
                    }
                }
            }
            assert!(split_tiles >= 20, "the fixture must exercise the split ({split_tiles} tiles)");
            assert!(stroke_tiles >= 16, "the fixture must exercise the stroke body ({stroke_tiles} tiles)");
            assert!(painted > 10_000, "the fixture must paint ({painted} px)");
            assert!(
                sabotaged_px > 0,
                "a 0.1 % stroke-width error must show: {sabotaged_frames} sabotaged frames, 0 px differ -- \
                 the gate cannot see the stroke body"
            );
            assert!(max_runs <= crate::render::compact::MAX_SPLIT_RUNS);
            eprintln!(
                "split basemap: {compared} frames compared over {split_tiles} split tiles ({stroke_tiles} with \
                 compact strokes), all identical; stroke vertices compact {stroke_vertices}, still 88-byte \
                 {legacy_stroke_vertices}; max runs {max_runs}; device bytes {legacy_bytes} -> {split_bytes} \
                 ({:.1}%); sabotage: {sabotaged_px} px over {sabotaged_frames} frames",
                100.0 * split_bytes as f64 / legacy_bytes as f64
            );
        });
    }

    /// The stroke body's paint-only rewrite: a line recoloured, rewidened and
    /// given a casing gap, plus a new fill colour that shifts every stroke
    /// entry's texel offset, lands in place and draws the 88-byte frame.
    #[test]
    fn a_split_tiles_stroke_paint_rewrite_lands_in_place_and_matches_the_88_byte_tile() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 128u32;
            let line = |color: Rgba, width: f32, gap: f32| crate::style::PaintOp::Line {
                metrics: false,
                color_from: color,
                color_to: color,
                width_from: width,
                width_to: width * 1.5,
                gap_from: gap,
                gap_to: gap,
                cap: crate::style::LineCap::Round,
                join: crate::style::LineJoin::Round,
                dash: crate::style::DashPattern::none(),
                geometry: std::sync::Arc::new(geo_types::Geometry::LineString(geo_types::LineString::from(vec![
                    (200, 300),
                    (2000, 3600),
                    (3900, 900),
                ]))),
            };
            let mut plan = compact_test_fill_plan();
            plan.ops.push(line(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }, 6.0, 0.0));
            let before = crate::render::tessellate_plan(&plan, 4096, crate::render::mesh::COORD_SCALE, crate::render::subdivide::Subdivision::NONE);
            let n = plan.ops.len();
            plan.ops[n - 1] = line(Rgba { r: 0.9, g: 0.9, b: 0.2, a: 0.8 }, 9.0, 2.0);
            if let crate::style::PaintOp::Fill { color_from, .. } = &mut plan.ops[1] {
                *color_from = Rgba { r: 0.3, g: 0.3, b: 0.3, a: 1.0 };
            }
            let after = crate::render::tessellate_plan(&plan, 4096, crate::render::mesh::COORD_SCALE, crate::render::subdivide::Subdivision::NONE);
            let split = pipeline.install_split(
                &ctx,
                None,
                crate::render::compact::split_tile_mesh_with(&before, true).expect("splits"),
            );
            assert!(split.has_compact_strokes());
            assert!(pipeline.write_vertex_buffer(&ctx, &split, &after), "same topology: rewritten in place");
            let legacy = pipeline.create_tile_buffers(&ctx, &after);
            let camera = crate::render::Camera {
                center_lon: 0.0,
                center_lat: 0.0,
                zoom: 0.4,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: size as f32,
                viewport_height_px: size as f32,
                fov_y: crate::render::Camera::DEFAULT_FOV_Y,
            };
            let tile = crate::geo::TileId { z: 0, x: 0, y: 0 };
            let matrix = crate::render::combined_tile_matrix(&camera, tile, 4096.0).to_cols_array_2d();
            let (_m, group) = pipeline.create_model_bind_group(&ctx, matrix, 0.4, 0.5, GlobeParams::FLAT);
            let a = render_tile_draws(&ctx, &pipeline, &camera, size, wgpu::Color::BLACK, &[TileDraw::Legacy(&legacy, &group)]).await;
            let b = render_tile_draws(&ctx, &pipeline, &camera, size, wgpu::Color::BLACK, &[TileDraw::Legacy(&split, &group)]).await;
            let old = pipeline.create_tile_buffers(&ctx, &before);
            let o = render_tile_draws(&ctx, &pipeline, &camera, size, wgpu::Color::BLACK, &[TileDraw::Legacy(&old, &group)]).await;
            let moved = a.chunks(4).zip(o.chunks(4)).filter(|(x, y)| x != y).count();
            assert!(moved > 200, "the new paint must show: {moved} px moved");
            assert_eq!(a, b);
        });
    }

    /// The mask's index lists are the only bytes a masked install adds: the
    /// same tile installed with and without ranks differs by exactly
    /// `MaskIndexLists::bytes`, and by nothing with no ranks. Both the split
    /// and the unsplit (`GPUI_MAP_BASEMAP_COMPACT=0`) layouts are covered by
    /// driving `install_split` and `install_mesh` directly.
    #[test]
    fn a_masked_basemap_tile_counts_its_mask_bytes() {
        use crate::overlay::dotgrid::mask_class::{MaskRank, MaskRanks};
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let plan = compact_test_fill_plan();
            let (mesh, spans) = crate::render::tessellate_plan_with_ranges(
                &plan,
                4096,
                crate::render::mesh::COORD_SCALE,
                crate::render::subdivide::Subdivision::NONE,
            );
            let ranks = MaskRanks { classified: true, ops: vec![(0, MaskRank::Water), (2, MaskRank::Building)] };
            let plain = pipeline.install_basemap_mesh(&ctx, None, &mesh);
            let masked = pipeline.install_basemap_mesh_masked(&ctx, &mesh, &spans, &ranks);
            let lists = if crate::render::compact::basemap_compact_enabled() {
                crate::render::compact::split_tile_mesh_masked(&mesh, &spans, &ranks).expect("splits").1
            } else {
                crate::render::compact::mask_lists_unsplit(&mesh, &spans, &ranks)
            };
            assert!(!lists.is_empty());
            assert_eq!(masked.is_split(), plain.is_split());
            assert_eq!(masked.device_bytes(), plain.device_bytes() + lists.bytes());
            assert_eq!(masked.mask_bytes(), lists.bytes());
            let unranked = pipeline.install_basemap_mesh_masked(&ctx, &mesh, &spans, &MaskRanks::default());
            assert_eq!(unranked.device_bytes(), plain.device_bytes());
            assert!(unranked.mask.is_none());
            // Unsplit, the lists are over the whole mesh.
            let whole = pipeline.install_mesh_masked(&ctx, &mesh, &spans, &ranks);
            let unsplit_lists = crate::render::compact::mask_lists_unsplit(&mesh, &spans, &ranks);
            assert_eq!(whole.device_bytes(), pipeline.install_mesh(&ctx, None, &mesh).device_bytes() + unsplit_lists.bytes());
            assert!(whole.mask.as_ref().is_some_and(|m| m.compact.is_none() && m.legacy.is_some()));
            // A paint-only rewrite leaves the mask where it was.
            assert!(pipeline.write_vertex_buffer(&ctx, &masked, &mesh));
            assert_eq!(masked.mask_bytes(), lists.bytes());
        });
    }

    /// Ranks that do not align with the mesh's spans install no mask
    /// rather than index `spans` out of bounds; debug builds assert.
    #[test]
    #[cfg_attr(debug_assertions, should_panic(expected = "mask ranks do not align"))]
    fn misaligned_ranks_install_the_tile_without_a_mask() {
        use crate::overlay::dotgrid::mask_class::{MaskRank, MaskRanks};
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let plan = compact_test_fill_plan();
            let (mesh, spans) = crate::render::tessellate_plan_with_ranges(
                &plan,
                4096,
                crate::render::mesh::COORD_SCALE,
                crate::render::subdivide::Subdivision::NONE,
            );
            let past = MaskRanks { classified: true, ops: vec![(0, MaskRank::Water), (spans.len() as u32, MaskRank::Building)] };
            let buffers = pipeline.install_basemap_mesh_masked(&ctx, &mesh, &spans, &past);
            assert!(buffers.mask.is_none());
            assert_eq!(buffers.device_bytes(), pipeline.install_basemap_mesh(&ctx, None, &mesh).device_bytes());
        });
    }

    /// The mask is unused until Task 5 draws it: a masked tile draws the
    /// unmasked tile's frame byte for byte, split and unsplit.
    #[test]
    fn a_masked_basemap_tile_draws_the_unmasked_frame() {
        use crate::overlay::dotgrid::mask_class::{MaskRank, MaskRanks};
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 128u32;
            let plan = compact_test_fill_plan();
            let (mesh, spans) = crate::render::tessellate_plan_with_ranges(
                &plan,
                4096,
                crate::render::mesh::COORD_SCALE,
                crate::render::subdivide::Subdivision::NONE,
            );
            let ranks = MaskRanks { classified: true, ops: vec![(0, MaskRank::Green), (1, MaskRank::Water), (2, MaskRank::Building)] };
            let camera = crate::render::Camera {
                center_lon: 0.0,
                center_lat: 0.0,
                zoom: 0.4,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: size as f32,
                viewport_height_px: size as f32,
                fov_y: crate::render::Camera::DEFAULT_FOV_Y,
            };
            let tile = crate::geo::TileId { z: 0, x: 0, y: 0 };
            let matrix = crate::render::combined_tile_matrix(&camera, tile, 4096.0).to_cols_array_2d();
            let (_m, group) = pipeline.create_model_bind_group(&ctx, matrix, 0.4, 0.5, GlobeParams::FLAT);
            let plain = pipeline.install_basemap_mesh(&ctx, None, &mesh);
            let masked = pipeline.install_basemap_mesh_masked(&ctx, &mesh, &spans, &ranks);
            let whole = pipeline.install_mesh_masked(&ctx, &mesh, &spans, &ranks);
            assert!(masked.mask.is_some() && whole.mask.is_some());
            let a = render_tile_draws(&ctx, &pipeline, &camera, size, wgpu::Color::BLACK, &[TileDraw::Legacy(&plain, &group)]).await;
            let b = render_tile_draws(&ctx, &pipeline, &camera, size, wgpu::Color::BLACK, &[TileDraw::Legacy(&masked, &group)]).await;
            let c = render_tile_draws(&ctx, &pipeline, &camera, size, wgpu::Color::BLACK, &[TileDraw::Legacy(&whole, &group)]).await;
            assert!(a.chunks(4).filter(|p| p != &[0, 0, 0, 255]).count() > 1000, "the fixture paints");
            assert_eq!(a, b);
            assert_eq!(a, c);
        });
    }

    /// A paint-only zoom crossing on a split tile: `write_vertex_buffer`
    /// re-splits and rewrites the compact vertices and the paint table in
    /// place, and the frame is the 88-byte tile's with the new paint.
    #[test]
    fn a_split_tiles_paint_rewrite_lands_in_place_and_matches_the_88_byte_tile() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 128u32;
            let mut plan = compact_test_fill_plan();
            let before = crate::render::tessellate_plan(&plan, 4096, crate::render::mesh::COORD_SCALE, crate::render::subdivide::Subdivision::NONE);
            if let crate::style::PaintOp::Fill { color_from, color_to, .. } = &mut plan.ops[0] {
                *color_from = Rgba { r: 0.7, g: 0.1, b: 0.9, a: 1.0 };
                *color_to = Rgba { r: 0.1, g: 0.9, b: 0.2, a: 0.8 };
            }
            let after = crate::render::tessellate_plan(&plan, 4096, crate::render::mesh::COORD_SCALE, crate::render::subdivide::Subdivision::NONE);
            let split = pipeline.create_basemap_tile_buffers(&ctx, &before);
            assert!(split.is_split());
            assert!(pipeline.write_vertex_buffer(&ctx, &split, &after), "same topology: rewritten in place");
            let legacy = pipeline.create_tile_buffers(&ctx, &after);
            let camera = crate::render::Camera {
                center_lon: 0.0,
                center_lat: 0.0,
                zoom: 0.4,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: size as f32,
                viewport_height_px: size as f32,
                fov_y: crate::render::Camera::DEFAULT_FOV_Y,
            };
            let tile = crate::geo::TileId { z: 0, x: 0, y: 0 };
            let matrix = crate::render::combined_tile_matrix(&camera, tile, 4096.0).to_cols_array_2d();
            let (_m, group) = pipeline.create_model_bind_group(&ctx, matrix, 0.4, 0.5, GlobeParams::FLAT);
            let a = render_tile_draws(&ctx, &pipeline, &camera, size, wgpu::Color::BLACK, &[TileDraw::Legacy(&legacy, &group)]).await;
            let b = render_tile_draws(&ctx, &pipeline, &camera, size, wgpu::Color::BLACK, &[TileDraw::Legacy(&split, &group)]).await;
            assert!(a.chunks(4).filter(|p| p != &[0, 0, 0, 255]).count() > 1000, "the fixture paints");
            assert_eq!(a, b);
            // A different topology cannot be written in place.
            let lines_only = TileMesh { vertices: before.vertices[..3].to_vec(), indices: vec![0, 1, 2] };
            assert!(!pipeline.write_vertex_buffer(&ctx, &split, &lines_only));
        });
    }

    /// **THE TASK 5 GATE.** `vs_compact` duplicates `vertex_main`'s fill
    /// path; this is the only thing keeping the two from drifting. Flat
    /// (transition 0), mid-blend (0.6) and full globe (1.0), with a sun, a
    /// translucent op and a `blend_t` that mixes a colour pair: the two
    /// frames must be byte-identical.
    #[test]
    #[cfg_attr(not(target_os = "macos"), ignore = "rasterizer precision differs from Metal's (fails on lavapipe)")]
    fn a_compact_fill_is_pixel_identical_to_the_legacy_fill_flat_and_on_the_globe() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 512u32;
            let extent = crate::mvt::MVT_EXTENT;
            let tile = crate::geo::TileId { z: 3, x: 2, y: 3 };
            let plan = compact_test_fill_plan();
            let (mesh, ranges) = crate::render::tessellate_plan_with_ranges(
                &plan,
                extent,
                crate::render::mesh::COORD_SCALE,
                crate::overlay::tile_plan::overlay_tile_subdivision(tile),
            );
            // The compact vertex snaps to integer MVT units. The rings' own
            // vertices already are; the subdivision's diagonal cuts are not
            // (they move by <= 0.5 unit, 1/16 px at the tile's own zoom).
            // The SHADER identity is proven over the same snapped geometry;
            // the snap itself is checked against the raw mesh below.
            let fractional = mesh.vertices.iter().filter(|v| v.position.iter().any(|c| (c / 1000.0).fract() != 0.0)).count();
            assert!(fractional > 0, "the subdivision must have cut the fixture's triangles");
            let mut snapped = mesh.clone();
            for v in &mut snapped.vertices {
                v.position = v.position.map(|c| (c / 1000.0).round() * 1000.0);
            }
            assert!(mesh.vertices.len() > 12, "the subdivision must have cut the rings: {}", mesh.vertices.len());
            let paints: Vec<(Rgba, Rgba)> = plan
                .ops
                .iter()
                .map(|op| match op {
                    crate::style::PaintOp::Fill { color_from, color_to, .. } => (*color_from, *color_to),
                    _ => unreachable!(),
                })
                .collect();
            let op_paint: Vec<u16> = (0..paints.len() as u16).collect();
            let compact = crate::render::compact::compact_from_tile_mesh(&mesh, &ranges, &op_paint, &[]).unwrap();
            let legacy_buffers = pipeline.create_tile_buffers(&ctx, &snapped);
            let raw_buffers = pipeline.create_tile_buffers(&ctx, &mesh);
            let compact_buffers = pipeline.create_compact_buffers(&ctx, &compact);
            let table = pipeline.create_paint_table(&ctx, &crate::render::compact::paint_table(&paints));
            let camera = crate::render::Camera {
                center_lon: -62.0,
                center_lat: 24.0,
                zoom: 3.4,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: size as f32,
                viewport_height_px: size as f32,
                fov_y: crate::render::Camera::DEFAULT_FOV_Y,
            };
            let frame = crate::render::globe::globe_frame(&camera);
            let sun = crate::render::atmosphere::Sun::Map { lon_deg: -150.0, lat_deg: 10.0 }
                .direction_sphere(&glam::DMat4::IDENTITY);
            let tile_world_size = crate::render::WORLD_SIZE / 2f32.powi(i32::from(tile.z));
            let matrix = crate::render::combined_tile_matrix(&camera, tile, extent as f32).to_cols_array_2d();
            let mut globe_frames = 0;
            for transition in [0.0_f32, 0.6, 1.0] {
                let params = GlobeParams {
                    globe_matrix: frame.globe_view_proj.as_mat4().to_cols_array_2d(),
                    tile_mercator_coords: crate::render::globe::tile_mercator_coords(tile),
                    clipping_plane: frame.clipping_plane.map(|c| c as f32),
                    transition,
                    lyon_to_tile_unit: 1.0 / (crate::render::mesh::COORD_SCALE * extent as f32),
                    tile_units_per_px: 1.0 / (tile_world_size * 2f32.powf(camera.zoom as f32)),
                    sun_direction: [sun.x as f32, sun.y as f32, sun.z as f32],
                    night_brightness: 0.25,
                };
                let blend_t = 0.37;
                let (_legacy_model, legacy_group) =
                    pipeline.create_model_bind_group(&ctx, matrix, blend_t, 0.5, params);
                let compact_model =
                    pipeline.create_model_buffer_clipped(&ctx, matrix, blend_t, 0.5, params, TileClip::WHOLE);
                let compact_group = pipeline.create_compact_model_bind_group(&ctx, &compact_model, &table);
                let legacy = render_tile_draws(
                    &ctx,
                    &pipeline,
                    &camera,
                    size,
                    wgpu::Color::TRANSPARENT,
                    &[TileDraw::Legacy(&legacy_buffers, &legacy_group)],
                )
                .await;
                let packed = render_tile_draws(
                    &ctx,
                    &pipeline,
                    &camera,
                    size,
                    wgpu::Color::TRANSPARENT,
                    &[TileDraw::CompactFill(&compact_buffers, &compact_group)],
                )
                .await;
                let raw = render_tile_draws(
                    &ctx,
                    &pipeline,
                    &camera,
                    size,
                    wgpu::Color::TRANSPARENT,
                    &[TileDraw::Legacy(&raw_buffers, &legacy_group)],
                )
                .await;
                let painted = legacy.chunks(4).filter(|p| p[3] > 0).count();
                assert!(painted > 10_000, "transition {transition}: the fixture must paint ({painted} px)");
                let differing = legacy.chunks(4).zip(packed.chunks(4)).filter(|(a, b)| a != b).count();
                assert_eq!(differing, 0, "transition {transition}: compact path must be bit-identical");
                // The snap against the REAL (unsnapped) mesh: interior
                // vertices only move, so coverage is unchanged and a flat
                // frame is identical; on the globe the per-vertex sphere
                // position and sun term shift a handful of pixels by a
                // rounding step (measured: 0 / 5 / 8 px of ~100k painted).
                let snap_moved = raw.chunks(4).zip(packed.chunks(4)).filter(|(a, b)| a != b).count();
                let snap_delta = raw.iter().zip(&packed).map(|(a, b)| a.abs_diff(*b)).max().unwrap_or(0);
                if transition == 0.0 {
                    assert_eq!(snap_moved, 0, "a flat frame of the real mesh must be identical after the snap");
                } else {
                    assert!(snap_moved <= 64 && snap_delta <= 2, "the snap moved {snap_moved} px by up to {snap_delta}");
                }
                globe_frames += usize::from(transition > 0.0);
            }
            assert_eq!(globe_frames, 2);
        });
    }

    /// The hairline: `compact_outline` draws A2's line list over the fill's
    /// own vertices, one pixel wide, in the colour the bound table gives
    /// the paint index.
    #[test]
    fn the_compact_outline_draws_one_pixel_lines_over_the_fill_vertices() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 512u32;
            let tile = crate::geo::TileId { z: 0, x: 0, y: 0 };
            let square = [[1024i32, 1024], [3072, 1024], [3072, 3072], [1024, 3072]];
            let mesh = TileMesh {
                vertices: square
                    .iter()
                    .map(|p| Vertex { position: [p[0] as f32 * 1000.0, p[1] as f32 * 1000.0], ..bytemuck::Zeroable::zeroed() })
                    .collect(),
                indices: vec![0, 1, 2, 0, 2, 3],
            };
            let c = crate::render::compact::compact_from_tile_mesh(
                &mesh,
                &[crate::render::mesh::OpSpan { vertices: 0..4, indices: 0..6 }],
                &[0],
                &[0, 1, 1, 2, 2, 3, 3, 0],
            )
            .unwrap();
            let black = Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 };
            let buffers = pipeline.create_compact_buffers(&ctx, &c);
            assert!(buffers.has_outline());
            let table = pipeline.create_paint_table(&ctx, &crate::render::compact::paint_table(&[(black, black)]));
            let camera = crate::render::Camera {
                center_lon: 0.0,
                center_lat: 0.0,
                zoom: 0.0,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: size as f32,
                viewport_height_px: size as f32,
                fov_y: crate::render::Camera::DEFAULT_FOV_Y,
            };
            let matrix = crate::render::combined_tile_matrix(&camera, tile, crate::mvt::MVT_EXTENT as f32)
                .to_cols_array_2d();
            let model = pipeline.create_model_buffer_clipped(&ctx, matrix, 0.0, 0.0, GlobeParams::FLAT, TileClip::WHOLE);
            let group = pipeline.create_compact_model_bind_group(&ctx, &model, &table);
            let frame = render_tile_draws(
                &ctx,
                &pipeline,
                &camera,
                size,
                wgpu::Color::WHITE,
                &[TileDraw::CompactOutline(&buffers, &group)],
            )
            .await;
            let bytes_per_row = (size * 4).next_multiple_of(256) as usize;
            let mut dark = 0;
            for y in 0..size as usize {
                for x in 0..size as usize {
                    if frame[y * bytes_per_row + x * 4] < 64 {
                        dark += 1;
                    }
                }
            }
            // Four 256-px edges, 1 px wide.
            assert!((1000..=1100).contains(&dark), "{dark}");
        });
    }

    #[test]
    fn the_compact_shader_adds_no_varyings() {
        let src = include_str!("shader.wgsl");
        assert_eq!(vertex_output_locations(src).len(), 12, "11, plus Task 5's line_progress");
        assert!(src.contains("fn vs_compact(input: CompactInput) -> VertexOutput"));
    }

    fn make_color_texture(ctx: &GpuContext, size: u32) -> wgpu::Texture {
        ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("test-color"),
            size: wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        })
    }

    /// `write_vertex_buffer`'s whole reason to exist: a tile whose vertex
    /// buffer is rewritten in place (as `MapView::plan_for` does on a
    /// paint-only zoom crossing, via `render::rewrite_paint_attributes`)
    /// must render with the NEW color on the next frame, using the SAME
    /// `TileBuffers` (same vertex/index buffers, same `index_count`) --
    /// not a freshly recreated one.
    #[test]
    fn write_vertex_buffer_updates_the_rendered_color_without_recreating_the_buffer() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);

            let size = 64u32;
            let identity = glam::Mat4::IDENTITY.to_cols_array_2d();
            let camera_bind_group = pipeline.test_camera_bind_group(&ctx, identity, 0.0);
            let model_bind_group = pipeline.test_model_bind_group(&ctx, identity, 1.0, 0.0);
            let tile_buffers = pipeline.create_tile_buffers(&ctx, &triangle_mesh([0.0, 1.0, 0.0, 1.0]));

            let render_and_read_center_pixel = |ctx: &GpuContext, buffers: &TileBuffers| {
                let color_texture = make_color_texture(ctx, size);
                let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor::default());
                let depth_view = pipeline.create_depth_view(ctx, size, size);
                let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
                pipeline.render(
                    &mut encoder,
                    &color_view,
                    &depth_view,
                    wgpu::Color::TRANSPARENT,
                    &camera_bind_group,
                    &[(buffers, &model_bind_group)],
                    None,
                );
                (color_texture, encoder)
            };

            let (color_texture, encoder) = render_and_read_center_pixel(&ctx, &tile_buffers);
            let data = read_all_pixels(&ctx, &color_texture, encoder, size).await;
            let bytes_per_row = (size * 4).next_multiple_of(256);
            let center_pixel_offset = (size / 2 * bytes_per_row + (size / 2) * 4) as usize;
            let before = &data[center_pixel_offset..center_pixel_offset + 4];
            assert!(before[1] > 200, "expected the initial green triangle, got {before:?}");
            assert!(before[0] < 50, "expected the initial green triangle, got {before:?}");

            // Rewrite the SAME buffer's vertex colors in place -- same
            // vertex count/positions, only `color`/`color_to` differ, as
            // `rewrite_paint_attributes` would produce for a real paint-only
            // crossing.
            pipeline.write_vertex_buffer(&ctx, &tile_buffers, &triangle_mesh([1.0, 0.0, 0.0, 1.0]));

            let (color_texture, encoder) = render_and_read_center_pixel(&ctx, &tile_buffers);
            let data = read_all_pixels(&ctx, &color_texture, encoder, size).await;
            let after = &data[center_pixel_offset..center_pixel_offset + 4];
            assert!(after[0] > 200, "expected the rewritten red triangle, got {after:?}");
            assert!(after[1] < 50, "expected the rewritten red triangle, got {after:?}");
        });
    }

    /// Regression test for a real Task 6 review finding: the shader's line
    /// extrusion used `length(model.matrix[0].xy)` (the tile model matrix's
    /// own scale) as its world-units-per-pixel factor. That scale shrinks
    /// toward zero as tiles subdivide at higher zoom (~1.5e-5 at zoom 2 for
    /// a 4096-extent tile, versus the correct ~0.25 world units/px at that
    /// zoom), collapsing every line's extrusion to an imperceptible fraction
    /// of a pixel — lines rendered, but at zero visible width. The fix
    /// derives the extrusion from `camera.zoom` directly
    /// (`1.0 / pow(2.0, camera.zoom)`), matching the camera's own
    /// screen_pixels_per_world_unit = 2^zoom convention (`render::camera`'s
    /// module docs) — independent of any particular tile's model-matrix
    /// scale, so line width stays constant across zoom instead of shrinking
    /// with it.
    ///
    /// This exercises the real production path end to end (`tessellate_plan`
    /// building the mesh, the real `combined_tile_matrix`, the real
    /// `Camera::view_projection`), then reads back the whole framebuffer and
    /// counts colored pixels, rather than asserting on a single pixel — the
    /// bug was "renders at ~0 width," which a single-pixel sample could
    /// still accidentally land inside or just as easily miss.
    #[test]
    fn stroke_extrusion_scales_with_camera_zoom_not_tile_model_scale() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);

            let size = 512u32;
            let color_texture = make_color_texture(&ctx, size);
            let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor::default());
            let depth_view = pipeline.create_depth_view(&ctx, size, size);

            // z=2 tile whose corner sits exactly at the world center (see
            // `render::camera`'s own tests for this same fact) — putting the
            // line's start there keeps the math easy to reason about.
            let tile = crate::geo::TileId { z: 2, x: 2, y: 2 };
            let extent = 4096u32;

            let zoom = 2.0f64;
            let camera = crate::render::Camera {
                center_lon: 0.0,
                center_lat: 0.0,
                zoom,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: size as f32,
                viewport_height_px: size as f32,
                fov_y: crate::render::Camera::DEFAULT_FOV_Y,
            };
            let view_proj = camera.view_projection().to_cols_array_2d();
            // `model.matrix` now carries the camera baked in (see shader.wgsl's
            // `vs_main`), so the test's model bind group must be built from
            // `combined_tile_matrix`, matching what production `paint()` writes,
            // not the bare tile-local `tile_model_matrix`.
            let model = crate::render::combined_tile_matrix(&camera, tile, extent as f32).to_cols_array_2d();
            let camera_bind_group = pipeline.test_camera_bind_group(&ctx, view_proj, zoom as f32);
            let model_bind_group = pipeline.test_model_bind_group(&ctx, model, 1.0, 0.0);

            // A horizontal line spanning the tile's full width, built via the
            // real tessellation path so this test exercises the exact shader
            // input the bug affected (not a hand-built mesh).
            let line = geo_types::Geometry::LineString(geo_types::LineString::from(vec![
                (0i32, 0i32),
                (extent as i32, 0i32),
            ]));
            let plan = crate::style::PaintPlan {
                ops: vec![crate::style::PaintOp::Line {
                    metrics: false,
                    color_from: crate::style::Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
                    color_to: crate::style::Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
                    width_from: 2.0,
                    width_to: 2.0,
                    gap_from: 0.0,
                    gap_to: 0.0,
                    cap: crate::style::LineCap::Butt,
                    join: crate::style::LineJoin::Miter,
                    dash: crate::style::DashPattern::none(),
                    geometry: Arc::new(line),
                }],
            };
            let mesh = crate::render::tessellate_plan(&plan, extent, crate::render::mesh::COORD_SCALE, crate::render::subdivide::Subdivision::NONE);
            assert!(!mesh.indices.is_empty(), "line tessellation produced no geometry");
            let tile_buffers = pipeline.create_tile_buffers(&ctx, &mesh);

            let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            pipeline.render(
                &mut encoder,
                &color_view,
                &depth_view,
                wgpu::Color::TRANSPARENT,
                &camera_bind_group,
                &[(&tile_buffers, &model_bind_group)],
                None,
            );

            let data = read_all_pixels(&ctx, &color_texture, encoder, size).await;
            let bytes_per_row = (size * 4).next_multiple_of(256);
            let mut colored_pixels: u32 = 0;
            for row in 0..size {
                for col in 0..size {
                    let offset = (row * bytes_per_row + col * 4) as usize;
                    if data[offset + 3] > 20 {
                        colored_pixels += 1;
                    }
                }
            }

            // At zoom 2, screen_pixels_per_world_unit = 2^2 = 4, the line
            // spans the tile's full 64-world-unit width (WORLD_SIZE / 4
            // tiles-across at z=2), and `half_width_px = 1.0` in the shader
            // means a 2px-wide line — so ~64*4*2 = 512 colored pixels,
            // modulo antialiasing at the edges/caps. With the old
            // model-matrix-scale formula the extrusion was ~16,000x smaller
            // than correct, which rounds to zero coverage; this assertion
            // would fail under that formula.
            assert!(
                (200..40_000).contains(&colored_pixels),
                "expected a visibly-wide line (~512px of coverage), got {colored_pixels} colored pixels \
                 — the line-extrusion fix may have regressed"
            );
        });
    }

    /// The engine half of the line trim, on a real device: a horizontal
    /// line built WITH metrics, drawn under `line_fx`. `LINE_FX_NONE` draws
    /// it whole; a trim of `0..0.5` keeps only its first half (every lit
    /// column left of the midpoint); a line WITHOUT metrics ignores the
    /// same trim; and an opacity of 0.5 halves the alpha. The capture-path
    /// pixel test of the trim is Task 7's.
    #[test]
    fn line_fx_trims_a_metrics_line_and_fades_it() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 512u32;
            let tile = crate::geo::TileId { z: 2, x: 2, y: 2 };
            let extent = 4096u32;
            let camera = crate::render::Camera {
                center_lon: 0.0,
                center_lat: 0.0,
                zoom: 2.0,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: size as f32,
                viewport_height_px: size as f32,
                fov_y: crate::render::Camera::DEFAULT_FOV_Y,
            };
            let view_proj = camera.view_projection().to_cols_array_2d();
            let model = crate::render::combined_tile_matrix(&camera, tile, extent as f32).to_cols_array_2d();
            let camera_bind_group = pipeline.test_camera_bind_group(&ctx, view_proj, 2.0);
            // Lit columns (min, max, count) and the brightest alpha.
            // `how` is the write: `FX_PARTIAL` is `write_model_line_fx`;
            // `FX_RAW` writes `line_fx` straight into the buffer, bypassing
            // `normalise_line_fx`, to exercise the shader's own guard;
            // `FX_PER_FRAME` is the overlay host's per-frame path (T5
            // review F3) -- two frames of `write_model_uniform_fx` over a
            // placeholder buffer; `FX_THEN_PLAIN` is a trim followed by a
            // plain whole-uniform rewrite, the ordering hazard F3 removed.
            const FX_PARTIAL: u8 = 0;
            const FX_RAW: u8 = 1;
            const FX_PER_FRAME: u8 = 2;
            const FX_THEN_PLAIN: u8 = 3;
            let draw_with = |metrics: bool, line_fx: [f32; 4], how: u8| {
                let ctx = &ctx;
                let pipeline = &pipeline;
                let camera_bind_group = &camera_bind_group;
                async move {
                    let color_texture = make_color_texture(ctx, size);
                    let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor::default());
                    let depth_view = pipeline.create_depth_view(ctx, size, size);
                    let (model_buffer, model_bind_group) =
                        pipeline.create_model_bind_group(ctx, model, 1.0, 0.0, GlobeParams::FLAT);
                    match how {
                        FX_RAW => ctx.queue.write_buffer(&model_buffer, LINE_FX_OFFSET, bytemuck::cast_slice(&line_fx)),
                        FX_PER_FRAME => {
                            for _frame in 0..2 {
                                pipeline.write_model_uniform_fx(
                                    ctx,
                                    &model_buffer,
                                    model,
                                    1.0,
                                    0.0,
                                    GlobeParams::FLAT,
                                    TileClip::WHOLE,
                                    line_fx,
                                );
                            }
                        }
                        FX_THEN_PLAIN => {
                            pipeline.write_model_line_fx(ctx, &model_buffer, line_fx);
                            pipeline.write_model_uniform(ctx, &model_buffer, model, 1.0, 0.0, GlobeParams::FLAT);
                        }
                        _ => pipeline.write_model_line_fx(ctx, &model_buffer, line_fx),
                    }
                    // Half the tile's width: the tile is 512 px here, and
                    // the viewport shows only its first 256 from the centre.
                    let line = geo_types::Geometry::LineString(geo_types::LineString::from(vec![
                        (0i32, 0i32),
                        (extent as i32 / 2, 0i32),
                    ]));
                    let plan = crate::style::PaintPlan {
                        ops: vec![crate::style::PaintOp::Line {
                            color_from: crate::style::Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
                            color_to: crate::style::Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
                            width_from: 6.0,
                            width_to: 6.0,
                            gap_from: 0.0,
                            gap_to: 0.0,
                            // ROUND: the start cap's vertices all carry progress
                            // exactly 0, which is what an empty range must still cut.
                            cap: crate::style::LineCap::Round,
                            join: crate::style::LineJoin::Miter,
                            dash: crate::style::DashPattern::none(),
                            metrics,
                            geometry: Arc::new(line),
                        }],
                    };
                    let mesh = crate::render::tessellate_plan(
                        &plan,
                        extent,
                        crate::render::mesh::COORD_SCALE,
                        crate::render::subdivide::Subdivision::NONE,
                    );
                    let tile_buffers = pipeline.create_tile_buffers(ctx, &mesh);
                    let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
                    pipeline.render(
                        &mut encoder,
                        &color_view,
                        &depth_view,
                        wgpu::Color::TRANSPARENT,
                        camera_bind_group,
                        &[(&tile_buffers, &model_bind_group)],
                        None,
                    );
                    let data = read_all_pixels(ctx, &color_texture, encoder, size).await;
                    let bytes_per_row = (size * 4).next_multiple_of(256);
                    let (mut min_col, mut max_col, mut lit, mut max_alpha) = (u32::MAX, 0u32, 0u32, 0u8);
                    for row in 0..size {
                        for col in 0..size {
                            let a = data[(row * bytes_per_row + col * 4 + 3) as usize];
                            if a > 20 {
                                min_col = min_col.min(col);
                                max_col = max_col.max(col);
                                lit += 1;
                            }
                            max_alpha = max_alpha.max(a);
                        }
                    }
                    // Every byte of the frame, so equality is pixel identity.
                    let frame = {
                        use std::hash::{Hash, Hasher};
                        let mut h = std::collections::hash_map::DefaultHasher::new();
                        data.hash(&mut h);
                        h.finish()
                    };
                    (min_col, max_col, lit, max_alpha, frame)
                }
            };
            let draw = |metrics: bool, line_fx: [f32; 4]| draw_with(metrics, line_fx, FX_PARTIAL);
            let whole = draw(true, LINE_FX_NONE).await;
            let plain = draw(false, LINE_FX_NONE).await;
            assert_eq!(whole, plain, "LINE_FX_NONE draws a metrics line exactly as a plain one");
            let (min_col, max_col, lit, alpha, _) = whole;
            assert!(lit > 500 && alpha == 255, "the line drew: {whole:?}");
            let mid = (min_col + max_col) / 2;

            let half = draw(true, [0.0, 0.5, 1.0, 1.0]).await;
            assert_eq!(half.0, min_col, "the trim keeps the start: {half:?} vs {whole:?}");
            assert!(half.1 <= mid + 2 && half.1 + 2 >= mid, "the trim ends half-way: {half:?}, mid {mid}");
            assert!(half.2 * 2 <= lit + 20 && half.2 * 2 + 20 >= lit, "half the pixels: {half:?} vs {whole:?}");

            let tail = draw(true, [0.5, 1.0, 1.0, 1.0]).await;
            assert_eq!(tail.1, max_col, "a trim of 0.5..1 keeps the end");
            assert!(tail.0 + 2 >= mid, "and drops the start: {tail:?}, mid {mid}");

            let untrimmed = draw(false, [0.0, 0.5, 1.0, 1.0]).await;
            assert_eq!(untrimmed, whole, "a line without metrics ignores the trim");

            // F5: an empty kept range draws nothing -- not even the start cap.
            let none = draw(true, [0.0, 0.0, 1.0, 1.0]).await;
            assert_eq!(none.2, 0, "a kept range of 0..0 draws nothing: {none:?}");
            let inverted = draw(true, [0.6, 0.4, 1.0, 1.0]).await;
            assert_eq!(inverted.2, 0, "a kept range with y < x draws nothing: {inverted:?}");

            // F2: a full kept range with the trim ON is pixel-identical to
            // the whole line (host-normalised to off, and the shader never
            // cuts at 0 or 1 either).
            assert_eq!(normalise_line_fx([0.0, 1.0, 1.0, 1.0]), LINE_FX_NONE);
            let full = draw(true, [0.0, 1.0, 1.0, 1.0]).await;
            assert_eq!(full, whole, "[0, 1, 1, 1] draws the whole line");
            let full_raw = draw_with(true, [0.0, 1.0, 1.0, 1.0], FX_RAW).await;
            assert_eq!(full_raw, whole, "[0, 1, 1, 1] with the trim ON in the shader is pixel-identical too");
            let empty_raw = draw_with(true, [0.3, 0.3, 1.0, 1.0], FX_RAW).await;
            assert_eq!(empty_raw.2, 0, "the shader alone draws nothing for an empty range: {empty_raw:?}");

            let faded = draw(true, [0.0, 1.0, 0.5, 0.0]).await;
            assert!((126..=129).contains(&faded.3), "opacity 0.5 halves the alpha: {faded:?}");

            // T5 review F3: the per-frame whole-uniform write CARRIES the
            // trim, so it survives the uniform being rewritten every frame
            // (what `MapView::paint` and the overlay host do) ...
            let per_frame = draw_with(true, [0.0, 0.5, 1.0, 1.0], FX_PER_FRAME).await;
            assert_eq!(per_frame, half, "a trim written with the per-frame uniform survives a rewrite frame");
            let per_frame_fade = draw_with(true, [0.0, 1.0, 0.5, 0.0], FX_PER_FRAME).await;
            assert_eq!(per_frame_fade, faded, "and so does a fade");
            // ... where a partial write followed by a plain rewrite loses it.
            let lost = draw_with(true, [0.0, 0.5, 1.0, 1.0], FX_THEN_PLAIN).await;
            assert_eq!(lost, whole, "a plain rewrite resets line_fx, which is why the host never uses one");
        });
    }

    /// Regression test: before this fix, `shader.wgsl` hardcoded every stroke's
    /// extrusion to a constant 1px regardless of the style's declared width, so
    /// a wide casing line and a narrow fill line on top of it rendered
    /// identically -- the casing/fill layering added to basic_map.rs this
    /// session was invisible. Renders the same line twice, once at width=2.0 and
    /// once at width=12.0, and asserts the wider line covers meaningfully more
    /// pixels.
    #[test]
    fn wider_declared_line_width_covers_more_pixels() {
        #[allow(clippy::too_many_arguments)]
        async fn render_and_count(
            ctx: &GpuContext,
            pipeline: &TilePipeline,
            size: u32,
            view_proj: [[f32; 4]; 4],
            zoom: f32,
            model: [[f32; 4]; 4],
            extent: u32,
            width: f32,
        ) -> u32 {
            let color_texture = make_color_texture(ctx, size);
            let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor::default());
            let depth_view = pipeline.create_depth_view(ctx, size, size);
            let camera_bind_group = pipeline.test_camera_bind_group(ctx, view_proj, zoom);
            let model_bind_group = pipeline.test_model_bind_group(ctx, model, 1.0, 0.0);

            let line = geo_types::Geometry::LineString(geo_types::LineString::from(vec![
                (0i32, 0i32),
                (extent as i32, 0i32),
            ]));
            let plan = crate::style::PaintPlan {
                ops: vec![crate::style::PaintOp::Line {
                    metrics: false,
                    color_from: crate::style::Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
                    color_to: crate::style::Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
                    width_from: width,
                    width_to: width,
                    gap_from: 0.0,
                    gap_to: 0.0,
                    cap: crate::style::LineCap::Butt,
                    join: crate::style::LineJoin::Miter,
                    dash: crate::style::DashPattern::none(),
                    geometry: Arc::new(line),
                }],
            };
            let mesh = crate::render::tessellate_plan(&plan, extent, crate::render::mesh::COORD_SCALE, crate::render::subdivide::Subdivision::NONE);
            let tile_buffers = pipeline.create_tile_buffers(ctx, &mesh);

            let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            pipeline.render(
                &mut encoder,
                &color_view,
                &depth_view,
                wgpu::Color::TRANSPARENT,
                &camera_bind_group,
                &[(&tile_buffers, &model_bind_group)],
                None,
            );

            let data = read_all_pixels(ctx, &color_texture, encoder, size).await;
            let bytes_per_row = (size * 4).next_multiple_of(256);
            let mut colored_pixels: u32 = 0;
            for row in 0..size {
                for col in 0..size {
                    let offset = (row * bytes_per_row + col * 4) as usize;
                    if data[offset + 3] > 20 {
                        colored_pixels += 1;
                    }
                }
            }
            colored_pixels
        }

        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 256u32;

            let tile = crate::geo::TileId { z: 2, x: 2, y: 2 };
            let extent = 4096u32;
            let zoom = 2.0f64;
            let camera = crate::render::Camera {
                center_lon: 0.0,
                center_lat: 0.0,
                zoom,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: size as f32,
                viewport_height_px: size as f32,
                fov_y: crate::render::Camera::DEFAULT_FOV_Y,
            };
            let view_proj = camera.view_projection().to_cols_array_2d();
            // `model.matrix` now carries the camera baked in (see shader.wgsl's
            // `vs_main`), so the test's model bind group must be built from
            // `combined_tile_matrix`, matching what production `paint()` writes,
            // not the bare tile-local `tile_model_matrix`.
            let model = crate::render::combined_tile_matrix(&camera, tile, extent as f32).to_cols_array_2d();

            let narrow = render_and_count(&ctx, &pipeline, size, view_proj, zoom as f32, model, extent, 2.0).await;
            let wide = render_and_count(&ctx, &pipeline, size, view_proj, zoom as f32, model, extent, 12.0).await;

            assert!(
                wide > narrow * 2,
                "wide line ({wide}px) should cover meaningfully more pixels than narrow ({narrow}px) -- if not, half_width_px isn't reaching the shader"
            );
        });
    }

    /// Regression test for a real review finding on this task: an earlier
    /// version of the stroke-extrusion fix added the extrude offset directly
    /// in NDC axes (`input.normal * half_width_px * (2.0 / viewport_size) *
    /// clip.w`), which never accounted for `camera.view_proj` baking in
    /// bearing rotation -- the offset's axes stayed screen-aligned while the
    /// line itself rotated with the camera, so apparent line width scaled by
    /// `|cos(bearing)|` and hit exactly zero at bearing 90 degrees (reachable
    /// in the shipped UI via Shift+drag, which adds to `camera.bearing` with
    /// no clamp). The fix instead extrudes a small vector in world space and
    /// projects *that vector* through `camera.view_proj` as a direction, so
    /// it inherits the same bearing rotation the line's own vertices do.
    /// This test renders the same line at bearing 0 and bearing 90 degrees
    /// and asserts coverage does not collapse toward zero at 90 degrees.
    #[test]
    fn line_width_survives_camera_bearing_rotation() {
        async fn render_and_count_at_bearing(
            ctx: &GpuContext,
            pipeline: &TilePipeline,
            size: u32,
            tile: crate::geo::TileId,
            extent: u32,
            bearing: f32,
        ) -> u32 {
            let zoom = 2.0f64;
            let camera = crate::render::Camera {
                center_lon: 0.0,
                center_lat: 0.0,
                zoom,
                pitch: 0.0,
                bearing,
                viewport_width_px: size as f32,
                viewport_height_px: size as f32,
                fov_y: crate::render::Camera::DEFAULT_FOV_Y,
            };
            let view_proj = camera.view_projection().to_cols_array_2d();
            // `model.matrix` now carries the camera baked in (see shader.wgsl's
            // `vs_main`), so it must be recomputed per-bearing here from
            // `combined_tile_matrix`, matching what production `paint()` writes
            // -- unlike the old bare `tile_model_matrix`, it isn't invariant
            // across the two bearings this test compares.
            let model = crate::render::combined_tile_matrix(&camera, tile, extent as f32).to_cols_array_2d();

            let color_texture = make_color_texture(ctx, size);
            let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor::default());
            let depth_view = pipeline.create_depth_view(ctx, size, size);
            let camera_bind_group = pipeline.test_camera_bind_group(ctx, view_proj, zoom as f32);
            let model_bind_group = pipeline.test_model_bind_group(ctx, model, 1.0, 0.0);

            let line = geo_types::Geometry::LineString(geo_types::LineString::from(vec![
                (0i32, 0i32),
                (extent as i32, 0i32),
            ]));
            let plan = crate::style::PaintPlan {
                ops: vec![crate::style::PaintOp::Line {
                    metrics: false,
                    color_from: crate::style::Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
                    color_to: crate::style::Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
                    width_from: 8.0,
                    width_to: 8.0,
                    gap_from: 0.0,
                    gap_to: 0.0,
                    cap: crate::style::LineCap::Butt,
                    join: crate::style::LineJoin::Miter,
                    dash: crate::style::DashPattern::none(),
                    geometry: Arc::new(line),
                }],
            };
            let mesh = crate::render::tessellate_plan(&plan, extent, crate::render::mesh::COORD_SCALE, crate::render::subdivide::Subdivision::NONE);
            let tile_buffers = pipeline.create_tile_buffers(ctx, &mesh);

            let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            pipeline.render(
                &mut encoder,
                &color_view,
                &depth_view,
                wgpu::Color::TRANSPARENT,
                &camera_bind_group,
                &[(&tile_buffers, &model_bind_group)],
                None,
            );

            let data = read_all_pixels(ctx, &color_texture, encoder, size).await;
            let bytes_per_row = (size * 4).next_multiple_of(256);
            let mut colored_pixels: u32 = 0;
            for row in 0..size {
                for col in 0..size {
                    let offset = (row * bytes_per_row + col * 4) as usize;
                    if data[offset + 3] > 20 {
                        colored_pixels += 1;
                    }
                }
            }
            colored_pixels
        }

        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 256u32;

            let tile = crate::geo::TileId { z: 2, x: 2, y: 2 };
            let extent = 4096u32;

            let bearing_0 = render_and_count_at_bearing(&ctx, &pipeline, size, tile, extent, 0.0).await;
            let bearing_90 =
                render_and_count_at_bearing(&ctx, &pipeline, size, tile, extent, std::f32::consts::FRAC_PI_2)
                    .await;

            assert!(bearing_0 > 200, "sanity check: bearing-0 line should render with visible coverage, got {bearing_0}");
            assert!(
                bearing_90 > bearing_0 / 4,
                "line coverage collapsed at bearing 90 degrees ({bearing_90}px) relative to bearing 0 ({bearing_0}px) \
                 -- the extrusion is not surviving camera bearing rotation"
            );
        });
    }

    #[test]
    fn render_skips_a_tile_with_zero_geometry_instead_of_panicking() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 64u32;
            let color_texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("test-color"),
                size: wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor::default());
            let depth_view = pipeline.create_depth_view(&ctx, size, size);
            let identity = glam::Mat4::IDENTITY.to_cols_array_2d();
            let camera_bind_group = pipeline.test_camera_bind_group(&ctx, identity, 0.0);

            let empty_mesh = crate::render::TileMesh { vertices: vec![], indices: vec![] };
            let empty_buffers = pipeline.create_tile_buffers(&ctx, &empty_mesh);
            let model_bind_group = pipeline.test_model_bind_group(&ctx, identity, 1.0, 0.0);

            let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            // Must not panic.
            pipeline.render(
                &mut encoder,
                &color_view,
                &depth_view,
                wgpu::Color::TRANSPARENT,
                &camera_bind_group,
                &[(&empty_buffers, &model_bind_group)],
                None,
            );
            ctx.queue.submit(Some(encoder.finish()));
        });
    }

    /// Regression test for a real Task 6 review finding: `tessellate_plan`
    /// merges every style layer's geometry for a tile into one shared
    /// vertex/index buffer, drawn in a single `draw_indexed` call — so two
    /// overlapping fills from different layers (all v1-scope geometry sits
    /// at z=0, no extrusion/terrain yet) are genuinely coplanar fragments
    /// within that one draw call. `depth_compare: Less` rejected the second
    /// layer's fragments outright wherever they overlapped the first (equal
    /// depth does not satisfy "strictly less than"), silently discarding
    /// every layer drawn after the first at a given pixel. The fix
    /// (`LessEqual`) makes the layer drawn later — by `PaintPlan::ops`
    /// order, i.e. draw order — win at equal depth, matching the intended
    /// "layers paint over each other in order" semantics, while leaving real
    /// depth testing intact for any future non-coplanar (elevated) geometry.
    #[test]
    fn later_fill_layer_at_equal_depth_overwrites_the_earlier_one() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);

            let size = 64u32;
            let color_texture = make_color_texture(&ctx, size);
            let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor::default());
            let depth_view = pipeline.create_depth_view(&ctx, size, size);

            let identity = glam::Mat4::IDENTITY.to_cols_array_2d();
            let camera_bind_group = pipeline.test_camera_bind_group(&ctx, identity, 0.0);
            let model_bind_group = pipeline.test_model_bind_group(&ctx, identity, 1.0, 0.0);

            // Two fully-overlapping, viewport-covering squares at the
            // identical z=0 plane (identity model/camera, so clip.z is
            // exactly 0.0 for every vertex of both) — red drawn first, green
            // drawn second — merged into one mesh exactly as
            // `tessellate_plan` would merge two style layers' `PaintOp::Fill`
            // ops for one tile.
            let full_square = Arc::new(geo_types::Geometry::Polygon(geo_types::Polygon::new(
                geo_types::LineString::from(vec![
                    (-1000, -1000),
                    (1000, -1000),
                    (1000, 1000),
                    (-1000, 1000),
                    (-1000, -1000),
                ]),
                vec![],
            )));
            let plan = crate::style::PaintPlan {
                ops: vec![
                    crate::style::PaintOp::Fill {
                        color_from: crate::style::Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
                        color_to: crate::style::Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
                        geometry: full_square.clone(),
                    },
                    crate::style::PaintOp::Fill {
                        color_from: crate::style::Rgba { r: 0.0, g: 1.0, b: 0.0, a: 1.0 },
                        color_to: crate::style::Rgba { r: 0.0, g: 1.0, b: 0.0, a: 1.0 },
                        geometry: full_square,
                    },
                ],
            };
            let mesh = crate::render::tessellate_plan(&plan, 4096, crate::render::mesh::COORD_SCALE, crate::render::subdivide::Subdivision::NONE);
            let tile_buffers = pipeline.create_tile_buffers(&ctx, &mesh);

            let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            pipeline.render(
                &mut encoder,
                &color_view,
                &depth_view,
                wgpu::Color::TRANSPARENT,
                &camera_bind_group,
                &[(&tile_buffers, &model_bind_group)],
                None,
            );

            let data = read_all_pixels(&ctx, &color_texture, encoder, size).await;
            let bytes_per_row = (size * 4).next_multiple_of(256);
            let center_pixel_offset = (size / 2 * bytes_per_row + (size / 2) * 4) as usize;
            let pixel = &data[center_pixel_offset..center_pixel_offset + 4];

            // With the `Less` bug, the first (red) layer would win here —
            // this assertion would fail under that comparison function.
            assert!(pixel[1] > 200, "expected the later (green) layer to win at equal depth, got {pixel:?}");
            assert!(pixel[0] < 50, "expected the earlier (red) layer to be fully overwritten, got {pixel:?}");
        });
    }

    /// Pin for `append_stroke`'s corrected comment (Task 6 review, fix
    /// rounds 1-2). Renders a real hairpin turn (`next_tangent.dot(
    /// prev_tangent) < 0`, lyon's own `angle_is_sharp` condition)
    /// tessellated via the real production path (`tessellate_plan`, not a
    /// hand-built mesh) at two wildly different STYLE-declared widths (4.0
    /// vs 400.0), and asserts the emitted geometry -- vertex count, index
    /// buffer, and every vertex's position/normal/is_stroke -- is
    /// bit-for-bit IDENTICAL between them.
    ///
    /// What this actually pins, precisely (a fix-round-2 review correction:
    /// an earlier version of this test/name claimed to pin the fold/
    /// no-fold JOIN-TOPOLOGY branch `append_stroke`'s comment also
    /// documents -- it does not, for THIS geometry): `extruded_normal`'s
    /// magnitude at the join is bounded by the fixed tessellation
    /// half-width (50 lyon units, `STROKE_TESSELLATION_WIDTH` / 2), which
    /// is orders of magnitude smaller than this hairpin's segment lengths
    /// (hundreds of thousands of lyon units after `COORD_SCALE`'s 1000x),
    /// so lyon's fold condition (`d_next.min(d_prev) > 0.0`) is never
    /// satisfied here -- the fold branch itself remains unpinned by any
    /// test today (see `append_stroke`'s own comment: "add that only if
    /// it's straightforward" -- it needs edges on the order of 1-2 tile
    /// units long, which this hairpin's are not). What DOES vary here is
    /// `normal`'s exact f32 value: at these large (up to ~3e6 lyon-unit)
    /// coordinates, ordinary floating-point cancellation makes `normal`'s
    /// computation genuinely sensitive to the configured width -- this
    /// test confirms that sensitivity is fully eliminated by fixing the
    /// tessellation width, for a sharp turn at real production coordinate
    /// scale, not only for the smaller/simpler geometry `render::mesh`'s
    /// cache-equivalence test covers. Then renders the join and confirms
    /// it actually paints pixels (a corrupted join is exactly the kind of
    /// defect that compiles clean and only shows up as a visible gap).
    #[test]
    fn sharp_turn_wide_line_join_normals_are_independent_of_declared_width() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);

            // A hairpin turn within one MVT tile's extent: p0 -> p1 heads
            // due "east" (+x); p1 -> p2 heads back sharply toward "south-
            // west", so `next_tangent.dot(prev_tangent) < 0` -- lyon's own
            // `angle_is_sharp` condition (see `stroke.rs` in the vendored
            // `lyon_tessellation` source, the join-fold branch).
            let extent = 4096i32;
            let hairpin = std::sync::Arc::new(geo_types::Geometry::LineString(geo_types::LineString::from(vec![
                (0i32, 2048i32),
                (3000i32, 2048i32),
                (2700i32, 2600i32),
            ])));

            let red = crate::style::Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 };
            let plan_at = |width: f32| crate::style::PaintPlan {
                ops: vec![crate::style::PaintOp::Line {
                    metrics: false,
                    color_from: red,
                    color_to: red,
                    width_from: width,
                    width_to: width,
                    gap_from: 0.0,
                    gap_to: 0.0,
                    cap: crate::style::LineCap::Butt,
                    join: crate::style::LineJoin::Miter,
                    dash: crate::style::DashPattern::none(),
                    geometry: hairpin.clone(),
                }],
            };

            let narrow = crate::render::tessellate_plan(&plan_at(4.0), extent as u32, crate::render::mesh::COORD_SCALE, crate::render::subdivide::Subdivision::NONE);
            let wide = crate::render::tessellate_plan(&plan_at(400.0), extent as u32, crate::render::mesh::COORD_SCALE, crate::render::subdivide::Subdivision::NONE);

            assert!(!narrow.vertices.is_empty(), "the hairpin must actually produce stroke geometry");
            assert_eq!(
                narrow.vertices.len(),
                wide.vertices.len(),
                "a sharp join's vertex count must be independent of the style's declared width -- this \
                 hairpin's segments are far longer than the fixed tessellation half-width, so it does not \
                 exercise lyon's fold/no-fold branch (see this test's doc comment), but any width-dependent \
                 vertex-count divergence here would still indicate a regression in append_stroke's \
                 STROKE_TESSELLATION_WIDTH fix"
            );
            assert_eq!(narrow.indices, wide.indices, "index buffer (topology) must be identical across declared widths");
            let narrow_geometry: Vec<([f32; 2], [f32; 2], f32)> =
                narrow.vertices.iter().map(|v| (v.position, v.normal, v.is_stroke)).collect();
            let wide_geometry: Vec<([f32; 2], [f32; 2], f32)> =
                wide.vertices.iter().map(|v| (v.position, v.normal, v.is_stroke)).collect();
            assert_eq!(
                narrow_geometry, wide_geometry,
                "position/normal/is_stroke must be bit-for-bit identical across declared widths -- this IS \
                 the actual proof for this geometry (f32 cancellation at large tile-scale coordinates is \
                 what would diverge here, not the fold/no-fold topology branch -- see this test's doc \
                 comment); only half_width_px/half_width_px_to (deliberately excluded above) may differ"
            );

            // Now render the join for real and confirm it isn't a hole: a
            // fold decision that incorrectly OMITTED the back vertex would
            // still produce SOME triangles (folding just changes which),
            // but an outright corrupted join (e.g. degenerate/self-
            // intersecting geometry) would collapse coverage near zero.
            let size = 512u32;
            let color_texture = make_color_texture(&ctx, size);
            let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor::default());
            let depth_view = pipeline.create_depth_view(&ctx, size, size);

            // Same tile/zoom pairing as
            // `stroke_extrusion_scales_with_camera_zoom_not_tile_model_scale`
            // above: at z=2, the tile's corner sits exactly at the world
            // center, which is what keeps this geometry (well within the
            // tile's 0..4096 extent) inside the visible frustum.
            let tile = crate::geo::TileId { z: 2, x: 2, y: 2 };
            let zoom = 2.0f64;
            let camera = crate::render::Camera {
                center_lon: 0.0,
                center_lat: 0.0,
                zoom,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: size as f32,
                viewport_height_px: size as f32,
                fov_y: crate::render::Camera::DEFAULT_FOV_Y,
            };
            let view_proj = camera.view_projection().to_cols_array_2d();
            let model = crate::render::combined_tile_matrix(&camera, tile, extent as f32).to_cols_array_2d();
            let camera_bind_group = pipeline.test_camera_bind_group(&ctx, view_proj, zoom as f32);
            let model_bind_group = pipeline.test_model_bind_group(&ctx, model, 1.0, 0.0);

            let render_mesh = crate::render::tessellate_plan(&plan_at(40.0), extent as u32, crate::render::mesh::COORD_SCALE, crate::render::subdivide::Subdivision::NONE);
            let tile_buffers = pipeline.create_tile_buffers(&ctx, &render_mesh);

            let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            pipeline.render(
                &mut encoder,
                &color_view,
                &depth_view,
                wgpu::Color::TRANSPARENT,
                &camera_bind_group,
                &[(&tile_buffers, &model_bind_group)],
                None,
            );

            let data = read_all_pixels(&ctx, &color_texture, encoder, size).await;
            let bytes_per_row = (size * 4).next_multiple_of(256);
            let mut colored_pixels: u32 = 0;
            for row in 0..size {
                for col in 0..size {
                    let offset = (row * bytes_per_row + col * 4) as usize;
                    if data[offset + 3] > 20 {
                        colored_pixels += 1;
                    }
                }
            }
            assert!(
                colored_pixels > 50,
                "expected the hairpin's join to actually paint a visible number of pixels, got {colored_pixels}"
            );
        });
    }

    /// Two triangles, side by side and not overlapping, so a partial
    /// write's byte offset is answerable from the rendered image: which
    /// half changed colour says which vertices the write landed on.
    fn two_triangles(left: [f32; 4], right: [f32; 4]) -> crate::render::TileMesh {
        let vertex = |x: f32, y: f32, color: [f32; 4]| Vertex {
            position: [x, y],
            normal: [0.0, 0.0],
            color,
            is_stroke: 0.0,
            half_width_px: 0.0,
            color_to: color,
            half_width_px_to: 0.0,
            gap_half_px: 0.0,
            gap_half_px_to: 0.0,
            line_distance: 0.0,
            dash_runs: [0.0; 4], line_progress: -1.0,
        };
        crate::render::TileMesh {
            vertices: vec![
                vertex(-0.9, -0.9, left),
                vertex(-0.1, -0.9, left),
                vertex(-0.5, 0.9, left),
                vertex(0.1, -0.9, right),
                vertex(0.9, -0.9, right),
                vertex(0.5, 0.9, right),
            ],
            indices: vec![0, 1, 2, 3, 4, 5],
        }
    }

    /// A partial write must land on exactly the vertices it names.
    ///
    /// This is the risk the paint-only overlay update introduces: the
    /// overlay used to push its vertex buffer whole, and an off-by-one in a
    /// byte offset would corrupt GEOMETRY rather than colour -- positions
    /// and normals shifted by a fraction of a vertex -- which looks nothing
    /// like the paint change that caused it. So the check is made against a
    /// real device: recolour only the SECOND triangle's three vertices and
    /// read back both halves of the image.
    #[test]
    fn write_vertex_range_recolours_only_the_vertices_it_names() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);

            let size = 64u32;
            let green = [0.0, 1.0, 0.0, 1.0];
            let red = [1.0, 0.0, 0.0, 1.0];
            let identity = glam::Mat4::IDENTITY.to_cols_array_2d();
            let camera_bind_group = pipeline.test_camera_bind_group(&ctx, identity, 0.0);
            let model_bind_group = pipeline.test_model_bind_group(&ctx, identity, 1.0, 0.0);
            let buffers = pipeline.create_tile_buffers(&ctx, &two_triangles(green, green));

            // Only the right triangle's vertices, which start at vertex 3 --
            // so the write must begin 3 * size_of::<Vertex>() bytes in.
            pipeline.write_vertex_range(&ctx, &buffers, &two_triangles(green, red), 3..6);

            let color_texture = make_color_texture(&ctx, size);
            let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor::default());
            let depth_view = pipeline.create_depth_view(&ctx, size, size);
            let mut encoder =
                ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            pipeline.render(
                &mut encoder,
                &color_view,
                &depth_view,
                wgpu::Color::TRANSPARENT,
                &camera_bind_group,
                &[(&buffers, &model_bind_group)],
                None,
            );
            let data = read_all_pixels(&ctx, &color_texture, encoder, size).await;

            let bytes_per_row = (size * 4).next_multiple_of(256);
            let pixel = |x: u32, y: u32| {
                let offset = (y * bytes_per_row + x * 4) as usize;
                [data[offset], data[offset + 1], data[offset + 2]]
            };
            // Three-quarters down, where both triangles are still wide.
            // `shader.wgsl` mirrors x after projection (see its comment on
            // `out.clip_position.x = -out.clip_position.x`), so the FIRST
            // triangle -- the one at negative x, whose vertices the write
            // did not name -- is the one on the RIGHT of the image.
            let y = size * 3 / 4;
            let unnamed = pixel(size * 3 / 4, y);
            let named = pixel(size / 4, y);
            assert!(
                unnamed[1] > 200 && unnamed[0] < 50,
                "vertices 0..3 were not named and must still be green: {unnamed:?}"
            );
            assert!(
                named[0] > 200 && named[1] < 50,
                "vertices 3..6 were named and must be red: {named:?}"
            );
        });
    }

    /// An op can tessellate to nothing at all, and wgpu rejects a
    /// zero-length write outright -- so an empty range is a no-op, not a
    /// validation failure the caller has to remember to avoid.
    #[test]
    fn write_vertex_range_ignores_an_empty_range() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let buffers = pipeline.create_tile_buffers(&ctx, &triangle_mesh([0.0, 1.0, 0.0, 1.0]));
            pipeline.write_vertex_range(&ctx, &buffers, &triangle_mesh([1.0, 0.0, 0.0, 1.0]), 2..2);
            ctx.device.poll(wgpu::PollType::wait_indefinitely()).expect("no validation error");
        });
    }

    /// Just the left triangle of `two_triangles`, so a reinstall can shrink
    /// the mesh without changing where the surviving triangle draws --
    /// which is what makes "did the leftover right triangle disappear?" an
    /// answerable question about the rendered image.
    fn left_triangle_only(left: [f32; 4]) -> crate::render::TileMesh {
        let mut mesh = two_triangles(left, left);
        mesh.vertices.truncate(3);
        mesh.indices.truncate(3);
        mesh
    }

    /// The defect: every mesh install built a fresh pair of buffers, and on
    /// the overlay that is a ~204 MB `create_buffer_init` per install with
    /// no reuse at all. A caller pushing a live feed installs a new
    /// generation per tick, so the allocation rate was bounded by the tick
    /// rate and nothing else.
    ///
    /// Identity, not size: the claim is that the SAME buffers came back,
    /// and two allocations of equal size are indistinguishable by capacity.
    #[test]
    fn reinstalling_a_mesh_that_fits_reuses_the_same_buffers() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let green = [0.0, 1.0, 0.0, 1.0];
            let red = [1.0, 0.0, 0.0, 1.0];

            let mut buffers = pipeline.create_tile_buffers(&ctx, &two_triangles(green, green));
            let vertex = buffers.vertex_buffer.handle();
            let index = buffers.index_buffer.handle();

            let installs_before = crate::render::allocation_stats().mesh_installs_total;
            for tick in 0..20 {
                let colour = if tick % 2 == 0 { red } else { green };
                buffers = pipeline.install_mesh(&ctx, Some(buffers), &two_triangles(colour, colour));
                assert_eq!(buffers.vertex_buffer.handle(), vertex, "tick {tick} must reuse the vertex buffer");
                assert_eq!(buffers.index_buffer.handle(), index, "tick {tick} must reuse the index buffer");
            }
            assert!(
                crate::render::allocation_stats().mesh_installs_total >= installs_before + 20,
                "every reinstall is still an install and must still be counted -- \
                 mesh_installs_total climbing while gpu_buffers_created_total does not is the point"
            );
        });
    }

    /// Grow-and-keep, both halves. A mesh that does not fit forces exactly
    /// one reallocation; a smaller mesh afterwards keeps the grown buffers
    /// rather than trading them for tighter ones.
    ///
    /// Exact-size reuse would fail the second half, and that is the half
    /// that matters: an overlay's vertex count moves on nearly every push,
    /// so a guard that only reuses on an exact match reallocates on nearly
    /// every push.
    #[test]
    fn a_bigger_mesh_grows_the_buffers_and_a_smaller_one_keeps_the_growth() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let green = [0.0, 1.0, 0.0, 1.0];

            let small = pipeline.create_tile_buffers(&ctx, &left_triangle_only(green));
            let small_vertex_capacity = small.vertex_capacity_bytes();
            let small_index_capacity = small.index_capacity_bytes();

            let grown = pipeline.install_mesh(&ctx, Some(small), &two_triangles(green, green));
            assert!(
                grown.vertex_capacity_bytes() > small_vertex_capacity,
                "six vertices do not fit in a three-vertex buffer ({} vs {small_vertex_capacity})",
                grown.vertex_capacity_bytes()
            );
            assert!(grown.index_capacity_bytes() > small_index_capacity, "nor six indices in a three-index buffer");
            let grown_vertex = grown.vertex_buffer.handle();
            let grown_index = grown.index_buffer.handle();
            let grown_vertex_capacity = grown.vertex_capacity_bytes();
            let grown_index_capacity = grown.index_capacity_bytes();

            let shrunk = pipeline.install_mesh(&ctx, Some(grown), &left_triangle_only(green));
            assert_eq!(shrunk.vertex_buffer.handle(), grown_vertex, "a smaller mesh must keep the grown vertex buffer");
            assert_eq!(shrunk.index_buffer.handle(), grown_index, "a smaller mesh must keep the grown index buffer");
            assert_eq!(shrunk.vertex_capacity_bytes(), grown_vertex_capacity, "capacity must never shrink");
            assert_eq!(shrunk.index_capacity_bytes(), grown_index_capacity, "capacity must never shrink");
            assert_eq!(shrunk.index_count, 3, "the DRAW must shrink even though the buffer does not");
        });
    }

    /// Reuse is only worth anything if the new mesh actually reaches the
    /// GPU. Reinstall a recoloured mesh into buffers that already fit it and
    /// read the pixels back: a reuse path that silently kept the old bytes
    /// would pass every capacity assertion above and render last tick's
    /// data forever.
    #[test]
    fn a_reinstalled_mesh_is_what_gets_drawn() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);

            let size = 64u32;
            let green = [0.0, 1.0, 0.0, 1.0];
            let red = [1.0, 0.0, 0.0, 1.0];
            let identity = glam::Mat4::IDENTITY.to_cols_array_2d();
            let camera_bind_group = pipeline.test_camera_bind_group(&ctx, identity, 0.0);
            let model_bind_group = pipeline.test_model_bind_group(&ctx, identity, 1.0, 0.0);

            let buffers = pipeline.create_tile_buffers(&ctx, &two_triangles(green, green));
            let buffers = pipeline.install_mesh(&ctx, Some(buffers), &two_triangles(red, red));

            let color_texture = make_color_texture(&ctx, size);
            let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor::default());
            let depth_view = pipeline.create_depth_view(&ctx, size, size);
            let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            pipeline.render(&mut encoder, &color_view, &depth_view, wgpu::Color::TRANSPARENT, &camera_bind_group, &[(&buffers, &model_bind_group)], None);
            let data = read_all_pixels(&ctx, &color_texture, encoder, size).await;

            let bytes_per_row = (size * 4).next_multiple_of(256);
            let offset = ((size * 3 / 4) * bytes_per_row + (size / 4) * 4) as usize;
            let pixel = [data[offset], data[offset + 1], data[offset + 2]];
            assert!(pixel[0] > 200 && pixel[1] < 50, "the reinstalled mesh's red must be on screen, not the green it replaced: {pixel:?}");
        });
    }

    /// The correctness risk grow-and-keep introduces: a smaller mesh leaves
    /// the previous one's vertices and indices sitting past its own end, and
    /// `draw_indexed(0..index_count)` is the only thing stopping them being
    /// drawn. Install two triangles, then reinstall just the left one, and
    /// check the right one is gone from the image.
    #[test]
    fn a_smaller_reinstalled_mesh_does_not_draw_the_leftover_triangles() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);

            let size = 64u32;
            let green = [0.0, 1.0, 0.0, 1.0];
            let identity = glam::Mat4::IDENTITY.to_cols_array_2d();
            let camera_bind_group = pipeline.test_camera_bind_group(&ctx, identity, 0.0);
            let model_bind_group = pipeline.test_model_bind_group(&ctx, identity, 1.0, 0.0);

            let buffers = pipeline.create_tile_buffers(&ctx, &two_triangles(green, green));
            let buffers = pipeline.install_mesh(&ctx, Some(buffers), &left_triangle_only(green));

            let color_texture = make_color_texture(&ctx, size);
            let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor::default());
            let depth_view = pipeline.create_depth_view(&ctx, size, size);
            let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            pipeline.render(&mut encoder, &color_view, &depth_view, wgpu::Color::TRANSPARENT, &camera_bind_group, &[(&buffers, &model_bind_group)], None);
            let data = read_all_pixels(&ctx, &color_texture, encoder, size).await;

            let bytes_per_row = (size * 4).next_multiple_of(256);
            let alpha = |x: u32, y: u32| data[((y * bytes_per_row) + x * 4 + 3) as usize];
            let y = size * 3 / 4;
            // `shader.wgsl` mirrors x after projection, so the surviving
            // triangle (at negative x) is the one on the RIGHT of the image
            // -- the same mapping `write_vertex_range_recolours_only_the_
            // vertices_it_names` relies on.
            assert!(alpha(size * 3 / 4, y) > 200, "the reinstalled triangle must still draw");
            assert_eq!(alpha(size / 4, y), 0, "the dropped triangle's vertices are still in the buffer but must not be drawn");
        });
    }

    /// The whole point of `PassTimer` is that the numbers are REAL, and
    /// nothing about the wiring proves that: a query set that is never
    /// written resolves to whatever the driver left there, and a period of
    /// zero would turn every duration into a confident 0 ns. So this runs
    /// the production path end to end -- write, resolve, copy, map -- and
    /// reads the number back.
    ///
    /// Its device is built with `GpuContext::new`, not `new_headless`.
    /// `new_headless` deliberately requests the WebGPU baseline and no
    /// features (it is this crate's portability tripwire), so a `PassTimer`
    /// can never exist on it and a test written against it would pass by
    /// measuring nothing.
    ///
    /// SKIPS, loudly, on an adapter without `TIMESTAMP_QUERY`. That is the
    /// same degradation every consumer gets, and a test that failed there
    /// would be asserting a property of the machine rather than of the code.
    #[test]
    fn a_timed_pass_reports_a_real_gpu_duration() {
        pollster::block_on(async {
            let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
            let Ok(adapter) = instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::LowPower,
                    ..Default::default()
                })
                .await
            else {
                eprintln!("a_timed_pass_reports_a_real_gpu_duration: no adapter, skipping");
                return;
            };
            let ctx = GpuContext::new(&adapter).await.expect("a device on an adapter that offered one");
            let Some(timer) = PassTimer::new(&ctx) else {
                eprintln!("a_timed_pass_reports_a_real_gpu_duration: adapter has no TIMESTAMP_QUERY, skipping");
                return;
            };

            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 256u32;
            let identity = glam::Mat4::IDENTITY.to_cols_array_2d();
            let camera_bind_group = pipeline.test_camera_bind_group(&ctx, identity, 0.0);
            let model_bind_group = pipeline.test_model_bind_group(&ctx, identity, 1.0, 0.0);
            let tile_buffers = pipeline.create_tile_buffers(&ctx, &triangle_mesh([0.0, 1.0, 0.0, 1.0]));
            let color_texture = make_color_texture(&ctx, size);
            let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor::default());
            let depth_view = pipeline.create_depth_view(&ctx, size, size);

            let before = crate::render::alloc::allocation_stats();

            // TWO frames, because one cannot work: the resolve has to be on
            // a later command buffer than the pass it reads. This is the
            // exact sequence `MapView::paint` runs, and running it any other
            // way is what produced the flat zero documented on `PassTimer`.
            let timed_frame = |symbol_pass_ran: bool| {
                let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
                let resolved = timer.begin_frame(&mut encoder);
                pipeline.render(
                    &mut encoder,
                    &color_view,
                    &depth_view,
                    wgpu::Color::TRANSPARENT,
                    &camera_bind_group,
                    &[(&tile_buffers, &model_bind_group)],
                    Some(timer.tile_writes()),
                );
                // The symbol pass never runs here, so its slots hold nothing
                // and must not be reported -- the same `false` `paint`
                // passes on a frame with no labels, icons or plate.
                timer.end_frame(symbol_pass_ran);
                ctx.queue.submit(Some(encoder.finish()));
                timer.read_back(resolved);
                // The one place in this crate that WAITS on a readback.
                // `paint` polls and moves on; a test has to know the callback
                // ran before it can assert on what the callback published.
                ctx.device.poll(wgpu::PollType::wait_indefinitely()).expect("the submitted work to complete");
                resolved
            };
            assert_eq!(timed_frame(false), None, "the first frame has no earlier frame to resolve");
            assert_eq!(timed_frame(false), Some(false), "the second frame resolves the first");

            let after = crate::render::alloc::allocation_stats();

            assert_eq!(
                after.gpu_pass_samples,
                before.gpu_pass_samples + 1,
                "one timed pass must publish exactly one sample"
            );
            assert_eq!(
                after.gpu_symbol_pass_samples, before.gpu_symbol_pass_samples,
                "a frame whose symbol pass never opened must not publish a symbol sample"
            );
            // A lower bound of 1 ns, not of some plausible microsecond
            // figure: the assertion worth making is that the ticks and the
            // period are both non-degenerate. Pinning a duration would pin
            // this machine's GPU.
            assert!(
                after.gpu_tile_pass_nanos_sum > before.gpu_tile_pass_nanos_sum,
                "drawing a triangle over {size}x{size} px must take a measurable, non-zero time on the GPU; \
                 got {} ns, which means either the query slots were never written or the timestamp period is 0",
                after.gpu_tile_pass_nanos_sum - before.gpu_tile_pass_nanos_sum
            );
            // `>=`, not `==`: `DRAW_CALLS` is process-global and the other
            // GPU tests in this binary run on other threads, so the delta
            // across this test's two frames is at least its own two draws
            // and possibly theirs as well. Two is the claim -- one
            // `draw_indexed` per non-empty tile per frame -- and a floor is
            // the strongest form of it that is not racy.
            assert!(
                after.draw_calls_total >= before.draw_calls_total + 2,
                "one non-empty tile in `draws` is one `draw_indexed`, once per frame: expected at \
                 least 2 more than {}, got {}",
                before.draw_calls_total,
                after.draw_calls_total
            );
        });
    }

    /// A grid of `cells x cells` quads covering the whole of one tile in
    /// `[0, 1]` in-tile space, in lyon units, as a solid fill.
    ///
    /// Subdivided rather than the two triangles a tile-covering polygon would
    /// tessellate to, because the globe projection is applied PER VERTEX: two
    /// triangles projected onto a sphere are still two flat triangles, cutting
    /// a chord straight through it. Task 5 of the globe plan is what makes
    /// real tile geometry arrive subdivided; this builds its own so the
    /// projection can be tested before that exists.
    fn tile_covering_grid(cells: u32, extent: u32, color: [f32; 4]) -> crate::render::TileMesh {
        let span = crate::render::mesh::COORD_SCALE * extent as f32;
        let step = span / cells as f32;
        let mut vertices = Vec::new();
        for row in 0..=cells {
            for col in 0..=cells {
                vertices.push(Vertex {
                    position: [col as f32 * step, row as f32 * step],
                    normal: [0.0, 0.0],
                    color,
                    is_stroke: 0.0,
                    half_width_px: 0.0,
                    color_to: color,
                    half_width_px_to: 0.0,
                    gap_half_px: 0.0,
                    gap_half_px_to: 0.0,
                    line_distance: 0.0,
                    dash_runs: [0.0; 4], line_progress: -1.0,
                });
            }
        }
        let stride = cells + 1;
        let mut indices = Vec::new();
        for row in 0..cells {
            for col in 0..cells {
                let a = row * stride + col;
                // The bake's winding (`render::subdivide::orient_fill_triangles`):
                // `fs_main` drops a globe fill triangle that faces away.
                indices.extend_from_slice(&[a, a + stride, a + 1, a + 1, a + stride, a + stride + 1]);
            }
        }
        crate::render::TileMesh { vertices, indices }
    }

    /// **The acceptance criterion for the globe's vertex projection: at
    /// `transition = 1.0` the drawn extent is a DISC, and at
    /// `transition = 0.0` the same mesh and camera fill the frame.**
    ///
    /// A vertex-count or matrix assertion cannot tell a sphere from a plane;
    /// only the silhouette can. So this renders the whole world twice, from
    /// one mesh and one camera, changing nothing but
    /// `GlobeParams::transition`, and samples the four screen corners:
    ///
    /// - flat, they are COVERED -- at z1.5 the z0 tile spans `WORLD_SIZE`
    ///   (512) world units, i.e. 512 * 2^1.5 = 1448 logical px across a 512 px
    ///   viewport, so a rectangle overflows every edge;
    /// - globe, they are BACKGROUND, because a sphere of this radius at this
    ///   camera distance is inscribed well inside the frame.
    ///
    /// The centre must be covered in BOTH, which is what stops the globe half
    /// passing by drawing nothing at all -- the failure a "corners are clear"
    /// assertion on its own would wave through.
    ///
    /// # What this measured, and why the figures agree
    ///
    /// Measured on an Apple M-series adapter: **262,144 lit pixels flat**
    /// (every pixel of the 512x512 target) against **81,676 on the globe**,
    /// with 4 corners covered flat and 0 on the globe.
    ///
    /// 81,676 is a disc and can be checked as one rather than taken on faith.
    /// `globe_radius_world` is `WORLD_SIZE / 2*PI` = 81.487 world units at the
    /// equator, `Camera::distance()` at z1.5 on a 512 px viewport is 156.767,
    /// so the eye sits 238.254 from the sphere's centre and the tangent cone's
    /// half-angle is `asin(81.487 / 238.254)` = exactly 20.000 degrees. With a
    /// 60-degree vertical FOV that puts the silhouette at
    /// `256 * tan(20) / tan(30)` = **161.37 px**, for a disc of
    /// **81,806 px** -- 0.16% above what was counted, the shortfall being the
    /// two polar caps beyond +/-85.051 degrees that a Mercator grid does not
    /// reach, plus the alpha > 20 threshold at the antialiased rim.
    ///
    /// The chord confirms it independently: at row 128 (127.5 px above
    /// centre), a disc of radius 161.37 is `2 * sqrt(161.37^2 - 127.5^2)` =
    /// 197.4 px wide, and 198 px were counted. A rounded rectangle of the same
    /// area would be ~286 px there.
    ///
    /// The assertions below deliberately do NOT pin 161.37: that figure is
    /// `render::globe::globe_frame`'s camera chain's business, tested there,
    /// and pinning it here would re-pin that chain from the far end of the
    /// pipeline. What is asserted is the shape.
    #[test]
    fn at_full_transition_the_world_is_drawn_as_a_disc_and_not_a_rectangle() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);

            let size = 512u32;
            let extent = 4096u32;
            // The single root tile, which is the one tile MapLibre's globe
            // tile selection takes at low zoom
            // (`globe_covering_tiles_details_provider.ts:104`) -- and the only
            // tile whose `tile_mercator_coords` spans the entire sphere, which
            // is what makes a disc rather than a patch of one.
            let tile = crate::geo::TileId { z: 0, x: 0, y: 0 };
            // Low enough that the globe is comfortably inscribed in the
            // viewport, high enough that the FLAT render of the same tile
            // overflows it -- the two conditions that make the corner samples
            // discriminating in both directions.
            let zoom = 1.5f64;
            let camera = crate::render::Camera {
                center_lon: 0.0,
                center_lat: 0.0,
                zoom,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: size as f32,
                viewport_height_px: size as f32,
                fov_y: crate::render::Camera::DEFAULT_FOV_Y,
            };

            let frame = crate::render::globe::globe_frame(&camera);
            let mesh = tile_covering_grid(64, extent, [1.0, 0.0, 0.0, 1.0]);
            let tile_buffers = pipeline.create_tile_buffers(&ctx, &mesh);
            let view_proj = camera.view_projection().to_cols_array_2d();
            let model = crate::render::combined_tile_matrix(&camera, tile, extent as f32).to_cols_array_2d();
            let tile_world_size = crate::render::WORLD_SIZE / 2f32.powi(i32::from(tile.z));

            // `transition` is the ONLY thing that differs between the two
            // renders below. Everything else -- mesh, camera, matrices, plane
            // -- is shared, so a difference in the output can only come from
            // the projection.
            let globe_params = |transition: f32| GlobeParams {
                globe_matrix: frame.globe_view_proj.as_mat4().to_cols_array_2d(),
                tile_mercator_coords: crate::render::globe::tile_mercator_coords(tile),
                clipping_plane: frame.clipping_plane.map(|c| c as f32),
                transition,
                lyon_to_tile_unit: 1.0 / (crate::render::mesh::COORD_SCALE * extent as f32),
                tile_units_per_px: 1.0 / (tile_world_size * 2f32.powf(zoom as f32)),
                // Unlit: this test grades a SHAPE, from alpha. See
                // `UNLIT_SUN`.
                ..GlobeParams::FLAT
            }
            .unlit();

            let bytes_per_row = (size * 4).next_multiple_of(256);
            let covered = |data: &[u8], col: u32, row: u32| -> bool {
                let offset = (row * bytes_per_row + col * 4) as usize;
                data[offset + 3] > 20
            };

            let mut coverage = Vec::new();
            for transition in [0.0f32, 1.0] {
                let color_texture = make_color_texture(&ctx, size);
                let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor::default());
                let depth_view = pipeline.create_depth_view(&ctx, size, size);
                let camera_bind_group = pipeline.test_camera_bind_group(&ctx, view_proj, zoom as f32);
                let model_bind_group = pipeline
                    .create_model_bind_group(&ctx, model, 1.0, 0.0, globe_params(transition))
                    .1;

                let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
                pipeline.render(
                    &mut encoder,
                    &color_view,
                    &depth_view,
                    wgpu::Color::TRANSPARENT,
                    &camera_bind_group,
                    &[(&tile_buffers, &model_bind_group)],
                    None,
                );
                let data = read_all_pixels(&ctx, &color_texture, encoder, size).await;

                let last = size - 1;
                // Inset by two pixels: the outermost row and column are where
                // a half-pixel rasterization difference would decide the
                // answer, and the claim being tested is about a shape, not
                // about the sampling rule at the very edge of the target.
                let corners = [(2, 2), (last - 2, 2), (2, last - 2), (last - 2, last - 2)];
                let corners_covered = corners.iter().filter(|(c, r)| covered(&data, *c, *r)).count();
                let centre_covered = covered(&data, size / 2, size / 2);
                let lit = (0..size)
                    .flat_map(|row| (0..size).map(move |col| (col, row)))
                    .filter(|(c, r)| covered(&data, *c, *r))
                    .count();
                coverage.push((corners_covered, centre_covered, lit));
            }

            let (flat_corners, flat_centre, flat_lit) = coverage[0];
            let (globe_corners, globe_centre, globe_lit) = coverage[1];

            assert_eq!(
                flat_corners, 4,
                "the FLAT render is the control, and it is only a control if it really does \
                 overflow the viewport: at z{zoom} the z0 tile is {} logical px across in a \
                 {size} px frame, so all four corners must be covered. \
                 {flat_corners} were, over {flat_lit} lit pixels",
                tile_world_size * 2f32.powf(zoom as f32)
            );
            assert!(flat_centre, "the flat render must cover the centre too");

            assert_eq!(
                globe_corners, 0,
                "AT transition = 1.0 THE DRAWN EXTENT MUST BE A DISC: none of the four corners \
                 may be covered, but {globe_corners} were. A rectangle here means the vertex \
                 shader never reached `project_to_sphere` -- check that `transition` survived \
                 the uniform's 16-byte alignment before suspecting the projection itself"
            );
            assert!(
                globe_centre,
                "the globe must still DRAW: the centre of the frame is the point on the sphere \
                 directly under the camera, and it being background means the geometry was \
                 clipped away wholesale -- most likely `globe_clipping_z` inverted, which sends \
                 every vertex out of the 0 <= z <= w clip volume instead of only the far side"
            );
            // A disc inscribed in a frame the rectangle overflows covers
            // strictly less of it: 81,676 against 262,144 as measured, a ratio
            // of 3.2. The bound is `2x` rather than `3x` because the exact
            // figure is `render::globe`'s camera chain's business, not this
            // test's -- see this test's doc comment.
            assert!(
                globe_lit * 2 < flat_lit,
                "a disc inscribed in the frame must cover well under half of what a rectangle \
                 overflowing it does: globe {globe_lit}, flat {flat_lit}"
            );
        });
    }

    /// One `PaintOp::Fill` whose polygon covers the whole of one tile, in
    /// MVT coordinates (`0..extent`), as a `PaintPlan`.
    ///
    /// The realistic low-zoom fill: an ocean, or a landcover band, clipped
    /// to the tile it was encoded for -- and, crucially for the pole tests,
    /// a polygon with an edge lying exactly on the tile's north and south
    /// boundary, which is what `render::subdivide::append_pole_caps` grows a
    /// cap from.
    fn tile_covering_fill_plan(extent: u32, color: crate::style::Rgba) -> crate::style::PaintPlan {
        let e = extent as i32;
        let ring = geo_types::LineString::from(vec![(0, 0), (e, 0), (e, e), (0, e), (0, 0)]);
        crate::style::PaintPlan {
            ops: vec![crate::style::PaintOp::Fill {
                color_from: color,
                color_to: color,
                geometry: std::sync::Arc::new(geo_types::Geometry::Polygon(
                    geo_types::Polygon::new(ring, Vec::new()),
                )),
            }],
        }
    }

    /// Everything a globe pixel test needs for one level of tiles: the
    /// tessellated meshes and per-tile `GlobeParams`, at a given
    /// `Subdivision` policy.
    ///
    /// `subdivide` is a closure rather than a flag so the two halves of each
    /// test differ ONLY in what it returns -- the camera, the level, the
    /// geometry and every matrix are shared.
    ///
    /// Every tile comes out **unlit** (`GlobeParams::unlit`), so a test
    /// about a silhouette, an anchor or a stroke width reads the exact
    /// colours it read before the ground was lit. A test about the SUN
    /// overwrites the two fields afterwards -- see `lit_globe_draws`.
    #[allow(clippy::type_complexity)]
    fn globe_level_draws(
        camera: &crate::render::Camera,
        level: u8,
        extent: u32,
        subdivide: impl Fn(crate::geo::TileId) -> crate::render::subdivide::Subdivision,
    ) -> (Vec<crate::render::TileMesh>, Vec<([[f32; 4]; 4], GlobeParams)>) {
        globe_level_draws_in(
            camera,
            level,
            extent,
            crate::style::Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
            subdivide,
        )
    }

    /// [`globe_level_draws`] in a caller-chosen colour, still unlit.
    ///
    /// One colour for the whole level, not four: a lighting test needs the
    /// two sides of the terminator to differ ONLY by the light, and four
    /// tile colours would make every comparison a comparison of tiles.
    #[allow(clippy::type_complexity)]
    fn globe_level_draws_in(
        camera: &crate::render::Camera,
        level: u8,
        extent: u32,
        color: crate::style::Rgba,
        subdivide: impl Fn(crate::geo::TileId) -> crate::render::subdivide::Subdivision,
    ) -> (Vec<crate::render::TileMesh>, Vec<([[f32; 4]; 4], GlobeParams)>) {
        let frame = crate::render::globe::globe_frame(camera);
        let plan = tile_covering_fill_plan(extent, color);
        let side = 1u32 << level;
        let mut meshes = Vec::new();
        let mut uniforms = Vec::new();
        for y in 0..side {
            for x in 0..side {
                let tile = crate::geo::TileId { z: level, x, y };
                let mesh = crate::render::tessellate_plan(
                    &plan,
                    extent,
                    crate::render::mesh::COORD_SCALE,
                    subdivide(tile),
                );
                let tile_world_size = crate::render::WORLD_SIZE / 2f32.powi(i32::from(level));
                uniforms.push((
                    crate::render::combined_tile_matrix(camera, tile, extent as f32)
                        .to_cols_array_2d(),
                    GlobeParams {
                        globe_matrix: frame.globe_view_proj.as_mat4().to_cols_array_2d(),
                        tile_mercator_coords: crate::render::globe::tile_mercator_coords(tile),
                        clipping_plane: frame.clipping_plane.map(|c| c as f32),
                        transition: 1.0,
                        lyon_to_tile_unit: 1.0 / (crate::render::mesh::COORD_SCALE * extent as f32),
                        tile_units_per_px: 1.0 / (tile_world_size * 2f32.powf(camera.zoom as f32)),
                        ..GlobeParams::FLAT
                    }
                    .unlit(),
                ));
                meshes.push(mesh);
            }
        }
        (meshes, uniforms)
    }

    /// One level of globe tiles in one colour, LIT: every tile's uniform
    /// carries the same sun direction and night floor.
    ///
    /// Per frame, not per tile, exactly as `render::globe::GlobeUpload`
    /// hands it out — so this helper cannot accidentally test a per-tile sun
    /// that production does not have.
    #[allow(clippy::type_complexity)]
    fn lit_globe_draws(
        camera: &crate::render::Camera,
        level: u8,
        extent: u32,
        color: crate::style::Rgba,
        sun: glam::DVec3,
        night_brightness: f32,
    ) -> (Vec<crate::render::TileMesh>, Vec<([[f32; 4]; 4], GlobeParams)>) {
        let (meshes, mut uniforms) =
            globe_level_draws_in(camera, level, extent, color, |tile| {
                crate::render::subdivide::Subdivision::for_tile(tile)
            });
        assert!(
            (sun.length() - 1.0).abs() < 1e-9,
            "the sun direction must be a UNIT vector -- `shader.wgsl` dots it against a \
             normalised normal and calls the result a cosine. It was {}",
            sun.length()
        );
        for (_, params) in &mut uniforms {
            params.sun_direction = [sun.x as f32, sun.y as f32, sun.z as f32];
            params.night_brightness = night_brightness;
        }
        (meshes, uniforms)
    }

    /// Where a lon/lat on the sphere lands, in pixels, at `camera` — through
    /// the SAME `globe_view_proj` the vertex shader multiplies by, and
    /// nothing else.
    ///
    /// No horizontal flip, and that is the load-bearing detail Task 6's
    /// mirrored-label defect established: `shader.wgsl`'s `clip.x = -clip.x`
    /// applies to the MERCATOR half only, because `globe_frame` builds its
    /// matrix without `Camera::eye_target_up`'s basis and the sphere's `+x`
    /// is already screen right. So east is right here, as it is on screen.
    fn sphere_lon_lat_to_px(camera: &crate::render::Camera, lon: f64, lat: f64, size: u32) -> (f64, f64) {
        let p = crate::render::space::sky_direction(lon, lat);
        let clip = crate::render::globe::globe_frame(camera).globe_view_proj
            * glam::DVec4::new(p.x, p.y, p.z, 1.0);
        let (ndc_x, ndc_y) = (clip.x / clip.w, clip.y / clip.w);
        (
            (ndc_x + 1.0) * 0.5 * f64::from(size),
            // NDC y is up, a framebuffer row index is down.
            (1.0 - ndc_y) * 0.5 * f64::from(size),
        )
    }

    /// Renders one level of globe tiles and returns the RGBA readback.
    async fn render_globe_level(
        ctx: &GpuContext,
        pipeline: &TilePipeline,
        camera: &crate::render::Camera,
        size: u32,
        meshes: &[crate::render::TileMesh],
        uniforms: &[([[f32; 4]; 4], GlobeParams)],
    ) -> Vec<u8> {
        let color_texture = make_color_texture(ctx, size);
        let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let depth_view = pipeline.create_depth_view(ctx, size, size);
        let camera_bind_group = pipeline.test_camera_bind_group(
            ctx,
            camera.view_projection().to_cols_array_2d(),
            camera.zoom as f32,
        );
        let buffers: Vec<TileBuffers> =
            meshes.iter().map(|m| pipeline.create_tile_buffers(ctx, m)).collect();
        let groups: Vec<wgpu::BindGroup> = uniforms
            .iter()
            .map(|(matrix, globe)| {
                pipeline.create_model_bind_group(ctx, *matrix, 1.0, 0.0, *globe).1
            })
            .collect();
        let draws: Vec<(&TileBuffers, &wgpu::BindGroup)> =
            buffers.iter().zip(groups.iter()).collect();
        let mut encoder =
            ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        pipeline.render(
            &mut encoder,
            &color_view,
            &depth_view,
            wgpu::Color::TRANSPARENT,
            &camera_bind_group,
            &draws,
            None,
        );
        read_all_pixels(ctx, &color_texture, encoder, size).await
    }

    /// **TWO ADJACENT TILES DO NOT DOUBLE-COMPOSITE THEIR SHARED EDGE, LIT
    /// OR UNLIT.**
    ///
    /// # The report, and the gap this closes
    ///
    /// Reported as pale streaks along curved arcs -- "lines on meridians" --
    /// across large `landcover` areas at zoom 3-6, most visible where the
    /// day/night terminator has darkened everything around them. A tile edge
    /// at z3 IS a meridian or a parallel, and `landcover` in the shipped
    /// Mapbox style is a near-white wash at `fill-opacity` 0.3: composite
    /// that wash twice and 51 becomes 155 instead of 112, which is what a
    /// pale streak looks like.
    ///
    /// `a_translucent_fill_is_composited_exactly_once_per_pixel_on_the_globe`
    /// graded overdraw but its wash shapes lie wholly inside one tile, so a
    /// SEAM between two tiles was invisible to it by construction. This
    /// grades the seam: a whole level of tiles, each an opaque base with a
    /// tile-COVERING translucent wash over it, so every tile's wash runs
    /// right up to every one of its four edges and meets its neighbour's
    /// there.
    ///
    /// It also runs the sweep twice, unlit and then LIT with the sun placed
    /// 90 degrees of longitude from the camera so the terminator crosses the
    /// middle of the frame. The lit pass is not decoration: shading is
    /// interpolated per vertex (`globe_light.x`), so a tile boundary is
    /// where two tiles' interpolations meet, and a disagreement there would
    /// be a bright line along exactly the geometry the report describes and
    /// would only be visible near the terminator -- which is what the report
    /// says. A ridge scan (a pixel lighter than both its neighbours by more
    /// than one byte) catches that; the 112/155 band count catches the
    /// double composite.
    ///
    /// # Measured, on an Apple M-series adapter, at lon 88.8 lat 61 z3.14
    ///
    /// ```text
    ///              h ridges  v ridges  cracks  ONE composite  TWO OR MORE
    ///   unlit             0         0       0        262,144            0
    ///   lit, terminator   0         0       0        106,771            0
    /// ```
    ///
    /// 262,144 is 512x512: every pixel of the frame carries the wash exactly
    /// once. So the seam hypothesis is dead -- and the mechanism behind that
    /// is in `style::a_clipped_fill_never_leaves_its_own_tile_box`, which is
    /// the reason rather than the symptom.
    // `3.14` below is a ZOOM the report names, not an approximation of pi.
    // `clippy::approx_constant` cannot tell the two apart, and writing the
    // camera as anything other than the number the user typed would make the
    // fixture unreadable against the report it grades.
    #[allow(clippy::approx_constant)]
    #[test]
    fn adjacent_tiles_do_not_double_composite_their_shared_edge() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 512u32;
            let extent = 4096u32;
            for (lit, label) in [(false, "unlit"), (true, "lit")] {
                let camera = crate::render::Camera {
                    center_lon: 88.8,
                    center_lat: 61.0,
                    zoom: 3.14,
                    pitch: 0.0,
                    bearing: 0.0,
                    viewport_width_px: size as f32,
                    viewport_height_px: size as f32,
                    fov_y: crate::render::Camera::DEFAULT_FOV_Y,
                };
                let level = 3u8;
                let frame = crate::render::globe::globe_frame(&camera);
                let transition = crate::render::globe::transition_for_zoom(camera.zoom);
                let e = extent as i32;
                let ring = geo_types::LineString::from(vec![
                    (0, 0),
                    (e, 0),
                    (e, e),
                    (0, e),
                    (0, 0),
                ]);
                let grey = crate::style::Rgba { r: 0.2, g: 0.2, b: 0.2, a: 1.0 };
                let wash = crate::style::Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.3 };
                let whole_tile = std::sync::Arc::new(geo_types::Geometry::Polygon(
                    geo_types::Polygon::new(ring, Vec::new()),
                ));
                let plan = crate::style::PaintPlan {
                    ops: vec![
                        crate::style::PaintOp::Fill {
                            color_from: grey,
                            color_to: grey,
                            geometry: std::sync::Arc::clone(&whole_tile),
                        },
                        crate::style::PaintOp::Fill {
                            color_from: wash,
                            color_to: wash,
                            geometry: whole_tile,
                        },
                    ],
                };
                // The sun 90 degrees of longitude west of the camera, so the
                // terminator runs right through the middle of the view.
                let sun = crate::render::atmosphere::Sun::Map { lon_deg: -1.2, lat_deg: 0.0 }
                    .direction_sphere(&glam::DMat4::IDENTITY);
                let side = 1u32 << level;
                let mut meshes = Vec::new();
                let mut uniforms = Vec::new();
                for y in 0..side {
                    for x in 0..side {
                        let tile = crate::geo::TileId { z: level, x, y };
                        let tile_world_size =
                            crate::render::WORLD_SIZE / 2f32.powi(i32::from(level));
                        meshes.push(crate::render::tessellate_plan(
                            &plan,
                            extent,
                            crate::render::mesh::COORD_SCALE,
                            crate::render::subdivide::Subdivision::for_tile(tile),
                        ));
                        let params = GlobeParams {
                            globe_matrix: frame.globe_view_proj.as_mat4().to_cols_array_2d(),
                            tile_mercator_coords: crate::render::globe::tile_mercator_coords(tile),
                            clipping_plane: frame.clipping_plane.map(|c| c as f32),
                            transition,
                            lyon_to_tile_unit: 1.0
                                / (crate::render::mesh::COORD_SCALE * extent as f32),
                            tile_units_per_px: 1.0
                                / (tile_world_size * 2f32.powf(camera.zoom as f32)),
                            sun_direction: [sun.x as f32, sun.y as f32, sun.z as f32],
                            night_brightness: 0.25,
                        };
                        uniforms.push((
                            crate::render::combined_tile_matrix(&camera, tile, extent as f32)
                                .to_cols_array_2d(),
                            if lit { params } else { params.unlit() },
                        ));
                    }
                }
                let data =
                    render_globe_level(&ctx, &pipeline, &camera, size, &meshes, &uniforms).await;
                // A streak is a pixel LIGHTER than both of its neighbours on
                // one axis, by more than a rounding step.
                let at = |c: u32, r: u32| rgb_at(&data, size, c, r)[0];
                let mut h_ridges: Vec<(u32, u32, u8, u8, u8)> = Vec::new();
                let mut v_ridges: Vec<(u32, u32, u8, u8, u8)> = Vec::new();
                let mut dark_holes = 0usize;
                for r in 1..size - 1 {
                    for c in 1..size - 1 {
                        let (m, l, rr, u, d) =
                            (at(c, r), at(c - 1, r), at(c + 1, r), at(c, r - 1), at(c, r + 1));
                        if m == 0 {
                            continue;
                        }
                        if l > 0 && rr > 0 && m > l + 1 && m > rr + 1 {
                            h_ridges.push((c, r, l, m, rr));
                        }
                        if u > 0 && d > 0 && m > u + 1 && m > d + 1 {
                            v_ridges.push((c, r, u, m, d));
                        }
                        if l > 0 && rr > 0 && l > m + 1 && rr > m + 1 {
                            dark_holes += 1;
                        }
                    }
                }
                let worst_h = h_ridges.iter().map(|t| t.3 - t.2.max(t.4)).max().unwrap_or(0);
                let worst_v = v_ridges.iter().map(|t| t.3 - t.2.max(t.4)).max().unwrap_or(0);
                // The unlit arithmetic: base 51, one composite 112, two 155.
                let mut once = 0usize;
                let mut twice = 0usize;
                for r in 0..size {
                    for c in 0..size {
                        match i32::from(at(c, r)) {
                            105..=119 => once += 1,
                            v if v >= 140 => twice += 1,
                            _ => {}
                        }
                    }
                }
                println!(
                    "{label}: horizontal ridges {} (worst +{worst_h}), vertical ridges {} \
                     (worst +{worst_v}), dark holes {dark_holes}, once {once}, TWICE+ {twice}; \
                     sample h {:?} v {:?}",
                    h_ridges.len(),
                    v_ridges.len(),
                    &h_ridges[..h_ridges.len().min(10)],
                    &v_ridges[..v_ridges.len().min(10)],
                );
                assert!(
                    once > 100_000,
                    "{label}: only {once} pixels carry exactly one composite, so this \
                     pass grades nothing"
                );
                assert_eq!(
                    twice, 0,
                    "{label}: {twice} pixels carry the translucent wash TWICE or more. \
                     Two tiles' washes meet only along their shared edge, so a doubled \
                     pixel there is a seam."
                );
                assert!(
                    h_ridges.is_empty() && v_ridges.is_empty(),
                    "{label}: {} horizontal and {} vertical pixels are lighter than BOTH \
                     their neighbours (worst +{worst_h} / +{worst_v}). On a smoothly \
                     shaded sphere covered by one uniform wash there is no such pixel; \
                     a run of them along a tile edge is the reported streak. First few: \
                     h {:?} v {:?}",
                    h_ridges.len(),
                    v_ridges.len(),
                    &h_ridges[..h_ridges.len().min(10)],
                    &v_ridges[..v_ridges.len().min(10)],
                );
                assert_eq!(
                    dark_holes, 0,
                    "{label}: {dark_holes} pixels are darker than both their neighbours \
                     -- a crack between two tiles, which is the same defect with the \
                     sign flipped"
                );
            }
        });
    }

    /// **THE SHADED GLOBE IS THE SAME PICTURE HOWEVER FINE THE SUBDIVISION
    /// GRID IS.**
    ///
    /// # What this grades that nothing else does
    ///
    /// `adjacent_tiles_do_not_double_composite_their_shared_edge` scans for
    /// a pixel LIGHTER THAN BOTH ITS NEIGHBOURS. That finds a step, and a
    /// step is what a crack or a double composite makes. It cannot find a
    /// KINK -- a place where the brightness is continuous but its slope is
    /// not -- because a kink is nowhere a local maximum. Every ridge scan
    /// this project has run is blind to a kink by construction, and a kink
    /// on a smooth gradient is exactly what the eye reports as a pale line
    /// (a Mach band).
    ///
    /// And a kink is what per-vertex lighting makes. `shader.wgsl` computes
    /// `globe_light.x = dot(normalize(sphere_pos), sun)` PER VERTEX and lets
    /// the rasterizer interpolate it, so the lit surface is a piecewise
    /// LINEAR approximation of a cosine, with its breaks on the edges of
    /// `render::subdivide`'s cells -- which on the globe are parallels and
    /// meridians.
    ///
    /// **This is also the one hypothesis the comparison against Mapbox GL JS
    /// cannot falsify**, because Mapbox GL JS does not shade the ground at
    /// all (see `TERMINATOR_SOFTNESS` in `shader.wgsl`: "there is no
    /// terminator softness anywhere in MapLibre's shaders to copy"). A clean
    /// Mapbox frame is evidence about geometry, not about a term Mapbox
    /// never computes.
    ///
    /// # The instrument
    ///
    /// Render the same frame twice, changing NOTHING but the subdivision
    /// granularity (the production 8 that `Subdivision::for_tile` gives a z3
    /// tile, against 32). Then measure only where the two meshes cover the
    /// same pixels with the same flat colour -- a mask built from the UNLIT
    /// pair, keeping a pixel only when it and all eight of its neighbours
    /// read identically in both. On that mask the geometry is provably
    /// equal, so any difference in the LIT pair is the lighting term alone,
    /// and its size is the size of the kink.
    ///
    /// The wash is deliberately NOT tile-shaped: a tile-covering polygon
    /// subdivides to the reference grid, where every vertex is a cell corner
    /// and the case where a clip inserts a vertex never arises.
    #[allow(clippy::approx_constant)]
    #[test]
    #[cfg_attr(not(target_os = "macos"), ignore = "rasterizer precision differs from Metal's (fails on lavapipe)")]
    fn the_shaded_globe_is_the_same_picture_at_any_subdivision_granularity() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 512u32;
            let extent = 4096u32;
            let camera = crate::render::Camera {
                center_lon: 88.8,
                center_lat: 61.0,
                zoom: 3.14,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: size as f32,
                viewport_height_px: size as f32,
                fov_y: crate::render::Camera::DEFAULT_FOV_Y,
            };
            let level = 3u8;
            let frame = crate::render::globe::globe_frame(&camera);
            let transition = crate::render::globe::transition_for_zoom(camera.zoom);
            let e = extent as i32;
            let whole_tile = std::sync::Arc::new(geo_types::Geometry::Polygon(
                geo_types::Polygon::new(
                    geo_types::LineString::from(vec![(0, 0), (e, 0), (e, e), (0, e), (0, 0)]),
                    Vec::new(),
                ),
            ));
            // Off the grid on purpose: at granularity 8 a cell is 512 extent
            // units, and none of these coordinates is a multiple of 512, of
            // 128, or of each other.
            let off_grid = |f: f64| (f * f64::from(e)) as i32;
            let wash_ring = geo_types::LineString::from(vec![
                (off_grid(0.037), off_grid(0.113)),
                (off_grid(0.941), off_grid(0.059)),
                (off_grid(0.977), off_grid(0.859)),
                (off_grid(0.613), off_grid(0.937)),
                (off_grid(0.069), off_grid(0.881)),
                (off_grid(0.037), off_grid(0.113)),
            ]);
            let wash_shape = std::sync::Arc::new(geo_types::Geometry::Polygon(
                geo_types::Polygon::new(wash_ring, Vec::new()),
            ));
            // Near-white, like the `landcover` the report names: a 1-part-in-
            // 255 kink is a bigger absolute step on a bright surface, and
            // this test is looking for a kink about that size.
            let pale = crate::style::Rgba { r: 0.85, g: 0.85, b: 0.85, a: 1.0 };
            let wash = crate::style::Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.3 };
            let plan = crate::style::PaintPlan {
                ops: vec![
                    crate::style::PaintOp::Fill {
                        color_from: pale,
                        color_to: pale,
                        geometry: whole_tile,
                    },
                    crate::style::PaintOp::Fill {
                        color_from: wash,
                        color_to: wash,
                        geometry: wash_shape,
                    },
                ],
            };
            let sun = crate::render::atmosphere::Sun::Map { lon_deg: -1.2, lat_deg: 0.0 }
                .direction_sphere(&glam::DMat4::IDENTITY);
            let side = 1u32 << level;
            // Borrowed once, so the closure below is `Fn` and not `FnOnce`:
            // a shared reference is `Copy`, the things it points at are not.
            let (ctx, pipeline, camera, frame, plan) = (&ctx, &pipeline, &camera, &frame, &plan);
            let render = |granularity: u32, lit: bool| {
                async move {
                    let mut meshes = Vec::new();
                    let mut uniforms = Vec::new();
                    for y in 0..side {
                        for x in 0..side {
                            let tile = crate::geo::TileId { z: level, x, y };
                            let tile_world_size =
                                crate::render::WORLD_SIZE / 2f32.powi(i32::from(level));
                            let production = crate::render::subdivide::Subdivision::for_tile(tile);
                            meshes.push(crate::render::tessellate_plan(
                                plan,
                                extent,
                                crate::render::mesh::COORD_SCALE,
                                crate::render::subdivide::Subdivision { granularity, ..production },
                            ));
                            let params = GlobeParams {
                                globe_matrix: frame.globe_view_proj.as_mat4().to_cols_array_2d(),
                                tile_mercator_coords: crate::render::globe::tile_mercator_coords(
                                    tile,
                                ),
                                clipping_plane: frame.clipping_plane.map(|c| c as f32),
                                transition,
                                lyon_to_tile_unit: 1.0
                                    / (crate::render::mesh::COORD_SCALE * extent as f32),
                                tile_units_per_px: 1.0
                                    / (tile_world_size * 2f32.powf(camera.zoom as f32)),
                                sun_direction: [sun.x as f32, sun.y as f32, sun.z as f32],
                                night_brightness: 0.25,
                            };
                            uniforms.push((
                                crate::render::combined_tile_matrix(camera, tile, extent as f32)
                                    .to_cols_array_2d(),
                                if lit { params } else { params.unlit() },
                            ));
                        }
                    }
                    render_globe_level(ctx, pipeline, camera, size, &meshes, &uniforms).await
                }
            };
            const COARSE: u32 = 8; // what `Subdivision::for_tile` gives a z3 tile
            const FINE: u32 = 32;
            let unlit_coarse = render(COARSE, false).await;
            let unlit_fine = render(FINE, false).await;
            let lit_coarse = render(COARSE, true).await;
            let lit_fine = render(FINE, true).await;

            let value = |data: &[u8], c: u32, r: u32| i32::from(rgb_at(data, size, c, r)[0]);
            let mut graded = 0usize;
            let mut worst = 0i32;
            let mut over = [0usize; 4]; // differing by >=1, >=2, >=3, >=4
            let mut first: Vec<(u32, u32, i32, i32)> = Vec::new();
            for r in 1..size - 1 {
                for c in 1..size - 1 {
                    // The mask: this pixel and all eight neighbours read the
                    // same in both UNLIT frames, so the two meshes cover them
                    // with the same colour and no edge is near.
                    let flat = (0..9).all(|n| {
                        let (dc, dr) = (n % 3 - 1, n / 3 - 1);
                        let (nc, nr) = ((c as i32 + dc) as u32, (r as i32 + dr) as u32);
                        value(&unlit_coarse, nc, nr) == value(&unlit_coarse, c, r)
                            && value(&unlit_fine, nc, nr) == value(&unlit_coarse, c, r)
                    });
                    if !flat || value(&unlit_coarse, c, r) == 0 {
                        continue;
                    }
                    graded += 1;
                    let delta = (value(&lit_coarse, c, r) - value(&lit_fine, c, r)).abs();
                    for (step, count) in over.iter_mut().enumerate() {
                        if delta > step as i32 {
                            *count += 1;
                        }
                    }
                    if delta > worst {
                        worst = delta;
                    }
                    if delta >= 2 && first.len() < 10 {
                        first.push((c, r, value(&lit_coarse, c, r), value(&lit_fine, c, r)));
                    }
                }
            }
            println!(
                "granularity {COARSE} against {FINE}: {graded} pixels graded (same geometry, \
                 same flat colour), worst lit difference {worst} of 255; differing by \
                 >=1: {}, >=2: {}, >=3: {}, >=4: {}; first {first:?}",
                over[0], over[1], over[2], over[3],
            );
            assert!(
                graded > 20_000,
                "only {graded} pixels are gradeable, so this pass measures nothing"
            );
            assert!(
                worst <= 1,
                "the lit globe changes by up to {worst} of 255 when only the subdivision \
                 granularity changes, on {graded} pixels the two meshes cover identically \
                 ({} pixels differ by 2 or more). The surface is shaded from a per-vertex \
                 `dot(normal, sun)` interpolated across each triangle, so the brightness \
                 has a KINK on every cell boundary -- a parallel or a meridian -- and the \
                 coarser the grid the bigger it is. That is a pale line on a smooth \
                 gradient, visible only where the terminator puts the gradient there. \
                 Samples (col, row, coarse, fine): {first:?}",
                over[1],
            );
        });
    }

    /// **GROUND BEHIND THE HORIZON PAINTS NOTHING, AT EVERY TRANSITION --
    /// NOT JUST ON THE PURE GLOBE.**
    ///
    /// # The defect this closes
    ///
    /// Reported as "some lens-based effect": a thin grey arc running
    /// parallel to the globe's limb and slightly INSIDE it, over otherwise
    /// correct map content; at whole-globe zoom it hugs the limb, and zoomed
    /// in it becomes two arcs at the left and right edges of the viewport --
    /// opposite sides of one circle larger than the screen.
    ///
    /// That is what the BACK of the planet looks like when it is drawn.
    /// Perspective compresses everything beyond the horizon into a narrow
    /// annulus just inside the limb, so a far-side leak of any size shows up
    /// as an arc there and nowhere else.
    ///
    /// The leak was in `shader.wgsl`'s `interpolate_projection`: over the
    /// whole blended band it scaled the far-side test's clip-space z by the
    /// transition ramp, and scaling a side-of-plane test moves the plane
    /// rather than fading it (the full argument, and the `1/k` table, are in
    /// that function). This test is graded against the tiles the crate's OWN
    /// cull has already decided are behind the horizon -- if the shader
    /// agreed with `GlobeCover`, those tiles could not paint a pixel even if
    /// they were drawn, and here they are deliberately drawn.
    ///
    /// # Why a fixture of whole hidden TILES and not a band of ground
    ///
    /// An earlier attempt at this measurement used a lon/lat band placed
    /// just behind the tangent plane and found nothing, for a reason it
    /// wrote down: at high transition that band is sub-pixel, and at low
    /// transition `interpolate_projection`'s `xyw` blend drags it off the
    /// viewport entirely, so the fixture could not have shown a leak of any
    /// size. Whole tiles have no such escape route -- a z3 tile is 45
    /// degrees of the planet and the ones behind the horizon at these
    /// cameras project into the disc, on screen, at every transition in the
    /// band.
    ///
    /// # Measured, on an Apple M-series adapter, 512x512, BEFORE the fix
    ///
    /// ```text
    ///   camera (lon 88.8, lat 61)  transition  hidden tiles  PIXELS PAINTED
    ///   z3.0  L3                       1.0000            48               0
    ///   z3.14 L3                       0.9937            50               0
    ///   z3.48 L3                       0.9314            52               0
    ///   z4.0  L4                       0.7407           240             175
    /// ```
    ///
    /// and the 175 pixels are **a ring, at radius 254.0 to 281.6 px from
    /// the centre of a 512 px frame** -- which is the report, in numbers.
    /// The run stops at the first camera that fails, so the two cameras past
    /// z4 were never reached before; after the fix every row reads zero.
    ///
    /// The z3.0 row is the control: `transition == 1.0` takes
    /// `interpolate_projection`'s pure-globe early return, which never had
    /// the defect, so it read zero before as well as after. The two rows
    /// either side of it read zero before the fix too, and that is the
    /// fixture being conservative rather than the defect being absent: at
    /// `transition` 0.99 the plane moves by `1/k = 1.008`, which is less far
    /// past the horizon than a WHOLE z3 tile lies, so the leak there belongs
    /// to the far EDGE of a tile that straddles the horizon and is not in
    /// this fixture. Those rows are still worth their runtime -- they are
    /// what would catch a fix that clipped too much at high transition.
    // `3.14` in the sweep below is a ZOOM the report names, not an
    // approximation of pi -- see the note on
    // `adjacent_tiles_do_not_double_composite_their_shared_edge`.
    #[allow(clippy::approx_constant)]
    #[test]
    fn no_tile_behind_the_horizon_paints_a_pixel_at_any_transition() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 512u32;
            let extent = 4096u32;
            let e = extent as i32;
            let magenta = crate::style::Rgba { r: 1.0, g: 0.0, b: 1.0, a: 1.0 };
            let plan = crate::style::PaintPlan {
                ops: vec![crate::style::PaintOp::Fill {
                    color_from: magenta,
                    color_to: magenta,
                    geometry: std::sync::Arc::new(geo_types::Geometry::Polygon(
                        geo_types::Polygon::new(
                            geo_types::LineString::from(vec![
                                (0, 0),
                                (e, 0),
                                (e, e),
                                (0, e),
                                (0, 0),
                            ]),
                            Vec::new(),
                        ),
                    )),
                }],
            };
            // Every camera here has `transition > 0.2`, i.e. a non-zero
            // `clip_ramp` in `interpolate_projection` -- the band in which the
            // far-side clip is supposed to be doing something. Below 0.2 the
            // reference turns it off entirely and so do we, deliberately: at
            // 20% globeness there is no limb on screen to leak past.
            for (zoom, level) in
                [(3.0f64, 3u8), (3.14, 3), (3.48, 3), (4.0, 4), (4.5, 4), (5.0, 4)]
            {
                let camera = crate::render::Camera {
                    center_lon: 88.8,
                    center_lat: 61.0,
                    zoom,
                    pitch: 0.0,
                    bearing: 0.0,
                    viewport_width_px: size as f32,
                    viewport_height_px: size as f32,
                    fov_y: crate::render::Camera::DEFAULT_FOV_Y,
                };
                let frame = crate::render::globe::globe_frame(&camera);
                let cover = crate::render::globe::GlobeCover::new(&frame);
                let transition = crate::render::globe::transition_for_zoom(zoom);
                let side = 1u32 << level;
                let mut meshes = Vec::new();
                let mut uniforms = Vec::new();
                for y in 0..side {
                    for x in 0..side {
                        let tile = crate::geo::TileId { z: level, x, y };
                        if cover.tile_visible(tile) {
                            continue;
                        }
                        let tile_world_size =
                            crate::render::WORLD_SIZE / 2f32.powi(i32::from(level));
                        meshes.push(crate::render::tessellate_plan(
                            &plan,
                            extent,
                            crate::render::mesh::COORD_SCALE,
                            crate::render::subdivide::Subdivision::for_tile(tile),
                        ));
                        uniforms.push((
                            crate::render::combined_tile_matrix(&camera, tile, extent as f32)
                                .to_cols_array_2d(),
                            GlobeParams {
                                globe_matrix: frame
                                    .globe_view_proj
                                    .as_mat4()
                                    .to_cols_array_2d(),
                                tile_mercator_coords:
                                    crate::render::globe::tile_mercator_coords(tile),
                                clipping_plane: frame.clipping_plane.map(|c| c as f32),
                                transition,
                                lyon_to_tile_unit: 1.0
                                    / (crate::render::mesh::COORD_SCALE * extent as f32),
                                tile_units_per_px: 1.0
                                    / (tile_world_size * 2f32.powf(camera.zoom as f32)),
                                ..GlobeParams::FLAT
                            }
                            .unlit(),
                        ));
                    }
                }
                let hidden = meshes.len();
                let data =
                    render_globe_level(&ctx, &pipeline, &camera, size, &meshes, &uniforms).await;
                let mut painted = 0usize;
                let (mut min_r, mut max_r) = (f64::INFINITY, 0.0f64);
                for row in 0..size {
                    for col in 0..size {
                        if data[((row * size + col) * 4 + 3) as usize] > 0 {
                            painted += 1;
                            let (dx, dy) =
                                (col as f64 - 255.5, row as f64 - 255.5);
                            let r = (dx * dx + dy * dy).sqrt();
                            min_r = min_r.min(r);
                            max_r = max_r.max(r);
                        }
                    }
                }
                println!(
                    "z{zoom} L{level} (transition {transition:.4}): {hidden} hidden tiles \
                     painted {painted} px, radius {min_r:.1}..{max_r:.1}"
                );
                assert!(
                    hidden > 0,
                    "z{zoom}: no tile at level {level} is behind the horizon at this \
                     camera, so this row grades nothing"
                );
                assert_eq!(
                    painted, 0,
                    "z{zoom} (transition {transition:.4}): {hidden} tiles the cull says \
                     are behind the horizon painted {painted} pixels, in a ring at radius \
                     {min_r:.1}..{max_r:.1} px from the centre of a 512 px frame. Ground \
                     past the horizon projects into a thin annulus just inside the limb, \
                     which is the arc the report describes."
                );
            }
        });
    }

    /// **THE ACCEPTANCE CRITERION FOR TASK 5: after subdivision the drawn
    /// silhouette follows a CIRCLE, and a coarser grid measurably does not.**
    ///
    /// `at_full_transition_the_world_is_drawn_as_a_disc_and_not_a_rectangle`
    /// established that a globe is a disc and not a rectangle, using a mesh
    /// it subdivided BY HAND (`tile_covering_grid(64, ...)`) precisely
    /// because nothing else did yet. This is the same camera and the same
    /// analytic disc, with the hand-built grid replaced by real geometry
    /// through the real bake -- `tessellate_plan` over a tile-covering
    /// polygon, at `render::subdivide::Subdivision::for_tile` -- and it
    /// measures the LIMB rather than just the four corners, which is the
    /// only thing that can tell a polygon from a circle.
    ///
    /// # The camera, and the circle it predicts
    ///
    /// Identical to that test's: lon 0, lat 0, z1.5, 512x512.
    /// `globe_radius_world` is `WORLD_SIZE / 2*PI` = 81.487 world units at
    /// the equator, `Camera::distance()` at z1.5 on a 512 px viewport is
    /// 156.767, so the eye is 238.254 from the sphere's centre, the tangent
    /// cone's half-angle is `asin(81.487 / 238.254)` = 20.000 degrees, and
    /// with a 60-degree vertical FOV the silhouette sits at
    /// `256 * tan(20) / tan(30)` = **161.385 px**, for a disc of
    /// **81,823 px**. Recomputed below rather than quoted, so a change in
    /// `render::globe`'s camera chain fails loudly here instead of silently
    /// re-grading the test. (That older test quotes 161.37/81,806, computed
    /// through rounded intermediates -- 0.01% and 0.02% below these.)
    ///
    /// The tiles are the four of **z1**, not the single z0 tile, so that the
    /// per-tile `tile_mercator_coords` and the seams between tiles are in
    /// play -- a subdivision watertight within a tile but cracked between
    /// them would pass a one-tile test.
    ///
    /// # Three meshes, one camera, and why the middle one is the real test
    ///
    /// For each of 120 rays from the disc centre the outermost lit radius is
    /// found, and compared with 161.385. Measured on an Apple M-series
    /// adapter:
    ///
    /// ```text
    ///   mesh                          lit px   vs 81,823   worst ray error
    ///   Subdivision::NONE (g=1)            0     -100.0%       161.38 px
    ///   granularity 4                 70,194      -14.2%        25.00 px
    ///   Subdivision::for_tile (g=32)  81,676       -0.2%         1.50 px
    /// ```
    ///
    /// **`granularity 4` is the discriminating control, not `NONE`.** The
    /// unsubdivided mesh draws literally nothing at this camera, and the
    /// reason is worth writing down: each z1 tile is two triangles whose
    /// corners are at lon +/-180 or 0 and lat 0 or +/-85.051, exactly one of
    /// which (lon 0, lat 0) is on the near side. The chords from it to the
    /// other three pass through the middle of the planet, so they project
    /// almost onto the disc's own centre and each triangle collapses to a
    /// sliver of sub-pixel area. A test whose "before" is blank cannot tell
    /// a polygon from a circle -- it can only tell drawing from not drawing.
    ///
    /// At granularity 4 the mesh really is a visible polygon, 25.00 px away
    /// from the circle at its worst ray -- a **16.7x** improvement to
    /// 1.50 px at granularity 32. Both measured figures sit ABOVE the
    /// purely longitudinal chord formula (`r * (1 - cos(alpha/2))`, which
    /// gives 12.28 px at a 45-degree cell and 0.19 px at a 5.625-degree
    /// one), and for two reasons worth recording rather than filing off:
    ///
    ///  - **a Mercator grid's LATITUDE spacing is far coarser than its
    ///    longitude spacing at low granularity.** At granularity 4 a z1
    ///    tile's four rows land at latitude 85.051, 79.171, 66.511, 40.980
    ///    and 0 -- a 25.5-degree step right where the limb crosses (70
    ///    degrees from the camera's look-at), against the 45-degree
    ///    longitude step. The limb also crosses cells diagonally, so the
    ///    relevant chord is neither of the two axes alone.
    ///  - **pixel quantisation is a floor of about 1 px.** The march steps
    ///    a quarter of a pixel and rounds to an integer pixel, and a pixel
    ///    counts as lit only when its centre is covered, so the outermost
    ///    lit pixel sits up to about 0.7 px inside the true edge. That
    ///    floor, not the chord, is what the 1.50 px figure is mostly made
    ///    of -- which is the honest reason the tolerance below is 3 px and
    ///    not 0.5.
    ///
    /// So the assertions bracket the two: under 3 px for the real
    /// granularity, over 6 px for granularity 4. A tolerance both passed
    /// would prove nothing.
    #[test]
    fn subdivision_makes_the_globes_limb_a_circle() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 512u32;
            let extent = 4096u32;
            let camera = crate::render::Camera {
                center_lon: 0.0,
                center_lat: 0.0,
                zoom: 1.5,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: size as f32,
                viewport_height_px: size as f32,
                fov_y: crate::render::Camera::DEFAULT_FOV_Y,
            };
            let radius_world = f64::from(crate::render::WORLD_SIZE) / std::f64::consts::TAU;
            let eye = camera.distance() + radius_world;
            let half_angle = (radius_world / eye).asin();
            let expected_radius =
                f64::from(size) / 2.0 * half_angle.tan() / std::f64::consts::FRAC_PI_6.tan();
            let expected_area = std::f64::consts::PI * expected_radius * expected_radius;
            assert!(
                (expected_radius - 161.385).abs() < 0.01 && (expected_area - 81_823.0).abs() < 6.0,
                "the analytic disc this test is graded against moved: radius {expected_radius}, \
                 area {expected_area}. Either `render::globe`'s camera chain changed or \
                 `Camera::distance` did -- read this test's doc comment before touching the \
                 tolerances below"
            );

            let bytes_per_row = (size * 4).next_multiple_of(256);
            let covered = |data: &[u8], col: i64, row: i64| -> bool {
                if col < 0 || row < 0 || col >= i64::from(size) || row >= i64::from(size) {
                    return false;
                }
                let offset = (row as u32 * bytes_per_row + col as u32 * 4) as usize;
                data[offset + 3] > 20
            };

            // Three meshes from one geometry and one camera. `None` is the
            // unsubdivided control, `Some(4)` the coarse-but-visible polygon
            // that makes the roundness metric discriminating, and `None`'s
            // sibling `for_tile` the real thing. See the doc comment.
            let mut results = Vec::new();
            for coarse in [Some(1u32), Some(4), None] {
                let (meshes, uniforms) = globe_level_draws(&camera, 1, extent, |tile| {
                    let full = crate::render::subdivide::Subdivision::for_tile(tile);
                    match coarse {
                        Some(granularity) => {
                            crate::render::subdivide::Subdivision { granularity, ..full }
                        }
                        None => full,
                    }
                });
                let data =
                    render_globe_level(&ctx, &pipeline, &camera, size, &meshes, &uniforms).await;
                let lit = (0..size)
                    .flat_map(|row| (0..size).map(move |col| (col, row)))
                    .filter(|(c, r)| covered(&data, i64::from(*c), i64::from(*r)))
                    .count();
                // The outermost lit radius along each of 120 rays. Marched
                // from the OUTSIDE in, so a ray that crosses a gap further
                // in still reports the silhouette.
                let centre = f64::from(size) / 2.0;
                let mut worst_error = 0.0f64;
                for step in 0..120 {
                    let angle = std::f64::consts::TAU * f64::from(step) / 120.0;
                    let (dx, dy) = (angle.cos(), angle.sin());
                    let mut found = 0.0f64;
                    let mut r = expected_radius + 8.0;
                    while r > 0.0 {
                        let col = (centre + dx * r).round() as i64;
                        let row = (centre + dy * r).round() as i64;
                        if covered(&data, col, row) {
                            found = r;
                            break;
                        }
                        r -= 0.25;
                    }
                    worst_error = worst_error.max((found - expected_radius).abs());
                }
                results.push((lit, worst_error));
            }
            let (bare_lit, _bare_worst) = results[0];
            let (coarse_lit, coarse_worst) = results[1];
            let (round_lit, round_worst) = results[2];

            // 1. The silhouette is measurably ROUND. 3 px of tolerance on a
            //    161 px radius, against a predicted 0.19 px of chord error
            //    plus the quarter-pixel march.
            assert!(
                round_worst < 3.0,
                "the subdivided limb must follow the analytic circle of radius \
                 {expected_radius:.2} px to within 3 px; worst of 120 rays was \
                 {round_worst:.2} px out"
            );
            // 2. ...and a coarser grid measurably is NOT. This is the
            //    assertion that makes the one above mean something: the
            //    method can resolve a polygon when there is one.
            assert!(
                coarse_worst > 6.0,
                "granularity 4 must FAIL the roundness check -- a 45-degree cell is 12.28 px of \
                 chord error at this radius, and a method that cannot see that cannot claim to \
                 have seen the circle either. Worst ray {coarse_worst:.2} px out"
            );
            // 3. The lit area gets monotonically closer to the analytic disc
            //    as the grid gets finer, which is the plan's own wording for
            //    the acceptance criterion.
            let gap = |lit: usize| (lit as f64 - expected_area).abs();
            assert!(
                gap(round_lit) < gap(coarse_lit) && gap(coarse_lit) < gap(bare_lit),
                "the lit area must approach the analytic disc {expected_area:.0} as granularity \
                 rises: g1 {bare_lit} (off by {:.0}), g4 {coarse_lit} (off by {:.0}), g32 \
                 {round_lit} (off by {:.0})",
                gap(bare_lit),
                gap(coarse_lit),
                gap(round_lit)
            );
            // 4. And close in absolute terms, not merely closer: within 1%
            //    of the analytic area.
            assert!(
                gap(round_lit) / expected_area < 0.01,
                "the subdivided disc must be within 1% of {expected_area:.0} px; got \
                 {round_lit} ({:+.2}%)",
                100.0 * (round_lit as f64 / expected_area - 1.0)
            );
        });
    }

    /// **The sphere CLOSES at the poles**: with pole caps there is no
    /// background left where Mercator runs out at +/-85.051 degrees.
    ///
    /// Task 2 deliberately shipped no pole handling, and the consequence was
    /// visible -- a globe seen from over a pole had a hole in it, and seen
    /// from the side it was a truncated barrel. `render::subdivide` grows a
    /// cap from every fill edge on a pole-row tile's boundary; this is the
    /// test that the cap actually covers the hole.
    ///
    /// # How the hole is located, rather than guessed
    ///
    /// The camera looks straight down at a pole (lat +/-85, as far as
    /// Mercator goes), and the pole's screen position is computed by pushing
    /// the sphere point `(0, +/-1, 0)` through the very matrix the shader
    /// uses, `GlobeFrame::globe_view_proj`. Every pixel within 10 px of that
    /// point must be covered.
    ///
    /// The frame is NOT a whole globe, and that is worth stating because the
    /// obvious mental picture is wrong. `render::globe::globe_radius_world`
    /// carries MapLibre's `mercatorScale(lat)` factor -- the sphere grows by
    /// `1 / cos(lat)` so a feature at the map centre is the same size in
    /// pixels on the globe as on the flat map -- and `1 / cos(85 deg)` is
    /// **11.47**. So at z1.5 this camera shows a magnified patch of the
    /// polar region, the look-at projects to the screen centre (256, 256),
    /// and the pole projects **225.4 px** from it, at (256.0, 30.6) for
    /// north and (256.0, 481.4) for south -- measured, inside the frame,
    /// which is what the guard below asserts before sampling anything.
    ///
    /// At that scale everything north of latitude 85.0511 is missing
    /// geometry, so the hole is large and the 10 px disc around the pole is
    /// deep inside it. Measured: **317 of 317 sampled pixels lit with the
    /// caps, 0 of 317 without**, at both poles. Zero, not "a few": without
    /// a cap the polygon simply stops at the tile edge, so the hole is a
    /// clean window onto the background.
    ///
    /// The control is the same mesh with `north_pole`/`south_pole` cleared
    /// and the grid left alone, so the only difference between the two
    /// halves is the cap itself -- and the screen centre, which is real tile
    /// geometry either way, is asserted lit in BOTH, so a failure cannot be
    /// "the globe was not drawn at all".
    #[test]
    fn the_pole_caps_close_the_sphere() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 512u32;
            let extent = 4096u32;
            let bytes_per_row = (size * 4).next_multiple_of(256);

            for (name, lat, pole_y) in [("north", 85.0f64, 1.0f32), ("south", -85.0, -1.0)] {
                let camera = crate::render::Camera {
                    center_lon: 0.0,
                    center_lat: lat,
                    zoom: 1.5,
                    pitch: 0.0,
                    bearing: 0.0,
                    viewport_width_px: size as f32,
                    viewport_height_px: size as f32,
                    fov_y: crate::render::Camera::DEFAULT_FOV_Y,
                };
                let frame = crate::render::globe::globe_frame(&camera);
                // The pole, through the shader's own matrix. `+y` is north
                // (`render::globe::mercator_to_sphere`).
                let clip =
                    frame.globe_view_proj.as_mat4() * glam::Vec4::new(0.0, pole_y, 0.0, 1.0);
                let ndc = clip.truncate() / clip.w;
                let pole_col = (ndc.x + 1.0) * 0.5 * size as f32;
                let pole_row = (1.0 - ndc.y) * 0.5 * size as f32;
                assert!(
                    pole_col > 16.0
                        && pole_col < size as f32 - 16.0
                        && pole_row > 16.0
                        && pole_row < size as f32 - 16.0,
                    "{name}: the pole must project inside the frame to be testable, got \
                     ({pole_col}, {pole_row})"
                );

                let mut lit_counts = Vec::new();
                let mut centre_lit = Vec::new();
                for capped in [false, true] {
                    let (meshes, uniforms) = globe_level_draws(&camera, 1, extent, |tile| {
                        let full = crate::render::subdivide::Subdivision::for_tile(tile);
                        if capped {
                            full
                        } else {
                            crate::render::subdivide::Subdivision {
                                north_pole: false,
                                south_pole: false,
                                ..full
                            }
                        }
                    });
                    let data =
                        render_globe_level(&ctx, &pipeline, &camera, size, &meshes, &uniforms)
                            .await;
                    let mut total = 0usize;
                    let mut lit = 0usize;
                    for row in -10i32..=10 {
                        for col in -10i32..=10 {
                            if col * col + row * row > 100 {
                                continue;
                            }
                            total += 1;
                            let c = (pole_col.round() as i32 + col) as u32;
                            let r = (pole_row.round() as i32 + row) as u32;
                            let offset = (r * bytes_per_row + c * 4) as usize;
                            if data[offset + 3] > 20 {
                                lit += 1;
                            }
                        }
                    }
                    lit_counts.push((lit, total));
                    let centre = ((size / 2) * bytes_per_row + (size / 2) * 4) as usize;
                    centre_lit.push(data[centre + 3] > 20);
                }
                assert!(
                    centre_lit[0] && centre_lit[1],
                    "{name}: the screen centre is the camera's look-at, which is inside the tile \
                     geometry at latitude {lat} -- it must be drawn with and without the caps, \
                     or the globe is not being drawn at all and the counts below mean nothing"
                );
                let (uncapped, total) = lit_counts[0];
                let (capped, _) = lit_counts[1];
                assert_eq!(
                    capped, total,
                    "{name}: EVERY pixel within 10 px of the pole must be drawn once the cap \
                     exists -- {capped} of {total} were. A partial count means the cap is being \
                     grown from only some of the tile's boundary edges, or that its pole vertex \
                     is not reaching `(0, {pole_y}, 0)` in the shader"
                );
                assert_eq!(
                    uncapped, 0,
                    "{name}: the control must show the HOLE -- with the caps switched off, none \
                     of the {total} pixels around the pole may be drawn, but {uncapped} were. \
                     If this is non-zero the test is not measuring the cap"
                );
            }
        });
    }

    /// A sphere whose every tile carries a tile-covering fill, over the
    /// backdrop sphere, on black space -- and **at the transition
    /// `camera.zoom` really asks for**, which is the one thing
    /// [`globe_level_draws_in`] pins to 1.0 and the two tests below need
    /// swept.
    ///
    /// The tiles cover the whole Mercator square AND both pole caps, and
    /// the backdrop covers exactly the same sphere with the same
    /// subdivision (`render::subdivide`'s cell is 5.625 degrees at every
    /// level in the band, so z0 at granularity 64 puts its vertices on the
    /// same parallels and meridians as any level in the band). The backdrop
    /// is drawn first and writes no depth, so **no pixel of it may survive
    /// anywhere**: every background-coloured pixel in the returned frame is
    /// a geometry defect, and the two tests below differ only in where they
    /// look for one.
    #[allow(clippy::too_many_arguments)]
    async fn tiled_globe_over_backdrop(
        ctx: &GpuContext,
        pipeline: &TilePipeline,
        camera: &crate::render::Camera,
        size: u32,
        extent: u32,
        level: u8,
        water: crate::style::Rgba,
        background: crate::style::Rgba,
        capped: bool,
    ) -> Vec<u8> {
        let (meshes, mut uniforms) = globe_level_draws_in(camera, level, extent, water, |tile| {
            let full = crate::render::subdivide::Subdivision::for_tile(tile);
            if capped {
                full
            } else {
                // The control: a pole-row tile with no cap of its own, which
                // is the state it is in when no fill of its own reaches the
                // tile edge. The BACKDROP keeps its caps either way -- it is
                // a tile-covering ring by construction, so its cap is
                // unconditional.
                crate::render::subdivide::Subdivision {
                    north_pole: false,
                    south_pole: false,
                    ..full
                }
            }
        });
        let transition = crate::render::globe::transition_for_zoom(camera.zoom);
        for (_, params) in &mut uniforms {
            params.transition = transition;
        }
        let buffers: Vec<TileBuffers> =
            meshes.iter().map(|mesh| pipeline.create_tile_buffers(ctx, mesh)).collect();
        let groups: Vec<wgpu::BindGroup> = uniforms
            .iter()
            .map(|(matrix, globe)| {
                pipeline.create_model_bind_group(ctx, *matrix, 1.0, 0.0, *globe).1
            })
            .collect();
        let draws: Vec<(&TileBuffers, &wgpu::BindGroup)> =
            buffers.iter().zip(groups.iter()).collect();
        // `backdrop_draw` takes its transition from `globe_frame(camera)`,
        // i.e. from the same `transition_for_zoom` the tiles were just
        // given, so the two spheres are blended by the same scalar.
        let (backdrop_buffers, backdrop_group) =
            backdrop_draw(ctx, pipeline, camera, extent, background);
        render_space_frame(
            ctx,
            pipeline,
            camera,
            size,
            wgpu::Color::BLACK,
            &SpaceDraw {
                sky: None,
                stars: None,
                backdrop: Some((&backdrop_buffers, &backdrop_group)),
                atmosphere: None,
                look: None,
            },
            &draws,
        )
        .await
    }

    /// The wash shapes `globe_level_base_and_wash` lays over its base, in
    /// one tile's MVT coordinates. Deliberately DISJOINT from one another:
    /// any pixel carrying two composites is the renderer's doing, not the
    /// fixture's.
    fn wash_rings(e: i32) -> Vec<geo_types::Polygon<i32>> {
        let ring = |pts: Vec<(i32, i32)>| {
            geo_types::Polygon::new(geo_types::LineString::from(pts), Vec::new())
        };
        vec![
            // A concave blob crossing many cells, reaching the west edge.
            ring(vec![
                (0, e / 8), (e / 3, e / 16), (e / 2, e / 4), (e / 3, e / 3),
                (e / 2, e / 2), (e / 6, e / 2), (0, e / 3), (0, e / 8),
            ]),
            // A diagonal wedge -- every edge crosses cells obliquely.
            ring(vec![
                (e * 5 / 8, e / 16), (e, e / 3), (e * 3 / 4, e / 2),
                (e * 5 / 8, e / 16),
            ]),
            // A staircase whose steps land ON cell boundaries at z2/z3
            // granularity, which is where a clipper that keeps a boundary
            // vertex twice would show.
            ring(vec![
                (0, e * 5 / 8), (e / 4, e * 5 / 8), (e / 4, e * 3 / 4),
                (e / 2, e * 3 / 4), (e / 2, e * 5 / 8), (e * 3 / 4, e * 5 / 8),
                (e * 3 / 4, e), (0, e), (0, e * 5 / 8),
            ]),
            // A ring with a hole, reaching the tile's south and east edges
            // -- so it grows pole caps on a pole-row tile.
            geo_types::Polygon::new(
                geo_types::LineString::from(vec![
                    (e * 13 / 16, e * 5 / 8), (e, e * 5 / 8), (e, e),
                    (e * 13 / 16, e), (e * 13 / 16, e * 5 / 8),
                ]),
                vec![geo_types::LineString::from(vec![
                    (e * 7 / 8, e * 11 / 16), (e * 15 / 16, e * 11 / 16),
                    (e * 15 / 16, e * 15 / 16), (e * 7 / 8, e * 15 / 16),
                    (e * 7 / 8, e * 11 / 16),
                ])],
            ),
        ]
    }

    /// One level of globe tiles, each drawn as an OPAQUE base with a
    /// TRANSLUCENT wash of the same shape over it -- the shape a basemap
    /// actually has at low zoom, where `landcover` is near-white at
    /// `fill-opacity` 0.3 over ground that is the backdrop sphere.
    #[allow(clippy::type_complexity)]
    fn globe_level_base_and_wash(
        camera: &crate::render::Camera,
        level: u8,
        extent: u32,
        base: crate::style::Rgba,
        wash: crate::style::Rgba,
    ) -> (Vec<crate::render::TileMesh>, Vec<([[f32; 4]; 4], GlobeParams)>) {
        let frame = crate::render::globe::globe_frame(camera);
        let transition = crate::render::globe::transition_for_zoom(camera.zoom);
        let e = extent as i32;
        let ring =
            geo_types::LineString::from(vec![(0, 0), (e, 0), (e, e), (0, e), (0, 0)]);
        let geometry = std::sync::Arc::new(geo_types::Geometry::Polygon(
            geo_types::Polygon::new(ring, Vec::new()),
        ));
        let mut ops = vec![crate::style::PaintOp::Fill {
            color_from: base,
            color_to: base,
            geometry,
        }];
        // The wash's own shapes. NOT tile-covering: a fill whose outline
        // follows the tile's own edges meets the subdivision grid only where
        // the grid meets the tile, and real landcover does not look like
        // that. These cut across it -- a concave blob, a diagonal wedge, a
        // staircase along cell boundaries and a ring with a hole -- so the
        // clipper's every branch is in the mesh being graded.
        for ring in wash_rings(e) {
            ops.push(crate::style::PaintOp::Fill {
                color_from: wash,
                color_to: wash,
                geometry: std::sync::Arc::new(geo_types::Geometry::Polygon(ring)),
            });
        }
        let plan = crate::style::PaintPlan { ops };
        let side = 1u32 << level;
        let mut meshes = Vec::new();
        let mut uniforms = Vec::new();
        for y in 0..side {
            for x in 0..side {
                let tile = crate::geo::TileId { z: level, x, y };
                let tile_world_size = crate::render::WORLD_SIZE / 2f32.powi(i32::from(level));
                meshes.push(crate::render::tessellate_plan(
                    &plan,
                    extent,
                    crate::render::mesh::COORD_SCALE,
                    crate::render::subdivide::Subdivision::for_tile(tile),
                ));
                uniforms.push((
                    crate::render::combined_tile_matrix(camera, tile, extent as f32)
                        .to_cols_array_2d(),
                    GlobeParams {
                        globe_matrix: frame.globe_view_proj.as_mat4().to_cols_array_2d(),
                        tile_mercator_coords: crate::render::globe::tile_mercator_coords(tile),
                        clipping_plane: frame.clipping_plane.map(|c| c as f32),
                        transition,
                        lyon_to_tile_unit: 1.0
                            / (crate::render::mesh::COORD_SCALE * extent as f32),
                        tile_units_per_px: 1.0
                            / (tile_world_size * 2f32.powf(camera.zoom as f32)),
                        ..GlobeParams::FLAT
                    }
                    .unlit(),
                ));
            }
        }
        (meshes, uniforms)
    }

    /// **A TRANSLUCENT FILL IS COMPOSITED EXACTLY ONCE PER PIXEL IT COVERS,
    /// ON THE GLOBE, AT EVERY TRANSITION.**
    ///
    /// # Why a translucent fill is the fixture and an opaque one is not
    ///
    /// Overdraw is INVISIBLE in an opaque fill: drawing the same triangle
    /// twice writes the same bytes. It is visible, and only visible, when
    /// the fill has alpha -- and a basemap's low-zoom landcover does. In the
    /// shipped Mapbox Standard style `landcover` is `#fcfcfc` at
    /// `fill-opacity` 0.3 with `maxzoom: 7`, i.e. a near-white wash that
    /// exists ONLY in the zoom band where the globe exists, over ground that
    /// is the backdrop sphere. Composite that wash twice and the result
    /// moves a third of the way to white again; three times and it is white.
    /// Pale patchwork with hard polygon edges is what that looks like, which
    /// is the report this test was written against.
    ///
    /// So the fixture is one opaque base (0.2 grey, byte 51) with one
    /// white wash at alpha 0.3 over it, both tile-covering, both through the
    /// real bake at `Subdivision::for_tile` -- so the subdivision grid, the
    /// tile seams and the pole caps are all in the mesh being graded. The
    /// arithmetic is unambiguous:
    ///
    /// ```text
    ///   composites   byte
    ///   0 (a crack)    51
    ///   1 (correct)   112
    ///   2              155
    ///   3              185
    /// ```
    ///
    /// Anything at or above 140 is a pixel the wash reached twice.
    ///
    /// # What it measured, and what that eliminates
    ///
    /// Written against a report of "pale grey/white patchwork with hard
    /// polygon edges" across Siberia at low zoom. Measured on an Apple
    /// M-series adapter, a 512x512 frame, the real bake:
    ///
    /// ```text
    ///   camera                        transition   once   base-only   TWICE+
    ///   lon 0   lat 0   z1.5  L1          1.0000  41935       39741        0
    ///   lon 0   lat 0   z2.5  L2          1.0000 115406       98918        0
    ///   lon 88.8 lat 61 z2.97 L3          1.0000 134613      127526        0
    ///   lon 88.8 lat 61 z3.14 L3          0.9937 133468      128667        0
    ///   lon 88.8 lat 61 z3.48 L3          0.9314 125256      136879        0
    ///   lon 0   lat -85 z2.5  L2          1.0000 222859       39285        0
    ///   lon 0   lat 85  z3.48 L3          0.9314  81425      180719        0
    /// ```
    ///
    /// lon 88.8 lat 61 is the reported camera and 3.14/3.48 the reported
    /// zooms. Zero overdrawn pixels at any of them, at both poles, on both
    /// sides of the `transition > 0.999` branch, over the subdivision grid,
    /// the tile seams and the pole caps together. So whatever the reported
    /// patchwork is, the mesh is not compositing a translucent fill twice
    /// anywhere -- and the same run reports no pixel where the wash is
    /// MISSING inside its own outline either.
    // `3.14` below is a ZOOM the report names, not an approximation of pi.
    // `clippy::approx_constant` cannot tell the two apart, and writing the
    // camera as anything other than the number the user typed would make the
    // fixture unreadable against the report it grades.
    #[allow(clippy::approx_constant)]
    #[test]
    #[cfg_attr(not(target_os = "macos"), ignore = "rasterizer precision differs from Metal's (fails on lavapipe)")]
    fn a_translucent_fill_is_composited_exactly_once_per_pixel_on_the_globe() {
        #[cfg(feature = "probe")]
        let _serialised = crate::render::alloc::phase_lock();
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 512u32;
            let extent = 4096u32;
            let base = crate::style::Rgba { r: 0.2, g: 0.2, b: 0.2, a: 1.0 };
            let wash = crate::style::Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.3 };

            let mut worst: Vec<(f64, f64, f64, usize)> = Vec::new();
            // The reported camera (lon 88.8, lat 61.0, "big scale") and the
            // two poles, across the pure-globe end and the blended band.
            for (lon, lat, zoom, level) in [
                (0.0f64, 0.0f64, 1.5f64, 1u8),
                (0.0, 0.0, 2.5, 2),
                (88.8, 61.0, 2.97, 3),
                (88.8, 61.0, 3.14, 3),
                (88.8, 61.0, 3.48, 3),
                (0.0, -85.0, 2.5, 2),
                (0.0, 85.0, 3.48, 3),
            ] {
                let camera = crate::render::Camera {
                    center_lon: lon,
                    center_lat: lat,
                    zoom,
                    pitch: 0.0,
                    bearing: 0.0,
                    viewport_width_px: size as f32,
                    viewport_height_px: size as f32,
                    fov_y: crate::render::Camera::DEFAULT_FOV_Y,
                };
                let (meshes, uniforms) =
                    globe_level_base_and_wash(&camera, level, extent, base, wash);
                let data =
                    render_globe_level(&ctx, &pipeline, &camera, size, &meshes, &uniforms).await;
                let (mut once, mut cracks) = (0usize, 0usize);
                let mut offenders: Vec<(u32, u32, u8)> = Vec::new();
                for row in 0..size {
                    for col in 0..size {
                        let byte = rgb_at(&data, size, col, row)[0];
                        match i32::from(byte) {
                            0..=20 => {}                 // off the disc
                            45..=57 => cracks += 1,
                            105..=119 => once += 1,
                            v if v >= 140 => offenders.push((col, row, byte)),
                            _ => {}
                        }
                    }
                }
                let transition = crate::render::globe::transition_for_zoom(zoom);
                println!(
                    "lon {lon} lat {lat} z{zoom} L{level} (transition {transition:.4}): \
                     once {once}, base-only {cracks}, TWICE-OR-MORE {} {:?}",
                    offenders.len(),
                    &offenders[..offenders.len().min(12)]
                );
                assert!(
                    once > 5_000,
                    "lon {lon} lat {lat} z{zoom}: the wash must actually be drawn -- only \
                     {once} pixels of {} carry exactly one composite, so this camera grades \
                     nothing",
                    size * size
                );
                worst.push((lon, lat, zoom, offenders.len()));
            }
            let total: usize = worst.iter().map(|(_, _, _, n)| *n).sum();
            assert_eq!(
                total, 0,
                "{total} pixels across the sweep carry the translucent wash TWICE or \
                 more. Overdraw of a translucent fill is the pale patchwork the report \
                 describes. Per camera (lon, lat, zoom, offenders): {worst:?}"
            );
        });
    }

    /// Every pixel of `data` whose colour is within `tolerance` of `wanted`,
    /// as `(col, row)`.
    fn pixels_matching(
        data: &[u8],
        size: u32,
        wanted: [u8; 3],
        tolerance: i32,
    ) -> Vec<(u32, u32)> {
        let mut found = Vec::new();
        for row in 0..size {
            for col in 0..size {
                if rgb_gap(rgb_at(data, size, col, row), wanted) <= tolerance {
                    found.push((col, row));
                }
            }
        }
        found
    }

    /// **The pole cap closes the sphere at EVERY transition, and the
    /// control in this test is the ring once seen there.**
    ///
    /// # Why this test exists when `the_pole_caps_close_the_sphere` passed
    ///
    /// That test samples a **10 px disc at the pole itself**, at
    /// `transition == 1.0` only, and passed 317 of 317. It is blind by
    /// construction to a malformed ANNULUS five degrees away, and blind to
    /// every transition between the two ends -- which is exactly where a
    /// sawtooth ring of pale wedges once appeared. This one samples
    /// EVERY pixel of the frame, reports the offenders by their radius from
    /// the pole so a ring is legible in the failure message rather than
    /// inferred, and sweeps the whole `GLOBE_ZOOM_MIN`..`GLOBE_ZOOM_MAX`
    /// range -- which is the range over which the planet is drawn as a ball
    /// in space, and now entirely inside the pure-globe half of the
    /// projection (see `render::globe::PROJECTION_BLEND_ZOOM_MIN`).
    ///
    /// # The control arm IS the reported defect, rendered
    ///
    /// The fixture is a water sphere -- every z2 tile a tile-covering fill,
    /// caps and all -- over the backdrop sphere in the style's BACKGROUND
    /// colour. The backdrop is drawn first and writes no depth, so no pixel
    /// of it may survive: a background-coloured pixel is the polar cap
    /// standing open. The control arm switches the TILES' caps off and
    /// leaves the backdrop's on, which is precisely the state a pole-row
    /// tile is in when no fill of its own reaches the tile edge, and it
    /// paints the ring:
    ///
    /// ```text
    ///   camera            transition   caps on   caps off (the control)
    ///   lat 85, zoom 1.0      1.0000         0    66,592 px, r 0..159
    ///   lat 85, zoom 1.5      1.0000         0    91,006 px, r 0..224
    ///   lat 85, zoom 2.5      1.0000         0   115,468 px, r 0..439
    ///   lat 85, zoom 3.5      0.9259         0   120,158 px, r 0..841
    ///   lat 85, zoom 4.0      0.7407         0   120,648 px, r 0..1151
    ///   lat 85, zoom 5.0      0.2593         0   116,458 px, r 0..2059
    /// ```
    ///
    /// The camera sits at latitude 85 and the tile edge is at 85.0511, so
    /// everything above the screen centre is polar cap at every zoom -- the
    /// one place on the globe whose coverage is caps and nothing else. (The
    /// pole's own projected position leaves the frame above zoom 2; the
    /// radii are still measured from it, which is why they grow.)
    #[test]
    fn no_pale_wedge_rings_the_pole_at_any_transition() {
        // `render::alloc`'s `DRAW_CALLS` is PROCESS-GLOBAL, and
        // `the_space_setup_costs_nothing_a_flat_frame_can_notice` asserts an
        // exact ZERO against a delta of it around a flat frame. The frames
        // below are big enough to be seen from over there: when this test
        // first landed without this lock, that one read 17 draw calls on a
        // frame that issued none. So take the same lock it takes. Only
        // under `probe`, because that is the only build in which the
        // counter is read -- and `phase_lock` only exists there.
        #[cfg(feature = "probe")]
        let _serialised = crate::render::alloc::phase_lock();
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 512u32;
            let extent = 4096u32;
            // A light basemap's pair, which is what the report was against:
            // a pale background with a blue ocean over it.
            let background = crate::style::Rgba { r: 0.95, g: 0.94, b: 0.91, a: 1.0 };
            let water = crate::style::Rgba { r: 0.31, g: 0.45, b: 0.60, a: 1.0 };
            let background_bytes = [242u8, 240, 232];
            let water_bytes = [79u8, 115, 153];

            for lat in [85.0f64, -85.0] {
                for zoom in [1.0f64, 1.5, 2.5, 3.5, 4.0, 5.0] {
                    let camera = crate::render::Camera {
                        center_lon: 0.0,
                        center_lat: lat,
                        zoom,
                        pitch: 0.0,
                        bearing: 0.0,
                        viewport_width_px: size as f32,
                        viewport_height_px: size as f32,
                        fov_y: crate::render::Camera::DEFAULT_FOV_Y,
                    };
                    let transition = crate::render::globe::transition_for_zoom(zoom);
                    assert!(
                        transition > 0.0,
                        "zoom {zoom} must be inside the globe band, or there are no caps \
                         to get wrong"
                    );
                    let (pole_col, pole_row) =
                        sphere_lon_lat_to_px(&camera, 0.0, lat.signum() * 90.0, size);

                    let mut counts = Vec::new();
                    for capped in [true, false] {
                        let data = tiled_globe_over_backdrop(
                            &ctx, &pipeline, &camera, size, extent, 2, water, background, capped,
                        )
                        .await;
                        // The fixture has to be WATER, or "no background
                        // pixel" is satisfied by a frame with nothing in it
                        // -- Task 8's warning about a fixture that cannot
                        // distinguish the failure.
                        let wet = pixels_matching(&data, size, water_bytes, 3).len();
                        assert!(
                            wet > 20_000,
                            "lat {lat} zoom {zoom}, caps {capped}: the ocean must actually \
                             be drawn -- only {wet} pixels of {} are water",
                            size * size
                        );
                        let pale = pixels_matching(&data, size, background_bytes, 8);
                        let radii: Vec<f64> = pale
                            .iter()
                            .map(|(c, r)| {
                                (f64::from(*c) - pole_col).hypot(f64::from(*r) - pole_row)
                            })
                            .collect();
                        let hi = radii.iter().copied().fold(0.0, f64::max);
                        counts.push((pale.len(), hi));
                    }

                    assert_eq!(
                        counts[0].0,
                        0,
                        "lat {lat} zoom {zoom} (transition {transition:.4}): {} pixels of \
                         {} are the style's BACKGROUND colour {background_bytes:?}, out to \
                         {:.0} px from the pole. Every one of them is the backdrop sphere \
                         showing through a fully tiled globe, which nothing can \
                         legitimately do: the backdrop is drawn first, writes no depth, \
                         and carries the same 5.625-degree subdivision cell as the tiles \
                         over it",
                        counts[0].0,
                        size * size,
                        counts[0].1
                    );
                    // ...and the control must SHOW the ring, or the
                    // assertion above is about a frame in which the polar
                    // cap was never on screen.
                    assert!(
                        counts[1].0 > 10_000,
                        "lat {lat} zoom {zoom}: the control -- the tiles' pole caps \
                         switched off, the backdrop's left on -- must paint the reported \
                         ring, but only {} pixels came out background-coloured. Without \
                         that this test is looking somewhere the polar cap is not",
                        counts[1].0
                    );
                }
            }
        });
    }

    /// **No band of the style's background colour between the tile cover's
    /// edge and the limb** -- and the cover's horizon cut IS the silhouette,
    /// asserted rather than assumed.
    ///
    /// # The cause is NOT the tile cover, and that was measured before
    /// anything was changed
    ///
    /// The first guess was that `render::globe::GlobeCover`'s horizon cut is
    /// tighter than the silhouette, so tiles stop short of the limb and the
    /// backdrop shows through the gap. It is not. `clipping_plane` is called
    /// as `clipping_plane(camera, d / r)` where `d` is the eye-to-SURFACE
    /// distance, and the matrix chain puts the sphere centre a further `r`
    /// behind, so `-w` comes out as exactly `r / (d + r)` and the keep test
    /// `max_dot + w >= 0` reads `cos(theta) >= r/D` -- the tangent cone
    /// itself. Measured against `acos(r/D)` computed independently,
    /// 1024x768 at pitch 0:
    ///
    /// ```text
    ///   camera                   silhouette acos(r/D)   acos(-w)     diff
    ///   lon 0 lat 0 z2.0                    70.7985       70.7985        0
    ///   lon 0 lat 0 z2.5                    65.8357       65.8357        0
    ///   lon 0 lat 0 z3.0                    60.3312       60.3312  1.4e-14
    ///   lon 13.4 lat 52.5 z2.5              57.8337       57.8337        0
    /// ```
    ///
    /// The Task 7 report's "65.84 degrees at z2.5, LESS than a hemisphere"
    /// is that same silhouette angle: the true statement that
    /// `acos(r/D) < 90` at a finite distance, not a shortfall.
    ///
    /// # What a band inside the limb actually takes
    ///
    /// Mercator stops at +/-85.0511, so the only thing that can cover the
    /// last 4.95 degrees to a pole is a cap. At an oblique camera the near
    /// pole is inside the silhouette from about 20 degrees of latitude
    /// upward, so a cap that does not close puts a background-coloured band
    /// just inside the limb ON THE SIDE THE POLE IS ON -- which is what
    /// "wider on one side" is. The control arm -- the tiles' caps off, the
    /// backdrop's left on -- renders exactly that, and is what makes the
    /// zero above a measurement rather than an empty frame:
    ///
    /// ```text
    ///   camera (lon 13.4, 512x512)   caps on   caps off (the control)
    ///   lat 52.5, zoom 1.0                 0            780 px
    ///   lat 52.5, zoom 1.5                 0          1,270 px
    ///   lat 52.5, zoom 2.0                 0            762 px
    ///   lat 65.0, zoom 2.0                 0          1,133 px
    /// ```
    ///
    /// The four cameras are the ones where the near pole is both inside the
    /// silhouette and inside the frame; at lat 52.5 zoom 2.5 and lat 65
    /// zoom 2.5 the control measures 0, so those would grade nothing and
    /// are deliberately not in the list.
    #[test]
    fn no_pale_crescent_between_the_tile_cover_and_the_limb() {
        // `render::alloc`'s `DRAW_CALLS` is PROCESS-GLOBAL, and
        // `the_space_setup_costs_nothing_a_flat_frame_can_notice` asserts an
        // exact ZERO against a delta of it around a flat frame. The frames
        // below are big enough to be seen from over there: when this test
        // first landed without this lock, that one read 17 draw calls on a
        // frame that issued none. So take the same lock it takes. Only
        // under `probe`, because that is the only build in which the
        // counter is read -- and `phase_lock` only exists there.
        #[cfg(feature = "probe")]
        let _serialised = crate::render::alloc::phase_lock();
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 512u32;
            let extent = 4096u32;
            let background = crate::style::Rgba { r: 0.95, g: 0.94, b: 0.91, a: 1.0 };
            let water = crate::style::Rgba { r: 0.31, g: 0.45, b: 0.60, a: 1.0 };
            let background_bytes = [242u8, 240, 232];

            // The cover's horizon cut IS the tangent cone. Asserted here
            // rather than quoted, so the refutation above cannot rot.
            for zoom in [2.0f64, 2.5, 3.0] {
                let camera = crate::render::Camera {
                    center_lon: 13.4,
                    center_lat: 52.5,
                    zoom,
                    pitch: 0.0,
                    bearing: 0.0,
                    viewport_width_px: 1024.0,
                    viewport_height_px: 768.0,
                    fov_y: crate::render::Camera::DEFAULT_FOV_Y,
                };
                let radius = crate::render::globe::globe_radius_world(&camera);
                let silhouette = (radius / (camera.distance() + radius)).acos().to_degrees();
                let cut = (-crate::render::globe::globe_frame(&camera).clipping_plane[3])
                    .acos()
                    .to_degrees();
                assert!(
                    (silhouette - cut).abs() < 1e-9,
                    "the cover's horizon cut must BE the silhouette, or tiles stop short \
                     of the limb: silhouette {silhouette:.4} deg against cut {cut:.4} deg \
                     at zoom {zoom}"
                );
            }

            // The camera the crescent was reported from: a mid-latitude
            // oblique view with the whole disc on screen.
            for (lat, zoom) in [(52.5f64, 1.0f64), (52.5, 1.5), (52.5, 2.0), (65.0, 2.0)] {
                let camera = crate::render::Camera {
                    center_lon: 13.4,
                    center_lat: lat,
                    zoom,
                    pitch: 0.0,
                    bearing: 0.0,
                    viewport_width_px: size as f32,
                    viewport_height_px: size as f32,
                    fov_y: crate::render::Camera::DEFAULT_FOV_Y,
                };
                let mut counts = Vec::new();
                for capped in [true, false] {
                    let data = tiled_globe_over_backdrop(
                        &ctx, &pipeline, &camera, size, extent, 2, water, background, capped,
                    )
                    .await;
                    let wet = pixels_matching(&data, size, [79u8, 115, 153], 3).len();
                    assert!(
                        wet > 20_000,
                        "zoom {zoom}, caps {capped}: the ocean must actually be drawn -- \
                         only {wet} pixels are water"
                    );
                    counts.push(pixels_matching(&data, size, background_bytes, 8).len());
                }
                assert_eq!(
                    counts[0], 0,
                    "at zoom {zoom} (transition {:.4}) {} pixels are the style's \
                     BACKGROUND colour -- the backdrop sphere showing through a fully \
                     tiled globe",
                    crate::render::globe::transition_for_zoom(zoom),
                    counts[0]
                );
                assert!(
                    counts[1] > 500,
                    "at zoom {zoom} the control -- the tiles' pole caps off, the \
                     backdrop's on -- must paint the crescent, but only {} pixels came \
                     out background-coloured",
                    counts[1]
                );
            }
        });
    }

    /// **Subdivision opens no crack in a flat tile** -- the claim the whole
    /// "subdivide unconditionally, independent of projection" divergence
    /// rests on (`render::subdivide`'s module docs, and the globe plan's
    /// "Where we diverge" #2).
    ///
    /// Mercator is affine in tile space, so cutting a triangle on a grid and
    /// projecting the pieces flat covers the same AREA as projecting the
    /// whole triangle. Two abutting triangles show a seam the moment their
    /// shared vertices differ by one ULP, which is why
    /// `render::subdivide::intersect` orders its endpoints and why the clip
    /// does rows before columns. This is the proof, against the rasterizer
    /// rather than against a model of it: a z2 tile's granularity of 16
    /// against `Subdivision::NONE`, at `transition = 0.0`, over the whole
    /// 512x512 readback. The geometry is a polygon WITH A HOLE, so the split
    /// has interior boundaries to get wrong, and none of its coordinates is
    /// a multiple of the 256-unit cell, so almost every cell it touches is a
    /// partial one.
    ///
    /// # It is NOT byte-identical, and the four pixels are worth explaining
    ///
    /// Measured on an Apple M-series adapter: **0 pixels lost, 0 recoloured,
    /// 4 gained** of 184,000-odd drawn -- at (476,113), (361,131),
    /// (209,463) and (37,468), each a background pixel that the subdivided
    /// mesh covers.
    ///
    /// That is the rasterizer, not the split. A vertex inserted in the
    /// middle of an outer-boundary edge is a NEW vertex, and the GPU snaps
    /// each vertex to its own sub-pixel grid independently, so the
    /// piecewise-linear edge through it can sit up to half a sub-pixel step
    /// (about 1/512 px) off the straight line between the edge's original
    /// endpoints. Over roughly 1,500 px of boundary that is a handful of
    /// pixel centres either side of the line -- which is what came out.
    ///
    /// So the assertion is split three ways, and only the direction that
    /// could be a defect is pinned to zero: **lost must be 0** (a lost pixel
    /// is a gap along a cell boundary, the crack this whole design exists to
    /// avoid), **recoloured must be 0** (a split of one opaque fill cannot
    /// change a colour), and gained is bounded at 16 -- loose enough for
    /// another GPU's snapping rule, tight enough that geometry actually
    /// moving fails it.
    ///
    /// Fills only, deliberately. A subdivided LINE is not expected to be
    /// pixel-stable at all: `subdivide_line` inserts collinear points, which
    /// changes where `run_split::split_into_runs` puts its run boundaries
    /// and therefore which origin each run is rebased against, and rebasing
    /// is an f32 operation. `render::subdivide`'s
    /// `a_subdivided_line_is_the_same_line_with_more_points_in_order` pins
    /// the property that actually matters for a line (its points stay on it,
    /// in order).
    #[test]
    fn subdividing_a_flat_tile_opens_no_crack() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 512u32;
            let extent = 4096u32;
            let tile = crate::geo::TileId { z: 2, x: 1, y: 1 };
            let camera = crate::render::Camera {
                center_lon: -45.0,
                center_lat: 40.0,
                zoom: 2.0,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: size as f32,
                viewport_height_px: size as f32,
                fov_y: crate::render::Camera::DEFAULT_FOV_Y,
            };
            // Offset from the grid on both axes, and with a hole, so the
            // clip has interior edges and partial cells to get wrong: at
            // granularity 16 a cell is 256 MVT units, and none of these
            // coordinates is a multiple of it.
            let outer = geo_types::LineString::from(vec![
                (273, 191),
                (3785, 137),
                (3901, 3702),
                (150, 3811),
                (273, 191),
            ]);
            let hole = geo_types::LineString::from(vec![
                (1100, 1210),
                (2900, 1010),
                (2750, 2900),
                (1010, 2650),
                (1100, 1210),
            ]);
            let color = crate::style::Rgba { r: 0.2, g: 0.7, b: 0.9, a: 1.0 };
            let plan = crate::style::PaintPlan {
                ops: vec![crate::style::PaintOp::Fill {
                    color_from: color,
                    color_to: color,
                    geometry: std::sync::Arc::new(geo_types::Geometry::Polygon(
                        geo_types::Polygon::new(outer, vec![hole]),
                    )),
                }],
            };

            let mut frames = Vec::new();
            let mut counts = Vec::new();
            for subdivision in [
                crate::render::subdivide::Subdivision::NONE,
                crate::render::subdivide::Subdivision::for_tile(tile),
            ] {
                let mesh = crate::render::tessellate_plan(
                    &plan,
                    extent,
                    crate::render::mesh::COORD_SCALE,
                    subdivision,
                );
                counts.push(mesh.vertices.len());
                let color_texture = make_color_texture(&ctx, size);
                let color_view =
                    color_texture.create_view(&wgpu::TextureViewDescriptor::default());
                let depth_view = pipeline.create_depth_view(&ctx, size, size);
                let camera_bind_group = pipeline.test_camera_bind_group(
                    &ctx,
                    camera.view_projection().to_cols_array_2d(),
                    camera.zoom as f32,
                );
                let matrix = crate::render::combined_tile_matrix(&camera, tile, extent as f32)
                    .to_cols_array_2d();
                // `GlobeParams::FLAT` -- `transition == 0.0`, which is
                // `vs_main`'s Mercator early-out.
                let model =
                    pipeline.create_model_bind_group(&ctx, matrix, 1.0, 0.0, GlobeParams::FLAT).1;
                let buffers = pipeline.create_tile_buffers(&ctx, &mesh);
                let mut encoder = ctx
                    .device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
                pipeline.render(
                    &mut encoder,
                    &color_view,
                    &depth_view,
                    wgpu::Color::TRANSPARENT,
                    &camera_bind_group,
                    &[(&buffers, &model)],
                    None,
                );
                frames.push(read_all_pixels(&ctx, &color_texture, encoder, size).await);
            }

            assert!(
                counts[1] > counts[0] * 2,
                "the two halves must really be different meshes: {} vertices unsubdivided, {} \
                 at granularity {}",
                counts[0],
                counts[1],
                crate::render::subdivide::granularity_for_zoom(tile.z)
            );
            // Lost and gained are counted SEPARATELY, and only one of them
            // is allowed to be non-zero -- see this test's doc comment.
            let bytes_per_row = (size * 4).next_multiple_of(256) as usize;
            let mut lost = 0usize;
            let mut gained = 0usize;
            let mut recoloured = 0usize;
            for row in 0..size as usize {
                for col in 0..size as usize {
                    let offset = row * bytes_per_row + col * 4;
                    let before = &frames[0][offset..offset + 4];
                    let after = &frames[1][offset..offset + 4];
                    if before == after {
                        continue;
                    }
                    match (before[3] > 20, after[3] > 20) {
                        (true, false) => lost += 1,
                        (false, true) => gained += 1,
                        _ => recoloured += 1,
                    }
                }
            }
            let drawn = (0..size as usize)
                .flat_map(|row| (0..size as usize).map(move |col| (col, row)))
                .filter(|(col, row)| frames[0][row * bytes_per_row + col * 4 + 3] > 20)
                .count();
            assert_eq!(
                lost, 0,
                "SUBDIVISION MUST NOT OPEN A CRACK: {lost} pixels of the {drawn} the undivided \
                 mesh covered are background after the split. That is a gap along a cell \
                 boundary -- check `render::subdivide::intersect`'s endpoint ordering and that \
                 the clip still does rows before columns"
            );
            assert_eq!(
                recoloured, 0,
                "{recoloured} pixels changed colour without changing coverage, which a split of \
                 one opaque fill cannot do"
            );
            assert!(
                gained <= 16,
                "{gained} pixels gained of {drawn} drawn -- far more than the handful the \
                 rasterizer's own vertex snapping can explain (see this test's doc comment). \
                 The split has moved geometry"
            );
        });
    }

    /// The camera every Task 8 pixel test uses: lon 0, lat 0, z1.5, 512x512
    /// -- the same one `at_full_transition_the_world_is_drawn_as_a_disc_and_not_a_rectangle`
    /// and `subdivision_makes_the_globes_limb_a_circle` use, so the
    /// analytic silhouette of **161.385 px** those two derive applies here
    /// unchanged.
    fn space_camera(center_lon: f64, center_lat: f64) -> crate::render::Camera {
        crate::render::Camera {
            center_lon,
            center_lat,
            zoom: 1.5,
            pitch: 0.0,
            bearing: 0.0,
            viewport_width_px: 512.0,
            viewport_height_px: 512.0,
            fov_y: crate::render::Camera::DEFAULT_FOV_Y,
        }
    }

    /// The analytic silhouette radius in pixels for a `space_camera`,
    /// recomputed rather than quoted -- the same derivation
    /// `subdivision_makes_the_globes_limb_a_circle` uses, so a change in
    /// `render::globe`'s camera chain fails loudly instead of silently
    /// re-grading these tests.
    fn analytic_disc_radius_px(camera: &crate::render::Camera) -> f64 {
        let radius_world = crate::render::globe::globe_radius_world(camera);
        let eye = camera.distance() + radius_world;
        let half_angle = (radius_world / eye).asin();
        f64::from(camera.viewport_width_px) / 2.0 * half_angle.tan()
            / std::f64::consts::FRAC_PI_6.tan()
    }

    /// The backdrop's `TileBuffers` and model bind group for one camera --
    /// exactly what `MapView::paint` hands `SpaceDraw::backdrop`.
    fn backdrop_draw(
        ctx: &GpuContext,
        pipeline: &TilePipeline,
        camera: &crate::render::Camera,
        extent: u32,
        color: crate::style::Rgba,
    ) -> (TileBuffers, wgpu::BindGroup) {
        let tile = crate::render::space::BACKDROP_TILE;
        let mesh = crate::render::space::backdrop_mesh(extent, color, color);
        let buffers = pipeline.create_tile_buffers(ctx, &mesh);
        // UNLIT. `the_globe_sits_on_space_and_its_silhouette_reads` and
        // `the_glow_reads_outside_the_limb_and_does_not_wash_out_the_planet`
        // both grade the ocean sphere against the exact byte triple
        // (79, 116, 153), and both predate ground shading. In PRODUCTION
        // this same mesh IS shaded -- `MapView::paint` hands it
        // `globe.for_tile(BACKDROP_TILE)`, which carries the frame's sun
        // like every other tile, pinned by
        // `the_frames_sun_reaches_every_tile_including_the_backdrops`.
        let globe = crate::render::globe::GlobeUpload::new(
            &crate::render::globe::globe_frame(camera),
            camera,
            extent as f32,
            UNLIT_SUN,
            UNLIT_NIGHT,
        );
        let model =
            crate::render::combined_tile_matrix(camera, tile, extent as f32).to_cols_array_2d();
        let group = pipeline.create_model_bind_group(ctx, model, 1.0, 0.0, globe.for_tile(tile)).1;
        (buffers, group)
    }

    /// Renders one frame through `render_with_space` and reads it back.
    #[allow(clippy::too_many_arguments)]
    async fn render_space_frame(
        ctx: &GpuContext,
        pipeline: &TilePipeline,
        camera: &crate::render::Camera,
        size: u32,
        clear: wgpu::Color,
        space: &SpaceDraw<'_>,
        draws: &[(&TileBuffers, &wgpu::BindGroup)],
    ) -> Vec<u8> {
        let color_texture = make_color_texture(ctx, size);
        let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let depth_view = pipeline.create_depth_view(ctx, size, size);
        let camera_bind_group = pipeline.test_camera_bind_group(
            ctx,
            camera.view_projection().to_cols_array_2d(),
            camera.zoom as f32,
        );
        let mut encoder =
            ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        pipeline.render_with_space(
            &mut encoder,
            &color_view,
            &depth_view,
            clear,
            &camera_bind_group,
            space,
            draws,
            None,
        );
        read_all_pixels(ctx, &color_texture, encoder, size).await
    }

    /// One pixel's RGB, as bytes.
    fn rgb_at(data: &[u8], size: u32, col: u32, row: u32) -> [u8; 3] {
        let bytes_per_row = (size * 4).next_multiple_of(256);
        let offset = (row * bytes_per_row + col * 4) as usize;
        [data[offset], data[offset + 1], data[offset + 2]]
    }

    /// Manhattan-ish distance between two RGB bytes triples, as the largest
    /// per-channel difference. A colour metric with no perceptual pretence:
    /// what these tests need is "are these two the same colour or not".
    fn rgb_gap(a: [u8; 3], b: [u8; 3]) -> i32 {
        (0..3)
            .map(|i| (i32::from(a[i]) - i32::from(b[i])).abs())
            .max()
            .expect("three channels")
    }

    /// **THE ACCEPTANCE CRITERION FOR TASK 8's FIRST HALF: at full
    /// transition a pixel OUTSIDE the sphere is distinguishable from a pixel
    /// INSIDE it — and before this task it was not.**
    ///
    /// The defect this task exists for is not subtle once stated: the map's
    /// background is the render pass's CLEAR COLOUR, so on the globe it
    /// paints the entire viewport including outside the silhouette, and the
    /// planet sits on a field of its own ocean colour. This test measures
    /// exactly that, by rendering the same camera and the same sphere twice:
    ///
    ///  - **the control** is the pre-task arrangement — clear to the
    ///    background colour, no ocean sphere;
    ///  - **the fix** is `space::space_clear_color` at transition 1.0 plus
    ///    the ocean sphere as geometry.
    ///
    /// Measured on an Apple M-series adapter, with the background a
    /// plausible ocean `#4f7499` and the space colour the shipped default
    /// `#05070f`:
    ///
    /// ```text
    ///   arrangement            corner RGB      centre RGB    largest gap
    ///   control (clear only)  (79,116,153)   (79,116,153)             0
    ///   space + backdrop       ( 0,  0,  0)   (79,116,153)           153
    /// ```
    ///
    /// Zero, not "small": the control's corner and centre are the same
    /// bytes, because they are the same clear. That is the whole finding
    /// Task 5's author reported, as a number. 153 is the blue channel,
    /// which is the largest of the three.
    ///
    /// The centre must come out as the BACKGROUND colour in both, which is
    /// what stops the fixed half passing by drawing nothing: a globe that
    /// vanished would give a corner and a centre that are both space, and
    /// the gap would be zero again.
    #[test]
    fn the_globe_sits_on_space_and_its_silhouette_reads() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 512u32;
            let extent = 4096u32;
            let camera = space_camera(0.0, 0.0);
            // #4f7499, a plausible low-zoom ocean, deliberately nothing like
            // the space colour: what is under test is whether the two are
            // distinguishable, and a background already the colour of space
            // would make the question unanswerable rather than answered.
            // The hard case -- a near-black basemap, where the largest gap
            // a colour can buy is 19 byte units -- is measured instead in
            // `space::the_default_space_colour_maximises_the_gap_to_the_darkest_shipped_background`,
            // which is where the honest limit of a colour-only fix is
            // recorded.
            let background =
                crate::style::Rgba { r: 79.0 / 255.0, g: 116.0 / 255.0, b: 153.0 / 255.0, a: 1.0 };
            let space = crate::render::space::Space::default();

            let radius = analytic_disc_radius_px(&camera);
            assert!(
                (radius - 161.385).abs() < 0.01,
                "the analytic disc this test samples inside and outside of moved: {radius}"
            );
            // Sample points chosen from that radius rather than from a
            // picture: a corner is 362 px from the centre, comfortably
            // outside, and the centre is the sub-camera point.
            let (inside_col, inside_row) = (size / 2, size / 2);
            let (outside_col, outside_row) = (6u32, 6u32);
            assert!(
                (f64::from(outside_col) - 256.0).hypot(f64::from(outside_row) - 256.0) > radius + 8.0,
                "the OUTSIDE sample must really be outside the analytic disc"
            );

            let (buffers, group) = backdrop_draw(&ctx, &pipeline, &camera, extent, background);

            let mut measured = Vec::new();
            for with_space in [false, true] {
                let clear = TilePipeline::clear_color(&crate::render::space::space_clear_color(
                    Some(background),
                    &space,
                    if with_space { 1.0 } else { 0.0 },
                ));
                let draw = if with_space {
                    SpaceDraw { sky: None, stars: None, backdrop: Some((&buffers, &group)), atmosphere: None, look: None }
                } else {
                    SpaceDraw::NONE
                };
                let data =
                    render_space_frame(&ctx, &pipeline, &camera, size, clear, &draw, &[]).await;
                let outside = rgb_at(&data, size, outside_col, outside_row);
                let inside = rgb_at(&data, size, inside_col, inside_row);
                measured.push((outside, inside, rgb_gap(outside, inside)));
            }

            let (control_outside, control_inside, control_gap) = measured[0];
            let (outside, inside, gap) = measured[1];

            assert_eq!(
                control_gap, 0,
                "THE CONTROL IS THE DEFECT, and it is only a control if it really does \
                 paint the same colour inside and outside the sphere: got \
                 {control_outside:?} outside and {control_inside:?} inside"
            );
            // The planet is still the background colour, drawn as geometry
            // rather than as a clear.
            assert!(
                rgb_gap(inside, [79, 116, 153]) <= 2,
                "the centre of the disc must be the BACKGROUND colour -- the ocean \
                 sphere is the style's background layer, drawn: got {inside:?}"
            );
            assert!(
                rgb_gap(outside, [0, 0, 0]) <= 2,
                "outside the disc must be the SPACE colour: got {outside:?}"
            );
            assert!(
                gap > 80,
                "the silhouette must read: the largest per-channel gap between a pixel \
                 outside the sphere ({outside:?}) and one inside it ({inside:?}) is \
                 {gap}, where the control's was {control_gap}"
            );
        });
    }

    /// **A ground observer's stars light only the sky above the horizon,
    /// in pixels** (`MapView::set_sky_stars`): a camera over Tokyo pitched
    /// 80 degrees at the default FOV puts the horizon 10 degrees above the
    /// centre; drawn with `write_sky_over_horizon`, the star field lights
    /// pixels above that row and not one below it (two rows of slack for
    /// a billboard's radius).
    #[test]
    fn sky_stars_light_only_pixels_above_the_horizon() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let stars = crate::render::space::StarPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, pipeline.depth_format());
            let size = 512u32;
            for (bearing_deg, lat) in [(0.0f32, 35.63), (200.0, -33.9), (95.0, 64.1)] {
                let camera = crate::render::Camera {
                    center_lon: 139.78,
                    center_lat: lat,
                    zoom: 19.0,
                    pitch: 80f32.to_radians(),
                    bearing: bearing_deg.to_radians(),
                    viewport_width_px: size as f32,
                    viewport_height_px: size as f32,
                    fov_y: crate::render::Camera::DEFAULT_FOV_Y,
                };
                stars.write_sky_over_horizon(&ctx, &camera, 1.0, 1.0);
                assert!(stars.over_sky());
                let data = render_space_frame(
                    &ctx,
                    &pipeline,
                    &camera,
                    size,
                    wgpu::Color::BLACK,
                    &SpaceDraw { sky: None, stars: Some(&stars), backdrop: None, atmosphere: None, look: None },
                    &[],
                )
                .await;
                let f = f64::from(size) / 2.0 / (camera.fov_y / 2.0).tan();
                let horizon_row = f64::from(size) / 2.0 - f * 10f64.to_radians().tan();
                let (mut lit_above, mut lit_below) = (0, 0);
                for row in 0..size {
                    for col in 0..size {
                        if rgb_at(&data, size, col, row) != [0, 0, 0] {
                            if f64::from(row) < horizon_row + 2.0 {
                                lit_above += 1;
                            } else {
                                lit_below += 1;
                            }
                        }
                    }
                }
                assert_eq!(lit_below, 0, "bearing {bearing_deg}, lat {lat}: {lit_below} lit pixels under the horizon (row {horizon_row:.1})");
                assert!(lit_above > 10, "bearing {bearing_deg}, lat {lat}: only {lit_above} lit pixels in the sky");
            }
            // The space view's write puts the stars back under the sky pass.
            stars.write_sky(&ctx, &space_camera(0.0, 0.0), &crate::render::space::Space { star_intensity: 1.0, ..Default::default() });
            assert!(!stars.over_sky());
        });
    }

    /// **THE ACCEPTANCE CRITERION FOR TASK 8's SECOND HALF: a named star
    /// lands where the camera puts it, and 20 px away it is dark.**
    ///
    /// # The star, and why this one
    ///
    /// **Spica (alpha Virginis, HR 5056, V = +0.98, RA 201.298, Dec
    /// -11.161)** — the brightest catalogue star that falls in the annulus
    /// between the silhouette (161.385 px) and the frame edge at the shared
    /// `space_camera`, found by projecting all 9,096 and sorting by
    /// magnitude rather than by picking a famous name and hoping. Sirius is
    /// 79.2 degrees off this camera's axis, i.e. 2,323 px out, so it is not
    /// available; Spica is 21.0 degrees off, at **196.72 px** from the
    /// centre — 35 px clear of the limb, so the backdrop cannot be what
    /// lights the pixel.
    ///
    /// # The expected position is NOT computed from `sky_view_proj`
    ///
    /// If it were, this test would only prove the shader and the CPU agree
    /// about a matrix they both got from the same place. Instead the pixel
    /// is predicted from the pinhole geometry alone — the direction rotated
    /// by the camera's pitch and bearing (both zero here), then
    /// `k = (size/2) / tan(fov_y/2)` and `col = size/2 + k*x/(-z)` — which
    /// has no near plane, no far plane, no sky radius and no matrix in it.
    /// A wrong `SKY_NEAR`/`SKY_FAR`, a dropped aspect term or a sign error
    /// in the billboard offset all fail here.
    ///
    /// Measured on an Apple M-series adapter: the predicted pixel is
    /// **(83.14, 349.90)**, 196.72 px from the centre, and the rendered
    /// star's brightest pixel is **(83, 349)** — **0.91 px** away — reading
    /// **(235, 235, 235)** against a space colour of (0, 0, 0). It is 235
    /// rather than 255 because that pixel's own centre is 0.91 px off the
    /// star's, where the quadratic falloff has taken 8% off. The probe
    /// 20 px away reads **(0, 0, 0)**: the space colour EXACTLY, because
    /// the nearest catalogue star to that point is Spica itself at 20.0 px
    /// and every other one is further, against a largest billboard radius
    /// of 2.456 px.
    #[test]
    fn a_named_star_lands_where_the_camera_puts_it_and_is_dark_20_px_away() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let stars = crate::render::space::StarPipeline::new(
                &ctx,
                wgpu::TextureFormat::Rgba8Unorm,
                pipeline.depth_format(),
            );
            let size = 512u32;
            let camera = space_camera(0.0, 0.0);
            let space = crate::render::space::Space {
                star_intensity: 1.0,
                ..crate::render::space::Space::default()
            };

            // Spica, J2000.
            const SPICA_RA: f64 = 201.298_33;
            const SPICA_DEC: f64 = -11.161_39;
            // It must actually BE in the shipped catalogue, or this test is
            // about a direction rather than about a star.
            let shipped = crate::render::space::catalogue()
                .find(|star| {
                    (f64::from(star.ra_deg) - SPICA_RA).abs() < 0.01
                        && (f64::from(star.dec_deg) - SPICA_DEC).abs() < 0.01
                })
                .expect("Spica is in the shipped catalogue");
            assert!(
                (shipped.vmag - 0.98).abs() < 0.04,
                "the star at Spica's position must be Spica's magnitude, not {}",
                shipped.vmag
            );

            // The prediction, from the pinhole geometry and nothing else.
            let dir = crate::render::space::sky_direction(SPICA_RA, SPICA_DEC);
            assert!(dir.z < 0.0, "the star must be in front of this camera to be testable");
            let k = f64::from(size) / 2.0 / std::f64::consts::FRAC_PI_6.tan();
            let predicted_col = f64::from(size) / 2.0 + k * dir.x / -dir.z;
            let predicted_row = f64::from(size) / 2.0 - k * dir.y / -dir.z;
            let from_centre =
                (predicted_col - 256.0).hypot(predicted_row - 256.0);
            let radius = analytic_disc_radius_px(&camera);
            assert!(
                from_centre > radius + 25.0 && from_centre < 240.0,
                "Spica must land outside the {radius:.2} px silhouette and inside the \
                 frame for this test to mean anything; it landed {from_centre:.2} px \
                 from the centre at ({predicted_col:.2}, {predicted_row:.2})"
            );

            stars.write_sky(&ctx, &camera, &space);
            let clear = TilePipeline::clear_color(&space.color);
            let data = render_space_frame(
                &ctx,
                &pipeline,
                &camera,
                size,
                clear,
                &SpaceDraw { sky: None, stars: Some(&stars), backdrop: None, atmosphere: None, look: None },
                &[],
            )
            .await;

            let sky = [0u8, 0, 0];
            let sky_at = |col: u32, row: u32| rgb_gap(rgb_at(&data, size, col, row), sky);
            assert!(
                sky_at(6, 6) == 0,
                "a corner far from every star must be the space colour exactly, not {:?}",
                rgb_at(&data, size, 6, 6)
            );

            // The star's own brightest pixel, searched in a 5 px box around
            // the prediction. A box rather than the single predicted pixel
            // because a 2 px-radius disc's brightest pixel is the one whose
            // CENTRE is nearest the star, which is up to half a pixel off.
            let mut best = (0i32, 0u32, 0u32);
            for row in -5i32..=5 {
                for col in -5i32..=5 {
                    let c = (predicted_col.round() as i32 + col) as u32;
                    let r = (predicted_row.round() as i32 + row) as u32;
                    let lit = sky_at(c, r);
                    if lit > best.0 {
                        best = (lit, c, r);
                    }
                }
            }
            assert!(
                best.0 > 100,
                "Spica must LIGHT the pixel the camera puts it at: the brightest pixel \
                 within 5 px of ({predicted_col:.2}, {predicted_row:.2}) is only {} above \
                 the space colour",
                best.0
            );
            let offset = (f64::from(best.1) - predicted_col).hypot(f64::from(best.2) - predicted_row);
            assert!(
                offset < 2.0,
                "Spica's brightest pixel is at ({}, {}), {offset:.2} px from the \
                 predicted ({predicted_col:.2}, {predicted_row:.2}) -- more than \
                 rasterisation can explain",
                best.1,
                best.2
            );

            // ...and DARK a known distance away. 20 px straight up, where
            // the nearest catalogue star is Spica itself (verified by
            // projecting all 9,096: every other star is further than 20 px
            // from this point) against a largest billboard radius of
            // 2.46 px.
            let dark_col = predicted_col.round() as u32;
            let dark_row = (predicted_row.round() - 20.0) as u32;
            assert_eq!(
                sky_at(dark_col, dark_row),
                0,
                "20 px from Spica must be the space colour exactly; got {:?} at \
                 ({dark_col}, {dark_row}). A star field that lights that pixel is \
                 drawing discs 20 px across, not stars",
                rgb_at(&data, size, dark_col, dark_row)
            );
        });
    }

    /// **THE STARS ARE FIXED TO THE GROUND, IN PIXELS: pan the map and the
    /// sky turns with the planet; zoom and it does not move at all.**
    ///
    /// # What this test used to assert, and why that was wrong
    ///
    /// It was `panning_the_map_turns_the_planet_and_leaves_the_sky_still`,
    /// and its claim was that Spica lights the SAME pixel at centre
    /// longitude 0 and centre longitude 90 -- "the sky is inertial, so
    /// panning must not move it" -- with the planet's four-coloured tiles
    /// as the control that something had moved. Task 8 sabotage-checked it
    /// by adding `Rx(lat) * Ry(-lon)` to `space::sky_view_proj` and
    /// recorded the resulting failure as proof the test worked.
    ///
    /// **It was defending a defect.** A camera's centre longitude and
    /// latitude are where the eye SITS on the planet, so they decide which
    /// way it LOOKS; a camera over the Pacific and a camera over Africa do
    /// not see the same stars. With those two rotations omitted the star
    /// field slid along with the viewport while the planet turned
    /// underneath it, a defect that came back once after its first fix.
    ///
    /// # The prediction comes from the PLANET's matrix, not the sky's
    ///
    /// For each camera the direction is rotated by the rotational part of
    /// `GlobeFrame::globe_view` -- the planet's own chain, with its radius
    /// divided out -- and then put on the screen by the pinhole geometry
    /// alone, `k = (size/2)/tan(fov_y/2)`, `col = size/2 + k*x/(-z)`. No
    /// sky matrix, no near plane, no far plane, no sky radius. So this
    /// asserts the thing the defect denied: **a star sits where the ground
    /// puts it.**
    ///
    /// # Measured
    ///
    /// ```text
    ///   camera         what is measured there          pixel      excess over space
    ///   lon 0,   z1.5  Spica, where the ground puts it (83.1, 349.9)      235
    ///   lon 90,  z1.5  that same pixel                 (83.1, 349.9)        0
    ///   lon 90,  z1.5  the brightest star the ground
    ///                  puts over THAT camera, V = 0.75 (23.2, 177.9)      242
    ///   lon 360, z1.5  Spica again                     (83.1, 349.9)      235
    ///   lon 0,   z1.0  Spica again                     (83.1, 349.9)      235
    ///
    ///   planet probe (196, 216)   lon 0 (255,0,0)   lon 90 (0,255,0)   lon 360 (255,0,0)
    ///   shrink probe (399, 256)   z1.5 on the planet (disc 161.38 px)
    ///                             z1.0 off it        (disc 123.72 px)
    /// ```
    ///
    /// The 235 is the same byte at lon 0, lon 360 and z1.0, and an exact 0
    /// at lon 90 -- not "dimmer", nothing at all.
    ///
    /// Longitude 360 is the SAME GROUND POINT under the camera, so it must
    /// move neither the star nor the planet; longitude 90 must move both;
    /// the zoom must move the planet and not the star.
    #[test]
    fn panning_the_map_turns_the_planet_and_the_sky_with_it() {
        // Four frames where there used to be two, so this test now draws
        // twice as much into `render::alloc`'s process-global `DRAW_CALLS`.
        // See `no_pale_wedge_rings_the_pole_at_any_transition` for what
        // reads that counter and why an exact zero over there needs this
        // lock over here.
        #[cfg(feature = "probe")]
        let _serialised = crate::render::alloc::phase_lock();
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let stars = crate::render::space::StarPipeline::new(
                &ctx,
                wgpu::TextureFormat::Rgba8Unorm,
                pipeline.depth_format(),
            );
            let size = 512u32;
            let extent = 4096u32;
            let space = crate::render::space::Space {
                star_intensity: 1.0,
                ..crate::render::space::Space::default()
            };
            let clear = TilePipeline::clear_color(&space.color);
            let k = f64::from(size) / 2.0 / std::f64::consts::FRAC_PI_6.tan();

            /// Where the GROUND's own frame puts a sky direction, in pixels.
            /// The rotation is read off `globe_view`, with the sphere's
            /// radius divided out; the screen position is the pinhole
            /// formula. Returns `None` for a direction behind the camera.
            fn ground_px(
                camera: &crate::render::Camera,
                ra: f64,
                dec: f64,
                size: u32,
                k: f64,
            ) -> Option<(f64, f64)> {
                let frame = crate::render::globe::globe_frame(camera);
                let rot = glam::DMat3::from_mat4(frame.globe_view)
                    * (1.0 / crate::render::globe::globe_radius_world(camera));
                let d = rot * crate::render::space::sky_direction(ra, dec);
                if d.z >= 0.0 {
                    return None;
                }
                Some((
                    f64::from(size) / 2.0 + k * d.x / -d.z,
                    f64::from(size) / 2.0 - k * d.y / -d.z,
                ))
            }

            // Four z1 tiles, four colours. `y = 0` is the NORTHERN row. A
            // uniform sphere looks the same however you turn it, so this is
            // how the planet is given a position at all.
            let tile_color = |tile: crate::geo::TileId| match (tile.x, tile.y) {
                (0, 0) => crate::style::Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
                (1, 0) => crate::style::Rgba { r: 0.0, g: 1.0, b: 0.0, a: 1.0 },
                (0, 1) => crate::style::Rgba { r: 0.0, g: 0.0, b: 1.0, a: 1.0 },
                _ => crate::style::Rgba { r: 1.0, g: 1.0, b: 0.0, a: 1.0 },
            };

            let cameras = [
                ("lon 0", space_camera(0.0, 0.0)),
                ("lon 90", space_camera(90.0, 0.0)),
                ("lon 360", space_camera(360.0, 0.0)),
                ("zoom 1", crate::render::Camera { zoom: 1.0, ..space_camera(0.0, 0.0) }),
            ];

            let mut frames = Vec::new();
            let mut radii = Vec::new();
            for (_, camera) in &cameras {
                radii.push(analytic_disc_radius_px(camera));
                let frame = crate::render::globe::globe_frame(camera);
                let mut meshes = Vec::new();
                let mut uniforms = Vec::new();
                for y in 0..2u32 {
                    for x in 0..2u32 {
                        let tile = crate::geo::TileId { z: 1, x, y };
                        let plan = crate::render::space::backdrop_plan(
                            extent,
                            tile_color(tile),
                            tile_color(tile),
                        );
                        meshes.push(crate::render::tessellate_plan(
                            &plan,
                            extent,
                            crate::render::mesh::COORD_SCALE,
                            crate::render::subdivide::Subdivision::for_tile(tile),
                        ));
                        let tile_world_size = crate::render::WORLD_SIZE / 2f32.powi(1);
                        uniforms.push((
                            crate::render::combined_tile_matrix(camera, tile, extent as f32)
                                .to_cols_array_2d(),
                            GlobeParams {
                                globe_matrix: frame.globe_view_proj.as_mat4().to_cols_array_2d(),
                                tile_mercator_coords:
                                    crate::render::globe::tile_mercator_coords(tile),
                                clipping_plane: frame.clipping_plane.map(|c| c as f32),
                                transition: 1.0,
                                lyon_to_tile_unit: 1.0
                                    / (crate::render::mesh::COORD_SCALE * extent as f32),
                                tile_units_per_px: 1.0
                                    / (tile_world_size * 2f32.powf(camera.zoom as f32)),
                                // Unlit: this test grades four exact tile
                                // colours. See `UNLIT_SUN`.
                                ..GlobeParams::FLAT
                            }
                            .unlit(),
                        ));
                    }
                }
                let buffers: Vec<TileBuffers> =
                    meshes.iter().map(|m| pipeline.create_tile_buffers(&ctx, m)).collect();
                let groups: Vec<wgpu::BindGroup> = uniforms
                    .iter()
                    .map(|(matrix, globe)| {
                        pipeline.create_model_bind_group(&ctx, *matrix, 1.0, 0.0, *globe).1
                    })
                    .collect();
                let draws: Vec<(&TileBuffers, &wgpu::BindGroup)> =
                    buffers.iter().zip(groups.iter()).collect();

                stars.write_sky(&ctx, camera, &space);
                frames.push(
                    render_space_frame(
                        &ctx,
                        &pipeline,
                        camera,
                        size,
                        clear,
                        &SpaceDraw { sky: None, stars: Some(&stars), backdrop: None, atmosphere: None, look: None },
                        &draws,
                    )
                    .await,
                );
            }

            let sky = [0u8, 0, 0];
            // The brightest pixel within 3 px of a prediction, as an excess
            // over the space colour. A box because a 2 px-radius disc's
            // brightest pixel is the one whose CENTRE is nearest the star.
            let brightest = |data: &Vec<u8>, col: f64, row: f64| {
                let mut best = 0i32;
                for dr in -3i32..=3 {
                    for dc in -3i32..=3 {
                        best = best.max(rgb_gap(
                            rgb_at(
                                data,
                                size,
                                (col.round() as i32 + dc) as u32,
                                (row.round() as i32 + dr) as u32,
                            ),
                            sky,
                        ));
                    }
                }
                best
            };

            // Spica, J2000 -- the brightest catalogue star in the annulus
            // between the silhouette and the frame edge at `lon 0`.
            const SPICA_RA: f64 = 201.298_33;
            const SPICA_DEC: f64 = -11.161_39;
            let (spica_col, spica_row) = ground_px(&cameras[0].1, SPICA_RA, SPICA_DEC, size, k)
                .expect("Spica is in front of the lon 0 camera");

            // 1. AT LON 0 Spica lights its pixel...
            let lit_0 = brightest(&frames[0], spica_col, spica_row);
            assert!(
                lit_0 > 100,
                "Spica must light ({spica_col:.0}, {spica_row:.0}) at lon 0: measured \
                 {lit_0} above the space colour"
            );

            // 2. ...AND AT LON 90 IT IS GONE FROM THAT PIXEL. The camera has
            // moved a quarter of the way round the planet, so it faces a
            // different quarter of the sky: the ground frame carries Spica
            // to 69 degrees off the axis, i.e. off the frame entirely.
            // Asserted, not assumed -- and so is the absence of any OTHER
            // catalogue star near that pixel, or "dark" would be an
            // accident of which star happened to land there.
            assert!(
                ground_px(&cameras[1].1, SPICA_RA, SPICA_DEC, size, k)
                    .is_none_or(|(c, r)| c < -8.0 || c > f64::from(size) + 8.0 || r < -8.0
                        || r > f64::from(size) + 8.0),
                "the ground frame must carry Spica out of the frame at lon 90, or this \
                 arm is not testing what it says"
            );
            let intruder = crate::render::space::catalogue().find(|star| {
                ground_px(&cameras[1].1, f64::from(star.ra_deg), f64::from(star.dec_deg), size, k)
                    .is_some_and(|(c, r)| (c - spica_col).hypot(r - spica_row) < 8.0)
            });
            assert!(
                intruder.is_none(),
                "no catalogue star may land within 8 px of ({spica_col:.0}, \
                 {spica_row:.0}) at lon 90, or the darkness below proves nothing: \
                 {intruder:?}"
            );
            let lit_90 = brightest(&frames[1], spica_col, spica_row);
            assert_eq!(
                lit_90, 0,
                "at lon 90 Spica's lon-0 pixel must be the space colour EXACTLY -- the \
                 sky turned with the ground. Measured {lit_90} above it; a sky that \
                 ignored the camera's longitude would still read about {lit_0} here"
            );

            // 3. ...and the sky that IS over the lon 90 camera lands where
            // the ground puts it. The positive half: "gone" alone would be
            // satisfied by a sky that was not drawn.
            let mut best_star: Option<(f32, f64, f64)> = None;
            for star in crate::render::space::catalogue() {
                let Some((col, row)) =
                    ground_px(&cameras[1].1, f64::from(star.ra_deg), f64::from(star.dec_deg), size, k)
                else {
                    continue;
                };
                let from_centre = (col - 256.0).hypot(row - 256.0);
                if col < 8.0
                    || col > f64::from(size) - 8.0
                    || row < 8.0
                    || row > f64::from(size) - 8.0
                    || from_centre < radii[1] + 25.0
                {
                    continue;
                }
                if best_star.is_none_or(|(mag, _, _)| star.vmag < mag) {
                    best_star = Some((star.vmag, col, row));
                }
            }
            let (mag, col, row) = best_star.expect("some star is over the lon 90 camera");
            let lit_new = brightest(&frames[1], col, row);
            assert!(
                lit_new > 100,
                "the brightest star the GROUND puts over the lon 90 camera (V = {mag}) \
                 must light ({col:.0}, {row:.0}): measured {lit_new}. If this fails the \
                 sky is not turning with the planet, it is merely not being drawn"
            );

            // 4. A WHOLE TURN OF LONGITUDE is the same ground point, so it
            // must move neither the star nor the planet.
            let lit_360 = brightest(&frames[2], spica_col, spica_row);
            assert_eq!(
                lit_360, lit_0,
                "360 degrees of longitude is the same ground point under the camera: \
                 Spica must read the same byte, {lit_0}, not {lit_360}"
            );

            // 5. THE ZOOM MOVES NO STAR. A star is at infinity.
            //
            // The fourth camera zooms OUT, to 1.0, not in. Zooming in grows
            // the silhouette -- 161.4 px at z1.5, 456.6 px at z3.0 -- and
            // Spica is only 196.7 px from the centre, so an arm at z3.0
            // measures the PLANET at Spica's pixel and reads 255 for a red
            // tile. That is the first draft of this arm, and what it
            // measured.
            let lit_z1 = brightest(&frames[3], spica_col, spica_row);
            assert_eq!(
                lit_z1, lit_0,
                "zooming from 1.5 to 1.0 must leave Spica on the same pixel at the same \
                 brightness: {lit_0} against {lit_z1}"
            );

            // 6. THE PLANET'S OWN PROBES, so none of the above is a
            // statement about a frame in which nothing moved.
            //
            // 60 px left of and 40 px above the centre. Left is west
            // (`the_globe_draws_east_on_the_right_like_the_flat_map_does`),
            // so at lon 0 it sits at a negative longitude -- tile `x = 0` --
            // and at lon 90 near longitude 68, still inside tile `x = 1`.
            let (probe_col, probe_row) = (size / 2 - 60, size / 2 - 40);
            let probe = |data: &Vec<u8>| rgb_at(data, size, probe_col, probe_row);
            assert_eq!(
                probe(&frames[0]),
                [255, 0, 0],
                "at lon 0 the probe at ({probe_col}, {probe_row}) must be in the \
                 north-WEST z1 tile (red); got {:?}",
                probe(&frames[0])
            );
            assert_eq!(
                probe(&frames[1]),
                [0, 255, 0],
                "at lon 90 the same probe must be in the north-EAST z1 tile (green) -- \
                 the planet rotated under it; got {:?}",
                probe(&frames[1])
            );
            assert_eq!(
                probe(&frames[2]),
                probe(&frames[0]),
                "at lon 360 the planet must be back exactly where it was"
            );
            // And the zoom's own control: a pixel INSIDE the z1.5
            // silhouette is outside the z1.0 one, because the disc shrinks.
            let shrink_col = (256.0 + (radii[0] + radii[3]) / 2.0).round() as u32;
            assert!(
                radii[3] < radii[0] - 30.0,
                "the shrink probe needs the disc to shrink: {:.1} px at z1.5 against \
                 {:.1} px at z1.0",
                radii[0],
                radii[3]
            );
            assert_ne!(
                rgb_at(&frames[0], size, shrink_col, size / 2),
                [0, 0, 0],
                "at z1.5 the shrink probe must be ON the planet"
            );
            assert_eq!(
                rgb_at(&frames[3], size, shrink_col, size / 2),
                [0, 0, 0],
                "at z1.0 the same pixel must be OFF the planet -- the zoom moved the \
                 planet, which is what makes 'the zoom moved no star' a measurement"
            );
        });
    }

    /// **THE ACCEPTANCE CRITERION FOR TASK 6: a label's anchor lands on the
    /// feature it names, checked against the GPU's own pixels rather than
    /// against a second copy of the CPU's arithmetic.**
    ///
    /// Four z1 tiles in four colours, one anchor well inside each, at a
    /// camera showing a 70-degree cap of the planet. `symbol_pass::
    /// AnchorProjection` says where each anchor is on screen; the rendered
    /// frame says which tile that pixel belongs to. They must agree, for all
    /// four.
    ///
    /// # Why a rendered frame and not a formula
    ///
    /// The symbol pass and the vertex shader reach the sphere by two
    /// different routes -- `globe::mercator_to_sphere` in f64 on one side,
    /// `project_to_sphere` in f32 WGSL on the other -- and only one of them
    /// applies `shader.wgsl`'s horizontal flip, because that flip exists to
    /// undo `Camera::eye_target_up`'s basis and the globe matrix does not use
    /// that basis. The first draft of `AnchorProjection::screen` flipped the
    /// blended result and mirrored every label about the central meridian.
    /// Its unit test did not notice, because the same author wrote the
    /// prediction with the same sign. Reading a real pixel is the only check
    /// that could not make that mistake twice.
    ///
    /// # Why the mirror probe is asserted too
    ///
    /// Each anchor's pixel is also sampled mirrored about the vertical
    /// centre line, and asserted to be a DIFFERENT colour. Without that, a
    /// test on a symmetric fixture would pass either way round; with it, the
    /// exact defect this test was written for fails it.
    #[test]
    fn a_symbol_anchor_lands_on_the_tile_that_owns_it_on_the_globe() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 512u32;
            let extent = crate::mvt::MVT_EXTENT;
            let camera = space_camera(0.0, 0.0);
            assert_eq!(
                crate::render::globe::transition_for_zoom(camera.zoom),
                1.0,
                "this test is about the sphere, so the camera must be at full transition"
            );
            let frame = crate::render::globe::globe_frame(&camera);
            let anchors = crate::render::globe::GlobeAnchors::new(&frame);

            // Four z1 tiles, four colours. `y = 0` is the NORTHERN row and
            // `x = 0` the WESTERN column, the same mapping
            // `panning_the_map_turns_the_planet_and_leaves_the_sky_still`
            // uses.
            let tile_color = |tile: crate::geo::TileId| match (tile.x, tile.y) {
                (0, 0) => crate::style::Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
                (1, 0) => crate::style::Rgba { r: 0.0, g: 1.0, b: 0.0, a: 1.0 },
                (0, 1) => crate::style::Rgba { r: 0.0, g: 0.0, b: 1.0, a: 1.0 },
                _ => crate::style::Rgba { r: 1.0, g: 1.0, b: 0.0, a: 1.0 },
            };
            let rgb = |c: crate::style::Rgba| {
                [(c.r * 255.0) as u8, (c.g * 255.0) as u8, (c.b * 255.0) as u8]
            };

            let mut meshes = Vec::new();
            let mut uniforms = Vec::new();
            let mut tiles = Vec::new();
            for y in 0..2u32 {
                for x in 0..2u32 {
                    let tile = crate::geo::TileId { z: 1, x, y };
                    tiles.push(tile);
                    let plan = crate::render::space::backdrop_plan(
                        extent,
                        tile_color(tile),
                        tile_color(tile),
                    );
                    meshes.push(crate::render::tessellate_plan(
                        &plan,
                        extent,
                        crate::render::mesh::COORD_SCALE,
                        crate::render::subdivide::Subdivision::for_tile(tile),
                    ));
                    let globe = crate::render::globe::GlobeUpload::new(
                        &frame, &camera, extent as f32, UNLIT_SUN, UNLIT_NIGHT,
                    );
                    uniforms.push((
                        crate::render::combined_tile_matrix(&camera, tile, extent as f32)
                            .to_cols_array_2d(),
                        globe.for_tile(tile),
                    ));
                }
            }
            let buffers: Vec<TileBuffers> =
                meshes.iter().map(|m| pipeline.create_tile_buffers(&ctx, m)).collect();
            let groups: Vec<wgpu::BindGroup> = uniforms
                .iter()
                .map(|(matrix, globe)| {
                    pipeline.create_model_bind_group(&ctx, *matrix, 1.0, 0.0, *globe).1
                })
                .collect();
            let draws: Vec<(&TileBuffers, &wgpu::BindGroup)> =
                buffers.iter().zip(groups.iter()).collect();
            let data = render_space_frame(
                &ctx,
                &pipeline,
                &camera,
                size,
                wgpu::Color::BLACK,
                &SpaceDraw { sky: None, stars: None, backdrop: None, atmosphere: None, look: None },
                &draws,
            )
            .await;

            // One anchor per tile, at lon +/-35 and lat +/-25 -- 42 degrees
            // of arc from the camera's axis, so comfortably inside the
            // 70-degree cap this camera shows, and comfortably away from
            // both the limb and the tile boundaries the equator and the
            // central meridian draw. The MVT coordinates are the tile-local
            // ones for those lon/lats; each is named with the degrees it
            // came from so a reader can check it without re-deriving the
            // Mercator y.
            let probes = [
                (crate::geo::TileId { z: 1, x: 0, y: 0 }, geo_types::Point::new(3300, 3507), "lon -35, lat +25"),
                (crate::geo::TileId { z: 1, x: 1, y: 0 }, geo_types::Point::new(796, 3507), "lon +35, lat +25"),
                (crate::geo::TileId { z: 1, x: 0, y: 1 }, geo_types::Point::new(3300, 588), "lon -35, lat -25"),
                (crate::geo::TileId { z: 1, x: 1, y: 1 }, geo_types::Point::new(796, 588), "lon +35, lat -25"),
            ];
            for (tile, anchor, name) in probes {
                let projection = crate::symbol_pass::AnchorProjection::new(&camera, tile, &anchors);
                assert!(
                    !projection.occluded(anchor),
                    "{name} is 42 degrees from the camera's axis and must be on the NEAR side"
                );
                let (x, y) = projection
                    .screen(&camera, anchor)
                    .expect("a near-side anchor is in front of the near plane");
                let (col, row) = (x.round() as u32, y.round() as u32);
                let found = rgb_at(&data, size, col, row);
                let expected = rgb(tile_color(tile));
                eprintln!(
                    "{name} -> tile {}/{}/{}: pixel ({col}, {row}) reads {found:?}, want {expected:?}",
                    tile.z, tile.x, tile.y
                );
                assert_eq!(
                    found, expected,
                    "the anchor for {name} projected to ({col}, {row}), and the GPU drew \
                     {found:?} there rather than tile {}/{}/{}'s {expected:?} -- the CPU's \
                     anchor projection and the shader's vertex projection disagree",
                    tile.z, tile.x, tile.y
                );
                // The mirror probe: if the projection were flipped east for
                // west, THIS is the pixel it would have chosen, and it must
                // not be the same colour.
                let mirrored = rgb_at(&data, size, size - 1 - col, row);
                assert_ne!(
                    mirrored, expected,
                    "the pixel mirrored about the vertical centre line reads the same colour, \
                     so this probe cannot tell east from west and proves nothing about {name}"
                );
            }
        });
    }

    /// **The ocean sphere must not punch holes in the tiles drawn over
    /// it**, which is what `TilePipeline::backdrop` — the depth-write-off
    /// twin — exists to prevent.
    ///
    /// The backdrop is a `z0` tile-covering fill and the tiles over it are
    /// `z3`, and `shader.wgsl`'s `project_to_sphere` reaches the same
    /// sphere point from `(0,0) + (1,1)*p` for one and `(x/8,y/8) +
    /// (1/8,1/8)*p` for the other. Those agree mathematically and round
    /// differently in f32, so with `depth_compare: LessEqual` and the
    /// backdrop WRITING depth, every pixel where the backdrop's z landed
    /// the smaller of the two rejects the tile fragment drawn afterwards
    /// and shows ocean instead of land.
    ///
    /// Measured on an Apple M-series adapter over the 64 z3 tiles at
    /// lon 0, lat 0, z1.5, 512x512, with the backdrop magenta and the tiles
    /// red, counting only pixels 2 px inside the analytic 161.385 px limb:
    ///
    /// ```text
    ///   backdrop pipeline            tile px   backdrop px   backdrop share
    ///   depth_write_enabled: true     35,004        44,784            56.1%
    ///   depth_write_enabled: false    79,788             0             0.0%
    /// ```
    ///
    /// **56% of the planet**, not a rim and not a rounding artefact — the
    /// ULP goes either way per vertex, so the pattern is a coarse dither
    /// over the whole disc. The `true` row was measured by pointing this
    /// draw at `self.pipeline` and re-running; the assertion below is on the
    /// `false` one, which is the one that ships.
    #[test]
    fn the_ocean_sphere_does_not_punch_holes_in_the_tiles_over_it() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 512u32;
            let extent = 4096u32;
            let camera = space_camera(0.0, 0.0);
            // Magenta under green: the two are maximally distinguishable
            // per channel, so a single wrong pixel is unambiguous.
            let (backdrop_buffers, backdrop_group) = backdrop_draw(
                &ctx,
                &pipeline,
                &camera,
                extent,
                crate::style::Rgba { r: 1.0, g: 0.0, b: 1.0, a: 1.0 },
            );
            // z3, where `granularity_for_zoom` is 8 -- the level whose
            // 5.625-degree cells coincide with the z0 backdrop's, so the
            // two meshes really do put vertices at the same sphere points
            // and the ULP question is live rather than hypothetical.
            let (meshes, uniforms) = globe_level_draws(&camera, 3, extent, |tile| {
                crate::render::subdivide::Subdivision::for_tile(tile)
            });
            let buffers: Vec<TileBuffers> =
                meshes.iter().map(|m| pipeline.create_tile_buffers(&ctx, m)).collect();
            let groups: Vec<wgpu::BindGroup> = uniforms
                .iter()
                .map(|(matrix, globe)| {
                    pipeline.create_model_bind_group(&ctx, *matrix, 1.0, 0.0, *globe).1
                })
                .collect();
            let draws: Vec<(&TileBuffers, &wgpu::BindGroup)> =
                buffers.iter().zip(groups.iter()).collect();

            let space = crate::render::space::Space::default();
            let data = render_space_frame(
                &ctx,
                &pipeline,
                &camera,
                size,
                TilePipeline::clear_color(&space.color),
                &SpaceDraw {
                    sky: None,
                    stars: None,
                    backdrop: Some((&backdrop_buffers, &backdrop_group)),
                    atmosphere: None,
                    look: None,
                },
                &draws,
            )
            .await;

            let radius = analytic_disc_radius_px(&camera);
            let mut magenta = 0usize;
            let mut tiles = 0usize;
            for row in 0..size {
                for col in 0..size {
                    // Two pixels in from the analytic limb, so the
                    // antialiased rim -- where the backdrop legitimately
                    // shows through a tile's own coverage -- is not counted.
                    if (f64::from(col) - 255.5).hypot(f64::from(row) - 255.5) > radius - 2.0 {
                        continue;
                    }
                    // `globe_level_draws` fills its tiles RED, so the two
                    // are told apart by the BLUE channel alone: magenta has
                    // it, red does not.
                    let rgb = rgb_at(&data, size, col, row);
                    if rgb[0] > 128 && rgb[2] > 128 {
                        magenta += 1;
                    } else if rgb[0] > 128 {
                        tiles += 1;
                    }
                }
            }
            assert!(
                tiles > 60_000,
                "the control: the tiles must actually cover the disc, and only {tiles} \
                 pixels of it are theirs"
            );
            assert_eq!(
                magenta, 0,
                "{magenta} pixels inside the silhouette show the OCEAN SPHERE rather \
                 than the tile drawn over it, of {} the tiles claimed. The backdrop is \
                 writing depth",
                tiles + magenta
            );
        });
    }

    /// **A style layer drawn LATER must cover the one drawn before it, at
    /// every transition** — the painter's order the whole style spec is
    /// defined in.
    ///
    /// # The defect this pins
    ///
    /// Reported as "pale green slivers and wedges lying on open water", in
    /// the Malacca Strait and across the Indian Ocean. They are the Liberty
    /// style's `park` layer — OpenMapTiles' `class=marine` protected areas,
    /// which really do span open sea — showing THROUGH the ocean. `park` is
    /// style layer 2 and `water` is style layer 17 with an opaque
    /// `rgb(158,189,255)`, so a marine park is supposed to be invisible:
    /// the ocean is painted over it.
    ///
    /// It was not invisible, because `TilePipeline::new` built the tile
    /// pipeline with `depth_write_enabled: true` while `depth_compare` is
    /// `LessEqual`. Every fill in a tile's `PaintPlan` therefore wrote
    /// depth, and the next fill over the same ground was tested against it
    /// — and the two are NOT coplanar. `depth_compare`'s own comment
    /// asserted they were ("all v1-scope geometry sits at z=0"), which is
    /// true of the Mercator path and false of the globe:
    /// `interpolate_projection` writes `result.z = result.w * (1 -
    /// clip_ramp * (1 - globe_clipping_z(...)))`, a per-VERTEX value that
    /// carries the far-side clip. Two different triangulations covering the
    /// same pixel interpolate it differently and round differently in f32,
    /// so wherever the earlier layer's z landed the smaller one the later
    /// layer's fragment was REJECTED and the earlier layer showed.
    ///
    /// This is the same failure, between two style layers of one tile, that
    /// `the_ocean_sphere_does_not_punch_holes_in_the_tiles_over_it` pins
    /// between the backdrop and the tiles — and it has the same fix, which
    /// is that a tile fill has no reason to write depth at all. The
    /// planet's surface reaches the depth buffer through
    /// `TilePipeline::backdrop_depth`, drawn after the tiles for exactly
    /// that purpose, so nothing downstream loses anything.
    ///
    /// Measured through `examples/globe_capture`, lon 97 lat -13, 2048 px,
    /// `--only-layer background,park,water`, as the ocean's own pixel count
    /// with and without the park layer in front of it:
    ///
    /// ```text
    ///   camera            water alone    with park     ocean pixels lost
    ///   z4.5  (t 0.500)     3,563,988    3,364,830     199,158
    ///   z5.99 (t 0.000)     4,046,719    3,999,974      46,745
    /// ```
    ///
    /// After: 4 and 0. It is not a globe-only defect — the flat row is the
    /// one that says so, and it reaches every consumer of this engine at
    /// city zooms, not just the globe.
    ///
    /// # Why two opaque colours rather than the real style's blend
    ///
    /// The two fills here are the same full-tile square, so they are
    /// exactly coincident and every fragment is the ULP question. Both are
    /// opaque, so "the later layer won" needs no compositing arithmetic:
    /// any green pixel at all is a blue fragment that was thrown away.
    #[test]
    fn a_later_style_layer_covers_an_earlier_one_at_every_transition() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 512u32;
            let extent = 4096u32;
            let e = extent as i32;
            let green = crate::style::Rgba { r: 0.0, g: 1.0, b: 0.0, a: 1.0 };
            let blue = crate::style::Rgba { r: 0.0, g: 0.0, b: 1.0, a: 1.0 };
            let square = std::sync::Arc::new(geo_types::Geometry::Polygon(
                geo_types::Polygon::new(
                    geo_types::LineString::from(vec![(0, 0), (e, 0), (e, e), (0, e), (0, 0)]),
                    Vec::new(),
                ),
            ));
            // **A DIFFERENT shape from the square, and that is the whole
            // point.** Two fills tessellated from the SAME ring get the same
            // vertices and therefore bit-identical z, so `LessEqual` passes
            // and the defect cannot appear -- the first draft of this test
            // used two squares and PASSED with the bug reintroduced. The ULP
            // disagreement needs two DIFFERENT triangulations covering the
            // same pixel, which is what a marine park polygon under the
            // ocean's own ring actually is.
            let disc: Vec<(i32, i32)> = (0..=64)
                .map(|i| {
                    let a = f64::from(i) / 64.0 * std::f64::consts::TAU;
                    let r = f64::from(e) * 0.4;
                    (
                        (f64::from(e) / 2.0 + r * a.cos()) as i32,
                        (f64::from(e) / 2.0 + r * a.sin()) as i32,
                    )
                })
                .collect();
            // The `park` then `water` of the report, in one tile's plan and
            // in that order: a marine protected area, then the opaque ocean
            // that is supposed to hide it.
            let plan = crate::style::PaintPlan {
                ops: vec![
                    crate::style::PaintOp::Fill {
                        color_from: green,
                        color_to: green,
                        geometry: std::sync::Arc::new(geo_types::Geometry::Polygon(
                            geo_types::Polygon::new(
                                geo_types::LineString::from(disc),
                                Vec::new(),
                            ),
                        )),
                    },
                    crate::style::PaintOp::Fill {
                        color_from: blue,
                        color_to: blue,
                        geometry: square,
                    },
                ],
            };

            for (zoom, level) in [(3.0f64, 3u8), (3.5, 3), (4.0, 4), (4.5, 4), (5.0, 4)] {
                let camera = crate::render::Camera {
                    center_lon: 105.0,
                    center_lat: 3.0,
                    zoom,
                    pitch: 0.0,
                    bearing: 0.0,
                    viewport_width_px: size as f32,
                    viewport_height_px: size as f32,
                    fov_y: crate::render::Camera::DEFAULT_FOV_Y,
                };
                let frame = crate::render::globe::globe_frame(&camera);
                let cover = crate::render::globe::GlobeCover::new(&frame);
                let transition = crate::render::globe::transition_for_zoom(zoom);
                let side = 1u32 << level;
                let mut meshes = Vec::new();
                let mut uniforms = Vec::new();
                for y in 0..side {
                    for x in 0..side {
                        let tile = crate::geo::TileId { z: level, x, y };
                        // The VISIBLE tiles this time: the defect is on the
                        // face of the planet, not behind its horizon.
                        if !cover.tile_visible(tile) {
                            continue;
                        }
                        let tile_world_size =
                            crate::render::WORLD_SIZE / 2f32.powi(i32::from(level));
                        meshes.push(crate::render::tessellate_plan(
                            &plan,
                            extent,
                            crate::render::mesh::COORD_SCALE,
                            crate::render::subdivide::Subdivision::for_tile(tile),
                        ));
                        uniforms.push((
                            crate::render::combined_tile_matrix(&camera, tile, extent as f32)
                                .to_cols_array_2d(),
                            GlobeParams {
                                globe_matrix: frame.globe_view_proj.as_mat4().to_cols_array_2d(),
                                tile_mercator_coords: crate::render::globe::tile_mercator_coords(
                                    tile,
                                ),
                                clipping_plane: frame.clipping_plane.map(|c| c as f32),
                                transition,
                                lyon_to_tile_unit: 1.0
                                    / (crate::render::mesh::COORD_SCALE * extent as f32),
                                tile_units_per_px: 1.0
                                    / (tile_world_size * 2f32.powf(camera.zoom as f32)),
                                ..GlobeParams::FLAT
                            }
                            .unlit(),
                        ));
                    }
                }
                assert!(
                    !meshes.is_empty(),
                    "z{zoom}: no visible tile at level {level}, so this row grades nothing"
                );
                let data =
                    render_globe_level(&ctx, &pipeline, &camera, size, &meshes, &uniforms).await;

                // Pixels the tiles actually painted, then ERODED by two,
                // so the measurement is the interior of the silhouette and
                // not its edge. `the_ocean_sphere_does_not_punch_holes_in_
                // the_tiles_over_it` insets from the analytic limb for the
                // same reason and says why; here the disc is wider than the
                // frame, so the limb only clips the corners and an erosion
                // of the painted mask is the same idea without needing the
                // radius. Without it this test also grades the silhouette
                // edge, where a tile's projected outline is degenerate and
                // a handful of pixels (30 of 257,416 at z3) show the
                // earlier layer for reasons that have nothing to do with
                // depth -- a separate, known limb-geometry question.
                let painted: Vec<bool> = (0..(size * size))
                    .map(|i| data[(i * 4 + 3) as usize] != 0)
                    .collect();
                let interior = |col: u32, row: u32| -> bool {
                    if col < 2 || row < 2 || col + 2 >= size || row + 2 >= size {
                        return false;
                    }
                    for dy in -2i32..=2 {
                        for dx in -2i32..=2 {
                            let c = (col as i32 + dx) as u32;
                            let r = (row as i32 + dy) as u32;
                            if !painted[(r * size + c) as usize] {
                                return false;
                            }
                        }
                    }
                    true
                };
                let (mut covered, mut leaked) = (0usize, 0usize);
                for row in 0..size {
                    for col in 0..size {
                        if !interior(col, row) {
                            continue;
                        }
                        covered += 1;
                        // Told apart by one channel each: the later layer is
                        // pure blue, the earlier one pure green.
                        if rgb_at(&data, size, col, row)[1] > 128 {
                            leaked += 1;
                        }
                    }
                }
                println!(
                    "z{zoom} L{level} (transition {transition:.4}): {covered} covered px, \
                     {leaked} showing the EARLIER layer"
                );
                assert!(
                    covered > 20_000,
                    "z{zoom}: the control -- the tiles must actually cover the frame, and \
                     only {covered} pixels are theirs"
                );
                assert_eq!(
                    leaked, 0,
                    "z{zoom} (transition {transition:.4}): {leaked} of {covered} covered \
                     pixels show the style layer drawn FIRST instead of the opaque one \
                     drawn over it. A tile fill is writing depth and rejecting the next \
                     layer through `LessEqual`; this is the `park` layer showing through \
                     `water` on open sea."
                );
            }
        });
    }

    /// The glow just outside the drawn planet, per ray: how far the
    /// atmosphere frame departs from the space colour, and where the planet
    /// actually ends.
    ///
    /// Returns `(glow, limb_px)` per ray, in the order of `angles`.
    ///
    /// # Why the CONTROL frame decides where to start looking
    ///
    /// The lit shell is
    /// `(ATMOSPHERE_RADIUS_M / EARTH_RADIUS_M - 1) * 161.385` = **2.53 px**
    /// wide, and it is brightest at its INNER edge, where the air a grazing
    /// ray crosses is thickest. A fixed inset from the analytic limb either
    /// eats most of that — the first draft used 1.5 px, which left a 1 px
    /// fringe at the outer edge and measured, for the same sun reflected
    /// through the centre, a peak of 131 one way and 49 the other — or, if
    /// made smaller, starts sampling the planet and reports its ocean
    /// colour as glow (at an inset of 0, the control's brightest sample is
    /// **153**, which is the ocean).
    ///
    /// So the inner edge is not guessed. Each ray walks outward and starts
    /// measuring at the first radius where the CONTROL frame — the identical
    /// frame with the atmosphere withheld — reads the space colour exactly.
    /// That is the drawn planet's own edge along that ray, including the
    /// subdivided polygon's departure from a true circle, and it is returned
    /// so the caller can check it against the analytic limb rather than
    /// trust it.
    ///
    /// **FLOOR, not round**, converting a point to a pixel: pixel `i` covers
    /// `[i, i+1)` and its centre is `i + 0.5`. Rounding offsets every sample
    /// by half a pixel in each axis, which on a 2.53 px ring is enough to
    /// break the frame's own symmetry — that is the defect the 131-against-49
    /// reading above actually was.
    fn glow_around_the_limb(
        control: &[u8],
        lit: &[u8],
        size: u32,
        limb_px: f64,
        space: [u8; 3],
        angles: &[f64],
    ) -> Vec<(i32, f64)> {
        let shell = crate::render::atmosphere::ATMOSPHERE_RADIUS_M
            / crate::render::atmosphere::EARTH_RADIUS_M;
        let centre = f64::from(size) / 2.0;
        angles
            .iter()
            .map(|angle| {
                let mut best = 0;
                let mut edge = f64::NAN;
                let mut radius = limb_px - 4.0;
                while radius <= limb_px * shell + 2.0 {
                    // Screen bearing, clockwise from straight up -- the
                    // convention `render::atmosphere::Sun::Viewport`'s
                    // `bearing_deg` uses and `render::space`'s bearing tests
                    // already share.
                    let col = (centre + radius * angle.sin()).floor() as i64;
                    let row = (centre - radius * angle.cos()).floor() as i64;
                    radius += 0.25;
                    if col < 0 || row < 0 || col >= i64::from(size) || row >= i64::from(size) {
                        continue;
                    }
                    let (col, row) = (col as u32, row as u32);
                    if rgb_at(control, size, col, row) != space {
                        continue;
                    }
                    if edge.is_nan() {
                        edge = radius - 0.25;
                    }
                    best = best.max(rgb_gap(rgb_at(lit, size, col, row), space));
                }
                (best, edge)
            })
            .collect()
    }

    /// **THE ACCEPTANCE CRITERION FOR TASK 9: outside the limb there is a
    /// glow, far from the planet there is not, the planet is not washed out,
    /// and the lit side is brighter than the dark side.**
    ///
    /// Four claims, one frame, and a control frame with the atmosphere
    /// switched off so that each of them is a DIFFERENCE rather than an
    /// absolute reading. The camera is the shared `space_camera` — lon 0,
    /// lat 0, z1.5, 512x512 — whose analytic silhouette is 161.385 px, so
    /// the shell the glow can occupy is 161.385 to 163.92 px: **2.53 px
    /// wide**, which is what a 100 km atmosphere on a 6,371 km planet is and
    /// is why this is a rim rather than a halo.
    ///
    /// Measured on an Apple M-series adapter, ocean `#4f7499`, space black,
    /// and `Sun::VIEWPORT_DEFAULT` (tilt 55, bearing −45).
    ///
    /// **The sun is NAMED here and no longer taken from
    /// `Atmosphere::default()`**, which is now `Sun::Now` — the wall clock.
    /// This test predicts a screen bearing for the lit arc, and a
    /// clock-anchored sun has no screen bearing: it would put the
    /// terminator somewhere different every time the suite ran, and the
    /// antisunward-zero assertion below would pass or fail by the hour.
    /// `Sun::VIEWPORT_DEFAULT` is the constant that default used to be, so
    /// what this test measures is unchanged.
    ///
    /// ```text
    ///   probe                                   control        atmosphere
    ///   brightest just outside the limb            0              131
    ///   summed over the 21 rays facing the sun     0            2,109
    ///   summed over the 20 rays facing away        0                0
    ///   disc centre                          (79,116,153)   (79,116,154)
    ///   pixels changed beyond 165.42 px            -                0
    /// ```
    ///
    /// The first two rows were **142** and **2,310** before the
    /// path-length factor landed; the last was **(75,120,165)**. That is
    /// the trade the factor makes, and it is the one the plan asked for:
    /// the brightest rim pixel keeps **92.3%** of itself and the whole lit
    /// arc **91.3%**, while the air in front of the planet goes from 12
    /// byte units to 1. The antisunward zero is unmoved and still exact,
    /// which it has to be — the factor multiplies, so it can turn a zero
    /// into a zero and nothing else.
    ///
    /// Around the whole limb the lit arc runs from screen bearing 201
    /// through 0 to 69 and the dark arc from 72 to 198, both rounded to the
    /// 3-degree ray spacing. **The dark arc is 126 degrees and not 180**,
    /// and that is twilight rather than an error: the geometric terminator
    /// cuts the surface, while the shell 100 km above it stays in sunlight
    /// past that line, exactly as Earth's does.
    ///
    /// **The antisunward zero is the finding, and it is exact**: the
    /// night limb is dark because a sample there has its secondary ray pass
    /// through the planet, where the density term explodes and the
    /// attenuation goes to zero (`atmosphere_shader.wgsl`'s `scatter`). The
    /// terminator is therefore a consequence of the integral and not a
    /// special case in it. The two arcs hold 21 and 20 of the 120 rays and
    /// are measured by the same code, so what is being compared is the sun
    /// and nothing else — and it is an ATMOSPHERIC terminator: the planet
    /// under it is exactly as bright on the night side as on the day side,
    /// because nothing here shades the ground. See
    /// `render::atmosphere::Sun` for why that is, and what a `light`
    /// implementation would change.
    ///
    /// **The disc centre moves by 1 byte unit, and that bound is a
    /// DELIBERATE REVERSAL of what this test used to assert.**
    ///
    /// Task 9 wrote this pair of assertions:
    ///
    /// ```text
    ///   assert!(haze > 0,  "...the pass is drawing a ring outside the limb
    ///                       rather than air in front of the planet");
    ///   assert!(haze <= 16, "...that is not haze, that is washing the
    ///                       planet out");
    /// ```
    ///
    /// and it measured 12, comfortably inside the upper bound and
    /// deliberately away from the lower one. **Both halves of that are now
    /// overruled.** The user put this globe beside another renderer's and
    /// reported that ours "looks a bit shaded" and that the glow and the
    /// colours "unnaturally overlapped"; Task 10 then measured the whole
    /// radial profile and found the 12 at the centre was not a local
    /// reading but a **wash across 99.0% of the disc at z3, mean 13.63
    /// byte units**, desaturating as well as dimming. Haze in front of the
    /// planet is the defect, not the feature — so:
    ///
    ///  - **the upper bound is 16 -> 2**, the plan's own grade for the fix;
    ///  - **the lower bound is GONE.** "A pass that changed the centre by
    ///    nothing at all would be drawing a ring outside the limb rather
    ///    than air" was the reasoning, and a ring outside the limb is now
    ///    exactly what is wanted. What stops the bound being satisfied by
    ///    deleting the pass is claim 1 above — the rim, which still has to
    ///    read 100 byte units over space — and not a floor under the wash.
    ///
    /// The fix is `atmosphere_shader.wgsl`'s `path_fraction`, and
    /// `the_haze_in_front_of_the_planet_is_a_rim_and_not_a_wash` grades the
    /// whole disc at two cameras with the factor-less arm rendered beside
    /// it. Shipping the reference's 1/2.2 gamma instead moved this pixel by
    /// **102** and clipped the blue channel to 255 across the whole disc;
    /// `atmosphere_shader.wgsl`'s `fs_main` carries that table too, and
    /// neither decision undoes the other — the gamma was an amplifier on a
    /// wash, this is the wash itself.
    ///
    /// **Zero bytes beyond 1.03 x the limb** is the strongest of the four
    /// and it is exact rather than a tolerance: a ray that misses the shell
    /// returns `(0, 0, 0, 1)` from `scatter`, `fs_main` turns that into an
    /// alpha of 0, and premultiplied blending leaves `dst * (1 - 0) + 0`.
    /// So the glow cannot wash out the star field however bright it is set,
    /// and that is a property of the blend equation rather than of a
    /// threshold.
    #[test]
    fn the_glow_reads_outside_the_limb_and_does_not_wash_out_the_planet() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 512u32;
            let extent = 4096u32;
            let camera = space_camera(0.0, 0.0);
            let transition = crate::render::globe::transition_for_zoom(camera.zoom);
            assert_eq!(transition, 1.0, "this test's camera must be a FULL globe");

            let background =
                crate::style::Rgba { r: 79.0 / 255.0, g: 116.0 / 255.0, b: 153.0 / 255.0, a: 1.0 };
            let space = crate::render::space::Space {
                atmosphere: crate::render::atmosphere::Atmosphere {
                    sun: crate::render::atmosphere::Sun::VIEWPORT_DEFAULT,
                    ..crate::render::atmosphere::Atmosphere::default()
                },
                ..crate::render::space::Space::default()
            };
            let sun_bearing_deg = match space.atmosphere.sun {
                crate::render::atmosphere::Sun::Viewport { bearing_deg, tilt_deg } => {
                    assert!(
                        tilt_deg > 20.0 && tilt_deg < 160.0,
                        "a tilt of {tilt_deg} puts the terminator off the disc, and the \
                         lit/dark comparison below would be measuring nothing"
                    );
                    f64::from(bearing_deg)
                }
                other => panic!("this test predicts a SCREEN bearing; {other:?} has none"),
            };

            let limb = analytic_disc_radius_px(&camera);
            assert!((limb - 161.385).abs() < 0.01, "the analytic disc moved: {limb}");

            let (buffers, group) = backdrop_draw(&ctx, &pipeline, &camera, extent, background);
            let atmosphere = crate::render::atmosphere::AtmospherePipeline::new(
                &ctx,
                wgpu::TextureFormat::Rgba8Unorm,
                pipeline.depth_format(),
            );
            let frame = crate::render::globe::globe_frame(&camera);
            atmosphere.write(&ctx, &camera, &frame, &space.atmosphere);

            let clear = TilePipeline::clear_color(&crate::render::space::space_clear_color(
                Some(background),
                &space,
                transition,
            ));
            let mut frames = Vec::new();
            for with_atmosphere in [false, true] {
                let draw = SpaceDraw {
                    sky: None,
                    stars: None,
                    backdrop: Some((&buffers, &group)),
                    atmosphere: if with_atmosphere { Some(&atmosphere) } else { None },
                    look: None,
                };
                frames.push(
                    render_space_frame(&ctx, &pipeline, &camera, size, clear, &draw, &[]).await,
                );
            }

            let sky = [0u8, 0, 0];
            let angles: Vec<f64> =
                (0..120).map(|i| f64::from(i) * 3.0_f64.to_radians()).collect();
            // THE CONTROL IS THE DEFECT, and it is used twice: once as the
            // frame that decides where the drawn planet ends along each ray
            // (see `glow_around_the_limb`), and once as its own measurement,
            // which is trivially zero because the mask is "the control reads
            // space here". What is NOT trivial and is asserted instead is
            // that the mask lands where the geometry says it should.
            let measured = glow_around_the_limb(&frames[0], &frames[1], size, limb, sky, &angles);
            let lit: Vec<i32> = measured.iter().map(|(glow, _)| *glow).collect();
            let edges: Vec<f64> = measured.iter().map(|(_, edge)| *edge).collect();
            assert!(
                edges.iter().all(|edge| edge.is_finite()),
                "some ray never reached the space colour at all, so the planet has no \
                 edge along it and there is nothing outside the limb to measure"
            );
            let (nearest, furthest) = (
                edges.iter().copied().fold(f64::INFINITY, f64::min),
                edges.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            );
            assert!(
                nearest > limb - 3.0 && furthest < limb + 1.5,
                "the drawn planet's edge runs {nearest:.2} to {furthest:.2} px from the \
                 centre, against an analytic limb of {limb:.3}. Measured 160.88 to \
                 162.13: the subdivided polygon is inscribed in the true circle and the \
                 sample is quantised to whole pixels, so the per-ray edge straddles the \
                 analytic limb by about a pixel either way"
            );

            // 1. There is a glow. Measured brightest: 142 of 255.
            let brightest = lit.iter().copied().max().expect("120 rays");
            // PRINTED as well as asserted, because three of the four rows
            // of this test's table moved when the path-length factor
            // landed and the next person to change the shader's shape will
            // want the readings rather than the verdict.
            println!(
                "glow at z1.5 512x512: brightest {brightest}, edges {:.2}-{:.2} px against \
                 an analytic {limb:.3}",
                nearest, furthest
            );
            assert!(
                brightest >= 100,
                "the limb must GLOW: the brightest pixel in the 2.53 px shell around the \
                 161.4 px silhouette is only {brightest} byte units above space"
            );

            // 2. The lit side is brighter than the dark side -- the
            //    terminator. Two assertions rather than one comparison,
            //    because the dark side is EXACTLY zero and a ratio against
            //    zero says nothing: the arc facing away must be untouched
            //    space, and the arc facing the sun must be lit. Either one
            //    alone is satisfiable by a limb that is dark all the way
            //    round.
            let toward = |target: f64| {
                angles
                    .iter()
                    .zip(lit.iter())
                    .filter(|(angle, _)| {
                        let delta = (angle.to_degrees() - target).rem_euclid(360.0);
                        delta.min(360.0 - delta) <= 30.0
                    })
                    .map(|(_, value)| f64::from(*value))
                    .sum::<f64>()
            };
            let sunward = toward(sun_bearing_deg);
            let antisunward = toward(sun_bearing_deg + 180.0);
            println!("glow at z1.5 512x512: sunward {sunward}, antisunward {antisunward}");
            assert_eq!(
                antisunward, 0.0,
                "THE TERMINATOR: the 60-degree arc of limb facing AWAY from the sun must \
                 be the space colour EXACTLY -- a sample there has its secondary ray pass \
                 through the planet, the density term explodes, and the attenuation goes \
                 to zero. It summed to {antisunward} over 20 rays instead"
            );
            assert!(
                sunward >= 1_000.0,
                "...and the 21 rays facing the SUN must be lit: they summed to {sunward}, \
                 against a measured 2,310. Without this half, a sun pointing the wrong \
                 way would satisfy the assertion above with a dark limb all the way round"
            );

            // 3. The planet is not washed out: the centre of the disc is
            //    the ocean colour, to within the one byte unit a `u8`
            //    framebuffer cannot subdivide. **RE-GRADED from 16, and
            //    the lower bound that used to sit above this is GONE** --
            //    see this test's doc comment, and
            //    `the_haze_in_front_of_the_planet_is_a_rim_and_not_a_wash`
            //    for the whole disc rather than one pixel of it.
            let ocean = [79u8, 116, 153];
            let centre_control = rgb_at(&frames[0], size, size / 2, size / 2);
            let centre_lit = rgb_at(&frames[1], size, size / 2, size / 2);
            assert_eq!(centre_control, ocean, "the control's disc centre is the ocean");
            let haze = rgb_gap(centre_lit, centre_control);
            assert!(
                haze <= 2,
                "the centre of the disc moved {haze} byte units, from {centre_control:?} \
                 to {centre_lit:?}. This bound was 16 until the path-length factor landed \
                 and it PASSED at 12; a side-by-side against another renderer made that a \
                 defect rather than a tolerance, and the grade is now the plan's own \
                 <= 2. Measured 1"
            );

            // 4. It does not leak. Beyond the top of the shell the frame is
            //    byte-identical to the control, which is where a star field
            //    would be.
            let bytes_per_row = (size * 4).next_multiple_of(256) as usize;
            let outer = limb
                * (crate::render::atmosphere::ATMOSPHERE_RADIUS_M
                    / crate::render::atmosphere::EARTH_RADIUS_M)
                + 1.5;
            let mut leaked = 0usize;
            let mut outside = 0usize;
            for row in 0..size as usize {
                for col in 0..size as usize {
                    let distance = (col as f64 + 0.5 - 256.0).hypot(row as f64 + 0.5 - 256.0);
                    if distance <= outer {
                        continue;
                    }
                    outside += 1;
                    let offset = row * bytes_per_row + col * 4;
                    if frames[0][offset..offset + 4] != frames[1][offset..offset + 4] {
                        leaked += 1;
                    }
                }
            }
            assert!(outside > 100_000, "only {outside} pixels lie outside the shell");
            assert_eq!(
                leaked, 0,
                "{leaked} of {outside} pixels beyond {outer:.2} px changed. A ray that \
                 misses the shell returns opacity 1, so premultiplied blending must \
                 leave them EXACTLY alone -- if it does not, the glow is washing out \
                 whatever is behind it"
            );
        });
    }

    /// **The glow must not wash the OCEAN, and it takes a 2048 px disc to
    /// see that it did.**
    ///
    /// `AtmospherePipeline::new` draws the glow at the far plane and tests
    /// `LessEqual`, so it paints only where nothing nearer was drawn. That
    /// fixed the band over LAND completely. It could not fix it over water,
    /// because the ground writes depth only where a fill was drawn and the
    /// ocean is not a fill -- it is `render::space::backdrop_mesh`, drawn
    /// on `TilePipeline::backdrop`, whose entire purpose is to write no
    /// depth. Over water the buffer still held the cleared 1.0, the glow's
    /// test passed, and the scattering integral composited over the sea.
    ///
    /// `TilePipeline::backdrop_depth` is the fix: the same sphere drawn a
    /// second time after the tiles, depth only, no colour.
    ///
    /// # Why 2048 px, when every other pixel test in this file is 161
    ///
    /// The band is a fraction of the DISC RADIUS, not a pixel count, so it
    /// scales with the window. On the 161 px disc `space_camera` gives, the
    /// 0.97-0.99 annulus this asserts over is **1.6 px wide** and the whole
    /// defect hides inside the rim; at 2048 px the same annulus is 20 px of
    /// visibly pale sea. That is how a band the user could see survived
    /// `the_glow_reads_outside_the_limb_and_does_not_wash_out_the_planet`
    /// (which samples r/limb 0, 0.5, 0.95 and then jumps to 1.005) and
    /// `the_haze_in_front_of_the_planet_is_a_rim_and_not_a_wash` (512 and
    /// 768 px, where the interior mean reads 0.88 and 0.98 -- both already
    /// passing, both blind to this).
    ///
    /// # Measured, on this camera, backdrop only and no tiles
    ///
    /// ```text
    ///   r / limb       before     after
    ///   0.90 - 0.95    mean 1.13  mean 0.00
    ///   0.95 - 0.97    mean 1.54  mean 0.25
    ///   0.97 - 0.99    mean 2.97  mean 0.99
    /// ```
    ///
    /// The "before" column is this test run with the `backdrop_depth` draw
    /// in `render_with_space` disabled and nothing else changed; it FAILS
    /// on the last row, which is what makes this a grade of the fix rather
    /// than of the weather. It is the defect, so this is an acceptance test
    /// whose before really is the bug -- the standard
    /// `the_haze_in_front_of_the_planet_is_a_rim_and_not_a_wash` sets when
    /// it renders a factor-less arm rather than describing one.
    ///
    /// **These are smaller than the figures on
    /// `TilePipeline::backdrop_depth` (6.26 / 13.52 / 29.25), and that is
    /// not a discrepancy.** Those were measured on the real 32-tile frame
    /// with the terminator lit, where the shading darkens the sea the glow
    /// lands on and the same absolute glow is a much larger fraction of
    /// it. The frame here is unlit and backdrop-only -- the quieter
    /// arrangement -- and it is the one this test uses because it needs no
    /// tile bytes and no network.
    ///
    /// The `after` column is not zero, and that is expected rather than
    /// residual defect: the backdrop is a chord polygon inscribed in the
    /// analytic sphere, so a thin annulus just inside the limb has no
    /// backdrop fragment to write depth and the glow legitimately reaches
    /// it. That is the sagitta `capture::CaptureRequest::atmosphere`
    /// describes, and it is why the bar here is "under 2 byte units"
    /// rather than zero. On the real frame, where tiles cover that annulus
    /// too, the same measurement is 0.00 everywhere.
    #[test]
    fn the_glow_does_not_wash_the_ocean() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            // 2048 px, and z3.5 at lat 50 -- the globe camera the band was
            // reported at, not the 512 px one the rest of this file shares.
            let size = 2048u32;
            let camera = crate::render::Camera {
                center_lon: 15.0,
                center_lat: 50.0,
                zoom: 3.5,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: size as f32,
                viewport_height_px: size as f32,
                fov_y: crate::render::Camera::DEFAULT_FOV_Y,
            };
            let extent = 4096u32;
            let transition = crate::render::globe::transition_for_zoom(camera.zoom);
            assert!(transition > 0.9, "this camera must be essentially a globe: {transition}");

            let background =
                crate::style::Rgba { r: 79.0 / 255.0, g: 116.0 / 255.0, b: 153.0 / 255.0, a: 1.0 };
            let space = crate::render::space::Space {
                atmosphere: crate::render::atmosphere::Atmosphere {
                    sun: crate::render::atmosphere::Sun::VIEWPORT_DEFAULT,
                    ..crate::render::atmosphere::Atmosphere::default()
                },
                ..crate::render::space::Space::default()
            };
            let limb = analytic_disc_radius_px(&camera);

            let (buffers, group) = backdrop_draw(&ctx, &pipeline, &camera, extent, background);
            let atmosphere = crate::render::atmosphere::AtmospherePipeline::new(
                &ctx,
                wgpu::TextureFormat::Rgba8Unorm,
                pipeline.depth_format(),
            );
            let frame = crate::render::globe::globe_frame(&camera);
            atmosphere.write(&ctx, &camera, &frame, &space.atmosphere);

            let clear = TilePipeline::clear_color(&crate::render::space::space_clear_color(
                Some(background),
                &space,
                transition,
            ));
            let mut frames = Vec::new();
            for with_atmosphere in [false, true] {
                let draw = SpaceDraw {
                    sky: None,
                    stars: None,
                    backdrop: Some((&buffers, &group)),
                    atmosphere: if with_atmosphere { Some(&atmosphere) } else { None },
                    look: None,
                };
                frames.push(
                    render_space_frame(&ctx, &pipeline, &camera, size, clear, &draw, &[]).await,
                );
            }

            let bytes_per_row = (size * 4).next_multiple_of(256) as usize;
            let centre = f64::from(size) / 2.0;
            // Mean largest-channel change per annulus, over pixels the
            // control actually drew the planet on.
            let band = |lo: f64, hi: f64| -> (f64, i32, usize) {
                let (mut sum, mut worst, mut count) = (0.0f64, 0i32, 0usize);
                for row in 0..size as usize {
                    for col in 0..size as usize {
                        let distance =
                            (col as f64 + 0.5 - centre).hypot(row as f64 + 0.5 - centre);
                        if distance < lo * limb || distance >= hi * limb {
                            continue;
                        }
                        let offset = row * bytes_per_row + col * 4;
                        let (a, b) = (&frames[0][offset..offset + 3], &frames[1][offset..offset + 3]);
                        let gap = (0..3)
                            .map(|i| i32::from(a[i]).abs_diff(i32::from(b[i])) as i32)
                            .max()
                            .unwrap_or(0);
                        sum += f64::from(gap);
                        worst = worst.max(gap);
                        count += 1;
                    }
                }
                (if count == 0 { 0.0 } else { sum / count as f64 }, worst, count)
            };

            for (lo, hi) in [(0.90, 0.95), (0.95, 0.97), (0.97, 0.99)] {
                let (mean, worst, count) = band(lo, hi);
                println!(
                    "ocean wash {size}px r/limb {lo:.2}-{hi:.2}: mean {mean:.2} worst {worst} \
                     over {count} px"
                );
                assert!(count > 10_000, "only {count} px in the {lo}-{hi} annulus");
                assert!(
                    mean <= 2.0,
                    "the glow washed the ocean: the {lo:.2}-{hi:.2} annulus moved a mean \
                     of {mean:.2} byte units (worst {worst}) when the atmosphere pass was \
                     added, over {count} pixels of backdrop. Measured 6.52 / 13.80 / 29.90 \
                     for the three annuli before `TilePipeline::backdrop_depth` put the \
                     ocean's own surface into the depth buffer, and 0.00 after"
                );
            }
        });
    }

    /// The atmosphere's contribution at one point on the disc, per channel:
    /// `lit - control`, signed, plus the largest-channel gap.
    ///
    /// Signed and per-channel because the wash Task 10 measured was not
    /// only a dimming -- (79,116,153) became (75,120,165), red DOWN 4 and
    /// blue UP 12 -- and a largest-channel figure hides the sign that makes
    /// that a desaturation rather than a haze.
    ///
    /// **FLOOR, not round**, for the reason `glow_around_the_limb`'s doc
    /// comment gives at length: pixel `i` covers `[i, i+1)`.
    fn haze_at(
        control: &[u8],
        lit: &[u8],
        size: u32,
        radius: f64,
        bearing_rad: f64,
    ) -> ([i32; 3], i32) {
        let centre = f64::from(size) / 2.0;
        // Screen bearing, clockwise from straight up -- `Sun::Viewport`'s
        // own convention, shared with `glow_around_the_limb`.
        let col = (centre + radius * bearing_rad.sin()).floor() as u32;
        let row = (centre - radius * bearing_rad.cos()).floor() as u32;
        let (c, l) = (rgb_at(control, size, col, row), rgb_at(lit, size, col, row));
        let per_channel = [0, 1, 2].map(|i| i32::from(l[i]) - i32::from(c[i]));
        (per_channel, rgb_gap(l, c))
    }

    /// Every pixel of the disc INTERIOR -- inside 0.95 of the analytic limb,
    /// clear of the 1.57% shell the rim lives in -- summarised against the
    /// control: how many changed at all, the mean largest-channel gap over
    /// all of them, and the worst single one.
    ///
    /// 0.95 is Task 10's own cut and it is kept so the figures stay
    /// comparable: at z1.5 it encloses 73,832 pixels and at 768x768 z3
    /// 407,076, both of which that task's table quotes.
    fn interior_haze(
        control: &[u8],
        lit: &[u8],
        size: u32,
        limb_px: f64,
    ) -> (usize, usize, f64, i32) {
        let centre = f64::from(size) / 2.0;
        let cut = limb_px * 0.95;
        let (mut total, mut changed, mut sum, mut worst) = (0usize, 0usize, 0i64, 0i32);
        for row in 0..size {
            for col in 0..size {
                let distance =
                    (f64::from(col) + 0.5 - centre).hypot(f64::from(row) + 0.5 - centre);
                if distance > cut {
                    continue;
                }
                total += 1;
                let gap = rgb_gap(rgb_at(lit, size, col, row), rgb_at(control, size, col, row));
                if gap > 0 {
                    changed += 1;
                }
                sum += i64::from(gap);
                worst = worst.max(gap);
            }
        }
        (changed, total, sum as f64 / total as f64, worst)
    }

    /// `atmosphere_shader.wgsl` with **both** of the things that keep the
    /// glow off the planet edited out: the path-length factor in `fs_main`,
    /// and the far-plane triangle position in `vs_main`. The Task 9
    /// atmosphere exactly as it shipped, which is the wash Task 10 measured
    /// and the defect a side-by-side showed.
    ///
    /// # Why it takes TWO edits now, when one used to be enough
    ///
    /// There are two independent guards in front of the disc, added by
    /// different tasks for different reasons:
    ///
    ///  - `* path_fraction` DIMS the glow by how much of the ray's path lay
    ///    in air rather than in planet; and
    ///  - the triangle at NDC `z = 1.0` plus the pipeline's `LessEqual`
    ///    REJECTS it outright wherever something nearer was drawn -- which,
    ///    since `TilePipeline::backdrop_depth`, includes the ocean.
    ///
    /// Removing only the factor no longer reproduces the defect, because
    /// the depth test still rejects every fragment over the backdrop and
    /// the arm comes back clean -- an acceptance test grading the fix
    /// against itself, which is the exact failure the assertions below
    /// exist to prevent. Putting the triangle back at `z = 0` (where
    /// `LessEqual` against any real depth passes) restores the historical
    /// behaviour, so this arm is once again the defective frame rather than a frame with one of two fixes still in it.
    ///
    /// Both edits assert they matched, for the reason
    /// `shader_without_ground_shading` asserts the same of its gate: a
    /// `replace` that matched nothing would silently hand the caller a
    /// second copy of the shipped shader, and an acceptance test would then
    /// be grading it against itself and passing.
    fn atmosphere_shader_without_the_path_factor() -> String {
        let source = include_str!("atmosphere_shader.wgsl");
        const FACTOR: &str = " * path_fraction";
        assert_eq!(
            source.matches(FACTOR).count(),
            1,
            "atmosphere_shader.wgsl no longer applies exactly one factor spelled \
             `{FACTOR}` -- so the arm this builds is the shipped shader, and \
             `the_haze_in_front_of_the_planet_is_a_rim_and_not_a_wash` is grading the \
             fix against itself"
        );
        // The far-plane position `AtmospherePipeline::new`'s `LessEqual`
        // reads. See this function's doc comment for why removing the
        // factor alone stopped being the defect.
        const FAR_PLANE: &str = "vec4<f32>(ndc, 1.0, 1.0)";
        const NEAR_PLANE: &str = "vec4<f32>(ndc, 0.0, 1.0)";
        assert_eq!(
            source.matches(FAR_PLANE).count(),
            1,
            "atmosphere_shader.wgsl no longer puts its triangle at the far plane with \
             exactly one `{FAR_PLANE}` -- so this arm still has the depth guard in it \
             and is not the defect"
        );
        source.replace(FACTOR, "").replace(FAR_PLANE, NEAR_PLANE)
    }

    /// **THE ACCEPTANCE CRITERION FOR THE RIM: the air in FRONT of the
    /// planet is gone, and the air at the limb is not.**
    ///
    /// This test exists because Task 10 measured the opposite and left it
    /// standing. Its radial profile of the shipped Task 9 atmosphere, taken
    /// against a control with the pass withheld, ocean `#4f7499`,
    /// `Sun::VIEWPORT_DEFAULT`, largest channel:
    ///
    /// ```text
    ///                  512x512 z1.5 (limb 161.4)   768x768 z3 (limb 378.9)
    ///   r/limb    toward sun   away   perp      toward sun   away   perp
    ///   0.00         d12       d12    d12          d12       d12    d12
    ///   0.50         d 9       d18    d13          d 7       d22    d13
    ///   0.95         d 8       d 0    d19          d 8       d 0    d17
    ///   1.005 rim   d128       d 0   d142          d79       d 0    d93
    ///   interior mean gap      11.99 (95.5% of 73,832 px changed)
    ///                          13.63 (99.0% of 407,076 px changed)
    /// ```
    ///
    /// **Flat, not peaked** -- toward the sun the wash at 0.95 of the limb
    /// is no weaker than at the disc centre -- and it **desaturated as well
    /// as dimmed**: (79,116,153) to (75,120,165), red -4, green +4, blue
    /// +12, so every style colour on the disc was pulled toward one blue.
    /// Side by side with another renderer's globe, ours looked shaded, and
    /// the glow and the colours overlapped unnaturally.
    ///
    /// The fix is `atmosphere_shader.wgsl`'s `path_fraction`, argued at
    /// length in `fs_main`: the ray's own path through the shell, over the
    /// longest path there is. Measured here, same cameras, same control,
    /// same probes -- these are the numbers this test prints:
    ///
    /// ```text
    ///                  512x512 z1.5               768x768 z3
    ///   r/limb    toward sun   away   perp    toward sun   away   perp
    ///   0.00         d 1       d 1    d 1        d 1       d 1    d 1
    ///   0.50         d 0       d 1    d 1        d 0       d 1    d 1
    ///   0.95         d 1       d 0    d 3        d 1       d 0    d 3
    ///   1.005 rim   d115       d 0   d128        d67       d 0    d79
    ///   interior mean gap       0.88 (81.5% of 73,832 px changed, worst 3)
    ///                           0.98 (80.1% of 407,076 px changed, worst 3)
    ///   disc centre        (79,116,153) -> (79,116,154): (0, 0, +1)
    /// ```
    ///
    /// **The rim keeps 90% of itself at z1.5 (142 -> 128, and 128 -> 115
    /// toward the sun) and 85% at z3 (93 -> 79, 79 -> 67)**, against an
    /// interior that went from a mean of 11.99/13.63 to 0.88/0.98. The
    /// profile is now PEAKED: at z3, 1 at the centre, 0 at half a radius,
    /// 1-3 at 0.95, and 79 on the rim -- a peak of 81x over the interior
    /// mean, where Task 10 measured 6.8x.
    ///
    /// The 80% of interior pixels that still "changed" changed by ONE byte
    /// unit of blue, which is the smallest move a `u8` framebuffer can
    /// represent; the count is reported rather than asserted for exactly
    /// that reason, and what is asserted is the mean and the per-channel
    /// move at the centre.
    ///
    /// # What is asserted, and why those three and not the table
    ///
    ///  - **the interior mean gap is at most 2 byte units**, at BOTH
    ///    cameras. This is the plan's own grade for the fix and it replaces
    ///    a bound of 12 that
    ///    `the_glow_reads_outside_the_limb_and_does_not_wash_out_the_planet`
    ///    used to assert as desirable;
    ///  - **the disc centre moves at most 1 unit on EVERY channel
    ///    separately**, which is the desaturation claim. A largest-channel
    ///    bound alone would pass a frame that moved red -2 and blue +2;
    ///  - **the rim still reads at least 60 units above the ocean**, which
    ///    is the floor without which the two bounds above are satisfied by
    ///    deleting the pass.
    ///
    /// Those three together are the "peaked, not flat" claim said in
    /// numbers. The test PRINTS the whole profile at both cameras, so the
    /// table above can be re-read rather than trusted.
    ///
    /// # The OTHER candidate, measured rather than argued away
    ///
    /// Task 10 named two possible fixes and this is the first. The second
    /// was "leave the alpha channel linear while exposing only rgb" — one
    /// step further along the axis Task 9 tabulated when it took the 1/2.2
    /// gamma off rgb and then off alpha (rim/centre 1.8x, 4.3x, 10.7x).
    /// Four readings of it were built as shader variants and rendered at
    /// the same two cameras against the same control; at 768x768 z3, disc
    /// interior mean / disc-centre per channel / rim:
    ///
    /// ```text
    ///   shipped Task 9 (neither)      13.63   (-4, +4, +12)   d79 d0 d93
    ///   alpha ^ 2.2                   47.02   (+11,+26, +41)  d79 d0 d93
    ///   alpha ^ 4.4                   58.02   (+14,+30, +47)  d79 d0 d93
    ///   alpha linear in optical depth 11.90   ( -6, +1,  +8)  d79 d0 d93
    ///   gamma 2.2 on rgb AND alpha     5.37   ( -3, -2,   0)  d19 d0 d28
    ///   THE PATH-LENGTH FACTOR          0.98  (  0,  0,  +1)  d67 d0 d79
    /// ```
    ///
    /// **Every alpha-only reading is worse or barely better, and the
    /// reason is the blend equation.** The composite is
    /// `dst * (1 - A) + S`, so suppressing `A` alone REMOVES the only term
    /// that was darkening the disc and leaves the emitted light `S` — about
    /// 38 byte units of blue at the disc centre — landing on an
    /// undiminished background. The two `pow` rows make the wash three to
    /// four times worse and turn it from bluing into brightening. The one
    /// row that helps, gamma on rgb as well as alpha, buys a 2.5x
    /// improvement by destroying **70% of the rim** (d93 to d28), which is
    /// the trade Task 9 already rejected under a different name: it is a
    /// volume knob.
    ///
    /// The path-length factor is the only one of the six that touches the
    /// disc without touching the rim, because it is the only one that is a
    /// function of WHERE THE RAY WENT rather than of how bright it came
    /// out.
    ///
    /// # The FACTOR-LESS arm is RENDERED, not described
    ///
    /// Three arms, not two: the control with no atmosphere at all, the
    /// shipped shader, and the shipped shader with ` * path_fraction`
    /// edited out of `fs_main` -- see
    /// `atmosphere_shader_without_the_path_factor`, which refuses to build
    /// an arm identical to the shipped one. The third is asserted to BE the
    /// defect, a mean of at least 10 byte units, for the reason
    /// `the_ground_shading_is_what_darkens_the_night_side` gives: "an
    /// acceptance test whose 'before' is not the defect grades nothing".
    /// It reads 11.99 and 13.63, which are Task 10's own two figures, and
    /// it makes the sabotage check permanent rather than a line in a commit
    /// message.
    ///
    /// # Why these two cameras
    ///
    /// z1.5 512x512 is the shared `space_camera` every other globe pixel
    /// test in this file uses, so its 161.385 px silhouette is already
    /// derived and checked elsewhere. **z3 is the worse case AND is the
    /// default example's globe camera**, which is what makes it the one the
    /// user was looking at. Both are square because `render_space_frame`
    /// reads back a square texture; the example window is 1024x768, and the
    /// haze is a function of `r / limb` rather than of the viewport, so
    /// squaring it changes which pixels are sampled and not what they say.
    #[test]
    fn the_haze_in_front_of_the_planet_is_a_rim_and_not_a_wash() {
        pollster::block_on(async {
            const ATMOSPHERE_SOURCE: &str = include_str!("atmosphere_shader.wgsl");
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let extent = 4096u32;
            let background =
                crate::style::Rgba { r: 79.0 / 255.0, g: 116.0 / 255.0, b: 153.0 / 255.0, a: 1.0 };
            let ocean = [79u8, 116, 153];
            // NAMED, not `Atmosphere::default()`, for the same reason
            // `the_glow_reads_outside_the_limb_and_does_not_wash_out_the_planet`
            // names it: the default is now the wall clock, and this test
            // samples three fixed SCREEN bearings.
            let space = crate::render::space::Space {
                atmosphere: crate::render::atmosphere::Atmosphere {
                    sun: crate::render::atmosphere::Sun::VIEWPORT_DEFAULT,
                    ..crate::render::atmosphere::Atmosphere::default()
                },
                ..crate::render::space::Space::default()
            };
            let sun_bearing_deg = match space.atmosphere.sun {
                crate::render::atmosphere::Sun::Viewport { bearing_deg, .. } => {
                    f64::from(bearing_deg)
                }
                other => panic!("this test samples SCREEN bearings; {other:?} has none"),
            };
            let no_factor = atmosphere_shader_without_the_path_factor();

            for (size, zoom, expected_limb) in [(512u32, 1.5_f64, 161.385_f64), (768, 3.0, 378.9)]
            {
                let camera = crate::render::Camera {
                    center_lon: 0.0,
                    center_lat: 0.0,
                    zoom,
                    pitch: 0.0,
                    bearing: 0.0,
                    viewport_width_px: size as f32,
                    viewport_height_px: size as f32,
                    fov_y: crate::render::Camera::DEFAULT_FOV_Y,
                };
                let transition = crate::render::globe::transition_for_zoom(camera.zoom);
                assert_eq!(transition, 1.0, "z{zoom} must be a FULL globe for this test");
                let limb = analytic_disc_radius_px(&camera);
                assert!(
                    (limb - expected_limb).abs() < 0.1,
                    "the analytic disc at z{zoom} in {size}x{size} is {limb:.3} px, not the \
                     {expected_limb} this test's table is indexed by"
                );

                let (buffers, group) = backdrop_draw(&ctx, &pipeline, &camera, extent, background);
                let clear = TilePipeline::clear_color(&crate::render::space::space_clear_color(
                    Some(background),
                    &space,
                    transition,
                ));
                let frame = crate::render::globe::globe_frame(&camera);

                let mut frames = Vec::new();
                for source in [None, Some(ATMOSPHERE_SOURCE), Some(no_factor.as_str())] {
                    let atmosphere = source.map(|source| {
                        let air = crate::render::atmosphere::AtmospherePipeline::from_source(
                            &ctx,
                            wgpu::TextureFormat::Rgba8Unorm,
                            pipeline.depth_format(),
                            source,
                        );
                        air.write(&ctx, &camera, &frame, &space.atmosphere);
                        air
                    });
                    let draw = SpaceDraw {
                        sky: None,
                        stars: None,
                        backdrop: Some((&buffers, &group)),
                        atmosphere: atmosphere.as_ref(), look: None };
                    frames.push(
                        render_space_frame(&ctx, &pipeline, &camera, size, clear, &draw, &[]).await,
                    );
                }
                let control = &frames[0];
                assert_eq!(
                    rgb_at(control, size, size / 2, size / 2),
                    ocean,
                    "the control's disc centre must be the ocean colour, or every \
                     difference below is measured against the wrong thing"
                );

                let bearings = [
                    ("toward sun", sun_bearing_deg),
                    ("away", sun_bearing_deg + 180.0),
                    ("perp", sun_bearing_deg + 90.0),
                ];
                for (label, index) in [("shipped  ", 1usize), ("no factor", 2usize)] {
                    let lit = &frames[index];
                    // `r / limb` of 1.005 is inside the 1.0157 shell and
                    // just outside the drawn planet: the rim's own row.
                    for fraction in [0.0, 0.5, 0.95, 1.005] {
                        let row: Vec<String> = bearings
                            .iter()
                            .map(|(_, bearing)| {
                                let (channels, gap) = haze_at(
                                    control,
                                    lit,
                                    size,
                                    limb * fraction,
                                    bearing.to_radians(),
                                );
                                format!("d{gap:<3} {channels:?}")
                            })
                            .collect();
                        println!(
                            "haze z{zoom} {size}x{size} {label} r/limb {fraction:<5}: {}",
                            row.join("  ")
                        );
                    }
                    let (changed, total, mean, worst) = interior_haze(control, lit, size, limb);
                    println!(
                        "haze z{zoom} {size}x{size} {label} interior: {changed} of {total} px \
                         changed ({:.1}%), mean gap {mean:.2}, worst {worst}",
                        changed as f64 * 100.0 / total as f64
                    );
                }

                let lit = &frames[1];
                let (_, total, mean, worst) = interior_haze(control, lit, size, limb);

                // 0. THE DEFECT IS RENDERED, and it is the same frame with
                //    one factor taken out of one line. Without this arm
                //    every bound below is satisfied by an atmosphere that
                //    was never washing anything out.
                let (_, _, defect_mean, defect_worst) =
                    interior_haze(control, &frames[2], size, limb);
                let (defect_centre, _) = haze_at(control, &frames[2], size, 0.0, 0.0);
                assert!(
                    defect_mean >= 10.0,
                    "the arm with ` * path_fraction` removed moves the disc interior by a \
                     mean of only {defect_mean:.2} byte units (worst {defect_worst}, \
                     centre {defect_centre:?}) at z{zoom}. Task 10 measured 11.99 at z1.5 \
                     and 13.63 at z3, so this arm is not the defect and the bounds below \
                     are grading the fix against itself"
                );

                // 1. THE WASH IS GONE. Task 10's grade for a fix, verbatim:
                //    "disc-interior mean gap from 12-14 down to <= 2 byte
                //    units".
                assert!(
                    mean <= 2.0,
                    "at z{zoom} the atmosphere moves the {total} px of the disc interior by \
                     a mean of {mean:.2} byte units (worst {worst}). Task 10 measured 11.99 \
                     at z1.5 and 13.63 at z3 and graded a fix at <= 2: that is haze in \
                     FRONT of the planet, and it is what makes a styled basemap read as \
                     shaded"
                );

                // 2. AND IT IS GONE ON EVERY CHANNEL. The disc centre moved
                //    (-4, +4, +12) before the fix -- a desaturation, not a
                //    dimming -- and a largest-channel bound would pass a
                //    frame that moved red -2 and blue +2.
                let (centre_channels, centre_gap) = haze_at(control, lit, size, 0.0, 0.0);
                assert!(
                    centre_channels.iter().all(|delta| delta.abs() <= 1),
                    "the disc centre moves {centre_channels:?} per channel (largest \
                     {centre_gap}), against (-4, +4, +12) before the fix. Every channel \
                     has to collapse, not just the largest: a style's own colour is the \
                     RATIO between them"
                );

                // 3. THE RIM SURVIVES, and this is the floor without which
                //    1 and 2 are satisfied by deleting the pass.
                let rim = bearings
                    .iter()
                    .map(|(_, bearing)| {
                        haze_at(control, lit, size, limb * 1.005, bearing.to_radians()).1
                    })
                    .max()
                    .expect("three bearings");
                assert!(
                    rim >= 60,
                    "the brightest of the three rim probes at 1.005 x the limb reads only \
                     {rim} byte units above the ocean at z{zoom}. Task 10 measured 142 at \
                     z1.5 and 93 at z3, and the fix is a SHAPE change: an interior mean of \
                     {mean:.2} with no rim left is not a rim, it is a deleted pass"
                );
            }
        });
    }

    /// **THE COST OF A FULL-SCREEN SCATTERING PASS, measured, at the worst
    /// camera in the globe band.**
    ///
    /// This is the one piece of the globe plan that could plausibly break
    /// its standing **0.5 ms** rule, and it is the only piece whose cost is
    /// on the GPU rather than in `paint`: five primary and three secondary
    /// ray steps, each with two `exp`s, over every pixel of the viewport.
    /// A `PaintPhase` number cannot see it — `paint` ends at `queue.submit`
    /// — so this is an in-process A/B of the whole submit-and-wait: the
    /// SAME frame, the same tiles and the same backdrop, drawn with the
    /// atmosphere passed and with it withheld, `device.poll(Wait)` after
    /// each so the measurement includes the GPU and not just the encoder.
    ///
    /// **1024x768**, the default example window
    /// (`examples/common/window.rs`), at two cameras:
    ///
    ///  - **z3**, where `transition_for_zoom` is exactly 1.0 and the whole
    ///    disc with its limb is on screen;
    ///  - **z5.8**, the deepest camera the band reaches before the
    ///    transition runs out at z6, where the planet subtends 61.6 degrees
    ///    against a 30-degree half field of view and so covers the viewport
    ///    completely.
    ///
    /// Measured in RELEASE on an Apple M-series adapter, median of 5 batches
    /// of 40 frames each way, repeated four times:
    ///
    /// ```text
    ///   camera     without      with      delta (median)   over 5 runs
    ///   z3        0.0497 ms  0.1582 ms       0.1050 ms    0.0979-0.1072
    ///   z5.8      0.0377 ms  0.2083 ms       0.1704 ms    0.1650-0.1718
    /// ```
    ///
    /// The first four columns are one representative run of the test; the
    /// last is the range of its `delta (median)` across five separate runs
    /// of it, because one run is not a measurement. A sixth run, the very
    /// first after a build, read 0.1538 ms at z3 — the warm-up above covers
    /// twenty frames and evidently not the first shader compile.
    ///
    /// **0.170 ms at the worse of the two, about 3x under the rule**, and
    /// z5.8 is the worse one for a reason the fragment count predicts. A ray
    /// that misses the atmosphere shell leaves `scatter` at its first `if`,
    /// before the march; a ray that hits it runs all 5 x 3 steps whether it
    /// stops at the planet's surface after 100 km or crosses the whole
    /// 2,262 km chord. So the cost goes with the fragments that HIT, and
    /// those are counted here rather than estimated: at z3 the shell's
    /// silhouette is **386.8 px** in a 1024x768 viewport and **59.7%** of
    /// the fragments are inside it; at z5.8 it is 1,270.3 px and **100%**
    /// are. **Predicted ratio 1/0.597 = 1.68x; measured 1.62-1.67x over
    /// four runs.**
    ///
    /// The first run of this test measured a delta of **0.4261 ms** at z3
    /// with a spread of 0.2327-0.5433 — four times the figure above and
    /// through the ceiling at the top of its spread — because it called
    /// `poll(Wait)` after EVERY frame. That measures the round-trip latency
    /// of a submit as much as the work in it, and a frame budget is a
    /// throughput question: how much longer each frame takes when the queue
    /// is full of them. One wait per batch answers that one. The discarded
    /// figure is recorded because it is the number a careless version of
    /// this test would have reported, and it would have failed the plan.
    ///
    /// One run is not a measurement, so this reports a median of five and
    /// asserts on it; and the assertion is the plan's own 0.5 ms ceiling
    /// rather than the measured figure, because a slower machine running
    /// this suite should report a real number rather than fail a threshold
    /// fitted to this one.
    ///
    /// # RE-MEASURED after the path-length factor, and it is free
    ///
    /// `atmosphere_shader.wgsl`'s `path_fraction` adds one divide, one
    /// `clamp` and one multiply per fragment to a 5x3-step march. Three
    /// release runs of this test with it, `delta (median)` per run:
    ///
    /// ```text
    ///   camera    run 1      run 2      run 3     Task 9's five runs
    ///   z3       0.1007 ms  0.1059 ms  0.1005 ms   0.0979-0.1072
    ///   z5.8     0.1834 ms  0.1727 ms  0.1687 ms   0.1650-0.1718
    /// ```
    ///
    /// **z3 is inside Task 9's own range; z5.8 straddles its top edge.**
    /// The three z5.8 medians average 0.175 ms against 0.170, which is
    /// 3% -- smaller than the 4% spread Task 9 saw across its own five
    /// runs, and measured on a machine that had three other cargo builds
    /// on it. What is claimed is therefore a BOUND and not a delta: the
    /// pass still costs under 0.19 ms at the worse camera, about 2.7x
    /// under the plan's rule, and the factor is not resolvable by this
    /// harness. The same finding Task 10 recorded for the ground shading,
    /// and for the same reason.
    #[test]
    #[cfg_attr(not(target_os = "macos"), ignore = "the budget is for a hardware GPU; Linux CI renders in software (lavapipe)")]
    fn the_full_screen_atmosphere_costs_less_than_the_plans_half_millisecond() {
        // A budget on a shared virtual GPU measures the neighbours too.
        if std::env::var("GPUI_MAP_SKIP_TIMING").is_ok_and(|v| v == "1") {
            eprintln!("skipped: GPUI_MAP_SKIP_TIMING=1 (a GPU time budget)");
            return;
        }
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let (width, height) = (1024u32, 768u32);
            let extent = 4096u32;
            let atmosphere = crate::render::atmosphere::AtmospherePipeline::new(
                &ctx,
                wgpu::TextureFormat::Rgba8Unorm,
                pipeline.depth_format(),
            );
            let color_texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("atmosphere-cost-color"),
                size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            });
            let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor::default());
            let depth_view = pipeline.create_depth_view(&ctx, width, height);

            for zoom in [3.0_f64, 5.8] {
                let camera = crate::render::Camera {
                    center_lon: 0.0,
                    center_lat: 0.0,
                    zoom,
                    pitch: 0.0,
                    bearing: 0.0,
                    viewport_width_px: width as f32,
                    viewport_height_px: height as f32,
                    fov_y: crate::render::Camera::DEFAULT_FOV_Y,
                };
                let transition = crate::render::globe::transition_for_zoom(camera.zoom);
                assert!(
                    transition > 0.0,
                    "z{zoom} is outside the globe band, so there would be no pass to \
                     measure"
                );
                let frame = crate::render::globe::globe_frame(&camera);
                let space = crate::render::space::Space::default();
                atmosphere.write(&ctx, &camera, &frame, &space.atmosphere);
                let background = crate::style::Rgba {
                    r: 79.0 / 255.0,
                    g: 116.0 / 255.0,
                    b: 153.0 / 255.0,
                    a: 1.0,
                };
                // Built but NOT handed to `SpaceDraw` below -- see the
                // `backdrop: None` comment there. Kept so the cost of
                // building it is still paid on the same path the shipped
                // frame pays it on, and so this stays one edit away from
                // timing the occluded case should anyone want that number.
                let (_buffers, _group) =
                    backdrop_draw(&ctx, &pipeline, &camera, extent, background);
                let camera_bind_group = pipeline.test_camera_bind_group(
                    &ctx,
                    camera.view_projection().to_cols_array_2d(),
                    camera.zoom as f32,
                );
                let clear = TilePipeline::clear_color(&space.color);

                // How much of the viewport actually MARCHES, computed from
                // the geometry and counted pixel by pixel rather than from
                // pi r squared -- at z3 the shell overruns the top and
                // bottom edges by half a pixel and at z5.8 it covers
                // everything, so a closed form would need two cases and a
                // clipped-circle area. This is the number the two cameras'
                // timings are predicted to be in the ratio of.
                let radius_world = crate::render::globe::globe_radius_world(&camera);
                let shell = crate::render::atmosphere::ATMOSPHERE_RADIUS_M
                    / crate::render::atmosphere::EARTH_RADIUS_M;
                let half_angle = (radius_world * shell
                    / (camera.distance() + radius_world))
                    .min(1.0)
                    .asin();
                let shell_px = f64::from(height) / 2.0 * half_angle.tan()
                    / (crate::render::camera::FOV_Y_RADIANS_F64 / 2.0).tan();
                let marching = (0..height)
                    .flat_map(|row| (0..width).map(move |col| (col, row)))
                    .filter(|(col, row)| {
                        (f64::from(*col) + 0.5 - f64::from(width) / 2.0)
                            .hypot(f64::from(*row) + 0.5 - f64::from(height) / 2.0)
                            <= shell_px
                    })
                    .count();
                let fraction = marching as f64 / f64::from(width * height);

                // Pinned, because it is the explanation as well as a
                // diagnostic: if this moves, the timing ratio below stops
                // being predicted by anything.
                let expected = if zoom < 4.0 { 0.597 } else { 1.0 };
                assert!(
                    (fraction - expected).abs() < 0.005,
                    "at z{zoom} {:.1}% of a {width}x{height} viewport lies inside the \
                     {shell_px:.1} px shell, not the {:.1}% this test's doc predicts the \
                     two cameras' costs from",
                    fraction * 100.0,
                    expected * 100.0
                );

                let before = crate::render::allocation_stats().draw_calls_total;
                let one_batch = |with: bool, frames: u32| {
                    // **NO BACKDROP, and that is what makes this a cost
                    // ceiling rather than a cost sample.**
                    //
                    // With one, the two arms stop differing by "the
                    // atmosphere pass" alone: `TilePipeline::backdrop_depth`
                    // rides along in the `with` arm, puts the planet in the
                    // depth buffer, and early-z then rejects the march over
                    // the whole disc. The pass gets so cheap that the delta
                    // falls into the noise -- measured -0.0305 ms median,
                    // spread -0.0859 to +0.1050 -- and the floor below can
                    // no longer tell "drawn and nearly free" from "not
                    // drawn at all", which is the one thing it exists to
                    // tell.
                    //
                    // Withholding the backdrop leaves the depth buffer at
                    // its cleared 1.0, so every fragment passes the glow's
                    // `LessEqual` and the full 5x3-step march runs over the
                    // whole viewport. That is the MOST the pass can ever
                    // cost, which is the right thing for a ceiling to
                    // grade, and it restores a delta the floor can see.
                    let draw = SpaceDraw {
                        sky: None,
                        stars: None,
                        backdrop: None,
                        atmosphere: if with { Some(&atmosphere) } else { None },
                        look: None,
                    };
                    let start = crate::timing::Instant::now();
                    for _ in 0..frames {
                        let mut encoder = ctx
                            .device
                            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
                        pipeline.render_with_space(
                            &mut encoder,
                            &color_view,
                            &depth_view,
                            clear,
                            &camera_bind_group,
                            &draw,
                            &[],
                            None,
                        );
                        ctx.queue.submit(Some(encoder.finish()));
                    }
                    // ONE wait, at the end of the batch, rather than one per
                    // frame. See this test's doc comment: waiting per frame
                    // measured 0.4261 ms at z3 against this arrangement's
                    // 0.1030 ms, because it timed the round trip of a submit
                    // as well as the work inside it.
                    let _ = ctx.device.poll(wgpu::PollType::wait_indefinitely());
                    start.elapsed().as_secs_f64() * 1000.0 / f64::from(frames)
                };

                // Warm-up: the first submit compiles and uploads, and the
                // first `poll(Wait)` pays for a pipeline state object.
                one_batch(false, 10);
                one_batch(true, 10);

                let mut deltas = Vec::new();
                let (mut without_sum, mut with_sum) = (0.0, 0.0);
                for _ in 0..5 {
                    let without = one_batch(false, 40);
                    let with = one_batch(true, 40);
                    without_sum += without;
                    with_sum += with;
                    deltas.push(with - without);
                }
                deltas.sort_by(f64::total_cmp);
                let median = deltas[deltas.len() / 2];

                // GATE ON PROOF OF PAINT, the plan's standing rule: a frame
                // that issued no draw call took no GPU time and its
                // "measurement" is of an empty queue.
                let issued = crate::render::allocation_stats().draw_calls_total - before;
                assert!(
                    issued > 0,
                    "no draw call was issued at z{zoom}, so there is nothing to have timed"
                );

                println!(
                    "atmosphere cost at z{zoom}: without {:.4} ms, with {:.4} ms, delta \
                     median {median:.4} ms (spread {:.4}-{:.4}), {issued} draw calls, \
                     shell radius {shell_px:.1} px, {:.1}% of fragments march",
                    without_sum / 5.0,
                    with_sum / 5.0,
                    deltas[0],
                    deltas[deltas.len() - 1],
                    fraction * 100.0,
                );
                // ...AND THE MEASUREMENT MUST HAVE MEASURED SOMETHING. A
                // full-screen pass that costs literally nothing did not run:
                // deleting the draw from `render_with_space` leaves this
                // test's two arms identical, and without this floor it
                // passes the ceiling below with a delta of ~0. (The pixel
                // test catches that too, but a cost test that cannot notice
                // its subject being deleted grades nothing.) 0.005 ms is two
                // orders of magnitude under the smallest real reading and
                // still far above a same-arm difference.
                assert!(
                    median > 0.005,
                    "the atmosphere arm cost {median:.4} ms more than the arm without \
                     it, which is not a measurement of a 5x3-step march over \
                     {marching} fragments -- the pass is not being drawn"
                );
                assert!(
                    median < 0.5,
                    "THE PLAN'S 0.5 ms RULE: the full-screen atmosphere pass costs \
                     {median:.4} ms a frame at z{zoom} in a {width}x{height} viewport, \
                     over a median of five batches of 40 frames ({:.4} to {:.4}). Stop \
                     and report rather than shipping it",
                    deltas[0],
                    deltas[deltas.len() - 1],
                );
            }
        });
    }

    /// One globe-band frame the way the shells build it -- through
    /// `frame_plan::space_plan`, with the style's default `sky` -- over an
    /// ocean sphere of `ocean`, and the pixel at the top centre of the view.
    async fn sky_probe(
        ctx: &GpuContext,
        pipeline: &TilePipeline,
        sky: &crate::render::sky::SkyPipeline,
        zoom: f64,
        sun: crate::render::atmosphere::Sun,
        background: crate::style::Rgba,
        ocean: crate::style::Rgba,
    ) -> ([u8; 3], crate::render::frame_plan::SpacePlan) {
        let size = 256u32;
        let camera = crate::render::Camera {
            center_lon: 38.0,
            center_lat: 5.0,
            zoom,
            pitch: std::f32::consts::FRAC_PI_3,
            bearing: 0.0,
            viewport_width_px: size as f32,
            viewport_height_px: size as f32,
            fov_y: crate::render::Camera::DEFAULT_FOV_Y,
        };
        let frame = crate::render::globe::globe_frame(&camera);
        let space = crate::render::space::Space::default();
        let atmosphere = crate::render::atmosphere::Atmosphere {
            sun,
            // The terminator ON, named: the default is off since 2026-09-29.
            night_brightness: crate::render::atmosphere::Atmosphere::TERMINATOR_NIGHT_BRIGHTNESS,
            ..Default::default()
        };
        let plan = crate::render::frame_plan::space_plan(
            Some(background),
            &space,
            &atmosphere,
            &crate::style::SkyFrame::default(),
            frame.space_transition,
            crate::render::sky::sees_above_horizon(&camera, &frame),
        );
        if plan.draws_sky {
            sky.write(ctx, &camera, &frame, &plan.sky, &atmosphere, plan.ground);
        }
        let (backdrop_buffers, backdrop_group) = backdrop_draw(ctx, pipeline, &camera, 4096, ocean);
        let space_draw = SpaceDraw {
            sky: if plan.draws_sky { Some(sky) } else { None },
            backdrop: Some((&backdrop_buffers, &backdrop_group)),
            ..SpaceDraw::NONE
        };
        let clear = TilePipeline::clear_color(&plan.clear_color);
        let pixels = render_space_frame(ctx, pipeline, &camera, size, clear, &space_draw, &[]).await;
        (rgb_at(&pixels, size, size / 2, 2), plan)
    }

    /// **THE GREY SKY, AS A PIXEL TEST.** Tilted to 60 degrees at z5 over
    /// East Africa -- the user's iPhone view -- the top of the screen is
    /// above the planet's limb, and it used to be the clear colour
    /// `lerp(style background, space, transition)`: flat land-grey.
    ///
    /// Now it is the style's `sky` (spec defaults: `#88C6FC` over a white
    /// horizon), blended toward space by the transition; at z3 the sky has
    /// faded out entirely and the pixel is the space colour; and with the
    /// sun on the far side of the planet the same pixel is dark, not blue.
    ///
    /// The ocean sphere is GREEN so the probe cannot mistake it for sky.
    #[test]
    fn the_sky_above_the_limb_is_blue_at_z5_fades_to_space_at_z3_and_is_dark_at_night() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let sky = crate::render::sky::SkyPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, pipeline.depth_format());
            let background = crate::style::Rgba { r: 0xE7 as f32 / 255.0, g: 0xE3 as f32 / 255.0, b: 0xDF as f32 / 255.0, a: 1.0 };
            let ocean = crate::style::Rgba { r: 0.1, g: 0.6, b: 0.1, a: 1.0 };
            let day = crate::render::atmosphere::Sun::Map { lon_deg: 38.0, lat_deg: 5.0 };
            let night = crate::render::atmosphere::Sun::Map { lon_deg: 38.0 - 180.0, lat_deg: -5.0 };
            let grey = [0xE7u8, 0xE3, 0xDF];

            let (z5, plan) = sky_probe(&ctx, &pipeline, &sky, 5.0, day, background, ocean).await;
            eprintln!("z5 day top pixel {z5:?}, plan {plan:?}");
            assert!(plan.draws_sky, "z5 is inside the globe band: the sky must draw");
            assert!(
                rgb_gap(z5, grey) > 40,
                "z5 pitch 60: the sky above the limb is {z5:?}, which is the style background grey {grey:?}"
            );
            assert!(
                i32::from(z5[2]) > i32::from(z5[0]) + 30 && z5[2] > 120,
                "z5 pitch 60: the sky above the limb is {z5:?}, which is not blue"
            );
            assert!(z5[1] < 180 || z5[0] > 150, "{z5:?} reads as the green ocean sphere, not sky");

            let (z3, plan3) = sky_probe(&ctx, &pipeline, &sky, 3.0, day, background, ocean).await;
            eprintln!("z3 day top pixel {z3:?}");
            assert!(!plan3.draws_sky, "at full globe nothing of the sky is left");
            assert!(z3.iter().all(|&c| c <= 2), "z3: the top of the view is {z3:?}, not the space colour");

            let (dark, _) = sky_probe(&ctx, &pipeline, &sky, 5.0, night, background, ocean).await;
            eprintln!("z5 night top pixel {dark:?}");
            assert!(
                dark.iter().map(|&c| u32::from(c)).sum::<u32>() * 2 < z5.iter().map(|&c| u32::from(c)).sum::<u32>(),
                "the night-side sky {dark:?} is not clearly darker than the day-side {z5:?}"
            );
            assert!(dark[2] < 90, "the night-side sky is still bright blue: {dark:?}");
        });
    }

    /// The style background the sky tests grade against: the user's
    /// `#F4F1EF`, which must never show above the horizon.
    const SKY_TEST_BACKGROUND: crate::style::Rgba =
        crate::style::Rgba { r: 0xF4 as f32 / 255.0, g: 0xF1 as f32 / 255.0, b: 0xEF as f32 / 255.0, a: 1.0 };
    /// A GREEN ocean, so no probe can mistake the ground for sky.
    const SKY_TEST_OCEAN: crate::style::Rgba = crate::style::Rgba { r: 0.1, g: 0.6, b: 0.1, a: 1.0 };

    /// One frame of `zoom` tilted to `MAX_PITCH` over (38E, 5N), built the
    /// way the shells build it: `frame_plan::space_plan` with the geometric
    /// sky gate, the frame's clear, the star field (`with_stars`) and the
    /// ocean sphere LIT by the frame's sun through the shared
    /// `atmosphere::night_floor` -- as `MapView::paint` lights it. The ocean
    /// is drawn at every zoom, gate or not, so there is ground under the
    /// sky past z6 too. Returns the readback, its side and the plan.
    async fn sky_scene(
        ctx: &GpuContext,
        pipeline: &TilePipeline,
        sky: &crate::render::sky::SkyPipeline,
        stars: &crate::render::space::StarPipeline,
        zoom: f64,
        sun: crate::render::atmosphere::Sun,
        with_stars: bool,
    ) -> (Vec<u8>, u32, crate::render::frame_plan::SpacePlan) {
        let size = 256u32;
        let camera = crate::render::Camera {
            center_lon: 38.0,
            center_lat: 5.0,
            zoom,
            pitch: crate::camera_control::MAX_PITCH,
            bearing: 0.0,
            viewport_width_px: size as f32,
            viewport_height_px: size as f32,
            fov_y: crate::render::Camera::DEFAULT_FOV_Y,
        };
        let frame = crate::render::globe::globe_frame(&camera);
        let space = crate::render::space::Space { star_intensity: 1.0, ..Default::default() };
        let atmosphere = crate::render::atmosphere::Atmosphere {
            sun,
            // The terminator ON, named: the default is off since 2026-09-29.
            night_brightness: crate::render::atmosphere::Atmosphere::TERMINATOR_NIGHT_BRIGHTNESS,
            ..Default::default()
        };
        let plan = crate::render::frame_plan::space_plan(
            Some(SKY_TEST_BACKGROUND),
            &space,
            &atmosphere,
            &crate::style::SkyFrame::default(),
            frame.space_transition,
            crate::render::sky::sees_above_horizon(&camera, &frame),
        );
        if plan.draws_sky {
            // The ground below the limb in the OCEAN's colour, as the shells
            // paint it in the colour of their backdrop (both are the style
            // background there) -- so the probes below see one ground.
            sky.write(ctx, &camera, &frame, &plan.sky, &atmosphere, SKY_TEST_OCEAN);
        }
        if plan.draws_stars {
            stars.write_sky(ctx, &camera, &space);
        }
        let tile = crate::render::space::BACKDROP_TILE;
        let mesh = crate::render::space::backdrop_mesh(4096, SKY_TEST_OCEAN, SKY_TEST_OCEAN);
        let buffers = pipeline.create_tile_buffers(ctx, &mesh);
        let upload = crate::render::globe::GlobeUpload::new(
            &frame,
            &camera,
            4096.0,
            atmosphere.sun.direction_sphere(&frame.globe_view.inverse()),
            crate::render::atmosphere::night_floor(atmosphere.night_brightness_clamped(), frame.space_transition),
        );
        let model = crate::render::combined_tile_matrix(&camera, tile, 4096.0).to_cols_array_2d();
        let group = pipeline.create_model_bind_group(ctx, model, 1.0, 0.0, upload.for_tile(tile)).1;
        let space_draw = SpaceDraw {
            sky: if plan.draws_sky { Some(sky) } else { None },
            stars: if with_stars && plan.draws_stars { Some(stars) } else { None },
            backdrop: Some((&buffers, &group)),
            ..SpaceDraw::NONE
        };
        // The shells' own branch: the plan's clear on a globe-band frame or
        // one whose sky draws, the style background otherwise.
        let clear = if frame.space_transition > 0.0 || plan.draws_sky {
            TilePipeline::clear_color(&plan.clear_color)
        } else {
            TilePipeline::clear_color(&SKY_TEST_BACKGROUND)
        };
        let pixels = render_space_frame(ctx, pipeline, &camera, size, clear, &space_draw, &[]).await;
        (pixels, size, plan)
    }

    /// Whether a pixel is the green ocean sphere (at any shade).
    fn is_sky_test_ocean(p: [u8; 3]) -> bool {
        i32::from(p[1]) > 2 * i32::from(p[0]) + 10 && i32::from(p[1]) > 2 * i32::from(p[2]) + 10
    }

    /// The row of the limb in the centre column: the first ocean pixel from
    /// the top. Panics if the column has no sky at all.
    fn sky_test_limb_row(pixels: &[u8], size: u32) -> u32 {
        let row = (0..size).find(|&y| is_sky_test_ocean(rgb_at(pixels, size, size / 2, y))).unwrap_or(size);
        assert!(row > 8, "the centre column is ground from row {row}: no sky on screen to grade");
        row
    }

    /// **THE NIGHT RULE, AS PIXELS.** The user: "on those areas that are
    /// under terminator shadow, sky should be black stars not grey not
    /// blue". Tilted to the limit at z4.5, z5 and z6.5 -- inside the band
    /// and past it -- with the sun over the far side of the planet:
    ///
    ///  - the sky above the limb is SPACE: every channel under 20, with the
    ///    stars off so a star cannot be mistaken for sky (master: z4.5
    ///    `(18, 25, 32)` -- the white horizon times 0.25, grey);
    ///  - with the stars on, they show: some sky pixel gains 60+;
    ///  - by DAY the same frames are blue and the stars do NOT show: no sky
    ///    pixel differs by more than 2 with the star field on or off
    ///    (master: at z4.5 half of every star came through the blue).
    #[test]
    fn the_night_sky_is_black_with_stars_and_the_day_sky_is_blue_without() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let sky = crate::render::sky::SkyPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, pipeline.depth_format());
            let stars = crate::render::space::StarPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, pipeline.depth_format());
            let day = crate::render::atmosphere::Sun::Map { lon_deg: 38.0, lat_deg: 5.0 };
            let night = crate::render::atmosphere::Sun::Map { lon_deg: 38.0 - 180.0, lat_deg: -5.0 };
            for zoom in [4.5, 5.0, 6.5] {
                let (dark, size, plan) = sky_scene(&ctx, &pipeline, &sky, &stars, zoom, night, false).await;
                assert!(plan.draws_sky, "z{zoom}: the sky must draw");
                let limb = sky_test_limb_row(&dark, size);
                for (x, y) in [(size / 2, 2), (size / 4, 2), (3 * size / 4, 2), (size / 2, limb / 2), (size / 2, limb - 3)] {
                    let p = rgb_at(&dark, size, x, y);
                    eprintln!("z{zoom} night sky ({x},{y}) {p:?}");
                    assert!(p.iter().all(|&c| c < 20), "z{zoom}: the night sky at ({x},{y}) is {p:?}, not space");
                }
                let (starry, _, _) = sky_scene(&ctx, &pipeline, &sky, &stars, zoom, night, true).await;
                let night_gain = sky_star_gain(&dark, &starry, size, limb);
                eprintln!("z{zoom} night: the brightest star adds {night_gain}");
                assert!(night_gain >= 60, "z{zoom}: the stars do not show in the night sky (brightest adds {night_gain})");

                let (blue, _, _) = sky_scene(&ctx, &pipeline, &sky, &stars, zoom, day, false).await;
                let top = rgb_at(&blue, size, size / 2, 2);
                eprintln!("z{zoom} day sky top {top:?}");
                assert!(i32::from(top[2]) > i32::from(top[0]) + 30, "z{zoom}: the day sky {top:?} is not blue");
                let (blue_starry, _, _) = sky_scene(&ctx, &pipeline, &sky, &stars, zoom, day, true).await;
                let day_limb = sky_test_limb_row(&blue, size);
                let day_gain = sky_star_gain(&blue, &blue_starry, size, day_limb);
                eprintln!("z{zoom} day: the brightest star adds {day_gain}");
                assert!(day_gain <= 2, "z{zoom}: a star shows through the DAY sky (adds {day_gain})");
            }
        });
    }

    /// The most any star adds to any SKY pixel (rows above `limb`, not
    /// ocean) between a frame without the star field and one with it.
    fn sky_star_gain(without: &[u8], with: &[u8], size: u32, limb: u32) -> i32 {
        let mut gain = 0;
        for y in 0..limb.saturating_sub(2) {
            for x in 0..size {
                let a = rgb_at(without, size, x, y);
                if is_sky_test_ocean(a) {
                    continue;
                }
                gain = gain.max(rgb_gap(a, rgb_at(with, size, x, y)));
            }
        }
        gain
    }

    /// **THE GREY BAND AT THE LIMB.** The user, on an iPhone, by day: "I
    /// think at some point sky looks a bit grey" -- `#ABC7E3` at the top
    /// running to a near-white `#E2E4E7` at the horizon. MapLibre's white
    /// `horizon-color` blended into `#88C6FC` is grey haze, not bright sky.
    /// On a daytime camera the pixel just above the limb must be BLUE --
    /// blue over red by 35 and saturation of at least 0.15 -- at z4.5 and
    /// z5.5. Master: z5.5 was `(228, 232, 236)`, saturation 0.03.
    #[test]
    fn the_day_sky_is_blue_down_to_the_limb() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let sky = crate::render::sky::SkyPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, pipeline.depth_format());
            let stars = crate::render::space::StarPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, pipeline.depth_format());
            let day = crate::render::atmosphere::Sun::Map { lon_deg: 38.0, lat_deg: 5.0 };
            for zoom in [4.5, 5.5] {
                let (pixels, size, _) = sky_scene(&ctx, &pipeline, &sky, &stars, zoom, day, false).await;
                let limb = sky_test_limb_row(&pixels, size);
                let p = rgb_at(&pixels, size, size / 2, limb - 3);
                let (max, min) = (i32::from(*p.iter().max().unwrap()), i32::from(*p.iter().min().unwrap()));
                let saturation = (max - min) as f32 / max.max(1) as f32;
                eprintln!("z{zoom} day: just above the limb (row {}) {p:?}, saturation {saturation:.3}", limb - 3);
                assert!(
                    i32::from(p[2]) > i32::from(p[0]) + 35 && saturation >= 0.15,
                    "z{zoom}: the day sky just above the limb is {p:?} (saturation {saturation:.3}) -- grey haze, not blue"
                );
            }
        });
    }

    /// **The ground under a night sky is dark too.** At z4.5 the night-side
    /// ocean used to be at 0.625 of the day side (`1 - 0.75 * t`, t = 0.5)
    /// under a sky darkened to 0.25: a lit sea under a night sky. Through
    /// `atmosphere::night_floor` it is at `1 - 0.75 * t * (2 - t)` = 0.4375,
    /// and the pixels say so.
    #[test]
    fn the_night_side_ground_is_dark_under_the_night_sky() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let sky = crate::render::sky::SkyPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, pipeline.depth_format());
            let stars = crate::render::space::StarPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, pipeline.depth_format());
            let day = crate::render::atmosphere::Sun::Map { lon_deg: 38.0, lat_deg: 5.0 };
            let night = crate::render::atmosphere::Sun::Map { lon_deg: 38.0 - 180.0, lat_deg: -5.0 };
            let (lit, size, _) = sky_scene(&ctx, &pipeline, &sky, &stars, 4.5, day, false).await;
            let (dark, _, _) = sky_scene(&ctx, &pipeline, &sky, &stars, 4.5, night, false).await;
            let (a, b) = (rgb_at(&lit, size, size / 2, size - 4), rgb_at(&dark, size, size / 2, size - 4));
            let ratio = f32::from(b[1]) / f32::from(a[1]);
            let t = crate::render::globe::space_transition_for_zoom(4.5);
            let floor = crate::render::atmosphere::night_floor(0.25, t);
            eprintln!("z4.5 ground: day {a:?}, night {b:?}, ratio {ratio:.3}, floor {floor:.4}");
            assert!(ratio <= 0.5, "the night-side ocean is {ratio:.3} of the day side -- a lit sea under a night sky");
            assert!((ratio - floor).abs() < 0.02, "the ground is shaded by {ratio:.3}, not the shared floor {floor:.4}");
        });
    }

    /// **THE FLAT BACKGROUND ABOVE THE CURVED HORIZON, AS PIXELS.** At
    /// z6.0, z6.5, z7 and z8, tilted to the limit, the curved horizon is on
    /// screen (the projection is spherical up to z11). Master drew the
    /// sky only while `space_transition > 0`, so everything above the limb
    /// was the style background `#F4F1EF`. Now not one pixel of the frame is
    /// that colour, by day or by night, and the night sky is space.
    #[test]
    fn no_background_shows_above_the_curved_horizon_past_the_globe_band() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let sky = crate::render::sky::SkyPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, pipeline.depth_format());
            let stars = crate::render::space::StarPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, pipeline.depth_format());
            let day = crate::render::atmosphere::Sun::Map { lon_deg: 38.0, lat_deg: 5.0 };
            let night = crate::render::atmosphere::Sun::Map { lon_deg: 38.0 - 180.0, lat_deg: -5.0 };
            let background = [0xF4u8, 0xF1, 0xEF];
            for zoom in [6.0, 6.5, 7.0, 8.0] {
                for (sun, name) in [(day, "day"), (night, "night")] {
                    let (pixels, size, plan) = sky_scene(&ctx, &pipeline, &sky, &stars, zoom, sun, false).await;
                    let limb = sky_test_limb_row(&pixels, size);
                    let top = rgb_at(&pixels, size, size / 2, 1);
                    let background_px = (0..size)
                        .flat_map(|y| (0..size).map(move |x| (x, y)))
                        .filter(|&(x, y)| rgb_gap(rgb_at(&pixels, size, x, y), background) <= 6)
                        .count();
                    eprintln!("z{zoom} {name}: limb row {limb}, top {top:?}, {background_px} background pixels, sky {}", plan.draws_sky);
                    assert!(plan.draws_sky, "z{zoom} {name}: the curved horizon is on screen, the sky must draw");
                    assert_eq!(background_px, 0, "z{zoom} {name}: {background_px} pixels are the flat style background");
                    if name == "night" {
                        assert!(top.iter().all(|&c| c < 20), "z{zoom} night: the sky above the horizon is {top:?}, not space");
                    } else {
                        assert!(i32::from(top[2]) > i32::from(top[0]) + 30, "z{zoom} day: the sky {top:?} is not blue");
                    }
                }
            }
        });
    }

    /// 2026-09-24T22:30Z, the instant of the user's Kansas screenshot: the
    /// sun is up over the whole of the US.
    const KANSAS_EVENING_UTC: f64 = 1_790_289_000.0;

    /// The user's camera: z6.77 over Kansas, tilted to the limit.
    fn kansas_camera(size: u32) -> crate::render::Camera {
        crate::render::Camera {
            center_lon: -95.618,
            center_lat: 39.4235,
            zoom: 6.77,
            pitch: crate::camera_control::MAX_PITCH,
            bearing: 0.0,
            viewport_width_px: size as f32,
            viewport_height_px: size as f32,
            fov_y: crate::render::Camera::DEFAULT_FOV_Y,
        }
    }

    /// One frame built EXACTLY as the shells build it -- unlike
    /// [`sky_scene`], which draws its ocean at every zoom whatever the plan
    /// says, and so could not see a frame whose ground was missing. The
    /// style BACKGROUND is bare land (Mapbox Streets paints it with its
    /// `background` layer and nothing else): the backdrop sphere in that
    /// colour draws only when `plan.draws_backdrop` says so, the sky pass is
    /// written with `plan.ground`, and the clear is the shells' own branch.
    /// `fill`, when given, is a translucent planet-wide fill drawn as a tile
    /// -- a landuse/urban polygon at its style opacity. `with_sky: false` is
    /// the pre-`932a7ae` frame: no sky pass past z6, so the frame clears to
    /// the background.
    #[allow(clippy::too_many_arguments)]
    async fn shell_scene(
        ctx: &GpuContext,
        pipeline: &TilePipeline,
        sky: &crate::render::sky::SkyPipeline,
        stars: &crate::render::space::StarPipeline,
        camera: crate::render::Camera,
        sun: crate::render::atmosphere::Sun,
        with_stars: bool,
        with_sky: bool,
        fill: Option<crate::style::Rgba>,
    ) -> (Vec<u8>, crate::render::frame_plan::SpacePlan) {
        let size = camera.viewport_height_px as u32;
        let frame = crate::render::globe::globe_frame(&camera);
        let space = crate::render::space::Space { star_intensity: 1.0, ..Default::default() };
        let atmosphere = crate::render::atmosphere::Atmosphere {
            sun,
            // The terminator ON, named: the default is off since 2026-09-29.
            night_brightness: crate::render::atmosphere::Atmosphere::TERMINATOR_NIGHT_BRIGHTNESS,
            ..Default::default()
        };
        let plan = crate::render::frame_plan::space_plan(
            Some(SKY_TEST_BACKGROUND),
            &space,
            &atmosphere,
            &crate::style::SkyFrame::default(),
            frame.space_transition,
            with_sky && crate::render::sky::sees_above_horizon(&camera, &frame),
        );
        if plan.draws_sky {
            sky.write(ctx, &camera, &frame, &plan.sky, &atmosphere, plan.ground);
        }
        if plan.draws_stars {
            stars.write_sky(ctx, &camera, &space);
        }
        let tile = crate::render::space::BACKDROP_TILE;
        let upload = crate::render::globe::GlobeUpload::new(
            &frame,
            &camera,
            4096.0,
            atmosphere.sun.direction_sphere(&frame.globe_view.inverse()),
            crate::render::atmosphere::night_floor(atmosphere.night_brightness_clamped(), frame.space_transition),
        );
        let model = crate::render::combined_tile_matrix(&camera, tile, 4096.0).to_cols_array_2d();
        let group = pipeline.create_model_bind_group(ctx, model, 1.0, 0.0, upload.for_tile(tile)).1;
        let ground = crate::render::space::backdrop_mesh(4096, SKY_TEST_BACKGROUND, SKY_TEST_BACKGROUND);
        let ground = pipeline.create_tile_buffers(ctx, &ground);
        let fill = fill.map(|colour| pipeline.create_tile_buffers(ctx, &crate::render::space::backdrop_mesh(4096, colour, colour)));
        let draws: Vec<(&TileBuffers, &wgpu::BindGroup)> = fill.iter().map(|buffers| (buffers, &group)).collect();
        let space_draw = SpaceDraw {
            sky: if plan.draws_sky { Some(sky) } else { None },
            stars: if with_stars && plan.draws_stars { Some(stars) } else { None },
            backdrop: if plan.draws_backdrop { Some((&ground, &group)) } else { None },
            ..SpaceDraw::NONE
        };
        let clear = if frame.space_transition > 0.0 || plan.draws_sky {
            TilePipeline::clear_color(&plan.clear_color)
        } else {
            TilePipeline::clear_color(&SKY_TEST_BACKGROUND)
        };
        let pixels = render_space_frame(ctx, pipeline, &camera, size, clear, &space_draw, &draws).await;
        (pixels, plan)
    }

    fn luminance(p: [u8; 3]) -> f32 {
        0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2])
    }

    /// Rows from here down are GROUND in every z6.77 MAX_PITCH frame these
    /// tests draw: the limb is about a quarter of the way down.
    fn ground_rows(size: u32) -> std::ops::Range<u32> {
        (size * 2 / 5)..size
    }

    /// **(a) THE KANSAS DAYLIGHT FRAME IS AS BRIGHT AS IT WAS.** The user's
    /// screenshot: z6.77, tilted, 2026-09-24T22:30Z -- sun up everywhere in
    /// view -- and a dark curved "shadow" over Nebraska, Kansas and Iowa.
    /// Since `932a7ae` such a frame draws its sky and clears to SPACE, and
    /// from z6 up there is no ocean sphere under the tiles, so bare land
    /// showed the full-screen sky pass's below-limb colour: the horizon
    /// colour by "day" and black by "night", with a night computed from a
    /// normal the sky shader takes from each ray's point nearest the
    /// planet's centre -- meaningless for a ray that hits the ground. Every ground pixel must
    /// be within 2% of the luminance of the pre-`932a7ae` frame (no sky,
    /// background clear), at the real sun and the sun overhead.
    ///
    /// Master: the evening frame's ground averaged 0.83 of the baseline in
    /// the capture harness, with pixels down to black.
    #[test]
    fn the_kansas_daylight_ground_is_as_bright_as_before_the_sky_drew_past_z6() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let sky = crate::render::sky::SkyPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, pipeline.depth_format());
            let stars = crate::render::space::StarPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, pipeline.depth_format());
            let size = 256;
            let camera = kansas_camera(size);
            let evening = crate::render::atmosphere::Sun::at_utc(KANSAS_EVENING_UTC);
            let overhead = crate::render::atmosphere::Sun::Map { lon_deg: -95.618, lat_deg: 39.4235 };
            let (baseline, base_plan) = shell_scene(&ctx, &pipeline, &sky, &stars, camera, evening, false, false, None).await;
            assert!(!base_plan.draws_sky, "the baseline is the frame with no sky");
            for (sun, name) in [(evening, "evening"), (overhead, "overhead")] {
                let (pixels, plan) = shell_scene(&ctx, &pipeline, &sky, &stars, camera, sun, true, true, None).await;
                assert!(plan.draws_sky, "{name}: the curved horizon is on screen, so the sky draws");
                let top = rgb_at(&pixels, size, size / 2, 1);
                assert!(i32::from(top[2]) > i32::from(top[0]) + 30, "{name}: the day sky {top:?} is not blue");
                let mut worst = (0.0f32, 0, 0, [0u8; 3], [0u8; 3]);
                for y in ground_rows(size) {
                    for x in 0..size {
                        let (now, then) = (rgb_at(&pixels, size, x, y), rgb_at(&baseline, size, x, y));
                        let off = (luminance(now) - luminance(then)).abs() / luminance(then).max(1.0);
                        if off > worst.0 {
                            worst = (off, x, y, now, then);
                        }
                    }
                }
                let (off, x, y, now, then) = worst;
                eprintln!("{name}: worst ground pixel ({x},{y}) {now:?} vs baseline {then:?}, {:.2}%", off * 100.0);
                assert!(
                    off <= 0.02,
                    "{name}: the ground at ({x},{y}) is {now:?}, {:.1}% off the pre-sky frame's {then:?}",
                    off * 100.0
                );
            }
        });
    }

    /// **(b) NO FILL RENDERS BLACK AT z6.77, UNDER ANY SUN.** The user's
    /// black Omaha, Kansas City, Wichita, Lincoln and Hutchinson: urban and
    /// residential fills are translucent over bare land, so over black space
    /// they were black. And the Atacama at z6.86: large areas of pure black
    /// ground with STARS in them. Over the Kansas camera, with the sun at
    /// the real 22:30Z point, overhead, on the far side of the planet and at
    /// eight longitudes around the equator, a translucent fill over the
    /// background has no pixel darker than half the fill's own composite
    /// over the background, and the ground rows are the same pixels with the
    /// star field on as off -- no star below the horizon.
    #[test]
    fn no_fill_renders_black_and_no_star_shows_below_the_horizon_at_z6_77() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let sky = crate::render::sky::SkyPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, pipeline.depth_format());
            let stars = crate::render::space::StarPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, pipeline.depth_format());
            let size = 256;
            let camera = kansas_camera(size);
            // Mapbox Streets' residential/urban fill: pale grey at half
            // opacity. Over the background that is a pale grey pixel.
            let urban = crate::style::Rgba { r: 0.85, g: 0.85, b: 0.87, a: 0.5 };
            let mut suns = vec![
                crate::render::atmosphere::Sun::at_utc(KANSAS_EVENING_UTC),
                crate::render::atmosphere::Sun::Map { lon_deg: -95.618, lat_deg: 39.4235 },
                crate::render::atmosphere::Sun::Map { lon_deg: 84.382, lat_deg: -39.4235 },
            ];
            suns.extend((0..8).map(|i| crate::render::atmosphere::Sun::Map { lon_deg: -180.0 + 45.0 * f64::from(i), lat_deg: 0.0 }));
            for sun in suns {
                let (quiet, _) = shell_scene(&ctx, &pipeline, &sky, &stars, camera, sun, false, true, Some(urban)).await;
                let (starry, plan) = shell_scene(&ctx, &pipeline, &sky, &stars, camera, sun, true, true, Some(urban)).await;
                assert!(plan.draws_sky && plan.draws_stars, "{sun:?}: the sky and the stars draw above the limb");
                let (mut darkest, mut at) = (f32::MAX, (0, 0, [0u8; 3]));
                let mut star_px = 0;
                for y in ground_rows(size) {
                    for x in 0..size {
                        let p = rgb_at(&starry, size, x, y);
                        if luminance(p) < darkest {
                            darkest = luminance(p);
                            at = (x, y, p);
                        }
                        if rgb_gap(p, rgb_at(&quiet, size, x, y)) > 2 {
                            star_px += 1;
                        }
                    }
                }
                eprintln!("{sun:?}: darkest ground pixel {:?} at ({},{}), {star_px} star pixels on the ground", at.2, at.0, at.1);
                // Over the background the fill composites to ~(233, 231, 232):
                // luminance ~231. Half of that is a generous floor that pure
                // black, and the dark green over black, both fail.
                assert!(darkest > 115.0, "{sun:?}: the fill at ({},{}) is {:?} -- black under the map", at.0, at.1, at.2);
                assert_eq!(star_px, 0, "{sun:?}: {star_px} ground pixels change with the star field on -- stars below the horizon");
            }
        });
    }

    /// **(c) THE NIGHT IS ON THE RIGHT SIDE OF THE TERMINATOR**, for the
    /// real sun of 2026-09-24T22:30Z from `render::solar`. The evening
    /// terminator is 90 degrees EAST of the subsolar point: the day is west
    /// of it (toward the sun), the night east.
    ///
    ///  - At z6.86 tilted (the Atacama frame, moved 15 degrees either side
    ///    of the terminator): the sky west of it is blue, east of it space;
    ///    and the ground
    ///    is not dimmed at all -- the night floor is 1.0 from z6 -- so every
    ///    ground pixel is the background, not the black of the Atacama
    ///    screenshot.
    ///  - At z4.5, pitch 0, inside the globe band where the ground IS
    ///    shaded: the west of the view is lit and the east is dark.
    #[test]
    fn the_night_side_is_east_of_the_evening_terminator_as_solar_puts_it() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let sky = crate::render::sky::SkyPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, pipeline.depth_format());
            let stars = crate::render::space::StarPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, pipeline.depth_format());
            let (sub_lon, sub_lat) = crate::render::solar::subsolar_point(KANSAS_EVENING_UTC);
            assert!((sub_lon + 159.5).abs() < 1.5 && sub_lat.abs() < 1.0, "solar: 22:30Z near the equinox is ({sub_lon}, {sub_lat})");
            let sun = crate::render::atmosphere::Sun::at_utc(KANSAS_EVENING_UTC);
            let terminator_lon = sub_lon + 90.0;
            let size = 256;
            let background = [0xF4u8, 0xF1, 0xEF];

            // A z6.86 view spans only a few degrees -- about the
            // terminator's own width -- so each side is graded from a camera
            // 15 degrees off it.
            let tilted = |lon: f64| crate::render::Camera { center_lon: lon, center_lat: 0.0, zoom: 6.86, ..kansas_camera(size) };
            for (offset, side) in [(-15.0, "west"), (15.0, "east")] {
                let camera = tilted(terminator_lon + offset);
                let (pixels, plan) = shell_scene(&ctx, &pipeline, &sky, &stars, camera, sun, true, true, None).await;
                assert!(plan.draws_sky, "z6.86 {side}: the sky draws");
                let top = rgb_at(&pixels, size, size / 2, 2);
                eprintln!("z6.86, 15 degrees {side} of the terminator: sky {top:?}");
                if side == "west" {
                    assert!(i32::from(top[2]) > i32::from(top[0]) + 30, "the sky on the sun's side (west) is {top:?}, not day blue");
                } else {
                    assert!(luminance(top) < 20.0, "the sky away from the sun (east) is {top:?}, not night");
                }
                let not_ground = ground_rows(size)
                    .flat_map(|y| (0..size).map(move |x| (x, y)))
                    .filter(|&(x, y)| rgb_gap(rgb_at(&pixels, size, x, y), background) > 2)
                    .count();
                assert_eq!(
                    not_ground, 0,
                    "z6.86 {side}: {not_ground} ground pixels are not the background -- the ground is never dimmed from z6"
                );
            }

            let flat = crate::render::Camera { zoom: 4.5, pitch: 0.0, ..tilted(terminator_lon) };
            let (pixels, _) = shell_scene(&ctx, &pipeline, &sky, &stars, flat, sun, false, true, None).await;
            let (west, east) = (rgb_at(&pixels, size, size / 8, size / 2), rgb_at(&pixels, size, 7 * size / 8, size / 2));
            eprintln!("z4.5 on the terminator: ground west {west:?}, east {east:?}");
            assert!(rgb_gap(west, background) <= 2, "the ground toward the sun (west) is {west:?}, not lit");
            assert!(luminance(east) < 0.6 * luminance(west), "the ground away from the sun (east) is {east:?}, not night");
        });
    }

    /// **A flat frame is pixel-identical through the space-aware path.**
    ///
    /// Tasks 3, 5 and 7 each preserved bit-identical flat output and each
    /// has a test for it; this change lands closer to the flat path than
    /// any of them, because it touches the clear colour itself. So the
    /// assertion is byte equality of the whole readback, between
    ///
    ///  - `render`, the seven-argument entry point every pre-existing call
    ///    site uses, and
    ///  - `render_with_space` handed a `SpaceDraw` built the way
    ///    `MapView::paint` builds one at `transition == 0.0` — which is
    ///    `SpaceDraw::NONE`, and that is what is being tested: that the
    ///    GATE, not the delegation, is what makes them equal.
    ///
    /// The backdrop and the star pipeline are both CONSTRUCTED here and
    /// fully populated, and whether they are PASSED is decided by the same
    /// two gates `paint` reads. That is not decoration: sabotaging each
    /// gate in turn is how two separate ways of this test proving nothing
    /// were found.
    ///
    ///  - **the backdrop was the background colour**, so a backdrop
    ///    wrongly drawn on a flat frame painted the style's own background
    ///    over a clear of the same colour and changed no bytes. It is
    ///    magenta now, and loosening `space::draws_backdrop` to `>= 0.0`
    ///    moves **408,771** bytes.
    ///  - **the sky uniform was never written**, so the star field drew
    ///    nothing whatever the gate said: a zeroed matrix makes every
    ///    star's `w` zero and `star_shader.wgsl`'s behind-the-camera guard
    ///    fire for all 9,096. `write_sky` is called now, and loosening
    ///    `Space::draws_stars` to `>= 0.0` moves **1,822** bytes.
    ///
    /// Measured with both gates intact: **0 of 1,048,576 bytes differ**,
    /// over a flat z14 tile at `transition = 0.0` with 136,257 of the
    /// 262,144 pixels at the clear colour.
    #[test]
    fn a_flat_frame_is_pixel_identical_through_the_space_aware_path() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 512u32;
            let extent = 4096u32;
            // Above PROJECTION_BLEND_ZOOM_MAX, so `transition_for_zoom` is
            // exactly 0.0 -- a real flat frame, not a globe one with the globe
            // turned off by hand.
            let camera = crate::render::Camera {
                center_lon: 139.7671,
                center_lat: 35.6812,
                zoom: 14.0,
                pitch: 0.3,
                bearing: 0.2,
                viewport_width_px: size as f32,
                viewport_height_px: size as f32,
                fov_y: crate::render::Camera::DEFAULT_FOV_Y,
            };
            let transition = crate::render::globe::transition_for_zoom(camera.zoom);
            assert_eq!(transition, 0.0, "this test's camera must be a FLAT one");

            let background =
                crate::style::Rgba { r: 79.0 / 255.0, g: 116.0 / 255.0, b: 153.0 / 255.0, a: 1.0 };
            // Stars turned fully ON in the config, so the only thing
            // keeping them off the frame is the transition gate.
            let space = crate::render::space::Space {
                star_intensity: 1.0,
                ..crate::render::space::Space::default()
            };
            let clear = TilePipeline::clear_color(&crate::render::space::space_clear_color(
                Some(background),
                &space,
                transition,
            ));
            assert_eq!(
                clear,
                TilePipeline::clear_color(&background),
                "the flat clear colour must be the background colour, byte for byte"
            );

            let (tile_x, tile_y) =
                crate::geo::lon_lat_to_tile_xy(camera.center_lon, camera.center_lat, 14.0);
            let tile =
                crate::geo::TileId { z: 14, x: tile_x as u32, y: tile_y as u32 };
            let mesh = crate::render::tessellate_plan(
                &crate::render::space::backdrop_plan(
                    extent,
                    crate::style::Rgba { r: 0.8, g: 0.3, b: 0.1, a: 1.0 },
                    crate::style::Rgba { r: 0.8, g: 0.3, b: 0.1, a: 1.0 },
                ),
                extent,
                crate::render::mesh::COORD_SCALE,
                crate::render::subdivide::Subdivision::for_tile(tile),
            );
            let buffers = pipeline.create_tile_buffers(&ctx, &mesh);
            let model =
                crate::render::combined_tile_matrix(&camera, tile, extent as f32).to_cols_array_2d();
            let group = pipeline
                .create_model_bind_group(&ctx, model, 1.0, 0.0, GlobeParams::FLAT)
                .1;
            let draws = [(&buffers, &group)];

            // Both halves are BUILT, and whether they are passed is decided
            // by the SAME two gates `MapView::paint` reads -- see this
            // test's doc comment for why it must not decide for itself.
            let stars = crate::render::space::StarPipeline::new(
                &ctx,
                wgpu::TextureFormat::Rgba8Unorm,
                pipeline.depth_format(),
            );
            // The sky uniform is WRITTEN, and that matters for the same
            // reason the magenta below does. Left unwritten the buffer is
            // zeroes, `sky_view_proj * vec4(dir, 1)` comes out with
            // `w == 0`, `star_shader.wgsl`'s behind-the-camera guard fires
            // for every star and the field draws nothing -- so this test
            // passed with the star gate deleted, by accident rather than by
            // the gate.
            stars.write_sky(&ctx, &camera, &space);
            // MAGENTA, not the background colour: a backdrop wrongly drawn
            // on a flat frame would paint the style's own background over
            // the clear of the same colour and leave the readback
            // unchanged, so the test would pass with the gate deleted. It
            // did, until this line. (A flat z0 tile at camera z14 is
            // 512 * 2^14 logical px across, so it covers the viewport
            // completely -- there is nowhere for it to hide.)
            let (backdrop_buffers, backdrop_group) = backdrop_draw(
                &ctx,
                &pipeline,
                &camera,
                extent,
                crate::style::Rgba { r: 1.0, g: 0.0, b: 1.0, a: 1.0 },
            );
            // THE ATMOSPHERE'S UNIFORM IS WRITTEN FOR A GLOBE FRAME, ON
            // PURPOSE, and it is the same trick as the magenta above.
            //
            // `Atmosphere::blend` is `intensity * transition`, so a uniform
            // written from THIS camera would carry a blend of exactly zero,
            // the shader would emit `(0,0,0,0)`, and premultiplied blending
            // would leave the framebuffer untouched -- meaning a deleted
            // gate would change no bytes and this test would grade nothing.
            // Writing a full-globe frame's uniform instead makes the draw
            // loud if it ever happens: sabotaging `Atmosphere::draws` to
            // `>= 0.0` moves **514,056 of the 1,048,576 bytes**, measured.
            let atmosphere = crate::render::atmosphere::AtmospherePipeline::new(
                &ctx,
                wgpu::TextureFormat::Rgba8Unorm,
                pipeline.depth_format(),
            );
            let globe_camera = crate::render::Camera { zoom: 2.0, ..camera };
            let globe_frame = crate::render::globe::globe_frame(&globe_camera);
            assert_eq!(
                globe_frame.transition, 1.0,
                "the uniform this test writes must be a FULL-globe one, or a leaked \
                 atmosphere draw would be invisible and the gate below untested"
            );
            atmosphere.write(&ctx, &globe_camera, &globe_frame, &space.atmosphere);
            // THE SKY, the same trick again: its uniform is written from a
            // mid-band globe camera (z4.5, coverage well above zero), so a
            // sky pass leaked onto this flat frame would paint the whole
            // viewport. Whether it is passed is the plan's own gate.
            let sky = crate::render::sky::SkyPipeline::new(
                &ctx,
                wgpu::TextureFormat::Rgba8Unorm,
                pipeline.depth_format(),
            );
            let sky_camera = crate::render::Camera { zoom: 4.5, ..camera };
            let sky_frame = crate::render::globe::globe_frame(&sky_camera);
            assert!(crate::render::sky::sky_alpha(sky_frame.space_transition) > 0.3);
            let sky_style = crate::style::SkyFrame::default();
            // A MAGENTA ground, so a leaked pass is loud below the limb too.
            let leak_ground = crate::style::Rgba { r: 1.0, g: 0.0, b: 1.0, a: 1.0 };
            sky.write(&ctx, &sky_camera, &sky_frame, &sky_style, &space.atmosphere, leak_ground);
            let gated = SpaceDraw {
                // The geometric gate, from THIS frame's camera: a z14 frame
                // at pitch 0.3 rad sees no pixel above the horizon.
                sky: if crate::render::sky::draws_sky(
                    &sky_style,
                    transition,
                    crate::render::sky::sees_above_horizon(&camera, &crate::render::globe::globe_frame(&camera)),
                ) {
                    Some(&sky)
                } else {
                    None
                },
                stars: if space.draws_stars(transition) { Some(&stars) } else { None },
                backdrop: if crate::render::space::draws_backdrop(transition) {
                    Some((&backdrop_buffers, &backdrop_group))
                } else {
                    None
                },
                atmosphere: if space.atmosphere.draws(transition) {
                    Some(&atmosphere)
                } else {
                    None
                },
                look: None,
            };
            assert_eq!(
                space.atmosphere.intensity, 1.0,
                "the atmosphere must be fully ON in the config, so that the only thing \
                 keeping it off this frame is the transition gate"
            );

            let through_space =
                render_space_frame(&ctx, &pipeline, &camera, size, clear, &gated, &draws).await;

            let color_texture = make_color_texture(&ctx, size);
            let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor::default());
            let depth_view = pipeline.create_depth_view(&ctx, size, size);
            let camera_bind_group = pipeline.test_camera_bind_group(
                &ctx,
                camera.view_projection().to_cols_array_2d(),
                camera.zoom as f32,
            );
            let mut encoder =
                ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            pipeline.render(
                &mut encoder,
                &color_view,
                &depth_view,
                clear,
                &camera_bind_group,
                &draws,
                None,
            );
            let through_plain = read_all_pixels(&ctx, &color_texture, encoder, size).await;

            let differing = through_space
                .iter()
                .zip(through_plain.iter())
                .filter(|(a, b)| a != b)
                .count();
            assert_eq!(
                differing, 0,
                "{differing} of {} bytes differ between the flat frame drawn through \
                 `render` and the same frame drawn through `render_with_space`",
                through_plain.len()
            );
            // ...and the frame is not empty, or "identical" would be a
            // statement about two blank textures.
            let lit = through_plain.as_chunks::<4>().0.iter().filter(|p| p[3] > 20).count();
            assert!(lit > 1_000, "only {lit} lit pixels -- the flat control drew nothing");
            // ...and more than half of it is the CLEAR colour, which is
            // where a leaked star or a leaked backdrop would have to show.
            // Measured: 136,257 of 262,144. A frame the tile covered
            // completely would make the byte equality above vacuous, and
            // that is not a hypothetical -- see this test's doc comment for
            // the two ways it was vacuous before.
            let clear_rgb = [
                (background.r * 255.0).round() as u8,
                (background.g * 255.0).round() as u8,
                (background.b * 255.0).round() as u8,
            ];
            let uncovered = (0..size)
                .flat_map(|row| (0..size).map(move |col| (col, row)))
                .filter(|(c, r)| rgb_at(&through_plain, size, *c, *r) == clear_rgb)
                .count();
            assert!(
                uncovered > 100_000,
                "only {uncovered} of {} pixels are the clear colour, so there is nowhere \
                 for a wrongly-drawn backdrop or star to appear and the equality above \
                 proves less than it looks like",
                size * size
            );
        });
    }

    /// **THE ACCEPTANCE CRITERION FOR `MapView::set_globe_enabled(false)`:
    /// inside the blend band, the switched-off frame is the pure-Mercator
    /// frame, byte for byte.**
    ///
    /// The switch has to be graded at a zoom where it actually does
    /// something. Above `PROJECTION_BLEND_ZOOM_MAX` every frame is already
    /// flat, so "globe off looks like Mercator" is true there for a reason
    /// that has nothing to do with the switch. **z11.133** is inside the
    /// band, and is the zoom MapLibre's own `projection_factory.ts` puts at
    /// 0.867 -- which is what `transition_for_zoom` returns here. So the
    /// globe-on frame at this camera is 87% sphere, and the switch has to
    /// move it all the way back.
    ///
    /// Three renders of ONE mesh, ONE camera and ONE model matrix, differing
    /// only in the `GlobeParams` bound:
    ///
    ///  1. **`GlobeParams::FLAT`** — what this renderer drew before the
    ///     globe existed, and the reference "pure Mercator" frame.
    ///  2. **The switch off**: `globe_frame(&camera).flattened()` through
    ///     the real `GlobeUpload::new(..).for_tile(..)` production path, not
    ///     a hand-built uniform. Must equal (1) byte for byte.
    ///  3. **The switch on**: the same frame un-flattened. Must NOT equal
    ///     (1), or (2) would be a statement about a camera where nothing was
    ///     going to happen anyway.
    ///
    /// (2) is the non-obvious one: a flattened frame still carries the real
    /// globe matrix and the real clipping plane, and `for_tile` still writes
    /// its real `lyon_to_tile_unit` and `tile_units_per_px` — none of which
    /// `GlobeParams::FLAT` has. It comes out identical because
    /// `shader.wgsl`'s `model.transition <= 0.0` early-out (`:734`) returns
    /// before any of them is read, and `project_line_thickness` is behind
    /// the same gate. That is a claim about the shader, so it is tested on
    /// the GPU rather than by comparing two structs.
    ///
    /// # The mesh: a split tile, so a two-pixel shift is visible
    ///
    /// The two projections are chosen to AGREE at this band — that is why
    /// the band is up here — so the disagreement is single-digit pixels at
    /// the frame corner and exactly zero at the centre. A solid tile
    /// covering the viewport would therefore render identically under both
    /// and (3) would prove nothing. `tile_covering_grid_split` puts a
    /// north–south colour boundary through the tile and the tile's own
    /// edges in the frame, both away from the centre, and pitch and bearing
    /// are non-zero so neither runs down a symmetry axis.
    ///
    /// Measured: **(2) vs (1) = 0 of 1,048,576 bytes differ; (3) vs (1) =
    /// 4,632** — about 1,158 pixels, along the moved edges. Four thousand
    /// is a small number and it is supposed to be: the whole reason the
    /// band sits at z11–z12 rather than z3–z6 is that the two projections
    /// barely disagree up here. It is still four thousand bytes more than
    /// zero, which is all the non-vacuity argument needs. If it ever
    /// collapses to zero, this test has stopped grading the switch and the
    /// mesh or the camera is what needs fixing — not the assertion.
    #[test]
    fn the_globe_switch_renders_a_banded_zoom_as_the_pure_mercator_frame() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 512u32;
            let extent = 4096u32;
            let zoom = 11.133f64;
            let camera = crate::render::Camera {
                center_lon: 13.4,
                center_lat: 52.5,
                zoom,
                pitch: 0.3,
                bearing: 0.2,
                viewport_width_px: size as f32,
                viewport_height_px: size as f32,
                fov_y: crate::render::Camera::DEFAULT_FOV_Y,
            };

            // The band, asserted rather than assumed: this test is void at
            // any zoom where the globe is already off.
            let banded = crate::render::globe::transition_for_zoom(zoom);
            assert!(
                (banded - 0.867).abs() < 5e-4,
                "z{zoom} must sit inside the projection band at ~0.867, not {banded} -- \
                 if the band moved, move this camera with it"
            );

            let frame = crate::render::globe::globe_frame(&camera);
            assert_eq!(frame.transition, banded);
            let switched_off = frame.flattened();

            let (tile_x, tile_y) =
                crate::geo::lon_lat_to_tile_xy(camera.center_lon, camera.center_lat, 11.0);
            let tile = crate::geo::TileId { z: 11, x: tile_x as u32, y: tile_y as u32 };
            let mesh = tile_covering_grid_split(
                64,
                extent,
                [1.0, 0.1, 0.1, 1.0],
                [0.1, 0.1, 1.0, 1.0],
            );
            let buffers = pipeline.create_tile_buffers(&ctx, &mesh);
            let model = crate::render::combined_tile_matrix(&camera, tile, extent as f32)
                .to_cols_array_2d();

            // A sun that shades nothing, so a difference below is the
            // PROJECTION and never the light -- the same device
            // `render::globe`'s own tests use.
            let unlit_sun = glam::DVec3::Z;
            let unlit_night = 1.0f32;
            let params = [
                GlobeParams::FLAT,
                crate::render::globe::GlobeUpload::new(
                    &switched_off,
                    &camera,
                    extent as f32,
                    unlit_sun,
                    unlit_night,
                )
                .for_tile(tile),
                crate::render::globe::GlobeUpload::new(
                    &frame,
                    &camera,
                    extent as f32,
                    unlit_sun,
                    unlit_night,
                )
                .for_tile(tile),
            ];

            let mut frames = Vec::new();
            for globe in params {
                let color_texture = make_color_texture(&ctx, size);
                let color_view =
                    color_texture.create_view(&wgpu::TextureViewDescriptor::default());
                let depth_view = pipeline.create_depth_view(&ctx, size, size);
                let camera_bind_group = pipeline.test_camera_bind_group(
                    &ctx,
                    camera.view_projection().to_cols_array_2d(),
                    zoom as f32,
                );
                let group =
                    pipeline.create_model_bind_group(&ctx, model, 1.0, 0.0, globe).1;
                let mut encoder = ctx
                    .device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
                pipeline.render(
                    &mut encoder,
                    &color_view,
                    &depth_view,
                    wgpu::Color::TRANSPARENT,
                    &camera_bind_group,
                    &[(&buffers, &group)],
                    None,
                );
                frames.push(read_all_pixels(&ctx, &color_texture, encoder, size).await);
            }
            let (mercator, off, on) = (&frames[0], &frames[1], &frames[2]);

            let differ = |a: &[u8], b: &[u8]| a.iter().zip(b.iter()).filter(|(x, y)| x != y).count();

            let off_vs_mercator = differ(mercator, off);
            assert_eq!(
                off_vs_mercator, 0,
                "{off_vs_mercator} of {} bytes differ between the pure-Mercator frame and the \
                 same frame with the globe switched off at z{zoom} -- `set_globe_enabled(false)` \
                 must be the flat path, not an approximation of it",
                mercator.len()
            );

            let on_vs_mercator = differ(mercator, on);
            assert!(
                on_vs_mercator > 1_000,
                "only {on_vs_mercator} of {} bytes differ between the pure-Mercator frame and \
                 the GLOBE-ON frame at z{zoom} (transition {banded}) -- the two renders this \
                 test compares are indistinguishable, so the equality above grades nothing",
                mercator.len()
            );

            // ...and the frames are not blank, or all three comparisons are
            // statements about empty textures.
            let lit = mercator.as_chunks::<4>().0.iter().filter(|p| p[3] > 20).count();
            assert!(lit > 1_000, "only {lit} lit pixels -- the Mercator control drew nothing");

            // THE MECHANISM, asserted after the pixels rather than before
            // them, deliberately. These are cheap and would trip first, and
            // a cheap assert that fires first is a shield: reverting
            // `flattened` would fail here and the pixel comparison above --
            // the claim this test exists for -- would never run. Ordered
            // this way, a broken switch fails on what a user would see.
            //
            // The switch zeroes the two transitions and NOTHING else: the
            // matrices and the plane survive, which is what makes the byte
            // equality above a fact about the shader's `transition <= 0.0`
            // early-out rather than about a frame blanked on the CPU.
            assert_eq!(switched_off.transition, 0.0, "the projection must be pure Mercator");
            assert_eq!(switched_off.space_transition, 0.0, "the space effects go with it");
            assert_eq!(switched_off.globe_view_proj, frame.globe_view_proj);
            assert_eq!(switched_off.globe_view, frame.globe_view);
            assert_eq!(switched_off.mercator_view_proj, frame.mercator_view_proj);
            assert_eq!(switched_off.clipping_plane, frame.clipping_plane);
            // And every structural reader downstream takes its flat arm.
            assert_eq!(
                crate::render::globe::GlobeCover::new(&switched_off),
                crate::render::globe::GlobeCover::FLAT,
                "a switched-off frame must select the flat tile cover"
            );
            assert!(!crate::render::globe::GlobeAnchors::new(&switched_off).is_globe());
        });
    }

    /// **THE ACCEPTANCE CRITERION FOR GROUND SHADING: the night side of the
    /// planet is dark, the day side is not, and the dark side is not black.**
    ///
    /// Task 9 shipped an ATMOSPHERIC terminator and said so plainly — "the
    /// planet under it is exactly as bright on the night side as on the day
    /// side, because nothing here shades the ground". This is that, fixed,
    /// and measured the way
    /// `the_glow_reads_outside_the_limb_and_does_not_wash_out_the_planet`
    /// measures the glow: one camera, one frame, two probes, and a control
    /// with the feature withheld.
    ///
    /// # The camera and the sun, chosen so the two probes are MIRRORS
    ///
    /// `space_camera(-102.869, 0.0)` — z1.5, 512x512, the analytic
    /// silhouette 161.385 px this file's other globe tests share — with a
    /// subsolar point at **lon −12.869, lat 0**, i.e. exactly 90 degrees
    /// EAST of the camera's centre and exactly on the equator.
    ///
    /// That is not a convenience. With the sun 90 degrees east of the
    /// camera's axis the geometric terminator is the meridian through the
    /// screen's centre column; with the camera at lat 0 and the sun at
    /// declination 0 the whole frame is symmetric under a mirror about that
    /// column — every tile is the same colour, every triangle the mirror of
    /// another. **So the pixel at `centre + d` and the pixel at
    /// `centre − d` differ by the LIGHT and by nothing else**, which is what
    /// makes a brightness comparison between them a measurement rather than
    /// an anecdote. A four-colour fixture, which the disc tests use to tell
    /// east from west, would have made every comparison a comparison of
    /// tiles.
    ///
    /// The subsolar longitude −12.869 is `render::solar`'s answer for the
    /// September 2024 equinox and is tied to NOAA's apparent-noon table in
    /// that module's own tests; here it is a round number to put the sun on.
    ///
    /// # Measured, Apple M-series adapter, fill `#4f7499`, night floor 0.25
    ///
    /// ```text
    ///   probe (row 256)        unlit control     shaded      ratio
    ///   +20 deg E  (x 332)     (79,116,153)   (79,116,153)   1.00
    ///   -20 deg W  (x 179)     (79,116,153)   (20,29,38)     0.25
    ///   +40 deg E  (x 388)     (79,116,153)   (79,116,153)   1.00
    ///   -40 deg W  (x 123)     (79,116,153)   (20,29,38)     0.25
    ///   +60 deg E  (x 414)     (79,116,153)   (79,116,153)   1.00
    ///   -60 deg W  (x  97)     (79,116,153)   (20,29,38)     0.25
    /// ```
    ///
    /// The three pairs sit 76/77, 132/133 and 158/159 px either side of the
    /// centre column — the mirror symmetry the camera was chosen for, off by
    /// the one pixel a `floor` of a continuous position costs.
    ///
    /// **Margin 115 byte units on the blue channel**, day against night, at
    /// the same radius from the disc's centre. The dark side is
    /// **(20, 29, 38)** and not `(0, 0, 0)`, which is the second half of the
    /// criterion and the whole point of a floor — see
    /// `render::atmosphere::Atmosphere::night_brightness`.
    ///
    /// The UNLIT control is the defect: it reads the fill colour on both
    /// sides, to the byte, which is exactly the thing Task 9 described.
    #[test]
    fn the_night_side_of_the_planet_is_dark_and_the_lit_side_is_not() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 512u32;
            let extent = 4096u32;

            // The subsolar point for a known instant, then a camera put 90
            // degrees west of it -- so the SUN is what is known and the
            // camera follows, rather than the other way round.
            const EQUINOX: f64 = 1_727_009_040.0; // 2024-09-22T12:44:00Z
            let (subsolar_lon, subsolar_lat) = crate::render::solar::subsolar_point(EQUINOX);
            assert!(
                (subsolar_lon + 12.869).abs() < 0.01 && subsolar_lat.abs() < 0.02,
                "the equinox subsolar point moved: {subsolar_lon}, {subsolar_lat}"
            );
            let camera = space_camera(subsolar_lon - 90.0, 0.0);
            assert_eq!(
                crate::render::globe::transition_for_zoom(camera.zoom),
                1.0,
                "this test's camera must be a FULL globe"
            );
            let limb = analytic_disc_radius_px(&camera);
            assert!((limb - 161.385).abs() < 0.01, "the analytic disc moved: {limb}");

            // Latitude exactly 0, not the equinox's -0.0012 degrees: the
            // mirror symmetry this test rests on has to be exact, and
            // 0.0012 degrees of declination would break it by a hair for no
            // gain. What comes from the clock is the LONGITUDE.
            let sun = crate::render::atmosphere::Sun::Map { lon_deg: subsolar_lon, lat_deg: 0.0 }
                .direction_sphere(&crate::render::globe::globe_frame(&camera).globe_view.inverse());

            let fill =
                crate::style::Rgba { r: 79.0 / 255.0, g: 116.0 / 255.0, b: 153.0 / 255.0, a: 1.0 };
            let night = crate::render::atmosphere::Atmosphere::TERMINATOR_NIGHT_BRIGHTNESS;
            assert!((night - 0.25).abs() < 1e-6, "the default night floor moved: {night}");

            let mut frames = Vec::new();
            for shaded in [false, true] {
                let (meshes, uniforms) = if shaded {
                    lit_globe_draws(&camera, 1, extent, fill, sun, night)
                } else {
                    // THE CONTROL IS THE DEFECT: the same geometry, the same
                    // camera and the same fill, with the ground unlit --
                    // which is what this crate drew before this change and
                    // what Task 9 described.
                    globe_level_draws_in(&camera, 1, extent, fill, |tile| {
                        crate::render::subdivide::Subdivision::for_tile(tile)
                    })
                };
                frames.push(
                    render_globe_level(&ctx, &pipeline, &camera, size, &meshes, &uniforms).await,
                );
            }
            let (control, shaded) = (&frames[0], &frames[1]);

            let unshaded_rgb = [79u8, 116, 153];
            let mut day_min = i32::MAX;
            let mut night_max = i32::MIN;
            for arc_deg in [20.0f64, 40.0, 60.0] {
                for (label, lon_offset) in [("east/day", arc_deg), ("west/night", -arc_deg)] {
                    let (col, row) = sphere_lon_lat_to_px(
                        &camera,
                        camera.center_lon + lon_offset,
                        0.0,
                        size,
                    );
                    let (col, row) = (col.floor() as u32, row.floor() as u32);
                    // Both probes must be ON the drawn planet, or a
                    // comparison between them is a comparison of background.
                    let alpha = |data: &[u8]| {
                        let stride = (size * 4).next_multiple_of(256);
                        data[(row * stride + col * 4 + 3) as usize]
                    };
                    assert!(
                        alpha(control) > 20 && alpha(shaded) > 20,
                        "{label} at {arc_deg} degrees lands at ({col}, {row}), where no \
                         tile drew -- the probe is off the planet"
                    );
                    assert_eq!(
                        rgb_at(control, size, col, row),
                        unshaded_rgb,
                        "THE CONTROL IS THE DEFECT and must read the fill colour on BOTH \
                         sides of the terminator: {label} at {arc_deg} degrees, pixel \
                         ({col}, {row})"
                    );
                    let value = rgb_at(shaded, size, col, row);
                    println!(
                        "  {label} {arc_deg:>4} deg -> ({col}, {row}): control \
                         {:?}, shaded {value:?}",
                        rgb_at(control, size, col, row)
                    );
                    let blue = i32::from(value[2]);
                    if lon_offset > 0.0 {
                        day_min = day_min.min(blue);
                        assert_eq!(
                            value, unshaded_rgb,
                            "the DAY side must be undimmed: {label} at {arc_deg} degrees \
                             read {value:?}"
                        );
                    } else {
                        night_max = night_max.max(blue);
                        // 3. THE DARK SIDE IS NOT BLACK. Every channel, not
                        //    just the brightest -- a floor that clipped the
                        //    red channel to zero would still satisfy a luma
                        //    test.
                        assert!(
                            value.iter().all(|c| *c > 0),
                            "the NIGHT side must be DARK, not BLACK: {label} at {arc_deg} \
                             degrees read {value:?}, and `night_brightness` is {night}"
                        );
                        // ...and it is the floor, not some other number:
                        // 0.25 x (79, 116, 153) = (19.75, 29, 38.25).
                        assert_eq!(
                            value,
                            [20u8, 29, 38],
                            "the night side must be exactly the fill times the {night} \
                             floor; {label} at {arc_deg} degrees read {value:?}"
                        );
                    }
                }
            }

            // 1 and 2, as one margin rather than two absolutes: the day side
            // is brighter than the night side at the SAME radius from the
            // disc's centre, by a stated amount. Measured 115 on the blue
            // channel (153 against 38).
            let margin = day_min - night_max;
            assert!(
                margin >= 100,
                "the day side must beat the night side by at least 100 byte units of blue \
                 at the same radius; the worst pair differed by {margin} ({day_min} \
                 against {night_max})"
            );

            // And the disc is still a disc: the shading multiplies COLOUR,
            // so it must not change one pixel's coverage. Exact, not a
            // tolerance -- alpha is untouched by construction, `fs_main`
            // returning the same `alpha` it always did.
            let stride = (size * 4).next_multiple_of(256) as usize;
            let coverage = |data: &[u8]| -> Vec<bool> {
                (0..size as usize)
                    .flat_map(|row| (0..size as usize).map(move |col| (col, row)))
                    .map(|(col, row)| data[row * stride + col * 4 + 3] > 20)
                    .collect()
            };
            let (a, b) = (coverage(control), coverage(shaded));
            let moved = a.iter().zip(b.iter()).filter(|(x, y)| x != y).count();
            assert_eq!(
                moved, 0,
                "{moved} pixels changed COVERAGE between the unlit and shaded frames -- \
                 ground shading multiplies rgb and must leave alpha alone"
            );
            let drawn = a.iter().filter(|x| **x).count();
            assert!(
                drawn > 50_000,
                "only {drawn} pixels of planet were drawn, against the 81,823 px the \
                 analytic disc predicts -- there is not enough planet here to light"
            );
        });
    }

    /// `shader.wgsl` with the ground shading DELETED, for the control arm of
    /// the two tests below.
    ///
    /// One substitution, on the gate rather than on the block: `fs_main`'s
    /// `if (input.globe_light.y > 0.0)` becomes `if (false)`, so the shading
    /// is unreachable and everything else about the shader — the varying,
    /// the vertex-stage `normalize` and `dot`, the uniform's layout, every
    /// other branch — is byte-identical. That is what makes the arms
    /// comparable: the difference between them is the four lines and nothing
    /// else.
    ///
    /// It therefore isolates the FRAGMENT half of the cost and not the
    /// vertex half, and that is deliberate as well as convenient. The vertex
    /// half is one `inversesqrt` and one `dot` on a branch that already pays
    /// for an `exp`, two divisions and a `mat4` multiply; the fragment half
    /// runs once per covered pixel, which at z3 is most of the viewport.
    fn shader_without_ground_shading() -> String {
        let source = include_str!("shader.wgsl");
        const GATE: &str = "if (input.globe_light.y > 0.0) {";
        assert_eq!(
            source.matches(GATE).count(),
            1,
            "shader.wgsl no longer contains exactly one ground-shading gate spelled \
             `{GATE}` -- the two tests that render a control arm are silently grading \
             the shipped shader against itself"
        );
        source.replace(GATE, "if (false) {")
    }

    /// **The four lines in `fs_main` are what darkens the night side**, and
    /// this is the sabotage check made permanent rather than run once.
    ///
    /// Task 5's `granularity 4` arm is the precedent: an acceptance test
    /// whose "before" is not the defect grades nothing, so the defect is
    /// rendered alongside the fix, from the same geometry and the same
    /// camera, and asserted to BE the defect. Here the control is the
    /// shipped shader with one gate flipped to `false` — see
    /// `shader_without_ground_shading` — which is exactly the state this
    /// crate was in when Task 9 wrote "the planet under it is exactly as
    /// bright on the night side as on the day side".
    ///
    /// Measured: the night probe reads **(20, 29, 38)** through the shipped
    /// shader and **(79, 116, 153)** through the stripped one, at the same
    /// pixel, from the same mesh, with the same uniform.
    #[test]
    fn the_ground_shading_is_what_darkens_the_night_side() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let size = 512u32;
            let extent = 4096u32;
            const EQUINOX: f64 = 1_727_009_040.0;
            let (subsolar_lon, _) = crate::render::solar::subsolar_point(EQUINOX);
            let camera = space_camera(subsolar_lon - 90.0, 0.0);
            let sun = crate::render::atmosphere::Sun::Map { lon_deg: subsolar_lon, lat_deg: 0.0 }
                .direction_sphere(&crate::render::globe::globe_frame(&camera).globe_view.inverse());
            let fill =
                crate::style::Rgba { r: 79.0 / 255.0, g: 116.0 / 255.0, b: 153.0 / 255.0, a: 1.0 };
            let night = crate::render::atmosphere::Atmosphere::TERMINATOR_NIGHT_BRIGHTNESS;

            // The probe: 60 degrees WEST of the camera's centre, which with
            // the sun 90 degrees east is deep on the night side.
            let (col, row) = sphere_lon_lat_to_px(&camera, camera.center_lon - 60.0, 0.0, size);
            let (col, row) = (col.floor() as u32, row.floor() as u32);

            let stripped = shader_without_ground_shading();
            let mut readings = Vec::new();
            for source in [include_str!("shader.wgsl"), stripped.as_str()] {
                let pipeline =
                    TilePipeline::from_source(&ctx, wgpu::TextureFormat::Rgba8Unorm, source);
                let (meshes, uniforms) = lit_globe_draws(&camera, 1, extent, fill, sun, night);
                let data =
                    render_globe_level(&ctx, &pipeline, &camera, size, &meshes, &uniforms).await;
                readings.push(rgb_at(&data, size, col, row));
            }
            let (shipped, control) = (readings[0], readings[1]);
            assert_eq!(
                control,
                [79u8, 116, 153],
                "WITH THE SHADING DELETED the night side must read the fill colour -- if it \
                 does not, the control is not the defect and the comparison below grades \
                 nothing"
            );
            assert_eq!(
                shipped,
                [20u8, 29, 38],
                "WITH THE SHADING IN the night side must read the fill times the {night} \
                 floor; it read {shipped:?} where the control read {control:?}"
            );
        });
    }

    /// **What the ground shading costs, against the plan's 0.5 ms rule.**
    ///
    /// `paint()` ends at `queue.submit`, so a fragment-stage cost cannot be
    /// a `PaintPhase` number — the same reason
    /// `the_full_screen_atmosphere_costs_less_than_the_plans_half_millisecond`
    /// exists and the same harness: submit N frames, wait ONCE at the end of
    /// the batch (waiting per frame timed the round trip of a submit and read
    /// 4x high in Task 9's first attempt), take the median of five batches.
    ///
    /// **The A/B is two shader sources**, not two uniforms, because ground
    /// shading is not a draw that can be withheld — there is no
    /// `night_brightness` that makes it cost nothing, only ones that make it
    /// look like nothing. See `shader_without_ground_shading`.
    ///
    /// The camera is z3 at 1024x768, the default example window: a 758 px
    /// disc, 64 z3 tiles (the whole level, which is four times what the
    /// globe cover at that camera actually asks for — 16 tiles, measured in
    /// Task 7 — so the figure is an upper bound rather than a typical one).
    ///
    /// # Measured, Apple M-series adapter, RELEASE, median of five batches
    ///
    /// **The cost is not resolvable by this harness, and that is the
    /// finding.** Four runs of the test, each a median of five 40-frame
    /// batches per arm, interleaved:
    ///
    /// ```text
    ///   run   without     with      delta of medians
    ///    1   0.0517 ms  0.0555 ms      +0.0038 ms
    ///    2   0.0547 ms  0.0543 ms      -0.0004 ms
    ///    3   0.0519 ms  0.0539 ms      +0.0020 ms
    ///    4   0.0745 ms  0.0601 ms      -0.0144 ms
    /// ```
    ///
    /// **The sign is not stable**, the per-run spreads overlap almost
    /// completely (0.048-0.098 ms in both arms), and the whole 64-tile frame
    /// is about 0.05 ms. So the honest statement is that a smoothstep, two
    /// `mix`es and a `vec3` multiply per covered fragment is below this
    /// harness's noise floor — bounded by the ±0.014 ms it CAN see, which is
    /// 35x under the plan's 0.5 ms ceiling, and not claimed to be any
    /// smaller number than that.
    ///
    /// The assertion is therefore the ceiling and NOT a floor. A floor
    /// invented to look rigorous would fail on a quiet machine, and two of
    /// the four runs above would already have failed one. What proves the
    /// work happens at all is
    /// `the_ground_shading_is_what_darkens_the_night_side`, which reads it
    /// as a pixel rather than as a microsecond.
    ///
    /// The first version of this measurement ran one arm to completion and
    /// then the other, and read the stripped arm **0.0145 ms slower** in
    /// release — whichever arm goes first pays for whatever the driver warms
    /// up once. The batches are interleaved now.
    #[test]
    #[cfg_attr(not(target_os = "macos"), ignore = "the budget is for a hardware GPU; Linux CI renders in software (lavapipe)")]
    fn the_ground_shading_costs_less_than_the_plans_half_millisecond() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let (width, height) = (1024u32, 768u32);
            let extent = 4096u32;
            let camera = crate::render::Camera {
                center_lon: 0.0,
                center_lat: 0.0,
                zoom: 3.0,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: width as f32,
                viewport_height_px: height as f32,
                fov_y: crate::render::Camera::DEFAULT_FOV_Y,
            };
            assert_eq!(
                crate::render::globe::transition_for_zoom(camera.zoom),
                1.0,
                "z3 must be a full globe, or the shading would be faded and the figure a \
                 fraction of the real one"
            );
            let sun = crate::render::atmosphere::Sun::at_utc(1_727_009_040.0)
                .direction_sphere(&crate::render::globe::globe_frame(&camera).globe_view.inverse());
            let fill =
                crate::style::Rgba { r: 79.0 / 255.0, g: 116.0 / 255.0, b: 153.0 / 255.0, a: 1.0 };

            let color_texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("ground-shading-cost-color"),
                size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            });
            let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor::default());

            let stripped = shader_without_ground_shading();
            let before = crate::render::allocation_stats().draw_calls_total;
            // BOTH arms built up front, then the batches INTERLEAVED. The
            // first version ran one arm to completion and then the other,
            // and read the stripped arm 0.0145 ms SLOWER in release --
            // negative delta, because whichever arm goes first pays for
            // whatever the driver warms up once. Alternating puts that cost
            // on both.
            let arms: Vec<_> = [stripped.as_str(), include_str!("shader.wgsl")]
                .into_iter()
                .map(|source| {
                    let pipeline =
                        TilePipeline::from_source(&ctx, wgpu::TextureFormat::Rgba8Unorm, source);
                    let depth_view = pipeline.create_depth_view(&ctx, width, height);
                    let camera_bind_group = pipeline.test_camera_bind_group(
                        &ctx,
                        camera.view_projection().to_cols_array_2d(),
                        camera.zoom as f32,
                    );
                    let (meshes, uniforms) = lit_globe_draws(
                        &camera,
                        3,
                        extent,
                        fill,
                        sun,
                        crate::render::atmosphere::Atmosphere::TERMINATOR_NIGHT_BRIGHTNESS,
                    );
                    let buffers: Vec<TileBuffers> =
                        meshes.iter().map(|m| pipeline.create_tile_buffers(&ctx, m)).collect();
                    let groups: Vec<wgpu::BindGroup> = uniforms
                        .iter()
                        .map(|(matrix, globe)| {
                            pipeline.create_model_bind_group(&ctx, *matrix, 1.0, 0.0, *globe).1
                        })
                        .collect();
                    (pipeline, depth_view, camera_bind_group, buffers, groups)
                })
                .collect();

            let one_batch = |arm: usize, frames: u32| {
                let (pipeline, depth_view, camera_bind_group, buffers, groups) = &arms[arm];
                let draws: Vec<(&TileBuffers, &wgpu::BindGroup)> =
                    buffers.iter().zip(groups.iter()).collect();
                let start = crate::timing::Instant::now();
                for _ in 0..frames {
                    let mut encoder = ctx
                        .device
                        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
                    pipeline.render(
                        &mut encoder,
                        &color_view,
                        depth_view,
                        wgpu::Color::TRANSPARENT,
                        camera_bind_group,
                        &draws,
                        None,
                    );
                    ctx.queue.submit(Some(encoder.finish()));
                }
                // ONE wait per batch, not per frame -- Task 9's finding: a
                // wait per frame timed the round trip of a submit and read
                // four times high.
                let _ = ctx.device.poll(wgpu::PollType::wait_indefinitely());
                start.elapsed().as_secs_f64() * 1000.0 / f64::from(frames)
            };
            // Warm-up, both arms: the first submit on each compiles and
            // uploads, and the first `poll(Wait)` pays for a pipeline state
            // object.
            one_batch(0, 10);
            one_batch(1, 10);
            let mut timings = vec![Vec::new(), Vec::new()];
            for _ in 0..5 {
                timings[0].push(one_batch(0, 40));
                timings[1].push(one_batch(1, 40));
            }
            for batches in &mut timings {
                batches.sort_by(f64::total_cmp);
            }

            // GATE ON PROOF OF PAINT, the plan's standing rule.
            let issued = crate::render::allocation_stats().draw_calls_total - before;
            assert!(issued > 0, "no draw call was issued, so there is nothing to have timed");

            let (without, with) = (&timings[0], &timings[1]);
            let delta = with[2] - without[2];
            println!(
                "ground shading at z3, {width}x{height}, 64 z3 tiles: without {:.4} ms \
                 ({:.4}-{:.4}), with {:.4} ms ({:.4}-{:.4}), delta of medians {delta:.4} ms, \
                 {issued} draw calls",
                without[2],
                without[0],
                without[4],
                with[2],
                with[0],
                with[4],
            );
            assert!(
                delta < 0.5,
                "THE PLAN'S 0.5 ms RULE: ground shading costs {delta:.4} ms a frame at z3 \
                 in a {width}x{height} viewport over 64 tiles, against medians of {:.4} \
                 and {:.4} ms. Stop and report rather than shipping it",
                without[2],
                with[2],
            );
            // Both arms must have drawn a planet, or two empty passes would
            // differ by nothing and pass the ceiling.
            assert!(
                with[2] > 0.0 && without[2] > 0.0,
                "one of the two arms took no measurable time at all: {:.4} and {:.4} ms",
                without[2],
                with[2]
            );
        });
    }

    /// **THE LIT HEMISPHERE IS THE COLOUR THE STYLE AUTHORED IT, TO THE
    /// BYTE.** Not "brighter than the night side" — *identical to no
    /// shading at all*.
    ///
    /// # Why this test exists separately from the margin test
    ///
    /// `the_night_side_of_the_planet_is_dark_and_the_lit_side_is_not`
    /// compares a lit probe against a dark probe, and **a test shaped like
    /// that passes a globe that is uniformly too dark**. It was reported
    /// from a side-by-side against Mapbox's globe — "our looks a bit
    /// shaded" — and it is a different claim from the terminator being
    /// right: a correct day/night terminator and an undimmed day side are
    /// two properties, and it would be easy to ship the first while
    /// believing the second.
    ///
    /// So: the camera looks straight down at the SUBSOLAR POINT. Every
    /// visible point is then within the horizon's arc of the sub-solar
    /// point, which at this zoom is well inside the 87 degrees at which
    /// `smoothstep(-sin 3deg, +sin 3deg, cos)` saturates — so the whole disc
    /// is fully lit and **every drawn pixel must be bit-identical to the
    /// unshaded control.** Zero, not a tolerance.
    ///
    /// # Why it is exactly zero rather than nearly zero
    ///
    /// `smoothstep` returns exactly `1.0` above its upper edge, `mix(a, 1.0,
    /// 1.0)` is exactly `1.0`, `mix(1.0, 1.0, t)` is exactly `1.0` for any
    /// `t`, and `rgb * 1.0` is `rgb`. The lit side is untouched by
    /// construction, not by rounding — which is the property worth pinning,
    /// because it is what makes ground shading safe to have on by default
    /// over somebody else's style.
    ///
    /// **This says nothing about the ATMOSPHERE.** The limb glow is a
    /// separate pass and it does put haze in front of the planet — Task 9
    /// measured the disc centre moving 12 byte units and bounded that as
    /// desirable. This test renders no atmosphere, so what it isolates is
    /// the ground shading's own contribution to "looks a bit shaded", and
    /// that contribution is zero.
    #[test]
    fn the_lit_hemisphere_is_the_colour_the_style_authored() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 512u32;
            let extent = 4096u32;

            const EQUINOX: f64 = 1_727_009_040.0; // 2024-09-22T12:44:00Z
            let (subsolar_lon, _) = crate::render::solar::subsolar_point(EQUINOX);
            // THE CAMERA IS ON THE SUN'S OWN MERIDIAN, so the sub-solar
            // point is the screen centre and the whole visible cap is day.
            let camera = space_camera(subsolar_lon, 0.0);
            assert_eq!(
                crate::render::globe::transition_for_zoom(camera.zoom),
                1.0,
                "this test's camera must be a FULL globe, or the shading would be faded \
                 and `identical` would be a statement about a transition rather than \
                 about the light"
            );
            let sun = crate::render::atmosphere::Sun::Map { lon_deg: subsolar_lon, lat_deg: 0.0 }
                .direction_sphere(&crate::render::globe::globe_frame(&camera).globe_view.inverse());
            // The sun really is behind the camera: the direction toward it
            // and the direction toward the eye are the same ray, so the
            // whole disc faces it.
            let eye = crate::render::globe::globe_frame(&camera)
                .globe_view
                .inverse()
                .transform_point3(glam::DVec3::ZERO)
                .normalize();
            assert!(
                sun.dot(eye) > 0.999,
                "the sun must be behind the camera for this test to be about a fully LIT \
                 disc; it is {:.4} degrees off the eye direction",
                sun.dot(eye).clamp(-1.0, 1.0).acos().to_degrees()
            );

            let fill =
                crate::style::Rgba { r: 79.0 / 255.0, g: 116.0 / 255.0, b: 153.0 / 255.0, a: 1.0 };
            let night = crate::render::atmosphere::Atmosphere::TERMINATOR_NIGHT_BRIGHTNESS;
            assert!(
                night < 0.99,
                "the night floor is {night}, which is `unshaded` -- this test would pass \
                 with the whole feature turned off"
            );

            let mut frames = Vec::new();
            for shaded in [false, true] {
                let (meshes, uniforms) = if shaded {
                    lit_globe_draws(&camera, 1, extent, fill, sun, night)
                } else {
                    globe_level_draws_in(&camera, 1, extent, fill, |tile| {
                        crate::render::subdivide::Subdivision::for_tile(tile)
                    })
                };
                frames.push(
                    render_globe_level(&ctx, &pipeline, &camera, size, &meshes, &uniforms).await,
                );
            }
            let (control, shaded) = (&frames[0], &frames[1]);

            // THE ASSERTION: the sub-solar point itself, exactly the style's
            // own colour. `assert_eq!` on the byte triple, not a tolerance
            // -- see this test's doc comment for why exact is available.
            let centre = size / 2;
            assert_eq!(
                rgb_at(shaded, size, centre, centre),
                [79u8, 116, 153],
                "THE SUB-SOLAR POINT MUST BE THE COLOUR THE STYLE AUTHORED. It read {:?} \
                 against the fill's (79, 116, 153) -- a lit side dimmed by anything at all \
                 is the defect a lit-versus-dark margin test cannot see",
                rgb_at(shaded, size, centre, centre)
            );

            // ...and so must every other drawn pixel, because every one of
            // them is inside the arc at which the smoothstep saturates.
            let stride = (size * 4).next_multiple_of(256) as usize;
            let mut drawn = 0usize;
            let mut dimmed = 0usize;
            let mut worst = 0i32;
            let mut worst_at = (0u32, 0u32);
            for row in 0..size {
                for col in 0..size {
                    if shaded[row as usize * stride + col as usize * 4 + 3] <= 20 {
                        continue;
                    }
                    drawn += 1;
                    let (a, b) =
                        (rgb_at(control, size, col, row), rgb_at(shaded, size, col, row));
                    if a != b {
                        dimmed += 1;
                        let gap = rgb_gap(a, b);
                        if gap > worst {
                            worst = gap;
                            worst_at = (col, row);
                        }
                    }
                }
            }
            println!(
                "lit hemisphere: {dimmed} of {drawn} drawn pixels differ from the unshaded \
                 control, worst gap {worst} at {worst_at:?}"
            );
            assert!(
                drawn > 50_000,
                "only {drawn} pixels of planet were drawn, against the 81,823 px the \
                 analytic disc predicts -- there is not enough lit planet here to grade"
            );
            assert_eq!(
                dimmed, 0,
                "{dimmed} of {drawn} drawn pixels are dimmer than the style authored them, \
                 the worst by {worst} byte units at {worst_at:?}. GROUND SHADING MUST NOT \
                 TOUCH THE LIT SIDE"
            );
        });
    }

    /// **THE SECOND HALF OF THE ACCEPTANCE CRITERION: move the clock six
    /// hours and the terminator moves across the screen.**
    ///
    /// The camera does not move, the geometry does not move, the tile colour
    /// does not move. The only input that changes is the UTC timestamp handed
    /// to `render::atmosphere::Sun::at_utc`, and what has to come out is a
    /// terminator in a different place — the rule "the sunlight on the
    /// planet is true to the current time" stated as a pixel.
    ///
    /// # The camera, chosen so BOTH terminators are on the disc
    ///
    /// The subsolar point walks 90 degrees west in six hours, so the camera
    /// is centred 45 degrees west of the sun at `T`: relative sun longitude
    /// `+45` then, `−45` six hours later, and the geometric terminator
    /// therefore at relative `−45` and at relative `+45` — one either side
    /// of the centre column, symmetric, and both comfortably inside the
    /// 70.0-degree visible cap at this zoom.
    ///
    /// A camera that put the sun 90 degrees east instead (the one
    /// `the_night_side_of_the_planet_is_dark_and_the_lit_side_is_not` uses)
    /// would be the wrong instrument here: six hours later the subsolar
    /// point would be ON the camera's axis and the whole disc lit, so there
    /// would be no terminator left to have moved.
    ///
    /// # Measured, Apple M-series adapter
    ///
    /// ```text
    ///   timestamp    subsolar lon   terminator column   probe west    probe east
    ///   T                -12.869           114.5        (20,29,38)   (79,116,153)
    ///   T + 6 h         -102.869           396.5       (79,116,153)   (20,29,38)
    /// ```
    ///
    /// **282.0 px of movement** on a 512 px viewport, symmetric about the
    /// centre column to **1.0 px** (141.5 px west of it against 140.5 px
    /// east), and the two probes swap outright. The columns are found by
    /// scanning the centre row for the half-way point between the lit and
    /// night values, not predicted — a prediction would share the projection
    /// with the thing it is grading.
    #[test]
    fn the_terminator_moves_across_the_screen_when_the_clock_does() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 512u32;
            let extent = 4096u32;

            const EQUINOX: f64 = 1_727_009_040.0; // 2024-09-22T12:44:00Z
            const SIX_HOURS: f64 = 6.0 * 3_600.0;
            let (subsolar_lon, _) = crate::render::solar::subsolar_point(EQUINOX);
            let camera = space_camera(subsolar_lon - 45.0, 0.0);
            let inverse_view =
                crate::render::globe::globe_frame(&camera).globe_view.inverse();
            let fill =
                crate::style::Rgba { r: 79.0 / 255.0, g: 116.0 / 255.0, b: 153.0 / 255.0, a: 1.0 };
            let night = crate::render::atmosphere::Atmosphere::TERMINATOR_NIGHT_BRIGHTNESS;

            // The two probes, at relative longitude -45 and +45: exactly
            // where each frame's terminator falls, so each probe is deep on
            // the night side in one frame and deep on the day side in the
            // other. Positions from the pinhole projection, not guessed.
            let probe = |lon_offset: f64| {
                let (col, row) =
                    sphere_lon_lat_to_px(&camera, camera.center_lon + lon_offset, 0.0, size);
                (col.floor() as u32, row.floor() as u32)
            };
            let (west, east) = (probe(-60.0), probe(60.0));

            let mut readings = Vec::new();
            for offset in [0.0, SIX_HOURS] {
                let sun = crate::render::atmosphere::Sun::at_utc(EQUINOX + offset)
                    .direction_sphere(&inverse_view);
                let (meshes, uniforms) =
                    lit_globe_draws(&camera, 1, extent, fill, sun, night);
                let data =
                    render_globe_level(&ctx, &pipeline, &camera, size, &meshes, &uniforms).await;
                // The terminator's screen column, on the centre row: the one
                // place where blue crosses half way between the night value
                // and the day value, BETWEEN TWO DRAWN PIXELS.
                //
                // Not "the first column above the midpoint scanning east",
                // which is the first version of this and was wrong: in the
                // frame whose west side is LIT, that finds the west limb
                // rather than the terminator, and the measured movement came
                // out 46 px instead of the 229 the geometry predicts. A
                // change of state between two adjacent DRAWN pixels cannot
                // be the limb, because the limb is where drawn meets
                // undrawn.
                let row = size / 2;
                let day = 153i32;
                let midpoint = f64::from(day) * f64::from(1.0 + night) / 2.0;
                let stride = (size * 4).next_multiple_of(256) as usize;
                let mut crossings = Vec::new();
                let mut previous: Option<(u32, bool)> = None;
                for col in 0..size {
                    if data[row as usize * stride + col as usize * 4 + 3] <= 20 {
                        continue;
                    }
                    let is_lit = f64::from(rgb_at(&data, size, col, row)[2]) >= midpoint;
                    if let Some((before, was_lit)) = previous {
                        if was_lit != is_lit && col == before + 1 {
                            crossings.push(f64::from(col) - 0.5);
                        }
                    }
                    previous = Some((col, is_lit));
                }
                assert_eq!(
                    crossings.len(),
                    1,
                    "the centre row must cross the terminator exactly once -- the sun is \
                     45 degrees off the camera's axis, so one of the two meridians where \
                     `dot(normal, sun)` is zero is on the visible cap and the other is \
                     not. Found {crossings:?}"
                );
                let crossing = crossings[0];
                readings.push((
                    crossing,
                    rgb_at(&data, size, west.0, west.1),
                    rgb_at(&data, size, east.0, east.1),
                ));
            }
            let (first_crossing, first_west, first_east) = readings[0];
            let (later_crossing, later_west, later_east) = readings[1];

            assert!(
                first_crossing.is_finite() && later_crossing.is_finite(),
                "one of the two frames has no terminator on its centre row at all: \
                 {first_crossing}, {later_crossing} -- so there is nothing to have moved"
            );

            // THE MOVEMENT. 229 px measured; the assertion is deliberately
            // far looser than that, because what is under test is that the
            // clock reaches the shader at all, not the projection's exact
            // scale (`the_night_side_of_the_planet_is_dark_and_the_lit_side_is_not`
            // grades the geometry).
            let moved = later_crossing - first_crossing;
            println!(
                "terminator column: T {first_crossing:.1}, T+6h {later_crossing:.1}, \
                 moved {moved:.1} px; probes west {first_west:?} -> {later_west:?}, east \
                 {first_east:?} -> {later_east:?}"
            );
            assert!(
                moved > 100.0,
                "six hours of clock must move the terminator EAST across the screen; it \
                 moved {moved:.1} px, from column {first_crossing:.1} to {later_crossing:.1}"
            );
            // Symmetric about the centre column, which is the sign check:
            // the two subsolar longitudes are +45 and -45 relative to this
            // camera, so a terminator that moved the wrong way, or by the
            // wrong amount, lands off-centre.
            let centre = f64::from(size) / 2.0;
            let skew = (centre - first_crossing) - (later_crossing - centre);
            assert!(
                skew.abs() < 8.0,
                "the two terminators must straddle the centre column symmetrically -- the \
                 sun is 45 degrees east at T and 45 degrees west at T+6h. Columns \
                 {first_crossing:.1} and {later_crossing:.1} about {centre:.1}, skew \
                 {skew:.1} px"
            );

            // And the two probes SWAP, which is the same finding read as a
            // pixel rather than as a column. Stated as inequalities on blue
            // so a change in the night floor does not re-grade the test.
            assert!(
                i32::from(first_west[2]) + 60 < i32::from(first_east[2]),
                "at T the sun is 45 degrees EAST, so the west probe must be much darker \
                 than the east one: {first_west:?} against {first_east:?}"
            );
            assert!(
                i32::from(later_east[2]) + 60 < i32::from(later_west[2]),
                "six hours later the sun is 45 degrees WEST, so the two must have swapped: \
                 west {later_west:?}, east {later_east:?}"
            );
        });
    }

    /// **A flat frame is pixel-identical however the sun is written.**
    ///
    /// Tasks 3, 5, 7, 8 and 9 each preserved a bit-identical flat path and
    /// each has a test for it; Task 8 measured 0 of 1,048,576 bytes and Task
    /// 9 the same. Ground shading lands closer to that path than any of
    /// them, because it is not a separate draw at all — it is four lines
    /// inside the fragment shader that draws every flat tile on every flat
    /// frame. So the assertion is byte equality of the whole readback across
    /// **THREE** renders of the SAME flat z14 frame:
    ///
    ///  - **A** — the shipped shader with `GlobeParams::FLAT`, which is what
    ///    `MapView::paint` writes above `PROJECTION_BLEND_ZOOM_MAX`: sun zero, night
    ///    floor zero;
    ///  - **B** — the shipped shader with a **full globe frame's** sun
    ///    direction and a night floor of **1.0**, `transition` still 0.0;
    ///  - **C** — the same frame through a shader with the ground shading
    ///    **deleted** (`shader_without_ground_shading`).
    ///
    /// # Three arms, because two of them measured NOTHING
    ///
    /// The first version of this test was A against a B carrying a night
    /// floor of **0.0**, on the argument that 1.0 is the "unshaded" value
    /// and would make a leaked multiply a no-op. **Sabotage showed the
    /// argument was backwards and the test graded nothing:**
    ///
    /// ```text
    ///   sabotage                                   two-arm test   three-arm test
    ///   `vs_main`'s flat early-out writes
    ///     `globe_light = (0, 1, night)` instead
    ///     of `(0, 0, 0)`                                    -      377,661 bytes
    ///   the `globe_light.y > 0.0` gate loosened
    ///     to `>=`                                    0 bytes      caught by arm C
    ///   `mix(1.0, shade, .y)` removed from `fs_main`  0 bytes        0 bytes
    /// ```
    ///
    /// The first two zeroes were zero because `vs_main`'s flat early-out
    /// writes `globe_light = vec3(0.0)`, so `globe_light.z` — the night
    /// floor the fragment stage reads — is **zero whatever the uniform
    /// says**, and both arms shaded identically. The uniform cannot reach
    /// the fragment stage on a flat frame at all; that is the guarantee, and
    /// it is also what made a uniform-only A/B blind.
    ///
    /// So B carries **1.0** now, and **C** was added. The top row is the
    /// realistic regression — the flat early-out stops zeroing the varying —
    /// and it moves **377,661 of 1,048,576 bytes**.
    ///
    /// **The bottom row is still zero, and that is honest rather than a
    /// gap.** With the `if` guard intact, removing the transition `mix`
    /// changes nothing on a flat frame, because the guard already stopped
    /// the body: the flat path has TWO gates and either one alone suffices.
    /// The same shape Task 8 recorded for `space_clear_color`'s guard —
    /// `mix(a, b, 0.0)` is bitwise `a` — written down instead of dressed up.
    ///
    /// And per Task 8's warning — a flat-path test proves nothing if its own
    /// fixture is the same colour as the thing that should not have drawn —
    /// the fixture is asserted BRIGHT. A multiply toward black on a black
    /// fill changes nothing. Measured: **125,887 of 262,144 pixels** drawn
    /// and bright, and **0 bytes** differing across all three arms.
    #[test]
    fn a_flat_frame_is_pixel_identical_however_the_sun_is_written() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 512u32;
            let extent = 4096u32;
            // Above PROJECTION_BLEND_ZOOM_MAX, so `transition_for_zoom` is
            // exactly 0.0.
            let camera = crate::render::Camera {
                center_lon: 139.7671,
                center_lat: 35.6812,
                zoom: 14.0,
                pitch: 0.3,
                bearing: 0.2,
                viewport_width_px: size as f32,
                viewport_height_px: size as f32,
                fov_y: crate::render::Camera::DEFAULT_FOV_Y,
            };
            assert_eq!(
                crate::render::globe::transition_for_zoom(camera.zoom),
                0.0,
                "this test's camera must be a FLAT one"
            );

            // A BRIGHT fill, and asserted bright below: the leak this test
            // is looking for is a multiply toward black, which a dark
            // fixture would hide. Task 8's own finding, in its own words:
            // "the flat identity test proved nothing twice".
            let fill = crate::style::Rgba { r: 0.95, g: 0.85, b: 0.55, a: 1.0 };
            let (tile_x, tile_y) =
                crate::geo::lon_lat_to_tile_xy(camera.center_lon, camera.center_lat, 14.0);
            let tile = crate::geo::TileId { z: 14, x: tile_x as u32, y: tile_y as u32 };
            let mesh = crate::render::tessellate_plan(
                &crate::render::space::backdrop_plan(extent, fill, fill),
                extent,
                crate::render::mesh::COORD_SCALE,
                crate::render::subdivide::Subdivision::for_tile(tile),
            );
            let matrix =
                crate::render::combined_tile_matrix(&camera, tile, extent as f32).to_cols_array_2d();

            // The sun from a FULL-globe frame, so the direction in the
            // second arm is a real one rather than a zero that could not
            // shade anything anyway.
            let globe_camera = crate::render::Camera { zoom: 2.0, ..camera };
            let globe_frame = crate::render::globe::globe_frame(&globe_camera);
            assert_eq!(
                globe_frame.transition, 1.0,
                "the sun this test writes into a flat uniform must come from a FULL-globe \
                 frame, or a leaked shading would have nothing to shade with"
            );
            let sun = crate::render::atmosphere::Sun::VIEWPORT_DEFAULT
                .direction_sphere(&globe_frame.globe_view.inverse());
            let loaded = GlobeParams {
                sun_direction: [sun.x as f32, sun.y as f32, sun.z as f32],
                // 1.0, and the doc comment above is the argument: 1.0 is
                // "unshaded", so under a regression that shades a flat
                // frame this arm stays bright while arm A -- carrying
                // `FLAT`'s 0.0 -- goes dark. 0.0 here made the two arms
                // shade identically and the test grade nothing, which
                // sabotage found.
                night_brightness: 1.0,
                ..GlobeParams::FLAT
            };
            assert_eq!(
                loaded.transition, 0.0,
                "the loaded arm must still be a FLAT uniform -- only the sun fields differ"
            );
            let stripped = shader_without_ground_shading();
            let stripped_pipeline =
                TilePipeline::from_source(&ctx, wgpu::TextureFormat::Rgba8Unorm, &stripped);

            let mut frames = Vec::new();
            for (pipeline, globe) in [
                (&pipeline, GlobeParams::FLAT),
                (&pipeline, loaded),
                // ARM C: the feature removed outright. Any regression that
                // shades a flat frame moves this one, including the two that
                // arms A and B cannot see between them.
                (&stripped_pipeline, GlobeParams::FLAT),
            ] {
                let buffers = pipeline.create_tile_buffers(&ctx, &mesh);
                let group =
                    pipeline.create_model_bind_group(&ctx, matrix, 1.0, 0.0, globe).1;
                let color_texture = make_color_texture(&ctx, size);
                let color_view =
                    color_texture.create_view(&wgpu::TextureViewDescriptor::default());
                let depth_view = pipeline.create_depth_view(&ctx, size, size);
                let camera_bind_group = pipeline.test_camera_bind_group(
                    &ctx,
                    camera.view_projection().to_cols_array_2d(),
                    camera.zoom as f32,
                );
                let mut encoder =
                    ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
                pipeline.render(
                    &mut encoder,
                    &color_view,
                    &depth_view,
                    wgpu::Color::TRANSPARENT,
                    &camera_bind_group,
                    &[(&buffers, &group)],
                    None,
                );
                frames.push(read_all_pixels(&ctx, &color_texture, encoder, size).await);
            }

            let differ = |a: &[u8], b: &[u8]| a.iter().zip(b.iter()).filter(|(x, y)| x != y).count();
            let against_loaded = differ(&frames[0], &frames[1]);
            let against_stripped = differ(&frames[0], &frames[2]);
            assert_eq!(
                against_loaded, 0,
                "{against_loaded} of {} bytes differ between a flat frame drawn with \
                 `GlobeParams::FLAT` and the same frame drawn with a real sun and a night \
                 floor of 1.0 written into it. THE FLAT MAP MUST BE UNTOUCHED",
                frames[0].len()
            );
            assert_eq!(
                against_stripped, 0,
                "{against_stripped} of {} bytes differ between a flat frame drawn by the \
                 shipped shader and the same frame drawn by a shader with the ground \
                 shading DELETED. THE FLAT MAP MUST BE UNTOUCHED",
                frames[0].len()
            );

            // ...and the fixture is bright enough for the leak to have shown.
            // A 0.5x multiply on anything under 2 byte units is invisible.
            let bright = frames[0]
                .as_chunks::<4>().0.iter()
                .filter(|p| p[3] > 20 && (i32::from(p[0]) + i32::from(p[1]) + i32::from(p[2])) > 90)
                .count();
            println!(
                "flat identity: {against_loaded} bytes vs the loaded uniform and \
                 {against_stripped} vs the stripped shader, of {} each; {bright} of {} \
                 pixels drawn and bright",
                frames[0].len(),
                size * size
            );
            assert!(
                bright > 100_000,
                "only {bright} pixels are both drawn and bright, so a shading leak toward \
                 black would have had almost nowhere to appear and the equality above \
                 proves less than it looks like -- Task 8's finding, applied"
            );
        });
    }

    /// Two stroke quads across the same tile, each spanning `x0..x1` at its
    /// own in-tile `y`, with unit normals across the line so the vertex
    /// shader does the extruding.
    ///
    /// Hand-built rather than tessellated, because what is under test is the
    /// EXTRUSION, and the tessellator's own choices -- joins, caps,
    /// advancement -- would only add terms to the measurement that have
    /// nothing to do with it.
    fn two_horizontal_strokes(
        x0: f32,
        x1: f32,
        ys: [f32; 2],
        extent: u32,
        half_width_px: f32,
    ) -> crate::render::TileMesh {
        let span = crate::render::mesh::COORD_SCALE * extent as f32;
        let vertex = |x: f32, y: f32, ny: f32| Vertex {
            position: [x * span, y * span],
            normal: [0.0, ny],
            color: [1.0, 0.0, 0.0, 1.0],
            is_stroke: 1.0,
            half_width_px,
            color_to: [1.0, 0.0, 0.0, 1.0],
            half_width_px_to: half_width_px,
            gap_half_px: 0.0,
            gap_half_px_to: 0.0,
            line_distance: 0.0,
            dash_runs: [0.0; 4], line_progress: -1.0,
        };
        let mut vertices = Vec::new();
        let mut indices = Vec::new();
        for y in ys {
            let base = vertices.len() as u32;
            vertices.extend_from_slice(&[
                vertex(x0, y, -1.0),
                vertex(x0, y, 1.0),
                vertex(x1, y, -1.0),
                vertex(x1, y, 1.0),
            ]);
            indices.extend_from_slice(&[base, base + 1, base + 2, base + 1, base + 3, base + 2]);
        }
        crate::render::TileMesh { vertices, indices }
    }

    /// **`projectLineThickness`'s `circumferenceRatioAtTileY` factor is
    /// applied to the globe's stroke extrusion.**
    ///
    /// Mercator stretches by `1/cos(lat)` away from the equator, so one
    /// in-tile unit of northward offset is a SHORTER arc on the sphere the
    /// further from the equator it is: a stroke extruded by a fixed
    /// tile-space amount thins towards the poles. MapLibre's fix is to scale
    /// the tile-space extrusion by `1 / cos(lat)`
    /// (`_projection_globe.vertex.glsl:34-48`, applied at
    /// `line.vertex.glsl:85-88`), and `shader.wgsl`'s
    /// `circumference_ratio_at_tile_y` is that reciprocal's denominator.
    ///
    /// # Why two latitudes in ONE render, and not one latitude twice
    ///
    /// The obvious construction is to render the same stroke twice and cancel
    /// the shader's factor in the second render by pre-multiplying
    /// `tile_units_per_px` by `cos(lat)`. **That construction is circular and
    /// was measured being circular.** The ratio it produces is the ratio of
    /// the two `tile_units_per_px` values the test itself passed, so it comes
    /// out at `1/cos(lat)` whether the shader applies the factor or not --
    /// deleting `adjusted_thickness` from the extrusion left it passing.
    ///
    /// So: two strokes, at the equator and at 47 degrees, in one mesh, one
    /// draw, one uniform. Nothing the test passes can differ between them,
    /// because there is only one of everything; the only thing that can make
    /// their thicknesses differ in a latitude-dependent way is the shader.
    ///
    /// # The figures, BOTH measured
    ///
    /// The competing hypotheses are not derived here, they are measured: the
    /// second one by deleting `adjusted_thickness` from the extrusion,
    /// re-running, and writing down what came out. A closed-form model of the
    /// absolute thicknesses was attempted first and abandoned -- it agreed on
    /// the RATIO but came out about 10% off on the absolute px figures, so
    /// nothing analytic is quoted below.
    ///
    /// Measured on an Apple M-series adapter at 20 px half-width, in the
    /// column at longitude 0:
    ///
    ///     factor applied:   17 px at 47 deg,  36 px at the equator  -> 0.472
    ///     factor deleted:   11 px at 47 deg,  36 px at the equator  -> 0.306
    ///
    /// Two things to notice. The 47-degree stroke is **1.545x** thicker with
    /// the factor than without, against the `1/cos(lat)` = 1.478 the factor is
    /// supposed to supply -- 4.5% out, which is what one whole pixel in an
    /// 11 px run buys you, plus the fact that a tile-space extrusion is
    /// applied BEFORE a nonlinear projection, so a ribbon spanning 10 degrees
    /// of latitude does not scale quite linearly.
    ///
    /// And the equatorial control is **36 px either way, unchanged**. It must
    /// be: `cos(0)` is 1, so the factor is exactly 1 there. That invariance is
    /// as much the assertion as the ratio is -- a factor that also moved the
    /// equator would be scaling something other than latitude.
    ///
    /// Hence the band, 0.43 to 0.52: it brackets the measured 0.472 with room
    /// for the whole-pixel quantisation of a 17 px run, and it excludes the
    /// measured 0.306.
    ///
    /// The formula INSIDE the factor is not what this pins, and does not need
    /// to be: `circumference_ratio_at_tile_y` is the identical
    /// `(2t)/(t^2+1)` expression as `project_to_sphere`'s `cos_sy`, which
    /// `at_full_transition_the_world_is_drawn_as_a_disc_and_not_a_rectangle`
    /// exercises through the silhouette, and `render::globe`'s own tests
    /// check the same algebra in f64 against the direct formula.
    ///
    /// 47 degrees and not 80, for a reason worth keeping: the silhouette at
    /// this camera sits 70.0 degrees from the point under the camera
    /// (`clipping_plane` is `[0, 0, 1, -0.342]`, and `acos(0.342)` = 70.0),
    /// so a stroke much further north is on the far side and
    /// `globe_clipping_z` correctly draws none of it -- leaving one run in
    /// the column instead of two.
    #[test]
    fn a_globe_stroke_is_widened_by_the_circumference_ratio_at_its_latitude() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);

            let size = 512u32;
            let extent = 4096u32;
            let tile = crate::geo::TileId { z: 0, x: 0, y: 0 };
            let zoom = 1.5f64;
            let camera = crate::render::Camera {
                center_lon: 0.0,
                center_lat: 0.0,
                zoom,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: size as f32,
                viewport_height_px: size as f32,
                fov_y: crate::render::Camera::DEFAULT_FOV_Y,
            };

            // On the z0 tile the in-tile y IS the Mercator y, so these are the
            // arguments `circumference_ratio_at_tile_y` will be handed. 0.5 is
            // the equator, where the factor is exactly 1.
            let equator_y = 0.5f64;
            let mid_y = 0.35f64;
            let (_, cos_equator) = crate::render::globe::mercator_y_to_sin_cos_lat(equator_y);
            let (_, cos_mid) = crate::render::globe::mercator_y_to_sin_cos_lat(mid_y);
            assert!(
                (cos_equator - 1.0).abs() < 1e-12,
                "in-tile y 0.5 must BE the equator for the control stroke to be a control: \
                 cos(lat) came out {cos_equator}"
            );

            let frame = crate::render::globe::globe_frame(&camera);
            // 20 logical px of half-width, which is wide for a road and
            // deliberately so: the thickness is read off a whole-pixel
            // rasterization, and at 4 px the narrower of the two runs was
            // 4 px long and the comparison was quantisation noise.
            let mesh =
                two_horizontal_strokes(0.39, 0.61, [equator_y as f32, mid_y as f32], extent, 20.0);
            let tile_buffers = pipeline.create_tile_buffers(&ctx, &mesh);
            let view_proj = camera.view_projection().to_cols_array_2d();
            let model = crate::render::combined_tile_matrix(&camera, tile, extent as f32).to_cols_array_2d();
            let tile_world_size = crate::render::WORLD_SIZE / 2f32.powi(i32::from(tile.z));

            let params = GlobeParams {
                globe_matrix: frame.globe_view_proj.as_mat4().to_cols_array_2d(),
                tile_mercator_coords: crate::render::globe::tile_mercator_coords(tile),
                clipping_plane: frame.clipping_plane.map(|c| c as f32),
                transition: 1.0,
                lyon_to_tile_unit: 1.0 / (crate::render::mesh::COORD_SCALE * extent as f32),
                tile_units_per_px: 1.0 / (tile_world_size * 2f32.powf(zoom as f32)),
                // Unlit: this test grades a stroke WIDTH, in runs of lit
                // pixels. See `UNLIT_SUN`.
                ..GlobeParams::FLAT
            }
            .unlit();

            let color_texture = make_color_texture(&ctx, size);
            let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor::default());
            let depth_view = pipeline.create_depth_view(&ctx, size, size);
            // Device pixel ratio 1, so `aa_feather_px` is 1.0 and the
            // antialiasing band is the same absolute addition to both runs
            // rather than a scale on each.
            let camera_bind_group = pipeline.test_camera_bind_group(&ctx, view_proj, zoom as f32);
            let model_bind_group = pipeline.create_model_bind_group(&ctx, model, 1.0, 0.0, params).1;

            let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            pipeline.render(
                &mut encoder,
                &color_view,
                &depth_view,
                wgpu::Color::TRANSPARENT,
                &camera_bind_group,
                &[(&tile_buffers, &model_bind_group)],
                None,
            );
            let data = read_all_pixels(&ctx, &color_texture, encoder, size).await;

            // The contiguous runs of covered rows in the column at longitude
            // 0 -- one per stroke. A column rather than an area because the
            // area is the wrong instrument and was measured being wrong: it
            // also moves with each ribbon's screen LENGTH and with where its
            // caps land, and came out 1.758 / 1.561 / 1.602 at 8 / 16 / 32 px
            // half-width, converging on nothing.
            let bytes_per_row = (size * 4).next_multiple_of(256);
            let mut runs: Vec<u32> = Vec::new();
            let mut current = 0u32;
            for row in 0..size {
                if data[(row * bytes_per_row + size / 2 * 4) as usize + 3] > 20 {
                    current += 1;
                } else if current > 0 {
                    runs.push(current);
                    current = 0;
                }
            }
            if current > 0 {
                runs.push(current);
            }

            assert_eq!(
                runs.len(),
                2,
                "the column at longitude 0 must cross both strokes as two separate runs, \
                 got {runs:?}. One run means the two merged (or one was clipped away by \
                 `globe_clipping_z`), and no run at all means the strokes never drew"
            );
            // Northern first: rows ascend down the screen and the stroke at
            // in-tile y 0.35 is the northern one, so it is the FIRST run.
            let (mid_thickness, equator_thickness) = (runs[0], runs[1]);
            assert!(
                mid_thickness < equator_thickness,
                "the 47-degree stroke is nearer the silhouette and must still be the thinner \
                 of the two on screen -- the factor corrects the Mercator stretch, not the \
                 projection's own foreshortening: got {mid_thickness} px against \
                 {equator_thickness} px"
            );

            let measured = f64::from(mid_thickness) / f64::from(equator_thickness);
            // 0.472 measured with the factor, 0.306 measured without it --
            // see this test's doc comment, where the band is justified.
            assert!(
                (0.43..=0.52).contains(&measured),
                "the globe's stroke extrusion must carry the 1/cos(lat) factor \
                 (cos(lat) = {cos_mid:.4} here, so the factor is {:.4}). The thickness ratio \
                 between the 47-degree stroke and the equatorial one measures 0.472 with the \
                 factor applied and 0.306 with it deleted; this run got {measured:.4} \
                 ({mid_thickness} px against {equator_thickness} px). Near 0.31 means the \
                 factor is not reaching the extrusion at all",
                1.0 / cos_mid
            );
        });
    }

    /// A tile-covering grid whose vertices are coloured by which HALF of the
    /// tile they sit in — west `west_color`, east `east_color`.
    ///
    /// Exists for one test, `the_globe_draws_east_on_the_right_like_the_flat_map_does`:
    /// a single-colour mesh cannot tell a globe from a MIRRORED globe, and a
    /// mirrored globe passes every other assertion in this file.
    fn tile_covering_grid_split(
        cells: u32,
        extent: u32,
        west_color: [f32; 4],
        east_color: [f32; 4],
    ) -> crate::render::TileMesh {
        let mut mesh = tile_covering_grid(cells, extent, west_color);
        let span = crate::render::mesh::COORD_SCALE * extent as f32;
        for vertex in &mut mesh.vertices {
            if vertex.position[0] > span * 0.5 {
                vertex.color = east_color;
                vertex.color_to = east_color;
            }
        }
        mesh
    }

    /// **EAST IS ON THE RIGHT, on the globe as on the flat map** — the pixel
    /// form of `render::globe`'s
    /// `the_globe_and_mercator_chains_agree_on_which_way_is_east`.
    ///
    /// Task 2 moved the Mercator x-flip onto the flat position, *before*
    /// `interpolate_projection`, because it undoes a property of
    /// `camera::eye_target_up`'s basis that `globe_frame` does not inherit;
    /// flipping the blended result would mirror the sphere. Task 3 is what
    /// makes `transition` nonzero, so it is the commit that has to prove the
    /// sphere is not mirrored — and no existing test can: the disc test draws
    /// one colour, and a mirrored disc is the same disc.
    ///
    /// So the z0 tile is drawn with its west half blue and its east half red,
    /// and the same mesh and camera are rendered flat and spherical. In both,
    /// a sample right of centre must be red and one left of centre blue. The
    /// flat render is the control that the expectation itself is the right way
    /// round.
    #[test]
    fn the_globe_draws_east_on_the_right_like_the_flat_map_does() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);

            let size = 512u32;
            let extent = 4096u32;
            let tile = crate::geo::TileId { z: 0, x: 0, y: 0 };
            let zoom = 1.5f64;
            let camera = crate::render::Camera {
                center_lon: 0.0,
                center_lat: 0.0,
                zoom,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: size as f32,
                viewport_height_px: size as f32,
                fov_y: crate::render::Camera::DEFAULT_FOV_Y,
            };

            let west = [0.0, 0.0, 1.0, 1.0];
            let east = [1.0, 0.0, 0.0, 1.0];
            let mesh = tile_covering_grid_split(64, extent, west, east);
            let tile_buffers = pipeline.create_tile_buffers(&ctx, &mesh);
            let frame = crate::render::globe::globe_frame(&camera);
            let upload = crate::render::globe::GlobeUpload::new(
                &frame, &camera, extent as f32, UNLIT_SUN, UNLIT_NIGHT,
            );
            let model =
                crate::render::combined_tile_matrix(&camera, tile, extent as f32).to_cols_array_2d();
            let view_proj = camera.view_projection().to_cols_array_2d();

            let bytes_per_row = (size * 4).next_multiple_of(256);
            let mut sampled = Vec::new();
            for transition in [0.0f32, 1.0] {
                let mut params = upload.for_tile(tile);
                // `transition` is the only thing that differs between the two
                // renders, exactly as in the disc test above.
                params.transition = transition;

                let color_texture = make_color_texture(&ctx, size);
                let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor::default());
                let depth_view = pipeline.create_depth_view(&ctx, size, size);
                let camera_bind_group = pipeline.test_camera_bind_group(&ctx, view_proj, zoom as f32);
                let model_bind_group =
                    pipeline.create_model_bind_group(&ctx, model, 1.0, 0.0, params).1;

                let mut encoder =
                    ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
                pipeline.render(
                    &mut encoder,
                    &color_view,
                    &depth_view,
                    wgpu::Color::TRANSPARENT,
                    &camera_bind_group,
                    &[(&tile_buffers, &model_bind_group)],
                    None,
                );
                let data = read_all_pixels(&ctx, &color_texture, encoder, size).await;

                // A quarter of the width either side of centre: inside the
                // globe's silhouette (radius ~161 px, see the disc test) and
                // far enough from the seam at the prime meridian that vertex
                // colour interpolation across it cannot decide the answer.
                let sample = |col: u32| -> (u8, u8) {
                    let offset = (size / 2 * bytes_per_row + col * 4) as usize;
                    (data[offset], data[offset + 2])
                };
                sampled.push((sample(size / 4), sample(size * 3 / 4)));
            }

            for (label, (left, right)) in [("flat", sampled[0]), ("globe", sampled[1])] {
                let (left_r, left_b) = left;
                let (right_r, right_b) = right;
                assert!(
                    left_b > left_r,
                    "{label}: the WEST half of the tile is blue and must be drawn LEFT of \
                     centre; got r={left_r} b={left_b} there"
                );
                assert!(
                    right_r > right_b,
                    "{label}: THE EAST HALF IS RED AND MUST BE DRAWN RIGHT OF CENTRE; got \
                     r={right_r} b={right_b}. On the globe, this failing means the Mercator \
                     x-flip is being applied to the BLENDED position rather than to the flat \
                     one -- see shader.wgsl's note on `clip.x = -clip.x` and \
                     render::globe::the_mercator_x_flip_stays_on_the_flat_position_and_not_on_the_blend"
                );
            }
        });
    }

    /// **Task 3's measurement: what the `GlobeTransform` phase costs, at both
    /// ends of the transition, from a run that really drew.**
    ///
    /// `MapView::paint` is not reachable from a test -- it needs a live
    /// `wgpu::Surface`, which is why its own borrow-safety note says "nothing
    /// in this test suite exercises `paint()`". So this drives, headlessly and
    /// through the same `PhaseLap`, exactly the region `paint`'s
    /// `GlobeTransform` lap brackets: the one `globe_frame` call and the
    /// `GlobeUpload` narrowing, nothing else. The per-tile half is issued
    /// after that boundary, where `paint` charges it to `MeshUpload`, so a
    /// per-tile cost that leaked into the frame-level phase would show up here
    /// as growth with the tile count.
    ///
    /// Sixty-four tiles is a whole z3 world -- four times the ~16
    /// `MapView::tiles_for_zoom` actually selects at z3 on a 1200x900
    /// viewport -- so the figure is taken against a heavier tile set than the
    /// globe band produces.
    ///
    /// # What it measured
    ///
    /// Apple M-series, 64 tiles, 200 frames per end, means over the run, two
    /// independent invocations of this test (one run is not a measurement):
    ///
    /// ```text
    ///   transition   globe_transform   mesh_upload (64 uniform writes)
    ///   0.0 (z8)     0.0033 / 0.0033       0.2886 / 0.2826
    ///   1.0 (z2)     0.0032 / 0.0034       0.2751 / 0.2798
    /// ```
    ///
    /// **3.3 us, and the delta between the two ends is 0.0001 ms** -- smaller
    /// than the spread between two runs of the same end, which is what it
    /// should be: `globe_frame` runs the identical matrix chain whatever the
    /// transition comes out as, because the transition is an OUTPUT of it and
    /// not a branch in it. 3.3 us is 0.02% of a 16.7 ms frame and 0.7% of the
    /// plan's 0.5 ms ceiling, and it is 1.2% of the 64 per-tile uniform
    /// writes sitting next to it.
    ///
    /// The assertions deliberately pin only the CEILING and the shape
    /// (frame-level, not per-tile). Pinning 3.3 us would pin this machine's
    /// CPU.
    #[cfg(feature = "probe")]
    #[test]
    fn the_globe_transform_phase_costs_the_same_flat_or_spherical() {
        use crate::render::alloc::{paint_phases_mean, reset_paint_phases, PaintPhase, PhaseLap};
        // The plan's ceiling: "if the globe transform phase exceeds 0.5 ms,
        // stop and report".
        const CEILING_MS: f64 = 0.5;
        const FRAMES: usize = 200;

        let _guard = crate::render::alloc::phase_lock();
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 256u32;
            let extent = 4096u32;
            // One mesh, shared by all 64 draws: the phase being measured is
            // CPU-side uniform work, and 64 copies of the same triangles would
            // only measure the allocator.
            let mesh = tile_covering_grid(8, extent, [0.2, 0.4, 0.9, 1.0]);
            let buffers = pipeline.create_tile_buffers(&ctx, &mesh);
            let tiles: Vec<crate::geo::TileId> = (0..8u32)
                .flat_map(|x| (0..8u32).map(move |y| crate::geo::TileId { z: 3, x, y }))
                .collect();

            let color_texture = make_color_texture(&ctx, size);
            let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor::default());
            let depth_view = pipeline.create_depth_view(&ctx, size, size);

            let mut split = Vec::new();
            // z13 is above `PROJECTION_BLEND_ZOOM_MAX`, so
            // `transition_for_zoom` is exactly 0.0 and the shader takes its
            // flat early-out; z2 is below `PROJECTION_BLEND_ZOOM_MIN`, so it
            // is exactly 1.0 and every vertex goes through
            // `project_to_sphere`. The two ends the plan asks for.
            for zoom in [13.0f64, 2.0] {
                let camera = crate::render::Camera {
                    center_lon: 0.0,
                    center_lat: 0.0,
                    zoom,
                    pitch: 0.0,
                    bearing: 0.0,
                    viewport_width_px: size as f32,
                    viewport_height_px: size as f32,
                    fov_y: crate::render::Camera::DEFAULT_FOV_Y,
                };
                let camera_bind_group = pipeline.test_camera_bind_group(
                    &ctx,
                    camera.view_projection().to_cols_array_2d(),
                    zoom as f32,
                );
                // Created once per tile, outside the frame loop, because that
                // is where `paint` creates them too: the mesh-upload loop
                // builds a bind group on a tile's FIRST frame and the rewrite
                // loop only writes into it afterwards.
                let models: Vec<(
                    [[f32; 4]; 4],
                    (crate::render::alloc::TrackedBuffer, wgpu::BindGroup),
                )> = tiles
                    .iter()
                    .map(|&tile| {
                        let matrix =
                            crate::render::combined_tile_matrix(&camera, tile, extent as f32)
                                .to_cols_array_2d();
                        let group =
                            pipeline.create_model_bind_group(&ctx, matrix, 1.0, 0.0, GlobeParams::FLAT);
                        (matrix, group)
                    })
                    .collect();
                let draws: Vec<(&TileBuffers, &wgpu::BindGroup)> =
                    models.iter().map(|(_, (_, group))| (&buffers, group)).collect();

                reset_paint_phases();
                crate::render::globe::reset_globe_frame_calls();
                let before = crate::render::allocation_stats().draw_calls_total;
                let mut transition = f32::NAN;
                for _ in 0..FRAMES {
                    let mut lap = PhaseLap::default();
                    // THE PHASE. One `globe_frame`, one narrowing.
                    let globe = crate::render::globe::GlobeUpload::new(
                        &crate::render::globe::globe_frame(&camera),
                        &camera,
                        extent as f32,
                        UNLIT_SUN,
                        UNLIT_NIGHT,
                    );
                    lap.lap(PaintPhase::GlobeTransform);
                    transition = globe.transition();

                    for (&tile, (matrix, (buffer, _))) in tiles.iter().zip(models.iter()) {
                        pipeline.write_model_uniform(
                            &ctx,
                            buffer,
                            *matrix,
                            1.0,
                            0.0,
                            globe.for_tile(tile),
                        );
                    }
                    lap.lap(PaintPhase::MeshUpload);

                    let mut encoder = ctx
                        .device
                        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
                    pipeline.render(
                        &mut encoder,
                        &color_view,
                        &depth_view,
                        wgpu::Color::BLACK,
                        &camera_bind_group,
                        &draws,
                        None,
                    );
                    ctx.queue.submit(Some(encoder.finish()));
                    lap.lap(PaintPhase::TileEncode);
                }
                ctx.device
                    .poll(wgpu::PollType::wait_indefinitely())
                    .expect("the frames to complete");

                // **Proof the run painted**, the gate every performance figure
                // in this project carries: a phase split off a process that
                // never issued a draw call describes nothing. A floor rather
                // than an equality because `DRAW_CALLS` is process-global and
                // the other GPU tests in this binary draw on their own
                // threads.
                let drawn = crate::render::allocation_stats().draw_calls_total - before;
                assert!(
                    drawn >= (tiles.len() * FRAMES) as u64,
                    "expected at least {} draw calls from {} tiles over {FRAMES} frames, got \
                     {drawn} -- a phase split from a run that did not paint is not a measurement",
                    tiles.len() * FRAMES,
                    tiles.len()
                );

                // **The once-per-frame assertion, at frame scale.** 64 tiles a
                // frame for FRAMES frames is FRAMES `globe_frame` calls, not
                // 64 x FRAMES. This is the counter the plan asks for, and what
                // it refuses is MapLibre's per-tile `getProjectionData`
                // (`globe_transform.ts:284-295`).
                assert_eq!(
                    crate::render::globe::globe_frame_calls(),
                    FRAMES as u64,
                    "{FRAMES} frames of {} tiles must cost {FRAMES} globe_frame calls, not {}",
                    tiles.len(),
                    tiles.len() * FRAMES
                );

                let mean = paint_phases_mean();
                eprintln!(
                    "globe phase split, transition {transition:.1} ({} tiles x {FRAMES} frames, \
                     {drawn} draw calls): globe_transform {:.4} ms, mesh_upload {:.4} ms, \
                     tile_encode {:.4} ms",
                    tiles.len(),
                    mean.globe_transform_ms,
                    mean.mesh_upload_ms,
                    mean.tile_encode_ms,
                );
                split.push((transition, mean.globe_transform_ms, mean.mesh_upload_ms));
            }

            let (flat_transition, flat_ms, flat_upload) = split[0];
            let (globe_transition, globe_ms, globe_upload) = split[1];
            assert_eq!(
                flat_transition, 0.0,
                "this split's flat camera must be past PROJECTION_BLEND_ZOOM_MAX"
            );
            assert_eq!(
                globe_transition, 1.0,
                "z2 is below PROJECTION_BLEND_ZOOM_MIN, so the map must be a sphere"
            );

            assert!(
                flat_ms < CEILING_MS && globe_ms < CEILING_MS,
                "the globe transform phase must stay under {CEILING_MS} ms: flat {flat_ms:.4} ms, \
                 globe {globe_ms:.4} ms"
            );
            // The phase is frame-level, so it must be a small fraction of the
            // per-tile work beside it. If `globe_frame` ever migrated into the
            // tile loop this inequality is the first thing that would break --
            // 64 matrix chains would dwarf 64 uniform writes.
            assert!(
                flat_ms < flat_upload && globe_ms < globe_upload,
                "the once-per-frame globe phase must cost less than the 64 per-tile uniform \
                 writes beside it: globe_transform flat {flat_ms:.4} / globe {globe_ms:.4} ms \
                 against mesh_upload flat {flat_upload:.4} / globe {globe_upload:.4} ms"
            );
        });
    }

    /// **What Task 8 adds to `paint` per frame, measured — and why it got no
    /// `PaintPhase` of its own.**
    ///
    /// The plan's standing rule is that no new per-frame CPU work lands
    /// without a phase number behind it, and that a globe phase over 0.5 ms
    /// stops and reports. The number behind this work is `Surface`'s, whose
    /// span already covers the camera uniform write and the colour/depth
    /// views; this measures the increment rather than splitting it out,
    /// because a 21st phase for the figures below would cost more to read
    /// than it explains.
    ///
    /// What is measured is exactly what `paint` does, in `paint`'s order:
    /// `space_clear_color`, then the backdrop's `combined_tile_matrix` and
    /// 192-byte `write_model_uniform`, then the star field's 96-byte
    /// `write_sky`. NOT the backdrop's mesh — that is rebuilt only when the
    /// background colour changes, and its own cost is measured separately
    /// below and reported rather than amortised into a per-frame figure.
    ///
    /// Measured on an Apple M-series adapter, 200 frames, mean per frame:
    ///
    /// ```text
    ///                    space setup      GlobeTransform beside it
    ///   release, flat        0.0000 ms                    0.0002 ms
    ///   release, globe       0.0236 ms                    0.0002 ms
    ///   debug,   flat        0.0001 ms                    0.0031 ms
    ///   debug,   globe       0.0214 ms                    0.0035 ms
    /// ```
    ///
    /// **0.0236 ms in release, i.e. 21x under the plan's 0.5 ms ceiling**,
    /// and unmeasurable on a flat frame — which is the gate, and the number
    /// that matters most here. It is two `queue.write_buffer` calls and a
    /// `combined_tile_matrix`; the writes are why the release figure is not
    /// below the debug one.
    ///
    /// The one-off mesh build is the figure worth watching instead:
    /// `backdrop_mesh` at extent 4096, granularity 64 with both pole caps,
    /// reported below — **0.365 ms release (0.351–0.534 over 7 runs),
    /// 8.701 ms debug** — on the FRAME thread, unlike Task 5's subdivision
    /// which is on the blocking pool. It runs on the first globe frame and
    /// then only when the background colour's STOP PAIR changes;
    /// `render::space::backdrop_plan` explains why it is the pair and not
    /// the lerped colour, and that distinction is what keeps 0.365 ms off
    /// every frame of a zoom over a style whose background is a zoom
    /// expression.
    ///
    /// The assertions pin the CEILING and the flat/globe RATIO, not the
    /// microseconds: pinning 23 us would pin this machine's CPU.
    #[cfg(feature = "probe")]
    #[test]
    fn the_space_setup_costs_nothing_a_flat_frame_can_notice() {
        use crate::render::alloc::{paint_phases_mean, reset_paint_phases, PaintPhase, PhaseLap};
        const CEILING_MS: f64 = 0.5;
        const FRAMES: usize = 200;

        let _guard = crate::render::alloc::phase_lock();
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let stars = crate::render::space::StarPipeline::new(
                &ctx,
                wgpu::TextureFormat::Rgba8Unorm,
                pipeline.depth_format(),
            );
            let size = 256u32;
            let extent = 4096u32;
            let background =
                crate::style::Rgba { r: 79.0 / 255.0, g: 116.0 / 255.0, b: 153.0 / 255.0, a: 1.0 };
            let space = crate::render::space::Space {
                star_intensity: 1.0,
                ..crate::render::space::Space::default()
            };

            // The one-off, reported rather than amortised. Median of 7, and
            // the spread, because one run is not a measurement.
            let mut mesh_ms = Vec::new();
            for _ in 0..7 {
                let start = crate::timing::Instant::now();
                let mesh = crate::render::space::backdrop_mesh(extent, background, background);
                mesh_ms.push(start.elapsed().as_secs_f64() * 1.0e3);
                assert!(!mesh.indices.is_empty());
            }
            mesh_ms.sort_by(f64::total_cmp);
            eprintln!(
                "backdrop_mesh (granularity 64, both pole caps): p50 {:.3} ms ({:.3}-{:.3})",
                mesh_ms[3], mesh_ms[0], mesh_ms[6]
            );

            let color_texture = make_color_texture(&ctx, size);
            let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor::default());
            let depth_view = pipeline.create_depth_view(&ctx, size, size);

            let mut split = Vec::new();
            // z13, not z8: the projection blend band is z11-z12 now, so z8
            // is inside the pure-globe half. See
            // `render::globe::PROJECTION_BLEND_ZOOM_MIN`.
            for zoom in [13.0f64, 2.0] {
                let camera = crate::render::Camera {
                    center_lon: 0.0,
                    center_lat: 0.0,
                    zoom,
                    pitch: 0.0,
                    bearing: 0.0,
                    viewport_width_px: size as f32,
                    viewport_height_px: size as f32,
                    fov_y: crate::render::Camera::DEFAULT_FOV_Y,
                };
                let camera_bind_group = pipeline.test_camera_bind_group(
                    &ctx,
                    camera.view_projection().to_cols_array_2d(),
                    zoom as f32,
                );
                let (backdrop_buffers, backdrop_group) =
                    backdrop_draw(&ctx, &pipeline, &camera, extent, background);
                // The backdrop's own uniform buffer, so the per-frame
                // rewrite `paint` does is really being timed and not
                // skipped.
                let tile = crate::render::space::BACKDROP_TILE;
                let matrix =
                    crate::render::combined_tile_matrix(&camera, tile, extent as f32)
                        .to_cols_array_2d();
                let (model_buffer, _unused) = pipeline.create_model_bind_group(
                    &ctx,
                    matrix,
                    1.0,
                    0.0,
                    GlobeParams::FLAT,
                );

                reset_paint_phases();
                let before = crate::render::allocation_stats().draw_calls_total;
                let mut transition = f32::NAN;
                for _ in 0..FRAMES {
                    let mut lap = PhaseLap::default();
                    let globe = crate::render::globe::GlobeUpload::new(
                        &crate::render::globe::globe_frame(&camera),
                        &camera,
                        extent as f32,
                        UNLIT_SUN,
                        UNLIT_NIGHT,
                    );
                    lap.lap(PaintPhase::GlobeTransform);
                    transition = globe.transition();

                    // THE INCREMENT, in `paint`'s own order. Lapped into
                    // `Surface`, which is the phase it lands in there.
                    let _clear = TilePipeline::clear_color(
                        &crate::render::space::space_clear_color(
                            Some(background),
                            &space,
                            transition,
                        ),
                    );
                    if crate::render::space::draws_backdrop(transition) {
                        let matrix =
                            crate::render::combined_tile_matrix(&camera, tile, extent as f32)
                                .to_cols_array_2d();
                        pipeline.write_model_uniform(
                            &ctx,
                            &model_buffer,
                            matrix,
                            1.0,
                            0.0,
                            globe.for_tile(tile),
                        );
                    }
                    if space.draws_stars(transition) {
                        stars.write_sky(&ctx, &camera, &space);
                    }
                    lap.lap(PaintPhase::Surface);

                    let draw = if transition > 0.0 {
                        SpaceDraw {
                            sky: None,
                            stars: Some(&stars),
                            backdrop: Some((&backdrop_buffers, &backdrop_group)),
                            // This test measures Task 8's per-frame SETUP
                            // cost and nothing else, so its figure stays
                            // comparable with the one Task 8 recorded. The
                            // atmosphere's cost is the GPU's rather than
                            // `paint`'s, and is measured on its own in
                            // `the_full_screen_atmosphere_costs_less_than_the_plans_half_millisecond`.
                            atmosphere: None,
                            look: None,
                        }
                    } else {
                        SpaceDraw::NONE
                    };
                    let mut encoder = ctx
                        .device
                        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
                    pipeline.render_with_space(
                        &mut encoder,
                        &color_view,
                        &depth_view,
                        wgpu::Color::BLACK,
                        &camera_bind_group,
                        &draw,
                        &[],
                        None,
                    );
                    ctx.queue.submit(Some(encoder.finish()));
                    lap.lap(PaintPhase::TileEncode);
                }
                ctx.device
                    .poll(wgpu::PollType::wait_indefinitely())
                    .expect("the frames to complete");

                // **Proof the run painted.** A globe frame issues two draw
                // calls here (the stars and the ocean sphere) and a flat
                // one issues none, so the gate is asserted at the end where
                // both figures are in hand.
                let drawn = crate::render::allocation_stats().draw_calls_total - before;
                let mean = paint_phases_mean();
                eprintln!(
                    "space setup, transition {transition:.1} ({FRAMES} frames, {drawn} draw \
                     calls): surface {:.4} ms, globe_transform {:.4} ms",
                    mean.surface_ms, mean.globe_transform_ms
                );
                split.push((transition, mean.surface_ms, mean.globe_transform_ms, drawn));
            }

            let (flat_transition, flat_ms, _, flat_drawn) = split[0];
            let (globe_transition, globe_ms, globe_transform_ms, globe_drawn) = split[1];
            assert_eq!(
                flat_transition, 0.0,
                "this split's flat camera must be past PROJECTION_BLEND_ZOOM_MAX"
            );
            assert_eq!(
                globe_transition, 1.0,
                "z2 is below PROJECTION_BLEND_ZOOM_MIN, so the map must be a sphere"
            );
            assert_eq!(
                flat_drawn, 0,
                "a FLAT frame must issue no space draw calls at all; it issued {flat_drawn}"
            );
            assert!(
                globe_drawn >= 2 * FRAMES as u64,
                "a globe frame draws the stars and the ocean sphere, so {FRAMES} frames \
                 must issue at least {} draw calls; {globe_drawn} says the run did not \
                 paint what it was measuring",
                2 * FRAMES
            );

            assert!(
                flat_ms < CEILING_MS && globe_ms < CEILING_MS,
                "the space setup must stay under {CEILING_MS} ms: flat {flat_ms:.4} ms, \
                 globe {globe_ms:.4} ms"
            );
            // THE GATE WORKS, as a ratio rather than as a claim: a flat
            // frame must cost an order of magnitude less than a globe one,
            // because on a flat frame the whole of this is one `f32`
            // comparison. Measured 0.0001 against 0.0205, a factor of 205.
            assert!(
                flat_ms * 10.0 < globe_ms,
                "a flat frame must cost at least 10x less than a globe frame here -- the \
                 gate is the point: flat {flat_ms:.4} ms, globe {globe_ms:.4} ms"
            );
            // The globe transform beside it is REPORTED and deliberately
            // not compared against. Two drafts of this test tried: first
            // "the space setup is smaller", which the debug measurement
            // refused (0.0205 against 0.0035), then "within two orders of
            // magnitude", which the RELEASE measurement refused (0.0236
            // against 0.0002 -- optimisation makes pure arithmetic 15x
            // faster and leaves a `queue.write_buffer` where it was). The
            // two are not comparable quantities and an inequality between
            // them was never evidence of anything.
            let _ = globe_transform_ms;
            // 40 ms is a frame at 25 Hz. The real figures are 0.365 ms
            // release and 8.701 ms debug, so this bound exists to catch a
            // granularity or a cell-count change turning a one-off into a
            // stall, not to grade the current number.
            assert!(
                mesh_ms[3] < 40.0,
                "the backdrop mesh is built on the FRAME thread, so a background-colour \
                 change must not cost a visible hitch: p50 {:.3} ms",
                mesh_ms[3]
            );
        });
    }

    // ---- A2/A3: fill outline as a line list, chunked overlay buffers ----

    fn square(cx: i32, cy: i32, half: i32) -> geo_types::Polygon<i32> {
        geo_types::Polygon::new(
            geo_types::LineString::from(vec![
                (cx - half, cy - half),
                (cx + half, cy - half),
                (cx + half, cy + half),
                (cx - half, cy + half),
                (cx - half, cy - half),
            ]),
            vec![],
        )
    }

    fn fill_plan(polygons: Vec<geo_types::Polygon<i32>>, color: crate::style::Rgba) -> crate::style::PaintPlan {
        crate::style::PaintPlan {
            ops: vec![crate::style::PaintOp::Fill {
                color_from: color,
                color_to: color,
                geometry: std::sync::Arc::new(geo_types::Geometry::MultiPolygon(geo_types::MultiPolygon(
                    polygons,
                ))),
            }],
        }
    }

    const GREEN: crate::style::Rgba = crate::style::Rgba { r: 0.0, g: 1.0, b: 0.0, a: 1.0 };
    const RED: crate::style::Rgba = crate::style::Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 };

    /// Tessellates `plan` through the overlay path at clip-space scale
    /// (coordinates in thousandths) and uploads it as chunks.
    fn overlay_chunks(
        ctx: &GpuContext,
        pipeline: &TilePipeline,
        plan: &crate::style::PaintPlan,
        outlines: &[Option<crate::style::Rgba>],
        max_vertices: usize,
    ) -> Vec<TileBuffers> {
        overlay_chunks_reusing(ctx, pipeline, plan, outlines, max_vertices, Vec::new())
    }

    /// [`overlay_chunks`], handing `existing`'s buffers to the install one
    /// chunk each, as a re-push does (`overlay::host::install_overlay_chunks`).
    fn overlay_chunks_reusing(
        ctx: &GpuContext,
        pipeline: &TilePipeline,
        plan: &crate::style::PaintPlan,
        outlines: &[Option<crate::style::Rgba>],
        max_vertices: usize,
        existing: Vec<TileBuffers>,
    ) -> Vec<TileBuffers> {
        let mut existing = existing.into_iter();
        let (mesh, _, extras) = crate::render::mesh::tessellate_overlay_plan_reusing(
            plan,
            outlines,
            4096,
            0.001,
            crate::render::subdivide::Subdivision::NONE,
            &|| false,
            crate::render::TileMesh::default(),
        )
        .unwrap();
        let plans =
            crate::render::mesh::plan_overlay_chunks(&mesh, &extras, max_vertices, ctx.max_buffer_size).unwrap();
        let (mut a, mut b, mut c, mut d) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        plans
            .iter()
            .map(|chunk| {
                let u16_format = chunk.is_u16();
                let indices = crate::render::mesh::rebase_indices(
                    &mesh.indices[chunk.indices.clone()],
                    chunk.vertices.start,
                    u16_format,
                    &mut a,
                    &mut b,
                );
                let outline = crate::render::mesh::rebase_indices(
                    &extras.outline[chunk.outline.clone()],
                    chunk.vertices.start,
                    u16_format,
                    &mut c,
                    &mut d,
                );
                pipeline.install_overlay_chunk(ctx, existing.next(), &mesh.vertices[chunk.vertices.clone()], indices, outline)
            })
            .collect()
    }

    async fn render_overlay(ctx: &GpuContext, pipeline: &TilePipeline, chunks: &[TileBuffers], size: u32) -> Vec<u8> {
        let identity = glam::Mat4::IDENTITY.to_cols_array_2d();
        let camera_bind_group = pipeline.test_camera_bind_group(ctx, identity, 0.0);
        let model_bind_group = pipeline.test_model_bind_group(ctx, identity, 1.0, 0.0);
        let color_texture = make_color_texture(ctx, size);
        let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let depth_view = pipeline.create_depth_view(ctx, size, size);
        let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        let draws: Vec<(&TileBuffers, &wgpu::BindGroup)> =
            chunks.iter().map(|chunk| (chunk, &model_bind_group)).collect();
        pipeline.render(&mut encoder, &color_view, &depth_view, wgpu::Color::TRANSPARENT, &camera_bind_group, &draws, None);
        read_all_pixels(ctx, &color_texture, encoder, size).await
    }

    fn pixel(data: &[u8], size: u32, x: u32, y: u32) -> [u8; 4] {
        let bytes_per_row = (size * 4).next_multiple_of(256);
        let at = (y * bytes_per_row + x * 4) as usize;
        [data[at], data[at + 1], data[at + 2], data[at + 3]]
    }

    /// A2's pixel gate: `fill-outline-color` draws its hairline ON the
    /// fill's edge, from the fill's own vertices, and the interior stays
    /// the fill colour. Without the outline the same edge pixel is green.
    #[test]
    fn a_fill_outline_draws_red_pixels_on_the_edge_of_a_green_fill() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 64u32;
            // Edge at clip x = +0.515 -> pixel column 48.48, row 32.
            let plan = fill_plan(vec![square(0, 0, 515)], GREEN);

            let outlined = overlay_chunks(&ctx, &pipeline, &plan, &[Some(RED)], usize::MAX);
            assert_eq!(outlined.len(), 1);
            assert_eq!(outlined[0].index_format, wgpu::IndexFormat::Uint16);
            assert_eq!(outlined[0].outline.as_ref().map(|(_, n)| *n), Some(8), "4 edges, 2 indices each");
            let data = render_overlay(&ctx, &pipeline, &outlined, size).await;
            let center = pixel(&data, size, 32, 32);
            assert!(center[1] > 200 && center[0] < 50, "interior is the fill, got {center:?}");
            let edge = (47..=49).map(|x| pixel(&data, size, x, 32)).find(|p| p[0] > 200 && p[1] < 50);
            assert!(edge.is_some(), "a red outline pixel on the right edge, row 32");

            let plain = overlay_chunks(&ctx, &pipeline, &plan, &[None], usize::MAX);
            assert!(plain[0].outline.is_none());
            let data = render_overlay(&ctx, &pipeline, &plain, size).await;
            assert!((47..=49).all(|x| pixel(&data, size, x, 32)[0] < 50), "no outline without fill-outline-color");
        });
    }

    /// **A re-push that reuses a source's buffers draws the NEW triangles,
    /// not the old ones.** The consumer-reported wedges: kilometre-long
    /// translucent triangles from one route vertex to another after a
    /// source was re-pushed.
    ///
    /// A u16 chunk with an ODD triangle count has an index byte length of
    /// `6 * n`, which is not a multiple of `wgpu::COPY_BUFFER_ALIGNMENT`.
    /// A fresh buffer never noticed -- `create_buffer_init` pads -- but a
    /// REUSED one is written with `queue.write_buffer`, which rejects an
    /// unaligned size as a validation error. The error handler prints and
    /// carries on, so the write is simply dropped: the new vertices land,
    /// the new index COUNT is recorded, and the draw reads the previous
    /// generation's indices over them -- triangles joining vertices that
    /// were never meant to meet.
    ///
    /// Here: an octagon (6 triangles, 36 index bytes) is installed, then
    /// its buffers are handed to a pentagon (9 indices, 18 bytes).
    /// The reused install must draw exactly what a fresh one draws, and
    /// wgpu must raise nothing.
    #[test]
    fn a_reused_chunk_with_an_odd_triangle_count_draws_its_own_triangles() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 64u32;
            let octagon = geo_types::Polygon::new(
                geo_types::LineString::from(
                    (0..=8)
                        .map(|k| {
                            let a = std::f64::consts::TAU * f64::from(k % 8) / 8.0;
                            ((500.0 + 400.0 * a.cos()) as i32, (400.0 * a.sin()) as i32)
                        })
                        .collect::<Vec<_>>(),
                ),
                vec![],
            );
            // A pentagon: 3 triangles, 9 u16 indices, 18 bytes.
            let pentagon = geo_types::Polygon::new(
                geo_types::LineString::from(vec![
                    (-900, -300),
                    (-500, -300),
                    (-400, 0),
                    (-700, 200),
                    (-950, 0),
                    (-900, -300),
                ]),
                vec![],
            );
            let before = overlay_chunks(&ctx, &pipeline, &fill_plan(vec![octagon], GREEN), &[None], usize::MAX);
            assert_eq!(before.len(), 1);
            assert_eq!(before[0].index_count, 18, "the octagon is 6 triangles");

            let scope = ctx.device.push_error_scope(wgpu::ErrorFilter::Validation);
            let reused = overlay_chunks_reusing(
                &ctx,
                &pipeline,
                &fill_plan(vec![pentagon.clone()], GREEN),
                &[None],
                usize::MAX,
                before,
            );
            let error = scope.pop().await;
            assert_eq!(reused[0].index_count, 9, "three triangles");
            assert_eq!(reused[0].index_format, wgpu::IndexFormat::Uint16);

            let fresh = overlay_chunks(&ctx, &pipeline, &fill_plan(vec![pentagon], GREEN), &[None], usize::MAX);
            let want = render_overlay(&ctx, &pipeline, &fresh, size).await;
            let got = render_overlay(&ctx, &pipeline, &reused, size).await;
            let differing = want.chunks(4).zip(got.chunks(4)).filter(|(a, b)| a != b).count();
            assert_eq!(differing, 0, "{differing} pixels differ between the reused and the fresh upload");
            assert!(error.is_none(), "reusing the buffers raised {error:?}");
        });
    }

    /// A3: a source over a (forced, tiny) chunk limit splits into several
    /// chunks with u16 indices, and every chunk draws.
    #[test]
    fn a_source_over_the_chunk_limit_splits_and_draws_every_chunk() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 64u32;
            // A 4x4 grid of squares, one unit each; 4 vertices a chunk forces
            // one square per chunk.
            let squares: Vec<_> = (0..4)
                .flat_map(|i| (0..4).map(move |j| square(-750 + i * 500, -750 + j * 500, 200)))
                .collect();
            let plan = fill_plan(squares, GREEN);
            let chunks = overlay_chunks(&ctx, &pipeline, &plan, &[Some(RED)], 4);
            assert_eq!(chunks.len(), 16, "one chunk per square");
            assert!(chunks.iter().all(|c| c.index_format == wgpu::IndexFormat::Uint16));
            let data = render_overlay(&ctx, &pipeline, &chunks, size).await;
            for i in 0..4 {
                for j in 0..4 {
                    // Square centre in clip (-0.75 + 0.5 i) -> pixel.
                    let x = ((-0.75 + 0.5 * i as f32 + 1.0) / 2.0 * size as f32) as u32;
                    let y = ((1.0 - (-0.75 + 0.5 * j as f32)) / 2.0 * size as f32) as u32;
                    let p = pixel(&data, size, x, y.min(size - 1));
                    assert!(p[1] > 200, "square ({i},{j}) drew, got {p:?}");
                }
            }
        });
    }

    /// **A stand-in's exclusion box hides exactly the circles under it.**
    /// Two instanced dots 100 px either side of the centre: both draw with
    /// `CircleParams::NONE`; with the east dot's surroundings excluded,
    /// only the west one does. What lets a coarser point layer draw under
    /// the camera's view without doubling it.
    #[test]
    fn a_circle_exclusion_box_and_filter_window_hide_only_the_instances_they_name() {
        use crate::render::circles::{CircleInstance, CircleParams};
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 256u32;
            let camera = crate::render::Camera {
                center_lon: 0.0,
                center_lat: 0.0,
                zoom: 10.0,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: size as f32,
                viewport_height_px: size as f32,
                fov_y: crate::render::Camera::DEFAULT_FOV_Y,
            };
            let origin = crate::overlay::geom::lon_lat_to_world(0.0, 0.0);
            // 100 px at z10, in lyon units of the origin.
            let off = (100.0 / 1024.0 * crate::overlay::geom::OVERLAY_UNITS_PER_WORLD) as f32;
            let instances = [
                CircleInstance { center: [-off, 0.0], radius_px: 8.0, paint: 0, filter: 0.0 },
                CircleInstance { center: [off, 0.0], radius_px: 8.0, paint: 0, filter: 5.0 },
            ];
            let red = Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 };
            let table = pipeline.create_paint_table(&ctx, &crate::render::compact::paint_table(&[(red, red)]));
            let buffers = pipeline.create_circle_buffers(&ctx, &instances);
            let (_model, model_group) = pipeline.create_model_bind_group(
                &ctx,
                crate::render::combined_overlay_matrix(&camera, origin).to_cols_array_2d(),
                0.0,
                0.0,
                GlobeParams::FLAT,
            );
            let pixel = |frame: &[u8], x: u32, y: u32| {
                let at = ((y * size + x) * 4) as usize;
                [frame[at], frame[at + 1], frame[at + 2], frame[at + 3]]
            };
            let (_p, all) = pipeline.create_circle_bind_group(&ctx, &table, CircleParams::NONE);
            let both = render_tile_draws(
                &ctx,
                &pipeline,
                &camera,
                size,
                wgpu::Color::BLACK,
                &[TileDraw::Circles(&buffers, &model_group, &all)],
            )
            .await;
            let east_box = [off * 0.5, -off * 0.5, off * 1.5, off * 0.5];
            let (_q, some) = pipeline.create_circle_bind_group(
                &ctx,
                &table,
                CircleParams { exclude_a: east_box, ..CircleParams::NONE },
            );
            let west_only = render_tile_draws(
                &ctx,
                &pipeline,
                &camera,
                size,
                wgpu::Color::BLACK,
                &[TileDraw::Circles(&buffers, &model_group, &some)],
            )
            .await;
            let (west, east) = ((size / 2 - 100, size / 2), (size / 2 + 100, size / 2));
            assert_eq!(pixel(&both, west.0, west.1), [255, 0, 0, 255]);
            assert_eq!(pixel(&both, east.0, east.1), [255, 0, 0, 255]);
            assert_eq!(pixel(&west_only, west.0, west.1), [255, 0, 0, 255]);
            assert_eq!(pixel(&west_only, east.0, east.1), [0, 0, 0, 255], "the excluded dot still drew");

            // The range filter (`overlay::filter`): the west dot's value is
            // 0, the east one's 5. A window of 4..=6 keeps only the east
            // one, 0..=0 only the west one (closed ends), and the same
            // instance buffer serves all three draws: moving the window is
            // a uniform write.
            let filtered = |window: [f32; 2]| {
                let (_b, group) = pipeline.create_circle_bind_group(&ctx, &table, CircleParams::NONE.with_filter(window));
                let buffers = &buffers;
                let model_group = &model_group;
                let pipeline = &pipeline;
                let ctx = &ctx;
                let camera = &camera;
                async move {
                    render_tile_draws(ctx, pipeline, camera, size, wgpu::Color::BLACK, &[TileDraw::Circles(buffers, model_group, &group)])
                        .await
                }
            };
            let east_only = filtered([4.0, 6.0]).await;
            assert_eq!(pixel(&east_only, west.0, west.1), [0, 0, 0, 255], "a value outside the window drew");
            assert_eq!(pixel(&east_only, east.0, east.1), [255, 0, 0, 255]);
            let west_end = filtered([0.0, 0.0]).await;
            assert_eq!(pixel(&west_end, west.0, west.1), [255, 0, 0, 255], "the window's end is inside it");
            assert_eq!(pixel(&west_end, east.0, east.1), [0, 0, 0, 255]);
            let none = filtered([6.0, 9.0]).await;
            assert_eq!(pixel(&none, west.0, west.1), [0, 0, 0, 255]);
            assert_eq!(pixel(&none, east.0, east.1), [0, 0, 0, 255]);

            // A packed two-field filter on the real device: the codes are
            // `bitcast` back exactly (the same instance buffer, rebuilt with
            // the code words), and each field's window gates its own dot.
            let packed = crate::overlay::filter::PointFilter::packed_from_values(&[
                ("year".into(), vec![2008.0, 2014.0], false),
                ("beds".into(), vec![3.0, 3.0], false),
            ])
            .expect("packed");
            let layout = packed.packed.clone().expect("a layout");
            let coded = [
                CircleInstance { filter: packed.value(0), ..instances[0] },
                CircleInstance { filter: packed.value(1), ..instances[1] },
            ];
            let coded_buffers = pipeline.create_circle_buffers(&ctx, &coded);
            let draw = |ranges: [Option<[f64; 2]>; 2]| {
                let (_b, group) = pipeline.create_circle_bind_group(&ctx, &table, CircleParams::NONE.with_packed_filter(layout.uniform(&ranges)));
                let (buffers, model_group, pipeline, ctx, camera) = (&coded_buffers, &model_group, &pipeline, &ctx, &camera);
                async move {
                    render_tile_draws(ctx, pipeline, camera, size, wgpu::Color::BLACK, &[TileDraw::Circles(buffers, model_group, &group)])
                        .await
                }
            };
            let late = draw([Some([2010.0, 2016.0]), None]).await;
            assert_eq!(pixel(&late, west.0, west.1), [0, 0, 0, 255], "2008 is outside 2010-2016");
            assert_eq!(pixel(&late, east.0, east.1), [255, 0, 0, 255]);
            let all = draw([None, Some([3.0, 3.0])]).await;
            assert_eq!(pixel(&all, west.0, west.1), [255, 0, 0, 255]);
            assert_eq!(pixel(&all, east.0, east.1), [255, 0, 0, 255]);
            let none = draw([Some([2006.0, 2020.0]), Some([4.0, 9.0])]).await;
            assert_eq!(pixel(&none, west.0, west.1), [0, 0, 0, 255], "the second field's window hides both");
            assert_eq!(pixel(&none, east.0, east.1), [0, 0, 0, 255]);
        });
    }

    /// The dot entry points stay inside the declared inter-stage need:
    /// `vs_dot` returns its own `DotGlobeOutput` (4 locations, for
    /// `fs_dot`) and `vs_dot_flat` its `DotFlatOutput` (3), each well
    /// under iOS's 15, and the shared `VertexOutput` the tile shader needs
    /// is untouched at 12.
    #[test]
    fn the_dot_pipeline_adds_no_inter_stage_variable() {
        let src = include_str!("shader.wgsl");
        assert!(
            src.contains("fn vs_dot(@builtin(vertex_index) vertex_index: u32) -> DotGlobeOutput"),
            "vs_dot returns DotGlobeOutput"
        );
        assert!(src.contains("fn fs_dot(input: DotGlobeOutput)"), "fs_dot reads it");
        let locations = vertex_output_locations(src);
        assert_eq!(locations.len(), 12, "12 of iOS's 15 inter-stage slots (line_progress is the tile shader's twelfth)");
        let declared = declared_needs()
            .into_iter()
            .find(|(name, _)| *name == "max_inter_stage_shader_variables")
            .expect("declared")
            .1;
        for name in ["DotFlatOutput", "DotGlobeOutput"] {
            let body = src.split_once(&format!("struct {name} {{")).unwrap_or_else(|| panic!("{name} is declared")).1;
            let body = body.split_once("\n}").expect("the struct is closed").0;
            let at: Vec<u32> = body
                .match_indices("@location(")
                .filter_map(|(at, _)| body[at + "@location(".len()..].split_once(')')?.0.trim().parse().ok())
                .collect();
            assert!(!at.is_empty(), "failed to parse {name}'s locations");
            assert!(at.len() <= 4, "{name} fills {} slots", at.len());
            assert!(u64::from(*at.iter().max().expect("non-empty")) < declared, "{name}: {at:?}");
        }
    }

    /// The dot pipelines -- every pipeline `TilePipeline::new` builds --
    /// validate on a device created with WebGL2's downlevel limits: three
    /// vertex-stage `textureLoad`s (one of an `Rgba32Uint` texture, one of
    /// the `R8Uint` dot mask), one more uniform, `vertex_index`, no vertex
    /// buffer, no storage and no compute. A live tile (a `pop_b` region) is
    /// uploaded and bound, with the 1x1 land mask.
    #[test]
    fn the_dot_pipeline_builds_under_downlevel_webgl2_limits() {
        pollster::block_on(async {
            let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
            let adapter = instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::LowPower,
                    ..Default::default()
                })
                .await
                .expect("an adapter");
            let limits = wgpu::Limits::downlevel_webgl2_defaults();
            let (device, queue) = adapter
                .request_device(&wgpu::DeviceDescriptor { required_limits: limits.clone(), ..Default::default() })
                .await
                .expect("a device at WebGL2's downlevel limits");
            let max_buffer_size = device.limits().max_buffer_size;
            let ctx = GpuContext { device, queue, max_buffer_size };
            let scope = ctx.device.push_error_scope(wgpu::ErrorFilter::Validation);
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let white = Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };
            let table = pipeline.create_paint_table(
                &ctx,
                &crate::render::compact::paint_table(&crate::render::dots::ramp_paints(&[white])),
            );
            let tile = pipeline.create_dot_tile(
                &ctx,
                &[vec![crate::render::dots::DotBlock { origin: [0.0; 2], pop: 1.0, first_dot: 0 }], vec![], vec![], vec![]],
                &[vec![2.0], vec![], vec![], vec![]],
            );
            let _dots = pipeline.create_dot_bind_group(
                &ctx,
                &table,
                &tile,
                None,
                crate::render::dots::DotParams {
                    exclude_a: crate::render::dots::DotParams::NONE_EXCLUDED,
                    exclude_b: crate::render::dots::DotParams::NONE_EXCLUDED,
                    ppd: 1.0,
                    ..bytemuck::Zeroable::zeroed()
                },
            );
            // And a real mask, relocating.
            let grid = crate::render::dot_mask::MaskGrid { zm: 10, origin: [0, 0], size: [256, 256] };
            let mask = crate::render::dot_mask::DotMaskGpu::from_texels(&ctx, grid, &vec![1u8; 256 * 256]);
            let _relocating = pipeline.create_dot_bind_group(
                &ctx,
                &table,
                &tile,
                Some(&mask),
                crate::render::dots::DotParams {
                    exclude_a: crate::render::dots::DotParams::NONE_EXCLUDED,
                    exclude_b: crate::render::dots::DotParams::NONE_EXCLUDED,
                    ppd: 1.0,
                    mask_k: 8,
                    ..bytemuck::Zeroable::zeroed()
                },
            );
            let error = scope.pop().await;
            assert!(error.is_none(), "validation error under WebGL2 limits: {error:?}");
            assert!(unmet_shader_needs(&limits).is_empty(), "{:?}", unmet_shader_needs(&limits));
        });
    }

    /// A z`z` tile's [`MaskTileDraw`] for `buffers`.
    fn mask_tile_draw(buffers: &TileBuffers, tile: crate::geo::TileId) -> MaskTileDraw<'_> {
        MaskTileDraw::for_tile(buffers, tile)
    }

    /// Renders `tiles` into a fresh mask on `grid` and reads it back.
    fn render_mask(ctx: &GpuContext, pipeline: &TilePipeline, grid: crate::render::dot_mask::MaskGrid, tiles: &[MaskTileDraw<'_>]) -> Vec<u8> {
        let mut mask = None;
        let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("mask-test") });
        let key = crate::render::dot_mask::MaskKey { grid, tiles: Vec::new() };
        let draws = pipeline.encode_dot_mask(ctx, &mut encoder, &mut mask, key.clone(), tiles);
        assert!(tiles.is_empty() || draws > 0, "the tiles drew nothing");
        ctx.queue.submit(Some(encoder.finish()));
        let mask = mask.expect("encode_dot_mask made the texture");
        assert_eq!(mask.key(), Some(&key));
        assert_eq!(mask.generation(), 1);
        mask.read_back(ctx)
    }

    /// Three overlapping squares in MVT units on an 8-unit lattice, which
    /// is one texel of a z10 tile on a zm-10 grid, so every edge sits on a
    /// texel boundary: green `800..2400`, water `1600..3200`, building
    /// `2000..3600` (both axes), ranked 2, 1 and 4.
    fn ranked_squares_plan() -> crate::style::PaintPlan {
        let rect = |a: i32, b: i32| geo_types::LineString::from(vec![(a, a), (b, a), (b, b), (a, b), (a, a)]);
        let c = Rgba { r: 0.5, g: 0.5, b: 0.5, a: 1.0 };
        let fill = |a, b| crate::style::PaintOp::Fill {
            color_from: c,
            color_to: c,
            geometry: std::sync::Arc::new(geo_types::Geometry::Polygon(geo_types::Polygon::new(rect(a, b), vec![]))),
        };
        crate::style::PaintPlan { ops: vec![fill(800, 2400), fill(1600, 3200), fill(2000, 3600)] }
    }

    /// Building > water > green > land: the last list drawn wins, and the
    /// lists go green, water, building across every tile. Split (compact
    /// body) and unsplit (legacy body) tiles alike.
    #[test]
    fn building_beats_water_beats_green_beats_land() {
        use crate::overlay::dotgrid::mask_class::{MaskRank, MaskRanks};
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let (mesh, spans) = crate::render::tessellate_plan_with_ranges(
                &ranked_squares_plan(),
                4096,
                crate::render::mesh::COORD_SCALE,
                crate::render::subdivide::Subdivision::NONE,
            );
            let ranks = MaskRanks { classified: true, ops: vec![(0, MaskRank::Green), (1, MaskRank::Water), (2, MaskRank::Building)] };
            let tile = crate::geo::TileId { z: 10, x: 500, y: 300 };
            let grid = crate::render::dot_mask::MaskGrid::plan(10.5, [500.0 / 1024.0, 300.0 / 1024.0, 501.0 / 1024.0, 301.0 / 1024.0], 0.0, 4096)
                .expect("a grid");
            assert_eq!((grid.zm, grid.origin), (10, [500 * 512, 300 * 512]), "the tile starts at texel 0");
            let split = pipeline.install_basemap_mesh_masked(&ctx, &mesh, &spans, &ranks);
            let whole = pipeline.install_mesh_masked(&ctx, &mesh, &spans, &ranks);
            assert!(whole.mask.as_ref().is_some_and(|m| m.legacy.is_some() && m.compact.is_none()));
            for buffers in [&split, &whole] {
                let texels = render_mask(&ctx, &pipeline, grid, &[mask_tile_draw(buffers, tile)]);
                assert_eq!(texels.len(), (grid.size[0] * grid.size[1]) as usize);
                // MVT (x, y) -> the texel's rank: 8 MVT units per texel.
                let at = |x: u32, y: u32| texels[((y / 8) * grid.size[0] + x / 8) as usize];
                assert_eq!(at(2200, 2200), 4, "all three overlap: building");
                assert_eq!(at(3400, 3400), 4, "building alone");
                assert_eq!(at(1800, 1800), 1, "water over green");
                assert_eq!(at(3000, 1700), 1, "water alone");
                assert_eq!(at(1000, 1000), 2, "green alone");
                assert_eq!(at(100, 100), 3, "land");
                assert_eq!(at(4000, 100), 3, "land");
                // Past the tile, inside the grid: no tile there, no data
                // (the clear); inside it every texel has data -- the tile's
                // footprint is land under its fills (DP-5 ruling).
                let no_data = crate::render::dot_mask::MASK_NO_DATA_RANK as u8;
                assert_eq!(texels[(600 * grid.size[0] + 600) as usize], no_data);
                for (i, &r) in texels.iter().enumerate() {
                    let (x, y) = (i as u32 % grid.size[0], i as u32 / grid.size[0]);
                    assert_eq!(r == no_data, x >= 512 || y >= 512, "texel ({x}, {y}) reads {r}");
                }
                assert_eq!(texels.iter().filter(|&&r| r == 4).count(), 200 * 200, "exactly the building's texels");
            }
        });
    }

    /// The grid is world-anchored: rendering the same tile into two grids
    /// whose origins differ by 256 texels gives the same texels where they
    /// overlap, byte for byte -- with edges off the texel lattice (a
    /// slanted triangle and `compact_test_fill_plan`'s rectangles, on a
    /// zm-11 grid of a z10 tile).
    #[test]
    fn two_snapped_origins_produce_identical_overlapping_texels() {
        use crate::overlay::dotgrid::mask_class::{MaskRank, MaskRanks};
        use crate::render::dot_mask::MaskGrid;
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let mut plan = compact_test_fill_plan();
            let c = Rgba { r: 0.5, g: 0.5, b: 0.5, a: 1.0 };
            plan.ops.push(crate::style::PaintOp::Fill {
                color_from: c,
                color_to: c,
                geometry: std::sync::Arc::new(geo_types::Geometry::Polygon(geo_types::Polygon::new(
                    geo_types::LineString::from(vec![(123, 457), (3011, 1333), (1777, 3901), (123, 457)]),
                    vec![],
                ))),
            });
            let (mesh, spans) = crate::render::tessellate_plan_with_ranges(
                &plan,
                4096,
                crate::render::mesh::COORD_SCALE,
                crate::render::subdivide::Subdivision::NONE,
            );
            let ranks = MaskRanks {
                classified: true,
                ops: vec![(0, MaskRank::Green), (1, MaskRank::Water), (2, MaskRank::Building), (3, MaskRank::Water)],
            };
            let tile = crate::geo::TileId { z: 10, x: 500, y: 300 };
            let buffers = pipeline.install_basemap_mesh_masked(&ctx, &mesh, &spans, &ranks);
            let a = MaskGrid::plan(11.2, [500.0 / 1024.0, 300.0 / 1024.0, 501.0 / 1024.0, 301.0 / 1024.0], 0.0, 4096).expect("a grid");
            assert_eq!(a.zm, 11);
            let b = MaskGrid { origin: [a.origin[0] + 256, a.origin[1] - 256], ..a };
            let ta = render_mask(&ctx, &pipeline, a, &[mask_tile_draw(&buffers, tile)]);
            let tb = render_mask(&ctx, &pipeline, b, &[mask_tile_draw(&buffers, tile)]);
            let (w, h) = (a.size[0] as usize, a.size[1] as usize);
            let mut compared = 0usize;
            let mut ranked = 0usize;
            for y in 0..h - 256 {
                for x in 256..w {
                    // World texel (a.origin + (x, y)) is b's (x - 256, y + 256).
                    let ra = ta[y * w + x];
                    assert_eq!(ra, tb[(y + 256) * w + (x - 256)], "texel ({x}, {y}) of the first grid");
                    compared += 1;
                    ranked += usize::from(ra != 3);
                }
            }
            assert!(compared > 500_000 && ranked > 100_000, "{compared} compared, {ranked} ranked");
        });
    }

    /// A tile whose rect misses the grid is culled: no draw, the mask stays
    /// cleared land; the same tile on the grid draws.
    #[test]
    fn a_tile_off_the_mask_grid_is_culled() {
        use crate::overlay::dotgrid::mask_class::{MaskRank, MaskRanks};
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let (mesh, spans) = crate::render::tessellate_plan_with_ranges(
                &ranked_squares_plan(),
                4096,
                crate::render::mesh::COORD_SCALE,
                crate::render::subdivide::Subdivision::NONE,
            );
            let ranks = MaskRanks { classified: true, ops: vec![(0, MaskRank::Green), (1, MaskRank::Water), (2, MaskRank::Building)] };
            let buffers = pipeline.install_basemap_mesh_masked(&ctx, &mesh, &spans, &ranks);
            let grid = crate::render::dot_mask::MaskGrid::plan(10.0, [500.0 / 1024.0, 300.0 / 1024.0, 501.0 / 1024.0, 301.0 / 1024.0], 0.0, 4096)
                .expect("a grid");
            let encode = |tile: crate::geo::TileId| {
                let mut mask = None;
                let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
                let key = crate::render::dot_mask::MaskKey { grid, tiles: vec![(tile, 1)] };
                let draws = pipeline.encode_dot_mask(&ctx, &mut encoder, &mut mask, key, &[mask_tile_draw(&buffers, tile)]);
                ctx.queue.submit(Some(encoder.finish()));
                (draws, mask.expect("made").read_back(&ctx))
            };
            let (on, texels) = encode(crate::geo::TileId { z: 10, x: 500, y: 300 });
            let no_data = crate::render::dot_mask::MASK_NO_DATA_RANK as u8;
            assert!(on > 0 && texels.iter().any(|&r| r != no_data));
            // The tile's footprint outside its ranked squares is land, drawn
            // over the no-data clear.
            assert!(texels.contains(&(MaskRank::Land as u8)), "the tile's footprint reads land");
            let (off, texels) = encode(crate::geo::TileId { z: 10, x: 520, y: 300 });
            assert_eq!(off, 0, "a tile 19 tiles east of the grid is culled");
            assert!(texels.iter().all(|&r| r == no_data), "no tile covers the grid: all no data");
        });
    }

    /// The mask entry points share `@group(0) @binding(0)` with `camera` in
    /// the module; that is safe only while they reference `mask_draw` and
    /// no other global, and call no helper that could.
    #[test]
    fn the_mask_entry_points_reference_only_mask_draw() {
        let src = include_str!("shader.wgsl");
        let globals: Vec<&str> = src
            .lines()
            .filter(|l| l.trim_start().starts_with("@group("))
            .filter_map(|l| l.split_once(" var")?.1.split_once(':')?.0.rsplit([' ', '>']).next())
            .map(str::trim)
            .collect();
        assert!(globals.contains(&"camera") && globals.contains(&"mask_draw"), "{globals:?}");
        for entry in ["fn vs_mask_compact(", "fn vs_mask_legacy(", "fn fs_mask("] {
            let body = src.split_once(entry).unwrap_or_else(|| panic!("{entry} exists")).1;
            let body = body.split_once("\n}").expect("closed").0;
            assert!(body.contains("mask_draw."), "{entry} reads mask_draw");
            let words: Vec<&str> = body.split(|c: char| !(c.is_alphanumeric() || c == '_')).collect();
            for g in globals.iter().filter(|g| **g != "mask_draw") {
                assert!(!words.contains(g), "{entry} references {g}");
            }
            // Calls: only type constructors and builtins, no helper.
            for (at, _) in body.match_indices('(') {
                let head = body[..at].trim_end();
                let name = head.rsplit(|c: char| !(c.is_alphanumeric() || c == '_' || c == '<' || c == '>')).next().unwrap_or("");
                assert!(
                    name.is_empty() || name.starts_with("vec") || name.starts_with("@location") || name == "location" || name == "builtin",
                    "{entry} calls {name}"
                );
            }
        }
    }

    /// The mask pipelines validate on a device at WebGL2's downlevel limits,
    /// and a pass into an `R8Uint` target encodes and submits without a
    /// validation error. (wgpu-hal's GLES backend lists `R8Uint` as
    /// colour-renderable and sampled -- GLES 3.0's `R8UI` -- so no
    /// `R8Unorm` fallback is needed.)
    #[test]
    fn the_mask_pipelines_build_under_downlevel_webgl2_limits() {
        use crate::overlay::dotgrid::mask_class::{MaskRank, MaskRanks};
        pollster::block_on(async {
            let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
            let adapter = instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::LowPower,
                    ..Default::default()
                })
                .await
                .expect("an adapter");
            let limits = wgpu::Limits::downlevel_webgl2_defaults();
            let (device, queue) = adapter
                .request_device(&wgpu::DeviceDescriptor { required_limits: limits.clone(), ..Default::default() })
                .await
                .expect("a device at WebGL2's downlevel limits");
            let max_buffer_size = device.limits().max_buffer_size;
            let ctx = GpuContext { device, queue, max_buffer_size };
            let scope = ctx.device.push_error_scope(wgpu::ErrorFilter::Validation);
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let (mesh, spans) = crate::render::tessellate_plan_with_ranges(
                &ranked_squares_plan(),
                4096,
                crate::render::mesh::COORD_SCALE,
                crate::render::subdivide::Subdivision::NONE,
            );
            let ranks = MaskRanks { classified: true, ops: vec![(0, MaskRank::Green), (1, MaskRank::Water), (2, MaskRank::Building)] };
            let split = pipeline.install_basemap_mesh_masked(&ctx, &mesh, &spans, &ranks);
            let whole = pipeline.install_mesh_masked(&ctx, &mesh, &spans, &ranks);
            let tile = crate::geo::TileId { z: 10, x: 500, y: 300 };
            let grid = crate::render::dot_mask::MaskGrid::plan(10.0, [500.0 / 1024.0, 300.0 / 1024.0, 501.0 / 1024.0, 301.0 / 1024.0], 0.0, 2048)
                .expect("a grid");
            let mut mask = None;
            let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
            pipeline.encode_dot_mask(
                &ctx,
                &mut encoder,
                &mut mask,
                crate::render::dot_mask::MaskKey { grid, tiles: vec![(tile, 1)] },
                &[mask_tile_draw(&split, tile), mask_tile_draw(&whole, tile)],
            );

            ctx.queue.submit(Some(encoder.finish()));
            let _ = pipeline.land_mask_view();
            ctx.device.poll(wgpu::PollType::wait_indefinitely()).expect("the device polls");
            let error = scope.pop().await;
            assert!(error.is_none(), "validation error under WebGL2 limits: {error:?}");
            assert!(unmet_shader_needs(&limits).is_empty(), "{:?}", unmet_shader_needs(&limits));
        });
    }

    /// DP-8: the mask texture is never a copy source outside test and probe
    /// builds. `dot_mask.rs`'s only `COPY_SRC` sits in an item gated
    /// `#[cfg(any(test, feature = "probe"))]`, and so does `read_back`.
    #[test]
    fn the_mask_texture_has_no_copy_src_outside_test_and_probe() {
        let src = include_str!("dot_mask.rs");
        let gate = r#"#[cfg(any(test, feature = "probe"))]"#;
        let lines: Vec<&str> = src.lines().collect();
        let gated = |i: usize| {
            // The attribute on the item this line opens (doc comments and
            // other attributes may sit between).
            lines[..i].iter().rev().map(|l| l.trim()).take_while(|l| l.starts_with("#[") || l.starts_with("///")).any(|l| l == gate)
        };
        let copy_src: Vec<usize> = lines.iter().enumerate().filter(|(_, l)| l.contains("COPY_SRC")).map(|(i, _)| i).collect();
        assert_eq!(copy_src.len(), 1, "dot_mask.rs names COPY_SRC once: {copy_src:?}");
        assert!(gated(copy_src[0]), "line {}: `{}` is not in a test/probe-gated item", copy_src[0] + 1, lines[copy_src[0]]);
        let read_back: Vec<usize> = lines.iter().enumerate().filter(|(_, l)| l.contains("fn read_back")).map(|(i, _)| i).collect();
        assert_eq!(read_back.len(), 1);
        assert!(gated(read_back[0]), "read_back is not test/probe-gated");
    }

    /// **The WGSL PCG is the CPU PCG, bit for bit.** 64 cells of `pop = 1`
    /// at `ppd = 1`, 64 px each, one dot per cell: every dot's lit-pixel
    /// centroid lands within 1 px of where `overlay::dotgrid::hash`
    /// puts it. A hash that differed in one bit would scatter the dots
    /// across their cells.
    #[test]
    fn wgsl_pcg_matches_the_cpu_pcg() {
        use crate::overlay::dotgrid::hash;
        use crate::render::dots::{DotBlock, DotParams};
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 512u32;
            let zoom = 10.0f64;
            let camera = crate::render::Camera {
                center_lon: 0.0,
                center_lat: 0.0,
                zoom,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: size as f32,
                viewport_height_px: size as f32,
                fov_y: crate::render::Camera::DEFAULT_FOV_Y,
            };
            let origin_world = crate::overlay::geom::lon_lat_to_world(0.0, 0.0);
            let lyon_per_world_unit = crate::overlay::geom::OVERLAY_UNITS_PER_WORLD;
            let px_per_world_unit = 2f64.powf(zoom);
            let lyon_per_px = lyon_per_world_unit / px_per_world_unit;
            let cell_width = (64.0 * lyon_per_px) as f32;
            let unit_world_per_lyon = 1.0 / (lyon_per_world_unit * f64::from(crate::render::WORLD_SIZE));
            let tile_origin_y = origin_world.1 / f64::from(crate::render::WORLD_SIZE);
            // SW corners: cell (i, j) spans x in [i-4, i-3) and y in
            // [j-4, j-3) cells from the centre, so its SW corner is at
            // (i-4, j-3) -- y grows south.
            let blocks: Vec<DotBlock> = (0..8)
                .flat_map(|j| (0..8).map(move |i| (i, j)))
                .map(|(i, j)| DotBlock {
                    origin: [(i as f32 - 4.0) * cell_width, (j as f32 - 3.0) * cell_width],
                    pop: 1.0,
                    first_dot: 0,
                })
                .collect();
            let expected: Vec<Option<(f64, f64)>> = blocks
                .iter()
                .map(|b| {
                    let key = hash::cell_key(b.origin);
                    let (h_t, h_u, h_v) = hash::dot_hashes(key, 0);
                    if hash::dot_alpha(b.pop, 1.0, 0, key) < 0.5 {
                        return None; // too faint to centroid reliably
                    }
                    let _ = h_t;
                    let y_world = tile_origin_y + f64::from(b.origin[1]) * unit_world_per_lyon;
                    let stretch = (std::f64::consts::PI * (1.0 - 2.0 * y_world)).cosh();
                    let x = f64::from(b.origin[0]) + f64::from(h_u) * f64::from(cell_width);
                    let y = f64::from(b.origin[1]) - f64::from(h_v) * f64::from(cell_width) * stretch;
                    Some((f64::from(size) / 2.0 + x / lyon_per_px, f64::from(size) / 2.0 + y / lyon_per_px))
                })
                .collect();
            let empty: [Vec<DotBlock>; 4] = [blocks, Vec::new(), Vec::new(), Vec::new()];
            let tile = pipeline.create_dot_tile(&ctx, &empty, &Default::default());
            let white = Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };
            let table = pipeline.create_paint_table(
                &ctx,
                &crate::render::compact::paint_table(&crate::render::dots::ramp_paints(&[white])),
            );
            let dot_group = pipeline.create_dot_bind_group(
                &ctx,
                &table,
                &tile,
                None,
                DotParams {
                    exclude_a: DotParams::NONE_EXCLUDED,
                    exclude_b: DotParams::NONE_EXCLUDED,
                    ppd: 1.0,
                    cell_width,
                    tile_origin_y: tile_origin_y as f32,
                    radius_px: 3.0,
                    t: 0.0,
                    ramp_row: 0,
                    live: 0,
                    unit_world_per_lyon: unit_world_per_lyon as f32,
                    ..bytemuck::Zeroable::zeroed()
                },
            );
            let (_model, model_group) = pipeline.create_model_bind_group(
                &ctx,
                crate::render::combined_overlay_matrix(&camera, origin_world).to_cols_array_2d(),
                0.0,
                0.0,
                GlobeParams::FLAT,
            );
            let frame = render_tile_draws(
                &ctx,
                &pipeline,
                &camera,
                size,
                wgpu::Color::BLACK,
                &[TileDraw::Dots(&dot_group, &model_group, false)],
            )
            .await;
            let mut checked = 0;
            for (n, want) in expected.iter().enumerate() {
                let Some((ex, ey)) = *want else { continue };
                // Skip a dot another dot sits within 16 px of: its window
                // would average the two.
                let crowded = expected.iter().enumerate().any(|(m, o)| {
                    m != n && o.is_some_and(|(ox, oy)| (ox - ex).hypot(oy - ey) < 16.0)
                });
                if crowded {
                    continue;
                }
                let (mut sum, mut sx, mut sy) = (0.0f64, 0.0f64, 0.0f64);
                for y in (ey as i64 - 8).max(0)..=(ey as i64 + 8).min(i64::from(size) - 1) {
                    for x in (ex as i64 - 8).max(0)..=(ex as i64 + 8).min(i64::from(size) - 1) {
                        let v = f64::from(frame[((y as u32 * size + x as u32) * 4) as usize]);
                        sum += v;
                        sx += v * (x as f64 + 0.5);
                        sy += v * (y as f64 + 0.5);
                    }
                }
                assert!(sum > 0.0, "dot {n}: nothing lit within 8 px of the CPU position ({ex:.1}, {ey:.1})");
                let (cx, cy) = (sx / sum, sy / sum);
                assert!(
                    (cx - ex).hypot(cy - ey) <= 1.0,
                    "dot {n}: GPU centroid ({cx:.2}, {cy:.2}) vs CPU ({ex:.2}, {ey:.2})"
                );
                checked += 1;
            }
            assert!(checked >= 20, "only {checked} of 64 dots were checkable");

            // `vs_dot_flat` is `vs_dot`'s flat path with a leaner output:
            // on this flat frame it must draw the same dots.
            let lean = render_tile_draws(
                &ctx,
                &pipeline,
                &camera,
                size,
                wgpu::Color::BLACK,
                &[TileDraw::Dots(&dot_group, &model_group, true)],
            )
            .await;
            let worst = frame.iter().zip(&lean).map(|(a, b)| a.abs_diff(*b)).max().unwrap_or(0);
            let differ = frame.as_chunks::<4>().0.iter().zip(lean.as_chunks::<4>().0.iter()).filter(|(a, b)| a != b).count();
            assert!(worst <= 2, "vs_dot_flat differs from vs_dot by up to {worst} levels ({differ} pixels)");
        });
    }

    /// **Dense cells draw smaller dots** (the spec's "shrinking by up to
    /// 30 % with density"; the user's "dots and big pixels collide"). Two
    /// sparse cells (under 1 person per km^2, ramp level 0) on the left of
    /// the frame and two dense ones (10^9, level 1) on the right, one dot
    /// each at `radius_px` 8 and `radius_shrink` 0.3: a dense dot's lit
    /// area is `0.7^2` of a sparse one's, as `dot_radius_px` says.
    #[test]
    fn dense_cells_draw_dots_shrunk_by_the_density_ramp() {
        use crate::render::dots::{dot_radius_px, DotBlock, DotParams};
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 512u32;
            let zoom = 10.0f64;
            let camera = crate::render::Camera {
                center_lon: 0.0,
                center_lat: 0.0,
                zoom,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: size as f32,
                viewport_height_px: size as f32,
                fov_y: crate::render::Camera::DEFAULT_FOV_Y,
            };
            let origin_world = crate::overlay::geom::lon_lat_to_world(0.0, 0.0);
            let lyon_per_world_unit = crate::overlay::geom::OVERLAY_UNITS_PER_WORLD;
            let lyon_per_px = lyon_per_world_unit / 2f64.powf(zoom);
            let cell_width = (64.0 * lyon_per_px) as f32;
            let unit_world_per_lyon = 1.0 / (lyon_per_world_unit * f64::from(crate::render::WORLD_SIZE));
            // Cells 64 px wide: sparse at x in [-256, -128), dense at
            // [64, 192); a dot of radius 8 stays 56 px clear of x = 0.
            let cell = |col: f32, row: f32, pop: f32| DotBlock {
                origin: [col * cell_width, row * cell_width],
                pop,
                first_dot: 0,
            };
            let blocks = vec![
                cell(-4.0, -1.0, 1e-3),
                cell(-3.0, 2.0, 1e-3),
                cell(1.0, -1.0, 1e9),
                cell(2.0, 2.0, 1e9),
            ];
            let tile = pipeline.create_dot_tile(&ctx, &[blocks, Vec::new(), Vec::new(), Vec::new()], &Default::default());
            let white = Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };
            let table = pipeline.create_paint_table(
                &ctx,
                &crate::render::compact::paint_table(&crate::render::dots::ramp_paints(&[white])),
            );
            let (radius, shrink) = (8.0f32, 0.3f32);
            let dot_group = pipeline.create_dot_bind_group(
                &ctx,
                &table,
                &tile,
                None,
                DotParams {
                    exclude_a: DotParams::NONE_EXCLUDED,
                    exclude_b: DotParams::NONE_EXCLUDED,
                    // Every cell's one dot is alive and fully faded in.
                    ppd: 1e-5,
                    cell_width,
                    tile_origin_y: (origin_world.1 / f64::from(crate::render::WORLD_SIZE)) as f32,
                    radius_px: radius,
                    unit_world_per_lyon: unit_world_per_lyon as f32,
                    radius_shrink: shrink,
                    ..bytemuck::Zeroable::zeroed()
                },
            );
            let (_model, model_group) = pipeline.create_model_bind_group(
                &ctx,
                crate::render::combined_overlay_matrix(&camera, origin_world).to_cols_array_2d(),
                0.0,
                0.0,
                GlobeParams::FLAT,
            );
            for flat in [false, true] {
                let frame = render_tile_draws(
                    &ctx,
                    &pipeline,
                    &camera,
                    size,
                    wgpu::Color::BLACK,
                    &[TileDraw::Dots(&dot_group, &model_group, flat)],
                )
                .await;
                // Coverage-weighted area, each half of the frame.
                let (mut left, mut right) = (0.0f64, 0.0f64);
                for (i, px) in frame.as_chunks::<4>().0.iter().enumerate() {
                    let v = f64::from(px[0]) / 255.0;
                    if (i as u32 % size) < size / 2 { left += v } else { right += v }
                }
                let sparse = std::f64::consts::PI * f64::from(radius).powi(2) * 2.0;
                let dense_r = dot_radius_px(radius, shrink, 1.0);
                let dense = std::f64::consts::PI * f64::from(dense_r).powi(2) * 2.0;
                assert!((left / sparse - 1.0).abs() < 0.15, "flat={flat}: sparse dots cover {left:.0} px, expected {sparse:.0}");
                assert!((right / dense - 1.0).abs() < 0.15, "flat={flat}: dense dots cover {right:.0} px, expected {dense:.0} (radius {dense_r})");
            }
        });
    }

    /// Eight 64-px cells of pop 16, 32, ... 2048 at ppd 1, radius 3, white
    /// on black, two rows of four with a cell's gap between, drawn with
    /// `overlap_gamma` `gamma` through `vs_dot` or `vs_dot_flat`. Returns
    /// the frame, its width, and each cell's 64x64 pixel window
    /// `(x0, y0)`, densest last.
    async fn overlap_fade_frame(gamma: f32, flat: bool) -> (Vec<u8>, u32, Vec<(u32, u32)>) {
        use crate::render::dots::{DotBlock, DotParams};
        let ctx = GpuContext::new_headless().await.expect("headless device");
        let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
        let size = 512u32;
        let zoom = 10.0f64;
        let camera = crate::render::Camera {
            center_lon: 0.0,
            center_lat: 0.0,
            zoom,
            pitch: 0.0,
            bearing: 0.0,
            viewport_width_px: size as f32,
            viewport_height_px: size as f32,
            fov_y: crate::render::Camera::DEFAULT_FOV_Y,
        };
        let origin_world = crate::overlay::geom::lon_lat_to_world(0.0, 0.0);
        let lyon_per_world_unit = crate::overlay::geom::OVERLAY_UNITS_PER_WORLD;
        let lyon_per_px = lyon_per_world_unit / 2f64.powf(zoom);
        let cell_width = (64.0 * lyon_per_px) as f32;
        let unit_world_per_lyon = 1.0 / (lyon_per_world_unit * f64::from(crate::render::WORLD_SIZE));
        let mut sixty_fours = Vec::new();
        let mut windows = Vec::new();
        for n in 0..8u32 {
            let pop = 16.0 * 2f32.powi(n as i32);
            // Columns -4, -2, 0, 2 (cells), SW corners at rows -1 and 2:
            // x in [-256, -192) ... px from the centre, y in [-128, -64)
            // and [64, 128) (the dot goes NORTH of the SW corner).
            let (col, row) = (-4.0 + 2.0 * (n % 4) as f32, if n < 4 { -1.0 } else { 2.0 });
            let origin = [col * cell_width, row * cell_width];
            for b in 0..(pop / 64.0).ceil() as u32 {
                sixty_fours.push(DotBlock { origin, pop, first_dot: 64 * b });
            }
            let x0 = (size as f32 / 2.0 + col * 64.0) as u32;
            let y0 = (size as f32 / 2.0 + row * 64.0 - 64.0) as u32;
            windows.push((x0, y0));
        }
        let tile = pipeline.create_dot_tile(
            &ctx,
            &[Vec::new(), Vec::new(), Vec::new(), sixty_fours],
            &Default::default(),
        );
        let white = Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };
        let table = pipeline.create_paint_table(
            &ctx,
            &crate::render::compact::paint_table(&crate::render::dots::ramp_paints(&[white])),
        );
        let dot_group = pipeline.create_dot_bind_group(
            &ctx,
            &table,
            &tile,
            None,
            DotParams {
                exclude_a: DotParams::NONE_EXCLUDED,
                exclude_b: DotParams::NONE_EXCLUDED,
                ppd: 1.0,
                cell_width,
                tile_origin_y: (origin_world.1 / f64::from(crate::render::WORLD_SIZE)) as f32,
                radius_px: 3.0,
                unit_world_per_lyon: unit_world_per_lyon as f32,
                radius_shrink: 0.0,
                overlap_gamma: gamma,
                ..bytemuck::Zeroable::zeroed()
            },
        );
        let (_model, model_group) = pipeline.create_model_bind_group(
            &ctx,
            crate::render::combined_overlay_matrix(&camera, origin_world).to_cols_array_2d(),
            0.0,
            0.0,
            GlobeParams::FLAT,
        );
        let frame = render_tile_draws(
            &ctx,
            &pipeline,
            &camera,
            size,
            wgpu::Color::BLACK,
            &[TileDraw::Dots(&dot_group, &model_group, flat)],
        )
        .await;
        (frame, size, windows)
    }

    /// The mean of `R / 255` over the 64x64 window at `(x0, y0)`.
    fn mean_ink(frame: &[u8], size: u32, (x0, y0): (u32, u32)) -> f64 {
        let mut sum = 0.0;
        for y in y0..y0 + 64 {
            for x in x0..x0 + 64 {
                sum += f64::from(frame[((y * size + x) * 4) as usize]) / 255.0;
            }
        }
        sum / 4096.0
    }

    /// **The overlap fade draws a gradient, not a block** (controller
    /// ruling A). Eight 64-px cells of pop 16, 32, ... 2048 at ppd 1,
    /// radius 3, white on black. With gamma 0.7 the mean ink per cell rises
    /// strictly with pop and the densest stays under 0.95; with gamma 0 the
    /// densest is above 0.99 -- the solid square this guards against.
    #[test]
    fn denser_cells_draw_more_ink_under_the_overlap_fade() {
        pollster::block_on(async {
            for flat in [false, true] {
                let (frame, size, windows) = overlap_fade_frame(0.7, flat).await;
                let inks: Vec<f64> = windows.iter().map(|&w| mean_ink(&frame, size, w)).collect();
                for pair in inks.windows(2) {
                    assert!(pair[1] > pair[0], "flat={flat}: ink must rise with pop: {inks:.3?}");
                }
                assert!(inks[7] < 0.95, "flat={flat}: the densest cell saturates under the fade: {inks:.3?}");
                let (off, size, windows) = overlap_fade_frame(0.0, flat).await;
                let solid = mean_ink(&off, size, windows[7]);
                assert!(solid > 0.99, "flat={flat}: without the fade the densest cell is solid, got {solid:.3}");
            }
        });
    }

    /// **The overlap fade changes alpha, never which dots exist**: no
    /// pixel is lit (R > 0) at gamma 0.7 that is dark at gamma 0, and every
    /// pixel inked at gamma 0 is still inked at 0.7 (the floor keeps every
    /// live dot's alpha at 0.1 or more). A rim pixel whose antialiased
    /// coverage rounds to 1 or 2 levels at gamma 0 can round to 0 at alpha
    /// 0.1-0.16, so "inked" is R >= 26 -- a tenth of full ink, which the
    /// floor maps to at least 2.6 levels -- not R > 0.
    ///
    /// The densest cell (2048 people, radius 3, 64 px) is left out: at
    /// gamma 0 its ink (14) is over `dots::INK_CAP`, so the saturation cap
    /// paints only the prefix whose ink is the cap -- its core is still
    /// solid (`denser_cells_draw_more_ink_under_the_overlap_fade`), but a
    /// rim pixel one dropped dot's antialias grazed is not. Which dots are
    /// ALIVE is still the fade's to leave alone; which are PAINTED is the
    /// cap's, and the cap reads the alpha.
    #[test]
    fn the_overlap_fade_changes_alpha_not_which_dots_exist() {
        let ink = |pop: f32| pop * std::f32::consts::PI * 9.0 / (64.0 * 64.0);
        assert!(ink(1024.0) < crate::render::dots::INK_CAP && ink(2048.0) > crate::render::dots::INK_CAP);
        pollster::block_on(async {
            for flat in [false, true] {
                let (off, size, windows) = overlap_fade_frame(0.0, flat).await;
                let (on, _, _) = overlap_fade_frame(0.7, flat).await;
                let (cx, cy) = windows[7];
                let capped = |i: usize| {
                    let (x, y) = ((i as u32 % size) as i64, (i as u32 / size) as i64);
                    (i64::from(cx) - 8..i64::from(cx) + 72).contains(&x) && (i64::from(cy) - 8..i64::from(cy) + 72).contains(&y)
                };
                let (mut appeared, mut vanished, mut lit) = (0, 0, 0);
                for (i, (a, b)) in off.as_chunks::<4>().0.iter().zip(on.as_chunks::<4>().0.iter()).enumerate() {
                    if capped(i) {
                        continue;
                    }
                    appeared += usize::from(a[0] == 0 && b[0] > 0);
                    vanished += usize::from(a[0] >= 26 && b[0] == 0);
                    lit += usize::from(b[0] > 0);
                }
                assert_eq!(appeared, 0, "flat={flat}: {appeared} pixels are lit only under the fade");
                assert_eq!(vanished, 0, "flat={flat}: {vanished} inked pixels went dark under the fade");
                assert!(lit > 0, "flat={flat}: nothing drew");
            }
        });
    }

    /// A dot tile's live mode, as the M1/M2 tests below build one: bucket 1
    /// holds `(pop, first_dot)` blocks at distinct origins.
    fn dot_tile_blocks(blocks: &[(f32, u32)]) -> [Vec<crate::render::dots::DotBlock>; 4] {
        let ones = blocks
            .iter()
            .enumerate()
            .map(|(i, &(pop, first_dot))| crate::render::dots::DotBlock { origin: [i as f32 * 10.0, 0.0], pop, first_dot })
            .collect();
        [ones, Vec::new(), Vec::new(), Vec::new()]
    }

    /// **The vertex count and the uniform's layout move together.** A group
    /// created at `ppd = 1` and rewritten to `ppd = 7.5` draws exactly what
    /// a group created at 7.5 draws; a second group over the SAME tile, left at
    /// 1, keeps its own count.
    #[test]
    fn rewriting_dot_params_moves_the_draw_count_with_the_layout() {
        use crate::render::dots::DotParams;
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let white = Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };
            let table = pipeline.create_paint_table(
                &ctx,
                &crate::render::compact::paint_table(&crate::render::dots::ramp_paints(&[white])),
            );
            // Dots 0..16 of a 16-person cell, one block each: at ppd 7.5
            // only dots 0, 1 and 2 can live (first_dot < 16 / 7.5 = 2.13).
            let blocks: Vec<(f32, u32)> = (0..16).map(|d| (16.0, d)).collect();
            let tile = pipeline.create_dot_tile(&ctx, &dot_tile_blocks(&blocks), &Default::default());
            let at = |ppd: f32| DotParams { ppd, ..bytemuck::Zeroable::zeroed() };
            let moved = pipeline.create_dot_bind_group(&ctx, &table, &tile, None, at(1.0));
            let kept = pipeline.create_dot_bind_group(&ctx, &table, &tile, None, at(1.0));
            assert_eq!(moved.vertices(), 16 * 3);
            pipeline.write_dot_params(&ctx, &moved, &tile, at(7.5));
            let fresh = pipeline.create_dot_bind_group(&ctx, &table, &tile, None, at(7.5));
            assert_eq!(moved.vertices(), fresh.vertices());
            assert_eq!(moved.vertices(), 3 * 3, "blocks 3..16 are dead at ppd 7.5 and skipped");
            assert_eq!(kept.vertices(), 16 * 3, "the other group over the same tile keeps its own layout");
            let (laid_out, vertices) = tile.layout(at(7.5));
            assert_eq!(vertices, fresh.vertices());
            assert_eq!(laid_out.seg_vertex, [0, 9, 9, 9]);
        });
    }

    /// **Overflow drops the sparsest blocks, from any bucket, and counts
    /// them.** Capacity 3 against four blocks: the one whose first dot
    /// dies first (bucket 4, key 10 / 8) goes, and the dense 64-block
    /// stays.
    #[test]
    fn a_dot_tile_over_capacity_drops_the_blocks_that_die_first() {
        use crate::render::dots::{DotBlock, DotParams};
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let block = |x: f32, pop: f32, first_dot: u32| DotBlock { origin: [x, 0.0], pop, first_dot };
            let buckets = [
                vec![block(0.0, 1.0, 0), block(1.0, 1.0, 0)],
                vec![block(2.0, 10.0, 8)],
                Vec::new(),
                vec![block(3.0, 70.0, 0)],
            ];
            let before = crate::render::dots::dot_blocks_dropped_total();
            let tile = pipeline.create_dot_tile_capped(&ctx, &buckets, &Default::default(), &Default::default(), &Default::default(), 3);
            assert_eq!(tile.dropped_blocks, 1);
            assert!(crate::render::dots::dot_blocks_dropped_total() > before);
            let (laid_out, vertices) = tile.layout(DotParams { ppd: 1e-3, ..bytemuck::Zeroable::zeroed() });
            assert_eq!(vertices, 3 * (2 + 64), "both 1-blocks and the 64-block are kept");
            assert_eq!(laid_out.seg_block, [0, 2, 2, 2]);
        });
    }

    /// The relocation tests' shared frame: 512 px at z10 over (0, 0), the
    /// dot tile's lyon origin at the frame centre, 64-px cells, white on
    /// black, no shrink, no fade, and a z10 mask grid (one texel per pixel)
    /// whose texel 512 sits under the frame centre.
    struct RelocationFixture {
        size: u32,
        camera: crate::render::Camera,
        origin_world: (f64, f64),
        cell_width: f32,
        lyon_per_px: f64,
        unit_world_per_lyon: f32,
        tile_origin_y: f32,
        grid: crate::render::dot_mask::MaskGrid,
        lookup: crate::render::dot_mask::MaskTileLookup,
    }

    impl RelocationFixture {
        /// `mask_width`: the grid's width in texels from 512 px west of
        /// the centre (1024 covers the whole frame, 512 only its west half).
        fn new(mask_width: u32) -> Self {
            let size = 512u32;
            let zoom = 10.0f64;
            let camera = crate::render::Camera {
                center_lon: 0.0,
                center_lat: 0.0,
                zoom,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: size as f32,
                viewport_height_px: size as f32,
                fov_y: crate::render::Camera::DEFAULT_FOV_Y,
            };
            let origin_world = crate::overlay::geom::lon_lat_to_world(0.0, 0.0);
            let lyon_per_world_unit = crate::overlay::geom::OVERLAY_UNITS_PER_WORLD;
            let lyon_per_px = lyon_per_world_unit / 2f64.powf(zoom);
            let unit_world_per_lyon = 1.0 / (lyon_per_world_unit * f64::from(crate::render::WORLD_SIZE));
            let world = f64::from(crate::render::WORLD_SIZE);
            let centre_texel = (0.5 * 2f64.powi(19)) as i64;
            let grid = crate::render::dot_mask::MaskGrid {
                zm: 10,
                origin: [centre_texel - 512, centre_texel - 512],
                size: [mask_width, 1024],
            };
            let lookup = grid.for_tile((origin_world.0 / world, origin_world.1 / world), unit_world_per_lyon);
            assert_eq!((lookup.int_offset, lookup.frac), ([512, 512], [0.0, 0.0]), "the tile origin is texel 512");
            Self {
                size,
                camera,
                origin_world,
                cell_width: (64.0 * lyon_per_px) as f32,
                lyon_per_px,
                unit_world_per_lyon: unit_world_per_lyon as f32,
                tile_origin_y: (origin_world.1 / world) as f32,
                grid,
                lookup,
            }
        }

        fn params(&self, radius_px: f32, mask_k: u32) -> crate::render::dots::DotParams {
            crate::render::dots::DotParams {
                exclude_a: crate::render::dots::DotParams::NONE_EXCLUDED,
                exclude_b: crate::render::dots::DotParams::NONE_EXCLUDED,
                ppd: 1.0,
                cell_width: self.cell_width,
                tile_origin_y: self.tile_origin_y,
                radius_px,
                unit_world_per_lyon: self.unit_world_per_lyon,
                ..bytemuck::Zeroable::zeroed()
            }
            .relocating(mask_k, self.lookup)
        }

        /// The mask rank at a texel of `texels` (this grid's), land outside.
        fn rank(&self, texels: &[u8], t: [i32; 2]) -> u8 {
            let [w, h] = self.grid.size;
            if t[0] < 0 || t[1] < 0 || t[0] >= w as i32 || t[1] >= h as i32 {
                return crate::render::dot_mask::MASK_CLEAR_RANK as u8;
            }
            texels[t[1] as usize * w as usize + t[0] as usize]
        }

        /// The CPU mirror of `dot_slot`'s placement: dot `dot` of the cell
        /// at `origin`, relocated over `texels` with `k` candidates. Returns
        /// its tile lyon position, the candidate index chosen and every
        /// candidate's rank.
        fn place(&self, origin: [f32; 2], dot: u32, k: u32, texels: &[u8]) -> ([f32; 2], usize, Vec<u8>) {
            use crate::overlay::dotgrid::hash;
            let key = hash::cell_key(origin);
            let y_world = self.tile_origin_y + origin[1] * self.unit_world_per_lyon;
            let stretch = (std::f32::consts::PI * (1.0 - 2.0 * y_world)).cosh();
            let at = |(u, v): (f32, f32)| [origin[0] + u * self.cell_width, origin[1] + -v * self.cell_width * stretch];
            let candidates: Vec<[f32; 2]> = hash::dot_candidates(key, dot, k).map(at).collect();
            let ranks: Vec<u8> = candidates.iter().map(|&p| self.rank(texels, self.lookup.texel(p))).collect();
            let chosen = hash::choose(ranks.iter().copied());
            (candidates[chosen], chosen, ranks)
        }

        /// A tile lyon point in frame pixels.
        fn px(&self, p: [f32; 2]) -> (f64, f64) {
            let half = f64::from(self.size) / 2.0;
            (half + f64::from(p[0]) / self.lyon_per_px, half + f64::from(p[1]) / self.lyon_per_px)
        }

        /// The frame `blocks` draw with `params` over `mask`, and the draw's
        /// vertex count.
        async fn frame(
            &self,
            ctx: &GpuContext,
            pipeline: &TilePipeline,
            blocks: &[Vec<crate::render::dots::DotBlock>; 4],
            mask: Option<&crate::render::dot_mask::DotMaskGpu>,
            params: crate::render::dots::DotParams,
            flat: bool,
        ) -> (Vec<u8>, u32) {
            let tile = pipeline.create_dot_tile(ctx, blocks, &Default::default());
            let white = Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };
            let table = pipeline
                .create_paint_table(ctx, &crate::render::compact::paint_table(&crate::render::dots::ramp_paints(&[white])));
            let group = pipeline.create_dot_bind_group(ctx, &table, &tile, mask, params);
            assert_eq!(group.mask_generation(), mask.map(crate::render::dot_mask::DotMaskGpu::generation));
            let (_model, model_group) = pipeline.create_model_bind_group(
                ctx,
                crate::render::combined_overlay_matrix(&self.camera, self.origin_world).to_cols_array_2d(),
                0.0,
                0.0,
                GlobeParams::FLAT,
            );
            let frame = render_tile_draws(
                ctx,
                pipeline,
                &self.camera,
                self.size,
                wgpu::Color::BLACK,
                &[TileDraw::Dots(&group, &model_group, flat)],
            )
            .await;
            (frame, group.vertices())
        }
    }

    /// Eight 250-person cells (2,000 dots) stacked in one column
    /// that straddles the frame's centre line, `x` in [-32, 32) px, as
    /// 64-dot blocks; `extra_column` adds another eight at that column (cells).
    fn straddling_cells(fx: &RelocationFixture, extra_column: Option<f32>) -> [Vec<crate::render::dots::DotBlock>; 4] {
        let mut sixty_fours = Vec::new();
        let mut columns = vec![-0.5f32];
        columns.extend(extra_column);
        for &col in &columns {
            for row in 0..8 {
                let origin = [col * fx.cell_width, (row as f32 - 3.0) * fx.cell_width];
                for b in 0..4 {
                    sixty_fours.push(crate::render::dots::DotBlock { origin, pop: 250.0, first_dot: 64 * b });
                }
            }
        }
        [Vec::new(), Vec::new(), Vec::new(), sixty_fours]
    }

    /// Lit pixels (R > 0) west of `x_max` px more than 2 px from every
    /// dot in `allowed` (pixel positions).
    fn stray_pixels(frame: &[u8], size: u32, x_max: u32, allowed: &[(f64, f64)]) -> usize {
        let mut stray = 0;
        for y in 0..size {
            for x in 0..x_max {
                if frame[((y * size + x) * 4) as usize] == 0 {
                    continue;
                }
                let (cx, cy) = (f64::from(x) + 0.5, f64::from(y) + 0.5);
                if !allowed.iter().any(|&(ax, ay)| (ax - cx).hypot(ay - cy) <= 2.0) {
                    stray += 1;
                }
            }
        }
        stray
    }

    /// **The WGSL candidate chain is the CPU chain.** 64 cells, 64 px, pop
    /// 1, ppd 1, K = 16. For cell n, a synthetic mask marks rank 4 ONLY at
    /// the texel under CPU candidate j = n % 16 (every other texel 3): each
    /// lit centroid lands within 1 px of the candidate the CPU mirror
    /// (`dot_candidates` + `choose`) picks -- one bit off in the chain
    /// moves a dot by a whole cell.
    #[test]
    fn wgsl_candidate_chain_matches_the_cpu_chain() {
        use crate::overlay::dotgrid::hash;
        use crate::render::dots::DotBlock;
        const K: u32 = 16;
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let fx = RelocationFixture::new(1024);
            let cw = fx.cell_width;
            let blocks: Vec<DotBlock> = (0..8)
                .flat_map(|j| (0..8).map(move |i| (i, j)))
                .map(|(i, j)| DotBlock { origin: [(i as f32 - 4.0) * cw, (j as f32 - 3.0) * cw], pop: 1.0, first_dot: 0 })
                .collect();
            let mut texels = vec![3u8; 1024 * 1024];
            for (n, b) in blocks.iter().enumerate() {
                let key = hash::cell_key(b.origin);
                let j = n % K as usize;
                let y_world = fx.tile_origin_y + b.origin[1] * fx.unit_world_per_lyon;
                let stretch = (std::f32::consts::PI * (1.0 - 2.0 * y_world)).cosh();
                let (u, v) = hash::dot_candidates(key, 0, K).nth(j).expect("K candidates");
                let t = fx.lookup.texel([b.origin[0] + u * cw, b.origin[1] + -v * cw * stretch]);
                texels[t[1] as usize * 1024 + t[0] as usize] = hash::RANK_BUILDING;
            }
            let mut picked_j = 0;
            let expected: Vec<(f64, f64)> = blocks
                .iter()
                .enumerate()
                .map(|(n, b)| {
                    let (p, chosen, _) = fx.place(b.origin, 0, K, &texels);
                    picked_j += usize::from(chosen == n % K as usize);
                    fx.px(p)
                })
                .collect();
            assert!(picked_j >= 60, "the fixture must select candidate n % 16 almost always: {picked_j}/64");
            let mask = crate::render::dot_mask::DotMaskGpu::from_texels(&ctx, fx.grid, &texels);
            let buckets = [blocks.clone(), Vec::new(), Vec::new(), Vec::new()];
            for flat in [false, true] {
                let (frame, _) = fx.frame(&ctx, &pipeline, &buckets, Some(&mask), fx.params(3.0, K), flat).await;
                let size = fx.size;
                let mut checked = 0;
                for (n, &(ex, ey)) in expected.iter().enumerate() {
                    let crowded = expected.iter().enumerate().any(|(m, &(ox, oy))| m != n && (ox - ex).hypot(oy - ey) < 16.0);
                    if crowded || ex < 8.0 || ey < 8.0 || ex > f64::from(size) - 8.0 || ey > f64::from(size) - 8.0 {
                        continue;
                    }
                    let (mut sum, mut sx, mut sy) = (0.0f64, 0.0f64, 0.0f64);
                    for y in (ey as i64 - 8)..=(ey as i64 + 8) {
                        for x in (ex as i64 - 8)..=(ex as i64 + 8) {
                            let v = f64::from(frame[((y as u32 * size + x as u32) * 4) as usize]);
                            sum += v;
                            sx += v * (x as f64 + 0.5);
                            sy += v * (y as f64 + 0.5);
                        }
                    }
                    assert!(sum > 0.0, "flat={flat} dot {n}: nothing lit within 8 px of the CPU choice ({ex:.1}, {ey:.1})");
                    let (cx, cy) = (sx / sum, sy / sum);
                    assert!(
                        (cx - ex).hypot(cy - ey) <= 1.0,
                        "flat={flat} dot {n}: GPU centroid ({cx:.2}, {cy:.2}) vs CPU candidate ({ex:.2}, {ey:.2})"
                    );
                    checked += 1;
                }
                assert!(checked >= 20, "flat={flat}: only {checked} of 64 dots were checkable");
            }
        });
    }

    /// `mask_k` 0 and 1 (a real mask bound, every texel 1) render
    /// pixel-identical to today's frame -- the one `wgsl_pcg_matches_the_cpu_pcg`
    /// draws (the 1x1 land texel, `mask_k` 0).
    #[test]
    fn mask_k_zero_and_one_render_todays_frame() {
        use crate::render::dots::DotBlock;
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let fx = RelocationFixture::new(1024);
            let cw = fx.cell_width;
            let blocks: Vec<DotBlock> = (0..8)
                .flat_map(|j| (0..8).map(move |i| (i, j)))
                .map(|(i, j)| DotBlock { origin: [(i as f32 - 4.0) * cw, (j as f32 - 3.0) * cw], pop: 1.0, first_dot: 0 })
                .collect();
            let buckets = [blocks, Vec::new(), Vec::new(), Vec::new()];
            let water = crate::render::dot_mask::DotMaskGpu::from_texels(&ctx, fx.grid, &vec![1u8; 1024 * 1024]);
            for flat in [false, true] {
                let (today, _) = fx.frame(&ctx, &pipeline, &buckets, None, fx.params(3.0, 0), flat).await;
                assert!(today.as_chunks::<4>().0.iter().any(|p| p[0] > 0), "nothing drew");
                for k in [0, 1] {
                    let (frame, _) = fx.frame(&ctx, &pipeline, &buckets, Some(&water), fx.params(3.0, k), flat).await;
                    let differ = today.as_chunks::<4>().0.iter().zip(frame.as_chunks::<4>().0.iter()).filter(|(a, b)| a != b).count();
                    assert_eq!(differ, 0, "flat={flat} mask_k={k}: {differ} pixels differ from today's frame");
                }
            }
        });
    }

    /// A mask whose left half is water (1) and right half land (3), K = 8,
    /// 2,000 dots in cells straddling the line: 0 dots centred on water
    /// among those with any land candidate (the CPU mirror decides which);
    /// without relocation, hundreds are.
    #[test]
    fn dots_leave_the_water_when_any_candidate_is_dry() {
        const K: u32 = 8;
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let fx = RelocationFixture::new(1024);
            let texels: Vec<u8> = (0..1024 * 1024).map(|i| if i % 1024 < 512 { 1 } else { 3 }).collect();
            let buckets = straddling_cells(&fx, None);
            let mut dots = 0;
            let mut wet = Vec::new();
            for b in &buckets[3] {
                for dot in b.first_dot..b.first_dot + 64 {
                    if !crate::overlay::dotgrid::hash::dot_alive(b.pop, 1.0, dot, crate::overlay::dotgrid::hash::cell_key(b.origin)) {
                        continue;
                    }
                    dots += 1;
                    let (p, _, ranks) = fx.place(b.origin, dot, K, &texels);
                    if ranks.iter().all(|&r| r == 1) {
                        wet.push(fx.px(p));
                    }
                }
            }
            assert_eq!(dots, 2000);
            assert!(wet.len() < 40, "about 2000 / 2^8 dots have no dry candidate: {}", wet.len());
            let mask = crate::render::dot_mask::DotMaskGpu::from_texels(&ctx, fx.grid, &texels);
            let x_max = fx.size / 2 - 3;
            for flat in [false, true] {
                let (on, _) = fx.frame(&ctx, &pipeline, &buckets, Some(&mask), fx.params(1.0, K), flat).await;
                assert_eq!(stray_pixels(&on, fx.size, x_max, &wet), 0, "flat={flat}: a dot with a dry candidate stayed wet");
                let (off, _) = fx.frame(&ctx, &pipeline, &buckets, Some(&mask), fx.params(1.0, 0), flat).await;
                let stray = stray_pixels(&off, fx.size, x_max, &wet);
                assert!(stray > 500, "flat={flat}: without relocation the fixture must put dots on water ({stray} px)");
            }
        });
    }

    /// A candidate outside the mask rect has no data, and a dot that reads
    /// one keeps candidate 0 (DP-5 ruling): over a water mask covering only
    /// the WEST half of the view, the straddling cells' dots -- which the
    /// old land-outside rule pulled east -- stay exactly where `mask_k` 0
    /// puts them, as do an east column's, and the CPU mirror agrees.
    #[test]
    fn a_candidate_outside_the_mask_rect_has_no_data() {
        const K: u32 = 8;
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let fx = RelocationFixture::new(512);
            let texels = vec![1u8; 512 * 1024];
            let buckets = straddling_cells(&fx, Some(2.0));
            let no_data = crate::render::dot_mask::MASK_NO_DATA_RANK as u8;
            let (mut mixed, mut placed) = (0, 0);
            for b in &buckets[3] {
                for dot in b.first_dot..b.first_dot + 64 {
                    let (_, chosen, ranks) = fx.place(b.origin, dot, K, &texels);
                    assert_eq!(chosen, 0, "every candidate is water or no data: candidate 0");
                    placed += 1;
                    // What the land-outside rule would have moved east.
                    mixed += usize::from(ranks.contains(&no_data) && ranks.contains(&1));
                }
            }
            assert!(mixed > 100, "the fixture straddles the mask edge ({mixed} of {placed})");
            let mask = crate::render::dot_mask::DotMaskGpu::from_texels(&ctx, fx.grid, &texels);
            for flat in [false, true] {
                let (on, _) = fx.frame(&ctx, &pipeline, &buckets, Some(&mask), fx.params(1.0, K), flat).await;
                let (off, _) = fx.frame(&ctx, &pipeline, &buckets, Some(&mask), fx.params(1.0, 0), flat).await;
                let lit = on.chunks(4).filter(|p| p[0] > 0).count();
                assert!(lit > 1000, "flat={flat}: the cells drew ({lit} px)");
                assert!(on == off, "flat={flat}: a dot next to no data moved");
            }
        });
    }

    /// **The no-data reset** (final review m6): a dot whose candidates read
    /// water, then land, then NO DATA, then a building keeps candidate 0 --
    /// the land candidate had already taken `(u, v)`, so the no-data branch
    /// must put back `(h_u, h_v)`, not merely stop the search. 64 cells, one
    /// dot each, K = 8; the CPU mirror (`choose`) agrees, and with the
    /// no-data texel turned to water it would pick the building instead.
    #[test]
    fn a_no_data_candidate_after_a_better_one_resets_the_dot_to_candidate_zero() {
        use crate::overlay::dotgrid::hash;
        use crate::render::dots::DotBlock;
        const K: u32 = 8;
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let fx = RelocationFixture::new(1024);
            let cw = fx.cell_width;
            let blocks: Vec<DotBlock> = (0..8)
                .flat_map(|j| (0..8).map(move |i| (i, j)))
                .map(|(i, j)| DotBlock { origin: [(i as f32 - 4.0) * cw, (j as f32 - 3.0) * cw], pop: 1.0, first_dot: 0 })
                .collect();
            let no_data = crate::render::dot_mask::MASK_NO_DATA_RANK as u8;
            let texel_of = |b: &DotBlock, j: usize| {
                let key = hash::cell_key(b.origin);
                let y_world = fx.tile_origin_y + b.origin[1] * fx.unit_world_per_lyon;
                let stretch = (std::f32::consts::PI * (1.0 - 2.0 * y_world)).cosh();
                let (u, v) = hash::dot_candidates(key, 0, K).nth(j).expect("K candidates");
                let t = fx.lookup.texel([b.origin[0] + u * cw, b.origin[1] + -v * cw * stretch]);
                t[1] as usize * 1024 + t[0] as usize
            };
            // Water everywhere; per cell, candidate 1 land, 2 no data, 3 a
            // building (written last-first, so a shared texel keeps the
            // earlier candidate's rank; such cells are skipped below).
            let mut texels = vec![1u8; 1024 * 1024];
            let mut dry = texels.clone();
            for b in &blocks {
                for (j, rank) in [(3, hash::RANK_BUILDING), (2, no_data), (1, 3u8)] {
                    texels[texel_of(b, j)] = rank;
                    dry[texel_of(b, j)] = if rank == no_data { 1 } else { rank };
                }
            }
            let mut expected = Vec::new();
            for b in &blocks {
                let (p0, chosen, ranks) = fx.place(b.origin, 0, K, &texels);
                if ranks[..4] != [1, 3, no_data, hash::RANK_BUILDING] {
                    continue;
                }
                assert_eq!(chosen, 0, "the CPU mirror resets to candidate 0: {ranks:?}");
                let (p1, _, _) = fx.place(b.origin, 0, 2, &texels);
                let (_, without, _) = fx.place(b.origin, 0, K, &dry);
                assert_eq!(without, 3, "with the no-data texel water, the building wins");
                let (a, l) = (fx.px(p0), fx.px(p1));
                // Only cells where candidate 0 and the land candidate are
                // told apart on screen grade the reset.
                if (a.0 - l.0).hypot(a.1 - l.1) > 4.0 {
                    expected.push(a);
                }
            }
            assert!(expected.len() >= 40, "the fixture must grade most cells: {}", expected.len());
            let mask = crate::render::dot_mask::DotMaskGpu::from_texels(&ctx, fx.grid, &texels);
            let buckets = [blocks.clone(), Vec::new(), Vec::new(), Vec::new()];
            for flat in [false, true] {
                let (frame, _) = fx.frame(&ctx, &pipeline, &buckets, Some(&mask), fx.params(3.0, K), flat).await;
                let size = fx.size;
                let mut checked = 0;
                for (n, &(ex, ey)) in expected.iter().enumerate() {
                    if ex < 8.0 || ey < 8.0 || ex > f64::from(size) - 8.0 || ey > f64::from(size) - 8.0 {
                        continue;
                    }
                    let (mut sum, mut sx, mut sy) = (0.0f64, 0.0f64, 0.0f64);
                    for y in (ey as i64 - 3)..=(ey as i64 + 3) {
                        for x in (ex as i64 - 3)..=(ex as i64 + 3) {
                            let v = f64::from(frame[((y as u32 * size + x as u32) * 4) as usize]);
                            sum += v;
                            sx += v * (x as f64 + 0.5);
                            sy += v * (y as f64 + 0.5);
                        }
                    }
                    assert!(sum > 0.0, "flat={flat} dot {n}: nothing lit at candidate 0 ({ex:.1}, {ey:.1})");
                    let (cx, cy) = (sx / sum, sy / sum);
                    assert!(
                        (cx - ex).hypot(cy - ey) <= 1.0,
                        "flat={flat} dot {n}: GPU centroid ({cx:.2}, {cy:.2}) vs candidate 0 ({ex:.2}, {ey:.2})"
                    );
                    checked += 1;
                }
                assert!(checked >= 30, "flat={flat}: only {checked} dots were checkable");
            }
        });
    }

    /// Relocation changes positions, never existence: with the fade off,
    /// the number of lit dots (8-connected components) and the draw's
    /// vertex count (what `DotStats::dots_drawn` counts) are equal on vs
    /// off -- 32 sparse dots over a striped mask of every rank, at least a
    /// third of which move.
    #[test]
    fn relocation_moves_dots_but_never_adds_or_drops_one() {
        use crate::render::dots::DotBlock;
        const K: u32 = 8;
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let fx = RelocationFixture::new(1024);
            let cw = fx.cell_width;
            // Cells one pixel east of the grid: the hashes that shift gives
            // put every pair of dots more than 7 px apart, on and off (the
            // assert below checks it; unshifted, two land 4.2 px apart).
            let shift = cw / 64.0;
            // Diagonal stripes 8 texels wide through ranks 1, 2, 3, 4.
            let texels: Vec<u8> = (0..1024 * 1024).map(|i| 1 + (((i % 1024) / 8 + (i / 1024) / 8) % 4) as u8).collect();
            let blocks: Vec<DotBlock> = (0..4)
                .flat_map(|j| (0..4).map(move |i| (i, j)))
                .map(|(i, j)| DotBlock { origin: [(2 * i - 4) as f32 * cw + shift, (2 * j - 3) as f32 * cw], pop: 2.0, first_dot: 0 })
                .collect();
            let buckets = [Vec::new(), blocks.clone(), Vec::new(), Vec::new()];
            let (mut on_px, mut off_px, mut moved) = (Vec::new(), Vec::new(), 0);
            for b in &blocks {
                for dot in 0..2 {
                    let (on, chosen, _) = fx.place(b.origin, dot, K, &texels);
                    let (off, _, _) = fx.place(b.origin, dot, 0, &texels);
                    on_px.push(fx.px(on));
                    off_px.push(fx.px(off));
                    moved += usize::from(chosen != 0);
                }
            }
            assert!(moved >= 11, "the fixture must move dots: {moved} of 32");
            let apart = |ps: &[(f64, f64)]| {
                ps.iter().enumerate().all(|(n, a)| ps[n + 1..].iter().all(|b| (a.0 - b.0).hypot(a.1 - b.1) > 7.0))
            };
            assert!(apart(&on_px) && apart(&off_px), "fixture: two dots within 7 px would merge");
            let mask = crate::render::dot_mask::DotMaskGpu::from_texels(&ctx, fx.grid, &texels);
            let components = |frame: &[u8]| {
                let n = fx.size as usize;
                let mut seen = vec![false; n * n];
                let mut count = 0;
                for start in 0..n * n {
                    if seen[start] || frame[start * 4] == 0 {
                        continue;
                    }
                    count += 1;
                    let mut stack = vec![start];
                    seen[start] = true;
                    while let Some(i) = stack.pop() {
                        let (x, y) = ((i % n) as i64, (i / n) as i64);
                        for (dx, dy) in [(-1, -1), (0, -1), (1, -1), (-1, 0), (1, 0), (-1, 1), (0, 1), (1, 1)] {
                            let (nx, ny) = (x + dx, y + dy);
                            if nx < 0 || ny < 0 || nx >= n as i64 || ny >= n as i64 {
                                continue;
                            }
                            let j = ny as usize * n + nx as usize;
                            if !seen[j] && frame[j * 4] > 0 {
                                seen[j] = true;
                                stack.push(j);
                            }
                        }
                    }
                }
                count
            };
            for flat in [false, true] {
                let (on, on_vertices) = fx.frame(&ctx, &pipeline, &buckets, Some(&mask), fx.params(2.0, K), flat).await;
                let (off, off_vertices) = fx.frame(&ctx, &pipeline, &buckets, Some(&mask), fx.params(2.0, 0), flat).await;
                assert_eq!(on_vertices, off_vertices, "flat={flat}: the draw's slots");
                assert_eq!(components(&off), 32, "flat={flat}: every dot lit, apart, with relocation off");
                assert_eq!(components(&on), 32, "flat={flat}: relocation added or dropped a dot");
                assert_ne!(on, off, "flat={flat}: relocation moved nothing");
            }
        });
    }
}

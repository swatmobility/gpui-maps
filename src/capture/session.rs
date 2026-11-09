//! [`CaptureSession`]: the capture path with its device, pipeline and fonts
//! kept across frames (map-stories plan, S0.1). [`capture`] is a one-frame
//! session; a caller rendering many frames builds one session and calls
//! [`CaptureSession::render`] per frame, paying the device and pipeline
//! construction once.
use super::*;

/// The label font system and shape cache, with the faces they were built
/// from. Rebuilt only when a request names different faces.
pub(crate) struct FontState {
    /// The `label_fonts` this state was built from, an `Arc` clone, so the
    /// per-frame check is a pointer compare when the caller reuses its
    /// `Arc` (`Arc<[_]>: Eq` short-cuts on the pointer), else a length-first
    /// `==`, not a hash of every face. `None` = system fonts.
    pub(crate) key: Option<LabelFonts>,
    pub(crate) system: glyphon::FontSystem,
    pub(crate) shapes: crate::symbol_pass::LabelShapeCache,
    /// The line-placed labels' glyph atlas, kept with the faces it was
    /// rastered from: a `GlyphKey` names a face by its id in THIS
    /// `system`'s database, so new faces must start a new atlas.
    pub(crate) glyphs: LineGlyphs,
}

/// What `CaptureRequest::line_labels` draws with: the `GlyphSink` pair
/// (`render::GlyphAtlas` and its swash cache) `map_view.rs` and the
/// browser build keep for the same call, and the atlas's texture, rebuilt
/// whenever the atlas's generation moves (it grew since the last upload).
///
/// Deterministic, and independent of every earlier frame: the atlas's
/// LAYOUT is cleared at the start of every line-label frame
/// (`GlyphAtlas::clear_layout`) and packs by arrival order, and the symbol
/// pass asks for glyphs in plan order, never a hash order -- so a frame's
/// texture is the one a fresh session builds. Only the distance fields
/// are kept across frames (`GlyphAtlas::keep_fields`), which are a pure
/// function of the glyph. Re-packing is not optional: a glyph sampled at
/// another origin can round differently in the bilinear weights, so a
/// warm atlas drew different bytes (review F3's test caught it). No raster
/// budget is set (`GlyphAtlas::begin_frame` is never called), so no label
/// is ever deferred by the wall clock.
pub(crate) struct LineGlyphs {
    pub(crate) atlas: crate::render::GlyphAtlas,
    pub(crate) swash: glyphon::SwashCache,
    bind_group: Option<wgpu::BindGroup>,
    generation: Option<u64>,
}

impl LineGlyphs {
    fn new() -> Self {
        let mut atlas = crate::render::GlyphAtlas::new();
        atlas.keep_fields();
        Self { atlas, swash: glyphon::SwashCache::new(), bind_group: None, generation: None }
    }
}

/// A persistent headless capture: one device, one [`TilePipeline`] and one
/// label font state, reused by every [`Self::render`].
///
/// Every frame is bit-identical to what [`capture`] draws for the same
/// request, whatever the session rendered before (the golden tests hold it
/// to that in both scene orders).
pub struct CaptureSession {
    pub(crate) ctx: GpuContext,
    pub(crate) pipeline: TilePipeline,
    pub(crate) format: wgpu::TextureFormat,
    adapter: wgpu::AdapterInfo,
    pub(crate) fonts: Option<FontState>,
    /// The nav chrome's system-font `FontSystem`, apart from the label
    /// state so a nav frame never evicts the label faces (the one-shot path
    /// built a fresh system `FontSystem` for the chrome every frame).
    nav_system: Option<glyphon::FontSystem>,
    /// The bounds the label shape cache is trimmed to at the end of every
    /// frame: `(cap, expensive_kept)`, the live view's by default. Tests
    /// shrink them to exercise the trim without a thousand strings.
    pub(crate) shape_cache_caps: (usize, usize),
    /// How many shape-cache entries the last frame used (read just before
    /// its trim), for the bounded-cache test.
    #[cfg(test)]
    pub(crate) last_frame_shapes_used: usize,
    /// The boxes of the symbols the last frame's collision placement kept
    /// (text and icon), for the chrome-obstacle test.
    #[cfg(test)]
    pub(crate) last_placed_boxes: Vec<crate::collision::CollisionBox>,
    /// The text of every line-placed label the last frame drew, in draw
    /// order, for the churn measurement.
    #[cfg(test)]
    pub(crate) last_line_label_texts: Vec<String>,
    /// The story marks the last frame placed and drew (spec §6.3, §13),
    /// for the live-parity tests.
    #[cfg(test)]
    pub(crate) last_story_marks: super::story_overlays::FrameMarks,
    /// The line-glyph atlas's height cap, in rows: the atlas's own
    /// maximum, lowered only by tests that need a full atlas.
    pub(crate) glyph_atlas_max_height: u32,
    /// Baked tiles kept across frames (S0.2), keyed by every input a bake
    /// reads; see `mesh_cache`.
    mesh_cache: super::mesh_cache::MeshCache,
    /// The story overlay layers' meshes (`CaptureRequest::story_layers`),
    /// tessellated once and redrawn with per-frame trim, opacity and paint
    /// (see `story_overlays`). A one-shot `capture` gets a fresh one, so a
    /// one-frame capture costs what it did.
    story_overlays: super::story_overlays::StoryOverlayCache,
    /// The story arrowheads' pipeline and sprite
    /// (`StoryLayerDraw::arrowheads`), built on the first frame that draws
    /// one and kept.
    story_arrowheads: Option<super::story_overlays::ArrowheadGpu>,
    /// The story markers' pipeline (`StoryLayerDraw::markers`, spec §6.4),
    /// built on the first frame that draws one and kept.
    story_markers: Option<crate::render::marker_pipeline::MarkerPipeline>,
    /// The altitude layer's pipeline, built on the first frame that draws
    /// one and kept: a shader module and two render pipelines per frame
    /// were the cost before. Safe to reuse across pipelined frames because
    /// each frame is submitted before the next one's `write_buffer`s, and
    /// wgpu orders a queue write after all work submitted before it.
    altitude: Option<crate::render::altitude_pipeline::AltitudePipeline>,
    /// The globe's limb glow, sky and star pipelines, each built on the
    /// first frame that draws it and kept, as `altitude` is (review F8: a
    /// globe story rebuilt three shader modules and pipelines per frame).
    /// Every frame rewrites their uniforms before drawing, so a cached one
    /// draws exactly what a fresh one would.
    atmosphere_gpu: Option<crate::render::atmosphere::AtmospherePipeline>,
    sky_gpu: Option<crate::render::sky::SkyPipeline>,
    stars_gpu: Option<crate::render::space::StarPipeline>,
    /// The space look's pipeline and the texture set it was built from,
    /// rebuilt only when a request brings a different set
    /// (`Arc::ptr_eq`), so a story's frames upload the textures once.
    look_gpu: Option<(
        std::sync::Arc<crate::render::space_look::SpaceTextures>,
        crate::render::space_look::SpaceLookPipeline,
    )>,
    /// The chrome pass's pipelines and mark artwork (`capture::chrome`),
    /// built on the first frame that burns any chrome in and kept.
    chrome: Option<super::chrome::ChromeGpu>,
    /// The icon pipeline and the packed sprite atlas of the last request
    /// that carried `sprites`, built on the first such frame and rebuilt
    /// only when a request hands a different `Arc`.
    icons: Option<IconAtlasGpu>,
    /// The one `IconPipeline` both the sprite atlas and the line-label
    /// glyph atlas draw through, in ONE `render_batches` call (see its doc
    /// comment: a second call would clobber the first's vertices). Built
    /// on the first frame that draws either.
    icon_pipeline: Option<crate::render::icon_pipeline::IconPipeline>,
    /// Frames submitted by [`Self::render_pipelined`] and not yet read,
    /// oldest first; at most [`PIPELINE_DEPTH`] between calls.
    in_flight: std::collections::VecDeque<InFlight>,
    /// `Some` once [`Self::catch_device_errors`] has routed the device's
    /// uncaptured errors here; every frame takes it after its submit and
    /// after its wait.
    device_errors: Option<DeviceErrorSlot>,
    /// Test only: spoil the next frames' command buffers with a validation
    /// error (see `encode_frame`).
    #[cfg(test)]
    pub(crate) inject_validation_error: bool,
    /// The stage timings of frames [`Self::render_pipelined`] and
    /// [`Self::flush`] handed back, oldest first, until
    /// [`Self::take_stage_times`] takes them; at most
    /// [`STAGE_TIMES_KEPT`] (a caller that never takes them loses the
    /// oldest).
    stage_times: std::collections::VecDeque<StageTimes>,
}

/// A sprite sheet packed and uploaded once (`render::icon_pipeline`), the
/// way `web/src/app.rs` packs a fetched sheet: every name, sorted, so the
/// atlas is the same texture on every run.
pub(crate) struct IconAtlasGpu {
    /// The atlas this was packed from, held so its pointer (the cache
    /// key) cannot be reused by another allocation while cached.
    source: std::sync::Arc<crate::sprite::SpriteAtlas>,
    bind_group: wgpu::BindGroup,
    rects: std::collections::HashMap<String, crate::render::IconRect>,
    size: (u32, u32),
}

impl IconAtlasGpu {
    fn new(
        ctx: &GpuContext,
        pipeline: &crate::render::icon_pipeline::IconPipeline,
        sprites: &std::sync::Arc<crate::sprite::SpriteAtlas>,
    ) -> Self {
        let mut names: Vec<String> = sprites.names().cloned().collect();
        names.sort();
        let (image, rects) = crate::render::icon_pipeline::pack_icon_atlas(sprites, &names);
        let bind_group = pipeline.upload_atlas(ctx, &image);
        Self { source: std::sync::Arc::clone(sprites), bind_group, rects, size: image.dimensions() }
    }
}

/// Where one frame's time went inside the session, in milliseconds of
/// wall time: measurement only, never an input to a pixel.
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize)]
pub struct StageTimes {
    /// Baking (or fetching from the mesh cache) every drawn tile.
    pub bake_ms: f64,
    /// Recording the passes and submitting, less the burn-in.
    pub encode_ms: f64,
    /// Recording the chrome pass (captions, credit, mark).
    pub burn_in_ms: f64,
    /// Waiting for the GPU and copying the rows out.
    pub readback_ms: f64,
}

/// How many handed-back frames' [`StageTimes`] a session keeps.
pub const STAGE_TIMES_KEPT: usize = 64;

/// How many submitted frames [`CaptureSession::render_pipelined`] keeps
/// before it reads the oldest back: the CPU encodes frame N+2 while the
/// GPU draws N+1 and N's readback lands.
const PIPELINE_DEPTH: usize = 2;

impl CaptureSession {
    /// A headless device and pipeline. `label_fonts`, when given, pre-warms
    /// the font state for requests that name the same faces.
    ///
    /// # Errors
    /// No adapter or no device.
    pub async fn new(label_fonts: Option<LabelFonts>) -> anyhow::Result<Self> {
        let (ctx, adapter) = GpuContext::new_headless_with_info().await?;
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let pipeline = TilePipeline::new(&ctx, format);
        let mut session = Self {
            ctx,
            pipeline,
            format,
            adapter,
            fonts: None,
            nav_system: None,
            shape_cache_caps: (
                crate::symbol_pass::LABEL_SHAPE_CACHE_MAX_ENTRIES,
                crate::symbol_pass::EXPENSIVE_SHAPES_KEPT,
            ),
            #[cfg(test)]
            last_frame_shapes_used: 0,
            #[cfg(test)]
            last_placed_boxes: Vec::new(),
            #[cfg(test)]
            last_line_label_texts: Vec::new(),
            #[cfg(test)]
            last_story_marks: Default::default(),
            glyph_atlas_max_height: u32::MAX,
            mesh_cache: super::mesh_cache::MeshCache::new(super::mesh_cache::DEFAULT_BUDGET),
            story_overlays: super::story_overlays::StoryOverlayCache::default(),
            altitude: None,
            atmosphere_gpu: None,
            sky_gpu: None,
            stars_gpu: None,
            look_gpu: None,
            chrome: None,
            icons: None,
            icon_pipeline: None,
            story_arrowheads: None,
            story_markers: None,
            in_flight: std::collections::VecDeque::new(),
            device_errors: None,
            #[cfg(test)]
            inject_validation_error: false,
            stage_times: std::collections::VecDeque::new(),
        };
        if label_fonts.is_some() {
            fonts_for(&mut session.fonts, &label_fonts);
        }
        Ok(session)
    }

    /// [`Self::new`] for a caller without a runtime.
    ///
    /// # Errors
    /// See [`Self::new`].
    pub fn new_blocking(label_fonts: Option<LabelFonts>) -> anyhow::Result<Self> {
        pollster::block_on(Self::new(label_fonts))
    }

    /// One frame.
    ///
    /// Returned at once, ahead of any frames still queued by
    /// [`Self::render_pipelined`]: mixing the two on one session hands
    /// frames back out of submission order. [`Self::flush`] first to keep
    /// the order.
    ///
    /// # Errors
    /// See [`capture`].
    pub fn render(&mut self, request: &CaptureRequest, tiles: &TileStore, style: &Style) -> anyhow::Result<Capture> {
        pollster::block_on(self.render_async(request, tiles, style))
    }

    /// [`Self::render`] for a caller that already has a runtime.
    ///
    /// # Errors
    /// See [`capture`].
    pub async fn render_async(
        &mut self,
        request: &CaptureRequest,
        tiles: &TileStore,
        style: &Style,
    ) -> anyhow::Result<Capture> {
        render_inner(self, request, tiles, style).await
    }

    /// Submit this frame; return the frame submitted two calls ago, if any.
    ///
    /// The same pixels and counters as [`Self::render`], in submission
    /// order, but the readback of one frame overlaps the encoding of the
    /// next two. Call [`Self::flush`] after the last frame for the rest.
    ///
    /// A [`Self::render`] in between returns its own frame immediately,
    /// before the older frames still queued here, so the caller sees frames
    /// out of submission order. Flush before switching to `render`.
    ///
    /// # Errors
    /// See [`capture`]. A frame that fails to encode is not queued; an
    /// error reading the oldest frame back drops that frame.
    pub fn render_pipelined(
        &mut self,
        request: &CaptureRequest,
        tiles: &TileStore,
        style: &Style,
    ) -> anyhow::Result<Option<Capture>> {
        let in_flight = pollster::block_on(encode_frame(self, request, tiles, style))?;
        self.in_flight.push_back(in_flight);
        if self.in_flight.len() <= PIPELINE_DEPTH {
            return Ok(None);
        }
        let oldest = self.in_flight.pop_front().expect("more than PIPELINE_DEPTH queued");
        self.read_back_timed(oldest).map(Some)
    }

    /// [`read_back`], recording the frame's [`StageTimes`].
    fn read_back_timed(&mut self, frame: InFlight) -> anyhow::Result<Capture> {
        let started = std::time::Instant::now();
        let mut times = frame.times;
        let capture = read_back(&self.ctx, self.device_errors.as_ref(), frame)?;
        times.readback_ms = started.elapsed().as_secs_f64() * 1e3;
        if self.stage_times.len() >= STAGE_TIMES_KEPT {
            self.stage_times.pop_front();
        }
        self.stage_times.push_back(times);
        Ok(capture)
    }

    /// The oldest handed-back frame's [`StageTimes`] not yet taken: one
    /// per frame [`Self::render_pipelined`] or [`Self::flush`] returned,
    /// in the same order.
    ///
    /// A FIFO kept in step with the frames only by the caller taking one
    /// per frame it receives: a frame whose readback fails records none,
    /// and [`Self::render`] records none, so a caller that mixes those in
    /// must drain this first or its pairing drifts. The story player uses
    /// only the pipelined path and takes one per frame; the numbers are
    /// measurement, never an input to a pixel.
    pub fn take_stage_times(&mut self) -> Option<StageTimes> {
        self.stage_times.pop_front()
    }

    /// Drain every in-flight frame, oldest first.
    ///
    /// # Errors
    /// The first frame that fails to read back; the frames after it are
    /// dropped.
    pub fn flush(&mut self) -> anyhow::Result<Vec<Capture>> {
        let mut out = Vec::with_capacity(self.in_flight.len());
        while let Some(frame) = self.in_flight.pop_front() {
            match self.read_back_timed(frame) {
                Ok(capture) => out.push(capture),
                Err(e) => {
                    self.in_flight.clear();
                    return Err(e);
                }
            }
        }
        Ok(out)
    }

    /// Story path only: route wgpu validation errors into `Err` instead of
    /// wgpu's panic.
    ///
    /// The session's device keeps wgpu's panicking default otherwise, so a
    /// test's validation error stays loud (see
    /// `GpuContext::new_headless_with_info`). After this, the device's
    /// uncaptured errors land in a slot every frame takes after its submit
    /// and after its wait, failing that frame with `device error: ..`. The
    /// first error since the last take is kept; later ones until then are
    /// the same failure's echoes.
    pub fn catch_device_errors(&mut self) {
        let slot: DeviceErrorSlot = std::sync::Arc::default();
        let sink = std::sync::Arc::clone(&slot);
        self.ctx.device.on_uncaptured_error(std::sync::Arc::new(move |error| {
            let mut kept = sink.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if kept.is_none() {
                *kept = Some(error.to_string());
            }
        }));
        self.device_errors = Some(slot);
    }

    /// Test only: leave `message` in the device-error slot as a late
    /// error would (one the device reports after a frame's post-submit
    /// take), so the next wait takes it.
    #[cfg(test)]
    pub(crate) fn inject_late_device_error(&self, message: &str) {
        let slot = self.device_errors.as_ref().expect("catch_device_errors first");
        *slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(message.to_string());
    }

    /// The baked-tile cache's byte budget (default 512 MiB). Shrinking it
    /// evicts least-recently-used bakes at once.
    pub fn set_mesh_cache_budget(&mut self, bytes: u64) {
        self.mesh_cache.set_budget(bytes);
    }

    /// The heaviest cached bake's budget charge, for the bound test.
    #[cfg(test)]
    pub(crate) fn mesh_cache_largest_entry(&self) -> u64 {
        self.mesh_cache.largest_entry_bytes()
    }

    /// The baked-tile cache's hit and miss counts and its size.
    pub fn mesh_cache_stats(&self) -> super::MeshCacheStats {
        self.mesh_cache.stats()
    }

    /// The line-glyph atlas's layout (every key and its rect), for tests
    /// that must know two sessions' atlases really differ.
    #[cfg(test)]
    pub(crate) fn line_glyph_layout(
        &self,
    ) -> Vec<(crate::render::GlyphKey, Option<crate::render::glyph_atlas::GlyphRect>)> {
        self.fonts.as_ref().map(|f| f.glyphs.atlas.layout()).unwrap_or_default()
    }

    /// How many glyphs' distance fields the line-glyph state keeps across
    /// frames.
    #[cfg(test)]
    pub(crate) fn line_glyph_fields_kept(&self) -> usize {
        self.fonts.as_ref().map_or(0, |f| f.glyphs.atlas.fields_kept())
    }

    /// The adapter's name and backend, for a manifest.
    pub fn device_name(&self) -> String {
        format!("{} ({:?})", self.adapter.name, self.adapter.backend)
    }
}

/// The font system and shape cache for `label_fonts`, built on first use and
/// rebuilt only when the faces change. `None` = system fonts
/// (`font_fallback::new_font_system`), exactly as the one-shot path built
/// them. A free function over the field so the render body can hold it
/// alongside borrows of the device and pipeline.
pub(crate) fn fonts_for<'a>(
    fonts: &'a mut Option<FontState>,
    label_fonts: &Option<LabelFonts>,
) -> &'a mut FontState {
    if fonts.as_ref().is_none_or(|f| f.key != *label_fonts) {
        let (system, shapes) = match label_fonts {
            None => (crate::font_fallback::new_font_system(), crate::symbol_pass::LabelShapeCache::new()),
            Some((files, family)) => {
                let mut db = glyphon::fontdb::Database::new();
                for bytes in files.iter() {
                    db.load_font_data(bytes.clone());
                }
                // The story design card (spec §4.1, D5) draws through
                // THIS SAME `FontSystem` -- `build_story_chrome` shapes
                // both the map's labels and the card's text against
                // whatever `fonts_for` returns. `label_fonts` overrides
                // the LABEL faces (deterministic test fixtures, or a
                // caller's own stack); the card's face is a story-chrome
                // constant, independent of that override, so the story
                // faces are loaded here too -- exactly as `new_font_system`
                // loads them for the `None` branch above (both call the
                // shared `load_story_fonts`, so the two paths cannot drift
                // apart again) -- or a request with an overridden
                // `label_fonts` (every golden capture, and any native
                // caller that sets one) would ask to shape
                // `STORY_FAMILY`/`STORY_ARABIC_FAMILY` against a database
                // that does not have them, and cosmic-text would fall
                // back to whichever label face happens to be installed.
                crate::font_fallback::load_story_fonts(&mut db);
                (
                    crate::font_fallback::new_font_system_from_db(String::from("en-US"), db),
                    crate::symbol_pass::LabelShapeCache::with_default_family(family),
                )
            }
        };
        *fonts = Some(FontState { key: label_fonts.clone(), system, shapes, glyphs: LineGlyphs::new() });
    }
    fonts.as_mut().expect("just set")
}

/// One frame, synchronously: [`encode_frame`] then [`read_back`], which is
/// the old one-shot path byte for byte.
pub(crate) async fn render_inner(
    session: &mut CaptureSession,
    request: &CaptureRequest,
    tiles: &TileStore,
    style: &Style,
) -> anyhow::Result<Capture> {
    let in_flight = encode_frame(session, request, tiles, style).await?;
    read_back(&session.ctx, session.device_errors.as_ref(), in_flight)
}

/// The body of the old one-shot `capture_async` up to its submit, moved
/// here verbatim except that the device, pipeline and fonts come from
/// `session`: encodes and submits the frame and asks for its readback to
/// map, without waiting for it.
pub(crate) async fn encode_frame(
    session: &mut CaptureSession,
    request: &CaptureRequest,
    tiles: &TileStore,
    style: &Style,
) -> anyhow::Result<InFlight> {
    anyhow::ensure!(
        request.pixel_ratio.is_finite() && (0.5..=8.0).contains(&request.pixel_ratio),
        "pixel_ratio must be finite and in [0.5, 8], got {}",
        request.pixel_ratio
    );
    // The primary guard for an oversized target: refuse it here, with both
    // numbers, rather than let wgpu reject the texture. The device-error
    // slot (`catch_device_errors`) catches whatever this does not foresee.
    let max_side = session.ctx.device.limits().max_texture_dimension_2d;
    anyhow::ensure!(
        (1..=max_side).contains(&request.width) && (1..=max_side).contains(&request.height),
        "capture target {}x{} is outside this device's 1..={max_side} texture side",
        request.width,
        request.height
    );
    // The building floor, as the app resolves it (master's
    // `capture_async`, moved here with the rest of the body). A caller that
    // fetched `request.cover()` unresolved may lack the floor's finer
    // tiles; then the frame keeps the cover it fetched rather than draw
    // holes.
    let resolved = request.resolved(style);
    let request = if resolved.building_floor != request.building_floor
        && resolved.cover().iter().any(|t| !tiles.contains_key(t))
        && request.cover().iter().all(|t| tiles.contains_key(t))
    {
        // A test that forgot to fetch the resolved cover is a bug in the
        // test: fail it. A tool degrades to the cover it fetched.
        #[cfg(test)]
        anyhow::bail!("capture: the store lacks the building floor's tiles; fetch request.resolved(style).cover()");
        #[cfg(not(test))]
        {
            eprintln!("capture: building floor not applied: the store lacks its tiles (fetch request.resolved(style).cover())");
            request
        }
    } else {
        &resolved
    };
    let frame_started = std::time::Instant::now();
    let glyph_atlas_max_height = session.glyph_atlas_max_height;
    let CaptureSession { ctx, pipeline, format, fonts, nav_system, shape_cache_caps, mesh_cache, story_overlays, altitude: altitude_pipeline, atmosphere_gpu, sky_gpu, stars_gpu, look_gpu, device_errors, chrome, icons, icon_pipeline, story_arrowheads, story_markers, .. } = session;
    let (ctx, pipeline, format): (&GpuContext, &TilePipeline, wgpu::TextureFormat) = (ctx, pipeline, *format);
    let camera = request.camera();
    let frame = crate::render::globe::globe_frame(&camera).with_globe_enabled(request.globe);
    // Unlit unless the request names a sun -- see `CaptureRequest::sun`.
    let (sun, night_brightness) = match request.sun {
        // `GlobeParams::unlit`, spelled as the two values that make it.
        None => (glam::DVec3::ZERO, 1.0),
        // The night floor: the request's own atmosphere's, when it names
        // one (so a capture can draw exactly what a `MapView` with that
        // `Space` draws), else the terminator ON -- naming a sun without
        // an atmosphere has always meant "show me the terminator".
        Some(sun) => (
            sun.direction_sphere(&frame.globe_view.inverse()),
            request.atmosphere.map_or(
                crate::render::atmosphere::Atmosphere::TERMINATOR_NIGHT_BRIGHTNESS,
                |atmosphere| atmosphere.night_brightness_clamped(),
            ),
        ),
    };
    // The night FLOOR is raised by the space fade, exactly as
    // `MapView::paint` raises it -- see the comment there for why that is
    // algebraically the same frame the old shared transition produced.
    let night_floor = crate::render::atmosphere::night_floor(night_brightness, frame.space_transition);
    let globe = GlobeUpload::new(&frame, &camera, MVT_EXTENT as f32, sun, night_floor);
    let transition = globe.transition();
    // **Two globenesses, and this harness must use the same one production
    // does for each.** `transition` is how much of the matrix is the sphere
    // (1.0 up to z11); `space_transition` is whether the planet is still a
    // ball in space with a backdrop, air and stars around it (0.0 from z6).
    // Gating the backdrop or the glow on the former would draw a harness
    // frame `MapView::paint` never draws.
    let space_transition = globe.space_transition();

    let cover = request.cover();
    let display_zoom = request.display_zoom();

    let (draw_order, mut missing) =
        draw_order(&cover, &request.absent, request.stand_in_levels, |tile| tiles.contains_key(tile));

    let mut held = Vec::new();
    // What this frame's buildings draw (`render::frame_plan::extrusion_plan`).
    // Translucency on: `fill-extrusion-opacity` < 1 draws as a depth
    // prepass plus a stencilled colour pass, as in MapView. Each drawn
    // tile's extrusion buffers live in its cached bake (`CachedMesh::extrusion`),
    // tessellated only when the frame can extrude at all (`MeshKey::extrudes`).
    let extrusion_plan = crate::render::frame_plan::extrusion_plan(
        &style.layers,
        camera.zoom,
        camera.center_lat,
        f64::from(camera.bearing),
        crate::render::frame_plan::ExtrusionOptions {
            translucency: true,
            opacity_override: request.extrusion_opacity,
            flatten: request.extrusion_flat,
            look: request.extrusion_look,
            depth: if request.flat && space_transition == 0.0 {
                let background = background_pair(style, display_zoom)
                    .map_or(crate::style::Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }, |(from, to)| {
                        from.lerp(&to, request.zoom.fract() as f32)
                    });
                let building_reach = crate::render::cover_plan::flat_cover(&camera, request.flat_cover_request()).building_reach;
                crate::render::frame_plan::extrusion_frame_depth(&camera, building_reach, background)
            } else {
                crate::render::frame_plan::ExtrusionDepth {
                    viewport_px: [camera.viewport_width_px, camera.viewport_height_px],
                    ..Default::default()
                }
            },
            ..Default::default()
        },
    );
    let extrudes = !extrusion_plan.steps.is_empty();
    // The drawn tiles' roofs as height grids, for label occlusion
    // (`CachedMesh::occluder`; stand-ins too, as master's capture pushes
    // them: they occlude while their children load).
    let mut occluders: Vec<(TileId, std::sync::Arc<crate::render::extrusion::ExtrusionMesh>)> = Vec::new();
    let mut label_occlusion_stats = (0, 0.0);
    // One baked `SymbolPlan` per drawn tile, when the request asked for
    // labels. Baked here rather than in a second loop over `draw_order`
    // because the decoded tile is already in hand, and at the display zoom
    // rather than the tile's own for the reason `MapView::symbol_plan_for`
    // gives: `symbol-spacing` is a screen distance and
    // `tile_units_per_px` is what carries it into the tile's 0..4096 space.
    // Shared with the mesh cache: the entry carries the plan.
    let mut symbol_plans: Vec<(TileId, std::sync::Arc<super::mesh_cache::CachedMesh>)> = Vec::new();
    // The cover's own zoom, for `symbol_line::symbol_spacing_zoom`.
    let cover_tile_zoom = if request.flat { request.flat_tile_zoom() } else { display_zoom };
    let (mut vertices, mut triangles) = (0usize, 0usize);
    // The basemap carries its mask lists only when the dots relocate
    // against it, and only as styled: `repaint` drops ops, which the
    // plan's ranks would then misname.
    let masked = matches!(request.paint, Paint::AsStyled)
        && request.dots.as_ref().is_some_and(CaptureDots::relocates_over_the_basemap);
    // The tiles drawn whole: what the dot mask is rendered from.
    let mut mask_tiles: Vec<(TileId, usize)> = Vec::new();
    // Debug builds only: the style every hit must have been baked under
    // (see `mesh_cache`'s module doc), computed on the frame's first hit.
    #[cfg(debug_assertions)]
    let style_fp = std::cell::OnceCell::new();
    #[cfg(debug_assertions)]
    let check_hit = |hit: &super::mesh_cache::CachedMesh| {
        let now = *style_fp.get_or_init(|| super::mesh_cache::style_fingerprint(style));
        debug_assert_eq!(
            hit.style_fingerprint, now,
            "style changed without a new style_generation ({}): a stale mesh would be drawn",
            request.style_generation
        );
    };
    #[cfg(debug_assertions)]
    let baked_under = || *style_fp.get_or_init(|| super::mesh_cache::style_fingerprint(style));
    // Resolved once per frame: the request's choice, or the live map's
    // environment switch when it leaves it open.
    let compact_strokes =
        request.basemap_compact_strokes.unwrap_or_else(crate::render::compact::basemap_compact_strokes_enabled);
    let key_for = |tile: TileId, data: u64, masked: bool, labels: bool, extrudes: bool| super::mesh_cache::MeshKey {
        tile,
        data,
        display_zoom,
        cover_tile_zoom,
        style_gen: request.style_generation,
        paint: super::mesh_cache::paint_key(request.paint),
        subdivision: super::mesh_cache::subdivision_key(request.subdivision.for_tile(tile)),
        masked,
        compact: request.basemap_compact,
        compact_strokes,
        labels,
        extrudes,
    };
    for &(tile, clip) in &draw_order {
        let Some(decoded) = tiles.get(&tile) else {
            missing.push(tile);
            continue;
        };
        let key = key_for(tile, decoded.fingerprint(), masked, request.labels, extrudes);
        let baked = match mesh_cache.get(&key) {
            Some(hit) => {
                #[cfg(debug_assertions)]
                check_hit(&hit);
                hit
            }
            None => {
                let symbol_plan = request.labels.then(|| {
                    crate::style::match_tile_symbols(
                        decoded,
                        style,
                        display_zoom,
                        crate::symbol_line::tile_units_per_px(
                            crate::symbol_line::symbol_spacing_zoom(display_zoom, cover_tile_zoom, tile.z),
                            tile.z,
                        ),
                    )
                });
                let matched = crate::style::match_tile_static_masked(decoded, style, masked);
                let (plan, ranks) = if masked {
                    crate::style::resolve_plan_with_mask(decoded, style, &matched, display_zoom)
                } else {
                    (crate::style::resolve_plan(decoded, style, &matched, display_zoom), Default::default())
                };
                let plan = repaint(plan, request.paint);
                let (mesh, spans) = crate::render::mesh::tessellate_plan_with_ranges(
                    &plan,
                    MVT_EXTENT,
                    crate::render::mesh::COORD_SCALE,
                    request.subdivision.for_tile(tile),
                );
                // The tile's buildings, beside its flat buffers: drawn with
                // the same model group, after every other draw (see below).
                // Tessellated only when the frame can extrude at all.
                let (mut extrusion_vertices, mut extrusion_triangles) = (0usize, 0usize);
                let mut occluder = None;
                let extrusion = if extrudes {
                    let extrusion = crate::render::extrusion::tessellate_extrusions(&plan);
                    if extrusion.over_budget() {
                        eprintln!(
                            "capture: extrusion over budget on {tile:?}: {} vertices (budget {})",
                            extrusion.vertex_count(),
                            crate::render::extrusion::EXTRUSION_VERTEX_BUDGET
                        );
                    }
                    if extrusion.roof_failures > 0 {
                        eprintln!("capture: {} extrusion roof(s) on {tile:?} failed to tessellate and were skipped", extrusion.roof_failures);
                    }
                    // `create_extrusion_buffers` returns `None` for a mesh
                    // with no index at all (`ExtrusionMesh::is_empty`).
                    let gpu = pipeline.create_extrusion_buffers(ctx, &extrusion);
                    if gpu.is_some() {
                        extrusion_vertices = extrusion.vertex_count();
                        extrusion_triangles = extrusion.index_count() / 3;
                        // Only the roofs' height grid is kept (what
                        // `BuildingOcclusion` reads), not the chunks.
                        occluder = Some(std::sync::Arc::new(crate::render::extrusion::ExtrusionMesh {
                            height_grid: extrusion.height_grid.clone(),
                            chunks: Vec::new(),
                            roof_failures: extrusion.roof_failures,
                        }));
                    }
                    gpu
                } else {
                    None
                };
                let buffers = (!mesh.indices.is_empty()).then(|| match (masked, request.basemap_compact) {
                    (true, true) => pipeline.install_basemap_mesh_masked_with(ctx, &mesh, &spans, &ranks, Some(compact_strokes)),
                    (true, false) => pipeline.install_mesh_masked(ctx, &mesh, &spans, &ranks),
                    (false, true) => pipeline.install_basemap_mesh_with(ctx, None, &mesh, compact_strokes),
                    (false, false) => pipeline.create_tile_buffers(ctx, &mesh),
                });
                mesh_cache.insert(
                    key,
                    super::mesh_cache::CachedMesh {
                        buffers,
                        extrusion,
                        occluder,
                        vertices: mesh.vertices.len() + extrusion_vertices,
                        triangles: mesh.indices.len() / 3 + extrusion_triangles,
                        symbol_plan,
                        #[cfg(debug_assertions)]
                        style_fingerprint: baked_under(),
                    },
                )
            }
        };
        if request.labels {
            symbol_plans.push((tile, std::sync::Arc::clone(&baked)));
        }
        // A tile with no flat geometry is still installed when it has
        // buildings (a buildings-only source, review F3): it has no flat
        // draw, only its walls and roofs.
        if !baked.draws_anything() {
            continue;
        }
        vertices += baked.vertices;
        triangles += baked.triangles;
        if request.label_occlusion {
            if let Some(occluder) = &baked.occluder {
                occluders.push((tile, std::sync::Arc::clone(occluder)));
            }
        }
        if clip.is_whole() {
            mask_tiles.push((tile, held.len()));
        }
        let (_model_buffer, model_bind_group) = pipeline.create_model_bind_group_clipped(
            ctx,
            crate::render::combined_tile_matrix(&camera, tile, MVT_EXTENT as f32)
                .to_cols_array_2d(),
            // `MapView::tile_blend_t`: a tile baked at the camera's own
            // bucket blends by the camera's fractional zoom.
            camera.zoom.fract() as f32,
            crate::render::dash_scale(&camera, tile, MVT_EXTENT as f32),
            globe.for_tile(tile),
            // `TileClip::WHOLE` for every ordinary tile; the missing tile's
            // own sub-box for a stand-in -- see `draw_order`.
            clip,
        );
        // The buffer has to outlive the render pass that reads it, and the
        // bind group holds it only by handle.
        held.push((baked, model_bind_group, _model_buffer));
    }
    let tiles_drawn = held.len();
    // The mask's ring past the cover (DP-5 ruling): installed with their
    // mask lists, rasterised into the mask, never drawn.
    let mut ring_held: Vec<(TileId, std::sync::Arc<super::mesh_cache::CachedMesh>)> = Vec::new();
    if masked {
        let ring = request.mask_ring();
        for &tile in &ring {
            let Some(decoded) = tiles.get(&tile) else { continue };
            // No symbol plan for a ring tile: keyed `labels: false`, which
            // is exactly the bake a cover tile gets on a label-less frame.
            // Nor buildings: a ring tile is only rasterised into the mask,
            // which reads flat footprints, so it is keyed `extrudes: false`.
            let key = key_for(tile, decoded.fingerprint(), true, false, false);
            let baked = match mesh_cache.get(&key) {
                Some(hit) => {
                    #[cfg(debug_assertions)]
                    check_hit(&hit);
                    hit
                }
                None => {
                    let matched = crate::style::match_tile_static_masked(decoded, style, true);
                    let (plan, ranks) = crate::style::resolve_plan_with_mask(decoded, style, &matched, display_zoom);
                    let (mesh, spans) = crate::render::mesh::tessellate_plan_with_ranges(
                        &plan,
                        MVT_EXTENT,
                        crate::render::mesh::COORD_SCALE,
                        request.subdivision.for_tile(tile),
                    );
                    let buffers = (!mesh.indices.is_empty()).then(|| {
                        if request.basemap_compact {
                            pipeline.install_basemap_mesh_masked_with(ctx, &mesh, &spans, &ranks, Some(compact_strokes))
                        } else {
                            pipeline.install_mesh_masked(ctx, &mesh, &spans, &ranks)
                        }
                    });
                    mesh_cache.insert(
                        key,
                        super::mesh_cache::CachedMesh {
                            buffers,
                            extrusion: None,
                            occluder: None,
                            vertices: mesh.vertices.len(),
                            triangles: mesh.indices.len() / 3,
                            symbol_plan: None,
                            #[cfg(debug_assertions)]
                            style_fingerprint: baked_under(),
                        },
                    )
                }
            };
            if baked.buffers.is_some() {
                ring_held.push((tile, baked));
            }
        }
        // The window's cap, nearest first (`dot_mask::cap_ring` over the
        // tiles' device bytes; one with no buffers costs nothing), so the
        // capture and the window draw the same ring at z16+.
        let bytes_of = |tile: TileId| {
            Some(ring_held.iter().find(|(t, _)| *t == tile).map_or(0, |(_, b)| b.buffers.as_ref().map_or(0, |b| b.device_bytes() as usize)))
        };
        let kept = crate::render::dot_mask::cap_ring(&ring, bytes_of, 0, crate::render::dot_mask::MASK_RING_MAX_BYTES);
        ring_held.retain(|(tile, _)| kept.contains(tile));
    }

    let baked_at = std::time::Instant::now();
    let color_texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("capture-color"),
        size: wgpu::Extent3d {
            width: request.width,
            height: request.height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor::default());
    let depth_view = pipeline.create_depth_view(ctx, request.width, request.height);
    let (camera_buffer, camera_bind_group) = pipeline.create_camera_bind_group(
        ctx,
        camera.view_projection().to_cols_array_2d(),
        camera.zoom as f32,
        // `1.0` at the default ratio, so one framebuffer pixel is one
        // logical pixel; see `CaptureRequest::pixel_ratio`.
        request.pixel_ratio,
    );
    // `circle-pitch-scale: viewport`'s centre distance, as `MapView::paint`
    // writes it.
    pipeline.write_circle_frame(ctx, &camera_buffer, camera.distance() as f32);
    // THE HORIZON FADE, as `MapView::paint` writes it: only on a flat frame
    // (`space_transition == 0`), from the flat cover's cut and the colour
    // the flat frame clears to. Written only for an opted-in flat request,
    // so every globe capture keeps the fade-off uniform it always had.
    let fog = if request.flat && space_transition == 0.0 {
        let reach = crate::render::cover_plan::flat_cover(&camera, request.flat_cover_request()).reach;
        let pair = background_pair(style, display_zoom);
        let background = pair.map(|(from, to)| from.lerp(&to, request.zoom.fract() as f32));
        // No imagery cut: a capture's raster tiles are handed in, not covered.
        let fog = crate::render::cover_plan::frame_fog(&camera, reach, None, background, space_transition);
        pipeline.write_horizon_fog(ctx, &camera_buffer, fog);
        fog
    } else {
        crate::render::HorizonFog::NONE
    };

    // The route line, as `MapView::paint` draws it: synced to the host's
    // layer and pushed LAST, over every basemap tile.
    let mut nav_route = None;
    if let Some(nav) = &request.nav {
        crate::render::nav_layer::sync_route(&mut nav_route, ctx, pipeline, nav.host.route_layer(), &camera, &globe);
    }
    // The runtime overlay, as `MapView::paint` draws one source.
    let overlay_cover = request.overlay_tiling.then_some(cover.as_slice());
    // `overlay` then each of `overlay_more`, each its own source, as
    // `MapView` draws several named sources in order.
    let mut overlay: Vec<CaptureOverlayDraw> = Vec::new();
    let geojson = request.overlay.as_deref().into_iter().chain(request.overlay_more.iter().map(String::as_str));
    let csv = request.overlay_csv.as_deref().map(OverlayData::Csv);
    for data in geojson.map(OverlayData::GeoJson).chain(csv) {
        overlay.extend(overlay_draws(
            data,
            ctx,
            pipeline,
            &camera,
            &globe,
            display_zoom,
            overlay_cover,
            request.overlay_paint.as_deref(),
            request.overlay_points,
        )?);
    }
    let highlight = match (&request.overlay, &request.overlay_highlight) {
        (Some(json), Some((index, style))) => {
            highlight_draw(json, *index, style, request.overlay_paint.as_deref(), ctx, pipeline, &camera, &globe, display_zoom)?
        }
        _ => None,
    };
    for draw in overlay.iter().chain(&highlight) {
        vertices += draw.vertices;
        triangles += draw.triangles;
    }
    // The story overlay layers (`CaptureRequest::story_layers`): each
    // prepared from the session cache (built on a miss, else its uniform
    // and changed paint rewritten), then its marks built for this frame.
    // A layer at opacity 0 is prepared (so the cache is warm) but not drawn.
    // One cached mesh per key: two draws sharing a key would share one
    // uniform, and the later one's trim and overrides would win for both.
    {
        let mut keys: Vec<u64> = request.story_layers.iter().map(|l| l.key).collect();
        keys.sort_unstable();
        anyhow::ensure!(
            keys.windows(2).all(|w| w[0] != w[1]),
            "story_layers: two layers share one key; every StoryLayerDraw::key in a request must be distinct"
        );
    }
    story_overlays.begin_frame();
    let mut story_retessellations = 0;
    let mut story_marks: Vec<Option<super::story_overlays::MarksDraw>> = Vec::with_capacity(request.story_layers.len());
    for layer in &request.story_layers {
        // A layer drawn as markers (spec §6.4) has no mesh.
        if layer.markers.is_some() {
            story_marks.push(None);
            continue;
        }
        if story_overlays.prepare(ctx, pipeline, &camera, &globe, display_zoom, layer)? {
            story_retessellations += 1;
        }
        let opacity = layer.line_fx[2];
        story_marks.push(match layer.marks.as_deref() {
            Some(marks) if opacity > 0.0 => super::story_overlays::marks_draw(marks, opacity, ctx, pipeline, &camera, &globe, display_zoom)?,
            _ => None,
        });
    }
    story_overlays.end_frame();
    let story_overlays: &super::story_overlays::StoryOverlayCache = story_overlays;
    // `(slot above the labels, buffers, bind group)` in draw order: each
    // layer, then its marks. The marks (the draw head, drop and pulse
    // rings) always go ABOVE the labels: a moving pen is UI, not data, and
    // must not vanish under a road shield or a place name.
    //
    // A layer `above_buildings` (and not above the labels) goes to the
    // above-buildings pass instead of the tile pass: drawn after the
    // extrusions with a fresh depth buffer, so a route under a wall or a
    // roof is not cut into dashes, and still under the labels.
    let mut story_draws: Vec<(StorySlot, &crate::render::TileBuffers, &wgpu::BindGroup)> = Vec::new();
    for (layer, marks) in request.story_layers.iter().zip(&story_marks) {
        if layer.line_fx[2] <= 0.0 || layer.markers.is_some() {
            continue;
        }
        if let Some((buffers, bind_group, v, t)) = story_overlays.draw_of(layer.key) {
            vertices += v;
            triangles += t;
            let slot = if layer.above_labels {
                StorySlot::AboveLabels
            } else if layer.above_buildings {
                StorySlot::AboveBuildings
            } else {
                StorySlot::Tiles
            };
            story_draws.push((slot, buffers, bind_group));
        }
        if let Some(marks) = marks {
            vertices += marks.vertices;
            triangles += marks.triangles;
            story_draws.push((StorySlot::AboveLabels, &marks.buffers, &marks.bind_group));
        }
    }
    let mut draws: Vec<crate::render::TileDraw<'_>> =
        held
            .iter()
            .filter_map(|(baked, bind_group, _)| baked.buffers.as_ref().map(|b| crate::render::TileDraw::Legacy(b, bind_group)))
            .collect();
    // The capture's one overlay source, in the slot `MapView::paint` would
    // give it: the tile pass (below the labels) or the pass after them.
    let overlay_slot = if request.overlay_above_labels {
        crate::render::overlay_order::LabelSlot::AboveLabels
    } else {
        crate::render::overlay_order::LabelSlot::BelowLabels
    };
    let (below_labels, above_labels) =
        crate::render::overlay_order::partition_overlay_order(&[()], |_| overlay_slot);
    let mut above_draws: Vec<crate::render::TileDraw<'_>> = Vec::new();
    // And over the buildings, the way `MapView::paint` lifts it: on a frame
    // that draws them and none of the caller's own columns.
    let overlay_building = if request.overlay_under_buildings {
        crate::render::overlay_order::BuildingSlot::UnderBuildings
    } else {
        crate::render::overlay_order::BuildingSlot::OverBuildings
    };
    let lift = extrudes && request.extrusions.is_empty();
    let mut above_buildings_draws: Vec<crate::render::TileDraw<'_>> = Vec::new();
    for slot in below_labels.iter().map(|_| overlay_slot).chain(above_labels.iter().map(|_| overlay_slot)) {
        use crate::overlay::host::OverlayPass;
        let draws = match crate::render::overlay_order::overlay_pass(slot, overlay_building, lift) {
            OverlayPass::Tiles => &mut draws,
            OverlayPass::AboveBuildings => &mut above_buildings_draws,
            OverlayPass::AboveLabels => &mut above_draws,
        };
        // Compact fills first, then their hairlines, as `MapView::paint` does.
        for draw in &overlay {
            match &draw.body {
                CaptureOverlayBody::Legacy(buffers, bind_group) => {
                    draws.push(crate::render::TileDraw::Legacy(buffers, bind_group));
                }
                CaptureOverlayBody::Compact(chunks, fill, _) => {
                    draws.extend(chunks.iter().map(|c| crate::render::TileDraw::CompactFill(c, fill)));
                }
                CaptureOverlayBody::Circles(buffers, model, group, _, _) => {
                    draws.push(crate::render::TileDraw::Circles(buffers, model, group));
                }
            }
        }
        for draw in &overlay {
            if let CaptureOverlayBody::Compact(chunks, _, Some(outline)) = &draw.body {
                draws.extend(
                    chunks
                        .iter()
                        .filter(|c| c.has_outline())
                        .map(|c| crate::render::TileDraw::CompactOutline(c, outline)),
                );
            }
        }
        // The highlight, right over its source, as `MapView::paint` draws it.
        if let Some(CaptureOverlayDraw { body: CaptureOverlayBody::Legacy(buffers, bind_group), .. }) = &highlight {
            draws.push(crate::render::TileDraw::Legacy(buffers, bind_group));
        }
    }
    // The story overlay layers, after `overlay` and `overlay_more`, in
    // declaration order, each in its own slot.
    for &(slot, buffers, bind_group) in &story_draws {
        let draws = match slot {
            StorySlot::Tiles => &mut draws,
            StorySlot::AboveBuildings => &mut above_buildings_draws,
            StorySlot::AboveLabels => &mut above_draws,
        };
        draws.push(crate::render::TileDraw::Legacy(buffers, bind_group));
    }
    // The dot layer, over the overlay and under the route line.
    let (_dot_table, dot_draws) = match &request.dots {
        Some(dots) => {
            let basemap: Vec<(TileId, &crate::render::TileBuffers)> = mask_tiles
                .iter()
                .filter_map(|&(tile, at)| held[at].0.buffers.as_ref().map(|b| (tile, b)))
                .chain(ring_held.iter().filter_map(|(tile, baked)| baked.buffers.as_ref().map(|b| (*tile, b))))
                .collect();
            let mask = capture_dot_mask(dots, ctx, pipeline, &camera, transition, &basemap, masked);
            #[cfg(any(test, feature = "probe"))]
            {
                // A synthetic mask is its own texels (its texture has no
                // copy source).
                *LAST_DOT_MASK.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = match &dots.synthetic_mask {
                    Some(synthetic) => Some(synthetic.clone()),
                    None => mask.as_ref().map(|(m, grid)| (*grid, m.read_back(ctx))),
                };
            }
            let (table, built) =
                dot_draws(dots, ctx, pipeline, &camera, &globe, None, mask.as_ref().map(|(m, grid)| (m, grid)), None);
            (Some(table), built)
        }
        None => (None, Vec::new()),
    };
    for dot in &dot_draws {
        // What the draw issues: the live prefix of each bucket.
        let drawn = dot.dots.vertices() as usize;
        vertices += drawn;
        triangles += drawn / crate::render::dots::DOT_VERTICES as usize;
        draws.push(crate::render::TileDraw::Dots(&dot.dots, &dot.model_group, transition == 0.0));
    }
    if let Some(draw) = nav_route.as_ref().and_then(|route: &crate::render::nav_layer::RouteMeshGpu| route.draw()) {
        draws.push(draw.into());
    }
    // The buildings, after every other draw in the tile pass (R4, R6), as
    // `MapView::paint` pushes them: walls cover the roads, fills, overlays,
    // dots and route behind them, and the label pass still draws over
    // them. Layer-major.
    let mut extrusion_params = crate::render::extrusion_gpu::ExtrusionParamsGpu::new();
    extrusion_params.write(
        ctx,
        pipeline.extrusion(),
        &extrusion_plan.layers.iter().map(|l| l.params).collect::<Vec<_>>(),
    );
    let extrusion_tiles: Vec<(&crate::render::extrusion_gpu::ExtrusionGpu, &wgpu::BindGroup)> = held
        .iter()
        .filter_map(|(baked, bind_group, _)| baked.extrusion.as_ref().map(|gpu| (gpu, bind_group)))
        .collect();
    crate::render::extrusion_gpu::push_extrusion_draws(&extrusion_plan, &extrusion_tiles, &extrusion_params, &mut draws);
    // Overlay extrusion layers (`MapView::set_extrusion_layer`), after the
    // buildings in the same pass, flat or on the globe (each tile with the
    // frame's `globe.for_tile`), lit with the style's own building light:
    // what `MapView::paint` pushes.
    let mut overlay_extrusions: Vec<crate::render::overlay_extrusion::OverlayExtrusionGpu> = Vec::new();
    if !request.extrusions.is_empty() {
        let light = crate::overlay::extrude::style_light(&style.layers, camera.zoom);
        for e in &request.extrusions {
            let mut gpu = crate::render::overlay_extrusion::OverlayExtrusionGpu::upload(ctx, pipeline, &e.set);
            gpu.prepare(ctx, pipeline, &camera, Some(&globe), &e.style, &light);
            overlay_extrusions.push(gpu);
        }
    }
    for (k, gpu) in overlay_extrusions.iter().enumerate() {
        gpu.push_draws(k, &mut draws);
    }

    // The ocean sphere, built exactly as `MapView::paint` builds it: the
    // background colour PAIR baked into the mesh, `BACKDROP_TILE`'s matrix
    // and globe params, and the background blend as `blend_t`.
    // A flat world projection draws the backdrop too: through the planar
    // branch it is the projection's outline, filled with the background.
    let planar = crate::projection::active().is_some();
    let backdrop = (request.backdrop && (planar || crate::render::space::draws_backdrop(space_transition)))
        .then(|| {
            let transparent = Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 };
            let ocean = background_pair(style, display_zoom).unwrap_or((transparent, transparent));
            let tile = crate::render::space::BACKDROP_TILE;
            let mesh = crate::render::space::backdrop_mesh(MVT_EXTENT, ocean.0, ocean.1);
            let buffers = pipeline.create_tile_buffers(ctx, &mesh);
            let (model_buffer, model_bind_group) = pipeline.create_model_bind_group(
                ctx,
                crate::render::combined_tile_matrix(&camera, tile, MVT_EXTENT as f32)
                    .to_cols_array_2d(),
                camera.zoom.fract() as f32,
                0.0,
                globe.for_tile(tile),
            );
            (buffers, model_bind_group, model_buffer)
        });
    // The limb glow, built and written exactly as `MapView::paint` builds
    // and writes it: the SAME `frame` the tile uniforms came from, so the
    // glow sits on the limb the tiles actually drew.
    let atmosphere_pipeline: Option<&crate::render::atmosphere::AtmospherePipeline> =
        match request.atmosphere.filter(|a| a.draws(space_transition)) {
            Some(a) => {
                let glow = atmosphere_gpu.get_or_insert_with(|| {
                    crate::render::atmosphere::AtmospherePipeline::new(ctx, format, pipeline.depth_format())
                });
                glow.write(ctx, &camera, &frame, &a);
                Some(&*glow)
            }
            None => None,
        };
    // The sky above the limb, as the shells draw it -- but only for a
    // request that clears to the STYLE: a sentinel clear exists to show
    // every pixel the ground did not cover, and a sky would hide them.
    let sky_frame = style.sky.at_zoom(request.zoom);
    let sky_pipeline: Option<&crate::render::sky::SkyPipeline> = (matches!(request.clear, Clear::Style)
        && !planar
        && crate::render::sky::draws_sky(
            &sky_frame,
            space_transition,
            crate::render::sky::sees_above_horizon(&camera, &frame),
        ))
    .then(|| {
        let sky = sky_gpu.get_or_insert_with(|| crate::render::sky::SkyPipeline::new(ctx, format, pipeline.depth_format()));
        // The SAME sun and night the ground was lit with above, so the sky
        // and the ground under it agree about where night is. An unlit
        // request (`sun: None`) has no night: `night_brightness` 1.0.
        let atmosphere = crate::render::atmosphere::Atmosphere {
            sun: request.sun.unwrap_or(request.atmosphere.unwrap_or_default().sun),
            night_brightness,
            ..request.atmosphere.unwrap_or_default()
        };
        // The ground below the limb: the background `clear_color` would
        // have cleared to, by `MapView::paint`'s expression -- see
        // `render::sky`'s "The ground below the limb".
        let ground = background_pair(style, request.display_zoom())
            .map_or(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }, |(from, to)| from.lerp(&to, request.zoom.fract() as f32));
        sky.write(ctx, &camera, &frame, &sky_frame, &atmosphere, ground);
        &*sky
    });
    // The stars, as the shells draw them: gated on the SAME transition, and
    // only over a style clear (a sentinel clear exists to be counted).
    // The space look (`CaptureRequest::space_look`): only over a style
    // clear, only on a globe frame. It draws the point stars too, at its own
    // intensity, turned by the request's sidereal angle (0 with no clock).
    let look = request
        .space_look
        .as_ref()
        .filter(|(look, _, _)| matches!(request.clear, Clear::Style) && look.draws(space_transition));
    let look_sidereal = look.map_or(0.0, |(_, _, t)| crate::render::space_look::sidereal_for(*t));
    let star_space = crate::render::space::Space {
        star_intensity: look.map_or(1.0, |(look, _, _)| look.star_intensity),
        ..crate::render::space::Space::default()
    };
    let star_pipeline: Option<&crate::render::space::StarPipeline> = ((request.stars || look.is_some())
        && matches!(request.clear, Clear::Style)
        && (star_space.draws_stars(space_transition) || sky_pipeline.is_some()))
    .then(|| {
        let stars =
            stars_gpu.get_or_insert_with(|| crate::render::space::StarPipeline::new(ctx, format, pipeline.depth_format()));
        stars.write_sky_turned(ctx, &camera, &star_space, look_sidereal);
        &*stars
    });
    // The sun it shades with is the request's, or the plain map's daylight
    // sun (which `space_look::look_sun` turns into its studio sun).
    let look_pipeline = look.map(|(settings, textures, _)| {
        if !look_gpu.as_ref().is_some_and(|(held, _)| std::sync::Arc::ptr_eq(held, textures)) {
            let built = crate::render::space_look::SpaceLookPipeline::new(ctx, format, pipeline.depth_format(), textures, 1);
            *look_gpu = Some((textures.clone(), built));
        }
        let (_, look) = look_gpu.as_ref().expect("built above");
        let sun = request.sun.unwrap_or(crate::render::atmosphere::DAYLIGHT_SUN);
        look.write(ctx, &camera, &frame, settings, sun, look_sidereal);
        look
    });
    let space = crate::render::SpaceDraw {
        sky: sky_pipeline,
        stars: star_pipeline,
        backdrop: backdrop.as_ref().map(|(buffers, bind_group, _)| (buffers, bind_group)),
        atmosphere: atmosphere_pipeline,
        look: look_pipeline,
    };

    // **The raster basemap.** Built only when the request carries imagery,
    // so a capture that predates raster constructs no pipeline, uploads no
    // texture and issues no draw -- `render_with_space_and_raster` takes
    // `None` and the command stream is the one it was.
    //
    // Note what is NOT here: any tile-readiness logic. `draw_order`'s
    // ancestor stand-ins exist because a vector tile that has not arrived
    // leaves a hole in a mesh; a raster tile that has not arrived is
    // simply not in `request.raster` and is not drawn. Standing an
    // ancestor raster tile in for a missing child is a real feature and a
    // separate one -- it wants the reference's `u_scale_parent` /
    // `u_tl_parent` sub-rectangle sampling (`raster.vertex.glsl:59`), not
    // a clip box -- and inventing half of it here would make the pixel
    // tests below grade a frame production does not draw.
    let raster_pipeline = (!request.raster.is_empty())
        .then(|| crate::render::raster_pipeline::RasterPipeline::new(ctx, format, pipeline));
    let mut raster_held = Vec::new();
    // One square per distinct granularity, shared by every tile that wants
    // it -- `render::raster_mesh`'s whole point. At one zoom this holds one
    // entry, or three when the cover reaches both pole rows. Declared out
    // here because the bind groups below borrow from it and it has to
    // outlive the pass.
    let mut raster_squares: std::collections::HashMap<
        crate::render::raster_mesh::RasterMeshKey,
        crate::render::raster_pipeline::RasterBuffers,
    > = std::collections::HashMap::new();
    if let Some(raster_pipeline) = raster_pipeline.as_ref() {
        let squares = &mut raster_squares;
        // Sorted, so a frame is reproducible: a `HashMap`'s iteration
        // order is not, and two raster tiles overlapping at the limb would
        // otherwise composite in whichever order this run happened to
        // pick.
        let mut raster_tiles: Vec<TileId> =
            cover.iter().copied().filter(|t| request.raster.contains_key(t)).collect();
        raster_tiles.sort_by_key(|t| (t.z, t.y, t.x));
        let stand_ins: Vec<(TileId, Option<TileId>)> = request
            .raster_stand_ins
            .iter()
            .filter(|(a, _)| request.raster.contains_key(a))
            .map(|&(a, m)| (a, Some(m)))
            .chain(raster_tiles.into_iter().map(|t| (t, None)))
            .collect();

        for (tile, clip_to) in stand_ins {
            let image = &request.raster[&tile];
            let key = crate::render::raster_mesh::RasterMeshKey::for_tile(tile);
            squares.entry(key).or_insert_with(|| {
                let mesh = crate::render::raster_mesh::build(
                    key,
                    MVT_EXTENT,
                    crate::render::mesh::COORD_SCALE,
                );
                raster_pipeline.create_mesh_buffers(ctx, &mesh)
            });
            let texture = raster_pipeline.upload_tile(ctx, image);
            let (paint_buffer, paint_bind_group) =
                raster_pipeline.create_paint_bind_group(ctx, &texture, request.raster_opacity);
            // The SAME per-tile model uniform shape the vector draw uses
            // -- `combined_tile_matrix`, the tile's `GlobeParams`, the
            // camera's fractional zoom as `blend_t`. A cover tile is drawn
            // whole; a `raster_stand_ins` ancestor is clipped to the
            // missing tile's footprint, as `MapView::paint` clips its own.
            let (model_buffer, model_bind_group) = pipeline.create_model_bind_group_clipped(
                ctx,
                crate::render::combined_tile_matrix(&camera, tile, MVT_EXTENT as f32)
                    .to_cols_array_2d(),
                camera.zoom.fract() as f32,
                crate::render::dash_scale(&camera, tile, MVT_EXTENT as f32),
                globe.for_tile(tile),
                clip_to.map_or(crate::render::TileClip::WHOLE, |m| crate::render::TileClip::for_descendant(tile, m)),
            );
            raster_held.push((
                key,
                model_bind_group,
                paint_bind_group,
                model_buffer,
                paint_buffer,
                texture,
            ));
        }
    }
    let raster_draws: Vec<(
        &crate::render::raster_pipeline::RasterBuffers,
        &wgpu::BindGroup,
        &wgpu::BindGroup,
    )> = raster_held
        .iter()
        .map(|(key, model, paint, _, _, _)| (&raster_squares[key], model, paint))
        .collect();
    let raster = raster_pipeline
        .as_ref()
        .filter(|_| !raster_draws.is_empty())
        .map(|pipeline| crate::render::gpu::RasterDraws { pipeline, draws: &raster_draws, after_tile_draws: 0 });
    // **The labels, shaped and placed BEFORE the encoder opens.** glyphon's
    // `prepare` uploads to its own atlas and can fail (`AtlasFull`), so it
    // belongs outside the pass for the same reason `MapView::paint` keeps it
    // outside: a failed prepare must cost this frame its text and nothing
    // else.
    //
    // Every step between the plans and `areas` is `symbol_pass`, which is
    // the whole point -- a capture that placed labels its own way could not
    // grade a change to how production places them. See
    // `CaptureRequest::labels` for the three things it does less than
    // `paint`.
    let basemap_labels = request.labels && !symbol_plans.is_empty();
    let mut overlay_labels_drawn = 0;
    let has_story_labels = request.story_layers.iter().any(|l| l.line_fx[2] > 0.0 && !l.labels.is_empty());
    // The sprite sheet's atlas, packed once per distinct `Arc` and kept.
    // Without one the rects are empty and the size `(0, 0)`, the signal
    // `symbol_pass` reads to draw every icon-bearing feature's text and
    // skip its icon -- the frame every capture drew before sprites.
    let icon_atlas = match request.sprites.as_ref() {
        Some(sprites) => {
            if !icons.as_ref().is_some_and(|cached| std::sync::Arc::ptr_eq(&cached.source, sprites)) {
                let pipeline = icon_pipeline
                    .get_or_insert_with(|| crate::render::icon_pipeline::IconPipeline::new(ctx, format));
                *icons = Some(IconAtlasGpu::new(ctx, pipeline, sprites));
            }
            icons.as_ref()
        }
        None => None,
    };
    let no_icon_rects: std::collections::HashMap<String, crate::render::IconRect> = std::collections::HashMap::new();
    let (icon_rects, icon_atlas_size) = match icon_atlas.as_ref() {
        Some(atlas) => (&atlas.rects, atlas.size),
        None => (&no_icon_rects, (0, 0)),
    };
    // The placed icons, in draw order, filled by the label closure below.
    let mut icon_instances: Vec<crate::render::icon_pipeline::IconInstance> = Vec::new();
    // The line-placed labels' glyph quads, in draw order, UVs normalised
    // to the glyph atlas texture uploaded for them; empty unless
    // `request.line_labels`.
    let mut glyph_instances: Vec<crate::render::icon_pipeline::IconInstance> = Vec::new();
    // Glyph requests of this frame's line labels the atlas could not hold.
    let mut line_glyphs_dropped = 0;
    // The burn-in chrome laid out ONCE, before label placement (final review
    // V1): its footprints -- the credit plate, the mark, the wordmark and
    // every story element -- are label obstacles below, and the chrome pass
    // draws this same layout, so a label can never sit under a plate. The
    // mark's artwork size is the vendored PNG's (both variants share it),
    // as `paint` lays it out.
    let reshapes_before = fonts.as_ref().map_or(0, |f| f.shapes.reshape_count);
    let pre_layout_started = std::time::Instant::now();
    let chrome_layout = super::chrome::wanted(request).then(|| {
        let font_state = fonts_for(fonts, &request.label_fonts);
        // The credit pill's tone against what is behind the corner, as
        // `paint` decides it (the mark's backdrop).
        let tone = crate::attribution::AttributionTone::for_background(
            crate::brand_mark::backdrop(style_background(request, style), &crate::render::space::Space::default(), space_transition)
                .map(|c| [c.r, c.g, c.b]),
        );
        super::chrome::layout(request, font_state, request.effective_brand_mark().map(|_| crate::brand_mark::artwork_size()), tone)
    });
    let chrome_layout_ms = pre_layout_started.elapsed().as_secs_f64() * 1e3;
    // The story's marks (spec §6.3-§6.4, §7), placed once from the chrome
    // layout above by the live map's own builder
    // (`story_overlays::capture_frame_marks`): the markers, drawn in the
    // symbol pass under every label; the point labels, over the map's;
    // the arrowheads, above the labels. Markers and labels are obstacles
    // to the basemap's labels (§7: placed before them, so a story mark
    // wins), and a label under the card is hidden with a warning.
    let story_marks = super::story_overlays::capture_frame_marks(request, &camera, &frame, chrome_layout.as_ref(), fonts);
    #[cfg(test)]
    {
        session.last_story_marks = story_marks.clone();
    }
    let marker_instances = &story_marks.markers;
    let story_label_warnings = story_marks.warnings.clone();
    let labels = (basemap_labels || request.overlay_labels.is_some() || has_story_labels)
        .then(|| {
            let FontState { system: font_system, shapes: shape_cache, glyphs: line_glyphs, .. } =
                fonts_for(fonts, &request.label_fonts);
            // Overlay extrusion layers join the same occlusion on a flat
            // frame (`ExtrusionStyle::occlude_labels`), as on the map.
            let overlay_occluders: Vec<_> = if space_transition == 0.0 {
                request.extrusions.iter().flat_map(|e| crate::overlay::extrude::label_occluders(&e.set, &e.style)).collect()
            } else {
                Vec::new()
            };
            let building_occlusion = (!occluders.is_empty() || !overlay_occluders.is_empty()).then(|| {
                let meters_to_world = if extrusion_plan.meters_to_world > 0.0 {
                    extrusion_plan.meters_to_world
                } else {
                    crate::overlay::extrude::meters_to_world(camera.center_lat)
                };
                // No memory: a capture decides every label afresh, with no
                // hysteresis and no budget, so it is deterministic.
                crate::render::extrusion::BuildingOcclusion::new(
                    &camera,
                    meters_to_world,
                    occluders.clone(),
                    &style.layers,
                    crate::render::extrusion::OcclusionLayers::default(),
                    None,
                )
                .with_overlay_occluders(overlay_occluders)
            });
            let symbol_frame = crate::symbol_pass::SymbolFrame {
                occlusion: building_occlusion.as_ref(),
                label_paint: None,
                camera: &camera,
                style_layers: &style.layers,
                // `1.0` at the default ratio, where physical and logical
                // space are the same space and every `* scale_factor` below
                // is a no-op -- see `CaptureRequest::pixel_ratio`.
                scale_factor: request.pixel_ratio,
                size_scale: request.pixel_ratio,
                // Physical, as `SymbolFrame::collision_padding` wants and
                // `MapView::paint` and the browser build pass it: scaled by
                // the ratio (a no-op at 1).
                collision_padding: crate::symbol_pass::SYMBOL_COLLISION_PADDING_LOGICAL_PX * request.pixel_ratio,
                size_blend_t: camera.zoom.fract() as f32,
                icon_rects,
                icon_atlas_size,
                globe: crate::render::globe::GlobeAnchors::new(&frame),
                // **The SAME sun and the SAME night the ground above was
                // drawn with**, from the same two values `GlobeUpload::new`
                // was handed -- not a second resolve. `night_brightness`
                // rather than `night_floor`: `SymbolLight::new` applies the
                // space fade itself, over a raised floor
                // (`render::SymbolLight`), so handing it the pre-faded
                // ground value would fade twice.
                //
                // A request with no `sun` gets `night_brightness` 1.0
                // above, which makes this light unshaded -- so an unlit
                // capture is byte-identical to one taken before labels
                // could be lit at all.
                light: crate::render::SymbolLight::new(
                    sun,
                    night_brightness,
                    frame.space_transition,
                ),
                // The fade written into the camera uniform above.
                fog,
            };
            let mut candidates = Vec::new();
            let mut bypassed = Vec::new();
            // A fresh layout per frame (see `LineGlyphs`): whether a glyph
            // fits, and where it lands, depends on this frame alone.
            if request.line_labels {
                line_glyphs.atlas.clear_layout();
                line_glyphs.atlas.set_max_height(glyph_atlas_max_height);
            }
            let refused_before = line_glyphs.atlas.refused_for_space();
            // A real `GlyphSink` only when asked: `None` is the frame the
            // golden hashes were blessed on (see `CaptureRequest::labels`).
            let mut glyph_sink =
                crate::symbol_pass::GlyphSink { atlas: &mut line_glyphs.atlas, swash: &mut line_glyphs.swash };
            crate::symbol_pass::collect_candidates(
                &symbol_frame,
                symbol_plans
                    .iter()
                    .filter(|_| basemap_labels)
                    .filter_map(|(tile, baked)| baked.symbol_plan.as_ref().map(|plan| (*tile, plan))),
                font_system,
                shape_cache,
                request.line_labels.then_some(&mut glyph_sink),
                &mut candidates,
                &mut bypassed,
            );
            // Whatever this frame's own glyphs could not fit (one frame
            // asking for more distinct glyphs than the atlas holds): counted,
            // never silent -- each is a street name with a letter missing.
            line_glyphs_dropped = (line_glyphs.atlas.refused_for_space() - refused_before) as usize;
            if let Some(occlusion) = building_occlusion.as_ref() {
                label_occlusion_stats = occlusion.stats();
            }
            let mut index = crate::collision::CollisionIndex::new(
                crate::symbol_pass::symbol_collision_bounds(
                    camera.viewport_width_px,
                    camera.viewport_height_px,
                    request.pixel_ratio,
                ),
            );
            // The route's screen path first, as in `MapView::paint`.
            if let Some(nav) = &request.nav {
                for route_box in nav.host.collision_boxes(&camera, request.pixel_ratio) {
                    index.insert_obstacle(route_box);
                }
            }
            // The burned-in chrome -- the credit plate, the mark, the
            // wordmark and every story element -- from the layout the
            // chrome pass draws, so no label is placed under any of it.
            for &rect in chrome_layout.iter().flat_map(|c| &c.label_obstacles) {
                index.insert_obstacle(crate::symbol_pass::chrome_obstacle_rect(rect, request.pixel_ratio));
            }
            // The story's point labels and markers (spec §7), placed
            // BEFORE the basemap's symbols: a story label wins any
            // collision.
            for obstacle in story_marks.obstacles() {
                index.insert_obstacle(obstacle);
            }
            let mut drawn =
                crate::symbol_pass::place_visible_symbols(candidates, &mut index);
            #[cfg(test)]
            {
                session.last_placed_boxes = drawn.iter().flat_map(|d| d.text_box.into_iter().chain(d.icon_box)).collect();
            }
            drawn.extend(bypassed);
            crate::symbol_pass::sort_symbols_for_drawing(&mut drawn);
            crate::symbol_pass::clip_labels_behind_later_icons(&mut drawn);
            // `label_plan::label_draw_plan`'s split: every icon in draw
            // order (they all go under every label, as in `paint`), the
            // labels with their clips, and the line labels' glyph quads.
            let mut symbol_text = Vec::new();
            #[cfg(test)]
            session.last_line_label_texts.clear();
            for symbol in drawn {
                icon_instances.extend(symbol.icons);
                if let Some(text) = symbol.text {
                    symbol_text.push((text, symbol.text_clip));
                }
                if let Some(run) = symbol.glyphs {
                    #[cfg(test)]
                    session.last_line_label_texts.push(run.text.clone());
                    glyph_instances.extend(run.quads);
                }
            }
            // The glyph atlas's texture and the UVs that address it,
            // settled together, as `map_view.rs` and the browser build do:
            // the atlas grew while `collect_candidates` rastered, so it is
            // re-uploaded when its generation moved, and every quad's
            // texel UVs are divided by the size that upload has.
            if !glyph_instances.is_empty() {
                let generation = line_glyphs.atlas.generation();
                if line_glyphs.bind_group.is_none() || line_glyphs.generation != Some(generation) {
                    let pipeline = icon_pipeline
                        .get_or_insert_with(|| crate::render::icon_pipeline::IconPipeline::new(ctx, format));
                    line_glyphs.bind_group = Some(pipeline.upload_atlas(ctx, line_glyphs.atlas.rgba()));
                    line_glyphs.generation = Some(generation);
                }
                let (_, atlas_width, atlas_height) = line_glyphs.atlas.image();
                crate::symbol_pass::normalise_glyph_uvs(&mut glyph_instances, atlas_width, atlas_height);
            }
            // The overlay's labels over the basemap's, as `paint` draws a
            // bypassing overlay symbol (every race count label bypasses).
            if let Some(json) = &request.overlay_labels {
                let overlay = capture_overlay_labels(json, &camera, &frame, display_zoom, font_system, shape_cache);
                overlay_labels_drawn = overlay.len();
                symbol_text.extend(overlay.into_iter().map(|draw| (draw, None)));
            }
            // The story's point labels and numerals, over everything the
            // map labelled, in the design face (spec §4.1's map label).
            symbol_text.extend(super::story_overlays::story_label_runs(&story_marks.labels, font_system, shape_cache));
            if symbol_text.is_empty() {
                return None;
            }

            let glyphon_cache = glyphon::Cache::new(&ctx.device);
            let mut viewport = glyphon::Viewport::new(&ctx.device, &glyphon_cache);
            // `ColorMode::Web`, the mode `MapView` builds its atlas with --
            // see the comment there. A capture that composited its glyphs
            // in a different colour space from production would be a frame
            // nobody draws.
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
            viewport.update(
                &ctx.queue,
                glyphon::Resolution { width: request.width, height: request.height },
            );
            let areas = crate::symbol_pass::text_areas(
                &symbol_text,
                shape_cache,
                request.width as f32,
                request.height as f32,
            );
            let placed = symbol_text.len();
            renderer
                .prepare(
                    &ctx.device,
                    &ctx.queue,
                    font_system,
                    &mut atlas,
                    &viewport,
                    areas,
                    &mut swash,
                )
                .ok()?;
            Some((glyphon_cache, viewport, atlas, renderer, placed))
        })
        .flatten();

    let mut encoder =
        ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    pipeline.render_with_space_and_raster(
        &mut encoder,
        &color_view,
        &depth_view,
        clear_color(request, style, space_transition, sky_pipeline.is_some()),
        &camera_bind_group,
        &space,
        raster,
        &draws,
        None,
    );
    // The story layers over the buildings (`StoryLayerDraw::above_buildings`):
    // their own pass right after the tile pass, as `MapView::paint` orders
    // it, before the donuts and the labels. Nothing is opened when empty,
    // so a frame without one is unchanged.
    pipeline.render_overlay_above_buildings(&mut encoder, &color_view, &depth_view, &camera_bind_group, &above_buildings_draws);
    // The arcs, first in `paint`'s symbol pass: over the tile pass, under
    // the donuts and the labels.
    if !request.arcs.is_empty() {
        let draws: Vec<crate::render::arcs::ArcLayerDraw<'_>> = request
            .arcs
            .iter()
            .enumerate()
            .map(|(i, a)| crate::render::arcs::ArcLayerDraw {
                id: ARC_IDS[i.min(ARC_IDS.len() - 1)],
                generation: 1,
                set: &a.set,
                style: a.style,
                filter: a.window.and_then(|w| a.set.relative_window(w)),
            })
            .collect();
        let frame_arcs = crate::render::arcs::ArcFrame::new(&camera, &frame, request.pixel_ratio);
        let mut arcs = crate::render::arcs::ArcPipeline::new(ctx, format, None);
        arcs.prepare(ctx, &frame_arcs, &draws);
        arcs.accumulate(ctx, &mut encoder, (request.width, request.height));
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("capture-arc-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &color_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        arcs.draw(&mut pass);
    }
    // The trips, right after the arcs as in `paint`'s symbol pass.
    if !request.trips.is_empty() {
        let draws: Vec<crate::render::trips::TripLayerDraw<'_>> = request
            .trips
            .iter()
            .enumerate()
            .map(|(i, t)| crate::render::trips::TripLayerDraw {
                id: ARC_IDS[i.min(ARC_IDS.len() - 1)],
                generation: 1,
                set: &t.set,
                style: t.style,
                clock: t.clock,
            })
            .collect();
        let frame_trips = crate::render::trips::TripFrame::new(&camera, &frame, request.pixel_ratio);
        let mut trips = crate::render::trips::TripPipeline::new(ctx, format, None);
        trips.prepare(ctx, &frame_trips, &draws);
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("capture-trip-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &color_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        trips.draw(&mut pass);
    }
    // The donuts, at the start of `paint`'s symbol pass: over the tile
    // pass, under the labels. Their own pass here only because glyphon's
    // below needs one of its own.
    let (donut_instances, donut_batches, donuts_truncated) = capture_donuts(request, &camera, &frame);
    if !donut_instances.is_empty() {
        let mut donuts = crate::render::donut::DonutPipeline::new(ctx, format);
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("capture-donut-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &color_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        donuts.render_batches(ctx, &mut pass, &donut_instances, &donut_batches, request.width as f32, request.height as f32);
    }
    // The altitude layer, right after the donuts and before the labels, as
    // in `paint`'s symbol pass. Only on a globe frame (R3).
    let mut altitude_points_drawn = 0;
    if let Some(altitude) = request.altitude.as_ref() {
        let (m, s, delta) = match altitude.keyframes.as_ref() {
            Some(k) => (
                match k.frame {
                    crate::render::altitude::RefFrame::Inertial => crate::render::altitude::teme_to_sphere(k.scene_unix),
                    crate::render::altitude::RefFrame::EarthFixed => glam::DMat3::IDENTITY,
                },
                ((k.scene_unix - k.t0_unix) / k.delta_s) as f32,
                k.delta_s as f32,
            ),
            None => (glam::DMat3::IDENTITY, 0.0, 60.0),
        };
        if let Some(af) = crate::render::altitude_pipeline::AltitudeFrame::new(&camera, &frame, m, s, delta, 1.0) {
            let (main, emphasis) = match altitude.keyframes.as_ref() {
                Some(k) => {
                    let hidden: Vec<[f32; 2]> = k
                        .k0
                        .iter()
                        .zip(&k.k1)
                        .map(|(a, b)| match (a.valid, b.valid) {
                            (true, true) => [0.0, 1.0],
                            (true, false) => [0.0, 0.5],
                            (false, true) => [0.5, 1.0],
                            (false, false) => [1.0, 0.0],
                        })
                        .collect();
                    crate::render::altitude_pipeline::keyed_instances_for(&k.k0, &k.k1, k.delta_s, &hidden, &altitude.style, &[], altitude.emphasis)
                }
                None => {
                    let sphere: Vec<_> = altitude.points.iter().map(|&p| crate::render::altitude::geo_to_sphere(p)).collect();
                    crate::render::altitude_pipeline::instances_for(&sphere, &altitude.style, &[], altitude.emphasis)
                }
            };
            let af = crate::render::altitude_pipeline::AltitudeFrame { rate: altitude.keyframes.as_ref().map_or(0.0, |k| k.rate as f32), ..af };
            let points = altitude_pipeline.get_or_insert_with(|| crate::render::altitude_pipeline::AltitudePipeline::new(ctx, format, None));
            points.prepare_points(ctx, &af, &altitude.style, &main, &emphasis);
            altitude_points_drawn = points.points_drawn();
            // The line layer: its own `M` and width, drawn before the points.
            let segments = crate::render::altitude_pipeline::line_instances(&altitude.lines);
            let lines_m = match altitude.lines_frame {
                crate::render::altitude::RefFrame::Inertial => crate::render::altitude::teme_to_sphere(altitude.lines_scene_unix),
                crate::render::altitude::RefFrame::EarthFixed => glam::DMat3::IDENTITY,
            };
            let line_frame = crate::render::altitude_pipeline::AltitudeFrame { m: lines_m, ..af };
            let line_batch = (!segments.is_empty()).then(|| points.prepare_lines(ctx, &line_frame, altitude.lines_width_px, altitude.lines_look, &segments));
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("capture-altitude-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &color_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if let Some(batch) = line_batch.as_ref() {
                points.draw_lines(&mut pass, batch);
            }
            points.draw_points(&mut pass);
        }
    }
    // A SECOND pass, loading what the first one wrote and with no depth
    // attachment at all -- `MapView::paint`'s symbol pass, which draws over
    // the tiles AND over the atmosphere (Task 9's decision: "a label is not
    // hazed by 100 km of simulated air"). Labels have no depth of their
    // own; the planet already hid the ones behind it, analytically, in
    // `AnchorProjection::occluded`.
    let icons_drawn = if icon_atlas.is_some() { icon_instances.len() } else { 0 };
    let line_glyph_atlas = fonts.as_ref().and_then(|f| f.glyphs.bind_group.as_ref()).filter(|_| !glyph_instances.is_empty());
    let line_glyphs_drawn = if line_glyph_atlas.is_some() { glyph_instances.len() } else { 0 };
    if labels.is_some() || icons_drawn > 0 || line_glyphs_drawn > 0 || !marker_instances.is_empty() {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("capture-symbol-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &color_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        // The story markers first (spec §6.3-§6.4): map content, under
        // every icon and label, over the tiles and the story's lines.
        if !marker_instances.is_empty() {
            let markers = story_markers.get_or_insert_with(|| crate::render::marker_pipeline::MarkerPipeline::new(ctx, format));
            markers.render(ctx, &mut pass, marker_instances, request.width as f32, request.height as f32);
        }
        // Icons BEFORE text, as `paint` and the browser build order them:
        // this pass has no depth test, and a route number has to land on
        // its shield, not under it.
        //
        // The street names ride in the SAME call as a second batch (one
        // pipeline, one vertex buffer, two draws with two bind groups),
        // after the icons, as the browser build draws them.
        let mut batches: Vec<(&wgpu::BindGroup, &[crate::render::icon_pipeline::IconInstance])> = Vec::new();
        if let Some(atlas) = icon_atlas.filter(|_| icons_drawn > 0) {
            batches.push((&atlas.bind_group, &icon_instances));
        }
        if let Some(glyph_atlas) = line_glyph_atlas {
            batches.push((glyph_atlas, &glyph_instances));
        }
        if let (false, Some(pipeline)) = (batches.is_empty(), icon_pipeline.as_mut()) {
            pipeline.render_batches(ctx, &mut pass, &batches, request.width as f32, request.height as f32);
        }
        // A render error here loses the text and keeps the frame, which is
        // the same trade `paint` makes.
        if let Some((_cache, viewport, atlas, renderer, _)) = labels.as_ref() {
            let _ = renderer.render(atlas, viewport, &mut pass);
        }
    }
    // The overlay above the labels, when asked: its own pass, after them
    // and before the nav chrome, as `MapView::paint` orders it.
    pipeline.render_overlay_above_labels(&mut encoder, &color_view, &depth_view, &camera_bind_group, &above_draws);
    // The story arrowheads (spec §6.4), above the labels with the marks
    // (a drawn arrow's end is UI, as the draw head is): an SDF triangle at
    // each `arrow` line's last vertex, turned to the last segment's
    // bearing on screen, the same projection the overlay's vertices take.
    // The projection is built only for a frame that draws one.
    let arrowheads = &story_marks.arrowheads;
    if !arrowheads.is_empty() {
        let gpu = story_arrowheads.get_or_insert_with(|| super::story_overlays::ArrowheadGpu::new(ctx, format));
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("capture-story-arrowheads"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &color_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        gpu.draw(ctx, &mut pass, arrowheads, request.width as f32, request.height as f32);
    }
    // The navigation chrome in its own pass over everything, at a scale
    // factor of 1 (one framebuffer pixel is one logical pixel here).
    if let Some(nav) = &request.nav {
        let scene = nav.host.scene(&camera, &nav.frame);
        if !scene.is_empty() {
            let mut chrome = crate::render::nav_layer::NavChromeGpu::new(ctx, format);
            // The session's own system-font state for the chrome, built on
            // the first nav frame; the label state is left alone.
            let font_system = nav_system.get_or_insert_with(crate::font_fallback::new_font_system);
            chrome.render(
                ctx,
                &mut encoder,
                &color_view,
                &scene,
                Some(font_system),
                (request.width, request.height),
                1.0,
            );
        }
    }
    // The story burn-in (`capture::chrome`, spec §5.4): captions, the
    // credit and the SWAT mark, over everything. A request that names none
    // of them builds nothing and draws nothing (the golden frames).
    let burn_in_started = std::time::Instant::now();
    let mut layout_warnings = Vec::new();
    if let Some(chrome_layout) = chrome_layout {
        // The mark's artwork against what is BEHIND it, through the one
        // composition `paint` uses; see `brand_mark::resolve_for_frame`.
        let variant = request.effective_brand_mark().map(|mark| {
            crate::brand_mark::resolve_for_frame(
                mark.variant,
                style_background(request, style),
                &crate::render::space::Space::default(),
                space_transition,
            )
        });
        let chrome = chrome.get_or_insert_with(|| super::chrome::ChromeGpu::new(ctx, format));
        let font_state = fonts_for(fonts, &request.label_fonts);
        layout_warnings =
            super::chrome::encode(ctx, chrome, font_state, request, variant, chrome_layout, &mut encoder, &color_view);
    }
    // Spec §7: a story label under the card is hidden, and says so.
    for w in story_label_warnings {
        if !layout_warnings.iter().any(|x: &crate::story::layout::LayoutWarning| x.id == w.id && x.code == w.code) {
            layout_warnings.push(w);
        }
    }
    let burn_in_ms = burn_in_started.elapsed().as_secs_f64() * 1e3 + chrome_layout_ms;
    // The frame is over for the label shapes: trim the cache as the live
    // view does (`MapView::paint`), keeping what this frame drew, so a long
    // story holds one frame's shapes rather than every label on its route.
    // The one-shot path never needed this; it dropped the cache per frame.
    let (cap, expensive_kept) = *shape_cache_caps;
    let story_reshapes = fonts.as_ref().map_or(0, |f| f.shapes.reshape_count).saturating_sub(reshapes_before);
    if let Some(f) = fonts.as_mut() {
        #[cfg(test)]
        {
            session.last_frame_shapes_used = f.shapes.used_this_frame.len();
        }
        f.shapes.end_frame_with_caps(cap, expensive_kept);
    }

    let bytes_per_row = (request.width * 4).next_multiple_of(256);
    let readback = crate::render::TrackedBuffer::new(
        &ctx.device,
        &wgpu::BufferDescriptor {
            label: Some("capture-readback"),
            size: u64::from(bytes_per_row) * u64::from(request.height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        },
    );
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &color_texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: Some(request.height),
            },
        },
        wgpu::Extent3d {
            width: request.width,
            height: request.height,
            depth_or_array_layers: 1,
        },
    );
    // A frame the test suite spoils on purpose, to prove a validation
    // error reaches the caller as an `Err` (see `catch_device_errors`):
    // a clear at an offset that is not a multiple of 4 fails validation
    // when the encoder finishes.
    #[cfg(test)]
    if session.inject_validation_error {
        encoder.clear_buffer(&readback, 1, None);
    }
    let submission = ctx.queue.submit(Some(encoder.finish()));
    take_device_error(device_errors.as_ref())?;
    let times = StageTimes {
        bake_ms: baked_at.duration_since(frame_started).as_secs_f64() * 1e3,
        encode_ms: (baked_at.elapsed().as_secs_f64() * 1e3 - burn_in_ms).max(0.0),
        burn_in_ms,
        readback_ms: 0.0,
    };

    let (tx, rx) = std::sync::mpsc::channel();
    readback.slice(..).map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });

    Ok(InFlight {
        _color_texture: color_texture,
        readback,
        bytes_per_row,
        submission,
        mapped: rx,
        times,
        frame: Capture {
            width: request.width,
            height: request.height,
            rgba: Vec::new(),
            tiles_drawn,
            tiles_missing: missing,
            vertices,
            triangles,
            transition,
            raster_tiles_drawn: raster_draws.len(),
            labels_drawn: labels.as_ref().map_or(0, |(.., placed)| *placed),
            label_occlusion: label_occlusion_stats,
            mask_ring_tiles: ring_held.len(),
            donuts_drawn: donut_instances.len(),
            donuts_truncated,
            altitude_points_drawn,
            overlay_labels_drawn,
            icons_drawn,
            line_glyphs_drawn,
            line_glyphs_dropped,
            story_retessellations,
            story_reshapes,
            chrome_layout_us: chrome_layout_ms * 1e3,
            layout_warnings,
        },
    })
}

/// A submitted frame whose pixels have not been read yet: its colour
/// target, its readback buffer (already asked to map) and every per-frame
/// counter. [`read_back`] turns it into the [`Capture`].
pub(crate) struct InFlight {
    /// Held so the target outlives the copy out of it, whatever wgpu's own
    /// lifetime tracking does.
    _color_texture: wgpu::Texture,
    readback: crate::render::TrackedBuffer,
    bytes_per_row: u32,
    submission: wgpu::SubmissionIndex,
    mapped: std::sync::mpsc::Receiver<Result<(), wgpu::BufferAsyncError>>,
    /// The frame's counters, with `rgba` still empty.
    frame: Capture,
    /// Where its encode time went; `readback_ms` is filled at read-back.
    times: StageTimes,
}

/// Wait for `in_flight`'s submission, copy its rows out tightly packed and
/// unmap. `device_errors` is the session's `catch_device_errors` slot.
pub(crate) fn read_back(
    ctx: &GpuContext,
    device_errors: Option<&DeviceErrorSlot>,
    in_flight: InFlight,
) -> anyhow::Result<Capture> {
    let InFlight { _color_texture, readback, bytes_per_row, submission, mapped, mut frame, .. } = in_flight;
    ctx.device.poll(wgpu::PollType::Wait { submission_index: Some(submission), timeout: None })?;
    take_device_error(device_errors)?;
    mapped.recv()??;
    let slice = readback.slice(..);
    let padded = slice.get_mapped_range().map_err(|err| anyhow::anyhow!("{err:?}"))?;

    let row_bytes = (frame.width * 4) as usize;
    let mut rgba = Vec::with_capacity(row_bytes * frame.height as usize);
    for row in 0..frame.height as usize {
        let start = row * bytes_per_row as usize;
        rgba.extend_from_slice(&padded[start..start + row_bytes]);
    }
    drop(padded);
    readback.unmap();
    frame.rgba = rgba;
    Ok(frame)
}

/// Where `catch_device_errors`' handler leaves the device's first
/// uncaptured error until a frame takes it.
pub(crate) type DeviceErrorSlot = std::sync::Arc<std::sync::Mutex<Option<String>>>;

/// `Err` if the device reported an error since the last take. A no-op
/// without `catch_device_errors` (wgpu's panicking default is then in
/// force and there is nothing to take).
fn take_device_error(slot: Option<&DeviceErrorSlot>) -> anyhow::Result<()> {
    let Some(slot) = slot else { return Ok(()) };
    let taken = slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
    match taken {
        Some(error) => Err(anyhow::anyhow!("device error: {error}")),
        None => Ok(()),
    }
}

/// Which pass a story overlay draw goes to (`CaptureRequest::story_layers`).
#[derive(Clone, Copy)]
enum StorySlot {
    /// The tile pass, under the buildings and the labels.
    Tiles,
    /// The above-buildings pass: over the extrusions, under the labels.
    AboveBuildings,
    /// The above-labels pass.
    AboveLabels,
}

/// Layer ids for a capture's arc layers (the pipeline keys its buffers by
/// id); the last is shared past eight layers, which only re-uploads.
const ARC_IDS: [&str; 8] = ["arcs-0", "arcs-1", "arcs-2", "arcs-3", "arcs-4", "arcs-5", "arcs-6", "arcs-7"];

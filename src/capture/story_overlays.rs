//! The story overlay layers' meshes, kept across a session's frames
//! (story-content 1a, Task 7).
//!
//! A story overlay layer ([`StoryLayerDraw`]) is tessellated ONCE, on the
//! first frame that draws it (or when the display zoom its paint was
//! resolved at changes), and drawn from this cache on every later frame.
//! What an effect changes per frame costs no re-tessellation:
//!
//! - **trim and opacity** (`draw`, `show`, `hide`, `fade`) are the model
//!   uniform's `line_fx`, written inside the one per-frame whole-uniform
//!   write ([`TilePipeline::write_model_uniform_fx`], T5 review F3), with
//!   the camera matrix;
//! - **per-feature overrides** (`highlight`, `drop`, `pulse`) rewrite the
//!   paint attributes of the changed features' ops only, in the cached
//!   vertices, and upload that span (`render::mesh`'s `rewrite_one_op`, the
//!   body of `rewrite_paint_attributes_for_ops`).
//!
//! **Determinism.** Every override is rewritten from the PRISTINE plan op
//! plus that frame's override (never from the previous frame's vertices),
//! and a feature whose override went away is rewritten from the pristine
//! op, so a frame's vertices are the ones a fresh cache builds for the
//! same inputs: seeking equals playing at the frame level.
//!
//! **Paint.** The layer's paint is MapLibre style-spec JSON. Expressions
//! (and `line-gradient`) are declared as the source's `SourcePaint`;
//! constants -- and `line-width`, which `SourcePaint` does not take -- are
//! written into every feature's properties, where `FeaturePaint` reads
//! them. Named colours are accepted in every colour key.
use std::collections::BTreeMap;

use super::*;
use crate::overlay::plan::{BakeBracket, OverlayPlan};
use crate::render::mesh::OpSpan;
use crate::render::TileBuffers;
use crate::story::effects::FeatureFx;

/// The cache's byte budget (CPU vertices plus device buffers).
pub(crate) const STORY_OVERLAY_BUDGET: u64 = 256 << 20;

/// One cached layer's GPU half, `None` in [`CachedLayer`] when the layer
/// has nothing drawable.
struct LayerGpu {
    /// The tessellated vertices, kept so an override is rewritten in place
    /// (the indices live only on the device).
    vertices: Vec<crate::render::Vertex>,
    ranges: Vec<OpSpan>,
    buffers: TileBuffers,
    model: crate::render::TrackedBuffer,
    bind_group: wgpu::BindGroup,
    triangles: usize,
}

/// One layer's cached mesh and what it was built from.
pub(crate) struct CachedLayer {
    /// `None` when the layer has nothing drawable (cached empty, so it is
    /// not rebuilt every frame).
    plan: Option<OverlayPlan>,
    gpu: Option<LayerGpu>,
    display_zoom: u8,
    origin: (f64, f64),
    /// The overrides the cached vertices currently carry, in id order.
    applied: Vec<FeatureFx>,
    /// Feature id -> its position in `plan.feature_ops`.
    positions: BTreeMap<u64, usize>,
    last_used: u64,
    bytes: u64,
}

/// The session's story overlay meshes, by [`StoryLayerDraw::key`]; LRU by
/// bytes under [`STORY_OVERLAY_BUDGET`].
pub(crate) struct StoryOverlayCache {
    layers: BTreeMap<u64, CachedLayer>,
    budget: u64,
    frame: u64,
}

impl Default for StoryOverlayCache {
    fn default() -> Self {
        Self { layers: BTreeMap::new(), budget: STORY_OVERLAY_BUDGET, frame: 0 }
    }
}

/// `color`'s alpha times `k`.
fn scaled(color: Rgba, k: f32) -> Rgba {
    Rgba { a: color.a * k, ..color }
}

/// `base` with `fx` applied: `color` replaces the colour (its alpha times
/// the op's own, so a layer's `*-opacity` still holds), `alpha`
/// multiplies the alpha, `radius_scale` a dot's diameter.
fn overridden(base: &PaintOp, fx: &FeatureFx) -> PaintOp {
    let recolour = |c: Rgba| -> Rgba {
        let c = match fx.color {
            Some([r, g, b, a]) => Rgba {
                r: f32::from(r) / 255.0,
                g: f32::from(g) / 255.0,
                b: f32::from(b) / 255.0,
                a: f32::from(a) / 255.0 * c.a,
            },
            None => c,
        };
        scaled(c, fx.alpha)
    };
    let mut op = base.clone();
    match &mut op {
        PaintOp::Line { color_from, color_to, width_from, width_to, geometry, .. } => {
            *color_from = recolour(*color_from);
            *color_to = recolour(*color_to);
            if matches!(**geometry, geo_types::Geometry::MultiPoint(_)) {
                *width_from *= fx.radius_scale;
                *width_to *= fx.radius_scale;
            }
        }
        PaintOp::Fill { color_from, color_to, .. } => {
            *color_from = recolour(*color_from);
            *color_to = recolour(*color_to);
        }
        PaintOp::Background { .. } | PaintOp::Extrusion { .. } => {}
    }
    op
}

/// Whether a paint value is an expression (an array or object), not a constant.
fn is_expression(v: &serde_json::Value) -> bool {
    v.is_array() || v.is_object()
}

/// A constant colour as `#rrggbbaa` (what `FeaturePaint` reads from a
/// feature's properties), from any CSS form `parse_css_color` or the named
/// table takes; anything else unchanged (the store then names it).
fn as_hex(v: &serde_json::Value) -> serde_json::Value {
    let Some(text) = v.as_str() else { return v.clone() };
    let parsed = crate::style_spec::color::css_named_color(text).or_else(|| crate::style_spec::parse_css_color(text).ok());
    match parsed {
        Some(c) => {
            let [r, g, b, a] = crate::story::effects::rgba_bytes(c);
            serde_json::Value::String(format!("#{r:02x}{g:02x}{b:02x}{a:02x}"))
        }
        None => v.clone(),
    }
}

/// `paint` split into what `SourcePaint` declares (expressions,
/// `line-gradient`, and an opacity beside a declared colour) and the
/// constants every feature carries in its properties (and `line-width`).
/// Named colours are spelled out in every colour key first.
fn split_paint(
    paint: &serde_json::Map<String, serde_json::Value>,
) -> (serde_json::Map<String, serde_json::Value>, serde_json::Map<String, serde_json::Value>) {
    let mut declared = serde_json::Map::new();
    let mut constants = serde_json::Map::new();
    let spelled = |k: &str, v: &serde_json::Value| {
        if k.ends_with("-color") || k == "line-gradient" {
            crate::overlay::source_paint::with_named_colours_spelled_out(v)
        } else {
            v.clone()
        }
    };
    for (k, v) in paint {
        if k.ends_with("-opacity") {
            continue;
        }
        let v = spelled(k, v);
        // `line-dasharray` is an array but a constant: every feature
        // carries it (`FeaturePaint` reads it from the properties).
        // `line-cap` and `line-join` are the layer's stroke, which only
        // the source declares (`SourcePaint`, read by the plan).
        let stroke = k == "line-cap" || k == "line-join";
        // `circle-translate` and `text-offset` are arrays but constants too
        // (spec §6.4): the markers and labels read them from the paint.
        let constant_array = matches!(k.as_str(), "line-dasharray" | "circle-translate" | "text-offset");
        if stroke || (k != "line-width" && !constant_array && (k == "line-gradient" || is_expression(&v))) {
            declared.insert(k.clone(), v);
        } else if k.ends_with("-color") {
            // A feature's own colour property is hex only (`FeaturePaint`).
            constants.insert(k.clone(), as_hex(&v));
        } else {
            constants.insert(k.clone(), v);
        }
    }
    for (k, v) in paint.iter().filter(|(k, _)| k.ends_with("-opacity")) {
        let beside = match k.as_str() {
            "line-opacity" => declared.contains_key("line-color") || declared.contains_key("line-gradient"),
            "fill-opacity" => declared.contains_key("fill-color") || declared.contains_key("fill-outline-color"),
            _ => false,
        };
        if beside {
            declared.insert(k.clone(), v.clone());
        } else {
            constants.insert(k.clone(), v.clone());
        }
    }
    (declared, constants)
}

/// A story layer's `paint` split as a source holds it (`split_paint`):
/// what the source declares, and `geojson` with the constants written into
/// every feature's properties. Shared by the capture's [`layer_store`] and
/// the live map (`MapView::set_story_layer_source`), so the two hold one
/// layer the same way.
pub(crate) fn story_layer_parts<'g>(
    layer_id: &str,
    geojson: &'g str,
    paint: &serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<(serde_json::Map<String, serde_json::Value>, std::borrow::Cow<'g, str>)> {
    let (mut declared, constants) = split_paint(paint);
    // Ruling R8-a: a `["get", ..]` text-field is read per feature into its
    // `text-field` property (`text_field_for`), as the capture's labels
    // read it, never declared (the source cannot take it).
    let per_feature_text = declared.remove("text-field");
    let json = if constants.is_empty() && per_feature_text.is_none() {
        std::borrow::Cow::Borrowed(geojson)
    } else {
        let mut doc: serde_json::Value = serde_json::from_str(geojson)
            .map_err(|err| anyhow::anyhow!("story layer `{layer_id}`: GeoJSON is not JSON: {err}"))?;
        if let Some(features) = doc.get_mut("features").and_then(serde_json::Value::as_array_mut) {
            for feature in features {
                if let Some(obj) = feature.as_object_mut() {
                    let props = obj.entry("properties").or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
                    if !props.is_object() {
                        *props = serde_json::Value::Object(serde_json::Map::new());
                    }
                    if let Some(props) = props.as_object_mut() {
                        for (k, v) in &constants {
                            props.insert(k.clone(), v.clone());
                        }
                        if let Some(field) = &per_feature_text {
                            match crate::story::overlay_layers::text_field_for(field, Some(props)) {
                                Some(text) => props.insert("text-field".into(), serde_json::Value::String(text)),
                                None => props.remove("text-field"),
                            };
                        }
                    }
                }
            }
        }
        std::borrow::Cow::Owned(doc.to_string())
    };
    Ok((declared, json))
}

/// The store holding `geojson` with `paint` and `line_metrics`, as
/// `MapView` holds a source: parsed, declared and pushed at `display_zoom`.
pub(crate) fn layer_store(
    layer_id: &str,
    geojson: &str,
    paint: &serde_json::Map<String, serde_json::Value>,
    line_metrics: bool,
    display_zoom: u8,
) -> anyhow::Result<crate::overlay::OverlayStore> {
    let name = crate::overlay::DEFAULT_SOURCE;
    let (declared, json) = story_layer_parts(layer_id, geojson, paint)?;
    let mut store = crate::overlay::OverlayStore::new();
    if line_metrics {
        // From the source's CURRENT options (T6 review F7): a later
        // whole-struct declare must never silently turn metrics off.
        store
            .declare_source_line_metrics(name, true)
            .map_err(|err| anyhow::anyhow!("story layer `{layer_id}`: line metrics refused: {err:?}"))?;
    }
    // Always whole-source, whatever GPUI_MAP_OVERLAY_TILING says (review
    // #6): from the current options, so metrics stay as declared (F7).
    let options = crate::overlay::SourceOptions { whole_source: true, ..store.source_options(name).unwrap_or_default() };
    store
        .declare_source(name, options)
        .map_err(|err| anyhow::anyhow!("story layer `{layer_id}`: source options refused: {err:?}"))?;
    if !declared.is_empty() {
        store
            .declare_source_paint(name, &declared)
            .map_err(|err| anyhow::anyhow!("story layer `{layer_id}`: paint refused: {err:?}"))?;
    }
    store
        .replace(&json, display_zoom)
        .map_err(|err| anyhow::anyhow!("story layer `{layer_id}`: GeoJSON rejected: {err:?}"))?;
    Ok(store)
}

/// A feature id as the cache's `u64` (a story layer's ids are document
/// indices, `story::overlay_layers::layer_sources`).
fn id_u64(id: Option<&crate::overlay::geojson::FeatureId>) -> Option<u64> {
    match id? {
        crate::overlay::geojson::FeatureId::Number(n) if *n >= 0.0 && n.fract() == 0.0 => Some(*n as u64),
        _ => None,
    }
}

impl StoryOverlayCache {
    /// Start a frame: entries used from here on are this frame's (never
    /// evicted by [`Self::end_frame`]).
    pub(crate) fn begin_frame(&mut self) {
        self.frame += 1;
    }

    /// Evict least recently used layers until the cache is within budget,
    /// never one this frame drew.
    pub(crate) fn end_frame(&mut self) {
        let mut total: u64 = self.layers.values().map(|l| l.bytes).sum();
        while total > self.budget {
            let Some((&key, oldest)) = self.layers.iter().filter(|(_, l)| l.last_used < self.frame).min_by_key(|(_, l)| l.last_used) else {
                break;
            };
            total -= oldest.bytes;
            self.layers.remove(&key);
        }
    }

    /// Prepare `layer` for this frame: built on a miss or a display-zoom
    /// change (returns `true`); otherwise the cached mesh with its model
    /// uniform rewritten (camera matrix and `line_fx` in one write) and,
    /// when `layer.features` differs from what the vertices carry, the
    /// changed features' ops rewritten in place. Draw it with
    /// [`Self::draw_of`].
    ///
    /// # Errors
    /// The layer's GeoJSON or paint is refused, or its source is so large
    /// the store would tile it (story layers draw whole sources only).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare(
        &mut self,
        ctx: &GpuContext,
        pipeline: &TilePipeline,
        camera: &Camera,
        globe: &GlobeUpload,
        display_zoom: u8,
        layer: &StoryLayerDraw,
    ) -> anyhow::Result<bool> {
        let frame = self.frame;
        let rebuild = self.layers.get(&layer.key).is_none_or(|c| c.display_zoom != display_zoom);
        if rebuild {
            let built = Self::build(ctx, pipeline, camera, globe, display_zoom, layer)?;
            self.layers.insert(layer.key, built);
        }
        let Some(cached) = self.layers.get_mut(&layer.key) else { return Ok(rebuild) };
        cached.last_used = frame;
        if let (Some(gpu), Some(plan)) = (&cached.gpu, &cached.plan) {
            pipeline.write_model_uniform_fx(
                ctx,
                &gpu.model,
                crate::render::combined_overlay_matrix(camera, cached.origin).to_cols_array_2d(),
                plan.bracket.blend_t(camera.zoom),
                crate::render::overlay_dash_scale(camera),
                globe.for_overlay(cached.origin),
                TileClip::WHOLE,
                layer.line_fx,
            );
        }
        if cached.applied != layer.features {
            cached.apply(ctx, pipeline, &layer.features);
        }
        Ok(rebuild)
    }

    /// The draw of a layer [`Self::prepare`]d this frame: its buffers and
    /// model bind group, its vertex and triangle counts. `None` when it has
    /// nothing drawable.
    pub(crate) fn draw_of(&self, key: u64) -> Option<(&TileBuffers, &wgpu::BindGroup, usize, usize)> {
        let gpu = self.layers.get(&key)?.gpu.as_ref()?;
        Some((&gpu.buffers, &gpu.bind_group, gpu.vertices.len(), gpu.triangles))
    }

    fn build(
        ctx: &GpuContext,
        pipeline: &TilePipeline,
        camera: &Camera,
        globe: &GlobeUpload,
        display_zoom: u8,
        layer: &StoryLayerDraw,
    ) -> anyhow::Result<CachedLayer> {
        let store = layer_store(&layer.layer_id, &layer.geojson, &layer.paint, layer.line_metrics, display_zoom)?;
        let snapshot = store
            .source_snapshot(crate::overlay::DEFAULT_SOURCE)
            .expect("the default source exists for the store's whole life");
        let points = crate::overlay::tile_plan::tiling_points(&snapshot.features);
        anyhow::ensure!(
            !snapshot.tiled && points < crate::overlay::tile_plan::TILED_SOURCE_MIN_POINTS,
            "story layer `{}` has {points} or more line/polygon points, over the whole-source limit of {}; \
             simplify or split it (the player refuses this before rendering)",
            layer.layer_id,
            crate::overlay::tile_plan::TILED_SOURCE_MIN_POINTS
        );
        let bracket = BakeBracket::Bucket(display_zoom);
        let plan = if layer.per_feature {
            crate::overlay::plan::build_plan_per_feature(&snapshot.features, &snapshot.properties, bracket, &snapshot.paint)
        } else {
            crate::overlay::plan::build_plan(&snapshot.features, &snapshot.properties, bracket, &snapshot.paint)
        };
        let positions: BTreeMap<u64, usize> =
            snapshot.features.iter().enumerate().filter_map(|(at, f)| id_u64(f.id.as_ref()).map(|id| (id, at))).collect();
        let Some(plan) = plan else {
            return Ok(CachedLayer { plan: None, gpu: None, display_zoom, origin: (0.0, 0.0), applied: Vec::new(), positions, last_used: 0, bytes: 0 });
        };
        let (mut mesh, ranges) = crate::render::mesh::tessellate_overlay_plan_with_ranges(&plan.plan, plan.origin.coord_scale);
        plan.colour_mesh(&mut mesh.vertices);
        let origin = (plan.origin.world_x, plan.origin.world_y);
        let gpu = (!mesh.indices.is_empty()).then(|| {
            let buffers = pipeline.create_tile_buffers(ctx, &mesh);
            let (model, bind_group) = pipeline.create_model_bind_group_fx(
                ctx,
                crate::render::combined_overlay_matrix(camera, origin).to_cols_array_2d(),
                plan.bracket.blend_t(camera.zoom),
                crate::render::overlay_dash_scale(camera),
                globe.for_overlay(origin),
                TileClip::WHOLE,
                layer.line_fx,
            );
            let triangles = mesh.indices.len() / 3;
            LayerGpu { vertices: std::mem::take(&mut mesh.vertices), ranges, buffers, model, bind_group, triangles }
        });
        let bytes = gpu.as_ref().map_or(0, |g| {
            let vertex_bytes = (g.vertices.len() * std::mem::size_of::<crate::render::Vertex>()) as u64;
            // The CPU copy, and the device's vertex and index buffers.
            2 * vertex_bytes + (g.triangles * 3 * 4) as u64
        });
        Ok(CachedLayer { plan: Some(plan), gpu, display_zoom, origin, applied: Vec::new(), positions, last_used: 0, bytes })
    }
}

impl CachedLayer {
    /// Rewrite the ops of every feature whose override differs between
    /// `applied` and `features`, each from its PRISTINE plan op plus its
    /// new override (none: the pristine op), and upload those spans.
    fn apply(&mut self, ctx: &GpuContext, pipeline: &TilePipeline, features: &[FeatureFx]) {
        let old: BTreeMap<u64, &FeatureFx> = self.applied.iter().map(|f| (f.id, f)).collect();
        let new: BTreeMap<u64, &FeatureFx> = features.iter().map(|f| (f.id, f)).collect();
        let mut changed: Vec<u64> = old.keys().chain(new.keys()).copied().filter(|id| old.get(id) != new.get(id)).collect();
        changed.sort_unstable();
        changed.dedup();
        if let (Some(gpu), Some(plan)) = (&mut self.gpu, &self.plan) {
            for id in changed {
                let Some(ops) = self.positions.get(&id).and_then(|&at| plan.feature_ops.get(at)).cloned() else { continue };
                for op in ops {
                    let (Some(base), Some(span)) = (plan.plan.ops.get(op), gpu.ranges.get(op)) else { continue };
                    let span = span.vertices.clone();
                    if span.is_empty() {
                        continue;
                    }
                    let paint = match new.get(&id) {
                        Some(fx) => overridden(base, fx),
                        None => base.clone(),
                    };
                    let Some(slice) = gpu.vertices.get_mut(span.clone()) else { continue };
                    crate::render::mesh::rewrite_op_paint(slice, &paint);
                    plan.colour_mesh(slice);
                    pipeline.write_vertices_at(ctx, &gpu.buffers, span.start, slice);
                }
            }
        }
        self.applied = features.to_vec();
    }
}

/// One frame's marks (the draw head, drop and pulse rings): a tiny point
/// source rebuilt every frame, drawn right after its layer at the layer's
/// opacity. Not counted in `Capture::story_retessellations` (it is per
/// frame by design, a few hundred points at most).
pub(crate) struct MarksDraw {
    pub(crate) buffers: TileBuffers,
    pub(crate) bind_group: wgpu::BindGroup,
    _model: crate::render::TrackedBuffer,
    pub(crate) vertices: usize,
    pub(crate) triangles: usize,
}

/// `marks` (Point features with `circle-radius`/`circle-color`
/// properties) as one draw at `opacity`; `None` when nothing is drawable.
///
/// # Errors
/// The marks document is refused by the overlay parser.
#[allow(clippy::too_many_arguments)]
pub(crate) fn marks_draw(
    marks: &str,
    opacity: f32,
    ctx: &GpuContext,
    pipeline: &TilePipeline,
    camera: &Camera,
    globe: &GlobeUpload,
    display_zoom: u8,
) -> anyhow::Result<Option<MarksDraw>> {
    let mut store = crate::overlay::OverlayStore::new();
    store.replace(marks, display_zoom).map_err(|err| anyhow::anyhow!("story marks rejected: {err:?}"))?;
    let snapshot = store
        .source_snapshot(crate::overlay::DEFAULT_SOURCE)
        .expect("the default source exists for the store's whole life");
    let bracket = BakeBracket::Bucket(display_zoom);
    let Some(plan) = crate::overlay::plan::build_plan(&snapshot.features, &snapshot.properties, bracket, &snapshot.paint) else {
        return Ok(None);
    };
    let mesh = crate::render::tessellate_overlay_plan(&plan.plan, plan.origin.coord_scale);
    if mesh.indices.is_empty() {
        return Ok(None);
    }
    let origin = (plan.origin.world_x, plan.origin.world_y);
    let (model, bind_group) = pipeline.create_model_bind_group_fx(
        ctx,
        crate::render::combined_overlay_matrix(camera, origin).to_cols_array_2d(),
        bracket.blend_t(camera.zoom),
        crate::render::overlay_dash_scale(camera),
        globe.for_overlay(origin),
        TileClip::WHOLE,
        [0.0, 1.0, opacity, 0.0],
    );
    Ok(Some(MarksDraw {
        buffers: pipeline.create_tile_buffers(ctx, &mesh),
        bind_group,
        _model: model,
        vertices: mesh.vertices.len(),
        triangles: mesh.indices.len() / 3,
    }))
}

/// The story arrowheads' GPU state ([`StoryLayerDraw::arrowheads`], spec
/// §6.4): their own icon pipeline (one vertex buffer per pipeline per
/// frame, so it shares none) and one `story-arrowhead` SDF sprite per
/// stub width drawn ([`StubKey`]), each built on the first frame that
/// draws it and kept by the session.
pub(crate) struct ArrowheadGpu {
    icons: crate::render::IconPipeline,
    sprites: Vec<(StubKey, wgpu::BindGroup)>,
}

/// A sprite's stub width, quarter texels (units): the arrow core's width
/// as the sprite draws it.
pub(crate) type StubKey = u32;

impl ArrowheadGpu {
    pub(crate) fn new(ctx: &GpuContext, format: wgpu::TextureFormat) -> Self {
        Self { icons: crate::render::IconPipeline::new(ctx, format), sprites: Vec::new() }
    }

    /// Draws `quads` into `pass` (a `w`x`h` physical-pixel target), each
    /// with the sprite of its stub width (uploaded on first use).
    pub(crate) fn draw(&mut self, ctx: &GpuContext, pass: &mut wgpu::RenderPass<'_>, quads: &[(StubKey, crate::render::IconInstance)], w: f32, h: f32) {
        let mut keys: Vec<StubKey> = quads.iter().map(|(k, _)| *k).collect();
        keys.sort_unstable();
        keys.dedup();
        for &key in &keys {
            if !self.sprites.iter().any(|(k, _)| *k == key) {
                let sprite = self.icons.upload_atlas(ctx, &super::chrome::story_arrowhead_sprite(key as f32 / 4.0));
                self.sprites.push((key, sprite));
            }
        }
        let grouped: Vec<(StubKey, Vec<crate::render::IconInstance>)> =
            keys.iter().map(|&key| (key, quads.iter().filter(|(k, _)| *k == key).map(|(_, q)| *q).collect())).collect();
        let batches: Vec<(&wgpu::BindGroup, &[crate::render::IconInstance])> = grouped
            .iter()
            .filter_map(|(key, qs)| Some((&self.sprites.iter().find(|(k, _)| k == key)?.1, &qs[..])))
            .collect();
        if !batches.is_empty() {
            self.icons.render_batches(ctx, pass, &batches, w, h);
        }
    }
}

/// `head`'s stub as its sprite draws it ([`StubKey`]): the core's width in
/// the sprite's texels, clamped to `chrome::STORY_ARROWHEAD_STUB`.
pub(crate) fn stub_key(head: &StoryArrowhead) -> StubKey {
    use super::chrome::{STORY_ARROWHEAD_STUB, STORY_ARROWHEAD_W};
    let texels = if head.width > 0.0 { head.stub_width * STORY_ARROWHEAD_W / head.width } else { 0.0 };
    let texels = if texels.is_finite() { texels.clamp(STORY_ARROWHEAD_STUB.0, STORY_ARROWHEAD_STUB.1) } else { STORY_ARROWHEAD_STUB.0 };
    (texels * 4.0).round() as StubKey
}

/// One arrowhead's quad of the `story-arrowhead` sprite (physical
/// pixels), from its line end and the vertex before it already on screen
/// (`end` and `from`, LOGICAL pixels): the triangle's base centred on
/// `end`, pointing from `from` through `end`, its length and width the
/// arrowhead's times `pixel_ratio`, its fill the colour at `opacity`.
/// `None` when the two points coincide on screen (no bearing).
pub(crate) fn arrowhead_quad(head: &StoryArrowhead, end: (f32, f32), from: (f32, f32), pixel_ratio: f32, opacity: f32) -> Option<crate::render::IconInstance> {
    use super::chrome::{STORY_ARROWHEAD_BASE_Y, STORY_ARROWHEAD_LEN, STORY_ARROWHEAD_SIZE, STORY_ARROWHEAD_W};
    let (dx, dy) = (end.0 - from.0, end.1 - from.1);
    let len = dx.hypot(dy);
    if !(len.is_finite() && len > 1e-3) {
        return None;
    }
    let (ux, uy) = (dx / len, dy / len);
    let length = head.length * pixel_ratio;
    let width = head.width * pixel_ratio;
    // The sprite's texels per pixel along and across the arrowhead.
    let (along, across) = (length / STORY_ARROWHEAD_LEN, width / STORY_ARROWHEAD_W);
    let side = STORY_ARROWHEAD_SIZE as f32;
    let (qw, qh) = (side * across, side * along);
    // The base on the end: the sprite's centre is this far on from it.
    let on = (STORY_ARROWHEAD_BASE_Y - side / 2.0) * along;
    let (cx, cy) = (end.0 * pixel_ratio + ux * on, end.1 * pixel_ratio + uy * on);
    let [r, g, b, a] = head.color.map(|v| f32::from(v) / 255.0);
    let [er, eg, eb, ea] = head.edge_color.map(|v| f32::from(v) / 255.0);
    Some(crate::render::IconInstance {
        screen_x: cx - qw / 2.0,
        screen_y: cy - qh / 2.0,
        width: qw,
        height: qh,
        uv_min: (0.0, 0.0),
        uv_max: (1.0, 1.0),
        // The sprite points up (y down): clockwise from up to the bearing.
        rotation_rad: ux.atan2(-uy),
        sdf: Some(crate::render::SdfPaint {
            fill: [r, g, b, a * opacity],
            halo: [er, eg, eb, ea * opacity],
            halo_width: head.edge_width * pixel_ratio,
            font_scale: along.min(across),
        }),
        opacity: 1.0,
    })
}

/// Whether any of `layers` draws an arrowhead this frame (one with
/// arrowheads, at an opacity above 0): the capture builds the projection
/// [`arrowhead_quads`] needs only then.
pub(crate) fn any_arrowheads(layers: &[StoryLayerDraw]) -> bool {
    layers.iter().any(|l| l.line_fx[2] > 0.0 && !l.arrowheads.is_empty())
}

/// Every arrowhead `layers` draw this frame (each at its layer's opacity,
/// a layer at opacity 0 drawing none), with `project` taking a world point
/// to logical pixels (`render::globe::WorldProjector`), each with its
/// sprite's [`StubKey`].
///
/// **The antimeridian:** a story line's longitudes are already unwrapped
/// the short way (`story::overlay_layers`), so its end may sit past 180;
/// `from` is unwrapped against `end` here as well, so a last segment
/// handed over wrapped (179 to -179) still points the short way, as the
/// line itself is drawn.
pub(crate) fn arrowhead_quads(
    layers: &[StoryLayerDraw],
    project: impl Fn((f64, f64)) -> Option<(f32, f32)>,
    pixel_ratio: f32,
) -> Vec<(StubKey, crate::render::IconInstance)> {
    let world = |(lon, lat): (f64, f64)| crate::overlay::geom::lon_lat_to_world(lon, lat);
    let short_way = |from: (f64, f64), end: (f64, f64)| (from.0 + 360.0 * ((end.0 - from.0) / 360.0).round(), from.1);
    layers
        .iter()
        .filter(|l| l.line_fx[2] > 0.0)
        .flat_map(|l| l.arrowheads.iter().map(move |h| (h, l.line_fx[2])))
        .filter_map(|(h, opacity)| {
            let from = project(world(short_way(h.from, h.end)))?;
            Some((stub_key(h), arrowhead_quad(h, project(world(h.end))?, from, pixel_ratio, opacity)?))
        })
        .collect()
}

/// The circle paint keys that make a story circle layer draw as markers
/// (spec §6.4): the overlay's tessellated dots carry none of them.
/// `circle-opacity` and `circle-stroke-opacity` too: the mesh has no
/// circle opacity, so a layer that sets one draws as markers.
pub(crate) const MARKER_KEYS: [&str; 6] =
    ["circle-stroke-width", "circle-stroke-color", "circle-blur", "circle-translate", "circle-opacity", "circle-stroke-opacity"];

/// A colour value as straight sRGB bytes: any CSS form or name; `None`
/// for an expression or an unknown name.
fn colour_bytes(v: &serde_json::Value) -> Option<[u8; 4]> {
    let text = v.as_str()?;
    let c = crate::style_spec::color::css_named_color(text).or_else(|| crate::style_spec::parse_css_color(text).ok())?;
    Some(crate::story::effects::rgba_bytes(c))
}

/// A circle layer's points as markers (spec §6.4), when its LAYER paint
/// carries one of [`MARKER_KEYS`] (a feature property never switches a 1a
/// layer's draw): each Point (a MultiPoint's every point) with the
/// paint's constant, else -- for `circle-radius` and `circle-color` only,
/// as the mesh reads them -- the feature's own property, else MapLibre's
/// default (radius 5, black, no stroke, no blur, no translate).
/// `circle-opacity` and `circle-stroke-opacity` scale their colours'
/// alpha. `None` without a marker key: the layer draws as its mesh.
/// Expressions are not read (the default stands).
pub(crate) fn circle_markers(geojson: &str, paint: &serde_json::Map<String, serde_json::Value>) -> Option<Vec<StoryMarker>> {
    if !MARKER_KEYS.iter().any(|k| paint.contains_key(*k)) {
        return None;
    }
    let doc: serde_json::Value = serde_json::from_str(geojson).ok()?;
    let features = doc.get("features")?.as_array()?;
    let mut out = Vec::new();
    for f in features {
        let props = f.get("properties").and_then(serde_json::Value::as_object);
        let get = |k: &str| {
            let own = matches!(k, "circle-radius" | "circle-color").then(|| props.and_then(|p| p.get(k))).flatten();
            paint.get(k).filter(|v| !v.is_array() || k == "circle-translate").or(own)
        };
        let num = |k: &str, d: f32| get(k).and_then(serde_json::Value::as_f64).map_or(d, |v| v as f32);
        let colour = |k: &str, opacity: &str| {
            let [r, g, b, a] = get(k).and_then(colour_bytes).unwrap_or([0, 0, 0, 255]);
            [r, g, b, (f32::from(a) * num(opacity, 1.0).clamp(0.0, 1.0)).round() as u8]
        };
        let translate = get("circle-translate").and_then(serde_json::Value::as_array).map_or((0.0, 0.0), |a| {
            (a.first().and_then(serde_json::Value::as_f64).unwrap_or(0.0) as f32, a.get(1).and_then(serde_json::Value::as_f64).unwrap_or(0.0) as f32)
        });
        let id = f.get("id").and_then(serde_json::Value::as_u64);
        let marker = |at: (f64, f64)| StoryMarker {
            at,
            radius: num("circle-radius", 5.0).max(0.0),
            fill: colour("circle-color", "circle-opacity"),
            stroke_width: num("circle-stroke-width", 0.0).max(0.0),
            stroke: colour("circle-stroke-color", "circle-stroke-opacity"),
            blur: num("circle-blur", 0.0).max(0.0),
            translate,
            id,
        };
        let Some(g) = f.get("geometry") else { continue };
        let pos = |c: &serde_json::Value| Some((c.get(0)?.as_f64()?, c.get(1)?.as_f64()?));
        match g.get("type").and_then(serde_json::Value::as_str) {
            Some("Point") => out.extend(g.get("coordinates").and_then(pos).map(marker)),
            Some("MultiPoint") => out.extend(g.get("coordinates").and_then(serde_json::Value::as_array).into_iter().flatten().filter_map(pos).map(marker)),
            _ => {}
        }
    }
    Some(out)
}

/// An unthemed story circle layer's labels (ruling R8-a): its `text-field`
/// (a string, or `["get", <property>]` read per feature) drawn through the
/// story-label path in the map label type (§4.1: 26 / 700, ink, a 6-unit
/// white halo), `text-size`, `text-color`, `text-halo-color` and
/// `text-halo-width` overriding; placed 28 from the point on its
/// `label_side` (right by default), by the same edge, rail, line and card
/// rules as a themed label. Empty without `text-field`.
pub(crate) fn circle_labels(layer_id: &str, geojson: &str, paint: &serde_json::Map<String, serde_json::Value>) -> Vec<StoryLabel> {
    use crate::story::theme_layers::{LabelSide, LABEL_HALO, LABEL_OFFSET};
    let Some(field) = paint.get("text-field") else { return Vec::new() };
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(geojson) else { return Vec::new() };
    let num = |k: &str, d: f32| paint.get(k).and_then(serde_json::Value::as_f64).map_or(d, |v| v as f32);
    let colour = |k: &str, d: [u8; 4]| paint.get(k).and_then(colour_bytes).unwrap_or(d);
    let [r, g, b] = crate::story::design::INK;
    let token = crate::story::design::MAP_LABEL;
    let mut out = Vec::new();
    for f in doc.get("features").and_then(serde_json::Value::as_array).into_iter().flatten() {
        let props = f.get("properties").and_then(serde_json::Value::as_object);
        let Some(text) = crate::story::overlay_layers::text_field_for(field, props) else { continue };
        let Some(at) = f.get("geometry").and_then(|g| g.get("coordinates")).and_then(|c| {
            let c = if c.get(0).is_some_and(serde_json::Value::is_array) { c.get(0)? } else { c };
            Some((c.get(0)?.as_f64()?, c.get(1)?.as_f64()?))
        }) else {
            continue;
        };
        let side = match props.and_then(|p| p.get("label_side")).and_then(serde_json::Value::as_str) {
            Some("left") => LabelSide::Left,
            _ => LabelSide::Right,
        };
        let id = f.get("id").and_then(serde_json::Value::as_u64);
        out.push(StoryLabel {
            at,
            // Spec §9 (R14 review): a no-op without an RTL letter.
            text: crate::story::cards::isolate_ltr_runs(&text),
            size: num("text-size", token.size),
            weight: token.weight,
            color: colour("text-color", [r, g, b, 0xFF]),
            halo: colour("text-halo-color", [0xFF; 4]),
            halo_width: num("text-halo-width", LABEL_HALO),
            beside: Some((LABEL_OFFSET, side)),
            feature: format!("{layer_id}/{}", id.unwrap_or_default()),
            id,
            // Ruling R13-a: static, not timeline-driven, so it ranks
            // below a real `now` label (`StoryLabel::state`'s doc).
            state: crate::story::theme_layers::PointState::Next,
        });
    }
    out
}

/// Every marker `layers` draw this frame, in layer then marker order, as
/// physical-pixel instances (`project` takes a world point to LOGICAL
/// pixels, as for [`arrowhead_quads`]): each at its layer's opacity, its
/// lengths and translate times `pixel_ratio`. A marker that does not
/// project (behind the globe) or lies wholly off a `w`x`h` frame is left
/// out.
pub(crate) fn marker_instances(
    layers: &[StoryLayerDraw],
    project: impl Fn((f64, f64)) -> Option<(f32, f32)>,
    pixel_ratio: f32,
    (w, h): (f32, f32),
) -> Vec<crate::render::marker_pipeline::MarkerInstance> {
    let world = |(lon, lat): (f64, f64)| crate::overlay::geom::lon_lat_to_world(lon, lat);
    let mut out = Vec::new();
    for layer in layers.iter().filter(|l| l.line_fx[2] > 0.0) {
        let opacity = layer.line_fx[2].min(1.0);
        for m in layer.markers.iter().flatten() {
            let Some((x, y)) = project(world(m.at)) else { continue };
            let c = [(x + m.translate.0) * pixel_ratio, (y + m.translate.1) * pixel_ratio];
            let (r, sw) = (m.radius * pixel_ratio, m.stroke_width * pixel_ratio);
            let reach = r + sw + 1.0;
            if c[0] + reach < 0.0 || c[1] + reach < 0.0 || c[0] - reach > w || c[1] - reach > h {
                continue;
            }
            let rgba = |[r, g, b, a]: [u8; 4]| [f32::from(r) / 255.0, f32::from(g) / 255.0, f32::from(b) / 255.0, f32::from(a) / 255.0 * opacity];
            if m.fill[3] == 0 && (m.stroke[3] == 0 || sw <= 0.0) {
                continue;
            }
            out.push(crate::render::marker_pipeline::MarkerInstance { center: c, radius_px: r, stroke_px: sw, blur: m.blur, fill: rgba(m.fill), stroke: rgba(m.stroke) });
        }
    }
    out
}

/// One story label placed on the frame (physical pixels).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PlacedStoryLabel {
    pub text: String,
    /// Physical px.
    pub size: f32,
    pub weight: u16,
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
    pub color: [u8; 4],
    pub halo: [u8; 4],
    /// Physical px.
    pub halo_width: f32,
}

/// The alpha each of a halo's stacked copies draws at so that `n` of
/// them, overlapping, composite to `alpha` (`1 - (1 - a)^(1/n)`): a fading
/// label's halo then fades with its text instead of reaching near-white
/// while the text is still faint. `alpha` 1 stays 1, so a label at full
/// strength draws exactly as before.
#[must_use]
pub(crate) fn halo_copy_alpha(alpha: f32, n: usize) -> f32 {
    let a = alpha.clamp(0.0, 1.0);
    if a >= 1.0 || n <= 1 {
        return a;
    }
    1.0 - (1.0 - a).powf(1.0 / n as f32)
}

/// The stack depth [`halo_copy_alpha`] solves for, from [`halo_disc`]'s
/// `copies`: half of them, calibrated by measurement
/// (`a_fading_labels_halo_fades_with_it`: the ring's most covered pixel
/// composites to the label's alpha, within the 8-bit rounding of each
/// copy's colour). Thinner parts of the ring sit a little below it.
#[must_use]
pub(crate) fn halo_stack_depth(copies: usize) -> usize {
    copies.div_ceil(2).max(1)
}

/// The offsets, physical px, of the copies that draw a story label's
/// `width`-px halo as a filled disc: each whole radius from 1 to `width`,
/// with a copy every ~2 px round it (at least 8), at whole pixels, each
/// once. Empty for no halo. Computed once per width (rounded) and kept.
pub(crate) fn halo_disc(width: f32) -> std::sync::Arc<[(f32, f32)]> {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock, PoisonError};
    static CACHE: OnceLock<Mutex<HashMap<u32, Arc<[(f32, f32)]>>>> = OnceLock::new();
    let rings = if width.is_finite() && width > 0.0 { width.round().clamp(1.0, 64.0) as u32 } else { 0 };
    let mut cache = CACHE.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap_or_else(PoisonError::into_inner);
    Arc::clone(cache.entry(rings).or_insert_with(|| {
        let mut out = Vec::new();
        for r in 1..=rings {
            let r = r as f32;
            let n = ((std::f32::consts::TAU * r / 2.0).ceil() as u32).max(8);
            for k in 0..n {
                let a = std::f32::consts::TAU * k as f32 / n as f32;
                // Whole pixels: a fractional copy smears (`symbol_pass::halo_offsets`).
                let o = ((r * a.cos()).round(), (r * a.sin()).round());
                if !out.contains(&o) {
                    out.push(o);
                }
            }
        }
        out.into()
    }))
}

/// Whether the segment `a`-`b` meets the rect `(x0, y0, x1, y1)`
/// (Liang-Barsky clipping).
pub(crate) fn segment_meets_rect(a: (f32, f32), b: (f32, f32), (x0, y0, x1, y1): crate::story::tiers::Rect) -> bool {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let (mut t0, mut t1) = (0.0_f32, 1.0_f32);
    for (p, q) in [(-dx, a.0 - x0), (dx, x1 - a.0), (-dy, a.1 - y0), (dy, y1 - a.1)] {
        if p == 0.0 {
            if q < 0.0 {
                return false;
            }
        } else {
            let t = q / p;
            if p < 0.0 {
                t0 = t0.max(t);
            } else {
                t1 = t1.min(t);
            }
            if t0 > t1 {
                return false;
            }
        }
    }
    true
}

/// Every visible story line of `layers` on screen (logical px, through
/// `project`), for [`place_story_labels`]: each LineString (a
/// MultiLineString's every part) of each line layer drawn this frame,
/// the first [`STORY_LINE_VERTICES_FOR_LABELS`] vertices in all.
pub(crate) fn story_lines_on_screen(layers: &[StoryLayerDraw], project: impl Fn((f64, f64)) -> Option<(f32, f32)>) -> Vec<Vec<(f32, f32)>> {
    let world = |(lon, lat): (f64, f64)| crate::overlay::geom::lon_lat_to_world(lon, lat);
    let mut out = Vec::new();
    let mut budget = STORY_LINE_VERTICES_FOR_LABELS;
    // A themed line's sublayers share one document: read it once.
    let mut seen = std::collections::HashSet::new();
    for l in layers.iter().filter(|l| l.line_fx[2] > 0.0 && l.markers.is_none() && l.geojson.contains("LineString")) {
        if !seen.insert(&*l.geojson) {
            continue;
        }
        let Ok(doc) = serde_json::from_str::<serde_json::Value>(&l.geojson) else { continue };
        for f in doc.get("features").and_then(serde_json::Value::as_array).into_iter().flatten() {
            let Some(g) = f.get("geometry") else { continue };
            let parts: Vec<&serde_json::Value> = match g.get("type").and_then(serde_json::Value::as_str) {
                Some("LineString") => g.get("coordinates").into_iter().collect(),
                Some("MultiLineString") => g.get("coordinates").and_then(serde_json::Value::as_array).into_iter().flatten().collect(),
                _ => continue,
            };
            for part in parts {
                let pts: Vec<(f32, f32)> = part
                    .as_array()
                    .into_iter()
                    .flatten()
                    .take(budget)
                    .filter_map(|c| project(world((c.get(0)?.as_f64()?, c.get(1)?.as_f64()?))))
                    .collect();
                budget = budget.saturating_sub(pts.len());
                out.push(pts);
                if budget == 0 {
                    return out;
                }
            }
        }
    }
    out
}

/// The most story-line vertices [`story_lines_on_screen`] projects per
/// frame for label sides: a story's own lines are short; past this a
/// label keeps its preferred side.
pub(crate) const STORY_LINE_VERTICES_FOR_LABELS: usize = 20_000;

/// The most story labels (names and numerals) one frame draws (R8
/// review): each carries a halo of tens of glyph copies. Past it the
/// later labels are left out with the warning `story_labels_capped`.
pub(crate) const MAX_STORY_LABELS: usize = 64;

/// Places `layers`' labels this frame (spec §6.3, §7): each numeral
/// centred on its marker; each name `offset` from it on its preferred
/// side -- the other side when the preferred box crosses one of the
/// frame's story `lines` (logical px, [`story_lines_on_screen`]) and the
/// other does not -- then flipped when it would end within 24 units of the
/// frame's edge or meet a safe-area `rails` rect (1080 units,
/// [`crate::story::theme_layers::place_label`]). A name that meets one of
/// `blockers` (physical px: the card, `true`, or a colophon-row run,
/// `false`, `StoryChromeDraw::label_blockers`) takes the other side if
/// that one is clear, else is hidden with the warning `label_under_card`
/// (spec §11's template, R10 review round 1: one wording for either
/// cause, the card or the colophon row). `measure(text, physical size,
/// weight)` is the shaped
/// `(width, height)`. A label at opacity 0, or whose marker does not
/// project, is left out. Past [`MAX_STORY_LABELS`] *successfully placed*
/// labels the rest are dropped by state priority (Task R13, spec §7:
/// `now > next > done > muted`, [`crate::story::theme_layers::PointState::drop_rank`]),
/// then by distance from the frame's centre -- the cap keeps the labels
/// nearest the action, not whichever a layer happened to list first.
/// Both keys are deterministic for one frame's data, so scrubbing to the
/// same `t` always drops the same labels (seek-stable).
///
/// Review round 1: a label that ranks inside the cap but is then hidden
/// behind the card (warned, `label_under_card`) or clipped off the frame
/// (unwarned) does not spend its slot -- the next-ranked candidate is
/// tried in its place, so the cap always ends with the 64 best-ranked
/// labels that actually draw (or fewer, if there are not 64 placeable
/// candidates at all). `story_labels_capped`'s count is exactly the
/// candidates never even attempted because 64 were already placed.
#[allow(clippy::too_many_arguments)]
pub(crate) fn place_story_labels(
    layers: &[StoryLayerDraw],
    project: impl Fn((f64, f64)) -> Option<(f32, f32)>,
    mut measure: impl FnMut(&str, f32, u16) -> (f32, f32),
    pixel_ratio: f32,
    (w, h): (f32, f32),
    rails: &[crate::story::tiers::Rect],
    blockers: &[((f32, f32, f32, f32), bool)],
    lines: &[Vec<(f32, f32)>],
) -> (Vec<PlacedStoryLabel>, Vec<crate::story::layout::LayoutWarning>) {
    use crate::story::theme_layers::{label_box, place_label};
    let world = |(lon, lat): (f64, f64)| crate::overlay::geom::lon_lat_to_world(lon, lat);
    let r = if pixel_ratio.is_finite() && pixel_ratio > 0.0 { pixel_ratio } else { 1.0 };
    let mut warnings = Vec::new();

    // Every visible, on-screen label, ranked for the cap: state priority
    // first, then distance from the frame's centre (logical px -- a
    // uniform scale of both axes by `r`, so ranking in logical units
    // orders exactly as physical would). `(layer_idx, label_idx)` breaks
    // an exact tie so the order never depends on a hash or a sort's
    // internal tie behaviour.
    let (cx, cy) = (w / (2.0 * r), h / (2.0 * r));
    let mut candidates: Vec<(usize, usize, u8, f32)> = Vec::new();
    for (li, layer) in layers.iter().enumerate().filter(|(_, l)| l.line_fx[2] > 0.0) {
        for (lj, label) in layer.labels.iter().enumerate().filter(|(_, l)| l.color[3] > 0) {
            if let Some((x, y)) = project(world(label.at)) {
                let dist2 = (x - cx).powi(2) + (y - cy).powi(2);
                candidates.push((li, lj, label.state.drop_rank(), dist2));
            }
        }
    }
    let mut ranked: Vec<usize> = (0..candidates.len()).collect();
    ranked.sort_by(|&a, &b| {
        let (la, lja, pa, da) = candidates[a];
        let (lb, ljb, pb, db) = candidates[b];
        pa.cmp(&pb).then(da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)).then((la, lja).cmp(&(lb, ljb)))
    });

    // Attempts `(li, lj)`'s real placement: side-flip, the card/rail
    // blockers (warning and `None` when both sides are blocked), the
    // frame-edge clip (`None`, unwarned). Exactly `place_visible_symbols`'
    // candidate/placement split, one label at a time.
    let mut try_place = |li: usize, lj: usize| -> Option<PlacedStoryLabel> {
        let layer = &layers[li];
        let label = &layer.labels[lj];
        let opacity = layer.line_fx[2].min(1.0);
        let fade = |[cr, cg, cb, ca]: [u8; 4]| [cr, cg, cb, (f32::from(ca) * opacity).round() as u8];
        let (x, y) = project(world(label.at))?;
        let size = label.size * r;
        let (tw, th) = measure(&label.text, size, label.weight);
        let (left, top) = match label.beside {
            None => (x * r - tw / 2.0, y * r - th / 2.0),
            Some((offset, side)) => {
                let units = (tw / r, th / r);
                let crosses = |side| {
                    let b = label_box((x, y), units, offset, side);
                    lines.iter().any(|l| l.windows(2).any(|s| matches!(s, &[a, b2] if segment_meets_rect(a, b2, b))))
                };
                let side = if crosses(side) && !crosses(side.other()) { side.other() } else { side };
                let phys = |q: crate::story::tiers::Rect| (q.0 * r, q.1 * r, q.2 * r, q.3 * r);
                let blocked = |q: crate::story::tiers::Rect| {
                    let p = phys(q);
                    blockers.iter().find(|(o, _)| p.0 < o.2 && o.0 < p.2 && p.1 < o.3 && o.1 < p.3).map(|(_, card)| *card)
                };
                let (side, rect) = place_label((x, y), units, offset, side, (w / r, h / r), rails);
                let rect = match blocked(rect) {
                    None => rect,
                    Some(card) => {
                        let other = label_box((x, y), units, offset, side.other());
                        // The other side only when it clears the frame's edge
                        // margin and the rails, as `place_label` asks of the
                        // first side: a label pushed off the card must not be
                        // cut by the edge or sit under a platform's buttons.
                        let inside = crate::story::theme_layers::label_clear(other, (w / r, h / r), rails);
                        if blocked(other).is_none() && inside {
                            other
                        } else {
                            // Spec §11's template names one cause, not
                            // `card`/`inside`'s four; the card (or, with
                            // no card, the colophon row) hides it either
                            // way, so one message covers both.
                            let _ = card;
                            warnings.push(crate::story::layout::LayoutWarning {
                                id: label.feature.clone(),
                                code: "label_under_card",
                                message: format!(
                                    "The map label \"{}\" is hidden behind the card. Move the camera or the card, or set label to \"none\" on {}.",
                                    label.text, layer.layer_id
                                ),
                            });
                            return None;
                        }
                    }
                };
                let p = phys(rect);
                (p.0, p.1)
            }
        };
        if left + tw < 0.0 || top + th < 0.0 || left > w || top > h {
            return None;
        }
        Some(PlacedStoryLabel {
            text: label.text.clone(),
            size,
            weight: label.weight,
            left,
            top,
            width: tw,
            height: th,
            color: fade(label.color),
            halo: fade(label.halo),
            halo_width: label.halo_width * r,
        })
    };

    // Walk the ranking, backfilling: a hidden or clipped label (`None`)
    // never counts toward the 64, so the next-ranked candidate gets its
    // slot. `attempted` stops counting the moment 64 are actually
    // placed, so `candidates.len() - attempted` is exactly the labels
    // never even tried.
    let mut by_key: std::collections::HashMap<(usize, usize), PlacedStoryLabel> = std::collections::HashMap::new();
    let mut attempted = 0usize;
    for &idx in &ranked {
        if by_key.len() >= MAX_STORY_LABELS {
            break;
        }
        attempted += 1;
        let (li, lj, ..) = candidates[idx];
        if let Some(p) = try_place(li, lj) {
            by_key.insert((li, lj), p);
        }
    }
    let capped = candidates.len() - attempted;

    // The draw order stays today's: per layer, per label, as declared --
    // only the ranking decided which ones are in `by_key` at all.
    let mut placed = Vec::with_capacity(by_key.len());
    for (li, layer) in layers.iter().enumerate().filter(|(_, l)| l.line_fx[2] > 0.0) {
        for (lj, _) in layer.labels.iter().enumerate().filter(|(_, l)| l.color[3] > 0) {
            if let Some(p) = by_key.remove(&(li, lj)) {
                placed.push(p);
            }
        }
    }
    if capped > 0 {
        warnings.push(crate::story::layout::LayoutWarning {
            id: "story-labels".into(),
            code: "story_labels_capped",
            message: format!("{capped} story labels were left out of this frame: at most {MAX_STORY_LABELS} draw at once. Show fewer with the layer's `label`."),
        });
    }
    (placed, warnings)
}

/// One frame's story marks, placed (spec §6.3-§6.4, §7): what the
/// capture's frame and the live map's paint both draw from
/// [`frame_marks`] -- the markers (the symbol pass, under every label),
/// the point labels and numerals (over the map's labels) and the
/// arrowheads (above the labels, with the marks), all physical pixels.
#[derive(Clone, Debug, Default)]
pub(crate) struct FrameMarks {
    pub markers: Vec<crate::render::marker_pipeline::MarkerInstance>,
    pub labels: Vec<PlacedStoryLabel>,
    pub arrowheads: Vec<(StubKey, crate::render::IconInstance)>,
    /// `label_under_card` and `story_labels_capped`.
    pub warnings: Vec<crate::story::layout::LayoutWarning>,
}

impl FrameMarks {
    /// Whether the frame draws no mark at all.
    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.markers.is_empty() && self.labels.is_empty() && self.arrowheads.is_empty()
    }

    /// The markers and the labels as collision obstacles for the
    /// basemap's labels (spec §7: placed before them, so a story mark
    /// wins), physical pixels.
    pub(crate) fn obstacles(&self) -> impl Iterator<Item = crate::collision::CollisionBox> + '_ {
        let markers = self.markers.iter().map(|m| {
            let r = m.radius_px + m.stroke_px;
            crate::collision::CollisionBox { left: m.center[0] - r, top: m.center[1] - r, right: m.center[0] + r, bottom: m.center[1] + r }
        });
        let labels = self.labels.iter().map(|l| crate::collision::CollisionBox { left: l.left, top: l.top, right: l.left + l.width, bottom: l.top + l.height });
        markers.chain(labels)
    }
}

/// The rectangles a story label may not overlap (spec §7,
/// [`place_story_labels`]' `blockers`, physical px): the card and the
/// colophon row's runs (`StoryChromeDraw::label_blockers`), and -- when
/// there are any, a design frame -- the mark that heads the row.
pub(crate) fn label_blockers(
    story: Option<&super::story_chrome::StoryChromeDraw>,
    mark: Option<&crate::render::IconInstance>,
) -> Vec<((f32, f32, f32, f32), bool)> {
    let mut blockers: Vec<((f32, f32, f32, f32), bool)> = story.into_iter().flat_map(|s| s.label_blockers.iter().copied()).collect();
    if !blockers.is_empty() {
        if let Some(m) = mark {
            blockers.push(((m.screen_x, m.screen_y, m.screen_x + m.width, m.screen_y + m.height), false));
        }
    }
    blockers
}

/// Every story mark `layers` draw on one `size` (physical px) frame:
/// [`marker_instances`], [`place_story_labels`] (against the frame's
/// story lines, its safe-area `rails` and the chrome's `blockers`,
/// `measure` shaping in the design face, [`measure_story_label`]) and
/// [`arrowhead_quads`]. `project` takes a
/// world point to the frame's 1080-unit pixels (logical pixels at
/// `pixel_ratio`). The capture's frame and the live map's paint both call
/// it (spec §13): one producer, so the two draw the same marks.
#[allow(clippy::too_many_arguments)]
pub(crate) fn frame_marks(
    layers: &[StoryLayerDraw],
    project: impl Fn((f64, f64)) -> Option<(f32, f32)>,
    measure: impl FnMut(&str, f32, u16) -> (f32, f32),
    pixel_ratio: f32,
    size: (f32, f32),
    rails: &[crate::story::tiers::Rect],
    blockers: &[((f32, f32, f32, f32), bool)],
) -> FrameMarks {
    let markers = marker_instances(layers, &project, pixel_ratio, size);
    let has_labels = layers.iter().any(|l| l.line_fx[2] > 0.0 && !l.labels.is_empty());
    let (labels, warnings) = if has_labels {
        let lines = story_lines_on_screen(layers, &project);
        place_story_labels(
            layers,
            &project,
            measure,
            pixel_ratio,
            size,
            rails,
            blockers,
            &lines,
        )
    } else {
        (Vec::new(), Vec::new())
    };
    let arrowheads = if any_arrowheads(layers) { arrowhead_quads(layers, &project, pixel_ratio) } else { Vec::new() };
    FrameMarks { markers, labels, arrowheads, warnings }
}

/// A story label's shaped `(width, height)`, physical px: `text` at
/// `size` (physical px) and `weight` in the design face (spec §4.1's map
/// label), through `shape_cache` -- [`frame_marks`]' `measure`.
pub(crate) fn measure_story_label(
    font_system: &mut glyphon::FontSystem,
    shape_cache: &mut crate::symbol_pass::LabelShapeCache,
    text: &str,
    size: f32,
    weight: u16,
) -> (f32, f32) {
    let face = super::story_chrome::design_face_for(text, weight);
    let shaped = shape_cache.get_or_shape(font_system, text, size, None, crate::overlay::paint::TextJustify::Center, &face);
    (shaped.measured_width, shaped.measured_height)
}

/// A capture frame's [`FrameMarks`]: `request`'s story layers on its
/// `camera` and globe `frame`, the label blockers from its `chrome` layout
/// ([`label_blockers`]), its story's safe-area rails, the labels measured
/// in `fonts` (built only when a label is drawn). Nothing without a mark.
pub(crate) fn capture_frame_marks(
    request: &CaptureRequest,
    camera: &crate::render::Camera,
    frame: &crate::render::globe::GlobeFrame,
    chrome: Option<&super::chrome::ChromeLayout>,
    fonts: &mut Option<super::session::FontState>,
) -> FrameMarks {
    let layers = &request.story_layers;
    if !layers.iter().any(|l| l.markers.is_some() || !l.labels.is_empty() || !l.arrowheads.is_empty()) {
        return FrameMarks::default();
    }
    let anchors = crate::render::globe::GlobeAnchors::new(frame);
    let projector = crate::render::globe::WorldProjector::new(camera, &anchors);
    let blockers = label_blockers(chrome.and_then(|c| c.story.as_ref()), chrome.and_then(|c| c.mark.as_ref()));
    let rails = request.story.as_ref().map(|s| s.safe_area.clone()).unwrap_or_default();
    frame_marks(
        layers,
        |w| projector.project(w),
        |text, size, weight| {
            let f = super::session::fonts_for(fonts, &request.label_fonts);
            measure_story_label(&mut f.system, &mut f.shapes, text, size, weight)
        },
        request.pixel_ratio,
        (request.width as f32, request.height as f32),
        &rails,
        &blockers,
    )
}

/// Runs `f` with the globe anchors a view's `camera` paints with
/// (`globe_frame(camera).with_globe_enabled(globe)`, as `MapView::paint`
/// builds them once per frame): for `map_view`'s live-mark tests, which
/// may not build a `GlobeFrame` in `map_view.rs` itself
/// (`paint_calls_globe_frame_exactly_once_and_before_its_tile_loops`).
#[cfg(test)]
pub(crate) fn with_view_anchors<R>(camera: &crate::render::Camera, globe: bool, f: impl FnOnce(&crate::render::globe::GlobeAnchors) -> R) -> R {
    let frame = crate::render::globe::globe_frame(camera).with_globe_enabled(globe);
    f(&crate::render::globe::GlobeAnchors::new(&frame))
}

/// `labels`' text runs, in the design face (spec §4.1's map label): for
/// each, its halo as a filled disc of copies (the 6-unit halo is wider
/// than `symbol_pass::halo_offsets`' one ring of four reads cleanly at:
/// every whole radius out to the width, a copy every ~2 px round each),
/// each copy at the alpha whose stack composites to the label's
/// ([`halo_copy_alpha`]: at alpha 1 every copy is opaque), then the text.
/// Shaped into `shape_cache`; what the capture's label pass and the live
/// map's story-label pass both draw.
pub(crate) fn story_label_runs(
    labels: &[PlacedStoryLabel],
    font_system: &mut glyphon::FontSystem,
    shape_cache: &mut crate::symbol_pass::LabelShapeCache,
) -> Vec<(crate::symbol_pass::LabelDraw, Option<crate::collision::CollisionBox>)> {
    let rgba = |[r, g, b, a]: [u8; 4]| crate::style::Rgba { r: f32::from(r) / 255.0, g: f32::from(g) / 255.0, b: f32::from(b) / 255.0, a: f32::from(a) / 255.0 };
    let mut out = Vec::new();
    for l in labels {
        let face = super::story_chrome::design_face_for(&l.text, l.weight);
        let justify = crate::overlay::paint::TextJustify::Center;
        let _ = shape_cache.get_or_shape(font_system, &l.text, l.size, None, justify, &face);
        let key = crate::symbol_pass::LabelShapeCache::key(&l.text, l.size, None, justify, &face);
        let disc = halo_disc(l.halo_width);
        let mut halo = rgba(l.halo);
        halo.a = halo_copy_alpha(halo.a, halo_stack_depth(disc.len()));
        for &(dx, dy) in disc.iter() {
            out.push(((key.clone(), l.left + dx, l.top + dy, halo, 0.0, halo, 0.0), None));
        }
        out.push(((key, l.left, l.top, rgba(l.color), 0.0, rgba(l.halo), 0.0), None));
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn paint_splits_expressions_from_constants_and_spells_named_colours() {
        let paint: serde_json::Map<String, serde_json::Value> = serde_json::from_value(serde_json::json!({
            "line-color": ["match", ["get", "k"], "a", "red", "blue"],
            "line-width": 4,
            "line-opacity": 0.5,
            "circle-color": "orange",
            "fill-opacity": 0.3
        }))
        .unwrap();
        let (declared, constants) = split_paint(&paint);
        let sorted = |m: &serde_json::Map<String, serde_json::Value>| {
            let mut k: Vec<String> = m.keys().cloned().collect();
            k.sort();
            k
        };
        assert_eq!(sorted(&declared), ["line-color", "line-opacity"]);
        assert!(!declared["line-color"].to_string().contains("red"), "named colours spelled out: {}", declared["line-color"]);
        assert_eq!(sorted(&constants), ["circle-color", "fill-opacity", "line-width"]);
        assert_eq!(constants["circle-color"], "#ffa500ff", "a constant colour is hex, as a feature property must be");
    }

    /// Review #6: a story layer is never tiled, whatever its size or
    /// GPUI_MAP_OVERLAY_TILING (the flag, not the variable, is tested: the
    /// variable is process-wide).
    #[test]
    fn a_story_layer_source_is_always_whole() {
        let n = crate::overlay::tile_plan::TILED_SOURCE_MIN_POINTS + 10;
        let coords: Vec<[f64; 2]> = (0..n).map(|i| [i as f64 * 1e-4 - 10.0, (i % 7) as f64 * 1e-3]).collect();
        let doc = serde_json::json!({"type": "FeatureCollection", "features": [{"type": "Feature", "id": 0, "properties": {},
            "geometry": {"type": "LineString", "coordinates": coords}}]});
        let store = layer_store("big", &doc.to_string(), &serde_json::Map::new(), false, 3).unwrap();
        let snapshot = store.source_snapshot(crate::overlay::DEFAULT_SOURCE).unwrap();
        assert!(
            crate::overlay::tile_plan::tiling_points(&snapshot.features) >= crate::overlay::tile_plan::TILED_SOURCE_MIN_POINTS,
            "the size alone would tile it"
        );
        assert!(!snapshot.tiled, "a story layer's source is whole");
    }

    /// A flat sentinel-cleared `w`x`h` frame centred on (0, 0) at zoom 0
    /// (the world 512 px wide), no tiles: one story layer over `geojson`
    /// with `paint`, line metrics on.
    fn one_layer_frame(w: u32, h: u32, geojson: &serde_json::Value, paint: serde_json::Value, arrowheads: Vec<StoryArrowhead>) -> Capture {
        let paint: serde_json::Map<String, serde_json::Value> = serde_json::from_value(paint).unwrap();
        let geojson = geojson.to_string();
        let markers = circle_markers(&geojson, &paint);
        let labels = circle_labels("l", &geojson, &paint);
        let layer = StoryLayerDraw {
            key: StoryLayerDraw::content_key("l", &geojson, &paint, true, false),
            layer_id: "l".into(),
            geojson: geojson.into(),
            paint: std::sync::Arc::new(paint),
            line_metrics: true,
            per_feature: false,
            above_labels: false,
            above_buildings: false,
            line_fx: crate::render::gpu::LINE_FX_NONE,
            features: Vec::new(),
            marks: None,
            arrowheads, markers, labels,
        };
        let request = CaptureRequest {
            center_lon: 0.0,
            center_lat: 0.0,
            zoom: 0.0,
            width: w,
            height: h,
            pixel_ratio: 1.0,
            flat: true,
            globe: false,
            clear: Clear::SENTINEL,
            backdrop: false,
            story_layers: vec![layer],
            ..CaptureRequest::default()
        };
        let style = Style { layers: Vec::new(), ..Style::default() };
        CaptureSession::new_blocking(None).unwrap().render(&request, &TileStore::new(), &style).unwrap()
    }

    fn is_red(p: [u8; 4]) -> bool {
        p[0] > 200 && p[1] < 80 && p[2] < 80
    }

    fn one_dot(paint: serde_json::Value) -> Capture {
        let doc = serde_json::json!({"type": "FeatureCollection", "features": [{"type": "Feature", "id": 0, "properties": {},
            "geometry": {"type": "Point", "coordinates": [0.0, 0.0]}}]});
        one_layer_frame(96, 96, &doc, paint, Vec::new())
    }

    /// Spec §6.4: `circle-stroke-width` and `-color` draw a ring OUTSIDE
    /// the fill (MapLibre's circle): a 10 px red fill, a 6 px blue stroke
    /// from 10 to 16 px, the sentinel past it.
    #[test]
    fn a_story_circle_draws_its_stroke_outside_its_fill() {
        let shot = one_dot(serde_json::json!({"circle-radius": 10, "circle-color": "#ff0000", "circle-stroke-width": 6, "circle-stroke-color": "#0000ff"}));
        let blue = |p: [u8; 4]| p[2] > 200 && p[0] < 60 && p[1] < 60;
        assert!(is_red(shot.pixel(48, 48)) && is_red(shot.pixel(48 + 7, 48)), "the fill to 10 px");
        assert!(blue(shot.pixel(48 + 13, 48)) && blue(shot.pixel(48, 48 - 13)), "the stroke from 10 to 16 px, all round");
        let plain = one_dot(serde_json::json!({"circle-radius": 10, "circle-color": "#ff0000"}));
        assert_eq!(shot.pixel(48 + 20, 48), plain.pixel(48 + 20, 48), "nothing past the stroke");
    }

    /// Spec §6.4: `circle-blur` softens the edge over that fraction of the
    /// radius (MapLibre: the disc fades from `(1 - blur) r` to `r`): at
    /// blur .5 the pixel at .75 r is part-covered where blur 0 is solid.
    #[test]
    fn a_story_circle_blurs_its_edge() {
        let sharp = one_dot(serde_json::json!({"circle-radius": 20, "circle-color": "#ff0000"}));
        let soft = one_dot(serde_json::json!({"circle-radius": 20, "circle-color": "#ff0000", "circle-blur": 0.5}));
        assert!(is_red(sharp.pixel(48 + 15, 48)), "solid at .75 r without blur");
        let p = soft.pixel(48 + 15, 48);
        assert!(p != sharp.pixel(48 + 15, 48) && p[0] > 60, "part-covered at .75 r with blur .5: {p:?}");
        assert!(is_red(soft.pixel(48 + 2, 48)), "solid inside (1 - blur) r");
    }

    /// Spec §6.4: `circle-translate` moves the disc on screen, (x, y) in
    /// px, y down: a 6 px dot moved (0, 20) sits 20 px below its point.
    #[test]
    fn a_story_circle_moves_by_its_translate() {
        let moved = one_dot(serde_json::json!({"circle-radius": 6, "circle-color": "#ff0000", "circle-translate": [0, 20]}));
        assert!(!is_red(moved.pixel(48, 48)), "not on its point");
        assert!(is_red(moved.pixel(48, 68)), "20 px below it");
    }

    /// R8 fix round 2: a label fading at alpha .3 has a halo near .3 too,
    /// not one that stacked to near-white. The halo is white over the
    /// magenta sentinel, so its coverage is a pixel's green over 255; the
    /// text is ink (green 0), so the brightest green in the label's box is
    /// the halo ring's.
    #[test]
    fn a_fading_labels_halo_fades_with_it() {
        use crate::story::theme_layers::LabelSide;
        assert_eq!(halo_copy_alpha(1.0, 40), 1.0, "full strength unchanged");
        let a = halo_copy_alpha(0.3, 20);
        assert!((1.0 - (1.0 - a).powi(20) - 0.3).abs() < 1e-4, "twenty copies stack to .3");
        let shot = |alpha: u8| {
            let label = StoryLabel {
                at: (0.0, 0.0), text: "Harbour".into(), size: 26.0, weight: 700, color: [0x0A, 0x0F, 0x1A, alpha], halo: [255, 255, 255, alpha],
                halo_width: 6.0, beside: Some((28.0, LabelSide::Right)), feature: "l/0".into(), id: Some(0),
                state: crate::story::theme_layers::PointState::Now,
            };
            let doc = serde_json::json!({"type": "FeatureCollection", "features": []});
            let mut paint = serde_json::Map::new();
            paint.insert("circle-radius".into(), serde_json::json!(0));
            let geojson = doc.to_string();
            let layer = StoryLayerDraw {
                key: 7, layer_id: "l".into(), geojson: geojson.into(), paint: std::sync::Arc::new(paint), line_metrics: false, per_feature: false,
                above_labels: false, above_buildings: false, line_fx: crate::render::gpu::LINE_FX_NONE, features: Vec::new(), marks: None,
                arrowheads: Vec::new(), markers: Some(Vec::new()), labels: vec![label],
            };
            let request = CaptureRequest {
                center_lon: 0.0, center_lat: 0.0, zoom: 0.0, width: 400, height: 96, pixel_ratio: 1.0, flat: true, globe: false,
                clear: Clear::SENTINEL, backdrop: false, story_layers: vec![layer], ..CaptureRequest::default()
            };
            let style = Style { layers: Vec::new(), ..Style::default() };
            let shot = CaptureSession::new_blocking(None).unwrap().render(&request, &TileStore::new(), &style).unwrap();
            (200..400).flat_map(|x| (20..76).map(move |y| (x, y))).map(|(x, y)| f32::from(shot.pixel(x, y)[1]) / 255.0).fold(0.0, f32::max)
        };
        let full = shot(255);
        let faded = shot((0.3_f32 * 255.0).round() as u8);
        eprintln!("halo coverage: full {full:.3}, at .3 {faded:.3}");
        assert!(full > 0.95, "the halo is opaque at full strength: {full}");
        assert!((0.2..=0.4).contains(&faded), "the halo's coverage at alpha .3 is about .3, not near 1: {faded}");
        let half = shot((0.6_f32 * 255.0).round() as u8);
        eprintln!("halo coverage at .6: {half:.3}");
        assert!((0.48..=0.72).contains(&half), "and about .6 at .6: {half}");
    }

    /// R8 review #6: a feature property never switches a 1a circle layer
    /// to the marker path; only the layer's paint does.
    #[test]
    fn only_layer_paint_makes_a_circle_layer_draw_as_markers() {
        let doc = serde_json::json!({"type": "FeatureCollection", "features": [{"type": "Feature", "id": 0,
            "properties": {"circle-stroke-width": 4, "circle-blur": 1}, "geometry": {"type": "Point", "coordinates": [0.0, 0.0]}}]}).to_string();
        let paint = |v: serde_json::Value| -> serde_json::Map<String, serde_json::Value> { serde_json::from_value(v).unwrap() };
        assert!(circle_markers(&doc, &paint(serde_json::json!({"circle-color": "red"}))).is_none(), "the 1a mesh draws it");
        let m = circle_markers(&doc, &paint(serde_json::json!({"circle-blur": 0.2}))).unwrap();
        assert_eq!((m[0].blur, m[0].stroke_width), (0.2, 0.0), "the layer's keys only, never a property's");
    }

    /// Ruling R8-a: an unthemed story circle layer's `text-field` -- a
    /// `["get", ..]` expression -- draws in the capture through the story
    /// label path, and the live map's source carries the same text per
    /// feature as its symbol (`story_layer_parts`, what
    /// `MapView::set_story_layer_source` pushes).
    #[test]
    fn an_unthemed_text_field_draws_in_capture_and_live_alike() {
        let doc = serde_json::json!({"type": "FeatureCollection", "features": [{"type": "Feature", "id": 0, "properties": {"name": "Port"},
            "geometry": {"type": "Point", "coordinates": [0.0, 0.0]}}]});
        let paint = serde_json::json!({"circle-radius": 6, "circle-color": "#ff0000", "text-field": ["get", "name"]});
        let labels = circle_labels("l", &doc.to_string(), &serde_json::from_value(paint.clone()).unwrap());
        assert_eq!(labels.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), ["Port"]);
        let with = one_layer_frame(320, 96, &doc, paint.clone(), Vec::new());
        let mut plain_paint = paint.clone();
        plain_paint.as_object_mut().unwrap().remove("text-field");
        let without = one_layer_frame(320, 96, &doc, plain_paint, Vec::new());
        assert!(with.labels_drawn > without.labels_drawn, "the label run is drawn: {} vs {}", with.labels_drawn, without.labels_drawn);
        let dark = |shot: &Capture| (160 + 34..160 + 90).filter(|&x| (40..56).any(|y| shot.pixel(x, y)[0] < 80)).count();
        assert!(dark(&with) > 0 && dark(&without) == 0, "ink to the right of the dot");
        // The live source: the text resolved into each feature, as a symbol.
        let text = doc.to_string();
        let (declared, json) = story_layer_parts("l", &text, &serde_json::from_value(paint).unwrap()).unwrap();
        assert!(!declared.contains_key("text-field"), "never declared: the source cannot take an expression");
        let mut store = crate::overlay::OverlayStore::new();
        store.replace(&json, 3).unwrap();
        let snapshot = store.source_snapshot(crate::overlay::DEFAULT_SOURCE).unwrap();
        assert_eq!(snapshot.features[0].paint.text_field.as_deref(), Some("Port"), "the live symbol's text");
    }

    /// R8 review #4: a label whose side meets the card takes the other
    /// side; one blocked both ways is hidden, the warning naming why.
    /// This is also Task R13's brief's `a_story_label_under_the_card_is_hidden_and_warns`
    /// -- covered here already, not duplicated (see the R13 report).
    #[test]
    fn a_label_under_the_card_tries_the_other_side_first() {
        use crate::story::theme_layers::LabelSide;
        let label = StoryLabel {
            at: (0.0, 0.0), text: "Port".into(), size: 26.0, weight: 700, color: [0, 0, 0, 255], halo: [255; 4], halo_width: 0.0,
            beside: Some((28.0, LabelSide::Right)), feature: "l/0".into(), id: Some(0),
            state: crate::story::theme_layers::PointState::Now,
        };
        let layer = StoryLayerDraw {
            key: 1, layer_id: "l".into(), geojson: "{}".into(), paint: std::sync::Arc::new(serde_json::Map::new()), line_metrics: false,
            per_feature: false, above_labels: false, above_buildings: false, line_fx: crate::render::gpu::LINE_FX_NONE, features: Vec::new(),
            marks: None, arrowheads: Vec::new(), markers: None, labels: vec![label],
        };
        let place = |blockers: &[((f32, f32, f32, f32), bool)]| {
            place_story_labels(std::slice::from_ref(&layer), |_| Some((500.0, 500.0)), |_, _, _| (100.0, 26.0), 1.0, (1080.0, 1920.0), &[], blockers, &[])
        };
        let (placed, warnings) = place(&[((520.0, 480.0, 700.0, 520.0), true)]);
        assert!(warnings.is_empty() && placed[0].left < 500.0, "the card on the right: the label goes left: {placed:?}");
        let (placed, warnings) = place(&[((300.0, 480.0, 700.0, 520.0), false)]);
        assert!(placed.is_empty());
        assert_eq!(warnings[0].code, "label_under_card");
        assert!(warnings[0].message.contains("hidden behind the card") && warnings[0].message.contains("on l."), "{}", warnings[0].message);
        let (_, warnings) = place(&[((300.0, 480.0, 700.0, 520.0), true)]);
        assert!(warnings[0].message.contains("hidden behind the card"), "{}", warnings[0].message);
        // R9 (seen on the area showcase): the card on the left near the
        // right edge -- the other side would run off the frame, so the
        // label is hidden rather than cut by the edge.
        let near_edge = |blockers: &[((f32, f32, f32, f32), bool)]| {
            place_story_labels(std::slice::from_ref(&layer), |_| Some((1000.0, 500.0)), |_, _, _| (100.0, 26.0), 1.0, (1080.0, 1920.0), &[], blockers, &[])
        };
        let (placed, warnings) = near_edge(&[((800.0, 480.0, 990.0, 520.0), true)]);
        assert!(placed.is_empty(), "never cut by the frame's edge: {placed:?}");
        assert_eq!(warnings[0].code, "label_under_card");
        assert!(warnings[0].message.contains("hidden behind the card"), "{}", warnings[0].message);
        // The spec's 24-unit margin, as `place_label` keeps for the first
        // side. At x 935 the right box (963..1063) is inside the frame but
        // within 24 of its edge, so the label goes left -- onto the card --
        // and the right side is no escape either: hidden. At x 920 the
        // right box (948..1048) keeps the margin and the label sits there.
        let at = |x: f32, blockers: &[((f32, f32, f32, f32), bool)], rails: &[crate::story::tiers::Rect]| {
            place_story_labels(std::slice::from_ref(&layer), move |_| Some((x, 500.0)), |_, _, _| (100.0, 26.0), 1.0, (1080.0, 1920.0), rails, blockers, &[])
        };
        let card_left = |x: f32| [((x - 200.0, 480.0, x - 10.0, 520.0), true)];
        let (placed, _) = at(935.0, &card_left(935.0), &[]);
        assert!(placed.is_empty(), "963..1063 ends within 24 of 1080: {placed:?}");
        let (placed, _) = at(920.0, &card_left(920.0), &[]);
        assert_eq!(placed.len(), 1, "948..1048 keeps the margin");
        // A platform's rail on the right, the card on the left: hidden.
        let (placed, _) = at(700.0, &card_left(700.0), &[(720.0, 400.0, 1080.0, 600.0)]);
        assert!(placed.is_empty(), "728..828 meets the rail: {placed:?}");
        // R8 review #3: at most MAX_STORY_LABELS a frame, the rest warned.
        let many = StoryLayerDraw { labels: vec![layer.labels[0].clone(); MAX_STORY_LABELS + 6], ..layer.clone() };
        let (placed, warnings) =
            place_story_labels(std::slice::from_ref(&many), |_| Some((500.0, 500.0)), |_, _, _| (100.0, 26.0), 1.0, (1080.0, 1920.0), &[], &[], &[]);
        assert_eq!(placed.len(), MAX_STORY_LABELS);
        assert_eq!((warnings.len(), warnings[0].code), (1, "story_labels_capped"));
        assert_eq!(halo_disc(6.0).as_ptr(), halo_disc(6.0).as_ptr(), "the halo offsets are computed once per width");
    }

    /// Task R13 (spec §7, point 3): "a story label wins any collision
    /// against a basemap label" -- story marks go into the collision
    /// index before the basemap's own symbols
    /// (`capture::session::render`, `map_view::MapView::paint`, both
    /// from this same [`FrameMarks::obstacles`]). A synthetic basemap
    /// candidate box at the exact rectangle `place_story_labels` gave a
    /// story label cannot place once that box is in the index -- "the
    /// same spot" means neither wins by the usual symbol priority, only
    /// by which was inserted first.
    #[test]
    fn a_story_label_beats_a_basemap_label_at_the_same_spot() {
        use crate::story::theme_layers::PointState;
        let label = StoryLabel {
            at: (0.0, 0.0),
            text: "Malacca, 1511".into(),
            size: 26.0,
            weight: 700,
            color: [0, 0, 0, 255],
            halo: [255; 4],
            halo_width: 6.0,
            beside: None,
            feature: "l/0".into(),
            id: Some(0),
            state: PointState::Now,
        };
        let layer = StoryLayerDraw {
            key: 1,
            layer_id: "l".into(),
            geojson: "{}".into(),
            paint: std::sync::Arc::new(serde_json::Map::new()),
            line_metrics: false,
            per_feature: false,
            above_labels: false,
            above_buildings: false,
            line_fx: crate::render::gpu::LINE_FX_NONE,
            features: Vec::new(),
            marks: None,
            arrowheads: Vec::new(),
            markers: None,
            labels: vec![label],
        };
        let (placed, warnings) =
            place_story_labels(std::slice::from_ref(&layer), |_| Some((500.0, 500.0)), |_, _, _| (120.0, 26.0), 1.0, (1080.0, 1920.0), &[], &[], &[]);
        assert!(warnings.is_empty() && placed.len() == 1, "{placed:?} {warnings:?}");
        let marks = FrameMarks { labels: placed.clone(), ..FrameMarks::default() };
        let mut index = crate::collision::CollisionIndex::new(crate::collision::CollisionBox { left: 0.0, top: 0.0, right: 1080.0, bottom: 1920.0 });
        // The story mark, inserted as the spec orders it: before any
        // basemap symbol reaches the index.
        for obstacle in marks.obstacles() {
            index.insert_obstacle(obstacle);
        }
        // A basemap label ("Malacca City") at the exact same box.
        let basemap_candidate = crate::collision::CollisionBox {
            left: placed[0].left,
            top: placed[0].top,
            right: placed[0].left + placed[0].width,
            bottom: placed[0].top + placed[0].height,
        };
        assert!(!index.place(basemap_candidate), "a basemap label at the story label's exact box must not place: the story label wins");
    }

    /// Task R13 (R8's parked item, `progress.md`): past the 64 cap, the
    /// labels dropped are chosen by state priority (`now > next > done >
    /// muted`) then by distance from the frame's centre -- never by
    /// where a layer happened to list them. 70 labels: 10 `now`, 10
    /// `next` and 10 `done` (30, well under the cap) at the FARTHEST
    /// positions, and 40 `muted` ones at the CLOSEST 40 positions, with
    /// the 40 muted labels listed FIRST in the array -- the arrangement
    /// the old iteration-order drop got backwards (it would have kept
    /// every muted label and dropped six of the named `now`/`next`/`done`
    /// ones). The correct drop keeps every `now`/`next`/`done` label and
    /// the 34 muted ones nearest the centre, dropping the 6 farthest
    /// muted labels -- and it does this regardless of array order,
    /// i.e. deterministically and seek-stably.
    #[test]
    fn the_label_cap_drops_by_state_priority_then_distance_not_order() {
        use crate::story::theme_layers::PointState;
        // Position index `i` -> a distinct `(lon, 0)` whose projection
        // `place_story_labels` is told to resolve to `(540 + i*5, 960)`:
        // strictly farther from the frame's centre `(540, 960)` as `i`
        // grows, so position order IS distance order.
        let worlds: Vec<(f64, f64)> = (0..70).map(|i| crate::overlay::geom::lon_lat_to_world(f64::from(i), 0.0)).collect();
        let positions = worlds.clone();
        let project = move |w: (f64, f64)| positions.iter().position(|&p| p == w).map(|i| (540.0 + i as f32 * 5.0, 960.0));
        let label_at = |i: u32, state: PointState, tag: &str| StoryLabel {
            at: (f64::from(i), 0.0),
            text: format!("{tag}{i}"),
            size: 26.0,
            weight: 700,
            color: [0, 0, 0, 255],
            halo: [255; 4],
            halo_width: 0.0,
            beside: None,
            feature: format!("l/{i}"),
            id: None,
            state,
        };
        // Array order: every muted label first (closest 40 positions,
        // i 0..40), THEN the named ones (farthest 30 positions, i 40..70).
        let mut labels: Vec<StoryLabel> = (0..40).map(|i| label_at(i, PointState::Muted, "M")).collect();
        labels.extend((40..50).map(|i| label_at(i, PointState::Now, "N")));
        labels.extend((50..60).map(|i| label_at(i, PointState::Next, "X")));
        labels.extend((60..70).map(|i| label_at(i, PointState::Done, "D")));
        let layer = StoryLayerDraw {
            key: 1,
            layer_id: "l".into(),
            geojson: "{}".into(),
            paint: std::sync::Arc::new(serde_json::Map::new()),
            line_metrics: false,
            per_feature: false,
            above_labels: false,
            above_buildings: false,
            line_fx: crate::render::gpu::LINE_FX_NONE,
            features: Vec::new(),
            marks: None,
            arrowheads: Vec::new(),
            markers: None,
            labels,
        };
        let (placed, warnings) =
            place_story_labels(std::slice::from_ref(&layer), project, |_, _, _| (100.0, 26.0), 1.0, (1080.0, 1920.0), &[], &[], &[]);
        assert_eq!(placed.len(), MAX_STORY_LABELS, "{placed:?}");
        assert_eq!((warnings.len(), warnings[0].code), (1, "story_labels_capped"));
        assert!(warnings[0].message.contains('6'), "{}", warnings[0].message);
        let kept: std::collections::HashSet<&str> = placed.iter().map(|p| p.text.as_str()).collect();
        for i in 40..70 {
            assert!(kept.contains(format!("{}{i}", if i < 50 { "N" } else if i < 60 { "X" } else { "D" }).as_str()), "every now/next/done label stays: {i}");
        }
        for i in 0..34 {
            assert!(kept.contains(format!("M{i}").as_str()), "the 34 closest muted labels stay: M{i}");
        }
        for i in 34..40 {
            assert!(!kept.contains(format!("M{i}").as_str()), "the 6 farthest muted labels drop: M{i}");
        }
    }

    /// Task R13 review round 1: a top-ranked candidate that is then
    /// hidden -- behind the card (warned, `label_under_card`) or clipped
    /// off the frame (unwarned) -- must not spend a cap slot. Two `now`
    /// labels rank first (so the old, pre-backfill cap would have
    /// reserved their slots and never even tried placing them) but
    /// neither one draws; 70 `muted` fillers, all placeable, rank after
    /// them. The fix backfills from the next-ranked candidate, so
    /// exactly 64 labels are placed (the 64 closest fillers) and the
    /// warning count -- one `label_under_card`, one `story_labels_capped`
    /// for the 6 farthest fillers that were never even attempted -- is
    /// exactly what a correctly-backfilled cap gives, not what a
    /// rank-then-place cap (61 placed, no mention of the 2 wasted slots)
    /// would.
    #[test]
    fn a_label_hidden_or_clipped_does_not_spend_the_caps_slot() {
        use crate::story::theme_layers::{LabelSide, PointState};
        let world = crate::overlay::geom::lon_lat_to_world;
        let filler_worlds: Vec<(f64, f64)> = (0..70).map(|i| world(100.0 + f64::from(i), 0.0)).collect();
        let (under_card_lon, off_frame_lon) = (900.0, 901.0);
        let (under_card_world, off_frame_world) = (world(under_card_lon, 0.0), world(off_frame_lon, 0.0));
        let positions = filler_worlds.clone();
        let project = move |w: (f64, f64)| {
            if w == under_card_world {
                return Some((500.0, 500.0));
            }
            if w == off_frame_world {
                return Some((5000.0, 500.0));
            }
            positions.iter().position(|&p| p == w).map(|i| (590.0 + i as f32 * 5.0, 960.0))
        };
        let under_card = StoryLabel {
            at: (under_card_lon, 0.0),
            text: "UnderCard".into(),
            size: 26.0,
            weight: 700,
            color: [0, 0, 0, 255],
            halo: [255; 4],
            halo_width: 0.0,
            beside: Some((28.0, LabelSide::Right)),
            feature: "l/under-card".into(),
            id: None,
            state: PointState::Now,
        };
        let off_frame = StoryLabel {
            at: (off_frame_lon, 0.0),
            text: "OffFrame".into(),
            size: 26.0,
            weight: 700,
            color: [0, 0, 0, 255],
            halo: [255; 4],
            halo_width: 0.0,
            beside: None,
            feature: "l/off-frame".into(),
            id: None,
            state: PointState::Now,
        };
        let filler = |i: u32| StoryLabel {
            at: (100.0 + f64::from(i), 0.0),
            text: format!("F{i}"),
            size: 26.0,
            weight: 700,
            color: [0, 0, 0, 255],
            halo: [255; 4],
            halo_width: 0.0,
            beside: None,
            feature: format!("l/f{i}"),
            id: None,
            state: PointState::Muted,
        };
        let mut labels = vec![under_card, off_frame];
        labels.extend((0..70).map(filler));
        let layer = StoryLayerDraw {
            key: 1,
            layer_id: "l".into(),
            geojson: "{}".into(),
            paint: std::sync::Arc::new(serde_json::Map::new()),
            line_metrics: false,
            per_feature: false,
            above_labels: false,
            above_buildings: false,
            line_fx: crate::render::gpu::LINE_FX_NONE,
            features: Vec::new(),
            marks: None,
            arrowheads: Vec::new(),
            markers: None,
            labels,
        };
        // Blocks both sides of `under_card`'s preferred box at (500, 500)
        // (the same geometry `a_label_under_the_card_tries_the_other_side_first`
        // uses for its "both blocked" case).
        let blockers: [((f32, f32, f32, f32), bool); 1] = [((300.0, 480.0, 700.0, 520.0), false)];
        let (placed, warnings) =
            place_story_labels(std::slice::from_ref(&layer), project, |_, _, _| (100.0, 26.0), 1.0, (1080.0, 1920.0), &[], &blockers, &[]);
        assert_eq!(placed.len(), MAX_STORY_LABELS, "the two wasted slots are backfilled: {}", placed.len());
        assert!(placed.iter().all(|p| p.text != "UnderCard" && p.text != "OffFrame"), "neither doomed label drew: {placed:?}");
        let codes: Vec<&str> = warnings.iter().map(|w| w.code).collect();
        assert_eq!(codes, ["label_under_card", "story_labels_capped"], "{warnings:?}");
        assert!(warnings[1].message.contains('6'), "6 fillers never attempted, not 8: {}", warnings[1].message);
        let kept: std::collections::HashSet<&str> = placed.iter().map(|p| p.text.as_str()).collect();
        for i in 0..64 {
            assert!(kept.contains(format!("F{i}").as_str()), "the 64 closest fillers stay: F{i}");
        }
        for i in 64..70 {
            assert!(!kept.contains(format!("F{i}").as_str()), "the 6 farthest fillers, never attempted, drop: F{i}");
        }
    }

    /// Ruling R13-a (review round 1): an unthemed circle label ranks
    /// `Next`, not `Now` -- a timeline-driven `now` label must always
    /// beat a static one for the cap, however far it sits from the
    /// frame's centre. `circle_labels` itself carries the ruling.
    #[test]
    fn an_unthemed_circle_label_ranks_as_next_not_now() {
        let doc = serde_json::json!({"type": "FeatureCollection", "features": [{"type": "Feature", "id": 0, "properties": {"name": "Jetty"},
            "geometry": {"type": "Point", "coordinates": [0.0, 0.0]}}]});
        let paint = serde_json::json!({"text-field": ["get", "name"]});
        let labels = circle_labels("l", &doc.to_string(), &serde_json::from_value(paint).unwrap());
        assert_eq!(labels[0].state, crate::story::theme_layers::PointState::Next, "{labels:?}");
    }

    /// Ruling R13-a, the mixed case it exists for: 64 unthemed (`Next`)
    /// labels at the 64 positions CLOSEST to the frame's centre, and one
    /// themed `now` label FARTHER than every one of them. By distance
    /// alone the `now` label would be the very last of 65 tied
    /// candidates and lose the cap; by state priority it is attempted
    /// first and always survives. The one dropped is the farthest
    /// unthemed label, not the `now` one.
    #[test]
    fn a_now_label_survives_the_cap_against_many_closer_unthemed_labels() {
        use crate::story::theme_layers::PointState;
        let world = crate::overlay::geom::lon_lat_to_world;
        // i 0..64: the unthemed labels, closest to farthest. i == 64:
        // the `now` label, farther than all 64 of them (x 860 vs <= 855).
        let positions: Vec<(f64, f64)> = (0..=64).map(|i| world(f64::from(i), 0.0)).collect();
        let lookup = positions.clone();
        let project = move |w: (f64, f64)| lookup.iter().position(|&p| p == w).map(|i| (540.0 + i as f32 * 5.0, 960.0));
        let unthemed = |i: u32| StoryLabel {
            at: (f64::from(i), 0.0),
            text: format!("U{i}"),
            size: 26.0,
            weight: 700,
            color: [0, 0, 0, 255],
            halo: [255; 4],
            halo_width: 0.0,
            beside: None,
            feature: format!("l/u{i}"),
            id: None,
            state: PointState::Next,
        };
        let now_label = StoryLabel {
            at: (64.0, 0.0),
            text: "NowLabel".into(),
            size: 26.0,
            weight: 700,
            color: [0, 0, 0, 255],
            halo: [255; 4],
            halo_width: 0.0,
            beside: None,
            feature: "l/now".into(),
            id: None,
            state: PointState::Now,
        };
        let mut labels: Vec<StoryLabel> = (0..64).map(unthemed).collect();
        labels.push(now_label);
        let layer = StoryLayerDraw {
            key: 1,
            layer_id: "l".into(),
            geojson: "{}".into(),
            paint: std::sync::Arc::new(serde_json::Map::new()),
            line_metrics: false,
            per_feature: false,
            above_labels: false,
            above_buildings: false,
            line_fx: crate::render::gpu::LINE_FX_NONE,
            features: Vec::new(),
            marks: None,
            arrowheads: Vec::new(),
            markers: None,
            labels,
        };
        let (placed, warnings) =
            place_story_labels(std::slice::from_ref(&layer), project, |_, _, _| (100.0, 26.0), 1.0, (1080.0, 1920.0), &[], &[], &[]);
        assert_eq!(placed.len(), MAX_STORY_LABELS, "{placed:?}");
        let kept: std::collections::HashSet<&str> = placed.iter().map(|p| p.text.as_str()).collect();
        assert!(kept.contains("NowLabel"), "the now label always wins the cap: {placed:?}");
        for i in 0..63 {
            assert!(kept.contains(format!("U{i}").as_str()), "the 63 closest unthemed labels stay: U{i}");
        }
        assert!(!kept.contains("U63"), "the farthest unthemed label drops, not the now label: {warnings:?}");
    }

    /// Spec §6.3-§6.4: `line-cap: round` reaches the stroke. A 40 px line
    /// from lon -50 to 50 (x 89 to 231 at zoom 0): butt-capped it ends at
    /// the vertices; round-capped it reaches half its width past them, on
    /// the line's axis only (a disc, not a square). Hermetic: no tiles.
    #[test]
    fn line_cap_round_draws_round_ends() {
        let doc = serde_json::json!({"type": "FeatureCollection", "features": [{"type": "Feature", "id": 0, "properties": {},
            "geometry": {"type": "LineString", "coordinates": [[-50.0, 0.0], [50.0, 0.0]]}}]});
        let shot = |cap: Option<&str>| {
            let mut paint = serde_json::json!({"line-color": "#ff0000", "line-width": 40});
            if let Some(cap) = cap {
                paint["line-cap"] = serde_json::json!(cap);
                paint["line-join"] = serde_json::json!("round");
            }
            one_layer_frame(320, 320, &doc, paint, Vec::new())
        };
        let (butt, absent, round) = (shot(Some("butt")), shot(None), shot(Some("round")));
        let east = 160.0 + 50.0 / 360.0 * 512.0; // 231.1
        let (x_past, y) = ((east + 12.0) as u32, 160);
        assert!(is_red(butt.pixel(x_past - 20, y)), "the line itself drew");
        assert!(!is_red(butt.pixel(x_past, y)), "a butt end stops at the vertex");
        assert_eq!(absent.pixel(x_past, y), butt.pixel(x_past, y), "no `line-cap` is the butt end every overlay has drawn");
        assert!(is_red(round.pixel(x_past, y)), "a round end reaches past the vertex");
        assert!(is_red(round.pixel((160.0 - 50.0 / 360.0 * 512.0 - 12.0) as u32, y)), "at both ends");
        assert!(!is_red(round.pixel(x_past + 4, y - 18)), "the end is a disc: its corner is not drawn");
    }

    /// Spec §6.3-§6.4: the arrowhead's base is centred on the line's last
    /// vertex and it points on along the last segment, whichever way that
    /// runs on screen: here east, then north.
    #[test]
    fn the_arrowhead_sits_on_the_last_vertex_rotated_to_the_bearing() {
        let red = [255, 0, 0, 255];
        let at = |end: (f64, f64), from: (f64, f64)| {
            let doc = serde_json::json!({"type": "FeatureCollection", "features": []});
            let head = StoryArrowhead { end, from, color: red, length: 44.0, width: 40.0, edge_color: [255, 255, 255, 255], edge_width: 5.0, stub_width: 12.0 };
            one_layer_frame(320, 320, &doc, serde_json::json!({"line-color": "#ff0000"}), vec![head])
        };
        // East: the end at the centre, the line coming from the west.
        let east = at((0.0, 0.0), (-20.0, 0.0));
        assert!(is_red(east.pixel(160 + 10, 160)), "the triangle starts at the end");
        assert!(is_red(east.pixel(160 + 36, 160)), "and runs on to near its tip, 44 px east");
        assert!(!is_red(east.pixel(160 + 50, 160)), "nothing past the tip");
        assert!(is_red(east.pixel(160 - 6, 160)), "the stub lays the core's colour over the shaft's end");
        assert!(!is_red(east.pixel(160 - 6, 160 - 10)), "the stub is core-wide");
        let white = |p: [u8; 4]| p[0] > 230 && p[1] > 230 && p[2] > 230;
        assert!(white(east.pixel(160 + 46, 160)), "a white edge past the tip");
        assert!(white(east.pixel(160 - 6, 160 - 8)), "the edge runs on along the stub, as the casing does");
        assert!(!white(east.pixel(160 - 6, 160)), "and never crosses the shaft");
        assert!(is_red(east.pixel(160 + 3, 160 - 16)) && is_red(east.pixel(160 + 3, 160 + 16)), "the base is 40 wide, across the line");
        assert!(!is_red(east.pixel(160 + 36, 160 - 16)), "and narrows to the tip");
        // North: the same triangle turned a quarter.
        let north = at((0.0, 0.0), (0.0, -20.0));
        assert!(is_red(north.pixel(160, 160 - 36)) && !is_red(north.pixel(160 + 36, 160)), "it points north");
    }

    fn head(end: (f64, f64), from: (f64, f64)) -> StoryArrowhead {
        StoryArrowhead { end, from, color: [255, 0, 0, 255], length: 22.0, width: 20.0, edge_color: [255; 4], edge_width: 2.5, stub_width: 6.0 }
    }

    /// No bearing, no arrowhead: two points on one screen pixel.
    #[test]
    fn an_arrowhead_with_no_bearing_on_screen_is_not_drawn() {
        let h = head((10.0, 0.0), (9.0, 0.0));
        assert!(arrowhead_quad(&h, (100.0, 50.0), (100.0, 50.0), 1.0, 1.0).is_none());
        assert!(arrowhead_quad(&h, (100.0, 50.0), (100.0, 50.0 + 1e-5), 1.0, 1.0).is_none());
        assert!(arrowhead_quad(&h, (100.0, 50.0), (f32::NAN, 50.0), 1.0, 1.0).is_none());
        assert!(arrowhead_quad(&h, (100.0, 50.0), (90.0, 50.0), 1.0, 1.0).is_some());
    }

    /// The antimeridian: a last segment from 179 to 181 (a story line,
    /// unwrapped) and one handed over wrapped, 179 to -179, both point
    /// EAST, the short way, on a projection linear in world x; -179 to
    /// 179 points WEST.
    #[test]
    fn an_arrowhead_across_the_antimeridian_points_the_short_way() {
        let project = |(x, y): (f64, f64)| Some(((x / 1000.0) as f32, (y / 1000.0) as f32));
        use std::f32::consts::FRAC_PI_2;
        for (end, from, want) in
            [((181.0, 0.0), (179.0, 0.0), FRAC_PI_2), ((-179.0, 0.0), (179.0, 0.0), FRAC_PI_2), ((179.0, 0.0), (-179.0, 0.0), -FRAC_PI_2)]
        {
            let mut layer = StoryLayerDraw {
                key: 0,
                layer_id: "a".into(),
                geojson: "{}".into(),
                paint: std::sync::Arc::new(serde_json::Map::new()),
                line_metrics: false,
                per_feature: false,
                above_labels: false,
                above_buildings: false,
                line_fx: crate::render::gpu::LINE_FX_NONE,
                features: Vec::new(),
                marks: None,
                arrowheads: vec![head(end, from)], markers: None, labels: Vec::new(),
            };
            let quads = arrowhead_quads(std::slice::from_ref(&layer), project, 1.0);
            assert_eq!(quads.len(), 1);
            assert!((quads[0].1.rotation_rad - want).abs() < 1e-3, "{from:?} -> {end:?}: rotation {}", quads[0].1.rotation_rad);
            assert_eq!(quads[0].0, 24, "a 6-unit stub at the 20-unit base: 6 texels, 24 quarters");
            layer.line_fx[2] = 0.0;
            assert!(!any_arrowheads(std::slice::from_ref(&layer)), "a hidden layer draws none, so no projection is built");
        }
    }

    #[test]
    fn an_override_recolours_scales_alpha_and_a_dots_diameter() {
        let red = Rgba { r: 1.0, g: 0.0, b: 0.0, a: 0.5 };
        let dot = crate::overlay::plan::line_op(
            geo_types::Geometry::MultiPoint(geo_types::MultiPoint(vec![geo_types::Point::new(0, 0)])),
            red,
            (10.0, 10.0),
            crate::style::DashPattern::none(),
        );
        let fx = FeatureFx { id: 0, color: Some([0, 0, 255, 255]), alpha: 0.5, radius_scale: 2.0 };
        let PaintOp::Line { color_from, width_from, .. } = overridden(&dot, &fx) else { panic!("a line op") };
        assert_eq!((color_from.r, color_from.b), (0.0, 1.0));
        assert!((color_from.a - 0.25).abs() < 1e-6, "the op's own alpha kept, times the override: {}", color_from.a);
        assert_eq!(width_from, 20.0);
    }
}

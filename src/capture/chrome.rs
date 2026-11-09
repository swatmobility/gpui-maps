//! The capture's **chrome pass** (map-stories spec §5.4): the story
//! burn-in drawn over everything else in the frame -- captions, the
//! attribution plate bottom-right, the SWAT mark bottom-left.
//!
//! None of it is new drawing. The credit is `map_view::attribution_chrome`,
//! the layout `MapView::paint` draws; the mark is `brand_mark::instance`
//! through the `IconPipeline`, with its artwork picked by
//! `brand_mark::resolve_for_frame`, as `paint` does; captions are glyphon
//! text on the same `PlatePipeline` rounded plate. A request that names
//! none of the three builds nothing and draws nothing, so every golden
//! frame is unchanged.
use super::session::FontState;
use super::{CaptureRequest, StoryFrame};
use crate::render::{GpuContext, IconInstance, PlateInstance};
use crate::story::timeline::{ActiveCaption, Anchor};
use crate::symbol_pass::{chrome_text_face, LabelDraw, LabelShapeCache};

/// A caption's distance from the frame's edges, in LOGICAL pixels.
pub(crate) const CAPTION_MARGIN_PX: f32 = 24.0;
/// Room between a caption's plate and its text, in LOGICAL pixels.
pub(crate) const CAPTION_PLATE_PADDING_X_PX: f32 = 12.0;
pub(crate) const CAPTION_PLATE_PADDING_Y_PX: f32 = 6.0;
/// The caption plate's corner radius, in LOGICAL pixels.
pub(crate) const CAPTION_PLATE_RADIUS_PX: f32 = 6.0;
/// The caption plate: black at 55%, under the default white text
/// (`timeline::DEFAULT_CAPTION_COLOR`). Its alpha is multiplied by the
/// caption's fade.
pub(crate) const CAPTION_PLATE_COLOR: [f32; 4] = [0.0, 0.0, 0.0, 0.55];

/// The chrome atlas's solid sprite: an all-inside SDF square, so any quad
/// drawn from it with an [`crate::render::SdfPaint`] is a flat rectangle
/// in the paint's fill at any size and angle. (Task 14's furniture draws
/// its swatches, bars and round circle swatches as plates instead: the
/// plate pipeline's rounded rectangle is a true circle, this square is not.)
#[allow(dead_code)]
pub(crate) const STORY_SOLID: &str = "story-solid";
/// [`STORY_SOLID`]'s side, texels.
pub(crate) const STORY_SOLID_SIZE: u32 = 8;

/// [`STORY_SOLID`]'s pixels: every texel fully inside the shape (an SDF
/// texel at 255 is the farthest inside a field encodes).
pub(crate) fn story_solid_sprite() -> image::RgbaImage {
    image::RgbaImage::from_pixel(STORY_SOLID_SIZE, STORY_SOLID_SIZE, image::Rgba([255, 255, 255, 255]))
}

/// The chrome atlas's line sprite: a horizontal bar whose distance field
/// ramps across its short axis, so the `SdfPaint` smoothstep antialiases a
/// slanted line's long edges (review F5). Callout leaders draw with it.
#[allow(dead_code)]
pub(crate) const STORY_LINE: &str = "story-line";
/// [`STORY_LINE`]'s side, texels.
pub(crate) const STORY_LINE_SIZE: u32 = 8;
/// How many of [`STORY_LINE`]'s rows the line itself fills: its edges sit
/// one texel in from the top and the bottom, leaving one texel of field
/// outside each for the ramp.
pub(crate) const STORY_LINE_INNER: u32 = 6;

/// [`STORY_LINE`]'s pixels: each row's signed distance (texels, positive
/// inside) to the nearer line edge, encoded as every SDF sprite here is --
/// the edge at [`crate::render::icon_pipeline::SDF_EDGE`], `1 / SDF_PX` of the field per
/// texel -- and the same along every column.
pub(crate) fn story_line_sprite() -> image::RgbaImage {
    let edge = (STORY_LINE_SIZE - STORY_LINE_INNER) as f32 / 2.0;
    image::RgbaImage::from_fn(STORY_LINE_SIZE, STORY_LINE_SIZE, |_, y| {
        let centre = y as f32 + 0.5;
        let d = (centre - edge).min(STORY_LINE_SIZE as f32 - edge - centre);
        let field = crate::render::icon_pipeline::SDF_EDGE + d / crate::render::icon_pipeline::SDF_PX;
        let v = (field * 255.0).round().clamp(0.0, 255.0) as u8;
        image::Rgba([v, v, v, v])
    })
}

/// The compass needle's sprite side, texels, and the needle's length
/// inside it, tip to tip (the rest is field for the edge and the halo).
pub(crate) const NORTH_ARROW_SDF_SIZE: u32 = 64;
pub(crate) const NORTH_ARROW_SDF_ARROW: u32 = 52;

/// The compass needle (y down, north up): a diamond, its north tip, east
/// corner, south tip, west corner. Centred on the sprite, so a quad turned
/// about its centre turns the needle about its pivot.
const NEEDLE: [(f32, f32); 4] = [(32.0, 6.0), (42.0, 32.0), (32.0, 58.0), (22.0, 32.0)];
/// The needle's south half: the triangle below the diamond's waist.
const NEEDLE_SOUTH: [(f32, f32); 3] = [(42.0, 32.0), (32.0, 58.0), (22.0, 32.0)];
/// How far the light south half is inset inside the dark diamond, texels:
/// the dark rim that outlines it on the light plate.
const NEEDLE_SOUTH_INSET: f32 = 2.0;

/// `poly`'s distance field (texels, positive inside, the edge moved
/// `inset` texels inward), encoded as every SDF sprite here is: the edge
/// at [`crate::render::icon_pipeline::SDF_EDGE`], `1 / SDF_PX` of the
/// field per texel.
fn polygon_sdf(poly: &[(f32, f32)], inset: f32) -> image::GrayImage {
    polygon_sdf_sized(poly, inset, NORTH_ARROW_SDF_SIZE)
}

/// [`polygon_sdf`] on a `size`-texel square sprite.
fn polygon_sdf_sized(poly: &[(f32, f32)], inset: f32, size: u32) -> image::GrayImage {
    let n = poly.len();
    image::GrayImage::from_fn(size, size, |x, y| {
        let p = (x as f32 + 0.5, y as f32 + 0.5);
        let mut nearest = f32::INFINITY;
        let mut inside = false;
        for (i, &a) in poly.iter().enumerate() {
            let b = poly.get((i + 1) % n).copied().unwrap_or(a);
            let (dx, dy) = (b.0 - a.0, b.1 - a.1);
            let len2 = (dx * dx + dy * dy).max(f32::EPSILON);
            let t = (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0);
            nearest = nearest.min((p.0 - a.0 - t * dx).hypot(p.1 - a.1 - t * dy));
            if (a.1 > p.1) != (b.1 > p.1) && p.0 < a.0 + (p.1 - a.1) / (b.1 - a.1) * (b.0 - a.0) {
                inside = !inside;
            }
        }
        let d = if inside { nearest } else { -nearest } - inset;
        let field = crate::render::icon_pipeline::SDF_EDGE + d / crate::render::icon_pipeline::SDF_PX;
        image::Luma([(field * 255.0).round().clamp(0.0, 255.0) as u8])
    })
}

/// The compass needle (spec §4.5's north arrow) as a distance field: the
/// whole diamond, drawn dark with a light halo. Built procedurally: no
/// asset.
pub(crate) fn furniture_arrow_sdf() -> image::GrayImage {
    polygon_sdf(&NEEDLE, 0.0)
}

/// The needle's south half, inset by [`NEEDLE_SOUTH_INSET`]: drawn light
/// over [`furniture_arrow_sdf`], it leaves the north half dark and a dark
/// rim round the light south half -- a two-tone needle, not an arrowhead.
pub(crate) fn furniture_arrow_south_sdf() -> image::GrayImage {
    polygon_sdf(&NEEDLE_SOUTH, NEEDLE_SOUTH_INSET)
}

/// The story arrowhead (spec §6.3-§6.4's `story-arrowhead`) in its
/// [`STORY_ARROWHEAD_SIZE`] sprite, y down, at one texel a unit -- so at
/// a pixel ratio of 1 a texel is a pixel, the 2.5-unit edge stays well
/// inside the field's 6 texels and its soft outer ramp never reaches the
/// sprite's empty corners (at two texels a unit they showed as a faint
/// square): a triangle pointing up, [`STORY_ARROWHEAD_LEN`] texels tip to base and
/// [`STORY_ARROWHEAD_W`] across the base, its base at
/// [`STORY_ARROWHEAD_BASE_Y`]; and behind the base a stub as wide as the
/// arrow's core (the theme's 6 units, or the author's `line-width`,
/// [`story_arrowhead_sdf`]'s `stub`) running out of the sprite's bottom
/// edge. The
/// stub lies over the shaft's last units, so the head's white edge (its
/// SDF halo, R7-a) runs on along the stub's sides as the casing does and
/// never crosses the shaft: shaft and head read as one shape. The tip
/// keeps 5 texels of field for the edge.
fn arrowhead_polygon(stub: f32) -> [(f32, f32); 7] {
    let (l, r) = (18.0 - stub / 2.0, 18.0 + stub / 2.0);
    [(18.0, 5.0), (28.0, 27.0), (r, 27.0), (r, 36.5), (l, 36.5), (l, 27.0), (8.0, 27.0)]
}
/// The narrowest and widest stub the sprite draws, texels (units): a core
/// wider than the widest is narrowed under the head, so the base keeps
/// 2 units of shoulder each side.
pub(crate) const STORY_ARROWHEAD_STUB: (f32, f32) = (1.0, 16.0);
/// The arrowhead sprite's side, texels.
pub(crate) const STORY_ARROWHEAD_SIZE: u32 = 36;
/// The arrowhead's length and base width inside its sprite, texels: the
/// spec's 22 x 20 units.
pub(crate) const STORY_ARROWHEAD_LEN: f32 = 22.0;
pub(crate) const STORY_ARROWHEAD_W: f32 = 20.0;
/// Where the arrowhead's base sits in its sprite, texels from the top:
/// the sprite's centre is `BASE_Y - SIZE / 2` texels on from the base.
pub(crate) const STORY_ARROWHEAD_BASE_Y: f32 = 27.0;

/// The story arrowhead as a distance field, its stub `stub` texels wide
/// (clamped to [`STORY_ARROWHEAD_STUB`]): drawn as an SDF icon in the
/// arrow core's colour. Built procedurally: no asset.
pub(crate) fn story_arrowhead_sdf(stub: f32) -> image::GrayImage {
    let stub = stub.clamp(STORY_ARROWHEAD_STUB.0, STORY_ARROWHEAD_STUB.1);
    polygon_sdf_sized(&arrowhead_polygon(stub), 0.0, STORY_ARROWHEAD_SIZE)
}

/// [`story_arrowhead_sdf`] as the RGBA the icon atlas uploads.
pub(crate) fn story_arrowhead_sprite(stub: f32) -> image::RgbaImage {
    sdf_sprite(&story_arrowhead_sdf(stub))
}

/// A field as the RGBA the icon atlas uploads (the field in every channel,
/// as [`story_line_sprite`] writes it).
fn sdf_sprite(sdf: &image::GrayImage) -> image::RgbaImage {
    image::RgbaImage::from_fn(sdf.width(), sdf.height(), |x, y| {
        let v = sdf.get_pixel(x, y).0[0];
        image::Rgba([v, v, v, v])
    })
}

/// The chrome pass's GPU state, built on the first frame that draws any
/// chrome and kept by the session: the two pipelines and each brand-mark
/// variant's uploaded artwork.
pub(crate) struct ChromeGpu {
    icons: crate::render::IconPipeline,
    plates: crate::render::PlatePipeline,
    marks: [Option<(wgpu::BindGroup, (u32, u32))>; crate::brand_mark::RESOLVED_VARIANTS],
    /// The user-supplied wordmark's upload, keyed by the `Arc` it came
    /// from: a new artwork is uploaded once, the same one never again.
    wordmark: Option<(std::sync::Arc<image::RgbaImage>, wgpu::BindGroup)>,
    /// The text state, kept across frames as `MapView` keeps its own: the
    /// atlas holds last frame's glyphs (trimmed after each frame), so a
    /// caption or credit that stays on screen is not re-rasterised.
    text: ChromeText,
    /// The story pass's own pipelines and text renderer (sharing the
    /// atlas), built on the first frame with a story: each pipeline keeps
    /// one vertex buffer per frame, so the story's draws cannot share the
    /// chrome's without one overwriting the other before either runs.
    story: Option<StoryGpu>,
    /// The target's format, for building `story` late.
    format: wgpu::TextureFormat,
    /// [`STORY_SOLID`], uploaded once in [`Self::new`].
    #[allow(dead_code)]
    solid: wgpu::BindGroup,
}

/// The story pass's own pipelines: each keeps one vertex buffer per frame,
/// so the panel plates (under the images) and the image credits' plates
/// (over them) need two. Shared by the capture's chrome pass and the live
/// `MapView` (spec R2: the engine draws the panels, one way): both build
/// it over their own text atlas and draw a [`StoryChromeDraw`] with
/// [`Self::draw`].
///
/// [`StoryChromeDraw`]: super::story_chrome::StoryChromeDraw
pub(crate) struct StoryGpu {
    plates: crate::render::PlatePipeline,
    /// The callouts' leaders, between the panel plates and the callout
    /// plates.
    leaders: crate::render::IconPipeline,
    /// The callouts' dots and cards, over every leader (review F2).
    callout_plates: crate::render::PlatePipeline,
    icons: crate::render::IconPipeline,
    credit_plates: crate::render::PlatePipeline,
    /// The story's text, over the shared atlas: the owner prepares it.
    pub(crate) text: glyphon::TextRenderer,
    /// [`STORY_LINE`].
    line: wgpu::BindGroup,
    /// [`furniture_arrow_sdf`] and [`furniture_arrow_south_sdf`].
    arrow: wgpu::BindGroup,
    arrow_south: wgpu::BindGroup,
    /// One texture per story image (spec §4.3), keyed by its `Arc`'s
    /// address, and the `Arc` itself (so no other image can take that
    /// address while the entry lives). Searched linearly, never iterated
    /// for draw order; an entry no frame and no player still holds is
    /// dropped.
    images: Vec<(super::story_chrome::ImageKey, std::sync::Arc<image::RgbaImage>, wgpu::BindGroup)>,
}

/// A mip chain already built for an image (`Arc` address -> its levels),
/// so a caller that prepared them off the paint path (the live map's
/// `story::LiveStory`) only uploads. `None`: build it here.
pub(crate) type PremippedLookup<'a> = &'a dyn Fn(&std::sync::Arc<image::RgbaImage>) -> Option<std::sync::Arc<Vec<image::RgbaImage>>>;

impl StoryGpu {
    pub(crate) fn new(ctx: &GpuContext, format: wgpu::TextureFormat, atlas: &mut glyphon::TextAtlas) -> Self {
        let icons = crate::render::IconPipeline::new(ctx, format);
        let line = icons.upload_atlas(ctx, &story_line_sprite());
        let arrow = icons.upload_atlas(ctx, &sdf_sprite(&furniture_arrow_sdf()));
        let arrow_south = icons.upload_atlas(ctx, &sdf_sprite(&furniture_arrow_south_sdf()));
        Self {
            plates: crate::render::PlatePipeline::new(ctx, format),
            leaders: crate::render::IconPipeline::new(ctx, format),
            callout_plates: crate::render::PlatePipeline::new(ctx, format),
            icons,
            credit_plates: crate::render::PlatePipeline::new(ctx, format),
            text: glyphon::TextRenderer::new(atlas, &ctx.device, wgpu::MultisampleState::default(), None),
            line,
            arrow,
            arrow_south,
            images: Vec::new(),
        }
    }

    /// Uploads `story`'s images (once per `Arc`, with their mip chains)
    /// and drops every entry neither this frame nor anyone else holds.
    pub(crate) fn sync_images(
        &mut self,
        ctx: &GpuContext,
        story: Option<(&super::story_chrome::StoryChromeDraw, &StoryFrame)>,
        premipped: Option<PremippedLookup<'_>>,
    ) {
        if let Some((s, frame)) = story {
            for (key, _) in &s.quads {
                if self.images.iter().any(|(k, ..)| k == key) {
                    continue;
                }
                let held = frame.images.iter().find(|i| std::sync::Arc::as_ptr(&i.image) as super::story_chrome::ImageKey == *key);
                if let Some(img) = held.filter(|i| i.image.width() > 0 && i.image.height() > 0) {
                    // With its mip chain: a corner draws a frame-sized photo
                    // several times smaller (Task 12 review F12).
                    let levels = premipped
                        .and_then(|lookup| lookup(&img.image))
                        .unwrap_or_else(|| std::sync::Arc::new(crate::story::media::mip_chain(&img.image)));
                    if let Some(bind_group) = self.icons.upload_mipped(ctx, &levels) {
                        self.images.push((*key, std::sync::Arc::clone(&img.image), bind_group));
                    }
                }
            }
        }
        let used: Vec<super::story_chrome::ImageKey> = story.map(|(s, _)| s.quads.iter().map(|(k, _)| *k).collect()).unwrap_or_default();
        self.images.retain(|(key, image, _)| used.contains(key) || std::sync::Arc::strong_count(image) > 1);
    }

    /// Draws `s` into `pass`: the dim and panel plates, the leaders and
    /// the compass needle, the callout plates, the images in ONE
    /// `render_batches` call, the image credits' plates, and last the text
    /// `self.text` was prepared with, when `text` is given.
    pub(crate) fn draw(
        &mut self,
        ctx: &GpuContext,
        pass: &mut wgpu::RenderPass<'_>,
        s: &super::story_chrome::StoryChromeDraw,
        text: Option<(&glyphon::TextAtlas, &glyphon::Viewport)>,
        (fw, fh): (f32, f32),
    ) {
        let story_plates: Vec<PlateInstance> = s.dim.iter().chain(s.plates.iter()).copied().collect();
        self.plates.render(ctx, pass, &story_plates, fw, fh);
        // The callouts' leaders (each underlay before its line), then
        // their dots and cards over every leader, so a card hides any
        // other callout's leader crossing it (review F2). Each in its
        // own pipeline: one vertex buffer per pipeline per frame.
        // The north arrow rides the same call: one vertex buffer per
        // pipeline per frame.
        let lines: Vec<(&wgpu::BindGroup, &[IconInstance])> =
            [(&self.line, &s.leaders[..]), (&self.arrow, &s.north[..]), (&self.arrow_south, &s.north_south[..])]
                .into_iter()
                .filter(|(_, quads)| !quads.is_empty())
                .collect();
        if !lines.is_empty() {
            self.leaders.render_batches(ctx, pass, &lines, fw, fh);
        }
        if !s.callout_plates.is_empty() {
            self.callout_plates.render(ctx, pass, &s.callout_plates, fw, fh);
        }
        // The images over their panels and callout cards, in placement
        // order, in ONE `render_batches` call.
        let images = &self.images;
        let batches: Vec<(&wgpu::BindGroup, &[IconInstance])> = s
            .quads
            .iter()
            .filter_map(|(key, quad)| {
                let (.., bind_group) = images.iter().find(|(k, ..)| k == key)?;
                Some((bind_group, std::slice::from_ref(quad)))
            })
            .collect();
        if !batches.is_empty() {
            self.icons.render_batches(ctx, pass, &batches, fw, fh);
        }
        if !s.credit_plates.is_empty() {
            self.credit_plates.render(ctx, pass, &s.credit_plates, fw, fh);
        }
        if let Some((atlas, viewport)) = text {
            let _ = self.text.render(atlas, viewport, pass);
        }
    }
}

struct ChromeText {
    _cache: glyphon::Cache,
    viewport: glyphon::Viewport,
    atlas: glyphon::TextAtlas,
    renderer: glyphon::TextRenderer,
    swash: glyphon::SwashCache,
}

impl ChromeGpu {
    pub(crate) fn new(ctx: &GpuContext, format: wgpu::TextureFormat) -> Self {
        let cache = glyphon::Cache::new(&ctx.device);
        let viewport = glyphon::Viewport::new(&ctx.device, &cache);
        // `ColorMode::Web`, as `MapView` and the capture's label pass build it.
        let mut atlas =
            glyphon::TextAtlas::with_color_mode(&ctx.device, &ctx.queue, &cache, format, glyphon::ColorMode::Web);
        let renderer = glyphon::TextRenderer::new(&mut atlas, &ctx.device, wgpu::MultisampleState::default(), None);
        let icons = crate::render::IconPipeline::new(ctx, format);
        let solid = icons.upload_atlas(ctx, &story_solid_sprite());
        Self {
            icons,
            solid,
            plates: crate::render::PlatePipeline::new(ctx, format),
            marks: [None, None],
            wordmark: None,
            text: ChromeText { _cache: cache, viewport, atlas, renderer, swash: glyphon::SwashCache::new() },
            story: None,
            format,
        }
    }

    /// The wordmark's bind group, uploading `artwork` when it is not the
    /// one already uploaded.
    fn wordmark_artwork(&mut self, ctx: &GpuContext, artwork: &std::sync::Arc<image::RgbaImage>) -> Option<&wgpu::BindGroup> {
        let fresh = !self.wordmark.as_ref().is_some_and(|(held, _)| std::sync::Arc::ptr_eq(held, artwork));
        if fresh && artwork.width() > 0 && artwork.height() > 0 {
            self.wordmark = Some((std::sync::Arc::clone(artwork), self.icons.upload_atlas(ctx, artwork)));
        }
        self.wordmark.as_ref().map(|(_, bind_group)| bind_group)
    }

    /// The artwork size of `variant`, uploading it on first use; `None`
    /// if the vendored bytes somehow do not decode.
    fn mark_artwork(&mut self, ctx: &GpuContext, variant: crate::brand_mark::ResolvedVariant) -> Option<(u32, u32)> {
        let slot = variant.index();
        if self.marks.get(slot).is_some_and(Option::is_none) {
            match crate::brand_mark::decode(variant.png()) {
                Ok(artwork) => {
                    let dimensions = artwork.dimensions();
                    if let Some(entry) = self.marks.get_mut(slot) {
                        *entry = Some((self.icons.upload_atlas(ctx, &artwork), dimensions));
                    }
                }
                Err(err) => eprintln!("gpui-maps: brand mark failed to decode, not drawing it: {err:#}"),
            }
        }
        self.marks.get(slot).and_then(|m| m.as_ref()).map(|(_, dimensions)| *dimensions)
    }
}

/// Whether `request` asks for any chrome at all.
pub(crate) fn wanted(request: &CaptureRequest) -> bool {
    request.attribution.as_deref().is_some_and(|c| !c.is_empty())
        || request.brand_mark.is_some()
        || request.wordmark.is_some()
        || request.captions.iter().any(|c| c.alpha > 0.0 && !c.text.is_empty())
        || request.story.is_some()
}

/// A rectangle as `(left, top, right, bottom)`, physical pixels.
pub(crate) type Rect = (f32, f32, f32, f32);

fn intersects(a: Rect, b: Rect) -> bool {
    a.0 < b.2 && b.0 < a.2 && a.1 < b.3 && b.1 < a.3
}

/// Where a caption box of `size` (plate included) sits: at `anchor`, a
/// `margin` in from the frame's edges, moved by `offset`, then kept clear
/// of every `obstacle` (the mark's footprint, the credit's plate). A box
/// that meets one moves away from the edge it is anchored to -- up for a
/// bottom or middle anchor, down for a top one -- to a `margin / 2` gap,
/// and is finally clamped inside the frame. Physical pixels throughout.
pub(crate) fn caption_origin(
    anchor: Anchor,
    offset: (f32, f32),
    size: (f32, f32),
    frame: (f32, f32),
    margin: f32,
    obstacles: &[Rect],
) -> (f32, f32) {
    let (w, h) = size;
    let (fw, fh) = frame;
    let x = match anchor {
        Anchor::TopLeft | Anchor::Left | Anchor::BottomLeft => margin,
        Anchor::Top | Anchor::Center | Anchor::Bottom => (fw - w) / 2.0,
        Anchor::TopRight | Anchor::Right | Anchor::BottomRight => fw - margin - w,
    };
    let y = match anchor {
        Anchor::TopLeft | Anchor::Top | Anchor::TopRight => margin,
        Anchor::Left | Anchor::Center | Anchor::Right => (fh - h) / 2.0,
        Anchor::BottomLeft | Anchor::Bottom | Anchor::BottomRight => fh - margin - h,
    };
    let (x, mut y) = (x + offset.0, y + offset.1);
    let downward = matches!(anchor, Anchor::TopLeft | Anchor::Top | Anchor::TopRight);
    let gap = margin / 2.0;
    // Each pass can only move the box further the same way, so as many
    // passes as there are obstacles settle it.
    for _ in 0..obstacles.len() {
        let mut moved = false;
        for &o in obstacles {
            if intersects((x, y, x + w, y + h), o) {
                y = if downward { o.3 + gap } else { o.1 - gap - h };
                moved = true;
            }
        }
        if !moved {
            break;
        }
    }
    let x = x.clamp(0.0, (fw - w).max(0.0));
    let y = y.clamp(0.0, (fh - h).max(0.0));
    (x.round(), y.round())
}

fn caption_text_color(caption: &ActiveCaption) -> crate::style::Rgba {
    let [r, g, b, a] = caption.color;
    crate::style::Rgba {
        r: f32::from(r) / 255.0,
        g: f32::from(g) / 255.0,
        b: f32::from(b) / 255.0,
        a: f32::from(a) / 255.0 * caption.alpha.clamp(0.0, 1.0),
    }
}

/// The chrome's geometry for one frame, physical pixels: the plates
/// (captions' and the credit's), the text draws (captions, then the
/// credit), and the mark's quad.
pub(crate) struct ChromeLayout {
    pub(crate) plates: Vec<PlateInstance>,
    pub(crate) text: Vec<(LabelDraw, Option<crate::collision::CollisionBox>)>,
    pub(crate) mark: Option<IconInstance>,
    /// The wordmark's quad, beside the mark ([`wordmark_quad`]).
    pub(crate) wordmark: Option<IconInstance>,
    /// The story's panels and title card (`request.story`), drawn before
    /// everything above so the credit, the mark and the captions stay on
    /// top. `None` without a story.
    pub(crate) story: Option<super::story_chrome::StoryChromeDraw>,
    /// Every footprint above that basemap labels must keep clear of,
    /// physical pixels ([`label_obstacle_rects`]): the credit's plate, the
    /// mark, the wordmark, then every placed story element.
    pub(crate) label_obstacles: Vec<Rect>,
    /// The credit's plate, for the tests.
    #[cfg(test)]
    pub(crate) credit_plate: Option<PlateInstance>,
}

/// One chrome text draw: what `symbol_pass::text_areas` takes.
pub(crate) type ChromeTextDraw = (LabelDraw, Option<crate::collision::CollisionBox>);

/// A caption the story layout placed (`placed`, 1080 units): its plate,
/// when it has one, and its text, shaped into `shape_cache` at the wrap
/// the layout measured it at. `None` for an empty one. `fw` is the frame
/// width in physical pixels.
pub(crate) fn placed_caption(
    placed: &crate::story::layout::Placed,
    caption: &ActiveCaption,
    (fw, pr): (f32, f32),
    font_system: &mut glyphon::FontSystem,
    shape_cache: &mut LabelShapeCache,
) -> Option<(Option<PlateInstance>, ChromeTextDraw)> {
    let (_, inner, units, words) = placed.text.first()?;
    if words.is_empty() {
        return None;
    }
    let face = chrome_text_face();
    let justify = crate::overlay::paint::TextJustify::Center;
    let size = units * pr;
    // The layout's own wrap when a side rail narrowed the caption, else
    // the frame's caption wrap.
    let wrap = Some(placed.text_wrap.unwrap_or_else(|| crate::story::layout::caption_wrap(fw / pr, caption.plate)) * pr);
    shape_cache.get_or_shape(font_system, words, size, wrap, justify, &face);
    let (left, top) = ((placed.rect.0 * pr).round(), (placed.rect.1 * pr).round());
    let alpha = caption.alpha.clamp(0.0, 1.0);
    let plate = caption.plate.then(|| {
        let [r, g, b, a] = CAPTION_PLATE_COLOR;
        PlateInstance {
            screen_x: left,
            screen_y: top,
            width: (placed.rect.2 - placed.rect.0) * pr,
            height: (placed.rect.3 - placed.rect.1) * pr,
            radius_px: CAPTION_PLATE_RADIUS_PX * pr,
            color: [r, g, b, a * alpha],
        }
    });
    let color = caption_text_color(caption);
    let (tx, ty) = (left + ((inner.0 - placed.rect.0) * pr).round(), top + ((inner.1 - placed.rect.1) * pr).round());
    Some((plate, ((LabelShapeCache::key(words, size, wrap, justify, &face), tx, ty, color, 0.0, color, 0.0), None)))
}

/// How far (physical pixels) the corner chrome sits in from the right and
/// up from the bottom of a `frame` (physical pixels) at `pr`: the story's
/// safe-area bottom band and side rail ([`crate::story::layout::corner_insets`]),
/// so the licence credit and the mark stay visible on the platform (final
/// review F3). `(0, 0)` without a story or a safe area -- every frame drawn
/// before it (the goldens).
pub(crate) fn story_corner_inset(story: Option<&StoryFrame>, frame: (f32, f32), pr: f32) -> (f32, f32) {
    let pr = if pr.is_finite() && pr > 0.0 { pr } else { 1.0 };
    story.map_or((0.0, 0.0), |s| {
        let (right, bottom) = crate::story::layout::corner_insets((frame.0 / pr, frame.1 / pr), &s.safe_area);
        (right * pr, bottom * pr)
    })
}

/// Every chrome footprint basemap labels keep clear of, physical pixels
/// (spec §7, Task R13 fix: the order, not just the membership). The
/// credit's plate first (final review F7: the non-story corner, never
/// part of a design frame's own card or row); then the story's placed
/// elements ([`super::story_chrome::StoryChromeDraw::obstacles`], final
/// review V1) -- the card grown by 8, then the colophon row's fine-print
/// lines grown by 8; then `mark` and `wordmark`, which for a design frame
/// ARE the row's own logo quad (it heads the row, spec §7.2), so they
/// belong after the row, not before the card. One list for the capture's
/// label placement and the live map's, each padded on insertion by
/// [`crate::symbol_pass::chrome_obstacle`].
pub(crate) fn label_obstacle_rects(
    plate: Option<&PlateInstance>,
    mark: Option<&IconInstance>,
    wordmark: Option<&IconInstance>,
    story: Option<&super::story_chrome::StoryChromeDraw>,
) -> Vec<Rect> {
    let plate = plate.map(|p| rect_of(p.screen_x, p.screen_y, p.width, p.height));
    let quads = mark.into_iter().chain(wordmark).map(rect_of_quad);
    plate.into_iter().chain(story.into_iter().flat_map(|s| s.obstacles.iter().copied())).chain(quads).collect()
}

/// A story frame laid out for the LIVE map (`MapView::set_story_frame`,
/// spec R2): [`super::story_chrome::build_story_chrome`] against the
/// view's own corner chrome, exactly as [`layout`] calls it for a capture
/// of the same size, and what [`layout`] draws after the story: the
/// credit's and the captions' plates ([`Self::tail_plates`], in
/// `layout`'s order) and their text ([`Self::tail_text`]).
pub(crate) struct LiveStoryChrome {
    pub draw: super::story_chrome::StoryChromeDraw,
    /// The credit plate, then each placed caption's plate.
    pub tail_plates: Vec<PlateInstance>,
    /// Each placed caption's text, then the credit's lines.
    pub tail_text: Vec<ChromeTextDraw>,
    /// [`label_obstacle_rects`] for this frame: what the capture's label
    /// placement keeps clear of for the same frame.
    pub label_obstacles: Vec<Rect>,
}

/// The view's corner chrome as [`live_story_chrome`] reads it (physical
/// pixels): the credit's plate and lines, and the mark's quad.
pub(crate) struct LiveCorner<'a> {
    pub plate: Option<&'a PlateInstance>,
    /// The plate's (i) button, drawn over it.
    pub button: Option<&'a PlateInstance>,
    pub credit_lines: &'a [ChromeTextDraw],
    pub mark: Option<&'a IconInstance>,
    /// The wordmark's quad (final review F7). `MapView` draws no wordmark
    /// today, so it passes `None`; a live map that draws one passes it
    /// here, and the story and the labels keep clear of it as the
    /// capture's do.
    pub wordmark: Option<&'a IconInstance>,
    /// The design frame's slot mark width, 1080 units
    /// ([`crate::story::frame_layout::slot_mark_w`] of the mark the view
    /// draws), as [`CaptureRequest::slot_mark_w`] reads it for a capture.
    pub slot_mark: Option<f32>,
    /// The view's own tile credit: the fine print's on a design frame
    /// that carries none (a live story does not know the view's tile
    /// source), so the live map never drops its licence credit. A frame
    /// with a credit draws its own, as a capture does.
    pub credit: Option<&'a str>,
}

/// Builds [`LiveStoryChrome`] for `frame` on a `size` (physical pixels)
/// view at `pr`, clear of `corner`'s plate and mark, shaping into
/// `font_system`/`shape_cache` -- the story's own font state, the faces
/// the video draws with. `captions` are the frame's, indexed as its
/// `caption/<i>` items name them.
pub(crate) fn live_story_chrome(
    frame: &StoryFrame,
    captions: &[ActiveCaption],
    size: (u32, u32),
    pr: f32,
    corner: &LiveCorner<'_>,
    font_system: &mut glyphon::FontSystem,
    shape_cache: &mut LabelShapeCache,
) -> LiveStoryChrome {
    let mut obstacles: Vec<Rect> = Vec::new();
    obstacles.extend(corner.plate.map(|p| rect_of(p.screen_x, p.screen_y, p.width, p.height)));
    obstacles.extend(corner.mark.map(rect_of_quad));
    obstacles.extend(corner.wordmark.map(rect_of_quad));
    let units: Vec<Rect> = obstacles.iter().map(|o| (o.0 / pr, o.1 / pr, o.2 / pr, o.3 / pr)).collect();
    // A design frame without a credit takes the view's (`LiveCorner::credit`).
    let filled = frame.design.as_ref().filter(|d| d.credit.is_none()).and(corner.credit.filter(|c| !c.is_empty())).map(|credit| {
        let mut f = frame.clone();
        if let Some(d) = f.design.as_mut() {
            d.credit = Some(credit.to_string());
        }
        f
    });
    let frame = filled.as_ref().unwrap_or(frame);
    let draw = {
        let fonts = std::cell::RefCell::new((&mut *font_system, &mut *shape_cache));
        super::story_chrome::build_story_chrome(frame, size, pr, &units, corner.slot_mark, &fonts)
    };
    let label_obstacles = label_obstacle_rects(corner.plate, corner.mark, corner.wordmark, Some(&draw));
    let mut out = LiveStoryChrome { draw, tail_plates: corner.plate.copied().into_iter().chain(corner.button.copied()).collect(), tail_text: Vec::new(), label_obstacles };
    for (i, caption) in captions.iter().enumerate().filter(|(_, c)| c.alpha > 0.0 && !c.text.is_empty()) {
        let id = super::story_chrome::caption_id(i);
        let Some(placed) = out.draw.captions.iter().find(|p| p.id == id) else { continue };
        if let Some((plate, draw)) = placed_caption(placed, caption, (size.0 as f32, pr), font_system, shape_cache) {
            out.tail_plates.extend(plate);
            out.tail_text.push(draw);
        }
    }
    out.tail_text.extend(corner.credit_lines.iter().cloned());
    out
}

fn rect_of(x: f32, y: f32, w: f32, h: f32) -> Rect {
    (x, y, x + w, y + h)
}

fn rect_of_quad(q: &IconInstance) -> Rect {
    rect_of(q.screen_x, q.screen_y, q.width, q.height)
}

/// The widest a wordmark may be, logical pixels (a banner-shaped artwork
/// is scaled down to fit rather than run under the credit).
const WORDMARK_MAX_WIDTH_PX: f32 = 140.0;

/// Where a `size` (pixels) wordmark sits: at the mark's height, right of
/// the mark with half its height as the gap, bottoms aligned -- or where
/// the default mark would sit when none draws. If that meets the credit's
/// `plate` (a frame too narrow for all three on one row) it goes above
/// the mark instead, left edges aligned. Never over the mark: the two
/// rectangles are disjoint by construction.
pub(crate) fn wordmark_quad(
    mark: Option<&IconInstance>,
    plate: Option<Rect>,
    size: (u32, u32),
    frame: (f32, f32),
    pixel_ratio: f32,
) -> Option<IconInstance> {
    let (aw, ah) = (size.0 as f32, size.1 as f32);
    if aw <= 0.0 || ah <= 0.0 {
        return None;
    }
    // Without a mark, the wordmark takes the mark's own corner slot.
    let slot = match mark {
        Some(m) => (m.screen_x, m.screen_y, m.width, m.height),
        None => {
            let (x, y, _, h) = crate::brand_mark::BrandMark::default().footprint(frame.1 / pixel_ratio.max(1e-3));
            (x * pixel_ratio, y * pixel_ratio, 0.0, h * pixel_ratio)
        }
    };
    let (mark_x, mark_y, mark_w, mark_h) = slot;
    let mut h = mark_h;
    let mut w = h * aw / ah;
    let max_w = WORDMARK_MAX_WIDTH_PX * pixel_ratio;
    if w > max_w {
        h *= max_w / w;
        w = max_w;
    }
    let gap = if mark.is_some() { mark_h / 2.0 } else { 0.0 };
    let beside = (mark_x + mark_w + gap, mark_y + mark_h - h);
    let above = (mark_x, mark_y - mark_h / 2.0 - h);
    let meets_plate = |(x, y): (f32, f32)| plate.is_some_and(|p| intersects((x, y, x + w, y + h), p)) || x + w > frame.0;
    let (x, y) = if meets_plate(beside) { above } else { beside };
    Some(IconInstance {
        screen_x: x.round(),
        screen_y: y.round(),
        width: w,
        height: h,
        uv_min: (0.0, 0.0),
        uv_max: (1.0, 1.0),
        rotation_rad: 0.0,
        sdf: None,
        opacity: 1.0,
    })
}

/// The design system's mark, bottom-left on every frame (spec §3.6, D7
/// as amended 2026-10-05), physical pixels: [`crate::story::card_layout::LOGO_H`]
/// units tall at the artwork's aspect, its INK's top-left at `ink_at`
/// (1080 units, [`super::story_chrome::design_mark_origin`]) -- the ink
/// edge rounded to a whole pixel and the quad standing `margin` units
/// (the artwork's transparent side margin,
/// [`crate::story::frame_layout::slot_mark_margin`]) further left.
pub(crate) fn design_logo_quad(artwork: (u32, u32), ink_at: (f32, f32), margin: f32, opacity: f32, pr: f32) -> Option<IconInstance> {
    let (aw, ah) = (artwork.0 as f32, artwork.1 as f32);
    if aw <= 0.0 || ah <= 0.0 || !(pr.is_finite() && pr > 0.0) || !(ink_at.0.is_finite() && ink_at.1.is_finite()) {
        return None;
    }
    let h = crate::story::card_layout::LOGO_H * pr;
    Some(IconInstance {
        screen_x: (ink_at.0 * pr).round() - margin * pr,
        screen_y: (ink_at.1 * pr).round(),
        width: h * aw / ah,
        height: h,
        uv_min: (0.0, 0.0),
        uv_max: (1.0, 1.0),
        rotation_rad: 0.0,
        sdf: None,
        opacity: opacity.clamp(0.0, 1.0),
    })
}

/// Lays the chrome out. `mark_artwork` is the mark's artwork size when a
/// mark draws. The credit is `map_view::attribution_chrome` (stacked, and
/// broken at words when one credit alone is too wide); if its plate still
/// meets the mark's corner quad -- a frame too narrow for both on one row
/// -- the mark moves up to sit above the plate, never under it. Captions
/// then keep clear of the plate, the mark and each other.
pub(crate) fn layout(
    request: &CaptureRequest,
    fonts: &mut FontState,
    mark_artwork: Option<(u32, u32)>,
    tone: crate::attribution::AttributionTone,
) -> ChromeLayout {
    let (fw, fh) = (request.width as f32, request.height as f32);
    let pr = request.pixel_ratio;
    let FontState { system: font_system, shapes: shape_cache, .. } = fonts;
    let mut plates: Vec<PlateInstance> = Vec::new();
    let mut text: Vec<(LabelDraw, Option<crate::collision::CollisionBox>)> = Vec::new();
    let mut obstacles: Vec<Rect> = Vec::new();

    // The credit, bottom-right, and the mark, bottom-left, lifted above the
    // plate if the two would meet: `paint`'s own layout (`corner_chrome`),
    // the same one label placement kept clear of -- above the story's
    // safe-area bottom band, when it has one.
    let inset = story_corner_inset(request.story.as_deref(), (fw, fh), pr);
    // A design-system frame (the slice) draws its credit in its own fine
    // print and its logo at (64, h - 58): no corner plate, no corner mark.
    let design = request.story.as_deref().is_some_and(|s| s.design.is_some());
    let corner = crate::map_view::corner_chrome(
        request.attribution.as_deref().filter(|_| !design),
        fw,
        fh,
        pr,
        request.brand_mark.as_ref().zip(mark_artwork).filter(|_| !design),
        inset,
        // A capture is never collapsed: a still has no press to reopen it.
        crate::attribution::CompactState::default(),
        tone,
        font_system,
        shape_cache,
    );
    let credit_plate = corner.plate;
    let credit_button = corner.button;
    let credit_text = corner.credit_lines;
    // A design frame's mark is bottom-left (spec §3.6): where the design
    // layout puts it, the same call the live corner makes.
    let slot = request.slot_mark_w(mark_artwork);
    let design_at = request.story.as_deref().filter(|_| design).and_then(|frame| {
        let fonts = std::cell::RefCell::new((&mut *font_system, &mut *shape_cache));
        super::story_chrome::design_mark_origin(frame, (request.width, request.height), pr, slot, None, &fonts)
    });
    let mark = if design {
        // P3: the mark only when the policy draws it (a Mapbox wordmark
        // takes the slot otherwise).
        let margin = crate::story::frame_layout::slot_mark_margin(true);
        request.effective_brand_mark().zip(mark_artwork).zip(design_at).and_then(|((b, art), at)| design_logo_quad(art, at, margin, b.opacity, pr))
    } else {
        corner.mark
    };
    if let Some(plate) = credit_plate {
        obstacles.push(rect_of(plate.screen_x, plate.screen_y, plate.width, plate.height));
        plates.push(plate);
        plates.extend(credit_button);
    }
    if let Some(quad) = mark.as_ref() {
        obstacles.push(rect_of(quad.screen_x, quad.screen_y, quad.width, quad.height));
    }
    let wordmark = request.wordmark.as_ref().and_then(|art| {
        if design && mark.is_none() {
            return design_at.and_then(|at| design_logo_quad(art.dimensions(), at, crate::story::frame_layout::slot_mark_margin(false), 1.0, pr));
        }
        let plate = credit_plate.map(|p| rect_of(p.screen_x, p.screen_y, p.width, p.height));
        wordmark_quad(mark.as_ref(), plate, art.dimensions(), (fw - inset.0, fh - inset.1), pr)
    });
    if let Some(quad) = wordmark.as_ref() {
        obstacles.push(rect_of_quad(quad));
    }

    // The story (spec §4.1): laid out clear of the three above, in 1080
    // units, before the captions -- which it places too.
    let story = request.story.as_deref().map(|frame| {
        let units: Vec<Rect> = obstacles.iter().map(|o| (o.0 / pr, o.1 / pr, o.2 / pr, o.3 / pr)).collect();
        let fonts = std::cell::RefCell::new((&mut *font_system, &mut *shape_cache));
        // P3: the slot's mark as drawn above (the logo, else the wordmark).
        super::story_chrome::build_story_chrome(frame, (request.width, request.height), pr, &units, slot, &fonts)
    });

    // Captions, clear of both and of each other.
    let face = chrome_text_face();
    let justify = crate::overlay::paint::TextJustify::Center;
    for (i, caption) in request.captions.iter().enumerate().filter(|(_, c)| c.alpha > 0.0 && !c.text.is_empty()) {
        // With a story, the layout placed it (and fit its text); its plate
        // and colour are still the caption's own.
        let laid = story.as_ref().and_then(|s| {
            let id = super::story_chrome::caption_id(i);
            s.captions.iter().find(|p| p.id == id)
        });
        if let Some(placed) = laid {
            if let Some((plate, draw)) = placed_caption(placed, caption, (fw, pr), font_system, shape_cache) {
                plates.extend(plate);
                text.push(draw);
            }
            continue;
        }
        let size = caption.size_px * pr;
        let shaped = shape_cache.get_or_shape(font_system, &caption.text, size, None, justify, &face);
        let (tw, th) = (shaped.measured_width, shaped.measured_height);
        let (pad_x, pad_y) = if caption.plate {
            (CAPTION_PLATE_PADDING_X_PX * pr, CAPTION_PLATE_PADDING_Y_PX * pr)
        } else {
            (0.0, 0.0)
        };
        let (w, h) = (tw + pad_x * 2.0, th + pad_y * 2.0);
        let (left, top) = caption_origin(
            caption.anchor,
            (caption.offset_px.0 * pr, caption.offset_px.1 * pr),
            (w, h),
            (fw, fh),
            CAPTION_MARGIN_PX * pr,
            &obstacles,
        );
        obstacles.push(rect_of(left, top, w, h));
        let alpha = caption.alpha.clamp(0.0, 1.0);
        if caption.plate {
            let [r, g, b, a] = CAPTION_PLATE_COLOR;
            plates.push(PlateInstance {
                screen_x: left,
                screen_y: top,
                width: w,
                height: h,
                radius_px: CAPTION_PLATE_RADIUS_PX * pr,
                color: [r, g, b, a * alpha],
            });
        }
        let color = caption_text_color(caption);
        text.push((
            (LabelShapeCache::key(&caption.text, size, None, justify, &face), left + pad_x, top + pad_y, color, 0.0, color, 0.0),
            None,
        ));
    }
    // Captions first, then the credit (spec §5.4's order).
    text.extend(credit_text);
    let label_obstacles = label_obstacle_rects(credit_plate.as_ref(), mark.as_ref(), wordmark.as_ref(), story.as_ref());
    ChromeLayout {
        plates,
        text,
        mark,
        wordmark,
        story,
        label_obstacles,
        #[cfg(test)]
        credit_plate,
    }
}

/// Encodes the chrome pass into `encoder`, over `color_view`: the story
/// first (the title's dim, the panel plates, their text), then the chrome's
/// plates, text and the mark, so the credit, the mark and the captions stay
/// on top. `variant` is the mark's artwork, already decided by
/// `brand_mark::resolve_for_frame` against what is behind it. `chrome` is
/// this frame's [`layout`] -- the one the session's label placement kept
/// clear of ([`ChromeLayout::label_obstacles`]), laid out once, so what is
/// drawn and what labels avoided cannot differ. Returns the story layout's
/// warnings (empty without a story).
#[allow(clippy::too_many_arguments)]
pub(crate) fn encode(
    ctx: &GpuContext,
    gpu: &mut ChromeGpu,
    fonts: &mut FontState,
    request: &CaptureRequest,
    variant: Option<crate::brand_mark::ResolvedVariant>,
    mut chrome: ChromeLayout,
    encoder: &mut wgpu::CommandEncoder,
    color_view: &wgpu::TextureView,
) -> Vec<crate::story::layout::LayoutWarning> {
    let (fw, fh) = (request.width as f32, request.height as f32);
    let artwork = match (request.brand_mark, variant) {
        (Some(_), Some(variant)) => gpu.mark_artwork(ctx, variant).map(|size| (variant.index(), size)),
        _ => None,
    };
    let story = chrome.story.take();
    if story.is_some() && gpu.story.is_none() {
        gpu.story = Some(StoryGpu::new(ctx, gpu.format, &mut gpu.text.atlas));
    }
    // This frame's images uploaded (once per `Arc`); entries nothing holds
    // any more dropped.
    if let Some(sg) = gpu.story.as_mut() {
        sg.sync_images(ctx, story.as_ref().zip(request.story.as_deref()), None);
    }
    // Uploaded before the pass borrows the text state.
    let wordmark_ready = request.wordmark.as_ref().is_some_and(|art| gpu.wordmark_artwork(ctx, art).is_some());
    let FontState { system: font_system, shapes: shape_cache, .. } = fonts;

    let t = &mut gpu.text;
    t.viewport.update(&ctx.queue, glyphon::Resolution { width: request.width, height: request.height });
    // The story's text first, into its own renderer over the shared atlas.
    let story_ready = match (story.as_ref(), gpu.story.as_mut()) {
        (Some(s), Some(StoryGpu { text: renderer, .. })) => {
            let areas = crate::symbol_pass::text_areas(&s.text, shape_cache, fw, fh);
            renderer.prepare(&ctx.device, &ctx.queue, font_system, &mut t.atlas, &t.viewport, areas, &mut t.swash).is_ok()
        }
        _ => false,
    };
    let areas = crate::symbol_pass::text_areas(&chrome.text, shape_cache, fw, fh);
    // A prepare error loses the text and keeps the frame, as `paint` does.
    let text_ready = t
        .renderer
        .prepare(&ctx.device, &ctx.queue, font_system, &mut t.atlas, &t.viewport, areas, &mut t.swash)
        .is_ok();

    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("capture-chrome-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: color_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        if let (Some(s), Some(sg)) = (story.as_ref(), gpu.story.as_mut()) {
            let text = story_ready.then_some((&t.atlas, &t.viewport));
            sg.draw(ctx, &mut pass, s, text, (fw, fh));
        }
        gpu.plates.render(ctx, &mut pass, &chrome.plates, fw, fh);
        if text_ready {
            let _ = t.renderer.render(&t.atlas, &t.viewport, &mut pass);
        }
        // The mark and the wordmark in ONE `render_batches` call: the icon
        // pipeline keeps one vertex buffer, and a second call would
        // overwrite the first's quads before either draw ran (see its doc
        // comment) -- both would draw at the wordmark's place.
        let mark_quad = chrome.mark.map(|q| [q]);
        let wordmark_quad = chrome.wordmark.filter(|_| wordmark_ready).map(|q| [q]);
        let mut batches: Vec<(&wgpu::BindGroup, &[IconInstance])> = Vec::with_capacity(2);
        if let (Some(quads), Some((slot, _))) = (mark_quad.as_ref(), artwork) {
            if let Some(Some((bind_group, _))) = gpu.marks.get(slot) {
                batches.push((bind_group, &quads[..]));
            }
        }
        if let (Some(quads), Some((_, bind_group))) = (wordmark_quad.as_ref(), gpu.wordmark.as_ref()) {
            batches.push((bind_group, &quads[..]));
        }
        if !batches.is_empty() {
            gpu.icons.render_batches(ctx, &mut pass, &batches, fw, fh);
        }
    }
    // Glyphs no frame is using any more leave the atlas, as `paint` trims.
    t.atlas.trim();
    story.map(|s| s.warnings).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quad(x: f32, y: f32, w: f32, h: f32) -> IconInstance {
        IconInstance { screen_x: x, screen_y: y, width: w, height: h, uv_min: (0.0, 0.0), uv_max: (1.0, 1.0), rotation_rad: 0.0, sdf: None, opacity: 1.0 }
    }

    /// The compass needle's fields: the diamond is inside along its
    /// spine from tip to tip and outside beside its waist; the south half
    /// is inside only below the waist and inset from the diamond's edge;
    /// both leave field to spare for the halo and are mirror-symmetric.
    #[test]
    fn the_compass_needle_is_a_diamond_with_an_inset_south_half() {
        let (all, south) = (furniture_arrow_sdf(), furniture_arrow_south_sdf());
        let edge = (crate::render::icon_pipeline::SDF_EDGE * 255.0).round() as u8;
        let at = |img: &image::GrayImage, x: u32, y: u32| img.get_pixel(x, y).0[0];
        assert!(at(&all, 32, 10) > edge && at(&all, 32, 54) > edge && at(&all, 32, 32) > edge, "spine inside");
        assert!(at(&all, 46, 32) < edge && at(&all, 18, 32) < edge, "outside the waist");
        assert!(at(&all, 32, 3) > 0 && at(&all, 2, 2) == 0, "room for the halo, empty corners");
        assert!(at(&south, 32, 20) < edge, "the north half is not in the south sprite");
        assert!(at(&south, 32, 40) > edge, "the south half is");
        // Inset: just inside the diamond's south edge, the south sprite is outside.
        assert!(at(&all, 32, 56) > edge && at(&south, 32, 56) < edge, "a dark rim at the south tip");
        for img in [&all, &south] {
            for y in 0..64 {
                for x in 0..32 {
                    assert_eq!(at(img, x, y), at(img, 63 - x, y), "({x}, {y})");
                }
            }
        }
    }

    /// The Task 17 ruling: a user-supplied wordmark draws beside the SWAT
    /// mark, at its height, bottoms aligned, never over it or the credit.
    #[test]
    fn the_wordmark_sits_beside_the_mark_and_never_meets_it_or_the_credit() {
        let mark = quad(4.0, 1055.0, 87.0, 18.0);
        let plate = (1500.0, 1050.0, 1916.0, 1076.0);
        let w = wordmark_quad(Some(&mark), Some(plate), (400, 90), (1920.0, 1080.0), 1.0).unwrap();
        assert_eq!(w.height, 18.0);
        assert!((w.width - 80.0).abs() < 0.01, "aspect kept: {}", w.width);
        assert_eq!(w.screen_x, 4.0 + 87.0 + 9.0, "half the mark's height as the gap");
        assert_eq!(w.screen_y + w.height, mark.screen_y + mark.height, "bottoms aligned");
        assert!(!intersects(rect_of_quad(&w), rect_of_quad(&mark)));
        assert!(!intersects(rect_of_quad(&w), plate));
        // A frame too narrow for all three on one row: above the mark.
        let narrow_plate = (150.0, 1050.0, 400.0, 1076.0);
        let w = wordmark_quad(Some(&mark), Some(narrow_plate), (400, 90), (400.0, 1080.0), 1.0).unwrap();
        assert_eq!(w.screen_x, mark.screen_x);
        assert!(w.screen_y + w.height <= mark.screen_y, "above the mark");
        assert!(!intersects(rect_of_quad(&w), narrow_plate));
        // A banner-shaped artwork is scaled down to the cap.
        let w = wordmark_quad(Some(&mark), None, (2000, 50), (1920.0, 1080.0), 2.0).unwrap();
        assert!(w.width <= WORDMARK_MAX_WIDTH_PX * 2.0 + 1e-3 && w.height < 18.0);
        // Degenerate artwork draws nothing.
        assert!(wordmark_quad(Some(&mark), None, (0, 10), (1920.0, 1080.0), 1.0).is_none());
    }

    #[test]
    fn a_bottom_left_caption_is_lifted_clear_of_the_mark() {
        let mark = (4.0, 300.0, 40.0, 353.0);
        let (x, y) = caption_origin(Anchor::BottomLeft, (0.0, 0.0), (200.0, 40.0), (640.0, 360.0), 24.0, &[mark]);
        assert_eq!(x, 24.0);
        assert!(y + 40.0 <= mark.1, "caption bottom {} over the mark's top {}", y + 40.0, mark.1);
        assert_eq!(y, 300.0 - 12.0 - 40.0);
    }

    #[test]
    fn a_caption_clear_of_everything_stays_at_its_anchor() {
        let plate = (500.0, 340.0, 636.0, 356.0);
        assert_eq!(caption_origin(Anchor::TopRight, (0.0, 0.0), (100.0, 30.0), (640.0, 360.0), 24.0, &[plate]), (516.0, 24.0));
        assert_eq!(caption_origin(Anchor::Center, (10.0, -5.0), (100.0, 30.0), (640.0, 360.0), 24.0, &[plate]), (280.0, 160.0));
    }

    #[test]
    fn a_caption_is_kept_inside_the_frame() {
        let (x, y) = caption_origin(Anchor::BottomRight, (500.0, 500.0), (100.0, 30.0), (640.0, 360.0), 24.0, &[]);
        assert_eq!((x, y), (540.0, 330.0));
    }

    #[test]
    fn two_captions_at_one_anchor_do_not_overlap() {
        let first = caption_origin(Anchor::Bottom, (0.0, 0.0), (200.0, 40.0), (640.0, 360.0), 24.0, &[]);
        let placed = (first.0, first.1, first.0 + 200.0, first.1 + 40.0);
        let second = caption_origin(Anchor::Bottom, (0.0, 0.0), (200.0, 40.0), (640.0, 360.0), 24.0, &[placed]);
        assert!(!intersects((second.0, second.1, second.0 + 200.0, second.1 + 40.0), placed), "{first:?} {second:?}");
    }

    fn noto_fonts() -> FontState {
        let f = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/fonts/NotoSans-Regular.latin-subset.ttf")).unwrap();
        let mut state = None;
        let fonts: Option<crate::capture::LabelFonts> = Some((vec![f].into(), "Noto Sans".to_string()));
        super::super::session::fonts_for(&mut state, &fonts);
        state.unwrap()
    }

    /// F2: at any output width the credit plate and the SWAT mark never
    /// meet, the plate stays inside the frame, and no credit line is wider
    /// than the frame -- with the Sentinel-2 credit, the longest one.
    #[test]
    fn the_credit_and_the_mark_never_overlap_and_the_credit_is_never_clipped() {
        let credit = [crate::story::burn_in::OFM_CREDIT, crate::story::burn_in::S2CLOUDLESS_CREDIT].join(" | ");
        let artwork = {
            let a = crate::brand_mark::decode(crate::brand_mark::SWAT_ON_LIGHT_PNG).unwrap();
            a.dimensions()
        };
        for (width, height, ratio) in [(480, 270, 1.0), (590, 332, 1.0), (1280, 720, 1.0), (240, 400, 1.0), (1920, 1080, 2.0)] {
            let mut fonts = noto_fonts();
            let request = CaptureRequest {
                width,
                height,
                pixel_ratio: ratio,
                attribution: Some(credit.clone()),
                brand_mark: Some(crate::brand_mark::BrandMark::default()),
                ..CaptureRequest::default()
            };
            let chrome = layout(&request, &mut fonts, Some(artwork), Default::default());
            let plate = chrome.credit_plate.unwrap();
            let mark = chrome.mark.unwrap();
            let p = rect_of(plate.screen_x, plate.screen_y, plate.width, plate.height);
            let m = rect_of(mark.screen_x, mark.screen_y, mark.width, mark.height);
            assert!(!intersects(p, m), "{width}x{height}@{ratio}: plate {p:?} meets mark {m:?}");
            assert!(p.0 >= 0.0 && p.2 <= width as f32 && p.1 >= 0.0, "{width}: plate {p:?} leaves the frame");
            assert!(m.1 >= 0.0, "{width}: mark {m:?} off the top");
            for ((key, left, ..), _) in &chrome.text {
                let shaped = fonts.shapes.get(key).unwrap();
                assert!(
                    *left >= p.0 && left + shaped.measured_width <= p.2 + 0.5,
                    "{width}: line {:?} at {left} + {} spills out of plate {p:?}",
                    key.0,
                    shaped.measured_width
                );
            }
            // Wide frames keep the mark at its corner.
            if width >= 1280 {
                let corner = crate::brand_mark::instance(&crate::brand_mark::BrandMark::default(), artwork, height as f32, ratio);
                assert_eq!(mark.screen_y, corner.screen_y, "{width}: the mark moved on a wide frame");
            }
        }
    }

    /// Final review F3 (licensing): under a story with a platform safe
    /// area, the credit plate, the SWAT mark and the wordmark sit above the
    /// platform's bottom band, outside every safe-area rectangle, at 1080p
    /// and at 4K -- so the credit stays visible on Reels/TikTok/Shorts. With
    /// no safe area, they stay at the frame's corner.
    #[test]
    fn under_a_platform_safe_area_the_credit_plate_is_outside_every_safe_area_rect() {
        use crate::story::tiers::{safe_area_rects, Aspect, SafeArea};
        let credit = [crate::story::burn_in::OFM_CREDIT, crate::story::burn_in::S2CLOUDLESS_CREDIT].join(" | ");
        let artwork = crate::brand_mark::artwork_size();
        let wordmark = std::sync::Arc::new(image::RgbaImage::from_pixel(400, 90, image::Rgba([0, 0, 0, 255])));
        for area in [SafeArea::Reels, SafeArea::Tiktok, SafeArea::Shorts, SafeArea::None] {
            for (aspect, (w, h)) in [(Aspect::Portrait, (1080u32, 1920u32)), (Aspect::Feed, (1080, 1350))] {
                for ratio in [1.0f32, 2.0] {
                    let rects = safe_area_rects(area, aspect);
                    let request = CaptureRequest {
                        width: (w as f32 * ratio) as u32,
                        height: (h as f32 * ratio) as u32,
                        pixel_ratio: ratio,
                        attribution: Some(credit.clone()),
                        brand_mark: Some(crate::brand_mark::BrandMark::default()),
                        wordmark: Some(crate::capture::Artwork(std::sync::Arc::clone(&wordmark))),
                        story: Some(std::sync::Arc::new(StoryFrame { safe_area: rects.clone(), ..StoryFrame::default() })),
                        ..CaptureRequest::default()
                    };
                    let chrome = layout(&request, &mut noto_fonts(), Some(artwork), Default::default());
                    let plate = chrome.credit_plate.unwrap();
                    let pieces = [
                        ("plate", rect_of(plate.screen_x, plate.screen_y, plate.width, plate.height)),
                        ("mark", rect_of_quad(&chrome.mark.unwrap())),
                        ("wordmark", rect_of_quad(&chrome.wordmark.unwrap())),
                    ];
                    for (name, r) in pieces {
                        let units = (r.0 / ratio, r.1 / ratio, r.2 / ratio, r.3 / ratio);
                        for s in &rects {
                            assert!(!intersects(units, *s), "{area:?} {aspect:?} @{ratio}: the {name} {units:?} meets {s:?}");
                        }
                        assert!(units.1 >= 0.0, "{area:?} {aspect:?}: the {name} stays on the frame");
                    }
                    // The labels keep clear of all three.
                    for (_, r) in pieces {
                        assert!(chrome.label_obstacles.contains(&r), "{area:?}: {r:?} is a label obstacle");
                    }
                    if area == SafeArea::None {
                        let corner = crate::brand_mark::instance(&crate::brand_mark::BrandMark::default(), artwork, h as f32 * ratio, ratio);
                        assert_eq!(chrome.mark.unwrap().screen_y, corner.screen_y, "no safe area: the mark at its corner");
                    } else {
                        let band = rects.iter().map(|r| r.1).filter(|t| *t > h as f32 / 2.0).fold(f32::INFINITY, f32::min);
                        assert!((band - pieces[0].1 .3 / ratio) < 40.0, "{area:?} {aspect:?}: the plate sits just above the band");
                    }
                }
            }
        }
    }

    /// Task 11 review F2: a plate-less caption laid out by the story layout
    /// draws its text where the story-less rule puts it (no plate padding
    /// moving it in), and a plated one keeps its plate's padding.
    #[test]
    fn a_plate_less_caption_keeps_its_old_position_under_a_story() {
        for plate in [false, true] {
            for anchor in [Anchor::BottomLeft, Anchor::Bottom, Anchor::TopRight, Anchor::Center] {
                let caption = ActiveCaption {
                    text: "Marina Bay".into(),
                    anchor,
                    offset_px: (10.0, -20.0),
                    size_px: 32.0,
                    color: [255, 255, 255, 255],
                    plate,
                    alpha: 1.0,
                };
                let base = CaptureRequest { width: 1920, height: 1080, captions: vec![caption.clone()], ..CaptureRequest::default() };
                let item = crate::story::layout::LayoutItem::Caption {
                    id: super::super::story_chrome::caption_id(0),
                    text: caption.text.clone(),
                    size: caption.size_px,
                    anchor,
                    offset: caption.offset_px,
                    plate,
                };
                let frame = crate::capture::StoryFrame { items: vec![item], ..crate::capture::StoryFrame::default() };
                let with_story = CaptureRequest { story: Some(std::sync::Arc::new(frame)), ..base.clone() };
                let mut fonts = noto_fonts();
                let old = layout(&base, &mut fonts, None, Default::default());
                let new = layout(&with_story, &mut fonts, None, Default::default());
                assert_eq!(new.story.as_ref().unwrap().captions.len(), 1, "the layout placed it");
                let ((_, ox, oy, ..), _) = &old.text[0];
                let ((_, nx, ny, ..), _) = &new.text[0];
                assert!((ox - nx).abs() <= 0.5 && (oy - ny).abs() <= 0.5, "plate {plate} {anchor:?}: old ({ox}, {oy}) new ({nx}, {ny})");
                assert_eq!(old.plates.len(), new.plates.len(), "plate {plate}: a plate only when asked");
                if let (Some(a), Some(b)) = (old.plates.first(), new.plates.first()) {
                    assert!((a.screen_x - b.screen_x).abs() <= 0.5 && (a.screen_y - b.screen_y).abs() <= 0.5);
                }
            }
        }
    }

    #[test]
    fn a_caption_fades_its_text_alpha() {
        let c = ActiveCaption {
            text: "x".into(),
            anchor: Anchor::Bottom,
            offset_px: (0.0, 0.0),
            size_px: 24.0,
            color: [255, 255, 255, 255],
            plate: true,
            alpha: 0.5,
        };
        assert!((caption_text_color(&c).a - 0.5).abs() < 1e-6);
    }
}

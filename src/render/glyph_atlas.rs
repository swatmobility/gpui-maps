//! A glyph atlas we own, for text that has to be drawn one rotated quad at
//! a time.
//!
//! # Why this exists
//!
//! Three `highway-name-*` layers in `examples/liberty-style.json` ask for
//! `symbol-placement: line` with `text-rotation-alignment: map`, and
//! `symbol_line` already computes the per-glyph angles they need. Those
//! angles cannot reach the text, though: every label in this engine is drawn
//! by `glyphon::TextArea`, which has no rotation, so a line-placed street
//! name is currently laid out along the polyline and then drawn
//! horizontally. Drawing it properly means one quad per glyph, each with its
//! own rotation -- and a quad needs a texture to sample. `glyphon`'s own
//! atlas is private to its renderer (`TextAtlas`'s allocations are reachable
//! only from `TextRenderer::prepare`), so per-glyph quads need an atlas of
//! our own. This is it. `symbol_pass::line_glyph_quads` is what draws from
//! it, through `IconPipeline`'s SDF branch as a second batch beside the
//! sprite atlas.
//!
//! # A signed distance field, not coverage -- and why coverage was rejected
//!
//! What swash hands back for a glyph is an alpha COVERAGE bitmap: one byte
//! per pixel saying how much of that pixel the outline covers, at one
//! specific pixel size. This module does not store that. It rasterises the
//! glyph to a binary mask at a fixed [`RASTER_SIZE_PX`] and runs it through
//! [`crate::sprite::sdf_from_mask`], so what lands in the atlas is a signed
//! DISTANCE FIELD in the crate's existing encoding -- the same encoding
//! `icon_shader.wgsl` already decodes for SDF sprites.
//!
//! Coverage was built first and rejected. It is the cheaper thing to store
//! and it is genuinely sharper at 11 px, because swash's own antialiasing
//! and pixel-grid hinting are preserved rather than re-derived. What killed
//! it is the halo. All five layers that need this feature ask for one:
//!
//! ```text
//! highway-name-path        halo-width 0.5  #f8f4f0  text hsl(30,23%,62%)
//! highway-name-minor       halo-width 1.2  #ffffff  text #7b7b7b
//! highway-name-secondary   halo-width 1.2  #ffffff  text #4a4a4a
//! highway-name-expressway  halo-width 1.4  #ffffff  text #2b7048
//! highway-name-major       halo-width 1.2  #ffffff  text #4a4a4a
//! ```
//!
//! A halo is the distance field thresholded FURTHER OUT from the shape's
//! edge. Coverage carries no distance, so there is nothing to threshold: a
//! coverage atlas cannot draw a halo at all, at any cost. `#7b7b7b` street
//! names over a grey road casing without the white halo the style asks for
//! is a label nobody can read, which is worse than no feature. That is not a
//! trade, it is a floor.
//!
//! Two more things came free with the reversal, both of which the coverage
//! version had to work around:
//!
//! - **Size leaves [`GlyphKey`].** A field is size-independent, so ONE
//!   raster serves every `text-size`. A zoom ramp across 11.4 px and 12.0 px
//!   no longer rasters twice, and the atlas has one entry per
//!   `(face, glyph, weight)` instead of one per size as well.
//! - **Colour comes from the quad, with no shader change.** `push_quad`
//!   already emits an SDF instance as two quads -- halo then fill -- each
//!   carrying its own colour (`push_one_quad(.., paint.halo, ..)`), so a
//!   per-label `text-color` and `text-halo-color` need nothing new in
//!   `icon_shader.wgsl`. The coverage version would have needed a third
//!   fragment branch, because its non-SDF path draws the texel's own pixels
//!   and would have drawn every street name white.
//!
//! What it costs, stated as plainly as the coverage note stated its own
//! costs:
//!
//! - **Softness, and a quantised edge, at small sizes.**
//!   [`crate::sprite::sdf_from_mask`] thresholds the mask at coverage 128,
//!   so the outline is quantised to the [`RASTER_SIZE_PX`] pixel grid BEFORE
//!   the distance transform runs. Everything the field knows about the
//!   letter's edge it knows to within one raster pixel. At 24 px that is
//!   visible as slight softness and slightly wobbly diagonals on an 11 px
//!   label, next to the hinted, per-size raster glyphon draws horizontal
//!   text with. Raising [`RASTER_SIZE_PX`] is the knob that buys that back.
//! - **A distance transform per glyph, once.** `sdf_from_mask` is
//!   an exact O(pixels) Euclidean distance transform (Felzenszwalb, as
//!   TinySDF), ~0.1 ms for one padded glyph (the brute-force search it
//!   replaced took ~1 ms). It runs once per `(face, glyph, weight)` for the lifetime of the
//!   atlas -- but the first frame a long unseen label appears pays for all
//!   its glyphs at once, and in a debug build that is measurable. See the
//!   task report's concerns.
//! - **No colour emoji.** A field is a silhouette; running a colour bitmap
//!   through it throws the colour away. `get_or_raster` answers `None` for
//!   one rather than drawing a blob (see [`GlyphAtlas::get_or_raster`]).
//!
//! # Why 24 px
//!
//! [`RASTER_SIZE_PX`] was 24 -- MapLibre's own glyph size -- until an iPhone
//! 15 Pro Max (3x) report: highway shields (point-placed, drawn by glyphon at
//! their true display size) looked crisp, but street names (line-placed,
//! drawn from THIS atlas) looked "creepy" -- soft, aliased edges next to the
//! shields' clean ones, on the same screen.
//!
//! The cause is `font_scale = physical_text_size / RASTER_SIZE_PX`
//! (`symbol_pass::line_glyph_quads`'s `physical_text_size` already carries
//! the display's scale factor). At `RASTER_SIZE_PX = 24`, a real
//! `road-label` at Singapore z17 (`examples/mapbox-style.json`, text-size
//! interpolated to ~13.17 at that zoom) has `font_scale` = 0.55 on a 1x
//! display -- MINIFIED, sampling the 24 px field down, which is exactly why
//! this never showed on desktop. On a 3x display the same label is `font_scale`
//! = 1.65 -- MAGNIFIED, stretching the same 24 px field's quantised edges
//! across more physical pixels than it has texels for, which is the
//! softness/aliasing the report describes. Point labels never go through
//! this atlas at all (`glyphon::TextArea` rasters at the true display size,
//! every time), which is why they stayed crisp beside it; even the
//! `symbol-placement: line` shield layers in both vendored styles are
//! `text-rotation-alignment: viewport` and so take the SAME glyphon path as
//! a point label (see `symbol_pass`'s routing, keyed on `text-rotation-alignment
//! == map`, not on `symbol-placement` alone) -- only `road-label`,
//! `road-label-secondary`, `road-label-minor` (this style) and
//! `highway-name-path`/`-minor`/`-major` (`examples/liberty-style.json`) are
//! `map`-aligned and reach this module.
//!
//! [`RASTER_SIZE_PX`] is now 48, doubling the raster and halving that
//! magnification (3x `road-label` drops from `font_scale` 1.65 to 0.82 --
//! MINIFIED again, the same regime the 1x build was already fine in). Two
//! quantities still pull against each other, and doubling rather than
//! matching a 3x display exactly (72) is why:
//!
//! - **Edge accuracy** wants it LARGE. The mask is binarised on the raster
//!   grid, so the field's knowledge of the outline is good to one raster
//!   pixel; doubling the raster size halves that error.
//! - **Halo headroom** wants it SMALL, and by an amount [`font_scale`]'s own
//!   invariance to scale factor makes SCALE-FACTOR-INDEPENDENT: the encoding
//!   leaves exactly 6 usable field texels outside the shape (`sdf_from_mask`'s
//!   doc; [`SDF_MARGIN`]), a texel is worth `font_scale` physical pixels, and
//!   a halo can reach `6 * font_scale` physical pixels before
//!   `sdf_thresholds`' clamp saturates it. Substituting `font_scale =
//!   (text_size * scale_factor) / RASTER_SIZE_PX` against a halo requirement
//!   of `halo_width_pt * scale_factor` physical pixels, the `scale_factor`
//!   on both sides CANCELS -- so the headroom ratio at 1x is the SAME
//!   headroom ratio at 3x, and the binding case is the SMALLEST real
//!   `text-size` a `map`-aligned layer ever asks for, at ANY display density.
//!   That is `road-label-minor`'s 9 px (its own `minzoom`, `halo-width: 1`):
//!   `9 * 6 / 1 = 54` is the largest `RASTER_SIZE_PX` that does not clip
//!   that label's halo, at ANY scale factor. 48 clears it with ~12% to
//!   spare (`the_smallest_real_road_label_keeps_halo_headroom_at_the_new_raster_size`);
//!   72 -- matching a 3x display exactly, which would make `font_scale`
//!   PERFECTLY invariant to scale factor rather than merely close -- does
//!   not (`54 / 72 < 1`, a clipped halo), and this module cannot widen
//!   [`SDF_MARGIN`] to buy 72 back without breaking the `SDF_HALO_BASE`
//!   parity `icon_pipeline.rs` (out of scope here) pins.
//!
//! 24 used to also make `font_scale` exactly MapLibre's `text_size / 24`,
//! the number `icon_pipeline::sdf_thresholds`'s halo-width arithmetic was
//! transcribed from line for line. That arithmetic is parametric on
//! whatever `font_scale` it is handed (`sdf_thresholds` never writes `24`
//! itself), so it is unaffected by this module choosing a different raster
//! size; only the literal value a style's halo width is divided by changes,
//! which is the point.
//!
//! Hinting is forced OFF for these rasters ([`GlyphKey::cache_key`]). A
//! hinted outline is distorted to fit the raster grid it was rastered on,
//! and that distortion is wrong at every other size the one field is then
//! drawn at. Mapbox's own glyph pipeline loads with `FT_LOAD_NO_HINTING` for
//! the same reason.
//!
//! # Memory, choosing 48 over 72
//!
//! Atlas memory is O(raster²) per glyph (`sdf_from_mask` pads and
//! transforms a `(RASTER_SIZE_PX + 2*SDF_MARGIN)²`-ish bitmap), so 24 -> 48
//! is 4x the texels per glyph, not the 9x a 24 -> 72 match to a 3x display
//! would cost. The atlas's own cap ([`MAX_ATLAS_HEIGHT`]) is unchanged, so
//! the WORST CASE resident texture is still exactly
//! `ATLAS_WIDTH * MAX_ATLAS_HEIGHT * 4` bytes (16 MiB) either way; what
//! changes is how many DISTINCT glyphs fit under that cap before
//! `get_or_raster` starts refusing new ones -- roughly 3,700 at 24 px, and
//! roughly 925 at 48 px. A Latin-script road network's distinct glyphs
//! (a few dozen letters and digits, times a couple of weights) is nowhere
//! near either number.

use std::collections::BTreeMap;

/// The em size, in pixels, every glyph in this atlas is rastered at before
/// the distance transform. See the module doc's "Why 48 px" (was 24 --
/// raised to cut the magnification a 3x display saw the 24 px field
/// through; see that section for the arithmetic and why 48 rather than a
/// 3x-exact 72).
///
/// This is NOT a display size. A quad drawn from this atlas is scaled by
/// [`font_scale`], and the drawn size can be anything.
pub(crate) const RASTER_SIZE_PX: f32 = 48.0;

/// Transparent margin, in raster pixels, left around a glyph's ink before
/// the distance transform -- so the field has room to run outward and a
/// halo is bounded by the ENCODING rather than clipped by the bitmap's own
/// edge.
///
/// 6 exactly, because 6 is the encoding's usable range:
/// `sdf_from_mask` writes `SDF_EDGE - distance / SDF_PX`, which reaches 0
/// six pixels outside the shape and cannot describe anything further. A
/// seventh pixel of margin would be atlas space holding a guaranteed zero.
const SDF_MARGIN: u32 = 6;

/// MapLibre's `fontScale` for a glyph from this atlas: how many physical
/// pixels one of its field texels is drawn as.
///
/// `SdfPaint::font_scale` wants exactly this, and it is division in the one
/// direction that is easy to get backwards -- hence a function rather than a
/// comment. `display_size_px` is the label's PHYSICAL pixel text size
/// (`feature.text_size * scale_factor`), which is the same quantity
/// `symbol_pass`'s `LabelShapeCache` keys on.
pub(crate) fn font_scale(display_size_px: f32) -> f32 {
    display_size_px / RASTER_SIZE_PX
}

/// Where one glyph's distance field sits in [`GlyphAtlas`]'s image.
///
/// # Units
///
/// Every number here is in ATLAS TEXELS at [`RASTER_SIZE_PX`], NOT in
/// display pixels -- that is the whole point of a size-independent field,
/// and it is the one thing a caller can get silently wrong. A quad is built
/// by multiplying `width`, `height` and both halves of `bearing` by
/// [`font_scale`] of the label's physical text size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct GlyphRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    /// Offset from the glyph's pen position to its bitmap's top-left, in
    /// atlas texels. Swash returns this as `placement.left`/`.top`, with
    /// [`SDF_MARGIN`] taken off each (the bitmap grew outwards by the
    /// margin, so its top-left moved left and up), and otherwise with
    /// swash's own signs unchanged: `left` is positive to the RIGHT of the
    /// pen, `top` is positive ABOVE the baseline. In the y-down physical
    /// screen space the rest of the symbol code uses, a quad's top-left is
    /// `(pen_x + bearing.0 * s, pen_y - bearing.1 * s)` for
    /// `s = font_scale(display_size_px)`.
    ///
    /// A quad positioned without it puts every glyph on its own baseline:
    /// the bitmap swash returns is cropped to the glyph's ink, so "S" and
    /// "y" come back as boxes of different heights whose tops sit at
    /// different distances above the pen. Place both boxes at the pen and
    /// the descender of the "y" rides up level with the cap of the "S" --
    /// text that spells the right letters and has no baseline.
    pub bearing: (f32, f32),
}

/// One rasterisable glyph: which face, which glyph in it, and the two
/// attributes that change its OUTLINE.
///
/// Deliberately no size. The atlas holds a distance field, which is
/// size-independent: one raster serves a label at 11 px and the same label
/// at 40 px, and a `text-size` zoom ramp costs nothing after the first
/// sighting. (The coverage version this replaced had to carry the size,
/// quantised to 1/64 px, and rastered again on every distinct one.)
///
/// `weight` and `flags` are here because cosmic-text's own `CacheKey`
/// carries them and they change the pixels: a VARIABLE font renders a
/// different outline per weight from the same `font_id` (see cosmic-text's
/// `swash_image`, which feeds `font_weight` into the `wght` axis), and
/// `FAKE_ITALIC` skews the outline. Keying on `(font_id, glyph_id)` alone
/// would serve the regular field for a bold label off the same variable
/// face.
///
/// What is deliberately NOT in the key is cosmic-text's subpixel binning
/// (`x_bin`/`y_bin`). glyphon quantises a glyph's fractional position into
/// four bins per axis and rasters up to 16 variants of it, which buys
/// crisper horizontal text at 16x the raster work. There is nothing to buy
/// here: the field is resampled by the quad's own scale and rotation
/// regardless, and a glyph on a rotated baseline lands at a fractional
/// offset no bin describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct GlyphKey {
    pub font_id: glyphon::fontdb::ID,
    pub glyph_id: u16,
    pub weight: glyphon::Weight,
    pub flags: glyphon::cosmic_text::CacheKeyFlags,
}

impl GlyphKey {
    /// The key for one shaped glyph.
    ///
    /// This is the constructor the draw loop wants: `LayoutGlyph` is what
    /// `buffer.layout_runs()`'s `run.glyphs` hands out. Note what is NOT
    /// read from it -- `glyph.font_size`. A field does not have a size, so
    /// the same glyph shaped at any size gives the same key, which is
    /// pinned by `two_text_sizes_share_one_raster`.
    pub(crate) fn for_glyph(glyph: &glyphon::LayoutGlyph) -> Self {
        Self::new(glyph.font_id, glyph.glyph_id)
            .with_weight(glyph.font_weight)
            .with_flags(glyph.cache_key_flags)
    }

    /// The key for `glyph_id` in `font_id`, regular weight, no flags.
    pub(crate) fn new(font_id: glyphon::fontdb::ID, glyph_id: u16) -> Self {
        Self {
            font_id,
            glyph_id,
            weight: glyphon::Weight::NORMAL,
            flags: glyphon::cosmic_text::CacheKeyFlags::empty(),
        }
    }

    pub(crate) fn with_weight(mut self, weight: glyphon::Weight) -> Self {
        self.weight = weight;
        self
    }

    pub(crate) fn with_flags(mut self, flags: glyphon::cosmic_text::CacheKeyFlags) -> Self {
        self.flags = flags;
        self
    }

    /// The cosmic-text key `SwashCache` is addressed by, at
    /// [`RASTER_SIZE_PX`].
    ///
    /// Two things are forced rather than taken from the glyph.
    /// `DISABLE_HINTING`, because a hinted outline is distorted to fit the
    /// 24 px grid and this one field is then drawn at every other size --
    /// Mapbox's glyph pipeline uses `FT_LOAD_NO_HINTING` for the same
    /// reason. And both subpixel bins are `Zero`; see the type's doc.
    ///
    /// Forcing the flag here rather than in the key would mean two keys
    /// mapping to one raster, so `with_flags` is what the caller sees and
    /// this is what swash sees; the difference is exactly the hinting bit.
    fn cache_key(&self) -> glyphon::CacheKey {
        glyphon::CacheKey {
            font_id: self.font_id,
            glyph_id: self.glyph_id,
            font_size_bits: RASTER_SIZE_PX.to_bits(),
            x_bin: glyphon::SubpixelBin::Zero,
            y_bin: glyphon::SubpixelBin::Zero,
            font_weight: self.weight,
            flags: self.flags | glyphon::cosmic_text::CacheKeyFlags::DISABLE_HINTING,
        }
    }
}

/// One shared RGBA image holding a distance field per glyph, plus the table
/// saying where each one is.
///
/// # Packing
///
/// The same shelf packer as [`crate::render::pack_icon_atlas`] (glyphs
/// placed left-to-right in rows, a new row started when the current one is
/// full, one transparent texel of padding between neighbours so the bilinear
/// sampler cannot bleed one glyph's field into the next one's edge), with
/// one difference: this atlas is filled INCREMENTALLY, a glyph at a time as
/// the draw loop asks for it, where the icon packer sees the whole set at
/// once and packs it in one pass.
///
/// That difference is why placement here is by ARRIVAL order rather than by
/// a sort. `pack_icon_atlas` sorts its names first, and its reason is worth
/// repeating because it is the invariant this module has to satisfy too: "a
/// `HashMap` iteration order would put the same sheet at different origins
/// on two runs -- harmless for drawing, fatal for a name-by-origin lookup."
/// We satisfy it differently. A glyph is never looked up by origin -- the
/// origin IS the answer, returned from [`GlyphAtlas::get_or_raster`] and
/// stored per key -- and the rect table is a `BTreeMap`, so every walk over
/// this atlas's contents is ordered by key rather than by hash. What remains
/// is that two runs agree on origins only if they ask for glyphs in the same
/// order; a caller that collects its glyph keys into a `HashSet` and iterates
/// that would get a differently-packed (but equally correct) atlas each run,
/// and would deserve the same sort the icon packer does. Pinned by
/// `two_atlases_fed_the_same_sequence_agree_on_every_origin`.
pub(crate) struct GlyphAtlas {
    /// Every key ever asked for, and where its field landed. `None` is a
    /// glyph this atlas will not draw -- a space, an empty outline, a
    /// colour emoji, a full atlas -- cached so that a string full of spaces
    /// does not re-enter swash once per space per frame.
    rects: BTreeMap<GlyphKey, Option<GlyphRect>>,
    /// RGBA, `ATLAS_WIDTH` wide, grown downwards. Alpha is DISTANCE, not
    /// coverage, in `sprite::sdf_from_mask`'s encoding. Starts zero-height
    /// and allocates nothing until the first glyph lands.
    image: image::RgbaImage,
    /// Shelf cursor, in atlas pixels.
    cursor_x: u32,
    cursor_y: u32,
    /// The tallest glyph on the current shelf, which is how far down the
    /// next shelf starts.
    row_height: u32,
    generation: u64,
    /// Set once the atlas has refused a glyph for want of height, so the
    /// warning is logged once rather than once per glyph per frame.
    warned_full: bool,
    /// The keys refused for want of height (a subset of the `None`s in
    /// `rects`), so a later HIT on one still counts as a refusal.
    refused_keys: std::collections::BTreeSet<GlyphKey>,
    /// Every time a glyph was asked for and not drawn because the atlas
    /// was full -- the first refusal and every hit on a refused key since.
    /// Monotonic; a caller diffs it around a frame. See
    /// [`Self::refused_for_space`].
    refused_for_space: u64,
    /// How tall the atlas may grow: [`MAX_ATLAS_HEIGHT`], lowered only by
    /// tests that need a full atlas without a thousand glyphs.
    max_height: u32,
    /// Opt-in ([`Self::keep_fields`]): every distance field ever made, by
    /// key (`None` = a glyph with no drawable ink), kept across
    /// [`Self::clear_layout`] so a caller that re-packs the atlas every
    /// frame pays the raster and the distance transform once per glyph,
    /// not once per frame. `None` for the live map, which never clears.
    field_cache: Option<BTreeMap<GlyphKey, Option<(image::RgbaImage, (f32, f32))>>>,
    /// The bytes of every field in `field_cache`, and the cap past which
    /// [`Self::clear_layout`] drops them all ([`FIELD_CACHE_MAX_BYTES`];
    /// lowered only by tests). A long multi-script story otherwise keeps
    /// one field per distinct glyph and size for the session's life.
    /// Dropping is safe for determinism: a field is a pure function of
    /// its key, so a re-made one is the same bytes.
    field_cache_bytes: usize,
    field_cache_max_bytes: usize,
    /// The per-frame rastering allowance, `None` = unbounded (the default,
    /// and what every caller but the native frame loop gets). See
    /// [`GlyphAtlas::begin_frame`].
    raster_budget: Option<std::time::Duration>,
    /// Rastering time spent since the last `begin_frame`.
    rastered_this_frame: std::time::Duration,
    /// Labels [`GlyphAtlas::admits`] turned away since the last
    /// `begin_frame`.
    deferred_labels: usize,
    /// Other new-label work this frame (shaping) that the allowance also
    /// pays for. See [`Self::charge`].
    charged_this_frame: std::time::Duration,
    /// Running mean cost of one new glyph (raster + distance transform),
    /// which `admits` uses to keep a long label from overshooting the
    /// allowance by its whole length.
    glyph_cost: std::time::Duration,
}

/// Matches `pack_icon_atlas`'s atlas width, for the same reason: wide enough
/// that the shelf packer wastes little on a row, narrow enough to stay well
/// inside every target's `max_texture_dimension_2d`.
const ATLAS_WIDTH: u32 = 1024;

/// One transparent texel between neighbours -- MapLibre's own atlas
/// padding, and `pack_icon_atlas`'s. This is padding BETWEEN glyphs and is
/// not [`SDF_MARGIN`], which is part of a glyph's own bitmap: the margin is
/// field the halo reads, this is a gap the sampler must not read across.
/// Flush packing lets one glyph's field appear as a sliver of halo-coloured
/// ink down the side of its neighbour -- the SDF failure mode
/// `pack_icon_atlas`'s own padding note describes, and rotation makes it
/// worse rather than better, since a rotated quad's edge never lands on a
/// texel boundary and so is always interpolating across the border.
const PAD: u32 = 1;

/// The tallest this atlas will grow. wgpu's default
/// `max_texture_dimension_2d` is 8192 and the iOS/WebGL2 limit work in this
/// crate has already shown how a limit nobody checked becomes a device that
/// draws nothing; 4096 leaves room to spare. With the size out of the key a
/// 1024x4096 atlas holds every glyph of every face a map is likely to load
/// -- roughly 925 fields at a padded 48 px (was ~3,700 at the 24 px this
/// atlas used before the module doc's "Why 48 px"; the atlas's own cap is
/// unchanged, so the WORST CASE resident texture is the same 16 MiB either
/// way, and only the number of distinct glyphs that fit under it shrank).
/// Past it `get_or_raster` answers `None` -- the glyph is not drawn --
/// rather than building a texture the device will refuse.
const MAX_ATLAS_HEIGHT: u32 = 4096;

/// How many bytes of distance fields [`GlyphAtlas::keep_fields`] keeps
/// before [`GlyphAtlas::clear_layout`] drops them all: 64 MiB, some
/// 20 000 glyphs at label sizes -- far more than any one frame asks for,
/// so a frame's own glyphs are re-made at most once per crossing.
pub(crate) const FIELD_CACHE_MAX_BYTES: usize = 64 << 20;

impl GlyphAtlas {
    pub(crate) fn new() -> Self {
        Self {
            rects: BTreeMap::new(),
            image: image::RgbaImage::new(ATLAS_WIDTH, 0),
            cursor_x: 0,
            cursor_y: 0,
            row_height: 0,
            generation: 0,
            warned_full: false,
            refused_keys: std::collections::BTreeSet::new(),
            refused_for_space: 0,
            max_height: MAX_ATLAS_HEIGHT,
            field_cache: None,
            field_cache_bytes: 0,
            field_cache_max_bytes: FIELD_CACHE_MAX_BYTES,
            raster_budget: None,
            rastered_this_frame: std::time::Duration::ZERO,
            deferred_labels: 0,
            charged_this_frame: std::time::Duration::ZERO,
            glyph_cost: std::time::Duration::ZERO,
        }
    }

    /// Starts a frame's rastering allowance. `budget` bounds the time this
    /// frame may spend making NEW glyphs (`sprite::sdf_from_mask` is
    /// O(pixels) per glyph, ~0.1 ms at 48 px); `None` lifts the bound.
    ///
    /// Why a budget at all: the first time the camera reaches a zoom whose
    /// road names use glyphs not yet in the atlas, every one of them is
    /// rastered in that one frame -- measured at 25-40 ms of
    /// `symbol_collect` on the first z12 frame of the block-groups sweep
    /// (2026-09-26), against under 1 ms of shaping. MapLibre never pays
    /// this on the render thread: glyphs arrive as pre-rendered SDF PBFs
    /// through its worker. Here the label that does not fit this frame's
    /// allowance is left for a later one -- see [`Self::admits`].
    pub(crate) fn begin_frame(&mut self, budget: Option<std::time::Duration>) {
        self.raster_budget = budget;
        self.rastered_this_frame = std::time::Duration::ZERO;
        self.deferred_labels = 0;
        self.charged_this_frame = std::time::Duration::ZERO;
    }

    /// Whether a label made of `keys` may be drawn this frame: always when
    /// every glyph is already resident (a hit costs nothing), and otherwise
    /// only while the frame's rastering allowance is not yet spent.
    ///
    /// Per LABEL, never per glyph: a label is admitted whole or deferred
    /// whole, so the budget never draws a street name with letters
    /// missing. A label is admitted when its new glyphs, at the running
    /// mean cost of one, fit what is left of the allowance -- or when it
    /// is the frame's first, so progress never stalls on a label longer
    /// than the whole allowance.
    pub(crate) fn admits(&mut self, keys: impl Iterator<Item = GlyphKey>) -> bool {
        let Some(budget) = self.raster_budget else { return true };
        let mut missing = keys.filter(|k| !self.rects.contains_key(k)).collect::<Vec<_>>();
        missing.sort_unstable();
        missing.dedup();
        if missing.is_empty() || self.rastered_this_frame.is_zero() {
            return true;
        }
        let estimate = self.glyph_cost * missing.len() as u32;
        if self.charged_this_frame + self.rastered_this_frame + estimate <= budget {
            return true;
        }
        self.deferred_labels += 1;
        false
    }

    /// Labels turned away by [`Self::admits`] since the last
    /// [`Self::begin_frame`]. Non-zero means the caller must draw another
    /// frame, or the deferred labels would wait for the next camera move.
    /// Charges `spent` -- other new-label work this frame did, shaping --
    /// against the allowance [`Self::admits`] grants rastering from, so the
    /// frame's NEW label work as a whole stays inside the budget. It never
    /// defers anything by itself, and a frame's first rastering label is
    /// still admitted, so progress cannot stall behind shaping.
    pub(crate) fn charge(&mut self, spent: std::time::Duration) {
        self.charged_this_frame += spent;
    }

    /// Time spent rastering new glyphs since the last [`Self::begin_frame`].
    #[allow(dead_code)]
    pub(crate) fn rastered_ns(&self) -> u64 {
        self.rastered_this_frame.as_nanos() as u64
    }

    pub(crate) fn deferred_labels(&self) -> usize {
        self.deferred_labels
    }

    /// The rect for `key`, rastering and distance-transforming it on a miss.
    ///
    /// `None` means this atlas will not draw this glyph, and the answer is
    /// cached so a miss costs one swash call plus one distance transform and
    /// a hit costs a `BTreeMap` lookup. Three things answer `None`:
    /// a glyph with no ink (every space in every label); a COLOUR glyph,
    /// because a distance field is a silhouette and running an emoji
    /// through it would draw a blob in the text colour rather than the
    /// emoji; and a full atlas.
    ///
    /// `swash` is the caller's own `SwashCache` -- `map_view`'s
    /// `glyphon_swash_cache`, the one glyphon's `prepare` is already given.
    /// A second cache would double both the rasterisation work and the
    /// resident bitmap memory. Note that the rasters this asks for are NOT
    /// the ones glyphon asks for (fixed 24 px, hinting off, bin zero), so
    /// the two uses share the cache without sharing entries.
    pub(crate) fn get_or_raster(
        &mut self,
        swash: &mut glyphon::SwashCache,
        font_system: &mut glyphon::FontSystem,
        key: GlyphKey,
    ) -> Option<GlyphRect> {
        if let Some(cached) = self.rects.get(&key) {
            // A HIT returns without touching `generation`. A caller
            // re-uploads the texture when the generation moves, so bumping
            // it here would make every frame re-upload the whole atlas and
            // undo the entire point of caching the raster.
            if cached.is_none() && self.refused_keys.contains(&key) {
                self.refused_for_space += 1;
            }
            return *cached;
        }
        let started = crate::timing::Instant::now();
        let rect = self.raster_miss(swash, font_system, key);
        let cost = started.elapsed();
        self.rastered_this_frame += cost;
        // An exponential mean, 1/8 weight: tracks the face and size mix of
        // the labels actually on screen without one outlier moving it.
        self.glyph_cost = if self.glyph_cost.is_zero() { cost } else { (self.glyph_cost * 7 + cost) / 8 };
        rect
    }

    /// `get_or_raster`'s miss: raster, distance-transform and place one
    /// glyph.
    fn raster_miss(
        &mut self,
        swash: &mut glyphon::SwashCache,
        font_system: &mut glyphon::FontSystem,
        key: GlyphKey,
    ) -> Option<GlyphRect> {
        let cached = self.field_cache.as_ref().and_then(|cache| cache.get(&key)).cloned();
        let made = match cached {
            Some(made) => made,
            None => {
                let rastered = swash.get_image(font_system, key.cache_key()).as_ref().and_then(|image| {
                    let mask = glyph_mask(image)?;
                    // Swash crops to the ink and the mask grew by the margin on
                    // every side, so the bitmap's top-left moved out by exactly
                    // that much: left by `SDF_MARGIN`, and UP by it, which in
                    // swash's above-the-baseline-is-positive convention is `+`.
                    let bearing = (
                        image.placement.left as f32 - SDF_MARGIN as f32,
                        image.placement.top as f32 + SDF_MARGIN as f32,
                    );
                    Some((mask, bearing))
                });
                // The one line this whole reversal turns on: the crate's
                // existing SDF generator, the same one that builds SDF
                // sprites, in the same encoding `icon_shader.wgsl` already
                // decodes. Nothing here implements a distance transform.
                let made = rastered.map(|(mask, bearing)| (crate::sprite::sdf_from_mask(&mask), bearing));
                if let Some(cache) = self.field_cache.as_mut() {
                    self.field_cache_bytes += made.as_ref().map_or(0, |(f, _)| f.as_raw().len()) + std::mem::size_of::<GlyphKey>();
                    cache.insert(key, made.clone());
                }
                made
            }
        };
        let Some((field, bearing)) = made else {
            self.rects.insert(key, None);
            return None;
        };
        let (w, h) = field.dimensions();

        let Some((x, y)) = self.allocate(w, h) else {
            // Out of atlas. Cached as `None` so a full atlas does not
            // re-raster the same glyph every frame forever, which also
            // means the refusal is permanent for this glyph -- acceptable
            // at 4096 rows, and loud in the log the first time. Counted, so
            // a caller can tell (`refused_for_space`).
            self.rects.insert(key, None);
            self.refused_keys.insert(key);
            self.refused_for_space += 1;
            return None;
        };
        for row in 0..h {
            for col in 0..w {
                self.image.put_pixel(x + col, y + row, *field.get_pixel(col, row));
            }
        }
        // A MISS that wrote pixels is exactly when the texture on the GPU
        // has gone stale, so this is the only place the generation moves.
        self.generation += 1;
        let rect = GlyphRect { x, y, width: w, height: h, bearing };
        self.rects.insert(key, Some(rect));
        Some(rect)
    }

    /// Reserves a `w` x `h` shelf slot, growing the image if the shelf it
    /// lands on is below the allocated height. `None` once the atlas would
    /// pass [`MAX_ATLAS_HEIGHT`].
    fn allocate(&mut self, w: u32, h: u32) -> Option<(u32, u32)> {
        // A glyph wider than the whole atlas cannot be placed on any shelf.
        // Not reachable at a 24 px raster, but the loop below would
        // otherwise place it at x = 0 and write past the row.
        if w > ATLAS_WIDTH {
            return None;
        }
        if self.cursor_x + w > ATLAS_WIDTH {
            self.cursor_x = 0;
            self.cursor_y += self.row_height + PAD;
            self.row_height = 0;
        }
        let needed_height = self.cursor_y + h;
        if needed_height > self.max_height {
            if !self.warned_full {
                self.warned_full = true;
                eprintln!(
                    "gpui-maps: glyph atlas full at {ATLAS_WIDTH}x{}; \
                     further line-placed glyphs will not be drawn",
                    self.max_height
                );
            }
            return None;
        }
        if needed_height > self.image.height() {
            // Grow in POWER-OF-TWO steps rather than to exactly what this
            // glyph needs: growing reallocates and copies the whole image,
            // and a label's worth of glyphs arriving one at a time would
            // otherwise do that once per glyph.
            let mut new_height = self.image.height().max(256);
            while new_height < needed_height {
                new_height = new_height.saturating_mul(2);
            }
            let new_height = new_height.min(self.max_height);
            let mut grown = image::RgbaImage::new(ATLAS_WIDTH, new_height);
            // The width is unchanged, so the old rows are a prefix of the
            // new buffer in row-major order -- no per-pixel copy needed,
            // and every existing `GlyphRect` stays valid.
            let old = self.image.as_raw();
            let grown_bytes: &mut [u8] = &mut grown;
            grown_bytes[..old.len()].copy_from_slice(old);
            self.image = grown;
        }
        let placement = (self.cursor_x, self.cursor_y);
        self.cursor_x += w + PAD;
        self.row_height = self.row_height.max(h);
        Some(placement)
    }

    /// The atlas bytes, RGBA8, and its width and height in pixels. Alpha is
    /// DISTANCE in `sprite::sdf_from_mask`'s encoding, not coverage.
    ///
    /// Height is 0 until the first glyph lands. `IconPipeline::upload_atlas`
    /// clamps both dimensions to 1, so an empty atlas uploads as a 1x1
    /// transparent texture rather than failing.
    pub(crate) fn image(&self) -> (&[u8], u32, u32) {
        (self.image.as_raw(), self.image.width(), self.image.height())
    }

    /// The same image as an `RgbaImage`, which is what
    /// `IconPipeline::upload_atlas` takes -- so the glyph atlas uploads
    /// through the icon atlas's existing code path with no new pipeline.
    pub(crate) fn rgba(&self) -> &image::RgbaImage {
        &self.image
    }

    /// Bumped every time a field is added to the image, and at no other
    /// time. A caller uploads the texture when this differs from the
    /// generation it last uploaded.
    ///
    /// This exists because a growing atlas that never signals is how a
    /// label draws as a blank quad: the rect is correct, the UVs are
    /// correct, and the texture on the GPU is the one from before the glyph
    /// was rastered.
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// How many glyph requests this atlas has turned away for want of
    /// room, ever: the refusal that filled it and every later request for
    /// a refused key. A caller diffs it around a frame to learn whether
    /// that frame drew a label with letters missing.
    pub(crate) fn refused_for_space(&self) -> u64 {
        self.refused_for_space
    }

    /// Keep every distance field made from now on, across
    /// [`Self::clear_layout`]. For a caller that re-packs every frame.
    pub(crate) fn keep_fields(&mut self) {
        self.field_cache.get_or_insert_with(BTreeMap::new);
    }

    /// Empties the atlas's LAYOUT -- no glyph placed, no refusal cached,
    /// zero height -- keeping the distance fields [`Self::keep_fields`]
    /// kept. The next glyphs pack from the top-left in the order they are
    /// asked for, exactly as in a new atlas, so the texture a frame draws
    /// from depends on that frame alone. The generation moves on (never
    /// back), so a caller's cached upload is stale.
    ///
    /// Also where the kept fields are bounded: past
    /// [`FIELD_CACHE_MAX_BYTES`] they are all dropped here, between
    /// frames, and re-made on demand (the same bytes: a field depends on
    /// its key alone).
    pub(crate) fn clear_layout(&mut self) {
        if self.field_cache_bytes > self.field_cache_max_bytes {
            if let Some(cache) = self.field_cache.as_mut() {
                cache.clear();
            }
            self.field_cache_bytes = 0;
        }
        self.rects.clear();
        self.refused_keys.clear();
        self.image = image::RgbaImage::new(ATLAS_WIDTH, 0);
        self.cursor_x = 0;
        self.cursor_y = 0;
        self.row_height = 0;
        self.generation += 1;
    }

    /// Caps the atlas's height below [`MAX_ATLAS_HEIGHT`] (never above
    /// it). For tests that need a full atlas; applied to an empty atlas.
    pub(crate) fn set_max_height(&mut self, rows: u32) {
        self.max_height = rows.min(MAX_ATLAS_HEIGHT);
    }

    /// Every key and where it landed, in key order: tests compare two
    /// atlases' layouts with it.
    /// How many distance fields [`Self::keep_fields`] holds.
    #[cfg(test)]
    pub(crate) fn fields_kept(&self) -> usize {
        self.field_cache.as_ref().map_or(0, BTreeMap::len)
    }

    /// Lowers the kept-fields cap, for the test that crosses it.
    #[cfg(test)]
    pub(crate) fn set_field_cache_max_bytes(&mut self, bytes: usize) {
        self.field_cache_max_bytes = bytes;
    }

    #[cfg(test)]
    pub(crate) fn layout(&self) -> Vec<(GlyphKey, Option<GlyphRect>)> {
        self.rects.iter().map(|(k, r)| (*k, *r)).collect()
    }
}

impl Default for GlyphAtlas {
    fn default() -> Self {
        Self::new()
    }
}

/// One glyph's ink as a binary-ready `GrayImage`, padded by [`SDF_MARGIN`]
/// on every side so the distance transform has room to run outward.
///
/// `None` for a glyph this atlas will not hold: no ink at all, or a COLOUR
/// bitmap (see [`GlyphAtlas::get_or_raster`]).
fn glyph_mask(image: &glyphon::SwashImage) -> Option<image::GrayImage> {
    let (ink_w, ink_h) = (image.placement.width, image.placement.height);
    // A zero-extent placement is how swash reports "this glyph has no ink"
    // -- every space in every label arrives here.
    if ink_w == 0 || ink_h == 0 {
        return None;
    }
    match image.content {
        glyphon::SwashContent::Mask => {}
        // A colour bitmap or colour outline -- an emoji. A distance field
        // has one channel and would keep only the silhouette, so the glyph
        // is dropped rather than drawn as a blob in the text colour. If
        // line-placed emoji ever matter, the answer is a second, non-SDF
        // glyph atlas, not a silhouette.
        glyphon::SwashContent::Color => return None,
        // cosmic-text asks for `Format::Alpha` and logs a TODO if it ever
        // sees this, so it is unreachable through `SwashCache`. Skipped
        // rather than guessed at: misreading a subpixel mask as coverage
        // would put colour fringes into the mask as ink.
        glyphon::SwashContent::SubpixelMask => return None,
    }
    let count = (ink_w as usize) * (ink_h as usize);
    if image.data.len() < count {
        // Not reachable from swash, which sizes `data` from `placement`
        // itself. Answering `None` rather than indexing keeps a short
        // buffer from panicking the paint.
        eprintln!("gpui-maps: glyph mask shorter than its placement; skipping");
        return None;
    }
    let mut mask = image::GrayImage::new(ink_w + 2 * SDF_MARGIN, ink_h + 2 * SDF_MARGIN);
    for row in 0..ink_h {
        for col in 0..ink_w {
            let coverage = image.data[(row * ink_w + col) as usize];
            mask.put_pixel(col + SDF_MARGIN, row + SDF_MARGIN, image::Luma([coverage]));
        }
    }
    Some(mask)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::icon_pipeline::{SDF_EDGE, SDF_HALO_BASE, SDF_PX};

    /// A `FontSystem` plus the `SwashCache` that rasters from it.
    ///
    /// `FontSystem::new()` enumerates the system's fonts and is expensive,
    /// so each test builds one and reuses it for every glyph it needs --
    /// the same discipline `map_view`'s shaping tests follow.
    fn test_font_system() -> (glyphon::FontSystem, glyphon::SwashCache) {
        (glyphon::FontSystem::new(), glyphon::SwashCache::new())
    }

    /// The key for the nth glyph `text` shapes to at `size_px`.
    ///
    /// Going through a real `Buffer` rather than hand-picking a glyph id is
    /// the point: this is the same `run.glyphs` the draw loop will read, so
    /// the test proves a [`GlyphKey`] is derivable from what shaping
    /// actually hands out. `size_px` affects the SHAPING, not the key --
    /// which is what `two_text_sizes_share_one_raster` is about.
    fn nth_glyph_of(text: &str, size_px: f32, n: usize, fs: &mut glyphon::FontSystem) -> GlyphKey {
        let mut buffer = glyphon::Buffer::new(fs, glyphon::Metrics::new(size_px, size_px * 1.2));
        buffer.set_text(
            text,
            &glyphon::Attrs::new().family(glyphon::Family::SansSerif),
            glyphon::Shaping::Advanced,
            None,
        );
        buffer.shape_until_scroll(fs, false);
        let run = buffer.layout_runs().next().expect("a shaped run");
        let glyph = run.glyphs.get(n).expect("a shaped glyph at that index");
        GlyphKey::for_glyph(glyph)
    }

    fn first_glyph_of(text: &str, size_px: f32, fs: &mut glyphon::FontSystem) -> GlyphKey {
        nth_glyph_of(text, size_px, 0, fs)
    }

    #[test]
    fn a_spent_raster_budget_defers_a_label_needing_new_glyphs_but_not_a_resident_one() {
        let mut atlas = GlyphAtlas::new();
        let (mut fs, mut swash) = test_font_system();
        let resident = first_glyph_of("S", 12.0, &mut fs);
        let fresh = first_glyph_of("W", 12.0, &mut fs);
        // Unbounded (the default): everything is admitted.
        assert!(atlas.admits([fresh].into_iter()));
        atlas.get_or_raster(&mut swash, &mut fs, resident).expect("rasters");

        // The frame's first label is always admitted, so a label longer
        // than the whole allowance cannot stall forever.
        atlas.begin_frame(Some(std::time::Duration::ZERO));
        assert!(atlas.admits([resident, fresh].into_iter()));
        // Once anything has been rastered, a spent allowance defers the
        // label needing a new glyph, WHOLE -- even beside a resident
        // glyph -- and counts it.
        let other = first_glyph_of("K", 12.0, &mut fs);
        atlas.get_or_raster(&mut swash, &mut fs, other).expect("rasters");
        assert!(!atlas.admits([resident, fresh].into_iter()));
        assert_eq!(atlas.deferred_labels(), 1);
        // A label whose glyphs are all resident costs nothing and is drawn.
        assert!(atlas.admits([resident].into_iter()));
        assert_eq!(atlas.deferred_labels(), 1);

        // A new frame resets the count; lifting the bound admits it again.
        atlas.begin_frame(None);
        assert_eq!(atlas.deferred_labels(), 0);
        assert!(atlas.admits([fresh].into_iter()));
    }

    #[test]
    fn shaping_charged_to_the_frame_shrinks_what_rastering_may_spend() {
        let mut atlas = GlyphAtlas::new();
        let (mut fs, mut swash) = test_font_system();
        let first = first_glyph_of("K", 12.0, &mut fs);
        let fresh = first_glyph_of("W", 12.0, &mut fs);
        atlas.begin_frame(Some(std::time::Duration::from_secs(60)));
        atlas.get_or_raster(&mut swash, &mut fs, first).expect("rasters");
        // A minute of allowance admits the next label...
        assert!(atlas.admits([fresh].into_iter()));
        // ...until shaping has spent it.
        atlas.charge(std::time::Duration::from_secs(60));
        assert!(!atlas.admits([fresh].into_iter()));
        // A charge alone never blocks the frame's first rastering label.
        atlas.begin_frame(Some(std::time::Duration::ZERO));
        atlas.charge(std::time::Duration::from_secs(60));
        assert!(atlas.admits([fresh].into_iter()));
    }

    #[test]
    fn a_repeated_glyph_is_rastered_once_and_shares_its_rect() {
        let mut atlas = GlyphAtlas::new();
        let (mut fs, mut swash) = test_font_system();
        let key = first_glyph_of("Shibuya", 12.0, &mut fs);
        let a = atlas.get_or_raster(&mut swash, &mut fs, key).expect("a visible glyph rasters");
        let gen_after_first = atlas.generation();
        let b = atlas.get_or_raster(&mut swash, &mut fs, key).expect("still there");
        assert_eq!(a, b, "the same key must return the same rect");
        assert_eq!(
            atlas.generation(),
            gen_after_first,
            "a cache HIT must not bump the generation -- a caller that re-uploads \
             the texture every frame because of this would undo the whole point"
        );
    }

    #[test]
    fn two_text_sizes_share_one_raster() {
        // THE property the distance field buys, and the reason the size left
        // `GlyphKey`. The coverage version this replaced answered two
        // different rects here and rastered twice; a `text-size` zoom ramp
        // did that at every stop it crossed.
        let mut atlas = GlyphAtlas::new();
        let (mut fs, mut swash) = test_font_system();
        let small = first_glyph_of("Shibuya", 11.0, &mut fs);
        let large = first_glyph_of("Shibuya", 40.0, &mut fs);
        assert_eq!(small, large, "a field has no size, so neither does its key");

        let small_rect = atlas.get_or_raster(&mut swash, &mut fs, small).expect("11 px rasters");
        assert_eq!(atlas.generation(), 1);
        let large_rect = atlas.get_or_raster(&mut swash, &mut fs, large).expect("40 px hits");
        assert_eq!(small_rect, large_rect, "both sizes must sample the same field");
        assert_eq!(atlas.generation(), 1, "the second size must not raster again");
    }

    #[test]
    fn font_scale_is_maplibres_and_divides_the_right_way_round() {
        // `SdfPaint::font_scale` is physical pixels per field texel. Getting
        // this upside down does not fail loudly -- it gives an edge that
        // softens with zoom and a halo that grows with it, which reads as
        // "the SDF maths is subtly wrong" rather than as a unit error.
        assert_eq!(font_scale(RASTER_SIZE_PX), 1.0, "drawn at the raster size, one texel is one pixel");
        assert_eq!(font_scale(RASTER_SIZE_PX / 2.0), 0.5, "drawn half size, a texel is half a pixel");
        assert_eq!(font_scale(RASTER_SIZE_PX * 2.0), 2.0);
    }

    /// The measured regression this atlas's raster size was raised for: a
    /// real `road-label` (`examples/mapbox-style.json`, text-size
    /// interpolated to ~13.17 logical px at Singapore z17) must not be
    /// MAGNIFIED on a 3x display the way it was at `RASTER_SIZE_PX = 24`
    /// (where this same computation gave `font_scale` = 1.65 -- see the
    /// module doc's "Why 48 px").
    #[test]
    fn a_real_road_label_is_no_longer_magnified_on_a_3x_display() {
        let road_label_text_size_at_z17 = 13.166_666_f32;
        let scale_factor_3x = 3.0f32;
        let physical_text_size = road_label_text_size_at_z17 * scale_factor_3x;
        let scale = font_scale(physical_text_size);
        assert!(
            scale <= 1.0,
            "font_scale {scale} on a 3x display must not exceed 1.0 -- above 1.0 the quad draws \
             MORE physical pixels than the field has texels for, which is the magnification the \
             bug report saw as soft, aliased street names"
        );
        // Before this fix (RASTER_SIZE_PX = 24) this same computation gave
        // 1.65 -- true magnification. 0.82 is still LARGER than the 1x
        // build's own 0.27 (a denser display legitimately samples the field
        // more finely for the same logical size), which is expected and not
        // what this test is about; the regression this test guards is
        // crossing 1.0, not moving at all.
    }

    /// The other half of "Why 48 px": doubling the raster halves headroom
    /// too, and this is the REAL style value ([`RASTER_SIZE_PX`] = 48 was
    /// chosen against it) rather than the older test's illustrative one.
    /// `road-label-minor` (`examples/mapbox-style.json`) is 9 logical px at
    /// its own `minzoom` and asks for a 1 px halo -- the smallest real
    /// `text-size` any `map`-aligned layer in either vendored style reaches,
    /// which by the module doc's cancellation is the binding case at EVERY
    /// scale factor, not just the one asserted here.
    #[test]
    fn the_smallest_real_road_label_keeps_halo_headroom_at_the_new_raster_size() {
        let text_size = 9.0f32;
        let halo_width_pt = 1.0f32;
        for scale_factor in [1.0f32, 2.0, 3.0] {
            let physical_text_size = text_size * scale_factor;
            let scale = font_scale(physical_text_size);
            let reach_physical = SDF_HALO_BASE * scale;
            let halo_width_physical = halo_width_pt * scale_factor;
            assert!(
                reach_physical >= halo_width_physical,
                "at scale factor {scale_factor}, the encoding's {SDF_HALO_BASE} usable texels reach \
                 {reach_physical} physical px, short of the {halo_width_physical} px \
                 road-label-minor's own halo asks for"
            );
        }
    }

    #[test]
    fn the_margin_is_exactly_the_halo_the_style_asks_for() {
        // The arithmetic behind `RASTER_SIZE_PX` and `SDF_MARGIN = 6`, run
        // against an illustrative 1.4-logical-point halo on a 2x display --
        // deliberately a harder case than either vendored style's own
        // widest (1 px; see `the_smallest_real_road_label_keeps_halo_
        // headroom_at_the_new_raster_size` for the real value the current
        // `RASTER_SIZE_PX = 48` was actually chosen against). If a future
        // change raises the raster size, THIS is the test that catches the
        // halo being clipped rather than a screenshot review.
        let halo_width_physical = 1.4 * 2.0;
        // A 12-point label on a 2x display.
        let display_size_px = 12.0 * 2.0;
        let scale = font_scale(display_size_px);
        let reach_physical = SDF_HALO_BASE * scale;
        assert!(
            reach_physical >= halo_width_physical,
            "the encoding's {SDF_HALO_BASE} usable texels reach {reach_physical} physical px, \
             short of the {halo_width_physical} px the style asks for"
        );
        // And the same thing from `sdf_thresholds`' side: the halo
        // threshold must stay above zero, or its clamp saturates and the
        // halo silently stops widening.
        let buffer = (SDF_HALO_BASE - halo_width_physical / scale) / SDF_PX;
        assert!(buffer > 0.0, "the halo threshold saturated at {buffer}");
        assert_eq!(SDF_MARGIN as f32, SDF_HALO_BASE, "the bitmap margin is the encoding's own range");
    }

    #[test]
    fn a_rastered_glyph_is_a_distance_field_not_coverage() {
        // Pins the thing the whole reversal is about. Inside the letter the
        // alpha must exceed `SDF_EDGE` (which is what the shader thresholds
        // the fill against), outside it must fall below, and out at the
        // margin's own edge it must have bottomed out at zero -- which is
        // what proves the field has room to run and the halo will not be
        // clipped by the bitmap.
        let mut atlas = GlyphAtlas::new();
        let (mut fs, mut swash) = test_font_system();
        let key = first_glyph_of("Shibuya", 12.0, &mut fs);
        let rect = atlas.get_or_raster(&mut swash, &mut fs, key).expect("rasters");
        let (bytes, width, height) = atlas.image();
        assert!(height >= rect.y + rect.height, "the image must contain the rect it reported");

        let at = |col: u32, row: u32| bytes[(((rect.y + row) * width + rect.x + col) * 4) as usize + 3];
        let edge = (SDF_EDGE * 255.0).round() as u8;

        let mut inside = 0usize;
        let mut in_between = 0usize;
        for row in 0..rect.height {
            for col in 0..rect.width {
                let alpha = at(col, row);
                if alpha > edge {
                    inside += 1;
                } else if alpha > 0 {
                    in_between += 1;
                }
                // Every inked texel is white; the shader takes the colour
                // from the quad, and white is what `sdf_from_mask` writes.
                let i = (((rect.y + row) * width + rect.x + col) * 4) as usize;
                assert_eq!([bytes[i], bytes[i + 1], bytes[i + 2]], [255, 255, 255]);
            }
        }
        assert!(inside > 0, "a letter must have an interior above SDF_EDGE");
        assert!(
            in_between > inside,
            "a FIELD has a wide gradient outside the shape, not a one-texel coverage ramp: \
             {in_between} graded texels against {inside} interior"
        );

        // The bitmap's four corners are `SDF_MARGIN` from the ink on both
        // axes, so the field has bottomed out there.
        for (col, row) in [(0, 0), (rect.width - 1, 0), (0, rect.height - 1), (rect.width - 1, rect.height - 1)] {
            assert_eq!(at(col, row), 0, "the corner {col},{row} should be past the field's range");
        }
    }

    #[test]
    fn a_new_glyph_bumps_the_generation_and_lands_somewhere_else() {
        // The other half of the generation contract: a raster that changed
        // the image MUST signal, or the label draws as a blank quad from a
        // texture uploaded before the glyph existed.
        let mut atlas = GlyphAtlas::new();
        let (mut fs, mut swash) = test_font_system();
        assert_eq!(atlas.generation(), 0, "a fresh atlas has uploaded nothing");

        let s = first_glyph_of("Shibuya", 12.0, &mut fs);
        let h = nth_glyph_of("Shibuya", 12.0, 1, &mut fs);
        assert_ne!(s.glyph_id, h.glyph_id, "'S' and 'h' must be different glyphs");

        let s_rect = atlas.get_or_raster(&mut swash, &mut fs, s).expect("'S' rasters");
        assert_eq!(atlas.generation(), 1);
        let h_rect = atlas.get_or_raster(&mut swash, &mut fs, h).expect("'h' rasters");
        assert_eq!(atlas.generation(), 2, "a second distinct raster must signal again");
        assert_ne!(
            (s_rect.x, s_rect.y),
            (h_rect.x, h_rect.y),
            "two glyphs must not be packed on top of each other"
        );
        // PAD is what keeps the bilinear sampler from dragging one glyph's
        // field into the other's edge, and a rotated quad samples across
        // that edge on every single fragment.
        assert!(
            h_rect.x >= s_rect.x + s_rect.width + PAD,
            "the second glyph must start at least one padding texel past the first: \
             {h_rect:?} against {s_rect:?}"
        );
    }

    #[test]
    fn a_space_has_no_bitmap_and_is_answered_once() {
        // Every label with two words hits this path. `None` has to be
        // CACHED: re-entering swash and the distance transform for each
        // space on each frame is the cost this whole module exists to
        // avoid.
        let mut atlas = GlyphAtlas::new();
        let (mut fs, mut swash) = test_font_system();
        let space = nth_glyph_of("a b", 12.0, 1, &mut fs);

        assert!(atlas.get_or_raster(&mut swash, &mut fs, space).is_none(), "a space has no ink");
        assert_eq!(atlas.generation(), 0, "a glyph with no bitmap changed no pixels");
        assert!(atlas.get_or_raster(&mut swash, &mut fs, space).is_none(), "still none");
        assert_eq!(atlas.generation(), 0);
    }

    #[test]
    fn bearing_is_swashs_own_plus_the_margin_and_puts_two_letters_on_one_baseline() {
        // Why `bearing` is on the rect at all, and why the margin has to be
        // taken off it. Swash crops each bitmap to the glyph's ink, so 'S'
        // and 'y' come back as boxes of different heights sitting at
        // different distances above the pen; `top` is that distance. The
        // bitmap then grows outward by `SDF_MARGIN` on every side, so a
        // bearing that did not account for it would shift every glyph six
        // texels right and six down.
        let (mut fs, mut swash) = test_font_system();
        let mut atlas = GlyphAtlas::new();
        let cap = first_glyph_of("Sy", 40.0, &mut fs);
        let descender = nth_glyph_of("Sy", 40.0, 1, &mut fs);

        let cap_rect = atlas.get_or_raster(&mut swash, &mut fs, cap).expect("'S' rasters");
        let desc_rect = atlas.get_or_raster(&mut swash, &mut fs, descender).expect("'y' rasters");

        // y-down screen space, both glyphs at the same pen, drawn at the
        // raster size so `font_scale` is 1 and texels are pixels.
        let pen_y = 0.0f32;
        let margin = SDF_MARGIN as f32;
        // Back out the margin to recover where the INK sits, which is what
        // a baseline is about.
        let cap_ink_top = pen_y - cap_rect.bearing.1 + margin;
        let cap_ink_bottom = cap_ink_top + (cap_rect.height as f32 - 2.0 * margin);
        let desc_ink_top = pen_y - desc_rect.bearing.1 + margin;
        let desc_ink_bottom = desc_ink_top + (desc_rect.height as f32 - 2.0 * margin);

        assert!(
            cap_ink_top < desc_ink_top,
            "the cap of 'S' must start higher than the x-height of 'y': {cap_rect:?} against {desc_rect:?}"
        );
        assert!(
            desc_ink_bottom > cap_ink_bottom,
            "'y' must descend BELOW the baseline 'S' sits on: {desc_ink_bottom} against {cap_ink_bottom}"
        );
        // 2 px of slack, not zero: 'S' is a round letter and every type
        // designer gives those a little overshoot past the baseline so they
        // do not read as short beside a flat-bottomed letter.
        assert!(
            cap_ink_bottom <= pen_y + 2.0,
            "'S' must sit on the baseline, not below it: bottom {cap_ink_bottom} against pen {pen_y}"
        );
    }

    #[test]
    fn two_atlases_fed_the_same_sequence_agree_on_every_origin() {
        // The determinism discipline `pack_icon_atlas` sorts for. Its
        // reason applies here verbatim -- "a `HashMap` iteration order
        // would put the same sheet at different origins on two runs --
        // harmless for drawing, fatal for a name-by-origin lookup" -- and
        // this atlas answers it by being a pure function of the sequence of
        // misses, with the rect table a `BTreeMap` so no walk over it is
        // hash-ordered. This test is what would fail if a future change
        // made placement depend on hash iteration.
        let (mut fs, mut swash) = test_font_system();
        let keys: Vec<GlyphKey> =
            (0..5).map(|n| nth_glyph_of("Shibuya", 13.0, n, &mut fs)).collect();

        let pack = |fs: &mut glyphon::FontSystem, swash: &mut glyphon::SwashCache| {
            let mut atlas = GlyphAtlas::new();
            let rects: Vec<Option<GlyphRect>> =
                keys.iter().map(|k| atlas.get_or_raster(swash, fs, *k)).collect();
            let (_, w, h) = atlas.image();
            (rects, w, h, atlas.generation())
        };

        let first = pack(&mut fs, &mut swash);
        let second = pack(&mut fs, &mut swash);
        assert_eq!(first, second, "the same call sequence must pack to the same atlas");
        assert!(
            first.0.iter().any(|r| r.is_some()),
            "the sequence has to actually pack something for this to mean anything"
        );
    }

    #[test]
    fn a_cleared_layout_packs_like_a_new_atlas_and_keeps_its_fields() {
        // What the capture relies on for range independence: after
        // `clear_layout`, the glyphs a frame asks for land exactly where a
        // new atlas would put them -- while an uncleared atlas, warmed on
        // other glyphs first, puts them elsewhere.
        let (mut fs, mut swash) = test_font_system();
        let warm_keys: Vec<GlyphKey> = (0..6).map(|n| nth_glyph_of("Orchard", 13.0, n, &mut fs)).collect();
        let keys: Vec<GlyphKey> = (0..5).map(|n| nth_glyph_of("Shibuya", 13.0, n, &mut fs)).collect();
        let place = |atlas: &mut GlyphAtlas, fs: &mut glyphon::FontSystem, swash: &mut glyphon::SwashCache| {
            keys.iter().map(|k| atlas.get_or_raster(swash, fs, *k)).collect::<Vec<_>>()
        };
        let fresh = place(&mut GlyphAtlas::new(), &mut fs, &mut swash);

        let mut warm = GlyphAtlas::new();
        warm.keep_fields();
        for k in &warm_keys {
            warm.get_or_raster(&mut swash, &mut fs, *k);
        }
        let mut uncleared = GlyphAtlas::new();
        for k in &warm_keys {
            uncleared.get_or_raster(&mut swash, &mut fs, *k);
        }
        assert_ne!(place(&mut uncleared, &mut fs, &mut swash), fresh, "a warmed atlas packs elsewhere");

        let generation = warm.generation();
        warm.clear_layout();
        assert!(warm.generation() > generation, "a cleared atlas must be re-uploaded");
        assert_eq!(warm.image().2, 0);
        assert_eq!(place(&mut warm, &mut fs, &mut swash), fresh);
        let distinct: std::collections::BTreeSet<_> = warm_keys.iter().chain(&keys).collect();
        assert_eq!(warm.fields_kept(), distinct.len(), "every field made, warm or not, is kept");
    }

    /// Final review F9: the kept fields are bounded. Past the cap,
    /// `clear_layout` drops them, and the next frame packs and draws the
    /// same bytes as before (a field depends on its key alone).
    #[test]
    fn kept_fields_are_dropped_past_the_cap_and_the_frame_is_unchanged() {
        let (mut fs, mut swash) = test_font_system();
        let keys: Vec<GlyphKey> = (0..7).map(|n| nth_glyph_of("Marina Bay", 13.0, n, &mut fs)).collect();
        let frame = |atlas: &mut GlyphAtlas, fs: &mut glyphon::FontSystem, swash: &mut glyphon::SwashCache| {
            atlas.clear_layout();
            let rects: Vec<_> = keys.iter().map(|k| atlas.get_or_raster(swash, fs, *k)).collect();
            (rects, atlas.image().0.to_vec())
        };
        let mut atlas = GlyphAtlas::new();
        atlas.keep_fields();
        atlas.set_field_cache_max_bytes(1);
        let first = frame(&mut atlas, &mut fs, &mut swash);
        assert!(atlas.fields_kept() > 0, "the frame's fields are kept until the next clear");
        let second = frame(&mut atlas, &mut fs, &mut swash);
        assert_eq!(first, second, "a frame after the drop is the same bytes");
        // The drop happened at the second clear, and this frame refilled it.
        assert!(atlas.fields_kept() > 0 && atlas.fields_kept() <= keys.len());
        // Under a generous cap nothing is ever dropped.
        let mut roomy = GlyphAtlas::new();
        roomy.keep_fields();
        let _ = frame(&mut roomy, &mut fs, &mut swash);
        let kept = roomy.fields_kept();
        let _ = frame(&mut roomy, &mut fs, &mut swash);
        assert_eq!(roomy.fields_kept(), kept);
        assert!(roomy.field_cache_bytes > 0 && roomy.field_cache_bytes < FIELD_CACHE_MAX_BYTES);
    }

    #[test]
    fn a_full_atlas_counts_every_refusal_including_repeats() {
        let (mut fs, mut swash) = test_font_system();
        let text = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        let keys: Vec<GlyphKey> = (0..text.len()).map(|n| nth_glyph_of(text, 13.0, n, &mut fs)).collect();
        let mut atlas = GlyphAtlas::new();
        atlas.set_max_height(64);
        for k in &keys {
            atlas.get_or_raster(&mut swash, &mut fs, *k);
        }
        let refused = atlas.refused_for_space();
        assert!(refused > 0, "62 fields cannot share one 64-row shelf");
        for k in &keys {
            atlas.get_or_raster(&mut swash, &mut fs, *k);
        }
        assert_eq!(atlas.refused_for_space(), refused * 2, "a hit on a refused key is a refusal too");
    }

    #[test]
    fn the_shelf_wraps_and_the_image_grows_to_hold_it() {
        // The packer's one moving part. Placed glyphs must stay inside the
        // reported image on both axes: a rect past the image's height is a
        // UV that samples nothing, which draws as a blank quad.
        let mut atlas = GlyphAtlas::new();
        let (mut fs, mut swash) = test_font_system();
        let mut wrapped = false;
        let mut previous_y = 0;
        // A 48 px raster padded by the margin is ~60 texels wide, so the
        // alphabet needs more than one 1024-wide shelf.
        for (n, ch) in "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789".chars().enumerate() {
            let key = first_glyph_of(&ch.to_string(), 24.0, &mut fs);
            let Some(rect) = atlas.get_or_raster(&mut swash, &mut fs, key) else { continue };
            let (bytes, w, h) = atlas.image();
            assert!(rect.x + rect.width <= w, "glyph {n} runs past the atlas width: {rect:?}");
            assert!(rect.y + rect.height <= h, "glyph {n} runs past the atlas height: {rect:?}");
            assert_eq!(bytes.len(), (w * h * 4) as usize, "the buffer must match its dimensions");
            if rect.y > previous_y {
                wrapped = true;
            }
            previous_y = rect.y;
        }
        assert!(wrapped, "62 padded 24 px glyphs must not all fit on one 1024 px shelf");
    }

    #[test]
    fn growing_the_image_keeps_the_glyphs_already_in_it() {
        // Growth reallocates the image. It copies the old rows in as a
        // prefix, which is only valid because the width never changes; if
        // that ever stopped being true, every rect issued before the growth
        // would point at the wrong pixels and every label drawn so far
        // would turn to noise.
        let mut atlas = GlyphAtlas::new();
        let (mut fs, mut swash) = test_font_system();
        let first = first_glyph_of("S", 24.0, &mut fs);
        let first_rect = atlas.get_or_raster(&mut swash, &mut fs, first).expect("'S' rasters");
        let before: Vec<u8> = {
            let (bytes, w, _) = atlas.image();
            glyph_bytes(bytes, w, first_rect)
        };
        let height_before = atlas.image().2;

        // Forced through `allocate` rather than by rastering hundreds of
        // glyphs: growth is what is under test, and a full-width slab makes
        // it happen in one step instead of depending on how many letters a
        // shelf happens to hold.
        atlas.allocate(ATLAS_WIDTH, height_before + 64).expect("a slab past the allocation fits");
        assert!(atlas.image().2 > height_before, "the image has to have actually grown");

        let after: Vec<u8> = {
            let (bytes, w, _) = atlas.image();
            glyph_bytes(bytes, w, first_rect)
        };
        assert_eq!(before, after, "the first glyph's pixels must survive the reallocation");
    }

    fn glyph_bytes(bytes: &[u8], width: u32, rect: GlyphRect) -> Vec<u8> {
        let mut out = Vec::new();
        for row in 0..rect.height {
            let start = (((rect.y + row) * width + rect.x) * 4) as usize;
            out.extend_from_slice(&bytes[start..start + (rect.width * 4) as usize]);
        }
        out
    }

    #[test]
    fn an_empty_atlas_reports_a_zero_height_image_and_no_generation() {
        // What the caller sees before any line-placed label exists. Height
        // 0 is deliberate -- `IconPipeline::upload_atlas` clamps to 1x1 --
        // and generation 0 means "nothing to upload yet".
        let atlas = GlyphAtlas::new();
        let (bytes, w, h) = atlas.image();
        assert_eq!((w, h), (ATLAS_WIDTH, 0));
        assert!(bytes.is_empty(), "an atlas with no glyphs allocates no pixels");
        assert_eq!(atlas.generation(), 0);
    }

    #[test]
    fn a_glyph_wider_than_the_atlas_is_refused_rather_than_written_past_the_row() {
        // Not reachable at a 24 px raster, and the reason it is checked
        // anyway: the shelf packer would place a too-wide glyph at x = 0 and
        // then write its pixels past the end of every row it touches.
        let mut atlas = GlyphAtlas::new();
        assert_eq!(atlas.allocate(ATLAS_WIDTH + 1, 10), None);
        assert_eq!(atlas.allocate(ATLAS_WIDTH, 10), Some((0, 0)), "exactly the full width fits");
    }

    #[test]
    fn a_full_atlas_refuses_a_glyph_instead_of_promising_a_texture_no_device_will_take() {
        // The height cap. wgpu's default `max_texture_dimension_2d` is
        // 8192 and this crate has already shipped one bug where an
        // unchecked limit meant a device that drew nothing, so the packer
        // answers `None` past MAX_ATLAS_HEIGHT rather than building an
        // image that cannot be uploaded.
        let mut atlas = GlyphAtlas::new();
        let tall = MAX_ATLAS_HEIGHT / 2;
        assert_eq!(atlas.allocate(ATLAS_WIDTH, tall), Some((0, 0)));
        // Second shelf: starts at tall + PAD, so a second full-height slab
        // overruns the cap.
        assert_eq!(atlas.allocate(ATLAS_WIDTH, tall), None, "the cap has to actually bite");
        assert!(atlas.warned_full, "and it has to say so, once");
        assert!(
            atlas.image().2 <= MAX_ATLAS_HEIGHT,
            "a refused allocation must not have grown the image past the cap"
        );
    }
}

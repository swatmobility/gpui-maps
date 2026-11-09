//! The semantic stage of story validation (spec §2.2, stage 2) and the
//! loader that turns a script's paths and source names into
//! [`SceneInputs`].
//!
//! [`scene_inputs`] resolves every file a script names against a base
//! directory: the story's own folder (the `story` example passes the
//! script file's folder, so `examples/stories/*.json` name their style as
//! `../mapbox-streets-style.json`; an agent's inline script passes its
//! `base_dir`), and the
//! basemap source by name: `fixture`, `openfreemap`, `mapbox`
//! (`MAPBOX_ACCESS_TOKEN`), `pmtiles:<path|url>`, or `{tiles, max_zoom,
//! attribution}`.
//!
//! [`validate_scene`] loads the same things, collecting every problem as a
//! [`Diagnostic`] instead of stopping at the first, then checks the style
//! edits, the source's reachability (a z0 fetch unless `offline`) and
//! ffmpeg, and estimates the render from the tile plan.
//!
//! **Layout warnings** (Task 19). [`validate_scene`] lays the story out
//! headlessly -- the chrome pass's own layout, no GPU
//! (`player::LayoutProbe`) -- on the frames [`layout_sample_frames`]
//! picks: per panel or image at most [`LAYOUT_SAMPLES_PER_ELEMENT`] (first,
//! faded in, midpoint, last); per callout every frame of its span (its
//! anchor moves with the camera), [`MAX_DENSE_CALLOUT_FRAMES`] in all on an
//! even stride beyond that, plus the frames either side of each schedule
//! change; per chapter its first frame and still; per caption its faded-in
//! frame and midpoint. Each warning names the event that caused it
//! (`/events/<i>`, a caption's own `/events/<i>` or `/captions/<j>`,
//! furniture's `/furniture` or `/chapters/<i>/furniture`, a full frame's
//! `/output`), once per path and code, at the earliest sampled time, which
//! its message starts with. Warnings never block: `ok` is false only for
//! an error. Text is measured with the shipped faces: a render the `story`
//! example runs with `--system-fonts` may wrap differently.
//!
//! **The estimate** ([`ValidateOptions::estimate`]) plans the chosen
//! output's tiles every frame and the other tiers' every
//! [`ESTIMATE_TIER_STRIDE_S`]; the agent's `story.validate` skips it by
//! default for a fast answer.
//!
//! **Every warning code** (and every note, `info`) a report can carry -- the
//! same table as [`WARNING_CODES`] and `docs/story-validation.md`, held
//! equal by `the_warning_table_is_the_code_list`:
//!
//! | Code | Severity | Raised by | Meaning |
//! |---|---|---|---|
//! | `text_shrunk` | warning | layout | An element's text was set smaller than asked to fit its card (never below the 28-unit floor). |
//! | `text_truncated` | warning | layout | At the size floor the text still did not fit; it ends with an ellipsis. |
//! | `text_dropped` | warning | layout | A text block had no room at all, not even for an ellipsis, and is not drawn. |
//! | `in_safe_zone` | warning | layout | No position clears the platform's safe-area rectangles (`output.safe_area`); the element sits under the app's UI. |
//! | `overlap` | warning | layout | No position clears another element or the corner chrome (credit plate, SWAT mark, wordmark). |
//! | `bad_measure` | warning | layout | Measuring the element's text gave a NaN, infinite or negative size; it was taken as 0. |
//! | `bad_input` | warning | layout | A caption's size or offset is not a finite number; the floor or 0 is used. |
//! | `callout_offscreen` | warning | layout | A callout's anchor did not project to a finite point; it is not drawn. |
//! | `frame_too_small` | warning | layout | The frame has no room inside its margins and safe areas; no story element is drawn (path `/output`). |
//! | `scale_bar_hidden_pitch` | info | layout | The scale bar was asked for but the camera is pitched too far for one scale; it is hidden on that frame. |
//! | `missing_alt` | warning | images | An image has no `alt` text. |
//! | `missing_credit` | warning | images | An image has no `credit`: the render is not publishable. |
//! | `animated_image_first_frame` | warning | images | An animated image draws only its first frame. |
//! | `safe_area_mismatch` | warning | validate | `output.safe_area` names an app whose UI was measured for another aspect; no safe-area rectangles apply. |
//! | `chapter_without_alt` | warning | validate | A chapter has neither a non-blank `alt` nor a story `summary` to fall back to. |
//! | `raster_upscaled` | warning | validate | The raster has no @2x tiles at a pixel ratio above 1; its tiles are drawn upscaled. |
//! | `cover_too_large` | warning | validate | One frame covers more than 150 tiles: slow and memory-hungry. |
//! | `gpu_memory` | warning | validate | The frame exceeds the capture device's texture or buffer limit, or the GPU memory estimate exceeds 2 GB. |
//! | `too_many_features` | warning | validate | An overlay has more than 50,000 features. |
//! | `hairline` | warning | validate | A line layer's width is under 2 units. |
//! | `unsupported_paint` | warning | validate | An overlay layer names paint properties the story renderer ignores. |
//! | `highlight_over_gradient` | warning | validate | A highlight targets a layer drawn with `line-gradient`, which it cannot recolour. |
//! | `legend_unsupported` | warning | furniture | A legend's layer colour is an expression the legend cannot list. |
//! | `area_property_unmatched` | warning | validate | No feature of an area layer has the categorical legend's `property` equal to an item label: every area would be drawn "Other". |
//! | `area_property_not_numeric` | warning | validate | No feature of an area layer has a number in the sequential legend's `property`: the area is drawn in one colour. |
//! | `legend_overridden` | warning | validate | A legend's own values for an area are replaced by an earlier legend's, which both cards and the fill show. |
//! | `legend_field_unused` | info | validate | A legend field that changes nothing: `ramp` on a sequential legend, `stops` on a categorical one without `ramp`, `property` on a card legend naming no area, a `legend` on a layer that is not an area. |
//! | `chapter_past_end` | warning | chapters | A chapter's `to` is past the story's end; it is clamped. |
//! | `features_without_id` | warning | timeline | A highlight's `ids` cannot name the layer's features that have no GeoJSON `id` while others do. |
//! | `unknown_feature_id` | warning | timeline | A highlight's `ids` names a feature the layer does not have. |
//! | `highlight_base_color` | warning | timeline | Highlighted features have no colour to ramp from; they switch to the highlight colour at once. |
//! | `sun_time_without_terminator` | warning | script | `sun.time` has no visible effect without the terminator. |
//! | `empty_attribution` | warning | script | The basemap states no credit: the render is not publishable. |
//! | `mapbox_wordmark_missing` | warning | script | A Mapbox basemap has no `wordmark`: the render is not publishable. |
//! | `until_ignored` | warning | script | `until` on a `show`/`hide` effect is ignored; it ramps over `fade_s`. |
//! | `lon_wrapped` | warning | script | A longitude outside +/-180 is wrapped. |
//! | `ffmpeg_found` | info | validate | The ffmpeg a video render would use, and its version. |
//! | `ffmpeg_missing` | info | validate | No ffmpeg: a video render writes numbered PNGs and an `encode.sh` instead. |
//! | `safe_area_not_chosen` | info | validate | A design story names no `output.safe_area`; the card sits near the bottom edge (D1). |
//! | `extra_supporting_item` | warning | validate | A card has more than one supporting item (legend, stats, photo, a quote's attribution); only the first (spec §3.2's order) is shown. |
//! | `headline_ignored` | warning | validate | A card has both a `quote` and a `headline`; the quote is shown. |
//! | `supporting_item_deferred` | warning | validate | A card's first row needs three lines, which drops its supporting item. |
//! | `card_too_tall` | warning | validate | The card leaves less than 0.75 of the visible frame to the map (spec §5.6). |
//! | `too_many_stats` | warning | validate | A card names more than two stats; only the first two are drawn. |
//! | `legend_crowded` | warning | validate | A card's legend has more than four items: it draws smaller, in three columns. |
//! | `legend_too_many_items` | warning | validate | A card's legend has more than six items: the rest are grouped as "Other". |
//! | `sources_too_long` | warning | validate | The colophon row's sources need more than two lines; one group is left out. |
//! | `accent_low_contrast` | warning | validate | The design accent is under 3:1 against a `paper` card. |
//! | `index_without_tour` | warning | validate | A card's `"index": true` has no `numbered` layer (not a `tour`) to count against. |
//! | `unknown_dir` | warning | validate | `design.dir` is neither `ltr` nor `rtl`; the direction follows `lang`. |
//! | `unknown_credits` | warning | validate | `design.credits` is neither `bottom` nor `above_platform`; the credits sit on the bottom row. |
//! | `label_under_card` | warning | validate | A basemap label would sit behind the card (or the colophon row) on either side, so it is hidden on this frame. |
//! | `story_labels_capped` | warning | validate | More than 64 story labels would draw on this frame; the rest are left out. |
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::capture::{CaptureRequest, LabelFonts};
use crate::geo::TileId;
use crate::story::overlay_layers::{document_feature_count, layer_sources, LayerSource};
use crate::story::player::{line_and_polygon_points, overlay_doc, DesignCtx, SceneInputs, BASEMAP_COMPACT, BASEMAP_COMPACT_STROKES};
use crate::story::prefetch::{plan, STAND_IN_LEVELS};
use crate::story::script::{parse_utc, BasemapSpec, CardSpec, EffectKind, EventAction, IndexSpec, OverlayLayerKind, Script, Severity};
use crate::story::sink::{self, FfmpegInfo};
use crate::story::tiers::{frame_size, pixel_ratio, Tier};
use crate::story::tracks::{for_scene_track, parse_track, TrackSet};
use crate::story::Diagnostic;
use crate::tile_source::http::HttpTileSource;
use crate::tile_source::TileSource;

/// OpenFreeMap's keyless planet, the exact path `examples/basic_map.rs`
/// draws.
pub const OPENFREEMAP_TILES: &str = "https://tiles.openfreemap.org/planet/20260802_080001_pt/{z}/{x}/{y}.pbf";
/// OpenFreeMap's stated credit (HTML, as its TileJSON gives it).
pub const OPENFREEMAP_ATTRIBUTION: &str = concat!(
    r#"<a href="https://openfreemap.org" target="_blank">OpenFreeMap</a> "#,
    r#"<a href="https://www.openmaptiles.org/" target="_blank">&copy; OpenMapTiles</a> "#,
    r#"Data from <a href="https://www.openstreetmap.org/copyright" target="_blank">OpenStreetMap</a>"#,
);
const MAPBOX_ATTRIBUTION: &str = concat!(
    r#"<a href="https://www.mapbox.com/about/maps" target="_blank">&copy; Mapbox</a> "#,
    r#"<a href="https://www.openstreetmap.org/copyright/" target="_blank">&copy; OpenStreetMap</a>"#,
);
/// The environment variable the Mapbox source reads its token from.
pub const MAPBOX_TOKEN_ENV: &str = "MAPBOX_ACCESS_TOKEN";
/// The estimate's bytes per tile (spec: "bytes at the median tile size").
pub const ESTIMATE_BYTES_PER_TILE: u64 = 60 * 1024;
/// A frame covering more tiles than this is warned about
/// (`cover_too_large`): the render will be slow and memory-hungry.
pub const MAX_COVER_WARN: usize = 150;
/// An overlay with more features than this is warned about
/// (`too_many_features`, spec §3.3): the web and mobile budget.
pub const MAX_OVERLAY_FEATURES_WARN: usize = 50_000;
/// A line layer's `line-width` under this, at 1080-referenced units, is
/// warned about (`hairline`): style widths are already in logical px, so
/// no pixel-ratio scaling enters this comparison.
pub const HAIRLINE_WIDTH_PX: f64 = 2.0;

/// Spec §7's GPU memory budget: an estimate above this at the chosen
/// output warns `gpu_memory` (never blocks).
pub const GPU_BUDGET_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Per frame pixel, in [`gpu_bytes`]: 4 colour + 4 depth + a three-deep
/// readback ring of 4.
pub const GPU_BYTES_PER_PIXEL: u64 = 4 + 4 + 3 * 4;
/// The glyph atlas (one byte per texel), in [`gpu_bytes`].
pub const GLYPH_ATLAS_BYTES: u64 = 2048 * 2048;
/// The icon atlas at its worst case (4096 x 4096 RGBA), in [`gpu_bytes`].
pub const ICON_ATLAS_BYTES: u64 = 4096 * 4096 * 4;
/// The render's wall time per frame that does not grow with the frame
/// (tile decode, layout, the encode hand-off), in [`render_seconds`].
/// Fitted 2026-10-01 to two warm-cache keyless renders of
/// `orbit-marina.json` through ffmpeg 8.0.1 libx264 `slow` on an Apple
/// M-series machine: 3840x2160 at 12.2 fps (0.082 s a frame) and
/// 1080x1920 at 29.5 fps (0.034 s). An order of magnitude, not a
/// promise: a cold tile cache adds the fetches.
pub const ESTIMATE_FRAME_FIXED_S: f64 = 0.018;
/// The render's wall time per frame per megapixel (draw, readback,
/// encode), in [`render_seconds`]. Same reference as
/// [`ESTIMATE_FRAME_FIXED_S`].
pub const ESTIMATE_FRAME_S_PER_MPX: f64 = 0.0077;

/// The other tiers' tile counts in the [`Estimate`] plan every frame this
/// many story seconds apart (the chosen output plans every frame).
pub const ESTIMATE_TIER_STRIDE_S: f64 = 0.5;

/// The capture device's largest texture side: the story renders through
/// `GpuContext::new_headless_with_info`, which asks for the WebGPU
/// baseline limits whatever the adapter offers (8192 there; a Metal
/// adapter would offer 16384), so this, not the adapter's own figure,
/// is the limit a frame must fit.
#[must_use]
pub fn capture_max_texture_dimension_2d() -> u32 {
    wgpu::Limits::default().max_texture_dimension_2d
}

/// The capture device's largest buffer, which the readback of one frame
/// (`width * 4` padded to 256, times `height`) must fit.
#[must_use]
pub fn capture_max_buffer_size() -> u64 {
    wgpu::Limits::default().max_buffer_size
}

/// Spec §7's conservative GPU memory estimate for a `width`x`height`
/// frame: the frame buffers ([`GPU_BYTES_PER_PIXEL`] each), the glyph and
/// icon atlases, every image at RGBA (`image_bytes`) and the mesh cache's
/// budget (`mesh_cache_bytes`, the most it may hold).
#[must_use]
pub fn gpu_bytes(width: u32, height: u32, image_bytes: u64, mesh_cache_bytes: u64) -> u64 {
    u64::from(width) * u64::from(height) * GPU_BYTES_PER_PIXEL + GLYPH_ATLAS_BYTES + ICON_ATLAS_BYTES + image_bytes + mesh_cache_bytes
}

/// The estimated wall time of `frames` frames of `width`x`height`
/// ([`ESTIMATE_FRAME_FIXED_S`] + megapixels x
/// [`ESTIMATE_FRAME_S_PER_MPX`] per frame), with warm tiles.
#[must_use]
pub fn render_seconds(frames: usize, width: u32, height: u32) -> f64 {
    let mpx = f64::from(width) * f64::from(height) / 1e6;
    frames as f64 * (ESTIMATE_FRAME_FIXED_S + mpx * ESTIMATE_FRAME_S_PER_MPX)
}

/// The most a video at `kbps` (the encoder's `-maxrate`, with
/// `-bufsize` twice that) can weigh over `duration_s`: the cap for the
/// duration plus one full VBV buffer, which a short clip can spend on
/// top (a measured 2 s 4K orbit averaged 40.7 Mbit/s under a 35 Mbit/s
/// cap). Bytes.
#[must_use]
pub fn video_bytes_bound(kbps: u32, duration_s: f64) -> u64 {
    let bits = f64::from(kbps) * 1000.0 * (duration_s.max(0.0) + 2.0);
    (bits / 8.0).ceil() as u64
}

/// One resolution tier's share of the [`Estimate`] (spec §7: "render
/// time, tile count and file size per tier"), at the script's aspect.
#[derive(Clone, Debug, serde::Serialize)]
pub struct TierEstimate {
    pub tier: Tier,
    pub width: u32,
    pub height: u32,
    /// [`render_seconds`] for the story's frames at this size.
    pub wall_s_est: Option<f64>,
    /// Distinct tiles over the whole story at this tier (the tile plan
    /// at its pixel ratio).
    pub tiles: usize,
    /// [`video_bytes_bound`] at this tier: an upper bound on the video's
    /// size (CRF usually lands under the cap on a long story).
    pub bytes: u64,
    /// [`gpu_bytes`] at this tier, every image at the size this tier
    /// loads it.
    pub gpu_bytes: u64,
}

/// One row of the warning table (module doc): a code, the severity it is
/// reported at, and the stage that raises it.
#[derive(Clone, Copy, Debug)]
pub struct WarningCode {
    pub code: &'static str,
    pub severity: Severity,
    /// `layout`, `images`, `validate`, `furniture`, `chapters`,
    /// `timeline` or `script`.
    pub raised_by: &'static str,
}

/// Every warning (and note) code a validation report can carry, in the
/// module doc's table order.
pub const WARNING_CODES: &[WarningCode] = &[
    WarningCode { code: "text_shrunk", severity: Severity::Warning, raised_by: "layout" },
    WarningCode { code: "text_truncated", severity: Severity::Warning, raised_by: "layout" },
    WarningCode { code: "text_dropped", severity: Severity::Warning, raised_by: "layout" },
    WarningCode { code: "in_safe_zone", severity: Severity::Warning, raised_by: "layout" },
    WarningCode { code: "overlap", severity: Severity::Warning, raised_by: "layout" },
    WarningCode { code: "bad_measure", severity: Severity::Warning, raised_by: "layout" },
    WarningCode { code: "bad_input", severity: Severity::Warning, raised_by: "layout" },
    WarningCode { code: "callout_offscreen", severity: Severity::Warning, raised_by: "layout" },
    WarningCode { code: "frame_too_small", severity: Severity::Warning, raised_by: "layout" },
    WarningCode { code: "scale_bar_hidden_pitch", severity: Severity::Info, raised_by: "layout" },
    WarningCode { code: "missing_alt", severity: Severity::Warning, raised_by: "images" },
    WarningCode { code: "missing_credit", severity: Severity::Warning, raised_by: "images" },
    WarningCode { code: "animated_image_first_frame", severity: Severity::Warning, raised_by: "images" },
    WarningCode { code: "safe_area_mismatch", severity: Severity::Warning, raised_by: "validate" },
    WarningCode { code: "chapter_without_alt", severity: Severity::Warning, raised_by: "validate" },
    WarningCode { code: "raster_upscaled", severity: Severity::Warning, raised_by: "validate" },
    WarningCode { code: "cover_too_large", severity: Severity::Warning, raised_by: "validate" },
    WarningCode { code: "gpu_memory", severity: Severity::Warning, raised_by: "validate" },
    WarningCode { code: "too_many_features", severity: Severity::Warning, raised_by: "validate" },
    WarningCode { code: "hairline", severity: Severity::Warning, raised_by: "validate" },
    WarningCode { code: "unsupported_paint", severity: Severity::Warning, raised_by: "validate" },
    WarningCode { code: "highlight_over_gradient", severity: Severity::Warning, raised_by: "validate" },
    WarningCode { code: "legend_unsupported", severity: Severity::Warning, raised_by: "furniture" },
    WarningCode { code: "area_property_unmatched", severity: Severity::Warning, raised_by: "validate" },
    WarningCode { code: "area_property_not_numeric", severity: Severity::Warning, raised_by: "validate" },
    WarningCode { code: "legend_overridden", severity: Severity::Warning, raised_by: "validate" },
    WarningCode { code: "legend_field_unused", severity: Severity::Info, raised_by: "validate" },
    WarningCode { code: "chapter_past_end", severity: Severity::Warning, raised_by: "chapters" },
    WarningCode { code: "features_without_id", severity: Severity::Warning, raised_by: "timeline" },
    WarningCode { code: "unknown_feature_id", severity: Severity::Warning, raised_by: "timeline" },
    WarningCode { code: "highlight_base_color", severity: Severity::Warning, raised_by: "timeline" },
    WarningCode { code: "sun_time_without_terminator", severity: Severity::Warning, raised_by: "script" },
    WarningCode { code: "empty_attribution", severity: Severity::Warning, raised_by: "script" },
    WarningCode { code: "mapbox_wordmark_missing", severity: Severity::Warning, raised_by: "script" },
    WarningCode { code: "until_ignored", severity: Severity::Warning, raised_by: "script" },
    WarningCode { code: "lon_wrapped", severity: Severity::Warning, raised_by: "script" },
    WarningCode { code: "ffmpeg_found", severity: Severity::Info, raised_by: "validate" },
    WarningCode { code: "ffmpeg_missing", severity: Severity::Info, raised_by: "validate" },
    // The design system (spec §11, R10).
    WarningCode { code: "safe_area_not_chosen", severity: Severity::Info, raised_by: "validate" },
    WarningCode { code: "extra_supporting_item", severity: Severity::Warning, raised_by: "validate" },
    WarningCode { code: "headline_ignored", severity: Severity::Warning, raised_by: "validate" },
    WarningCode { code: "supporting_item_deferred", severity: Severity::Warning, raised_by: "validate" },
    WarningCode { code: "card_too_tall", severity: Severity::Warning, raised_by: "validate" },
    WarningCode { code: "too_many_stats", severity: Severity::Warning, raised_by: "validate" },
    WarningCode { code: "legend_crowded", severity: Severity::Warning, raised_by: "validate" },
    WarningCode { code: "legend_too_many_items", severity: Severity::Warning, raised_by: "validate" },
    WarningCode { code: "sources_too_long", severity: Severity::Warning, raised_by: "validate" },
    WarningCode { code: "accent_low_contrast", severity: Severity::Warning, raised_by: "validate" },
    WarningCode { code: "index_without_tour", severity: Severity::Warning, raised_by: "validate" },
    WarningCode { code: "unknown_dir", severity: Severity::Warning, raised_by: "validate" },
    WarningCode { code: "unknown_credits", severity: Severity::Warning, raised_by: "validate" },
    // Basemap-label collision (spec §7, R10 review round 1): built in
    // `src/capture/story_overlays.rs`, outside `src/story`, so this
    // table is their only check (`label_warning_codes_in`).
    WarningCode { code: "label_under_card", severity: Severity::Warning, raised_by: "validate" },
    WarningCode { code: "story_labels_capped", severity: Severity::Warning, raised_by: "validate" },
];

/// The most frames [`layout_sample_frames`] samples for one panel or
/// image (first, faded in, midpoint, last).
pub const LAYOUT_SAMPLES_PER_ELEMENT: usize = 4;
/// The most callout frames [`layout_sample_frames`] lays out over the
/// whole story (every frame of every callout's span until then, an even
/// stride beyond), besides each schedule change.
pub const MAX_DENSE_CALLOUT_FRAMES: usize = 4000;

/// How [`validate_scene`] runs.
#[derive(Clone, Debug)]
pub struct ValidateOptions {
    /// Skip the source reachability fetch.
    pub offline: bool,
    /// What relative paths in the script resolve against.
    pub base_dir: PathBuf,
    /// Where fetched images are cached; `None` is
    /// `media::default_image_cache_dir()`.
    pub image_cache_dir: Option<PathBuf>,
    /// Plan every frame's tiles for the [`Estimate`] (and the
    /// `cover_too_large` warning, which reads the plan). The `story` CLI
    /// sets it; the agent's `story.validate` does not, for a fast answer
    /// in its layout loop: `estimate` is then `null` and the rest of the
    /// report is the same.
    pub estimate: bool,
    /// The SWAT mark the render will carry (`RenderOptions::brand_mark`):
    /// the design frame's slot, which the layout probe and the camera
    /// pass lay out around (spec §3.6, P3).
    pub brand_mark: Option<crate::brand_mark::BrandMark>,
    /// The render's `RenderOptions::brand_mark_policy`.
    pub brand_mark_policy: crate::brand_mark::BrandMarkPolicy,
}

impl Default for ValidateOptions {
    /// Online, the working directory, the default image cache, no
    /// estimate, and the render's default marks.
    fn default() -> Self {
        let marks = crate::story::RenderOptions::default();
        Self {
            offline: false,
            base_dir: PathBuf::new(),
            image_cache_dir: None,
            estimate: false,
            brand_mark: marks.brand_mark,
            brand_mark_policy: marks.brand_mark_policy,
        }
    }
}

/// What a render would cost, from the tile plan (no fetch).
#[derive(Clone, Debug, serde::Serialize)]
pub struct Estimate {
    pub duration_s: f64,
    pub frames: usize,
    /// Distinct tiles over the whole story (exact and stand-in).
    pub tiles: usize,
    /// The largest single frame's exact-tile count.
    pub max_cover: usize,
    /// `tiles` at [`ESTIMATE_BYTES_PER_TILE`].
    pub bytes: u64,
    /// [`render_seconds`] at the script's output size.
    pub wall_s_est: Option<f64>,
    /// [`gpu_bytes`] at the script's output size.
    pub gpu_bytes: u64,
    /// Every tier at the script's aspect, smallest first.
    pub tiers: Vec<TierEstimate>,
}

/// Everything [`validate_scene`] found.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Report {
    /// No diagnostic is an error.
    pub ok: bool,
    pub diagnostics: Vec<Diagnostic>,
    /// `None` when the camera path or the timeline does not resolve.
    pub estimate: Option<Estimate>,
    /// The ffmpeg a video render would use; `None` means PNGs and an
    /// `encode.sh` instead.
    pub ffmpeg: Option<FfmpegInfo>,
    /// The design system's decisions (spec §11, R10): `None` for a 1a
    /// story (no top-level `design`), which draws exactly as before.
    pub design: Option<DesignReport>,
}

/// [`Report::design`] (spec §11): what the design system decided for
/// this story, in plain English, never a Rust type name (P15).
#[derive(Clone, Debug, serde::Serialize)]
pub struct DesignReport {
    /// The archetype (spec §2.1): `design.archetype`, or detected.
    pub archetype: String,
    /// Which spec §2.1 rule (1-7) decided it; 1 for an explicit
    /// `design.archetype`.
    pub rule: u8,
    /// The overlay theme (spec §6.2): `design.theme`, or the
    /// archetype's.
    pub theme: String,
    /// `paper` or `ink` (spec §4.3).
    pub tone: String,
    /// `output.safe_area`, `none` when absent (D1).
    pub safe_area: String,
    /// The lowest [`crate::story::frame_layout::CardLayout::map_share`]
    /// over every card the story shows (spec §5.6): below 0.75 is
    /// `card_too_tall`. `1.0` for a story with no card on screen.
    pub map_share_min: f32,
}

/// The shipped label faces (spec §5.5), `font_fallback::SHIPPED_FONTS`
/// (the browser shell's list and order), default family `Noto Sans`.
/// Every machine shapes the same glyphs from these.
#[must_use]
pub fn shipped_label_fonts() -> LabelFonts {
    let files: Vec<Vec<u8>> = crate::font_fallback::SHIPPED_FONTS.iter().map(|f| f.to_vec()).collect();
    (files.into(), "Noto Sans".to_string())
}

/// A user-supplied tile or archive URL as it may appear in the manifest,
/// on stderr and in a cache directory name: no query string or fragment
/// (where `key=`, `token=`, `sig=`, `X-Amz-*` and presigned signatures
/// live) and no `user:password@`. A secret written into the URL's PATH
/// cannot be told from the path and is kept -- name such a source with a
/// query parameter instead.
#[must_use]
pub fn redacted_url(url: &str) -> String {
    let cut = url.find(['?', '#']).map_or(url, |i| &url[..i]);
    match cut.find("://") {
        Some(scheme_end) => {
            let (scheme, rest) = cut.split_at(scheme_end + 3);
            let host_end = rest.find('/').unwrap_or(rest.len());
            let (authority, path) = rest.split_at(host_end);
            let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
            format!("{scheme}{host}{path}")
        }
        None => cut.to_string(),
    }
}

/// `p` against `base_dir` unless it is absolute.
fn resolve(base_dir: &Path, p: &str) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() { path.to_path_buf() } else { base_dir.join(path) }
}

/// Resolve `rel` inside `base_dir`: refuses an absolute path or one with a
/// `..` component outright (`path_escapes`, no filesystem access needed to
/// tell), then refuses any path whose canonical form (following symlinks)
/// is not under `base_dir`'s own canonical form (`path_escapes`: a
/// symlink pointed outside it), and finally a path that does not exist
/// (`file_not_found`, the same code a `geojson` path's own missing-file
/// diagnostic uses -- one vocabulary for authors, review Finding 3). An
/// overlay's `data` path (spec §2.2) must stay inside the story's folder;
/// `check_bad_path` (stage 1) already refuses `..`, `~` and an absolute
/// path syntactically, but only a symlink followed at load time can point
/// outside without saying so in its own text, which is what the
/// canonicalize check catches.
///
/// # Errors
/// `path_escapes` or `file_not_found`, as above. The returned diagnostic's
/// `path` is empty; the caller sets it to the script's own JSON pointer.
pub fn resolve_in_story_dir(base_dir: &Path, rel: &str) -> Result<PathBuf, Diagnostic> {
    let escapes = || {
        diag(
            Severity::Error,
            "",
            "path_escapes",
            format!("`{rel}` must be a path inside the story's own folder"),
            Some("move the file into the story folder, or reference it with a path that stays inside it"),
        )
    };
    let p = Path::new(rel);
    if p.is_absolute() || p.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err(escapes());
    }
    let canon_base = base_dir
        .canonicalize()
        .map_err(|e| err("", "file_not_found", format!("cannot read the story folder {}: {e}", base_dir.display())))?;
    let candidate = base_dir.join(p);
    match candidate.canonicalize() {
        Ok(canon) if canon.starts_with(&canon_base) => Ok(canon),
        Ok(_) => Err(escapes()),
        Err(e) => Err(err("", "file_not_found", format!("cannot read {}: {e}", candidate.display()))),
    }
}

/// The height, in pixels, an SVG wordmark is rasterised at: 8x the SWAT
/// mark's 18 logical pixels, so it is drawn downscaled at any pixel ratio a
/// story renders at.
pub const WORDMARK_SVG_HEIGHT_PX: f32 = 144.0;
/// The widest raster an SVG wordmark is rasterised to.
const WORDMARK_SVG_MAX_WIDTH_PX: f32 = 2048.0;

/// A user-supplied basemap wordmark (`scene.basemap.wordmark`): a PNG (or
/// any format `image` decodes) as it is, or an SVG rasterised at
/// [`WORDMARK_SVG_HEIGHT_PX`] with its aspect kept. The engine ships no
/// third-party logo: this is the only way one is drawn.
///
/// # Errors
/// The file cannot be read, does not decode, or is empty.
pub fn load_wordmark(path: &Path) -> anyhow::Result<image::RgbaImage> {
    let bytes = std::fs::read(path).map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;
    let is_svg = path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("svg"));
    let image = if is_svg { rasterise_svg(&bytes)? } else { image::load_from_memory(&bytes)?.to_rgba8() };
    anyhow::ensure!(image.width() > 0 && image.height() > 0, "{} is empty", path.display());
    Ok(image)
}

fn rasterise_svg(bytes: &[u8]) -> anyhow::Result<image::RgbaImage> {
    use resvg::{tiny_skia, usvg};
    let tree = usvg::Tree::from_data(bytes, &usvg::Options::default())?;
    let size = tree.size();
    anyhow::ensure!(size.width() > 0.0 && size.height() > 0.0, "the SVG has no size");
    let scale = (WORDMARK_SVG_HEIGHT_PX / size.height()).min(WORDMARK_SVG_MAX_WIDTH_PX / size.width());
    let (w, h) = ((size.width() * scale).ceil().max(1.0) as u32, (size.height() * scale).ceil().max(1.0) as u32);
    let mut pixmap = tiny_skia::Pixmap::new(w, h).ok_or_else(|| anyhow::anyhow!("cannot allocate a {w}x{h} raster"))?;
    resvg::render(&tree, tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    // tiny-skia holds premultiplied alpha; the icon pipeline takes straight.
    let straight: Vec<u8> = pixmap
        .pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect();
    image::RgbaImage::from_raw(w, h, straight).ok_or_else(|| anyhow::anyhow!("the SVG raster has the wrong size"))
}

fn diag(severity: Severity, path: &str, code: &'static str, message: impl Into<String>, hint: Option<&str>) -> Diagnostic {
    Diagnostic { severity, path: path.to_string(), code, message: message.into(), hint: hint.map(str::to_string) }
}

fn err(path: &str, code: &'static str, message: impl Into<String>) -> Diagnostic {
    diag(Severity::Error, path, code, message, None)
}

/// A JSON document at `p`, or the diagnostic saying why not.
fn read_json(base_dir: &Path, p: &str, at: &str) -> Result<serde_json::Value, Diagnostic> {
    if p.starts_with("http://") || p.starts_with("https://") {
        return Err(diag(
            Severity::Error,
            at,
            "url_not_supported",
            format!("`{p}` is a URL; the story loader reads local files"),
            Some("download it and name the local path"),
        ));
    }
    let path = resolve(base_dir, p);
    let bytes = std::fs::read(&path).map_err(|e| err(at, "file_not_found", format!("cannot read {}: {e}", path.display())))?;
    serde_json::from_slice(&bytes).map_err(|e| err(at, "file_not_json", format!("{} is not JSON: {e}", path.display())))
}

/// The basemap source a script names, its manifest label, its zoom
/// ceiling and the credit it states. `Err` is the diagnostic at
/// `/scene/basemap`.
fn basemap_source(
    spec: &BasemapSpec,
    style: Option<&serde_json::Value>,
    base_dir: &Path,
) -> Result<(Arc<dyn TileSource>, String, u8), Diagnostic> {
    const AT: &str = "/scene/basemap/source";
    match spec {
        BasemapSpec::Tiles { tiles, max_zoom, attribution } => {
            let url = tiles.first().ok_or_else(|| err("/scene/basemap/tiles", "no_tile_url", "`tiles` names no URL"))?;
            let clean = redacted_url(url);
            let label = clean.split("/{z}").next().unwrap_or(&clean).to_string();
            // `${MAPBOX_ACCESS_TOKEN}` in a tiles URL is filled here, at
            // load, as the `mapbox` source fills its own: a script (and
            // the manifest's copy of it) names the token, never holds it.
            let marker = format!("${{{}}}", crate::story::ACCESS_TOKEN_NAME);
            let url = if url.contains(&marker) {
                let token = std::env::var(MAPBOX_TOKEN_ENV).ok().filter(|t| !t.is_empty()).ok_or_else(|| {
                    diag(
                        Severity::Error,
                        "/scene/basemap/tiles",
                        "mapbox_token_missing",
                        format!("the tiles URL names ${{{MAPBOX_TOKEN_ENV}}} and it is not set"),
                        Some("set the token (a public `pk.` token)"),
                    )
                })?;
                url.replace(&marker, &token)
            } else {
                url.clone()
            };
            Ok((Arc::new(HttpTileSource::new(url, *max_zoom, attribution.clone())), label, *max_zoom))
        }
        BasemapSpec::Named { source, .. } => match source.as_str() {
            "fixture" => Ok((Arc::new(crate::story::fixture::FixtureSource), "fixture".to_string(), 14)),
            "openfreemap" => {
                Ok((Arc::new(HttpTileSource::new(OPENFREEMAP_TILES, 14, OPENFREEMAP_ATTRIBUTION)), "openfreemap".to_string(), 14))
            }
            "mapbox" => {
                let tilesets = style
                    .and_then(|s| s.pointer("/sources/composite/url"))
                    .and_then(serde_json::Value::as_str)
                    .and_then(|u| u.strip_prefix("mapbox://"))
                    .ok_or_else(|| {
                        diag(
                            Severity::Error,
                            AT,
                            "mapbox_style_has_no_composite",
                            "the `mapbox` source reads its tileset ids from the style's `sources.composite.url` (a mapbox:// URL), and this style has none",
                            Some("use examples/mapbox-streets-style.json, or name the tiles with {\"tiles\": [...]}"),
                        )
                    })?;
                let token = std::env::var(MAPBOX_TOKEN_ENV).ok().filter(|t| !t.is_empty()).ok_or_else(|| {
                    diag(
                        Severity::Error,
                        AT,
                        "mapbox_token_missing",
                        format!("the `mapbox` source needs {MAPBOX_TOKEN_ENV} (a public `pk.` token)"),
                        Some("set the token, or render the keyless twin with `--keyless` (Liberty on OpenFreeMap)"),
                    )
                })?;
                let url = format!("https://api.mapbox.com/v4/{tilesets}/{{z}}/{{x}}/{{y}}.mvt?access_token={token}");
                // The label names the tilesets, never the token.
                Ok((Arc::new(HttpTileSource::new(url, 16, MAPBOX_ATTRIBUTION)), format!("mapbox:{tilesets}"), 16))
            }
            other => match other.strip_prefix("pmtiles:") {
                Some(target) => {
                    let target_label = target;
                    let target = target.to_string();
                    let local = resolve(base_dir, &target);
                    // Opened on a thread of its own with its own runtime, so
                    // this works whether or not the caller is inside one.
                    let opened = std::thread::spawn(move || -> anyhow::Result<crate::tile_source::pmtiles::PmtilesSource> {
                        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
                        if target.starts_with("http://") || target.starts_with("https://") {
                            rt.block_on(crate::tile_source::pmtiles::PmtilesSource::open_url(&target))
                        } else {
                            rt.block_on(crate::tile_source::pmtiles::PmtilesSource::open(&local))
                        }
                    })
                    .join()
                    .map_err(|_| anyhow::anyhow!("opening the archive panicked"))
                    .and_then(|r| r)
                    .map_err(|e| err(AT, "pmtiles_open_failed", format!("cannot open `pmtiles:{}`: {e:#}", redacted_url(target_label))))?;
                    let max = opened.max_zoom();
                    Ok((Arc::new(opened), format!("pmtiles:{}", redacted_url(target_label)), max))
                }
                None => Err(diag(
                    Severity::Error,
                    AT,
                    "unknown_source",
                    format!("unknown basemap source `{other}`"),
                    Some("one of \"openfreemap\", \"mapbox\", \"fixture\", \"pmtiles:<path|url>\", or {\"tiles\": [...], \"max_zoom\": n, \"attribution\": \"...\"}"),
                )),
            },
        },
    }
}

/// Every overlay document `script` names, keyed as `player::overlay_doc`
/// looks them up: the scene's `geojson` paths, every `set_overlay_data`
/// document and every string `data` path (read inside the story folder).
/// A diagnostic for each one that will not load.
pub(crate) fn overlay_documents(script: &Script, base_dir: &Path, diags: &mut Vec<Diagnostic>) -> HashMap<String, String> {
    let mut overlays = HashMap::new();
    {
        let mut read_overlay = |p: &str, at: String, diags: &mut Vec<Diagnostic>| {
            if overlays.contains_key(p) {
                return;
            }
            let path = resolve(base_dir, p);
            match std::fs::read_to_string(&path) {
                Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
                    Ok(_) => {
                        overlays.insert(p.to_string(), text);
                    }
                    Err(e) => diags.push(err(&at, "file_not_json", format!("{} is not JSON: {e}", path.display()))),
                },
                Err(e) => diags.push(err(&at, "file_not_found", format!("cannot read {}: {e}", path.display()))),
            }
        };
        for (i, o) in script.scene.overlays.iter().enumerate() {
            if let Some(geojson) = &o.geojson {
                read_overlay(geojson, format!("/scene/overlays/{i}/geojson"), diags);
            }
        }
        for (i, e) in script.events.iter().enumerate() {
            if let EventAction::SetOverlayData { geojson, .. } = &e.action {
                read_overlay(geojson, format!("/events/{i}/set_overlay_data/geojson"), diags);
            }
        }
    }
    // An overlay's `data` (spec §2.2, Task 8): an inline object is read by
    // `player::overlay_doc` straight off `OverlaySpec::data`, nothing to
    // load here; a string is a path inside the story folder
    // (`resolve_in_story_dir`), read once and kept under that same path
    // string, exactly what `overlay_doc` looks it up by. Its diagnostics
    // reuse the `geojson` path's own codes (`file_not_found`,
    // `file_not_json`; review Finding 3 -- one vocabulary for authors,
    // whichever of `data`/`geojson` they used) plus `path_escapes`, and
    // every one of the three is an ERROR that makes `scene_inputs` and
    // `scene_inputs_with_sprites` bail, exactly like a broken `geojson`
    // path already does (spec §7's "render refused" list; review
    // Finding 1 -- a `data` file that never loads must refuse the render,
    // not become a silent gap, since the agent's render path calls
    // `scene_inputs` directly and never runs `validate_scene` first).
    for (i, o) in script.scene.overlays.iter().enumerate() {
        if let Some(serde_json::Value::String(path)) = &o.data {
            let at = format!("/scene/overlays/{i}/data");
            match resolve_in_story_dir(base_dir, path) {
                Ok(resolved) => match std::fs::read_to_string(&resolved) {
                    Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
                        Ok(_) => {
                            overlays.insert(path.clone(), text);
                        }
                        Err(e) => diags.push(err(&at, "file_not_json", format!("{} is not JSON: {e}", resolved.display()))),
                    },
                    Err(e) => diags.push(err(&at, "file_not_found", format!("cannot read {}: {e}", resolved.display()))),
                },
                Err(mut d) => {
                    d.path = at;
                    diags.push(d);
                }
            }
        }
    }
    overlays
}

/// Load everything `script` names, collecting a diagnostic for each thing
/// that cannot be loaded. `None` when something the render cannot do
/// without (the style, the source) failed.
fn load(script: &Script, base_dir: &Path, diags: &mut Vec<Diagnostic>) -> Option<SceneInputs> {
    let style = match read_json(base_dir, &script.scene.style, "/scene/style") {
        Ok(v) => Some(v),
        Err(d) => {
            diags.push(d);
            None
        }
    };
    let source = match basemap_source(&script.scene.basemap, style.as_ref(), base_dir) {
        Ok(s) => Some(s),
        Err(d) => {
            diags.push(d);
            None
        }
    };

    let overlays = overlay_documents(script, base_dir, diags);

    // Tracks, each diagnostic pointing into the file from the script.
    let clock_start = parse_utc(&script.clock.start_utc).unwrap_or(0.0);
    let mut tracks = TrackSet::default();
    for (i, t) in script.scene.tracks.iter().enumerate() {
        let path = resolve(base_dir, &t.file);
        match std::fs::read(&path) {
            Ok(bytes) => match parse_track(&bytes, &t.id, t.style.clone(), clock_start) {
                Ok(track) => tracks.tracks.push(track),
                Err(d) => diags.push(for_scene_track(d, i)),
            },
            Err(e) => diags.push(err(&format!("/scene/tracks/{i}/file"), "file_not_found", format!("cannot read {}: {e}", path.display()))),
        }
    }

    // `set_style` documents, loaded now so a missing one is reported here
    // (the timeline reports it again, by event, if it is still missing).
    let mut styles: HashMap<String, serde_json::Value> = HashMap::new();
    for (i, e) in script.events.iter().enumerate() {
        if let EventAction::SetStyle { style } = &e.action {
            match read_json(base_dir, style, &format!("/events/{i}/set_style/style")) {
                Ok(v) => {
                    styles.insert(style.clone(), v);
                }
                Err(d) => diags.push(d),
            }
        }
    }

    // The user's basemap wordmark (Mapbox's terms require theirs; the
    // engine ships none).
    let wordmark = match &script.scene.basemap {
        BasemapSpec::Named { wordmark: Some(p), .. } => match load_wordmark(&resolve(base_dir, p)) {
            Ok(image) => Some(Arc::new(image)),
            Err(e) => {
                diags.push(err("/scene/basemap/wordmark", "wordmark_unreadable", format!("{e:#}")));
                None
            }
        },
        _ => None,
    };

    let (source, source_label, flat_tile_zoom_max) = source?;
    Some(SceneInputs {
        source,
        source_label,
        base_style: style?,
        load_style: Box::new(move |p| styles.get(p).cloned()),
        overlays,
        tracks,
        label_fonts: Some(shipped_label_fonts()),
        flat_tile_zoom_max,
        raster: None,
        credits: Vec::new(),
        wordmark,
        sprites: None,
        sprite_sheet: None,
        sprite_notes: Vec::new(),
        story_dir: base_dir.to_path_buf(),
        offline: false,
        images_validated: false,
    })
}

/// The scene's sprite sheet and every line the manifest's gaps should
/// carry about icons.
pub(crate) struct SceneSprites {
    pub atlas: Option<Arc<crate::sprite::SpriteAtlas>>,
    pub info: Option<crate::story::player::SpriteSheetInfo>,
    pub notes: Vec<String>,
}

/// The scene style's sprite sheet, for icons: its `"sprite"` resolved
/// (`sprite::resolve_sprite_url`, a `mapbox://` one with
/// `MAPBOX_ACCESS_TOKEN`), then [`sprite_sheet_in`] under
/// `~/.cache/gpui-map/sprites/`.
///
/// No sheet for the fixture source (tests render hermetically, with no
/// network) or a style with no `"sprite"`. Every other way of losing icons
/// is a note, for the manifest's gaps and stderr: an array-form
/// `"sprite"` (not supported yet), a sheet that cannot be had, and a
/// `set_style` document (`set_styles`: its path and document) naming a
/// different sheet (icons keep the scene style's).
fn scene_sprites(
    style: &serde_json::Value,
    set_styles: &[(String, Option<serde_json::Value>)],
    source_label: &str,
    offline: bool,
    pinned: Option<&str>,
) -> SceneSprites {
    let mut out = SceneSprites { atlas: None, info: None, notes: Vec::new() };
    if source_label == "fixture" {
        return out;
    }
    let base = style.get("sprite");
    for (path, doc) in set_styles {
        let theirs = doc.as_ref().and_then(|d| d.get("sprite"));
        if theirs.is_some() && theirs != base {
            out.notes.push(format!(
                "icons after set_style {path}: its sprite sheet differs from the scene style's, and icons keep the scene style's sheet"
            ));
        }
    }
    let sprite = match base {
        None => return out,
        Some(serde_json::Value::String(s)) => s.as_str(),
        Some(_) => {
            out.notes.push("icons (the style's \"sprite\" is an array of sheets, which the capture does not load yet)".to_string());
            return out;
        }
    };
    let token = std::env::var(MAPBOX_TOKEN_ENV).ok().filter(|t| !t.is_empty());
    let loaded = crate::sprite::resolve_sprite_url(sprite, token.as_deref()).and_then(|base| {
        crate::sprite_cache::sprite_sheet_in(
            &crate::sprite_cache::sprite_cache_dir(&base),
            &base,
            offline,
            pinned,
            &crate::sprite_cache::fetch_sprite_sheet,
        )
    });
    match loaded {
        Ok(sheet) => {
            if let Some(want) = pinned.filter(|want| *want != sheet.info.sha256) {
                out.notes.push(format!(
                    "icons: the sprite sheet the earlier render drew (sha256 {want}) is no longer cached; this range draws sheet {}, \
                     so its icons may differ from the frames it is spliced into",
                    sheet.info.sha256
                ));
            }
            out.atlas = Some(Arc::new(sheet.atlas));
            out.info = Some(sheet.info);
        }
        Err(e) => out.notes.push(format!(
            "icons (the sprite sheet {} did not load: {})",
            redacted_url(sprite),
            crate::sprite::redact_url_credentials(&format!("{e:#}"))
        )),
    }
    out
}

/// What a live preview needs from a script's files: the scene style, the
/// documents its `set_style` events name, and its tracks. No tile source
/// is built (the live map keeps its own), so nothing here opens a network
/// connection or needs a token.
pub struct SceneDocuments {
    pub base_style: serde_json::Value,
    pub load_style: crate::story::player::StyleLoader,
    pub tracks: TrackSet,
}

/// [`SceneDocuments`] of `script`, resolved against `base_dir`.
///
/// # Errors
/// Every style or track file that cannot be read, listed in one error.
pub fn scene_documents(script: &Script, base_dir: &Path) -> anyhow::Result<SceneDocuments> {
    let mut diags = Vec::new();
    let style = read_json(base_dir, &script.scene.style, "/scene/style").map_err(|d| diags.push(d)).ok();
    let clock_start = parse_utc(&script.clock.start_utc).unwrap_or(0.0);
    let mut tracks = TrackSet::default();
    for (i, t) in script.scene.tracks.iter().enumerate() {
        let path = resolve(base_dir, &t.file);
        match std::fs::read(&path) {
            Ok(bytes) => match parse_track(&bytes, &t.id, t.style.clone(), clock_start) {
                Ok(track) => tracks.tracks.push(track),
                Err(d) => diags.push(for_scene_track(d, i)),
            },
            Err(e) => diags.push(err(&format!("/scene/tracks/{i}/file"), "file_not_found", format!("cannot read {}: {e}", path.display()))),
        }
    }
    let mut styles: HashMap<String, serde_json::Value> = HashMap::new();
    for (i, e) in script.events.iter().enumerate() {
        if let EventAction::SetStyle { style } = &e.action {
            match read_json(base_dir, style, &format!("/events/{i}/set_style/style")) {
                Ok(v) => {
                    styles.insert(style.clone(), v);
                }
                Err(d) => diags.push(d),
            }
        }
    }
    let errors: Vec<String> =
        diags.iter().filter(|d| d.severity == Severity::Error).map(|d| format!("{} ({}): {}", d.path, d.code, d.message)).collect();
    match style {
        Some(base_style) if errors.is_empty() => {
            Ok(SceneDocuments { base_style, load_style: Box::new(move |p| styles.get(p).cloned()), tracks })
        }
        _ => anyhow::bail!("the scene does not load: {}", errors.join("; ")),
    }
}

/// Resolve `script`'s style, overlays, tracks, `set_style` documents,
/// basemap source and fonts (the shipped faces) against `base_dir`.
/// Loading never fetches a tile. It does fetch the style's sprite sheet
/// once into `~/.cache/gpui-map/sprites/` (the icons, on every request as
/// `CaptureRequest::sprites`); `offline` reads that cache and never the
/// network. A sheet that cannot be had is not an error: the render names
/// icons in its gaps.
///
/// # Errors
/// Every file or source that cannot be loaded, listed in one error.
pub fn scene_inputs(script: &Script, base_dir: &Path, offline: bool) -> anyhow::Result<SceneInputs> {
    scene_inputs_with_sprites(script, base_dir, offline, None)
}

/// [`scene_inputs`], drawing the sprite sheet an earlier render recorded
/// (`RenderManifest::sprites`' `sha256`) when it is still cached, so a
/// frame range re-rendered to splice into that render draws its icons. A
/// sheet no longer cached is a note in the gaps, and the usual sheet is
/// drawn.
///
/// # Errors
/// See [`scene_inputs`].
pub fn scene_inputs_with_sprites(
    script: &Script,
    base_dir: &Path,
    offline: bool,
    sprite_sha256: Option<&str>,
) -> anyhow::Result<SceneInputs> {
    let mut diags = Vec::new();
    let inputs = load(script, base_dir, &mut diags);
    // An overlay `data` path that will not load refuses the render, the
    // same as a broken `geojson` path already does (spec §7; review
    // Finding 1): every caller of `scene_inputs`/`scene_inputs_with_sprites`
    // gets this, not only one that already ran `validate_scene` first --
    // the agent's render action (`story_agentic.rs`'s `render_job`) calls
    // this directly and never runs `validate_scene`.
    let errors: Vec<String> =
        diags.iter().filter(|d| d.severity == Severity::Error).map(|d| format!("{} ({}): {}", d.path, d.code, d.message)).collect();
    match inputs {
        Some(mut inputs) if errors.is_empty() => {
            let set_styles: Vec<(String, Option<serde_json::Value>)> = script
                .events
                .iter()
                .filter_map(|e| match &e.action {
                    EventAction::SetStyle { style } => Some((style.clone(), (inputs.load_style)(style))),
                    _ => None,
                })
                .collect();
            let sprites = scene_sprites(&inputs.base_style, &set_styles, &inputs.source_label, offline, sprite_sha256);
            inputs.sprites = sprites.atlas;
            inputs.sprite_sheet = sprites.info;
            inputs.sprite_notes = sprites.notes;
            inputs.offline = offline;
            Ok(inputs)
        }
        _ => anyhow::bail!("the scene does not load: {}", errors.join("; ")),
    }
}

/// The publish table (spec §6) a render of `script` would carry, judged
/// before it: [`crate::story::publish_table_for`] over the loaded scene
/// (offline: no fetch), every image the script names counted as drawn.
/// When the scene does not load (a Mapbox story without its token), the
/// table is judged from the script alone: the basemap by its name (or its
/// `tiles` URL, redacted, with its stated `attribution`), the wordmark by
/// whether its file loads. `video`: the render will be more than one frame.
#[must_use]
pub fn publish_preview(script: &Script, base_dir: &Path, video: bool) -> Vec<crate::story::burn_in::PublishRow> {
    use crate::story::burn_in::{publish_table, PublishInputs, VerdictSource};
    let mut ignored = Vec::new();
    if let Some(inputs) = load(script, base_dir, &mut ignored) {
        return crate::story::publish_table_for(script, &inputs, video, &|_| true, &[], crate::story::burn_in::style_has_mapbox_sprite(&inputs.base_style));
    }
    let (label, stated) = match &script.scene.basemap {
        BasemapSpec::Named { source, .. } if source == "fixture" => (source.clone(), Some(crate::story::fixture::FIXTURE_CREDIT.to_string())),
        BasemapSpec::Named { source, .. } => (source.clone(), None),
        BasemapSpec::Tiles { tiles, attribution, .. } => {
            let clean = redacted_url(tiles.first().map_or("", String::as_str));
            (clean.split("/{z}").next().unwrap_or(&clean).to_string(), Some(attribution.clone()))
        }
    };
    let wordmark_drawn =
        matches!(&script.scene.basemap, BasemapSpec::Named { wordmark: Some(p), .. } if load_wordmark(&resolve(base_dir, p)).is_ok());
    let credits: Vec<String> = stated.iter().cloned().chain(script.scene.credits.iter().cloned()).collect();
    let overlays: Vec<(String, Option<String>)> = script.scene.overlays.iter().map(|o| (o.id.clone(), o.attribution.clone())).collect();
    let images: Vec<(String, Option<String>)> =
        crate::story::media::script_images(script).into_iter().map(|(_, spec)| (spec.src.clone(), spec.credit.clone())).collect();
    let mapbox_sprite =
        read_json(base_dir, &script.scene.style, "/scene/style").is_ok_and(|s| crate::story::burn_in::style_has_mapbox_sprite(&s));
    let basemap = VerdictSource { label: &label, stated: stated.as_deref(), acknowledge: &script.output.acknowledge };
    publish_table(&PublishInputs {
        basemap: &basemap,
        credits: &credits,
        raster: None,
        video,
        wordmark_drawn,
        overlays: &overlays,
        images: &images,
        layers: &[],
        mapbox_sprite,
    })
}

/// The spec §2.2 stage-2 checks on a script that already parsed: the style
/// and every file load, the style edits compile at the time they fire,
/// `MAPBOX_ACCESS_TOKEN` is set for Mapbox, the source answers a z0 fetch
/// (skipped when `offline`), ffmpeg (information), and the estimate from
/// the tile plan (a warning when one frame covers more than
/// [`MAX_COVER_WARN`] tiles). The stage-1 warnings of `raw`
/// (`parse_with_warnings`) come first. Never panics.
pub async fn validate_scene(script: &Script, raw: &[u8], opts: &ValidateOptions) -> Report {
    let mut diags = match crate::story::script::parse_with_warnings(raw) {
        Ok((_, warnings)) => warnings,
        Err(errors) => errors,
    };
    let inputs = load(script, &opts.base_dir, &mut diags);
    warn_safe_area_mismatch(script, &mut diags);
    warn_index_without_tour(script, &mut diags);
    warn_unknown_dir(script, &mut diags);
    warn_unknown_credits(script, &mut diags);
    // Every image (spec §4.3): loaded as the render will load it, so a
    // missing, unreachable, oversized or undecodable one refuses here.
    let output = script.output();
    let mut images = crate::story::media::load_script_images(
        script,
        &opts.base_dir,
        &opts.image_cache_dir.clone().unwrap_or_else(crate::story::media::default_image_cache_dir),
        output.width.max(output.height),
        opts.offline,
    )
    .await;
    diags.extend(std::mem::take(&mut images.diagnostics));
    // Each image's upright size before any downscale: the GPU estimate
    // sizes it as each tier would load it.
    let image_sizes: Vec<(u32, u32)> = images.loaded.iter().map(|l| l.original).collect();
    if let Some(raster) = script.scene.raster.as_deref() {
        let (_, two_x) = crate::story::tiers::resolve_raster(raster, output.pixel_ratio, None);
        if output.pixel_ratio > crate::story::tiers::RASTER_2X_ABOVE && !two_x {
            diags.push(diag(
                Severity::Warning,
                "/scene/raster",
                "raster_upscaled",
                format!(
                    "at pixel ratio {} the raster `{}` offers no @2x tiles: its 1x tiles are drawn upscaled (softer than the map)",
                    output.pixel_ratio,
                    crate::story::redact_url_secrets(raster)
                ),
                Some("use a template with {r} or @2x tiles, or a lower tier"),
            ));
        }
    }

    let ffmpeg = sink::detect_ffmpeg();
    diags.push(match &ffmpeg {
        Some(f) => diag(Severity::Info, "", "ffmpeg_found", format!("Found ffmpeg `{}` ({}).", f.name(), f.version), None),
        None => diag(
            Severity::Info,
            "",
            "ffmpeg_missing",
            "No ffmpeg: a video render writes numbered PNGs and an encode.sh instead.",
            Some("install ffmpeg or set GPUI_MAP_FFMPEG"),
        ),
    });

    let mut estimate = None;
    let mut design = None;
    if let Some(inputs) = inputs {
        if !opts.offline {
            let tile = TileId { z: 0, x: 0, y: 0 };
            let fetch = tokio::time::timeout(std::time::Duration::from_secs(15), inputs.source.fetch(tile));
            match fetch.await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => diags.push(diag(
                    Severity::Error,
                    "/scene/basemap/source",
                    "source_unreachable",
                    format!(
                        "`{}` did not answer a z0 fetch: {}",
                        inputs.source_label,
                        crate::sprite::redact_url_credentials(&format!("{e:#}"))
                    ),
                    Some("check the network, or pass --offline to skip this check"),
                )),
                Err(_) => diags.push(diag(
                    Severity::Error,
                    "/scene/basemap/source",
                    "source_unreachable",
                    format!("`{}` did not answer a z0 fetch within 15 s", inputs.source_label),
                    Some("check the network, or pass --offline to skip this check"),
                )),
            }
        }
        let marks = crate::story::RenderOptions { brand_mark: opts.brand_mark, brand_mark_policy: opts.brand_mark_policy, ..crate::story::RenderOptions::default() };
        estimate = check_timeline(script, inputs, &images, &image_sizes, opts.estimate, &marks, &mut diags, &mut design);
    }

    let ok = !diags.iter().any(|d| d.severity == Severity::Error);
    Report { ok, diagnostics: diags, estimate, ffmpeg, design }
}

/// Task 1's rule (`tiers::safe_area_rects`): an explicit `output.safe_area`
/// naming an app measured for another aspect -- `reels`, `tiktok` or
/// `shorts` on 16:9 or 1:1, `youtube` on 9:16 or 4:5 -- has no rectangles,
/// which is worth saying (`safe_area_mismatch`).
fn warn_safe_area_mismatch(script: &Script, diags: &mut Vec<Diagnostic>) {
    use crate::story::tiers::{safe_area_rects, SafeArea};
    let format = script.format();
    let Some(area) = script.output.safe_area else { return };
    if area != SafeArea::None && safe_area_rects(area, format.aspect).is_empty() {
        let name = serde_json::to_value(area).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
        let aspect = serde_json::to_value(format.aspect).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
        diags.push(diag(
            Severity::Warning,
            "/output/safe_area",
            "safe_area_mismatch",
            format!("the `{name}` safe area is measured for another aspect than {aspect}: no safe-area rectangles apply"),
            Some("use the safe area of the app this aspect is for, or `none`"),
        ));
    }
}

/// `chapter_without_alt` (spec §5): each chapter whose resolved `alt` is
/// none -- `chapters::resolve` already fell back to the story's
/// `summary` and dropped blank text, so this is exactly what the
/// transcript and the manifest's coverage see. At the chapter
/// (`/chapters/<i>`), or at `/summary` for the implicit chapter of a
/// script without `chapters`.
fn warn_chapter_alt(timeline: &crate::story::timeline::Timeline, diags: &mut Vec<Diagnostic>) {
    for (i, chapter) in timeline.chapters().iter().enumerate() {
        if chapter.alt.is_some() {
            continue;
        }
        let (path, hint) = if chapter.implicit {
            ("/summary".to_string(), "describe the story in `summary`: the alt text of its still and its transcript")
        } else {
            (format!("/chapters/{i}"), "describe what the chapter shows in its `alt`, or the whole story in `summary`")
        };
        diags.push(diag(
            Severity::Warning,
            &path,
            "chapter_without_alt",
            format!("Chapter `{}` has no alt text for its still (no `alt` and no `summary`).", chapter.id),
            Some(hint),
        ));
    }
}

/// The frames [`warn_layout`] lays out, sorted and distinct.
///
/// - Per panel or image (`Timeline::element_spans`): its first frame, its
///   first fully-faded-in frame (`at + fade_s`), its midpoint and its last
///   frame ([`LAYOUT_SAMPLES_PER_ELEMENT`]). They do not move, and every
///   element is also laid out at every other one's samples, which covers
///   each change in what is on screen together.
/// - Per callout: every frame of its span, since its anchor moves with
///   the camera -- [`MAX_DENSE_CALLOUT_FRAMES`] in all, on an even stride
///   beyond that -- plus the frames either side of each `callout_changes`
///   time (its schedule's side flips and visibility changes).
/// - Per chapter its first frame and its still; per caption its
///   faded-in frame and its midpoint.
///
/// Only frames inside the span that asked for them count, so an element
/// shorter than a frame adds none.
pub fn layout_sample_frames(timeline: &crate::story::timeline::Timeline, fps: u32, frames: usize, callout_changes: &[f64]) -> Vec<usize> {
    let fps = f64::from(fps.max(1));
    let last = frames.saturating_sub(1);
    // The first frame at or after `t`.
    let at_or_after = |t: f64| (t * fps - 1e-9).ceil().max(0.0);
    let mut out = std::collections::BTreeSet::new();
    let mut push = |n: f64, from: f64, until: f64| {
        let t = n / fps;
        if n.is_finite() && t >= from - 1e-9 && t < until && n >= 0.0 {
            out.insert((n as usize).min(last));
        }
    };
    let callouts: Vec<(f64, f64)> = timeline.callout_plans().into_iter().map(|(_, at, until, _)| (at, until)).collect();
    for (_, at, until, fade_s) in timeline.element_spans() {
        let first = at_or_after(at);
        for n in [first, at_or_after(at + fade_s.max(0.0)), at_or_after((at + until) / 2.0), at_or_after(until) - 1.0] {
            push(n, at, until);
        }
    }
    // Every callout frame, on a stride once there are too many.
    let span_frames = |&(at, until): &(f64, f64)| (at_or_after(until) - at_or_after(at)).max(0.0);
    let dense: f64 = callouts.iter().map(span_frames).sum();
    let stride = (dense / MAX_DENSE_CALLOUT_FRAMES as f64).ceil().max(1.0);
    for &(at, until) in &callouts {
        let (first, end) = (at_or_after(at), at_or_after(until));
        let mut n = first;
        while n < end {
            push(n, at, until);
            n += stride;
        }
        push(end - 1.0, at, until);
    }
    for &t in callout_changes {
        if let Some(&(at, until)) = callouts.iter().find(|(at, until)| (*at..*until).contains(&t)) {
            let n = at_or_after(t);
            push(n - 1.0, at, until);
            push(n, at, until);
        }
    }
    for c in timeline.chapters() {
        let until = if c.to > c.from { c.to } else { c.from + 1.0 / fps };
        push(at_or_after(c.from), c.from, until);
        push(at_or_after(c.still_t), c.from, until.max(c.still_t + 1.0 / fps));
    }
    for (at, until, _) in timeline.caption_inputs() {
        push(at_or_after(at + crate::story::timeline::DEFAULT_FADE_S), at, until);
        push(at_or_after((at + until) / 2.0), at, until);
    }
    out.into_iter().collect()
}

/// What a layout warning's id names, as a pointer into the script: an
/// element's `/events/<i>` as it is; `caption/<k>` that frame's caption's
/// own event (`/events/<i>`) or `captions` cue (`/captions/<j>`);
/// `furniture/<piece>` the furniture in force at `t` (its chapter's
/// `/chapters/<i>/furniture`, or `/furniture`); `frame` (and anything
/// else) the `/output`.
fn layout_path(id: &str, frame: &crate::story::player::ProbedFrame, script: &Script, timeline: &crate::story::timeline::Timeline) -> String {
    if id.starts_with("/events/") {
        return id.to_string();
    }
    if let Some(k) = id.strip_prefix("caption/").and_then(|k| k.parse::<usize>().ok()) {
        return frame.caption_paths.get(k).cloned().unwrap_or_else(|| "/captions".to_string());
    }
    if id == "furniture" || id.starts_with("furniture/") {
        let chapters = timeline.chapters();
        let own = crate::story::chapter_at(chapters, frame.t)
            .and_then(|c| chapters.iter().position(|x| std::ptr::eq(x, c)))
            .filter(|&i| script.chapters.get(i).is_some_and(|c| c.furniture.is_some()));
        return own.map_or_else(|| "/furniture".to_string(), |i| format!("/chapters/{i}/furniture"));
    }
    // `frame` (`frame_too_small`), and any id the layout adds later: the
    // output is what the author can change.
    debug_assert!(id == "frame", "an unmapped layout id `{id}`");
    "/output".to_string()
}

/// The hint a layout warning carries in the report.
fn layout_hint(code: &str) -> Option<&'static str> {
    Some(match code {
        "text_shrunk" | "text_truncated" | "text_dropped" => "shorten the text, or use a placement with more room (`side`, `title`)",
        "in_safe_zone" => "move the element (its placement, or a callout's coordinate) away from the platform's UI, or choose another `output.safe_area`",
        "overlap" => "show fewer elements at once, or give them different placements",
        "callout_offscreen" => "check the callout's `at_coord`",
        "frame_too_small" => "use a larger frame or a smaller safe area",
        "scale_bar_hidden_pitch" => "lower the pitch where the scale bar matters",
        _ => return None,
    })
}

/// `source`'s themed core sublayer's own `line-width` (spec §6.3, R10):
/// `None` for a layer with no `design` or no role (declared or inferred).
/// The role itself resolves through [`crate::story::player::resolved_role`],
/// the SAME pure helper `player::themed_sublayers` calls (through a
/// `DesignCtx`) -- shared, not mirrored (R10 review round 1), so the two
/// cannot drift. `resolved_role` takes `tour` as a plain bool rather
/// than a [`DesignCtx`] for exactly this: `check_timeline` builds
/// `sources` before `path_and_timeline` resolves the camera path a
/// `DesignCtx` needs, so this runs too early to have one.
fn themed_core_width(script: &Script, source: &LayerSource) -> Option<f64> {
    let design = script.design.as_ref()?;
    let (archetype, _) = crate::story::design::archetype_for(script);
    let theme = design.theme.as_deref().and_then(crate::story::design::theme_named).unwrap_or_else(|| crate::story::design::theme_for(archetype));
    let accent = design.accent.as_deref().and_then(crate::story::card_layout::parse_hex).unwrap_or_else(|| crate::story::design::accent(theme));
    let tour = archetype == crate::story::design::Archetype::Tour;
    let (role, draws) = crate::story::player::resolved_role(script, source, tour)?;
    let legend = (role == "area").then(|| crate::story::card_layout::area_legend_for(script, &source.layer_id)).flatten();
    let subs = crate::story::theme_layers::expand(source, role, theme, accent, &source.paint, draws, legend.as_ref())?;
    let core = subs.iter().find(|s| s.id == source.layer_id)?;
    core.paint.get("line-width").and_then(serde_json::Value::as_f64)
}

/// `script`'s own screen span id (`events[<i>]`, or a chapter's own id)
/// as a JSON pointer to its card (spec §11, R10).
fn card_span_path(script: &Script, id: &str) -> String {
    if let Some(i) = id.strip_prefix("events[").and_then(|s| s.strip_suffix(']')).and_then(|s| s.parse::<usize>().ok()) {
        return format!("/events/{i}/card");
    }
    match script.chapters.iter().position(|c| c.id == id) {
        Some(i) => format!("/chapters/{i}/card"),
        None => "/chapters".to_string(),
    }
}

/// `card`'s own warnings (spec §11, R10), from a span's bare codes
/// ([`crate::story::card_layout::measure_card_at`]'s `warnings` carry no
/// detail of their own) and `card`'s own fields: `extra_supporting_item`,
/// `headline_ignored`, `supporting_item_deferred` and `card_too_tall`
/// name the ranked supporting item spec §3.2 chooses
/// ([`crate::story::card_layout::ranked_items`]); `too_many_stats`,
/// `legend_crowded` and `legend_too_many_items` read the chosen item's
/// own values. `text_shrunk`/`text_truncated` (1a codes too) are left
/// to the layout probe elsewhere: a card-level occurrence needs the row
/// a note belongs to, which `measure_card_at` does not expose yet.
fn warn_card_warnings(card: &crate::story::card_layout::CardContent, at: &str, map_share: f32, warnings: &[&'static str], diags: &mut Vec<Diagnostic>) {
    use crate::story::card_layout::{ranked_items, CardItem, MAX_STATS};
    let ranked = ranked_items(card);
    let mut extra_seen = 0usize;
    for code in warnings {
        match *code {
            "extra_supporting_item" => {
                if let (Some(a), Some(b)) = (ranked.first(), ranked.get(1 + extra_seen)) {
                    diags.push(diag(
                        Severity::Warning,
                        at,
                        "extra_supporting_item",
                        format!("This card has both {} and {}. Only the {} is shown; put the {} on another card.", a.name(), b.name(), a.name(), b.name()),
                        None,
                    ));
                }
                extra_seen += 1;
            }
            "headline_ignored" => diags.push(diag(
                Severity::Warning,
                at,
                "headline_ignored",
                "This card has a quote and a headline. The quote is shown; remove the headline or move it to another card.",
                None,
            )),
            "supporting_item_deferred" => {
                if let Some(item) = ranked.first() {
                    diags.push(diag(
                        Severity::Warning,
                        at,
                        "supporting_item_deferred",
                        format!("The headline needs three lines, so the {} is not shown. Shorten the headline or move the {} to the next card.", item.name(), item.name()),
                        None,
                    ));
                }
            }
            "card_too_tall" => {
                let msg = match ranked.first() {
                    Some(item) => format!(
                        "The card covers too much of the map ({map_share:.2} of the frame is left; aim for 0.75). Shorten the text or drop the {}.",
                        item.name()
                    ),
                    None => format!("The card covers too much of the map ({map_share:.2} of the frame is left; aim for 0.75). Shorten the text."),
                };
                diags.push(diag(Severity::Warning, at, "card_too_tall", msg, None));
            }
            "too_many_stats" => {
                if let Some(CardItem::Stats(stats)) = card.items.iter().find(|i| matches!(i, CardItem::Stats(_))) {
                    if let Some(dropped) = stats.get(MAX_STATS) {
                        diags.push(diag(
                            Severity::Warning,
                            at,
                            "too_many_stats",
                            format!("Only the first two stats fit on a card. Move \"{}\" to another card.", dropped.label),
                            None,
                        ));
                    }
                }
            }
            "legend_crowded" => {
                // Spec §11's literal wording ("Four", not the digit), and
                // the count the author actually wrote -- `rows.len()`,
                // before `shown_rows`' own fold, which `legend_too_many_items`
                // already covers for the same card past six (review round 1).
                if let Some(CardItem::Legend { rows, .. }) = card.items.iter().find(|i| matches!(i, CardItem::Legend { .. })) {
                    diags.push(diag(
                        Severity::Warning,
                        at,
                        "legend_crowded",
                        format!("A legend with {} items is drawn smaller. Four or fewer stay at full size.", rows.len()),
                        None,
                    ));
                }
            }
            "legend_too_many_items" => diags.push(diag(
                Severity::Warning,
                at,
                "legend_too_many_items",
                "Only six legend items fit. The rest are grouped as \"Other\".",
                None,
            )),
            _ => {}
        }
    }
}

/// Spec §11, §5.6 and D1 (R10): the design system's own report
/// (`Report::design`), `safe_area_not_chosen` with no chosen platform,
/// `accent_low_contrast` on a hard-to-read accent, and -- one card at a
/// time, over every screen span ([`DesignCtx::validation_cards`]) --
/// [`warn_card_warnings`]'s codes and `sources_too_long`. `None` without
/// a top-level `design`: a 1a story draws exactly as before and raises
/// none of these (`a_1a_story_reports_no_design_and_no_safe_area_info`).
fn warn_design(
    script: &Script,
    inputs: &SceneInputs,
    path: &crate::story::camera_path::CameraPath,
    timeline: &crate::story::timeline::Timeline,
    marks: &crate::story::RenderOptions,
    mapbox_sprite: bool,
    diags: &mut Vec<Diagnostic>,
) -> Option<DesignReport> {
    let design = DesignCtx::for_render(script, timeline.chapters(), path, inputs, marks, mapbox_sprite)?;
    let (archetype, rule) = crate::story::design::archetype_for(script);

    if script.output.safe_area.is_none() {
        diags.push(diag(
            Severity::Info,
            "/output/safe_area",
            "safe_area_not_chosen",
            "No platform is chosen, so the card sits near the bottom edge. Set output.safe_area to tiktok, reels, shorts or all to keep it above the app's buttons.",
            None,
        ));
    }

    // `accent_low_contrast` (spec §11): only `paper` draws the accent
    // unlifted; `ink` auto-lifts it to clear 4.5:1 (`design::lift_accent`),
    // a stricter bar than this 3:1 graphic minimum.
    if design.tone == crate::story::design::Tone::Paper {
        const WHITE: [u8; 3] = [0xFF, 0xFF, 0xFF];
        let ratio = crate::story::design::contrast(design.accent, WHITE);
        if ratio < 3.0 {
            let [r, g, b] = design.accent;
            diags.push(diag(
                Severity::Warning,
                "/design/accent",
                "accent_low_contrast",
                format!("The accent #{r:02X}{g:02X}{b:02X} is hard to see on the card ({ratio:.1}:1; 3:1 is the minimum). Pick a darker colour."),
                None,
            ));
        }
    }

    let output = script.output();
    let ratio = if output.pixel_ratio.is_finite() && output.pixel_ratio > 0.0 { output.pixel_ratio } else { 1.0 };
    let frame = crate::story::frame_layout::units((output.width, output.height), ratio);
    let mut fonts = crate::capture::story_chrome::LayoutFonts::new(inputs.label_fonts.clone());
    let credit = design.credit.clone();
    let cards = fonts.with_card_measure(ratio, |m| design.validation_cards(frame, credit.as_deref(), m));

    let mut map_share_min = 1.0_f32;
    let mut sources_too_long_reported = false;
    for (span, layout) in &cards {
        map_share_min = map_share_min.min(layout.map_share);
        let at = card_span_path(script, &span.id);
        warn_card_warnings(&span.content, &at, layout.map_share, &layout.warnings, diags);
        if !sources_too_long_reported && layout.warnings.contains(&"sources_too_long") {
            if let Some(group) = fonts.with_card_measure(ratio, |m| design.sources_too_long_group(frame, credit.as_deref(), Some(&span.content), m)) {
                diags.push(diag(
                    Severity::Warning,
                    &at,
                    "sources_too_long",
                    format!("The sources need more than two lines, so \"{group}\" is not shown. Shorten them."),
                    None,
                ));
                sources_too_long_reported = true;
            }
        }
    }

    Some(DesignReport {
        archetype: archetype.name().to_string(),
        rule,
        theme: design.theme.name().to_string(),
        tone: design.tone.name().to_string(),
        safe_area: design.safe_area.name().to_string(),
        map_share_min,
    })
}

/// Spec §11's `unknown_dir` (R14, mirroring `unknown_theme`): a
/// `design.dir` other than `ltr` or `rtl` would otherwise fall silently
/// back to `lang` ([`crate::story::cards::dir_for`]), which hides a typo.
/// A warning, not an error: the story still renders, in `lang`'s direction.
fn warn_unknown_dir(script: &Script, diags: &mut Vec<Diagnostic>) {
    let Some(dir) = script.design.as_ref().and_then(|d| d.dir.as_deref()) else { return };
    if !matches!(dir.trim().to_ascii_lowercase().as_str(), "ltr" | "rtl") {
        let shown: String = dir.chars().take(40).collect();
        diags.push(diag(
            Severity::Warning,
            "/design/dir",
            "unknown_dir",
            format!("There is no direction called \"{shown}\", so the story follows its lang. Use ltr or rtl."),
            None,
        ));
    }
}

/// Spec §11's `unknown_credits` (amended 2026-10-05, mirroring
/// `unknown_dir`): a `design.credits` other than `bottom` or
/// `above_platform` would otherwise fall silently back to `bottom`
/// ([`crate::story::frame_layout::CreditsPlacement::named`]). A warning:
/// the story still renders, its credits on the bottom row.
fn warn_unknown_credits(script: &Script, diags: &mut Vec<Diagnostic>) {
    let Some(credits) = script.design.as_ref().and_then(|d| d.credits.as_deref()) else { return };
    if crate::story::frame_layout::CreditsPlacement::parse(credits).is_none() {
        let shown: String = credits.chars().take(40).collect();
        diags.push(diag(
            Severity::Warning,
            "/design/credits",
            "unknown_credits",
            format!("There is no credits placement called \"{shown}\", so the credits sit on the bottom row. Use bottom or above_platform."),
            None,
        ));
    }
}

/// Spec §3.3's `index_without_tour` (R10): `"index": true` only counts
/// against a `numbered` layer, which only a `tour` archetype infers
/// (`theme_layers::infer_role`); elsewhere it has nothing to count
/// against. Independent of `design` (a 1a script's `card.index` is the
/// same field).
fn warn_index_without_tour(script: &Script, diags: &mut Vec<Diagnostic>) {
    if crate::story::design::archetype_for(script).0 == crate::story::design::Archetype::Tour {
        return;
    }
    let check = |card: &CardSpec, path: String, diags: &mut Vec<Diagnostic>| {
        if matches!(card.index, Some(IndexSpec::Auto(true))) {
            diags.push(diag(
                Severity::Warning,
                &format!("{path}/index"),
                "index_without_tour",
                "\"index\": true needs a numbered layer to count. Give the index as {\"i\": 3, \"n\": 5} instead.",
                None,
            ));
        }
    };
    for (i, c) in script.chapters.iter().enumerate() {
        if let Some(card) = &c.card {
            check(card, format!("/chapters/{i}/card"), diags);
        }
    }
    for (i, e) in script.events.iter().enumerate() {
        if let EventAction::Card(card) = &e.action {
            check(card, format!("/events/{i}/card"), diags);
        }
    }
}

/// Spec §11's rules for a validation message: a time prefix
/// (`at D.DD s: `) when there is one, then a sentence starting with a
/// capital letter and ending with a full stop, at most 200 characters
/// in all. `Err` names why not. A test-time lint
/// (`message_lint_passes_on_fired_messages`), not yet wired into `diag`
/// itself: most existing 1a messages were written before this rule and
/// still need auditing one by one (R10 follow-up).
#[cfg(test)]
fn message_lint(message: &str) -> Result<(), String> {
    if message.chars().count() > 200 {
        return Err(format!("{} characters, over 200: {message:?}", message.chars().count()));
    }
    let body = strip_time_prefix(message);
    if !body.ends_with('.') {
        return Err(format!("does not end with a full stop: {message:?}"));
    }
    match body.chars().next() {
        Some(c) if c.is_uppercase() || !c.is_alphabetic() => Ok(()),
        _ => Err(format!("does not start with a capital letter: {message:?}")),
    }
}

/// `message` past its leading `at D+.D+ s: ` time prefix, if it has one
/// (spec §11: "a time comes first when there is one").
#[cfg(test)]
fn strip_time_prefix(message: &str) -> &str {
    let Some(rest) = message.strip_prefix("at ") else { return message };
    let Some(after_digits) = rest.find(" s: ") else { return message };
    let digits = &rest[..after_digits];
    if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit() || c == '.') {
        &rest[after_digits + 4..]
    } else {
        message
    }
}

/// Spec §7's layout warnings (Task 19): `player::probe_layout` -- the
/// chrome pass's own layout over the shipped faces, no GPU -- on every
/// [`layout_sample_frames`] frame, each warning once per (path, code) at
/// its earliest frame, its message prefixed with that time. A
/// `scale_bar_hidden_pitch` is a note ([`Severity::Info`]); every other
/// code is a warning. Never an error.
#[allow(clippy::too_many_arguments)]
fn warn_layout(
    script: &Script,
    inputs: &SceneInputs,
    path: &crate::story::camera_path::CameraPath,
    timeline: &crate::story::timeline::Timeline,
    sources: &[LayerSource],
    images: &crate::story::media::ScriptImages,
    mapbox_sprite: bool,
    marks: &crate::story::RenderOptions,
    frames: usize,
    diags: &mut Vec<Diagnostic>,
) {
    let mut probe = crate::story::player::LayoutProbe::new(script, inputs, path, timeline, sources, images, mapbox_sprite, marks);
    let samples = layout_sample_frames(timeline, script.output().fps, frames, &probe.callout_changes());
    if samples.is_empty() {
        return;
    }
    let mut seen: Vec<(String, &'static str)> = Vec::new();
    for frame in probe.frames(&samples) {
        for w in &frame.warnings {
            let at = layout_path(&w.id, &frame, script, timeline);
            if seen.iter().any(|(p, c)| *p == at && *c == w.code) {
                continue;
            }
            seen.push((at.clone(), w.code));
            // The table says which layout codes are notes.
            let severity = WARNING_CODES.iter().find(|c| c.code == w.code).map_or(Severity::Warning, |c| c.severity);
            // The message names the layout's id; a caption's or a furniture
            // piece's is not a script path, so it names the path instead.
            let message = if at == w.id { w.message.clone() } else { w.message.replace(&format!("`{}`", w.id), &format!("`{at}`")) };
            diags.push(diag(severity, &at, w.code, format!("at {:.2} s: {message}", frame.t), layout_hint(w.code)));
        }
    }
}

/// Every overlay layer's own source, built exactly as `Player::new` builds
/// them (so `path_and_timeline`'s `follow_draw` resolution, and everything
/// below in [`check_timeline`], see what a render would actually draw),
/// plus every diagnostic Task 8 can raise from the document alone (spec
/// §3.3): `too_many_features` above [`MAX_OVERLAY_FEATURES_WARN`],
/// `unsupported_paint`, `line_metrics_too_large` above
/// `TILED_SOURCE_MIN_POINTS` (any whole-source layer, not only a
/// `line_metrics` one -- `Player::new`'s own `ensure!` is unconditional
/// too, review Finding 2), and `hairline` under [`HAIRLINE_WIDTH_PX`]. (A
/// feature far from its layer's centre is no longer refused: the overlay
/// grid reaches the whole world from any origin, `OverlayOrigin::coord_scale`.)
/// An overlay with no layers, or whose document did not load, is skipped
/// (already diagnosed while loading, or drawing nothing is not an error).
fn overlay_layer_sources(script: &Script, overlays: &HashMap<String, String>, diags: &mut Vec<Diagnostic>) -> Vec<LayerSource> {
    let mut out = Vec::new();
    for (i, overlay) in script.scene.overlays.iter().enumerate() {
        if overlay.layers.is_empty() {
            continue;
        }
        let doc = match overlay_doc(overlay, overlays) {
            Ok(Some(d)) => d,
            Ok(None) | Err(_) => continue,
        };
        let at = format!("/scene/overlays/{i}");
        let feature_count = document_feature_count(&doc);
        if feature_count > MAX_OVERLAY_FEATURES_WARN {
            diags.push(diag(
                Severity::Warning,
                &at,
                "too_many_features",
                format!(
                    "overlay `{}` has {feature_count} features (over {MAX_OVERLAY_FEATURES_WARN}): a web or mobile viewer may be slow",
                    overlay.id
                ),
                Some("split the overlay, or simplify/thin the geometry"),
            ));
        }
        let sources = match layer_sources(i, overlay, &doc) {
            Ok(s) => s,
            Err(d) => {
                diags.push(d);
                continue;
            }
        };
        for (li, source) in sources.iter().enumerate() {
            let layer_path = format!("{at}/layers/{li}");
            if !source.unsupported_paint.is_empty() {
                diags.push(diag(
                    Severity::Warning,
                    &layer_path,
                    "unsupported_paint",
                    format!(
                        "layer `{}` sets paint keys the story renderer does not draw: {}",
                        source.layer_id,
                        source.unsupported_paint.join(", ")
                    ),
                    None,
                ));
            }
            // Unconditional (review Finding 2): `Player::new`'s own
            // `ensure!` (player.rs) refuses ANY whole-source layer over
            // this limit, not only a `line_metrics` one, so a script that
            // is guaranteed to fail the instant a real render starts must
            // not validate `ok: true`. Same constant, same message style.
            let layer_doc: serde_json::Value = serde_json::from_str(&source.geojson).unwrap_or(serde_json::Value::Null);
            let points = line_and_polygon_points(&layer_doc);
            if points >= crate::overlay::tile_plan::TILED_SOURCE_MIN_POINTS {
                diags.push(diag(
                    Severity::Error,
                    &layer_path,
                    "line_metrics_too_large",
                    format!(
                        "overlay `{}` layer `{}` has {points} line/polygon points, over the whole-source limit of {}; \
                         simplify the geometry or split the layer",
                        overlay.id,
                        source.layer_id,
                        crate::overlay::tile_plan::TILED_SOURCE_MIN_POINTS
                    ),
                    Some("simplify the geometry or split the layer"),
                ));
            }
            if source.kind == OverlayLayerKind::Line {
                // A themed layer's drawn width is the theme's (spec §6.3),
                // not the author's own `line-width`: the two can disagree
                // either way (a thin author width the theme overrides to
                // something wider, or none at all where the theme always
                // gives one), so this checks the SAME width
                // `theme_layers::expand` draws (`themed_core_width`), with
                // the author's declared width a fallback for an unthemed
                // layer (R10).
                let width = themed_core_width(script, source).or_else(|| source.paint.get("line-width").and_then(serde_json::Value::as_f64));
                if let Some(width) = width {
                    if width < HAIRLINE_WIDTH_PX {
                        diags.push(diag(
                            Severity::Warning,
                            &layer_path,
                            "hairline",
                            format!(
                                "layer `{}`'s line-width is {width} at 1080-referenced units, under {HAIRLINE_WIDTH_PX}: it may not be visible",
                                source.layer_id
                            ),
                            Some("raise line-width, or accept a thin line intentionally"),
                        ));
                    }
                }
            }
        }
        out.extend(sources);
    }
    out
}

/// Task 7 review #10: a `highlight` targeting a layer with `line-gradient`
/// never shows its colour (the gradient overwrites it; only the dimming
/// shows). Needs the timeline's effect plans, so it runs after
/// `path_and_timeline` builds them from [`overlay_layer_sources`]'s output.
fn warn_highlight_over_gradient(script: &Script, sources: &[LayerSource], timeline: &crate::story::timeline::Timeline, diags: &mut Vec<Diagnostic>) {
    for source in sources {
        if source.paint.get("line-gradient").is_none() {
            continue;
        }
        let highlighted =
            timeline.effect_plans().iter().any(|p| p.layer_id == source.layer_id && matches!(p.kind, EffectKind::Highlight));
        if !highlighted {
            continue;
        }
        let path = script
            .scene
            .overlays
            .iter()
            .position(|o| o.id == source.overlay_id)
            .and_then(|oi| script.scene.overlays.get(oi).map(|o| (oi, o)))
            .and_then(|(oi, o)| o.layers.iter().position(|l| l.id == source.layer_id).map(|li| format!("/scene/overlays/{oi}/layers/{li}")))
            .unwrap_or_else(|| "/scene/overlays".to_string());
        diags.push(diag(
            Severity::Warning,
            &path,
            "highlight_over_gradient",
            format!(
                "layer `{}` has both a `line-gradient` paint and a `highlight` effect: the gradient replaces the highlight \
                 colour, so only the dimming shows",
                source.layer_id
            ),
            Some("remove line-gradient, or drop the highlight and use draw/pulse instead"),
        ));
    }
}

/// Resolve the camera path and the timeline, check the style edits and
/// plan every frame's tiles for the estimate.
/// Spec §4.5: a legend over a layer whose colour is an expression the
/// legend cannot list (`legend_unsupported`, a warning), for the story's
/// furniture and each chapter's own, at that legend's path.
fn warn_legends(script: &Script, sources: &[LayerSource], diags: &mut Vec<Diagnostic>) {
    let legends = std::iter::once(("/furniture".to_string(), script.furniture.legend.as_ref()))
        .chain(script.chapters.iter().enumerate().map(|(i, c)| (format!("/chapters/{i}/furniture"), c.furniture.as_ref().and_then(|f| f.legend.as_ref()))));
    for (root, legend) in legends {
        let Some(spec) = legend else { continue };
        for mut d in crate::story::furniture::legend(spec, sources).1 {
            if let Some(rest) = d.path.strip_prefix("/furniture") {
                d.path = format!("{root}{rest}");
            }
            diags.push(d);
        }
    }
}

/// Spec §6.3's silent authoring traps around an `area`'s legend, in a
/// design story (R9 review): the property its fill reads matches nothing
/// (`area_property_unmatched`, `area_property_not_numeric`); a legend's
/// own values replaced by an earlier one's (`legend_overridden`, both
/// named); and fields that change nothing (`legend_field_unused`, info).
fn warn_area_legends(script: &Script, sources: &[LayerSource], diags: &mut Vec<Diagnostic>) {
    use crate::story::card_layout::{area_legend_origin, card_legends_naming, is_area_layer, AreaLegend, AreaScale};
    if script.design.is_none() {
        return;
    }
    let sequential = |l: &crate::story::script::CardLegendSpec| l.kind.as_deref().map_or(l.stops.is_some() && l.items.is_none(), |k| k.trim() == "sequential");
    let mut unused = |path: String, field: &str, why: &str, diags: &mut Vec<Diagnostic>| {
        diags.push(diag(Severity::Info, &format!("{path}/{field}"), "legend_field_unused", format!("This legend's \"{field}\" changes nothing: {why}."), None));
    };
    let unused_in = |path: &str, l: &crate::story::script::CardLegendSpec, diags: &mut Vec<Diagnostic>, unused: &mut dyn FnMut(String, &str, &str, &mut Vec<Diagnostic>)| {
        if sequential(l) && l.ramp.is_some() {
            unused(path.to_string(), "ramp", "a sequential legend is already one ramp", diags);
        } else if !sequential(l) && l.stops.is_some() && l.ramp.is_none() {
            unused(path.to_string(), "stops", "a categorical legend takes its colours from its items unless it sets \"ramp\": \"sequential\"", diags);
        }
    };
    let layers: Vec<(usize, usize, &crate::story::script::OverlayLayerSpec)> =
        script.scene.overlays.iter().enumerate().flat_map(|(oi, o)| o.layers.iter().enumerate().map(move |(li, l)| (oi, li, l))).collect();
    let is_area = |id: &str| layers.iter().any(|(_, _, l)| l.id == id && is_area_layer(l));
    // Every card legend's fields.
    let cards = script.chapters.iter().enumerate().filter_map(|(i, c)| Some((format!("/chapters/{i}/card/legend"), c.card.as_ref()?.legend.as_deref()?)));
    let events = script.events.iter().enumerate().filter_map(|(i, e)| match &e.action {
        EventAction::Card(spec) => Some((format!("/events/{i}/card/legend"), spec.legend.as_deref()?)),
        _ => None,
    });
    for (path, l) in cards.chain(events) {
        unused_in(&path, l, diags, &mut unused);
        if l.property.is_some() && !l.layers.iter().flatten().any(|id| is_area(id)) {
            unused(path, "property", "this legend names no area layer, so no fill reads it", diags);
        }
    }
    for &(oi, li, layer) in &layers {
        let Some(own) = layer.legend.as_deref() else { continue };
        let path = format!("/scene/overlays/{oi}/layers/{li}/legend");
        if !is_area_layer(layer) {
            diags.push(diag(
                Severity::Info,
                &path,
                "legend_field_unused",
                format!("Layer \"{}\" is not an area, so its \"legend\" changes nothing. Give it role \"area\" or remove the legend.", layer.id),
                None,
            ));
            continue;
        }
        unused_in(&path, own, diags, &mut unused);
    }
    for &(oi, li, layer) in &layers {
        let Some((chosen, origin_path, origin)) = area_legend_origin(script, &layer.id) else { continue };
        // Legends with values of their own that lose to `chosen`.
        let mut losers: Vec<(String, String)> = card_legends_naming(script, &layer.id)
            .into_iter()
            .filter(|(path, _, l)| *path != origin_path && AreaLegend::from_spec(l).is_some())
            .map(|(path, name, _)| (path, name))
            .collect();
        let own_path = format!("/scene/overlays/{oi}/layers/{li}/legend");
        if own_path != origin_path && layer.legend.as_deref().and_then(AreaLegend::from_spec).is_some() {
            losers.push((own_path, format!("layer \"{}\"", layer.id)));
        }
        for (path, name) in losers {
            diags.push(diag(
                Severity::Warning,
                &path,
                "legend_overridden",
                format!(
                    "The legend on {name} lists its own values for layer \"{}\", but {origin} already gives that layer its values, so {origin}'s are shown on every card and on the map. Remove the values from {name}, or move them to {origin}.",
                    layer.id
                ),
                None,
            ));
        }
        let Some(source) = sources.iter().find(|s| s.layer_id == layer.id) else { continue };
        let doc: serde_json::Value = serde_json::from_str(&source.geojson).unwrap_or(serde_json::Value::Null);
        let features = doc.get("features").and_then(serde_json::Value::as_array).cloned().unwrap_or_default();
        if features.is_empty() {
            continue;
        }
        let prop = |f: &serde_json::Value| f.get("properties").and_then(|p| p.get(&chosen.property)).cloned();
        match &chosen.scale {
            AreaScale::Categorical { rows } => {
                let text = |v: serde_json::Value| match v {
                    serde_json::Value::String(s) => Some(s),
                    serde_json::Value::Number(n) => Some(n.to_string()),
                    serde_json::Value::Bool(b) => Some(b.to_string()),
                    _ => None,
                };
                if !features.iter().filter_map(|f| prop(f).and_then(text)).any(|v| rows.iter().any(|r| r.label == v)) {
                    diags.push(diag(
                        Severity::Warning,
                        &origin_path,
                        "area_property_unmatched",
                        format!(
                            "No feature of layer \"{}\" has a \"{}\" equal to one of the legend's labels, so every area is drawn as \"Other\". Set the legend's \"property\" to the feature property that holds the classes, or make the labels match its values.",
                            layer.id, chosen.property
                        ),
                        None,
                    ));
                }
            }
            AreaScale::Sequential { .. } => {
                if crate::story::theme_layers::property_range(&source.geojson, &chosen.property).is_none() {
                    diags.push(diag(
                        Severity::Warning,
                        &origin_path,
                        "area_property_not_numeric",
                        format!(
                            "No feature of layer \"{}\" has a number in \"{}\", so the area is drawn in one colour. Set the legend's \"property\" to a numeric feature property.",
                            layer.id, chosen.property
                        ),
                        None,
                    ));
                }
            }
        }
    }
}

/// The RGBA bytes of every image at the size a `long_edge` frame loads it
/// (`media::load_script_images` caps the long edge at the frame's), plus
/// a third for the mip chain uploaded with it.
fn image_gpu_bytes(sizes: &[(u32, u32)], long_edge: u32) -> u64 {
    sizes
        .iter()
        .map(|&(w, h)| {
            let long = w.max(h).max(1);
            let scale = (f64::from(long_edge) / f64::from(long)).min(1.0);
            let (w, h) = ((f64::from(w) * scale).ceil(), (f64::from(h) * scale).ceil());
            (w * h * 4.0 * 4.0 / 3.0) as u64
        })
        .sum()
}

/// Spec §7's GPU memory warning for the chosen output: over
/// [`GPU_BUDGET_BYTES`], a side over the capture device's
/// [`capture_max_texture_dimension_2d`], or a readback over its
/// [`capture_max_buffer_size`]. Conservative, and never an error.
fn warn_gpu_memory(width: u32, height: u32, gpu: u64, diags: &mut Vec<Diagnostic>) {
    let max_side = capture_max_texture_dimension_2d();
    if width > max_side || height > max_side {
        diags.push(diag(
            Severity::Warning,
            "/output",
            "gpu_memory",
            format!("the {width}x{height} frame is wider or taller than the capture device's {max_side}-pixel texture limit: the render will be refused"),
            Some("choose a tier up to 4k, or a smaller width/height"),
        ));
    }
    let readback = u64::from(width * 4).next_multiple_of(256) * u64::from(height);
    if readback > capture_max_buffer_size() {
        diags.push(diag(
            Severity::Warning,
            "/output",
            "gpu_memory",
            format!("one {width}x{height} frame's readback ({} MB) exceeds the capture device's largest buffer ({} MB)", readback / 1_000_000, capture_max_buffer_size() / 1_000_000),
            Some("choose a smaller tier"),
        ));
    }
    if gpu > GPU_BUDGET_BYTES {
        diags.push(diag(
            Severity::Warning,
            "/output",
            "gpu_memory",
            format!(
                "the render may need ~{} MB of GPU memory (frame buffers, atlases, images and the mesh cache), over the {} MB budget",
                gpu / 1_000_000,
                GPU_BUDGET_BYTES / 1_000_000
            ),
            Some("choose a smaller tier, or use fewer or smaller images"),
        ));
    }
}

#[allow(clippy::too_many_arguments)]
fn check_timeline(
    script: &Script,
    mut inputs: SceneInputs,
    images: &crate::story::media::ScriptImages,
    image_sizes: &[(u32, u32)],
    estimate: bool,
    marks: &crate::story::RenderOptions,
    diags: &mut Vec<Diagnostic>,
    design_out: &mut Option<DesignReport>,
) -> Option<Estimate> {
    let output = script.output();
    let ratio = if output.pixel_ratio.is_finite() && output.pixel_ratio > 0.0 { output.pixel_ratio } else { 1.0 };
    let viewport = (output.width as f32 / ratio, output.height as f32 / ratio);
    let tracks = Arc::new(std::mem::take(&mut inputs.tracks));
    // Judged while the style is intact, as `Player::new` does.
    let mapbox_sprite = crate::story::burn_in::style_has_mapbox_sprite(&inputs.base_style);
    let base_style = std::mem::take(&mut inputs.base_style);
    // Every overlay layer's own source, exactly as `Player::new` builds
    // them (Task 8): `path_and_timeline` needs them for `follow_draw`, and
    // this function's own per-layer checks below read them too, so
    // validation sees what a render would actually draw.
    let sources = overlay_layer_sources(script, &inputs.overlays, diags);
    warn_legends(script, &sources, diags);
    warn_area_legends(script, &sources, diags);
    let (path, timeline) = match crate::story::timeline::path_and_timeline(viewport, script, tracks, &sources, base_style, &*inputs.load_style) {
        Ok(built) => built,
        Err((_, d)) => {
            diags.extend(d);
            return None;
        }
    };
    // The render's camera pass (spec §5.3): the poses the probe and the
    // estimate read are the video's.
    let path = match crate::story::player::render_camera_path(script, &inputs, marks, mapbox_sprite, path, &timeline) {
        Ok(path) => path,
        Err(d) => {
            diags.extend(d);
            return None;
        }
    };
    diags.extend(crate::story::style_cache::validate_style_edits(&timeline));
    // One card at a time (spec §10.1): `cards_overlap`.
    diags.extend(crate::story::cards::card_spans_in(script, timeline.chapters(), &path).1);
    warn_highlight_over_gradient(script, &sources, &timeline, diags);

    let fps = output.fps.max(1);
    let duration = path.duration();
    let f = (duration * f64::from(fps) - 1e-9).ceil();
    let frames = if f.is_finite() && f >= 1.0 { f as usize } else { 1 };
    warn_chapter_alt(&timeline, diags);
    warn_layout(script, &inputs, &path, &timeline, &sources, images, mapbox_sprite, marks, frames, diags);
    *design_out = warn_design(script, &inputs, &path, &timeline, marks, mapbox_sprite, diags);
    let mesh = crate::story::player::DEFAULT_MESH_CACHE_BYTES;
    let gpu = gpu_bytes(output.width, output.height, image_gpu_bytes(image_sizes, output.width.max(output.height)), mesh);
    warn_gpu_memory(output.width, output.height, gpu, diags);
    if !estimate {
        return None;
    }
    // Each frame's camera and globe, once (`pose_at` and `state_at` are
    // the plan's dearest part, and every tier reads the same ones).
    let cams: Vec<(crate::render::Camera, bool)> = (0..frames)
        .map(|n| {
            let t = n as f64 / f64::from(fps);
            let pose = path.pose_at(t);
            (pose.camera, timeline.globe_at(t).unwrap_or(pose.globe))
        })
        .collect();
    // The cover of each `stride`-th frame from its pose and scene alone:
    // the player's request with only the fields `cover()` reads filled in.
    let cams = &cams;
    let requests = |width: u32, height: u32, pixel_ratio: f32, stride: usize| {
        cams.iter().step_by(stride.max(1)).map(move |(c, globe)| CaptureRequest {
            center_lon: c.center_lon,
            center_lat: c.center_lat,
            zoom: c.zoom,
            pitch: c.pitch,
            bearing: c.bearing,
            width,
            height,
            pixel_ratio,
            flat: true,
            globe: *globe,
            flat_tile_zoom_max: inputs.flat_tile_zoom_max,
            stand_in_levels: Some(STAND_IN_LEVELS),
            basemap_compact: BASEMAP_COMPACT,
            basemap_compact_strokes: Some(BASEMAP_COMPACT_STROKES),
            ..CaptureRequest::default()
        })
    };
    // The chosen output, every frame: its tile count and `max_cover` are
    // exact. The other tiers on [`ESTIMATE_TIER_STRIDE_S`]: a tier's
    // distinct tiles change slowly frame to frame, and an estimate need
    // not be exact.
    let tile_plan = plan(requests(output.width, output.height, output.pixel_ratio, 1));
    let tier_stride = ((ESTIMATE_TIER_STRIDE_S * f64::from(fps)).round() as usize).max(1);
    if tile_plan.max_cover > MAX_COVER_WARN {
        diags.push(diag(
            Severity::Warning,
            "/camera",
            "cover_too_large",
            format!("one frame covers {} tiles (over {MAX_COVER_WARN}): slow and memory-hungry", tile_plan.max_cover),
            Some("lower the pitch or raise the zoom where the camera looks at the horizon"),
        ));
    }
    let aspect = script.format().aspect;
    let tiers = [Tier::Hd, Tier::Fullhd, Tier::Qhd, Tier::Uhd]
        .into_iter()
        .map(|tier| {
            let (width, height) = frame_size(aspect, tier);
            let ratio = pixel_ratio(tier);
            // The chosen output's plan is already made.
            let tiles = if (width, height) == (output.width, output.height) && ratio == output.pixel_ratio {
                tile_plan.total_tiles
            } else {
                plan(requests(width, height, ratio, tier_stride)).total_tiles
            };
            let kbps = crate::story::tiers::bitrate_kbps(tier, fps);
            TierEstimate {
                tier,
                width,
                height,
                wall_s_est: Some(render_seconds(frames, width, height)),
                tiles,
                bytes: video_bytes_bound(kbps, duration),
                gpu_bytes: gpu_bytes(width, height, image_gpu_bytes(image_sizes, width.max(height)), mesh),
            }
        })
        .collect();
    Some(Estimate {
        duration_s: duration,
        frames,
        tiles: tile_plan.total_tiles,
        max_cover: tile_plan.max_cover,
        bytes: tile_plan.total_tiles as u64 * ESTIMATE_BYTES_PER_TILE,
        wall_s_est: Some(render_seconds(frames, output.width, output.height)),
        gpu_bytes: gpu,
        tiers,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// `warn_area_legends` over a design story with one area layer
    /// `zones` (its own legend `layer_legend`) on `features`, and cards
    /// with `card_legends`: the codes it raises, with their paths.
    fn area_warnings(layer_legend: serde_json::Value, card_legends: &[serde_json::Value], features: serde_json::Value) -> Vec<(&'static str, String, String)> {
        let doc = serde_json::json!({"type": "FeatureCollection", "features": features});
        let mut layer = serde_json::json!({"id": "zones", "type": "fill", "role": "area"});
        if !layer_legend.is_null() {
            layer["legend"] = layer_legend;
        }
        let chapters: Vec<serde_json::Value> = card_legends
            .iter()
            .enumerate()
            .map(|(i, l)| serde_json::json!({"id": format!("c{i}"), "title": "C", "from": i as f64, "to": i as f64 + 1.0, "card": {"headline": "H", "legend": l}}))
            .collect();
        let v = crate::story::test_util::minimal_json(serde_json::json!({
            "design": {},
            "scene": {"style": "s.json", "overlays": [{"id": "o", "data": doc.clone(), "layers": [layer]},
                {"id": "lines", "data": {"type": "FeatureCollection", "features": []}, "layers": [{"id": "road", "type": "line", "legend": {"items": [{"label": "x"}]}}]}]},
            "chapters": chapters
        }));
        let script = crate::story::parse(&serde_json::to_vec(&v).unwrap()).expect("parses");
        let sources = layer_sources(0, &script.scene.overlays[0], &doc).unwrap();
        let mut diags = Vec::new();
        warn_area_legends(&script, &sources, &mut diags);
        diags.into_iter().map(|d| (d.code, d.path, d.message)).collect()
    }

    fn poly(props: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"type": "Feature", "properties": props, "geometry": {"type": "Polygon", "coordinates": [[[0, 0], [1, 0], [1, 1], [0, 0]]]}})
    }

    /// R9 review: a categorical `property` no feature matches would draw an
    /// all-"Other" map -- warned; one match is enough.
    #[test]
    fn a_categorical_property_matching_no_label_warns() {
        let legend = serde_json::json!({"property": "kind", "items": [{"label": "Park"}, {"label": "Port"}]});
        let codes = |features| area_warnings(legend.clone(), &[], features).into_iter().map(|(c, p, _)| (c, p)).collect::<Vec<_>>();
        let unmatched = codes(serde_json::json!([poly(serde_json::json!({"kind": "park"})), poly(serde_json::json!({"type": "Park"}))]));
        assert!(unmatched.contains(&("area_property_unmatched", "/scene/overlays/0/layers/0/legend".into())), "{unmatched:?}");
        let ok = codes(serde_json::json!([poly(serde_json::json!({"kind": "Port"})), poly(serde_json::json!({"kind": "x"}))]));
        assert!(!ok.iter().any(|(c, _)| *c == "area_property_unmatched"), "{ok:?}");
    }

    /// R9 review: a sequential area whose `property` is missing or not a
    /// number in every feature draws one colour -- warned.
    #[test]
    fn a_sequential_property_with_no_number_warns() {
        let legend = serde_json::json!({"type": "sequential", "property": "pop"});
        let got = area_warnings(legend.clone(), &[], serde_json::json!([poly(serde_json::json!({"pop": "many"})), poly(serde_json::json!({}))]));
        assert!(got.iter().any(|(c, _, m)| *c == "area_property_not_numeric" && m.contains("\"pop\"")), "{got:?}");
        let ok = area_warnings(legend, &[], serde_json::json!([poly(serde_json::json!({"pop": 3}))]));
        assert!(!ok.iter().any(|(c, _, _)| *c == "area_property_not_numeric"), "{ok:?}");
    }

    /// R9 review: a card's own values replaced by an earlier legend's are
    /// warned at the replaced card, naming both cards (spec §11 wording).
    #[test]
    fn a_card_legend_replaced_by_an_earlier_one_warns_naming_both() {
        let first = serde_json::json!({"layers": ["zones"], "property": "kind", "items": [{"label": "Park"}]});
        let second = serde_json::json!({"layers": ["zones"], "property": "kind", "items": [{"label": "Port"}]});
        let shows_only = serde_json::json!({"layers": ["zones"]});
        let got = area_warnings(serde_json::Value::Null, &[first, shows_only, second], serde_json::json!([poly(serde_json::json!({"kind": "Park"}))]));
        let over: Vec<_> = got.iter().filter(|(c, _, _)| *c == "legend_overridden").collect();
        assert_eq!(over.len(), 1, "only the card with values of its own: {got:?}");
        assert_eq!(over[0].1, "/chapters/2/card/legend");
        assert!(over[0].2.contains("chapter \"c2\"") && over[0].2.contains("chapter \"c0\""), "{}", over[0].2);
        // A layer's own legend under a card's values: warned at the layer.
        let layer_own = serde_json::json!({"property": "kind", "items": [{"label": "Park"}]});
        let card = serde_json::json!({"layers": ["zones"], "property": "kind", "items": [{"label": "Park"}]});
        let got = area_warnings(layer_own, &[card], serde_json::json!([poly(serde_json::json!({"kind": "Park"}))]));
        assert!(got.iter().any(|(c, p, _)| *c == "legend_overridden" && p == "/scene/overlays/0/layers/0/legend"), "{got:?}");
    }

    /// R9 review: legend fields that change nothing are noted (info).
    #[test]
    fn unused_legend_fields_are_noted() {
        let seq_with_ramp = serde_json::json!({"type": "sequential", "ramp": "sequential", "property": "pop"});
        let cat_with_stops = serde_json::json!({"layers": ["zones"], "property": "pop", "stops": ["#000000"], "items": [{"label": "a"}]});
        let no_area = serde_json::json!({"layers": ["road"], "property": "x", "items": [{"label": "a"}]});
        let got = area_warnings(seq_with_ramp, &[cat_with_stops, no_area], serde_json::json!([poly(serde_json::json!({"pop": 1}))]));
        let notes: Vec<&str> = got.iter().filter(|(c, _, _)| *c == "legend_field_unused").map(|(_, p, _)| p.as_str()).collect();
        assert!(notes.contains(&"/chapters/0/card/legend/stops"), "{notes:?}");
        assert!(notes.contains(&"/chapters/1/card/legend/property"), "{notes:?}");
        assert!(notes.contains(&"/scene/overlays/1/layers/0/legend"), "a legend on a line layer: {notes:?}");
        // The layer's own sequential legend loses to the card's values, so
        // its `ramp` is not reached by the per-layer note; checked alone:
        let alone = area_warnings(serde_json::json!({"type": "sequential", "ramp": "sequential", "property": "pop"}), &[], serde_json::json!([poly(serde_json::json!({"pop": 1}))]));
        assert!(alone.iter().any(|(c, p, _)| *c == "legend_field_unused" && p == "/scene/overlays/0/layers/0/legend/ramp"), "{alone:?}");
        assert!(WARNING_CODES.iter().any(|w| w.code == "legend_field_unused" && w.severity == Severity::Info));
    }

    fn root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    /// Review F1, through the scene loader: the gap line for a sheet that
    /// did not load names no secret.
    #[test]
    fn a_sprite_gap_line_names_no_secret() {
        let url = "http://user:SECRET@127.0.0.1:9/sprite?key=SECRET&access_token=SECRET";
        let style = serde_json::json!({"sprite": url});
        let notes = scene_sprites(&style, &[], "openfreemap", true, None).notes;
        assert!(notes.len() == 1 && !notes[0].contains("SECRET"), "{notes:?}");
    }

    /// Review F7: an array-form `"sprite"`, and a `set_style` naming a
    /// different sheet, each leave a gap line rather than losing icons
    /// silently.
    #[test]
    fn an_array_sprite_and_a_set_style_sheet_swap_are_named_in_the_gaps() {
        let array = serde_json::json!({"sprite": [{"id": "default", "url": "https://h.example/s"}]});
        let out = scene_sprites(&array, &[], "openfreemap", true, None);
        assert!(out.atlas.is_none());
        assert!(out.notes.iter().any(|n| n.contains("array")), "{:?}", out.notes);

        let plain = serde_json::json!({});
        let other = serde_json::json!({"sprite": "https://h.example/other"});
        let same = serde_json::json!({});
        let out = scene_sprites(&plain, &[("b.json".into(), Some(other)), ("c.json".into(), Some(same))], "openfreemap", true, None);
        assert_eq!(out.notes.len(), 1, "{:?}", out.notes);
        assert!(out.notes[0].contains("set_style b.json"), "{:?}", out.notes);
        assert!(scene_sprites(&array, &[], "fixture", true, None).notes.is_empty(), "the fixture stays silent and hermetic");
    }

    /// Ruling (Task 17/18): the wordmark is the user's file, PNG or SVG;
    /// an SVG is rasterised at the pinned height with its aspect kept and
    /// straight alpha, and a file that does not load is a scene error.
    #[test]
    fn a_user_wordmark_loads_from_png_or_svg() {
        let dir = tempfile::tempdir().unwrap();
        let svg = dir.path().join("wm.svg");
        std::fs::write(
            &svg,
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="10"><rect width="40" height="10" fill="#ff0000" fill-opacity="0.5"/></svg>"##,
        )
        .unwrap();
        let img = load_wordmark(&svg).unwrap();
        assert_eq!(img.height(), WORDMARK_SVG_HEIGHT_PX as u32);
        assert_eq!(img.width(), 4 * img.height());
        let px = img.get_pixel(img.width() / 2, img.height() / 2).0;
        assert!(px[0] >= 250 && px[1] == 0 && (126..=129).contains(&px[3]), "straight alpha: {px:?}");
        let png = dir.path().join("wm.png");
        image::RgbaImage::from_pixel(30, 6, image::Rgba([1, 2, 3, 255])).save(&png).unwrap();
        assert_eq!(load_wordmark(&png).unwrap().dimensions(), (30, 6));
        assert!(load_wordmark(&dir.path().join("missing.png")).is_err());
        // Through the scene loader: named and loaded, or an error at its path.
        let mut v = base(serde_json::json!([]));
        v["scene"]["basemap"] = serde_json::json!({"source": "fixture", "wordmark": svg});
        let (script, _) = script_of(&v);
        let inputs = scene_inputs(&script, &root(), true).unwrap();
        assert!(inputs.wordmark.is_some());
        v["scene"]["basemap"]["wordmark"] = serde_json::json!(dir.path().join("missing.svg"));
        let (script, _) = script_of(&v);
        let e = scene_inputs(&script, &root(), true).err().unwrap();
        assert!(format!("{e:#}").contains("wordmark_unreadable"), "{e:#}");
    }

    fn script_of(v: &serde_json::Value) -> (Script, Vec<u8>) {
        let raw = serde_json::to_vec(v).unwrap();
        (crate::story::parse(&raw).unwrap(), raw)
    }

    fn base(events: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"story":1,"output":{"width":480,"height":270,"fps":10},
          "scene":{"style":"examples/liberty-style.json","basemap":{"source":"fixture"}},
          "camera":[{"type":"jump","to":{"lon":103.851,"lat":1.2905,"zoom":14}},{"type":"orbit","degrees":30,"duration":1.0}],
          "events": events})
    }

    fn opts() -> ValidateOptions {
        ValidateOptions { offline: true, base_dir: root(), image_cache_dir: None, estimate: true, ..ValidateOptions::default() }
    }

    fn codes(r: &Report) -> Vec<&'static str> {
        r.diagnostics.iter().map(|d| d.code).collect()
    }

    /// A design story (`"design": {}`), 9:16 hd `reels` (so
    /// `safe_area_not_chosen` never fires here by accident), one chapter
    /// over the whole 2 s story with `card` (R10's test battery).
    fn design_base(card: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"story":1,"design":{},"output":{"aspect":"9:16","tier":"hd","safe_area":"reels","fps":10},
          "scene":{"style":"examples/liberty-style.json","basemap":{"source":"fixture"}},
          "camera":[{"type":"jump","to":{"lon":103.851,"lat":1.2905,"zoom":14}},{"type":"hold","duration":2.0}],
          "chapters":[{"id":"c0","title":"C","from":0.0,"to":2.0,"card":card}]})
    }

    /// Spec §11, R10: each new card-level code fires on a minimal design
    /// story, with the chapter's own path.
    #[test]
    fn extra_supporting_item_and_headline_ignored_fire() {
        let card = serde_json::json!({"headline": "H", "stats": [{"label": "A", "value": "1"}], "legend": {"items": [{"label": "x"}]}});
        let r = run(&design_base(card));
        let w = find(&r, "extra_supporting_item");
        assert_eq!(w.len(), 1, "{:?}", r.diagnostics);
        assert_eq!(w[0].path, "/chapters/0/card");
        assert!(w[0].message.contains("legend") && w[0].message.contains("stats"), "{}", w[0].message);
        assert!(message_lint(&w[0].message).is_ok(), "{}", w[0].message);

        let card = serde_json::json!({"headline": "H", "quote": {"text": "Q"}});
        let r = run(&design_base(card));
        let w = find(&r, "headline_ignored");
        assert_eq!(w.len(), 1, "{:?}", r.diagnostics);
        assert_eq!(w[0].path, "/chapters/0/card");
        assert!(message_lint(&w[0].message).is_ok(), "{}", w[0].message);
    }

    #[test]
    fn supporting_item_deferred_and_card_too_tall_fire() {
        let card = serde_json::json!({"headline": long_headline(), "stats": [{"label": "A", "value": "1"}]});
        let r = run(&design_base(card));
        let w = find(&r, "supporting_item_deferred");
        assert_eq!(w.len(), 1, "{:?}", r.diagnostics);
        assert!(w[0].message.contains("stats"), "{}", w[0].message);
        assert!(message_lint(&w[0].message).is_ok(), "{}", w[0].message);

        let card = serde_json::json!({
            "headline": long_headline(), "body": "body ".repeat(40),
            "legend": {"title": "t", "items": [{"label":"c0","value":"1"},{"label":"c1","value":"1"},{"label":"c2","value":"1"},{"label":"c3","value":"1"}]}
        });
        // With the credits lifted above reels' band (`above_platform`, the
        // old D2) the floor is the row's, low enough for this card to take
        // over a quarter of the visible map.
        let mut v = design_base(card);
        v["design"]["credits"] = "above_platform".into();
        let r = run(&v);
        let w = find(&r, "card_too_tall");
        assert!(!w.is_empty(), "{:?}", r.diagnostics);
        assert!(w[0].message.contains("aim for 0.75"), "{}", w[0].message);
        assert!(message_lint(&w[0].message).is_ok(), "{}", w[0].message);
        let report = r.design.expect("a design story reports `design`");
        assert!(report.map_share_min < 0.75, "{}", report.map_share_min);
    }

    #[test]
    fn too_many_stats_and_legend_codes_fire() {
        let card = serde_json::json!({"headline": "H", "stats": [{"label": "A", "value": "1"}, {"label": "B", "value": "2"}, {"label": "C", "value": "3"}]});
        let r = run(&design_base(card));
        let w = find(&r, "too_many_stats");
        assert_eq!(w.len(), 1, "{:?}", r.diagnostics);
        assert!(w[0].message.contains('C'), "{}", w[0].message);
        assert!(message_lint(&w[0].message).is_ok(), "{}", w[0].message);

        let five: Vec<serde_json::Value> = (0..5).map(|i| serde_json::json!({"label": format!("l{i}")})).collect();
        let card = serde_json::json!({"headline": "H", "legend": {"items": five}});
        let r = run(&design_base(card));
        let w = find(&r, "legend_crowded");
        assert_eq!(w.len(), 1, "{:?}", r.diagnostics);
        assert!(w[0].message.contains('5'), "{}", w[0].message);
        assert!(message_lint(&w[0].message).is_ok(), "{}", w[0].message);

        let seven: Vec<serde_json::Value> = (0..7).map(|i| serde_json::json!({"label": format!("l{i}")})).collect();
        let card = serde_json::json!({"headline": "H", "legend": {"items": seven}});
        let r = run(&design_base(card));
        let w = find(&r, "legend_too_many_items");
        assert_eq!(w.len(), 1, "{:?}", r.diagnostics);
        assert!(message_lint(&w[0].message).is_ok(), "{}", w[0].message);
        // Review round 1: the count is the seven the author wrote, not
        // `shown_rows`' own post-fold six (five plus "Other").
        let crowded = find(&r, "legend_crowded");
        assert_eq!(crowded.len(), 1, "{:?}", r.diagnostics);
        assert!(crowded[0].message.contains("with 7 items") && crowded[0].message.contains("Four or fewer"), "{}", crowded[0].message);
    }

    #[test]
    fn index_without_tour_fires_outside_a_tour() {
        let card = serde_json::json!({"headline": "H", "index": true});
        let mut v = design_base(card);
        // Rule 1 always wins (spec §2.1): without this, `any_card_has_index`
        // alone would detect `tour` (rule 6), which is exactly the case
        // this warning does not apply to.
        v["design"]["archetype"] = "place".into();
        let r = run(&v);
        let w = find(&r, "index_without_tour");
        assert_eq!(w.len(), 1, "{:?}", r.diagnostics);
        assert!(message_lint(&w[0].message).is_ok(), "{}", w[0].message);
    }

    /// Spec §11 (R14): an unknown `design.dir` warns and names the value;
    /// `ltr`, `rtl` (any case) and no `dir` do not.
    #[test]
    fn unknown_dir_fires_on_a_bad_direction() {
        let mut v = design_base(serde_json::json!({"headline": "H"}));
        v["design"]["dir"] = "sideways".into();
        let r = run(&v);
        let w = find(&r, "unknown_dir");
        assert_eq!(w.len(), 1, "{:?}", r.diagnostics);
        assert_eq!((w[0].path.as_str(), w[0].severity), ("/design/dir", Severity::Warning));
        assert!(w[0].message.contains("\"sideways\""), "{}", w[0].message);
        assert!(message_lint(&w[0].message).is_ok(), "{}", w[0].message);
        for ok in [Some("rtl"), Some(" LTR "), None] {
            let mut v = design_base(serde_json::json!({"headline": "H"}));
            if let Some(d) = ok {
                v["design"]["dir"] = d.into();
            }
            assert!(find(&run(&v), "unknown_dir").is_empty(), "{ok:?}");
        }
    }

    /// Spec §11 (amended 2026-10-05): an unknown `design.credits` warns
    /// and names the value; `bottom`, `above_platform` (any case) and no
    /// `credits` do not.
    #[test]
    fn unknown_credits_fires_on_a_bad_placement() {
        let mut v = design_base(serde_json::json!({"headline": "H"}));
        v["design"]["credits"] = "floating".into();
        let r = run(&v);
        let w = find(&r, "unknown_credits");
        assert_eq!(w.len(), 1, "{:?}", r.diagnostics);
        assert_eq!((w[0].path.as_str(), w[0].severity), ("/design/credits", Severity::Warning));
        assert!(w[0].message.contains("\"floating\""), "{}", w[0].message);
        assert!(message_lint(&w[0].message).is_ok(), "{}", w[0].message);
        for ok in [Some("bottom"), Some(" Above_Platform "), None] {
            let mut v = design_base(serde_json::json!({"headline": "H"}));
            if let Some(c) = ok {
                v["design"]["credits"] = c.into();
            }
            assert!(find(&run(&v), "unknown_credits").is_empty(), "{ok:?}");
        }
    }

    #[test]
    fn accent_low_contrast_fires_on_a_pale_accent() {
        let mut v = design_base(serde_json::json!({"headline": "H"}));
        v["design"]["accent"] = "#fdf6e3".into();
        let r = run(&v);
        let w = find(&r, "accent_low_contrast");
        assert_eq!(w.len(), 1, "{:?}", r.diagnostics);
        assert!(w[0].message.contains("#FDF6E3"), "{}", w[0].message);
        assert!(message_lint(&w[0].message).is_ok(), "{}", w[0].message);
    }

    /// Spec D1: a design story that names no `output.safe_area` gets the
    /// info `safe_area_not_chosen`; a 1a story (no top-level `design`)
    /// reports neither it nor `Report::design`
    /// (`a_1a_story_reports_no_design_and_no_safe_area_info`).
    #[test]
    fn a_1a_story_reports_no_design_and_no_safe_area_info() {
        let mut v = design_base(serde_json::json!({"headline": "H"}));
        v["output"].as_object_mut().unwrap().remove("safe_area");
        let r = run(&v);
        assert_eq!(find(&r, "safe_area_not_chosen").len(), 1, "{:?}", r.diagnostics);
        assert!(r.design.is_some());

        let v = base(serde_json::json!([]));
        let r = run(&v);
        assert!(find(&r, "safe_area_not_chosen").is_empty(), "{:?}", r.diagnostics);
        assert!(r.design.is_none(), "{:?}", r.design);

        // Review round 1: `base()`'s plain width/height output never
        // touches `tiers::default_safe_area` at all, so it does not
        // actually exercise D1 ("absent means `none`... in the design
        // path"); a 9:16 1a story (aspect/tier, where 1a's OWN default
        // safe area would otherwise apply) with no `output.safe_area`
        // and no top-level `design` is the case D1 covers, and it still
        // reports neither.
        let mut v = vertical(serde_json::json!([]));
        v["output"].as_object_mut().unwrap().remove("safe_area");
        assert!(v.get("design").is_none(), "a 1a story: no top-level design");
        let r = run(&v);
        assert!(find(&r, "safe_area_not_chosen").is_empty(), "{:?}", r.diagnostics);
        assert!(r.design.is_none(), "{:?}", r.design);
    }

    /// Spec §11, R10: `message_lint` passes on every template this test
    /// battery and the file's own existing tests fire (1a's codes
    /// rewritten to the same rules; the design system's new ones built
    /// to them from the start).
    #[test]
    fn message_lint_passes_on_fired_messages() {
        assert!(message_lint("Fine.").is_ok());
        assert!(message_lint("at 4.20 s: Fine.").is_ok());
        assert!(message_lint("no capital.").is_err());
        assert!(message_lint("No full stop").is_err());
        assert!(message_lint(&"x".repeat(201)).is_err());

        let mut messages = Vec::new();
        messages.extend(find(&run(&base(serde_json::json!([]))), "ffmpeg_found").into_iter().map(|d| d.message.clone()));
        let long = run(&vertical(serde_json::json!([
            {"at": 0.0, "until": 2.0, "panel": {"placement": "lower-third", "headline": long_headline()}}
        ])));
        messages.extend(long.diagnostics.iter().map(|d| d.message.clone()));
        let card = serde_json::json!({"headline": "H", "stats": [{"label": "A", "value": "1"}], "legend": {"items": [{"label": "x"}]}});
        messages.extend(run(&design_base(card)).diagnostics.iter().map(|d| d.message.clone()));
        // Review round 1: every `unknown_layer` site this file does not
        // otherwise reach (an event's `overlay.layer`, a legend's
        // `layers`, a `follow_draw`; `timeline.rs`'s style-layer one).
        let unknown_layer_scripts = [
            serde_json::json!({"story":1,"output":{"aspect":"16:9"},"scene":{"style":"x.json","basemap":{"source":"openfreemap"}},
                "camera":[{"type":"jump","to":{"lon":103.8,"lat":1.29,"zoom":10}},{"type":"hold","duration":1}],
                "events":[{"at":1.0,"until":2.0,"overlay":{"layer":"nope","effect":"show"}}]}),
            serde_json::json!({"story":1,"output":{"aspect":"16:9"},"scene":{"style":"x.json","basemap":{"source":"openfreemap"}},
                "camera":[{"type":"jump","to":{"lon":103.8,"lat":1.29,"zoom":10}},{"type":"hold","duration":1}],
                "furniture":{"legend":{"layers":["nope"]}}}),
            serde_json::json!({"story":1,"output":{"aspect":"16:9"},"scene":{"style":"x.json","basemap":{"source":"openfreemap"}},
                "camera":[{"type":"jump","label":"start","to":{"lon":103.8,"lat":1.29,"zoom":10}},{"type":"follow","follow_draw":"nope","duration":5.0}]}),
        ];
        for v in &unknown_layer_scripts {
            let err = crate::story::parse(v.to_string().as_bytes()).expect_err("unknown_layer");
            let unknown = err.iter().find(|d| d.code == "unknown_layer").unwrap_or_else(|| panic!("{err:?}"));
            messages.push(unknown.message.clone());
        }
        // `timeline.rs`'s OTHER `unknown_layer` (a style layer, not an
        // overlay one): `set_layer_visibility` naming a style layer the
        // loaded style does not have.
        let style_unknown = run(&base(serde_json::json!([{"at": 0.5, "set_layer_visibility": {"layer": "nope", "visible": false}}])));
        let d = find(&style_unknown, "unknown_layer");
        assert!(!d.is_empty(), "{:?}", style_unknown.diagnostics);
        messages.extend(d.into_iter().map(|d| d.message.clone()));
        let bad: Vec<(String, String)> = messages.into_iter().filter_map(|m| message_lint(&m).err().map(|e| (m, e))).collect();
        assert!(bad.is_empty(), "{bad:#?}");
    }

    #[test]
    fn a_clean_fixture_story_is_ok_with_an_estimate() {
        let (s, raw) = script_of(&base(serde_json::json!([])));
        let r = pollster::block_on(validate_scene(&s, &raw, &opts()));
        assert!(r.ok, "{:?}", r.diagnostics);
        let e = r.estimate.clone().expect("estimate");
        assert_eq!(e.frames, 10);
        assert!(e.tiles > 0 && e.max_cover > 0 && e.max_cover <= e.tiles, "{e:?}");
        assert_eq!(e.bytes, e.tiles as u64 * ESTIMATE_BYTES_PER_TILE);
        assert!(codes(&r).iter().any(|c| *c == "ffmpeg_found" || *c == "ffmpeg_missing"));
    }

    /// Spec §10.1 (R6): two event cards on screen at once refuse, at the
    /// later card.
    #[test]
    fn overlapping_cards_are_an_error_at_the_later_card() {
        let (s, raw) = script_of(&base(serde_json::json!([
            {"at": 0.0, "until": 0.6, "card": {"headline": "First"}},
            {"at": 0.4, "until": 1.0, "card": {"headline": "Second"}}
        ])));
        let r = pollster::block_on(validate_scene(&s, &raw, &opts()));
        assert!(!r.ok);
        let d = r.diagnostics.iter().find(|d| d.code == "cards_overlap").unwrap_or_else(|| panic!("{:?}", r.diagnostics));
        assert_eq!((d.severity, d.path.as_str()), (Severity::Error, "/events/1/card"));
        assert!(d.message.starts_with("at 0.40 s: "), "{}", d.message);
    }

    #[test]
    fn an_edit_to_a_layer_the_style_lacks_is_an_error_at_the_event() {
        let (s, raw) = script_of(&base(serde_json::json!([{"at":0.5,"set_layer_visibility":{"layer":"no-such-layer","visible":false}}])));
        let r = pollster::block_on(validate_scene(&s, &raw, &opts()));
        assert!(!r.ok);
        assert!(r.diagnostics.iter().any(|d| d.severity == Severity::Error && d.path.starts_with("/events/0")), "{:?}", r.diagnostics);
    }

    #[test]
    fn missing_files_are_each_reported_at_their_path() {
        let mut v = base(serde_json::json!([]));
        v["scene"]["overlays"] = serde_json::json!([{"id":"a","geojson":"nope/a.geojson"}]);
        v["scene"]["tracks"] = serde_json::json!([{"id":"bus","file":"nope/bus.csv","style":{"color":"#000","radius_px":4,"trail_s":10}}]);
        let (s, raw) = script_of(&v);
        let r = pollster::block_on(validate_scene(&s, &raw, &opts()));
        assert!(!r.ok);
        let paths: Vec<&str> = r.diagnostics.iter().filter(|d| d.code == "file_not_found").map(|d| d.path.as_str()).collect();
        assert!(paths.contains(&"/scene/overlays/0/geojson"), "{paths:?}");
        assert!(paths.contains(&"/scene/tracks/0/file"), "{paths:?}");
    }

    #[test]
    fn a_data_path_that_escapes_the_story_folder_is_refused_even_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("x.geojson"), "{}").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        assert_eq!(resolve_in_story_dir(dir.path(), "link/x.geojson").unwrap_err().code, "path_escapes");
        assert_eq!(resolve_in_story_dir(dir.path(), "../x.geojson").unwrap_err().code, "path_escapes");
        assert_eq!(resolve_in_story_dir(dir.path(), "missing.geojson").unwrap_err().code, "file_not_found");
    }

    /// Task 12 review F5: `validate_scene` loads every image -- panel,
    /// standalone and callout -- and reports each failure as an error at
    /// its own `src`, plus `missing_alt`/`missing_credit` warnings at the
    /// image.
    #[test]
    fn validate_reports_each_image_problem_at_its_own_pointer() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::copy(root().join("examples/liberty-style.json"), dir.path().join("style.json")).unwrap();
        std::fs::create_dir(dir.path().join("img")).unwrap();
        image::RgbaImage::from_pixel(6, 4, image::Rgba([1, 2, 3, 255])).save(dir.path().join("img/ok.png")).unwrap();
        std::fs::write(dir.path().join("img/a.gif"), b"GIF89a\x01\x00\x01\x00").unwrap();
        image::RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 255])).save(outside.path().join("x.png")).unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        let mut v = base(serde_json::json!([
            {"at": 0.1, "until": 0.9, "panel": {"placement": "lower-third", "headline": "H", "image": {"src": "img/ok.png"}}},
            {"at": 0.1, "until": 0.9, "image": {"src": "link/x.png", "alt": "a", "credit": "c", "placement": "corner"}},
            {"at": 0.1, "until": 0.9, "callout": {"at_coord": [103.85, 1.29], "text": "T", "image": {"src": "img/a.gif", "alt": "a", "credit": "c"}}},
            {"at": 0.1, "until": 0.9, "image": {"src": "img/none.png", "alt": "a", "credit": "c"}}
        ]));
        v["scene"]["style"] = "style.json".into();
        let (s, raw) = script_of(&v);
        let o = ValidateOptions { offline: true, base_dir: dir.path().into(), image_cache_dir: Some(dir.path().join("cache")), estimate: true, ..ValidateOptions::default() };
        let r = tokio::runtime::Runtime::new().unwrap().block_on(validate_scene(&s, &raw, &o));
        assert!(!r.ok);
        let has = |sev: Severity, code: &str, path: &str| r.diagnostics.iter().any(|d| d.severity == sev && d.code == code && d.path == path);
        assert!(has(Severity::Warning, "missing_alt", "/events/0/panel/image"), "{:?}", r.diagnostics);
        assert!(has(Severity::Warning, "missing_credit", "/events/0/panel/image"), "{:?}", r.diagnostics);
        assert!(has(Severity::Error, "path_escapes", "/events/1/image/src"), "{:?}", r.diagnostics);
        assert!(has(Severity::Error, "image_format", "/events/2/callout/image/src"), "{:?}", r.diagnostics);
        assert!(has(Severity::Error, "file_not_found", "/events/3/image/src"), "{:?}", r.diagnostics);
        // The good image raised no error of its own.
        assert!(!r.diagnostics.iter().any(|d| d.severity == Severity::Error && d.path.starts_with("/events/0")), "{:?}", r.diagnostics);
    }

    /// A syntactically clean relative path (no `..`, so stage 1's
    /// `bad_path` never fires) that a symlink carries outside the story
    /// folder is still refused, at the overlay's own path.
    #[test]
    fn a_data_path_escaping_through_a_symlink_is_an_error_at_its_own_path() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("x.geojson"), "{}").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        std::fs::copy(root().join("examples/liberty-style.json"), dir.path().join("style.json")).unwrap();
        let mut v = base(serde_json::json!([]));
        v["scene"]["style"] = "style.json".into();
        v["scene"]["overlays"] = serde_json::json!([{"id": "a", "data": "link/x.geojson",
            "layers": [{"id": "l", "type": "line"}]}]);
        let (s, raw) = script_of(&v);
        let r = pollster::block_on(validate_scene(&s, &raw, &ValidateOptions { offline: true, base_dir: dir.path().into(), image_cache_dir: None, estimate: true, ..ValidateOptions::default() }));
        assert!(!r.ok);
        assert!(
            r.diagnostics.iter().any(|d| d.code == "path_escapes" && d.path == "/scene/overlays/0/data"),
            "{:?}",
            r.diagnostics
        );
    }

    /// Review Finding 1: a `data` overlay whose file cannot be read
    /// refuses `scene_inputs` outright, exactly as a broken `geojson`
    /// overlay already did -- spec §7's "render refused" list, and the
    /// path that matters most, since the agent's render action calls
    /// `scene_inputs` directly and never runs `validate_scene` first.
    #[test]
    fn scene_inputs_refuses_a_missing_data_file() {
        let mut v = base(serde_json::json!([]));
        v["scene"]["overlays"] = serde_json::json!([{"id": "a", "data": "nope/route.geojson",
            "layers": [{"id": "l", "type": "line"}]}]);
        let (s, _) = script_of(&v);
        let err = scene_inputs(&s, &root(), true).err().expect("a missing `data` file must refuse the render");
        let msg = format!("{err:#}");
        assert!(msg.contains("file_not_found"), "{msg}");
    }

    #[test]
    fn over_50k_features_warns_and_a_hairline_warns() {
        let n = MAX_OVERLAY_FEATURES_WARN + 1;
        let features: Vec<serde_json::Value> = (0..n)
            .map(|i| serde_json::json!({"type": "Feature", "properties": {},
                "geometry": {"type": "Point", "coordinates": [103.8 + (i as f64) * 1e-6, 1.3]}}))
            .collect();
        let mut v = base(serde_json::json!([]));
        v["scene"]["overlays"] = serde_json::json!([{
            "id": "dots", "data": {"type": "FeatureCollection", "features": features},
            "layers": [{"id": "route-line", "type": "line", "paint": {"line-width": 1}},
                       {"id": "dot-layer", "type": "circle"}]
        }]);
        let (s, raw) = script_of(&v);
        let r = pollster::block_on(validate_scene(&s, &raw, &opts()));
        assert!(codes(&r).contains(&"too_many_features"), "{:?}", r.diagnostics);
        assert!(
            r.diagnostics
                .iter()
                .any(|d| d.code == "too_many_features" && d.severity == Severity::Warning && d.path == "/scene/overlays/0"),
            "{:?}",
            r.diagnostics
        );
        assert!(
            r.diagnostics.iter().any(|d| d.code == "hairline" && d.path == "/scene/overlays/0/layers/0"),
            "{:?}",
            r.diagnostics
        );
    }

    /// Task 7 review #10: a `highlight` on a `line-gradient` layer is
    /// warned about (the gradient overwrites the highlight colour; only
    /// the dimming shows), and a layer with no gradient or no highlight is
    /// silent.
    #[test]
    fn a_highlight_over_a_line_gradient_layer_warns() {
        let mut v = base(serde_json::json!([{
            "at": 0.0, "until": 2.0,
            "overlay": {"layer": "route-line", "effect": "highlight", "ids": [0], "color": "#f00"}
        }]));
        v["scene"]["overlays"] = serde_json::json!([{
            "id": "route", "line_metrics": true,
            "data": {"type": "FeatureCollection", "features": [{"type": "Feature", "properties": {},
                "geometry": {"type": "LineString", "coordinates": [[103.8, 1.3], [103.9, 1.3]]}}]},
            "layers": [{"id": "route-line", "type": "line",
                "paint": {"line-gradient": ["interpolate", ["linear"], ["line-progress"], 0, "blue", 1, "red"]}}]
        }]);
        let (s, raw) = script_of(&v);
        let r = pollster::block_on(validate_scene(&s, &raw, &opts()));
        assert!(
            r.diagnostics.iter().any(|d| d.code == "highlight_over_gradient" && d.severity == Severity::Warning),
            "{:?}",
            r.diagnostics
        );

        // No highlight on it: silent.
        let mut quiet = base(serde_json::json!([]));
        quiet["scene"]["overlays"] = v["scene"]["overlays"].clone();
        let (s, raw) = script_of(&quiet);
        let r = pollster::block_on(validate_scene(&s, &raw, &opts()));
        assert!(!codes(&r).contains(&"highlight_over_gradient"), "{:?}", r.diagnostics);
    }

    #[test]
    fn a_bad_track_file_is_prefixed_with_its_scene_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bus.csv"), "t,lon,lat\n0,103.8,1.3\n0,103.9,1.3\n").unwrap();
        std::fs::copy(root().join("examples/liberty-style.json"), dir.path().join("style.json")).unwrap();
        let mut v = base(serde_json::json!([]));
        v["scene"]["style"] = "style.json".into();
        v["scene"]["tracks"] = serde_json::json!([{"id":"bus","file":"bus.csv","style":{"color":"#000","radius_px":4,"trail_s":10}}]);
        let (s, raw) = script_of(&v);
        let r = pollster::block_on(validate_scene(&s, &raw, &ValidateOptions { offline: true, base_dir: dir.path().into(), image_cache_dir: None, estimate: true, ..ValidateOptions::default() }));
        assert!(
            r.diagnostics.iter().any(|d| d.severity == Severity::Error && d.path.starts_with("/scene/tracks/0/file/")),
            "{:?}",
            r.diagnostics
        );
    }

    #[test]
    fn parse_warnings_reach_the_report() {
        let mut v = base(serde_json::json!([]));
        v["camera"][0]["to"]["lon"] = 463.851.into();
        let (s, raw) = script_of(&v);
        let r = pollster::block_on(validate_scene(&s, &raw, &opts()));
        assert!(r.diagnostics.iter().any(|d| d.severity == Severity::Warning && d.path == "/camera/0/to/lon"), "{:?}", r.diagnostics);
    }

    #[test]
    fn mapbox_without_a_token_names_the_token_and_the_keyless_twin() {
        if std::env::var_os(MAPBOX_TOKEN_ENV).is_some() {
            eprintln!("skipped: {MAPBOX_TOKEN_ENV} is set");
            return;
        }
        let mut v = base(serde_json::json!([]));
        let Some(style) = crate::mapbox_fixtures::path("mapbox-streets-style.json") else { return };
        v["scene"]["style"] = style.display().to_string().into();
        v["scene"]["basemap"] = serde_json::json!({"source":"mapbox"});
        let (s, raw) = script_of(&v);
        let r = pollster::block_on(validate_scene(&s, &raw, &opts()));
        let d = r.diagnostics.iter().find(|d| d.code == "mapbox_token_missing").expect("token diagnostic");
        assert_eq!(d.path, "/scene/basemap/source");
        assert!(d.hint.as_deref().unwrap_or_default().contains("--keyless"));
        assert!(!r.ok);
    }

    #[test]
    fn an_unknown_source_is_an_error_with_the_choices() {
        let mut v = base(serde_json::json!([]));
        v["scene"]["basemap"] = serde_json::json!({"source":"osm-carto"});
        let (s, raw) = script_of(&v);
        let r = pollster::block_on(validate_scene(&s, &raw, &opts()));
        assert!(codes(&r).contains(&"unknown_source"), "{:?}", r.diagnostics);
    }

    #[test]
    fn a_huge_cover_is_warned_about() {
        let mut v = base(serde_json::json!([]));
        // 8K-wide, pitched 45 at z12: ~480 tiles in one frame (measured).
        v["output"] = serde_json::json!({"width":8192,"height":4320,"fps":1});
        v["camera"][0]["to"] = serde_json::json!({"lon":103.85,"lat":1.29,"zoom":12,"pitch":45});
        let (s, raw) = script_of(&v);
        let r = pollster::block_on(validate_scene(&s, &raw, &opts()));
        let e = r.estimate.as_ref().expect("estimate");
        assert!(e.max_cover > MAX_COVER_WARN, "the scene must be big enough to test anything: {e:?}");
        assert!(codes(&r).contains(&"cover_too_large"), "{:?}", r.diagnostics);

        // And a small, flat frame stays under the limit with no warning.
        let (s, raw) = script_of(&base(serde_json::json!([])));
        let r = pollster::block_on(validate_scene(&s, &raw, &opts()));
        assert!(r.estimate.as_ref().expect("estimate").max_cover <= MAX_COVER_WARN);
        assert!(!codes(&r).contains(&"cover_too_large"), "{:?}", r.diagnostics);
    }

    #[test]
    fn user_urls_lose_their_keys_in_the_label() {
        assert_eq!(redacted_url("https://t.example/v1/{z}/{x}/{y}.pbf?key=SECRET"), "https://t.example/v1/{z}/{x}/{y}.pbf");
        assert_eq!(redacted_url("https://t.example/tiles?z={z}&x={x}&y={y}&token=SECRET"), "https://t.example/tiles");
        assert_eq!(
            redacted_url("https://bucket.s3.amazonaws.com/a.pmtiles?X-Amz-Signature=SECRET&X-Amz-Credential=AK"),
            "https://bucket.s3.amazonaws.com/a.pmtiles"
        );
        assert_eq!(redacted_url("https://user:SECRET@t.example/a.pmtiles#frag"), "https://t.example/a.pmtiles");
        assert_eq!(redacted_url("data/local.pmtiles"), "data/local.pmtiles");

        // Through the loader: the manifest's source and the cache folder.
        let mut v = base(serde_json::json!([]));
        v["scene"]["basemap"] =
            serde_json::json!({"tiles": ["https://t.example/tiles?z={z}&x={x}&y={y}&key=SECRET"], "max_zoom": 14, "attribution": "x"});
        let (s, _) = script_of(&v);
        let i = scene_inputs(&s, &root(), true).unwrap();
        assert!(!i.source_label.contains("SECRET"), "{}", i.source_label);
        let cache = crate::story::default_cache_dir(&i.source_label);
        assert!(!cache.to_string_lossy().contains("SECRET"), "{}", cache.display());
    }

    /// Final review F1, the validate half: a keyed tiles URL whose z0
    /// fetch fails is reported without any of its credentials.
    #[test]
    fn a_failing_keyed_source_is_reported_without_its_credentials() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let server = rt.block_on(wiremock::MockServer::start());
        rt.block_on(
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .respond_with(wiremock::ResponseTemplate::new(500))
                .mount(&server),
        );
        let url = format!(
            "{}/{{z}}/{{x}}/{{y}}.mvt?access_token=SECRET1&key=SECRET2",
            server.uri().replace("://", "://user:SECRET3@")
        );
        let mut v = base(serde_json::json!([]));
        v["scene"]["basemap"] = serde_json::json!({"tiles": [url], "max_zoom": 14, "attribution": "© Test"});
        let (s, raw) = script_of(&v);
        let r = rt.block_on(validate_scene(&s, &raw, &ValidateOptions { offline: false, base_dir: root(), image_cache_dir: None, estimate: true, ..ValidateOptions::default() }));
        assert!(codes(&r).contains(&"source_unreachable"), "{:?}", r.diagnostics);
        let text = serde_json::to_string(&r).unwrap();
        assert!(!text.contains("SECRET"), "{text}");
    }

    #[test]
    fn scene_inputs_for_the_fixture_matches_the_hand_built_inputs() {
        let (s, _) = script_of(&base(serde_json::json!([])));
        let i = scene_inputs(&s, &root(), true).unwrap();
        assert_eq!(i.source_label, "fixture");
        assert_eq!(i.flat_tile_zoom_max, 14);
        assert!(i.credits.is_empty() && i.overlays.is_empty() && i.raster.is_none());
        let (files, family) = i.label_fonts.unwrap();
        assert_eq!(family, "Noto Sans");
        assert_eq!(files.len(), 4);
    }

    /// Spec §7's estimate per tier (Task 18): every tier at the script's
    /// aspect, smallest first, and the frame memory scales with the
    /// pixel count -- 4K is four times 1080p.
    #[test]
    fn the_estimate_covers_every_tier_and_4k_costs_four_times_1080p_in_frame_memory() {
        let raw = std::fs::read(root().join("tests/fixtures/stories/det.json")).unwrap();
        let s = crate::story::parse(&raw).unwrap();
        let o = ValidateOptions { offline: true, base_dir: root().join("tests/fixtures/stories"), image_cache_dir: None, estimate: true, ..ValidateOptions::default() };
        let r = pollster::block_on(validate_scene(&s, &raw, &o));
        assert!(r.ok, "{:?}", r.diagnostics);
        let e = r.estimate.clone().expect("estimate");
        let tiers: Vec<Tier> = e.tiers.iter().map(|t| t.tier).collect();
        assert_eq!(tiers, [Tier::Hd, Tier::Fullhd, Tier::Qhd, Tier::Uhd]);
        let by = |tier: Tier| e.tiers.iter().find(|t| t.tier == tier).unwrap();
        let (fhd, uhd) = (by(Tier::Fullhd), by(Tier::Uhd));
        assert_eq!((fhd.width, fhd.height, uhd.width, uhd.height), (1920, 1080, 3840, 2160), "det.json is 16:9");
        let fixed = GLYPH_ATLAS_BYTES + ICON_ATLAS_BYTES + crate::story::player::DEFAULT_MESH_CACHE_BYTES;
        assert_eq!(uhd.gpu_bytes - fixed, 4 * (fhd.gpu_bytes - fixed), "no images: frame memory alone, 4x");
        assert_eq!(fhd.gpu_bytes - fixed, 1920 * 1080 * GPU_BYTES_PER_PIXEL);
        for t in &e.tiers {
            assert!(t.tiles > 0, "{t:?}");
            let kbps = crate::story::tiers::bitrate_kbps(t.tier, 10);
            assert_eq!(t.bytes, (f64::from(kbps) * 125.0 * (e.duration_s + 2.0)).ceil() as u64, "{t:?}");
            assert!(t.wall_s_est.is_some_and(|w| w > 0.0), "{t:?}");
        }
        assert!(uhd.wall_s_est > fhd.wall_s_est && uhd.bytes > fhd.bytes);
        assert!(e.gpu_bytes < GPU_BUDGET_BYTES);
        assert!(!codes(&r).contains(&"gpu_memory"), "a 480x270 story fits");
        // The report the agent reads carries the per-tier rows.
        let j = serde_json::to_value(&r).unwrap();
        assert_eq!(j["estimate"]["tiers"][3]["tier"], "4k", "{}", j["estimate"]);
    }

    /// Spec §7: a frame past the capture device's texture side (8192 at
    /// the baseline limits it runs with) warns `gpu_memory` -- never an
    /// error -- while a 4K frame passes.
    #[test]
    fn a_frame_past_the_texture_limit_warns_gpu_memory_and_4k_does_not() {
        assert!(capture_max_texture_dimension_2d() >= 3840, "4K fits every conforming device");
        let mut v = base(serde_json::json!([]));
        v["output"] = serde_json::json!({"aspect": "16:9", "tier": "4k", "fps": 10});
        let (s, raw) = script_of(&v);
        let r = pollster::block_on(validate_scene(&s, &raw, &opts()));
        assert!(r.ok && !codes(&r).contains(&"gpu_memory"), "{:?}", r.diagnostics);
        // A script past 8192 physical pixels is already refused at parse
        // (`too_large`); this check is the backstop for a device whose
        // limit is lower than the parse limit.
        let max = capture_max_texture_dimension_2d();
        let mut diags = Vec::new();
        warn_gpu_memory(max + 16, 270, 0, &mut diags);
        let d = diags.iter().find(|d| d.code == "gpu_memory").expect("gpu_memory");
        assert_eq!(d.severity, Severity::Warning, "conservative, never blocks");
        assert!(d.message.contains(&max.to_string()), "{}", d.message);
        // The budget arithmetic alone: an oversized image set crosses it.
        assert!(gpu_bytes(3840, 2160, GPU_BUDGET_BYTES, 0) > GPU_BUDGET_BYTES);
        let mut diags = Vec::new();
        warn_gpu_memory(3840, 2160, GPU_BUDGET_BYTES + 1, &mut diags);
        assert_eq!(diags.iter().map(|d| d.code).collect::<Vec<_>>(), ["gpu_memory"]);
    }

    /// Spec §2.4: a raster with no @2x tiles at a ratio above 1.5 is
    /// drawn upscaled, and validation says so; one that offers `{r}` is
    /// not warned about, and neither is any raster at 1080p.
    #[test]
    fn a_raster_without_2x_tiles_at_4k_warns_upscaled() {
        let run = |raster: &str, tier: &str| {
            let mut v = base(serde_json::json!([]));
            v["output"] = serde_json::json!({"aspect": "16:9", "tier": tier, "fps": 10});
            v["scene"]["raster"] = raster.into();
            let (s, raw) = script_of(&v);
            codes(&pollster::block_on(validate_scene(&s, &raw, &opts()))).contains(&"raster_upscaled")
        };
        assert!(run("https://img.example.com/{z}/{x}/{y}.png", "4k"));
        assert!(!run("https://img.example.com/{z}/{x}/{y}{r}.png", "4k"));
        assert!(!run("https://img.example.com/{z}/{x}/{y}.png", "fullhd"));
    }

    /// A 9:16 hd reels story (spec §2.4) held still over Marina Bay, so
    /// a callout's anchor stays where it is put.
    fn vertical(events: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"story":1,"summary":"A test story.","output":{"aspect":"9:16","tier":"hd","safe_area":"reels","fps":10},
          "scene":{"style":"examples/liberty-style.json","basemap":{"source":"fixture"}},
          "camera":[{"type":"jump","to":{"lon":103.851,"lat":1.2905,"zoom":14}},{"type":"hold","duration":2.0}],
          "events": events})
    }

    fn run(v: &serde_json::Value) -> Report {
        let (s, raw) = script_of(v);
        tokio::runtime::Runtime::new().unwrap().block_on(validate_scene(&s, &raw, &opts()))
    }

    fn find<'r>(r: &'r Report, code: &str) -> Vec<&'r Diagnostic> {
        r.diagnostics.iter().filter(|d| d.code == code).collect()
    }

    /// Task 10's Review Focus 1 headline (`layout.rs`'s
    /// `a_very_long_headline_at_9x16_hd_..`), repeated until even the
    /// floor needs an ellipsis.
    fn long_headline() -> String {
        "The longest container route ever sailed between two ports in a single season, and why it matters for the city ".repeat(10)
    }

    /// Review Focus 1 (brief Step 1): a very long lower-third headline on
    /// a 9:16 hd reels frame is a `text_truncated` WARNING at the event
    /// that caused it, with the time in its message, and the story stays
    /// `ok`. A callout whose anchor sits inside the reels right rail
    /// (`dlon` 0.02 at zoom 14 projects ~466 units right of centre: x
    /// ~1006 of 1080, the rail is 940..1080 x 700..1500) is moved clear
    /// when it can be -- a one-line card shifts up out of the rail, no
    /// warning -- and is `in_safe_zone` only when it cannot: a card as
    /// tall as the free band has nowhere to go.
    #[test]
    fn layout_problems_are_warnings_with_the_element_path() {
        let r = run(&vertical(serde_json::json!([
            {"at": 0.0, "until": 2.0, "panel": {"placement": "lower-third", "headline": long_headline()}}
        ])));
        assert!(r.ok, "warnings never block: {:?}", r.diagnostics);
        let w = find(&r, "text_truncated");
        assert!(w.len() == 1 && w[0].path == "/events/0" && w[0].severity == Severity::Warning, "{:?}", r.diagnostics);
        assert!(w[0].message.starts_with("at ") && w[0].hint.is_some(), "{:?}", w[0]);

        let callout = |text: String| {
            run(&vertical(serde_json::json!([
                {"at": 0.0, "until": 2.0, "callout": {"at_coord": [103.871, 1.2905], "text": text}}
            ])))
        };
        let movable = callout("Marina Bay Sands".into());
        assert!(movable.ok && find(&movable, "in_safe_zone").is_empty(), "{:?}", movable.diagnostics);
        let stuck = callout("A long callout about the bay and its history. ".repeat(40));
        assert!(stuck.ok, "{:?}", stuck.diagnostics);
        let w = find(&stuck, "in_safe_zone");
        assert!(w.len() == 1 && w[0].path == "/events/0" && w[0].severity == Severity::Warning, "{:?}", stuck.diagnostics);
        assert!(w[0].message.contains("safe_area"), "{:?}", w[0]);
    }

    /// Brief Step 1: the report the agent's `story.validate` serialises
    /// (`examples/common/story_agentic.rs` passes `Report` through
    /// `serde_json::to_value`) carries each new warning's path, code,
    /// severity and hint.
    #[test]
    fn the_agent_report_is_the_same_json() {
        let r = run(&vertical(serde_json::json!([
            {"at": 0.0, "until": 2.0, "panel": {"placement": "lower-third", "headline": long_headline()}}
        ])));
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["ok"], true);
        let d = v["diagnostics"].as_array().unwrap();
        let ours = d.iter().find(|d| d["code"] == "text_truncated").expect("text_truncated in the JSON");
        assert_eq!((ours["path"].as_str(), ours["severity"].as_str()), (Some("/events/0"), Some("warning")), "{ours}");
        assert!(ours["hint"].is_string() && ours["message"].as_str().unwrap().starts_with("at "), "{ours}");
        assert!(v.get("estimate").is_some() && v.get("ffmpeg").is_some(), "the report keeps its shape: {v}");
    }

    /// Several lower-thirds at once: the later ones have nowhere left to
    /// go and `overlap` the earlier, named at their own events.
    #[test]
    fn crowded_panels_overlap_at_their_own_paths() {
        let panel = |i: usize| serde_json::json!({"at": 0.0, "until": 2.0, "panel": {"placement": "lower-third", "headline": format!("Panel {i}"), "body": long_headline()}});
        let r = run(&vertical(serde_json::Value::Array((0..4).map(panel).collect())));
        assert!(r.ok, "{:?}", r.diagnostics);
        let w = find(&r, "overlap");
        assert!(!w.is_empty() && w.iter().all(|d| d.path.starts_with("/events/") && d.path != "/events/0"), "{:?}", r.diagnostics);
    }

    /// A caption's layout warning names the caption event, not the
    /// layout's `caption/<k>` id, and a `captions` cue names its entry.
    #[test]
    fn a_caption_warning_names_its_event_or_cue() {
        let mut v = vertical(serde_json::json!([
            {"at": 0.0, "until": 0.1, "panel": {"placement": "corner", "headline": "x"}},
            {"at": 0.0, "until": 2.0, "caption": {"text": long_headline().repeat(4), "anchor": "bottom"}}
        ]));
        // A caption tall enough to cover the frame meets the reels bars
        // wherever it goes.
        let named = |r: &Report, path: &str| {
            r.diagnostics.iter().any(|d| d.code == "in_safe_zone" && d.path == path && d.message.contains(&format!("`{path}`")) && !d.message.contains("caption/"))
        };
        let r = run(&v);
        assert!(named(&r, "/events/1"), "{:?}", r.diagnostics);
        v["events"] = serde_json::json!([]);
        v["captions"] = serde_json::json!([{"from": 0.0, "to": 2.0, "text": long_headline().repeat(4)}]);
        let r = run(&v);
        assert!(named(&r, "/captions/0"), "{:?}", r.diagnostics);
        // Two captions with the same text each name their own source (by
        // index, not by text): two events, and an event beside a cue.
        let mut v = vertical(serde_json::json!([
            {"at": 0.0, "until": 0.1, "panel": {"placement": "corner", "headline": "x"}},
            {"at": 0.0, "until": 1.0, "caption": {"text": long_headline().repeat(4), "anchor": "bottom"}},
            {"at": 1.0, "until": 2.0, "caption": {"text": long_headline().repeat(4), "anchor": "bottom"}}
        ]));
        let r = run(&v);
        assert!(named(&r, "/events/1") && named(&r, "/events/2"), "{:?}", r.diagnostics);
        v["events"].as_array_mut().unwrap().pop();
        v["captions"] = serde_json::json!([{"from": 1.0, "to": 2.0, "text": long_headline().repeat(4)}]);
        let r = run(&v);
        assert!(named(&r, "/events/1") && named(&r, "/captions/0"), "{:?}", r.diagnostics);
    }

    /// Task 14's pitched-camera note reaches the report as an `info` at the
    /// furniture that asked for the scale bar: the story's, or the
    /// chapter's own override.
    #[test]
    fn a_scale_bar_hidden_by_pitch_is_a_note_at_its_furniture() {
        let mut v = vertical(serde_json::json!([]));
        v["camera"][0]["to"]["pitch"] = 60.into();
        v["furniture"] = serde_json::json!({"scale_bar": "metric"});
        let r = run(&v);
        let w = find(&r, "scale_bar_hidden_pitch");
        assert!(w.len() == 1 && w[0].path == "/furniture" && w[0].severity == Severity::Info, "{:?}", r.diagnostics);
        v["furniture"] = serde_json::json!({});
        v["chapters"] = serde_json::json!([{"id": "a", "title": "A", "from": 0.0, "to": 2.0, "alt": "x", "furniture": {"scale_bar": "metric"}}]);
        let r = run(&v);
        let w = find(&r, "scale_bar_hidden_pitch");
        assert!(w.len() == 1 && w[0].path == "/chapters/0/furniture", "{:?}", r.diagnostics);
    }

    /// Task 1's rule (progress P6): an app's safe area on an aspect it was
    /// not measured for is `safe_area_mismatch`; the right pairing, the
    /// default and `none` are not.
    #[test]
    fn a_safe_area_for_another_aspect_is_a_mismatch() {
        let with = |aspect: &str, area: Option<&str>| {
            let mut v = base(serde_json::json!([]));
            v["output"] = serde_json::json!({"aspect": aspect, "tier": "hd", "fps": 10});
            if let Some(area) = area {
                v["output"]["safe_area"] = area.into();
            }
            find(&run(&v), "safe_area_mismatch").iter().map(|d| (d.path.clone(), d.severity)).collect::<Vec<_>>()
        };
        let mismatch = vec![("/output/safe_area".to_string(), Severity::Warning)];
        assert_eq!(with("16:9", Some("reels")), mismatch);
        assert_eq!(with("1:1", Some("tiktok")), mismatch);
        assert_eq!(with("9:16", Some("youtube")), mismatch);
        assert_eq!(with("4:5", Some("youtube")), mismatch);
        for (aspect, area) in [("9:16", Some("reels")), ("4:5", Some("shorts")), ("16:9", Some("youtube")), ("16:9", Some("none")), ("9:16", None)] {
            assert!(with(aspect, area).is_empty(), "{aspect} {area:?}");
        }
    }

    /// Spec §5: a chapter with no `alt` warns at the chapter; the implicit
    /// chapter counts the story's `summary`, at the document root.
    #[test]
    fn a_chapter_without_alt_warns_at_the_chapter() {
        let mut v = base(serde_json::json!([]));
        let paths = |v: &serde_json::Value| find(&run(v), "chapter_without_alt").iter().map(|d| d.path.clone()).collect::<Vec<_>>();
        assert_eq!(paths(&v), ["/summary"], "the implicit chapter, no summary");
        v["summary"] = "A short orbit.".into();
        assert!(paths(&v).is_empty(), "the summary is the implicit chapter's alt");
        v["chapters"] = serde_json::json!([
            {"id": "a", "title": "A", "from": 0.0, "to": 0.5, "alt": "The first half."},
            {"id": "b", "title": "B", "from": 0.5, "to": 1.0}
        ]);
        assert!(paths(&v).is_empty(), "the story's summary stands in for a chapter's alt (ChapterSpec::alt)");
        v["summary"] = "   ".into();
        assert_eq!(paths(&v), ["/chapters/1"], "a blank summary is none");
        v["summary"] = serde_json::Value::Null;
        v.as_object_mut().unwrap().remove("summary");
        assert_eq!(paths(&v), ["/chapters/1"], "no alt and no summary");
        v["chapters"][1]["alt"] = "  ".into();
        assert_eq!(paths(&v), ["/chapters/1"], "blank alt text is none");
        v["chapters"][1]["alt"] = "The second half.".into();
        assert!(paths(&v).is_empty());
    }

    /// Ruling 1: the fallback is applied once, in `chapters::resolve`, so
    /// the warning, the transcript's `<img alt>` and the manifest's
    /// coverage all agree on what a chapter's alt is.
    #[test]
    fn the_warning_the_transcript_and_the_coverage_agree_on_chapter_alt() {
        let check = |summary: Option<&str>| {
            let mut v = base(serde_json::json!([]));
            v["chapters"] = serde_json::json!([
                {"id": "a", "title": "A", "from": 0.0, "to": 0.5, "alt": "The first half."},
                {"id": "b", "title": "B", "from": 0.5, "to": 1.0, "alt": " "}
            ]);
            if let Some(s) = summary {
                v["summary"] = s.into();
            }
            let (s, _) = script_of(&v);
            let mut inputs = scene_inputs(&s, &root(), true).unwrap();
            let tracks = Arc::new(std::mem::take(&mut inputs.tracks));
            let base_style = std::mem::take(&mut inputs.base_style);
            let (_, timeline) =
                crate::story::timeline::path_and_timeline((480.0, 270.0), &s, tracks, &[], base_style, &*inputs.load_style).map_err(|e| e.0).unwrap();
            let html = crate::story::transcript::transcript_html("T", s.summary.as_deref(), None, timeline.chapters(), &[], &[], &[]);
            let coverage = crate::story::player::alt_coverage(&s, timeline.chapters(), &crate::story::media::ScriptImages::default());
            let warned = find(&run(&v), "chapter_without_alt").iter().map(|d| d.path.clone()).collect::<Vec<_>>();
            (html, coverage, warned)
        };
        let (html, coverage, warned) = check(Some("A short orbit."));
        assert!(html.contains(r#"alt="A short orbit.""#), "{html}");
        assert_eq!(coverage, (2, 2));
        assert!(warned.is_empty(), "{warned:?}");
        let (html, coverage, warned) = check(None);
        assert!(html.contains(r#"alt="""#), "{html}");
        assert_eq!(coverage, (1, 2));
        assert_eq!(warned, ["/chapters/1"]);
    }

    /// Ruling 2 (Task 19 review F2), validation's side: a Mapbox sprite
    /// under the fixture basemap is a `sprite` row in the preview table
    /// and Mapbox's credit on the plate the layout probe lays out with --
    /// the flag judged before `check_timeline` takes the style, as the
    /// render's `Player::new` does.
    #[test]
    fn a_mapbox_sprite_under_another_basemap_is_credited_by_validation() {
        let dir = tempfile::tempdir().unwrap();
        let mut style: serde_json::Value = serde_json::from_slice(&std::fs::read(root().join("examples/liberty-style.json")).unwrap()).unwrap();
        style["sprite"] = "mapbox://sprites/mapbox/streets-v12".into();
        std::fs::write(dir.path().join("style.json"), serde_json::to_vec(&style).unwrap()).unwrap();
        let mut v = base(serde_json::json!([{"at": 0.0, "until": 1.0, "panel": {"placement": "corner", "headline": "x"}}]));
        v["scene"]["style"] = "style.json".into();
        let (s, _) = script_of(&v);
        assert!(publish_preview(&s, dir.path(), true).iter().any(|r| r.source == "sprite"));
        let mut inputs = scene_inputs(&s, dir.path(), true).unwrap();
        let tracks = Arc::new(std::mem::take(&mut inputs.tracks));
        let mapbox_sprite = crate::story::burn_in::style_has_mapbox_sprite(&inputs.base_style);
        let base_style = std::mem::take(&mut inputs.base_style);
        assert!(mapbox_sprite && !crate::story::burn_in::style_has_mapbox_sprite(&inputs.base_style), "judged before the take");
        let (path, timeline) =
            crate::story::timeline::path_and_timeline((480.0, 270.0), &s, tracks, &[], base_style, &*inputs.load_style).map_err(|e| e.0).unwrap();
        let probed = crate::story::player::probe_layout(&s, &inputs, &path, &timeline, &[], &crate::story::media::ScriptImages::default(), mapbox_sprite, &[5]);
        let plate = probed[0].attribution.clone().unwrap_or_default();
        assert!(plate.contains(crate::story::burn_in::MAPBOX_CREDIT), "{plate}");
    }

    /// A 10-minute 25 fps story (15,000 frames): 40 panels, 10 captions,
    /// 2 chapters and 10 callouts of `callout_s` seconds each.
    fn ten_minutes(callout_s: f64) -> serde_json::Value {
        let mut events: Vec<serde_json::Value> = (0..40)
            .map(|i| serde_json::json!({"at": f64::from(i) * 15.0, "until": f64::from(i) * 15.0 + 10.0,
                "panel": {"placement": if i % 2 == 0 { "lower-third" } else { "side" }, "headline": format!("Panel {i}"), "body": "Some body text."}}))
            .collect();
        events.extend((0..10).map(|i| serde_json::json!({"at": f64::from(i) * 60.0, "until": f64::from(i) * 60.0 + 5.0, "caption": {"text": format!("Caption {i}"), "anchor": "bottom"}})));
        events.extend((0..10).map(|i| serde_json::json!({"at": f64::from(i) * 60.0 + 20.0, "until": f64::from(i) * 60.0 + 20.0 + callout_s,
            "callout": {"at_coord": [103.855, 1.2925], "text": format!("Callout {i}")}})));
        serde_json::json!({"story":1,"summary":"Ten minutes.","output":{"aspect":"16:9","tier":"hd","fps":25},
          "scene":{"style":"examples/liberty-style.json","basemap":{"source":"fixture"}},
          "camera":[{"type":"jump","to":{"lon":103.851,"lat":1.2905,"zoom":14}},{"type":"orbit","degrees":360,"duration":600.0}],
          "chapters":[{"id":"a","title":"A","from":0.0,"to":300.0,"alt":"a"},{"id":"b","title":"B","from":300.0,"to":600.0,"alt":"b"}],
          "events": events})
    }

    /// The cost is bounded per element, not per frame: panels and images
    /// at most 4 frames each, every callout frame up to
    /// [`MAX_DENSE_CALLOUT_FRAMES`] (an even stride past it), plus chapter,
    /// caption and schedule-change frames. Prints both modes' wall time
    /// (the report records the figures), and on a release build holds the
    /// one without the estimate to spec §14.3's 0.25 s.
    #[test]
    fn a_ten_minute_story_samples_a_bounded_number_of_frames() {
        let timeline_of = |v: &serde_json::Value| {
            let (s, raw) = script_of(v);
            let mut inputs = scene_inputs(&s, &root(), true).unwrap();
            let tracks = Arc::new(std::mem::take(&mut inputs.tracks));
            let base_style = std::mem::take(&mut inputs.base_style);
            let o = s.output();
            let viewport = (o.width as f32 / o.pixel_ratio, o.height as f32 / o.pixel_ratio);
            let (path, timeline) =
                crate::story::timeline::path_and_timeline(viewport, &s, tracks, &[], base_style, &*inputs.load_style).map_err(|e| e.0).unwrap();
            (s, raw, inputs, path, timeline)
        };
        // Ten 10 s callouts: 2,500 frames, every one of them.
        let (s, raw, inputs, path, timeline) = timeline_of(&ten_minutes(10.0));
        let frames = (path.duration() * 25.0).ceil() as usize;
        assert_eq!(frames, 15_000);
        let images = crate::story::media::ScriptImages::default();
        let mut probe = crate::story::player::LayoutProbe::new(&s, &inputs, &path, &timeline, &[], &images, false, &crate::story::RenderOptions::default());
        let changes = probe.callout_changes();
        let samples = layout_sample_frames(&timeline, 25, frames, &changes);
        let callout_frames = 10 * 250;
        for i in 0..10usize {
            let first = (i * 60 + 20) * 25;
            assert!((first..first + 250).all(|n| samples.binary_search(&n).is_ok()), "callout {i}: every frame of its span");
        }
        assert!(samples.len() <= 4 * 60 + 2 * 2 + 2 * 10 + callout_frames + 2 * changes.len(), "{}", samples.len());
        let elapsed = crate::story::player::stopwatch();
        let probed = probe.frames(&samples);
        let layout_s = elapsed();
        assert_eq!(probed.len(), samples.len());
        let timed = |estimate: bool| {
            let o = ValidateOptions { estimate, ..opts() };
            let elapsed = crate::story::player::stopwatch();
            let r = tokio::runtime::Runtime::new().unwrap().block_on(validate_scene(&s, &raw, &o));
            assert!(r.ok, "{:?}", r.diagnostics);
            assert_eq!(r.estimate.is_some(), estimate);
            elapsed()
        };
        let (fast, full) = (timed(false), timed(true));
        eprintln!(
            "10-minute story: {} of {frames} frames laid out in {layout_s:.2} s; validate_scene {fast:.2} s without the estimate, {full:.2} s with it",
            samples.len()
        );
        // Spec §14.3's budget for the 10-minute reference story: 0.25 s on
        // a release build, validated without the estimate. A debug build
        // (~0.2 s alone, more under a loaded full run) is not graded.
        if !cfg!(debug_assertions) {
            assert!(fast <= 0.25, "10-minute story validated in {fast:.3} s: over spec §14.3's 0.25 s budget");
        }

        // Ten callouts to the end (76,500 callout frames), strided to the cap.
        let (_, _, _, path, timeline) = timeline_of(&ten_minutes(540.0));
        let frames = (path.duration() * 25.0).ceil() as usize;
        let samples = layout_sample_frames(&timeline, 25, frames, &[]);
        assert!(samples.len() <= 4 * 60 + 2 * 2 + 2 * 10 + MAX_DENSE_CALLOUT_FRAMES + 10, "{}", samples.len());
        // 76,500 callout frames over a 4,000 cap: a stride of 20 frames, and
        // no callout frame is further than that from a sampled one (the
        // overlapping spans share their strided frames).
        let in_callouts: Vec<usize> = samples.iter().copied().filter(|&n| n >= 500).collect();
        let widest = in_callouts.windows(2).map(|w| w[1] - w[0]).max().unwrap();
        assert!(widest <= 20, "a {widest}-frame gap between callout samples");
        assert_eq!(*in_callouts.last().unwrap(), frames - 1);
    }

    /// Validation is deterministic: the same script gives the same JSON.
    #[test]
    fn layout_validation_is_deterministic() {
        let v = vertical(serde_json::json!([
            {"at": 0.0, "until": 2.0, "panel": {"placement": "lower-third", "headline": long_headline()}},
            {"at": 0.5, "until": 2.0, "callout": {"at_coord": [103.871, 1.2905], "text": "A long callout about the bay and its history. ".repeat(40)}}
        ]));
        let json = |r: Report| {
            let mut v = serde_json::to_value(&r).unwrap();
            v["ffmpeg"] = serde_json::Value::Null;
            v
        };
        assert_eq!(json(run(&v)), json(run(&v)));
    }

    /// The rows of the markdown table under `heading` in `doc` (`//! `
    /// stripped for the module doc), as `(code, severity, raised by)`.
    fn table_rows(doc: &str) -> Vec<(String, String, String)> {
        doc.lines()
            .map(|l| l.trim_start_matches("//!").trim())
            .filter(|l| l.starts_with("| `"))
            .map(|l| {
                let cells: Vec<&str> = l.trim_matches('|').split('|').map(str::trim).collect();
                (cells[0].trim_matches('`').to_string(), cells[1].to_string(), cells[2].to_string())
            })
            .collect()
    }

    /// The module doc's table, `docs/story-validation.md`'s and
    /// [`WARNING_CODES`] are one list; the layout's own code list
    /// (`LayoutWarning::code`'s doc) is exactly the table's `layout` rows;
    /// and every warning `validate.rs` and `media.rs` raise is in it.
    #[test]
    fn the_warning_table_is_the_code_list() {
        let ours: Vec<(String, String, String)> = WARNING_CODES
            .iter()
            .map(|w| (w.code.to_string(), if w.severity == Severity::Info { "info" } else { "warning" }.to_string(), w.raised_by.to_string()))
            .collect();
        let module = include_str!("validate.rs");
        let module_doc: String = module.lines().take_while(|l| l.starts_with("//!")).collect::<Vec<_>>().join("\n");
        assert_eq!(table_rows(&module_doc), ours, "the module doc's table");
        let docs = std::fs::read_to_string(root().join("docs/story-validation.md")).unwrap();
        assert_eq!(table_rows(&docs), ours, "docs/story-validation.md's table");
        let mut codes: Vec<&str> = WARNING_CODES.iter().map(|w| w.code).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), WARNING_CODES.len(), "a code is listed twice");

        // The layout's own list of codes.
        let layout = include_str!("layout.rs");
        let doc = &layout[layout.find("pub struct LayoutWarning").unwrap()..];
        let doc = &doc[..doc.find("pub code").unwrap()];
        let mut layout_codes: Vec<&str> = doc.split('`').skip(1).step_by(2).filter(|c| c.chars().all(|ch| ch.is_ascii_lowercase() || ch == '_'))
            // The function the text codes come from, named in the list.
            .filter(|c| *c != "fit_text")
            .collect();
        layout_codes.sort_unstable();
        let mut table_layout: Vec<&str> = WARNING_CODES.iter().filter(|w| w.raised_by == "layout").map(|w| w.code).collect();
        table_layout.sort_unstable();
        assert_eq!(layout_codes, table_layout, "layout.rs's LayoutWarning codes");

        // Every place a warning or a note is built, in every non-test
        // `src/story` source, names a tabled code in its own arguments.
        let mut literals_anywhere: Vec<String> = Vec::new();
        let mut dir: Vec<_> = std::fs::read_dir(root().join("src/story")).unwrap().map(|e| e.unwrap().path()).collect();
        dir.sort();
        let tabled = |c: &str| WARNING_CODES.iter().any(|w| w.code == c);

        // `label_under_card`/`story_labels_capped` (spec §7): built as
        // bare `crate::story::layout::LayoutWarning { code: "…", .. }`
        // literals in `src/capture/story_overlays.rs`, outside the
        // `src/story` directory the scan below covers and not a
        // `Severity::Warning`/`warn(`/`warning(` shape `construction_sites`
        // recognises -- so this table, not that scan, is their only
        // check (R10 review round 1).
        for code in label_warning_codes_in(&root().join("src/capture/story_overlays.rs")) {
            assert!(tabled(&code), "story_overlays.rs: LayoutWarning code `{code}` is not in WARNING_CODES");
            literals_anywhere.push(code);
        }

        for file in dir.iter().filter(|p| p.extension().is_some_and(|e| e == "rs")) {
            let name = file.file_name().unwrap().to_string_lossy().into_owned();
            let full = std::fs::read_to_string(file).unwrap();
            let src = &full[..test_module_start(&full)];
            let (masked, lits) = mask_source(src);
            for (at, span) in construction_sites(&masked) {
                let inside: Vec<&str> = lits.iter().filter(|(p, _)| span.contains(p)).map(|(_, l)| l.as_str()).collect();
                literals_anywhere.extend(inside.iter().map(|l| l.to_string()));
                if inside.iter().any(|l| tabled(l)) || dynamic_site_is_covered(&name, &masked, span.clone(), &lits) {
                    continue;
                }
                let line = src[..at].lines().count();
                panic!("{name}:{line}: a warning or note whose own arguments name no WARNING_CODES code: {:?}", &src[span]);
            }
            // A code passed as a variable comes from a `for code in [..]`
            // list or Task 10's `fit_text` (`Some("text_..")`): each of those
            // literals is tabled.
            for span in for_code_lists(&masked) {
                for (_, l) in lits.iter().filter(|(p, _)| span.contains(p)) {
                    literals_anywhere.push(l.clone());
                    assert!(tabled(l), "{name}: `for code in` names `{l}`, which is not in WARNING_CODES");
                }
            }
        }
        let panel = include_str!("panel.rs");
        let fit = &panel[panel.find("pub fn fit_text").unwrap()..];
        let fit = &fit[..fit.find("\n}\n").unwrap()];
        let fit_codes: Vec<&str> = fit
            .split("Some(\"")
            .skip(1)
            .map(|rest| &rest[..rest.find('"').unwrap()])
            .filter(|c| !c.is_empty() && c.chars().all(|ch| ch.is_ascii_lowercase() || ch == '_'))
            .collect();
        assert_eq!(fit_codes.len(), 3, "{fit_codes:?}");
        for code in fit_codes {
            assert!(tabled(code), "panel::fit_text returns `{code}`, which is not in WARNING_CODES");
            literals_anywhere.push(code.to_string());
        }
        // And no row is stale: each code is raised -- a literal in some
        // site's own arguments, a `for code in` list or `fit_text` -- not
        // merely mentioned (the table and the hints are not sources).
        for w in WARNING_CODES {
            assert!(literals_anywhere.iter().any(|l| l == w.code), "`{}` is tabled but nothing in src/story raises it", w.code);
        }
    }

    /// Where `src`'s first test-only module starts: a top-level
    /// `#[cfg(test)]` (other attributes may follow) on a `mod`. A test-only
    /// item inside the code (a `#[cfg(test)] fn`) is not a cut.
    fn test_module_start(src: &str) -> usize {
        let mut at = 0;
        for line in src.split_inclusive('\n') {
            if line.starts_with("#[cfg(test)]") {
                let rest = src[at + line.len()..].lines().find(|l| !l.starts_with("#["));
                if rest.is_some_and(|l| l.contains("mod ")) {
                    return at;
                }
            }
            at += line.len();
        }
        src.len()
    }

    /// `src` with every string literal and comment blanked to spaces
    /// (offsets kept), and every string literal's `(offset, text)`.
    fn mask_source(src: &str) -> (String, Vec<(usize, String)>) {
        let b = src.as_bytes();
        let mut out = b.to_vec();
        let mut lits = Vec::new();
        let mut i = 0;
        let blank = |out: &mut Vec<u8>, from: usize, to: usize| {
            for c in &mut out[from..to] {
                if *c != b'\n' {
                    *c = b' ';
                }
            }
        };
        while i < b.len() {
            if b[i..].starts_with(b"//") {
                let end = src[i..].find('\n').map_or(b.len(), |n| i + n);
                blank(&mut out, i, end);
                i = end;
            } else if b[i] == b'r' && (b[i..].starts_with(b"r\"") || b[i..].starts_with(b"r#")) && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_')) {
                let hashes = b[i + 1..].iter().take_while(|c| **c == b'#').count();
                if b.get(i + 1 + hashes) != Some(&b'"') {
                    i += 1;
                    continue;
                }
                let close = format!("\"{}", "#".repeat(hashes));
                let body = i + 2 + hashes;
                let end = src[body..].find(&close).map_or(b.len(), |n| body + n);
                lits.push((i, src[body..end].to_string()));
                let stop = (end + close.len()).min(b.len());
                blank(&mut out, i, stop);
                i = stop;
            } else if b[i] == b'"' {
                let mut j = i + 1;
                while j < b.len() && b[j] != b'"' {
                    j += if b[j] == b'\\' { 2 } else { 1 };
                }
                lits.push((i, src[i + 1..j.min(b.len())].to_string()));
                let stop = (j + 1).min(b.len());
                blank(&mut out, i, stop);
                i = stop;
            } else if b[i] == b'\'' {
                // A char literal ('x', '\n', '"'); a lifetime has no closing quote.
                let len = if b.get(i + 1) == Some(&b'\\') { b[i + 2..].iter().position(|c| *c == b'\'').map(|p| p + 3) } else { (b.get(i + 2) == Some(&b'\'')).then_some(3) };
                match len {
                    Some(n) => {
                        blank(&mut out, i, i + n);
                        i += n;
                    }
                    None => i += 1,
                }
            } else {
                i += 1;
            }
        }
        (String::from_utf8(out).unwrap(), lits)
    }

    /// Every `code` string [`crate::story::layout::LayoutWarning`] is
    /// built with as a bare struct literal (`LayoutWarning { .., code:
    /// "…", .. }`) in `path`'s non-test code (R10 review round 1): finds
    /// each `LayoutWarning {`'s matching `}`, then the literal right
    /// after a `code:` inside that span (never `message:`'s or `id:`'s,
    /// which have string literals of their own in the same span).
    fn label_warning_codes_in(path: &Path) -> Vec<String> {
        let full = std::fs::read_to_string(path).unwrap();
        let src = &full[..test_module_start(&full)];
        let (masked, lits) = mask_source(src);
        let mut codes = Vec::new();
        for (at, _) in masked.match_indices("LayoutWarning {") {
            let open = at + "LayoutWarning {".len() - 1;
            let span = matching(&masked, open);
            for (p, code) in &lits {
                if span.contains(p) && src[..*p].trim_end().ends_with("code:") {
                    codes.push(code.clone());
                }
            }
        }
        codes
    }

    /// The span from `open` (a `(`, `[` or `{`) to its matching close.
    fn matching(m: &str, open: usize) -> std::ops::Range<usize> {
        let mut depth = 0i32;
        for (k, c) in m[open..].char_indices() {
            match c {
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return open..open + k + 1;
                    }
                }
                _ => {}
            }
        }
        open..m.len()
    }

    /// Every place a diagnostic of severity warning or info is built, as
    /// `(offset, argument span)`: a `Severity::Warning`/`Severity::Info`
    /// (not a comparison or a match arm) in its innermost `(..)`/`{..}`,
    /// and every call of `warning(`, `Diagnostic::warning(` or `warn(`
    /// (not its definition). The table's own rows are not sites.
    fn construction_sites(m: &str) -> Vec<(usize, std::ops::Range<usize>)> {
        let mut out = Vec::new();
        for needle in ["Severity::Warning", "Severity::Info"] {
            for (at, _) in m.match_indices(needle) {
                let before = m[..at].trim_end();
                let after = m[at + needle.len()..].trim_start();
                if before.ends_with("==") || before.ends_with("!=") || before.ends_with('|') || after.starts_with("=>") || after.starts_with('|') {
                    continue;
                }
                let mut depth = 0i32;
                let open = m[..at].char_indices().rev().find(|(_, c)| {
                    match c {
                        ')' | ']' | '}' => depth += 1,
                        '(' | '[' | '{' if depth == 0 => return true,
                        '(' | '[' | '{' => depth -= 1,
                        _ => {}
                    }
                    false
                });
                let Some((open, _)) = open else { continue };
                if m[..open].trim_end().ends_with("WarningCode") {
                    continue;
                }
                out.push((at, matching(m, open)));
            }
        }
        for name in ["warning(", "warn("] {
            for (at, _) in m.match_indices(name) {
                let prev = m[..at].chars().next_back();
                if prev.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.') && !m[..at].ends_with("Diagnostic::") {
                    continue;
                }
                if m[..at].trim_end().ends_with("fn") {
                    continue;
                }
                out.push((at, matching(m, at + name.len() - 1)));
            }
        }
        out
    }

    /// Every `for code in [..]` list's span.
    fn for_code_lists(m: &str) -> Vec<std::ops::Range<usize>> {
        m.match_indices("for code in [").map(|(at, n)| matching(m, at + n.len() - 1)).collect()
    }

    /// A site whose code is a variable. Two kinds are covered elsewhere:
    /// a builder (a `fn` or closure taking `code: &'static str`, like
    /// `chapters::warning`, `Diagnostic::warning` or `media.rs`'s `warn`)
    /// whose every call is a site of its own; and `layout.rs`'s
    /// `warn(.., code, ..)` with `code` from a `for code in [..]` list or
    /// `fit_text`, both checked by the caller. (And `warn_layout`'s
    /// severity lookup in the table.) Only a span that passes `code` as a
    /// bare argument or field, with no code-like literal of its own,
    /// counts.
    fn dynamic_site_is_covered(file: &str, m: &str, span: std::ops::Range<usize>, lits: &[(usize, String)]) -> bool {
        let text = &m[span.clone()];
        let bare = text.contains(", code,") || text.contains(" code,\n") || text.contains("(code,") || text.contains(" code, ");
        let no_literal = !lits.iter().any(|(p, l)| span.contains(p) && !l.is_empty() && l.chars().all(|c| c.is_ascii_lowercase() || c == '_'));
        let head = &m[span.start.saturating_sub(300)..span.start];
        let builder = head.contains("code: &'static str") && head.rfind("code: &'static str").is_some_and(|at| !head[at..].contains(';'));
        // `warn_layout`'s translation takes each layout code's severity from
        // the table itself (the layout's codes are checked against it above).
        let from_table = file == "validate.rs" && text.contains("|c| c.severity");
        from_table || (bare && no_literal && (builder || file == "layout.rs"))
    }
}

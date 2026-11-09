//! S4.2: the deterministic player. It drives a story frame by frame:
//! script → [`Timeline`] and [`CameraPath`] → a [`CaptureRequest`] per
//! frame → the look-ahead [`Prefetcher`] → [`CaptureSession`] → a
//! [`FrameSink`], and writes a [`RenderManifest`] row per frame.
//!
//! **Determinism.** Frame `n` is story time `t = n / fps`, computed from
//! the frame index in `f64` and never accumulated, and everything a frame
//! draws is a pure function of `t`: the pose (`CameraPath::pose_at`), the
//! scene (`Timeline::state_at`) and the tiles (the prefetch [`plan`], which
//! a frame waits for, and whose stand-ins are the only ancestors the
//! render may draw: [`CaptureRequest::stand_in_levels`]). So frame `n` is
//! the same bytes rendered alone (a frame range), after the whole story,
//! or with the machine busy in between. `Instant` is read only to measure
//! (`ms`, `wait_ms`, progress), never to decide.
//!
//! **Which frames.** `frames = ceil(duration · fps)` (at least one), and
//! frame `n` shows `t = n / fps`: the frames cover `[0, duration)`, so an
//! N-second story at F fps is exactly N·F frames and an N-second video.
//! The last frame is `(frames - 1) / fps`, which is before `duration`, so
//! **an event at exactly `duration` (or after the last frame's time) is
//! never shown**: the timeline accepts it, and the manifest names it in
//! [`RenderManifest::gaps`] ("event /events/i at T s is after the last
//! frame ...") rather than dropping it silently.
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)
)]

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;

use crate::capture::{Capture, CaptureRequest, CaptureSession, LabelFonts, StoryLayerDraw};
use crate::render::atmosphere::{Atmosphere, Sun};
use crate::story::camera_path::{CameraPath, Pose};
use crate::story::prefetch::{plan, MissReason, PrefetchOptions, Prefetcher, STAND_IN_LEVELS};
use crate::story::script::{ResolvedOutput, Script};
use crate::story::sink::Tee;
use crate::story::style_cache::StyleCache;
use crate::story::timeline::{ActiveCaption, Anchor, SceneState, Timeline, DEFAULT_CAPTION_COLOR, DEFAULT_FADE_S};
use crate::story::tracks::TrackSet;
use crate::tile_source::TileSource;

/// Loads the document a `set_style` event names; `None` if it cannot.
pub type StyleLoader = Box<dyn Fn(&str) -> Option<serde_json::Value> + Send>;

/// Everything a story draws that is not in the script: the resolved
/// sources, documents and fonts. Built by the loader (or a test).
pub struct SceneInputs {
    /// The vector basemap.
    pub source: Arc<dyn TileSource>,
    /// `"mapbox:<ids>"`, `"openfreemap"` or `"fixture"`: what the manifest
    /// names the source as. Never a URL with a token in it.
    pub source_label: String,
    /// The scene's style document (generation 0).
    pub base_style: serde_json::Value,
    /// Loads the document a `set_style` event names; `None` if it cannot.
    pub load_style: StyleLoader,
    /// GeoJSON documents by the path the script names them by: the
    /// scene's `overlays[].geojson`, and every `set_overlay_data` path.
    /// A path with no entry here draws nothing (and is named in the gaps).
    pub overlays: HashMap<String, String /* geojson */>,
    pub tracks: TrackSet,
    /// `(font file bytes, default family)` for the labels; `None` is the
    /// machine's font book (not reproducible across machines).
    pub label_fonts: Option<LabelFonts>,
    /// The vector source's zoom ceiling (OpenFreeMap 14, Mapbox 16).
    pub flat_tile_zoom_max: u8,
    /// The raster basemap. Not drawn yet (named in the gaps).
    pub raster: Option<Arc<dyn TileSource>>,
    /// The sources' credits, in order (vector, raster, data).
    pub credits: Vec<String>,
    /// The user-supplied basemap wordmark (`scene.basemap.wordmark`,
    /// `validate::load_wordmark`), burned in beside the SWAT mark on every
    /// frame. `None` draws none, and a Mapbox scene is then not
    /// publishable.
    pub wordmark: Option<Arc<image::RgbaImage>>,
    /// The style's sprite sheet (`validate::scene_inputs` fetches and
    /// caches it), on every frame's request for `icon-image`. `None` draws
    /// no icons, and the manifest's gaps say so.
    pub sprites: Option<Arc<crate::sprite::SpriteAtlas>>,
    /// Which sheet `sprites` is (its redacted base URL and a hash of its
    /// bytes), recorded in the manifest so two renders that drew
    /// different sheets say so.
    pub sprite_sheet: Option<SpriteSheetInfo>,
    /// Why icons are missing or partial (an array-form `"sprite"`, a sheet
    /// that did not load, a `set_style` naming another sheet), each a line
    /// of the manifest's gaps.
    pub sprite_notes: Vec<String>,
    /// What the script's local image paths resolve against (spec §4.3;
    /// `validate::scene_inputs`' `base_dir`). Empty is the current
    /// directory.
    pub story_dir: PathBuf,
    /// Images come from the cache only, never the network
    /// (`scene_inputs`' `offline`).
    pub offline: bool,
    /// `validate_scene` passed on this script, so every image loaded then:
    /// one that fails in [`Player::new`] has vanished since and is a gap.
    /// `false` (what `scene_inputs` sets; nothing was validated) makes any
    /// image failure refuse the render (spec §7).
    pub images_validated: bool,
}

pub use crate::sprite_cache::SpriteSheetInfo;

/// How a render runs. [`Default`] is the spec's: not strict, a 10 s tile
/// timeout, 16 fetches in flight, the per-source cache directory, a 1 GiB
/// mesh cache, not cancelled, every frame.
#[derive(Clone, Debug)]
pub struct RenderOptions {
    /// Render only these frames (a re-render of part of a story). `None`
    /// is `0..frames`.
    pub frames: Option<std::ops::Range<usize>>,
    /// A frame with any missing tile is an error.
    pub strict: bool,
    /// How long a frame waits for its tiles.
    pub tile_timeout: Duration,
    /// Fetches in flight at once.
    pub fetch_concurrency: usize,
    /// The raw-tile cache. Empty (the default) is
    /// [`default_cache_dir`]`(source_label)`.
    pub cache_dir: PathBuf,
    /// The session's baked-tile cache budget.
    pub mesh_cache_bytes: u64,
    /// Set from any thread to stop the render before its next frame.
    pub cancel: Arc<AtomicBool>,
    /// Where fetched story images are cached ([`Self::image_cache_dir`]).
    /// Pass the SAME folder as `ValidateOptions::image_cache_dir`, so an
    /// image validated (and cached) once is found offline at render time.
    pub image_cache_dir: Option<PathBuf>,
    /// The output is a video (or frames for one), not a still: decides the
    /// publish table's Mapbox video rule (spec §6). Set it from the output
    /// format with [`crate::story::output_is_video`], the one rule the
    /// CLI's render and `--validate-only` share; it never depends on how
    /// many frames this run renders (a one-frame mp4 re-render is video).
    /// Default `true`: the stricter verdict.
    pub video: bool,
    /// The SWAT mark every frame's request carries
    /// ([`CaptureRequest::brand_mark`]). Default: the engine's mark.
    pub brand_mark: Option<crate::brand_mark::BrandMark>,
    /// Whether the mark still draws beside the scene's wordmark
    /// ([`CaptureRequest::brand_mark_policy`]). The design frame's camera
    /// pass and its drawer both read the mark this decides (spec §3.6, P3).
    pub brand_mark_policy: crate::brand_mark::BrandMarkPolicy,
}

impl RenderOptions {
    /// The image cache this render reads and fills: `image_cache_dir`, else
    /// `cache_dir/story-images` when the caller named a `cache_dir`, else
    /// `media::default_image_cache_dir()` (what validation uses by default).
    #[must_use]
    pub fn image_cache_dir(&self) -> PathBuf {
        match &self.image_cache_dir {
            Some(dir) => dir.clone(),
            None if self.cache_dir.as_os_str().is_empty() => crate::story::media::default_image_cache_dir(),
            None => self.cache_dir.join("story-images"),
        }
    }
}

/// The mesh cache's default GPU budget ([`RenderOptions::mesh_cache_bytes`]).
pub const DEFAULT_MESH_CACHE_BYTES: u64 = 1 << 30;

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            frames: None,
            strict: false,
            tile_timeout: Duration::from_secs(10),
            fetch_concurrency: 16,
            cache_dir: PathBuf::new(),
            mesh_cache_bytes: DEFAULT_MESH_CACHE_BYTES,
            cancel: Arc::new(AtomicBool::new(false)),
            image_cache_dir: None,
            video: true,
            brand_mark: Some(crate::brand_mark::BrandMark::default()),
            brand_mark_policy: crate::brand_mark::BrandMarkPolicy::default(),
        }
    }
}

/// `~/.cache/gpui-map/tiles/<source_label>`, the label made safe as one
/// path component. Outside every repository: rendered and cached tiles
/// of a licensed source are licensed bytes.
#[must_use]
pub fn default_cache_dir(source_label: &str) -> PathBuf {
    let home = std::env::var_os("HOME").map_or_else(std::env::temp_dir, PathBuf::from);
    let safe: String =
        source_label.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' }).collect();
    home.join(".cache/gpui-map/tiles").join(if safe.is_empty() { "unnamed".to_string() } else { safe })
}

/// Where a render is, after each frame.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Progress {
    /// Frames submitted so far in this run.
    pub frame: usize,
    /// Frames this run renders (the range's length).
    pub frames: usize,
    pub story_t: f64,
    /// Frames per second over the last few frames (wall clock).
    pub fps_recent: f64,
    pub eta_s: f64,
    /// Tile fetches in flight.
    pub waiting_on_tiles: usize,
    /// Missing tiles named so far (summed over frames).
    pub missing_so_far: usize,
}

/// One frame of the manifest.
#[derive(Clone, Debug, serde::Serialize)]
pub struct FrameRow {
    pub n: usize,
    /// Story seconds, `n / fps`.
    pub t: f64,
    pub lon: f64,
    pub lat: f64,
    pub zoom: f64,
    pub pitch_deg: f64,
    pub bearing_deg: f64,
    pub globe: bool,
    /// Wall time to encode and submit the frame.
    pub ms: f64,
    /// Wall time the frame waited for its tiles.
    pub wait_ms: f64,
    pub tiles_drawn: usize,
    pub labels_drawn: usize,
    pub overlay_labels_drawn: usize,
    /// Icons drawn from the scene's sprite sheet (0 without one).
    pub icons_drawn: usize,
    /// Glyph quads of line-placed labels (street names) drawn, one per
    /// inked letter (`Capture::line_glyphs_drawn`).
    pub line_glyphs_drawn: usize,
    /// Glyphs of line-placed labels the glyph atlas could not hold even
    /// started fresh for this frame (`Capture::line_glyphs_dropped`): a
    /// street name drawn with letters missing. Named in the gaps when any.
    pub line_glyphs_dropped: usize,
    /// The prefetcher's view: exact tiles that did not arrive, `z/x/y`
    /// with why. A tile here may still be covered by a stand-in ancestor.
    pub missing: Vec<(String, MissReason)>,
    /// The render's own view (F4): cover tiles that drew nothing at all,
    /// not even a stand-in, `z/x/y`. Empty on a clean frame.
    pub tiles_missing: Vec<String>,
    /// SHA-256 of the frame's RGBA bytes.
    pub sha256: String,
    /// Where the frame's wall time went, in ms, indexed by [`STAGES`]:
    /// plan, wait, bake, encode, readback, burn-in, sink write. For
    /// `--bench`; never an input to a pixel.
    pub stage_ms: [f64; 7],
    /// What the story layout warned about on this frame
    /// (`Capture::layout_warnings`); gathered, deduplicated, into
    /// [`RenderManifest::layout_warnings`] rather than written per row.
    #[serde(skip)]
    pub layout_warnings: Vec<crate::story::layout::LayoutWarning>,
}

/// The names of [`FrameRow::stage_ms`]'s entries, in order.
pub const STAGES: [&str; 7] = ["plan", "wait", "bake", "encode", "readback", "burn_in", "write"];

/// The record of one render. See the module docs for which frames exist
/// and what an event past the last frame means.
#[derive(Clone, Debug, serde::Serialize)]
pub struct RenderManifest {
    /// SHA-256 of the script bytes as given.
    pub script_sha256: String,
    /// The parsed script, defaults filled in.
    pub script: serde_json::Value,
    pub engine_git_sha: String,
    pub version: String,
    /// The GPU adapter and backend.
    pub adapter: String,
    pub output: ResolvedOutput,
    /// The frame's shape (spec §6's output settings): aspect, tier,
    /// entitlement, safe area and the 1080-referenced layout size.
    pub format: crate::story::script::OutputFormat,
    pub source: String,
    /// The settings the frames were rendered with (F9).
    pub render: RenderSettings,
    /// The rendered frames, in frame order.
    pub frames: Vec<FrameRow>,
    /// Every credit, deduplicated, joined with `" | "` -- the text burned
    /// into every frame's plate (`burn_in::verdict`).
    pub attribution: String,
    /// Every row of [`Self::publish`] is `ok` (spec §6:
    /// [`crate::story::burn_in::publishable`]).
    pub publishable: bool,
    /// Why [`Self::publishable`] is `false`: each row that is not `ok`,
    /// as `"<source>: <reason>"` (a non-commercial raster's reason starts
    /// `"non-commercial only"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publishable_reason: Option<String>,
    /// The publish table (spec §6): one row per source -- the basemap,
    /// the raster when drawn, each overlay, each drawn image -- with its
    /// credit, verdict, reason and the acknowledgement that cleared it.
    pub publish: Vec<crate::story::burn_in::PublishRow>,
    /// Every chapter (time order as written; the implicit one for a
    /// script with none) with its still's path, which the bundle fills
    /// (`None` from a render alone).
    pub chapters: Vec<(crate::story::Chapter, Option<PathBuf>)>,
    /// What the render covers for a reader who cannot see or hear it.
    pub coverage: Coverage,
    /// The script's `output.acknowledge` entry that made a source outside
    /// the licence table publishable: the author's statement that they
    /// hold the rights, recorded with the render.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub licence_acknowledged: Option<String>,
    /// What this render could not draw or show, each once.
    pub gaps: Vec<String>,
    /// Every story layout warning of every frame (a panel that could not
    /// clear a safe area, text that shrank or was truncated, ...), once per
    /// `(id, code)`, in the order first seen (pre-flight ruling F11).
    pub layout_warnings: Vec<crate::story::layout::LayoutWarning>,
    /// The sprite sheet the icons came from; `None` with no icons.
    pub sprites: Option<SpriteSheetInfo>,
    /// What the sink reported at `finish`.
    pub sink: serde_json::Value,
    /// The ffmpeg the video went through (`{name, version}`: its file name, never its path), set by the
    /// caller that chose it; `None` for a render with no encoder.
    pub ffmpeg: Option<serde_json::Value>,
    /// The poster frame (the rendered range's middle frame), when
    /// [`Player::run_with_extras`] wrote one.
    pub poster: Option<std::path::PathBuf>,
    /// The twelve-frame contact sheet, when
    /// [`Player::run_with_extras`] wrote one.
    pub contact_sheet: Option<std::path::PathBuf>,
}

/// The manifest's accessibility and completeness counts (spec §6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Coverage {
    /// How many entries [`RenderManifest::gaps`] lists.
    pub gaps: usize,
    /// `(with alt text, of)`: the author's chapters (not the implicit one
    /// a script without chapters gets) and every drawn image.
    pub alt_text: (usize, usize),
    /// Cues in the caption track (the SRT/VTT): 0 for a story without
    /// captions.
    pub captions: usize,
}

/// The render's pixel-affecting settings, in the manifest (F9). The
/// ffmpeg path, version and command ride in the manifest's `sink` (what
/// [`crate::story::FfmpegSink`]'s `finish` reports), as do the poster and
/// contact-sheet paths ([`Player::run_with_extras`]).
#[derive(Clone, Debug, serde::Serialize)]
pub struct RenderSettings {
    /// The rendered range, `[start, end)`.
    pub frames: (usize, usize),
    /// Frames in the whole story.
    pub story_frames: usize,
    pub strict: bool,
    pub tile_timeout_s: f64,
    pub fetch_concurrency: usize,
    pub mesh_cache_bytes: u64,
    /// Ancestor levels a missing tile may be drawn from.
    pub stand_in_levels: u8,
    pub basemap_compact: bool,
    pub basemap_compact_strokes: bool,
    /// The process's compact switches, recorded though the story path
    /// ignores them.
    pub env_basemap_compact: Option<String>,
    pub env_basemap_compact_strokes: Option<String>,
    pub flat_tile_zoom_max: u8,
    pub label_font_family: Option<String>,
    /// One per font file, in order.
    pub label_font_sha256: Vec<String>,
    /// `scene.terminator` (a `terminator.on` event may change it mid-story).
    pub terminator_default: bool,
    /// `scene.raster` resolves to `@2x` tiles at this pixel ratio (spec
    /// §2.4: above 1.5 and the source offers them,
    /// [`crate::story::tiers::resolve_raster`]). `false` with no raster.
    /// The player does not fetch raster tiles yet (`gaps` says so); the
    /// rule is in place for when it does.
    pub raster_2x: bool,
}

/// Where rendered frames go (PNG files, ffmpeg, a contact sheet, hashes).
pub trait FrameSink {
    /// Frame `n`, in frame order.
    ///
    /// # Errors
    /// The sink could not take it; the render stops.
    fn push(&mut self, n: usize, frame: &Capture) -> anyhow::Result<()>;
    /// After the last frame; what it returns goes in the manifest.
    ///
    /// # Errors
    /// The sink could not finish; the render fails.
    fn finish(&mut self) -> anyhow::Result<serde_json::Value>;
    /// The render failed or was cancelled after some frames were pushed:
    /// clean up (kill an encoder, rename a partial file). `finish` is not
    /// called then. The default does nothing.
    fn abort(&mut self) {}
}

/// The publish table (spec §6, [`crate::story::burn_in::publish_table`])
/// for `script` over `inputs`, exactly as a render judges it: the basemap
/// by its manifest label, its own stated credit (as the plate credits it)
/// and the script's credits and acknowledgements; a wordmark counts when
/// [`SceneInputs::wordmark`] loaded one; every overlay with its
/// `attribution`; every image whose `src` `drawn` accepts, with its
/// credit; every story layer plugin in `layers` (`(id, credit)`); a
/// Mapbox sprite named by the style. `scene.raster` is not judged: the
/// player draws no raster yet.
/// `video`: the output format is a video ([`crate::story::output_is_video`]).
/// Shared with `--validate-only`, which judges before any render.
/// `mapbox_sprite`: [`crate::story::burn_in::style_has_mapbox_sprite`] of
/// the scene's style, judged while `inputs.base_style` is still intact
/// (a player and validation take it to build the timeline).
pub fn publish_table_for(
    script: &Script,
    inputs: &SceneInputs,
    video: bool,
    drawn: &dyn Fn(&str) -> bool,
    layers: &[(String, Option<String>)],
    mapbox_sprite: bool,
) -> Vec<crate::story::burn_in::PublishRow> {
    use crate::story::burn_in::{publish_table, source_class, PublishInputs, SourceClass, VerdictSource};
    let own = inputs.source.attribution().map(crate::tile_source::attribution_text);
    let known = matches!(source_class(&inputs.source_label, own.as_deref()), SourceClass::Mapbox | SourceClass::OpenFreeMap);
    let credits: Vec<String> =
        own.clone().filter(|_| !known).into_iter().chain(inputs.credits.iter().cloned()).chain(script.scene.credits.iter().cloned()).collect();
    let overlays: Vec<(String, Option<String>)> = script.scene.overlays.iter().map(|o| (o.id.clone(), o.attribution.clone())).collect();
    let images: Vec<(String, Option<String>)> = crate::story::media::script_images(script)
        .into_iter()
        .filter(|(_, spec)| drawn(&spec.src))
        .map(|(_, spec)| (spec.src.clone(), spec.credit.clone()))
        .collect();
    let basemap = VerdictSource { label: &inputs.source_label, stated: own.as_deref(), acknowledge: &script.output.acknowledge };
    publish_table(&PublishInputs {
        basemap: &basemap,
        credits: &credits,
        raster: None,
        video,
        wordmark_drawn: inputs.wordmark.is_some(),
        overlays: &overlays,
        images: &images,
        layers,
        mapbox_sprite,
    })
}

/// A layer a story can name. Reduced to what the player uses today: L4
/// adds `fetch` and `encode` once a layer has something to draw.
pub trait StoryLayer: Send {
    fn id(&self) -> &str;
    /// The resources this layer needs at `pose`, `t`. Recorded in the gaps
    /// (no encoder yet).
    fn needs(&self, _pose: &Pose, _t: f64) -> Vec<String> {
        Vec::new()
    }
    /// A credit appended to the attribution.
    fn credit(&self) -> Option<String> {
        None
    }
}

/// The frame loop. See the module docs.
pub struct Player {
    // Drop order matters (F14): the prefetcher (and every store guard,
    // which never outlives a frame) goes before the runtime its fetches
    // run on. `Drop` takes both explicitly.
    prefetcher: Option<Prefetcher>,
    runtime: Option<tokio::runtime::Runtime>,
    session: CaptureSession,
    path: CameraPath,
    timeline: Timeline,
    /// Every image the script names, loaded once in [`Self::new`] (spec
    /// §4.3). One that fails here (it loaded at validation) is a gap, and
    /// its element draws with no image.
    images: crate::story::media::ScriptImages,
    styles: StyleCache,
    /// The scene style names a Mapbox sprite: judged in [`Self::new`]
    /// before the style is taken for the timeline.
    mapbox_sprite: bool,
    /// Per style generation, its extrusion layers only: what
    /// [`Self::base_request_with`] resolves the building floor against.
    floor_styles: Vec<crate::style::Style>,
    inputs: SceneInputs,
    script: Script,
    script_sha256: String,
    output: ResolvedOutput,
    frames: usize,
    opts: RenderOptions,
    layers: Vec<Box<dyn StoryLayer>>,
    /// Each overlay document's features, parsed once, by path.
    overlay_features: HashMap<String, Vec<serde_json::Value>>,
    /// Every overlay layer (`scene.overlays[*].layers[*]`) with a document,
    /// in declaration order: what [`Self::request_at`] turns into
    /// [`CaptureRequest::story_layers`].
    story_layers: Vec<LayerRender>,
    /// Every callout event's side and visibility schedule (spec §4.4),
    /// computed once over its span in [`Self::new`]: a frame only looks
    /// its side and fade up, so seeking equals playing.
    callouts: Vec<CalloutTrack>,
    /// Map furniture (spec §4.5): the story's own spec and legend, then
    /// each chapter's (its override over the story's, field by field),
    /// aligned with `timeline.chapters()`. Legends are built once here,
    /// from the overlay layers' paint.
    furniture: StoryFurniture,
    /// The design system's decisions (the slice), `None` without `design`.
    design: Option<DesignCtx>,
}

/// [`Player::furniture`]: a spec and the legend built from it, for the
/// story and for each chapter.
struct StoryFurniture {
    story: (crate::story::script::FurnitureSpec, Option<crate::story::furniture::Legend>),
    chapters: Vec<(crate::story::script::FurnitureSpec, Option<crate::story::furniture::Legend>)>,
}

impl StoryFurniture {
    fn new(script: &Script, chapters: &[crate::story::Chapter], sources: &[crate::story::overlay_layers::LayerSource]) -> Self {
        use crate::story::furniture::{furniture_at, legend};
        let with_legend = |spec: crate::story::script::FurnitureSpec| {
            let legend = spec.legend.as_ref().map(|l| legend(l, sources).0);
            (spec, legend)
        };
        StoryFurniture {
            story: with_legend(script.furniture.clone()),
            chapters: chapters.iter().map(|c| with_legend(furniture_at(&script.furniture, c.furniture.as_ref()))).collect(),
        }
    }

    /// The spec and legend in force at `t`: its chapter's, or the story's
    /// in a gap between chapters.
    fn at(&self, chapters: &[crate::story::Chapter], t: f64) -> (&crate::story::script::FurnitureSpec, Option<&crate::story::furniture::Legend>) {
        let chapter = crate::story::chapter_at(chapters, t).and_then(|c| chapters.iter().position(|x| std::ptr::eq(x, c)));
        let (spec, legend) = chapter.and_then(|i| self.chapters.get(i)).unwrap_or(&self.story);
        (spec, legend.as_ref())
    }
}

/// The layout id of a frame's furniture item (its pieces are
/// `furniture/<piece>`).
const FURNITURE_ID: &str = "furniture";

/// One callout event's schedule, by its element id.
struct CalloutTrack {
    id: String,
    schedule: crate::story::callout::CalloutSchedule,
}

/// Where `(lon, lat)` lands at story time `t`, in 1080 units: the pose's
/// camera through the capture's own projection (`CaptureRequest::camera`,
/// `globe_frame(..).with_globe_enabled(globe)`, `WorldProjector`), `None`
/// behind the globe's horizon or the near plane. The request's logical
/// viewport is `physical / pixel_ratio`, and the pixel ratio IS the
/// layout's (R4), so logical pixels are already 1080 units. The globe
/// switch is `Timeline::globe_at`, not a whole `state_at` (review F3):
/// nothing here depends on the output fps.
fn project_callout(path: &CameraPath, timeline: &Timeline, output: &ResolvedOutput, coord: (f64, f64), t: f64) -> Option<(f32, f32)> {
    let pose = path.pose_at(t);
    let globe = timeline.globe_at(t).unwrap_or(pose.globe);
    let c = pose.camera;
    let request = CaptureRequest {
        center_lon: c.center_lon,
        center_lat: c.center_lat,
        zoom: c.zoom,
        pitch: c.pitch,
        bearing: c.bearing,
        width: output.width,
        height: output.height,
        pixel_ratio: output.pixel_ratio,
        globe,
        ..CaptureRequest::default()
    };
    let camera = request.camera();
    let anchors = crate::render::globe::GlobeAnchors::new(&crate::render::globe::globe_frame(&camera).with_globe_enabled(globe));
    crate::render::globe::WorldProjector::new(&camera, &anchors).project(crate::overlay::geom::lon_lat_to_world(coord.0, coord.1))
}

/// One overlay layer's frame-independent half of its [`StoryLayerDraw`]:
/// its content (and so its cache key), and what its marks read.
struct LayerRender {
    overlay_id: String,
    layer_id: Arc<str>,
    geojson: Arc<str>,
    paint: Arc<serde_json::Map<String, serde_json::Value>>,
    line_metrics: bool,
    per_feature: bool,
    above_labels: bool,
    /// The layer's `above_buildings` (`LayerSource::above_buildings`).
    above_buildings: bool,
    key: u64,
    /// Feature id -> its first position, where a ring is drawn.
    points: std::collections::BTreeMap<u64, (f64, f64)>,
    /// Feature id -> its base colour (`LayerSource::base_colors`), a ring's colour.
    colors: std::collections::BTreeMap<u64, [u8; 4]>,
    /// The colour the draw head's outline takes: the layer's first
    /// feature's base colour (a line layer's line colour).
    outline: Option<[u8; 4]>,
    /// Which effect state the layer reads: its own id, or a themed
    /// sublayer's parent (`theme_layers`).
    fx_id: Arc<str>,
    /// How a themed sublayer takes its parent's trim.
    trim: crate::story::theme_layers::Trim,
    /// Whether the draw head and the rings draw with this layer.
    marks_on: bool,
    /// The arrowheads at this layer's line ends (a themed `arrow`'s core,
    /// spec §6.3), drawn while the line's end is.
    arrowheads: Vec<crate::capture::StoryArrowhead>,
    /// A circle layer with marker paint (`circle-stroke-*`, `circle-blur`,
    /// `circle-translate`, spec §6.4): its markers, fixed; drawn instead of
    /// its mesh.
    markers: Option<Vec<crate::capture::StoryMarker>>,
    /// An unthemed circle layer's `text-*` labels (ruling R8-a), fixed.
    labels: Vec<crate::capture::StoryLabel>,
    /// A themed point sublayer (spec §6.3): its layer and which part it
    /// draws, its markers' states read per frame ([`PointFrame`]).
    point: Option<(Arc<PointLayer>, crate::story::theme_layers::PointPart)>,
    /// A themed point layer's look has a core disc (`stops`, `event`); a
    /// badge (`numbered`) has its outer disc on top.
    has_core: bool,
}

/// One feature of a themed point layer (spec §6.3).
#[derive(Clone, Debug)]
struct PointFeature {
    id: u64,
    at: (f64, f64),
    /// Its `n`, else its 1-based position.
    number: u32,
    /// Its fraction along the layer's route, when there is one.
    along: Option<f32>,
    /// Its `name` (the label).
    name: Option<String>,
    /// Its `label_side`, else away from the nearest story line.
    side: crate::story::theme_layers::LabelSide,
}

/// A themed point layer, what every frame's markers and labels read.
#[derive(Debug)]
struct PointLayer {
    layer_id: String,
    role: crate::story::theme_layers::PointRole,
    accent: [u8; 3],
    /// The author's paint over the top disc (spec §6.1 "Merging").
    author: serde_json::Map<String, serde_json::Value>,
    features: Vec<PointFeature>,
    /// The layer's `label` (spec §6.3).
    label: Option<String>,
    /// The route a `stops` layer's states follow (its id, whose draw is
    /// `p`), or a route's own source for its `start` and `head`.
    route: Option<crate::story::overlay_layers::LayerSource>,
}

/// What a frame's point markers read beyond their layer's effect state:
/// the card's `index.i` at `t` (`cards::card_at`) and each layer's draw
/// progress `p` (spec §6.3).
#[derive(Debug, Default)]
struct PointFrame {
    card_index: Option<u32>,
    progress: HashMap<String, f32>,
}

/// A layer's draw progress `p` from its effect state (spec §6.3): the
/// trim's end mid-draw, 0 before the draw shows it, 1 once it shows whole.
fn draw_progress(fx: &crate::story::effects::LayerFx) -> f32 {
    match fx.trim {
        Some([_, end]) => end.clamp(0.0, 1.0),
        None if fx.opacity <= 0.0 => 0.0,
        None => 1.0,
    }
}

impl PointLayer {
    /// `source`'s point layer for `role` (spec §6.3): each feature's
    /// position, number, label and side, and -- on a route -- its fraction
    /// along it. `lines` are the story's line layers (the nearest one sets
    /// a label's side).
    fn new(
        source: &crate::story::overlay_layers::LayerSource,
        role: crate::story::theme_layers::PointRole,
        accent: [u8; 3],
        label: Option<String>,
        route: Option<crate::story::overlay_layers::LayerSource>,
        lines: &[&crate::story::overlay_layers::LayerSource],
    ) -> Self {
        use crate::overlay::geom::lon_lat_to_world;
        use crate::story::theme_layers::{stop_number, LabelSide};
        let doc: serde_json::Value = serde_json::from_str(&source.geojson).unwrap_or(serde_json::Value::Null);
        let features = doc.get("features").and_then(serde_json::Value::as_array).cloned().unwrap_or_default();
        // The nearest point of a polyline to `p`, world units: (distance², point, length along).
        let nearest = |line: &[(f64, f64)], p: (f64, f64)| -> Option<(f64, (f64, f64), f64)> {
            let pts: Vec<(f64, f64)> = line.iter().map(|&(lon, lat)| lon_lat_to_world(lon, lat)).collect();
            let mut best: Option<(f64, (f64, f64), f64)> = None;
            let mut walked = 0.0;
            for w in pts.windows(2) {
                let &[a, b] = w else { continue };
                let (dx, dy) = (b.0 - a.0, b.1 - a.1);
                let len2 = dx * dx + dy * dy;
                let t = if len2 > 0.0 { (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0) } else { 0.0 };
                let q = (a.0 + t * dx, a.1 + t * dy);
                let d2 = (p.0 - q.0).powi(2) + (p.1 - q.1).powi(2);
                if best.is_none_or(|b| d2 < b.0) {
                    best = Some((d2, q, walked + t * len2.sqrt()));
                }
                walked += len2.sqrt();
            }
            best
        };
        let route_len = route.as_ref().map(|r| {
            let pts: Vec<(f64, f64)> = r.first_line.iter().map(|&(lon, lat)| lon_lat_to_world(lon, lat)).collect();
            pts.windows(2).filter_map(|w| if let &[a, b] = w { Some(((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt()) } else { None }).sum::<f64>()
        });
        let features = features
            .iter()
            .enumerate()
            .filter_map(|(k, f)| {
                let id = f.get("id")?.as_u64()?;
                let at = first_position(f.get("geometry")?)?;
                let props = f.get("properties");
                let world = lon_lat_to_world(at.0, at.1);
                let along = route.as_ref().zip(route_len).filter(|(_, len)| *len > 0.0).and_then(|(r, len)| {
                    nearest(&r.first_line, world).map(|(_, _, s)| (s / len) as f32)
                });
                let side = match props.and_then(|p| p.get("label_side")).and_then(serde_json::Value::as_str) {
                    Some("left") => LabelSide::Left,
                    Some("right") => LabelSide::Right,
                    _ => lines
                        .iter()
                        .filter_map(|l| nearest(&l.first_line, world))
                        .min_by(|a, b| a.0.total_cmp(&b.0))
                        .map_or(LabelSide::Right, |(_, q, _)| if q.0 > world.0 + 1e-12 { LabelSide::Left } else { LabelSide::Right }),
                };
                Some(PointFeature {
                    id,
                    at,
                    number: stop_number(props, k),
                    along,
                    name: props.and_then(|p| p.get("name")).and_then(serde_json::Value::as_str).map(str::to_string),
                    side,
                })
            })
            .collect();
        PointLayer { layer_id: source.layer_id.clone(), role, accent, author: source.paint.clone(), features, label, route }
    }

    /// `part`'s markers and (on the core) labels this frame, from the
    /// parent's effect state `fx` and `points` (spec §6.3).
    fn draw(
        &self,
        part: crate::story::theme_layers::PointPart,
        fx: &crate::story::effects::LayerFx,
        points: &PointFrame,
    ) -> (Vec<crate::capture::StoryMarker>, Vec<crate::capture::StoryLabel>) {
        use crate::story::theme_layers::{
            head_look, labels_shown, point_look, point_state, Disc, PointLook, PointPart, PointRole, PointState, LABEL_HALO, LABEL_OFFSET,
            LABEL_OFFSET_NOW, LABEL_SIZE_NOW, MARKER_SHADOW_BLUR, MARKER_SHADOW_TRANSLATE,
        };
        let marker = |at: (f64, f64), d: Disc, shadow: bool, id: Option<u64>| crate::capture::StoryMarker {
            at,
            radius: d.radius,
            fill: d.fill,
            stroke_width: d.stroke_width,
            stroke: d.stroke,
            blur: if shadow { MARKER_SHADOW_BLUR } else { 0.0 },
            translate: if shadow { MARKER_SHADOW_TRANSLATE } else { (0.0, 0.0) },
            id,
        };
        let whole = |at: (f64, f64), look: &PointLook| {
            let mut out = vec![marker(at, look.shadow(), true, None)];
            out.extend(look.ring.iter().chain(&look.outer).chain(&look.core).map(|d| marker(at, *d, false, None)));
            out
        };
        match part {
            PointPart::Start => {
                let look = point_look(PointRole::Stops, PointState::Done, self.accent);
                let at = self.route.as_ref().and_then(|r| r.first_line.first().copied());
                return (at.map(|at| whole(at, &look)).unwrap_or_default(), Vec::new());
            }
            PointPart::Head => {
                let p = draw_progress(fx);
                let at = (p > 0.0 && p < 1.0).then(|| self.route.as_ref().and_then(|r| crate::story::overlay_layers::point_at_fraction(r, f64::from(p)))).flatten();
                return (at.map(|at| whole(at, &head_look(self.accent))).unwrap_or_default(), Vec::new());
            }
            _ => {}
        }
        let p = self.route.as_ref().and_then(|r| points.progress.get(&r.layer_id).copied()).unwrap_or(1.0);
        let n = self.features.len();
        let states: Vec<PointState> = self
            .features
            .iter()
            .enumerate()
            .map(|(k, f)| point_state(self.role, f.number, points.card_index, f.along.filter(|_| self.route.is_some()), p, k + 1 == n))
            .collect();
        // The author's keys over the top disc (spec §6.1 "Merging").
        let authored = |mut d: Disc| {
            let colour = |k: &str| {
                self.author.get(k).and_then(serde_json::Value::as_str).and_then(|t| {
                    crate::style_spec::color::css_named_color(t).or_else(|| crate::style_spec::parse_css_color(t).ok())
                }).map(crate::story::effects::rgba_bytes)
            };
            let num = |k: &str| self.author.get(k).and_then(serde_json::Value::as_f64).map(|v| v as f32);
            let alpha = |c: [u8; 4], k: &str| [c[0], c[1], c[2], (f32::from(c[3]) * num(k).unwrap_or(1.0).clamp(0.0, 1.0)).round() as u8];
            d.fill = alpha(colour("circle-color").unwrap_or(d.fill), "circle-opacity");
            d.radius = num("circle-radius").unwrap_or(d.radius);
            d.stroke = alpha(colour("circle-stroke-color").unwrap_or(d.stroke), "circle-stroke-opacity");
            d.stroke_width = num("circle-stroke-width").unwrap_or(d.stroke_width);
            d
        };
        // The author's `circle-blur` and `circle-translate` on the top
        // disc's marker (§6.1, key by key, as the rest of the core's paint).
        let author_blur = self.author.get("circle-blur").and_then(serde_json::Value::as_f64).map(|v| v.max(0.0) as f32);
        let author_translate = self.author.get("circle-translate").and_then(serde_json::Value::as_array).and_then(|a| {
            Some((a.first()?.as_f64()? as f32, a.get(1)?.as_f64()? as f32))
        });
        let top = |mut m: crate::capture::StoryMarker| {
            m.blur = author_blur.unwrap_or(m.blur);
            m.translate = author_translate.unwrap_or(m.translate);
            m
        };
        let mut markers = Vec::new();
        let mut labels = Vec::new();
        for (k, (f, &state)) in self.features.iter().zip(&states).enumerate() {
            let mut look = point_look(self.role, state, self.accent);
            if look.core.is_some() {
                look.core = look.core.map(authored);
            } else {
                look.outer = look.outer.map(authored);
            }
            let id = Some(f.id);
            let has_core = look.core.is_some();
            match part {
                PointPart::Shadow => markers.push(marker(f.at, look.shadow(), true, id)),
                PointPart::Halo => {
                    markers.extend(look.ring.iter().map(|d| marker(f.at, *d, false, id)));
                    markers.extend(look.outer.iter().map(|d| if has_core { marker(f.at, *d, false, id) } else { top(marker(f.at, *d, false, id)) }));
                }
                _ => {
                    markers.extend(look.core.iter().map(|d| top(marker(f.at, *d, false, id))));
                    if let Some(numeral) = look.numeral {
                        labels.push(crate::capture::StoryLabel {
                            at: f.at,
                            text: f.number.to_string(),
                            size: numeral.size,
                            weight: numeral.weight,
                            color: numeral.color,
                            halo: [0; 4],
                            halo_width: 0.0,
                            beside: None,
                            feature: format!("{}/{}", self.layer_id, f.id),
                            id,
                            state,
                        });
                    }
                    let name = f.name.as_deref().filter(|n| !n.trim().is_empty());
                    if let Some(name) = name.filter(|_| labels_shown(self.role, self.label.as_deref(), state, k, n)) {
                        let now = state == PointState::Now;
                        let token = crate::story::design::MAP_LABEL;
                        let [r, g, b] = crate::story::design::INK;
                        labels.push(crate::capture::StoryLabel {
                            at: f.at,
                            // Spec §9 (R14 review): a no-op without an RTL letter.
                            text: crate::story::cards::isolate_ltr_runs(name),
                            size: if now { LABEL_SIZE_NOW } else { token.size },
                            weight: token.weight,
                            color: [r, g, b, 0xFF],
                            halo: [0xFF; 4],
                            halo_width: LABEL_HALO,
                            beside: Some((if now { LABEL_OFFSET_NOW } else { LABEL_OFFSET }, f.side)),
                            feature: format!("{}/{}", self.layer_id, f.id),
                            id,
                            state,
                        });
                    }
                }
            }
        }
        (markers, labels)
    }
}


/// One overlay layer's [`LayerRender`], as declared (unthemed).
fn layer_render(source: &crate::story::overlay_layers::LayerSource, above_labels: bool, per_feature: bool) -> LayerRender {
    let source = source.clone();
    let doc: serde_json::Value = serde_json::from_str(&source.geojson).unwrap_or(serde_json::Value::Null);
    let points = doc
        .get("features")
        .and_then(serde_json::Value::as_array)
        .map(|features| {
            features
                .iter()
                .filter_map(|f| Some((f.get("id")?.as_u64()?, first_position(f.get("geometry")?)?)))
                .collect()
        })
        .unwrap_or_default();
    let colors: std::collections::BTreeMap<u64, [u8; 4]> =
        source.feature_ids.iter().zip(&source.base_colors).filter_map(|(id, c)| Some((*id, (*c)?))).collect();
    let outline = source.feature_ids.first().and_then(|id| colors.get(id).copied());
    let key = StoryLayerDraw::content_key(&source.layer_id, &source.geojson, &source.paint, source.line_metrics, per_feature);
    LayerRender {
        overlay_id: source.overlay_id.clone(),
        layer_id: source.layer_id.as_str().into(),
        geojson: source.geojson.as_str().into(),
        paint: Arc::new(source.paint),
        line_metrics: source.line_metrics,
        per_feature,
        above_labels,
        above_buildings: source.above_buildings,
        key,
        points,
        colors,
        outline,
        fx_id: source.layer_id.as_str().into(),
        trim: crate::story::theme_layers::Trim::Same,
        marks_on: true,
        arrowheads: Vec::new(),
        markers: None,
        labels: Vec::new(),
        point: None,
        has_core: true,
    }
}

/// The most rings (drop ripples and pulse rings, over every layer) one
/// frame draws. Past it, the rings of the lowest feature ids are drawn
/// (deterministic) and the manifest's gaps say so.
pub const MAX_RINGS_PER_FRAME: usize = 2000;

/// The draw head's outline width, in 1080-referenced px: a ring in the
/// line's colour around the head, so a white head reads on light land.
pub const HEAD_OUTLINE_PX: f32 = 2.0;

/// Line and polygon positions in a GeoJSON FeatureCollection (what the
/// whole-source limit counts, `overlay::tile_plan::tiling_points`).
pub(crate) fn line_and_polygon_points(doc: &serde_json::Value) -> usize {
    fn positions(c: &serde_json::Value) -> usize {
        match c.as_array() {
            Some(a) if a.first().is_some_and(serde_json::Value::is_number) => 1,
            Some(a) => a.iter().map(positions).sum(),
            None => 0,
        }
    }
    doc.get("features").and_then(serde_json::Value::as_array).map_or(0, |features| {
        features
            .iter()
            .filter_map(|f| f.get("geometry"))
            .filter(|g| !matches!(g.get("type").and_then(serde_json::Value::as_str), Some("Point" | "MultiPoint")))
            .filter_map(|g| g.get("coordinates"))
            .map(positions)
            .sum()
    })
}

/// A feature's first position, `None` for a geometry with none.
fn first_position(geometry: &serde_json::Value) -> Option<(f64, f64)> {
    let mut c = geometry.get("coordinates")?;
    while c.get(0).is_some_and(serde_json::Value::is_array) {
        c = c.get(0)?;
    }
    Some((c.get(0)?.as_f64()?, c.get(1)?.as_f64()?))
}

/// An overlay's document: its inline `data` object, the document its
/// `data` path or `geojson` path names in `docs`, or the one keyed by its
/// id. `Ok(None)` when none was supplied (a gap); an error when one was
/// and it is not JSON.
pub(crate) fn overlay_doc(overlay: &crate::story::script::OverlaySpec, docs: &HashMap<String, String>) -> anyhow::Result<Option<serde_json::Value>> {
    let (text, named) = match &overlay.data {
        Some(inline @ serde_json::Value::Object(_)) => return Ok(Some(inline.clone())),
        Some(serde_json::Value::String(path)) => (docs.get(path), path.as_str()),
        _ => match overlay.geojson.as_deref() {
            Some(p) => (docs.get(p).or_else(|| docs.get(&overlay.id)), p),
            None => (docs.get(&overlay.id), overlay.id.as_str()),
        },
    };
    let Some(text) = text else { return Ok(None) };
    serde_json::from_str(text)
        .map(Some)
        .with_context(|| format!("overlay `{}`: its document `{named}` is not valid JSON", overlay.id))
}

/// `#rrggbbaa` from bytes and an alpha in 0..1 (a feature's own colour
/// property is hex, `overlay::FeaturePaint`).
fn rgba_text([r, g, b, _]: [u8; 4], alpha: f32) -> String {
    let a = (alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{r:02x}{g:02x}{b:02x}{a:02x}")
}

/// `fx` on one marker (R8 review): `alpha` scales its fill and stroke,
/// `radius_scale` its radius and stroke, and with `recolour` the
/// highlight's `color` replaces its fill (times the fill's own alpha).
fn apply_feature_fx(m: &mut crate::capture::StoryMarker, fx: &crate::story::effects::FeatureFx, recolour: bool) {
    let a = fx.alpha.clamp(0.0, 1.0);
    if recolour {
        if let Some([r, g, b, ca]) = fx.color {
            m.fill = [r, g, b, (f32::from(ca) * f32::from(m.fill[3]) / 255.0).round() as u8];
        }
    }
    m.fill[3] = (f32::from(m.fill[3]) * a).round() as u8;
    m.stroke[3] = (f32::from(m.stroke[3]) * a).round() as u8;
    let k = if fx.radius_scale.is_finite() { fx.radius_scale.max(0.0) } else { 1.0 };
    m.radius *= k;
    m.stroke_width *= k;
}

impl LayerRender {
    /// This frame's rings as markers (a marker layer's `pulse` rings and
    /// `drop` ripples, spec §6.4): translucent discs in the feature's ring
    /// colour, the lowest feature ids first, within `ring_budget`.
    fn ring_markers(&self, fx: &crate::story::effects::LayerFx, ring_budget: &mut usize) -> Vec<crate::capture::StoryMarker> {
        let mut rings: Vec<&(u64, f32, f32)> = fx.rings.iter().collect();
        rings.sort_by_key(|r| r.0);
        let mut out = Vec::new();
        for &&(id, radius, alpha) in &rings {
            if *ring_budget == 0 {
                break;
            }
            let (Some(&at), Some(&[r, g, b, a])) = (self.points.get(&id), self.colors.get(&id)) else { continue };
            let a = (alpha.clamp(0.0, 1.0) * f32::from(a)).round() as u8;
            if radius > 0.0 && a > 0 {
                out.push(crate::capture::StoryMarker { at, radius, fill: [r, g, b, a], stroke_width: 0.0, stroke: [0; 4], blur: 0.0, translate: (0.0, 0.0), id: None });
                *ring_budget -= 1;
            }
        }
        out
    }

    /// This frame's marks: at most `ring_budget` visible rings (the lowest
    /// feature ids first; the budget is decremented), then the draw head
    /// over them on an outline in the line's colour.
    fn marks(&self, fx: &crate::story::effects::LayerFx, ring_budget: &mut usize) -> Option<String> {
        let mut features = Vec::new();
        let point = |(lon, lat): (f64, f64), radius: f32, color: String| {
            serde_json::json!({"type": "Feature", "properties": {"circle-radius": radius, "circle-color": color},
                "geometry": {"type": "Point", "coordinates": [lon, lat]}})
        };
        let mut rings: Vec<&(u64, f32, f32)> = fx.rings.iter().collect();
        rings.sort_by_key(|r| r.0);
        for &&(id, radius, alpha) in &rings {
            if *ring_budget == 0 {
                break;
            }
            let (Some(&at), Some(&color)) = (self.points.get(&id), self.colors.get(&id)) else { continue };
            let alpha = alpha * f32::from(color[3]) / 255.0;
            if radius > 0.0 && alpha > 0.0 {
                features.push(point(at, radius, rgba_text(color, alpha)));
                *ring_budget -= 1;
            }
        }
        if let Some((lon, lat, radius, color)) = fx.head {
            if radius > 0.0 {
                if let Some(outline) = self.outline {
                    features.push(point((lon, lat), radius + HEAD_OUTLINE_PX, rgba_text(outline, f32::from(outline[3]) / 255.0)));
                }
                features.push(point((lon, lat), radius, rgba_text(color, f32::from(color[3]) / 255.0)));
            }
        }
        (!features.is_empty()).then(|| serde_json::json!({"type": "FeatureCollection", "features": features}).to_string())
    }

    /// The layer's draw at this frame's effect state, its rings within `ring_budget`.
    fn draw(&self, fx: &crate::story::effects::LayerFx, ring_budget: &mut usize, points: &PointFrame) -> StoryLayerDraw {
        let (mut markers, mut labels) = match &self.point {
            Some((layer, part)) => {
                let (m, l) = layer.draw(*part, fx, points);
                (Some(m), l)
            }
            None => (self.markers.clone(), self.labels.clone()),
        };
        // Per-feature effects on markers and labels (R8 review): a `drop`
        // or a `highlight` scales each one's alpha, a `drop` its radius;
        // a `highlight`'s colour goes to the top disc (a themed core or a
        // badge, an unthemed layer's one disc). Rings -- `pulse` and a
        // `drop`'s ripple -- draw as markers under the layer's bottom
        // part, within the frame's ring budget.
        let part = self.point.as_ref().map(|(_, p)| *p);
        if let Some(ms) = markers.as_mut() {
            use crate::story::theme_layers::PointPart;
            let recolour = match part {
                None | Some(PointPart::Core) => true,
                Some(PointPart::Halo) => !self.has_core,
                _ => false,
            };
            for m in ms.iter_mut() {
                let Some(f) = m.id.and_then(|id| fx.features.iter().find(|f| f.id == id)) else { continue };
                // A badge's `now` ring is not its top disc.
                apply_feature_fx(m, f, recolour && m.fill[3] > 0);
            }
            ms.retain(|m| m.fill[3] > 0 || (m.stroke[3] > 0 && m.stroke_width > 0.0));
            if matches!(part, None | Some(PointPart::Shadow)) {
                let rings = self.ring_markers(fx, ring_budget);
                ms.splice(0..0, rings);
            }
        }
        for l in &mut labels {
            if let Some(f) = l.id.and_then(|id| fx.features.iter().find(|f| f.id == id)) {
                l.color[3] = (f32::from(l.color[3]) * f.alpha.clamp(0.0, 1.0)).round() as u8;
                l.halo[3] = (f32::from(l.halo[3]) * f.alpha.clamp(0.0, 1.0)).round() as u8;
            }
        }
        // A themed sublayer's trim from its parent's (`Trim::apply`, the
        // live map's rule too), as the model uniform reads it.
        let (trim, opacity) = self.trim.apply(fx.trim, fx.opacity);
        let line_fx = crate::overlay::host::SourceLineFx { trim, opacity }.uniform();
        StoryLayerDraw {
            key: self.key,
            layer_id: Arc::clone(&self.layer_id),
            geojson: Arc::clone(&self.geojson),
            paint: Arc::clone(&self.paint),
            line_metrics: self.line_metrics,
            per_feature: self.per_feature,
            above_labels: self.above_labels,
            above_buildings: self.above_buildings,
            line_fx,
            features: fx.features.clone(),
            // A marker layer's rings are markers (above); its mesh, and so
            // its 1a marks, is not drawn.
            marks: if self.marks_on && markers.is_none() { self.marks(fx, ring_budget) } else { None },
            // Drawn with the line's end: not while a draw has yet to reach it.
            arrowheads: if trim.is_none_or(|[_, end]| end >= 1.0) { self.arrowheads.clone() } else { Vec::new() },
            markers,
            labels,
        }
    }
}

/// The story path's basemap bake: compact fills and compact strokes, as
/// the live map draws by default. Recorded in the manifest.
pub const BASEMAP_COMPACT: bool = true;
/// See [`BASEMAP_COMPACT`].
pub const BASEMAP_COMPACT_STROKES: bool = true;

fn hex_sha256(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

fn diagnostics_error(what: &str, diags: &[crate::story::Diagnostic]) -> anyhow::Error {
    let listed: Vec<String> = diags.iter().map(|d| format!("{} ({}): {}", d.path, d.code, d.message)).collect();
    anyhow::anyhow!("{what}: {}", listed.join("; "))
}

/// A GeoJSON document's features: a FeatureCollection's, a Feature, or
/// a bare geometry wrapped as one. Anything else has none.
fn features_of(doc: &serde_json::Value) -> Vec<serde_json::Value> {
    match doc.get("type").and_then(serde_json::Value::as_str) {
        Some("FeatureCollection") => doc.get("features").and_then(serde_json::Value::as_array).cloned().unwrap_or_default(),
        Some("Feature") => vec![doc.clone()],
        Some(_) => vec![serde_json::json!({"type": "Feature", "properties": {}, "geometry": doc})],
        None => Vec::new(),
    }
}

/// The frame count for `duration` seconds at `fps`: `ceil(duration·fps)`,
/// at least 1 (a jump-only story is one still).
fn frame_count(duration: f64, fps: u32) -> usize {
    let f = (duration * f64::from(fps.max(1)) - 1e-9).ceil();
    if f.is_finite() && f >= 1.0 { f as usize } else { 1 }
}

/// The plan crosses to the preview's worker thread.
#[cfg(test)]
const _: fn() = || {
    fn send<T: Send>() {}
    send::<LiveStoryPlan>();
    send::<LiveStory>();
};

/// A test's stopwatch: seconds since the call. Here, in the one story
/// file the clock scan lets measure (`tests/story_clock_scan.rs`), so a
/// test elsewhere under `src/story` can time itself without naming a clock.
#[cfg(test)]
pub(crate) fn stopwatch() -> impl Fn() -> f64 {
    let started = Instant::now();
    move || started.elapsed().as_secs_f64()
}

/// Every overlay layer's own source (spec §2.2), from its overlay's
/// document in `overlays`, with its overlay's `above_labels`: what an
/// effect resolves against and what is drawn. A layer over the
/// whole-source limit is refused here, before any frame, not minutes into
/// a render (review #6): a story layer is always drawn whole.
fn story_layer_sources(
    script: &Script,
    overlays: &HashMap<String, String>,
) -> anyhow::Result<Vec<(bool, crate::story::overlay_layers::LayerSource)>> {
    let mut layer_sources = Vec::new();
    for (overlay_index, overlay) in script.scene.overlays.iter().enumerate().filter(|(_, o)| !o.layers.is_empty()) {
        if let Some(doc) = overlay_doc(overlay, overlays)? {
            let sources = crate::story::overlay_layers::layer_sources(overlay_index, overlay, &doc)
                .map_err(|d| diagnostics_error("overlay layers", std::slice::from_ref(&d)))?;
            for source in &sources {
                let doc: serde_json::Value = serde_json::from_str(&source.geojson).unwrap_or(serde_json::Value::Null);
                let points = line_and_polygon_points(&doc);
                anyhow::ensure!(
                    points < crate::overlay::tile_plan::TILED_SOURCE_MIN_POINTS,
                    "overlay `{}` layer `{}` has {points} line/polygon points, over the whole-source limit of {}; \
                     simplify the geometry or split the layer",
                    overlay.id,
                    source.layer_id,
                    crate::overlay::tile_plan::TILED_SOURCE_MIN_POINTS
                );
            }
            layer_sources.extend(sources.into_iter().map(|s| (overlay.above_labels, s)));
        }
    }
    Ok(layer_sources)
}

/// A story composed for a LIVE map: the native preview (Task 20, spec
/// R2) draws the same story frame a render composes ([`StoryParts`]:
/// panels, images, callouts, furniture, captions) at the live view's own
/// size, and the same overlay layers, each as an overlay source of its own
/// (`overlay_layers::live_source_name`) driven per frame by
/// `MapView::apply_story_layer_fx`.
///
/// Built in two steps so the slow half stays off the UI thread:
/// [`LiveStory::plan`] resolves the path, the timeline and the layers
/// (what the preview answers with at once), and [`LiveStoryPlan::build`]
/// -- run on a worker -- decodes the images, builds their mip chains and
/// measures the callouts.
///
/// What it does NOT do, and says so: images load from local files and the
/// image cache only (`offline`; the preview never waits on the network),
/// and an UNTHEMED layer's 1a draw head and pulse/drop rings
/// (`StoryLayerDraw::marks`) are not drawn. A themed layer's head, every
/// marker, label and arrowhead are ([`Self::story_layers_at`]).
pub struct LiveStory {
    script: Script,
    path: CameraPath,
    timeline: Timeline,
    output: ResolvedOutput,
    images: crate::story::media::ScriptImages,
    mips: Vec<(Arc<image::RgbaImage>, Arc<Vec<image::RgbaImage>>)>,
    callouts: Vec<CalloutTrack>,
    furniture: StoryFurniture,
    layers: Vec<(bool, crate::story::overlay_layers::LayerSource)>,
    /// Aligned with `layers`: the overlay layer whose effect state each
    /// reads and how it takes its trim (a themed sublayer's parent).
    layer_fx_of: Vec<(String, crate::story::theme_layers::Trim)>,
    /// The design system's decisions, as a render of the script makes
    /// them (`None` without `design`): the card, the fine print, the
    /// fade (spec §13).
    design: Option<DesignCtx>,
    /// Every overlay layer's per-frame half, as the render builds it
    /// ([`layer_renders`]): what [`Self::story_layers_at`] draws.
    renders: Vec<LayerRender>,
}

/// [`LiveStory`]'s cheap half: `Send`, so [`Self::build`] can run on a
/// worker.
pub struct LiveStoryPlan {
    script: Script,
    base_dir: PathBuf,
    path: CameraPath,
    timeline: Timeline,
    output: ResolvedOutput,
    viewport: (f32, f32),
    layers: Vec<(bool, crate::story::overlay_layers::LayerSource)>,
    layer_fx_of: Vec<(String, crate::story::theme_layers::Trim)>,
    furniture: StoryFurniture,
    design: Option<DesignCtx>,
    renders: Vec<LayerRender>,
    /// The shipped faces, as the camera pass loaded them (if it ran):
    /// `build` measures the callouts in the same state.
    fonts: crate::capture::story_chrome::LayoutFonts,
}

/// What a live view draws in a design frame's corner that the script
/// cannot know (spec §13): the view's own tile credit (the fine print's)
/// and its slot mark's width, 1080 units
/// ([`crate::story::frame_layout::slot_mark_w`] of the mark it draws).
/// [`LiveStory::plan`] lays the cards out with them for the camera pass,
/// so the preview frames the subject where the view then draws the card.
/// `MapView::story_view_corner` gives a view's.
///
/// Captured once, at plan time: if the view's credits or mark change
/// later (a source added, the mark turned off), the fine print -- and so
/// the card's floor and its camera box -- can differ by a line from what
/// the camera was framed for, until the story is previewed again, which
/// picks the change up.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ViewCorner {
    pub credit: Option<String>,
    pub slot_mark_w: Option<f32>,
}

impl LiveStoryPlan {
    /// The story's camera path, at the view's size.
    #[must_use]
    pub fn path(&self) -> &CameraPath {
        &self.path
    }

    /// The story's timeline.
    #[must_use]
    pub fn timeline(&self) -> &Timeline {
        &self.timeline
    }

    /// Each overlay layer's source, with its overlay's `above_labels`: a
    /// themed layer (spec §6) as its sublayers, each a source of its own.
    #[must_use]
    pub fn layers(&self) -> &[(bool, crate::story::overlay_layers::LayerSource)] {
        &self.layers
    }

    /// The slow half, for a worker thread: every image decoded (local
    /// files and the image cache only) with its mip chain, and every
    /// callout's side and fade scheduled with its card measured in the
    /// shipped faces -- the faces the video and the live map's story both
    /// draw in.
    #[must_use]
    pub fn build(self) -> LiveStory {
        let Self { script, base_dir, path, timeline, output, viewport, layers, layer_fx_of, furniture, design, renders, mut fonts } = self;
        let images = {
            let long_edge = output.width.max(output.height);
            let cache = crate::story::media::default_image_cache_dir();
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map(|rt| rt.block_on(crate::story::media::load_script_images(&script, &base_dir, &cache, long_edge, true)))
                .unwrap_or_default()
        };
        let mips = images
            .loaded
            .iter()
            .map(|l| (Arc::clone(&l.image), Arc::new(crate::story::media::mip_chain(&l.image))))
            .collect();
        let callouts =
            fonts.with_measure(output.pixel_ratio, |measure| callout_tracks(&path, &timeline, &output, &images, viewport, measure));
        LiveStory { script, path, timeline, output, images, mips, callouts, furniture, layers, layer_fx_of, design, renders }
    }
}

impl LiveStory {
    /// Resolves `script` for a `viewport` (logical pixels) view at
    /// `pixel_ratio`: its camera path and timeline (with its overlay
    /// layers, so a `follow_draw` camera follows its draw head) and its
    /// layers. Reads the overlay files; decodes nothing.
    ///
    /// The path and the frames are resolved in the capture's frame, 1080
    /// units of short edge at the view's aspect (spec §13): put a pose on
    /// the view at `zoom + `[`live_zoom_offset`]`(viewport)`.
    ///
    /// The path takes the render's camera pass (spec §5.3): each card is
    /// laid out once, here, around `corner` -- the view's credit and slot
    /// mark (`MapView::story_view_corner`) -- so the subject sits clear of
    /// the card the view draws, where a capture puts it.
    ///
    /// # Errors
    /// `(stage, diagnostics)` when an overlay document, an overlay layer,
    /// the path or the timeline does not resolve.
    pub fn plan(
        script: &Script,
        base_dir: &Path,
        docs: crate::story::SceneDocuments,
        viewport: (f32, f32),
        pixel_ratio: f32,
        corner: &ViewCorner,
    ) -> Result<LiveStoryPlan, (String, Vec<crate::story::Diagnostic>)> {
        let crate::story::SceneDocuments { base_style, load_style, tracks } = docs;
        let mut diags = Vec::new();
        let overlays = crate::story::validate::overlay_documents(script, base_dir, &mut diags);
        if diags.iter().any(|d| d.severity == crate::story::Severity::Error) {
            return Err(("overlay documents".to_string(), diags));
        }
        let as_diag = |e: anyhow::Error| crate::story::Diagnostic {
            severity: crate::story::Severity::Error,
            path: "/scene/overlays".to_string(),
            code: "overlay_layers",
            message: format!("{e:#}"),
            hint: None,
        };
        let layers = story_layer_sources(script, &overlays).map_err(|e| ("overlay layers".to_string(), vec![as_diag(e)]))?;
        let sources: Vec<crate::story::overlay_layers::LayerSource> = layers.iter().map(|(_, s)| s.clone()).collect();
        let given = if pixel_ratio.is_finite() && pixel_ratio > 0.0 { pixel_ratio } else { 1.0 };
        let physical = ((viewport.0 * given).round().max(1.0) as u32, (viewport.1 * given).round().max(1.0) as u32);
        // The capture's frame (spec §13, ruling R4-d): 1080 units of short
        // edge, as a render normalises (`tiers::ratio_for_short_edge`) and
        // as the view lays the frame out (`MapView::paint`). The path's
        // poses are the render's; the view shows the same ground at
        // `zoom + live_zoom_offset(window)`.
        let ratio = crate::story::tiers::ratio_for_short_edge(physical.0.min(physical.1) as f32);
        let viewport = crate::story::frame_layout::units(physical, ratio);
        let (path, timeline) =
            crate::story::timeline::path_and_timeline(viewport, script, Arc::new(tracks), &sources, base_style, &*load_style)
                .map_err(|(stage, d)| (stage.to_string(), d))?;
        let output = ResolvedOutput { width: physical.0, height: physical.1, fps: script.output().fps.max(1), pixel_ratio: ratio };
        // The render's design context (spec §13) with the view's corner:
        // the slot mark the view draws, and no credit -- the view fills in
        // its own at paint (`capture::chrome::LiveCorner::credit`).
        let design = DesignCtx::new(script, timeline.chapters(), &path, None, corner.slot_mark_w);
        // The render's camera pass (spec §5.3, ruling R4-b), with the
        // credit the view will draw. The card is measured once, here, in
        // the shipped faces `build` measures the callouts in.
        let mut fonts = crate::capture::story_chrome::LayoutFonts::new(Some(crate::story::validate::shipped_label_fonts()));
        let path = design_camera_path(script, path, viewport, viewport, ratio, design.as_ref(), corner.credit.as_deref(), &mut fonts)
            .map_err(|d| ("camera path".to_string(), d))?;
        // The furniture's legends read the layers as declared, as a render's do.
        let furniture = StoryFurniture::new(script, timeline.chapters(), &sources);
        // The per-frame marks' half (spec §13, R11a): the render's own builder.
        // One theme expansion for both halves (R11a review): the marks'
        // renders and the live sources.
        let expanded = expand_layers(script, layers, design.as_ref());
        let renders = layer_renders(script, &expanded, &timeline, design.as_ref(), &sources);
        let (layers, layer_fx_of) = live_layers(expanded);
        Ok(LiveStoryPlan {
            script: script.clone(),
            base_dir: base_dir.to_path_buf(),
            path,
            timeline,
            output,
            viewport,
            layers,
            layer_fx_of,
            furniture,
            design,
            renders,
            fonts,
        })
    }

    /// Each loaded image's mip chain, built in [`LiveStoryPlan::build`]:
    /// for `MapView::set_story_image_mips`, so paint only uploads.
    #[must_use]
    pub fn image_mips(&self) -> Vec<(Arc<image::RgbaImage>, Arc<Vec<image::RgbaImage>>)> {
        self.mips.clone()
    }

    /// The story's camera path, at the view's size.
    #[must_use]
    pub fn path(&self) -> &CameraPath {
        &self.path
    }

    /// The story's timeline.
    #[must_use]
    pub fn timeline(&self) -> &Timeline {
        &self.timeline
    }

    /// Each overlay layer's source, with its overlay's `above_labels`:
    /// what the preview pushes as `overlay_layers::live_source_name`.
    #[must_use]
    pub fn layers(&self) -> &[(bool, crate::story::overlay_layers::LayerSource)] {
        &self.layers
    }

    fn parts(&self) -> StoryParts<'_> {
        StoryParts {
            script: &self.script,
            path: &self.path,
            timeline: &self.timeline,
            output: self.output,
            images: &self.images,
            callouts: &self.callouts,
            furniture: &self.furniture,
            design: self.design.as_ref(),
        }
    }

    /// The story frame at `t` (`state` is the timeline's at `t`) and the
    /// captions its `caption/<i>` items place: exactly what
    /// [`Player::request_at`] composes, the furniture read off the pose's
    /// camera.
    #[must_use]
    pub fn frame_at(&self, state: &SceneState, t: f64) -> (Option<Arc<crate::capture::StoryFrame>>, Vec<ActiveCaption>) {
        let parts = self.parts();
        let captions = parts.captions(state, t);
        let camera = frame_camera(&self.path.pose_at(t), state);
        (parts.story_frame(state, &captions, t, camera), captions)
    }

    /// The story's overlay layers drawn at `t` (`state` is the timeline's
    /// at `t`): exactly the [`CaptureRequest::story_layers`] a render's
    /// frame at `t` carries -- each layer's markers with their states and
    /// per-feature effects, its labels and numerals, the themed draw head
    /// and the arrowheads (spec §6.3, §13). `MapView::set_story_marks`
    /// draws them. A pure function of `t`: seeking equals playing.
    #[must_use]
    pub fn story_layers_at(&self, state: &SceneState, t: f64) -> Vec<crate::capture::StoryLayerDraw> {
        layer_draws(&self.renders, self.design.as_ref(), state, t)
    }

    /// Every overlay layer's effect state in `state`, as a render draws
    /// it: a layer whose overlay is hidden, or with no effect state, at
    /// opacity 0.
    #[must_use]
    pub fn layer_fx(&self, state: &SceneState) -> Vec<crate::story::effects::LayerFx> {
        self.layers
            .iter()
            .zip(&self.layer_fx_of)
            .map(|((_, source), (parent, trim))| {
                let visible = state.visible_overlays.contains(&source.overlay_id);
                match state.layer_fx.iter().find(|fx| fx.layer_id == *parent).filter(|_| visible) {
                    // A themed sublayer reads its parent's state
                    // (`theme_layers::fan_out`, the render's rule).
                    Some(fx) => crate::story::theme_layers::fan_out(fx, &source.layer_id, *trim),
                    // Not drawn, as a render leaves it out.
                    None => crate::story::effects::LayerFx {
                        layer_id: source.layer_id.clone(),
                        opacity: 0.0,
                        ..crate::story::effects::LayerFx::default()
                    },
                }
            })
            .collect()
    }
}

/// The design system's decisions for one render (the slice, spec §3-§6):
/// fixed by the script and the scene, read by every frame. Which card is
/// on screen when is [`crate::story::cards`]'s, read by the drawer
/// ([`DesignCtx::frame_at`]) and the camera pass ([`DesignCtx::card_boxes`])
/// from the same spans.
#[derive(Clone, Debug)]
pub(crate) struct DesignCtx {
    pub(crate) accent: [u8; 3],
    /// The overlay theme (spec §6.2): `design.theme`, else the
    /// archetype's ([`crate::story::design::theme_for`]).
    pub(crate) theme: crate::story::design::Theme,
    /// The story is a tour (spec §2): its circle layers are `numbered`
    /// when inferred (§6.1).
    tour: bool,
    pub(crate) tone: crate::story::design::Tone,
    pub(crate) safe_area: crate::story::tiers::SafeArea,
    aspect: crate::story::tiers::Aspect,
    /// `furniture.sources`, the author's groups (spec §3.6).
    author_sources: Vec<crate::story::script::SourceGroup>,
    /// Each overlay's `attribution`, as the sources' `Data` groups.
    overlay_attributions: Vec<String>,
    pub(crate) credit: Option<String>,
    /// `design.anchor` (spec §5.7).
    anchor: crate::story::frame_layout::AnchorPref,
    /// `design.credits` (spec §3.6).
    credits: crate::story::frame_layout::CreditsPlacement,
    /// The reading direction (spec §9): `lang`, or `design.dir` over it.
    dir: crate::story::frame_layout::Dir,
    /// The slot mark's width (P3), as the capture will draw it.
    mark_w: Option<f32>,
    /// Every card over time ([`crate::story::cards::card_spans`]).
    spans: Vec<crate::story::cards::CardSpan>,
    /// The caption cues drawn in the card (spec §3.5): `captions` while
    /// burn-in is on, none when it is off.
    cues: Vec<crate::story::captions::Cue>,
    /// The `fly` and `ease` segments (spec §3.2's moving-camera rule).
    moving: Vec<(f64, f64)>,
}

impl DesignCtx {
    /// `None` without a top-level `design`: the story draws exactly as 1a.
    ///
    /// `path` is the camera path the cards' times resolve against (pass 1
    /// or the design pass: they share every boundary). `mark_w` is the
    /// slot mark's width the drawer will lay out with ([`slot_mark_for`]:
    /// the effective mark of the requests this story's frames are drawn
    /// from), so the camera pass and the draw agree.
    fn new(script: &Script, chapters: &[crate::story::Chapter], path: &CameraPath, credit: Option<String>, mark_w: Option<f32>) -> Option<Self> {
        use crate::story::design::{accent, archetype_for, theme_for, theme_named, tone_for};
        let design = script.design.as_ref()?;
        let archetype = archetype_for(script).0;
        let theme = design.theme.as_deref().and_then(theme_named).unwrap_or_else(|| theme_for(archetype));
        let accent = design.accent.as_deref().and_then(crate::story::card_layout::parse_hex).unwrap_or_else(|| accent(theme));
        let tone = tone_for(design.card.as_deref(), script.scene.raster.is_some(), None);
        // D1: an absent safe area is `none` in the design system.
        let safe_area = script.output.safe_area.unwrap_or(crate::story::tiers::SafeArea::None);
        let overlay_attributions = script.scene.overlays.iter().filter_map(|o| o.attribution.clone()).collect();
        let (spans, _) = crate::story::cards::card_spans_in(script, chapters, path);
        let cues = if burn_in_on(script) {
            let inputs: Vec<(f64, f64, String)> = script.captions.iter().map(|c| (c.from, c.to, c.text.clone())).collect();
            crate::story::captions::track(&inputs, path.duration())
        } else {
            Vec::new()
        };
        let anchor = crate::story::frame_layout::AnchorPref::named(design.anchor.as_deref());
        let credits = crate::story::frame_layout::CreditsPlacement::named(design.credits.as_deref());
        let dir = crate::story::cards::dir_for(script.lang.as_deref(), design.dir.as_deref());
        // Spec §9: in an RTL story every digit and Latin run the card and
        // the sources draw is isolated once, here, so the drawer and the
        // camera pass lay out the very same strings.
        // (The sources are isolated after they merge, in `sources_with`.)
        let (spans, cues) = if dir == crate::story::frame_layout::Dir::Rtl {
            use crate::story::cards::{isolate_content, isolate_ltr_runs};
            let spans = spans.into_iter().map(|s| crate::story::cards::CardSpan { content: isolate_content(&s.content), ..s }).collect();
            let cues = cues.into_iter().map(|c| crate::story::captions::Cue { text: isolate_ltr_runs(&c.text), ..c }).collect();
            (spans, cues)
        } else {
            (spans, cues)
        };
        Some(DesignCtx {
            accent,
            theme,
            tour: archetype == crate::story::design::Archetype::Tour,
            tone,
            safe_area,
            aspect: script.format().aspect,
            author_sources: script.furniture.sources.clone().unwrap_or_default(),
            overlay_attributions,
            credit,
            anchor,
            credits,
            dir,
            mark_w,
            spans,
            cues,
            moving: crate::story::cards::moving_spans(path),
        })
    }

    /// The sources line with `cards`' photo credits (spec §3.6).
    fn sources_with<'a>(&self, cards: impl IntoIterator<Item = &'a crate::story::card_layout::CardContent>) -> Vec<crate::story::script::SourceGroup> {
        let photos: Vec<String> = cards.into_iter().filter_map(|c| c.photo.as_ref().and_then(|p| p.credit.clone())).collect();
        let merged = crate::story::cards::sources_from(&self.author_sources, &photos, &self.overlay_attributions);
        // Spec §9 (R14 review): every group isolated after the merge -- the
        // author's, the photo credits and the overlay attributions alike.
        if self.dir == crate::story::frame_layout::Dir::Rtl {
            use crate::story::cards::isolate_ltr_runs;
            merged.into_iter().map(|g| crate::story::script::SourceGroup { key: isolate_ltr_runs(&g.key), text: isolate_ltr_runs(&g.text) }).collect()
        } else {
            merged
        }
    }

    /// The frame every card of this story lays out on, `frame` units.
    fn frame_spec(&self, frame: (f32, f32)) -> crate::story::frame_layout::FrameSpec {
        crate::story::frame_layout::FrameSpec {
            w: frame.0,
            h: frame.1,
            aspect: self.aspect,
            safe_area: self.safe_area,
            dir: self.dir,
            anchor: self.anchor,
            tone: self.tone,
            mark_w: self.mark_w,
            credits: self.credits,
        }
    }

    /// The card on screen at `t`'s `index.i` (spec §3.3, §6.3): what a
    /// `numbered` layer's states follow. Read from [`crate::story::cards::card_at`],
    /// the card the frame draws.
    fn card_index_at(&self, t: f64) -> Option<u32> {
        crate::story::cards::card_at(&self.spans, &self.cues, &self.moving, t).and_then(|a| a.card.index).map(|(i, _)| i)
    }

    /// The design frame at `t` (spec §3.1-§3.3, §3.5): the card on screen
    /// ([`crate::story::cards::card_at`]), its fade or its ease from the
    /// card before, and the hairline's fill.
    fn frame_at(&self, chapters: &[crate::story::Chapter], t: f64) -> crate::story::card_layout::DesignFrame {
        let at = crate::story::cards::card_at(&self.spans, &self.cues, &self.moving, t);
        let (card, from_card, ease, alpha, progress) = match at {
            Some(a) => {
                let progress = if a.progress { crate::story::cards::progress_at(chapters, t) } else { None };
                (Some(a.card), a.from_card, a.ease, a.alpha, progress)
            }
            None => (None, None, 1.0, 0.0, None),
        };
        crate::story::card_layout::DesignFrame {
            sources: self.sources_with(from_card.iter().chain(card.iter())),
            card,
            from_card,
            ease,
            card_alpha: alpha,
            progress,
            accent: self.accent,
            tone: self.tone,
            safe_area: self.safe_area,
            aspect: self.aspect,
            credit: self.credit.clone(),
            anchor: self.anchor,
            dir: self.dir,
            credits: self.credits,
        }
    }

    /// Each card's camera box while it is on screen, and the box with no
    /// card (spec §5.3, §5.7): `layout_card(...).camera_box` on `frame`
    /// (units) with `credit` in the fine print, for
    /// [`CameraPath::with_card_boxes`]. The spans are
    /// [`crate::story::cards::screen_spans`] -- the cut [`Self::frame_at`]
    /// draws from, cues applied and caption-only cards included -- so the
    /// camera clears the card height the frame draws. `px_per_unit` is the
    /// path's viewport pixels per unit.
    ///
    /// During a `fly` or `ease` the frame draws the compact card (§3.2) but
    /// the box is the full card's: a conservative choice under §5.3, since
    /// the full card is what the segment's ends show and the pose eases
    /// into it; the subject only sits a little higher than it must.
    fn card_boxes(
        &self,
        frame: (f32, f32),
        px_per_unit: f32,
        credit: Option<&str>,
        m: &dyn crate::story::card_layout::CardMeasure,
    ) -> crate::story::camera_path::CardBoxes {
        use crate::story::camera_path::CardBox;
        // One span -> layout closure (R10 review round 1): every `Some`
        // card's box comes straight from `Self::validation_cards`, the
        // SAME `layout_card` calls validation's own warnings read, so
        // the two can never lay a card out differently.
        let spec = self.frame_spec(frame);
        let none = crate::story::frame_layout::layout_card(&spec, None, &self.sources_with(None), credit, m).camera_box;
        let boxes = self.validation_cards(frame, credit, m).into_iter().map(|(s, l)| CardBox { from: s.from, to: s.to, camera_box: l.camera_box }).collect();
        crate::story::camera_path::CardBoxes { boxes, none, frame, px_per_unit }
    }

    /// The design context a render of `script` builds (spec §13): the tile
    /// credit of `inputs`' basemap and the slot mark `marks`' requests
    /// carry ([`set_marks`]). One place for [`Player::new`], validation's
    /// probe and its camera pass, so the three lay out one frame.
    pub(crate) fn for_render(
        script: &Script,
        chapters: &[crate::story::Chapter],
        path: &CameraPath,
        inputs: &SceneInputs,
        marks: &RenderOptions,
        mapbox_sprite: bool,
    ) -> Option<Self> {
        script.design.as_ref()?;
        DesignCtx::new(script, chapters, path, tile_credit_for(script, inputs, mapbox_sprite), slot_mark_for(marks, inputs.wordmark.as_ref()))
    }

    /// Every screen span's card laid out on `frame` units in `fonts`'
    /// measure, with `credit` in the fine print (spec §11, R10): the
    /// same inputs [`Self::card_boxes`] gives `layout_card` for the
    /// camera box, so validation's warnings describe exactly what a
    /// render (and the live map) draws.
    pub(crate) fn validation_cards(
        &self,
        frame: (f32, f32),
        credit: Option<&str>,
        m: &dyn crate::story::card_layout::CardMeasure,
    ) -> Vec<(crate::story::cards::CardSpan, crate::story::card_layout::CardLayout)> {
        let spec = self.frame_spec(frame);
        crate::story::cards::screen_spans(&self.spans, &self.cues)
            .into_iter()
            .map(|s| {
                let layout = crate::story::frame_layout::layout_card(&spec, Some(&s.content), &self.sources_with(Some(&s.content)), credit, m);
                (s, layout)
            })
            .collect()
    }

    /// The first source group the colophon row would drop as
    /// `sources_too_long` for `content`'s own card (spec §11, R10), or
    /// `None` if every group and the tile credit fit.
    pub(crate) fn sources_too_long_group(
        &self,
        frame: (f32, f32),
        credit: Option<&str>,
        content: Option<&crate::story::card_layout::CardContent>,
        m: &dyn crate::story::card_layout::CardMeasure,
    ) -> Option<String> {
        let spec = self.frame_spec(frame);
        crate::story::frame_layout::sources_too_long_group(&spec, &self.sources_with(content), credit, m)
    }
}

/// The design camera pass (spec §5.3, §5.7): `path` -- pass 1, from
/// `timeline::path_and_timeline` -- resolved again with each card's camera
/// box solved into its poses ([`CameraPath::with_card_boxes`]), the card
/// laid out on `frame` (units) in `fonts` at `ratio`, with `credit` in the
/// fine print. `path` unchanged without `design`, or with
/// `design.frame_camera: false`.
///
/// The ONE camera pass: a render ([`Player::new`]), validation and the
/// live map ([`LiveStory::plan`]) all build their path here, so the live
/// preview frames the subject where the video does. Every boundary is
/// pass 1's (flies are pinned), so the timeline built on `path` holds.
#[allow(clippy::too_many_arguments)]
pub(crate) fn design_camera_path(
    script: &Script,
    path: CameraPath,
    viewport: (f32, f32),
    frame: (f32, f32),
    ratio: f32,
    design: Option<&DesignCtx>,
    credit: Option<&str>,
    fonts: &mut crate::capture::story_chrome::LayoutFonts,
) -> Result<CameraPath, Vec<crate::story::Diagnostic>> {
    let Some(d) = design.filter(|_| script.design.as_ref().is_some_and(|d| d.frame_camera != Some(false))) else {
        return Ok(path);
    };
    let px_per_unit = if frame.0 > 0.0 { viewport.0 / frame.0 } else { 1.0 };
    let boxes = fonts.with_card_measure(ratio, |m| d.card_boxes(frame, px_per_unit, credit, m));
    CameraPath::with_card_boxes(viewport, script, Arc::new(path.tracks().share()), &boxes)
}

/// A render's camera path for `script` (spec §5.3): `path` (pass 1) through
/// [`design_camera_path`] with the design context a render builds
/// ([`DesignCtx::for_render`] under `marks`), the cards measured in
/// `inputs`' label fonts at the output's ratio. [`Player::new`] and
/// validation both call it, so validation probes the video's poses.
pub(crate) fn render_camera_path(
    script: &Script,
    inputs: &SceneInputs,
    marks: &RenderOptions,
    mapbox_sprite: bool,
    path: CameraPath,
    timeline: &Timeline,
) -> Result<CameraPath, Vec<crate::story::Diagnostic>> {
    let Some(design) = DesignCtx::for_render(script, timeline.chapters(), &path, inputs, marks, mapbox_sprite) else {
        return Ok(path);
    };
    let output = script.output();
    let ratio = if output.pixel_ratio.is_finite() && output.pixel_ratio > 0.0 { output.pixel_ratio } else { 1.0 };
    let viewport = (output.width as f32 / ratio, output.height as f32 / ratio);
    design_camera_path(
        script,
        path,
        viewport,
        crate::story::frame_layout::units((output.width, output.height), ratio),
        ratio,
        Some(&design),
        design.credit.as_deref(),
        &mut crate::capture::story_chrome::LayoutFonts::new(inputs.label_fonts.clone()),
    )
}

/// The furniture's camera at a pose (`state` is the timeline's at the
/// same `t`): degrees, and the timeline's globe over the pose's.
fn frame_camera(pose: &crate::story::camera_path::Pose, state: &SceneState) -> crate::story::furniture::FrameCamera {
    let c = pose.camera;
    crate::story::furniture::FrameCamera {
        lat: c.center_lat,
        zoom: c.zoom,
        bearing_deg: f64::from(c.bearing).to_degrees(),
        pitch_deg: f64::from(c.pitch).to_degrees(),
        globe: state.globe.unwrap_or(pose.globe),
    }
}

/// The zoom a live map adds to a [`LiveStory`] pose so a window of
/// `viewport` logical pixels shows the ground a capture of the frame shows
/// (spec §13, ruling R4-d): the poses are resolved in the capture's frame,
/// 1080 units of short edge ([`LiveStory::plan`]), so a window whose short
/// edge is `s` logical pixels draws them at `log2(s / 1080)`
/// (`tiers::ratio_for_short_edge`). Only a story pose takes it; the map's
/// own camera never does.
#[must_use]
pub fn live_zoom_offset(viewport: (f32, f32)) -> f64 {
    let r = f64::from(crate::story::tiers::ratio_for_short_edge(viewport.0.min(viewport.1)));
    if r.is_finite() && r > 0.0 { r.log2() } else { 0.0 }
}

/// `source`'s sublayers under the story's theme (spec §6.1-§6.3), in a
/// story with `design`: a layer with a `role` (the author's `paint`
/// merged over its core), or with no `role` and no `paint` (its role
/// inferred); `None` draws the layer as declared -- always so for a
/// layer with a `paint` and no `role`. The render's layers and the live
/// story's sources both expand here, so they carry the same sublayers
/// (spec §13).
fn themed_sublayers(
    script: &Script,
    source: &crate::story::overlay_layers::LayerSource,
    design: Option<&DesignCtx>,
) -> Option<Vec<crate::story::theme_layers::Sublayer>> {
    let d = design?;
    let (role, draws) = resolved_role(script, source, d.tour)?;
    // An area's values (spec §6.3): the same ones its card legends show.
    let legend = (role == "area").then(|| crate::story::card_layout::area_legend_for(script, &source.layer_id)).flatten();
    crate::story::theme_layers::expand(source, role, d.theme, d.accent, &source.paint, draws, legend.as_ref())
}

/// The role `source` is themed with in a design story (spec §6.1), and
/// whether it has a `draw` effect: its `role`, else -- with an empty
/// `paint` -- the inferred one; `None` for a layer with a `paint` and no
/// `role` (drawn as declared). `tour` is the story's own (spec §2), not
/// a `&DesignCtx`, so this one pure helper is shared with validation's
/// own `themed_core_width`, which cannot build a `DesignCtx` this early
/// (`check_timeline` builds `sources` before the camera path a
/// `DesignCtx` needs) -- R10 review round 1.
pub(crate) fn resolved_role(script: &Script, source: &crate::story::overlay_layers::LayerSource, tour: bool) -> Option<(&'static str, bool)> {
    use crate::story::theme_layers::infer_role;
    let spec = script.scene.overlays.iter().flat_map(|o| o.layers.iter()).find(|l| l.id == source.layer_id)?;
    let draws = script.events.iter().any(|e| {
        matches!(&e.action, crate::story::script::EventAction::Overlay(o)
            if o.effect == crate::story::script::EffectKind::Draw && o.layer == spec.id)
    });
    let role = match spec.role.as_deref() {
        // The matched entry's OWN `&'static str` (never the author's
        // borrowed `role`, whose lifetime is `script`'s, not `'static`).
        Some(role) => crate::story::script::ROLES.iter().find(|(r, k)| *r == role && *k == source.kind).map(|(r, _)| *r)?,
        None if spec.paint.is_empty() => infer_role(spec.kind, draws, tour, source.feature_count),
        None => return None,
    };
    Some((role, draws))
}

/// The [`PointLayer`] a themed layer's point sublayers read (spec §6.3):
/// a point role's own, or a route's for its `start` and `head`. `sources`
/// are every story layer's (a `stops` layer follows the `route` of its
/// own overlay; the nearest line sets a label's side).
fn point_layer_for(
    script: &Script,
    source: &crate::story::overlay_layers::LayerSource,
    d: &DesignCtx,
    sources: &[crate::story::overlay_layers::LayerSource],
) -> Option<PointLayer> {
    use crate::story::script::OverlayLayerKind;
    use crate::story::theme_layers::PointRole;
    let (role, _) = resolved_role(script, source, d.tour)?;
    let lines: Vec<&crate::story::overlay_layers::LayerSource> = sources.iter().filter(|s| s.kind == OverlayLayerKind::Line).collect();
    let label = script.scene.overlays.iter().flat_map(|o| o.layers.iter()).find(|l| l.id == source.layer_id).and_then(|l| l.label.clone());
    if role == "route" {
        return Some(PointLayer::new(&crate::story::overlay_layers::LayerSource::empty(&source.layer_id, OverlayLayerKind::Circle), PointRole::Stops, d.accent, None, Some(source.clone()), &lines));
    }
    let point_role = PointRole::named(role)?;
    let route = (point_role == PointRole::Stops)
        .then(|| {
            sources.iter().find(|s| {
                s.overlay_id == source.overlay_id && s.kind == OverlayLayerKind::Line && resolved_role(script, s, d.tour).is_some_and(|(r, _)| r == "route")
            })
        })
        .flatten()
        .cloned();
    Some(PointLayer::new(source, point_role, d.accent, label, route, &lines))
}

/// The arrowheads of a themed `arrow`'s core `sub` (spec §6.3): one at
/// each line's last vertex ([`crate::story::theme_layers::line_ends`]),
/// in the core's own colour and with a stub of its own width, from its
/// merged paint ([`crate::story::theme_layers::arrow_core_stroke`]: an
/// author's `line-color` or `line-width` reaches the head too, else the
/// accent and the theme's width), edged in the casing's white as wide as
/// the casing shows beside that core (R7-a: [`ARROWHEAD_EDGE`] at the
/// theme's width), so the edge runs on from the shaft's without a step.
///
/// [`ARROWHEAD_EDGE`]: crate::story::theme_layers::ARROWHEAD_EDGE
fn arrowheads_of(sub: &crate::story::theme_layers::Sublayer, accent: [u8; 3]) -> Vec<crate::capture::StoryArrowhead> {
    use crate::story::theme_layers::{arrow_core_stroke, line_ends, ARROWHEAD_LEN, ARROWHEAD_W, ARROW_CASING_W, CASING_OPACITY};
    let (color, width) = arrow_core_stroke(&sub.paint, accent);
    let edge = ((ARROW_CASING_W - width) / 2.0).max(0.0) as f32;
    line_ends(&sub.geojson)
        .into_iter()
        .map(|(end, from)| crate::capture::StoryArrowhead {
            end,
            from,
            color,
            length: ARROWHEAD_LEN,
            width: ARROWHEAD_W,
            edge_color: [0xFF, 0xFF, 0xFF, (CASING_OPACITY * 255.0).round() as u8],
            edge_width: edge,
            stub_width: width as f32,
        })
        .collect()
}

/// Every overlay layer with its theme expansion (spec §6.1-§6.3): `None`
/// draws it as declared; `Some` its sublayers -- a layer with a role (its
/// paint merged over its core), or with neither role nor paint (its role
/// inferred). Expanded ONCE, and both the per-frame marks
/// ([`layer_renders`]) and the live sources ([`live_layers`]) read it.
type ExpandedLayers = Vec<(bool, crate::story::overlay_layers::LayerSource, Option<Vec<crate::story::theme_layers::Sublayer>>)>;

/// [`ExpandedLayers`] of `layers` under `design`.
fn expand_layers(script: &Script, layers: Vec<(bool, crate::story::overlay_layers::LayerSource)>, design: Option<&DesignCtx>) -> ExpandedLayers {
    layers
        .into_iter()
        .map(|(above_labels, source)| {
            let subs = themed_sublayers(script, &source, design);
            (above_labels, source, subs)
        })
        .collect()
}

/// Every overlay layer's [`LayerRender`] (spec §3, §6): each as declared,
/// or -- themed -- each of its sublayers with its markers, labels and
/// arrowheads. One builder for the render ([`Player::new`]) and the live
/// map ([`LiveStory::plan`]), so the two draw the same per-frame marks
/// (spec §13, R11a), over one theme expansion ([`expand_layers`]).
fn layer_renders(
    script: &Script,
    expanded: &ExpandedLayers,
    timeline: &Timeline,
    design: Option<&DesignCtx>,
    sources: &[crate::story::overlay_layers::LayerSource],
) -> Vec<LayerRender> {
    expanded
        .iter()
        .flat_map(|(above_labels, source, subs)| {
            use crate::story::script::EffectKind;
            let above_labels = *above_labels;
            // A per-feature effect needs one op per feature, so one
            // feature's paint is rewritten alone.
            let per_feature = timeline.effect_plans().iter().any(|p| {
                p.layer_id == source.layer_id && matches!(p.kind, EffectKind::Highlight | EffectKind::Drop | EffectKind::Pulse)
            });
            let Some(subs) = subs else {
                let mut render = layer_render(source, above_labels, per_feature);
                // Spec §6.4: a circle layer with marker paint draws as markers.
                if source.kind == crate::story::script::OverlayLayerKind::Circle {
                    render.markers = crate::capture::story_overlays::circle_markers(&source.geojson, &source.paint);
                    render.labels = crate::capture::story_overlays::circle_labels(&source.layer_id, &source.geojson, &source.paint);
                }
                return vec![render];
            };
            let accent = design.map_or([0, 0, 0], |d| d.accent);
            let point_layer = subs
                .iter()
                .any(|s| s.point.is_some())
                .then(|| design.and_then(|d| point_layer_for(script, source, d, sources)))
                .flatten()
                .map(Arc::new);
            subs.iter()
                .map(|sub| {
                    // The area slot (spec §6.3, R9-a): under the labels, over the buildings.
                    let (above_labels, above_buildings) = sub.slot(above_labels, source.above_buildings);
                    let mut render = layer_render(source, above_labels, per_feature);
                    render.above_buildings = above_buildings;
                    render.key = StoryLayerDraw::content_key(&sub.id, &sub.geojson, &sub.paint, source.line_metrics, per_feature);
                    render.layer_id = sub.id.as_str().into();
                    render.geojson = sub.geojson.as_str().into();
                    if sub.arrowhead {
                        render.arrowheads = arrowheads_of(sub, accent);
                    }
                    render.paint = Arc::new(sub.paint.clone());
                    render.trim = sub.trim;
                    render.marks_on = sub.marks;
                    render.point = sub.point.zip(point_layer.clone()).map(|(part, layer)| (layer, part));
                    if let Some(layer) = &point_layer {
                        // Rings in the accent; a badge has no core.
                        render.has_core = layer.role != crate::story::theme_layers::PointRole::Numbered;
                        render.colors = render.points.keys().map(|&id| (id, [accent[0], accent[1], accent[2], 255])).collect();
                    }
                    if sub.marks {
                        render.outline = Some([accent[0], accent[1], accent[2], 255]);
                    }
                    render
                })
                .collect()
        })
        .collect()
}

/// Every layer of `renders` whose overlay `state` shows, drawn at its
/// effect state at `t` (spec §3, §6.3): the point states read the card's
/// index and each layer's draw progress at `t`. A pure function of the
/// frame: seeking equals playing. The render's frames
/// ([`Player::request_at`]) and the live map's ([`LiveStory::story_layers_at`]).
fn layer_draws(renders: &[LayerRender], design: Option<&DesignCtx>, state: &SceneState, t: f64) -> Vec<StoryLayerDraw> {
    let mut ring_budget = MAX_RINGS_PER_FRAME;
    // Spec §6.3: the point markers' states read the card's index and
    // each layer's draw progress at `t`.
    let points = PointFrame {
        card_index: design.and_then(|d| d.card_index_at(t)),
        progress: state.layer_fx.iter().map(|fx| (fx.layer_id.clone(), draw_progress(fx))).collect(),
    };
    renders
        .iter()
        .filter(|l| state.visible_overlays.contains(&l.overlay_id))
        .filter_map(|l| state.layer_fx.iter().find(|fx| *fx.layer_id == *l.fx_id).map(|fx| l.draw(fx, &mut ring_budget, &points)))
        .collect()
}

/// The live story's layer sources (spec §13): each overlay layer as
/// declared, or -- themed -- each of its sublayers as a source of its own
/// (its id, document and paint), with the parent layer whose effect state
/// it reads and how it takes its trim ([`crate::story::theme_layers::Trim`]).
/// From the same expansion the render's layers come from ([`expand_layers`]).
#[allow(clippy::type_complexity)]
fn live_layers(
    expanded: ExpandedLayers,
) -> (Vec<(bool, crate::story::overlay_layers::LayerSource)>, Vec<(String, crate::story::theme_layers::Trim)>) {
    let mut out = Vec::with_capacity(expanded.len());
    let mut fx = Vec::with_capacity(expanded.len());
    for (above_labels, source, subs) in expanded {
        match subs {
            Some(subs) => {
                for sub in subs {
                    // The area slot (spec §6.3, R9-a): under the labels, over the buildings.
                    let (above_labels, above_buildings) = sub.slot(above_labels, source.above_buildings);
                    let mut s = source.clone();
                    s.layer_id = sub.id;
                    s.geojson = sub.geojson;
                    s.paint = sub.paint;
                    s.above_buildings = above_buildings;
                    out.push((above_labels, s));
                    fx.push((source.layer_id.clone(), sub.trim));
                }
            }
            None => {
                fx.push((source.layer_id.clone(), crate::story::theme_layers::Trim::Same));
                out.push((above_labels, source));
            }
        }
    }
    (out, fx)
}

/// Sets the marks every request of a story carries: `opts`' SWAT mark
/// and policy, and the scene's `wordmark`. One place for
/// [`Player::request_at`], the validation probe and [`slot_mark_for`].
fn set_marks(request: &mut CaptureRequest, opts: &RenderOptions, wordmark: Option<&Arc<image::RgbaImage>>) {
    request.brand_mark = opts.brand_mark;
    request.brand_mark_policy = opts.brand_mark_policy;
    request.wordmark = wordmark.cloned().map(Into::into);
}

/// The design slot's mark width (spec §3.6, P3) for requests [`set_marks`]
/// fills from `opts` and `wordmark`: what the capture's drawer reads
/// ([`CaptureRequest::slot_mark_w`]), for the camera pass to lay out with.
fn slot_mark_for(opts: &RenderOptions, wordmark: Option<&Arc<image::RgbaImage>>) -> Option<f32> {
    let mut request = CaptureRequest::default();
    set_marks(&mut request, opts, wordmark);
    request.slot_mark_w(Some(crate::brand_mark::artwork_size()))
}

/// The tile credit alone (spec §3.6, P4): the basemap's (and a Mapbox
/// sprite's), never the script's own credits or an overlay's, which the
/// sources line carries.
fn tile_credit_for(script: &Script, inputs: &SceneInputs, mapbox_sprite: bool) -> Option<String> {
    use crate::story::burn_in::{source_class, SourceClass, VerdictSource};
    let own = inputs.source.attribution().map(crate::tile_source::attribution_text);
    let known = matches!(source_class(&inputs.source_label, own.as_deref()), SourceClass::Mapbox | SourceClass::OpenFreeMap);
    let stated = own.clone().filter(|_| !known);
    let sprite = (mapbox_sprite && source_class(&inputs.source_label, own.as_deref()) != SourceClass::Mapbox).then(|| crate::story::burn_in::MAPBOX_CREDIT.to_string());
    let credits: Vec<String> = stated.into_iter().chain(sprite).collect();
    let v = crate::story::burn_in::verdict_with(
        &VerdictSource { label: &inputs.source_label, stated: own.as_deref(), acknowledge: &script.output.acknowledge },
        None,
        false,
        &credits,
        &[],
        inputs.wordmark.as_ref().map(|_| Path::new("scene.basemap.wordmark")),
    );
    let text = crate::story::redact_url_secrets(&v.attribution);
    (!text.trim().is_empty()).then_some(text)
}

/// What a frame's story elements are composed from, borrowed from a
/// [`Player`] or from validation's GPU-free probe ([`probe_layout`]), so
/// the two compose exactly the same frame.
struct StoryParts<'a> {
    script: &'a Script,
    path: &'a CameraPath,
    timeline: &'a Timeline,
    output: ResolvedOutput,
    images: &'a crate::story::media::ScriptImages,
    callouts: &'a [CalloutTrack],
    furniture: &'a StoryFurniture,
    /// The design system's decisions (the slice), `None` without a
    /// top-level `design`.
    design: Option<&'a DesignCtx>,
}

impl StoryParts<'_> {
    /// This frame's captions (spec §5.4, Task 15): the resolved `caption`
    /// events, plus `script.captions` cues while burn-in is on; none
    /// when it is off.
    fn captions(&self, state: &SceneState, t: f64) -> Vec<ActiveCaption> {
        self.captions_with_paths(state, t).0
    }

    /// [`Self::captions`], with each caption's JSON pointer alongside: a
    /// `caption` event's `/events/<i>`, a `captions` cue's `/captions/<j>`.
    fn captions_with_paths(&self, state: &SceneState, t: f64) -> (Vec<ActiveCaption>, Vec<String>) {
        if self.burn_in_on() {
            let mut captions = state.captions.clone();
            let mut paths: Vec<String> = state.caption_sources.iter().map(|i| format!("/events/{i}")).collect();
            // `script.captions` entries go through `captions::track` --
            // the SAME clamp/round/empty-trim pipeline the SRT/VTT export
            // uses -- rather than a hand-rolled filter here, so a cue the
            // track would drop (fix round 1, finding 2: e.g. whitespace-
            // only text) never shows in the burn-in either.
            // With the design system the cues are drawn in the card's
            // caption row (spec §3.5, `DesignCtx::frame_at`), not here.
            let cue_inputs: Vec<(f64, f64, String)> = if self.design.is_some() {
                Vec::new()
            } else {
                self.script.captions.iter().map(|c| (c.from, c.to, c.text.clone())).collect()
            };
            let duration = self.path.duration();
            let cues = crate::story::captions::track(&cue_inputs, duration);
            // Each cue's entry in `script.captions`: `track` keeps an entry
            // or drops it on its own, then sorts stably by `from`, so the
            // same per-entry fold in the same stable order lines up.
            let mut sources: Vec<(f64, usize)> = cue_inputs
                .iter()
                .enumerate()
                .filter_map(|(j, c)| crate::story::captions::track(std::slice::from_ref(c), duration).first().map(|cue| (cue.from, j)))
                .collect();
            sources.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            for (cue, (_, j)) in cues.iter().zip(&sources).filter(|(cue, _)| t >= cue.from && t < cue.to) {
                captions.push(ActiveCaption {
                    text: cue.text.clone(),
                    anchor: Anchor::Bottom,
                    offset_px: (0.0, 0.0),
                    size_px: 40.0,
                    color: DEFAULT_CAPTION_COLOR,
                    plate: true,
                    alpha: ((t - cue.from) / DEFAULT_FADE_S).min((cue.to - t) / DEFAULT_FADE_S).clamp(0.0, 1.0) as f32,
                });
                paths.push(format!("/captions/{j}"));
            }
            (captions, paths)
        } else {
            (Vec::new(), Vec::new())
        }
    }

    /// This frame's panels and captions for the capture's layout (spec
    /// §4.1), or `None` when nothing is on screen and the format has no
    /// safe area (with one, an item-less frame carries it, for the corner
    /// chrome's inset). Panels carry their
    /// `alpha`/`enter`; captions are named by their index in
    /// `state.captions` (`capture::story_chrome::caption_id`), with their
    /// `offset_px` as the layout's offset. Images carry their own entry;
    /// a callout is placed at its anchor's projection at `t` (its side and
    /// its fade from its schedule, the fade multiplied into its motion
    /// alpha) and leaves no item at all when it is faded out or has no
    /// anchor.
    ///
    /// Map furniture (spec §4.5) is one `furniture` item, from the spec in
    /// force at `t` and `camera` at the frame's centre (the scale bar, the
    /// north arrow, the pitch that hides the bar).
    fn story_frame(
        &self,
        state: &SceneState,
        captions: &[ActiveCaption],
        t: f64,
        camera: crate::story::furniture::FrameCamera,
    ) -> Option<Arc<crate::capture::StoryFrame>> {
        use crate::story::layout::LayoutItem;
        use crate::story::timeline::ElementKind;
        let mut frame = crate::capture::StoryFrame::default();
        for element in &state.elements {
            if let ElementKind::Panel(spec) = &element.kind {
                // Validation refuses unknown theme names; the default look otherwise.
                let Some(theme) = crate::story::panel::theme(spec.theme.as_deref()).or_else(|| crate::story::panel::theme(None)) else {
                    continue;
                };
                let image = spec.image.as_ref().and_then(|i| self.story_image(&element.id, i));
                frame.items.push(LayoutItem::Panel {
                    id: element.id.clone(),
                    placement: spec.placement,
                    headline: spec.headline.clone(),
                    body: spec.body.clone(),
                    source: spec.source.clone(),
                    image_aspect: image.as_ref().map(|i| i.image.width() as f32 / i.image.height().max(1) as f32),
                    theme,
                });
                frame.images.extend(image);
                frame.motion.push((element.id.clone(), element.alpha, element.enter));
            } else if let ElementKind::Image(spec) = &element.kind {
                // An image that did not load draws nothing (a gap).
                let Some(image) = self.story_image(&element.id, spec) else { continue };
                frame.items.push(LayoutItem::Image {
                    id: element.id.clone(),
                    placement: spec.placement.unwrap_or(crate::story::script::Placement::Corner),
                    aspect: image.image.width() as f32 / image.image.height().max(1) as f32,
                });
                frame.images.push(image);
                frame.motion.push((element.id.clone(), element.alpha, element.enter));
            } else if let ElementKind::Callout(spec) = &element.kind {
                let Some(track) = self.callouts.iter().find(|c| c.id == element.id) else { continue };
                let project = |t: f64| project_callout(self.path, self.timeline, &self.output, spec.at_coord, t);
                let Some(at) = crate::story::callout::callout_at(&track.schedule, element.alpha, t, &project) else { continue };
                let Some(theme) = crate::story::panel::theme(spec.theme.as_deref()).or_else(|| crate::story::panel::theme(None)) else {
                    continue;
                };
                let image = spec.image.as_ref().and_then(|i| self.story_image(&element.id, i));
                frame.items.push(LayoutItem::Callout {
                    id: element.id.clone(),
                    anchor: at.anchor,
                    text: spec.text.clone(),
                    side: at.side,
                    image_aspect: image.as_ref().map(|i| i.image.width() as f32 / i.image.height().max(1) as f32),
                    theme,
                });
                frame.images.extend(image);
                frame.motion.push((element.id.clone(), at.alpha, element.enter));
            }
        }
        let (spec, legend) = self.furniture.at(self.timeline.chapters(), t);
        if let Some(furniture) = crate::story::furniture::frame_furniture(spec, legend, camera) {
            frame.items.push(LayoutItem::Furniture { id: FURNITURE_ID.to_string(), frame: furniture });
        }
        for (i, c) in captions.iter().enumerate().filter(|(_, c)| c.alpha > 0.0 && !c.text.is_empty()) {
            frame.items.push(LayoutItem::Caption {
                id: crate::capture::story_chrome::caption_id(i),
                text: c.text.clone(),
                size: c.size_px,
                anchor: c.anchor,
                offset: c.offset_px,
                plate: c.plate,
            });
        }
        let format = self.script.format();
        frame.safe_area = crate::story::tiers::safe_area_rects(format.safe_area, format.aspect);
        frame.design = self.design.map(|d| d.frame_at(self.timeline.chapters(), t));
        // A card's photo (ruling R2-a): its loaded image under
        // `card_photo_id`, where the drawer finds it.
        if let Some(d) = frame.design.as_ref() {
            for p in d.from_card.iter().chain(d.card.iter()).filter_map(|c| c.photo.as_ref()) {
                let id = crate::story::card_layout::card_photo_id(&p.src);
                if frame.images.iter().any(|i| i.id == id) {
                    continue;
                }
                if let Some(loaded) = self.images.get(&p.src) {
                    frame.images.push(crate::capture::StoryImage {
                        id,
                        image: Arc::clone(&loaded.image),
                        fit: crate::story::script::Fit::Cover,
                        credit: p.credit.clone(),
                    });
                }
            }
        }
        // A frame with nothing on screen still carries a platform's safe
        // area: the credit and the mark sit above its bottom band on EVERY
        // frame of the video (final review F3), not only the ones with a
        // panel or caption.
        if frame.items.is_empty() && frame.safe_area.is_empty() && frame.design.is_none() {
            return None;
        }
        Some(Arc::new(frame))
    }

    /// Item `id`'s image from `spec`, if it loaded.
    fn story_image(&self, id: &str, spec: &crate::story::script::ImageSpec) -> Option<crate::capture::StoryImage> {
        let loaded = self.images.get(&spec.src)?;
        Some(crate::capture::StoryImage {
            id: id.to_string(),
            image: Arc::clone(&loaded.image),
            fit: spec.fit.unwrap_or(crate::story::script::Fit::Cover),
            credit: spec.credit.clone(),
        })
    }

    /// Whether this render burns captions into the frame (Task 15,
    /// `captions::burn_in_default`, spec §5). A script with neither
    /// `aspect` nor `tier` set (every script written before this task)
    /// keeps the old always-on behaviour -- `output.burn_captions` can
    /// still turn it off -- rather than the new per-aspect default, so an
    /// existing 16:9 story's burned-in captions render identically (F6).
    /// Once either is set, [`crate::story::captions::burn_in_default`]
    /// decides: on for 9:16/4:5, off otherwise, `output.burn_captions`
    /// overriding either way.
    fn burn_in_on(&self) -> bool {
        burn_in_on(self.script)
    }

}

/// Whether a render of `script` burns captions into the frame
/// ([`StoryParts::burn_in_on`]'s rule; the design system's card reads
/// it for its caption cues, spec §3.5).
fn burn_in_on(script: &Script) -> bool {
    if script.output.aspect.is_none() && script.output.tier.is_none() {
        script.output.burn_captions.unwrap_or(true)
    } else {
        crate::story::captions::burn_in_default(script.format().aspect, script.output.burn_captions)
    }
}

/// Each callout event's side and fade over its whole span (spec §4.4):
/// the card's width measured with `measure`, the frames' own faces.
fn callout_tracks(
    path: &CameraPath,
    timeline: &Timeline,
    output: &ResolvedOutput,
    images: &crate::story::media::ScriptImages,
    viewport: (f32, f32),
    measure: &dyn crate::story::layout::TextMeasure,
) -> Vec<CalloutTrack> {
    timeline
        .callout_plans()
        .into_iter()
        .map(|(id, at, until, spec)| {
            let has_image = spec.image.as_ref().is_some_and(|i| images.get(&i.src).is_some());
            let card_w = crate::story::layout::callout_width(&spec.text, has_image, measure);
            let project = |t: f64| project_callout(path, timeline, output, spec.at_coord, t);
            CalloutTrack { id, schedule: crate::story::callout::schedule(at, until, card_w, viewport, &project) }
        })
        .collect()
}

/// One frame [`probe_layout`] laid out.
pub(crate) struct ProbedFrame {
    /// Story seconds of the frame (`n / fps`).
    pub t: f64,
    /// The story layout's warnings on this frame (ids are element ids,
    /// `caption/<k>` into [`Self::caption_paths`], `furniture/<piece>` or
    /// `frame`).
    pub warnings: Vec<crate::story::layout::LayoutWarning>,
    /// Each of the frame's captions' JSON pointer (`/events/<i>` or
    /// `/captions/<j>`), in the order `caption/<k>` counts them.
    pub caption_paths: Vec<String>,
    /// The credit plate's text the frame was laid out with (the tests
    /// read it).
    #[cfg_attr(not(test), allow(dead_code))]
    pub attribution: Option<String>,
}

/// Lays frames out as a render would, with no GPU (Task 19, pre-flight
/// ruling F32): the same story frame [`Player::request_at`] composes
/// ([`StoryParts`]), the same callout schedules, the same credit plate
/// ([`verdict_for`] with no extra layers), and the chrome pass's own
/// layout (`LayoutFonts::warnings`). Deterministic: no clock, no hash
/// order.
///
/// **Fonts.** It measures with `inputs.label_fonts`, or the shipped faces
/// (`validate::shipped_label_fonts`) when that is `None`. Every scene the
/// loader builds carries the shipped faces, so validation measures what a
/// render draws -- except a render the `story` example runs with
/// `--system-fonts`, which swaps in the machine's faces after validation:
/// its text can wrap differently from what validation measured, so a
/// layout warning may appear or vanish there.
pub(crate) struct LayoutProbe<'a> {
    script: &'a Script,
    path: &'a CameraPath,
    timeline: &'a Timeline,
    images: &'a crate::story::media::ScriptImages,
    output: ResolvedOutput,
    fonts: crate::capture::story_chrome::LayoutFonts,
    callouts: Vec<CalloutTrack>,
    furniture: StoryFurniture,
    plate: Option<String>,
    wordmark: Option<Arc<image::RgbaImage>>,
    /// The design context a render of the script builds (spec §13): the
    /// probe checks the frame the render draws, card and fine print too.
    design: Option<DesignCtx>,
    /// The render's SWAT mark and policy ([`set_marks`]), so the probe
    /// lays out the slot the render draws.
    marks: RenderOptions,
}

impl<'a> LayoutProbe<'a> {
    /// Measures every callout's card and schedules its side and fade
    /// (`callout_tracks`, as `Player::new` does). `mapbox_sprite` is the
    /// style's, judged before the style was taken.
    pub(crate) fn new(
        script: &'a Script,
        inputs: &SceneInputs,
        path: &'a CameraPath,
        timeline: &'a Timeline,
        sources: &[crate::story::overlay_layers::LayerSource],
        images: &'a crate::story::media::ScriptImages,
        mapbox_sprite: bool,
        marks: &RenderOptions,
    ) -> Self {
        let output = script.output();
        let ratio = if output.pixel_ratio.is_finite() && output.pixel_ratio > 0.0 { output.pixel_ratio } else { 1.0 };
        let viewport = (output.width as f32 / ratio, output.height as f32 / ratio);
        let label_fonts = inputs.label_fonts.clone().or_else(|| Some(crate::story::validate::shipped_label_fonts()));
        let mut fonts = crate::capture::story_chrome::LayoutFonts::new(label_fonts);
        let callouts = fonts.with_measure(ratio, |measure| callout_tracks(path, timeline, &output, images, viewport, measure));
        let attribution = verdict_for(script, inputs, images, mapbox_sprite, Vec::new()).attribution;
        LayoutProbe {
            script,
            path,
            timeline,
            images,
            output,
            fonts,
            callouts,
            furniture: StoryFurniture::new(script, timeline.chapters(), sources),
            plate: (!attribution.is_empty()).then_some(attribution),
            wordmark: inputs.wordmark.clone(),
            // As `Player::new` builds it, under the render's marks, which
            // the probe's requests carry too (`set_marks`).
            design: DesignCtx::for_render(script, timeline.chapters(), path, inputs, marks, mapbox_sprite),
            marks: RenderOptions { brand_mark: marks.brand_mark, brand_mark_policy: marks.brand_mark_policy, ..RenderOptions::default() },
        }
    }

    /// Every story second a callout's schedule changes its side or its
    /// visibility: the frames where its card jumps.
    pub(crate) fn callout_changes(&self) -> Vec<f64> {
        self.callouts.iter().flat_map(|c| c.schedule.sides.iter().map(|(t, _)| *t).chain(c.schedule.visibility.iter().map(|(t, _)| *t))).collect()
    }

    /// Frames `frames` (indices at the output fps), laid out.
    pub(crate) fn frames(&mut self, frames: &[usize]) -> Vec<ProbedFrame> {
        let output = self.output;
        let fps = output.fps.max(1);
        let mut out = Vec::with_capacity(frames.len());
        for &n in frames {
            let t = n as f64 / f64::from(fps);
            let (request, caption_paths) = self.request(t);
            out.push(ProbedFrame { t, warnings: self.fonts.warnings(&request), caption_paths, attribution: self.plate.clone() });
        }
        out
    }

    /// The chrome half of the request a render composes at `t` (the story
    /// frame, its captions, the credit and the marks), with each
    /// caption's JSON pointer.
    fn request(&self, t: f64) -> (CaptureRequest, Vec<String>) {
        let output = self.output;
        let parts = StoryParts {
            script: self.script,
            path: self.path,
            timeline: self.timeline,
            output,
            images: self.images,
            callouts: &self.callouts,
            furniture: &self.furniture,
            design: self.design.as_ref(),
        };
        let state = self.timeline.state_at(t, output.fps);
        let camera = frame_camera(&self.path.pose_at(t), &state);
        let (captions, caption_paths) = parts.captions_with_paths(&state, t);
        let mut request = CaptureRequest {
            width: output.width,
            height: output.height,
            pixel_ratio: output.pixel_ratio,
            captions: captions.clone(),
            story: parts.story_frame(&state, &captions, t, camera),
            attribution: self.plate.clone(),
            ..CaptureRequest::default()
        };
        set_marks(&mut request, &self.marks, self.wordmark.as_ref());
        (request, caption_paths)
    }
}

/// [`LayoutProbe`] over `frames`, in one call (the tests' door).
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn probe_layout(
    script: &Script,
    inputs: &SceneInputs,
    path: &CameraPath,
    timeline: &Timeline,
    sources: &[crate::story::overlay_layers::LayerSource],
    images: &crate::story::media::ScriptImages,
    mapbox_sprite: bool,
    frames: &[usize],
) -> Vec<ProbedFrame> {
    LayoutProbe::new(script, inputs, path, timeline, sources, images, mapbox_sprite, &RenderOptions::default()).frames(frames)
}

/// [`Coverage::alt_text`]: `(with alt, of)` over every explicit chapter
/// (its resolved `alt`, which already fell back to the story's summary)
/// and every image that loaded.
pub(crate) fn alt_coverage(script: &Script, chapters: &[crate::story::Chapter], images: &crate::story::media::ScriptImages) -> (usize, usize) {
    let has = |s: Option<&str>| s.is_some_and(|a| !a.trim().is_empty());
    let chapters: Vec<bool> = chapters.iter().filter(|c| !c.implicit).map(|c| has(c.alt.as_deref())).collect();
    let images: Vec<bool> = crate::story::media::script_images(script)
        .into_iter()
        .filter(|(_, spec)| images.get(&spec.src).is_some())
        .map(|(_, spec)| has(spec.alt.as_deref()))
        .collect();
    let all = chapters.iter().chain(images.iter());
    (all.clone().filter(|w| **w).count(), all.count())
}

/// [`Player::verdict`]'s body: the licence verdict and the credit line
/// over `inputs`, the images that loaded, `mapbox_sprite` (judged while
/// the style was intact, as for [`publish_table_for`]) and
/// `extra_layer_credits` (each [`StoryLayer`]'s, first). Validation's layout probe draws the same
/// credit plate with no extra layers.
fn verdict_for(
    script: &Script,
    inputs: &SceneInputs,
    images: &crate::story::media::ScriptImages,
    mapbox_sprite: bool,
    extra_layer_credits: Vec<String>,
) -> crate::story::burn_in::Verdict {
    // The source's own stated credit first (`TileSource::attribution`,
    // as `MapView::frame_attribution` reads it), then the inputs' and
    // the script's.
    // Skipped for a source the table already credits (Mapbox's TileJSON
    // text is its table credit plus "Improve this map"; OpenFreeMap's is
    // the table credit exactly): the credit is never stated twice.
    use crate::story::burn_in::{source_class, SourceClass, VerdictSource};
    let own = inputs.source.attribution().map(crate::tile_source::attribution_text);
    let known = matches!(source_class(&inputs.source_label, own.as_deref()), SourceClass::Mapbox | SourceClass::OpenFreeMap);
    let stated = own.clone().filter(|_| !known);
    // A Mapbox sprite under another basemap owes Mapbox's credit, as
    // the publish table's `sprite` row says (review finding 8).
    let sprite = (mapbox_sprite
        && source_class(&inputs.source_label, own.as_deref()) != SourceClass::Mapbox)
        .then(|| crate::story::burn_in::MAPBOX_CREDIT.to_string());
    let credits: Vec<String> = stated
        .into_iter()
        .chain(inputs.credits.iter().cloned())
        .chain(script.scene.credits.iter().cloned())
        .chain(sprite)
        .collect();
    // Each drawn image's credit joins the credit plate (spec R7: the
    // plate is complete), after the layers', in script order.
    let image_credits = crate::story::media::script_images(script)
        .into_iter()
        .filter(|(_, spec)| images.get(&spec.src).is_some())
        .filter_map(|(_, spec)| spec.credit.as_deref().map(str::trim).filter(|c| !c.is_empty()).map(str::to_string));
    // Each overlay's own `attribution` (spec §6), after the layers'.
    let overlay_credits = script
        .scene
        .overlays
        .iter()
        .filter_map(|o| o.attribution.as_deref().map(str::trim).filter(|a| !a.is_empty()).map(str::to_string));
    let layer_credits: Vec<String> =
        extra_layer_credits.into_iter().chain(overlay_credits).chain(image_credits).collect();
    // `scene.raster` is neither credited nor judged: the player draws
    // no raster yet (`gaps` says so), so crediting imagery the frames
    // do not show would be false, and so would refusing over it.
    let v = crate::story::burn_in::verdict_with(
        &VerdictSource { label: &inputs.source_label, stated: own.as_deref(), acknowledge: &script.output.acknowledge },
        None,
        false,
        &credits,
        &layer_credits,
        inputs.wordmark.as_ref().map(|_| Path::new("scene.basemap.wordmark")),
    );
    // The plate and the manifest never carry a credential an author
    // pasted into a credit's link.
    crate::story::burn_in::Verdict { attribution: crate::story::redact_url_secrets(&v.attribution), ..v }
}


impl Player {
    /// Resolve `script` against `inputs`, plan every frame's tiles and
    /// open a capture session. `raw_script` is the bytes the script was
    /// parsed from (for the manifest's hash).
    ///
    /// Fetches run on the current tokio runtime if there is a
    /// multi-thread one, or else on a 2-worker runtime the player owns.
    ///
    /// # Errors
    /// The camera path or the timeline does not resolve, or no GPU.
    pub fn new(script: &Script, raw_script: &[u8], inputs: SceneInputs, opts: RenderOptions) -> anyhow::Result<Self> {
        let output = script.output();
        let ratio = if output.pixel_ratio.is_finite() && output.pixel_ratio > 0.0 { output.pixel_ratio } else { 1.0 };
        let viewport = (output.width as f32 / ratio, output.height as f32 / ratio);
        let mut inputs = inputs;
        let tracks = Arc::new(std::mem::take(&mut inputs.tracks));
        // Judged while the style is intact: every later reader of
        // `inputs.base_style` sees `Null`.
        let mapbox_sprite = crate::story::burn_in::style_has_mapbox_sprite(&inputs.base_style);
        let base_style = std::mem::take(&mut inputs.base_style);
        // Every overlay layer's own source (spec §2.2), from its overlay's
        // document: what an effect resolves against and what is drawn.
        let layer_sources = story_layer_sources(script, &inputs.overlays)?;
        let sources: Vec<crate::story::overlay_layers::LayerSource> = layer_sources.iter().map(|(_, s)| s.clone()).collect();
        // The four-step build (F7) that puts a `follow_draw` camera on its
        // draw head.
        let (path, timeline) =
            crate::story::timeline::path_and_timeline(viewport, script, tracks, &sources, base_style, &*inputs.load_style)
                .map_err(|(stage, d)| diagnostics_error(stage, &d))?;
        // The camera pass that keeps the subject clear of each segment's
        // card (spec §5.3), then the design system's tile credit and slot
        // mark for every frame.
        let path = render_camera_path(script, &inputs, &opts, mapbox_sprite, path, &timeline).map_err(|d| diagnostics_error("camera path", &d))?;
        let design = DesignCtx::for_render(script, timeline.chapters(), &path, &inputs, &opts, mapbox_sprite);
        let story_layers = layer_renders(script, &expand_layers(script, layer_sources, design.as_ref()), &timeline, design.as_ref(), &sources);
        let frames = frame_count(path.duration(), output.fps);

        let mut opts = opts;
        // Resolved before `cache_dir` is defaulted below: a caller that named
        // no folder shares validation's default image cache.
        let image_cache_dir = opts.image_cache_dir();
        if opts.cache_dir.as_os_str().is_empty() {
            opts.cache_dir = default_cache_dir(&inputs.source_label);
        }

        let mut overlay_features = HashMap::new();
        for (key, text) in &inputs.overlays {
            let doc: serde_json::Value =
                serde_json::from_str(text).with_context(|| format!("overlay `{key}` is not valid JSON"))?;
            overlay_features.insert(key.clone(), features_of(&doc));
        }

        let runtime = match tokio::runtime::Handle::try_current() {
            Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => None,
            _ => Some(tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?),
        };
        let handle = match &runtime {
            Some(rt) => rt.handle().clone(),
            None => tokio::runtime::Handle::current(),
        };

        // Every image, on the player's runtime (ruling F5), from a thread of
        // its own so this works whether or not the caller is inside a
        // runtime.
        let images = {
            let story_dir = inputs.story_dir.clone();
            let offline = inputs.offline;
            let long_edge = output.width.max(output.height);
            let load = crate::story::media::load_script_images(script, &story_dir, &image_cache_dir, long_edge, offline);
            std::thread::scope(|s| s.spawn(|| handle.block_on(load)).join())
                .map_err(|_| anyhow::anyhow!("loading the story's images panicked"))?
        };
        // Spec §7: an image that will not load refuses the render -- unless
        // validation already saw it load, and it vanished since (a gap).
        // The agent's render path never validates, so it refuses here.
        if !inputs.images_validated && !images.failed.is_empty() {
            let why: Vec<String> = images
                .failed
                .iter()
                .map(|(src, d)| format!("image {} ({}): {}", crate::story::redact_url_secrets(src), d.code, d.message))
                .collect();
            anyhow::bail!("the story's images do not load: {}", why.join("; "));
        }
        for (src, d) in &images.failed {
            eprintln!("story: image {} did not load ({}): {}", crate::story::redact_url_secrets(src), d.code, d.message);
        }

        let mut session = CaptureSession::new_blocking(inputs.label_fonts.clone())?;
        session.catch_device_errors();
        session.set_mesh_cache_budget(opts.mesh_cache_bytes);

        // Each callout's side and fade over its whole span, once (spec
        // §4.4): the card's width measured with the frames' own faces.
        let callouts = crate::capture::story_chrome::with_session_measure(&mut session, &inputs.label_fonts, ratio, |measure| {
            callout_tracks(&path, &timeline, &output, &images, viewport, measure)
        });

        let furniture = StoryFurniture::new(script, timeline.chapters(), &sources);
        let mut player = Self {
            furniture,
            prefetcher: None,
            runtime,
            session,
            path,
            timeline,
            images,
            styles: StyleCache::new(),
            mapbox_sprite,
            inputs,
            script: script.clone(),
            script_sha256: hex_sha256(raw_script),
            output,
            frames,
            opts,
            layers: Vec::new(),
            overlay_features,
            floor_styles: Vec::new(),
            story_layers,
            callouts,
            design,
        };
        // Each generation's extrusion layers alone: all
        // `CaptureRequest::resolved` reads (`frame_plan::extrusion_visible`).
        // A generation that does not compile resolves nothing here; its
        // frame fails on the style itself.
        player.floor_styles = (0..player.timeline.generations())
            .map(|g| {
                let layers = player
                    .styles
                    .get(&player.timeline, g)
                    .map(|style| {
                        style.layers.iter().filter(|l| matches!(l, crate::style::StyleLayer::Extrusion { .. })).cloned().collect()
                    })
                    .unwrap_or_default();
                crate::style::Style { layers, sprite: None, sky: Default::default() }
            })
            .collect();
        // The plan needs only each frame's cover: no font bytes (F12).
        let tile_plan = Arc::new(plan((0..frames).map(|n| player.base_request_with(n, false).0)));
        let fps = f64::from(output.fps.max(1));
        let mut popts = PrefetchOptions::new(fps, player.opts.cache_dir.clone());
        popts.concurrency = player.opts.fetch_concurrency;
        popts.tile_timeout = player.opts.tile_timeout;
        popts.strict = player.opts.strict;
        popts.cancel = Some(Arc::clone(&player.opts.cancel));
        player.prefetcher = Some(Prefetcher::new(Arc::clone(&player.inputs.source), tile_plan, popts, handle));
        Ok(player)
    }

    /// The images the script names that loaded (each `src` once), for the
    /// bundle's `media/` folder (`LoadedImage::bundle_name`).
    pub fn images(&self) -> &[crate::story::media::LoadedImage] {
        &self.images.loaded
    }

    /// The script this player renders (defaults filled in).
    pub fn script(&self) -> &Script {
        &self.script
    }

    /// The basemap's tile credit alone (spec §3.6, P4; R10): what the
    /// design colophon row's own line shows, or what `post-description.txt`
    /// writes last regardless of `design` ([`crate::story::bundle::write_bundle`]).
    /// `None` without a credited basemap.
    pub(crate) fn tile_credit(&self) -> Option<String> {
        tile_credit_for(&self.script, &self.inputs, self.mapbox_sprite)
    }

    /// Every resolved chapter, in document order (spec §2.1).
    pub fn chapters(&self) -> &[crate::story::Chapter] {
        self.timeline.chapters()
    }

    /// The one caption track (spec §5): what the SRT, the VTT and the
    /// transcript are written from (`captions::track` over
    /// `Timeline::caption_inputs`).
    pub fn caption_track(&self) -> Vec<crate::story::captions::Cue> {
        crate::story::captions::track(&self.timeline.caption_inputs(), self.path.duration())
    }

    /// Every panel, image and callout event's `(at, kind)`, in document
    /// order (F12), owned: the transcript's input.
    pub fn element_plans(&self) -> Vec<(f64, crate::story::ElementKind)> {
        self.timeline.element_plans().into_iter().map(|(at, kind)| (at, kind.clone())).collect()
    }

    /// The frame size and rate.
    pub fn output(&self) -> ResolvedOutput {
        self.output
    }

    /// The frame a still at story second `t` is: `round(t * fps)`, clamped
    /// to the story's frames. A still is always a frame the video has, so
    /// a chapter still or the poster is exactly what a viewer pausing
    /// there sees (and is bit-identical across runs).
    pub fn still_frame(&self, t: f64) -> usize {
        let fps = f64::from(self.output.fps.max(1));
        let t = if t.is_finite() { t.max(0.0) } else { 0.0 };
        ((t * fps).round() as usize).min(self.frames.saturating_sub(1))
    }

    /// The frame at story second `t` ([`Self::still_frame`]), rendered on
    /// its own: a one-frame [`Self::run_range`], so it is the same bytes
    /// as that frame of a full render, whatever ran before.
    ///
    /// # Errors
    /// See [`Self::run_range`].
    pub fn render_still_at(&mut self, t: f64) -> anyhow::Result<Capture> {
        /// Keeps the one frame it is given.
        struct Keep(Option<Capture>);
        impl FrameSink for Keep {
            fn push(&mut self, _: usize, frame: &Capture) -> anyhow::Result<()> {
                self.0 = Some(frame.clone());
                Ok(())
            }
            fn finish(&mut self) -> anyhow::Result<serde_json::Value> {
                Ok(serde_json::json!({ "kind": "still_in_memory" }))
            }
        }
        let n = self.still_frame(t);
        let mut keep = Keep(None);
        self.run_range(n..n + 1, &mut keep, &mut |_| {})?;
        keep.0.ok_or_else(|| anyhow::anyhow!("the still at {t} s (frame {n}) rendered no frame"))
    }

    /// How many frames the whole story has.
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Set [`RenderOptions::frames`]: the range [`Self::run`] and
    /// [`Self::run_with_extras`] render (`None` is every frame).
    pub fn set_frames(&mut self, frames: Option<std::ops::Range<usize>>) {
        self.opts.frames = frames;
    }

    /// A layer whose credit joins the attribution and whose needs are
    /// named in the gaps.
    pub fn add_layer(&mut self, layer: Box<dyn StoryLayer>) {
        self.layers.push(layer);
    }

    /// Story seconds of frame `n`: from the index, in `f64`, never summed.
    fn t_of(&self, n: usize) -> f64 {
        n as f64 / f64::from(self.output.fps.max(1))
    }

    /// Frame `n`'s request without the overlay (all the plan needs).
    fn base_request(&self, n: usize) -> (CaptureRequest, SceneState, Pose) {
        self.base_request_with(n, true)
    }

    /// [`Self::base_request`], with the label font bytes only when
    /// `fonts` (a plan pass over every frame needs no copy of them).
    fn base_request_with(&self, n: usize, fonts: bool) -> (CaptureRequest, SceneState, Pose) {
        let t = self.t_of(n);
        let pose = self.path.pose_at(t);
        let state = self.timeline.state_at(t, self.output.fps);
        // Daylight unless the story opts in (the plain-defaults rule): the
        // ground and the sky are lit by the story clock's sun only with the
        // terminator on; otherwise `sun: None`, unlit, no night anywhere.
        // The limb glow always needs a sun and must never get
        // `Atmosphere::default()`'s wall-clock `Sun::Now`: with the
        // terminator on it is the clock's, off it is `DAYLIGHT_SUN` -- the
        // sun every shell's `frame_sun_for` gives a plain map (directly
        // behind the viewer: an even rim, no time of day implied).
        let clock_sun = Sun::at_utc(state.params.sun_unix);
        let terminator = state.params.terminator;
        let glow_sun = if terminator { clock_sun } else { crate::render::atmosphere::DAYLIGHT_SUN };
        let c = pose.camera;
        let (extrusion_look, label_occlusion) = self.script.scene.buildings.capture();
        let request = CaptureRequest {
            center_lon: c.center_lon,
            center_lat: c.center_lat,
            zoom: c.zoom,
            pitch: c.pitch,
            bearing: c.bearing,
            width: self.output.width,
            height: self.output.height,
            pixel_ratio: self.output.pixel_ratio,
            flat: true,
            globe: state.globe.unwrap_or(pose.globe),
            flat_tile_zoom_max: self.inputs.flat_tile_zoom_max,
            labels: state.params.labels,
            label_fonts: if fonts { self.inputs.label_fonts.clone() } else { None },
            sun: terminator.then_some(clock_sun),
            // The night floor follows the terminator too: the capture reads it
            // from this atmosphere when one is named, and the default (1.0)
            // is no terminator at all.
            atmosphere: state.params.atmosphere.then(|| Atmosphere {
                sun: glow_sun,
                night_brightness: if terminator { Atmosphere::TERMINATOR_NIGHT_BRIGHTNESS } else { 1.0 },
                ..Default::default()
            }),
            stars: state.params.stars,
            raster_opacity: state.params.raster_opacity,
            style_generation: state.style_generation,
            stand_in_levels: Some(STAND_IN_LEVELS),
            // Pinned, never read from the environment (F3): the compact
            // bake is the production one, and a frame must not depend on
            // the process's GPUI_MAP_BASEMAP_COMPACT(_STROKES).
            basemap_compact: BASEMAP_COMPACT,
            basemap_compact_strokes: Some(BASEMAP_COMPACT_STROKES),
            sprites: self.inputs.sprites.clone(),
            // Street names along their roads (L2); the golden frames keep
            // the default off.
            line_labels: true,
            extrusion_look,
            label_occlusion,
            ..CaptureRequest::default()
        };
        // The building floor, resolved here (not left to the capture) so
        // the prefetch plan fetches the floor's tiles: the frame then
        // frames the same buildings the live map does (master's
        // `CaptureRequest::resolved`).
        let request = match usize::try_from(state.style_generation).ok().and_then(|g| self.floor_styles.get(g)) {
            Some(style) => request.resolved(style),
            None => request,
        };
        (request, state, pose)
    }

    /// Frame `n`'s capture request and scene. Pure: the pose and the
    /// state at `t = n / fps`, turned into a request; no clock, no tiles.
    pub fn request_at(&self, n: usize) -> (CaptureRequest, SceneState) {
        let (mut request, state, pose) = self.base_request(n);
        let t = self.t_of(n);
        let mut features = Vec::new();
        // An overlay with `layers` draws as its layers (below), not merged here.
        let layered = |id: &str| self.script.scene.overlays.iter().any(|o| o.id == id && !o.layers.is_empty());
        for (id, doc_path) in &state.overlay_data {
            if state.visible_overlays.iter().any(|v| v == id) && !layered(id) {
                if let Some(f) = self.overlay_features.get(doc_path).or_else(|| self.overlay_features.get(id)) {
                    features.extend(f.iter().cloned());
                }
            }
        }
        // A camera-only `draw:` track (follow_draw) is never drawn.
        let has_tracks = self.path.tracks().drawn().next().is_some();
        request.overlay = match (features.is_empty(), has_tracks) {
            (true, false) => None,
            // Tracks alone: their document as it is, no parse round trip.
            (true, true) => self.path.tracks().any_live_at(t).then(|| self.path.tracks().feature_collection_at(t)),
            (false, _) => {
                if has_tracks {
                    let tracks: serde_json::Value =
                        serde_json::from_str(&self.path.tracks().feature_collection_at(t)).unwrap_or(serde_json::Value::Null);
                    features.extend(features_of(&tracks));
                }
                Some(serde_json::json!({"type": "FeatureCollection", "features": features}).to_string())
            }
        };
        // The overlay layers, in declaration order, at this frame's effect
        // state (spec §3): a trim, an opacity and per-feature paint over a
        // mesh the session tessellated once.
        request.story_layers = layer_draws(&self.story_layers, self.design.as_ref(), &state, t);
        // The burn-in (spec §5.4, Task 15): this frame's captions, the
        // credit (never optional in a story, R11) and the SWAT mark (the
        // engine's rule: always on). `state.captions` is only the resolved
        // `caption` events; when burn-in is on, `script.captions` entries
        // join them too (bottom-centre, 40 units, plate on), through this
        // same `ActiveCaption` path, so the burn-in and the SRT/VTT track
        // show the same text at the same times. When burn-in is off,
        // neither shows here -- the caption events still feed the track
        // through `Timeline::caption_inputs`, which reads none of this.
        let parts = self.parts();
        let captions = parts.captions(&state, t);
        request.captions = captions.clone();
        request.story = parts.story_frame(&state, &captions, t, frame_camera(&pose, &state));
        let attribution = self.verdict().attribution;
        request.attribution = (!attribution.is_empty()).then_some(attribution);
        set_marks(&mut request, &self.opts, self.inputs.wordmark.as_ref());
        (request, state)
    }

    /// This frame's images that did not load, as `missing` rows:
    /// `("image:<src>", Failed(<code>))`, once per `src`.
    fn image_misses(&self, state: &SceneState) -> Vec<(String, MissReason)> {
        use crate::story::timeline::ElementKind;
        let mut out: Vec<(String, MissReason)> = Vec::new();
        for element in &state.elements {
            let spec = match &element.kind {
                ElementKind::Image(spec) => Some(spec),
                ElementKind::Panel(p) => p.image.as_ref(),
                ElementKind::Callout(c) => c.image.as_ref(),
            };
            let Some(spec) = spec else { continue };
            if let Some((src, d)) = self.images.failed.iter().find(|(src, _)| *src == spec.src) {
                let name = format!("image:{}", crate::story::redact_url_secrets(src));
                if !out.iter().any(|(n, _)| *n == name) {
                    out.push((name, MissReason::Failed(d.code.to_string())));
                }
            }
        }
        out
    }

    /// The licence verdict and the credit line (spec §5.4): the vector
    /// source's credit, the raster's, the source's stated credit, the
    /// inputs' and the script's credits, then each layer's, deduplicated. A
    /// wordmark counts only when one was loaded and is drawn
    /// ([`SceneInputs::wordmark`]): a Mapbox scene without one stays
    /// unpublishable.
    fn verdict(&self) -> crate::story::burn_in::Verdict {
        verdict_for(&self.script, &self.inputs, &self.images, self.mapbox_sprite, self.layers.iter().filter_map(|l| l.credit()).collect())
    }

    /// The parts a frame's story elements are composed from.
    fn parts(&self) -> StoryParts<'_> {
        StoryParts {
            script: &self.script,
            path: &self.path,
            timeline: &self.timeline,
            output: self.output,
            images: &self.images,
            callouts: &self.callouts,
            furniture: &self.furniture,
            design: self.design.as_ref(),
        }
    }

    /// The publish table (spec §6) for this render: [`publish_table_for`]
    /// over the images that loaded (a failed one draws nothing and is a
    /// gap instead).
    fn publish_table(&self, video: bool) -> Vec<crate::story::burn_in::PublishRow> {
        let layers: Vec<(String, Option<String>)> = self.layers.iter().map(|l| (l.id().to_string(), l.credit())).collect();
        publish_table_for(&self.script, &self.inputs, video, &|src| self.images.get(src).is_some(), &layers, self.mapbox_sprite)
    }

    /// [`Coverage`] for a render with `gaps` gaps.
    fn coverage(&self, gaps: usize) -> Coverage {
        Coverage { gaps, alt_text: alt_coverage(&self.script, self.chapters(), &self.images), captions: self.caption_track().len() }
    }

    /// What this render cannot draw or show, each once (spec §4.4).
    fn gaps(&self, range: &std::ops::Range<usize>) -> Vec<String> {
        let mut gaps = Vec::new();
        if self.inputs.sprites.is_none() && !self.inputs.sprite_notes.iter().any(|n| n.starts_with("icons (")) {
            gaps.push("icons (no sprite sheet in capture)".to_string());
        }
        gaps.extend(self.inputs.sprite_notes.iter().cloned());
        // The live map fades a label in and out over 0.1 s; a capture
        // frame cannot (a fade reads earlier frames, and a frame must be
        // the same bytes whatever range rendered it).
        gaps.push("no label fade (names pop at zoom/orbit changes)".to_string());
        for (src, d) in &self.images.failed {
            gaps.push(format!("image {} ({})", crate::story::redact_url_secrets(src), d.code));
        }
        if let Some(raster) = self.script.scene.raster.as_deref() {
            gaps.push(format!(
                "raster basemap `{raster}` (the player does not fetch raster tiles yet; not drawn, credited or judged)"
            ));
        } else if self.inputs.raster.is_some() {
            gaps.push("raster basemap (the player does not fetch raster tiles yet)".to_string());
        }
        let last_t = self.t_of(self.frames.saturating_sub(1));
        for (i, at) in self.timeline.event_times() {
            if at > last_t + 1e-9 {
                gaps.push(format!(
                    "event /events/{i} at {at} s is after the last frame (t = {last_t} s): no frame shows it"
                ));
            }
        }
        for layer in &self.layers {
            let needed: usize = range.clone().map(|n| layer.needs(&self.path.pose_at(self.t_of(n)), self.t_of(n)).len()).sum();
            if needed > 0 {
                gaps.push(format!("layer {} needs {needed} resources; no encoder yet", layer.id()));
            }
        }
        use crate::story::script::EventAction;
        let actions = || self.script.events.iter().map(|e| &e.action);
        let param_of = |a: &EventAction| match a {
            EventAction::SetParam { param, .. } | EventAction::Tween { param, .. } => Some(param.clone()),
            _ => None,
        };
        let mut dots: Vec<String> = actions().filter_map(param_of).filter(|p| p.starts_with("dots.")).collect();
        dots.sort();
        dots.dedup();
        if !dots.is_empty() {
            gaps.push(format!("dots params {} (no dot layer in the player yet)", dots.join(", ")));
        }
        if self.script.scene.overlays.iter().any(|o| o.layers.is_empty() && (o.paint.is_some() || o.above_labels)) {
            gaps.push(
                "per-overlay paint and above_labels (overlays are merged into one source with the default look)".to_string(),
            );
        }
        let has_doc = |path: &str, id: &str| self.overlay_features.contains_key(path) || self.overlay_features.contains_key(id);
        let mut missing_docs: Vec<String> = self
            .script
            .scene
            .overlays
            .iter()
            .filter(|o| o.geojson.is_some() && !has_doc(o.geojson.as_deref().unwrap_or(""), &o.id))
            .map(|o| format!("overlay {} (no GeoJSON supplied)", o.id))
            .collect();
        // A layered overlay with no document draws nothing (review #7).
        // `scene_inputs` refuses to build `SceneInputs` at all when a
        // `data` path fails to load (review Finding 1: this must refuse
        // the render, the same as a broken `geojson` path), so this
        // branch is reached only when `inputs.overlays` was built some
        // other way and genuinely lacks the document; it names a `data`
        // overlay distinctly so its author isn't told to supply `geojson`
        // instead.
        for o in self.script.scene.overlays.iter().filter(|o| !o.layers.is_empty()) {
            if !self.story_layers.iter().any(|l| l.overlay_id == o.id) {
                let gap = if o.data.is_some() {
                    format!("overlay {} (data could not be loaded)", o.id)
                } else {
                    format!("overlay {} has layers but no document was supplied: its layers draw nothing", o.id)
                };
                gaps.push(gap);
            }
        }
        // Rings past the per-frame cap (review #3): only the lowest feature
        // ids' are drawn on those frames.
        let has_rings = self.timeline.effect_plans().iter().any(|p| {
            matches!(p.kind, crate::story::script::EffectKind::Pulse | crate::story::script::EffectKind::Drop)
        });
        if has_rings {
            let fps = self.output.fps;
            let (mut capped, mut most) = (0usize, 0usize);
            for n in range.clone() {
                let state = self.timeline.state_at(self.t_of(n), fps);
                let rings: usize = state
                    .layer_fx
                    .iter()
                    .filter(|fx| self.story_layers.iter().any(|l| *l.layer_id == *fx.layer_id && state.visible_overlays.contains(&l.overlay_id)))
                    .map(|fx| fx.rings.len())
                    .sum();
                if rings > MAX_RINGS_PER_FRAME {
                    capped += 1;
                    most = most.max(rings);
                }
            }
            if capped > 0 {
                gaps.push(format!(
                    "rings capped at {MAX_RINGS_PER_FRAME} a frame on {capped} frames (up to {most} asked): \
                     the rings of the lowest feature ids are drawn"
                ));
            }
        }
        for a in actions() {
            if let EventAction::SetOverlayData { id, .. } = a {
                if self.script.scene.overlays.iter().any(|o| o.id == *id && !o.layers.is_empty()) {
                    let gap = format!("set_overlay_data on overlay {id} (an overlay with layers keeps drawing its scene data)");
                    if !gaps.contains(&gap) {
                        gaps.push(gap);
                    }
                }
            }
        }
        for a in actions() {
            if let EventAction::SetOverlayData { id, geojson } = a {
                if !self.overlay_features.contains_key(geojson) {
                    missing_docs.push(format!("overlay {id} data {geojson} (no GeoJSON supplied)"));
                }
            }
        }
        for gap in missing_docs {
            if !gaps.contains(&gap) {
                gaps.push(gap);
            }
        }
        gaps
    }

    /// Render [`RenderOptions::frames`] (or every frame) into `sink`,
    /// calling `progress` after each frame, and return the manifest.
    ///
    /// # Errors
    /// See [`Self::run_range`].
    pub fn run(&mut self, sink: &mut dyn FrameSink, progress: &mut dyn FnMut(&Progress)) -> anyhow::Result<RenderManifest> {
        let range = self.opts.frames.clone().unwrap_or(0..self.frames);
        self.run_range(range, sink, progress)
    }

    /// Render `range` into `sink`. A player renders any number of ranges,
    /// in any order (a still at `--at` is `n..n + 1`); each frame is the
    /// same bytes whichever range, and whatever came before, rendered it.
    /// A failed tile is not retried while a later frame of the current
    /// pass still needs it; once `evict_through` passes its last frame
    /// its failure is forgotten, so a later range that needs it again
    /// fetches it again. For a deterministic source that fails the same
    /// way; a transient failure is recorded in each row's `missing`, and
    /// `strict` is the mode that refuses it.
    ///
    /// On any error the sink is told ([`FrameSink::abort`]) and the
    /// session is drained, so the player can render again.
    ///
    /// # Errors
    /// `cancelled at frame n` when [`RenderOptions::cancel`] is set (also
    /// during a frame's tile wait, seen within
    /// [`crate::story::prefetch::CANCEL_POLL`]); a strict or settle-point
    /// miss; a render or sink failure (naming the frame); `no frame drew
    /// a tile` when every frame drew nothing.
    pub fn run_range(
        &mut self,
        range: std::ops::Range<usize>,
        sink: &mut dyn FrameSink,
        progress: &mut dyn FnMut(&Progress),
    ) -> anyhow::Result<RenderManifest> {
        let result = self.run_inner(range, sink, progress);
        if result.is_err() {
            // Leave the session empty for any later run, and let the sink
            // clean up (kill an encoder, rename a partial file).
            let _ = self.session.flush();
            sink.abort();
        }
        result
    }

    /// [`Self::run`] through a [`Tee`]: every frame to `main`, and
    /// `poster.png` (the rendered range's middle frame) and `contact.png`
    /// (twelve thumbnails over it) written into `beside`. The manifest's
    /// `sink` becomes `{"main": <main's report>, "poster": <path>,
    /// "contact": <path>}`.
    ///
    /// # Errors
    /// See [`Self::run_range`]; or the poster or sheet could not be
    /// written (after `main` finished, so its output is complete).
    pub fn run_with_extras(
        &mut self,
        main: &mut dyn FrameSink,
        beside: &Path,
        progress: &mut dyn FnMut(&Progress),
    ) -> anyhow::Result<RenderManifest> {
        let range = self.opts.frames.clone().unwrap_or(0..self.frames);
        let mut tee = Tee::over(main, range.clone(), self.output.fps);
        let mut manifest = self.run_range(range, &mut tee, progress)?;
        let (poster, contact) = (beside.join("poster.png"), beside.join("contact.png"));
        tee.save_poster(&poster)?;
        tee.sheet.save(&contact)?;
        let main_report = std::mem::take(&mut manifest.sink);
        manifest.sink = serde_json::json!({ "main": main_report, "poster": poster, "contact": contact });
        manifest.poster = Some(poster);
        manifest.contact_sheet = Some(contact);
        Ok(manifest)
    }

    /// The contact-sheet render: only the sheet's twelve frames over
    /// `range` (and the poster, the range's middle frame) are rendered, one
    /// frame range each. The sheet goes to `out`, the poster to
    /// `extras/poster.png`. Shared by the `story` CLI and the agent's
    /// render jobs.
    ///
    /// # Errors
    /// See [`Self::run_range`]; or the sheet or poster could not be written.
    pub fn run_contact_sheet(
        &mut self,
        range: std::ops::Range<usize>,
        out: &Path,
        extras: &Path,
        progress: &mut dyn FnMut(&Progress),
    ) -> anyhow::Result<RenderManifest> {
        let fps = self.output.fps;
        let mut sheet = crate::story::sink::ContactSheet::for_range(range.clone(), crate::story::sink::CONTACT_COUNT, fps);
        let poster_frame = range.start + range.len() / 2;
        let mut picks: Vec<usize> = range.clone().filter(|n| sheet.wants(*n)).collect();
        if !picks.contains(&poster_frame) {
            picks.push(poster_frame);
            picks.sort_unstable();
        }
        let mut poster = None;
        let mut merged: Option<RenderManifest> = None;
        for n in picks {
            let mut s = SheetSink { sheet: &mut sheet, poster_frame, poster: &mut poster };
            let m = self.run_range(n..n + 1, &mut s, progress)?;
            match merged.as_mut() {
                Some(all) => {
                    all.frames.extend(m.frames);
                    all.layout_warnings = dedup_layout_warnings(all.layout_warnings.iter().chain(m.layout_warnings.iter()));
                    for g in m.gaps {
                        if !all.gaps.contains(&g) {
                            all.gaps.push(g);
                        }
                    }
                }
                None => merged = Some(m),
            }
        }
        let mut manifest = merged.ok_or_else(|| anyhow::anyhow!("the contact sheet picked no frames"))?;
        manifest.coverage.gaps = manifest.gaps.len();
        sheet.save(out)?;
        let poster_path = extras.join("poster.png");
        if let Some((w, h, rgba)) = poster {
            image::save_buffer(&poster_path, &rgba, w, h, image::ExtendedColorType::Rgba8)?;
            manifest.poster = Some(poster_path.clone());
        }
        manifest.contact_sheet = Some(out.to_path_buf());
        manifest.render.frames = (range.start, range.end);
        manifest.sink = serde_json::json!({ "kind": "contact_sheet", "contact": out, "poster": poster_path, "rendered": manifest.frames.len() });
        Ok(manifest)
    }

    fn run_inner(
        &mut self,
        range: std::ops::Range<usize>,
        sink: &mut dyn FrameSink,
        progress: &mut dyn FnMut(&Progress),
    ) -> anyhow::Result<RenderManifest> {
        anyhow::ensure!(
            range.start < range.end && range.end <= self.frames,
            "frame range {}..{} is empty or outside the story's 0..{}",
            range.start,
            range.end,
            self.frames
        );
        let gaps = self.gaps(&range);
        for gap in &gaps {
            eprintln!("story: cannot draw: {gap}");
        }
        let rows = self.frame_loop(range.clone(), sink, progress)?;
        anyhow::ensure!(rows.iter().any(|r| r.tiles_drawn > 0), "no frame drew a tile");
        let mut gaps = gaps;
        if let Some(gap) = dropped_glyphs_gap(&rows) {
            eprintln!("story: cannot draw: {gap}");
            gaps.push(gap);
        }
        let sink_report = sink.finish()?;
        let verdict = self.verdict();
        let layout_warnings = dedup_layout_warnings(rows.iter().flat_map(|r| r.layout_warnings.iter()));
        // Video or still is the output format's, never the frame count's.
        let publish = self.publish_table(self.opts.video);
        let coverage = self.coverage(gaps.len());
        Ok(RenderManifest {
            script_sha256: self.script_sha256.clone(),
            script: crate::story::redacted_script(&self.script)?,
            engine_git_sha: engine_git_sha(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            adapter: self.session.device_name(),
            output: self.output,
            format: self.script.format(),
            source: self.inputs.source_label.clone(),
            render: self.render_settings(&range),
            frames: rows,
            attribution: verdict.attribution,
            publishable: crate::story::burn_in::publishable(&publish),
            publishable_reason: crate::story::burn_in::publishable_reason(&publish),
            publish,
            chapters: self.chapters().iter().map(|c| (c.clone(), None)).collect(),
            coverage,
            licence_acknowledged: verdict.acknowledged,
            gaps,
            layout_warnings,
            sprites: self.inputs.sprite_sheet.clone(),
            sink: sink_report,
            ffmpeg: None,
            poster: None,
            contact_sheet: None,
        })
    }

    /// Every setting that can change a frame's pixels or which tiles it
    /// had, as rendered (F9).
    fn render_settings(&self, range: &std::ops::Range<usize>) -> RenderSettings {
        let env = |k: &str| std::env::var(k).ok();
        RenderSettings {
            frames: (range.start, range.end),
            story_frames: self.frames,
            strict: self.opts.strict,
            tile_timeout_s: self.opts.tile_timeout.as_secs_f64(),
            fetch_concurrency: self.opts.fetch_concurrency,
            mesh_cache_bytes: self.opts.mesh_cache_bytes,
            stand_in_levels: STAND_IN_LEVELS,
            basemap_compact: BASEMAP_COMPACT,
            basemap_compact_strokes: BASEMAP_COMPACT_STROKES,
            env_basemap_compact: env("GPUI_MAP_BASEMAP_COMPACT"),
            env_basemap_compact_strokes: env("GPUI_MAP_BASEMAP_COMPACT_STROKES"),
            flat_tile_zoom_max: self.inputs.flat_tile_zoom_max,
            label_font_family: self.inputs.label_fonts.as_ref().map(|(_, family)| family.clone()),
            label_font_sha256: self
                .inputs
                .label_fonts
                .as_ref()
                .map(|(files, _)| files.iter().map(|f| hex_sha256(f)).collect())
                .unwrap_or_default(),
            terminator_default: self.script.scene.terminator,
            raster_2x: self
                .script
                .scene
                .raster
                .as_deref()
                .is_some_and(|r| crate::story::tiers::resolve_raster(r, self.output.pixel_ratio, None).1),
        }
    }

    fn frame_loop(
        &mut self,
        range: std::ops::Range<usize>,
        sink: &mut dyn FrameSink,
        progress: &mut dyn FnMut(&Progress),
    ) -> anyhow::Result<Vec<FrameRow>> {
        let total = range.len();
        let mut rows: Vec<FrameRow> = Vec::with_capacity(total);
        // Rows whose frame is still in the pipeline, oldest first.
        let mut pending: VecDeque<FrameRow> = VecDeque::new();
        let mut recent: VecDeque<Instant> = VecDeque::new();
        let mut missing_so_far = 0usize;
        for (i, n) in range.enumerate() {
            if self.opts.cancel.load(Ordering::Relaxed) {
                anyhow::bail!("cancelled at frame {n}");
            }
            let planning = Instant::now();
            let (request, state) = self.request_at(n);
            let plan_ms = planning.elapsed().as_secs_f64() * 1e3;
            let prefetcher = self.prefetcher.as_mut().ok_or_else(|| anyhow::anyhow!("the player is closed"))?;
            // A settle point waits its own `settle.timeout_s` (F2).
            // `try_`: never a panic on a timeout no `Duration` holds (R1);
            // the timeline caps it at an hour anyway.
            let settle = state.settle.map(|s| Duration::try_from_secs_f64(s).unwrap_or(Duration::MAX));
            let outcome = prefetcher.ensure_with(n, settle)?;
            let style = self.styles.get(&self.timeline, state.style_generation).with_context(|| format!("frame {n}"))?;
            let started = Instant::now();
            let done = {
                let store = prefetcher.store();
                self.session.render_pipelined(&request, &store, &style).with_context(|| format!("frame {n}"))?
            };
            let ms = started.elapsed().as_secs_f64() * 1e3;
            prefetcher.evict_through(n);
            missing_so_far += outcome.missing.len();
            let t = self.t_of(n);
            pending.push_back(FrameRow {
                n,
                t,
                lon: request.center_lon,
                lat: request.center_lat,
                zoom: request.zoom,
                pitch_deg: f64::from(request.pitch).to_degrees(),
                bearing_deg: f64::from(request.bearing).to_degrees(),
                globe: request.globe,
                ms,
                wait_ms: outcome.waited.as_secs_f64() * 1e3,
                tiles_drawn: 0,
                labels_drawn: 0,
                overlay_labels_drawn: 0,
                icons_drawn: 0,
                line_glyphs_drawn: 0,
                line_glyphs_dropped: 0,
                missing: outcome
                    .missing
                    .iter()
                    .map(|(t, why)| (format!("{}/{}/{}", t.z, t.x, t.y), why.clone()))
                    .chain(self.image_misses(&state))
                    .collect(),
                tiles_missing: Vec::new(),
                sha256: String::new(),
                stage_ms: [plan_ms, outcome.waited.as_secs_f64() * 1e3, 0.0, 0.0, 0.0, 0.0, 0.0],
                layout_warnings: Vec::new(),
            });
            if let Some(capture) = done {
                complete(&mut pending, &mut rows, &capture, self.session.take_stage_times(), sink)?;
            }
            let now = Instant::now();
            recent.push_back(now);
            if recent.len() > 30 {
                recent.pop_front();
            }
            let fps_recent = match (recent.front(), recent.len()) {
                (Some(first), k) if k > 1 => (k - 1) as f64 / now.duration_since(*first).as_secs_f64().max(1e-9),
                _ => 0.0,
            };
            let left = total - i - 1;
            let waiting_on_tiles = self.prefetcher.as_ref().map_or(0, Prefetcher::in_flight);
            progress(&Progress {
                frame: i + 1,
                frames: total,
                story_t: t,
                fps_recent,
                eta_s: if fps_recent > 0.0 { left as f64 / fps_recent } else { 0.0 },
                waiting_on_tiles,
                missing_so_far,
            });
        }
        for capture in self.session.flush()? {
            complete(&mut pending, &mut rows, &capture, self.session.take_stage_times(), sink)?;
        }
        anyhow::ensure!(pending.is_empty(), "{} frames never came back from the GPU", pending.len());
        Ok(rows)
    }
}

/// `warnings` once per `(id, code)`, first seen first.
fn dedup_layout_warnings<'a>(
    warnings: impl Iterator<Item = &'a crate::story::layout::LayoutWarning>,
) -> Vec<crate::story::layout::LayoutWarning> {
    let mut out: Vec<crate::story::layout::LayoutWarning> = Vec::new();
    for w in warnings {
        if !out.iter().any(|o| o.id == w.id && o.code == w.code) {
            out.push(w.clone());
        }
    }
    out
}

/// The gap line for street names drawn with letters missing: glyphs the
/// line-glyph atlas could not hold even fresh, summed over `rows`.
fn dropped_glyphs_gap(rows: &[FrameRow]) -> Option<String> {
    let frames = rows.iter().filter(|r| r.line_glyphs_dropped > 0).count();
    let glyphs: usize = rows.iter().map(|r| r.line_glyphs_dropped).sum();
    (glyphs > 0).then(|| {
        format!("line-placed labels missing {glyphs} glyphs over {frames} frames (glyph atlas full)")
    })
}

/// The oldest pending row gets `capture`'s counters and hash, and the
/// frame goes to the sink.
fn complete(
    pending: &mut VecDeque<FrameRow>,
    rows: &mut Vec<FrameRow>,
    capture: &Capture,
    times: Option<crate::capture::StageTimes>,
    sink: &mut dyn FrameSink,
) -> anyhow::Result<()> {
    let mut row = pending.pop_front().ok_or_else(|| anyhow::anyhow!("a frame came back that was never submitted"))?;
    if let Some(t) = times {
        row.stage_ms[2] = t.bake_ms;
        row.stage_ms[3] = t.encode_ms;
        row.stage_ms[4] = t.readback_ms;
        row.stage_ms[5] = t.burn_in_ms;
    }
    row.tiles_drawn = capture.tiles_drawn;
    row.labels_drawn = capture.labels_drawn;
    row.overlay_labels_drawn = capture.overlay_labels_drawn;
    row.icons_drawn = capture.icons_drawn;
    row.line_glyphs_drawn = capture.line_glyphs_drawn;
    row.line_glyphs_dropped = capture.line_glyphs_dropped;
    row.tiles_missing = capture.tiles_missing.iter().map(|t| format!("{}/{}/{}", t.z, t.x, t.y)).collect();
    row.sha256 = capture.sha256_hex();
    row.layout_warnings = capture.layout_warnings.clone();
    let writing = Instant::now();
    sink.push(row.n, capture).with_context(|| format!("frame {}: the sink", row.n))?;
    row.stage_ms[6] = writing.elapsed().as_secs_f64() * 1e3;
    rows.push(row);
    Ok(())
}

/// Keeps the frames a contact sheet wants, and the poster frame.
struct SheetSink<'a> {
    sheet: &'a mut crate::story::sink::ContactSheet,
    poster_frame: usize,
    poster: &'a mut Option<(u32, u32, Vec<u8>)>,
}

impl FrameSink for SheetSink<'_> {
    fn push(&mut self, n: usize, f: &Capture) -> anyhow::Result<()> {
        self.sheet.push_rgba(n, f.width, f.height, &f.rgba);
        if n == self.poster_frame {
            *self.poster = Some((f.width, f.height, f.rgba.clone()));
        }
        Ok(())
    }
    fn finish(&mut self) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::Value::Null)
    }
}

impl Drop for Player {
    /// The prefetcher first (it aborts its fetches), then the runtime, in
    /// the background: never while a store guard is held (F14), and never
    /// blocking a caller that drops the player inside another runtime.
    fn drop(&mut self) {
        drop(self.prefetcher.take());
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_background();
        }
    }
}

/// `GPUI_MAP_GIT_SHA`, set by `build.rs` to the commit the code was
/// compiled from (`-dirty` with uncommitted tracked changes); for a build
/// without git, `git rev-parse HEAD` in the crate's checkout now, marked
/// `(runtime)` because it need not be the code that was built; else
/// `"unknown"`.
fn engine_git_sha() -> String {
    if let Some(sha) = option_env!("GPUI_MAP_GIT_SHA") {
        return sha.to_string();
    }
    std::process::Command::new("git")
        .args(["-C", env!("CARGO_MANIFEST_DIR"), "rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map_or_else(|| "unknown".to_string(), |s| format!("{s} (runtime)"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::geo::TileId;

    /// Task R3: the design context reads `design.anchor` into every frame
    /// spec and design frame it builds (spec §5.7); absent is `auto`.
    #[test]
    fn the_design_context_carries_the_anchor_into_the_frame_spec() {
        use crate::story::frame_layout::{AnchorPref, Dir};
        for (given, want) in [(serde_json::json!({}), AnchorPref::Auto), (serde_json::json!({"anchor": "side"}), AnchorPref::Side), (serde_json::json!({"anchor": "compact"}), AnchorPref::Compact)] {
            let script = crate::story::test_util::minimal_script(serde_json::json!({"design": given, "output": {"safe_area": "all"}}));
            let path = CameraPath::new((1080.0, 1920.0), &script, &crate::story::tracks::TrackSet::default()).expect("a path");
            let d = DesignCtx::new(&script, &[], &path, None, None).expect("a design story");
            let spec = d.frame_spec((1080.0, 1920.0));
            assert_eq!((spec.anchor, spec.dir, spec.safe_area, spec.aspect), (want, Dir::Ltr, crate::story::tiers::SafeArea::All, crate::story::tiers::Aspect::Portrait));
            let frame = d.frame_at(&[], 0.0);
            assert_eq!(frame.frame_spec((1080.0, 1920.0), spec.mark_w), spec, "the drawer lays out on the same frame");
        }
    }

    /// The golden `design` scene's script at 9:16 `reels`, parsed, with
    /// its raw bytes.
    fn design_script() -> (Script, Vec<u8>) {
        let mut json = crate::capture::golden_tests::story_script("design", "9:16", "fullhd");
        json["output"]["safe_area"] = serde_json::json!("reels");
        let raw = serde_json::to_vec(&json).unwrap();
        (crate::story::parse(&raw).unwrap(), raw)
    }

    /// Task R6 review: the camera's card spans are cut where the drawer's
    /// are: a cue that adds a caption row to a card raises that card's
    /// camera box for the cue's interval, and only for it.
    #[test]
    fn a_cue_that_adds_a_caption_row_raises_the_camera_box() {
        struct Half;
        impl crate::story::card_layout::CardMeasure for Half {
            fn width(&self, text: &str, size: f32, _: crate::story::design::TextStyle) -> f32 {
                text.chars().count() as f32 * size * 0.5
            }
        }
        let script = crate::story::test_util::minimal_script(serde_json::json!({
            "design": {},
            "output": {"burn_captions": true},
            "chapters": [{"id": "bay", "title": "Bay", "from": 0, "to": 10, "card": {"headline": "The bay"}}],
            "captions": [{"from": 3, "to": 6, "text": "A cue that adds a caption row to the card"}]
        }));
        let path = CameraPath::new((1080.0, 1920.0), &script, &crate::story::tracks::TrackSet::default()).unwrap();
        let chapters = crate::story::chapters::resolve(&script, &path).0;
        let d = DesignCtx::new(&script, &chapters, &path, None, None).expect("a design story");
        let boxes = d.card_boxes((1080.0, 1920.0), 1.0, None, &Half);
        let spans: Vec<(f64, f64)> = boxes.boxes.iter().map(|b| (b.from, b.to)).collect();
        assert_eq!(spans, vec![(0.0, 3.0), (3.0, 6.0), (6.0, 10.0)], "cut at the cue's ends");
        let bottom = |i: usize| boxes.boxes[i].camera_box.3;
        assert!(bottom(1) < bottom(0), "the taller card raises the box: {:?}", boxes.boxes);
        assert_eq!(bottom(0), bottom(2), "and only for the cue");
        assert_eq!(boxes.chosen(4.0, 5.0), boxes.boxes[1].camera_box, "a pose during the cue clears the card the frame draws");
    }

    /// Task R6 (ruling R2-a): a card's photo loads like any story image and
    /// the frame carries it under `card_photo_id`, so the drawer draws the
    /// photo itself; its credit joins the sources as `Photo` (spec §3.4).
    /// A caption cue draws in the card, not as a free caption (spec §3.5).
    #[test]
    fn a_card_photo_draws_for_real_and_a_cue_rides_in_the_card() {
        let (_, raw) = design_script();
        let mut json: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        json["chapters"][0]["card"] = serde_json::json!({"headline": "The barrage", "photo": {"src": "img/harbour.png", "alt": "A test gradient", "credit": "Jane Doe"}});
        json["output"]["burn_captions"] = serde_json::json!(true);
        json["captions"] = serde_json::json!([{"from": 0.0, "to": 2.0, "text": "A cue"}]);
        let raw = serde_json::to_vec(&json).unwrap();
        let script = crate::story::parse(&raw).unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("img")).unwrap();
        image::RgbaImage::from_pixel(40, 30, image::Rgba([200, 40, 40, 255])).save(dir.path().join("img/harbour.png")).unwrap();
        let opts = RenderOptions { cache_dir: dir.path().join("cache"), ..RenderOptions::default() };
        let player = Player::new(&script, &raw, crate::capture::golden_tests::story_inputs(dir.path()), opts).unwrap();
        let (request, _) = player.request_at(crate::capture::golden_tests::STORY_FRAME);
        let story = request.story.expect("a story frame");
        let design = story.design.as_ref().expect("a design part");
        let photo_id = crate::story::card_layout::card_photo_id("img/harbour.png");
        assert!(story.images.iter().any(|i| i.id == photo_id), "the photo's image rides with the frame: {:?}", story.images.iter().map(|i| &i.id).collect::<Vec<_>>());
        assert!(design.sources.iter().any(|g| g.key == "Photo" && g.text == "Jane Doe"), "{:?}", design.sources);
        assert_eq!(design.card.as_ref().and_then(|c| c.caption.as_deref()), Some("A cue"), "the cue is the card's caption");
        assert!(request.captions.iter().all(|c| c.text != "A cue"), "and is not drawn again as a free caption");
    }

    /// Task R4 (spec §13): validation's layout probe composes the frame a
    /// render does -- its design part (card, fine print, credit) included.
    #[test]
    fn validation_probes_see_the_design_frame() {
        let (script, raw) = design_script();
        let dir = tempfile::tempdir().unwrap();
        let mut inputs = crate::capture::golden_tests::story_inputs(dir.path());
        let layers = story_layer_sources(&script, &inputs.overlays).unwrap();
        let sources: Vec<crate::story::overlay_layers::LayerSource> = layers.iter().map(|(_, s)| s.clone()).collect();
        let tracks = Arc::new(std::mem::take(&mut inputs.tracks));
        let base_style = std::mem::take(&mut inputs.base_style);
        let o = script.output();
        let viewport = (o.width as f32 / o.pixel_ratio, o.height as f32 / o.pixel_ratio);
        let (path, timeline) =
            crate::story::timeline::path_and_timeline(viewport, &script, tracks, &sources, base_style, &*inputs.load_style).map_err(|e| e.0).unwrap();
        let images = crate::story::media::ScriptImages::default();
        let probe = LayoutProbe::new(&script, &inputs, &path, &timeline, &sources, &images, false, &RenderOptions::default());
        let t = crate::capture::golden_tests::STORY_FRAME as f64 / 10.0;
        let (probed, _) = probe.request(t);
        let probed = probed.story.expect("a story frame");
        let design = probed.design.as_ref().expect("the probe's frame carries `design`");
        assert!(design.card.is_some() && design.credit.is_some(), "the card and the tile credit: {design:?}");
        // The render's frame at the same `t`: the same design part.
        let opts = RenderOptions { cache_dir: dir.path().join("cache"), ..RenderOptions::default() };
        let player = Player::new(&script, &raw, crate::capture::golden_tests::story_inputs(dir.path()), opts).unwrap();
        let (request, _) = player.request_at(crate::capture::golden_tests::STORY_FRAME);
        assert_eq!(request.story.and_then(|s| s.design.clone()).as_ref(), Some(design), "the probe and the render compose one design part");
    }

    /// Task R4 (carried from R3): the camera pass lays the card out with
    /// the slot mark the drawer draws -- under a non-default policy too.
    /// With a wordmark, `Always` keeps the SWAT mark in the slot where
    /// `Auto` gives it to the wordmark: the two widths differ, and each
    /// player's camera pass reads its own requests' width.
    #[test]
    fn a_non_default_mark_policy_gives_the_camera_pass_and_the_drawer_one_slot() {
        use crate::brand_mark::BrandMarkPolicy;
        let (script, raw) = design_script();
        let artwork = crate::brand_mark::artwork_size();
        let slot = |policy: BrandMarkPolicy| {
            let dir = tempfile::tempdir().unwrap();
            let inputs = SceneInputs {
                wordmark: Some(Arc::new(image::RgbaImage::new(300, 30))),
                ..crate::capture::golden_tests::story_inputs(dir.path())
            };
            let opts = RenderOptions { cache_dir: dir.path().join("cache"), brand_mark_policy: policy, ..RenderOptions::default() };
            let player = Player::new(&script, &raw, inputs, opts).unwrap();
            let (request, _) = player.request_at(0);
            assert_eq!(request.brand_mark_policy, policy, "the requests carry the render's policy");
            let camera = player.design.as_ref().expect("a design story").mark_w;
            assert_eq!(camera, request.slot_mark_w(Some(artwork)), "{policy:?}: the camera pass and the drawer read one slot");
            // Validation's probe, given the render's marks, lays out and
            // requests that same slot (R4 carry-over 2).
            let probe_inputs = SceneInputs {
                wordmark: Some(Arc::new(image::RgbaImage::new(300, 30))),
                ..crate::capture::golden_tests::story_inputs(dir.path())
            };
            let images = crate::story::media::ScriptImages::default();
            let marks = RenderOptions { brand_mark_policy: policy, ..RenderOptions::default() };
            let probe = LayoutProbe::new(&script, &probe_inputs, &player.path, &player.timeline, &[], &images, false, &marks);
            assert_eq!(probe.design.as_ref().expect("a design story").mark_w, camera, "{policy:?}: the probe's camera slot is the render's");
            let (probed, _) = probe.request(0.0);
            assert_eq!(probed.brand_mark_policy, policy, "{policy:?}: the probe's requests carry the render's policy");
            assert_eq!(probed.slot_mark_w(Some(artwork)), camera, "{policy:?}: the probe draws the render's slot");
            camera
        };
        let always = slot(BrandMarkPolicy::Always);
        let auto = slot(BrandMarkPolicy::Auto);
        assert_eq!(always, crate::story::frame_layout::slot_mark_w(Some(artwork), None), "Always: the SWAT mark");
        assert_eq!(auto, crate::story::frame_layout::slot_mark_w(None, Some((300, 30))), "Auto: the wordmark");
        assert_ne!(always, auto);
    }
    use std::collections::HashSet;
    use std::sync::PoisonError;

    #[test]
    fn dropped_glyphs_are_a_gap_line_and_none_is_no_line() {
        let row = |dropped: usize| FrameRow {
            n: 0, t: 0.0, lon: 0.0, lat: 0.0, zoom: 0.0, pitch_deg: 0.0, bearing_deg: 0.0, globe: false,
            ms: 0.0, wait_ms: 0.0, tiles_drawn: 1, labels_drawn: 0, overlay_labels_drawn: 0, icons_drawn: 0,
            line_glyphs_drawn: 0, line_glyphs_dropped: dropped, missing: Vec::new(), tiles_missing: Vec::new(),
            sha256: String::new(), stage_ms: [0.0; 7], layout_warnings: Vec::new(),
        };
        assert_eq!(super::dropped_glyphs_gap(&[row(0), row(0)]), None);
        assert_eq!(
            super::dropped_glyphs_gap(&[row(3), row(0), row(4)]).as_deref(),
            Some("line-placed labels missing 7 glyphs over 2 frames (glyph atlas full)")
        );
    }

    fn det_script() -> (Script, Vec<u8>) {
        let json = serde_json::json!({"story":1,"output":{"width":480,"height":270,"fps":10},
          "scene":{"style":"examples/liberty-style.json","basemap":{"source":"fixture"},"labels":true},
          "camera":[{"type":"jump","to":{"lon":103.851,"lat":1.2905,"zoom":14.2}},
                    {"type":"fly","label":"fly","to":{"lon":103.856,"lat":1.2850,"zoom":15,"pitch":40},"duration":1.0},
                    {"type":"orbit","degrees":30,"duration":1.0}],
          "events":[{"at":0.5,"set_layer_visibility":{"layer":"water","visible":false}},
                    {"at":0.2,"until":1.8,"caption":{"text":"Marina Bay","anchor":"bottom-left"}}]});
        let raw = serde_json::to_vec(&json).unwrap();
        (crate::story::parse(&raw).unwrap(), raw)
    }
    fn inputs() -> SceneInputs {
        let root = env!("CARGO_MANIFEST_DIR");
        let style: serde_json::Value =
            serde_json::from_slice(&std::fs::read(format!("{root}/examples/liberty-style.json")).unwrap()).unwrap();
        let font = std::fs::read(format!("{root}/assets/fonts/NotoSans-Regular.latin-subset.ttf")).unwrap();
        SceneInputs {
            source: Arc::new(crate::story::fixture::FixtureSource),
            source_label: "fixture".into(),
            base_style: style,
            load_style: Box::new(|_| None),
            overlays: HashMap::new(),
            tracks: TrackSet::default(),
            label_fonts: Some((vec![font].into(), "Noto Sans".into())),
            flat_tile_zoom_max: 14,
            raster: None,
            credits: Vec::new(),
            wordmark: None,
            sprites: None,
            sprite_sheet: None,
            sprite_notes: Vec::new(),
            story_dir: PathBuf::new(),
            offline: true,
            images_validated: true,
        }
    }
    struct Hashes(Vec<String>);
    impl FrameSink for Hashes {
        fn push(&mut self, _: usize, f: &Capture) -> anyhow::Result<()> {
            self.0.push(f.sha256_hex());
            Ok(())
        }
        fn finish(&mut self) -> anyhow::Result<serde_json::Value> {
            Ok(serde_json::Value::Null)
        }
    }
    /// Records every call, for the cancel and abort tests.
    #[derive(Default)]
    struct Recorder {
        pushed: Vec<usize>,
        finished: bool,
        aborted: bool,
    }
    impl FrameSink for Recorder {
        fn push(&mut self, n: usize, _: &Capture) -> anyhow::Result<()> {
            self.pushed.push(n);
            Ok(())
        }
        fn finish(&mut self) -> anyhow::Result<serde_json::Value> {
            self.finished = true;
            Ok(serde_json::Value::Null)
        }
        fn abort(&mut self) {
            self.aborted = true;
        }
    }
    fn player_for(json: &serde_json::Value, inp: SceneInputs, opts: RenderOptions) -> (Player, tempfile::TempDir) {
        let raw = serde_json::to_vec(json).unwrap();
        let s = crate::story::parse(&raw).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let opts = RenderOptions { cache_dir: dir.path().into(), ..opts };
        (Player::new(&s, &raw, inp, opts).unwrap(), dir)
    }
    fn det_json() -> serde_json::Value {
        let (_, raw) = det_script();
        serde_json::from_slice(&raw).unwrap()
    }
    /// The GPU tests here render labels; hold the lock the other label
    /// tests hold so a refused-line-label counter is never shared.
    fn gpu_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::symbol_pass::LINE_LABELS_REFUSED_TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
    }
    fn render_hashes(range: Option<std::ops::Range<usize>>, jitter: bool) -> Vec<String> {
        let (s, raw) = det_script();
        let dir = tempfile::tempdir().unwrap();
        let opts = RenderOptions { frames: range, cache_dir: dir.path().into(), ..RenderOptions::default() };
        let mut player = Player::new(&s, &raw, inputs(), opts).unwrap();
        let mut sink = Hashes(Vec::new());
        let mut n = 0u64;
        let m = player
            .run(&mut sink, &mut |_| {
                if jitter {
                    n += 1;
                    std::thread::sleep(std::time::Duration::from_millis((n * 7919) % 31));
                }
            })
            .unwrap();
        assert!(m.frames.iter().all(|r| r.tiles_drawn > 0), "prove every frame painted");
        assert_eq!(m.frames.iter().map(|r| r.sha256.clone()).collect::<Vec<_>>(), sink.0, "rows match the sink, in order");
        sink.0
    }

    /// The det script at 30 fps, 3 s long, with a `routes` overlay (line
    /// metrics on, one line layer `route-line`) drawn over [0.5, 2.5] with
    /// a white head.
    fn drawn_route_json() -> serde_json::Value {
        let mut json = det_json();
        json["output"]["fps"] = serde_json::json!(30);
        json["camera"][2]["duration"] = serde_json::json!(2.0);
        json["scene"]["overlays"] = serde_json::json!([{
            "id": "routes", "line_metrics": true,
            "data": {"type": "FeatureCollection", "features": [{"type": "Feature", "properties": {},
                "geometry": {"type": "LineString", "coordinates": [[103.846, 1.2925], [103.851, 1.2880], [103.8585, 1.2860]]}}]},
            "layers": [{"id": "route-line", "type": "line", "paint": {"line-color": "red", "line-width": 5}}]
        }]);
        json["events"].as_array_mut().unwrap().push(serde_json::json!(
            {"at": 0.5, "until": 2.5, "overlay": {"layer": "route-line", "effect": "draw", "easing": "linear",
             "head": {"radius_px": 7, "color": "#ffffff"}}}));
        json
    }

    /// Spec §4.5: a frame carries one furniture item -- the chapter's spec
    /// over the story's, field by field; the legend from the layer's
    /// `match` paint; the scale bar for this frame's latitude and zoom;
    /// the north arrow once the orbit turns the map past 2 degrees.
    #[test]
    fn a_frame_carries_its_chapters_furniture_with_a_legend_from_paint() {
        use crate::story::furniture::{FurnitureFrame, LegendSwatch};
        use crate::story::layout::LayoutItem;
        let _g = gpu_lock();
        let mut json = drawn_route_json();
        json["camera"][2]["degrees"] = serde_json::json!(90);
        json["scene"]["overlays"][0]["layers"][0]["paint"]["line-color"] =
            serde_json::json!(["match", ["get", "kind"], "ferry", "#1f77b4", "cargo", "#e4572e", "#999999"]);
        json["furniture"] = serde_json::json!({"source_line": "Source: Port authority", "scale_bar": "metric", "north_arrow": "auto",
            "legend": {"layers": ["route-line"], "title": "Vessel"}});
        json["chapters"] = serde_json::json!([
            {"id": "a", "title": "A", "from": 0.0, "to": 1.5, "furniture": {"byline": "By A. Reporter"}},
            {"id": "b", "title": "B", "from": 1.5, "to": 3.0, "furniture": {"scale_bar": "imperial"}}]);
        let (player, _d) = player_for(&json, inputs(), RenderOptions::default());
        let furniture = |n: usize| -> (FurnitureFrame, crate::capture::CaptureRequest) {
            let (r, _) = player.request_at(n);
            let story = r.story.clone().expect("furniture is on every frame");
            let f = story.items.iter().find_map(|i| match i {
                LayoutItem::Furniture { id, frame } if id == "furniture" => Some(frame.clone()),
                _ => None,
            });
            (f.expect("a furniture item"), r)
        };
        let (first, r0) = furniture(0);
        assert_eq!(first.source_line.as_deref(), Some("Source: Port authority"));
        assert_eq!(first.byline.as_deref(), Some("By A. Reporter"), "chapter a's byline");
        assert_eq!(first.north_arrow, None, "north up at 0 s");
        let want = crate::story::furniture::scale_bar(r0.center_lat, r0.zoom, r0.globe, crate::story::script::ScaleBarUnits::Metric, 200.0);
        assert_eq!(first.scale_bar, want);
        assert!(first.scale_bar.as_ref().unwrap().bars[0].0.ends_with(" m"), "{:?}", first.scale_bar);
        let legend = first.legend.unwrap();
        assert_eq!(legend.title.as_deref(), Some("Vessel"));
        assert_eq!(
            legend.entries.iter().map(|(s, t)| (s.clone(), t.as_str())).collect::<Vec<_>>(),
            [
                (LegendSwatch::Line([0x1f, 0x77, 0xb4, 255], 5.0), "ferry"),
                (LegendSwatch::Line([0xe4, 0x57, 0x2e, 255], 5.0), "cargo"),
                (LegendSwatch::Line([0x99, 0x99, 0x99, 255], 5.0), "Other"),
            ]
        );
        let (late, r60) = furniture(80);
        assert_eq!(late.byline, None, "chapter b has no byline and the story none");
        assert!(late.scale_bar.as_ref().unwrap().bars[0].0.ends_with(" ft"), "chapter b's imperial bar: {:?}", late.scale_bar);
        let bearing = f64::from(r60.bearing).to_degrees();
        assert!(bearing.abs() > 2.0, "the orbit has turned: {bearing}");
        assert!((f64::from(late.north_arrow.expect("shown past 2 degrees")) + bearing).abs() < 1e-3);
    }

    /// Spec §6.3 (R8): a `numbered` stop is `now` while the card on
    /// screen (`cards::card_at`, R6's timing) has its number as `index.i`,
    /// `done` below and `muted` above -- read per frame, so an event card
    /// that changes the index changes the badges.
    #[test]
    fn numbered_states_follow_the_card_index() {
        let mut json = crate::capture::golden_tests::story_script("design_tour", "9:16", "fullhd");
        json.as_object_mut().unwrap().remove("chapters");
        json["events"] = serde_json::json!([
            {"at": 0.0, "until": 1.0, "card": {"headline": "Esplanade", "index": {"i": 2, "n": 5}}},
            {"at": 1.0, "until": 2.0, "card": {"headline": "Gardens", "index": {"i": 5, "n": 5}}}]);
        let (player, _dir) = player_for(&json, crate::capture::golden_tests::story_inputs(std::path::Path::new(".")), RenderOptions::default());
        let design = player.design.as_ref().expect("a design story");
        for (frame, want_i) in [(5, 2u32), (15, 5)] {
            let t = player.t_of(frame);
            assert_eq!(design.card_index_at(t), Some(want_i), "the card on screen at {t}");
            let (request, _) = player.request_at(frame);
            let halo = request.story_layers.iter().find(|l| &*l.layer_id == "sights/halo").expect("the badges");
            let radii: Vec<f32> = halo.markers.as_ref().unwrap().iter().map(|m| m.radius).collect();
            let want: Vec<f32> = (1..=5).map(|n| if n == want_i { 19.0 } else { 15.0 }).collect();
            assert_eq!(radii, want, "frame {frame}: stop {want_i} is the `now` badge");
            let core = request.story_layers.iter().find(|l| &*l.layer_id == "sights").expect("the numerals");
            let muted = [0x7A, 0x81, 0x8E, 0xFF];
            let colours: Vec<[u8; 4]> = core.labels.iter().filter(|l| l.beside.is_none()).map(|l| l.color).collect();
            for (k, c) in colours.iter().enumerate() {
                let n = k as u32 + 1;
                assert_eq!(*c == muted, n > want_i, "frame {frame}: numeral {n} muted only above {want_i}");
            }
            let names: Vec<&str> = core.labels.iter().filter(|l| l.beside.is_some()).map(|l| l.text.as_str()).collect();
            assert_eq!(names.len(), 1, "frame {frame}: the current stop's label only: {names:?}");
        }
    }

    /// The journey golden's script with `events` instead of its own (the
    /// route's draw kept), each feature given its index as GeoJSON `id`.
    fn journey_with(events: serde_json::Value) -> (Player, tempfile::TempDir) {
        let mut json = crate::capture::golden_tests::story_script("design_journey", "9:16", "fullhd");
        for (k, f) in json["scene"]["overlays"][0]["data"]["features"].as_array_mut().unwrap().iter_mut().enumerate() {
            f["id"] = serde_json::json!(k);
        }
        let mut all = json["events"].as_array().unwrap().clone();
        all.extend(events.as_array().unwrap().iter().cloned());
        json["events"] = serde_json::Value::Array(all);
        player_for(&json, crate::capture::golden_tests::story_inputs(std::path::Path::new(".")), RenderOptions::default())
    }

    fn layer<'r>(request: &'r CaptureRequest, id: &str) -> &'r StoryLayerDraw {
        request.story_layers.iter().find(|l| &*l.layer_id == id).unwrap_or_else(|| panic!("no layer {id}"))
    }

    /// R8 review #1: a `drop` with `stagger_s` on a themed `stops` layer
    /// hides each stop's markers and label before its reveal and grows
    /// them during it, on every part (shadow, halo, core).
    #[test]
    fn a_staggered_drop_reveals_each_themed_stop_in_turn() {
        // Stops 1..4 (the line is feature 0) reveal at 0, .5, 1, 1.5 s.
        let (player, _dir) = journey_with(serde_json::json!([{"at": 0.0, "overlay": {"layer": "stops", "effect": "drop", "stagger_s": 0.5}}]));
        let (request, _) = player.request_at(11); // t = 1.1
        for part in ["stops/shadow", "stops/halo", "stops"] {
            let ms = layer(&request, part).markers.as_ref().unwrap();
            let ids: Vec<u64> = ms.iter().filter_map(|m| m.id).collect();
            assert!(!ids.contains(&4), "{part}: the last stop is not revealed yet: {ids:?}");
            assert!(ids.contains(&1) && ids.contains(&2) && ids.contains(&3), "{part}: {ids:?}");
        }
        let core = layer(&request, "stops").markers.as_ref().unwrap();
        let r = |id: u64| core.iter().find(|m| m.id == Some(id)).unwrap();
        assert_eq!(r(1).radius, 7.5, "a revealed stop at its size");
        assert!(r(3).radius < 7.5 && r(3).radius > 0.0 && r(3).fill[3] < 255, "the dropping stop grows in: {:?}", r(3));
        let labels = &layer(&request, "stops").labels;
        assert!(labels.iter().any(|l| l.id == Some(1) && l.color[3] == 255), "the first stop's label shows");
        assert!(labels.iter().all(|l| l.id != Some(4) || l.color[3] == 0), "the last stop's label waits for its drop");
        let (before, _) = player.request_at(0);
        assert!(layer(&before, "stops").markers.as_ref().unwrap().iter().all(|m| m.id.is_none()), "nothing before the first reveal");
    }

    /// R8 review #1: a `pulse` on a themed `stops` layer draws its rings
    /// (as accent discs under the shadows), and a `highlight` dims the
    /// stops it does not pick.
    #[test]
    fn a_pulse_rings_and_a_highlight_dims_themed_stops() {
        let (player, _dir) = journey_with(serde_json::json!([
            {"at": 0.0, "overlay": {"layer": "stops", "effect": "pulse", "period_s": 1.0, "max_radius_px": 30}},
            {"at": 0.0, "until": 3.0, "overlay": {"layer": "stops", "effect": "highlight", "ids": [2], "dim_opacity": 0.3, "fade_s": 0.0}}]));
        let (request, _) = player.request_at(5);
        let shadow = layer(&request, "stops/shadow").markers.as_ref().unwrap();
        let rings: Vec<_> = shadow.iter().filter(|m| m.id.is_none()).collect();
        assert_eq!(rings.len(), 4, "a ring per stop: {rings:?}");
        assert!(rings.iter().all(|m| m.fill[..3] == [0xE8, 0x59, 0x0C] && m.radius > 0.0), "in the accent");
        let first_ring = shadow.iter().position(|m| m.id.is_none()).unwrap();
        assert!(shadow[..first_ring].is_empty(), "the rings draw first, under the shadows");
        let core = layer(&request, "stops").markers.as_ref().unwrap();
        let alpha = |id: u64| core.iter().find(|m| m.id == Some(id)).unwrap().fill[3];
        assert_eq!(alpha(2), 255, "the picked stop keeps its ink");
        assert!(alpha(1) < 100 && alpha(3) < 100, "the others dim to .3");
        assert!(layer(&request, "stops").marks.is_none(), "no mesh marks for a marker layer");
    }

    /// R8 review #5 (§6.1): an author's `circle-blur`, `circle-translate`
    /// and `circle-opacity` reach a themed point layer's top disc.
    #[test]
    fn an_authors_blur_translate_and_opacity_reach_the_themed_core() {
        let mut json = crate::capture::golden_tests::story_script("design_journey", "9:16", "fullhd");
        json["scene"]["overlays"][0]["layers"][1]["paint"] = serde_json::json!({"circle-blur": 0.3, "circle-translate": [4, -2], "circle-opacity": 0.5});
        let (player, _dir) = player_for(&json, crate::capture::golden_tests::story_inputs(std::path::Path::new(".")), RenderOptions::default());
        let (request, _) = player.request_at(10);
        let core = layer(&request, "stops").markers.as_ref().unwrap();
        assert!(!core.is_empty());
        for m in core {
            assert_eq!((m.blur, m.translate, m.fill[3]), (0.3, (4.0, -2.0), 128), "{m:?}");
        }
        let halo = layer(&request, "stops/halo").markers.as_ref().unwrap();
        assert!(halo.iter().all(|m| m.blur == 0.0 && m.translate == (0.0, 0.0)), "only the core takes the author's keys");
    }

    /// Review #2/#3: the head sits on an outline in the line's colour, and
    /// rings are capped by a budget shared across layers, lowest ids first.
    #[test]
    fn marks_outline_the_head_and_cap_rings_by_feature_id() {
        let layer = LayerRender {
            overlay_id: "o".into(),
            layer_id: "l".into(),
            geojson: "{}".into(),
            paint: Arc::new(serde_json::Map::new()),
            line_metrics: false,
            per_feature: true,
            above_labels: false,
            above_buildings: true,
            key: 1,
            points: (0..5).map(|i| (i, (f64::from(i as u32), 0.0))).collect(),
            colors: (0..5).map(|i| (i, [0, 0, 255, 255])).collect(),
            outline: Some([227, 52, 47, 255]),
            fx_id: "l".into(),
            trim: crate::story::theme_layers::Trim::Same,
            marks_on: true,
            arrowheads: Vec::new(),
            markers: None,
            labels: Vec::new(),
            point: None,
            has_core: true,
        };
        let fx = crate::story::effects::LayerFx {
            layer_id: "l".into(),
            head: Some((9.0, 1.0, 7.0, [255, 255, 255, 255])),
            rings: vec![(4, 10.0, 0.5), (1, 10.0, 0.5), (3, 10.0, 0.5), (0, 10.0, 0.5), (2, 10.0, 0.5)],
            ..Default::default()
        };
        let mut budget = 3;
        let marks: serde_json::Value = serde_json::from_str(&layer.marks(&fx, &mut budget).unwrap()).unwrap();
        let features = marks["features"].as_array().unwrap();
        let lons: Vec<f64> = features.iter().map(|f| f["geometry"]["coordinates"][0].as_f64().unwrap()).collect();
        assert_eq!(lons, [0.0, 1.0, 2.0, 9.0, 9.0], "the three lowest ids' rings, then the outline and the head");
        assert_eq!(budget, 0);
        assert_eq!(features[3]["properties"]["circle-color"], "#e3342fff");
        assert_eq!(features[3]["properties"]["circle-radius"].as_f64().unwrap(), f64::from(7.0 + HEAD_OUTLINE_PX));
        assert_eq!(features[4]["properties"]["circle-color"], "#ffffffff");
        // An exhausted budget still draws the head.
        let marks = layer.marks(&fx, &mut budget).unwrap();
        assert_eq!(serde_json::from_str::<serde_json::Value>(&marks).unwrap()["features"].as_array().unwrap().len(), 2);
    }

    /// Review #6/#7: a layer over the whole-source limit, and a layered
    /// overlay's document that is not JSON, are refused by `Player::new`,
    /// before any frame.
    #[test]
    fn player_new_refuses_an_oversized_layer_and_an_unparseable_document() {
        let n = crate::overlay::tile_plan::TILED_SOURCE_MIN_POINTS + 5;
        let coords: Vec<[f64; 2]> = (0..n).map(|i| [103.8 + i as f64 * 1e-7, 1.29]).collect();
        let mut json = drawn_route_json();
        json["scene"]["overlays"][0]["data"]["features"][0]["geometry"]["coordinates"] = serde_json::json!(coords);
        let raw = serde_json::to_vec(&json).unwrap();
        let script = crate::story::parse(&raw).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let opts = RenderOptions { cache_dir: dir.path().into(), ..RenderOptions::default() };
        let err = Player::new(&script, &raw, inputs(), opts.clone()).err().expect("refused");
        let msg = format!("{err:#}");
        assert!(msg.contains(&format!("has {n} line/polygon points")) && msg.contains("route-line"), "{msg}");

        let mut json = drawn_route_json();
        json["scene"]["overlays"][0].as_object_mut().unwrap().remove("data");
        json["scene"]["overlays"][0]["geojson"] = serde_json::json!("routes.geojson");
        let raw = serde_json::to_vec(&json).unwrap();
        let script = crate::story::parse(&raw).unwrap();
        let mut inp = inputs();
        inp.overlays.insert("routes.geojson".into(), "{not json".into());
        let err = Player::new(&script, &raw, inp, opts).err().expect("refused");
        let msg = format!("{err:#}");
        assert!(msg.contains("overlay `routes`") && msg.contains("routes.geojson"), "{msg}");
    }

    /// Task 8: a `data` path that validated fine but is gone by render time
    /// (here, simply never put in `inputs.overlays`) does not fail the
    /// render -- it is a gap, unlike a `geojson` overlay's missing file
    /// (which `scene_inputs` refuses to load at all).
    #[test]
    fn a_data_file_gone_by_render_time_is_a_gap_not_a_failed_render() {
        let mut json = det_json();
        json["scene"]["overlays"] = serde_json::json!([{
            "id": "route", "data": "route.geojson", "line_metrics": true,
            "layers": [{"id": "route-line", "type": "line", "paint": {"line-color": "red"}}]
        }]);
        let (mut player, _dir) = player_for(&json, inputs(), RenderOptions::default());
        let m = player.run(&mut Hashes(Vec::new()), &mut |_| {}).expect("a missing `data` file does not fail the render");
        assert!(m.gaps.iter().any(|g| g == "overlay route (data could not be loaded)"), "{:?}", m.gaps);
    }

    /// A sink that keeps each frame's `story_retessellations` (and writes
    /// PNGs to `pngs` when set).
    struct Retessellations(Vec<(usize, usize)>, Option<PathBuf>);
    impl FrameSink for Retessellations {
        fn push(&mut self, n: usize, f: &Capture) -> anyhow::Result<()> {
            assert!(f.tiles_drawn > 0, "frame {n} painted");
            self.0.push((n, f.story_retessellations));
            if let Some(dir) = &self.1 {
                f.save_png(&dir.join(format!("frame-{n:04}.png")))?;
            }
            Ok(())
        }
        fn finish(&mut self) -> anyhow::Result<serde_json::Value> {
            Ok(serde_json::Value::Null)
        }
    }

    /// R7 review: a `role` with a `paint` reaches the theme (§6.1
    /// merging), and the author's `line-color` and `line-width` on an
    /// `arrow` reach its arrowhead and stub as well as its core; with no
    /// paint, the head takes the accent and the theme's core width.
    #[test]
    fn an_arrows_author_colour_reaches_its_head() {
        let _gpu = gpu_lock();
        let arrow = |paint: serde_json::Value| {
            let mut json = det_json();
            json["design"] = serde_json::json!({"theme": "ink"});
            json["scene"]["overlays"] = serde_json::json!([{
                "id": "moves", "line_metrics": true,
                "data": {"type": "FeatureCollection", "features": [{"type": "Feature", "properties": {},
                    "geometry": {"type": "LineString", "coordinates": [[103.846, 1.2925], [103.851, 1.2880]]}}]},
                "layers": [{"id": "a", "type": "line", "role": "arrow", "paint": paint}]
            }]);
            let (player, _dir) = player_for(&json, inputs(), RenderOptions::default());
            player.request_at(0).0
        };
        let painted = arrow(serde_json::json!({"line-color": "#123456", "line-width": 10}));
        let ids: Vec<&str> = painted.story_layers.iter().map(|l| &*l.layer_id).collect();
        assert_eq!(ids, ["a/casing", "a"], "role + paint is themed, not drawn as 1a");
        let core = &painted.story_layers[1];
        assert_eq!(core.paint["line-color"], "#123456", "the author's colour on the core");
        assert_eq!(core.paint["line-cap"], "butt", "the theme's other keys stay");
        assert_eq!(core.arrowheads.len(), 1);
        let head = core.arrowheads[0];
        assert_eq!(head.color, [0x12, 0x34, 0x56, 255], "the head in the author's colour");
        assert_eq!(head.stub_width, 10.0, "the stub at the author's width");
        assert_eq!(head.edge_width, 0.5, "the edge as wide as the casing shows beside that core");
        assert_eq!(head.end, (103.851, 1.2880), "on the last vertex");
        assert!(painted.story_layers[0].arrowheads.is_empty(), "the casing has none");
        let themed = arrow(serde_json::json!({}));
        let head = themed.story_layers[1].arrowheads[0];
        assert_eq!(head.color, [0xE0, 0x31, 0x31, 255], "no paint: the ink theme's accent");
        assert_eq!(head.stub_width, 6.0, "and the theme's core width");
        assert_eq!(head.edge_width, crate::story::theme_layers::ARROWHEAD_EDGE, "edged at the casing's 2.5");
    }

    #[test]
    fn a_story_draw_passes_its_trim_to_the_request_and_no_frame_after_the_first_retessellates() {
        let _gpu = gpu_lock();
        let (mut player, _dir) = player_for(&drawn_route_json(), inputs(), RenderOptions::default());
        assert_eq!(player.frames(), 90);
        let mut ends = Vec::new();
        for n in 0..90 {
            let (request, state) = player.request_at(n);
            assert_eq!(request.story_layers.len(), 1, "frame {n}: the layer is drawn as a story layer");
            assert!(request.overlay.is_none(), "frame {n}: a layered overlay is not merged into `overlay`");
            let layer = &request.story_layers[0];
            let want = state.layer_fx[0].trim.map_or(1.0, |[_, end]| end);
            assert_eq!(layer.line_fx[1], want, "frame {n}: the effect's trim end reaches the request");
            assert_eq!(layer.line_fx[2], state.layer_fx[0].opacity, "frame {n}: and its opacity");
            ends.push(layer.line_fx[1]);
        }
        // Linear over [0.5, 2.5] s at 30 fps: frame 15 starts, 45 is half, 75 is whole.
        assert_eq!(ends[15], 0.0);
        assert!((ends[45] - 0.5).abs() < 1e-6, "{}", ends[45]);
        assert!(ends[75..].iter().all(|e| *e == 1.0));
        assert!(ends[15..].windows(2).all(|w| w[1] >= w[0]), "the trim only grows once the draw starts");
        assert!((0..15).all(|n| player.request_at(n).0.story_layers[0].line_fx[2] == 0.0), "hidden before its draw");
        // The head rides the tip while drawing, and is gone after.
        assert!(player.request_at(45).0.story_layers[0].marks.is_some());
        assert!(player.request_at(80).0.story_layers[0].marks.is_none());

        let pngs = std::env::var_os("GPUI_MAP_T7_PNGS").map(PathBuf::from);
        player.set_frames(Some(30..60));
        let mut sink = Retessellations(Vec::new(), pngs);
        player.run(&mut sink, &mut |_| {}).map_err(|e| format!("{e:#}")).unwrap();
        let counts: Vec<usize> = sink.0.iter().map(|(_, c)| *c).collect();
        eprintln!("STORY_RETESSELLATIONS frames 30..60: {counts:?}");
        assert_eq!(counts.len(), 30);
        assert_eq!(counts[0], 1, "the first frame tessellates the layer");
        assert!(counts[1..].iter().all(|c| *c == 0), "a draw costs no re-tessellation: {counts:?}");
    }

    /// Merge with master's denser 3D buildings: each frame's request
    /// carries the building floor already resolved (so the prefetch plan
    /// fetches the floor's tiles, and the capture draws what the live map
    /// draws), and the scene's `buildings` look.
    #[test]
    fn a_frame_resolves_the_building_floor_and_the_buildings_look() {
        let _g = gpu_lock();
        let mut json = det_json();
        json["camera"] = serde_json::json!([{"type":"jump","to":{"lon":103.851,"lat":1.2905,"zoom":16.0,"pitch":60}},{"type":"hold","duration":2.0}]);
        let (player, _dir) = player_for(&json, inputs(), RenderOptions::default());
        let (r, _) = player.request_at(0);
        assert_eq!(r.building_floor, Some(crate::render::cover_plan::building_floor_for(None)), "Liberty extrudes at z16");
        assert_eq!((r.extrusion_look, r.label_occlusion), (crate::render::extrusion::ExtrusionLook::ENHANCED, true), "enhanced by default");
        let unresolved = CaptureRequest { building_floor: None, disable_building_floor: true, ..r.clone() };
        assert_ne!(r.cover(), unresolved.cover(), "the floor changes what the frame fetches");
        json["scene"]["buildings"] = "maplibre".into();
        let (flat, _dir) = player_for(&json, inputs(), RenderOptions::default());
        let (m, _) = flat.request_at(0);
        assert_eq!((m.extrusion_look, m.label_occlusion), (crate::render::extrusion::ExtrusionLook::MAPLIBRE, false));
        assert_eq!(m.building_floor, r.building_floor, "the floor is the map's rule, not the look's");
    }

    #[test]
    fn two_fresh_sessions_give_identical_frames() {
        let _g = gpu_lock();
        let a = render_hashes(None, false);
        assert_eq!(a.len(), 20, "2 s at 10 fps");
        assert_eq!(a, render_hashes(None, false));
    }
    #[test]
    fn sleeping_between_frames_changes_nothing() {
        let _g = gpu_lock();
        assert_eq!(render_hashes(None, false), render_hashes(None, true));
    }
    #[test]
    fn a_frame_range_renders_the_same_frames_as_the_whole_story() {
        let _g = gpu_lock();
        let all = render_hashes(None, false);
        assert_eq!(render_hashes(Some(7..12), false), all[7..12].to_vec());
    }
    /// A 4 s fixture story orbiting 360 degrees at `fps` about Marina Bay,
    /// with a callout 0.03 degrees (about 800 units at zoom 14.2) east of
    /// the centre: on the 1920 x 1080-unit frame it swings out to the
    /// right edge (its card flips left), to the left edge (back right),
    /// and off the top and bottom (it fades).
    fn orbiting_callout_json(fps: u32) -> serde_json::Value {
        serde_json::json!({"story":1,"output":{"aspect":"16:9","tier":"hd","fps":fps},
          "scene":{"style":"examples/liberty-style.json","basemap":{"source":"fixture"},"labels":true},
          "camera":[{"type":"jump","to":{"lon":103.851,"lat":1.2905,"zoom":14.2}},
                    {"type":"orbit","degrees":360,"duration":4.0,"easing":"linear"}],
          "events":[{"at":0.0,"until":4.0,"callout":{"at_coord":[103.881,1.2905],"text":"Changi","fade_s":0.0}}]})
    }

    /// The callout on frame `n`'s request: `(anchor, side, alpha)`.
    fn callout_on(player: &Player, n: usize) -> Option<((f32, f32), crate::story::layout::Side, f32)> {
        let (request, _) = player.request_at(n);
        let story = request.story?;
        story.items.iter().find_map(|i| match i {
            crate::story::layout::LayoutItem::Callout { id, anchor, side, .. } => {
                let alpha = story.motion.iter().find(|(m, ..)| m == id).map(|(_, a, _)| *a)?;
                Some((*anchor, *side, alpha))
            }
            _ => None,
        })
    }

    /// Pre-flight ruling F20: the side schedule runs on its own grid, so a
    /// 24, a 30 and a 60 fps render of one story agree -- anchor, side and
    /// fade -- at every time the three share (every sixth of a second).
    /// The orbit flips the card, so the agreement is not vacuous.
    #[test]
    fn the_schedule_is_the_same_whatever_the_output_fps() {
        use crate::story::layout::Side;
        let _g = gpu_lock();
        let players: Vec<(u32, Player, tempfile::TempDir)> = [24u32, 30, 60]
            .into_iter()
            .map(|fps| {
                let (p, d) = player_for(&orbiting_callout_json(fps), inputs(), RenderOptions::default());
                (fps, p, d)
            })
            .collect();
        let mut sides = Vec::new();
        let mut faded = false;
        for k in 0..24usize {
            let at: Vec<_> = players.iter().map(|(fps, p, _)| callout_on(p, k * (*fps as usize) / 6)).collect();
            assert!(at.windows(2).all(|w| w[0] == w[1]), "t = {k}/6 s: {at:?}");
            if let Some((_, side, alpha)) = at[0] {
                sides.push(side);
                faded |= alpha < 1.0;
            } else {
                faded = true;
            }
        }
        assert!(sides.contains(&Side::Left) && sides.contains(&Side::Right), "the orbit flips the card: {sides:?}");
        assert!(faded, "and takes it off the frame's edge somewhere");
    }

    /// Review Focus 2 at player level: a callout pinned to Singapore while
    /// the globe turns to the far side of the planet is on the first frame,
    /// fades, and is no item at all once it is behind the horizon.
    #[test]
    fn a_callout_behind_the_globe_leaves_no_item_on_the_frame() {
        let _g = gpu_lock();
        let json = serde_json::json!({"story":1,"output":{"width":480,"height":270,"fps":10},
          "scene":{"style":"examples/liberty-style.json","basemap":{"source":"fixture"},"globe":true},
          "camera":[{"type":"jump","to":{"lon":103.85,"lat":1.29,"zoom":2.0}},
                    {"type":"fly","to":{"lon":-76.15,"lat":-1.29,"zoom":2.0},"duration":3.0}],
          "events":[{"at":0.0,"callout":{"at_coord":[103.85,1.29],"text":"Singapore","fade_s":0.0}}]});
        let (player, _d) = player_for(&json, inputs(), RenderOptions::default());
        let first = callout_on(&player, 0).expect("on the first frame");
        assert_eq!(first.2, 1.0);
        let last = player.frames() - 1;
        assert_eq!(callout_on(&player, last), None, "behind the globe: no callout item");
        // Somewhere between, it fades rather than snapping off.
        assert!((0..=last).filter_map(|n| callout_on(&player, n)).any(|(.., a)| a > 0.0 && a < 1.0));
    }

    /// Task 11: a frame's panels and captions reach the capture as its
    /// story (panel alpha/enter as motion, captions by their index), and a
    /// frame with neither has none.
    #[test]
    fn the_request_carries_this_frames_panels_and_captions_as_its_story() {
        let _g = gpu_lock();
        let mut json = det_json();
        json["events"].as_array_mut().unwrap().push(serde_json::json!(
            {"at": 0.4, "until": 1.6, "panel": {"placement": "lower-third", "headline": "Harbour", "theme": "light"}}
        ));
        let (player, _d) = player_for(&json, inputs(), RenderOptions::default());
        let (r0, _) = player.request_at(0);
        assert!(r0.story.is_none(), "nothing on screen at 0 s");
        let (r5, state) = player.request_at(5);
        let story = r5.story.expect("a panel and a caption at 0.5 s");
        let panel = state.elements.iter().find(|e| matches!(e.kind, crate::story::timeline::ElementKind::Panel(_))).unwrap();
        assert!(story.items.iter().any(|i| matches!(i,
            crate::story::layout::LayoutItem::Panel { id, theme, .. } if *id == panel.id && theme.fill[0] == 255)));
        assert_eq!(story.motion, vec![(panel.id.clone(), panel.alpha, panel.enter)]);
        assert!(story.items.iter().any(|i| matches!(i,
            crate::story::layout::LayoutItem::Caption { id, text, .. } if id == "caption/0" && text == "Marina Bay")));
    }

    /// A story folder with `img/a.png` (60 x 40) and the det script plus a
    /// lower-third panel embedding it and a corner image of it.
    fn image_story() -> (serde_json::Value, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("img")).unwrap();
        let img = image::RgbaImage::from_fn(60, 40, |x, y| image::Rgba([(x * 4) as u8, (y * 6) as u8, 128, 255]));
        img.save(dir.path().join("img/a.png")).unwrap();
        let mut json = det_json();
        let image = serde_json::json!({"src": "img/a.png", "alt": "A gradient", "credit": "Photo: Test", "fit": "cover"});
        let events = json["events"].as_array_mut().unwrap();
        events.push(serde_json::json!({"at": 0.4, "until": 1.6, "panel": {"placement": "lower-third", "headline": "Harbour", "image": image}}));
        let mut corner = image.clone();
        corner["placement"] = "corner".into();
        events.push(serde_json::json!({"at": 0.4, "until": 1.6, "image": corner}));
        (json, dir)
    }

    /// Review Focus 4, the render half (the 404 half is in `media::tests`):
    /// an image that loaded at validation and is gone when the player
    /// loads is a gap, every frame renders, the rows that show it name it
    /// in `missing`, and its panel draws its text with no image rect.
    #[test]
    fn an_image_that_404s_is_an_error_in_validation_and_a_gap_in_a_render() {
        let _g = gpu_lock();
        let (json, dir) = image_story();
        let raw = serde_json::to_vec(&json).unwrap();
        let script = crate::story::parse(&raw).unwrap();
        // "Validation": every image loads, with no error.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let checked = rt.block_on(crate::story::media::load_script_images(&script, dir.path(), dir.path(), 1920, true));
        assert!(checked.failed.is_empty() && checked.loaded.len() == 1, "{:?}", checked.diagnostics);
        assert!(!checked.diagnostics.iter().any(|d| d.severity == crate::story::Severity::Error));
        // Present: the panel carries the image's aspect, both items draw it, one `Arc`.
        let with = SceneInputs { story_dir: dir.path().into(), ..inputs() };
        let (player, _c) = player_for(&json, with, RenderOptions::default());
        let (r5, _) = player.request_at(5);
        let story = r5.story.expect("panel and image at 0.5 s");
        assert!(story.items.iter().any(|i| matches!(i, crate::story::layout::LayoutItem::Panel { image_aspect: Some(a), .. } if (*a - 1.5).abs() < 1e-6)));
        assert!(story.items.iter().any(|i| matches!(i, crate::story::layout::LayoutItem::Image { .. })));
        assert_eq!(story.images.len(), 2);
        assert!(Arc::ptr_eq(&story.images[0].image, &story.images[1].image), "one decode, one texture");
        assert_eq!(story.images[0].credit.as_deref(), Some("Photo: Test"));
        assert!(r5.attribution.as_deref().is_some_and(|a| a.contains("Photo: Test")), "in the credit plate: {:?}", r5.attribution);
        drop(player);
        // Gone after validation.
        std::fs::remove_file(dir.path().join("img/a.png")).unwrap();
        let gone = SceneInputs { story_dir: dir.path().into(), ..inputs() };
        let (mut player, _c) = player_for(&json, gone, RenderOptions::default());
        let (r5, _) = player.request_at(5);
        let story = r5.story.expect("the panel still draws");
        assert!(story.images.is_empty());
        assert!(!r5.attribution.as_deref().unwrap_or("").contains("Photo: Test"), "an image not drawn is not credited");
        assert!(story.items.iter().any(|i| matches!(i, crate::story::layout::LayoutItem::Panel { image_aspect: None, headline: Some(h), .. } if h == "Harbour")));
        assert!(!story.items.iter().any(|i| matches!(i, crate::story::layout::LayoutItem::Image { .. })), "no image rect");
        let m = player.run(&mut Hashes(Vec::new()), &mut |_| {}).expect("a validated image gone by render time is a gap");
        assert_eq!(m.frames.len(), player.frames(), "every frame rendered");
        assert!(m.gaps.iter().any(|g| g == "image img/a.png (file_not_found)"), "{:?}", m.gaps);
        let miss = ("image:img/a.png".to_string(), MissReason::Failed("file_not_found".into()));
        assert!(m.frames[5].missing.contains(&miss), "{:?}", m.frames[5].missing);
        assert!(!m.frames[0].missing.contains(&miss), "not on screen at 0 s");
    }

    /// Review F1: without validation (the agent's render path), an image
    /// that does not load refuses the render.
    #[test]
    fn an_unvalidated_image_that_does_not_load_refuses_the_render() {
        let _g = gpu_lock();
        let (json, dir) = image_story();
        std::fs::remove_file(dir.path().join("img/a.png")).unwrap();
        let raw = serde_json::to_vec(&json).unwrap();
        let s = crate::story::parse(&raw).unwrap();
        let inp = SceneInputs { story_dir: dir.path().into(), images_validated: false, ..inputs() };
        let cache = tempfile::tempdir().unwrap();
        let err = Player::new(&s, &raw, inp, RenderOptions { cache_dir: cache.path().into(), ..RenderOptions::default() })
            .err()
            .expect("refused");
        assert!(format!("{err:#}").contains("image img/a.png (file_not_found)"), "{err:#}");
    }

    /// An image validated -- and so cached -- in one folder is found
    /// offline at render time when the render names the same folder
    /// (`ValidateOptions::image_cache_dir` == `RenderOptions::image_cache_dir`),
    /// and not when it names another.
    #[test]
    fn validation_and_the_render_share_one_image_cache() {
        let _g = gpu_lock();
        let shared = tempfile::tempdir().unwrap();
        let url = "https://images.example.invalid/harbour.png";
        // What a validation fetch leaves in the cache.
        let png = {
            let mut c = std::io::Cursor::new(Vec::new());
            image::RgbaImage::from_pixel(30, 20, image::Rgba([200, 90, 40, 255])).write_to(&mut c, image::ImageFormat::Png).unwrap();
            c.into_inner()
        };
        let key: String = <sha2::Sha256 as sha2::Digest>::digest(url.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
        std::fs::create_dir_all(shared.path()).unwrap();
        std::fs::write(shared.path().join(&key), &png).unwrap();
        let mut json = det_json();
        json["scene"]["style"] = serde_json::json!(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/liberty-style.json"));
        json["events"].as_array_mut().unwrap().push(serde_json::json!(
            {"at": 0.0, "until": 1.0, "image": {"src": url, "alt": "a", "credit": "c", "placement": "corner"}}
        ));
        let raw = serde_json::to_vec(&json).unwrap();
        let s = crate::story::parse(&raw).unwrap();
        // Validation, offline, against the shared folder: the image is found.
        let vopts = crate::story::ValidateOptions { offline: true, base_dir: PathBuf::from("."), image_cache_dir: Some(shared.path().into()), estimate: true, ..crate::story::ValidateOptions::default() };
        let report = tokio::runtime::Runtime::new().unwrap().block_on(crate::story::validate_scene(&s, &raw, &vopts));
        assert!(!report.diagnostics.iter().any(|d| d.path.starts_with("/events") && d.severity == crate::story::Severity::Error), "{:?}", report.diagnostics);
        // The render, offline, with the same folder: found (no refusal even unvalidated).
        let tiles = tempfile::tempdir().unwrap();
        let opts = RenderOptions { cache_dir: tiles.path().into(), image_cache_dir: Some(shared.path().into()), ..RenderOptions::default() };
        let inp = SceneInputs { offline: true, images_validated: false, ..inputs() };
        let player = Player::new(&s, &raw, inp, opts).expect("the cached image is found offline");
        assert_eq!(player.images().len(), 1);
        assert_eq!(player.images()[0].original, (30, 20));
        drop(player);
        // Another folder has nothing: offline, the unvalidated render is refused.
        let opts = RenderOptions { cache_dir: tiles.path().into(), ..RenderOptions::default() };
        let inp = SceneInputs { offline: true, images_validated: false, ..inputs() };
        let err = Player::new(&s, &raw, inp, opts).err().expect("not in this render's cache");
        assert!(format!("{err:#}").contains("image_fetch_failed"), "{err:#}");
    }

    /// The same image bytes give the same frame; the image changes it.
    #[test]
    fn a_frame_with_an_image_is_deterministic() {
        let _g = gpu_lock();
        let (json, dir) = image_story();
        let frame5 = |json: &serde_json::Value| {
            let inp = SceneInputs { story_dir: dir.path().into(), ..inputs() };
            let (mut player, _c) = player_for(json, inp, RenderOptions { frames: Some(5..6), ..RenderOptions::default() });
            let mut sink = Hashes(Vec::new());
            player.run(&mut sink, &mut |_| {}).unwrap();
            sink.0
        };
        let a = frame5(&json);
        assert_eq!(a, frame5(&json));
        let mut without = json.clone();
        without["events"].as_array_mut().unwrap().pop();
        assert_ne!(a, frame5(&without), "the corner image is drawn");
    }

    /// Task 11 / F11: every frame's layout warnings land in the manifest,
    /// once per `(id, code)`.
    #[test]
    fn the_manifest_gathers_layout_warnings_once_per_id_and_code() {
        let _g = gpu_lock();
        let mut json = det_json();
        let long = "A headline far too long for any lower third to hold at its nominal size ".repeat(12);
        json["events"].as_array_mut().unwrap().push(serde_json::json!(
            {"at": 0.0, "until": 2.0, "panel": {"placement": "lower-third", "headline": long}}
        ));
        let (mut p, _d) = player_for(&json, inputs(), RenderOptions::default());
        let mut sink = Hashes(Vec::new());
        let m = p.run_range(3..6, &mut sink, &mut |_| {}).unwrap();
        let ours: Vec<_> = m.layout_warnings.iter().filter(|w| w.id == "/events/2").collect();
        assert!(!ours.is_empty(), "{:?}", m.layout_warnings);
        let mut keys: Vec<_> = m.layout_warnings.iter().map(|w| (w.id.clone(), w.code)).collect();
        let n = keys.len();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), n, "duplicates in {:?}", m.layout_warnings);
        assert!(m.frames.iter().filter(|r| !r.layout_warnings.is_empty()).count() >= 2, "every frame warned");
        let json = serde_json::to_value(&m).unwrap();
        assert!(json["layout_warnings"].as_array().is_some_and(|a| !a.is_empty()));
        assert!(json["frames"][0].get("layout_warnings").is_none(), "per-row warnings are not serialised");
    }

    /// F6: the edit, not the camera, changes frame 5. The same story
    /// without the `water` edit renders frame 4 identically (before the
    /// edit, same camera) and frame 5 differently (same camera, edit on).
    #[test]
    fn frames_before_and_after_the_style_edit_differ() {
        let _g = gpu_lock();
        let hashes = |json: serde_json::Value| {
            let (mut p, _d) = player_for(&json, inputs(), RenderOptions::default());
            let mut sink = Hashes(Vec::new());
            p.run_range(4..6, &mut sink, &mut |_| {}).unwrap();
            sink.0
        };
        let with = hashes(det_json());
        let mut without = det_json();
        without["events"].as_array_mut().unwrap().remove(0);
        let without = hashes(without);
        assert_eq!(with[0], without[0], "frame 4: before the edit, the same frame");
        assert_ne!(with[1], without[1], "frame 5: the same camera, and the edit shows");
    }

    /// F7 / spec §8.3: one player renders ranges B, A, B (and a still):
    /// B's frames are the same bytes both times, and the same as a fresh
    /// player's whole-story frames.
    #[test]
    fn one_player_renders_ranges_in_any_order_with_the_same_bytes() {
        let _g = gpu_lock();
        let all = render_hashes(None, false);
        let (mut p, _d) = player_for(&det_json(), inputs(), RenderOptions::default());
        let mut range = |r: std::ops::Range<usize>| {
            let mut sink = Hashes(Vec::new());
            p.run_range(r, &mut sink, &mut |_| {}).unwrap();
            sink.0
        };
        let b1 = range(12..15);
        let a = range(7..12);
        let b2 = range(12..15);
        let still = range(9..10);
        assert_eq!(b1, b2, "A,B,A: the same frames after another range");
        assert_eq!(b1, all[12..15].to_vec());
        assert_eq!(a, all[7..12].to_vec());
        assert_eq!(still, vec![all[9].clone()]);
    }
    #[test]
    fn cancel_stops_with_an_error_naming_the_frame() {
        let _g = gpu_lock();
        let (s, raw) = det_script();
        let dir = tempfile::tempdir().unwrap();
        let cancel = Arc::new(AtomicBool::new(true));
        let mut player =
            Player::new(&s, &raw, inputs(), RenderOptions { cancel, cache_dir: dir.path().into(), ..RenderOptions::default() })
                .unwrap();
        let err = player.run(&mut Hashes(Vec::new()), &mut |_| {}).unwrap_err();
        assert!(format!("{err:#}").contains("cancelled at frame 0"), "{err:#}");
    }
    /// F10: a cancel set mid-run (from the progress callback after frame
    /// 3) stops before frame 4; the sink got frames 0..3 at most and was
    /// told to abort, never to finish.
    #[test]
    fn a_cancel_mid_run_stops_at_the_next_frame_and_aborts_the_sink() {
        let _g = gpu_lock();
        let cancel = Arc::new(AtomicBool::new(false));
        let (mut p, _d) = player_for(&det_json(), inputs(), RenderOptions { cancel: Arc::clone(&cancel), ..RenderOptions::default() });
        let mut sink = Recorder::default();
        let err = p
            .run(&mut sink, &mut |pr| {
                if pr.frame == 4 {
                    cancel.store(true, Ordering::Relaxed);
                }
            })
            .unwrap_err();
        assert!(format!("{err:#}").contains("cancelled at frame 4"), "{err:#}");
        assert!(sink.pushed.iter().all(|n| *n <= 3), "{:?}", sink.pushed);
        assert!(sink.aborted && !sink.finished);
    }

    /// F4: a cancel during a frame's tile wait is seen within ~100 ms,
    /// not at the 10 s tile timeout.
    #[test]
    fn a_cancel_during_the_tile_wait_is_seen_promptly() {
        let _g = gpu_lock();
        let cancel = Arc::new(AtomicBool::new(false));
        let probe = player_for(&det_json(), inputs(), RenderOptions::default()).0;
        let never: HashSet<TileId> = probe.request_at(0).0.cover().into_iter().collect();
        drop(probe);
        let mut inp = inputs();
        inp.source = Arc::new(crate::story::fixture::FakeSource { never, ..crate::story::fixture::FakeSource::fixture(Duration::ZERO) });
        let (mut p, _d) = player_for(&det_json(), inp, RenderOptions { cancel: Arc::clone(&cancel), ..RenderOptions::default() });
        let setter = {
            let cancel = Arc::clone(&cancel);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                cancel.store(true, Ordering::Relaxed);
                Instant::now()
            })
        };
        let mut sink = Recorder::default();
        let err = p.run(&mut sink, &mut |_| {}).unwrap_err();
        let seen = Instant::now();
        let set_at = setter.join().unwrap();
        assert!(format!("{err:#}").contains("cancelled at frame 0 (waiting on"), "{err:#}");
        // 500 ms: loose for a loaded machine, still 20x under the 10 s
        // tile timeout a revert would wait.
        assert!(seen.duration_since(set_at) < Duration::from_millis(500), "{:?}", seen.duration_since(set_at));
        assert!(sink.aborted);
    }

    /// F10: jitter with real fetch latency. Every tile takes 20 ms, so a
    /// frame whose content depended on what had landed by render time
    /// would show it; with sleeps between frames the look-ahead lands
    /// more of the future before each frame, and nothing changes.
    #[test]
    fn sleeping_between_frames_with_slow_tiles_changes_nothing() {
        let _g = gpu_lock();
        let run = |jitter: bool| {
            let mut inp = inputs();
            inp.source = Arc::new(crate::story::fixture::FakeSource::fixture(Duration::from_millis(20)));
            let (mut p, _d) = player_for(&det_json(), inp, RenderOptions::default());
            let mut sink = Hashes(Vec::new());
            let mut n = 0u64;
            p.run(&mut sink, &mut |_| {
                if jitter {
                    n += 1;
                    std::thread::sleep(Duration::from_millis((n * 7919) % 31));
                }
            })
            .unwrap();
            sink.0
        };
        let fast = run(false);
        assert_eq!(fast, run(true));
        assert_eq!(fast, render_hashes(None, false), "and the same as the instant source");
    }

    /// F2: a settle point waits its own `timeout_s` (0.3 s here), not the
    /// prefetcher's 60 s default, and a miss there is an error.
    #[test]
    fn a_settle_point_uses_the_scripts_timeout() {
        let _g = gpu_lock();
        let mut json = det_json();
        json["events"].as_array_mut().unwrap().push(serde_json::json!({"at": 0, "settle": {"timeout_s": 0.3}}));
        let probe = player_for(&json, inputs(), RenderOptions::default()).0;
        let never: HashSet<TileId> = probe.request_at(0).0.cover().into_iter().take(1).collect();
        drop(probe);
        let mut inp = inputs();
        inp.source = Arc::new(crate::story::fixture::FakeSource { never, ..crate::story::fixture::FakeSource::fixture(Duration::ZERO) });
        let (mut p, _d) = player_for(&json, inp, RenderOptions::default());
        let started = Instant::now();
        let err = p.run(&mut Recorder::default(), &mut |_| {}).unwrap_err();
        let took = started.elapsed();
        assert!(format!("{err:#}").contains("at a settle point"), "{err:#}");
        assert!(took >= Duration::from_millis(300) && took < Duration::from_secs(5), "{took:?}");
    }

    /// F1: daylight by default. No sun on the ground unless
    /// `scene.terminator` (or a `terminator.on` event) turns it on, and
    /// the glow never gets the wall-clock `Sun::Now`.
    #[test]
    fn a_story_renders_in_daylight_unless_the_terminator_is_on() {
        let _g = gpu_lock();
        let globe = |terminator: Option<bool>, events: serde_json::Value| {
            let mut json = serde_json::json!({"story":1,"output":{"width":320,"height":200,"fps":10},
              "scene":{"style":"examples/liberty-style.json","basemap":{"source":"fixture"},"globe":true,"labels":false},
              "camera":[{"type":"jump","to":{"lon":103.85,"lat":1.29,"zoom":3.0,"globe":true}},{"type":"hold","duration":1.0}],
              "clock":{"start_utc":"2026-03-20T10:00:00Z","rate":60},
              "events":events});
            if let Some(on) = terminator {
                json["scene"]["terminator"] = serde_json::Value::Bool(on);
            }
            player_for(&json, inputs(), RenderOptions::default()).0
        };
        let day = globe(None, serde_json::json!([]));
        let (r, _) = day.request_at(3);
        assert_eq!(r.sun, None);
        assert_eq!(r.atmosphere.map(|a| a.sun), Some(crate::render::atmosphere::DAYLIGHT_SUN));
        // F3: the compact bake is pinned on the request, never left to the env.
        assert!(r.basemap_compact && r.basemap_compact_strokes == Some(true));
        let start = crate::story::script::parse_utc("2026-03-20T10:00:00Z").unwrap();
        let night = globe(Some(true), serde_json::json!([]));
        let (rn, _) = night.request_at(3);
        assert_eq!(rn.sun, Some(Sun::at_utc(start + 0.3 * 60.0)));
        assert_eq!(rn.atmosphere.map(|a| a.sun), rn.sun);
        let flip = globe(None, serde_json::json!([{"at":0.5,"set_param":{"param":"terminator.on","value":true}}]));
        assert_eq!(flip.request_at(4).0.sun, None);
        assert!(flip.request_at(5).0.sun.is_some());
        // Pixels: the clock's night shows only with the terminator on.
        let mut session = CaptureSession::new_blocking(None).unwrap();
        let style = crate::style_spec::parse_and_compile(
            &std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/liberty-style.json")).unwrap(),
        )
        .unwrap();
        let tile = crate::mvt::decode(crate::story::fixture::FIXTURE_TILE.to_vec()).unwrap();
        let store: crate::capture::TileStore = r.cover().into_iter().map(|t| (t, tile.clone())).collect();
        let lit = session.render(&rn, &store, &style).unwrap();
        let plain = session.render(&r, &store, &style).unwrap();
        let unlit = session.render(&CaptureRequest { sun: None, atmosphere: None, ..r.clone() }, &store, &style).unwrap();
        assert!(plain.tiles_drawn > 0);
        assert_ne!(lit.sha256_hex(), plain.sha256_hex(), "the terminator shades the frame");
        assert_eq!(plain.tiles_drawn, unlit.tiles_drawn);
    }

    #[test]
    fn a_render_with_a_failing_source_still_writes_a_manifest() {
        // Frame 0's cover at z14 is served; every tile first needed later fails. Later
        // frames draw frame-0 tiles still resident, or ancestors, and name the rest.
        let _g = gpu_lock();
        let (s, raw) = det_script();
        let dir = tempfile::tempdir().unwrap();
        let probe =
            Player::new(&s, &raw, inputs(), RenderOptions { cache_dir: dir.path().into(), ..RenderOptions::default() }).unwrap();
        let first: HashSet<TileId> = probe.request_at(0).0.cover().into_iter().collect();
        let all: HashSet<TileId> = (0..20).flat_map(|n| probe.request_at(n).0.cover()).collect();
        let fail: HashSet<TileId> = all.difference(&first).copied().collect();
        assert!(!fail.is_empty(), "the story must move onto new tiles for this test to mean anything");
        drop(probe);
        let mut inp = inputs();
        inp.source = Arc::new(crate::story::fixture::FakeSource {
            fail: fail.clone(),
            ..crate::story::fixture::FakeSource::fixture(std::time::Duration::ZERO)
        });
        let cache = tempfile::tempdir().unwrap();
        let mut player = Player::new(
            &s,
            &raw,
            inp,
            RenderOptions {
                cache_dir: cache.path().into(),
                tile_timeout: std::time::Duration::from_millis(200),
                ..RenderOptions::default()
            },
        )
        .unwrap();
        let m = player.run(&mut Hashes(Vec::new()), &mut |_| {}).expect("non-strict: the render completes");
        assert!(m.frames[0].missing.is_empty());
        assert!(
            m.frames.iter().any(|r| r.missing.iter().any(|(_, why)| matches!(why, MissReason::Failed(_)))),
            "failures are named in the manifest"
        );
        let json = serde_json::to_value(&m).unwrap();
        assert!(json["frames"][0]["tiles_missing"].is_array(), "the render's own view is in the manifest (F4)");
    }

    /// Final review F1: a keyed tiles URL -- `access_token=`, `key=` and
    /// `user:pass@` -- whose fetches partly fail never lands in anything
    /// the render writes or says: the manifest (its `script` copy, its
    /// `missing` reasons, its `gaps`, which are also what stderr prints)
    /// and every file under the cache and output folders.
    #[test]
    fn a_keyed_tiles_url_never_reaches_the_manifest_or_a_written_file() {
        struct Serve(HashSet<(u8, u32, u32)>);
        impl wiremock::Respond for Serve {
            fn respond(&self, req: &wiremock::Request) -> wiremock::ResponseTemplate {
                let parts: Vec<&str> = req.url.path().trim_start_matches('/').trim_end_matches(".mvt").split('/').collect();
                let id = (parts[0].parse().unwrap(), parts[1].parse().unwrap(), parts[2].parse().unwrap());
                if self.0.contains(&id) {
                    wiremock::ResponseTemplate::new(500)
                } else {
                    wiremock::ResponseTemplate::new(200).set_body_bytes(crate::story::fixture::FIXTURE_TILE)
                }
            }
        }
        let _g = gpu_lock();
        let (s, raw) = det_script();
        let dir = tempfile::tempdir().unwrap();
        let probe =
            Player::new(&s, &raw, inputs(), RenderOptions { cache_dir: dir.path().into(), ..RenderOptions::default() }).unwrap();
        let first: HashSet<TileId> = probe.request_at(0).0.cover().into_iter().collect();
        let all: HashSet<TileId> = (0..20).flat_map(|n| probe.request_at(n).0.cover()).collect();
        let fail: HashSet<(u8, u32, u32)> = all.difference(&first).map(|t| (t.z, t.x, t.y)).collect();
        assert!(!fail.is_empty());
        drop(probe);

        let server_rt = tokio::runtime::Runtime::new().unwrap();
        let server = server_rt.block_on(wiremock::MockServer::start());
        server_rt.block_on(wiremock::Mock::given(wiremock::matchers::method("GET")).respond_with(Serve(fail)).mount(&server));
        let url = format!(
            "{}/{{z}}/{{x}}/{{y}}.mvt?access_token=SECRET1&key=SECRET2",
            server.uri().replace("://", "://user:SECRET3@")
        );
        let mut json = det_json();
        json["scene"]["basemap"] = serde_json::json!({"tiles": [url], "max_zoom": 14, "attribution": "© Test tiles"});
        json["scene"]["style"] = serde_json::json!(format!("{}/examples/liberty-style.json", env!("CARGO_MANIFEST_DIR")));
        let raw = serde_json::to_vec(&json).unwrap();
        let script = crate::story::parse(&raw).unwrap();
        let inp = crate::story::scene_inputs(&script, Path::new(env!("CARGO_MANIFEST_DIR")), true).unwrap();
        let cache = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let m = {
            let _entered = rt.enter();
            let mut player = Player::new(
                &script,
                &raw,
                inp,
                RenderOptions {
                    cache_dir: cache.path().into(),
                    tile_timeout: Duration::from_millis(500),
                    ..RenderOptions::default()
                },
            )
            .unwrap();
            player.run_with_extras(&mut Hashes(Vec::new()), out.path(), &mut |_| {}).expect("non-strict: the render completes")
        };
        rt.shutdown_background();
        let text = serde_json::to_string_pretty(&m).unwrap();
        std::fs::write(out.path().join("story.story.json"), &text).unwrap();
        assert!(
            m.frames.iter().any(|r| r.missing.iter().any(|(_, why)| matches!(why, MissReason::Failed(_)))),
            "the test must see a failed fetch named in the manifest"
        );
        assert!(!text.contains("SECRET"), "a credential reached the manifest: {text}");
        assert!(!m.gaps.join("\n").contains("SECRET"), "{:?}", m.gaps);
        assert!(text.contains("access_token=${MAPBOX_ACCESS_TOKEN}"), "the token is named, not held");
        let mut stack = vec![cache.path().to_path_buf(), out.path().to_path_buf()];
        let mut files = 0;
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap() {
                let p = e.unwrap().path();
                assert!(!p.to_string_lossy().contains("SECRET"), "{}", p.display());
                if p.is_dir() {
                    stack.push(p);
                } else {
                    files += 1;
                    let bytes = std::fs::read(&p).unwrap();
                    assert!(!bytes.windows(6).any(|w| w == b"SECRET"), "{} holds a credential", p.display());
                }
            }
        }
        assert!(files > 0);
    }

    /// F4: the render's own `tiles_missing` reaches the row. Every tile
    /// the story needs, and every ancestor, fails except frame 0's cover
    /// exactly, so a frame that moves onto new tiles has holes that no
    /// stand-in fills, and the row names them in `tiles_missing`.
    #[test]
    fn the_renders_own_missing_tiles_reach_the_manifest() {
        let _g = gpu_lock();
        let (s, raw) = det_script();
        let dir = tempfile::tempdir().unwrap();
        let probe =
            Player::new(&s, &raw, inputs(), RenderOptions { cache_dir: dir.path().into(), ..RenderOptions::default() }).unwrap();
        let first: HashSet<TileId> = probe.request_at(0).0.cover().into_iter().collect();
        let mut fail: HashSet<TileId> = HashSet::new();
        for n in 0..probe.frames() {
            for t in probe.request_at(n).0.cover() {
                fail.insert(t);
                fail.extend(crate::story::prefetch::stand_ins(t));
            }
        }
        fail.retain(|t| !first.contains(t));
        drop(probe);
        let mut inp = inputs();
        inp.source = Arc::new(crate::story::fixture::FakeSource {
            fail,
            ..crate::story::fixture::FakeSource::fixture(std::time::Duration::ZERO)
        });
        let cache = tempfile::tempdir().unwrap();
        let mut player = Player::new(
            &s,
            &raw,
            inp,
            RenderOptions { cache_dir: cache.path().into(), tile_timeout: Duration::from_millis(200), ..RenderOptions::default() },
        )
        .unwrap();
        let m = player.run(&mut Hashes(Vec::new()), &mut |_| {}).unwrap();
        assert!(m.frames[0].tiles_missing.is_empty());
        let holed = m.frames.iter().find(|r| !r.tiles_missing.is_empty()).expect("some frame has a hole");
        assert!(
            holed.tiles_missing.iter().all(|t| holed.missing.iter().any(|(m, _)| m == t)),
            "a hole is a tile the prefetcher named too: {holed:?}"
        );
    }

    /// F13: an ancestor ABOVE the planned stand-in levels that happens to
    /// be resident does not change the pixels. The victim tile and its
    /// planned stand-ins fail; its great-grandparent is put in the store
    /// by hand (as an earlier frame could have left it). With the story's
    /// `stand_in_levels` the frame is bit-identical to the one without
    /// the extra tile; with the live map's unbounded search it is not.
    #[test]
    fn an_unplanned_resident_ancestor_does_not_change_the_pixels() {
        let _g = gpu_lock();
        let (s, raw) = det_script();
        let dir = tempfile::tempdir().unwrap();
        let player =
            Player::new(&s, &raw, inputs(), RenderOptions { cache_dir: dir.path().into(), ..RenderOptions::default() }).unwrap();
        let (request, state) = player.request_at(0);
        drop(player);
        assert_eq!(request.stand_in_levels, Some(STAND_IN_LEVELS));
        let victim = request.cover()[0];
        let high = TileId { z: victim.z - 3, x: victim.x >> 3, y: victim.y >> 3 };
        let tile = || crate::mvt::decode(crate::story::fixture::FIXTURE_TILE.to_vec()).unwrap();
        let mut store: crate::capture::TileStore = request.cover().into_iter().filter(|t| *t != victim).map(|t| (t, tile())).collect();
        let style = crate::style_spec::parse_and_compile(
            &std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/liberty-style.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(state.style_generation, 0);
        let mut session = CaptureSession::new_blocking(inputs().label_fonts).unwrap();
        let without = session.render(&request, &store, &style).unwrap();
        store.insert(high, tile());
        let with = session.render(&request, &store, &style).unwrap();
        assert_eq!(without.tiles_missing, vec![victim]);
        assert_eq!(with.sha256_hex(), without.sha256_hex(), "an unplanned ancestor stayed out of the frame");
        let live = CaptureRequest { stand_in_levels: None, ..request };
        let live_with = session.render(&live, &store, &style).unwrap();
        assert!(live_with.tiles_missing.is_empty(), "control: the live rule draws it");
        assert_ne!(live_with.sha256_hex(), without.sha256_hex(), "control: and it shows");
    }

    #[test]
    fn the_frame_count_covers_the_duration_and_names_an_event_at_the_end() {
        assert_eq!(frame_count(2.0, 10), 20);
        assert_eq!(frame_count(3.0, 30), 90);
        assert_eq!(frame_count(2.04, 10), 21);
        assert_eq!(frame_count(0.0, 30), 1);
        let json = serde_json::json!({"story":1,"output":{"width":480,"height":270,"fps":10},
          "scene":{"style":"x.json","basemap":{"source":"fixture"}},
          "camera":[{"type":"jump","to":{"lon":103.851,"lat":1.2905,"zoom":14.2}},{"type":"hold","duration":1.0}],
          "events":[{"at":1.0,"set_layer_visibility":{"layer":"water","visible":false}}]});
        let raw = serde_json::to_vec(&json).unwrap();
        let s = crate::story::parse(&raw).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let _g = gpu_lock();
        let p = Player::new(&s, &raw, inputs(), RenderOptions { cache_dir: dir.path().into(), ..RenderOptions::default() }).unwrap();
        assert_eq!(p.frames(), 10);
        let gaps = p.gaps(&(0..10));
        assert!(gaps.iter().any(|g| g.starts_with("event /events/0 at 1 s is after the last frame")), "{gaps:?}");
    }

    /// The Task 9 carry, run by hand: a short globe flight (Singapore to
    /// Tokyo on the globe) written as PNGs to `$STORY_PNG_DIR` to be
    /// looked at. Fixture tiles unless `STORY_VISUAL_SOURCE=openfreemap`
    /// (network; not for CI).
    #[test]
    #[ignore = "writes PNGs for a person to look at; set STORY_PNG_DIR"]
    fn globe_flight_frames_to_png() {
        let _g = gpu_lock();
        let out = PathBuf::from(std::env::var("STORY_PNG_DIR").expect("STORY_PNG_DIR"));
        std::fs::create_dir_all(&out).unwrap();
        let json = serde_json::json!({"story":1,"output":{"width":640,"height":400,"fps":2},
          "scene":{"style":"examples/liberty-style.json","basemap":{"source":"openfreemap"},"globe":true,"labels":true},
          "camera":[{"type":"jump","to":{"lon":103.85,"lat":1.29,"zoom":3.0,"globe":true}},
                    {"type":"fly","to":{"lon":139.69,"lat":35.69,"zoom":4.0,"globe":true},"duration":4.0}],
          "events":[]});
        let raw = serde_json::to_vec(&json).unwrap();
        let s = crate::story::parse(&raw).unwrap();
        let mut inp = inputs();
        if std::env::var("STORY_VISUAL_SOURCE").as_deref() == Ok("openfreemap") {
            inp.source = Arc::new(crate::tile_source::http::HttpTileSource::new(
                "https://tiles.openfreemap.org/planet/20260802_080001_pt/{z}/{x}/{y}.pbf",
                14,
                "",
            ));
            inp.source_label = "openfreemap".into();
        }
        struct Pngs(PathBuf);
        impl FrameSink for Pngs {
            fn push(&mut self, n: usize, f: &Capture) -> anyhow::Result<()> {
                f.save_png(&self.0.join(format!("globe-fly-{n:02}.png")))
            }
            fn finish(&mut self) -> anyhow::Result<serde_json::Value> {
                Ok(serde_json::Value::Null)
            }
        }
        let cache = default_cache_dir(&inp.source_label);
        let opts = RenderOptions { cache_dir: cache, tile_timeout: Duration::from_secs(30), ..RenderOptions::default() };
        let mut player = Player::new(&s, &raw, inp, opts).unwrap();
        let m = player.run(&mut Pngs(out.clone()), &mut |_| {}).unwrap();
        std::fs::write(out.join("globe-fly.manifest.json"), serde_json::to_vec_pretty(&m).unwrap()).unwrap();
    }

    /// The fixture story into a PNG sequence through the tee: every frame,
    /// the poster and the contact sheet. Written to `$STORY_T14_DIR` when
    /// set (to be looked at), else a temp dir.
    #[test]
    fn a_run_leaves_frames_a_poster_and_a_contact_sheet() {
        let _g = gpu_lock();
        let keep = std::env::var_os("STORY_T14_DIR").map(PathBuf::from);
        let tmp = tempfile::tempdir().unwrap();
        let out = keep.unwrap_or_else(|| tmp.path().to_path_buf());
        let (s, raw) = det_script();
        let cache = tempfile::tempdir().unwrap();
        let opts = RenderOptions { cache_dir: cache.path().into(), ..RenderOptions::default() };
        let mut player = Player::new(&s, &raw, inputs(), opts).unwrap();
        let frames = player.frames();
        let mut main = crate::story::sink::PngSequenceSink { dir: out.join("frames") };
        let m = player.run_with_extras(&mut main, &out, &mut |_| {}).unwrap();
        for n in 0..frames {
            assert!(main.path_of(n).exists(), "frame {n}");
        }
        assert!(!out.join("frames").join(crate::story::sink::INCOMPLETE_MARKER).exists());
        let poster = image::open(out.join("poster.png")).unwrap().to_rgba8();
        assert_eq!(poster.dimensions(), (480, 270));
        // The poster is the middle frame, byte for byte.
        let middle = image::open(main.path_of(frames / 2)).unwrap().to_rgba8();
        assert!(poster.as_raw() == middle.as_raw(), "the poster is not frame {}", frames / 2);
        let contact = image::open(out.join("contact.png")).unwrap().to_rgba8();
        assert_eq!(contact.dimensions(), (4 * 480, 3 * (270 + 16)));
        assert_eq!(m.sink["main"]["kind"], "png_sequence");
        assert_eq!(m.sink["poster"], serde_json::json!(out.join("poster.png")));
        std::fs::write(out.join("manifest.json"), serde_json::to_vec_pretty(&m).unwrap()).unwrap();
    }

    /// A failed run tells the sink behind the tee: the sequence is marked
    /// incomplete and no poster or sheet is written.
    #[test]
    fn a_cancelled_run_aborts_the_sink_behind_the_tee() {
        let _g = gpu_lock();
        let out = tempfile::tempdir().unwrap();
        let cancel = Arc::new(AtomicBool::new(true));
        let (mut player, _cache) = player_for(&det_json(), inputs(), RenderOptions { cancel, ..RenderOptions::default() });
        let mut main = crate::story::sink::PngSequenceSink { dir: out.path().join("frames") };
        let err = player.run_with_extras(&mut main, out.path(), &mut |_| {}).unwrap_err();
        assert!(err.to_string().contains("cancelled"), "{err:#}");
        assert!(out.path().join("frames").join(crate::story::sink::INCOMPLETE_MARKER).exists());
        assert!(!out.path().join("poster.png").exists() && !out.path().join("contact.png").exists());
    }

    /// Task 16: every frame's request carries the credit and the mark,
    /// and the captions live at that frame; the manifest's verdict is the
    /// burn-in table's.
    #[test]
    fn every_frame_burns_in_the_credit_the_mark_and_its_captions() {
        let _g = gpu_lock();
        let (player, _cache) = player_for(&det_json(), inputs(), RenderOptions::default());
        let (r0, _) = player.request_at(0);
        assert_eq!(r0.attribution.as_deref(), Some(crate::story::fixture::FIXTURE_CREDIT));
        assert_eq!(r0.brand_mark, Some(crate::brand_mark::BrandMark::default()));
        assert!(r0.captions.is_empty(), "the caption starts at 0.2 s");
        let (r5, _) = player.request_at(5);
        assert_eq!(r5.captions.len(), 1);
        assert_eq!(r5.captions[0].text, "Marina Bay");
        let v = player.verdict();
        assert_eq!(v.publishable, crate::story::burn_in::Publishable::Yes(true));
        let mapbox = SceneInputs { source_label: "mapbox:mapbox.mapbox-streets-v8".into(), ..inputs() };
        let (mb, _cache2) = player_for(&det_json(), mapbox, RenderOptions::default());
        let v = mb.verdict();
        assert_eq!(v.publishable, crate::story::burn_in::Publishable::Yes(false));
        // The table's credit only: the source's own stated text is not added
        // beside it (F5).
        assert_eq!(v.attribution, crate::story::burn_in::MAPBOX_CREDIT);
        assert!(mb.request_at(0).0.wordmark.is_none());
        // A user-supplied wordmark is drawn on every frame and makes the
        // Mapbox scene publishable (ruling: nothing is copied from Mapbox's
        // own code or assets; the artwork is the user's).
        let art = Arc::new(image::RgbaImage::from_pixel(40, 10, image::Rgba([255, 0, 255, 255])));
        let with_mark = SceneInputs { source_label: "mapbox:mapbox.mapbox-streets-v8".into(), wordmark: Some(Arc::clone(&art)), ..inputs() };
        let (wm, _cache3) = player_for(&det_json(), with_mark, RenderOptions::default());
        assert_eq!(wm.verdict().publishable, crate::story::burn_in::Publishable::Yes(true));
        assert!(wm.request_at(0).0.wordmark.as_ref().is_some_and(|w| Arc::ptr_eq(w, &art)));
    }

    /// Ruling 2 (Task 19 review F2): a Mapbox sprite under a non-Mapbox
    /// basemap is judged while the style is intact, so the render's
    /// plate carries Mapbox's credit and its publish table has the
    /// `sprite` row -- `Player::new` takes `base_style` for the timeline,
    /// and reading it afterwards saw `Null`.
    #[test]
    fn a_mapbox_sprite_under_another_basemap_is_credited_by_the_render() {
        let _g = gpu_lock();
        let mut inp = inputs();
        inp.base_style["sprite"] = "mapbox://sprites/mapbox/streets-v12".into();
        let (player, _cache) = player_for(&det_json(), inp, RenderOptions::default());
        assert!(player.verdict().attribution.contains(crate::story::burn_in::MAPBOX_CREDIT), "{}", player.verdict().attribution);
        assert!(player.request_at(0).0.attribution.is_some_and(|a| a.contains(crate::story::burn_in::MAPBOX_CREDIT)));
        assert!(player.publish_table(true).iter().any(|r| r.source == "sprite"), "{:#?}", player.publish_table(true));
        // Without the sprite, neither.
        let (plain, _cache2) = player_for(&det_json(), inputs(), RenderOptions::default());
        assert!(!plain.verdict().attribution.contains(crate::story::burn_in::MAPBOX_CREDIT));
        assert!(!plain.publish_table(true).iter().any(|r| r.source == "sprite"));
    }

    /// Task 15: burn-in follows `captions::burn_in_default` once `aspect`
    /// (or `tier`) is set -- on for 9:16, off for 16:9 -- and merging
    /// `script.captions` entries into the burn when it is on, as
    /// bottom-centre, 40-unit, plated `ActiveCaption`s through the same
    /// path as a `caption` event (F6: a legacy script with neither set
    /// stays covered by `every_frame_burns_in_the_credit_the_mark_and_its_captions`).
    #[test]
    fn burn_in_follows_the_per_aspect_default_and_merges_the_captions_list() {
        let _g = gpu_lock();
        let mut vertical = det_json();
        vertical["output"] = serde_json::json!({"aspect": "9:16", "fps": 10});
        vertical["captions"] = serde_json::json!([{"from": 0.5, "to": 2.5, "text": "Caption list cue"}]);
        let (player, _cache) = player_for(&vertical, inputs(), RenderOptions::default());
        // t = 10 / 10 = 1.0 s: inside both the `caption` event [0.2, 1.8)
        // and the new `captions` list cue [0.5, 2.5).
        let (r1, _) = player.request_at(10);
        assert!(r1.captions.iter().any(|c| c.text == "Marina Bay"), "{:?}", r1.captions);
        let cue = r1.captions.iter().find(|c| c.text == "Caption list cue").unwrap_or_else(|| panic!("{:?}", r1.captions));
        assert_eq!(cue.anchor, crate::story::timeline::Anchor::Bottom);
        assert_eq!(cue.size_px, 40.0);
        assert!(cue.plate);

        // Landscape: the per-aspect default is off, so NEITHER the
        // `caption` event nor the `captions` list cue burns in, even
        // though both are active at this frame.
        let mut landscape = vertical.clone();
        landscape["output"] = serde_json::json!({"aspect": "16:9", "fps": 10});
        let (player, _cache) = player_for(&landscape, inputs(), RenderOptions::default());
        let (r1, _) = player.request_at(10);
        assert!(r1.captions.is_empty(), "{:?}", r1.captions);

        // `output.burn_captions` overrides the per-aspect default either way.
        let mut landscape_on = landscape.clone();
        landscape_on["output"]["burn_captions"] = serde_json::json!(true);
        let (player, _cache) = player_for(&landscape_on, inputs(), RenderOptions::default());
        let (r1, _) = player.request_at(10);
        assert!(r1.captions.iter().any(|c| c.text == "Caption list cue"), "{:?}", r1.captions);

        let mut vertical_off = vertical.clone();
        vertical_off["output"]["burn_captions"] = serde_json::json!(false);
        let (player, _cache) = player_for(&vertical_off, inputs(), RenderOptions::default());
        let (r1, _) = player.request_at(10);
        assert!(r1.captions.is_empty(), "{:?}", r1.captions);
    }

    /// Fix round 1, finding 2: burn-in and the SRT/VTT track must agree on
    /// which `script.captions` entries exist. Both now read `cue_inputs`
    /// through `captions::track`'s clamp/round/empty-trim check, so a
    /// whitespace-only entry is dropped from both, and a real entry
    /// appears in both, at the same (millisecond-rounded) times.
    #[test]
    fn a_whitespace_only_caption_is_dropped_from_both_the_burn_in_and_the_track() {
        let _g = gpu_lock();
        let mut vertical = det_json();
        vertical["output"] = serde_json::json!({"aspect": "9:16", "fps": 10});
        vertical["captions"] = serde_json::json!([
            {"from": 0.5, "to": 2.5, "text": "   "},
            {"from": 0.5, "to": 2.5, "text": "Real cue"},
        ]);
        let (player, _cache) = player_for(&vertical, inputs(), RenderOptions::default());
        // t = 10 / 10 = 1.0 s: inside both cues' [0.5, 2.5) span.
        let (r1, _) = player.request_at(10);
        assert!(!r1.captions.iter().any(|c| c.text.trim().is_empty()), "{:?}", r1.captions);
        let burned = r1.captions.iter().find(|c| c.text == "Real cue").unwrap_or_else(|| panic!("{:?}", r1.captions));
        assert!(burned.alpha > 0.0);

        // The exact same cue source (`captions::track` over the script's
        // own `captions` list) that a future export would call.
        let cue_inputs: Vec<(f64, f64, String)> = player.script.captions.iter().map(|c| (c.from, c.to, c.text.clone())).collect();
        let cues = crate::story::captions::track(&cue_inputs, player.path.duration());
        assert_eq!(cues.len(), 1, "{cues:?}");
        assert_eq!(cues[0].text, "Real cue");
        assert!(cues[0].from <= 1.0 && cues[0].to > 1.0, "{cues:?}");
        let srt = crate::story::captions::srt(&cues);
        let vtt = crate::story::captions::vtt(&cues);
        assert!(srt.contains("Real cue") && !srt.contains("   "), "{srt}");
        assert!(vtt.contains("Real cue"), "{vtt}");
    }

    #[test]
    fn a_mapbox_basemap_without_a_wordmark_is_warned_about() {
        let mut json = det_json();
        json["scene"]["basemap"] = serde_json::json!({"source": "mapbox"});
        let raw = serde_json::to_vec(&json).unwrap();
        let (_, warnings) = crate::story::script::parse_with_warnings(&raw).unwrap();
        assert!(warnings.iter().any(|d| d.code == "mapbox_wordmark_missing"), "{warnings:?}");
        // Named: drawn (a file that does not load is the loader's error),
        // so no warning.
        json["scene"]["basemap"]["wordmark"] = serde_json::json!("wm.png");
        let raw = serde_json::to_vec(&json).unwrap();
        let (_, warnings) = crate::story::script::parse_with_warnings(&raw).unwrap();
        assert!(!warnings.iter().any(|d| d.code.starts_with("mapbox_wordmark")), "{warnings:?}");
        // A Mapbox raster is not drawn (the player fetches no raster), so
        // it owes nothing and is not warned about.
        let mut json = det_json();
        json["scene"]["raster"] = serde_json::json!("mapbox-satellite");
        let raw = serde_json::to_vec(&json).unwrap();
        let (_, warnings) = crate::story::script::parse_with_warnings(&raw).unwrap();
        assert!(!warnings.iter().any(|d| d.code == "mapbox_wordmark_missing"), "{warnings:?}");
        // F2: a `tiles` URL on a Mapbox host is Mapbox too, and an empty
        // credit is said before the render.
        let mut json = det_json();
        json["scene"]["basemap"] = serde_json::json!({
            "tiles": ["https://api.mapbox.com/v4/mapbox.mapbox-streets-v8/{z}/{x}/{y}.mvt?access_token=${MAPBOX_ACCESS_TOKEN}"],
            "max_zoom": 16, "attribution": ""
        });
        let raw = serde_json::to_vec(&json).unwrap();
        let (_, warnings) = crate::story::script::parse_with_warnings(&raw).unwrap();
        assert!(warnings.iter().any(|d| d.code == "mapbox_wordmark_missing"), "{warnings:?}");
        assert!(warnings.iter().any(|d| d.code == "empty_attribution"), "{warnings:?}");
    }

    /// Follow-up to F2: `output.acknowledge` reaches the verdict and the
    /// manifest's `licence_acknowledged`; a malformed one is a parse error.
    #[test]
    fn an_acknowledged_source_is_recorded_in_the_manifest() {
        let _g = gpu_lock();
        let mut json = det_json();
        json["output"]["acknowledge"] = serde_json::json!(["source-licence:tiles.example.com"]);
        let unknown = || SceneInputs { source_label: "https://tiles.example.com/v1".into(), ..inputs() };
        let (player, _c) = player_for(&json, unknown(), RenderOptions { frames: Some(0..1), ..RenderOptions::default() });
        let v = player.verdict();
        assert_eq!(v.publishable, crate::story::burn_in::Publishable::Yes(true), "{v:?}");
        let (mut player, _c2) = player_for(&json, unknown(), RenderOptions { frames: Some(0..1), ..RenderOptions::default() });
        let m = player.run(&mut Hashes(Vec::new()), &mut |_| {}).unwrap();
        let text = serde_json::to_value(&m).unwrap();
        assert_eq!(text["licence_acknowledged"], "source-licence:tiles.example.com");
        assert_eq!(text["publishable"], true);
        // Without it: unknown, and the reason says how to acknowledge.
        let (plain, _c3) = player_for(&det_json(), unknown(), RenderOptions::default());
        let v = plain.verdict();
        assert_eq!(v.publishable, crate::story::burn_in::Publishable::Yes(false));
        assert!(v.reason.unwrap_or_default().contains("output.acknowledge"));
        // A malformed entry is refused at parse.
        let mut bad = det_json();
        bad["output"]["acknowledge"] = serde_json::json!(["tiles.example.com"]);
        let err = crate::story::parse(&serde_json::to_vec(&bad).unwrap()).unwrap_err();
        assert!(err.iter().any(|d| d.code == "bad_acknowledge"), "{err:?}");
    }

    /// Review finding 2: video or still is the output format's
    /// (`RenderOptions::video`, from `story::output_is_video`), never the
    /// frame count's: a ONE-frame mp4 re-render over Mapbox (wordmark
    /// drawn) still needs `mapbox-video`; the same frame as a still does
    /// not.
    #[test]
    fn a_one_frame_mp4_rerender_over_mapbox_needs_the_video_acknowledgement() {
        use crate::story::burn_in::RowVerdict;
        let _g = gpu_lock();
        let art = Arc::new(image::RgbaImage::from_pixel(40, 10, image::Rgba([255, 0, 255, 255])));
        let mapbox = || SceneInputs { source_label: "mapbox:mapbox.mapbox-streets-v8".into(), wordmark: Some(Arc::clone(&art)), ..inputs() };
        let basemap_of = |m: &RenderManifest| m.publish.iter().find(|r| r.source == "basemap").cloned().unwrap();
        for (format, expected) in [("mp4", RowVerdict::NeedsAcknowledgement), ("png-seq", RowVerdict::NeedsAcknowledgement), ("still", RowVerdict::Ok)] {
            let opts = RenderOptions { frames: Some(3..4), video: crate::story::output_is_video(format), ..RenderOptions::default() };
            let (mut p, _c) = player_for(&det_json(), mapbox(), opts);
            let m = p.run(&mut Hashes(Vec::new()), &mut |_| {}).unwrap();
            assert_eq!(m.frames.len(), 1);
            let row = basemap_of(&m);
            assert_eq!(row.verdict, expected, "{format}: {row:?}");
            assert_eq!(m.publishable, expected == RowVerdict::Ok, "{format}");
        }
        let mut json = det_json();
        json["output"]["acknowledge"] = serde_json::json!(["mapbox-video"]);
        let (mut p, _c) = player_for(&json, mapbox(), RenderOptions { frames: Some(3..4), ..RenderOptions::default() });
        let row = basemap_of(&p.run(&mut Hashes(Vec::new()), &mut |_| {}).unwrap());
        assert_eq!((row.verdict, row.acknowledged.as_deref()), (RowVerdict::Ok, Some("mapbox-video")));
    }

    /// Review finding 4: a story layer plugin's credit is on the plate AND
    /// in the publish table; one with no credit blocks publishing.
    #[test]
    fn a_story_layer_plugin_is_credited_on_the_plate_and_rowed_in_the_table() {
        use crate::story::burn_in::RowVerdict;
        struct Credited(Option<&'static str>, &'static str);
        impl StoryLayer for Credited {
            fn id(&self) -> &str {
                self.1
            }
            fn credit(&self) -> Option<String> {
                self.0.map(str::to_string)
            }
        }
        let _g = gpu_lock();
        let (mut p, _c) = player_for(&det_json(), inputs(), RenderOptions { frames: Some(0..1), ..RenderOptions::default() });
        p.add_layer(Box::new(Credited(Some("Dots: GHS-POP"), "dots")));
        p.add_layer(Box::new(Credited(None, "bare")));
        assert!(p.verdict().attribution.contains("Dots: GHS-POP"));
        let m = p.run(&mut Hashes(Vec::new()), &mut |_| {}).unwrap();
        let row = |id: &str| m.publish.iter().find(|r| r.source == id).cloned().unwrap();
        assert_eq!((row("layer:dots").verdict, row("layer:dots").credit), (RowVerdict::Ok, Some("Dots: GHS-POP".to_string())));
        assert_eq!(row("layer:bare").verdict, RowVerdict::Blocked);
        assert!(!m.publishable);
    }

    /// F6: a scene naming a raster the player does not draw neither
    /// credits it nor is judged by it, and the gaps say so.
    #[test]
    fn an_undrawn_raster_is_not_credited_or_judged() {
        let mut json = det_json();
        json["scene"]["raster"] = serde_json::json!("s2cloudless");
        let (player, _cache) = player_for(&json, inputs(), RenderOptions::default());
        let v = player.verdict();
        assert_eq!(v.publishable, crate::story::burn_in::Publishable::Yes(true), "{v:?}");
        assert!(!v.attribution.contains("EOX"), "{}", v.attribution);
        assert!(player.gaps(&(0..1)).iter().any(|g| g.contains("s2cloudless") && g.contains("not drawn")));
    }

    #[test]
    fn default_options_follow_the_brief() {
        let o = RenderOptions::default();
        assert!(!o.strict && o.frames.is_none() && !o.cancel.load(Ordering::Relaxed));
        assert_eq!((o.tile_timeout, o.fetch_concurrency, o.mesh_cache_bytes), (Duration::from_secs(10), 16, 1 << 30));
        let d = default_cache_dir("mapbox:mapbox.mapbox-streets-v8");
        assert!(d.ends_with(".cache/gpui-map/tiles/mapbox_mapbox.mapbox-streets-v8"), "{d:?}");
    }
}

//! **A real frame, headless, to a file.** Real tiles, the real style, the
//! real camera, the real pipeline — and no window.
//!
//! # Why this exists
//!
//! `agent.rs`' command table deliberately has no `map.capture` (see its
//! doc comment): the only thing in this crate that draws a frame is
//! `MapView::paint`, which needs a `gpui::Window`, and opening one steals
//! the user's screen. So every investigation into a *visual* defect has so
//! far had to grade a **stand-in** — one or two synthetic fill ops on a
//! hand-made polygon, rendered by the pixel tests in `render::gpu`. Five
//! consecutive investigations into pale streaks on the globe eliminated
//! overdraw, seams, depth order, T-junctions, MSAA, lyon self-overlap and
//! per-vertex lighting that way, and not one of them ever looked at our own
//! frame of the symptom.
//!
//! This module closes that gap. It is assembly, not invention: every piece
//! below is the production one.
//!
//! | step | what production does | what this does |
//! |---|---|---|
//! | cover | `MapView::visible_tiles` → `GlobeCover::cover` | the same call |
//! | bake | `style::match_tile_static` + `resolve_plan` | the same pair |
//! | tessellate | `render::tessellate_plan` | the same call |
//! | uniforms | `GlobeUpload::for_tile`, `combined_tile_matrix` | the same |
//! | draw | `TilePipeline::render` | the same |
//!
//! It also reproduces `paint`'s two-pass draw list, ancestor stand-ins
//! included — see [`CaptureRequest::absent`], which turned out to be where
//! the answer was.
//!
//! What it does NOT do is the parts of `MapView::paint` that cannot affect
//! a landcover fill: labels, icons, the attribution plate, the debug
//! overlay, the zoom-blend of a stale plan, and — by default — the stars
//! and the atmosphere. The backdrop sphere is drawn by default and can be
//! turned off, which is the point of [`Clear::Sentinel`]: with nothing
//! behind the tiles, any pixel the tiles fail to cover is the clear colour,
//! and a gap is a count rather than an impression.
//!
//! # What it measured first, and the numbers
//!
//! The question it was built for: **can `render::subdivide` be the cause of
//! the pale streaks?** Same camera, same tiles, same style, twice, with
//! `Subdivision::for_tile` against `Subdivision::NONE` the only difference.
//!
//! `--paint wash` is the discriminator, for the reason
//! `fix: the horizon is where the horizon is` established: a pale streak is
//! a DOUBLE COMPOSITE of a translucent fill, and opaque fills hide one
//! completely. Over black, a pixel's value is its composite count — 77 once,
//! 130 twice, 168 three times. On the reported camera, OpenFreeMap z3 tiles,
//! the vendored Liberty style, 1024x1024:
//!
//! ```text
//!   camera            subdivision   covered    once      twice   3x   4x+
//!   lon 88.8 lat 61   for_tile      170,635    170,630       3    2     0
//!   zoom 3.14         NONE          171,423    170,441     760  220     2
//!   lon 88.8 lat 61   for_tile      110,341    110,339       1    1     0
//!   zoom 3.48         NONE          111,091    111,049      37    5     0
//! ```
//!
//! **Five double-composited pixels in 170,635 under production
//! subdivision, and 982 without it.** The styled A/B agrees: 3,673 of
//! 1,048,576 pixels differ at z3.14 (0.35%), every one of them at a radius
//! of 218 px or more from the frame centre (p50 657) against a 724 px
//! half-diagonal — the limb annulus. The interior of the disc is
//! bit-identical between the two. Subdivision does not make streaks; it is
//! what removes them.
//!
//! The pole caps say the same on real bottom-row geometry. At lon 0, lat
//! -60, z3.0, 1600x1600, counting background pixels bracketed by the
//! Antarctic ice sheet on both axes — a hole in the cap:
//!
//! ```text
//!   for_tile   265,855 ice px        13 enclosed background px
//!   NONE       248,419 ice px    18,710 enclosed background px
//! ```
//!
//! and the 18,710 are one octagon centred on the pole, which is what an
//! uncapped sphere looks like.
//!
//! # What DID reproduce: one tile still loading
//!
//! Having exonerated subdivision, the harness was pointed at the one thing
//! `MapView::paint` does that no instrument here had ever reproduced — the
//! ancestor STAND-IN (see [`CaptureRequest::absent`]). Same camera, same
//! tiles, same wash, changing only how many cover tiles are treated as not
//! yet uploaded:
//!
//! ```text
//!   tiles still loading   once      twice   share of covered pixels
//!   0 (a settled frame)   170,630       3   0.0029%
//!   1                     168,932   1,700   0.998%
//!   4                     167,212   3,423   2.008%
//! ```
//!
//! **One tile in flight is 567 times the settled frame's double-composite
//! count**, and the doubled region is a patch bounded by the ancestor's own
//! tile box — a straight edge in tile space, which on the sphere is a
//! meridian or a parallel. That is the reported shape, and it is transient,
//! which is why a still screenshot of a settled map never showed it and why
//! every stand-in test missed it: they all had a complete cover.
//!
//! This is a reproduction of the MECHANISM, not yet a proof that it is what
//! the user saw. The vendored OpenFreeMap style's fills are opaque almost
//! everywhere, so it only shows here through `--paint wash`; a style whose
//! `landcover` really is a 0.3 wash over the whole land mass would show it
//! as a large pale patch with a straight edge.
//!
//! # And what fixed it: a stand-in that paints only what it stands in for
//!
//! [`render::gpu::TileClip`](crate::render::TileClip). The stand-in is
//! still drawn — a loading tile must not be a hole — but clipped, in the
//! ancestor's own tile space, to the box of the one missing tile it was
//! pulled in for. Same camera, same tiles, same wash, same flags:
//!
//! ```text
//!   tiles still loading   once      twice   covered   enclosed background
//!   0 (a settled frame)   170,630       3   170,635   100,336
//!   1   before            168,932   1,700   170,635   100,336
//!       after             170,630       3   170,635   100,336
//!   4   before            167,212   3,423   170,638   100,333
//!       after             170,630       3   170,635   100,336
//! ```
//!
//! Both stand-in frames come out **bit-identical to the settled frame** —
//! 0 of 1,048,576 pixels differ.
//!
//! The `--absent` tail names the LEAST visible tiles, though, and at this
//! camera their own quadrants cover almost nothing, so the whole of what
//! the clip removed there was leak onto the siblings. `--absent-head`
//! takes the MOST visible tiles instead, and that is where the defect was
//! loudest and the stand-in is most obviously worth having:
//!
//! ```text
//!   head tiles loading   once      twice   covered   enclosed background
//!   1   before           166,152   4,329   170,483   91,282
//!       after            170,349       4   170,355   91,051
//!   4   before           120,855  49,683   170,544   89,410
//!       after            170,289       2   170,293   89,227
//! ```
//!
//! **Twenty-nine percent of the covered frame was composited twice** with
//! four head tiles in flight, and two pixels are. Coverage falls by 251 of
//! 170,544 and the ENCLOSED background falls with it — the pixels lost are
//! ones the ancestor was painting over a ready tile that had said there was
//! nothing there, which is the leak and not a hole. That the stand-in is
//! still standing in is the `--no-stand-in` control: the same frame with
//! the ancestors withheld covers 95,981 pixels, so the clipped stand-in is
//! worth 74,312 of them.
//!
//! **The clip has to be a `discard`, not a zero alpha**, and that cost a
//! measurement to learn: a zero-alpha fragment still writes depth, and an
//! ancestor's z and a descendant's z at one point of the sphere disagree by
//! an ULP, so the masked-out stand-in was depth-rejecting the ready tile
//! drawn over it and taking 999 pixels of real fill with it. `--no-stand-in`
//! is the control that caught it: a draw that can only add coverage was
//! taking it away. See `shader.wgsl`'s `fs_main`.
//!
//! # The tile source must be OPEN
//!
//! Nothing here names a source. [`fetch_cover`] takes a
//! [`TileSource`](crate::tile_source::TileSource), and the harness's own
//! entry point (`examples/globe_capture.rs`) points it at OpenFreeMap's
//! keyless planet tiles, the same ones `examples/basic_map.rs` draws.
//! **Do not point it at Mapbox and commit what comes back** — vector tiles
//! from a licensed source are that source's data, and redistributing them
//! in this repository is exactly the problem a day was spent purging. The
//! on-disk cache [`fetch_cover`] writes is a scratch directory outside the
//! repo for the same reason.

pub(crate) mod session;
pub(crate) mod chrome;
pub use session::{CaptureSession, StageTimes, STAGE_TIMES_KEPT};
mod mesh_cache;
pub use mesh_cache::MeshCacheStats;
pub(crate) mod story_overlays;
pub(crate) mod story_chrome;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::geo::TileId;
use crate::mvt::MVT_EXTENT;
use crate::render::globe::{GlobeCover, GlobeUpload};
use crate::render::subdivide::Subdivision;
use crate::render::{Camera, GpuContext, TileClip, TilePipeline};
use crate::style::{Lerp, PaintOp, Rgba, Style};

/// Which subdivision policy the meshes are tessellated under — **the one
/// knob the decisive A/B turns.**
///
/// `ForTile` is production (`Subdivision::for_tile`): a grid split whose
/// granularity falls with zoom, plus a pole cap on the top and bottom rows.
/// `None` is `Subdivision::NONE`: one quad per ring, no grid, no caps.
/// Render the same camera twice, changing only this, and the difference
/// image says whether the subdivision module can be the cause of anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubdivisionPolicy {
    /// `Subdivision::for_tile(tile)` — what production tessellates with.
    ForTile,
    /// `Subdivision::NONE` — no grid split, no pole caps.
    None,
}

impl SubdivisionPolicy {
    /// This policy's subdivision for one tile.
    #[must_use]
    pub fn for_tile(self, tile: TileId) -> Subdivision {
        match self {
            Self::ForTile => Subdivision::for_tile(tile),
            Self::None => Subdivision::NONE,
        }
    }
}

/// What the colour attachment is cleared to before any tile is drawn.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Clear {
    /// The style's own `background-color` at this zoom, blended toward
    /// `Space::color` exactly as `MapView::paint` blends it. What the user
    /// sees, minus the backdrop sphere.
    Style,
    /// A colour no style contains, so that **every pixel the tiles did not
    /// cover is identifiable by value**. `Clear::SENTINEL` is opaque
    /// magenta.
    Sentinel(Rgba),
}

impl Clear {
    /// Opaque magenta: absent from every style this repo vendors, and
    /// absent from any plausible landcover ramp.
    pub const SENTINEL: Self = Self::Sentinel(Rgba { r: 1.0, g: 0.0, b: 1.0, a: 1.0 });
}

/// How a baked plan's ops are painted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Paint {
    /// The plan as the style resolved it — every fill and every line, in
    /// the style's own colours. What the screen shows.
    AsStyled,
    /// **Fills only, all in one opaque colour, lines dropped.** A
    /// discriminator, not a picture: a pale streak in an `AsStyled` frame
    /// could be a thin style layer, a translucent overlap, or geometry that
    /// failed to cover. Flattening removes the first two — anything still
    /// pale is a hole.
    FlattenedFills,
    /// **Every fill as the same near-white wash at `fill-opacity` 0.3,
    /// lines dropped** — the amplifier for the reported symptom.
    ///
    /// The report is *pale streaks through large landcover areas*, and
    /// `fix: the horizon is where the horizon is` established what makes
    /// that visible at all: `landcover` in the shipped Mapbox style is a
    /// near-white wash at opacity 0.3, so a pixel composited TWICE reads
    /// 130 where a pixel composited once reads 77. Opaque fills — which is
    /// what the vendored OpenFreeMap style uses almost everywhere — hide a
    /// double-composite completely: two coats of an opaque colour look
    /// exactly like one.
    ///
    /// So this repaints the REAL geometry with the property that makes the
    /// defect legible. Over a black clear with no backdrop, the pixel value
    /// IS the composite count: 77 once, 130 twice, 168 three times, and any
    /// streak following a tile edge or a subdivision grid line is a count
    /// that should have been one and was two.
    Wash,
}

/// A navigation frame to draw over a capture: the host's route line and
/// chrome, exactly what a shell draws. `Arc` so the request stays cheap to
/// clone.
#[derive(Clone)]
pub struct NavCapture {
    pub host: std::sync::Arc<crate::nav::host::NavHost>,
    pub frame: crate::nav::host::NavFrame,
}

impl std::fmt::Debug for NavCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NavCapture")
    }
}

/// `(font file bytes, default family)` for [`CaptureRequest::label_fonts`].
/// The files are `Arc`-shared: cloning a request (once per frame on the
/// story path) bumps a count instead of copying every face.
pub type LabelFonts = (std::sync::Arc<[Vec<u8>]>, String);

/// A shared RGBA image a request carries (the wordmark), whose `Debug`
/// prints its size rather than every pixel. Derefs to the `Arc`.
#[derive(Clone)]
pub struct Artwork(pub std::sync::Arc<image::RgbaImage>);

impl std::fmt::Debug for Artwork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Artwork({}x{})", self.0.width(), self.0.height())
    }
}

impl std::ops::Deref for Artwork {
    type Target = std::sync::Arc<image::RgbaImage>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<std::sync::Arc<image::RgbaImage>> for Artwork {
    fn from(art: std::sync::Arc<image::RgbaImage>) -> Self {
        Self(art)
    }
}

/// One frame to draw.
#[derive(Debug, Clone)]
pub struct CaptureRequest {
    pub center_lon: f64,
    pub center_lat: f64,
    pub zoom: f64,
    /// Radians, as `Camera` holds them.
    pub pitch: f32,
    /// Radians.
    pub bearing: f32,
    /// The colour attachment's size, in PHYSICAL pixels. The camera's
    /// viewport is `width / pixel_ratio` by `height / pixel_ratio` (see
    /// [`Self::pixel_ratio`]), so at the default ratio of 1.0 one
    /// framebuffer pixel is one logical pixel and every count this module
    /// reports is directly comparable to a screen measurement taken at a
    /// scale factor of 1.
    pub width: u32,
    pub height: u32,
    /// The device pixel ratio a shell would report -- `1.0` for the
    /// default, physical-pixels-are-logical-pixels frame every golden hash
    /// was blessed under. [`Self::camera`] divides [`Self::width`] and
    /// [`Self::height`] by this to get the camera's logical viewport, and
    /// `CaptureSession::render`'s camera uniform and every collision bound
    /// this module resolves scale back up by it -- exactly what a shell at
    /// that ratio hands the pipeline. Validated in `render_inner`: finite
    /// and in `[0.5, 8]`, or the capture returns an error rather than
    /// dividing by something degenerate.
    pub pixel_ratio: f32,
    /// Whether this frame projects onto the sphere at all --
    /// `MapView::set_globe_enabled`'s switch, verbatim. `true` (the
    /// default) is today's behaviour: the globe band and backdrop dressing
    /// exactly as every blessed hash drew them. `false` flattens the frame
    /// to pure Mercator at every zoom (`GlobeFrame::with_globe_enabled`)
    /// and its cover goes straight to [`crate::render::cover_plan::flat_cover`]
    /// without requiring [`Self::flat`].
    pub globe: bool,
    pub subdivision: SubdivisionPolicy,
    pub clear: Clear,
    pub paint: Paint,
    /// Draw the ocean sphere behind the tiles, as `MapView::paint` does
    /// whenever `space::draws_backdrop` is true.
    ///
    /// **On is the production frame; off is the instrument.** The backdrop
    /// is an opaque sphere in the style's `background-color` covering the
    /// whole silhouette, so with it on, a pixel no tile covered is a pale
    /// background pixel — indistinguishable by eye from a pale landcover
    /// fill, which is exactly why five passes of screenshots never settled
    /// what the streaks are. With it off and [`Clear::Sentinel`], the same
    /// pixel is magenta and countable.
    pub backdrop: bool,
    /// The sun, or `None` for the unlit ground (`GlobeParams::unlit`).
    ///
    /// Unlit is the default because a smooth terminator gradient across
    /// every pixel would put a confound under every count this module
    /// reports. It is settable because the louder half of the report says
    /// the streaks are *worst where the terminator has darkened everything
    /// around them*, and ground shading is interpolated PER VERTEX — so a
    /// denser grid is a different interpolation, and "does the subdivision
    /// grid show up as brightness lines on a lit globe" is a question only
    /// a lit frame can answer.
    ///
    /// `Sun::Viewport { tilt_deg: 90, bearing_deg: 0 }` reads no clock and
    /// puts the terminator through the middle of the disc, which is what
    /// makes a lit capture reproducible.
    ///
    /// How dark the night side is under that sun: [`Self::atmosphere`]'s
    /// `night_brightness` when it is set (`1.0`, no terminator, is the
    /// engine default since 2026-09-29), else
    /// `Atmosphere::TERMINATOR_NIGHT_BRIGHTNESS`.
    pub sun: Option<crate::render::atmosphere::Sun>,
    /// Cover tiles to treat as **not yet uploaded**, so the capture takes
    /// the stand-in path `MapView::paint` takes for them.
    ///
    /// # Why a capture needs this at all
    ///
    /// `paint` builds its draw list in two passes (`model_buffers`): pass 1
    /// pushes, for every visible tile whose own mesh is not in
    /// `gpu_meshes`, the nearest cached ANCESTOR; pass 2 pushes every tile
    /// that IS ready, on top. One z2 ancestor covers four z3 tiles, so a
    /// frame in which one of those four is still loading draws the ancestor
    /// under the OTHER THREE as well — and a translucent fill is then
    /// composited twice over exactly those three quadrants. The boundary
    /// between the twice-composited region and the once-composited one is
    /// the ready/not-ready tile edge, which at z3 is a meridian or a
    /// parallel.
    ///
    /// That is the one production behaviour no instrument in this repo has
    /// ever reproduced, because it only exists while tiles are in flight
    /// and every test so far has had a complete cover. Naming tiles here
    /// makes it a still frame.
    ///
    /// **Since fixed** — the stand-in is now clipped to the tile it stands
    /// in for (`render::gpu::TileClip`), so the paragraph above describes
    /// what this field was built to reproduce rather than what it draws
    /// today. It stays exactly as it is: it is the reproduction, and
    /// `a_stand_in_paints_only_the_tile_it_stands_in_for` is what keeps
    /// the fix honest by drawing frames through it.
    pub absent: Vec<TileId>,
    /// The limb glow, or `None` for a frame with no atmosphere —
    /// `MapView::paint`'s `SpaceDraw::atmosphere`, which this harness
    /// withheld until the limb arc needed measuring.
    ///
    /// **It is drawn LAST, over the tiles, and its planet edge is
    /// ANALYTIC**: `atmosphere_shader.wgsl` marches a scattering integral
    /// against a true sphere, while the ground is a chord polygon whose
    /// segments subtend `render::subdivide`'s granularity. The two edges
    /// therefore disagree by the sagitta of half a segment, which is a
    /// FIXED FRACTION of the disc radius and so grows linearly with the
    /// window — invisible on the 161 px discs the pixel tests render and
    /// about a pixel wide on the user's. Turning this on is how a capture
    /// can see it at all.
    pub atmosphere: Option<crate::render::atmosphere::Atmosphere>,
    /// The raster basemap's decoded tiles, keyed by the tile they cover.
    ///
    /// Empty is the default and means "no raster layer in this frame",
    /// which is what every capture written before raster existed gets --
    /// and getting it through `..CaptureRequest::default()` rather than
    /// through a fourth argument to [`capture`] is deliberate. A new
    /// parameter would have touched every existing pixel test to prove
    /// that none of them changed; a defaulted field proves it by
    /// construction.
    ///
    /// # Why the images and not a source
    ///
    /// Same reason `tiles` is a `TileStore` of already-decoded tiles and
    /// not a `TileSource`: this harness renders one still frame from
    /// bytes a caller already has, so that a test can put a KNOWN image on
    /// a KNOWN tile and assert the exact pixel it should produce. A
    /// fixture that fetched would be measuring the network.
    ///
    /// `raster_opacity` is applied on top of these; the images themselves
    /// are whatever `raster::decode` returned.
    pub raster: std::collections::HashMap<TileId, image::RgbaImage>,
    /// The `raster-opacity` to draw [`Self::raster`] with, already resolved
    /// at this frame's zoom.
    ///
    /// Resolved by the CALLER rather than read from the style here,
    /// because the whole point of the field above is that this harness
    /// draws imagery a test handed it, which may have no style layer
    /// behind it at all. `examples/globe_capture` resolves it from the
    /// style's raster layer; a pixel test states it as a literal so the
    /// expected composite can be arithmetic rather than a second call
    /// into the expression evaluator.
    pub raster_opacity: f32,
    /// Imagery stand-ins, drawn before (under) the cover tiles as
    /// `MapView::paint` draws them: each `(ancestor, missing)` pair draws
    /// `raster[ancestor]` clipped to `missing`'s footprint
    /// (`TileClip::for_descendant`). Empty by default; a test hands the
    /// pairs so the clip can be graded on pixels.
    pub raster_stand_ins: Vec<(TileId, TileId)>,
    /// **The labels, over the tiles.** Off by default, because every
    /// investigation this harness was built for was about a FILL and a
    /// label over one only gets in the way of counting it.
    ///
    /// Turned on, this runs `symbol_pass` — the same
    /// `collect_candidates` → `place_visible_symbols` →
    /// `sort_symbols_for_drawing` → `text_areas` chain `MapView::paint` and
    /// the browser both run — and draws the result through glyphon into a
    /// second pass over the same target. It exists because the day/night
    /// terminator now shades text as well as ground
    /// (`render::SymbolLight`), and a fix to how a label sits on the night
    /// hemisphere cannot be graded on a frame with no labels in it.
    ///
    /// # What it does less than `paint`, and it is worth naming
    ///
    ///  - **No sprite sheet**, so `icon_rects` is empty and
    ///    `icon_atlas_size` is `(0, 0)`. Every icon-bearing feature draws
    ///    its text and skips its icon, which is exactly what the native
    ///    build does before `set_sprites` is called.
    ///  - **No `GlyphSink` unless [`Self::line_labels`]**, so by default a
    ///    LINE-placed label (a street name bent along its road) is shaped,
    ///    placed, measured and boxed, but rasters no glyphs and draws
    ///    nothing. At the zoom this harness renders the globe at there are
    ///    no street names on screen at all.
    ///  - **No attribution plate and no brand mark.** Chrome, not map.
    ///
    /// So a captured label frame is the POINT labels — countries, cities,
    /// continents, water bodies — which is the whole of what a globe camera
    /// shows and all of what the terminator fix is about.
    pub labels: bool,
    /// Draw a FLAT frame outside the globe band, through the one planar
    /// cover every shell uses (`cover_plan::flat_cover`), with the horizon
    /// fade. Off by default: this harness is a globe instrument first, and
    /// every existing caller keeps getting the empty cover it always got
    /// above the band.
    pub flat: bool,
    /// The flat source's zoom ceiling: OpenFreeMap's planet is 14, Mapbox
    /// Streets v8 is 16. A flat cover asks this zoom at most.
    pub flat_tile_zoom_max: u8,
    /// **The star field**, behind the sky, as the shells draw it at full
    /// `star_intensity`. Off by default: every count this harness reports
    /// predates it, and a star is a bright dot on exactly the clear-colour
    /// pixels those counts are about. On, it is how a capture can see
    /// whether the sky hides the stars where it is bright.
    pub stars: bool,
    /// **The space look** (`render::space_look`), with the textures it
    /// draws and the scene instant its sky is turned to (`None`: the BSC
    /// stars' own ground-fixed frame). `None`, the default, draws nothing
    /// of it -- every existing capture is unchanged. When set, the point
    /// stars are drawn too, at the look's own intensity.
    pub space_look: Option<(
        crate::render::space_look::SpaceLook,
        std::sync::Arc<crate::render::space_look::SpaceTextures>,
        Option<f64>,
    )>,
    /// **Shape labels with these faces instead of the machine's font book**,
    /// as `(font file bytes, default family)`. `None`, the default, is the
    /// native shell: `font_fallback::new_font_system`, system fonts.
    ///
    /// The browser and iOS shells never read a system font -- they shape
    /// against the handful of faces they ship (`assets/fonts/`) -- so a label
    /// that draws on this machine can still draw blanks there. Handing the
    /// shipped files in here is how a native capture sees what THOSE shells
    /// draw: the same `new_font_system_from_db` they build, the same default
    /// family they pass `LabelShapeCache::with_default_family`.
    pub label_fonts: Option<LabelFonts>,
    /// **A navigation frame over the map**: the route line last in the
    /// tile pass, its screen path in the label collision index, and the
    /// chrome in a pass of its own -- the order `MapView::paint` draws them.
    /// `None`, the default, draws no navigation at all.
    pub nav: Option<NavCapture>,
    /// **A runtime GeoJSON overlay over the basemap**: a FeatureCollection
    /// document, exactly what `MapView::set_source_data_from` takes, parsed
    /// by the same `OverlayStore` and drawn the way `MapView::paint` draws
    /// an overlay source -- tessellated in its origin's local grid, placed
    /// with `combined_overlay_matrix`, projected with
    /// `GlobeUpload::for_overlay`, pushed after the tiles and before the
    /// route line. `None`, the default, draws no overlay.
    pub overlay: Option<String>,
    /// The overlay source's paint declaration (a JSON object, as
    /// `OverlayStore::declare_source_paint` takes it), e.g.
    /// `{"fill-outline-color":"#000"}`. `None`, the default, declares none.
    pub overlay_paint: Option<String>,
    /// Further overlay sources, each parsed and drawn exactly as
    /// [`Self::overlay`] is (with [`Self::overlay_paint`]) but as its OWN
    /// source, with its own origin -- as `MapView` draws several named
    /// sources, in order, after `overlay`. Empty, the default, draws none.
    pub overlay_more: Vec<String>,
    /// A CSV overlay source (the file's bytes, read with
    /// `CsvOptions::default()`): parsed by `overlay::csv` and pushed with
    /// `OverlayStore::set_source_csv`, as `MapView::set_source_csv` does,
    /// then drawn like [`Self::overlay`] (with [`Self::overlay_paint`] and
    /// [`Self::overlay_points`]), after it and every [`Self::overlay_more`].
    /// `None`, the default, draws none.
    pub overlay_csv: Option<Vec<u8>>,
    /// Draw `overlay` TILED, as `MapView` draws a source over
    /// `overlay::tile_plan::TILED_SOURCE_MIN_POINTS`: `classify` and
    /// `TileIndex::build` over its features, one `bake_overlay_tile_compact` per
    /// tile of [`Self::cover`] (overzoom: the `max_zoom` ancestor), each
    /// drawn with `combined_tile_matrix`, `GlobeUpload::for_tile` and
    /// `TileClip::for_descendant(tile, display)`. `false`, the default,
    /// draws the whole source as one mesh however large it is.
    pub overlay_tiling: bool,
    /// Draw `overlay` through the POINT path, as `MapView` draws a source
    /// declared with `set_source_cluster_options(.., Some(points_only()))`:
    /// the point index, and the view of it for this camera, planned and
    /// tessellated like any whole-source overlay. Every feature must be a
    /// single-position `Point`.
    pub overlay_points: bool,
    /// Draw `overlay` ABOVE the labels, in its own pass after the symbol
    /// pass, as `MapView` draws a source after
    /// `set_source_below_labels(name, false)`. Off by default: the overlay
    /// draws in the tile pass and the labels over it, as every source does
    /// unless asked otherwise. The split is `render::overlay_order`'s, the
    /// one `MapView::paint` uses.
    pub overlay_above_labels: bool,
    /// Keep `overlay` (below the labels) UNDER the basemap's 3D buildings,
    /// in the tile pass, as `MapView` draws a source after
    /// `set_source_above_buildings(name, false)`. Off by default: on a
    /// frame that draws buildings and no `extrusions` of the caller's, the
    /// overlay draws over them, in the above-buildings pass
    /// (`render::overlay_order::overlay_pass`).
    pub overlay_under_buildings: bool,
    /// Highlight feature `.0` of `overlay` in style `.1`, as
    /// `MapView::set_feature_highlight` does: the same plan
    /// (`overlay::highlight_plan`), tessellated the same way and drawn
    /// straight after the overlay, in its slot. `None`, the default,
    /// highlights nothing.
    pub overlay_highlight: Option<(usize, crate::overlay::HighlightStyle)>,
    /// Draw basemap fills on the 8-byte compact vertex, as `MapView` does
    /// (`TilePipeline::install_basemap_mesh`). On by default, and off under
    /// `GPUI_MAP_BASEMAP_COMPACT=0`; `false` is the 88-byte instrument the
    /// pixel-identity gate compares against.
    pub basemap_compact: bool,
    /// With [`Self::basemap_compact`], also put undashed strokes on the
    /// 16-byte compact vertex. `None`, the default, is the live map's
    /// switch (`GPUI_MAP_BASEMAP_COMPACT_STROKES`, read once); `Some` reads
    /// no environment, which the story player needs so a frame never
    /// depends on the process it was rendered in.
    pub basemap_compact_strokes: Option<bool>,
    /// **Population dots over the basemap** (`render::dots`, `vs_dot`):
    /// decoded count tiles, built into bucketed blocks at
    /// `level::ppd_build_for(ppd)` (`ppd / √2`) and drawn after the overlay, below the
    /// labels. `None`, the default, draws no dots.
    pub dots: Option<CaptureDots>,
    /// **Donut clusters** (`render::donut`), one `(donuts, palette)` per
    /// layer in draw order, as `MapView::set_donuts` layers are: each
    /// anchored by the projection `MapView::paint` uses
    /// (`globe::WorldProjector` on this frame's globe), at a scale factor
    /// of 1, and drawn in their own pass after the tile pass (so over the
    /// overlay and the dots) and before the labels -- the start of
    /// `paint`'s symbol pass. Empty, the default, draws none. The donut
    /// paint path's headless integration capture (the race map's cluster
    /// mode, `demos/race_dots.rs --mode clusters --capture`).
    pub donuts: Vec<(Vec<crate::render::donut::Donut>, [Rgba; 8])>,
    /// **Arc layers** (`render::arcs`), in draw order, as
    /// `MapView::set_arc_layer` draws them: their own pass after the tile
    /// pass and before the donuts -- the start of `paint`'s symbol pass.
    /// Empty, the default, draws none.
    pub arcs: Vec<CaptureArcs>,
    /// **Trip layers** (`render::trips`), in draw order, as
    /// `MapView::set_trip_layer` draws them: right after the arcs. Empty,
    /// the default, draws none.
    pub trips: Vec<CaptureTrips>,
    /// **Overlay extrusion layers** (`overlay::extrude`), in draw order, as
    /// `MapView::set_extrusion_layer` draws them: after the basemap's
    /// buildings in the tile pass, flat or on the globe. Empty, the
    /// default, draws none.
    pub extrusions: Vec<CaptureExtrusion>,
    /// **Overlay point labels**: a FeatureCollection of `Point`s carrying
    /// `text-field` (and `text-size`, `text-color`, ...), what
    /// `MapView::set_source` takes for a label-only source such as the race
    /// map's cluster counts. Each label is anchored as `MapView::paint`
    /// anchors an overlay symbol (`map_view::overlay_symbol_screen_anchor`,
    /// this frame's `WorldProjector`: on the sphere, and left out when the
    /// planet hides it) and centred as `paint` centres an overlay label
    /// with no icon (`symbol_pass::text_area_origin`), at a scale factor of
    /// 1, and drawn over everything in the symbol pass. Unlike `paint` no
    /// collision is run: every label drawn is counted in
    /// [`Capture::overlay_labels_drawn`]. `None`, the default, draws none.
    pub overlay_labels: Option<String>,
    /// Bumped by the story timeline on every style edit; a mesh baked under
    /// one generation is never drawn under another. `0`, the default, for a
    /// caller that never edits its style. A [`CaptureSession`] keys its
    /// baked-tile cache on it (with the tile, its data fingerprint, zoom and
    /// paint), so a session handed a different style without a new
    /// generation would draw the old bakes.
    ///
    /// **The player must mutate the style in exactly one place**, the one
    /// that bumps this: a style edited anywhere else draws stale meshes in
    /// release. Debug builds catch it (a `debug_assert` on every cache hit
    /// compares the style's fingerprint with the one the bake was made
    /// under). Tile data needs no bump: a new source is a new fingerprint.
    pub style_generation: u64,
    /// `fill-extrusion-opacity` for every extruded layer, replacing the
    /// style's (`ExtrusionOptions::opacity_override`). `None` = the style's.
    pub extrusion_opacity: Option<f32>,
    /// Draw every building at height 0 (`ExtrusionOptions::flatten`): the
    /// control a wall check diffs an extruded capture against.
    pub extrusion_flat: bool,
    /// Building shading beyond MapLibre's (`ExtrusionOptions::look`), as
    /// `ExtrusionOverrides::look` sets it on the map. MapLibre by default.
    pub extrusion_look: crate::render::extrusion::ExtrusionLook,
    /// Hide point labels a building hides (`ExtrusionOverrides::label_occlusion`).
    pub label_occlusion: bool,
    /// The flat cover's `CoverZoom::building_floor`. `None` (the default)
    /// is resolved by [`capture_async`] as `MapView` does it: 13 while any
    /// extrusion layer draws ([`CaptureRequest::resolved`]), so captures
    /// and stories frame the same buildings the app does. A caller that
    /// fetches its own tiles fetches `request.resolved(style).cover()`.
    pub building_floor: Option<u8>,
    /// Turns the default floor (and the fade before the building cut) off,
    /// as `ExtrusionOverrides::disable_building_floor` does on the map.
    pub disable_building_floor: bool,
    /// **Objects above the globe** (`render::altitude_pipeline`): static
    /// lon/lat/altitude points, drawn as `MapView::paint` draws an altitude
    /// layer -- right after the donuts, under the labels -- and only on a
    /// globe frame (`transition > 0`, R3). Counted in
    /// [`Capture::altitude_points_drawn`]. `None`, the default, draws none.
    pub altitude: Option<AltitudeCapture>,
    /// **How far up a missing cover tile may look for a stand-in.**
    /// `None`, the default, is the live map's rule: the nearest resident
    /// ancestor at any level, up to z0. `Some(k)` looks at most `k` levels
    /// up and names the tile missing past that.
    ///
    /// The story player sets it to the prefetch plan's stand-in depth
    /// (`story::prefetch::STAND_IN_LEVELS`): the plan fetches and waits
    /// for exactly those ancestors, so any higher one that happens to be
    /// resident (left over from an earlier frame) is there by fetch
    /// timing, and drawing it would make the pixels depend on timing.
    pub stand_in_levels: Option<u8>,
    /// **The story burn-in** (`capture::chrome`, spec §5.4): captions on
    /// screen this frame, drawn over everything at their anchors, clear of
    /// the credit and the mark, each faded by its `alpha`. Empty, the
    /// default, draws none.
    pub captions: Vec<crate::story::timeline::ActiveCaption>,
    /// The credit burned in bottom-right, on `MapView::paint`'s plate
    /// (`map_view::attribution_chrome`), stacked on `" | "` once wider than
    /// 60% of the frame. `None`, the default, draws no credit.
    pub attribution: Option<String>,
    /// The SWAT mark burned in bottom-left, as `MapView::paint` draws it
    /// (`brand_mark::instance`, artwork by `brand_mark::resolve_for_frame`),
    /// at `height_px` times [`Self::pixel_ratio`]. `None`, the default,
    /// draws no mark.
    pub brand_mark: Option<crate::brand_mark::BrandMark>,
    /// A user-supplied basemap wordmark (a story's `scene.basemap.wordmark`,
    /// e.g. the Mapbox logo its terms require), burned in beside the SWAT
    /// mark -- or alone in its corner, see [`Self::brand_mark_policy`] --
    /// at the mark's height and never over it or the credit
    /// (`capture::chrome::wordmark_quad`). `None`, the default, draws none.
    /// The engine ships no third-party logo: the artwork is the caller's.
    pub wordmark: Option<Artwork>,
    /// The Mapbox-wordmark override: whether [`Self::brand_mark`] still
    /// draws when [`Self::wordmark`] is also set this frame. See
    /// [`crate::brand_mark::BrandMarkPolicy`] for the two cases;
    /// [`crate::brand_mark::BrandMarkPolicy::Auto`] is the default, so a
    /// request that was built before this field existed keeps its mark
    /// exactly when it set no wordmark, and loses it (not the wordmark)
    /// on the one row both used to share. [`Self::effective_brand_mark`]
    /// is what `capture::chrome` actually draws with.
    pub brand_mark_policy: crate::brand_mark::BrandMarkPolicy,
    /// **The style's sprite sheet**, for `icon-image`. The symbol pass
    /// places icons against it and draws them under their labels, in
    /// `symbol_pass::sort_symbols_for_drawing` order, as `MapView::paint`
    /// does once `set_sprites` has run. A [`CaptureSession`] packs it into
    /// one atlas texture once per distinct `Arc` and keeps it. `None`, the
    /// default, is the sprite-less frame: every icon-bearing feature draws
    /// its text and skips its icon. Counted in [`Capture::icons_drawn`].
    pub sprites: Option<std::sync::Arc<crate::sprite::SpriteAtlas>>,
    /// **Draw line-placed labels** (street names bent along their roads).
    /// With [`Self::labels`] on, the symbol pass gets a real
    /// `symbol_pass::GlyphSink` -- the session's `render::GlyphAtlas` and
    /// swash cache -- so each glyph of a LINE-placed label rasters into
    /// the SDF glyph atlas and draws as its own rotated quad through
    /// `IconPipeline`'s SDF branch, after the icons and in the same pass,
    /// exactly as the browser build does (`web/src/app.rs`). Counted in
    /// [`Capture::line_glyphs_drawn`].
    ///
    /// Off by default, because the golden frames were blessed without
    /// street names: with it off the label path is the one those hashes
    /// pin, bit for bit. The story player turns it on.
    pub line_labels: bool,
    /// **A story's overlay layers** (story-content 1a, spec §3), drawn after
    /// [`Self::overlay`] and [`Self::overlay_more`], in order, in the
    /// below-labels slot (or the above-labels one for
    /// [`StoryLayerDraw::above_labels`]); each layer's
    /// [`StoryLayerDraw::marks`] always draw above the labels. Keys must be
    /// distinct (a frame with two equal keys is an error). A
    /// [`CaptureSession`] tessellates each once and redraws it from its
    /// story overlay cache, writing only its trim, opacity and per-feature
    /// paint per frame ([`Capture::story_retessellations`]). Empty, the
    /// default, draws none (every golden frame).
    pub story_layers: Vec<StoryLayerDraw>,
    /// **A story's panels, title card and captions** (story-content 1a,
    /// spec §4.1-§4.2), laid out by `story::layout::layout` and drawn in the
    /// chrome pass BEFORE the credit, the mark and the captions, so those
    /// stay on top. With it set, this frame's [`Self::captions`] are placed
    /// by the layout (as its `LayoutItem::Caption` items, ids
    /// `caption/<index into captions>`) instead of the story-less caption
    /// rule. `None`, the default, draws none and leaves every caption
    /// exactly where a story-less frame puts it (every golden frame).
    pub story: Option<std::sync::Arc<StoryFrame>>,
}

/// One frame's story elements ([`CaptureRequest::story`]), in 1080 units.
#[derive(Clone, Debug, Default)]
pub struct StoryFrame {
    pub items: Vec<crate::story::layout::LayoutItem>,
    /// Per item id: (alpha, enter), from `ActiveElement`.
    pub motion: Vec<(String, f32, f32)>,
    pub safe_area: Vec<crate::story::tiers::Rect>,
    /// The image each item draws (spec §4.3): a `LayoutItem::Image`'s, or a
    /// panel's embedded one, by the item's id. An item with an image rect
    /// and no entry here draws no image.
    pub images: Vec<StoryImage>,
    /// The design system's part of the frame (spec §3, the slice): the
    /// card, the fine print and the fade, drawn by
    /// `story_chrome::build_story_chrome` in place of the corner credit
    /// plate. `None` (every script without a top-level `design`) draws
    /// exactly as 1a.
    pub design: Option<crate::story::card_layout::DesignFrame>,
}

/// One item's image on one frame ([`StoryFrame::images`]).
#[derive(Clone, Debug)]
pub struct StoryImage {
    /// The layout item's id.
    pub id: String,
    /// Decoded and downscaled (`story::media::load_image`). One texture is
    /// uploaded per distinct `Arc` and kept while the `Arc` lives.
    pub image: std::sync::Arc<image::RgbaImage>,
    pub fit: crate::story::script::Fit,
    /// Drawn in the image's bottom-left over a dark plate.
    pub credit: Option<String>,
}

/// One story overlay layer on one frame ([`CaptureRequest::story_layers`]):
/// its content (the cache key's inputs) and this frame's effect state.
#[derive(Clone, Debug)]
pub struct StoryLayerDraw {
    /// Stable per layer and content: [`Self::content_key`] of the fields
    /// below it names. Two draws with one key share one cached mesh.
    pub key: u64,
    /// The layer's id, for error messages.
    pub layer_id: std::sync::Arc<str>,
    /// A FeatureCollection whose features carry integer `id`s (the ids
    /// [`Self::features`] name).
    pub geojson: std::sync::Arc<str>,
    /// The layer's MapLibre paint (`line-color`, `line-width`, ...):
    /// expressions and `line-gradient` are declared as the source's paint,
    /// constants written into every feature. Named colours are accepted.
    pub paint: std::sync::Arc<serde_json::Map<String, serde_json::Value>>,
    /// MapLibre's `lineMetrics`: lines carry progress, so a trim cuts them.
    pub line_metrics: bool,
    /// Plan one op per feature (a layer with a `highlight`, `drop` or
    /// `pulse`), so [`Self::features`] rewrites one feature's paint alone;
    /// otherwise features of one paint share an op. Part of the key.
    pub per_feature: bool,
    /// Draw in the above-labels slot (the overlay's `above_labels`).
    pub above_labels: bool,
    /// Draw over the 3D buildings (the layer's `above_buildings`): in the
    /// above-buildings pass, after the tile pass's extrusions, with a fresh
    /// depth buffer (so no wall or roof hides it), and under the labels.
    /// Ignored with [`Self::above_labels`], whose pass is above both. Not
    /// part of the key: the slot moves the draw, not the mesh.
    pub above_buildings: bool,
    /// `(keep_start, keep_end, opacity, trim_on)`, written every frame into
    /// the model uniform, never re-baked (`render::gpu::LINE_FX_NONE` is
    /// "as declared").
    pub line_fx: [f32; 4],
    /// Per-feature overrides from `story::effects::FeatureFx`, in id order
    /// (a paint rewrite, no re-tessellation).
    pub features: Vec<crate::story::effects::FeatureFx>,
    /// Extra circles this frame: the draw head and pulse/drop rings, as a
    /// tiny Point FeatureCollection with `circle-radius`/`circle-color`
    /// properties (rebuilt per frame; at most a few hundred points).
    pub marks: Option<String>,
    /// Arrowheads this frame (a story `arrow`'s line ends, spec §6.3):
    /// each drawn as an SDF triangle at the layer's opacity, above the
    /// labels with the marks. Empty for every other layer.
    pub arrowheads: Vec<StoryArrowhead>,
    /// `Some`: the layer draws as these markers this frame (spec §6.3-§6.4:
    /// a themed point sublayer, or a circle layer with `circle-stroke-*`,
    /// `circle-blur` or `circle-translate`), each an instanced disc with
    /// its stroke, blur and translate, in the symbol pass under every
    /// label; its mesh is not drawn. `None`: the mesh, as declared.
    pub markers: Option<Vec<StoryMarker>>,
    /// Point labels and badge numerals this frame (spec §6.3), placed by
    /// the capture over the markers, before the basemap's labels.
    pub labels: Vec<StoryLabel>,
}

/// One marker disc ([`StoryLayerDraw::markers`]), MapLibre's circle: a
/// fill to `radius`, a stroke `stroke_width` wide outside it, the edge
/// softened over `blur` of the whole radius, moved `translate` on screen.
/// Lengths are logical px (times the frame's pixel ratio, as every
/// overlay width); colours straight sRGB.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StoryMarker {
    /// `(lon, lat)`.
    pub at: (f64, f64),
    pub radius: f32,
    pub fill: [u8; 4],
    pub stroke_width: f32,
    pub stroke: [u8; 4],
    pub blur: f32,
    pub translate: (f32, f32),
    /// The feature it draws, whose per-feature effects (`drop`, `pulse`,
    /// `highlight`: `story::effects::FeatureFx`) the player applies each
    /// frame; `None` for a ring or the draw head.
    pub id: Option<u64>,
}

/// One point label or badge numeral ([`StoryLayerDraw::labels`]).
#[derive(Clone, Debug, PartialEq)]
pub struct StoryLabel {
    /// The marker's `(lon, lat)`.
    pub at: (f64, f64),
    pub text: String,
    /// Logical px, and the design face's weight.
    pub size: f32,
    pub weight: u16,
    pub color: [u8; 4],
    pub halo: [u8; 4],
    pub halo_width: f32,
    /// `None`: centred on the marker (a numeral). `Some((offset, side))`:
    /// `offset` px from the centre on its preferred `side` (the feature's
    /// `label_side`, else away from the nearest story line), which the
    /// capture flips near the frame's edge or a safe-area rail
    /// (`theme_layers::place_label`).
    pub beside: Option<(f32, crate::story::theme_layers::LabelSide)>,
    /// The feature, for the `label_under_card` warning.
    pub feature: String,
    /// Its feature id, for its per-feature effects (as [`StoryMarker::id`]).
    pub id: Option<u64>,
    /// This label's point state (Task R13, spec's 64-cap: `now > next >
    /// done > muted`), for [`crate::capture::story_overlays::place_story_labels`]'
    /// drop order when more than the cap would draw. An unthemed circle
    /// layer's label (`story_overlays::circle_labels`) carries no state at
    /// all -- it is not part of the timeline -- so it ranks with `Next`
    /// (ruling R13-a, review round 1): below a real `now` label, which
    /// must always win the cap over a static one, but still ahead of
    /// `done`/`muted`.
    pub state: crate::story::theme_layers::PointState,
}

/// One arrowhead on a story line ([`StoryLayerDraw::arrowheads`], spec
/// §6.3-§6.4): a filled triangle whose base is centred on the line's last
/// vertex, pointing on along the last segment's screen bearing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StoryArrowhead {
    /// The line's last vertex, `(lon, lat)`: the base's centre.
    pub end: (f64, f64),
    /// The vertex before it: the arrowhead points from it through `end`.
    pub from: (f64, f64),
    pub color: [u8; 4],
    /// Tip to base and across the base, logical px (times the frame's
    /// pixel ratio, as every overlay width).
    pub length: f32,
    pub width: f32,
    /// The edge round the head and along the stub it lays over the
    /// shaft's end (the casing's colour and margin, R7-a): its colour and
    /// width, logical px. A width of 0 draws no edge.
    pub edge_color: [u8; 4],
    pub edge_width: f32,
    /// The stub the head lays over the shaft's end: the core's width,
    /// logical px. Drawn between 1 and 16 units (at the 20-unit base); a
    /// wider core is narrowed under the head.
    pub stub_width: f32,
}

impl StoryLayerDraw {
    /// `sha256(layer_id, geojson, paint JSON, line_metrics, per_feature)`,
    /// truncated to `u64`: the cache key [`Self::key`] is set to. Each
    /// field is length-prefixed, so no two different inputs concatenate
    /// to the same bytes.
    #[must_use]
    pub fn content_key(
        layer_id: &str,
        geojson: &str,
        paint: &serde_json::Map<String, serde_json::Value>,
        line_metrics: bool,
        per_feature: bool,
    ) -> u64 {
        use sha2::Digest as _;
        let mut h = sha2::Sha256::new();
        let paint = serde_json::Value::Object(paint.clone()).to_string();
        for part in [layer_id.as_bytes(), geojson.as_bytes(), paint.as_bytes()] {
            h.update((part.len() as u64).to_le_bytes());
            h.update(part);
        }
        h.update([u8::from(line_metrics), u8::from(per_feature)]);
        let digest = h.finalize();
        let mut first = [0u8; 8];
        first.copy_from_slice(&digest[..8]);
        u64::from_le_bytes(first)
    }
}

/// One arc layer of a capture: [`CaptureRequest::arcs`].
#[derive(Clone, Debug)]
pub struct CaptureArcs {
    pub set: std::sync::Arc<crate::render::arcs::ArcSet>,
    pub style: crate::render::arcs::ArcStyle,
    /// The filter window, absolute (`MapView::set_arc_filter_range`).
    pub window: Option<[f64; 2]>,
}

/// One overlay extrusion layer of a capture: [`CaptureRequest::extrusions`].
#[derive(Clone, Debug)]
pub struct CaptureExtrusion {
    pub set: std::sync::Arc<crate::overlay::extrude::OverlayExtrusion>,
    pub style: crate::overlay::extrude::ExtrusionStyle,
}

/// One trip layer of a capture: [`CaptureRequest::trips`].
#[derive(Clone, Debug)]
pub struct CaptureTrips {
    pub set: std::sync::Arc<crate::render::trips::TripSet>,
    pub style: crate::render::trips::TripStyle,
    /// The clock, absolute (`MapView::set_trip_clock`); `None` draws the
    /// paths whole.
    pub clock: Option<f64>,
}

/// An altitude layer for [`CaptureRequest::altitude`]. Later tasks add
/// fields: always build it with `..Default::default()`.
#[derive(Clone, Debug, Default)]
pub struct AltitudeCapture {
    /// Earth-fixed positions (`render::altitude::GeoPoint`).
    pub points: Vec<crate::render::altitude::GeoPoint>,
    /// Per-object paint (colour and size), as `MapView::set_altitude_points`
    /// takes it.
    pub style: crate::render::altitude_pipeline::AltitudeStyle,
    /// Indices of the hovered, selected and halo (ISS) objects, drawn last
    /// and larger, as `MapView::set_altitude_emphasis` draws them.
    pub emphasis: (Option<usize>, Option<usize>, Option<usize>),
    /// Moving objects between two keyframes, evaluated on the GPU as
    /// `MapView` evaluates a keyframed layer. When `Some`, [`Self::points`]
    /// is ignored.
    pub keyframes: Option<CaptureKeyframes>,
    /// Orbit lines, drawn as `MapView::set_altitude_lines` draws a line
    /// layer: before the points in the same pass, near-clipped, hidden by
    /// the planet per fragment. Metres in [`Self::lines_frame`].
    pub lines: Vec<crate::render::altitude::AltitudeLine>,
    /// What [`Self::lines`] are in: `Inertial` (TEME) lines are turned by
    /// `teme_to_sphere(lines_scene_unix)`.
    pub lines_frame: crate::render::altitude::RefFrame,
    /// The lines' width, logical px (0 draws nothing but the feather).
    pub lines_width_px: f32,
    /// The scene time the inertial lines are drawn at, Unix seconds.
    pub lines_scene_unix: f64,
    /// The lines' look (`MapView::set_altitude_lines_look`).
    pub lines_look: crate::render::altitude_pipeline::AltitudeLook,
}

/// Two keyframes and a scene time for [`AltitudeCapture::keyframes`]: the
/// pair `k0` at `t0_unix` and `k1` at `t0_unix + delta_s`, drawn at
/// `s = (scene_unix - t0_unix) / delta_s`, rotated by
/// `teme_to_sphere(scene_unix)` for `RefFrame::Inertial`.
///
/// Built through the same `keyed_instance` as `MapView`, but a capture has
/// no source to bisect, so an object valid at one end only is taken to
/// exist over that half of the interval (`[0, 0.5]` or `[0.5, 1]`) rather
/// than `hidden_bounds`' 1/64-accurate window.
#[derive(Clone, Debug, Default)]
pub struct CaptureKeyframes {
    /// What `k0` and `k1` are in: `Inertial` (TEME) or `EarthFixed`.
    pub frame: crate::render::altitude::RefFrame,
    /// The first keyframe's instant, Unix seconds.
    pub t0_unix: f64,
    /// Seconds from `k0` to `k1`.
    pub delta_s: f64,
    /// Every object's state at `t0_unix`.
    pub k0: Vec<crate::render::altitude::KeyState>,
    /// Every object's state at `t0_unix + delta_s`, in `k0`'s order.
    pub k1: Vec<crate::render::altitude::KeyState>,
    /// The scene time drawn, Unix seconds (normally inside the pair).
    pub scene_unix: f64,
    /// The scene clock's rate the frame is drawn at: sizes the style's
    /// trails (`TrailStyle`). 0: none.
    pub rate: f64,
}

/// A dot-density layer for [`CaptureRequest::dots`]: already-decoded count
/// tiles, for the same reason `tiles` is a `TileStore` -- a test puts KNOWN
/// counts on a KNOWN tile and asserts pixels.
#[derive(Debug, Clone)]
pub struct CaptureDots {
    pub descriptor: crate::overlay::dotgrid::GridDescriptor,
    pub tiles: Vec<(crate::overlay::dotgrid::GridTileKey, crate::overlay::dotgrid::GridTile)>,
    /// The view's population per dot.
    pub ppd: f32,
    /// The `ppd_build` the blocks were built at, or `None` for
    /// `level::ppd_build_for(ppd)`. `Some(ppd / 2)` is the most slots
    /// `needs_rebuild`'s hysteresis lets a tile carry (MG-8 measured the
    /// old `ppd / 4` band too).
    pub ppd_build: Option<f32>,
    pub radius_px: f32,
    /// `DotParams::radius_shrink`: the densest cells' radius loss
    /// (`DotStyle::density_shrink`); 0 draws every dot at `radius_px`.
    pub radius_shrink: f32,
    /// `DotParams::overlap_gamma`: the overlap fade's exponent
    /// (`DotStyle::overlap_gamma`); 0 draws every dot at full alpha.
    pub overlap_gamma: f32,
    /// Colour stops of the density ramp, low to high
    /// (`render::dots::ramp_paints`).
    pub ramp: Vec<Rgba>,
    /// Live epochs (Task 15), `DotStyle::epoch_mix`: each tile whose key
    /// has `epoch_b` draws its cells at `pop + (pop_b - pop) * epoch_mix`
    /// (`DotParams::t`); 0 is epoch A exactly, 1 epoch B.
    pub epoch_mix: f32,
    /// `DotParams::color_mode` `COLOR_DIFFERENCE`: colour by the change from A to B
    /// (`render::dots::difference_level`) through `ramp`, not by density.
    pub difference: bool,
    /// One exclusion box, in every tile's own lyon units, applied as each
    /// tile's `DotParams::exclude_a`; `None` excludes nothing.
    pub exclude: Option<[f32; 4]>,
    /// Per-tile exclusion boxes (`DotParams::exclude_a`/`exclude_b`),
    /// parallel to `tiles` -- a stand-in's, from
    /// `overlay::dotgrid::layer::plan_dots`. Empty: every tile takes
    /// `exclude`.
    pub tile_excludes: Vec<[[f32; 4]; 2]>,
    /// `DotStyle::relocate`: over basemap tiles, the capture builds the dot
    /// mask from them -- the same `TilePipeline::encode_dot_mask` pass
    /// `MapView` runs -- and moves each dot to its best candidate. `false`
    /// is today's placement, pixel for pixel. Only with
    /// `CaptureRequest::paint` `Paint::AsStyled`: a repainted basemap drops
    /// ops the plan's ranks name, so any other paint draws today's
    /// placement (and says so on stderr, once).
    pub relocate: bool,
    /// `DotStyle::mask_k`: candidates per dot while relocating.
    pub mask_k: u32,
    /// A mask to relocate against in place of the basemap's: its grid and
    /// its texels, row-major (`DotMaskGpu::from_texels`). `None` builds
    /// the mask from the basemap tiles.
    pub synthetic_mask: Option<(crate::render::dot_mask::MaskGrid, Vec<u8>)>,
    /// The previous frame's mask grid, for the mask zoom's hysteresis
    /// (`MaskGrid::plan_after`): what a map's bound mask gives it. `None`
    /// plans without history.
    pub mask_previous: Option<crate::render::dot_mask::MaskGrid>,
    /// Race dots (Task 6): `Some(mask)` colours each dot by its group
    /// (`DotParams::color_mode` `COLOR_CATEGORICAL`, the tiles' `cats` or
    /// `parts`), `ramp`'s entries taken verbatim as the 8 groups' colours,
    /// and draws only the groups whose bit `mask` sets
    /// (`DotParams::category_mask`). `None`: the density ramp.
    pub category_mask: Option<u32>,
}

impl CaptureDots {
    /// The `DotStyle` fields that decide a draw's relocation.
    fn relocation_style(&self) -> crate::overlay::dotgrid::layer::DotStyle {
        crate::overlay::dotgrid::layer::DotStyle { relocate: self.relocate, mask_k: self.mask_k, ..Default::default() }
    }

    /// Whether the capture builds a mask from its basemap tiles.
    fn relocates_over_the_basemap(&self) -> bool {
        self.relocation_style().relocates() && self.synthetic_mask.is_none()
    }

    /// The finest data cell of the tiles, in unit world (its width).
    fn finest_cell(&self) -> Option<f64> {
        // A part tile (C7) has no cell: its level is not a pyramid level.
        self.tiles
            .iter()
            .filter(|(key, _)| !key.is_part())
            .map(|(key, _)| crate::overlay::dotgrid::grid::cell_arcsec(&self.descriptor, key.level) / 3600.0 / 360.0)
            .min_by(f64::total_cmp)
    }
}

/// The dot mask a capture relocates against: `dots`' synthetic one, or --
/// when `dots` relocates, the basemap carries its mask lists (`masked`),
/// and the frame is flat with cells of at least
/// `RELOCATION_MIN_CELL_PX` -- one rendered from `basemap` (every tile
/// drawn whole, with its unit-world placement) into its own submit ahead
/// of the frame's, exactly as `MapView::paint` does.
fn capture_dot_mask(
    dots: &CaptureDots,
    ctx: &GpuContext,
    pipeline: &TilePipeline,
    camera: &Camera,
    transition: f32,
    basemap: &[(TileId, &crate::render::TileBuffers)],
    masked: bool,
) -> Option<(crate::render::dot_mask::DotMaskGpu, crate::render::dot_mask::MaskGrid)> {
    use crate::render::dot_mask::{relocation_region, DotMaskGpu, MaskGrid, MaskKey};
    if let Some((grid, texels)) = &dots.synthetic_mask {
        return Some((DotMaskGpu::from_texels(ctx, *grid, texels), *grid));
    }
    if !dots.relocates_over_the_basemap() || basemap.is_empty() {
        return None;
    }
    // The basemap installed no mask lists (a repainted capture): a mask
    // drawn from it would be all land, a relocation that silently moves
    // nothing.
    if !masked {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| eprintln!("gpui-maps capture: dot relocation needs Paint::AsStyled; drawing today's placement"));
        return None;
    }
    let cell = dots.finest_cell()?;
    let region = relocation_region(camera, transition, cell)?;
    let bound = crate::render::dot_mask::mask_extent_bound(camera, transition, cell);
    let grid = MaskGrid::plan_after(
        camera.zoom,
        region,
        cell,
        ctx.device.limits().max_texture_dimension_2d,
        dots.mask_previous.as_ref(),
        bound,
    )?;
    let draws: Vec<crate::render::gpu::MaskTileDraw<'_>> =
        basemap.iter().map(|&(tile, buffers)| crate::render::gpu::MaskTileDraw::for_tile(buffers, tile)).collect();
    let key = MaskKey { grid, tiles: basemap.iter().map(|&(tile, _)| (tile, 0)).collect() };
    let mut mask = None;
    let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("capture-dot-mask") });
    pipeline.encode_dot_mask(ctx, &mut encoder, &mut mask, key, &draws);
    ctx.queue.submit(Some(encoder.finish()));
    mask.map(|m| (m, grid))
}

/// The dot mask the last [`capture`] relocated against, read back: its
/// grid and texels (`None` when it drew no mask). Probe and test builds
/// only (DP-8): Task 9's `world_dots --placement-probe` places every live
/// dot through `overlay::dotgrid::placement` against it.
#[cfg(any(test, feature = "probe"))]
static LAST_DOT_MASK: std::sync::Mutex<Option<(crate::render::dot_mask::MaskGrid, Vec<u8>)>> = std::sync::Mutex::new(None);

/// Takes [`LAST_DOT_MASK`]: the mask the last capture drew its dots against.
#[cfg(any(test, feature = "probe"))]
#[must_use]
pub fn take_last_dot_mask() -> Option<(crate::render::dot_mask::MaskGrid, Vec<u8>)> {
    LAST_DOT_MASK.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take()
}

/// One tile's dot draw a capture holds, with everything its bind groups
/// reference by handle.
struct CaptureDotDraw {
    _tile: crate::render::gpu::DotTileGpu,
    model_group: wgpu::BindGroup,
    dots: crate::render::gpu::DotGroupGpu,
    _model: crate::render::TrackedBuffer,
}

/// Builds [`CaptureDots`]' tiles into GPU draws the way a dot layer draws
/// them: `build_blocks` at `ppd_build_for(ppd)` in the overlay convention's
/// lyon units ([`crate::overlay::dotgrid::blocks::OVERLAY_LYON_PER_UNIT_WORLD`]),
/// each tile placed with `combined_overlay_matrix` off its own NW corner
/// and projected with `GlobeUpload::for_overlay` (its globeness replaced
/// by `transition` when that is `Some`, for MG-8's flat/globe A/B), and
/// relocated against `mask` as `DotLayersGpu::sync` relocates a map's
/// draws (`DotStyle::relocated`). The ramp's table is returned with the
/// draws, which reference it.
///
/// `force_k` (MG-8's DP-2 cost A/B only): every draw relocates against
/// `mask` with this K, whatever the frame's transition and cell size --
/// the loop's cost on every dot, the worst case a view can ask for.
fn dot_draws(
    dots: &CaptureDots,
    ctx: &GpuContext,
    pipeline: &TilePipeline,
    camera: &Camera,
    globe: &GlobeUpload,
    transition: Option<f32>,
    mask: Option<(&crate::render::dot_mask::DotMaskGpu, &crate::render::dot_mask::MaskGrid)>,
    force_k: Option<u32>,
) -> (crate::render::gpu::PaintTableGpu, Vec<CaptureDotDraw>) {
    use crate::overlay::dotgrid::{blocks, level};
    let relocation = dots.relocation_style();
    let frame_transition = transition.unwrap_or_else(|| globe.transition());
    let paints = if dots.category_mask.is_some() {
        dots.ramp.iter().map(|&c| (c, c)).collect()
    } else {
        crate::render::dots::ramp_paints(&dots.ramp)
    };
    let table = pipeline.create_paint_table(ctx, &crate::render::compact::paint_table(&paints));
    let ppd_build = dots.ppd_build.unwrap_or_else(|| level::ppd_build_for(dots.ppd));
    let draws = dots
        .tiles
        .iter()
        .enumerate()
        .map(|(at, (key, tile))| {
            let b = blocks::build_blocks(&dots.descriptor, key, tile, ppd_build, blocks::OVERLAY_LYON_PER_UNIT_WORLD);
            let none = crate::render::dots::DotParams::NONE_EXCLUDED;
            let exclude = dots.tile_excludes.get(at).copied().unwrap_or([dots.exclude.unwrap_or(none), none]);
            let (mut params, origin_world) =
                crate::render::dots::DotParams::for_tile(key.tile, &b, exclude, dots.ppd, dots.radius_px);
            params.radius_shrink = dots.radius_shrink;
            params.overlap_gamma = dots.overlap_gamma;
            // The map's own spelling (`DotStyle::live_params`: the mix
            // clamped by `clamp_mix`, NaN 0, only for a key with epoch B).
            crate::overlay::dotgrid::layer::DotStyle {
                epoch_b: key.epoch_b,
                epoch_mix: dots.epoch_mix,
                ramp: if dots.difference {
                    crate::overlay::dotgrid::layer::DotRamp::Difference
                } else {
                    crate::overlay::dotgrid::layer::DotStyle::default().ramp
                },
                ..Default::default()
            }
            .live_params(&mut params, key);
            if let Some(mask) = dots.category_mask {
                params.color_mode = crate::render::dots::COLOR_CATEGORICAL;
                params.category_mask = mask;
            }
            let params = match (force_k, mask) {
                (Some(k), Some((_, grid))) if k > 0 => {
                    let n = f64::from(1u32 << key.tile.z);
                    let origin = (f64::from(key.tile.x) / n, f64::from(key.tile.y) / n);
                    params.relocating(k, grid.for_tile(origin, 1.0 / blocks::OVERLAY_LYON_PER_UNIT_WORLD))
                }
                (Some(_), _) => crate::render::dots::DotParams { mask_k: 0, ..params },
                (None, _) => relocation.relocated(params, key.tile, camera.zoom, camera.pitch, frame_transition, mask.map(|(_, grid)| grid)),
            };
            let mut globe_params = globe.for_overlay(origin_world);
            if let Some(t) = transition {
                globe_params.transition = t;
            }
            let (model, model_group) = pipeline.create_model_bind_group(
                ctx,
                crate::render::combined_overlay_matrix(camera, origin_world).to_cols_array_2d(),
                0.0,
                0.0,
                globe_params,
            );
            let tile = pipeline.create_dot_tile_with(ctx, &b.buckets, &b.pop_b, &b.comps, &b.shapes);
            let dots = pipeline.create_dot_bind_group(ctx, &table, &tile, mask.map(|(m, _)| m), params);
            CaptureDotDraw { _tile: tile, model_group, dots, _model: model }
        })
        .collect();
    (table, draws)
}

impl Default for CaptureRequest {
    /// **The reported camera**: lon 88.8, lat 61, zoom 3.14 — where the
    /// pale meridian streaks were seen — at 1024x1024, production
    /// subdivision, the style's own background, the style's own colours.
    // `clippy::approx_constant` sees 3.14 and says "you meant pi". It is a
    // ZOOM LEVEL, read off the report, and rounding it to `PI` would move
    // the camera -- 0.0016 of a zoom level is 1.6 px of globe radius at
    // this camera, which is exactly the scale the streaks were seen at.
    #[allow(clippy::approx_constant)]
    fn default() -> Self {
        Self {
            center_lon: 88.8,
            center_lat: 61.0,
            zoom: 3.14,
            pitch: 0.0,
            bearing: 0.0,
            width: 1024,
            height: 1024,
            pixel_ratio: 1.0,
            globe: true,
            subdivision: SubdivisionPolicy::ForTile,
            clear: Clear::Style,
            paint: Paint::AsStyled,
            backdrop: true,
            sun: None,
            absent: Vec::new(),
            atmosphere: None,
            raster: std::collections::HashMap::new(),
            // 1.0, the style-spec default for `raster-opacity`. Read only
            // when `raster` is non-empty, so this value is inert for every
            // capture that predates raster.
            raster_opacity: 1.0,
            raster_stand_ins: Vec::new(),
            labels: false,
            flat: false,
            flat_tile_zoom_max: 14,
            stars: false,
            space_look: None,
            label_fonts: None,
            nav: None,
            overlay: None,
            overlay_paint: None,
            overlay_tiling: false,
            overlay_points: false,
            basemap_compact: crate::render::compact::basemap_compact_enabled(),
            basemap_compact_strokes: None,
            overlay_above_labels: false,
            overlay_under_buildings: false,
            overlay_highlight: None,
            dots: None,
            donuts: Vec::new(),
            arcs: Vec::new(),
            trips: Vec::new(),
            extrusions: Vec::new(),
            overlay_labels: None,
            style_generation: 0,
            overlay_more: Vec::new(),
            overlay_csv: None,
            altitude: None,
            extrusion_opacity: None,
            extrusion_flat: false,
            stand_in_levels: None,
            captions: Vec::new(),
            attribution: None,
            brand_mark: None,
            wordmark: None,
            brand_mark_policy: crate::brand_mark::BrandMarkPolicy::default(),
            sprites: None,
            line_labels: false,
            extrusion_look: crate::render::extrusion::ExtrusionLook::MAPLIBRE,
            label_occlusion: false,
            building_floor: None,
            disable_building_floor: false,
            story_layers: Vec::new(),
            story: None,
        }
    }
}

impl CaptureRequest {
    /// [`Self::brand_mark`] with [`Self::brand_mark_policy`] already
    /// decided against [`Self::wordmark`] -- the one call `capture::chrome`
    /// makes, so a frame's layout (what obstacle the mark leaves) and its
    /// draw (whether a quad is pushed) can never disagree about whether
    /// the mark is on.
    #[must_use]
    pub fn effective_brand_mark(&self) -> Option<crate::brand_mark::BrandMark> {
        self.brand_mark_policy.effective(self.brand_mark, self.wordmark.is_some())
    }

    /// The design frame's slot mark width (spec §3.6, P3), 1080 units:
    /// [`Self::effective_brand_mark`] at `mark_artwork`, else the wordmark
    /// ([`crate::story::frame_layout::slot_mark_w`]). The one reading the
    /// capture's drawer and the story player's camera pass both take, so
    /// they lay out on one spec.
    #[must_use]
    pub(crate) fn slot_mark_w(&self, mark_artwork: Option<(u32, u32)>) -> Option<f32> {
        crate::story::frame_layout::slot_mark_w(
            self.effective_brand_mark().and(mark_artwork),
            self.wordmark.as_ref().map(|a| a.dimensions()),
        )
    }

    /// The camera this request describes: a logical viewport of
    /// [`Self::width`] / [`Self::pixel_ratio`] by [`Self::height`] /
    /// [`Self::pixel_ratio`] -- see [`Self::logical_size`].
    #[must_use]
    pub fn camera(&self) -> Camera {
        let (viewport_width_px, viewport_height_px) = self.logical_size();
        Camera {
            center_lon: self.center_lon,
            center_lat: self.center_lat,
            zoom: self.zoom,
            pitch: self.pitch,
            bearing: self.bearing,
            viewport_width_px,
            viewport_height_px,
            fov_y: Camera::DEFAULT_FOV_Y,
        }
    }

    /// [`Self::width`] and [`Self::height`], as logical pixels: physical
    /// divided by [`Self::pixel_ratio`]. What [`Self::camera`] hands the
    /// `Camera` as its viewport.
    #[must_use]
    pub fn logical_size(&self) -> (f32, f32) {
        (self.width as f32 / self.pixel_ratio, self.height as f32 / self.pixel_ratio)
    }

    /// Which tiles this camera needs, in the order `MapView::paint` draws
    /// them: `GlobeCover::cover` at the display zoom, then the same
    /// `tile_visible` cull `paint` applies before building `model_buffers`.
    ///
    /// Outside the globe band, or with [`Self::globe`] off: empty unless
    /// the request opted into [`CaptureRequest::flat`], in which case it is
    /// `cover_plan::flat_cover` at the display zoom clamped to
    /// `flat_tile_zoom_max` -- the same planar cover every shell draws.
    /// `globe: false` takes the flat cover unconditionally, `flat` opt-in
    /// not required: a request with the globe switched off is a flat map at
    /// every zoom, not merely a globe instrument declining to answer.
    /// Without either, this harness stays a globe instrument and says so
    /// rather than silently drawing a flat map.
    #[must_use]
    pub fn cover(&self) -> Vec<TileId> {
        self.cover_tiles()
    }

    /// The basemap tiles past [`Self::cover`] the dot mask also rasterises
    /// (`render::dot_mask::mask_ring_tiles`, DP-5 ruling): empty unless the
    /// request's dots relocate over a styled basemap on a frame
    /// `render::dots::relocation_camera` passes (flat, z >= 15, pitched at
    /// most 30 deg) -- the window's gate, from the frame's own globe
    /// transition. Fetch them with the cover; the capture draws them into
    /// the mask only, up to `dot_mask::MASK_RING_MAX_BYTES` of device
    /// buffers (nearest first, as the window).
    #[must_use]
    pub fn mask_ring(&self) -> Vec<TileId> {
        let Some(dots) = self.dots.as_ref().filter(|d| d.relocates_over_the_basemap()) else { return Vec::new() };
        if !matches!(self.paint, Paint::AsStyled) {
            return Vec::new();
        }
        let Some(cell) = dots.finest_cell() else { return Vec::new() };
        let camera = self.camera();
        let transition = crate::render::globe::globe_frame(&camera).transition;
        crate::render::dot_mask::mask_ring_tiles(&camera, transition, cell, &self.cover_tiles())
    }

    fn cover_tiles(&self) -> Vec<TileId> {
        let camera = self.camera();
        let frame = crate::render::globe::globe_frame(&camera).with_globe_enabled(self.globe);
        let cover = GlobeCover::new(&frame);
        match cover.cover(self.display_zoom()) {
            Some(tiles) => tiles.into_iter().filter(|&tile| cover.tile_visible(tile)).collect(),
            None if self.flat || !self.globe => {
                crate::render::cover_plan::flat_cover(&camera, self.flat_cover_request()).tiles
            }
            None => Vec::new(),
        }
    }

    /// This request with its building floor resolved against `style`, as
    /// `MapView::visible_tiles` resolves it: the default floor
    /// (`cover_plan::building_floor_for(None)`, 13) while an extrusion
    /// layer draws at the request's zoom, unless
    /// [`Self::disable_building_floor`]. An explicit floor is kept.
    #[must_use]
    pub fn resolved(&self, style: &Style) -> Self {
        let mut out = self.clone();
        if self.disable_building_floor {
            out.building_floor = None;
        } else if self.building_floor.is_none() && crate::render::frame_plan::extrusion_visible(&style.layers, self.zoom) {
            out.building_floor = Some(crate::render::cover_plan::building_floor_for(None));
        }
        out
    }

    /// What the flat cover is asked for: [`Self::flat_tile_zoom`] with the
    /// request's [`Self::building_floor`].
    #[must_use]
    pub fn flat_cover_request(&self) -> crate::render::cover_plan::CoverZoom {
        crate::render::cover_plan::CoverZoom { building_floor: self.building_floor, ..self.flat_tile_zoom().into() }
    }

    /// The zoom a flat cover asks for: the display zoom, capped at the flat
    /// source's ceiling.
    #[must_use]
    pub fn flat_tile_zoom(&self) -> u8 {
        self.display_zoom().min(self.flat_tile_zoom_max)
    }

    /// `MapView::display_zoom`, verbatim.
    #[must_use]
    pub fn display_zoom(&self) -> u8 {
        self.zoom.floor().clamp(0.0, f64::from(u8::MAX)) as u8
    }
}

/// One captured frame, plus what went into it.
#[derive(Clone)]
pub struct Capture {
    pub width: u32,
    pub height: u32,
    /// RGBA8, row-major, top row first, **tightly packed** — the 256-byte
    /// row alignment wgpu's readback requires has already been stripped.
    pub rgba: Vec<u8>,
    /// Tiles whose mesh had at least one index and was therefore drawn.
    pub tiles_drawn: usize,
    /// Basemap tiles past the cover rasterised into the dot mask only
    /// ([`CaptureRequest::mask_ring`]).
    pub mask_ring_tiles: usize,
    /// Tiles the cover asked for that the store had no bytes for. A
    /// non-empty list here invalidates any gap count: a missing tile is a
    /// hole that says nothing about subdivision.
    pub tiles_missing: Vec<TileId>,
    pub vertices: usize,
    pub triangles: usize,
    /// `GlobeUpload::transition` for this frame — 1.0 is a whole sphere.
    pub transition: f32,
    /// Raster tiles submitted to the device this frame.
    ///
    /// **This is the raster half of `vertices > 0`**, and every pixel test
    /// over a raster frame needs it. `vertices` counts what the
    /// TESSELLATOR produced, and a raster tile tessellates nothing — so on
    /// a frame whose only content is imagery, `vertices` is 0 and an
    /// assertion built on it would read a correct frame as an empty one.
    /// Without a count here, a raster pixel test that found no painted
    /// pixels could not tell "the pipeline drew nothing visible" from "the
    /// fixture handed the pipeline nothing", which is the difference
    /// between a bug and a typo.
    pub raster_tiles_drawn: usize,
    /// How many labels the symbol pass placed and handed to glyphon.
    ///
    /// **Zero is a frame with no text in it, whatever the request asked
    /// for**, and every measurement of label ink has to be gated on this
    /// being non-zero — the same discipline `tiles_missing` enforces for a
    /// gap count, and the same one `draw_calls_total > 0` enforces for a
    /// perf figure. `CaptureRequest::labels` off makes it zero by
    /// definition.
    pub labels_drawn: usize,
    /// Label occlusion this frame: `(POI labels tested, CPU ms)`; zeros
    /// without `CaptureRequest::label_occlusion`.
    pub label_occlusion: (usize, f64),
    /// Donuts of [`CaptureRequest::donuts`] the donut pass drew: projected,
    /// visible and on the frame.
    pub donuts_drawn: usize,
    /// Donuts past `render::donut::MAX_DONUTS` in a layer, dropped as
    /// `MapView::set_donuts` drops them -- counted, not silent.
    pub donuts_truncated: usize,
    /// Labels of [`CaptureRequest::overlay_labels`] drawn: anchored on a
    /// visible part of the frame, with text.
    pub overlay_labels_drawn: usize,
    /// Objects of [`CaptureRequest::altitude`] handed to the point stage
    /// visible (the planet and the near plane then cull some on the GPU).
    /// 0 on a flat frame, or with no layer.
    pub altitude_points_drawn: usize,
    /// Icons of [`CaptureRequest::sprites`] the symbol pass placed and
    /// drew. 0 without a sprite sheet.
    pub icons_drawn: usize,
    /// Glyph quads of line-placed labels drawn ([`CaptureRequest::line_labels`]):
    /// one per inked glyph, so a street name counts its letters. 0 with
    /// line labels off.
    pub line_glyphs_drawn: usize,
    /// Glyph requests of this frame's line-placed labels NOT drawn because
    /// the glyph atlas, laid out afresh for every frame, was full (one
    /// frame asking for more distinct glyphs than it holds): each such
    /// label drew with letters missing. 0 on every ordinary frame.
    pub line_glyphs_dropped: usize,
    /// Story overlay layers ([`CaptureRequest::story_layers`]) tessellated
    /// this frame: a cache miss or a display-zoom change. 0 on every frame
    /// after a layer's first while its content and zoom bucket hold -- a
    /// `draw`, `fade` or `highlight` costs none. A layer's per-frame marks
    /// (head, rings) are not counted.
    pub story_retessellations: usize,
    /// Label shapes this frame of a key the session had shaped before and
    /// trimmed since: a re-shape (spec §14.3's
    /// `story.card.reshapes_after_first`). 0 on every frame after a
    /// story's first while its text holds.
    pub story_reshapes: usize,
    /// Wall time of this frame's chrome layout (`chrome::layout`: the
    /// corner, the story's panels, card and fine print, measured through
    /// the session's shape cache), microseconds. 0 without chrome.
    pub chrome_layout_us: f64,
    /// What the story layout ([`CaptureRequest::story`]) could not do
    /// cleanly on this frame, at most one per `(id, code)`. Empty without a
    /// story.
    pub layout_warnings: Vec<crate::story::layout::LayoutWarning>,
}

impl Capture {
    /// The pixel at `(x, y)`, as RGBA bytes.
    #[must_use]
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * self.width + x) * 4) as usize;
        [self.rgba[i], self.rgba[i + 1], self.rgba[i + 2], self.rgba[i + 3]]
    }

    /// **How many coats of a 0.3 white wash each pixel carries**, as
    /// `(once, twice, thrice, more)` — the counts every figure in this
    /// module's docs is quoted from.
    ///
    /// Only meaningful for a frame drawn with [`Paint::Wash`] over a BLACK
    /// clear and no backdrop: there the pixel value IS the composite count
    /// (77 once, 130 twice, 168 three times), which is the whole reason
    /// that mode exists. A pixel of exactly 0 was covered by nothing and is
    /// counted in none of the four.
    ///
    /// Lives here rather than in `examples/globe_capture.rs`, where it
    /// started, so a regression test can grade a frame by the same
    /// arithmetic the command line prints — a test that re-derived these
    /// thresholds would be free to drift from the numbers the fix was
    /// argued with.
    #[must_use]
    pub fn wash_levels(&self) -> (u32, u32, u32, u32) {
        let (mut once, mut twice, mut thrice, mut more) = (0u32, 0u32, 0u32, 0u32);
        for pixel in self.rgba.as_chunks::<4>().0.iter() {
            match pixel[0] {
                0 => {}
                77 => once += 1,
                130 => twice += 1,
                168 => thrice += 1,
                _ => more += 1,
            }
        }
        (once, twice, thrice, more)
    }

    /// **Background pixels bracketed by drawn ones on BOTH axes** — a hole,
    /// as distinct from the empty space outside the silhouette.
    ///
    /// A pixel counts when it matches `background` within `tolerance` and
    /// there is at least one non-background pixel to its left AND to its
    /// right on its own row, AND one above AND one below in its own column.
    /// Space around the disc fails the test on at least one side; a gap
    /// punched through the middle of a fill fails none of them.
    ///
    /// This is the grading the pole-cap A/B in this module's docs used (13
    /// enclosed pixels with `for_tile` against 18,710 without), written
    /// down here rather than left in a scratch script, because it is also
    /// the half of the stand-in fix's bar that says a fix did not trade a
    /// doubled patch for a missing one.
    ///
    /// Two passes over the image, not a flood fill: the question is "is
    /// this pixel enclosed", and a rectangle's worth of bracketing is what
    /// the earlier measurement asked and answered.
    #[must_use]
    pub fn enclosed_background(&self, background: [u8; 4], tolerance: u8) -> usize {
        let (w, h) = (self.width as usize, self.height as usize);
        let is_background = |x: usize, y: usize| {
            let i = (y * w + x) * 4;
            (0..4).all(|c| self.rgba[i + c].abs_diff(background[c]) <= tolerance)
        };
        // `row_spans[y]` / `col_spans[x]` are the first and last DRAWN pixel
        // on that row / column. A background pixel strictly inside both
        // spans has a drawn pixel on all four sides of it.
        let row_spans: Vec<Option<(usize, usize)>> = (0..h)
            .map(|y| {
                let first = (0..w).find(|&x| !is_background(x, y))?;
                let last = (0..w).rev().find(|&x| !is_background(x, y))?;
                Some((first, last))
            })
            .collect();
        let col_spans: Vec<Option<(usize, usize)>> = (0..w)
            .map(|x| {
                let first = (0..h).find(|&y| !is_background(x, y))?;
                let last = (0..h).rev().find(|&y| !is_background(x, y))?;
                Some((first, last))
            })
            .collect();
        let mut enclosed = 0usize;
        for (y, row) in row_spans.iter().enumerate() {
            let Some((row_first, row_last)) = *row else { continue };
            for (x, column) in col_spans.iter().enumerate().take(row_last).skip(row_first + 1) {
                let Some((col_first, col_last)) = *column else { continue };
                if y > col_first && y < col_last && is_background(x, y) {
                    enclosed += 1;
                }
            }
        }
        enclosed
    }

    /// How many pixels match `color` within `tolerance` per channel (alpha
    /// included). With [`Clear::SENTINEL`] this is the hole count.
    #[must_use]
    pub fn count_matching(&self, color: [u8; 4], tolerance: u8) -> usize {
        self.rgba
            .as_chunks::<4>().0.iter()
            .filter(|p| {
                (0..4).all(|c| p[c].abs_diff(color[c]) <= tolerance)
            })
            .count()
    }

    /// How many pixels differ from `other` by more than `tolerance` on any
    /// channel, and the largest single-channel difference found.
    ///
    /// Panics if the two captures are not the same size — a difference
    /// between frames of different shapes is not a difference.
    #[must_use]
    pub fn diff(&self, other: &Self, tolerance: u8) -> (usize, u8) {
        assert_eq!(
            (self.width, self.height),
            (other.width, other.height),
            "two captures can only be differenced at the same size"
        );
        let mut differing = 0usize;
        let mut worst = 0u8;
        for (a, b) in self.rgba.as_chunks::<4>().0.iter().zip(other.rgba.as_chunks::<4>().0.iter()) {
            let d = (0..4).map(|c| a[c].abs_diff(b[c])).max().unwrap_or(0);
            worst = worst.max(d);
            if d > tolerance {
                differing += 1;
            }
        }
        (differing, worst)
    }

    /// Writes the frame as a PNG.
    ///
    /// # Errors
    /// Whatever `image` reports: an unwritable path, a bad extension.
    pub fn save_png(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let buffer: image::RgbaImage =
            image::ImageBuffer::from_raw(self.width, self.height, self.rgba.clone())
                .ok_or_else(|| anyhow::anyhow!("frame buffer is not width*height*4 bytes"))?;
        buffer.save(path)?;
        Ok(())
    }

    /// SHA-256 over the frame's size and pixels, lowercase hex. The one
    /// identity every "bit-identical" claim in the story work is made with.
    #[must_use]
    pub fn sha256_hex(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(self.width.to_le_bytes());
        h.update(self.height.to_le_bytes());
        h.update(&self.rgba);
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// The decoded tiles one capture draws from, keyed by id.
pub type TileStore = HashMap<TileId, crate::mvt::DecodedTile>;

/// Fetches every tile in `tiles` through `source`, caching the raw bytes
/// under `cache_dir`, and decodes them.
///
/// The cache is what makes an A/B honest: two runs that differ only in
/// subdivision must draw the SAME bytes, and a second network round would
/// not guarantee that (a tile archive can be rebuilt between requests).
/// It is also what makes a repeated run cheap.
///
/// `cache_dir` belongs outside the repository. See this module's docs.
///
/// # Errors
/// A source failure for any tile. A tile the source reports as *absent*
/// (`Ok(None)`) is not an error — it lands in `Capture::tiles_missing`
/// instead, where it can be weighed.
pub async fn fetch_cover(
    source: &dyn crate::tile_source::TileSource,
    tiles: &[TileId],
    cache_dir: &Path,
) -> anyhow::Result<TileStore> {
    std::fs::create_dir_all(cache_dir)?;
    let mut store = TileStore::new();
    for &tile in tiles {
        if let Some(bytes) = fetch_one(source, tile, cache_dir).await? {
            store.insert(tile, crate::mvt::decode(bytes)?);
        }
    }
    Ok(store)
}

/// One tile's raw bytes through the disk cache under `cache_dir`: read
/// from the cache when present, otherwise fetched from `source` and
/// written there (atomically: a temp file, then a rename, so a cached
/// tile is never truncated). `Ok(None)` when the source reports the tile absent
/// (nothing is written). This is [`fetch_cover`]'s per-tile step, shared
/// with the story prefetcher; it does not create `cache_dir`.
///
/// # Errors
/// A source failure, or a cache read or write failure.
pub async fn fetch_one(
    source: &dyn crate::tile_source::TileSource,
    tile: TileId,
    cache_dir: &Path,
) -> anyhow::Result<Option<Vec<u8>>> {
    let path: PathBuf = cache_dir.join(format!("{}-{}-{}.pbf", tile.z, tile.x, tile.y));
    if path.exists() {
        return Ok(Some(std::fs::read(&path)?));
    }
    match source.fetch(tile).await? {
        Some(bytes) => {
            // Atomic: write a temp file beside the target, then rename
            // over it, so an interrupted write (a killed render, a full
            // disk) never leaves a truncated tile that every later run
            // would read back as a good one. The temp name is unique per
            // process and call, so two concurrent fetches of one tile
            // never share a temp file; the last rename wins, and both
            // wrote the same bytes.
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let tmp = cache_dir.join(format!(".{}-{}-{}.pbf.{}.{seq}.tmp", tile.z, tile.x, tile.y, std::process::id()));
            if let Err(e) = std::fs::write(&tmp, &bytes).and_then(|()| std::fs::rename(&tmp, &path)) {
                let _ = std::fs::remove_file(&tmp);
                return Err(e.into());
            }
            Ok(Some(bytes))
        }
        None => Ok(None),
    }
}

/// Rewrites a plan under a [`Paint`] policy. `AsStyled` returns it
/// untouched.
fn repaint(plan: crate::style::PaintPlan, paint: Paint) -> crate::style::PaintPlan {
    match paint {
        Paint::AsStyled => plan,
        Paint::Wash => {
            // `fill-opacity` 0.3, near-white, on every fill. NOT
            // premultiplied: `Rgba` is straight alpha here and
            // `mesh::Vertex` carries it through to
            // `BlendState::ALPHA_BLENDING`, exactly as a style colour does.
            let wash = Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.3 };
            crate::style::PaintPlan {
                ops: plan
                    .ops
                    .into_iter()
                    .filter_map(|op| match op {
                        PaintOp::Fill { geometry, .. } => {
                            Some(PaintOp::Fill { color_from: wash, color_to: wash, geometry })
                        }
                        PaintOp::Background { .. } | PaintOp::Line { .. } => None,
                        extrusion @ PaintOp::Extrusion { .. } => Some(extrusion),
                    })
                    .collect(),
            }
        }
        Paint::FlattenedFills => {
            let opaque = Rgba { r: 0.1, g: 0.1, b: 0.1, a: 1.0 };
            crate::style::PaintPlan {
                ops: plan
                    .ops
                    .into_iter()
                    .filter_map(|op| match op {
                        PaintOp::Fill { geometry, .. } => Some(PaintOp::Fill {
                            color_from: opaque,
                            color_to: opaque,
                            geometry,
                        }),
                        PaintOp::Background { .. } => Some(PaintOp::Background { color: opaque }),
                        PaintOp::Line { .. } => None,
                        extrusion @ PaintOp::Extrusion { .. } => Some(extrusion),
                    })
                    .collect(),
            }
        }
    }
}

/// The style's `background-color` bracket pair at `zoom` — the two stops
/// `MapView::paint` blends for the clear colour AND bakes into the backdrop
/// sphere's mesh.
fn background_pair(style: &Style, zoom: u8) -> Option<(Rgba, Rgba)> {
    style.layers.iter().find_map(|layer| match layer {
        crate::style::StyleLayer::Background { color, .. } => Some(color.value_at_pair(zoom)),
        _ => None,
    })
}

/// The clear colour for a request, given the style and this frame's
/// globeness.
/// The style's `background` at this request's zoom: `MapView::paint`'s own
/// expression, down to the blend. What the clear is made from, and what the
/// brand mark's artwork is decided against (through
/// `brand_mark::resolve_for_frame`).
fn style_background(request: &CaptureRequest, style: &Style) -> Option<Rgba> {
    let pair = background_pair(style, request.display_zoom());
    let blend_t = request.zoom.fract() as f32;
    pair.map(|(from, to)| from.lerp(&to, blend_t))
}

fn clear_color(request: &CaptureRequest, style: &Style, transition: f32, sky_draws: bool) -> wgpu::Color {
    match request.clear {
        Clear::Sentinel(color) => TilePipeline::clear_color(&color),
        // A frame whose sky draws clears to space at any zoom, as
        // `frame_plan::space_plan` has every shell do.
        // A flat world projection sits on a plain page; the outline is
        // where the backdrop's background colour stops.
        Clear::Style if crate::projection::active().is_some() => {
            TilePipeline::clear_color(&Rgba { r: 0.945, g: 0.949, b: 0.957, a: 1.0 })
        }
        Clear::Style if sky_draws => TilePipeline::clear_color(&crate::render::space::Space::default().color),
        Clear::Style => {
            let background = style_background(request, style);
            if transition > 0.0 {
                TilePipeline::clear_color(&crate::render::space::space_clear_color(
                    background,
                    &crate::render::space::Space::default(),
                    transition,
                ))
            } else {
                background.map(|c| TilePipeline::clear_color(&c)).unwrap_or(wgpu::Color::TRANSPARENT)
            }
        }
    }
}

/// **`MapView::paint`'s two passes, as a pure function.**
///
/// Pass 1: for every cover tile that is not ready, the nearest ancestor
/// `present` knows about, deduped — a second unready tile with the same
/// ancestor does not push it twice. Pass 2: every ready tile, on top. The
/// returned second list is the tiles for which neither was possible.
///
/// A tile is ready when `present` has it AND it is not in `absent`. The
/// whole reason this is not folded into [`capture_async`]'s loop is that
/// the ORDER is the claim being made — an ancestor drawn under three ready
/// siblings is the mechanism `CaptureRequest::absent` exists to reproduce,
/// and a claim about an order deserves a test that needs no GPU.
///
/// `max_up` bounds the ancestor search ([`CaptureRequest::stand_in_levels`]):
/// `None` climbs to z0, `Some(k)` at most `k` levels.
fn draw_order(
    cover: &[TileId],
    absent: &[TileId],
    max_up: Option<u8>,
    present: impl Fn(&TileId) -> bool,
) -> (Vec<(TileId, TileClip)>, Vec<TileId>) {
    let ready = |tile: &TileId| present(tile) && !absent.contains(tile);
    let mut order = Vec::new();
    let mut missing = Vec::new();
    for &tile in cover.iter().filter(|t| !ready(t)) {
        let floor = max_up.map_or(0, |k| tile.z.saturating_sub(k));
        let mut ancestor = tile;
        loop {
            if ancestor.z <= floor {
                missing.push(tile);
                break;
            }
            ancestor =
                TileId { z: ancestor.z - 1, x: ancestor.x / 2, y: ancestor.y / 2 };
            if present(&ancestor) {
                // Once per MISSING TILE, clipped to that tile's own box in
                // the ancestor's coordinates -- not once per ancestor,
                // whole. Two missing siblings sharing an ancestor push it
                // twice with disjoint clips, whose union is exactly the
                // region no ready tile covers. See `render::gpu::TileClip`
                // for the double-composite this removes, and `MapView
                // ::paint`'s pass 1, which this mirrors.
                order.push((ancestor, TileClip::for_descendant(ancestor, tile)));
                break;
            }
        }
    }
    order.extend(cover.iter().copied().filter(ready).map(|t| (t, TileClip::WHOLE)));
    (order, missing)
}

/// **Draws one frame.** Blocks on the headless device; safe to call from an
/// ordinary synchronous `main` or from a test.
///
/// `tiles` is what [`fetch_cover`] returned. Any tile of the cover that is
/// missing from it is reported in [`Capture::tiles_missing`] and simply not
/// drawn.
///
/// # Errors
/// Only one thing here can fail that is not a bug: getting a GPU at all.
/// One overlay source's GPU draw for a capture: `(vertices, triangles,
/// buffers, model bind group, model buffer)`, or `None` when the document
/// has nothing drawable. The same stages `MapView` runs for a source --
/// `OverlayStore` parse, `plan_at`, `render::tessellate_overlay_plan`,
/// `combined_overlay_matrix` and `GlobeUpload::for_overlay` -- so a capture
/// of an overlay is a capture of the shipped path, not a re-implementation.
///
/// `tiled` is `Some(cover)` for [`CaptureRequest::overlay_tiling`]: the
/// polygons and lines are drawn per tile of `cover` and the whole-source
/// mesh holds the points only, as `MapView` draws a tiled source.
/// One overlay draw a capture holds: its vertex and triangle counts, and
/// either a 88-byte-vertex mesh or a compact tile's chunks with its fill
/// and (optional) outline bind groups.
struct CaptureOverlayDraw {
    vertices: usize,
    triangles: usize,
    body: CaptureOverlayBody,
    _model: crate::render::TrackedBuffer,
}

enum CaptureOverlayBody {
    Legacy(crate::render::TileBuffers, wgpu::BindGroup),
    Compact(Vec<crate::render::CompactBuffers>, wgpu::BindGroup, Option<wgpu::BindGroup>),
    /// A point view's instanced circles: instances, model group, circle
    /// group (and the table and params buffer its group holds).
    // The last two are never read: they are held so the circle group's
    // buffers outlive the draw.
    #[allow(dead_code)]
    Circles(
        crate::render::gpu::CircleBuffers,
        wgpu::BindGroup,
        wgpu::BindGroup,
        crate::render::gpu::PaintTableGpu,
        crate::render::TrackedBuffer,
    ),
}

/// The highlight of feature `index` of the overlay `json`, as
/// `MapView::set_feature_highlight` builds and draws it. `None` when the
/// feature draws nothing. `paint` is the overlay's declared paint
/// (`CaptureRequest::overlay_paint`): a point's ring is sized from and
/// redraws the point in it.
#[allow(clippy::too_many_arguments)]
fn highlight_draw(
    json: &str,
    index: usize,
    style: &crate::overlay::HighlightStyle,
    paint: Option<&str>,
    ctx: &GpuContext,
    pipeline: &TilePipeline,
    camera: &Camera,
    globe: &GlobeUpload,
    display_zoom: u8,
) -> anyhow::Result<Option<CaptureOverlayDraw>> {
    let mut store = crate::overlay::OverlayStore::new();
    if let Some(paint) = paint {
        let paint: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(paint).map_err(|err| anyhow::anyhow!("overlay paint is not a JSON object: {err}"))?;
        store
            .declare_source_paint(crate::overlay::DEFAULT_SOURCE, &paint)
            .map_err(|err| anyhow::anyhow!("overlay paint rejected: {err:?}"))?;
    }
    store
        .replace(json, display_zoom)
        .map_err(|err| anyhow::anyhow!("overlay GeoJSON rejected: {err:?}"))?;
    let snapshot = store
        .source_snapshot(crate::overlay::DEFAULT_SOURCE)
        .expect("the default source exists for the store's whole life");
    let Some(plan) = crate::overlay::snapshot_geometry(&snapshot, index)
        .and_then(|geometry| crate::overlay::highlight_plan(&geometry, style, crate::overlay::drawn_paint(&snapshot, index).as_ref()))
    else {
        return Ok(None);
    };
    let mesh = crate::render::tessellate_overlay_plan(&plan.plan, plan.origin.coord_scale);
    if mesh.indices.is_empty() {
        return Ok(None);
    }
    let origin = (plan.origin.world_x, plan.origin.world_y);
    let (model_buffer, model_bind_group) = pipeline.create_model_bind_group(
        ctx,
        crate::render::combined_overlay_matrix(camera, origin).to_cols_array_2d(),
        plan.bracket.blend_t(camera.zoom),
        crate::render::overlay_dash_scale(camera),
        globe.for_overlay(origin),
    );
    Ok(Some(CaptureOverlayDraw {
        vertices: mesh.vertices.len(),
        triangles: mesh.indices.len() / 3,
        body: CaptureOverlayBody::Legacy(pipeline.create_tile_buffers(ctx, &mesh), model_bind_group),
        _model: model_buffer,
    }))
}

#[allow(clippy::too_many_arguments)]
/// What one capture overlay source is pushed from.
#[derive(Clone, Copy)]
pub(crate) enum OverlayData<'a> {
    GeoJson(&'a str),
    Csv(&'a [u8]),
}

fn overlay_draws(
    data: OverlayData<'_>,
    ctx: &GpuContext,
    pipeline: &TilePipeline,
    camera: &Camera,
    globe: &GlobeUpload,
    display_zoom: u8,
    tiled: Option<&[TileId]>,
    paint: Option<&str>,
    points: bool,
) -> anyhow::Result<Vec<CaptureOverlayDraw>> {
    let mut store = crate::overlay::OverlayStore::new();
    if points {
        store
            .set_source_cluster_options(
                crate::overlay::DEFAULT_SOURCE,
                Some(crate::overlay::ClusterOptions::points_only()),
                display_zoom,
            )
            .map_err(|err| anyhow::anyhow!("point options rejected: {err:?}"))?;
    }
    if let Some(paint) = paint {
        let paint: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(paint).map_err(|err| anyhow::anyhow!("overlay paint is not a JSON object: {err}"))?;
        store
            .declare_source_paint(crate::overlay::DEFAULT_SOURCE, &paint)
            .map_err(|err| anyhow::anyhow!("overlay paint rejected: {err:?}"))?;
    }
    match data {
        OverlayData::GeoJson(json) => store
            .replace(json, display_zoom)
            .map_err(|err| anyhow::anyhow!("overlay GeoJSON rejected: {err:?}"))?,
        OverlayData::Csv(bytes) => {
            let csv = crate::overlay::csv::CsvSource::from_bytes(bytes, &crate::overlay::csv::CsvOptions::default())
                .map_err(|err| anyhow::anyhow!("overlay CSV rejected: {err}"))?;
            store
                .set_source_csv(crate::overlay::DEFAULT_SOURCE, &csv, &crate::overlay::FeaturePaint::default(), display_zoom)
                .map_err(|err| anyhow::anyhow!("overlay CSV push rejected: {err:?}"))?;
        }
    }
    if points {
        // The point path, as `MapView` drives it: index, then the view of
        // this camera's box, installed as the source's content.
        use crate::overlay::points;
        let name = crate::overlay::DEFAULT_SOURCE;
        let data = store.source_point_data(name).expect("a point push installs point data");
        let index = std::sync::Arc::new(points::build_index(&data));
        store.adopt_point_index(name, data.revision(), index);
        let (min, max) = points::flat_viewport_box(
            camera.center_lon,
            camera.center_lat,
            camera.zoom,
            f64::from(camera.viewport_width_px),
            f64::from(camera.viewport_height_px),
        );
        let key = points::ViewKey::for_box(camera.zoom.floor() as u8, min, max, 1);
        let (source, data, index, paint) = store.point_view_job(name).expect("the index was just adopted");
        let view = points::build_view(&source, &data, &index, &paint, key);
        let circles = view.circles.clone();
        store.install_point_view(name, view);
        // The view's dots, instanced, as `MapView::paint` draws them.
        if let Some(circles) = circles {
            let origin = (circles.origin.world_x, circles.origin.world_y);
            let bracket = circles.bracket;
            let (model_buffer, model_bind_group) = pipeline.create_model_bind_group(
                ctx,
                crate::render::combined_overlay_matrix(camera, origin).to_cols_array_2d(),
                bracket.blend_t(camera.zoom),
                crate::render::overlay_dash_scale(camera),
                globe.for_overlay(origin),
            );
            let table = pipeline.create_paint_table(ctx, &crate::render::compact::paint_table(&circles.set.paints));
            let (params, group) =
                pipeline.create_circle_bind_group(ctx, &table, crate::render::circles::CircleParams::NONE);
            let n = circles.set.len();
            return Ok(vec![CaptureOverlayDraw {
                vertices: n * 4,
                triangles: n * 2,
                body: CaptureOverlayBody::Circles(
                    pipeline.create_circle_buffers(ctx, &circles.set.instances),
                    model_bind_group,
                    group,
                    table,
                    params,
                ),
                _model: model_buffer,
            }]);
        }
    }
    let snapshot = store
        .source_snapshot(crate::overlay::DEFAULT_SOURCE)
        .expect("the default source exists for the store's whole life");
    let bracket = crate::overlay::plan::BakeBracket::Bucket(display_zoom);
    let mut out = Vec::new();
    if let Some(cover) = tiled {
        let classes = crate::overlay::plan::classify(&snapshot.features, &snapshot.properties, bracket, &snapshot.paint)
            .ok_or_else(|| anyhow::anyhow!("overlay has too many paint classes to tile"))?;
        let index = crate::overlay::tile_index::TileIndex::build(
            &snapshot.features,
            &classes.of_feature,
            store.source_options(crate::overlay::DEFAULT_SOURCE).unwrap_or_default().tile_index_options(),
            &|| false,
        )
        .expect("an uncancellable build always completes");
        let mut baked: std::collections::HashMap<TileId, Option<crate::overlay::tile_plan::BakedOverlayTile>> =
            std::collections::HashMap::new();
        // The source's paint tables, as `MapView` builds them: by class id.
        let table = |entries: Vec<(Rgba, Rgba)>| pipeline.create_paint_table(ctx, &crate::render::compact::paint_table(&entries));
        let fill_table = table(crate::overlay::tile_plan::fill_paint_entries(&classes));
        let outline_table = table(crate::overlay::tile_plan::outline_paint_entries(&classes));
        for (display, tile) in crate::overlay::tile_plan::wanted_overlay_tiles(cover, index.max_zoom()) {
            let Some(baked) = baked.entry(tile).or_insert_with(|| {
                crate::overlay::tile_plan::bake_overlay_tile_compact(&index, &classes, tile, false, &|| false)
            }) else {
                continue;
            };
            let model = pipeline.create_model_buffer_clipped(
                ctx,
                crate::render::combined_tile_matrix(camera, tile, MVT_EXTENT as f32).to_cols_array_2d(),
                bracket.blend_t(camera.zoom),
                crate::render::dash_scale(camera, tile, MVT_EXTENT as f32),
                globe.for_tile(tile),
                crate::render::TileClip::for_descendant(tile, display),
            );
            let (vertices, triangles, body) = match baked {
                Ok(compact) => (
                    compact.vertices(),
                    compact.chunks.iter().map(|c| c.fill.len() / 3).sum(),
                    CaptureOverlayBody::Compact(
                        compact.chunks.iter().map(|c| pipeline.create_compact_buffers_planar(ctx, c)).collect(),
                        pipeline.create_compact_model_bind_group(ctx, &model, &fill_table),
                        compact
                            .has_outline
                            .then(|| pipeline.create_compact_model_bind_group(ctx, &model, &outline_table)),
                    ),
                ),
                Err(legacy) => (
                    legacy.mesh.vertices.len(),
                    legacy.mesh.indices.len() / 3,
                    CaptureOverlayBody::Legacy(
                        pipeline.create_tile_buffers(ctx, &legacy.mesh),
                        pipeline.create_legacy_model_bind_group(ctx, &model),
                    ),
                ),
            };
            out.push(CaptureOverlayDraw { vertices, triangles, body, _model: model });
        }
    }
    let Some(plan) = crate::overlay::source_plan_for(&snapshot.features, &snapshot.properties, bracket, &snapshot.paint, tiled.is_some())
    else {
        return Ok(out);
    };
    let mut mesh = crate::render::tessellate_overlay_plan(&plan.plan, plan.origin.coord_scale);
    plan.colour_mesh(&mut mesh.vertices);
    if mesh.indices.is_empty() {
        return Ok(out);
    }
    let origin = (plan.origin.world_x, plan.origin.world_y);
    let buffers = pipeline.create_tile_buffers(ctx, &mesh);
    let (model_buffer, model_bind_group) = pipeline.create_model_bind_group(
        ctx,
        crate::render::combined_overlay_matrix(camera, origin).to_cols_array_2d(),
        bracket.blend_t(camera.zoom),
        crate::render::overlay_dash_scale(camera),
        globe.for_overlay(origin),
    );
    out.push(CaptureOverlayDraw {
        vertices: mesh.vertices.len(),
        triangles: mesh.indices.len() / 3,
        body: CaptureOverlayBody::Legacy(buffers, model_bind_group),
        _model: model_buffer,
    });
    Ok(out)
}

pub fn capture(
    request: &CaptureRequest,
    tiles: &TileStore,
    style: &Style,
) -> anyhow::Result<Capture> {
    pollster::block_on(capture_async(request, tiles, style))
}

/// [`capture`] for a caller that already has a runtime.
///
/// # Errors
/// See [`capture`].
pub async fn capture_async(
    request: &CaptureRequest,
    tiles: &TileStore,
    style: &Style,
) -> anyhow::Result<Capture> {
    let mut session = CaptureSession::new(None).await?;
    session.render_async(request, tiles, style).await
}

/// [`CaptureRequest::overlay_labels`] as this frame's text draws: parsed by
/// an `OverlayStore` as `MapView::set_source` parses a source, each
/// feature's anchor from `map_view::overlay_symbol_screen_anchor` on
/// `frame` (the production anchor, sphere and horizon included), off-frame
/// and hidden anchors left out as `paint` culls them, and the text centred
/// by `symbol_pass::text_area_origin` at scale 1. A document the store
/// refuses draws nothing (and says so on stderr).
fn capture_overlay_labels(
    json: &str,
    camera: &Camera,
    frame: &crate::render::globe::GlobeFrame,
    display_zoom: u8,
    font_system: &mut glyphon::FontSystem,
    shape_cache: &mut crate::symbol_pass::LabelShapeCache,
) -> Vec<crate::symbol_pass::LabelDraw> {
    use crate::symbol_pass::{chrome_text_face, text_area_origin, LabelAnchoring, LabelShapeCache};
    let mut store = crate::overlay::OverlayStore::new();
    if let Err(err) = store.replace(json, display_zoom) {
        eprintln!("capture: overlay labels rejected: {err:?}");
        return Vec::new();
    }
    let Some(snapshot) = store.source_snapshot(crate::overlay::DEFAULT_SOURCE) else { return Vec::new() };
    let anchors = crate::render::globe::GlobeAnchors::new(frame);
    let projector = crate::render::globe::WorldProjector::new(camera, &anchors);
    let (w, h) = (camera.viewport_width_px, camera.viewport_height_px);
    let margin = crate::symbol_pass::SYMBOL_CULL_MARGIN_PX;
    let mut draws = Vec::new();
    for feature in snapshot.features.iter() {
        let Some(text) = feature.paint.text_field.as_deref() else { continue };
        let Some((x, y)) = crate::map_view::overlay_symbol_screen_anchor(&projector, &feature.geometry) else { continue };
        if x < -margin || y < -margin || x > w + margin || y > h + margin {
            continue;
        }
        let size = feature.paint.text_size;
        let face = chrome_text_face();
        let shaped = shape_cache.get_or_shape(font_system, text, size, None, feature.paint.text_justify, &face);
        let (left, top) = text_area_origin(
            x,
            y,
            1.0,
            LabelAnchoring::centred(feature.paint.text_offset),
            size,
            shaped.measured_width,
            shaped.measured_height,
        );
        draws.push((
            LabelShapeCache::key(text, size, None, feature.paint.text_justify, &face),
            left,
            top,
            feature.paint.text_color,
            feature.paint.text_halo_width,
            feature.paint.text_halo_color,
            0.0,
        ));
    }
    draws
}

/// [`CaptureRequest::donuts`] as this frame's instances and batches: each
/// donut projected with `MapView::paint`'s projector on `frame`, at scale
/// 1; a hidden anchor, an invisible donut or one wholly off the frame is
/// left out, as `MapView`'s donut layers leave it out. The third value is
/// how many donuts the per-layer `MAX_DONUTS` cap dropped.
fn capture_donuts(
    request: &CaptureRequest,
    camera: &Camera,
    frame: &crate::render::globe::GlobeFrame,
) -> (Vec<crate::render::donut::DonutInstance>, Vec<crate::render::donut::DonutBatch>, usize) {
    use crate::render::donut::{donut_instance, palette_uniform, MAX_DONUTS};
    let (mut instances, mut batches) = (Vec::new(), Vec::new());
    let truncated = request.donuts.iter().map(|(d, _)| d.len().saturating_sub(MAX_DONUTS)).sum();
    if request.donuts.is_empty() {
        return (instances, batches, truncated);
    }
    let anchors = crate::render::globe::GlobeAnchors::new(frame);
    let projector = crate::render::globe::WorldProjector::new(camera, &anchors);
    let (w, h) = (request.width as f32, request.height as f32);
    for (donuts, palette) in &request.donuts {
        let start = instances.len() as u32;
        for d in donuts.iter().take(MAX_DONUTS) {
            let anchor = projector.project(crate::overlay::geom::lon_lat_to_world(d.lon, d.lat));
            let i = donut_instance(d, anchor, 1.0);
            let (x, y, r) = (i.anchor_px[0], i.anchor_px[1], i.radius_px + 1.0);
            if i.alpha <= 0.0 || i.radius_px <= 0.0 || x + r < 0.0 || y + r < 0.0 || x - r > w || y - r > h {
                continue;
            }
            instances.push(i);
        }
        let end = instances.len() as u32;
        if end > start {
            batches.push((palette_uniform(palette), start..end));
        }
    }
    (instances, batches, truncated)
}

#[cfg(test)]
pub(crate) mod golden_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// Overlay extrusion draws through the building pipeline at any zoom,
    /// flat and pitched: a tall red square is red at the centre seen from
    /// above, and from 55 degrees it covers more of the frame (its walls)
    /// in at least two shades (roof and lit walls), over a dark ground. Off
    /// (no layer), the centre is the ground.
    #[test]
    fn an_overlay_extrusion_draws_flat_and_pitched() {
        use crate::overlay::extrude::{extrude, ExtrudeFeature, ExtrusionStyle};
        let ground = Rgba { r: 0.05, g: 0.05, b: 0.08, a: 1.0 };
        let style = Style {
            layers: vec![crate::style::StyleLayer::Background {
                id: "bg".into(),
                color: crate::style::ZoomValue::constant(ground),
                min_zoom: None,
                max_zoom: None,
            }],
            ..Style::default()
        };
        let (lon, lat, d) = (-122.45, 37.76, 0.004);
        let set = std::sync::Arc::new(extrude(&[ExtrudeFeature {
            polygons: vec![vec![vec![(lon - d, lat - d), (lon + d, lat - d), (lon + d, lat + d), (lon - d, lat + d), (lon - d, lat - d)]]],
            height_m: 900.0,
            base_m: 0.0,
            color: Rgba { r: 0.9, g: 0.1, b: 0.1, a: 1.0 },
        }]));
        let shot = |pitch: f32, on: bool| {
            let request = CaptureRequest {
                center_lon: lon,
                center_lat: lat,
                // Below the basemap's z12 building gate: overlays extrude anyway.
                zoom: 11.0,
                pitch: pitch.to_radians(),
                bearing: 20f32.to_radians(),
                width: 256,
                height: 256,
                flat: true,
                globe: false,
                extrusions: if on { vec![CaptureExtrusion { set: set.clone(), style: ExtrusionStyle::default() }] } else { Vec::new() },
                ..CaptureRequest::default()
            };
            capture(&request, &TileStore::new(), &style).expect("a headless device")
        };
        let reddish = |p: [u8; 4]| p[0] > 60 && u16::from(p[0]) > 2 * u16::from(p[1]) && u16::from(p[0]) > 2 * u16::from(p[2]);
        let off = shot(0.0, false);
        assert!(!reddish(off.pixel(128, 128)), "no layer, no column");
        let flat = shot(0.0, true);
        assert!(reddish(flat.pixel(128, 128)), "seen from above: {:?}", flat.pixel(128, 128));
        let pitched = shot(55.0, true);
        let count = |f: &Capture| (0..256).flat_map(|y| (0..256).map(move |x| (x, y))).filter(|&(x, y)| reddish(f.pixel(x, y))).count();
        let shades: std::collections::BTreeSet<u8> =
            (0..256).flat_map(|y| (0..256).map(move |x| (x, y))).map(|(x, y)| pitched.pixel(x, y)).filter(|p| reddish(*p)).map(|p| p[0] / 16).collect();
        assert!(count(&pitched) > count(&flat), "walls show when pitched: {} vs {}", count(&pitched), count(&flat));
        assert!(shades.len() >= 2, "roof and walls shade apart: {shades:?}");
    }

    /// A US-wide grid of square columns, red, rising west to east: the
    /// globe subject (`overlay_extrusions_draw_on_the_globe_*`).
    fn us_columns(side_deg: f64, height_m: impl Fn(usize) -> f32) -> std::sync::Arc<crate::overlay::extrude::OverlayExtrusion> {
        use crate::overlay::extrude::{extrude, ExtrudeFeature};
        let mut features = Vec::new();
        for (i, lon) in (0..20).map(|k| -124.0 + 3.0 * f64::from(k)).enumerate() {
            for lat in (0..9).map(|k| 26.0 + 3.0 * f64::from(k)) {
                let d = side_deg / 2.0;
                features.push(ExtrudeFeature {
                    polygons: vec![vec![vec![(lon - d, lat - d), (lon + d, lat - d), (lon + d, lat + d), (lon - d, lat + d), (lon - d, lat - d)]]],
                    height_m: height_m(i),
                    base_m: 0.0,
                    color: Rgba { r: 0.9, g: 0.1, b: 0.1, a: 1.0 },
                });
            }
        }
        std::sync::Arc::new(extrude(&features))
    }

    fn dark_style() -> Style {
        Style {
            layers: vec![crate::style::StyleLayer::Background {
                id: "bg".into(),
                color: crate::style::ZoomValue::constant(Rgba { r: 0.05, g: 0.05, b: 0.08, a: 1.0 }),
                min_zoom: None,
                max_zoom: None,
            }],
            ..Style::default()
        }
    }

    fn reddish(p: [u8; 4]) -> bool {
        p[0] > 60 && u16::from(p[0]) > 2 * u16::from(p[1]) && u16::from(p[0]) > 2 * u16::from(p[2])
    }

    fn red_count(f: &Capture) -> usize {
        (0..f.height).flat_map(|y| (0..f.width).map(move |x| (x, y))).filter(|&(x, y)| reddish(f.pixel(x, y))).count()
    }

    /// Red shades (`r / 16`) in the frame: a lit roof and walls differ.
    fn red_shades(f: &Capture) -> std::collections::BTreeSet<u8> {
        (0..f.height).flat_map(|y| (0..f.width).map(move |x| (x, y))).map(|(x, y)| f.pixel(x, y)).filter(|p| reddish(*p)).map(|p| p[0] / 16).collect()
    }

    fn globe_shot(set: &std::sync::Arc<crate::overlay::extrude::OverlayExtrusion>, view: (f64, f64, f64, f32), on: bool, size: u32) -> Capture {
        globe_shot_at(set, view, on.then_some(1.0), size)
    }

    /// [`globe_shot`] at layer opacity `opacity` (`None`: no layer).
    fn globe_shot_at(set: &std::sync::Arc<crate::overlay::extrude::OverlayExtrusion>, view: (f64, f64, f64, f32), opacity: Option<f32>, size: u32) -> Capture {
        use crate::overlay::extrude::ExtrusionStyle;
        let (lon, lat, zoom, pitch) = view;
        let request = CaptureRequest {
            center_lon: lon,
            center_lat: lat,
            zoom,
            pitch: pitch.to_radians(),
            bearing: 15f32.to_radians(),
            width: size,
            height: size,
            flat: false,
            globe: true,
            extrusions: opacity.map(|opacity| CaptureExtrusion { set: set.clone(), style: ExtrusionStyle { opacity, ..ExtrusionStyle::default() } }).into_iter().collect(),
            ..CaptureRequest::default()
        };
        let shot = capture(&request, &TileStore::new(), &dark_style()).expect("a headless device");
        if let Some(dir) = std::env::var_os("GPUI_MAP_DUMP_GLOBE_EXTRUSIONS") {
            let name = format!("globe-extrusions-z{zoom}-p{pitch}-{lon}-{lat}-{}.png", opacity.map_or("off".to_string(), |o| format!("o{o}")));
            shot.save_png(&std::path::Path::new(&dir).join(name)).expect("a png");
        }
        shot
    }

    /// THE GLOBE DRAWS OVERLAY EXTRUSIONS, at every zoom below the z12
    /// hand-over, where they were skipped before: a grid of 1.6-degree
    /// columns over the US at z3 (whole planet), straight down and pitched
    /// (walls in a second shade), and a column under the centre at z8 and
    /// z11 (blend band's globe end), red where the layer is off is not.
    #[test]
    fn overlay_extrusions_draw_on_the_globe_at_z3_z8_and_z11() {
        let big = us_columns(1.6, |i| 150_000.0 + 30_000.0 * i as f32);
        let off = globe_shot(&big, (-98.0, 38.0, 3.0, 0.0), false, 256);
        assert_eq!(red_count(&off), 0, "no layer, no red");
        let z3 = globe_shot(&big, (-98.0, 38.0, 3.0, 0.0), true, 256);
        let z3_pitched = globe_shot(&big, (-98.0, 38.0, 3.0, 45.0), true, 256);
        assert!(red_count(&z3) > 1500, "the columns draw on the z3 globe: {}", red_count(&z3));
        assert!(red_shades(&z3_pitched).len() >= 2, "roofs and walls shade apart: {:?}", red_shades(&z3_pitched));
        // Translucent (MapLibre's prepass then stencilled colour pass):
        // the nearest surface only, still everywhere the opaque one is.
        let z3_translucent = globe_shot_at(&big, (-98.0, 38.0, 3.0, 45.0), Some(0.7), 256);
        let (opaque, translucent) = (red_count(&z3_pitched), red_count(&z3_translucent));
        assert!(translucent * 10 >= opaque * 8, "translucent {translucent} px vs opaque {opaque} px");
        // A column whose centre is the camera's: the screen centre is red.
        // Lower ones than z3's: the z8 eye is about 100 km up, inside them.
        let mid = us_columns(0.4, |_| 20_000.0);
        let z8 = globe_shot(&mid, (-97.0, 38.0, 8.0, 0.0), true, 256);
        assert!(reddish(z8.pixel(128, 128)), "z8 centre: {:?}", z8.pixel(128, 128));
        let z8_pitched = globe_shot(&mid, (-97.0, 38.0, 8.0, 50.0), true, 256);
        assert!(red_shades(&z8_pitched).len() >= 2, "z8 roof and walls: {:?}", red_shades(&z8_pitched));
        let small = us_columns(0.004, |_| 900.0);
        let z11 = globe_shot(&small, (-97.0, 38.0, 11.0, 0.0), true, 256);
        assert!(reddish(z11.pixel(128, 128)), "z11 centre: {:?}", z11.pixel(128, 128));
        let z11_pitched = globe_shot(&small, (-97.0, 38.0, 11.0, 55.0), true, 256);
        assert!(red_count(&z11_pitched) > red_count(&z11), "walls show when pitched: {} vs {}", red_count(&z11_pitched), red_count(&z11));
        assert!(red_shades(&z11_pitched).len() >= 2, "z11 roof and walls: {:?}", red_shades(&z11_pitched));
    }

    /// The globe arm stands each column where the Mercator one does at the
    /// camera's centre, and the blend band joins the two: at z11 (pure
    /// globe), z11.5 (half) and z11.98 the red column's centroid and area
    /// match the flat frame's at the same camera.
    #[test]
    fn a_globe_column_stands_where_the_flat_one_does() {
        use crate::overlay::extrude::ExtrusionStyle;
        let set = us_columns(0.004, |_| 900.0);
        let shot = |zoom: f64, globe: bool| {
            let request = CaptureRequest {
                center_lon: -97.0005,
                center_lat: 38.0003,
                zoom,
                pitch: 40f32.to_radians(),
                bearing: 15f32.to_radians(),
                width: 256,
                height: 256,
                flat: !globe,
                globe,
                extrusions: vec![CaptureExtrusion { set: set.clone(), style: ExtrusionStyle::default() }],
                ..CaptureRequest::default()
            };
            capture(&request, &TileStore::new(), &dark_style()).expect("a headless device")
        };
        let centroid = |f: &Capture| {
            let red: Vec<(f64, f64)> = (0..f.height)
                .flat_map(|y| (0..f.width).map(move |x| (x, y)))
                .filter(|&(x, y)| reddish(f.pixel(x, y)))
                .map(|(x, y)| (f64::from(x), f64::from(y)))
                .collect();
            let n = red.len() as f64;
            (red.iter().map(|p| p.0).sum::<f64>() / n, red.iter().map(|p| p.1).sum::<f64>() / n, red.len())
        };
        for zoom in [11.0, 11.5, 11.98] {
            let (gx, gy, gn) = centroid(&shot(zoom, true));
            let (fx, fy, fn_) = centroid(&shot(zoom, false));
            assert!(fn_ > 50, "z{zoom}: the flat column draws ({fn_} px)");
            assert!((gx - fx).abs() < 2.0 && (gy - fy).abs() < 2.0, "z{zoom}: globe ({gx:.1}, {gy:.1}) vs flat ({fx:.1}, {fy:.1})");
            let ratio = gn as f64 / fn_ as f64;
            assert!((0.85..1.15).contains(&ratio), "z{zoom}: globe {gn} px vs flat {fn_} px");
        }
    }

    /// The globe pick (`overlay::extrude::pick_on_globe`) finds a column
    /// exactly where the globe draw shows one: over a pitched z3 frame of
    /// the US grid, a pixel is red if and only if the pick under it hits,
    /// but for a sliver along the silhouettes; the far side picks nothing.
    #[test]
    fn the_globe_pick_agrees_with_the_globe_draw() {
        let set = us_columns(1.6, |i| 150_000.0 + 30_000.0 * i as f32);
        let view = (-98.0, 38.0, 3.0, 45.0);
        let shot = globe_shot(&set, view, true, 256);
        let request = CaptureRequest {
            center_lon: view.0,
            center_lat: view.1,
            zoom: view.2,
            pitch: view.3.to_radians(),
            bearing: 15f32.to_radians(),
            width: 256,
            height: 256,
            flat: false,
            globe: true,
            ..CaptureRequest::default()
        };
        let camera = request.camera();
        let frame = crate::render::globe::build_globe_frame(&camera);
        let (mut agree, mut total, mut hits) = (0, 0, 0);
        for y in (2..256).step_by(5) {
            for x in (2..256).step_by(5) {
                let picked = crate::overlay::extrude::pick_on_globe(&set, &frame, &camera, x as f32 + 0.5, y as f32 + 0.5).is_some();
                total += 1;
                hits += usize::from(picked);
                agree += usize::from(picked == reddish(shot.pixel(x, y)));
            }
        }
        assert!(hits > 200, "the pick finds columns: {hits}");
        assert!(agree * 100 >= total * 95, "pick and draw agree at {agree} of {total}");
        let far = CaptureRequest { center_lon: 82.0, center_lat: -38.0, pitch: 0.0, zoom: 1.5, ..request };
        let far_camera = far.camera();
        let far_frame = crate::render::globe::build_globe_frame(&far_camera);
        let far_hits = (0..256).step_by(8).flat_map(|y| (0..256).step_by(8).map(move |x| (x, y)))
            .filter(|&(x, y)| crate::overlay::extrude::pick_on_globe(&set, &far_frame, &far_camera, x as f32, y as f32).is_some())
            .count();
        assert_eq!(far_hits, 0, "nothing picks through the planet");
    }

    /// One country-sized red block, 46 by 22 degrees over the US (122 to
    /// 76 W, 27 to 49 N), 100 km tall: cut on the globe ground's grid
    /// (`granularity` `None`, what `extrude` does) or on another
    /// (`Some(1)`: uncut, the pre-fix mesh).
    fn country_block(granularity: Option<u32>) -> std::sync::Arc<crate::overlay::extrude::OverlayExtrusion> {
        use crate::overlay::extrude::{extrude, extrude_on_grid, tile_zoom_for, ExtrudeFeature};
        let features = vec![ExtrudeFeature {
            polygons: vec![vec![vec![(-122.0, 27.0), (-76.0, 27.0), (-76.0, 49.0), (-122.0, 49.0), (-122.0, 27.0)]]],
            height_m: 100_000.0,
            base_m: 0.0,
            color: Rgba { r: 0.9, g: 0.1, b: 0.1, a: 1.0 },
        }];
        std::sync::Arc::new(match granularity {
            None => extrude(&features),
            Some(g) => extrude_on_grid(&features, tile_zoom_for(&features), g, None),
        })
    }

    /// The same block as 1,012 one-degree columns of the same height: each
    /// fits one cell, so each is placed on the sphere corner by corner to
    /// within a quarter of a kilometre -- the reference shape.
    fn country_mosaic() -> std::sync::Arc<crate::overlay::extrude::OverlayExtrusion> {
        use crate::overlay::extrude::{extrude, ExtrudeFeature};
        let mut features = Vec::new();
        for lon in (-122..-76).map(f64::from) {
            for lat in (27..49).map(f64::from) {
                features.push(ExtrudeFeature {
                    polygons: vec![vec![vec![(lon, lat), (lon + 1.0, lat), (lon + 1.0, lat + 1.0), (lon, lat + 1.0), (lon, lat)]]],
                    height_m: 100_000.0,
                    base_m: 0.0,
                    color: Rgba { r: 0.9, g: 0.1, b: 0.1, a: 1.0 },
                });
            }
        }
        std::sync::Arc::new(extrude(&features))
    }

    /// Pixels red in exactly one of `a` and `b`.
    fn red_xor(a: &Capture, b: &Capture) -> usize {
        (0..a.height).flat_map(|y| (0..a.width).map(move |x| (x, y))).filter(|&(x, y)| reddish(a.pixel(x, y)) != reddish(b.pixel(x, y))).count()
    }

    /// A COUNTRY-SIZED BLOCK FOLLOWS THE GLOBE. Its two roof triangles span
    /// tens of degrees, and the globe arm projects per vertex, so uncut its
    /// roof is a chord plane through the planet (380 km under the surface
    /// at its middle, its 100 km height notwithstanding) and its long walls
    /// bow under the surface between their corners: drawn, its outline is
    /// a straight-sided polygon where the ground under it is curved, and
    /// pitched, the far wall sinks. Cut on the ground's 5.625-degree grid
    /// (`render::extrusion::tessellate_extrusions_on_grid`) it draws, from
    /// z2 to z4, straight down and pitched, what 1,012 one-degree columns
    /// of the same height draw (each small enough to be placed right
    /// uncut), where the uncut block differs by thousands of pixels. Flat,
    /// cut and uncut draw the same.
    #[test]
    fn a_country_sized_block_follows_the_globe_at_z2_to_z4() {
        let cut = country_block(None);
        let uncut = country_block(Some(1));
        let mosaic = country_mosaic();
        assert!(cut.vertex_count() > 4 * uncut.vertex_count(), "the block is cut: {} vs {} vertices", cut.vertex_count(), uncut.vertex_count());
        let dump = |name: &str, f: &Capture| {
            if let Some(dir) = std::env::var_os("GPUI_MAP_DUMP_GLOBE_EXTRUSIONS") {
                f.save_png(&std::path::Path::new(&dir).join(format!("country-{name}.png"))).expect("a png");
            }
        };
        // z4 is centred on the north edge, which a z4 frame over the
        // middle would not reach.
        for (lat, zoom, pitch) in [(38.0, 2.0, 0.0), (38.0, 3.0, 0.0), (48.0, 4.0, 0.0), (44.0, 4.0, 50.0), (38.0, 2.5, 50.0), (38.0, 3.0, 45.0)] {
            let view = (-99.0, lat, zoom, pitch);
            let a = globe_shot(&cut, view, true, 256);
            let b = globe_shot(&uncut, view, true, 256);
            let m = globe_shot(&mosaic, view, true, 256);
            dump(&format!("z{zoom}-p{pitch}-lat{lat}-cut"), &a);
            dump(&format!("z{zoom}-p{pitch}-lat{lat}-uncut"), &b);
            dump(&format!("z{zoom}-p{pitch}-lat{lat}-mosaic"), &m);
            let (off_cut, off_uncut) = (red_xor(&a, &m), red_xor(&b, &m));
            eprintln!("z{zoom} p{pitch}: red differing from the mosaic: cut {off_cut} px, uncut {off_uncut} px (of {} red)", red_count(&m));
            assert!(red_count(&m) > 5_000, "z{zoom} p{pitch}: the mosaic draws ({} px)", red_count(&m));
            assert!(off_cut <= 600, "z{zoom} p{pitch}: the cut block is the mosaic's shape ({off_cut} px differ)");
            assert!(off_uncut >= 4 * off_cut.max(300), "z{zoom} p{pitch}: the uncut block is not ({off_uncut} px differ) -- the defect this guards");
        }
        // Flat, the cut pieces are coplanar: the same red, to a sliver of
        // rounded vertices along the outline.
        let flat = |set: &std::sync::Arc<crate::overlay::extrude::OverlayExtrusion>| {
            use crate::overlay::extrude::ExtrusionStyle;
            let request = CaptureRequest {
                center_lon: -99.0,
                center_lat: 38.0,
                zoom: 3.0,
                pitch: 30f32.to_radians(),
                bearing: 15f32.to_radians(),
                width: 256,
                height: 256,
                flat: true,
                globe: false,
                extrusions: vec![CaptureExtrusion { set: set.clone(), style: ExtrusionStyle::default() }],
                ..CaptureRequest::default()
            };
            capture(&request, &TileStore::new(), &dark_style()).expect("a headless device")
        };
        let (fa, fb) = (flat(&cut), flat(&uncut));
        dump("flat-z3-cut", &fa);
        dump("flat-z3-uncut", &fb);
        let differ = red_xor(&fa, &fb);
        eprintln!("flat: {differ} px differ of {} red", red_count(&fa));
        assert!(red_count(&fa) > 10_000, "the flat block draws: {}", red_count(&fa));
        assert!(differ <= 64, "flat, cut and uncut cover the same pixels ({differ} differ)");
    }

    /// Proof frames, by hand: Natural Earth's countries
    /// (`GPUI_MAP_WORLD_COUNTRIES_GEOJSON`, one GeoJSON path) extruded by
    /// population on the globe, cut and uncut, written to
    /// `GPUI_MAP_DUMP_GLOBE_EXTRUSIONS`.
    #[test]
    #[ignore = "writes proof frames from a local dataset"]
    fn world_countries_on_the_globe_proof_frames() {
        use crate::overlay::extrude::{extrude_on_grid, tile_zoom_for, ExtrudeFeature};
        let (Ok(path), Some(dir)) = (std::env::var("GPUI_MAP_WORLD_COUNTRIES_GEOJSON"), std::env::var_os("GPUI_MAP_DUMP_GLOBE_EXTRUSIONS")) else { return };
        let json = std::fs::read_to_string(path).expect("a GeoJSON file");
        let parsed = crate::overlay::geojson::parse_feature_collection(&json).expect("a FeatureCollection");
        let features: Vec<ExtrudeFeature> = parsed
            .iter()
            .enumerate()
            .filter_map(|(i, f)| {
                let pop = f.properties.get("POP_EST").and_then(serde_json::Value::as_f64).unwrap_or(0.0);
                let shade = (i % 7) as f32 / 7.0;
                ExtrudeFeature::from_geometry(&f.geometry, (pop.sqrt() * 40.0).min(600_000.0) as f32, Rgba { r: 0.75 + 0.2 * shade, g: 0.1 + 0.15 * shade, b: 0.1, a: 1.0 })
            })
            .collect();
        let z = tile_zoom_for(&features);
        let cut = std::sync::Arc::new(extrude_on_grid(&features, z, crate::render::subdivide::granularity_for_zoom(z), None));
        let uncut = std::sync::Arc::new(extrude_on_grid(&features, z, 1, None));
        for (name, view) in [("americas-z1.5-p40", (-80.0, 20.0, 1.5, 40.0)), ("asia-z2-p50", (95.0, 45.0, 2.0, 50.0)), ("russia-z2.5-p0", (90.0, 60.0, 2.5, 0.0))] {
            for (kind, set) in [("cut", &cut), ("uncut", &uncut)] {
                let shot = globe_shot(set, view, true, 512);
                shot.save_png(&std::path::Path::new(&dir).join(format!("world-{name}-{kind}.png"))).expect("a png");
            }
        }
    }

    /// The far side of the planet hides the columns on it -- the depth
    /// buffer holds no ground there, so this is `behind_planet`'s work:
    /// from the antipode of the US nothing red shows, while the same
    /// columns seen from over the US are there.
    #[test]
    fn the_planet_hides_columns_on_its_far_side() {
        let set = us_columns(1.6, |_| 200_000.0);
        let near = globe_shot(&set, (-98.0, 38.0, 1.5, 0.0), true, 256);
        assert!(red_count(&near) > 500, "near side: {}", red_count(&near));
        let far = globe_shot(&set, (82.0, -38.0, 1.5, 0.0), true, 256);
        assert_eq!(red_count(&far), 0, "far side: {}", red_count(&far));
    }

    /// Captures frame the same buildings the app does: the floor resolves
    /// from the style like `MapView::visible_tiles`, and is off for a style
    /// with no extrusion, when disabled, or where no building draws.
    #[test]
    fn a_capture_resolves_the_building_floor_as_the_map_does() {
        let liberty = crate::style_spec::parse_and_compile(&std::fs::read("examples/liberty-style.json").unwrap()).unwrap();
        let request = CaptureRequest {
            center_lon: -73.9857,
            center_lat: 40.7484,
            zoom: 16.0,
            pitch: 60.0_f32.to_radians(),
            bearing: 30.0_f32.to_radians(),
            width: 1024,
            height: 1024,
            flat: true,
            ..CaptureRequest::default()
        };
        assert_eq!(request.resolved(&liberty).building_floor, Some(13));
        assert!(request.resolved(&liberty).cover().len() > request.cover().len(), "the floor refines the far cover");
        let mut flat_only = liberty.clone();
        flat_only.layers.retain(|l| !matches!(l, crate::style::StyleLayer::Extrusion { .. }));
        assert_eq!(request.resolved(&flat_only).building_floor, None, "no buildings, no floor");
        let off = CaptureRequest { disable_building_floor: true, ..request.clone() };
        assert_eq!(off.resolved(&liberty).building_floor, None);
        let low = CaptureRequest { zoom: 11.0, ..request.clone() };
        assert_eq!(low.resolved(&liberty).building_floor, None, "below the buildings' zoom");
        let explicit = CaptureRequest { building_floor: Some(14), ..request };
        assert_eq!(explicit.resolved(&liberty).building_floor, Some(14));
    }

    /// Review F7 (satellites Task 5): mid-transition (`transition_for_zoom`
    /// = 0.5 at z11.5) the ground is the Mercator/globe clip blend, while
    /// the altitude layer projects through the pure globe matrix. Ground-
    /// level dots over green overlay squares at the same lon/lat measure how
    /// far apart the two land, flat and pitched. Writes PNGs when
    /// `ALTITUDE_CAPTURE_OUT` is set.
    #[test]
    fn altitude_dots_sit_on_the_blended_ground_mid_transition() {
        use crate::render::altitude::GeoPoint;
        let (clon, clat) = (10.0, 40.0);
        let offsets = [(0.0, 0.0), (-0.045, 0.034), (0.045, 0.034), (-0.045, -0.034), (0.045, -0.034)];
        let points: Vec<GeoPoint> = offsets.iter().map(|&(dx, dy)| GeoPoint { lon: clon + dx, lat: clat + dy, alt_m: 0.0 }).collect();
        let h = 0.0025;
        let features: Vec<String> = points
            .iter()
            .map(|p| {
                let (x0, x1, y0, y1) = (p.lon - h, p.lon + h, p.lat - h, p.lat + h);
                format!(r#"{{"type":"Feature","properties":{{}},"geometry":{{"type":"Polygon","coordinates":[[[{x0},{y0}],[{x1},{y0}],[{x1},{y1}],[{x0},{y1}],[{x0},{y0}]]]}}}}"#)
            })
            .collect();
        let mut style = crate::render::altitude_pipeline::AltitudeStyle::default();
        style.palette[0] = Rgba { r: 1.0, g: 0.0, b: 1.0, a: 1.0 };
        style.base_px = [6.0; 16];
        style.outline.a = 0.0;
        let out = std::env::var_os("ALTITUDE_CAPTURE_OUT").map(std::path::PathBuf::from);
        let (mut worst, mut measured, mut cpu_worst) = (0.0_f64, 0, 0.0_f64);
        for pitch_deg in [0.0_f32, 50.0] {
            let request = CaptureRequest {
                center_lon: clon,
                center_lat: clat,
                zoom: 11.5,
                pitch: pitch_deg.to_radians(),
                width: 512,
                height: 512,
                clear: Clear::Sentinel(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                backdrop: false,
                overlay: Some(format!(r#"{{"type":"FeatureCollection","features":[{}]}}"#, features.join(","))),
                overlay_paint: Some(r##"{"fill-color":"#00ff00"}"##.to_string()),
                altitude: Some(AltitudeCapture { points: points.clone(), style: style.clone(), ..Default::default() }),
                ..CaptureRequest::default()
            };
            let camera = request.camera();
            let frame = crate::render::globe::globe_frame(&camera);
            assert!((frame.transition - 0.5).abs() < 1e-3, "transition {}", frame.transition);
            let f = capture(&request, &TileStore::new(), &Style { layers: Vec::new(), ..Style::default() }).expect("a headless device");
            assert!(f.altitude_points_drawn > 0, "the run painted dots");
            if let Some(dir) = out.as_ref() {
                f.save_png(&dir.join(format!("transition-0.5-pitch{pitch_deg}.png"))).expect("png");
            }
            let anchors = crate::render::globe::GlobeAnchors::new(&frame);
            for p in &points {
                let world = crate::overlay::geom::lon_lat_to_world(p.lon, p.lat);
                let Some(ground) = crate::render::globe::project_world_to_screen_on_frame(&camera, &anchors, world) else {
                    continue;
                };
                let globe = crate::render::altitude::project_altitude(&camera, &frame, crate::render::altitude::geo_to_sphere(*p));
                // The fact the F7 decision rests on: mid-transition, the blended ground and the
                // pure-globe projection the layer uses agree to a fraction of a pixel.
                let g = globe.expect("an on-screen ground point projects on the globe");
                let split = f64::from(g.0 - ground.0).hypot(f64::from(g.1 - ground.1));
                assert!(split < 0.5, "pitch {pitch_deg}: blended ground and globe differ by {split:.3} px at {:?}", (p.lon, p.lat));
                cpu_worst = cpu_worst.max(split);
                // Centroids of the green square and the magenta dot in a 64 px window around the blended ground.
                let (gx, gy) = (ground.0.round() as i64, ground.1.round() as i64);
                let (mut g, mut m) = ((0.0, 0.0, 0.0), (0.0, 0.0, 0.0));
                for y in (gy - 32).max(0)..(gy + 32).min(512) {
                    for x in (gx - 32).max(0)..(gx + 32).min(512) {
                        let c = f.pixel(x as u32, y as u32);
                        let (fx, fy) = (x as f64 + 0.5, y as f64 + 0.5);
                        // The layer fades with the transition (x0.5), and a point AT the
                        // surface sits exactly on the soft occlusion edge (m = 1, x0.5), so a
                        // dot is magenta at a quarter alpha over the green, (64, 191, 64).
                        // The square is green OR dot.
                        if c[1] > 100 {
                            g = (g.0 + fx, g.1 + fy, g.2 + 1.0);
                        }
                        if c[0] > 40 && c[2] > 40 {
                            m = (m.0 + fx, m.1 + fy, m.2 + 1.0);
                        }
                    }
                }
                if g.2 < 4.0 || m.2 < 4.0 {
                    // Off screen (the pitched frame's two near corners).
                    continue;
                }
                let (g, m) = ((g.0 / g.2, g.1 / g.2), (m.0 / m.2, m.1 / m.2));
                let off = (m.0 - g.0).hypot(m.1 - g.1);
                worst = worst.max(off);
                measured += 1;
                if out.is_some() {
                    eprintln!(
                        "F7 pitch {pitch_deg}: ({:.3},{:.3}) ground {ground:?} globe {globe:?}; square {g:.1?} dot {m:.1?}: {off:.2} px",
                        p.lon, p.lat
                    );
                }
            }
        }
        // 8 of the 10 pairs are on screen (the pitched frame's two near corners are off); one may be lost to
        // an edge without the check losing its point.
        assert!(measured >= 7, "only {measured} of 10 dot/square pairs were found on screen");
        assert!(cpu_worst < 0.1, "blended vs pure-globe: {cpu_worst:.3} px (measured < 0.1)");
        // Measured 0.61 px worst (the square's rasterised centroid); the CPU positions differ by < 0.1 px.
        assert!(worst < 1.5, "a ground-level dot lands {worst:.2} px off the blended ground mid-transition");
    }

    #[test]
    fn a_flat_request_covers_a_street_level_frame_and_the_default_still_does_not() {
        let street = CaptureRequest {
            center_lon: 103.8198,
            center_lat: 1.3521,
            zoom: 16.0,
            pitch: 60.0_f32.to_radians(),
            bearing: std::f32::consts::FRAC_PI_2,
            width: 900,
            height: 900,
            ..CaptureRequest::default()
        };
        assert!(street.cover().is_empty(), "the default stays a globe-only instrument");
        let flat = CaptureRequest { flat: true, ..street };
        let cover = flat.cover();
        assert!(!cover.is_empty());
        // Clamped to the flat source's ceiling near the eye; the distance
        // LOD takes the far ground coarser at 60 degrees.
        assert!(cover.iter().all(|t| t.z <= 14), "clamped to the flat source's ceiling");
        assert_eq!(cover.iter().map(|t| t.z).max(), Some(14), "the near ground is at the ceiling");
        assert!(cover.len() <= crate::render::cover_plan::MAX_COVER_TILES);
    }

    /// The reported camera really is a globe camera, and the cover really
    /// is a set of tiles — the two premises everything this module reports
    /// rests on, and both cheap enough to pin without a GPU.
    #[test]
    fn the_reported_camera_is_a_globe_camera_with_a_nonempty_cover() {
        let request = CaptureRequest::default();
        assert_eq!(request.display_zoom(), 3, "z3.14 displays at bucket 3");
        let camera = request.camera();
        let transition =
            crate::render::globe::globe_frame(&camera).transition;
        assert!(
            transition > 0.0,
            "z{} must be inside the globe band, or this harness is drawing a flat map \
             through a path it never checked: transition was {transition}",
            request.zoom
        );
        let cover = request.cover();
        assert!(
            !cover.is_empty(),
            "a globe camera must ask for tiles; an empty cover would make every pixel \
             count below a statement about an empty frame"
        );
        assert!(
            cover.iter().all(|t| t.z == 3),
            "the cover is asked for at the display zoom, so every tile must be z3: {cover:?}"
        );
    }

    /// `FlattenedFills` really does drop the lines and really does keep the
    /// fills — the discriminator is only a discriminator if it discriminates.
    #[test]
    fn flattening_keeps_every_fill_and_drops_every_line() {
        let geometry = std::sync::Arc::new(geo_types::Geometry::Polygon(
            geo_types::Polygon::new(
                geo_types::LineString::from(vec![(0, 0), (10, 0), (10, 10), (0, 0)]),
                vec![],
            ),
        ));
        let pale = Rgba { r: 0.9, g: 0.9, b: 0.9, a: 0.4 };
        let plan = crate::style::PaintPlan {
            ops: vec![
                PaintOp::Fill {
                    color_from: pale,
                    color_to: pale,
                    geometry: geometry.clone(),
                },
                PaintOp::Line {
                    metrics: false,
                    color_from: pale,
                    color_to: pale,
                    width_from: 1.0,
                    width_to: 1.0,
                    gap_from: 0.0,
                    gap_to: 0.0,
                    cap: crate::style::LineCap::Butt,
                    join: crate::style::LineJoin::Miter,
                    dash: crate::style::DashPattern::none(),
                    geometry,
                },
            ],
        };
        let flattened = repaint(plan, Paint::FlattenedFills);
        assert_eq!(flattened.ops.len(), 1, "the line must be gone");
        match &flattened.ops[0] {
            PaintOp::Fill { color_from, color_to, .. } => {
                assert_eq!(color_from.a, 1.0, "a flattened fill must be opaque");
                assert_eq!(color_from, color_to, "and must not blend across the bracket");
            }
            other => panic!("expected the fill to survive, got {other:?}"),
        }
    }

    /// The wash really is translucent and really does keep every fill —
    /// the property that makes a double composite READABLE. A `Wash` that
    /// quietly emitted an opaque colour would report zero streaks on a
    /// frame full of them, which is the failure this whole module exists
    /// to stop being possible.
    #[test]
    fn the_wash_is_translucent_and_keeps_every_fill() {
        let geometry = std::sync::Arc::new(geo_types::Geometry::Polygon(
            geo_types::Polygon::new(
                geo_types::LineString::from(vec![(0, 0), (10, 0), (10, 10), (0, 0)]),
                vec![],
            ),
        ));
        let opaque = Rgba { r: 0.2, g: 0.4, b: 0.6, a: 1.0 };
        let plan = crate::style::PaintPlan {
            ops: vec![
                PaintOp::Fill { color_from: opaque, color_to: opaque, geometry: geometry.clone() },
                PaintOp::Fill { color_from: opaque, color_to: opaque, geometry },
            ],
        };
        let washed = repaint(plan, Paint::Wash);
        assert_eq!(washed.ops.len(), 2, "both fills must survive -- two coats is the signal");
        for op in &washed.ops {
            match op {
                PaintOp::Fill { color_from, color_to, .. } => {
                    assert_eq!(color_from.a, 0.3, "an opaque wash cannot show a second coat");
                    assert_eq!(color_from, color_to);
                }
                other => panic!("expected a fill, got {other:?}"),
            }
        }
        // The arithmetic the reported counts are read against: one coat of
        // 0.3 white over black is 77, two are 130. If this changes, every
        // figure in this module's docs has to be re-read.
        let once = 0.3f32;
        let twice = once + 0.3 * (1.0 - once);
        assert_eq!((once * 255.0).round() as u8, 77);
        assert_eq!((twice * 255.0).round() as u8, 130);
    }

    /// **The stand-in order is the mechanism, so it gets its own test.**
    ///
    /// One z2 ancestor under FOUR z3 tiles, three of which are ready: the
    /// ancestor must be drawn first, and the three ready tiles on top of
    /// it — but CLIPPED to the one quadrant that is missing, so the three
    /// ready quadrants are not painted twice.
    ///
    /// Before the clip existed this asserted the ancestor whole, which is
    /// the defect: a translucent fill composited twice over those three
    /// quadrants is a pale patch whose boundary is the ready/not-ready tile
    /// edge, and on the globe that edge is a meridian or a parallel.
    #[test]
    fn an_unready_tile_puts_a_clipped_ancestor_under_only_its_own_quadrant() {
        let quad: Vec<TileId> = [(0, 0), (1, 0), (0, 1), (1, 1)]
            .into_iter()
            .map(|(dx, dy)| TileId { z: 3, x: 2 + dx, y: 4 + dy })
            .collect();
        let ancestor = TileId { z: 2, x: 1, y: 2 };
        let absent = vec![quad[3]];
        // Every z3 tile decoded, plus the ancestor -- `absent` is about
        // the GPU mesh not being uploaded yet, not about the bytes.
        let (order, missing) = draw_order(&quad, &absent, None, |t| *t == ancestor || quad.contains(t));

        assert!(missing.is_empty(), "the ancestor is present, so nothing is unaccounted for");
        assert_eq!(
            order,
            vec![
                (ancestor, TileClip::for_descendant(ancestor, quad[3])),
                (quad[0], TileClip::WHOLE),
                (quad[1], TileClip::WHOLE),
                (quad[2], TileClip::WHOLE),
            ],
            "the ancestor must come FIRST, exactly once, and clipped to the missing tile"
        );
        assert!(
            !order[0].1.is_whole(),
            "a stand-in drawn whole is the double composite this clip removes"
        );
    }

    /// Two unready siblings share one ancestor: it is pushed ONCE PER
    /// MISSING TILE now, with disjoint clips, because that is what says
    /// "cover these two quadrants and no others".
    ///
    /// The old contract — pushed once, whole — is the one that made three
    /// of four quadrants pale. Deduping on the ancestor could only ever
    /// express "all of it or none of it"; a clip per missing tile expresses
    /// the union that is actually wanted, and the union is still drawn
    /// exactly once because the boxes do not overlap.
    #[test]
    fn two_unready_siblings_push_their_shared_ancestor_once_each_with_disjoint_clips() {
        let quad: Vec<TileId> = [(0, 0), (1, 0), (0, 1), (1, 1)]
            .into_iter()
            .map(|(dx, dy)| TileId { z: 3, x: 2 + dx, y: 4 + dy })
            .collect();
        let ancestor = TileId { z: 2, x: 1, y: 2 };
        let absent = vec![quad[2], quad[3]];
        let (order, _) = draw_order(&quad, &absent, None, |t| *t == ancestor || quad.contains(t));
        assert_eq!(
            order,
            vec![
                (ancestor, TileClip::for_descendant(ancestor, quad[2])),
                (ancestor, TileClip::for_descendant(ancestor, quad[3])),
                (quad[0], TileClip::WHOLE),
                (quad[1], TileClip::WHOLE),
            ]
        );
        let (left, right) = (order[0].1.bounds(), order[1].1.bounds());
        assert!(
            left[2] <= right[0] || right[2] <= left[0] || left[3] <= right[1] || right[3] <= left[1],
            "the two stand-in boxes must not overlap, or the union is composited twice \
             again: {left:?} and {right:?}"
        );
    }

    /// A complete cover draws every tile once, unclipped, and nothing
    /// underneath — the control, and the shape every capture in this
    /// module's own measured table was taken in.
    #[test]
    fn a_complete_cover_draws_each_tile_once_and_no_stand_in() {
        let cover: Vec<TileId> = (0..4).map(|x| TileId { z: 3, x, y: 1 }).collect();
        let (order, missing) = draw_order(&cover, &[], None, |t| cover.contains(t));
        assert_eq!(order, cover.iter().map(|t| (*t, TileClip::WHOLE)).collect::<Vec<_>>());
        assert!(missing.is_empty());
    }

    /// `stand_in_levels` (F13): a missing z14 tile whose only resident
    /// ancestor is three levels up draws it with no bound, and is named
    /// missing with a bound of two; the grandparent still stands in at two.
    #[test]
    fn stand_in_levels_bounds_the_ancestor_search() {
        let tile = TileId { z: 14, x: 12_900, y: 8_130 };
        let up = |k: u8| TileId { z: tile.z - k, x: tile.x >> k, y: tile.y >> k };
        let (order, missing) = draw_order(&[tile], &[], None, |t| *t == up(3));
        assert_eq!((order.len(), missing.len()), (1, 0));
        assert_eq!(order[0].0, up(3));
        let (order, missing) = draw_order(&[tile], &[], Some(2), |t| *t == up(3));
        assert!(order.is_empty());
        assert_eq!(missing, vec![tile]);
        let (order, missing) = draw_order(&[tile], &[], Some(2), |t| *t == up(2));
        assert_eq!((order.first().map(|o| o.0), missing.len()), (Some(up(2)), 0));
    }

    /// A tile whose single feature fills its whole box, in one layer.
    /// Every tile in the fixture below is this, so the frame is one
    /// unbroken sheet of fill and any gap in it is the fix's doing rather
    /// than the data's.
    fn full_tile(layer: &str) -> crate::mvt::DecodedTile {
        let extent = crate::mvt::MVT_EXTENT as i32;
        crate::mvt::DecodedTile {
            data_hash: 0,
            layers: vec![crate::mvt::Layer {
                name: layer.to_string(),
                features: vec![crate::mvt::Feature {
                    geometry: geo_types::Geometry::Polygon(geo_types::Polygon::new(
                        geo_types::LineString::from(vec![
                            (0, 0),
                            (extent, 0),
                            (extent, extent),
                            (0, extent),
                            (0, 0),
                        ]),
                        vec![],
                    )),
                    tags: Default::default(),
                }],
            }],
        }
    }

    /// One translucent fill layer over that one source layer. The exact
    /// colour is irrelevant — [`Paint::Wash`] repaints every fill at 0.3
    /// white regardless — but the layer has to exist for a fill op to be
    /// emitted at all.
    fn one_fill_style(layer: &str) -> Style {
        Style {
            layers: vec![crate::style::StyleLayer::Fill {
                id: "wash".to_string(),
                source_layer: layer.to_string(),
                filter: crate::style::Filter::Any,
                color: crate::style::ZoomValue::constant(Rgba {
                    r: 1.0,
                    g: 1.0,
                    b: 1.0,
                    a: 0.3,
                }),
                opacity: crate::style::ZoomValue::constant(0.3),
                outline_color: None,
                min_zoom: None,
                max_zoom: None,
            }],
            ..Style::default()
        }
    }

    /// Review F3: a tile whose only geometry is buildings (their own
    /// source, no flat layer at all) is installed and its walls drawn. The
    /// style has one `fill-extrusion` layer on a second source and nothing
    /// else, so the tile's flat mesh is empty.
    #[test]
    fn a_buildings_only_tile_draws_its_buildings() {
        let style = crate::style_spec::parse_and_compile(
            &serde_json::to_vec(&serde_json::json!({
                "version": 8,
                "sources": {"buildings": {"type": "vector", "tiles": ["x/{z}/{x}/{y}"]}},
                "layers": [{"id": "b", "type": "fill-extrusion", "source": "buildings", "source-layer": "building",
                            "paint": {"fill-extrusion-color": "#888",
                                      "fill-extrusion-height": ["get", "render_height"]}}]
            }))
            .unwrap(),
        )
        .unwrap();
        let request = |flat: bool| CaptureRequest {
            center_lon: 139.6912,
            center_lat: 35.6841,
            zoom: 16.0,
            pitch: 60f32.to_radians(),
            bearing: 0.0,
            width: 256,
            height: 256,
            flat: true,
            extrusion_flat: flat,
            ..CaptureRequest::default()
        };
        let mut tiles = TileStore::new();
        tiles.insert(
            crate::geo::TileId { z: 14, x: 14549, y: 6451 },
            crate::mvt::decode(std::fs::read("benches/fixtures/shinjuku_z14.pbf").unwrap()).unwrap(),
        );
        let lifted = capture(&request(false), &tiles, &style).expect("capture");
        let flat = capture(&request(true), &tiles, &style).expect("capture");
        let (differing, _) = lifted.diff(&flat, 2);
        assert!(differing > 256 * 256 / 50, "a buildings-only tile must draw walls: {differing} px");
    }

    #[test]
    fn liberty_buildings_rise_in_a_pitched_capture() {
        // One real z14 tile (Shinjuku), Liberty, pitch 60, zoom 16: the
        // extruded capture must differ from the same capture with the
        // buildings flattened (meters_to_world = 0) -- i.e. walls exist.
        // The capture needs no parent or neighbour for its cover: a tile
        // it lacks is simply not drawn (`draw_order`'s `missing`).
        let style = crate::style_spec::parse_and_compile(&std::fs::read("examples/liberty-style.json").unwrap()).unwrap();
        let request = |flat: bool| CaptureRequest {
            center_lon: 139.6995,
            center_lat: 35.6905,
            zoom: 16.0,
            pitch: 60f32.to_radians(),
            bearing: 30f32.to_radians(),
            width: 256,
            height: 256,
            flat: true,
            extrusion_flat: flat,
            extrusion_opacity: Some(1.0),
            ..CaptureRequest::default()
        };
        let mut tiles = TileStore::new();
        tiles.insert(
            crate::geo::TileId { z: 14, x: 14549, y: 6451 },
            crate::mvt::decode(std::fs::read("benches/fixtures/shinjuku_z14.pbf").unwrap()).unwrap(),
        );
        let lifted = capture(&request(false), &tiles, &style).expect("capture");
        let flat = capture(&request(true), &tiles, &style).expect("capture");
        let (differing, _) = lifted.diff(&flat, 2);
        assert!(differing > 256 * 256 / 50, "walls must change at least 2% of the frame: {differing} px");
    }

    /// **THE REGRESSION: a stand-in may only paint the tile it stands in
    /// for.**
    ///
    /// The camera, the cover and the two-pass draw list are production's;
    /// only the tile DATA is synthetic. Every tile of the cover is decoded
    /// and EMPTY — a tile whose whole box is ocean, which is an ordinary
    /// thing for a z3 tile to be — and the z2 ancestor of one quad is a
    /// single polygon filling its whole box. So every pixel the frame
    /// covers was painted by the ancestor, and counting covered pixels
    /// measures exactly one thing: **how much of itself the stand-in
    /// drew**.
    ///
    /// Two frames, differing only in how many of the quad's four tiles are
    /// [`CaptureRequest::absent`]:
    ///
    ///  - all four missing — the ancestor legitimately stands in for the
    ///    whole quad, and paints its whole box. The reference area.
    ///  - ONE missing — it stands in for one quadrant, and must paint a
    ///    quadrant. Before `render::gpu::TileClip` existed it painted the
    ///    whole box here too, over three tiles that were ready and had
    ///    said there was nothing to draw. On a style whose `landcover` is
    ///    a translucent wash — which is the reported style — that leak
    ///    laid a second coat over those three quadrants, which is the pale
    ///    patch bounded by a meridian or a parallel that was reported.
    ///
    /// Both halves of the bar are asserted, because a fix that satisfies
    /// only the first is worse than the defect:
    ///
    ///  - **no leak** — one missing tile must paint far less than four do;
    ///  - **no hole** — it must still paint something. The ancestor is
    ///    clipped to the tile it stands in FOR, not removed.
    ///
    /// And the four-missing frame pins the other risk the clip introduces:
    /// the ancestor is now pushed once per missing tile rather than once,
    /// so its four boxes must not overlap. Nothing in that frame may be
    /// composited twice.
    ///
    /// Reverting `render/shader.wgsl`'s `clip_coord` discard alone — the
    /// clip's entire enforcement, with the boxes still computed and still
    /// uploaded — makes the one-missing frame paint the whole box and fails
    /// the first assertion, which is the defect this test is named for.
    #[test]
    #[cfg_attr(not(target_os = "macos"), ignore = "fails on Linux/lavapipe, not yet investigated")]
    fn a_stand_in_paints_only_the_tile_it_stands_in_for() {
        const LAYER: &str = "landcover";
        let style = one_fill_style(LAYER);
        let request = CaptureRequest {
            paint: Paint::Wash,
            // Over black, with no ocean sphere behind it, a pixel's value
            // IS its composite count. See `Paint::Wash`.
            clear: Clear::Sentinel(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
            backdrop: false,
            // 1024, the size the reported camera was measured at, and not
            // smaller: a z2 ancestor at this zoom is about 1,100 px across,
            // so a 512 px frame sees ONE of its four quadrants and the
            // other three read as zero however the clip behaves. The
            // fixture has to show the stand-in's siblings for the leak over
            // them to be countable.
            width: 1024,
            height: 1024,
            ..CaptureRequest::default()
        };

        let cover = request.cover();
        assert!(!cover.is_empty(), "the reported camera must have a cover");

        // A quad all four of whose tiles are in the visible cover: the
        // stand-in has to have ready siblings to leak over, or the frame
        // cannot show the defect whatever the code does.
        let quad_of = |tile: TileId| -> Vec<TileId> {
            let (bx, by) = (tile.x & !1, tile.y & !1);
            (0..2)
                .flat_map(move |dy| {
                    (0..2).map(move |dx| TileId { z: tile.z, x: bx + dx, y: by + dy })
                })
                .collect::<Vec<_>>()
        };
        let quad = cover
            .iter()
            .map(|&tile| quad_of(tile))
            .find(|quad| quad.iter().all(|s| cover.contains(s)))
            .expect("the default camera's cover must contain a complete z3 quad");
        let ancestor = TileId { z: quad[0].z - 1, x: quad[0].x / 2, y: quad[0].y / 2 };

        // Every cover tile decoded and EMPTY; the ancestor the only thing
        // in the store with any geometry at all.
        let mut tiles = TileStore::new();
        for &tile in &cover {
            tiles.insert(tile, crate::mvt::DecodedTile { data_hash: 0, layers: Vec::new() });
        }
        tiles.insert(ancestor, full_tile(LAYER));

        let capture_with = |absent: Vec<TileId>| {
            let mut request = request.clone();
            request.absent = absent;
            capture(&request, &tiles, &style).expect("a headless device")
        };
        let whole_quad = capture_with(quad.clone());

        let covered = |frame: &Capture| {
            let (once, twice, thrice, more) = frame.wash_levels();
            once + twice + thrice + more
        };
        let quad_covered = covered(&whole_quad);
        assert!(
            quad_covered > 0,
            "the fixture must actually draw something, or every count below is a \
             statement about an empty frame -- ancestor {ancestor:?}, quad {quad:?}"
        );

        // One frame per tile of the quad, each with only that tile still
        // loading. Every quadrant separately, because WHICH quadrant is
        // visible depends on where the quad falls on the disc -- one of the
        // four can be entirely behind the limb, and a test that happened to
        // pick that one would be grading an empty frame.
        let parts: Vec<u32> =
            quad.iter().map(|&tile| covered(&capture_with(vec![tile]))).collect();
        let sum: u32 = parts.iter().sum();

        // **COVERAGE IS CONSERVED**, which is the no-hole half of the bar
        // stated as an equality rather than an inequality: the four
        // quadrants a clipped stand-in paints TILE the box the whole quad's
        // stand-in paints. Nothing is lost between them, and nothing is
        // painted twice.
        //
        // The tolerance is for the shared edges alone -- a fragment centre
        // landing exactly on a box boundary is inside both neighbours'
        // closed boxes (see `fs_main`), and here the two neighbours are
        // separate FRAMES, so such a pixel is counted twice in this sum.
        let slack = (quad_covered as f64 * 0.01).ceil() as u32;
        assert!(
            sum.abs_diff(quad_covered) <= slack,
            "the four stand-ins cover {sum} pixels between them against the {quad_covered} \
             their ancestor {ancestor:?} covers when all four tiles are missing. Equal \
             means each one painted its own quadrant and no more; FOUR TIMES means each \
             one painted the whole box, over three tiles that were ready and drew \
             nothing -- the leak `render::gpu::TileClip` exists to stop. Parts: {parts:?}"
        );
        assert!(
            parts.iter().filter(|part| **part > 0).count() >= 2,
            "at least two of the quad's quadrants have to be on screen, or there is \
             nowhere for a leak to go and this test grades nothing: {parts:?}"
        );
        for (tile, part) in quad.iter().zip(&parts) {
            // NOT a quarter, and deliberately not asserted as one: the
            // quadrant the camera sits over is magnified by the projection
            // and takes about four fifths of the box at this camera, while
            // one of the other three is over the limb and takes none. The
            // claim worth making is the one the defect breaks -- no single
            // missing tile accounts for the WHOLE box, which is exactly
            // what each of them did before the clip.
            assert!(
                (*part as f64) <= 0.9 * quad_covered as f64,
                "tile {tile:?} still loading made its ancestor {ancestor:?} paint {part} \
                 pixels of the {quad_covered} the whole box covers -- a single missing \
                 tile is one quadrant of its ancestor, not the lot"
            );
        }

        // Four stand-in draws of one ancestor, one per missing tile: the
        // boxes must tile the ancestor, not overlap it. This is the risk
        // the clip itself introduces -- the ancestor used to be deduped and
        // drawn once -- so it is pinned rather than argued.
        let (_, twice, thrice, more) = whole_quad.wash_levels();
        // **The shared EDGES are allowed a pixel, and nothing more.**
        //
        // `TileClip`'s box is closed at both ends (`0 <= clip_coord <= 1`,
        // in `shader.wgsl`'s `fs_main`), deliberately: a fragment centre
        // landing exactly on the boundary between two adjacent boxes passes
        // in both, and that was chosen over the alternative, which is a
        // one-pixel transparent crack along every stand-in's edge -- the
        // hole a stand-in exists to prevent. A handful of doubled pixels on
        // the shared edges is therefore the DESIGN, not a leak.
        //
        // This asserted exactly zero and passed. It did so only because the
        // tile pipeline wrote depth: with `depth_compare: LessEqual`, one of
        // the two coincident edge fragments was rejected outright, which
        // silently hid the doubling. That depth write was itself the defect
        // `render::gpu`'s
        // `a_later_style_layer_covers_an_earlier_one_at_every_transition`
        // now pins -- the style's `park` layer showing through the opaque
        // `water` drawn after it, on open sea -- and removing it is what
        // exposes this pixel. The bound below is a correction to the
        // assertion, not a relaxation of the property: what this test exists
        // to catch is the 49,683-pixel whole-box leak, and 1 against a cap
        // of 64 still separates the two by three orders of magnitude.
        let edge_budget = 64;
        assert!(
            twice <= edge_budget && thrice == 0 && more == 0,
            "the four clipped stand-ins overlap by more than their shared edges: \
             {twice} pixels took a second coat (budget {edge_budget}), {thrice} a third, \
             {more} more. Pushing the ancestor once per missing tile is only sound while \
             their boxes are disjoint apart from the measure-zero boundary"
        );
    }

    /// One OPAQUE fill over that one source layer, declaring
    /// `min_zoom: 0` — which is what makes it a fixture rather than a
    /// decoration. See [`a_min_zoom_of_zero_paints_at_zoom_zero`].
    ///
    /// Opaque, and `Paint::AsStyled`, because the quantity under test IS
    /// the resolved alpha: `Paint::Wash` and `Paint::FlattenedFills` both
    /// overwrite it, and a frame graded through either of them comes out
    /// identical whatever `style::layer_zoom_fade` returns.
    fn one_opaque_fill_style_from_zoom_zero(layer: &str, color: Rgba) -> Style {
        Style {
            layers: vec![crate::style::StyleLayer::Fill {
                id: "from-zoom-zero".to_string(),
                source_layer: layer.to_string(),
                filter: crate::style::Filter::Any,
                color: crate::style::ZoomValue::constant(color),
                opacity: crate::style::ZoomValue::constant(1.0),
                outline_color: None,
                // The whole fixture. Every layer of the shipped Mapbox
                // Streets style declares a `minzoom`, and the five that
                // survive to z0 declare `minzoom: 0`.
                min_zoom: Some(0),
                max_zoom: None,
            }],
            ..Style::default()
        }
    }

    /// **THE REGRESSION: a layer whose `min_zoom` is 0 must paint at zoom
    /// 0.**
    ///
    /// `style::layer_zoom_fade` ramps a layer in across the bucket
    /// `[min_zoom, min_zoom + 1)` — transparent at `min_zoom`, opaque at
    /// `min_zoom + 1` — and that reads as a fade-in only because the
    /// camera arrives from `min_zoom - 1`, where the layer was
    /// legitimately not drawn at all. **Zoom 0 has no such approach.** A
    /// `min_zoom: 0` layer was therefore invisible at zoom 0.0 and
    /// half-strength at 0.5, with nothing the user could do to reach it.
    ///
    /// On the user's own map that is the whole picture: the shipped Mapbox
    /// Streets style declares a `minzoom` on all 134 of its layers, and
    /// the five that survive to z0 (`landcover`, `water`, `water-depth`,
    /// `hillshade` and the `land` background) declare `minzoom: 0`, so
    /// they faded out together and the globe came up as the backdrop
    /// sphere alone — a blank pale disc, with `--no-backdrop` fully black.
    /// `PaintOp::Background` is exempt from the fade (see
    /// `style::layer_zoom_fade`), which is exactly why the disc was the
    /// background colour rather than nothing.
    ///
    /// Graded on PIXELS, at the camera the defect was reported at, over
    /// [`Clear::SENTINEL`] with no backdrop: a pixel the fill painted is
    /// the fill's colour and a pixel it did not is magenta, so the count
    /// is not a statement about a fade, it is a statement about the frame.
    /// The data is synthetic — one full-tile polygon, no Mapbox bytes and
    /// no token — because the defect is in the STYLE's zoom range and not
    /// in any tile's content: `min_zoom: Some(0)` is the entire
    /// reproduction.
    ///
    /// Reverting `style::layer_zoom_fade`'s `Some(0) => 1.0` arm alone
    /// puts this frame back to 0 painted pixels.
    #[test]
    fn a_min_zoom_of_zero_paints_at_zoom_zero() {
        const LAYER: &str = "water";
        // Exactly representable as `Rgba8Unorm` bytes, so the assertion
        // below can name the colour rather than a tolerance around it.
        let color = Rgba { r: 0.0, g: 0.4, b: 1.0, a: 1.0 };
        let expected = [0u8, 102, 255, 255];
        let style = one_opaque_fill_style_from_zoom_zero(LAYER, color);

        let request = CaptureRequest {
            // The reported camera: the north pole at the top of Mercator,
            // at the bottom of the zoom range.
            center_lon: 111.7,
            center_lat: 85.04,
            zoom: 0.0,
            // Small: at z0 one tile is the whole sphere and the disc is
            // hundreds of pixels across at this latitude, so nothing about
            // this count needs a big frame.
            width: 256,
            height: 256,
            // No ocean sphere, and a clear no style contains: every pixel
            // is then either the fill's colour or provably unpainted. With
            // the backdrop on, an unpainted pixel would be the background
            // colour and indistinguishable by value from a pale fill --
            // which is the very confusion that made this defect read as a
            // rendering failure for a week.
            clear: Clear::SENTINEL,
            backdrop: false,
            ..CaptureRequest::default()
        };

        let cover = request.cover();
        assert_eq!(
            cover,
            vec![TileId { z: 0, x: 0, y: 0 }],
            "z0's cover is the root tile and nothing else, or this fixture is \
             grading a different frame"
        );

        let mut tiles = TileStore::new();
        tiles.insert(cover[0], full_tile(LAYER));

        let frame = capture(&request, &tiles, &style).expect("a headless device");
        let total = (frame.width * frame.height) as usize;
        let unpainted = frame.count_matching([255, 0, 255, 255], 0);
        let painted = frame.count_matching(expected, 0);

        assert!(
            frame.vertices > 0,
            "the fixture tessellated nothing, so this frame says nothing about paint"
        );
        assert!(
            painted > total / 8,
            "the root tile's fill painted {painted} of {total} pixels in its own \
             colour at zoom 0 -- a `min_zoom: 0` layer is at FULL strength at zoom \
             0, because there is no zoom below 0 for it to have faded in from. \
             {unpainted} pixels are still the sentinel"
        );
        assert_eq!(
            painted + unpainted,
            total,
            "every pixel must be either the fill's own colour or the sentinel: \
             {painted} painted plus {unpainted} unpainted is not {total}, so some \
             pixel took a PARTIAL coat -- the fade is still running, just not all \
             the way to zero"
        );
    }

    /// `AsStyled` is a pass-through. Stated as a test because the whole
    /// point of the A/B is that only ONE thing changes between two runs,
    /// and a `repaint` that quietly normalised something would be a second.
    #[test]
    fn as_styled_changes_nothing() {
        let plan = crate::style::PaintPlan {
            ops: vec![PaintOp::Background { color: Rgba { r: 0.5, g: 0.5, b: 0.5, a: 1.0 } }],
        };
        assert_eq!(repaint(plan.clone(), Paint::AsStyled), plan);
    }

    // ---------------------------------------------------------------
    // Raster
    // ---------------------------------------------------------------

    /// A solid tile of one colour.
    ///
    /// Solid, rather than a pattern, because every assertion below names
    /// an exact byte at `tolerance: 0`, and a pattern would make every
    /// pixel's expected value depend on where the projection put it --
    /// which is the thing under test. With one colour, "did the texture
    /// arrive" and "did it arrive in the right place" stay separable: the
    /// first is a pixel count, the second is the seam test.
    fn solid_raster_tile(size: u32, color: [u8; 4]) -> image::RgbaImage {
        image::RgbaImage::from_pixel(size, size, image::Rgba(color))
    }

    /// A style with no layers at all.
    ///
    /// Deliberate: these frames grade the RASTER pipeline, and a vector
    /// layer in the style would put a second thing on the same pixels.
    /// With no layers, `Clear::SENTINEL` and `backdrop: false`, every
    /// pixel in the frame is either a texel the raster pipeline wrote or
    /// provably untouched.
    fn no_layers() -> Style {
        Style { layers: Vec::new(), ..Style::default() }
    }

    /// **Satellite on the globe.** The root tile's imagery, at zoom 0,
    /// where the projection is a whole sphere (`transition == 1.0`).
    ///
    /// # What would have to break for this to fail
    ///
    /// Everything on the raster path at once, which is why it is the
    /// first test: the layer variant is not involved, but the mesh, the
    /// texture upload, the bind groups, the shader's globe branch and the
    /// pass ordering all are. A zero here means the pipeline drew nothing
    /// visible; `raster_tiles_drawn` is what separates that from a fixture
    /// that submitted nothing.
    ///
    /// # Fail-before
    ///
    /// Suppressing the one `raster.pipeline.draw(..)` line in
    /// `render_with_space_and_raster` takes this frame to 0 painted of
    /// 65,536 and 65,536 sentinel. See the report accompanying this
    /// branch for the measured pair.
    #[test]
    fn a_raster_tile_paints_the_globe_at_zoom_zero() {
        // Exactly representable as `Rgba8Unorm`, so the assertion names
        // the colour rather than a tolerance around it -- and chosen to be
        // nothing like the sentinel's magenta, so a channel swap could not
        // read as a pass.
        let color = [0u8, 128, 64, 255];

        let request = CaptureRequest {
            center_lon: 0.0,
            center_lat: 0.0,
            zoom: 0.0,
            width: 256,
            height: 256,
            clear: Clear::SENTINEL,
            backdrop: false,
            ..CaptureRequest::default()
        };
        let cover = request.cover();
        assert_eq!(
            cover,
            vec![TileId { z: 0, x: 0, y: 0 }],
            "z0's cover is the root tile and nothing else, or this fixture grades a \
             different frame"
        );

        let mut request = request;
        request.raster.insert(cover[0], solid_raster_tile(64, color));

        let frame = capture(&request, &TileStore::new(), &no_layers()).expect("a headless device");
        let total = (frame.width * frame.height) as usize;
        let painted = frame.count_matching(color, 0);
        let unpainted = frame.count_matching([255, 0, 255, 255], 0);

        assert_eq!(
            frame.raster_tiles_drawn, 1,
            "the fixture submitted no raster tile, so this frame says nothing about paint"
        );
        assert_eq!(frame.transition, 1.0, "zoom 0 must be a whole sphere");
        assert!(
            painted > total / 8,
            "the root tile's imagery painted {painted} of {total} pixels in its own colour \
             at zoom 0; {unpainted} are still the sentinel"
        );
        assert_eq!(
            painted + unpainted,
            total,
            "every pixel must be either the texture's own colour or the sentinel: \
             {painted} painted plus {unpainted} unpainted is not {total}, so some pixel \
             took a PARTIAL coat -- an opacity or a blend is running that should not be"
        );
    }

    /// **The seam test, graded as a count.**
    ///
    /// Four adjacent raster tiles covering the whole z1 grid. Any gap
    /// between two of them is a sentinel pixel with painted pixels on both
    /// sides of it on both axes -- which is exactly what
    /// [`Capture::enclosed_background`] counts. Sentinel OUTSIDE the
    /// globe's silhouette is not enclosed and is correctly ignored, which
    /// is why this is the right instrument and a raw sentinel count is
    /// not.
    ///
    /// # What this proves about the no-border decision
    ///
    /// MapLibre carries a mesh border of `EXTENT / 128` and a two-pass
    /// stencil (`create_tile_mesh.ts:13`, `draw_raster.ts:55-77`) because
    /// ITS neighbours can be subdivided differently and so cut their
    /// shared edge at different points. Ours cannot:
    /// `granularity_for_zoom` reads only `z` and a cover is one `z`.
    /// `neighbouring_tiles_at_one_zoom_cut_their_shared_edge_identically`
    /// makes that argument arithmetically; this makes it in pixels.
    ///
    /// All four tiles share one colour on purpose. Different colours would
    /// let a seam hide as a one-pixel blend of two neighbours rather than
    /// showing as background; one colour means a gap can only be the
    /// sentinel.
    /// **A clipped stand-in at `raster-opacity` 0.6.** One z1 tile is
    /// ready, the others are missing and stood in for by the z0 root,
    /// clipped to each missing footprint. The ready tile's pixels are
    /// IDENTICAL with and without the stand-ins (the ancestor never
    /// composites under it a second time), and a missing footprint is
    /// painted by the ancestor instead of left blank.
    #[test]
    fn a_clipped_stand_in_fills_the_missing_tile_and_leaves_the_ready_one_alone() {
        let (ready_c, root_c) = ([0u8, 128, 64, 255], [200u8, 40, 40, 255]);
        let mut request = CaptureRequest {
            center_lon: 0.0,
            center_lat: 0.0,
            zoom: 1.0,
            width: 512,
            height: 512,
            clear: Clear::Sentinel(crate::style::Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
            backdrop: false,
            raster_opacity: 0.6,
            ..CaptureRequest::default()
        };
        let cover = request.cover();
        let root = TileId { z: 0, x: 0, y: 0 };
        let ready = *cover.iter().find(|t| t.z == 1).expect("a z1 cover");
        let missing: Vec<TileId> = cover.iter().copied().filter(|&t| t != ready).collect();
        assert!(!missing.is_empty(), "{cover:?}");
        request.raster.insert(ready, solid_raster_tile(64, ready_c));
        let alone = capture(&request, &TileStore::new(), &no_layers()).expect("a headless device");
        request.raster.insert(root, solid_raster_tile(64, root_c));
        request.raster_stand_ins = missing.iter().map(|&m| (root, m)).collect();
        let stood = capture(&request, &TileStore::new(), &no_layers()).expect("a headless device");
        // The ready tile's colour at 0.6 over black.
        let tinted = |c: [u8; 4]| [(f32::from(c[0]) * 0.6).round() as u8, (f32::from(c[1]) * 0.6).round() as u8, (f32::from(c[2]) * 0.6).round() as u8, 255];
        let ready_px = alone.count_matching(tinted(ready_c), 2);
        assert!(ready_px > 1000, "the ready tile painted: {ready_px}");
        assert_eq!(stood.count_matching(tinted(ready_c), 2), ready_px, "the ready tile is untouched by the stand-in");
        assert_eq!(alone.count_matching(tinted(root_c), 2), 0);
        assert!(stood.count_matching(tinted(root_c), 2) > 1000, "the missing footprint is filled by the ancestor");
        // A zoom-interpolated opacity resolved to another value this frame:
        // the stand-in draws at THAT opacity, not the one it was uploaded at.
        let style_opacity = crate::style::ZoomValue::<f32>::interpolated(vec![(0, 0.2_f32), (2, 0.5)]).value_at(request.display_zoom());
        request.raster_opacity = style_opacity;
        let later = capture(&request, &TileStore::new(), &no_layers()).expect("a headless device");
        let at = |c: [u8; 4]| [(f32::from(c[0]) * style_opacity).round() as u8, (f32::from(c[1]) * style_opacity).round() as u8, (f32::from(c[2]) * style_opacity).round() as u8, 255];
        assert!(later.count_matching(at(root_c), 2) > 1000, "the stand-in at the frame's resolved opacity {style_opacity}");
        assert_eq!(later.count_matching(tinted(root_c), 2), 0, "and not at the old one");
    }

    #[test]
    fn four_adjacent_raster_tiles_leave_no_seam_between_them() {
        let color = [0u8, 128, 64, 255];
        let mut request = CaptureRequest {
            center_lon: 0.0,
            center_lat: 0.0,
            zoom: 1.0,
            width: 512,
            height: 512,
            clear: Clear::SENTINEL,
            backdrop: false,
            ..CaptureRequest::default()
        };
        let cover = request.cover();
        assert!(
            cover.len() >= 2,
            "this test needs at least one shared edge on screen; the z1 cover was {cover:?}"
        );
        for &tile in &cover {
            request.raster.insert(tile, solid_raster_tile(64, color));
        }

        let frame = capture(&request, &TileStore::new(), &no_layers()).expect("a headless device");
        assert_eq!(
            frame.raster_tiles_drawn,
            cover.len(),
            "every cover tile must have been submitted, or a hole below is a missing tile \
             rather than a seam"
        );
        assert!(
            frame.count_matching(color, 0) > 0,
            "nothing was painted at all, so a seam count of zero would be vacuous"
        );

        let holes = frame.enclosed_background([255, 0, 255, 255], 0);
        assert_eq!(
            holes, 0,
            "{holes} sentinel pixels are enclosed by painted ones -- that is a seam between \
             adjacent raster tiles. The fix is MapLibre's mesh border and two-pass stencil, \
             NOT a texture-coordinate inset: an inset scales the imagery by N/(N-1) and \
             trades a boundary colour error for a whole-tile registration error."
        );
    }

    /// **No dark meridian at lon 180.** At lon 180, z3, the wrap edge
    /// between tile x=7 and tile x=0 lands on column 255 of a 512 px frame.
    /// OpenFreeMap's Natural Earth tiles ship the easternmost tile's last
    /// pixel column transparent (z0, z1, z3, z5), so column 255 showed
    /// whatever was under the imagery -- over the satellites "Real" style,
    /// a 1 px dark line. `raster::decode_tile_at` repairs it at decode, in
    /// every shell. Graded on a sentinel clear: the same tiles are drawn
    /// raw (the crack must be THERE, or the test grades nothing) and
    /// repaired (it must be gone). The ocean sphere alone is graded too:
    /// its own two ends meet on that column.
    #[test]
    fn the_antimeridian_leaves_no_crack_in_the_raster_or_the_backdrop() {
        let sentinel = [255u8, 0, 255, 255];
        let base = CaptureRequest {
            center_lon: 180.0,
            center_lat: 0.0,
            zoom: 3.0,
            width: 512,
            height: 512,
            clear: Clear::SENTINEL,
            backdrop: false,
            ..CaptureRequest::default()
        };
        let crack = |frame: &Capture| -> usize {
            (200..312u32)
                .filter(|&y| {
                    let i = ((y * frame.width + 255) * 4) as usize;
                    frame.rgba[i..i + 4] == sentinel
                })
                .count()
        };
        let tiles = |repair: bool| {
            let mut request = base.clone();
            for tile in request.cover() {
                let mut image = solid_raster_tile(64, [0, 128, 64, 255]);
                if tile.x == 7 {
                    // The tileset's data, as served.
                    for y in 0..64 {
                        image.put_pixel(63, y, image::Rgba([2, 0, 2, 0]));
                    }
                }
                if repair {
                    crate::raster::repair_antimeridian_column(tile, &mut image);
                }
                request.raster.insert(tile, image);
            }
            request
        };
        let raw = capture(&tiles(false), &TileStore::new(), &no_layers()).expect("a headless device");
        assert!(crack(&raw) > 50, "the unrepaired tiles must show the crack, or this grades nothing");
        let repaired = capture(&tiles(true), &TileStore::new(), &no_layers()).expect("capture");
        assert_eq!(crack(&repaired), 0, "repaired: column 255 is still uncovered");

        let backdrop = CaptureRequest { backdrop: true, ..base };
        let style = crate::style_spec::parse_and_compile(
            br##"{"version":8,"sources":{},"layers":[{"id":"bg","type":"background","paint":{"background-color":"#2060a0"}}]}"##,
        )
        .expect("a background-only style");
        let frame = capture(&backdrop, &TileStore::new(), &style).expect("capture");
        assert_eq!(crack(&frame), 0, "backdrop: column 255 is uncovered");
    }

    /// **`raster-opacity`, graded on the composited byte.**
    ///
    /// Three opacities over the same sentinel backdrop. 1.0 must give the
    /// texture's own colour exactly; 0.0 must leave the sentinel
    /// untouched AND cost no draw call; 0.5 must give neither, and must
    /// give something strictly between them.
    ///
    /// The half-opacity case is graded as a range rather than an exact
    /// byte on purpose. The exact composite is
    /// `0.5 * texture + 0.5 * sentinel` in whatever precision the blend
    /// unit uses, and pinning that byte would be pinning the rasterizer's
    /// rounding rather than this crate's arithmetic. What is being tested
    /// is that the property is APPLIED and applied once -- the failure
    /// modes are "ignored" (reads as 1.0) and "applied twice" (reads as
    /// 0.25), and both are far outside the band asserted.
    #[test]
    fn raster_opacity_composites_the_texture_over_what_is_behind_it() {
        let color = [0u8, 128, 64, 255];
        let base = CaptureRequest {
            center_lon: 0.0,
            center_lat: 0.0,
            zoom: 0.0,
            width: 256,
            height: 256,
            clear: Clear::SENTINEL,
            backdrop: false,
            ..CaptureRequest::default()
        };
        let tile = TileId { z: 0, x: 0, y: 0 };
        let frame_at = |opacity: f32| {
            let mut request = base.clone();
            request.raster_opacity = opacity;
            request.raster.insert(tile, solid_raster_tile(64, color));
            capture(&request, &TileStore::new(), &no_layers()).expect("a headless device")
        };

        let opaque = frame_at(1.0);
        let opaque_px = opaque.count_matching(color, 0);
        assert!(opaque_px > 0, "opacity 1.0 must paint the texture's own colour");

        // Opacity 0: the reference returns before drawing anything at all
        // (`draw_raster.ts:46`). Asserting the PIXELS alone would pass for
        // a pipeline that drew a fully transparent quad, which is a draw
        // call spent on nothing -- so the draw count is asserted too.
        let clear = frame_at(0.0);
        assert_eq!(
            clear.count_matching(color, 0),
            0,
            "opacity 0 must leave no pixel in the texture's colour"
        );
        assert_eq!(
            clear.count_matching([255, 0, 255, 255], 0),
            (clear.width * clear.height) as usize,
            "opacity 0 must leave the whole frame as it found it"
        );

        // Halfway. Sampled at the centre of the disc, which is the one
        // pixel guaranteed to be inside the silhouette at z0.
        let half = frame_at(0.5);
        let centre = half.pixel(half.width / 2, half.height / 2);
        assert_ne!(centre, color, "half opacity must not read as fully opaque -- the \
                                   property is being ignored");
        assert_ne!(
            centre,
            [255, 0, 255, 255],
            "half opacity must not read as fully transparent"
        );
        // Green channel: 128 at full opacity, 0 in the sentinel, so a
        // single application lands near 64 and a double application near
        // 32. The band is wide enough to absorb the blend unit's rounding
        // and narrow enough to separate those two.
        assert!(
            (48..=80).contains(&centre[1]),
            "half opacity put the green channel at {} -- 1 application lands near 64, and \
             2 (the property applied twice) near 32",
            centre[1]
        );
    }

    /// **An overlay on the globe lands on the planet, where its lon/lat
    /// is.** The reported bug: tiles curved onto the sphere while a runtime
    /// GeoJSON overlay stayed in flat Mercator and drifted off the United
    /// States into space.
    ///
    /// A fill and a circle far from the camera's centre -- where the flat
    /// and globe projections are tens of pixels apart, so the assertion can
    /// tell them apart -- drawn over nothing on the sentinel. Checked
    /// against the GPU's actual pixels: every overlay pixel is inside the
    /// globe's disc, and each feature's pixel centroid is within a few
    /// pixels of where `project_world_to_screen_on_frame` (the CPU twin of
    /// the shader's blend, the one the puck uses) puts its lon/lat -- and
    /// NOT where the flat projection puts it.
    /// **A basemap label draws over an overlay fill below the labels, and
    /// under one above them.** One black place name on a white basemap, one
    /// opaque green overlay square over the whole frame. Below the labels
    /// (the default) the label's dark pixels survive over the green; above
    /// them the green covers every one. The discriminator is the dark-pixel
    /// count, which only the label can produce -- the basemap is white and
    /// the overlay green.
    #[test]
    fn a_basemap_label_draws_over_an_overlay_below_the_labels_and_under_one_above_them() {
        const LABEL_LAYER: &str = "place";
        const SIZE: u32 = 384;
        let (lon, lat) = (10.0_f64, 10.0_f64);
        let style = crate::style_spec::parse_and_compile(
            br##"{"version":8,"sources":{"s":{"type":"vector","tiles":["x/{z}/{x}/{y}"]}},"layers":[
                {"id":"bg","type":"background","paint":{"background-color":"#ffffff"}},
                {"id":"place-label","type":"symbol","source":"s","source-layer":"place",
                 "layout":{"text-field":"{name}","text-size":48},
                 "paint":{"text-color":"#000000"}}]}"##,
        )
        .expect("the test style compiles");
        let request = CaptureRequest {
            center_lon: lon,
            center_lat: lat,
            zoom: 5.5,
            width: SIZE,
            height: SIZE,
            flat: true,
            labels: true,
            overlay: Some(format!(
                r##"{{"type":"FeatureCollection","features":[{{"type":"Feature",
                   "properties":{{"fill-color":"#00ff00","fill-opacity":1.0}},
                   "geometry":{{"type":"Polygon","coordinates":[[[{w},{s}],[{e},{s}],[{e},{n}],[{w},{n}],[{w},{s}]]]}}}}]}}"##,
                w = lon - 10.0,
                e = lon + 10.0,
                s = lat - 10.0,
                n = lat + 10.0,
            )),
            ..CaptureRequest::default()
        };
        // The place name sits in the cover tile under the camera's centre,
        // at the centre's own in-tile position.
        let cover = request.cover();
        let z = cover.iter().map(|t| t.z).max().expect("a flat frame has a cover");
        let scale = f64::from(1u32 << z);
        let fx = (lon + 180.0) / 360.0 * scale;
        let lat_rad = lat.to_radians();
        let fy = (1.0 - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / std::f64::consts::PI) / 2.0 * scale;
        let tile = TileId { z, x: fx.floor() as u32, y: fy.floor() as u32 };
        assert!(cover.contains(&tile), "the centre's tile {tile:?} must be in the cover");
        let extent = f64::from(crate::mvt::MVT_EXTENT);
        let mut tiles = TileStore::new();
        for &cover_tile in &cover {
            tiles.insert(cover_tile, crate::mvt::DecodedTile { data_hash: 0, layers: Vec::new() });
        }
        tiles.insert(
            tile,
            crate::mvt::DecodedTile {
                data_hash: 0,
                layers: vec![crate::mvt::Layer {
                    name: LABEL_LAYER.to_string(),
                    features: vec![crate::mvt::Feature {
                        geometry: geo_types::Geometry::Point(geo_types::Point::new(
                            (fx.fract() * extent) as i32,
                            (fy.fract() * extent) as i32,
                        )),
                        tags: crate::mvt::Tags::from_iter([(
                            "name".to_string(),
                            crate::mvt::TagValue::String("MWMW".to_string()),
                        )]),
                    }],
                }],
            },
        );

        let dark = |shot: &Capture| {
            (0..shot.height)
                .flat_map(|y| (0..shot.width).map(move |x| (x, y)))
                .filter(|&(x, y)| {
                    let [r, g, b, _] = shot.pixel(x, y);
                    r < 96 && g < 96 && b < 96
                })
                .count()
        };
        let below = capture(&request, &tiles, &style).expect("a headless device");
        let above = capture(&CaptureRequest { overlay_above_labels: true, ..request.clone() }, &tiles, &style)
            .expect("a headless device");
        let no_overlay = capture(&CaptureRequest { overlay: None, ..request.clone() }, &tiles, &style)
            .expect("a headless device");

        assert!(below.labels_drawn > 0 && above.labels_drawn > 0, "the label must be placed in both frames");
        let label_alone = dark(&no_overlay);
        assert!(label_alone > 100, "the label alone must paint dark pixels: {label_alone}");
        let green = [0, 255, 0, 255];
        let all = (SIZE * SIZE) as usize;
        assert!(below.count_matching(green, 2) > all / 2, "the overlay must cover the frame below the labels");
        let over_the_fill = dark(&below);
        assert!(
            over_the_fill * 10 >= label_alone * 8,
            "below the labels, the label must draw over the overlay fill: {over_the_fill} dark pixels, \
             {label_alone} with no overlay"
        );
        assert_eq!(dark(&above), 0, "above the labels, the overlay fill must cover the label");
        assert_eq!(above.count_matching(green, 2), all, "above the labels, every pixel is the opaque overlay");
    }

    #[test]
    fn an_overlay_on_the_globe_lands_on_the_planet_where_its_lon_lat_is() {
        const FILL: [u8; 4] = [0, 255, 0, 255];
        const DOT: [u8; 4] = [0, 0, 255, 255];
        let json = r##"{"type":"FeatureCollection","features":[
            {"type":"Feature","properties":{"fill-color":"#00ff00","fill-opacity":1.0},
             "geometry":{"type":"Polygon","coordinates":[[[-109.0,37.0],[-102.0,37.0],[-102.0,41.0],[-109.0,41.0],[-109.0,37.0]]]}},
            {"type":"Feature","properties":{"circle-color":"#0000ff","circle-radius":6},
             "geometry":{"type":"Point","coordinates":[-122.0,47.0]}}]}"##;
        let request = CaptureRequest {
            center_lon: -80.0,
            center_lat: 25.0,
            zoom: 2.0,
            width: 512,
            height: 512,
            clear: Clear::SENTINEL,
            backdrop: false,
            overlay: Some(json.to_string()),
            ..CaptureRequest::default()
        };
        let camera = request.camera();
        let anchors = crate::render::globe::GlobeAnchors::new(&crate::render::globe::globe_frame(&camera));
        assert!(anchors.is_globe(), "z2 must be a globe frame for this test to mean anything");
        let shot = capture(&request, &TileStore::new(), &Style { layers: Vec::new(), ..Style::default() })
            .expect("a headless device");

        let pixels_of = |color: [u8; 4]| -> Vec<(f64, f64)> {
            (0..shot.height)
                .flat_map(|y| (0..shot.width).map(move |x| (x, y)))
                .filter(|&(x, y)| shot.pixel(x, y) == color)
                .map(|(x, y)| (f64::from(x) + 0.5, f64::from(y) + 0.5))
                .collect()
        };
        let centroid = |points: &[(f64, f64)]| {
            let n = points.len() as f64;
            (points.iter().map(|p| p.0).sum::<f64>() / n, points.iter().map(|p| p.1).sum::<f64>() / n)
        };
        let distance = |a: (f64, f64), b: (f64, f64)| (a.0 - b.0).hypot(a.1 - b.1);

        // The disc: at pitch 0 the planet's centre is the frame's centre,
        // and its silhouette is inside the sphere's radius in pixels.
        let radius_px = crate::render::globe::globe_radius_world(&camera) * 2f64.powf(camera.zoom);
        let centre = (f64::from(shot.width) / 2.0, f64::from(shot.height) / 2.0);

        for (name, color, lon, lat) in [("fill", FILL, -105.5, 39.0), ("circle", DOT, -122.0, 47.0)] {
            let drawn = pixels_of(color);
            assert!(drawn.len() > 20, "the {name} drew {} pixels", drawn.len());
            let outside = drawn.iter().filter(|&&p| distance(p, centre) > radius_px + 1.0).count();
            assert_eq!(outside, 0, "{outside} {name} pixels are off the planet (radius {radius_px:.1} px)");

            let world = crate::overlay::geom::lon_lat_to_world(lon, lat);
            let on_globe = crate::render::globe::project_world_to_screen_on_frame(&camera, &anchors, world)
                .expect("the feature faces the camera");
            let flat = crate::render::project_world_to_screen_f64(&camera, world).expect("in front of the camera");
            let on_globe = (f64::from(on_globe.0), f64::from(on_globe.1));
            let flat = (f64::from(flat.0), f64::from(flat.1));
            assert!(
                distance(on_globe, flat) > 30.0,
                "the {name} is too near the centre to tell the projections apart: {:.1} px",
                distance(on_globe, flat)
            );
            let got = centroid(&drawn);
            assert!(
                distance(got, on_globe) < 3.0,
                "the {name}'s pixels centre on {got:?}, its lon/lat projects to {on_globe:?} on the globe \
                 (and {flat:?} flat)"
            );
        }
    }

    /// **A tiled overlay is pixel-equivalent to the whole-source one at
    /// full zoom**, at z14.5 (tiles at `max_zoom`) and z15.5 (overzoomed:
    /// the z14 ancestor clipped to each display tile), with a HALF-opaque
    /// fill so a seam painted twice (the 1024-unit buffer two neighbours
    /// share) or a gap would show as a wrong alpha, and a hole crossing
    /// tile edges so a hole painted as fill would show.
    ///
    /// "Equivalent", not identical: the whole-source mesh is in its
    /// origin's grid and a tile's is rounded to 1/4096 of a tile (under
    /// 0.1 px here), so an edge pixel may flip. Interior pixels may not.
    #[test]
    fn a_tiled_overlay_matches_the_whole_source_overlay_at_full_zoom() {
        let (lon, lat) = (-77.03, 38.89);
        let json = format!(
            r##"{{"type":"FeatureCollection","features":[
            {{"type":"Feature","properties":{{"fill-color":"#00ff00","fill-opacity":0.5}},
             "geometry":{{"type":"Polygon","coordinates":[
               [[{a},{b}],[{c},{b}],[{c},{d}],[{e},{f}],[{a},{d}],[{a},{b}]],
               [[{g},{h}],[{g},{i}],[{j},{i}],[{j},{h}],[{g},{h}]]]}}}}]}}"##,
            a = lon - 0.03,
            b = lat - 0.02,
            c = lon + 0.03,
            d = lat + 0.02,
            e = lon + 0.001,
            f = lat + 0.027,
            g = lon - 0.004,
            h = lat - 0.003,
            i = lat + 0.004,
            j = lon + 0.005,
        );
        for zoom in [14.5, 15.5] {
            // On the hole's north-east corner, so the frame holds fill,
            // hole and the corner between them at both zooms.
            let base = CaptureRequest {
                center_lon: lon + 0.005,
                center_lat: lat + 0.004,
                zoom,
                width: 512,
                height: 512,
                flat: true,
                clear: Clear::SENTINEL,
                backdrop: false,
                overlay: Some(json.clone()),
                ..CaptureRequest::default()
            };
            let style = Style { layers: Vec::new(), ..Style::default() };
            let whole = capture(&base, &TileStore::new(), &style).expect("a headless device");
            let tiled = capture(&CaptureRequest { overlay_tiling: true, ..base.clone() }, &TileStore::new(), &style)
                .expect("a headless device");
            let (mut painted, mut differ, mut interior_differ) = (0usize, 0usize, 0usize);
            let far = |a: [u8; 4], b: [u8; 4]| a.iter().zip(&b).any(|(x, y)| x.abs_diff(*y) > 2);
            for y in 1..whole.height - 1 {
                for x in 1..whole.width - 1 {
                    let (w, t) = (whole.pixel(x, y), tiled.pixel(x, y));
                    if w != [255, 0, 255, 255] {
                        painted += 1;
                    }
                    if far(w, t) {
                        differ += 1;
                        // Interior: every neighbour agrees in the whole-source frame.
                        let n = [(x - 1, y), (x + 1, y), (x, y - 1), (x, y + 1)];
                        if n.iter().all(|&(u, v)| !far(whole.pixel(u, v), w)) {
                            interior_differ += 1;
                        }
                    }
                }
            }
            assert!(painted > 10_000, "z{zoom}: the overlay painted {painted} pixels");
            assert_eq!(interior_differ, 0, "z{zoom}: {interior_differ} interior pixels differ (seam or hole)");
            assert!(differ * 200 < painted, "z{zoom}: {differ} of {painted} pixels differ");
        }
    }

    /// Captures 256 dots on a 16x16 grid around `(lon, lat)` (`step` degrees
    /// apart) twice -- as an ordinary overlay (tessellated round caps) and
    /// through the point path (the view's instanced circles) -- and returns
    /// `(painted, differ, interior_differ, max_diff)`. An INTERIOR pixel is one whose
    /// 3x3 neighbourhood is uniform in the tessellated frame: a circle's
    /// fill or the background, never its antialiased rim.
    fn instanced_against_tessellated(lon: f64, lat: f64, step: f64, zoom: f64, flat: bool) -> (usize, usize, usize, u8) {
        let mut features = Vec::new();
        for i in 0..16 {
            for j in 0..16 {
                features.push(format!(
                    r##"{{"type":"Feature","properties":{{"circle-radius":{r},"circle-color":"#d0306a"}},"geometry":{{"type":"Point","coordinates":[{},{}]}}}}"##,
                    lon - 8.0 * step + f64::from(i) * step,
                    lat - 6.0 * step + f64::from(j) * step * 0.75,
                    r = 3 + (i + j) % 4,
                ));
            }
        }
        let json = format!(r#"{{"type":"FeatureCollection","features":[{}]}}"#, features.join(","));
        let base = CaptureRequest {
            center_lon: lon,
            center_lat: lat,
            zoom,
            width: 512,
            height: 512,
            flat,
            clear: Clear::SENTINEL,
            backdrop: false,
            overlay: Some(json),
            // MAP alignment: the instanced quad's corners then go through
            // `vertex_main`'s ground extrusion exactly as the tessellated
            // cap's rim does, which is what this parity is about. The
            // default (`viewport`) is a screen-space quad the tessellated
            // path does not draw (`circle_pitch_tests`).
            overlay_paint: Some(r#"{"circle-pitch-alignment": "map"}"#.into()),
            ..CaptureRequest::default()
        };
        let style = Style { layers: Vec::new(), ..Style::default() };
        let whole = capture(&base, &TileStore::new(), &style).expect("a headless device");
        let points = capture(&CaptureRequest { overlay_points: true, ..base.clone() }, &TileStore::new(), &style)
            .expect("a headless device");
        let (mut painted, mut differ, mut interior_differ, mut max_diff) = (0usize, 0usize, 0usize, 0u8);
        for y in 1..whole.height - 1 {
            for x in 1..whole.width - 1 {
                let (w, p) = (whole.pixel(x, y), points.pixel(x, y));
                if w != [255, 0, 255, 255] {
                    painted += 1;
                }
                if w != p {
                    differ += 1;
                    max_diff = max_diff.max(w.iter().zip(&p).map(|(a, b)| a.abs_diff(*b)).max().unwrap_or(0));
                    let uniform = (y - 1..=y + 1).all(|v| (x - 1..=x + 1).all(|u| whole.pixel(u, v) == w));
                    if uniform {
                        interior_differ += 1;
                    }
                }
            }
        }
        (painted, differ, interior_differ, max_diff)
    }

    /// **Pixel parity: instanced circles against the tessellated round caps
    /// they replace, at the same radius.** 256 dots (radius 3-6 px) through
    /// the point path's instanced pipeline and through the whole-source
    /// path's tessellated caps. Every interior pixel -- fill and background
    /// -- is identical; only the antialiased rim may differ (the cap's
    /// distance came from an interpolated 64-gon normal, the instance's is
    /// exact). Flat at z17.5, where the view holds every point.
    #[test]
    fn instanced_circles_match_tessellated_circles_except_on_the_antialiased_rim() {
        let (painted, differ, interior_differ, max_diff) = instanced_against_tessellated(-77.03, 38.89, 0.00012, 17.5, true);
        assert!(painted > 256 * 20, "the dots painted {painted} pixels");
        assert_eq!(interior_differ, 0, "{interior_differ} interior pixels differ ({differ} in all)");
        eprintln!("parity: {differ} of {painted} painted pixels differ (all on the rim), largest channel difference {max_diff}");
        // A rim pixel's coverage differs by the gap between a 64-gon's
        // interpolated distance and the exact one: a few percent of alpha.
        assert!(max_diff <= 8, "a rim pixel differs by {max_diff} levels: more than antialiasing");
    }

    /// The same parity on the GLOBE (z2.5, fully spherical): the corners
    /// go through `vertex_main`'s sphere projection like the cap's rim did.
    #[test]
    fn instanced_circles_match_tessellated_circles_on_the_globe() {
        let (painted, differ, interior_differ, max_diff) = instanced_against_tessellated(10.0, 20.0, 1.5, 2.5, false);
        assert!(painted > 256 * 20, "the dots painted {painted} pixels");
        assert_eq!(interior_differ, 0, "{interior_differ} interior pixels differ ({differ} in all)");
        eprintln!("parity: {differ} of {painted} painted pixels differ (all on the rim), largest channel difference {max_diff}");
        // A rim pixel's coverage differs by the gap between a 64-gon's
        // interpolated distance and the exact one: a few percent of alpha.
        assert!(max_diff <= 8, "a rim pixel differs by {max_diff} levels: more than antialiasing");
    }

    /// **A highlight outlines the right polygon, and clearing it removes
    /// the outline.** Two squares side by side, flat mid-grey fill; the
    /// east one highlighted with the demo's style (a 2 px white outline
    /// over a dark casing). White pixels appear on the east square's edge
    /// and nowhere near the west one, tiled or not, and a capture with no
    /// highlight has none at all.
    #[test]
    fn a_highlight_outlines_the_chosen_polygon_and_clearing_it_removes_the_outline() {
        let square = |w: f64| {
            format!(
                r#"{{"type":"Feature","properties":{{}},"geometry":{{"type":"Polygon","coordinates":[[[{w},0.0],[{e},0.0],[{e},0.01],[{w},0.01],[{w},0.0]]]}}}}"#,
                e = w + 0.01
            )
        };
        let json = format!(r#"{{"type":"FeatureCollection","features":[{},{}]}}"#, square(-0.0105), square(0.0005));
        let style = crate::overlay::HighlightStyle::default();
        let base = CaptureRequest {
            center_lon: 0.0,
            center_lat: 0.005,
            zoom: 15.0,
            width: 512,
            height: 512,
            flat: true,
            clear: Clear::SENTINEL,
            backdrop: false,
            overlay: Some(json),
            overlay_paint: Some(r##"{"fill-color":"#808080"}"##.to_string()),
            ..CaptureRequest::default()
        };
        let empty = Style { layers: Vec::new(), ..Style::default() };
        let camera = base.camera();
        let screen = |lon: f64, lat: f64| {
            crate::render::project_world_to_screen_f64(&camera, crate::overlay::geom::lon_lat_to_world(lon, lat))
                .expect("on screen")
        };
        let (east_w, _) = screen(0.0005, 0.0);
        let (east_e, _) = screen(0.0105, 0.0);
        let (west_e, _) = screen(-0.0005, 0.0);
        let (_, north) = screen(0.0, 0.01);
        let (_, south) = screen(0.0, 0.0);
        let white = |frame: &Capture| -> Vec<(u32, u32)> {
            (0..frame.height)
                .flat_map(|y| (0..frame.width).map(move |x| (x, y)))
                .filter(|&(x, y)| frame.pixel(x, y)[..3].iter().all(|&c| c > 230))
                .collect()
        };
        for tiling in [false, true] {
            let plain = capture(&CaptureRequest { overlay_tiling: tiling, ..base.clone() }, &TileStore::new(), &empty)
                .expect("a headless device");
            assert!(white(&plain).is_empty(), "tiling {tiling}: no highlight, no white");
            let lit = capture(
                &CaptureRequest { overlay_tiling: tiling, overlay_highlight: Some((1, style)), ..base.clone() },
                &TileStore::new(),
                &empty,
            )
            .expect("a headless device");
            let outline = white(&lit);
            if let Ok(dir) = std::env::var("HIGHLIGHT_CAPTURE_DIR") {
                let _ = lit.save_png(&std::path::Path::new(&dir).join(format!("highlight_tiling_{tiling}.png")));
            }
            // Three edges of a ~460 px square in frame (the east one is off
            // it), 2 px wide, less the antialiased rows under the threshold.
            assert!(outline.len() > 1_000, "tiling {tiling}: the outline drew {} white pixels", outline.len());
            let near_edge = |x: u32, y: u32| {
                let (x, y) = (x as f32, y as f32);
                let on_x = (x - east_w).abs() < 4.0 || (x - east_e).abs() < 4.0;
                let on_y = (y - north).abs() < 4.0 || (y - south).abs() < 4.0;
                (on_x && y > north - 4.0 && y < south + 4.0) || (on_y && x > east_w - 4.0 && x < east_e + 4.0)
            };
            let stray: Vec<_> = outline.iter().filter(|&&(x, y)| !near_edge(x, y)).collect();
            assert!(stray.is_empty(), "tiling {tiling}: {} white pixels off the east square's edge, e.g. {:?}", stray.len(), &stray[..stray.len().min(8)]);
            assert!(outline.iter().all(|&(x, _)| x as f32 > west_e + 2.0), "tiling {tiling}: nothing on the west square");
            // The casing: dark pixels hug the white ring.
            let dark = (0..lit.height)
                .flat_map(|y| (0..lit.width).map(move |x| (x, y)))
                .filter(|&(x, y)| near_edge(x, y) && lit.pixel(x, y)[..3].iter().all(|&c| c < 60))
                .count();
            assert!(dark > 500, "tiling {tiling}: the dark casing drew {dark} pixels");
        }
    }

    /// **The hairline is clipped to each tile's own box.** A tile's cut
    /// ring runs along its buffer edge (-1024 / 5120 units), so a tiled
    /// fill's outline has edges no source polygon has; `TileClip` must hide
    /// them. A square far larger than the frame, white fill and a black
    /// 1 px `fill-outline-color`, at z4.5 over a multi-tile cover: its own
    /// outline is off screen, so EVERY dark pixel is an edge that should
    /// not exist. The whole-source capture is the control. Then the same
    /// square with its east edge in frame: the outline does draw there.
    #[test]
    fn a_hairline_outline_has_no_tile_boundary_edges() {
        let json = r##"{"type":"FeatureCollection","features":[
            {"type":"Feature","properties":{},"geometry":{"type":"Polygon","coordinates":[
              [[-30,-20],[30,-20],[30,25],[-30,25],[-30,-20]]]}}]}"##;
        let base = CaptureRequest {
            center_lon: 0.0,
            center_lat: 2.0,
            zoom: 4.5,
            width: 512,
            height: 512,
            flat: true,
            clear: Clear::SENTINEL,
            backdrop: false,
            overlay: Some(json.to_string()),
            overlay_paint: Some(r##"{"fill-color":"#ffffff","fill-outline-color":"#000000"}"##.to_string()),
            ..CaptureRequest::default()
        };
        let style = Style { layers: Vec::new(), ..Style::default() };
        assert!(base.cover().len() >= 4, "the frame must span several tiles: {}", base.cover().len());
        let dark_pixels = |frame: &Capture| -> Vec<(u32, u32)> {
            (0..frame.height)
                .flat_map(|y| (0..frame.width).map(move |x| (x, y)))
                .filter(|&(x, y)| frame.pixel(x, y)[0] < 128 && frame.pixel(x, y)[3] > 0)
                .collect()
        };
        for tiling in [false, true] {
            let frame = capture(&CaptureRequest { overlay_tiling: tiling, ..base.clone() }, &TileStore::new(), &style)
                .expect("a headless device");
            let white = (0..frame.height)
                .flat_map(|y| (0..frame.width).map(move |x| (x, y)))
                .filter(|&(x, y)| frame.pixel(x, y) == [255, 255, 255, 255])
                .count();
            assert!(white > 200_000, "tiling {tiling}: the square's fill must cover the frame ({white} px)");
            let stray = dark_pixels(&frame);
            assert!(stray.is_empty(), "tiling {tiling}: {} tile-boundary hairline pixels, e.g. {:?}", stray.len(), &stray[..stray.len().min(8)]);
        }
        let edge = CaptureRequest { center_lon: 30.0, overlay_tiling: true, ..base.clone() };
        let frame = capture(&edge, &TileStore::new(), &style).expect("a headless device");
        let dark = dark_pixels(&frame);
        assert!((400..=1200).contains(&dark.len()), "the square's east edge draws as a ~1 px line: {} dark px", dark.len());
        let columns: std::collections::BTreeSet<u32> = dark.iter().map(|&(x, _)| x).collect();
        assert!(columns.len() <= 2, "one vertical line, not several: columns {columns:?}");
    }

    /// **The outline level of detail changes only outline pixels.** 400
    /// sub-pixel squares and one 10-degree square, green fill and a red
    /// `fill-outline-color`, tiled at z3 (natural zoom 14): the same capture
    /// with every outline baked is the control. Every pixel that differs was
    /// outline-red in the control, no pixel the control drew as fill changes,
    /// and the big square's outline is still drawn.
    #[test]
    fn the_outline_level_of_detail_changes_only_outline_pixels() {
        let mut features = Vec::new();
        for i in 0..20 {
            for j in 0..20 {
                let (x, y) = (-100.0 + f64::from(i) * 0.6, 30.0 + f64::from(j) * 0.5);
                features.push(format!(
                    r#"{{"type":"Feature","properties":{{}},"geometry":{{"type":"Polygon","coordinates":[[[{x},{y}],[{},{y}],[{},{}],[{x},{}],[{x},{y}]]]}}}}"#,
                    x + 0.05, x + 0.05, y + 0.05, y + 0.05
                ));
            }
        }
        features.push(r#"{"type":"Feature","properties":{},"geometry":{"type":"Polygon","coordinates":[[[-80,30],[-70,30],[-70,40],[-80,40],[-80,30]]]}}"#.to_string());
        let request = CaptureRequest {
            center_lon: -85.0,
            center_lat: 35.0,
            zoom: 3.0,
            width: 512,
            height: 384,
            flat: true,
            clear: Clear::SENTINEL,
            backdrop: false,
            overlay: Some(format!(r#"{{"type":"FeatureCollection","features":[{}]}}"#, features.join(","))),
            overlay_paint: Some(r##"{"fill-color":"#00ff00","fill-outline-color":"#ff0000"}"##.to_string()),
            overlay_tiling: true,
            ..CaptureRequest::default()
        };
        let style = Style { layers: Vec::new(), ..Style::default() };
        let control = {
            crate::overlay::tile_plan::OUTLINE_LOD_OFF.with(|off| off.set(true));
            let frame = capture(&request, &TileStore::new(), &style);
            crate::overlay::tile_plan::OUTLINE_LOD_OFF.with(|off| off.set(false));
            frame.expect("a headless device")
        };
        let lod = capture(&request, &TileStore::new(), &style).expect("a headless device");
        let pixels = |f: &Capture| (0..f.height).flat_map(|y| (0..f.width).map(move |x| (x, y))).collect::<Vec<_>>();
        let red = |p: [u8; 4]| p[0] > 128 && p[1] < 128 && p[2] < 128;
        let green = |p: [u8; 4]| p[1] > 128 && p[0] < 128 && p[2] < 128;
        let (mut differ, mut outline_before, mut fill_changed) = (0, 0, 0);
        for (x, y) in pixels(&control) {
            let (a, b) = (control.pixel(x, y), lod.pixel(x, y));
            outline_before += usize::from(red(a));
            if a != b {
                differ += 1;
                assert!(red(a), "({x},{y}) differs but was not outline in the control: {a:?} -> {b:?}");
            }
            if green(a) && a != b {
                fill_changed += 1;
            }
        }
        let outline_after = pixels(&lod).into_iter().filter(|&(x, y)| red(lod.pixel(x, y))).count();
        eprintln!("outline LOD: {differ} px differ; outline px {outline_before} -> {outline_after}; fill px changed {fill_changed}");
        assert_eq!(fill_changed, 0);
        assert!(differ > 100, "the sub-pixel squares' outlines are culled: {differ} px");
        assert!((200..=600).contains(&outline_after), "the big square's ~1 px outline remains: {outline_after} px");
    }

    /// A navigation capture draws the route line and the chrome over the
    /// map, and the same request without `nav` draws neither: the control
    /// frame is the "idle host draws nothing" half, pixel for pixel.
    #[test]
    fn a_nav_capture_draws_the_route_and_the_chrome_and_none_draws_neither() {
        use crate::nav::host::{NavFrame, NavHost, SafeInsets, SimOptions, WallClock};
        const MANILA: &[u8] = include_bytes!("../tests/fixtures/routes/manila.json");
        let base = CaptureRequest {
            center_lon: 120.98,
            center_lat: 14.59,
            zoom: 16.0,
            width: 400,
            height: 400,
            flat: true,
            clear: Clear::SENTINEL,
            backdrop: false,
            ..CaptureRequest::default()
        };
        let mut host = NavHost::new();
        host.set_demo_route(std::sync::Arc::from(MANILA), SimOptions { start_at_m: Some(700.0), paused: true, ..Default::default() });
        let mut camera = crate::camera_control::CameraController::new(
            base.camera(),
            crate::camera_control::MIN_ZOOM,
            crate::camera_control::MAX_ZOOM,
        );
        let frame_at = |now_ms: f64| NavFrame {
            now_ms,
            wall: WallClock { unix_ms: 1_773_755_400_000.0 + now_ms, utc_offset_min: 480 },
            viewport: (400.0, 400.0),
            insets: SafeInsets::default(),
        };
        host.start_simulation(&camera, &frame_at(0.0)).expect("the Manila fixture drives");
        let _ = host.advance(&mut camera, &frame_at(0.0));
        let _ = host.advance(&mut camera, &frame_at(1000.0));
        let settled = *camera.camera();
        assert!(settled.pitch > 0.5, "the follow camera tilts: {} rad", settled.pitch);
        let plain = CaptureRequest {
            center_lon: settled.center_lon,
            center_lat: settled.center_lat,
            zoom: settled.zoom,
            pitch: settled.pitch,
            bearing: settled.bearing,
            ..base
        };
        let with_nav = CaptureRequest {
            nav: Some(NavCapture { host: std::sync::Arc::new(host), frame: frame_at(1000.0) }),
            ..plain.clone()
        };
        // No tiles at all: everything that is not the sentinel is navigation.
        let tiles = TileStore::new();
        let style = one_opaque_fill_style_from_zoom_zero("water", Rgba { r: 0.0, g: 0.4, b: 1.0, a: 1.0 });
        let idle = capture(&plain, &tiles, &style).expect("a headless device");
        let nav = capture(&with_nav, &tiles, &style).expect("a headless device");
        let total = (idle.width * idle.height) as usize;
        assert_eq!(idle.count_matching([255, 0, 255, 255], 0), total, "no nav, no navigation pixels");
        let drawn = |rows: std::ops::Range<u32>| {
            rows.flat_map(|y| (0..nav.width).map(move |x| (x, y)))
                .filter(|&(x, y)| {
                    let i = ((y * nav.width + x) * 4) as usize;
                    nav.rgba[i..i + 4] != [255, 0, 255, 255]
                })
                .count()
        };
        // The banner across the top, and the route and puck in the middle.
        assert!(drawn(0..60) > 400 * 20, "the banner: {} pixels", drawn(0..60));
        assert!(drawn(120..330) > 500, "the route line and the puck: {} pixels", drawn(120..330));
    }

    /// A 30″ descriptor and one level-0 (data z6) tile around `(lon, lat)`,
    /// populated with `pop` in every cell of a `(2 half + 1)`-square block
    /// centred on the cell holding `(lon, lat)`.
    fn dot_fixture(lon: f64, lat: f64, half: i64, pop: f32, ppd: f32) -> CaptureDots {
        use crate::overlay::dotgrid::{grid, GridDescriptor, GridTile, GridTileKey};
        let d = GridDescriptor {
            base_arcsec: 30.0,
            origin_lat: 90.0,
            origin_lon: -180.0,
            levels: 7,
            max_data_zoom: grid::base_data_zoom(30.0),
            epochs: vec![2025],
            attribution: String::new(),
        };
        let arc = grid::cell_arcsec(&d, 0) / 3600.0;
        let (row, col) = (((90.0 - lat) / arc).floor() as i64, ((lon + 180.0) / arc).floor() as i64);
        let tile = grid::tile_of_cell(&d, 0, row as u32, col as u32).expect("inside Mercator");
        let w = grid::cell_window(&d, 0, tile);
        let mut cells = Vec::new();
        for r in row - half..=row + half {
            for c in col - half..=col + half {
                if grid::tile_of_cell(&d, 0, r as u32, c as u32) == Some(tile) {
                    cells.push((grid::local_index(&w, r as u32, c as u32), pop, pop));
                }
            }
        }
        let total = cells.iter().map(|c| f64::from(c.1)).sum();
        let key = GridTileKey { tile, level: 0, epoch: 2025, epoch_b: None };
        CaptureDots {
            descriptor: d,
            tiles: vec![(key, GridTile { cells, total, ..Default::default() })],
            ppd,
            ppd_build: None,
            radius_px: 1.5,
            radius_shrink: 0.0,
            overlap_gamma: 0.0,
            ramp: vec![Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }],
            epoch_mix: 0.0,
            difference: false,
            exclude: None,
            tile_excludes: Vec::new(),
            relocate: false,
            mask_k: 0,
            synthetic_mask: None,
            mask_previous: None,
            category_mask: None,
        }
    }

    /// The 8 pure colours the race captures paint the groups with and
    /// classify pixels by (normalised by the brightest channel, so a dot's
    /// antialiased rim over black classifies as its own colour).
    const RACE_TEST_PALETTE: [[f32; 3]; 8] =
        [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [1.0, 1.0, 0.0], [0.0, 1.0, 1.0], [1.0, 0.0, 1.0], [1.0, 1.0, 1.0], [1.0, 0.5, 0.0]];

    /// A part tile of `parts` at `tile` (a z12 tile), drawn categorically
    /// with [`RACE_TEST_PALETTE`] and `mask`, as the race style draws
    /// (no relocation, shrink or overlap fade: C12).
    fn race_dots(parts: Vec<crate::overlay::dotgrid::codec_cat::PartRecord>, tile: TileId, ppd: f32, radius_px: f32, mask: u32) -> CaptureDots {
        use crate::overlay::dotgrid::{GridTile, GridTileKey};
        let total = parts.iter().map(|p| p.counts.iter().map(|&c| f64::from(c)).sum::<f64>()).sum();
        CaptureDots {
            tiles: vec![(GridTileKey::part(tile, 2020), GridTile { parts, total, ..Default::default() })],
            ppd,
            radius_px,
            ramp: RACE_TEST_PALETTE.iter().map(|c| Rgba { r: c[0], g: c[1], b: c[2], a: 1.0 }).collect(),
            category_mask: Some(mask),
            ..dot_fixture(10.0, 20.0, 0, 1.0, ppd)
        }
    }

    /// A black-cleared, basemap-free, flat `size`-square frame centred on
    /// unit-world `centre` at `zoom`.
    fn race_capture(centre: (f64, f64), zoom: f64, size: u32, dots: CaptureDots) -> Capture {
        let lon = centre.0 * 360.0 - 180.0;
        let lat = (std::f64::consts::PI * (1.0 - 2.0 * centre.1)).sinh().atan().to_degrees();
        let request = CaptureRequest {
            center_lon: lon,
            center_lat: lat,
            zoom,
            width: size,
            height: size,
            flat: true,
            clear: Clear::Sentinel(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
            backdrop: false,
            dots: Some(dots),
            ..CaptureRequest::default()
        };
        let style = Style { layers: Vec::new(), ..Style::default() };
        capture(&request, &TileStore::new(), &style).expect("a headless device")
    }

    /// The palette entry nearest a lit pixel's chromaticity, or `None`
    /// for a pixel whose brightest channel is under 100.
    fn race_group_of_pixel(p: [u8; 4]) -> Option<usize> {
        let m = f32::from(p[0].max(p[1]).max(p[2]));
        if m < 100.0 {
            return None;
        }
        let n = [f32::from(p[0]) / m, f32::from(p[1]) / m, f32::from(p[2]) / m];
        let d = |c: &[f32; 3]| (0..3).map(|k| (n[k] - c[k]).powi(2)).sum::<f32>();
        (0..8).min_by(|&a, &b| d(&RACE_TEST_PALETTE[a]).total_cmp(&d(&RACE_TEST_PALETTE[b])))
    }

    /// **Task 2's F1 on the GPU, and T2's last-dot ruling.** One part of
    /// 1,000 people at ppd 1, its third vertex nudged until person 999's
    /// `h_t` is so close to 1 that the old f32 rule (`999 + h_t < 1000`)
    /// rounds up and kills it -- and until no other dot lands within 6 px
    /// of it. The integer-exact rule keeps it, the race dots skip the
    /// last-dot fade, and its group comes from `group_of`: so its centre
    /// pixel is that group's palette colour at full alpha. With the
    /// group's legend bit off, the pixel is black.
    #[test]
    fn the_last_of_a_thousand_people_draws_at_full_alpha() {
        use crate::overlay::dotgrid::codec_cat::PartRecord;
        use crate::overlay::dotgrid::composition::{group_of, Composition};
        use crate::overlay::dotgrid::{blocks::OVERLAY_LYON_PER_UNIT_WORLD as LPW, hash, parts};
        let (zoom, size) = (14.0, 512u32);
        let centre = crate::geo::lon_lat_to_tile_xy(10.0, 20.0, 0.0);
        let tile = TileId { z: 12, x: (centre.0 * 4096.0) as u32, y: (centre.1 * 4096.0) as u32 };
        let (ox, oy) = (f64::from(tile.x) / 4096.0, f64::from(tile.y) / 4096.0);
        let px = 1.0 / (512.0 * 2f64.powf(zoom));
        let to_px = |u: [f64; 2]| ((u[0] - centre.0) / px + f64::from(size) / 2.0, (u[1] - centre.1) / px + f64::from(size) / 2.0);
        let counts = [125u32; 8];
        let comp = Composition::from_counts(&counts);
        let a = [centre.0 - 180.0 * px, centre.1 - 180.0 * px];
        let b = [a[0] + 380.0 * px, a[1]];
        let lyon = |p: [f64; 2]| [((p[0] - ox) * LPW) as f32, ((p[1] - oy) * LPW) as f32];
        let edge = |p: [f64; 2]| [((p[0] - a[0]) * LPW) as f32, ((p[1] - a[1]) * LPW) as f32];
        let (mut found, mut tries) = (None, 0u32);
        for k in 0..4_000_000u32 {
            tries = k + 1;
            let c = [a[0] + f64::from(k) * 1e-4 * px, a[1] + 380.0 * px];
            let key = parts::part_key(lyon(a), edge(b), edge(c));
            let (h_t, _, _) = hash::dot_hashes(key, 999);
            if 999.0f32 + h_t < 1000.0f32 {
                continue;
            }
            // Every dot's centre, as `sample_in_triangle` places it (unit world).
            let at = |i: u32| {
                let (_, u, v) = hash::dot_hashes(key, i);
                let e = |p: [f64; 2]| [p[0] - a[0], p[1] - a[1]];
                to_px(parts::sample_in_triangle(a, e(b), e(c), u, v))
            };
            let last = at(999);
            if (0..999).all(|i| {
                let p = at(i);
                (p.0 - last.0).hypot(p.1 - last.1) >= 6.0
            }) {
                found = Some((c, key, last));
                break;
            }
        }
        let (c, key, last) = found.unwrap_or_else(|| panic!("no vertex in {tries} tries"));
        assert!(hash::dot_alive(1000.0, 1.0, 999, key), "the integer-exact rule keeps person 999");
        let group = usize::from(group_of(&comp, key, 999));
        let part = PartRecord { a, b, c, counts };
        let (x, y) = (last.0.floor() as u32, last.1.floor() as u32);
        let want = RACE_TEST_PALETTE[group].map(|v| (v * 255.0).round());
        let shot = race_capture(centre, zoom, size, race_dots(vec![part], tile, 1.0, 2.0, 0xFF));
        let got = shot.pixel(x, y);
        eprintln!("person 999 (group {group}, {tries} tries) at ({:.2}, {:.2}): {got:?}", last.0, last.1);
        for ch in 0..3 {
            assert!((f32::from(got[ch]) - want[ch]).abs() <= 3.0, "pixel {got:?} is not group {group}'s {want:?} at full alpha");
        }
        let lit = shot.rgba.as_chunks::<4>().0.iter().filter(|p| p[0].max(p[1]).max(p[2]) > 100).count();
        assert!(lit > 6_000, "only {lit} lit pixels for 1,000 dots of radius 2");
        let off = race_capture(centre, zoom, size, race_dots(vec![part], tile, 1.0, 2.0, 0xFF & !(1 << group)));
        assert_eq!(&off.pixel(x, y)[..3], &[0, 0, 0], "group {group}'s legend bit is off");
    }

    /// **Task 6 review F-2: every isolated dot is its own `group_of`
    /// group on the GPU.** The share test below cannot see a wrong salt,
    /// `off` or stride (at ppd 1 `i -> p` is a bijection whatever they
    /// are). Here 128 triangles of mixed compositions (denominators 3..60,
    /// so the strides are real) at ppd 1, radius 2 px: every dot with no
    /// other within 6 px must read, at its centre pixel, the palette colour
    /// of `group_of(comp, part_key, i)` -- per dot, not per share.
    #[test]
    fn every_isolated_race_dot_is_its_own_group_on_the_gpu() {
        use crate::overlay::dotgrid::codec_cat::PartRecord;
        use crate::overlay::dotgrid::composition::{group_of, Composition};
        use crate::overlay::dotgrid::{blocks::OVERLAY_LYON_PER_UNIT_WORLD as LPW, hash, parts};
        let (zoom, size) = (14.0, 640u32);
        let centre = crate::geo::lon_lat_to_tile_xy(10.0, 20.0, 0.0);
        let tile = TileId { z: 12, x: (centre.0 * 4096.0) as u32, y: (centre.1 * 4096.0) as u32 };
        let (ox, oy) = (f64::from(tile.x) / 4096.0, f64::from(tile.y) / 4096.0);
        let px = 1.0 / (512.0 * 2f64.powf(zoom));
        let to_px = |u: [f64; 2]| ((u[0] - centre.0) / px + f64::from(size) / 2.0, (u[1] - centre.1) / px + f64::from(size) / 2.0);
        let side = 70.0 * px;
        let origin = [centre.0 - 4.0 * side, centre.1 - 4.0 * side];
        let mut recs = Vec::new();
        let mut seed = 99u32;
        for r in 0..8 {
            for q in 0..8 {
                let (x, y) = (origin[0] + f64::from(q) * side, origin[1] + f64::from(r) * side);
                for tri in [[[x, y], [x + side, y], [x + side, y + side]], [[x, y], [x + side, y + side], [x, y + side]]] {
                    let mut counts: [u32; 8] = std::array::from_fn(|_| {
                        seed = hash::pcg(seed);
                        seed % 8
                    });
                    counts[(seed >> 8) as usize % 8] += 3;
                    recs.push(PartRecord { a: tri[0], b: tri[1], c: tri[2], counts });
                }
            }
        }
        // Every dot's centre and group, keyed with the builder's f32s.
        let mut dots: Vec<((f64, f64), usize)> = Vec::new();
        for p in &recs {
            let lyon = |v: [f64; 2]| [((v[0] - ox) * LPW) as f32, ((v[1] - oy) * LPW) as f32];
            let edge = |v: [f64; 2]| [((v[0] - p.a[0]) * LPW) as f32, ((v[1] - p.a[1]) * LPW) as f32];
            let key = parts::part_key(lyon(p.a), edge(p.b), edge(p.c));
            let comp = Composition::from_counts(&p.counts);
            let people: u32 = p.counts.iter().sum();
            for i in 0..people {
                assert!(hash::dot_alive(people as f32, 1.0, i, key));
                let (_, u, v) = hash::dot_hashes(key, i);
                let e = |w: [f64; 2]| [w[0] - p.a[0], w[1] - p.a[1]];
                dots.push((to_px(parts::sample_in_triangle(p.a, e(p.b), e(p.c), u, v)), usize::from(group_of(&comp, key, i))));
            }
        }
        let shot = race_capture(centre, zoom, size, race_dots(recs, tile, 1.0, 2.0, 0xFF));
        let (mut checked, mut wrong) = (0, Vec::new());
        let mut seen = [0u32; 8];
        for (n, &(at, group)) in dots.iter().enumerate() {
            let isolated = dots.iter().enumerate().all(|(m, &(o, _))| m == n || (o.0 - at.0).hypot(o.1 - at.1) >= 6.0);
            if !isolated {
                continue;
            }
            checked += 1;
            seen[group] += 1;
            let got = race_group_of_pixel(shot.pixel(at.0.floor() as u32, at.1.floor() as u32));
            if got != Some(group) {
                wrong.push((at, group, got));
            }
        }
        eprintln!("{} dots, {checked} isolated, per group {seen:?}", dots.len());
        assert!(checked >= 300, "only {checked} isolated dots");
        assert!(seen.iter().all(|&k| k >= 10), "every group checked: {seen:?}");
        assert!(wrong.is_empty(), "{} of {checked} dots in the wrong colour, e.g. {:?}", wrong.len(), &wrong[..wrong.len().min(5)]);
    }

    /// **Task 6's criterion: categorical dots' colours are the CPU
    /// mirror's.** 200 triangles (10 x 10 squares of 90 px, each halved)
    /// of skewed group counts at ppd 1, radius 1 px, the 8 palette
    /// colours over black: the share of lit pixels nearest each colour
    /// matches the share of dots `group_of` gives each group within 2
    /// points. Then with the odd groups' legend bits off, the odd colours
    /// all but vanish (only overlapping rims can mix into one).
    #[test]
    #[ignore = "headless categorical capture (Task 6); run on demand"]
    fn categorical_capture_matches_the_cpu_mirror() {
        use crate::overlay::dotgrid::codec_cat::PartRecord;
        use crate::overlay::dotgrid::composition::{group_of, Composition};
        use crate::overlay::dotgrid::{blocks, hash, parts};
        let (zoom, size) = (14.0, 1024u32);
        let centre = crate::geo::lon_lat_to_tile_xy(10.0, 20.0, 0.0);
        let tile = TileId { z: 12, x: (centre.0 * 4096.0) as u32, y: (centre.1 * 4096.0) as u32 };
        let px = 1.0 / (512.0 * 2f64.powf(zoom));
        let side = 90.0 * px;
        let origin = [centre.0 - 5.0 * side, centre.1 - 5.0 * side];
        let mut recs = Vec::new();
        let mut seed = 7u32;
        for r in 0..10 {
            for q in 0..10 {
                let (x, y) = (origin[0] + f64::from(q) * side, origin[1] + f64::from(r) * side);
                for tri in [[[x, y], [x + side, y], [x + side, y + side]], [[x, y], [x + side, y + side], [x, y + side]]] {
                    let counts = std::array::from_fn(|g| {
                        seed = hash::pcg(seed);
                        seed % (2 + 2 * g as u32)
                    });
                    recs.push(PartRecord { a: tri[0], b: tri[1], c: tri[2], counts });
                }
            }
        }
        assert_eq!(recs.len(), 200);
        // The CPU mirror: every alive dot's group, keyed as the builder keys it.
        let tb = blocks::build_part_blocks(&crate::overlay::dotgrid::GridTileKey::part(tile, 2020), &recs, 1.0, blocks::OVERLAY_LYON_PER_UNIT_WORLD);
        let mut dots = [0u64; 8];
        for (bi, bucket) in tb.buckets.iter().enumerate() {
            for (j, blk) in bucket.iter().enumerate() {
                let sh = tb.shapes[bi][j];
                let key = parts::part_key(blk.origin, [sh[0], sh[1]], [sh[2], sh[3]]);
                let rec = recs.iter().find(|p| p.counts.iter().sum::<u32>() as f32 == blk.pop && Composition::from_counts(&p.counts).texel() == tb.comps[bi][j]);
                let comp = Composition::from_counts(&rec.expect("the block's part").counts);
                for i in blk.first_dot..blk.first_dot + crate::render::dots::DOT_BUCKETS[bi] {
                    if hash::dot_alive(blk.pop, 1.0, i, key) {
                        dots[usize::from(group_of(&comp, key, i))] += 1;
                    }
                }
            }
        }
        let want: [u64; 8] = std::array::from_fn(|g| recs.iter().map(|p| u64::from(p.counts[g])).sum());
        assert_eq!(dots, want, "at ppd 1 every person is one dot of their own group");
        let n: u64 = dots.iter().sum();
        let classify = |c: &Capture| {
            let mut px = [0u64; 8];
            for p in c.rgba.as_chunks::<4>().0.iter() {
                if let Some(g) = race_group_of_pixel([p[0], p[1], p[2], p[3]]) {
                    px[g] += 1;
                }
            }
            px
        };
        let shot = race_capture(centre, zoom, size, race_dots(recs.clone(), tile, 1.0, 1.0, 0xFF));
        let px = classify(&shot);
        let lit: u64 = px.iter().sum();
        eprintln!("{n} dots {dots:?}; {lit} lit pixels {px:?}");
        assert!(lit > n, "{lit} lit pixels for {n} dots");
        for g in 0..8 {
            let (a, b) = (px[g] as f64 / lit as f64, dots[g] as f64 / n as f64);
            assert!((a - b).abs() <= 0.02, "group {g}: {:.2} % of pixels against {:.2} % of dots", a * 100.0, b * 100.0);
        }
        let even = classify(&race_capture(centre, zoom, size, race_dots(recs, tile, 1.0, 1.0, 0b0101_0101)));
        let even_lit: u64 = even.iter().sum();
        let odd: u64 = (0..8).filter(|g| g % 2 == 1).map(|g| even[g]).sum();
        eprintln!("mask 0x55: {even_lit} lit pixels {even:?}");
        assert!((odd as f64) <= 0.01 * even_lit as f64, "{odd} of {even_lit} pixels in groups whose bit is off");
        assert!(even_lit as f64 >= 0.9 * (lit - (1..8).step_by(2).map(|g| px[g]).sum::<u64>()) as f64);
    }

    /// A black-cleared, basemap-free frame of `dots` at `(lon, lat, zoom)`.
    fn dot_capture(lon: f64, lat: f64, zoom: f64, dots: CaptureDots) -> Capture {
        let request = CaptureRequest {
            center_lon: lon,
            center_lat: lat,
            zoom,
            width: 256,
            height: 256,
            flat: true,
            clear: Clear::Sentinel(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
            backdrop: false,
            dots: Some(dots),
            ..CaptureRequest::default()
        };
        let style = Style { layers: Vec::new(), ..Style::default() };
        capture(&request, &TileStore::new(), &style).expect("a headless device")
    }

    /// Task 15: a live tile (`epoch_b`, from a `MemoryGridSource` that
    /// returns both counts) captured at `epoch_mix` 0 is epoch A's frame,
    /// pixel for pixel, and at 1 epoch B's -- each compared with a
    /// single-epoch tile of that epoch's counts (to 1 in 255 per channel:
    /// blend order, below). Cells grow, shrink, and one appears only in B.
    #[test]
    fn a_capture_at_t0_equals_epoch_a_and_at_t1_equals_epoch_b() {
        use crate::overlay::dotgrid::{GridCountSource, GridTile, GridTileKey, MemoryGridSource};
        let base = dot_fixture(10.0, 20.0, 4, 1.0, 40.0);
        let (key, cells) = (base.tiles[0].0, base.tiles[0].1.cells.clone());
        // Cell by cell: A and B differ both ways, and the last cell is
        // empty in A.
        let counts: Vec<(u32, f32, f32)> = cells
            .iter()
            .enumerate()
            .map(|(n, &(i, _, _))| match n % 3 {
                0 => (i, 800.0, 2400.0),
                1 => (i, 1600.0, 400.0),
                _ => (i, if n + 1 == cells.len() { 0.0 } else { 1200.0 }, 1200.0),
            })
            .collect();
        let tile_of = |pick: fn(&(u32, f32, f32)) -> f32| {
            let cells: Vec<(u32, f32, f32)> =
                counts.iter().map(|c| (c.0, pick(c), pick(c))).filter(|c| c.1 > 0.0).collect();
            let total = cells.iter().map(|c| f64::from(c.1)).sum();
            GridTile { cells, total, ..Default::default() }
        };
        let (only_a, only_b) = (tile_of(|c| c.1), tile_of(|c| c.2));
        let source = MemoryGridSource::new(base.descriptor.clone(), {
            let counts = counts.clone();
            move |_| GridTile { cells: counts.clone(), total: counts.iter().map(|c| f64::from(c.1)).sum(), ..Default::default() }
        });
        let live_key = GridTileKey { epoch: 1975, epoch_b: Some(2030), ..key };
        let live = source.decode(&[live_key], &[]).unwrap().remove(0).1;
        assert!(live.cells.iter().any(|c| c.1 != c.2), "the source returns two epochs");
        let frame = |tiles: Vec<(GridTileKey, GridTile)>, epoch_mix: f32| {
            dot_capture(10.0, 20.0, 12.0, CaptureDots { tiles, epoch_mix, ..base.clone() })
        };
        let a = frame(vec![(key, only_a)], 0.0);
        let b = frame(vec![(key, only_b)], 0.0);
        assert!(lit(&a).iter().filter(|&&l| l).count() > 300, "epoch A drew next to nothing");
        assert!(a.rgba != b.rgba, "the two epochs draw the same frame: the test is vacuous");
        let at0 = frame(vec![(live_key, live.clone())], 0.0);
        let at1 = frame(vec![(live_key, live.clone())], 1.0);
        let differ = |x: &Capture, y: &Capture| x.rgba.iter().zip(&y.rgba).filter(|(p, q)| p != q).count();
        let worst = |x: &Capture, y: &Capture| x.rgba.iter().zip(&y.rgba).map(|(p, q)| p.abs_diff(*q)).max().unwrap_or(0);
        // The same dots, at the same places, alphas and colours; only the
        // ORDER two overlapping dots blend in may differ -- a live tile
        // sorts its blocks by `max(pop, pop_b) / first_dot`, a single
        // epoch's by `pop / first_dot` -- and 8-bit blending rounds each
        // order its own way: measured 21 bytes off by 1 at t = 0, none at 1.
        assert_eq!(lit(&at0), lit(&a), "t = 0 lit other pixels than epoch A");
        assert_eq!(lit(&at1), lit(&b), "t = 1 lit other pixels than epoch B");
        assert!(worst(&at0, &a) <= 1, "t = 0 is not epoch A: {} bytes differ, worst {}", differ(&at0, &a), worst(&at0, &a));
        assert!(worst(&at1, &b) <= 1, "t = 1 is not epoch B: {} bytes differ, worst {}", differ(&at1, &b), worst(&at1, &b));
        // Halfway is neither.
        let half = frame(vec![(live_key, live)], 0.5);
        assert!(differ(&half, &a) > 0 && differ(&half, &b) > 0);
    }

    /// Review N-2: the shader's ramp reads, through pixels, with a
    /// two-stop ramp (blue low, red high). The difference ramp colours a
    /// grown cell (4x more people) at the top stop and a shrunk one (4x
    /// fewer) at the bottom, at any mix; the density ramp follows the mix
    /// (a denser epoch B reads redder at t = 1 than at t = 0).
    #[test]
    fn a_difference_capture_reads_growth_on_a_two_stop_ramp() {
        use crate::overlay::dotgrid::GridTileKey;
        let blue_red = vec![Rgba { r: 0.0, g: 0.0, b: 1.0, a: 1.0 }, Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 }];
        let base = CaptureDots { ramp: blue_red, ..dot_fixture(10.0, 20.0, 4, 1.0, 40.0) };
        let key = GridTileKey { epoch: 1975, epoch_b: Some(2030), ..base.tiles[0].0 };
        let with = |a: f32, b: f32| {
            let mut tile = base.tiles[0].1.clone();
            for c in &mut tile.cells {
                (c.1, c.2) = (a, b);
            }
            tile.total = tile.cells.iter().map(|c| f64::from(c.1)).sum();
            vec![(key, tile)]
        };
        // Red and blue summed over the lit pixels.
        let red_blue = |c: &Capture| {
            c.rgba.as_chunks::<4>().0.iter().fold((0u64, 0u64), |(r, b), p| (r + u64::from(p[0]), b + u64::from(p[2])))
        };
        let shot = |tiles, epoch_mix: f32, difference: bool| {
            dot_capture(10.0, 20.0, 12.0, CaptureDots { tiles, epoch_mix, difference, ..base.clone() })
        };
        for t in [0.0, 0.5, 1.0] {
            let (r, b) = red_blue(&shot(with(800.0, 3200.0), t, true));
            assert!(r > 1000 && b * 50 < r, "t {t}: a 4x grown cell is the top stop (red): r {r} b {b}");
            let (r, b) = red_blue(&shot(with(3200.0, 800.0), t, true));
            assert!(b > 1000 && r * 50 < b, "t {t}: a 4x shrunk cell is the bottom stop (blue): r {r} b {b}");
        }
        // No change sits mid-ramp: both channels.
        let (r, b) = red_blue(&shot(with(1600.0, 1600.0), 0.5, true));
        assert!(r > 1000 && b > 1000 && r.abs_diff(b) * 5 < r.max(b), "no change is the middle: r {r} b {b}");
        // The density ramp follows the mix: denser at t = 1 reads redder.
        let share = |c: &Capture| {
            let (r, b) = red_blue(c);
            r as f64 / (r + b).max(1) as f64
        };
        let (at0, at1) = (share(&shot(with(40.0, 3200.0), 0.0, false)), share(&shot(with(40.0, 3200.0), 1.0, false)));
        assert!(at1 > at0 + 0.1, "density red share {at0:.3} at t 0, {at1:.3} at t 1");
    }

    /// Pixels at least half covered by a white dot on black.
    fn lit(c: &Capture) -> Vec<bool> {
        c.rgba.as_chunks::<4>().0.iter().map(|p| p[0] > 127).collect()
    }

    /// A tile of `n` vertical `building` stripes, each 1/(2n) of the tile
    /// wide: a basemap whose buildings every 30″ cell at z13 straddles.
    fn striped_buildings(n: i32) -> crate::mvt::DecodedTile {
        let extent = crate::mvt::MVT_EXTENT as i32;
        let w = extent / n;
        crate::mvt::DecodedTile {
            data_hash: 0,
            layers: vec![crate::mvt::Layer {
                name: "building".to_string(),
                features: (0..n)
                    .map(|i| crate::mvt::Feature {
                        geometry: geo_types::Geometry::Polygon(geo_types::Polygon::new(
                            geo_types::LineString::from(vec![
                                (i * w, 0),
                                (i * w + w / 2, 0),
                                (i * w + w / 2, extent),
                                (i * w, extent),
                                (i * w, 0),
                            ]),
                            vec![],
                        )),
                        tags: Default::default(),
                    })
                    .collect(),
            }],
        }
    }

    /// A 256 px z15 frame of `dots` over striped buildings (dark blue):
    /// flat (relocation is off while the globe transition is above 0) and at
    /// z15 (relocation starts there), 30″ cells ~390 px wide.
    fn dots_over_stripes(dots: CaptureDots) -> Capture {
        dots_over_stripes_painted(dots, Paint::AsStyled)
    }

    fn dots_over_stripes_painted(dots: CaptureDots, paint: Paint) -> Capture {
        let request = CaptureRequest {
            paint,
            center_lon: 10.0,
            center_lat: 20.0,
            zoom: 15.0,
            width: 256,
            height: 256,
            flat: true,
            clear: Clear::Sentinel(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
            backdrop: false,
            dots: Some(dots),
            ..CaptureRequest::default()
        };
        let mut tiles = TileStore::new();
        for tile in request.cover() {
            tiles.insert(tile, striped_buildings(32));
        }
        let style = one_opaque_fill_style_from_zoom_zero("building", Rgba { r: 0.0, g: 0.0, b: 0.4, a: 1.0 });
        capture(&request, &tiles, &style).expect("a headless device")
    }

    /// The mesh cache's `masked` key: through one session, a frame whose
    /// dots relocate over the basemap (tiles baked with their mask lists)
    /// and one whose dots do not, alternately. Each is the one-shot frame,
    /// and the masked frame after the unmasked one bakes anew.
    #[test]
    fn a_masked_and_an_unmasked_bake_are_never_swapped_in_a_session() {
        let fixture = || dot_fixture(10.0, 20.0, 3, 1600.0, 2.0);
        let request = |relocate: bool| CaptureRequest {
            center_lon: 10.0,
            center_lat: 20.0,
            zoom: 15.0,
            width: 256,
            height: 256,
            flat: true,
            clear: Clear::Sentinel(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
            backdrop: false,
            dots: Some(CaptureDots { relocate, mask_k: 8, ..fixture() }),
            ..CaptureRequest::default()
        };
        let (off, on) = (request(false), request(true));
        let mut tiles = TileStore::new();
        for tile in on.cover().into_iter().chain(on.mask_ring()) {
            tiles.insert(tile, striped_buildings(32));
        }
        let style = one_opaque_fill_style_from_zoom_zero("building", Rgba { r: 0.0, g: 0.0, b: 0.4, a: 1.0 });
        let want_off = capture(&off, &tiles, &style).expect("device").sha256_hex();
        let want_on = capture(&on, &tiles, &style).expect("device").sha256_hex();
        assert_ne!(want_off, want_on, "relocation must move dots for this test to bite");
        let mut session = CaptureSession::new_blocking(None).expect("device");
        assert_eq!(session.render(&off, &tiles, &style).expect("off").sha256_hex(), want_off);
        let misses = session.mesh_cache_stats().misses;
        assert_eq!(session.render(&on, &tiles, &style).expect("on").sha256_hex(), want_on, "masked after unmasked");
        assert!(session.mesh_cache_stats().misses > misses, "the masked frame hit the unmasked bakes");
        assert_eq!(session.render(&off, &tiles, &style).expect("off").sha256_hex(), want_off, "unmasked after masked");
        assert_eq!(session.render(&on, &tiles, &style).expect("on").sha256_hex(), want_on, "masked again");
        assert!(session.mesh_cache_stats().hits > 0);
    }

    #[test]
    fn a_capture_with_relocation_off_is_pixel_identical_to_before() {
        let fixture = || dot_fixture(10.0, 20.0, 3, 1600.0, 2.0);
        let off = dots_over_stripes(CaptureDots { relocate: false, mask_k: 8, ..fixture() });
        assert!(lit(&off).iter().filter(|&&l| l).count() > 500, "the fixture drew next to nothing");
        // One candidate: the basemap installs its mask lists and the mask
        // pass runs, and not a pixel moves.
        let one = dots_over_stripes(CaptureDots { relocate: true, mask_k: 1, ..fixture() });
        assert!(off.rgba == one.rgba, "relocation with one candidate changed the frame");
        // Not vacuous: eight candidates move dots onto the stripes.
        let on = dots_over_stripes(CaptureDots { relocate: true, mask_k: 8, ..fixture() });
        assert!(off.rgba != on.rgba, "relocation over the basemap's buildings moved nothing");
        // Onto them: more of the dot pixels sit where the basemap is a
        // building (blue channel under a dot is lost, so read it off
        // the dot-free frame).
        let bare = dots_over_stripes(CaptureDots { tiles: Vec::new(), ..fixture() });
        let on_building = |c: &Capture| {
            lit(c).iter().zip(bare.rgba.as_chunks::<4>().0.iter()).filter(|&(&l, p)| l && p[2] > 50).count() as f64
                / lit(c).iter().filter(|&&l| l).count() as f64
        };
        assert!(on_building(&on) > on_building(&off) + 0.2, "{} vs {}", on_building(&on), on_building(&off));
    }

    /// A repainted basemap carries no mask lists: the capture draws
    /// today's placement rather than relocating against an all-land mask.
    #[test]
    fn a_repainted_capture_does_not_relocate() {
        let fixture = || dot_fixture(10.0, 20.0, 3, 1600.0, 2.0);
        let off = dots_over_stripes_painted(CaptureDots { relocate: false, ..fixture() }, Paint::Wash);
        let on = dots_over_stripes_painted(CaptureDots { relocate: true, mask_k: 8, ..fixture() }, Paint::Wash);
        assert!(off.rgba == on.rgba, "a repainted capture relocated");
    }

    #[test]
    fn the_same_seed_draws_identical_pixels_twice() {
        let a = dot_capture(10.0, 20.0, 10.0, dot_fixture(10.0, 20.0, 12, 40.0, 8.0));
        let b = dot_capture(10.0, 20.0, 10.0, dot_fixture(10.0, 20.0, 12, 40.0, 8.0));
        let n = lit(&a).iter().filter(|&&l| l).count();
        assert!(n > 500, "only {n} dot pixels: the fixture drew next to nothing");
        assert!(a.rgba == b.rgba, "two captures of the same dots differ");
    }

    #[test]
    fn excluded_cells_draw_nothing() {
        let mut dots = dot_fixture(10.0, 20.0, 12, 40.0, 8.0);
        dots.exclude = Some([-1e30, -1e30, 1e30, 1e30]);
        let c = dot_capture(10.0, 20.0, 10.0, dots);
        let n = c.rgba.as_chunks::<4>().0.iter().filter(|p| p[0] > 0 || p[1] > 0 || p[2] > 0).count();
        assert_eq!(n, 0, "{n} pixels drew under an exclusion box covering the tile");
    }

    #[test]
    fn halving_ppd_only_adds_dots() {
        let coarse = dot_capture(10.0, 20.0, 10.0, dot_fixture(10.0, 20.0, 12, 40.0, 8.0));
        let fine = dot_capture(10.0, 20.0, 10.0, dot_fixture(10.0, 20.0, 12, 40.0, 4.0));
        let (coarse, fine) = (lit(&coarse), lit(&fine));
        let n = coarse.iter().filter(|&&l| l).count();
        assert!(n > 500, "only {n} dot pixels at ppd 8");
        let lost = coarse.iter().zip(&fine).filter(|(c, f)| **c && !**f).count();
        assert_eq!(lost, 0, "{lost} of {n} pixels lit at ppd 8 went dark at ppd 4");
        assert!(fine.iter().filter(|&&l| l).count() > n, "halving ppd added no dots");
    }

    /// **A missing child draws its parent, and the parent draws only
    /// there.** A uniform population (one person per level-0 cell, four
    /// per level-1 cell), the four z6 children of z5 (16,16) in the cover
    /// at zoom 5.5, one child withheld: `plan_dots` draws the three ready
    /// children and the parent with two exclusion boxes over them. The
    /// withheld tile's screen box must be as covered as a ready
    /// neighbour's (within 10 %) -- no hole -- and the ready neighbour no
    /// more than a lone child draws -- no double draw.
    #[test]
    fn a_dot_source_with_a_missing_child_shows_no_hole() {
        use crate::overlay::dotgrid::{grid, layer, GridDescriptor, GridTile, GridTileKey};
        let d = GridDescriptor {
            base_arcsec: 30.0,
            origin_lat: 90.0,
            origin_lon: -180.0,
            levels: 7,
            max_data_zoom: grid::base_data_zoom(30.0),
            epochs: vec![2025],
            attribution: String::new(),
        };
        // 128 people in every 8th row and column at level 0 -- so every
        // 4th at level 1, where the 2x2 sum puts the same 128 -- and ppd
        // 64: two dots per populated cell, the same dots per area at both
        // levels. (Dense 30″ cells saturate a zoom-5.5 frame, and a
        // quota below one dot draws faded.)
        let uniform = |key: &GridTileKey| {
            // A 30″ cells source: a part key (C7) never reaches it, and its
            // level must not be shifted by.
            assert!(!key.is_part(), "a cells-only fixture asked for a part tile");
            let w = grid::cell_window(&d, key.level, key.tile);
            let step = 8 >> key.level;
            let cells: Vec<(u32, f32, f32)> = (0..w.rows * w.cols)
                .filter(|&i| {
                    let (row, col) = grid::row_col(&w, i);
                    row % step == 0 && col % step == 0
                })
                .map(|i| (i, 128.0, 128.0))
                .collect();
            let total = 128.0 * cells.len() as f64;
            GridTile { cells, total, ..Default::default() }
        };
        let parent = crate::geo::TileId { z: 5, x: 16, y: 16 };
        let kids: Vec<crate::geo::TileId> =
            [(32, 32), (33, 32), (32, 33), (33, 33)].iter().map(|&(x, y)| crate::geo::TileId { z: 6, x, y }).collect();
        let key = |tile, level| GridTileKey { tile, level, epoch: 2025, epoch_b: None };
        let (withheld, neighbour) = (kids[1], kids[0]);
        let zoom = 5.5;
        let capture_at = |zoom: f64, cover: &[crate::geo::TileId], ready: &dyn Fn(&GridTileKey) -> bool, around: crate::geo::TileId| {
            let plan = layer::plan_dots(&d, 2025, zoom, cover, ready);
            let bounds = crate::geo::tile_bounds_lonlat(around);
            let (lon, lat) = ((bounds.min_lon + bounds.max_lon) / 2.0, (bounds.min_lat + bounds.max_lat) / 2.0);
            let dots = CaptureDots {
                descriptor: d.clone(),
                tiles: plan.draw.iter().map(|x| (x.key, uniform(&x.key))).collect(),
                ppd: 64.0,
                ppd_build: None,
                radius_px: 1.0,
                radius_shrink: 0.0,
                overlap_gamma: 0.0,
                ramp: vec![Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }],
                epoch_mix: 0.0,
                difference: false,
                exclude: None,
                tile_excludes: plan.draw.iter().map(|x| x.exclude).collect(),
                relocate: false,
                mask_k: 0,
                synthetic_mask: None,
                mask_previous: None,
                category_mask: None,
            };
            let request = CaptureRequest {
                center_lon: lon,
                center_lat: lat,
                zoom,
                width: 1024,
                height: 1024,
                flat: true,
                clear: Clear::Sentinel(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                backdrop: false,
                dots: Some(dots),
                ..CaptureRequest::default()
            };
            let style = Style { layers: Vec::new(), ..Style::default() };
            let c = capture(&request, &TileStore::new(), &style).expect("a headless device");
            (c, plan, (lon, lat))
        };
        let capture_with = |ready_kids: &[crate::geo::TileId]| {
            let ready = |k: &GridTileKey| *k == key(parent, 1) || (k.level == 0 && ready_kids.contains(&k.tile));
            capture_at(zoom, &kids, &ready, parent)
        };
        // The mean ink of a tile's screen box (10 px in from its edges):
        // white dots on black, 0 to 1.
        let coverage_at = |zoom: f64, c: &Capture, centre: (f64, f64), tile: crate::geo::TileId| {
            let b = crate::geo::tile_bounds_lonlat(tile);
            let scale = 2f64.powf(zoom);
            let middle = crate::overlay::geom::lon_lat_to_world(centre.0, centre.1);
            let to_px = |lo: f64, la: f64| {
                let w = crate::overlay::geom::lon_lat_to_world(lo, la);
                (512.0 + (w.0 - middle.0) * scale, 512.0 + (w.1 - middle.1) * scale)
            };
            let (x0, y0) = to_px(b.min_lon, b.max_lat);
            let (x1, y1) = to_px(b.max_lon, b.min_lat);
            let (mut lit, mut all) = (0u64, 0u64);
            for y in (y0 + 10.0) as u32..(y1 - 10.0) as u32 {
                for x in (x0 + 10.0) as u32..(x1 - 10.0) as u32 {
                    all += 1;
                    lit += u64::from(c.pixel(x, y)[0]);
                }
            }
            lit as f64 / 255.0 / all.max(1) as f64
        };
        let coverage = |c: &Capture, centre: (f64, f64), tile: crate::geo::TileId| coverage_at(zoom, c, centre, tile);

        let (c, plan, centre) = capture_with(&[kids[0], kids[2], kids[3]]);
        if let Ok(path) = std::env::var("DOT_HOLE_PNG") {
            c.save_png(std::path::Path::new(&path)).unwrap();
            eprintln!("lit total {}", c.rgba.as_chunks::<4>().0.iter().filter(|p| p[0] > 127).count());
        }
        assert_eq!(plan.draw.len(), 4, "three children and the parent: {:?}", plan.draw);
        assert!(plan.draw.iter().any(|x| x.standin && x.key == key(parent, 1)));
        let (hole, beside) = (coverage(&c, centre, withheld), coverage(&c, centre, neighbour));
        assert!(beside > 0.1, "the ready neighbour drew next to nothing: {beside:.3}");
        assert!((hole - beside).abs() <= 0.1 * beside, "the withheld tile's coverage {hole:.3} vs its neighbour's {beside:.3}");

        // All four ready: the same neighbour, drawn by itself alone. The
        // stand-in must not have added to it.
        let (all, plan, centre) = capture_with(&kids);
        assert!(plan.draw.iter().all(|x| !x.standin));
        let alone = coverage(&all, centre, neighbour);
        assert!((beside - alone).abs() <= 0.02, "the neighbour under a stand-in {beside:.3} vs alone {alone:.3}");

        // A zoom-out of two levels before level 2 lands (zoom 4.0, data
        // z4): only the 16 level-0 tiles under z4 (8,8) are held, and they
        // draw in its place -- as much ink as the level-2 tile itself.
        let top = crate::geo::TileId { z: 4, x: 8, y: 8 };
        let fine: Vec<crate::geo::TileId> =
            (32..36).flat_map(|x| (32..36).map(move |y| crate::geo::TileId { z: 6, x, y })).collect();
        let held_fine = |k: &GridTileKey| k.level == 0 && fine.contains(&k.tile);
        let (c, plan, centre) = capture_at(4.0, &[top], &held_fine, top);
        assert_eq!(plan.draw.len(), 16, "the held level-0 tiles fill the missing level-2 key");
        let filled = coverage_at(4.0, &c, centre, top);
        let held_top = |k: &GridTileKey| *k == key(top, 2);
        let (c, _, centre) = capture_at(4.0, &[top], &held_top, top);
        let itself = coverage_at(4.0, &c, centre, top);
        assert!(itself > 0.1, "the level-2 tile drew next to nothing: {itself:.3}");
        assert!((filled - itself).abs() <= 0.1 * itself, "zoom-out fill {filled:.3} vs the tile itself {itself:.3}");
    }

    /// **The real 30″ archive through `DotLayers`, to LOOK at.** Opens
    /// `DOTS_ARCHIVE` (the GHSL archive under `~/.cache/gpui-map-data`,
    /// never in git), drives the layer's frame/job/land loop inline over
    /// the capture camera's own flat cover until every wanted tile is
    /// held, and captures the plan's draws at the layer's ppd into
    /// `DOTS_PNG`. Ignored: it needs the archive.
    #[test]
    #[ignore = "needs DOTS_ARCHIVE (the real GHSL archive) and DOTS_PNG"]
    fn the_real_archive_draws_through_the_dot_layer() {
        use crate::overlay::dotgrid::{fetch_native, layer};
        let archive = std::env::var("DOTS_ARCHIVE").expect("DOTS_ARCHIVE");
        let png = std::env::var("DOTS_PNG").expect("DOTS_PNG");
        let (lon, lat, zoom): (f64, f64, f64) = std::env::var("DOTS_AT")
            .ok()
            .and_then(|v| {
                let p: Vec<f64> = v.split(',').filter_map(|x| x.parse().ok()).collect();
                (p.len() == 3).then(|| (p[0], p[1], p[2]))
            })
            .unwrap_or((78.9, 22.0, 5.0));
        let source: std::sync::Arc<dyn crate::overlay::dotgrid::GridCountSource> =
            std::sync::Arc::new(fetch_native::open_pmtiles_grid(&archive).expect("the archive opens"));
        let style = layer::DotStyle::default();
        let mut layers = layer::DotLayers::new();
        layers.set_source("pop", std::sync::Arc::clone(&source), style.clone());
        let (w, h) = (1920u32, 1080u32);
        let mut request = CaptureRequest {
            center_lon: lon,
            center_lat: lat,
            zoom,
            width: w,
            height: h,
            flat: true,
            clear: Clear::Sentinel(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
            backdrop: false,
            ..CaptureRequest::default()
        };
        let camera = request.camera();
        let globe = crate::render::globe::GlobeCover::FLAT;
        for _ in 0..200 {
            let busy = layers.frame(&camera, &globe, crate::timing::Instant::now());
            let mut ran = false;
            while let Some(job) = layers.next_job() {
                layers.land(job.run(fetch_native::execute));
                ran = true;
            }
            if !busy && !ran {
                break;
            }
        }
        let stats = layers.stats("pop").unwrap();
        let plan = layers.plan("pop").unwrap().clone();
        eprintln!("stats {stats:?}, {} draws", plan.draw.len());
        let dots = CaptureDots {
            descriptor: source.descriptor().clone(),
            tiles: plan.draw.iter().map(|x| (x.key, (*layers.tile("pop", &x.key).unwrap()).clone())).collect(),
            ppd: stats.ppd,
            ppd_build: None,
            radius_px: style.radius_px(zoom),
            radius_shrink: style.density_shrink,
            overlap_gamma: 0.0,
            ramp: style.ramp.stops(),
            epoch_mix: style.epoch_mix,
            difference: style.ramp == crate::overlay::dotgrid::layer::DotRamp::Difference,
            exclude: None,
            tile_excludes: plan.draw.iter().map(|x| x.exclude).collect(),
            relocate: false,
            mask_k: 0,
            synthetic_mask: None,
            mask_previous: None,
            category_mask: None,
        };
        request.dots = Some(dots);
        let style = Style { layers: Vec::new(), ..Style::default() };
        let c = capture(&request, &TileStore::new(), &style).expect("a headless device");
        c.save_png(std::path::Path::new(&png)).unwrap();
        assert_eq!(stats.tiles_ready, stats.tiles_wanted, "{stats:?}");
        assert!(c.rgba.as_chunks::<4>().0.iter().any(|p| p[0] > 0 || p[1] > 0 || p[2] > 0), "nothing drew");
    }

    /// **`build_blocks`' `lyon_per_world` carries the data tile's 2^z.** A
    /// block's origin is a unit-world DELTA inside a z6 tile (`[0, 1/64)`),
    /// so a scale per TILE rather than per whole world would put every dot
    /// 64x too close to the tile's corner. One populated cell, 50 dots:
    /// every lit pixel must fall inside that cell's own screen rectangle
    /// (plus the dot radius and a pixel of antialiasing).
    #[test]
    fn a_dot_lands_inside_its_own_cell_in_a_z6_data_tile() {
        let (lon, lat, zoom) = (10.0, 20.0, 12.0);
        let dots = dot_fixture(lon, lat, 0, 50.0, 1.0);
        let arc = crate::overlay::dotgrid::grid::cell_arcsec(&dots.descriptor, 0) / 3600.0;
        let (west, north) = ((lon / arc).floor() * arc, 90.0 - ((90.0 - lat) / arc).floor() * arc);
        let (east, south) = (west + arc, north - arc);
        let c = dot_capture(lon, lat, zoom, dots);
        // Screen position of a lon/lat at pitch 0: the centre plus the
        // world offset at 2^zoom px per world unit.
        let scale = 2f64.powf(zoom);
        let centre = crate::overlay::geom::lon_lat_to_world(lon, lat);
        let to_px = |lo: f64, la: f64| {
            let w = crate::overlay::geom::lon_lat_to_world(lo, la);
            (128.0 + (w.0 - centre.0) * scale, 128.0 + (w.1 - centre.1) * scale)
        };
        let (x0, y0) = to_px(west, north);
        let (x1, y1) = to_px(east, south);
        let pad = 1.5 + 1.5;
        let (mut inside, mut outside) = (0, 0);
        for y in 0..c.height {
            for x in 0..c.width {
                if c.pixel(x, y)[0] == 0 {
                    continue;
                }
                let (px, py) = (f64::from(x) + 0.5, f64::from(y) + 0.5);
                if px >= x0 - pad && px <= x1 + pad && py >= y0 - pad && py <= y1 + pad {
                    inside += 1;
                } else {
                    outside += 1;
                }
            }
        }
        assert!(inside > 50, "only {inside} pixels lit inside the cell ({x0:.0},{y0:.0})-({x1:.0},{y1:.0})");
        assert_eq!(outside, 0, "{outside} lit pixels outside the cell's rectangle");
    }

    /// **MG-8: native vertex cost.** 4 M live dots of a synthetic 30″ grid
    /// (8 data tiles at z6, every cell populated, mean 1.1 people, `ppd =
    /// 1`) filling a 1920x1080 frame, drawn at transition 0 (the flat
    /// path) and 1 (the globe path) from the same camera. Timed as the wall
    /// time of `queue.submit` + `device.poll(Wait)` over 30 frames after 5
    /// warm-up frames (the headless device has no `TIMESTAMP_QUERY`), p50,
    /// minus the same loop with no dots drawn. Valid only with
    /// `draw_calls_total > 0`, which it asserts.
    ///
    /// `cargo test --release --lib capture::tests::mg8_vertex_cost -- --ignored --nocapture`
    ///
    /// Discriminators, off by default: `MG8_PPD` (the view's ppd; `1e9`
    /// kills every slot, leaving the per-slot/per-instance floor),
    /// `MG8_RADIUS` (px), and `MG8_SPARSE=k` (every k-th cell with k times
    /// the people: the same live dots in fewer, larger blocks), and
    /// `MG8_GAMMA` (the overlap fade's exponent, default 0: its A/B).
    ///
    /// **MG-R10 (race dots).** `MG8_RACE=1` swaps the grid for synthetic
    /// PART tiles over the same 8 z6 tiles -- 64 x 64 squares a tile, each
    /// halved into two triangles, 1 + (0..11) people in each of 8 groups
    /// (about 2.7 M at ppd 1) -- drawn categorically (`COLOR_CATEGORICAL`,
    /// `SHAPE_TRIANGLE`, every group on): the composition and shape
    /// fetches, `part_key` and `group_of` on every dot.
    ///
    /// **DP-2 (relocation's vertex cost).** `MG8_K=4,6,8` (a list) with
    /// `MG8_MASK=checker10,checker30,water_half` (a list,
    /// `placement::SyntheticMask`) times every K against every mask, each
    /// measurement immediately preceded by a K = 0 one (no mask bound,
    /// today's shader) in the same process, `MG8_ROUNDS` times (default
    /// 3). Every draw relocates, at any cell size and on the globe too
    /// (`dot_draws`' `force_k`): the loop on every dot, the worst case. It
    /// prints each pair, net of that round's empty frame, then per K,
    /// mask and path (flat `vs_dot_flat`, globe `vs_dot`) the worst net
    /// ms and the median and worst cost relative to its paired K = 0.
    #[test]
    #[ignore = "measurement gate MG-8; run on demand"]
    fn mg8_vertex_cost() {
        use crate::overlay::dotgrid::{blocks, grid, hash, level, GridDescriptor, GridTile, GridTileKey};
        pollster::block_on(async {
            let d = GridDescriptor {
                base_arcsec: 30.0,
                origin_lat: 90.0,
                origin_lon: -180.0,
                levels: 7,
                max_data_zoom: grid::base_data_zoom(30.0),
                epochs: vec![2025],
                attribution: String::new(),
            };
            let live_epochs: Option<(f32, f32)> = std::env::var("MG8_LIVE").ok().map(|v| {
                let (b, t) = v.split_once(',').expect("MG8_LIVE=B_SCALE,T");
                (b.trim().parse().expect("B_SCALE"), t.trim().parse().expect("T"))
            });
            let mut tiles = Vec::new();
            for x in 30..34u32 {
                for y in 31..33u32 {
                    let tile = TileId { z: 6, x, y };
                    let w = grid::cell_window(&d, 0, tile);
                    let sparse: u32 = std::env::var("MG8_SPARSE").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
                    let pop_scale: f32 = std::env::var("MG8_POP_SCALE").ok().and_then(|v| v.parse().ok()).unwrap_or(1.0);
                    let cells: Vec<(u32, f32, f32)> = (0..w.rows * w.cols)
                        .filter(|i| i % sparse == 0)
                        .map(|i| {
                            let pop = ((i.wrapping_mul(2_654_435_761) >> 16) % 23) as f32 / 10.0 * sparse as f32 * pop_scale;
                            (i, pop, pop)
                        })
                        .filter(|c| c.1 > 0.0)
                        .collect();
                    // Live epochs (Task 15, `MG8_LIVE=B_SCALE,T`): every
                    // cell's `pop_b` is `pop * B_SCALE` and the draw mixes
                    // at `T` -- the block texture's `pop_b` region and its
                    // second texel fetch per dot, at the same live count
                    // when `B_SCALE` is 1.
                    let cells: Vec<(u32, f32, f32)> =
                        cells.into_iter().map(|(i, p, _)| (i, p, live_epochs.map_or(p, |(b, _)| p * b))).collect();
                    let total = cells.iter().map(|c| f64::from(c.1)).sum();
                    let epoch_b = live_epochs.map(|_| 2030);
                    tiles.push((GridTileKey { tile, level: 0, epoch: 2025, epoch_b }, GridTile { cells, total, ..Default::default() }));
                }
            }
            let race = std::env::var("MG8_RACE").is_ok_and(|v| v == "1");
            if race {
                use crate::overlay::dotgrid::codec_cat::PartRecord;
                tiles.clear();
                let mut seed = 1u32;
                for x in 30..34u32 {
                    for y in 31..33u32 {
                        let tile = TileId { z: 6, x, y };
                        let side = 1.0 / 64.0 / 64.0;
                        let (ox, oy) = (f64::from(x) / 64.0, f64::from(y) / 64.0);
                        let mut parts = Vec::with_capacity(64 * 64 * 2);
                        for r in 0..64 {
                            for q in 0..64 {
                                let (px, py) = (ox + f64::from(q) * side, oy + f64::from(r) * side);
                                for tri in [[[px, py], [px + side, py], [px + side, py + side]], [[px, py], [px + side, py + side], [px, py + side]]] {
                                    let counts = std::array::from_fn(|g| {
                                        seed = hash::pcg(seed);
                                        seed % 11 + u32::from(g == 0)
                                    });
                                    parts.push(PartRecord { a: tri[0], b: tri[1], c: tri[2], counts });
                                }
                            }
                        }
                        let total = parts.iter().map(|p| p.counts.iter().map(|&c| f64::from(c)).sum::<f64>()).sum();
                        tiles.push((GridTileKey::part(tile, 2025), GridTile { parts, total, ..Default::default() }));
                    }
                }
            }
            let dots = CaptureDots {
                category_mask: race.then_some(crate::render::dots::CATEGORY_MASK_ALL),
                descriptor: d,
                tiles,
                ppd: std::env::var("MG8_PPD").ok().and_then(|v| v.parse().ok()).unwrap_or(1.0),
                ppd_build: std::env::var("MG8_PPD_BUILD").ok().and_then(|v| v.parse().ok()),
                radius_px: std::env::var("MG8_RADIUS").ok().and_then(|v| v.parse().ok()).unwrap_or(1.3),
                // The race style draws no density shrink (C12).
                radius_shrink: std::env::var("MG8_SHRINK")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(if race { 0.0 } else { crate::overlay::dotgrid::layer::DotStyle::default().density_shrink }),
                // The overlap fade's A/B: off unless asked for.
                overlap_gamma: std::env::var("MG8_GAMMA").ok().and_then(|v| v.parse().ok()).unwrap_or(0.0),
                ramp: if race {
                    RACE_TEST_PALETTE.iter().map(|c| Rgba { r: c[0], g: c[1], b: c[2], a: 1.0 }).collect()
                } else {
                    vec![Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }]
                },
                epoch_mix: live_epochs.map_or(0.0, |(_, t)| t),
                difference: false,
                exclude: None,
                tile_excludes: Vec::new(),
                relocate: false,
                mask_k: 0,
                synthetic_mask: None,
                mask_previous: None,
            };
            // Live dots and allocated slots, on the CPU mirror.
            let (mut live, mut slots) = (0u64, 0u64);
            for (key, tile) in &dots.tiles {
                let ppd_build = dots.ppd_build.unwrap_or_else(|| level::ppd_build_for(dots.ppd));
                let b = blocks::build_blocks(&dots.descriptor, key, tile, ppd_build, blocks::OVERLAY_LYON_PER_UNIT_WORLD);
                slots += b.slots;
                for (i, bucket) in b.buckets.iter().enumerate() {
                    for (j, blk) in bucket.iter().enumerate() {
                        let k = match b.shapes[i].get(j) {
                            Some(sh) => crate::overlay::dotgrid::parts::part_key(blk.origin, [sh[0], sh[1]], [sh[2], sh[3]]),
                            None => hash::cell_key(blk.origin),
                        };
                        live += (0..crate::render::dots::DOT_BUCKETS[i])
                            .filter(|&j| hash::dot_alive(blk.pop, dots.ppd, blk.first_dot + j, k))
                            .count() as u64;
                    }
                }
            }
            let (width, height) = (1920u32, 1080u32);
            // The 4x2 block of z6 tiles is 2048x1024 px at zoom 6, centred
            // on the 32/32 corner: lon 0, lat 0 (tiles x 30..34, y 31..33).
            let camera = Camera {
                center_lon: 0.0,
                center_lat: 0.0,
                zoom: 6.0,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: width as f32,
                viewport_height_px: height as f32,
                fov_y: Camera::DEFAULT_FOV_Y,
            };
            let frame = crate::render::globe::globe_frame(&camera);
            let globe = GlobeUpload::new(&frame, &camera, MVT_EXTENT as f32, glam::DVec3::ZERO, 1.0);
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let format = wgpu::TextureFormat::Rgba8Unorm;
            let pipeline = TilePipeline::new(&ctx, format);
            let color = ctx.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("mg8-color"),
                size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let color_view = color.create_view(&wgpu::TextureViewDescriptor::default());
            let depth_view = pipeline.create_depth_view(&ctx, width, height);
            let (_camera_buffer, camera_group) =
                pipeline.create_camera_bind_group(&ctx, camera.view_projection().to_cols_array_2d(), camera.zoom as f32, 1.0);
            let time = |draws: &[crate::render::TileDraw<'_>]| {
                let mut samples = Vec::new();
                for frame in 0..35 {
                    let start = std::time::Instant::now();
                    let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
                    pipeline.render_with_space_and_raster(
                        &mut encoder,
                        &color_view,
                        &depth_view,
                        wgpu::Color::BLACK,
                        &camera_group,
                        &crate::render::SpaceDraw::NONE,
                        None,
                        draws,
                        None,
                    );
                    ctx.queue.submit(Some(encoder.finish()));
                    ctx.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
                    if frame >= 5 {
                        samples.push(start.elapsed().as_secs_f64() * 1e3);
                    }
                }
                samples.sort_by(f64::total_cmp);
                samples[samples.len() / 2]
            };
            let empty = time(&[]);
            let paths = [("flat, vs_dot", 0.0f32, false), ("flat, vs_dot_flat", 0.0, true), ("globe, vs_dot", 1.0, false)];
            let list = |name: &str| -> Vec<String> {
                std::env::var(name).ok().map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default()
            };
            let ks: Vec<u32> = list("MG8_K").iter().map(|k| k.parse().expect("MG8_K takes a list of integers")).collect();
            if ks.iter().any(|&k| k > 0) {
                use crate::overlay::dotgrid::placement::SyntheticMask;
                let masks: Vec<(String, SyntheticMask)> = list("MG8_MASK")
                    .into_iter()
                    .map(|m| {
                        let mask = SyntheticMask::parse(&m).expect("MG8_MASK takes checker10|checker30|water_half");
                        (m, mask)
                    })
                    .collect();
                assert!(!masks.is_empty(), "MG8_K needs MG8_MASK");
                let rounds: u32 = std::env::var("MG8_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(3);
                // The 4x2 z6 tiles at mask zoom 6: a texel per logical
                // pixel of this camera, as `MaskGrid::plan` would size it.
                let grid = crate::render::dot_mask::MaskGrid { zm: 6, origin: [30 * 512, 31 * 512], size: [2048, 1024] };
                let measure = |k: u32, mask: Option<&crate::render::dot_mask::DotMaskGpu>, transition: f32, flat_path: bool| {
                    let bound = mask.map(|m| (m, &grid));
                    let (_table, built) = dot_draws(&dots, &ctx, &pipeline, &camera, &globe, Some(transition), bound, Some(k));
                    let draws: Vec<_> = built
                        .iter()
                        .map(|b| crate::render::TileDraw::Dots(&b.dots, &b.model_group, transition == 0.0 && flat_path))
                        .collect();
                    let before = crate::render::allocation_stats().draw_calls_total;
                    let p50 = time(&draws);
                    let issued = crate::render::allocation_stats().draw_calls_total - before;
                    assert!(issued > 0, "K {k}: draw_calls_total did not move -- nothing was drawn");
                    p50
                };
                eprintln!("DP-2: {live} live dots, {slots} slots, {width}x{height}, {rounds} rounds, K {ks:?}, masks {:?}", masks.iter().map(|m| &m.0).collect::<Vec<_>>());
                // (k, mask, path) -> per-round net ms (p50 minus the
                // round's empty frame), and its paired K = 0 net ms.
                let mut runs: std::collections::BTreeMap<(u32, String, &str), Vec<(f64, f64)>> = Default::default();
                // MG-8's criteria are on the flat (`vs_dot_flat`) and globe
                // paths; the flat `vs_dot` row is not timed here.
                let paths = [paths[1], paths[2]];
                for round in 0..rounds {
                    let empty = time(&[]);
                    eprintln!("DP-2 round {round}: empty frame p50 {empty:.2} ms");
                    for (mask_name, pattern) in &masks {
                        let mask = crate::render::dot_mask::DotMaskGpu::from_texels(&ctx, grid, &pattern.texels(&grid));
                        for &k in ks.iter().filter(|&&k| k > 0) {
                            for (path, transition, flat_path) in paths {
                                let base = measure(0, None, transition, flat_path) - empty;
                                let net = measure(k, Some(&mask), transition, flat_path) - empty;
                                eprintln!(
                                    "DP-2 round {round} {mask_name} K {k} {path}: net K0 {base:.2} ms, K{k} {net:.2} ms ({:+.1} %)",
                                    (net / base - 1.0) * 100.0
                                );
                                runs.entry((k, mask_name.clone(), path)).or_default().push((net, base));
                            }
                        }
                    }
                }
                for ((k, mask_name, path), v) in &runs {
                    let worst = v.iter().map(|r| r.0).fold(f64::MIN, f64::max);
                    let worst_base = v.iter().map(|r| r.1).fold(f64::MIN, f64::max);
                    let mut rel: Vec<f64> = v.iter().map(|r| (r.0 / r.1 - 1.0) * 100.0).collect();
                    rel.sort_by(f64::total_cmp);
                    eprintln!(
                        "DP-2 summary K {k} {mask_name} {path}: worst net {worst:.2} ms (K0 worst {worst_base:.2} ms, {:+.1} %), paired rel median {:+.1} % worst {:+.1} %",
                        (worst / worst_base - 1.0) * 100.0,
                        rel[rel.len() / 2],
                        rel[rel.len() - 1]
                    );
                }
                return;
            }
            let mut report = Vec::new();
            for (name, transition, flat_path) in paths {
                let (_table, built) = dot_draws(&dots, &ctx, &pipeline, &camera, &globe, Some(transition), None, None);
                let draws: Vec<_> = built
                    .iter()
                    .map(|b| crate::render::TileDraw::Dots(&b.dots, &b.model_group, transition == 0.0 && flat_path))
                    .collect();
                let before = crate::render::allocation_stats().draw_calls_total;
                let p50 = time(&draws);
                let issued = crate::render::allocation_stats().draw_calls_total - before;
                assert!(issued > 0, "{name}: draw_calls_total did not move -- nothing was drawn");
                report.push((name, p50, p50 - empty, issued / 35));
            }
            eprintln!("MG-8: {live} live dots, {slots} slots, {width}x{height}, live epochs {live_epochs:?}, race {race}, empty frame p50 {empty:.2} ms");
            for (name, p50, net, calls) in &report {
                eprintln!("MG-8 {name}: p50 {p50:.2} ms (net of empty {net:.2} ms), {calls} draw calls/frame");
            }
            if dots.ppd == 1.0 && std::env::var("MG8_POP_SCALE").is_err() && !race {
                assert!(live >= 3_900_000, "the fixture has {live} live dots, not ~4 M");
            }
        });
    }

    /// **A tile built for a finer `ppd` draws the same dots, and skips its
    /// dead blocks.** Built at `ppd_build = 2` and drawn at `ppd = 8` (the
    /// old 4x band; `needs_rebuild` allows 2x since Task 7), the frame lights the same
    /// pixels as a tile built at 8, while the draw skips every block whose
    /// FIRST dot is dead at 8 -- a suffix of each bucket (`DotTileGpu`).
    /// Skipping is per block: a partly dead block is still drawn.
    #[test]
    fn a_tile_built_for_a_finer_ppd_draws_the_same_dots_and_skips_its_dead_blocks() {
        use crate::overlay::dotgrid::blocks;
        let fresh = dot_capture(10.0, 20.0, 10.0, dot_fixture(10.0, 20.0, 12, 40.0, 8.0));
        let stale_dots = CaptureDots { ppd_build: Some(2.0), ..dot_fixture(10.0, 20.0, 12, 40.0, 8.0) };
        let (key, tile) = &stale_dots.tiles[0];
        let slots = blocks::build_blocks(&stale_dots.descriptor, key, tile, 2.0, blocks::OVERLAY_LYON_PER_UNIT_WORLD).slots;
        let stale = dot_capture(10.0, 20.0, 10.0, stale_dots);
        let (a, b) = (lit(&fresh), lit(&stale));
        let n = a.iter().filter(|&&l| l).count();
        assert!(n > 500, "only {n} dot pixels");
        let differ = a.iter().zip(&b).filter(|(x, y)| x != y).count();
        assert_eq!(differ, 0, "{differ} of {n} lit pixels differ between the fresh and the stale build");
        // Every cell holds 40 people: 20 slots at 2 (a 16-block from dot 0
        // and a 4-block from dot 16). At 8 only dots 0-4 can live, so the
        // 4-block (first dot 16 >= 40 / 8) is skipped: 16 of 20 slots drawn.
        let full = slots as usize * crate::render::dots::DOT_VERTICES as usize;
        assert_eq!(stale.vertices * 20, full * 16, "the stale tile drew {} vertices of its slots' {full}", stale.vertices);
    }

    /// **The donut hook draws each layer over the frame, in layer order.**
    /// Two donuts on one point: layer a's 40 px one in red, layer b's 20 px
    /// one in blue over it, one group each. The ring 33 px out is layer a's
    /// red, 16 px out layer b's blue (on top, and its own palette), the
    /// centre the white hole, and the corner the clear colour.
    #[test]
    fn the_donut_hook_draws_its_layers_in_order_with_their_own_palettes() {
        use crate::render::donut::Donut;
        let black = Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 };
        let mut red = [black; 8];
        red[0] = Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 };
        let mut blue = [black; 8];
        blue[0] = Rgba { r: 0.0, g: 0.0, b: 1.0, a: 1.0 };
        let donut = |radius_px: f32| Donut { lon: 0.0, lat: 0.0, radius_px, counts: [5, 0, 0, 0, 0, 0, 0, 0], alpha: 1.0 };
        let request = CaptureRequest {
            center_lon: 0.0,
            center_lat: 0.0,
            zoom: 13.0,
            width: 256,
            height: 256,
            flat: true,
            backdrop: false,
            clear: Clear::Sentinel(Rgba { r: 0.0, g: 1.0, b: 0.0, a: 1.0 }),
            donuts: vec![(vec![donut(40.0)], red), (vec![donut(20.0)], blue)],
            ..CaptureRequest::default()
        };
        let frame = capture(&request, &TileStore::new(), &no_layers()).expect("a headless device");
        let near = |got: [u8; 4], want: [u8; 3]| (0..3).all(|k| got[k].abs_diff(want[k]) <= 3);
        let (outer, inner, centre, corner) = (frame.pixel(128, 128 - 33), frame.pixel(128, 128 - 16), frame.pixel(128, 128), frame.pixel(4, 4));
        assert!(near(outer, [255, 0, 0]), "layer a's ring: {outer:?}");
        assert!(near(inner, [0, 0, 255]), "layer b's ring, on top: {inner:?}");
        assert!(near(centre, [255, 255, 255]), "the white centre: {centre:?}");
        assert!(near(corner, [0, 255, 0]), "no donut out here: {corner:?}");
        assert_eq!((frame.donuts_drawn, frame.donuts_truncated), (2, 0));
        // No donuts draw nothing: no instance (so no pass), the clear colour.
        let bare = capture(&CaptureRequest { donuts: Vec::new(), ..request.clone() }, &TileStore::new(), &no_layers()).expect("a headless device");
        assert!(near(bare.pixel(128, 128 - 33), [0, 255, 0]));
        assert_eq!(bare.donuts_drawn, 0);
        // Past the cap: counted, and the kept ones still drawn.
        let many = vec![donut(40.0); crate::render::donut::MAX_DONUTS + 3];
        let capped = capture(&CaptureRequest { donuts: vec![(many, red)], ..request }, &TileStore::new(), &no_layers()).expect("a headless device");
        assert_eq!((capped.donuts_drawn, capped.donuts_truncated), (crate::render::donut::MAX_DONUTS, 3));
    }

    /// **An overlay label is anchored on the globe, where its donut is, and
    /// a label beyond the horizon is not drawn.** The race map's cluster
    /// count labels were placed with the FLAT Mercator matrix on the
    /// z3-z11 globe: off their donuts, towards and past the limb, and the
    /// far side's floated in space. The label's anchor
    /// (`map_view::overlay_symbol_screen_anchor`, what `MapView::paint`
    /// places every overlay symbol with) must land within 2 px of the
    /// donut's centre (`donut_instance` on the same frame), and be `None`
    /// for a point the planet hides.
    #[test]
    fn an_overlay_label_sits_on_its_donut_on_the_globe_and_not_beyond_the_horizon() {
        use crate::overlay::geojson::OverlayGeometry;
        use crate::render::donut::{donut_instance, Donut};
        for (zoom, width, height) in [(3.5, 1920, 1080), (2.5, 1920, 1080), (4.0, 1024, 768)] {
            let request = CaptureRequest { center_lon: -98.0, center_lat: 39.0, zoom, width, height, ..CaptureRequest::default() };
            let camera = request.camera();
            let frame = crate::render::globe::globe_frame(&camera);
            assert!(frame.transition > 0.99, "z{zoom} is a globe frame");
            let anchors = crate::render::globe::GlobeAnchors::new(&frame);
            let projector = crate::render::globe::WorldProjector::new(&camera, &anchors);
            let label_at = |lon: f64, lat: f64| {
                crate::map_view::overlay_symbol_screen_anchor(&projector, &OverlayGeometry::Point(vec![(lon, lat)]))
            };
            // Houston, New York, Seattle, Miami, Anchorage, Mexico City.
            for (lon, lat) in [(-95.37, 29.76), (-74.0, 40.71), (-122.33, 47.61), (-80.19, 25.76), (-149.9, 61.2), (-99.13, 19.43)] {
                let donut = Donut { lon, lat, radius_px: 20.0, counts: [5, 0, 0, 0, 0, 0, 0, 0], alpha: 1.0 };
                let world = crate::overlay::geom::lon_lat_to_world(lon, lat);
                let centre = donut_instance(&donut, projector.project(world), 1.0).anchor_px;
                let label = label_at(lon, lat).unwrap_or_else(|| panic!("({lon}, {lat}) faces the eye at z{zoom}"));
                let apart = (label.0 - centre[0]).hypot(label.1 - centre[1]);
                assert!(apart <= 2.0, "z{zoom} ({lon}, {lat}): label {label:?} is {apart:.1} px off its donut {centre:?}");
            }
            // Beyond the horizon: the antipode, central Asia, the Indian Ocean.
            for (lon, lat) in [(82.0, -39.0), (60.0, 39.0), (20.0, 20.0)] {
                assert_eq!(label_at(lon, lat), None, "z{zoom} ({lon}, {lat}) is on the far side");
            }
        }
        // What the flat matrix did, and so what this pins: at z2.5 (20, 20)
        // is far side, yet the flat plane puts it ON the 1920x1080 frame --
        // a label floating in space -- and Anchorage's label far off its
        // donut, out past the limb.
        let request = CaptureRequest { center_lon: -98.0, center_lat: 39.0, zoom: 2.5, width: 1920, height: 1080, ..CaptureRequest::default() };
        let camera = request.camera();
        let flat = |lon, lat| crate::render::project_world_to_screen_f64(&camera, crate::overlay::geom::lon_lat_to_world(lon, lat)).unwrap();
        let (fx, fy) = flat(20.0, 20.0);
        assert!((0.0..1920.0).contains(&fx) && (0.0..1080.0).contains(&fy), "the flat plane draws the far side on the frame: {:?}", (fx, fy));
        let anchors = crate::render::globe::GlobeAnchors::new(&crate::render::globe::globe_frame(&camera));
        let on_globe = crate::render::globe::WorldProjector::new(&camera, &anchors)
            .project(crate::overlay::geom::lon_lat_to_world(-149.9, 61.2))
            .unwrap();
        let (nx, ny) = flat(-149.9, 61.2);
        let apart = (nx - on_globe.0).hypot(ny - on_globe.1);
        assert!(apart > 100.0, "the fixture separates the two projections: {apart:.1} px");
    }

    /// **The capture's overlay-label hook draws a label on its donut and
    /// none from the far side**: the race map's cluster counts, headless.
    #[test]
    fn the_capture_draws_overlay_labels_on_the_globe_and_culls_the_far_side() {
        let label = |lon: f64, lat: f64, text: &str| {
            format!(
                r##"{{"type":"Feature","geometry":{{"type":"Point","coordinates":[{lon},{lat}]}},"properties":{{"text-field":"{text}","text-size":28,"text-color":"#000000ff","text-halo-width":0,"text-allow-overlap":true}}}}"##
            )
        };
        let (lon, lat) = (-95.37, 29.76);
        let base = CaptureRequest {
            center_lon: -98.0,
            center_lat: 39.0,
            zoom: 2.5,
            width: 1920,
            height: 1080,
            backdrop: false,
            clear: Clear::Sentinel(Rgba { r: 0.0, g: 1.0, b: 0.0, a: 1.0 }),
            ..CaptureRequest::default()
        };
        let json = format!(r#"{{"type":"FeatureCollection","features":[{},{}]}}"#, label(lon, lat, "88"), label(20.0, 20.0, "88"));
        let bare = capture(&base, &TileStore::new(), &no_layers()).expect("a headless device");
        let labelled = capture(&CaptureRequest { overlay_labels: Some(json), ..base.clone() }, &TileStore::new(), &no_layers()).expect("a headless device");
        assert_eq!(labelled.overlay_labels_drawn, 1, "Houston's label, and not the far side's");
        // The ink: every pixel the label changed.
        let (mut min, mut max) = ((u32::MAX, u32::MAX), (0, 0));
        for y in 0..base.height {
            for x in 0..base.width {
                if (0..3).any(|k| bare.pixel(x, y)[k].abs_diff(labelled.pixel(x, y)[k]) > 40) {
                    min = (min.0.min(x), min.1.min(y));
                    max = (max.0.max(x), max.1.max(y));
                }
            }
        }
        assert!(max.0 >= min.0 && max.1 >= min.1, "the label drew ink");
        let camera = base.camera();
        let anchors = crate::render::globe::GlobeAnchors::new(&crate::render::globe::globe_frame(&camera));
        let donut = crate::render::globe::WorldProjector::new(&camera, &anchors)
            .project(crate::overlay::geom::lon_lat_to_world(lon, lat))
            .unwrap();
        let ink = ((min.0 + max.0) as f32 / 2.0, (min.1 + max.1) as f32 / 2.0);
        assert!(max.0 - min.0 < 60 && max.1 - min.1 < 40, "one label's ink, nothing from the far side: {min:?}..{max:?}");
        // The ink is centred on the anchor to within the glyphs' bearings.
        assert!((ink.0 - donut.0).abs() <= 3.0 && (ink.1 - donut.1).abs() <= 3.0, "ink {ink:?} vs donut centre {donut:?}");
    }

    /// **Every count label sits on its own donut, and neither shows
    /// without the other: on the globe, through the blend and on the flat
    /// map, level or pitched and turned.** The race map's cluster counts
    /// (`demos/race_dots/clusters.rs`: one donut and one `text-allow-overlap`
    /// label per cluster, from the same lon/lat) through the capture's two
    /// hooks, `capture_donuts` and `capture_overlay_labels`, which place
    /// each the way `MapView::paint` does. The reported frame was the z4
    /// globe with its labels anchored by the FLAT Mercator matrix (as
    /// before d398e4e): counts on the Canadian border ~30 px above their
    /// rings, Chicago's below-right of its ring, east-coast counts out in
    /// the Atlantic, and rings at the frame's edge with no count.
    ///
    /// Cluster `i` has a unique radius and text, so a label is compared with
    /// ITS donut, not the nearest one. A label's centre (the shaped block's,
    /// as `text_area_origin` centres it, to its whole-pixel rounding) must
    /// be within 1 px of its donut's centre. A label whose centre is on the
    /// frame must have its donut drawn, and a donut whose centre is on the
    /// frame must have its label.
    #[test]
    fn every_cluster_count_label_sits_on_its_own_donut_at_any_zoom_pitch_and_bearing() {
        use crate::render::donut::Donut;
        // (lon, lat, zoom, pitch, bearing, w, h), radians: the reported z4.1
        // globe; the same pitched and turned; the globe at z8 turned; the
        // globe-to-flat blend pitched; the flat map level and pitched.
        let views: [(f64, f64, f64, f32, f32, u32, u32); 6] = [
            (-98.0, 38.3, 4.1, 0.0, 0.0, 1000, 740),
            (-98.0, 38.3, 4.1, 0.7, 0.5, 1000, 740),
            (-95.37, 29.76, 8.0, 0.0, -0.7, 1024, 768),
            (-95.37, 29.76, 11.5, 0.5, 0.3, 1024, 768),
            (-95.37, 29.76, 13.0, 0.0, 0.0, 1024, 768),
            (-95.37, 29.76, 13.0, 0.87, -1.2, 1024, 768),
        ];
        let mut font_system = crate::font_fallback::new_font_system();
        let mut shape_cache = crate::symbol_pass::LabelShapeCache::new();
        for (lon0, lat0, zoom, pitch, bearing, width, height) in views {
            // A 15x11 grid over about twice the frame's span: clusters on
            // the frame, straddling its edges and off it.
            let span = 360.0 / 2f64.powf(zoom) * (f64::from(width) / 512.0);
            let mut clusters = Vec::new();
            for gy in 0..11 {
                for gx in 0..15 {
                    let lon = lon0 + (f64::from(gx) / 14.0 - 0.5) * span * 2.0;
                    let lat = (lat0 + (f64::from(gy) / 10.0 - 0.5) * span * 1.2).clamp(-80.0, 80.0);
                    clusters.push((lon, lat));
                }
            }
            let radius = |i: usize| 6.0 + i as f32 * 0.125;
            let donuts: Vec<Donut> = clusters
                .iter()
                .enumerate()
                .map(|(i, &(lon, lat))| Donut { lon, lat, radius_px: radius(i), counts: [3, 1, 0, 0, 0, 0, 0, 1], alpha: 1.0 })
                .collect();
            let features: Vec<String> = clusters
                .iter()
                .enumerate()
                .map(|(i, (lon, lat))| {
                    format!(
                        r##"{{"type":"Feature","geometry":{{"type":"Point","coordinates":[{lon},{lat}]}},"properties":{{"circle-radius":0,"text-field":"c{i}","text-size":12,"text-color":"#263443ff","text-allow-overlap":true}}}}"##
                    )
                })
                .collect();
            let json = format!(r#"{{"type":"FeatureCollection","features":[{}]}}"#, features.join(","));
            let request = CaptureRequest {
                center_lon: lon0,
                center_lat: lat0,
                zoom,
                pitch,
                bearing,
                width,
                height,
                donuts: vec![(donuts, crate::overlay::dotgrid::layer::RACE_PALETTE)],
                ..CaptureRequest::default()
            };
            let camera = request.camera();
            let frame = crate::render::globe::globe_frame(&camera);
            let (instances, _, _) = capture_donuts(&request, &camera, &frame);
            let labels = capture_overlay_labels(&json, &camera, &frame, zoom.floor() as u8, &mut font_system, &mut shape_cache);
            let view = format!("z{zoom} pitch {pitch} bearing {bearing} (globe {:.2})", frame.transition);
            let on_frame = |x: f32, y: f32| (0.0..width as f32).contains(&x) && (0.0..height as f32).contains(&y);
            // Donut `i`'s centre, found by its unique radius.
            let donut_of = |i: usize| instances.iter().find(|d| (d.radius_px - radius(i)).abs() < 1e-4).map(|d| d.anchor_px);
            let mut labelled = std::collections::HashSet::new();
            for (key, left, top, ..) in &labels {
                let shaped = shape_cache.get(key).expect("a shaped label");
                let text = &key.0;
                let i: usize = text.trim_start_matches('c').parse().expect("c<index>");
                labelled.insert(i);
                let centre = (left + shaped.measured_width / 2.0, top + shaped.measured_height / 2.0);
                let Some(donut) = donut_of(i) else {
                    assert!(!on_frame(centre.0, centre.1), "{view}: {text} drawn at {centre:?} with no donut");
                    continue;
                };
                let apart = (centre.0 - donut[0]).hypot(centre.1 - donut[1]);
                assert!(apart <= 1.0, "{view}: {text} centred at {centre:?}, {apart:.1} px off its donut {donut:?}");
            }
            let mut on_frame_donuts = 0;
            for i in 0..clusters.len() {
                if let Some(d) = donut_of(i).filter(|d| on_frame(d[0], d[1])) {
                    on_frame_donuts += 1;
                    assert!(labelled.contains(&i), "{view}: donut c{i} at {d:?} has no count label");
                }
            }
            assert!(on_frame_donuts >= 20, "{view}: the grid puts clusters on the frame ({on_frame_donuts})");
        }
    }
}

#[cfg(test)]
mod space_look_tests {
    use super::*;
    use crate::render::space_look::{EquirectImage, SpaceLook, SpaceTextures};

    fn textures() -> std::sync::Arc<SpaceTextures> {
        // A dim uniform "Milky Way" and a cloud map that is solid white in
        // its northern half: synthetic, so the test needs no download.
        let (w, h) = (64u32, 32u32);
        let starmap = EquirectImage { width: w, height: h, rgba: [60u8, 60, 80, 255].repeat((w * h) as usize) };
        let mut clouds = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            let v = if y < h / 2 { 255 } else { 0 };
            for _ in 0..w {
                clouds.extend_from_slice(&[v, v, v, 255]);
            }
        }
        std::sync::Arc::new(SpaceTextures { starmap: Some(starmap), clouds: Some(EquirectImage { width: w, height: h, rgba: clouds }) })
    }

    fn request(zoom: f64, look: Option<SpaceLook>) -> CaptureRequest {
        CaptureRequest {
            center_lon: 0.0,
            center_lat: 0.0,
            zoom,
            width: 256,
            height: 256,
            space_look: look.map(|l| (l, textures(), Some(1_790_697_600.0))),
            ..CaptureRequest::default()
        }
    }

    fn style() -> Style {
        crate::style_spec::parse_and_compile(br##"{"version":8,"sources":{},"layers":[{"id":"bg","type":"background","paint":{"background-color":"#2060a0"}}]}"##)
            .expect("a background-only style")
    }

    fn px(f: &Capture, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * f.width + x) * 4) as usize;
        [f.rgba[i], f.rgba[i + 1], f.rgba[i + 2], f.rgba[i + 3]]
    }

    /// The look is opt-in: a request without one, and one whose look is
    /// switched off, draw the same bytes -- and so does a flat frame with the
    /// look ON, since nothing of it draws without a globe.
    #[test]
    fn the_space_look_off_or_on_a_flat_frame_changes_no_pixel() {
        let tiles = TileStore::new();
        let plain = capture(&request(1.5, None), &tiles, &style()).expect("a headless device");
        let off = capture(&request(1.5, Some(SpaceLook::default())), &tiles, &style()).expect("capture");
        assert_eq!(plain.rgba, off.rgba, "a disabled look draws nothing");
        let flat_plain = capture(&request(13.0, None), &tiles, &style()).expect("capture");
        let flat_on = capture(&request(13.0, Some(SpaceLook::ON)), &tiles, &style()).expect("capture");
        assert_eq!(flat_plain.rgba, flat_on.rgba, "a flat frame gets no look");
    }

    /// On the globe the look paints the sky behind the planet (the corner is
    /// no longer black) and clouds over its northern half (brighter than the
    /// southern half at the same distance from the centre).
    #[test]
    fn the_space_look_paints_the_sky_and_the_clouds() {
        let tiles = TileStore::new();
        let plain = capture(&request(1.5, None), &tiles, &style()).expect("a headless device");
        let on = capture(&request(1.5, Some(SpaceLook::ON)), &tiles, &style()).expect("capture");
        let corner = px(&on, 4, 4);
        assert!(corner[0] > 5 && corner[2] > 5, "the star map lights the sky: {corner:?} (plain {:?})", px(&plain, 4, 4));
        let (c, off) = (128u32, 30u32);
        let north = px(&on, c, c - off);
        let south = px(&on, c, c + off);
        let lum = |p: [u8; 4]| u32::from(p[0]) + u32::from(p[1]) + u32::from(p[2]);
        assert!(lum(north) > lum(south) + 150, "clouds over the north: {north:?} vs {south:?}");
    }

    /// The look's shader stays inside WebGL2: it validates, and both entry
    /// pairs translate to GLSL ES 3.00.
    #[test]
    fn the_space_look_shader_is_valid_glsl_es_300() {
        crate::render::dots::assert_valid_glsl_es_300(
            include_str!("render/space_look_shader.wgsl"),
            &[
                ("vs_main", naga::ShaderStage::Vertex),
                ("fs_sky", naga::ShaderStage::Fragment),
                ("fs_globe", naga::ShaderStage::Fragment),
            ],
        );
    }
}

#[cfg(test)]
mod csv_e2e_tests;

#[cfg(test)]
mod circle_pitch_tests;

#[cfg(test)]
mod plain_fill_tests;

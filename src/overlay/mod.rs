//! Caller-supplied features drawn over the basemap.
//!
//! See `docs/superpowers/specs/2026-08-25-declarative-geojson-state-design.md`
//! for why these live in a store of their own rather than being clipped into
//! synthetic tile layers: a feature clipped into a tile becomes part of that
//! tile's decoded content, so moving one vehicle would invalidate five
//! caches for every tile on screen.

// `pub(crate)`, not `pub`. What these modules hold -- `build_plan`,
// `OverlayPlan`, `OverlayOrigin::to_local`, `OVERLAY_UNITS_PER_WORLD`,
// `parse_color`, `ParsedFeature`, `HitIndex`, `tolerance_world`,
// `world_units_per_px` -- is how this crate happens to draw an overlay
// today, not anything a caller needs or should be able to depend on.
// `plan` and `geom` in particular are
// exactly the modules that change when style expressions land, and every
// item visible from outside is a promise about a future version.
//
// What a caller genuinely needs is re-exported below, and nothing else.
// `pub` for `demos/big_points.rs --measure` and `BuildPhases`.
pub mod cluster;
pub(crate) mod columns;
pub(crate) mod geojson;
pub(crate) mod geom;
pub(crate) mod hit;
pub(crate) mod input;
pub(crate) mod paint;
pub(crate) mod plan;
// `pub` for the same reason as `tile_index`: `demos/big_points.rs --measure`
// times an index build and a view build on the synthetic data.
#[doc(hidden)]
pub mod points;
/// GPU range/time filters on point sources, and their playback clock.
pub mod filter;
/// Labelled caller markers in two kinds (address, arrival) as one GeoJSON
/// source: what a host page's `setMarkers` draws.
pub mod markers;
// Origin-destination arcs from columns: `render::arcs::ArcSet` built from
// source/target positions, colours (`color_scale`), widths and a filter column.
pub mod arcs;
// Trips from Kepler's timed-line GeoJSON: `render::trips::TripSet` with
// per-trip colours from a colour scale.
pub mod trips;
mod highlight;
pub use highlight::{HighlightStyle, HighlightWhere, WhereTest};
pub(crate) use highlight::{drawn_paint, group_highlight_plan, highlight_plan, snapshot_geometry};
pub(crate) mod source_paint;
/// Symbols placed along a source's lines: `OverlayStore::declare_source_symbols`.
pub mod line_symbols;
// `pub` only so `demos/us_block_groups.rs --measure` can time a build on
// the national data before phase B draws through it; hidden because it is
// the same kind of internal as the modules above, not a promise.
#[doc(hidden)]
pub mod tile_index;
/// Incremental live updates: `SourceDiff` and the worker that applies it.
pub mod live;
pub use live::{AddFeature, FeatureUpdate, LivePath, LiveUpdate, SourceDiff};
// `pub` for the same reason as `tile_index`: `--measure` times a cover bake.
#[doc(hidden)]
pub mod tile_plan;
/// GHSL population grid geometry and the count-tile codec (world dot
/// density). `pub`: `render::dots` and the offline converter both need it.
pub mod dotgrid;
/// H3 hexagon tiles of the race map's hexagon view: records, their codec
/// and the PMTiles/ClickHouse sources. `pub` for the converter.
pub mod hexes;
/// The race map's donut-cluster tiles (`CLT1`): records and their codec.
/// `pub` for the converter.
pub mod clusters_tile;
pub mod host;
/// Kepler.gl's colour and size scales (quantile, quantize, ordinal,
/// linear; linear/sqrt/log sizes) and named palettes, compiled into the
/// paint expressions a source takes. See the module doc.
pub mod color_scale;
/// Runtime hexagon binning of points (deck.gl's `HexagonLayer` grid):
/// count, sum, mean, max, min, median, deviation, variance, mode or unique
/// count per bin, out as overlay polygons.
pub mod hexbin;
/// Runtime square-grid binning of points (deck.gl's `GridLayer`): the
/// square sibling of [`hexbin`], with the same aggregations.
pub mod gridbin;
/// A second vector-tile source (MVT XYZ or vector PMTiles) as an overlay
/// with its own `fill`/`line`/`circle` style layers, fetched per viewport;
/// `MapView::add_vector_overlay` drives it.
pub mod vector_tiles;
/// 3D extrusion of overlay polygons through the basemap's
/// `fill-extrusion` mesh path.
pub mod extrude;
/// Arc, overlay-extrusion, trip and heatmap layers for a shell that is not
/// `MapView` (iOS): the engine's renderers in `MapView::paint`'s order.
pub mod shell_layers;
/// CSV/TSV as an overlay source: points, WKT or GeoJSON geometry, typed
/// property columns, a parse report. See the module doc.
pub mod csv;
/// FlatGeobuf as an overlay source: typed columns, a packed Hilbert R-tree
/// queried by bbox from bytes, a file or HTTP range requests. See the
/// module doc.
pub mod fgb;
/// GeoParquet as an overlay source: WKB or GeoArrow geometry, typed Arrow
/// columns, row groups skipped by their bbox statistics, from bytes, a file
/// or HTTP range requests. See the module doc.
#[cfg(feature = "geoparquet")]
pub mod geoparquet;
/// The headless re-push harness (`examples/repush_bench.rs`). `probe` only.
#[cfg(feature = "probe")]
#[doc(hidden)]
pub mod repush_bench;

pub use geojson::{FeatureError, FeatureId, OverlayGeometry};
pub use input::{DataFeatureInput, FeatureInput, GeometryInput};
// The property bag a data-driven source's paint expressions read, and the
// value type inside it. `TagValue` is `crate::mvt`'s and is re-exported
// rather than wrapped, because it IS the type every compiled style filter
// is already evaluated against -- a parallel overlay-only property value
// would have meant a parallel evaluator, which is the one thing
// `source_paint` exists not to be.
pub use crate::mvt::TagValue;
// `no_properties` is re-exported alongside the type because it is the value
// `DataFeatureInput`'s own doc comment tells a caller to use for a feature
// with no properties, and `source_paint` is `pub(crate)` -- so without this
// that instruction names something an application cannot reach.
pub use source_paint::{no_properties, FeatureProperties};
// The typed-column door (`OverlayStore::set_source_columns`) and the
// table a source's properties are held in.
pub use columns::{PropertyColumn, PropertyTable};
pub use cluster::{BuildPhases, ClusterItem, ClusterMode, ClusterOptions, ClusterProperty, ClusterReduce};

/// One point of a cluster, as `get_cluster_leaves` hands it back: its
/// position in the pushed collection, its id, where it is, and its own
/// properties.
#[derive(Debug, Clone, PartialEq)]
pub struct ClusterLeaf {
    pub index: usize,
    pub id: Option<FeatureId>,
    pub lon_lat: [f64; 2],
    pub properties: Arc<FeatureProperties>,
}
// `IconAnchor`, `TextFit`, and `TextJustify` are re-exported for the same
// reason `FeaturePaint` is: they are the types of three of its public
// fields (`icon_anchor`, `icon_text_fit`, `text_justify`), so a caller who
// cannot name them cannot write a `FeaturePaint` struct literal at all --
// which left `set_features_from`'s paint half reachable only by building
// the `serde_json::Map` the structured door exists to avoid.
//
// The rest of the field types were already reachable: `Rgba`,
// `ZoomValue<f32>` and `DashPattern` live in the public `crate::style`,
// and everything else is a primitive, an array, a tuple, or
// `Option<String>`. See `tests/paint_from_outside.rs`, which names every
// one of them from a crate that is not this one.
pub use paint::{FeaturePaint, IconAnchor, TextFit, TextJustify};

use std::collections::HashMap;
use std::sync::Arc;

use geojson::parse_feature_collection;
use plan::{build_plan, OverlayPlan};

/// A `FeatureId` in a form that can key a `HashMap`.
///
/// `FeatureId` itself cannot: its `Number(f64)` variant is neither `Eq` nor
/// `Hash`, and it must not become either -- deriving them would make every
/// id comparison in the API a bit-pattern comparison, including the
/// `FeatureRef` equality applications match hovers on.
///
/// Numbers key on their BITS, with `-0.0` folded onto `0.0` so the map
/// agrees with `f64` equality on the one value where bits and `==`
/// disagree in the direction that matters. They still disagree on `NaN`,
/// which is equal to nothing including itself under `==` and equal to
/// itself here: two `NaN` ids in one collection are refused as duplicates
/// rather than admitted as two features neither of which can be addressed.
/// That is the loud direction.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum IdKey {
    String(String),
    Number(u64),
}

impl IdKey {
    fn of(id: &FeatureId) -> Self {
        match id {
            FeatureId::String(text) => Self::String(text.clone()),
            // `+ 0.0` normalises `-0.0` to `0.0`; every other value, `NaN`
            // included, is left exactly as it is.
            FeatureId::Number(number) => Self::Number((number + 0.0).to_bits()),
        }
    }
}

/// One feature, as stored: its position in the collection, its identity, its
/// geometry in lon/lat, its resolved paint, and its own properties.
#[derive(Debug, Clone)]
pub struct OverlayFeature {
    pub index: usize,
    pub id: Option<FeatureId>,
    pub geometry: OverlayGeometry,
    pub paint: FeaturePaint,
    // No properties here: they live in the source's columnar
    // `PropertyTable`, at row `index`. Read one feature's with
    // `OverlayStore::feature_properties` (or `MapView::feature_properties`
    // for a hovered or clicked `FeatureRef`).
}

/// A source's name. `Arc<str>` because it is cloned into every
/// `FeatureRef` the map hands out -- one per hover event, per click, per
/// hit-index entry -- and a `String` there would be an allocation per
/// event and per feature.
pub type SourceId = Arc<str>;

/// The source `set_features`, `set_features_from` and
/// `set_features_deferred` write, and the one an `OverlayStore` always has.
///
/// It exists so those three calls -- which predate sources and have
/// out-of-repo callers -- are exactly "the default source", rather than a
/// second mechanism beside sources. Its name is the empty string, so it
/// reads unmistakably in a `FeatureRef` and sorts first in the source
/// order (it is always index 0, so named sources draw ON TOP of whatever
/// `set_features` put down).
///
/// `set_source("")` is legal and is the same thing as `set_features_from`;
/// nothing rejects it, because a rejection would be a rule to remember for
/// no gain.
pub const DEFAULT_SOURCE: &str = "";

/// What a caller declares about a source beyond the features in it.
///
/// `Default` is "declare nothing", which is exactly the behaviour every
/// source had before this type existed: the library works the bracket out
/// for itself and a source whose widths admit a fixed bracket gets one.
/// `zoom_independent` only ever adds a GUARANTEE -- it never makes a
/// source slower, and never turns a working push into a different-looking
/// one. `no_tiny_polygon_reduction` is the one opt-out: it changes how a
/// tiled source's sub-pixel polygons draw at low zoom.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SourceOptions {
    /// Declares that this source's bake must be exact at every zoom, so
    /// that a zoom change can never touch it.
    ///
    /// This is an ASSERTION, checked, not a hint. A source whose widths
    /// admit a `BakeBracket::Ramp` is zoom-independent whether or not
    /// anybody says so -- see `plan::zoom_dependence`, which is the
    /// predicate and is applied to every source. What setting this adds is
    /// that a source which is NOT zoom-independent is refused, loudly,
    /// naming the feature responsible, instead of being quietly baked at a
    /// `Bucket` bracket and re-resolved at every integer zoom.
    ///
    /// That is the direction a caller wants. A stop layer of five thousand
    /// features that silently starts paying a re-resolve per crossing
    /// because one feature acquired a `step` width is precisely the
    /// regression this flag exists to make impossible; the failure is
    /// visible at the push that caused it rather than in a frame-time
    /// graph a week later.
    ///
    /// It says nothing about whether the CALLER needs to re-push on a zoom
    /// change. A caller computing marker geometry from the camera's metres
    /// per pixel has a source whose geometry really is a function of zoom,
    /// and no declaration here can change that -- such a source simply
    /// leaves this `false` and pushes when it must.
    pub zoom_independent: bool,

    /// Turns OFF tiny-polygon reduction for this source's tiled polygons
    /// (named after tippecanoe's `--no-tiny-polygon-reduction`).
    ///
    /// By default, as tippecanoe does, a polygon too small to keep at a
    /// zoom (under one tolerance-square, 0.375 px on a side) is not simply
    /// dropped: the area of such polygons accumulates across the tile and
    /// comes back as tolerance-sized squares, each carrying the feature that
    /// tipped the total, so a dense area (a city of census block groups at
    /// national zoom) keeps its fill and, statistically, its colour mix.
    /// With this set, they are dropped as geojson-vt -- and MapLibre's
    /// GeoJSON source -- drop them.
    ///
    /// Read when the source's tile index is built (a push); declare it
    /// before the push it should apply to.
    pub no_tiny_polygon_reduction: bool,

    /// MapLibre's GeoJSON-source `lineMetrics`: every line of this source
    /// carries its progress (`0` at a LineString's first vertex, `1` at
    /// its last, by length), which `["line-progress"]`, `line-gradient`
    /// and the per-frame trim (`MapView::set_source_line_trim`) read.
    ///
    /// Computed on the WHOLE-SOURCE path only (story-content plan D3): a
    /// source with this set is never tiled
    /// ([`tile_plan::should_tile`]), so each line is one mesh and its
    /// progress is continuous across every map-tile boundary. A source
    /// over the tiling threshold ([`tile_plan::TILED_SOURCE_MIN_POINTS`])
    /// is therefore REFUSED with it, at the declaration and at every push,
    /// rather than silently drawn without metrics.
    ///
    /// Read when the source is planned (a push); declare it before the
    /// push it should apply to.
    pub line_metrics: bool,

    /// Never tile this source, whatever its size or
    /// `GPUI_MAP_OVERLAY_TILING`: it is always drawn whole. What a story
    /// overlay layer declares (the capture's story overlay cache draws the
    /// whole-source mesh only), so a story renders the same on every
    /// machine; its caller refuses a source over
    /// [`tile_plan::TILED_SOURCE_MIN_POINTS`] up front.
    pub whole_source: bool,
}

impl SourceOptions {
    /// The tile index options this source's declaration implies.
    pub(crate) fn tile_index_options(&self) -> tile_index::TileIndexOptions {
        tile_index::TileIndexOptions {
            tiny_polygon_reduction: !self.no_tiny_polygon_reduction,
            ..tile_index::TileIndexOptions::default()
        }
    }
}

/// How an event names a feature.
///
/// `source` is which source the feature belongs to. `index` is always
/// present because GeoJSON `id` is optional and hover edge-detection needs
/// an identity that always exists. It identifies a feature only WITHIN one
/// source's currently drawn collection -- a push to that source redefines
/// it, so it is meaningful to compare between two events and never to
/// store across an update. `id` is what an application should key its own
/// state on, and `id` is only unique within a source, which is why the
/// source travels with it: two sources may legitimately both contain a
/// feature with id `"42"`.
#[derive(Debug, Clone, PartialEq)]
pub struct FeatureRef {
    pub source: SourceId,
    pub index: usize,
    pub id: Option<FeatureId>,
}

impl FeatureRef {
    pub fn of(source: &SourceId, feature: &OverlayFeature) -> Self {
        Self { source: Arc::clone(source), index: feature.index, id: feature.id.clone() }
    }

    /// Whether this names the same feature of the same source as `other`.
    ///
    /// The same thing `PartialEq` does; spelled out as a method because
    /// hover edge-detection's whole job is this comparison and getting it
    /// wrong by forgetting the source -- two sources each with a feature
    /// at index 0 -- is a silent stuck hover rather than a failure.
    pub fn is(&self, other: &Self) -> bool {
        self == other
    }
}

/// What the map tells the application about its own features.
#[derive(Debug, Clone, PartialEq)]
pub enum FeatureEvent {
    /// `lon_lat` is the ground point the user actually pointed at, not the
    /// feature's centre -- a caller positioning UI wants where the click
    /// landed.
    Click { feature: FeatureRef, lon_lat: (f64, f64) },
    HoverEnter { feature: FeatureRef },
    HoverLeave { feature: FeatureRef },
}

/// A paint-only update, resolved against the source it names but not yet
/// applied to it.
///
/// Resolving and applying are two steps because everything fallible has to
/// be behind us before any field is written -- the same all-or-nothing
/// contract `set_source` keeps -- and because the caller (`MapView`) has one
/// more reason of its own to take the slow path that the store cannot see:
/// whether the mesh currently on screen belongs to this generation at all.
#[derive(Debug, Clone)]
pub(crate) struct PaintUpdate {
    /// Which source this was resolved against. Carried rather than passed
    /// again at apply time: `features` and `ops` are indices into ONE
    /// source's collection and plan, and applying them to another source
    /// would recolour somebody else's line without failing. `apply_*`
    /// re-finds the source by this name, so the two halves cannot be
    /// mismatched by a caller.
    source: SourceId,
    /// Which feature each entry of the caller's `updates` slice names, in
    /// the same order. Resolved once here so `apply_*` cannot fail.
    features: Vec<usize>,
    /// The `plan.ops` those features produced -- ascending, deduplicated,
    /// and empty for a source with no plan. This is what the mesh rewrite
    /// and the GPU upload are both restricted to.
    ops: Vec<usize>,
    /// True when the update changes something no in-place rewrite can
    /// express, and the whole source has to be re-planned and re-baked.
    rebake: bool,
}

impl PaintUpdate {
    pub(crate) fn ops(&self) -> &[usize] {
        &self.ops
    }

    pub(crate) fn needs_rebake(&self) -> bool {
        self.rebake
    }

    pub(crate) fn source(&self) -> &SourceId {
        &self.source
    }
}

/// One source: a named, independently replaceable collection of features,
/// with the plan, hit index and id map built from exactly those features.
///
/// This is the unit the whole overlay is now keyed by, and the reason is
/// the defect it removes. The store used to hold ONE collection, so a push
/// of forty moving vehicles re-planned, re-tessellated, re-indexed and
/// re-uploaded the five thousand bus stops beside them -- which is why a
/// consumer found that HIDING the stop layer made the buses move faster.
/// Nothing a source does costs another source anything: `generation` is
/// per source, `plan` is per source (its own origin and its own bake
/// bracket -- see `OverlayPlan`), and the mesh baked from it is its own
/// draw call.
///
/// MapLibre's shape, arrived at from the same constraint: a `SourceCache`
/// per source, a bucket per source-layer per tile, and a draw per bucket.
/// Its `GeoJSONSource.setData` reloads that source's tiles and touches no
/// other source's; this is the same statement with "tile" deleted, since
/// an overlay is not tiled.
#[derive(Debug)]
struct Source {
    name: SourceId,
    /// Behind an `Arc` so a whole generation can be snapshotted by
    /// `snapshot()` for the price of a refcount bump, rather than by cloning
    /// every feature on every replacement.
    features: Arc<Vec<OverlayFeature>>,
    /// The features' properties, as columns: row `i` is feature index `i`.
    /// See `columns`.
    properties: Arc<PropertyTable>,
    plan: Option<OverlayPlan>,
    /// `Arc` for the same reason as `features`, and more so: rebuilding a
    /// second copy of the index just to snapshot it would be the one
    /// genuinely expensive part of a replacement done twice.
    hit_index: Arc<hit::HitIndex>,
    /// Where each declared `id` sits in `features`.
    ///
    /// Built once with the features rather than scanned per update:
    /// `set_feature_paint` is called on every row step through a fleet
    /// grid, and a linear scan of 206 features per update would be the
    /// only part of that path that grows with the collection.
    ///
    /// Not in `OverlaySnapshot`: an id names a feature in the REQUESTED
    /// collection, which is what an update addresses. Labels and hit
    /// targets are the things that must lag the drawn generation, and this
    /// is neither.
    by_id: HashMap<IdKey, usize>,
    /// The store's counter as of this source's last change. NOT a count of
    /// this source's own changes: stamps come from one monotonic supply so
    /// that they are unique across the whole store, which is what lets a
    /// bake carry `(name, stamp)` and be judged by equality.
    ///
    /// This -- not `OverlayStore::generation` -- is what a bake's staleness
    /// is decided against. Keyed on the store's counter, a push to
    /// `vehicles` would invalidate an in-flight `stops` bake and the stops
    /// would re-tessellate for nothing, which is the defect wearing a
    /// generation counter as a disguise.
    generation: u64,
    /// The same number as `generation`, published where a thread that does
    /// not own the store can read it.
    ///
    /// PER SOURCE, for exactly the reason above: a bake polls this to
    /// discover mid-tessellation that it has been superseded, and a
    /// store-wide atomic would have every push cancel every other source's
    /// bake.
    ///
    /// # Why `Relaxed` is enough, at the store and at the reader
    ///
    /// The strong reason is that a stale read here is always stale in the
    /// SAFE DIRECTION, which makes cancellation conservative by
    /// construction rather than by recovery.
    ///
    /// This field is written only where `generation` is written,
    /// immediately after, and the store's counter only ever counts up -- so
    /// a source's stamps only ever count up too. Every value this atomic
    /// has ever held is therefore a value this source has already reached,
    /// and a reader -- however far behind -- can only observe a number LESS
    /// THAN OR EQUAL TO the true current stamp. It can never observe one
    /// that is too high.
    ///
    /// A bake cancels itself on `its_generation < live.load()`. Since the
    /// load cannot overshoot, that comparison can only be true when the
    /// bake really has been superseded. The failure mode is therefore
    /// one-sided: a lagging read can let a DEAD bake keep working for a
    /// moment, and can never cancel a LIVE one. Nothing correct is ever
    /// thrown away, at any ordering.
    ///
    /// A REMOVED source writes its successor stamp here before it is
    /// dropped (see `remove_source`), so a bake whose source has gone away
    /// still cancels -- the `Arc` the bake holds outlives the `Source` it
    /// came from, which is the whole point of it being an `Arc`.
    ///
    /// The weaker reason, which would hold even without that: this signal
    /// is an optimisation and not a correctness mechanism at all. A reader
    /// that misses an increment does more work than it needed to and then
    /// reaches `MapView::install_overlay_mesh`, which re-reads the store's
    /// own plain stamp under `&mut self` on the UI thread and rejects the
    /// mesh. And no data is published alongside this number that a reader
    /// has to see -- the mesh travels by move, not through shared memory --
    /// so there is nothing for an `Acquire`/`Release` pair to order.
    live_generation: Arc<std::sync::atomic::AtomicU64>,
    options: SourceOptions,
    /// This source's paint EXPRESSIONS, compiled -- what
    /// `declare_source_paint` installs and what `build_plan` resolves each
    /// feature's colour through.
    ///
    /// Default is "nothing declared", which makes `build_plan` take the
    /// original one-op-per-feature path. STICKY, like `options`, and for the
    /// same reason: it is declared once at startup and every later push to
    /// the name uses it, so the hot path stays two arguments.
    ///
    /// `Arc` because `OverlaySnapshot` carries it: a zoom-bucket crossing
    /// re-resolves the DRAWN generation's plan (`OverlaySnapshot::plan_at`),
    /// and it has to do that through the same expressions the mesh on
    /// screen was built with -- reading the live source there would let a
    /// declaration made mid-flight repaint a mesh it does not describe.
    paint: Arc<source_paint::SourcePaint>,
    /// This source's line-placed symbols (`declare_source_symbols`), or
    /// `None`. STICKY, like `paint`, and carried into the snapshot at the
    /// push so the arrows on screen always belong to the lines they were
    /// laid out along.
    line_symbols: Option<Arc<line_symbols::LineSymbolLayout>>,
    /// The declaration as of the last push -- what the snapshot carries. A
    /// declaration governs the NEXT push, never the lines already drawn.
    line_symbols_pushed: Option<Arc<line_symbols::LineSymbolLayout>>,
    /// Drawn through the tile index (`tile_plan::should_tile`) rather than
    /// as one whole-source mesh. `plan` then holds the point features only:
    /// polygons and lines are cut per tile by `MapView`'s tile set.
    tiled: bool,
    /// A tiled source's tile index, once adopted (`adopt_tile_index`). From
    /// then on it is the ONE copy of the polygons' and lines' geometry:
    /// their `features[i].geometry` is `OverlayGeometry::Indexed`, and the
    /// hit test and any rebuild (a paint update's re-class, the untiled
    /// fallback) read the index. Cleared by a push, which brings new
    /// geometry; kept by a paint update, which does not.
    tile_index: Option<Arc<tile_index::TileIndex>>,
    /// The point-path declaration (`set_source_cluster_options`). STICKY,
    /// like `options`: every later push to the name goes down the point
    /// path while it is set.
    cluster: Option<cluster::ClusterOptions>,
    /// The point path's data, index and drawn view key. `features`,
    /// `properties`, `plan` and `hit_index` then hold the VIEW.
    points: Option<PointState>,
    /// The row the next live-added feature gets (`accept_source_diff`):
    /// rows only append, so a `FeatureRef::index` stays valid across
    /// incremental updates. Ahead of `features.len()` while a diff job
    /// is on a worker.
    next_row: usize,
    /// Diffs accepted, for `LiveUpdate::seq`.
    live_seq: u64,
}

/// A point-path source's columns, its index once built, and the key of
/// the view installed as its content.
#[derive(Debug)]
pub(crate) struct PointState {
    pub data: Arc<points::PointData>,
    pub index: Option<Arc<cluster::ClusterIndex>>,
    pub view: Option<points::ViewKey>,
    /// The installed view's pushed row per feature (`None`: a cluster);
    /// empty when the view was built unfiltered. See
    /// `OverlaySnapshot::point_rows`.
    pub rows: Arc<Vec<Option<usize>>>,
}

/// Everything the caller has asked to be drawn, in sources.
///
/// This is the REQUESTED state, which is not always the DRAWN state: between
/// a push and the moment its mesh finishes baking, that source is a
/// generation ahead of what is on screen. `MapView` holds a snapshot of each
/// source's drawn generation and serves labels, icons and hit-testing from
/// those, so nothing a user sees or clicks is ever taken from here directly.
/// See `MapView::overlay_drawn`.
///
/// # Order is draw order
///
/// `sources` is a `Vec`, not a map, because the order sources were first
/// pushed in is the order they draw in -- later on top, the way a style's
/// layer list works. A push to an existing name keeps that source's
/// position, so a feed that re-pushes `vehicles` every tick does not
/// shuffle itself above or below `stops`. A `HashMap` would have made draw
/// order a function of hash iteration, which is to say nondeterministic.
///
/// Linear lookup by name is deliberate too: a source lookup happens once
/// per push and once per bake install, never per frame and never per
/// feature, and the number of sources is the number of layers an
/// application draws -- single digits. A map would buy nothing measurable
/// and cost the ordering.
#[derive(Debug)]
pub struct OverlayStore {
    sources: Vec<Source>,
    /// The single monotonic supply every source's stamp is drawn from, and
    /// the number `generation()` reports.
    generation: u64,
}

impl Default for OverlayStore {
    fn default() -> Self {
        // The default source exists from the start rather than being
        // created on first use, so `features()`, `plan()` and `snapshot()`
        // -- the pre-sources API, which has out-of-repo callers -- are
        // total functions rather than ones that can fail to find their own
        // source.
        Self { sources: vec![Source::empty(DEFAULT_SOURCE.into())], generation: 0 }
    }
}

impl Source {
    fn empty(name: SourceId) -> Self {
        Self {
            name,
            features: Arc::default(),
            properties: PropertyTable::empty(),
            plan: None,
            hit_index: Arc::default(),
            by_id: HashMap::new(),
            generation: 0,
            live_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            options: SourceOptions::default(),
            paint: Arc::default(),
            line_symbols: None,
            line_symbols_pushed: None,
            tiled: false,
            tile_index: None,
            cluster: None,
            points: None,
            next_row: 0,
            live_seq: 0,
        }
    }

    fn snapshot(&self) -> OverlaySnapshot {
        OverlaySnapshot {
            source: Arc::clone(&self.name),
            generation: self.generation,
            features: Arc::clone(&self.features),
            properties: Arc::clone(&self.properties),
            hit_index: Arc::clone(&self.hit_index),
            paint: Arc::clone(&self.paint),
            line_symbols: self.line_symbols_pushed.clone(),
            tiled: self.tiled,
            tile_index: self.tile_index.clone(),
            point_rows: self.points.as_ref().map_or_else(Arc::default, |p| Arc::clone(&p.rows)),
            has_symbols: self
                .features
                .iter()
                .any(|feature| feature.paint.text_field.is_some() || feature.paint.icon_image.is_some()),
        }
    }

    /// Whether a zoom change can be guaranteed never to touch this
    /// source's mesh. See `plan::zoom_dependence` for the predicate, and
    /// `SourceOptions::zoom_independent` for what declaring it adds.
    ///
    /// Read off the BRACKET the plan was actually baked at, not recomputed
    /// from the features: the bracket is what `refresh_overlay_bake_zoom`
    /// branches on, so anything else here would be a second opinion about
    /// the thing that actually happens.
    ///
    /// A source with nothing drawable is zoom-independent vacuously and
    /// truthfully -- there is no mesh for a crossing to touch.
    fn zoom_independent(&self) -> bool {
        match &self.plan {
            Some(plan) => matches!(plan.bracket, plan::BakeBracket::Ramp { .. }),
            None => true,
        }
    }
}

/// One source's one generation, held by whoever is drawing it.
///
/// The features and the hit index travel TOGETHER with the source name and
/// the generation they belong to, which is the whole point: labels, icons
/// and hit targets must describe the same feature set the mesh on screen
/// was tessellated from, and must be attributed to the source that mesh
/// came from or a `FeatureRef` handed to the application names the wrong
/// collection. Both halves are `Arc`s, so holding one costs two refcounts.
#[derive(Debug, Clone)]
pub(crate) struct OverlaySnapshot {
    pub source: SourceId,
    pub generation: u64,
    pub features: Arc<Vec<OverlayFeature>>,
    /// This generation's feature properties, row `i` for feature index `i`.
    pub properties: Arc<PropertyTable>,
    pub hit_index: Arc<hit::HitIndex>,
    /// The paint expressions this generation's plan was built with. See
    /// `Source::paint` for why the snapshot carries them rather than
    /// reading the live source.
    pub paint: Arc<source_paint::SourcePaint>,
    /// The line-symbol declaration this generation was pushed under (see
    /// `Source::line_symbols`).
    pub line_symbols: Option<Arc<line_symbols::LineSymbolLayout>>,
    /// Whether any feature of this generation has a label or an icon.
    /// Worked out once per snapshot -- a push, an install, a paint update
    /// -- so the per-frame symbol pass can skip a source without walking
    /// it: 242,748 unlabelled block groups cost 2.7 ms a frame to find
    /// nothing in.
    pub has_symbols: bool,
    /// See `Source::tiled`. The whole-source plan (`plan_at`) is then the
    /// point features' only.
    pub tiled: bool,
    /// See `Source::tile_index`: where an `Indexed` feature's geometry --
    /// and so its exact hit test -- lives.
    pub tile_index: Option<Arc<tile_index::TileIndex>>,
    /// A filtered point source's pushed row behind each feature of this
    /// view (`None`: a cluster): a view's feature `i` is NOT row `i`, and
    /// the filter window is a row's (`MapView::feature_at`). Empty for
    /// every other source.
    pub point_rows: Arc<Vec<Option<usize>>>,
}

/// The part of an [`OverlaySnapshot`] a paint update reads
/// (`OverlayStore::source_paint_snapshot`).
pub(crate) struct PaintSnapshot {
    pub source: SourceId,
    pub generation: u64,
    pub features: Arc<Vec<OverlayFeature>>,
    pub properties: Arc<PropertyTable>,
    pub paint: Arc<source_paint::SourcePaint>,
    pub tiled: bool,
}

/// The whole-source plan a source bakes: every feature's, or -- for a
/// TILED source -- the point features' only, since its polygons and lines
/// are drawn per tile. `feature_ops` keeps one entry per input feature
/// either way (a non-point feature of a tiled source gets an empty range),
/// so a paint update's op lookup stays aligned.
pub(crate) fn source_plan_for(
    features: &[OverlayFeature],
    properties: &PropertyTable,
    bracket: plan::BakeBracket,
    paint: &source_paint::SourcePaint,
    tiled: bool,
) -> Option<OverlayPlan> {
    if !tiled {
        return build_plan(features, properties, bracket, paint);
    }
    if !features.iter().any(|f| matches!(f.geometry, OverlayGeometry::Point(_))) {
        return None;
    }
    let points: Vec<OverlayFeature> = features
        .iter()
        .map(|f| match f.geometry {
            OverlayGeometry::Point(_) => f.clone(),
            _ => OverlayFeature {
                index: f.index,
                id: f.id.clone(),
                geometry: OverlayGeometry::Empty,
                paint: f.paint.clone(),
            },
        })
        .collect();
    build_plan(&points, properties, bracket, paint)
}

impl OverlaySnapshot {
    /// Re-resolves THIS generation's features into a plan for a different
    /// zoom bucket, without re-parsing or re-validating anything: the paint
    /// is already compiled (`FeaturePaint::line_width` is a `ZoomValue`),
    /// and the geometry is already in hand behind the `features` `Arc`.
    ///
    /// This is what a zoom-bucket crossing costs a `Bucket`-baked source:
    /// one walk of ITS features building fresh `PaintOp`s, and then a
    /// paint-attribute rewrite of ITS existing mesh
    /// (`render::rewrite_paint_attributes`). No tessellation, and nothing
    /// at all for the sources beside it -- a `Ramp` source is skipped
    /// before this is ever called.
    ///
    /// It hangs off the SNAPSHOT rather than off `Source` on purpose. The
    /// re-resolve has to describe the feature set whose mesh is currently
    /// on screen, and during a bake window the source is already a
    /// generation ahead of that -- exactly the skew
    /// `MapView::overlay_drawn` exists to prevent. A snapshot is the
    /// store's own handle to one generation, so asking it is asking the
    /// store for the right answer.
    ///
    /// `None` for a generation with nothing drawable (all labels, or empty),
    /// the same condition `build_plan` reports on the way in. Geometry does
    /// not depend on zoom, so a generation that had a plan at one zoom has
    /// one at every zoom -- but the caller must still handle `None` rather
    /// than unwrapping, because nothing structurally forbids being asked
    /// about a label-only generation.
    pub(crate) fn plan_at(&self, bracket: plan::BakeBracket) -> Option<OverlayPlan> {
        source_plan_for(&self.features, &self.properties, bracket, &self.paint, self.tiled)
    }

    /// Feature `index`'s properties in THIS generation. See
    /// `OverlayStore::feature_properties`.
    pub(crate) fn feature_properties(&self, index: usize) -> Option<Arc<FeatureProperties>> {
        (index < self.features.len() && !self.properties.is_dead(index)).then(|| row_properties(&self.properties, index))
    }

    /// The topmost feature of THIS generation within `tolerance_world` of
    /// `world`, or `None`.
    ///
    /// The two halves of a hit test -- the index's bounding boxes and the
    /// features whose geometry the exact test reads (see `hit`'s module
    /// doc) -- must come from the same generation, and this is where that
    /// is guaranteed rather than remembered: a snapshot IS one generation's
    /// features and one generation's index, so there is no way to pass the
    /// pair from two. `MapView::feature_at` calls only this.
    pub(crate) fn feature_at(
        &self,
        world: (f64, f64),
        tolerance_world: f64,
        world_units_per_px: f64,
    ) -> Option<FeatureRef> {
        self.feature_at_where(world, tolerance_world, world_units_per_px, &|_| true)
    }

    /// [`Self::feature_at`] over the features `keep` admits.
    pub(crate) fn feature_at_where(
        &self,
        world: (f64, f64),
        tolerance_world: f64,
        world_units_per_px: f64,
        keep: &dyn Fn(usize) -> bool,
    ) -> Option<FeatureRef> {
        self.hit_index.hit_test_where(&self.features, self.tile_index.as_deref(), world, tolerance_world, world_units_per_px, keep)
    }
}

impl OverlayStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// True when no source holds a single feature.
    pub fn is_empty(&self) -> bool {
        self.sources.iter().all(|source| source.features.is_empty())
    }

    /// What the overlay sources hold resident for as long as they are set:
    /// each adopted tile index (the one geometry copy of a tiled source,
    /// `TileIndex::bytes`), the feature vector's block, the hit index's
    /// table, the `id` map and the property columns. O(sources): no feature is walked, so a
    /// source's lon/lat geometry before its index is adopted -- transient,
    /// freed at adoption -- is not counted.
    ///
    /// `MapView` gives the basemap's CPU tile caches this much less room
    /// (see `basemap_cpu_budget_bytes` there).
    #[must_use]
    pub fn resident_bytes(&self) -> usize {
        use crate::render::{hashmap_table_bytes, vec_block_bytes};
        self.sources
            .iter()
            .map(|source| {
                source.tile_index.as_ref().map_or(0, |index| index.bytes())
                    + vec_block_bytes::<OverlayFeature>(source.features.capacity())
                    + source.hit_index.probe_bytes()
                    + hashmap_table_bytes::<IdKey, usize>(source.by_id.capacity())
                    + source.properties.bytes()
                    + source.points.as_ref().map_or(0, |p| {
                        p.data.bytes() + p.index.as_ref().map_or(0, |index| index.bytes())
                    })
            })
            .sum()
    }

    /// Every source's name, in DRAW ORDER -- earliest-pushed first, drawn
    /// first, and therefore underneath. [`DEFAULT_SOURCE`] is always
    /// present and always first.
    pub fn source_names(&self) -> impl Iterator<Item = &SourceId> {
        self.sources.iter().map(|source| &source.name)
    }

    /// [`DEFAULT_SOURCE`]'s features -- what `set_features` and
    /// `replace` write.
    ///
    /// NOT every source's features concatenated, and it must not become
    /// that: `OverlayFeature::index` and a `FeatureRef`'s `index` are
    /// positions within ONE source's collection, so a concatenation would
    /// hand out indices that address nothing. Ask
    /// [`Self::source_features`] for a named source.
    pub fn features(&self) -> &[OverlayFeature] {
        self.source_features(DEFAULT_SOURCE).unwrap_or(&[])
    }

    /// One named source's features, or `None` if no such source exists.
    pub fn source_features(&self, name: &str) -> Option<&[OverlayFeature]> {
        self.source(name).map(|source| source.features.as_slice())
    }

    /// Whether a zoom change is guaranteed never to touch this source's
    /// mesh, or `None` if no such source exists.
    ///
    /// The honest answer, computed from what was actually baked. See
    /// [`SourceOptions::zoom_independent`].
    pub fn source_is_zoom_independent(&self, name: &str) -> Option<bool> {
        self.source(name).map(Source::zoom_independent)
    }

    /// The stamp a bake of this source carries and is judged by, or `None`
    /// if no such source exists.
    pub fn source_generation(&self, name: &str) -> Option<u64> {
        self.source(name).map(|source| source.generation)
    }

    fn source(&self, name: &str) -> Option<&Source> {
        self.sources.iter().find(|source| &*source.name == name)
    }

    fn source_mut(&mut self, name: &str) -> Option<&mut Source> {
        self.sources.iter_mut().find(|source| &*source.name == name)
    }

    /// [`DEFAULT_SOURCE`]'s plan. `None` when no feature in it has
    /// drawable geometry -- an empty collection, or one that is all labels.
    ///
    /// `pub(crate)`: `OverlayPlan` is this crate's internal tessellation
    /// input, not API. It reaches the outside only through
    /// `MapView::overlay()`, and nothing outside the crate has any use for
    /// it.
    #[cfg(test)]
    pub(crate) fn plan(&self) -> Option<&OverlayPlan> {
        self.source_plan(DEFAULT_SOURCE)
    }

    pub(crate) fn source_plan(&self, name: &str) -> Option<&OverlayPlan> {
        self.source(name)?.plan.as_ref()
    }

    /// [`DEFAULT_SOURCE`]'s features and hit index, paired with its
    /// generation, for a caller that must keep the three consistent with
    /// each other while the store moves on.
    /// [`DEFAULT_SOURCE`]'s id map and hit index. Tests only, and for the
    /// same reason as the wrappers above: the whole-state comparison in
    /// `the_two_paths_build_identical_stores` renders every piece of a
    /// store's internals so the structured door is graded against the
    /// string door rather than against what its author remembered to
    /// check, and these are two of the pieces.
    #[cfg(test)]
    fn default_by_id(&self) -> &HashMap<IdKey, usize> {
        static EMPTY: std::sync::OnceLock<HashMap<IdKey, usize>> = std::sync::OnceLock::new();
        self.source(DEFAULT_SOURCE)
            .map(|source| &source.by_id)
            .unwrap_or_else(|| EMPTY.get_or_init(HashMap::new))
    }

    #[cfg(test)]
    fn default_hit_index(&self) -> &hit::HitIndex {
        static EMPTY: std::sync::OnceLock<hit::HitIndex> = std::sync::OnceLock::new();
        self.source(DEFAULT_SOURCE)
            .map(|source| &*source.hit_index)
            .unwrap_or_else(|| EMPTY.get_or_init(hit::HitIndex::default))
    }

    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> OverlaySnapshot {
        self.source_snapshot(DEFAULT_SOURCE)
            .expect("the default source is created with the store and is never removed")
    }

    pub(crate) fn source_snapshot(&self, name: &str) -> Option<OverlaySnapshot> {
        self.source(name).map(Source::snapshot)
    }

    /// What a paint update reads of source `name` -- [`Self::source_snapshot`]
    /// without `has_symbols`, whose walk over every feature a slider drag
    /// would otherwise pay per source per move (~2.7 ms nationally).
    pub(crate) fn source_paint_snapshot(&self, name: &str) -> Option<PaintSnapshot> {
        self.source(name).map(|source| PaintSnapshot {
            source: Arc::clone(&source.name),
            generation: source.generation,
            features: Arc::clone(&source.features),
            properties: Arc::clone(&source.properties),
            paint: Arc::clone(&source.paint),
            tiled: source.tiled,
        })
    }

    /// [`Self::source_snapshot`] after a live diff landed, from `prev`
    /// (the same generation's): its `has_symbols` is kept when the feature
    /// vector is the same one -- a writes-only diff -- instead of walking
    /// every feature again (~3 ms nationally, on the UI thread).
    pub(crate) fn live_snapshot(&self, name: &str, prev: &OverlaySnapshot) -> Option<OverlaySnapshot> {
        let source = self.source(name)?;
        if source.generation != prev.generation || !Arc::ptr_eq(&source.features, &prev.features) {
            return Some(source.snapshot());
        }
        Some(OverlaySnapshot {
            properties: Arc::clone(&source.properties),
            hit_index: Arc::clone(&source.hit_index),
            tile_index: source.tile_index.clone(),
            ..prev.clone()
        })
    }

    /// Source `name`'s shared `Arc<str>`, without building a snapshot (which
    /// walks every feature for `has_symbols`).
    pub(crate) fn source_id(&self, name: &str) -> Option<SourceId> {
        self.source(name).map(|source| Arc::clone(&source.name))
    }

    /// Where the feature declaring `id` sits in [`DEFAULT_SOURCE`], or
    /// `None` if no feature there declares it. Features with no `id` at all
    /// are unaddressable -- inherent to naming things by id, not a gap.
    #[cfg(test)]
    pub(crate) fn index_of(&self, id: &FeatureId) -> Option<usize> {
        self.source_index_of(DEFAULT_SOURCE, id)
    }

    pub(crate) fn source_index_of(&self, name: &str, id: &FeatureId) -> Option<usize> {
        self.source(name)?.by_id.get(&IdKey::of(id)).copied()
    }

    /// `index_of`, but an id that names nothing is an ERROR carrying it
    /// rather than a `None` the caller has to remember to reject.
    #[cfg(test)]
    pub(crate) fn resolve_id(&self, id: &FeatureId) -> Result<usize, FeatureError> {
        self.source_resolve_id(DEFAULT_SOURCE, id)
    }

    pub(crate) fn source_resolve_id(
        &self,
        name: &str,
        id: &FeatureId,
    ) -> Result<usize, FeatureError> {
        self.source_index_of(name, id).ok_or_else(|| FeatureError::UnknownId(id.clone()))
    }

    /// Increments on every SUCCESSFUL change to any source, and never on a
    /// rejected one.
    ///
    /// This is the STORE's counter -- the supply every source's stamp is
    /// drawn from, and a gauge an application can watch to see that
    /// something changed. It is NOT what a bake's staleness is judged by;
    /// that is [`Self::source_generation`], per source, because a push to
    /// one source must not invalidate another's bake. See `Source::
    /// generation`.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// A handle to one source's stamp that a bake of that source can carry
    /// onto a worker thread and poll while it tessellates.
    ///
    /// The point is that this OUTLIVES the borrow: the returned `Arc` keeps
    /// reading the live value after the caller has given the store back,
    /// which is what lets a bake started for `("stops", 7)` discover,
    /// halfway through its own tessellation, that `stops` has reached 9 and
    /// that nothing it produces can be installed -- while a hundred pushes
    /// to `vehicles` in between leave it alone.
    ///
    /// See `Source::live_generation` for why `Relaxed` is enough and why a
    /// removed source still cancels its bakes.
    pub(crate) fn source_live_generation(
        &self,
        name: &str,
    ) -> Option<Arc<std::sync::atomic::AtomicU64>> {
        self.source(name).map(|source| Arc::clone(&source.live_generation))
    }

    /// Every source's areas, LIVE, for labels over data on the browser and
    /// phone shells (`ShellOverlays::area_cover`), whose sources all draw
    /// under the labels.
    pub(crate) fn area_cover(&self) -> hit::AreaCover {
        let mut cover = hit::AreaCover::default();
        for source in &self.sources {
            cover.push(&source.name, source.generation, &source.features, &source.hit_index, source.tile_index.as_ref());
        }
        cover
    }

    /// The feature of source `name` under `world`, against the LIVE source
    /// -- what `source_snapshot(name)?.feature_at(..)` answers, without
    /// building the snapshot (which walks every feature for `has_symbols`:
    /// 25 k features for California, per source, per hover). For the
    /// browser and phone shells (`overlay::host::ShellOverlays::feature_at`),
    /// which have no drawn-generation copy to prefer.
    pub(crate) fn live_feature_at(
        &self,
        name: &str,
        world: (f64, f64),
        tolerance_world: f64,
        world_units_per_px: f64,
    ) -> Option<FeatureRef> {
        let source = self.source(name)?;
        source.hit_index.hit_test(&source.features, source.tile_index.as_deref(), world, tolerance_world, world_units_per_px)
    }

    // No `hit_index()` accessor. `HitIndex` is an implementation of
    // hit-testing, not API -- a caller asks "what is under this point"
    // through the map, not by querying an index -- and the only legitimate
    // reader inside the crate is `MapView::feature_at`, which must take the
    // DRAWN generation's copy out of a snapshot rather than this live one.
    // Handing out a borrow of the live index is precisely the mistake that
    // let clicks run a generation ahead of the mesh.

    /// Resolves a paint-only update against [`DEFAULT_SOURCE`].
    ///
    /// `#[cfg(test)]`, along with `plan`, `snapshot`, `index_of` and
    /// `resolve_id` above: every non-test caller inside the crate now names
    /// its source, because every one of them has a source to name. These
    /// remain because this crate's own forty-odd overlay tests were written
    /// against a store with one collection in it, and rewriting them to say
    /// `DEFAULT_SOURCE` forty times would change what they exercise --
    /// which, in the change that introduces sources, is exactly the wrong
    /// thing to do to the tests grading it.
    #[cfg(test)]
    pub(crate) fn plan_paint_update(
        &self,
        updates: &[(FeatureId, FeaturePaint)],
    ) -> Result<PaintUpdate, FeatureError> {
        self.plan_source_paint_update(DEFAULT_SOURCE, updates)
    }

    /// Resolves a paint-only update: which features of `name` it names,
    /// which ops those features produced, and whether it can be applied in
    /// place at all.
    ///
    /// Mutates nothing. An unknown id fails the whole call here, before any
    /// feature has been written -- the same all-or-nothing rule a push
    /// keeps, and for the same reason: half an update applied is worse than
    /// none, and the caller's next move is to fix their ids and push again.
    /// A name that is not a source fails the same way.
    ///
    /// Two things force `rebake`, both because they change more than the
    /// paint of the ops they name:
    ///
    /// - **The bake bracket flips.** `shared_width_ramp` returns one
    ///   bracket for the whole SOURCE, because a source is a single draw
    ///   call with one `blend_t` uniform. A `line-width` ramp that stops
    ///   agreeing with the others therefore rebakes every feature's width
    ///   in that source -- and, now, only in that source. Compared as
    ///   `shared_width_ramp` before against after, rather than against the
    ///   plan's stored bracket: a `Bucket` bracket also moves with the
    ///   camera (see `MapView::refresh_overlay_bake_zoom`), and a camera
    ///   that has since moved is not a reason to re-tessellate anything.
    /// - **A dot appears or disappears.** `circle-radius: 0` is not a paint
    ///   value but "draw no dot", and `build_plan` omits the op outright.
    ///   Crossing that boundary changes the op SET, and an in-place rewrite
    ///   can only change an op's contents.
    pub(crate) fn plan_source_paint_update(
        &self,
        name: &str,
        updates: &[(FeatureId, FeaturePaint)],
    ) -> Result<PaintUpdate, FeatureError> {
        let source = self.source(name).ok_or_else(|| unknown_source(name))?;

        let mut features = Vec::with_capacity(updates.len());
        for (id, _) in updates {
            features.push(self.source_resolve_id(name, id)?);
        }

        let mut ops = Vec::new();
        if let Some(plan) = &source.plan {
            for &index in &features {
                ops.extend(plan.feature_ops[index].clone());
            }
            ops.sort_unstable();
            ops.dedup();
        }

        // The `circle-radius` boundary, per feature.
        let dot_appears_or_disappears = updates.iter().zip(&features).any(|((_, paint), &index)| {
            matches!(source.features[index].geometry, OverlayGeometry::Point(_))
                && (source.features[index].paint.circle_radius > 0.0) != (paint.circle_radius > 0.0)
        });

        // The bracket, over the source as it WOULD be. Cheap: it reads one
        // `ZoomValue` per feature and no geometry at all.
        let bracket_flips = source.plan.is_some() && {
            let before = plan::shared_width_ramp(&source.features);
            let mut widths: Vec<&crate::style::ZoomValue<f32>> =
                source.features.iter().map(|feature| &feature.paint.line_width).collect();
            for ((_, paint), &index) in updates.iter().zip(&features) {
                widths[index] = &paint.line_width;
            }
            plan::shared_width_ramp_of(widths.into_iter()) != before
        };

        // A MERGED plan has no fast path at all, and cannot: one op is
        // shared by every feature that resolved to the same paint, so an
        // in-place vertex rewrite of that op would recolour all of them
        // from whichever one the lookup happened to find. Rebaking is the
        // correct answer rather than a conservative one -- the op set
        // itself changes when a feature's paint moves it into a different
        // group, or creates a group of one, which is exactly what an
        // in-place rewrite cannot express.
        //
        // Note also that on a data-driven source the COLOURS in `updates`
        // are ignored by the rebuild: the source's expression is what
        // paints, and the feature's own colour is only its fallback. The
        // rest of the update -- width, dot radius, dash, label -- applies
        // normally. See `declare_source_paint`.
        let merged = source.plan.as_ref().is_some_and(|plan| plan.data_driven);
        // NOT forced by a `line-gradient`: the in-place patch re-colours
        // the ranges it rewrote by progress (`OverlayPlan::colour_ranges`),
        // so a story highlight on a gradient route stays a vertex rewrite.

        Ok(PaintUpdate {
            source: Arc::clone(&source.name),
            features,
            ops,
            rebake: dot_appears_or_disappears || bracket_flips || merged,
        })
    }

    /// Applies a resolved update to the features and to the ops it names,
    /// and to nothing else -- in the source the update was resolved
    /// against, which it carries.
    ///
    /// THE GENERATION DOES NOT MOVE, this source's or the store's. No bake
    /// is spawned, so nothing is pending; and the source's stamp is part of
    /// the key `MapView::paint` caches its GPU upload under, so bumping it
    /// would force a full re-upload of that source's whole vertex buffer --
    /// the one cost this path exists to avoid.
    ///
    /// Only legitimate when `update.needs_rebake()` is false; the plan's op
    /// list is assumed to still describe the features, which is exactly
    /// what that flag denies.
    pub(crate) fn apply_paint_in_place(
        &mut self,
        updates: &[(FeatureId, FeaturePaint)],
        update: &PaintUpdate,
    ) {
        debug_assert!(!update.rebake, "an update that needs a rebake cannot be applied in place");
        let Some(source) = self.source_mut(&update.source) else { return };
        source.write_paint(updates, update);
        // Resolved at the plan's OWN bracket, not at a fresh one: every
        // other op in this plan holds values baked at that bracket, and one
        // op resolved at another would be blended by a weight that does not
        // belong to it.
        let Some(built) = &mut source.plan else { return };
        let bracket = built.bracket;
        for &op in &update.ops {
            // The op belongs to exactly one feature, and `feature_ops` is
            // what says which -- looking it up there is what makes this
            // correct rather than a guess about op ordering.
            let feature = update
                .features
                .iter()
                .copied()
                .find(|&index| built.feature_ops[index].contains(&op))
                .expect("every op in an update comes from one of its features");
            plan::repaint_op(&mut built.plan.ops[op], &source.features[feature], bracket);
            // Repainted at the source's width scale, as its plan was built.
            let k = source.paint.width_scale();
            if k != 1.0 {
                plan::scale_op_widths(&mut built.plan.ops[op], k);
            }
        }
    }

    /// Bakes every line width (and dot diameter) of whole-source `name` at
    /// `scale` times its declared value (`SourcePaint::width_scale`) from
    /// now on, re-planning the source when the scale changes (a new
    /// generation for a bake to carry). The live map's story layers under
    /// a story frame (spec §13, ruling R11-b). `false` when nothing
    /// changed or there is no such source. A later `declare_source_paint`
    /// compiles a fresh paint at scale 1.
    pub(crate) fn set_source_width_scale(&mut self, name: &str, scale: f32, display_zoom: u8) -> bool {
        let scale = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
        let stamp = self.generation + 1;
        let Some(source) = self.source_mut(name) else { return false };
        if source.paint.width_scale() == scale {
            return false;
        }
        let mut paint = (*source.paint).clone();
        paint.width_scale = scale;
        let paint = Arc::new(paint);
        source.paint = Arc::clone(&paint);
        if source.features.is_empty() {
            return true;
        }
        // The plan's own bracket when it has one: only the widths move.
        let bracket = source
            .plan
            .as_ref()
            .map(|p| p.bracket)
            .or_else(|| plan::shared_width_ramp(&source.features))
            .unwrap_or(plan::BakeBracket::Bucket(display_zoom));
        source.plan = source_plan_for(&source.features, &source.properties, bracket, &paint, source.tiled);
        source.stamp(stamp);
        self.generation = stamp;
        crate::render::alloc::note_overlay_generation(self.generation);
        true
    }

    /// Applies a resolved update and rebuilds everything downstream of it
    /// IN ITS OWN SOURCE: a new bracket, a new plan, a new stamp for a bake
    /// to carry.
    ///
    /// The slow path, for the two cases `plan_source_paint_update` flags and
    /// for `MapView`'s own (a drawn mesh that does not belong to this
    /// generation). It is a push in everything but the parse: same
    /// features, same geometry, same hit index -- only the paint and
    /// therefore the plan differ.
    pub(crate) fn apply_paint_and_replan(
        &mut self,
        updates: &[(FeatureId, FeaturePaint)],
        update: &PaintUpdate,
        display_zoom: u8,
    ) {
        let stamp = self.generation + 1;
        let Some(source) = self.source_mut(&update.source) else { return };
        source.write_paint(updates, update);
        let bracket = plan::shared_width_ramp(&source.features)
            .unwrap_or(plan::BakeBracket::Bucket(display_zoom));
        let paint = Arc::clone(&source.paint);
        source.plan = source_plan_for(&source.features, &source.properties, bracket, &paint, source.tiled);
        source.stamp(stamp);
        self.generation = stamp;
        crate::render::alloc::note_overlay_generation(self.generation);
    }

    /// Replaces [`DEFAULT_SOURCE`] with the given GeoJSON
    /// `FeatureCollection`.
    ///
    /// Whole-state, not incremental: what this is given is what is in that
    /// source afterwards, and an empty collection is how a caller clears
    /// it. Other sources are not touched, read, or re-planned.
    ///
    /// Everything is parsed and validated BEFORE any field is written, so a
    /// rejected call leaves the previously drawn features exactly as they
    /// were. That ordering is the entire contract; moving a mutation above
    /// the last fallible step breaks it.
    ///
    /// `display_zoom` is the caller's `MapView::display_zoom()`, and is used
    /// ONLY as the fallback bracket for a source whose widths have no
    /// shared linear ramp. In that case it is not a property of the
    /// features and does not survive as one: the camera moves, and
    /// `OverlaySnapshot::plan_at` re-resolves the very same features at
    /// whatever bucket the camera has reached. Passing today's bucket in
    /// merely spares the first frame after a push an immediate re-resolve.
    /// When the widths DO share a ramp -- the common case, and the only one
    /// in this crate's own example -- the bracket comes from the expression
    /// and this argument is ignored entirely.
    pub fn replace(&mut self, geojson: &str, display_zoom: u8) -> Result<(), FeatureError> {
        self.set_source(DEFAULT_SOURCE, geojson, display_zoom)
    }

    /// `replace`, for a caller who has the features already.
    ///
    /// Skips `parse_feature_collection` and `FeaturePaint::from_properties`
    /// -- the two stages that exist only because the other door speaks
    /// strings, and 74% of the synchronous cost of a push on this crate's
    /// own 28.8 MB dataset -- and joins the pipeline at the first stage
    /// that does not care where the data came from. `install` is that
    /// stage, and it is SHARED rather than mirrored.
    ///
    /// The contract is `replace`'s, unchanged: whole-state, all-or-nothing,
    /// an empty slice clears, and a rejected call leaves the previously
    /// drawn features exactly as they were.
    ///
    /// It cannot return `FeatureError::Json` or `NotAFeatureCollection` --
    /// there is no document to be malformed -- but it returns
    /// `FeatureError::Feature` with the same by-`id` naming, because a
    /// caller pushing a hundred vehicles still needs to know which one was
    /// refused.
    pub fn replace_from(
        &mut self,
        features: &[input::FeatureInput<'_>],
        display_zoom: u8,
    ) -> Result<(), FeatureError> {
        self.set_source_from(DEFAULT_SOURCE, features, display_zoom)
    }

    /// Declares what a source promises beyond its features, creating an
    /// empty source if this is the first mention of the name.
    ///
    /// STICKY. The declaration is checked at the moment it is made -- so a
    /// source that is already not zoom-independent is refused now, naming
    /// the feature responsible -- and then again on every subsequent push
    /// to that name. That is what lets the hot path stay two arguments:
    /// declare once at startup, push forever, and hear about it at the push
    /// that breaks the promise rather than in a frame-time graph next week.
    ///
    /// Changes NO generation and spawns no bake. A declaration cannot
    /// change what was baked -- the bake bracket is whatever
    /// `plan::zoom_dependence` says about the features, declared or not --
    /// it only changes whether a future push that denies it is accepted.
    pub fn declare_source(
        &mut self,
        name: &str,
        options: SourceOptions,
    ) -> Result<(), FeatureError> {
        if let Some(source) = self.source(name) {
            check_declaration(name, &source.features, options)?;
            check_gradient_has_metrics(name, &source.paint, options.line_metrics)?;
            // Not `source` -- that borrow is immutable and the check above
            // needed it.
            let source = self.source_mut(name).expect("just found");
            source.options = options;
            mirror_line_metrics(&mut source.paint, options.line_metrics);
            return Ok(());
        }
        let mut created = Source::empty(name.into());
        created.options = options;
        mirror_line_metrics(&mut created.paint, options.line_metrics);
        self.sources.push(created);
        Ok(())
    }

    /// MapLibre's GeoJSON `lineMetrics` for source `name`, creating an
    /// empty source if this is the first mention of the name: sets
    /// [`SourceOptions::line_metrics`] and leaves every other option as
    /// declared. STICKY, and it governs the NEXT push, exactly like
    /// [`Self::declare_source`] -- a mesh tessellated without progress
    /// cannot gain it without a re-push.
    ///
    /// Refused (changing nothing) when the source already holds more than
    /// the tiling threshold's points, and when turning it OFF under a
    /// declared `line-gradient`, which needs it.
    pub fn declare_source_line_metrics(&mut self, name: &str, on: bool) -> Result<(), FeatureError> {
        let options = SourceOptions { line_metrics: on, ..self.source_options(name).unwrap_or_default() };
        self.declare_source(name, options)
    }

    /// What a source currently promises, or `None` if no such source
    /// exists.
    pub fn source_options(&self, name: &str) -> Option<SourceOptions> {
        self.source(name).map(|source| source.options)
    }

    /// Declares a source's paint as EXPRESSIONS over its features' own
    /// properties, creating an empty source if this is the first mention of
    /// the name.
    ///
    /// `paint` is a style-spec paint object, with Mapbox's own property
    /// names -- `{"fill-color": ["get", "route_colour"]}`, or
    /// `{"line-color": ["match", ["get", "route"], "12", "#e6194b",
    /// "#888888"]}`. Only `line-color`, `fill-color` and `circle-color` are
    /// accepted; anything else is refused naming the key. See
    /// `source_paint::SourcePaint::compile`.
    ///
    /// STICKY, and it changes what a LATER push builds. Like
    /// [`Self::declare_source`] it moves no generation and bakes nothing --
    /// a declaration cannot repaint a mesh that is already on screen, and
    /// pretending it could would mean rewriting vertices from a plan the
    /// mesh was not tessellated from. Declare at startup, push forever. A
    /// caller who declares against a source that already holds features
    /// must push again for the declaration to reach the screen; that is one
    /// call, and it is the same call they were going to make next tick.
    ///
    /// # What declaring this changes about a source
    ///
    /// 1. **The colours it names come from the expression, not from each
    ///    feature.** A feature's own `FeaturePaint` colour becomes the
    ///    FALLBACK, used when no branch matches. Everything else in
    ///    `FeaturePaint` -- width, dot radius, dash, label, `min-zoom`,
    ///    icon -- stays per feature and is untouched.
    /// 2. **Its ops are merged.** Every feature that resolves to the same
    ///    paint collapses into ONE `PaintOp`, which is the point: 3,140
    ///    vehicles in twelve route colours become twelve ops rather than
    ///    3,140, while staying 3,140 features with 3,140 ids and 3,140 hit
    ///    targets. See `plan::build_plan_merged`, including what it says
    ///    about draw order.
    /// 3. **A paint-only update to it always rebakes.** A merged op is
    ///    shared, so there is no longer one feature whose colour an in-place
    ///    vertex rewrite could write. See
    ///    [`Self::plan_source_paint_update`].
    ///
    /// It cannot change whether the source is zoom-independent: a colour
    /// expression that varies with zoom is refused here, so the predicate in
    /// `plan::zoom_dependence` is unaffected by construction.
    ///
    /// `line-cap` / `line-join` (constants: `butt`/`round`/`square`,
    /// `bevel`/`round`/`miter`) stroke the source's lines; absent, a line
    /// keeps the overlay's `Butt` / `Bevel`. They are honoured on the
    /// whole-source plan only: declaring one on a source that is tiled, or
    /// pushing a source that declares one and would tile, is refused
    /// (`FeatureError::SourcePaint`) rather than dropped -- declare
    /// `SourceOptions::whole_source` (as every story layer does) or line
    /// metrics, which never tile. A shell that tiles every source
    /// (`Self::force_tiled`) draws them with the overlay's own stroke.
    pub fn declare_source_paint(
        &mut self,
        name: &str,
        paint: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), FeatureError> {
        // Compiled BEFORE anything is written, so a malformed expression
        // leaves the source exactly as it was -- the same all-or-nothing
        // rule every other door here keeps.
        let mut compiled = source_paint::SourcePaint::compile(paint)
            .map_err(|reason| FeatureError::SourcePaint { source: name.to_string(), reason })?;
        let line_metrics = self.source_options(name).unwrap_or_default().line_metrics;
        compiled.line_metrics = line_metrics;
        check_gradient_has_metrics(name, &compiled, line_metrics)?;
        if let Some(source) = self.source(name) {
            check_gradient_budget(name, &source.features, &compiled)?;
            check_stroke_is_whole_source(name, &compiled, source.tiled)?;
        }
        let compiled = Arc::new(compiled);
        match self.source_mut(name) {
            Some(source) => source.paint = compiled,
            None => {
                let mut created = Source::empty(name.into());
                created.paint = compiled;
                self.sources.push(created);
            }
        }
        Ok(())
    }

    /// The paint last declared for `name` (`declare_source_paint` or a
    /// paint update), as given; `None` if no such source exists. Empty for
    /// a source that never declared one.
    pub fn declared_source_paint(&self, name: &str) -> Option<&serde_json::Map<String, serde_json::Value>> {
        self.source(name).map(|source| &*source.paint.declared)
    }

    /// Declares symbols placed ALONG source `name`'s lines -- MapLibre's
    /// `symbol-placement: "line"` -- creating an empty source if this is
    /// the first mention of the name. `None`, or an empty object, removes
    /// the declaration.
    ///
    /// `layout` is a MapLibre symbol-layer `layout` object restricted to
    /// `symbol-placement` (`"line"`, required), `symbol-spacing` (logical
    /// px; a number or a zoom expression; default 250), `icon-image` (a
    /// sprite id string, required), `icon-rotation-alignment` (`"map"` or
    /// `"auto"`), `icon-size` (a number or a zoom expression),
    /// `icon-allow-overlap`, `icon-ignore-placement`, `minzoom` and
    /// `maxzoom`. Anything else -- an unknown key, another placement, a
    /// data-driven `icon-image`, `"viewport"` alignment -- is refused
    /// (`FeatureError::SourcePaint`, its reason starting `line symbols:`)
    /// and changes nothing. See `line_symbols::LineSymbolLayout::compile`.
    ///
    /// STICKY, and it governs the NEXT push, exactly like
    /// [`Self::declare_source_paint`]: it moves no generation and bakes
    /// nothing. The anchors are laid out off the UI thread, per integer
    /// zoom, from the lines the push installed (`line_symbols`), never at
    /// the push and never per frame.
    ///
    /// Draw order is fixed: a source's line symbols draw above its own
    /// lines and below its own point symbols (labels and icons).
    pub fn declare_source_symbols(
        &mut self,
        name: &str,
        layout: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> Result<(), FeatureError> {
        let compiled = match layout.filter(|layout| !layout.is_empty()) {
            Some(layout) => Some(Arc::new(
                line_symbols::LineSymbolLayout::compile(layout)
                    .map_err(|reason| FeatureError::SourcePaint { source: name.to_string(), reason })?,
            )),
            None => None,
        };
        match self.source_mut(name) {
            Some(source) => source.line_symbols = compiled,
            None => {
                if compiled.is_none() {
                    return Ok(());
                }
                let mut created = Source::empty(name.into());
                created.line_symbols = compiled;
                self.sources.push(created);
            }
        }
        Ok(())
    }

    /// The line-symbol layout last declared for `name`
    /// ([`Self::declare_source_symbols`]), as given; `None` when there is
    /// no such source or it declares none.
    pub fn declared_source_symbols(&self, name: &str) -> Option<&serde_json::Map<String, serde_json::Value>> {
        self.source(name).and_then(|source| source.line_symbols.as_deref()).map(|layout| &layout.declared)
    }

    /// Whether this source's paint is an expression over feature
    /// properties, or `None` if no such source exists.
    pub fn source_is_data_driven(&self, name: &str) -> Option<bool> {
        self.source(name).map(|source| !source.paint.is_empty())
    }

    /// How many `PaintOp`s this source's current plan holds, or `None` if
    /// no such source exists.
    ///
    /// The number data-driven paint exists to collapse, exposed so a host
    /// can watch it on its own data rather than inferring it from a frame
    /// graph. For a resolved-paint source it is "one per drawable feature";
    /// for a data-driven one it is "one per distinct resolved paint", and
    /// the ratio between the two is the whole measurement. A source with
    /// nothing drawable reports 0.
    pub fn source_op_count(&self, name: &str) -> Option<usize> {
        self.source(name)
            .map(|source| source.plan.as_ref().map_or(0, |plan| plan.plan.ops.len()))
    }

    /// Replaces ONE named source with the given GeoJSON
    /// `FeatureCollection`, creating it if this is the first push to that
    /// name.
    ///
    /// Everything `replace` promises, scoped to one source: whole-state,
    /// all-or-nothing, an empty collection clears that source, a rejected
    /// call changes nothing. And one promise `replace` could not make --
    /// **no other source is planned, tessellated, indexed or uploaded.**
    ///
    /// A new name is appended, so it draws ON TOP of every source pushed
    /// before it. A name that already exists keeps its place in draw order,
    /// which is what lets a feed re-push itself every tick without
    /// shuffling.
    pub fn set_source(
        &mut self,
        name: &str,
        geojson: &str,
        display_zoom: u8,
    ) -> Result<(), FeatureError> {
        let parsed = parse_feature_collection(geojson)?;
        if let Some(options) = self.source_cluster_options(name) {
            let mut lon_lat = Vec::with_capacity(parsed.len());
            let mut ids = Vec::with_capacity(parsed.len());
            let mut paints = Vec::with_capacity(parsed.len());
            for (index, feature) in parsed.iter().enumerate() {
                let paint = FeaturePaint::from_properties(&feature.properties).map_err(|reason| {
                    FeatureError::Feature { index, id: feature.id.clone(), reason }
                })?;
                lon_lat.push(single_point(index, &feature.id, &feature.geometry)?);
                ids.push(feature.id.clone());
                paints.push(paint);
            }
            let properties = PropertyTable::from_json(parsed.iter().map(|feature| &feature.properties));
            return self.install_points(name, lon_lat, Some(ids), &|i| &paints[i], properties, options);
        }

        // Every source keeps its properties, declared or not: a declared
        // paint reads them, and so does `MapView::pick`, whose card is a
        // host's hover popup. Skipping them for an undeclared source (when
        // they were a hash map per feature) left every plain GeoJSON
        // polygon and line with an empty card -- the delivery page's
        // hexagons showed a title and no count (2026-10-12). Since
        // `PropertyTable` went columnar they cost a typed column per key.

        // Annotated because `HitIndex::build(&features)` below would
        // otherwise let inference settle on the unsized slice it borrows.
        let mut features: Vec<OverlayFeature> = Vec::with_capacity(parsed.len());
        for (index, feature) in parsed.iter().enumerate() {
            let paint = FeaturePaint::from_properties(&feature.properties).map_err(|reason| {
                FeatureError::Feature { index, id: feature.id.clone(), reason }
            })?;
            features.push(OverlayFeature { index, id: feature.id.clone(), geometry: OverlayGeometry::Empty, paint });
        }
        let properties = PropertyTable::from_json(parsed.iter().map(|feature| &feature.properties));
        for (feature, parsed) in features.iter_mut().zip(parsed) {
            feature.geometry = parsed.geometry;
        }

        self.install(name, features, properties, display_zoom)
    }

    /// `set_source`, for a caller who has the features already. The
    /// structured door, and the one a live feed should use -- see
    /// `replace_from` for why there is no serialisation here.
    pub fn set_source_from(
        &mut self,
        name: &str,
        features: &[input::FeatureInput<'_>],
        display_zoom: u8,
    ) -> Result<(), FeatureError> {
        if let Some(options) = self.source_cluster_options(name) {
            return self.install_point_inputs(name, features.iter(), PropertyTable::default(), options);
        }
        let mut built: Vec<OverlayFeature> = Vec::with_capacity(features.len());
        for (index, feature) in features.iter().enumerate() {
            built.push(read_feature(index, feature)?);
        }
        self.install(name, built, PropertyTable::default(), display_zoom)
    }

    /// `set_source_from`, for a source whose paint is an expression over
    /// feature properties -- see [`Self::declare_source_paint`].
    ///
    /// Identical in every respect but one: each feature also hands in the
    /// properties its source's expressions read. Same validation, same
    /// all-or-nothing contract, same `install`, same everything past the
    /// read -- `read_feature` is shared rather than mirrored, because two
    /// doors that validate differently is the one failure this design can
    /// actually suffer.
    ///
    /// Legal on a source with NO declaration, and then it behaves exactly
    /// as `set_source_from` does: the properties are stored and nothing
    /// reads them. That is deliberate -- it lets a caller push properties
    /// before declaring the expression that will use them, rather than
    /// having the two calls constrain each other's order.
    pub fn set_source_data_from(
        &mut self,
        name: &str,
        features: &[input::DataFeatureInput<'_>],
        display_zoom: u8,
    ) -> Result<(), FeatureError> {
        if let Some(options) = self.source_cluster_options(name) {
            let properties = PropertyTable::from_maps(features.iter().map(|entry| &*entry.properties));
            return self.install_point_inputs(name, features.iter().map(|entry| &entry.feature), properties, options);
        }
        let mut built: Vec<OverlayFeature> = Vec::with_capacity(features.len());
        for (index, entry) in features.iter().enumerate() {
            built.push(read_feature(index, &entry.feature)?);
        }
        let properties = PropertyTable::from_maps(features.iter().map(|entry| &*entry.properties));
        self.install(name, built, properties, display_zoom)
    }

    /// `set_source_data_from`, with the properties handed in as typed
    /// COLUMNS rather than one map per feature: `columns[k].1[i]` is
    /// feature `i`'s value for key `columns[k].0`, `None` where it has
    /// none. No per-feature map is ever built.
    ///
    /// Every column must be exactly `features.len()` long and no key may
    /// repeat; either mistake is refused (as `FeatureError::SourcePaint`,
    /// naming the source) before anything is written.
    pub fn set_source_columns(
        &mut self,
        name: &str,
        features: &[input::FeatureInput<'_>],
        columns: &[(&str, PropertyColumn<'_>)],
        display_zoom: u8,
    ) -> Result<(), FeatureError> {
        let properties = PropertyTable::from_columns(features.len(), columns)
            .map_err(|reason| FeatureError::SourcePaint { source: name.to_string(), reason })?;
        if let Some(options) = self.source_cluster_options(name) {
            return self.install_point_inputs(name, features.iter(), properties, options);
        }
        let mut built: Vec<OverlayFeature> = Vec::with_capacity(features.len());
        for (index, feature) in features.iter().enumerate() {
            built.push(read_feature(index, feature)?);
        }
        self.install(name, built, properties, display_zoom)
    }

    /// One feature's properties, as a map built on demand: what a caller
    /// reads for the feature a `FeatureEvent` names. `None` for an unknown
    /// source or an index past its features; a feature with no properties
    /// gets the shared empty map.
    ///
    /// Built fresh rather than stored: one small map per hover or click is
    /// microseconds at event rates, where a map per FEATURE held for the
    /// source's life was the cost the columns remove.
    pub fn feature_properties(&self, name: &str, index: usize) -> Option<Arc<FeatureProperties>> {
        let source = self.source(name)?;
        if index >= source.features.len() || source.properties.is_dead(index) {
            return None;
        }
        Some(row_properties(&source.properties, index))
    }

    /// Removes a source and everything built from it. `true` if there was
    /// one.
    ///
    /// [`DEFAULT_SOURCE`] is CLEARED rather than removed -- it is the
    /// source the pre-sources API writes and reads, and making it
    /// disappear would turn `features()` into a lie about a store that
    /// still works. Clearing it is what `replace` with an empty collection
    /// does, so this agrees with that.
    ///
    /// Any bake of this source still tessellating is cancelled: the new
    /// stamp is published into the atomic BEFORE the source is dropped, and
    /// the bake holds an `Arc` to it. See `Source::live_generation`.
    pub fn remove_source(&mut self, name: &str) -> bool {
        let Some(at) = self.sources.iter().position(|source| &*source.name == name) else {
            return false;
        };
        let stamp = self.generation + 1;
        // Published before the `Source` goes away, which is the whole
        // reason this is not just a `Vec::remove`: a bake of this source
        // polls that atomic through its own `Arc` clone, so a removal that
        // dropped the source without moving the number would leave the bake
        // tessellating happily and then be refused at the door -- correct,
        // but after paying for it.
        self.sources[at].stamp(stamp);
        if &*self.sources[at].name == DEFAULT_SOURCE {
            // Emptied in place rather than replaced, so the name's
            // `Arc<str>`, the `live_generation` a bake is polling, and the
            // declaration a caller made all survive -- the source is
            // cleared, not reset.
            let source = &mut self.sources[at];
            source.features = Arc::default();
            source.properties = PropertyTable::empty();
            source.plan = None;
            source.hit_index = Arc::default();
            source.by_id = HashMap::new();
            source.points = None;
            source.next_row = 0;
        } else {
            self.sources.remove(at);
        }
        self.generation = stamp;
        crate::render::alloc::note_overlay_generation(self.generation);
        true
    }

    /// Puts the named sources in `names`' order, BOTTOM FIRST, among the
    /// positions they hold now: every other source keeps its place, so a
    /// host that orders only its own sources never moves another's. Names
    /// no source has are skipped, as are repeats and [`DEFAULT_SOURCE`]
    /// (always first). `true` if any source moved.
    ///
    /// MapLibre's `Style::moveLayer(id, before)` splices one layer into
    /// one ordered list; this takes the whole wanted order at once, which
    /// is what a host mirroring its own layer list needs and is
    /// idempotent: calling it after every push costs a scan of a
    /// single-digit list and moves nothing when nothing changed. Nothing
    /// re-bakes: draw order is read from this list every frame.
    pub fn set_source_order(&mut self, names: &[&str]) -> bool {
        let picks: Vec<usize> = names
            .iter()
            .filter(|name| **name != DEFAULT_SOURCE)
            .filter_map(|name| self.sources.iter().position(|source| &*source.name == *name))
            .collect();
        permute_among_slots(&mut self.sources, picks)
    }

    /// Everything a push does once its features exist, whichever door they
    /// came in by.
    ///
    /// SHARED, not duplicated. `set_source` and `set_source_from` differ
    /// only in how they obtain a `Vec<OverlayFeature>`; past that point
    /// every difference between them would be a bug, and the id map is the
    /// sharpest case -- two maps that can disagree about which feature owns
    /// an id is exactly what `OverlayStore::source_resolve_id` would then
    /// answer wrongly, silently, on one path only.
    ///
    /// Fallible, and mutates nothing until every fallible step is behind
    /// it: that ordering IS the all-or-nothing contract, and moving a
    /// mutation above the duplicate-id scan would break it.
    fn install(
        &mut self,
        name: &str,
        features: Vec<OverlayFeature>,
        properties: PropertyTable,
        display_zoom: u8,
    ) -> Result<(), FeatureError> {
        // Whatever this source already promises -- see `declare_source`.
        // `default()` for a name nobody has mentioned before, which
        // promises nothing and so refuses nothing.
        let options = self.source_options(name).unwrap_or_default();
        // Two features sharing an id make the SOURCE malformed: an update
        // naming that id has no answer, and picking either one silently
        // recolours a coin flip. Refused here, where the caller can see
        // which feature did it, rather than at update time, where all that
        // is left is the id. Ids need only be unique WITHIN a source --
        // two sources may each have a feature `"42"`, which is why a
        // `FeatureRef` carries the source name.
        let mut by_id = HashMap::with_capacity(features.len());
        for feature in &features {
            let Some(id) = &feature.id else { continue };
            if by_id.insert(IdKey::of(id), feature.index).is_some() {
                return Err(FeatureError::Feature {
                    index: feature.index,
                    id: feature.id.clone(),
                    reason: "duplicate id -- ids must be unique within a source".into(),
                });
            }
        }

        // THE predicate, and the declaration graded against it. Refusing
        // here, before anything is written, is what makes the declaration
        // all-or-nothing like every other guard: a source that cannot keep
        // the promise is not half-installed with the promise dropped.
        //
        // Fail CLOSED on the other side of it: a source nobody promised
        // anything about, whose widths deny a fixed bracket, is baked at
        // the camera's bucket and re-resolved at every crossing. Slower,
        // and correct.
        check_declaration(name, &features, options)?;
        if let Some(source) = self.source(name) {
            check_gradient_budget(name, &features, &source.paint)?;
        }
        let bracket = plan::zoom_dependence(&features)
            .unwrap_or(plan::BakeBracket::Bucket(display_zoom));

        // STICKY, exactly as `options` is: a declaration made once at
        // startup governs every later push to the name. `default()` --
        // "nothing declared" -- for a name nobody has mentioned, which puts
        // `build_plan` on the original one-op-per-feature path.
        let paint = self
            .source(name)
            .map(|source| Arc::clone(&source.paint))
            .unwrap_or_default();
        let name: SourceId = match self.source(name) {
            // Reuse the existing `Arc<str>`, so every `FeatureRef` this
            // source has ever handed out shares one allocation with it
            // rather than one per push.
            Some(existing) => Arc::clone(&existing.name),
            None => name.into(),
        };
        let mut features = features;
        if paint.circle_radius_declared() {
            for feature in &mut features {
                if matches!(feature.geometry, OverlayGeometry::Point(_)) {
                    if let Some(r) = paint.circle_radius(&properties.row_map(feature.index)) {
                        feature.paint.circle_radius = r;
                    }
                }
            }
        }
        let tiled = !options.whole_source && tile_plan::should_tile(&features, options.line_metrics);
        check_stroke_is_whole_source(&name, &paint, tiled)?;
        let properties = Arc::new(properties);
        let plan = source_plan_for(&features, &properties, bracket, &paint, tiled);
        let hit_index = Arc::new(hit::HitIndex::build(&name, &features));
        let stamp = self.generation + 1;

        // Everything fallible is behind us; only now does the store change.
        let at = match self.sources.iter().position(|source| source.name == name) {
            Some(at) => at,
            None => {
                self.sources.push(Source::empty(Arc::clone(&name)));
                self.sources.len() - 1
            }
        };
        let source = &mut self.sources[at];
        source.hit_index = hit_index;
        source.next_row = features.len();
        source.features = Arc::new(features);
        source.properties = properties;
        source.by_id = by_id;
        source.plan = plan;
        source.options = options;
        source.paint = paint;
        source.line_symbols_pushed = source.line_symbols.clone();
        source.tiled = tiled;
        source.tile_index = None;
        source.points = None;
        source.stamp(stamp);
        self.generation = stamp;
        crate::render::alloc::note_overlay_generation(self.generation);
        Ok(())
    }

    /// Takes a tiled source back to the whole-source path, at a new stamp:
    /// what a tiled source falls back to when its paints cannot be
    /// classed (`plan::classify` returns `None` past 65,534 classes).
    /// `false` when there is no such source or it is not tiled.
    pub(crate) fn force_untiled(&mut self, name: &str, display_zoom: u8) -> bool {
        let stamp = self.generation + 1;
        let Some(source) = self.source_mut(name) else { return false };
        if !source.tiled {
            return false;
        }
        let bracket = plan::zoom_dependence(&source.features)
            .unwrap_or(plan::BakeBracket::Bucket(display_zoom));
        // An adopted index holds the only geometry copy: the whole-source
        // path needs lon/lat back, so rebuild it from the index.
        if let Some(index) = source.tile_index.take() {
            source.features = Arc::new(index.restore_geometry(&source.features));
        }
        source.tiled = false;
        source.plan = build_plan(&source.features, &source.properties, bracket, &source.paint);
        source.stamp(stamp);
        self.generation = stamp;
        crate::render::alloc::note_overlay_generation(self.generation);
        true
    }

    /// [`Self::force_tiled`] for a host that opted a source in
    /// (`MapView::tile_source_for_paint_updates`): only a source of
    /// polygons and lines ONLY (a point feature would keep the source on
    /// the whole-source plan, so tiling it buys no table path), only when
    /// its paint classes (`plan::classify`; otherwise the tile-set build
    /// would `force_untiled` it straight back), and never under
    /// `GPUI_MAP_OVERLAY_TILING=never`. `false` changes nothing.
    pub fn force_tiled_if_classable(&mut self, name: &str, display_zoom: u8) -> bool {
        if std::env::var("GPUI_MAP_OVERLAY_TILING").as_deref() == Ok("never") {
            return false;
        }
        let Some(source) = self.source(name) else { return false };
        if source.tiled
            || source.paint.line_cap.is_some()
            || source.paint.line_join.is_some()
            || source.options.line_metrics
            || source.points.is_some()
            || source.features.is_empty()
            || !source
                .features
                .iter()
                .all(|f| matches!(f.geometry, OverlayGeometry::Polygon(_) | OverlayGeometry::Line(_)))
        {
            return false;
        }
        let bracket = plan::zoom_dependence(&source.features).unwrap_or(plan::BakeBracket::Bucket(display_zoom));
        if plan::classify(&source.features, &source.properties, bracket, &source.paint).is_none() {
            return false;
        }
        self.force_tiled(name, display_zoom)
    }

    /// Puts an untiled source with polygon or line features on the tiled
    /// path, at a new stamp, whatever its size: a shell with no whole-source
    /// mesh path (`overlay::host::ShellOverlays`) draws every such source
    /// through its tile index. `false` when there is no such source, it is
    /// already tiled, it holds points, or it has nothing to tile.
    pub(crate) fn force_tiled(&mut self, name: &str, display_zoom: u8) -> bool {
        let stamp = self.generation + 1;
        let Some(source) = self.source_mut(name) else { return false };
        if source.tiled || source.points.is_some() {
            return false;
        }
        let tileable = source
            .features
            .iter()
            .any(|f| matches!(f.geometry, OverlayGeometry::Polygon(_) | OverlayGeometry::Line(_)));
        if !tileable {
            return false;
        }
        let bracket = plan::zoom_dependence(&source.features).unwrap_or(plan::BakeBracket::Bucket(display_zoom));
        source.tiled = true;
        source.plan = source_plan_for(&source.features, &source.properties, bracket, &source.paint, true);
        source.stamp(stamp);
        self.generation = stamp;
        crate::render::alloc::note_overlay_generation(self.generation);
        true
    }

    /// Makes `index` the ONE geometry copy of tiled source `name`: every
    /// polygon and line feature the index holds is rewritten to
    /// `OverlayGeometry::Indexed` (its lon/lat box and kind), and the
    /// lon/lat `Vec`s are freed when the last snapshot of the old feature
    /// vector drops. Ids, paint and properties are untouched, so a hit
    /// still answers the original feature. Points, and anything the index
    /// skipped, keep their geometry.
    ///
    /// The generation does NOT move: nothing drawn changes. A no-op for a
    /// source that is not tiled, or already holds an index.
    ///
    /// What `MapView` does when a tiled source's index lands, with the same
    /// `Arc` its tile set cuts from.
    pub fn adopt_tile_index(&mut self, name: &str, index: Arc<tile_index::TileIndex>) {
        let Some(source) = self.source_mut(name) else { return };
        if !source.tiled || source.tile_index.is_some() {
            return;
        }
        let features: Vec<OverlayFeature> = source
            .features
            .iter()
            .enumerate()
            .map(|(at, f)| {
                let geometry = match (&f.geometry, index.feature_bbox_lon_lat(at as u32)) {
                    (OverlayGeometry::Polygon(_), Some(bbox_lon_lat)) => {
                        OverlayGeometry::Indexed { bbox_lon_lat, line: false }
                    }
                    (OverlayGeometry::Line(_), Some(bbox_lon_lat)) => {
                        OverlayGeometry::Indexed { bbox_lon_lat, line: true }
                    }
                    (g, _) => g.clone(),
                };
                OverlayFeature {
                    index: f.index,
                    id: f.id.clone(),
                    geometry,
                    paint: f.paint.clone(),
                }
            })
            .collect();
        source.features = Arc::new(features);
        source.tile_index = Some(index);
    }
}

/// The live-update half of the store: see `live`'s module doc.
impl OverlayStore {
    /// Validates `diff` against source `name` and resolves its ids to rows.
    ///
    /// All-or-nothing: an added feature without an id, two adds sharing an
    /// id, or a geometry or paint a push would refuse fails the call with
    /// nothing written. Unknown ids in `remove_ids` and `update` are
    /// skipped and counted (MapLibre's rule).
    ///
    /// On the incremental path (a tiled source whose index has landed, with
    /// no point features) the id map and row counter move NOW, and the
    /// returned `ResolvedDiff` is what the caller applies (`live::apply`)
    /// and installs (`install_live`). Anything else is applied here as a
    /// push, at a new generation, and `None` is returned.
    pub(crate) fn accept_source_diff(
        &mut self,
        name: &str,
        diff: &SourceDiff<'_>,
        display_zoom: u8,
    ) -> Result<(LiveUpdate, Option<live::ResolvedDiff>), FeatureError> {
        let source = self.source(name).ok_or_else(|| unknown_source(name))?;
        if source.points.is_some() || source.cluster.is_some() {
            return Err(FeatureError::SourcePaint {
                source: name.to_string(),
                reason: "a point-path source is updated with set_source_points".into(),
            });
        }
        let mut added = Vec::with_capacity(diff.add.len());
        let mut seen = std::collections::HashSet::new();
        for (k, a) in diff.add.iter().enumerate() {
            let Some(id) = &a.feature.id else {
                return Err(FeatureError::Feature { index: k, id: None, reason: "an added feature needs an id".into() });
            };
            if !seen.insert(IdKey::of(id)) {
                return Err(FeatureError::Feature {
                    index: k,
                    id: Some(id.clone()),
                    reason: "duplicate id -- two adds in one diff share it".into(),
                });
            }
            added.push(read_feature(k, &a.feature)?);
        }
        for (k, u) in diff.update.iter().enumerate() {
            if let Some(g) = &u.geometry {
                g.validate()
                    .map_err(|reason| FeatureError::Feature { index: k, id: Some(u.id.clone()), reason })?;
            }
        }
        check_declaration(name, &added, source.options)?;
        let incremental = source.tiled
            && source.tile_index.is_some()
            && source.plan.is_none()
            && !added.iter().any(|f| matches!(f.geometry, OverlayGeometry::Point(_)))
            && !diff.update.iter().any(|u| matches!(u.geometry, Some(input::GeometryInput::Point(_))));

        // Everything fallible is behind us.
        let source = self.source_mut(name).expect("checked above");
        source.live_seq += 1;
        let seq = source.live_seq;
        if incremental {
            let (mut resolved, skipped) = resolve_diff(&mut source.by_id, &mut source.next_row, diff, added);
            resolved.seq = seq;
            return Ok((LiveUpdate { path: LivePath::Incremental, skipped, seq }, Some(resolved)));
        }
        // The push path: the post-diff collection, compacted, installed.
        let mut by_id = source.by_id.clone();
        let mut next_row = source.features.len();
        let (resolved, skipped) = resolve_diff(&mut by_id, &mut next_row, diff, added);
        let mut features: Vec<OverlayFeature> = match &source.tile_index {
            Some(index) => index.restore_geometry(&source.features),
            None => source.features.as_ref().clone(),
        };
        features.extend(resolved.add.iter().cloned());
        for (row, g) in &resolved.geometry {
            features[*row].geometry = g.clone();
        }
        let writes: Vec<columns::CellWrite<'_>> =
            resolved.writes.iter().map(|(r, k, v)| (*r, k.as_str(), v.as_ref())).collect();
        let properties = source.properties.edited(features.len(), &writes, &resolved.kill);
        let mut keep = vec![true; features.len()];
        for &row in &resolved.kill {
            keep[row] = false;
        }
        let properties = properties.compacted(&keep);
        let features: Vec<OverlayFeature> = features
            .into_iter()
            .zip(&keep)
            .filter(|(_, &k)| k)
            .enumerate()
            .map(|(at, (mut f, _))| {
                f.index = at;
                f
            })
            .collect();
        self.install(name, features, properties, display_zoom)?;
        Ok((LiveUpdate { path: LivePath::Push, skipped, seq }, None))
    }

    /// A tiled source's current state, for `live::apply`, with the paint
    /// classes and bracket its tile set holds. `None` without an index.
    pub(crate) fn live_base(
        &self,
        name: &str,
        classes: Arc<plan::FeatureClasses>,
        bracket: plan::BakeBracket,
    ) -> Option<live::LiveBase> {
        let source = self.source(name)?;
        Some(live::LiveBase {
            source: Arc::clone(&source.name),
            features: Arc::clone(&source.features),
            properties: Arc::clone(&source.properties),
            index: source.tile_index.clone()?,
            classes,
            hit_index: Arc::clone(&source.hit_index),
            paint: Arc::clone(&source.paint),
            bracket,
        })
    }

    /// Swaps an applied diff into source `name`, if it is still at
    /// `generation` (a push since supersedes it). The generation does not
    /// move: the tiles it touched re-bake in place.
    ///
    /// Returns what it displaced, for the caller to drop off the UI thread
    /// (a national feature vector is ~250k small blocks); `None` when it
    /// installed nothing.
    pub(crate) fn install_live(
        &mut self,
        name: &str,
        generation: u64,
        result: &live::LiveResult,
    ) -> Option<Box<dyn std::any::Any + Send>> {
        let source = self.source_mut(name)?;
        if source.generation != generation || source.tile_index.is_none() {
            return None;
        }
        let old = (
            std::mem::replace(&mut source.features, Arc::clone(&result.features)),
            std::mem::replace(&mut source.properties, Arc::clone(&result.properties)),
            std::mem::replace(&mut source.hit_index, Arc::clone(&result.hit_index)),
            source.tile_index.replace(Arc::clone(&result.index)),
        );
        if let Some(remap) = result.remap.as_deref() {
            // Rows compacted: the id map and the row counter follow (ids
            // accepted since the job started included). Once per
            // compaction, ~1 ms per 100k ids.
            for row in source.by_id.values_mut() {
                *row = live::remapped_row(remap, *row).expect("a live id names a kept row");
            }
            source.next_row = live::remapped_row(remap, source.next_row).expect("past every row");
        }
        Some(Box::new(old))
    }
}

/// Resolves a diff's ids to rows against `by_id`, moving it and
/// `next_row` as MapLibre's order says: removes, then adds (replacing an
/// existing id), then updates. `added[k]` is `diff.add[k]` read.
fn resolve_diff(
    by_id: &mut HashMap<IdKey, usize>,
    next_row: &mut usize,
    diff: &SourceDiff<'_>,
    added: Vec<OverlayFeature>,
) -> (live::ResolvedDiff, usize) {
    let mut r = live::ResolvedDiff::default();
    let mut skipped = 0;
    for id in &diff.remove_ids {
        match by_id.remove(&IdKey::of(id)) {
            Some(row) => r.kill.push(row),
            None => skipped += 1,
        }
    }
    for (a, mut f) in diff.add.iter().zip(added) {
        let key = IdKey::of(f.id.as_ref().expect("checked: every add has an id"));
        if let Some(row) = by_id.remove(&key) {
            r.kill.push(row);
        }
        let row = *next_row;
        *next_row += 1;
        f.index = row;
        by_id.insert(key, row);
        for (k, v) in &a.properties {
            r.writes.push((row, k.clone(), Some(v.clone())));
        }
        r.add.push(f);
    }
    for u in &diff.update {
        let Some(&row) = by_id.get(&IdKey::of(&u.id)) else {
            skipped += 1;
            continue;
        };
        if let Some(g) = u.geometry {
            r.geometry.push((row, g.to_geometry()));
        }
        for (k, v) in &u.properties {
            r.writes.push((row, k.clone(), v.clone()));
        }
    }
    (r, skipped)
}

/// The one position of a point-path feature: a single-position `Point`,
/// and nothing else.
fn single_point(index: usize, id: &Option<FeatureId>, geometry: &OverlayGeometry) -> Result<[f64; 2], FeatureError> {
    match geometry {
        OverlayGeometry::Point(p) if p.len() == 1 => Ok([p[0].0, p[0].1]),
        _ => Err(FeatureError::Feature {
            index,
            id: id.clone(),
            reason: "a clustered or point-indexed source takes single-position Point features only".into(),
        }),
    }
}

/// The point path: `set_source_cluster_options`, the pushes it reroutes,
/// the index and view hand-offs `MapView` drives, and the cluster queries.
/// See `points`' module doc.
impl OverlayStore {
    /// Declares (or with `None` clears) the range filter column of point
    /// source `name` (`overlay::filter`): reads the column once and moves
    /// the source to a new revision WITH ITS INDEX KEPT, so its views are
    /// rebuilt carrying each point's value and nothing is re-pushed or
    /// re-indexed. Returns the column's `[min, max]`, or `None` when the
    /// source is not a point source or the column has no value.
    pub fn set_source_filter_field(&mut self, name: &str, field: Option<&str>) -> Option<[f64; 2]> {
        let fields: Vec<&str> = field.into_iter().collect();
        self.set_source_filter_fields(name, &fields).and_then(|d| d.first().copied())
    }

    /// [`Self::set_source_filter_field`] for up to
    /// [`filter::MAX_FILTER_FIELDS`] columns at once, packed into one value
    /// per point ([`filter::PointFilter::read_many`]). An empty `fields`
    /// clears the filter. Returns each field's `[min, max]`.
    pub fn set_source_filter_fields(&mut self, name: &str, fields: &[&str]) -> Option<Vec<[f64; 2]>> {
        let stamp = self.generation + 1;
        let source = self.source_mut(name)?;
        let state = source.points.as_mut()?;
        let old = &*state.data;
        let filter = filter::PointFilter::read_many(&old.properties, old.len(), fields).map(Arc::new);
        if old.filter.as_ref().map(|f| (&f.field, f.values.len())) == filter.as_ref().map(|f| (&f.field, f.values.len())) {
            return filter.map(|f| f.domains());
        }
        let domain = filter.as_ref().map(|f| f.domains());
        let data = points::PointData {
            lon_lat: old.lon_lat.clone(),
            ids: old.ids.clone(),
            paints: old.paints.clone(),
            paint_of: old.paint_of.clone(),
            properties: Arc::clone(&old.properties),
            options: old.options.clone(),
            filter,
            revision: stamp,
        };
        state.data = Arc::new(data);
        state.view = None;
        source.stamp(stamp);
        self.generation = stamp;
        domain
    }

    /// Declares (or, with `None`, withdraws) that `name` is a point-path
    /// source. Sticky. `Ok(true)` when the source already holds points whose
    /// index must be rebuilt for the new options.
    ///
    /// Withdrawing it from a source that holds points turns them back into
    /// ordinary features, at a new stamp.
    pub fn set_source_cluster_options(
        &mut self,
        name: &str,
        options: Option<cluster::ClusterOptions>,
        display_zoom: u8,
    ) -> Result<bool, FeatureError> {
        if let Some(options) = &options {
            options
                .validate()
                .map_err(|reason| FeatureError::SourcePaint { source: name.to_string(), reason })?;
        }
        if self.source(name).is_none() {
            self.sources.push(Source::empty(name.into()));
        }
        let stamp = self.generation + 1;
        let source = self.source_mut(name).expect("created above");
        source.cluster = options.clone();
        let Some(state) = source.points.take() else { return Ok(false) };
        match options {
            Some(options) => {
                let old = &*state.data;
                let data = points::PointData {
                    lon_lat: old.lon_lat.clone(),
                    ids: old.ids.clone(),
                    paints: old.paints.clone(),
                    paint_of: old.paint_of.clone(),
                    properties: Arc::clone(&old.properties),
                    options,
                    filter: old.filter.clone(),
                    revision: stamp,
                };
                source.points = Some(PointState { data: Arc::new(data), index: None, view: None, rows: Arc::default() });
                source.stamp(stamp);
                self.generation = stamp;
                Ok(true)
            }
            None => {
                let features: Vec<OverlayFeature> = (0..state.data.len()).map(|i| state.data.feature(i, i)).collect();
                let properties = (*state.data.properties).clone();
                self.install(name, features, properties, display_zoom)?;
                Ok(false)
            }
        }
    }

    /// The point-path declaration of `name`, if any.
    pub fn source_cluster_options(&self, name: &str) -> Option<cluster::ClusterOptions> {
        self.source(name)?.cluster.clone()
    }

    /// The lean point door: positions, optional ids, one paint for every
    /// point, and typed property columns. A source with no declaration is
    /// declared [`cluster::ClusterOptions::points_only`].
    pub fn set_source_points(
        &mut self,
        name: &str,
        lon_lat: &[[f64; 2]],
        ids: Option<&[Option<FeatureId>]>,
        paint: &FeaturePaint,
        columns: &[(&str, PropertyColumn<'_>)],
    ) -> Result<(), FeatureError> {
        if ids.is_some_and(|ids| ids.len() != lon_lat.len()) {
            return Err(FeatureError::SourcePaint {
                source: name.to_string(),
                reason: format!("{} ids for {} points", ids.map_or(0, <[_]>::len), lon_lat.len()),
            });
        }
        let properties = PropertyTable::from_columns(lon_lat.len(), columns)
            .map_err(|reason| FeatureError::SourcePaint { source: name.to_string(), reason })?;
        let options = match self.source_cluster_options(name) {
            Some(options) => options,
            None => {
                let options = cluster::ClusterOptions::points_only();
                self.set_source_cluster_options(name, Some(options.clone()), 0)?;
                options
            }
        };
        self.install_points(name, lon_lat.to_vec(), ids.map(<[_]>::to_vec), &|_| paint, properties, options)
    }

    fn install_point_inputs<'a, 'b: 'a>(
        &mut self,
        name: &str,
        features: impl Iterator<Item = &'a input::FeatureInput<'b>>,
        properties: PropertyTable,
        options: cluster::ClusterOptions,
    ) -> Result<(), FeatureError> {
        let features: Vec<&input::FeatureInput<'_>> = features.collect();
        let mut lon_lat = Vec::with_capacity(features.len());
        let mut ids = Vec::with_capacity(features.len());
        for (index, feature) in features.iter().enumerate() {
            let fail = |reason: String| FeatureError::Feature { index, id: feature.id.clone(), reason };
            feature.geometry.validate().map_err(fail)?;
            match feature.geometry {
                input::GeometryInput::Point([p]) => lon_lat.push(*p),
                _ => {
                    return Err(fail(
                        "a clustered or point-indexed source takes single-position Point features only".into(),
                    ))
                }
            }
            ids.push(feature.id.clone());
        }
        self.install_points(name, lon_lat, Some(ids), &|i| &features[i].paint, properties, options)
    }

    /// Everything a point push does once its columns exist: the index is
    /// NOT built here (that is `MapView`'s, off the UI thread); the source's
    /// drawn content is emptied until the first view lands.
    fn install_points<'p>(
        &mut self,
        name: &str,
        lon_lat: Vec<[f64; 2]>,
        ids: Option<Vec<Option<FeatureId>>>,
        paint_for: &'p dyn Fn(usize) -> &'p FeaturePaint,
        properties: PropertyTable,
        options: cluster::ClusterOptions,
    ) -> Result<(), FeatureError> {
        // The failing point's id is not recovered: `ids` moved into the
        // build, and cloning two million ids to name one would cost more
        // than the push.
        let mut data = points::PointData::build(lon_lat, ids, paint_for, properties, options)
            .map_err(|(index, reason)| FeatureError::Feature { index, id: None, reason })?;
        let stamp = self.generation + 1;
        data.revision = stamp;
        if self.source(name).is_none() {
            self.sources.push(Source::empty(name.into()));
        }
        let source = self.source_mut(name).expect("created above");
        source.features = Arc::default();
        source.properties = PropertyTable::empty();
        source.plan = None;
        source.hit_index = Arc::default();
        source.by_id = HashMap::new();
        source.tiled = false;
        source.tile_index = None;
        source.points = Some(PointState { data: Arc::new(data), index: None, view: None, rows: Arc::default() });
        source.stamp(stamp);
        self.generation = stamp;
        crate::render::alloc::note_overlay_generation(self.generation);
        Ok(())
    }

    /// Every point-path source's name.
    pub(crate) fn point_source_names(&self) -> Vec<SourceId> {
        self.sources.iter().filter(|s| s.points.is_some()).map(|s| Arc::clone(&s.name)).collect()
    }

    /// The data a point source's index must be built from, when it has no
    /// index yet.
    pub(crate) fn point_index_job(&self, name: &str) -> Option<Arc<points::PointData>> {
        let state = self.source(name)?.points.as_ref()?;
        state.index.is_none().then(|| Arc::clone(&state.data))
    }

    /// Adopts an index built for `revision`; `false` (and nothing kept) if
    /// the source has moved on.
    pub fn adopt_point_index(&mut self, name: &str, revision: u64, index: Arc<cluster::ClusterIndex>) -> bool {
        let Some(state) = self.source_mut(name).and_then(|s| s.points.as_mut()) else { return false };
        if state.data.revision != revision {
            return false;
        }
        state.index = Some(index);
        true
    }

    /// Whether `name`'s index has been built.
    pub fn source_point_index_ready(&self, name: &str) -> bool {
        self.source(name).and_then(|s| s.points.as_ref()).is_some_and(|p| p.index.is_some())
    }

    /// The point source's index, once built.
    pub fn source_point_index(&self, name: &str) -> Option<Arc<cluster::ClusterIndex>> {
        self.source(name)?.points.as_ref()?.index.clone()
    }

    /// The point source's data.
    pub fn source_point_data(&self, name: &str) -> Option<Arc<points::PointData>> {
        Some(Arc::clone(&self.source(name)?.points.as_ref()?.data))
    }

    /// The installed view's key.
    pub(crate) fn point_view_key(&self, name: &str) -> Option<points::ViewKey> {
        self.source(name)?.points.as_ref()?.view
    }

    /// Everything a view build needs, once the index is in.
    #[allow(clippy::type_complexity)]
    pub(crate) fn point_view_job(
        &self,
        name: &str,
    ) -> Option<(SourceId, Arc<points::PointData>, Arc<cluster::ClusterIndex>, Arc<source_paint::SourcePaint>)> {
        let source = self.source(name)?;
        let state = source.points.as_ref()?;
        Some((Arc::clone(&source.name), Arc::clone(&state.data), state.index.clone()?, Arc::clone(&source.paint)))
    }

    /// Installs a built view as the source's content, at a new stamp.
    /// `false` if the source's data moved on since the view was built.
    pub fn install_point_view(&mut self, name: &str, view: points::PointView) -> bool {
        let stamp = self.generation + 1;
        let Some(source) = self.source_mut(name) else { return false };
        let Some(state) = source.points.as_mut() else { return false };
        if state.data.revision != view.revision {
            return false;
        }
        state.view = Some(view.key);
        state.rows = Arc::new(view.rows);
        source.features = Arc::new(view.features);
        source.properties = Arc::new(view.properties);
        source.plan = view.plan;
        source.hit_index = Arc::new(view.hit_index);
        source.stamp(stamp);
        self.generation = stamp;
        crate::render::alloc::note_overlay_generation(self.generation);
        true
    }

    /// Forgets the installed view's key, so the next frame builds a fresh
    /// one (a paint change: the view's per-feature radius and labels are
    /// resolved at build time).
    pub(crate) fn invalidate_point_view(&mut self, name: &str) {
        if let Some(state) = self.source_mut(name).and_then(|s| s.points.as_mut()) {
            state.view = None;
        }
    }

    /// MapLibre's `getClusterExpansionZoom`. `None` for an unknown source,
    /// an index not built yet, or an id that is no cluster.
    pub fn cluster_expansion_zoom(&self, name: &str, cluster_id: u32) -> Option<u8> {
        self.source_point_index(name)?.expansion_zoom(cluster_id)
    }

    /// MapLibre's `getClusterChildren`: clusters, and leaves by their
    /// index in the pushed collection.
    pub fn cluster_children(&self, name: &str, cluster_id: u32) -> Option<Vec<cluster::ClusterItem>> {
        self.source_point_index(name)?.children(cluster_id)
    }

    /// MapLibre's `getClusterLeaves`: up to `limit` points after `offset`.
    pub fn cluster_leaves(&self, name: &str, cluster_id: u32, limit: usize, offset: usize) -> Option<Vec<ClusterLeaf>> {
        let index = self.source_point_index(name)?;
        let data = self.source_point_data(name)?;
        let leaves = index.leaves(cluster_id, limit, offset)?;
        Some(
            leaves
                .into_iter()
                .map(|i| {
                    let i = i as usize;
                    ClusterLeaf {
                        index: i,
                        id: data.id(i),
                        lon_lat: data.lon_lat[i],
                        properties: row_properties(&data.properties, i),
                    }
                })
                .collect(),
        )
    }
}

impl Source {
    /// Moves this source to a new stamp and publishes it where a bake can
    /// read it. The two writes are one call precisely so they cannot
    /// disagree -- `live_generation`'s whole safety argument is that it is
    /// written only where `generation` is.
    fn stamp(&mut self, stamp: u64) {
        self.generation = stamp;
        self.live_generation.store(stamp, std::sync::atomic::Ordering::Relaxed);
    }

    /// Writes the new paint onto the named features and nothing else.
    ///
    /// `Arc::make_mut` clones the whole feature vector if anything else
    /// holds a reference to it -- and the drawn generation holds one. The
    /// caller that cares (`MapView::apply_feature_paint`) hands its
    /// reference back first, which is what keeps a recolour from copying
    /// 414k coordinates it is not changing.
    fn write_paint(&mut self, updates: &[(FeatureId, FeaturePaint)], update: &PaintUpdate) {
        let features = Arc::make_mut(&mut self.features);
        for ((_, paint), &index) in updates.iter().zip(&update.features) {
            features[index].paint = paint.clone();
        }
    }
}

/// Reads one borrowed `FeatureInput` into the owned `OverlayFeature` the
/// store keeps, running the guards the string path gets from
/// `FeaturePaint::from_properties` and this one skips along with the parse.
///
/// SHARED by `set_source_from` and `set_source_data_from`, not mirrored.
/// The two differ in exactly one thing -- where `properties` comes from --
/// and every other difference between them would be a rule a caller has to
/// learn on one door and not the other. The guards in particular: a
/// negative `line-width` fails a `set_source_from` call today and must fail
/// a `set_source_data_from` call identically, in the same position in the
/// loop, so a collection with a bad width and a duplicate id after it
/// reports the same one of the two through either door.
fn read_feature(index: usize, feature: &input::FeatureInput<'_>) -> Result<OverlayFeature, FeatureError> {
    let fail = |reason: String| FeatureError::Feature { index, id: feature.id.clone(), reason };
    feature.paint.validate().map_err(fail)?;
    // Not a rule the string path enforces, because JSON cannot express a
    // coordinate that breaks it. See `GeometryInput::validate` for why one
    // infinite longitude in one feature silently flattens the whole
    // collection.
    feature.geometry.validate().map_err(fail)?;
    Ok(OverlayFeature {
        index,
        id: feature.id.clone(),
        geometry: feature.geometry.to_geometry(),
        paint: feature.paint.clone(),
    })
}

/// Moves `items[picks[k]]` into the `k`-th smallest of `picks`' positions:
/// the picked items are put in `picks`' order among the slots they hold,
/// and every other item stays where it is. A repeated pick counts once
/// (its first). `true` if anything moved. Shared by the overlay store and
/// `MapView`'s arc layers, so both lists reorder by the same rule.
pub(crate) fn permute_among_slots<T>(items: &mut Vec<T>, picks: Vec<usize>) -> bool {
    let mut seen = std::collections::HashSet::new();
    let picks: Vec<usize> = picks.into_iter().filter(|&i| i < items.len() && seen.insert(i)).collect();
    let mut slots = picks.clone();
    slots.sort_unstable();
    if slots == picks {
        return false;
    }
    let mut taken: Vec<Option<T>> = items.drain(..).map(Some).collect();
    let moved: Vec<T> = picks.iter().map(|&i| taken[i].take().expect("each pick once")).collect();
    for (slot, item) in slots.into_iter().zip(moved) {
        taken[slot] = Some(item);
    }
    items.extend(taken.into_iter().map(|item| item.expect("every slot refilled")));
    true
}

/// Row `row` of `table` as a shared map: the process-wide empty map when
/// the row has nothing, so a property-less feature allocates nothing.
fn row_properties(table: &PropertyTable, row: usize) -> Arc<FeatureProperties> {
    let map = table.row_map(row);
    if map.is_empty() {
        source_paint::no_properties()
    } else {
        Arc::new(map)
    }
}

/// A GeoJSON `properties` object as the `FeatureProperties` a compiled
/// filter reads.
///
/// The mapping is `mvt`'s, because the consumer of this map is
/// `style::Filter::matches` and that is the vocabulary it speaks: strings,
/// numbers and booleans. `null`, arrays and nested objects are DROPPED
/// rather than stringified -- a filter has no predicate that could match
/// them, and turning `[1,2]` into `"[1,2]"` would make
/// `["==", ["get", "k"], "[1,2]"]` appear to work, which is worse than not
/// matching.
///
/// Test-only now: `PropertyTable::from_json` applies the same mapping
/// straight into columns. Kept as the reference the tests build maps with.
#[cfg(test)]
fn properties_from_json(
    properties: &serde_json::Map<String, serde_json::Value>,
) -> FeatureProperties {
    properties
        .iter()
        .filter_map(|(key, value)| {
            let value = match value {
                serde_json::Value::String(text) => TagValue::String(text.clone()),
                serde_json::Value::Number(number) => TagValue::Number(number.as_f64()?),
                serde_json::Value::Bool(flag) => TagValue::Bool(*flag),
                _ => return None,
            };
            Some((key.clone(), value))
        })
        .collect()
}

/// A name that is not a source, reported the way an unknown id is.
///
/// `FeatureError::UnknownSource` rather than a string reason, because a
/// caller's recovery differs: an unknown id means their data moved on, an
/// unknown source means they have not pushed it yet.
fn unknown_source(name: &str) -> FeatureError {
    FeatureError::UnknownSource(name.to_string())
}

/// Grades a source's declared `SourceOptions` against what its features
/// actually admit, naming the feature responsible when they disagree.
///
/// Called from `install` (every push) and from `declare_source` (the
/// moment the promise is made), so a declaration can never be accepted
/// against features that already deny it and can never survive a push that
/// starts to.
///
/// `Ok(())` when nothing is declared. That is not the same as "the source
/// is zoom-independent" -- ask `source_is_zoom_independent` for that. This
/// answers only "is the promise kept", and no promise is trivially kept.
fn check_declaration(
    name: &str,
    features: &[OverlayFeature],
    options: SourceOptions,
) -> Result<(), FeatureError> {
    // Plan D3: line metrics are whole-source only, so a source too big to
    // stay whole cannot have them. Refused rather than tiled without
    // progress, which would draw a `draw` trim as the whole line.
    if options.line_metrics && tile_plan::tiling_points(features) >= tile_plan::TILED_SOURCE_MIN_POINTS {
        return Err(FeatureError::SourcePaint {
            source: name.to_string(),
            reason: format!(
                "lineMetrics is computed on the whole-source path only, and this source has at least {} line and \
                 polygon points (the tiling threshold); split it, or simplify its lines",
                tile_plan::TILED_SOURCE_MIN_POINTS
            ),
        });
    }
    if !options.zoom_independent {
        return Ok(());
    }
    match plan::zoom_dependence(features) {
        Ok(_) => Ok(()),
        Err(dependence) => Err(FeatureError::Feature {
            index: dependence.feature,
            id: features.get(dependence.feature).and_then(|feature| feature.id.clone()),
            reason: format!(
                "source {name:?} is declared zoom-independent, but {}",
                dependence.reason
            ),
        }),
    }
}

/// The style spec's `line-gradient` requires a GeoJSON source with
/// `lineMetrics` (there is no progress to evaluate it at otherwise); this
/// refuses the combination at the declaration of either half.
/// Refuses a declared `line-cap` / `line-join` on a source that is (or a
/// push would make) tiled: the tiled plan (`tile_plan`) strokes every line
/// with the overlay's own `Butt` / `Bevel`, so the declaration would be
/// silently dropped. See `OverlayStore::declare_source_paint`.
fn check_stroke_is_whole_source(name: &str, paint: &source_paint::SourcePaint, tiled: bool) -> Result<(), FeatureError> {
    if tiled && (paint.line_cap.is_some() || paint.line_join.is_some()) {
        return Err(FeatureError::SourcePaint {
            source: name.to_string(),
            reason: "line-cap / line-join are drawn on a whole source only, and this source is tiled: \
                     declare SourceOptions::whole_source (or line metrics) for it, or drop the stroke"
                .into(),
        });
    }
    Ok(())
}

fn check_gradient_has_metrics(
    name: &str,
    paint: &source_paint::SourcePaint,
    line_metrics: bool,
) -> Result<(), FeatureError> {
    if paint.line_gradient.is_some() && !line_metrics {
        return Err(FeatureError::SourcePaint {
            source: name.to_string(),
            reason: "line-gradient needs lineMetrics: declare the source's line metrics before its gradient".into(),
        });
    }
    Ok(())
}

/// Refuses a `line-gradient` source whose pushed line points plus the
/// points its STOP cuts add (`plan::gradient_cut_points`) exceed
/// `plan::GRADIENT_POINT_BUDGET` -- the D3 gate, counting what
/// densification adds. The uniform parts need no refusal: `build_plan`
/// shares what is left of the budget out by length.
fn check_gradient_budget(
    name: &str,
    features: &[OverlayFeature],
    paint: &source_paint::SourcePaint,
) -> Result<(), FeatureError> {
    let Some(gradient) = &paint.line_gradient else { return Ok(()) };
    let pushed = tile_plan::tiling_points(features);
    let cuts = plan::gradient_cut_points(features, gradient);
    if pushed.saturating_add(cuts) > plan::GRADIENT_POINT_BUDGET {
        return Err(FeatureError::SourcePaint {
            source: name.to_string(),
            reason: format!(
                "line-gradient: cutting every line at the gradient's stops would add {cuts} points to {pushed}, over \
                 the {} a whole-source line-metrics source may hold; use fewer stops or fewer, longer lines",
                plan::GRADIENT_POINT_BUDGET
            ),
        });
    }
    Ok(())
}

/// Keeps `SourcePaint::line_metrics` equal to the source's option, which
/// is what `plan::build_plan` reads.
fn mirror_line_metrics(paint: &mut Arc<source_paint::SourcePaint>, on: bool) {
    if paint.line_metrics != on {
        Arc::make_mut(paint).line_metrics = on;
    }
}

/// Heap bytes an `OverlayStore` holds, split by owner, for the
/// `us_block_groups` breakdown
/// (`docs/measurements/2026-09-25-us-block-groups-breakdown.md`).
/// Capacity-based (`vec_block_bytes`), the unit `PaintPlan::footprint_bytes`
/// already uses.
#[cfg(feature = "probe")]
#[derive(Debug, Default, Clone, Copy)]
pub struct OverlayStoreCensus {
    pub sources: usize,
    pub features: usize,
    pub rings: usize,
    pub coordinates: usize,
    /// `Vec<OverlayFeature>` blocks: `size_of::<OverlayFeature>()` x capacity.
    pub feature_vec_bytes: usize,
    /// Ring and polygon `Vec` blocks plus the coordinate blocks.
    pub geometry_bytes: usize,
    /// Of `geometry_bytes`, the `(f64, f64)` coordinate blocks alone.
    pub coordinate_bytes: usize,
    pub by_id_bytes: usize,
    pub hit_index_bytes: usize,
    /// Adopted tile indexes (`TileIndex::bytes`): after Task 7 the one
    /// geometry copy of a tiled source's polygons and lines.
    pub tile_index_bytes: usize,
    /// `PaintPlan::footprint_bytes`: the plan's own `Geometry<i32>` copy.
    pub plan_bytes: usize,
    pub plan_ops: usize,
    pub feature_ops_bytes: usize,
    /// Every source's feature properties: hash tables, key and value
    /// strings, counted once per distinct `Arc` (B9 baseline) or the
    /// columnar table (B9).
    pub property_bytes: usize,
}

#[cfg(feature = "probe")]
impl OverlayStore {
    /// See [`OverlayStoreCensus`].
    pub fn probe_census(&self) -> OverlayStoreCensus {
        use crate::render::{hashmap_table_bytes, vec_block_bytes};
        let mut c = OverlayStoreCensus::default();
        let run = |c: &mut OverlayStoreCensus, run: &Vec<(f64, f64)>| {
            c.rings += 1;
            c.coordinates += run.len();
            c.coordinate_bytes += vec_block_bytes::<(f64, f64)>(run.capacity());
        };
        for source in &self.sources {
            c.sources += 1;
            c.features += source.features.len();
            c.feature_vec_bytes += vec_block_bytes::<OverlayFeature>(source.features.capacity());
            for feature in source.features.iter() {
                match &feature.geometry {
                    OverlayGeometry::Line(parts) => {
                        c.geometry_bytes += vec_block_bytes::<Vec<(f64, f64)>>(parts.capacity());
                        parts.iter().for_each(|r| run(&mut c, r));
                    }
                    OverlayGeometry::Polygon(polygons) => {
                        c.geometry_bytes += vec_block_bytes::<Vec<Vec<(f64, f64)>>>(polygons.capacity());
                        for rings in polygons {
                            c.geometry_bytes += vec_block_bytes::<Vec<(f64, f64)>>(rings.capacity());
                            rings.iter().for_each(|r| run(&mut c, r));
                        }
                    }
                    OverlayGeometry::Point(points) => run(&mut c, points),
                    OverlayGeometry::Empty | OverlayGeometry::Indexed { .. } => {}
                }
            }
            c.by_id_bytes += hashmap_table_bytes::<IdKey, usize>(source.by_id.capacity());
            c.hit_index_bytes += source.hit_index.probe_bytes();
            c.tile_index_bytes += source.tile_index.as_ref().map_or(0, |index| index.bytes());
            if let Some(plan) = &source.plan {
                c.plan_bytes += plan.plan.footprint_bytes();
                c.plan_ops += plan.plan.ops.len();
                c.feature_ops_bytes += vec_block_bytes::<std::ops::Range<usize>>(plan.feature_ops.capacity());
            }
        }
        c.geometry_bytes += c.coordinate_bytes;
        c.property_bytes = self.probe_property_bytes();
        c
    }

    fn probe_property_bytes(&self) -> usize {
        self.sources.iter().map(|source| source.properties.bytes()).sum()
    }

    /// Wall time of one `plan::classify` of source `name`: the paint-change
    /// path's re-class, measured on its own.
    pub fn probe_classify(&self, name: &str) -> Option<std::time::Duration> {
        let snapshot = self.source_snapshot(name)?;
        let bracket = plan::zoom_dependence(&snapshot.features).unwrap_or(plan::BakeBracket::Bucket(3));
        let started = crate::timing::Instant::now();
        let classes = plan::classify(&snapshot.features, &snapshot.properties, bracket, &snapshot.paint)?;
        let took = started.elapsed();
        drop(classes);
        Some(took)
    }

    /// Every source's current plan, for a probe that re-tessellates them.
    pub(crate) fn probe_plans(&self) -> Vec<&OverlayPlan> {
        self.sources.iter().filter_map(|s| s.plan.as_ref()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use input::{FeatureInput, GeometryInput};

    /// Every way the two paths could come apart, in one collection.
    ///
    /// A string id, a numeric id and no id at all; a point, a multipoint, a
    /// line, a multiline, a polygon with a hole and a multipolygon; a null
    /// geometry that still carries a label; and the four shapes
    /// `build_plan` DROPS -- a one-position line part, a three-position
    /// ring, an empty point list and `circle-radius: 0` -- because a drop
    /// is exactly where feature index and op index come apart, and
    /// `feature_ops` is what has to survive it.
    const EVERY_SHAPE: &str = r##"{"type":"FeatureCollection","features":[
        {"type":"Feature","id":"label-only","geometry":null,
         "properties":{"text-field":"42","text-size":14}},
        {"type":"Feature","id":"point","geometry":{"type":"Point","coordinates":[103.85,1.29]},
         "properties":{"circle-radius":5,"circle-color":"#00ff00"}},
        {"type":"Feature","id":7,"geometry":{"type":"MultiPoint",
         "coordinates":[[103.851,1.291],[103.852,1.292]]},
         "properties":{"circle-radius":3,"icon-image":"pin","icon-anchor":"bottom"}},
        {"type":"Feature","geometry":{"type":"LineString",
         "coordinates":[[103.85,1.29],[103.86,1.30],[103.87,1.31]]},
         "properties":{"line-color":"#ff0000","line-width":6,"line-dasharray":[2,2,6,2]}},
        {"type":"Feature","id":"multiline","geometry":{"type":"MultiLineString",
         "coordinates":[[[103.80,1.20],[103.81,1.21]],[[103.90,1.40],[103.91,1.41]]]},
         "properties":{"line-width":["interpolate",["linear"],["zoom"],8,2.4,18,7.2]}},
        {"type":"Feature","id":"polygon","geometry":{"type":"Polygon",
         "coordinates":[[[0,0],[4,0],[4,4],[0,4],[0,0]],[[1,1],[2,1],[2,2],[1,2],[1,1]]]},
         "properties":{"fill-color":"#00ff0080","fill-opacity":0.5}},
        {"type":"Feature","id":"multipolygon","geometry":{"type":"MultiPolygon",
         "coordinates":[[[[10,10],[11,10],[11,11],[10,10]]],[[[20,20],[21,20],[21,21],[20,20]]]]},
         "properties":{}},
        {"type":"Feature","id":"short-line","geometry":{"type":"MultiLineString",
         "coordinates":[[[103.85,1.29]]]},"properties":{}},
        {"type":"Feature","id":"short-ring","geometry":{"type":"Polygon",
         "coordinates":[[[103.85,1.29],[103.86,1.29],[103.85,1.29]]]},"properties":{}},
        {"type":"Feature","id":"no-points","geometry":{"type":"MultiPoint","coordinates":[]},
         "properties":{}},
        {"type":"Feature","id":"no-dot","geometry":{"type":"Point","coordinates":[103.88,1.33]},
         "properties":{"circle-radius":0,"text-field":"stop","text-offset":[0,-1.5]}}]}"##;

    /// The same collection as `&[FeatureInput]`.
    ///
    /// The PAINT is taken through `FeaturePaint::from_properties` from the
    /// same property objects, deliberately: this test is about the
    /// conversion of geometry, id and index, and resolving the paint twice
    /// from the same source is what keeps the comparison pointed at that
    /// rather than at a paint table typed out twice.
    fn every_shape_structured() -> Vec<FeatureInput<'static>> {
        fn paint(json: &str) -> FeaturePaint {
            FeaturePaint::from_properties(&serde_json::from_str(json).unwrap()).unwrap()
        }

        // Borrowed all the way down, so the nesting has to be materialised
        // ring by ring. `&'static` here only because the test's data is
        // literal; a real caller borrows from whatever it already holds.
        const LINE: &[[f64; 2]] = &[[103.85, 1.29], [103.86, 1.30], [103.87, 1.31]];
        const LINE_PARTS: &[&[[f64; 2]]] = &[LINE];
        const MULTILINE_A: &[[f64; 2]] = &[[103.80, 1.20], [103.81, 1.21]];
        const MULTILINE_B: &[[f64; 2]] = &[[103.90, 1.40], [103.91, 1.41]];
        const MULTILINE: &[&[[f64; 2]]] = &[MULTILINE_A, MULTILINE_B];
        const EXTERIOR: &[[f64; 2]] = &[[0.0, 0.0], [4.0, 0.0], [4.0, 4.0], [0.0, 4.0], [0.0, 0.0]];
        const HOLE: &[[f64; 2]] = &[[1.0, 1.0], [2.0, 1.0], [2.0, 2.0], [1.0, 2.0], [1.0, 1.0]];
        const POLYGON: &[&[[f64; 2]]] = &[EXTERIOR, HOLE];
        const POLYGONS: &[&[&[[f64; 2]]]] = &[POLYGON];
        const POLY_A: &[[f64; 2]] = &[[10.0, 10.0], [11.0, 10.0], [11.0, 11.0], [10.0, 10.0]];
        const POLY_B: &[[f64; 2]] = &[[20.0, 20.0], [21.0, 20.0], [21.0, 21.0], [20.0, 20.0]];
        const MULTIPOLY_A: &[&[[f64; 2]]] = &[POLY_A];
        const MULTIPOLY_B: &[&[[f64; 2]]] = &[POLY_B];
        const MULTIPOLYGON: &[&[&[[f64; 2]]]] = &[MULTIPOLY_A, MULTIPOLY_B];
        const ONE_POINT: &[[f64; 2]] = &[[103.85, 1.29]];
        const TWO_POINTS: &[[f64; 2]] = &[[103.851, 1.291], [103.852, 1.292]];
        const SHORT_LINE: &[[f64; 2]] = &[[103.85, 1.29]];
        const SHORT_LINE_PARTS: &[&[[f64; 2]]] = &[SHORT_LINE];
        const SHORT_RING: &[[f64; 2]] = &[[103.85, 1.29], [103.86, 1.29], [103.85, 1.29]];
        const SHORT_RINGS: &[&[[f64; 2]]] = &[SHORT_RING];
        const SHORT_POLYGON: &[&[&[[f64; 2]]]] = &[SHORT_RINGS];
        const NO_DOT: &[[f64; 2]] = &[[103.88, 1.33]];

        vec![
            FeatureInput {
                id: Some(FeatureId::String("label-only".into())),
                geometry: GeometryInput::Empty,
                paint: paint(r#"{"text-field":"42","text-size":14}"#),
            },
            FeatureInput {
                id: Some(FeatureId::String("point".into())),
                geometry: GeometryInput::Point(ONE_POINT),
                paint: paint(r##"{"circle-radius":5,"circle-color":"#00ff00"}"##),
            },
            FeatureInput {
                id: Some(FeatureId::Number(7.0)),
                geometry: GeometryInput::Point(TWO_POINTS),
                paint: paint(r#"{"circle-radius":3,"icon-image":"pin","icon-anchor":"bottom"}"#),
            },
            FeatureInput {
                id: None,
                geometry: GeometryInput::Line(LINE_PARTS),
                paint: paint(r##"{"line-color":"#ff0000","line-width":6,"line-dasharray":[2,2,6,2]}"##),
            },
            FeatureInput {
                id: Some(FeatureId::String("multiline".into())),
                geometry: GeometryInput::Line(MULTILINE),
                paint: paint(r#"{"line-width":["interpolate",["linear"],["zoom"],8,2.4,18,7.2]}"#),
            },
            FeatureInput {
                id: Some(FeatureId::String("polygon".into())),
                geometry: GeometryInput::Polygon(POLYGONS),
                paint: paint(r##"{"fill-color":"#00ff0080","fill-opacity":0.5}"##),
            },
            FeatureInput {
                id: Some(FeatureId::String("multipolygon".into())),
                geometry: GeometryInput::Polygon(MULTIPOLYGON),
                paint: paint("{}"),
            },
            FeatureInput {
                id: Some(FeatureId::String("short-line".into())),
                geometry: GeometryInput::Line(SHORT_LINE_PARTS),
                paint: paint("{}"),
            },
            FeatureInput {
                id: Some(FeatureId::String("short-ring".into())),
                geometry: GeometryInput::Polygon(SHORT_POLYGON),
                paint: paint("{}"),
            },
            FeatureInput {
                id: Some(FeatureId::String("no-points".into())),
                geometry: GeometryInput::Point(&[]),
                paint: paint("{}"),
            },
            FeatureInput {
                id: Some(FeatureId::String("no-dot".into())),
                geometry: GeometryInput::Point(NO_DOT),
                paint: paint(r#"{"circle-radius":0,"text-field":"stop","text-offset":[0,-1.5]}"#),
            },
        ]
    }

    /// Every piece of a store's state, rendered so two of them can be
    /// compared whole.
    ///
    /// Through `Debug` rather than through `PartialEq` because `OverlayPlan`,
    /// `PaintPlan` and `HitIndex` are internal types with no equality and no
    /// reason to grow one for a test. `Debug` for a float prints the shortest
    /// string that round-trips, so two values print alike exactly when they
    /// are the same value -- which is the comparison wanted, and a stricter
    /// one than any epsilon.
    ///
    /// The point of rendering ALL of it, rather than counting features and
    /// ops, is that it grades the new path against the known-good one
    /// instead of against what its author remembered to check.
    fn state(store: &OverlayStore) -> String {
        format!(
            "generation: {}\nfeatures: {:#?}\nplan: {:#?}\nhit index: {:#?}",
            store.generation, store.features(), store.plan(), store.default_hit_index(),
        )
    }

    /// THE test this design exists to be graded by: the same collection
    /// through both doors, producing the same store down to the last field.
    ///
    /// Two entry points that must stay behaviourally identical past their
    /// join point is the one failure mode this change can actually suffer,
    /// and it is not one any per-variant test would catch -- a conversion
    /// that dropped a hole, reordered a ring or lost an id would pass every
    /// test written from the author's own model of what the conversion does.
    #[test]
    fn the_two_paths_build_identical_stores() {
        let mut from_string = OverlayStore::new();
        from_string.replace(EVERY_SHAPE, 12).unwrap();

        let mut from_structs = OverlayStore::new();
        from_structs.replace_from(&every_shape_structured(), 12).unwrap();

        assert_eq!(state(&from_string), state(&from_structs));
        assert_eq!(from_string.default_by_id(), from_structs.default_by_id(), "the id maps must agree");
    }

    /// Separately from the whole-state comparison, because these are the
    /// three things a later change is most likely to break quietly and
    /// because a diff of two 400-line `Debug` dumps does not say which.
    #[test]
    fn the_two_paths_agree_on_the_bracket_the_ops_and_the_feature_ops() {
        let mut from_string = OverlayStore::new();
        from_string.replace(EVERY_SHAPE, 12).unwrap();
        let mut from_structs = OverlayStore::new();
        from_structs.replace_from(&every_shape_structured(), 12).unwrap();

        let one = from_string.plan().expect("this collection draws something");
        let two = from_structs.plan().expect("this collection draws something");
        assert_eq!(one.bracket, two.bracket);
        assert_eq!(one.plan.ops.len(), two.plan.ops.len());
        assert_eq!(one.feature_ops, two.feature_ops);
        assert_eq!((one.origin.world_x, one.origin.world_y), (two.origin.world_x, two.origin.world_y));

        // Not vacuous: this collection really does drop features, so
        // `feature_ops` really is where index and op come apart.
        assert!(
            one.feature_ops.iter().any(|range| range.is_empty()),
            "the collection must exercise a dropped feature",
        );
        assert_eq!(one.feature_ops.len(), 11, "one entry per INPUT feature");
    }

    const ONE_ROUTE: &str = r#"{"type":"FeatureCollection","features":[
        {"type":"Feature","id":"route-42","geometry":{"type":"LineString",
         "coordinates":[[103.85,1.29],[103.86,1.30]]},"properties":{"line-width":6}}]}"#;

    /// An empty slice clears, exactly as `{"features":[]}` does -- the
    /// structured door needs no `clear()` either.
    #[test]
    fn replacing_from_an_empty_slice_clears_the_store() {
        let mut store = OverlayStore::new();
        store.replace_from(&every_shape_structured(), 12).unwrap();
        assert!(!store.is_empty());

        store.replace_from(&[], 12).unwrap();
        assert!(store.is_empty());
        assert!(store.plan().is_none());
        assert_eq!(store.generation(), 2, "clearing is a replacement like any other");

        // And identically to the string path's own clear.
        let mut from_string = OverlayStore::new();
        from_string.replace(EVERY_SHAPE, 12).unwrap();
        from_string.replace(r#"{"type":"FeatureCollection","features":[]}"#, 12).unwrap();
        assert_eq!(state(&from_string), state(&store));
    }

    /// The guard the spec singles out. `replace` refuses this collection;
    /// `replace_from` takes the paint pre-resolved, so it would sail past
    /// unless `FeaturePaint::validate` is run -- and the map would draw a
    /// width nobody asked for.
    #[test]
    fn a_negative_line_width_is_refused_on_the_structured_path_too() {
        let mut paint = FeaturePaint::from_properties(&serde_json::Map::new()).unwrap();
        paint.line_width = crate::style::ZoomValue::constant(-3.0);

        let mut store = OverlayStore::new();
        let error = store
            .replace_from(
                &[FeatureInput {
                    id: Some(FeatureId::String("bad-width".into())),
                    geometry: GeometryInput::Empty,
                    paint,
                }],
                12,
            )
            .unwrap_err();
        match error {
            FeatureError::Feature { index, id, reason } => {
                assert_eq!(index, 0);
                assert_eq!(id, Some(FeatureId::String("bad-width".into())));
                assert!(reason.contains("line-width"), "reason was {reason}");
            }
            other => panic!("expected a feature error, got {other:?}"),
        }
        assert!(store.is_empty(), "a rejected call changes nothing");
        assert_eq!(store.generation(), 0, "a failure is not a generation");
    }

    /// A negative `circle-radius`, the spec's other named guard, and the
    /// one that would otherwise reach the hit index as a marker with a
    /// negative reach.
    #[test]
    fn a_negative_circle_radius_is_refused_on_the_structured_path_too() {
        let mut paint = FeaturePaint::from_properties(&serde_json::Map::new()).unwrap();
        paint.circle_radius = -5.0;
        let mut store = OverlayStore::new();
        let error = store
            .replace_from(
                &[FeatureInput { id: None, geometry: GeometryInput::Empty, paint }],
                12,
            )
            .unwrap_err();
        assert!(error.to_string().contains("circle-radius"), "error was {error}");
    }

    /// The whole call fails, naming the feature -- rather than the
    /// collection quietly collapsing onto a line because
    /// `OverlayOrigin::centred_on` took an infinite bounding box.
    #[test]
    fn a_non_finite_coordinate_is_refused_and_names_its_feature() {
        let good: &[[f64; 2]] = &[[103.85, 1.29], [103.86, 1.30]];
        let bad: &[[f64; 2]] = &[[f64::INFINITY, 1.29], [103.86, 1.30]];
        let good_parts = [good];
        let bad_parts = [bad];
        let paint = || FeaturePaint::from_properties(&serde_json::Map::new()).unwrap();

        let mut store = OverlayStore::new();
        let error = store
            .replace_from(
                &[
                    FeatureInput { id: Some(FeatureId::String("fine".into())), geometry: GeometryInput::Line(&good_parts), paint: paint() },
                    FeatureInput { id: Some(FeatureId::String("off-the-map".into())), geometry: GeometryInput::Line(&bad_parts), paint: paint() },
                ],
                12,
            )
            .unwrap_err();
        match error {
            FeatureError::Feature { index, id, reason } => {
                assert_eq!(index, 1);
                assert_eq!(id, Some(FeatureId::String("off-the-map".into())));
                assert!(reason.contains("finite"), "reason was {reason}");
            }
            other => panic!("expected a feature error, got {other:?}"),
        }
        assert!(store.is_empty(), "a rejected call changes nothing");
    }

    /// A rejected call leaves what was drawn exactly as it was, including
    /// the features listed BEFORE the offending one.
    #[test]
    fn a_rejected_structured_replacement_leaves_the_previous_features_untouched() {
        let mut store = OverlayStore::new();
        store.replace(ONE_ROUTE, 12).unwrap();
        let before = state(&store);

        let mut bad = FeaturePaint::from_properties(&serde_json::Map::new()).unwrap();
        bad.text_size = -1.0;
        let good = FeaturePaint::from_properties(&serde_json::Map::new()).unwrap();
        assert!(store
            .replace_from(
                &[
                    FeatureInput { id: Some(FeatureId::String("fine".into())), geometry: GeometryInput::Empty, paint: good },
                    FeatureInput { id: Some(FeatureId::String("not-fine".into())), geometry: GeometryInput::Empty, paint: bad },
                ],
                12,
            )
            .is_err());
        assert_eq!(state(&store), before);
    }

    /// The duplicate-id rule is `install`'s, so it applies to both doors by
    /// construction -- asserted anyway, because "by construction" is a claim
    /// about code that can be refactored.
    #[test]
    fn two_features_sharing_an_id_are_refused_on_the_structured_path_too() {
        let paint = || FeaturePaint::from_properties(&serde_json::Map::new()).unwrap();
        let mut store = OverlayStore::new();
        let error = store
            .replace_from(
                &[
                    FeatureInput { id: Some(FeatureId::String("same".into())), geometry: GeometryInput::Empty, paint: paint() },
                    FeatureInput { id: Some(FeatureId::String("same".into())), geometry: GeometryInput::Empty, paint: paint() },
                ],
                12,
            )
            .unwrap_err();
        match error {
            FeatureError::Feature { index, reason, .. } => {
                assert_eq!(index, 1, "the SECOND feature is the one that reused the id");
                assert!(reason.contains("duplicate id"), "reason was {reason}");
            }
            other => panic!("expected a feature error, got {other:?}"),
        }
        assert!(store.is_empty());
    }

    /// `None` survives as `None`: the feature is stored, drawn and
    /// hit-testable, and simply cannot be addressed by id -- which is what
    /// an absent id means, not a gap.
    #[test]
    fn a_feature_with_no_id_is_kept_and_is_merely_unaddressable() {
        let line: &[[f64; 2]] = &[[103.85, 1.29], [103.86, 1.30]];
        let parts = [line];
        let mut store = OverlayStore::new();
        store
            .replace_from(
                &[FeatureInput {
                    id: None,
                    geometry: GeometryInput::Line(&parts),
                    paint: FeaturePaint::from_properties(&serde_json::Map::new()).unwrap(),
                }],
                12,
            )
            .unwrap();
        assert_eq!(store.features().len(), 1);
        assert_eq!(store.features()[0].id, None);
        assert_eq!(store.features()[0].index, 0);
        assert!(store.default_by_id().is_empty(), "nothing to address it by");
        assert!(store.plan().is_some(), "it still draws");
    }

    /// `Empty` behaves exactly as GeoJSON `null` geometry does: kept,
    /// carrying its label, producing no ops, and dropped by
    /// `HitIndex::build` because there is nothing on screen to point at.
    #[test]
    fn an_empty_geometry_draws_nothing_but_is_still_a_feature() {
        let mut paint = FeaturePaint::from_properties(&serde_json::Map::new()).unwrap();
        paint.text_field = Some("42".into());

        let mut structured = OverlayStore::new();
        structured
            .replace_from(
                &[FeatureInput {
                    id: Some(FeatureId::String("label-only".into())),
                    geometry: GeometryInput::Empty,
                    paint,
                }],
                12,
            )
            .unwrap();

        let mut from_string = OverlayStore::new();
        from_string
            .replace(
                r#"{"type":"FeatureCollection","features":[{"type":"Feature","id":"label-only",
                   "geometry":null,"properties":{"text-field":"42"}}]}"#,
                12,
            )
            .unwrap();

        assert_eq!(state(&structured), state(&from_string));
        assert_eq!(structured.features().len(), 1);
        assert_eq!(structured.features()[0].paint.text_field.as_deref(), Some("42"));
        assert!(structured.plan().is_none(), "nothing drawable");
        assert!(
            structured.snapshot().feature_at((0.0, 0.0), 1e9, 1.0).is_none(),
            "an empty geometry carries no hit target",
        );
    }

    /// The benchmark this path exists for, on this crate's own parity
    /// dataset: the same 1,132 features pushed as a GeoJSON string and as
    /// `&[FeatureInput]`.
    ///
    ///     cargo test --release the_structured_push_against_the_string_push -- --ignored --nocapture
    ///
    /// `#[ignore]`d because it reads a 29 MB file, and because a timing is
    /// not an assertion -- it is evidence, and it belongs to whoever asks
    /// for it.
    ///
    /// WHAT IS AND IS NOT ON THE CLOCK. Reading the file and building the
    /// caller's own structures are OUTSIDE the timer on both sides, because
    /// a real caller already holds their data; what is inside is exactly
    /// what `set_features` and `set_features_from` each do synchronously
    /// before spawning a bake. The string side therefore pays the parse and
    /// the structured side pays the borrow-to-owned conversion, which is
    /// the whole comparison.
    ///
    /// WHAT IT UNDERSTATES: the consumer's own half. Building and
    /// serialising this same data was measured at 106-126 ms on their side,
    /// none of which exists at all once the API stops speaking strings, and
    /// none of which is measured here.
    #[test]
    #[ignore]
    fn the_structured_push_against_the_string_push_on_the_simulation_dataset() {
        use std::time::Duration;

        let json = std::fs::read_to_string("geojsons/simulation.json")
            .expect("geojsons/simulation.json");

        // The caller's own data, built once and outside every timer. A
        // consumer holds this already; turning it into GeoJSON is the cost
        // this whole change exists to delete, and charging it to one side
        // here would measure the wrong thing.
        let document: serde_json::Value = serde_json::from_str(&json).unwrap();
        let mut geometries: Vec<Vec<Vec<[f64; 2]>>> = Vec::new();
        let mut paints: Vec<FeaturePaint> = Vec::new();
        let mut ids: Vec<Option<FeatureId>> = Vec::new();
        let mut kinds: Vec<&str> = Vec::new();
        for (index, feature) in document["features"].as_array().unwrap().iter().enumerate() {
            let geometry = &feature["geometry"];
            let kind = geometry["type"].as_str().unwrap_or("null");
            let read = |positions: &serde_json::Value| -> Vec<[f64; 2]> {
                positions
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|p| {
                        let p = p.as_array().unwrap();
                        [p[0].as_f64().unwrap(), p[1].as_f64().unwrap()]
                    })
                    .collect()
            };
            geometries.push(match kind {
                "Point" => {
                    let p = geometry["coordinates"].as_array().unwrap();
                    vec![vec![[p[0].as_f64().unwrap(), p[1].as_f64().unwrap()]]]
                }
                "LineString" => vec![read(&geometry["coordinates"])],
                other => panic!("this harness covers what the dataset holds, not {other:?}"),
            });
            kinds.push(if kind == "Point" { "Point" } else { "Line" });
            paints.push(
                FeaturePaint::from_properties(feature["properties"].as_object().unwrap()).unwrap(),
            );
            // The file declares no ids; give each feature its index, which
            // is what a caller with a fleet already has.
            ids.push(Some(FeatureId::Number(index as f64)));
        }

        // The borrow tree, also outside the timers: it is pointers, not
        // coordinates, and a caller builds it at the call site.
        let parts: Vec<Vec<&[[f64; 2]]>> = geometries
            .iter()
            .map(|feature| feature.iter().map(|part| part.as_slice()).collect())
            .collect();
        let inputs: Vec<FeatureInput<'_>> = (0..geometries.len())
            .map(|index| FeatureInput {
                id: ids[index].clone(),
                geometry: match kinds[index] {
                    "Point" => GeometryInput::Point(parts[index][0]),
                    _ => GeometryInput::Line(&parts[index]),
                },
                paint: paints[index].clone(),
            })
            .collect();

        let median = |mut runs: Vec<Duration>| {
            runs.sort_unstable();
            runs[runs.len() / 2]
        };
        let time = |mut run: Box<dyn FnMut()>| {
            median(
                (0..5)
                    .map(|_| {
                        let started = crate::timing::Instant::now();
                        run();
                        started.elapsed()
                    })
                    .collect(),
            )
        };

        let coordinates: usize = geometries.iter().flatten().map(Vec::len).sum();
        println!(
            "\n{} features, {coordinates} coordinates, {:.1} MB of GeoJSON",
            inputs.len(),
            json.len() as f64 / 1e6,
        );

        // The stages, each timed on its own, because the headline
        // difference is worth nothing if it cannot be accounted for. A
        // stage measured in isolation is not simply subtractable from the
        // whole -- the allocator is in a different state for each -- but
        // the shape is what the claim is about.
        let value = time(Box::new(|| {
            let _: serde_json::Value = serde_json::from_str(&json).unwrap();
        }));
        let parse = time(Box::new(|| {
            let _ = parse_feature_collection(&json).unwrap();
        }));
        // `black_box` because this one is small enough that an optimiser
        // eliding it would be indistinguishable from it being fast.
        let convert = time(Box::new(|| {
            let converted: Vec<OverlayGeometry> =
                inputs.iter().map(|input| input.geometry.to_geometry()).collect();
            std::hint::black_box(&converted);
        }));

        let mut store = OverlayStore::new();
        let string = time(Box::new(|| store.replace(&json, 12).unwrap()));
        let mut store = OverlayStore::new();
        let structured = time(Box::new(|| store.replace_from(&inputs, 12).unwrap()));

        println!("  serde_json -> Value               {value:?}");
        println!("  parse_feature_collection          {parse:?}  (includes the above)");
        println!("  GeometryInput -> OverlayGeometry  {convert:?}");
        println!("  ------------------------------------------");
        println!("  OverlayStore::replace             {string:?}");
        println!("  OverlayStore::replace_from        {structured:?}");
        println!(
            "  saved: {:?} ({:.0}%)\n",
            string.saturating_sub(structured),
            100.0 * (1.0 - structured.as_secs_f64() / string.as_secs_f64()),
        );

        // Not an assertion about a NUMBER -- a timing is evidence, not a
        // contract -- but the two stores must still agree, or the timing is
        // of two different things.
        let mut from_string = OverlayStore::new();
        from_string.replace(&json, 12).unwrap();
        assert_eq!(from_string.features().len(), store.features().len());
        assert_eq!(
            from_string.plan().unwrap().plan.ops.len(),
            store.plan().unwrap().plan.ops.len(),
        );
    }

    #[test]
    fn a_new_store_is_empty_and_has_no_plan() {
        let store = OverlayStore::new();
        assert!(store.is_empty());
        assert!(store.plan().is_none());
        assert_eq!(store.generation(), 0);
    }

    #[test]
    fn replacing_with_features_fills_the_store_and_bumps_the_generation() {
        let mut store = OverlayStore::new();
        store.replace(ONE_ROUTE, 12).unwrap();
        assert_eq!(store.features().len(), 1);
        assert_eq!(store.features()[0].index, 0);
        assert_eq!(store.features()[0].id, Some(FeatureId::String("route-42".into())));
        assert!(store.plan().is_some());
        assert_eq!(store.generation(), 1);
    }

    /// An empty collection is how a caller clears the overlay -- the spec
    /// gives them no `clear()` to call instead.
    #[test]
    fn replacing_with_an_empty_collection_clears_the_store() {
        let mut store = OverlayStore::new();
        store.replace(ONE_ROUTE, 12).unwrap();
        store.replace(r#"{"type":"FeatureCollection","features":[]}"#, 12).unwrap();
        assert!(store.is_empty());
        assert!(store.plan().is_none());
        assert_eq!(store.generation(), 2, "clearing is a replacement like any other");
    }

    /// All-or-nothing. A rejected call leaves what was drawn exactly as it
    /// was -- half a fleet on the map is worse than none.
    #[test]
    fn a_rejected_replacement_leaves_the_previous_features_untouched() {
        let mut store = OverlayStore::new();
        store.replace(ONE_ROUTE, 12).unwrap();
        let generation = store.generation();

        assert!(store.replace("{not json", 12).is_err());
        assert!(store
            .replace(
                r#"{"type":"FeatureCollection","features":[
                   {"type":"Feature","geometry":null,"properties":{"line-width":-3}}]}"#,
                12
            )
            .is_err());

        assert_eq!(store.features().len(), 1);
        assert_eq!(store.features()[0].id, Some(FeatureId::String("route-42".into())));
        assert_eq!(store.generation(), generation, "a failure is not a generation");
    }

    /// A paint failure names the feature, the same way a geometry failure
    /// does -- one error type, one feature named, whatever went wrong.
    #[test]
    fn a_paint_error_is_reported_against_its_feature() {
        let mut store = OverlayStore::new();
        let error = store
            .replace(
                r#"{"type":"FeatureCollection","features":[
                   {"type":"Feature","geometry":null,"properties":{}},
                   {"type":"Feature","id":"bad","geometry":null,
                    "properties":{"line-color":"nope"}}]}"#,
                12,
            )
            .unwrap_err();
        match error {
            FeatureError::Feature { index, id, reason } => {
                assert_eq!(index, 1);
                assert_eq!(id, Some(FeatureId::String("bad".into())));
                assert!(reason.contains("line-color"), "reason was {reason}");
            }
            other => panic!("expected a feature error, got {other:?}"),
        }
    }

    /// Features with no drawable geometry are kept -- they still carry a
    /// label -- even though there is no plan. They carry no hit target:
    /// `HitIndex::build` drops them, because there is nothing on screen to
    /// point at (see `hit::tests::an_empty_geometry_is_never_hit`).
    #[test]
    fn label_only_features_are_stored_without_a_plan() {
        let mut store = OverlayStore::new();
        store
            .replace(
                r#"{"type":"FeatureCollection","features":[
                   {"type":"Feature","geometry":null,"properties":{"text-field":"42"}}]}"#,
                12,
            )
            .unwrap();
        assert_eq!(store.features().len(), 1);
        assert!(store.plan().is_none());
        assert!(!store.is_empty());
    }

    /// The index is rebuilt on every replacement, so it can never describe
    /// features that are no longer drawn.
    #[test]
    fn the_hit_index_is_rebuilt_on_every_replacement() {
        let mut store = OverlayStore::new();
        store.replace(ONE_ROUTE, 12).unwrap();
        let world = geom::lon_lat_to_world(103.855, 1.295);
        assert!(store.snapshot().feature_at(world, 0.01, 0.01).is_some());

        store.replace(r#"{"type":"FeatureCollection","features":[]}"#, 12).unwrap();
        assert!(
            store.snapshot().feature_at(world, 0.01, 0.01).is_none(),
            "the cleared store must not still hit its old features"
        );
    }

    /// A zoom crossing must not cost a re-parse. The store already holds
    /// everything a fresh plan needs -- geometry, and paint compiled to
    /// `ZoomValue`s -- so a snapshot can hand back a plan for any bucket.
    #[test]
    fn a_snapshot_re_resolves_its_own_features_at_another_zoom() {
        const RAMPED: &str = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":{"type":"LineString",
             "coordinates":[[103.85,1.29],[103.86,1.30]]},
             "properties":{"line-width":["interpolate",["linear"],["zoom"],8,2.4,18,7.2]}}]}"#;
        let mut store = OverlayStore::new();
        store.replace(RAMPED, 9).unwrap();
        assert_eq!(
            store.plan().unwrap().bracket,
            plan::BakeBracket::Ramp { lo: 8, hi: 18 },
            "a lone two-stop ramp is baked at its own stops, not at the zoom passed in"
        );

        let snapshot = store.snapshot();
        let width_at = |zoom| match &snapshot
            .plan_at(plan::BakeBracket::Bucket(zoom))
            .unwrap()
            .plan
            .ops[0]
        {
            crate::style::PaintOp::Line { width_from, .. } => *width_from,
            other => panic!("expected a line op, got {other:?}"),
        };
        assert!((width_at(9) - 2.88).abs() < 1e-4, "z9 was {}", width_at(9));
        assert!((width_at(16) - 6.24).abs() < 1e-4, "z16 was {}", width_at(16));
    }

    /// The bracket a collection is baked at is chosen from its FEATURES, not
    /// from the camera -- so the same GeoJSON gives the same plan whatever
    /// the camera happened to be doing when it arrived. That is what makes
    /// a ramped overlay's mesh valid at every zoom instead of only at one.
    #[test]
    fn a_ramped_collection_bakes_the_same_bracket_whatever_zoom_it_arrives_at() {
        const RAMPED: &str = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":{"type":"LineString",
             "coordinates":[[103.85,1.29],[103.86,1.30]]},
             "properties":{"line-width":["interpolate",["linear"],["zoom"],8,2.4,18,7.2]}}]}"#;
        let bracket_at = |zoom| {
            let mut store = OverlayStore::new();
            store.replace(RAMPED, zoom).unwrap();
            store.plan().unwrap().bracket
        };
        assert_eq!(bracket_at(3), bracket_at(19));
        assert_eq!(bracket_at(3), plan::BakeBracket::Ramp { lo: 8, hi: 18 });
    }

    /// Two features of ONE SOURCE ramping over DIFFERENT zoom ranges
    /// cannot share one bracket, because a source is a single draw call
    /// with a single `blend_t` uniform. The store must notice and fall
    /// back rather than pick one range and draw the other feature wrong.
    ///
    /// Two features in DIFFERENT sources may ramp differently and both
    /// stay exact -- each source is its own draw with its own weight. See
    /// `a_step_width_in_one_source_does_not_drag_another_onto_the_bucket_
    /// path`.
    #[test]
    fn features_that_ramp_over_different_ranges_fall_back_to_the_camera_bucket() {
        const MIXED: &str = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":{"type":"LineString",
             "coordinates":[[103.85,1.29],[103.86,1.30]]},
             "properties":{"line-width":["interpolate",["linear"],["zoom"],8,2.4,18,7.2]}},
            {"type":"Feature","geometry":{"type":"LineString",
             "coordinates":[[103.87,1.29],[103.88,1.30]]},
             "properties":{"line-width":["interpolate",["linear"],["zoom"],10,1.0,14,3.0]}}]}"#;
        let mut store = OverlayStore::new();
        store.replace(MIXED, 11).unwrap();
        assert_eq!(store.plan().unwrap().bracket, plan::BakeBracket::Bucket(11));
    }

    /// A constant width is exact under ANY bracket, so a collection of them
    /// takes the free path too. Before the bracket existed this case still
    /// paid a full replan and vertex rewrite at every integer zoom, writing
    /// back the values the vertices already held.
    #[test]
    fn an_all_constant_collection_is_baked_once_rather_than_per_bucket() {
        let mut store = OverlayStore::new();
        store.replace(ONE_ROUTE, 12).unwrap();
        assert!(matches!(store.plan().unwrap().bracket, plan::BakeBracket::Ramp { .. }));
    }

    /// A constant alongside a ramp does not block the ramp: the constant
    /// resolves the same at the ramp's two stops as anywhere else, so the
    /// two coexist under one bracket.
    #[test]
    fn a_constant_width_beside_a_ramp_still_takes_the_ramp_bracket() {
        const MIXED: &str = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":{"type":"LineString",
             "coordinates":[[103.85,1.29],[103.86,1.30]]},
             "properties":{"line-width":6}},
            {"type":"Feature","geometry":{"type":"LineString",
             "coordinates":[[103.87,1.29],[103.88,1.30]]},
             "properties":{"line-width":["interpolate",["linear"],["zoom"],8,2.4,18,7.2]}}]}"#;
        let mut store = OverlayStore::new();
        store.replace(MIXED, 11).unwrap();
        let built = store.plan().unwrap();
        assert_eq!(built.bracket, plan::BakeBracket::Ramp { lo: 8, hi: 18 });
        // And the constant is still 6px at both ends of that bracket, so the
        // shader's `mix` stays a no-op for it at every zoom.
        match &built.plan.ops[0] {
            crate::style::PaintOp::Line { width_from, width_to, .. } => {
                assert_eq!((*width_from, *width_to), (6.0, 6.0));
            }
            other => panic!("expected a line op, got {other:?}"),
        }
    }

    /// A label-only generation has paint but no triangles. Re-resolving one
    /// must report that, not panic -- `MapView` asks every frame the display
    /// bucket moves, and it does not know in advance which kind it holds.
    #[test]
    fn re_resolving_a_label_only_generation_yields_no_plan_rather_than_panicking() {
        let mut store = OverlayStore::new();
        store
            .replace(
                r#"{"type":"FeatureCollection","features":[
                   {"type":"Feature","geometry":null,"properties":{"text-field":"42"}}]}"#,
                12,
            )
            .unwrap();
        assert!(store.snapshot().plan_at(plan::BakeBracket::Bucket(16)).is_none());
    }

    #[test]
    fn a_feature_ref_carries_the_index_and_the_optional_id() {
        let mut store = OverlayStore::new();
        store.replace(ONE_ROUTE, 12).unwrap();
        let reference = FeatureRef::of(&DEFAULT_SOURCE.into(), &store.features()[0]);
        assert_eq!(reference.index, 0);
        assert_eq!(reference.id, Some(FeatureId::String("route-42".into())));
    }

    const TWO_ROUTES: &str = r#"{"type":"FeatureCollection","features":[
        {"type":"Feature","id":"route-a","geometry":{"type":"LineString",
         "coordinates":[[103.85,1.29],[103.86,1.30]]},"properties":{}},
        {"type":"Feature","id":42,"geometry":{"type":"LineString",
         "coordinates":[[103.87,1.29],[103.88,1.30]]},"properties":{}}]}"#;

    /// A paint update names features by `id`, so the store keeps the map
    /// from id to feature index rather than making every update scan.
    #[test]
    fn a_feature_is_addressable_by_its_id_whichever_form_the_id_takes() {
        let mut store = OverlayStore::new();
        store.replace(TWO_ROUTES, 12).unwrap();
        assert_eq!(store.index_of(&FeatureId::String("route-a".into())), Some(0));
        assert_eq!(store.index_of(&FeatureId::Number(42.0)), Some(1));
    }

    /// An id matching no feature is an error naming it, not a silent no-op:
    /// a caller holding a stale id has a real bug, and quietly recolouring
    /// nothing is how it would stay hidden.
    #[test]
    fn an_unknown_id_is_an_error_that_names_it() {
        let mut store = OverlayStore::new();
        store.replace(TWO_ROUTES, 12).unwrap();
        assert_eq!(store.index_of(&FeatureId::String("route-z".into())), None);
        let error = store.resolve_id(&FeatureId::String("route-z".into())).unwrap_err();
        assert_eq!(error, FeatureError::UnknownId(FeatureId::String("route-z".into())));
        assert!(error.to_string().contains("route-z"), "message was {error}");
    }

    /// A feature with no `id` is unaddressable. That is inherent to naming
    /// things by id, not a gap to paper over with an index fallback.
    #[test]
    fn a_feature_without_an_id_is_not_addressable() {
        let mut store = OverlayStore::new();
        store
            .replace(
                r#"{"type":"FeatureCollection","features":[
                   {"type":"Feature","geometry":{"type":"LineString",
                    "coordinates":[[0,0],[1,1]]},"properties":{}}]}"#,
                12,
            )
            .unwrap();
        assert_eq!(store.features().len(), 1);
        assert_eq!(store.index_of(&FeatureId::String("anything".into())), None);
    }

    /// Two features sharing an id make the COLLECTION malformed, so it is
    /// refused when it is built -- not later, when an update fails to find
    /// out which of the two it meant.
    #[test]
    fn a_duplicate_id_is_rejected_at_replace_time() {
        let mut store = OverlayStore::new();
        store.replace(ONE_ROUTE, 12).unwrap();
        let error = store
            .replace(
                r#"{"type":"FeatureCollection","features":[
                   {"type":"Feature","id":"twice","geometry":{"type":"LineString",
                    "coordinates":[[0,0],[1,1]]},"properties":{}},
                   {"type":"Feature","id":"twice","geometry":{"type":"LineString",
                    "coordinates":[[2,2],[3,3]]},"properties":{}}]}"#,
                12,
            )
            .unwrap_err();
        match &error {
            FeatureError::Feature { index, id, reason } => {
                assert_eq!(*index, 1, "the SECOND occurrence is the one refused");
                assert_eq!(*id, Some(FeatureId::String("twice".into())));
                assert!(reason.contains("duplicate"), "reason was {reason}");
            }
            other => panic!("expected a feature error, got {other:?}"),
        }
        // Rejected before anything was written, like every other failure.
        assert_eq!(store.features().len(), 1);
        assert_eq!(store.features()[0].id, Some(FeatureId::String("route-42".into())));
        assert_eq!(store.generation(), 1, "a failure is not a generation");
    }

    /// A numeric duplicate is caught too -- the map keys numbers by their
    /// bits, and `0.0`/`-0.0` must still collide the way `f64` equality
    /// says they do.
    #[test]
    fn a_duplicate_numeric_id_is_rejected_including_signed_zero() {
        let mut store = OverlayStore::new();
        assert!(store
            .replace(
                r#"{"type":"FeatureCollection","features":[
                   {"type":"Feature","id":0,"geometry":{"type":"LineString",
                    "coordinates":[[0,0],[1,1]]},"properties":{}},
                   {"type":"Feature","id":-0.0,"geometry":{"type":"LineString",
                    "coordinates":[[2,2],[3,3]]},"properties":{}}]}"#,
                12,
            )
            .is_err());
    }

    /// Ids from the PREVIOUS collection stop resolving once a replacement
    /// lands -- the map is rebuilt with the features, like the hit index.
    #[test]
    fn the_id_map_is_rebuilt_on_every_replacement() {
        let mut store = OverlayStore::new();
        store.replace(ONE_ROUTE, 12).unwrap();
        assert_eq!(store.index_of(&FeatureId::String("route-42".into())), Some(0));
        store.replace(r#"{"type":"FeatureCollection","features":[]}"#, 12).unwrap();
        assert_eq!(store.index_of(&FeatureId::String("route-42".into())), None);
    }

    // --- paint-only updates -------------------------------------------

    fn paint_from(properties: &str) -> FeaturePaint {
        let map: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(properties).unwrap();
        FeaturePaint::from_properties(&map).unwrap()
    }

    const A_AND_B: &str = r##"{"type":"FeatureCollection","features":[
        {"type":"Feature","id":"a","geometry":{"type":"LineString",
         "coordinates":[[103.85,1.29],[103.86,1.30]]},
         "properties":{"line-color":"#ff0000","line-width":4}},
        {"type":"Feature","id":"b","geometry":{"type":"LineString",
         "coordinates":[[103.87,1.29],[103.88,1.30]]},
         "properties":{"line-color":"#00ff00","line-width":4}}]}"##;

    /// The whole point: the named feature's paint changes and nothing else
    /// does -- not the geometry, not the other feature, not the generation.
    #[test]
    fn a_paint_update_changes_only_the_named_features_paint() {
        let mut store = OverlayStore::new();
        store.replace(A_AND_B, 12).unwrap();
        let geometry_before = store.features()[0].geometry.clone();
        let b_before = store.features()[1].paint.clone();

        let updates =
            vec![(FeatureId::String("a".into()), paint_from(r##"{"line-color":"#0000ff"}"##))];
        let update = store.plan_paint_update(&updates).unwrap();
        assert!(!update.needs_rebake());
        store.apply_paint_in_place(&updates, &update);

        assert_eq!(store.features()[0].paint.line_color.b, 1.0, "a is now blue");
        assert_eq!(store.features()[0].geometry, geometry_before, "geometry is untouched");
        assert_eq!(store.features()[1].paint, b_before, "b is untouched");
    }

    /// NO BAKE IS SPAWNED, so the generation must not move. It is the key
    /// the GPU upload is cached under (`MapView::paint`), and bumping it
    /// forces a full re-upload of the whole vertex buffer -- precisely the
    /// cost this path exists to avoid.
    #[test]
    fn a_paint_update_does_not_move_the_generation() {
        let mut store = OverlayStore::new();
        store.replace(A_AND_B, 12).unwrap();
        let generation = store.generation();
        let updates =
            vec![(FeatureId::String("a".into()), paint_from(r##"{"line-color":"#0000ff"}"##))];
        let update = store.plan_paint_update(&updates).unwrap();
        store.apply_paint_in_place(&updates, &update);
        assert_eq!(store.generation(), generation);
    }

    /// The op list is the named features' ops and no others -- this is what
    /// the mesh rewrite and the GPU upload are both restricted to, so a
    /// wrong entry here is a wrong-coloured line somewhere else.
    #[test]
    fn a_paint_update_names_exactly_the_ops_of_the_features_it_updates() {
        let mut store = OverlayStore::new();
        store.replace(A_AND_B, 12).unwrap();
        let updates =
            vec![(FeatureId::String("b".into()), paint_from(r##"{"line-color":"#0000ff"}"##))];
        assert_eq!(store.plan_paint_update(&updates).unwrap().ops(), &[1]);
    }

    /// A feature that draws nothing has no ops to rewrite, and updating its
    /// paint is still legitimate -- it carries a label and an icon.
    #[test]
    fn updating_a_feature_that_draws_nothing_names_no_ops() {
        let mut store = OverlayStore::new();
        store
            .replace(
                r#"{"type":"FeatureCollection","features":[
                   {"type":"Feature","id":"label","geometry":null,
                    "properties":{"text-field":"42"}},
                   {"type":"Feature","id":"line","geometry":{"type":"LineString",
                    "coordinates":[[0,0],[1,1]]},"properties":{}}]}"#,
                12,
            )
            .unwrap();
        let updates =
            vec![(FeatureId::String("label".into()), paint_from(r##"{"icon-color":"#00ff00"}"##))];
        let update = store.plan_paint_update(&updates).unwrap();
        assert!(update.ops().is_empty());
        store.apply_paint_in_place(&updates, &update);
        assert_eq!(store.features()[0].paint.icon_color.g, 1.0);
    }

    /// One call, two features, each named once.
    #[test]
    fn a_batch_names_each_feature_and_op_exactly_once() {
        let mut store = OverlayStore::new();
        store.replace(A_AND_B, 12).unwrap();
        let updates = vec![
            (FeatureId::String("b".into()), paint_from(r##"{"line-color":"#0000ff"}"##)),
            (FeatureId::String("a".into()), paint_from(r##"{"line-color":"#0000ff"}"##)),
        ];
        let update = store.plan_paint_update(&updates).unwrap();
        assert_eq!(update.ops(), &[0, 1], "ascending, and each op once");
        store.apply_paint_in_place(&updates, &update);
        assert_eq!(store.features()[0].paint.line_color.b, 1.0);
        assert_eq!(store.features()[1].paint.line_color.b, 1.0);
    }

    /// The same id twice in one call is the caller's business, not an
    /// error: the last write wins, and the op is still rewritten once.
    #[test]
    fn the_same_id_twice_in_one_call_takes_the_last_paint_and_one_op_entry() {
        let mut store = OverlayStore::new();
        store.replace(A_AND_B, 12).unwrap();
        let updates = vec![
            (FeatureId::String("a".into()), paint_from(r##"{"line-color":"#0000ff"}"##)),
            (FeatureId::String("a".into()), paint_from(r##"{"line-color":"#00ff00"}"##)),
        ];
        let update = store.plan_paint_update(&updates).unwrap();
        assert_eq!(update.ops(), &[0]);
        store.apply_paint_in_place(&updates, &update);
        assert_eq!(store.features()[0].paint.line_color.g, 1.0, "the last write wins");
    }

    /// An unknown id fails the whole call before anything is written.
    #[test]
    fn an_unknown_id_in_an_update_changes_nothing() {
        let mut store = OverlayStore::new();
        store.replace(A_AND_B, 12).unwrap();
        let before = store.features().to_vec();
        let updates = vec![
            (FeatureId::String("a".into()), paint_from(r##"{"line-color":"#0000ff"}"##)),
            (FeatureId::String("ghost".into()), paint_from(r##"{"line-color":"#0000ff"}"##)),
        ];
        let error = store.plan_paint_update(&updates).unwrap_err();
        assert_eq!(error, FeatureError::UnknownId(FeatureId::String("ghost".into())));
        assert_eq!(store.features()[0].paint, before[0].paint, "not even the valid one");
        assert_eq!(store.generation(), 1);
    }

    /// A `line-width` ramp that stops agreeing with the rest of the
    /// collection flips the whole thing's `BakeBracket`, which invalidates
    /// EVERY vertex -- not just the updated feature's. That is the one case
    /// the fast path cannot serve, and it must say so rather than patch a
    /// mesh whose other 205 features are now baked wrong.
    #[test]
    fn a_width_ramp_that_flips_the_bracket_asks_for_a_rebake() {
        let mut store = OverlayStore::new();
        store.replace(A_AND_B, 12).unwrap();
        assert_eq!(store.plan().unwrap().bracket, plan::BakeBracket::Ramp { lo: 0, hi: 1 });
        let updates = vec![(
            FeatureId::String("a".into()),
            paint_from(r#"{"line-width":["interpolate",["linear"],["zoom"],8,2.0,18,7.0]}"#),
        )];
        assert!(store.plan_paint_update(&updates).unwrap().needs_rebake());
    }

    /// A width change that leaves the shared ramp alone does NOT.
    #[test]
    fn a_width_change_within_the_same_ramp_stays_on_the_fast_path() {
        let mut store = OverlayStore::new();
        store.replace(A_AND_B, 12).unwrap();
        let updates = vec![(FeatureId::String("a".into()), paint_from(r#"{"line-width":9}"#))];
        let update = store.plan_paint_update(&updates).unwrap();
        assert!(!update.needs_rebake());
        store.apply_paint_in_place(&updates, &update);
        match &store.plan().unwrap().plan.ops[0] {
            crate::style::PaintOp::Line { width_from, width_to, .. } => {
                assert_eq!((*width_from, *width_to), (9.0, 9.0));
            }
            other => panic!("expected a line op, got {other:?}"),
        }
    }

    /// `circle-radius: 0` is not a paint value, it is "draw no dot at all"
    /// -- `build_plan` skips the op entirely. Crossing that boundary
    /// changes the OP SET, which no in-place rewrite can express, so it
    /// asks for a rebake too.
    #[test]
    fn turning_a_dot_off_asks_for_a_rebake() {
        let mut store = OverlayStore::new();
        store
            .replace(
                r#"{"type":"FeatureCollection","features":[
                   {"type":"Feature","id":"stop","geometry":{"type":"Point",
                    "coordinates":[103.85,1.29]},"properties":{"circle-radius":5}}]}"#,
                12,
            )
            .unwrap();
        let off = vec![(
            FeatureId::String("stop".into()),
            paint_from(r#"{"circle-radius":0,"icon-image":"pin"}"#),
        )];
        assert!(store.plan_paint_update(&off).unwrap().needs_rebake());
        // ...and back on again, from a collection that has no dot.
        let mut store = OverlayStore::new();
        store
            .replace(
                r#"{"type":"FeatureCollection","features":[
                   {"type":"Feature","id":"stop","geometry":{"type":"Point",
                    "coordinates":[103.85,1.29]},"properties":{"circle-radius":0}},
                   {"type":"Feature","geometry":{"type":"LineString",
                    "coordinates":[[0,0],[1,1]]},"properties":{}}]}"#,
                12,
            )
            .unwrap();
        let on = vec![(FeatureId::String("stop".into()), paint_from(r#"{"circle-radius":5}"#))];
        assert!(store.plan_paint_update(&on).unwrap().needs_rebake());
    }

    /// A dot that stays a dot only changes colour and diameter, both of
    /// which are vertex attributes.
    #[test]
    fn recolouring_a_dot_that_stays_a_dot_stays_on_the_fast_path() {
        let mut store = OverlayStore::new();
        store
            .replace(
                r#"{"type":"FeatureCollection","features":[
                   {"type":"Feature","id":"stop","geometry":{"type":"Point",
                    "coordinates":[103.85,1.29]},"properties":{"circle-radius":5}}]}"#,
                12,
            )
            .unwrap();
        let updates = vec![(
            FeatureId::String("stop".into()),
            paint_from(r##"{"circle-radius":8,"circle-color":"#0000ff"}"##),
        )];
        let update = store.plan_paint_update(&updates).unwrap();
        assert!(!update.needs_rebake());
        store.apply_paint_in_place(&updates, &update);
        match &store.plan().unwrap().plan.ops[0] {
            crate::style::PaintOp::Line { color_from, width_from, .. } => {
                assert_eq!(*width_from, 16.0, "the dot's stroke width is its DIAMETER");
                assert_eq!(color_from.b, 1.0);
            }
            other => panic!("expected a line op, got {other:?}"),
        }
    }

    /// THE EQUIVALENCE PROPERTY, at the plan level: an in-place update must
    /// leave the store holding exactly what a full replacement of the same
    /// end state would have built. The two paths have to agree forever, and
    /// this is what holds them together.
    #[test]
    fn an_in_place_update_leaves_the_plan_a_full_replace_would_have_built() {
        let mut patched = OverlayStore::new();
        patched.replace(A_AND_B, 12).unwrap();
        let updates = vec![(
            FeatureId::String("a".into()),
            paint_from(r##"{"line-color":"#0000ff","line-width":7,"line-dasharray":[2,3]}"##),
        )];
        let update = patched.plan_paint_update(&updates).unwrap();
        patched.apply_paint_in_place(&updates, &update);

        let mut pushed = OverlayStore::new();
        pushed
            .replace(
                &A_AND_B.replace(
                    r##""line-color":"#ff0000","line-width":4"##,
                    r##""line-color":"#0000ff","line-width":7,"line-dasharray":[2,3]"##,
                ),
                12,
            )
            .unwrap();

        assert_eq!(
            patched.plan().unwrap().plan.ops,
            pushed.plan().unwrap().plan.ops,
            "the patched plan must be the pushed plan"
        );
        assert_eq!(patched.plan().unwrap().bracket, pushed.plan().unwrap().bracket);
        assert_eq!(patched.features()[0].paint, pushed.features()[0].paint);
    }

    /// The rebake path writes the paint AND replans, and it IS a new
    /// generation -- every vertex has to be baked again, so the mesh on
    /// screen is a generation behind until it lands, exactly as it is after
    /// a `replace`.
    #[test]
    fn a_replan_writes_the_paint_bumps_the_generation_and_rebuilds_the_plan() {
        let mut store = OverlayStore::new();
        store.replace(A_AND_B, 12).unwrap();
        let updates = vec![(
            FeatureId::String("a".into()),
            paint_from(r#"{"line-width":["interpolate",["linear"],["zoom"],8,2.0,18,7.0]}"#),
        )];
        let update = store.plan_paint_update(&updates).unwrap();
        assert!(update.needs_rebake());
        store.apply_paint_and_replan(&updates, &update, 12);
        assert_eq!(store.generation(), 2);
        assert_eq!(
            store.plan().unwrap().bracket,
            plan::BakeBracket::Ramp { lo: 8, hi: 18 },
            "the collection now agrees on a's ramp, which is a different bracket entirely"
        );
    }

    // ---------------------------------------------------------------
    // Sources
    // ---------------------------------------------------------------

    /// Two dots with CONSTANT paint -- so this source takes a `Ramp`
    /// bracket and is genuinely zoom-independent.
    pub(super) const STATIC_SOURCE: &str = r#"{"type":"FeatureCollection","features":[
        {"type":"Feature","id":"stop-1","geometry":{"type":"Point",
         "coordinates":[103.85,1.29]},"properties":{"circle-radius":4}},
        {"type":"Feature","id":"stop-2","geometry":{"type":"Point",
         "coordinates":[103.86,1.30]},"properties":{"circle-radius":4}}]}"#;

    /// One route whose width is a `step` -- so no fixed bracket can
    /// reproduce it, and it fails closed onto `Bucket`.
    const STEPPED_SOURCE: &str = r#"{"type":"FeatureCollection","features":[
        {"type":"Feature","id":"bus-1","geometry":{"type":"LineString",
         "coordinates":[[103.90,1.40],[103.91,1.41]]},
         "properties":{"line-width":["step",["zoom"],2,10,4,14,8]}}]}"#;

    /// THE test the whole change exists for, at the store: a push to one
    /// source does not move another source's stamp, so nothing downstream
    /// of that stamp -- the plan, the mesh, the hit index, the GPU
    /// buffers -- is rebuilt for it.
    ///
    /// The consumer reported this as "hiding the bus-stop layer makes the
    /// buses move faster". It was true, and this is where it was true.
    #[test]
    fn pushing_one_source_leaves_every_other_sources_stamp_and_plan_alone() {
        let mut store = OverlayStore::new();
        store.set_source("stops", STATIC_SOURCE, 12).unwrap();
        store.set_source("vehicles", STEPPED_SOURCE, 12).unwrap();

        let stops_stamp = store.source_generation("stops").unwrap();
        let stops_plan = format!("{:?}", store.source_plan("stops").unwrap());

        for _ in 0..10 {
            store.set_source("vehicles", STEPPED_SOURCE, 12).unwrap();
        }

        assert_eq!(
            store.source_generation("stops").unwrap(),
            stops_stamp,
            "ten pushes to `vehicles` must not move `stops`'s stamp"
        );
        assert_eq!(
            format!("{:?}", store.source_plan("stops").unwrap()),
            stops_plan,
            "nor rebuild its plan"
        );
        assert!(
            store.source_generation("vehicles").unwrap() > stops_stamp,
            "`vehicles` did move, or this test is measuring nothing"
        );
    }

    /// Every source gets its own origin and its own bracket, which is why
    /// a source is the unit a plan is built at rather than a range inside
    /// one. See `OverlayPlan`'s doc comment.
    #[test]
    fn each_source_gets_its_own_origin_and_its_own_bake_bracket() {
        let mut store = OverlayStore::new();
        store.set_source("singapore", STATIC_SOURCE, 12).unwrap();
        store
            .set_source(
                "new-york",
                r#"{"type":"FeatureCollection","features":[
                    {"type":"Feature","geometry":{"type":"LineString",
                     "coordinates":[[-74.01,40.70],[-74.00,40.71]]},
                     "properties":{"line-width":["interpolate",["linear"],["zoom"],8,2,18,7]}}]}"#,
                12,
            )
            .unwrap();

        let one = store.source_plan("singapore").unwrap();
        let two = store.source_plan("new-york").unwrap();
        assert_ne!(
            (one.origin.world_x, one.origin.world_y),
            (two.origin.world_x, two.origin.world_y),
            "two sources a hemisphere apart cannot share an origin"
        );
        assert_eq!(one.bracket, plan::BakeBracket::Ramp { lo: 0, hi: 1 });
        assert_eq!(two.bracket, plan::BakeBracket::Ramp { lo: 8, hi: 18 });
    }

    /// The bracket win, stated as the thing it fixes: a `step` width in
    /// ONE source used to put every other source on the `Bucket` path with
    /// it, because `shared_width_ramp` was asked of the whole overlay.
    #[test]
    fn a_step_width_in_one_source_does_not_drag_another_onto_the_bucket_path() {
        let mut store = OverlayStore::new();
        store.set_source("stops", STATIC_SOURCE, 12).unwrap();
        store.set_source("vehicles", STEPPED_SOURCE, 12).unwrap();

        assert_eq!(
            store.source_plan("stops").unwrap().bracket,
            plan::BakeBracket::Ramp { lo: 0, hi: 1 },
            "the stops' widths are constant, so their bracket is exact at every zoom"
        );
        assert_eq!(
            store.source_plan("vehicles").unwrap().bracket,
            plan::BakeBracket::Bucket(12),
            "a step width fails closed onto the camera's bucket"
        );
        assert_eq!(store.source_is_zoom_independent("stops"), Some(true));
        assert_eq!(store.source_is_zoom_independent("vehicles"), Some(false));
    }

    /// A declaration is an ASSERTION, checked, and the refusal names the
    /// feature responsible -- not a hint that is silently dropped.
    #[test]
    fn declaring_zoom_independence_over_a_step_width_is_refused_and_names_the_feature() {
        let mut store = OverlayStore::new();
        store
            .declare_source("vehicles", SourceOptions { zoom_independent: true, ..Default::default() })
            .expect("an empty source promises nothing it cannot keep");

        let error = store.set_source("vehicles", STEPPED_SOURCE, 12).unwrap_err();
        match error {
            FeatureError::Feature { index, id, reason } => {
                assert_eq!(index, 0);
                assert_eq!(id, Some(FeatureId::String("bus-1".into())));
                assert!(reason.contains("zoom-independent"), "reason was {reason}");
                assert!(reason.contains("line-width"), "reason was {reason}");
            }
            other => panic!("expected a feature error, got {other:?}"),
        }
        assert!(
            store.source_features("vehicles").unwrap().is_empty(),
            "a rejected push changes nothing"
        );
    }

    /// The same declaration over widths that DO admit a fixed bracket is
    /// accepted, and sticks: the source keeps it across pushes, which is
    /// what lets the hot path stay two arguments.
    #[test]
    fn a_zoom_independence_declaration_is_accepted_and_survives_later_pushes() {
        let mut store = OverlayStore::new();
        store.declare_source("stops", SourceOptions { zoom_independent: true, ..Default::default() }).unwrap();
        store.set_source("stops", STATIC_SOURCE, 12).unwrap();
        assert_eq!(store.source_options("stops"), Some(SourceOptions { zoom_independent: true, ..Default::default() }));
        assert_eq!(store.source_is_zoom_independent("stops"), Some(true));

        // The declaration is still in force, so a later push that breaks it
        // is refused rather than quietly costing a re-resolve per crossing.
        let error = store.set_source("stops", STEPPED_SOURCE, 12).unwrap_err();
        assert!(matches!(error, FeatureError::Feature { .. }), "got {error:?}");
        assert_eq!(
            store.source_features("stops").unwrap().len(),
            2,
            "and the accepted features are still there"
        );
    }

    /// Declaring over features that ALREADY deny the promise fails at the
    /// declaration, not at the next push -- a promise accepted against
    /// data that breaks it is worse than one refused.
    #[test]
    fn declaring_zoom_independence_over_features_that_already_deny_it_fails_at_once() {
        let mut store = OverlayStore::new();
        store.set_source("vehicles", STEPPED_SOURCE, 12).unwrap();
        let error =
            store.declare_source("vehicles", SourceOptions { zoom_independent: true, ..Default::default() }).unwrap_err();
        assert!(matches!(error, FeatureError::Feature { .. }), "got {error:?}");
        assert_eq!(
            store.source_options("vehicles"),
            Some(SourceOptions::default()),
            "a refused declaration is not recorded"
        );
    }

    /// Draw order is push order, and a re-push keeps its place. A feed
    /// re-pushing itself every tick must not shuffle above or below its
    /// neighbours.
    #[test]
    fn sources_keep_their_push_order_across_re_pushes() {
        let mut store = OverlayStore::new();
        store.set_source("a", STATIC_SOURCE, 12).unwrap();
        store.set_source("b", STATIC_SOURCE, 12).unwrap();
        store.set_source("c", STATIC_SOURCE, 12).unwrap();
        let order = |store: &OverlayStore| -> Vec<String> {
            store.source_names().map(|name| name.to_string()).collect()
        };
        assert_eq!(order(&store), ["", "a", "b", "c"]);

        store.set_source("a", STATIC_SOURCE, 12).unwrap();
        assert_eq!(order(&store), ["", "a", "b", "c"], "a re-push keeps its slot");

        store.remove_source("b");
        assert_eq!(order(&store), ["", "a", "c"]);
        store.set_source("b", STATIC_SOURCE, 12).unwrap();
        assert_eq!(order(&store), ["", "a", "c", "b"], "a re-added source is a new source");
    }

    /// `set_source_order` puts the named sources in the given order among
    /// their own slots; an unnamed source, an unknown name, a repeat and
    /// the default source stay where they are, and a later re-push keeps
    /// the new place.
    #[test]
    fn set_source_order_permutes_only_the_named_sources() {
        let mut store = OverlayStore::new();
        for name in ["a", "b", "x", "c"] {
            store.set_source(name, STATIC_SOURCE, 12).unwrap();
        }
        let order = |store: &OverlayStore| -> Vec<String> { store.source_names().map(|name| name.to_string()).collect() };
        assert!(store.set_source_order(&["c", "nope", "a", "b", "c", ""]));
        assert_eq!(order(&store), ["", "c", "a", "x", "b"], "x keeps its slot; c, a, b fill a, b, c's");
        assert!(!store.set_source_order(&["c", "a", "b"]), "already in that order: nothing moves");
        store.set_source("a", STATIC_SOURCE, 12).unwrap();
        assert_eq!(order(&store), ["", "c", "a", "x", "b"], "a re-push keeps the reordered slot");
        assert!(store.set_source_order(&["b", "c"]));
        assert_eq!(order(&store), ["", "b", "a", "x", "c"]);
        assert!(store.source_features("b").is_some_and(|f| !f.is_empty()), "a moved source keeps its features");
    }

    #[test]
    fn permute_among_slots_keeps_the_unpicked_in_place() {
        let mut v = vec!['a', 'b', 'c', 'd', 'e'];
        assert!(permute_among_slots(&mut v, vec![3, 1, 9, 3]));
        assert_eq!(v, ['a', 'd', 'c', 'b', 'e']);
        assert!(!permute_among_slots(&mut v, vec![0, 2, 4]));
        assert!(!permute_among_slots(&mut v, vec![]));
    }

    /// A removed source publishes its successor stamp before it is
    /// dropped, so a bake of it still cancels itself. The `Arc` a bake
    /// holds is what makes that observable after the source is gone.
    #[test]
    fn removing_a_source_cancels_a_bake_of_it_and_leaves_its_neighbours() {
        let mut store = OverlayStore::new();
        store.set_source("stops", STATIC_SOURCE, 12).unwrap();
        store.set_source("vehicles", STATIC_SOURCE, 12).unwrap();

        // What a bake of `stops` would carry onto its worker.
        let baked_for = store.source_generation("stops").unwrap();
        let live = store.source_live_generation("stops").unwrap();
        assert_eq!(live.load(std::sync::atomic::Ordering::Relaxed), baked_for, "not yet stale");

        let vehicles_stamp = store.source_generation("vehicles").unwrap();
        assert!(store.remove_source("stops"));

        assert!(
            baked_for < live.load(std::sync::atomic::Ordering::Relaxed),
            "the removal must make an in-flight bake of `stops` cancel itself"
        );
        assert!(store.source_generation("stops").is_none(), "and the source is gone");
        assert_eq!(
            store.source_generation("vehicles").unwrap(),
            vehicles_stamp,
            "removing one source does not touch another"
        );
    }

    /// The default source is what `replace` and `set_features` read and
    /// write, so it is CLEARED rather than removed -- `features()` must not
    /// start lying about a store that still works.
    #[test]
    fn removing_the_default_source_clears_it_rather_than_deleting_it() {
        let mut store = OverlayStore::new();
        store.replace(STATIC_SOURCE, 12).unwrap();
        assert!(store.remove_source(DEFAULT_SOURCE));

        assert!(store.features().is_empty());
        assert!(store.plan().is_none());
        assert_eq!(
            store.source_names().map(|name| name.to_string()).collect::<Vec<_>>(),
            [""],
            "the default source is still there, and still first"
        );
    }

    /// An id is unique WITHIN a source, which is exactly why a
    /// `FeatureRef` carries the source name: two layers legitimately both
    /// have a feature called `stop-1`.
    #[test]
    fn ids_need_only_be_unique_within_a_source() {
        let mut store = OverlayStore::new();
        store.set_source("stops", STATIC_SOURCE, 12).unwrap();
        store.set_source("shadows", STATIC_SOURCE, 12).unwrap();

        let id = FeatureId::String("stop-1".into());
        assert_eq!(store.source_index_of("stops", &id), Some(0));
        assert_eq!(store.source_index_of("shadows", &id), Some(0));

        // And a paint update against one is resolved in that one only.
        let update = store
            .plan_source_paint_update(
                "stops",
                &[(id.clone(), paint_from(r##"{"circle-radius":9}"##))],
            )
            .unwrap();
        assert_eq!(&**update.source(), "stops");
    }

    /// A name that is not a source is an error naming it, not a silent
    /// no-op: a typo in a source name must not become a layer that never
    /// appears and nobody can explain.
    #[test]
    fn an_update_against_an_unknown_source_is_an_error_that_names_it() {
        let mut store = OverlayStore::new();
        let error = store
            .plan_source_paint_update(
                "typo",
                &[(FeatureId::String("stop-1".into()), paint_from("{}"))],
            )
            .unwrap_err();
        match error {
            FeatureError::UnknownSource(name) => assert_eq!(name, "typo"),
            other => panic!("expected an unknown-source error, got {other:?}"),
        }
        assert!(!store.remove_source("typo"), "and removing one reports that there was none");
    }

    /// `set_features`/`replace` are the default source and nothing else,
    /// so a caller who never mentions a source sees exactly the old
    /// behaviour -- including the store-wide `generation()` they may be
    /// watching.
    #[test]
    fn the_pre_sources_api_is_the_default_source_and_moves_the_store_counter() {
        let mut store = OverlayStore::new();
        assert_eq!(store.generation(), 0);
        store.replace(STATIC_SOURCE, 12).unwrap();
        assert_eq!(store.generation(), 1);
        assert_eq!(store.source_generation(DEFAULT_SOURCE), Some(1));
        assert_eq!(store.features().len(), 2);

        // And a named source shares the counter without disturbing the
        // default source's own stamp.
        store.set_source("vehicles", STATIC_SOURCE, 12).unwrap();
        assert_eq!(store.generation(), 2, "the store counter is store-wide");
        assert_eq!(
            store.source_generation(DEFAULT_SOURCE),
            Some(1),
            "the default source did not change, so its stamp did not move"
        );
    }
}

/// A source whose paint is an expression over its features' properties:
/// what it collapses, what it must NOT change, and the guards around it.
#[cfg(test)]
mod data_driven_tests {
    use super::*;
    use input::{DataFeatureInput, FeatureInput, GeometryInput};

    const ROUTES: usize = 12;
    const PALETTE: [&str; ROUTES] = [
        "#e6194b", "#3cb44b", "#ffe119", "#4363d8", "#f58231", "#911eb4", "#46f0f0", "#f032e6",
        "#bcf60c", "#fabebe", "#008080", "#e6beff",
    ];

    fn declaration(json: &str) -> serde_json::Map<String, serde_json::Value> {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn the_declared_paint_reads_back_as_given_and_a_refused_one_changes_nothing() {
        let mut store = OverlayStore::new();
        assert!(store.declared_source_paint("s").is_none(), "no such source");
        let first = declaration(r##"{"circle-color": ["step", ["get", "pop"], "#000000", 10, "#ffffff"], "circle-radius": 4}"##);
        store.declare_source_paint("s", &first).unwrap();
        assert_eq!(store.declared_source_paint("s"), Some(&first));
        // A malformed declaration is refused and the old one stands.
        assert!(store.declare_source_paint("s", &declaration(r#"{"circle-color": ["nope"]}"#)).is_err());
        assert_eq!(store.declared_source_paint("s"), Some(&first));
        let second = declaration(r##"{"circle-color": "#123456"}"##);
        store.declare_source_paint("s", &second).unwrap();
        assert_eq!(store.declared_source_paint("s"), Some(&second));
        let order: Vec<String> = store.source_names().map(|n| n.to_string()).collect();
        assert_eq!(order.last().map(String::as_str), Some("s"));
    }

    const ONE_ROUTE: &str = r#"{"type": "FeatureCollection", "features": [
        {"type": "Feature", "geometry": {"type": "LineString", "coordinates": [[103.8, 1.3], [103.9, 1.3]]}, "properties": {}}
    ]}"#;

    fn chevrons() -> serde_json::Map<String, serde_json::Value> {
        declaration(r#"{"symbol-placement": "line", "symbol-spacing": 200, "icon-image": "chevron"}"#)
    }

    #[test]
    fn line_symbols_are_sticky_and_reach_the_snapshot_only_at_the_next_push() {
        let mut store = OverlayStore::new();
        // A declaration before the first push creates the source...
        store.declare_source_symbols("routes", Some(&chevrons())).unwrap();
        assert_eq!(store.declared_source_symbols("routes"), Some(&chevrons()));
        assert!(store.source_snapshot("routes").unwrap().line_symbols.is_none(), "nothing pushed yet");
        // ...and the push carries it.
        store.set_source("routes", ONE_ROUTE, 12).unwrap();
        let pushed = store.source_snapshot("routes").unwrap().line_symbols.expect("the push carries the declaration");
        assert_eq!(pushed.icon_image, "chevron");
        // STICKY: the next push keeps it without being told again.
        store.set_source("routes", ONE_ROUTE, 12).unwrap();
        assert!(store.source_snapshot("routes").unwrap().line_symbols.is_some());
        // A change governs the NEXT push, never the lines already drawn.
        store
            .declare_source_symbols(
                "routes",
                Some(&declaration(r#"{"symbol-placement": "line", "icon-image": "arrow"}"#)),
            )
            .unwrap();
        assert_eq!(store.source_snapshot("routes").unwrap().line_symbols.unwrap().icon_image, "chevron");
        store.set_source("routes", ONE_ROUTE, 12).unwrap();
        assert_eq!(store.source_snapshot("routes").unwrap().line_symbols.unwrap().icon_image, "arrow");
        // `None` and an empty object both remove it, at the next push.
        store.declare_source_symbols("routes", None).unwrap();
        assert!(store.declared_source_symbols("routes").is_none());
        assert!(store.source_snapshot("routes").unwrap().line_symbols.is_some(), "still the pushed one");
        store.set_source("routes", ONE_ROUTE, 12).unwrap();
        assert!(store.source_snapshot("routes").unwrap().line_symbols.is_none());
        store.declare_source_symbols("routes", Some(&chevrons())).unwrap();
        store.declare_source_symbols("routes", Some(&serde_json::Map::new())).unwrap();
        assert!(store.declared_source_symbols("routes").is_none());
        // Removing from a source nobody mentioned creates nothing.
        store.declare_source_symbols("nobody", None).unwrap();
        assert!(store.source_snapshot("nobody").is_none());
    }

    #[test]
    fn a_refused_line_symbol_declaration_changes_nothing() {
        let mut store = OverlayStore::new();
        store.declare_source_symbols("routes", Some(&chevrons())).unwrap();
        for bad in [
            r#"{"symbol-placement": "line", "icon-image": "chevron", "text-field": "x"}"#,
            r#"{"symbol-placement": "point", "icon-image": "chevron"}"#,
            r#"{"symbol-placement": "line", "icon-image": ["get", "icon"]}"#,
        ] {
            let error = store.declare_source_symbols("routes", Some(&declaration(bad))).unwrap_err();
            assert!(error.to_string().contains("line symbols:"), "{error}");
            assert_eq!(store.declared_source_symbols("routes"), Some(&chevrons()), "{bad} must change nothing");
        }
        assert!(store.declare_source_symbols("fresh", Some(&declaration(r#"{"icon-image": "a"}"#))).is_err());
        assert!(store.source_snapshot("fresh").is_none(), "a refused declaration creates no source");
    }

    fn route_properties(route: usize) -> Arc<FeatureProperties> {
        Arc::new(
            [("route_colour".to_string(), TagValue::String(PALETTE[route].to_string()))]
                .into_iter()
                .collect(),
        )
    }

    /// One dot per vehicle, each carrying its route colour as a property --
    /// the consumer's shape, shrunk to a size a unit test can assert on.
    fn fleet(count: usize) -> (Vec<Vec<[f64; 2]>>, Vec<Arc<FeatureProperties>>) {
        let points = (0..count)
            .map(|index| vec![[103.80 + index as f64 * 1e-3, 1.30 + index as f64 * 1e-3]])
            .collect();
        let properties = (0..count).map(|index| route_properties(index % ROUTES)).collect();
        (points, properties)
    }

    fn dot_inputs<'a>(
        points: &'a [Vec<[f64; 2]>],
        properties: &'a [Arc<FeatureProperties>],
    ) -> Vec<DataFeatureInput<'a>> {
        points
            .iter()
            .enumerate()
            .map(|(index, point)| DataFeatureInput {
                feature: FeatureInput {
                    id: Some(FeatureId::String(format!("bus-{index}"))),
                    geometry: GeometryInput::Point(point.as_slice()),
                    paint: FeaturePaint { circle_radius: 6.0, ..FeaturePaint::default() },
                },
                properties: Arc::clone(&properties[index]),
            })
            .collect()
    }

    /// THE headline case, at store level: 120 vehicles in twelve route
    /// colours, one op each without a declaration and twelve with one --
    /// while staying 120 features with 120 ids.
    #[test]
    fn a_fleet_in_twelve_colours_collapses_to_twelve_ops_and_keeps_every_feature() {
        let (points, properties) = fleet(120);
        let inputs = dot_inputs(&points, &properties);

        // Without a declaration: one op per vehicle, and the properties are
        // carried but read by nothing.
        let mut plain = OverlayStore::new();
        plain.set_source_data_from("vehicles", &inputs, 12).unwrap();
        assert_eq!(plain.source_op_count("vehicles"), Some(120));
        assert_eq!(plain.source_is_data_driven("vehicles"), Some(false));

        // With one: twelve ops, 120 features, 120 ids.
        let mut store = OverlayStore::new();
        store
            .declare_source_paint("vehicles", &declaration(r#"{"circle-color":["get","route_colour"]}"#))
            .unwrap();
        store.set_source_data_from("vehicles", &inputs, 12).unwrap();
        assert_eq!(store.source_op_count("vehicles"), Some(ROUTES));
        assert_eq!(store.source_is_data_driven("vehicles"), Some(true));
        assert_eq!(store.source_features("vehicles").map(|f| f.len()), Some(120));
        for index in 0..120usize {
            assert_eq!(
                store.source_index_of("vehicles", &FeatureId::String(format!("bus-{index}"))),
                Some(index),
                "every vehicle is still addressable by its own id",
            );
        }
    }

    /// Requirement 3: one mesh now covers many logical objects, and a hit
    /// still resolves to the right one.
    ///
    /// The hit index is built from FEATURES and never from ops, so this is
    /// true by construction -- asserted anyway, per vehicle, because "by
    /// construction" is a claim about code that can be refactored, and the
    /// failure it would produce is a click on bus 7 opening bus 0's panel.
    #[test]
    fn a_hit_on_a_merged_source_resolves_to_the_individual_feature() {
        let (points, properties) = fleet(120);
        let inputs = dot_inputs(&points, &properties);
        let mut store = OverlayStore::new();
        store
            .declare_source_paint("vehicles", &declaration(r#"{"circle-color":["get","route_colour"]}"#))
            .unwrap();
        store.set_source_data_from("vehicles", &inputs, 12).unwrap();
        assert_eq!(store.source_op_count("vehicles"), Some(ROUTES), "the ops really are merged");

        let snapshot = store.source_snapshot("vehicles").unwrap();
        for (index, point) in points.iter().enumerate() {
            let world = geom::lon_lat_to_world(point[0][0], point[0][1]);
            let hit = snapshot
                .feature_at(world, 1e-9, 1e-9)
                .unwrap_or_else(|| panic!("vehicle {index} must be hit-testable"));
            assert_eq!(hit.index, index, "a hit must name the vehicle under the cursor");
            assert_eq!(hit.id, Some(FeatureId::String(format!("bus-{index}"))));
            assert_eq!(&*hit.source, "vehicles");
        }
    }

    /// B9: a hit still hands the caller the feature's own properties --
    /// the same keys and values it pushed -- now read back from the
    /// source's columns rather than from a map the feature held.
    #[test]
    fn a_hit_returns_the_properties_the_feature_was_pushed_with() {
        let (points, properties) = fleet(60);
        // Per-feature extras beside the shared route colour, including a
        // key only some features have and a key of mixed type.
        let properties: Vec<Arc<FeatureProperties>> = properties
            .iter()
            .enumerate()
            .map(|(index, shared)| {
                let mut map = (**shared).clone();
                map.insert("seq".into(), TagValue::Number(index as f64));
                if index % 3 == 0 {
                    map.insert("late".into(), TagValue::Bool(true));
                }
                map.insert(
                    "mixed".into(),
                    if index % 2 == 0 { TagValue::String(format!("m{index}")) } else { TagValue::Number(0.5) },
                );
                Arc::new(map)
            })
            .collect();
        let inputs = dot_inputs(&points, &properties);
        let mut store = OverlayStore::new();
        store
            .declare_source_paint("vehicles", &declaration(r#"{"circle-color":["get","route_colour"]}"#))
            .unwrap();
        store.set_source_data_from("vehicles", &inputs, 12).unwrap();
        let snapshot = store.source_snapshot("vehicles").unwrap();
        for (index, point) in points.iter().enumerate() {
            let world = geom::lon_lat_to_world(point[0][0], point[0][1]);
            let hit = snapshot.feature_at(world, 1e-9, 1e-9).unwrap();
            assert_eq!(hit.index, index);
            let got = store.feature_properties(&hit.source, hit.index).unwrap();
            assert_eq!(*got, *properties[index], "feature {index}");
            assert_eq!(snapshot.feature_properties(hit.index).unwrap(), got);
        }
        assert!(store.feature_properties("vehicles", points.len()).is_none());
        assert!(store.feature_properties("nope", 0).is_none());
    }

    /// The typed-column door builds the same store as the per-feature-map
    /// door: the same ops, the same colours, the same properties back.
    #[test]
    fn the_columns_door_agrees_with_the_map_door() {
        let (points, properties) = fleet(90);
        let inputs = dot_inputs(&points, &properties);
        let colours: Vec<Option<&str>> = properties
            .iter()
            .map(|p| match p.get("route_colour") {
                Some(TagValue::String(text)) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        let decl = declaration(r#"{"circle-color":["get","route_colour"]}"#);
        let mut by_map = OverlayStore::new();
        by_map.declare_source_paint("v", &decl).unwrap();
        by_map.set_source_data_from("v", &inputs, 12).unwrap();
        let mut by_column = OverlayStore::new();
        by_column.declare_source_paint("v", &decl).unwrap();
        let features: Vec<FeatureInput<'_>> = inputs.iter().map(|i| i.feature.clone()).collect();
        by_column
            .set_source_columns("v", &features, &[("route_colour", PropertyColumn::Str(&colours))], 12)
            .unwrap();
        assert_eq!(by_map.source_op_count("v"), by_column.source_op_count("v"));
        assert_eq!(
            format!("{:?}", by_map.source_plan("v").unwrap().plan.ops),
            format!("{:?}", by_column.source_plan("v").unwrap().plan.ops)
        );
        for index in 0..points.len() {
            assert_eq!(by_map.feature_properties("v", index), by_column.feature_properties("v", index));
        }
        // A column of the wrong length is refused and changes nothing.
        let short = &colours[..3];
        assert!(by_column
            .set_source_columns("v", &features, &[("route_colour", PropertyColumn::Str(short))], 12)
            .is_err());
        assert_eq!(by_column.source_features("v").unwrap().len(), points.len());
    }

    /// Backward compatibility, stated as an equality rather than argued:
    /// the structured door with empty properties and the data door with
    /// empty properties build the same store, field for field -- and so
    /// does the GeoJSON door.
    #[test]
    fn the_three_doors_agree_when_nothing_is_declared() {
        let point: &[[f64; 2]] = &[[103.85, 1.29]];
        let line: &[[f64; 2]] = &[[103.85, 1.29], [103.86, 1.30]];
        let parts = [line];
        let inputs = vec![
            FeatureInput {
                id: Some(FeatureId::String("dot".into())),
                geometry: GeometryInput::Point(point),
                paint: FeaturePaint::from_properties(&declaration(
                    r##"{"circle-radius":5,"circle-color":"#00ff00"}"##,
                ))
                .unwrap(),
            },
            FeatureInput {
                id: Some(FeatureId::String("route".into())),
                geometry: GeometryInput::Line(&parts),
                paint: FeaturePaint::from_properties(&declaration(
                    r##"{"line-color":"#ff0000","line-width":6}"##,
                ))
                .unwrap(),
            },
        ];
        let wrapped: Vec<DataFeatureInput<'_>> = inputs
            .iter()
            .cloned()
            .map(|feature| DataFeatureInput {
                feature,
                properties: source_paint::no_properties(),
            })
            .collect();

        let mut structured = OverlayStore::new();
        structured.set_source_from("s", &inputs, 12).unwrap();
        let mut data = OverlayStore::new();
        data.set_source_data_from("s", &wrapped, 12).unwrap();

        let state = |store: &OverlayStore| {
            let source = store.source("s").unwrap();
            format!("{:#?}\n{:#?}\n{:#?}", source.features, source.plan, source.hit_index)
        };
        assert_eq!(state(&structured), state(&data));
        assert_eq!(structured.source_op_count("s"), Some(2));
        assert_eq!(data.source_op_count("s"), Some(2));
    }

    /// The GeoJSON door reaches the same place: `["get", ...]` reads the
    /// feature's own `properties`, which is where a GeoJSON caller already
    /// keeps its data.
    #[test]
    fn the_geojson_door_feeds_the_same_expression() {
        let json = r##"{"type":"FeatureCollection","features":[
            {"type":"Feature","id":"a","geometry":{"type":"Point","coordinates":[103.85,1.29]},
             "properties":{"circle-radius":5,"route_colour":"#ff0000"}},
            {"type":"Feature","id":"b","geometry":{"type":"Point","coordinates":[103.86,1.30]},
             "properties":{"circle-radius":5,"route_colour":"#ff0000"}},
            {"type":"Feature","id":"c","geometry":{"type":"Point","coordinates":[103.87,1.31]},
             "properties":{"circle-radius":5,"route_colour":"#0000ff"}}]}"##;

        let mut store = OverlayStore::new();
        store
            .declare_source_paint("stops", &declaration(r#"{"circle-color":["get","route_colour"]}"#))
            .unwrap();
        store.set_source("stops", json, 12).unwrap();
        assert_eq!(store.source_op_count("stops"), Some(2), "two colours, two ops");

        let colour = |op: usize| match &store.source_plan("stops").unwrap().plan.ops[op] {
            plan_op @ crate::style::PaintOp::Line { color_from, .. } => {
                let _ = plan_op;
                *color_from
            }
            other => panic!("expected a line op, got {other:?}"),
        };
        assert_eq!(colour(0), crate::style_spec::color::parse_css_color("#ff0000").unwrap());
        assert_eq!(colour(1), crate::style_spec::color::parse_css_color("#0000ff").unwrap());
    }

    /// An UNDECLARED source keeps its properties too: nothing in its paint
    /// reads them, but a pick does (a host's hover card). A plain polygon
    /// source used to keep none, and the delivery page's hexagon card had
    /// no count (2026-10-12).
    #[test]
    fn an_undeclared_source_keeps_its_properties_for_a_pick() {
        let json = r##"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":{"type":"Point","coordinates":[103.85,1.29]},
             "properties":{"circle-radius":5,"route_colour":"#ff0000","vehicle_id":"SBA1234X"}},
            {"type":"Feature","geometry":{"type":"Polygon","coordinates":[[[103.8,1.3],[103.81,1.3],[103.81,1.31],[103.8,1.3]]]},
             "properties":{"fill-color":"#00ff00","parcels":1284}}]}"##;
        let mut store = OverlayStore::new();
        store.set_source("stops", json, 12).unwrap();
        assert_eq!(store.source("stops").unwrap().properties.rows(), 2);
        assert_eq!(store.feature_properties("stops", 0).unwrap().get("vehicle_id"), Some(&TagValue::String("SBA1234X".into())));
        assert_eq!(store.feature_properties("stops", 1).unwrap().get("parcels"), Some(&TagValue::Number(1284.0)));

        // And a DECLARED one does carry them, with the JSON types mapped
        // onto the vocabulary a compiled filter reads.
        let mut declared = OverlayStore::new();
        declared
            .declare_source_paint("stops", &declaration(r#"{"circle-color":["get","route_colour"]}"#))
            .unwrap();
        declared.set_source("stops", json, 12).unwrap();
        let properties = declared.feature_properties("stops", 0).unwrap();
        assert_eq!(
            properties.get("route_colour"),
            Some(&TagValue::String("#ff0000".into())),
        );
        assert_eq!(
            properties.get("circle-radius"),
            Some(&TagValue::Number(5.0)),
            "a paint key is still the caller's own property as far as this map is concerned",
        );
    }

    /// A declaration is STICKY and governs the next push, exactly as
    /// `declare_source` does -- and it creates the source if it is the
    /// first mention of the name.
    #[test]
    fn a_declaration_is_sticky_across_pushes_and_creates_the_source() {
        let (points, properties) = fleet(24);
        let inputs = dot_inputs(&points, &properties);
        let mut store = OverlayStore::new();
        store
            .declare_source_paint("vehicles", &declaration(r#"{"circle-color":["get","route_colour"]}"#))
            .unwrap();
        assert_eq!(store.source_is_data_driven("vehicles"), Some(true), "the source now exists");
        assert_eq!(store.generation(), 0, "a declaration is not a generation");

        store.set_source_data_from("vehicles", &inputs, 12).unwrap();
        assert_eq!(store.source_op_count("vehicles"), Some(ROUTES));
        store.set_source_data_from("vehicles", &inputs, 12).unwrap();
        assert_eq!(store.source_op_count("vehicles"), Some(ROUTES), "still declared");
    }

    /// A malformed declaration is refused and changes nothing -- the same
    /// all-or-nothing rule every other door keeps.
    #[test]
    fn a_refused_declaration_leaves_the_source_exactly_as_it_was() {
        let (points, properties) = fleet(24);
        let inputs = dot_inputs(&points, &properties);
        let mut store = OverlayStore::new();
        store.set_source_data_from("vehicles", &inputs, 12).unwrap();
        assert_eq!(store.source_op_count("vehicles"), Some(24));

        let error = store
            .declare_source_paint("vehicles", &declaration(r#"{"line-width":4}"#))
            .unwrap_err();
        match &error {
            FeatureError::SourcePaint { source, reason } => {
                assert_eq!(source, "vehicles");
                assert!(reason.contains("line-width"), "reason was {reason}");
            }
            other => panic!("expected a source-paint error, got {other:?}"),
        }
        assert!(error.to_string().contains("vehicles"), "error was {error}");
        assert_eq!(store.source_is_data_driven("vehicles"), Some(false));
        assert_eq!(store.source_op_count("vehicles"), Some(24), "nothing was rebuilt");
    }

    /// Requirement 2, at store level: a data-driven colour composes with a
    /// checked `zoom_independent` declaration rather than invalidating it.
    ///
    /// The two declarations are made in both orders, because a promise that
    /// only survives one of them is a promise a caller has to remember the
    /// ordering of.
    #[test]
    fn a_data_driven_colour_keeps_a_zoom_independent_declaration() {
        let (points, properties) = fleet(24);
        let inputs = dot_inputs(&points, &properties);
        let colour = declaration(r#"{"circle-color":["get","route_colour"]}"#);

        for paint_first in [true, false] {
            let mut store = OverlayStore::new();
            if paint_first {
                store.declare_source_paint("vehicles", &colour).unwrap();
                store.declare_source("vehicles", SourceOptions { zoom_independent: true, ..Default::default() }).unwrap();
            } else {
                store.declare_source("vehicles", SourceOptions { zoom_independent: true, ..Default::default() }).unwrap();
                store.declare_source_paint("vehicles", &colour).unwrap();
            }
            store.set_source_data_from("vehicles", &inputs, 12).unwrap();
            assert_eq!(store.source_is_zoom_independent("vehicles"), Some(true));
            assert_eq!(store.source_op_count("vehicles"), Some(ROUTES));
            assert!(matches!(
                store.source_plan("vehicles").unwrap().bracket,
                plan::BakeBracket::Ramp { .. }
            ));
        }
    }

    /// A zoom-varying colour is refused at the declaration, which is what
    /// makes the claim above hold by construction rather than by luck.
    #[test]
    fn a_zoom_varying_source_colour_is_refused_at_the_declaration() {
        let mut store = OverlayStore::new();
        let error = store
            .declare_source_paint(
                "vehicles",
                &declaration(
                    r##"{"circle-color":["interpolate",["linear"],["zoom"],8,"#ff0000",18,"#0000ff"]}"##,
                ),
            )
            .unwrap_err();
        assert!(error.to_string().contains("ZOOM"), "error was {error}");
    }

    /// A paint-only update to a data-driven source REBAKES rather than
    /// patching vertices in place: its ops are shared, so there is no
    /// longer one feature whose colour an in-place rewrite could write.
    #[test]
    fn a_paint_update_on_a_merged_source_needs_a_rebake() {
        let (points, properties) = fleet(24);
        let inputs = dot_inputs(&points, &properties);

        // Without a declaration the same update is patched in place, which
        // is the contrast that makes the assertion mean something.
        let mut plain = OverlayStore::new();
        plain.set_source_data_from("vehicles", &inputs, 12).unwrap();
        let update = [(
            FeatureId::String("bus-3".into()),
            FeaturePaint { circle_radius: 7.0, ..FeaturePaint::default() },
        )];
        assert!(!plain.plan_source_paint_update("vehicles", &update).unwrap().needs_rebake());

        let mut store = OverlayStore::new();
        store
            .declare_source_paint("vehicles", &declaration(r#"{"circle-color":["get","route_colour"]}"#))
            .unwrap();
        store.set_source_data_from("vehicles", &inputs, 12).unwrap();
        assert!(store.plan_source_paint_update("vehicles", &update).unwrap().needs_rebake());
    }

    /// A zoom-bucket crossing re-resolves a data-driven source through the
    /// SAME expressions the drawn mesh was built with, and lands on the
    /// same op set -- which is what `rewrite_paint_attributes`'s 1:1
    /// `ranges`/`ops` assertion requires.
    #[test]
    fn a_snapshot_replans_a_merged_source_at_a_new_bucket_with_the_same_ops() {
        let (points, properties) = fleet(120);
        let inputs = dot_inputs(&points, &properties);
        let mut store = OverlayStore::new();
        store
            .declare_source_paint("vehicles", &declaration(r#"{"circle-color":["get","route_colour"]}"#))
            .unwrap();
        store.set_source_data_from("vehicles", &inputs, 12).unwrap();

        let snapshot = store.source_snapshot("vehicles").unwrap();
        let replanned = snapshot.plan_at(plan::BakeBracket::Bucket(16)).unwrap();
        assert!(replanned.data_driven, "the snapshot carries the expressions");
        assert_eq!(replanned.plan.ops.len(), ROUTES);
        assert_eq!(replanned.feature_ops, store.source_plan("vehicles").unwrap().feature_ops);
    }

    /// Pushing a data-driven source touches no other source -- the
    /// property per-source baking exists for, restated because merging
    /// happens inside one source's plan and must stay there.
    #[test]
    fn pushing_a_data_driven_source_leaves_its_neighbour_alone() {
        let (points, properties) = fleet(24);
        let inputs = dot_inputs(&points, &properties);
        let mut store = OverlayStore::new();
        store.set_source("stops", super::tests::STATIC_SOURCE, 12).unwrap();
        let stops_stamp = store.source_generation("stops");
        store
            .declare_source_paint("vehicles", &declaration(r#"{"circle-color":["get","route_colour"]}"#))
            .unwrap();
        store.set_source_data_from("vehicles", &inputs, 12).unwrap();
        assert_eq!(store.source_generation("stops"), stops_stamp);
        assert_eq!(store.source_is_data_driven("stops"), Some(false));
        assert_eq!(store.source_op_count("stops"), Some(2));
    }

    /// The guards `set_source_from` runs are the guards
    /// `set_source_data_from` runs, in the same position in the loop -- the
    /// two doors share `read_feature` rather than mirroring it.
    #[test]
    fn the_data_door_refuses_everything_the_structured_door_refuses() {
        let bad: &[[f64; 2]] = &[[f64::INFINITY, 1.29]];
        let good: &[[f64; 2]] = &[[103.85, 1.29]];
        let mut store = OverlayStore::new();

        let error = store
            .set_source_data_from(
                "v",
                &[DataFeatureInput {
                    feature: FeatureInput {
                        id: Some(FeatureId::String("off-the-map".into())),
                        geometry: GeometryInput::Point(bad),
                        paint: FeaturePaint::default(),
                    },
                    properties: source_paint::no_properties(),
                }],
                12,
            )
            .unwrap_err();
        assert!(error.to_string().contains("finite"), "error was {error}");

        let paint = FeaturePaint { circle_radius: -5.0, ..FeaturePaint::default() };
        let error = store
            .set_source_data_from(
                "v",
                &[DataFeatureInput {
                    feature: FeatureInput { id: None, geometry: GeometryInput::Point(good), paint },
                    properties: source_paint::no_properties(),
                }],
                12,
            )
            .unwrap_err();
        assert!(error.to_string().contains("circle-radius"), "error was {error}");

        let duplicate = |id: &str| DataFeatureInput {
            feature: FeatureInput {
                id: Some(FeatureId::String(id.into())),
                geometry: GeometryInput::Point(good),
                paint: FeaturePaint::default(),
            },
            properties: source_paint::no_properties(),
        };
        let error =
            store.set_source_data_from("v", &[duplicate("same"), duplicate("same")], 12).unwrap_err();
        assert!(error.to_string().contains("duplicate id"), "error was {error}");
        assert!(store.is_empty(), "every rejected call changed nothing");
    }
}

/// Phase B / Task 7: a tiled source's index is its one geometry copy, and
/// hit-testing answers the ORIGINAL feature from it.
#[cfg(test)]
mod tiled_hit_tests {
    use super::*;
    use crate::overlay::geom::lon_lat_to_world;

    fn ring_json(ring: &[(f64, f64)]) -> String {
        format!("[{}]", ring.iter().map(|(x, y)| format!("[{x},{y}]")).collect::<Vec<_>>().join(","))
    }

    fn rect(w: f64, s: f64, e: f64, n: f64) -> Vec<(f64, f64)> {
        vec![(w, s), (e, s), (e, n), (w, n), (w, s)]
    }

    fn collection(features: &[String]) -> String {
        format!(r#"{{"type":"FeatureCollection","features":[{}]}}"#, features.join(","))
    }

    /// One polygon over lon -100..-80, lat 35..45: it crosses the z4 tile
    /// edges at lon -90 and lat ~41.0, so four z4 tiles.
    fn crossing_polygon_json(id: &str, name: &str) -> String {
        collection(&[format!(
            r#"{{"type":"Feature","id":"{id}","properties":{{"name":"{name}"}},"geometry":{{"type":"Polygon","coordinates":[{}]}}}}"#,
            ring_json(&rect(-100.0, 35.0, -80.0, 45.0))
        )])
    }

    fn probe_points_in_each_quadrant() -> Vec<(f64, f64)> {
        [(-95.0, 38.0), (-85.0, 38.0), (-95.0, 43.0), (-85.0, 43.0)]
            .iter()
            .map(|&(x, y)| lon_lat_to_world(x, y))
            .collect()
    }

    fn polygon_with_hole_json() -> String {
        collection(&[format!(
            r#"{{"type":"Feature","id":1,"properties":{{}},"geometry":{{"type":"Polygon","coordinates":[{},{}]}}}}"#,
            ring_json(&rect(-100.0, 30.0, -80.0, 45.0)),
            ring_json(&rect(-92.0, 36.0, -88.0, 40.0))
        )])
    }

    fn hole_centre_world() -> (f64, f64) {
        lon_lat_to_world(-90.0, 38.0)
    }

    /// A `sqrt(n)` x `sqrt(n)` grid of 0.8-degree squares, 1 degree apart,
    /// ids 0..n.
    fn many_small_polygons_json(n: usize) -> String {
        let side = (n as f64).sqrt() as usize;
        let features: Vec<String> = (0..n)
            .map(|i| {
                let (w, s) = (-100.0 + (i % side) as f64, 30.0 + (i / side) as f64);
                format!(
                    r#"{{"type":"Feature","id":{i},"properties":{{}},"geometry":{{"type":"Polygon","coordinates":[{}]}}}}"#,
                    ring_json(&rect(w, s, w + 0.8, s + 0.8))
                )
            })
            .collect();
        collection(&features)
    }

    /// `n` x `n` probes over lon -101..-79, lat 29..51, offset so none
    /// lands exactly on a square's edge.
    fn probe_grid(n: usize) -> Vec<(f64, f64)> {
        let mut out = Vec::new();
        for i in 0..n {
            for j in 0..n {
                let lon = -101.0137 + 22.0 * i as f64 / (n - 1) as f64;
                let lat = 29.0071 + 22.0 * j as f64 / (n - 1) as f64;
                out.push(lon_lat_to_world(lon, lat));
            }
        }
        out
    }

    /// What `MapView` builds when a tiled source lands: `classify` plus
    /// `TileIndex::build` on the snapshot. The source is marked tiled first:
    /// these are far under `TILED_SOURCE_MIN_POINTS`.
    fn build_index_for_test(store: &mut OverlayStore, name: &str) -> Arc<tile_index::TileIndex> {
        store.source_mut(name).unwrap().tiled = true;
        let snap = store.source_snapshot(name).unwrap();
        let classes = plan::classify(&snap.features, &snap.properties, plan::BakeBracket::Bucket(4), &snap.paint).unwrap();
        Arc::new(
            tile_index::TileIndex::build(&snap.features, &classes.of_feature, Default::default(), &|| false).unwrap(),
        )
    }

    fn adopt(store: &mut OverlayStore, name: &str) {
        let index = build_index_for_test(store, name);
        store.adopt_tile_index(name, index);
    }

    /// Accept, apply and install one diff, as `MapView` does across its
    /// worker: the classes are the ones the tile set would hold.
    fn live_update(store: &mut OverlayStore, name: &str, classes: &mut Arc<plan::FeatureClasses>, diff: SourceDiff<'_>) -> LiveUpdate {
        let generation = store.source_generation(name).unwrap();
        let (update, resolved) = store.accept_source_diff(name, &diff, 4).unwrap();
        let resolved = resolved.expect("incremental");
        let base = store.live_base(name, Arc::clone(classes), plan::BakeBracket::Bucket(4)).unwrap();
        let result = live::apply(&base, &[resolved]).unwrap();
        assert!(store.install_live(name, generation, &result).is_some());
        assert_eq!(store.source_generation(name), Some(generation), "the incremental path keeps the generation");
        *classes = Arc::clone(&result.classes);
        update
    }

    fn square(w: f64, s: f64, e: f64, n: f64) -> Vec<[f64; 2]> {
        vec![[w, s], [e, s], [e, n], [w, n], [w, s]]
    }

    #[test]
    fn hits_follow_live_updates_adds_and_removes() {
        let mut store = OverlayStore::new();
        store.set_source("s", &many_small_polygons_json(100), 4).unwrap();
        store.source_mut("s").unwrap().tiled = true;
        store.source_mut("s").unwrap().plan = None; // as `source_plan_for` leaves a tiled source with no points
        let snap = store.source_snapshot("s").unwrap();
        let classes = plan::classify(&snap.features, &snap.properties, plan::BakeBracket::Bucket(4), &snap.paint).unwrap();
        let index = tile_index::TileIndex::build(&snap.features, &classes.of_feature, Default::default(), &|| false).unwrap();
        store.adopt_tile_index("s", Arc::new(index));
        let mut classes = Arc::new(classes);
        let hit = |store: &OverlayStore, lon: f64, lat: f64| {
            store.source_snapshot("s").unwrap().feature_at(lon_lat_to_world(lon, lat), 0.0, 1.0)
        };
        // Square 0 is (-100, 30)..(-99.2, 30.8).
        assert_eq!(hit(&store, -99.6, 30.4).unwrap().index, 0);
        // Move feature 0 north-east of the grid; add feature "new" where it was.
        let moved = square(-70.0, 60.0, -69.0, 61.0);
        let moved_ring: [&[[f64; 2]]; 1] = [&moved];
        let moved_poly: [&[&[[f64; 2]]]; 1] = [&moved_ring];
        let added = square(-100.0, 30.0, -99.2, 30.8);
        let added_ring: [&[[f64; 2]]; 1] = [&added];
        let added_poly: [&[&[[f64; 2]]]; 1] = [&added_ring];
        let update = live_update(
            &mut store,
            "s",
            &mut classes,
            SourceDiff {
                remove_ids: vec![FeatureId::Number(5.0), FeatureId::Number(12345.0)],
                add: vec![AddFeature {
                    feature: FeatureInput {
                        id: Some(FeatureId::String("new".into())),
                        geometry: GeometryInput::Polygon(&added_poly),
                        paint: FeaturePaint::default(),
                    },
                    properties: vec![("income".into(), TagValue::Number(5.0))],
                }],
                update: vec![FeatureUpdate {
                    id: FeatureId::Number(0.0),
                    geometry: Some(GeometryInput::Polygon(&moved_poly)),
                    properties: vec![("income".into(), Some(TagValue::Number(7.0)))],
                }],
            },
        );
        assert_eq!((update.path, update.skipped), (LivePath::Incremental, 1));
        // Updated: hits at its new place, as the same row and id.
        let at_new = hit(&store, -69.5, 60.5).expect("the moved feature");
        assert_eq!((at_new.index, at_new.id.clone()), (0, Some(FeatureId::Number(0.0))));
        // Added: a new row at the end, where feature 0 used to be.
        let at_old = hit(&store, -99.6, 30.4).expect("the added feature");
        assert_eq!((at_old.index, at_old.id.clone()), (100, Some(FeatureId::String("new".into()))));
        assert_eq!(store.feature_properties("s", 100).unwrap().get("income"), Some(&TagValue::Number(5.0)));
        assert_eq!(store.feature_properties("s", 0).unwrap().get("income"), Some(&TagValue::Number(7.0)));
        // Removed: feature 5's square misses, and its row reads as gone.
        let side = 10;
        let (w, s) = (-100.0 + (5 % side) as f64, 30.0 + (5 / side) as f64);
        assert!(hit(&store, w + 0.4, s + 0.4).is_none(), "a removed feature is never hit");
        assert!(store.feature_properties("s", 5).is_none());
        // Every other row is untouched.
        assert_eq!(hit(&store, -98.6, 30.4).unwrap().index, 1);
        assert_eq!(store.source_resolve_id("s", &FeatureId::String("new".into())).unwrap(), 100);
        assert!(store.source_resolve_id("s", &FeatureId::Number(5.0)).is_err());
        // An add that reuses an id replaces the feature.
        let update = live_update(
            &mut store,
            "s",
            &mut classes,
            SourceDiff {
                add: vec![AddFeature {
                    feature: FeatureInput {
                        id: Some(FeatureId::String("new".into())),
                        geometry: GeometryInput::Polygon(&moved_poly),
                        paint: FeaturePaint::default(),
                    },
                    properties: Vec::new(),
                }],
                ..SourceDiff::default()
            },
        );
        assert_eq!(update.skipped, 0);
        assert!(hit(&store, -99.6, 30.4).is_none(), "the replaced feature's old geometry is gone");
        assert_eq!(store.source_resolve_id("s", &FeatureId::String("new".into())).unwrap(), 101);
        // A diff without ids on its adds is refused and changes nothing.
        let before = store.source_features("s").unwrap().len();
        let refused = store.accept_source_diff(
            "s",
            &SourceDiff {
                add: vec![AddFeature {
                    feature: FeatureInput { id: None, geometry: GeometryInput::Polygon(&added_poly), paint: FeaturePaint::default() },
                    properties: Vec::new(),
                }],
                ..SourceDiff::default()
            },
            4,
        );
        assert!(refused.is_err());
        assert_eq!(store.source_features("s").unwrap().len(), before);
    }

    /// R7 review: a declared `line-cap` / `line-join` is refused on a
    /// source that tiles, rather than silently stroked Butt / Bevel by the
    /// tiled plan; the same push as a whole source keeps it.
    #[test]
    fn a_tiled_push_with_a_declared_line_cap_is_refused() {
        let n = tile_plan::TILED_SOURCE_MIN_POINTS + 10;
        let coords: Vec<[f64; 2]> = (0..n).map(|i| [i as f64 * 1e-4 - 10.0, (i % 7) as f64 * 1e-3]).collect();
        let doc = serde_json::json!({"type": "FeatureCollection", "features": [{"type": "Feature", "properties": {},
            "geometry": {"type": "LineString", "coordinates": coords}}]})
        .to_string();
        let cap: serde_json::Map<String, serde_json::Value> = serde_json::from_str(r#"{"line-cap": "round"}"#).unwrap();
        let mut store = OverlayStore::new();
        store.declare_source_paint("s", &cap).unwrap();
        let refused = store.set_source("s", &doc, 4);
        match refused {
            Err(FeatureError::SourcePaint { reason, .. }) => assert!(reason.contains("line-cap"), "{reason}"),
            other => panic!("a tiled push with a cap must be refused, got {other:?}"),
        }
        assert!(store.source_features("s").is_none_or(|f| f.is_empty()), "nothing was installed");

        let mut whole = OverlayStore::new();
        whole.declare_source("s", SourceOptions { whole_source: true, ..SourceOptions::default() }).unwrap();
        whole.declare_source_paint("s", &cap).unwrap();
        whole.set_source("s", &doc, 4).expect("a whole source keeps its cap");

        // Declared after a tiled push: refused too.
        let mut late = OverlayStore::new();
        late.set_source("s", &doc, 4).unwrap();
        assert!(late.source("s").unwrap().tiled, "the size alone tiles it");
        assert!(late.declare_source_paint("s", &cap).is_err());
    }

    #[test]
    fn an_untiled_source_takes_the_push_path() {
        let mut store = OverlayStore::new();
        store.set_source("s", &many_small_polygons_json(16), 4).unwrap();
        let generation = store.source_generation("s").unwrap();
        let (update, resolved) = store
            .accept_source_diff("s", &SourceDiff { remove_ids: vec![FeatureId::Number(3.0)], ..SourceDiff::default() }, 4)
            .unwrap();
        assert!(resolved.is_none());
        assert_eq!(update.path, LivePath::Push);
        assert!(store.source_generation("s").unwrap() > generation);
        assert_eq!(store.source_features("s").unwrap().len(), 15);
        assert_eq!(store.source_resolve_id("s", &FeatureId::Number(4.0)).unwrap(), 3, "compacted on the push path");
    }

    #[test]
    fn a_tiled_source_hit_returns_the_original_feature_wherever_it_is_clicked() {
        let mut store = OverlayStore::new();
        store.set_source("s", &crossing_polygon_json("bg-1", "one"), 4).unwrap();
        let generation = store.source_generation("s");
        adopt(&mut store, "s");
        assert_eq!(store.source_generation("s"), generation, "adoption draws nothing new");
        let snap = store.source_snapshot("s").unwrap();
        assert!(snap.tile_index.is_some());
        for world in probe_points_in_each_quadrant() {
            let hit = snap.feature_at(world, 0.0, 1.0).expect("inside");
            assert_eq!(hit.id, Some(FeatureId::String("bg-1".into())));
            assert_eq!(hit.index, 0);
            assert_eq!(&*hit.source, "s");
        }
        assert_eq!(snap.features[0].id, Some(FeatureId::String("bg-1".into())));
    }

    #[test]
    fn a_tiled_features_properties_survive_adoption() {
        let mut store = OverlayStore::new();
        let paint = serde_json::json!({"fill-color": ["match", ["get", "name"], "one", "#ff0000", "#0000ff"]});
        store.declare_source_paint("s", paint.as_object().unwrap()).unwrap();
        store.set_source("s", &crossing_polygon_json("bg-1", "one"), 4).unwrap();
        let before = store.source_snapshot("s").unwrap().properties;
        adopt(&mut store, "s");
        let snap = store.source_snapshot("s").unwrap();
        assert!(Arc::ptr_eq(&before, &snap.properties), "the same property table, not a copy");
        assert_eq!(
            snap.feature_properties(0).unwrap().get("name"),
            Some(&crate::mvt::TagValue::String("one".into()))
        );
    }

    #[test]
    fn a_tiled_source_misses_inside_a_hole_and_outside() {
        let mut store = OverlayStore::new();
        store.set_source("s", &polygon_with_hole_json(), 4).unwrap();
        adopt(&mut store, "s");
        let snap = store.source_snapshot("s").unwrap();
        assert!(snap.feature_at(lon_lat_to_world(-96.0, 33.0), 0.0, 1.0).is_some(), "in the ring");
        assert!(snap.feature_at(hole_centre_world(), 0.0, 1.0).is_none(), "in the hole");
        assert!(snap.feature_at((0.5, 0.5), 0.0, 1.0).is_none(), "outside");
    }

    #[test]
    fn a_tiled_boundary_is_sharp_to_a_fraction_of_a_z20_pixel() {
        // 4e-7 world units is ~3.4 fixed-point units and ~0.4 px at z20.
        let eps = 4e-7;
        let mut store = OverlayStore::new();
        store.set_source("s", &polygon_with_hole_json(), 4).unwrap();
        adopt(&mut store, "s");
        let snap = store.source_snapshot("s").unwrap();
        let (east, y) = lon_lat_to_world(-80.0, 38.0);
        assert!(snap.feature_at((east - eps, y), 0.0, 1.0).is_some(), "just inside the east edge");
        assert!(snap.feature_at((east + eps, y), 0.0, 1.0).is_none(), "just outside it");
        let (hole_west, y) = lon_lat_to_world(-92.0, 38.0);
        assert!(snap.feature_at((hole_west - eps, y), 0.0, 1.0).is_some(), "just outside the hole");
        assert!(snap.feature_at((hole_west + eps, y), 0.0, 1.0).is_none(), "just inside the hole");
    }

    #[test]
    fn a_tiled_line_hits_within_tolerance_and_misses_beyond_it() {
        let json = collection(&[format!(
            r#"{{"type":"Feature","id":"l","properties":{{}},"geometry":{{"type":"LineString","coordinates":{}}}}}"#,
            ring_json(&[(-100.0, 38.0), (-80.0, 38.0), (-80.0, 44.0)])
        )]);
        let mut store = OverlayStore::new();
        store.set_source("s", &json, 4).unwrap();
        adopt(&mut store, "s");
        let snap = store.source_snapshot("s").unwrap();
        assert!(matches!(snap.features[0].geometry, OverlayGeometry::Indexed { line: true, .. }));
        let tol = 1e-3;
        let (x, y) = lon_lat_to_world(-90.0, 38.0);
        assert_eq!(
            snap.feature_at((x, y + 0.5 * tol), tol, 1.0).map(|h| h.id),
            Some(Some(FeatureId::String("l".into())))
        );
        assert!(snap.feature_at((x, y + 1.5 * tol), tol, 1.0).is_none());
        let (x, y) = lon_lat_to_world(-80.0, 41.0);
        assert!(snap.feature_at((x - 0.5 * tol, y), tol, 1.0).is_some(), "the second segment");
    }

    #[test]
    fn adopting_the_index_frees_the_lon_lat_geometry() {
        let mut store = OverlayStore::new();
        store.set_source("s", &crossing_polygon_json("bg-1", "one"), 4).unwrap();
        let before = store.source_snapshot("s").unwrap().features;
        adopt(&mut store, "s");
        let snap = store.source_snapshot("s").unwrap();
        assert!(matches!(snap.features[0].geometry, OverlayGeometry::Indexed { line: false, .. }));
        assert!(!Arc::ptr_eq(&before, &snap.features), "a new vector: the old one frees with its last snapshot");
        let OverlayGeometry::Indexed { bbox_lon_lat: (w, s, e, n), .. } = snap.features[0].geometry else {
            panic!()
        };
        for (got, want) in [(w, -100.0), (s, 35.0), (e, -80.0), (n, 45.0)] {
            assert!((got - want).abs() < 1e-6, "{got} vs {want}");
        }
    }

    #[test]
    fn an_untiled_source_is_never_adopted() {
        let mut store = OverlayStore::new();
        store.set_source("s", &crossing_polygon_json("bg-1", "one"), 4).unwrap();
        let index = build_index_for_test(&mut store, "s");
        store.source_mut("s").unwrap().tiled = false;
        store.adopt_tile_index("s", index);
        assert!(matches!(store.source_snapshot("s").unwrap().features[0].geometry, OverlayGeometry::Polygon(_)));
    }

    #[test]
    fn tiled_and_untiled_hits_agree_on_a_grid_of_probes() {
        let json = many_small_polygons_json(400);
        let mut a = OverlayStore::new();
        a.set_source("s", &json, 6).unwrap();
        let mut b = OverlayStore::new();
        b.set_source("s", &json, 6).unwrap();
        adopt(&mut b, "s");
        let (sa, sb) = (a.source_snapshot("s").unwrap(), b.source_snapshot("s").unwrap());
        let mut hits = 0;
        for p in probe_grid(97) {
            let (ra, rb) = (sa.feature_at(p, 1e-4, 1e-4), sb.feature_at(p, 1e-4, 1e-4));
            assert_eq!(ra.as_ref().map(|r| r.index), rb.as_ref().map(|r| r.index), "{p:?}");
            assert_eq!(ra.map(|r| r.id), rb.as_ref().map(|r| r.id.clone()), "{p:?}");
            hits += usize::from(rb.is_some());
        }
        assert!(hits > 1000, "the grid really probes the squares: {hits} hits");
    }

    #[test]
    fn a_push_after_adoption_brings_new_geometry_and_drops_the_index() {
        let mut store = OverlayStore::new();
        store.set_source("s", &crossing_polygon_json("bg-1", "one"), 4).unwrap();
        adopt(&mut store, "s");
        store.set_source("s", &crossing_polygon_json("bg-2", "two"), 4).unwrap();
        let snap = store.source_snapshot("s").unwrap();
        assert!(snap.tile_index.is_none());
        assert!(matches!(snap.features[0].geometry, OverlayGeometry::Polygon(_)));
    }

    #[test]
    fn the_untiled_fallback_after_adoption_gets_its_polygons_back() {
        let mut store = OverlayStore::new();
        store.set_source("s", &polygon_with_hole_json(), 4).unwrap();
        adopt(&mut store, "s");
        assert!(store.force_untiled("s", 4));
        let snap = store.source_snapshot("s").unwrap();
        let OverlayGeometry::Polygon(p) = &snap.features[0].geometry else { panic!("lon/lat restored") };
        assert_eq!(p[0].len(), 2, "outer and hole");
        assert!((p[0][1][0].0 - -92.0).abs() < 1e-6 && (p[0][1][0].1 - 36.0).abs() < 1e-6, "{:?}", p[0][1][0]);
        assert_eq!(store.source_plan("s").unwrap().plan.ops.len(), 1, "the polygon is back in the plan");
        assert!(snap.feature_at(hole_centre_world(), 0.0, 1.0).is_none());
        assert!(snap.feature_at(lon_lat_to_world(-96.0, 33.0), 0.0, 1.0).is_some());
    }

    #[test]
    fn a_re_class_after_adoption_cuts_the_same_geometry() {
        let mut store = OverlayStore::new();
        store.set_source("s", &polygon_with_hole_json(), 4).unwrap();
        let index = build_index_for_test(&mut store, "s");
        let tile = crate::geo::TileId { z: 4, x: 3, y: 6 };
        let cut = index.cut(tile);
        assert!(!cut.features.is_empty());
        store.adopt_tile_index("s", Arc::clone(&index));
        let snap = store.source_snapshot("s").unwrap();
        let classes = plan::classify(&snap.features, &snap.properties, plan::BakeBracket::Bucket(4), &snap.paint).unwrap();
        assert_eq!(classes.of_feature, vec![0], "an Indexed polygon is still classed as a fill");
        assert_eq!(index.reclassified(&classes.of_feature).cut(tile), cut);
        assert!(index.reclassified(&[tile_index::NO_CLASS]).cut(tile).features.is_empty());
    }
}

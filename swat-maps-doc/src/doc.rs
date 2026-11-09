//! The project document (spec §6): one JSON file, pretty-printed with
//! sorted keys. Every struct carries a flattened `extra` map, so a key this
//! version does not know (written by a later Studio, or by hand) survives a
//! load and a save.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::filters::{FilterKind, FilterValue, Placement};

/// The document version this crate writes.
pub const MAPDOC_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum DocError {
    /// Not JSON, or JSON of the wrong shape. The message names the place.
    #[error("This is not a map document: {0}")]
    Shape(String),
    /// A newer document than this Studio reads.
    #[error("This map document is version {0}; this Studio reads version {MAPDOC_VERSION}. Update the Studio to open it.")]
    Version(u32),
}

/// The whole project.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Doc {
    pub mapdoc: u32,
    pub profile: String,
    #[serde(default)]
    pub title: String,
    /// The app version only: never a user or machine name (spec §12).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    #[serde(default)]
    pub sources: BTreeMap<String, Source>,
    #[serde(default)]
    pub map: MapBlock,
    /// P3. Empty in P1.
    #[serde(default)]
    pub filters: Vec<Filter>,
    /// Explorer-only state (field type overrides, panel state).
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub explorer: Value,
    /// Floating chart cards over the map (agent chat spec §5.3.1).
    /// Additive: `mapdoc: 1` stays, an older Studio keeps the key as an
    /// unknown one, and a document without charts writes no key.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub charts: Vec<Chart>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// What a chart card draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChartKind {
    /// A numeric field binned along its range.
    Histogram,
    /// A categorical field's values as bars.
    Bar,
    /// A time field binned along its range.
    TimeSeries,
}

impl ChartKind {
    pub fn name(self) -> &'static str {
        match self {
            ChartKind::Histogram => "histogram",
            ChartKind::Bar => "bar",
            ChartKind::TimeSeries => "time_series",
        }
    }

    pub fn parse(name: &str) -> Option<ChartKind> {
        Some(match name {
            "histogram" => ChartKind::Histogram,
            "bar" => ChartKind::Bar,
            "time_series" => ChartKind::TimeSeries,
            _ => return None,
        })
    }
}

/// What a chart's bar measures: a row count, or a numeric field summed or
/// averaged.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Measure {
    /// `count` | `sum` | `mean`.
    pub op: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
}

/// One floating chart card over one dataset's field.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Chart {
    pub id: String,
    /// The dataset id.
    pub source: String,
    pub kind: ChartKind,
    pub field: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measure: Option<Measure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bins: Option<u32>,
    /// A brush on the chart writes the dataset's range filter on `field`.
    #[serde(default = "yes")]
    pub brush_filters: bool,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Chart {
    pub fn new(
        id: impl Into<String>,
        source: impl Into<String>,
        kind: ChartKind,
        field: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            source: source.into(),
            kind,
            field: field.into(),
            measure: None,
            bins: None,
            brush_filters: true,
            extra: Map::new(),
        }
    }
}

impl Default for Doc {
    fn default() -> Self {
        Self {
            mapdoc: MAPDOC_VERSION,
            profile: "map".into(),
            title: String::new(),
            created_by: None,
            sources: BTreeMap::new(),
            map: MapBlock::default(),
            filters: Vec::new(),
            explorer: Value::Null,
            charts: Vec::new(),
            extra: Map::new(),
        }
    }
}

/// A dataset the document refers to.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Source {
    /// `csv` | `geojson` | `geoparquet` | `fgb`.
    pub kind: String,
    pub data: DataRef,
    #[serde(default)]
    pub label: String,
    /// Column name -> type name, as detected (or overridden).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub columns: BTreeMap<String, String>,
    /// `{ "lat": .., "lon": .. }` | `{ "wkt": .. }` | `{ "geojson": .. }` | `{ "file": true }`.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub geometry: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribution: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Where a source's bytes are (spec §6.3). A struct rather than a tagged
/// enum so an unknown `ref` kind still round-trips.
///
/// Known kinds: `file`, `url`, `embedded`, `sample`, `query`
/// ([`crate::sql`]) and `dataset`: a dataset hosted by a document service,
/// `{"ref": "dataset", "dataset": "<id>", "version": 3}` or `"version":
/// "latest"` ([`DataRef::dataset`]). A host resolves a `dataset` ref to
/// bytes itself; the keys live in `extra`, so a reader that does not know
/// the kind keeps them.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DataRef {
    /// `file` | `url` | `embedded` | `sample`.
    #[serde(rename = "ref")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `embedded` only: `text` (the file's own bytes, UTF-8) or
    /// `geoparquet-base64`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
    /// `embedded` with `encoding: "text"`: the data itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// `embedded` with `encoding: "geoparquet-base64"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl DataRef {
    pub fn file(path: impl Into<String>) -> Self {
        Self {
            kind: "file".into(),
            path: Some(path.into()),
            ..Self::default()
        }
    }
    pub fn url(url: impl Into<String>) -> Self {
        Self {
            kind: "url".into(),
            url: Some(url.into()),
            ..Self::default()
        }
    }
    /// Data carried inside the document as text (a CSV or GeoJSON file's
    /// own bytes); `name` keeps the file name for its kind and label.
    pub fn embedded_text(name: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            kind: "embedded".into(),
            encoding: Some("text".into()),
            text: Some(text.into()),
            name: Some(name.into()),
            ..Self::default()
        }
    }
    /// A sample bundled with the Studio, by name.
    pub fn sample(name: impl Into<String>) -> Self {
        Self {
            kind: "sample".into(),
            name: Some(name.into()),
            ..Self::default()
        }
    }
    /// A hosted dataset by id: version `Some(n)` pins it, `None` follows
    /// the latest version (`"version": "latest"`).
    pub fn dataset(id: impl Into<String>, version: Option<u64>) -> Self {
        let mut extra = Map::new();
        extra.insert("dataset".into(), Value::String(id.into()));
        extra.insert(
            "version".into(),
            version.map_or_else(|| Value::String(DATASET_LATEST.into()), Value::from),
        );
        Self {
            kind: DATASET_REF.into(),
            extra,
            ..Self::default()
        }
    }
    /// A `dataset` ref's id, `None` for any other kind.
    pub fn dataset_id(&self) -> Option<&str> {
        (self.kind == DATASET_REF)
            .then(|| self.extra.get("dataset").and_then(Value::as_str))
            .flatten()
    }
    /// A `dataset` ref's version: `Some(Some(n))` pinned, `Some(None)`
    /// latest (also when the key is absent), `None` for any other kind.
    pub fn dataset_version(&self) -> Option<Option<u64>> {
        (self.kind == DATASET_REF).then(|| self.extra.get("version").and_then(Value::as_u64))
    }
}

/// `data.ref` of a hosted dataset ([`DataRef::dataset`]).
pub const DATASET_REF: &str = "dataset";
/// `data.version` of a dataset ref that follows the latest version.
pub const DATASET_LATEST: &str = "latest";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MapBlock {
    #[serde(default)]
    pub view: View,
    #[serde(default)]
    pub layers: Vec<Layer>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for MapBlock {
    fn default() -> Self {
        Self {
            view: View::default(),
            layers: Vec::new(),
            extra: Map::new(),
        }
    }
}

/// The camera and the basemap by NAME (never a style URL, which could
/// carry a token). `basemap` is `streets` (the default, and what a
/// document without the key reads as), `dark` or `light` -- the names a
/// Kepler.gl import writes -- or one of the Studio style chooser's others
/// (`open-streets`, `swat-routes-dark`, `liberty`, `imagery`, and the
/// `mapbox-*` styles, drawn as their open look-alikes without Mapbox). A
/// name a reader does not know is drawn as `streets` and written back as
/// it was.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct View {
    pub lon: f64,
    pub lat: f64,
    pub zoom: f64,
    #[serde(default)]
    pub pitch: f64,
    #[serde(default)]
    pub bearing: f64,
    #[serde(default = "default_basemap")]
    pub basemap: String,
    #[serde(default = "default_projection")]
    pub projection: String,
    /// The unit `pitch` and `bearing` are in: always [`ANGLES_DEGREES`] once
    /// read. Documents written before this key existed may hold radians
    /// (`project::migrate` converts them); the key makes the unit
    /// unambiguous for every document written since.
    #[serde(default = "default_angles")]
    pub angles: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// [`View::extra`]'s key for a basemap the reader chose (`true`): drawn
/// as named in both appearances, even a default light name (`streets`,
/// `light`) that dark appearance otherwise draws on Graphite
/// (`dashboard::view_basemap_for_appearance`). Absent is unpinned.
pub const BASEMAP_PINNED: &str = "basemapPinned";

impl View {
    /// The reader's choice of basemap: `name`, pinned ([`BASEMAP_PINNED`]).
    pub fn pin_basemap(&mut self, name: &str) {
        self.basemap = name.to_string();
        self.extra.insert(BASEMAP_PINNED.into(), Value::Bool(true));
    }

    /// Whether the reader chose [`Self::basemap`] ([`Self::pin_basemap`]).
    pub fn basemap_pinned(&self) -> bool {
        self.extra
            .get(BASEMAP_PINNED)
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }
}

/// [`View::angles`]: pitch and bearing in degrees.
pub const ANGLES_DEGREES: &str = "degrees";

fn default_angles() -> String {
    ANGLES_DEGREES.into()
}

fn default_basemap() -> String {
    "streets".into()
}
fn default_projection() -> String {
    "flat".into()
}

impl Default for View {
    fn default() -> Self {
        Self {
            lon: 0.0,
            lat: 20.0,
            zoom: 1.5,
            pitch: 0.0,
            bearing: 0.0,
            basemap: default_basemap(),
            projection: default_projection(),
            angles: default_angles(),
            extra: Map::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LayerType {
    Points,
    GeoJson,
    /// Points binned into hexagons (Kepler's `hexagon`).
    Hexbin,
    /// Points binned into squares (Kepler's `grid`).
    Grid,
    /// Origin-destination arcs from two coordinate pairs.
    Arc,
    /// The same pairs drawn flat (an arc of height 0).
    Line,
    /// Points grouped into count-sized clusters.
    Cluster,
    /// A column of H3 cell ids drawn as hexagons (native only).
    H3,
    /// Screen-space point density (Kepler's `heatmap`). Its size channel
    /// (`Layer::radius`) is the weight by column; its colour binding (no
    /// field) holds the ramp's palette.
    Heatmap,
    /// Animated paths with per-vertex times (Kepler's `trip`): GeoJSON
    /// lines whose coordinates carry a time, or rows of
    /// `id`/`lat`/`lon`/`timestamp` grouped by `id`.
    Trip,
    /// Origin-destination rows aggregated into corridors between places
    /// (both ends snapped to a hexagonal lattice, weights summed), the
    /// heaviest `topN` drawn Edge-Path bundled, brightness by the load
    /// sharing each stretch. Reads the arc's two pairs, plus an optional
    /// `weight` column. Not a Kepler type: exported as an arc.
    Flow,
    /// Origin-destination rows as a hexagon choropleth of departures; a
    /// clicked cell fans arcs to the cells it exchanges journeys with,
    /// read outbound, inbound or both (`direction`). Reads the arc's two
    /// pairs, plus an optional `weight` column. Not a Kepler type:
    /// exported as an arc.
    #[serde(rename = "hex_demand")]
    HexDemand,
}

impl LayerType {
    /// Every type, in the editor's order.
    pub const ALL: [LayerType; 12] = [
        LayerType::Points,
        LayerType::GeoJson,
        LayerType::Hexbin,
        LayerType::Grid,
        LayerType::Heatmap,
        LayerType::Arc,
        LayerType::Line,
        LayerType::Flow,
        LayerType::HexDemand,
        LayerType::Trip,
        LayerType::Cluster,
        LayerType::H3,
    ];

    pub fn label(self) -> &'static str {
        match self {
            LayerType::Points => "Point",
            LayerType::GeoJson => "GeoJSON",
            LayerType::Hexbin => "Hexbin",
            LayerType::Grid => "Grid",
            LayerType::Arc => "Arc",
            LayerType::Line => "Line",
            LayerType::Cluster => "Cluster",
            LayerType::H3 => "H3",
            LayerType::Heatmap => "Heatmap",
            LayerType::Trip => "Trip",
            LayerType::Flow => "Bundled flows",
            LayerType::HexDemand => "Hex demand",
        }
    }

    /// Kepler.gl's layer `type`.
    pub fn kepler_name(self) -> &'static str {
        match self {
            LayerType::Points => "point",
            LayerType::GeoJson => "geojson",
            LayerType::Hexbin => "hexagon",
            LayerType::Grid => "grid",
            LayerType::Arc => "arc",
            LayerType::Line => "line",
            LayerType::Cluster => "cluster",
            LayerType::H3 => "hexagonId",
            LayerType::Heatmap => "heatmap",
            LayerType::Trip => "trip",
            // Kepler has neither (an export writes them as the arc they
            // read); these names key the Studio's own chips.
            LayerType::Flow => "flow",
            LayerType::HexDemand => "hex_demand",
        }
    }

    /// The type a Kepler.gl layer `type` names.
    pub fn from_kepler(name: &str) -> Option<LayerType> {
        Some(match name {
            "point" => LayerType::Points,
            "geojson" => LayerType::GeoJson,
            "hexagon" => LayerType::Hexbin,
            "grid" => LayerType::Grid,
            "arc" => LayerType::Arc,
            "line" => LayerType::Line,
            "cluster" => LayerType::Cluster,
            "hexagonId" | "h3" => LayerType::H3,
            "heatmap" => LayerType::Heatmap,
            "trip" => LayerType::Trip,
            _ => return None,
        })
    }

    /// Binned on the CPU: its colour reads each cell's aggregate.
    pub fn is_binned(self) -> bool {
        matches!(self, LayerType::Hexbin | LayerType::Grid)
    }

    /// Drawn from two coordinate pairs.
    pub fn is_od(self) -> bool {
        matches!(
            self,
            LayerType::Arc | LayerType::Line | LayerType::Flow | LayerType::HexDemand
        )
    }

    /// Reads its rows as flows between places (aggregated onto a lattice,
    /// optionally weighted by a `weight` column), not one mark per row.
    pub fn is_flow(self) -> bool {
        matches!(self, LayerType::Flow | LayerType::HexDemand)
    }

    /// Reads one lat/lon pair (the table's point geometry).
    pub fn reads_points(self) -> bool {
        matches!(
            self,
            LayerType::Points
                | LayerType::Hexbin
                | LayerType::Grid
                | LayerType::Cluster
                | LayerType::Heatmap
        )
    }

    /// Filtered on the GPU (a window is a uniform): points and heatmaps.
    pub fn gpu_filtered(self) -> bool {
        matches!(self, LayerType::Points | LayerType::Heatmap)
    }
}

/// Kepler's heatmap defaults (`heatmapRadius`, `intensity`, `threshold`).
pub const DEFAULT_HEATMAP_RADIUS: f64 = 20.0;
pub const DEFAULT_INTENSITY: f64 = 1.0;
pub const DEFAULT_THRESHOLD: f64 = 0.18;
/// Bundled flows' defaults: the corridors bundled, the paper's detour
/// limit (`k` = 2, Wallinger et al. 2022 section 6.2) and edge-weight
/// exponent (`d` = 2).
///
/// 10,000 corridors, measured on the ONS commute (33,493 cell pairs at
/// 5 km): 2,000 drew isolated city stars (413 routed); 10,000 draws the
/// connected field the reference shows (5,621 routed, 184k strands, 737k
/// vertices, bundled in 254 ms in a debug build); all 33,493 saturate the
/// country to white under screen blending (867k strands, 3.0 s debug).
pub const DEFAULT_TOP_N: usize = 10_000;
/// Bundled flows' detour limit and hop exponent: the paper's and the
/// mobitwin reference's `k` = 2 and `d` = 2. 3 and 3 were tried
/// (2026-10-09: routes chain through hubs) and drew big overshooting loops
/// at regional zooms; they stay an opt-in ([`TUNED_MAX_DETOUR`]).
pub const DEFAULT_MAX_DETOUR: f64 = 2.0;
pub const DEFAULT_WEIGHT_EXPONENT: f64 = 2.0;
/// The tuned, opt-in detour and exponent.
pub const TUNED_MAX_DETOUR: f64 = 3.0;
pub const TUNED_WEIGHT_EXPONENT: f64 = 3.0;
/// Kepler's trip `trailLength`, seconds.
pub const DEFAULT_TRAIL_S: f64 = 180.0;

/// Kepler.gl's hexbin and grid defaults (`constants/src/layers.ts`).
pub const DEFAULT_CELL_KM: f64 = 1.0;
pub const DEFAULT_COVERAGE: f64 = 1.0;
pub const DEFAULT_ELEVATION_SCALE: f64 = 5.0;
/// Kepler's `elevationRange` / `heightRange`, metres before the scale.
pub const DEFAULT_HEIGHT_RANGE: [f64; 2] = [0.0, 500.0];
/// Kepler's `clusterRadius`, px.
pub const DEFAULT_CLUSTER_RADIUS: f64 = 40.0;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Layer {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: LayerType,
    pub source: String,
    #[serde(default)]
    pub label: String,
    #[serde(default = "yes")]
    pub visible: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub columns: BTreeMap<String, String>,
    #[serde(default)]
    pub vis: Vis,
    /// Colour by column (P2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<ChannelBinding>,
    /// Size by column (P2); a heatmap's weight by column (Kepler's
    /// `weightField`, onto `0..=1`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub radius: Option<ChannelBinding>,
    /// Height by column: a 3D GeoJSON layer's extrusion (P6, Kepler's
    /// `heightField` + `heightScale` + `heightRange`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<ChannelBinding>,
    #[serde(default)]
    pub hover: Hover,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn yes() -> bool {
    true
}

/// Kepler's `visConfig` subset P1 edits.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Vis {
    /// Kepler units, 0..=100 (converted to pixels by the host).
    pub radius: f64,
    /// 0..=1.
    pub opacity: f64,
    #[serde(default = "yes")]
    pub filled: bool,
    #[serde(default)]
    pub outline: bool,
    /// Stroke width in px, 0..=10.
    #[serde(default = "one")]
    pub thickness: f64,
    pub color: [u8; 3],
    /// The outline's colour; `None` draws it a shade darker than the fill
    /// (or than each fill class, under a colour scale).
    #[serde(
        default,
        rename = "strokeColor",
        skip_serializing_if = "Option::is_none"
    )]
    pub stroke_color: Option<[u8; 3]>,
    /// Hexbin and grid: a cell's size in km (Kepler's `worldUnitSize`).
    #[serde(
        default,
        rename = "worldUnitSize",
        skip_serializing_if = "Option::is_none"
    )]
    pub cell_km: Option<f64>,
    /// Hexbin and grid: the drawn share of a cell, `0..=1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<f64>,
    /// Hexbin and grid: the percentile window of cell values drawn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percentile: Option<[f64; 2]>,
    /// Hexbin, grid and GeoJSON: extruded in 3D.
    #[serde(default, rename = "enable3d", skip_serializing_if = "is_false")]
    pub enable3d: bool,
    /// The 3D height's multiplier (Kepler's `elevationScale`).
    #[serde(
        default,
        rename = "elevationScale",
        skip_serializing_if = "Option::is_none"
    )]
    pub elevation_scale: Option<f64>,
    /// Hexbin and grid: how a cell's points become its colour (and
    /// height): `count`, `sum`, `average`, ... (Kepler's names).
    #[serde(
        default,
        rename = "colorAggregation",
        skip_serializing_if = "Option::is_none"
    )]
    pub aggregation: Option<String>,
    /// Arc and line: the colour at the target end.
    #[serde(
        default,
        rename = "targetColor",
        skip_serializing_if = "Option::is_none"
    )]
    pub target_color: Option<[u8; 3]>,
    /// Cluster: the grouping radius in px (Kepler's `clusterRadius`).
    #[serde(
        default,
        rename = "clusterRadius",
        skip_serializing_if = "Option::is_none"
    )]
    pub cluster_radius: Option<f64>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Vis {
    pub fn cell_km(&self) -> f64 {
        self.cell_km.unwrap_or(DEFAULT_CELL_KM)
    }
    pub fn coverage(&self) -> f64 {
        self.coverage.unwrap_or(DEFAULT_COVERAGE)
    }
    pub fn percentile(&self) -> [f64; 2] {
        self.percentile.unwrap_or([0.0, 100.0])
    }
    pub fn elevation_scale(&self) -> f64 {
        self.elevation_scale.unwrap_or(DEFAULT_ELEVATION_SCALE)
    }
    pub fn aggregation(&self) -> &str {
        self.aggregation.as_deref().unwrap_or("count")
    }
    pub fn cluster_radius(&self) -> f64 {
        self.cluster_radius.unwrap_or(DEFAULT_CLUSTER_RADIUS)
    }
    /// A Kepler `visConfig` number kept in `extra` (the keys only some
    /// types read, round-tripped as they came).
    fn extra_num(&self, key: &str) -> Option<f64> {
        self.extra
            .get(key)
            .and_then(Value::as_f64)
            .filter(|v| v.is_finite())
    }
    /// Heatmap: the density multiplier (Kepler's `intensity`).
    pub fn intensity(&self) -> f64 {
        self.extra_num("intensity").unwrap_or(DEFAULT_INTENSITY)
    }
    /// Heatmap: the share of the maximum below which density fades out.
    pub fn threshold(&self) -> f64 {
        self.extra_num("threshold").unwrap_or(DEFAULT_THRESHOLD)
    }
    /// Trip: how far behind its head a trail reaches, seconds (Kepler's
    /// `trailLength`).
    pub fn trail_s(&self) -> f64 {
        self.extra_num("trailLength").unwrap_or(DEFAULT_TRAIL_S)
    }
    /// Bundled flows: how many of the heaviest corridors are bundled.
    pub fn top_n(&self) -> usize {
        self.extra_num("topN")
            .map_or(DEFAULT_TOP_N, |v| v.round().clamp(1.0, 1e6) as usize)
    }
    /// Bundled flows: how many times the straight line a bundled route
    /// may be (the paper's `t`).
    pub fn max_detour(&self) -> f64 {
        self.extra_num("maxDetour").unwrap_or(DEFAULT_MAX_DETOUR)
    }
    /// Bundled flows: the exponent on a hop's length when costing a
    /// route (the paper's edge weight `d`).
    pub fn weight_exponent(&self) -> f64 {
        self.extra_num("weightExponent")
            .unwrap_or(DEFAULT_WEIGHT_EXPONENT)
    }
    /// Bundled flows: how the corridors are bundled, `path` (edge-path
    /// bundling, the default) or `density` (kernel-density bundling).
    pub fn bundling(&self) -> &str {
        match self.extra.get("bundling").and_then(Value::as_str) {
            Some("density") => "density",
            _ => "path",
        }
    }
    /// Bundled flows and hex demand: the lattice's cell radius, km; `None`
    /// chooses one from the data's extent.
    pub fn node_km(&self) -> Option<f64> {
        self.extra_num("nodeKm").filter(|v| *v > 0.0)
    }
    /// Bundled flows: how the strands are drawn, `glow` (the default:
    /// every corridor its own hairline, composited so coincident strands
    /// brighten, after the mobitwin-edgebundling render) or `bands` (load
    /// banded into a palette over opaque strokes).
    pub fn flow_style(&self) -> &str {
        match self.extra.get("flowStyle").and_then(Value::as_str) {
            Some("bands") => "bands",
            _ => "glow",
        }
    }
    /// Hex demand: which way a selected cell's flows read, `out`, `in` or
    /// `both`.
    pub fn direction(&self) -> &str {
        self.extra
            .get("direction")
            .and_then(Value::as_str)
            .unwrap_or("out")
    }
    /// Sets one of the `extra` numbers ([`Self::intensity`] and kin).
    pub fn set_extra_num(&mut self, key: &str, v: f64) {
        self.extra.insert(key.into(), serde_json::json!(v));
    }
}

fn one() -> f64 {
    1.0
}

impl Default for Vis {
    fn default() -> Self {
        Self {
            radius: 10.0,
            opacity: 0.8,
            filled: true,
            outline: false,
            thickness: 1.0,
            color: [0x2e, 0x86, 0xab],
            stroke_color: None,
            cell_km: None,
            coverage: None,
            percentile: None,
            enable3d: false,
            elevation_scale: None,
            aggregation: None,
            target_color: None,
            cluster_radius: None,
            extra: Map::new(),
        }
    }
}

/// `{"field", "scale", "palette", "steps", "reversed", "range"}` (spec §6.2).
///
/// A colour binding names its palette (a colour range's exact name, which
/// fixes the step count) and may reverse it. A size binding carries its
/// `range` in the host's units (Kepler units for a point radius, which the
/// host converts to pixels as it does the fixed radius).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChannelBinding {
    pub field: String,
    /// `quantile` | `quantize` | `linear` | `ordinal` (colour);
    /// `linear` | `sqrt` | `log` (size).
    pub scale: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub palette: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub reversed: bool,
    /// The palette's colour count, kept beside its name for readers that
    /// do not know the palette table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<[f64; 2]>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl ChannelBinding {
    /// A binding of `field` through `scale`, nothing else set.
    pub fn new(field: impl Into<String>, scale: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            scale: scale.into(),
            palette: None,
            reversed: false,
            steps: None,
            range: None,
            extra: Map::new(),
        }
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Hover {
    #[serde(default)]
    pub card: Vec<HoverField>,
    #[serde(default)]
    pub highlight: bool,
    /// The hover highlight's colour (Kepler's `highlightColor`); `None` is
    /// the Studio's default white glow.
    #[serde(
        default,
        rename = "highlightColor",
        skip_serializing_if = "Option::is_none"
    )]
    pub highlight_color: Option<[u8; 3]>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Hover {
    /// Whether the layer shows a hover card at all (a Kepler.gl config
    /// with `interactionConfig.tooltip.enabled: false` turns it off).
    pub fn enabled(&self) -> bool {
        self.extra.get("enabled").and_then(Value::as_bool) != Some(false)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HoverField {
    pub field: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl HoverField {
    pub fn new(field: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            format: None,
            extra: Map::new(),
        }
    }
}

/// A filter (spec §5.1): Kepler's `{id, dataId, name, type, value,
/// enabled}`. Where it runs ([`Placement`]) is not saved: it is
/// recomputed on load, in creation order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Filter {
    pub id: String,
    /// The dataset (a source id).
    #[serde(rename = "dataId")]
    pub source: String,
    /// The field it filters.
    #[serde(rename = "name")]
    pub field: String,
    #[serde(rename = "type")]
    pub kind: FilterKind,
    #[serde(default)]
    pub value: FilterValue,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Playback speed of a time filter (0.5, 1, 2, 4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<f64>,
    #[serde(skip)]
    pub placement: Placement,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Filter {
    pub fn new(
        id: impl Into<String>,
        source: impl Into<String>,
        field: impl Into<String>,
        kind: FilterKind,
        value: FilterValue,
    ) -> Self {
        Self {
            id: id.into(),
            source: source.into(),
            field: field.into(),
            kind,
            value,
            enabled: true,
            speed: None,
            placement: Placement::Cpu,
            extra: Map::new(),
        }
    }

    /// The window a GPU slot draws: the value's, or none when disabled.
    pub fn window(&self) -> Option<[f64; 2]> {
        match (&self.value, self.enabled) {
            (FilterValue::Range(r), true) => Some(*r),
            _ => None,
        }
    }
}

impl Doc {
    /// Parses a document. A version newer than [`MAPDOC_VERSION`] is an
    /// error; unknown keys are kept.
    pub fn from_json(text: &str) -> Result<Doc, DocError> {
        let value: Value =
            serde_json::from_str(text).map_err(|e| DocError::Shape(e.to_string()))?;
        let value = crate::project::migrate(value)?;
        let mut doc: Doc =
            serde_json::from_value(value).map_err(|e| DocError::Shape(e.to_string()))?;
        doc.place_filters();
        Ok(doc)
    }

    /// Pretty JSON with every object's keys sorted, whatever
    /// `serde_json`'s map order is in the build that links this crate.
    pub fn to_json(&self) -> String {
        let value = serde_json::to_value(self).expect("a Doc always serialises");
        let mut out =
            serde_json::to_string_pretty(&sorted(value)).expect("a Value always serialises");
        out.push('\n');
        out
    }

    pub fn layer(&self, id: &str) -> Option<&Layer> {
        self.map.layers.iter().find(|l| l.id == id)
    }

    pub fn layer_mut(&mut self, id: &str) -> Option<&mut Layer> {
        self.map.layers.iter_mut().find(|l| l.id == id)
    }

    /// Whether `source` is drawn only by point layers (and by at least
    /// one): its range and time filters may take GPU slots.
    pub fn gpu_capable(&self, source: &str) -> bool {
        // Binned, clustered and arc layers read every row on the CPU.
        let mut layers = self.layers_of(source).peekable();
        layers.peek().is_some() && layers.all(|l| l.kind.gpu_filtered())
    }

    /// Recomputes every filter's placement ([`crate::filters::allocate_slots`]),
    /// keeping the slots filters already hold.
    pub fn place_filters(&mut self) {
        let previous = self
            .filters
            .iter()
            .map(|f| (f.id.clone(), f.placement))
            .collect();
        let placed =
            crate::filters::allocate_slots(&self.filters, &previous, |s| self.gpu_capable(s));
        for (f, p) in self.filters.iter_mut().zip(placed) {
            f.placement = p;
        }
    }

    pub fn filter(&self, id: &str) -> Option<&Filter> {
        self.filters.iter().find(|f| f.id == id)
    }

    pub fn chart(&self, id: &str) -> Option<&Chart> {
        self.charts.iter().find(|c| c.id == id)
    }

    /// The layers drawing `source`.
    pub fn layers_of<'a>(&'a self, source: &'a str) -> impl Iterator<Item = &'a Layer> + 'a {
        self.map.layers.iter().filter(move |l| l.source == source)
    }
}

/// `value` with every object's keys in sorted order.
fn sorted(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(entries.into_iter().map(|(k, v)| (k, sorted(v))).collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sorted).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
  "mapdoc": 1,
  "profile": "map",
  "title": "Cities",
  "created_by": "gpui-maps Studio 0.7",
  "sources": {
    "cities": {
      "kind": "csv",
      "data": { "ref": "file", "path": "data/world_cities.csv", "bytes": 1520 },
      "label": "World cities",
      "columns": { "population": "integer", "name": "string" },
      "geometry": { "lat": "lat", "lon": "lon" },
      "attribution": "gpui-maps sample"
    }
  },
  "map": {
    "view": { "lon": 10.0, "lat": 20.0, "zoom": 1.5, "basemap": "streets" },
    "layers": [
      {
        "id": "cities-points", "type": "points", "source": "cities", "label": "Cities", "visible": true,
        "columns": { "lat": "lat", "lon": "lon" },
        "vis": { "radius": 10, "opacity": 0.8, "filled": true, "outline": false, "thickness": 2, "color": [46, 134, 171] },
        "hover": { "card": [ { "field": "name" }, { "field": "population", "format": ",d" } ], "highlight": true }
      }
    ]
  },
  "filters": [],
  "explorer": { "panels": { "layers": "open" } }
}"#;

    #[test]
    fn add_chart_round_trips_and_older_docs_load() {
        // An older document (no charts) loads with none and writes no key.
        let old = Doc::from_json(SAMPLE).expect("parses");
        assert!(old.charts.is_empty());
        assert!(!old.to_json().contains("\"charts\""));
        assert_eq!(old.mapdoc, MAPDOC_VERSION, "mapdoc 1 unchanged");
        let mut doc = old.clone();
        let mut chart = Chart::new("chart-1", "cities", ChartKind::Histogram, "population");
        chart.bins = Some(30);
        chart.measure = Some(Measure {
            op: "sum".into(),
            field: Some("population".into()),
        });
        doc.apply(crate::action::DocAction::AddChart(chart.clone()));
        let text = doc.to_json();
        assert!(text.contains("\"mapdoc\": 1"));
        let back = Doc::from_json(&text).expect("re-parses");
        assert_eq!(back.charts, vec![chart]);
        assert_eq!(back.to_json(), text, "byte for byte");
    }

    /// `view.basemap`: absent reads as `streets`; a chooser name (and one
    /// this build does not know) round-trips as written.
    #[test]
    fn the_basemap_name_defaults_to_streets_and_round_trips() {
        let without = SAMPLE.replace(r#", "basemap": "streets""#, "");
        assert!(!without.contains("basemap"));
        assert_eq!(
            Doc::from_json(&without).expect("parses").map.view.basemap,
            "streets",
            "absent is the default"
        );
        for name in [
            "dark",
            "liberty",
            "mapbox-swat-routes-dark",
            "imagery",
            "a-future-style",
        ] {
            let mut doc = Doc::from_json(SAMPLE).expect("parses");
            let mut view = doc.map.view.clone();
            view.basemap = name.into();
            assert_eq!(
                doc.apply(crate::action::DocAction::SetView(view)),
                vec![crate::action::Effect::Camera]
            );
            let text = doc.to_json();
            assert!(
                text.contains(&format!("\"basemap\": \"{name}\"")),
                "{name} is written"
            );
            let back = Doc::from_json(&text).expect("re-parses");
            assert_eq!(back.map.view.basemap, name);
            assert_eq!(back.to_json(), text, "{name}: byte for byte");
        }
    }

    #[test]
    fn a_document_round_trips_byte_for_byte() {
        let doc = Doc::from_json(SAMPLE).expect("parses");
        let once = doc.to_json();
        let again = Doc::from_json(&once).expect("re-parses").to_json();
        assert_eq!(once, again);
        // Sorted keys: "created_by" before "explorer" before "filters".
        let (a, b, c) = (
            once.find("\"created_by\"").unwrap(),
            once.find("\"explorer\"").unwrap(),
            once.find("\"filters\"").unwrap(),
        );
        assert!(a < b && b < c, "{once}");
        assert_eq!(Doc::from_json(&once).unwrap(), doc);
    }

    #[test]
    fn unknown_keys_survive_a_load_and_save() {
        let text = SAMPLE
            .replacen(
                "\"title\": \"Cities\",",
                "\"title\": \"Cities\", \"future_top\": {\"x\": 1},",
                1,
            )
            .replacen(
                "\"kind\": \"csv\",",
                "\"kind\": \"csv\", \"future_source\": [1, 2],",
                1,
            )
            .replacen(
                "\"id\": \"cities-points\",",
                "\"id\": \"cities-points\", \"blend\": \"additive\",",
                1,
            )
            .replacen(
                "\"radius\": 10,",
                "\"radius\": 10, \"fixedRadius\": false,",
                1,
            )
            .replacen("\"ref\": \"file\",", "\"ref\": \"file\", \"mtime\": 7,", 1);
        let saved = Doc::from_json(&text).expect("parses").to_json();
        let value: Value = serde_json::from_str(&saved).unwrap();
        assert_eq!(value["future_top"]["x"], 1);
        assert_eq!(
            value["sources"]["cities"]["future_source"],
            serde_json::json!([1, 2])
        );
        assert_eq!(value["sources"]["cities"]["data"]["mtime"], 7);
        assert_eq!(value["map"]["layers"][0]["blend"], "additive");
        assert_eq!(value["map"]["layers"][0]["vis"]["fixedRadius"], false);
    }

    #[test]
    fn a_newer_document_is_refused_with_a_message() {
        let text = SAMPLE.replacen("\"mapdoc\": 1", "\"mapdoc\": 9", 1);
        let err = Doc::from_json(&text).unwrap_err().to_string();
        assert!(err.contains("version 9") && err.len() <= 200, "{err}");
    }
}

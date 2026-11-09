//! GHSL population grid geometry and the count-tile wire codec.
//!
//! `grid` maps a lon/lat population raster (GHSL's native lat/lon grid, at
//! 3″ or 30″ per cell) onto the Mercator tile pyramid this renderer already
//! draws, and back: a data-tile zoom, a row/col window per tile, and a
//! coarser pyramid built by doubling the cell size (`levels`, 0 = the
//! finest/base resolution). `codec` is the wire format one populated tile
//! is stored and shipped as. See
//! `docs/superpowers/specs/2026-09-26-world-dot-density-design.md` for the
//! design this implements.

pub mod blocks;
// Live count tiles from ClickHouse: the SQL, the RowBinary decode and the
// browser's proxy source are sans-IO and compile everywhere (Task 16: the
// web shell reads through `web/serve.py`'s `/clickhouse/` proxy, and the
// proxy's Python copy of the template is pinned to this file's by a
// fixture). `connect`, the only IO, is native-only. The native demo's
// `--clickhouse` switch is still behind `Cargo.toml`'s `clickhouse-demo`.
pub mod clickhouse;
pub mod codec;
pub mod codec_cat;
pub mod composition;
pub mod grid;
pub mod hash;
pub mod in_view;
pub mod ink;
pub mod layer;
pub mod level;
pub mod mask_class;
pub mod mask_step;
pub mod parts;
pub mod placement;
pub mod pmtiles_dir;
pub mod pmtiles_source;
pub mod shell_fetch;
pub mod sql_guard;

// File pread, blocking `reqwest`, and the worker thread that runs them --
// none of which `wasm32-unknown-unknown` can link (see `fetch_native`'s
// own module doc). `pmtiles_dir`/`pmtiles_source` above are sans-IO on
// purpose so they compile for every shell; this is the one file in
// `dotgrid` that does not.
#[cfg(not(target_arch = "wasm32"))]
pub mod fetch_native;

use std::sync::Arc;

use crate::geo::TileId;

/// One data source's shape: how big a cell is at the base level, where the
/// grid starts, how many pyramid levels it has, and what it can be asked
/// for (epochs).
#[derive(Debug, Clone, PartialEq)]
pub struct GridDescriptor {
    /// Arc-seconds of a level-0 cell: 3.0 or 30.0.
    pub base_arcsec: f64,
    /// Latitude of the top edge of row 0 (90.0 unless MG-2 says otherwise).
    pub origin_lat: f64,
    /// Longitude of the west edge of col 0 (-180.0).
    pub origin_lon: f64,
    /// Number of pyramid levels, 0 = base.
    pub levels: u8,
    /// Data-tile zoom of level 0 (`grid::base_data_zoom(base_arcsec)`), 9 for 3″, 6 for 30″.
    pub max_data_zoom: u8,
    pub epochs: Vec<u16>,
    pub attribution: String,
}

impl GridDescriptor {
    /// Refuses a descriptor the grid math cannot use -- what an archive's
    /// metadata or a ClickHouse table says is untrusted, and `levels - 1`
    /// (`level::level_for_zoom`, `layer::plan_dots`) and `max_data_zoom -
    /// level` (`grid::data_zoom`) are unsigned subtractions:
    /// - `levels >= 1`, and `levels <= max_data_zoom + 1` (the coarsest
    ///   level's data zoom is 0 at the lowest);
    /// - `base_arcsec` finite and `> 0`;
    /// - the origin finite and on the globe (`|origin_lat| <= 90`,
    ///   `|origin_lon| <= 180`).
    ///
    /// Both adapters call it: `pmtiles_source` at open, and
    /// `ClickHouseGridSource::from_descriptor` (which `connect` ends in).
    pub fn validate(&self) -> Result<(), GridError> {
        let refuse = |why: String| Err(GridError::Server(format!("unusable grid descriptor: {why}")));
        if self.levels == 0 {
            return refuse("0 levels".to_string());
        }
        if u16::from(self.levels) > u16::from(self.max_data_zoom) + 1 {
            return refuse(format!("{} levels but level 0 is at data zoom {}", self.levels, self.max_data_zoom));
        }
        if !(self.base_arcsec.is_finite() && self.base_arcsec > 0.0) {
            return refuse(format!("base_arcsec {}", self.base_arcsec));
        }
        if !(self.origin_lat.is_finite() && self.origin_lat.abs() <= 90.0) {
            return refuse(format!("origin_lat {}", self.origin_lat));
        }
        if !(self.origin_lon.is_finite() && self.origin_lon.abs() <= 180.0) {
            return refuse(format!("origin_lon {}", self.origin_lon));
        }
        Ok(())
    }
}

/// A race-dots archive's second half (C7): what its `"race"` metadata key
/// says, next to the [`GridDescriptor`] of its cell levels. Above
/// `crossover` the layer draws block-part tiles (`codec_cat`'s `DGP1`) at
/// `part_zoom`, keyed [`GridTileKey::part`]; below it, the grid's cell
/// levels as any dot source (`layer::race_plan_kind`).
///
/// `grid` is the same descriptor the source's
/// [`GridCountSource::descriptor`] returns, held here too so the plan kind
/// is a function of this value and a zoom alone.
#[derive(Debug, Clone, PartialEq)]
pub struct RaceDescriptor {
    pub grid: GridDescriptor,
    /// Groups per record (`composition::GROUPS`, 8).
    pub groups: u8,
    /// Mercator zoom the part tiles are baked at (12). Above
    /// `grid.max_data_zoom`: no part tile is a pyramid level's tile.
    pub part_zoom: u8,
    /// Camera zoom at and above which the layer draws parts (9.72).
    pub crossover: f64,
}

impl RaceDescriptor {
    /// The `"kind"` a race archive's `"race"` metadata key carries.
    pub const KIND: &'static str = "race-dots";

    /// Refuses a race descriptor the layer cannot plan: the grid must
    /// validate, `groups` must be `composition::GROUPS`, `part_zoom` must
    /// be finer than every cell level and at most 4 zooms past their base
    /// (so a far-field cover tile between the two splits into at most 8x8
    /// part tiles, `layer::plan_dots_kind`), and `crossover` within
    /// `[part_zoom - 3, part_zoom]`.
    pub fn validate(&self) -> Result<(), GridError> {
        self.grid.validate()?;
        let refuse = |why: String| Err(GridError::Server(format!("unusable race descriptor: {why}")));
        if usize::from(self.groups) != composition::GROUPS {
            return refuse(format!("{} groups, not {}", self.groups, composition::GROUPS));
        }
        if self.part_zoom <= self.grid.max_data_zoom || self.part_zoom > self.grid.max_data_zoom + 4 {
            return refuse(format!("part_zoom {} (cell levels end at z{})", self.part_zoom, self.grid.max_data_zoom));
        }
        // Parts from at most 3 zooms below their own: at the part zoom a
        // window is ~20 part tiles a side, 3 zooms out ~160 (review F1).
        let pz = f64::from(self.part_zoom);
        if !(self.crossover.is_finite() && self.crossover >= pz - 3.0 && self.crossover <= pz) {
            return refuse(format!("crossover {} for part_zoom {}", self.crossover, self.part_zoom));
        }
        Ok(())
    }
}

/// Which count tile a request or a cache entry names: a data tile at a
/// pyramid level, for one epoch, or a pair of epochs when a caller wants
/// both counts at once (an A/B transition -- `epoch_b == epoch` when there
/// is no second epoch, per `GridTile`'s own doc comment).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GridTileKey {
    pub tile: TileId,
    pub level: u8,
    pub epoch: u16,
    pub epoch_b: Option<u16>,
}

impl GridTileKey {
    /// The `level` of a part-tile key (C7): not a pyramid level. A pyramid
    /// level is below `GridDescriptor::levels`, a `u8`, so at most 254 --
    /// 255 can never name one. Nothing derives a zoom from it: the grid
    /// helpers (`grid::data_zoom`, `cell_arcsec`, `cell_window`) assert
    /// they are never handed it, and a part tile's zoom is its `tile.z`.
    pub const PART_LEVEL: u8 = u8::MAX;

    /// The key of the block-part tile (`codec_cat`'s `DGP1`) at `tile`.
    pub fn part(tile: TileId, epoch: u16) -> Self {
        Self { tile, level: Self::PART_LEVEL, epoch, epoch_b: None }
    }

    /// Whether this names a part tile rather than a pyramid level's cell tile.
    pub fn is_part(&self) -> bool {
        self.level == Self::PART_LEVEL
    }
}

/// One decoded count tile: every populated cell in the tile's window, as
/// `(local index, pop, pop_b)`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GridTile {
    /// (local index, pop, pop_b). pop_b == pop when the key has no epoch_b.
    pub cells: Vec<(u32, f32, f32)>,
    /// Sum of pop (f64 so a world tile sums exactly enough).
    pub total: f64,
    /// The 8 group counts of each cell, parallel to `cells` (a categorical
    /// `DGT2` tile, whose `pop` is their sum). Empty for a v1 `DGT1` tile.
    pub cats: Vec<[u32; composition::GROUPS]>,
    /// The block parts of a `DGP1` part tile, in unit world `[0, 1)`.
    /// Empty for a cell tile (and `cells` is empty for a part tile).
    pub parts: Vec<codec_cat::PartRecord>,
}

/// Everything that can go wrong fetching or decoding a count tile.
///
/// Hand-written `Display`/`Error` rather than `derive(thiserror::Error)`:
/// `thiserror` is not a dependency of this crate (see `Cargo.toml`), and
/// adding one just for this enum is not worth a new dependency.
#[derive(Debug, Clone, PartialEq)]
pub enum GridError {
    /// The byte stream ended before a header, index or count did.
    Truncated,
    /// The bytes a magic check expected weren't there: `codec`'s own
    /// 4-byte `MAGIC` for a count tile, or `pmtiles_dir`'s 7-byte
    /// `"PMTiles"` for an archive header.
    BadMagic,
    /// The version byte is not one this decoder understands.
    BadVersion(u8),
    /// A stored index does not fit inside its own header's `cells` window.
    IndexOutOfTile { idx: u32, cells: u32 },
    /// The gzip body did not inflate.
    Inflate(String),
    /// A tile fetch's HTTP response was not 2xx.
    Http { status: u16, body: String },
    /// A tile source reported a problem of its own (a bad archive, say).
    Server(String),
    /// A local I/O error reading a tile (file mode).
    Io(String),
    /// A fetch that did not connect or answer in time.
    Timeout(String),
}

impl std::fmt::Display for GridError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated => write!(f, "count tile ended before its header, index or counts did"),
            Self::BadMagic => write!(f, "bad magic: not a count tile or PMTiles archive"),
            Self::BadVersion(v) => write!(f, "count tile version {v} is not supported"),
            Self::IndexOutOfTile { idx, cells } => {
                write!(f, "cell index {idx} is outside the tile's {cells}-cell window")
            }
            Self::Inflate(e) => write!(f, "count tile did not inflate: {e}"),
            Self::Http { status, body } => write!(f, "count tile fetch failed: HTTP {status}: {body}"),
            Self::Server(e) => write!(f, "count tile source reported an error: {e}"),
            Self::Io(e) => write!(f, "count tile read failed: {e}"),
            Self::Timeout(e) => write!(f, "count tile fetch timed out: {e}"),
        }
    }
}

impl std::error::Error for GridError {}

/// Most bytes of a server's error text a [`GridError`] keeps: enough for a
/// ClickHouse exception's first line, not a whole HTML error page (or an
/// archive a misbehaving server sends back as the "error").
pub const MAX_ERROR_TEXT: usize = 512;

impl GridError {
    /// [`GridError::Http`] with `body` cut to [`MAX_ERROR_TEXT`] bytes.
    pub fn http(status: u16, body: &str) -> Self {
        Self::Http { status, body: bounded_error_text(body) }
    }
}

/// `text` trimmed and cut to [`MAX_ERROR_TEXT`] bytes (on a char
/// boundary), marked with `…` when cut.
pub(crate) fn bounded_error_text(text: &str) -> String {
    let text = text.trim();
    if text.len() <= MAX_ERROR_TEXT {
        return text.to_string();
    }
    let mut end = MAX_ERROR_TEXT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// What a [`GridCountSource`] needs done to answer a [`GridTileKey`] batch:
/// a byte range to read (a local path or an http(s) URL, the two things
/// [`fetch_native::execute`] and a wasm/iOS-side fetcher both know how to
/// turn into bytes), a ClickHouse POST, or nothing at all.
#[derive(Debug, Clone, PartialEq)]
pub enum GridRequest {
    /// A byte range of a local path or an http(s) URL.
    Range { location: String, offset: u64, length: u64 },
    /// ClickHouse: a POST of a SQL body.
    Post { url: String, body: String, headers: Vec<(String, String)> },
    /// A whole-body GET: the browser's ClickHouse proxy
    /// (`clickhouse::ClickHouseProxySource`, `web/serve.py`'s
    /// `/clickhouse/tile`), which takes validated parameters, never SQL.
    Get { url: String },
    /// Known absent (not in the directory): no IO, decodes to an empty tile.
    Absent,
}

/// One data source of count tiles -- a PMTiles archive, a ClickHouse
/// query, or (for tests) a closure -- described sans-IO: `request` says
/// what bytes are needed, a caller fetches them however its shell knows
/// how (native's blocking [`fetch_native::execute`], or a wasm/iOS
/// equivalent), and `decode` turns the result into tiles. No source here
/// ever performs IO itself, which is what lets `pmtiles_source` compile
/// for `wasm32-unknown-unknown` even though nothing there can open a
/// socket or a file.
pub trait GridCountSource: Send + Sync {
    fn descriptor(&self) -> &GridDescriptor;
    /// Most keys one request may carry (all at the same level and epoch).
    /// 1 for PMTiles, where every tile is its own byte range; a ClickHouse
    /// source can batch many keys into one query body.
    fn max_batch(&self) -> usize {
        1
    }
    fn request(&self, keys: &[GridTileKey]) -> GridRequest;
    fn decode(&self, keys: &[GridTileKey], body: &[u8]) -> Result<Vec<(GridTileKey, GridTile)>, GridError>;
    /// A race-dots source's part tiles and crossover (C7); `None` for a
    /// plain count-tile source, which only ever plans cell levels.
    fn race(&self) -> Option<&RaceDescriptor> {
        None
    }
}

/// A synthetic [`GridCountSource`] for tests: every tile comes from a
/// closure, never from IO. `request` is always [`GridRequest::Absent`] --
/// there is nothing to fetch -- and `decode` ignores the body entirely and
/// calls the closure for each key instead.
pub struct MemoryGridSource {
    descriptor: GridDescriptor,
    tiles: Arc<dyn Fn(&GridTileKey) -> GridTile + Send + Sync>,
    race: Option<RaceDescriptor>,
}

impl MemoryGridSource {
    pub fn new(descriptor: GridDescriptor, tiles: impl Fn(&GridTileKey) -> GridTile + Send + Sync + 'static) -> Self {
        Self { descriptor, tiles: Arc::new(tiles), race: None }
    }

    /// A race-dots source ([`GridCountSource::race`]): `race.grid` is its
    /// descriptor, and the closure answers part keys too.
    pub fn for_race(race: RaceDescriptor, tiles: impl Fn(&GridTileKey) -> GridTile + Send + Sync + 'static) -> Self {
        Self { descriptor: race.grid.clone(), tiles: Arc::new(tiles), race: Some(race) }
    }
}

impl GridCountSource for MemoryGridSource {
    fn descriptor(&self) -> &GridDescriptor {
        &self.descriptor
    }

    fn request(&self, _keys: &[GridTileKey]) -> GridRequest {
        GridRequest::Absent
    }

    fn decode(&self, keys: &[GridTileKey], _body: &[u8]) -> Result<Vec<(GridTileKey, GridTile)>, GridError> {
        Ok(keys.iter().map(|k| (*k, (self.tiles)(k))).collect())
    }

    fn race(&self) -> Option<&RaceDescriptor> {
        self.race.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d30() -> GridDescriptor {
        GridDescriptor {
            base_arcsec: 30.0,
            origin_lat: 90.0,
            origin_lon: -180.0,
            levels: 7,
            max_data_zoom: 6,
            epochs: vec![2025],
            attribution: "test".to_string(),
        }
    }

    fn refused(d: GridDescriptor) -> bool {
        matches!(d.validate(), Err(GridError::Server(_)))
    }

    #[test]
    fn the_shipped_descriptors_validate() {
        assert_eq!(d30().validate(), Ok(()));
        let d3 = GridDescriptor { base_arcsec: 3.0, levels: 10, max_data_zoom: 9, ..d30() };
        assert_eq!(d3.validate(), Ok(()));
    }

    #[test]
    fn a_descriptor_with_no_levels_is_refused() {
        assert!(refused(GridDescriptor { levels: 0, ..d30() }));
    }

    #[test]
    fn a_descriptor_with_more_levels_than_data_zooms_is_refused() {
        assert!(refused(GridDescriptor { levels: 8, ..d30() }));
        assert!(refused(GridDescriptor { levels: 255, max_data_zoom: 6, ..d30() }));
    }

    #[test]
    fn a_non_positive_or_non_finite_cell_size_is_refused() {
        for base_arcsec in [0.0, -30.0, f64::NAN, f64::INFINITY] {
            assert!(refused(GridDescriptor { base_arcsec, ..d30() }), "{base_arcsec}");
        }
    }

    #[test]
    fn an_origin_off_the_globe_is_refused() {
        for (origin_lat, origin_lon) in [(90.5, -180.0), (-91.0, -180.0), (90.0, -180.5), (90.0, 181.0), (f64::NAN, -180.0), (90.0, f64::INFINITY)] {
            assert!(refused(GridDescriptor { origin_lat, origin_lon, ..d30() }), "{origin_lat} {origin_lon}");
        }
    }

    /// Final review: an HTTP error keeps a bounded prefix of its body.
    #[test]
    fn an_http_error_body_is_bounded() {
        let GridError::Http { body, .. } = GridError::http(500, &"é".repeat(10_000)) else { unreachable!() };
        assert!(body.len() <= MAX_ERROR_TEXT + '…'.len_utf8(), "{} bytes", body.len());
        assert!(body.ends_with('…'));
        assert_eq!(GridError::http(404, " not found\n"), GridError::Http { status: 404, body: "not found".to_string() });
    }

    #[test]
    fn memory_grid_source_calls_the_closure_per_key_and_never_requests_io() {
        let descriptor = GridDescriptor {
            base_arcsec: 30.0,
            origin_lat: 90.0,
            origin_lon: -180.0,
            levels: 7,
            max_data_zoom: 6,
            epochs: vec![2025],
            attribution: "test".to_string(),
        };
        let source = MemoryGridSource::new(descriptor, |key| GridTile {
            cells: vec![(0, key.tile.x as f32, key.tile.x as f32)],
            total: f64::from(key.tile.x),
            ..Default::default()
        });

        let key_a = GridTileKey { tile: TileId { z: 6, x: 3, y: 4 }, level: 0, epoch: 2025, epoch_b: None };
        let key_b = GridTileKey { tile: TileId { z: 6, x: 9, y: 4 }, level: 0, epoch: 2025, epoch_b: None };

        assert_eq!(source.request(&[key_a]), GridRequest::Absent);

        let tiles = source.decode(&[key_a, key_b], &[]).unwrap();
        assert_eq!(tiles.len(), 2);
        assert_eq!(tiles[0], (key_a, GridTile { cells: vec![(0, 3.0, 3.0)], total: 3.0, ..Default::default() }));
        assert_eq!(tiles[1], (key_b, GridTile { cells: vec![(0, 9.0, 9.0)], total: 9.0, ..Default::default() }));
    }
}

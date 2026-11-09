//! MBTiles: a tileset in one SQLite file, as a [`TileSource`].
//!
//! The sibling of [`PmtilesSource`](crate::tile_source::pmtiles::PmtilesSource),
//! and shaped like it: [`MbtilesSource::open`] reads the file's own metadata
//! once (credit, zoom range, bounds, `vector_layers`), [`MbtilesSource::coverage`]
//! says where the data is, and the source goes to `MapView` as any other
//! `Arc<dyn TileSource>` -- `MapView::new` for vector (`pbf`) tiles,
//! `MapView::set_raster_source` for `png` / `jpg` / `webp` ones
//! ([`MbtilesMetadata::format`] says which). No second pipeline: the bytes a
//! fetch returns are the bytes an HTTP or PMTiles source would have returned.
//!
//! # The format (MBTiles 1.3)
//!
//! - `tiles(zoom_level, tile_column, tile_row, tile_data)`, a table or (in
//!   deduplicating writers such as OpenMapTiles' `map` + `images`) a view.
//!   `tile_row` is **TMS**: row 0 is the south edge, so XYZ `y` is stored as
//!   `2^z - 1 - y`. A `scheme` row of `xyz` in `metadata` (a few writers
//!   emit one) is honoured and the flip skipped.
//! - `metadata(name, value)`: `format`, `bounds` (`w,s,e,n`), `center`
//!   (`lon,lat,zoom`), `minzoom`, `maxzoom`, `attribution` (HTML, as
//!   TileJSON) and `json` (`vector_layers`).
//! - Vector tiles are usually gzip-compressed in the blob; they come out of
//!   [`TileSource::fetch`] decompressed, as `PmtilesSource`'s do. Raster
//!   tiles come out as stored.
//!
//! # Reads
//!
//! Every SQLite call runs on tokio's blocking pool (`spawn_blocking`), never
//! on the caller's thread, and through a small pool of read-only
//! connections, so concurrent fetches read concurrently instead of queueing
//! on one connection's mutex. See [`MbtilesSource::fetch`].
//!
//! Native only: an MBTiles file is a local file. Behind the `mbtiles`
//! feature (default on), and not compiled for `wasm32`.

use std::collections::{BTreeMap, HashMap};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OpenFlags, OptionalExtension as _};

use crate::geo::{LonLatBounds, TileId};
use crate::tile_source::pmtiles::Coverage;
use crate::tile_source::{TileSource, DEFAULT_MAX_ZOOM};

/// The tile query. Through `prepare_cached`, so each pooled connection
/// compiles it once.
const TILE_QUERY: &str = "SELECT tile_data FROM tiles WHERE zoom_level = ?1 AND tile_column = ?2 AND tile_row = ?3";

/// How many idle connections the pool keeps. More concurrent fetches than
/// this still each get a connection (one is opened for them, ~50 us); the
/// surplus is closed when returned instead of kept. Eight covers tokio's
/// blocking-pool concurrency for a screenful of tiles in practice: a
/// viewport asks for 12-30 tiles, and SQLite answers one in tens of
/// microseconds, so few are ever in flight at once.
const MAX_IDLE_CONNECTIONS: usize = 8;

/// Read-only connections to one file. `rusqlite::Connection` is `Send` but
/// not `Sync`: one connection serves one thread at a time, so concurrency
/// comes from several connections, not from sharing one.
struct Pool {
    path: PathBuf,
    idle: Mutex<Vec<Connection>>,
}

impl Pool {
    fn connect(path: &Path) -> rusqlite::Result<Connection> {
        // READ_ONLY: this source never writes, and a read-only open cannot
        // create a missing file (an empty database would "open" fine and
        // then fail at the first query, far from the typo). NO_MUTEX: the
        // pool already guarantees one thread per connection.
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.set_prepared_statement_cache_capacity(4);
        Ok(conn)
    }

    fn take(&self) -> rusqlite::Result<Connection> {
        let pooled = self.idle.lock().expect("mbtiles pool mutex poisoned").pop();
        match pooled {
            Some(conn) => Ok(conn),
            None => Self::connect(&self.path),
        }
    }

    fn give_back(&self, conn: Connection) {
        let mut idle = self.idle.lock().expect("mbtiles pool mutex poisoned");
        if idle.len() < MAX_IDLE_CONNECTIONS {
            idle.push(conn);
        }
    }
}

/// What the file's tiles are, from its `format` metadata row (or, when the
/// row is missing, sniffed from the first tile's magic bytes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MbtilesFormat {
    /// Mapbox Vector Tiles (`pbf`, also spelled `mvt`): goes to `MapView::new`.
    Pbf,
    Png,
    Jpeg,
    Webp,
    /// A `format` value this crate does not recognise, verbatim.
    Other(String),
}

impl MbtilesFormat {
    fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "pbf" | "mvt" => Self::Pbf,
            "png" => Self::Png,
            "jpg" | "jpeg" => Self::Jpeg,
            "webp" => Self::Webp,
            _ => Self::Other(value.to_string()),
        }
    }

    /// By magic bytes. Anything that is not a known image is taken for a
    /// vector tile: gzip (`1f 8b`), zlib or a bare protobuf all mean MVT.
    fn sniff(bytes: &[u8]) -> Self {
        if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
            Self::Png
        } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
            Self::Jpeg
        } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
            Self::Webp
        } else {
            Self::Pbf
        }
    }

    /// Whether these are vector tiles (`MapView::new`) rather than raster
    /// ones (`MapView::set_raster_source`).
    #[must_use]
    pub fn is_vector(&self) -> bool {
        matches!(self, Self::Pbf)
    }

    /// The format as MBTiles spells it: `pbf`, `png`, `jpg`, `webp`, or the
    /// unrecognised value.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Pbf => "pbf",
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Webp => "webp",
            Self::Other(s) => s,
        }
    }
}

/// An MBTiles file's description of itself, read once at open. See
/// [`MbtilesSource::metadata`].
#[derive(Debug, Clone)]
pub struct MbtilesMetadata {
    /// The `name` row.
    pub name: Option<String>,
    pub format: MbtilesFormat,
    /// The `description` row.
    pub description: Option<String>,
    /// `west, south, east, north` from `bounds`. `None` when absent or
    /// unparseable; the file is then treated as covering the world.
    pub bounds: Option<LonLatBounds>,
    /// `(lon, lat, zoom)` from `center`; the zoom is optional in practice.
    pub center: Option<(f64, f64, Option<u8>)>,
    /// The `minzoom`/`maxzoom` rows narrowed to the zooms `tiles` really
    /// holds (and those zooms alone when a row is missing). Narrowed
    /// because writers overstate: a z15-only city cut from a country file
    /// often keeps the country's `minzoom 0`, and a map that believed it
    /// would open on zoom levels with nothing in them.
    pub min_zoom: u8,
    pub max_zoom: u8,
    /// The extent of the tiles actually stored at `min_zoom`, read from the
    /// tile index at open. [`MbtilesSource::coverage`] frames this instead
    /// of `bounds`/`center` when the stated centre falls outside it.
    pub data_bounds: Option<LonLatBounds>,
    /// The `vector_layers[].id`s from the `json` row, in file order.
    pub vector_layers: Vec<String>,
    /// Every metadata row verbatim, `json` included.
    pub entries: BTreeMap<String, String>,
}

/// An MBTiles file as a [`TileSource`]. See the module docs.
pub struct MbtilesSource {
    pool: Arc<Pool>,
    metadata: MbtilesMetadata,
    /// `attribution_text` of the `attribution` row; empty when none.
    attribution: String,
    /// `true` when `tile_row` is TMS (the default) and must be flipped.
    tms: bool,
    layer_min_zooms: HashMap<String, u8>,
}

impl MbtilesSource {
    /// Opens the file read-only and reads its metadata, on tokio's blocking
    /// pool.
    ///
    /// Fails when the file does not exist, is not SQLite, or has no `tiles`
    /// table or view -- an MBTiles file without tiles is a wrong path, not
    /// an empty map. Missing or malformed metadata is NOT an error, as for
    /// `PmtilesSource`: no `attribution` leaves the credit empty, no
    /// `bounds` means the world, no `minzoom`/`maxzoom` reads the range off
    /// the tiles themselves, and no `format` is sniffed from the first tile.
    pub async fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || Self::open_blocking(&path)).await?
    }

    /// [`MbtilesSource::open`] on the calling thread, for callers outside a
    /// tokio runtime (tools, tests, a `main` that has not started one yet).
    pub fn open_blocking(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        if !path.is_file() {
            anyhow::bail!("{}: no such MBTiles file", path.display());
        }
        let conn = Pool::connect(path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        // Compiling the tile query is the test that this is MBTiles at all:
        // it fails on a non-SQLite file ("file is not a database") and on a
        // database without `tiles`.
        conn.prepare_cached(TILE_QUERY)
            .map_err(|e| anyhow::anyhow!("{} is not an MBTiles file: {e}", path.display()))?;
        let entries = read_metadata_rows(&conn)?;
        let metadata = describe(&conn, entries)?;
        let tms = !metadata.entries.get("scheme").is_some_and(|s| s.trim().eq_ignore_ascii_case("xyz"));
        let attribution = metadata
            .entries
            .get("attribution")
            .map(|html| crate::tile_source::attribution_text(html))
            .unwrap_or_default();
        let layer_min_zooms = metadata
            .entries
            .get("json")
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
            .map(|json| crate::tile_source::vector_layer_min_zooms(&json))
            .unwrap_or_default();
        let pool = Arc::new(Pool { path: path.to_path_buf(), idle: Mutex::new(vec![conn]) });
        Ok(Self { pool, metadata, attribution, tms, layer_min_zooms })
    }

    /// Everything the file says about itself: name, format, bounds, centre,
    /// zoom range, vector layers and every raw row.
    #[must_use]
    pub fn metadata(&self) -> &MbtilesMetadata {
        &self.metadata
    }

    /// Where the file holds data, in the shape `PmtilesSource::coverage`
    /// returns, so [`Coverage::opening_camera`] frames it the same way.
    /// Without `bounds` the coverage is the whole Web Mercator world; without
    /// `center`, the middle of the bounds with no stated zoom.
    ///
    /// When the stated centre (or, without one, the middle of the stated
    /// bounds) lies outside [`MbtilesMetadata::data_bounds`] -- a city
    /// extract still carrying its country's metadata -- the coverage is the
    /// data's own extent instead, with no stated zoom, so
    /// [`Coverage::opening_camera`] frames the tiles rather than an empty
    /// page.
    #[must_use]
    pub fn coverage(&self) -> Coverage {
        let m = &self.metadata;
        let stated = m.bounds.unwrap_or(LonLatBounds { min_lon: -180.0, min_lat: -85.051_129, max_lon: 180.0, max_lat: 85.051_129 });
        let middle = |b: &LonLatBounds| ((b.min_lon + b.max_lon) / 2.0, (b.min_lat + b.max_lat) / 2.0);
        let stated_center = m.center.unwrap_or_else(|| {
            let (lon, lat) = middle(&stated);
            (lon, lat, None)
        });
        let inside = |b: &LonLatBounds, (lon, lat): (f64, f64)| (b.min_lon..=b.max_lon).contains(&lon) && (b.min_lat..=b.max_lat).contains(&lat);
        let (b, (center_lon, center_lat, center_zoom)) = match m.data_bounds {
            Some(data) if !inside(&data, (stated_center.0, stated_center.1)) => {
                let (lon, lat) = middle(&data);
                (data, (lon, lat, None))
            }
            _ => (stated, stated_center),
        };
        Coverage {
            min_lon: b.min_lon,
            min_lat: b.min_lat,
            max_lon: b.max_lon,
            max_lat: b.max_lat,
            center_lon,
            center_lat,
            center_zoom: center_zoom.filter(|z| *z != 0),
            min_zoom: m.min_zoom,
            max_zoom: m.max_zoom,
        }
    }

    /// Whether the file can hold `tile` at all: inside the zoom range and
    /// within one tile of the stated bounds. The margin is there because
    /// writers round `bounds` (OpenMapTiles to 2-3 decimals), and a rounded
    /// edge can fall just inside a border tile that does hold data.
    fn may_hold(&self, tile: TileId) -> bool {
        if tile.z < self.metadata.min_zoom || tile.z > self.metadata.max_zoom {
            return false;
        }
        let Some(b) = self.metadata.bounds else { return true };
        let t = crate::geo::tile_bounds_lonlat(tile);
        let w = t.max_lon - t.min_lon;
        let h = t.max_lat - t.min_lat;
        t.max_lon + w >= b.min_lon && t.min_lon - w <= b.max_lon && t.max_lat + h >= b.min_lat && t.min_lat - h <= b.max_lat
    }
}

/// `metadata` as name -> value. A file without a `metadata` table is still
/// readable (the tiles are what matter), so that is an empty map, not an
/// error.
fn read_metadata_rows(conn: &Connection) -> anyhow::Result<BTreeMap<String, String>> {
    let Ok(mut stmt) = conn.prepare("SELECT name, value FROM metadata") else {
        return Ok(BTreeMap::new());
    };
    let rows = stmt.query_map([], |row| {
        // `value` is TEXT by spec; a writer that stored a number gets it
        // read back as its text.
        let name: String = row.get(0)?;
        let value = match row.get_ref(1)? {
            rusqlite::types::ValueRef::Text(t) | rusqlite::types::ValueRef::Blob(t) => String::from_utf8_lossy(t).into_owned(),
            rusqlite::types::ValueRef::Integer(i) => i.to_string(),
            rusqlite::types::ValueRef::Real(r) => r.to_string(),
            rusqlite::types::ValueRef::Null => String::new(),
        };
        Ok((name, value))
    })?;
    Ok(rows.filter_map(Result::ok).collect())
}

fn parse_floats(s: &str) -> Vec<f64> {
    s.split(',').filter_map(|p| p.trim().parse::<f64>().ok()).collect()
}

fn parse_zoom(s: Option<&String>) -> Option<u8> {
    // Some writers store "14.0".
    s.and_then(|v| v.trim().parse::<f64>().ok()).filter(|z| z.is_finite() && (0.0..=30.0).contains(z)).map(|z| z as u8)
}

fn describe(conn: &Connection, entries: BTreeMap<String, String>) -> anyhow::Result<MbtilesMetadata> {
    let bounds = entries.get("bounds").map(|s| parse_floats(s)).and_then(|v| {
        (v.len() == 4 && v.iter().all(|x| x.is_finite()) && v[0] <= v[2] && v[1] <= v[3]).then(|| LonLatBounds {
            min_lon: v[0].max(-180.0),
            min_lat: v[1].max(-90.0),
            max_lon: v[2].min(180.0),
            max_lat: v[3].min(90.0),
        })
    });
    let center = entries.get("center").map(|s| parse_floats(s)).and_then(|v| match v.as_slice() {
        [lon, lat] => Some((*lon, *lat, None)),
        [lon, lat, z, ..] => Some((*lon, *lat, (z.is_finite() && (0.0..=30.0).contains(z)).then_some(*z as u8))),
        _ => None,
    });

    // The zooms the tiles really hold. MIN/MAX over the leading column of
    // the tile index is an index seek, not a scan, on a plain `tiles`
    // table.
    let (lo, hi): (Option<i64>, Option<i64>) =
        conn.query_row("SELECT MIN(zoom_level), MAX(zoom_level) FROM tiles", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let (held_min, held_max) = (lo.and_then(|z| u8::try_from(z).ok()), hi.and_then(|z| u8::try_from(z).ok()));
    let (stated_min, stated_max) = (parse_zoom(entries.get("minzoom")), parse_zoom(entries.get("maxzoom")));
    let narrow = |stated: Option<u8>, held: Option<u8>, pick: fn(u8, u8) -> u8| match (stated, held) {
        (Some(s), Some(h)) => Some(pick(s, h)),
        (s, h) => s.or(h),
    };
    let min_zoom = narrow(stated_min, held_min, u8::max).unwrap_or(0);
    let max_zoom = narrow(stated_max, held_max, u8::min).unwrap_or(DEFAULT_MAX_ZOOM).max(min_zoom);

    // The extent of the tiles at `min_zoom`: the column range is an index
    // seek, the row range a walk over that one zoom's entries (the
    // fewest of any zoom).
    let tms = !entries.get("scheme").is_some_and(|s| s.trim().eq_ignore_ascii_case("xyz"));
    let data_bounds = conn
        .query_row(
            "SELECT MIN(tile_column), MAX(tile_column), MIN(tile_row), MAX(tile_row) FROM tiles WHERE zoom_level = ?1",
            [min_zoom],
            |r| Ok((r.get::<_, Option<u32>>(0)?, r.get::<_, Option<u32>>(1)?, r.get::<_, Option<u32>>(2)?, r.get::<_, Option<u32>>(3)?)),
        )
        .ok()
        .and_then(|(x0, x1, r0, r1)| {
            let (x0, x1, r0, r1) = (x0?, x1?, r0?, r1?);
            let n = 1u32.checked_shl(u32::from(min_zoom))?;
            if x1 >= n || r1 >= n {
                return None;
            }
            let (y0, y1) = if tms { (n - 1 - r1, n - 1 - r0) } else { (r0, r1) };
            let nw = crate::geo::tile_bounds_lonlat(TileId { z: min_zoom, x: x0, y: y0 });
            let se = crate::geo::tile_bounds_lonlat(TileId { z: min_zoom, x: x1, y: y1 });
            Some(LonLatBounds { min_lon: nw.min_lon, min_lat: se.min_lat, max_lon: se.max_lon, max_lat: nw.max_lat })
        });

    let format = match entries.get("format") {
        Some(f) if !f.trim().is_empty() => MbtilesFormat::parse(f),
        _ => conn
            .query_row("SELECT tile_data FROM tiles LIMIT 1", [], |r| r.get::<_, Vec<u8>>(0))
            .optional()?
            .map_or(MbtilesFormat::Pbf, |bytes| MbtilesFormat::sniff(&bytes)),
    };

    let vector_layers = entries
        .get("json")
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|json| {
            json.get("vector_layers")?
                .as_array()
                .map(|layers| layers.iter().filter_map(|l| l.get("id")?.as_str().map(str::to_string)).collect())
        })
        .unwrap_or_default();

    Ok(MbtilesMetadata {
        name: entries.get("name").cloned(),
        format,
        description: entries.get("description").cloned(),
        bounds,
        center,
        min_zoom,
        max_zoom,
        data_bounds,
        vector_layers,
        entries,
    })
}

/// One tile's blob, by MBTiles' own coordinates.
fn read_tile(pool: &Pool, z: u8, column: u32, row: u32) -> anyhow::Result<Option<Vec<u8>>> {
    let conn = pool.take()?;
    let result = conn
        .prepare_cached(TILE_QUERY)
        .and_then(|mut stmt| stmt.query_row((z, column, row), |r| r.get::<_, Vec<u8>>(0)).optional());
    // A connection that just failed a query is still a good connection
    // (the error is about the query or the file); it goes back either way.
    pool.give_back(conn);
    Ok(result?)
}

/// gzip (`1f 8b`, what tippecanoe, Planetiler and OpenMapTiles write) and
/// zlib (`78 xx`, some older writers) are undone; anything else is returned
/// as stored. Only called for vector tiles: a raster tile is never
/// re-encoded, and no image format starts with either header anyway.
fn decompress(bytes: Vec<u8>) -> anyhow::Result<Vec<u8>> {
    let gzip = bytes.starts_with(&[0x1f, 0x8b]);
    let zlib = bytes.len() >= 2 && bytes[0] == 0x78 && (u16::from(bytes[0]) << 8 | u16::from(bytes[1])) % 31 == 0;
    if !gzip && !zlib {
        return Ok(bytes);
    }
    let mut out = Vec::with_capacity(bytes.len() * 4);
    if gzip {
        flate2::read::MultiGzDecoder::new(&bytes[..]).read_to_end(&mut out)?;
    } else {
        flate2::read::ZlibDecoder::new(&bytes[..]).read_to_end(&mut out)?;
    }
    Ok(out)
}

#[async_trait::async_trait]
impl TileSource for MbtilesSource {
    /// The tile at XYZ `tile`, decompressed if it is a gzipped vector tile.
    ///
    /// `Ok(None)` -- absence, never retried -- for a tile outside the file's
    /// zoom range or bounds (answered without touching SQLite) and for one
    /// the file simply does not contain. An `x`/`y` outside the `2^z` grid
    /// is an error, as for `PmtilesSource`: it is the caller's arithmetic,
    /// not the data, that is wrong.
    ///
    /// The SQLite read and the gunzip run on tokio's blocking pool; outside
    /// a tokio runtime they run inline.
    async fn fetch(&self, tile: TileId) -> anyhow::Result<Option<Vec<u8>>> {
        if tile.z > 30 || u64::from(tile.x) >= 1u64 << tile.z || u64::from(tile.y) >= 1u64 << tile.z {
            anyhow::bail!("tile {}/{}/{} is outside the 2^z grid", tile.z, tile.x, tile.y);
        }
        if !self.may_hold(tile) {
            return Ok(None);
        }
        let row = if self.tms { (1u32 << tile.z) - 1 - tile.y } else { tile.y };
        let pool = self.pool.clone();
        let vector = self.metadata.format.is_vector();
        let read = move || -> anyhow::Result<Option<Vec<u8>>> {
            match read_tile(&pool, tile.z, tile.x, row)? {
                Some(bytes) if vector => decompress(bytes).map(Some),
                other => Ok(other),
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => handle.spawn_blocking(read).await?,
            Err(_) => read(),
        }
    }

    /// The file's own `maxzoom` (or its deepest tile): above it the map
    /// overzooms instead of asking for tiles that cannot exist.
    fn max_zoom(&self) -> u8 {
        self.metadata.max_zoom
    }

    /// The `attribution` row, reduced to text. Reaches the map's credit line
    /// as the basemap (or, through `set_raster_source`, imagery) credit with
    /// no help from the caller.
    fn attribution(&self) -> Option<&str> {
        (!self.attribution.is_empty()).then_some(self.attribution.as_str())
    }

    fn layer_min_zoom(&self, source_layer: &str) -> Option<u8> {
        self.layer_min_zooms.get(source_layer).copied()
    }
}

#[cfg(test)]
mod tests;

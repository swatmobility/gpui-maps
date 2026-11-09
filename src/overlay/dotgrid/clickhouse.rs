//! Count tiles read live from ClickHouse: the loader's
//! `gpui_map_dots.<table>` (`tools/dotgrid-build`, spec §7.2) queried per
//! batch of up to eight data tiles, instead of a PMTiles file. The native
//! demo reaches it with `--features clickhouse-demo`; the browser through
//! [`ClickHouseProxySource`] and `web/serve.py`'s `/clickhouse/` proxy.
//!
//! **The proxy's copy.** `web/serve.py` builds the same SQL in Python from
//! validated parameters (the page never sends SQL). Its text is pinned to
//! [`ClickHouseGridSource::tile_query`]'s by `web/tests/fixtures/ch_tile_query.sql`:
//! the Rust test `web_proxy_template_fixture` fails when this file's
//! output drifts from the fixture, and serve.py's `test_proxy_template_matches_rust`
//! fails when the Python drifts from it. Change the template here, re-run
//! with `GPUI_MAP_WRITE_FIXTURES=1`, and change serve.py to match.
//!
//! Sans-IO like every [`GridCountSource`]: [`GridCountSource::request`] is
//! a [`GridRequest::Post`] of one SQL body, the shell executes it
//! (`fetch_native::execute`), and [`GridCountSource::decode`] splits the
//! RowBinary rows back into one tile per requested key. Only
//! [`ClickHouseGridSource::connect`] does IO itself (the descriptor's two
//! queries, blocking, bounded by [`CONNECT_TIMEOUT`]).
//!
//! Read-only: every statement here is a `SELECT` from `gpui_map_dots`
//! with its columns enumerated -- `all_queries_for_test` hands each one to
//! `sql_guard`, which fails on a star select or a foreign database. Every
//! value spliced into SQL is an engine-produced integer (`u16` epoch, `u8`
//! level, `u32` tile id) or a table name checked to be a bare identifier.
//!
//! A tile's primary-key range comes from the loader's `tile` column,
//! `(x << 16) | y` at the level's data zoom (`clickhouse_load.rs`), which
//! sits in the table's `ORDER BY (epoch, level, tile, row, col)`.
//!
//! No tile cache of its own: `DotLayers` already holds every fetched tile,
//! and the query's `use_query_cache = 1` answers a repeat on the server. A
//! cache split across `request` (skip the cached keys) and `decode` (fill
//! them back in) could be evicted between the two calls and turn a cached
//! tile into a silently empty one.

use std::time::Duration;

use super::grid::{self, CellWindow};
use super::{GridCountSource, GridDescriptor, GridError, GridRequest, GridTile, GridTileKey};
use crate::geo::TileId;

/// One deadline for both of [`ClickHouseGridSource::connect`]'s descriptor
/// queries together (the second gets what the first left): a server that
/// is down or does not answer in this time is an error, so the demo opens
/// without dots, at most this late, and says why instead of hanging.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Bytes of one RowBinary row: `k UInt32, deci UInt32` (MG-14 ruling:
/// 8 B per row, down from 16 B of `tile, row, col, pop`).
const ROW_BYTES: usize = 8;
/// A live batch's row (Task 15): `k UInt32, pop_a UInt32, pop_b UInt32`,
/// the two epochs' deci-counts.
const LIVE_ROW_BYTES: usize = 12;

/// `k`'s low bits: the cell's local index in its tile's window. 24 bits
/// hold the largest window of either grid (MG-1: 712,336 cells at 3″,
/// 454,950 at 30″); the [`SLOT_BITS`] above carry the tile's slot in the
/// batch (0..8 today). A deci-count is a `u32`: the measured maximum is
/// 254,444,880 (level 6, 30″), 5.9 % of `u32::MAX`.
const INDEX_BITS: u32 = 24;
const INDEX_MASK: u32 = (1 << INDEX_BITS) - 1;
/// `k`'s high bits: up to 256 tiles per batch.
const SLOT_BITS: u32 = 32 - INDEX_BITS;

/// Whether a tile response is gzip-compressed by default. Plain, by the
/// controller's ruling (progress.md, Task 11: "plain (uncompressed) is
/// the ClickHouse default and gzip is opt-in"): on localhost gzip costs
/// more than it saves -- MG-14's cold 200-batch pass at 8 B rows measured
/// p95 104.7 ms gzipped vs 35.5 ms plain, and a dense tile's server time
/// p50 26.8 ms vs 9.4 ms (gzip shrinks a row to 4.9-5.4 B). A remote
/// server over a slow link may want it back (`with_gzip(true)`); see
/// `docs/measurements/2026-09-26-world-dot-density.md`.
pub const GZIP_DEFAULT: bool = false;

/// The dot layer's credit (spec: alongside the basemap's, never instead).
const ATTRIBUTION: &str = "GHS-POP R2023A, European Commission JRC";

/// The tables the browser's proxy serves (`web/serve.py`'s whitelist, the
/// same two names): nothing else in `gpui_map_dots`, and nothing outside it.
pub const PROXY_TABLES: [&str; 2] = ["ghs_cells_30ss", "ghs_cells_3ss"];

/// A proxied table's grid: `(base_arcsec, origin_lat)` -- the table stores
/// cells, not these (see [`ClickHouseGridSource::connect`]). Both GHSL
/// grids are snapped to a 90° origin. `None` for a table the proxy does
/// not serve.
#[must_use]
pub fn table_grid(table: &str) -> Option<(f64, f64)> {
    match table {
        "ghs_cells_30ss" => Some((30.0, 90.0)),
        "ghs_cells_3ss" => Some((3.0, 90.0)),
        _ => None,
    }
}

/// The descriptor of a table holding `epochs` and levels `0..=max_level`
/// of a `base_arcsec` grid: what [`ClickHouseGridSource::connect`] reads
/// and what the proxy's `/clickhouse/descriptor` answers.
fn table_descriptor(table: &str, epochs: Vec<u16>, max_level: u8, base_arcsec: f64, origin_lat: f64) -> Result<GridDescriptor, GridError> {
    if epochs.is_empty() {
        return Err(GridError::Server(format!("gpui_map_dots.{table} has no epoch loaded")));
    }
    let max_data_zoom = grid::base_data_zoom(base_arcsec);
    if max_level > max_data_zoom {
        return Err(GridError::Server(format!(
            "gpui_map_dots.{table} has level {max_level}, coarser than data zoom 0 at {base_arcsec}″"
        )));
    }
    Ok(GridDescriptor {
        base_arcsec,
        origin_lat,
        origin_lon: -180.0,
        levels: max_level + 1,
        max_data_zoom,
        epochs,
        attribution: ATTRIBUTION.to_string(),
    })
}

/// The loader's tile key: `(x << 16) | y` (`tools/dotgrid-build`'s
/// `clickhouse_load.rs`). Data zooms stop at 9, so x and y fit 16 bits.
fn tile_id(t: TileId) -> u32 {
    (t.x << 16) | t.y
}

/// `table` if it is a bare identifier (it is spliced into SQL), else an error.
fn checked_table(table: &str) -> Result<&str, GridError> {
    if !table.is_empty() && table.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Ok(table)
    } else {
        Err(GridError::Server(format!("{table:?} is not a table name")))
    }
}

fn epochs_query(table: &str) -> String {
    format!("SELECT DISTINCT epoch FROM gpui_map_dots.{table} ORDER BY epoch FORMAT TabSeparated")
}

fn max_level_query(table: &str) -> String {
    format!("SELECT max(level) FROM gpui_map_dots.{table} FORMAT TabSeparated")
}

/// `url` with `wait_end_of_query=1`: ClickHouse then buffers the whole
/// result, so an exception mid-query arrives as an HTTP error with
/// `X-ClickHouse-Exception-Code`, never as a 200 whose body ends in
/// `Code: …` after some rows (the loader does the same).
pub(crate) fn endpoint(url: &str) -> String {
    if url.contains('?') {
        format!("{url}&wait_end_of_query=1")
    } else {
        format!("{}/?wait_end_of_query=1", url.trim_end_matches('/'))
    }
}

/// A body that is a ClickHouse exception, not data: an error, never rows.
pub(crate) fn exception_in(body: &[u8]) -> Option<GridError> {
    body.starts_with(b"Code: ").then(|| {
        let text = String::from_utf8_lossy(&body[..body.len().min(400)]);
        GridError::Server(text.trim().to_string())
    })
}

/// Count tiles from `gpui_map_dots.<table>` over ClickHouse's HTTP interface.
pub struct ClickHouseGridSource {
    url: String,
    table: String,
    descriptor: GridDescriptor,
    query_cache: bool,
    gzip: bool,
    /// Extra headers on every POST: a caller's auth
    /// (`X-ClickHouse-User` / `X-ClickHouse-Key`), never logged.
    headers: Vec<(String, String)>,
}

impl ClickHouseGridSource {
    /// Blocking: reads the descriptor (levels, epochs) with one enumerated query:
    /// SELECT DISTINCT epoch FROM gpui_map_dots.T  and  SELECT max(level) FROM gpui_map_dots.T
    ///
    /// `base_arcsec` and `origin_lat` are the loaded grid's (30.0 and 90.0
    /// for the snapped GHSL 30″ world); the table stores cells, not them.
    /// Unreachable, slow (both queries together over [`CONNECT_TIMEOUT`]),
    /// empty or missing: `Err`.
    pub fn connect(url: &str, table: &str, base_arcsec: f64, origin_lat: f64) -> Result<Self, GridError> {
        Self::connect_with_headers(url, Vec::new(), table, base_arcsec, origin_lat)
    }

    /// [`Self::connect`] with `headers` on every request, the descriptor's
    /// two queries included: how a host that keeps the credentials (a
    /// saved connection's user and password) hands them over without
    /// putting them in the URL.
    pub fn connect_with_headers(url: &str, headers: Vec<(String, String)>, table: &str, base_arcsec: f64, origin_lat: f64) -> Result<Self, GridError> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let table = checked_table(table)?;
            let started = crate::timing::Instant::now();
            let run = |sql: String| -> Result<String, GridError> {
                let left = CONNECT_TIMEOUT.saturating_sub(started.elapsed());
                if left.is_zero() {
                    return Err(GridError::Timeout(format!("the descriptor took over {CONNECT_TIMEOUT:?}")));
                }
                let req = GridRequest::Post { url: endpoint(url), body: sql, headers: headers.clone() };
                let body = super::fetch_native::execute_within(&req, left)?;
                if let Some(e) = exception_in(&body) {
                    return Err(e);
                }
                Ok(String::from_utf8_lossy(&body).into_owned())
            };
            let epochs = run(epochs_query(table))?
                .lines()
                .map(|l| l.trim().parse::<u16>().map_err(|_| GridError::Server(format!("not an epoch: {l:?}"))))
                .collect::<Result<Vec<u16>, GridError>>()?;
            let text = run(max_level_query(table))?;
            let max_level: u8 =
                text.trim().parse().map_err(|_| GridError::Server(format!("not a level: {:?}", text.trim())))?;
            let descriptor = table_descriptor(table, epochs, max_level, base_arcsec, origin_lat)?;
            Ok(Self::from_descriptor(url, table, descriptor)?.with_headers(headers))
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = (url, headers, table, base_arcsec, origin_lat);
            Err(GridError::Server("ClickHouseGridSource::connect is native-only".to_string()))
        }
    }

    /// A source over a descriptor already known (tests, and a shell that
    /// read the descriptor its own way). No IO. Refuses a descriptor
    /// [`GridDescriptor::validate`] refuses.
    pub fn from_descriptor(url: &str, table: &str, descriptor: GridDescriptor) -> Result<Self, GridError> {
        descriptor.validate()?;
        Ok(Self { url: url.to_string(), table: checked_table(table)?.to_string(), descriptor, query_cache: true, gzip: GZIP_DEFAULT, headers: Vec::new() })
    }

    /// The same source sending `headers` on every tile query too.
    #[must_use]
    pub fn with_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.headers = headers;
        self
    }

    /// The same source with `use_query_cache = 0`: every query reads the
    /// table (MG-14's cold pass). The demo never draws with this.
    #[must_use]
    pub fn without_query_cache(mut self) -> Self {
        self.query_cache = false;
        self
    }

    /// The same source with the response gzip-compressed (`true`) or
    /// sent plain (MG-14 measures both; [`GZIP_DEFAULT`] is the faster).
    #[must_use]
    pub fn with_gzip(mut self, gzip: bool) -> Self {
        self.gzip = gzip;
        self
    }

    /// The batch's distinct tiles, ascending: a row's slot is its tile's
    /// position here, so `ORDER BY tile, row, col` (the primary key's own
    /// order) is also the order of `k`.
    fn slots(&self, keys: &[GridTileKey]) -> Vec<(u32, CellWindow)> {
        let mut slots: Vec<(u32, CellWindow)> =
            keys.iter().map(|k| (tile_id(k.tile), grid::cell_window(&self.descriptor, k.level, k.tile))).collect();
        slots.sort_by_key(|s| s.0);
        slots.dedup_by_key(|s| s.0);
        slots
    }

    /// The SQL one batch sends: every key at one level and epoch. Each row
    /// comes back as `k = slot << 24 | local index` and the cell's
    /// deci-count `round(pop * 10)` (half away from zero, as
    /// `codec::encode` rounds), computed in SQL from the tiles' windows
    /// (`grid::cell_window`), which the engine splices in as integer
    /// arrays. A cell rounding to 0 is not sent (the archive drops it too).
    ///
    /// `k = row * cols + col + offset`, `offset = slot << 24 - row0 * cols -
    /// col0` per tile, by two `transform`s: measured at 20.4 ms server
    /// time for two dense tiles against 30.5 ms for `indexOf` into the
    /// arrays three times per row, and 19.8 ms for one tile's constants.
    pub fn tile_query(&self, keys: &[GridTileKey]) -> String {
        debug_assert!(
            keys.iter().all(|k| k.level == keys[0].level && k.epoch == keys[0].epoch && k.epoch_b == keys[0].epoch_b),
            "a ClickHouse batch is one level and one epoch"
        );
        let (epoch, level, epoch_b) = keys.first().map_or((0, 0, None), |k| (k.epoch, k.level, k.epoch_b));
        let slots = self.slots(keys);
        // A wider batch or window would wrap into the next slot's bits.
        debug_assert!(slots.len() <= 1 << SLOT_BITS, "{} tiles overflow {SLOT_BITS} slot bits", slots.len());
        debug_assert!(
            slots.iter().all(|(_, w)| u64::from(w.rows) * u64::from(w.cols) <= 1 << INDEX_BITS),
            "a tile window overflows {INDEX_BITS} index bits"
        );
        let list = |f: &dyn Fn(&(u32, CellWindow)) -> u32| {
            slots.iter().map(|s| format!("{}", f(s))).collect::<Vec<String>>().join(", ")
        };
        let offsets: Vec<String> = slots
            .iter()
            .enumerate()
            .map(|(slot, (_, w))| {
                format!("{}", ((slot as i64) << INDEX_BITS) - i64::from(w.row0) * i64::from(w.cols) - i64::from(w.col0))
            })
            .collect();
        let (tiles, cols) = (list(&|s| s.0), list(&|s| s.1.cols));
        let k = format!(
            "toUInt32(row * transform(tile, [{tiles}], [{cols}], 0) + col + transform(tile, [{tiles}], [{offsets}], 0)) AS k",
            offsets = offsets.join(", ")
        );
        let settings = format!(
            "SETTINGS use_query_cache = {}, enable_http_compression = {}",
            u8::from(self.query_cache),
            u8::from(self.gzip)
        );
        let table = &self.table;
        match epoch_b {
            None => format!(
                "SELECT {k},\n\
                 \x20      toUInt32(floor(pop * 10 + 0.5)) AS deci\n\
                 FROM gpui_map_dots.{table}\n\
                 WHERE epoch = {epoch} AND level = {level} AND tile IN ({tiles}) AND deci > 0\n\
                 ORDER BY tile, row, col\n\
                 FORMAT RowBinary\n\
                 {settings}"
            ),
            // Live (Task 15): both epochs' deci-counts per cell in one
            // pass over the primary key, `sumIf` per epoch (a cell missing
            // from one epoch sums to 0 there). A cell is sent when either
            // count is non-zero -- a town that appears only in epoch B
            // must have its slots.
            Some(b) => format!(
                "SELECT {k},\n\
                 \x20      toUInt32(floor(sumIf(pop, epoch = {epoch}) * 10 + 0.5)) AS pop_a,\n\
                 \x20      toUInt32(floor(sumIf(pop, epoch = {b}) * 10 + 0.5)) AS pop_b\n\
                 FROM gpui_map_dots.{table}\n\
                 WHERE epoch IN ({epoch}, {b}) AND level = {level} AND tile IN ({tiles})\n\
                 GROUP BY tile, row, col\n\
                 HAVING pop_a > 0 OR pop_b > 0\n\
                 ORDER BY tile, row, col\n\
                 FORMAT RowBinary\n\
                 {settings}"
            ),
        }
    }
}

impl GridCountSource for ClickHouseGridSource {
    fn descriptor(&self) -> &GridDescriptor {
        &self.descriptor
    }

    fn max_batch(&self) -> usize {
        8
    }

    /// One POST per batch. When compressed, `Accept-Encoding: gzip`, not
    /// zstd: the native executor's `reqwest` decodes gzip (its `gzip`
    /// feature) and has no zstd decoder compiled in. Plain is `identity`,
    /// said explicitly: `reqwest` would otherwise ask for gzip itself.
    fn request(&self, keys: &[GridTileKey]) -> GridRequest {
        // A part key (C7) has no cell window: no query for it; `decode`
        // refuses it (review F2).
        if keys.is_empty() || keys.iter().any(GridTileKey::is_part) {
            return GridRequest::Absent;
        }
        GridRequest::Post {
            url: endpoint(&self.url),
            body: self.tile_query(keys),
            headers: std::iter::once(("Accept-Encoding".to_string(), if self.gzip { "gzip" } else { "identity" }.to_string())).chain(self.headers.iter().cloned()).collect(),
        }
    }

    /// RowBinary `(k u32, deci u32)` rows -- `(k u32, pop_a u32, pop_b
    /// u32)` for a live batch (`epoch_b`: both epochs' deci-counts, 12 B)
    /// -- in tile then row then col order, split back into one tile per
    /// key (empty when the server returned no rows for it). An exception
    /// body, a partial row, a slot beyond the batch, an index outside its
    /// tile's window or a zero count (live: both zero) is an error for the
    /// whole batch -- never a tile of garbage dots. (A `Code: ` body cannot
    /// be mistaken for rows: its first four bytes read as slot 101.)
    ///
    /// Without `epoch_b`, `pop_b == pop`. `GridTile::total` is epoch A's.
    fn decode(&self, keys: &[GridTileKey], body: &[u8]) -> Result<Vec<(GridTileKey, GridTile)>, GridError> {
        if keys.iter().any(GridTileKey::is_part) {
            return Err(part_keys_refused());
        }
        if let Some(e) = exception_in(body) {
            return Err(e);
        }
        let live = keys.first().is_some_and(|k| k.epoch_b.is_some());
        let row_bytes = if live { LIVE_ROW_BYTES } else { ROW_BYTES };
        if !body.len().is_multiple_of(row_bytes) {
            return Err(GridError::Truncated);
        }
        let slots = self.slots(keys);
        debug_assert_eq!(slots.len(), keys.len(), "a batch's keys are distinct");
        let mut tiles = vec![GridTile::default(); slots.len()];
        let u32_at = |row: &[u8], at: usize| u32::from_le_bytes([row[at], row[at + 1], row[at + 2], row[at + 3]]);
        for row in body.chunks_exact(row_bytes) {
            let (k, deci) = (u32_at(row, 0), u32_at(row, 4));
            let deci_b = if live { u32_at(row, 8) } else { deci };
            let (slot, idx) = ((k >> INDEX_BITS) as usize, k & INDEX_MASK);
            let Some((tile, w)) = slots.get(slot) else {
                return Err(GridError::Server(format!("a row for slot {slot} of a {}-tile batch", slots.len())));
            };
            let cells = w.rows * w.cols;
            if idx >= cells {
                return Err(GridError::IndexOutOfTile { idx, cells });
            }
            if deci == 0 && deci_b == 0 {
                return Err(GridError::Server(format!("cell {idx} of tile {tile:#x} has a zero count")));
            }
            let pop = (f64::from(deci) / 10.0) as f32;
            let pop_b = (f64::from(deci_b) / 10.0) as f32;
            let out = &mut tiles[slot];
            out.cells.push((idx, pop, pop_b));
            out.total += f64::from(deci) / 10.0;
        }
        Ok(keys
            .iter()
            .map(|k| {
                // Keys are distinct, so each slot's tile is taken once.
                let slot = slots.binary_search_by_key(&tile_id(k.tile), |s| s.0).expect("every key has a slot");
                (*k, std::mem::take(&mut tiles[slot]))
            })
            .collect())
    }
}

/// Count tiles through the page's own server (Task 16, the browser): a
/// [`GridRequest::Get`] of `<base>/tile?table=&epoch=&level=&tiles=`,
/// which `web/serve.py` validates, turns into [`ClickHouseGridSource::tile_query`]'s
/// SQL (its Python copy, pinned by `web/tests/fixtures/ch_tile_query.sql`)
/// and runs with `readonly=1`. The page never holds SQL or the ClickHouse
/// URL. The rows are the single-epoch 8 B rows, decoded here exactly as a
/// direct source decodes them.
///
/// One epoch per request: a key with `epoch_b` (Task 15's live pair) is
/// refused at decode, never drawn as a pair it is not.
pub struct ClickHouseProxySource {
    base: String,
    inner: ClickHouseGridSource,
}

impl ClickHouseProxySource {
    /// A source over `<base>/tile` for `table`, from the proxy's
    /// `<base>/descriptor?table=` reply: `{"epochs": [..], "max_level": n}`.
    /// A table the proxy does not serve, or a reply that is not that
    /// shape, is an error.
    pub fn from_descriptor_reply(base: &str, table: &str, reply: &[u8]) -> Result<Self, GridError> {
        let (base_arcsec, origin_lat) =
            table_grid(table).ok_or_else(|| GridError::Server(format!("{table:?} is not a table the proxy serves")))?;
        let bad = |why: &str| GridError::Server(format!("the proxy's descriptor reply {why}"));
        let value: serde_json::Value = serde_json::from_slice(reply).map_err(|_| bad("is not JSON"))?;
        let epochs = value
            .get("epochs")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| bad("has no epochs"))?
            .iter()
            .map(|e| e.as_u64().and_then(|e| u16::try_from(e).ok()).ok_or_else(|| bad("has an epoch that is not a year")))
            .collect::<Result<Vec<u16>, GridError>>()?;
        let max_level = value
            .get("max_level")
            .and_then(serde_json::Value::as_u64)
            .and_then(|l| u8::try_from(l).ok())
            .ok_or_else(|| bad("has no max_level"))?;
        let descriptor = table_descriptor(table, epochs, max_level, base_arcsec, origin_lat)?;
        Self::from_descriptor(base, table, descriptor)
    }

    /// A source over a known descriptor (tests). `table` must be one the
    /// proxy serves.
    pub fn from_descriptor(base: &str, table: &str, descriptor: GridDescriptor) -> Result<Self, GridError> {
        if table_grid(table).is_none() {
            return Err(GridError::Server(format!("{table:?} is not a table the proxy serves")));
        }
        Ok(Self { base: base.trim_end_matches('/').to_string(), inner: ClickHouseGridSource::from_descriptor("", table, descriptor)? })
    }

    /// The proxy's descriptor URL for `table` under `base`.
    #[must_use]
    pub fn descriptor_url(base: &str, table: &str) -> String {
        format!("{}/descriptor?table={table}", base.trim_end_matches('/'))
    }

    /// The SQL the proxy runs for `keys`: the direct source's, at its
    /// defaults (query cache on, plain). What the fixture pins.
    #[must_use]
    pub fn proxied_sql(&self, keys: &[GridTileKey]) -> String {
        self.inner.tile_query(keys)
    }

    /// The `tiles=` list: the batch's loader tile ids, ascending and
    /// distinct -- the slot order `decode` reads `k`'s high bits by.
    fn tile_ids(&self, keys: &[GridTileKey]) -> String {
        self.inner.slots(keys).iter().map(|(id, _)| id.to_string()).collect::<Vec<String>>().join(",")
    }
}

impl GridCountSource for ClickHouseProxySource {
    fn descriptor(&self) -> &GridDescriptor {
        self.inner.descriptor()
    }

    /// The proxy's own limit (`web/serve.py`: at most 8 tiles).
    fn max_batch(&self) -> usize {
        self.inner.max_batch()
    }

    fn request(&self, keys: &[GridTileKey]) -> GridRequest {
        let Some(first) = keys.first() else { return GridRequest::Absent };
        if keys.iter().any(|k| k.epoch_b.is_some()) {
            // `decode` refuses a live (two-epoch) batch outright -- see
            // its own `Err` below -- so building the GET here would only
            // spend a real ClickHouse query on a reply this source is
            // certain to reject. `Absent` costs no IO
            // (`fetch_native::execute` answers it with an empty body with
            // no network call) and still reaches that same `decode` error
            // on the empty body, so the caller sees the same message
            // without the round trip.
            return GridRequest::Absent;
        }
        if keys.iter().any(GridTileKey::is_part) {
            // Refused by `decode`, as a live batch is (review F2).
            return GridRequest::Absent;
        }
        GridRequest::Get {
            url: format!(
                "{}/tile?table={}&epoch={}&level={}&tiles={}",
                self.base,
                self.inner.table,
                first.epoch,
                first.level,
                self.tile_ids(keys)
            ),
        }
    }

    fn decode(&self, keys: &[GridTileKey], body: &[u8]) -> Result<Vec<(GridTileKey, GridTile)>, GridError> {
        if keys.iter().any(|k| k.epoch_b.is_some()) {
            return Err(GridError::Server("the ClickHouse proxy serves one epoch per request".to_string()));
        }
        if keys.iter().any(GridTileKey::is_part) {
            return Err(part_keys_refused());
        }
        self.inner.decode(keys, body)
    }
}

/// A ClickHouse source serves cell tiles only: a block-part key (C7,
/// `GridTileKey::PART_LEVEL`) is an error, never a cell window computed
/// from level 255 (review F2).
fn part_keys_refused() -> GridError {
    GridError::Server("a ClickHouse dot source serves no block-part tiles".to_string())
}

/// Every SQL template this module sends, filled with sample values, for
/// the `sql_guard` tests.
#[doc(hidden)]
pub fn all_queries_for_test() -> Vec<String> {
    let table = "ghs_cells_30ss";
    let descriptor = GridDescriptor {
        base_arcsec: 30.0,
        origin_lat: 90.0,
        origin_lon: -180.0,
        levels: 7,
        max_data_zoom: 6,
        epochs: vec![2025],
        attribution: ATTRIBUTION.to_string(),
    };
    let source = ClickHouseGridSource::from_descriptor("http://localhost:8123", table, descriptor)
        .expect("a bare table name");
    let keys: Vec<GridTileKey> = (0..8)
        .map(|x| GridTileKey { tile: TileId { z: 6, x: 44 + x, y: 27 }, level: 0, epoch: 2025, epoch_b: None })
        .collect();
    let cold = ClickHouseGridSource { query_cache: false, ..source };
    let plain = ClickHouseGridSource { gzip: !GZIP_DEFAULT, ..cold };
    let live: Vec<GridTileKey> = keys.iter().map(|k| GridTileKey { epoch: 1975, epoch_b: Some(2030), ..*k }).collect();
    vec![
        epochs_query(table),
        max_level_query(table),
        plain.tile_query(&keys),
        plain.tile_query(&keys[..1]),
        plain.tile_query(&live),
        plain.tile_query(&live[..1]),
    ]
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::overlay::dotgrid::layer::{DotLayers, DotStyle};
    use crate::overlay::dotgrid::sql_guard::{names_foreign_database, star_select_in, DOTS_DB};

    fn d30() -> GridDescriptor {
        GridDescriptor {
            base_arcsec: 30.0,
            origin_lat: 90.0,
            origin_lon: -180.0,
            levels: 7,
            max_data_zoom: 6,
            epochs: vec![2025],
            attribution: ATTRIBUTION.to_string(),
        }
    }

    /// Final review I3: `connect` ends in `from_descriptor`, which refuses
    /// a descriptor the grid math cannot use.
    #[test]
    fn a_source_over_an_unusable_descriptor_is_refused() {
        let zero = GridDescriptor { levels: 0, ..d30() };
        assert!(matches!(ClickHouseGridSource::from_descriptor("http://localhost:8123", "ghs_cells_30ss", zero), Err(GridError::Server(_))));
        let deep = GridDescriptor { levels: 8, ..d30() };
        assert!(ClickHouseGridSource::from_descriptor("http://localhost:8123", "ghs_cells_30ss", deep).is_err());
    }

    fn source() -> ClickHouseGridSource {
        ClickHouseGridSource::from_descriptor("http://localhost:8123", "ghs_cells_30ss", d30()).unwrap()
    }

    fn key(x: u32, y: u32, level: u8) -> GridTileKey {
        GridTileKey { tile: TileId { z: 6 - level, x, y }, level, epoch: 2025, epoch_b: None }
    }

    /// One wire row: `slot << 24 | idx`, then the deci-count.
    fn row(slot: u32, idx: u32, deci: u32) -> Vec<u8> {
        [((slot << INDEX_BITS) | idx).to_le_bytes(), deci.to_le_bytes()].concat()
    }

    #[test]
    fn no_adapter_query_selects_star() {
        for q in all_queries_for_test() {
            assert!(!star_select_in(&q), "{q}");
        }
    }

    #[test]
    fn every_adapter_query_reads_only_gpui_map_dots() {
        for q in all_queries_for_test() {
            assert!(!names_foreign_database(&q, DOTS_DB), "{q}");
        }
    }

    #[test]
    fn a_tile_query_packs_slot_and_local_index_from_the_loaders_tile_ids() {
        let s = source();
        // Out of order on purpose: slots follow ascending tile id.
        let (a, b) = (key(48, 27, 0), key(47, 27, 0));
        let (wa, wb) = (grid::cell_window(&d30(), 0, a.tile), grid::cell_window(&d30(), 0, b.tile));
        let (ia, ib) = ((48 << 16) | 27, (47 << 16) | 27);
        let q = s.tile_query(&[a, b]);
        assert_eq!(
            q,
            format!(
                "SELECT toUInt32(row * transform(tile, [{ib}, {ia}], [{}, {}], 0) + col + transform(tile, [{ib}, {ia}], [{}, {}], 0)) AS k,\n\
                 \x20      toUInt32(floor(pop * 10 + 0.5)) AS deci\n\
                 FROM gpui_map_dots.ghs_cells_30ss\n\
                 WHERE epoch = 2025 AND level = 0 AND tile IN ({ib}, {ia}) AND deci > 0\n\
                 ORDER BY tile, row, col\nFORMAT RowBinary\nSETTINGS use_query_cache = 1, enable_http_compression = {}",
                wb.cols,
                wa.cols,
                -i64::from(wb.row0) * i64::from(wb.cols) - i64::from(wb.col0),
                (1i64 << 24) - i64::from(wa.row0) * i64::from(wa.cols) - i64::from(wa.col0),
                u8::from(GZIP_DEFAULT)
            )
        );
        match s.request(&[a]) {
            GridRequest::Post { url, headers, .. } => {
                assert_eq!(url, "http://localhost:8123/?wait_end_of_query=1");
                let enc = if GZIP_DEFAULT { "gzip" } else { "identity" };
                assert_eq!(headers, vec![("Accept-Encoding".to_string(), enc.to_string())]);
            }
            other => panic!("expected a POST, got {other:?}"),
        }
        match source().with_gzip(false).request(&[a]) {
            GridRequest::Post { body, headers, .. } => {
                assert!(body.ends_with("enable_http_compression = 0"), "{body}");
                assert_eq!(headers[0].1, "identity", "reqwest would ask for gzip itself");
            }
            other => panic!("expected a POST, got {other:?}"),
        }
        // A host's auth headers ride on every tile query, after the encoding.
        let auth = vec![("X-ClickHouse-User".to_string(), "reader".to_string())];
        match source().with_headers(auth.clone()).request(&[a]) {
            GridRequest::Post { headers, .. } => assert_eq!(&headers[1..], &auth[..]),
            other => panic!("expected a POST, got {other:?}"),
        }
        assert!(ClickHouseGridSource::from_descriptor("http://h", "t; DROP TABLE x", d30()).is_err());
    }

    fn live_key(x: u32, y: u32, level: u8) -> GridTileKey {
        GridTileKey { epoch: 1975, epoch_b: Some(2030), ..key(x, y, level) }
    }

    /// One live wire row: `k`, epoch A's deci-count, epoch B's.
    fn live_row(slot: u32, idx: u32, deci_a: u32, deci_b: u32) -> Vec<u8> {
        [((slot << INDEX_BITS) | idx).to_le_bytes(), deci_a.to_le_bytes(), deci_b.to_le_bytes()].concat()
    }

    /// Task 15: a key with `epoch_b` asks for both epochs in one pass,
    /// every column named (the brief's `sumIf` per epoch), one level, and
    /// keeps a cell either epoch populates.
    #[test]
    fn the_live_query_enumerates_columns_and_both_epochs() {
        let s = source();
        let q = s.tile_query(&[live_key(47, 27, 0), live_key(48, 27, 0)]);
        assert!(!star_select_in(&q), "{q}");
        assert!(!names_foreign_database(&q, DOTS_DB), "{q}");
        assert!(q.contains("sumIf(pop, epoch = 1975)") && q.contains("sumIf(pop, epoch = 2030)"), "{q}");
        assert!(q.contains("AS pop_a") && q.contains("AS pop_b"), "{q}");
        assert!(q.contains("WHERE epoch IN (1975, 2030) AND level = 0 AND tile IN ("), "{q}");
        assert!(q.contains("GROUP BY tile, row, col\n"), "{q}");
        assert!(q.contains("HAVING pop_a > 0 OR pop_b > 0"), "{q}");
        assert!(q.contains("ORDER BY tile, row, col\nFORMAT RowBinary"), "{q}");
        // The slot/index packing is the single-epoch query's, unchanged.
        let single = s.tile_query(&[key(47, 27, 0), key(48, 27, 0)]);
        let k = |q: &str| q.split(" AS k").next().unwrap().to_string();
        assert_eq!(k(&q), k(&single));
        assert!(!single.contains("sumIf"), "a key without epoch_b is the 8-byte query: {single}");
    }

    #[test]
    fn decode_fills_pop_b() {
        let s = source();
        let keys = [live_key(44, 27, 0), live_key(45, 27, 0)];
        let body = [
            live_row(0, 3, 50, 120),
            // Only in epoch B (a new town) and only in epoch A.
            live_row(0, 7, 0, 15),
            live_row(1, 2, 40, 0),
        ]
        .concat();
        let tiles = s.decode(&keys, &body).unwrap();
        assert_eq!(tiles[0].1.cells, vec![(3, 5.0, 12.0), (7, 0.0, 1.5)]);
        assert_eq!(tiles[0].1.total, 5.0, "the total is epoch A's");
        assert_eq!(tiles[1].1.cells, vec![(2, 4.0, 0.0)]);
        // A live row is 12 bytes: 8 of them is a truncated body.
        assert_eq!(s.decode(&keys, &body[..8]), Err(GridError::Truncated));
        // Both counts zero is not a populated cell.
        assert!(matches!(s.decode(&keys, &live_row(0, 1, 0, 0)), Err(GridError::Server(_))));
        // Without epoch_b the 8-byte rows still decode with pop_b == pop.
        let plain = s.decode(&[key(44, 27, 0)], &row(0, 3, 50)).unwrap();
        assert_eq!(plain[0].1.cells, vec![(3, 5.0, 5.0)]);
    }

    #[test]
    fn a_batch_of_eight_splits_back_by_tile() {
        let s = source();
        // Requested in reverse: slots are by ascending tile id, results in request order.
        let keys: Vec<GridTileKey> = (0..8).rev().map(|i| key(44 + i, 27, 0)).collect();
        let slot_of = |x: u32| x - 44; // tile ids ascend with x at a fixed y
        let w = |x: u32| grid::cell_window(&d30(), 0, TileId { z: 6, x, y: 27 });
        let (w45, w48, w51) = (w(45), w(48), w(51));
        let body = [
            row(slot_of(45), 0, 50),
            row(slot_of(45), 2 * w45.cols + 3, 15),
            row(slot_of(48), w48.rows * w48.cols - 1, 73),
            row(slot_of(51), 10 * w51.cols, 1000),
        ]
        .concat();

        let tiles = s.decode(&keys, &body).unwrap();
        assert_eq!(tiles.len(), 8, "one tile per requested key");
        assert_eq!(tiles.iter().map(|(k, _)| *k).collect::<Vec<_>>(), keys, "in request order");
        let by_x = |x: u32| &tiles.iter().find(|(k, _)| k.tile.x == x).unwrap().1;
        for x in [44, 46, 47, 49, 50] {
            assert_eq!(*by_x(x), GridTile::default(), "tile {x} had no rows");
        }
        assert_eq!(by_x(45).cells, vec![(0, 5.0, 5.0), (2 * w45.cols + 3, 1.5, 1.5)]);
        assert_eq!(by_x(45).total, 6.5);
        assert_eq!(by_x(48).cells, vec![(w48.rows * w48.cols - 1, 7.3, 7.3)]);
        assert_eq!(by_x(51).cells, vec![(10 * w51.cols, 100.0, 100.0)]);
        assert_eq!(by_x(51).total, 100.0);
    }

    #[test]
    fn a_row_outside_the_batch_or_its_window_is_an_error() {
        let s = source();
        let keys = [key(44, 27, 0), key(45, 27, 0)];
        let w = grid::cell_window(&d30(), 0, keys[0].tile);
        assert!(matches!(s.decode(&keys, &row(2, 0, 1)), Err(GridError::Server(_))), "slot 2 of a 2-tile batch");
        assert!(matches!(s.decode(&keys, &row(0, w.rows * w.cols, 1)), Err(GridError::IndexOutOfTile { .. })));
        assert!(matches!(s.decode(&keys, &row(0, 0, 0)), Err(GridError::Server(_))), "a zero count");
    }

    /// The layer's planner (`DotLayers::fetch_batch`) never hands this
    /// source a mixed batch: a zoom whose plan wants level-1 parents and
    /// level-0 cover tiles is fetched as single-level batches of at most
    /// eight. (`tile_query` debug-asserts it too -- below.)
    #[test]
    fn a_batch_never_mixes_levels() {
        let s: Arc<dyn GridCountSource> = Arc::new(source());
        let mut layers = DotLayers::new();
        layers.set_source("pop", Arc::clone(&s), DotStyle::default());
        // Zoom 5.5 on 30″ is level 0 (data z6); its cover's 16 tiles want
        // their 4 level-1 parents first.
        let cover: Vec<TileId> =
            (0..4).flat_map(|x| (0..4).map(move |y| TileId { z: 6, x: 44 + x, y: 26 + y })).collect();
        layers.frame_with_cover(5.5, |_| cover.clone(), ([0.60, 0.40], [0.66, 0.46]), crate::timing::Instant::now());
        let mut levels_seen = Vec::new();
        let mut batches = 0;
        let mut jobs = Vec::new();
        while let Some(job) = layers.next_job() {
            jobs.push(job);
        }
        for job in &jobs {
            let Some(GridRequest::Post { body, .. }) = job.request() else { panic!("a fetch is a POST") };
            let level_terms: Vec<&str> = body.split(" AND ").filter(|t| t.starts_with("level = ")).collect();
            assert_eq!(level_terms.len(), 1, "{body}");
            let n = body.split("tile IN (").nth(1).unwrap().split(')').next().unwrap().split(", ").count();
            assert!(n <= 8, "{n} keys in one batch");
            levels_seen.push(level_terms[0].to_string());
            batches += 1;
        }
        assert!(batches >= 2, "parents and cover come as separate batches: {levels_seen:?}");
        assert!(levels_seen.contains(&"level = 0".to_string()) && levels_seen.contains(&"level = 1".to_string()));
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "one level and one epoch")]
    fn a_mixed_level_request_panics_in_debug() {
        let _ = source().request(&[key(44, 27, 0), key(22, 13, 1)]);
    }

    #[test]
    fn a_clickhouse_exception_body_is_an_error() {
        let body = b"Code: 60. DB::Exception: Table gpui_map_dots.ghs_cells_3ss doesn't exist. (UNKNOWN_TABLE) (version 26.2.1.587)\n";
        match source().decode(&[key(44, 27, 0)], body) {
            Err(GridError::Server(text)) => assert!(text.starts_with("Code: 60."), "{text}"),
            other => panic!("expected GridError::Server, got {other:?}"),
        }
    }

    #[test]
    fn a_truncated_row_binary_body_is_an_error() {
        // The brief's fixture said 13 of 14 bytes; a row here is 8
        // (`k` and `deci`, both UInt32), so 7 of 8.
        let keys = [key(44, 27, 0)];
        let whole = row(0, 0, 30);
        assert!(source().decode(&keys, &whole).is_ok());
        let two = [whole.clone(), row(0, 1, 30)].concat();
        assert_eq!(source().decode(&keys, &two[..ROW_BYTES + 7]), Err(GridError::Truncated));
        assert_eq!(source().decode(&keys, &whole[..7]), Err(GridError::Truncated));
    }

    /// Review focus 5 (the library half; the demo's `choose_source` test
    /// is the other): nothing listens on port 1, and `connect` says so
    /// well inside two seconds instead of hanging.
    #[test]
    fn an_unreachable_server_surfaces_an_error_and_no_dots() {
        let started = crate::timing::Instant::now();
        let result = ClickHouseGridSource::connect("http://127.0.0.1:1", "ghs_cells_30ss", 30.0, 90.0);
        assert!(result.is_err(), "nothing listens on port 1");
        assert!(started.elapsed() < Duration::from_secs(2), "took {:?}", started.elapsed());
    }

    /// Task 15, live: a pair key's tile carries, cell for cell, what each
    /// epoch's own single-epoch query returns (a cell missing from one
    /// epoch reads 0 there). Needs 1975 and 2030 loaded.
    #[test]
    #[ignore = "needs the local ClickHouse with epochs 1975 and 2030 loaded"]
    fn a_live_pair_tile_matches_each_epoch_alone() {
        let url = std::env::var("CLICKHOUSE_URL").unwrap_or_else(|_| "http://localhost:8123".into());
        let ch = ClickHouseGridSource::connect(&url, "ghs_cells_30ss", 30.0, 90.0).expect("the local ClickHouse");
        assert!(ch.descriptor().epochs.contains(&1975) && ch.descriptor().epochs.contains(&2030), "{:?}", ch.descriptor().epochs);
        // Delhi (77.21 E, 28.61 N) and its east neighbour at data z6.
        let delhi = grid::tile_of_cell(ch.descriptor(), 0, ((90.0 - 28.61) * 120.0) as u32, ((77.21 + 180.0) * 120.0) as u32).unwrap();
        let tiles = [delhi, TileId { x: delhi.x + 1, ..delhi }];
        let fetch = |epoch: u16, epoch_b: Option<u16>| {
            let keys: Vec<GridTileKey> = tiles.iter().map(|&tile| GridTileKey { tile, level: 0, epoch, epoch_b }).collect();
            let body = crate::overlay::dotgrid::fetch_native::execute(&ch.request(&keys)).unwrap();
            ch.decode(&keys, &body).unwrap()
        };
        let (pair, a, b) = (fetch(1975, Some(2030)), fetch(1975, None), fetch(2030, None));
        for i in 0..tiles.len() {
            let (tile, a, b) = (&pair[i].1, &a[i].1, &b[i].1);
            let by = |t: &GridTile| t.cells.iter().map(|&(i, p, _)| (i, p)).collect::<std::collections::HashMap<u32, f32>>();
            let (ma, mb) = (by(a), by(b));
            for &(idx, pa, pb) in &tile.cells {
                assert_eq!(pa, ma.get(&idx).copied().unwrap_or(0.0), "cell {idx}: 1975");
                assert_eq!(pb, mb.get(&idx).copied().unwrap_or(0.0), "cell {idx}: 2030");
            }
            let union: std::collections::HashSet<u32> = ma.keys().chain(mb.keys()).copied().collect();
            assert_eq!(tile.cells.len(), union.len(), "every cell either epoch populates, once");
            assert_eq!(tile.total, a.total, "the total is 1975's");
            let total_b: f64 = tile.cells.iter().map(|c| f64::from(c.2)).sum();
            println!("{:?}: {} cells, 1975 {:.0} people, 2030 {total_b:.0} (alone {:.0})", pair[i].0.tile, tile.cells.len(), tile.total, b.total);
        }
    }

    /// A key read from ClickHouse and from the 30″ archive: the same cells
    /// (the archive rounds to 0.1 people and drops cells that round to 0).
    #[test]
    #[ignore = "needs the local ClickHouse with gpui_map_dots loaded"]
    fn a_live_dhaka_tile_matches_the_archive_tile() {
        let url = std::env::var("CLICKHOUSE_URL").unwrap_or_else(|_| "http://localhost:8123".into());
        let ch = ClickHouseGridSource::connect(&url, "ghs_cells_30ss", 30.0, 90.0).expect("the local ClickHouse");
        let home = std::env::var("HOME").unwrap();
        let archive = crate::overlay::dotgrid::fetch_native::open_pmtiles_grid(&format!(
            "{home}/.cache/gpui-map-data/ghsl/E2025_30ss.pmtiles"
        ))
        .expect("the 30″ archive");
        assert_eq!(ch.descriptor().levels, archive.descriptor().levels);
        // The table holds every loaded epoch (Task 15 loads 1975..2030);
        // the archive, 2025 alone.
        assert!(archive.descriptor().epochs.iter().all(|e| ch.descriptor().epochs.contains(e)), "{:?}", ch.descriptor().epochs);
        // Dhaka (90.41 E, 23.81 N) at data z6 and its level-1 parent: two
        // batches, so the batch path runs too (Dhaka's tile with its east
        // neighbour).
        let dhaka = grid::tile_of_cell(ch.descriptor(), 0, (66.19 * 120.0) as u32, (270.41 * 120.0) as u32).unwrap();
        let east = TileId { x: dhaka.x + 1, ..dhaka };
        let k = |t: TileId, level: u8| GridTileKey { tile: t, level, epoch: 2025, epoch_b: None };
        for keys in [vec![k(dhaka, 0), k(east, 0)], vec![k(TileId { z: 5, x: dhaka.x / 2, y: dhaka.y / 2 }, 1)]] {
            let body = crate::overlay::dotgrid::fetch_native::execute(&ch.request(&keys)).unwrap();
            let live = ch.decode(&keys, &body).unwrap();
            for (key, tile) in live {
                let body = crate::overlay::dotgrid::fetch_native::execute(&archive.request(&[key])).unwrap();
                let file = archive.decode(&[key], &body).unwrap().remove(0).1;
                assert!(!tile.cells.is_empty(), "{key:?} is populated");
                let file_cells: std::collections::HashMap<u32, f32> = file.cells.iter().map(|&(i, p, _)| (i, p)).collect();
                for &(i, p, _) in &tile.cells {
                    let f = file_cells.get(&i).copied().unwrap_or(0.0);
                    // Both sides are the same deci-count / 10 as f32: exact.
                    assert_eq!(p, f, "{key:?} cell {i}: live {p}, archive {f}");
                }
                let live_idx: std::collections::HashSet<u32> = tile.cells.iter().map(|c| c.0).collect();
                let missing = file.cells.iter().filter(|c| !live_idx.contains(&c.0)).count();
                let extra = tile.cells.len() + missing - file.cells.len();
                println!("{key:?}: archive cells missing live {missing}, live cells not in the archive {extra}");
                assert_eq!((missing, extra), (0, 0), "{key:?}: the same cells on both sides");
                // The same deci-counts summed in the same (index) order.
                assert_eq!(tile.total, file.total, "{key:?}: totals");
                let rel = (tile.total - file.total).abs() / file.total;
                println!("{key:?}: {} cells, total {:.0} (archive {:.0}, rel {rel:.2e})", tile.cells.len(), tile.total, file.total);
            }
        }
    }

    fn proxy() -> ClickHouseProxySource {
        ClickHouseProxySource::from_descriptor("./clickhouse/", "ghs_cells_30ss", d30()).unwrap()
    }

    /// Task 16: the page asks the proxy by parameters, never SQL -- the
    /// batch's tile ids ascending, in the slot order `decode` reads.
    #[test]
    fn a_proxy_request_is_parameters_not_sql() {
        let keys = [key(45, 27, 0), key(44, 27, 0), key(45, 27, 0)];
        let GridRequest::Get { url } = proxy().request(&keys[..2]) else { panic!("a GET") };
        let (a, b) = ((44u32 << 16) | 27, (45u32 << 16) | 27);
        assert_eq!(url, format!("./clickhouse/tile?table=ghs_cells_30ss&epoch=2025&level=0&tiles={a},{b}"));
        assert!(!url.to_ascii_uppercase().contains("SELECT"), "{url}");
        assert_eq!(proxy().request(&[]), GridRequest::Absent);
        assert_eq!(proxy().max_batch(), 8, "serve.py refuses a ninth tile");
    }

    /// M-4: a live (two-epoch) batch is refused up front, not sent as a
    /// doomed single-epoch GET. `request` must not even try to build a
    /// URL for it -- `decode` already refuses these (see
    /// `a_proxy_reply_decodes_as_the_direct_sources`), and issuing the GET
    /// anyway would spend a real ClickHouse query on a reply this source
    /// is certain to reject.
    #[test]
    fn a_live_batch_is_refused_before_any_request_is_built() {
        let keys = [key(44, 27, 0), key(45, 27, 0)];
        let live: Vec<GridTileKey> = keys.iter().map(|k| GridTileKey { epoch_b: Some(2030), ..*k }).collect();
        assert_eq!(proxy().request(&live), GridRequest::Absent, "no GET for a live batch");
        // `Absent` costs no IO and still lands on the same refusal `decode` gives a live batch.
        let body = crate::overlay::dotgrid::fetch_native::execute(&proxy().request(&live)).unwrap();
        assert!(body.is_empty());
        assert!(proxy().decode(&live, &body).is_err(), "one epoch per request");
    }

    #[test]
    fn a_proxy_reply_decodes_as_the_direct_sources() {
        let keys = [key(44, 27, 0), key(45, 27, 0)];
        let body = [row(0, 3, 25), row(1, 7, 10)].concat();
        assert_eq!(proxy().decode(&keys, &body).unwrap(), source().decode(&keys, &body).unwrap());
        let live: Vec<GridTileKey> = keys.iter().map(|k| GridTileKey { epoch_b: Some(2030), ..*k }).collect();
        assert!(proxy().decode(&live, &body).is_err(), "one epoch per request");
    }

    #[test]
    fn the_proxy_serves_two_tables_and_reads_their_descriptor() {
        assert!(ClickHouseProxySource::from_descriptor("./clickhouse", "system_users", d30()).is_err());
        let s = ClickHouseProxySource::from_descriptor_reply("./clickhouse", "ghs_cells_30ss", br#"{"epochs": [1975, 2025], "max_level": 6}"#).unwrap();
        assert_eq!(s.descriptor(), &GridDescriptor { epochs: vec![1975, 2025], ..d30() });
        let fine = ClickHouseProxySource::from_descriptor_reply("./clickhouse", "ghs_cells_3ss", br#"{"epochs": [2025], "max_level": 9}"#).unwrap();
        assert_eq!((fine.descriptor().base_arcsec, fine.descriptor().levels, fine.descriptor().max_data_zoom), (3.0, 10, 9));
        for bad in [&b"not json"[..], br#"{"epochs": [], "max_level": 6}"#, br#"{"epochs": [2025], "max_level": 7}"#, br#"{"epochs": [70000], "max_level": 1}"#, br#"{"max_level": 1}"#] {
            assert!(ClickHouseProxySource::from_descriptor_reply("./clickhouse", "ghs_cells_30ss", bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
        assert_eq!(ClickHouseProxySource::descriptor_url("./clickhouse/", "ghs_cells_30ss"), "./clickhouse/descriptor?table=ghs_cells_30ss");
    }

    /// The cases `web/tests/fixtures/ch_tile_query.sql` pins: each case's
    /// header names the proxy's parameters, and its body is the SQL this
    /// file builds for them -- what `web/serve.py` must build too. Level 0
    /// and coarser levels, both tables, both hemispheres, the antimeridian
    /// columns and the polar clamp, so the Python port of `cell_window`
    /// is held to this one's on every branch it has.
    fn proxy_fixture() -> String {
        let mut out = String::from(
            "-- The SQL `web/serve.py`'s /clickhouse/ proxy must build, case by case.\n\
             -- Written by the Rust test `web_proxy_template_fixture` (src/overlay/dotgrid/clickhouse.rs);\n\
             -- read by serve.py's `test_proxy_template_matches_rust`. Do not edit by hand:\n\
             -- GPUI_MAP_WRITE_FIXTURES=1 cargo test web_proxy_template_fixture\n",
        );
        let tables = [("ghs_cells_30ss", 30.0), ("ghs_cells_3ss", 3.0)];
        let tile_cases: [(usize, u16, u8, &[(u32, u32)]); 6] = [
            (0, 2025, 0, &[(44, 27), (45, 27), (46, 27), (47, 27), (48, 27), (49, 27), (50, 27), (51, 27)]),
            (0, 1975, 3, &[(0, 0), (7, 7), (3, 4)]),
            (0, 2030, 6, &[(0, 0)]),
            (0, 2020, 1, &[(31, 14), (0, 16), (31, 31)]),
            (1, 2025, 0, &[(385, 222), (386, 222)]),
            (1, 2025, 4, &[(24, 13)]),
        ];
        for (t, epoch, level, tiles) in tile_cases {
            let (table, base) = tables[t];
            let max_data_zoom = grid::base_data_zoom(base);
            let d = GridDescriptor { base_arcsec: base, levels: max_data_zoom + 1, max_data_zoom, ..d30() };
            let source = ClickHouseProxySource::from_descriptor("./clickhouse", table, d).unwrap();
            let keys: Vec<GridTileKey> = tiles
                .iter()
                .map(|&(x, y)| GridTileKey { tile: TileId { z: max_data_zoom - level, x, y }, level, epoch, epoch_b: None })
                .collect();
            let ids: Vec<String> = keys.iter().map(|k| tile_id(k.tile).to_string()).collect();
            out.push_str(&format!("-- case tile table={table} epoch={epoch} level={level} tiles={}\n", ids.join(",")));
            out.push_str(&source.proxied_sql(&keys));
            out.push('\n');
        }
        for (table, _) in tables {
            out.push_str(&format!("-- case epochs table={table}\n{}\n", epochs_query(table)));
            out.push_str(&format!("-- case max_level table={table}\n{}\n", max_level_query(table)));
        }
        out
    }

    /// Task 16: the proxy's Python template is held to this file's. This
    /// test fails when the Rust output drifts from the committed fixture;
    /// `GPUI_MAP_WRITE_FIXTURES=1` rewrites it (then serve.py's
    /// `test_proxy_template_matches_rust` says whether the Python follows).
    #[test]
    fn web_proxy_template_fixture() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("web/tests/fixtures/ch_tile_query.sql");
        let want = proxy_fixture();
        if std::env::var_os("GPUI_MAP_WRITE_FIXTURES").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &want).unwrap();
        }
        let have = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            have == want,
            "{} drifted from clickhouse.rs's SQL; re-run with GPUI_MAP_WRITE_FIXTURES=1 and bring web/serve.py's template along",
            path.display()
        );
        for sql in want.split("\n-- case ").skip(1) {
            assert!(!star_select_in(sql) && !names_foreign_database(sql, DOTS_DB), "{sql}");
        }
    }

    /// Review F2: a block-part key (C7) is refused with an error by both
    /// ClickHouse sources -- no query is built, and no cell window is ever
    /// computed from level 255.
    #[test]
    fn a_part_key_is_an_error_not_a_query() {
        let part = GridTileKey::part(TileId { z: 12, x: 1050, y: 1520 }, 2025);
        assert_eq!(source().request(&[part]), GridRequest::Absent);
        assert!(matches!(source().decode(&[part], &[]), Err(GridError::Server(_))));
        assert_eq!(proxy().request(&[part]), GridRequest::Absent);
        assert!(matches!(proxy().decode(&[part], &[]), Err(GridError::Server(_))));
    }
}

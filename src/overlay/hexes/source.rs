//! The two [`HexSource`]s.
//!
//! - [`PmtilesHexSource`]: `race-hex-2020.pmtiles`, opened sans-IO through
//!   [`PmtilesHexOpener`] (the same three steps as the dot archive's
//!   `PmtilesOpener`, sharing `ArchiveOpener`). Compiles for every shell.
//!   Its metadata is `{"race_hex":{"version":1,"groups":8,"min_total":10,
//!   "tile_zoom":[[4,3],[5,5],[6,7],[7,9],[8,10]]},"attribution":"…"}`;
//!   an archive whose zooms or minimum disagree with this build's
//!   [`HEX_TILE_ZOOM`] / [`HEX_MIN_TOTAL`] is refused at open.
//! - `ClickHouseHexSource` (native only, `clickhouse-demo` plus `h3`):
//!   `gpui_map_dots.race_hex_2020` queried per `(res, tile)` on the cell
//!   centre, with `h3o` baking each boundary. Its SQL ([`hex_tile_query`])
//!   and row format ([`parse_rows`]) are sans-IO and compiled everywhere,
//!   so the `sql_guard` test covers the SQL in every build. It never goes
//!   to the web: a browser ClickHouse path would need `web/serve.py`'s
//!   locked-down proxy (whitelist, header, readonly), which this task does
//!   not add.

use super::codec;
use super::{hex_tile_zoom, HexRecord, HexSource, GROUPS, HEX_MIN_TOTAL, HEX_TILE_ZOOM};
use crate::geo::TileId;
use crate::overlay::dotgrid::pmtiles_dir::{self, PmEntry};
use crate::overlay::dotgrid::pmtiles_source::{find_entry, ArchiveOpener, OpenedArchive};
use crate::overlay::dotgrid::{GridError, GridRequest};

/// The `race_hex` metadata key's version this build reads.
pub const HEX_ARCHIVE_VERSION: u64 = 1;

/// What a hex archive's metadata describes.
#[derive(Debug, Clone, PartialEq)]
pub struct HexArchiveMeta {
    pub attribution: String,
}

/// Reads and checks the `race_hex` metadata key.
pub fn parse_hex_meta(meta_bytes: &[u8], compression: u8) -> Result<HexArchiveMeta, GridError> {
    let raw = pmtiles_dir::decompress(meta_bytes, compression)?;
    let root: serde_json::Value =
        serde_json::from_slice(&raw).map_err(|e| GridError::Server(format!("archive metadata is not JSON: {e}")))?;
    let hex = root
        .get("race_hex")
        .ok_or_else(|| GridError::Server("archive metadata has no \"race_hex\" key".to_string()))?;
    let int = |name: &str| hex.get(name).and_then(serde_json::Value::as_u64);
    if int("version") != Some(HEX_ARCHIVE_VERSION) {
        return Err(GridError::Server(format!("unsupported race_hex version {:?}", hex.get("version"))));
    }
    if int("groups") != Some(GROUPS as u64) || int("min_total") != Some(u64::from(HEX_MIN_TOTAL)) {
        return Err(GridError::Server("race_hex groups or min_total differ from this build's".to_string()));
    }
    let zooms: Option<Vec<(u64, u64)>> = hex.get("tile_zoom").and_then(serde_json::Value::as_array).map(|a| {
        a.iter()
            .filter_map(|p| Some((p.get(0)?.as_u64()?, p.get(1)?.as_u64()?)))
            .collect()
    });
    let want: Vec<(u64, u64)> = HEX_TILE_ZOOM.iter().map(|&(r, z)| (u64::from(r), u64::from(z))).collect();
    if zooms.as_ref() != Some(&want) {
        return Err(GridError::Server(format!("race_hex tile_zoom {zooms:?} is not this build's {want:?}")));
    }
    let attribution = root.get("attribution").and_then(serde_json::Value::as_str).unwrap_or_default().to_string();
    Ok(HexArchiveMeta { attribution })
}

/// The JSON metadata [`parse_hex_meta`] accepts (the converter writes it).
pub fn hex_meta_json(attribution: &str) -> serde_json::Value {
    serde_json::json!({
        "race_hex": {
            "version": HEX_ARCHIVE_VERSION,
            "groups": GROUPS,
            "min_total": HEX_MIN_TOTAL,
            "tile_zoom": HEX_TILE_ZOOM.iter().map(|&(r, z)| [r, z]).collect::<Vec<_>>(),
        },
        "attribution": attribution,
    })
}

/// Opens a hex archive in the same three sans-IO steps as the dot
/// archive's `PmtilesOpener`: [`Self::first_request`], then
/// [`Self::after_first`]'s follow-ups, then [`Self::finish`].
pub struct PmtilesHexOpener(ArchiveOpener<HexArchiveMeta>);

impl PmtilesHexOpener {
    pub fn new(location: impl Into<String>) -> Self {
        Self(ArchiveOpener::new(location, parse_hex_meta))
    }

    pub fn first_request(&self) -> GridRequest {
        self.0.first_request()
    }

    pub fn after_first(&mut self, bytes: &[u8]) -> Result<Vec<GridRequest>, GridError> {
        self.0.after_first(bytes)
    }

    pub fn finish(self, bodies: &[Vec<u8>]) -> Result<PmtilesHexSource, GridError> {
        let OpenedArchive { meta, entries, location } = self.0.finish_archive(bodies)?;
        Ok(PmtilesHexSource { meta, entries, location })
    }
}

/// Hex tiles from a PMTiles archive: one entry per `(res, tile)`, keyed
/// by the tile's own `z/x/y` (each resolution has its own zoom).
pub struct PmtilesHexSource {
    meta: HexArchiveMeta,
    entries: Vec<PmEntry>,
    location: String,
}

impl PmtilesHexSource {
    pub fn meta(&self) -> &HexArchiveMeta {
        &self.meta
    }

    /// Every `(res, tile)` the archive holds, in tile-id order (the
    /// converter's read-back walks these).
    pub fn tiles(&self) -> Vec<(u8, TileId)> {
        HEX_TILE_ZOOM
            .iter()
            .flat_map(|&(res, z)| {
                let n = 1u32 << z;
                // Only the tiles an entry covers: the archive is sparse.
                self.entries
                    .iter()
                    .flat_map(|e| (0..u64::from(e.run_length)).map(move |k| e.tile_id + k))
                    .filter_map(move |id| tile_of_id(id, z, n).map(|t| (res, t)))
            })
            .collect()
    }
}

/// The `z`-zoom tile a PMTiles tile id names, or `None` when it names a
/// tile at another zoom.
fn tile_of_id(id: u64, z: u8, n: u32) -> Option<TileId> {
    // Tile ids count zooms in order: z's ids start after every tile of
    // the zooms before it.
    let base = pmtiles_dir::zxy_to_tile_id(z, 0, 0);
    let count = u64::from(n) * u64::from(n);
    if id < base || id >= base + count {
        return None;
    }
    // Invert the Hilbert index by search over the rows would be slow; a
    // tile id is found back by walking the curve (d2xy).
    let d = id - base;
    let (mut x, mut y, mut t) = (0u64, 0u64, d);
    let mut s = 1u64;
    while s < u64::from(n) {
        let rx = 1 & (t / 2);
        let ry = 1 & (t ^ rx);
        if ry == 0 {
            if rx == 1 {
                x = s - 1 - x;
                y = s - 1 - y;
            }
            std::mem::swap(&mut x, &mut y);
        }
        x += s * rx;
        y += s * ry;
        t /= 4;
        s *= 2;
    }
    let tile = TileId { z, x: x as u32, y: y as u32 };
    (pmtiles_dir::zxy_to_tile_id(z, tile.x, tile.y) == id).then_some(tile)
}

impl HexSource for PmtilesHexSource {
    fn attribution(&self) -> Option<&str> {
        Some(self.meta.attribution.trim()).filter(|a| !a.is_empty())
    }

    fn request(&self, res: u8, tile: TileId) -> GridRequest {
        if hex_tile_zoom(res) != Some(tile.z) {
            debug_assert!(false, "an r{res} hex tile requested at z{}", tile.z);
            return GridRequest::Absent;
        }
        match find_entry(&self.entries, pmtiles_dir::zxy_to_tile_id(tile.z, tile.x, tile.y)) {
            Some(e) => GridRequest::Range { location: self.location.clone(), offset: e.offset, length: u64::from(e.length) },
            None => GridRequest::Absent,
        }
    }

    /// An empty body is the [`GridRequest::Absent`] case: no hexagons. A
    /// tile whose header names another resolution or tile is an error.
    fn decode(&self, res: u8, tile: TileId, body: &[u8]) -> Result<Vec<HexRecord>, GridError> {
        if body.is_empty() {
            return Ok(Vec::new());
        }
        let t = codec::decode(body)?;
        if (t.res, t.tile) != (res, tile) {
            return Err(GridError::Server(format!(
                "hex tile header names r{} {}/{}/{}, not the requested r{res} {}/{}/{}",
                t.res, t.tile.z, t.tile.x, t.tile.y, tile.z, tile.x, tile.y
            )));
        }
        Ok(t.records)
    }
}

/// Bytes of one RowBinary row of [`hex_tile_query`]: `h3 UInt64` and
/// `c1`..`c8 UInt32`.
pub const HEX_ROW_BYTES: usize = 8 + 4 * GROUPS;

/// How far past the tile's edges the SQL's centre box reaches, in
/// degrees: ClickHouse's `h3ToGeo` and `h3o`'s centre can differ in the
/// last bits, so the box is padded and the rows are cut back to the tile
/// by `h3o`'s centre ([`super::owning_tile`], the rule the converter uses).
pub const CENTRE_PAD_DEG: f64 = 1e-6;

/// The SQL of one `(res, tile)`: every hexagon of `res` with at least
/// [`HEX_MIN_TOTAL`] people whose centre lies in the tile's (padded)
/// lon/lat box, columns enumerated, in H3 order.
///
/// `h3ToGeo` returns `(lat, lon)` on the ClickHouse this was built
/// against (26.2; the argument-order change of 25.5 turned `geoToH3` to
/// `(lat, lon)` too); `live_centre_is_lat_then_lon` pins it.
pub fn hex_tile_query(res: u8, tile: TileId) -> String {
    let b = crate::geo::tile_bounds_lonlat(tile);
    let (s, n) = (b.min_lat - CENTRE_PAD_DEG, b.max_lat + CENTRE_PAD_DEG);
    let (w, e) = (b.min_lon - CENTRE_PAD_DEG, b.max_lon + CENTRE_PAD_DEG);
    format!(
        "SELECT h.h3, h.c1, h.c2, h.c3, h.c4, h.c5, h.c6, h.c7, h.c8\n\
         FROM gpui_map_dots.race_hex_2020 AS h\n\
         WHERE h.res = {res} AND h.total >= {HEX_MIN_TOTAL}\n\
         \x20 AND tupleElement(h3ToGeo(h.h3), 1) BETWEEN {s:.9} AND {n:.9}\n\
         \x20 AND tupleElement(h3ToGeo(h.h3), 2) BETWEEN {w:.9} AND {e:.9}\n\
         ORDER BY h.h3\n\
         FORMAT RowBinary\n\
         SETTINGS use_query_cache = 1"
    )
}

/// Every SQL this module sends, for the `sql_guard` test.
#[doc(hidden)]
pub fn all_queries_for_test() -> Vec<String> {
    HEX_TILE_ZOOM.iter().map(|&(res, z)| hex_tile_query(res, crate::geo::tile_id_at(-73.97, 40.78, z))).collect()
}

/// [`hex_tile_query`]'s RowBinary rows as `(h3, counts)`, checked: whole
/// rows, ascending distinct H3 cells of `res`, at least [`HEX_MIN_TOTAL`]
/// people within `u32`. A ClickHouse exception body is an error.
pub fn parse_rows(res: u8, body: &[u8]) -> Result<Vec<(u64, [u32; GROUPS])>, GridError> {
    if let Some(e) = crate::overlay::dotgrid::clickhouse::exception_in(body) {
        return Err(e);
    }
    if !body.len().is_multiple_of(HEX_ROW_BYTES) {
        return Err(GridError::Truncated);
    }
    let mut out = Vec::with_capacity(body.len() / HEX_ROW_BYTES);
    let mut prev = None;
    for row in body.as_chunks::<HEX_ROW_BYTES>().0.iter() {
        let h3 = u64::from_le_bytes(row[..8].try_into().expect("8 bytes"));
        let mut c = [0u32; GROUPS];
        for (g, v) in c.iter_mut().enumerate() {
            *v = u32::from_le_bytes(row[8 + 4 * g..12 + 4 * g].try_into().expect("4 bytes"));
        }
        if !codec::is_cell_of_res(h3, res) || prev.is_some_and(|p| p >= h3) {
            return Err(GridError::Server(format!("row {h3:#x} is not an ascending r{res} cell")));
        }
        let total = c.iter().try_fold(0u32, |a, &v| a.checked_add(v));
        if total.is_none_or(|t| t < HEX_MIN_TOTAL) {
            return Err(GridError::Server(format!("row {h3:#x} has {total:?} people, not 10..=u32::MAX")));
        }
        prev = Some(h3);
        out.push((h3, c));
    }
    Ok(out)
}

/// `race_hex_2020` read live from ClickHouse, native only: one POST per
/// `(res, tile)` ([`hex_tile_query`]), each row's boundary baked by
/// `h3o` and cut back to the hexagons whose `h3o` centre the tile owns.
#[cfg(all(feature = "clickhouse-demo", feature = "h3", not(target_arch = "wasm32")))]
pub struct ClickHouseHexSource {
    url: String,
}

#[cfg(all(feature = "clickhouse-demo", feature = "h3", not(target_arch = "wasm32")))]
impl ClickHouseHexSource {
    /// No IO: the first tile request is the first contact.
    pub fn new(url: &str) -> Self {
        Self { url: url.to_string() }
    }
}

#[cfg(all(feature = "clickhouse-demo", feature = "h3", not(target_arch = "wasm32")))]
impl HexSource for ClickHouseHexSource {
    fn request(&self, res: u8, tile: TileId) -> GridRequest {
        if hex_tile_zoom(res) != Some(tile.z) {
            debug_assert!(false, "an r{res} hex tile requested at z{}", tile.z);
            return GridRequest::Absent;
        }
        GridRequest::Post {
            url: crate::overlay::dotgrid::clickhouse::endpoint(&self.url),
            body: hex_tile_query(res, tile),
            headers: vec![("Accept-Encoding".to_string(), "identity".to_string())],
        }
    }

    fn decode(&self, res: u8, tile: TileId, body: &[u8]) -> Result<Vec<HexRecord>, GridError> {
        let mut out = Vec::new();
        for (h3, counts) in parse_rows(res, body)? {
            let (lon, lat) = super::cell_centre(h3)?;
            if super::owning_tile(res, lon, lat) != Some(tile) {
                continue; // in the padding: the neighbouring tile's
            }
            let rec = super::record_for_cell(h3, counts)?;
            if rec.res != res {
                return Err(GridError::Server(format!("{h3:#x} is not an r{res} cell")));
            }
            out.push(rec);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use ::pmtiles::{Compression, PmTilesWriter, TileCoord, TileType};

    use super::super::codec::tests::{fake_cell, nyc, rec_at};
    use super::*;
    use crate::overlay::dotgrid::sql_guard::{names_foreign_database, star_select_in};

    fn archive(tiles: &[(TileId, Vec<u8>)], meta: &str) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut w = PmTilesWriter::new(TileType::Unknown)
                .metadata(meta)
                .tile_compression(Compression::None)
                .create(&mut buf)
                .unwrap();
            for (t, data) in tiles {
                w.add_tile(TileCoord::new(t.z, t.x, t.y).unwrap(), data).unwrap();
            }
            w.finalize().unwrap();
        }
        buf.into_inner()
    }

    fn fetch(bytes: &[u8], req: &GridRequest) -> Vec<u8> {
        match req {
            GridRequest::Range { offset, length, .. } => {
                bytes[*offset as usize..(*offset + *length).min(bytes.len() as u64) as usize].to_vec()
            }
            GridRequest::Absent => Vec::new(),
            other => panic!("{other:?}"),
        }
    }

    fn open(bytes: &[u8]) -> Result<PmtilesHexSource, GridError> {
        let mut o = PmtilesHexOpener::new("mem");
        let more = o.after_first(&fetch(bytes, &o.first_request()))?;
        let bodies: Vec<Vec<u8>> = more.iter().map(|r| fetch(bytes, r)).collect();
        o.finish(&bodies)
    }

    fn meta() -> String {
        hex_meta_json("U.S. Census Bureau").to_string()
    }

    #[test]
    fn a_pmtiles_archive_serves_each_resolution_at_its_zoom() {
        let r7: Vec<HexRecord> = (0..5).map(|i| rec_at(7, i, [10 + i as u32, 0, 0, 0, 0, 0, 0, 0])).collect();
        let r8 = vec![rec_at(8, 0, [3, 30, 0, 0, 0, 0, 0, 0])];
        let bytes = archive(
            &[(nyc(7), codec::encode_tile(7, nyc(7), &r7)), (nyc(8), codec::encode_tile(8, nyc(8), &r8))],
            &meta(),
        );
        let src = open(&bytes).unwrap();
        assert_eq!(src.meta().attribution, "U.S. Census Bureau");
        let got = src.decode(7, nyc(7), &fetch(&bytes, &src.request(7, nyc(7)))).unwrap();
        let mut want: Vec<u64> = r7.iter().map(|r| r.h3).collect();
        want.sort_unstable();
        assert_eq!(got.iter().map(|r| r.h3).collect::<Vec<_>>(), want);
        assert_eq!(src.decode(8, nyc(8), &fetch(&bytes, &src.request(8, nyc(8)))).unwrap()[0].counts[1], 30);
        // A tile the archive does not hold is absent, and decodes to nothing.
        let other = TileId { z: 9, x: 1, y: 1 };
        assert_eq!(src.request(7, other), GridRequest::Absent);
        assert!(src.decode(7, other, &[]).unwrap().is_empty());
        let mut listed = src.tiles();
        listed.sort_by_key(|t| t.0);
        assert_eq!(listed, vec![(7, nyc(7)), (8, nyc(8))]);
    }

    #[test]
    fn a_tile_served_under_another_key_is_an_error() {
        let body = codec::encode_tile(7, nyc(7), &[rec_at(7, 0, [10, 0, 0, 0, 0, 0, 0, 0])]);
        let src = open(&archive(&[(nyc(7), body.clone())], &meta())).unwrap();
        let wrong = TileId { x: nyc(7).x + 1, ..nyc(7) };
        assert!(matches!(src.decode(7, wrong, &body), Err(GridError::Server(e)) if e.contains("not the requested")));
    }

    #[test]
    fn an_archive_of_other_zooms_or_minimum_is_refused_at_open() {
        for bad in [
            meta().replace("[8,10]", "[8,11]"),
            meta().replace("\"min_total\":10", "\"min_total\":5"),
            meta().replace("\"version\":1", "\"version\":2"),
            r#"{"dotgrid":{}}"#.to_string(),
        ] {
            assert!(open(&archive(&[], &bad)).is_err(), "{bad}");
        }
        assert!(open(&archive(&[], &meta())).is_ok());
    }

    #[test]
    fn tile_ids_invert() {
        for (z, x, y) in [(3u8, 2u32, 3u32), (9, 150, 192), (10, 301, 385), (5, 31, 0), (0, 0, 0)] {
            let id = pmtiles_dir::zxy_to_tile_id(z, x, y);
            assert_eq!(tile_of_id(id, z, 1 << z), Some(TileId { z, x, y }));
            assert_eq!(tile_of_id(id, z + 1, 2 << z), None);
        }
    }

    #[test]
    fn hex_sql_is_read_only_enumerated_and_in_our_database() {
        for q in all_queries_for_test() {
            assert!(!star_select_in(&q), "{q}");
            assert!(!names_foreign_database(&q, "gpui_map_dots"), "{q}");
            assert!(q.starts_with("SELECT ") && !q.contains(';'), "{q}");
            assert!(q.contains("h.total >= 10"), "{q}");
        }
    }

    #[test]
    fn clickhouse_rows_are_checked() {
        let row = |h3: u64, c0: u32| {
            let mut b = h3.to_le_bytes().to_vec();
            b.extend_from_slice(&c0.to_le_bytes());
            b.extend_from_slice(&[0u8; 28]);
            b
        };
        let (a, b) = (fake_cell(7, 20, 1), fake_cell(7, 20, 2));
        let ok = [row(a, 10), row(b, 12)].concat();
        assert_eq!(parse_rows(7, &ok).unwrap(), vec![(a, [10, 0, 0, 0, 0, 0, 0, 0]), (b, [12, 0, 0, 0, 0, 0, 0, 0])]);
        assert_eq!(parse_rows(7, &ok[..HEX_ROW_BYTES + 3]), Err(GridError::Truncated));
        assert!(parse_rows(7, &[row(b, 10), row(a, 10)].concat()).is_err()); // descending
        assert!(parse_rows(7, &row(a, 9)).is_err()); // under 10
        assert!(parse_rows(8, &row(a, 10)).is_err()); // another resolution
        let mut over = row(a, u32::MAX);
        over[12] = 1;
        assert!(parse_rows(7, &over).is_err()); // past u32
        assert!(parse_rows(7, b"Code: 60. DB::Exception: no table").is_err());
    }

    /// Needs the local ClickHouse with Task 9's tables. Pins `h3ToGeo`'s
    /// `(lat, lon)` order and that a tile's rows are exactly its hexagons.
    #[cfg(all(feature = "clickhouse-demo", feature = "h3"))]
    #[test]
    #[ignore = "needs the local ClickHouse with gpui_map_dots.race_hex_2020; CLICKHOUSE_URL"]
    fn live_centre_is_lat_then_lon() {
        let url = std::env::var("CLICKHOUSE_URL").unwrap_or_else(|_| "http://localhost:8123".into());
        let src = ClickHouseHexSource::new(&url);
        let tile = nyc(7);
        let body = crate::overlay::dotgrid::fetch_native::execute(&src.request(7, tile)).unwrap();
        let recs = src.decode(7, tile, &body).unwrap();
        assert!(recs.len() > 100, "{} hexagons over New York at r7", recs.len());
        for r in &recs {
            let (lon, lat) = super::super::cell_centre(r.h3).unwrap();
            assert_eq!(super::super::owning_tile(7, lon, lat), Some(tile));
            assert!(r.total() >= HEX_MIN_TOTAL);
        }
    }
}

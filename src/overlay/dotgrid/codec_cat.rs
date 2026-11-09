//! Codec v2: categorical count tiles (`DGT2`, 8 group counts per cell) and
//! block-part tiles (`DGP1`, the triangles of census blocks with their group
//! counts). Spec `2026-09-27-us-race-dot-map-design.md` §3.
//!
//! Both are framed like v1 (`codec`): a little-endian header and a varint
//! body, the whole gzipped, inflated under [`codec::MAX_INFLATED_BYTES`].
//! [`decode_any`] reads any of the three by its magic, so a v1 archive (the
//! world 30″ and 3″ GHSL pyramids) still decodes unchanged. Sans-IO: the
//! web and iOS shells decode through this too.

use std::collections::HashMap;
use std::io::Write;

use super::codec::{self, write_varint, Reader};
use super::composition::GROUPS;
use super::parts::{Part, WORLD_QUANTA};
use super::{GridError, GridTile};
use crate::geo::TileId;

/// Magic of a categorical cell tile.
pub const MAGIC_CAT: [u8; 4] = *b"DGT2";
/// Magic of a block-part tile.
pub const MAGIC_PART: [u8; 4] = *b"DGP1";

/// The version byte both v2 formats write after their magic.
const VERSION: u8 = 1;

/// The most cells a `DGT2` tile's window may hold: 2^20. A data tile's
/// window is about 845 x 845 cells at the densest pyramid level (714,025 --
/// `codec`'s own test window; MG-7's largest real tile holds 359,602), so
/// this is ~1.5x headroom. A header claiming a bigger window is refused, and
/// the cell count is bounded by the window: at 44 B of heap a decoded cell
/// (12 B in `cells`, 32 in `cats`), the records of one tile come to at most
/// 46 MiB, whatever its body claims. That is not the decoder's whole peak:
/// [`decode_any`] keeps the inflated body (up to
/// [`codec::MAX_INFLATED_BYTES`], 64 MiB) live while the records decode, and
/// a growing `Vec` briefly holds its old and new buffers, so one worker's
/// worst case is about 64 + 46 + ~23 MiB, ~133 MiB.
pub const MAX_CELLS_PER_TILE: u32 = 1 << 20;

/// The most parts a `DGP1` tile may hold: 2^19 (524,288). A z12 part tile
/// is ~10 km across; even Manhattan's densest tiles hold a few thousand
/// blocks at tens of triangles each, well under this. The converter (Task
/// 5) must not write more -- `encode_parts` asserts it -- so an over-full
/// tile fails the build loudly instead of shipping one no shell will read.
/// With the vertex pool capped at 3 per part, one tile's records come to at
/// most 80 B x 2^19 + 16 B x 3 x 2^19 = 64 MiB. On top of that [`decode_any`]
/// keeps the inflated body (up to [`codec::MAX_INFLATED_BYTES`], 64 MiB) live
/// while they decode, and a growing `Vec` briefly holds its old and new
/// buffers, so one worker's worst case is about 64 + 64 + ~20 MiB, ~148 MiB.
pub const MAX_PARTS_PER_TILE: u32 = 1 << 19;

/// The most records a decoder reserves before it has read any: the rest
/// grow as records actually decode, so a header's claim never sizes an
/// allocation by itself.
const PREALLOC: usize = 4096;

/// A `Vec` capacity for `n` untrusted records: at most [`PREALLOC`].
fn prealloc(n: u32) -> usize {
    (n as usize).min(PREALLOC)
}

/// One decoded block part: a triangle in unit world `[0, 1)` (world-grid
/// quanta / 2^31, exact in f64) and the people of each group it holds.
///
/// An in-memory decode result, not a fixed-size wire record: on the wire a
/// part is three vertex-pool indices, a mask and its nonzero counts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PartRecord {
    pub a: [f64; 2],
    pub b: [f64; 2],
    pub c: [f64; 2],
    pub counts: [u32; GROUPS],
}

/// Bit `g` set when group `g`'s count is nonzero.
fn nonzero_mask(c: &[u32; GROUPS]) -> u8 {
    c.iter().enumerate().fold(0u8, |m, (g, &n)| if n > 0 { m | 1 << g } else { m })
}

/// The mask byte, then each nonzero count as a varint, in group order.
fn write_counts(buf: &mut Vec<u8>, c: &[u32; GROUPS]) {
    buf.push(nonzero_mask(c));
    for &n in c.iter().filter(|&&n| n > 0) {
        write_varint(buf, u64::from(n));
    }
}

/// Inverse of [`write_counts`]. A zero mask is refused: the encoders never
/// store an empty cell or part, so one is a corrupt tile.
fn read_counts(r: &mut Reader<'_>) -> Result<[u32; GROUPS], GridError> {
    let mask = r.u8()?;
    if mask == 0 {
        return Err(corrupt("a cell or part with no people"));
    }
    let mut c = [0u32; GROUPS];
    for (g, n) in c.iter_mut().enumerate() {
        if mask & (1 << g) != 0 {
            let v = r.varint()?;
            *n = u32::try_from(v).ok().filter(|&v| v > 0).ok_or_else(|| corrupt("a group count of 0 or past u32"))?;
        }
    }
    Ok(c)
}

fn corrupt(why: &str) -> GridError {
    GridError::Server(format!("corrupt v2 dot tile: {why}"))
}

fn gzip(body: &[u8]) -> Vec<u8> {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(body).expect("gzip write to a Vec sink never fails");
    gz.finish().expect("gzip finish on a Vec sink never fails")
}

/// A categorical cell tile. Header (little-endian, before compression):
/// [`MAGIC_CAT`], version u8 = 1, level u8, window_cells u32 (the window's
/// rows*cols), n u32 (stored cells). Then per cell: its index as a delta
/// varint (first absolute), a nonzero-mask byte, and the nonzero group
/// counts as varints. Whole body gzip. Unlike `DGT1` there is no epoch in
/// the header: a key's epoch comes from the archive's descriptor (P10).
///
/// A cell whose 8 counts are all 0 is not stored. `cells` must be sorted
/// by index, each index once (debug-asserted): the decoder refuses a
/// repeated index.
pub fn encode_cells(level: u8, window_cells: u32, cells: &[(u32, [u32; GROUPS])]) -> Vec<u8> {
    debug_assert!(cells.windows(2).all(|w| w[0].0 < w[1].0), "encode_cells requires cells sorted by index, each index once");
    assert!(window_cells <= MAX_CELLS_PER_TILE, "a {window_cells}-cell window is past MAX_CELLS_PER_TILE");
    let mut payload = Vec::new();
    let mut n = 0u32;
    let mut prev = 0u32;
    for (idx, c) in cells {
        if c.iter().all(|&v| v == 0) {
            continue;
        }
        write_varint(&mut payload, u64::from(if n == 0 { *idx } else { idx - prev }));
        write_counts(&mut payload, c);
        prev = *idx;
        n += 1;
    }
    let mut body = Vec::with_capacity(14 + payload.len());
    body.extend_from_slice(&MAGIC_CAT);
    body.push(VERSION);
    body.push(level);
    body.extend_from_slice(&window_cells.to_le_bytes());
    body.extend_from_slice(&n.to_le_bytes());
    body.extend_from_slice(&payload);
    gzip(&body)
}

fn decode_cells(body: &[u8]) -> Result<GridTile, GridError> {
    let mut r = Reader::new(body);
    r.take(4)?; // magic, checked by the caller
    let version = r.u8()?;
    if version != VERSION {
        return Err(GridError::BadVersion(version));
    }
    let _level = r.u8()?;
    let window = r.u32_le()?;
    let n = r.u32_le()?;
    if window > MAX_CELLS_PER_TILE || n > window {
        return Err(corrupt("more cells than a tile window holds"));
    }
    // Every cell costs at least 3 bytes (index, mask, one count): refuse a
    // count the body cannot hold before reading on.
    if u64::from(n) * 3 > r.remaining() as u64 {
        return Err(GridError::Truncated);
    }
    let mut cells = Vec::with_capacity(prealloc(n));
    let mut cats = Vec::with_capacity(prealloc(n));
    let mut total = 0f64;
    let mut idx = 0u32;
    for i in 0..n {
        let delta = u32::try_from(r.varint()?).map_err(|_| corrupt("an index delta past u32"))?;
        if i > 0 && delta == 0 {
            return Err(corrupt("a cell index stored twice"));
        }
        idx = if i == 0 { delta } else { idx.checked_add(delta).ok_or_else(|| corrupt("an index past u32"))? };
        if idx >= window {
            return Err(GridError::IndexOutOfTile { idx, cells: window });
        }
        let c = read_counts(&mut r)?;
        let sum: u64 = c.iter().map(|&v| u64::from(v)).sum();
        cells.push((idx, sum as f32, sum as f32));
        cats.push(c);
        total += sum as f64;
    }
    if r.remaining() != 0 {
        return Err(corrupt("bytes after the last cell"));
    }
    Ok(GridTile { cells, total, cats, parts: Vec::new() })
}

/// The world-grid quanta of a tile's NW corner: `2^31 / 2^z` quanta a tile.
fn tile_origin(tile: TileId) -> [i64; 2] {
    let shift = 31 - u32::from(tile.z);
    [i64::from(tile.x) << shift, i64::from(tile.y) << shift]
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

/// A block-part tile. Header (little-endian, before compression):
/// [`MAGIC_PART`], version u8 = 1, z u8, x u32, y u32, vertex count u32,
/// part count u32. Then the vertex pool: each vertex's x and y in quanta
/// relative to the tile's NW corner (so an i32: a part may straddle the
/// tile's edge), each stored as the zig-zag varint of its delta from the
/// previous vertex's (the first from 0). Then per part: its three pool
/// indices as varints, a nonzero-mask byte and the nonzero group counts as
/// varints. Whole body gzip.
///
/// Vertices shared between parts are pooled once, in first-use order.
///
/// # Panics
/// If `tile.z > 31`, or a vertex lies more than 2^31 quanta from the tile's
/// corner (it cannot: vertices are on the world grid, `[0, 2^31)`).
pub fn encode_parts(tile: TileId, parts: &[Part]) -> Vec<u8> {
    assert!(tile.z <= 31, "a part tile at z{} is below the world grid's quantum", tile.z);
    let o = tile_origin(tile);
    let mut pool: Vec<[i32; 2]> = Vec::new();
    let mut at: HashMap<[i32; 2], u32> = HashMap::new();
    let mut part_bytes = Vec::new();
    let mut n_parts = 0u32;
    assert!(parts.len() as u64 <= u64::from(MAX_PARTS_PER_TILE), "{} parts is past MAX_PARTS_PER_TILE", parts.len());
    for p in parts {
        if p.c.iter().all(|&v| v == 0) {
            continue;
        }
        for v in [p.tri.a, p.tri.b, p.tri.c] {
            let rel = [v[0] - o[0], v[1] - o[1]].map(|q| i32::try_from(q).expect("a part vertex within 2^31 quanta of its tile"));
            let i = *at.entry(rel).or_insert_with(|| {
                pool.push(rel);
                pool.len() as u32 - 1
            });
            write_varint(&mut part_bytes, u64::from(i));
        }
        write_counts(&mut part_bytes, &p.c);
        n_parts += 1;
    }
    let mut body = Vec::with_capacity(22 + pool.len() * 4 + part_bytes.len());
    body.extend_from_slice(&MAGIC_PART);
    body.push(VERSION);
    body.push(tile.z);
    body.extend_from_slice(&tile.x.to_le_bytes());
    body.extend_from_slice(&tile.y.to_le_bytes());
    body.extend_from_slice(&(pool.len() as u32).to_le_bytes());
    body.extend_from_slice(&n_parts.to_le_bytes());
    let mut prev = [0i64; 2];
    for v in &pool {
        for k in 0..2 {
            write_varint(&mut body, zigzag(i64::from(v[k]) - prev[k]));
            prev[k] = i64::from(v[k]);
        }
    }
    body.extend_from_slice(&part_bytes);
    gzip(&body)
}

/// `expected`, when given, is the tile the body was fetched for: a header
/// naming another tile is refused, since the vertices are placed relative
/// to the header's origin and would otherwise be drawn, and counted, there.
fn decode_parts(body: &[u8], expected: Option<TileId>) -> Result<GridTile, GridError> {
    let mut r = Reader::new(body);
    r.take(4)?; // magic, checked by the caller
    let version = r.u8()?;
    if version != VERSION {
        return Err(GridError::BadVersion(version));
    }
    let z = r.u8()?;
    let x = r.u32_le()?;
    let y = r.u32_le()?;
    if z > 31 || u64::from(x) >> z != 0 || u64::from(y) >> z != 0 {
        return Err(corrupt("a tile id off the world grid"));
    }
    if let Some(want) = expected {
        if (z, x, y) != (want.z, want.x, want.y) {
            return Err(corrupt(&format!("a part tile {z}/{x}/{y} fetched as {}/{}/{}", want.z, want.x, want.y)));
        }
    }
    let n_verts = r.u32_le()?;
    let n_parts = r.u32_le()?;
    if n_parts > MAX_PARTS_PER_TILE || u64::from(n_verts) > 3 * u64::from(n_parts) {
        return Err(corrupt("more parts or vertices than a part tile holds"));
    }
    // A vertex costs at least 2 bytes, a part at least 5 (3 indices, a
    // mask, one count): refuse counts the body cannot hold up front.
    if u64::from(n_verts) * 2 + u64::from(n_parts) * 5 > r.remaining() as u64 {
        return Err(GridError::Truncated);
    }
    let o = tile_origin(TileId { z, x, y });
    let q = WORLD_QUANTA as f64;
    let mut pool = Vec::with_capacity(prealloc(n_verts));
    let mut prev = [0i64; 2];
    for _ in 0..n_verts {
        let mut v = [0f64; 2];
        for k in 0..2 {
            let rel = prev[k]
                .checked_add(unzigzag(r.varint()?))
                .filter(|rel| i32::try_from(*rel).is_ok())
                .ok_or_else(|| corrupt("a vertex past i32 quanta from its tile"))?;
            prev[k] = rel;
            v[k] = (o[k] + rel) as f64 / q;
        }
        pool.push(v);
    }
    let mut parts = Vec::with_capacity(prealloc(n_parts));
    let mut total = 0f64;
    for _ in 0..n_parts {
        let mut tri = [[0f64; 2]; 3];
        for t in &mut tri {
            let i = r.varint()?;
            *t = *usize::try_from(i)
                .ok()
                .and_then(|i| pool.get(i))
                .ok_or(GridError::IndexOutOfTile { idx: i.min(u64::from(u32::MAX)) as u32, cells: n_verts })?;
        }
        let counts = read_counts(&mut r)?;
        total += counts.iter().map(|&v| f64::from(v)).sum::<f64>();
        parts.push(PartRecord { a: tri[0], b: tri[1], c: tri[2], counts });
    }
    if r.remaining() != 0 {
        return Err(corrupt("bytes after the last part"));
    }
    Ok(GridTile { cells: Vec::new(), total, cats: Vec::new(), parts })
}

/// Decodes a count tile of any version by its magic: `DGT1` through
/// [`codec`] (so `cats` and `parts` come back empty), `DGT2` into `cells`
/// and `cats`, `DGP1` into `parts`. Every short read is
/// [`GridError::Truncated`], never a panic; foreign bytes are
/// [`GridError::BadMagic`].
pub fn decode_any(bytes: &[u8]) -> Result<GridTile, GridError> {
    decode(bytes, None)
}

/// [`decode_any`] for a body fetched for `tile`: a `DGP1` header naming any
/// other tile is refused (its vertices are relative to the header's
/// origin). `DGT1`/`DGT2` carry no tile id and decode as [`decode_any`]
/// does.
pub fn decode_any_at(bytes: &[u8], tile: TileId) -> Result<GridTile, GridError> {
    decode(bytes, Some(tile))
}

fn decode(bytes: &[u8], expected: Option<TileId>) -> Result<GridTile, GridError> {
    let body = codec::inflate_capped(bytes, codec::MAX_INFLATED_BYTES)?;
    let magic = body.get(..4).ok_or(GridError::Truncated)?;
    if magic == codec::MAGIC {
        codec::decode_body(&body)
    } else if magic == MAGIC_CAT {
        decode_cells(&body)
    } else if magic == MAGIC_PART {
        decode_parts(&body, expected)
    } else {
        Err(GridError::BadMagic)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::dotgrid::parts::Tri;

    #[test]
    fn categorical_cells_round_trip() {
        let cells = vec![(0u32, [1, 0, 0, 0, 0, 0, 0, 0]), (17, [0, 5, 0, 0, 2, 0, 0, 9]), (4000, [0; 8])];
        let t = decode_any(&encode_cells(0, 4096, &cells)).unwrap();
        assert_eq!(t.cells.iter().map(|c| c.0).collect::<Vec<_>>(), vec![0, 17]); // all-zero cells are not stored
        assert_eq!(t.cats, vec![[1, 0, 0, 0, 0, 0, 0, 0], [0, 5, 0, 0, 2, 0, 0, 9]]);
        assert_eq!(t.cells[1].1, 16.0);
        assert_eq!(t.total, 17.0);
    }

    #[test]
    fn parts_round_trip_to_within_one_quantum() {
        let tile = TileId { z: 12, x: 1206, y: 1538 };
        let (x0, y0) = (1206i64 << 19, 1538i64 << 19); // 2^31 / 2^12 = 2^19 quanta per tile
        let p = Part { tri: Tri { a: [x0 + 5, y0 + 7], b: [x0 + 900, y0 + 7], c: [x0 + 5, y0 + 1200] }, c: [0, 3, 1, 0, 0, 0, 0, 0] };
        let t = decode_any(&encode_parts(tile, &[p])).unwrap();
        let r = &t.parts[0];
        assert_eq!(r.counts, [0, 3, 1, 0, 0, 0, 0, 0]);
        assert!((r.a[0] - (x0 + 5) as f64 / (1u64 << 31) as f64).abs() < 1e-9);
        assert_eq!(t.total, 4.0);
    }

    #[test]
    fn a_truncated_categorical_tile_is_an_error_not_a_panic() {
        let b = encode_cells(0, 4096, &[(3, [1, 2, 3, 4, 5, 6, 7, 8])]);
        for n in 0..b.len() {
            assert!(decode_any(&b[..n]).is_err(), "len {n}");
        }
    }

    #[test]
    fn v1_tiles_still_decode() {
        let b = crate::overlay::dotgrid::codec::encode(0, 2025, 64, &[(1, 2.5)]);
        assert!(decode_any(&b).unwrap().cats.is_empty());
    }

    /// Compatibility: a v1 tile decodes through `decode_any` exactly as
    /// through `codec::decode` -- same cells, same total, nothing extra.
    #[test]
    fn a_v1_tile_decodes_the_same_through_either_door() {
        let b = crate::overlay::dotgrid::codec::encode(3, 2020, 70_000, &[(1, 2.5), (9, 0.04), (69_999, 812.3)]);
        let any = decode_any(&b).unwrap();
        assert_eq!(any, crate::overlay::dotgrid::codec::decode(&b).unwrap());
        assert_eq!(any.cells, vec![(1, 2.5, 2.5), (69_999, 812.3, 812.3)]);
        assert!(any.parts.is_empty());
    }

    /// Exact, not just within a quantum: every vertex is a whole number of
    /// quanta, and quanta / 2^31 is exact in f64. Shared vertices pool once,
    /// and a vertex beyond the tile's edge (a straddling part) survives.
    #[test]
    fn parts_round_trip_exactly_with_shared_and_straddling_vertices() {
        let tile = TileId { z: 12, x: 1206, y: 1538 };
        let (x0, y0) = (1206i64 << 19, 1538i64 << 19);
        let a = [x0 - 40, y0 + 3]; // west of the tile
        let b = [x0 + (1 << 19) + 7, y0 + 11]; // east of it
        let c = [x0 + 100, y0 + 200];
        let d = [x0 + 100, y0 - 9]; // north of it
        let ps = [
            Part { tri: Tri { a, b, c }, c: [70_000, 0, 0, 0, 0, 0, 0, 1] },
            Part { tri: Tri { a: b, b: c, c: d }, c: [0, 0, 5, 0, 0, 0, 0, 0] },
            Part { tri: Tri { a: d, b: a, c }, c: [0; 8] }, // empty: not stored
        ];
        let bytes = encode_parts(tile, &ps);
        let t = decode_any(&bytes).unwrap();
        let unit = |v: [i64; 2]| [v[0] as f64 / WORLD_QUANTA as f64, v[1] as f64 / WORLD_QUANTA as f64];
        assert_eq!(
            t.parts,
            vec![
                PartRecord { a: unit(a), b: unit(b), c: unit(c), counts: ps[0].c },
                PartRecord { a: unit(b), b: unit(c), c: unit(d), counts: ps[1].c },
            ]
        );
        assert_eq!(t.total, 70_006.0);
        assert!(t.cells.is_empty() && t.cats.is_empty());
        for n in 0..bytes.len() {
            assert!(decode_any(&bytes[..n]).is_err(), "len {n}");
        }
    }

    #[test]
    fn corners_of_the_world_round_trip() {
        let top = WORLD_QUANTA - 1;
        let p = Part { tri: Tri { a: [0, 0], b: [top, 0], c: [top, top] }, c: [1; 8] };
        let t = decode_any(&encode_parts(TileId { z: 0, x: 0, y: 0 }, &[p])).unwrap();
        let far = top as f64 / WORLD_QUANTA as f64;
        assert_eq!((t.parts[0].a, t.parts[0].b, t.parts[0].c), ([0.0, 0.0], [far, 0.0], [far, far]));
        let t = decode_any(&encode_parts(TileId { z: 31, x: (1 << 31) - 1, y: 0 }, &[p])).unwrap();
        assert_eq!(t.parts[0].c, [far, far]);
    }

    fn gz(body: &[u8]) -> Vec<u8> {
        gzip(body)
    }

    #[test]
    fn an_index_outside_the_window_is_refused() {
        let b = encode_cells(0, 10, &[(11, [1, 0, 0, 0, 0, 0, 0, 0])]);
        assert!(matches!(decode_any(&b), Err(GridError::IndexOutOfTile { idx: 11, cells: 10 })));
    }

    #[test]
    fn a_pool_index_past_the_pool_is_refused() {
        let mut body = Vec::new();
        body.extend_from_slice(&MAGIC_PART);
        body.extend_from_slice(&[VERSION, 0]);
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&1u32.to_le_bytes()); // one vertex
        body.extend_from_slice(&1u32.to_le_bytes()); // one part
        body.extend_from_slice(&[0, 0]); // the vertex
        body.extend_from_slice(&[0, 0, 1, 1, 1]); // indices 0, 0, 1 (past the pool), mask, count
        assert!(matches!(decode_any(&gz(&body)), Err(GridError::IndexOutOfTile { idx: 1, cells: 1 })));
    }

    #[test]
    fn headers_claiming_more_than_the_body_holds_are_refused_up_front() {
        let mut body = Vec::new();
        body.extend_from_slice(&MAGIC_CAT);
        body.extend_from_slice(&[VERSION, 0]);
        body.extend_from_slice(&MAX_CELLS_PER_TILE.to_le_bytes());
        body.extend_from_slice(&MAX_CELLS_PER_TILE.to_le_bytes());
        body.extend_from_slice(&[0, 1, 1]);
        assert_eq!(decode_any(&gz(&body)), Err(GridError::Truncated));

        let mut body = Vec::new();
        body.extend_from_slice(&MAGIC_PART);
        body.extend_from_slice(&[VERSION, 0]);
        body.extend_from_slice(&[0; 8]);
        body.extend_from_slice(&(3 * MAX_PARTS_PER_TILE).to_le_bytes());
        body.extend_from_slice(&MAX_PARTS_PER_TILE.to_le_bytes());
        body.extend_from_slice(&[0; 7]);
        assert_eq!(decode_any(&gz(&body)), Err(GridError::Truncated));
    }

    /// Review F1: a header's counts never size an allocation. A claim past
    /// the per-tile maximum is refused before any read; a claim within it
    /// but past the body is refused before any record decodes; and what a
    /// decoder reserves up front is at most PREALLOC records either way.
    /// (No wall-clock assert: the host-purity test bars `std::time` from
    /// this file. Each refusal happens before the first record is read.)
    #[test]
    fn huge_claims_are_refused_quickly_and_reserve_little() {
        fn cat_header(window: u32, n: u32) -> Vec<u8> {
            let mut b = MAGIC_CAT.to_vec();
            b.extend_from_slice(&[VERSION, 0]);
            b.extend_from_slice(&window.to_le_bytes());
            b.extend_from_slice(&n.to_le_bytes());
            b
        }
        fn part_header(n_verts: u32, n_parts: u32) -> Vec<u8> {
            let mut b = MAGIC_PART.to_vec();
            b.extend_from_slice(&[VERSION, 0]);
            b.extend_from_slice(&[0; 8]);
            b.extend_from_slice(&n_verts.to_le_bytes());
            b.extend_from_slice(&n_parts.to_le_bytes());
            b
        }
        // Past the maxima, with a body that would otherwise hold them: 4 MiB
        // of minimum-cost records (gzip makes that a few KiB on the wire).
        let pad = |mut b: Vec<u8>| {
            b.extend(std::iter::repeat_n([1u8, 1, 1], 4 << 20 >> 1).flatten());
            gz(&b)
        };
        assert!(matches!(decode_any(&pad(cat_header(u32::MAX, 1 << 21))), Err(GridError::Server(_))));
        assert!(matches!(decode_any(&pad(cat_header(MAX_CELLS_PER_TILE + 1, 1))), Err(GridError::Server(_))));
        assert!(matches!(decode_any(&pad(cat_header(10, 11))), Err(GridError::Server(_))));
        assert!(matches!(decode_any(&pad(part_header(3, MAX_PARTS_PER_TILE + 1))), Err(GridError::Server(_))));
        assert!(matches!(decode_any(&pad(part_header(u32::MAX, 1 << 18))), Err(GridError::Server(_))));
        // Within the maxima but past a small body.
        assert_eq!(decode_any(&gz(&cat_header(MAX_CELLS_PER_TILE, MAX_CELLS_PER_TILE))), Err(GridError::Truncated));
        assert_eq!(decode_any(&gz(&part_header(3 * MAX_PARTS_PER_TILE, MAX_PARTS_PER_TILE))), Err(GridError::Truncated));
        // The up-front reservation is capped whatever the claim.
        assert_eq!(prealloc(u32::MAX), PREALLOC);
        assert_eq!(prealloc(MAX_CELLS_PER_TILE), PREALLOC);
        assert_eq!(prealloc(7), 7);
        // Worst-case heap of a full tile stays near the inflate cap.
        let cell_heap = MAX_CELLS_PER_TILE as usize * (size_of::<(u32, f32, f32)>() + size_of::<[u32; GROUPS]>());
        let part_heap = MAX_PARTS_PER_TILE as usize * (size_of::<PartRecord>() + 3 * size_of::<[f64; 2]>());
        assert!(cell_heap <= 64 << 20 && part_heap <= 64 << 20, "{cell_heap} / {part_heap}");
    }

    #[test]
    fn corrupt_bodies_are_errors_not_panics() {
        // A zero mask, a tile off the grid, a bad version, foreign magic.
        let mut cells = Vec::new();
        cells.extend_from_slice(&MAGIC_CAT);
        cells.extend_from_slice(&[VERSION, 0]);
        cells.extend_from_slice(&10u32.to_le_bytes());
        cells.extend_from_slice(&1u32.to_le_bytes());
        cells.extend_from_slice(&[0, 0, 0]);
        assert!(matches!(decode_any(&gz(&cells)), Err(GridError::Server(_))));

        let mut part = Vec::new();
        part.extend_from_slice(&MAGIC_PART);
        part.extend_from_slice(&[VERSION, 2]);
        part.extend_from_slice(&4u32.to_le_bytes()); // x = 4 at z2
        part.extend_from_slice(&[0; 12]);
        assert!(matches!(decode_any(&gz(&part)), Err(GridError::Server(_))));

        let mut v = encode_cells(0, 4, &[]);
        v = gz(&{
            let mut b = codec::inflate_capped(&v, 1 << 10).unwrap();
            b[4] = 9;
            b
        });
        assert_eq!(decode_any(&v), Err(GridError::BadVersion(9)));
        assert_eq!(decode_any(&gz(b"DGX1 not a tile")), Err(GridError::BadMagic));
        assert_eq!(decode_any(&gz(b"DG")), Err(GridError::Truncated));
    }

    /// Final review #5: a `DGT2` body must end with its last cell. The
    /// same tile without the extra byte decodes.
    #[test]
    fn a_categorical_tile_with_bytes_after_its_last_cell_is_refused() {
        let good = codec::inflate_capped(&encode_cells(0, 64, &[(3, [1, 0, 0, 0, 0, 0, 0, 2])]), 1 << 10).unwrap();
        assert!(decode_any(&gz(&good)).is_ok());
        let mut padded = good.clone();
        padded.push(0);
        assert!(matches!(decode_any(&gz(&padded)), Err(GridError::Server(_))));
    }

    /// Final review #5: a `DGP1` body must end with its last part.
    #[test]
    fn a_part_tile_with_bytes_after_its_last_part_is_refused() {
        let tile = TileId { z: 12, x: 1206, y: 1538 };
        let (x0, y0) = (1206i64 << 19, 1538i64 << 19);
        let p = Part { tri: Tri { a: [x0, y0], b: [x0 + 9, y0], c: [x0, y0 + 9] }, c: [0, 3, 0, 0, 0, 0, 0, 0] };
        let good = codec::inflate_capped(&encode_parts(tile, &[p]), 1 << 10).unwrap();
        assert!(decode_any(&gz(&good)).is_ok());
        let mut padded = good.clone();
        padded.extend_from_slice(&[0; 5]);
        assert!(matches!(decode_any(&gz(&padded)), Err(GridError::Server(_))));
    }

    /// Final review #5: after the first cell a delta of 0 repeats an index,
    /// and is refused; a first cell at index 0 (absolute 0) and a delta of
    /// 1 still decode.
    #[test]
    fn a_repeated_cell_index_is_refused() {
        let body = |second_delta: u8| {
            let mut b = MAGIC_CAT.to_vec();
            b.extend_from_slice(&[VERSION, 0]);
            b.extend_from_slice(&16u32.to_le_bytes());
            b.extend_from_slice(&2u32.to_le_bytes());
            b.extend_from_slice(&[0, 1, 1]); // index 0, group 0, count 1
            b.extend_from_slice(&[second_delta, 1, 1]);
            gz(&b)
        };
        let t = decode_any(&body(1)).unwrap();
        assert_eq!(t.cells.iter().map(|c| c.0).collect::<Vec<_>>(), vec![0, 1]);
        assert!(matches!(decode_any(&body(0)), Err(GridError::Server(_))));
    }

    /// Final review #6: a part tile fetched for another tile is refused by
    /// `decode_any_at`; fetched for its own, it decodes. Cell tiles carry
    /// no tile id and are unaffected.
    #[test]
    fn a_part_tile_under_another_tiles_id_is_refused() {
        let tile = TileId { z: 12, x: 1206, y: 1538 };
        let (x0, y0) = (1206i64 << 19, 1538i64 << 19);
        let p = Part { tri: Tri { a: [x0, y0], b: [x0 + 9, y0], c: [x0, y0 + 9] }, c: [1, 0, 0, 0, 0, 0, 0, 0] };
        let bytes = encode_parts(tile, &[p]);
        assert_eq!(decode_any_at(&bytes, tile).unwrap(), decode_any(&bytes).unwrap());
        for other in [TileId { x: 1207, ..tile }, TileId { y: 1537, ..tile }, TileId { z: 11, x: 603, y: 769 }] {
            assert!(matches!(decode_any_at(&bytes, other), Err(GridError::Server(_))), "{other:?}");
        }
        let cells = encode_cells(0, 16, &[(3, [1, 0, 0, 0, 0, 0, 0, 0])]);
        assert!(decode_any_at(&cells, TileId { z: 9, x: 1, y: 2 }).is_ok());
    }

    /// C7: 255 is never a pyramid level -- a validated descriptor has at
    /// most 255 levels, numbered 0..=254.
    #[test]
    fn the_part_level_is_never_a_pyramid_level() {
        use crate::overlay::dotgrid::{GridDescriptor, GridTileKey};
        let d = GridDescriptor {
            base_arcsec: 3.0,
            origin_lat: 90.0,
            origin_lon: -180.0,
            levels: u8::MAX,
            max_data_zoom: u8::MAX,
            epochs: vec![2020],
            attribution: String::new(),
        };
        assert!(d.validate().is_ok());
        assert!(d.levels - 1 < GridTileKey::PART_LEVEL);
        let k = GridTileKey::part(TileId { z: 12, x: 1, y: 2 }, 2020);
        assert!(k.is_part() && k.epoch_b.is_none());
        assert!(!GridTileKey { level: 0, ..k }.is_part());
    }
}

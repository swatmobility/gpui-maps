//! The hex tile wire format, `HXT1`: one resolution's hexagons in one
//! tile, their counts and baked boundaries, the whole gzipped.
//!
//! Layout (little-endian, before compression):
//!
//! - [`MAGIC`], version u8 = 1, res u8, z u8, x u32, y u32 -- the tile's
//!   resolution and id, checked against the request by the source;
//! - the tile's **hidden** people: a group mask byte and each nonzero
//!   group's count as a varint (0 mask: none). They are the hexagons under
//!   [`HEX_MIN_TOTAL`], folded in by [`encode_tile`] so an archive still
//!   sums to every person without drawing them;
//! - n u32, then n records in strictly ascending H3 order:
//!   - the H3 index as a varint delta from the previous record's (the
//!     first absolute);
//!   - a nonzero-group mask byte and the nonzero counts as varints;
//!   - the vertex count u8 ([`MIN_VERTICES`]..=[`MAX_VERTICES`]);
//!   - the vertices, `[lon, lat]` as i64 quanta of 1e-7° (i32 in range),
//!     each coordinate a zig-zag varint: vertex 0 as a delta from the
//!     previous record's vertex 0 (the first record's from 0°, 0°: an
//!     integer origin, so no platform's float rounding can move a tile); edge `i` (vertex `i` minus vertex `i - 1`) as a residual
//!     against a prediction -- for edges 1..=3 the previous record's same
//!     edge (neighbouring cells of one resolution are nearly congruent),
//!     from edge 4 on the negated edge three back (a hexagon's opposite
//!     edges are nearly antiparallel). The prediction only saves bytes: a
//!     pentagon or a distorted cell still round-trips exactly.
//!
//! **Hardened** (hex tiles come over the network to the web and iOS
//! shells; the `codec_cat`/BGPK bug class): the inflate is capped at
//! [`MAX_INFLATED_BYTES`], every count is bounded before anything is sized
//! by it ([`MAX_HEXES_PER_TILE`], the body's own length, a reservation of
//! at most [`PREALLOC`] records), every arithmetic step on untrusted
//! values is checked, and every short read is [`GridError::Truncated`] --
//! never a panic. Each guard has its own error text and its own test.

use std::io::Write;

use super::{hex_tile_zoom, HexRecord, GROUPS, HEX_MIN_TOTAL};
use crate::geo::TileId;
use crate::overlay::dotgrid::codec::{inflate_capped, write_varint, Reader};
use crate::overlay::dotgrid::GridError;

/// Magic of a hex tile.
pub const MAGIC: [u8; 4] = *b"HXT1";
const VERSION: u8 = 1;

/// Fewest vertices an H3 boundary has (a pentagon).
pub const MIN_VERTICES: usize = 5;
/// Most vertices an H3 boundary has (a distorted pentagon: `h3o`'s
/// `MAX_BNDRY_VERTS`).
pub const MAX_VERTICES: usize = 10;

/// The most hexagons one tile may hold: 2^16. The densest real tile is
/// far below it -- a z3 tile holds at most ~14 k r4 cells of land, a z10
/// tile ~2.1 k r8 cells; the largest in the national archive is 4,105
/// records, ~100 KB inflated.
///
/// **Worst-case memory of one accepted tile: about 31 MiB.** The records
/// take ~14.5 MiB (65,536 × (72 B of `HexRecord` + 10 × 16 B of
/// vertices)), and the inflated body -- up to [`MAX_INFLATED_BYTES`], 16
/// MiB -- stays live alongside them for the whole decode, so the two add
/// (plus the records `Vec`'s last doubling, 32 k -> 64 k, while it grows).
pub const MAX_HEXES_PER_TILE: u32 = 1 << 16;

/// The most one hex tile may inflate to: 16 MiB, [`MAX_HEXES_PER_TILE`]
/// records of the largest possible encoding (~250 B).
pub const MAX_INFLATED_BYTES: u64 = 16 << 20;

/// The fewest bytes a record can take: an H3 delta, a mask, one count
/// (>= 10 fits one byte), the vertex count and 5 vertices of two one-byte
/// coordinates.
const MIN_RECORD_BYTES: u64 = 1 + 1 + 1 + 1 + 2 * MIN_VERTICES as u64;

/// The most records a decoder reserves before any has decoded.
pub const PREALLOC: usize = 1024;

/// Quanta per degree.
const SCALE: f64 = 1e7;
/// A vertex's longitude may sit past ±180° (unwrapped around its cell's
/// centre across the antimeridian) but never by more than a cell's width:
/// ±190° bounds it, and fits an i32.
const MAX_LON_Q: i64 = 1_900_000_000;
const MAX_LAT_Q: i64 = 900_000_000;

/// A `Vec` capacity for `n` untrusted records: at most [`PREALLOC`].
pub fn prealloc(n: u32) -> usize {
    (n as usize).min(PREALLOC)
}

/// One decoded hex tile.
#[derive(Debug, Clone, PartialEq)]
pub struct HexTile {
    pub res: u8,
    pub tile: TileId,
    /// Every hexagon of [`HEX_MIN_TOTAL`] people or more, in H3 order.
    pub records: Vec<HexRecord>,
    /// The people of the tile's hexagons under [`HEX_MIN_TOTAL`], by group.
    pub hidden: [u32; GROUPS],
}

impl HexTile {
    /// Every person the tile carries, drawn or hidden, by group.
    pub fn people(&self) -> [u64; GROUPS] {
        let mut sum = self.hidden.map(u64::from);
        for r in &self.records {
            for (s, &c) in sum.iter_mut().zip(&r.counts) {
                *s += u64::from(c);
            }
        }
        sum
    }
}

fn corrupt(why: &str) -> GridError {
    GridError::Server(format!("corrupt hex tile: {why}"))
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

/// Whether `h3` is laid out as an H3 **cell** index of resolution `res`:
/// reserved bit 0, mode 1, reserved 0, the resolution field, a base cell
/// in 0..=121 and every digit past `res` set to 7 (unused). Not a full
/// validity check (that needs `h3o`, which the shells do not link), but a
/// corrupt or foreign index almost never passes it.
pub fn is_cell_of_res(h3: u64, res: u8) -> bool {
    let field = |shift: u32, bits: u32| (h3 >> shift) & ((1 << bits) - 1);
    if h3 >> 63 != 0 || field(59, 4) != 1 || field(56, 3) != 0 || field(52, 4) != u64::from(res) || res > 15 {
        return false;
    }
    if field(45, 7) > 121 {
        return false;
    }
    (u32::from(res) + 1..=15).all(|d| field(3 * (15 - d), 3) == 7)
}

fn quantise(v: [f64; 2]) -> [i64; 2] {
    let q = [(v[0] * SCALE).round() as i64, (v[1] * SCALE).round() as i64];
    assert!(q[0].unsigned_abs() <= MAX_LON_Q as u64 && q[1].unsigned_abs() <= MAX_LAT_Q as u64, "a hexagon vertex {v:?} off the globe");
    q
}

fn write_counts(buf: &mut Vec<u8>, c: &[u32; GROUPS]) {
    let mask = c.iter().enumerate().fold(0u8, |m, (g, &n)| if n > 0 { m | 1 << g } else { m });
    buf.push(mask);
    for &n in c.iter().filter(|&&n| n > 0) {
        write_varint(buf, u64::from(n));
    }
}

/// One count varint: nonzero and within `u32`.
fn read_count(r: &mut Reader<'_>) -> Result<u32, GridError> {
    let v = r.varint()?;
    u32::try_from(v).ok().filter(|&v| v > 0).ok_or_else(|| corrupt("a group count of 0 or past u32"))
}

/// The prediction of edge `i` (1-based: vertex `i` minus vertex `i - 1`).
fn predict(i: usize, q: &[[i64; 2]], prev_edges: &[[i64; 2]; 3]) -> [i64; 2] {
    if i <= 3 {
        prev_edges[i - 1]
    } else {
        // -(q[i-3] - q[i-4]); every q is bounded, so this cannot overflow.
        [q[i - 4][0] - q[i - 3][0], q[i - 4][1] - q[i - 3][1]]
    }
}

/// Encodes one tile of resolution `res`. Hexagons under [`HEX_MIN_TOTAL`]
/// people are not written: their counts are added to the tile's hidden
/// people. `recs` may come in any order.
///
/// # Panics
/// On a converter bug: `tile` not at `res`'s tile zoom, a record of
/// another resolution, a repeated H3 index, a boundary of fewer than 5 or
/// more than 10 vertices or off the globe, more than [`MAX_HEXES_PER_TILE`]
/// kept hexagons, or hidden people past `u32`.
pub fn encode_tile(res: u8, tile: TileId, recs: &[HexRecord]) -> Vec<u8> {
    assert_eq!(hex_tile_zoom(res), Some(tile.z), "an r{res} tile at z{}", tile.z);
    assert!(u64::from(tile.x) >> tile.z == 0 && u64::from(tile.y) >> tile.z == 0, "tile {tile:?} off the grid");
    let mut hidden = [0u64; GROUPS];
    let mut kept: Vec<&HexRecord> = Vec::new();
    for r in recs {
        assert_eq!(r.res, res, "an r{} hexagon in an r{res} tile", r.res);
        assert!(is_cell_of_res(r.h3, res), "{:#x} is not an r{res} cell", r.h3);
        let total: u64 = r.counts.iter().map(|&c| u64::from(c)).sum();
        assert!(total <= u64::from(u32::MAX), "a hexagon of {total} people");
        if total < u64::from(HEX_MIN_TOTAL) {
            for (h, &c) in hidden.iter_mut().zip(&r.counts) {
                *h += u64::from(c);
            }
        } else {
            let n = r.boundary.len();
            assert!((MIN_VERTICES..=MAX_VERTICES).contains(&n), "a boundary of {n} vertices");
            kept.push(r);
        }
    }
    assert!(kept.len() as u64 <= u64::from(MAX_HEXES_PER_TILE), "{} hexagons in one tile", kept.len());
    kept.sort_by_key(|r| r.h3);
    assert!(kept.windows(2).all(|w| w[0].h3 < w[1].h3), "a repeated H3 index");
    let hidden = hidden.map(|h| u32::try_from(h).expect("hidden people within u32"));

    let mut body = Vec::new();
    body.extend_from_slice(&MAGIC);
    body.extend_from_slice(&[VERSION, res, tile.z]);
    body.extend_from_slice(&tile.x.to_le_bytes());
    body.extend_from_slice(&tile.y.to_le_bytes());
    write_counts(&mut body, &hidden);
    body.extend_from_slice(&(kept.len() as u32).to_le_bytes());
    let mut prev_h3 = 0u64;
    let mut prev_v0 = [0i64; 2];
    let mut prev_edges = [[0i64; 2]; 3];
    for (i, r) in kept.iter().enumerate() {
        write_varint(&mut body, if i == 0 { r.h3 } else { r.h3 - prev_h3 });
        prev_h3 = r.h3;
        write_counts(&mut body, &r.counts);
        body.push(r.boundary.len() as u8);
        let q: Vec<[i64; 2]> = r.boundary.iter().map(|&v| quantise(v)).collect();
        for k in 0..2 {
            write_varint(&mut body, zigzag(q[0][k] - prev_v0[k]));
        }
        prev_v0 = q[0];
        for i in 1..q.len() {
            let p = predict(i, &q, &prev_edges);
            for k in 0..2 {
                write_varint(&mut body, zigzag(q[i][k] - q[i - 1][k] - p[k]));
            }
        }
        for (i, e) in prev_edges.iter_mut().enumerate() {
            *e = [q[i + 1][0] - q[i][0], q[i + 1][1] - q[i][1]];
        }
    }
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    gz.write_all(&body).expect("gzip write to a Vec sink never fails");
    gz.finish().expect("gzip finish on a Vec sink never fails")
}

/// Decodes one hex tile. Every short read is [`GridError::Truncated`],
/// foreign bytes [`GridError::BadMagic`], anything else malformed a
/// [`GridError::Server`] naming the guard -- never a panic.
pub fn decode(bytes: &[u8]) -> Result<HexTile, GridError> {
    let body = inflate_capped(bytes, MAX_INFLATED_BYTES)?;
    decode_body(&body)
}

/// [`decode`] on an inflated body.
pub fn decode_body(body: &[u8]) -> Result<HexTile, GridError> {
    let mut r = Reader::new(body);
    if r.take(4)? != MAGIC {
        return Err(GridError::BadMagic);
    }
    let version = r.u8()?;
    if version != VERSION {
        return Err(GridError::BadVersion(version));
    }
    let res = r.u8()?;
    let z = r.u8()?;
    let x = r.u32_le()?;
    let y = r.u32_le()?;
    if hex_tile_zoom(res) != Some(z) {
        return Err(corrupt("a resolution that is not drawn, or a tile not at its zoom"));
    }
    if u64::from(x) >> z != 0 || u64::from(y) >> z != 0 {
        return Err(corrupt("a tile id off the world grid"));
    }
    let tile = TileId { z, x, y };

    let hidden_mask = r.u8()?;
    let mut hidden = [0u32; GROUPS];
    for (g, h) in hidden.iter_mut().enumerate() {
        if hidden_mask & (1 << g) != 0 {
            *h = read_count(&mut r)?;
        }
    }

    let n = r.u32_le()?;
    if n > MAX_HEXES_PER_TILE {
        return Err(corrupt("more hexagons than a tile holds"));
    }
    if u64::from(n) * MIN_RECORD_BYTES > r.remaining() as u64 {
        return Err(GridError::Truncated);
    }
    let mut records = Vec::with_capacity(prealloc(n));
    let mut prev_h3 = 0u64;
    let mut prev_v0 = [0i64; 2];
    let mut prev_edges = [[0i64; 2]; 3];
    for i in 0..n {
        let d = r.varint()?;
        let h3 = if i == 0 {
            d
        } else {
            prev_h3.checked_add(d).filter(|_| d > 0).ok_or_else(|| corrupt("hexagons not in ascending H3 order"))?
        };
        if !is_cell_of_res(h3, res) {
            return Err(corrupt("an index that is not an H3 cell of the tile's resolution"));
        }
        prev_h3 = h3;

        let mask = r.u8()?;
        if mask == 0 {
            return Err(corrupt("a hexagon with no people"));
        }
        let mut counts = [0u32; GROUPS];
        let mut total = 0u32;
        for (g, c) in counts.iter_mut().enumerate() {
            if mask & (1 << g) != 0 {
                *c = read_count(&mut r)?;
                total = total.checked_add(*c).ok_or_else(|| corrupt("a hexagon's people past u32"))?;
            }
        }
        if total < HEX_MIN_TOTAL {
            return Err(corrupt("a hexagon under the 10-person minimum"));
        }

        let nv = usize::from(r.u8()?);
        if !(MIN_VERTICES..=MAX_VERTICES).contains(&nv) {
            return Err(corrupt("a boundary vertex count outside 5..=10"));
        }
        let mut q = [[0i64; 2]; MAX_VERTICES];
        for vi in 0..nv {
            let (base, pred) =
                if vi == 0 { (prev_v0, [0, 0]) } else { (q[vi - 1], predict(vi, &q[..vi], &prev_edges)) };
            for k in 0..2 {
                let d = unzigzag(r.varint()?);
                q[vi][k] = base[k]
                    .checked_add(pred[k])
                    .and_then(|v| v.checked_add(d))
                    .filter(|v| v.unsigned_abs() <= if k == 0 { MAX_LON_Q } else { MAX_LAT_Q } as u64)
                    .ok_or_else(|| corrupt("a vertex off the globe"))?;
            }
        }
        prev_v0 = q[0];
        for (i, e) in prev_edges.iter_mut().enumerate() {
            *e = [q[i + 1][0] - q[i][0], q[i + 1][1] - q[i][1]];
        }
        let boundary = q[..nv].iter().map(|v| [v[0] as f64 / SCALE, v[1] as f64 / SCALE]).collect();
        records.push(HexRecord { res, h3, counts, boundary });
    }
    if r.remaining() != 0 {
        return Err(corrupt("bytes after the last hexagon"));
    }
    Ok(HexTile { res, tile, records, hidden })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A well-formed r`res` cell index (no `h3o` here): base cell `base`,
    /// digits from `n`'s base-7 expansion, the unused digits 7.
    pub(crate) fn fake_cell(res: u8, base: u64, n: u64) -> u64 {
        let mut h = 1u64 << 59 | u64::from(res) << 52 | base << 45;
        let mut n = n;
        for d in 1..=15u32 {
            let digit = if d <= u32::from(res) {
                let v = n % 7;
                n /= 7;
                v
            } else {
                7
            };
            h |= digit << (3 * (15 - d));
        }
        h
    }

    /// A regular hexagon of `r` degrees around `(lon, lat)`.
    pub(crate) fn hexagon(lon: f64, lat: f64, r: f64, n: usize) -> Vec<[f64; 2]> {
        (0..n)
            .map(|i| {
                let a = std::f64::consts::TAU * i as f64 / n as f64;
                [lon + r * a.cos(), lat + r * a.sin()]
            })
            .collect()
    }

    pub(crate) fn rec(res: u8, counts: [u32; GROUPS]) -> HexRecord {
        rec_at(res, 0, counts)
    }

    pub(crate) fn rec_at(res: u8, i: u64, counts: [u32; GROUPS]) -> HexRecord {
        let (lon, lat) = (-73.97 + 0.01 * i as f64, 40.78 + 0.003 * i as f64);
        HexRecord { res, h3: fake_cell(res, 20, 3 * i + 1), counts, boundary: hexagon(lon, lat, 0.02, 6) }
    }

    pub(crate) fn nyc(res: u8) -> TileId {
        super::super::owning_tile(res, -73.97, 40.78).unwrap()
    }

    fn gz(body: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(body).unwrap();
        e.finish().unwrap()
    }

    fn inflated(res: u8, recs: &[HexRecord]) -> Vec<u8> {
        inflate_capped(&encode_tile(res, nyc(res), recs), MAX_INFLATED_BYTES).unwrap()
    }

    /// The header of an r7 NYC tile with `hidden` absent and `n` records.
    fn header(n: u32) -> Vec<u8> {
        let t = nyc(7);
        let mut b = MAGIC.to_vec();
        b.extend_from_slice(&[VERSION, 7, t.z]);
        b.extend_from_slice(&t.x.to_le_bytes());
        b.extend_from_slice(&t.y.to_le_bytes());
        b.push(0);
        b.extend_from_slice(&n.to_le_bytes());
        b
    }

    /// One record body after `header`: `h3`, mask, count, `nv` vertices
    /// of zero deltas (so each coordinate is one byte) from the corner.
    fn record(h3: u64, mask: u8, count: u64, nv: u8) -> Vec<u8> {
        let mut b = Vec::new();
        write_varint(&mut b, h3);
        b.push(mask);
        if mask != 0 {
            write_varint(&mut b, count);
        }
        b.push(nv);
        b.extend(std::iter::repeat_n(0u8, 2 * usize::from(nv)));
        b
    }

    fn err_text(r: Result<HexTile, GridError>) -> String {
        match r {
            Err(GridError::Server(s)) => s,
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn hex_tiles_round_trip_and_drop_totals_under_ten() {
        let recs = vec![rec_at(7, 0, [9, 0, 0, 0, 0, 0, 0, 0]), rec_at(7, 1, [6, 4, 0, 0, 0, 0, 0, 0])];
        let out = decode(&encode_tile(7, nyc(7), &recs)).unwrap();
        assert_eq!(out.records.len(), 1);
        assert_eq!(out.records[0].counts, [6, 4, 0, 0, 0, 0, 0, 0]);
        // The hidden hexagon's people are still carried.
        assert_eq!(out.hidden, [9, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(out.people(), [15, 4, 0, 0, 0, 0, 0, 0]);
        assert_eq!((out.res, out.tile), (7, nyc(7)));
    }

    #[test]
    fn boundaries_round_trip_to_the_quantum_in_any_record_order() {
        let mut recs: Vec<HexRecord> = (0..40).map(|i| rec_at(8, i, [10 + i as u32, 0, 3, 0, 0, 0, 0, 1])).collect();
        recs[5].boundary = hexagon(-73.9, 40.7, 0.01, 5); // a pentagon
        recs[9].boundary = hexagon(-73.8, 40.8, 0.01, 10); // a distorted cell's 10
        recs[11].boundary = hexagon(-73.8, 40.9, 0.01, 8);
        recs.reverse();
        let out = decode(&encode_tile(8, nyc(8), &recs)).unwrap();
        recs.sort_by_key(|r| r.h3);
        assert_eq!(out.records.len(), recs.len());
        for (a, b) in out.records.iter().zip(&recs) {
            assert_eq!((a.res, a.h3, a.counts), (b.res, b.h3, b.counts));
            assert_eq!(a.boundary.len(), b.boundary.len());
            for (p, q) in a.boundary.iter().zip(&b.boundary) {
                assert!((p[0] - q[0]).abs() <= 0.51e-7 && (p[1] - q[1]).abs() <= 0.51e-7, "{p:?} vs {q:?}");
            }
        }
    }

    #[test]
    fn an_antimeridian_boundary_past_180_round_trips() {
        let mut r = rec(4, [50, 0, 0, 0, 0, 0, 0, 0]);
        r.boundary = hexagon(179.9, 51.8, 0.3, 6);
        let t = TileId { z: 3, x: 7, y: 2 };
        let out = decode(&encode_tile(4, t, &[r.clone()])).unwrap();
        assert!(out.records[0].boundary.iter().any(|v| v[0] > 180.0));
        assert!((out.records[0].boundary[0][0] - r.boundary[0][0]).abs() < 1e-7);
    }

    #[test]
    fn an_empty_tile_round_trips() {
        let out = decode(&encode_tile(5, nyc(5), &[])).unwrap();
        assert!(out.records.is_empty());
        assert_eq!(out.hidden, [0; GROUPS]);
    }

    #[test]
    fn a_truncated_hex_tile_is_an_error_not_a_panic() {
        let b = encode_tile(6, nyc(6), &[rec(6, [10, 0, 0, 0, 0, 0, 0, 0])]);
        for n in 0..b.len() {
            assert!(decode(&b[..n]).is_err(), "{n}");
        }
        let body = inflate_capped(&b, MAX_INFLATED_BYTES).unwrap();
        for n in 0..body.len() {
            assert!(decode_body(&body[..n]).is_err(), "{n}");
        }
    }

    /// The corruption sweep: every byte of a real tile's body, flipped four
    /// ways, then 20,000 seeded random mutations -- errors or tiles, never
    /// a panic, and never a record the guards should have refused.
    #[test]
    fn corrupt_hex_tiles_never_panic() {
        let mut recs: Vec<HexRecord> = (0..12).map(|i| rec_at(7, i, [10 + i as u32, 2, 0, 0, 7, 0, 0, 1])).collect();
        recs.push(rec_at(7, 20, [3, 0, 0, 0, 0, 0, 0, 0]));
        recs[3].boundary = hexagon(-73.5, 40.5, 0.01, 8);
        let body = inflated(7, &recs);
        let check = |b: &[u8]| {
            if let Ok(t) = decode_body(b) {
                for r in &t.records {
                    assert!(r.total() >= HEX_MIN_TOTAL && (5..=10).contains(&r.boundary.len()));
                    assert!(is_cell_of_res(r.h3, t.res));
                    assert!(r.boundary.iter().all(|v| v[0].abs() <= 190.0 && v[1].abs() <= 90.0));
                }
            }
        };
        for i in 0..body.len() {
            for f in [|b: u8| b ^ 0x01, |b: u8| b ^ 0x80, |_| 0xFF, |_| 0x00] {
                let mut b = body.clone();
                b[i] = f(b[i]);
                check(&b);
                assert!(decode(&gz(&b)).is_ok() == decode_body(&b).is_ok());
            }
        }
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        for _ in 0..20_000 {
            let mut b = body.clone();
            for _ in 0..1 + next() % 6 {
                let i = (next() % b.len() as u64) as usize;
                b[i] = next() as u8;
            }
            if next() % 4 == 0 {
                let cut = (next() % b.len() as u64) as usize;
                b.truncate(cut);
            }
            check(&b);
        }
        // Pure noise after a valid header, and noise alone.
        for len in [0usize, 1, 17, 64, 1000] {
            let mut b = header(3);
            b.extend((0..len).map(|_| next() as u8));
            check(&b);
            let noise: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            check(&noise);
            let _ = decode(&noise);
        }
    }

    // One test per guard. Each asserts the error only its guard gives, so
    // removing the guard fails the test (the decode then fails later, and
    // differently, or succeeds).

    #[test]
    fn guard_inflate_cap() {
        let bomb = gz(&vec![0u8; MAX_INFLATED_BYTES as usize + 1]);
        assert!(matches!(decode(&bomb), Err(GridError::Inflate(e)) if e.contains("limit")));
    }

    #[test]
    fn guard_magic_and_version() {
        assert_eq!(decode(&gz(b"DGT2 not a hex tile")), Err(GridError::BadMagic));
        let mut b = header(0);
        b[4] = 9;
        assert_eq!(decode(&gz(&b)), Err(GridError::BadVersion(9)));
    }

    #[test]
    fn guard_resolution_and_tile_zoom() {
        let mut b = header(0);
        b[5] = 9; // r9 is not drawn
        assert!(err_text(decode_body(&b)).contains("not at its zoom"));
        let mut b = header(0);
        b[6] = 10; // r7 at z10
        assert!(err_text(decode_body(&b)).contains("not at its zoom"));
    }

    #[test]
    fn guard_tile_off_the_grid() {
        let mut b = header(0);
        b[7..11].copy_from_slice(&512u32.to_le_bytes()); // x = 512 at z9
        assert!(err_text(decode_body(&b)).contains("off the world grid"));
    }

    #[test]
    fn guard_hidden_count() {
        let mut b = header(0);
        b[15] = 1; // hidden mask: group 0
        b.truncate(16);
        // Past u32 by 5: a truncating read would see a valid count of 4.
        write_varint(&mut b, u64::from(u32::MAX) + 5);
        b.extend_from_slice(&0u32.to_le_bytes());
        assert!(err_text(decode_body(&b)).contains("past u32"));
    }

    #[test]
    fn guard_hexagons_per_tile() {
        // A claim past the maximum, over a body that could hold it.
        let mut b = header(MAX_HEXES_PER_TILE + 1);
        for i in 0..=MAX_HEXES_PER_TILE {
            b.extend(record(if i == 0 { fake_cell(7, 20, 1) } else { 1 }, 1, 10, 5));
        }
        assert!(err_text(decode_body(&b)).contains("more hexagons than a tile holds"));
    }

    #[test]
    fn guard_count_against_the_body() {
        // 1,000 claimed, one (corrupt) record present: refused before any
        // record is read, so the error is Truncated, not the record's.
        let mut b = header(1000);
        b.extend(record(fake_cell(7, 20, 1), 0, 0, 5));
        assert_eq!(decode_body(&b), Err(GridError::Truncated));
    }

    #[test]
    fn guard_reservation() {
        assert_eq!(prealloc(u32::MAX), PREALLOC);
        assert_eq!(prealloc(MAX_HEXES_PER_TILE), PREALLOC);
        assert_eq!(prealloc(3), 3);
        // The peak of an accepted worst tile: the records AND the inflated
        // body they were decoded from, live together (review F1): ~31 MiB.
        let records = MAX_HEXES_PER_TILE as usize * (size_of::<HexRecord>() + MAX_VERTICES * size_of::<[f64; 2]>());
        let peak = records + MAX_INFLATED_BYTES as usize;
        assert!(peak <= 32 << 20, "{peak}");
        assert!(peak >= 30 << 20, "the doc's ~31 MiB is stale: {peak}");
    }

    #[test]
    fn guard_ascending_h3() {
        let mut b = header(2);
        b.extend(record(fake_cell(7, 20, 1), 1, 10, 5));
        b.extend(record(0, 1, 10, 5)); // a repeat
        assert!(err_text(decode_body(&b)).contains("ascending"));
        let mut b = header(2);
        b.extend(record(fake_cell(7, 20, 1), 1, 10, 5));
        b.extend(record(u64::MAX, 1, 10, 5)); // wraps
        assert!(err_text(decode_body(&b)).contains("ascending"));
    }

    #[test]
    fn guard_h3_cell_of_the_resolution() {
        for h3 in [fake_cell(8, 20, 1), fake_cell(7, 122, 1), fake_cell(7, 20, 1) | 1 << 63, fake_cell(7, 20, 1) & !7] {
            let mut b = header(1);
            b.extend(record(h3, 1, 10, 5));
            assert!(err_text(decode_body(&b)).contains("not an H3 cell"), "{h3:#x}");
        }
        assert!(is_cell_of_res(fake_cell(7, 20, 1), 7));
    }

    #[test]
    fn guard_no_people() {
        let mut b = header(1);
        b.extend(record(fake_cell(7, 20, 1), 0, 0, 5));
        assert!(err_text(decode_body(&b)).contains("no people"));
    }

    #[test]
    fn guard_group_count() {
        let mut b = header(1);
        b.extend(record(fake_cell(7, 20, 1), 1, 0, 5));
        assert!(err_text(decode_body(&b)).contains("a group count of 0"));
    }

    #[test]
    fn guard_total_past_u32() {
        let mut b = header(1);
        write_varint(&mut b, fake_cell(7, 20, 1));
        b.push(0b11);
        write_varint(&mut b, u64::from(u32::MAX));
        write_varint(&mut b, 1);
        b.push(5);
        b.extend([0u8; 10]);
        assert!(err_text(decode_body(&b)).contains("people past u32"));
    }

    #[test]
    fn guard_minimum_total() {
        let mut b = header(1);
        b.extend(record(fake_cell(7, 20, 1), 1, 9, 5));
        assert!(err_text(decode_body(&b)).contains("10-person minimum"));
    }

    #[test]
    fn guard_vertex_count() {
        for nv in [0u8, 4, 11, 255] {
            let mut b = header(1);
            b.extend(record(fake_cell(7, 20, 1), 1, 10, nv));
            b.extend([0u8; 16]);
            assert!(err_text(decode_body(&b)).contains("vertex count"), "{nv}");
        }
    }

    #[test]
    fn guard_vertex_off_the_globe() {
        for (k, delta) in [(1usize, 90.1f64), (0, 190.1), (0, -1e12)] {
            let mut b = header(1);
            write_varint(&mut b, fake_cell(7, 20, 1));
            b.extend([1, 10, 5]);
            for kk in 0..2 {
                write_varint(&mut b, zigzag(if kk == k { (delta * SCALE) as i64 } else { 0 }));
            }
            b.extend([0u8; 8]);
            assert!(err_text(decode_body(&b)).contains("off the globe"), "{k} {delta}");
        }
        // A huge residual that would overflow i64 without the checked add.
        let mut b = header(1);
        write_varint(&mut b, fake_cell(7, 20, 1));
        b.extend([1, 10, 5]);
        write_varint(&mut b, u64::MAX);
        b.extend([0u8; 9]);
        assert!(err_text(decode_body(&b)).contains("off the globe"));
    }

    #[test]
    fn guard_trailing_bytes() {
        let mut b = inflated(7, &[rec(7, [10, 0, 0, 0, 0, 0, 0, 0])]);
        b.push(0);
        assert!(err_text(decode_body(&b)).contains("after the last hexagon"));
    }

    #[test]
    fn real_sized_records_are_compact() {
        // 2,000 r8-sized cells on a lattice: well under 30 B a record
        // gzipped (the archive budget, MG-R12, is 100 MB for ~2.7 M).
        let recs: Vec<HexRecord> = (0..2000u64)
            .map(|i| {
                let (lon, lat) = (-74.0 + 0.009 * (i % 50) as f64, 40.5 + 0.008 * (i / 50) as f64);
                HexRecord {
                    res: 8,
                    h3: fake_cell(8, 20, 5 * i + 2),
                    counts: [100 + i as u32 % 900, 40, 12, 0, 30, 0, 2, 9],
                    boundary: hexagon(lon, lat, 0.0048, 6),
                }
            })
            .collect();
        let bytes = encode_tile(8, nyc(8), &recs);
        assert!(bytes.len() < 30 * recs.len(), "{} B for {} records", bytes.len(), recs.len());
        assert_eq!(decode(&bytes).unwrap().records.len(), 2000);
    }
}

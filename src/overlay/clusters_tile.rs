//! The race map's cluster tiles, `CLT1`: the donut view's offline cluster
//! hierarchy (spec `2026-09-27-us-race-dot-map-design.md` §5.1), one
//! zoom's clusters and leaves in one tile, the whole gzipped.
//!
//! `tools/race-build export-clusters` runs the engine's own Supercluster
//! port ([`crate::overlay::cluster::ClusterIndex`], radius mode, 50 px of
//! a 512 px tile, `max_zoom` 11, 9 sums) over the populated blocks'
//! interior points and writes, for every zoom [`MIN_ZOOM`]..=[`MAX_ZOOM`],
//! every item of that zoom -- cluster or leaf block -- into the tile of
//! that zoom holding its position. So each zoom's tiles carry all
//! 331,449,281 people, and a tile holds only its own zoom's items.
//!
//! Layout (little-endian, before compression):
//!
//! - [`MAGIC`], version u8 = 1, n u32;
//! - n records, each:
//!   - flags u8 ([`FLAG_LEAF`]; every other bit 0);
//!   - `expansion_zoom` u8, 1..=[`DOTS_ZOOM`];
//!   - lon, lat: i64 quanta of 1e-7°, each a zig-zag varint delta from the
//!     previous record's (the first from 0°, 0°: an integer origin, so no
//!     platform's float rounding can move a tile);
//!   - `blocks` varint (a leaf: 1; a cluster: at least 2);
//!   - `pop` varint (at least 1);
//!   - a nonzero-group mask byte and each nonzero group count as a varint;
//!     the groups sum to at most `pop`.
//!
//! **Hardened** (cluster tiles come over the network to the web and iOS
//! shells; the `codec_cat`/BGPK/`HXT1` bug class): the inflate is capped
//! at [`MAX_INFLATED_BYTES`], the record count is bounded before anything
//! is sized by it ([`MAX_RECORDS_PER_TILE`], then the body's own length at
//! [`MIN_RECORD_BYTES`] a record, then a reservation of at most
//! [`PREALLOC`] records), every arithmetic step on untrusted values is
//! checked, and every short read is [`GridError::Truncated`] -- never a
//! panic. Each guard has its own error text and its own test.
//!
//! **Worst-case memory of one accepted tile: 896 KiB** (917,504 B),
//! measured by `tests/cluster_tile_peak_memory.rs` under a counting
//! allocator. The inflated body is at most [`MAX_INFLATED_BYTES`] (512
//! KiB) -- `inflate_capped` reserves exactly, so its capacity is the
//! body's bound too, not the next doubling -- and stays live for the whole
//! decode; the records are at most [`MAX_RECORDS_PER_TILE`] × 64 B
//! (`size_of::<ClusterRecord>()`) = 256 KiB, and the records `Vec`'s last
//! doubling (2,048 -> 4,096) holds the old 128 KiB and the new 256 KiB at
//! once: 512 + 128 + 256 = 896 KiB. The inflate's own peak (the body's
//! last growth, 256 + 512 KiB, plus the gzip state) is below that.
//! The caller's compressed bytes are on top, and bounded by the fetch.

use std::io::Write;

use crate::overlay::dotgrid::codec::{inflate_capped, write_varint, Reader};
use crate::overlay::dotgrid::GridError;

/// Groups per record: the race map's 8.
pub const GROUPS: usize = crate::overlay::dotgrid::composition::GROUPS;

/// Magic of a cluster tile.
pub const MAGIC: [u8; 4] = *b"CLT1";
const VERSION: u8 = 1;

/// The first zoom with cluster tiles (Walker's cluster layer starts at z3).
pub const MIN_ZOOM: u8 = 3;
/// The last zoom with cluster tiles: the index's `max_zoom`.
pub const MAX_ZOOM: u8 = 11;
/// Where the dots take over in cluster mode (Walker:
/// `setLayerZoomRange('people', 12, 24)`), and so the largest expansion
/// zoom: a z11 cluster breaks into the raw blocks at z12. A leaf's
/// expansion zoom is this too -- a click on a block zooms to its dots.
pub const DOTS_ZOOM: u8 = 12;
/// The index's cluster radius, in pixels of a [`crate::overlay::cluster::EXTENT`]
/// (512) px tile: MapLibre's default.
pub const RADIUS_PX: f64 = 50.0;

/// [`ClusterRecord::leaf`] in the flags byte.
pub const FLAG_LEAF: u8 = 1;

/// The most records one tile may hold: 2^12. The densest real tile is
/// far below it: the national archive's largest holds 92 records (z11;
/// `export-clusters` reports `max_per_tile`), since at 50 px of a 512 px
/// tile the items of one zoom sit about a radius apart -- on the order of
/// (512 / 50)^2 a tile. 44x headroom.
pub const MAX_RECORDS_PER_TILE: u32 = 1 << 12;

/// The most one cluster tile may inflate to: 512 KiB, above
/// [`MAX_RECORDS_PER_TILE`] records of the largest encoding (63 B: flags,
/// zoom, two 5-byte coordinates, 5-byte blocks and pop, the mask and 8
/// 5-byte counts -- 252 KiB) plus the header. The largest real tile
/// inflates to ~2 KB.
pub const MAX_INFLATED_BYTES: u64 = 512 << 10;

/// The fewest bytes a record can take: flags, zoom, two one-byte
/// coordinates, blocks, pop, the mask and one count.
pub const MIN_RECORD_BYTES: u64 = 8;

/// The most records a decoder reserves before any has decoded.
pub const PREALLOC: usize = 1024;

/// Quanta per degree.
const SCALE: f64 = 1e7;
const MAX_LON_Q: i64 = 1_800_000_000;
const MAX_LAT_Q: i64 = 900_000_000;

/// A `Vec` capacity for `n` untrusted records: at most [`PREALLOC`].
pub fn prealloc(n: u32) -> usize {
    (n as usize).min(PREALLOC)
}

/// One cluster or leaf block of one zoom.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClusterRecord {
    /// The block-count-weighted centre (Supercluster's), or the leaf
    /// block's interior point, in degrees (on the 1e-7° quantum).
    pub lon: f64,
    pub lat: f64,
    /// Supercluster's `point_count`: blocks in the cluster (1 for a leaf).
    pub blocks: u32,
    /// People: the `pop` sum.
    pub pop: u32,
    /// People per group (the `c1`..`c8` sums).
    pub counts: [u32; GROUPS],
    /// Supercluster's `getClusterExpansionZoom`: the zoom a click eases
    /// to. [`DOTS_ZOOM`] for a leaf.
    pub expansion_zoom: u8,
    /// An unclustered block.
    pub leaf: bool,
}

fn corrupt(why: &str) -> GridError {
    GridError::Server(format!("corrupt cluster tile: {why}"))
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

/// `v` degrees as quanta.
pub fn quantise(v: f64) -> i64 {
    (v * SCALE).round() as i64
}

/// `v` degrees snapped to the 1e-7° quantum, exactly as a decode returns
/// it (the exporter snaps its records so they compare equal read back).
pub fn snap(v: f64) -> f64 {
    quantise(v) as f64 / SCALE
}

/// Encodes one tile's records, in the order given.
///
/// # Panics
/// On an exporter bug: more than [`MAX_RECORDS_PER_TILE`] records, or a
/// record [`decode`] would refuse (a non-finite or off-globe position, an expansion zoom
/// outside 1..=[`DOTS_ZOOM`], a leaf of other than 1 block or a cluster
/// of fewer than 2, no people, or groups past `pop`).
pub fn encode(recs: &[ClusterRecord]) -> Vec<u8> {
    assert!(recs.len() as u64 <= u64::from(MAX_RECORDS_PER_TILE), "{} records in one tile", recs.len());
    let mut body = Vec::with_capacity(9 + recs.len() * 24);
    body.extend_from_slice(&MAGIC);
    body.push(VERSION);
    body.extend_from_slice(&(recs.len() as u32).to_le_bytes());
    let mut prev = [0i64; 2];
    for r in recs {
        assert!((1..=DOTS_ZOOM).contains(&r.expansion_zoom), "expansion zoom {}", r.expansion_zoom);
        assert!(if r.leaf { r.blocks == 1 } else { r.blocks >= 2 }, "a {} of {} blocks", if r.leaf { "leaf" } else { "cluster" }, r.blocks);
        assert!(r.pop >= 1, "a record with no people");
        let groups: u64 = r.counts.iter().map(|&c| u64::from(c)).sum();
        assert!(groups >= 1 && groups <= u64::from(r.pop), "groups {groups} against pop {}", r.pop);
        // `quantise(NaN)` is 0: without this a NaN would be written at 0°, 0°.
        assert!(r.lon.is_finite() && r.lat.is_finite(), "a record at {}, {} is not a position", r.lon, r.lat);
        let q = [quantise(r.lon), quantise(r.lat)];
        assert!(q[0].unsigned_abs() <= MAX_LON_Q as u64 && q[1].unsigned_abs() <= MAX_LAT_Q as u64, "a record at {}, {} off the globe", r.lon, r.lat);
        body.push(if r.leaf { FLAG_LEAF } else { 0 });
        body.push(r.expansion_zoom);
        for k in 0..2 {
            write_varint(&mut body, zigzag(q[k] - prev[k]));
        }
        prev = q;
        write_varint(&mut body, u64::from(r.blocks));
        write_varint(&mut body, u64::from(r.pop));
        let mask = r.counts.iter().enumerate().fold(0u8, |m, (g, &n)| if n > 0 { m | 1 << g } else { m });
        body.push(mask);
        for &n in r.counts.iter().filter(|&&n| n > 0) {
            write_varint(&mut body, u64::from(n));
        }
    }
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    gz.write_all(&body).expect("gzip write to a Vec sink never fails");
    gz.finish().expect("gzip finish on a Vec sink never fails")
}

/// One varint within `u32`, at least `min`.
fn read_u32(r: &mut Reader<'_>, min: u32, why: &str) -> Result<u32, GridError> {
    let v = r.varint()?;
    u32::try_from(v).ok().filter(|&v| v >= min).ok_or_else(|| corrupt(why))
}

/// Decodes one cluster tile. Every short read is [`GridError::Truncated`],
/// foreign bytes [`GridError::BadMagic`], anything else malformed a
/// [`GridError::Server`] naming the guard -- never a panic.
///
/// **The tile does not know its zoom**, so this accepts any expansion
/// zoom in 1..=[`DOTS_ZOOM`]. The caller, which knows the requested `z`,
/// must refuse a tile holding a cluster whose `expansion_zoom <= z` (a
/// click on it would zoom out, or stay put), and may check each record's
/// position lies in the requested tile (Task 14's source).
pub fn decode(b: &[u8]) -> Result<Vec<ClusterRecord>, GridError> {
    let body = inflate_capped(b, MAX_INFLATED_BYTES)?;
    decode_body(&body)
}

/// [`decode`] on an inflated body.
pub fn decode_body(body: &[u8]) -> Result<Vec<ClusterRecord>, GridError> {
    let mut r = Reader::new(body);
    if r.take(4)? != MAGIC {
        return Err(GridError::BadMagic);
    }
    let version = r.u8()?;
    if version != VERSION {
        return Err(GridError::BadVersion(version));
    }
    let n = r.u32_le()?;
    if n > MAX_RECORDS_PER_TILE {
        return Err(corrupt("more records than a tile holds"));
    }
    if u64::from(n) * MIN_RECORD_BYTES > r.remaining() as u64 {
        return Err(GridError::Truncated);
    }
    let mut out = Vec::with_capacity(prealloc(n));
    let mut prev = [0i64; 2];
    for _ in 0..n {
        let flags = r.u8()?;
        if flags & !FLAG_LEAF != 0 {
            return Err(corrupt("unknown flag bits"));
        }
        let leaf = flags & FLAG_LEAF != 0;
        let expansion_zoom = r.u8()?;
        if !(1..=DOTS_ZOOM).contains(&expansion_zoom) {
            return Err(corrupt("an expansion zoom outside 1..=12"));
        }
        let mut q = [0i64; 2];
        for k in 0..2 {
            let d = unzigzag(r.varint()?);
            let bound = if k == 0 { MAX_LON_Q } else { MAX_LAT_Q };
            q[k] = prev[k].checked_add(d).filter(|v| v.unsigned_abs() <= bound as u64).ok_or_else(|| corrupt("a position off the globe"))?;
        }
        prev = q;
        let blocks = read_u32(&mut r, 1, "a block count of 0 or past u32")?;
        if leaf != (blocks == 1) {
            return Err(corrupt("a leaf of more than one block, or a cluster of one"));
        }
        let pop = read_u32(&mut r, 1, "a population of 0 or past u32")?;
        let mask = r.u8()?;
        if mask == 0 {
            return Err(corrupt("a record with no group people"));
        }
        let mut counts = [0u32; GROUPS];
        let mut groups = 0u32;
        for (g, c) in counts.iter_mut().enumerate() {
            if mask & (1 << g) != 0 {
                *c = read_u32(&mut r, 1, "a group count of 0 or past u32")?;
                groups = groups.checked_add(*c).filter(|&s| s <= pop).ok_or_else(|| corrupt("groups past the population"))?;
            }
        }
        out.push(ClusterRecord {
            lon: q[0] as f64 / SCALE,
            lat: q[1] as f64 / SCALE,
            blocks,
            pop,
            counts,
            expansion_zoom,
            leaf,
        });
    }
    if r.remaining() != 0 {
        return Err(corrupt("bytes after the last record"));
    }
    Ok(out)
}

/// The `race_clusters` metadata key's version this build reads.
pub const CLUSTER_ARCHIVE_VERSION: u64 = 1;

/// The JSON metadata [`parse_cluster_meta`] accepts (the exporter writes
/// it).
pub fn cluster_meta_json(attribution: &str) -> serde_json::Value {
    serde_json::json!({
        "race_clusters": {
            "version": CLUSTER_ARCHIVE_VERSION,
            "groups": GROUPS,
            "min_zoom": MIN_ZOOM,
            "max_zoom": MAX_ZOOM,
            "dots_zoom": DOTS_ZOOM,
            "radius_px": RADIUS_PX,
        },
        "attribution": attribution,
    })
}

/// What a cluster archive's metadata describes.
#[derive(Debug, Clone, PartialEq)]
pub struct ClusterArchiveMeta {
    pub attribution: String,
}

/// Reads and checks the `race_clusters` metadata key (a
/// `dotgrid::pmtiles_source::ArchiveOpener` parse function): an archive
/// cut at other zooms than this build's is refused at open.
pub fn parse_cluster_meta(meta_bytes: &[u8], compression: u8) -> Result<ClusterArchiveMeta, GridError> {
    let raw = crate::overlay::dotgrid::pmtiles_dir::decompress(meta_bytes, compression)?;
    let root: serde_json::Value =
        serde_json::from_slice(&raw).map_err(|e| GridError::Server(format!("archive metadata is not JSON: {e}")))?;
    let meta = root
        .get("race_clusters")
        .ok_or_else(|| GridError::Server("archive metadata has no \"race_clusters\" key".to_string()))?;
    let int = |name: &str| meta.get(name).and_then(serde_json::Value::as_u64);
    if int("version") != Some(CLUSTER_ARCHIVE_VERSION) {
        return Err(GridError::Server(format!("unsupported race_clusters version {:?}", meta.get("version"))));
    }
    let want = [("groups", GROUPS as u64), ("min_zoom", MIN_ZOOM.into()), ("max_zoom", MAX_ZOOM.into()), ("dots_zoom", DOTS_ZOOM.into())];
    if want.iter().any(|&(k, v)| int(k) != Some(v)) {
        return Err(GridError::Server("race_clusters groups or zooms differ from this build's".to_string()));
    }
    let attribution = root.get("attribution").and_then(serde_json::Value::as_str).unwrap_or_default().to_string();
    Ok(ClusterArchiveMeta { attribution })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gz(body: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(body).unwrap();
        e.finish().unwrap()
    }

    fn rec(i: u32) -> ClusterRecord {
        let leaf = i.is_multiple_of(3);
        let counts = [10 + i, 2, 0, 0, 7 * i, 0, 0, 1];
        ClusterRecord {
            lon: snap(-73.97 + 0.013 * f64::from(i)),
            lat: snap(40.78 - 0.007 * f64::from(i)),
            blocks: if leaf { 1 } else { 2 + i },
            pop: counts.iter().sum::<u32>() + i % 2,
            counts,
            expansion_zoom: if leaf { DOTS_ZOOM } else { 4 + (i % 8) as u8 },
            leaf,
        }
    }

    fn header(n: u32) -> Vec<u8> {
        let mut b = MAGIC.to_vec();
        b.push(VERSION);
        b.extend_from_slice(&n.to_le_bytes());
        b
    }

    /// One minimal record: `flags`, `ez`, zero deltas, `blocks`, `pop`,
    /// `mask` and one count per set bit of `count`.
    fn record(flags: u8, ez: u8, blocks: u64, pop: u64, mask: u8, count: u64) -> Vec<u8> {
        let mut b = vec![flags, ez, 0, 0];
        write_varint(&mut b, blocks);
        write_varint(&mut b, pop);
        b.push(mask);
        for _ in 0..mask.count_ones() {
            write_varint(&mut b, count);
        }
        b
    }

    fn err_text(r: Result<Vec<ClusterRecord>, GridError>) -> String {
        match r {
            Err(GridError::Server(s)) => s,
            other => format!("{other:?}"),
        }
    }

    #[test]
    #[allow(clippy::clone_on_copy, clippy::cloned_ref_to_slice_refs)] // the plan's test, verbatim
    fn cluster_tiles_round_trip() {
        let r = ClusterRecord { lon: -73.9, lat: 40.7, blocks: 12, pop: 9000, counts: [1, 2, 3, 4, 5, 6, 7, 8964], expansion_zoom: 9, leaf: false };
        assert_eq!(decode(&encode(&[r.clone()])).unwrap(), vec![r]);
    }

    #[test]
    fn many_records_round_trip_in_order_and_at_the_globe_edges() {
        let mut recs: Vec<ClusterRecord> = (0..500).map(rec).collect();
        recs.push(ClusterRecord { lon: 180.0, lat: -85.0511287, ..rec(1) });
        recs.push(ClusterRecord { lon: -180.0, lat: 85.0511287, ..rec(3) });
        recs.push(ClusterRecord { lon: 179.9999999, lat: 71.3, pop: u32::MAX, counts: [u32::MAX, 0, 0, 0, 0, 0, 0, 0], ..rec(4) });
        assert_eq!(decode(&encode(&recs)).unwrap(), recs);
        assert!(decode(&encode(&[])).unwrap().is_empty());
    }

    #[test]
    fn records_are_compact() {
        // A z11-like tile of 400 clusters on a lattice: well under 20 B a
        // record gzipped.
        let recs: Vec<ClusterRecord> = (0..400u32)
            .map(|i| {
                let counts = [800 + i, 300, 12, 0, 90, 0, 20, 45];
                ClusterRecord {
                    lon: snap(-74.0 + 0.007 * f64::from(i % 20)),
                    lat: snap(40.5 + 0.005 * f64::from(i / 20)),
                    blocks: 20 + i % 40,
                    pop: counts.iter().sum(),
                    counts,
                    expansion_zoom: 12,
                    leaf: false,
                }
            })
            .collect();
        let bytes = encode(&recs);
        assert!(bytes.len() < 20 * recs.len(), "{} B for {} records", bytes.len(), recs.len());
    }

    #[test]
    fn a_truncated_cluster_tile_is_an_error_not_a_panic() {
        let recs: Vec<ClusterRecord> = (0..6).map(rec).collect();
        let b = encode(&recs);
        for n in 0..b.len() {
            assert!(decode(&b[..n]).is_err(), "{n}");
        }
        let body = inflate_capped(&b, MAX_INFLATED_BYTES).unwrap();
        for n in 0..body.len() {
            assert!(decode_body(&body[..n]).is_err(), "{n}");
        }
    }

    /// The corruption sweep: every byte of a real body flipped four ways,
    /// then 20,000 seeded random mutations and cuts, then noise -- errors
    /// or records, never a panic, and never a record a guard refuses.
    #[test]
    fn corrupt_cluster_tiles_never_panic() {
        let recs: Vec<ClusterRecord> = (0..24).map(rec).collect();
        let body = inflate_capped(&encode(&recs), MAX_INFLATED_BYTES).unwrap();
        let check = |b: &[u8]| {
            if let Ok(t) = decode_body(b) {
                assert!(t.len() as u32 <= MAX_RECORDS_PER_TILE);
                for r in &t {
                    assert!((1..=DOTS_ZOOM).contains(&r.expansion_zoom));
                    assert_eq!(r.leaf, r.blocks == 1);
                    assert!(r.pop >= 1 && r.counts.iter().map(|&c| u64::from(c)).sum::<u64>() <= u64::from(r.pop));
                    assert!(r.lon.abs() <= 180.0 && r.lat.abs() <= 90.0);
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
        for len in [0usize, 1, 17, 64, 1000, 30_000] {
            let mut b = header(len as u32 / 8);
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
        assert_eq!(decode(&gz(b"HXT1 not a cluster tile")), Err(GridError::BadMagic));
        let mut b = header(0);
        b[4] = 9;
        assert_eq!(decode(&gz(&b)), Err(GridError::BadVersion(9)));
    }

    #[test]
    fn guard_records_per_tile() {
        // A claim past the maximum, over a body that could hold it.
        let mut b = header(MAX_RECORDS_PER_TILE + 1);
        for _ in 0..=MAX_RECORDS_PER_TILE {
            b.extend(record(FLAG_LEAF, 12, 1, 5, 1, 5));
        }
        assert!(err_text(decode_body(&b)).contains("more records than a tile holds"));
    }

    #[test]
    fn guard_count_against_the_body() {
        // 1,000 claimed, one (corrupt) record present: refused before any
        // record is read, so the error is Truncated, not the record's.
        let mut b = header(1000);
        b.extend(record(0xFF, 12, 1, 5, 1, 5));
        assert_eq!(decode_body(&b), Err(GridError::Truncated));
    }

    #[test]
    fn guard_reservation() {
        assert_eq!(prealloc(u32::MAX), PREALLOC);
        assert_eq!(prealloc(MAX_RECORDS_PER_TILE), PREALLOC);
        assert_eq!(prealloc(3), 3);
        // And decode reserves by it: 1,500 records grow from 1,024 (to
        // 2,048), where an unguarded reservation takes exactly 1,500.
        let recs: Vec<ClusterRecord> = (0..1500).map(rec).collect();
        assert!(decode(&encode(&recs)).unwrap().capacity() > 1500);
        // The largest encoding of a record fits the inflate cap MAX times.
        let widest = ClusterRecord {
            lon: 180.0,
            lat: -90.0,
            blocks: u32::MAX,
            pop: u32::MAX,
            counts: [u32::MAX / 8; 8],
            expansion_zoom: 12,
            leaf: false,
        };
        let one = inflate_capped(&encode(&[widest, ClusterRecord { lon: -180.0, lat: 90.0, ..widest }]), MAX_INFLATED_BYTES).unwrap();
        let per = (one.len() - 9) / 2;
        assert!(per <= 63, "{per}");
        assert!(9 + u64::from(MAX_RECORDS_PER_TILE) * 63 <= MAX_INFLATED_BYTES);
        // Measured, not restated (review M1): a body of exactly the cap is
        // accepted in an allocation of exactly the cap -- `read_to_end`
        // would have doubled it to 1 MiB -- and one byte more is refused.
        let cap = MAX_INFLATED_BYTES as usize;
        let at_cap = inflate_capped(&gz(&vec![7u8; cap]), MAX_INFLATED_BYTES).unwrap();
        assert_eq!((at_cap.len(), at_cap.capacity()), (cap, cap));
        assert!(matches!(inflate_capped(&gz(&vec![7u8; cap + 1]), MAX_INFLATED_BYTES), Err(GridError::Inflate(_))));
        // A full tile's records end in a Vec of exactly the maximum.
        let full = decode(&encode(&(0..MAX_RECORDS_PER_TILE).map(rec).collect::<Vec<_>>())).unwrap();
        assert_eq!(full.capacity(), MAX_RECORDS_PER_TILE as usize);
        // So the peak of an accepted worst tile, from the measured
        // capacities: the body, the full records Vec and the half-size one
        // it grew from (1,024 -> 2,048 -> 4,096), all live. The counting
        // allocator in tests/cluster_tile_peak_memory.rs measures the same.
        assert_eq!(size_of::<ClusterRecord>(), 64);
        let records = full.capacity() * size_of::<ClusterRecord>();
        let peak = at_cap.capacity() + records + records / 2;
        assert_eq!(peak, 896 << 10, "the doc's 896 KiB is stale: {peak}");
    }

    #[test]
    #[should_panic(expected = "is not a position")]
    fn encode_refuses_a_nan_position() {
        // `quantise(NaN)` is 0, so without the guard this is written at 0°, 0°.
        encode(&[ClusterRecord { lon: f64::NAN, ..rec(1) }]);
    }

    #[test]
    #[should_panic(expected = "is not a position")]
    fn encode_refuses_an_infinite_position() {
        encode(&[ClusterRecord { lat: f64::INFINITY, ..rec(1) }]);
    }

    #[test]
    fn guard_flags() {
        let mut b = header(1);
        b.extend(record(0b10, 12, 2, 5, 1, 5));
        assert!(err_text(decode_body(&b)).contains("unknown flag bits"));
    }

    #[test]
    fn guard_expansion_zoom() {
        for ez in [0u8, 13, 255] {
            let mut b = header(1);
            b.extend(record(0, ez, 2, 5, 1, 5));
            assert!(err_text(decode_body(&b)).contains("expansion zoom"), "{ez}");
        }
    }

    #[test]
    fn guard_position_off_the_globe() {
        for (k, deg) in [(0usize, 180.0000001f64), (0, -180.0000001), (1, 90.0000001), (1, -1e12)] {
            let mut b = header(1);
            b.extend([0, 5]);
            for kk in 0..2 {
                write_varint(&mut b, zigzag(if kk == k { quantise(deg) } else { 0 }));
            }
            b.extend(&record(0, 5, 2, 5, 1, 5)[4..]);
            assert!(err_text(decode_body(&b)).contains("off the globe"), "{k} {deg}");
        }
        // A delta that would overflow i64 without the checked add.
        let mut b = header(2);
        let mut first = vec![0u8, 5];
        write_varint(&mut first, zigzag(MAX_LON_Q));
        first.push(0);
        first.extend(&record(0, 5, 2, 5, 1, 5)[4..]);
        b.extend(&first);
        b.extend([0, 5]);
        write_varint(&mut b, zigzag(i64::MAX));
        b.push(0);
        b.extend(&record(0, 5, 2, 5, 1, 5)[4..]);
        assert!(err_text(decode_body(&b)).contains("off the globe"));
    }

    #[test]
    fn guard_block_count() {
        for blocks in [0u64, u64::from(u32::MAX) + 3] {
            let mut b = header(1);
            b.extend(record(0, 5, blocks, 5, 1, 5));
            assert!(err_text(decode_body(&b)).contains("block count"), "{blocks}");
        }
    }

    #[test]
    fn guard_leaf_is_one_block() {
        let mut b = header(1);
        b.extend(record(FLAG_LEAF, 12, 2, 5, 1, 5));
        assert!(err_text(decode_body(&b)).contains("a leaf of more than one block"));
        let mut b = header(1);
        b.extend(record(0, 12, 1, 5, 1, 5));
        assert!(err_text(decode_body(&b)).contains("a cluster of one"));
    }

    #[test]
    fn guard_population() {
        for pop in [0u64, u64::from(u32::MAX) + 5] {
            let mut b = header(1);
            b.extend(record(0, 5, 2, pop, 1, 4));
            assert!(err_text(decode_body(&b)).contains("a population of 0"), "{pop}");
        }
    }

    #[test]
    fn guard_no_group_people() {
        let mut b = header(1);
        b.extend(record(0, 5, 2, 5, 0, 0));
        b.push(0); // a maskless record is 7 B, under the 8 B minimum
        assert!(err_text(decode_body(&b)).contains("no group people"));
    }

    #[test]
    fn guard_group_count() {
        for c in [0u64, u64::from(u32::MAX) + 5] {
            let mut b = header(1);
            b.extend(record(0, 5, 2, 5, 1, c));
            assert!(err_text(decode_body(&b)).contains("a group count of 0"), "{c}");
        }
    }

    #[test]
    fn guard_groups_past_the_population() {
        // Within u32 but past pop.
        let mut b = header(1);
        b.extend(record(0, 5, 2, 5, 0b11, 3));
        assert!(err_text(decode_body(&b)).contains("groups past the population"));
        // Past u32 itself (a wrapping add would come back under pop).
        let mut b = header(1);
        b.extend(record(0, 5, 2, u64::from(u32::MAX), 0b11, u64::from(u32::MAX)));
        assert!(err_text(decode_body(&b)).contains("groups past the population"));
    }

    #[test]
    fn guard_trailing_bytes() {
        let mut b = inflate_capped(&encode(&[rec(1)]), MAX_INFLATED_BYTES).unwrap();
        b.push(0);
        assert!(err_text(decode_body(&b)).contains("after the last record"));
    }

    #[test]
    fn the_metadata_round_trips_and_other_zooms_are_refused() {
        let text = cluster_meta_json("U.S. Census Bureau").to_string();
        assert_eq!(parse_cluster_meta(text.as_bytes(), 1).unwrap().attribution, "U.S. Census Bureau");
        let other = text.replace("\"max_zoom\":11", "\"max_zoom\":14");
        assert_ne!(other, text);
        assert!(parse_cluster_meta(other.as_bytes(), 1).is_err());
        assert!(parse_cluster_meta(b"{\"race_hex\":{}}", 1).is_err());
    }
}

//! The converter's two fixed-size record files (ruling C8): `parts.spill`,
//! one [`SpillPart`] per block part (read back by the tile stage here and by
//! Task 9's ClickHouse load), and `blocks.points`, one [`BlockPoint`] per
//! populated block (Task 12's cluster input).
//!
//! Both are `#[repr(C)]` so their size is pinned by a `const` assertion, and
//! both go through explicit little-endian [`SpillPart::encode`] /
//! [`SpillPart::decode`] (no transmute, no bincode): the byte layout is the
//! contract, and it is the same on every host.

use std::io::{self, Read, Write};

use gpui_maps::overlay::dotgrid::parts::{Part, Tri};

/// Bytes of one `parts.spill` record.
pub const PART_RECORD_BYTES: usize = 56;
/// Bytes of one `blocks.points` record.
pub const POINT_RECORD_BYTES: usize = 48;

/// `SpillPart::flags` bit: the block had no usable polygon and its people
/// sit in the 10 m fallback triangle around its interior point.
pub const FLAG_FALLBACK: u8 = 1;

/// One block part as spilled: 56 bytes, little-endian, in field order.
///
/// | offset | field | |
/// |---|---|---|
/// | 0 | `geoid` u64 | the block's 15-digit GEOID20 |
/// | 8 | `part` u32 | the part's index within its block (0-based) |
/// | 12 | `state` u8 | state FIPS |
/// | 13 | `flags` u8 | [`FLAG_FALLBACK`] |
/// | 14 | `_pad` u16 | 0 |
/// | 16 | `v` 3 × [i32; 2] | the triangle's vertices a, b, c on the 2^31 world grid |
/// | 40 | `c` 8 × u16 | people per group |
///
/// Vertices fit an i32 because world-grid quanta are in `[0, 2^31)`. Counts
/// fit a u16 because every part is split to at most 65,535 people in total
/// (`split_part`'s cap, the composition texel's denominator).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SpillPart {
    pub geoid: u64,
    pub part: u32,
    pub state: u8,
    pub flags: u8,
    pub _pad: u16,
    pub v: [[i32; 2]; 3],
    pub c: [u16; 8],
}

const _: () = assert!(std::mem::size_of::<SpillPart>() == PART_RECORD_BYTES);

impl SpillPart {
    /// The record of `p`, part `part` of block `geoid`.
    ///
    /// # Panics
    /// If a vertex is off the world grid or a count is past u16 -- the
    /// converter's own invariants, so a panic is a bug, not bad input.
    pub fn new(geoid: u64, state: u8, part: u32, flags: u8, p: &Part) -> Self {
        let v = [p.tri.a, p.tri.b, p.tri.c].map(|q| q.map(|x| i32::try_from(x).expect("a vertex on the 2^31 world grid")));
        let c = p.c.map(|n| u16::try_from(n).expect("a split part holds at most 65,535 people"));
        Self { geoid, part, state, flags, _pad: 0, v, c }
    }

    pub fn part(&self) -> Part {
        let q = |i: usize| [i64::from(self.v[i][0]), i64::from(self.v[i][1])];
        Part { tri: Tri { a: q(0), b: q(1), c: q(2) }, c: self.c.map(u32::from) }
    }

    pub fn people(&self) -> u32 {
        self.c.iter().map(|&n| u32::from(n)).sum()
    }

    /// The centroid in world-grid quanta.
    pub fn centroid(&self) -> [f64; 2] {
        let s = |k: usize| (i64::from(self.v[0][k]) + i64::from(self.v[1][k]) + i64::from(self.v[2][k])) as f64 / 3.0;
        [s(0), s(1)]
    }

    pub fn encode(&self) -> [u8; PART_RECORD_BYTES] {
        let mut b = [0u8; PART_RECORD_BYTES];
        b[0..8].copy_from_slice(&self.geoid.to_le_bytes());
        b[8..12].copy_from_slice(&self.part.to_le_bytes());
        b[12] = self.state;
        b[13] = self.flags;
        b[14..16].copy_from_slice(&self._pad.to_le_bytes());
        for (i, v) in self.v.iter().enumerate() {
            b[16 + 8 * i..20 + 8 * i].copy_from_slice(&v[0].to_le_bytes());
            b[20 + 8 * i..24 + 8 * i].copy_from_slice(&v[1].to_le_bytes());
        }
        for (g, n) in self.c.iter().enumerate() {
            b[40 + 2 * g..42 + 2 * g].copy_from_slice(&n.to_le_bytes());
        }
        b
    }

    pub fn decode(b: &[u8; PART_RECORD_BYTES]) -> Self {
        let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let i32_at = |o: usize| i32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let mut v = [[0i32; 2]; 3];
        for (i, v) in v.iter_mut().enumerate() {
            *v = [i32_at(16 + 8 * i), i32_at(20 + 8 * i)];
        }
        let mut c = [0u16; 8];
        for (g, n) in c.iter_mut().enumerate() {
            *n = u16::from_le_bytes(b[40 + 2 * g..42 + 2 * g].try_into().unwrap());
        }
        Self {
            geoid: u64::from_le_bytes(b[0..8].try_into().unwrap()),
            part: u32_at(8),
            state: b[12],
            flags: b[13],
            _pad: u16::from_le_bytes(b[14..16].try_into().unwrap()),
            v,
            c,
        }
    }
}

/// One populated block as spilled: 48 bytes, little-endian, in field order.
///
/// | offset | field | |
/// |---|---|---|
/// | 0 | `geoid` u64 | GEOID20 (its first two digits are the state FIPS) |
/// | 8 | `lat_e7` i32 | P.L. `INTPTLAT` × 10^7, rounded |
/// | 12 | `lon_e7` i32 | P.L. `INTPTLON` × 10^7, rounded |
/// | 16 | `c` 8 × u32 | the block's people per group |
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BlockPoint {
    pub geoid: u64,
    pub lat_e7: i32,
    pub lon_e7: i32,
    pub c: [u32; 8],
}

const _: () = assert!(std::mem::size_of::<BlockPoint>() == POINT_RECORD_BYTES);

impl BlockPoint {
    pub fn new(geoid: u64, lat: f64, lon: f64, c: [u32; 8]) -> Self {
        Self { geoid, lat_e7: (lat * 1e7).round() as i32, lon_e7: (lon * 1e7).round() as i32, c }
    }

    /// The block's people: its 8 groups' sum (P2 groups partition P2_001N).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn pop(&self) -> u32 {
        self.c.iter().sum()
    }

    pub fn encode(&self) -> [u8; POINT_RECORD_BYTES] {
        let mut b = [0u8; POINT_RECORD_BYTES];
        b[0..8].copy_from_slice(&self.geoid.to_le_bytes());
        b[8..12].copy_from_slice(&self.lat_e7.to_le_bytes());
        b[12..16].copy_from_slice(&self.lon_e7.to_le_bytes());
        for (g, n) in self.c.iter().enumerate() {
            b[16 + 4 * g..20 + 4 * g].copy_from_slice(&n.to_le_bytes());
        }
        b
    }

#[cfg_attr(not(test), allow(dead_code))] // tests, and the readers of later tasks
    pub fn decode(b: &[u8; POINT_RECORD_BYTES]) -> Self {
        let mut c = [0u32; 8];
        for (g, n) in c.iter_mut().enumerate() {
            *n = u32::from_le_bytes(b[16 + 4 * g..20 + 4 * g].try_into().unwrap());
        }
        Self {
            geoid: u64::from_le_bytes(b[0..8].try_into().unwrap()),
            lat_e7: i32::from_le_bytes(b[8..12].try_into().unwrap()),
            lon_e7: i32::from_le_bytes(b[12..16].try_into().unwrap()),
            c,
        }
    }
}

pub fn write_part(w: &mut impl Write, p: &SpillPart) -> io::Result<()> {
    w.write_all(&p.encode())
}

pub fn write_point(w: &mut impl Write, p: &BlockPoint) -> io::Result<()> {
    w.write_all(&p.encode())
}

/// Every record of a `parts.spill` stream, in order. A trailing partial
/// record is an error (the file is truncated to whole records on resume).
pub fn for_each_part(mut r: impl Read, mut f: impl FnMut(SpillPart) -> anyhow::Result<()>) -> anyhow::Result<u64> {
    let mut buf = vec![0u8; PART_RECORD_BYTES * 8192];
    let mut n = 0u64;
    loop {
        let mut filled = 0;
        while filled < buf.len() {
            match r.read(&mut buf[filled..])? {
                0 => break,
                k => filled += k,
            }
        }
        if filled % PART_RECORD_BYTES != 0 {
            anyhow::bail!("parts.spill ends in a partial record ({} stray bytes)", filled % PART_RECORD_BYTES);
        }
        for rec in buf[..filled].chunks_exact(PART_RECORD_BYTES) {
            f(SpillPart::decode(rec.try_into().unwrap()))?;
            n += 1;
        }
        if filled < buf.len() {
            return Ok(n);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spill_part_round_trips_through_its_56_bytes() {
        let p = Part {
            tri: Tri { a: [0, (1 << 31) - 1], b: [1_234_567_890, 5], c: [42, 2_000_000_000] },
            c: [65_535, 0, 1, 2, 3, 4, 5, 60_000],
        };
        let rec = SpillPart::new(560_019_801_001_000, 56, 7, FLAG_FALLBACK, &p);
        let bytes = rec.encode();
        assert_eq!(bytes.len(), 56);
        let back = SpillPart::decode(&bytes);
        assert_eq!(back, rec);
        assert_eq!(back.part(), p);
        // Layout pinned: GEOID first, counts last.
        assert_eq!(&bytes[0..8], &560_019_801_001_000u64.to_le_bytes());
        assert_eq!(&bytes[54..56], &60_000u16.to_le_bytes());
    }

    #[test]
    fn a_block_point_round_trips_through_its_48_bytes() {
        let p = BlockPoint::new(110_010_001_011_000, 38.8951234, -77.0364567, [1, 2, 3, 4, 5, 6, 7, 100_000]);
        assert_eq!((p.lat_e7, p.lon_e7), (388_951_234, -770_364_567));
        let back = BlockPoint::decode(&p.encode());
        assert_eq!(back, p);
    }

    #[test]
    fn for_each_part_reads_every_record_and_refuses_a_partial_one() {
        let recs: Vec<SpillPart> = (0..20_000u32).map(|i| SpillPart { geoid: u64::from(i), part: i, ..Default::default() }).collect();
        let mut bytes = Vec::new();
        for r in &recs {
            write_part(&mut bytes, r).unwrap();
        }
        let mut got = Vec::new();
        let n = for_each_part(&bytes[..], |r| {
            got.push(r);
            Ok(())
        })
        .unwrap();
        assert_eq!(n, 20_000);
        assert_eq!(got, recs);
        bytes.push(0);
        assert!(for_each_part(&bytes[..], |_| Ok(())).is_err());
    }
}

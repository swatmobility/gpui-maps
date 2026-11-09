//! `race-build verify-geoids`: proves after the fact, without the TIGER
//! zips, that every block's geometry in `parts.spill` belongs to its GEOID
//! (review F1). The `.shp` and `.dbf` are paired by record position. The
//! exact sums cannot see a shifted pairing, because the join to P.L. is by
//! GEOID. So this check uses a fact independent of TIGER's record order:
//! the P.L. geoheader's own interior point (`INTPTLAT`/`INTPTLON`, keyed by
//! GEOID) must fall on that block's parts.
//!
//! The spill holds only the triangles that received people. A triangle
//! allocated 0 people is not spilled, so a block's interior point can
//! honestly miss every part. Each block is therefore graded:
//! - **on a part**: inside a part triangle, edges included, or within
//!   `SIMPLIFY_M` of one;
//! - **in the parts' box**: inside the bounding box of the block's parts,
//!   grown by `SIMPLIFY_M` (a triangle dropped for having 0 people);
//! - **off**: neither. With a correct pairing this should not happen.
//!
//! The same grading is also run against a deliberately wrong pairing: each
//! interior point against the *previous* block's parts in spill order,
//! which is TIGER record order. A shifted pairing would look exactly like
//! that, so the gap between the two columns shows what the check would
//! catch.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use anyhow::{bail, Context, Result};
use gpui_maps::overlay::dotgrid::parts::Tri;
use serde::{Deserialize, Serialize};

use crate::build::{SIMPLIFY_M, STATES};
use crate::spill::{for_each_part, SpillPart};
use crate::{pl94, tiger};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grade {
    OnPart,
    InBox,
    Off,
}

/// Twice the signed area of (a, b, p), exactly.
fn orient(a: [i64; 2], b: [i64; 2], p: [i64; 2]) -> i128 {
    i128::from(b[0] - a[0]) * i128::from(p[1] - a[1]) - i128::from(b[1] - a[1]) * i128::from(p[0] - a[0])
}

/// Whether `p` is in the closed triangle `t` (edges and vertices count).
pub fn in_triangle(t: &Tri, p: [i64; 2]) -> bool {
    let (d1, d2, d3) = (orient(t.a, t.b, p), orient(t.b, t.c, p), orient(t.c, t.a, p));
    let neg = d1 < 0 || d2 < 0 || d3 < 0;
    let pos = d1 > 0 || d2 > 0 || d3 > 0;
    !(neg && pos)
}

/// Grades a block's interior point against its parts' triangles.
pub fn grade(tris: &[Tri], intpt: (f64, f64)) -> (Grade, f64) {
    let (lat, lon) = intpt;
    let p = tiger::project(lon, lat);
    let tol = tiger::quanta_per_metre(lat) * SIMPLIFY_M;
    if tris.iter().any(|t| in_triangle(t, p)) {
        return (Grade::OnPart, 0.0);
    }
    let pf = [p[0] as f64, p[1] as f64];
    let f = |q: [i64; 2]| [q[0] as f64, q[1] as f64];
    let d = tris
        .iter()
        .flat_map(|t| [(t.a, t.b), (t.b, t.c), (t.c, t.a)])
        .map(|(a, b)| tiger::seg_dist2(pf, f(a), f(b)))
        .fold(f64::INFINITY, f64::min)
        .sqrt();
    let d_m = d / tiger::quanta_per_metre(lat);
    if d <= tol {
        return (Grade::OnPart, d_m);
    }
    let lo = tris.iter().flat_map(|t| [t.a, t.b, t.c]).reduce(|m, q| [m[0].min(q[0]), m[1].min(q[1])]).unwrap();
    let hi = tris.iter().flat_map(|t| [t.a, t.b, t.c]).reduce(|m, q| [m[0].max(q[0]), m[1].max(q[1])]).unwrap();
    let inb = |k: usize| pf[k] >= lo[k] as f64 - tol && pf[k] <= hi[k] as f64 + tol;
    if inb(0) && inb(1) {
        (Grade::InBox, d_m)
    } else {
        (Grade::Off, d_m)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Tally {
    pub on_part: u64,
    pub in_box: u64,
    pub off: u64,
}

impl Tally {
    fn add(&mut self, g: Grade) {
        match g {
            Grade::OnPart => self.on_part += 1,
            Grade::InBox => self.in_box += 1,
            Grade::Off => self.off += 1,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StateGeoids {
    pub abbr: String,
    pub blocks: u64,
    /// The block's own parts.
    pub own: Tally,
    /// The previous block's parts (a one-record shift), for contrast.
    pub shifted: Tally,
    /// The first "off" blocks: GEOID and distance to their parts, metres.
    pub off_examples: Vec<(u64, f64)>,
    /// Populated P.L. blocks absent from the spill (must be 0).
    pub missing: u64,
    /// "Off" blocks whose interior point is on the previous or the next
    /// record's parts instead -- what a pairing error would look like.
    pub off_on_neighbour: u64,
    /// People in the "off" blocks, and how many of those blocks hold at
    /// most 5 people (few parts: most of their triangles got no one).
    pub off_people: u64,
    pub off_le_5_people: u64,
}

/// Grades every block of a spill stream against the interior points that
/// `intpts(state)` returns (GEOID → (lat, lon), populated blocks only).
pub fn verify_stream(
    spill: impl std::io::Read,
    mut intpts: impl FnMut(u8) -> Result<HashMap<u64, (f64, f64)>>,
) -> Result<BTreeMap<u8, StateGeoids>> {
    let mut out: BTreeMap<u8, StateGeoids> = BTreeMap::new();
    let mut state: Option<(u8, HashMap<u64, (f64, f64)>)> = None;
    let mut cur: Option<(u8, u64, Vec<Tri>, u64)> = None;
    let mut prev_tris: Vec<Tri> = Vec::new();
    // The last block's interior point when it graded "off", to test
    // against the next block's parts.
    let mut pending_off: Option<(f64, f64)> = None;

    // Closes one block: grade it against its own and the previous parts.
    let mut close = |blk: (u8, u64, Vec<Tri>, u64),
                     state: &mut Option<(u8, HashMap<u64, (f64, f64)>)>,
                     prev: &mut Vec<Tri>,
                     pending_off: &mut Option<(f64, f64)>,
                     out: &mut BTreeMap<u8, StateGeoids>|
     -> Result<()> {
        let (fips, geoid, tris, people) = blk;
        if state.as_ref().map(|s| s.0) != Some(fips) {
            if let Some((old, map)) = state.take() {
                out.entry(old).or_default().missing = map.len() as u64;
            }
            if out.contains_key(&fips) {
                bail!("state {fips:02} appears twice in the spill, not as one run");
            }
            *state = Some((fips, intpts(fips)?));
            prev.clear();
            *pending_off = None;
        }
        let map = &mut state.as_mut().unwrap().1;
        let intpt = map.remove(&geoid).with_context(|| format!("block {geoid:015} is in the spill but not a populated P.L. block"))?;
        let st = out.entry(fips).or_default();
        st.blocks += 1;
        if let Some(p) = pending_off.take() {
            if grade(&tris, p).0 == Grade::OnPart {
                st.off_on_neighbour += 1;
            }
        }
        let (g, d) = grade(&tris, intpt);
        st.own.add(g);
        let shifted = if prev.is_empty() { None } else { Some(grade(prev, intpt).0) };
        if let Some(sg) = shifted {
            st.shifted.add(sg);
        }
        if g == Grade::Off {
            if st.off_examples.len() < 20 {
                st.off_examples.push((geoid, d));
            }
            st.off_people += people;
            st.off_le_5_people += u64::from(people <= 5);
            if shifted == Some(Grade::OnPart) {
                st.off_on_neighbour += 1;
            } else {
                *pending_off = Some(intpt);
            }
        }
        *prev = tris;
        Ok(())
    };

    for_each_part(spill, |p: SpillPart| {
        match &mut cur {
            Some((f, g, tris, people)) if *f == p.state && *g == p.geoid => {
                tris.push(p.part().tri);
                *people += u64::from(p.people());
            }
            _ => {
                if let Some(blk) = cur.take() {
                    close(blk, &mut state, &mut prev_tris, &mut pending_off, &mut out)?;
                }
                cur = Some((p.state, p.geoid, vec![p.part().tri], u64::from(p.people())));
            }
        }
        Ok(())
    })?;
    if let Some(blk) = cur.take() {
        close(blk, &mut state, &mut prev_tris, &mut pending_off, &mut out)?;
    }
    if let Some((old, map)) = state.take() {
        out.entry(old).or_default().missing = map.len() as u64;
    }
    for (fips, st) in out.iter_mut() {
        st.abbr = STATES.iter().find(|s| s.1 == *fips).map(|s| s.0.to_string()).unwrap_or_default();
    }
    Ok(out)
}

/// `verify-geoids` over the real spill and the cached P.L. zips.
pub fn verify_geoids(spill: &Path, cache: &Path) -> Result<BTreeMap<u8, StateGeoids>> {
    let f = File::open(spill).with_context(|| format!("opening {}", spill.display()))?;
    verify_stream(BufReader::with_capacity(1 << 22, f), |fips| {
        let abbr = STATES.iter().find(|s| s.1 == fips).with_context(|| format!("unknown state {fips}"))?.0;
        let pl = pl94::parse_state(&cache.join("pl").join(format!("{abbr}2020.pl.zip")))?;
        Ok(pl.blocks.iter().filter(|b| b.c.iter().any(|&n| n > 0)).map(|b| (b.geoid, b.intpt)).collect())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spill::write_part;
    use gpui_maps::overlay::dotgrid::parts::Part;

    /// A row of `n` 100 m square blocks along a street, each one part pair,
    /// and each block's interior point at its centre.
    fn street(n: usize) -> (Vec<(u64, Vec<Tri>)>, HashMap<u64, (f64, f64)>) {
        let mut blocks = Vec::new();
        let mut pts = HashMap::new();
        for i in 0..n {
            let lon = -77.0 + i as f64 * 0.002;
            let (a, c) = (tiger::project(lon, 38.9), tiger::project(lon + 0.0012, 38.9 + 0.0009));
            let b = [c[0], a[1]];
            let d = [a[0], c[1]];
            let geoid = 110_010_001_011_000 + i as u64;
            blocks.push((geoid, vec![Tri { a, b, c }, Tri { a, b: c, c: d }]));
            pts.insert(geoid, (38.9 + 0.00045, lon + 0.0006));
        }
        (blocks, pts)
    }

    fn spill_of(blocks: &[(u64, Vec<Tri>)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for (geoid, tris) in blocks {
            for (k, t) in tris.iter().enumerate() {
                let p = Part { tri: *t, c: [1, 0, 0, 0, 0, 0, 0, 0] };
                write_part(&mut bytes, &SpillPart::new(*geoid, 11, k as u32, 0, &p)).unwrap();
            }
        }
        bytes
    }

    #[test]
    fn correct_pairing_puts_every_interior_point_on_its_parts() {
        let (blocks, pts) = street(50);
        let r = verify_stream(&spill_of(&blocks)[..], |_| Ok(pts.clone())).unwrap();
        let st = &r[&11];
        assert_eq!((st.blocks, st.own.on_part, st.own.off, st.missing), (50, 50, 0, 0));
        // The built-in contrast: against the neighbour's parts, all are off.
        assert_eq!(st.shifted.off, 49);
    }

    #[test]
    fn a_shifted_record_pairing_is_caught() {
        // .shp and .dbf off by one record: block i carries block i+1's
        // polygon (the last wraps around).
        let (blocks, pts) = street(50);
        let shifted: Vec<(u64, Vec<Tri>)> =
            blocks.iter().enumerate().map(|(i, (g, _))| (*g, blocks[(i + 1) % 50].1.clone())).collect();
        let r = verify_stream(&spill_of(&shifted)[..], |_| Ok(pts.clone())).unwrap();
        let st = &r[&11];
        assert_eq!(st.own.off, 50, "{st:?}");
        assert_eq!(st.own.on_part, 0);
        // And each lands on its neighbour's parts: the signature of a shift.
        assert_eq!(st.off_on_neighbour, 49);
    }

    #[test]
    fn a_missing_block_and_an_edge_point_are_reported() {
        let (blocks, mut pts) = street(3);
        // An interior point exactly on a triangle's vertex counts as on it.
        let t = blocks[0].1[0];
        assert!(in_triangle(&t, t.a) && in_triangle(&t, [(t.a[0] + t.b[0]) / 2, t.a[1]]));
        pts.insert(999, (0.0, 0.0));
        let r = verify_stream(&spill_of(&blocks)[..], |_| Ok(pts.clone())).unwrap();
        assert_eq!(r[&11].missing, 1);
    }
}

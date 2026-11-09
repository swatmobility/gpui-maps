//! Shared-arc simplification: every block group's rings, nationally, cut
//! into arcs at the junctions, each arc simplified once and reused (reversed
//! where needed) by every ring on it. Two neighbours therefore carry the
//! same simplified edge, so no sliver opens between them at any zoom, and
//! state lines are handled like any other shared edge.
//!
//! The tables are `u32` vertex ids over `i32` quanta (the pack's 1e-6 deg),
//! never `f64` keys. MG-C2 (peak RSS <= 3 GB) shaped the memory; national
//! peak is 2.59 GB, of which 1.3 GB is the input geometry itself:
//! - the vertex table is the sorted, deduplicated `u64`-packed quanta (each
//!   state deduplicated before they are joined), a vertex's id its index,
//!   found by binary search. A `HashMap` from quanta to id measured 4.2 GB
//!   peak, then 3.27 GB even sized up front: ~0.9 GB of buckets beside a
//!   coordinate table, and macOS's allocator kept the dropped map resident;
//! - the id buffer is sized once from the input's point count;
//! - each input ring's allocation is cleared and refilled with its output
//!   rather than freed and reallocated;
//! - the MG-C3 edge list is built last, sized exactly, after the id
//!   buffers are dropped.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

use gpui_maps::census::pack::{Feature, QUANTUM_DEG};

use crate::simplify::{dp_keep, simplify_ring};

/// What the national pass found; `unmatched_interior_edges` is MG-C3.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TopoStats {
    /// Distinct vertices at the pack's quantum.
    pub unique_vertices: usize,
    /// Vertices where arcs end: not exactly two distinct neighbours, or
    /// the first point of some ring.
    pub junctions: usize,
    /// Distinct arcs, each simplified once.
    pub arcs: usize,
    /// Simplified directed edges on an arc two or more rings use whose
    /// reverse no ring carries (0 when every shared edge matches).
    pub unmatched_interior_edges: usize,
    /// Arcs only one ring uses: the national outline and the shore, or a
    /// neighbour whose shared edge is not vertex-identical (a sliver).
    pub single_use_arcs: usize,
    /// Points the distinct simplified arcs hold, counting each arc once:
    /// what a pack storing arcs rather than rings would carry.
    pub arc_points: usize,
    /// Rings whose arcs simplified to fewer than 3 distinct points and were
    /// re-simplified at 0 m (or, if still degenerate, per ring).
    pub collapsed_rings: usize,
}

/// FxHash: the tables hash tens of millions of small integer keys, where
/// SipHash's DoS resistance buys nothing.
#[derive(Default)]
struct Fx(u64);

impl Fx {
    fn add(&mut self, x: u64) {
        self.0 = (self.0.rotate_left(5) ^ x).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
}

impl Hasher for Fx {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.add(u64::from(b));
        }
    }
    fn write_u32(&mut self, i: u32) {
        self.add(u64::from(i));
    }
    fn write_i32(&mut self, i: i32) {
        self.add(u64::from(i as u32));
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

type FxMap<K, V> = HashMap<K, V, BuildHasherDefault<Fx>>;

const NONE: u32 = u32::MAX;

fn quantise(p: [f64; 2]) -> (i32, i32) {
    ((p[0] / QUANTUM_DEG).round() as i32, (p[1] / QUANTUM_DEG).round() as i32)
}

/// A quantum as one sortable `u64` (the order is not numeric, only total).
fn pack_key(q: (i32, i32)) -> u64 {
    u64::from(q.0 as u32) << 32 | u64::from(q.1 as u32)
}

fn unpack_key(k: u64) -> (i32, i32) {
    ((k >> 32) as u32 as i32, k as u32 as i32)
}

fn point(q: (i32, i32)) -> [f64; 2] {
    [f64::from(q.0) * QUANTUM_DEG, f64::from(q.1) * QUANTUM_DEG]
}

/// One distinct arc: its vertex ids in canonical direction run from
/// `start` to `end` over `orig_len` points; the simplified ids are
/// `kept[kept_at..kept_at + kept_len]`.
struct Arc {
    start: u32,
    end: u32,
    orig_len: u32,
    kept_at: usize,
    kept_len: u32,
    uses: u32,
}

/// How a ring is written back.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RingKind {
    /// Its arcs, concatenated.
    Arcs,
    /// Fewer than 3 distinct points in: written back as it came.
    Degenerate,
    /// Still under 3 distinct points after a 0 m re-simplification of its
    /// arcs (a zero-area ring): `simplify_ring`'s 4-point floor, per ring.
    Floor,
}

/// The same junction rule as `gpui_maps::census::pack`'s `StateArcs`, which
/// re-cuts each state's (already shared) rings into arcs for storage; a
/// change to the rule belongs in both.
///
/// Simplifies every ring of every state's features in place at `tol_m`
/// (Douglas-Peucker in local metres), sharing each arc between the rings
/// that carry it. Output points sit exactly on the 1e-6 deg quantum.
pub fn simplify_national(states: &mut [(u8, Vec<Feature>)], tol_m: f64) -> TopoStats {
    // 1. Quantise: vertex ids, and every ring as ids (closed, without
    //    consecutive repeats). The f64 ring is cleared as it goes.
    let (total, ring_count) = states
        .iter()
        .flat_map(|(_, f)| f.iter())
        .flatten()
        .flatten()
        .fold((0usize, 0usize), |(n, r), ring| (n + ring.len(), r + 1));
    // Deduplicated a state at a time, then nationally (only state-line
    // vertices repeat across states): about half the points are distinct
    // in a planar partition (49-55 % in every state measured).
    let mut coords: Vec<u64> = Vec::with_capacity(total / 2 + 1);
    let mut chunk: Vec<u64> = Vec::new();
    for (_, features) in states.iter() {
        chunk.clear();
        chunk.extend(features.iter().flatten().flatten().flatten().map(|&p| pack_key(quantise(p))));
        chunk.sort_unstable();
        chunk.dedup();
        coords.extend_from_slice(&chunk);
    }
    drop(chunk);
    coords.sort_unstable();
    coords.dedup();
    let id_of = |q: (i32, i32)| coords.binary_search(&pack_key(q)).expect("every point was collected") as u32;
    let mut ids: Vec<u32> = Vec::with_capacity(total + ring_count);
    let mut ring_off: Vec<usize> = Vec::with_capacity(ring_count + 1);
    ring_off.push(0);
    for ring in states.iter_mut().flat_map(|(_, f)| f.iter_mut()).flatten().flatten() {
        let at = ids.len();
        for &p in ring.iter() {
            let id = id_of(quantise(p));
            if ids.len() == at || ids[ids.len() - 1] != id {
                ids.push(id);
            }
        }
        if ids.len() > at && ids[at] != ids[ids.len() - 1] {
            ids.push(ids[at]);
        }
        ring_off.push(ids.len());
        ring.clear();
    }
    let rings = ring_off.len() - 1;
    let ring_ids = |k: usize| &ids[ring_off[k]..ring_off[k + 1]];
    let mut kind: Vec<RingKind> =
        (0..rings).map(|k| if ring_ids(k).len() < 4 { RingKind::Degenerate } else { RingKind::Arcs }).collect();

    // 2. Junctions: up to two distinct neighbours per vertex, from each
    //    ring's previous and next vertex; a third marks a junction, as does
    //    being a ring's first point. (So every ring already starts at a
    //    junction and needs no rotation: an enclave's ring and its host's
    //    hole, which meet no third ring, are cut at both first points.)
    let mut nb: Vec<[u32; 2]> = vec![[NONE; 2]; coords.len()];
    let mut junction: Vec<bool> = vec![false; coords.len()];
    let mut add = |v: u32, u: u32, junction: &mut Vec<bool>| {
        let s = &mut nb[v as usize];
        if s[0] == u || s[1] == u {
        } else if s[0] == NONE {
            s[0] = u;
        } else if s[1] == NONE {
            s[1] = u;
        } else {
            junction[v as usize] = true;
        }
    };
    for (k, &ring_kind) in kind.iter().enumerate() {
        let r = ring_ids(k);
        if ring_kind == RingKind::Degenerate {
            for &v in r {
                junction[v as usize] = true;
            }
            continue;
        }
        let n = r.len() - 1; // r[n] == r[0]
        for i in 0..n {
            add(r[i], r[(i + n - 1) % n], &mut junction);
            add(r[i], r[i + 1], &mut junction);
        }
        junction[r[0] as usize] = true;
    }
    for (v, s) in nb.iter().enumerate() {
        if s[1] == NONE {
            junction[v] = true;
        }
    }
    drop(nb);
    let junctions = junction.iter().filter(|j| **j).count();

    // 3. Arcs: cut each ring at its junctions; key an arc by its canonical
    //    first step (start id, second id) -- the lesser of the forward and
    //    reversed steps. Interior vertices have exactly two neighbours, so
    //    the first step fixes the whole arc; a hit re-checks its far end
    //    and length. Simplified once, in canonical direction.
    let mut arc_of: FxMap<(u32, u32), u32> = FxMap::default();
    let mut arcs: Vec<Arc> = Vec::new();
    let mut kept: Vec<u32> = Vec::new();
    let mut ring_arcs: Vec<u32> = Vec::new(); // arc << 1 | reversed
    let mut ring_arc_off: Vec<usize> = vec![0];
    let mut pts: Vec<[f64; 2]> = Vec::new();
    let mut canon: Vec<u32> = Vec::new();
    let simplify_into = |canon: &[u32], tol: f64, pts: &mut Vec<[f64; 2]>, kept: &mut Vec<u32>| -> (usize, u32) {
        pts.clear();
        pts.extend(canon.iter().map(|&v| point(unpack_key(coords[v as usize]))));
        let keep = dp_keep(pts, tol, &vec![false; pts.len()]);
        let at = kept.len();
        kept.extend(canon.iter().zip(&keep).filter(|(_, k)| **k).map(|(v, _)| *v));
        (at, (kept.len() - at) as u32)
    };
    // Each ring's arcs as `(first index, last index, reversed)` segments.
    let segments = |r: &[u32], junction: &[bool], out: &mut Vec<(usize, usize)>| {
        out.clear();
        let mut i = 0;
        for j in 1..r.len() {
            if junction[r[j] as usize] {
                out.push((i, j));
                i = j;
            }
        }
    };
    let mut segs: Vec<(usize, usize)> = Vec::new();
    for (k, &ring_kind) in kind.iter().enumerate() {
        if ring_kind == RingKind::Arcs {
            let r = ring_ids(k);
            segments(r, &junction, &mut segs);
            for &(i, j) in &segs {
                let seg = &r[i..=j];
                let fwd = (seg[0], seg[1]);
                let rev = (seg[seg.len() - 1], seg[seg.len() - 2]);
                let reversed = rev < fwd;
                let key = fwd.min(rev);
                let arc = match arc_of.get(&key) {
                    Some(&a) => {
                        let found = &mut arcs[a as usize];
                        let end = if reversed { seg[0] } else { seg[seg.len() - 1] };
                        assert!(
                            found.end == end && found.orig_len as usize == seg.len(),
                            "an arc's first step fixes the arc: interior vertices have exactly two neighbours"
                        );
                        found.uses += 1;
                        a
                    }
                    None => {
                        canon.clear();
                        if reversed {
                            canon.extend(seg.iter().rev());
                        } else {
                            canon.extend_from_slice(seg);
                        }
                        let (kept_at, kept_len) = simplify_into(&canon, tol_m, &mut pts, &mut kept);
                        arcs.push(Arc {
                            start: canon[0],
                            end: canon[canon.len() - 1],
                            orig_len: canon.len() as u32,
                            kept_at,
                            kept_len,
                            uses: 1,
                        });
                        arc_of.insert(key, (arcs.len() - 1) as u32);
                        (arcs.len() - 1) as u32
                    }
                };
                ring_arcs.push(arc << 1 | u32::from(reversed));
            }
        }
        ring_arc_off.push(ring_arcs.len());
    }
    drop(arc_of);

    // 4. The floor: a ring whose arcs keep fewer than 3 distinct points
    //    re-simplifies those arcs at 0 m (only collinear points go). The
    //    arcs are shared, so its neighbours take the same points.
    let distinct = |k: usize, arcs: &[Arc], ring_arcs: &[u32]| -> usize {
        ring_arcs[ring_arc_off[k]..ring_arc_off[k + 1]].iter().map(|&a| arcs[(a >> 1) as usize].kept_len as usize - 1).sum()
    };
    let mut collapsed_rings = 0;
    for k in 0..rings {
        if kind[k] != RingKind::Arcs || distinct(k, &arcs, &ring_arcs) >= 3 {
            continue;
        }
        collapsed_rings += 1;
        let r = ring_ids(k);
        segments(r, &junction, &mut segs);
        for (&(i, j), &a) in segs.iter().zip(&ring_arcs[ring_arc_off[k]..ring_arc_off[k + 1]]) {
            canon.clear();
            if a & 1 == 1 {
                canon.extend(r[i..=j].iter().rev());
            } else {
                canon.extend_from_slice(&r[i..=j]);
            }
            let (kept_at, kept_len) = simplify_into(&canon, 0.0, &mut pts, &mut kept);
            let arc = &mut arcs[(a >> 1) as usize];
            debug_assert_eq!(arc.start, canon[0]);
            (arc.kept_at, arc.kept_len) = (kept_at, kept_len);
        }
        if distinct(k, &arcs, &ring_arcs) < 3 {
            kind[k] = RingKind::Floor;
        }
    }
    drop(junction);

    // 5. Reassemble, into each input ring's own (cleared) allocation.
    for (k, ring) in states.iter_mut().flat_map(|(_, f)| f.iter_mut()).flatten().flatten().enumerate() {
        let r = ring_ids(k);
        match kind[k] {
            RingKind::Degenerate => ring.extend(r.iter().map(|&v| point(unpack_key(coords[v as usize])))),
            // Rebuilt at `tol_m` alone, so its edges no longer match the 0 m
            // arcs its neighbours kept: MG-C3 counts those edges (the ring
            // still counts in `uses` but not in the shared runs), so this is
            // flagged, not silent. 0 such rings nationally.
            RingKind::Floor => {
                let orig: Vec<[f64; 2]> = r.iter().map(|&v| point(unpack_key(coords[v as usize]))).collect();
                *ring = simplify_ring(&orig, tol_m, &vec![false; orig.len()]);
            }
            RingKind::Arcs => {
                for &a in &ring_arcs[ring_arc_off[k]..ring_arc_off[k + 1]] {
                    let arc = &arcs[(a >> 1) as usize];
                    let run = &kept[arc.kept_at..arc.kept_at + arc.kept_len as usize];
                    let mut push = |v: u32| {
                        if ring.is_empty() || pack_key(quantise(ring[ring.len() - 1])) != coords[v as usize] {
                            ring.push(point(unpack_key(coords[v as usize])));
                        }
                    };
                    if a & 1 == 1 {
                        run.iter().rev().for_each(|&v| push(v));
                    } else {
                        run.iter().for_each(|&v| push(v));
                    }
                }
            }
        }
    }
    let unique_vertices = coords.len();
    drop(ids);
    drop(coords);

    // MG-C3: every simplified directed edge on an arc two or more rings
    // use must appear reversed somewhere. From the rings' arc lists, not
    // the arc table's bookkeeping, and sized before it is filled.
    let shared_runs = || {
        (0..rings)
            .filter(|&k| kind[k] == RingKind::Arcs)
            .flat_map(|k| ring_arcs[ring_arc_off[k]..ring_arc_off[k + 1]].iter())
            .filter(|&&a| arcs[(a >> 1) as usize].uses >= 2)
    };
    let edge_count: usize = shared_runs().map(|&a| arcs[(a >> 1) as usize].kept_len as usize - 1).sum();
    let mut shared_edges: Vec<u64> = Vec::with_capacity(edge_count);
    for &a in shared_runs() {
        let arc = &arcs[(a >> 1) as usize];
        let run = &kept[arc.kept_at..arc.kept_at + arc.kept_len as usize];
        for w in run.windows(2) {
            let (from, to) = if a & 1 == 1 { (w[1], w[0]) } else { (w[0], w[1]) };
            shared_edges.push(u64::from(from) << 32 | u64::from(to));
        }
    }
    shared_edges.sort_unstable();
    let unmatched_interior_edges =
        shared_edges.iter().filter(|&&e| shared_edges.binary_search(&e.rotate_right(32)).is_err()).count();
    TopoStats {
        unique_vertices,
        junctions,
        arcs: arcs.len(),
        unmatched_interior_edges,
        single_use_arcs: arcs.iter().filter(|a| a.uses == 1).count(),
        arc_points: arcs.iter().map(|a| a.kept_len as usize).sum(),
        collapsed_rings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two unit squares sharing the edge x = 1e-3, with a wiggly shared
    /// edge that simplification will thin.
    fn neighbours() -> Vec<(u8, Vec<Feature>)> {
        let wiggle: Vec<[f64; 2]> = (0..=20).map(|i| [1e-3 + if i % 2 == 0 { 0.0 } else { 3e-6 }, f64::from(i) * 5e-5]).collect();
        let mut left = vec![[0.0, 0.0]];
        left.extend(wiggle.iter().copied());
        left.push([0.0, 1e-3]);
        left.push([0.0, 0.0]);
        let mut right = vec![[2e-3, 0.0], [2e-3, 1e-3]];
        right.extend(wiggle.iter().rev().copied());
        right.push([2e-3, 0.0]);
        vec![(1, vec![vec![vec![left]]]), (2, vec![vec![vec![right]]])]
    }

    #[test]
    fn neighbours_share_identical_simplified_edges() {
        let mut states = neighbours();
        let stats = simplify_national(&mut states, 5.0);
        assert_eq!(stats.unmatched_interior_edges, 0);
        let edges = |ring: &Vec<[f64; 2]>| -> std::collections::HashSet<[i64; 4]> {
            ring.windows(2)
                .map(|w| [(w[0][0] * 1e6).round() as i64, (w[0][1] * 1e6).round() as i64, (w[1][0] * 1e6).round() as i64, (w[1][1] * 1e6).round() as i64])
                .collect()
        };
        let l = edges(&states[0].1[0][0][0]);
        let r = edges(&states[1].1[0][0][0]);
        let shared = l.iter().filter(|e| r.contains(&[e[2], e[3], e[0], e[1]])).count();
        assert!(shared >= 1, "the shared edge survives, reversed, in both");
        assert!(states[0].1[0][0][0].len() < 24, "and was simplified");
    }

    #[test]
    fn a_junction_of_three_rings_is_kept() {
        // Three wedges meeting at the origin: the origin must survive in all three.
        let wedge = |a: f64, b: f64| {
            let p = |t: f64| [1e-3 * t.cos(), 1e-3 * t.sin()];
            vec![[0.0, 0.0], p(a), p((a + b) / 2.0), p(b), [0.0, 0.0]]
        };
        let tau = std::f64::consts::TAU;
        let mut states = vec![(1u8, vec![vec![vec![wedge(0.0, tau / 3.0)]], vec![vec![wedge(tau / 3.0, 2.0 * tau / 3.0)]], vec![vec![wedge(2.0 * tau / 3.0, tau)]]])];
        simplify_national(&mut states, 50.0);
        for f in &states[0].1 {
            assert!(f[0][0].contains(&[0.0, 0.0]));
        }
    }

    #[test]
    fn edges_across_a_state_line_match_too() {
        let mut states = neighbours(); // fips 1 and 2: a state border
        assert_eq!(simplify_national(&mut states, 5.0).unmatched_interior_edges, 0);
    }

    /// Every directed edge of a ring, in quanta.
    fn edge_set(ring: &[[f64; 2]]) -> std::collections::HashSet<[i64; 4]> {
        let q = |v: f64| (v * 1e6).round() as i64;
        ring.windows(2).map(|w| [q(w[0][0]), q(w[0][1]), q(w[1][0]), q(w[1][1])]).collect()
    }

    fn reversed(edges: &std::collections::HashSet<[i64; 4]>) -> std::collections::HashSet<[i64; 4]> {
        edges.iter().map(|e| [e[2], e[3], e[0], e[1]]).collect()
    }

    /// A wiggly square ring (40 points a side, 3e-6 deg of wiggle), starting
    /// at corner `start` (0..4), clockwise when `cw`.
    fn wiggly_square(x0: f64, y0: f64, side: f64, start: usize, cw: bool) -> Vec<[f64; 2]> {
        square(x0, y0, side, start, cw, 40, 3e-6)
    }

    /// A square ring of `n` points a side, every other one pushed `wiggle`
    /// deg off the side.
    fn square(x0: f64, y0: f64, side: f64, start: usize, cw: bool, n: u32, wiggle: f64) -> Vec<[f64; 2]> {
        let corners = [[x0, y0], [x0 + side, y0], [x0 + side, y0 + side], [x0, y0 + side]];
        let mut ring = Vec::new();
        for c in 0..4 {
            let (a, b) = (corners[(start + c) % 4], corners[(start + c + 1) % 4]);
            for i in 0..n {
                let t = f64::from(i) / f64::from(n);
                let w = if i % 2 == 1 { wiggle } else { 0.0 };
                ring.push([a[0] + (b[0] - a[0]) * t + w, a[1] + (b[1] - a[1]) * t + w]);
            }
        }
        ring.push(ring[0]);
        if cw {
            ring.reverse();
        }
        ring
    }

    /// An enclave meets no third ring, so it has no junction but the rings'
    /// first points: its ring and its host's hole start at different
    /// corners and must still come out as the same edges, reversed.
    #[test]
    fn an_enclave_and_its_hosts_hole_match() {
        let outer = wiggly_square(0.0, 0.0, 3e-3, 0, false);
        let hole = wiggly_square(1e-3, 1e-3, 1e-3, 0, true);
        let enclave = wiggly_square(1e-3, 1e-3, 1e-3, 2, false);
        let mut states = vec![(44u8, vec![vec![vec![outer, hole]], vec![vec![enclave]]])];
        let stats = simplify_national(&mut states, 5.0);
        assert_eq!(stats.unmatched_interior_edges, 0);
        let hole = &states[0].1[0][0][1];
        let enclave = &states[0].1[1][0][0];
        assert!(hole.len() < 100 && hole.len() >= 4, "simplified: {}", hole.len());
        assert_eq!(edge_set(enclave), reversed(&edge_set(hole)), "the same edges, opposite ways");
    }

    /// A vertex three rings share on a straight run is a junction, kept
    /// where a straight edge would otherwise drop it (a T-junction).
    #[test]
    fn a_t_junction_on_a_straight_edge_is_kept() {
        let j = [1e-3, 0.0];
        let top = vec![[0.0, 0.0], j, [2e-3, 0.0], [2e-3, 1e-3], [0.0, 1e-3], [0.0, 0.0]];
        let below_a = vec![[0.0, 0.0], [0.0, -1e-3], [1e-3, -1e-3], j, [0.0, 0.0]];
        let below_b = vec![j, [1e-3, -1e-3], [2e-3, -1e-3], [2e-3, 0.0], j];
        let mut states = vec![(1u8, vec![vec![vec![top]], vec![vec![below_a]], vec![vec![below_b]]])];
        let stats = simplify_national(&mut states, 5.0);
        assert!(states[0].1[0][0][0].contains(&j));
        assert_eq!(stats.unmatched_interior_edges, 0);
    }

    /// A ring smaller than the tolerance keeps at least 4 points, and its
    /// neighbour keeps the same ones.
    #[test]
    fn a_ring_under_the_tolerance_does_not_collapse() {
        let tiny = square(1e-3, 1e-3, 2e-5, 0, false, 4, 0.0); // about 2 m
        let hole = square(1e-3, 1e-3, 2e-5, 1, true, 4, 0.0);
        let outer = wiggly_square(0.0, 0.0, 3e-3, 0, false);
        let mut states = vec![(1u8, vec![vec![vec![outer, hole]], vec![vec![tiny]]])];
        let stats = simplify_national(&mut states, 5.0);
        assert!(stats.collapsed_rings >= 1);
        let hole = &states[0].1[0][0][1];
        let tiny = &states[0].1[1][0][0];
        assert!(tiny.len() >= 4 && hole.len() >= 4);
        assert_eq!(tiny.first(), tiny.last());
        assert_eq!(edge_set(tiny), reversed(&edge_set(hole)));
        assert_eq!(stats.unmatched_interior_edges, 0);
    }

    /// Two rings running one shared edge the same way (an overlap, not a
    /// neighbour) is what MG-C3 exists to catch.
    #[test]
    fn a_shared_edge_run_the_same_way_is_unmatched() {
        let a = vec![[0.0, 0.0], [1e-3, 0.0], [1e-3, 1e-3], [0.0, 1e-3], [0.0, 0.0]];
        let b = vec![[0.0, 0.0], [1e-3, 0.0], [1e-3, -1e-3], [0.0, -1e-3], [0.0, 0.0]];
        let mut states = vec![(1u8, vec![vec![vec![a]], vec![vec![b]]])];
        assert!(simplify_national(&mut states, 5.0).unmatched_interior_edges > 0);
    }

    #[test]
    fn stats_count_vertices_junctions_and_arcs() {
        let mut states = neighbours();
        let stats = simplify_national(&mut states, 5.0);
        // 21 wiggle + 2 left + 2 right corners.
        assert_eq!(stats.unique_vertices, 25);
        // Both wiggle ends (3 neighbours) and the two first points.
        assert_eq!(stats.junctions, 4);
        // The wiggle, the left's two outline runs, the right's two.
        assert_eq!(stats.arcs, 5);
        assert_eq!(stats.single_use_arcs, 4);
        assert_eq!(stats.collapsed_rings, 0);
    }
}

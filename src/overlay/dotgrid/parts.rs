//! Block parts: how a census block's people are divided among its triangles
//! (spec `2026-09-27-us-race-dot-map-design.md` §3.2).
//!
//! The converter triangulates each populated block (Task 5) into [`Tri`]s on
//! the 2^31 world grid ([`WORLD_QUANTA`] quanta per world side). [`allocate`]
//! gives every triangle its share of each group's count by area, exactly and
//! deterministically on integers, so every shell and the converter agree bit
//! for bit. [`split_part`] then subdivides a part until it holds at most
//! 65,535 people (the composition texel's u16 denominator, §3.3) and fits the
//! part-tile extent cap. On the GPU, a part's dots are placed by
//! [`sample_in_triangle`] and hashed by [`part_key`].

use super::composition::GROUPS;
use super::hash::{cell_key, pcg};

/// World-grid quanta per side of the Web Mercator world square: triangle
/// vertices are integers in `[0, WORLD_QUANTA)`.
pub const WORLD_QUANTA: i64 = 1 << 31;

/// A triangle on the world grid, in [`WORLD_QUANTA`] units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Tri {
    pub a: [i64; 2],
    pub b: [i64; 2],
    pub c: [i64; 2],
}

/// One block part: a triangle and the people of each group it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Part {
    pub tri: Tri,
    pub c: [u32; GROUPS],
}

/// Twice the triangle's area, exact: `|(b − a) × (c − a)|`. Coordinates are
/// below 2^31, so each product is below 2^62 and the i128 never overflows.
fn doubled_area(t: &Tri) -> i128 {
    let (ex, ey) = (i128::from(t.b[0] - t.a[0]), i128::from(t.b[1] - t.a[1]));
    let (fx, fy) = (i128::from(t.c[0] - t.a[0]), i128::from(t.c[1] - t.a[1]));
    (ex * fy - ey * fx).abs()
}

/// The larger of the triangle's bounding-box width and height, in quanta.
pub fn extent(t: &Tri) -> i64 {
    let xs = [t.a[0], t.b[0], t.c[0]];
    let ys = [t.a[1], t.b[1], t.c[1]];
    let w = xs.iter().max().unwrap() - xs.iter().min().unwrap();
    let h = ys.iter().max().unwrap() - ys.iter().min().unwrap();
    w.max(h)
}

/// Divide a block's group counts among its triangles by area.
///
/// Each group is allocated independently by largest remainder over the exact
/// doubled areas: triangle `i` gets `floor(n·area_i / Σarea)`, and the
/// leftover people go one each to the largest remainders. A tie on the
/// remainder goes to the lower `pcg(salt ^ i)` (salt folded to 32 bits as
/// `lo ^ hi`), then to the lower index. When the total area is 0 (lyon
/// slivers after rounding, or degenerate split children -- C27) the first
/// triangle takes everything.
///
/// Every group's count is conserved exactly. Only triangles that end up
/// with at least one person are returned, in `tris` order.
///
/// # Panics
/// If `tris` is empty while `block_counts` holds people: they would be lost.
pub fn allocate(block_counts: &[u32; GROUPS], tris: &[Tri], salt: u64) -> Vec<Part> {
    let people: u64 = block_counts.iter().map(|&n| u64::from(n)).sum();
    if people == 0 {
        return Vec::new();
    }
    assert!(!tris.is_empty(), "allocate: {people} people and no triangles");

    let mut counts = vec![[0u32; GROUPS]; tris.len()];
    let areas: Vec<i128> = tris.iter().map(doubled_area).collect();
    let total: i128 = areas.iter().sum();

    if total == 0 {
        counts[0] = *block_counts;
    } else {
        let s = (salt as u32) ^ ((salt >> 32) as u32);
        // The tie order is the same for every group; compute it once.
        let tie: Vec<u32> = (0..tris.len()).map(|i| pcg(s ^ i as u32)).collect();
        let mut rem: Vec<(i128, usize)> = Vec::with_capacity(tris.len());
        for (g, &n) in block_counts.iter().enumerate() {
            if n == 0 {
                continue;
            }
            rem.clear();
            let mut given = 0u32;
            for (i, &area) in areas.iter().enumerate() {
                let q = i128::from(n) * area;
                let floor = (q / total) as u32;
                counts[i][g] = floor;
                given += floor;
                rem.push((q % total, i));
            }
            let left = (n - given) as usize;
            if left > 0 {
                rem.sort_unstable_by(|&(ra, ia), &(rb, ib)| {
                    rb.cmp(&ra).then(tie[ia].cmp(&tie[ib])).then(ia.cmp(&ib))
                });
                for &(_, i) in &rem[..left] {
                    counts[i][g] += 1;
                }
            }
        }
    }

    tris.iter()
        .zip(counts)
        .filter(|(_, c)| c.iter().any(|&n| n > 0))
        .map(|(&tri, c)| Part { tri, c })
        .collect()
}

/// Floor midpoint of two grid points (an arithmetic shift, so it also rounds
/// toward −∞ for negative coordinates).
fn mid(p: [i64; 2], q: [i64; 2]) -> [i64; 2] {
    [(p[0] + q[0]) >> 1, (p[1] + q[1]) >> 1]
}

/// A deterministic 64-bit salt from a triangle's vertices, for the
/// allocation over its children.
fn tri_salt(t: &Tri) -> u64 {
    [t.a, t.b, t.c].iter().flatten().fold(0xCBF2_9CE4_8422_2325u64, |h, &v| {
        (h ^ v as u64).wrapping_mul(0x0000_0100_0000_01B3).rotate_left(29)
    })
}

/// Split a part until it holds at most `max_people` people and its
/// [`extent`] is at most `max_extent` quanta.
///
/// A part over either cap is midpoint-subdivided into 4 children (the three
/// corners and the middle), its people re-allocated over them by
/// [`allocate`], and each child split recursively. Subdivision stops at an
/// extent of 1 quantum (C27): a part that small and still over the people
/// cap is duplicated, the copies sharing the triangle and dividing the counts
/// (group by group, filling each copy to `max_people` in turn). Such copies
/// share a [`part_key`] and so draw coincident dots; at 1 quantum (about
/// 2 cm on the ground) with more than `max_people` people this never
/// happens on real blocks.
///
/// # Panics
/// If `max_people` is 0.
pub fn split_part(p: Part, max_people: u32, max_extent: i64) -> Vec<Part> {
    assert!(max_people > 0, "split_part: max_people must be positive");
    let mut out = Vec::new();
    split_into(p, max_people, max_extent, &mut out);
    out
}

fn split_into(p: Part, max_people: u32, max_extent: i64, out: &mut Vec<Part>) {
    let people: u64 = p.c.iter().map(|&n| u64::from(n)).sum();
    let ext = extent(&p.tri);
    let over_people = people > u64::from(max_people);
    if !over_people && ext <= max_extent {
        out.push(p);
        return;
    }
    if ext <= 1 {
        // Too small to subdivide: meet the people cap by duplication.
        duplicate_into(p, max_people, out);
        return;
    }
    let Tri { a, b, c } = p.tri;
    let (ab, bc, ca) = (mid(a, b), mid(b, c), mid(c, a));
    let children = [
        Tri { a, b: ab, c: ca },
        Tri { a: ab, b, c: bc },
        Tri { a: ca, b: bc, c },
        Tri { a: ab, b: bc, c: ca },
    ];
    // Each child's extent is at most ceil(ext / 2) < ext for ext >= 2, so
    // the recursion ends.
    for child in allocate(&p.c, &children, tri_salt(&p.tri)) {
        split_into(child, max_people, max_extent, out);
    }
}

fn duplicate_into(p: Part, max_people: u32, out: &mut Vec<Part>) {
    let mut left = p.c;
    let mut g = 0;
    while g < GROUPS {
        let mut c = [0u32; GROUPS];
        let mut room = max_people;
        while g < GROUPS && room > 0 {
            let take = left[g].min(room);
            c[g] = take;
            left[g] -= take;
            room -= take;
            if left[g] == 0 {
                g += 1;
            }
        }
        if c.iter().any(|&n| n > 0) {
            out.push(Part { tri: p.tri, c });
        }
    }
}

/// A point of the triangle `a`, `a + e1`, `a + e2` from two unit samples.
///
/// `(u, v)` outside the lower-left half of the unit square is folded back
/// by `(1 − u, 1 − v)`, so the map is uniform over the triangle. The fold
/// test is written `u > 1 − v` rather than `u + v > 1`: for hash samples
/// (24-bit fractions, `hash::to_unit`) `1 − v` and the comparison are exact
/// in f32, so Task 6's WGSL should use this same form to fold identically.
pub fn sample_in_triangle(a: [f64; 2], e1: [f64; 2], e2: [f64; 2], u: f32, v: f32) -> [f64; 2] {
    let (mut u, mut v) = (f64::from(u), f64::from(v));
    if u > 1.0 - v {
        u = 1.0 - u;
        v = 1.0 - v;
    }
    [a[0] + u * e1[0] + v * e2[0], a[1] + u * e1[1] + v * e2[1]]
}

/// A part's dot hash key, from all three vertices (C13): `origin` is vertex
/// `a`, and `e1 = b − a`, `e2 = c − a` are the shape texel's two edges, all
/// as the exact f32s the vertex shader loads (the tile's lyon units).
///
/// ```text
/// part_key = cell_key(a) ^ pcg(bits(e1.x) ^ pcg(bits(e1.y) ^ pcg(bits(e2.x) ^ pcg(bits(e2.y)))))
/// cell_key(a) = bits(a.x) ^ pcg(bits(a.y))
/// ```
///
/// where `bits` is `f32::to_bits` / WGSL `bitcast<u32>` and all arithmetic
/// is wrapping u32. Keying on the vertex `a` alone gave fan triangles and
/// split children identical dot patterns; the ruling's shorter example
/// (`e1.x` and `e2.y` only) still collides for the two halves of an
/// axis-aligned square, so all four edge components are hashed.
pub fn part_key(a: [f32; 2], e1: [f32; 2], e2: [f32; 2]) -> u32 {
    let edges = pcg(e1[0].to_bits()
        ^ pcg(e1[1].to_bits() ^ pcg(e2[0].to_bits() ^ pcg(e2[1].to_bits()))));
    cell_key(a) ^ edges
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sq(x0: i64, y0: i64, s: i64) -> Vec<Tri> {
        vec![
            Tri { a: [x0, y0], b: [x0 + s, y0], c: [x0 + s, y0 + s] },
            Tri { a: [x0, y0], b: [x0 + s, y0 + s], c: [x0, y0 + s] },
        ]
    }

    fn sums(parts: &[Part]) -> [u32; 8] {
        let mut sum = [0u32; 8];
        for p in parts {
            for (s, &n) in sum.iter_mut().zip(&p.c) {
                *s += n;
            }
        }
        sum
    }

    #[test]
    fn allocation_conserves_every_group_exactly() {
        let c = [7, 13, 0, 1, 0, 0, 2, 5];
        let tris = sq(0, 0, 1000);
        let parts = allocate(&c, &tris, 42);
        assert_eq!(sums(&parts), c);
        assert!(parts.iter().all(|p| p.c.iter().sum::<u32>() > 0), "no empty parts are emitted");
    }

    #[test]
    fn allocation_is_deterministic() {
        let c = [3, 3, 3, 3, 3, 3, 3, 3];
        assert_eq!(allocate(&c, &sq(0, 0, 10), 9), allocate(&c, &sq(0, 0, 10), 9));
    }

    #[test]
    fn allocation_follows_area() {
        let tris = vec![
            Tri { a: [0, 0], b: [900, 0], c: [0, 900] },
            Tri { a: [0, 0], b: [100, 0], c: [0, 100] },
        ];
        let parts = allocate(&[1000, 0, 0, 0, 0, 0, 0, 0], &tris, 1);
        let big = parts.iter().find(|p| p.tri == tris[0]).unwrap().c[0];
        assert!((985..=990).contains(&big), "{big}"); // 810000 / 820000 * 1000 = 987.8
    }

    #[test]
    fn allocation_with_zero_total_area_gives_everything_to_the_first_triangle() {
        // C27: collinear slivers (lyon output after rounding).
        let tris = vec![
            Tri { a: [0, 0], b: [10, 0], c: [20, 0] },
            Tri { a: [5, 5], b: [5, 5], c: [5, 9] },
        ];
        let c = [4, 0, 9, 0, 0, 1, 0, 0];
        let parts = allocate(&c, &tris, 3);
        assert_eq!(parts, vec![Part { tri: tris[0], c }]);
    }

    #[test]
    fn allocation_ties_follow_the_salt_not_the_index() {
        // Two equal halves, one person: the remainder tie is broken by
        // pcg(salt ^ i), so some salt gives it to each half.
        let tris = sq(0, 0, 100);
        let winners: std::collections::HashSet<Tri> = (0..64u64)
            .map(|salt| allocate(&[1, 0, 0, 0, 0, 0, 0, 0], &tris, salt)[0].tri)
            .collect();
        assert_eq!(winners.len(), 2);
    }

    #[test]
    fn a_part_over_65535_people_is_split() {
        let p = Part { tri: sq(0, 0, 1 << 20)[0], c: [70_000, 0, 0, 0, 0, 0, 0, 1] };
        let out = split_part(p, 65_535, i64::MAX);
        assert!(out.len() >= 2);
        assert!(out.iter().all(|q| q.c.iter().sum::<u32>() <= 65_535));
        assert_eq!(out.iter().map(|q| q.c[0]).sum::<u32>(), 70_000);
    }

    #[test]
    fn a_part_wider_than_max_extent_is_subdivided() {
        let p = Part { tri: sq(0, 0, 4096)[0], c: [10, 0, 0, 0, 0, 0, 0, 0] };
        let out = split_part(p, 65_535, 1024);
        assert!(out.iter().all(|q| extent(&q.tri) <= 1024));
        assert_eq!(out.iter().map(|q| q.c[0]).sum::<u32>(), 10);
    }

    #[test]
    fn a_part_within_both_caps_is_returned_unchanged() {
        let p = Part { tri: sq(0, 0, 4096)[1], c: [1, 2, 3, 4, 5, 6, 7, 8] };
        assert_eq!(split_part(p, 65_535, 4096), vec![p]);
    }

    #[test]
    fn a_one_quantum_part_over_the_people_cap_is_duplicated() {
        // C27: subdivision stops at extent 1; the cap is met by copies.
        let tri = Tri { a: [7, 7], b: [8, 7], c: [8, 8] };
        let c = [200_000, 3, 0, 0, 0, 0, 0, 65_535];
        let out = split_part(Part { tri, c }, 65_535, i64::MAX);
        assert!(out.iter().all(|q| q.tri == tri));
        assert!(out.iter().all(|q| (1..=65_535).contains(&q.c.iter().sum::<u32>())));
        assert_eq!(out.len(), 5); // ceil(265_538 / 65_535)
        assert_eq!(sums(&out), c);
    }

    #[test]
    fn a_wide_zero_area_part_over_the_people_cap_terminates() {
        // Collinear: every child has area 0, so allocate hands all people
        // to the first child until the extent reaches 1, then duplicates.
        let tri = Tri { a: [0, 0], b: [1 << 30, 0], c: [1 << 29, 0] };
        let c = [100_000, 0, 0, 0, 0, 0, 0, 0];
        let out = split_part(Part { tri, c }, 65_535, 1 << 10);
        assert!(out.iter().all(|q| q.c.iter().sum::<u32>() <= 65_535));
        assert!(out.iter().all(|q| extent(&q.tri) <= 1 << 10));
        assert_eq!(sums(&out), c);
    }

    #[test]
    fn splitting_conserves_every_group_at_world_scale() {
        let tri = Tri { a: [123_456, 7_890_123], b: [WORLD_QUANTA - 1, 5], c: [99, WORLD_QUANTA - 3] };
        let c = [90_000, 12_345, 7, 0, 65_535, 1, 2, 3];
        let out = split_part(Part { tri, c }, 65_535, 1 << 24);
        assert_eq!(sums(&out), c);
        assert!(out.iter().all(|q| q.c.iter().sum::<u32>() <= 65_535 && extent(&q.tri) <= 1 << 24));
    }

    #[test]
    fn samples_stay_inside_the_triangle() {
        let (a, e1, e2) = ([10.0, 20.0], [5.0, 0.0], [0.0, 3.0]);
        for k in 0..10_000u32 {
            let (u, v) = (((k * 7919) % 10_000) as f32 / 10_000.0, ((k * 104_729) % 10_000) as f32 / 10_000.0);
            let p = sample_in_triangle(a, e1, e2, u, v);
            let (s, t) = ((p[0] - a[0]) / 5.0, (p[1] - a[1]) / 3.0);
            assert!(s >= -1e-9 && t >= -1e-9 && s + t <= 1.0 + 1e-9, "{p:?}");
        }
    }

    #[test]
    fn the_fold_is_exact_on_the_diagonal_for_24_bit_samples() {
        // u + v = 1 + 2^-24 rounds to 1 in f32, so `u + v > 1` would not
        // fold it; `u > 1 - v` does.
        let u = 0.5f32 + 2f32.powi(-24);
        let v = 0.5f32;
        assert!(u + v <= 1.0, "the f32 sum rounds down to 1");
        assert!(u > 1.0 - v, "the f32 form Task 6 uses folds");
        let p = sample_in_triangle([0.0, 0.0], [1.0, 0.0], [0.0, 1.0], u, v);
        assert!(p[0] + p[1] <= 1.0, "{p:?}");
    }

    #[test]
    fn part_keys_differ_for_triangles_sharing_vertex_a() {
        // C13: the two halves of a square, and a fan, share `a`.
        let k = |a: [f32; 2], b: [f32; 2], c: [f32; 2]| {
            part_key(a, [b[0] - a[0], b[1] - a[1]], [c[0] - a[0], c[1] - a[1]])
        };
        let o = [3.0, 4.0];
        let keys = [
            k(o, [4.0, 4.0], [4.0, 5.0]),
            k(o, [4.0, 5.0], [3.0, 5.0]),
            k(o, [3.0, 5.0], [2.0, 5.0]),
            k(o, [4.0, 4.0], [3.5, 4.5]),
        ];
        let distinct: std::collections::HashSet<u32> = keys.iter().copied().collect();
        assert_eq!(distinct.len(), keys.len(), "{keys:?}");
        assert!(keys.iter().all(|&key| key != cell_key(o)));
    }
}

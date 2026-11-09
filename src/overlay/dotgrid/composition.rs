//! A dot's race/ethnicity group: the CPU mirror of the shader's categorical
//! rule (spec `2026-09-27-us-race-dot-map-design.md` §3.3).
//!
//! A record (a block part, or a grid cell) of `P` people carries 8 group
//! counts. With `D = min(P, 65535)`, dot `i` of the record maps to a person
//! `p = (stride * (i % D) + off) % D`, and its group is the one whose
//! cumulative range holds `p`. `gcd(stride, D) = 1`, so `i -> p` is a
//! bijection on `[0, D)`: at ppd 1 (`hash::dot_alive` keeps exactly
//! `i < P`) every group gets exactly its count. At ppd > 1 the alive dots
//! are a prefix `i < n`, and a stride whose `stride / D` has small
//! continued-fraction partial quotients makes every prefix a
//! low-discrepancy sample of the groups (C1 ruling). Nothing reads `ppd`,
//! so zooming never recolours a dot.
//!
//! The stride travels in the composition texel, so how it is chosen is
//! CPU-only; `off` and `p` are recomputed in WGSL (Task 6) from the texel
//! and checked against [`group_of`].

use std::sync::atomic::{AtomicU16, Ordering};

use super::hash::pcg;

/// Race/ethnicity groups per record (spec §2.1).
pub const GROUPS: usize = 8;

/// The largest denominator a composition texel can hold (u16).
const MAX_DENOM: u32 = 65_535;

/// XORed into the dot hash key for the per-record permutation offset, so
/// `off` is not correlated with the dot hashes that `key` also seeds.
pub const OFFSET_SALT: u32 = 0x9E37_79B9;

/// One record's group boundaries and stride, as the shader reads them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Composition {
    /// Cumulative counts after groups 0..=6, in units of `denom`; group 7
    /// ends at `denom` itself.
    pub bounds: [u16; 7],
    /// `D = min(P, 65535)`.
    pub denom: u16,
    /// [`stride_for`]`(denom)`.
    pub stride: u16,
}

impl Composition {
    /// The composition of a record with these group counts. Exact
    /// cumulative counts when the total is at most 65,535; otherwise the
    /// counts are largest-remainder rounded to 65,535 (ties to the lower
    /// group index).
    pub fn from_counts(c: &[u32; GROUPS]) -> Self {
        let total: u64 = c.iter().map(|&n| u64::from(n)).sum();
        let scaled: [u32; GROUPS] = if total <= u64::from(MAX_DENOM) {
            *c
        } else {
            largest_remainder(c, total)
        };
        let denom = scaled.iter().sum::<u32>() as u16;
        let mut bounds = [0u16; 7];
        let mut acc = 0u32;
        for (b, &n) in bounds.iter_mut().zip(&scaled) {
            acc += n;
            *b = acc as u16;
        }
        Composition { bounds, denom, stride: stride_for(denom) }
    }

    /// The `Rgba32Uint` texel: `[c0|c1, c2|c3, c4|c5, c6|stride]`, low
    /// half first.
    pub fn texel(&self) -> [u32; 4] {
        let b = self.bounds.map(u32::from);
        [b[0] | b[1] << 16, b[2] | b[3] << 16, b[4] | b[5] << 16, b[6] | u32::from(self.stride) << 16]
    }
}

/// `c` scaled to sum to exactly [`MAX_DENOM`]: floors, then one more to
/// the largest remainders, lower index first on a tie.
fn largest_remainder(c: &[u32; GROUPS], total: u64) -> [u32; GROUPS] {
    let target = u64::from(MAX_DENOM);
    let mut out = [0u32; GROUPS];
    let mut rem = [0u64; GROUPS];
    for g in 0..GROUPS {
        let x = u64::from(c[g]) * target;
        out[g] = (x / total) as u32;
        rem[g] = x % total;
    }
    let short = target - out.iter().map(|&n| u64::from(n)).sum::<u64>();
    let mut order: [usize; GROUPS] = std::array::from_fn(|g| g);
    // Stable sort: equal remainders keep index order.
    order.sort_by(|&a, &b| rem[b].cmp(&rem[a]));
    for &g in order.iter().take(short as usize) {
        out[g] += 1;
    }
    out
}

/// Every prime below 65,536 (there are 6,542), ascending.
static PRIMES: [u16; 6542] = primes();

const fn primes() -> [u16; 6542] {
    const N: usize = 65_536;
    let mut composite = [false; N];
    let mut i = 2;
    while i * i < N {
        if !composite[i] {
            let mut j = i * i;
            while j < N {
                composite[j] = true;
                j += i;
            }
        }
        i += 1;
    }
    let mut out = [0u16; 6542];
    let mut n = 0;
    let mut k = 2;
    while k < N {
        if !composite[k] {
            out[n] = k as u16;
            n += 1;
        }
        k += 1;
    }
    assert!(n == 6542);
    out
}

/// The largest partial quotient of `s / d`'s continued fraction
/// (Euclid's quotients of `d / s` on, the leading 0 skipped), or
/// anything `>= cap` once it reaches `cap`.
fn max_partial_quotient(s: u32, d: u32, cap: u32) -> u32 {
    let (mut a, mut b) = (d, s);
    let mut worst = 0;
    while b != 0 {
        worst = worst.max(a / b);
        if worst >= cap {
            return worst;
        }
        (a, b) = (b, a % b);
    }
    worst
}

/// The stride for denominator `d` (C1 ruling): 1 when `d <= 2`, else the
/// prime in `(d/2, d)` whose `s / d` has the smallest largest
/// continued-fraction partial quotient, ties to the prime nearest
/// `0.618 d` (then the lower). Such a prime exists (Bertrand) and cannot
/// divide `d`, so `gcd(s, d) = 1`.
///
/// The search reads up to ~3,000 primes, so each answer is memoised
/// (a 128 KB table, filled on demand; 0 = not yet computed).
pub fn stride_for(d: u16) -> u16 {
    static MEMO: [AtomicU16; 65_536] = [const { AtomicU16::new(0) }; 65_536];
    if d <= 2 {
        return 1;
    }
    let slot = &MEMO[usize::from(d)];
    match slot.load(Ordering::Relaxed) {
        0 => {
            let s = search_stride(d);
            slot.store(s, Ordering::Relaxed);
            s
        }
        s => s,
    }
}

fn search_stride(d: u16) -> u16 {
    let d32 = u32::from(d);
    let lo = PRIMES.partition_point(|&p| 2 * u32::from(p) <= d32);
    let hi = PRIMES.partition_point(|&p| p < d);
    let range = &PRIMES[lo..hi];
    let target = 0.618 * f64::from(d);
    let dist = |p: u16| (f64::from(p) - target).abs();
    // Walk outward from the prime nearest the target, so a later prime
    // wins only on a strictly smaller quotient.
    let mut right = range.partition_point(|&p| f64::from(p) < target);
    let mut left = right; // range[..left] are below the target
    let mut best = (u32::MAX, range[0]);
    loop {
        let next = match (left.checked_sub(1).map(|l| range[l]), range.get(right).copied()) {
            (None, None) => break,
            (Some(l), None) => {
                left -= 1;
                l
            }
            (None, Some(r)) => {
                right += 1;
                r
            }
            (Some(l), Some(r)) => {
                if dist(l) <= dist(r) {
                    left -= 1;
                    l
                } else {
                    right += 1;
                    r
                }
            }
        };
        let q = max_partial_quotient(u32::from(next), d32, best.0);
        if q < best.0 {
            best = (q, next);
        }
    }
    best.1
}

/// The group (0..8) of dot `i` of the record keyed by `key` -- the shader's
/// rule bit for bit: `off = pcg(key ^ 0x9E3779B9) % D`,
/// `p = (stride * (i % D) + off) % D`, the first `k` with `p < bounds[k]`,
/// else 7. An empty record (`D = 0`) answers 7.
pub fn group_of(comp: &Composition, key: u32, i: u32) -> u8 {
    let d = u32::from(comp.denom).max(1);
    let off = pcg(key ^ OFFSET_SALT) % d;
    // stride < 2^16 and i % d < 2^16, so the product fits in u32.
    let p = (u32::from(comp.stride) * (i % d) + off) % d;
    comp.bounds.iter().position(|&b| p < u32::from(b)).map_or(7, |k| k as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::dotgrid::hash::{dot_alive, pcg};

    fn gcd(a: u32, b: u32) -> u32 {
        if b == 0 { a } else { gcd(b, a % b) }
    }

    #[test]
    fn stride_is_coprime_for_every_denominator() {
        for d in 1..=u16::MAX {
            let s = stride_for(d);
            assert!(s >= 1 && (d <= 2 || s < d), "d={d} s={s}");
            assert!(d <= 2 || 2 * u32::from(s) > u32::from(d), "d={d} s={s}: not in (d/2, d)");
            assert_eq!(gcd(u32::from(s), u32::from(d)), 1, "d={d} s={s}");
        }
    }

    /// C1 ruling's worked value.
    #[test]
    fn the_stride_for_31000_is_19531() {
        assert_eq!(stride_for(31_000), 19_531);
        assert_eq!(PRIMES.len(), 6542);
        assert_eq!((PRIMES[0], PRIMES[6541]), (2, 65_521));
    }

    /// Task 2's F2: `group_of` is spec §3.3's rule, spelled out here
    /// independently -- `off = pcg(key ^ 0x9E3779B9) % D`,
    /// `p = (stride * (i % D) + off) % D`, the first `k` with `p < c_k` --
    /// on fixed vectors, so a change to the salt or to where `off` enters
    /// fails here (and `render::dots`' `the_shader_group_rule_is_group_of`
    /// holds the WGSL to the same spelling).
    #[test]
    fn group_of_is_the_spec_rule_with_its_salt() {
        assert_eq!(OFFSET_SALT, 0x9E37_79B9, "spec §3.3's salt");
        let spec = |c: &[u32; 8], key: u32, i: u32| -> u8 {
            let d: u32 = c.iter().sum::<u32>().clamp(1, 65_535);
            let off = pcg(key ^ 0x9E37_79B9) % d;
            let p = (u32::from(stride_for(d as u16)) * (i % d) + off) % d;
            let mut acc = 0;
            for (k, &n) in c.iter().enumerate().take(7) {
                acc += n;
                if p < acc {
                    return k as u8;
                }
            }
            7
        };
        let cases: [[u32; 8]; 4] = [[3, 0, 5, 1, 0, 0, 2, 4], [1, 1, 1, 1, 1, 1, 1, 1], [0, 0, 0, 0, 0, 0, 0, 9], [900, 30, 12, 0, 7, 0, 51, 0]];
        for c in &cases {
            let comp = Composition::from_counts(c);
            for key in [0u32, 1, 0xDEAD_BEEF, 0x9E37_79B9] {
                for i in 0..2_000 {
                    assert_eq!(group_of(&comp, key, i), spec(c, key, i), "{c:?} key {key:#x} i {i}");
                }
            }
        }
        // Literal values, computed outside Rust (Python, the PCG and the
        // rule above; stride_for(15) = 11): a salt or offset change moves them.
        let comp = Composition::from_counts(&[3, 0, 5, 1, 0, 0, 2, 4]);
        assert_eq!(comp.stride, 11);
        assert_eq!(pcg(12_345 ^ OFFSET_SALT) % 15, 4, "off");
        let got: Vec<u8> = (0..15).map(|i| group_of(&comp, 12_345, i)).collect();
        assert_eq!(got, [2, 0, 7, 2, 2, 7, 6, 2, 0, 7, 6, 2, 0, 7, 3]);
    }

    #[test]
    fn tiny_parts_use_stride_one() {
        assert_eq!(stride_for(1), 1);
        assert_eq!(stride_for(2), 1);
        let c = Composition::from_counts(&[1, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(group_of(&c, 7, 0), 0);
    }

    /// At ppd 1 the alive dots are exactly 0..P, and each group gets exactly its count.
    #[test]
    fn ppd1_counts_are_exact() {
        let mut s = 1u32;
        for _ in 0..500 {
            let mut c = [0u32; 8];
            for g in &mut c {
                s = pcg(s);
                *g = s % 40;
            }
            let p: u32 = c.iter().sum();
            if p == 0 {
                continue;
            }
            let comp = Composition::from_counts(&c);
            let key = pcg(s);
            let mut got = [0u32; 8];
            for i in 0..p + 2 {
                if dot_alive(p as f32, 1.0, i, key) {
                    got[usize::from(group_of(&comp, key, i))] += 1;
                }
            }
            assert_eq!(got, c, "p={p}");
        }
    }

    /// Checks every prefix of `n <= 4096` dots is within `bound` of
    /// proportional in every group; returns the worst deviation seen.
    fn worst_prefix_deviation(c: &[u32; 8], key: u32, bound: f64) -> f64 {
        let p: u32 = c.iter().sum();
        let comp = Composition::from_counts(c);
        let mut got = [0u32; 8];
        let mut worst = 0f64;
        for i in 0..4096u32 {
            got[usize::from(group_of(&comp, key, i))] += 1;
            let n = f64::from(i + 1);
            for g in 0..8 {
                let want = n * f64::from(c[g]) / f64::from(p);
                let dev = (f64::from(got[g]) - want).abs();
                worst = worst.max(dev);
                assert!(dev <= bound, "c={c:?} key={key} n={n} g={g} got={} want={want:.1}", got[g]);
            }
        }
        worst
    }

    /// Any prefix of n <= 4096 dots is near proportional in every group:
    /// within 3 for the plan's composition (worst 2.55, C1 ruling), and
    /// within 4.5 over seeded random compositions -- totals from a handful
    /// of people to past the 65,535 denominator, with skewed group sizes
    /// like real blocks and cells. 3 cannot hold for every composition:
    /// arbitrary intervals under a golden-ratio Weyl sequence itself reach
    /// about 3.66 by n = 4096, and 3,000 random compositions here reach
    /// 3.97 (measured 2026-09-27, task-2-report.md).
    #[test]
    fn prefixes_are_representative() {
        let plan = [3000, 20000, 5000, 100, 1500, 50, 200, 1150];
        let w = worst_prefix_deviation(&plan, 99, 3.0);
        assert!(w <= 2.6, "the plan's composition: worst {w}");

        let mut s = 0x5EED_u32;
        let mut next = || {
            s = pcg(s);
            s
        };
        let mut worst = 0f64;
        for _ in 0..300 {
            let scale = [10u32, 100, 1_000, 10_000, 60_000, 400_000][(next() % 6) as usize];
            let mut c = [0u32; 8];
            for g in &mut c {
                // Skewed: a few dominant groups, some tiny or empty.
                let r = next();
                *g = match r % 4 {
                    0 => 0,
                    1 => next() % (scale / 10 + 1),
                    _ => next() % (scale + 1),
                };
            }
            if c.iter().sum::<u32>() == 0 {
                continue;
            }
            worst = worst.max(worst_prefix_deviation(&c, next(), 4.5));
        }
        assert!(worst > 2.0, "the property test exercised nothing: worst {worst}");
    }

    /// C3 ruling: across two ppd values, every dot alive at both keeps
    /// the same group.
    #[test]
    fn a_dots_group_does_not_depend_on_ppd() {
        let mut s = 7u32;
        let mut checked = 0u64;
        for _ in 0..200 {
            let mut c = [0u32; 8];
            for g in &mut c {
                s = pcg(s);
                *g = s % 300;
            }
            let p: u32 = c.iter().sum();
            let comp = Composition::from_counts(&c);
            let key = pcg(s);
            for &(ppd_a, ppd_b) in &[(1.0f32, 2.0f32), (1.7, 5.3), (3.0, 64.0)] {
                // The group as it is read while drawing at each ppd.
                let at = |ppd: f32| -> Vec<Option<u8>> {
                    (0..p + 2).map(|i| dot_alive(p as f32, ppd, i, key).then(|| group_of(&comp, key, i))).collect()
                };
                let (a, b) = (at(ppd_a), at(ppd_b));
                for (i, (ga, gb)) in a.iter().zip(&b).enumerate() {
                    if let (Some(ga), Some(gb)) = (ga, gb) {
                        checked += 1;
                        assert_eq!(ga, gb, "i={i} ppd {ppd_a} vs {ppd_b}");
                    }
                }
            }
        }
        assert!(checked > 10_000, "{checked}");
    }

    #[test]
    fn the_texel_round_trips_the_bounds_and_stride() {
        let comp = Composition::from_counts(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let t = comp.texel();
        assert_eq!(t[0] & 0xFFFF, 1);
        assert_eq!(t[0] >> 16, 3);
        assert_eq!(t[3] >> 16, u32::from(comp.stride));
        assert_eq!(comp.denom, 36);
        let back = [t[0] & 0xFFFF, t[0] >> 16, t[1] & 0xFFFF, t[1] >> 16, t[2] & 0xFFFF, t[2] >> 16, t[3] & 0xFFFF];
        assert_eq!(back, comp.bounds.map(u32::from));
    }

    #[test]
    fn a_record_over_65535_is_largest_remainder_rounded() {
        // 3 equal groups of 100,000: 65535 / 3 = 21845 exactly.
        let c = Composition::from_counts(&[100_000, 100_000, 100_000, 0, 0, 0, 0, 0]);
        assert_eq!(c.denom, 65_535);
        assert_eq!(&c.bounds[..3], &[21_845, 43_690, 65_535]);
        // 7 equal groups of 10,000 and an empty one: 65535 / 7 = 9362 r 1,
        // so group 0 (the lowest index on the tie) takes the one extra.
        let c = Composition::from_counts(&[10_000, 10_000, 10_000, 10_000, 10_000, 10_000, 10_000, 0]);
        assert_eq!(c.denom, 65_535);
        assert_eq!(c.bounds, [9_363, 18_725, 28_087, 37_449, 46_811, 56_173, 65_535]);
        // A larger remainder beats a lower index.
        let c = Composition::from_counts(&[65_536, 65_536 * 2 - 1, 0, 0, 0, 0, 0, 0]);
        assert_eq!(c.denom, 65_535);
        assert_eq!(&c.bounds[..2], &[21_845, 65_535]);
    }

    #[test]
    fn an_empty_record_answers_group_seven() {
        let c = Composition::from_counts(&[0; 8]);
        assert_eq!((c.denom, c.stride), (0, 1));
        assert_eq!(group_of(&c, 3, 0), 7);
    }
}

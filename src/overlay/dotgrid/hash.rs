//! CPU mirror of the vertex shader's per-dot hash and nesting rule (`vs_dot`,
//! Task 4). PCG-RXS-M-XS 32 (O'Neill 2014, the shuffle Jarzynski & Olano 2020
//! describe) is a single 32-bit permutation, used four ways below to turn a
//! cell's key and a dot index into a stable, nested subset of dots.

/// PCG-RXS-M-XS 32: `state = v*747796405 + 2891336453;
/// word = ((state >> ((state >> 28) + 4)) ^ state) * 277803737;
/// return (word >> 22) ^ word` (all arithmetic mod 2^32, i.e. wrapping).
pub fn pcg(v: u32) -> u32 {
    let state = v.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    let word = ((state >> ((state >> 28) + 4)) ^ state).wrapping_mul(277_803_737);
    (word >> 22) ^ word
}

/// A cell's hash key from its stable world-space origin (its SW corner, so
/// every dot request for the same cell hashes from the same two floats).
pub fn cell_key(origin: [f32; 2]) -> u32 {
    origin[0].to_bits() ^ pcg(origin[1].to_bits())
}

/// Top 24 bits of a PCG output as an exact `f32` in `[0, 1)` on every
/// backend: no denormals, no backend-dependent rounding of the low byte.
fn to_unit(x: u32) -> f32 {
    (x >> 8) as f32 / 16_777_216.0
}

/// Three hashes for dot `i` of the cell keyed by `key`: `(h_t, h_u, h_v)`.
/// `h_t` jitters dot `i`'s threshold within the population count (used by
/// [`dot_alive`]/[`dot_alpha`]); `h_u`/`h_v` place the dot within its cell
/// (the vertex shader's job, not this module's -- produced here so the CPU
/// and GPU hash chains are identical bit for bit).
pub fn dot_hashes(key: u32, i: u32) -> (f32, f32, f32) {
    let a = pcg(key ^ pcg(i));
    let b = pcg(a);
    let c = pcg(b);
    (to_unit(a), to_unit(b), to_unit(c))
}

/// Whether dot `i` of a cell with population `pop` is drawn at population-
/// per-dot budget `ppd`. Nested by construction: `h_t` does not depend on
/// `ppd`, and `pop / ppd` only grows as `ppd` shrinks, so a dot alive at some
/// `ppd` stays alive at any smaller `ppd` (checked by
/// `nesting_holds_on_a_sampled_grid`).
///
/// Integer-exact (C2 ruling): with `q = pop / ppd`, dot `i` lives iff
/// `i < floor(q)`, or `i == floor(q)` and `h_t < fract(q)`. The f32 sum
/// `i + h_t < q` meant the same in exact arithmetic, but it rounds up to
/// `i + 1` when `h_t` is within an ulp of 1, so at ppd 1 the last of `P`
/// people could lose its dot. `shader.wgsl`'s `dot_slot` is the same rule.
pub fn dot_alive(pop: f32, ppd: f32, i: u32, key: u32) -> bool {
    let q = (pop / ppd).max(0.0);
    let whole = q.floor();
    // Clamped as the shader clamps it (4294967040 is the largest f32
    // below 2^32), so the WGSL u32 conversion stays in range.
    let n = whole.min(4_294_967_040.0) as u32;
    if i < n {
        return true;
    }
    if i > n {
        return false;
    }
    let (h_t, _, _) = dot_hashes(key, i);
    h_t < q - whole
}

/// Antialiased coverage for dot `i`: 0 outside the population count, 1 fully
/// inside, ramped across a quarter-dot band around the same boundary
/// [`dot_alive`] tests -- so the last (fractional) dot fades in rather than
/// popping.
pub fn dot_alpha(pop: f32, ppd: f32, i: u32, key: u32) -> f32 {
    let (h_t, _, _) = dot_hashes(key, i);
    ((pop / ppd - i as f32 - h_t) * 4.0).clamp(0.0, 1.0)
}

/// The relocation candidate chain's per-index state: `p` is the previous
/// candidate's raw hash (unused for `j == 0`, which reads `b`/`c` straight
/// from [`dot_hashes`]'s chain so candidate 0 is bit-identical to today's
/// placement).
struct Candidates {
    b: u32,
    c: u32,
    p: u32,
    j: u32,
    k: u32,
}

impl Iterator for Candidates {
    type Item = (f32, f32);

    fn next(&mut self) -> Option<Self::Item> {
        if self.j >= self.k {
            return None;
        }
        let out = if self.j == 0 {
            (to_unit(self.b), to_unit(self.c))
        } else {
            (to_unit(self.p), to_unit(pcg(self.p)))
        };
        self.p = if self.j == 0 { pcg(self.c) } else { pcg(pcg(self.p)) };
        self.j += 1;
        Some(out)
    }
}

/// Candidate `j` of dot `i`, as `(h_u, h_v)` in [0, 1)^2, j = 0..k.
/// j = 0 is `dot_hashes(key, i).1, .2` bit for bit. With `c` = the third
/// hash of `dot_hashes`: p_1 = pcg(c); candidate j = (unit(p_j), unit(pcg(p_j)));
/// p_{j+1} = pcg(pcg(p_j)). `k == 0` yields candidate 0 alone.
pub fn dot_candidates(key: u32, i: u32, k: u32) -> impl Iterator<Item = (f32, f32)> {
    let a = pcg(key ^ pcg(i));
    let b = pcg(a);
    let c = pcg(b);
    Candidates { b, c, p: 0, j: 0, k: k.max(1) }
}

/// The rank value that ends the search early: [`choose`] never needs to
/// look past the cell's first building candidate, since nothing outranks
/// it. Defined from [`super::mask_class::MaskRank::Building`] rather than
/// repeating its `4` so the two can't drift (Task 2/3 pre-flight note,
/// `progress.md`).
pub const RANK_BUILDING: u8 = super::mask_class::MaskRank::Building as u8;

/// The rank of a mask texel no basemap tile covers (the mask's clear
/// value, and anything outside the mask): below water.
/// `render::dot_mask::MASK_NO_DATA_RANK`; `shader.wgsl`'s
/// `DOT_RANK_NO_DATA` (`the_shader_no_data_rank_matches_the_mask_clear_rank`).
pub const RANK_NO_DATA: u8 = 0;

/// The index of the lowest candidate with the highest rank; stops reading
/// at the first rank 4 (building). A candidate read before the search ends
/// that has no data ([`RANK_NO_DATA`]) ends it at candidate 0: the dot does
/// not guess where the basemap is missing, so a tile arriving never moves a
/// dot that read only data (DP-5 ruling). Empty -> 0.
pub fn choose(ranks: impl IntoIterator<Item = u8>) -> usize {
    let mut best = 0u8;
    let mut at = 0usize;
    for (idx, rank) in ranks.into_iter().enumerate() {
        if rank == RANK_NO_DATA {
            return 0;
        }
        if rank > best {
            best = rank;
            at = idx;
        }
        if best == RANK_BUILDING {
            break;
        }
    }
    at
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::dotgrid::mask_class::MaskRank;

    #[test]
    fn pcg_matches_the_reference_vectors() {
        // Values computed once from the reference formula. The WGSL port is
        // checked end to end rather than against these vectors:
        // `render::gpu`'s `wgsl_pcg_matches_the_cpu_pcg` places dots on the
        // GPU and compares them with `dot_hashes`.
        assert_eq!(pcg(0), 129_708_002);
        assert_eq!(pcg(1), 2_831_084_092);
        assert_eq!(pcg(0xdead_beef), 1_730_779_506);
    }

    #[test]
    fn nesting_holds_on_a_sampled_grid() {
        let mut violations = 0u64;
        for cell in 0..20_000u32 {
            let key = cell_key([cell as f32 * 0.37, cell as f32 * 1.91]);
            let pop = (cell % 500) as f32 * 0.73;
            for ppd in [1.0f32, 1.7, 2.0, 16.0, 333.0, 2048.0] {
                for i in 0..(pop / ppd).ceil() as u32 + 2 {
                    if dot_alive(pop, ppd, i, key) && !dot_alive(pop, ppd / 2.0, i, key) {
                        violations += 1;
                    }
                }
            }
        }
        assert_eq!(violations, 0);
    }

    #[test]
    fn expected_dots_equal_population_at_ppd_one() {
        let mut dots = 0u64;
        let mut pop_sum = 0f64;
        for cell in 0..200_000u32 {
            let key = cell_key([cell as f32, (cell * 3) as f32]);
            let pop = ((cell.wrapping_mul(2_654_435_761) >> 16) % 400) as f32 / 10.0;
            pop_sum += pop as f64;
            dots += (0..pop.ceil() as u32 + 1).filter(|&i| dot_alive(pop, 1.0, i, key)).count() as u64;
        }
        assert!(((dots as f64 - pop_sum) / pop_sum).abs() < 0.001, "{dots} vs {pop_sum}");
    }

    /// C2 ruling: at ppd 1 an integer population `P` keeps every index
    /// below `P`, including `P - 1` whose `h_t` is within 2^-16 of 1 --
    /// the f32 sum `(P - 1) + h_t` rounds up to `P` there, and the dot
    /// died. Searches keys for such an `h_t` at several populations.
    #[test]
    fn the_last_dot_lives_when_its_threshold_is_nearly_one() {
        for pop in [2u32, 300, 1_000, 31_000, 65_535, 1_000_000] {
            let last = pop - 1;
            let mut found = 0;
            let mut key = 0u32;
            while found < 3 {
                let (h_t, _, _) = dot_hashes(key, last);
                if h_t > 1.0 - 1.0 / 65_536.0 {
                    found += 1;
                    assert!(dot_alive(pop as f32, 1.0, last, key), "pop={pop} key={key} h_t={h_t}");
                    assert!(!dot_alive(pop as f32, 1.0, pop, key), "pop={pop} key={key}: dot P is dead");
                }
                key += 1;
            }
            assert_eq!(slot_count_at_ppd1(pop, key), u64::from(pop));
        }
    }

    fn slot_count_at_ppd1(pop: u32, key: u32) -> u64 {
        (0..pop + 2).filter(|&i| dot_alive(pop as f32, 1.0, i, key)).count() as u64
    }

    /// The alive test's two halves: every index below `floor(q)` lives, the
    /// one at it lives iff `h_t < fract(q)`, nothing above it lives, and a
    /// non-positive quota draws nothing.
    #[test]
    fn alive_is_the_integer_exact_rule() {
        for key in 0..2_000u32 {
            for &(pop, ppd) in &[(7.25f32, 1.0f32), (1_000.5, 1.0), (40_001.0, 3.0), (0.4, 1.0)] {
                let q = pop / ppd;
                let whole = q.floor() as u32;
                for i in 0..whole + 3 {
                    let (h_t, _, _) = dot_hashes(key, i);
                    let want = i < whole || (i == whole && h_t < q - q.floor());
                    assert_eq!(dot_alive(pop, ppd, i, key), want, "key={key} pop={pop} ppd={ppd} i={i}");
                }
            }
            assert!(!dot_alive(0.0, 1.0, 0, key));
            assert!(!dot_alive(-3.5, 1.0, 0, key));
        }
    }

    #[test]
    fn candidate_zero_is_todays_position_bit_for_bit() {
        for cell in 0..5_000u32 {
            let key = cell_key([cell as f32 * 0.37, cell as f32 * 1.91]);
            for i in 0..8 {
                let (_, u, v) = dot_hashes(key, i);
                let c0 = dot_candidates(key, i, 8).next().unwrap();
                assert_eq!((c0.0.to_bits(), c0.1.to_bits()), (u.to_bits(), v.to_bits()));
            }
        }
    }

    #[test]
    fn k_zero_and_k_one_yield_only_candidate_zero() {
        assert_eq!(dot_candidates(7, 3, 0).count(), 1);
        assert_eq!(dot_candidates(7, 3, 1).count(), 1);
        assert_eq!(dot_candidates(7, 3, 16).count(), 16);
    }

    #[test]
    fn the_chain_is_the_spec_recurrence() {
        let a = pcg(99 ^ pcg(5));
        let b = pcg(a);
        let c = pcg(b);
        let p1 = pcg(c);
        let p2 = pcg(pcg(p1));
        let v: Vec<_> = dot_candidates(99, 5, 3).collect();
        assert_eq!(v[1], (to_unit(p1), to_unit(pcg(p1))));
        assert_eq!(v[2], (to_unit(p2), to_unit(pcg(p2))));
    }

    #[test]
    fn choose_takes_the_lowest_best_and_falls_back_to_zero() {
        assert_eq!(choose([3, 3, 3]), 0); // nothing better: stay
        assert_eq!(choose([1, 2, 2, 1]), 1); // lowest index of the best
        assert_eq!(choose([3, 4, 4]), 1); // stops at the first building
        assert_eq!(choose([4, 1]), 0);
        assert_eq!(choose(std::iter::empty()), 0);
    }

    #[test]
    fn choose_reads_nothing_after_a_building() {
        let mut read = 0;
        let _ = choose([2u8, 4, 1, 1].into_iter().inspect(|_| read += 1));
        assert_eq!(read, 2);
    }

    /// DP-6's nesting half: a dot alive at `ppd` is alive at `ppd / 2`.
    /// Nothing in the candidate chain or the pick reads `ppd`; the
    /// position half (same candidate at `ppd / 2`, through the full CPU
    /// mirror against a fixed mask) is
    /// `placement::tests::the_dots_alive_at_ppd_keep_their_place_at_half_the_ppd`.
    #[test]
    fn relocated_positions_do_not_depend_on_ppd() {
        let mut checked = 0u64;
        for cell in 0..5_000u32 {
            let key = cell_key([cell as f32 * 0.37, cell as f32 * 1.91]);
            let pop = (cell % 300) as f32 * 0.91;
            for ppd in [1.0f32, 1.7, 4.0, 33.0] {
                for i in 0..(pop / ppd).ceil() as u32 + 1 {
                    if dot_alive(pop, ppd, i, key) {
                        checked += 1;
                        assert!(dot_alive(pop, ppd / 2.0, i, key), "nesting");
                    }
                }
            }
        }
        assert!(checked > 100_000, "{checked}");
    }

    /// DP-5 ruling: a no-data candidate read before the search ends puts
    /// the dot at candidate 0, whatever ranks precede it; one after the
    /// first building is never read.
    #[test]
    fn a_no_data_candidate_keeps_candidate_zero() {
        let (land, water, building) = (3u8, 1u8, RANK_BUILDING);
        assert_eq!(choose([water, land, RANK_NO_DATA, building]), 0);
        assert_eq!(choose([RANK_NO_DATA, building]), 0);
        assert_eq!(choose([water, building, RANK_NO_DATA]), 1);
        assert_eq!(choose([water, land, land]), 1);
    }

    #[test]
    fn a_hit_is_uniform_over_the_building_area_and_matches_the_analytic_rate() {
        // Building = h_u < f (a strip). Over 200k dots at K = 8, f = 0.2: the
        // hit share is within 0.5 pp of 1 - (1 - f)^8, and the hit u / f
        // passes a 10-bin chi-square (p > 0.001).
        const N: u32 = 200_000;
        const K: u32 = 8;
        const F: f32 = 0.2;

        let mut hits = 0u32;
        let mut bins = [0u32; 10];
        for cell in 0..N {
            let key = cell_key([cell as f32 * 0.53, cell as f32 * 2.17]);
            let candidates: Vec<(f32, f32)> = dot_candidates(key, 0, K).collect();
            let ranks: Vec<u8> =
                candidates.iter().map(|&(u, _)| if u < F { RANK_BUILDING } else { MaskRank::Land as u8 }).collect();
            let idx = choose(ranks.iter().copied());
            if ranks[idx] == RANK_BUILDING {
                hits += 1;
                let x = (candidates[idx].0 / F).clamp(0.0, 0.999_999);
                bins[(x * 10.0) as usize] += 1;
            }
        }

        let hit_share = hits as f64 / N as f64;
        let expected = 1.0 - (1.0 - F as f64).powi(K as i32);
        assert!((hit_share - expected).abs() < 0.005, "hit_share={hit_share} expected={expected}");

        let expected_per_bin = hits as f64 / 10.0;
        let chi2: f64 = bins
            .iter()
            .map(|&count| {
                let d = count as f64 - expected_per_bin;
                d * d / expected_per_bin
            })
            .sum();
        // Critical value for 9 degrees of freedom at p = 0.001.
        assert!(chi2 < 27.877, "chi2={chi2}");
    }
}

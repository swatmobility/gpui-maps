//! Trip ends into **places**: data-driven nodes for a flow map, not a
//! lattice.
//!
//! Edge-Path Bundling routes corridors through a graph whose nodes are the
//! places the flows run between. The reference implementation
//! (mobitwin-edgebundling) takes them as given -- region centroids -- and
//! a node set laid on a regular lattice ([`super::od::OdGrid::build`])
//! shows through the bundle: regularly spaced nodes, strands that form a
//! mesh, and rural strands cut to one lattice step.
//!
//! [`cluster`] makes the places from the data instead:
//!
//! 1. Every row end gets a KEY: the caller's place id when the table has
//!    one (an area code), else its exact coordinate. Ends with one key are
//!    one candidate place, at their weight-weighted centroid, carrying
//!    their summed weight. (An OD table between area centroids -- the ONS
//!    commute, say -- has exactly one coordinate per area, so the
//!    coordinate IS the area's id.)
//! 2. Ends keyed by coordinate are always grouped (a table of jittered or
//!    one-off points has a distinct coordinate per row, and those are not
//!    places); ids are grouped only when there are more than `max_places`
//!    of them (a few hundred given areas are kept as the reference keeps
//!    its region centroids). Grouping is
//!    weight-ordered greedy clustering. The candidate in the densest
//!    neighbourhood (summed weight of its radius-sized bucket), heaviest
//!    first within it, seeds a place; every later one joins the nearest seed within `radius_m`, or
//!    seeds its own. The places are where the weight is, at irregular,
//!    data-given spacing; nothing is snapped.
//! 3. A place sits at the weight-weighted centroid of its members' ends.
//!
//! Deterministic: candidates are ordered by weight, ties by key, so the
//! input's row order changes nothing. Linear time: one hash pass over the
//! rows, a sort of the candidates, and a bucket grid of `radius_m` cells
//! for the nearest-seed query.

use std::collections::HashMap;

/// Mercator's latitude limit, degrees.
const MAX_LAT: f64 = 85.051_128_779_806_59;
/// Metres per degree of latitude (and of longitude at the equator).
const M_PER_DEG: f64 = 111_320.0;

/// How [`cluster`] groups the candidates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlaceOptions {
    /// A candidate joins a seed this close, metres.
    pub radius_m: f64,
    /// With ids and no more candidates than this, every id is its own
    /// place and nothing is grouped.
    pub max_places: usize,
}

/// The places, and which one each row's ends belong to.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Places {
    /// Each place's weight-weighted centroid, `[lon, lat]`.
    pub centroids: Vec<[f64; 2]>,
    /// Each place's summed end weight (a row counts at both its ends).
    pub weights: Vec<f64>,
    /// Row `i`'s origin place and destination place; [`Places::NONE`] for
    /// a skipped row (a non-finite end or weight).
    pub origin: Vec<u32>,
    pub destination: Vec<u32>,
    /// Candidates before grouping (distinct keys).
    pub candidates: usize,
    /// The grouping radius used, metres (0 when nothing was grouped).
    pub radius_m: f64,
}

impl Places {
    pub const NONE: u32 = u32::MAX;
}

fn ok(p: [f64; 2]) -> bool {
    p[0].is_finite() && p[1].is_finite() && p[1].abs() <= MAX_LAT
}

/// A coordinate's key: its two numbers' bits.
fn coord_key(p: [f64; 2]) -> u64 {
    (p[0].to_bits().rotate_left(29) ^ p[1].to_bits()).wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

/// Groups `origins[i] -> destinations[i]` ends (each `[lon, lat]`, weight
/// `weights[i]`, every row 1 when `None`) into places. `keys`, when given,
/// is each row's `(origin id, destination id)`, hashed by the caller: ends
/// with one id are one candidate whatever their coordinates.
#[must_use]
pub fn cluster(origins: &[[f64; 2]], destinations: &[[f64; 2]], weights: Option<&[f64]>, keys: Option<(&[u64], &[u64])>, options: PlaceOptions) -> Places {
    let rows = origins.len().min(destinations.len());
    // 1. Candidates by key.
    struct Cand {
        key: u64,
        sum: [f64; 2],
        w: f64,
        /// The summed weight (the centroid's weight has a floor).
        weight: f64,
    }
    let mut at: HashMap<u64, u32> = HashMap::new();
    let mut cands: Vec<Cand> = Vec::new();
    let mut of_origin = vec![Places::NONE; rows];
    let mut of_destination = vec![Places::NONE; rows];
    for i in 0..rows {
        let (a, b) = (origins[i], destinations[i]);
        let w = weights.map_or(1.0, |w| w.get(i).copied().unwrap_or(f64::NAN));
        if !ok(a) || !ok(b) || !(w >= 0.0) || !w.is_finite() {
            continue;
        }
        let (ka, kb) = match keys {
            Some((ko, kd)) => (ko.get(i).copied().unwrap_or_else(|| coord_key(a)), kd.get(i).copied().unwrap_or_else(|| coord_key(b))),
            None => (coord_key(a), coord_key(b)),
        };
        let cw = w.max(1e-9);
        for (k, p, slot) in [(ka, a, &mut of_origin[i]), (kb, b, &mut of_destination[i])] {
            let c = *at.entry(k).or_insert_with(|| {
                cands.push(Cand { key: k, sum: [0.0; 2], w: 0.0, weight: 0.0 });
                (cands.len() - 1) as u32
            });
            let e = &mut cands[c as usize];
            e.sum[0] += p[0] * cw;
            e.sum[1] += p[1] * cw;
            e.w += cw;
            e.weight += w;
            *slot = c;
        }
    }
    let pos: Vec<[f64; 2]> = cands.iter().map(|c| [c.sum[0] / c.w, c.sum[1] / c.w]).collect();
    let n = cands.len();
    // 2. Group them, heaviest first.
    let mut place_of: Vec<u32> = (0..n as u32).collect();
    let mut radius_used = 0.0;
    if (keys.is_none() || n > options.max_places.max(1)) && options.radius_m > 0.0 {
        radius_used = options.radius_m;
        // Local metres (equirectangular at the candidates' middle latitude).
        let (lo, hi) = pos.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), p| (lo.min(p[1]), hi.max(p[1])));
        let cos = ((lo + hi) / 2.0).to_radians().cos().abs().max(0.01);
        let xy = |p: [f64; 2]| [p[0] * M_PER_DEG * cos, p[1] * M_PER_DEG];
        let r = options.radius_m;
        let bucket = |q: [f64; 2]| ((q[0] / r).floor() as i64, (q[1] / r).floor() as i64);
        // Seeds go where the weight is DENSE first: the summed weight of
        // the candidate's radius-sized neighbourhood (its bucket), then its
        // own, then its key. On a table of one-off ends (taxi rides, every
        // end weight 1) the own weight alone would seed in key order.
        let mut around: HashMap<(i64, i64), f64> = HashMap::new();
        for (c, p) in cands.iter().zip(&pos) {
            *around.entry(bucket(xy(*p))).or_insert(0.0) += c.weight;
        }
        let dense: Vec<f64> = pos.iter().map(|p| around[&bucket(xy(*p))]).collect();
        let mut order: Vec<u32> = (0..n as u32).collect();
        order.sort_unstable_by(|a, b| {
            let (da, db) = (dense[*a as usize], dense[*b as usize]);
            let (a, b) = (&cands[*a as usize], &cands[*b as usize]);
            db.total_cmp(&da).then(b.weight.total_cmp(&a.weight)).then(a.key.cmp(&b.key))
        });
        let mut seeds: HashMap<(i64, i64), Vec<u32>> = HashMap::new();
        let mut seed_xy: Vec<[f64; 2]> = Vec::new();
        for c in order {
            let q = xy(pos[c as usize]);
            let (bx, by) = bucket(q);
            let mut best: Option<(f64, u32)> = None;
            for dx in -1..=1 {
                for dy in -1..=1 {
                    for s in seeds.get(&(bx + dx, by + dy)).into_iter().flatten() {
                        let p = seed_xy[*s as usize];
                        let d = (p[0] - q[0]).hypot(p[1] - q[1]);
                        if d <= r && best.is_none_or(|(bd, bs)| d < bd || (d == bd && *s < bs)) {
                            best = Some((d, *s));
                        }
                    }
                }
            }
            place_of[c as usize] = match best {
                Some((_, s)) => s,
                None => {
                    let s = seed_xy.len() as u32;
                    seed_xy.push(q);
                    seeds.entry((bx, by)).or_default().push(s);
                    s
                }
            };
        }
    } else {
        // Every candidate its own place, numbered heaviest first.
        let mut order: Vec<u32> = (0..n as u32).collect();
        order.sort_unstable_by(|a, b| {
            let (a, b) = (&cands[*a as usize], &cands[*b as usize]);
            b.weight.total_cmp(&a.weight).then(a.key.cmp(&b.key))
        });
        for (k, c) in order.iter().enumerate() {
            place_of[*c as usize] = k as u32;
        }
    }
    // 3. Each place at its members' weighted centroid.
    let places = place_of.iter().copied().max().map_or(0, |m| m as usize + 1);
    let mut sum = vec![[0.0f64; 3]; places];
    let mut weight = vec![0.0f64; places];
    for (c, p) in place_of.iter().enumerate() {
        let e = &cands[c];
        let s = &mut sum[*p as usize];
        s[0] += e.sum[0];
        s[1] += e.sum[1];
        s[2] += e.w;
        weight[*p as usize] += e.weight;
    }
    let centroids = sum.iter().map(|s| [s[0] / s[2], s[1] / s[2]]).collect();
    let map = |v: Vec<u32>| v.into_iter().map(|c| if c == Places::NONE { c } else { place_of[c as usize] }).collect();
    Places { centroids, weights: weight, origin: map(of_origin), destination: map(of_destination), candidates: n, radius_m: radius_used }
}

/// A grouping radius for `points`: their robust extent's longer side (2nd
/// to 98th percentile per axis) over `places_across`, metres, rounded to 1,
/// 2 or 5 times a power of ten.
#[must_use]
pub fn auto_place_radius_m(points: &[[f64; 2]], places_across: f64) -> f64 {
    // The lattice's own rule, without its hexagon's sqrt(3).
    super::od::auto_radius_m(points, places_across / 3f64.sqrt())
}

/// A string place id hashed for [`cluster`]'s `keys` (FNV-1a).
#[must_use]
pub fn id_key(id: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in id.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod probe {
    use super::*;
    use crate::flow::edge_path_bundling::{bundle_paths_loaded, PathBundlingParams, PathEdge};
    use crate::flow::od::OdGrid;

    /// Times [`cluster`] and the bundle on a real OD CSV, over a few
    /// `places_across`: `GPUI_MAP_PLACES_PROBE_CSV=path`,
    /// `GPUI_MAP_PLACES_PROBE_COLS=lon0,lat0,lon1,lat1[,weight]`,
    /// `GPUI_MAP_PLACES_PROBE_ACROSS=40,60,90`, `..._TOP=10000`. A
    /// measurement: `--release --ignored --nocapture`.
    #[test]
    #[ignore = "timing probe on a real CSV; see the doc comment"]
    fn probe_places_csv() {
        let Ok(path) = std::env::var("GPUI_MAP_PLACES_PROBE_CSV") else { return };
        let cols = std::env::var("GPUI_MAP_PLACES_PROBE_COLS").unwrap_or_default();
        let cols: Vec<&str> = cols.split(',').collect();
        let text = std::fs::read_to_string(&path).expect("the csv");
        let mut lines = text.lines();
        let header: Vec<&str> = lines.next().expect("a header").split(',').collect();
        let ix: Vec<usize> = cols.iter().map(|c| header.iter().position(|h| h == c).unwrap_or_else(|| panic!("no column {c}"))).collect();
        let (mut o, mut d, mut w) = (Vec::new(), Vec::new(), Vec::new());
        for l in lines {
            let f: Vec<f64> = l.split(',').map(|v| v.trim().parse().unwrap_or(f64::NAN)).collect();
            o.push([f[ix[0]], f[ix[1]]]);
            d.push([f[ix[2]], f[ix[3]]]);
            w.push(ix.get(4).map_or(1.0, |i| f[*i]));
        }
        let ends: Vec<[f64; 2]> = o.iter().chain(&d).copied().collect();
        let top: usize = std::env::var("GPUI_MAP_PLACES_PROBE_TOP").ok().and_then(|v| v.parse().ok()).unwrap_or(10_000);
        let across: Vec<f64> = std::env::var("GPUI_MAP_PLACES_PROBE_ACROSS").ok().map_or_else(|| vec![40.0, 60.0, 90.0], |v| v.split(',').filter_map(|n| n.parse().ok()).collect());
        for a in across {
            let r = auto_place_radius_m(&ends, a);
            let t = std::time::Instant::now();
            let p = cluster(&o, &d, Some(&w), None, PlaceOptions { radius_m: r, max_places: 0 });
            let cluster_ms = t.elapsed().as_secs_f64() * 1e3;
            let t = std::time::Instant::now();
            let g = OdGrid::from_places(&o, &d, Some(&w), &p, r);
            let grid_ms = t.elapsed().as_secs_f64() * 1e3;
            let pairs: Vec<_> = g.pairs.iter().take(top).collect();
            let edges: Vec<PathEdge> = pairs.iter().map(|q| PathEdge { from: g.cells[q.from as usize].centroid, to: g.cells[q.to as usize].centroid }).collect();
            let weights: Vec<f64> = pairs.iter().map(|q| q.weight).collect();
            let t = std::time::Instant::now();
            let out = bundle_paths_loaded(&edges, &weights, PathBundlingParams { samples: 32, unrouted_bow: 0.0, ..Default::default() });
            let routed = out.iter().filter(|p| p.routed).count();
            println!(
                "across {a}: radius {r} m; {} rows, {} candidates -> {} places in {cluster_ms:.0} ms, grid {grid_ms:.0} ms, {} pairs; top {} ({:.1}% of weight) bundled in {:.0} ms, {routed} routed",
                o.len(),
                p.candidates,
                p.centroids.len(),
                g.pairs.len(),
                pairs.len(),
                g.top_share(top) * 100.0,
                t.elapsed().as_secs_f64() * 1e3
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pseudo-random scatter (an LCG: no lattice, no dependency).
    fn scatter(n: usize, seed: u64) -> Vec<([f64; 2], f64)> {
        let mut s = seed;
        let mut next = || {
            s = s.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            (s >> 11) as f64 / (1u64 << 53) as f64
        };
        (0..n).map(|_| ([-5.0 + 6.0 * next(), 50.0 + 5.0 * next()], (1.0 + 99.0 * next()).round())).collect()
    }

    fn rows(pts: &[([f64; 2], f64)]) -> (Vec<[f64; 2]>, Vec<[f64; 2]>, Vec<f64>) {
        let o: Vec<[f64; 2]> = pts.iter().map(|p| p.0).collect();
        let d: Vec<[f64; 2]> = pts.iter().rev().map(|p| p.0).collect();
        let w: Vec<f64> = pts.iter().map(|p| p.1).collect();
        (o, d, w)
    }

    #[test]
    fn few_ids_are_each_their_own_place_at_their_centroid() {
        let a = [-0.1, 51.5];
        let b = [-2.2, 53.4];
        let (ka, kb) = (id_key("A"), id_key("B"));
        let p = cluster(&[a, a, b], &[b, b, a], Some(&[3.0, 1.0, 2.0]), Some((&[ka, ka, kb], &[kb, kb, ka])), PlaceOptions { radius_m: 50_000.0, max_places: 10 });
        assert_eq!(p.centroids.len(), 2);
        assert_eq!(p.radius_m, 0.0, "nothing grouped");
        assert_eq!(p.origin[0], p.origin[1]);
        assert_ne!(p.origin[0], p.destination[0]);
        let c = p.centroids[p.origin[0] as usize];
        assert!((c[0] - a[0]).abs() < 1e-12 && (c[1] - a[1]).abs() < 1e-12, "{c:?}");
        // Heaviest first: b's ends carry 3 + 1 + 2, a's the same; ties by key.
        assert_eq!(p.weights.iter().sum::<f64>(), 12.0);
    }

    /// Jittered one-off ends (one distinct coordinate per row, far fewer
    /// than `max_places`) are still grouped into the towns they scatter
    /// round: a coordinate is not a place.
    #[test]
    fn jittered_ends_group_into_their_towns() {
        let town = |c: [f64; 2], k: usize| [c[0] + 0.002 * (k % 5) as f64, c[1] + 0.002 * (k / 5) as f64];
        let (t1, t2) = ([103.89, 1.39], [103.85, 1.28]);
        let o: Vec<[f64; 2]> = (0..20).map(|k| town(t1, k)).collect();
        let d: Vec<[f64; 2]> = (0..20).map(|k| town(t2, k)).collect();
        let p = cluster(&o, &d, None, None, PlaceOptions { radius_m: 2_000.0, max_places: 1_500 });
        assert_eq!(p.candidates, 40);
        assert_eq!(p.centroids.len(), 2, "two towns");
        assert!(p.origin.iter().all(|c| *c == p.origin[0]) && p.destination.iter().all(|c| *c == p.destination[0]));
    }

    #[test]
    fn ids_merge_ends_whatever_their_coordinates() {
        let a = [-0.1, 51.5];
        let a2 = [-0.2, 51.6];
        let b = [-2.2, 53.4];
        let (ka, kb) = (id_key("E02000001"), id_key("E02000002"));
        let p = cluster(&[a, a2], &[b, b], Some(&[1.0, 3.0]), Some((&[ka, ka], &[kb, kb])), PlaceOptions { radius_m: 1.0, max_places: 10 });
        assert_eq!(p.centroids.len(), 2);
        assert_eq!(p.origin[0], p.origin[1]);
        let c = p.centroids[p.origin[0] as usize];
        assert!((c[0] - -0.175).abs() < 1e-9 && (c[1] - 51.575).abs() < 1e-9, "the weighted centroid: {c:?}");
    }

    /// Weights are preserved: every place's weight is its members' sum, and
    /// all of them the total of both ends of every row.
    #[test]
    fn grouping_preserves_the_weight() {
        let pts = scatter(5_000, 7);
        let (o, d, w) = rows(&pts);
        let p = cluster(&o, &d, Some(&w), None, PlaceOptions { radius_m: 20_000.0, max_places: 100 });
        assert!(p.centroids.len() < p.candidates, "{} of {}", p.centroids.len(), p.candidates);
        let total: f64 = 2.0 * w.iter().sum::<f64>();
        assert!((p.weights.iter().sum::<f64>() - total).abs() < 1e-6 * total);
        // Each row's ends map into the places.
        assert!(p.origin.iter().chain(&p.destination).all(|c| (*c as usize) < p.centroids.len()));
    }

    /// The same rows in another order make the same places.
    #[test]
    fn grouping_is_deterministic_and_order_free() {
        let pts = scatter(3_000, 11);
        let (o, d, w) = rows(&pts);
        let opt = PlaceOptions { radius_m: 15_000.0, max_places: 50 };
        let a = cluster(&o, &d, Some(&w), None, opt);
        assert_eq!(a, cluster(&o, &d, Some(&w), None, opt), "deterministic");
        let mut perm: Vec<usize> = (0..o.len()).collect();
        perm.reverse();
        perm.swap(3, 1_700);
        let (o2, d2, w2): (Vec<_>, Vec<_>, Vec<_>) = (perm.iter().map(|i| o[*i]).collect(), perm.iter().map(|i| d[*i]).collect(), perm.iter().map(|i| w[*i]).collect());
        let b = cluster(&o2, &d2, Some(&w2), None, opt);
        // The same places; only the float sums' order differs.
        assert_eq!(a.centroids.len(), b.centroids.len(), "the row order changes nothing");
        for (x, y) in a.centroids.iter().zip(&b.centroids) {
            assert!((x[0] - y[0]).abs() < 1e-9 && (x[1] - y[1]).abs() < 1e-9, "{x:?} vs {y:?}");
        }
        for (k, i) in perm.iter().enumerate() {
            assert_eq!(a.origin[*i], b.origin[k]);
        }
    }

    /// No lattice: the places are not regularly spaced. On a lattice every
    /// nearest-neighbour distance is one step (coefficient of variation
    /// 0); here they spread. Uniform scatter is greedy clustering's most
    /// regular case (the seeds are then a Poisson-disk sample, cv about
    /// 0.13); real data, clumped by where people are, spreads far more.
    #[test]
    fn places_are_irregular_not_a_lattice() {
        let pts = scatter(8_000, 3);
        let (o, d, w) = rows(&pts);
        let p = cluster(&o, &d, Some(&w), None, PlaceOptions { radius_m: 25_000.0, max_places: 100 });
        let cos = 52.5f64.to_radians().cos();
        let m = |a: [f64; 2], b: [f64; 2]| ((a[0] - b[0]) * M_PER_DEG * cos).hypot((a[1] - b[1]) * M_PER_DEG);
        let nn: Vec<f64> = p.centroids.iter().enumerate().map(|(i, a)| p.centroids.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, b)| m(*a, *b)).fold(f64::INFINITY, f64::min)).collect();
        let mean = nn.iter().sum::<f64>() / nn.len() as f64;
        let sd = (nn.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / nn.len() as f64).sqrt();
        assert!(sd / mean > 0.08, "nearest-neighbour spacing varies: cv {:.3}", sd / mean);
        // And the heaviest candidate's place sits near it, not at a cell centre.
        let heaviest = pts.iter().fold(pts[0], |a, b| if b.1 > a.1 { *b } else { a });
        assert!(p.centroids.iter().any(|c| m(*c, heaviest.0) < 25_000.0));
    }
}

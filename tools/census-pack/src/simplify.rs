//! Douglas-Peucker in local metres, and a ring's 4-point floor. `topo`
//! runs it once per shared arc, so neighbouring block groups keep the same
//! simplified edge.

/// Douglas-Peucker in local metres (equirectangular at the ring's mean
/// latitude), endpoints and `pinned` vertices always kept.
pub fn dp_keep(pts: &[[f64; 2]], tol_m: f64, pinned: &[bool]) -> Vec<bool> {
    let n = pts.len();
    let mut keep: Vec<bool> = pinned.to_vec();
    if n == 0 {
        return keep;
    }
    keep[0] = true;
    keep[n - 1] = true;
    let lat = pts.iter().map(|p| p[1]).sum::<f64>() / n as f64;
    let kx = 111_320.0 * lat.to_radians().cos();
    let ky = 110_540.0;
    let xy: Vec<(f64, f64)> = pts.iter().map(|p| (p[0] * kx, p[1] * ky)).collect();
    // Split at every kept vertex first, so pins are honoured.
    let anchors: Vec<usize> = (0..n).filter(|&i| keep[i]).collect();
    let mut stack: Vec<(usize, usize)> = anchors.windows(2).map(|w| (w[0], w[1])).collect();
    while let Some((a, b)) = stack.pop() {
        if b <= a + 1 {
            continue;
        }
        let (ax, ay) = xy[a];
        let (dx, dy) = (xy[b].0 - ax, xy[b].1 - ay);
        let len = dx.hypot(dy);
        let (mut far, mut at) = (0.0, a);
        for (i, &(x, y)) in xy.iter().enumerate().take(b).skip(a + 1) {
            let d = if len == 0.0 { (x - ax).hypot(y - ay) } else { (dx * (y - ay) - dy * (x - ax)).abs() / len };
            if d > far {
                (far, at) = (d, i);
            }
        }
        if far > tol_m {
            keep[at] = true;
            stack.push((a, at));
            stack.push((at, b));
        }
    }
    keep
}

/// One closed ring, simplified; never fewer than 4 points (3 + closure):
/// a collapse keeps the 4 points farthest from the first instead.
pub fn simplify_ring(ring: &[[f64; 2]], tol_m: f64, pinned: &[bool]) -> Vec<[f64; 2]> {
    let keep = dp_keep(ring, tol_m, pinned);
    let out: Vec<[f64; 2]> = ring.iter().zip(&keep).filter(|(_, k)| **k).map(|(p, _)| *p).collect();
    if out.len() >= 4 || ring.len() < 4 {
        return out;
    }
    let first = ring[0];
    let mut idx: Vec<usize> = (1..ring.len() - 1).collect();
    idx.sort_by(|&i, &j| {
        let d = |p: [f64; 2]| (p[0] - first[0]).hypot(p[1] - first[1]);
        d(ring[j]).total_cmp(&d(ring[i]))
    });
    let mut chosen: Vec<usize> = idx.into_iter().take(2).collect();
    chosen.sort_unstable();
    let mut out = vec![first];
    out.extend(chosen.into_iter().map(|i| ring[i]));
    out.push(first);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_straight_run_keeps_only_its_ends() {
        let ring: Vec<[f64; 2]> = (0..=10).map(|i| [-100.0 + f64::from(i) * 1e-4, 40.0]).collect();
        let keep = dp_keep(&ring, 5.0, &vec![false; ring.len()]);
        assert_eq!(keep.iter().filter(|k| **k).count(), 2);
    }

    #[test]
    fn a_pinned_vertex_is_never_dropped() {
        let ring: Vec<[f64; 2]> = (0..=10).map(|i| [-100.0 + f64::from(i) * 1e-4, 40.0]).collect();
        let mut pinned = vec![false; ring.len()];
        pinned[4] = true;
        assert!(dp_keep(&ring, 5.0, &pinned)[4]);
    }

    #[test]
    fn a_ring_never_collapses_below_four_points() {
        let tiny = vec![[0.0, 0.0], [1e-7, 0.0], [1e-7, 1e-7], [0.0, 1e-7], [0.0, 0.0]];
        let out = simplify_ring(&tiny, 5.0, &vec![false; tiny.len()]);
        assert!(out.len() >= 4);
        assert_eq!(out.first(), out.last());
    }

    #[test]
    fn a_ten_metre_bump_survives_five_metres_and_not_twenty() {
        // 1e-4 deg of latitude is about 11 m.
        let ring = vec![[0.0, 0.0], [5e-4, 1e-4], [1e-3, 0.0], [1e-3, 1e-3], [0.0, 1e-3], [0.0, 0.0]];
        let pins = vec![false; ring.len()];
        assert!(simplify_ring(&ring, 5.0, &pins).contains(&[5e-4, 1e-4]));
        assert!(!simplify_ring(&ring, 20.0, &pins).contains(&[5e-4, 1e-4]));
    }
}

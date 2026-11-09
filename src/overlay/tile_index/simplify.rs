//! Douglas–Peucker importance, ported from geojson-vt 4.0.2 `src/simplify.js`
//! (ISC, Copyright (c) 2015, Mapbox). Iterative rather than recursive: a
//! national ring has up to ~10^5 vertices and a degenerate one recurses
//! linearly.
//!
//! geojson-vt is distributed under the ISC License:
//!
//! > Copyright (c) 2015, Mapbox
//! >
//! > Permission to use, copy, modify, and/or distribute this software for any
//! > purpose with or without fee is hereby granted, provided that the above
//! > copyright notice and this permission notice appear in all copies.
//! >
//! > THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
//! > WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
//! > MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
//! > ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
//! > WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
//! > ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
//! > OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

/// Writes each vertex's DP importance (the squared distance at which it
/// stops being simplified away) into `out`. Endpoints get `INFINITY`;
/// vertices never selected above `sq_floor` get 0. `xy` is in [0,1]
/// Mercator units, as geojson-vt's projected coordinates.
pub(crate) fn importance(xy: &[[f64; 2]], sq_floor: f64, out: &mut [f64]) {
    let n = xy.len();
    debug_assert_eq!(out.len(), n);
    out.fill(0.0);
    if n == 0 {
        return;
    }
    out[0] = f64::INFINITY;
    out[n - 1] = f64::INFINITY;
    let mut stack: Vec<(usize, usize)> = vec![(0, n - 1)];
    while let Some((first, last)) = stack.pop() {
        if last <= first + 1 {
            continue;
        }
        let mid = first + ((last - first) >> 1);
        let mut max_sq = sq_floor;
        let mut min_pos_to_mid = last - first;
        let mut index: Option<usize> = None;
        let (a, b) = (xy[first], xy[last]);
        for (i, p) in xy.iter().enumerate().take(last).skip(first + 1) {
            let d = sq_seg_dist(*p, a, b);
            if d > max_sq {
                index = Some(i);
                max_sq = d;
            } else if d == max_sq {
                // geojson-vt issue #104: prefer a pivot near the middle.
                let pos = i.abs_diff(mid);
                if pos < min_pos_to_mid {
                    index = Some(i);
                    min_pos_to_mid = pos;
                }
            }
        }
        if max_sq > sq_floor {
            if let Some(i) = index {
                out[i] = max_sq;
                stack.push((first, i));
                stack.push((i, last));
            }
        }
    }
}

fn sq_seg_dist(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    let (mut x, mut y) = (a[0], a[1]);
    let (mut dx, mut dy) = (b[0] - x, b[1] - y);
    if dx != 0.0 || dy != 0.0 {
        let t = ((p[0] - x) * dx + (p[1] - y) * dy) / (dx * dx + dy * dy);
        if t > 1.0 {
            x = b[0];
            y = b[1];
        } else if t > 0.0 {
            x += dx * t;
            y += dy * t;
        }
    }
    dx = p[0] - x;
    dy = p[1] - y;
    dx * dx + dy * dy
}

/// The smallest zoom `z < sq_tol.len()` at which geojson-vt's
/// `geom[i + 2] > sqTolerance` keeps a vertex of this importance, or
/// `sq_tol.len()` (= max_zoom) when none does: `createTile` uses tolerance
/// 0 at max zoom, where every vertex is kept.
pub(crate) fn min_zoom_of(importance: f64, sq_tol: &[f64]) -> u8 {
    for (z, &t) in sq_tol.iter().enumerate() {
        if importance > t {
            return z as u8;
        }
    }
    sq_tol.len() as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_are_always_kept_and_a_collinear_middle_is_not() {
        let xy = [[0.0, 0.0], [0.5, 0.0], [1.0, 0.0]];
        let mut imp = [0.0; 3];
        importance(&xy, 0.0, &mut imp);
        assert_eq!(imp[0], f64::INFINITY);
        assert_eq!(imp[2], f64::INFINITY);
        assert_eq!(imp[1], 0.0);
    }

    #[test]
    fn a_peak_gets_its_squared_distance_as_importance() {
        let xy = [[0.0, 0.0], [0.5, 0.25], [1.0, 0.0]];
        let mut imp = [0.0; 3];
        importance(&xy, 0.0, &mut imp);
        assert!((imp[1] - 0.0625).abs() < 1e-15);
    }

    #[test]
    fn min_zoom_is_the_first_zoom_whose_tolerance_the_vertex_beats() {
        // sq_tol[z] = (3 / (4096 * 2^z))^2
        let sq_tol: Vec<f64> = (0..14).map(|z| (3.0 / (4096.0 * f64::from(1u32 << z))).powi(2)).collect();
        assert_eq!(min_zoom_of(f64::INFINITY, &sq_tol), 0);
        assert_eq!(min_zoom_of(sq_tol[5] * 1.0001, &sq_tol), 5);
        assert_eq!(min_zoom_of(sq_tol[5], &sq_tol), 6, "strictly greater, as geojson-vt's `>`");
        assert_eq!(min_zoom_of(0.0, &sq_tol), 14, "never beats a tolerance: kept only at max_zoom");
    }

    #[test]
    fn a_long_degenerate_run_does_not_overflow_the_stack() {
        let xy: Vec<[f64; 2]> = (0..200_000).map(|i| [i as f64, 0.0]).collect();
        let mut imp = vec![0.0; xy.len()];
        importance(&xy, 0.0, &mut imp);
        assert_eq!(imp.iter().filter(|v| v.is_infinite()).count(), 2);
    }
}

//! Axis-stripe clipping, ported from geojson-vt 4.0.2 `src/clip.js`
//! (ISC, Copyright (c) 2015, Mapbox). Keeps `k1 <= v <= k2` on `axis`.
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

/// Signed area of a closed ring (first == last), by the shoelace formula.
pub(crate) fn shoelace(ring: &[[f64; 2]]) -> f64 {
    let mut s = 0.0;
    for w in ring.windows(2) {
        s += w[0][0] * w[1][1] - w[1][0] * w[0][1];
    }
    s * 0.5
}

fn intersect(a: [f64; 2], b: [f64; 2], k: f64, axis: usize) -> [f64; 2] {
    if axis == 0 {
        let t = (k - a[0]) / (b[0] - a[0]);
        [k, a[1] + (b[1] - a[1]) * t]
    } else {
        let t = (k - a[1]) / (b[1] - a[1]);
        [a[0] + (b[0] - a[0]) * t, k]
    }
}

/// Polygon ring (closed, first == last) clipped to the stripe; `out` is
/// cleared first and is closed on return when it has at least 3 points.
pub(crate) fn clip_ring(ring: &[[f64; 2]], k1: f64, k2: f64, axis: usize, out: &mut Vec<[f64; 2]>) {
    out.clear();
    if ring.len() < 2 {
        return;
    }
    for w in ring.windows(2) {
        let (a, b) = (w[0], w[1]);
        let (av, bv) = (a[axis], b[axis]);
        if av < k1 {
            if bv > k1 {
                out.push(intersect(a, b, k1, axis));
            }
        } else if av > k2 {
            if bv < k2 {
                out.push(intersect(a, b, k2, axis));
            }
        } else {
            out.push(a);
        }
        if bv < k1 && av >= k1 {
            out.push(intersect(a, b, k1, axis));
        }
        if bv > k2 && av <= k2 {
            out.push(intersect(a, b, k2, axis));
        }
    }
    let last = ring[ring.len() - 1];
    if last[axis] >= k1 && last[axis] <= k2 {
        out.push(last);
    }
    if out.len() >= 3 && out[0] != out[out.len() - 1] {
        out.push(out[0]);
    }
}

/// Open polyline clipped to the stripe; every exit starts a new slice.
pub(crate) fn clip_line(line: &[[f64; 2]], k1: f64, k2: f64, axis: usize, slices: &mut Vec<Vec<[f64; 2]>>) {
    let mut slice: Vec<[f64; 2]> = Vec::new();
    for w in line.windows(2) {
        let (a, b) = (w[0], w[1]);
        let (av, bv) = (a[axis], b[axis]);
        let mut exited = false;
        if av < k1 {
            if bv > k1 {
                slice.push(intersect(a, b, k1, axis));
            }
        } else if av > k2 {
            if bv < k2 {
                slice.push(intersect(a, b, k2, axis));
            }
        } else {
            slice.push(a);
        }
        if bv < k1 && av >= k1 {
            slice.push(intersect(a, b, k1, axis));
            exited = true;
        }
        if bv > k2 && av <= k2 {
            slice.push(intersect(a, b, k2, axis));
            exited = true;
        }
        if exited {
            if slice.len() >= 2 {
                slices.push(std::mem::take(&mut slice));
            } else {
                slice.clear();
            }
        }
    }
    if let Some(&last) = line.last() {
        if last[axis] >= k1 && last[axis] <= k2 {
            slice.push(last);
        }
    }
    if slice.len() >= 2 {
        slices.push(slice);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(x0: f64, y0: f64, x1: f64, y1: f64) -> Vec<[f64; 2]> {
        vec![[x0, y0], [x1, y0], [x1, y1], [x0, y1], [x0, y0]]
    }

    #[test]
    fn a_ring_straddling_the_stripe_is_cut_at_both_lines_and_closed() {
        let mut out = Vec::new();
        clip_ring(&square(-10.0, 0.0, 10.0, 5.0), 0.0, 4.0, 0, &mut out);
        assert_eq!(out.first(), out.last(), "closed");
        assert!(out.iter().all(|p| p[0] >= 0.0 && p[0] <= 4.0));
        assert!((shoelace(&out).abs() - 20.0).abs() < 1e-9);
    }

    #[test]
    fn a_ring_outside_the_stripe_is_empty() {
        let mut out = Vec::new();
        clip_ring(&square(10.0, 0.0, 20.0, 5.0), 0.0, 4.0, 0, &mut out);
        assert!(out.len() < 4);
    }

    #[test]
    fn a_line_leaving_and_reentering_becomes_two_slices() {
        let line = [[0.0, 0.0], [10.0, 0.0], [10.0, 1.0], [0.0, 1.0]];
        let mut slices = Vec::new();
        clip_line(&line, -1.0, 5.0, 0, &mut slices);
        assert_eq!(slices.len(), 2);
        assert_eq!(slices[0], vec![[0.0, 0.0], [5.0, 0.0]]);
        assert_eq!(slices[1], vec![[5.0, 1.0], [0.0, 1.0]]);
    }

    #[test]
    fn intersections_are_shift_invariant_by_a_whole_tile() {
        // The same edge seen from a tile and from its right-hand neighbour.
        let a = [[4000.123, 1.0], [4200.5, 777.25], [4000.123, 1.0]];
        let b: Vec<[f64; 2]> = a.iter().map(|p| [p[0] - 4096.0, p[1]]).collect();
        let (mut oa, mut ob) = (Vec::new(), Vec::new());
        clip_ring(&a, 4096.0 - 1024.0, 4096.0 + 1024.0, 0, &mut oa);
        clip_ring(&b, -1024.0, 1024.0, 0, &mut ob);
        let shifted: Vec<[f64; 2]> = ob.iter().map(|p| [p[0] + 4096.0, p[1]]).collect();
        assert_eq!(oa, shifted, "bit-identical after the exact 4096 shift");
    }
}

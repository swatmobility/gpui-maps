//! Small chart painters for the satellites panel's "Distributions" card:
//! a stacked histogram with a brushed selection, painted as quads on one
//! canvas (no element per bar), in the census legend's manner
//! (`kit/src/panels/census.rs`' `histogram_strip`, which is tied to its
//! income axis and so not reused directly). Nothing here knows about
//! satellites: bins of per-class counts, class colours, a selection.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{div, prelude::*, px};

/// One chart's bars: `bins[k][c]` objects of class `c` in bin `k`,
/// stacked bottom-up in class order, each class in `colours[c]`. `inside`
/// (same shape, each count at most its `bins` one) is the part that passes
/// the chart's own filter too: drawn bright over the dimmed context. Left
/// empty, a bin is bright when its centre is inside the selection.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stacked {
    pub bins: Vec<Vec<u32>>,
    pub inside: Vec<Vec<u32>>,
    pub colours: Vec<gpui::Rgba>,
}

impl Stacked {
    /// The tallest bin's total (0 for an empty chart).
    #[must_use]
    pub fn peak(&self) -> u32 {
        self.bins.iter().map(|b| b.iter().sum::<u32>()).max().unwrap_or(0)
    }
}

/// Bars outside the selection are drawn at this alpha.
pub const OUTSIDE_ALPHA: f32 = 0.28;

/// A `width` x `height` stacked histogram: each class segment is drawn
/// dimmed, its `inside` part bright over it (crossfilter: the context the
/// other filters leave, the part the brush keeps); the selection `sel`
/// (fractions of the width) is a faint band between two thin handles in
/// `handle` (the theme's). Each paint records the chart's window
/// `(x, width)` in `bounds`, for the brush that drags on it. Heights are
/// on a square-root scale, so a bin of 30 still shows beside one of 10,000.
pub fn stacked_histogram(width: f32, height: f32, chart: Stacked, sel: (f32, f32), bounds: Rc<Cell<(f32, f32)>>, handle: gpui::Rgba) -> gpui::Div {
    div().relative().w(px(width)).h(px(height)).child(
        gpui::canvas(
            |_, _, _| {},
            move |b, _, window, _| {
                bounds.set((f32::from(b.origin.x), f32::from(b.size.width)));
                let (ox, bottom) = (b.origin.x, b.origin.y + b.size.height);
                let n = chart.bins.len().max(1);
                let bw = width / n as f32;
                let peak = (chart.peak() as f32).sqrt().max(1.0);
                for (k, bin) in chart.bins.iter().enumerate() {
                    let centre = (k as f32 + 0.5) / n as f32;
                    let by_centre = centre >= sel.0 && centre <= sel.1;
                    let total: u32 = bin.iter().sum();
                    if total == 0 {
                        continue;
                    }
                    let full = (total as f32).sqrt() / peak * (height - 2.0);
                    let mut y = 0.0;
                    for (c, &count) in bin.iter().enumerate() {
                        if count == 0 {
                            continue;
                        }
                        let h = (full * count as f32 / total as f32).max(0.75);
                        let colour = chart.colours.get(c).copied().unwrap_or(gpui::Rgba { r: 0.6, g: 0.6, b: 0.6, a: 1.0 });
                        let x = ox + px(k as f32 * bw + 0.5);
                        let w = px((bw - 1.0).max(0.75));
                        let bright = match chart.inside.get(k).and_then(|b| b.get(c)) {
                            Some(&n_in) => h * (n_in.min(count) as f32 / count as f32),
                            None if by_centre => h,
                            None => 0.0,
                        };
                        if bright < h {
                            window.paint_quad(gpui::fill(
                                gpui::Bounds::new(gpui::point(x, bottom - px(y + h)), gpui::size(w, px(h - bright))),
                                gpui::Rgba { a: OUTSIDE_ALPHA, ..colour },
                            ));
                        }
                        if bright > 0.0 {
                            window.paint_quad(gpui::fill(
                                gpui::Bounds::new(gpui::point(x, bottom - px(y + bright)), gpui::size(w, px(bright.max(0.75)))),
                                gpui::Rgba { a: 0.95, ..colour },
                            ));
                        }
                        y += h;
                    }
                }
                // The selection's band, faint, under the handles.
                let (l, r) = (sel.0.clamp(0.0, 1.0) * width, sel.1.clamp(0.0, 1.0) * width);
                if r - l < width - 0.5 {
                    window.paint_quad(gpui::fill(
                        gpui::Bounds::new(gpui::point(ox + px(l), b.origin.y), gpui::size(px((r - l).max(0.0)), px(height))),
                        gpui::Rgba { a: 0.08, ..handle },
                    ));
                }
                // The brush handles.
                for f in [sel.0, sel.1] {
                    let x = f.clamp(0.0, 1.0) * width;
                    window.paint_quad(gpui::fill(
                        gpui::Bounds::new(gpui::point(ox + px((x - 1.0).max(0.0)), b.origin.y), gpui::size(px(2.0), px(height))),
                        handle,
                    ));
                }
            },
        )
        .size_full(),
    )
}

/// Where a window x lands on a chart recorded as `(x, width)`, `0..1`.
#[must_use]
pub fn frac_at(bounds: (f32, f32), window_x: f32) -> f32 {
    if bounds.1 <= 0.0 { 0.0 } else { ((window_x - bounds.0) / bounds.1).clamp(0.0, 1.0) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_peak_is_the_tallest_stack_and_x_maps_onto_the_chart() {
        let c = Stacked { bins: vec![vec![1, 2], vec![5, 0], vec![]], ..Stacked::default() };
        assert_eq!(c.peak(), 5);
        assert_eq!(Stacked::default().peak(), 0);
        assert_eq!(frac_at((100.0, 200.0), 150.0), 0.25);
        assert_eq!(frac_at((100.0, 200.0), 50.0), 0.0);
        assert_eq!(frac_at((100.0, 0.0), 150.0), 0.0);
    }
}

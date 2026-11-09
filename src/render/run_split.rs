//! Splitting a long polyline into pieces small enough to tessellate
//! accurately.
//!
//! lyon computes a stroke vertex's normal as `(side_point - position) /
//! half_width` in f32. When the coordinate magnitude is large enough that
//! the float spacing approaches the half-width, those two points round to
//! the same value and the normal collapses to zero -- a zero-width,
//! invisible line. Keeping each tessellated run inside `MAX_RUN_EXTENT`
//! keeps the spacing far below the half-width.
//!
//! MapLibre reaches the same place from the other direction: its geometry
//! is clipped per tile, so a long route is already one small piece per tile
//! before it ever reaches a tessellator.

use std::ops::Range;

/// The largest coordinate magnitude a run may reach, in lyon units.
///
/// This value is squeezed from BOTH sides. Both bounds are real; changing
/// it without checking both is how the basemap breaks.
///
/// UPPER BOUND -- precision. f32 spacing is a step function of the
/// exponent, not a smooth curve: it is exactly 0.5 for EVERY magnitude in
/// `[2^22, 2^23)`, and steps to 1.0 at `2^23 = 8_388_608` itself. So
/// `8.0e6` sits just under that step with the spacing still 0.5, and it has
/// IDENTICAL precision to any smaller value in that octave -- the previous
/// `4.2e6` bought nothing at all over `8.0e6`. Against
/// `mesh::STROKE_TESSELLATION_WIDTH`'s half-width of 50, a spacing of 0.5
/// leaves a normal accurate to about 1%. (The earlier derivation of
/// "`0.5 * 2^23`" was wrong twice: that product is 4.19e6, and it is not
/// where the spacing changes.) This and `STROKE_TESSELLATION_WIDTH` are a
/// pair: raise the tessellation width and this may rise with it; lower it
/// and this must fall -- but it can only fall as far as the lower bound
/// below allows, and a fall within the same octave buys nothing.
///
/// LOWER BOUND -- basemap tile lines must never split. Only FILL geometry
/// is clipped to the tile extent (`style.rs`'s
/// `clip_fill_geometry_to_tile_extent`); LINE geometry reaches
/// `mesh::append_stroke` from the decoder with the tileset's clip buffer
/// intact. A line crossing a full tile therefore spans
/// `(4096 + 64 + 64) * mesh::COORD_SCALE` = 4.224e6 lyon units. Any bound
/// below that splits every such line, giving each a boundary notch and a
/// duplicated vertex for no benefit -- tiles have no precision problem to
/// solve, their coordinates are small by construction. `4.2e6` was under
/// this by 0.6%, which is exactly the trap.
/// `a_line_crossing_a_fully_buffered_tile_is_never_split` pins it.
pub(crate) const MAX_RUN_EXTENT: f32 = 8.0e6;

/// Index ranges covering `points`, each spanning no more than `max_extent`
/// on either axis.
///
/// Neighbouring ranges OVERLAP BY ONE INDEX: the vertex where a run ends is
/// also the vertex where the next begins, so every segment of the original
/// path belongs to exactly one run and none falls through the boundary.
///
/// A path already inside the bound yields a single range, so the common
/// case allocates one `Range` and copies nothing.
pub(crate) fn split_into_runs(
    points: &[lyon::math::Point],
    max_extent: f32,
) -> Vec<Range<usize>> {
    if points.len() < 2 {
        return Vec::new();
    }

    let mut runs = Vec::new();
    let mut start = 0usize;
    let (mut min_x, mut max_x) = (points[0].x, points[0].x);
    let (mut min_y, mut max_y) = (points[0].y, points[0].y);

    for index in 1..points.len() {
        let p = points[index];
        let (nx0, nx1) = (min_x.min(p.x), max_x.max(p.x));
        let (ny0, ny1) = (min_y.min(p.y), max_y.max(p.y));

        // `index > start + 1` because a run must keep at least one segment:
        // a single segment longer than the bound cannot be split -- there
        // is no vertex between its ends -- and is emitted whole rather than
        // dropped. It will tessellate with degraded normals, which is the
        // honest outcome for geometry that cannot be represented better.
        //
        // The run being closed must NOT absorb `p`: `p` is what pushed the
        // bbox past `max_extent`, so closing at `index` (excluding `p`)
        // keeps this run's own span at or under the bound -- it was fine
        // before `p` arrived, or this branch would have fired one point
        // earlier. The new run then starts at `index - 1`, the last point
        // still inside the old run, so that point is shared between both
        // runs (the required one-index overlap) and `p` becomes the new
        // run's second point rather than being dropped.
        if (nx1 - nx0 > max_extent || ny1 - ny0 > max_extent) && index > start + 1 {
            runs.push(start..index);
            start = index - 1;
            let anchor = points[start];
            min_x = anchor.x.min(p.x);
            max_x = anchor.x.max(p.x);
            min_y = anchor.y.min(p.y);
            max_y = anchor.y.max(p.y);
            continue;
        }
        min_x = nx0;
        max_x = nx1;
        min_y = ny0;
        max_y = ny1;
    }

    if start < points.len() - 1 {
        runs.push(start..points.len());
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;
    use lyon::math::point;

    fn line(n: usize, step: f32) -> Vec<lyon::math::Point> {
        (0..n).map(|i| point(i as f32 * step, 0.0)).collect()
    }

    /// The common case pays nothing: a path inside the bound is one run.
    #[test]
    fn a_short_path_is_a_single_run() {
        let points = line(500, 10.0); // 4990 units across
        assert_eq!(split_into_runs(&points, MAX_RUN_EXTENT), vec![0..500]);
    }

    /// Splitting is driven by EXTENT, not by vertex count: a dense trace
    /// inside a small area must not be split.
    #[test]
    fn a_dense_path_in_a_small_area_is_not_split() {
        let points = line(20_000, 0.1); // 20k vertices, 2000 units across
        assert_eq!(split_into_runs(&points, MAX_RUN_EXTENT).len(), 1);
    }

    /// ... and a sparse path that spans a long way IS split.
    #[test]
    fn a_sparse_path_spanning_a_long_way_is_split() {
        let points = line(4, 5.0e6); // 4 vertices, 15e6 units across
        assert!(split_into_runs(&points, MAX_RUN_EXTENT).len() > 1);
    }

    /// Neighbouring runs SHARE a vertex, so no segment falls between them.
    /// Without the overlap every boundary would drop one segment.
    #[test]
    fn neighbouring_runs_share_their_boundary_vertex() {
        let points = line(100, 1.0e5); // ~10e6 units across
        let runs = split_into_runs(&points, MAX_RUN_EXTENT);
        assert!(runs.len() > 1, "this path must split for the test to mean anything");
        for pair in runs.windows(2) {
            assert_eq!(pair[0].end - 1, pair[1].start, "runs must overlap by exactly one vertex");
        }
    }

    /// Every run is within the bound, which is the entire point.
    #[test]
    fn every_run_is_within_the_extent_bound() {
        let points = line(200, 1.0e5);
        for run in split_into_runs(&points, MAX_RUN_EXTENT) {
            let xs: Vec<f32> = points[run.clone()].iter().map(|p| p.x).collect();
            let span = xs.iter().cloned().fold(f32::MIN, f32::max)
                - xs.iter().cloned().fold(f32::MAX, f32::min);
            assert!(span <= MAX_RUN_EXTENT, "run {run:?} spans {span}");
        }
    }

    /// The runs cover the path exactly once, end to end.
    #[test]
    fn the_runs_cover_the_whole_path() {
        let points = line(200, 1.0e5);
        let runs = split_into_runs(&points, MAX_RUN_EXTENT);
        assert_eq!(runs.first().unwrap().start, 0);
        assert_eq!(runs.last().unwrap().end, points.len());
    }

    /// A BASEMAP TILE LINE MUST NEVER SPLIT. Read this before lowering
    /// `MAX_RUN_EXTENT`.
    ///
    /// Only FILL geometry is clipped to the tile extent (`style.rs`'s
    /// `clip_fill_geometry_to_tile_extent`). LINE geometry reaches
    /// `mesh::append_stroke` straight from the decoder with the tileset's
    /// clip buffer intact, so a line crossing a full tile spans
    /// `(4096 + 64 + 64) * COORD_SCALE` = 4.224e6 lyon units. If the bound
    /// sits below that, every such line splits and picks up a boundary
    /// notch and a duplicated vertex -- a visible regression on the basemap,
    /// invisible to the rest of the suite, whose tile fixtures all use
    /// comfortably in-extent coordinates.
    #[test]
    fn a_line_crossing_a_fully_buffered_tile_is_never_split() {
        let scale = crate::render::mesh::COORD_SCALE;
        // The widest a tile line can be: buffer to buffer, corner to corner.
        let lo = -64.0 * scale;
        let hi = (4096.0 + 64.0) * scale;
        let points: Vec<lyon::math::Point> =
            (0..=64).map(|i| point(lo + (hi - lo) * i as f32 / 64.0, lo)).collect();
        assert_eq!(
            split_into_runs(&points, MAX_RUN_EXTENT),
            vec![0..65],
            "a buffered tile line spans {} units and must stay a single run",
            hi - lo
        );
    }

    /// A single enormous segment cannot be split -- there is no vertex to
    /// split at -- and must still be returned rather than dropped.
    #[test]
    fn a_single_oversized_segment_is_returned_whole() {
        let points = vec![point(0.0, 0.0), point(1.0e9, 0.0)];
        assert_eq!(split_into_runs(&points, MAX_RUN_EXTENT), vec![0..2]);
    }

    #[test]
    fn a_degenerate_path_yields_nothing_to_draw() {
        assert!(split_into_runs(&[], MAX_RUN_EXTENT).is_empty());
        assert!(split_into_runs(&[point(0.0, 0.0)], MAX_RUN_EXTENT).is_empty());
    }

    /// The bound is derived from the half-width lyon must resolve. Pin the
    /// relationship, not just the number: at this magnitude f32 still
    /// resolves half a unit, so a 50-unit half-width is resolved to ~1%.
    #[test]
    fn the_bound_keeps_f32_able_to_resolve_the_half_width() {
        let spacing = (MAX_RUN_EXTENT.next_up() - MAX_RUN_EXTENT) as f64;
        assert!(spacing <= 0.5, "f32 spacing at the bound is {spacing}");
        let half_width = (crate::render::mesh::STROKE_TESSELLATION_WIDTH / 2.0) as f64;
        assert!(spacing / half_width < 0.02, "normals would carry {}% error", 100.0 * spacing / half_width);
    }
}

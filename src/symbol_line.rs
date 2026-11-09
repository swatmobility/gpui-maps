//! Anchor placement along a line, for `symbol-placement: line`.
//!
//! A pure geometric unit -- no GPU, no gpui, no style types, the same
//! separation `crate::collision` keeps. It walks a polyline in TILE
//! coordinates and returns evenly spaced anchors, each paired with a point
//! further along the line so the caller can derive a direction after
//! projection (see `LineAnchor::forward`).
//!
//! Two walks live here. `line_anchors` spaces whole symbols along a line --
//! one arrow, one shield -- and asks nothing about how long they are.
//! `place_glyphs_along` places the glyphs of ONE label, each carrying the
//! direction of the line beneath it, which is what makes a street name follow
//! its street instead of sitting at an angle across it.
//!
//! Modelled on MapLibre's `src/symbol/get_anchors.ts`. The glyph walk brings
//! over the two rejections that walk has and the anchor walk does not --
//! label-length awareness (a run that does not fit is refused rather than
//! running off the end) and max-angle rejection (text around a hairpin
//! overlaps itself, [`MAX_GLYPH_ANGLE_RAD`]). Cross-TILE deduplication is
//! still absent.
//!
//! Two more walks live here, both ported from MapLibre's
//! `src/symbol/symbol_layout.ts` and `src/symbol/merge_lines.ts`
//! (BSD-3-Clause), and both operate ACROSS features rather than within one:
//! [`merge_line_candidates`] stitches same-text candidates whose endpoints
//! touch into one longer polyline before the anchor walk runs on them --
//! what MapLibre calls `mergeLines`, and the reason a named road chopped
//! into one vector-tile feature per city block still gets a name instead of
//! having every block's label refused as longer than its own block.
//! [`anchor_too_close`] is MapLibre's `anchorIsTooClose`: it refuses a new
//! anchor that falls within half a `symbol-spacing` of an anchor already
//! accepted for the SAME text, which is what stops a dual carriageway's two
//! parallel, non-touching carriageways -- `merge_line_candidates` cannot
//! join them, since they share no endpoint -- from placing the same road
//! name twice side by side.
//!
//! `symbol_pass::line_glyph_quads` is the caller that turns
//! [`place_glyphs_along`]'s output into pixels -- one rotated quad per
//! glyph -- on an unpitched flat frame. Every pitched or globe frame
//! instead walks the PROJECTED line in screen pixels
//! ([`project_line_around`], [`place_glyphs_along_screen_upright`]), because
//! under perspective no single tile-units-per-pixel constant exists and the
//! tile walk spread near letters and piled far ones on top of each other.

use geo_types::{Coord, LineString, Point};

/// One placed symbol position along a line, in tile coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineAnchor {
    /// Where the symbol goes. Rounded to the nearest tile unit (see
    /// [`anchor_at_distance`]) -- fine for a DRAWN POSITION, but never use
    /// it to derive a rotation angle; see [`Self::segment_start`].
    pub at: Point<i32>,
    /// A point further along the line, in the geometry's own direction.
    /// Never equal to `at`. Used for the [`MAX_GLYPH_ANGLE_RAD`] bend check
    /// between successive anchors and, for a single line-placed SYMBOL
    /// (not a per-glyph run -- see `style::SymbolFeature::forward_ref`), as
    /// the caller's own rotation reference.
    pub forward: Point<i32>,
    /// The RAW, unrounded start vertex of the segment `at` was interpolated
    /// within.
    ///
    /// A per-glyph run must NOT derive its rotation angle from `at`: `at` is
    /// rounded to the nearest tile unit, so a caller computing
    /// `atan2(forward - at)` picks up that rounding as a spurious per-glyph
    /// wobble whenever `at` lands close to `forward` (a short segment, or a
    /// glyph near a segment's far end) -- consecutive glyphs in the same or
    /// adjacent short segments then get visibly different angles even
    /// though the underlying line is straight. MapLibre avoids this by
    /// deriving the angle from the segment's own two endpoint vertices,
    /// constant for every glyph placed within that segment
    /// (`placeGlyphAlongLine`, `src/symbol/projection.ts:915` in
    /// `maplibre-gl-js`, BSD-3-Clause: `segmentAngle = angle +
    /// Math.atan2(currentVertex.y - previousVertex.y, currentVertex.x -
    /// previousVertex.x)`, computed from the segment's raw endpoints, never
    /// from the interpolated point). `Self::segment_start` and
    /// `Self::segment_end` are that same pair here -- a caller drawing a
    /// per-glyph run should take its angle from `atan2(segment_end -
    /// segment_start)`, not from `forward - at`.
    pub segment_start: Point<i32>,
    /// The RAW, unrounded end vertex of that same segment. Equal to
    /// `forward` except in the rare case `forward` was resolved past a
    /// rounding coincidence (see [`anchor_at_distance`]) -- `segment_end`
    /// always names the immediate segment `at` sits in; `forward` may look
    /// further ahead.
    pub segment_end: Point<i32>,
}

/// One anchor from a multi-part walk, together with WHICH part it was
/// walked on.
///
/// The index is into the `parts` slice [`part_anchors_across`] was handed.
/// It exists because `symbol-spacing` is measured across the whole feature
/// (see [`line_anchors_across`]) while a curved label is laid out along ONE
/// polyline: [`place_glyphs_along`] takes a single `LineString`, so a
/// caller that means to bend text at this anchor needs the part beneath it,
/// and for a `MultiLineString` road that is neither the whole geometry nor
/// reliably part 0.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PartAnchor {
    /// Index into the `parts` slice -- the polyline this anchor sits on.
    pub part: usize,
    pub anchor: LineAnchor,
    /// How far along THAT PART the anchor sits, in tile units -- the
    /// `centre_distance` [`place_glyphs_along`] takes.
    ///
    /// Carried out of the walk rather than re-derived because the walk has
    /// it for free (`distance - consumed`, the feature-wide distance made
    /// part-local) and recovering it afterwards is both an O(n) re-walk of
    /// the part AND ambiguous: a part that doubles back passes through the
    /// same point at two different distances, and `anchor.at` alone cannot
    /// say which one this is.
    pub centre_distance: f64,
}

/// One GLYPH placed along a line by [`place_glyphs_along`], in tile
/// coordinates.
///
/// Not a [`LineAnchor`], because `at` here is NOT rounded. A symbol anchor
/// can afford to snap to a whole tile unit -- it moves one whole label by
/// under a unit -- but a glyph cannot: at pitch 0 each glyph is projected
/// from this position, and one tile unit is `512 * 2^(zoom - tile.z) /
/// 4096` logical px, 4 at z19 on a z14 tile (12 physical px on a 3x
/// phone). Snapping every glyph independently put letters on and off the
/// baseline and closed or opened the gaps between them by that much -- the
/// "Be a c h Rd" / "Mid dle Rd" defect. The screen walk
/// ([`place_glyphs_along_screen`]) never rounded; this is the tile walk
/// made as exact as it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GlyphAlong {
    /// The middle of the glyph's advance on the line, unrounded.
    pub at: (f64, f64),
    /// The RAW start vertex of the segment the glyph sits in -- see
    /// [`LineAnchor::segment_start`]; the glyph's rotation comes from this
    /// pair, never from `at`.
    pub segment_start: Point<i32>,
    /// The RAW end vertex of that segment.
    pub segment_end: Point<i32>,
}

/// Anchors every `spacing_tile_units` along `line`, the first at half a
/// spacing from its start.
///
/// A line shorter than one spacing gets a single anchor at its midpoint
/// rather than none: tiles cut roads at their boundaries, so short residual
/// pieces are common, and dropping them leaves visible gaps in an otherwise
/// continuous sequence of arrows.
///
/// Returns empty for a degenerate line (fewer than two distinct points) and
/// for a non-positive spacing.
///
/// A single-part convenience over `line_anchors_across`, which is what the
/// bake path calls: `symbol-spacing` is a property of the FEATURE, and a
/// feature is frequently a `MultiLineString`.
pub fn line_anchors(line: &LineString<i32>, spacing_tile_units: f64) -> Vec<LineAnchor> {
    line_anchors_across(std::slice::from_ref(line), spacing_tile_units)
}

/// Anchors every `spacing_tile_units` along a feature made of several
/// polyline `parts`, measured along the WHOLE feature rather than restarted
/// per part.
///
/// This is the distinction `symbol-spacing` lives or dies on. A road in a
/// vector tile is routinely one feature carrying dozens or hundreds of
/// `MultiLineString` parts -- split at every junction, bridge, or attribute
/// change upstream -- and each part is usually far shorter than one spacing.
/// Walking each part on its own therefore gives every part the
/// "shorter than one spacing" single-midpoint anchor, and a 439-part road
/// comes back with 439 labels: the spacing does nothing at all. Carrying one
/// accumulator across the parts gives the road `feature_length / spacing`
/// labels, which is what the property asks for.
///
/// The consequence at the short end is deliberate too: when the whole
/// FEATURE is shorter than one spacing it gets exactly ONE anchor, at the
/// feature's midpoint -- not one per part. The "short residual pieces still
/// get an anchor" guarantee is preserved, but it is now a guarantee about
/// the feature, which is the thing the reader sees as one road.
///
/// Modelled on MapLibre's `resample` in `src/symbol/get_anchors.ts`, whose
/// `markedDistance` accumulator is exactly this walk, one segment at a time.
/// Two things in MapLibre's version deliberately do NOT transfer:
///
/// * MapLibre restarts the accumulator per part because it runs
///   `mergeLines` first (`src/symbol/merge_lines.ts`), joining the parts of
///   a feature that share endpoints and carry the same label into one
///   polyline before layout, and then drops near-duplicates of the same text
///   with `anchorIsTooClose`. This crate has neither, so the accumulator
///   carries the whole feature instead -- the same end, one mechanism
///   earlier.
/// * MapLibre's phase alignment, `offset = (spacing / 2 * overscaling) %
///   spacing`, exists to make an overzoomed tile place anchors where its
///   PARENT tile placed them, because MapLibre keeps `symbol-spacing`
///   constant in a tile's own units and lets on-screen spacing grow with
///   overzoom. This crate makes the opposite choice -- `tile_units_per_px`
///   divides the spacing by the overzoom factor to hold the on-screen
///   spacing at the styled pixel count -- so the parent's anchor positions
///   are not a grid this walk could land on, and the `% spacing` term
///   reduces to the half-spacing start already used here.
///
/// Returns empty for a non-positive or NaN spacing and for a feature with no
/// part long enough to measure.
///
/// Drops the part attribution AND the part-local distance
/// [`part_anchors_across`] computes. Use that one when either is needed --
/// the bake path does, to carry both onto `style::SymbolFeature::line` and
/// `line_centre_distance`.
pub fn line_anchors_across(parts: &[LineString<i32>], spacing_tile_units: f64) -> Vec<LineAnchor> {
    part_anchors_across(parts, spacing_tile_units)
        .into_iter()
        .map(|placed| placed.anchor)
        .collect()
}

/// One part of a multi-part feature, measured up front for the anchor walk.
///
/// `part` is its index in the caller's `parts` slice, and that is the point
/// of naming this: the measured list is COMPACTED -- degenerate parts are
/// dropped, because they carry no length and can hold no anchor -- so a
/// position in the measured list is not a position in `parts`, and it is
/// `parts` the caller can index to find an anchor's polyline.
struct MeasuredPart {
    part: usize,
    points: Vec<Point<i32>>,
    /// Each segment's length, in `points` order.
    lengths: Vec<f64>,
    /// Their sum -- this part's length.
    total: f64,
}

/// [`line_anchors_across`], keeping the index of the part each anchor was
/// walked on.
///
/// This is the real walk; `line_anchors_across` is it with the index thrown
/// away. The index cannot be recovered afterwards from the anchor alone: a
/// `MultiLineString` road's parts touch at their endpoints and frequently
/// overlap in tile space, so "which part is this point on" has no unique
/// answer once the walk that knew has finished. See [`PartAnchor`].
pub fn part_anchors_across(parts: &[LineString<i32>], spacing_tile_units: f64) -> Vec<PartAnchor> {
    // Deliberately rejects NaN as well as non-positive values -- which is
    // why there is no separate NaN test above. Written as an explicit
    // `is_nan` check rather than `!(spacing_tile_units > 0.0)` (which relies
    // on NaN making every comparison false) so the NaN rejection is visible
    // to a reader rather than a comparison-negation trick they might
    // "simplify" into a hole; `cargo clippy --lib` also flags the negated
    // form (`neg_cmp_op_on_partial_ord`).
    if spacing_tile_units.is_nan() || spacing_tile_units <= 0.0 {
        return Vec::new();
    }

    // Measured once per part, up front: the walk below needs each part's
    // total before it can know where the feature's own midpoint is, and
    // re-measuring during the walk would read every segment twice.
    // Degenerate parts (a single point, or several coincident ones) are
    // dropped here rather than special-cased later -- they contribute no
    // length and can carry no anchor.
    //
    // Each entry keeps its index in `parts`. That is not the same as its
    // index in `measured` -- the `filter_map` below closes the gaps left by
    // dropped degenerate parts -- and it is the index the caller needs to
    // find the polyline an anchor sits on, so it has to be captured here,
    // before the compaction.
    let measured: Vec<MeasuredPart> = parts
        .iter()
        .enumerate()
        .filter_map(|(part, line)| {
            let points: Vec<Point<i32>> = line.points().collect();
            if points.len() < 2 {
                return None;
            }
            let (lengths, total) = segment_lengths(&points);
            if total <= 0.0 {
                return None;
            }
            Some(MeasuredPart { part, points, lengths, total })
        })
        .collect();
    if measured.is_empty() {
        return Vec::new();
    }

    let feature_total: f64 = measured.iter().map(|m| m.total).sum();

    let distances: Vec<f64> = if feature_total < spacing_tile_units {
        vec![feature_total / 2.0]
    } else {
        let mut d = spacing_tile_units / 2.0;
        let mut out = Vec::new();
        while d < feature_total {
            out.push(d);
            d += spacing_tile_units;
        }
        out
    };

    // `distances` is ascending and the parts are walked in order, so one
    // pass over both resolves every distance -- `consumed` is how much of
    // the feature lies behind the part currently under the cursor, which is
    // what turns a feature-wide distance into a part-local one.
    let mut anchors = Vec::with_capacity(distances.len());
    let mut cursor = 0usize;
    let mut consumed = 0.0f64;
    for measured_part in &measured {
        let part_end = consumed + measured_part.total;
        // Where in THIS part the last anchor landed: the distances are
        // ascending, so each resumes there (`anchor_at_distance_from`).
        let mut segment = (0usize, 0.0f64);
        while let Some(&distance) = distances.get(cursor) {
            // `>=`, not `>`: a distance landing exactly on a part's end is
            // resolved at the NEXT part's start instead, which is where the
            // feature actually continues. This mirrors MapLibre's strict
            // `markedDistance + spacing < distance + segmentDist`.
            if distance >= part_end {
                break;
            }
            // `distance - consumed` is the feature-wide distance made
            // part-local, and it is exactly what `place_glyphs_along` wants
            // as its `centre_distance` -- so it is carried out rather than
            // dropped on the floor. See `PartAnchor::centre_distance`.
            let centre_distance = distance - consumed;
            if let Some(anchor) = anchor_at_distance_from(
                &measured_part.points,
                &measured_part.lengths,
                centre_distance,
                &mut segment,
            ) {
                anchors.push(PartAnchor { part: measured_part.part, anchor, centre_distance });
            }
            cursor += 1;
        }
        consumed = part_end;
    }
    anchors
}

/// The polyline's total length, in tile units.
///
/// Exists for exactly one caller and one arithmetic: a label that reads
/// right-to-left on screen is re-placed against the REVERSED line, and the
/// same anchor addressed from the other end is `line_length(line) -
/// centre_distance`. Exposed rather than recomputed at the call site so the
/// two measurements are one measurement -- `place_glyphs_along` walks
/// [`segment_lengths`] and so does this, and a caller that summed the
/// vertices itself could disagree with it by a rounding step and push the
/// flipped run off the end of its own line.
///
/// `0.0` for a degenerate line, which is what `place_glyphs_along` refuses
/// on anyway.
pub fn line_length(line: &LineString<i32>) -> f64 {
    let points: Vec<Point<i32>> = line.points().collect();
    if points.len() < 2 {
        return 0.0;
    }
    segment_lengths(&points).1
}

/// Each segment's length, and their total. Separated so the anchor walk and
/// the glyph walk measure the line exactly one way.
fn segment_lengths(points: &[Point<i32>]) -> (Vec<f64>, f64) {
    let mut lengths = Vec::with_capacity(points.len().saturating_sub(1));
    let mut total = 0.0f64;
    for pair in points.windows(2) {
        let dx = (pair[1].x() - pair[0].x()) as f64;
        let dy = (pair[1].y() - pair[0].y()) as f64;
        let len = (dx * dx + dy * dy).sqrt();
        lengths.push(len);
        total += len;
    }
    (lengths, total)
}

/// The largest turn tolerated between two adjacent glyphs of one label --
/// MapLibre's `MAX_GLYPH_ANGLE`, `src/symbol/symbol_layout.ts`:
///
/// ```text
/// const MAX_GLYPH_ANGLE = 45 * Math.PI / 180;
/// ```
///
/// It is the value MapLibre hands its own `getAnchors` for every
/// line-placed label, so a street name this crate refuses to bend is one
/// MapLibre refuses to bend too. Named here rather than written as a
/// literal at the one call site so the citation travels with the number.
pub const MAX_GLYPH_ANGLE_RAD: f64 = 45.0 * std::f64::consts::PI / 180.0;

/// This anchor's heading in TILE space, for comparing one glyph's direction
/// against the next -- only ever used to ask how sharply the LINE ITSELF
/// bends, which the camera does not change.
///
/// From `segment_start`/`segment_end`, the segment's own RAW endpoints, not
/// `at`/`forward`: `at` is rounded (see [`LineAnchor::segment_start`]), and
/// this bend check must see the line's true geometry, not a glyph's own
/// rounding noise -- a straight run should never measure a "bend" at all.
#[cfg(test)]
fn heading(anchor: &LineAnchor) -> f64 {
    segment_heading(anchor.segment_start, anchor.segment_end)
}

/// [`heading`] for a placed glyph: the same raw-endpoint rule.
fn glyph_heading(glyph: &GlyphAlong) -> f64 {
    segment_heading(glyph.segment_start, glyph.segment_end)
}

fn segment_heading(start: Point<i32>, end: Point<i32>) -> f64 {
    let dx = (end.x() - start.x()) as f64;
    let dy = (end.y() - start.y()) as f64;
    dy.atan2(dx)
}

/// Places one glyph per entry in `advances` along `line`, centred on
/// `centre_distance`, each with the direction of the line beneath it.
///
/// This is what makes text follow a road rather than sit across it. Each
/// glyph gets its own position and its own `forward`, so a label bends with
/// the line instead of being one rigid rectangle -- the difference between a
/// street name and a street name drawn at an angle to its street.
///
/// `advances` are per-glyph advance widths in TILE units, in reading order;
/// the glyph is centred on its own advance, which is what keeps spacing even
/// through a bend. `max_angle_delta` is the largest turn tolerated between
/// two adjacent glyphs, in radians.
///
/// `None` -- meaning "do not place this label here" -- when:
///
/// * the run does not fit inside the line at this position. A label running
///   off the end is worse than one not drawn: the missing glyphs are silent.
/// * the line turns harder than `max_angle_delta` under the label. This is
///   the rejection MapLibre's `get_anchors.ts` applies for the same reason --
///   text around a hairpin overlaps itself and reads as a smudge. The caller
///   can retry at another anchor, which is how a curvy road still gets a
///   label on its straighter stretches.
///
/// Returns [`GlyphAlong`]s -- UNROUNDED positions plus the raw segment the
/// glyph sits in. See that type for why a glyph must not be snapped to a
/// whole tile unit.
///
/// [`MAX_GLYPH_ANGLE_RAD`] is the limit MapLibre itself passes.
pub fn place_glyphs_along(
    line: &LineString<i32>,
    centre_distance: f64,
    advances: &[f64],
    max_angle_delta: f64,
) -> Option<Vec<GlyphAlong>> {
    if advances.is_empty() {
        return None;
    }
    let points: Vec<Point<i32>> = line.points().collect();
    if points.len() < 2 {
        return None;
    }
    let (segment_lengths, total) = segment_lengths(&points);
    if total <= 0.0 {
        return None;
    }

    let run: f64 = advances.iter().sum();
    if !run.is_finite() || run <= 0.0 {
        return None;
    }
    // Centred on the anchor, the way a point label is centred on its own.
    let start = centre_distance - run / 2.0;
    if !start.is_finite() || start < 0.0 || start + run > total {
        return None;
    }

    let mut placements = Vec::with_capacity(advances.len());
    let mut cursor = start;
    for advance in advances {
        // The glyph sits at the middle of its OWN advance, not at its leading
        // edge: through a bend the leading edge of a wide glyph and its
        // trailing edge are at different headings, and centring is what keeps
        // the run evenly spaced instead of bunching on the inside of a curve.
        placements.push(glyph_at_distance(&points, &segment_lengths, cursor + advance / 2.0)?);
        cursor += advance;
    }

    for pair in placements.windows(2) {
        let mut delta = glyph_heading(&pair[1]) - glyph_heading(&pair[0]);
        // Wrap to [-pi, pi]: two headings either side of the +/-pi seam
        // differ by a hair, not by a full turn.
        while delta > std::f64::consts::PI {
            delta -= std::f64::consts::TAU;
        }
        while delta < -std::f64::consts::PI {
            delta += std::f64::consts::TAU;
        }
        if delta.abs() > max_angle_delta {
            return None;
        }
    }
    Some(placements)
}

/// Resolves a distance along the polyline to an UNROUNDED glyph position
/// and the raw segment it lies in -- [`anchor_at_distance`] without the
/// snap to a whole tile unit (see [`GlyphAlong`]).
fn glyph_at_distance(points: &[Point<i32>], segment_lengths: &[f64], distance: f64) -> Option<GlyphAlong> {
    let mut remaining = distance;
    for (i, &len) in segment_lengths.iter().enumerate() {
        if len <= 0.0 {
            continue;
        }
        if remaining <= len {
            let (a, b) = (points[i], points[i + 1]);
            let t = remaining / len;
            let at = (
                a.x() as f64 + (b.x() - a.x()) as f64 * t,
                a.y() as f64 + (b.y() - a.y()) as f64 * t,
            );
            return Some(GlyphAlong { at, segment_start: a, segment_end: b });
        }
        remaining -= len;
    }
    None
}

/// Resolves a distance along the polyline to a position, plus the next
/// vertex ahead of it.
#[cfg_attr(not(test), allow(dead_code))]
fn anchor_at_distance(
    points: &[Point<i32>],
    segment_lengths: &[f64],
    distance: f64,
) -> Option<LineAnchor> {
    anchor_at_distance_from(points, segment_lengths, distance, &mut (0, 0.0))
}

/// [`anchor_at_distance`], resuming from `cursor` -- `(segment, distance
/// at that segment's start)` -- and leaving it at the segment the anchor
/// landed in. A walk resolving ASCENDING distances along one part passes
/// the same cursor to each, so the part is walked once in total rather
/// than once per anchor: a 3,000-vertex, 150 km route at 3,000 anchors
/// was 9 M segment steps per zoom (`overlay::line_symbols`), a basemap
/// road a handful either way.
fn anchor_at_distance_from(
    points: &[Point<i32>],
    segment_lengths: &[f64],
    distance: f64,
    cursor: &mut (usize, f64),
) -> Option<LineAnchor> {
    let (mut i, mut start) = *cursor;
    while i < segment_lengths.len() {
        let len = segment_lengths[i];
        let remaining = distance - start;
        if len > 0.0 && remaining <= len {
            *cursor = (i, start);
            let (a, b) = (points[i], points[i + 1]);
            let t = remaining / len;
            let at = Point::new(
                (a.x() as f64 + (b.x() - a.x()) as f64 * t).round() as i32,
                (a.y() as f64 + (b.y() - a.y()) as f64 * t).round() as i32,
            );
            // `b` is the vertex ahead -- unless rounding put `at` exactly on
            // it, in which case walk on to the first later vertex that
            // differs. If there is no such vertex (`b` is the line's own
            // final point), extrapolate along this segment's own direction
            // instead: `a != b` here (zero-length segments are skipped
            // above), so `at + (b - a)` can never equal `at`.
            let forward = if at != b {
                b
            } else {
                match points[i + 2..].iter().find(|p| **p != at) {
                    Some(p) => *p,
                    None => Point::new(at.x() + (b.x() - a.x()), at.y() + (b.y() - a.y())),
                }
            };
            return Some(LineAnchor { at, forward, segment_start: a, segment_end: b });
        }
        start += len;
        i += 1;
    }
    *cursor = (i, start);
    None
}

/// How many tile units make up one LOGICAL pixel for a tile at `tile_z`
/// displayed at `bake_zoom`.
///
/// `symbol-spacing` is expressed in pixels but the anchor walk runs in tile
/// coordinates, so this is the conversion between them. `bake_zoom` is an
/// integer (`MapView::bake_zoom`), so the result is exact at a zoom
/// bucket's start and drifts to 2x by its end, corrected when the crossing
/// rebakes the plan -- the same quantisation `render::dash_scale` makes,
/// and the same one MapLibre's per-integer-zoom worker layout makes.
///
/// `bake_zoom >= tile_z` always holds (`MapView::bake_zoom` is
/// `tile.z.max(display_zoom())`), so the exponent is never negative.
pub fn tile_units_per_px(bake_zoom: u8, tile_z: u8) -> f64 {
    const MVT_EXTENT: f64 = 4096.0;
    let zoom_delta = bake_zoom.saturating_sub(tile_z) as i32;
    MVT_EXTENT / (crate::geo::TILE_SIZE * 2.0f64.powi(zoom_delta))
}

/// The zoom a tile's `symbol-spacing` is laid out FOR -- what
/// [`tile_units_per_px`] should be handed as its `bake_zoom`.
///
/// `tile_units_per_px(bake_zoom, tile_z)` reads `bake_zoom - tile_z` as how
/// much the tile is MAGNIFIED on screen. That held while the only tiles
/// below the bake zoom were overzoomed ones at the source's ceiling. Under
/// the distance LOD (`cover_plan::flat_cover`) a cover tile below
/// `tile_zoom` (the cover's own, source-clamped zoom) is coarse because it
/// is FAR, and is drawn near its native scale: laid out at the display
/// zoom, a z11 tile under a z16 camera got 250 px of spacing as 62.5 tile
/// units -- an anchor every ~8 screen px, 32x the candidates.
///
/// So a tile below `tile_zoom` is laid out at its own zoom -- MapLibre's
/// rule: a covering tile below the source maximum is its own
/// `overscaledZ` (`covering_tiles.ts`), and buckets are laid out at it --
/// and a tile at or above `tile_zoom` at `bake_zoom`, exactly as before.
/// The same holds for an ancestor used as a stand-in, which is drawn
/// magnified: its line labels come out sparse while it stands in, as
/// MapLibre's do. At pitch 0 the cover itself is all at `tile_zoom`, so
/// every cover tile's spacing is unchanged.
///
/// Deterministic in `(bake_zoom, tile_zoom, tile_z)`, so a bake stamped
/// with the display zoom is still valid for the same display zoom.
#[must_use]
pub fn symbol_spacing_zoom(bake_zoom: u8, tile_zoom: u8, tile_z: u8) -> u8 {
    if tile_z < tile_zoom {
        tile_z
    } else {
        bake_zoom
    }
}

/// One glyph of a line-placed label, placed on the SCREEN: where its centre
/// lands and which way it faces.
///
/// The one thing both the quad builder and the collision box consume
/// (`symbol_pass::line_glyph_quads`, `symbol_pass::glyph_run_collision_box`),
/// so what draws and what collides cannot disagree.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScreenGlyph {
    /// The middle of the glyph's advance ON the line, in PHYSICAL pixels,
    /// y-down.
    pub centre: (f32, f32),
    /// Clockwise rotation in radians (y-down screen), along the line's own
    /// digitised direction under the glyph.
    pub angle: f32,
}

/// A stretch of a polyline projected to the screen, reaching outward from
/// one anchor far enough to carry a label centred on it.
///
/// Built by [`project_line_around`] and walked by
/// [`place_glyphs_along_screen`]; separate so the walk can be tested on
/// hand-written points and the projection on a hand-written camera.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectedLine {
    /// Screen positions, in PHYSICAL pixels, in the line's digitised order.
    /// The anchor itself is one of them, at [`Self::anchor_index`].
    pub points: Vec<(f64, f64)>,
    /// `segments[j]` is the index of the TILE segment (`line[k] ->
    /// line[k + 1]`) that `points[j] -> points[j + 1]` is a piece of --
    /// the bend check's key back into tile space. Always
    /// `points.len() - 1` long.
    pub segments: Vec<usize>,
    /// Where the anchor sits in [`Self::points`].
    pub anchor_index: usize,
}

impl ProjectedLine {
    /// The same stretch walked from the other end: MapLibre's flipped
    /// placement for a label that would read right to left.
    #[must_use]
    pub fn reversed(&self) -> Self {
        Self {
            points: self.points.iter().rev().copied().collect(),
            segments: self.segments.iter().rev().copied().collect(),
            anchor_index: self.points.len().saturating_sub(1).saturating_sub(self.anchor_index),
        }
    }
}

/// The unrounded point `distance` tile units along `line`, and the index of
/// the segment it lies on.
///
/// The f64 twin of [`anchor_at_distance`]'s interpolation, without the
/// rounding that function applies for a DRAWN tile position: the screen
/// walk starts from this point, and rounding it would move the whole label
/// by up to half a tile unit.
///
/// `None` for a degenerate line, a non-finite or negative distance, or one
/// past the line's end.
pub fn point_at_distance(line: &LineString<i32>, distance: f64) -> Option<(usize, (f64, f64))> {
    if !distance.is_finite() || distance < 0.0 {
        return None;
    }
    let points: Vec<Point<i32>> = line.points().collect();
    if points.len() < 2 {
        return None;
    }
    let (lengths, _) = segment_lengths(&points);
    let mut remaining = distance;
    for (i, &len) in lengths.iter().enumerate() {
        if len <= 0.0 {
            continue;
        }
        if remaining <= len {
            let (a, b) = (points[i], points[i + 1]);
            let t = remaining / len;
            return Some((
                i,
                (
                    a.x() as f64 + (b.x() - a.x()) as f64 * t,
                    a.y() as f64 + (b.y() - a.y()) as f64 * t,
                ),
            ));
        }
        remaining -= len;
    }
    None
}

/// How far past the needed reach a synthesised end vertex is put, in
/// pixels, so float noise between the projection's running sum and the
/// walk's cannot refuse a label that exactly fits.
const SYNTHESISED_SLACK_PX: f64 = 1.0;

/// Projects `line` outward from the anchor on segment `segment` (tile
/// position `anchor_tile`, screen position `anchor_screen`) until at least
/// `reach_px` of SCREEN length is covered in each direction, or the line
/// ends.
///
/// Lazy: only the vertices a label of this length can reach are projected,
/// which on a long merged road is a handful out of hundreds.
///
/// # A vertex behind the near plane
///
/// `project` returns `None` for it. The segment toward it still has a
/// perfectly good screen DIRECTION -- the one it leaves the screen along --
/// so the stretch is finished with a synthesised vertex exactly far enough
/// along that direction to carry the rest of the run: MapLibre's
/// `projectTruncatedLineSegment` (`src/symbol/projection.ts:538-546`,
/// BSD-3-Clause), which projects a point one tile unit BACK from the last
/// good vertex (away from the lost one) and extends the line through the
/// two projected points. `None` when even that point fails to project.
///
/// Works on the globe as well as flat: `project` is whatever
/// `symbol_pass::AnchorProjection::screen` does for this frame.
pub fn project_line_around(
    line: &LineString<i32>,
    segment: usize,
    anchor_tile: (f64, f64),
    anchor_screen: (f64, f64),
    reach_px: f64,
    mut project: impl FnMut((f64, f64)) -> Option<(f64, f64)>,
) -> Option<ProjectedLine> {
    let tile: Vec<(f64, f64)> = line.points().map(|p| (p.x() as f64, p.y() as f64)).collect();
    if segment + 1 >= tile.len() || !reach_px.is_finite() || reach_px < 0.0 {
        return None;
    }
    let finite = |p: &(f64, f64)| p.0.is_finite() && p.1.is_finite();
    if !finite(&anchor_screen) || !finite(&anchor_tile) {
        return None;
    }
    let mut project = move |p: (f64, f64)| project(p).filter(finite);
    // A step outward to vertex `k` walks tile segment `k - 1` going
    // forward and tile segment `k` going backward.
    let forwards = (segment + 1..tile.len()).map(|k| (k, k - 1));
    let backwards = (0..=segment).rev().map(|k| (k, k));
    let forward = walk_out(&tile, anchor_tile, anchor_screen, reach_px, forwards, &mut project)?;
    let backward = walk_out(&tile, anchor_tile, anchor_screen, reach_px, backwards, &mut project)?;

    let mut points = Vec::with_capacity(backward.len() + forward.len() + 1);
    let mut segments = Vec::with_capacity(backward.len() + forward.len());
    for &(point, seg) in backward.iter().rev() {
        points.push(point);
        segments.push(seg);
    }
    let anchor_index = points.len();
    points.push(anchor_screen);
    for &(point, seg) in &forward {
        points.push(point);
        segments.push(seg);
    }
    Some(ProjectedLine { points, segments, anchor_index })
}

/// One direction of [`project_line_around`]: the projected vertices in
/// walking order, each with the tile segment the step to it crossed.
fn walk_out(
    tile: &[(f64, f64)],
    anchor_tile: (f64, f64),
    anchor_screen: (f64, f64),
    reach_px: f64,
    order: impl Iterator<Item = (usize, usize)>,
    project: &mut impl FnMut((f64, f64)) -> Option<(f64, f64)>,
) -> Option<Vec<((f64, f64), usize)>> {
    let mut out = Vec::new();
    let (mut prev_tile, mut prev_screen) = (anchor_tile, anchor_screen);
    let mut walked = 0.0f64;
    for (k, seg) in order {
        if walked >= reach_px {
            break;
        }
        let cur_tile = tile[k];
        let (dx, dy) = (cur_tile.0 - prev_tile.0, cur_tile.1 - prev_tile.1);
        let tile_len = dx.hypot(dy);
        if tile_len <= 0.0 {
            // A repeated vertex, or the anchor sitting exactly on one.
            continue;
        }
        if let Some(screen) = project(cur_tile) {
            walked += (screen.0 - prev_screen.0).hypot(screen.1 - prev_screen.1);
            out.push((screen, seg));
            prev_tile = cur_tile;
            prev_screen = screen;
            continue;
        }
        let behind = project((prev_tile.0 - dx / tile_len, prev_tile.1 - dy / tile_len))?;
        let (ux, uy) = (prev_screen.0 - behind.0, prev_screen.1 - behind.1);
        let len = ux.hypot(uy);
        if !(len > 0.0 && len.is_finite()) {
            return None;
        }
        let extra = reach_px - walked + SYNTHESISED_SLACK_PX;
        out.push(((prev_screen.0 + ux / len * extra, prev_screen.1 + uy / len * extra), seg));
        return Some(out);
    }
    Some(out)
}

/// The glyph whose centre is `d` pixels along `line` from its anchor
/// (negative: before it), with the tile segment it landed on.
fn screen_point_along(line: &ProjectedLine, d: f64) -> Option<(ScreenGlyph, usize)> {
    let points = &line.points;
    let a = line.anchor_index;
    let glyph = |p: (f64, f64), q: (f64, f64), t: f64, from: (f64, f64), to: (f64, f64), seg: usize| {
        let centre = (p.0 + (q.0 - p.0) * t, p.1 + (q.1 - p.1) * t);
        let angle = (to.1 - from.1).atan2(to.0 - from.0);
        (ScreenGlyph { centre: (centre.0 as f32, centre.1 as f32), angle: angle as f32 }, seg)
    };
    if d >= 0.0 {
        let mut remaining = d;
        for j in a..points.len().saturating_sub(1) {
            let (p, q) = (points[j], points[j + 1]);
            let len = (q.0 - p.0).hypot(q.1 - p.1);
            if len.is_nan() || len <= 0.0 {
                continue;
            }
            if remaining <= len {
                return Some(glyph(p, q, remaining / len, p, q, line.segments[j]));
            }
            remaining -= len;
        }
        if d > 0.0 {
            return None;
        }
    }
    let mut remaining = -d;
    for j in (1..=a).rev() {
        // Walking BACKWARD from `p` to `q`; the glyph still faces the
        // line's own direction, `q -> p`.
        let (p, q) = (points[j], points[j - 1]);
        let len = (q.0 - p.0).hypot(q.1 - p.1);
        if len.is_nan() || len <= 0.0 {
            continue;
        }
        if remaining <= len {
            return Some(glyph(p, q, remaining / len, q, p, line.segments[j - 1]));
        }
        remaining -= len;
    }
    None
}

/// Places one glyph per entry in `advances_px` along a PROJECTED line,
/// centred on its anchor, spaced in SCREEN pixels -- MapLibre's
/// `placeGlyphsAlongLine` / `placeGlyphAlongLine`
/// (`src/symbol/projection.ts:453`, `:808+`, BSD-3-Clause) for
/// viewport-pitched text.
///
/// # Why the screen and not the tile
///
/// The advances come off a buffer shaped at a size that already carries
/// the anchor's perspective ratio, so they are SCREEN lengths. Walking them
/// along the tile polyline through one flat tile-units-per-pixel constant
/// and then projecting each glyph with full perspective scaled every gap by
/// the local screen-pixels-per-flat-pixel: under pitch the far letters
/// overlapped ("RpublicBulevard") and the near ones spread ("R o c h o r").
/// Walking the projected line keeps consecutive centres exactly one mean
/// advance apart wherever the label is.
///
/// Glyph `i` sits at signed distance `-run/2 + sum(adv[..i]) + adv[i]/2`
/// from the anchor and takes its angle from the projected endpoints of the
/// segment under it -- constant across a segment, so a straight road has no
/// per-glyph wobble (see `symbol_pass::line_glyph_angle`).
///
/// `None` -- do not place -- when the run does not fit inside the projected
/// stretch at either end, or when the TILE line turns harder than
/// `max_angle_delta` between two consecutive glyphs (the same bend gate
/// [`place_glyphs_along`] applies, on the segments the glyphs actually
/// occupy).
pub fn place_glyphs_along_screen(
    line: &ProjectedLine,
    tile_line: &LineString<i32>,
    advances_px: &[f64],
    max_angle_delta: f64,
) -> Option<Vec<ScreenGlyph>> {
    if advances_px.is_empty()
        || line.points.len() < 2
        || line.anchor_index >= line.points.len()
        || line.segments.len() + 1 != line.points.len()
    {
        return None;
    }
    let run: f64 = advances_px.iter().sum();
    if !run.is_finite() || run <= 0.0 {
        return None;
    }
    // The whole run must fit, not only the glyph centres -- the rule the
    // tile walk has always applied.
    screen_point_along(line, -run / 2.0)?;
    screen_point_along(line, run / 2.0)?;

    let tile: Vec<Point<i32>> = tile_line.points().collect();
    let heading = |seg: usize| -> Option<f64> {
        let (a, b) = (tile.get(seg)?, tile.get(seg + 1)?);
        Some(((b.y() - a.y()) as f64).atan2((b.x() - a.x()) as f64))
    };
    let mut placed = Vec::with_capacity(advances_px.len());
    let mut previous: Option<f64> = None;
    let mut cursor = -run / 2.0;
    for advance in advances_px {
        let (glyph, seg) = screen_point_along(line, cursor + advance / 2.0)?;
        if !(glyph.centre.0.is_finite() && glyph.centre.1.is_finite() && glyph.angle.is_finite()) {
            return None;
        }
        let here = heading(seg)?;
        if let Some(before) = previous {
            let mut delta = here - before;
            while delta > std::f64::consts::PI {
                delta -= std::f64::consts::TAU;
            }
            while delta < -std::f64::consts::PI {
                delta += std::f64::consts::TAU;
            }
            if delta.abs() > max_angle_delta {
                return None;
            }
        }
        previous = Some(here);
        placed.push(glyph);
        cursor += advance;
    }
    Some(placed)
}

/// [`place_glyphs_along_screen`], kept upright: a run whose first glyph
/// lands right of its last reads backwards, so it is re-walked against the
/// reversed stretch, and if THAT does not fit either nothing is placed --
/// MapLibre's `needsFlipping` / `hideGlyphs` (`projection.ts:299-326`).
pub fn place_glyphs_along_screen_upright(
    line: &ProjectedLine,
    tile_line: &LineString<i32>,
    advances_px: &[f64],
    max_angle_delta: f64,
) -> Option<Vec<ScreenGlyph>> {
    let placed = place_glyphs_along_screen(line, tile_line, advances_px, max_angle_delta)?;
    match (placed.first(), placed.last()) {
        (Some(first), Some(last)) if first.centre.0 > last.centre.0 => {
            place_glyphs_along_screen(&line.reversed(), tile_line, advances_px, max_angle_delta)
        }
        _ => Some(placed),
    }
}

/// One LINE-placed symbol offered to [`merge_line_candidates`]: a resolved
/// label text, the polyline it would be placed on, and whatever the caller
/// needs carried onto every anchor eventually walked off the (possibly
/// merged) result.
///
/// A pure carrier, generic over the payload: this module knows nothing of
/// `style::SymbolFeature` (see the module doc comment's "no style types"
/// rule), so the caller's per-feature bag of resolved paint/layout values
/// travels as `T` and is copied back out onto every `SymbolFeature` the
/// caller builds from the anchors this produces.
#[derive(Debug, Clone)]
pub struct LineMergeCandidate<T> {
    pub text: String,
    pub points: LineString<i32>,
    pub payload: T,
}

/// Stitches [`LineMergeCandidate`]s that carry the SAME text and whose
/// endpoints touch into one longer polyline, before the anchor walk.
///
/// This is MapLibre's `mergeLines` (`src/symbol/merge_lines.ts`,
/// maplibre-gl-js, BSD-3-Clause), called for every `symbol-placement: line`
/// bucket right after its features are collected
/// (`src/data/bucket/symbol_bucket.ts`'s `if (layout.get('symbol-placement')
/// === 'line') { this.features = mergeLines(this.features); }`, whose own
/// comment reads "Merge adjacent lines with the same text to improve
/// labeling. It's better to place labels on one long line than on many
/// short segments"). A named road is routinely one vector-tile feature per
/// OSM way -- one per city block -- and without this step
/// [`part_anchors_across`] walks each block alone: a label longer than its
/// own block is refused outright by [`place_glyphs_along`]'s length check,
/// and only a naturally long block ever keeps its name. This module's own
/// former doc comment flagged this and [`anchor_too_close`] as the two
/// things "this crate has neither"; this is the first of the two closed.
///
/// A candidate with no text (empty string) is never a merge participant,
/// matching `mergeLines`'s own `if (!text) { add(k); continue; }` --  an
/// icon-only line (a one-way arrow) repeats along its own feature via
/// `symbol-spacing` already, and there is no text to key a merge on.
///
/// Ported BEHAVIOUR, not ported code: `merge_lines.ts` keys a candidate's
/// two ends by `` `${text}:${x}:${y}` `` and splices one JS array in place
/// with `pop`/`shift`/`concat`; this walks the same three cases -- extend
/// an existing run on its right, extend one on its left, or bridge two
/// existing runs together -- over owned `Vec`s, the natural shape for this
/// crate's `LineString<i32>`. A candidate that finds a left- AND a
/// right-neighbour that turn out to be the SAME run (a closing loop) takes
/// the same one-sided branch `merge_lines.ts`'s own `if`/`else if` chain
/// does rather than a hand-rolled "more correct" resolution -- real road
/// data does not close on itself, and matching the reference's own
/// edge-case behaviour beats inventing a different one it was never tested
/// against.
pub fn merge_line_candidates<T>(candidates: Vec<LineMergeCandidate<T>>) -> Vec<LineMergeCandidate<T>> {
    fn key(text: &str, p: Coord<i32>) -> (String, i32, i32) {
        (text.to_string(), p.x, p.y)
    }

    // Slot `i` becomes `None` once its content has been folded into
    // another slot -- `flatten()` at the end drops those.
    let mut merged: Vec<Option<LineMergeCandidate<T>>> = Vec::with_capacity(candidates.len());
    // Maps a live run's LEFT/RIGHT endpoint key to its slot in `merged`.
    let mut left_index: std::collections::HashMap<(String, i32, i32), usize> = std::collections::HashMap::new();
    let mut right_index: std::collections::HashMap<(String, i32, i32), usize> = std::collections::HashMap::new();

    for candidate in candidates {
        // No text, or too short to have two distinct ends: never merged.
        // Degenerate parts are passed through rather than dropped here --
        // the anchor walk drops them on its own (see `part_anchors_across`'s
        // `MeasuredPart` filter), which is the one place that already has
        // to make that call.
        if candidate.text.is_empty() || candidate.points.0.len() < 2 {
            merged.push(Some(candidate));
            continue;
        }
        let left_pt = candidate.points.0[0];
        let right_pt = *candidate.points.0.last().expect("length checked above");
        let left_key = key(&candidate.text, left_pt);
        let right_key = key(&candidate.text, right_pt);

        // An existing run ending exactly where `candidate` begins.
        let left_neighbor = right_index.get(&left_key).copied();
        // An existing run beginning exactly where `candidate` ends.
        let right_neighbor = left_index.get(&right_key).copied();

        if let (Some(li), Some(ri)) = (left_neighbor, right_neighbor) {
            if li != ri {
                // Bridges two existing runs end-to-end through `candidate`:
                // `li`'s run already ends where `candidate` begins, and
                // `ri`'s run already begins where `candidate` ends. All
                // three become one run kept at `li`'s slot; `ri`'s slot is
                // tombstoned.
                right_index.remove(&left_key);
                left_index.remove(&right_key);
                let mut left_run = merged[li].take().expect("index only ever names a live slot");
                let right_run = merged[ri].take().expect("index only ever names a live slot");
                let far_right_key = key(&candidate.text, *right_run.points.0.last().expect("merge only ever holds non-degenerate runs"));
                left_run.points.0.pop(); // drop the point shared with candidate's start
                left_run.points.0.extend(candidate.points.0.iter().copied());
                left_run.points.0.pop(); // drop the point shared with right_run's start
                left_run.points.0.extend(right_run.points.0);
                merged[li] = Some(left_run);
                merged[ri] = None;
                right_index.insert(far_right_key, li);
                continue;
            }
        }
        if let Some(li) = left_neighbor {
            // Extends an existing run on its RIGHT.
            right_index.remove(&left_key);
            let mut run = merged[li].take().expect("index only ever names a live slot");
            run.points.0.pop();
            run.points.0.extend(candidate.points.0.iter().copied());
            right_index.insert(right_key, li);
            merged[li] = Some(run);
        } else if let Some(ri) = right_neighbor {
            // Extends an existing run on its LEFT.
            left_index.remove(&right_key);
            let mut run = merged[ri].take().expect("index only ever names a live slot");
            let mut points = candidate.points.0.clone();
            points.pop();
            points.extend(run.points.0);
            run.points = LineString::new(points);
            left_index.insert(left_key, ri);
            merged[ri] = Some(run);
        } else {
            // No adjacent run: a new item.
            let idx = merged.len();
            left_index.insert(left_key, idx);
            right_index.insert(right_key, idx);
            merged.push(Some(candidate));
        }
    }

    merged.into_iter().flatten().collect()
}

/// MapLibre's `anchorIsTooClose` (`src/symbol/symbol_layout.ts`,
/// maplibre-gl-js, BSD-3-Clause): refuses a new line-placed anchor that
/// falls within `repeat_distance` of an anchor ALREADY accepted for the
/// same text (there, `textRepeatDistance = symbolMinDistance / 2`, i.e.
/// half the layer's `symbol-spacing`).
///
/// This is the second thing this module's own former doc comment named as
/// absent. It is what a dual carriageway needs: its two carriageways are
/// separate, PARALLEL `LineString` features carrying the same road name,
/// which never touch an endpoint and so [`merge_line_candidates`] cannot
/// join them -- but their independently-spaced anchors land close together
/// across the median, and this refuses the second one before it becomes a
/// second on-screen "Beach Road".
///
/// `accepted` holds every anchor already taken for this SAME text (the
/// caller keys its own map by text; this function does not), so the cost
/// is linear in how many times one name has already been placed -- the
/// same shape as MapLibre's own `compareText[text]` array.
pub fn anchor_too_close(accepted: &[Point<i32>], anchor: Point<i32>, repeat_distance: f64) -> bool {
    let threshold_sq = repeat_distance * repeat_distance;
    accepted.iter().any(|other| {
        let dx = (anchor.x() - other.x()) as f64;
        let dy = (anchor.y() - other.y()) as f64;
        dx * dx + dy * dy < threshold_sq
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo_types::{LineString, Point};

    fn line(points: &[(i32, i32)]) -> LineString<i32> {
        LineString::from(points.iter().map(|&(x, y)| (x, y)).collect::<Vec<_>>())
    }

    #[test]
    fn places_anchors_every_spacing_starting_at_half_a_spacing() {
        let anchors = line_anchors(&line(&[(0, 0), (4000, 0)]), 1000.0);
        let xs: Vec<i32> = anchors.iter().map(|a| a.at.x()).collect();
        assert_eq!(xs, vec![500, 1500, 2500, 3500]);
        assert!(anchors.iter().all(|a| a.at.y() == 0));
    }

    #[test]
    fn a_line_shorter_than_one_spacing_gets_a_single_midpoint_anchor() {
        // Roads are cut at tile boundaries, so short residual pieces are
        // common; dropping them would leave gaps in the arrow sequence.
        let anchors = line_anchors(&line(&[(0, 0), (300, 0)]), 1000.0);
        assert_eq!(anchors.len(), 1);
        assert_eq!(anchors[0].at.x(), 150);
    }

    #[test]
    fn forward_points_along_the_geometrys_own_direction() {
        let anchors = line_anchors(&line(&[(4000, 0), (0, 0)]), 1000.0);
        assert!(!anchors.is_empty());
        for anchor in &anchors {
            assert!(
                anchor.forward.x() < anchor.at.x(),
                "a line running in -x must have forward references in -x"
            );
        }
    }

    #[test]
    fn forward_is_never_equal_to_the_anchor() {
        // An anchor landing exactly on a vertex must take the NEXT vertex,
        // or atan2 of a zero-length difference gives a meaningless angle.
        let anchors = line_anchors(&line(&[(0, 0), (1000, 0), (2000, 0)]), 2000.0);
        assert_eq!(anchors.len(), 1, "one spacing fits once in a 2000-unit line");
        assert_eq!(anchors[0].at, Point::new(1000, 0), "midpoint lands on the middle vertex");
        assert_ne!(anchors[0].forward, anchors[0].at);
    }

    #[test]
    fn segment_start_and_segment_end_are_the_segments_raw_unrounded_endpoints() {
        // (0,0) -> (10,3): distance 5.0 interpolates to a non-integer point
        // that `anchor_at_distance` rounds for `at`, but `segment_start`/
        // `segment_end` must still name the segment's own vertices exactly,
        // not a rounded approximation of them.
        let points = vec![Point::new(0, 0), Point::new(10, 3)];
        let (lengths, _total) = segment_lengths(&points);
        let anchor = anchor_at_distance(&points, &lengths, 5.0).expect("distance is inside the segment");
        assert_eq!(anchor.at, Point::new(5, 1), "the rounded interpolated position");
        assert_eq!(anchor.segment_start, Point::new(0, 0));
        assert_eq!(anchor.segment_end, Point::new(10, 3));
    }

    #[test]
    fn every_glyph_in_the_same_segment_gets_the_identical_segment_endpoints() {
        // A single long, dead-straight two-point segment: every glyph's own
        // `at` advances along it, but `segment_start`/`segment_end` -- what
        // the DRAW ANGLE is derived from (see `symbol_pass::line_glyph_angle`)
        // -- must be the SAME pair for every one of them. That sameness is
        // the whole fix: a rotation derived from a constant pair cannot
        // wobble from one glyph to the next.
        let line = line(&[(0, 0), (10_000, 0)]);
        let advances = vec![50.0; 20];
        let placements = place_glyphs_along(&line, 5000.0, &advances, MAX_GLYPH_ANGLE_RAD)
            .expect("a straight, generously long line accepts 20 glyphs of 50 units each");
        assert_eq!(placements.len(), 20);
        for p in &placements {
            assert_eq!(p.segment_start, Point::new(0, 0));
            assert_eq!(p.segment_end, Point::new(10_000, 0));
        }
    }

    #[test]
    fn heading_uses_the_segments_raw_endpoints_not_the_rounded_interpolated_point() {
        // The same (0,0) -> (10,3) segment as above. The TRUE segment
        // heading is atan2(3, 10); the heading a caller would get by
        // (incorrectly) measuring from the ROUNDED `at` to `forward`
        // instead is measurably different -- exactly the rounding noise
        // that produced visibly bent glyph baselines on a straight road.
        // `heading` must report the former.
        let points = vec![Point::new(0, 0), Point::new(10, 3)];
        let (lengths, _total) = segment_lengths(&points);
        let anchor = anchor_at_distance(&points, &lengths, 5.0).expect("distance is inside the segment");
        let true_heading = heading(&anchor);
        let expected = 3.0_f64.atan2(10.0);
        assert!((true_heading - expected).abs() < 1e-9, "heading must be the segment's own exact bearing");
        let naive_heading = ((anchor.forward.y() - anchor.at.y()) as f64)
            .atan2((anchor.forward.x() - anchor.at.x()) as f64);
        assert!(
            (true_heading - naive_heading).abs() > 0.05,
            "rounding must move the naive at-to-forward heading measurably off the true segment \
             heading, or this test does not exercise the bug this fix closes"
        );
    }

    #[test]
    fn follows_a_bend_so_the_forward_reference_turns_with_the_line() {
        let anchors = line_anchors(&line(&[(0, 0), (1000, 0), (1000, 1000)]), 1500.0);
        assert_eq!(anchors.len(), 1);
        // Half of 1500 is 750, still on the first (horizontal) leg.
        assert_eq!(anchors[0].at, Point::new(750, 0));
        assert_eq!(anchors[0].forward, Point::new(1000, 0), "the leg's own end vertex");
    }

    #[test]
    fn a_forward_reference_that_would_run_off_the_end_of_the_line_extrapolates_instead_of_dropping_the_anchor()
    {
        // spacing = 20.2 makes the last walked distance 999.9 (10.1 +
        // 49*20.2), which rounds exactly onto the line's own final vertex
        // (1000, 0). There is no later vertex to fall back on, so `forward`
        // must be synthesized rather than the whole anchor silently dropped.
        let anchors = line_anchors(&line(&[(0, 0), (1000, 0)]), 20.2);
        let last = anchors
            .last()
            .expect("an anchor landing on the line's own end must not be dropped");
        assert_eq!(last.at, Point::new(1000, 0));
        assert_ne!(last.forward, last.at);
        assert!(
            last.forward.x() > last.at.x(),
            "forward must extend the line's own +x direction, not point backwards or vanish"
        );
    }

    #[test]
    fn a_short_lines_midpoint_landing_on_its_final_vertex_still_gets_an_anchor() {
        // The midpoint of a 1-unit line rounds exactly onto its own end
        // vertex, with no later vertex to fall back on either.
        let anchors = line_anchors(&line(&[(0, 0), (1, 0)]), 1000.0);
        assert_eq!(anchors.len(), 1, "a short line always gets exactly one anchor");
        assert_eq!(anchors[0].at, Point::new(1, 0));
        assert_ne!(anchors[0].forward, anchors[0].at);
        assert!(anchors[0].forward.x() > anchors[0].at.x());
    }

    #[test]
    fn degenerate_lines_yield_no_anchors_and_do_not_panic() {
        assert!(line_anchors(&line(&[]), 1000.0).is_empty());
        assert!(line_anchors(&line(&[(5, 5)]), 1000.0).is_empty());
        assert!(line_anchors(&line(&[(5, 5), (5, 5), (5, 5)]), 1000.0).is_empty());
    }

    /// The defect this walk exists for: a road split into many parts, each
    /// shorter than one spacing. Per-part walking gave every part the
    /// short-line midpoint anchor, so `symbol-spacing` thinned nothing.
    #[test]
    fn a_multipart_feature_is_spaced_along_its_whole_length_not_once_per_part() {
        // Twenty 100-unit parts laid end to end: 2000 units of road, in
        // pieces every one of which is a twentieth of the spacing.
        let parts: Vec<LineString<i32>> =
            (0..20).map(|i| line(&[(i * 100, 0), (i * 100 + 100, 0)])).collect();
        let anchors = line_anchors_across(&parts, 1000.0);

        let xs: Vec<i32> = anchors.iter().map(|a| a.at.x()).collect();
        assert_eq!(xs, vec![500, 1500], "2000 units at a 1000 spacing is two labels, not twenty");

        // And the per-part walk it replaces is exactly the bug: one each.
        let per_part: usize = parts.iter().map(|p| line_anchors(p, 1000.0).len()).sum();
        assert_eq!(per_part, 20, "the old per-part walk produced one label per part");
    }

    /// The short end of the same rule: a feature whose TOTAL is under one
    /// spacing gets one anchor for the feature, not one per part.
    #[test]
    fn a_multipart_feature_shorter_than_one_spacing_gets_a_single_feature_midpoint_anchor() {
        let parts = vec![line(&[(0, 0), (100, 0)]), line(&[(100, 0), (200, 0)]), line(&[(200, 0), (300, 0)])];
        let anchors = line_anchors_across(&parts, 1000.0);
        assert_eq!(anchors.len(), 1);
        assert_eq!(anchors[0].at.x(), 150, "the midpoint of the 300-unit FEATURE, not of any one part");
    }

    /// The accumulator is a feature-wide distance, so an anchor falls inside
    /// whichever part it lands in -- including across a bend between parts.
    #[test]
    fn an_anchor_lands_inside_the_part_the_running_distance_reaches() {
        // Part A runs 0..600 along +x; part B runs 0..600 along +y from
        // (1000, 0) -- disjoint, as parts of one road either side of a
        // junction often are.
        let parts = vec![line(&[(0, 0), (600, 0)]), line(&[(1000, 0), (1000, 600)])];
        let anchors = line_anchors_across(&parts, 400.0);
        // Distances 200, 600, 1000 along a 1200-unit feature: 200 in part A,
        // 600 exactly at A's end so resolved at B's start, 1000 at 400 into B.
        let at: Vec<(i32, i32)> = anchors.iter().map(|a| (a.at.x(), a.at.y())).collect();
        assert_eq!(at, vec![(200, 0), (1000, 0), (1000, 400)]);
        // The second anchor sits at part B's start and must take B's own
        // direction, not part A's.
        assert!(anchors[1].forward.y() > anchors[1].at.y(), "forward follows the part the anchor is in");
    }

    /// Parts that carry no length are skipped without shifting the
    /// accumulator or costing an anchor.
    #[test]
    fn degenerate_parts_are_skipped_rather_than_consuming_a_spacing_step() {
        let parts = vec![
            line(&[(5, 5)]),
            line(&[(0, 0), (1000, 0)]),
            line(&[(7, 7), (7, 7)]),
            line(&[(1000, 0), (2000, 0)]),
        ];
        let anchors = line_anchors_across(&parts, 1000.0);
        let xs: Vec<i32> = anchors.iter().map(|a| a.at.x()).collect();
        assert_eq!(xs, vec![500, 1500], "the two real parts measure 2000 units between them");

        assert!(line_anchors_across(&[], 1000.0).is_empty());
        assert!(line_anchors_across(&[line(&[(5, 5)])], 1000.0).is_empty());
    }

    /// A single-part feature must come out exactly as it did before the
    /// accumulator existed -- that is what keeps existing renders still.
    #[test]
    fn a_single_part_feature_walks_identically_to_the_one_line_walk() {
        for spacing in [37.0, 250.0, 1000.0, 5000.0] {
            let one = line(&[(0, 0), (1000, 0), (1000, 1000), (0, 1000)]);
            assert_eq!(
                line_anchors(&one, spacing),
                line_anchors_across(std::slice::from_ref(&one), spacing),
                "spacing {spacing}"
            );
        }
    }

    #[test]
    fn a_non_positive_spacing_yields_no_anchors_rather_than_looping_forever() {
        assert!(line_anchors(&line(&[(0, 0), (4000, 0)]), 0.0).is_empty());
        assert!(line_anchors(&line(&[(0, 0), (4000, 0)]), -10.0).is_empty());
    }

    #[test]
    fn tile_units_per_px_at_native_zoom_is_extent_over_tile_size() {
        // 4096 MVT units over a 512-point tile.
        assert_eq!(tile_units_per_px(16, 16), 8.0);
        // 250 logical px of spacing is therefore 2000 tile units: two anchors
        // across a tile, the right order for a z16 road.
        assert_eq!(250.0 * tile_units_per_px(16, 16), 2000.0);
    }

    /// A far LOD tile is laid out at its own zoom, a tile at the cover's
    /// zoom at the bake zoom (the overzoom case unchanged).
    #[test]
    fn a_far_lod_tile_is_spaced_at_its_own_zoom() {
        assert_eq!(symbol_spacing_zoom(16, 16, 11), 11);
        assert_eq!(tile_units_per_px(symbol_spacing_zoom(16, 16, 11), 11), 8.0, "native scale, not 32x denser");
        assert_eq!(symbol_spacing_zoom(16, 16, 16), 16);
        assert_eq!(symbol_spacing_zoom(16, 14, 14), 16, "overzoomed at the ceiling, as before");
        assert_eq!(symbol_spacing_zoom(15, 14, 15), 15, "a next-zoom prefetch tile");
    }

    #[test]
    fn tile_units_per_px_shrinks_as_a_tile_is_overzoomed() {
        // A z14 tile displayed at zoom 16 covers 2048 px, so one pixel is
        // fewer tile units and the same tile carries more anchors.
        assert_eq!(tile_units_per_px(16, 14), 2.0);
        assert_eq!(250.0 * tile_units_per_px(16, 14), 500.0);
    }

    #[test]
    fn spacing_baked_at_an_integer_zoom_drifts_at_most_two_fold_within_the_bucket() {
        // The tradeoff this design accepts, pinned rather than implied: spacing
        // is computed at the bake zoom and used until the next crossing, so by
        // the end of the bucket the same tile-unit spacing spans up to twice
        // the intended pixels. Corrected on the crossing's rebake.
        let baked = 250.0 * tile_units_per_px(16, 16);
        let exact_at_bucket_end = 250.0 * 8.0 / 2.0f64.powf(0.99);
        let drift = baked / exact_at_bucket_end;
        assert!(drift > 1.98 && drift <= 2.0, "drift was {drift}");
    }
    /// Along a straight line, glyphs come out evenly spaced, in order, all
    /// pointing the same way -- and the run is CENTRED on the anchor.
    #[test]
    fn glyphs_along_a_straight_line_are_centred_evenly_spaced_and_parallel() {
        let line = line(&[(0, 0), (1000, 0)]);
        let advances = [10.0, 20.0, 10.0]; // 40 wide, centred on 500 -> 480..520
        let placed = place_glyphs_along(&line, 500.0, &advances, std::f64::consts::PI)
            .expect("a 40-unit run fits easily in a 1000-unit line");

        assert_eq!(placed.len(), 3);
        // Centres at 485, 500, 515.
        assert_eq!(placed[0].at, (485.0, 0.0));
        assert_eq!(placed[1].at, (500.0, 0.0));
        assert_eq!(placed[2].at, (515.0, 0.0));
        for p in &placed {
            assert!(glyph_heading(p).abs() < 1e-12, "every glyph runs along +x");
        }
    }

    /// A glyph position is NOT snapped to a whole tile unit: at pitch 0 each
    /// glyph is projected from it, and one tile unit is 12 physical px at
    /// z19 on a z14 tile on a 3x phone (see [`GlyphAlong`]). Fractional
    /// advances on a slanted line land at their exact fractional points.
    #[test]
    fn glyph_positions_are_not_rounded_to_a_tile_unit() {
        let line = line(&[(0, 0), (1000, 300)]);
        let length = line_length(&line);
        let (ux, uy) = (1000.0 / length, 300.0 / length);
        let advances = [1.3, 0.7, 2.1, 0.9];
        let centre = 400.37;
        let placed = place_glyphs_along(&line, centre, &advances, MAX_GLYPH_ANGLE_RAD).expect("fits");
        let mut cursor = centre - advances.iter().sum::<f64>() / 2.0;
        for (glyph, advance) in placed.iter().zip(advances) {
            let d = cursor + advance / 2.0;
            let expected = (d * ux, d * uy);
            assert!(
                (glyph.at.0 - expected.0).abs() < 1e-9 && (glyph.at.1 - expected.1).abs() < 1e-9,
                "{:?} vs {expected:?}",
                glyph.at
            );
            cursor += advance;
        }
    }

    /// The whole point: on a bend, each glyph carries the direction of the
    /// line UNDER IT, not one shared direction for the label.
    #[test]
    fn glyphs_around_a_bend_each_take_their_own_direction() {
        // Right along x, then up along y -- a right-angle corner at (500, 0).
        let line = line(&[(0, 0), (500, 0), (500, 500)]);
        let advances = [40.0; 6]; // 240 wide, centred on the corner
        let placed = place_glyphs_along(&line, 500.0, &advances, std::f64::consts::PI)
            .expect("a generous max angle accepts the corner");

        let first = glyph_heading(&placed[0]);
        let last = glyph_heading(&placed[placed.len() - 1]);
        assert!(first.abs() < 0.01, "the first glyph runs along +x, got {first}");
        assert!(
            (last - std::f64::consts::FRAC_PI_2).abs() < 0.01,
            "the last glyph runs along +y, got {last}"
        );
    }

    /// That same corner is rejected when the caller will not tolerate the
    /// turn -- text around a hairpin overlaps itself.
    #[test]
    fn a_turn_sharper_than_the_limit_is_refused() {
        let line = line(&[(0, 0), (500, 0), (500, 500)]);
        let advances = [40.0; 6];
        assert!(
            place_glyphs_along(&line, 500.0, &advances, 0.2).is_none(),
            "a 90-degree corner must not be accepted under a 0.2rad limit"
        );
    }

    /// A label longer than the line it is placed on is refused rather than
    /// running off the end, where the missing glyphs would be silent.
    #[test]
    fn a_run_longer_than_the_line_is_refused() {
        let line = line(&[(0, 0), (100, 0)]);
        assert!(place_glyphs_along(&line, 50.0, &[40.0, 40.0, 40.0], std::f64::consts::PI).is_none());
    }

    /// Same run, same line, placed too near an end to fit.
    #[test]
    fn a_run_that_would_overhang_an_end_is_refused() {
        let line = line(&[(0, 0), (1000, 0)]);
        let advances = [40.0; 4]; // 160 wide, so a centre at 50 starts at -30
        assert!(place_glyphs_along(&line, 50.0, &advances, std::f64::consts::PI).is_none());
        assert!(place_glyphs_along(&line, 500.0, &advances, std::f64::consts::PI).is_some());
    }

    /// The arithmetic a right-to-left label's flip depends on: the SAME
    /// run, placed against the reversed line at `line_length -
    /// centre_distance`, lands on the same points in the opposite order
    /// and with every `forward` turned around.
    ///
    /// This is what `symbol_pass::line_glyph_quads` does when a run would
    /// read backwards on screen, so it is worth pinning as geometry rather
    /// than discovering as a rendering artifact.
    #[test]
    fn a_run_placed_against_the_reversed_line_is_the_same_run_backwards() {
        let forward = line(&[(0, 0), (600, 0), (600, 600)]);
        let reversed = line(&[(600, 600), (600, 0), (0, 0)]);
        let total = line_length(&forward);
        assert_eq!(total, 1200.0);
        assert_eq!(total, line_length(&reversed), "reversing cannot change a length");

        let advances = [40.0; 5];
        let centre = 500.0;
        let a = place_glyphs_along(&forward, centre, &advances, MAX_GLYPH_ANGLE_RAD)
            .expect("a 200-unit run fits at 500 along a 1200-unit line");
        let b = place_glyphs_along(&reversed, total - centre, &advances, MAX_GLYPH_ANGLE_RAD)
            .expect("and fits just as well addressed from the other end");
        assert_eq!(a.len(), b.len());
        for (i, (fwd, rev)) in a.iter().zip(b.iter().rev()).enumerate() {
            // Unrounded, so the two walks meet to float noise, not to a
            // tile unit: they reach the same point by different partial sums.
            assert!(
                (fwd.at.0 - rev.at.0).abs() < 1e-9 && (fwd.at.1 - rev.at.1).abs() < 1e-9,
                "glyph {i}: {:?} vs {:?}",
                fwd.at,
                rev.at
            );
        }
        // And the directions are opposed, which is what turns the glyphs
        // the right way up rather than merely reordering them.
        let turn = glyph_heading(&b[0]) - glyph_heading(&a[a.len() - 1]);
        assert!(
            (turn.abs() - std::f64::consts::PI).abs() < 0.01,
            "the reversed walk must run the other way, got a {turn} rad turn"
        );
    }

    #[test]
    fn the_max_glyph_angle_is_maplibres_forty_five_degrees() {
        assert!((MAX_GLYPH_ANGLE_RAD - std::f64::consts::FRAC_PI_4).abs() < 1e-12);
    }

    /// Degenerate inputs are refusals, not panics.
    #[test]
    fn degenerate_glyph_runs_are_refused() {
        let straight = line(&[(0, 0), (1000, 0)]);
        assert!(place_glyphs_along(&straight, 500.0, &[], std::f64::consts::PI).is_none());
        assert!(place_glyphs_along(&straight, 500.0, &[0.0, 0.0], std::f64::consts::PI).is_none());
        assert!(place_glyphs_along(&straight, 500.0, &[f64::NAN], std::f64::consts::PI).is_none());
        let degenerate = line(&[(7, 7), (7, 7)]);
        assert!(place_glyphs_along(&degenerate, 0.0, &[10.0], std::f64::consts::PI).is_none());
    }

    fn merge_candidate(text: &str, points: &[(i32, i32)]) -> LineMergeCandidate<()> {
        LineMergeCandidate { text: text.to_string(), points: line(points), payload: () }
    }

    /// Two same-named blocks that share an endpoint -- exactly the
    /// "one OSM way per city block" shape a named road actually has in a
    /// vector tile -- become one polyline carrying every point of both,
    /// with the shared point appearing once.
    #[test]
    fn two_touching_same_text_lines_merge_into_one() {
        let candidates = vec![
            merge_candidate("Beach Road", &[(0, 0), (100, 0)]),
            merge_candidate("Beach Road", &[(100, 0), (250, 0)]),
        ];
        let merged = merge_line_candidates(candidates);
        assert_eq!(merged.len(), 1, "two touching blocks of the same road become one line");
        assert_eq!(merged[0].points.0.len(), 3, "the shared joint is not duplicated");
        assert_eq!(merged[0].points.0.last().unwrap().x, 250);
    }

    /// A third block, added after the first two are already merged, still
    /// finds and extends the combined run -- merging is not limited to
    /// pairs.
    #[test]
    fn a_chain_of_three_touching_blocks_merges_into_one_line() {
        let candidates = vec![
            merge_candidate("Beach Road", &[(0, 0), (100, 0)]),
            merge_candidate("Beach Road", &[(100, 0), (250, 0)]),
            merge_candidate("Beach Road", &[(250, 0), (400, 0)]),
        ];
        let merged = merge_line_candidates(candidates);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].points.0.len(), 4);
        assert_eq!(merged[0].points.0.last().unwrap().x, 400);
    }

    /// A block dropped in AFTER both its neighbours are already on the map
    /// bridges them into one run, rather than joining only one side.
    #[test]
    fn a_middle_block_bridges_two_already_known_runs() {
        let candidates = vec![
            merge_candidate("Beach Road", &[(0, 0), (100, 0)]),
            merge_candidate("Beach Road", &[(250, 0), (400, 0)]),
            merge_candidate("Beach Road", &[(100, 0), (250, 0)]),
        ];
        let merged = merge_line_candidates(candidates);
        assert_eq!(merged.len(), 1, "the middle block bridges the two runs already on the map");
        let points = &merged[0].points.0;
        assert_eq!(points.first().unwrap().x, 0);
        assert_eq!(points.last().unwrap().x, 400);
    }

    /// Two carriageways of a dual carriageway -- same name, running
    /// parallel, never touching an endpoint -- are NOT merged: merging
    /// cannot be what keeps them from double-labelling (see
    /// `anchor_too_close` for the mechanism that actually does).
    #[test]
    fn parallel_non_touching_lines_are_not_merged_even_with_the_same_text() {
        let candidates = vec![
            merge_candidate("Beach Road", &[(0, 0), (200, 0)]),
            merge_candidate("Beach Road", &[(0, 20), (200, 20)]),
        ];
        let merged = merge_line_candidates(candidates);
        assert_eq!(merged.len(), 2, "parallel carriageways share no endpoint, so they stay separate features");
    }

    /// Touching lines with DIFFERENT text never merge, matching
    /// `merge_lines.ts`'s own text-keyed index.
    #[test]
    fn touching_lines_with_different_text_do_not_merge() {
        let candidates = vec![merge_candidate("Beach Road", &[(0, 0), (100, 0)]), merge_candidate("Middle Road", &[(100, 0), (200, 0)])];
        let merged = merge_line_candidates(candidates);
        assert_eq!(merged.len(), 2);
    }

    /// An icon-only candidate (no text -- a one-way arrow) is never a merge
    /// participant, matching `mergeLines`'s own `if (!text) continue`.
    #[test]
    fn text_less_candidates_are_never_merged() {
        let candidates = vec![merge_candidate("", &[(0, 0), (100, 0)]), merge_candidate("", &[(100, 0), (200, 0)])];
        let merged = merge_line_candidates(candidates);
        assert_eq!(merged.len(), 2, "arrows repeat along their own feature and are not merge participants");
    }

    /// `anchor_too_close` is the dual-carriageway backstop: a second anchor
    /// for the SAME text within `repeat_distance` of one already accepted
    /// is refused, and a farther one, or one under a different text, is
    /// not.
    #[test]
    fn anchor_too_close_refuses_only_a_near_same_text_repeat() {
        let accepted = [Point::new(100, 0)];
        assert!(anchor_too_close(&accepted, Point::new(105, 30), 50.0), "well within 50 tile units");
        assert!(!anchor_too_close(&accepted, Point::new(400, 0), 50.0), "far beyond 50 tile units");
        assert!(!anchor_too_close(&[], Point::new(100, 0), 50.0), "nothing accepted yet -- nothing to be close to");
    }

    // ---- The SCREEN walk (`place_glyphs_along_screen`) ----

    /// A projected stretch built by hand, every segment its own tile segment.
    fn projected(points: &[(f64, f64)], anchor_index: usize) -> ProjectedLine {
        ProjectedLine {
            points: points.to_vec(),
            segments: (0..points.len() - 1).collect(),
            anchor_index,
        }
    }

    fn gaps(glyphs: &[ScreenGlyph]) -> Vec<f32> {
        glyphs
            .windows(2)
            .map(|pair| (pair[1].centre.0 - pair[0].centre.0).hypot(pair[1].centre.1 - pair[0].centre.1))
            .collect()
    }

    /// Centred on the anchor, one mean advance apart, facing along the line
    /// -- including across a vertex that bends the line gently.
    #[test]
    fn screen_glyphs_are_centred_on_the_anchor_and_spaced_by_their_advances() {
        let tile = line(&[(0, 0), (1000, 0), (2000, 100)]);
        let screen = ProjectedLine {
            points: vec![(0.0, 0.0), (100.0, 0.0), (200.0, 0.0), (300.0, 10.0)],
            segments: vec![0, 0, 1],
            anchor_index: 1,
        };
        let advances = [10.0, 20.0, 10.0, 30.0];
        let placed = place_glyphs_along_screen(&screen, &tile, &advances, MAX_GLYPH_ANGLE_RAD).expect("fits");
        assert_eq!(placed.len(), 4);
        // Run 70 wide, centred on x = 100: 65..135, centres at 70, 85, 100, 120.
        let xs: Vec<f32> = placed.iter().map(|g| g.centre.0).collect();
        assert_eq!(xs, vec![70.0, 85.0, 100.0, 120.0]);
        assert_eq!(gaps(&placed), vec![15.0, 15.0, 20.0]);
        assert!(placed.iter().all(|g| g.angle == 0.0 && g.centre.1 == 0.0));
    }

    /// Past the anchor's own vertex the glyph takes the NEXT segment's
    /// angle, and the distance is measured along the bent path.
    #[test]
    fn a_screen_glyph_past_a_vertex_takes_the_next_segments_angle() {
        let tile = line(&[(0, 0), (1000, 0), (1700, 700)]);
        let screen = projected(&[(0.0, 0.0), (100.0, 0.0), (130.0, 30.0)], 1);
        let placed = place_glyphs_along_screen(&screen, &tile, &[20.0, 20.0, 20.0], MAX_GLYPH_ANGLE_RAD)
            .expect("a 45-degree tile turn is exactly the limit");
        assert_eq!(placed[1].centre, (100.0, 0.0));
        assert!((placed[2].angle - std::f32::consts::FRAC_PI_4).abs() < 1e-6, "{:?}", placed[2]);
        let along = (placed[2].centre.0 - 100.0).hypot(placed[2].centre.1);
        assert!((along - 20.0).abs() < 1e-4, "20 px along the second segment, got {along}");
    }

    /// Too long for the stretch at either end, a bend beyond the limit in
    /// TILE space, and nonsense inputs: all refusals.
    #[test]
    fn a_screen_run_that_does_not_fit_or_bends_too_hard_is_refused() {
        let tile = line(&[(0, 0), (1000, 0), (1000, 1000)]);
        let screen = projected(&[(0.0, 0.0), (50.0, 0.0), (50.0, 50.0)], 1);
        let any_turn = std::f64::consts::PI;
        assert!(place_glyphs_along_screen(&screen, &tile, &[10.0; 12], any_turn).is_none(), "120 px on 100 px");
        assert!(place_glyphs_along_screen(&screen, &tile, &[10.0; 6], any_turn).is_some());
        let bent = place_glyphs_along_screen(&screen, &tile, &[10.0; 6], MAX_GLYPH_ANGLE_RAD);
        assert!(bent.is_none(), "a 90-degree tile turn");
        assert!(place_glyphs_along_screen(&screen, &tile, &[], MAX_GLYPH_ANGLE_RAD).is_none());
        assert!(place_glyphs_along_screen(&screen, &tile, &[f64::NAN], MAX_GLYPH_ANGLE_RAD).is_none());
    }

    /// Keep-upright: a stretch digitised right to left is re-walked from its
    /// other end, so the first glyph lands left of the last and every glyph
    /// faces +x.
    #[test]
    fn a_screen_run_that_reads_backwards_is_rewalked_reversed() {
        let tile = line(&[(1000, 0), (0, 0)]);
        let screen = ProjectedLine {
            points: vec![(200.0, 0.0), (100.0, 0.0), (0.0, 0.0)],
            segments: vec![0, 0],
            anchor_index: 1,
        };
        let backwards = place_glyphs_along_screen(&screen, &tile, &[10.0; 4], MAX_GLYPH_ANGLE_RAD).expect("fits");
        assert!(backwards[0].centre.0 > backwards[3].centre.0, "the unflipped walk reads right to left");
        let upright = place_glyphs_along_screen_upright(&screen, &tile, &[10.0; 4], MAX_GLYPH_ANGLE_RAD).expect("fits");
        assert!(upright[0].centre.0 < upright[3].centre.0);
        assert!(upright.iter().all(|g| g.angle.abs() < 1e-6), "{upright:?}");
    }

    /// A vertex behind the near plane (`project` says `None`): the stretch
    /// ends on a vertex synthesised along the direction the segment leaves
    /// the screen, far enough for the run, and the glyphs keep their
    /// spacing along it. If even the probe fails, nothing is built.
    #[test]
    fn a_vertex_behind_the_near_plane_is_synthesised_along_the_segments_screen_direction() {
        // Tile x maps to screen x/10 with y = x/10 too (a 45-degree screen
        // line), and everything left of x = 400 is "behind the eye".
        let project = |(x, y): (f64, f64)| (x >= 400.0).then_some((x / 10.0, x / 10.0 + y));
        let tile = line(&[(0, 0), (1000, 0), (3000, 0)]);
        let anchor_tile = (500.0, 0.0);
        let built = project_line_around(&tile, 0, anchor_tile, (50.0, 50.0), 40.0, project).expect("builds");
        let anchor = built.points[built.anchor_index];
        assert_eq!(anchor, (50.0, 50.0));
        let synthesised = built.points[0];
        let reach = (synthesised.0 - anchor.0).hypot(synthesised.1 - anchor.1);
        assert!((40.0..=41.0 + 1e-9).contains(&reach), "reaches the run and one slack pixel: {reach}");
        assert!((synthesised.1 - synthesised.0).abs() < 1e-9, "along the screen line x = y: {synthesised:?}");
        let placed = place_glyphs_along_screen(&built, &tile, &[10.0; 8], MAX_GLYPH_ANGLE_RAD).expect("fits");
        assert!(gaps(&placed).iter().all(|gap| (gap - 10.0).abs() < 1e-3), "{:?}", gaps(&placed));

        let nothing_projects = |_: (f64, f64)| None;
        assert!(project_line_around(&tile, 0, anchor_tile, (50.0, 50.0), 40.0, nothing_projects).is_none());
    }

    /// Only the vertices a run of this length can reach are projected.
    #[test]
    fn the_projected_stretch_stops_once_it_covers_the_reach() {
        let tile = line(&[(0, 0), (100, 0), (200, 0), (300, 0), (400, 0), (500, 0), (600, 0)]);
        let mut calls = 0;
        let built = project_line_around(&tile, 3, (350.0, 0.0), (350.0, 0.0), 120.0, |p| {
            calls += 1;
            Some(p)
        })
        .expect("builds");
        assert_eq!(built.points, vec![(200.0, 0.0), (300.0, 0.0), (350.0, 0.0), (400.0, 0.0), (500.0, 0.0)]);
        assert_eq!(built.segments, vec![2, 3, 3, 4]);
        assert_eq!(calls, 4);
    }

    #[test]
    fn point_at_distance_is_unrounded_and_names_its_segment() {
        let tile = line(&[(0, 0), (3, 0), (3, 3)]);
        assert_eq!(point_at_distance(&tile, 1.5), Some((0, (1.5, 0.0))));
        assert_eq!(point_at_distance(&tile, 4.25), Some((1, (3.0, 1.25))));
        assert_eq!(point_at_distance(&tile, 7.0), None);
        assert_eq!(point_at_distance(&tile, -1.0), None);
        assert_eq!(point_at_distance(&tile, f64::NAN), None);
    }
}

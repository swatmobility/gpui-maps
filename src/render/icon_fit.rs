//! Growing an icon around its label -- Mapbox's `icon-text-fit`, and the
//! n-slice stretching that makes it possible.
//!
//! A badge with rounded corners and a tail cannot be scaled to fit its
//! text: uniform scaling distorts the corners and the tail along with
//! everything else. The sprite therefore names the bands that MAY stretch
//! (`stretchX`/`stretchY`) and the box its label sits in (`content`), and
//! this module turns "this label needs a box this big" into the patches
//! that draw such an icon.
//!
//! Everything here is pure geometry. `src` is in the sprite's own IMAGE
//! PIXELS, `dst` in LOGICAL POINTS, and the two are related by the
//! sprite's `pixelRatio` -- so a 35px pin at ratio 1.5 is 23.3pt before any
//! stretching.

/// One run along an axis, mapped from the sprite to the screen.
///
/// A fixed run has `dst_len == src_len / pixel_ratio`; a stretchable one
/// has whatever the fit asked for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Segment {
    pub src_start: f32,
    pub src_len: f32,
    pub dst_start: f32,
    pub dst_len: f32,
}

/// Splits one axis into fixed and stretchable runs, and gives the
/// stretchable ones whatever `target_pt` asks for beyond the natural size.
///
/// With no bands declared the icon has nothing that may stretch
/// selectively, so it scales uniformly -- one run covering the whole axis.
/// That is also what makes this safe to call for an ordinary icon.
///
/// Extra length is shared between bands in proportion to their own
/// lengths, so a pin with a wide band and a narrow one keeps their
/// relationship. A band never goes NEGATIVE: asking for less than the fixed
/// runs already occupy collapses the bands to nothing rather than
/// inverting them, which would draw the icon inside out.
pub(crate) fn axis_segments(
    natural_px: u32,
    bands: &[(u32, u32)],
    target_pt: f32,
    pixel_ratio: f32,
) -> Vec<Segment> {
    let ratio = if pixel_ratio.is_finite() && pixel_ratio > 0.0 { pixel_ratio } else { 1.0 };
    let natural_px = natural_px as f32;
    let natural_pt = natural_px / ratio;

    // Bands that are empty, inverted, or reach past the sprite tell us
    // nothing usable; treating them as absent scales uniformly, which is
    // wrong-but-recognisable rather than a panic on data fetched from a
    // network.
    let usable: Vec<(f32, f32)> = bands
        .iter()
        .filter(|(start, end)| end > start && (*end as f32) <= natural_px)
        .map(|(start, end)| (*start as f32, *end as f32))
        .collect();

    if usable.is_empty() {
        return vec![Segment {
            src_start: 0.0,
            src_len: natural_px,
            dst_start: 0.0,
            dst_len: target_pt.max(0.0),
        }];
    }

    let stretch_pt: f32 = usable.iter().map(|(start, end)| (end - start) / ratio).sum();
    let extra_pt = target_pt - natural_pt;

    let mut segments = Vec::with_capacity(usable.len() * 2 + 1);
    let mut cursor_px = 0.0f32;
    let mut cursor_pt = 0.0f32;
    let mut push = |src_start: f32, src_len: f32, dst_len: f32, cursor_pt: &mut f32| {
        if src_len <= 0.0 {
            return;
        }
        segments.push(Segment { src_start, src_len, dst_start: *cursor_pt, dst_len });
        *cursor_pt += dst_len;
    };

    for (start, end) in &usable {
        // The fixed run before this band, at its authored size.
        push(cursor_px, start - cursor_px, (start - cursor_px) / ratio, &mut cursor_pt);
        // The band itself, carrying its share of the difference.
        let band_pt = (end - start) / ratio;
        let share = if stretch_pt > 0.0 { band_pt / stretch_pt } else { 0.0 };
        push(*start, end - start, (band_pt + extra_pt * share).max(0.0), &mut cursor_pt);
        cursor_px = *end;
    }
    // The fixed run after the last band.
    push(cursor_px, natural_px - cursor_px, (natural_px - cursor_px) / ratio, &mut cursor_pt);

    segments
}

/// Where a point of the SPRITE lands on screen, given that axis's segments.
///
/// Used for the content box: the label goes where the box ends up AFTER
/// stretching, which is not where it started and not a fixed fraction of
/// the result either -- the stretch bands move it.
pub(crate) fn map_along(segments: &[Segment], src_pos: f32) -> f32 {
    let Some(first) = segments.first() else { return 0.0 };
    if src_pos <= first.src_start {
        return first.dst_start;
    }
    for segment in segments {
        let src_end = segment.src_start + segment.src_len;
        if src_pos <= src_end {
            let t = if segment.src_len > 0.0 { (src_pos - segment.src_start) / segment.src_len } else { 0.0 };
            return segment.dst_start + t * segment.dst_len;
        }
    }
    let last = segments.last().expect("non-empty, checked above");
    last.dst_start + last.dst_len
}

/// The size an icon must be drawn at for its content box to wrap a label of
/// `text_pt`, given `padding_pt` as `[left, top, right, bottom]`.
///
/// Grows only. Mapbox's `icon-text-fit` exists to stop a label overflowing
/// its badge; shrinking a badge below its authored size to hug a narrow
/// label would make a row of one-digit pins smaller than the sprite its
/// author drew.
///
/// With no content box there is nothing to fit to and the natural size is
/// returned unchanged -- the same answer as `icon-text-fit: none`.
pub(crate) fn fitted_size(
    natural_px: (u32, u32),
    pixel_ratio: f32,
    content_px: Option<[u32; 4]>,
    text_pt: (f32, f32),
    padding_pt: [f32; 4],
    fit_width: bool,
    fit_height: bool,
) -> (f32, f32) {
    let ratio = if pixel_ratio.is_finite() && pixel_ratio > 0.0 { pixel_ratio } else { 1.0 };
    let natural = (natural_px.0 as f32 / ratio, natural_px.1 as f32 / ratio);
    let Some(content) = content_px else { return natural };

    let content_w = (content[2] as f32 - content[0] as f32) / ratio;
    let content_h = (content[3] as f32 - content[1] as f32) / ratio;
    let needed_w = text_pt.0 + padding_pt[0] + padding_pt[2];
    let needed_h = text_pt.1 + padding_pt[1] + padding_pt[3];

    let width = if fit_width { natural.0 + (needed_w - content_w).max(0.0) } else { natural.0 };
    let height = if fit_height { natural.1 + (needed_h - content_h).max(0.0) } else { natural.1 };
    (width, height)
}

/// One piece of a stretched icon: which part of the sprite to sample, and
/// the rectangle to draw it in.
///
/// `src` is `(x, y, w, h)` in image pixels RELATIVE to the sprite's own
/// top-left; `dst` is `(x, y, w, h)` in logical points relative to the
/// icon's top-left.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Patch {
    pub src: (f32, f32, f32, f32),
    pub dst: (f32, f32, f32, f32),
}

/// Every patch an icon draws as, given both axes' segments.
///
/// The cross product, in row-major order. For an unstretched icon that is
/// exactly one patch covering the whole sprite, which is why the plain path
/// can go through here unchanged rather than needing a branch of its own.
pub(crate) fn patches(x: &[Segment], y: &[Segment]) -> Vec<Patch> {
    let mut out = Vec::with_capacity(x.len() * y.len());
    for row in y {
        for column in x {
            // A collapsed band contributes nothing but would still cost a
            // quad, six vertices and a fragment pass over zero area.
            if column.dst_len <= 0.0 || row.dst_len <= 0.0 {
                continue;
            }
            out.push(Patch {
                src: (column.src_start, row.src_start, column.src_len, row.src_len),
                dst: (column.dst_start, row.dst_start, column.dst_len, row.dst_len),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An icon with no declared bands scales uniformly, in ONE patch. This
    /// is the path every ordinary icon takes, so it has to come out
    /// identical to drawing the sprite directly.
    #[test]
    fn an_icon_with_no_bands_is_one_uniformly_scaled_patch() {
        let segments = axis_segments(32, &[], 16.0, 2.0);
        assert_eq!(
            segments,
            vec![Segment { src_start: 0.0, src_len: 32.0, dst_start: 0.0, dst_len: 16.0 }]
        );
        assert_eq!(patches(&segments, &segments).len(), 1);
    }

    /// At its natural size, a banded icon's patches reproduce the sprite
    /// exactly: every run keeps its authored length. If this drifts, an
    /// icon that asks for no growth still comes out subtly wrong.
    #[test]
    fn at_the_natural_size_every_run_keeps_its_authored_length() {
        // 35px at ratio 1.5 is 23.33pt.
        let natural_pt = 35.0 / 1.5;
        let segments = axis_segments(35, &[(11, 12), (22, 23)], natural_pt, 1.5);
        for segment in &segments {
            assert!(
                (segment.dst_len - segment.src_len / 1.5).abs() < 1e-4,
                "{segment:?} should keep its authored length"
            );
        }
        let total: f32 = segments.iter().map(|s| s.dst_len).sum();
        assert!((total - natural_pt).abs() < 1e-4, "total was {total}");
    }

    /// The extra width goes into the BANDS and nowhere else -- that is the
    /// whole point. A pin's rounded corners and tail are in the fixed runs,
    /// and stretching those is exactly the distortion this avoids.
    #[test]
    fn extra_length_goes_only_into_the_stretchable_bands() {
        let natural_pt = 35.0 / 1.5;
        let segments = axis_segments(35, &[(11, 12), (22, 23)], natural_pt + 9.0, 1.5);
        let mut stretched = 0.0;
        for segment in &segments {
            let authored = segment.src_len / 1.5;
            let is_band = segment.src_start == 11.0 || segment.src_start == 22.0;
            if is_band {
                stretched += segment.dst_len - authored;
            } else {
                assert!(
                    (segment.dst_len - authored).abs() < 1e-4,
                    "a fixed run must not stretch: {segment:?}"
                );
            }
        }
        assert!((stretched - 9.0).abs() < 1e-4, "the bands absorbed {stretched}, not 9");
    }

    /// Two bands of equal width share the growth equally.
    #[test]
    fn equal_bands_share_the_growth_equally() {
        let natural_pt = 35.0;
        let segments = axis_segments(35, &[(11, 12), (22, 23)], natural_pt + 10.0, 1.0);
        let bands: Vec<f32> = segments
            .iter()
            .filter(|s| s.src_start == 11.0 || s.src_start == 22.0)
            .map(|s| s.dst_len)
            .collect();
        assert_eq!(bands.len(), 2);
        assert!((bands[0] - bands[1]).abs() < 1e-4, "{bands:?}");
        assert!((bands[0] - 6.0).abs() < 1e-4, "1pt authored plus half of 10: {bands:?}");
    }

    /// Asking for less than the fixed runs already occupy collapses the
    /// bands instead of inverting them. A negative run would draw the icon
    /// inside out, sampling backwards across the sprite.
    #[test]
    fn an_impossible_target_collapses_the_bands_rather_than_inverting_them() {
        let segments = axis_segments(35, &[(11, 12), (22, 23)], 1.0, 1.0);
        for segment in &segments {
            assert!(segment.dst_len >= 0.0, "{segment:?}");
        }
        // ...and the collapsed bands are dropped rather than drawn empty.
        let full = axis_segments(35, &[], 35.0, 1.0);
        assert!(patches(&segments, &full).iter().all(|p| p.dst.2 > 0.0 && p.dst.3 > 0.0));
    }

    /// The content box has to be located AFTER stretching, because the
    /// bands move it. Placing the label from the natural box, or from a
    /// fixed fraction of the fitted icon, puts it off centre by however
    /// much the icon grew.
    #[test]
    fn the_content_box_is_located_after_the_stretch_not_before() {
        let natural_pt = 35.0;
        let grown = axis_segments(35, &[(11, 12), (22, 23)], natural_pt + 10.0, 1.0);
        // `content` runs 11..23, which brackets BOTH bands, so it must grow
        // by the whole 10.
        let left = map_along(&grown, 11.0);
        let right = map_along(&grown, 23.0);
        assert!((left - 11.0).abs() < 1e-4, "the box's left edge is before either band: {left}");
        assert!((right - 33.0).abs() < 1e-4, "12 authored plus 10 grown: {right}");
    }

    /// Positions outside the sprite clamp to its ends rather than
    /// extrapolating off the icon.
    #[test]
    fn mapping_clamps_outside_the_sprite() {
        let segments = axis_segments(35, &[(11, 12)], 40.0, 1.0);
        assert_eq!(map_along(&segments, -5.0), 0.0);
        assert!((map_along(&segments, 999.0) - 40.0).abs() < 1e-4);
    }

    /// The measured case: a 35px pin at ratio 1.5 with an 8x7.33pt content
    /// box, fitting a 12pt label. This is the number that explains why an
    /// unfitted pin looks short next to mapbox-gl's.
    #[test]
    fn fitting_a_tall_label_grows_the_badge_by_the_shortfall() {
        let (width, height) = fitted_size(
            (35, 35),
            1.5,
            Some([11, 9, 23, 20]),
            (7.0, 14.0),
            [0.0; 4],
            true,
            true,
        );
        // Content is 12px/1.5 = 8pt wide and 11px/1.5 = 7.33pt tall.
        // A 7pt-wide label already fits, so width does not move.
        assert!((width - 35.0 / 1.5).abs() < 1e-3, "width was {width}");
        // A 14pt-tall label needs 6.67pt more than the box has.
        assert!((height - (35.0 / 1.5 + 14.0 - 11.0 / 1.5)).abs() < 1e-3, "height was {height}");
        assert!(height > width, "the fitted pin must be taller than it is wide: {height} vs {width}");
    }

    /// Padding counts toward what the box must hold -- it is the gap the
    /// style asks for between the label and the badge's inner edge.
    #[test]
    fn padding_is_part_of_what_the_box_must_wrap() {
        let bare = fitted_size((35, 35), 1.0, Some([10, 10, 25, 25]), (20.0, 20.0), [0.0; 4], true, true);
        let padded =
            fitted_size((35, 35), 1.0, Some([10, 10, 25, 25]), (20.0, 20.0), [2.0, 3.0, 4.0, 5.0], true, true);
        assert!((padded.0 - bare.0 - 6.0).abs() < 1e-4, "left+right: {padded:?} vs {bare:?}");
        assert!((padded.1 - bare.1 - 8.0).abs() < 1e-4, "top+bottom: {padded:?} vs {bare:?}");
    }

    /// `icon-text-fit` names an AXIS, and the other one must not move.
    #[test]
    fn fitting_one_axis_leaves_the_other_at_its_natural_size() {
        let natural = 35.0;
        let text = (60.0, 60.0);
        let content = Some([10, 10, 25, 25]);
        let width_only = fitted_size((35, 35), 1.0, content, text, [0.0; 4], true, false);
        assert!(width_only.0 > natural);
        assert_eq!(width_only.1, natural);

        let height_only = fitted_size((35, 35), 1.0, content, text, [0.0; 4], false, true);
        assert_eq!(height_only.0, natural);
        assert!(height_only.1 > natural);
    }

    /// A label smaller than the box leaves the icon alone. Shrinking a
    /// badge to hug a narrow label would make a row of one-digit pins
    /// smaller than the sprite their author drew.
    #[test]
    fn a_label_that_already_fits_leaves_the_icon_at_its_authored_size() {
        let fitted =
            fitted_size((35, 35), 1.0, Some([5, 5, 30, 30]), (2.0, 2.0), [0.0; 4], true, true);
        assert_eq!(fitted, (35.0, 35.0));
    }

    /// No content box means nothing to fit to, whatever the label.
    #[test]
    fn an_icon_with_no_content_box_never_grows() {
        let fitted = fitted_size((35, 35), 1.0, None, (200.0, 200.0), [0.0; 4], true, true);
        assert_eq!(fitted, (35.0, 35.0));
    }

    /// Every patch of a stretched icon tiles the destination exactly -- no
    /// gaps between them (a visible seam through the badge) and no overlap
    /// (a doubled edge, which for a translucent halo reads as a dark line).
    #[test]
    fn the_patches_tile_the_icon_without_gaps_or_overlaps() {
        let x = axis_segments(35, &[(11, 12), (22, 23)], 45.0, 1.0);
        let y = axis_segments(35, &[(8, 19)], 50.0, 1.0);
        let patches = patches(&x, &y);

        let total_area: f32 = patches.iter().map(|p| p.dst.2 * p.dst.3).sum();
        assert!((total_area - 45.0 * 50.0).abs() < 1e-2, "patches covered {total_area} of 2250");

        // Column edges line up: every patch's right edge is some patch's
        // left edge, or the icon's own right edge.
        let mut rights: Vec<f32> = patches.iter().map(|p| p.dst.0 + p.dst.2).collect();
        rights.sort_by(f32::total_cmp);
        assert!((rights.last().expect("patches exist") - 45.0).abs() < 1e-4);
    }
}

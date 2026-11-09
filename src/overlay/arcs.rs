//! Origin-destination arcs from columns: a [`crate::render::arcs::ArcSet`]
//! built from source and target positions, per-arc colours (constant, or
//! from data through [`super::color_scale`]), widths, and an optional
//! filter column (`overlay::filter`'s relative values, so a time window is
//! a uniform). Pure: every shell builds its sets here.

use serde_json::Value;

use super::color_scale::ColorScale;
use super::csv::{CsvSource, CsvValues};
use super::filter::PointFilter;
use crate::render::arcs::{pack_color, ArcFilterInfo, ArcInstance, ArcSet};
use crate::style::Rgba;

/// Each arc's colours: one pair for all, or one pair per arc.
#[derive(Debug, Clone, PartialEq)]
pub enum ArcColors {
    /// Kepler's `color` and `targetColor`.
    Constant { source: Rgba, target: Rgba },
    /// Per arc, in row order.
    PerArc(Vec<(Rgba, Rgba)>),
}

/// Each arc's width in logical pixels.
#[derive(Debug, Clone, PartialEq)]
pub enum ArcWidths {
    Constant(f32),
    PerArc(Vec<f32>),
}

/// The arcs of `source[i] -> target[i]` (lon, lat), coloured and sized.
/// `filter`: one absolute value per row (`NaN` for none) and whether the
/// column was time; the set then carries relative values and its base.
/// A row with a non-finite end, or source == target, is dropped.
#[must_use]
pub fn build(
    source: &[[f64; 2]],
    target: &[[f64; 2]],
    colors: &ArcColors,
    widths: &ArcWidths,
    filter: Option<(&str, &[f64], bool)>,
) -> ArcSet {
    let n = source.len().min(target.len());
    let filter = filter.and_then(|(field, values, is_time)| PointFilter::from_values(field, values, is_time));
    let mut instances = Vec::with_capacity(n);
    // The input row of each pushed instance: a row without a colour pair
    // is skipped, so the push index is not the row.
    let mut row_of: Vec<u32> = Vec::with_capacity(n);
    for i in 0..n {
        let (s, t) = (source[i], target[i]);
        let (sc, tc) = match colors {
            ArcColors::Constant { source, target } => (*source, *target),
            ArcColors::PerArc(v) => match v.get(i) {
                Some(pair) => *pair,
                None => continue,
            },
        };
        let width_px = match widths {
            ArcWidths::Constant(w) => *w,
            ArcWidths::PerArc(v) => v.get(i).copied().unwrap_or(0.0),
        };
        instances.push(ArcInstance {
            source: [s[0] as f32, s[1] as f32],
            target: [t[0] as f32, t[1] as f32],
            source_color: pack_color(sc),
            target_color: pack_color(tc),
            width_px,
            filter: filter.as_ref().map_or(0.0, |f| f.value(i)),
        });
        row_of.push(i as u32);
    }
    let mut set = ArcSet::new(instances);
    for r in &mut set.rows {
        *r = row_of[*r as usize];
    }
    set.filter = filter.map(|f| ArcFilterInfo { base: f.base, domain: f.domain });
    set
}

/// Arc `index` of `set` as a hover highlight draws it
/// (`MapView::set_highlight`): two instances over the layer, a casing
/// `style.outline_width_px` wider each side in `style.outline_color`, and
/// the arc again on top, its colours brightened halfway to white and at
/// least a pixel and a half wide. The set's filter base is kept, so the
/// layer's window hides the highlight with the arc. `None` for an index
/// past the set.
#[must_use]
pub fn highlight(set: &ArcSet, index: usize, style: &super::HighlightStyle) -> Option<ArcSet> {
    let a = *set.instances.get(index)?;
    let unpack = |c: u32| {
        let [r, g, b, a] = c.to_le_bytes().map(|v| f32::from(v) / 255.0);
        Rgba { r, g, b, a }
    };
    let bright = |c: u32| {
        let c = unpack(c);
        pack_color(Rgba { r: c.r + (1.0 - c.r) * 0.5, g: c.g + (1.0 - c.g) * 0.5, b: c.b + (1.0 - c.b) * 0.5, a: 1.0 })
    };
    let core_width = a.width_px.max(1.5);
    let casing = ArcInstance {
        source_color: pack_color(style.outline_color),
        target_color: pack_color(style.outline_color),
        width_px: core_width + 2.0 * style.outline_width_px.max(0.0),
        ..a
    };
    let core = ArcInstance { source_color: bright(a.source_color), target_color: bright(a.target_color), width_px: core_width, ..a };
    let mut out = ArcSet::new(vec![casing, core]);
    out.filter = set.filter;
    // Both rows are the arc's own.
    let row = set.row(index)? as u32;
    out.rows = vec![row; out.instances.len()];
    Some(out)
}

/// Each value's colour under `scale` (`None` -> `missing`): per-arc colours
/// from data, as Kepler's `colorField` gives them.
#[must_use]
pub fn scale_colors(scale: &ColorScale, values: &[f64], missing: Rgba) -> Vec<Rgba> {
    let parsed = super::color_scale::rgba(&scale.colors);
    values
        .iter()
        .map(|&v| {
            scale
                .color_for(&Value::from(v))
                .and_then(|hex| scale.colors.iter().position(|c| c == hex))
                .and_then(|i| parsed.get(i).copied())
                .unwrap_or(missing)
        })
        .collect()
}

/// A CSV's `(lon, lat)` per row from two numeric columns; `None` when
/// either column is missing or not numeric. A row without a number is
/// `NaN` (and its arc dropped).
#[must_use]
pub fn csv_positions(csv: &CsvSource, lon: &str, lat: &str) -> Option<Vec<[f64; 2]>> {
    let (Some(CsvValues::Number(x)), Some(CsvValues::Number(y))) = (csv.column(lon), csv.column(lat)) else {
        return None;
    };
    Some(x.iter().zip(y).map(|(x, y)| [x.unwrap_or(f64::NAN), y.unwrap_or(f64::NAN)]).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: Rgba = Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 };
    const BLUE: Rgba = Rgba { r: 0.0, g: 0.0, b: 1.0, a: 1.0 };

    /// Packing: one 32-byte instance per valid row, colours RGBA8, widths
    /// as given, filter values relative to the column's minimum -- and the
    /// set's window maps an absolute range onto them.
    #[test]
    fn rows_pack_into_instances_with_relative_filter_values() {
        let source = [[-0.1, 51.5], [-2.24, 53.48], [f64::NAN, 0.0], [1.0, 1.0]];
        let target = [[-2.24, 53.48], [-0.1, 51.5], [1.0, 1.0], [1.0, 1.0]];
        let times = [1.421e12 + 60_000.0, 1.421e12, 1.421e12 + 5.0, 1.421e12 + 7.0];
        let set = build(
            &source,
            &target,
            &ArcColors::Constant { source: RED, target: BLUE },
            &ArcWidths::PerArc(vec![1.0, 2.5, 3.0, 4.0]),
            Some(("t", &times, true)),
        );
        assert_eq!(set.len(), 2, "the NaN row and the zero-length row drop");
        assert_eq!(set.upload_bytes(), 64);
        let info = set.filter.expect("filter info");
        assert_eq!(info.base, 1.421e12);
        assert_eq!(info.domain, [1.421e12, 1.421e12 + 60_000.0]);
        let mut values: Vec<f32> = set.instances.iter().map(|a| a.filter).collect();
        values.sort_by(f32::total_cmp);
        assert_eq!(values, vec![0.0, 60_000.0]);
        let first = set.instances.iter().find(|a| a.filter == 60_000.0).unwrap();
        assert_eq!(first.source, [-0.1, 51.5]);
        assert_eq!(first.width_px, 1.0);
        assert_eq!(first.source_color.to_le_bytes(), [255, 0, 0, 255]);
        assert_eq!(first.target_color.to_le_bytes(), [0, 0, 255, 255]);
        // The first minute keeps the second arc only.
        let [lo, hi] = set.relative_window([1.421e12, 1.421e12 + 59_000.0]).unwrap();
        let kept: Vec<f32> = set.instances.iter().map(|a| a.filter).filter(|v| *v >= lo && *v <= hi).collect();
        assert_eq!(kept, vec![0.0]);
    }

    #[test]
    fn colours_come_from_a_colour_scale() {
        let scale = ColorScale::quantize("flows", 0.0, 100.0, vec!["#ff0000".into(), "#0000ff".into()]).unwrap();
        let grey = Rgba { r: 0.5, g: 0.5, b: 0.5, a: 1.0 };
        let c = scale_colors(&scale, &[10.0, 90.0, f64::NAN], grey);
        assert_eq!(c, vec![RED, BLUE, grey]);
        let set = build(
            &[[0.0, 0.0], [0.0, 0.0]],
            &[[1.0, 0.0], [2.0, 0.0]],
            &ArcColors::PerArc(c.iter().map(|&x| (x, x)).collect()),
            &ArcWidths::Constant(1.0),
            None,
        );
        assert!(set.filter.is_none());
        let colours: Vec<[u8; 4]> = set.instances.iter().map(|a| a.source_color.to_le_bytes()).collect();
        assert!(colours.contains(&[255, 0, 0, 255]) && colours.contains(&[0, 0, 255, 255]));
    }

    /// The geometry's own columns become `points()`; any other pair of
    /// numeric columns reads as positions here.
    #[test]
    fn csv_columns_read_as_positions() {
        let csv = CsvSource::from_bytes(b"lng,lat,b_lng,b_lat\n1,2,3,4\n5,6,,8\n", &Default::default()).unwrap();
        let b = csv_positions(&csv, "b_lng", "b_lat").unwrap();
        assert_eq!(b[0], [3.0, 4.0]);
        assert!(b[1][0].is_nan() && b[1][1] == 8.0);
        assert!(csv_positions(&csv, "nope", "b_lat").is_none());
        assert_eq!(csv.points().unwrap(), &[[1.0, 2.0], [5.0, 6.0]]);
    }
}

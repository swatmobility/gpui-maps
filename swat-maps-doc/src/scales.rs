//! Which scales a field offers, and the binding a field gets when a user
//! first picks it (Kepler's `FIELD_OPTS`, `default-settings.ts`):
//!
//! | field type | colour scales | size scales |
//! |---|---|---|
//! | string, boolean | ordinal | none |
//! | integer, real | quantile, quantize, linear | linear, sqrt, log |
//! | timestamp, date, geometry | none in P2 (time arrives with P3) | none |
//!
//! Palettes are named, not listed: the host owns the palette table.

use crate::doc::ChannelBinding;
use crate::table::{Column, Field, FieldType};

/// The sequential palette a numeric field opens with: dark to bright, so
/// both ends hold on a light and a dark basemap.
pub const DEFAULT_SEQUENTIAL: &str = "Global Warming";
pub const DEFAULT_SEQUENTIAL_STEPS: u32 = 6;
/// The qualitative palette for up to 8 categories, and the larger one for
/// more.
pub const DEFAULT_QUALITATIVE: &str = "Uber Viz Qualitative 1.6";
pub const DEFAULT_QUALITATIVE_STEPS: u32 = 8;
pub const LARGE_QUALITATIVE: &str = "Uber Viz Qualitative 4";
pub const LARGE_QUALITATIVE_STEPS: u32 = 20;

/// The size range a point opens with, in Kepler radius units (the host's
/// `kepler_radius_px` turns 12..100 into roughly 2.5..21 px).
pub const DEFAULT_SIZE_RANGE: [f64; 2] = [12.0, 100.0];

/// The colour scales `ty` offers, the default first.
pub fn color_scales(ty: FieldType) -> &'static [&'static str] {
    match ty {
        FieldType::String | FieldType::Boolean => &["ordinal"],
        FieldType::Integer | FieldType::Real => &["quantile", "quantize", "linear"],
        // Milliseconds since 1970: even spans of time first.
        FieldType::Timestamp | FieldType::Date => &["quantize", "quantile", "linear"],
        FieldType::GeoJson | FieldType::Point => &[],
    }
}

/// The size scales `ty` offers, the default first.
pub fn size_scales(ty: FieldType) -> &'static [&'static str] {
    match ty {
        FieldType::Integer | FieldType::Real => &["sqrt", "linear", "log"],
        _ => &[],
    }
}

/// Whether `scale` suits a field of `range` (`log` needs a strictly
/// positive domain).
pub fn scale_fits(scale: &str, range: Option<[f64; 2]>) -> bool {
    scale != "log" || range.is_some_and(|[lo, _]| lo > 0.0)
}

/// How many distinct values a string or boolean field holds.
pub fn distinct(field: &Field) -> usize {
    match &field.column {
        Column::Str { dict, .. } => dict.len(),
        Column::Bool(v) => {
            let (t, f) = v
                .iter()
                .flatten()
                .fold((false, false), |(t, f), b| (t || *b, f || !*b));
            usize::from(t) + usize::from(f)
        }
        _ => 0,
    }
}

/// The colour binding `field` opens with, or `None` if it offers no
/// colour scale.
pub fn default_color(field: &Field) -> Option<ChannelBinding> {
    let scale = *color_scales(field.ty).first()?;
    let mut b = ChannelBinding::new(&field.name, scale);
    let (palette, steps) = if scale == "ordinal" {
        if distinct(field) > DEFAULT_QUALITATIVE_STEPS as usize {
            (LARGE_QUALITATIVE, LARGE_QUALITATIVE_STEPS)
        } else {
            (DEFAULT_QUALITATIVE, DEFAULT_QUALITATIVE_STEPS)
        }
    } else {
        (DEFAULT_SEQUENTIAL, DEFAULT_SEQUENTIAL_STEPS)
    };
    b.palette = Some(palette.into());
    b.steps = Some(steps);
    Some(b)
}

/// The size binding `field` opens with, or `None` if it offers no size
/// scale.
pub fn default_size(field: &Field) -> Option<ChannelBinding> {
    let scale = size_scales(field.ty)
        .iter()
        .copied()
        .find(|s| scale_fits(s, field.range()))?;
    let mut b = ChannelBinding::new(&field.name, scale);
    b.range = Some(DEFAULT_SIZE_RANGE);
    Some(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fields::field;

    #[test]
    fn an_ordinal_scale_is_offered_only_for_strings_and_booleans() {
        for ty in [
            FieldType::Boolean,
            FieldType::Integer,
            FieldType::Real,
            FieldType::String,
            FieldType::Timestamp,
            FieldType::Date,
            FieldType::GeoJson,
            FieldType::Point,
        ] {
            let ordinal = color_scales(ty).contains(&"ordinal");
            assert_eq!(
                ordinal,
                matches!(ty, FieldType::String | FieldType::Boolean),
                "{ty:?}"
            );
            let numeric = color_scales(ty).contains(&"quantile");
            assert_eq!(
                numeric,
                ty.is_numeric() || matches!(ty, FieldType::Timestamp | FieldType::Date),
                "{ty:?}: numbers and times"
            );
            assert_eq!(
                !size_scales(ty).is_empty(),
                ty.is_numeric(),
                "{ty:?}: size is numeric only"
            );
        }
    }

    #[test]
    fn defaults_follow_the_field_type() {
        let mut pop = field("population", Column::Number(vec![Some(1.0e6), Some(3.7e7)]));
        pop.ty = FieldType::Integer;
        let c = default_color(&pop).unwrap();
        assert_eq!(
            (c.scale.as_str(), c.palette.as_deref(), c.steps),
            ("quantile", Some(DEFAULT_SEQUENTIAL), Some(6))
        );
        let s = default_size(&pop).unwrap();
        assert_eq!(
            (s.scale.as_str(), s.range),
            ("sqrt", Some(DEFAULT_SIZE_RANGE))
        );
        let few = field("capital", Column::from_strs([Some("true"), Some("false")]));
        assert_eq!(default_color(&few).unwrap().scale, "ordinal");
        let many: Vec<String> = (0..30).map(|i| format!("c{i}")).collect();
        let many = field(
            "country",
            Column::from_strs(many.iter().map(|s| Some(s.as_str()))),
        );
        assert_eq!(
            default_color(&many).unwrap().palette.as_deref(),
            Some(LARGE_QUALITATIVE)
        );
        assert!(default_size(&many).is_none());
    }

    #[test]
    fn log_is_skipped_for_a_field_that_reaches_zero() {
        assert!(
            !scale_fits("log", Some([0.0, 5.0]))
                && scale_fits("log", Some([0.5, 5.0]))
                && scale_fits("sqrt", Some([-1.0, 5.0]))
        );
    }
}

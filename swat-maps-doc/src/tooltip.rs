//! Tooltip configuration (spec §2.6): which fields a layer's hover card
//! shows, in which order, and how each prints. Kepler's
//! `interactionConfig.tooltip.fieldsToShow`: a list of `{name, format}`
//! per dataset, at most [`MAX_FIELDS`] long, where `format` is a d3-format
//! string for numbers and a moment.js token for times. We keep Kepler's
//! format strings so a P5 export writes them through unchanged.

use crate::doc::{Hover, HoverField};
use crate::table::FieldType;

/// Kepler's `MAX_DEFAULT_TOOLTIPS` is 5 for a new layer; a user may check
/// up to ten.
pub const MAX_FIELDS: usize = 10;

/// The subset of Kepler's `TOOLTIP_FORMATS` the card offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Format {
    /// As the column reads: grouped integers, a sensible precision.
    None,
    /// `.1f`: one decimal.
    Fixed1,
    /// `.2f`: two decimals.
    Fixed2,
    /// `,`: grouped, rounded to a whole number.
    Grouped,
    /// `.0%`: a 0..1 fraction as a whole percentage.
    Percent,
    /// `L`: the date.
    Date,
    /// `L LT`: the date and time.
    DateTime,
    /// `LT`: the time of day.
    Time,
}

impl Format {
    pub const NUMBER: [Format; 5] = [
        Format::None,
        Format::Fixed1,
        Format::Fixed2,
        Format::Grouped,
        Format::Percent,
    ];
    pub const TIME: [Format; 4] = [Format::None, Format::Date, Format::DateTime, Format::Time];

    /// The string a document stores (Kepler's), `None` for the default.
    pub fn key(self) -> Option<&'static str> {
        match self {
            Format::None => None,
            Format::Fixed1 => Some(".1f"),
            Format::Fixed2 => Some(".2f"),
            Format::Grouped => Some(","),
            Format::Percent => Some(".0%"),
            Format::Date => Some("L"),
            Format::DateTime => Some("L LT"),
            Format::Time => Some("LT"),
        }
    }

    /// A stored format; an unknown string reads as the default (and is
    /// kept in the document as written until the user picks another).
    pub fn parse(key: Option<&str>) -> Format {
        match key {
            Some(".1f") => Format::Fixed1,
            Some(".2f") => Format::Fixed2,
            Some(",") | Some(",d") => Format::Grouped,
            Some(".0%") => Format::Percent,
            Some("L") => Format::Date,
            Some("L LT") => Format::DateTime,
            Some("LT") => Format::Time,
            _ => Format::None,
        }
    }

    /// What the format select shows.
    pub fn label(self) -> &'static str {
        match self {
            Format::None => "Auto",
            Format::Fixed1 => "1.2",
            Format::Fixed2 => "1.23",
            Format::Grouped => "1,234",
            Format::Percent => "12%",
            Format::Date => "Date",
            Format::DateTime => "Date & time",
            Format::Time => "Time",
        }
    }

    /// The formats a field of type `ty` offers (none for text).
    pub fn offered(ty: FieldType) -> &'static [Format] {
        match ty {
            FieldType::Integer | FieldType::Real => &Format::NUMBER,
            FieldType::Timestamp | FieldType::Date => &Format::TIME,
            _ => &[],
        }
    }
}

/// `hover` with `field` checked (appended last) or unchecked. Checking an
/// eleventh field changes nothing.
pub fn toggled(hover: &Hover, field: &str) -> Hover {
    let mut out = hover.clone();
    match out.card.iter().position(|h| h.field == field) {
        Some(i) => {
            out.card.remove(i);
        }
        None if out.card.len() < MAX_FIELDS => out.card.push(HoverField::new(field)),
        None => {}
    }
    out
}

/// `hover` with `field` moved to index `to` (clamped).
pub fn moved(hover: &Hover, field: &str, to: usize) -> Hover {
    let mut out = hover.clone();
    if let Some(from) = out.card.iter().position(|h| h.field == field) {
        let f = out.card.remove(from);
        let to = to.min(out.card.len());
        out.card.insert(to, f);
    }
    out
}

/// `hover` with `field`'s format set.
pub fn with_format(hover: &Hover, field: &str, format: Format) -> Hover {
    let mut out = hover.clone();
    if let Some(h) = out.card.iter_mut().find(|h| h.field == field) {
        h.format = format.key().map(str::to_string);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hover(names: &[&str]) -> Hover {
        Hover {
            card: names.iter().map(|n| HoverField::new(*n)).collect(),
            highlight: true,
            ..Hover::default()
        }
    }

    fn names(h: &Hover) -> Vec<&str> {
        h.card.iter().map(|f| f.field.as_str()).collect()
    }

    #[test]
    fn toggling_appends_and_removes() {
        let h = hover(&["a", "b"]);
        assert_eq!(names(&toggled(&h, "c")), ["a", "b", "c"]);
        assert_eq!(names(&toggled(&h, "a")), ["b"]);
        let full = hover(&["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"]);
        assert_eq!(toggled(&full, "x"), full, "an eleventh field is refused");
    }

    #[test]
    fn moving_reorders_and_clamps() {
        let h = hover(&["a", "b", "c"]);
        assert_eq!(names(&moved(&h, "c", 0)), ["c", "a", "b"]);
        assert_eq!(names(&moved(&h, "a", 99)), ["b", "c", "a"]);
        assert_eq!(moved(&h, "zz", 0), h);
    }

    #[test]
    fn formats_round_trip_through_their_kepler_strings() {
        for f in Format::NUMBER.iter().chain(&Format::TIME) {
            assert_eq!(Format::parse(f.key()), *f);
        }
        let h = with_format(&hover(&["mag"]), "mag", Format::Percent);
        assert_eq!(h.card[0].format.as_deref(), Some(".0%"));
        assert_eq!(with_format(&h, "mag", Format::None).card[0].format, None);
        assert!(Format::offered(FieldType::String).is_empty());
        assert_eq!(Format::offered(FieldType::Timestamp)[2], Format::DateTime);
    }
}

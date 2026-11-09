//! The in-memory dataset (spec §4.1): one geometry column and N typed,
//! columnar fields. Pure: the loaders that fill it live in the Studio,
//! because they call the engine's readers.

use std::sync::Arc;

/// Kepler's field types (`ALL_FIELD_TYPES` subset).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FieldType {
    Boolean,
    Integer,
    Real,
    String,
    Timestamp,
    Date,
    GeoJson,
    Point,
}

impl FieldType {
    /// The name a document stores (`Source::columns`).
    pub fn name(self) -> &'static str {
        match self {
            FieldType::Boolean => "boolean",
            FieldType::Integer => "integer",
            FieldType::Real => "real",
            FieldType::String => "string",
            FieldType::Timestamp => "timestamp",
            FieldType::Date => "date",
            FieldType::GeoJson => "geojson",
            FieldType::Point => "point",
        }
    }

    /// The type a document names ([`FieldType::name`]'s inverse).
    pub fn parse(name: &str) -> Option<FieldType> {
        Some(match name {
            "boolean" => FieldType::Boolean,
            "integer" => FieldType::Integer,
            "real" => FieldType::Real,
            "string" => FieldType::String,
            "timestamp" => FieldType::Timestamp,
            "date" => FieldType::Date,
            "geojson" => FieldType::GeoJson,
            "point" => FieldType::Point,
            _ => return None,
        })
    }

    pub fn is_numeric(self) -> bool {
        matches!(self, FieldType::Integer | FieldType::Real)
    }
}

/// A UI hint on top of the type, not a type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FieldHint {
    H3,
    Categorical,
}

/// Code meaning "missing" in [`Column::Str`].
pub const MISSING: u32 = u32::MAX;

#[derive(Clone, Debug, PartialEq)]
pub enum Column {
    /// Exactly the slice the engine's `PropertyColumn::Number` takes.
    Number(Vec<Option<f64>>),
    Bool(Vec<Option<bool>>),
    /// Dictionary-encoded; [`MISSING`] is a missing value.
    Str {
        codes: Vec<u32>,
        dict: Vec<Arc<str>>,
    },
    /// Milliseconds since 1970 UTC, and the raw text column for display
    /// when the values came from text.
    Time {
        ms: Vec<Option<f64>>,
        raw: Option<Box<Column>>,
    },
}

impl Column {
    pub fn len(&self) -> usize {
        match self {
            Column::Number(v) => v.len(),
            Column::Bool(v) => v.len(),
            Column::Str { codes, .. } => codes.len(),
            Column::Time { ms, .. } => ms.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Dictionary-encodes `values`.
    pub fn from_strs<'a>(values: impl IntoIterator<Item = Option<&'a str>>) -> Column {
        let mut index: std::collections::HashMap<&'a str, u32> = std::collections::HashMap::new();
        let mut dict: Vec<Arc<str>> = Vec::new();
        let codes = values
            .into_iter()
            .map(|v| match v {
                None => MISSING,
                Some(s) => *index.entry(s).or_insert_with(|| {
                    dict.push(Arc::from(s));
                    (dict.len() - 1) as u32
                }),
            })
            .collect();
        Column::Str { codes, dict }
    }

    /// Row `i` as text, for a card or a table cell.
    pub fn text(&self, i: usize) -> Option<String> {
        match self {
            Column::Number(v) => v.get(i).copied().flatten().map(|n| n.to_string()),
            Column::Bool(v) => v.get(i).copied().flatten().map(|b| b.to_string()),
            Column::Str { codes, dict } => codes
                .get(i)
                .and_then(|&c| dict.get(c as usize))
                .map(|s| s.to_string()),
            Column::Time { ms, raw } => match raw {
                Some(raw) => raw.text(i),
                None => ms.get(i).copied().flatten().map(|n| n.to_string()),
            },
        }
    }

    /// The rows `rows` (indices, in order) of this column; a string
    /// column keeps its dictionary.
    pub fn take(&self, rows: &[u32]) -> Column {
        fn pick<T: Clone>(v: &[T], rows: &[u32]) -> Vec<T> {
            rows.iter().map(|&r| v[r as usize].clone()).collect()
        }
        match self {
            Column::Number(v) => Column::Number(pick(v, rows)),
            Column::Bool(v) => Column::Bool(pick(v, rows)),
            Column::Str { codes, dict } => Column::Str {
                codes: pick(codes, rows),
                dict: dict.clone(),
            },
            Column::Time { ms, raw } => Column::Time {
                ms: pick(ms, rows),
                raw: raw.as_ref().map(|r| Box::new(r.take(rows))),
            },
        }
    }

    /// The numbers of a numeric or time column (time in ms).
    pub fn numbers(&self) -> Option<&[Option<f64>]> {
        match self {
            Column::Number(v) | Column::Time { ms: v, .. } => Some(v),
            _ => None,
        }
    }

    /// Row `i` as `&str` for a string column.
    pub fn str_at(&self, i: usize) -> Option<&str> {
        match self {
            Column::Str { codes, dict } => codes
                .get(i)
                .and_then(|&c| dict.get(c as usize))
                .map(|s| &**s),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub name: String,
    pub ty: FieldType,
    pub hint: Option<FieldHint>,
    pub column: Column,
}

impl Field {
    /// `[min, max]` of a numeric or time column, `None` when it has no
    /// value (computed on demand; P2 caches it).
    pub fn range(&self) -> Option<[f64; 2]> {
        let values = match &self.column {
            Column::Number(v) | Column::Time { ms: v, .. } => v,
            _ => return None,
        };
        values
            .iter()
            .flatten()
            .filter(|v| v.is_finite())
            .fold(None, |acc, &v| match acc {
                None => Some([v, v]),
                Some([lo, hi]) => Some([lo.min(v), hi.max(v)]),
            })
    }
}

/// The geometry column.
#[derive(Clone, Debug, PartialEq)]
pub enum Geometry {
    /// One `[lon, lat]` per row.
    Points(Vec<[f64; 2]>),
    /// One feature per row; the geometries themselves are an engine type
    /// and are held by the Studio beside the table. The count is here so
    /// pure code knows what it has.
    Features {
        count: usize,
        bounds: Option<[f64; 4]>,
        all_points: bool,
    },
    None,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    /// Unique within a document (the source id).
    pub id: String,
    pub label: String,
    pub rows: usize,
    pub geometry: Geometry,
    pub fields: Vec<Field>,
}

impl Table {
    /// `[min_lon, min_lat, max_lon, max_lat]` of the geometry, `None` when
    /// there is nothing finite to frame.
    pub fn bounds(&self) -> Option<[f64; 4]> {
        match &self.geometry {
            Geometry::Points(points) => {
                let mut b = [
                    f64::INFINITY,
                    f64::INFINITY,
                    f64::NEG_INFINITY,
                    f64::NEG_INFINITY,
                ];
                for [lon, lat] in points
                    .iter()
                    .filter(|p| p[0].is_finite() && p[1].is_finite())
                {
                    b = [
                        b[0].min(*lon),
                        b[1].min(*lat),
                        b[2].max(*lon),
                        b[3].max(*lat),
                    ];
                }
                b.iter().all(|v| v.is_finite()).then_some(b)
            }
            Geometry::Features { bounds, .. } => *bounds,
            Geometry::None => None,
        }
    }

    pub fn field(&self, name: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.name == name)
    }

    /// The table of rows `rows` only (indices into this one, in order): a
    /// CPU filter's passing rows. A feature table keeps its count and
    /// bounds; the host gathers its geometries.
    pub fn take(&self, rows: &[u32]) -> Table {
        let geometry = match &self.geometry {
            Geometry::Points(p) => Geometry::Points(rows.iter().map(|&r| p[r as usize]).collect()),
            Geometry::Features {
                bounds, all_points, ..
            } => Geometry::Features {
                count: rows.len(),
                bounds: *bounds,
                all_points: *all_points,
            },
            Geometry::None => Geometry::None,
        };
        let fields = self
            .fields
            .iter()
            .map(|f| Field {
                name: f.name.clone(),
                ty: f.ty,
                hint: f.hint,
                column: f.column.take(rows),
            })
            .collect();
        Table {
            id: self.id.clone(),
            label: self.label.clone(),
            rows: rows.len(),
            geometry,
            fields,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_are_dictionary_encoded_with_missing_as_max() {
        let c = Column::from_strs([Some("a"), None, Some("b"), Some("a")]);
        match &c {
            Column::Str { codes, dict } => {
                assert_eq!(codes, &[0, MISSING, 1, 0]);
                assert_eq!(dict.len(), 2);
            }
            _ => unreachable!(),
        }
        assert_eq!(c.str_at(3), Some("a"));
        assert_eq!(c.str_at(1), None);
    }

    #[test]
    fn take_keeps_the_rows_asked_for_in_order() {
        let t = Table {
            id: "t".into(),
            label: "t".into(),
            rows: 3,
            geometry: Geometry::Points(vec![[1.0, 1.0], [2.0, 2.0], [3.0, 3.0]]),
            fields: vec![Field {
                name: "s".into(),
                ty: FieldType::String,
                hint: None,
                column: Column::from_strs([Some("a"), Some("b"), None]),
            }],
        };
        let sub = t.take(&[2, 0]);
        assert_eq!(sub.rows, 2);
        assert_eq!(sub.geometry, Geometry::Points(vec![[3.0, 3.0], [1.0, 1.0]]));
        assert_eq!(
            (
                sub.fields[0].column.str_at(0),
                sub.fields[0].column.str_at(1)
            ),
            (None, Some("a"))
        );
    }

    #[test]
    fn point_bounds_skip_non_finite_rows() {
        let t = Table {
            id: "t".into(),
            label: "t".into(),
            rows: 3,
            geometry: Geometry::Points(vec![[1.0, 2.0], [f64::NAN, 0.0], [-3.0, 5.0]]),
            fields: vec![],
        };
        assert_eq!(t.bounds(), Some([-3.0, 2.0, 1.0, 5.0]));
    }
}

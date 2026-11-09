//! Trips from data: Kepler.gl's trip GeoJSON (`LineString`s whose
//! coordinates are `[lon, lat, altitude, timestamp]`) into a
//! [`crate::render::trips::TripSet`], each trip coloured by a property
//! through [`super::color_scale`] or one colour for all. Pure: every shell
//! builds its sets here.
//!
//! A feature's index in the collection is its trip row (what a pick
//! returns); a `MultiLineString` is one trip per part, sharing the row.

use serde_json::Value;

use super::color_scale::ColorScale;
use crate::render::trips::{TripInput, TripPoint, TripSet};
use crate::style::Rgba;

/// How each trip is coloured.
#[derive(Debug, Clone)]
pub enum TripColors<'a> {
    Constant(Rgba),
    /// The feature's `properties[scale.field]` through `scale`; `missing`
    /// where it has none.
    Scale { scale: &'a ColorScale, missing: Rgba },
}

/// One coordinate `[lon, lat, alt?, time?]` as a point. Kepler's own 4D
/// form; a 3D coordinate is `[lon, lat, time]` (no altitude).
fn point(c: &Value) -> Option<TripPoint> {
    let a = c.as_array()?;
    let n = |i: usize| a.get(i).and_then(Value::as_f64);
    match a.len() {
        3 => Some(TripPoint { lon: n(0)?, lat: n(1)?, alt_m: 0.0, time: n(2)? }),
        l if l >= 4 => Some(TripPoint { lon: n(0)?, lat: n(1)?, alt_m: n(2).unwrap_or(0.0), time: n(3)? }),
        _ => None,
    }
}

/// The trips of a FeatureCollection (or a lone Feature) of timed lines.
/// Features that are not lines, or whose coordinates carry no time, are
/// skipped.
#[must_use]
pub fn from_geojson(geojson: &Value, colors: &TripColors<'_>) -> TripSet {
    let features: Vec<&Value> = match geojson.get("features").and_then(Value::as_array) {
        Some(f) => f.iter().collect(),
        None => vec![geojson],
    };
    let parsed = colors_parsed(colors);
    let mut trips = Vec::new();
    for (row, f) in features.iter().enumerate() {
        let Some(g) = f.get("geometry") else { continue };
        let parts: Vec<&Value> = match g.get("type").and_then(Value::as_str) {
            Some("LineString") => g.get("coordinates").into_iter().collect(),
            Some("MultiLineString") => g.get("coordinates").and_then(Value::as_array).map(|p| p.iter().collect()).unwrap_or_default(),
            _ => continue,
        };
        let color = match colors {
            TripColors::Constant(c) => *c,
            TripColors::Scale { scale, missing } => f
                .get("properties")
                .and_then(|p| p.get(&scale.field))
                .and_then(|v| scale.color_for(v))
                .and_then(|hex| scale.colors.iter().position(|c| c == hex))
                .and_then(|i| parsed.get(i).copied())
                .unwrap_or(*missing),
        };
        for part in parts {
            let points: Vec<TripPoint> = part.as_array().map(|cs| cs.iter().filter_map(point).collect()).unwrap_or_default();
            trips.push(TripInput { row: row as u32, points, color, label: None });
        }
    }
    TripSet::from_trips(&trips)
}

fn colors_parsed(colors: &TripColors<'_>) -> Vec<Rgba> {
    match colors {
        TripColors::Constant(_) => Vec::new(),
        TripColors::Scale { scale, .. } => super::color_scale::rgba(&scale.colors),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Kepler's world-flights shape: 4D coordinates, coloured ordinally by
    /// `origin_country`; the feature index is the row.
    #[test]
    fn keplers_4d_trip_geojson_reads_as_trips() {
        let fc = json!({"type": "FeatureCollection", "features": [
            {"type": "Feature", "properties": {"origin_country": "Spain"},
             "geometry": {"type": "LineString", "coordinates": [[-3.7, 40.4, 1000.0, 1567555200.0], [-3.0, 41.0, 9000.0, 1567555500.0]]}},
            {"type": "Feature", "properties": {"origin_country": "France"},
             "geometry": {"type": "Point", "coordinates": [2.3, 48.8]}},
            {"type": "Feature", "properties": {},
             "geometry": {"type": "LineString", "coordinates": [[2.3, 48.8, 1567555300.0], [2.5, 49.0, 1567555400.0]]}}
        ]});
        let scale = ColorScale::ordinal("origin_country", vec![json!("France"), json!("Spain")], vec!["#ff0000".into(), "#0000ff".into()]).unwrap();
        let grey = Rgba { r: 0.5, g: 0.5, b: 0.5, a: 1.0 };
        let set = from_geojson(&fc, &TripColors::Scale { scale: &scale, missing: grey });
        assert_eq!(set.len(), 2);
        assert_eq!((set.trips[0].row, set.trips[1].row), (0, 2), "the Point is skipped, rows keep their index");
        assert_eq!(set.base, 1567555200.0);
        assert_eq!(set.vertices[1].alt_m, 9000.0);
        assert_eq!(set.vertices[1].time, 300.0);
        assert_eq!(set.vertices[0].color.to_le_bytes(), [0, 0, 255, 255], "Spain is the second ordinal colour");
        assert_eq!(set.vertices[2].color.to_le_bytes(), [128, 128, 128, 255], "no country: the missing colour");
        assert_eq!(set.vertices[2].alt_m, 0.0, "3D coordinates are lon, lat, time");
    }
}

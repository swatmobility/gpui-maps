//! A GeoJSON geometry reader (§7.3), beside the WKT one: a column holding
//! a GeoJSON geometry as a string (`{"type": "Polygon", "coordinates":
//! ...}`), the way a database without a geometry type stores one. `Point`, `MultiPoint`,
//! `LineString`, `MultiLineString`, `Polygon`, `MultiPolygon`,
//! `GeometryCollection` (when its members are one kind), and a `Feature`
//! wrapping one; 2D lon/lat, any extra ordinate ignored.

use serde_json::Value;

use super::wkt::Geometry;

/// Parses one GeoJSON geometry (or Feature) string, or `None` when it
/// cannot be read.
pub fn parse(text: &str) -> Option<Geometry> {
    let v: Value = serde_json::from_str(text.trim()).ok()?;
    of_value(&v)
}

fn pos(v: &Value) -> Option<[f64; 2]> {
    let a = v.as_array()?;
    let (x, y) = (a.first()?.as_f64()?, a.get(1)?.as_f64()?);
    (x.is_finite() && y.is_finite()).then_some([x, y])
}

fn line(v: &Value) -> Option<Vec<[f64; 2]>> {
    v.as_array()?.iter().map(pos).collect()
}

fn rings(v: &Value) -> Option<Vec<Vec<[f64; 2]>>> {
    v.as_array()?.iter().map(line).collect()
}

fn of_value(v: &Value) -> Option<Geometry> {
    let kind = v.get("type")?.as_str()?;
    let c = || v.get("coordinates");
    match kind {
        "Feature" => of_value(v.get("geometry")?),
        "Point" => Some(Geometry::Points(vec![pos(c()?)?])),
        "MultiPoint" => Some(Geometry::Points(line(c()?)?)),
        "LineString" => Some(Geometry::Lines(vec![line(c()?)?])),
        "MultiLineString" => Some(Geometry::Lines(rings(c()?)?)),
        "Polygon" => Some(Geometry::Polygons(vec![rings(c()?)?])),
        "MultiPolygon" => Some(Geometry::Polygons(c()?.as_array()?.iter().map(rings).collect::<Option<_>>()?)),
        "GeometryCollection" => {
            let parts: Vec<Geometry> = v.get("geometries")?.as_array()?.iter().filter_map(of_value).collect();
            let mut out: Option<Geometry> = None;
            for g in parts {
                out = Some(match (out, g) {
                    (None, g) => g,
                    (Some(Geometry::Points(mut a)), Geometry::Points(b)) => {
                        a.extend(b);
                        Geometry::Points(a)
                    }
                    (Some(Geometry::Lines(mut a)), Geometry::Lines(b)) => {
                        a.extend(b);
                        Geometry::Lines(a)
                    }
                    (Some(Geometry::Polygons(mut a)), Geometry::Polygons(b)) => {
                        a.extend(b);
                        Geometry::Polygons(a)
                    }
                    // Mixed kinds: the first kind wins.
                    (Some(a), _) => a,
                });
            }
            out
        }
        _ => None,
    }
}

/// A geometry cell in either encoding: GeoJSON when it is a JSON object,
/// WKT otherwise.
pub fn parse_any(text: &str) -> Option<Geometry> {
    if text.trim_start().starts_with('{') {
        parse(text)
    } else {
        super::wkt::parse(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_geometry_kind_reads() {
        assert_eq!(parse(r#"{"type":"Point","coordinates":[139.77,35.68]}"#), Some(Geometry::Points(vec![[139.77, 35.68]])));
        assert_eq!(parse(r#"{"type":"MultiPoint","coordinates":[[1,2],[3,4,5]]}"#), Some(Geometry::Points(vec![[1.0, 2.0], [3.0, 4.0]])));
        assert_eq!(parse(r#"{"type":"LineString","coordinates":[[1,2],[3,4]]}"#), Some(Geometry::Lines(vec![vec![[1.0, 2.0], [3.0, 4.0]]])));
        assert_eq!(parse(r#"{"type":"MultiLineString","coordinates":[[[1,2],[3,4]],[[5,6],[7,8]]]}"#).map(|g| matches!(g, Geometry::Lines(l) if l.len() == 2)), Some(true));
        let poly = r#"{"type":"Polygon","coordinates":[[[0,0],[1,0],[1,1],[0,0]]]}"#;
        assert_eq!(parse(poly), Some(Geometry::Polygons(vec![vec![vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 0.0]]]])));
        let multi = r#"{"type":"MultiPolygon","coordinates":[[[[0,0],[1,0],[1,1],[0,0]]],[[[2,2],[3,2],[3,3],[2,2]]]]}"#;
        assert!(matches!(parse(multi), Some(Geometry::Polygons(p)) if p.len() == 2));
        let feature = r#"{"type":"Feature","properties":{},"geometry":{"type":"Point","coordinates":[1,2]}}"#;
        assert_eq!(parse(feature), Some(Geometry::Points(vec![[1.0, 2.0]])));
        let coll = r#"{"type":"GeometryCollection","geometries":[{"type":"Point","coordinates":[1,2]},{"type":"Point","coordinates":[3,4]}]}"#;
        assert_eq!(parse(coll), Some(Geometry::Points(vec![[1.0, 2.0], [3.0, 4.0]])));
    }

    #[test]
    fn junk_is_none_and_parse_any_picks_the_encoding() {
        assert!(parse("{").is_none());
        assert!(parse(r#"{"type":"Point","coordinates":["a",1]}"#).is_none());
        assert!(parse(r#"{"type":"Circle","coordinates":[1,2]}"#).is_none());
        assert_eq!(parse_any("POINT(1 2)"), Some(Geometry::Points(vec![[1.0, 2.0]])));
        assert_eq!(parse_any(r#" {"type":"Point","coordinates":[1,2]}"#), Some(Geometry::Points(vec![[1.0, 2.0]])));
    }
}

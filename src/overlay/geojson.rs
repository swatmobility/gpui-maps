//! Reading a GeoJSON `FeatureCollection` into the shape the overlay draws.
//!
//! Real GeoJSON rather than a bespoke document: it is what a caller's data
//! already is, and every geospatial tool on either end of the pipe speaks
//! it. Coordinates are `[lon, lat]`, GeoJSON's own order.

use serde_json::{Map, Value};

/// A feature's GeoJSON `id`, which the format permits to be either a string
/// or a number, and permits to be absent.
#[derive(Debug, Clone, PartialEq)]
pub enum FeatureId {
    String(String),
    Number(f64),
}

/// Geometry with the singular and `Multi` forms collapsed together, so
/// everything downstream handles three cases rather than six.
#[derive(Debug, Clone, PartialEq)]
pub enum OverlayGeometry {
    /// One or more line parts, each a run of `(lon, lat)`.
    Line(Vec<Vec<(f64, f64)>>),
    /// One or more polygons, each a list of rings: exterior first, then
    /// holes.
    Polygon(Vec<Vec<Vec<(f64, f64)>>>),
    /// One or more `(lon, lat)` positions.
    Point(Vec<(f64, f64)>),
    /// GeoJSON `null` geometry. Draws nothing, but still carries a label.
    /// It carries no HIT TARGET: `hit::HitIndex::build` drops it, because
    /// with nothing on screen there is nothing to point at.
    Empty,
    /// A TILED source's polygon or line once its tile index has landed
    /// (`OverlayStore::adopt_tile_index`): the lon/lat copy is dropped and
    /// the index's fixed-point geometry is the one copy. What is kept is the
    /// bounding box -- `(min_lon, min_lat, max_lon, max_lat)`, for label
    /// anchors and the hit index's pre-test -- and which kind it was, which
    /// `plan::classify` needs to pick fill or line paint. The exact hit test
    /// goes to `TileIndex::feature_hit`; `runs` yields nothing, so the
    /// whole-source plan skips it as it skips `Empty`.
    Indexed { bbox_lon_lat: (f64, f64, f64, f64), line: bool },
}

/// What a tiled source draws a feature as: a fill or a line. Points and
/// empty geometry have none (they stay on the whole-source path).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileKind {
    Fill,
    Line,
}

impl OverlayGeometry {
    /// `Some` for a non-empty polygon or line, and for an `Indexed` one
    /// (which keeps its kind after its lon/lat is dropped).
    pub fn tile_kind(&self) -> Option<TileKind> {
        match self {
            Self::Polygon(p) if !p.is_empty() => Some(TileKind::Fill),
            Self::Line(l) if !l.is_empty() => Some(TileKind::Line),
            Self::Indexed { line: false, .. } => Some(TileKind::Fill),
            Self::Indexed { line: true, .. } => Some(TileKind::Line),
            _ => None,
        }
    }

    /// Every contiguous run of `(lon, lat)` positions this geometry
    /// contains: one run per line part, one per polygon ring (exteriors and
    /// holes alike), or the single run of point positions. `Empty` yields
    /// none.
    ///
    /// A RUN rather than a flat position stream, for two reasons that
    /// happen to agree. `hit::HitIndex`'s exact tests are per run -- a
    /// segment must not be formed across the join between two line parts,
    /// and a ring must be closed against its own first vertex. And a
    /// bounding box, which genuinely does not care where a run ends, is
    /// still faster accumulated run by run: the inner loop is then a walk
    /// of a contiguous slice rather than a three-way `chain` of nested
    /// `Vec`s, which measured 1.6 ns/coordinate against 4.0 (see
    /// `geom::LonLatBounds::of_runs`).
    ///
    /// Borrowed slices and no allocation: this is walked once per feature
    /// per push by `plan::build_plan` and `hit::HitIndex::build`, and the
    /// `Vec`-per-feature version it replaced was 3,140 allocations a push
    /// on this crate's own fleet dataset.
    pub fn runs(&self) -> impl Iterator<Item = &[(f64, f64)]> + '_ {
        let points = match self {
            Self::Point(points) => Some(points.as_slice()),
            _ => None,
        };
        let parts = match self {
            Self::Line(parts) => Some(parts),
            _ => None,
        };
        let polygons = match self {
            Self::Polygon(polygons) => Some(polygons),
            _ => None,
        };
        points
            .into_iter()
            .chain(parts.into_iter().flatten().map(Vec::as_slice))
            .chain(polygons.into_iter().flatten().flatten().map(Vec::as_slice))
    }

    /// Every `(lon, lat)` position this geometry contains, run boundaries
    /// discarded -- for the callers that want a COUNT or a set rather than
    /// a walk. Prefer `runs` for anything on a push path; see above.
    pub fn positions(&self) -> impl Iterator<Item = (f64, f64)> + '_ {
        self.runs().flatten().copied()
    }
}

#[derive(Debug, Clone)]
pub struct ParsedFeature {
    pub id: Option<FeatureId>,
    pub geometry: OverlayGeometry,
    pub properties: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FeatureError {
    /// The document is not valid JSON at all.
    Json(String),
    /// Valid JSON, but not a `FeatureCollection`.
    NotAFeatureCollection,
    /// One feature could not be used. Named by position, and by `id` where
    /// it declared one.
    Feature { index: usize, id: Option<FeatureId>, reason: String },
    /// An update named a feature that is not in the drawn collection.
    ///
    /// Loud rather than silent on purpose: a caller holding an id from a
    /// collection they have since replaced has a real bug, and a no-op
    /// recolour is exactly how it would go unnoticed.
    UnknownId(FeatureId),
    /// An update, or a removal, named a source that does not exist.
    ///
    /// Separate from `UnknownId` because the caller's recovery differs: an
    /// unknown id means the collection they were holding ids from has been
    /// replaced, an unknown source means they have not pushed that source
    /// yet (or have removed it). Both are loud for the same reason -- a
    /// silent no-op is how a typo in a source name becomes a layer that
    /// never appears and nobody can explain.
    UnknownSource(String),
    /// A source's declared paint EXPRESSIONS could not be compiled --
    /// `declare_source_paint` with an unsupported property, a malformed
    /// colour, or an expression that varies with zoom.
    ///
    /// Separate from `Feature` because no feature is at fault: this is a
    /// declaration about the source, made before (or independently of) any
    /// push, so there is no index or id to name. See
    /// `overlay::source_paint::SourcePaint::compile`.
    SourcePaint { source: String, reason: String },
}

impl std::fmt::Display for FeatureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(message) => write!(f, "not valid JSON: {message}"),
            Self::NotAFeatureCollection => {
                write!(f, "expected a GeoJSON FeatureCollection")
            }
            Self::Feature { index, id, reason } => match id {
                Some(FeatureId::String(id)) => {
                    write!(f, "feature {index} (id {id:?}): {reason}")
                }
                Some(FeatureId::Number(id)) => write!(f, "feature {index} (id {id}): {reason}"),
                None => write!(f, "feature {index}: {reason}"),
            },
            Self::UnknownId(FeatureId::String(id)) => {
                write!(f, "no feature with id {id:?}")
            }
            Self::UnknownId(FeatureId::Number(id)) => write!(f, "no feature with id {id}"),
            Self::UnknownSource(name) => write!(f, "no overlay source named {name:?}"),
            Self::SourcePaint { source, reason } => {
                write!(f, "source {source:?} paint declaration: {reason}")
            }
        }
    }
}

impl std::error::Error for FeatureError {}

/// Parses a whole collection, or fails without producing any of it.
///
/// All-or-nothing because the caller's next move on a rejection is to fix
/// their data and push again; half a fleet on the map is worse than none.
pub fn parse_feature_collection(json: &str) -> Result<Vec<ParsedFeature>, FeatureError> {
    let value: Value =
        serde_json::from_str(json).map_err(|e| FeatureError::Json(e.to_string()))?;
    if value.get("type").and_then(Value::as_str) != Some("FeatureCollection") {
        return Err(FeatureError::NotAFeatureCollection);
    }
    let features = value
        .get("features")
        .and_then(Value::as_array)
        .ok_or(FeatureError::NotAFeatureCollection)?;

    let mut parsed = Vec::with_capacity(features.len());
    for (index, feature) in features.iter().enumerate() {
        let id = parse_id(feature.get("id"));
        let fail = |reason: String| FeatureError::Feature { index, id: id.clone(), reason };

        let geometry = parse_geometry(feature.get("geometry")).map_err(fail)?;
        let properties = feature
            .get("properties")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        parsed.push(ParsedFeature { id, geometry, properties });
    }
    Ok(parsed)
}

fn parse_id(value: Option<&Value>) -> Option<FeatureId> {
    match value? {
        Value::String(s) => Some(FeatureId::String(s.clone())),
        Value::Number(n) => n.as_f64().map(FeatureId::Number),
        _ => None,
    }
}

pub(crate) fn parse_geometry(value: Option<&Value>) -> Result<OverlayGeometry, String> {
    let Some(value) = value else { return Ok(OverlayGeometry::Empty) };
    if value.is_null() {
        return Ok(OverlayGeometry::Empty);
    }
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| "geometry has no \"type\"".to_string())?;
    let coordinates = value
        .get("coordinates")
        .ok_or_else(|| format!("{kind} geometry has no \"coordinates\""))?;

    match kind {
        "Point" => Ok(OverlayGeometry::Point(vec![position(coordinates)?])),
        "MultiPoint" => Ok(OverlayGeometry::Point(list(coordinates, position)?)),
        "LineString" => Ok(OverlayGeometry::Line(vec![list(coordinates, position)?])),
        "MultiLineString" => {
            Ok(OverlayGeometry::Line(list(coordinates, |v| list(v, position))?))
        }
        "Polygon" => {
            Ok(OverlayGeometry::Polygon(vec![list(coordinates, |v| list(v, position))?]))
        }
        "MultiPolygon" => Ok(OverlayGeometry::Polygon(list(coordinates, |v| {
            list(v, |v| list(v, position))
        })?)),
        other => Err(format!("unsupported geometry type {other:?}")),
    }
}

fn list<T>(value: &Value, each: impl Fn(&Value) -> Result<T, String>) -> Result<Vec<T>, String> {
    value
        .as_array()
        .ok_or_else(|| "expected an array of coordinates".to_string())?
        .iter()
        .map(each)
        .collect()
}

/// One `[lon, lat]`. A third element (GeoJSON permits elevation) is ignored
/// rather than refused -- it is valid GeoJSON that this renderer has no use
/// for, and rejecting it would turn working data away.
fn position(value: &Value) -> Result<(f64, f64), String> {
    let array = value.as_array().ok_or_else(|| "position is not an array".to_string())?;
    if array.len() < 2 {
        return Err(format!("position needs at least [lon, lat], got {} value(s)", array.len()));
    }
    let number = |v: &Value| v.as_f64().ok_or_else(|| "position value is not a number".to_string());
    Ok((number(&array[0])?, number(&array[1])?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collection(features: &str) -> String {
        format!(r#"{{"type":"FeatureCollection","features":[{features}]}}"#)
    }

    #[test]
    fn an_empty_collection_parses_to_no_features() {
        let parsed = parse_feature_collection(&collection("")).unwrap();
        assert!(parsed.is_empty());
    }

    #[test]
    fn a_line_string_becomes_a_one_part_line() {
        let json = collection(
            r##"{"type":"Feature","id":"route-42","geometry":{"type":"LineString","coordinates":[[103.85,1.29],[103.86,1.30]]},"properties":{"line-color":"#2b7fff"}}"##,
        );
        let parsed = parse_feature_collection(&json).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].id, Some(FeatureId::String("route-42".into())));
        match &parsed[0].geometry {
            OverlayGeometry::Line(parts) => {
                assert_eq!(parts.len(), 1);
                assert_eq!(parts[0], vec![(103.85, 1.29), (103.86, 1.30)]);
            }
            other => panic!("expected a line, got {other:?}"),
        }
        assert_eq!(parsed[0].properties.get("line-color").unwrap(), "#2b7fff");
    }

    /// The Multi form collapses into the same shape, so downstream code has
    /// one case rather than two.
    #[test]
    fn a_multi_line_string_becomes_a_many_part_line() {
        let json = collection(
            r#"{"type":"Feature","geometry":{"type":"MultiLineString","coordinates":[[[0,0],[1,1]],[[2,2],[3,3]]]},"properties":{}}"#,
        );
        let parsed = parse_feature_collection(&json).unwrap();
        match &parsed[0].geometry {
            OverlayGeometry::Line(parts) => assert_eq!(parts.len(), 2),
            other => panic!("expected a line, got {other:?}"),
        }
    }

    /// A polygon's rings are kept: the first is the exterior, the rest are
    /// holes, and a fill that dropped them would paint over its own holes.
    #[test]
    fn a_polygon_keeps_its_holes() {
        let json = collection(
            r#"{"type":"Feature","geometry":{"type":"Polygon","coordinates":[[[0,0],[4,0],[4,4],[0,4],[0,0]],[[1,1],[2,1],[2,2],[1,2],[1,1]]]},"properties":{}}"#,
        );
        let parsed = parse_feature_collection(&json).unwrap();
        match &parsed[0].geometry {
            OverlayGeometry::Polygon(polys) => {
                assert_eq!(polys.len(), 1);
                assert_eq!(polys[0].len(), 2, "exterior plus one hole");
            }
            other => panic!("expected a polygon, got {other:?}"),
        }
    }

    #[test]
    fn a_point_and_a_multipoint_both_become_points() {
        let single = collection(
            r#"{"type":"Feature","geometry":{"type":"Point","coordinates":[103.85,1.29]},"properties":{}}"#,
        );
        let multi = collection(
            r#"{"type":"Feature","geometry":{"type":"MultiPoint","coordinates":[[1,1],[2,2]]},"properties":{}}"#,
        );
        match &parse_feature_collection(&single).unwrap()[0].geometry {
            OverlayGeometry::Point(points) => assert_eq!(points.len(), 1),
            other => panic!("expected points, got {other:?}"),
        }
        match &parse_feature_collection(&multi).unwrap()[0].geometry {
            OverlayGeometry::Point(points) => assert_eq!(points.len(), 2),
            other => panic!("expected points, got {other:?}"),
        }
    }

    /// GeoJSON allows a null geometry. Such a feature is kept and still
    /// carries its label; it is NOT hit-testable, since nothing of it is on
    /// screen to point at (`hit::tests::an_empty_geometry_is_never_hit`).
    #[test]
    fn a_null_geometry_parses_as_empty_rather_than_failing() {
        let json = collection(r#"{"type":"Feature","geometry":null,"properties":{"text-field":"42"}}"#);
        let parsed = parse_feature_collection(&json).unwrap();
        assert_eq!(parsed[0].geometry, OverlayGeometry::Empty);
    }

    #[test]
    fn a_numeric_id_is_kept_as_a_number() {
        let json = collection(r#"{"type":"Feature","id":42,"geometry":null,"properties":{}}"#);
        let parsed = parse_feature_collection(&json).unwrap();
        assert_eq!(parsed[0].id, Some(FeatureId::Number(42.0)));
    }

    #[test]
    fn a_feature_with_no_id_parses_with_none() {
        let json = collection(r#"{"type":"Feature","geometry":null,"properties":{}}"#);
        assert_eq!(parse_feature_collection(&json).unwrap()[0].id, None);
    }

    #[test]
    fn malformed_json_is_a_json_error() {
        assert!(matches!(
            parse_feature_collection("{not json"),
            Err(FeatureError::Json(_))
        ));
    }

    #[test]
    fn a_bare_feature_is_not_a_collection() {
        let json = r#"{"type":"Feature","geometry":null,"properties":{}}"#;
        assert!(matches!(
            parse_feature_collection(json),
            Err(FeatureError::NotAFeatureCollection)
        ));
    }

    /// A rejected feature is named by index AND by id -- a caller pushing a
    /// hundred vehicles needs to know which one was refused.
    #[test]
    fn an_unknown_geometry_type_names_the_offending_feature() {
        let json = collection(
            r#"{"type":"Feature","id":"bad-one","geometry":{"type":"GeometryCollection","geometries":[]},"properties":{}}"#,
        );
        match parse_feature_collection(&json) {
            Err(FeatureError::Feature { index, id, reason }) => {
                assert_eq!(index, 0);
                assert_eq!(id, Some(FeatureId::String("bad-one".into())));
                assert!(reason.contains("GeometryCollection"), "reason was {reason}");
            }
            other => panic!("expected a feature error, got {other:?}"),
        }
    }

    /// A coordinate that is not two numbers is a rejection, not a silently
    /// dropped point that would shorten a route without saying so.
    #[test]
    fn a_malformed_coordinate_is_rejected() {
        let json = collection(
            r#"{"type":"Feature","geometry":{"type":"LineString","coordinates":[[1],[2,2]]},"properties":{}}"#,
        );
        assert!(matches!(
            parse_feature_collection(&json),
            Err(FeatureError::Feature { .. })
        ));
    }

    #[test]
    fn a_missing_properties_object_is_treated_as_empty() {
        let json = collection(r#"{"type":"Feature","geometry":null}"#);
        assert!(parse_feature_collection(&json).unwrap()[0].properties.is_empty());
    }
}

//! Features handed in as Rust values rather than as a GeoJSON string.
//!
//! See `docs/superpowers/specs/2026-08-28-structured-overlay-input-design.md`.
//! `gpui-maps` is compiled into its consumer's binary -- same process, same
//! address space -- so a caller holding structured data and calling
//! `set_features` pays a serialise/deserialise round trip across a boundary
//! that does not exist. On this crate's own 28.8 MB parity dataset that
//! round trip is 74% of the synchronous cost of a push.
//!
//! What is here is only the SHAPE of the input and its conversion into
//! `OverlayGeometry`. Everything past that -- the bracket, the plan, the hit
//! index -- is `OverlayStore::replace`'s own code, shared rather than
//! reimplemented, because two paths that drift apart is the one failure this
//! design can actually suffer.

use super::geojson::{FeatureId, OverlayGeometry};
use super::paint::FeaturePaint;

/// Geometry handed in by a caller, borrowed.
///
/// Mirrors `OverlayGeometry`'s shape exactly -- singular and `Multi` forms
/// collapsed together -- so there is no second vocabulary to learn and no
/// case downstream has to be taught about.
///
/// `[f64; 2]` rather than `(f64, f64)`: it is GeoJSON's own position shape,
/// it is what a caller already holds, and Rust guarantees its layout where
/// it does not guarantee a tuple's. The order is `[lon, lat]`, GeoJSON's
/// order and this crate's, everywhere.
///
/// Borrowed rather than owned for one concrete reason: the known consumer's
/// route fans out into a casing line and a core line SHARING the identical
/// coordinates. Owned would copy them twice; borrowed makes the duplicate
/// free. The input is read synchronously, before any spawn, so the borrow
/// never outlives the call.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GeometryInput<'a> {
    /// One or more `[lon, lat]` positions.
    Point(&'a [[f64; 2]]),
    /// One or more line parts, each a run of positions.
    Line(&'a [&'a [[f64; 2]]]),
    /// One or more polygons, each a list of rings: exterior first, then
    /// holes.
    Polygon(&'a [&'a [&'a [[f64; 2]]]]),
    /// Draws nothing, but still carries a label. Exactly GeoJSON `null`
    /// geometry and `OverlayGeometry::Empty`, down to being dropped by
    /// `hit::HitIndex::build`.
    Empty,
}

impl GeometryInput<'_> {
    /// Reads the borrowed positions into the owned form the store keeps.
    ///
    /// This is the one copy the structured path pays, and it is why the
    /// spec says "no serialisation" rather than "zero copy":
    /// `OverlayGeometry` holds `Vec<Vec<(f64, f64)>>`, and borrowing all
    /// the way down would mean threading a lifetime through `OverlayStore`,
    /// `OverlayPlan` and `HitIndex` for the 1-2 ms this costs on 414k
    /// coordinates.
    pub(crate) fn to_geometry(self) -> OverlayGeometry {
        match self {
            Self::Point(positions) => OverlayGeometry::Point(positions.iter().map(pair).collect()),
            Self::Line(parts) => {
                OverlayGeometry::Line(parts.iter().map(|part| part.iter().map(pair).collect()).collect())
            }
            Self::Polygon(polygons) => OverlayGeometry::Polygon(
                polygons
                    .iter()
                    .map(|rings| rings.iter().map(|ring| ring.iter().map(pair).collect()).collect())
                    .collect(),
            ),
            Self::Empty => OverlayGeometry::Empty,
        }
    }
}

impl GeometryInput<'_> {
    /// Refuses a coordinate that is not a real number.
    ///
    /// THE ONE HAZARD THIS DOOR OPENS THAT THE OTHER DOES NOT. JSON has no
    /// `Infinity` and no `NaN` literal, so `parse_feature_collection` can
    /// never produce one and nothing downstream was ever written to expect
    /// one. `[f64; 2]` can hold both.
    ///
    /// The consequence is not local to the offending feature.
    /// `OverlayOrigin::centred_on` takes the bounding box of every
    /// coordinate in the whole COLLECTION, so one infinite longitude makes
    /// `origin.world_x` infinite; `OverlayOrigin::to_local` then computes
    /// `value - origin` as `NaN` for every point of every feature, and
    /// `NaN as i32` saturates to 0. One bad coordinate in one feature
    /// collapses the entire overlay onto a single line, with no error --
    /// which is exactly the "looks like a working map" failure this crate
    /// refuses everywhere else.
    ///
    /// Costs one `is_finite` per coordinate, measured at well under a
    /// millisecond over the 415k coordinates of `geojsons/simulation.json`
    /// (see the benchmark in `overlay::tests`) against the ~14 ms the whole
    /// structured push takes. Cheap enough not to be a trade.
    pub(crate) fn validate(&self) -> Result<(), String> {
        let check = |positions: &[[f64; 2]]| -> Result<(), String> {
            for position in positions {
                if !position[0].is_finite() || !position[1].is_finite() {
                    return Err(format!(
                        "position [{}, {}] is not finite -- every coordinate must be a real number",
                        position[0], position[1],
                    ));
                }
            }
            Ok(())
        };
        match self {
            Self::Point(positions) => check(positions),
            Self::Line(parts) => parts.iter().try_for_each(|part| check(part)),
            Self::Polygon(polygons) => polygons
                .iter()
                .try_for_each(|rings| rings.iter().try_for_each(|ring| check(ring))),
            Self::Empty => Ok(()),
        }
    }
}

/// `[lon, lat]` as `(lon, lat)`.
///
/// A free function rather than three inline closures so the order is
/// written down exactly ONCE. Getting it backwards puts every feature in
/// the wrong hemisphere while every round-trip test still passes.
fn pair(position: &[f64; 2]) -> (f64, f64) {
    (position[0], position[1])
}

/// One feature, as a caller hands it in.
///
/// `paint` is a resolved `FeaturePaint`, so `FeaturePaint::from_properties`
/// does not run at all -- a second saving beyond the parse. It is also the
/// reason `FeaturePaint::validate` exists: the rules `from_properties`
/// enforces on its way in are the same rules this path skips.
///
/// `id` is `Option`, matching GeoJSON (where it is optional) and
/// `ParsedFeature`. Requiring it would turn callers away who have none, and
/// it is what `MapView::set_feature_paint` addresses a feature by.
#[derive(Debug, Clone, PartialEq)]
pub struct FeatureInput<'a> {
    pub id: Option<FeatureId>,
    pub geometry: GeometryInput<'a>,
    pub paint: FeaturePaint,
}

/// One feature of a source whose paint is an EXPRESSION over feature
/// properties -- `MapView::set_source_data_from`'s input.
///
/// # Why this composes `FeatureInput` instead of extending it
///
/// A `properties` field on `FeatureInput` itself would have been the
/// smaller type and a BREAKING change: `FeatureInput { id, geometry, paint }`
/// is a struct literal, three out-of-repo consumers write it, and a fourth
/// field makes every one of those a compile error. Composing leaves that
/// struct exactly as it is, so `set_source_from` and `set_features_from`
/// keep their signatures and a caller adopting data-driven paint wraps the
/// inputs they already build rather than rewriting them.
///
/// It is also the honest shape. A feature still has its own resolved paint
/// -- its width, its dot radius, its label, its `min-zoom` -- and the
/// source's expressions override only the colours they name. So this really
/// is "a feature, plus the properties its source reads", not a different
/// kind of feature.
///
/// # `Arc`, and what the store keeps
///
/// The `Arc` is the long-standing signature and stays. The store does NOT
/// keep the map: a push reads every map once into the source's columnar
/// `PropertyTable` (see `overlay::columns`), which is what a zoom-bucket
/// crossing or a re-class reads afterwards. A caller that has its
/// properties as columns already can skip the maps entirely with
/// `OverlayStore::set_source_columns`.
#[derive(Debug, Clone, PartialEq)]
pub struct DataFeatureInput<'a> {
    pub feature: FeatureInput<'a>,
    /// Cheap to share: [`super::source_paint::no_properties`] is the
    /// process-wide empty map for a feature that has none.
    pub properties: std::sync::Arc<super::source_paint::FeatureProperties>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// JSON has no `Infinity` and no `NaN`, so the string path cannot
    /// receive one; `[f64; 2]` can. This is the one hazard the structured
    /// door opens that the other does not have.
    ///
    /// It is not a cosmetic one. `OverlayOrigin::centred_on` takes the
    /// bounding box of every coordinate in the COLLECTION, so a single
    /// infinite longitude in one feature makes `origin.world_x` infinite,
    /// and `to_local`'s `value - origin` is then `NaN` for every point of
    /// every feature -- which saturates to 0, collapsing the entire overlay
    /// onto one line. One bad coordinate, a whole map silently wrong.
    #[test]
    fn a_non_finite_coordinate_is_rejected() {
        let bad: &[[f64; 2]] = &[[f64::INFINITY, 1.29], [103.86, 1.30]];
        let parts = [bad];
        let error = GeometryInput::Line(&parts).validate().unwrap_err();
        assert!(error.contains("finite"), "error was {error}");

        assert!(GeometryInput::Point(&[[f64::NAN, 1.29]]).validate().is_err());

        let ring: &[[f64; 2]] = &[[0.0, f64::NEG_INFINITY], [1.0, 0.0], [1.0, 1.0], [0.0, 0.0]];
        let rings: &[&[[f64; 2]]] = &[ring];
        let polygons = [rings];
        assert!(GeometryInput::Polygon(&polygons).validate().is_err());
    }

    /// The error says WHICH position, because a caller pushing a route of
    /// four thousand coordinates cannot find it otherwise.
    #[test]
    fn a_non_finite_coordinate_names_itself() {
        let bad: &[[f64; 2]] = &[[103.85, 1.29], [f64::NAN, 1.30]];
        let parts = [bad];
        let error = GeometryInput::Line(&parts).validate().unwrap_err();
        assert!(error.contains("NaN"), "error was {error}");
    }

    #[test]
    fn ordinary_coordinates_and_empty_geometry_validate() {
        let line: &[[f64; 2]] = &[[103.85, 1.29], [103.86, 1.30]];
        let parts = [line];
        GeometryInput::Line(&parts).validate().unwrap();
        GeometryInput::Point(&[[103.85, 1.29]]).validate().unwrap();
        GeometryInput::Empty.validate().unwrap();
        GeometryInput::Point(&[]).validate().unwrap();
    }

    /// Raffles Place, Singapore: longitude 103.85, latitude 1.29. Asserted
    /// against the REAL coordinate rather than against a round trip,
    /// because a swap survives any round trip that made the same swap
    /// twice. Latitude 103.85 does not exist; longitude 1.29 is the Gulf of
    /// Guinea. Only one of the two readings is a place in Singapore.
    #[test]
    fn a_position_keeps_lon_lat_order() {
        let positions = [[103.85, 1.29]];
        let geometry = GeometryInput::Point(&positions).to_geometry();
        match geometry {
            OverlayGeometry::Point(points) => {
                assert_eq!(points, vec![(103.85, 1.29)]);
                let (lon, lat) = points[0];
                assert!(lon > 100.0, "longitude first: {lon}");
                assert!(lat.abs() < 10.0, "latitude second: {lat}");
            }
            other => panic!("expected points, got {other:?}"),
        }
    }

    /// One point and many points are the same variant, exactly as `Point`
    /// and `MultiPoint` collapse together on the string path.
    #[test]
    fn one_position_and_many_are_the_same_variant() {
        let many = [[103.85, 1.29], [103.86, 1.30]];
        match GeometryInput::Point(&many).to_geometry() {
            OverlayGeometry::Point(points) => assert_eq!(points.len(), 2),
            other => panic!("expected points, got {other:?}"),
        }
    }

    #[test]
    fn a_one_part_line_keeps_its_positions_in_order() {
        let part: &[[f64; 2]] = &[[103.85, 1.29], [103.86, 1.30]];
        let parts = [part];
        match GeometryInput::Line(&parts).to_geometry() {
            OverlayGeometry::Line(parts) => {
                assert_eq!(parts, vec![vec![(103.85, 1.29), (103.86, 1.30)]]);
            }
            other => panic!("expected a line, got {other:?}"),
        }
    }

    #[test]
    fn a_many_part_line_keeps_every_part() {
        let first: &[[f64; 2]] = &[[0.0, 0.0], [1.0, 1.0]];
        let second: &[[f64; 2]] = &[[2.0, 2.0], [3.0, 3.0]];
        let parts = [first, second];
        match GeometryInput::Line(&parts).to_geometry() {
            OverlayGeometry::Line(parts) => {
                assert_eq!(parts.len(), 2);
                assert_eq!(parts[1][0], (2.0, 2.0));
            }
            other => panic!("expected a line, got {other:?}"),
        }
    }

    /// Exterior first, then holes -- the ring order `OverlayGeometry` and
    /// the tessellator both assume. A polygon whose rings arrive the other
    /// way round paints over its own hole.
    #[test]
    fn a_polygon_keeps_its_exterior_first_and_then_its_holes() {
        let exterior: &[[f64; 2]] =
            &[[0.0, 0.0], [4.0, 0.0], [4.0, 4.0], [0.0, 4.0], [0.0, 0.0]];
        let hole: &[[f64; 2]] = &[[1.0, 1.0], [2.0, 1.0], [2.0, 2.0], [1.0, 2.0], [1.0, 1.0]];
        let rings: &[&[[f64; 2]]] = &[exterior, hole];
        let polygons = [rings];
        match GeometryInput::Polygon(&polygons).to_geometry() {
            OverlayGeometry::Polygon(polygons) => {
                assert_eq!(polygons.len(), 1);
                assert_eq!(polygons[0].len(), 2, "exterior plus one hole");
                assert_eq!(polygons[0][0][1], (4.0, 0.0), "the exterior comes first");
                assert_eq!(polygons[0][1][0], (1.0, 1.0), "the hole comes second");
            }
            other => panic!("expected a polygon, got {other:?}"),
        }
    }

    #[test]
    fn a_multi_polygon_keeps_every_polygon() {
        let one: &[[f64; 2]] = &[[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 0.0]];
        let two: &[[f64; 2]] = &[[5.0, 5.0], [6.0, 5.0], [6.0, 6.0], [5.0, 5.0]];
        let first: &[&[[f64; 2]]] = &[one];
        let second: &[&[[f64; 2]]] = &[two];
        let polygons = [first, second];
        match GeometryInput::Polygon(&polygons).to_geometry() {
            OverlayGeometry::Polygon(polygons) => assert_eq!(polygons.len(), 2),
            other => panic!("expected a polygon, got {other:?}"),
        }
    }

    #[test]
    fn empty_converts_to_empty() {
        assert_eq!(GeometryInput::Empty.to_geometry(), OverlayGeometry::Empty);
    }

    /// A `Point` with no positions is not the same thing as `Empty`: the
    /// string path's `{"type":"MultiPoint","coordinates":[]}` produces
    /// `Point(vec![])`, which `build_plan` skips and `HitIndex` drops
    /// through a different route. Keeping the distinction keeps the two
    /// paths' feature vectors identical.
    #[test]
    fn an_empty_position_list_is_still_its_own_variant() {
        assert_eq!(GeometryInput::Point(&[]).to_geometry(), OverlayGeometry::Point(vec![]));
        assert_eq!(GeometryInput::Line(&[]).to_geometry(), OverlayGeometry::Line(vec![]));
        assert_eq!(GeometryInput::Polygon(&[]).to_geometry(), OverlayGeometry::Polygon(vec![]));
    }
}

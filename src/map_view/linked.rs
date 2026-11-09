//! What a linked chart needs from the map, beyond the pick and the range
//! filter: the column it charts, and "light every feature in this bar".
//!
//! - [`MapView::source_column`]: one property of every feature a source
//!   holds, as pushed -- the values a histogram bins or a bar chart
//!   counts. A host that loaded the source through the library (CSV,
//!   FlatGeobuf, GeoParquet, parsed off the UI thread) has no other copy.
//! - [`MapView::set_highlight_where`]: every drawn feature whose property
//!   passes a test, highlighted at once -- the hovered bar's features.
//!   [`MapView::set_highlight`] lights ONE object (the hovered one); a bar
//!   or a bin stands for many. It is that same highlight (outline, casing,
//!   fill tint or point ring, drawn right after its source), built over
//!   every member as one mesh, kept per source beside the single one, and
//!   rebuilt when the source draws a new generation (a point source's next
//!   view after a pan), so it follows the data rather than a snapshot of
//!   it. vega-lite's `selection` + conditional encoding, or MapLibre's
//!   `setFeatureState` over a query's results, as one call.

use std::sync::Arc;

use super::{next_highlight_stamp, FeatureHighlight, HighlightMesh, MapView};
use crate::mvt::TagValue;
use crate::overlay::{FeatureRef, HighlightStyle, HighlightWhere, OverlayGeometry};

impl MapView {
    /// At most this many features light in one [`Self::set_highlight_where`]:
    /// the mesh is tessellated on the calling thread, and a bar of tens of
    /// thousands of polygons would stall it. Past this, the first this many
    /// in feature order light and [`Self::highlight_where`] reports both
    /// counts.
    pub const HIGHLIGHT_WHERE_MAX: usize = 4_000;

    /// Property `field` of every feature of source `name`, in push order
    /// (`None` where a feature has none). For a point source, every pushed
    /// point; for any other source, every feature of its current push.
    /// `None` when there is no such source or column -- a GeoJSON source
    /// keeps its properties only when its paint reads them
    /// ([`Self::declare_source_paint`]) or it was pushed as columns.
    #[must_use]
    pub fn source_column(&self, name: &str, field: &str) -> Option<Vec<Option<TagValue>>> {
        let (properties, rows) = match self.overlay_store.source_point_data(name) {
            Some(data) => (Arc::clone(&data.properties), data.len()),
            None => {
                let snapshot = self.overlay_store.source_paint_snapshot(name)?;
                let rows = snapshot.features.len();
                (snapshot.properties, rows)
            }
        };
        let column = properties.column(field)?;
        Some((0..rows).map(|row| properties.cell(column, row).map(crate::overlay::columns::Cell::to_tag)).collect())
    }

    /// Highlights every drawn feature of source `name` whose property passes
    /// `predicate`, in `style` -- or, with `None`, clears that source's
    /// group highlight. One group per source; a new one replaces the old.
    /// It sits beside [`Self::set_highlight`]'s single object (hovering a
    /// map object while a bar is lit draws both). Points a range filter
    /// hides are not lit.
    ///
    /// Returns how many features are lit now. Asking again for what is
    /// already lit, in the same style, changes nothing and asks for no
    /// frame.
    pub fn set_highlight_where(
        &mut self,
        name: &str,
        predicate: Option<HighlightWhere>,
        style: HighlightStyle,
        cx: &mut gpui::Context<Self>,
    ) -> usize {
        let same = self.where_highlight(name).map(|h| (h.predicate.as_deref(), h.style)) == Some((predicate.as_ref(), style));
        if same || (predicate.is_none() && self.where_highlight(name).is_none()) {
            return self.where_highlight(name).map_or(0, |h| h.members);
        }
        let lit = self.set_highlight_where_deferred(name, predicate, style);
        cx.notify();
        lit
    }

    /// [`Self::set_highlight_where`] without the `Context`: the host must
    /// `cx.notify()` for the change to reach the screen.
    pub fn set_highlight_where_deferred(&mut self, name: &str, predicate: Option<HighlightWhere>, style: HighlightStyle) -> usize {
        self.feature_highlights.retain(|h| &*h.source != name || h.predicate.is_none());
        self.frame_dirty = true;
        let Some(predicate) = predicate else { return 0 };
        let (generation, mesh, members) = self.build_where_highlight(name, &predicate, &style).unwrap_or((0, None, 0));
        self.feature_highlights.push(FeatureHighlight {
            source: Arc::from(name),
            feature: FeatureRef { source: Arc::from(name), index: 0, id: None },
            style,
            generation,
            stamp: next_highlight_stamp(),
            mesh,
            predicate: Some(Arc::new(predicate)),
            members,
        });
        members
    }

    /// Source `name`'s group highlight and how many features it lights.
    #[must_use]
    pub fn highlight_where(&self, name: &str) -> Option<(&HighlightWhere, usize)> {
        let h = self.where_highlight(name)?;
        Some((h.predicate.as_deref()?, h.members))
    }

    fn where_highlight(&self, name: &str) -> Option<&FeatureHighlight> {
        self.feature_highlights.iter().find(|h| &*h.source == name && h.predicate.is_some())
    }

    /// The group mesh over `name`'s DRAWN generation, that generation, and
    /// how many features it lights. `None` when the source draws nothing;
    /// a `None` mesh when no drawn feature passes.
    pub(super) fn build_where_highlight(
        &self,
        name: &str,
        predicate: &HighlightWhere,
        style: &HighlightStyle,
    ) -> Option<(u64, Option<HighlightMesh>, usize)> {
        let snapshot = &self.overlay_drawn_source(name)?.snapshot;
        let keeps = self.point_filter_keeps(name);
        let column = snapshot.properties.column(&predicate.field);
        let mut members: Vec<(OverlayGeometry, Option<crate::overlay::FeaturePaint>)> = Vec::new();
        if let Some(column) = column {
            for index in 0..snapshot.features.len() {
                if members.len() >= Self::HIGHLIGHT_WHERE_MAX {
                    break;
                }
                let value = snapshot.properties.cell(column, index).map(crate::overlay::columns::Cell::to_tag);
                if !predicate.matches(value.as_ref()) || keeps.as_ref().is_some_and(|k| !k(index)) {
                    continue;
                }
                let Some(geometry) = crate::overlay::snapshot_geometry(snapshot, index) else { continue };
                members.push((geometry, crate::overlay::drawn_paint(snapshot, index)));
            }
        }
        let lit = members.len();
        let mesh = crate::overlay::group_highlight_plan(&members, style).and_then(|plan| {
            let (mesh, _ranges, extras) = super::tessellate_overlay(&plan, &|| false, crate::render::TileMesh::default()).ok()?;
            (!mesh.indices.is_empty()).then_some(HighlightMesh { origin: plan.origin, mesh, extras, bracket: plan.bracket })
        });
        Some((snapshot.generation, mesh, lit))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::mvt::TagValue;
    use crate::overlay::{HighlightStyle, HighlightWhere};

    fn n(v: f64) -> TagValue {
        TagValue::Number(v)
    }

    #[test]
    fn a_range_and_a_set_test_the_value_they_name() {
        let bin = HighlightWhere::range("pop", [10.0, 20.0]);
        assert!(bin.matches(Some(&n(10.0))) && bin.matches(Some(&n(20.0))), "closed at both ends");
        assert!(!bin.matches(Some(&n(20.5))) && !bin.matches(None) && !bin.matches(Some(&TagValue::String("15".into()))));
        let bar = HighlightWhere::one_of("region", vec![TagValue::String("East".into()), n(3.0)]);
        assert!(bar.matches(Some(&TagValue::String("East".into()))) && bar.matches(Some(&n(3.0))));
        assert!(!bar.matches(Some(&TagValue::String("West".into()))) && !bar.matches(Some(&n(4.0))));
    }

    /// Hovering a bar lights every drawn feature in it, at once; clearing
    /// it leaves nothing lit; the column reads back as pushed.
    #[test]
    fn a_predicate_lights_every_member_and_only_them() {
        use super::super::{tessellate_overlay, DrawnOverlayMesh, MapView};
        let rt = tokio::runtime::Runtime::new().expect("a test runtime");
        let mut view = MapView::new(
            2.5,
            0.5,
            6.0,
            800.0,
            600.0,
            crate::style::Style { layers: vec![], sprite: None, sky: Default::default() },
            Arc::new(crate::tile_source::EmptyTileSource),
            rt.handle().clone(),
        );
        let json = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","properties":{"pop":5,"kind":"a"},"geometry":{"type":"Polygon","coordinates":[[[0,0],[1,0],[1,1],[0,1],[0,0]]]}},
            {"type":"Feature","properties":{"pop":15,"kind":"b"},"geometry":{"type":"Polygon","coordinates":[[[2,0],[3,0],[3,1],[2,1],[2,0]]]}},
            {"type":"Feature","properties":{"pop":18,"kind":"b"},"geometry":{"type":"Polygon","coordinates":[[[4,0],[5,0],[5,1],[4,1],[4,0]]]}}]}"#;
        // A source keeps its properties only when its paint reads them.
        let paint = serde_json::json!({"fill-color": ["case", [">", ["get", "pop"], 10], "#e4572e", "#2e86ab"]});
        view.declare_source_paint("t", paint.as_object().unwrap()).unwrap();
        view.overlay_store.set_source("t", json, view.display_zoom()).unwrap();
        let generation = view.overlay_store.source_generation("t").unwrap();
        let plan = view.overlay_store.source_plan("t").expect("draws").clone();
        let (mesh, ranges, _) = tessellate_overlay(&plan, &|| false, Default::default()).expect("uncancellable");
        view.install_overlay_mesh(&"t".into(), generation, DrawnOverlayMesh::new(plan.origin, mesh, ranges, plan.bracket));

        assert_eq!(view.source_column("t", "pop").unwrap(), vec![Some(n(5.0)), Some(n(15.0)), Some(n(18.0))]);
        assert_eq!(view.source_column("t", "nope"), None);
        let style = HighlightStyle::default();
        assert_eq!(view.set_highlight_where_deferred("t", Some(HighlightWhere::range("pop", [10.0, 20.0])), style), 2, "the two in the bin");
        assert_eq!(view.highlight_where("t").map(|(_, n)| n), Some(2));
        assert!(view.where_highlight("t").and_then(|h| h.mesh.as_ref()).is_some(), "one mesh for both");
        let one = HighlightWhere::one_of("kind", vec![TagValue::String("a".into())]);
        assert_eq!(view.set_highlight_where_deferred("t", Some(one), style), 1, "a new group replaces the last");
        assert_eq!(view.feature_highlights.len(), 1);
        assert_eq!(view.set_highlight_where_deferred("t", Some(HighlightWhere::range("pop", [100.0, 200.0])), style), 0);
        assert!(view.where_highlight("t").is_some_and(|h| h.mesh.is_none()), "asked for, nothing to light");
        assert_eq!(view.set_highlight_where_deferred("t", None, style), 0);
        assert!(view.highlight_where("t").is_none());
    }
}

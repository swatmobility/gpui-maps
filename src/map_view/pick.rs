//! One pick and one highlight for everything a `MapView` draws that can be
//! pointed at: overlay features (fills, lines, and points -- the circle
//! instances of a point source included), arcs, overlay extrusion columns
//! and altitude points.
//!
//! - [`MapView::pick`]: the topmost object under a point, in DRAW order
//!   (what is drawn last is on top), with its properties where its source
//!   has any. MapLibre's `queryRenderedFeatures` for a point, first hit
//!   only.
//! - [`MapView::set_highlight`]: one hovered object, drawn so it stands
//!   out on every kind -- an outline over a fill or line, a ring round a
//!   point, a casing and a brighter core on an arc, the column lit and
//!   grown, an altitude point drawn last and larger. MapLibre's
//!   feature-state hover, one object at a time.
//!
//! Per-kind queries stay: [`MapView::feature_at`], [`MapView::arc_at`],
//! [`MapView::extrusion_feature_at`], [`MapView::altitude_point_at`]; and
//! per-kind highlights: [`MapView::set_feature_highlight`] (one per
//! source) and [`MapView::set_altitude_emphasis`] (hover, selection and
//! halo). `set_highlight` drives them, so a host may mix the two -- but a
//! per-kind call does not move `set_highlight`'s record of what it lit.

use std::sync::Arc;

use super::MapView;
use crate::overlay::{FeatureProperties, FeatureRef, HighlightStyle};

/// Something a `MapView` draws that can be picked and highlighted.
#[derive(Debug, Clone, PartialEq)]
pub enum MapObject {
    /// An overlay feature (`set_source` and friends): a fill, a line, or a
    /// point -- a point source's circle instances are features too.
    Feature(FeatureRef),
    /// Arc `index` of arc layer `layer` (`set_arc_layer`): its index in the
    /// layer's `ArcSet` (`ArcSet::row` maps it to the input row).
    Arc { layer: String, index: usize },
    /// Feature `feature` of overlay extrusion layer `layer`
    /// (`set_extrusion_layer`): its index in the features `extrude` took.
    Extrusion { layer: String, feature: usize },
    /// Object `index` of altitude layer `layer` (`set_altitude_points`).
    AltitudePoint { layer: String, index: usize },
    /// Trip `trip` of trip layer `layer` (`set_trip_layer`): its index in
    /// the layer's `TripSet::trips` (`TripRange::row` is the input row),
    /// picked by its HEAD at the layer's clock. Not highlighted:
    /// `set_highlight` answers `false` for it.
    Trip { layer: String, trip: usize },
}

/// What [`MapView::pick`] found.
#[derive(Debug, Clone)]
pub struct Pick {
    pub object: MapObject,
    /// The object's properties: an overlay feature's own, an extrusion
    /// feature's or an arc's as the host attached them
    /// (`set_extrusion_layer_properties`, `set_arc_layer_properties`; an
    /// arc's always carry its ends and input row). `None` where there are
    /// none.
    pub properties: Option<Arc<FeatureProperties>>,
}

impl MapView {
    /// Arcs are hit within this many logical pixels of their drawn edge.
    pub const ARC_TOLERANCE_PX: f64 = 3.0;

    /// The ground `(lon, lat)` under a point in the map's own
    /// (viewport-local) logical pixels; `None` above the horizon. What a
    /// host reads a click's place from when it wants the ground, not an
    /// object (a click on a cell of its own lattice, say).
    #[must_use]
    pub fn lon_lat_at(&self, x: f32, y: f32) -> Option<(f64, f64)> {
        let w = crate::render::unproject_to_ground_f64(self.camera(), x, y)?;
        let size = f64::from(crate::render::WORLD_SIZE);
        let n = std::f64::consts::PI * (1.0 - 2.0 * w.y / size);
        Some((w.x / size * 360.0 - 180.0, n.sinh().atan().to_degrees()))
    }

    /// The topmost pickable object under a point in the map's own
    /// (viewport-local) logical pixels, with its properties.
    ///
    /// In draw order, last drawn first: an overlay source drawn above the
    /// labels; then altitude points (globe frames); then trip heads at
    /// their layer's clock ([`Self::trip_at`], drawn right after the arcs);
    /// then arcs (the start of the symbol pass, over every tile and
    /// overlay); then an overlay source drawn above the buildings (its own
    /// pass after the tile pass); then overlay extrusion columns
    /// (depth-tested, so over the ground they stand on); then the other
    /// overlay sources. Within a kind, that kind's own query decides
    /// ([`Self::feature_at`], [`Self::altitude_point_at`],
    /// [`Self::trip_at`], [`Self::arc_at`],
    /// [`Self::extrusion_feature_at`]). Basemap features and dot-density
    /// layers are not pickable.
    ///
    /// A query, not per-frame work: call it from a mouse handler.
    #[must_use]
    pub fn pick(&self, x: f32, y: f32) -> Option<Pick> {
        // `feature_at` already answers topmost-first across sources, so the
        // overlay hit is one feature, in one of three slots.
        let feature = self.feature_at(x, y);
        let above_labels = feature.as_ref().filter(|f| !self.source_below_labels(&f.source));
        // Over the columns only where the last frame drew it there
        // (`render::overlay_order::overlay_pass`), not wherever it may be.
        let above_buildings = feature.as_ref().filter(|f| self.overlay_paint_slot(&f.source) == 1);
        let object = above_labels
            .map(|f| MapObject::Feature(f.clone()))
            .or_else(|| self.altitude_point_at(x, y).map(|(layer, index)| MapObject::AltitudePoint { layer, index }))
            .or_else(|| self.trip_at(x, y).map(|(layer, trip)| MapObject::Trip { layer, trip }))
            .or_else(|| self.arc_at(x, y).map(|(layer, index)| MapObject::Arc { layer, index }))
            .or_else(|| above_buildings.map(|f| MapObject::Feature(f.clone())))
            .or_else(|| self.extrusion_feature_at(x, y).map(|(layer, feature)| MapObject::Extrusion { layer, feature }))
            .or_else(|| feature.map(MapObject::Feature))?;
        let properties = self.object_properties(&object);
        Some(Pick { object, properties })
    }

    /// `object`'s properties, as [`Self::pick`] returns them.
    #[must_use]
    pub fn object_properties(&self, object: &MapObject) -> Option<Arc<FeatureProperties>> {
        match object {
            MapObject::Feature(f) => self.feature_properties(f),
            MapObject::Arc { layer, index } => self.arc_properties(layer, *index),
            MapObject::Extrusion { layer, feature } => self.extrusion_properties(layer, *feature),
            MapObject::AltitudePoint { .. } => None,
            MapObject::Trip { layer, trip } => self.trip_properties(layer, *trip),
        }
    }

    /// Highlights `object` -- the one hovered thing -- or, with `None`,
    /// clears the highlight; a new one replaces the last. Returns whether
    /// a highlight is now drawn (`false` for an object the map is not
    /// drawing). Off by default: a map nobody hovers draws exactly what it
    /// drew before.
    ///
    /// How each kind shows it, over the object and in its own draw slot:
    /// - a fill or line: [`Self::set_feature_highlight`]'s outline (and
    ///   casing, and fill tint) in `style`;
    /// - a point: a ring -- `style`'s casing and outline as discs round the
    ///   point's own circle, the point drawn again on top;
    /// - an arc: a casing `style.outline_width_px` each side in
    ///   `style.outline_color`, and the arc again over it, brightened and
    ///   opaque, over every arc layer;
    /// - an extrusion column: the column again, 1.5 % wider and taller (so
    ///   it encloses the original rather than z-fighting it), its colour
    ///   55 % of the way to `style.outline_color`;
    /// - an altitude point: [`Self::set_altitude_emphasis`]'s hovered slot
    ///   (drawn last and larger), the layer's selection and halo kept.
    ///
    /// Asking again for what is already lit, in the same style, changes
    /// nothing and asks for no frame: a host may call it on every mouse
    /// move without re-extruding a column or re-uploading an arc.
    pub fn set_highlight(&mut self, object: Option<MapObject>, style: HighlightStyle, cx: &mut gpui::Context<Self>) -> bool {
        if let Some(same) = self.unchanged_highlight(object.as_ref(), &style) {
            return same;
        }
        let drawn = self.set_highlight_deferred(object, style);
        cx.notify();
        drawn
    }

    /// `Some(drawn)` when `object` in `style` is what is lit now (or both
    /// are nothing): the call would change nothing.
    fn unchanged_highlight(&self, object: Option<&MapObject>, style: &HighlightStyle) -> Option<bool> {
        match (object, &self.object_highlight) {
            (None, None) => Some(false),
            (Some(o), Some((lit, s))) if o == lit && s == style && self.highlight().is_some() => Some(true),
            _ => None,
        }
    }

    /// [`Self::set_highlight`] without the `Context`: the host must
    /// `cx.notify()` for the change to reach the screen.
    pub fn set_highlight_deferred(&mut self, object: Option<MapObject>, style: HighlightStyle) -> bool {
        if let Some(same) = self.unchanged_highlight(object.as_ref(), &style) {
            return same;
        }
        if let Some((previous, _)) = self.object_highlight.take() {
            match previous {
                MapObject::Feature(f) => {
                    self.set_feature_highlight_deferred(&f.source, None, style);
                }
                MapObject::Arc { .. } => {
                    self.arc_layers.set_highlight(None, &style);
                }
                MapObject::Extrusion { .. } => {
                    self.extrusion_layers.set_highlight(None, &style);
                }
                MapObject::AltitudePoint { layer, .. } => {
                    let (_, selected, halo) = self.altitude_layers.emphasis(&layer).unwrap_or_default();
                    self.set_altitude_emphasis(&layer, None, selected, halo);
                }
                MapObject::Trip { .. } => {}
            }
        }
        let Some(object) = object else { return false };
        let drawn = match &object {
            MapObject::Feature(f) => self.set_feature_highlight_deferred(&f.source.clone(), Some(f.clone()), style),
            MapObject::Arc { layer, index } => self.arc_layers.set_highlight(Some((layer, *index)), &style),
            MapObject::Extrusion { layer, feature } => self.extrusion_layers.set_highlight(Some((layer, *feature)), &style),
            MapObject::AltitudePoint { layer, index } => match self.altitude_layers.emphasis(layer) {
                Some((_, selected, halo)) => {
                    self.set_altitude_emphasis(layer, Some(*index), selected, halo);
                    true
                }
                None => false,
            },
            MapObject::Trip { .. } => false,
        };
        if drawn {
            self.object_highlight = Some((object, style));
        }
        self.frame_dirty = true;
        drawn
    }

    /// What [`Self::set_highlight`] last lit, if it is still drawn.
    #[must_use]
    pub fn highlight(&self) -> Option<&MapObject> {
        let (object, _) = self.object_highlight.as_ref()?;
        let still = match object {
            MapObject::Feature(f) => self.feature_highlight(&f.source).is_some_and(|h| h.is(f)),
            MapObject::Arc { layer, index } => self.arc_layers.highlighted() == Some((layer.as_str(), *index)),
            MapObject::Extrusion { layer, feature } => self.extrusion_layers.highlighted() == Some((layer.as_str(), *feature)),
            MapObject::AltitudePoint { layer, index } => self.altitude_layers.emphasis(layer).is_some_and(|e| e.0 == Some(*index)),
            MapObject::Trip { .. } => false,
        };
        still.then_some(object)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::sync::Arc;

    use gpui::{Entity, TestAppContext, VisualTestContext};

    use super::{MapObject, MapView};
    use crate::mvt::TagValue;
    use crate::overlay::extrude::{extrude, ExtrudeFeature, ExtrusionStyle};
    use crate::overlay::{FeatureProperties, HighlightStyle};
    use crate::render::arcs::ArcStyle;
    use crate::style::Rgba;

    /// A ~2 km square round the view's centre, id `"sq"`, with a name.
    const SQUARE: &str = r#"{"type":"FeatureCollection","features":[{"type":"Feature","id":"sq",
        "geometry":{"type":"Polygon","coordinates":[[[103.84,1.29],[103.86,1.29],[103.86,1.31],[103.84,1.31],[103.84,1.29]]]},
        "properties":{"name":"square"}}]}"#;

    /// Singapore at z13, 800 x 600, flat and unpitched: the square drawn
    /// (installed as a bake would), an arc due east-west across the
    /// view's centre, and one 100 m column in the square's south-west
    /// corner, clear of the arc.
    fn scene() -> MapView {
        super::super::HEADLESS_PAINT.with(|h| h.set(true));
        let handle = crate::map_view::test_runtime_handle();
        let mut view = MapView::new(
            103.85,
            1.30,
            13.0,
            800.0,
            600.0,
            crate::style::Style { layers: vec![], sprite: None, sky: Default::default() },
            Arc::new(crate::tile_source::EmptyTileSource),
            handle,
        );
        view.overlay_store.set_source("blocks", SQUARE, view.display_zoom()).unwrap();
        let generation = view.overlay_store.source_generation("blocks").unwrap();
        let plan = view.overlay_store.source_plan("blocks").expect("the square draws").clone();
        let (mesh, ranges, _) = super::super::tessellate_overlay(&plan, &|| false, Default::default()).expect("uncancellable");
        view.install_overlay_mesh(&"blocks".into(), generation, super::super::DrawnOverlayMesh::new(plan.origin, mesh, ranges, plan.bracket));
        view
    }

    fn column() -> ExtrudeFeature {
        let (w, s, d) = (103.842, 1.291, 0.005);
        ExtrudeFeature {
            polygons: vec![vec![vec![(w, s), (w + d, s), (w + d, s + d), (w, s + d), (w, s)]]],
            height_m: 100.0,
            base_m: 0.0,
            color: Rgba { r: 0.2, g: 0.4, b: 0.8, a: 1.0 },
        }
    }

    fn props(pairs: &[(&str, f64)]) -> Arc<FeatureProperties> {
        Arc::new(pairs.iter().map(|(k, v)| ((*k).to_string(), TagValue::Number(*v))).collect())
    }

    fn window(app: &mut TestAppContext) -> (Entity<MapView>, &mut VisualTestContext) {
        let (view, cx) = app.add_window_view(|_, _| scene());
        view.update(cx, |v, cx| {
            let set = crate::overlay::arcs::build(
                &[[103.80, 1.30]],
                &[[103.90, 1.30]],
                &crate::overlay::arcs::ArcColors::Constant { source: Rgba { r: 1.0, g: 0.5, b: 0.0, a: 1.0 }, target: Rgba { r: 1.0, g: 0.5, b: 0.0, a: 1.0 } },
                &crate::overlay::arcs::ArcWidths::Constant(2.0),
                Some(("t", &[5.0][..], false)),
            );
            // Low (a tenth of the chord at the peak), so the arc stays well
            // below the eye at this zoom.
            v.set_arc_layer("trips", Arc::new(set), ArcStyle { height: 0.2, ..ArcStyle::default() }, cx);
            assert!(v.set_arc_layer_properties("trips", Arc::from(vec![props(&[("count", 42.0)])])));
            v.set_extrusion_layer("cols", Arc::new(extrude(&[column()])), ExtrusionStyle::default(), cx);
            assert!(v.set_extrusion_layer_properties("cols", Arc::from(vec![props(&[("trees", 7.0)])])));
        });
        wait_for_arc_index(&view, cx, "trips");
        (view, cx)
    }

    /// Lets layer `id`'s off-thread pick index land; bounded at 20 s.
    pub(in crate::map_view) fn wait_for_arc_index(view: &Entity<MapView>, cx: &mut VisualTestContext, id: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !view.read_with(cx, |v, _| v.arc_layer_pickable(id)) {
            assert!(std::time::Instant::now() < deadline, "the arc index for {id} did not land in 20 s");
            cx.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// **The arc index is built off the UI thread** when the layer is set,
    /// and the layer picks nothing until it lands; a newer set's build
    /// replaces it (a stale one is dropped), and a host-built index makes
    /// a layer pickable at once.
    #[test]
    fn the_arc_index_lands_off_the_ui_thread() {
        let mut app = TestAppContext::single();
        let (view, cx) = app.add_window_view(|_, _| scene());
        let centre = screen(&view, cx, 103.85, 1.30);
        let set = |east: f64| {
            Arc::new(crate::overlay::arcs::build(
                &[[103.80, 1.30]],
                &[[east, 1.30]],
                &crate::overlay::arcs::ArcColors::Constant { source: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }, target: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 } },
                &crate::overlay::arcs::ArcWidths::Constant(2.0),
                None,
            ))
        };
        let low = ArcStyle { height: 0.2, ..ArcStyle::default() };
        view.update(cx, |v, cx| v.set_arc_layer("a", set(103.90), low, cx));
        assert!(!view.read_with(cx, |v, _| v.arc_layer_pickable("a")), "not built on the UI thread");
        assert_eq!(view.read_with(cx, |v, _| v.arc_at(centre.0, centre.1)), None, "nothing picks before it lands");
        wait_for_arc_index(&view, cx, "a");
        assert!(view.read_with(cx, |v, _| v.arc_at(centre.0, centre.1)).is_some(), "it lands, and picks");
        // A new set: the old index goes with it, the new one lands.
        view.update(cx, |v, cx| v.set_arc_layer("a", set(103.91), low, cx));
        assert!(!view.read_with(cx, |v, _| v.arc_layer_pickable("a")));
        wait_for_arc_index(&view, cx, "a");
        // A host's own index, for a fresh set, at once.
        let fresh = set(103.92);
        let index = Arc::new(crate::render::arc_pick::ArcPickIndex::new(&fresh));
        view.update(cx, |v, cx| {
            v.set_arc_layer("a", Arc::clone(&fresh), low, cx);
            assert!(v.set_arc_pick_index("a", Arc::clone(&index)));
            assert!(v.arc_layer_pickable("a"));
            assert!(v.arc_at(centre.0, centre.1).is_some());
            let other = Arc::new(crate::render::arc_pick::ArcPickIndex::new(&crate::render::arcs::ArcSet::default()));
            assert!(!v.set_arc_pick_index("a", other), "an index of another set is refused");
        });
        // Its own build lands later and changes nothing.
        cx.run_until_parked();
        assert!(view.read_with(cx, |v, _| v.arc_at(centre.0, centre.1)).is_some());
    }

    /// **`set_overlay_source_order` reorders sources and arc layers** by one
    /// list, bottom first, and the reorder is what the map does, not only
    /// what it reports: of two arcs on the same line the top one picks, and
    /// after the reorder it is the other.
    #[test]
    fn set_overlay_source_order_reorders_sources_and_arc_layers() {
        let mut app = TestAppContext::single();
        let (view, cx) = app.add_window_view(|_, _| scene());
        let centre = screen(&view, cx, 103.85, 1.30);
        let set = Arc::new(crate::overlay::arcs::build(
            &[[103.80, 1.30]],
            &[[103.90, 1.30]],
            &crate::overlay::arcs::ArcColors::Constant { source: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }, target: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 } },
            &crate::overlay::arcs::ArcWidths::Constant(2.0),
            None,
        ));
        let low = ArcStyle { height: 0.2, ..ArcStyle::default() };
        let point = r#"{"type":"FeatureCollection","features":[{"type":"Feature","geometry":{"type":"Point","coordinates":[103.85,1.30]},"properties":{}}]}"#;
        view.update(cx, |v, cx| {
            v.set_arc_layer("a", Arc::clone(&set), low, cx);
            v.set_arc_layer("b", Arc::clone(&set), low, cx);
            v.set_source("s1", point, cx).unwrap();
            v.set_source("s2", point, cx).unwrap();
        });
        wait_for_arc_index(&view, cx, "a");
        wait_for_arc_index(&view, cx, "b");
        let top = |view: &Entity<MapView>, cx: &mut VisualTestContext| view.read_with(cx, |v, _| v.arc_at(centre.0, centre.1)).map(|(id, _)| id);
        assert_eq!(top(&view, cx).as_deref(), Some("b"), "the later layer is on top");
        let moved = view.update(cx, |v, cx| v.set_overlay_source_order(&["b", "s2", "nope", "a", "s1"], cx));
        assert!(moved);
        view.read_with(cx, |v, _| {
            assert_eq!(v.arc_layer_order(), ["b", "a"]);
            assert_eq!(v.overlay_source_order(), ["", "blocks", "s2", "s1"], "the scene's own source keeps its slot");
        });
        assert_eq!(top(&view, cx).as_deref(), Some("a"), "reordered, the other arc is on top");
        assert!(!view.update(cx, |v, cx| v.set_overlay_source_order(&["b", "s2", "a", "s1"], cx)), "idempotent");
    }

    fn screen(view: &Entity<MapView>, cx: &mut VisualTestContext, lon: f64, lat: f64) -> (f32, f32) {
        view.read_with(cx, |v, _| {
            crate::render::project_world_to_screen_f64(v.camera_control.camera(), crate::overlay::geom::lon_lat_to_world(lon, lat)).expect("on screen")
        })
    }

    fn kind(view: &Entity<MapView>, cx: &mut VisualTestContext, at: (f32, f32)) -> Option<MapObject> {
        view.read_with(cx, |v, _| v.pick(at.0, at.1).map(|p| p.object))
    }

    /// One pick across kinds, in draw order: the arc over the square it
    /// crosses, the column over the square it stands in, the square where
    /// there is nothing else -- and a source moved above the labels over
    /// the arc. Each with its properties.
    #[test]
    fn the_topmost_object_wins_across_kinds() {
        let mut app = TestAppContext::single();
        let (view, cx) = window(&mut app);
        let centre = screen(&view, cx, 103.85, 1.30);
        let on_column = screen(&view, cx, 103.8445, 1.2935);
        let square_only = screen(&view, cx, 103.855, 1.307);
        let nothing = screen(&view, cx, 103.87, 1.32);

        assert_eq!(kind(&view, cx, centre), Some(MapObject::Arc { layer: "trips".into(), index: 0 }), "the arc over the square");
        assert_eq!(kind(&view, cx, on_column), Some(MapObject::Extrusion { layer: "cols".into(), feature: 0 }), "the column over the square");
        assert!(matches!(kind(&view, cx, square_only), Some(MapObject::Feature(f)) if &*f.source == "blocks"), "the square alone");
        assert_eq!(kind(&view, cx, nothing), None);

        let p = view.read_with(cx, |v, _| v.pick(centre.0, centre.1)).unwrap();
        let props = p.properties.expect("an arc has properties");
        assert_eq!(props.get("count"), Some(&TagValue::Number(42.0)), "the host's column");
        assert!(matches!(props.get("source_lon"), Some(TagValue::Number(x)) if (x - 103.80).abs() < 1e-4));
        assert!(matches!(props.get("target_lon"), Some(TagValue::Number(x)) if (x - 103.90).abs() < 1e-4));
        let p = view.read_with(cx, |v, _| v.pick(on_column.0, on_column.1)).unwrap();
        assert_eq!(p.properties.and_then(|p| p.get("trees").cloned()), Some(TagValue::Number(7.0)));
        // A feature's are `feature_properties`', whatever its source keeps.
        view.read_with(cx, |v, _| {
            let p = v.pick(square_only.0, square_only.1).unwrap();
            let MapObject::Feature(f) = &p.object else { panic!("{p:?}") };
            assert_eq!(p.properties, v.feature_properties(f));
        });

        // On a frame that lifts the sources over the buildings, the square
        // covers the column, not the arc.
        view.update(cx, |v, _| v.overlay_buildings_drawn = true);
        assert!(matches!(kind(&view, cx, on_column), Some(MapObject::Feature(_))), "over the column");
        assert!(matches!(kind(&view, cx, centre), Some(MapObject::Arc { .. })), "still under the arc");
        // Above the labels, over everything.
        view.update(cx, |v, _| v.set_source_below_labels("blocks", false));
        assert!(matches!(kind(&view, cx, centre), Some(MapObject::Feature(_))), "over the arc");
    }

    /// Overlay sources are hit in PAINT order, not push order: an
    /// above-labels source pushed first still covers a tile-pass source
    /// pushed after it, and an above-buildings one sits between.
    #[test]
    fn feature_at_follows_the_paint_order_across_slots() {
        let mut app = TestAppContext::single();
        let (view, cx) = app.add_window_view(|_, _| {
            let mut v = scene();
            for name in ["top", "middle", "bottom"] {
                v.overlay_store.set_source(name, SQUARE, v.display_zoom()).unwrap();
                let generation = v.overlay_store.source_generation(name).unwrap();
                let plan = v.overlay_store.source_plan(name).expect("the square draws").clone();
                let (mesh, ranges, _) = super::super::tessellate_overlay(&plan, &|| false, Default::default()).expect("uncancellable");
                v.install_overlay_mesh(&name.into(), generation, super::super::DrawnOverlayMesh::new(plan.origin, mesh, ranges, plan.bracket));
            }
            v
        });
        let at = screen(&view, cx, 103.855, 1.307);
        let hit = |cx: &mut VisualTestContext| view.read_with(cx, |v, _| v.feature_at(at.0, at.1).map(|f| f.source.to_string()));
        assert_eq!(hit(cx).as_deref(), Some("bottom"), "all in the tile pass: the last pushed is on top");
        // A frame with buildings lifts all three, in the same order.
        view.update(cx, |v, _| v.overlay_buildings_drawn = true);
        assert_eq!(hit(cx).as_deref(), Some("bottom"), "all over the buildings: still the last pushed");
        view.update(cx, |v, _| v.set_source_above_buildings("bottom", false));
        assert_eq!(hit(cx).as_deref(), Some("middle"), "the above-buildings pass draws after the tile pass");
        view.update(cx, |v, _| v.set_source_below_labels("top", false));
        assert_eq!(hit(cx).as_deref(), Some("top"), "the above-labels pass draws last, though pushed first");
    }

    /// The arc layer's filter window hides the arc from the pick too.
    #[test]
    fn a_filtered_out_arc_is_not_picked() {
        let mut app = TestAppContext::single();
        let (view, cx) = window(&mut app);
        let centre = screen(&view, cx, 103.85, 1.30);
        assert!(view.read_with(cx, |v, _| v.arc_at(centre.0, centre.1)).is_some());
        view.update(cx, |v, cx| v.set_arc_filter_range("trips", Some([6.0, 9.0]), cx));
        assert_eq!(view.read_with(cx, |v, _| v.arc_at(centre.0, centre.1)), None, "filtered out");
        assert!(matches!(kind(&view, cx, centre), Some(MapObject::Feature(_))), "the square under it is picked instead");
        view.update(cx, |v, cx| v.set_arc_filter_range("trips", Some([4.0, 6.0]), cx));
        assert!(view.read_with(cx, |v, _| v.arc_at(centre.0, centre.1)).is_some(), "inside the window again");
    }

    /// A trip is picked by its HEAD at the layer's clock: the head over the
    /// arc wins (trips draw after arcs), carrying its row, label and
    /// position; a clock before or after the trip, a layer with no clock,
    /// or a point away from the head picks what is under it instead. A
    /// trip has no highlight.
    #[test]
    fn a_trip_is_picked_by_its_head_at_the_clock() {
        use crate::render::trips::{TripInput, TripPoint, TripSet, TripStyle};
        let mut app = TestAppContext::single();
        let (view, cx) = window(&mut app);
        let centre = screen(&view, cx, 103.85, 1.30);
        let west = screen(&view, cx, 103.81, 1.30);
        // West to east along the arc's line: at the centre at t = 50.
        let point = |lon: f64, time: f64| TripPoint { lon, lat: 1.30, alt_m: 0.0, time };
        let set = TripSet::from_trips(&[TripInput {
            row: 7,
            points: vec![point(103.80, 0.0), point(103.90, 100.0)],
            color: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 },
            label: Some("SIA21".into()),
        }]);
        view.update(cx, |v, cx| v.set_trip_layer("flights", Arc::new(set), TripStyle { width_px: 1.5, ..TripStyle::default() }, cx));
        assert!(matches!(kind(&view, cx, centre), Some(MapObject::Arc { .. })), "no clock: paths whole, no head to pick");
        view.update(cx, |v, cx| v.set_trip_clock("flights", Some(50.0), cx));
        let trip = MapObject::Trip { layer: "flights".into(), trip: 0 };
        assert_eq!(kind(&view, cx, centre), Some(trip.clone()), "the head over the arc");
        assert_eq!(kind(&view, cx, (centre.0 + 3.0, centre.1 + 2.0)), Some(trip.clone()), "within the tolerance");
        assert!(matches!(kind(&view, cx, west), Some(MapObject::Arc { .. })), "the trail behind the head is not the trip");
        let p = view.read_with(cx, |v, _| v.pick(centre.0, centre.1)).unwrap();
        let props = p.properties.expect("a trip has properties");
        assert_eq!(props.get("row"), Some(&TagValue::Number(7.0)));
        assert_eq!(props.get("label"), Some(&TagValue::String("SIA21".into())));
        assert!(matches!(props.get("lon"), Some(TagValue::Number(x)) if (x - 103.85).abs() < 1e-4), "{props:?}");
        // The head moves with the clock: at t = 10 it is at 103.81.
        view.update(cx, |v, cx| v.set_trip_clock("flights", Some(10.0), cx));
        assert_eq!(kind(&view, cx, west), Some(trip.clone()));
        assert!(matches!(kind(&view, cx, centre), Some(MapObject::Arc { .. })));
        // Landed: no head.
        view.update(cx, |v, cx| v.set_trip_clock("flights", Some(150.0), cx));
        assert!(!matches!(kind(&view, cx, centre), Some(MapObject::Trip { .. })));
        assert!(!view.update(cx, |v, cx| v.set_highlight(Some(trip), HighlightStyle::default(), cx)), "a trip is not highlighted");
        assert_eq!(view.read_with(cx, |v, _| v.highlight().cloned()), None);
    }

    /// The CPU head projection agrees with the flat camera's own projection
    /// on the ground, and lifts with altitude only when the style scales it.
    #[test]
    fn the_head_projects_where_the_camera_puts_the_ground() {
        let mut app = TestAppContext::single();
        let (view, cx) = app.add_window_view(|_, _| scene());
        let at = screen(&view, cx, 103.86, 1.31);
        view.read_with(cx, |v, _| {
            let camera = v.camera_control.camera();
            let globe = crate::render::globe::globe_frame(camera).with_globe_enabled(v.globe_enabled);
            let frame = crate::render::arcs::ArcFrame::new(camera, &globe, 1.0);
            let p = crate::render::trips::head_screen(&frame, [103.86, 1.31, 0.0], 0.0).expect("on screen");
            assert!((p.0 - f64::from(at.0)).abs() < 0.5 && (p.1 - f64::from(at.1)).abs() < 0.5, "{p:?} vs {at:?}");
            let high = crate::render::trips::head_screen(&frame, [103.86, 1.31, 10_000.0], 0.0).unwrap();
            assert!((high.1 - p.1).abs() < 1e-6, "altitude_scale 0 draws on the ground");
        });
    }

    /// One highlight at a time, whatever the kind: each kind's own draw
    /// appears and the last one's goes.
    #[test]
    fn one_highlight_moves_between_kinds() {
        let mut app = TestAppContext::single();
        let (view, cx) = window(&mut app);
        let style = HighlightStyle::default();
        let arc = MapObject::Arc { layer: "trips".into(), index: 0 };
        let col = MapObject::Extrusion { layer: "cols".into(), feature: 0 };
        assert_eq!(view.read_with(cx, |v, _| v.arc_draw_count()), 1);
        assert!(view.update(cx, |v, cx| v.set_highlight(Some(arc.clone()), style, cx)));
        assert_eq!(view.read_with(cx, |v, _| (v.arc_draw_count(), v.highlight().cloned())), (2, Some(arc)), "the arc's highlight draws");
        assert!(view.update(cx, |v, cx| v.set_highlight(Some(col.clone()), style, cx)));
        assert_eq!(view.read_with(cx, |v, _| v.arc_draw_count()), 1, "the arc's went");
        assert!(view.read_with(cx, |v, _| v.extrusion_layers.highlight_set().is_some()), "the column's draws");
        let square = view.read_with(cx, |v, _| {
            let at = crate::render::project_world_to_screen_f64(v.camera_control.camera(), crate::overlay::geom::lon_lat_to_world(103.855, 1.307)).unwrap();
            v.feature_at(at.0, at.1).expect("the square")
        });
        assert!(view.update(cx, |v, cx| v.set_highlight(Some(MapObject::Feature(square.clone())), style, cx)));
        assert!(view.read_with(cx, |v, _| v.extrusion_layers.highlight_set().is_none()), "the column's went");
        assert!(view.read_with(cx, |v, _| v.feature_highlight("blocks").is_some_and(|h| h.is(&square))));
        assert!(!view.update(cx, |v, cx| v.set_highlight(None, style, cx)));
        assert!(view.read_with(cx, |v, _| v.feature_highlight("blocks").is_none() && v.highlight().is_none()));
        // A repeat changes nothing: no re-extrusion, no new arc upload.
        assert!(view.update(cx, |v, cx| v.set_highlight(Some(col.clone()), style, cx)));
        let first = view.read_with(cx, |v, _| v.extrusion_layers.highlight_set().unwrap().0);
        assert!(view.update(cx, |v, cx| v.set_highlight(Some(col.clone()), style, cx)));
        assert!(Arc::ptr_eq(&first, &view.read_with(cx, |v, _| v.extrusion_layers.highlight_set().unwrap().0)), "the same column, not re-extruded");
        let arc = MapObject::Arc { layer: "trips".into(), index: 0 };
        assert!(view.update(cx, |v, cx| v.set_highlight(Some(arc.clone()), style, cx)));
        let generation = view.read_with(cx, |v, _| v.arc_layers.draws()[1].generation);
        assert!(view.update(cx, |v, cx| v.set_highlight(Some(arc.clone()), style, cx)));
        assert_eq!(view.read_with(cx, |v, _| v.arc_layers.draws()[1].generation), generation, "no re-upload");
        let wider = HighlightStyle { outline_width_px: 4.0, ..style };
        assert!(view.update(cx, |v, cx| v.set_highlight(Some(arc.clone()), wider, cx)));
        assert_ne!(view.read_with(cx, |v, _| v.arc_layers.draws()[1].generation), generation, "a new style is redrawn");
        // The column's highlight follows its layer's style.
        assert!(view.update(cx, |v, cx| v.set_highlight(Some(col.clone()), style, cx)));
        view.update(cx, |v, cx| v.set_extrusion_layer_style("cols", ExtrusionStyle { opacity: 0.5, ..ExtrusionStyle::default() }, cx));
        let followed = view.read_with(cx, |v, _| v.extrusion_layers.highlight_set().unwrap().1);
        assert_eq!((followed.opacity, followed.occlude_labels), (0.5, false));
        assert!(!view.update(cx, |v, cx| v.set_highlight(None, style, cx)));
        // Nothing drawn there, nothing lit.
        assert!(!view.update(cx, |v, cx| v.set_highlight(Some(MapObject::Arc { layer: "trips".into(), index: 9 }), style, cx)));
        assert!(!view.update(cx, |v, cx| v.set_highlight(Some(MapObject::Extrusion { layer: "nope".into(), feature: 0 }), style, cx)));
    }

    // -----------------------------------------------------------------
    // The highlight changes the drawn pixels, per kind. Each capture draws
    // exactly what `MapView` hands its pipelines for the highlight.
    // -----------------------------------------------------------------

    use crate::capture::{capture, Capture, CaptureArcs, CaptureExtrusion, CaptureRequest, Clear, TileStore};
    use crate::style::Style;

    fn shot(request: CaptureRequest) -> Capture {
        capture(&request, &TileStore::new(), &Style::default()).expect("a headless device")
    }

    /// The scene's camera as a capture request, over black.
    fn scene_request() -> CaptureRequest {
        CaptureRequest {
            center_lon: 103.85,
            center_lat: 1.30,
            zoom: 13.0,
            width: 800,
            height: 600,
            flat: true,
            globe: false,
            clear: Clear::Sentinel(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
            backdrop: false,
            ..CaptureRequest::default()
        }
    }

    /// Pixels that differ between two frames, and how many of them lie
    /// within `r` px of `at`.
    fn changed(a: &Capture, b: &Capture, at: (f32, f32), r: f32) -> (usize, usize) {
        let mut all = 0;
        let mut near = 0;
        for y in 0..a.height {
            for x in 0..a.width {
                if a.pixel(x, y)[..3] != b.pixel(x, y)[..3] {
                    all += 1;
                    if (x as f32 - at.0).hypot(y as f32 - at.1) <= r {
                        near += 1;
                    }
                }
            }
        }
        (all, near)
    }

    /// Where `(lon, lat)` on the ground lands in [`scene_request`]'s frame.
    fn capture_screen(lon: f64, lat: f64) -> (f32, f32) {
        crate::render::project_world_to_screen_f64(&scene_request().camera(), crate::overlay::geom::lon_lat_to_world(lon, lat)).expect("in frame")
    }

    fn white(p: [u8; 4]) -> bool {
        p[..3].iter().all(|&c| c > 230)
    }

    fn whites(f: &Capture) -> usize {
        (0..f.height).flat_map(|y| (0..f.width).map(move |x| (x, y))).filter(|&(x, y)| white(f.pixel(x, y))).count()
    }

    /// An arc's highlight: a white casing and a brighter core over the
    /// arc, along it.
    #[test]
    fn an_arc_highlight_changes_the_pixels_on_the_arc() {
        let mut app = TestAppContext::single();
        let (view, cx) = window(&mut app);
        // A quarter of the way along, as the GPU draws it in this frame.
        let camera = scene_request().camera();
        let frame = crate::render::arcs::ArcFrame::new(&camera, &crate::render::globe::globe_frame(&camera).flattened(), 1.0);
        let on = frame.project((103.80, 1.30), (103.90, 1.30), 0.25, 0.2).expect("in frame");
        let centre = (on.0 as f32, on.1 as f32);
        let (layer, lit) = view.update(cx, |v, cx| {
            assert!(v.set_highlight(Some(MapObject::Arc { layer: "trips".into(), index: 0 }), HighlightStyle::default(), cx));
            let draws = v.arc_layers.draws();
            assert_eq!(draws.len(), 2);
            let layer = CaptureArcs { set: Arc::new(draws[0].set.clone()), style: draws[0].style, window: None };
            let lit = CaptureArcs { set: Arc::new(draws[1].set.clone()), style: draws[1].style, window: None };
            (layer, lit)
        });
        let plain = shot(CaptureRequest { arcs: vec![layer.clone()], ..scene_request() });
        let hot = shot(CaptureRequest { arcs: vec![layer, lit], ..scene_request() });
        let (all, near) = changed(&plain, &hot, centre, 12.0);
        let lit = |f: &Capture| (0..f.height).flat_map(|y| (0..f.width).map(move |x| (x, y))).filter(|&(x, y)| f.pixel(x, y)[..3].iter().any(|&c| c > 40)).count();
        assert!(near > 20, "the highlight drew on the arc: {near} px near {centre:?}; {all} changed; lit {} -> {}", lit(&plain), lit(&hot));
        assert!(all > 400, "along its length: {all} px");
        assert!(whites(&hot) > whites(&plain) + 200, "a white casing: {} vs {}", whites(&hot), whites(&plain));
    }

    /// **An arc that passes the eye picks only where it draws.** 11 km at
    /// z13, height 1: its middle rises to 5.5 km, past the near plane of a
    /// camera ~10 km up. Every cursor of a grid that picks it lies within
    /// its hit reach of a pixel the GPU lit.
    #[test]
    fn an_arc_through_the_near_plane_picks_only_where_it_draws() {
        use crate::render::arc_pick::{pick, ArcPickIndex};
        // At height 1 the second's apex (~9.75 km) lies between the near
        // plane and the eye (~9.93 km up, the near plane 5% of that below
        // it). Height 0.2 keeps both well below: the guard sees hits.
        for (west, east) in [(103.80, 103.90), (103.7624, 103.9376)] {
            let set = crate::overlay::arcs::build(
                &[[west, 1.30]],
                &[[east, 1.30]],
                &crate::overlay::arcs::ArcColors::Constant { source: Rgba { r: 1.0, g: 0.5, b: 0.0, a: 1.0 }, target: Rgba { r: 1.0, g: 0.5, b: 0.0, a: 1.0 } },
                &crate::overlay::arcs::ArcWidths::Constant(2.0),
                None,
            );
            for height in [0.2f32, 1.0] {
            let style = ArcStyle { height, ..ArcStyle::default() };
            let frame_shot = shot(CaptureRequest { arcs: vec![CaptureArcs { set: Arc::new(set.clone()), style, window: None }], ..scene_request() });
            let lit = |x: i64, y: i64| (x >= 0 && y >= 0 && x < 800 && y < 600) && frame_shot.pixel(x as u32, y as u32)[..3].iter().any(|&c| c > 20);
            let camera = scene_request().camera();
            let frame = crate::render::arcs::ArcFrame::new(&camera, &crate::render::globe::globe_frame(&camera).flattened(), 1.0);
            let index = ArcPickIndex::new(&set);
            let (mut picks, mut stray) = (0, Vec::new());
            for gy in 0..120 {
                for gx in 0..160 {
                    let (x, y) = (f64::from(gx) * 5.0 + 2.5, f64::from(gy) * 5.0 + 2.5);
                    if pick(&frame, &set, &index, &style, None, x, y, 3.0).is_some() {
                        picks += 1;
                        if !(-7i64..=7).any(|dy| (-7i64..=7).any(|dx| lit(x as i64 + dx, y as i64 + dy))) {
                            stray.push((x, y));
                        }
                    }
                }
            }
            assert!(stray.is_empty(), "{west}-{east} at height {height}: {} of {picks} picks where nothing is drawn, e.g. {:?}", stray.len(), &stray[..stray.len().min(6)]);
            }
        }
    }

    /// A column's highlight: the column brighter, in its own place.
    #[test]
    fn an_extrusion_highlight_lights_the_column() {
        let mut app = TestAppContext::single();
        let (view, cx) = window(&mut app);
        let at = capture_screen(103.8445, 1.2935);
        let (layer, lit) = view.update(cx, |v, cx| {
            assert!(v.set_highlight(Some(MapObject::Extrusion { layer: "cols".into(), feature: 0 }), HighlightStyle::default(), cx));
            let (set, style) = v.extrusion_layer("cols").unwrap();
            let (hset, hstyle) = v.extrusion_layers.highlight_set().unwrap();
            (CaptureExtrusion { set, style }, CaptureExtrusion { set: hset, style: hstyle })
        });
        let plain = shot(CaptureRequest { extrusions: vec![layer.clone()], ..scene_request() });
        let hot = shot(CaptureRequest { extrusions: vec![layer, lit], ..scene_request() });
        // Everything that changed lies on the column: inside the box of its
        // roof and its foot (corners projected), and a few pixels of the
        // grown shell round it.
        let camera = scene_request().camera();
        let m2w = f64::from(crate::overlay::extrude::meters_to_world(camera.center_lat));
        let w = f64::from(crate::render::WORLD_SIZE);
        let mut bx = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
        for (lon, lat) in [(103.842, 1.291), (103.847, 1.291), (103.847, 1.296), (103.842, 1.296)] {
            let (x, y) = crate::geo::lon_lat_to_tile_xy(lon, lat, 0.0);
            for z in [0.0, 102.0 * m2w] {
                let clip = camera.view_projection_f64() * glam::DVec4::new(x * w, y * w, z, 1.0);
                let (sx, sy) = (((-clip.x / clip.w + 1.0) * 400.0) as f32, ((1.0 - clip.y / clip.w) * 300.0) as f32);
                bx = [bx[0].min(sx), bx[1].min(sy), bx[2].max(sx), bx[3].max(sy)];
            }
        }
        let (all, near) = changed(&plain, &hot, at, 20.0);
        assert!(near > 100, "the roof changed: {near} px");
        let mut outside = 0;
        for y in 0..plain.height {
            for x in 0..plain.width {
                let (fx, fy) = (x as f32, y as f32);
                if plain.pixel(x, y)[..3] != hot.pixel(x, y)[..3] && !(fx > bx[0] - 3.0 && fx < bx[2] + 3.0 && fy > bx[1] - 3.0 && fy < bx[3] + 3.0) {
                    outside += 1;
                }
            }
        }
        assert_eq!(outside, 0, "{all} px changed, {outside} off the column's box {bx:?}");
        let sum = |p: [u8; 4]| u32::from(p[0]) + u32::from(p[1]) + u32::from(p[2]);
        let (x, y) = (at.0.round() as u32, at.1.round() as u32);
        assert!(sum(hot.pixel(x, y)) > sum(plain.pixel(x, y)) + 60, "brighter: {:?} -> {:?}", plain.pixel(x, y), hot.pixel(x, y));
    }

    /// A point's highlight is a ring: white round the point, the point's
    /// own colour still at its centre.
    #[test]
    fn a_point_highlight_is_a_ring() {
        let json = r#"{"type":"FeatureCollection","features":[{"type":"Feature","geometry":{"type":"Point","coordinates":[103.85,1.30]},"properties":{}}]}"#;
        let request = CaptureRequest {
            overlay: Some(json.to_string()),
            overlay_paint: Some(r##"{"circle-color":"#ff0000","circle-radius":8}"##.to_string()),
            ..scene_request()
        };
        let plain = shot(request.clone());
        let hot = shot(CaptureRequest { overlay_highlight: Some((0, HighlightStyle::default())), ..request });
        let (cx, cy) = (400u32, 300u32);
        assert_eq!(plain.pixel(cx, cy)[..3], hot.pixel(cx, cy)[..3], "the point's own fill stays at its centre");
        assert!(hot.pixel(cx, cy)[0] > 200 && hot.pixel(cx, cy)[1] < 60, "red: {:?}", hot.pixel(cx, cy));
        // Along each axis: red, then a white ring past the point's edge.
        for (dx, dy) in [(1i32, 0i32), (-1, 0), (0, 1), (0, -1)] {
            let at = |f: &Capture, k: i32| f.pixel((cx as i32 + dx * k) as u32, (cy as i32 + dy * k) as u32);
            let ring = (6..16).find(|&k| white(at(&hot, k)));
            assert!(
                ring.is_some_and(|k| k >= 7),
                "a white ring just outside the 8 px point along ({dx}, {dy}): {:?}",
                (0..16).map(|k| at(&hot, k)).collect::<Vec<_>>()
            );
            assert!(!white(at(&plain, ring.unwrap_or(0))), "not there unlit");
        }
    }

    /// An altitude point's hover emphasis (what `set_highlight` sets for
    /// one) draws it larger.
    #[test]
    fn an_altitude_highlight_draws_the_point_larger() {
        use crate::capture::AltitudeCapture;
        use crate::render::altitude::GeoPoint;
        let request = |hovered: Option<usize>| CaptureRequest {
            center_lon: 0.0,
            center_lat: 0.0,
            zoom: 2.0,
            width: 256,
            height: 256,
            clear: Clear::Sentinel(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
            backdrop: false,
            stars: false,
            atmosphere: None,
            altitude: Some(AltitudeCapture {
                points: vec![GeoPoint { lon: 0.0, lat: 0.0, alt_m: 420_000.0 }],
                emphasis: (hovered, None, None),
                ..Default::default()
            }),
            ..Default::default()
        };
        let plain = shot(request(None));
        let hot = shot(request(Some(0)));
        assert_eq!(hot.altitude_points_drawn, 1, "the run painted");
        let lit = |f: &Capture| (0..f.height).flat_map(|y| (0..f.width).map(move |x| (x, y))).filter(|&(x, y)| f.pixel(x, y)[..3].iter().any(|&c| c > 40)).count();
        assert!(lit(&hot) > lit(&plain), "larger: {} vs {} px", lit(&hot), lit(&plain));
    }
}

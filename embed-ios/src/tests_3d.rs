//! Arcs, extrusions, trips and heatmaps: picking headless, pixels on an
//! offscreen `CAMetalLayer`.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

/// An iPhone 13-sized view: 390 x 844 pt at 3x.
fn engine() -> Arc<MapEngine> {
    let engine = MapEngine::new(0, 1170, 2532, 3.0, String::new()).expect("starts without a GPU");
    engine.jump_to(CameraUpdate { lon: Some(103.85), lat: Some(1.29), zoom: Some(14.0), ..Default::default() });
    engine
}

const MID: (f64, f64) = (195.0, 422.0);

#[derive(Default)]
struct Taps {
    taps: Mutex<Vec<(String, String)>>,
    frames: AtomicUsize,
}

impl MapEngineDelegate for Taps {
    fn on_tap(&self, layer: String, id: String) {
        self.taps.lock().unwrap().push((layer, id));
    }
    fn on_camera_changed(&self, _camera: CameraState) {}
    fn on_needs_frame(&self) {
        self.frames.fetch_add(1, Ordering::Relaxed);
    }
}

fn ll(lon: f64, lat: f64) -> LngLat {
    LngLat { lon, lat }
}

/// A square of `half` degrees round `(lon, lat)`.
fn square(lon: f64, lat: f64, half: f64) -> Vec<LngLat> {
    vec![ll(lon - half, lat - half), ll(lon + half, lat - half), ll(lon + half, lat + half), ll(lon - half, lat + half), ll(lon - half, lat - half)]
}

fn flat_arc(id: &str, from: (f64, f64), to: (f64, f64), color: &str, width: f32) -> ArcFeature {
    ArcFeature { id: id.into(), source: ll(from.0, from.1), target: ll(to.0, to.1), source_color: color.into(), target_color: None, width_px: width }
}

#[test]
fn a_tap_on_an_arc_reports_its_id() {
    let engine = engine();
    // Flat (height 0), straight across the centre, east-west.
    let arcs = vec![flat_arc("east-west", (103.83, 1.29), (103.87, 1.29), "#ff00ff", 4.0), flat_arc("north", (103.85, 1.30), (103.85, 1.32), "#00ffff", 4.0)];
    engine.set_arc_layer("od".into(), arcs, ArcLayerStyle { opacity: 1.0, height: 0.0, blend: ArcBlendMode::Normal }).unwrap();
    assert_eq!(engine.feature_at(MID.0, MID.1), Some(FeatureHit { layer: "od".into(), id: "east-west".into() }));
    let recorder = Arc::new(Taps::default());
    engine.set_delegate(recorder.clone());
    engine.touch(TouchPhase::Began, 1, MID.0, MID.1, 0.0);
    engine.touch(TouchPhase::Ended, 1, MID.0, MID.1, 0.0);
    assert_eq!(recorder.taps.lock().unwrap().as_slice(), [("od".to_string(), "east-west".to_string())]);
    // Far off any arc: nothing.
    assert_eq!(engine.feature_at(MID.0, MID.1 + 200.0), None);
    engine.clear("od".into());
    assert_eq!(engine.feature_at(MID.0, MID.1), None, "a cleared arc layer picks nothing");
}

#[test]
fn a_tap_on_an_extruded_column_reports_its_id() {
    let engine = engine();
    engine.jump_to(CameraUpdate { pitch_deg: Some(45.0), ..Default::default() });
    let polys = vec![
        ExtrudedPolygon { id: "tower".into(), rings: vec![square(103.85, 1.29, 0.0015)], height_m: 300.0, base_m: 0.0, color: "#ff00ff".into() },
        ExtrudedPolygon { id: "far".into(), rings: vec![square(103.95, 1.39, 0.0015)], height_m: 50.0, base_m: 0.0, color: "#00ff00".into() },
    ];
    engine.set_extrusion_layer("cols".into(), polys, ExtrusionLayerStyle { opacity: 1.0, vertical_gradient: true }).unwrap();
    assert_eq!(engine.feature_at(MID.0, MID.1).map(|h| (h.layer, h.id)), Some(("cols".into(), "tower".into())));
    assert_eq!(engine.feature_at(20.0, 820.0), None);
}

#[test]
fn a_3d_layer_takes_its_name_from_a_flat_one_and_clear_removes_it() {
    let engine = engine();
    let p = PointFeature { id: "a".into(), lon: 103.85, lat: 1.29, props: HashMap::new() };
    engine.upsert_points("x".into(), vec![p]).unwrap();
    engine.set_heatmap_layer("x".into(), vec![HeatPoint { lon: 103.85, lat: 1.29, weight: 1.0 }], HeatmapLayerStyle { radius_px: 20.0, intensity: 1.0, threshold: 0.1, opacity: 0.8, colors: vec![] }).unwrap();
    assert!(engine.ids("x".into()).is_empty(), "the flat layer gave way");
    assert!(engine.shell_layer_kind("x").is_some());
    engine.clear("x".into());
    assert!(engine.shell_layer_kind("x").is_none());
}

fn trip(id: &str, t0: f64) -> TripFeature {
    TripFeature { id: id.into(), points: vec![TripStop { lon: 103.84, lat: 1.29, time: t0 }, TripStop { lon: 103.86, lat: 1.29, time: t0 + 60.0 }], color: "#ff8800".into() }
}

#[test]
fn trip_time_range_spans_the_trips_and_a_bad_colour_is_refused() {
    let engine = engine();
    let style = TripLayerStyle { width_px: 3.0, opacity: 1.0, trail: 30.0, fade: true };
    engine.set_trip_layer("buses".into(), vec![trip("a", 100.0), trip("b", 130.0)], style).unwrap();
    assert_eq!(engine.trip_time_range("buses".into()), Some(vec![100.0, 190.0]));
    assert!(engine.set_trip_time("buses".into(), Some(150.0)));
    let bad = TripFeature { color: "not a colour".into(), ..trip("c", 0.0) };
    assert!(matches!(engine.set_trip_layer("buses".into(), vec![bad], style), Err(MapError::Paint(_))));
}

/// The GPU half: each kind drawn into a real offscreen `CAMetalLayer` and
/// read back, its colour found on screen where nothing else has it.
#[cfg(target_vendor = "apple")]
mod on_metal {
    use super::*;

    /// Pixels within `tol` of `rgb`.
    fn count_near(frame: &(u32, u32, Vec<u8>), rgb: [u8; 3], tol: i32) -> usize {
        frame.2.chunks_exact(4).filter(|p| (0..3).all(|k| (i32::from(p[k]) - i32::from(rgb[k])).abs() <= tol)).count()
    }

    #[test]
    fn arcs_extrusions_trips_and_heatmaps_put_pixels_on_screen() {
        let layer = objc2_quartz_core::CAMetalLayer::new();
        let ptr = objc2::rc::Retained::as_ptr(&layer) as usize as u64;
        let engine = MapEngine::new(ptr, 780, 1688, 2.0, String::new()).expect("a surface on an offscreen layer");
        engine.jump_to(CameraUpdate { lon: Some(103.85), lat: Some(1.29), zoom: Some(14.0), ..Default::default() });
        let before = engine.capture_frame().expect("a frame reads back");
        assert_eq!((before.0, before.1), (780, 1688));
        assert!(count_near(&before, [255, 0, 255], 40) < 50, "no magenta before");

        engine.set_arc_layer("arcs".into(), vec![flat_arc("a", (103.83, 1.285), (103.87, 1.285), "#ff00ff", 6.0)], ArcLayerStyle { opacity: 1.0, height: 1.0, blend: ArcBlendMode::Normal }).unwrap();
        let f = engine.capture_frame().unwrap();
        let arcs = count_near(&f, [255, 0, 255], 40);
        assert!(arcs > 500, "an arc draws: {arcs} px");
        assert!(engine.layer_stats_3d().arcs == 1);
        engine.set_arc_layer("arcs".into(), vec![flat_arc("a", (103.83, 1.285), (103.87, 1.285), "#ff00ff", 6.0)], ArcLayerStyle { opacity: 1.0, height: 1.0, blend: ArcBlendMode::Glow }).unwrap();
        let f = engine.capture_frame().unwrap();
        let glow = f.2.chunks_exact(4).filter(|p| p[0] > 120 && p[2] > 120 && p[1] < 100).count();
        assert!(glow > 300, "a glow arc draws: {glow} px");
        engine.clear("arcs".into());

        engine.jump_to(CameraUpdate { pitch_deg: Some(50.0), ..Default::default() });
        let column = ExtrudedPolygon { id: "c".into(), rings: vec![square(103.85, 1.29, 0.002)], height_m: 400.0, base_m: 0.0, color: "#00ff00".into() };
        engine.set_extrusion_layer("cols".into(), vec![column], ExtrusionLayerStyle { opacity: 1.0, vertical_gradient: false }).unwrap();
        let f = engine.capture_frame().unwrap();
        let green = f.2.chunks_exact(4).filter(|p| p[1] > 100 && i32::from(p[1]) > 2 * i32::from(p[0]) && i32::from(p[1]) > 2 * i32::from(p[2])).count();
        assert!(green > 2000, "a lit green column: {green} px");
        assert!(engine.layer_stats_3d().extrusion_triangles > 0);
        engine.clear("cols".into());
        engine.jump_to(CameraUpdate { pitch_deg: Some(0.0), ..Default::default() });

        let path = TripFeature { id: "t".into(), points: (0..=20).map(|i| TripStop { lon: 103.83 + f64::from(i) * 0.002, lat: 1.29, time: f64::from(i) }).collect(), color: "#ff00ff".into() };
        engine.set_trip_layer("trips".into(), vec![path], TripLayerStyle { width_px: 8.0, opacity: 1.0, trail: 10.0, fade: false }).unwrap();
        engine.set_trip_time("trips".into(), Some(15.0));
        let f = engine.capture_frame().unwrap();
        let trail = count_near(&f, [255, 0, 255], 40);
        assert!(trail > 300, "a trail draws: {trail} px");
        engine.set_trip_time("trips".into(), Some(-100.0));
        let f = engine.capture_frame().unwrap();
        assert!(count_near(&f, [255, 0, 255], 40) < trail / 4, "before its start the trip is not drawn");
        engine.clear("trips".into());

        let points: Vec<HeatPoint> = (0..400).map(|i| HeatPoint { lon: 103.85 + f64::from(i % 20) * 0.0004, lat: 1.29 + f64::from(i / 20) * 0.0004, weight: 1.0 }).collect();
        let style = HeatmapLayerStyle { radius_px: 30.0, intensity: 1.0, threshold: 0.05, opacity: 1.0, colors: vec!["#ff00ff".into(), "#ff00ff".into()] };
        engine.set_heatmap_layer("heat".into(), points, style).unwrap();
        let f = engine.capture_frame().unwrap();
        let heat = count_near(&f, [255, 0, 255], 60);
        assert!(heat > 2000, "a heatmap draws: {heat} px");
        drop(engine);
        drop(layer);
    }
}

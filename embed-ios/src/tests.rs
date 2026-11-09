//! Headless: every test builds a `MapEngine` with `layer_ptr` 0, which runs
//! the whole engine (style, camera, gestures, overlays, picking) except the
//! GPU. `cargo test` from `embed-ios/`.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

/// An iPhone 13-sized view: 390 x 844 pt at 3x.
fn engine() -> Arc<MapEngine> {
    let engine = MapEngine::new(0, 1170, 2532, 3.0, String::new()).expect("starts without a GPU");
    engine.jump_to(CameraUpdate { lon: Some(103.85), lat: Some(1.29), zoom: Some(14.0), ..Default::default() });
    engine
}

const MID: (f64, f64) = (195.0, 422.0);

fn stop(id: &str, lon: f64, lat: f64) -> PointFeature {
    PointFeature {
        id: id.into(),
        lon,
        lat,
        props: HashMap::from([("name".to_string(), PropValue::Text { value: id.into() })]),
    }
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

#[derive(Default)]
struct Recorder {
    taps: Mutex<Vec<(String, String)>>,
    cameras: Mutex<Vec<CameraState>>,
    frames: AtomicUsize,
}

impl MapEngineDelegate for Recorder {
    fn on_tap(&self, layer: String, id: String) {
        self.taps.lock().unwrap().push((layer, id));
    }
    fn on_camera_changed(&self, camera: CameraState) {
        self.cameras.lock().unwrap().push(camera);
    }
    fn on_needs_frame(&self) {
        self.frames.fetch_add(1, Ordering::Relaxed);
    }
}

fn tap(engine: &MapEngine, id: u64, (x, y): (f64, f64)) {
    engine.touch(TouchPhase::Began, id, x, y, 0.0);
    engine.touch(TouchPhase::Ended, id, x, y, 0.0);
}

#[test]
fn an_upsert_by_an_existing_id_replaces_and_a_new_id_adds() {
    let engine = engine();
    engine.upsert_points("stops".into(), vec![stop("a", 103.85, 1.29), stop("b", 103.86, 1.29)]).unwrap();
    engine.upsert_points("stops".into(), vec![stop("a", 103.84, 1.28), stop("c", 103.87, 1.30)]).unwrap();
    assert_eq!(sorted(engine.ids("stops".into())), ["a", "b", "c"]);
}

#[test]
fn an_upserted_point_moves_where_it_is_picked() {
    let engine = engine();
    engine.upsert_points("buses".into(), vec![stop("bus", 103.85, 1.29)]).unwrap();
    assert_eq!(engine.feature_at(MID.0, MID.1).map(|h| h.id), Some("bus".into()));
    // Moved ~1 km east, by id: no longer under the centre.
    engine.upsert_points("buses".into(), vec![stop("bus", 103.86, 1.29)]).unwrap();
    assert_eq!(engine.feature_at(MID.0, MID.1), None);
    assert_eq!(engine.ids("buses".into()), ["bus"]);
}

#[test]
fn remove_reports_what_was_there_and_clear_empties() {
    let engine = engine();
    let stops = (0..10).map(|i| stop(&format!("s{i}"), 103.8 + f64::from(i) * 0.01, 1.29)).collect();
    engine.upsert_points("stops".into(), stops).unwrap();
    assert_eq!(engine.remove("stops".into(), vec!["s0".into(), "s5".into(), "nope".into()]), 2);
    let left = sorted(engine.ids("stops".into()));
    assert_eq!(left.len(), 8);
    assert!(!left.contains(&"s0".to_string()) && !left.contains(&"s5".to_string()));
    engine.clear("stops".into());
    assert!(engine.ids("stops".into()).is_empty());
    assert_eq!(engine.remove("nowhere".into(), vec!["x".into()]), 0);
}

#[test]
fn a_layer_refuses_a_second_kind_until_cleared() {
    let engine = engine();
    engine.upsert_points("x".into(), vec![stop("a", 103.85, 1.29)]).unwrap();
    let line = LineFeature {
        id: "l".into(),
        coords: vec![LngLat { lon: 103.8, lat: 1.3 }, LngLat { lon: 103.9, lat: 1.3 }],
        props: HashMap::new(),
    };
    assert!(matches!(engine.upsert_lines("x".into(), vec![line.clone()]), Err(MapError::KindMismatch(_))));
    engine.clear("x".into());
    engine.upsert_lines("x".into(), vec![line]).unwrap();
}

#[test]
fn a_tap_on_a_point_reaches_the_delegate_with_its_layer_and_id() {
    let engine = engine();
    let recorder = Arc::new(Recorder::default());
    engine.set_delegate(recorder.clone());
    engine.upsert_points("stops".into(), vec![stop("marina", 103.85, 1.29)]).unwrap();
    tap(&engine, 7, MID);
    // Far from the point: no tap reported.
    tap(&engine, 8, (20.0, 200.0));
    assert_eq!(*recorder.taps.lock().unwrap(), [("stops".to_string(), "marina".to_string())]);
}

#[test]
fn a_tap_on_a_line_and_on_a_heading_marker_reports_their_ids() {
    let engine = engine();
    let recorder = Arc::new(Recorder::default());
    engine.set_delegate(recorder.clone());
    let route = LineFeature {
        id: "r1".into(),
        coords: vec![LngLat { lon: 103.84, lat: 1.29 }, LngLat { lon: 103.86, lat: 1.29 }],
        props: HashMap::new(),
    };
    engine.upsert_lines("routes".into(), vec![route]).unwrap();
    tap(&engine, 1, MID);
    // A bus 0.003 degrees north of the centre: this many points above it
    // (north-up, unpitched, Mercator on 512 px tiles).
    let lat = 1.293;
    let marker = heading_marker(103.85, lat, 90.0, 14.0);
    engine
        .upsert_polygons("buses".into(), vec![PolygonFeature { id: "bus-7".into(), rings: vec![marker], props: HashMap::new() }])
        .unwrap();
    let up = 0.003 * 512.0 * 2f64.powf(14.0) / 360.0 / 1.29f64.to_radians().cos();
    tap(&engine, 2, (MID.0, MID.1 - up));
    assert_eq!(
        *recorder.taps.lock().unwrap(),
        [("routes".to_string(), "r1".to_string()), ("buses".to_string(), "bus-7".to_string())]
    );
}

#[test]
fn a_removed_feature_is_no_longer_picked() {
    let engine = engine();
    engine.upsert_points("stops".into(), vec![stop("gone", 103.85, 1.29)]).unwrap();
    assert!(engine.feature_at(MID.0, MID.1).is_some());
    engine.remove("stops".into(), vec!["gone".into()]);
    assert_eq!(engine.feature_at(MID.0, MID.1), None);
}

#[test]
fn the_topmost_layer_wins_a_pick() {
    let engine = engine();
    engine.upsert_points("below".into(), vec![stop("b", 103.85, 1.29)]).unwrap();
    engine.upsert_points("above".into(), vec![stop("a", 103.85, 1.29)]).unwrap();
    assert_eq!(engine.feature_at(MID.0, MID.1), Some(FeatureHit { layer: "above".into(), id: "a".into() }));
}

#[test]
fn set_style_takes_constants_and_expressions_and_refuses_junk() {
    let engine = engine();
    engine.set_style("stops".into(), r##"{"circle-radius": 6, "circle-color": ["get", "color"]}"##.into()).unwrap();
    let mut colored = stop("a", 103.85, 1.29);
    colored.props.insert("color".into(), PropValue::Text { value: "#e53935".into() });
    engine.upsert_points("stops".into(), vec![colored]).unwrap();
    assert!(engine.feature_at(MID.0, MID.1).is_some());
    assert!(matches!(engine.set_style("stops".into(), "[1]".into()), Err(MapError::Paint(_))));
    assert!(matches!(engine.set_style("stops".into(), r##"{"fill-colour": ["get", "x"]}"##.into()), Err(MapError::Paint(_))));
    // A restyle of a live layer goes through the engine's paint update.
    engine.set_style("stops".into(), r##"{"circle-color": ["get", "other"]}"##.into()).unwrap();
}

#[test]
fn mixed_property_types_still_push() {
    let engine = engine();
    let mut a = stop("a", 103.85, 1.29);
    a.props.insert("n".into(), PropValue::Number { value: 1.0 });
    let mut b = stop("b", 103.851, 1.29);
    b.props.insert("n".into(), PropValue::Text { value: "x".into() });
    b.props.insert("live".into(), PropValue::Bool { value: true });
    engine.upsert_points("stops".into(), vec![a, b]).unwrap();
    assert!(engine.feature_at(MID.0, MID.1).is_some());
}

#[test]
fn headless_render_is_idle_and_reports_the_camera_once_per_change() {
    let engine = engine();
    let recorder = Arc::new(Recorder::default());
    engine.set_delegate(recorder.clone());
    assert_eq!(engine.render(100.0), FrameRequest::Idle);
    assert_eq!(engine.render(100.016), FrameRequest::Idle);
    assert_eq!(recorder.cameras.lock().unwrap().len(), 1);
    engine.jump_to(CameraUpdate { zoom: Some(10.0), ..Default::default() });
    engine.render(100.032);
    let cameras = recorder.cameras.lock().unwrap();
    assert_eq!(cameras.len(), 2);
    assert_eq!(cameras[1].zoom, 10.0);
    assert!((cameras[1].lon - 103.85).abs() < 1e-9);
}

#[test]
fn the_frame_request_maps_the_engine_answer_onto_host_time() {
    assert_eq!(host::frame_request(true, Some(Instant::now())), host::FrameRequest::Again);
    assert_eq!(host::frame_request(false, None), host::FrameRequest::Idle);
    let at = Instant::now() + Duration::from_millis(250);
    assert_eq!(host::frame_request(false, Some(at)), host::FrameRequest::At(at));
}

#[test]
fn updates_ask_the_delegate_for_a_frame() {
    let engine = engine();
    let recorder = Arc::new(Recorder::default());
    engine.set_delegate(recorder.clone());
    let before = recorder.frames.load(Ordering::Relaxed);
    engine.upsert_points("stops".into(), vec![stop("a", 103.85, 1.29)]).unwrap();
    engine.touch(TouchPhase::Began, 1, 100.0, 100.0, 0.0);
    assert!(recorder.frames.load(Ordering::Relaxed) >= before + 2);
}

#[test]
fn a_one_finger_drag_pans_the_camera() {
    let engine = engine();
    let before = engine.camera();
    engine.touch(TouchPhase::Began, 1, 200.0, 400.0, 0.0);
    for i in 1..=10 {
        engine.touch(TouchPhase::Moved, 1, 200.0 - 10.0 * f64::from(i), 400.0, 0.0);
    }
    engine.touch(TouchPhase::Ended, 1, 100.0, 400.0, 0.0);
    // Dragged left: the map moves left, the centre east.
    assert!(engine.camera().lon > before.lon, "{:?} -> {:?}", before, engine.camera());
}

#[test]
fn ease_to_and_jump_to_move_the_camera() {
    let engine = engine();
    engine.jump_to(CameraUpdate {
        lon: Some(103.9),
        lat: Some(1.35),
        zoom: Some(12.0),
        bearing_deg: Some(30.0),
        pitch_deg: Some(20.0),
    });
    let c = engine.camera();
    assert!((c.lon - 103.9).abs() < 1e-6 && (c.lat - 1.35).abs() < 1e-6 && (c.zoom - 12.0).abs() < 1e-9);
    assert!((c.bearing_deg - 30.0).abs() < 1e-3 && (c.pitch_deg - 20.0).abs() < 1e-3, "{c:?}");
    engine.ease_to(CameraUpdate { zoom: Some(15.0), ..Default::default() }, 0.0);
}

#[test]
fn basemaps_are_keyless_switch_by_name_and_credit_openstreetmap() {
    let engine = engine();
    let names = engine.basemaps();
    assert!(names.len() >= 2, "{names:?}");
    engine.set_basemap(names[1].clone()).unwrap();
    assert!(matches!(engine.set_basemap("nope".into()), Err(MapError::UnknownBasemap(_))));
    let credits = engine.attributions();
    assert!(credits.iter().any(|c| c.contains("OpenStreetMap")), "{credits:?}");
    assert!(credits.iter().all(|c| !c.contains("Mapbox")), "{credits:?}");
}

#[test]
fn a_basemap_archive_is_credited_and_a_missing_one_refused() {
    let engine = engine();
    assert!(matches!(engine.set_basemap_archive(Some("/nowhere/region.pmtiles".into()), None, false), Err(MapError::Archive(_))));
    let archive = std::env::temp_dir().join(format!("embed-archive-{}.pmtiles", std::process::id()));
    std::fs::write(&archive, b"").unwrap();
    engine.set_basemap_archive(Some(archive.display().to_string()), None, false).unwrap();
    let credits = engine.attributions();
    assert!(credits.iter().any(|c| c.contains("OpenMapTiles") && !c.contains("OpenFreeMap")), "{credits:?}");
    engine.set_basemap_archive(None, None, false).unwrap();
    assert!(engine.attributions().iter().any(|c| c.contains("OpenFreeMap")));
    let _ = std::fs::remove_file(archive);
}

#[test]
#[should_panic(expected = "main-thread only")]
fn a_call_from_another_thread_is_refused() {
    let engine = engine();
    let other = Arc::clone(&engine);
    let result = std::thread::spawn(move || other.camera()).join();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[test]
fn the_heading_marker_points_where_the_vehicle_heads() {
    let ring = heading_marker(103.85, 1.29, 90.0, 14.0);
    assert_eq!(ring.first(), ring.last());
    let east = ring.iter().map(|p| p.lon).fold(f64::MIN, f64::max);
    assert_eq!(ring[0].lon, east);
    // Bigger on screen zoomed in, as the desktop draws it.
    let span = |z: f64| {
        let r = heading_marker(103.85, 1.29, 0.0, z);
        let (lo, hi) = r.iter().fold((f64::MAX, f64::MIN), |(lo, hi), p| (lo.min(p.lat), hi.max(p.lat)));
        (hi - lo) * 2f64.powf(z)
    };
    assert!(span(16.0) > span(10.0));
    assert_eq!(marker_polygon_limit(), 400);
}

/// What crosses the FFI boundary comes back as it went: every record and
/// enum the API takes or returns, through uniffi's own lower/lift.
#[test]
fn records_round_trip_through_the_ffi_converters() {
    use uniffi::{Lift, Lower};
    fn round<T>(value: T)
    where
        T: Lower<crate::UniFfiTag>
            + Lift<crate::UniFfiTag, FfiType = <T as Lower<crate::UniFfiTag>>::FfiType>
            + PartialEq
            + std::fmt::Debug
            + Clone,
    {
        let lowered = <T as Lower<crate::UniFfiTag>>::lower(value.clone());
        let lifted = <T as Lift<crate::UniFfiTag>>::try_lift(lowered).expect("lifts");
        assert_eq!(lifted, value);
    }
    let mut p = stop("a", 103.85, 1.29);
    p.props.insert("n".into(), PropValue::Number { value: 2.5 });
    p.props.insert("b".into(), PropValue::Bool { value: false });
    round(p);
    round(LineFeature { id: "l".into(), coords: vec![LngLat { lon: 1.0, lat: 2.0 }], props: HashMap::new() });
    round(PolygonFeature { id: "p".into(), rings: vec![vec![LngLat { lon: 1.0, lat: 2.0 }]], props: HashMap::new() });
    round(CameraUpdate { lon: Some(1.0), zoom: Some(3.0), ..Default::default() });
    round(CameraState { lon: 1.0, lat: 2.0, zoom: 3.0, bearing_deg: 4.0, pitch_deg: 5.0 });
    round(FrameRequest::At { t: 12.5 });
    round(FrameRequest::Idle);
    round(TouchPhase::Cancelled);
    round(FeatureHit { layer: "x".into(), id: "y".into() });
    round(EdgeInsets { top: 47.0, bottom: 34.0, left: 0.0, right: 12.0 });
}

/// The device bug at 74f97b73: on the embed path the camera is sized before
/// the renderer exists and the renderer is created at the final size, so
/// the first resize pass found nothing to reconfigure and returned before
/// setting the pixel ratio. Labels were laid out at 1x into a 3x target: a
/// third the size, squeezed into the top-left third. A real offscreen
/// `CAMetalLayer`, the app's own order of calls, at 3x, 2x and 1x.
#[cfg(target_vendor = "apple")]
#[test]
fn labels_lay_out_at_the_device_pixel_ratio_on_the_embed_path() {
    for scale in [3.0f64, 2.0, 1.0] {
        let layer = objc2_quartz_core::CAMetalLayer::new();
        let ptr = objc2::rc::Retained::as_ptr(&layer) as usize as u64;
        let (w, h) = ((390.0 * scale) as u32, (844.0 * scale) as u32);
        let engine = MapEngine::new(ptr, w, h, scale, String::new()).expect("a surface on an offscreen layer");
        engine.set_insets(EdgeInsets { top: 47.0, bottom: 34.0, left: 0.0, right: 0.0 });
        engine.render(0.0);
        assert_eq!(engine.lock().value.host.pixel_ratio(), scale as f32, "at {scale}x");
        // A resize to another size keeps it, and so does a rotation.
        engine.resize(h, w, scale);
        engine.render(0.016);
        assert_eq!(engine.lock().value.host.pixel_ratio(), scale as f32, "rotated, at {scale}x");
        drop(engine);
        drop(layer);
    }
}

/// Two stops framed under a bottom sheet: both land inside the uncovered
/// area, the box fills it, and nothing lands under the sheet.
#[test]
fn fit_bounds_frames_the_box_in_the_uncovered_area() {
    let engine = engine();
    let (a, b) = (stop("west", 103.80, 1.28), stop("east", 103.90, 1.32));
    engine.upsert_points("stops".into(), vec![a, b]).unwrap();
    engine.set_insets(EdgeInsets { top: 47.0, bottom: 500.0, left: 0.0, right: 0.0 });
    engine.fit_bounds(103.80, 1.28, 103.90, 1.32, 20.0, 17.0, 0.0);
    // 390 pt wide, 40 pt of margin: the 0.1 degree box spans 350 pt.
    let c = engine.camera();
    let span = 0.1 / 360.0 * 512.0 * 2f64.powf(c.zoom);
    assert!((span - 350.0).abs() < 1.0, "zoom {} spans {span} pt", c.zoom);
    // Uncovered: 47..344 pt; its middle is 195.5. The west stop sits at the
    // left margin, the east one at the right.
    let mid_y = (47.0 + (844.0 - 500.0)) / 2.0;
    // The box is 0.04 degrees tall: west (south) sits below the middle,
    // east (north) above it.
    let half_h = span * 0.4 / 2.0;
    let hit = engine.feature_at(20.0, mid_y + half_h).map(|h| h.id);
    assert_eq!(hit.as_deref(), Some("west"));
    assert_eq!(engine.feature_at(370.0, mid_y - half_h).map(|h| h.id).as_deref(), Some("east"));
    // A single stop stops at max_zoom.
    engine.fit_bounds(103.85, 1.29, 103.85, 1.29, 20.0, 17.0, 0.0);
    assert_eq!(engine.camera().zoom, 17.0);
}

/// The passenger's location never reaches the log: debug lines on, a
/// session over a known place with a tap, and no coordinate of it -- nor
/// the tap point -- appears in what was logged.
#[test]
fn a_debug_logged_session_writes_no_coordinates() {
    gpui_maps_ios::log::capture(true);
    let engine = MapEngine::new(0, 1170, 2532, 3.0, String::new()).unwrap();
    engine.set_debug_logging(true);
    // Somewhere no other test goes.
    engine.jump_to(CameraUpdate { lon: Some(139.7671), lat: Some(35.6812), zoom: Some(15.0), ..Default::default() });
    engine.upsert_points("stops".into(), vec![stop("tokyo", 139.7671, 35.6812)]).unwrap();
    engine.touch(TouchPhase::Began, 3, 195.0, 422.0, 0.0);
    for i in 1..=5 {
        engine.touch(TouchPhase::Moved, 3, 195.0 - 13.0 * f64::from(i), 422.0, 0.0);
    }
    engine.touch(TouchPhase::Ended, 3, 130.0, 422.0, 0.0);
    tap(&engine, 4, (233.0, 377.0));
    engine.render(1.0);
    let c = engine.camera();
    engine.set_debug_logging(false);
    let lines = gpui_maps_ios::log::capture(false);
    assert!(lines.iter().any(|l| l.contains("[touch]")), "the session logged something: {lines:?}");
    let (lon, lat) = (format!("{:.2}", c.lon), format!("{:.2}", c.lat));
    let forbidden = ["139.7", "35.68", "35.6", lon.as_str(), lat.as_str(), "233,377"];
    for line in &lines {
        for needle in forbidden {
            assert!(!line.contains(needle), "{needle:?} leaked into {line:?}");
        }
    }
    assert_eq!(gpui_maps_ios::log::position(35.6812, 139.7671), "-");
    assert_eq!(gpui_maps_ios::log::scrub("GET https://tiles.openfreemap.org/planet/15/29105/12903.pbf: 503"), "GET <url> 503");
}

/// A bottom sheet over the lower half: a move centres in the upper half,
/// where the uncovered area is.
#[test]
fn insets_centre_camera_moves_in_the_uncovered_area() {
    let engine = engine();
    engine.upsert_points("stops".into(), vec![stop("a", 103.85, 1.29)]).unwrap();
    engine.set_insets(EdgeInsets { top: 0.0, bottom: 422.0, left: 0.0, right: 0.0 });
    engine.jump_to(CameraUpdate { lon: Some(103.85), lat: Some(1.29), ..Default::default() });
    // The uncovered area is 0..422 pt; its middle is 211.
    assert!(engine.feature_at(MID.0, 211.0).is_some(), "the point sits mid-uncovered-area");
    assert_eq!(engine.feature_at(MID.0, MID.1), None);
}

/// Stops on a grid around the centre, labelled "code\nname" under each one
/// (`text-anchor: top`, `text-offset: [0, 0.9]`), as the transit app does.
fn labelled_stops(engine: &MapEngine, n_side: usize, step_deg: f64) {
    let style = r##"{"circle-radius": 5, "circle-color": "rgba(28, 28, 30, 0.9)",
        "text-field": "{code}\n{name}", "text-size": 11, "text-color": "rgb(20, 20, 20)",
        "text-halo-color": "#ffffffcc", "text-halo-width": 1.5,
        "text-anchor": "top", "text-offset": [0, 0.9]}"##;
    engine.set_style("stops".into(), style.into()).unwrap();
    let mut stops = Vec::new();
    for i in 0..n_side {
        for j in 0..n_side {
            let half = (n_side as f64 - 1.0) / 2.0;
            let lon = 103.85 + (i as f64 - half) * step_deg;
            let lat = 1.29 + (j as f64 - half) * step_deg;
            let mut s = stop(&format!("S{i:02}{j:02}"), lon, lat);
            s.props.insert("code".into(), PropValue::Text { value: format!("{:05}", i * 100 + j) });
            stops.push(s);
        }
    }
    engine.upsert_points("stops".into(), stops).unwrap();
}

fn place(engine: &MapEngine) -> Vec<(String, String, gpui_maps_ios::collision::CollisionBox)> {
    engine.lock().value.host.place_labels_headless()
}

/// 1,000 labelled stops: labels collide (with each other: no two placed
/// boxes overlap, and far fewer than 1,000 fit), and every placed label
/// hangs under its own stop -- horizontally centred on the projected
/// anchor, its top 0.9 em below it -- at 3x, 2x and 1x.
#[test]
fn a_thousand_stop_labels_collide_and_sit_under_their_stops_at_every_scale() {
    for scale in [3.0f64, 2.0, 1.0] {
        let engine = MapEngine::new(0, (390.0 * scale) as u32, (844.0 * scale) as u32, scale, String::new()).unwrap();
        engine.jump_to(CameraUpdate { lon: Some(103.85), lat: Some(1.29), zoom: Some(15.0), ..Default::default() });
        // 32 x 32 = 1,024 stops, ~110 m apart: the whole grid is on screen.
        labelled_stops(&engine, 32, 0.0004);
        let placed = place(&engine);
        assert!(placed.len() > 20 && placed.len() < 700, "{} of 1,024 placed at {scale}x", placed.len());
        for (a, (_, ida, boxa)) in placed.iter().enumerate() {
            for (_, idb, boxb) in &placed[a + 1..] {
                let overlap = boxa.left < boxb.right && boxb.left < boxa.right && boxa.top < boxb.bottom && boxb.top < boxa.bottom;
                assert!(!overlap, "{ida} and {idb} overlap at {scale}x: {boxa:?} {boxb:?}");
            }
        }
        let camera = engine.lock().value.host.camera();
        let s = scale as f32;
        for (layer, id, b) in &placed {
            assert_eq!(layer, "stops");
            let i: f64 = id[1..3].parse().unwrap();
            let j: f64 = id[3..5].parse().unwrap();
            let (lon, lat) = (103.85 + (i - 15.5) * 0.0004, 1.29 + (j - 15.5) * 0.0004);
            let (ax, ay, _) = camera.project_lon_lat_alt(lon, lat, 0.0).expect("on screen");
            let (ax, ay) = (ax * s, ay * s);
            let centre_x = (b.left + b.right) / 2.0;
            assert!((centre_x - ax).abs() < 3.0 * s, "{id} centred at {centre_x}, anchor {ax} ({scale}x)");
            // Top 0.9 em under the anchor (11 pt type), within the
            // collision padding and the line box's leading.
            let below = b.top - ay;
            let em = 11.0 * s;
            assert!(below > 0.5 * em && below < 1.4 * em, "{id}: top {below} px below the anchor, em {em} ({scale}x)");
            // Two lines of 11 pt: roughly 2.4 em tall with padding, scaled.
            let height = b.bottom - b.top;
            assert!(height > 1.8 * em && height < 3.2 * em, "{id}: {height} px tall, em {em} ({scale}x)");
        }
    }
}

/// An empty resolved label draws no text, and the stop's circle stays (it
/// is still a point of the layer, and still picked).
#[test]
fn an_empty_label_draws_no_text_but_keeps_its_circle() {
    let engine = engine();
    engine.set_style("stops".into(), r#"{"text-field": ["get", "label"]}"#.into()).unwrap();
    let mut named = stop("named", 103.85, 1.29);
    named.props.insert("label".into(), PropValue::Text { value: "Marina".into() });
    let mut blank = stop("blank", 103.852, 1.29);
    blank.props.insert("label".into(), PropValue::Text { value: String::new() });
    let missing = stop("missing", 103.848, 1.29);
    engine.upsert_points("stops".into(), vec![named, blank, missing]).unwrap();
    let ids: Vec<String> = place(&engine).into_iter().map(|(_, id, _)| id).collect();
    assert_eq!(ids, ["named"]);
    assert_eq!(sorted(engine.ids("stops".into())), ["blank", "missing", "named"]);
    let blank_x = MID.0 + 0.002 / 360.0 * 512.0 * 2f64.powf(14.0);
    assert_eq!(engine.feature_at(blank_x, MID.1).map(|h| h.id).as_deref(), Some("blank"));
}

/// The style forms: `["get", k]`, `"{k}"` tokens, data-driven size and
/// colour, CSS colours; a `text-*` key with no field, or an unknown one, is
/// refused by name.
#[test]
fn text_styles_compile_and_refuse_by_name() {
    let engine = engine();
    for ok in [
        r#"{"text-field": ["get", "name"], "text-size": ["get", "size"], "text-color": ["get", "colour"]}"#,
        r#"{"text-field": "{code} {name}", "text-anchor": "bottom-left", "text-max-width": 8}"#,
        r#"{"circle-color": "rgba(10, 20, 30, 0.5)", "text-field": "x", "text-halo-color": "rgb(255,255,255)"}"#,
    ] {
        engine.set_style("stops".into(), ok.into()).unwrap_or_else(|e| panic!("{ok}: {e}"));
    }
    for (bad, says) in [
        (r#"{"text-size": 12}"#, "needs a text-field"),
        (r#"{"text-field": "x", "text-rotate": 30}"#, "text-rotate"),
        (r#"{"text-field": ["concat", "a"]}"#, "text-field"),
        (r#"{"text-field": "x", "text-anchor": "middle"}"#, "text-anchor"),
    ] {
        match engine.set_style("stops".into(), bad.into()) {
            Err(MapError::Paint(m)) => assert!(m.contains(says), "{bad}: {m}"),
            other => panic!("{bad}: {other:?}"),
        }
    }
}

/// MapLibre's layer model: a text-only style is a symbol layer and draws no
/// circle under its labels; circle plus text draws both; a bare point
/// layer keeps the default circle.
#[test]
fn a_text_only_layer_draws_no_circle_and_a_bare_one_keeps_its_default() {
    let engine = engine();
    let circles = |layer: &str| engine.lock().value.host.draws_circles(layer);
    engine.upsert_points("bare".into(), vec![stop("a", 103.85, 1.29)]).unwrap();
    assert!(circles("bare"), "a bare point layer keeps the default circle");
    engine.set_style("labels".into(), r#"{"text-field": ["get", "name"]}"#.into()).unwrap();
    engine.upsert_points("labels".into(), vec![stop("b", 103.851, 1.29)]).unwrap();
    assert!(!circles("labels"), "text only: no circle");
    engine.set_style("both".into(), r#"{"circle-radius": 4, "text-field": ["get", "name"]}"#.into()).unwrap();
    engine.upsert_points("both".into(), vec![stop("c", 103.852, 1.29)]).unwrap();
    assert!(circles("both"), "circle plus text draws the circle");
    let labelled: Vec<String> = place(&engine).into_iter().map(|(layer, _, _)| layer).collect();
    assert!(labelled.contains(&"labels".to_string()) && labelled.contains(&"both".to_string()), "{labelled:?}");
}

// -- system fonts (CoreText; macOS has the same API and the same families) --

#[cfg(target_vendor = "apple")]
mod system_fonts {
    use gpui_maps_ios::system_fonts::{shape_label, ShapedGlyph};

    fn chars(text: &str) -> usize {
        text.chars().filter(|c| !c.is_whitespace()).count()
    }

    fn inked(glyphs: &[ShapedGlyph], text: &str) -> Vec<ShapedGlyph> {
        glyphs.iter().filter(|g| !text[g.start..g.end].trim().is_empty()).cloned().collect()
    }

    #[test]
    fn the_system_font_loader_finds_a_face_for_cjk_thai_arabic_and_hebrew() {
        let mut db = glyphon_db();
        let report = gpui_maps_ios::system_fonts::load(&mut db, "en-US");
        use gpui_maps_ios::system_fonts::Script;
        for script in [Script::Han, Script::Hiragana, Script::Thai, Script::Arabic, Script::Hebrew, Script::Hangul] {
            assert!(!report.missing_scripts.contains(&script), "{script:?} missing: {report:?}");
        }
        assert!(report.files > 3 && report.faces > report.files, "{report:?}");
    }

    fn glyphon_db() -> gpui_maps_ios::system_fonts::Database {
        gpui_maps_ios::system_fonts::Database::new()
    }

    #[test]
    fn japanese_and_thai_shape_with_no_missing_glyphs() {
        for text in ["東京駅前", "สถานีขนส่ง", "서울역", "台北車站"] {
            let glyphs = inked(&shape_label(text), text);
            assert!(!glyphs.is_empty(), "{text}");
            for g in &glyphs {
                assert_ne!(g.glyph_id, 0, "{text}: .notdef for {:?} in {}", &text[g.start..g.end], g.family);
                assert_ne!(g.family, "Noto Sans", "{text}: {:?} drawn by the Latin subset", &text[g.start..g.end]);
            }
        }
    }

    #[test]
    fn arabic_joins_and_runs_right_to_left() {
        let text = "محطة الحافلات";
        let glyphs = inked(&shape_label(text), text);
        assert!(glyphs.iter().all(|g| g.glyph_id != 0 && g.rtl), "{glyphs:?}");
        // Lam-alef in الحافلات is one ligature glyph: fewer glyphs than letters.
        assert!(glyphs.len() < chars(text), "{} glyphs for {} letters", glyphs.len(), chars(text));
        // Contextual forms: the same letter (ح) isolated and joined to what
        // follows takes different glyphs.
        let isolated = inked(&shape_label("ح"), "ح")[0].glyph_id;
        let initial = shape_label("حا").into_iter().find(|g| g.start == 0).unwrap().glyph_id;
        assert_ne!(isolated, initial, "ح isolated and initial share a glyph: no contextual forms");
        // Visual order: the first letter in logical order is the rightmost.
        let first = glyphs.iter().find(|g| g.start == 0).unwrap();
        assert!(glyphs.iter().all(|g| g.x <= first.x), "first letter not rightmost: {glyphs:?}");
    }

    #[test]
    fn a_mixed_label_orders_digits_and_arabic_correctly() {
        let text = "12 محطة";
        let glyphs = inked(&shape_label(text), text);
        let one = glyphs.iter().find(|g| &text[g.start..g.end] == "1").expect("1");
        let two = glyphs.iter().find(|g| &text[g.start..g.end] == "2").expect("2");
        // Digits keep left-to-right order inside the run...
        assert!(one.x < two.x, "{glyphs:?}");
        // ...and, the paragraph being Arabic (first strong letter), the
        // number that comes first logically sits to the right of the word.
        let arabic: Vec<_> = glyphs.iter().filter(|g| g.start >= 3).collect();
        assert!(arabic.iter().all(|g| g.x < one.x && g.rtl), "{glyphs:?}");
    }
}

fn line(id: &str, lat: f64, color: &str) -> LineFeature {
    LineFeature {
        id: id.into(),
        coords: vec![LngLat { lon: 103.84, lat }, LngLat { lon: 103.86, lat }],
        props: HashMap::from([("color".to_string(), PropValue::Text { value: color.into() })]),
    }
}

fn data_driven(engine: &MapEngine, layer: &str) -> Option<bool> {
    engine.lock().value.host.is_data_driven(layer)
}

/// The device bug at 2a2029fc: a frame between `set_style` and the first
/// upsert flushed the still-empty layer, which removed its source -- and
/// with it the declared `["get", "color"]` -- so every line drew default
/// blue. Both orders, lines and fills, and a re-upsert.
#[test]
fn get_colour_survives_a_frame_between_style_and_features() {
    let engine = engine();
    engine
        .set_style("network".into(), r#"{"line-width": 2.5, "line-color": ["get", "color"], "line-opacity": 0.6}"#.into())
        .unwrap();
    engine.set_style("buses".into(), r#"{"fill-color": ["get", "color"]}"#.into()).unwrap();
    engine.render(0.0); // the frame that used to drop the declaration
    engine.upsert_lines("network".into(), vec![line("a", 1.29, "#F4A261"), line("b", 1.291, "#FF5D8F")]).unwrap();
    let ring = heading_marker(103.85, 1.29, 0.0, 15.0);
    let bus = |id: &str, color: &str| PolygonFeature {
        id: id.into(),
        rings: vec![ring.clone()],
        props: HashMap::from([("color".to_string(), PropValue::Text { value: color.into() })]),
    };
    engine.upsert_polygons("buses".into(), vec![bus("b1", "#3A86FF"), bus("b2", "rgb(255, 93, 143)")]).unwrap();
    assert_eq!(data_driven(&engine, "network"), Some(true), "style, frame, then features");
    assert_eq!(data_driven(&engine, "buses"), Some(true));
    // Re-upsert by id keeps it.
    engine.upsert_lines("network".into(), vec![line("a", 1.292, "#3A86FF")]).unwrap();
    engine.render(0.016);
    assert_eq!(data_driven(&engine, "network"), Some(true), "after a re-upsert");
    // Features first, then the style.
    engine.upsert_lines("route".into(), vec![line("r", 1.29, "#FF5D8F")]).unwrap();
    engine.render(0.032);
    engine.set_style("route".into(), r#"{"line-width": 5, "line-color": ["get", "color"]}"#.into()).unwrap();
    assert_eq!(data_driven(&engine, "route"), Some(true), "features, then style");
    // Emptied and refilled: still declared.
    engine.clear("route".into());
    engine.render(0.048);
    engine.upsert_lines("route".into(), vec![line("r", 1.29, "#FF5D8F")]).unwrap();
    assert_eq!(data_driven(&engine, "route"), Some(true), "cleared, then refilled");
}

/// MapLibre's zoom range on an overlay layer: drawn while
/// `minzoom <= z < maxzoom`, for its circles (picking) and labels alike.
#[test]
fn min_and_max_zoom_hide_a_layer_outside_its_range() {
    let engine = engine();
    engine
        .set_style("stops".into(), r#"{"circle-radius": 5, "text-field": ["get", "name"], "min-zoom": 15, "maxzoom": 17}"#.into())
        .unwrap();
    engine.upsert_points("stops".into(), vec![stop("marina", 103.85, 1.29)]).unwrap();
    let at = |z: f64| {
        engine.jump_to(CameraUpdate { zoom: Some(z), ..Default::default() });
        let picked = engine.feature_at(MID.0, MID.1).is_some();
        let labelled = !place(&engine).is_empty();
        (picked, labelled)
    };
    assert_eq!(at(14.9), (false, false), "below min-zoom");
    assert_eq!(at(15.0), (true, true), "at min-zoom");
    assert_eq!(at(16.99), (true, true));
    assert_eq!(at(17.0), (false, false), "max-zoom is exclusive");
    assert!(matches!(engine.set_style("x".into(), r#"{"min-zoom": "15"}"#.into()), Err(MapError::Paint(_))));
}

/// swat-dark at street zoom over Singapore: the buildings stand (the plan
/// draws them at real height), and the host's overlays go to their own pass
/// over them, depth cleared (`render_overlay_above_buildings`), so a route
/// under a tower draws whole. `set_buildings_3d(false)` keeps drawing the
/// footprints with zero height -- no walls -- without recompiling the style.
#[test]
fn overlays_draw_over_buildings_and_buildings_can_lie_flat() {
    let engine = engine();
    engine.set_basemap("swat-dark".into()).unwrap();
    engine.jump_to(CameraUpdate { zoom: Some(16.5), ..Default::default() });
    let summary = || engine.lock().value.host.buildings_summary();
    let (steps, per_metre, above) = summary();
    assert!(steps > 0 && per_metre > 0.0, "standing buildings: {steps} steps, {per_metre} per metre");
    assert!(above, "the embed draws its overlays above the buildings");
    engine.set_buildings_3d(false);
    let (flat_steps, flat_per_metre, _) = summary();
    assert_eq!(flat_per_metre, 0.0, "flat: every roof at the ground");
    assert!(flat_steps > 0, "the footprints still draw");
    engine.set_buildings_3d(true);
    assert!(summary().1 > 0.0, "and stand again");
}

/// RTL mirrors the compass to the top-left: on a rotated map, a tap on the
/// mirrored disc is the compass's (reset north, no feature tap), and the
/// LTR corner is plain map again. A polygon under the whole view tells
/// which taps the chrome took.
#[test]
fn rtl_moves_the_compass_to_the_top_left() {
    let engine = engine();
    engine.set_insets(EdgeInsets { top: 47.0, bottom: 34.0, left: 0.0, right: 0.0 });
    let c = engine.camera();
    let (w, s) = (0.02, 0.02);
    let cover = vec![
        LngLat { lon: c.lon - w, lat: c.lat - s },
        LngLat { lon: c.lon + w, lat: c.lat - s },
        LngLat { lon: c.lon + w, lat: c.lat + s },
        LngLat { lon: c.lon - w, lat: c.lat + s },
        LngLat { lon: c.lon - w, lat: c.lat - s },
    ];
    engine
        .upsert_polygons("ground".into(), vec![PolygonFeature { id: "g".into(), rings: vec![cover], props: HashMap::new() }])
        .unwrap();
    let recorder = Arc::new(Recorder::default());
    engine.set_delegate(recorder.clone());
    engine.jump_to(CameraUpdate { bearing_deg: Some(40.0), ..Default::default() });
    let centre = |rtl: bool| {
        let (left, top, size) = gpui_maps_ios::compass::compass_rect_dir(390.0, 47.0, rtl);
        (f64::from(left + size / 2.0), f64::from(top + size / 2.0))
    };
    let taps = |r: &Recorder| r.taps.lock().unwrap().len();
    // LTR: the top-right disc is the compass; the top-left is map.
    tap(&engine, 1, centre(false));
    assert_eq!(taps(&recorder), 0, "LTR: top-right is the compass");
    engine.jump_to(CameraUpdate { bearing_deg: Some(40.0), ..Default::default() });
    tap(&engine, 2, centre(true));
    assert_eq!(taps(&recorder), 1, "LTR: top-left is map");
    // RTL: mirrored.
    engine.set_layout_direction(true);
    engine.jump_to(CameraUpdate { bearing_deg: Some(40.0), ..Default::default() });
    tap(&engine, 3, centre(true));
    assert_eq!(taps(&recorder), 1, "RTL: top-left is the compass");
    engine.jump_to(CameraUpdate { bearing_deg: Some(40.0), ..Default::default() });
    tap(&engine, 4, centre(false));
    assert_eq!(taps(&recorder), 2, "RTL: top-right is map");
}

fn sources(engine: &MapEngine) -> Vec<String> {
    engine.lock().value.host.engine_sources()
}

/// The device bug at d35d8371: a styled layer that stayed EMPTY through
/// frames (and was cleared while never filled) drew its first features in
/// the default colour, and restyling did not recover it. The engine now
/// never holds an empty declared source: a layer's declared expressions go
/// in together with its features, every time the source is (re)built. Each
/// sequence the host can produce, lines and fills, on a real GPU surface
/// where this machine has one (frames prepare the overlays) and headless
/// otherwise.
#[test]
fn every_style_and_fill_sequence_draws_data_driven_paint() {
    #[cfg(target_vendor = "apple")]
    let layer = objc2_quartz_core::CAMetalLayer::new();
    #[cfg(target_vendor = "apple")]
    let ptr = objc2::rc::Retained::as_ptr(&layer) as usize as u64;
    #[cfg(not(target_vendor = "apple"))]
    let ptr = 0;
    let engine = MapEngine::new(ptr, 1170, 2532, 3.0, String::new()).unwrap();
    engine.jump_to(CameraUpdate { lon: Some(103.85), lat: Some(1.29), zoom: Some(14.0), ..Default::default() });
    let mut t = 0.0;
    let mut frames = |n: usize| {
        for _ in 0..n {
            t += 0.016;
            engine.render(t);
        }
    };
    let line_style = r#"{"line-width": 5, "line-color": ["get", "color"]}"#;
    let fill_style = r#"{"fill-color": ["get", "color"]}"#;
    let ring = heading_marker(103.85, 1.29, 0.0, 15.0);
    let bus = |id: &str, color: &str| PolygonFeature {
        id: id.into(),
        rings: vec![ring.clone()],
        props: HashMap::from([("color".to_string(), PropValue::Text { value: color.into() })]),
    };

    // 1. style -> clear while empty -> fill.
    engine.set_style("route".into(), line_style.into()).unwrap();
    engine.set_style("buses".into(), fill_style.into()).unwrap();
    frames(3);
    engine.clear("route".into());
    engine.clear("buses".into());
    frames(3);
    assert!(!sources(&engine).iter().any(|s| s == "route" || s == "buses"), "an empty layer holds no engine source");
    engine.upsert_lines("route".into(), vec![line("r1", 1.29, "#F4A261")]).unwrap();
    engine.upsert_polygons("buses".into(), vec![bus("b1", "#3A86FF")]).unwrap();
    frames(2);
    assert_eq!(data_driven(&engine, "route"), Some(true), "style, clear while empty, fill");
    assert_eq!(data_driven(&engine, "buses"), Some(true));

    // 2. fill -> clear -> refill.
    engine.clear("route".into());
    frames(2);
    engine.upsert_lines("route".into(), vec![line("r2", 1.29, "#FF5D8F")]).unwrap();
    frames(2);
    assert_eq!(data_driven(&engine, "route"), Some(true), "fill, clear, refill");

    // 3. set_style twice with identical JSON, filled and between frames.
    engine.set_style("route".into(), line_style.into()).unwrap();
    frames(1);
    engine.set_style("route".into(), line_style.into()).unwrap();
    assert_eq!(data_driven(&engine, "route"), Some(true), "identical restyle");

    // 4. a constant restyle really replaces the expression, and the
    //    expression comes back.
    engine.set_style("route".into(), r##"{"line-color": "#FF0000"}"##.into()).unwrap();
    assert_eq!(data_driven(&engine, "route"), Some(false), "a constant colour is not data-driven");
    engine.set_style("route".into(), line_style.into()).unwrap();
    assert_eq!(data_driven(&engine, "route"), Some(true));
    #[cfg(target_vendor = "apple")]
    drop(engine);
}

fn scratch_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("gpui-maps-embed-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// The tile disk cache: a body put is got back; a stale one is not; the
/// directory stays under its byte cap, newest kept; file names do not say
/// which tile (no z/x/y on disk).
#[test]
fn the_disk_cache_keeps_bodies_bounded_and_unnamed() {
    use gpui_maps_ios::disk_cache::DiskCache;
    let dir = scratch_dir("cache");
    let cache = DiskCache::open(&dir, 10_000, std::time::Duration::from_secs(3600)).unwrap();
    let url = |i: u32| format!("https://tiles.openfreemap.org/planet/20250101/14/{}/{}.pbf", 12900 + i, 8100 + i);
    assert_eq!(cache.get(&url(0)), None);
    cache.put(&url(0), b"tile zero");
    assert_eq!(cache.get(&url(0)).as_deref(), Some(&b"tile zero"[..]));
    for i in 1..=30 {
        cache.put(&url(i), &vec![i as u8; 1000]);
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let (bytes, hits, _) = cache.stats();
    assert!(bytes <= 10_000, "{bytes} bytes over the cap");
    assert!(hits >= 1);
    assert!(cache.get(&url(30)).is_some(), "the newest survives");
    assert_eq!(cache.get(&url(1)), None, "the oldest went");
    for entry in std::fs::read_dir(&dir).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().to_string();
        assert!(!name.contains("12930") && !name.contains("/14/"), "{name} names a tile");
    }
    // Stale: a zero max age refetches everything.
    let stale = DiskCache::open(&dir, 10_000, std::time::Duration::ZERO).unwrap();
    assert_eq!(stale.get(&url(30)), None);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A cache directory given to `MapEngine::new` is created and used.
#[test]
fn map_engine_opens_its_tile_cache() {
    let dir = scratch_dir("engine");
    let _engine = MapEngine::new(0, 1170, 2532, 3.0, dir.to_string_lossy().to_string()).unwrap();
    assert!(dir.join("gpui-maps-tiles-v1").is_dir());
    let _ = std::fs::remove_dir_all(&dir);
}

/// `set_points`-shaped load, timed: N vehicles with two props, first push
/// then every one moved by id, each followed by the push a frame makes.
/// Run with `cargo test -p gpui-maps-embed --release -- --ignored
/// bench_upsert_points --nocapture`.
#[test]
#[ignore = "a benchmark: run explicitly, see the doc comment"]
fn bench_upsert_points() {
    for n in [400usize, 3_400, 20_000] {
        let engine = engine();
        engine.set_style("buses".into(), r#"{"circle-radius": 5, "circle-color": ["get", "color"]}"#.into()).unwrap();
        let fleet = |shift: f64| -> Vec<PointFeature> {
            (0..n)
                .map(|i| {
                    let a = i as f64 * 0.618;
                    PointFeature {
                        id: format!("bus-{i}"),
                        lon: 103.85 + (a.cos() * 0.1) + shift,
                        lat: 1.29 + (a.sin() * 0.05),
                        props: HashMap::from([
                            ("color".to_string(), PropValue::Text { value: "#3A86FF".into() }),
                            ("route".to_string(), PropValue::Text { value: format!("R{}", i % 40) }),
                        ]),
                    }
                })
                .collect()
        };
        let time = |label: &str, f: &mut dyn FnMut()| {
            let started = Instant::now();
            f();
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            println!("bench_upsert_points n={n:>6} {label:<18} {ms:>8.2} ms");
            ms
        };
        let first = fleet(0.0);
        time("first push", &mut || {
            engine.upsert_points("buses".into(), first.clone()).unwrap();
            engine.lock().value.host.flush();
        });
        let mut worst: f64 = 0.0;
        for step in 1..=5 {
            let moved = fleet(step as f64 * 1e-4);
            worst = worst.max(time("move all by id", &mut || {
                engine.upsert_points("buses".into(), moved.clone()).unwrap();
                engine.lock().value.host.flush();
            }));
        }
        println!("bench_upsert_points n={n:>6} worst move          {worst:>8.2} ms");
    }
}

/// The transit app's three layers, bottom first, as it creates them: an
/// empty-until-opened "route" line BELOW the "buses" polygons and their
/// "buses-ring".
fn route_and_buses(engine: &MapEngine) {
    let ring = heading_marker(103.85, 1.29, 0.0, 15.0);
    let bus = PolygonFeature { id: "bus-7".into(), rings: vec![ring], props: HashMap::new() };
    engine.upsert_lines("route".into(), vec![line("r", 1.29, "#FF5D8F")]).unwrap();
    engine.upsert_polygons("buses".into(), vec![bus.clone()]).unwrap();
    engine.upsert_polygons("buses-ring".into(), vec![bus]).unwrap();
}

const DRAW_ORDER: [&str; 3] = ["route", "buses", "buses-ring"];

/// The engine's overlay sources in draw order (bottom first), without the
/// engine's own always-present, always-first default source.
fn draw_order(engine: &MapEngine) -> Vec<String> {
    sources(engine).into_iter().filter(|n| !n.is_empty()).collect()
}

/// The transit app's bug: a layer emptied (its source leaves the engine)
/// and refilled was re-created on TOP of the engine's stack, so the route
/// drew over the buses once a line opened. The engine's draw order is the
/// host's creation order, whatever the layer's empty history.
#[test]
fn an_emptied_and_refilled_layer_keeps_its_draw_order() {
    let engine = engine();
    route_and_buses(&engine);
    assert_eq!(draw_order(&engine), DRAW_ORDER);
    engine.clear("route".into());
    assert_eq!(draw_order(&engine), ["buses", "buses-ring"], "an empty layer holds no source");
    engine.upsert_lines("route".into(), vec![line("r", 1.29, "#FF5D8F")]).unwrap();
    assert_eq!(draw_order(&engine), DRAW_ORDER, "refilled: still under the buses");
    // A middle layer, emptied by `remove` rather than `clear`.
    assert_eq!(engine.remove("buses".into(), vec!["bus-7".into()]), 1);
    let ring = heading_marker(103.85, 1.29, 0.0, 15.0);
    engine.upsert_polygons("buses".into(), vec![PolygonFeature { id: "bus-8".into(), rings: vec![ring], props: HashMap::new() }]).unwrap();
    assert_eq!(draw_order(&engine), DRAW_ORDER, "refilled middle layer");
}

/// The same for a layer hidden by its `min-zoom` and shown again.
#[test]
fn a_layer_hidden_by_its_zoom_range_keeps_its_draw_order() {
    let engine = engine();
    engine.set_style("route".into(), r#"{"line-width": 4, "min-zoom": 15}"#.into()).unwrap();
    route_and_buses(&engine);
    let at = |z: f64| {
        engine.jump_to(CameraUpdate { zoom: Some(z), ..Default::default() });
        engine.render(0.0);
        draw_order(&engine)
    };
    assert_eq!(at(16.0), DRAW_ORDER);
    assert_eq!(at(14.0), ["buses", "buses-ring"], "below min-zoom: hidden");
    assert_eq!(at(16.0), DRAW_ORDER, "shown again: still under the buses");
}

/// The same for a layer toggled off and on the way Studio for iOS does it
/// (`set_layer_visible`: clear; then restyle, clear and refill), and for a
/// restyle alone, which rebuilds the source.
#[test]
fn a_layer_toggled_off_and_on_keeps_its_draw_order() {
    let engine = engine();
    let style = r#"{"line-width": 5, "line-color": ["get", "color"]}"#;
    engine.set_style("route".into(), style.into()).unwrap();
    route_and_buses(&engine);
    assert_eq!(draw_order(&engine), DRAW_ORDER);
    engine.clear("route".into());
    assert_eq!(draw_order(&engine), ["buses", "buses-ring"], "off");
    engine.set_style("route".into(), style.into()).unwrap();
    engine.clear("route".into());
    engine.upsert_lines("route".into(), vec![line("r", 1.29, "#FF5D8F")]).unwrap();
    assert_eq!(draw_order(&engine), DRAW_ORDER, "on again: still under the buses");
    engine.set_style("route".into(), r#"{"line-width": 8, "line-color": ["get", "color"]}"#.into()).unwrap();
    assert_eq!(draw_order(&engine), DRAW_ORDER, "restyled: rebuilt in place");
}

#[test]
fn a_highlight_lights_one_feature_follows_it_and_is_never_picked() {
    let engine = engine();
    engine.upsert_points("stops".into(), vec![stop("a", 103.85, 1.29), stop("b", 103.86, 1.29)]).unwrap();
    engine.set_style("stops".into(), r##"{"circle-radius": ["get", "r"], "circle-color": "#e53935"}"##.into()).unwrap();
    assert!(!engine.set_highlight("stops".into(), Some("zzz".into())), "an id the layer does not hold");
    assert!(!engine.set_highlight("nope".into(), Some("a".into())), "a layer the map does not hold");
    assert!(engine.set_highlight("stops".into(), Some("a".into())));
    assert_eq!(engine.highlighted(), Some(FeatureHit { layer: "stops".into(), id: "a".into() }));
    {
        let mut g = engine.lock();
        let sources = g.value.host.engine_sources();
        assert!(sources.iter().filter(|s| s.contains("highlight")).count() >= 2, "the casing and ring draw: {sources:?}");
        assert!(sources.last().is_some_and(|s| s.contains("highlight")), "over every layer: {sources:?}");
    }
    // A later layer still goes under the highlight.
    engine.upsert_points("later".into(), vec![stop("x", 103.80, 1.30)]).unwrap();
    {
        let mut g = engine.lock();
        let sources = g.value.host.engine_sources();
        let later = sources.iter().position(|s| s == "later").unwrap();
        let first_light = sources.iter().position(|s| s.contains("highlight")).unwrap();
        assert!(later < first_light, "{sources:?}");
    }
    // A tap on the lit point is a tap on the point.
    let rec = Arc::new(Recorder::default());
    engine.set_delegate(rec.clone());
    tap(&engine, 1, MID);
    assert_eq!(rec.taps.lock().unwrap().as_slice(), [("stops".to_string(), "a".to_string())]);
    // It follows the feature; it goes with it.
    engine.upsert_points("stops".into(), vec![stop("a", 103.851, 1.291)]).unwrap();
    assert!(engine.highlighted().is_some());
    engine.remove("stops".into(), vec!["a".into()]);
    assert_eq!(engine.highlighted(), None);
    {
        let mut g = engine.lock();
        assert!(!g.value.host.engine_sources().iter().any(|s| s.contains("highlight")), "nothing left lit");
    }
    // A line lights its outline; None clears.
    engine.upsert_lines("route".into(), vec![LineFeature { id: "r1".into(), coords: vec![LngLat { lon: 103.84, lat: 1.28 }, LngLat { lon: 103.86, lat: 1.30 }], props: HashMap::new() }]).unwrap();
    assert!(engine.set_highlight("route".into(), Some("r1".into())));
    assert!(!engine.set_highlight(String::new(), None));
    assert_eq!(engine.highlighted(), None);
}

#[test]
fn under_an_archive_the_online_tiles_draw_outside_it_until_they_fail() {
    use gpui_maps_ios::app::tile_from_online;
    use gpui_maps_ios::geo::{LonLatBounds, TileId};
    // Serbia's bounds, as the archive's header states them.
    let serbia = LonLatBounds { min_lon: 18.8, min_lat: 42.2, max_lon: 23.0, max_lat: 46.2 };
    let novi_sad = TileId { z: 14, x: 9095, y: 5877 };
    let vienna = TileId { z: 14, x: 8937, y: 5681 };
    let europe = TileId { z: 4, x: 8, y: 5 };
    assert!(!tile_from_online(novi_sad, serbia, false), "inside: the archive");
    assert!(tile_from_online(vienna, serbia, false), "outside: online");
    assert!(tile_from_online(europe, serbia, false), "half inside: online, the archive's copy is half empty");
    assert!(!tile_from_online(europe, serbia, true), "online failed: the archive's half tile beats none");
}

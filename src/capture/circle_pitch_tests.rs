//! `circle-pitch-alignment` and `circle-pitch-scale` on the point path's
//! instanced circles (`render::circles`, `shader.wgsl`'s `vs_circle`),
//! measured off headless frames: one red circle where the ground turns
//! away from the camera -- at the globe's limb, and far up a pitched flat
//! map -- and the aspect ratio of the box its pixels fill.
//!
//! MapLibre's defaults (`viewport` alignment, `map` scale) keep a circle
//! ROUND on screen wherever it is (`circle.vertex.glsl`'s
//! `!u_pitch_with_map` branch); `map` alignment lays it on the ground, so
//! it foreshortens like the ground does.
use super::*;

fn point(lon: f64, lat: f64, radius: f64) -> String {
    serde_json::json!({"type": "FeatureCollection", "features": [{
        "type": "Feature",
        "properties": {"circle-color": "#ff0000", "circle-radius": radius},
        "geometry": {"type": "Point", "coordinates": [lon, lat]},
    }]})
    .to_string()
}

fn paint(alignment: &str, scale: &str) -> String {
    serde_json::json!({"circle-pitch-alignment": alignment, "circle-pitch-scale": scale}).to_string()
}

/// The bounding box `(width, height)` of the frame's red pixels.
fn red_box(shot: &Capture) -> (u32, u32) {
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
    for (i, p) in shot.rgba.as_chunks::<4>().0.iter().enumerate() {
        if p[0] > 200 && p[1] < 60 && p[2] < 60 {
            let (x, y) = (i as u32 % shot.width, i as u32 / shot.width);
            (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
        }
    }
    assert!(x1 >= x0 && y1 >= y0, "no red pixel drawn");
    (x1 - x0 + 1, y1 - y0 + 1)
}

fn shoot(request: &CaptureRequest, name: &str) -> (u32, u32) {
    let shot = capture(request, &TileStore::new(), &Style { layers: Vec::new(), ..Style::default() }).expect("a headless device");
    if let Some(dir) = std::env::var_os("GPUI_MAP_CIRCLE_SHOT") {
        shot.save_png(&std::path::Path::new(&dir).join(format!("{name}.png"))).expect("save");
    }
    red_box(&shot)
}

/// The globe at z1.5 centred on (0, 0), with a 12 px circle 65 degrees east:
/// the ground there faces the camera at about cos 65 = 0.42.
fn limb(alignment: &str, scale: &str) -> CaptureRequest {
    CaptureRequest {
        center_lon: 0.0,
        center_lat: 0.0,
        zoom: 1.5,
        width: 640,
        height: 640,
        clear: Clear::SENTINEL,
        backdrop: false,
        overlay: Some(point(65.0, 0.0, 12.0)),
        overlay_points: true,
        overlay_paint: Some(paint(alignment, scale)),
        ..CaptureRequest::default()
    }
}

/// A flat map at z4 pitched 60 degrees, with a 12 px circle north of centre
/// (up the screen, away from the camera).
fn pitched(alignment: &str, scale: &str) -> CaptureRequest {
    CaptureRequest {
        center_lon: 0.0,
        center_lat: 0.0,
        zoom: 4.0,
        pitch: 60f32.to_radians(),
        width: 640,
        height: 640,
        flat: true,
        globe: false,
        clear: Clear::SENTINEL,
        backdrop: false,
        overlay: Some(point(0.0, 4.0, 12.0)),
        overlay_points: true,
        overlay_paint: Some(paint(alignment, scale)),
        ..CaptureRequest::default()
    }
}

fn aspect((w, h): (u32, u32)) -> f64 {
    f64::from(w) / f64::from(h)
}

#[test]
fn a_viewport_aligned_circle_at_the_globe_limb_is_round() {
    let (w, h) = shoot(&limb("viewport", "map"), "limb_viewport");
    assert!((aspect((w, h)) - 1.0).abs() < 0.12, "viewport circle at the limb is {w}x{h} px");
    // `circle-pitch-scale: map` shrinks it by its depth over the centre's,
    // never past a few pixels at this distance.
    assert!((18..=27).contains(&w), "a 12 px radius drew {w} px wide");
}

#[test]
fn a_map_aligned_circle_at_the_globe_limb_lies_on_the_ground() {
    let (w, h) = shoot(&limb("map", "map"), "limb_map");
    assert!(aspect((w, h)) < 0.6, "map circle at the limb is {w}x{h} px: not foreshortened");
}

#[test]
fn a_viewport_aligned_circle_on_a_pitched_flat_map_is_round() {
    let (w, h) = shoot(&pitched("viewport", "map"), "pitched_viewport");
    assert!((aspect((w, h)) - 1.0).abs() < 0.12, "viewport circle on a pitched map is {w}x{h} px");
}

#[test]
fn a_map_aligned_circle_on_a_pitched_flat_map_is_foreshortened() {
    let (w, h) = shoot(&pitched("map", "map"), "pitched_map");
    assert!(aspect((h, w)) < 0.7, "map circle on a pitched map is {w}x{h} px: not foreshortened");
}

/// `circle-pitch-scale`: `map` sizes a circle by its distance (one far up a
/// pitched map draws smaller than its declared radius), `viewport` keeps
/// the declared radius wherever it is.
#[test]
fn circle_pitch_scale_viewport_keeps_the_declared_radius_far_up_a_pitched_map() {
    let (map_w, _) = shoot(&pitched("viewport", "map"), "pitched_scale_map");
    let (view_w, view_h) = shoot(&pitched("viewport", "viewport"), "pitched_scale_viewport");
    assert!((aspect((view_w, view_h)) - 1.0).abs() < 0.12, "{view_w}x{view_h}");
    assert!((23..=27).contains(&view_w), "a 12 px radius at viewport scale drew {view_w} px wide");
    assert!(map_w + 3 < view_w, "map scale {map_w} px, viewport scale {view_w} px: no shrink with distance");
}

/// A declared value outside the spec's two is refused, not ignored.
#[test]
fn an_unknown_circle_pitch_value_is_refused() {
    let paint: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(r#"{"circle-pitch-alignment": "auto"}"#).unwrap();
    assert!(crate::overlay::source_paint::SourcePaint::compile(&paint).is_err());
}


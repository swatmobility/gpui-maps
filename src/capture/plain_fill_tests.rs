//! **Plain never shows past the world**: a Plain map smaller than the
//! window shows no grey areas around the map. A red polygon over the whole Mercator square, drawn on a sentinel
//! clear with no backdrop: every magenta pixel is a pixel past the world's
//! edge. Unconstrained, a zoomed-out Plain camera shows bands of them; held
//! by `camera_control::plain_constrain`, as `MapView` holds it, none.
use super::*;

/// The whole Mercator square, red.
fn world() -> String {
    let (n, s) = (85.06, -85.06);
    serde_json::json!({"type": "FeatureCollection", "features": [{
        "type": "Feature",
        "properties": {"fill-color": "#ff0000", "fill-opacity": 1.0},
        "geometry": {"type": "Polygon", "coordinates": [[[-180.0, s], [180.0, s], [180.0, n], [-180.0, n], [-180.0, s]]]},
    }]})
    .to_string()
}

fn plain(lon: f64, lat: f64, zoom: f64, width: u32, height: u32) -> CaptureRequest {
    CaptureRequest {
        center_lon: lon,
        center_lat: lat,
        zoom,
        width,
        height,
        flat: true,
        globe: false,
        clear: Clear::SENTINEL,
        backdrop: false,
        overlay: Some(world()),
        ..CaptureRequest::default()
    }
}

fn constrained(r: CaptureRequest) -> CaptureRequest {
    let (lon, lat, zoom) = crate::camera_control::plain_constrain(r.center_lon, r.center_lat, r.zoom, f64::from(r.width), f64::from(r.height));
    CaptureRequest { center_lon: lon, center_lat: lat, zoom, ..r }
}

/// Magenta pixels: past the world's edge.
fn outside(shot: &Capture) -> usize {
    shot.rgba.as_chunks::<4>().0.iter().filter(|p| p[0] > 200 && p[1] < 60 && p[2] > 200).count()
}

#[test]
fn a_zoomed_out_plain_frame_has_no_band_past_the_world() {
    let style = Style { layers: Vec::new(), ..Style::default() };
    let mut session = CaptureSession::new_blocking(None).expect("a headless device");
    for (lon, lat, zoom, w, h) in [(0.0, 0.0, 0.5, 960, 600), (170.0, 70.0, 1.0, 960, 600), (-60.0, -50.0, 0.0, 480, 900), (175.0, 80.0, 2.0, 1280, 400)] {
        let before = session.render(&plain(lon, lat, zoom, w, h), &TileStore::new(), &style).expect("render");
        let after_req = constrained(plain(lon, lat, zoom, w, h));
        let after = session.render(&after_req, &TileStore::new(), &style).expect("render");
        if let Some(dir) = std::env::var_os("GPUI_MAP_PLAIN_SHOT") {
            before.save_png(&std::path::Path::new(&dir).join(format!("plain-before-{lon}-{lat}-z{zoom}.png"))).expect("save");
            after.save_png(&std::path::Path::new(&dir).join(format!("plain-after-{lon}-{lat}-z{zoom}.png"))).expect("save");
        }
        let total = (w * h) as usize;
        let (b, a) = (outside(&before), outside(&after));
        eprintln!("PLAIN_BANDS ({lon}, {lat}, z{zoom}) {w}x{h}: {b} px past the world unconstrained, {a} held (z{:.3})", after_req.zoom);
        assert!(b > total / 50, "the control: unconstrained z{zoom} must show past the world ({b} of {total})");
        assert_eq!(a, 0, "held by plain_constrain, no pixel past the world at ({lon}, {lat}, z{zoom})");
    }
}

//! A CSV of world cities, end to end: bytes -> `overlay::csv` -> the
//! store's point path -> the headless frame, with the CSV's typed columns
//! driving data-driven paint (`capital`, a bool, picks the colour;
//! `population`, a number, the radius).
//!
//! `tests/fixtures/world_cities.csv` is 46 cities from Natural Earth's
//! 1:10m populated places (public domain, v5.1.2): positions rounded to 0.01
//! degree, `pop_max` as the population, `adm0cap` as the capital flag.
use super::*;

const CITIES: &[u8] = include_bytes!("../../tests/fixtures/world_cities.csv");
const CAPITAL: [u8; 3] = [0xe4, 0x57, 0x2e];
const OTHER: [u8; 3] = [0x2e, 0x86, 0xab];

fn paint() -> String {
    serde_json::json!({
        "circle-color": ["case", ["==", ["get", "capital"], true], "#e4572e", "#2e86ab"],
        "circle-radius": ["interpolate", ["linear"], ["get", "population"], 100_000, 3, 40_000_000, 12],
    })
    .to_string()
}

fn request() -> CaptureRequest {
    CaptureRequest {
        center_lon: 10.0,
        center_lat: 20.0,
        zoom: 1.0,
        width: 1024,
        height: 640,
        flat: true,
        globe: false,
        clear: Clear::SENTINEL,
        backdrop: false,
        overlay_csv: Some(CITIES.to_vec()),
        overlay_points: true,
        overlay_paint: Some(paint()),
        ..CaptureRequest::default()
    }
}

/// The flat Web Mercator pixel of `(lon, lat)` for `request`'s camera.
fn pixel_of(request: &CaptureRequest, lon: f64, lat: f64) -> (f64, f64) {
    let merc = |lon: f64, lat: f64| {
        let x = (lon + 180.0) / 360.0;
        let s = lat.to_radians().sin();
        let y = 0.5 - ((1.0 + s) / (1.0 - s)).ln() / (4.0 * std::f64::consts::PI);
        (x, y)
    };
    let world = 512.0 * 2f64.powf(request.zoom);
    let (cx, cy) = merc(request.center_lon, request.center_lat);
    let (x, y) = merc(lon, lat);
    ((x - cx) * world + f64::from(request.width) / 2.0, (y - cy) * world + f64::from(request.height) / 2.0)
}

fn rgb_at(shot: &Capture, x: f64, y: f64) -> [u8; 3] {
    let i = ((y.round() as usize) * shot.width as usize + x.round() as usize) * 4;
    [shot.rgba[i], shot.rgba[i + 1], shot.rgba[i + 2]]
}

fn close(a: [u8; 3], b: [u8; 3]) -> bool {
    a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 6)
}

#[test]
fn a_csv_of_world_cities_draws_as_data_driven_circles() {
    let request = request();
    let shot = capture(&request, &TileStore::new(), &Style { layers: Vec::new(), ..Style::default() }).expect("a headless device");
    if let Some(dir) = std::env::var_os("GPUI_MAP_CSV_SHOT") {
        shot.save_png(&std::path::Path::new(&dir).join("csv_world_cities.png")).expect("save");
    }
    let count = |want: [u8; 3]| shot.rgba.as_chunks::<4>().0.iter().filter(|p| close([p[0], p[1], p[2]], want)).count();
    let (capitals, others) = (count(CAPITAL), count(OTHER));
    assert!(capitals > 500 && others > 300, "capital px {capitals}, other px {others}");

    // Each city is at its own position, in its own colour: the CSV's lat/lon
    // order, the bool column and the colour expression all survived.
    for (name, lon, lat, want) in [
        ("Tokyo", 139.69, 35.69, CAPITAL),
        ("Mumbai", 72.88, 19.08, OTHER),
        ("London", -0.13, 51.51, CAPITAL),
        ("Sydney", 151.21, -33.87, OTHER),
        ("Lima", -77.04, -12.05, CAPITAL),
        ("Anchorage", -149.90, 61.22, OTHER),
    ] {
        let (x, y) = pixel_of(&request, lon, lat);
        let got = rgb_at(&shot, x, y);
        assert!(close(got, want), "{name} at ({x:.0}, {y:.0}): {got:?}, want {want:?}");
    }
    // The radius reads the number column: Tokyo (37.4 M) is wider than
    // Reykjavik (140 k), measured along the row through each centre.
    let width = |lon: f64, lat: f64, want: [u8; 3]| {
        let (x, y) = pixel_of(&request, lon, lat);
        (0..shot.width).filter(|&px| close(rgb_at(&shot, f64::from(px), y), want) && (f64::from(px) - x).abs() < 40.0).count()
    };
    let (tokyo, reykjavik) = (width(139.69, 35.69, CAPITAL), width(-21.94, 64.15, CAPITAL));
    assert!(tokyo > reykjavik + 6, "Tokyo {tokyo} px wide, Reykjavik {reykjavik} px");
}

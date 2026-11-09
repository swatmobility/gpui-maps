//! Headless GPU captures of the altitude layer (spec §5.3). Each asserts the run painted.
use gpui_maps::capture::{capture, AltitudeCapture, CaptureRequest, Clear, TileStore};
use gpui_maps::render::altitude::{closest_approach, eye_sphere, geo_to_sphere, project_altitude, visible_by_horizon_angle, GeoPoint};
use gpui_maps::render::altitude_pipeline::{occlusion_eps, AltitudeStyle};
use gpui_maps::render::globe::globe_frame;
use gpui_maps::style::{Rgba, Style};

const MAGENTA: [u8; 4] = [255, 0, 255, 255]; // GEO, near half
const CYAN: [u8; 4] = [0, 255, 255, 255]; // GEO, far half
const GREEN: [u8; 4] = [0, 255, 0, 255]; // LEO, near half
const YELLOW: [u8; 4] = [255, 255, 0, 255]; // LEO, far half

fn rgba(c: [u8; 4]) -> Rgba {
    Rgba { r: f32::from(c[0]) / 255.0, g: f32::from(c[1]) / 255.0, b: f32::from(c[2]) / 255.0, a: 1.0 }
}

/// Two shells, each split into a near and a far half by colour, so that the hidden far side can never be
/// confused with the visible near side that lands on the same pixels.
fn shells() -> (Vec<GeoPoint>, AltitudeStyle) {
    let mut points = Vec::new();
    let mut paint = Vec::new();
    for i in 0..72 {
        let lon = -180.0 + 5.0 * f64::from(i);
        points.push(GeoPoint { lon, lat: 0.0, alt_m: 35_786_000.0 });
        paint.push(if lon.abs() < 90.0 { 0 } else { 1 });
    }
    for i in 0..36 {
        let lon = -180.0 + 10.0 * f64::from(i);
        points.push(GeoPoint { lon, lat: 0.0, alt_m: 550_000.0 });
        paint.push(if lon.abs() < 90.0 { 2 } else { 3 });
    }
    let mut palette = [Rgba { r: 0.5, g: 0.5, b: 0.5, a: 1.0 }; 16];
    for (k, c) in [MAGENTA, CYAN, GREEN, YELLOW].into_iter().enumerate() {
        palette[k] = rgba(c);
    }
    (points, AltitudeStyle { palette, paint, base_px: [5.0; 16], outline: Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }, ..AltitudeStyle::default() })
}

fn request(zoom: f64, lat: f64) -> CaptureRequest {
    let (points, style) = shells();
    CaptureRequest {
        center_lon: 0.0,
        center_lat: lat,
        zoom,
        pitch: 0.0,
        bearing: 0.0,
        width: 1024,
        height: 1024,
        clear: Clear::Sentinel(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
        stars: true,
        atmosphere: Some(Default::default()),
        altitude: Some(AltitudeCapture { points, style, ..Default::default() }),
        ..Default::default()
    }
}

/// Whether any pixel within `r` of (x, y) matches `c` within tolerance 24.
fn has_colour_near(frame: &gpui_maps::capture::Capture, x: f32, y: f32, r: i32, c: [u8; 4]) -> bool {
    let (w, h) = (frame.width as i32, frame.height as i32);
    for dy in -r..=r {
        for dx in -r..=r {
            let (px, py) = (x as i32 + dx, y as i32 + dy);
            if px < 0 || py < 0 || px >= w || py >= h {
                continue;
            }
            let p = frame.pixel(px as u32, py as u32);
            if (0..3).all(|k| (i32::from(p[k]) - i32::from(c[k])).abs() <= 24) {
                return true;
            }
        }
    }
    false
}

#[test]
fn geo_is_drawn_at_altitude_and_the_planet_hides_the_far_side() {
    let req = request(-1.0, 5.0);
    let before = gpui_maps::render::allocation_stats().draw_calls_total;
    let frame = capture(&req, &TileStore::new(), &Style::default()).expect("a headless device");
    assert!(gpui_maps::render::allocation_stats().draw_calls_total > before, "nothing was drawn");
    assert_eq!(frame.altitude_points_drawn, 108);

    let camera = req.camera();
    let gf = globe_frame(&camera);
    let eye = eye_sphere(&gf);
    let eps = occlusion_eps(&camera, eye);
    let (points, style) = shells();
    let colours = [MAGENTA, CYAN, GREEN, YELLOW];
    // Every object's screen position and verdict, so a dot another dot covers can be told apart from one
    // that was never drawn. Near the limb the equatorial ring is seen edge-on and neighbours 10 degrees apart
    // land about a pixel apart: LEO at lon -90 (far colour, in sight) sits under lon -80 (near colour, drawn
    // later), and over lon -100 (far colour, hidden) -- the colour split cannot separate those.
    let placed: Vec<_> = points
        .iter()
        .map(|&p| {
            let s = geo_to_sphere(p);
            (project_altitude(&camera, &gf, s), visible_by_horizon_angle(eye, s))
        })
        .collect();
    // A dot is 5 px times the depth cue (at most 1.5), plus 1 px of antialiasing.
    let reach = 0.5 * 5.0 * 1.5 + 1.0 + 2.0;
    let near = |i: usize, j: usize| match (placed[i].0, placed[j].0) {
        (Some(a), Some(b)) => (a.0 - b.0).hypot(a.1 - b.1) < reach,
        _ => false,
    };
    let (mut seen, mut hidden, mut past_far_plane, mut contested) = (0, 0, 0, 0);
    for (i, (p, paint)) in points.iter().zip(&style.paint).enumerate() {
        let s = geo_to_sphere(*p);
        let Some((x, y)) = placed[i].0 else { continue };
        if x < 3.0 || y < 3.0 || x > 1020.0 || y > 1020.0 {
            continue;
        }
        // Within the soft edge of the limb a dot may draw at partial alpha: either verdict is right.
        let m = closest_approach(eye, s);
        if (m - 1.0).abs() < 2.0 * eps {
            continue;
        }
        let c = colours[usize::from(*paint)];
        // In sight but under a later-drawn dot of another colour; or hidden, beside a drawn dot of its own
        // colour. Neither pixel test can speak for this object.
        let covered = (0..points.len()).any(|j| {
            j != i
                && placed[j].1
                && near(i, j)
                && if placed[i].1 { j > i && style.paint[j] != *paint } else { style.paint[j] == *paint }
        });
        if covered {
            contested += 1;
            continue;
        }
        if visible_by_horizon_angle(eye, s) {
            assert!(has_colour_near(&frame, x, y, 2, c), "{p:?} is visible but not drawn at ({x:.1},{y:.1}); m - 1 = {:e}", m - 1.0);
            seen += 1;
            // The globe projection's far plane is d + 2R: a visible object past it would be clipped by a
            // projected z. The shader writes its own.
            let clip = gf.globe_view_proj * s.extend(1.0);
            if clip.z > clip.w {
                past_far_plane += 1;
            }
        } else {
            assert!(!has_colour_near(&frame, x, y, 1, c), "{p:?} is behind the planet but drawn at ({x:.1},{y:.1})");
            hidden += 1;
        }
    }
    assert!(seen >= 60 && hidden >= 10, "the scene must exercise both verdicts: seen {seen}, hidden {hidden}");
    eprintln!("seen {seen}, hidden {hidden}, contested {contested}, past the far plane {past_far_plane}");
    assert!(contested <= 12, "only the limb crowding may be set aside: {contested}");
    assert!(past_far_plane >= 4, "GEO past the far plane and in sight must be in the scene: {past_far_plane}");

    // GEO at altitude, not on the surface: a magenta dot sits at least 5.5 disc radii out.
    let disc_r = gpui_maps::render::altitude::px_per_radius(&camera, eye);
    let far_out = (0..1024u32)
        .flat_map(|y| (0..1024u32).map(move |x| (x, y)))
        .filter(|&(x, y)| {
            let p = frame.pixel(x, y);
            p[0] > 230 && p[1] < 25 && p[2] > 230
        })
        .map(|(x, y)| ((x as f64 - 512.0).powi(2) + (y as f64 - 512.0).powi(2)).sqrt())
        .fold(0.0, f64::max);
    assert!(far_out > 5.5 * disc_r, "furthest magenta pixel {far_out:.0} px, disc radius {disc_r:.0} px");
}

#[test]
fn the_flat_map_is_bit_identical_with_and_without_the_layer() {
    let mut with = request(13.0, 5.0);
    with.stars = false;
    with.atmosphere = None;
    let mut without = with.clone();
    without.altitude = None;
    let a = capture(&with, &TileStore::new(), &Style::default()).unwrap();
    let b = capture(&without, &TileStore::new(), &Style::default()).unwrap();
    assert_eq!(a.transition, 0.0);
    assert_eq!(a.altitude_points_drawn, 0);
    assert!(a.rgba == b.rgba, "R3: the flat frame changed");
}

/// Spec §5.3, CPU/GPU parity: the hit test's projection (`project_altitude`, which
/// `AltitudeFrame::project` equals; pinned in `altitude_pipeline`'s tests) lands on the drawn dot's
/// centroid within half a pixel, over 200 objects from LEO to GEO.
#[test]
fn the_cpu_mirror_matches_the_gpu_within_half_a_pixel() {
    const COLOURS: [[u8; 4]; 8] = [
        [255, 0, 255, 255],
        [0, 255, 255, 255],
        [255, 255, 0, 255],
        [0, 255, 0, 255],
        [255, 0, 0, 255],
        [0, 0, 255, 255],
        [255, 128, 0, 255],
        [128, 0, 255, 255],
    ];
    let mut seed = 7u64;
    let mut rnd = || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (seed >> 11) as f64 / (1u64 << 53) as f64
    };
    let base = CaptureRequest {
        center_lon: 0.0,
        center_lat: 20.0,
        zoom: -1.0,
        width: 1024,
        height: 1024,
        backdrop: false,
        clear: Clear::Sentinel(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
        ..Default::default()
    };
    let camera = base.camera();
    let gf = globe_frame(&camera);
    let eye = eye_sphere(&gf);
    let eps = 1.0 / gpui_maps::render::altitude::px_per_radius(&camera, eye);
    let mut compared = 0;
    let mut worst = 0.0f64;
    while compared < 200 {
        // Draw 8 candidates; keep a batch only if all 8 are clearly visible, on screen and >= 16 px apart.
        let pts: Vec<GeoPoint> = (0..8)
            .map(|_| GeoPoint { lon: 180.0 * rnd() - 90.0, lat: 120.0 * rnd() - 60.0, alt_m: (300.0 + 35_700.0 * rnd()) * 1e3 })
            .collect();
        let proj: Vec<_> = pts
            .iter()
            .map(|p| {
                let s = geo_to_sphere(*p);
                let ok = visible_by_horizon_angle(eye, s) && (closest_approach(eye, s) - 1.0).abs() > 3.0 * eps;
                project_altitude(&camera, &gf, s).filter(|&(x, y)| ok && x > 12.0 && y > 12.0 && x < 1012.0 && y < 1012.0)
            })
            .collect();
        if proj.iter().any(Option::is_none) {
            continue;
        }
        let proj: Vec<(f32, f32)> = proj.into_iter().flatten().collect();
        if proj.iter().enumerate().any(|(i, a)| proj[i + 1..].iter().any(|b| (a.0 - b.0).hypot(a.1 - b.1) < 16.0)) {
            continue;
        }
        let mut style = AltitudeStyle::default();
        for (k, c) in COLOURS.iter().enumerate() {
            style.palette[k] = rgba(*c);
        }
        style.paint = (0..8).collect();
        style.base_px = [6.0; 16];
        style.outline.a = 0.0;
        let req = CaptureRequest { altitude: Some(AltitudeCapture { points: pts, style, ..Default::default() }), ..base.clone() };
        let before = gpui_maps::render::allocation_stats().draw_calls_total;
        let f = capture(&req, &TileStore::new(), &Style::default()).unwrap();
        assert!(gpui_maps::render::allocation_stats().draw_calls_total > before);
        for (k, &(x, y)) in proj.iter().enumerate() {
            let c = COLOURS[k];
            let (mut sx, mut sy, mut n) = (0.0, 0.0, 0.0);
            for py in (y as i32 - 8)..=(y as i32 + 8) {
                for px in (x as i32 - 8)..=(x as i32 + 8) {
                    let p = f.pixel(px as u32, py as u32);
                    if (0..3).all(|j| (i32::from(p[j]) - i32::from(c[j])).abs() <= 40) {
                        sx += f64::from(px) + 0.5;
                        sy += f64::from(py) + 0.5;
                        n += 1.0;
                    }
                }
            }
            assert!(n >= 4.0, "object {k} at ({x:.1},{y:.1}) not found");
            let d = (sx / n - f64::from(x)).hypot(sy / n - f64::from(y));
            worst = worst.max(d);
            compared += 1;
        }
    }
    eprintln!("CPU/GPU parity: worst {worst:.3} px over {compared} objects");
    assert!(worst <= 0.5, "worst CPU/GPU disagreement {worst:.3} px over {compared} objects");
}

/// Satellites Task 10: the ISS's white halo is invisible on light ground (Positron land is #f8f4f0), so a halo
/// object carries a dark outline ring OUTSIDE the white one. On a white ground, walking out from the dot's
/// centre: the fill, the outline, the white halo, and then a dark ring again within a few pixels. Without the
/// outer ring the white halo runs straight into the white ground.
#[test]
fn a_halo_keeps_a_dark_edge_on_white_ground() {
    let white = Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };
    let style = AltitudeStyle { paint: vec![0], ..AltitudeStyle::default() };
    let request = CaptureRequest {
        center_lon: 0.0,
        center_lat: 0.0,
        zoom: 2.0,
        width: 256,
        height: 256,
        clear: Clear::Sentinel(white),
        backdrop: false,
        stars: false,
        atmosphere: None,
        altitude: Some(AltitudeCapture {
            points: vec![GeoPoint { lon: 0.0, lat: 0.0, alt_m: 420_000.0 }],
            style,
            emphasis: (None, None, Some(0)),
            ..Default::default()
        }),
        ..Default::default()
    };
    let frame = capture(&request, &TileStore::new(), &Style::default()).unwrap();
    assert_eq!(frame.altitude_points_drawn, 1, "the run painted");
    let (cx, cy) = (128u32, 128u32);
    let is_white = |p: [u8; 4]| p[..3].iter().all(|&c| c >= 245);
    let is_dark = |p: [u8; 4]| p[..3].iter().all(|&c| c <= 200);
    for (dx, dy) in [(1i32, 0i32), (-1, 0), (0, 1), (0, -1)] {
        let at = |k: i32| frame.pixel((cx as i32 + dx * k) as u32, (cy as i32 + dy * k) as u32);
        assert!(!is_white(at(0)), "the dot's fill is at the centre: {:?}", at(0));
        let halo = (1..20).find(|&k| is_white(at(k))).expect("a white halo around the dot");
        let edge = (halo..halo + 5).find(|&k| is_dark(at(k)));
        let row: Vec<[u8; 4]> = (0..halo + 6).map(at).collect();
        assert!(edge.is_some(), "no dark ring outside the white halo (from {halo} px) along ({dx}, {dy}): {row:?}");
        assert!(is_white(at(halo + 8)), "the ground past the ring is the white clear");
    }
}

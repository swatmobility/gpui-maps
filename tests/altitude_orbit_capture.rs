//! The ISS moves ~460 km in 60 s of scene time, through the real GPU Hermite (spec §6 Task 5).
use std::sync::Arc;

use gpui_maps::capture::{capture, AltitudeCapture, CaptureKeyframes, CaptureRequest, Clear, TileStore};
use gpui_maps::orbit::{catalog::*, omm::parse_omm, propagate::CatalogSource};
use gpui_maps::render::altitude::{hermite, project_altitude, teme_to_sphere, KeyState, KeyframeSource, RefFrame};
use gpui_maps::render::altitude_pipeline::AltitudeStyle;
use gpui_maps::render::globe::{globe_frame, GLOBE_RADIUS_M};
use gpui_maps::style::{Rgba, Style};

fn keyframes(src: &CatalogSource, t0: f64, d: f64, scene: f64) -> CaptureKeyframes {
    let n = src.len();
    let (mut k0, mut k1) = (vec![KeyState::HIDDEN; n], vec![KeyState::HIDDEN; n]);
    src.states_at(t0, 0..n, &mut k0);
    src.states_at(t0 + d, 0..n, &mut k1);
    CaptureKeyframes { frame: RefFrame::Inertial, t0_unix: t0, delta_s: d, k0, k1, scene_unix: scene, rate: 0.0 }
}

#[test]
fn the_iss_moves_about_460_km_in_a_minute() {
    let recs = parse_omm(include_bytes!("fixtures/orbit/stations.json")).unwrap().records;
    let only_iss: Vec<_> = recs.into_iter().filter(|r| r.norad_id == ISS_NORAD).collect();
    let src = CatalogSource(Arc::new(Catalog::build(only_iss, &Membership::default())));
    let t = src.0.objects[0].record.epoch_unix;
    let mut style = AltitudeStyle::default();
    style.palette[0] = Rgba { r: 1.0, g: 0.0, b: 1.0, a: 1.0 };
    style.paint = vec![0];
    style.base_px = [6.0; 16];
    style.outline.a = 0.0;
    // Camera over the ISS's sub-point at t+45, z3: ~10 km a pixel, so 460 km is ~46 px.
    let (a, b) = (keyframes(&src, t, 60.0, t + 15.0), keyframes(&src, t + 60.0, 60.0, t + 75.0));
    let sphere = |k: &CaptureKeyframes| {
        let s = (k.scene_unix - k.t0_unix) / k.delta_s;
        let p = hermite(k.k0[0].position_m, k.k0[0].velocity_m_s, k.k1[0].position_m, k.k1[0].velocity_m_s, k.delta_s, s);
        teme_to_sphere(k.scene_unix) * (p / GLOBE_RADIUS_M)
    };
    let mid = (sphere(&a) + sphere(&b)).normalize();
    let (lon, lat) = (mid.x.atan2(mid.z).to_degrees(), mid.y.asin().to_degrees());
    let base = CaptureRequest {
        center_lon: lon,
        center_lat: lat,
        zoom: 3.0,
        width: 512,
        height: 512,
        clear: Clear::Sentinel(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
        ..Default::default()
    };
    let out = std::env::var_os("ALTITUDE_CAPTURE_OUT").map(std::path::PathBuf::from);
    let centroid = |k: CaptureKeyframes, name: &str| {
        let req = CaptureRequest {
            altitude: Some(AltitudeCapture { style: style.clone(), keyframes: Some(k), ..Default::default() }),
            ..base.clone()
        };
        let before = gpui_maps::render::allocation_stats().draw_calls_total;
        let f = capture(&req, &TileStore::new(), &Style::default()).expect("device");
        assert!(gpui_maps::render::allocation_stats().draw_calls_total > before);
        if let Some(dir) = out.as_ref() {
            f.save_png(&dir.join(name)).expect("png");
        }
        let (mut sx, mut sy, mut n) = (0.0, 0.0, 0.0);
        for y in 0..512u32 {
            for x in 0..512u32 {
                let p = f.pixel(x, y);
                if p[0] > 200 && p[1] < 60 && p[2] > 200 {
                    sx += f64::from(x) + 0.5;
                    sy += f64::from(y) + 0.5;
                    n += 1.0;
                }
            }
        }
        assert!(n > 4.0, "the ISS is not on screen");
        (sx / n, sy / n)
    };
    let (pa, pb) = (sphere(&a), sphere(&b));
    let km = (pb - pa).length() * GLOBE_RADIUS_M / 1000.0;
    assert!((430.0..490.0).contains(&km), "the ISS moved {km:.0} km in 60 s");
    let cam = base.camera();
    let gf = globe_frame(&cam);
    let (ea, eb) = (project_altitude(&cam, &gf, pa).unwrap(), project_altitude(&cam, &gf, pb).unwrap());
    let (ga, gb) = (centroid(a, "iss-t15.png"), centroid(b, "iss-t75.png"));
    let want = (f64::from(eb.0 - ea.0), f64::from(eb.1 - ea.1));
    let got = (gb.0 - ga.0, gb.1 - ga.1);
    if out.is_some() {
        eprintln!("ISS: {km:.1} km; GPU moved {got:?} px, CPU expects {want:?}; at {ga:?} then {gb:?}");
    }
    assert!(want.0.hypot(want.1) > 30.0, "the test must see real motion");
    assert!((got.0 - want.0).hypot(got.1 - want.1) < 1.0, "GPU moved {got:?}, CPU expects {want:?}");
}

#[test]
fn the_iss_ring_passes_behind_the_planet() {
    use gpui_maps::orbit::propagate::orbit_ring_teme;
    use gpui_maps::render::altitude::{closest_approach, eye_sphere, visible_by_horizon_angle, AltitudeLine};
    let recs = parse_omm(include_bytes!("fixtures/orbit/stations.json")).unwrap().records;
    let c = Catalog::build(recs, &Membership::default());
    let iss = &c.objects[c.index_of(ISS_NORAD).unwrap()];
    let t = iss.record.epoch_unix;
    let ring = orbit_ring_teme(iss, t, 256);
    let magenta = Rgba { r: 1.0, g: 0.0, b: 1.0, a: 1.0 };
    let req = CaptureRequest {
        center_lon: 0.0,
        center_lat: 0.0,
        zoom: 0.0,
        width: 1024,
        height: 1024,
        backdrop: false,
        clear: Clear::Sentinel(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
        altitude: Some(AltitudeCapture {
            lines: vec![AltitudeLine { points: ring.clone(), color: magenta, closed: true }],
            lines_frame: RefFrame::Inertial,
            lines_width_px: 2.0,
            lines_scene_unix: t,
            ..Default::default()
        }),
        ..Default::default()
    };
    let before = gpui_maps::render::allocation_stats().draw_calls_total;
    let f = capture(&req, &TileStore::new(), &Style::default()).unwrap();
    assert!(gpui_maps::render::allocation_stats().draw_calls_total > before);
    if let Some(dir) = std::env::var_os("ALTITUDE_CAPTURE_OUT").map(std::path::PathBuf::from) {
        f.save_png(&dir.join("iss-ring-test.png")).expect("png");
    }
    let cam = req.camera();
    let gf = globe_frame(&cam);
    let eye = eye_sphere(&gf);
    let eps = 1.0 / gpui_maps::render::altitude::px_per_radius(&cam, eye);
    let m = teme_to_sphere(t);
    let is_magenta = |x: i32, y: i32, r: i32| {
        (-r..=r).any(|dy| {
            (-r..=r).any(|dx| {
                let (px, py) = (x + dx, y + dy);
                px >= 0 && py >= 0 && px < 1024 && py < 1024 && {
                    let p = f.pixel(px as u32, py as u32);
                    p[0] > 200 && p[1] < 60 && p[2] > 200
                }
            })
        })
    };
    // Every sample clear of the soft limb, projected, split by the independent horizon predicate.
    let (mut visible, mut hidden) = (Vec::new(), Vec::new());
    for p in &ring {
        let s = m * (*p / GLOBE_RADIUS_M);
        if (closest_approach(eye, s) - 1.0).abs() < 3.0 * eps {
            continue;
        }
        let Some((x, y)) = project_altitude(&cam, &gf, s) else { continue };
        if visible_by_horizon_angle(eye, s) { visible.push((x, y)) } else { hidden.push((x, y)) }
    }
    for &(x, y) in &visible {
        assert!(is_magenta(x as i32, y as i32, 2), "visible ring sample not drawn at ({x:.0},{y:.0})");
    }
    // A ring seen near edge-on can put a hidden (far-side) sample on top of
    // the visible near-side part of the same ring, where magenta is right.
    // So a hidden sample within 3 px of any projected VISIBLE sample is not
    // asserted on; every other hidden sample must be dark.
    let near_visible = |x: f32, y: f32| visible.iter().any(|&(vx, vy): &(f32, f32)| (vx - x).hypot(vy - y) < 3.0);
    let mut checked_hidden = 0;
    for &(x, y) in &hidden {
        if near_visible(x, y) {
            continue;
        }
        assert!(!is_magenta(x as i32, y as i32, 1), "hidden ring sample drawn at ({x:.0},{y:.0})");
        checked_hidden += 1;
    }
    let (seen, hid) = (visible.len(), hidden.len());
    assert!(seen > 50 && hid > 50, "seen {seen}, hidden {hid}: the ring must cross the limb");
    assert!(checked_hidden > 50, "only {checked_hidden} hidden samples were clear of the near side");
}

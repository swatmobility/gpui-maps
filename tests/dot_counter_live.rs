//! The dot counter against the real GHSL 30″ archive, at the view of a
//! past defect (Nizhny Novgorod: "1,350,908 dots drawn · 1 dot =
//! 7 people" for a city of about 1.3 M). Ignored: it needs the archive
//! (`~/.cache/gpui-map-data/ghsl/E2025_30ss.pmtiles`, see `demos/README.md`).
//!
//! ```text
//! cargo test --release --test dot_counter_live -- --ignored --nocapture
//! ```

use std::sync::Arc;
use std::time::Instant;

use gpui_maps::overlay::dotgrid::in_view::ViewRegion;
use gpui_maps::overlay::dotgrid::layer::{DotLayers, DotStyle};
use gpui_maps::overlay::dotgrid::{fetch_native, GridCountSource};
use gpui_maps::render::globe::{globe_frame, GlobeCover};
use gpui_maps::render::Camera;

/// ClickHouse's `gpui_map_dots.ghs_cells_30ss`, epoch 2025, level 0: the
/// people in cells centred in 56.20-56.40 N x 43.75-44.10 E (the
/// controller's query, cell centre `90 - (row + 0.5) / 120`,
/// `-180 + (col + 0.5) / 120`).
const CITY_BOX_PEOPLE: f64 = 1_289_376.0;

fn archive() -> Arc<dyn GridCountSource> {
    let home = std::env::var("HOME").unwrap_or_default();
    let location = format!("{home}/.cache/gpui-map-data/ghsl/E2025_30ss.pmtiles");
    Arc::new(fetch_native::open_pmtiles_grid(&location).expect("the 30″ archive (see demos/README.md)"))
}

/// Unit-world Mercator of a lon/lat.
fn unit(lon: f64, lat: f64) -> [f64; 2] {
    let y = (std::f64::consts::FRAC_PI_4 + lat.to_radians() / 2.0).tan().ln();
    [(lon + 180.0) / 360.0, 0.5 - y / (2.0 * std::f64::consts::PI)]
}

fn the_users_camera() -> Camera {
    Camera {
        center_lon: 43.95,
        center_lat: 56.30,
        zoom: 9.5,
        pitch: 0.0,
        bearing: 0.0,
        viewport_width_px: 2000.0,
        viewport_height_px: 1260.0,
        fov_y: Camera::DEFAULT_FOV_Y,
    }
}

/// Runs frames over `view` until every wanted tile has landed.
fn settle(layers: &mut DotLayers, camera: &Camera, view: &ViewRegion) {
    for _ in 0..200 {
        let busy = layers.frame_with_view(
            camera.zoom,
            |z| gpui_maps::render::cover_plan::flat_cover(camera, z).tiles,
            view,
            gpui_maps::timing::Instant::now(),
        );
        let mut ran = false;
        while let Some(job) = layers.next_job() {
            layers.land(job.run(fetch_native::execute));
            ran = true;
        }
        if !busy && !ran {
            return;
        }
    }
    panic!("the layer never settled");
}

#[test]
#[ignore = "needs the GHSL 30″ archive"]
fn the_counter_reads_the_citys_people_at_the_users_view() {
    let camera = the_users_camera();
    let mut layers = DotLayers::new();
    layers.set_source("pop", archive(), DotStyle::default());

    // The city box as the view: within 1 % of ClickHouse's level-0 sum
    // (the archive stores deci-counts, rounded; MG-4 bounds that at far
    // under 1 %).
    let (nw, se) = (unit(43.75, 56.40), unit(44.10, 56.20));
    let city = ViewRegion::mercator_box(nw, se);
    settle(&mut layers, &camera, &city);
    let stats = layers.stats("pop").expect("the layer");
    assert_eq!(stats.level, 0, "zoom 9.5 draws the base level");
    assert_eq!(stats.standin_tiles, 0, "settled: no stand-ins");
    let err = (stats.people_in_view - CITY_BOX_PEOPLE) / CITY_BOX_PEOPLE;
    println!(
        "city box: {:.0} people in view (ClickHouse {CITY_BOX_PEOPLE}, {:+.3} %), {} dots at 1 dot = {:.1}",
        stats.people_in_view,
        err * 100.0,
        stats.dots_in_view,
        stats.ppd,
    );
    assert!(err.abs() < 0.01, "{:.0} people in the city box, ClickHouse says {CITY_BOX_PEOPLE}", stats.people_in_view);
    assert_eq!(stats.dots_in_view, (stats.people_in_view / f64::from(stats.ppd)).round() as u64);

    // The whole window, flat and globe: what the counter reads there, and
    // what counting it costs when the view moves (no cache hit).
    let india = Camera { center_lon: 78.9, center_lat: 22.0, zoom: 5.0, viewport_width_px: 1280.0, viewport_height_px: 800.0, ..camera };
    for (name, camera, globe) in [
        ("flat", camera, GlobeCover::FLAT),
        ("globe", camera, GlobeCover::new(&globe_frame(&camera))),
        ("India z5 globe", india, GlobeCover::new(&globe_frame(&india))),
    ] {
        let view = ViewRegion::for_frame(&camera, &globe);
        settle(&mut layers, &camera, &view);
        let stats = layers.stats("pop").expect("the layer");
        println!(
            "window ({name}): {:.0} people in view · {} dots · 1 dot = {:.1} ({} tiles)",
            stats.people_in_view, stats.dots_in_view, stats.ppd, stats.tiles_ready
        );
        if camera == the_users_camera() {
            assert!(stats.people_in_view > CITY_BOX_PEOPLE && stats.people_in_view < 3.0e6, "{name}: {}", stats.people_in_view);
        }
        // The same frames twice: the view moving (every count a miss),
        // then held (every count a cache hit) -- the difference is what
        // counting costs a moving frame.
        let mut p50 = [0.0; 2];
        for (pass, moving) in [true, false].into_iter().enumerate() {
            let mut ms = Vec::new();
            for i in 0..40 {
                let mut moved = camera;
                moved.center_lon += f64::from(i % 2) * 1e-4 * 2f64.powf(9.5 - camera.zoom);
                let globe_moved = if globe.is_globe() { GlobeCover::new(&globe_frame(&moved)) } else { GlobeCover::FLAT };
                let at = if moving { moved } else { camera };
                let view = ViewRegion::for_frame(&at, if moving { &globe_moved } else { &globe });
                let t = Instant::now();
                layers.frame_with_view(
                    camera.zoom,
                    |z| gpui_maps::render::cover_plan::flat_cover(&camera, z).tiles,
                    &view,
                    gpui_maps::timing::Instant::now(),
                );
                ms.push(t.elapsed().as_secs_f64() * 1e3);
                while let Some(job) = layers.next_job() {
                    layers.land(job.run(fetch_native::execute));
                }
            }
            ms.sort_by(f64::total_cmp);
            p50[pass] = ms[ms.len() / 2];
        }
        println!(
            "window ({name}): frame_with_view p50 {:.3} ms with the view moving, {:.3} ms held: counting costs {:.3} ms a moving frame",
            p50[0],
            p50[1],
            p50[0] - p50[1]
        );
    }
}

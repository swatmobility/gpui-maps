//! The vector-tile overlay (`overlay::vector_tiles`, `MapView::add_vector_overlay`)
//! headless: the perf gate's `vtoverlay.*` section (`--measure`).
//!
//! Seeded synthetic tiles shaped like the two Kepler samples it serves, so
//! no network and no data: four z10 tiles of 3,000 census-block squares each
//! (a `fill` layer with Kepler's stepped `total_pop` colours, the
//! population sample's view) and twelve z14 tiles of 6,000 places each (a
//! `circle` layer coloured by postcode, the FSQ sample's view). Measured:
//! MVT decode + clip + paint-case match (`tile_features`), the push into an
//! overlay store as `MapView`'s push builds it, and a frame of each view at
//! the sample's camera through the capture renderer.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use gpui_maps::geo::TileId;
use gpui_maps::mvt::TagValue;
use gpui_maps::overlay::vector_tiles::{self, VectorGeometry, VectorLayerKind};
use serde_json::json;

fn gate(name: &str, value: f64, unit: &str) {
    println!("gate: vtoverlay.{name} {value} {unit}");
}

fn round(v: f64, places: i32) -> f64 {
    let k = 10f64.powi(places);
    (v * k).round() / k
}

/// A tiny deterministic PRNG (xorshift), so every run seeds the same tiles.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

type Feature = (geo_types::Geometry<i32>, Vec<(String, TagValue)>);

fn block_tiles(rng: &mut Rng) -> Vec<(TileId, Vec<u8>)> {
    let mut out = Vec::new();
    for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
        let mut features: Vec<Feature> = Vec::new();
        for _ in 0..3000 {
            let (x, y) = (rng.below(4000) as i32, rng.below(4000) as i32);
            let s = 20 + rng.below(60) as i32;
            let ring = geo_types::LineString::from(vec![(x, y), (x + s, y), (x + s, y + s), (x, y + s), (x, y)]);
            let pop = rng.below(1000) as f64;
            let share = |r: &mut Rng| (r.below(100) as f64) / 100.0;
            features.push((
                geo_types::Geometry::Polygon(geo_types::Polygon::new(ring, Vec::new())),
                vec![
                    ("total_pop".into(), TagValue::Number(pop)),
                    ("AGE_LT_21".into(), TagValue::Number(share(rng))),
                    ("AGE_21_64".into(), TagValue::Number(share(rng))),
                    ("AGE_GT_65".into(), TagValue::Number(share(rng))),
                ],
            ));
        }
        out.push((TileId { z: 10, x: 163 + dx, y: 395 + dy }, vector_tiles::encode_tile(&[("state_06_s20", features)], 4096)));
    }
    out
}

fn place_tiles(rng: &mut Rng) -> Vec<(TileId, Vec<u8>)> {
    let mut out = Vec::new();
    for i in 0..12u32 {
        let mut features: Vec<Feature> = Vec::new();
        for k in 0..6000 {
            let p = geo_types::Point::new(rng.below(4096) as i32, rng.below(4096) as i32);
            features.push((
                geo_types::Geometry::Point(p),
                vec![
                    ("name".into(), TagValue::String(format!("place {i}-{k}"))),
                    ("postcode".into(), TagValue::String(format!("941{:02}", rng.below(40)))),
                    ("address".into(), TagValue::String(format!("{} Market St", rng.below(3000)))),
                ],
            ));
        }
        out.push((TileId { z: 14, x: 2618 + i % 4, y: 6329 + i / 4 }, vector_tiles::encode_tile(&[("places", features)], 4096)));
    }
    out
}

fn population_layers() -> serde_json::Value {
    json!([{"id": "pop", "type": "fill", "source-layer": "state_06_s20", "minzoom": 7, "paint": {
        "fill-color": ["step", ["get", "total_pop"], "#0198BD", 150, "#42C1BC", 300, "#9CE3B1", 450, "#F5B272", 600, "#EB7053", 750, "#D50255"],
        "fill-opacity": 0.8}}])
}

fn place_layers() -> serde_json::Value {
    let mut color = vec![json!("match"), json!(["get", "postcode"])];
    let palette = ["#4C0035", "#880030", "#B72F15", "#D6610A", "#EF9100", "#FFC300"];
    for i in 0..40 {
        color.push(json!(format!("941{i:02}")));
        color.push(json!(palette[i % 6]));
    }
    color.push(json!("#5A5A5A"));
    json!([{"id": "places", "type": "circle", "source-layer": "places", "paint": {"circle-radius": 1.5, "circle-color": color, "circle-opacity": 0.8}}])
}

/// One view: decode, push, frame. Returns the feature count.
fn view(name: &str, tiles: Vec<(TileId, Vec<u8>)>, layers: serde_json::Value, zoom: f64, center: (f64, f64)) {
    let compiled = vector_tiles::compile_layers(&layers).expect("layers compile");
    let started = Instant::now();
    let decoded: Vec<_> = tiles.iter().map(|(t, b)| vector_tiles::tile_features(&compiled, *t, b.clone(), zoom).expect("decodes")).collect();
    gate(&format!("{name}.decode_ms"), round(started.elapsed().as_secs_f64() * 1e3, 1), "ms");
    let features: usize = decoded.iter().map(|d| d.len()).sum();
    gate(&format!("{name}.features"), features as f64, "count");

    // The push, as `MapView::push_vector_overlay` builds it.
    let layer = &compiled[0];
    let started = Instant::now();
    let mut store = gpui_maps::overlay::OverlayStore::new();
    let mut geojson = Vec::with_capacity(features);
    match layer.kind {
        VectorLayerKind::Circle => {
            store.set_source_cluster_options(name, Some(gpui_maps::overlay::cluster::ClusterOptions::points_only()), zoom as u8).expect("declare");
            let mut rows: Vec<([[f64; 2]; 1], gpui_maps::overlay::FeaturePaint, Arc<gpui_maps::overlay::FeatureProperties>)> = Vec::new();
            for f in decoded.iter().flat_map(|d| &d.layers[0]) {
                let paint = layer.circle_paint(usize::from(f.case), zoom).expect("a case");
                if let VectorGeometry::Points(p) = &f.geometry {
                    for q in p {
                        rows.push((
                            [*q],
                            gpui_maps::overlay::FeaturePaint { circle_radius: paint.radius, circle_color: paint.color, ..Default::default() },
                            Arc::clone(&f.properties),
                        ));
                        geojson.push(json!({"type": "Feature", "geometry": {"type": "Point", "coordinates": q},
                            "properties": {"circle-radius": paint.radius, "circle-color": rgba(paint.color)}}));
                    }
                }
            }
            let inputs: Vec<gpui_maps::overlay::DataFeatureInput<'_>> = rows
                .iter()
                .map(|(at, paint, props)| gpui_maps::overlay::DataFeatureInput {
                    feature: gpui_maps::overlay::FeatureInput { id: None, geometry: gpui_maps::overlay::GeometryInput::Point(at), paint: paint.clone() },
                    properties: Arc::clone(props),
                })
                .collect();
            store.set_source_data_from(name, &inputs, zoom as u8).expect("push");
        }
        _ => {
            let mut held: Vec<(Vec<Vec<&[[f64; 2]]>>, gpui_maps::overlay::FeaturePaint, Arc<gpui_maps::overlay::FeatureProperties>)> = Vec::new();
            for f in decoded.iter().flat_map(|d| &d.layers[0]) {
                let color = layer.fill_paint(usize::from(f.case), zoom).expect("a case");
                if let VectorGeometry::Polygons(p) = &f.geometry {
                    held.push((
                        p.iter().map(|rings| rings.iter().map(Vec::as_slice).collect()).collect(),
                        gpui_maps::overlay::FeaturePaint { fill_color: color, ..Default::default() },
                        Arc::clone(&f.properties),
                    ));
                    geojson.push(json!({"type": "Feature", "geometry": {"type": "MultiPolygon", "coordinates": p},
                        "properties": {"fill-color": rgba(color)}}));
                }
            }
            let refs: Vec<Vec<&[&[[f64; 2]]]>> = held.iter().map(|(p, _, _)| p.iter().map(Vec::as_slice).collect()).collect();
            let inputs: Vec<gpui_maps::overlay::DataFeatureInput<'_>> = held
                .iter()
                .zip(&refs)
                .map(|((_, paint, props), polys)| gpui_maps::overlay::DataFeatureInput {
                    feature: gpui_maps::overlay::FeatureInput { id: None, geometry: gpui_maps::overlay::GeometryInput::Polygon(polys), paint: paint.clone() },
                    properties: Arc::clone(props),
                })
                .collect();
            store.set_source_data_from(name, &inputs, zoom as u8).expect("push");
        }
    }
    gate(&format!("{name}.push_ms"), round(started.elapsed().as_secs_f64() * 1e3, 1), "ms");

    // A frame at the sample's camera (no basemap: the overlay alone).
    let request = gpui_maps::capture::CaptureRequest {
        center_lon: center.0,
        center_lat: center.1,
        zoom,
        width: 1280,
        height: 800,
        flat: true,
        overlay: Some(json!({"type": "FeatureCollection", "features": geojson}).to_string()),
        overlay_points: layer.kind == VectorLayerKind::Circle,
        ..Default::default()
    };
    let style = gpui_maps::style::Style { layers: Vec::new(), ..gpui_maps::style_spec::parse_and_compile(br#"{"version":8,"sources":{},"layers":[]}"#).expect("empty style") };
    let mut session = gpui_maps::capture::CaptureSession::new_blocking(None).expect("a GPU");
    let tiles: gpui_maps::capture::TileStore = HashMap::new();
    let _ = session.render(&request, &tiles, &style).expect("first frame");
    let mut times = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        let _ = session.render(&request, &tiles, &style).expect("frame");
        times.push(t.elapsed().as_secs_f64() * 1e3);
    }
    times.sort_by(f64::total_cmp);
    gate(&format!("{name}.frame_ms"), round(times[times.len() / 2], 1), "ms");
}

fn rgba(c: gpui_maps::style::Rgba) -> String {
    format!("#{:02x}{:02x}{:02x}{:02x}", (c.r * 255.0).round() as u8, (c.g * 255.0).round() as u8, (c.b * 255.0).round() as u8, (c.a * 255.0).round() as u8)
}

fn measure() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    view("population", block_tiles(&mut rng), population_layers(), 12.35, (-122.4509, 37.7665));
    view("places", place_tiles(&mut rng), place_layers(), 14.1, (-122.42, 37.7933));
}

thread_local! {
    /// The point sources `push_all` has declared.
    static DECLARED: std::cell::RefCell<std::collections::HashSet<String>> = std::cell::RefCell::new(std::collections::HashSet::new());
}

/// Every style layer of `layers` over `tiles`, pushed into `store` as
/// `MapView::push_vector_overlay` builds it (circles with their stroke as
/// a second point source; fills and lines as data features). The number of
/// features pushed.
fn push_all(store: &mut gpui_maps::overlay::OverlayStore, name: &str, layers: &[vector_tiles::VectorLayer], tiles: &[&vector_tiles::TileFeatures], zoom: f64) -> usize {
    use gpui_maps::overlay::{DataFeatureInput, FeatureInput, FeaturePaint, GeometryInput};
    let mut pushed = 0;
    for (i, layer) in layers.iter().enumerate() {
        let features: Vec<&vector_tiles::VectorFeature> = if layer.visible_at(zoom) { tiles.iter().flat_map(|t| t.layers.get(i).into_iter().flatten()).collect() } else { Vec::new() };
        pushed += features.len();
        let source = format!("{name}/{}", layer.id);
        match layer.kind {
            VectorLayerKind::Circle => {
                let mut fill: Vec<([[f64; 2]; 1], FeaturePaint, Arc<gpui_maps::overlay::FeatureProperties>)> = Vec::new();
                let mut stroke = Vec::new();
                for f in &features {
                    let Some(paint) = layer.circle_paint(usize::from(f.case), zoom) else { continue };
                    let VectorGeometry::Points(points) = &f.geometry else { continue };
                    for p in points {
                        fill.push(([*p], FeaturePaint { circle_radius: paint.radius, circle_color: paint.color, ..Default::default() }, Arc::clone(&f.properties)));
                        if paint.stroke_width > 0.0 {
                            stroke.push(([*p], FeaturePaint { circle_radius: paint.radius + paint.stroke_width, circle_color: paint.stroke_color, ..Default::default() }, Arc::clone(&f.properties)));
                        }
                    }
                }
                for (src, rows) in [(format!("{source}/stroke"), &stroke), (source.clone(), &fill)] {
                    // Declared once per source, as `add_vector_overlay` does.
                    if DECLARED.with(|d| d.borrow_mut().insert(src.clone())) {
                        let _ = store.set_source_cluster_options(&src, Some(gpui_maps::overlay::cluster::ClusterOptions::points_only()), zoom as u8);
                    }
                    let inputs: Vec<DataFeatureInput<'_>> = rows
                        .iter()
                        .map(|(at, paint, props)| DataFeatureInput { feature: FeatureInput { id: None, geometry: GeometryInput::Point(at), paint: paint.clone() }, properties: Arc::clone(props) })
                        .collect();
                    store.set_source_data_from(&src, &inputs, zoom as u8).expect("push");
                }
            }
            VectorLayerKind::Fill | VectorLayerKind::Line => {
                let mut runs: Vec<Vec<&[[f64; 2]]>> = Vec::new();
                let mut polygons: Vec<Vec<Vec<&[[f64; 2]]>>> = Vec::new();
                let mut kept: Vec<(FeaturePaint, &vector_tiles::VectorFeature)> = Vec::new();
                for f in &features {
                    let case = usize::from(f.case);
                    match (&f.geometry, layer.kind) {
                        (VectorGeometry::Polygons(p), VectorLayerKind::Fill) => {
                            let Some(color) = layer.fill_paint(case, zoom) else { continue };
                            polygons.push(p.iter().map(|rings| rings.iter().map(Vec::as_slice).collect()).collect());
                            kept.push((FeaturePaint { fill_color: color, ..Default::default() }, f));
                        }
                        (VectorGeometry::Lines(l), VectorLayerKind::Line) => {
                            let Some(line) = layer.line_paint(case, zoom) else { continue };
                            runs.push(l.iter().map(Vec::as_slice).collect());
                            kept.push((FeaturePaint { line_color: line.color, line_width: line.width, line_dash: line.dash, ..Default::default() }, f));
                        }
                        _ => {}
                    }
                }
                let polygon_refs: Vec<Vec<&[&[[f64; 2]]]>> = polygons.iter().map(|p| p.iter().map(Vec::as_slice).collect()).collect();
                let inputs: Vec<DataFeatureInput<'_>> = kept
                    .iter()
                    .enumerate()
                    .map(|(k, (paint, f))| {
                        let geometry = if layer.kind == VectorLayerKind::Fill { GeometryInput::Polygon(&polygon_refs[k]) } else { GeometryInput::Line(&runs[k]) };
                        DataFeatureInput { feature: FeatureInput { id: None, geometry, paint: paint.clone() }, properties: Arc::clone(&f.properties) }
                    })
                    .collect();
                store.set_source_data_from(&source, &inputs, zoom as u8).expect("push");
            }
        }
    }
    pushed
}

/// The Overture Maps page's load, on real tiles saved to disk:
/// `--overture <dir> <overlays.json>` where `overlays.json` is
/// `[{"name", "theme", "layers": [...]}]` and `<dir>/<theme>/z_x_y.mvt`
/// holds the view's tiles. Per overlay: the decode (off the UI thread in
/// the app), and the pushes as the tiles land one by one (on the UI
/// thread: each landing re-pushes every landed tile).
fn overture(dir: &str, overlays: &str, zoom: f64) {
    let spec: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(overlays).expect("overlays.json")).expect("json");
    let mut total_push = 0.0;
    let mut worst_push: f64 = 0.0;
    for o in spec.as_array().expect("a list") {
        let name = o["name"].as_str().unwrap();
        let theme = o["theme"].as_str().unwrap();
        let compiled = vector_tiles::compile_layers(&o["layers"]).expect("layers compile");
        let mut files: Vec<_> = std::fs::read_dir(format!("{dir}/{theme}")).expect("theme dir").filter_map(Result::ok).map(|e| e.path()).collect();
        files.sort();
        let mut decoded = Vec::new();
        let (mut decode_ms, mut decode_max): (f64, f64) = (0.0, 0.0);
        for f in &files {
            let stem = f.file_stem().unwrap().to_string_lossy().to_string();
            let mut it = stem.split('_').map(|s| s.parse::<u32>().unwrap());
            let (z, x, y) = (it.next().unwrap() as u8, it.next().unwrap(), it.next().unwrap());
            let bytes = std::fs::read(f).unwrap();
            let t = Instant::now();
            decoded.push(vector_tiles::tile_features(&compiled, TileId { z, x, y }, bytes, zoom).expect("decodes"));
            let ms = t.elapsed().as_secs_f64() * 1e3;
            decode_ms += ms;
            decode_max = decode_max.max(ms);
        }
        let mut store = gpui_maps::overlay::OverlayStore::new();
        let (mut push_sum, mut push_max, mut features) = (0.0, 0.0f64, 0);
        for k in 1..=decoded.len() {
            let landed: Vec<&vector_tiles::TileFeatures> = decoded[..k].iter().collect();
            let t = Instant::now();
            features = push_all(&mut store, name, &compiled, &landed, zoom);
            let ms = t.elapsed().as_secs_f64() * 1e3;
            push_sum += ms;
            push_max = push_max.max(ms);
        }
        total_push += push_sum;
        worst_push = worst_push.max(push_max);
        println!(
            "overture {name:<24} tiles {:>2} features {features:>7} decode {decode_ms:>7.1} ms (max tile {decode_max:>6.1}) push max {push_max:>6.1} ms, sum over landings {push_sum:>7.1} ms",
            files.len()
        );
    }
    println!("overture UI-thread push: worst single {worst_push:.1} ms, all landings {total_push:.1} ms");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--measure") => measure(),
        Some("--overture") if args.len() >= 4 => overture(&args[2], &args[3], args.get(4).and_then(|z| z.parse().ok()).unwrap_or(15.6)),
        _ => eprintln!("usage: vtoverlay --measure | --overture <dir> <overlays.json> [zoom]"),
    }
}

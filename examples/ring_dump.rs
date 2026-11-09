//! **Dumps one decoded tile's polygon rings, and grades each one's winding
//! against exact arithmetic.**
//!
//! `mvt::parse_geometry` decides exterior-versus-interior by the sign of
//! `shoelace`, which is evaluated in an `f32` accumulator. MVT coordinates
//! reach a few thousand, so a single term `(y2 - y1) * (x2 + x1)` reaches
//! ~5e7 — already past the 1.6e7 where `f32` stops representing integers
//! exactly — and a ring of several hundred vertices accumulates hundreds of
//! them. This recomputes every ring's signed area in `i64`, where the answer
//! is exact, and reports where the two disagree.
//!
//! ```text
//! cargo run --release --example ring_dump -- <tile.pbf> [layer]
//! ```

use std::collections::BTreeMap;

/// `mvt::shoelace`, verbatim, including its `f32` accumulator.
fn shoelace_f32(points: &[geo_types::Coord<i32>]) -> f32 {
    let n = points.len();
    if n == 0 {
        return 0.0;
    }
    let mut area: f32 = 0.0;
    let mut v1 = points[n - 1];
    for v2 in points {
        area += (v2.y as f32 - v1.y as f32) * (v2.x as f32 + v1.x as f32);
        v1 = *v2;
    }
    area * 0.5
}

/// The same sum in `i64`, where every term and the total are exact: MVT
/// coordinates are `i32`, so a term fits in `i64` with room to spare and a
/// ring of any realistic length cannot overflow. Returned doubled, so the
/// value stays an integer.
fn shoelace_exact_x2(points: &[geo_types::Coord<i32>]) -> i64 {
    let n = points.len();
    if n == 0 {
        return 0;
    }
    let mut area: i64 = 0;
    let mut v1 = points[n - 1];
    for v2 in points {
        area += (i64::from(v2.y) - i64::from(v1.y)) * (i64::from(v2.x) + i64::from(v1.x));
        v1 = *v2;
    }
    area
}

fn bbox(points: &[geo_types::Coord<i32>]) -> (i32, i32, i32, i32) {
    let mut b = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
    for p in points {
        b.0 = b.0.min(p.x);
        b.1 = b.1.min(p.y);
        b.2 = b.2.max(p.x);
        b.3 = b.3.max(p.y);
    }
    b
}

fn report(label: &str, ring: &geo_types::LineString<i32>, mismatches: &mut Vec<String>) {
    let f = shoelace_f32(&ring.0);
    let e = shoelace_exact_x2(&ring.0);
    let exact = e as f64 / 2.0;
    let sign_f = if f > 0.0 { 1 } else if f < 0.0 { -1 } else { 0 };
    let sign_e = if e > 0 { 1 } else if e < 0 { -1 } else { 0 };
    let (x0, y0, x1, y1) = bbox(&ring.0);
    let flag = if sign_f != sign_e { "  <<< SIGN MISMATCH" } else { "" };
    let rel = if exact != 0.0 { (f64::from(f) - exact).abs() / exact.abs() } else { f64::INFINITY };
    println!(
        "  {label:<28} pts {:>5}  f32 {:>16.1}  exact {:>16.1}  rel.err {:>9.3e}  bbox [{x0},{y0}..{x1},{y1}]{flag}",
        ring.0.len(),
        f,
        exact,
        rel
    );
    if sign_f != sign_e {
        mismatches.push(format!("{label} (pts {}, f32 {f:.1}, exact {exact:.1})", ring.0.len()));
    }
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).ok_or_else(|| anyhow::anyhow!("usage: ring_dump <tile.pbf> [layer]"))?;
    let only_layer = args.get(2).cloned();

    let bytes = std::fs::read(path)?;
    let tile = gpui_maps::mvt::decode(bytes)?;
    println!("# {path}");

    let mut mismatches = Vec::new();
    // Rings whose EXTERIOR has the sign an interior should have: a hole
    // promoted to a filled shape, which is the reported defect's shape.
    let mut inverted_exteriors = 0usize;
    let mut per_layer: BTreeMap<String, (usize, usize)> = BTreeMap::new();

    for layer in &tile.layers {
        if only_layer.as_ref().is_some_and(|l| *l != layer.name) {
            continue;
        }
        let mut polys = 0usize;
        let mut rings = 0usize;
        let mut printed_header = false;
        for (fi, feature) in layer.features.iter().enumerate() {
            let list: Vec<&geo_types::Polygon<i32>> = match &feature.geometry {
                geo_types::Geometry::Polygon(p) => vec![p],
                geo_types::Geometry::MultiPolygon(m) => m.0.iter().collect(),
                _ => continue,
            };
            for (pi, p) in list.iter().enumerate() {
                polys += 1;
                rings += 1 + p.interiors().len();
                let ext_exact = shoelace_exact_x2(&p.exterior().0);
                let suspicious = ext_exact < 0
                    || shoelace_f32(&p.exterior().0).signum() as i64 != ext_exact.signum();
                if ext_exact < 0 {
                    inverted_exteriors += 1;
                }
                if suspicious {
                    if !printed_header {
                        println!("\n## layer `{}`", layer.name);
                        printed_header = true;
                    }
                    println!(
                        " feature {fi} polygon {pi}  ({} interiors)  tags {:?}",
                        p.interiors().len(),
                        feature.tags.get("class").or_else(|| feature.tags.get("subclass"))
                    );
                    report("exterior", p.exterior(), &mut mismatches);
                    for (ii, i) in p.interiors().iter().enumerate() {
                        report(&format!("interior {ii}"), i, &mut mismatches);
                    }
                }
            }
        }
        per_layer.insert(layer.name.clone(), (polys, rings));
    }

    // The biggest polygons per layer, with a CIRCULARITY score: the ratio of
    // the minimum to the maximum vertex distance from the ring's centroid.
    // A perfect circle scores 1.000. This is here because the reported
    // artefact is "a perfect pale-green circle on open ocean", and a circle
    // that is already a circle IN THE SOURCE BYTES is upstream data, not a
    // renderer defect -- marine protected areas are routinely mapped as a
    // radius around a point.
    println!("\n# largest polygons per layer (circularity 1.000 == a perfect circle)");
    for layer in &tile.layers {
        if only_layer.as_ref().is_some_and(|l| *l != layer.name) {
            continue;
        }
        let mut rows: Vec<(i64, String)> = Vec::new();
        for (fi, feature) in layer.features.iter().enumerate() {
            let list: Vec<&geo_types::Polygon<i32>> = match &feature.geometry {
                geo_types::Geometry::Polygon(p) => vec![p],
                geo_types::Geometry::MultiPolygon(m) => m.0.iter().collect(),
                _ => continue,
            };
            for (pi, p) in list.iter().enumerate() {
                let r = &p.exterior().0;
                if r.len() < 4 {
                    continue;
                }
                let area = shoelace_exact_x2(r).abs() / 2;
                let n = r.len() as f64;
                let cx = r.iter().map(|c| f64::from(c.x)).sum::<f64>() / n;
                let cy = r.iter().map(|c| f64::from(c.y)).sum::<f64>() / n;
                let (mut lo, mut hi) = (f64::MAX, 0.0f64);
                for c in r {
                    let d = ((f64::from(c.x) - cx).powi(2) + (f64::from(c.y) - cy).powi(2)).sqrt();
                    lo = lo.min(d);
                    hi = hi.max(d);
                }
                let circ = if hi > 0.0 { lo / hi } else { 0.0 };
                let (x0, y0, x1, y1) = bbox(r);
                let cls = feature
                    .tags
                    .get("class")
                    .or_else(|| feature.tags.get("subclass"))
                    .map(|v| format!("{v:?}"))
                    .unwrap_or_default();
                rows.push((
                    area,
                    format!(
                        "  f{fi}/p{pi} area {area:>12}  pts {:>5}  circularity {circ:.3}  bbox [{x0},{y0}..{x1},{y1}]  {cls}",
                        r.len()
                    ),
                ));
            }
        }
        rows.sort_by_key(|(a, _)| std::cmp::Reverse(*a));
        if !rows.is_empty() {
            println!("## layer `{}`", layer.name);
            for (_, line) in rows.iter().take(6) {
                println!("{line}");
            }
        }
    }

    println!("\n# summary");
    for (name, (polys, rings)) in &per_layer {
        if *polys > 0 {
            println!("  layer {name:<22} polygons {polys:>5}  rings {rings:>6}");
        }
    }
    println!("  EXTERIOR rings with negative exact area (a hole drawn as a shape): {inverted_exteriors}");
    println!("  rings where the f32 shoelace SIGN disagrees with exact: {}", mismatches.len());
    for m in &mismatches {
        println!("    {m}");
    }
    Ok(())
}

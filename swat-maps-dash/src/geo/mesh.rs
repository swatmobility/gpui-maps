//! Japanese standard grid squares (JIS X 0410). Level 1 is the 80 km square
//! (4 digits), level 2 the 10 km square (6), level 3 the 1 km square (8),
//! level 4 the 500 m half square (9) and level 5 the 250 m quarter square
//! (10). Levels 4 and 5 number their quarters 1 = south-west,
//! 2 = south-east, 3 = north-west, 4 = north-east.

/// The mesh code of the square of `level` (1–5) holding `(lat, lon)`.
pub fn code_at(lat: f64, lon: f64, level: u8) -> String {
    let y = lat * 1.5;
    let x = lon - 100.0;
    let p = y.floor();
    let u = x.floor();
    let mut code = format!("{:02}{:02}", p as i64, u as i64);
    if level < 2 {
        return code;
    }
    let (fy, fx) = ((y - p) * 8.0, (x - u) * 8.0);
    let (q, v) = (fy.floor(), fx.floor());
    code.push_str(&format!("{}{}", q as i64, v as i64));
    if level < 3 {
        return code;
    }
    let (gy, gx) = ((fy - q) * 10.0, (fx - v) * 10.0);
    let (r, w) = (gy.floor(), gx.floor());
    code.push_str(&format!("{}{}", r as i64, w as i64));
    let (mut hy, mut hx) = (gy - r, gx - w);
    for _ in 3..level.min(5) {
        let (ny, nx) = (hy >= 0.5, hx >= 0.5);
        code.push(char::from(b'1' + (ny as u8) * 2 + nx as u8));
        hy = if ny { hy * 2.0 - 1.0 } else { hy * 2.0 };
        hx = if nx { hx * 2.0 - 1.0 } else { hx * 2.0 };
    }
    code
}

/// `[west, south, east, north]` of a mesh code, or `None` when the text is
/// not a mesh code of levels 1–5.
pub fn bounds(code: &str) -> Option<[f64; 4]> {
    let d: Vec<u32> = code.chars().map(|c| c.to_digit(10)).collect::<Option<_>>()?;
    if !matches!(d.len(), 4 | 6 | 8 | 9 | 10) {
        return None;
    }
    let mut south = (d[0] * 10 + d[1]) as f64 / 1.5;
    let mut west = (d[2] * 10 + d[3]) as f64 + 100.0;
    let (mut h, mut w) = (1.0 / 1.5, 1.0);
    if d.len() >= 6 {
        if d[4] > 7 || d[5] > 7 {
            return None;
        }
        h /= 8.0;
        w /= 8.0;
        south += d[4] as f64 * h;
        west += d[5] as f64 * w;
    }
    if d.len() >= 8 {
        h /= 10.0;
        w /= 10.0;
        south += d[6] as f64 * h;
        west += d[7] as f64 * w;
    }
    for &q in d.iter().skip(8) {
        if !(1..=4).contains(&q) {
            return None;
        }
        h /= 2.0;
        w /= 2.0;
        if q >= 3 {
            south += h;
        }
        if q % 2 == 0 {
            west += w;
        }
    }
    Some([west, south, west + w, south + h])
}

/// The square's centre as `(lat, lon)`.
pub fn centroid(code: &str) -> Option<(f64, f64)> {
    let [w, s, e, n] = bounds(code)?;
    Some(((s + n) / 2.0, (w + e) / 2.0))
}

/// The square as a closed WKT polygon in lon/lat order.
pub fn wkt_polygon(code: &str) -> Option<String> {
    let [w, s, e, n] = bounds(code)?;
    Some(format!("POLYGON(({w} {s}, {e} {s}, {e} {n}, {w} {n}, {w} {s}))"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_point_round_trips_through_its_code_at_every_level() {
        let (lat, lon) = (35.6812, 139.7671);
        for level in 1..=5 {
            let code = code_at(lat, lon, level);
            let [w, s, e, n] = bounds(&code).expect("a mesh code");
            assert!(w <= lon && lon < e && s <= lat && lat < n, "level {level}: {code}");
        }
        assert_eq!(code_at(lat, lon, 3).len(), 8);
        assert_eq!(code_at(lat, lon, 4).len(), 9);
        assert!(code_at(lat, lon, 1).starts_with("5339"));
    }

    #[test]
    fn a_500m_square_is_a_quarter_of_its_1km_square() {
        let b3 = bounds("53394611").unwrap();
        let b4 = bounds("533946114").unwrap();
        assert!(((b3[2] - b3[0]) / 2.0 - (b4[2] - b4[0])).abs() < 1e-12);
        assert!((b4[2] - b3[2]).abs() < 1e-9, "quarter 4 is the north-east");
        assert!((b4[3] - b3[3]).abs() < 1e-9);
        assert!(bounds("53x3").is_none() && bounds("5339461").is_none() && bounds("533946115").is_none());
    }
}

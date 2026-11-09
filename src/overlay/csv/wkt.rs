//! Well-Known Text geometry, as a CSV cell carries it: `POINT`,
//! `LINESTRING`, `POLYGON` and their `MULTI*` forms, case-insensitive, with
//! an optional EWKT `SRID=...;` prefix and `Z`/`M`/`ZM` coordinates (the
//! extra ordinates are read and dropped -- this renderer draws in 2D).
//!
//! The result is the store's own [`OverlayGeometry`], so a WKT row and a
//! GeoJSON feature are the same thing from the push onwards. `EMPTY` and
//! `GEOMETRYCOLLECTION` are refused: the first has nothing to draw, the
//! second has no single-geometry shape in [`OverlayGeometry`].

use super::super::geojson::OverlayGeometry;

/// Why a WKT cell was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WktError {
    /// Not WKT at all, or truncated / malformed.
    Malformed,
    /// `EMPTY`, or a part with no positions.
    Empty,
    /// A geometry type with no overlay shape (`GEOMETRYCOLLECTION`,
    /// `CIRCULARSTRING`, ...).
    Unsupported,
}

/// Parses one WKT (or EWKT) geometry.
pub fn parse(text: &str) -> Result<OverlayGeometry, WktError> {
    let mut p = Parser { s: text.as_bytes(), at: 0 };
    p.space();
    // EWKT: `SRID=4326;POINT(...)`. Any SRID is accepted; the coordinates
    // are range-checked by the caller, which is where a projected CRS
    // (metres, not degrees) is caught.
    if p.s.len() >= p.at + 5 && p.s[p.at..p.at + 5].eq_ignore_ascii_case(b"SRID=") {
        let semi = p.s[p.at..].iter().position(|&b| b == b';').ok_or(WktError::Malformed)?;
        p.at += semi + 1;
        p.space();
    }
    let kind = p.word();
    let kind = kind.to_ascii_uppercase();
    p.space();
    // Dimension marker: `POINT Z (...)`, `POINT ZM (...)`, `POINTM(...)`.
    let (kind, mut dims) = split_dims(&kind);
    let marker = p.word().to_ascii_uppercase();
    match marker.as_str() {
        "" => {}
        "Z" | "M" => dims = 3,
        "ZM" => dims = 4,
        "EMPTY" => return Err(WktError::Empty),
        _ => return Err(WktError::Malformed),
    }
    p.space();
    if p.word().eq_ignore_ascii_case("EMPTY") {
        return Err(WktError::Empty);
    }
    let geometry = match kind {
        "POINT" => OverlayGeometry::Point(vec![p.wrapped(|p| p.position(dims))?]),
        "MULTIPOINT" => OverlayGeometry::Point(p.list(|p| {
            // Both `MULTIPOINT((1 2),(3 4))` and `MULTIPOINT(1 2,3 4)`.
            p.space();
            if p.peek() == Some(b'(') {
                p.wrapped(|p| p.position(dims))
            } else {
                p.position(dims)
            }
        })?),
        "LINESTRING" => OverlayGeometry::Line(vec![p.ring(dims)?]),
        "MULTILINESTRING" => OverlayGeometry::Line(p.list(|p| p.ring(dims))?),
        "POLYGON" => OverlayGeometry::Polygon(vec![p.list(|p| p.ring(dims))?]),
        "MULTIPOLYGON" => OverlayGeometry::Polygon(p.list(|p| p.list(|p| p.ring(dims)))?),
        "" => return Err(WktError::Malformed),
        _ => return Err(WktError::Unsupported),
    };
    p.space();
    if p.at != p.s.len() {
        return Err(WktError::Malformed);
    }
    Ok(geometry)
}

/// `POINTZ` -> (`POINT`, 3), `POINTZM` -> (`POINT`, 4); anything else as is.
fn split_dims(kind: &str) -> (&str, usize) {
    for (suffix, dims) in [("ZM", 4), ("Z", 3), ("M", 3)] {
        if let Some(base) = kind.strip_suffix(suffix) {
            if matches!(base, "POINT" | "MULTIPOINT" | "LINESTRING" | "MULTILINESTRING" | "POLYGON" | "MULTIPOLYGON") {
                return (base, dims);
            }
        }
    }
    (kind, 2)
}

struct Parser<'a> {
    s: &'a [u8],
    at: usize,
}

impl Parser<'_> {
    fn space(&mut self) {
        while self.at < self.s.len() && self.s[self.at].is_ascii_whitespace() {
            self.at += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.at).copied()
    }

    fn word(&mut self) -> String {
        let start = self.at;
        while self.at < self.s.len() && self.s[self.at].is_ascii_alphabetic() {
            self.at += 1;
        }
        String::from_utf8_lossy(&self.s[start..self.at]).into_owned()
    }

    fn expect(&mut self, byte: u8) -> Result<(), WktError> {
        self.space();
        if self.peek() == Some(byte) {
            self.at += 1;
            Ok(())
        } else {
            Err(WktError::Malformed)
        }
    }

    fn wrapped<T>(&mut self, inner: impl FnOnce(&mut Self) -> Result<T, WktError>) -> Result<T, WktError> {
        self.expect(b'(')?;
        let value = inner(self)?;
        self.expect(b')')?;
        Ok(value)
    }

    /// `( item, item, ... )`, at least one item.
    fn list<T>(&mut self, mut item: impl FnMut(&mut Self) -> Result<T, WktError>) -> Result<Vec<T>, WktError> {
        self.expect(b'(')?;
        let mut out = Vec::new();
        loop {
            out.push(item(self)?);
            self.space();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b')') => {
                    self.at += 1;
                    return Ok(out);
                }
                _ => return Err(WktError::Malformed),
            }
        }
    }

    /// `( x y, x y, ... )`: a line or a polygon ring.
    fn ring(&mut self, dims: usize) -> Result<Vec<(f64, f64)>, WktError> {
        self.list(|p| p.position(dims))
    }

    /// `x y` (plus `z`/`m` when the geometry declares them, or when one is
    /// simply there: plenty of writers emit `POINT (1 2 3)` with no `Z`).
    fn position(&mut self, dims: usize) -> Result<(f64, f64), WktError> {
        let x = self.number()?;
        let y = self.number()?;
        for _ in 2..dims {
            self.number()?;
        }
        // Undeclared extra ordinates.
        loop {
            self.space();
            match self.peek() {
                Some(b) if b == b'-' || b == b'+' || b == b'.' || b.is_ascii_digit() => {
                    self.number()?;
                }
                _ => break,
            }
        }
        Ok((x, y))
    }

    fn number(&mut self) -> Result<f64, WktError> {
        self.space();
        let start = self.at;
        while self.at < self.s.len()
            && matches!(self.s[self.at], b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E')
        {
            self.at += 1;
        }
        let text = std::str::from_utf8(&self.s[start..self.at]).map_err(|_| WktError::Malformed)?;
        let value: f64 = text.parse().map_err(|_| WktError::Malformed)?;
        if value.is_finite() {
            Ok(value)
        } else {
            Err(WktError::Malformed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_point_in_every_spelling() {
        let want = OverlayGeometry::Point(vec![(103.8, 1.35)]);
        for text in [
            "POINT(103.8 1.35)",
            "point (103.8 1.35)",
            "  POINT ( 103.8   1.35 )  ",
            "SRID=4326;POINT(103.8 1.35)",
            "POINT Z (103.8 1.35 12)",
            "POINTZ(103.8 1.35 12)",
            "POINT ZM (103.8 1.35 12 4)",
            "POINT M (103.8 1.35 4)",
            "POINT (103.8 1.35 12)",
            "POINT(1.038e2 1.35)",
        ] {
            assert_eq!(parse(text), Ok(want.clone()), "{text}");
        }
    }

    #[test]
    fn lines_and_polygons_keep_their_parts_and_holes() {
        assert_eq!(parse("LINESTRING (0 0, 1 1, 2 0)"), Ok(OverlayGeometry::Line(vec![vec![(0.0, 0.0), (1.0, 1.0), (2.0, 0.0)]])));
        assert_eq!(
            parse("MULTILINESTRING ((0 0, 1 1), (2 2, 3 3))"),
            Ok(OverlayGeometry::Line(vec![vec![(0.0, 0.0), (1.0, 1.0)], vec![(2.0, 2.0), (3.0, 3.0)]]))
        );
        let Ok(OverlayGeometry::Polygon(p)) = parse("POLYGON ((0 0, 4 0, 4 4, 0 4, 0 0), (1 1, 2 1, 2 2, 1 1))") else {
            panic!("a polygon")
        };
        assert_eq!((p.len(), p[0].len(), p[0][1].len()), (1, 2, 4));
        let Ok(OverlayGeometry::Polygon(mp)) =
            parse("MULTIPOLYGON (((0 0, 1 0, 1 1, 0 0)), ((5 5, 6 5, 6 6, 5 5), (5.2 5.1, 5.8 5.1, 5.8 5.7, 5.2 5.1)))")
        else {
            panic!("a multipolygon")
        };
        assert_eq!((mp.len(), mp[0].len(), mp[1].len()), (2, 1, 2));
    }

    #[test]
    fn a_multipoint_with_and_without_inner_parentheses() {
        let want = OverlayGeometry::Point(vec![(1.0, 2.0), (3.0, 4.0)]);
        assert_eq!(parse("MULTIPOINT ((1 2), (3 4))"), Ok(want.clone()));
        assert_eq!(parse("MULTIPOINT (1 2, 3 4)"), Ok(want));
    }

    #[test]
    fn empty_unsupported_and_malformed_are_told_apart() {
        assert_eq!(parse("POINT EMPTY"), Err(WktError::Empty));
        assert_eq!(parse("MULTIPOLYGON EMPTY"), Err(WktError::Empty));
        assert_eq!(parse("GEOMETRYCOLLECTION (POINT (1 2))"), Err(WktError::Unsupported));
        for bad in ["", "POINT", "POINT(1)", "POINT(1 2", "POINT(1 2))", "LINESTRING(0 0,)", "POINT(a b)", "POINT(1 2) x", "SRID=4326"] {
            assert_eq!(parse(bad), Err(WktError::Malformed), "{bad:?}");
        }
    }
}

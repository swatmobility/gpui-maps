//! A small WKT reader (§7.3): `POINT`, `MULTIPOINT`, `LINESTRING`,
//! `MULTILINESTRING`, `POLYGON` and `MULTIPOLYGON`, 2D, lon/lat.

/// One parsed geometry. Rings and lines are `[lon, lat]` vertices.
#[derive(Debug, Clone, PartialEq)]
pub enum Geometry {
    Points(Vec<[f64; 2]>),
    Lines(Vec<Vec<[f64; 2]>>),
    /// Polygons, each a list of rings (the first is the outer ring).
    Polygons(Vec<Vec<Vec<[f64; 2]>>>),
}

impl Geometry {
    /// `[west, south, east, north]`, or `None` for an empty geometry.
    pub fn bounds(&self) -> Option<[f64; 4]> {
        let mut b = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
        let mut add = |p: &[f64; 2]| {
            b[0] = b[0].min(p[0]);
            b[1] = b[1].min(p[1]);
            b[2] = b[2].max(p[0]);
            b[3] = b[3].max(p[1]);
        };
        match self {
            Geometry::Points(p) => p.iter().for_each(&mut add),
            Geometry::Lines(l) => l.iter().flatten().for_each(&mut add),
            Geometry::Polygons(p) => p.iter().flatten().flatten().for_each(&mut add),
        }
        b[0].is_finite().then_some(b)
    }
}

/// Parses one WKT string, or `None` when it cannot be read.
pub fn parse(text: &str) -> Option<Geometry> {
    let t = text.trim();
    let open = t.find('(')?;
    let tag = t[..open].trim().to_ascii_uppercase();
    let tree = Node::parse(&t[open..])?;
    Some(match tag.as_str() {
        "POINT" => Geometry::Points(vec![tree.point()?]),
        "MULTIPOINT" => Geometry::Points(tree.children()?.iter().map(Node::point_or_bare).collect::<Option<_>>()?),
        "LINESTRING" => Geometry::Lines(vec![tree.ring()?]),
        "MULTILINESTRING" => Geometry::Lines(tree.children()?.iter().map(Node::ring).collect::<Option<_>>()?),
        "POLYGON" => Geometry::Polygons(vec![tree.rings()?]),
        "MULTIPOLYGON" => Geometry::Polygons(tree.children()?.iter().map(Node::rings).collect::<Option<_>>()?),
        _ => return None,
    })
}

/// A parenthesised tree: a list of children, or one coordinate's text.
enum Node<'a> {
    List(Vec<Node<'a>>),
    Text(&'a str),
}

impl<'a> Node<'a> {
    fn parse(s: &'a str) -> Option<Node<'a>> {
        let (n, rest) = Self::list(s)?;
        rest.trim().is_empty().then_some(n)
    }

    /// `s` starts at `(`; returns the list and the text after its `)`.
    fn list(s: &'a str) -> Option<(Node<'a>, &'a str)> {
        let mut rest = s.trim_start().strip_prefix('(')?;
        let mut items = Vec::new();
        loop {
            rest = rest.trim_start();
            if rest.starts_with('(') {
                let (n, r) = Self::list(rest)?;
                items.push(n);
                rest = r;
            } else {
                let end = rest.find([',', ')'])?;
                items.push(Node::Text(rest[..end].trim()));
                rest = &rest[end..];
            }
            rest = rest.trim_start();
            if let Some(r) = rest.strip_prefix(',') {
                rest = r;
            } else {
                return Some((Node::List(items), rest.strip_prefix(')')?));
            }
        }
    }

    fn children(&self) -> Option<&[Node<'a>]> {
        match self {
            Node::List(v) => Some(v),
            Node::Text(_) => None,
        }
    }

    fn coord(s: &str) -> Option<[f64; 2]> {
        let mut it = s.split_whitespace();
        let x = it.next()?.parse().ok()?;
        let y = it.next()?.parse().ok()?;
        it.next().is_none().then_some([x, y])
    }

    fn point_or_bare(&self) -> Option<[f64; 2]> {
        match self {
            Node::Text(t) => Self::coord(t),
            Node::List(_) => self.point(),
        }
    }

    fn point(&self) -> Option<[f64; 2]> {
        match self.children()? {
            [Node::Text(t)] => Self::coord(t),
            _ => None,
        }
    }

    fn ring(&self) -> Option<Vec<[f64; 2]>> {
        self.children()?
            .iter()
            .map(|n| match n {
                Node::Text(t) => Self::coord(t),
                Node::List(_) => None,
            })
            .collect()
    }

    fn rings(&self) -> Option<Vec<Vec<[f64; 2]>>> {
        self.children()?.iter().map(Node::ring).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wkt_reads_each_type() {
        assert_eq!(parse("POINT(139.7 35.6)"), Some(Geometry::Points(vec![[139.7, 35.6]])));
        assert_eq!(parse("MULTIPOINT((1 2), (3 4))"), Some(Geometry::Points(vec![[1.0, 2.0], [3.0, 4.0]])));
        assert_eq!(parse("MULTIPOINT(1 2, 3 4)"), Some(Geometry::Points(vec![[1.0, 2.0], [3.0, 4.0]])));
        assert_eq!(parse("LINESTRING(1 2, 3 4)"), Some(Geometry::Lines(vec![vec![[1.0, 2.0], [3.0, 4.0]]])));
        assert!(matches!(parse("MULTILINESTRING((1 2, 3 4), (5 6, 7 8))"), Some(Geometry::Lines(l)) if l.len() == 2));
        let p = parse("POLYGON((0 0, 1 0, 1 1, 0 0))").unwrap();
        assert_eq!(p.bounds(), Some([0.0, 0.0, 1.0, 1.0]));
        let mp = parse("MULTIPOLYGON(((0 0, 1 0, 1 1, 0 0)), ((2 2, 3 2, 3 3, 2 2), (2.1 2.1, 2.2 2.1, 2.2 2.2, 2.1 2.1)))").unwrap();
        assert!(matches!(&mp, Geometry::Polygons(p) if p.len() == 2 && p[1].len() == 2));
    }

    #[test]
    fn unreadable_wkt_is_none() {
        for bad in ["", "POINT", "POINT(1)", "LINESTRING(1 2, 3)", "POLYGON((0 0, 1 0)", "CIRCLE(1 2)"] {
            assert!(parse(bad).is_none(), "{bad}");
        }
    }
}

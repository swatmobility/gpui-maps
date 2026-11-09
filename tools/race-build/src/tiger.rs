//! TIGER/Line 2020 block polygons (`tl_2020_<FIPS>_tabblock20.zip`), one
//! state at a time: stream the `.shp` and `.dbf` members of the zip in
//! lockstep (no extraction to disk, no whole-file buffer), project to the
//! 2^31 Web Mercator world grid, simplify and triangulate.
//!
//! The readers are hand-rolled rather than the `shapefile` crate the plan
//! names: its iterators need `Read + Seek`, and a zip member is a forward-only
//! stream, so using it meant either unpacking Texas's 740 MB to disk or
//! holding it in memory. TIGER writes plain polygons (shape type 5) and a
//! dBASE III table, both fixed layouts (ESRI Shapefile Technical
//! Description, 1998; dBASE III file header), so the forward-only readers
//! below are short.

use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;

use anyhow::{bail, Context, Result};
use gpui_maps::overlay::dotgrid::parts::{Tri, WORLD_QUANTA};
use lyon::math::point;
use lyon::path::Path as LyonPath;
use lyon::tessellation::{BuffersBuilder, FillOptions, FillRule, FillTessellator, FillVertex, VertexBuffers};

/// Mercator latitude limit (the renderer's square); no US block reaches it.
const MAX_LAT: f64 = 85.051_128_779_806_59;

/// Equatorial circumference in metres (WGS 84), the Mercator scale.
const EARTH_CIRCUMFERENCE_M: f64 = 40_075_016.686;

/// Lon/lat (degrees) to the 2^31 world grid: x east from 180°W, y south
/// from the top of the Mercator square, both rounded to the nearest quantum.
pub fn project(lon: f64, lat: f64) -> [i64; 2] {
    let q = WORLD_QUANTA as f64;
    let lat = lat.clamp(-MAX_LAT, MAX_LAT).to_radians();
    let x = (lon + 180.0) / 360.0 * q;
    let y = (1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0 * q;
    [(x.round() as i64).clamp(0, WORLD_QUANTA - 1), (y.round() as i64).clamp(0, WORLD_QUANTA - 1)]
}

/// Inverse of [`project`] for a point in (fractional) quanta: `(lat, lon)`.
pub fn unproject(p: [f64; 2]) -> (f64, f64) {
    let q = WORLD_QUANTA as f64;
    let lon = p[0] / q * 360.0 - 180.0;
    let lat = (std::f64::consts::PI * (1.0 - 2.0 * p[1] / q)).sinh().atan().to_degrees();
    (lat, lon)
}

/// World-grid quanta per ground metre at `lat` (Mercator scale 1/cos φ).
pub fn quanta_per_metre(lat: f64) -> f64 {
    WORLD_QUANTA as f64 / (EARTH_CIRCUMFERENCE_M * lat.to_radians().cos())
}

/// Squared distance from `p` to the segment `a`–`b`.
pub fn seg_dist2(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let len2 = dx * dx + dy * dy;
    let t = if len2 == 0.0 { 0.0 } else { (((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / len2).clamp(0.0, 1.0) };
    let (ex, ey) = (a[0] + t * dx - p[0], a[1] + t * dy - p[1]);
    ex * ex + ey * ey
}

fn f(p: [i64; 2]) -> [f64; 2] {
    [p[0] as f64, p[1] as f64]
}

/// `ring` without a closing point that repeats its first.
fn open_ring(ring: &[[i64; 2]]) -> &[[i64; 2]] {
    match ring {
        [first, .., last] if ring.len() > 1 && first == last => &ring[..ring.len() - 1],
        _ => ring,
    }
}

/// Douglas–Peucker on a closed ring: drops every vertex within `tol` quanta
/// of the chain the kept vertices make. The ring is split at its first
/// vertex and the vertex farthest from it, and each half simplified.
/// A closing duplicate of the first point is dropped from the output (and
/// accepted, not required, on input). A ring that would fall below 3
/// vertices comes back unsimplified: a tiny ring is kept, not lost.
pub fn simplify(ring: &[[i64; 2]], tol: i64) -> Vec<[i64; 2]> {
    let pts = open_ring(ring);
    let n = pts.len();
    if n < 4 {
        return pts.to_vec();
    }
    let p0 = f(pts[0]);
    let far = (1..n)
        .max_by(|&i, &j| {
            let d = |k: usize| {
                let q = f(pts[k]);
                (q[0] - p0[0]).powi(2) + (q[1] - p0[1]).powi(2)
            };
            d(i).total_cmp(&d(j))
        })
        .unwrap();
    let tol2 = (tol as f64) * (tol as f64);
    let mut keep = vec![false; n];
    keep[0] = true;
    keep[far] = true;
    // Chains [0, far] and [far, n] (index n is vertex 0 again).
    let at = |i: usize| f(pts[i % n]);
    let mut stack = vec![(0usize, far), (far, n)];
    while let Some((lo, hi)) = stack.pop() {
        if hi <= lo + 1 {
            continue;
        }
        let (a, b) = (at(lo), at(hi));
        let (mut best, mut best_d) = (0usize, -1.0f64);
        for i in lo + 1..hi {
            let d = seg_dist2(at(i), a, b);
            if d > best_d {
                best = i;
                best_d = d;
            }
        }
        if best_d > tol2 {
            keep[best % n] = true;
            stack.push((lo, best));
            stack.push((best, hi));
        }
    }
    let out: Vec<[i64; 2]> = pts.iter().zip(&keep).filter(|(_, &k)| k).map(|(&p, _)| p).collect();
    if out.len() < 3 {
        pts.to_vec()
    } else {
        out
    }
}

/// Twice the signed area of a triangle, exact.
pub fn doubled_area(t: &Tri) -> i128 {
    let (ex, ey) = (i128::from(t.b[0] - t.a[0]), i128::from(t.b[1] - t.a[1]));
    let (fx, fy) = (i128::from(t.c[0] - t.a[0]), i128::from(t.c[1] - t.a[1]));
    ex * fy - ey * fx
}

/// Triangulates a polygon given as rings (shell and holes, any winding) with
/// lyon's `FillTessellator` under the even-odd rule (ruling C5: TIGER rings
/// do not self-overlap, and even-odd needs no winding convention).
///
/// lyon works in f32, which is exact only to 2^24, so every ring is shifted
/// to the rings' bounding-box minimum first and the output rounded back to
/// integers and shifted home (C5). Zero-area triangles are dropped. An
/// input lyon refuses gives no triangles; the caller falls back.
pub fn triangulate(rings: &[Vec<[i64; 2]>]) -> Vec<Tri> {
    let Some(min) = rings.iter().flatten().copied().reduce(|m, p| [m[0].min(p[0]), m[1].min(p[1])]) else {
        return Vec::new();
    };
    let mut builder = LyonPath::builder();
    for ring in rings {
        let pts = open_ring(ring);
        if pts.len() < 3 {
            continue;
        }
        let rel = |p: [i64; 2]| point((p[0] - min[0]) as f32, (p[1] - min[1]) as f32);
        builder.begin(rel(pts[0]));
        for &p in &pts[1..] {
            builder.line_to(rel(p));
        }
        builder.end(true);
    }
    let path = builder.build();
    let mut buffers: VertexBuffers<[i64; 2], u32> = VertexBuffers::new();
    let options = FillOptions::tolerance(0.25).with_fill_rule(FillRule::EvenOdd);
    let result = FillTessellator::new().tessellate_path(
        &path,
        &options,
        &mut BuffersBuilder::new(&mut buffers, |v: FillVertex| {
            let p = v.position();
            [min[0] + p.x.round() as i64, min[1] + p.y.round() as i64]
        }),
    );
    if result.is_err() {
        return Vec::new();
    }
    buffers
        .indices
        .chunks_exact(3)
        .map(|t| Tri { a: buffers.vertices[t[0] as usize], b: buffers.vertices[t[1] as usize], c: buffers.vertices[t[2] as usize] })
        .filter(|t| doubled_area(t) != 0)
        .collect()
}

/// Even-odd point-in-polygon over every ring (a point on an edge may land
/// either side).
pub fn inside(p: [f64; 2], rings: &[Vec<[i64; 2]>]) -> bool {
    let mut odd = false;
    for ring in rings {
        let pts = open_ring(ring);
        let n = pts.len();
        for i in 0..n {
            let (a, b) = (f(pts[i]), f(pts[(i + 1) % n]));
            if (a[1] > p[1]) != (b[1] > p[1]) {
                let x = a[0] + (p[1] - a[1]) / (b[1] - a[1]) * (b[0] - a[0]);
                if p[0] < x {
                    odd = !odd;
                }
            }
        }
    }
    odd
}

/// Distance from `p` to the nearest ring edge, in quanta.
pub fn boundary_distance(p: [f64; 2], rings: &[Vec<[i64; 2]>]) -> f64 {
    let mut best = f64::INFINITY;
    for ring in rings {
        let pts = open_ring(ring);
        let n = pts.len();
        for i in 0..n {
            best = best.min(seg_dist2(p, f(pts[i]), f(pts[(i + 1) % n])));
        }
    }
    best.sqrt()
}

// ---------------------------------------------------------------------
// Forward-only .shp and .dbf readers.

/// Reads polygon records from a `.shp` stream in file order.
pub struct ShpReader<R: Read> {
    r: R,
}

fn read_exact_or_eof(r: &mut impl Read, buf: &mut [u8]) -> io::Result<bool> {
    let mut got = 0;
    while got < buf.len() {
        match r.read(&mut buf[got..])? {
            0 if got == 0 => return Ok(false),
            0 => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "a truncated record")),
            k => got += k,
        }
    }
    Ok(true)
}

impl<R: Read> ShpReader<R> {
    /// Reads and checks the 100-byte main file header.
    pub fn new(mut r: R) -> Result<Self> {
        let mut h = [0u8; 100];
        r.read_exact(&mut h).context("reading the .shp header")?;
        let code = i32::from_be_bytes(h[0..4].try_into().unwrap());
        if code != 9994 {
            bail!(".shp file code {code}, want 9994");
        }
        let shape = i32::from_le_bytes(h[32..36].try_into().unwrap());
        if shape != 5 {
            bail!(".shp shape type {shape}, want 5 (polygon)");
        }
        Ok(Self { r })
    }

    /// The next record's rings as `[lon, lat]`, or `None` at end of file. A
    /// null shape (type 0) is an empty ring list.
    pub fn next_polygon(&mut self) -> Result<Option<Vec<Vec<[f64; 2]>>>> {
        let mut rh = [0u8; 8];
        if !read_exact_or_eof(&mut self.r, &mut rh)? {
            return Ok(None);
        }
        let words = i32::from_be_bytes(rh[4..8].try_into().unwrap());
        let len = usize::try_from(words).context("a negative .shp content length")? * 2;
        let mut body = vec![0u8; len];
        self.r.read_exact(&mut body).context("reading a .shp record")?;
        parse_polygon(&body).map(Some)
    }
}

fn parse_polygon(body: &[u8]) -> Result<Vec<Vec<[f64; 2]>>> {
    let i32_at = |o: usize| -> Result<i32> {
        Ok(i32::from_le_bytes(body.get(o..o + 4).context("a short .shp record")?.try_into().unwrap()))
    };
    let f64_at = |o: usize| -> Result<f64> {
        Ok(f64::from_le_bytes(body.get(o..o + 8).context("a short .shp record")?.try_into().unwrap()))
    };
    match i32_at(0)? {
        0 => Ok(Vec::new()),
        5 => {
            let parts = usize::try_from(i32_at(36)?)?;
            let points = usize::try_from(i32_at(40)?)?;
            let starts: Vec<usize> = (0..parts).map(|i| i32_at(44 + 4 * i).map(|s| s as usize)).collect::<Result<_>>()?;
            let base = 44 + 4 * parts;
            let mut rings = Vec::with_capacity(parts);
            for (i, &s) in starts.iter().enumerate() {
                let e = starts.get(i + 1).copied().unwrap_or(points);
                if s > e || e > points {
                    bail!("a .shp part index out of order");
                }
                let ring = (s..e).map(|k| Ok([f64_at(base + 16 * k)?, f64_at(base + 16 * k + 8)?])).collect::<Result<Vec<_>>>()?;
                rings.push(ring);
            }
            Ok(rings)
        }
        t => bail!(".shp record of shape type {t}, want 5 (polygon)"),
    }
}

/// Reads one character field from a dBASE III stream, record by record.
pub struct DbfReader<R: Read> {
    r: R,
    records: u32,
    read: u32,
    record_len: usize,
    field_off: usize,
    field_len: usize,
}

impl<R: Read> DbfReader<R> {
    /// Reads the header and locates `field`.
    pub fn new(mut r: R, field: &str) -> Result<Self> {
        let mut h = [0u8; 32];
        r.read_exact(&mut h).context("reading the .dbf header")?;
        let records = u32::from_le_bytes(h[4..8].try_into().unwrap());
        let header_len = usize::from(u16::from_le_bytes(h[8..10].try_into().unwrap()));
        let record_len = usize::from(u16::from_le_bytes(h[10..12].try_into().unwrap()));
        let mut rest = vec![0u8; header_len.checked_sub(32).context("a .dbf header under 32 bytes")?];
        r.read_exact(&mut rest).context("reading the .dbf field descriptors")?;
        let mut off = 1; // the deletion flag
        let mut found = None;
        for d in rest.chunks_exact(32) {
            if d[0] == 0x0D {
                break;
            }
            let name = String::from_utf8_lossy(&d[..11]).trim_end_matches('\0').to_string();
            let len = usize::from(d[16]);
            if name == field {
                found = Some((off, len));
            }
            off += len;
        }
        let (field_off, field_len) = found.with_context(|| format!(".dbf has no {field} field"))?;
        if off != record_len {
            bail!(".dbf fields sum to {off} bytes, the header says {record_len}");
        }
        Ok(Self { r, records, read: 0, record_len, field_off, field_len })
    }

    /// Reads whatever follows the last record (the 0x1A end marker).
    pub fn drain(&mut self) -> Result<()> {
        io::copy(&mut self.r, &mut io::sink()).context("reading the .dbf to its end")?;
        Ok(())
    }

    /// The next record's field, trimmed, or `None` after the last record.
    pub fn next_field(&mut self, buf: &mut Vec<u8>) -> Result<Option<String>> {
        if self.read == self.records {
            return Ok(None);
        }
        buf.resize(self.record_len, 0);
        self.r.read_exact(buf).context("reading a .dbf record")?;
        self.read += 1;
        let v = &buf[self.field_off..self.field_off + self.field_len];
        Ok(Some(String::from_utf8_lossy(v).trim().to_string()))
    }
}

/// Streams one state's blocks from its TIGER zip: `visit(geoid, rings)` for
/// every record, in file order (rings as `[lon, lat]`). The zip is opened
/// twice so the `.shp` and `.dbf` members stream side by side.
pub fn for_each_block(zip_path: &Path, mut visit: impl FnMut(u64, Vec<Vec<[f64; 2]>>) -> Result<()>) -> Result<u64> {
    let open = || -> Result<zip::ZipArchive<BufReader<File>>> {
        let f = File::open(zip_path).with_context(|| format!("opening {}", zip_path.display()))?;
        zip::ZipArchive::new(BufReader::new(f)).with_context(|| format!("reading {} as a zip", zip_path.display()))
    };
    let (mut za, mut zb) = (open()?, open()?);
    let name = |za: &zip::ZipArchive<BufReader<File>>, ext: &str| -> Result<String> {
        za.file_names().find(|n| n.ends_with(ext)).map(str::to_string).with_context(|| format!("{}: no *{ext} member", zip_path.display()))
    };
    let (shp_name, dbf_name) = (name(&za, ".shp")?, name(&za, ".dbf")?);
    let mut shp = ShpReader::new(BufReader::with_capacity(1 << 20, za.by_name(&shp_name)?))?;
    let mut dbf = DbfReader::new(BufReader::with_capacity(1 << 20, zb.by_name(&dbf_name)?), "GEOID20")?;
    let mut buf = Vec::new();
    let mut n = 0u64;
    loop {
        match (shp.next_polygon()?, dbf.next_field(&mut buf)?) {
            (Some(rings), Some(geoid)) => {
                let geoid: u64 = geoid.parse().with_context(|| format!("GEOID20 {geoid:?} is not a number"))?;
                visit(geoid, rings)?;
                n += 1;
            }
            (None, None) => {
                // Drain the .dbf to its end so the zip reader checks its
                // CRC too (it only does at EOF; review F7).
                dbf.drain()?;
                return Ok(n);
            }
            _ => bail!("{}: the .shp and .dbf hold different record counts", zip_path.display()),
        }
    }
}

/// Unpacked bytes of every member of a zip (from its central directory).
pub fn unpacked_size(zip_path: &Path) -> Result<u64> {
    let f = File::open(zip_path).with_context(|| format!("opening {}", zip_path.display()))?;
    let mut za = zip::ZipArchive::new(BufReader::new(f))?;
    let mut total = 0;
    for i in 0..za.len() {
        total += za.by_index_raw(i)?.size();
    }
    Ok(total)
}

/// For tests: a minimal `.shp` byte stream of polygons.
#[cfg(test)]
pub fn shp_bytes(polys: &[Vec<Vec<[f64; 2]>>]) -> Vec<u8> {
    let mut body = Vec::new();
    for (i, rings) in polys.iter().enumerate() {
        let mut c = Vec::new();
        c.extend_from_slice(&5i32.to_le_bytes());
        c.extend_from_slice(&[0u8; 32]);
        let npts: usize = rings.iter().map(Vec::len).sum();
        c.extend_from_slice(&(rings.len() as i32).to_le_bytes());
        c.extend_from_slice(&(npts as i32).to_le_bytes());
        let mut s = 0i32;
        for r in rings {
            c.extend_from_slice(&s.to_le_bytes());
            s += r.len() as i32;
        }
        for p in rings.iter().flatten() {
            c.extend_from_slice(&p[0].to_le_bytes());
            c.extend_from_slice(&p[1].to_le_bytes());
        }
        body.extend_from_slice(&(i as i32 + 1).to_be_bytes());
        body.extend_from_slice(&((c.len() / 2) as i32).to_be_bytes());
        body.extend_from_slice(&c);
    }
    let mut h = vec![0u8; 100];
    h[0..4].copy_from_slice(&9994i32.to_be_bytes());
    h[24..28].copy_from_slice((((100 + body.len()) / 2) as i32).to_be_bytes().as_slice());
    h[28..32].copy_from_slice(&1000i32.to_le_bytes());
    h[32..36].copy_from_slice(&5i32.to_le_bytes());
    h.extend_from_slice(&body);
    h
}

/// For tests: a minimal `.dbf` with a leading `STATEFP20` C(2) field and a
/// `GEOID20` C(15) field.
#[cfg(test)]
pub fn dbf_bytes(geoids: &[u64]) -> Vec<u8> {
    let fields: [(&str, u8); 2] = [("STATEFP20", 2), ("GEOID20", 15)];
    let record_len = 1 + 2 + 15;
    let header_len = 32 + 32 * fields.len() + 1;
    let mut b = vec![0u8; 32];
    b[0] = 3;
    b[4..8].copy_from_slice(&(geoids.len() as u32).to_le_bytes());
    b[8..10].copy_from_slice(&(header_len as u16).to_le_bytes());
    b[10..12].copy_from_slice(&(record_len as u16).to_le_bytes());
    for (name, len) in fields {
        let mut d = [0u8; 32];
        d[..name.len()].copy_from_slice(name.as_bytes());
        d[11] = b'C';
        d[16] = len;
        b.extend_from_slice(&d);
    }
    b.push(0x0D);
    for g in geoids {
        b.push(b' ');
        b.extend_from_slice(format!("{:02}", g / 10u64.pow(13)).as_bytes());
        b.extend_from_slice(format!("{g:015}").as_bytes());
    }
    b.push(0x1A);
    b
}

/// Cross-reference for tests: every GEOID in a dbf stream.
#[cfg(test)]
fn all_geoids(bytes: &[u8]) -> Vec<String> {
    let mut d = DbfReader::new(bytes, "GEOID20").unwrap();
    let mut buf = Vec::new();
    let mut out = Vec::new();
    while let Some(g) = d.next_field(&mut buf).unwrap() {
        out.push(g);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simplify_drops_collinear_points_within_tolerance() {
        let ring = vec![[0, 0], [50, 1], [100, 0], [100, 100], [0, 100]];
        assert_eq!(simplify(&ring, 2), vec![[0, 0], [100, 0], [100, 100], [0, 100]]);
    }

    #[test]
    fn simplify_keeps_a_closed_ring_closed_off_and_a_tiny_ring_whole() {
        let closed = vec![[0, 0], [50, 1], [100, 0], [100, 100], [0, 100], [0, 0]];
        assert_eq!(simplify(&closed, 2), vec![[0, 0], [100, 0], [100, 100], [0, 100]]);
        // Everything within tolerance: the ring would collapse, so it stays.
        let tiny = vec![[0, 0], [1, 0], [1, 1], [0, 1]];
        assert_eq!(simplify(&tiny, 5), tiny);
    }

    #[test]
    fn triangulation_covers_the_polygon_area_exactly() {
        let shell = vec![[0, 0], [100, 0], [100, 100], [0, 100]];
        let hole = vec![[40, 40], [60, 40], [60, 60], [40, 60]];
        let tris = triangulate(&[shell, hole]);
        let area2: i128 = tris.iter().map(|t| doubled_area(t).abs()).sum();
        assert_eq!(area2, 2 * (10_000 - 400));
    }

    #[test]
    fn triangulation_is_exact_far_from_the_origin() {
        // A block near 2^31 quanta: f32 would round these to multiples of
        // 128 without the bbox shift (C5).
        let (x0, y0) = (2_000_000_001i64, 1_500_000_003i64);
        let shell = vec![[x0, y0], [x0 + 1001, y0], [x0 + 1001, y0 + 777], [x0, y0 + 777]];
        let tris = triangulate(&[shell]);
        let area2: i128 = tris.iter().map(|t| doubled_area(t).abs()).sum();
        assert_eq!(area2, 2 * 1001 * 777);
        assert!(tris.iter().all(|t| [t.a, t.b, t.c].iter().all(|p| p[0] >= x0 && p[1] >= y0)));
    }

    #[test]
    fn project_round_trips_and_scales() {
        let p = project(-77.0365, 38.8977);
        let (lat, lon) = unproject([p[0] as f64, p[1] as f64]);
        assert!((lat - 38.8977).abs() < 1e-6 && (lon + 77.0365).abs() < 1e-6, "{lat} {lon}");
        // One metre at the equator is 2^31 / 40,075,016.686 = 53.6 quanta.
        assert!((quanta_per_metre(0.0) - 53.586).abs() < 0.01);
    }

    #[test]
    fn inside_and_boundary_distance_follow_even_odd() {
        let shell = vec![[0, 0], [100, 0], [100, 100], [0, 100]];
        let hole = vec![[40, 40], [60, 40], [60, 60], [40, 60]];
        let rings = vec![shell, hole];
        assert!(inside([10.0, 10.0], &rings));
        assert!(!inside([50.0, 50.0], &rings));
        assert!(!inside([150.0, 50.0], &rings));
        assert!((boundary_distance([50.0, 50.0], &rings) - 10.0).abs() < 1e-9);
    }

    #[test]
    fn shp_and_dbf_streams_read_back_in_lockstep() {
        let polys = vec![
            vec![vec![[-77.0, 38.9], [-76.9, 38.9], [-76.9, 39.0], [-77.0, 38.9]]],
            vec![vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 0.0]], vec![[0.2, 0.1], [0.3, 0.1], [0.3, 0.2], [0.2, 0.1]]],
        ];
        let shp = shp_bytes(&polys);
        let mut r = ShpReader::new(&shp[..]).unwrap();
        assert_eq!(r.next_polygon().unwrap().unwrap(), polys[0]);
        assert_eq!(r.next_polygon().unwrap().unwrap(), polys[1]);
        assert!(r.next_polygon().unwrap().is_none());

        let dbf = dbf_bytes(&[110_010_001_011_000, 110_010_001_011_001]);
        assert_eq!(all_geoids(&dbf), vec!["110010001011000", "110010001011001"]);
    }
}

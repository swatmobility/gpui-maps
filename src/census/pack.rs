//! `BGPK` v2: the census demo's block groups for a browser. See
//! `docs/superpowers/specs/2026-09-28-census-web-design.md` section 2.2.
//! All integers little-endian; sections and tables are raw deflate.
//!
//! Layout: a 64-byte header, one 44-byte directory row per state (sorted
//! by FIPS), each state's geometry section, then one tables section (the
//! per-feature rows in pack order, the county names and the CBSA table).
//! A page reads it with three `Range` requests before any geometry: the
//! header, the header plus directory, the tables.
//!
//! A state section stores each shared edge once, as TopoJSON does: first
//! the state's arcs (a point count, then zig-zag varint deltas running on
//! from the previous arc), then per feature its polygons, per polygon its
//! rings, per ring its arc references. A reference is
//! `zigzag(index - previous index) << 1 | reversed`; a ring is its arcs
//! concatenated, each after the first dropping the point it shares with
//! the one before. An arc on a state line is stored in both states, so a
//! state still decodes alone. v1 (every ring's points in full) is refused
//! by version, naming the rebuild.

use std::collections::HashMap;

pub const MAGIC: [u8; 4] = *b"BGPK";
pub const VERSION: u32 = 2;
pub const HEADER_LEN: usize = 64;
pub const DIR_ROW: usize = 44;
pub const QUANTUM_DEG: f64 = 1e-6;
/// No decoded section or table may claim more than this (a bomb guard).
/// The largest honest section is Texas's, 3.37 MB raw at 5 m (2.4x under
/// the cap); the tables, which share the cap, are 4.97 MB (1.7x under). The
/// converter refuses to write past it.
///
/// Worst-case decoder memory for one section of `R` raw bytes (R <= 8 MiB),
/// every structure bounded by the bytes that describe it:
/// - the inflated section: `R`;
/// - the arc table (8 B an arc) and use counts (1 B): an arc takes at least
///   3 bytes, so at most `R/3` arcs, `3R`;
/// - with `P` stored points (2+ bytes each) and `S` features, polygons and
///   rings (1+ byte each), `2P + S <= R`: stored points `16P` (reserved
///   exactly), assembled points `32P` (at most twice the stored, reserved
///   exactly), and the three range tables `8S`, up to `24S` while a `Vec`
///   doubles (old and new block live at once). `48P + 24S <= 24R`.
///
/// So at most `28R`: 224 MiB at the cap, the largest single block 16R =
/// 128 MiB (well under wasm32's 2 GiB `isize::MAX`), plus the compressed
/// body the caller holds. Texas (3.37 MB raw, 787 k stored points) needs about
/// 45 MB by the same arithmetic.
pub const MAX_SECTION_RAW: u32 = 8 << 20;
const NONE: u32 = u32::MAX;
/// Deflate's worst-case expansion (a 258-byte match per ~2 bits), rounded
/// up: the most raw bytes one compressed byte can honestly claim.
const MAX_DEFLATE_RATIO: u64 = 1032;

pub type Ring = Vec<[f64; 2]>;
pub type Polygon = Vec<Ring>;
pub type Feature = Vec<Polygon>;

pub struct EncodeState {
    pub fips: u8,
    pub features: Vec<Feature>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PackRow {
    pub geoid: u64,
    pub income: Option<u32>,
    pub population: Option<u32>,
    pub aland_km2: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DirRow {
    pub fips: u8,
    pub first_feature: u32,
    pub feature_count: u32,
    pub offset: u64,
    pub len: u32,
    pub raw_len: u32,
    /// `[west, south, east, north]` in degrees. For a state across the
    /// antimeridian (Alaska's Aleutians) the narrower box is written with
    /// `east` past 180 (`[172.4, 51.2, 230.0, 71.4]`), not a box round the
    /// whole world; [`load_order`] tests it both ways.
    pub bbox: [f32; 4],
}

#[derive(Clone, Debug, PartialEq)]
pub struct PackHeader {
    pub tolerance_m: f32,
    pub feature_count: u32,
    pub states: Vec<DirRow>,
    pub tables_offset: u64,
    pub tables_len: u32,
    pub tables_raw_len: u32,
}

/// One state's geometry, flat: `features[i]` is a range of `polygons`,
/// `polygons[j]` a range of `rings`, `rings[k]` a range of `coords`.
#[derive(Debug, Default)]
pub struct StateGeometry {
    /// The rings' points, assembled from the arcs.
    pub coords: Vec<[f64; 2]>,
    /// Points the section stores: each arc once.
    pub stored_points: usize,
    pub rings: Vec<(u32, u32)>,
    pub polygons: Vec<(u32, u32)>,
    pub features: Vec<(u32, u32)>,
}

pub struct Tables {
    pub rows: Vec<PackRow>,
    pub county_text: String,
    pub cbsa_text: String,
}

#[derive(Debug, PartialEq)]
pub enum PackError {
    Magic,
    Version(u32),
    Truncated(&'static str),
    TooLarge { what: &'static str, claimed: u32 },
    Inflate(String),
    Count(&'static str),
    /// A header whose quantum or reserved word is not this format's.
    Header(&'static str),
    /// A decoded coordinate outside +-180 / +-90 (a corrupt delta).
    Coordinate { lon: f64, lat: f64 },
}

impl std::fmt::Display for PackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PackError::Version(v) => write!(
                f,
                "census pack: format v{v} is not read by this build (it reads v{VERSION}); rebuild the pack with tools/census-pack"
            ),
            _ => write!(f, "census pack: {self:?}"),
        }
    }
}

impl std::error::Error for PackError {}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Cursor<'_> {
    fn varint(&mut self, what: &'static str) -> Result<u64, PackError> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = *self.bytes.get(self.at).ok_or(PackError::Truncated(what))?;
            self.at += 1;
            // The tenth byte carries bit 63 only: more is an overlong varint.
            if shift == 63 && b > 1 { // guard: overlong varint
                return Err(PackError::Count(what));
            }
            v |= u64::from(b & 0x7f) << shift;
            if b < 0x80 {
                return Ok(v);
            }
        }
        Err(PackError::Truncated(what))
    }
    fn take(&mut self, n: usize, what: &'static str) -> Result<&[u8], PackError> {
        let end = self.at.checked_add(n).ok_or(PackError::Truncated(what))?;
        let s = self.bytes.get(self.at..end).ok_or(PackError::Truncated(what))?;
        self.at = end;
        Ok(s)
    }
    fn u32(&mut self, what: &'static str) -> Result<u32, PackError> {
        Ok(u32::from_le_bytes(self.take(4, what)?.try_into().expect("4 bytes")))
    }
    fn u64(&mut self, what: &'static str) -> Result<u64, PackError> {
        Ok(u64::from_le_bytes(self.take(8, what)?.try_into().expect("8 bytes")))
    }
    fn f32(&mut self, what: &'static str) -> Result<f32, PackError> {
        Ok(f32::from_le_bytes(self.take(4, what)?.try_into().expect("4 bytes")))
    }
}

fn deflate(raw: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(6));
    e.write_all(raw).expect("in-memory write");
    e.finish().expect("in-memory finish")
}

/// Inflates exactly `raw_len` bytes, refusing a claim over
/// [`MAX_SECTION_RAW`] before allocating, and a stream that ends early or
/// never reaches its end-of-stream marker.
fn inflate(body: &[u8], raw_len: u32, what: &'static str) -> Result<Vec<u8>, PackError> {
    // Deflate expands at most ~1032:1, so a claim past that for this body
    // is a lie -- refused before it can reserve 256 MiB for a 40-byte body.
    if raw_len > MAX_SECTION_RAW || u64::from(raw_len) > body.len() as u64 * MAX_DEFLATE_RATIO + 64 {
        return Err(PackError::TooLarge { what, claimed: raw_len });
    }
    // One spare byte, so a stream longer than it claims shows as a full
    // buffer rather than a silent cut.
    let cap = raw_len as usize + 1;
    let mut out = Vec::with_capacity(cap);
    let mut d = flate2::Decompress::new(false);
    loop {
        let (in_before, out_before) = (d.total_in(), d.total_out());
        let rest = body.get(in_before as usize..).unwrap_or_default();
        let status = d
            .decompress_vec(rest, &mut out, flate2::FlushDecompress::Finish)
            .map_err(|e| PackError::Inflate(e.to_string()))?;
        if out.len() >= cap {
            return Err(PackError::Count(what));
        }
        match status {
            // Only the decoder's own end marker says the body was whole: a
            // body cut inside its final block can still yield every byte.
            flate2::Status::StreamEnd => break,
            _ if d.total_in() == in_before && d.total_out() == out_before => return Err(PackError::Truncated(what)),
            _ => {}
        }
    }
    if out.len() != raw_len as usize {
        return Err(PackError::Truncated(what));
    }
    Ok(out)
}

fn q(v: f64) -> i64 {
    (v / QUANTUM_DEG).round() as i64
}

/// One state's rings cut into shared arcs, for the encoder.
struct StateArcs {
    /// Distinct quantised points; arcs and rings refer to them by index.
    points: Vec<(i64, i64)>,
    /// Each arc's point indices, in first-use order.
    arcs: Vec<Vec<u32>>,
    /// Per ring, in feature/polygon/ring order: `(arc, reversed)`.
    refs: Vec<Vec<(u32, bool)>>,
}

impl StateArcs {
    /// Cuts every clean ring (closed, at least 4 points, no repeated
    /// consecutive point) at its junctions: a point whose distinct
    /// neighbours are not exactly two, or a ring's first point. Between
    /// junctions every point has exactly two neighbours, so an arc is fixed
    /// by its first step, and two rings running one edge share one arc. Any
    /// other ring is its own private arc (an empty one, no arcs), so encoding
    /// is lossless for any input (the decoder rebuilds each ring point for
    /// point).
    ///
    /// The junction rule is `tools/census-pack/src/topo.rs`'s
    /// (`simplify_national`), which simplifies the national arcs. This one
    /// only rediscovers them per state for storage, and must stay lossless
    /// for rings topo never saw, so the two are kept apart: a change to the
    /// rule belongs in both.
    fn build(features: &[Feature]) -> StateArcs {
        const NONE: u32 = u32::MAX;
        let mut id_of: HashMap<(i64, i64), u32> = HashMap::new();
        let mut points: Vec<(i64, i64)> = Vec::new();
        let rings: Vec<Vec<u32>> = features
            .iter()
            .flatten()
            .flatten()
            .map(|ring| {
                ring.iter()
                    .map(|&[lon, lat]| {
                        let p = (q(lon), q(lat));
                        *id_of.entry(p).or_insert_with(|| {
                            points.push(p);
                            (points.len() - 1) as u32
                        })
                    })
                    .collect()
            })
            .collect();
        let clean = |r: &[u32]| r.len() >= 4 && r[0] == r[r.len() - 1] && r.windows(2).all(|w| w[0] != w[1]);
        let mut nb = vec![[NONE; 2]; points.len()];
        let mut junction = vec![false; points.len()];
        for r in rings.iter().filter(|r| clean(r)) {
            let n = r.len() - 1;
            for i in 0..n {
                for u in [r[(i + n - 1) % n], r[i + 1]] {
                    let s = &mut nb[r[i] as usize];
                    if s[0] == u || s[1] == u {
                    } else if s[0] == NONE {
                        s[0] = u;
                    } else if s[1] == NONE {
                        s[1] = u;
                    } else {
                        junction[r[i] as usize] = true;
                    }
                }
            }
            junction[r[0] as usize] = true;
        }
        for (j, s) in junction.iter_mut().zip(&nb) {
            *j |= s[1] == NONE;
        }
        let mut arcs: Vec<Vec<u32>> = Vec::new();
        let mut arc_of: HashMap<(u32, u32), u32> = HashMap::new();
        let mut refs = Vec::with_capacity(rings.len());
        for r in &rings {
            if r.is_empty() {
                refs.push(Vec::new());
                continue;
            }
            if !clean(r) {
                arcs.push(r.clone());
                refs.push(vec![((arcs.len() - 1) as u32, false)]);
                continue;
            }
            let mut ring_refs = Vec::new();
            let mut i = 0;
            for j in 1..r.len() {
                if !junction[r[j] as usize] {
                    continue;
                }
                let seg = &r[i..=j];
                let fwd = (seg[0], seg[1]);
                let rev = (seg[seg.len() - 1], seg[seg.len() - 2]);
                let reversed = rev < fwd;
                let arc = *arc_of.entry(fwd.min(rev)).or_insert_with(|| {
                    arcs.push(if reversed { seg.iter().rev().copied().collect() } else { seg.to_vec() });
                    (arcs.len() - 1) as u32
                });
                let stored = &arcs[arc as usize];
                assert!(
                    if reversed { stored.iter().eq(seg.iter().rev()) } else { stored.as_slice() == seg },
                    "an arc's first step fixes the arc"
                );
                ring_refs.push((arc, reversed));
                i = j;
            }
            refs.push(ring_refs);
        }
        StateArcs { points, arcs, refs }
    }
}

/// Writes a pack. `states` in any order (written sorted by FIPS); `rows`
/// in PACK order, i.e. sorted by FIPS then each state's feature order.
pub fn encode(states: &[EncodeState], rows: &[PackRow], county_text: &str, cbsa_text: &str, tolerance_m: f32) -> Vec<u8> {
    let mut order: Vec<&EncodeState> = states.iter().collect();
    order.sort_by_key(|s| s.fips);
    let total: usize = order.iter().map(|s| s.features.len()).sum();
    assert_eq!(total, rows.len(), "one row per feature");
    let mut sections = Vec::new();
    let mut dir = Vec::new();
    let mut first = 0u32;
    let mut offset = (HEADER_LEN + DIR_ROW * order.len()) as u64;
    for s in &order {
        let mut raw = Vec::new();
        let mut bbox = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
        // The same box with western longitudes shifted by +360, for a state
        // across the antimeridian.
        let mut shifted = [f32::MAX, f32::MIN];
        for &[lon, lat] in s.features.iter().flatten().flatten().flatten() {
            bbox = [bbox[0].min(lon as f32), bbox[1].min(lat as f32), bbox[2].max(lon as f32), bbox[3].max(lat as f32)];
            let east = if lon < 0.0 { lon + 360.0 } else { lon } as f32;
            shifted = [shifted[0].min(east), shifted[1].max(east)];
        }
        let arcs = StateArcs::build(&s.features);
        put_varint(&mut raw, arcs.arcs.len() as u64);
        let (mut px, mut py) = (0i64, 0i64);
        for arc in &arcs.arcs {
            put_varint(&mut raw, arc.len() as u64);
            for &v in arc {
                let (x, y) = arcs.points[v as usize];
                put_varint(&mut raw, zigzag(x - px));
                put_varint(&mut raw, zigzag(y - py));
                (px, py) = (x, y);
            }
        }
        let mut refs = arcs.refs.iter();
        let mut prev = 0i64;
        for feature in &s.features {
            put_varint(&mut raw, feature.len() as u64);
            for polygon in feature {
                put_varint(&mut raw, polygon.len() as u64);
                for _ in polygon {
                    let ring = refs.next().expect("one reference list per ring");
                    put_varint(&mut raw, ring.len() as u64);
                    for &(arc, reversed) in ring {
                        put_varint(&mut raw, zigzag(i64::from(arc) - prev) << 1 | u64::from(reversed));
                        prev = i64::from(arc);
                    }
                }
            }
        }
        if shifted[1] - shifted[0] < bbox[2] - bbox[0] {
            (bbox[0], bbox[2]) = (shifted[0], shifted[1]);
        }
        let body = deflate(&raw);
        dir.push(DirRow {
            fips: s.fips,
            first_feature: first,
            feature_count: s.features.len() as u32,
            offset,
            len: body.len() as u32,
            raw_len: raw.len() as u32,
            bbox,
        });
        first += s.features.len() as u32;
        offset += body.len() as u64;
        sections.push(body);
    }
    let mut raw = Vec::with_capacity(rows.len() * 20 + county_text.len() + cbsa_text.len() + 20);
    for r in rows {
        raw.extend_from_slice(&r.geoid.to_le_bytes());
        raw.extend_from_slice(&r.income.unwrap_or(NONE).to_le_bytes());
        raw.extend_from_slice(&r.population.unwrap_or(NONE).to_le_bytes());
        raw.extend_from_slice(&r.aland_km2.to_le_bytes());
    }
    for text in [county_text, cbsa_text] {
        put_varint(&mut raw, text.len() as u64);
        raw.extend_from_slice(text.as_bytes());
    }
    let tables = deflate(&raw);
    let mut out = Vec::with_capacity(offset as usize + tables.len());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&QUANTUM_DEG.to_le_bytes());
    out.extend_from_slice(&tolerance_m.to_le_bytes());
    out.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    out.extend_from_slice(&(order.len() as u32).to_le_bytes());
    out.extend_from_slice(&offset.to_le_bytes());
    out.extend_from_slice(&(tables.len() as u32).to_le_bytes());
    out.extend_from_slice(&(raw.len() as u32).to_le_bytes());
    out.resize(HEADER_LEN, 0);
    for d in &dir {
        out.push(d.fips);
        out.extend_from_slice(&[0, 0, 0]);
        out.extend_from_slice(&d.first_feature.to_le_bytes());
        out.extend_from_slice(&d.feature_count.to_le_bytes());
        out.extend_from_slice(&d.offset.to_le_bytes());
        out.extend_from_slice(&d.len.to_le_bytes());
        out.extend_from_slice(&d.raw_len.to_le_bytes());
        for v in d.bbox {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
    for s in sections {
        out.extend_from_slice(&s);
    }
    out.extend_from_slice(&tables);
    out
}

/// From the first `HEADER_LEN` bytes: how many bytes the header plus
/// directory take (the page's second `Range` request).
pub fn header_len(first: &[u8]) -> Result<usize, PackError> {
    let mut c = Cursor { bytes: first, at: 0 };
    if c.take(4, "magic")? != MAGIC {
        return Err(PackError::Magic);
    }
    let version = c.u32("version")?;
    if version != VERSION {
        return Err(PackError::Version(version));
    }
    c.at = 28;
    let states = c.u32("state count")?;
    if states > 128 {
        return Err(PackError::Count("state count"));
    }
    Ok(HEADER_LEN + DIR_ROW * states as usize)
}

pub fn parse_header(bytes: &[u8]) -> Result<PackHeader, PackError> {
    let need = header_len(bytes.get(..HEADER_LEN).ok_or(PackError::Truncated("header"))?)?;
    let mut c = Cursor { bytes: bytes.get(..need).ok_or(PackError::Truncated("directory"))?, at: 8 };
    // v1 fixes both: a file claiming another quantum would otherwise decode
    // silently at the wrong scale.
    if c.u32("reserved")? != 0 {
        return Err(PackError::Header("reserved word is not zero"));
    }
    if c.u64("quantum")? != QUANTUM_DEG.to_bits() {
        return Err(PackError::Header("quantum is not 1e-6 degrees"));
    }
    let tolerance_m = c.f32("tolerance")?;
    let feature_count = c.u32("feature count")?;
    let state_count = c.u32("state count")?;
    let tables_offset = c.u64("tables offset")?;
    let tables_len = c.u32("tables len")?;
    let tables_raw_len = c.u32("tables raw len")?;
    c.at = HEADER_LEN;
    let mut states = Vec::with_capacity(state_count as usize);
    for _ in 0..state_count {
        let fips = c.take(4, "dir")?[0];
        let first_feature = c.u32("dir")?;
        let feature_count = c.u32("dir")?;
        let offset = c.u64("dir")?;
        let len = c.u32("dir")?;
        let raw_len = c.u32("dir")?;
        let bbox = [c.f32("dir")?, c.f32("dir")?, c.f32("dir")?, c.f32("dir")?];
        states.push(DirRow { fips, first_feature, feature_count, offset, len, raw_len, bbox });
    }
    Ok(PackHeader { tolerance_m, feature_count, states, tables_offset, tables_len, tables_raw_len })
}

pub fn decode_state(row: &DirRow, body: &[u8]) -> Result<StateGeometry, PackError> {
    let raw = inflate(body, row.raw_len, "state section")?;
    let mut c = Cursor { bytes: &raw, at: 0 };
    let mut g = StateGeometry::default();
    // Every count is bounded by what the remaining bytes could hold: the
    // arc and point counts before anything is reserved for them; the
    // polygon, ring and reference counts (which reserve nothing: their loops
    // stop at the bytes anyway) for an early, named failure. Each guard is
    // one line, named by its error. Memory: see `MAX_SECTION_RAW`.
    let left = |c: &Cursor<'_>| (c.bytes.len() - c.at) as u64;
    let arc_count = c.varint("arc count")?;
    // An arc is at least 3 bytes: its point count and one point's 2 varints.
    if arc_count > left(&c) / 3 { return Err(PackError::Count("arc count")); } // guard: arc count
    // A first pass checks every arc's point count, so `stored` is reserved
    // once, exactly, rather than grown.
    let arcs_at = c.at;
    let mut total = 0u64;
    for _ in 0..arc_count {
        let n = c.varint("arc point count")?;
        // A point is two varints, so at least two bytes; an arc has one.
        if n == 0 || n > left(&c) / 2 { return Err(PackError::Count("arc point count")); } // guard: arc point count
        for _ in 0..2 * n {
            c.varint("arc point")?;
        }
        total += n;
    }
    c.at = arcs_at;
    let mut arcs: Vec<(u32, u32)> = Vec::with_capacity(arc_count as usize);
    let mut stored: Vec<[f64; 2]> = Vec::with_capacity(total as usize);
    let (mut x, mut y) = (0i64, 0i64);
    for _ in 0..arc_count {
        let n = c.varint("arc point count")?;
        let a = stored.len() as u32;
        for _ in 0..n {
            x = x.wrapping_add(unzigzag(c.varint("dlon")?));
            y = y.wrapping_add(unzigzag(c.varint("dlat")?));
            let (lon, lat) = (x as f64 * QUANTUM_DEG, y as f64 * QUANTUM_DEG);
            if !(-180.0..=180.0).contains(&lon) || !(-90.0..=90.0).contains(&lat) {
                return Err(PackError::Coordinate { lon, lat });
            }
            stored.push([lon, lat]);
        }
        arcs.push((a, stored.len() as u32));
    }
    g.stored_points = stored.len();
    // In a planar partition an arc borders at most two rings (every arc of
    // the shipped pack is used once or twice), so a third use is refused,
    // and the assembled points are at most twice the stored ones (1.954x
    // measured): reserved once, never grown.
    let mut uses: Vec<u8> = vec![0; arcs.len()];
    let assembled_cap = 2 * stored.len();
    g.coords = Vec::with_capacity(assembled_cap);
    let mut prev = 0i64;
    for _ in 0..row.feature_count {
        let polys = c.varint("polygon count")?;
        if polys > left(&c) { return Err(PackError::Count("polygon count")); } // guard: polygon count
        let p0 = g.polygons.len() as u32;
        for _ in 0..polys {
            let rings = c.varint("ring count")?;
            if rings > left(&c) { return Err(PackError::Count("ring count")); } // guard: ring count
            let r0 = g.rings.len() as u32;
            for _ in 0..rings {
                let refs = c.varint("arc ref count")?;
                if refs > left(&c) { return Err(PackError::Count("arc ref count")); } // guard: arc ref count
                let c0 = g.coords.len();
                for _ in 0..refs {
                    let v = c.varint("arc ref")?;
                    let index = prev + unzigzag(v >> 1);
                    if index < 0 || index as u64 >= arc_count { return Err(PackError::Count("arc ref")); } // guard: arc ref
                    prev = index;
                    uses[index as usize] += 1;
                    if uses[index as usize] > 2 { return Err(PackError::Count("arc used a third time")); } // guard: arc third use
                    let (a, b) = arcs[index as usize];
                    let arc = &stored[a as usize..b as usize];
                    let reversed = v & 1 == 1;
                    let first = if reversed { arc[arc.len() - 1] } else { arc[0] };
                    let skip = usize::from(g.coords.len() > c0);
                    if skip == 1 && g.coords.last() != Some(&first) { return Err(PackError::Count("ring arcs do not join")); } // guard: ring arcs do not join
                    // Implied by the use rule; kept so the reservation above can never be outgrown.
                    if g.coords.len() + arc.len() - skip > assembled_cap { return Err(PackError::Count("assembled points")); } // guard: assembled points
                    if reversed {
                        g.coords.extend(arc.iter().rev().skip(skip));
                    } else {
                        g.coords.extend(arc.iter().skip(skip));
                    }
                }
                g.rings.push((c0 as u32, g.coords.len() as u32));
            }
            g.polygons.push((r0, g.rings.len() as u32));
        }
        g.features.push((p0, g.polygons.len() as u32));
    }
    if c.at != raw.len() {
        return Err(PackError::Count("trailing bytes in a section"));
    }
    Ok(g)
}

pub fn decode_tables(header: &PackHeader, body: &[u8]) -> Result<Tables, PackError> {
    // 20 bytes a row: a corrupt feature count cannot reserve more than the
    // (capped) raw table could hold.
    if u64::from(header.feature_count) * 20 > u64::from(header.tables_raw_len) {
        return Err(PackError::Count("feature count"));
    }
    let raw = inflate(body, header.tables_raw_len, "tables")?;
    let mut c = Cursor { bytes: &raw, at: 0 };
    let opt = |v: u32| (v != NONE).then_some(v);
    let mut rows = Vec::with_capacity(header.feature_count as usize);
    for _ in 0..header.feature_count {
        let geoid = c.u64("row")?;
        let income = opt(c.u32("row")?);
        let population = opt(c.u32("row")?);
        let aland_km2 = c.f32("row")?;
        rows.push(PackRow { geoid, income, population, aland_km2 });
    }
    let mut text = |what| -> Result<String, PackError> {
        let n = usize::try_from(c.varint(what)?).map_err(|_| PackError::Count(what))?;
        String::from_utf8(c.take(n, what)?.to_vec()).map_err(|_| PackError::Count(what))
    };
    let county_text = text("county names")?;
    let cbsa_text = text("cbsa table")?;
    if c.at != raw.len() {
        return Err(PackError::Count("trailing bytes in the tables"));
    }
    Ok(Tables { rows, county_text, cbsa_text })
}

/// The order a page fetches the states in, as directory indices: the
/// states whose bbox meets `view` (`[west, south, east, north]`) first,
/// then the rest; larger `feature_count` first within each group, so the
/// view fills fastest where it is densest.
pub fn load_order(states: &[DirRow], view: [f64; 4]) -> Vec<usize> {
    // A box written past 180 (across the antimeridian) is also tried one
    // world west, where the view's -180..180 longitudes meet it.
    let meets = |s: &DirRow| {
        let b = s.bbox.map(f64::from);
        let lat = b[1] <= view[3] && b[3] >= view[1];
        let lon = |w: f64, e: f64| w <= view[2] && e >= view[0];
        lat && (lon(b[0], b[2]) || (b[2] > 180.0 && lon(b[0] - 360.0, b[2] - 360.0)))
    };
    let mut order: Vec<usize> = (0..states.len()).collect();
    order.sort_by_key(|&i| (!meets(&states[i]), std::cmp::Reverse(states[i].feature_count), i));
    order
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(lon: f64, lat: f64, d: f64) -> Vec<[f64; 2]> {
        vec![[lon, lat], [lon + d, lat], [lon + d, lat + d], [lon, lat + d], [lon, lat]]
    }

    fn two_state_pack() -> Vec<u8> {
        let ri = EncodeState {
            fips: 44,
            features: vec![
                vec![vec![square(-71.5, 41.8, 0.01)]],
                vec![vec![square(-71.49, 41.8, 0.01), square(-71.487, 41.803, 0.002)]],
            ],
        };
        let dc = EncodeState { fips: 11, features: vec![vec![vec![square(-77.03, 38.9, 0.005)]]] };
        let rows = vec![
            PackRow { geoid: 110010001011, income: Some(250_001), population: Some(1_200), aland_km2: 0.2 },
            PackRow { geoid: 440010301001, income: None, population: Some(0), aland_km2: 1.5 },
            PackRow { geoid: 440010301002, income: Some(0), population: None, aland_km2: 0.9 },
        ];
        encode(&[dc, ri], &rows, "44001\tBristol County\n", "44001\t39300\tProvidence-Warwick, RI-MA\n", 5.0)
    }

    #[test]
    fn a_pack_round_trips_geometry_to_the_quantum() {
        let bytes = two_state_pack();
        let header = parse_header(&bytes).expect("header");
        assert_eq!(header.feature_count, 3);
        assert_eq!(header.states.iter().map(|s| s.fips).collect::<Vec<_>>(), vec![11, 44], "sorted by FIPS");
        let ri = header.states.iter().find(|s| s.fips == 44).unwrap();
        assert_eq!((ri.first_feature, ri.feature_count), (1, 2));
        let body = &bytes[ri.offset as usize..(ri.offset + u64::from(ri.len)) as usize];
        let geo = decode_state(ri, body).expect("section");
        assert_eq!(geo.features.len(), 2);
        assert_eq!(geo.rings.len(), 3, "one ring, then an outer ring and its hole");
        let (a, b) = geo.rings[1];
        let first = geo.coords[a as usize];
        assert!((first[0] - -71.49).abs() <= QUANTUM_DEG / 2.0 + 1e-12);
        assert!((first[1] - 41.8).abs() <= QUANTUM_DEG / 2.0 + 1e-12);
        assert_eq!(b - a, 5);
    }

    #[test]
    fn pack_keeps_none_distinct_from_zero() {
        let bytes = two_state_pack();
        let header = parse_header(&bytes).unwrap();
        let t = &bytes[header.tables_offset as usize..(header.tables_offset + u64::from(header.tables_len)) as usize];
        let tables = decode_tables(&header, t).expect("tables");
        assert_eq!(tables.rows[1].income, None);
        assert_eq!(tables.rows[2].income, Some(0));
        assert_eq!(tables.rows[1].population, Some(0));
        assert_eq!(tables.rows[2].population, None);
        assert_eq!(tables.rows[0].income, Some(250_001));
        assert_eq!(tables.county_text, "44001\tBristol County\n");
        assert!(tables.cbsa_text.starts_with("44001\t39300\t"));
    }

    #[test]
    fn a_truncated_pack_is_an_error_not_a_panic() {
        let bytes = two_state_pack();
        for cut in [0, 3, 10, HEADER_LEN - 1, HEADER_LEN + 5] {
            assert!(parse_header(&bytes[..cut]).is_err(), "cut at {cut}");
        }
        let header = parse_header(&bytes).unwrap();
        let s = &header.states[0];
        let body = &bytes[s.offset as usize..(s.offset + u64::from(s.len)) as usize];
        for cut in [0, 1, body.len() / 2, body.len() - 1] {
            assert!(decode_state(s, &body[..cut]).is_err(), "section cut at {cut}");
        }
    }

    #[test]
    fn a_foreign_file_is_refused_at_the_magic() {
        let mut bytes = two_state_pack();
        bytes[0..4].copy_from_slice(b"PMTi");
        assert!(matches!(parse_header(&bytes), Err(PackError::Magic)));
        let mut bytes = two_state_pack();
        bytes[4..8].copy_from_slice(&3u32.to_le_bytes());
        assert!(matches!(parse_header(&bytes), Err(PackError::Version(3))));
    }

    /// A v1 pack (every ring's points in full) is refused by its version,
    /// before the directory is fetched, with a message naming the rebuild.
    #[test]
    fn a_v1_pack_is_refused_with_a_rebuild_message() {
        let mut bytes = two_state_pack();
        bytes[4..8].copy_from_slice(&1u32.to_le_bytes());
        let err = header_len(&bytes[..HEADER_LEN]).unwrap_err();
        assert_eq!(err, PackError::Version(1));
        let text = err.to_string();
        assert!(text.contains("v1") && text.contains("rebuild the pack with tools/census-pack"), "{text}");
        assert_eq!(parse_header(&bytes), Err(PackError::Version(1)));
    }

    /// Every ring comes back point for point (to the quantum): shared
    /// edges, a hole, and rings the arc cutter must store privately (open,
    /// repeated points, too short, empty).
    #[test]
    fn every_ring_decodes_exactly_and_shared_edges_are_stored_once() {
        let open = vec![[-71.40, 41.80], [-71.39, 41.80], [-71.39, 41.81]];
        let repeated = vec![[-71.30, 41.80], [-71.29, 41.80], [-71.29, 41.80], [-71.29, 41.81], [-71.30, 41.80]];
        let short = vec![[-71.20, 41.80], [-71.19, 41.80], [-71.20, 41.80]];
        // Three squares in a row and one below the first two: T-junctions.
        let features: Vec<Feature> = vec![
            vec![vec![square(-71.5, 41.8, 0.01), square(-71.497, 41.803, 0.002)]],
            vec![vec![square(-71.49, 41.8, 0.01)]],
            vec![vec![square(-71.48, 41.8, 0.01)], vec![square(-71.497, 41.803, 0.002)]],
            vec![vec![vec![[-71.5, 41.8], [-71.5, 41.79], [-71.48, 41.79], [-71.48, 41.8], [-71.49, 41.8], [-71.5, 41.8]]]],
            vec![vec![open, repeated, short, Vec::new()]],
        ];
        let rows: Vec<PackRow> = (0..features.len() as u64)
            .map(|i| PackRow { geoid: i, income: None, population: None, aland_km2: 0.0 })
            .collect();
        let bytes = encode(&[EncodeState { fips: 44, features: features.clone() }], &rows, "", "", 5.0);
        let header = parse_header(&bytes).unwrap();
        let row = &header.states[0];
        let geo = decode_state(row, &bytes[row.offset as usize..(row.offset + u64::from(row.len)) as usize]).unwrap();
        let mut k = 0;
        for (f, &(p0, p1)) in features.iter().zip(&geo.features) {
            assert_eq!(f.len(), (p1 - p0) as usize);
            for (polygon, &(r0, r1)) in f.iter().zip(&geo.polygons[p0 as usize..p1 as usize]) {
                assert_eq!(polygon.len(), (r1 - r0) as usize);
                for (ring, &(a, b)) in polygon.iter().zip(&geo.rings[r0 as usize..r1 as usize]) {
                    let got: Vec<(i64, i64)> = geo.coords[a as usize..b as usize].iter().map(|p| (q(p[0]), q(p[1]))).collect();
                    let want: Vec<(i64, i64)> = ring.iter().map(|p| (q(p[0]), q(p[1]))).collect();
                    assert_eq!(got, want, "ring {k}");
                    k += 1;
                }
            }
        }
        assert_eq!(k, 10);
        // 12 arcs: A|B's side, B|C's side, the bottoms of A and B, and the
        // hole with its enclave are each stored once and used by two rings;
        // the four odd rings are private arcs.
        let arcs = StateArcs::build(&features);
        assert_eq!(arcs.arcs.len(), 12);
        let mut uses = vec![0; arcs.arcs.len()];
        arcs.refs.iter().flatten().for_each(|&(a, _)| uses[a as usize] += 1);
        assert_eq!(uses.iter().filter(|&&u| u == 2).count(), 5, "{uses:?}");
        assert_eq!((geo.stored_points, geo.coords.len()), (37, 42));
    }

    #[test]
    fn a_section_claiming_more_than_the_cap_is_refused() {
        let bytes = two_state_pack();
        let header = parse_header(&bytes).unwrap();
        let mut row = header.states[0].clone();
        row.raw_len = MAX_SECTION_RAW + 1;
        let body = &bytes[row.offset as usize..(row.offset + u64::from(row.len)) as usize];
        assert!(matches!(decode_state(&row, body), Err(PackError::TooLarge { .. })));
    }

    #[test]
    fn header_len_says_how_much_to_fetch_for_the_directory() {
        let bytes = two_state_pack();
        assert_eq!(header_len(&bytes[..HEADER_LEN]).unwrap(), HEADER_LEN + 2 * DIR_ROW);
    }

    #[test]
    fn the_view_states_load_first() {
        let bytes = two_state_pack();
        let header = parse_header(&bytes).unwrap();
        let ri = header.states.iter().position(|s| s.fips == 44).unwrap();
        // A view around Providence: RI's bbox meets it, DC's does not, and
        // DC's single feature would not outrank RI's two anyway -- so check
        // the other way too, with a view on DC.
        assert_eq!(load_order(&header.states, [-72.0, 41.5, -71.0, 42.0])[0], ri);
        let dc = header.states.iter().position(|s| s.fips == 11).unwrap();
        assert_eq!(load_order(&header.states, [-77.2, 38.8, -76.9, 39.0]), vec![dc, ri]);
        assert_eq!(load_order(&header.states, [0.0, 0.0, 1.0, 1.0]), vec![ri, dc], "none in view: largest first");
    }

    /// A hand-built section: `raw` deflated, claiming its own length.
    fn section(raw: &[u8], feature_count: u32) -> (DirRow, Vec<u8>) {
        let body = deflate(raw);
        let row = DirRow {
            fips: 44,
            first_feature: 0,
            feature_count,
            offset: 0,
            len: body.len() as u32,
            raw_len: raw.len() as u32,
            bbox: [0.0; 4],
        };
        (row, body)
    }

    fn varints(values: &[u64]) -> Vec<u8> {
        let mut out = Vec::new();
        for &v in values {
            put_varint(&mut out, v);
        }
        out
    }

    #[test]
    fn a_count_past_the_bytes_left_is_refused_at_its_bound() {
        // A billion arcs; one arc of a billion points (and one of none); then,
        // with no arcs, a feature claiming a billion polygons, a polygon a
        // billion rings, a ring a billion arc references. Each is refused by
        // name at its own bound, before anything is reserved for it.
        for (counts, what) in [
            (vec![1_000_000_000], "arc count"),
            (vec![1, 1_000_000_000], "arc point count"),
            (vec![1, 0], "arc point count"),
            (vec![0, 1_000_000_000], "polygon count"),
            (vec![0, 1, 1_000_000_000], "ring count"),
            (vec![0, 1, 1, 1_000_000_000], "arc ref count"),
        ] {
            let mut raw = varints(&counts);
            raw.extend_from_slice(&[0; 16]);
            let (row, body) = section(&raw, 1);
            assert_eq!(decode_state(&row, &body).unwrap_err(), PackError::Count(what));
        }
    }

    #[test]
    fn a_feature_count_the_tables_cannot_hold_is_refused_before_reserving() {
        let bytes = two_state_pack();
        let mut header = parse_header(&bytes).unwrap();
        let t = &bytes[header.tables_offset as usize..(header.tables_offset + u64::from(header.tables_len)) as usize];
        header.feature_count = u32::MAX;
        assert_eq!(decode_tables(&header, t).err(), Some(PackError::Count("feature count")));
    }

    #[test]
    fn a_stream_longer_than_it_claims_is_refused() {
        let raw = varints(&[1, 1, 5, 2, 2, 2, 0, 0, 1, 1, 0, 0, 1, 1]);
        let (mut row, body) = section(&raw, 1);
        row.raw_len -= 3;
        assert_eq!(decode_state(&row, &body).unwrap_err(), PackError::Count("state section"));
    }

    #[test]
    fn a_claim_past_deflates_ratio_is_refused_before_reserving() {
        let (mut row, body) = section(&varints(&[1, 1, 4]), 1);
        row.raw_len = (body.len() as u32) * 1032 + 65;
        assert!(row.raw_len < MAX_SECTION_RAW, "under the absolute cap: the ratio is what refuses it");
        assert!(matches!(decode_state(&row, &body), Err(PackError::TooLarge { .. })));
    }

    /// A reference: `zigzag(index - previous) << 1 | reversed`.
    fn arc_ref(delta: i64, reversed: bool) -> u64 {
        zigzag(delta) << 1 | u64::from(reversed)
    }

    #[test]
    fn an_arc_reference_past_the_arcs_is_refused() {
        // One arc of one point; a ring referring to arc 1, then to arc -1.
        for delta in [1, -1] {
            let raw = varints(&[1, 1, 0, 0, 1, 1, 1, arc_ref(delta, false)]);
            let (row, body) = section(&raw, 1);
            assert_eq!(decode_state(&row, &body).unwrap_err(), PackError::Count("arc ref"));
        }
    }

    #[test]
    fn arcs_that_do_not_join_are_refused() {
        // Arc 0: (0,0) -> (1,0). Arc 1: (5,5) -> (6,6). A ring of 0 then 1.
        let raw = varints(&[2, 2, 0, 0, 2, 0, 2, 8, 8, 2, 2, 1, 1, 2, arc_ref(0, false), arc_ref(1, false)]);
        let (row, body) = section(&raw, 1);
        assert_eq!(decode_state(&row, &body).unwrap_err(), PackError::Count("ring arcs do not join"));
    }

    /// One 50-point arc, then a ring running it forward and back 20 times:
    /// the third use is refused. (The `assembled points` cap is implied by
    /// this rule, at most two uses of each stored point, so no input
    /// reaches it alone; it guards the exact reservation if the rule ever
    /// changes, and with this guard removed the memory test sees it fire.)
    #[test]
    fn an_arc_used_a_third_time_is_refused() {
        let mut raw = varints(&[1, 50, 0, 0]);
        raw.extend(varints(&[zigzag(1), 0]).repeat(49));
        raw.extend(varints(&[1, 1, 20]));
        raw.extend((0..20).map(|i| arc_ref(0, i % 2 == 1) as u8));
        let (row, body) = section(&raw, 1);
        assert_eq!(decode_state(&row, &body).unwrap_err(), PackError::Count("arc used a third time"));
    }

    /// The review's crafted section (I1): one 1,000-point arc, then a ring
    /// whose one-byte references run it forward and back a million times.
    /// 293f37c let the ring assemble up to the raw length in points (16 B
    /// each, doubled by `Vec` growth): ~1.2 GB from a 64 KB body at 64 MiB
    /// raw. Honest data uses an arc at most twice, so the third use is
    /// refused, within a few MB of the inflated section itself.
    #[test]
    fn a_ring_running_one_arc_back_and_forth_is_refused_in_small_memory() {
        let mut raw = varints(&[1, 1000, 0, 0]);
        raw.extend(varints(&[zigzag(1), 0]).repeat(999));
        raw.extend(varints(&[1, 1, 1_000_000]));
        raw.extend((0..1_000_000).map(|i| arc_ref(0, i % 2 == 1) as u8));
        let (row, body) = section(&raw, 1);
        let base = crate::collision::alloc_counting::reset_peak();
        let result = decode_state(&row, &body);
        let peak = crate::collision::alloc_counting::peak_bytes() - base;
        // The inflated section is ~1 MB (plus its spare byte); everything the
        // decoder builds from it must stay small beside that.
        assert!(peak < 3 << 20, "peak {peak} B decoding a {} B section", raw.len());
        assert_eq!(result.unwrap_err(), PackError::Count("arc used a third time"));
    }

    /// An arc takes at least 3 bytes (a count and one point), so two arcs
    /// cannot fit in the 3 left: refused at the count, not after the first.
    #[test]
    fn more_arcs_than_three_bytes_each_is_refused() {
        let (row, body) = section(&varints(&[2, 1, 0, 0]), 1);
        assert_eq!(decode_state(&row, &body).unwrap_err(), PackError::Count("arc count"));
    }

    #[test]
    fn an_overlong_varint_is_refused() {
        // An arc count of 0 written in ten bytes, the last carrying a bit
        // past 63: read leniently it is 0 arcs of an empty state.
        let mut raw = vec![0x80; 9];
        raw.push(0x02);
        let (row, body) = section(&raw, 0);
        assert_eq!(decode_state(&row, &body).unwrap_err(), PackError::Count("arc count"));
    }

    #[test]
    fn a_coordinate_off_the_planet_is_refused() {
        // One arc of 4 points whose first delta jumps to lon 200.
        let raw = varints(&[1, 4, zigzag(200_000_000), 0, 0, 0, 0, 0, 0, 0]);
        let (row, body) = section(&raw, 1);
        assert!(matches!(decode_state(&row, &body), Err(PackError::Coordinate { .. })));
    }

    #[test]
    fn a_header_with_another_quantum_or_reserved_word_is_refused() {
        let mut bytes = two_state_pack();
        bytes[12..20].copy_from_slice(&1e-7f64.to_le_bytes());
        assert_eq!(parse_header(&bytes), Err(PackError::Header("quantum is not 1e-6 degrees")));
        let mut bytes = two_state_pack();
        bytes[8] = 1;
        assert_eq!(parse_header(&bytes), Err(PackError::Header("reserved word is not zero")));
    }

    #[test]
    fn trailing_bytes_after_the_tables_are_refused() {
        let bytes = two_state_pack();
        let mut header = parse_header(&bytes).unwrap();
        let t = &bytes[header.tables_offset as usize..(header.tables_offset + u64::from(header.tables_len)) as usize];
        let mut raw = inflate(t, header.tables_raw_len, "tables").unwrap();
        raw.push(7);
        let body = deflate(&raw);
        header.tables_raw_len = raw.len() as u32;
        assert_eq!(decode_tables(&header, &body).err(), Some(PackError::Count("trailing bytes in the tables")));
    }

    /// Every single-byte corruption of a whole pack: each decode step is an
    /// `Ok` or an `Err`, never a panic.
    #[test]
    fn no_single_byte_corruption_panics() {
        let pristine = two_state_pack();
        for at in 0..pristine.len() {
            for flip in [0x01u8, 0x80, 0xff] {
                let mut bytes = pristine.clone();
                bytes[at] ^= flip;
                let Ok(header) = parse_header(&bytes) else { continue };
                for row in &header.states {
                    let end = row.offset.checked_add(u64::from(row.len));
                    if let Some(body) = end.and_then(|e| bytes.get(row.offset as usize..e as usize)) {
                        let _ = decode_state(row, body);
                    }
                }
                let end = header.tables_offset.checked_add(u64::from(header.tables_len));
                if let Some(body) = end.and_then(|e| bytes.get(header.tables_offset as usize..e as usize)) {
                    let _ = decode_tables(&header, body);
                }
            }
        }
    }

    #[test]
    fn a_state_across_the_antimeridian_keeps_a_narrow_box_and_loads_by_it() {
        // Attu (172.9 E) to the panhandle (130 W).
        let ak = EncodeState {
            fips: 2,
            features: vec![vec![vec![vec![[172.9, 52.8], [179.9, 52.8], [179.9, 53.0], [172.9, 52.8]]]], vec![vec![
                vec![[-179.9, 51.0], [-130.0, 55.0], [-150.0, 70.0], [-179.9, 51.0]],
            ]]],
        };
        let ks = EncodeState { fips: 20, features: vec![vec![vec![square(-99.0, 38.0, 0.5)]]] };
        let rows: Vec<PackRow> = (0..3)
            .map(|i| PackRow { geoid: i, income: None, population: None, aland_km2: 0.0 })
            .collect();
        let bytes = encode(&[ak, ks], &rows, "", "", 5.0);
        let header = parse_header(&bytes).unwrap();
        let [w, _, e, _] = header.states[0].bbox;
        assert!((w - 172.9).abs() < 1e-3 && (e - 230.0).abs() < 1e-3, "{w} {e}");
        // Kansas in view: Alaska (more features) no longer jumps the queue.
        assert_eq!(load_order(&header.states, [-102.0, 37.0, -95.0, 40.0]), vec![1, 0]);
        // Anchorage in view: it does, from either side of the line.
        assert_eq!(load_order(&header.states, [-151.0, 60.0, -148.0, 62.0])[0], 0);
        assert_eq!(load_order(&header.states, [175.0, 52.0, 179.0, 53.0])[0], 0);
    }
}

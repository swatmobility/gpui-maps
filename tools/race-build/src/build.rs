//! `race-build build`: the national converter (Task 5). Streams the TIGER
//! 2020 block polygons one state at a time, joins them to the P.L. 94-171
//! counts, turns every populated block into parts, spills the parts and the
//! block points, and finally builds the 3″ categorical pyramid and the z12
//! part tiles into `race-dots-2020.pmtiles`.
//!
//! Disk (ledger ruling for Task 5): before each state, free space on the
//! cache volume must be at least zip + unpacked + 10 GiB, or the build stops
//! and says which states are done. Nothing is unpacked to disk (the zip
//! members stream), so the unpacked term is headroom. Each zip is deleted
//! once its state's checks and inside-check (C9) have passed. Progress is
//! checkpointed after every state, so rerunning the same command resumes.

use std::collections::{BTreeMap, BinaryHeap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use gpui_maps::geo::TileId;
use gpui_maps::overlay::dotgrid::hash::dot_hashes;
use gpui_maps::overlay::dotgrid::parts::{allocate, part_key, sample_in_triangle, split_part, Part, Tri, WORLD_QUANTA};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::pl94::{self, BlockCounts};
use crate::pyramid::{self, PyramidStats, BANDS};
use crate::spill::{self, BlockPoint, SpillPart, FLAG_FALLBACK};
use crate::tiger;

/// People cap per part: the composition texel's u16 denominator (§3.3).
pub const MAX_PART_PEOPLE: u32 = 65_535;
/// Extent cap per part: a quarter of a z12 tile (2^19 quanta).
pub const MAX_PART_EXTENT: i64 = (1 << 19) / 4;
/// The fallback triangle's side, metres.
const FALLBACK_SIDE_M: f64 = 10.0;
/// Douglas–Peucker tolerance, metres.
pub const SIMPLIFY_M: f64 = 1.0;
/// 10 GiB, the per-state headroom.
const HEADROOM: u64 = 10 << 30;

/// The 50 states + DC: USPS abbreviation and FIPS code.
pub const STATES: [(&str, u8); 51] = [
    ("al", 1), ("ak", 2), ("az", 4), ("ar", 5), ("ca", 6), ("co", 8), ("ct", 9), ("de", 10), ("dc", 11), ("fl", 12),
    ("ga", 13), ("hi", 15), ("id", 16), ("il", 17), ("in", 18), ("ia", 19), ("ks", 20), ("ky", 21), ("la", 22),
    ("me", 23), ("md", 24), ("ma", 25), ("mi", 26), ("mn", 27), ("ms", 28), ("mo", 29), ("mt", 30), ("ne", 31),
    ("nv", 32), ("nh", 33), ("nj", 34), ("nm", 35), ("ny", 36), ("nc", 37), ("nd", 38), ("oh", 39), ("ok", 40),
    ("or", 41), ("pa", 42), ("ri", 44), ("sc", 45), ("sd", 46), ("tn", 47), ("tx", 48), ("ut", 49), ("vt", 50),
    ("va", 51), ("wa", 53), ("wv", 54), ("wi", 55), ("wy", 56),
];

/// MG-R7's named state totals.
pub const STATE_CONSTANTS: [(&str, u64); 4] = [("ca", 39_538_223), ("tx", 29_145_505), ("dc", 689_545), ("wy", 576_851)];

// ---------------------------------------------------------------------
// Join and per-block geometry.

/// One TIGER block: its GEOID and rings as `[lon, lat]`.
#[cfg_attr(not(test), allow(dead_code))] // tests, and the readers of later tasks
#[derive(Debug, Clone, PartialEq)]
pub struct TigerBlock {
    pub geoid: u64,
    pub rings: Vec<Vec<[f64; 2]>>,
}

/// P.L. blocks by GEOID, and which populated ones TIGER has supplied.
pub struct Joiner {
    index: HashMap<u64, usize>,
    seen: Vec<bool>,
    /// TIGER GEOIDs with no P.L. block (reported, not fatal).
    pub not_in_pl: u64,
}

fn populated(b: &BlockCounts) -> bool {
    b.c.iter().any(|&n| n > 0)
}

impl Joiner {
    pub fn new(pl: &[BlockCounts]) -> Self {
        Self { index: pl.iter().enumerate().map(|(i, b)| (b.geoid, i)).collect(), seen: vec![false; pl.len()], not_in_pl: 0 }
    }

    /// The P.L. index of TIGER block `geoid` when it is populated.
    pub fn take(&mut self, pl: &[BlockCounts], geoid: u64) -> Result<Option<usize>> {
        let Some(&i) = self.index.get(&geoid) else {
            self.not_in_pl += 1;
            return Ok(None);
        };
        if std::mem::replace(&mut self.seen[i], true) {
            bail!("block {geoid:015} appears twice in TIGER");
        }
        Ok(populated(&pl[i]).then_some(i))
    }

    /// Fails naming every populated P.L. block TIGER never supplied (the
    /// first 20, and the count).
    pub fn finish(&self, pl: &[BlockCounts]) -> Result<()> {
        let missing: Vec<u64> = pl.iter().zip(&self.seen).filter(|(b, &s)| !s && populated(b)).map(|(b, _)| b.geoid).collect();
        if !missing.is_empty() {
            let names: Vec<String> = missing.iter().take(20).map(|g| format!("{g:015}")).collect();
            bail!("{} populated P.L. block(s) have no TIGER polygon: {}", missing.len(), names.join(", "));
        }
        Ok(())
    }
}

/// Pairs every populated P.L. block with its TIGER polygon; an error names
/// any populated block TIGER lacks.
#[cfg_attr(not(test), allow(dead_code))] // tests, and the readers of later tasks
pub fn join_state<'a>(pl: &'a [BlockCounts], tiger: &'a [TigerBlock]) -> Result<Vec<(&'a BlockCounts, &'a TigerBlock)>> {
    let mut j = Joiner::new(pl);
    let mut out = Vec::new();
    for t in tiger {
        if let Some(i) = j.take(pl, t.geoid)? {
            out.push((&pl[i], t));
        }
    }
    j.finish(pl)?;
    Ok(out)
}

/// Rings `[lon, lat]` to world-grid quanta, dropping repeated vertices the
/// rounding creates.
pub fn project_rings(rings: &[Vec<[f64; 2]>]) -> Vec<Vec<[i64; 2]>> {
    rings
        .iter()
        .map(|r| {
            let mut out: Vec<[i64; 2]> = Vec::with_capacity(r.len());
            for p in r {
                let q = tiger::project(p[0], p[1]);
                if out.last() != Some(&q) {
                    out.push(q);
                }
            }
            out
        })
        .collect()
}

/// A block's geometry through the converter.
#[derive(Debug, Clone)]
pub struct BlockGeom {
    /// The polygon on the world grid, before simplification.
    pub rings: Vec<Vec<[i64; 2]>>,
    pub simplified: Vec<Vec<[i64; 2]>>,
    pub triangles: usize,
    pub parts: Vec<Part>,
    pub fallback: bool,
}

/// A 10 m equilateral triangle centred on the interior point.
fn fallback_triangle(lat: f64, lon: f64) -> Tri {
    let c = tiger::project(lon, lat);
    let s = tiger::quanta_per_metre(lat) * FALLBACK_SIDE_M;
    let h = s * 3f64.sqrt() / 2.0;
    let p = |dx: f64, dy: f64| [c[0] + dx.round() as i64, c[1] + dy.round() as i64];
    Tri { a: p(-s / 2.0, h / 3.0), b: p(s / 2.0, h / 3.0), c: p(0.0, -2.0 * h / 3.0) }
}

/// Simplify, triangulate, allocate and split one populated block.
pub fn block_geometry(b: &BlockCounts, rings: Vec<Vec<[i64; 2]>>) -> BlockGeom {
    let (lat, lon) = b.intpt;
    let tol = (tiger::quanta_per_metre(lat) * SIMPLIFY_M).round().max(1.0) as i64;
    let simplified: Vec<Vec<[i64; 2]>> = rings.iter().map(|r| tiger::simplify(r, tol)).filter(|r| r.len() >= 3).collect();
    let mut tris = tiger::triangulate(&simplified);
    let fallback = tris.is_empty();
    if fallback {
        tris = vec![fallback_triangle(lat, lon)];
    }
    let triangles = tris.len();
    let max_extent = MAX_PART_EXTENT;
    // Parked T3 finding F4: split_part cannot subdivide below 1 quantum.
    debug_assert!(max_extent >= 1);
    let parts = allocate(&b.c, &tris, b.geoid).into_iter().flat_map(|p| split_part(p, MAX_PART_PEOPLE, max_extent)).collect();
    BlockGeom { rings, simplified, triangles, parts, fallback }
}

/// A block's parts from rings already on the world grid, and whether it
/// fell back to the interior-point triangle.
#[cfg_attr(not(test), allow(dead_code))] // tests, and the readers of later tasks
pub fn parts_for_block(b: &BlockCounts, rings: &[Vec<[i64; 2]>]) -> (Vec<Part>, bool) {
    let g = block_geometry(b, rings.to_vec());
    (g.parts, g.fallback)
}

// ---------------------------------------------------------------------
// MG-R8: dots inside their blocks.

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InsideStats {
    pub blocks: u64,
    pub dots: u64,
    pub in_simplified: u64,
    pub in_original: u64,
    /// Outside the original polygon but within `SIMPLIFY_M` of its boundary.
    pub near_original: u64,
    /// Largest distance outside the simplified polygon, quanta.
    pub max_out_simplified_q: f64,
    /// Largest distance outside the original polygon, metres.
    pub max_out_original_m: f64,
}

impl InsideStats {
    pub fn merge(&mut self, o: &InsideStats) {
        self.blocks += o.blocks;
        self.dots += o.dots;
        self.in_simplified += o.in_simplified;
        self.in_original += o.in_original;
        self.near_original += o.near_original;
        self.max_out_simplified_q = self.max_out_simplified_q.max(o.max_out_simplified_q);
        self.max_out_original_m = self.max_out_original_m.max(o.max_out_original_m);
    }
}

/// Samples `dots` dots over a block's parts, spread by people, with the
/// CPU mirror of the shader (`dot_hashes` → `sample_in_triangle`), and
/// tests each against the simplified and the original polygon.
///
/// The key here is `part_key` over the part's vertices in unit-world f32;
/// the shader keys on the tile's lyon units (Task 6), which only changes
/// which pseudo-random points are drawn, not the geometry being tested.
pub fn check_inside(g: &BlockGeom, lat: f64, dots: u32) -> InsideStats {
    let mut st = InsideStats { blocks: 1, ..Default::default() };
    let people: Vec<u64> = g.parts.iter().map(|p| p.c.iter().map(|&n| u64::from(n)).sum()).collect();
    let total: u64 = people.iter().sum();
    if total == 0 || dots == 0 {
        return st;
    }
    let mut cum = Vec::with_capacity(people.len());
    let mut acc = 0;
    for &p in &people {
        acc += p;
        cum.push(acc);
    }
    let qpm = tiger::quanta_per_metre(lat);
    let unit = |v: [i64; 2]| [(v[0] as f64 / WORLD_QUANTA as f64) as f32, (v[1] as f64 / WORLD_QUANTA as f64) as f32];
    for j in 0..dots {
        let target = u64::from(j) * total / u64::from(dots);
        let k = cum.partition_point(|&c| c <= target);
        let t = g.parts[k].tri;
        let a = [t.a[0] as f64, t.a[1] as f64];
        let e1 = [(t.b[0] - t.a[0]) as f64, (t.b[1] - t.a[1]) as f64];
        let e2 = [(t.c[0] - t.a[0]) as f64, (t.c[1] - t.a[1]) as f64];
        let (ua, ub, uc) = (unit(t.a), unit(t.b), unit(t.c));
        let key = part_key(ua, [ub[0] - ua[0], ub[1] - ua[1]], [uc[0] - ua[0], uc[1] - ua[1]]);
        let (_, u, v) = dot_hashes(key, j);
        let p = sample_in_triangle(a, e1, e2, u, v);
        st.dots += 1;
        if tiger::inside(p, &g.simplified) {
            st.in_simplified += 1;
        } else {
            st.max_out_simplified_q = st.max_out_simplified_q.max(tiger::boundary_distance(p, &g.simplified));
        }
        if tiger::inside(p, &g.rings) {
            st.in_original += 1;
        } else {
            let d = tiger::boundary_distance(p, &g.rings) / qpm;
            if d <= SIMPLIFY_M {
                st.near_original += 1;
            }
            st.max_out_original_m = st.max_out_original_m.max(d);
        }
    }
    st
}

fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// A sampled block for the inside-check, ordered by its hash so the heap
/// keeps the `quota` smallest.
struct Sample {
    hash: u64,
    lat: f64,
    geom: BlockGeom,
}

impl PartialEq for Sample {
    fn eq(&self, o: &Self) -> bool {
        self.hash == o.hash
    }
}
impl Eq for Sample {}
impl PartialOrd for Sample {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Sample {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        self.hash.cmp(&o.hash)
    }
}

// ---------------------------------------------------------------------
// One state.

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StateStats {
    pub abbr: String,
    pub fips: u8,
    pub zip_bytes: u64,
    pub zip_bytes_head: u64,
    pub unpacked_bytes: u64,
    pub free_before: u64,
    pub tiger_blocks: u64,
    pub tiger_not_in_pl: u64,
    pub pl_blocks: u64,
    pub populated: u64,
    pub triangles: u64,
    pub parts: u64,
    pub fallback: u64,
    pub max_part_people: u32,
    pub sums: [u64; 8],
    pub points_sums: [u64; 8],
    pub state_row: [u32; 9],
    pub secs: f64,
    pub inside: InsideStats,
    /// Populated blocks whose P.L. interior point is neither inside their
    /// parsed TIGER polygon nor within `SIMPLIFY_M` of it (review F1: an
    /// independent check of the .shp/.dbf record pairing), and the first few.
    #[serde(default)]
    pub intpt_outside: u64,
    #[serde(default)]
    pub intpt_outside_examples: Vec<(u64, f64)>,
}

/// Most blocks per thousand whose interior point may miss their polygon
/// before a state fails: a shifted .shp/.dbf pairing misses on nearly all.
pub const INTPT_OUTSIDE_PER_MILLE: u64 = 1;

/// Distance in metres from a block's P.L. interior point to its parsed
/// polygon: 0 when inside.
pub fn intpt_distance_m(rings: &[Vec<[i64; 2]>], intpt: (f64, f64)) -> f64 {
    let (lat, lon) = intpt;
    let p = tiger::project(lon, lat);
    let pf = [p[0] as f64, p[1] as f64];
    if tiger::inside(pf, rings) {
        0.0
    } else {
        tiger::boundary_distance(pf, rings) / tiger::quanta_per_metre(lat)
    }
}

pub struct StateIo<'a> {
    pub parts: &'a mut BufWriter<File>,
    pub points: &'a mut BufWriter<File>,
    pub hist: &'a mut [u64],
}

const BATCH: usize = 16_384;

/// Converts one state: join, geometry, spill, per-state checks, and the
/// inside-check of `quota` sampled blocks at `dots_per_block` dots each.
pub fn process_state(fips: u8, pl_zip: &Path, tiger_zip: &Path, io: StateIo<'_>, quota: usize, dots_per_block: u32) -> Result<StateStats> {
    let started = Instant::now();
    let pl = pl94::parse_state(pl_zip).with_context(|| format!("parsing {}", pl_zip.display()))?;
    if pl.state != fips {
        bail!("{} is state {}, want {fips}", pl_zip.display(), pl.state);
    }
    let mut st = StateStats { fips, pl_blocks: pl.blocks.len() as u64, state_row: pl.state_row, ..Default::default() };
    let mut joiner = Joiner::new(&pl.blocks);
    let mut batch: Vec<(usize, Vec<Vec<[f64; 2]>>)> = Vec::with_capacity(BATCH);
    let mut samples: BinaryHeap<Sample> = BinaryHeap::new();
    let StateIo { parts: parts_w, points: points_w, hist } = io;

    let mut run_batch = |batch: &mut Vec<(usize, Vec<Vec<[f64; 2]>>)>, st: &mut StateStats, samples: &mut BinaryHeap<Sample>| -> Result<()> {
        let geoms: Vec<(usize, BlockGeom)> =
            batch.par_drain(..).map(|(i, rings)| (i, block_geometry(&pl.blocks[i], project_rings(&rings)))).collect();
        for (i, g) in geoms {
            let b = &pl.blocks[i];
            st.populated += 1;
            // A null shape has nothing to check (it is a counted fallback).
            let d = if g.rings.is_empty() { 0.0 } else { intpt_distance_m(&g.rings, b.intpt) };
            if d > SIMPLIFY_M {
                st.intpt_outside += 1;
                if st.intpt_outside_examples.len() < 20 {
                    st.intpt_outside_examples.push((b.geoid, d));
                }
            }
            st.triangles += g.triangles as u64;
            st.fallback += u64::from(g.fallback);
            for (k, p) in g.parts.iter().enumerate() {
                let rec = SpillPart::new(b.geoid, fips, k as u32, if g.fallback { FLAG_FALLBACK } else { 0 }, p);
                spill::write_part(parts_w, &rec)?;
                hist[pyramid::band_of(&rec)] += 1;
                st.parts += 1;
                st.max_part_people = st.max_part_people.max(rec.people());
                for (s, &n) in st.sums.iter_mut().zip(&p.c) {
                    *s += u64::from(n);
                }
            }
            spill::write_point(points_w, &BlockPoint::new(b.geoid, b.intpt.0, b.intpt.1, b.c))?;
            for (s, &n) in st.points_sums.iter_mut().zip(&b.c) {
                *s += u64::from(n);
            }
            if quota > 0 && !g.fallback {
                let hash = splitmix(b.geoid);
                if samples.len() < quota || samples.peek().is_some_and(|m| hash < m.hash) {
                    samples.push(Sample { hash, lat: b.intpt.0, geom: g });
                    if samples.len() > quota {
                        samples.pop();
                    }
                }
            }
        }
        Ok(())
    };

    let tiger_blocks = tiger::for_each_block(tiger_zip, |geoid, rings| {
        if let Some(i) = joiner.take(&pl.blocks, geoid)? {
            batch.push((i, rings));
            if batch.len() == BATCH {
                run_batch(&mut batch, &mut st, &mut samples)?;
            }
        }
        Ok(())
    })?;
    run_batch(&mut batch, &mut st, &mut samples)?;
    joiner.finish(&pl.blocks)?;
    st.tiger_blocks = tiger_blocks;
    st.tiger_not_in_pl = joiner.not_in_pl;

    // Per-state checks: parts and points both hold exactly the state row.
    let want: Vec<u64> = pl.state_row[1..].iter().map(|&n| u64::from(n)).collect();
    let total: u64 = st.sums.iter().sum();
    if st.sums[..] != want[..] || total != u64::from(pl.state_row[0]) || st.points_sums != st.sums {
        bail!(
            "state {fips:02}: parts sum {total} {:?}, points {:?}, state row {:?}",
            st.sums,
            st.points_sums,
            pl.state_row
        );
    }

    // Review F1: the geometry paired by record position must hold its own
    // GEOID's P.L. interior point.
    if st.intpt_outside * 1000 > st.populated * INTPT_OUTSIDE_PER_MILLE {
        bail!(
            "state {fips:02}: {} of {} populated blocks have their P.L. interior point off their TIGER polygon (first: {:?}) -- is the .shp/.dbf pairing shifted?",
            st.intpt_outside,
            st.populated,
            st.intpt_outside_examples
        );
    }

    for s in samples.into_sorted_vec() {
        st.inside.merge(&check_inside(&s.geom, s.lat, dots_per_block));
    }
    st.secs = started.elapsed().as_secs_f64();
    Ok(st)
}

// ---------------------------------------------------------------------
// The national build: disk, downloads, checkpoints, the tile stage.

#[derive(Debug, Clone)]
pub struct BuildOpts {
    pub cache: PathBuf,
    pub out: PathBuf,
    /// Where `parts.spill`, `blocks.points`, the checkpoint and the report
    /// go; `--out`'s directory when `None`.
    pub work: Option<PathBuf>,
    /// Abbreviations to build; all 51 when empty.
    pub states: Vec<String>,
    pub inside_blocks: u32,
    pub inside_dots: u32,
    pub keep_zips: bool,
    pub max_pass_parts: u64,
    pub fresh: bool,
    /// Skip the states still to do and run the tile stage on what the
    /// checkpoint holds (a partial build's archive, for measuring).
    pub tiles_only: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Checkpoint {
    pub states: Vec<StateStats>,
    pub spill_len: u64,
    pub points_len: u64,
    pub hist: Vec<u64>,
    pub start_free: u64,
    pub min_free: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Report {
    pub states: Vec<StateStats>,
    pub pyramid: PyramidStats,
    pub archive_bytes: u64,
    pub spill_bytes: u64,
    pub points_bytes: u64,
    pub tile_stage_secs: f64,
    pub readback: Vec<(u8, [u64; 8])>,
    pub start_free: u64,
    pub min_free: u64,
}

/// Free bytes on the volume holding `path` (`df -k`).
pub fn df_free(path: &Path) -> Result<u64> {
    let out = Command::new("df").arg("-k").arg(path).output().context("running df")?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().last().context("df printed nothing")?;
    let kb: u64 = line.split_whitespace().nth(3).context("df: no Avail column")?.parse().context("df: Avail is not a number")?;
    Ok(kb * 1024)
}

fn tiger_url(fips: u8) -> String {
    format!("https://www2.census.gov/geo/tiger/TIGER2020/TABBLOCK20/tl_2020_{fips:02}_tabblock20.zip")
}

fn tiger_zip(cache: &Path, fips: u8) -> PathBuf {
    cache.join("tiger").join(format!("tl_2020_{fips:02}_tabblock20.zip"))
}

/// Content-Length of a HEAD request.
fn head_size(url: &str) -> Result<u64> {
    let out = Command::new("curl").args(["-sIL", url]).output().context("running curl")?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .filter_map(|l| l.strip_prefix("Content-Length:").or_else(|| l.strip_prefix("content-length:")))
        .last()
        .and_then(|v| v.trim().parse().ok())
        .with_context(|| format!("no Content-Length from HEAD {url}"))
}

fn fetch_script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/fetch-census-2020.sh")
}

/// Starts `fetch-census-2020.sh --tiger FIPS` (size-checked against its HEAD).
fn spawn_fetch(cache: &Path, fips: u8, need_mb: u64) -> Result<Child> {
    let data = cache.parent().context("the cache dir has no parent")?;
    Command::new("bash")
        .arg(fetch_script())
        .args(["--tiger", &format!("{fips:02}")])
        .env("GPUI_MAP_DATA", data)
        .env("GPUI_MAP_TIGER_NEED_MB", need_mb.to_string())
        .spawn()
        .context("starting fetch-census-2020.sh")
}

fn wait_fetch(mut child: Child, fips: u8) -> Result<()> {
    let status = child.wait()?;
    if !status.success() {
        bail!("fetch-census-2020.sh --tiger {fips:02} failed: {status}");
    }
    Ok(())
}

fn file_len(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

/// Opens a spill for appending at exactly `len` bytes (truncating any tail
/// a crashed state left).
fn open_at(path: &Path, len: u64) -> Result<BufWriter<File>> {
    let mut f = OpenOptions::new().create(true).read(true).write(true).truncate(false).open(path).with_context(|| format!("opening {}", path.display()))?;
    if f.metadata()?.len() < len {
        bail!("{} is shorter than its checkpointed {len} bytes -- rerun with --fresh", path.display());
    }
    f.set_len(len)?;
    f.seek(SeekFrom::Start(len))?;
    Ok(BufWriter::with_capacity(1 << 22, f))
}

fn sync(w: &mut BufWriter<File>) -> Result<u64> {
    w.flush()?;
    w.get_ref().sync_all()?;
    Ok(w.get_ref().metadata()?.len())
}

fn save_json<T: Serialize>(path: &Path, v: &T) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = File::create(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(v)?)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// The stop message: which states are done and how to resume.
fn stop(done: &[StateStats], why: &str) -> ! {
    let names: Vec<&str> = done.iter().map(|s| s.abbr.as_str()).collect();
    eprintln!("STOP: {why}");
    eprintln!("states done ({}): {}", names.len(), names.join(","));
    eprintln!(
        "resume: cargo run --release --manifest-path tools/race-build/Cargo.toml -- build   (the same arguments and the same code: the checkpoint picks up where this stopped, and a resume after changing the converter would mix outputs -- use --fresh then)"
    );
    std::process::exit(3);
}

pub fn build(opts: &BuildOpts) -> Result<()> {
    let out_dir = match &opts.work {
        Some(w) => w.clone(),
        None => opts.out.parent().context("--out has no parent directory")?.to_path_buf(),
    };
    std::fs::create_dir_all(&out_dir)?;
    std::fs::create_dir_all(opts.out.parent().context("--out has no parent directory")?)?;
    std::fs::create_dir_all(opts.cache.join("tiger"))?;
    let spill_path = out_dir.join("parts.spill");
    let points_path = out_dir.join("blocks.points");
    let ckpt_path = out_dir.join("race-build.checkpoint.json");
    let report_path = out_dir.join("race-build.report.json");

    let wanted: Vec<(&str, u8)> = if opts.states.is_empty() {
        STATES.to_vec()
    } else {
        opts.states
            .iter()
            .map(|a| STATES.iter().find(|s| s.0 == a).copied().with_context(|| format!("unknown state {a:?}")))
            .collect::<Result<_>>()?
    };

    let mut ck: Checkpoint = if !opts.fresh && ckpt_path.exists() {
        serde_json::from_slice(&std::fs::read(&ckpt_path)?).context("reading the checkpoint")?
    } else {
        Checkpoint { hist: vec![0; BANDS], ..Default::default() }
    };
    if let Some(extra) = ck.states.iter().find(|s| !wanted.iter().any(|w| w.1 == s.fips)) {
        bail!("the checkpoint holds state {} which this run does not ask for -- rerun with --fresh", extra.abbr);
    }
    let mut parts_w = open_at(&spill_path, ck.spill_len)?;
    let mut points_w = open_at(&points_path, ck.points_len)?;

    // MG-R3: free space on the zips' volume, sampled every 10 s for the
    // whole run. The per-state rule checks the same volume.
    let disk = opts.cache.clone();
    let free_now = df_free(&disk)?;
    if ck.start_free == 0 {
        ck.start_free = free_now;
        ck.min_free = free_now;
    }
    let min_free = Arc::new(AtomicU64::new(ck.min_free.min(free_now)));
    let done_flag = Arc::new(AtomicBool::new(false));
    let sampler = {
        let (min_free, done_flag, dir) = (min_free.clone(), done_flag.clone(), disk.clone());
        std::thread::spawn(move || {
            while !done_flag.load(Ordering::Relaxed) {
                if let Ok(f) = df_free(&dir) {
                    min_free.fetch_min(f, Ordering::Relaxed);
                }
                for _ in 0..100 {
                    if done_flag.load(Ordering::Relaxed) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        })
    };

    // A crash between the checkpoint and the delete leaves a done state's
    // zip behind (review F3): clear those now.
    if !opts.keep_zips {
        for s in &ck.states {
            let z = tiger_zip(&opts.cache, s.fips);
            if z.exists() {
                std::fs::remove_file(&z).with_context(|| format!("deleting {}", z.display()))?;
                eprintln!("deleted leftover {}", z.display());
            }
        }
    }

    let todo: Vec<(&str, u8)> = if opts.tiles_only {
        Vec::new()
    } else {
        wanted.iter().copied().filter(|w| !ck.states.iter().any(|s| s.fips == w.1)).collect()
    };
    // Largest first, while the disk is freest (Texas needs the most).
    let mut sized: Vec<(&str, u8, u64)> = todo.iter().map(|&(a, f)| head_size(&tiger_url(f)).map(|n| (a, f, n))).collect::<Result<_>>()?;
    sized.sort_by_key(|s| std::cmp::Reverse(s.2));
    eprintln!("{} states done, {} to go", ck.states.len(), sized.len());

    let total_wanted = wanted.len();
    let mut prefetch: Option<(u8, Child)> = None;
    for (n, &(abbr, fips, head)) in sized.iter().enumerate() {
        let zip = tiger_zip(&opts.cache, fips);
        if let Some((pf, child)) = prefetch.take() {
            wait_fetch(child, pf)?;
        }
        let free = df_free(&disk)?;
        let on_disk = if file_len(&zip) == head { head } else { 0 };
        // Nothing is unpacked; a stored zip unpacks to about its own size,
        // so `head` stands in until the central directory gives the truth.
        if free + on_disk < 2 * head + HEADROOM {
            stop(&ck.states, &format!("{abbr}: {} MB free, need zip {} MB + unpacked ~{} MB + 10 GiB", free >> 20, head >> 20, head >> 20));
        }
        if on_disk == 0 {
            let need_mb = (2 * head) >> 20;
            wait_fetch(spawn_fetch(&opts.cache, fips, need_mb)?, fips)?;
        }
        let zip_bytes = file_len(&zip);
        if zip_bytes != head {
            bail!("{}: {zip_bytes} bytes, HEAD said {head}", zip.display());
        }
        let unpacked = tiger::unpacked_size(&zip)?;
        let free = df_free(&disk)?;
        if free + zip_bytes < zip_bytes + unpacked + HEADROOM {
            stop(&ck.states, &format!("{abbr}: {} MB free with its zip on disk, need unpacked {} MB + 10 GiB", free >> 20, unpacked >> 20));
        }
        // Two in flight: fetch the next state while this one converts, if
        // the disk allows both.
        if let Some(&(_, nfips, nhead)) = sized.get(n + 1) {
            if file_len(&tiger_zip(&opts.cache, nfips)) != nhead && free >= unpacked + 2 * nhead + HEADROOM {
                prefetch = Some((nfips, spawn_fetch(&opts.cache, nfips, (2 * nhead) >> 20)?));
            }
        }

        let states_left = (total_wanted - ck.states.len()) as u64;
        let sampled: u64 = ck.states.iter().map(|s| s.inside.blocks).sum();
        let quota = u64::from(opts.inside_blocks).saturating_sub(sampled).div_ceil(states_left.max(1)) as usize;
        let dots_per_block = opts.inside_dots / opts.inside_blocks.max(1);
        let pl_zip = opts.cache.join("pl").join(format!("{abbr}2020.pl.zip"));
        let io = StateIo { parts: &mut parts_w, points: &mut points_w, hist: &mut ck.hist };
        let mut st = match process_state(fips, &pl_zip, &zip, io, quota, dots_per_block) {
            Ok(st) => st,
            Err(e) => {
                // Leave no orphaned download behind (review F9).
                if let Some((_, mut child)) = prefetch.take() {
                    child.kill().ok();
                    child.wait().ok();
                }
                return Err(e.context(format!("state {abbr}")));
            }
        };
        st.abbr = abbr.to_string();
        st.zip_bytes = zip_bytes;
        st.zip_bytes_head = head;
        st.unpacked_bytes = unpacked;
        st.free_before = free;
        ck.spill_len = sync(&mut parts_w)?;
        ck.points_len = sync(&mut points_w)?;
        ck.min_free = min_free.load(Ordering::Relaxed);
        eprintln!(
            "{abbr}: {} populated blocks, {} triangles, {} parts, {} fallback, sum {} [OK], intpt off {}, inside {}/{} simplified, {}/{} original, {:.0}s, free {} MB",
            st.populated,
            st.triangles,
            st.parts,
            st.fallback,
            st.sums.iter().sum::<u64>(),
            st.intpt_outside,
            st.inside.in_simplified,
            st.inside.dots,
            st.inside.in_original,
            st.inside.dots,
            st.secs,
            free >> 20
        );
        ck.states.push(st);
        save_json(&ckpt_path, &ck)?;
        if !opts.keep_zips {
            std::fs::remove_file(&zip).with_context(|| format!("deleting {}", zip.display()))?;
        }
    }
    if let Some((pf, child)) = prefetch.take() {
        wait_fetch(child, pf)?;
    }
    drop(parts_w);
    drop(points_w);

    // The tile stage.
    let free = df_free(&disk)?;
    let archive_guess = ck.spill_len / 3;
    if free < archive_guess + (5 << 30) {
        stop(&ck.states, &format!("tile stage: {} MB free, need ~{} MB for the archive + 5 GiB", free >> 20, archive_guess >> 20));
    }
    let started = Instant::now();
    let pyr = write_archive(&spill_path, &ck.hist, &opts.out, opts.max_pass_parts)?;
    let tile_stage_secs = started.elapsed().as_secs_f64();
    let readback = read_back(&opts.out)?;
    done_flag.store(true, Ordering::Relaxed);
    sampler.join().ok();

    let report = Report {
        states: ck.states.clone(),
        pyramid: pyr,
        archive_bytes: file_len(&opts.out),
        spill_bytes: ck.spill_len,
        points_bytes: ck.points_len,
        tile_stage_secs,
        readback,
        start_free: ck.start_free,
        min_free: min_free.load(Ordering::Relaxed),
    };
    save_json(&report_path, &report)?;
    verify(&report, ck.states.len() == STATES.len())?;
    eprintln!("report: {}", report_path.display());
    Ok(())
}

/// Writes the archive: every tile the pyramid emits, then the directory.
pub fn write_archive(spill: &Path, hist: &[u64], out: &Path, max_pass_parts: u64) -> Result<PyramidStats> {
    use pmtiles::{Compression, PmTilesWriter, TileCoord, TileType};
    let tmp = out.with_extension("pmtiles.partial");
    let sink_file = BufWriter::new(File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?);
    let mut writer = PmTilesWriter::new(TileType::Unknown)
        .tile_compression(Compression::None)
        .min_zoom(0)
        .max_zoom(pyramid::PART_ZOOM)
        .bounds(-180.0, -85.051_128_78, 180.0, 85.051_128_78)
        .metadata(pyramid::METADATA)
        .create(sink_file)?;
    let stats = {
        let mut sink = |t: TileId, b: &[u8]| -> Result<()> {
            writer.add_tile(TileCoord::new(t.z, t.x, t.y)?, b)?;
            Ok(())
        };
        pyramid::build_from_spill(spill, hist, max_pass_parts, &mut sink)?
    };
    writer.finalize()?;
    std::fs::rename(&tmp, out)?;
    Ok(stats)
}

/// Reads the archive back through the engine's own opener (so the C7
/// metadata must parse into the descriptor) and sums every tile's decoded
/// groups per level (255 = part tiles).
pub fn read_back(archive: &Path) -> Result<Vec<(u8, [u64; 8])>> {
    use gpui_maps::overlay::dotgrid::codec_cat::decode_any;
    use gpui_maps::overlay::dotgrid::pmtiles_source::PmtilesOpener;
    use gpui_maps::overlay::dotgrid::{GridCountSource, GridRequest, GridTileKey};

    let mut f = File::open(archive)?;
    let len = f.metadata()?.len();
    let mut read = |offset: u64, length: u64| -> Result<Vec<u8>> {
        let length = length.min(len.saturating_sub(offset));
        let mut buf = vec![0u8; length as usize];
        f.seek(SeekFrom::Start(offset))?;
        f.read_exact(&mut buf)?;
        Ok(buf)
    };
    let range = |r: &GridRequest| match r {
        GridRequest::Range { offset, length, .. } => Some((*offset, *length)),
        _ => None,
    };
    let mut opener = PmtilesOpener::new(archive.display().to_string());
    let (o, l) = range(&opener.first_request()).unwrap();
    let first = read(o, l)?;
    let more = opener.after_first(&first).map_err(|e| anyhow::anyhow!("opening the archive: {e:?}"))?;
    let bodies = more.iter().map(|r| range(r).context("a non-range follow-up").and_then(|(o, l)| read(o, l))).collect::<Result<Vec<_>>>()?;
    let src = opener.finish(&bodies).map_err(|e| anyhow::anyhow!("opening the archive: {e:?}"))?;
    if *src.descriptor() != pyramid::descriptor() {
        bail!("the archive's descriptor {:?} is not the build's", src.descriptor());
    }

    // Every tile id the archive can hold at its zooms, by walking the
    // tiles the pyramid said it wrote would need the list; instead walk
    // the zooms' tiles over the US bounding boxes.
    let mut sums: BTreeMap<u8, [u64; 8]> = BTreeMap::new();
    let d = pyramid::descriptor();
    let mut visit = |key: GridTileKey, level: u8, read: &mut dyn FnMut(u64, u64) -> Result<Vec<u8>>| -> Result<()> {
        if let Some((o, l)) = range(&src.request(&[key])) {
            let tile = decode_any(&read(o, l)?).map_err(|e| anyhow::anyhow!("tile {:?}: {e:?}", key.tile))?;
            let s = sums.entry(level).or_default();
            for c in tile.cats.iter().chain(tile.parts.iter().map(|p| &p.counts)) {
                for (a, &v) in s.iter_mut().zip(c) {
                    *a += u64::from(v);
                }
            }
        }
        Ok(())
    };
    // The US (with Alaska across the antimeridian, Hawaii) lies within
    // lat 17..72 on both sides of 180°: walk every tile of those rows.
    let rows = |z: u8| {
        let y = |lat: f64| (tiger::project(0.0, lat)[1] >> (31 - u32::from(z))) as u32;
        y(72.0)..=y(17.0)
    };
    for level in 0..d.levels {
        let z = d.max_data_zoom - level;
        for y in rows(z) {
            for x in 0..(1u32 << z) {
                visit(GridTileKey { tile: TileId { z, x, y }, level, epoch: 2020, epoch_b: None }, level, &mut read)?;
            }
        }
    }
    for y in rows(pyramid::PART_ZOOM) {
        for x in 0..(1u32 << pyramid::PART_ZOOM) {
            visit(GridTileKey::part(TileId { z: pyramid::PART_ZOOM, x, y }, 2020), GridTileKey::PART_LEVEL, &mut read)?;
        }
    }
    Ok(sums.into_iter().collect())
}

/// MG-R6/R7: exact sums at every level, per state, and the named states.
pub fn verify(r: &Report, national: bool) -> Result<()> {
    let mut want = [0u64; 8];
    for s in &r.states {
        for (w, &v) in want.iter_mut().zip(&s.sums) {
            *w += v;
        }
    }
    let mut bad = Vec::new();
    if national {
        if want != pl94::US_GROUP_TOTALS || want.iter().sum::<u64>() != pl94::US_TOTAL {
            bad.push(format!("the states sum to {want:?}, not the US totals"));
        }
        if r.states.len() != STATES.len() {
            bad.push(format!("{} states, want 51", r.states.len()));
        }
    }
    for l in r.pyramid.levels.iter().chain(std::iter::once(&r.pyramid.parts)) {
        if l.sums != want {
            bad.push(format!("level {} sums {:?}, want {want:?}", l.level, l.sums));
        }
    }
    for (level, s) in &r.readback {
        if *s != want {
            bad.push(format!("archive read-back level {level} sums {s:?}, want {want:?}"));
        }
    }
    if r.readback.len() != usize::from(pyramid::LEVELS) + 1 {
        bad.push(format!("archive read-back found {} levels, want 11", r.readback.len()));
    }
    for s in &r.states {
        let l0 = r.pyramid.level0_by_state.get(&s.fips).copied().unwrap_or_default();
        if l0 != s.sums || s.sums.iter().sum::<u64>() != u64::from(s.state_row[0]) {
            bad.push(format!("state {}: level 0 {l0:?}, parts {:?}, state row {:?}", s.abbr, s.sums, s.state_row));
        }
    }
    // MG-R8 as amended (review F2): every dot inside the simplified polygon,
    // and every dot inside the original or within SIMPLIFY_M of it.
    let mut ins = InsideStats::default();
    for s in &r.states {
        ins.merge(&s.inside);
        if s.intpt_outside * 1000 > s.populated * INTPT_OUTSIDE_PER_MILLE {
            bad.push(format!("state {}: {} interior points off their polygons", s.abbr, s.intpt_outside));
        }
    }
    if ins.in_simplified != ins.dots || ins.in_original + ins.near_original != ins.dots {
        bad.push(format!("MG-R8: {ins:?}"));
    }
    for (abbr, total) in STATE_CONSTANTS {
        if let Some(s) = r.states.iter().find(|s| s.abbr == abbr) {
            if s.sums.iter().sum::<u64>() != total {
                bad.push(format!("{abbr}: {} people, want {total}", s.sums.iter().sum::<u64>()));
            }
        }
    }
    if !bad.is_empty() {
        bail!("MISMATCH:\n  {}", bad.join("\n  "));
    }
    eprintln!("MG-R6/R7: every level and every state exact ({} people)", want.iter().sum::<u64>());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spill::{PART_RECORD_BYTES, POINT_RECORD_BYTES};

    fn pl_block(geoid: u64) -> BlockCounts {
        BlockCounts { geoid, state: (geoid / 10u64.pow(13)) as u8, c: [3, 1, 0, 0, 0, 0, 0, 0], intpt: (38.9, -77.0) }
    }

    #[test]
    fn a_degenerate_block_keeps_its_people_at_its_interior_point() {
        let b = BlockCounts { geoid: 1, state: 11, c: [2, 0, 0, 0, 0, 0, 0, 0], intpt: (38.9, -77.0) };
        let (parts, fell_back) = parts_for_block(&b, &[vec![[0, 0], [1, 0], [2, 0]]]); // collinear
        assert_eq!(parts.iter().map(|p| p.c[0]).sum::<u32>(), 2);
        assert!(fell_back);
        // The fallback triangle sits on the interior point, about 10 m a side.
        let centre = tiger::project(-77.0, 38.9);
        let t = parts[0].tri;
        let cx = (t.a[0] + t.b[0] + t.c[0]) / 3;
        let side_m = (t.b[0] - t.a[0]) as f64 / tiger::quanta_per_metre(38.9);
        assert!((cx - centre[0]).abs() <= 1 && (side_m - 10.0).abs() < 0.05, "{cx} {centre:?} {side_m}");
    }

    #[test]
    fn a_block_missing_from_tiger_is_an_error_naming_it() {
        let err = join_state(&[pl_block(110010001011000)], &[]).unwrap_err().to_string();
        assert!(err.contains("110010001011000"));
    }

    #[test]
    fn an_unpopulated_block_needs_no_polygon_and_a_populated_one_joins() {
        let mut empty = pl_block(110010001011001);
        empty.c = [0; 8];
        let pl = [pl_block(110010001011000), empty];
        let tiger = [TigerBlock { geoid: 110010001011000, rings: vec![] }, TigerBlock { geoid: 999, rings: vec![] }];
        let joined = join_state(&pl, &tiger).unwrap();
        assert_eq!(joined.len(), 1);
        assert_eq!(joined[0].0.geoid, 110010001011000);
    }

    #[test]
    fn a_block_conserves_its_groups_and_every_part_obeys_both_caps() {
        // A 2 km square with a hole, and more people than one part holds.
        let b = BlockCounts { geoid: 7, state: 11, c: [70_000, 3, 0, 5, 0, 0, 0, 1], intpt: (38.9, -77.0) };
        let o = tiger::project(-77.0, 38.9);
        let s = (tiger::quanta_per_metre(38.9) * 2000.0) as i64;
        let shell = vec![o, [o[0] + s, o[1]], [o[0] + s, o[1] + s], [o[0], o[1] + s], o];
        let hole = vec![[o[0] + s / 4, o[1] + s / 4], [o[0] + s / 2, o[1] + s / 4], [o[0] + s / 2, o[1] + s / 2], [o[0] + s / 4, o[1] + s / 2]];
        let g = block_geometry(&b, vec![shell, hole]);
        assert!(!g.fallback);
        let mut sum = [0u32; 8];
        for p in &g.parts {
            assert!(p.c.iter().sum::<u32>() <= MAX_PART_PEOPLE);
            assert!(gpui_maps::overlay::dotgrid::parts::extent(&p.tri) <= MAX_PART_EXTENT);
            for (a, &v) in sum.iter_mut().zip(&p.c) {
                *a += v;
            }
        }
        assert_eq!(sum, b.c);
        // Every sampled dot lies in the polygon.
        let st = check_inside(&g, 38.9, 2_000);
        assert_eq!(st.dots, 2_000);
        assert_eq!(st.in_original, 2_000, "{st:?}");
    }

    /// Writes a TIGER-like zip (deflated, like the real ones) of `polys`
    /// paired with `geoids` in record order.
    fn tiger_zip_of(path: &Path, polys: &[Vec<Vec<[f64; 2]>>], geoids: &[u64]) {
        let mut z = zip::ZipWriter::new(File::create(path).unwrap());
        let o = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        z.start_file("tl_2020_11_tabblock20.shp", o).unwrap();
        z.write_all(&tiger::shp_bytes(polys)).unwrap();
        z.start_file("tl_2020_11_tabblock20.dbf", o).unwrap();
        z.write_all(&tiger::dbf_bytes(geoids)).unwrap();
        z.finish().unwrap();
    }

    fn sq(lon: f64, lat: f64) -> Vec<Vec<[f64; 2]>> {
        vec![vec![[lon, lat], [lon + 0.001, lat], [lon + 0.001, lat + 0.001], [lon, lat + 0.001], [lon, lat]]]
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("race-build-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Runs `process_state` on a synthetic state; the P.L. interior points
    /// sit at each block's true square centre.
    fn run_state(dir: &Path, polys: &[Vec<Vec<[f64; 2]>>], geoids: &[u64], truth: &[(u64, [u32; 8], (f64, f64))]) -> Result<StateStats> {
        tiger_zip_of(&dir.join("t.zip"), polys, geoids);
        crate::pl94::write_test_zip(&dir.join("dc2020.pl.zip"), truth);
        let mut parts = BufWriter::new(File::create(dir.join("parts.spill")).unwrap());
        let mut points = BufWriter::new(File::create(dir.join("blocks.points")).unwrap());
        let mut hist = vec![0u64; BANDS];
        process_state(11, &dir.join("dc2020.pl.zip"), &dir.join("t.zip"), StateIo { parts: &mut parts, points: &mut points, hist: &mut hist }, 0, 0)
    }

    #[test]
    fn a_shifted_shp_dbf_pairing_fails_the_state() {
        // 20 blocks along a street; the .dbf is one record off, so every
        // polygon carries its neighbour's GEOID.
        let dir = scratch("shift");
        let lons: Vec<f64> = (0..20).map(|i| -77.03 + i as f64 * 0.002).collect();
        let polys: Vec<_> = lons.iter().map(|&lon| sq(lon, 38.9)).collect();
        let geoids: Vec<u64> = (0..20).map(|i| 110_010_001_011_000 + i).collect();
        let truth: Vec<_> = geoids.iter().zip(&lons).map(|(&g, &lon)| (g, [5, 0, 0, 0, 0, 0, 0, 0], (38.9005, lon + 0.0005))).collect();
        let ok = run_state(&dir, &polys, &geoids, &truth).unwrap();
        assert_eq!(ok.intpt_outside, 0);
        let mut shifted = geoids.clone();
        shifted.rotate_left(1);
        let err = format!("{:#}", run_state(&dir, &polys, &shifted, &truth).unwrap_err());
        assert!(err.contains("20 of 20") && err.contains("pairing"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_null_shape_falls_back_to_its_interior_point() {
        let dir = scratch("null");
        let polys = vec![sq(-77.03, 38.9), vec![]];
        let geoids = [110_010_001_011_000u64, 110_010_001_011_001];
        let truth = [(geoids[0], [5, 0, 0, 0, 0, 0, 0, 0], (38.9005, -77.0295)), (geoids[1], [0, 3, 0, 0, 0, 0, 0, 0], (38.95, -77.1))];
        let st = run_state(&dir, &polys, &geoids, &truth).unwrap();
        assert_eq!((st.populated, st.fallback, st.intpt_outside), (2, 1, 0));
        assert_eq!(st.sums, [5, 3, 0, 0, 0, 0, 0, 0]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn shp_and_dbf_of_different_lengths_are_refused() {
        let dir = scratch("len");
        tiger_zip_of(&dir.join("t.zip"), &[sq(-77.03, 38.9)], &[1, 2]);
        let err = tiger::for_each_block(&dir.join("t.zip"), |_, _| Ok(())).unwrap_err().to_string();
        assert!(err.contains("different record counts"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_state_zip_streams_through_the_join_the_spill_and_the_checks() {
        // A synthetic "state 11" zip: two populated blocks and an empty one.
        let dir = std::env::temp_dir().join(format!("race-build-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tiger_zip = dir.join("tl_2020_11_tabblock20.zip");
        let polys = vec![sq(-77.03, 38.90), sq(-77.02, 38.90), sq(-77.01, 38.90)];
        let geoids = [110_010_001_011_000u64, 110_010_001_011_001, 110_010_001_011_002];
        tiger_zip_of(&tiger_zip, &polys, &geoids);
        let pl_zip = dir.join("dc2020.pl.zip");
        crate::pl94::write_test_zip(
            &pl_zip,
            &[
                (geoids[0], [10, 4, 3, 1, 0, 0, 0, 2], (38.9005, -77.0295)),
                (geoids[1], [0; 8], (38.9005, -77.0195)),
                (geoids[2], [5, 0, 0, 0, 0, 0, 0, 0], (38.9005, -77.0095)),
            ],
        );
        let mut parts = BufWriter::new(File::create(dir.join("parts.spill")).unwrap());
        let mut points = BufWriter::new(File::create(dir.join("blocks.points")).unwrap());
        let mut hist = vec![0u64; BANDS];
        let st = process_state(11, &pl_zip, &tiger_zip, StateIo { parts: &mut parts, points: &mut points, hist: &mut hist }, 2, 50).unwrap();
        drop((parts, points));
        assert_eq!((st.tiger_blocks, st.populated, st.fallback), (3, 2, 0));
        assert_eq!(st.sums, [15, 4, 3, 1, 0, 0, 0, 2]);
        assert_eq!(st.inside.blocks, 2);
        assert_eq!(st.inside.in_original, st.inside.dots);
        assert_eq!(file_len(&dir.join("blocks.points")), 2 * POINT_RECORD_BYTES as u64);
        assert_eq!(file_len(&dir.join("parts.spill")), st.parts * PART_RECORD_BYTES as u64);
        assert_eq!(hist.iter().sum::<u64>(), st.parts);

        // And the archive holds them, read back through the engine's opener.
        let out = dir.join("race-dots-test.pmtiles");
        let pyr = write_archive(&dir.join("parts.spill"), &hist, &out, 1_000).unwrap();
        assert_eq!(pyr.parts.sums, st.sums);
        let rb = read_back(&out).unwrap();
        assert_eq!(rb.len(), 11);
        assert!(rb.iter().all(|(_, s)| *s == st.sums), "{rb:?}");
        std::fs::remove_dir_all(&dir).ok();
    }
}

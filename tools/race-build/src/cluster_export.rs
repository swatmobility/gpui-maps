//! `race-build export-clusters` (Task 12, spec §5.1, MG-R13's build half):
//! `blocks.points` into `race-clusters-2020.pmtiles`, the donut view's
//! offline cluster hierarchy.
//!
//! - Input: `blocks.points` (Task 5), one [`BlockPoint`] per populated
//!   block, read through [`BlockPoint::decode`] only; its sums are checked
//!   against the P.L. totals before anything is built.
//! - Index: the engine's own Supercluster port,
//!   [`ClusterIndex`] in radius mode, 50 px of a 512 px tile, `max_zoom`
//!   11, `min_points` 2, with 9 `Sum` properties: `pop` and `c1`..`c8`. A
//!   centre is weighted by block count (Supercluster's), the sums carry
//!   the people (spec §5.1 "Weighting").
//! - Export: for each zoom 3..=11, every item of that zoom -- cluster or
//!   leaf block -- goes to the tile of that zoom holding its position
//!   (`clusters_tile::encode`, positions on the 1e-7° quantum). A cluster
//!   records Supercluster's `getClusterExpansionZoom`; a leaf records
//!   [`DOTS_ZOOM`] (dots take over at z12 in cluster mode, spec §9).
//! - Checked twice, exactly: each zoom's records against the P.L. totals
//!   (331,449,281 people, the 8 group totals, and every populated block)
//!   before they are written, and the archive read back through the
//!   engine's own opener and decoder after. Any mismatch stops the export.
//! - Disk: estimated from the index's level sizes before anything is
//!   written ([`ESTIMATE_BYTES_PER_RECORD`], above the codec's measured
//!   size); refused (exit 4) unless the estimate plus 10 GB is free;
//!   stopped if free space falls under 5 GB while writing.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::ops::RangeInclusive;
use std::path::Path;
use std::time::Instant;

use anyhow::{bail, ensure, Context, Result};
use gpui_maps::geo::{tile_id_at, TileId};
use gpui_maps::overlay::cluster::{ClusterIndex, ClusterItem, ClusterMode, ClusterOptions, ClusterProperty, EXTENT};
use gpui_maps::overlay::clusters_tile::{self, snap, ClusterRecord, DOTS_ZOOM, MAX_ZOOM, MIN_ZOOM, RADIUS_PX};
use gpui_maps::overlay::dotgrid::pmtiles_dir::zxy_to_tile_id;
use gpui_maps::overlay::dotgrid::pmtiles_source::ArchiveOpener;
use gpui_maps::overlay::dotgrid::GridRequest;
use rayon::prelude::*;
use serde_json::{json, Value};

use crate::clickhouse_load::{df_free_mb, refuse};
use crate::pl94::{US_GROUP_TOTALS, US_TOTAL};
use crate::spill::{BlockPoint, POINT_RECORD_BYTES};

/// MG-R13's archive budget.
pub const ARCHIVE_BUDGET_BYTES: u64 = 100_000_000;
/// The per-record size the disk estimate assumes (the codec measures
/// under 20 B a record gzipped).
const ESTIMATE_BYTES_PER_RECORD: u64 = 32;
/// MG-R13's build gate: the export's own peak RSS at most 2 GB (decimal,
/// as the plan states it). Measured by the export itself
/// ([`peak_rss_bytes`]) and enforced ([`budget_failures`]), not read off
/// `/usr/bin/time` by hand (Task 12 review, M2).
pub const RSS_BUDGET_BYTES: u64 = 2_000_000_000;
const HEADROOM_MB: u64 = 10 * 1024;
const FLOOR_MB: u64 = 5 * 1024;
const ATTRIBUTION: &str = "U.S. Census Bureau, 2020 Census P.L. 94-171";
/// Populated blocks in `blocks.points` (MG-R2).
pub const US_POPULATED_BLOCKS: u64 = 5_769_942;

/// The index options: spec §5.1 verbatim.
pub fn options() -> ClusterOptions {
    let mut properties = vec![ClusterProperty::sum("pop", "pop")];
    properties.extend((1..=8).map(|g| ClusterProperty::sum(&format!("c{g}"), &format!("c{g}"))));
    ClusterOptions { cluster: true, radius_px: RADIUS_PX, max_zoom: MAX_ZOOM, min_points: 2, properties, mode: ClusterMode::Radius }
}

/// The hierarchy over a set of blocks, queried per zoom.
pub struct ClusterTiles {
    index: ClusterIndex,
    /// Each block's interior point, `[lon, lat]` in degrees (on the 1e-7°
    /// quantum: `blocks.points` stores e7 integers).
    points: Vec<[f64; 2]>,
    /// Each block's 8 group counts.
    counts: Vec<[u32; 8]>,
    zooms: RangeInclusive<u8>,
}

/// A `Sum` over whole numbers under 2^53 is exact in f64; this takes it
/// back, refusing anything that is not a whole `u32`.
fn exact_u32(v: f64) -> u32 {
    assert!(v.fract() == 0.0 && (0.0..=f64::from(u32::MAX)).contains(&v), "a sum {v} that is not a whole u32");
    v as u32
}

/// Builds the index (spec §5.1) over `pts` for the tile zooms `zooms`.
#[cfg_attr(not(test), allow(dead_code))]
pub fn build_cluster_tiles(pts: &[BlockPoint], zooms: RangeInclusive<u8>) -> ClusterTiles {
    let points: Vec<[f64; 2]> = pts.iter().map(|p| [f64::from(p.lon_e7) / 1e7, f64::from(p.lat_e7) / 1e7]).collect();
    build_from(points, pts.iter().map(|p| p.c).collect(), zooms)
}

/// [`build_cluster_tiles`] from the blocks' positions (degrees) and group
/// counts, so the export can drop its `BlockPoint`s (277 MB nationally)
/// before the index's own peak.
fn build_from(points: Vec<[f64; 2]>, counts: Vec<[u32; 8]>, zooms: RangeInclusive<u8>) -> ClusterTiles {
    assert!(*zooms.end() <= MAX_ZOOM, "tiles past the index's max_zoom");
    let mut inputs: Vec<Vec<f64>> = vec![counts.iter().map(|c| f64::from(c.iter().sum::<u32>())).collect()];
    inputs.extend((0..8).map(|g| counts.iter().map(|c| f64::from(c[g])).collect()));
    let index = ClusterIndex::build(&points, &options(), &inputs, true);
    ClusterTiles { index, points, counts, zooms }
}

impl ClusterTiles {
    fn record(&self, item: &ClusterItem) -> ClusterRecord {
        match *item {
            ClusterItem::Leaf { index } => {
                let (p, c) = (self.points[index as usize], self.counts[index as usize]);
                ClusterRecord { lon: snap(p[0]), lat: snap(p[1]), blocks: 1, pop: c.iter().sum(), counts: c, expansion_zoom: DOTS_ZOOM, leaf: true }
            }
            ClusterItem::Cluster { id, lon_lat, count, agg } => {
                let a = self.index.aggregates(agg);
                assert_eq!(a.len(), 9, "cluster {id} without its 9 sums");
                let mut counts = [0u32; 8];
                for (c, &v) in counts.iter_mut().zip(&a[1..]) {
                    *c = exact_u32(v);
                }
                let ez = self.index.expansion_zoom(id).expect("an id the index returned");
                ClusterRecord {
                    lon: snap(lon_lat[0]),
                    lat: snap(lon_lat[1]),
                    blocks: count,
                    pop: exact_u32(a[0]),
                    counts,
                    expansion_zoom: ez,
                    leaf: false,
                }
            }
        }
    }

    /// Every item of zoom `z`, with its record.
    fn items_at(&self, z: u8) -> Vec<(ClusterItem, ClusterRecord)> {
        let items = self.index.clusters([-180.0, -90.0, 180.0, 90.0], f64::from(z));
        items.into_par_iter().map(|it| (it, self.record(&it))).collect()
    }

    /// Every record of zoom `z`.
    pub fn clusters_at(&self, z: u8) -> Vec<ClusterRecord> {
        self.items_at(z).into_iter().map(|(_, r)| r).collect()
    }

    /// People over every record of zoom `z`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn pop_at(&self, z: u8) -> u64 {
        self.clusters_at(z).iter().map(|r| u64::from(r.pop)).sum()
    }

    /// What cluster `c` (a record of one of the tile zooms) is at zoom
    /// `z`: itself while `z` is at or under the zoom it formed at, its
    /// descendants in zoom `z`'s level past it (so, at its expansion zoom,
    /// the pieces it breaks into).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn children_at(&self, c: ClusterRecord, z: u8) -> Vec<ClusterRecord> {
        let item = self
            .zooms
            .clone()
            .find_map(|zz| self.items_at(zz).into_iter().find(|(_, r)| *r == c).map(|(it, _)| it))
            .expect("a record of the tile zooms");
        let mut out = Vec::new();
        self.descend(item, z, &mut out);
        out.iter().map(|it| self.record(it)).collect()
    }

    fn descend(&self, item: ClusterItem, z: u8, out: &mut Vec<ClusterItem>) {
        let ClusterItem::Cluster { id, .. } = item else {
            out.push(item);
            return;
        };
        // Supercluster's id: (origin << 5) + (zoom it formed at + 1) + n.
        let formed = ((id - self.index.len() as u32) % 32) as u8 - 1;
        if z <= formed {
            out.push(item);
            return;
        }
        for child in self.index.children(id).expect("a cluster has children") {
            self.descend(child, z, out);
        }
    }

    /// Zoom `z`'s records grouped into the tiles holding them, in tile-id
    /// order, each tile's records in Z-order (the codec's position deltas
    /// stay small).
    pub fn tiles_at(&self, z: u8) -> Vec<(TileId, Vec<ClusterRecord>)> {
        let mut by_tile: BTreeMap<u64, (TileId, Vec<(u64, ClusterRecord)>)> = BTreeMap::new();
        for (_, r) in self.items_at(z) {
            let t = tile_id_at(r.lon, r.lat, z);
            let key = morton(clusters_tile::quantise(r.lon), clusters_tile::quantise(r.lat));
            by_tile.entry(zxy_to_tile_id(t.z, t.x, t.y)).or_insert_with(|| (t, Vec::new())).1.push((key, r));
        }
        by_tile
            .into_values()
            .map(|(t, mut v)| {
                v.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.blocks.cmp(&b.1.blocks)));
                (t, v.into_iter().map(|(_, r)| r).collect())
            })
            .collect()
    }

    /// Items of each zoom in `zooms`, from the index's level sizes (no
    /// records made): the disk estimate's input.
    pub fn items_per_zoom(&self) -> BTreeMap<u8, u64> {
        self.index.level_sizes().into_iter().filter(|(z, _)| self.zooms.contains(z)).map(|(z, n)| (z, n as u64)).collect()
    }
}

/// A Z-order key of a position in quanta.
fn morton(lon_q: i64, lat_q: i64) -> u64 {
    fn spread(v: u32) -> u64 {
        let mut v = u64::from(v);
        v = (v | v << 16) & 0x0000_FFFF_0000_FFFF;
        v = (v | v << 8) & 0x00FF_00FF_00FF_00FF;
        v = (v | v << 4) & 0x0F0F_0F0F_0F0F_0F0F;
        v = (v | v << 2) & 0x3333_3333_3333_3333;
        (v | v << 1) & 0x5555_5555_5555_5555
    }
    let x = ((lon_q + 1_800_000_000) as u64 >> 1) as u32;
    let y = ((900_000_000 - lat_q) as u64) as u32;
    spread(x) | spread(y) << 1
}

/// Every `blocks.points` record, checked to be whole records.
pub fn read_block_points(path: &Path) -> Result<Vec<BlockPoint>> {
    let mut f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let len = f.metadata()?.len();
    ensure!(len % POINT_RECORD_BYTES as u64 == 0, "{}: {len} B is not whole {POINT_RECORD_BYTES}-byte records", path.display());
    let n = (len / POINT_RECORD_BYTES as u64) as usize;
    let mut out = Vec::with_capacity(n);
    let mut buf = vec![0u8; POINT_RECORD_BYTES * 65_536];
    let mut left = len as usize;
    while left > 0 {
        let take = left.min(buf.len());
        f.read_exact(&mut buf[..take])?;
        out.extend(buf[..take].as_chunks::<POINT_RECORD_BYTES>().0.iter().map(BlockPoint::decode));
        left -= take;
    }
    Ok(out)
}

/// People by group, blocks and records over `recs`.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct Sums {
    groups: [u64; 8],
    pop: u64,
    blocks: u64,
    records: u64,
    clusters: u64,
    leaves: u64,
}

impl Sums {
    fn add(&mut self, r: &ClusterRecord) {
        for (g, &c) in self.groups.iter_mut().zip(&r.counts) {
            *g += u64::from(c);
        }
        self.pop += u64::from(r.pop);
        self.blocks += u64::from(r.blocks);
        self.records += 1;
        if r.leaf {
            self.leaves += 1;
        } else {
            self.clusters += 1;
        }
    }

    fn exact(&self) -> bool {
        self.groups == US_GROUP_TOTALS && self.pop == US_TOTAL && self.blocks == US_POPULATED_BLOCKS
    }

    fn json(&self) -> Value {
        json!({"people": self.groups, "pop": self.pop, "blocks": self.blocks, "records": self.records,
               "clusters": self.clusters, "leaves": self.leaves, "exact": self.exact()})
    }
}

/// The zoom a PMTiles tile id is at.
fn zoom_of_id(id: u64) -> Option<u8> {
    (0..=MAX_ZOOM).find(|&z| id >= zxy_to_tile_id(z, 0, 0) && id < zxy_to_tile_id(z + 1, 0, 0))
}

/// Reads the archive back through the engine's opener and decoder: per
/// zoom, the sums, the tile count and the largest tile; every record
/// checked to sit in the tile it was found in.
fn read_back(archive: &Path) -> Result<BTreeMap<u8, (Sums, u64, usize)>> {
    let mut f = File::open(archive)?;
    let len = f.metadata()?.len();
    let mut read = |r: &GridRequest| -> Result<Vec<u8>> {
        let GridRequest::Range { offset, length, .. } = r else { bail!("not a range: {r:?}") };
        let length = (*length).min(len.saturating_sub(*offset));
        let mut buf = vec![0u8; length as usize];
        f.seek(SeekFrom::Start(*offset))?;
        f.read_exact(&mut buf)?;
        Ok(buf)
    };
    let mut opener = ArchiveOpener::new(archive.display().to_string(), clusters_tile::parse_cluster_meta);
    let first = read(&opener.first_request())?;
    let more = opener.after_first(&first).map_err(|e| anyhow::anyhow!("opening the archive: {e}"))?;
    let bodies = more.iter().map(&mut read).collect::<Result<Vec<_>>>()?;
    let opened = opener.finish_archive(&bodies).map_err(|e| anyhow::anyhow!("opening the archive: {e}"))?;
    let mut out: BTreeMap<u8, (Sums, u64, usize)> = BTreeMap::new();
    for e in &opened.entries {
        let body = read(&GridRequest::Range { location: String::new(), offset: e.offset, length: u64::from(e.length) })?;
        let recs = clusters_tile::decode(&body).map_err(|err| anyhow::anyhow!("tile id {}: {err}", e.tile_id))?;
        for k in 0..u64::from(e.run_length) {
            let id = e.tile_id + k;
            let z = zoom_of_id(id).with_context(|| format!("tile id {id} past z{MAX_ZOOM}"))?;
            let slot = out.entry(z).or_default();
            for r in &recs {
                let t = tile_id_at(r.lon, r.lat, z);
                ensure!(zxy_to_tile_id(t.z, t.x, t.y) == id, "z{z}: a record at {}, {} in tile id {id}, not its own", r.lon, r.lat);
                slot.0.add(r);
            }
            slot.1 += 1;
            slot.2 = slot.2.max(recs.len());
        }
    }
    Ok(out)
}

/// `ru_maxrss` in bytes: macOS reports bytes, Linux (and the other
/// Unixes this could run on) KiB.
pub fn normalise_maxrss(raw: i64, macos: bool) -> u64 {
    let raw = u64::try_from(raw).unwrap_or(0);
    if macos { raw } else { raw.saturating_mul(1024) }
}

/// This process's peak resident set so far, in bytes.
pub fn peak_rss_bytes() -> Result<u64> {
    // SAFETY: `getrusage` fills the zeroed struct it is handed; RUSAGE_SELF
    // is always a valid `who`.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    ensure!(rc == 0, "getrusage failed: {}", std::io::Error::last_os_error());
    Ok(normalise_maxrss(ru.ru_maxrss as i64, cfg!(target_os = "macos")))
}

/// The MG-R13 build gates this export broke, empty when it held: peak RSS
/// at most [`RSS_BUDGET_BYTES`], the archive at most
/// [`ARCHIVE_BUDGET_BYTES`]. A nonempty list fails the run (exit 5).
pub fn budget_failures(peak_rss: u64, archive_bytes: u64) -> Vec<String> {
    let mut out = Vec::new();
    if peak_rss > RSS_BUDGET_BYTES {
        out.push(format!("peak RSS {peak_rss} B is past the {RSS_BUDGET_BYTES} B gate"));
    }
    if archive_bytes > ARCHIVE_BUDGET_BYTES {
        out.push(format!("the archive's {archive_bytes} B is past the {ARCHIVE_BUDGET_BYTES} B gate"));
    }
    out
}

/// Builds, checks and writes the cluster archive, and returns the report.
/// A run that breaks a budget ([`budget_failures`]) returns its report
/// with `budget_failures` nonempty and leaves nothing at `out` (the caller
/// writes the report, then exits 5); a broken P.L. sum is an error.
pub fn export(cache: &Path, out: &Path) -> Result<Value> {
    let started = Instant::now();
    // 1. The input, checked against the P.L. totals.
    let pts = read_block_points(&cache.join("blocks.points"))?;
    let mut input = [0u64; 8];
    for p in &pts {
        for (s, &c) in input.iter_mut().zip(&p.c) {
            *s += u64::from(c);
        }
    }
    if input != US_GROUP_TOTALS || pts.len() as u64 != US_POPULATED_BLOCKS {
        bail!("blocks.points: {} blocks, groups {input:?} are not the P.L. totals", pts.len());
    }
    let read_s = started.elapsed().as_secs_f64();

    // 2. The index.
    let t = Instant::now();
    let points: Vec<[f64; 2]> = pts.iter().map(|p| [f64::from(p.lon_e7) / 1e7, f64::from(p.lat_e7) / 1e7]).collect();
    let counts: Vec<[u32; 8]> = pts.iter().map(|p| p.c).collect();
    drop(pts);
    let tiles = build_from(points, counts, MIN_ZOOM..=MAX_ZOOM);
    let build_s = t.elapsed().as_secs_f64();
    let per_zoom_items = tiles.items_per_zoom();
    eprintln!("export-clusters: index over {} blocks in {build_s:.1} s ({} MB), items per zoom {per_zoom_items:?}", tiles.index.len(), tiles.index.bytes() >> 20);

    // 3. Disk: the estimate plus 10 GB, before anything is written.
    let dir = out.parent().context("--out has no directory")?;
    std::fs::create_dir_all(dir)?;
    let estimate = per_zoom_items.values().sum::<u64>() * ESTIMATE_BYTES_PER_RECORD;
    let free_mb = df_free_mb(dir)?;
    eprintln!("export-clusters: estimate {} MB; {free_mb} MB free", estimate >> 20);
    if free_mb < (estimate >> 20) + HEADROOM_MB {
        return Err(refuse(4, format!("{free_mb} MB free at {}; the export needs {} MB + 10 GB", dir.display(), estimate >> 20)));
    }

    // 4. Encode, check and write, zoom by zoom (tile ids grow with the
    // zoom, so tiles go in order).
    let tmp = out.with_extension("pmtiles.tmp");
    let mut per_zoom = serde_json::Map::new();
    {
        let file = File::create(&tmp)?;
        let mut writer = pmtiles::PmTilesWriter::new(pmtiles::TileType::Unknown)
            .metadata(&clusters_tile::cluster_meta_json(ATTRIBUTION).to_string())
            .tile_compression(pmtiles::Compression::None)
            .min_zoom(MIN_ZOOM)
            .max_zoom(MAX_ZOOM)
            .create(file)?;
        for z in MIN_ZOOM..=MAX_ZOOM {
            let t0 = Instant::now();
            let zt = tiles.tiles_at(z);
            let mut sums = Sums::default();
            let (mut bytes, mut max_per_tile, mut bad_ez) = (0u64, 0usize, 0u64);
            let mut encoded = Vec::with_capacity(zt.len());
            for (t, recs) in &zt {
                for r in recs {
                    sums.add(r);
                    ensure!(r.counts.iter().map(|&c| u64::from(c)).sum::<u64>() == u64::from(r.pop), "z{z}: a record whose groups are not its pop: {r:?}");
                    bad_ez += u64::from(!r.leaf && !(r.expansion_zoom > z && r.expansion_zoom <= DOTS_ZOOM));
                }
                max_per_tile = max_per_tile.max(recs.len());
                ensure!(recs.len() as u64 <= u64::from(clusters_tile::MAX_RECORDS_PER_TILE), "z{z} {t:?}: {} records, past the codec's maximum", recs.len());
                let b = clusters_tile::encode(recs);
                bytes += b.len() as u64;
                encoded.push((*t, b));
            }
            if !sums.exact() || bad_ez > 0 {
                drop(writer);
                let _ = std::fs::remove_file(&tmp);
                bail!("z{z}: STOP -- the records do not carry the P.L. totals (or {bad_ez} expansion zooms are out of range): {:?}", sums);
            }
            for (t, b) in &encoded {
                writer.add_tile(pmtiles::TileCoord::new(t.z, t.x, t.y)?, b)?;
            }
            let free = df_free_mb(dir)?;
            if free < FLOOR_MB {
                drop(writer);
                let _ = std::fs::remove_file(&tmp);
                return Err(refuse(4, format!("free space fell to {free} MB while writing; stopped")));
            }
            eprintln!(
                "  z{z}: {} records ({} clusters, {} leaves), {} tiles, {} B, max {} a tile, pop {} ({:.1} s)",
                sums.records, sums.clusters, sums.leaves, zt.len(), bytes, max_per_tile, sums.pop, t0.elapsed().as_secs_f64()
            );
            let mut e = sums.json();
            let o = e.as_object_mut().unwrap();
            o.insert("tiles".into(), json!(zt.len()));
            o.insert("tile_bytes".into(), json!(bytes));
            o.insert("max_per_tile".into(), json!(max_per_tile));
            per_zoom.insert(format!("z{z}"), e);
        }
        writer.finalize()?;
    }
    let archive_bytes = std::fs::metadata(&tmp)?.len();

    // 5. Read back through the engine's own opener and decoder -- from the
    // temporary file, so a run that fails here or on a budget leaves
    // nothing at `out`.
    let back = read_back(&tmp)?;
    let mut exact = true;
    for z in MIN_ZOOM..=MAX_ZOOM {
        let (sums, tiles_n, max) = back.get(&z).copied().unwrap_or_default();
        let e = per_zoom.get_mut(&format!("z{z}")).and_then(Value::as_object_mut).unwrap();
        let ok = sums.exact() && e.get("tiles").and_then(Value::as_u64) == Some(tiles_n);
        exact &= ok;
        let mut rb = sums.json();
        rb.as_object_mut().unwrap().insert("tiles".into(), json!(tiles_n));
        rb.as_object_mut().unwrap().insert("max_per_tile".into(), json!(max));
        e.insert("read_back".into(), rb);
    }
    if !exact {
        let _ = std::fs::remove_file(&tmp);
        bail!("export-clusters: STOP -- the archive read back does not carry the P.L. totals at every zoom: {per_zoom:?}");
    }

    // 6. The budgets, enforced: the peak covers the whole run, read-back
    // included.
    let peak_rss = peak_rss_bytes()?;
    let failures = budget_failures(peak_rss, archive_bytes);
    if failures.is_empty() {
        std::fs::rename(&tmp, out)?;
    } else {
        let _ = std::fs::remove_file(&tmp);
    }

    Ok(json!({
        "archive": out.display().to_string(),
        "archive_bytes": archive_bytes,
        "budget_bytes": ARCHIVE_BUDGET_BYTES,
        "within_budget": archive_bytes <= ARCHIVE_BUDGET_BYTES,
        "peak_rss_bytes": peak_rss,
        "rss_budget_bytes": RSS_BUDGET_BYTES,
        "within_rss_budget": peak_rss <= RSS_BUDGET_BYTES,
        "budget_failures": failures,
        "estimate_bytes": estimate,
        "index": {"radius_px": RADIUS_PX, "extent": EXTENT, "max_zoom": MAX_ZOOM, "min_points": 2, "bytes": tiles.index.bytes()},
        "people_per_zoom": US_TOTAL,
        "blocks_per_zoom": US_POPULATED_BLOCKS,
        "per_zoom": per_zoom,
        "seconds": {"read": read_s, "index": build_s, "total": started.elapsed().as_secs_f64()},
    }))
}

/// Seeded blocks around a dozen US cities (normal-ish spreads of a few
/// kilometres to a degree), 1 to ~400 people each over the 8 groups.
#[cfg(test)]
pub(crate) fn synthetic_block_points(n: usize, seed: u64) -> Vec<BlockPoint> {
    let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut next = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        (s >> 11) as f64 / (1u64 << 53) as f64
    };
    let cities = [
        (-73.97, 40.78), (-118.25, 34.05), (-87.63, 41.88), (-95.37, 29.76), (-112.07, 33.45), (-75.16, 39.95),
        (-122.42, 37.77), (-80.19, 25.76), (-104.99, 39.74), (-149.9, 61.22), (-157.86, 21.31), (-100.0, 45.0),
    ];
    (0..n)
        .map(|i| {
            let (lon, lat) = cities[(next() * cities.len() as f64) as usize % cities.len()];
            let spread = [0.02, 0.2, 1.0][(next() * 3.0) as usize % 3];
            let dx = (next() + next() + next() - 1.5) * spread;
            let dy = (next() + next() + next() - 1.5) * spread;
            let mut c = [0u32; 8];
            for v in c.iter_mut() {
                if next() < 0.6 {
                    *v = (next() * next() * 100.0) as u32;
                }
            }
            if c.iter().all(|&v| v == 0) {
                c[(next() * 8.0) as usize % 8] = 1 + (next() * 20.0) as u32;
            }
            BlockPoint::new(i as u64, lat + dy, lon + dx, c)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_maps::overlay::clusters_tile::{decode, encode, DOTS_ZOOM};

    #[test]
    fn maxrss_is_bytes_on_macos_and_kib_elsewhere() {
        assert_eq!(normalise_maxrss(1_787_002_880, true), 1_787_002_880);
        assert_eq!(normalise_maxrss(1_745_120, false), 1_745_120 * 1024);
        assert_eq!(normalise_maxrss(-1, true), 0);
        // And this process's own reading is a plausible byte count: a test
        // binary is past 1 MB and nowhere near the gate.
        let own = peak_rss_bytes().unwrap();
        assert!(own > 1 << 20 && own < RSS_BUDGET_BYTES, "{own}");
    }

    #[test]
    fn the_rss_gate_fails_the_run_just_past_2_gb() {
        assert!(budget_failures(RSS_BUDGET_BYTES, 0).is_empty());
        assert!(budget_failures(1_787_002_880, 21_508_558).is_empty());
        let f = budget_failures(RSS_BUDGET_BYTES + 1, 0);
        assert_eq!(f.len(), 1);
        assert!(f[0].contains("peak RSS"), "{f:?}");
    }

    #[test]
    fn the_archive_gate_fails_the_run_just_past_100_mb() {
        assert!(budget_failures(0, ARCHIVE_BUDGET_BYTES).is_empty());
        let f = budget_failures(0, ARCHIVE_BUDGET_BYTES + 1);
        assert_eq!(f.len(), 1);
        assert!(f[0].contains("archive"), "{f:?}");
        assert_eq!(budget_failures(RSS_BUDGET_BYTES + 1, ARCHIVE_BUDGET_BYTES + 1).len(), 2);
    }

    #[test]
    fn clusters_conserve_population_at_every_zoom() {
        let pts = synthetic_block_points(20_000, 3); // seeded
        let tiles = build_cluster_tiles(&pts, 3..=11);
        let want: u64 = pts.iter().map(|p| u64::from(p.pop())).sum(); // brief: `p.pop`; BlockPoint keeps its 48 B layout, so a method
        for z in 3..=11 { assert_eq!(tiles.pop_at(z), want, "z{z}"); }
    }

    #[test]
    fn a_cluster_splits_at_its_expansion_zoom() {
        let pts = synthetic_block_points(2_000, 5);
        let tiles = build_cluster_tiles(&pts, 3..=11);
        let c = tiles.clusters_at(5).into_iter().find(|c| !c.leaf).unwrap();
        assert!(c.expansion_zoom > 5 && c.expansion_zoom <= 12);
        assert!(tiles.children_at(c, c.expansion_zoom).len() >= 2);
    }

    #[test]
    #[allow(clippy::clone_on_copy, clippy::cloned_ref_to_slice_refs)] // the plan's test, verbatim
    fn cluster_tiles_round_trip() {
        let r = ClusterRecord { lon: -73.9, lat: 40.7, blocks: 12, pop: 9000, counts: [1, 2, 3, 4, 5, 6, 7, 8964], expansion_zoom: 9, leaf: false };
        assert_eq!(decode(&encode(&[r.clone()])).unwrap(), vec![r]);
    }

    #[test]
    fn every_group_and_block_is_conserved_and_every_tile_holds_its_own_items() {
        let pts = synthetic_block_points(20_000, 7);
        let tiles = build_cluster_tiles(&pts, 3..=11);
        let mut want = [0u64; 8];
        for p in &pts {
            for (w, &c) in want.iter_mut().zip(&p.c) {
                *w += u64::from(c);
            }
        }
        for z in 3..=11u8 {
            let (mut groups, mut blocks) = ([0u64; 8], 0u64);
            for (tile, recs) in tiles.tiles_at(z) {
                assert_eq!(tile.z, z);
                let back = decode(&encode(&recs)).unwrap();
                assert_eq!(back, recs, "z{z} {tile:?} read back");
                for r in &back {
                    assert_eq!(gpui_maps::geo::tile_id_at(r.lon, r.lat, z), tile);
                    assert_eq!(r.counts.iter().map(|&c| u64::from(c)).sum::<u64>(), u64::from(r.pop));
                    if r.leaf {
                        assert_eq!((r.blocks, r.expansion_zoom), (1, DOTS_ZOOM));
                    } else {
                        assert!(r.expansion_zoom > z && r.expansion_zoom <= DOTS_ZOOM, "z{z}: {r:?}");
                    }
                    blocks += u64::from(r.blocks);
                    for (g, &c) in groups.iter_mut().zip(&r.counts) {
                        *g += u64::from(c);
                    }
                }
            }
            assert_eq!(groups, want, "z{z}");
            assert_eq!(blocks, pts.len() as u64, "z{z}");
        }
    }

    #[test]
    fn clusters_thin_out_toward_low_zooms_and_leaves_are_the_input_points() {
        let pts = synthetic_block_points(20_000, 11);
        let tiles = build_cluster_tiles(&pts, 3..=11);
        let n = |z| tiles.clusters_at(z).len();
        assert!(n(3) < n(7) && n(7) < n(11), "{} {} {}", n(3), n(7), n(11));
        for leaf in tiles.clusters_at(11).into_iter().filter(|r| r.leaf) {
            assert!(pts.iter().any(|p| f64::from(p.lon_e7) / 1e7 == leaf.lon && f64::from(p.lat_e7) / 1e7 == leaf.lat && p.c == leaf.counts));
        }
    }
}

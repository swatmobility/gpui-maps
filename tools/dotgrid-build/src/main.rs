//! `dotgrid-build`: GHSL GeoTIFF(s) -> a `dotgrid` PMTiles archive.
//!
//! ```text
//! dotgrid-build --tiff <file.tif>... --arcsec 30|3 --epoch 2025 --out <archive.pmtiles>
//!               [--spill-dir DIR] [--report report.json]
//! dotgrid-build --tiff-list <paths.list> [--delete-consumed] [--resume] ...   (3″: see scripts/fetch-ghsl.sh)
//! dotgrid-build --verify <archive.pmtiles> --against-report report.json
//! dotgrid-build --load-clickhouse --tiff <file.tif> --arcsec 30 --epoch 2025
//!               [--table ghs_cells_30ss] [--estimate-mb N] [--out ARCHIVE.pmtiles]
//! ```
//!
//! `--load-clickhouse` (see `clickhouse_load`) streams the same pyramid into
//! `gpui_map_dots.<table>` (at 3″ only level 0; `ghs_cells_3ss`'s levels
//! 1..=9 are 2×2 sums in SQL, and `--estimate-mb` defaults to
//! `ESTIMATE_3SS_MB`) on `CLICKHOUSE_URL` (default
//! `http://localhost:8123`) and exits 3 when `gpui_map_dots` is not its own,
//! 4 when disk is short, 5 when a post-load check fails.
//!
//! Streamed with bounded memory: `tiff_stream` decodes one internal chunk at a
//! time, `bin_spill` appends each populated cell to its level-0 data tile's
//! spill file, `pyramid` builds the coarser levels by 2×2 sums, and `archive`
//! writes the PMTiles file. The grid geometry and the count-tile codec are the
//! engine's own (`gpui_maps::overlay::dotgrid`), and `--verify` opens the
//! result with the engine's reader, so writer and reader cannot drift apart.

mod archive;
mod bin_spill;
mod clickhouse_load;
mod pyramid;
mod sql;
mod tiff_stream;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use gpui_maps::geo::TileId;
use gpui_maps::overlay::dotgrid::{fetch_native, grid, GridCountSource, GridDescriptor, GridRequest, GridTileKey};
use rayon::prelude::*;
use serde_json::{json, Value};

use crate::archive::{EncodedStore, SourceOffsets, StoreSnapshot};
use crate::bin_spill::Spill;
use crate::pyramid::LevelStats;

const ATTRIBUTION: &str = "GHS-POP R2023A, European Commission JRC";
/// In-memory spill buffers, all tiles together.
const SPILL_CAP: usize = 64 << 20;
/// `--tiff-list`: how long to wait for the fetch script's next path.
pub const LIST_IDLE_DEADLINE: Duration = Duration::from_secs(3600);
/// How far two sources' sub-cell offsets may differ and still snap together.
/// GHSL's pixel scale is 3.9e-9 relative off the nominal (the tolerance
/// `tiff_stream` accepts is 1e-8), and each 3″ tile's tiepoint is the
/// world's plus a multiple of that scale, so tiles 348,000 columns apart
/// measured 0.0014 cells apart; 1e-8 x 432,000 columns bounds it at 0.0043.
const RESIDUAL_AGREEMENT_CELLS: f64 = 0.005;
/// The bin pass's checkpoint, in the work directory (see `Checkpoint`).
const CHECKPOINT: &str = "checkpoint.json";

pub struct Options {
    pub tiffs: Vec<PathBuf>,
    pub tiff_list: Option<PathBuf>,
    pub delete_consumed: bool,
    /// Move a raster whose tiepoint is off the grid's lattice north-west onto
    /// the cell boundary at or before it (every cell moves by the residual,
    /// up to just under one cell; see `tiff_stream::grid_offset`) instead of
    /// refusing it. On by default: GHSL's own rasters sit 0.05 of a 30″ cell
    /// and half a 3″ cell off (MG-2 ruling, Task 12).
    pub snap_to_lattice: bool,
    pub arcsec: f64,
    pub epoch: u16,
    pub out: PathBuf,
    pub spill_dir: PathBuf,
    pub spill_cap: usize,
    /// Continue an interrupted run: keep the work directory, skip the sources
    /// its checkpoint lists as binned, and cut every spill file back to its
    /// checkpointed length (dropping a half-binned source's records).
    pub resume: bool,
    /// `--tiff-list`: give up after this long without a new path.
    pub list_idle: Duration,
    /// Refuse the archive write unless the output disk keeps this many MB
    /// free after it (the CLI uses the 10 GB floor; tests 0).
    pub disk_floor_mb: u64,
}

pub fn descriptor(arcsec: f64, epoch: u16) -> GridDescriptor {
    let max_data_zoom = grid::base_data_zoom(arcsec);
    GridDescriptor {
        base_arcsec: arcsec,
        origin_lat: 90.0,
        origin_lon: -180.0,
        // Level `k` bakes into data zoom `max_data_zoom - k`, down to z0.
        levels: max_data_zoom + 1,
        max_data_zoom,
        epochs: vec![epoch],
        attribution: ATTRIBUTION.to_string(),
    }
}

#[derive(Default)]
struct BinTotals {
    sum: f64,
    cells: u64,
    dropped_cells: u64,
    dropped_population: f64,
}

/// Bins one TIFF's populated pixels into level-0 spill files.
fn bin_tiff(
    path: &Path,
    d: &GridDescriptor,
    snap: bool,
    spill: &Mutex<Spill>,
) -> Result<(tiff_stream::GeoInfo, tiff_stream::Offset, BinTotals)> {
    let g = tiff_stream::info(path)?;
    tiff_stream::check_pixel_scale(g.pixel_scale, d.base_arcsec).with_context(|| path.display().to_string())?;
    let off = tiff_stream::grid_offset(&g, d.base_arcsec, d.origin_lon, d.origin_lat, snap)?;
    // Columns wrap at the antimeridian (GHSL's world overhangs it by a cell
    // on each side); rows never do.
    let cols_total = (360.0 * 3600.0 / d.base_arcsec).round() as i64;
    let totals = Mutex::new(BinTotals::default());
    tiff_stream::for_each_populated_chunk(path, |pixels| {
        let mut windows: HashMap<TileId, grid::CellWindow> = HashMap::new();
        let mut by_tile: HashMap<TileId, Vec<(u32, f32)>> = HashMap::new();
        let mut t = BinTotals::default();
        for &(r, c, v) in pixels {
            let row = (r as i64 + off.rows) as u32;
            let col = (c as i64 + off.cols).rem_euclid(cols_total) as u32;
            t.sum += v;
            t.cells += 1;
            match grid::tile_of_cell(d, 0, row, col) {
                None => {
                    t.dropped_cells += 1;
                    t.dropped_population += v;
                }
                Some(tile) => {
                    let w = *windows.entry(tile).or_insert_with(|| grid::cell_window(d, 0, tile));
                    by_tile.entry(tile).or_default().push((grid::local_index(&w, row, col), v as f32));
                }
            }
        }
        {
            let mut sp = spill.lock().unwrap();
            for (tile, recs) in by_tile {
                // An I/O error here is fatal for the run; surface it loudly.
                sp.append(0, tile, &recs).expect("spill append failed");
            }
        }
        let mut all = totals.lock().unwrap();
        all.sum += t.sum;
        all.cells += t.cells;
        all.dropped_cells += t.dropped_cells;
        all.dropped_population += t.dropped_population;
    })?;
    Ok((g, off, totals.into_inner().unwrap()))
}

/// Opens every `--tiff` and runs the same header checks `bin_tiff` will
/// (scale, lattice/snap), without decoding pixels, so a bad input fails
/// before anything is written. `--tiff-list` paths arrive later and are
/// checked as they are consumed.
pub fn validate_inputs(o: &Options) -> Result<()> {
    let d = descriptor(o.arcsec, o.epoch);
    for path in &o.tiffs {
        let g = tiff_stream::info(path).with_context(|| format!("input {}", path.display()))?;
        tiff_stream::check_pixel_scale(g.pixel_scale, d.base_arcsec).with_context(|| path.display().to_string())?;
        tiff_stream::grid_offset(&g, d.base_arcsec, d.origin_lon, d.origin_lat, o.snap_to_lattice)
            .with_context(|| path.display().to_string())?;
    }
    Ok(())
}

/// Yields TIFF paths from a list file as the fetch script appends them,
/// until an `END` line; errors after `idle` without progress.
fn next_listed(list: &Path, consumed: usize, idle: Duration) -> Result<Option<PathBuf>> {
    let started = Instant::now();
    loop {
        let text = std::fs::read_to_string(list).unwrap_or_default();
        // Only whole lines: the script may be mid-append.
        let lines: Vec<&str> = text.split_inclusive('\n').filter(|l| l.ends_with('\n') && !l.trim().is_empty()).collect();
        if let Some(line) = lines.get(consumed) {
            return Ok((line.trim() != "END").then(|| PathBuf::from(line.trim())));
        }
        if started.elapsed() > idle {
            bail!("no new path in {} for {idle:?}", list.display());
        }
        std::thread::sleep(Duration::from_millis(200).min(idle / 4).max(Duration::from_millis(10)));
    }
}

/// What the build has done so far, so a killed run resumes instead of
/// restarting (348 downloads at 3″). Written after every source in the bin
/// pass -- spill files only ever grow by appends, so their lengths at the
/// checkpoint are a consistent cut: anything past them belongs to a source
/// that was not finished -- then once when the bin pass is complete, and
/// after every pyramid level (with the encoded store's length and index).
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Checkpoint {
    /// File names of the sources binned, in order.
    consumed: Vec<String>,
    per_source: BTreeMap<String, f64>,
    sum: f64,
    cells: u64,
    dropped_cells: u64,
    dropped_population: f64,
    grids: Vec<Value>,
    /// The first source's sub-cell offset, and the range over all of them.
    residual: Option<SourceOffsets>,
    /// Level-0 spill files (relative to the work dir) and their lengths.
    spill_files: BTreeMap<String, u64>,
    /// Every source is binned: a resume goes straight to the pyramid.
    #[serde(default)]
    bin_complete: bool,
    /// Pyramid levels finished (their parents' spill is on disk).
    #[serde(default)]
    levels_done: u8,
    #[serde(default)]
    level_stats: Vec<LevelStats>,
    #[serde(default)]
    store: Option<StoreSnapshot>,
}

impl Checkpoint {
    fn load(work: &Path) -> Result<Option<Self>> {
        let path = work.join(CHECKPOINT);
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_slice(&std::fs::read(&path)?).with_context(|| path.display().to_string())?))
    }

    fn save(&mut self, work: &Path, spill: &Mutex<Spill>) -> Result<()> {
        spill.lock().unwrap().finish_level(0)?;
        self.spill_files = spill_lengths(work)?;
        self.write(work)
    }

    fn write(&self, work: &Path) -> Result<()> {
        let tmp = work.join(format!("{CHECKPOINT}.tmp"));
        std::fs::write(&tmp, serde_json::to_vec(self)?)?;
        std::fs::rename(&tmp, work.join(CHECKPOINT))?;
        Ok(())
    }

    /// Cuts the spill back to this checkpoint: in the bin pass, level-0
    /// files to their lengths; in the pyramid, the next level's files go
    /// (a killed level is redone) and finished levels' leftovers too.
    fn restore_spill(&self, work: &Path, levels: u8) -> Result<()> {
        if self.levels_done > 0 || self.bin_complete {
            for level in (0..levels).filter(|&l| l != self.levels_done) {
                let dir = work.join(format!("L{level}"));
                if dir.exists() {
                    std::fs::remove_dir_all(&dir)?;
                }
            }
            if self.levels_done > 0 {
                return Ok(());
            }
        }
        for (name, len) in spill_lengths(work)? {
            let path = work.join(&name);
            match self.spill_files.get(&name) {
                None => std::fs::remove_file(&path)?,
                Some(&want) if want < len => {
                    std::fs::OpenOptions::new().write(true).open(&path)?.set_len(want)?;
                }
                Some(&want) if want > len => bail!("{} is {len} bytes, shorter than its checkpointed {want}", path.display()),
                Some(_) => {}
            }
        }
        Ok(())
    }
}

fn spill_lengths(work: &Path) -> Result<BTreeMap<String, u64>> {
    let mut out = BTreeMap::new();
    let dir = work.join("L0");
    if dir.exists() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            out.insert(format!("L0/{}", entry.file_name().to_string_lossy()), entry.metadata()?.len());
        }
    }
    Ok(out)
}

pub fn build(o: &Options) -> Result<Value> {
    build_with(o, None)
}

/// [`build`], handing every level's cells to `sink` as the pyramid makes them.
pub fn build_with(o: &Options, sink: Option<&pyramid::CellSink>) -> Result<Value> {
    let started = Instant::now();
    let d = descriptor(o.arcsec, o.epoch);
    let stem = o.out.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "out".into());
    let work = o.spill_dir.join(&stem);
    let resumed = if o.resume {
        let Some(cp) = Checkpoint::load(&work)? else {
            bail!(
                "--resume: no checkpoint in {} -- nothing to resume (no source was binned yet, or the build finished \
                 and removed it); rerun without --resume to start over",
                work.display()
            );
        };
        if sink.is_some() && (cp.bin_complete || cp.levels_done > 0) {
            bail!("--resume cannot replay finished pyramid levels into a loader");
        }
        cp.restore_spill(&work, d.levels)?;
        eprintln!(
            "resuming {}: {} source(s) binned{}, {} pyramid level(s) done",
            work.display(),
            cp.consumed.len(),
            if cp.bin_complete { " (bin pass complete)" } else { "" },
            cp.levels_done
        );
        Some(cp)
    } else {
        if work.exists() {
            std::fs::remove_dir_all(&work)?;
        }
        std::fs::create_dir_all(&work)?;
        None
    };
    let spill = Mutex::new(Spill::new(&work, o.spill_cap)?);

    let mut cp = resumed.unwrap_or_default();
    let bin_complete = cp.bin_complete;
    let mut consume = |path: &Path| -> Result<()> {
        let name = path.file_name().context("source has no file name")?.to_string_lossy().to_string();
        if cp.consumed.contains(&name) {
            eprintln!("skipping {}: binned before the interruption", path.display());
            if o.delete_consumed && path.exists() {
                std::fs::remove_file(path)?;
            }
            return Ok(());
        }
        let t0 = Instant::now();
        let (g, off, t) = bin_tiff(path, &d, o.snap_to_lattice, &spill)?;
        eprintln!(
            "binned {}: {} populated cells, {:.0} people, {:.1} s",
            path.display(),
            t.cells,
            t.sum,
            t0.elapsed().as_secs_f64()
        );
        // Every source must sit on the same sub-cell offset, or snapping
        // would shift them by different amounts and tear seams between them.
        match &mut cp.residual {
            None => cp.residual = Some(SourceOffsets::new(off.residual)),
            Some(r)
                if (r.first.0 - off.residual.0).abs() > RESIDUAL_AGREEMENT_CELLS
                    || (r.first.1 - off.residual.1).abs() > RESIDUAL_AGREEMENT_CELLS =>
            {
                bail!("{} sits {:?} cells off the lattice, the first source {:?}", path.display(), off.residual, r.first)
            }
            Some(r) => r.add(off.residual),
        }
        cp.per_source.insert(name.clone(), t.sum);
        cp.sum += t.sum;
        cp.cells += t.cells;
        cp.dropped_cells += t.dropped_cells;
        cp.dropped_population += t.dropped_population;
        cp.grids.push(json!({
            "file": name,
            "width": g.width, "height": g.height,
            "pixel_scale": [g.pixel_scale.0, g.pixel_scale.1],
            "tiepoint": [g.tiepoint.0, g.tiepoint.1],
            "chunk": [g.chunk.0, g.chunk.1], "tiled": g.tiled,
            "grid_offset_cells": [off.rows, off.cols],
            "snap_residual_cells": [off.residual.0, off.residual.1],
        }));
        cp.consumed.push(name);
        cp.save(&work, &spill)?;
        // Deleted only once the checkpoint names it: a kill before this line
        // re-bins the source from its file on resume.
        if o.delete_consumed {
            std::fs::remove_file(path)?;
        }
        Ok(())
    };
    if !bin_complete {
        for path in &o.tiffs {
            consume(path)?;
        }
        if let Some(list) = &o.tiff_list {
            let mut n = 0;
            while let Some(path) = next_listed(list, n, o.list_idle)? {
                consume(&path)?;
                n += 1;
            }
        }
    }
    drop(consume);
    if !bin_complete {
        cp.bin_complete = true;
        cp.save(&work, &spill)?;
    }
    let bin_s = started.elapsed().as_secs_f64();
    let totals = BinTotals { sum: cp.sum, cells: cp.cells, dropped_cells: cp.dropped_cells, dropped_population: cp.dropped_population };

    let encoded = work.join("encoded.bin");
    let store = match (&cp.store, cp.levels_done) {
        (Some(snap), n) if n > 0 => EncodedStore::reopen(&encoded, snap)?,
        _ => EncodedStore::create(&encoded)?,
    };
    let stats = if cp.levels_done > 0 {
        cp.level_stats.clone()
    } else {
        vec![LevelStats { dropped_cells: totals.dropped_cells, dropped_population: totals.dropped_population, ..Default::default() }]
    };
    let levels = pyramid::build(&d, o.epoch, &spill, &store, sink, cp.levels_done, stats, |level, stats| {
        cp.levels_done = level + 1;
        cp.level_stats = stats.to_vec();
        cp.store = Some(store.snapshot()?);
        cp.write(&work)?;
        #[cfg(test)]
        if tests::KILL_AFTER_LEVEL.get() == Some(level) {
            bail!("killed after pyramid level {level} (test)");
        }
        Ok(())
    })?;
    let Checkpoint { consumed, per_source, grids, residual, .. } = cp;
    let tiles = store.len();
    let spill = spill.into_inner().unwrap();
    let offsets = residual.unwrap_or_default();
    let encoded_bytes = store.bytes();
    if o.disk_floor_mb > 0 {
        // The archive is the encoded tiles plus directories; refuse rather
        // than fill the disk (global constraints: >= 10 GB always free).
        let dir = o.out.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let (free_mb, _) = clickhouse_load::df_free_mb(dir)?;
        let need_mb = encoded_bytes.div_ceil(1 << 20) + 64;
        eprintln!("archive write: {free_mb} MB free on {}, need {need_mb} MB + {} MB floor", dir.display(), o.disk_floor_mb);
        if free_mb < need_mb + o.disk_floor_mb {
            return Err(clickhouse_load::refuse(
                3,
                format!("refusing the archive write: {free_mb} MB free < {need_mb} + {} MB", o.disk_floor_mb),
            ));
        }
    }
    let archive_bytes = store.write_archive(&d, &offsets, &o.out)?;
    std::fs::remove_dir_all(&work).ok();

    let per_level = |f: &dyn Fn(&LevelStats) -> Value| -> Vec<Value> { levels.iter().map(f).collect() };
    Ok(json!({
        "grid": {
            "base_arcsec": d.base_arcsec, "origin_lat": d.origin_lat, "origin_lon": d.origin_lon,
            "levels": d.levels, "max_data_zoom": d.max_data_zoom, "epoch": o.epoch,
            "snapped": o.snap_to_lattice,
            // The first source's offset; min/max over every source.
            "source_offset_cells": [offsets.first.0, offsets.first.1],
            "source_offset_cells_min": [offsets.min.0, offsets.min.1],
            "source_offset_cells_max": [offsets.max.0, offsets.max.1],
            "sources": grids,
        },
        "per_source_tile_sum": per_source,
        "sources_binned": consumed.len(),
        "world_sum": totals.sum,
        "source_cells": totals.cells,
        "dropped_beyond_mercator": {
            "per_level_cells": per_level(&|l| json!(l.dropped_cells)),
            "per_level_population": per_level(&|l| json!(l.dropped_population)),
        },
        "cells_per_level": per_level(&|l| json!(l.cells)),
        "cells_encoded_per_level": per_level(&|l| json!(l.cells_encoded)),
        "sum_per_level": per_level(&|l| json!(l.sum)),
        "sum_encoded_per_level": per_level(&|l| json!(l.sum_encoded)),
        "tiles_per_level": per_level(&|l| json!(l.tiles)),
        "max_cells_in_a_tile_per_level": per_level(&|l| json!(l.max_cells_in_a_tile)),
        "max_window_cells_per_level": per_level(&|l| json!(l.max_window_cells)),
        "tile_bytes_per_level": per_level(&|l| json!(l.bytes)),
        "archive_tiles": tiles,
        "archive_bytes": archive_bytes,
        "spill_peak_buffer_bytes": spill.peak_held,
        "spill_bytes_written": spill.bytes_written,
        "bin_s": bin_s,
        "wall_s": started.elapsed().as_secs_f64(),
    }))
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Decoded {
    pub tiles: u64,
    pub cells: u64,
    pub sum: f64,
    pub max_cells_in_a_tile: u64,
}

/// Opens `archive` with the engine's own reader and decodes every tile at
/// every level.
pub fn read_back(archive: &Path) -> Result<(GridDescriptor, Vec<Decoded>)> {
    let source = fetch_native::open_pmtiles_grid(&archive.to_string_lossy())
        .map_err(|e| anyhow::anyhow!("open {}: {e}", archive.display()))?;
    let d = source.descriptor().clone();
    let epoch = *d.epochs.first().context("archive names no epoch")?;
    let mut out = Vec::new();
    for level in 0..d.levels {
        let z = grid::data_zoom(&d, level);
        let n = 1u32 << z;
        let rows: Vec<Decoded> = (0..n)
            .into_par_iter()
            .map(|y| -> Result<Decoded> {
                let mut acc = Decoded::default();
                for x in 0..n {
                    let key = GridTileKey { tile: TileId { z, x, y }, level, epoch, epoch_b: None };
                    let req = source.request(&[key]);
                    if req == GridRequest::Absent {
                        continue;
                    }
                    let body = fetch_native::execute(&req).map_err(|e| anyhow::anyhow!("{key:?}: {e}"))?;
                    for (_, tile) in source.decode(&[key], &body).map_err(|e| anyhow::anyhow!("{key:?}: {e}"))? {
                        acc.tiles += 1;
                        acc.cells += tile.cells.len() as u64;
                        acc.sum += tile.total;
                        acc.max_cells_in_a_tile = acc.max_cells_in_a_tile.max(tile.cells.len() as u64);
                    }
                }
                Ok(acc)
            })
            .collect::<Result<_>>()?;
        out.push(rows.into_iter().fold(Decoded::default(), |a, b| Decoded {
            tiles: a.tiles + b.tiles,
            cells: a.cells + b.cells,
            sum: a.sum + b.sum,
            max_cells_in_a_tile: a.max_cells_in_a_tile.max(b.max_cells_in_a_tile),
        }));
    }
    Ok((d, out))
}

/// `--verify`: every level's decoded tile count, cell count and sum must
/// match what the build reported.
pub fn verify(archive: &Path, report: &Value) -> Result<()> {
    let (d, got) = read_back(archive)?;
    let g = &report["grid"];
    if g["base_arcsec"].as_f64() != Some(d.base_arcsec) || g["levels"].as_u64() != Some(d.levels as u64) {
        bail!("archive grid {d:?} does not match the report's {g}");
    }
    let mut bad = Vec::new();
    for (level, l) in got.iter().enumerate() {
        let want_tiles = report["tiles_per_level"][level].as_u64().unwrap_or(u64::MAX);
        let want_cells = report["cells_encoded_per_level"][level].as_u64().unwrap_or(u64::MAX);
        let want_sum = report["sum_encoded_per_level"][level].as_f64().unwrap_or(f64::NAN);
        let want_max = report["max_cells_in_a_tile_per_level"][level].as_u64().unwrap_or(u64::MAX);
        let sum_ok = (l.sum - want_sum).abs() <= 1e-9 * want_sum.abs().max(1.0);
        eprintln!(
            "level {level}: {} tiles, {} cells, max {} per tile, {:.1} people (report {want_tiles}, {want_cells}, {want_max}, {want_sum:.1})",
            l.tiles, l.cells, l.max_cells_in_a_tile, l.sum
        );
        // Tiles whose every cell rounds to 0 are never written, so the
        // archive may hold fewer tiles than the pyramid visited.
        if l.tiles > want_tiles || l.cells != want_cells || l.max_cells_in_a_tile != want_max || !sum_ok {
            bad.push(level);
        }
    }
    if !bad.is_empty() {
        bail!("levels {bad:?} do not match the report");
    }
    Ok(())
}

fn usage() -> ! {
    eprintln!(
        "usage: dotgrid-build --tiff <file.tif>... | --tiff-list FILE [--delete-consumed] [--resume] [--no-snap] --arcsec 30|3 --epoch YEAR \
         --out ARCHIVE.pmtiles [--spill-dir DIR] [--report REPORT.json]\n       \
         dotgrid-build --verify ARCHIVE.pmtiles --against-report REPORT.json\n       \
         dotgrid-build --load-clickhouse --tiff <file.tif>... --arcsec 30|3 --epoch YEAR [--table T] [--estimate-mb N] [--out ARCHIVE.pmtiles]"
    );
    std::process::exit(2)
}

fn main() {
    if let Err(e) = run() {
        eprintln!("Error: {e:#}");
        let code = e.downcast_ref::<clickhouse_load::Refusal>().map_or(1, |r| r.code);
        std::process::exit(code);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut tiffs = Vec::new();
    let (mut tiff_list, mut arcsec, mut epoch, mut out, mut spill_dir, mut report, mut verify_path, mut against) =
        (None, None, None, None, None, None, None, None);
    let mut delete_consumed = false;
    let mut resume = false;
    let mut snap_to_lattice = true;
    let (mut load_clickhouse, mut table, mut estimate_mb) = (false, None, None);
    let mut i = 0;
    let value = |i: usize| args.get(i + 1).cloned().unwrap_or_else(|| usage());
    while i < args.len() {
        match args[i].as_str() {
            "--tiff" => {
                while i + 1 < args.len() && !args[i + 1].starts_with("--") {
                    tiffs.push(PathBuf::from(&args[i + 1]));
                    i += 1;
                }
                i += 1;
                continue;
            }
            "--tiff-list" => tiff_list = Some(PathBuf::from(value(i))),
            "--delete-consumed" => {
                delete_consumed = true;
                i += 1;
                continue;
            }
            "--resume" => {
                resume = true;
                i += 1;
                continue;
            }
            "--load-clickhouse" => {
                load_clickhouse = true;
                i += 1;
                continue;
            }
            "--table" => table = Some(value(i)),
            "--estimate-mb" => estimate_mb = Some(value(i).parse::<u64>().unwrap_or_else(|_| usage())),
            "--no-snap" => {
                snap_to_lattice = false;
                i += 1;
                continue;
            }
            "--arcsec" => arcsec = Some(value(i)),
            "--epoch" => epoch = Some(value(i)),
            "--out" => out = Some(PathBuf::from(value(i))),
            "--spill-dir" => spill_dir = Some(PathBuf::from(value(i))),
            "--report" => report = Some(PathBuf::from(value(i))),
            "--verify" => verify_path = Some(PathBuf::from(value(i))),
            "--against-report" => against = Some(PathBuf::from(value(i))),
            _ => usage(),
        }
        i += 2;
    }

    if let Some(archive) = verify_path {
        let against = against.unwrap_or_else(|| usage());
        let report: Value = serde_json::from_slice(&std::fs::read(&against)?)?;
        verify(&archive, &report)?;
        eprintln!("verify: {} matches {}", archive.display(), against.display());
        return Ok(());
    }

    let arcsec: f64 = match arcsec.as_deref() {
        Some("30") => 30.0,
        Some("3") => 3.0,
        _ => usage(),
    };
    let epoch: u16 = epoch.and_then(|e| e.parse().ok()).unwrap_or_else(|| usage());
    if tiffs.is_empty() && tiff_list.is_none() {
        usage();
    }
    let home = std::env::var("HOME").context("HOME")?;
    let spill_dir = spill_dir.unwrap_or_else(|| PathBuf::from(&home).join(".cache/gpui-map-data/spill"));
    // A load without `--out` still builds the archive (its report is what
    // the table is checked against), into the spill directory, then drops it.
    let scratch_out = (load_clickhouse && out.is_none()).then(|| spill_dir.join(format!("load-E{epoch}.pmtiles")));
    let out = out.or(scratch_out.clone()).unwrap_or_else(|| usage());
    let o = Options {
        tiffs,
        tiff_list,
        delete_consumed,
        snap_to_lattice,
        arcsec,
        epoch,
        out,
        spill_dir,
        spill_cap: SPILL_CAP,
        resume,
        list_idle: LIST_IDLE_DEADLINE,
        disk_floor_mb: clickhouse_load::DISK_FLOOR_MB,
    };
    if load_clickhouse {
        let table = table.unwrap_or_else(|| format!("ghs_cells_{}ss", arcsec as u32));
        let want = format!("ghs_cells_{}ss", arcsec as u32);
        if table != want {
            bail!("--table {table} does not hold --arcsec {arcsec} cells; it must be {want}");
        }
        let estimate_mb = match (estimate_mb, arcsec == 30.0) {
            (Some(mb), _) => mb,
            (None, true) => clickhouse_load::ESTIMATE_30SS_MB,
            (None, false) => clickhouse_load::ESTIMATE_3SS_MB,
        };
        let ch = clickhouse_load::Ch::from_env()?;
        let result = clickhouse_load::load(&ch, &clickhouse_load::LoadOptions { build: o, table, estimate_mb, sql_levels: arcsec == 3.0 });
        if let Some(p) = scratch_out {
            std::fs::remove_file(p).ok();
        }
        if let (Ok(r), Some(path)) = (&result, report) {
            std::fs::write(path, serde_json::to_string_pretty(r)?)?;
        }
        return result.map(|_| ());
    }
    let r = build(&o)?;
    let text = serde_json::to_string_pretty(&r)?;
    if let Some(path) = report {
        std::fs::write(&path, &text)?;
    }
    println!("{text}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        /// Makes `build` fail right after checkpointing this pyramid level,
        /// as a kill there would.
        pub(crate) static KILL_AFTER_LEVEL: std::cell::Cell<Option<u8>> = const { std::cell::Cell::new(None) };
    }

    #[test]
    fn a_synthetic_tiff_converts_with_sums_conserved() {
        let dir = tempfile::tempdir().unwrap();
        let tif = dir.path().join("synthetic.tif");
        // 3600×1800 at 0.1° (360″) from (-180, 90), populated within ±80°
        // only, so no level's cells reach the Mercator limit.
        let (w, h) = (3600u32, 1800u32);
        let mut s = 0x2545_f491u32;
        let data: Vec<f64> = (0..w * h)
            .map(|k| {
                s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let lat = 90.0 - ((k / w) as f64 + 0.5) * 0.1;
                if lat.abs() < 80.0 && s % 5 == 0 {
                    (s >> 16) as f64 / 97.0
                } else if s % 7 == 0 {
                    -200.0
                } else {
                    0.0
                }
            })
            .collect();
        let source_sum: f64 = data.iter().filter(|v| **v > 0.0).sum();
        let source_cells = data.iter().filter(|v| **v > 0.0).count() as f64;
        tiff_stream::tests::write_geotiff(&tif, w, h, 0.1, (-180.0, 90.0), &data);

        let out = dir.path().join("synthetic.pmtiles");
        let o = Options {
            tiffs: vec![tif],
            tiff_list: None,
            delete_consumed: false,
            snap_to_lattice: false,
            arcsec: 360.0,
            epoch: 2025,
            out: out.clone(),
            spill_dir: dir.path().join("spill"),
            spill_cap: 1 << 20,
            resume: false,
            list_idle: LIST_IDLE_DEADLINE,
            disk_floor_mb: 0,
        };
        let report = build(&o).unwrap();
        assert!((report["world_sum"].as_f64().unwrap() - source_sum).abs() < 1e-6 * source_sum);
        assert!(report["spill_peak_buffer_bytes"].as_u64().unwrap() <= 1 << 20);

        let (d, levels) = read_back(&out).unwrap();
        assert_eq!(d.levels, grid::base_data_zoom(360.0) + 1);
        assert_eq!(d.attribution, ATTRIBUTION);
        for (level, l) in levels.iter().enumerate() {
            assert!(l.tiles > 0, "level {level} is empty");
            assert!(
                (l.sum - source_sum).abs() <= 0.05 * source_cells,
                "level {level}: {} vs source {source_sum}",
                l.sum
            );
        }
        verify(&out, &report).unwrap();
    }

    #[test]
    fn an_off_lattice_raster_snaps_onto_the_grid_and_wraps_the_antimeridian() {
        // GHSL's own geometry in miniature: 0.1° (360″) cells, the tiepoint
        // 0.95 of a cell west of 180°W and 108.05 cells south of 90°N, and
        // one column more than the world so the first overhangs the seam.
        let dir = tempfile::tempdir().unwrap();
        let tif = dir.path().join("offset.tif");
        let cell = 0.1;
        let (w, h) = (3601u32, 1200u32);
        let tie = (-180.0 - 0.95 * cell, 90.0 - 108.05 * cell);
        let pixels = [(0u32, 0u32, 7.0), (0, 1, 2.5), (500, 3600, 4.0), (1199, 1800, 11.0), (600, 2000, 0.3)];
        let mut data = vec![0.0f64; (w * h) as usize];
        for &(r, c, v) in &pixels {
            data[(r * w + c) as usize] = v;
        }
        tiff_stream::tests::write_geotiff(&tif, w, h, cell, tie, &data);

        let out = dir.path().join("offset.pmtiles");
        let o = Options {
            tiffs: vec![tif],
            tiff_list: None,
            delete_consumed: false,
            snap_to_lattice: true,
            arcsec: 360.0,
            epoch: 2025,
            out: out.clone(),
            spill_dir: dir.path().join("spill"),
            spill_cap: 1 << 20,
            resume: false,
            list_idle: LIST_IDLE_DEADLINE,
            disk_floor_mb: 0,
        };
        let report = build(&o).unwrap();
        let offset = &report["grid"]["source_offset_cells"];
        assert!((offset[0].as_f64().unwrap() - 0.05).abs() < 1e-6, "{offset}");
        assert!((offset[1].as_f64().unwrap() - 0.05).abs() < 1e-6, "{offset}");

        // The archive's own metadata carries the offset too.
        let bytes = std::fs::read(&out).unwrap();
        let header = gpui_maps::overlay::dotgrid::pmtiles_dir::parse_header(&bytes[..127]).unwrap();
        let meta = &bytes[header.metadata_offset as usize..(header.metadata_offset + header.metadata_length) as usize];
        let meta: Value = serde_json::from_slice(
            &gpui_maps::overlay::dotgrid::pmtiles_dir::decompress(meta, header.internal_compression).unwrap(),
        )
        .unwrap();
        assert!((meta["dotgrid"]["source_offset_cells"][1].as_f64().unwrap() - 0.05).abs() < 1e-6, "{meta}");

        // Level-0 cells land at raster (row + 108, col - 1), columns wrapped.
        let source = fetch_native::open_pmtiles_grid(&out.to_string_lossy()).unwrap();
        let d = source.descriptor().clone();
        let z = grid::data_zoom(&d, 0);
        let mut got = Vec::new();
        for y in 0..1u32 << z {
            for x in 0..1u32 << z {
                let key = GridTileKey { tile: TileId { z, x, y }, level: 0, epoch: 2025, epoch_b: None };
                let req = source.request(&[key]);
                if req == GridRequest::Absent {
                    continue;
                }
                let body = fetch_native::execute(&req).unwrap();
                let w = grid::cell_window(&d, 0, key.tile);
                for (_, tile) in source.decode(&[key], &body).unwrap() {
                    for (idx, pop, _) in tile.cells {
                        let (r, c) = grid::row_col(&w, idx);
                        got.push((r, c, pop));
                    }
                }
            }
        }
        got.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
        let mut want: Vec<(u32, u32, f32)> =
            pixels.iter().map(|&(r, c, v)| (r + 108, (c as i64 - 1).rem_euclid(3600) as u32, v as f32)).collect();
        want.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
        // Raster col 0 wraps to the grid's last column, 3599 -- the far
        // east edge tile, not a world-spanning sliver -- and col 3600 to 3599.
        assert!(want.iter().any(|c| c.1 == 3599) && want.iter().any(|c| c.1 == 0));
        assert_eq!(got, want);
        let total: f64 = pixels.iter().map(|p| p.2).sum();
        assert!((report["world_sum"].as_f64().unwrap() - total).abs() < 1e-9);
        for level in 0..d.levels as usize {
            let s = report["sum_encoded_per_level"][level].as_f64().unwrap();
            assert!((s - total).abs() < 1e-6, "level {level}: {s} vs {total}");
        }
        verify(&out, &report).unwrap();

        // Opting out refuses the same raster.
        let refused = build(&Options { snap_to_lattice: false, out: dir.path().join("no.pmtiles"), ..o });
        assert!(refused.is_err());
    }

    /// A small float64 GeoTIFF at `arcsec` whose populated pixels are
    /// `(row, col, value)`; the tiepoint is GHSL's own 1.5″ south-east of a
    /// cell boundary at (`lon0`, `lat0`), as its real tiles are.
    fn raster(path: &Path, arcsec: f64, w: u32, h: u32, lon0: f64, lat0: f64, pixels: &[(u32, u32, f64)]) {
        let mut data = vec![0.0f64; (w * h) as usize];
        for &(r, c, v) in pixels {
            data[(r * w + c) as usize] += v;
        }
        let tie = (lon0 + 1.5 / 3600.0, lat0 - 1.5 / 3600.0);
        tiff_stream::tests::write_geotiff(path, w, h, arcsec / 3600.0, tie, &data);
    }

    fn opts(dir: &Path, arcsec: f64, out: &str) -> Options {
        Options {
            tiffs: vec![],
            tiff_list: None,
            delete_consumed: false,
            snap_to_lattice: true,
            arcsec,
            epoch: 2025,
            out: dir.join(out),
            spill_dir: dir.join("spill"),
            spill_cap: 1 << 20,
            resume: false,
            list_idle: LIST_IDLE_DEADLINE,
            disk_floor_mb: 0,
        }
    }

    fn pseudo_pixels(seed: u32, w: u32, h: u32, n: usize) -> Vec<(u32, u32, f64)> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let r = (s >> 8) % h;
                s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((r, (s >> 8) % w, ((s >> 20) % 400) as f64 / 7.0))
            })
            .collect()
    }

    #[test]
    fn the_tiff_list_is_consumed_incrementally() {
        let dir = tempfile::tempdir().unwrap();
        let tifs: Vec<PathBuf> = (0..3).map(|i| dir.path().join(format!("t{i}.tif"))).collect();
        let mut want = 0.0;
        for (i, t) in tifs.iter().enumerate() {
            // Three 1°×1° 3″ tiles side by side along the equator.
            let px = pseudo_pixels(11 + i as u32, 1200, 1200, 3000);
            want += px.iter().map(|p| p.2).sum::<f64>();
            raster(t, 3.0, 1200, 1200, 20.0 + i as f64, 1.0, &px);
        }
        let list = dir.path().join("in.list");
        std::fs::write(&list, format!("{}\n", tifs[0].display())).unwrap();
        // A bounded idle wait, so a failing assertion ends the test instead
        // of leaving the build waiting an hour for the list's next line.
        let o = Options {
            tiff_list: Some(list.clone()),
            delete_consumed: true,
            list_idle: Duration::from_secs(20),
            ..opts(dir.path(), 3.0, "a.pmtiles")
        };

        let report = std::thread::scope(|scope| {
            let job = scope.spawn(|| build(&o));
            // The first is binned and deleted while the list is still open.
            let t0 = Instant::now();
            while tifs[0].exists() {
                assert!(t0.elapsed() < Duration::from_secs(60), "t0 never consumed");
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(tifs[1].exists() && !job.is_finished());
            let mut f = std::fs::OpenOptions::new().append(true).open(&list).unwrap();
            use std::io::Write;
            // A half-written line is not read as a path.
            write!(f, "{}", tifs[1].display()).unwrap();
            f.flush().unwrap();
            std::thread::sleep(Duration::from_millis(500));
            assert!(tifs[1].exists());
            writeln!(f).unwrap();
            writeln!(f, "{}\nEND", tifs[2].display()).unwrap();
            job.join().unwrap().unwrap()
        });
        assert!(tifs.iter().all(|t| !t.exists()), "every input is deleted once binned");
        assert_eq!(report["sources_binned"].as_u64(), Some(3));
        assert!((report["world_sum"].as_f64().unwrap() - want).abs() < 1e-6 * want);
        verify(&o.out, &report).unwrap();
    }

    #[test]
    fn an_interrupted_list_build_resumes_where_it_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let tifs: Vec<PathBuf> = (0..3).map(|i| dir.path().join(format!("t{i}.tif"))).collect();
        let pixels: Vec<_> = (0..3).map(|i| pseudo_pixels(40 + i, 1200, 1200, 5000)).collect();
        let write_all = || {
            for (i, t) in tifs.iter().enumerate() {
                raster(t, 3.0, 1200, 1200, -60.0 + i as f64, -34.0, &pixels[i]);
            }
        };
        // The reference: one uninterrupted run.
        write_all();
        let whole = build(&Options { tiffs: tifs.clone(), ..opts(dir.path(), 3.0, "whole.pmtiles") }).unwrap();

        // A run that dies after two sources (the list goes quiet).
        let list = dir.path().join("in.list");
        std::fs::write(&list, format!("{}\n{}\n", tifs[0].display(), tifs[1].display())).unwrap();
        let o = Options {
            tiff_list: Some(list.clone()),
            delete_consumed: true,
            list_idle: Duration::from_millis(300),
            ..opts(dir.path(), 3.0, "resumed.pmtiles")
        };
        assert!(build(&o).is_err());
        assert!(!tifs[0].exists() && !tifs[1].exists() && tifs[2].exists());
        // ... and was killed half way into a third: stray records past the
        // checkpoint in an existing spill file and in a new one.
        let l0 = dir.path().join("spill/resumed/L0");
        let first = std::fs::read_dir(&l0).unwrap().next().unwrap().unwrap().path();
        let mut f = std::fs::OpenOptions::new().append(true).open(&first).unwrap();
        use std::io::Write;
        f.write_all(&[7u8; 8 * 100]).unwrap();
        std::fs::write(l0.join("9_1_1.bin"), [9u8; 80]).unwrap();

        std::fs::write(&list, format!("{}\n{}\n{}\nEND\n", tifs[0].display(), tifs[1].display(), tifs[2].display())).unwrap();
        let resumed = build(&Options { resume: true, list_idle: LIST_IDLE_DEADLINE, ..o }).unwrap();
        assert!(!tifs[2].exists());
        assert_eq!(resumed["sources_binned"].as_u64(), Some(3));
        for key in ["cells_per_level", "cells_encoded_per_level", "tiles_per_level"] {
            assert_eq!(resumed[key], whole[key], "{key}");
        }
        // Sums: chunks are binned in parallel, so f64 addition order (and
        // the last bits) varies from run to run.
        let close = |a: &Value, b: &Value, what: &str| {
            let (a, b) = (a.as_f64().unwrap(), b.as_f64().unwrap());
            assert!((a - b).abs() <= 1e-9 * b.abs().max(1.0), "{what}: {a} vs {b}");
        };
        close(&resumed["world_sum"], &whole["world_sum"], "world_sum");
        for level in 0..10 {
            close(&resumed["sum_encoded_per_level"][level], &whole["sum_encoded_per_level"][level], "sum_encoded_per_level");
        }
        for t in &tifs {
            let name = t.file_name().unwrap().to_str().unwrap();
            close(&resumed["per_source_tile_sum"][name], &whole["per_source_tile_sum"][name], name);
        }
        for key in ["source_offset_cells", "source_offset_cells_min", "source_offset_cells_max"] {
            assert_eq!(resumed["grid"][key], whole["grid"][key], "{key}");
        }
        verify(&dir.path().join("resumed.pmtiles"), &resumed).unwrap();
    }

    #[test]
    fn a_kill_during_the_pyramid_resumes_at_a_level_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let tifs: Vec<PathBuf> = (0..2).map(|i| dir.path().join(format!("p{i}.tif"))).collect();
        let pixels: Vec<_> = (0..2).map(|i| pseudo_pixels(70 + i, 1200, 1200, 5000)).collect();
        for (i, t) in tifs.iter().enumerate() {
            raster(t, 3.0, 1200, 1200, 30.0 + i as f64, 10.0, &pixels[i]);
        }
        let whole = build(&Options { tiffs: tifs.clone(), ..opts(dir.path(), 3.0, "whole.pmtiles") }).unwrap();

        let o = Options { tiffs: tifs.clone(), delete_consumed: true, ..opts(dir.path(), 3.0, "killed.pmtiles") };
        KILL_AFTER_LEVEL.set(Some(2));
        let killed = build(&o);
        KILL_AFTER_LEVEL.set(None);
        assert!(killed.unwrap_err().to_string().contains("killed after pyramid level 2"));
        // The inputs are gone: a resume must not need them.
        assert!(tifs.iter().all(|t| !t.exists()));
        let work = dir.path().join("spill/killed");
        let cp = Checkpoint::load(&work).unwrap().unwrap();
        assert!(cp.bin_complete && cp.levels_done == 3, "{} {}", cp.bin_complete, cp.levels_done);
        assert!(!work.join("L0").exists() && work.join("L3").exists());
        // ... as if killed half way through level 3: a partial level-4 file,
        // and encoded bytes past the checkpoint.
        std::fs::create_dir_all(work.join("L4")).unwrap();
        std::fs::write(work.join("L4/5_1_1.bin"), [3u8; 64]).unwrap();
        let mut f = std::fs::OpenOptions::new().append(true).open(work.join("encoded.bin")).unwrap();
        use std::io::Write;
        f.write_all(&[0xAB; 999]).unwrap();

        let resumed = build(&Options { resume: true, ..o }).unwrap();
        for key in ["cells_per_level", "cells_encoded_per_level", "tiles_per_level", "tile_bytes_per_level", "archive_tiles"] {
            assert_eq!(resumed[key], whole[key], "{key}");
        }
        for level in 0..10 {
            let (a, b) = (resumed["sum_encoded_per_level"][level].as_f64().unwrap(), whole["sum_encoded_per_level"][level].as_f64().unwrap());
            assert!((a - b).abs() <= 1e-9 * b.max(1.0), "level {level}: {a} vs {b}");
        }
        verify(&dir.path().join("killed.pmtiles"), &resumed).unwrap();
        assert!(!work.exists(), "a finished build removes its work dir");
    }

    #[test]
    fn resume_without_a_checkpoint_refuses_and_keeps_the_work_dir() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("spill/nothing");
        std::fs::create_dir_all(work.join("L0")).unwrap();
        std::fs::write(work.join("L0/keep.bin"), [1u8; 8]).unwrap();
        let err = build(&Options { resume: true, tiffs: vec![dir.path().join("x.tif")], ..opts(dir.path(), 3.0, "nothing.pmtiles") })
            .unwrap_err()
            .to_string();
        assert!(err.contains("no checkpoint") && err.contains("without --resume"), "{err}");
        assert!(work.join("L0/keep.bin").exists());
    }

    #[test]
    fn a_3s_synthetic_tile_matches_its_30s_aggregate() {
        // 1200×1200 3″ (1°×1°) and its 10×10 block sum as a 120×120 30″
        // raster, both on GHSL's 1.5″-offset source lattice.
        let dir = tempfile::tempdir().unwrap();
        let fine = pseudo_pixels(5, 1200, 1200, 40_000);
        let coarse: Vec<(u32, u32, f64)> = fine.iter().map(|&(r, c, v)| (r / 10, c / 10, v)).collect();
        let cells = {
            let mut k: Vec<(u32, u32)> = fine.iter().map(|p| (p.0, p.1)).collect();
            k.sort_unstable();
            k.dedup();
            k.len() as f64
        };
        let (t3, t30) = (dir.path().join("f.tif"), dir.path().join("c.tif"));
        raster(&t3, 3.0, 1200, 1200, 90.0, 24.0, &fine);
        raster(&t30, 30.0, 120, 120, 90.0, 24.0, &coarse);
        let r3 = build(&Options { tiffs: vec![t3], ..opts(dir.path(), 3.0, "f.pmtiles") }).unwrap();
        let r30 = build(&Options { tiffs: vec![t30], ..opts(dir.path(), 30.0, "c.pmtiles") }).unwrap();
        let (s3, s30) = (r3["world_sum"].as_f64().unwrap(), r30["world_sum"].as_f64().unwrap());
        assert!((s3 - s30).abs() <= 0.05 * cells, "{s3} vs {s30}");
        // Both snapped by the same 1.5″ (half a 3″ cell, a twentieth of 30″).
        let off3 = r3["grid"]["source_offset_cells"][1].as_f64().unwrap() * 3.0;
        let off30 = r30["grid"]["source_offset_cells"][1].as_f64().unwrap() * 30.0;
        assert!((off3 - 1.5).abs() < 1e-3 && (off30 - 1.5).abs() < 1e-3, "{off3} {off30}");
        let (_, l3) = read_back(&dir.path().join("f.pmtiles")).unwrap();
        let (_, l30) = read_back(&dir.path().join("c.pmtiles")).unwrap();
        for l in l3.iter().chain(&l30) {
            assert!((l.sum - s30).abs() <= 0.05 * cells, "{} vs {s30}", l.sum);
        }
    }

    #[test]
    fn fetch_refuses_without_disk() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let shim = |name: &str, body: &str| {
            let p = bin.join(name);
            std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        // 1 GB free, and a JRC listing of one 300 MB zip -- no network.
        shim("df", "echo 'Filesystem 1M-blocks Used Available Capacity Mounted'; echo '/dev/x 500000 499000 1024 99% /'");
        shim(
            "curl",
            r#"case " $* " in *" -fsSI "*|*" -I "*) printf 'HTTP/1.1 200 OK\r\nContent-Length: 314572800\r\n\r\n';; *" -o "*) echo 'curl shim: no downloads' >&2; exit 99;; *) echo '<a href="X_R1_C1.zip">X_R1_C1.zip</a>';; esac"#,
        );
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/fetch-ghsl.sh");
        let out = std::process::Command::new("bash")
            .arg(&script)
            .args(["2025", "3ss"])
            .env("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()))
            .env("GPUI_MAP_DATA", dir.path().join("data"))
            .env("DOTGRID_BUILD", "/usr/bin/true")
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(3), "{err}");
        assert!(err.contains("1024 MB free"), "{err}");
    }
}

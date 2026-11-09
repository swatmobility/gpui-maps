//! `race-build export-hex` (Task 10, spec §4.2, MG-R12's first half):
//! `gpui_map_dots.race_hex_2020` (Task 9) into `race-hex-2020.pmtiles`.
//!
//! - Read-only: three enumerated `SELECT`s from `sql` ([`sql::HEX_EXPORT_COUNTS`],
//!   [`sql::HEX_EXPORT_ROWS`], [`sql::HEX_BOUNDARY_SAMPLE`]), covered by
//!   the `sql_guard` test with every other template.
//! - Each hexagon's boundary is **baked in** here, by `h3o` (the engine's
//!   `hexes::record_for_cell`, the one definition the native ClickHouse
//!   source uses too), so the shells need no `h3o`; a 1-in-500 sample is
//!   cross-checked against ClickHouse's own `h3ToGeoBoundary`.
//! - A hexagon goes to the tile holding its `h3o` centre at its
//!   resolution's `HEX_TILE_ZOOM`. Hexagons under 10 people are not
//!   written (Walker's filter): `codec::encode_tile` folds their people
//!   into the tile's hidden counts, so the archive still carries all
//!   331,449,281 people of every resolution.
//! - Checked twice: the rows' sums against the P.L. totals before
//!   writing, and every tile read back through the engine's own opener
//!   and decoder after, per resolution and group (hidden people
//!   included), with the drawn hexagons counted against ClickHouse's
//!   `countIf(total >= 10)`.
//! - Disk: the archive is estimated before anything is written (40 B a
//!   drawn hexagon, above the codec's measured ~12-30 B), and the export
//!   refuses (exit 4) unless the estimate plus 10 GB is free; it stops if
//!   free space falls under 5 GB while writing.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use gpui_maps::geo::TileId;
use gpui_maps::overlay::dotgrid::GridRequest;
use gpui_maps::overlay::hexes::{self, codec, source, HexRecord, GROUPS, HEX_MIN_TOTAL, HEX_TILE_ZOOM};
use serde_json::{json, Value};

use crate::clickhouse_load::{df_free_mb, refuse, Ch};
use crate::pl94::{US_GROUP_TOTALS, US_TOTAL};
use crate::sql;

/// MG-R12's archive budget.
pub const ARCHIVE_BUDGET_BYTES: u64 = 100_000_000;
/// The per-drawn-hexagon size the disk estimate assumes.
const ESTIMATE_BYTES_PER_HEX: u64 = 40;
const HEADROOM_MB: u64 = 10 * 1024;
const FLOOR_MB: u64 = 5 * 1024;
const ATTRIBUTION: &str = "U.S. Census Bureau, 2020 Census P.L. 94-171";

/// The record of cell `h3` with `counts`, its boundary baked by `h3o`.
///
/// # Panics
/// If `h3` is not an H3 cell (the table only holds `geoToH3` output).
pub fn record_for(h3: u64, counts: [u32; GROUPS]) -> HexRecord {
    hexes::record_for_cell(h3, counts).expect("race_hex_2020 holds H3 cells")
}

/// One resolution's tiles, encoded, in tile-id order.
pub struct ResTiles {
    pub tiles: Vec<(TileId, Vec<u8>)>,
    /// Hexagons written (>= 10 people).
    pub drawn: u64,
    /// Hexagons folded into hidden counts (< 10 people).
    pub hidden_hexes: u64,
    /// Drawn hexagons whose boundary has more than 6 vertices (C18).
    pub over_six: u64,
    pub max_per_tile: usize,
}

/// Groups `rows` (one resolution's cells and counts) into tiles by their
/// `h3o` centre and encodes each. Only drawn hexagons get a boundary.
pub fn encode_res(res: u8, rows: &[(u64, [u32; GROUPS])]) -> Result<ResTiles> {
    let mut by_tile: BTreeMap<u64, (TileId, Vec<HexRecord>)> = BTreeMap::new();
    let (mut drawn, mut hidden_hexes, mut over_six) = (0u64, 0u64, 0u64);
    for &(h3, counts) in rows {
        let (lon, lat) = hexes::cell_centre(h3).map_err(|e| anyhow::anyhow!("{e}"))?;
        let tile = hexes::owning_tile(res, lon, lat).context("a resolution the map does not draw")?;
        let total: u64 = counts.iter().map(|&c| u64::from(c)).sum();
        let rec = if total >= u64::from(HEX_MIN_TOTAL) {
            drawn += 1;
            let r = record_for(h3, counts);
            if r.res != res {
                bail!("{h3:#x} is r{}, in the r{res} rows", r.res);
            }
            over_six += u64::from(r.boundary.len() > 6);
            r
        } else {
            hidden_hexes += 1;
            HexRecord { res, h3, counts, boundary: Vec::new() }
        };
        let id = gpui_maps::overlay::dotgrid::pmtiles_dir::zxy_to_tile_id(tile.z, tile.x, tile.y);
        by_tile.entry(id).or_insert_with(|| (tile, Vec::new())).1.push(rec);
    }
    let max_per_tile = by_tile.values().map(|(_, v)| v.len()).max().unwrap_or(0);
    let tiles = by_tile.into_values().map(|(t, recs)| (t, codec::encode_tile(res, t, &recs))).collect();
    Ok(ResTiles { tiles, drawn, hidden_hexes, over_six, max_per_tile })
}

/// RowBinary `(h3 UInt64, c1..c8 UInt32)` rows.
fn parse_export_rows(body: &[u8]) -> Result<Vec<(u64, [u32; GROUPS])>> {
    if !body.len().is_multiple_of(source::HEX_ROW_BYTES) {
        bail!("{} B is not whole {}-byte rows", body.len(), source::HEX_ROW_BYTES);
    }
    Ok(body
        .chunks_exact(source::HEX_ROW_BYTES)
        .map(|row| {
            let h3 = u64::from_le_bytes(row[..8].try_into().unwrap());
            let mut c = [0u32; GROUPS];
            for (g, v) in c.iter_mut().enumerate() {
                *v = u32::from_le_bytes(row[8 + 4 * g..12 + 4 * g].try_into().unwrap());
            }
            (h3, c)
        })
        .collect())
}

/// ClickHouse's `[(lat,lon),…]` text as `[lon, lat]` pairs.
fn parse_ch_boundary(text: &str) -> Result<Vec<[f64; 2]>> {
    let inner = text.trim().trim_start_matches('[').trim_end_matches(']');
    inner
        .split("),(")
        .map(|p| {
            let p = p.trim_matches(|c| c == '(' || c == ')');
            let (lat, lon) = p.split_once(',').context("a vertex without a comma")?;
            Ok([lon.trim().parse::<f64>()?, lat.trim().parse::<f64>()?])
        })
        .collect()
}

/// Largest difference between our baked boundary and ClickHouse's for
/// the sampled cells (longitudes compared modulo 360°), and how many
/// disagree on the vertex count.
fn cross_check(ch: &Ch, res: u8) -> Result<(u64, u64, f64)> {
    let text = ch.run(sql::HEX_BOUNDARY_SAMPLE, &[("res", res.to_string())])?;
    let (mut n, mut count_mismatch, mut worst) = (0u64, 0u64, 0f64);
    for line in text.lines().filter(|l| !l.is_empty()) {
        let (h3, b) = line.split_once('\t').context("a sample row without a tab")?;
        let ours = record_for(h3.parse()?, [HEX_MIN_TOTAL, 0, 0, 0, 0, 0, 0, 0]).boundary;
        let theirs = parse_ch_boundary(b)?;
        n += 1;
        if ours.len() != theirs.len() {
            count_mismatch += 1;
            continue;
        }
        for (a, b) in ours.iter().zip(&theirs) {
            let dlon = ((a[0] - b[0] + 540.0).rem_euclid(360.0) - 180.0).abs();
            worst = worst.max(dlon).max((a[1] - b[1]).abs());
        }
    }
    Ok((n, count_mismatch, worst))
}

/// Reads the archive back through the engine's own opener and decoder:
/// people per resolution and group (hidden included) and drawn hexagons
/// per resolution.
/// Per resolution: people by group (hidden included), drawn hexagons, tiles.
pub type ReadBack = BTreeMap<u8, ([u64; GROUPS], u64, u64)>;

pub fn read_back(archive: &Path) -> Result<ReadBack> {
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
    let mut opener = source::PmtilesHexOpener::new(archive.display().to_string());
    let first = read(&opener.first_request())?;
    let more = opener.after_first(&first).map_err(|e| anyhow::anyhow!("opening the archive: {e}"))?;
    let bodies = more.iter().map(&mut read).collect::<Result<Vec<_>>>()?;
    let src = opener.finish(&bodies).map_err(|e| anyhow::anyhow!("opening the archive: {e}"))?;
    let mut out = ReadBack::new();
    use gpui_maps::overlay::hexes::HexSource;
    for (res, tile) in src.tiles() {
        let body = read(&src.request(res, tile))?;
        let t = codec::decode(&body).map_err(|e| anyhow::anyhow!("r{res} tile {tile:?}: {e}"))?;
        // The source's own decode checks the header against the key.
        let drawn = src.decode(res, tile, &body).map_err(|e| anyhow::anyhow!("r{res} tile {tile:?}: {e}"))?.len();
        let e = out.entry(res).or_default();
        for (s, p) in e.0.iter_mut().zip(t.people()) {
            *s += p;
        }
        e.1 += drawn as u64;
        e.2 += 1;
    }
    Ok(out)
}

pub fn export(ch: &Ch, out: &Path) -> Result<Value> {
    let started = Instant::now();
    // 1. What ClickHouse holds, checked against the P.L. totals.
    let mut counts = BTreeMap::new();
    for &(res, _) in &HEX_TILE_ZOOM {
        let v = ch.ints(sql::HEX_EXPORT_COUNTS, &[("res", res.to_string())])?;
        let sums: Vec<u64> = v[2..10].to_vec();
        if sums != US_GROUP_TOTALS.map(u64::from) || v[10] != US_TOTAL {
            bail!("r{res}: race_hex_2020 sums {sums:?} / {} are not the P.L. totals", v[10]);
        }
        counts.insert(res, (v[0], v[1]));
    }
    let drawn_all: u64 = counts.values().map(|c| c.1).sum();

    // 2. Disk: the estimate plus 10 GB, before anything is written.
    let dir = out.parent().context("--out has no directory")?;
    std::fs::create_dir_all(dir)?;
    let estimate = drawn_all * ESTIMATE_BYTES_PER_HEX;
    let free_mb = df_free_mb(dir)?;
    eprintln!("export-hex: {drawn_all} drawn hexagons, estimate {} MB; {free_mb} MB free", estimate >> 20);
    if free_mb < (estimate >> 20) + HEADROOM_MB {
        return Err(refuse(4, format!("{free_mb} MB free at {}; the export needs {} MB + 10 GB", dir.display(), estimate >> 20)));
    }

    // 3. Encode and write, resolution by resolution (tile ids grow with
    // the zoom, and each resolution has its own, so tiles go in order).
    let tmp = out.with_extension("pmtiles.tmp");
    let mut per_res = serde_json::Map::new();
    {
        let file = File::create(&tmp)?;
        let mut writer = pmtiles::PmTilesWriter::new(pmtiles::TileType::Unknown)
            .metadata(&source::hex_meta_json(ATTRIBUTION).to_string())
            .tile_compression(pmtiles::Compression::None)
            .min_zoom(3)
            .max_zoom(10)
            .create(file)?;
        for &(res, z) in &HEX_TILE_ZOOM {
            let t0 = Instant::now();
            let rows = parse_export_rows(&ch.run_bytes(sql::HEX_EXPORT_ROWS, &[("res", res.to_string())])?)?;
            if rows.len() as u64 != counts[&res].0 {
                bail!("r{res}: {} rows read, {} counted", rows.len(), counts[&res].0);
            }
            let enc = encode_res(res, &rows)?;
            let bytes: u64 = enc.tiles.iter().map(|(_, b)| b.len() as u64).sum();
            for (t, b) in &enc.tiles {
                writer.add_tile(pmtiles::TileCoord::new(t.z, t.x, t.y)?, b)?;
            }
            let free = df_free_mb(dir)?;
            if free < FLOOR_MB {
                drop(writer);
                let _ = std::fs::remove_file(&tmp);
                return Err(refuse(4, format!("free space fell to {free} MB while writing; stopped")));
            }
            eprintln!(
                "  r{res} z{z}: {} rows, {} drawn, {} hidden, {} tiles, {} B, {} over 6 vertices, max {} a tile ({:.1} s)",
                rows.len(), enc.drawn, enc.hidden_hexes, enc.tiles.len(), bytes, enc.over_six, enc.max_per_tile,
                t0.elapsed().as_secs_f64()
            );
            per_res.insert(
                format!("r{res}"),
                json!({"tile_zoom": z, "rows": rows.len(), "drawn": enc.drawn, "hidden_hexes": enc.hidden_hexes,
                       "tiles": enc.tiles.len(), "tile_bytes": bytes, "over_six_vertices": enc.over_six,
                       "max_per_tile": enc.max_per_tile}),
            );
        }
        writer.finalize()?;
    }
    std::fs::rename(&tmp, out)?;
    let archive_bytes = std::fs::metadata(out)?.len();

    // 4. Read back: every person of every resolution, and every drawn
    // hexagon ClickHouse counts.
    let back = read_back(out)?;
    let mut exact = true;
    for &(res, _) in &HEX_TILE_ZOOM {
        let (people, drawn, tiles) = back.get(&res).copied().unwrap_or_default();
        let ok = people == US_GROUP_TOTALS.map(u64::from) && drawn == counts[&res].1;
        exact &= ok;
        let total: u64 = people.iter().sum();
        let e = per_res.get_mut(&format!("r{res}")).and_then(Value::as_object_mut).unwrap();
        e.insert("read_back".into(), json!({"people": people, "total": total, "drawn": drawn, "tiles": tiles, "exact": ok}));
    }
    if !exact {
        bail!("export-hex: the archive read back does not carry the P.L. totals / ClickHouse's drawn counts: {per_res:?}");
    }

    // 5. The baked boundaries against ClickHouse's h3ToGeoBoundary.
    let mut boundary = serde_json::Map::new();
    for &(res, _) in &HEX_TILE_ZOOM {
        let (n, count_mismatch, worst) = cross_check(ch, res)?;
        boundary.insert(format!("r{res}"), json!({"sampled": n, "vertex_count_mismatch": count_mismatch, "worst_deg": worst}));
        if count_mismatch > 0 || worst > 1e-6 {
            bail!("r{res}: baked boundaries differ from h3ToGeoBoundary ({count_mismatch} counts, worst {worst}°)");
        }
    }

    Ok(json!({
        "archive": out.display().to_string(),
        "archive_bytes": archive_bytes,
        "budget_bytes": ARCHIVE_BUDGET_BYTES,
        "within_budget": archive_bytes <= ARCHIVE_BUDGET_BYTES,
        "estimate_bytes": estimate,
        "people_per_resolution": US_TOTAL,
        "per_res": per_res,
        "boundary_cross_check": boundary,
        "seconds": started.elapsed().as_secs_f64(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// C19: here, with no `cfg`: the tool always links `h3o`.
    #[test]
    fn baked_boundaries_match_h3o() {
        let cell = h3o::LatLng::new(40.78, -73.97).unwrap().to_cell(h3o::Resolution::Seven);
        let rec = record_for(u64::from(cell), [10, 0, 0, 0, 0, 0, 0, 0]);
        let want: Vec<_> = cell.boundary().iter().map(|ll| [ll.lng(), ll.lat()]).collect();
        assert_eq!(rec.boundary.len(), want.len());
        for (a, b) in rec.boundary.iter().zip(&want) {
            assert!((a[0] - b[0]).abs() < 1e-7 && (a[1] - b[1]).abs() < 1e-7);
        }
        // And through the codec: within the 1e-7° quantum.
        let tile = hexes::owning_tile(7, -73.97, 40.78).unwrap();
        let back = codec::decode(&codec::encode_tile(7, tile, &[rec])).unwrap().records.remove(0);
        for (a, b) in back.boundary.iter().zip(&want) {
            assert!((a[0] - b[0]).abs() < 1e-7 && (a[1] - b[1]).abs() < 1e-7);
        }
    }

    /// C18: a distorted r7 cell over North America (near Springfield, IL;
    /// 8 vertices, as ClickHouse's `h3ToGeoBoundary` says too) keeps every
    /// vertex through the export's encoding.
    #[test]
    fn a_cell_of_more_than_six_vertices_survives_the_export() {
        let h3 = 608_661_216_683_884_543u64;
        let rec = record_for(h3, [3000, 256, 0, 0, 0, 0, 0, 0]);
        assert_eq!(rec.boundary.len(), 8);
        let enc = encode_res(7, &[(h3, rec.counts)]).unwrap();
        assert_eq!(enc.over_six, 1);
        let t = codec::decode(&enc.tiles[0].1).unwrap();
        assert_eq!(t.records[0].boundary.len(), 8);
        for (a, b) in t.records[0].boundary.iter().zip(&rec.boundary) {
            assert!((a[0] - b[0]).abs() < 1e-7 && (a[1] - b[1]).abs() < 1e-7);
        }
    }

    #[test]
    fn hexagons_go_to_their_centres_tile_and_small_ones_are_carried_hidden() {
        let at = |lat, lon| u64::from(h3o::LatLng::new(lat, lon).unwrap().to_cell(h3o::Resolution::Eight));
        let rows = vec![
            (at(40.78, -73.97), [20, 1, 0, 0, 0, 0, 0, 0]),
            (at(40.70, -74.00), [2, 3, 0, 0, 0, 0, 0, 0]), // 5 people: hidden
            (at(34.05, -118.25), [0, 0, 12, 0, 0, 0, 0, 0]),
        ];
        let enc = encode_res(8, &rows).unwrap();
        assert_eq!((enc.drawn, enc.hidden_hexes), (2, 1));
        let mut people = [0u64; GROUPS];
        for (tile, bytes) in &enc.tiles {
            let t = codec::decode(bytes).unwrap();
            assert_eq!(t.tile, *tile);
            for r in &t.records {
                let (lon, lat) = hexes::cell_centre(r.h3).unwrap();
                assert_eq!(hexes::owning_tile(8, lon, lat), Some(*tile));
            }
            for (s, p) in people.iter_mut().zip(t.people()) {
                *s += p;
            }
        }
        assert_eq!(people, [22, 4, 12, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn clickhouse_boundary_text_parses_lat_then_lon() {
        let b = parse_ch_boundary("[(40.79,-73.95),(40.78,-73.97)]").unwrap();
        assert_eq!(b, vec![[-73.95, 40.79], [-73.97, 40.78]]);
    }
}

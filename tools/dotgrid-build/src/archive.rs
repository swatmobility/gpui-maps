//! Encoded count tiles, and the PMTiles v3 archive they end up in.
//!
//! Tiles are encoded in parallel in whatever order the pyramid finishes them,
//! so they first go to one scratch file (`EncodedStore`), indexed in memory;
//! the archive is then written in PMTiles tile-id order, which keeps it
//! "clustered" (a reader's ranges for neighbouring tiles are adjacent).
//!
//! The `pmtiles` writer reserves exactly the first 16 KiB for header + root
//! directory and starts the metadata at byte 16384 -- so metadata is never in
//! the engine's first 16 KiB fetch; `PmtilesOpener` asks for it as its own
//! small range (see `overlay::dotgrid::pmtiles_source`).

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use gpui_maps::geo::TileId;
use gpui_maps::overlay::dotgrid::GridDescriptor;
use pmtiles::{Compression, PmTilesWriter, TileCoord, TileId as PmTileId, TileType};

/// The dotgrid metadata JSON the engine's opener parses: `attribution` is a
/// sibling of `dotgrid`, and `epochs` names the single epoch this archive holds.
/// `source_offset_cells` is the (row, col) fraction of a cell the FIRST
/// source raster sat off the grid's lattice before it was snapped onto it
/// (MG-2: ≈ 0.05 for GHSL 30″, ≈ 0.5 for its 3″ tiles), and
/// `source_offset_cells_min` / `_max` bound it over every source (the float
/// scale drifts it from tile to tile); the engine reads none of them.
pub fn metadata_json(d: &GridDescriptor, offsets: &SourceOffsets) -> String {
    serde_json::json!({
        "dotgrid": {
            "version": 1,
            "base_arcsec": d.base_arcsec,
            "origin_lat": d.origin_lat,
            "origin_lon": d.origin_lon,
            "levels": d.levels,
            "max_data_zoom": d.max_data_zoom,
            "epochs": d.epochs,
            "source_offset_cells": [offsets.first.0, offsets.first.1],
            "source_offset_cells_min": [offsets.min.0, offsets.min.1],
            "source_offset_cells_max": [offsets.max.0, offsets.max.1],
        },
        "attribution": d.attribution,
    })
    .to_string()
}

/// Sub-cell offsets of the sources a build snapped (see `metadata_json`).
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SourceOffsets {
    pub first: (f64, f64),
    pub min: (f64, f64),
    pub max: (f64, f64),
}

impl SourceOffsets {
    pub fn new(first: (f64, f64)) -> Self {
        Self { first, min: first, max: first }
    }

    pub fn add(&mut self, r: (f64, f64)) {
        self.min = (self.min.0.min(r.0), self.min.1.min(r.1));
        self.max = (self.max.0.max(r.0), self.max.1.max(r.1));
    }
}

/// An `EncodedStore` at a level boundary: its length and its index as
/// `(pmtiles id, z, x, y, offset, len)`.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct StoreSnapshot {
    pub len: u64,
    pub index: Vec<(u64, u8, u32, u32, u64, u32)>,
}

pub struct EncodedStore {
    path: PathBuf,
    inner: Mutex<(File, u64, Vec<(u64, TileId, u64, u32)>)>,
}

impl EncodedStore {
    pub fn create(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("create {}", path.display()))?;
        Ok(Self { path: path.to_path_buf(), inner: Mutex::new((file, 0, Vec::new())) })
    }

    pub fn put(&self, tile: TileId, bytes: &[u8]) -> Result<()> {
        let id: u64 = PmTileId::from(TileCoord::new(tile.z, tile.x, tile.y)?).into();
        let mut g = self.inner.lock().unwrap();
        let offset = g.1;
        g.0.write_all(bytes)?;
        g.1 += bytes.len() as u64;
        g.2.push((id, tile, offset, bytes.len() as u32));
        Ok(())
    }

    /// Reopens a store at a checkpoint: cut back to `len` bytes and `index`.
    pub fn reopen(path: &Path, snapshot: &StoreSnapshot) -> Result<Self> {
        let mut file = OpenOptions::new().read(true).write(true).open(path).with_context(|| format!("open {}", path.display()))?;
        if file.metadata()?.len() < snapshot.len {
            anyhow::bail!("{} is shorter than its checkpointed {} bytes", path.display(), snapshot.len);
        }
        file.set_len(snapshot.len)?;
        file.seek(SeekFrom::Start(snapshot.len))?;
        let index = snapshot.index.iter().map(|&(id, z, x, y, off, len)| (id, TileId { z, x, y }, off, len)).collect();
        Ok(Self { path: path.to_path_buf(), inner: Mutex::new((file, snapshot.len, index)) })
    }

    /// What `reopen` needs to continue from here.
    pub fn snapshot(&self) -> Result<StoreSnapshot> {
        let mut g = self.inner.lock().unwrap();
        g.0.flush()?;
        Ok(StoreSnapshot { len: g.1, index: g.2.iter().map(|&(id, t, off, len)| (id, t.z, t.x, t.y, off, len)).collect() })
    }

    /// Encoded bytes stored so far.
    pub fn bytes(&self) -> u64 {
        self.inner.lock().unwrap().1
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().2.len()
    }

    /// Writes the archive in tile-id order; returns its size in bytes.
    pub fn write_archive(self, d: &GridDescriptor, offsets: &SourceOffsets, out: &Path) -> Result<u64> {
        let (mut file, _, mut index) = self.inner.into_inner().unwrap();
        index.sort_by_key(|e| e.0);
        file.flush()?;
        let tmp = out.with_extension("pmtiles.partial");
        {
            let sink = BufWriter::new(File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?);
            let mut writer = PmTilesWriter::new(TileType::Unknown)
                .tile_compression(Compression::None)
                .min_zoom(0)
                .max_zoom(d.max_data_zoom)
                .bounds(-180.0, -85.051_128_78, 180.0, 85.051_128_78)
                .metadata(&metadata_json(d, offsets))
                .create(sink)?;
            let mut buf = Vec::new();
            for (_, tile, offset, len) in index {
                buf.resize(len as usize, 0);
                file.seek(SeekFrom::Start(offset))?;
                file.read_exact(&mut buf)?;
                writer.add_tile(TileCoord::new(tile.z, tile.x, tile.y)?, &buf)?;
            }
            writer.finalize()?;
        }
        std::fs::rename(&tmp, out)?;
        drop(file);
        std::fs::remove_file(&self.path).ok();
        Ok(std::fs::metadata(out)?.len())
    }
}

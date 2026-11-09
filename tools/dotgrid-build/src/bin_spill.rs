//! The spill: `(local_index u32, value f32)` records appended per data tile
//! to files under a spill directory, with a hard cap on how many bytes of
//! buffers are held in memory at once. When an append would take the held
//! capacity past the cap, the biggest buffers are written out until the held
//! capacity is at most half of it -- the raster streams in spatial order, so
//! the few tiles being filled right now keep their small buffers and the
//! finished ones leave.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use gpui_maps::geo::TileId;

/// One record on disk: `u32` local index, `f32` value, little-endian.
pub const RECORD: usize = 8;

/// A buffer never grows by less than this (so a busy tile does not reallocate
/// every append) nor by more (so the cap is not blown by one doubling).
const MIN_GROW: usize = 4 << 10;
const MAX_GROW: usize = 1 << 20;

pub struct Spill {
    dir: PathBuf,
    cap: usize,
    bufs: HashMap<(u8, TileId), Vec<u8>>,
    /// Sum of `capacity()` over `bufs`, kept exact.
    held: usize,
    pub peak_held: usize,
    pub bytes_written: u64,
}

impl Spill {
    pub fn new(dir: &Path, cap: usize) -> Result<Self> {
        fs::create_dir_all(dir).with_context(|| format!("create spill dir {}", dir.display()))?;
        Ok(Self { dir: dir.to_path_buf(), cap, bufs: HashMap::new(), held: 0, peak_held: 0, bytes_written: 0 })
    }

    pub fn path_of(&self, level: u8, tile: TileId) -> PathBuf {
        self.dir.join(format!("L{level}")).join(format!("{}_{}_{}.bin", tile.z, tile.x, tile.y))
    }

    pub fn append(&mut self, level: u8, tile: TileId, records: &[(u32, f32)]) -> Result<()> {
        let mut rest = records;
        while !rest.is_empty() {
            let buf_len;
            let buf_cap;
            {
                let buf = self.bufs.entry((level, tile)).or_default();
                buf_len = buf.len();
                buf_cap = buf.capacity();
            }
            let room = (buf_cap - buf_len) / RECORD;
            if room == 0 {
                let max_grow = MAX_GROW.min(self.cap / 2).max(RECORD);
                let grow = (buf_len / 2).clamp(MIN_GROW.min(max_grow), max_grow);
                if self.held + grow > self.cap {
                    self.shed(grow)?;
                    // Shedding may have written this very buffer out; loop
                    // to re-read its state.
                    if self.held + grow > self.cap {
                        // Everything else is already out: write this one too.
                        self.write_out(&[(level, tile)])?;
                    }
                    continue;
                }
                let buf = self.bufs.get_mut(&(level, tile)).expect("entry exists");
                let before = buf.capacity();
                buf.reserve_exact(grow);
                self.held += buf.capacity() - before;
                self.peak_held = self.peak_held.max(self.held);
                continue;
            }
            let take = room.min(rest.len());
            let buf = self.bufs.get_mut(&(level, tile)).expect("entry exists");
            for &(idx, v) in &rest[..take] {
                buf.extend_from_slice(&idx.to_le_bytes());
                buf.extend_from_slice(&v.to_le_bytes());
            }
            rest = &rest[take..];
        }
        Ok(())
    }

    /// Writes the biggest buffers out until `held + need <= cap / 2`.
    fn shed(&mut self, need: usize) -> Result<()> {
        let mut by_size: Vec<((u8, TileId), usize)> = self.bufs.iter().map(|(k, b)| (*k, b.capacity())).collect();
        by_size.sort_by(|a, b| b.1.cmp(&a.1));
        let mut freed = 0usize;
        let mut victims = Vec::new();
        for (key, cap) in by_size {
            if self.held - freed + need <= self.cap / 2 {
                break;
            }
            victims.push(key);
            freed += cap;
        }
        self.write_out(&victims)
    }

    fn write_out(&mut self, keys: &[(u8, TileId)]) -> Result<()> {
        for key in keys {
            let Some(buf) = self.bufs.remove(key) else { continue };
            self.held -= buf.capacity();
            if buf.is_empty() {
                continue;
            }
            let path = self.path_of(key.0, key.1);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
            f.write_all(&buf).with_context(|| format!("append {}", path.display()))?;
            self.bytes_written += buf.len() as u64;
        }
        Ok(())
    }

    /// Writes every buffer of `level` out and returns the tiles that have a
    /// spill file at that level.
    pub fn finish_level(&mut self, level: u8) -> Result<Vec<TileId>> {
        let keys: Vec<(u8, TileId)> = self.bufs.keys().filter(|k| k.0 == level).copied().collect();
        self.write_out(&keys)?;
        let dir = self.dir.join(format!("L{level}"));
        let mut tiles = Vec::new();
        if dir.exists() {
            for entry in fs::read_dir(&dir)? {
                let name = entry?.file_name().to_string_lossy().to_string();
                let Some(stem) = name.strip_suffix(".bin") else { continue };
                let p: Vec<u32> = stem.split('_').filter_map(|s| s.parse().ok()).collect();
                if p.len() == 3 {
                    tiles.push(TileId { z: p[0] as u8, x: p[1], y: p[2] });
                }
            }
        }
        tiles.sort_by_key(|t| (t.z, t.y, t.x));
        Ok(tiles)
    }
}

impl Spill {
    /// Deletes every spill file of `level` (its buffers must be written out).
    pub fn remove_level(&mut self, level: u8) -> Result<()> {
        let dir = self.dir.join(format!("L{level}"));
        if dir.exists() {
            fs::remove_dir_all(&dir).with_context(|| format!("remove {}", dir.display()))?;
        }
        Ok(())
    }
}

/// Reads a spill file's records.
pub fn read_records(path: &Path) -> Result<Vec<(u32, f32)>> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Ok(bytes
        .chunks_exact(RECORD)
        .map(|r| {
            (u32::from_le_bytes(r[0..4].try_into().unwrap()), f32::from_le_bytes(r[4..8].try_into().unwrap()))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spill_memory_stays_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let cap = 1 << 20;
        let mut spill = Spill::new(dir.path(), cap).unwrap();
        // 5 M cells over 300 tiles, in runs (the way a raster streams).
        let mut want: HashMap<TileId, (u64, f64)> = HashMap::new();
        let mut s = 7u32;
        let mut run = Vec::new();
        for i in 0..5_000_000u32 {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let tile = TileId { z: 6, x: (i / 1000) % 300, y: 7 };
            run.push((s >> 12, (s % 97) as f32));
            if run.len() == 1000 {
                let e = want.entry(tile).or_default();
                e.0 += run.len() as u64;
                e.1 += run.iter().map(|r| r.1 as f64).sum::<f64>();
                spill.append(0, tile, &run).unwrap();
                run.clear();
            }
        }
        let tiles = spill.finish_level(0).unwrap();
        assert!(spill.peak_held <= cap, "peak {} > cap {cap}", spill.peak_held);
        assert_eq!(tiles.len(), 300);
        for t in tiles {
            let recs = read_records(&spill.path_of(0, t)).unwrap();
            let (n, sum) = want[&t];
            assert_eq!(recs.len() as u64, n);
            assert_eq!(recs.iter().map(|r| r.1 as f64).sum::<f64>(), sum);
        }
    }
}

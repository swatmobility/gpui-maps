//! Streams a GHSL GeoTIFF one internal chunk (tile or strip) at a time, so a
//! 43200×21600 float64 world (7.5 GB raw) is never resident. Pure Rust (the
//! `tiff` crate): GDAL is broken on this machine and is never used.
//!
//! Only two GeoTIFF tags are read: `ModelPixelScaleTag` (33550, degrees per
//! pixel in x and y) and `ModelTiepointTag` (33922, the raster's NW corner in
//! lon/lat). GHSL's 4326 products are `PixelIsArea`, so the tiepoint is the
//! corner of pixel (0, 0), not its centre.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use rayon::prelude::*;
use tiff::decoder::{Decoder, DecodingResult, Limits};
use tiff::tags::Tag;

/// How far a declared pixel scale may sit from `arcsec / 3600` degrees,
/// relative. GHSL's own 30″ world declares 0.008333333300327 (3.9e-9 off:
/// the value went through single precision somewhere upstream), which drifts
/// 1.7e-4 of a cell across its 43200 columns -- harmless. A scale like
/// 0.00833334 (8e-7 off) drifts 0.035 of a cell across the same width, and
/// means the raster is not the grid it claims to be, so it is refused. 1e-8
/// bounds the drift at 4.3e-4 of a cell over 43200 columns.
const SCALE_TOLERANCE_REL: f64 = 1e-8;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GeoInfo {
    pub width: u32,
    pub height: u32,
    /// Degrees per pixel, x then y (both positive).
    pub pixel_scale: (f64, f64),
    /// Lon/lat of the raster's NW corner.
    pub tiepoint: (f64, f64),
    /// Internal chunk size (a tile, or `(width, rows_per_strip)`).
    pub chunk: (u32, u32),
    pub chunks: u32,
    pub tiled: bool,
}

fn decoder(path: &Path) -> Result<Decoder<BufReader<File>>> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    // Default limits cap an IFD value at 1 MiB; a world raster's tile-offset
    // array is bigger than that at 3″. Chunks themselves stay small.
    Ok(Decoder::new(BufReader::new(file))?.with_limits(Limits::unlimited()))
}

pub fn info(path: &Path) -> Result<GeoInfo> {
    let mut dec = decoder(path)?;
    read_info(&mut dec)
}

fn read_info(dec: &mut Decoder<BufReader<File>>) -> Result<GeoInfo> {
    let (width, height) = dec.dimensions()?;
    let scale = dec.get_tag_f64_vec(Tag::ModelPixelScaleTag).context("no ModelPixelScaleTag (33550)")?;
    let tie = dec.get_tag_f64_vec(Tag::ModelTiepointTag).context("no ModelTiepointTag (33922)")?;
    if scale.len() < 2 || tie.len() < 6 {
        bail!("malformed geotags: scale {scale:?}, tiepoint {tie:?}");
    }
    if tie[0] != 0.0 || tie[1] != 0.0 {
        bail!("tiepoint anchored at raster ({}, {}), expected (0, 0)", tie[0], tie[1]);
    }
    let tiled = matches!(dec.get_chunk_type(), tiff::decoder::ChunkType::Tile);
    let chunks = if tiled { dec.tile_count()? } else { dec.strip_count()? };
    Ok(GeoInfo {
        width,
        height,
        pixel_scale: (scale[0], scale[1]),
        tiepoint: (tie[3], tie[4]),
        chunk: dec.chunk_dimensions(),
        chunks,
        tiled,
    })
}

/// Refuses a raster whose pixel scale is not exactly the declared cell size.
pub fn check_pixel_scale(pixel_scale: (f64, f64), arcsec: f64) -> Result<()> {
    let want = arcsec / 3600.0;
    for (axis, got) in [("x", pixel_scale.0), ("y", pixel_scale.1)] {
        if ((got - want) / want).abs() > SCALE_TOLERANCE_REL {
            bail!("pixel scale {axis} = {got:.15} deg is not {arcsec}″ ({want:.15} deg)");
        }
    }
    Ok(())
}

/// Where the raster's pixel (0, 0) sits in a grid anchored at
/// `(origin_lon, origin_lat)` with `arcsec` cells: the whole row/col offset,
/// and the fraction of a cell left over.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Offset {
    pub rows: i64,
    pub cols: i64,
    /// Exact offset minus the whole offset, in cells: `0 <= x < 1` (down to
    /// `-LATTICE_EPS` for a raster already on the lattice). The snap moves
    /// every cell by this much, so by up to just under one cell -- not half a
    /// cell, as a nearest-boundary snap would.
    pub residual: (f64, f64),
}

/// How far below a cell boundary a tiepoint may sit and still count as on it.
const LATTICE_EPS: f64 = 1e-6;

/// Errors unless the tiepoint sits on a cell boundary (±1e-6 of a cell),
/// or, with `snap` (the converter's default, by ruling on MG-2), moves the
/// raster north and west -- toward the grid origin -- onto the boundary at
/// or before it, and reports the residual: every cell moves by that fraction
/// of a cell, which can be anything short of a whole cell (a raster 0.9 cell
/// off moves 0.9, not 0.1 the other way). GHSL's 30″ world sits 0.05 cells south and east of the
/// 90°N/180°W lattice (MG-2); its 3″ tiles sit on the same source lattice,
/// 1.5″ = HALF a 3″ cell off, with the float pixel scale drifting each
/// tile's tiepoint a little to either side of the half. Rounding would snap
/// neighbouring tiles in opposite directions (a one-cell tear); always moving
/// toward the origin snaps every tile, and the 30″ world, by the same 1.5″,
/// so the 3″ and 30″ archives stay nested.
pub fn grid_offset(g: &GeoInfo, arcsec: f64, origin_lon: f64, origin_lat: f64, snap: bool) -> Result<Offset> {
    let cell = arcsec / 3600.0;
    let rows = (origin_lat - g.tiepoint.1) / cell;
    let cols = (g.tiepoint.0 - origin_lon) / cell;
    let (r0, c0) = ((rows + LATTICE_EPS).floor(), (cols + LATTICE_EPS).floor());
    let off = Offset { rows: r0 as i64, cols: c0 as i64, residual: (rows - r0, cols - c0) };
    if !snap {
        for (name, v) in [("row", rows), ("col", cols)] {
            if (v - v.round()).abs() > LATTICE_EPS {
                bail!(
                    "tiepoint {:?} is {v} {name}s from the grid origin: not on a cell boundary (the default snap moves every cell by the fraction; --no-snap refuses)",
                    g.tiepoint
                );
            }
        }
    }
    if off.rows < 0 {
        bail!("tiepoint {:?} is north of the grid origin", g.tiepoint);
    }
    Ok(off)
}

/// Decodes every chunk on the rayon pool (one decoder per worker thread) and
/// hands each chunk's populated pixels, as `(row, col, value)` in RASTER
/// coordinates, to `sink`. Values `<= 0` (GHSL's zero and its -200 no-data)
/// and NaN are skipped. `sink` runs on worker threads, once per chunk.
pub fn for_each_populated_chunk(path: &Path, sink: impl Fn(&[(u32, u32, f64)]) + Sync) -> Result<GeoInfo> {
    let g = info(path)?;
    let across = g.width.div_ceil(g.chunk.0);
    (0..g.chunks).into_par_iter().try_for_each_init(
        || decoder(path),
        |dec, i| -> Result<()> {
            let dec = dec.as_mut().map_err(|e| anyhow!("{e:#}"))?;
            let (w, _h) = dec.chunk_data_dimensions(i);
            let (row0, col0) = if g.tiled {
                ((i / across) * g.chunk.1, (i % across) * g.chunk.0)
            } else {
                (i * g.chunk.1, 0)
            };
            let data = match dec.read_chunk(i) {
                Ok(d) => d,
                // The crate's LZW reader (weezl) refuses some valid GHSL
                // chunks: it reads an edge tile row by row plus padding, and
                // on R9_C19's chunk 2114 (the table reaches the 10 -> 11 bit
                // switch at the last code) it consumes the last input bytes
                // early and then stalls at byte 521,984 of 524,288. A plain
                // decoder reads the whole chunk and its end code.
                Err(e) => read_chunk_lzw(path, dec, i, g.chunk)
                    .with_context(|| format!("chunk {i}: {e}, and the fallback LZW decode failed too"))?,
            };
            let mut out = Vec::new();
            let mut push = |k: usize, v: f64| {
                if v > 0.0 {
                    out.push((row0 + (k as u32) / w, col0 + (k as u32) % w, v));
                }
            };
            match data {
                DecodingResult::F64(v) => v.iter().enumerate().for_each(|(k, &x)| push(k, x)),
                DecodingResult::F32(v) => v.iter().enumerate().for_each(|(k, &x)| push(k, x as f64)),
                _ => bail!("chunk {i}: only float32/float64 rasters are supported"),
            }
            if !out.is_empty() {
                sink(&out);
            }
            Ok(())
        },
    )?;
    Ok(g)
}

/// Reads chunk `i` straight from the file and decodes it with [`lzw_decode`]:
/// uncompressed-predictor LZW float chunks only (what GHSL ships). Returns
/// the chunk cropped to its data dimensions, like `Decoder::read_chunk`.
fn read_chunk_lzw(path: &Path, dec: &mut Decoder<BufReader<File>>, i: u32, chunk: (u32, u32)) -> Result<DecodingResult> {
    use std::io::{Read, Seek, SeekFrom};
    use tiff::tags::{CompressionMethod, Predictor};
    let compression = dec.find_tag_unsigned::<u16>(Tag::Compression)?.unwrap_or(1);
    let predictor = dec.find_tag_unsigned::<u16>(Tag::Predictor)?.unwrap_or(1);
    let bits = dec.find_tag_unsigned::<u16>(Tag::BitsPerSample)?.unwrap_or(1);
    let format = dec.find_tag_unsigned::<u16>(Tag::SampleFormat)?.unwrap_or(1);
    let samples = dec.find_tag_unsigned::<u16>(Tag::SamplesPerPixel)?.unwrap_or(1);
    if compression != CompressionMethod::LZW.to_u16() || predictor != Predictor::None.to_u16() || format != 3 || samples != 1 {
        bail!("no fallback for compression {compression}, predictor {predictor}, sample format {format}, {samples} samples");
    }
    let (w, h) = dec.chunk_data_dimensions(i);
    // Tiles are stored full size (cropped below, like `read_chunk`); the
    // last strip is stored short.
    let (offsets, counts, stored_rows) = match dec.get_chunk_type() {
        tiff::decoder::ChunkType::Tile => (Tag::TileOffsets, Tag::TileByteCounts, chunk.1),
        tiff::decoder::ChunkType::Strip => (Tag::StripOffsets, Tag::StripByteCounts, h),
    };
    let offset = *dec.get_tag_u64_vec(offsets)?.get(i as usize).context("chunk offset")?;
    let count = *dec.get_tag_u64_vec(counts)?.get(i as usize).context("chunk byte count")?;
    let mut raw = vec![0u8; count as usize];
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    f.read_exact(&mut raw)?;
    let bytes = (bits / 8) as usize;
    let want = (chunk.0 * stored_rows) as usize * bytes;
    let mut out = lzw_decode(&raw, want)?;
    if out.len() < want {
        bail!("LZW chunk decoded to {} bytes, expected {want}", out.len());
    }
    out.truncate(want);
    let mut magic = [0u8; 2];
    f.seek(SeekFrom::Start(0))?;
    f.read_exact(&mut magic)?;
    let little = &magic == b"II";
    let pick = |r: u32, c: u32| &out[((r * chunk.0 + c) as usize) * bytes..][..bytes];
    let cells = (0..h).flat_map(|r| (0..w).map(move |c| (r, c)));
    Ok(match bits {
        64 => DecodingResult::F64(
            cells
                .map(|(r, c)| {
                    let b: [u8; 8] = pick(r, c).try_into().unwrap();
                    if little { f64::from_le_bytes(b) } else { f64::from_be_bytes(b) }
                })
                .collect(),
        ),
        32 => DecodingResult::F32(
            cells
                .map(|(r, c)| {
                    let b: [u8; 4] = pick(r, c).try_into().unwrap();
                    if little { f32::from_le_bytes(b) } else { f32::from_be_bytes(b) }
                })
                .collect(),
        ),
        other => bail!("{other}-bit float samples"),
    })
}

/// TIFF LZW (MSB-first codes, 9-12 bits, "early change" size switch, 256 =
/// clear, 257 = end). Stops at the end code, at `limit` output bytes, or
/// when the input runs out.
pub(crate) fn lzw_decode(data: &[u8], limit: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(limit);
    // Entry k is (prefix entry, last byte, length); entries < 256 are bytes.
    let mut table: Vec<(u32, u8, u32)> = (0..258u32).map(|b| (u32::MAX, b as u8, 1)).collect();
    let (mut pos, mut width, mut prev): (usize, usize, Option<u32>) = (0, 9, None);
    let nbits = data.len() * 8;
    let mut scratch = Vec::new();
    let emit = |table: &[(u32, u8, u32)], mut code: u32, scratch: &mut Vec<u8>, out: &mut Vec<u8>| -> u8 {
        scratch.clear();
        while code != u32::MAX {
            let (p, b, _) = table[code as usize];
            scratch.push(b);
            code = p;
        }
        out.extend(scratch.iter().rev());
        *scratch.last().unwrap()
    };
    while pos + width <= nbits && out.len() < limit {
        let mut v = 0u32;
        for k in 0..width {
            let bit = pos + k;
            v = (v << 1) | ((data[bit >> 3] >> (7 - (bit & 7))) & 1) as u32;
        }
        pos += width;
        match v {
            256 => {
                table.truncate(258);
                width = 9;
                prev = None;
                continue;
            }
            257 => break,
            _ => {}
        }
        let Some(p) = prev else {
            if v >= 256 {
                bail!("LZW code {v} right after a clear");
            }
            out.push(v as u8);
            prev = Some(v);
            continue;
        };
        if (v as usize) < table.len() && v != 256 && v != 257 {
            let first = emit(&table, v, &mut scratch, &mut out);
            table.push((p, first, table[p as usize].2 + 1));
        } else if v as usize == table.len() {
            // KwKwK: the new entry is prev + prev's first byte.
            let first = emit(&table, p, &mut scratch, &mut out);
            out.push(first);
            table.push((p, first, table[p as usize].2 + 1));
        } else {
            bail!("LZW code {v} past the table ({})", table.len());
        }
        prev = Some(v);
        if table.len() + 1 >= (1 << width) && width < 12 {
            width += 1;
        }
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use tiff::encoder::{colortype, compression::Lzw, TiffEncoder};

    /// Writes a float64 LZW GeoTIFF (strips) with the given geotags.
    pub(crate) fn write_geotiff(path: &Path, w: u32, h: u32, scale: f64, tie: (f64, f64), data: &[f64]) {
        let mut enc = TiffEncoder::new(File::create(path).unwrap()).unwrap();
        let mut img = enc.new_image_with_compression::<colortype::Gray64Float, _>(w, h, Lzw).unwrap();
        img.encoder().write_tag(Tag::ModelPixelScaleTag, &[scale, scale, 0.0][..]).unwrap();
        img.encoder().write_tag(Tag::ModelTiepointTag, &[0.0, 0.0, 0.0, tie.0, tie.1, 0.0][..]).unwrap();
        img.write_data(data).unwrap();
    }

    /// Packs LZW `codes` at the widths a TIFF decoder reads them (the table
    /// grows by one per code after the first following a clear; early change).
    pub(crate) fn lzw_pack(codes: &[u32]) -> Vec<u8> {
        let (mut bits, mut len, mut width, mut first) = (Vec::<bool>::new(), 258u32, 9u32, true);
        for &c in codes {
            (0..width).rev().for_each(|k| bits.push((c >> k) & 1 == 1));
            match c {
                256 => (len, width, first) = (258, 9, true),
                257 => break,
                _ if first => first = false,
                _ => {
                    len += 1;
                    if len + 1 >= 1 << width && width < 12 {
                        width += 1;
                    }
                }
            }
        }
        bits.chunks(8).map(|c| c.iter().enumerate().fold(0u8, |a, (i, &b)| a | ((b as u8) << (7 - i)))).collect()
    }

    /// Codes for a run of `byte`: after a clear, the literal then 258, 259, …
    /// (each the entry being defined), so code k of the round spells k bytes.
    fn ramp(byte: u8, m: u32) -> Vec<u32> {
        let mut v = vec![256, byte as u32];
        v.extend((0..m - 1).map(|k| 258 + k));
        v
    }

    /// Codes spelling `total` bytes of `byte` in two clear-separated rounds,
    /// the stream ending with a code of `last` bytes that fills the table to
    /// 1023 entries -- the 10 -> 11 bit switch -- and an 11-bit end code: the
    /// shape of GHSL R9_C19 chunk 2114.
    pub(crate) fn run_tile_codes(byte: u8, total: u32, last: u32) -> Vec<u32> {
        // Final round: a 765-code ramp (table 1022) and one code of `last` bytes.
        let m = 765u32;
        let fin = m * (m + 1) / 2 + last;
        let mut need = total - fin;
        let mut a = 1u32;
        while (a + 1) * (a + 2) / 2 <= need && a < 3000 {
            a += 1;
        }
        let mut codes = ramp(byte, a);
        need -= a * (a + 1) / 2;
        if need > 0 {
            codes.push(if need == 1 { byte as u32 } else { 258 + need - 2 });
        }
        codes.extend(ramp(byte, m));
        codes.push(if last == 1 { byte as u32 } else { 258 + last - 2 });
        codes.push(257);
        codes
    }

    #[test]
    fn a_chunk_the_crate_refuses_is_read_by_the_fallback() {
        // A tiled float64 LZW raster whose right-edge tile (224 of 256
        // columns, like GHSL's C-edge tiles) the crate's LZW reader stalls
        // on: it reads the tile row by row plus padding, and with this
        // stream's shape (table filled to the 10 -> 11 bit switch by the
        // last code, 11-bit end code) weezl consumes the last input bytes
        // early and then reports no progress at byte 521,984 -- the same
        // place it stalled on GHSL R9_C19 chunk 2114. Every byte is 0x40:
        // every pixel 0x4040404040404040 = 32.50196078431372.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("edge.tif");
        let packed = lzw_pack(&run_tile_codes(0x40, 256 * 256 * 8, 12));
        assert_eq!(lzw_decode(&packed, 1 << 20).unwrap(), vec![0x40u8; 256 * 256 * 8]);
        write_raw_tiff(&path, 256 + 224, 256, (256, 256), true, &[packed.clone(), packed]);

        let mut dec = decoder(&path).unwrap();
        assert!(dec.read_chunk(0).is_ok(), "the full-width tile is fine");
        let err = dec.read_chunk(1).expect_err("the crate refuses the edge tile");
        assert!(format!("{err}").contains("lzw"), "{err}");

        let v = f64::from_le_bytes([0x40; 8]);
        let seen = std::sync::Mutex::new(Vec::new());
        let g = for_each_populated_chunk(&path, |c| seen.lock().unwrap().extend_from_slice(c)).unwrap();
        assert_eq!((g.width, g.height, g.tiled), (480, 256, true));
        let mut seen = seen.into_inner().unwrap();
        seen.sort_by_key(|&(r, c, _)| (r, c));
        let want: Vec<(u32, u32, f64)> = (0..256).flat_map(|r| (0..480).map(move |c| (r, c, v))).collect();
        assert_eq!(seen, want);
    }

    /// A little-endian classic GeoTIFF of float64 LZW chunks given already
    /// compressed: tiles of `chunk` when `tiled`, else strips of `chunk.1` rows.
    pub(crate) fn write_raw_tiff(path: &Path, w: u32, h: u32, chunk: (u32, u32), tiled: bool, chunks: &[Vec<u8>]) {
        use std::io::Write;
        let mut data: Vec<u8> = vec![b'I', b'I', 42, 0, 0, 0, 0, 0];
        let mut offsets = Vec::new();
        for c in chunks {
            offsets.push(data.len() as u32);
            data.extend_from_slice(c);
        }
        let mut extra: Vec<u8> = Vec::new();
        let ifd_at = data.len() as u32;
        // (tag, type, count, inline value or a payload placed after the IFD)
        let mut entries: Vec<(u16, u16, u32, Vec<u8>)> = vec![
            (256, 4, 1, w.to_le_bytes().to_vec()),
            (257, 4, 1, h.to_le_bytes().to_vec()),
            (258, 3, 1, 64u16.to_le_bytes().to_vec()),
            (259, 3, 1, 5u16.to_le_bytes().to_vec()),
            (262, 3, 1, 1u16.to_le_bytes().to_vec()),
            (277, 3, 1, 1u16.to_le_bytes().to_vec()),
            (339, 3, 1, 3u16.to_le_bytes().to_vec()),
            (33550, 12, 3, [1.0f64, 1.0, 0.0].iter().flat_map(|v| v.to_le_bytes()).collect()),
            (33922, 12, 6, [0.0f64, 0.0, 0.0, -180.0, 90.0, 0.0].iter().flat_map(|v| v.to_le_bytes()).collect()),
        ];
        let counts: Vec<u8> = chunks.iter().flat_map(|c| (c.len() as u32).to_le_bytes()).collect();
        let offs: Vec<u8> = offsets.iter().flat_map(|o| o.to_le_bytes()).collect();
        let n = chunks.len() as u32;
        if tiled {
            entries.push((322, 4, 1, chunk.0.to_le_bytes().to_vec()));
            entries.push((323, 4, 1, chunk.1.to_le_bytes().to_vec()));
            entries.push((324, 4, n, offs));
            entries.push((325, 4, n, counts));
        } else {
            entries.push((273, 4, n, offs));
            entries.push((278, 4, 1, chunk.1.to_le_bytes().to_vec()));
            entries.push((279, 4, n, counts));
        }
        entries.sort_by_key(|e| e.0);
        let payload_at = ifd_at + 2 + 12 * entries.len() as u32 + 4;
        let mut ifd: Vec<u8> = (entries.len() as u16).to_le_bytes().to_vec();
        for (tag, typ, count, v) in &entries {
            ifd.extend_from_slice(&tag.to_le_bytes());
            ifd.extend_from_slice(&typ.to_le_bytes());
            ifd.extend_from_slice(&count.to_le_bytes());
            if v.len() <= 4 {
                let mut inl = v.clone();
                inl.resize(4, 0);
                ifd.extend_from_slice(&inl);
            } else {
                ifd.extend_from_slice(&(payload_at + extra.len() as u32).to_le_bytes());
                extra.extend_from_slice(v);
            }
        }
        ifd.extend_from_slice(&0u32.to_le_bytes());
        data[4..8].copy_from_slice(&ifd_at.to_le_bytes());
        data.extend_from_slice(&ifd);
        data.extend_from_slice(&extra);
        File::create(path).unwrap().write_all(&data).unwrap();
    }

    #[test]
    fn the_fallback_lzw_decoder_reads_what_the_crate_reads() {
        // A tiled float64 LZW GeoTIFF with edge tiles (300 = 256 + 44).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.tif");
        let (w, h) = (300u32, 280u32);
        let mut s = 99u32;
        let data: Vec<f64> = (0..w * h)
            .map(|k| {
                s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                if s % 3 == 0 { (s >> 20) as f64 / 3.0 } else if k % 11 == 0 { -200.0 } else { 0.0 }
            })
            .collect();
        write_geotiff(&path, w, h, 1.0, (-180.0, 90.0), &data);
        let mut dec = decoder(&path).unwrap();
        let g = read_info(&mut dec).unwrap();
        for i in 0..g.chunks {
            let want = dec.read_chunk(i).unwrap();
            let got = read_chunk_lzw(&path, &mut dec, i, g.chunk).unwrap();
            match (want, got) {
                (DecodingResult::F64(a), DecodingResult::F64(b)) => assert_eq!(a, b, "chunk {i}"),
                _ => panic!("chunk {i}: not float64"),
            }
        }
    }

    #[test]
    fn a_pixel_scale_that_is_not_the_declared_arcsec_is_refused() {
        assert!(check_pixel_scale((0.00833334, 0.00833334), 30.0).is_err());
        // What GHSL's 30″ world actually declares (MG-2).
        assert!(check_pixel_scale((0.008333333300327, 0.008333333300327), 30.0).is_ok());
        assert!(check_pixel_scale((30.0 / 3600.0, 30.0 / 3600.0), 30.0).is_ok());
        assert!(check_pixel_scale((30.0 / 3600.0, 30.0 / 3600.0), 3.0).is_err());
    }

    #[test]
    fn chunks_stream_every_populated_pixel_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.tif");
        let (w, h) = (37u32, 23u32);
        let data: Vec<f64> = (0..w * h).map(|k| if k % 3 == 0 { k as f64 } else { -200.0 }).collect();
        write_geotiff(&path, w, h, 1.0, (-180.0, 90.0), &data);

        let seen = std::sync::Mutex::new(Vec::new());
        let g = for_each_populated_chunk(&path, |c| seen.lock().unwrap().extend_from_slice(c)).unwrap();
        assert_eq!((g.width, g.height), (w, h));
        let mut seen = seen.into_inner().unwrap();
        seen.sort_by_key(|&(r, c, _)| (r, c));
        let want: Vec<(u32, u32, f64)> =
            (0..w * h).filter(|k| k % 3 == 0 && *k > 0).map(|k| (k / w, k % w, k as f64)).collect();
        assert_eq!(seen, want);
    }

    #[test]
    fn a_tiepoint_off_the_cell_lattice_is_refused() {
        let mut g = GeoInfo {
            width: 1,
            height: 1,
            pixel_scale: (0.1, 0.1),
            tiepoint: (-180.0, 89.1),
            chunk: (1, 1),
            chunks: 1,
            tiled: false,
        };
        let o = grid_offset(&g, 30.0, -180.0, 90.0, false).unwrap();
        assert_eq!((o.rows, o.cols), (108, 0));
        // GHSL's own 30″ world tiepoint (MG-2).
        g.tiepoint = (-180.00791593130032, 89.0995831776456);
        assert!(grid_offset(&g, 30.0, -180.0, 90.0, false).is_err());
        let o = grid_offset(&g, 30.0, -180.0, 90.0, true).unwrap();
        assert_eq!((o.rows, o.cols), (108, -1));
        assert!(o.residual.0.abs() < 0.051 && o.residual.1.abs() < 0.051, "{o:?}");
    }

    #[test]
    fn ghsl_3s_tiles_snap_the_same_way_across_the_half_cell() {
        // Two real GHSL 3″ tiles (R10_C1 and R6_C30): the source lattice is
        // the 30″ world's, 1.5″ off ours -- half a 3″ cell -- and the float
        // pixel scale drifts the tiepoints to either side of that half.
        // Rounding would snap one tile north-west and the other south-east,
        // a one-cell tear between them; both must move the same way, and by
        // the same amount the 30″ snap moves (north and west, 1.5″).
        let tile = |lon: f64, lat: f64| GeoInfo {
            width: 12_000,
            height: 12_000,
            pixel_scale: (0.000_833_333_330_032_692_4, 0.000_833_333_329_979_504),
            tiepoint: (lon, lat),
            chunk: (256, 256),
            chunks: 2209,
            tiled: true,
        };
        let a = grid_offset(&tile(-180.007_915_931_300_32, -0.900_416_460_140_832), 3.0, -180.0, 90.0, true).unwrap();
        let b = grid_offset(&tile(109.992_082_920_076_63, 39.099_583_378_875_366), 3.0, -180.0, 90.0, true).unwrap();
        assert_eq!((a.rows, a.cols), (109_080, -10), "{a:?}");
        assert_eq!((b.rows, b.cols), (61_080, 347_990), "{b:?}");
        for o in [a, b] {
            assert!((o.residual.0 - 0.5).abs() < 0.002 && (o.residual.1 - 0.5).abs() < 0.002, "{o:?}");
        }
        // Same displacement in degrees as the 30″ world's snap.
        let world = GeoInfo { tiepoint: (-180.007_915_931_300_32, 89.099_583_177_645_6), ..tile(0.0, 0.0) };
        let w = grid_offset(&world, 30.0, -180.0, 90.0, true).unwrap();
        assert!((w.residual.0 * 30.0 - a.residual.0 * 3.0).abs() < 0.01, "{w:?} vs {a:?}");
        assert!((w.residual.1 * 30.0 - a.residual.1 * 3.0).abs() < 0.01, "{w:?} vs {a:?}");
    }
}

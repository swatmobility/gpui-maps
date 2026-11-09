//! One internal tile's bytes to pixels: decompress, undo the predictor,
//! read the samples in the file's byte order, and keep what is drawn --
//! premultiplied RGBA for imagery, one band of `f32` (NaN for nodata) for
//! a ramp.

use super::{CogCompression, CogDataType, CogError, CogRendering, Format, Pixels};

/// Decodes one internal tile (`format.tile_w` x `format.tile_h`).
pub(crate) fn decode_tile(bytes: &[u8], format: &Format, rendering: &CogRendering, nodata: Option<f64>) -> Result<Pixels, CogError> {
    let (w, h) = (format.tile_w as usize, format.tile_h as usize);
    match format.compression {
        CogCompression::Jpeg => return jpeg(bytes, format, w, h),
        CogCompression::WebP => return webp(bytes, w, h, rendering, nodata),
        _ => {}
    }
    // Samples in this tile: one plane when bands are separate.
    let spp = if format.planar_separate { 1 } else { format.spp };
    let bps = format.dtype.bytes();
    let expected = w * h * spp * bps;
    let mut raw = match format.compression {
        CogCompression::None => bytes.to_vec(),
        CogCompression::Deflate => inflate(bytes, expected)?,
        CogCompression::Lzw => lzw(bytes, expected)?,
        CogCompression::PackBits => packbits(bytes, expected)?,
        CogCompression::Jpeg | CogCompression::WebP => unreachable!("handled above"),
    };
    if raw.len() < expected {
        // A truncated last strip of data is padding the writer skipped.
        raw.resize(expected, 0);
    }
    raw.truncate(expected);
    match format.predictor {
        2 => undo_horizontal(&mut raw, w, h, spp, bps, format.little_endian),
        3 => raw = undo_float(&raw, w, h, spp, bps, format.little_endian)?,
        _ => {}
    }
    match rendering {
        CogRendering::Rgb { alpha } => {
            let mut out = vec![0u8; w * h * 4];
            let nd = nodata.filter(|v| (0.0..=255.0).contains(v)).map(|v| v as u8);
            for (px, src) in out.as_chunks_mut::<4>().0.iter_mut().zip(raw.chunks_exact(spp)) {
                let a = if *alpha { src[3] } else { 255 };
                let a = if nd.is_some_and(|n| src[0] == n && src[1] == n && src[2] == n) { 0 } else { a };
                let pm = |c: u8| ((u16::from(c) * u16::from(a) + 127) / 255) as u8;
                px.copy_from_slice(&[pm(src[0]), pm(src[1]), pm(src[2]), a]);
            }
            Ok(Pixels::Rgba(out))
        }
        CogRendering::Ramp { band, .. } => {
            let band = if format.planar_separate { 0 } else { *band };
            let le = format.little_endian;
            let mut out = Vec::with_capacity(w * h);
            for i in 0..w * h {
                let at = (i * spp + band) * bps;
                let s = &raw[at..at + bps];
                let v = sample(s, format.dtype, le);
                let nodata_hit = nodata.is_some_and(|n| v == n || (n.is_nan() && v.is_nan()));
                out.push(if nodata_hit { f32::NAN } else { v as f32 });
            }
            Ok(Pixels::Values(out))
        }
    }
}

fn sample(s: &[u8], dtype: CogDataType, le: bool) -> f64 {
    macro_rules! num {
        ($t:ty) => {{
            let b = s.try_into().expect("sample width");
            f64::from(if le { <$t>::from_le_bytes(b) } else { <$t>::from_be_bytes(b) })
        }};
    }
    match dtype {
        CogDataType::U8 => f64::from(s[0]),
        CogDataType::I8 => f64::from(s[0] as i8),
        CogDataType::U16 => num!(u16),
        CogDataType::I16 => num!(i16),
        CogDataType::U32 => num!(u32),
        CogDataType::I32 => num!(i32),
        CogDataType::F32 => num!(f32),
        CogDataType::F64 => {
            let b: [u8; 8] = s.try_into().expect("8");
            if le {
                f64::from_le_bytes(b)
            } else {
                f64::from_be_bytes(b)
            }
        }
    }
}

fn inflate(bytes: &[u8], expected: usize) -> Result<Vec<u8>, CogError> {
    use std::io::Read as _;
    let mut out = Vec::with_capacity(expected);
    flate2::read::ZlibDecoder::new(bytes)
        .take(expected as u64)
        .read_to_end(&mut out)
        .map_err(|e| CogError::Decode(format!("deflate: {e}")))?;
    Ok(out)
}

fn lzw(bytes: &[u8], expected: usize) -> Result<Vec<u8>, CogError> {
    let mut out = Vec::with_capacity(expected);
    let mut decoder = weezl::decode::Decoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8);
    let result = decoder.into_vec(&mut out).decode_all(bytes);
    // Some writers end without the end code; what decoded is the data.
    if let Err(e) = result.status {
        if out.is_empty() {
            return Err(CogError::Decode(format!("LZW: {e}")));
        }
    }
    out.truncate(expected);
    Ok(out)
}

fn packbits(bytes: &[u8], expected: usize) -> Result<Vec<u8>, CogError> {
    let mut out = Vec::with_capacity(expected);
    let mut i = 0;
    while i < bytes.len() && out.len() < expected {
        let n = bytes[i] as i8;
        i += 1;
        if n >= 0 {
            let run = n as usize + 1;
            let end = (i + run).min(bytes.len());
            out.extend_from_slice(&bytes[i..end]);
            i = end;
        } else if n != -128 {
            let run = (1 - i32::from(n)) as usize;
            let Some(&b) = bytes.get(i) else { break };
            out.extend(std::iter::repeat_n(b, run));
            i += 1;
        }
    }
    Ok(out)
}

/// Predictor 2: each sample is stored as the difference from the one to
/// its left (per band, in the sample's own width).
fn undo_horizontal(raw: &mut [u8], w: usize, h: usize, spp: usize, bps: usize, le: bool) {
    let row = w * spp * bps;
    for r in 0..h {
        let line = &mut raw[r * row..(r + 1) * row];
        match bps {
            1 => {
                for i in spp..w * spp {
                    line[i] = line[i].wrapping_add(line[i - spp]);
                }
            }
            2 => {
                let get = |l: &[u8], i: usize| if le { u16::from_le_bytes([l[i * 2], l[i * 2 + 1]]) } else { u16::from_be_bytes([l[i * 2], l[i * 2 + 1]]) };
                for i in spp..w * spp {
                    let v = get(line, i).wrapping_add(get(line, i - spp));
                    let b = if le { v.to_le_bytes() } else { v.to_be_bytes() };
                    line[i * 2..i * 2 + 2].copy_from_slice(&b);
                }
            }
            4 => {
                let get = |l: &[u8], i: usize| {
                    let b: [u8; 4] = l[i * 4..i * 4 + 4].try_into().expect("4");
                    if le { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) }
                };
                for i in spp..w * spp {
                    let v = get(line, i).wrapping_add(get(line, i - spp));
                    let b = if le { v.to_le_bytes() } else { v.to_be_bytes() };
                    line[i * 4..i * 4 + 4].copy_from_slice(&b);
                }
            }
            _ => {
                let get = |l: &[u8], i: usize| {
                    let b: [u8; 8] = l[i * 8..i * 8 + 8].try_into().expect("8");
                    if le { u64::from_le_bytes(b) } else { u64::from_be_bytes(b) }
                };
                for i in spp..w * spp {
                    let v = get(line, i).wrapping_add(get(line, i - spp));
                    let b = if le { v.to_le_bytes() } else { v.to_be_bytes() };
                    line[i * 8..i * 8 + 8].copy_from_slice(&b);
                }
            }
        }
    }
}

/// Predictor 3 (floating point, Adobe TN3): each row's bytes are
/// differenced byte-wise, then stored as `bps` planes of the samples'
/// bytes most significant first. Returns the samples in the file's byte
/// order again.
fn undo_float(raw: &[u8], w: usize, h: usize, spp: usize, bps: usize, le: bool) -> Result<Vec<u8>, CogError> {
    if !matches!(bps, 2 | 4 | 8) {
        return Err(CogError::Unsupported(format!("floating-point predictor on {}-byte samples", bps)));
    }
    let n = w * spp;
    let row = n * bps;
    let mut out = vec![0u8; raw.len()];
    let mut line = vec![0u8; row];
    for r in 0..h {
        line.copy_from_slice(&raw[r * row..(r + 1) * row]);
        for i in spp..row {
            line[i] = line[i].wrapping_add(line[i - spp]);
        }
        for s in 0..n {
            for b in 0..bps {
                // Plane b holds byte b of each sample, big-endian order.
                let v = line[b * n + s];
                let at = if le { bps - 1 - b } else { b };
                out[r * row + s * bps + at] = v;
            }
        }
    }
    Ok(out)
}

/// JPEG: the tile's abbreviated stream with the shared `JPEGTables`
/// spliced in. YCbCr is converted by the decoder.
fn jpeg(bytes: &[u8], format: &Format, w: usize, h: usize) -> Result<Pixels, CogError> {
    let stream: std::borrow::Cow<[u8]> = match &format.jpeg_tables {
        Some(tables) if tables.len() > 4 && bytes.len() > 2 => {
            // tables = SOI .. EOI; tile = SOI .. EOI. Join as SOI tables tile-body.
            let mut s = Vec::with_capacity(tables.len() + bytes.len());
            s.extend_from_slice(&tables[..tables.len() - 2]);
            s.extend_from_slice(&bytes[2..]);
            s.into()
        }
        _ => bytes.into(),
    };
    let image = image::load_from_memory_with_format(&stream, image::ImageFormat::Jpeg).map_err(|e| CogError::Decode(format!("JPEG: {e}")))?;
    Ok(Pixels::Rgba(fit(image.to_rgba8(), w, h)))
}

fn webp(bytes: &[u8], w: usize, h: usize, rendering: &CogRendering, nodata: Option<f64>) -> Result<Pixels, CogError> {
    let image = image::load_from_memory_with_format(bytes, image::ImageFormat::WebP).map_err(|e| CogError::Decode(format!("WebP: {e}")))?;
    let rgba = image.to_rgba8();
    match rendering {
        CogRendering::Rgb { .. } => {
            let mut px = fit(rgba, w, h);
            let nd = nodata.filter(|v| (0.0..=255.0).contains(v)).map(|v| v as u8);
            for p in px.as_chunks_mut::<4>().0.iter_mut() {
                if nd.is_some_and(|n| p[0] == n && p[1] == n && p[2] == n) {
                    p[3] = 0;
                }
                let a = u16::from(p[3]);
                for c in &mut p[..3] {
                    *c = ((u16::from(*c) * a + 127) / 255) as u8;
                }
            }
            Ok(Pixels::Rgba(px))
        }
        CogRendering::Ramp { band, .. } => {
            let px = fit(rgba, w, h);
            let b = (*band).min(3);
            Ok(Pixels::Values(
                px.as_chunks::<4>().0.iter()
                    .map(|p| {
                        let v = f64::from(p[b]);
                        if nodata == Some(v) {
                            f32::NAN
                        } else {
                            v as f32
                        }
                    })
                    .collect(),
            ))
        }
    }
}

/// An image decoded to the tile's size (a codec may return a smaller
/// edge tile; pad it).
fn fit(image: image::RgbaImage, w: usize, h: usize) -> Vec<u8> {
    let (iw, ih) = (image.width() as usize, image.height() as usize);
    if iw == w && ih == h {
        return image.into_raw();
    }
    let raw = image.into_raw();
    let mut out = vec![0u8; w * h * 4];
    for y in 0..ih.min(h) {
        let n = iw.min(w) * 4;
        out[y * w * 4..y * w * 4 + n].copy_from_slice(&raw[y * iw * 4..y * iw * 4 + n]);
    }
    out
}

//! A small Cloud-Optimised GeoTIFF writer: tiled, with 2x overviews, the
//! directories first and the tile data after them, as GDAL's COG driver
//! lays a file out. For tests, fixtures and demos -- it writes what
//! [`super::CogSource`] reads (EPSG:3857 or a geographic CRS; u8 or f32
//! i16 or f32 samples; none, deflate or LZW; predictors 1-3; GDAL nodata and
//! statistics), not every GeoTIFF there is.

/// The samples, row-major, `spp` interleaved per pixel.
#[derive(Debug, Clone)]
pub enum WriteSamples {
    U8(Vec<u8>, usize),
    I16(Vec<i16>, usize),
    F32(Vec<f32>, usize),
}

impl WriteSamples {
    fn spp(&self) -> usize {
        match self {
            Self::U8(_, n) | Self::I16(_, n) | Self::F32(_, n) => *n,
        }
    }
}

/// Tile compression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteCompression {
    None,
    Deflate,
    Lzw,
    /// Each tile a complete baseline JPEG (no shared `JPEGTables`); u8
    /// RGB only.
    Jpeg,
    /// Each tile a lossless WebP; u8 RGB or RGBA only.
    WebP,
}

/// One image to write.
#[derive(Debug, Clone)]
pub struct WriteImage {
    pub width: u32,
    pub height: u32,
    pub samples: WriteSamples,
    /// Internal tile edge, px (a multiple of 16).
    pub tile: u32,
    /// Overviews to write; `None` halves until the image fits one tile.
    pub overviews: Option<usize>,
    pub compression: WriteCompression,
    /// 1 none, 2 horizontal (u8, i16), 3 floating point (f32).
    pub predictor: u16,
    /// The EPSG code, and whether it is geographic (else projected).
    pub epsg: u32,
    pub geographic: bool,
    /// The top-left corner, CRS units.
    pub origin: (f64, f64),
    /// Pixel size, CRS units (positive).
    pub resolution: (f64, f64),
    /// Written as `GDAL_NODATA`.
    pub nodata: Option<f64>,
    /// Written as GDAL `STATISTICS_MINIMUM`/`MAXIMUM` for band 0.
    pub stats: Option<(f64, f64)>,
    /// `MM` byte order instead of `II`.
    pub big_endian: bool,
}

impl WriteImage {
    /// A 256 x 256 grey u8 image in EPSG:3857 with its top-left corner at
    /// `(x0, y0)` metres and `resolution` metres per pixel; deflate.
    #[must_use]
    pub fn web_mercator_square(x0: f64, y0: f64, resolution: f64) -> Self {
        Self {
            width: 256,
            height: 256,
            samples: WriteSamples::U8(vec![128; 256 * 256], 1),
            tile: 128,
            overviews: None,
            compression: WriteCompression::Deflate,
            predictor: 1,
            epsg: 3857,
            geographic: false,
            origin: (x0, y0),
            resolution: (resolution, resolution),
            nodata: None,
            stats: None,
            big_endian: false,
        }
    }

    /// The same in EPSG:4326 degrees.
    #[must_use]
    pub fn geographic(lon0: f64, lat0: f64, resolution: f64) -> Self {
        Self { epsg: 4326, geographic: true, ..Self::web_mercator_square(lon0, lat0, resolution) }
    }
}

/// Halves an image (2x2 mean; NaN and `nodata` skipped for floats).
fn halve(samples: &WriteSamples, w: u32, h: u32, nodata: Option<f64>) -> (WriteSamples, u32, u32) {
    let (nw, nh) = (w.div_ceil(2), h.div_ceil(2));
    let spp = samples.spp();
    let idx = |x: u32, y: u32, c: usize| (y as usize * w as usize + x as usize) * spp + c;
    let quads = |x: u32, y: u32| {
        let xs = [2 * x, (2 * x + 1).min(w - 1)];
        let ys = [2 * y, (2 * y + 1).min(h - 1)];
        [(xs[0], ys[0]), (xs[1], ys[0]), (xs[0], ys[1]), (xs[1], ys[1])]
    };
    match samples {
        WriteSamples::U8(v, _) => {
            let mut out = Vec::with_capacity((nw * nh) as usize * spp);
            for y in 0..nh {
                for x in 0..nw {
                    for c in 0..spp {
                        let sum: u32 = quads(x, y).iter().map(|&(a, b)| u32::from(v[idx(a, b, c)])).sum();
                        out.push(((sum + 2) / 4) as u8);
                    }
                }
            }
            (WriteSamples::U8(out, spp), nw, nh)
        }
        WriteSamples::I16(v, _) => {
            let as_f32 = WriteSamples::F32(v.iter().map(|&s| f32::from(s)).collect(), spp);
            let (WriteSamples::F32(half, _), nw, nh) = halve(&as_f32, w, h, nodata) else { unreachable!("f32 in, f32 out") };
            (WriteSamples::I16(half.iter().map(|&s| s.round() as i16).collect(), spp), nw, nh)
        }
        WriteSamples::F32(v, _) => {
            let mut out = Vec::with_capacity((nw * nh) as usize * spp);
            for y in 0..nh {
                for x in 0..nw {
                    for c in 0..spp {
                        let good: Vec<f32> = quads(x, y)
                            .iter()
                            .map(|&(a, b)| v[idx(a, b, c)])
                            .filter(|s| !s.is_nan() && nodata.is_none_or(|n| f64::from(*s) != n))
                            .collect();
                        out.push(if good.is_empty() {
                            nodata.map_or(f32::NAN, |n| n as f32)
                        } else {
                            good.iter().sum::<f32>() / good.len() as f32
                        });
                    }
                }
            }
            (WriteSamples::F32(out, spp), nw, nh)
        }
    }
}

/// One tile's raw bytes (file byte order), padded to the full tile.
fn tile_bytes(samples: &WriteSamples, w: u32, h: u32, tx: u32, ty: u32, img: &WriteImage) -> Vec<u8> {
    let t = img.tile;
    let spp = samples.spp();
    let be = img.big_endian;
    let mut raw = Vec::new();
    for y in 0..t {
        let sy = ty * t + y;
        let mut row: Vec<u8> = Vec::new();
        for x in 0..t {
            let sx = tx * t + x;
            for c in 0..spp {
                let inside = sx < w && sy < h;
                let i = (sy as usize * w as usize + sx as usize) * spp + c;
                match samples {
                    WriteSamples::U8(v, _) => row.push(if inside { v[i] } else { 0 }),
                    WriteSamples::I16(v, _) => {
                        let s = if inside { v[i] } else { img.nodata.map_or(0, |n| n as i16) };
                        row.extend_from_slice(&if be { s.to_be_bytes() } else { s.to_le_bytes() });
                    }
                    WriteSamples::F32(v, _) => {
                        let s = if inside { v[i] } else { img.nodata.map_or(0.0, |n| n as f32) };
                        row.extend_from_slice(&if be { s.to_be_bytes() } else { s.to_le_bytes() });
                    }
                }
            }
        }
        match (img.predictor, samples) {
            (2, WriteSamples::U8(..)) => {
                for i in (spp..row.len()).rev() {
                    row[i] = row[i].wrapping_sub(row[i - spp]);
                }
            }
            (2, WriteSamples::I16(..)) => {
                let get = |r: &[u8], k: usize| if be { i16::from_be_bytes([r[k * 2], r[k * 2 + 1]]) } else { i16::from_le_bytes([r[k * 2], r[k * 2 + 1]]) };
                for k in (spp..row.len() / 2).rev() {
                    let d = get(&row, k).wrapping_sub(get(&row, k - spp));
                    row[k * 2..k * 2 + 2].copy_from_slice(&if be { d.to_be_bytes() } else { d.to_le_bytes() });
                }
            }
            (3, WriteSamples::F32(..)) => {
                // Byte planes, most significant first, then byte deltas.
                let n = t as usize * spp;
                let mut planes = vec![0u8; row.len()];
                for s in 0..n {
                    for b in 0..4 {
                        let at = if be { b } else { 3 - b };
                        planes[b * n + s] = row[s * 4 + at];
                    }
                }
                for i in (spp..planes.len()).rev() {
                    planes[i] = planes[i].wrapping_sub(planes[i - spp]);
                }
                row = planes;
            }
            _ => {}
        }
        raw.extend_from_slice(&row);
    }
    match img.compression {
        WriteCompression::None => raw,
        WriteCompression::Jpeg | WriteCompression::WebP => {
            let (t, spp) = (img.tile, samples.spp());
            let color = if spp == 4 { image::ExtendedColorType::Rgba8 } else { image::ExtendedColorType::Rgb8 };
            let mut out = Vec::new();
            if img.compression == WriteCompression::Jpeg {
                image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 95).encode(&raw, t, t, color).expect("JPEG into a Vec");
            } else {
                image::codecs::webp::WebPEncoder::new_lossless(&mut out).encode(&raw, t, t, color).expect("WebP into a Vec");
            }
            out
        }
        WriteCompression::Deflate => {
            use std::io::Write as _;
            let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            z.write_all(&raw).expect("deflate into a Vec");
            z.finish().expect("deflate into a Vec")
        }
        WriteCompression::Lzw => weezl::encode::Encoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8).encode(&raw).expect("LZW into a Vec"),
    }
}

/// A directory entry before layout: tag, type, count, value bytes.
struct Entry {
    tag: u16,
    ty: u16,
    count: u32,
    data: Vec<u8>,
}

/// Writes the image as a COG and returns the file's bytes.
///
/// # Panics
///
/// When the samples are not `width * height * spp` long.
#[must_use]
pub fn write(img: &WriteImage) -> Vec<u8> {
    let spp = img.samples.spp();
    let len = match &img.samples {
        WriteSamples::U8(v, _) => v.len(),
        WriteSamples::I16(v, _) => v.len(),
        WriteSamples::F32(v, _) => v.len(),
    };
    assert_eq!(len, img.width as usize * img.height as usize * spp, "samples are width * height * spp long");
    let be = img.big_endian;
    let u16b = |v: u16| if be { v.to_be_bytes().to_vec() } else { v.to_le_bytes().to_vec() };
    let u32b = |v: u32| if be { v.to_be_bytes().to_vec() } else { v.to_le_bytes().to_vec() };
    let f64b = |v: f64| if be { v.to_be_bytes().to_vec() } else { v.to_le_bytes().to_vec() };

    // Levels.
    let mut levels = vec![(img.samples.clone(), img.width, img.height)];
    let wanted = img.overviews.unwrap_or(usize::MAX);
    while levels.len() <= wanted {
        let (s, w, h) = levels.last().expect("a level");
        if img.overviews.is_none() && *w <= img.tile && *h <= img.tile {
            break;
        }
        if *w < 2 || *h < 2 {
            break;
        }
        let next = halve(s, *w, *h, img.nodata);
        levels.push(next);
    }
    let tiles: Vec<Vec<Vec<u8>>> = levels
        .iter()
        .map(|(s, w, h)| {
            let (across, down) = (w.div_ceil(img.tile), h.div_ceil(img.tile));
            (0..down).flat_map(|ty| (0..across).map(move |tx| (tx, ty))).map(|(tx, ty)| tile_bytes(s, *w, *h, tx, ty, img)).collect()
        })
        .collect();

    let (bits, format) = match img.samples {
        WriteSamples::U8(..) => (8u16, 1u16),
        WriteSamples::I16(..) => (16, 2),
        WriteSamples::F32(..) => (32, 3),
    };
    let shorts = |v: &[u16]| v.iter().flat_map(|&x| u16b(x)).collect::<Vec<u8>>();
    let entries_for = |level: usize, offsets: &[u32]| -> Vec<Entry> {
        let (_, w, h) = &levels[level];
        let counts: Vec<u32> = tiles[level].iter().map(|t| t.len() as u32).collect();
        let mut e = vec![
            Entry { tag: 254, ty: 4, count: 1, data: u32b(u32::from(level > 0)) },
            Entry { tag: 256, ty: 4, count: 1, data: u32b(*w) },
            Entry { tag: 257, ty: 4, count: 1, data: u32b(*h) },
            Entry { tag: 258, ty: 3, count: spp as u32, data: shorts(&vec![bits; spp]) },
            Entry {
                tag: 259,
                ty: 3,
                count: 1,
                data: u16b(match img.compression {
                    WriteCompression::None => 1,
                    WriteCompression::Deflate => 8,
                    WriteCompression::Lzw => 5,
                    WriteCompression::Jpeg => 7,
                    WriteCompression::WebP => 50001,
                }),
            },
            Entry { tag: 262, ty: 3, count: 1, data: u16b(if bits == 8 && spp >= 3 { 2 } else { 1 }) },
            Entry { tag: 277, ty: 3, count: 1, data: u16b(spp as u16) },
            Entry { tag: 284, ty: 3, count: 1, data: u16b(1) },
        ];
        if img.predictor != 1 {
            e.push(Entry { tag: 317, ty: 3, count: 1, data: u16b(img.predictor) });
        }
        e.push(Entry { tag: 322, ty: 4, count: 1, data: u32b(img.tile) });
        e.push(Entry { tag: 323, ty: 4, count: 1, data: u32b(img.tile) });
        e.push(Entry { tag: 324, ty: 4, count: offsets.len() as u32, data: offsets.iter().flat_map(|&o| u32b(o)).collect() });
        e.push(Entry { tag: 325, ty: 4, count: counts.len() as u32, data: counts.iter().flat_map(|&o| u32b(o)).collect() });
        if spp == 4 || spp == 2 {
            e.push(Entry { tag: 338, ty: 3, count: 1, data: u16b(2) });
        }
        e.push(Entry { tag: 339, ty: 3, count: spp as u32, data: shorts(&vec![format; spp]) });
        if level == 0 {
            e.push(Entry { tag: 33550, ty: 12, count: 3, data: [img.resolution.0, img.resolution.1, 0.0].iter().flat_map(|&v| f64b(v)).collect() });
            e.push(Entry {
                tag: 33922,
                ty: 12,
                count: 6,
                data: [0.0, 0.0, 0.0, img.origin.0, img.origin.1, 0.0].iter().flat_map(|&v| f64b(v)).collect(),
            });
            let keys: Vec<u16> = if img.geographic {
                vec![1, 1, 0, 3, 1024, 0, 1, 2, 1025, 0, 1, 1, 2048, 0, 1, img.epsg as u16]
            } else {
                vec![1, 1, 0, 3, 1024, 0, 1, 1, 1025, 0, 1, 1, 3072, 0, 1, img.epsg as u16]
            };
            e.push(Entry { tag: 34735, ty: 3, count: keys.len() as u32, data: shorts(&keys) });
            if let Some((min, max)) = img.stats {
                let xml = format!(
                    "<GDALMetadata>\n  <Item name=\"STATISTICS_MAXIMUM\" sample=\"0\">{max}</Item>\n  <Item name=\"STATISTICS_MINIMUM\" sample=\"0\">{min}</Item>\n</GDALMetadata>\n\0"
                );
                e.push(Entry { tag: 42112, ty: 2, count: xml.len() as u32, data: xml.into_bytes() });
            }
            if let Some(n) = img.nodata {
                let text = format!("{n}\0");
                e.push(Entry { tag: 42113, ty: 2, count: text.len() as u32, data: text.into_bytes() });
            }
        }
        e
    };

    // Pass 1: the directories' sizes (offset values do not change them).
    let ifd_size = |entries: &[Entry]| -> usize {
        let out_of_line: usize = entries.iter().filter(|e| e.data.len() > 4).map(|e| e.data.len().div_ceil(2) * 2).sum();
        2 + entries.len() * 12 + 4 + out_of_line
    };
    let placeholder: Vec<Vec<u32>> = tiles.iter().map(|t| vec![0; t.len()]).collect();
    let sizes: Vec<usize> = (0..levels.len()).map(|l| ifd_size(&entries_for(l, &placeholder[l]))).collect();
    let header_end = 8 + sizes.iter().sum::<usize>();
    // Tile data: the smallest overview first, as GDAL writes.
    let mut offsets: Vec<Vec<u32>> = placeholder;
    let mut at = header_end;
    for l in (0..levels.len()).rev() {
        for (i, t) in tiles[l].iter().enumerate() {
            offsets[l][i] = at as u32;
            at += t.len();
        }
    }

    // Pass 2.
    let mut out = Vec::with_capacity(at);
    out.extend_from_slice(if be { b"MM" } else { b"II" });
    out.extend_from_slice(&u16b(42));
    out.extend_from_slice(&u32b(8));
    let mut ifd_at = 8usize;
    for l in 0..levels.len() {
        let mut entries = entries_for(l, &offsets[l]);
        entries.sort_by_key(|e| e.tag);
        let mut data_at = ifd_at + 2 + entries.len() * 12 + 4;
        let mut tail = Vec::new();
        out.extend_from_slice(&u16b(entries.len() as u16));
        for e in &entries {
            out.extend_from_slice(&u16b(e.tag));
            out.extend_from_slice(&u16b(e.ty));
            out.extend_from_slice(&u32b(e.count));
            if e.data.len() <= 4 {
                let mut v = e.data.clone();
                v.resize(4, 0);
                out.extend_from_slice(&v);
            } else {
                out.extend_from_slice(&u32b(data_at as u32));
                tail.extend_from_slice(&e.data);
                if e.data.len() % 2 == 1 {
                    tail.push(0);
                }
                data_at += e.data.len().div_ceil(2) * 2;
            }
        }
        let next = if l + 1 < levels.len() { ifd_at + sizes[l] } else { 0 };
        out.extend_from_slice(&u32b(next as u32));
        out.extend_from_slice(&tail);
        ifd_at += sizes[l];
        debug_assert_eq!(out.len(), ifd_at);
    }
    for l in (0..levels.len()).rev() {
        for t in &tiles[l] {
            out.extend_from_slice(t);
        }
    }
    out
}

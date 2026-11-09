use super::write::{write, WriteCompression, WriteImage, WriteSamples};
use super::*;
use crate::geo::TileId;

const WORLD: f64 = 2.0 * HALF_WORLD;

/// An RGB image exactly covering map tile z4 (8, 5) at 1024 px (full
/// resolution = z5 at 512 px tiles), 256 px internal tiles, so two
/// overviews (512, 256). Red follows x, green y, blue is 77.
fn rgb_3857() -> (Vec<u8>, TileId) {
    let tile = TileId { z: 4, x: 8, y: 5 };
    let span = WORLD / 16.0;
    let x0 = -HALF_WORLD + f64::from(tile.x) * span;
    let y0 = HALF_WORLD - f64::from(tile.y) * span;
    let n = 1024u32;
    let mut px = Vec::with_capacity((n * n * 3) as usize);
    for y in 0..n {
        for x in 0..n {
            px.extend_from_slice(&[(x / 4) as u8, (y / 4) as u8, 77]);
        }
    }
    let img = WriteImage {
        width: n,
        height: n,
        samples: WriteSamples::U8(px, 3),
        tile: 256,
        predictor: 2,
        ..WriteImage::web_mercator_square(x0, y0, span / f64::from(n))
    };
    (write(&img), tile)
}

/// A float DEM in EPSG:4326: 400 x 200 px at 0.05 deg over lon 10..30,
/// lat 50..40, value = column index, with a nodata square in the middle.
fn dem_4326(compression: WriteCompression, predictor: u16, big_endian: bool) -> Vec<u8> {
    let (w, h) = (400u32, 200u32);
    let mut v = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        for x in 0..w {
            let hole = (180..220).contains(&x) && (80..120).contains(&y);
            v.push(if hole { -9999.0 } else { x as f32 });
        }
    }
    write(&WriteImage {
        width: w,
        height: h,
        samples: WriteSamples::F32(v, 1),
        tile: 64,
        compression,
        predictor,
        nodata: Some(-9999.0),
        stats: Some((0.0, 399.0)),
        big_endian,
        ..WriteImage::geographic(10.0, 50.0, 0.05)
    })
}

fn render(cog: &CogSource, tile: TileId) -> Option<image::RgbaImage> {
    block_ready(cog.render_tile(tile)).expect("renders")
}

#[test]
fn an_rgb_cog_reads_its_layout_and_crs() {
    let (bytes, _) = rgb_3857();
    let cog = CogSource::from_bytes(bytes, CogOptions::default()).unwrap();
    let m = cog.metadata();
    assert_eq!((m.width, m.height, m.bands), (1024, 1024, 3));
    assert_eq!(m.dtype, CogDataType::U8);
    assert_eq!(m.compression, CogCompression::Deflate);
    assert_eq!(m.crs, CogCrs::WebMercator { epsg: 3857 });
    assert_eq!(m.overviews, vec![(512, 512), (256, 256)]);
    assert_eq!(m.rendering, CogRendering::Rgb { alpha: false });
    assert_eq!(m.max_zoom, 5);
    let want = crate::geo::tile_bounds_lonlat(TileId { z: 4, x: 8, y: 5 });
    for (got, want) in m.bounds.iter().zip([want.min_lon, want.min_lat, want.max_lon, want.max_lat]) {
        assert!((got - want).abs() < 1e-9, "{:?} vs {want:?}", m.bounds);
    }
    assert_eq!(m.header_requests, 1);
}

#[test]
fn the_overview_matches_the_map_zoom() {
    let (bytes, _) = rgb_3857();
    let cog = CogSource::from_bytes(bytes, CogOptions::default()).unwrap();
    let m = cog.metadata();
    // 512 px map tiles: z5 is full resolution, z4 the 512 overview, z3
    // the 256 one, and anything coarser still the coarsest.
    assert_eq!(m.level_for(6, 512), 0, "past full resolution: the full image, magnified");
    assert_eq!(m.level_for(5, 512), 0);
    assert_eq!(m.level_for(4, 512), 1);
    assert_eq!(m.level_for(3, 512), 2);
    assert_eq!(m.level_for(0, 512), 2);
    // 256 px tiles ask one level finer for the same zoom.
    assert_eq!(m.level_for(4, 256), 2);
    assert_eq!(m.level_for(5, 256), 1);
    assert_eq!(m.level_for(6, 256), 0);
}

#[test]
fn a_map_tile_on_the_files_own_grid_is_its_pixels() {
    let (bytes, tile) = rgb_3857();
    let cog = CogSource::from_bytes(bytes, CogOptions { resampling: CogResampling::Nearest, ..CogOptions::default() }).unwrap();
    // z5 (16, 10) is the top-left quarter at full resolution: map pixel
    // (i, j) is file pixel (i, j).
    let img = render(&cog, TileId { z: 5, x: 16, y: 10 }).expect("inside");
    for &(i, j) in &[(0u32, 0u32), (100, 3), (511, 511), (257, 129)] {
        assert_eq!(img.get_pixel(i, j).0, [(i / 4) as u8, (j / 4) as u8, 77, 255], "pixel {i},{j}");
    }
    // The z4 tile reads the 512 overview: pixel (i, j) is file (2i, 2j).
    let img = render(&cog, tile).expect("inside");
    let p = img.get_pixel(300, 100).0;
    assert!((i32::from(p[0]) - 150).abs() <= 1 && (i32::from(p[1]) - 50).abs() <= 1, "{p:?}");
    let s = cog.stats();
    assert_eq!(s.map_tiles, 2);
    // z5: 2 x 2 of 256 px internal tiles (+1 px margin reaches a third
    // column/row); z4: the 512 overview's 2 x 2.
    assert!(s.tiles_decoded >= 8 && s.tiles_decoded <= 13, "{s:?}");
}

#[test]
fn bilinear_sampling_between_two_pixels_is_their_mean() {
    let (bytes, _) = rgb_3857();
    let cog = CogSource::from_bytes(bytes, CogOptions::default()).unwrap();
    // z6: each file pixel spans two map pixels, so a map pixel centre sits
    // a quarter of the way between two file pixel centres.
    let img = render(&cog, TileId { z: 6, x: 32, y: 20 }).expect("inside");
    let red = |i| f64::from(img.get_pixel(i, 0).0[0]);
    // File pixels 0..=3 are red 0, 4..=7 red 1: at map pixel 7 (file
    // x 3.25) the weight is 0.25 toward red 1 -- rounds to 0; at 8 (3.75),
    // 0.75 -- rounds to 1.
    assert_eq!((red(7), red(8)), (0.0, 1.0));
}

#[test]
fn a_tile_outside_the_image_is_absent_not_an_error() {
    let (bytes, _) = rgb_3857();
    let cog = CogSource::from_bytes(bytes, CogOptions::default()).unwrap();
    assert!(render(&cog, TileId { z: 4, x: 0, y: 0 }).is_none());
    assert!(render(&cog, TileId { z: 6, x: 31, y: 20 }).is_none());
    // A map tile larger than the image: drawn, transparent around it.
    let img = render(&cog, TileId { z: 2, x: 2, y: 1 }).expect("overlaps");
    assert_eq!(img.get_pixel(0, 0).0[3], 0);
    // z4 (8, 5) is z2 (2, 1)'s 128 px square at column 0, row 1.
    assert_eq!(img.get_pixel(64, 192).0[3], 255);
    assert_eq!(img.get_pixel(64, 64).0[3], 0);
}

#[test]
fn single_band_data_takes_the_ramp_and_the_files_statistics_and_nodata() {
    for (compression, predictor, be) in [
        (WriteCompression::Deflate, 3, false),
        (WriteCompression::Lzw, 1, true),
        (WriteCompression::None, 3, true),
        (WriteCompression::Lzw, 3, false),
    ] {
        let bytes = dem_4326(compression, predictor, be);
        let cog = CogSource::from_bytes(bytes, CogOptions::default().with_ramp(CogRamp::Greys)).unwrap();
        let m = cog.metadata();
        assert_eq!(m.crs, CogCrs::Geographic { epsg: 4326 });
        assert_eq!(m.dtype, CogDataType::F32);
        assert_eq!(m.nodata, Some(-9999.0));
        assert_eq!(m.stats, Some((0.0, 399.0)));
        assert_eq!(m.rendering, CogRendering::Ramp { band: 0, min: 0.0, max: 399.0 });
        assert_eq!(m.bounds, [10.0, 40.0, 30.0, 50.0]);
        // A map pixel at lon 25.0, lat 45.0 (where column 300 is) reads
        // grey 300/399*255; the hole at lon 20, lat 45 is transparent.
        let z = 6u8;
        let probe = |lon: f64, lat: f64| {
            let (tx, ty) = crate::geo::lon_lat_to_tile_xy(lon, lat, f64::from(z));
            let tile = TileId { z, x: tx.floor() as u32, y: ty.floor() as u32 };
            let img = render(&cog, tile).expect("inside");
            img.get_pixel(((tx.fract()) * 512.0) as u32, ((ty.fract()) * 512.0) as u32).0
        };
        let p = probe(25.0 + 0.025, 45.0);
        let want = (300.0 / 399.0 * 255.0) as i32;
        assert!((i32::from(p[0]) - want).abs() <= 2 && p[3] == 255, "{compression:?}/{predictor}/{be}: {p:?} vs {want}");
        assert_eq!(probe(20.0, 45.0)[3], 0, "nodata is transparent");
    }
}

#[test]
fn int16_samples_with_the_horizontal_predictor_read_back_exactly() {
    let (w, h) = (300u32, 200u32);
    let v: Vec<i16> = (0..w * h).map(|i| ((i % w) as i16 - 150) * 7).collect();
    let bytes = write(&WriteImage {
        width: w,
        height: h,
        samples: WriteSamples::I16(v, 1),
        tile: 64,
        predictor: 2,
        big_endian: true,
        ..WriteImage::geographic(0.0, 10.0, 0.01)
    });
    let cog = CogSource::from_bytes(bytes, CogOptions::default().with_range(-1050.0, 1043.0)).unwrap();
    assert_eq!(cog.metadata().dtype, CogDataType::I16);
    // A full-resolution map pixel: its value is the column's.
    let z = 12u8;
    let (tx, ty) = crate::geo::lon_lat_to_tile_xy(1.0 + 0.005, 9.5, f64::from(z));
    let tile = TileId { z, x: tx.floor() as u32, y: ty.floor() as u32 };
    let values = block_ready(cog.values_tile(tile, 512)).unwrap().expect("inside");
    let at = ((ty.fract() * 512.0) as usize) * 512 + (tx.fract() * 512.0) as usize;
    assert!((values[at] - (100.0 - 150.0) * 7.0).abs() < 7.5, "{}", values[at]);
}

#[test]
fn jpeg_and_webp_tiles_decode_to_the_colours_written() {
    use super::write::WriteCompression;
    for compression in [WriteCompression::Jpeg, WriteCompression::WebP] {
        let (w, h) = (256u32, 256u32);
        let px: Vec<u8> = (0..w * h).flat_map(|i| if (i % w) < 128 { [200u8, 40, 40] } else { [30, 90, 210] }).collect();
        let bytes = write(&WriteImage {
            width: w,
            height: h,
            samples: WriteSamples::U8(px, 3),
            tile: 128,
            compression,
            ..WriteImage::web_mercator_square(0.0, 0.0, 2.0 * HALF_WORLD / 256.0 / 64.0)
        });
        let cog = CogSource::from_bytes(bytes, CogOptions { resampling: CogResampling::Nearest, ..CogOptions::default() }).unwrap();
        assert_eq!(cog.metadata().rendering, CogRendering::Rgb { alpha: false }, "{compression:?}");
        // The image is z6 tile (32, 32) at 256 px: z6 at 512 px tiles
        // magnifies it 2x. Left half red, right half blue.
        let img = render(&cog, TileId { z: 6, x: 32, y: 32 }).expect("inside");
        let (l, r) = (img.get_pixel(100, 300).0, img.get_pixel(400, 300).0);
        let near = |a: [u8; 4], b: [u8; 3]| (0..3).all(|k| (i32::from(a[k]) - i32::from(b[k])).abs() <= 6) && a[3] == 255;
        assert!(near(l, [200, 40, 40]) && near(r, [30, 90, 210]), "{compression:?}: {l:?} {r:?}");
    }
}

#[test]
fn options_override_the_files_range_and_nodata() {
    let bytes = dem_4326(WriteCompression::Deflate, 1, false);
    let cog = CogSource::from_bytes(bytes, CogOptions::default().with_range(100.0, 200.0).with_nodata(5.0)).unwrap();
    assert_eq!(cog.metadata().rendering, CogRendering::Ramp { band: 0, min: 100.0, max: 200.0 });
    assert_eq!(cog.metadata().nodata, Some(5.0));
}

#[test]
fn single_band_data_with_no_range_anywhere_is_refused_with_the_fix() {
    let mut img = WriteImage::geographic(0.0, 10.0, 0.1);
    img.samples = WriteSamples::F32(vec![1.0; 256 * 256], 1);
    let err = CogSource::from_bytes(write(&img), CogOptions::default()).unwrap_err();
    assert_eq!(err, CogError::NoRange);
    assert!(err.to_string().contains("with_range"));
}

/// A map pixel's centre, through the plan's source-pixel coordinate and
/// the file's geotransform, comes back to the same longitude/latitude.
#[test]
fn map_pixels_round_trip_through_file_pixels_to_lon_lat() {
    let bytes = dem_4326(WriteCompression::Deflate, 1, false);
    let cog = CogSource::from_bytes(bytes, CogOptions::default()).unwrap();
    for (z, x, y) in [(6u8, 35u32, 22u32), (8, 140, 89), (5, 17, 11)] {
        let tile = TileId { z, x, y };
        let Some(plan) = cog.cog.plan(tile, 512) else { continue };
        let lay = &cog.cog.levels[plan.level];
        for &(i, j) in &[(0usize, 0usize), (255, 31), (511, 511)] {
            let lon = lay.origin.0 + (plan.xs[i + 1] + 0.5) * lay.res.0;
            let lat = lay.origin.1 - (plan.ys[j + 1] + 0.5) * lay.res.1;
            let (want_lon, want_lat) = crate::geo::tile_xy_to_lon_lat(
                f64::from(x) + (i as f64 + 0.5) / 512.0,
                f64::from(y) + (j as f64 + 0.5) / 512.0,
                f64::from(z),
            );
            assert!((lon - want_lon).abs() < 1e-9 && (lat - want_lat).abs() < 1e-9, "z{z} ({i},{j}): {lon},{lat} vs {want_lon},{want_lat}");
        }
    }
    // And for 3857: x is linear in metres.
    let (bytes, _) = rgb_3857();
    let cog = CogSource::from_bytes(bytes, CogOptions::default()).unwrap();
    let plan = cog.cog.plan(TileId { z: 5, x: 16, y: 10 }, 512).unwrap();
    assert!((plan.xs[1] - 0.0).abs() < 1e-6 && (plan.xs[512] - 511.0).abs() < 1e-6);
    assert!((plan.ys[1] - 0.0).abs() < 1e-6 && (plan.ys[512] - 511.0).abs() < 1e-6);
    assert_eq!(plan.xs.len(), 514, "a border column each side");
}

#[test]
fn a_second_render_of_the_same_tile_reads_nothing() {
    let (bytes, tile) = rgb_3857();
    let cog = CogSource::from_bytes(bytes, CogOptions::default()).unwrap();
    render(&cog, tile).unwrap();
    let first = cog.stats();
    render(&cog, tile).unwrap();
    let second = cog.stats();
    assert_eq!(second.requests, first.requests, "all from the cache");
    assert_eq!(second.tiles_decoded, first.tiles_decoded);
    assert!(second.cache_hits > first.cache_hits);
}

#[test]
fn the_cache_is_bounded_by_bytes() {
    let (bytes, _) = rgb_3857();
    // One 256 px RGBA tile is 256 KiB; room for two.
    let cog = CogSource::from_bytes(bytes, CogOptions { cache_bytes: 600 << 10, ..CogOptions::default() }).unwrap();
    for x in 16..18 {
        for y in 10..12 {
            render(&cog, TileId { z: 5, x, y }).unwrap();
        }
    }
    assert!(cog.cog.cache_bytes.load(std::sync::atomic::Ordering::Relaxed) <= 600 << 10);
    assert!(cog.cog.cache.lock().unwrap().len() <= 2);
}

/// The decoded-tile LRU under a long tour: every map tile at z5-z7 over the
/// image, three passes, with room for four internal tiles. The byte bound
/// holds after every render, not just at the end.
#[test]
fn the_cache_stays_bounded_over_a_long_tour() {
    let (bytes, _) = rgb_3857();
    let cap = 1 << 20;
    let cog = CogSource::from_bytes(bytes, CogOptions { cache_bytes: cap, ..CogOptions::default() }).unwrap();
    let mut renders = 0;
    for _ in 0..3 {
        for z in 5..=7u8 {
            let scale = 1u32 << (z - 4);
            for y in 5 * scale..6 * scale {
                for x in 8 * scale..9 * scale {
                    render(&cog, TileId { z, x, y }).unwrap();
                    renders += 1;
                    assert!(cog.cog.cache_bytes.load(std::sync::atomic::Ordering::Relaxed) <= cap as u64);
                    assert!(cog.cog.cache.lock().unwrap().len() <= 4);
                }
            }
        }
    }
    assert_eq!(renders, 3 * (4 + 16 + 64));
    assert!(cog.stats().tiles_decoded > 16, "the tour must churn the cache");
}

#[test]
fn coalescing_merges_neighbours_and_splits_at_the_gap_and_the_cap() {
    let r = |i: u32, start: u64, len: u64| ByteRange { key: (0, 0, i), start, len };
    // Three adjacent, one 10 bytes after, one far away.
    let ranges = [r(2, 200, 100), r(0, 0, 100), r(1, 100, 100), r(3, 310, 50), r(4, 10_000, 10)];
    let groups = coalesce(&ranges, 16, 1 << 20);
    assert_eq!(groups.len(), 2);
    assert_eq!((groups[0].start, groups[0].len, groups[0].members.len()), (0, 360, 4));
    assert_eq!((groups[1].start, groups[1].len), (10_000, 10));
    // No gap allowed: the 10-byte hole splits.
    assert_eq!(coalesce(&ranges, 0, 1 << 20).len(), 3);
    // A cap of 250 bytes: 0..200 then 200..360.
    let capped = coalesce(&ranges, 16, 250);
    assert_eq!(capped.iter().map(|g| (g.start, g.len)).collect::<Vec<_>>(), vec![(0, 200), (200, 160), (10_000, 10)]);
    // The same tile twice is fetched once.
    assert_eq!(coalesce(&[r(0, 0, 10), r(0, 0, 10)], 0, 100)[0].members.len(), 1);
    assert!(coalesce(&[], 16, 100).is_empty());
}

#[test]
fn one_map_tile_is_one_request_on_a_cog_written_row_by_row() {
    let (bytes, tile) = rgb_3857();
    let dir = std::env::temp_dir().join(format!("gpui-maps-cog-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("rgb.tif");
    std::fs::write(&path, &bytes).unwrap();
    let cog = CogSource::open_blocking(path.to_str().unwrap(), CogOptions::default()).unwrap();
    render(&cog, tile).unwrap();
    // The 512 overview's four tiles are consecutive in the file.
    assert_eq!(cog.stats().requests, 1, "{:?}", cog.stats());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn refusals_name_what_is_not_read() {
    let (bytes, _) = rgb_3857();
    // Patch the compression to ZSTD (50000) in the first IFD.
    let mut zstd = bytes.clone();
    let patch = |b: &mut Vec<u8>, tag: u16, value: u16| {
        let n = u16::from_le_bytes([b[8], b[9]]) as usize;
        for e in 0..n {
            let at = 10 + e * 12;
            if u16::from_le_bytes([b[at], b[at + 1]]) == tag {
                b[at + 8..at + 10].copy_from_slice(&value.to_le_bytes());
            }
        }
    };
    patch(&mut zstd, 259, 50000);
    let err = CogSource::from_bytes(zstd, CogOptions::default()).unwrap_err();
    assert!(err.to_string().contains("ZSTD"), "{err}");

    let mut utm = WriteImage::web_mercator_square(500_000.0, 6_000_000.0, 10.0);
    utm.epsg = 32633;
    let err = CogSource::from_bytes(write(&utm), CogOptions::default()).unwrap_err();
    assert!(matches!(&err, CogError::Crs(s) if s.contains("EPSG:32633")), "{err}");

    let mut nad = WriteImage::geographic(-122.0, 47.0, 0.001);
    nad.epsg = 4269;
    let cog = CogSource::from_bytes(write(&nad), CogOptions::default().with_band(0).with_range(0.0, 255.0)).unwrap();
    assert!(cog.metadata().warnings.iter().any(|w| w.contains("NAD83")));

    assert!(matches!(CogSource::from_bytes(b"GIF89a....".to_vec(), CogOptions::default()), Err(CogError::NotTiff(_))));
}

#[test]
fn a_header_larger_than_the_first_read_takes_a_second_request() {
    // 16 x 16 px tiles over 2048 x 2048: 16,384 tiles at level 0, whose
    // offset and count arrays (64 KiB each) end past the first 64 KiB.
    let n = 2048u32;
    let img = WriteImage {
        width: n,
        height: n,
        samples: WriteSamples::U8(vec![9; (n * n) as usize], 1),
        tile: 16,
        overviews: Some(0),
        compression: WriteCompression::Deflate,
        ..WriteImage::web_mercator_square(0.0, 0.0, 1.0)
    };
    let cog = CogSource::from_bytes(write(&img), CogOptions::default().with_range(0.0, 10.0)).unwrap();
    assert_eq!(cog.metadata().header_requests, 2);
}

#[test]
fn the_tile_source_hands_the_map_pixels_and_states_its_ceiling_and_credit() {
    use crate::tile_source::TileSource as _;
    let (bytes, tile) = rgb_3857();
    let cog = CogSource::from_bytes(bytes, CogOptions::default().with_attribution("Test imagery")).unwrap();
    assert_eq!(cog.max_zoom(), 5);
    assert_eq!(cog.attribution(), Some("Test imagery"));
    let img = block_ready(cog.fetch_rgba(tile, 512)).unwrap().expect("inside");
    assert_eq!(img.dimensions(), (512, 512));
    let png = block_ready(cog.fetch(tile)).unwrap().expect("inside");
    assert_eq!(crate::raster::decode_tile(&png, 512).unwrap().dimensions(), (512, 512));
}

#[test]
fn the_ramp_lut_runs_from_the_first_stop_to_the_last() {
    let lut = CogRamp::Greys.lut();
    assert_eq!(lut[0], [0, 0, 0, 255]);
    assert_eq!(lut[255], [255, 255, 255, 255]);
    assert_eq!(lut[128][0], 128);
}

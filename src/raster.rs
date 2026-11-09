//! **Raster tiles: bytes in, pixels out.**
//!
//! The counterpart of [`crate::mvt`] for a tile whose content is imagery
//! rather than geometry. Everything upstream of here is shared: a raster
//! tile is fetched through the very same
//! [`TileSource`](crate::tile_source::TileSource) trait a vector tile is,
//! because that trait returns opaque `Vec<u8>` and has no opinion about
//! what is in them. The format decision happens HERE, after the fetch, and
//! nowhere earlier.
//!
//! What this module does NOT do is as important as what it does. It does
//! not fetch, it does not cache, it does not touch a GPU, and it does not
//! know what a tile is for. It turns bytes into an
//! [`image::RgbaImage`] of a stated size, or says why it could not. That
//! keeps it testable with no network and no device, which is the only
//! reason the tests below can assert on exact pixel values.
//!
//! # Reference
//!
//! MapLibre GL JS (BSD-3) decodes a raster tile through the browser's own
//! image pipeline (`src/util/image_request.ts`, reached from
//! `raster_tile_source.ts`'s `loadTile`), so there is no algorithm to port
//! here -- only the two behaviours that pipeline implies and that this
//! module reproduces deliberately:
//!
//! - **Format is sniffed, not declared.** A `.png` URL template that
//!   actually serves JPEG works in a browser and works here.
//! - **The result is RGBA.** A three-channel JPEG gains an opaque alpha
//!   channel rather than needing a second sampling path in the shader.

/// The pixel size of one raster tile edge, as this engine assumes it.
///
/// **This is an assumption the style spec lets a source contradict**, and
/// the mismatch is the single most likely way a raster basemap comes out
/// subtly wrong rather than obviously broken. `raster_tile_source.ts`
/// defaults `tileSize` to 512 and its own doc comment warns that a
/// provider serving 256 without being told so renders blurry, because the
/// smaller image is stretched over the larger footprint.
///
/// This engine has the same 512 baked in for vector tiles
/// (`geo.rs`'s `TILE_SIZE`), and reading a source's declared `tileSize`
/// is not implemented anywhere (see the plan document's Task 2). So
/// rather than silently stretch, [`decode_tile`] takes the size it expects
/// as an argument and refuses anything else -- a loud failure a caller can
/// act on beats a blurry map nobody can diagnose.
pub const DEFAULT_TILE_SIZE: u32 = 512;

/// Decodes one raster tile's bytes into RGBA8 pixels.
///
/// The format is sniffed from the bytes themselves
/// (`image::load_from_memory`), not taken from the URL's extension: a
/// server is entitled to answer a `.png` template with JPEG and browsers
/// accept it, so a decoder keyed on the template would reject tiles that
/// work everywhere else. PNG and JPEG are both already in the dependency
/// graph -- `image` is a direct dependency with default features, so
/// `png` and `zune-jpeg` come with it and nothing new is linked for this.
///
/// The result is always four channels. A baseline JPEG decodes to three
/// and gains an opaque alpha here, so the sampling path downstream has one
/// shape rather than two.
///
/// # Errors
///
/// When the bytes are not an image this build can decode. The error
/// deliberately does NOT include the bytes or any URL: a tile URL can
/// carry an access token, and this error is logged.
pub fn decode(bytes: &[u8]) -> anyhow::Result<image::RgbaImage> {
    // `load_from_memory` rather than `load_from_memory_with_format`: see
    // the sniffing note above. `brand_mark::decode` uses the `_with_format`
    // form because it decodes ONE compiled-in asset whose format is known
    // at build time; a tile's is not.
    let decoded = image::load_from_memory(bytes)
        .map_err(|e| anyhow::anyhow!("raster tile is not a decodable image: {e}"))?;
    Ok(decoded.to_rgba8())
}

/// [`decode`], plus the size check that keeps a 256px tile from being
/// silently stretched over a 512px footprint.
///
/// # Why this is a hard error and not a resample
///
/// Because the wrong size is not a pixel problem, it is a REGISTRATION
/// problem, and resampling would hide it. A 256px tile stretched to 512
/// still lands in the right place on the map -- it is simply half the
/// resolution, which reads as "this imagery is a bit soft", not as "this
/// map is misconfigured". The operator then goes looking for a sharper
/// source instead of adding `"tileSize": 256`. Refusing makes the
/// misconfiguration say its own name.
///
/// The `expected` argument is threaded from the caller rather than read
/// from a source's declared `tileSize` because nothing in this crate reads
/// a style's `sources` block yet. When that lands, this signature does not
/// change -- only where its argument comes from.
///
/// # Errors
///
/// When [`decode`] fails, or when the image is not `expected` x `expected`.
pub fn decode_tile(bytes: &[u8], expected: u32) -> anyhow::Result<image::RgbaImage> {
    let image = decode(bytes)?;
    let (width, height) = image.dimensions();
    anyhow::ensure!(
        width == expected && height == expected,
        "raster tile is {width}x{height}, expected {expected}x{expected} -- \
         the source's tile size does not match what this map was configured \
         for, and drawing it anyway would stretch the imagery over the wrong \
         footprint"
    );
    Ok(image)
}

/// [`decode_tile`] for a known tile, with the one repair a real tileset
/// needs: [`repair_antimeridian_column`].
///
/// # Errors
///
/// As [`decode_tile`].
pub fn decode_tile_at(bytes: &[u8], expected: u32, tile: crate::geo::TileId) -> anyhow::Result<image::RgbaImage> {
    let mut image = decode_tile(bytes, expected)?;
    repair_antimeridian_column(tile, &mut image);
    Ok(image)
}

/// **Fills a fully transparent last pixel column on the antimeridian.**
///
/// OpenFreeMap's Natural Earth tileset (`natural_earth/ne2sr`) ships its
/// easternmost tiles (x = 2^z - 1) with the LAST pixel column at alpha 0
/// -- measured on z0, z1, z3 and z5, while z2, z4 and z6 are opaque. Drawn
/// on the globe that column is lon 180, and the transparent strip shows
/// whatever is under the imagery: over the satellites demo's "Real" style
/// (translucent water over a near-black background) a 1 px dark meridian.
/// It is the tileset's data, not a crack in the mesh -- both tile edges
/// project to the same pixel.
///
/// Only an easternmost tile is touched, and only when its whole last column
/// is transparent while the column before it is not: then the column
/// before is copied across. Any other tile, and any tile whose transparency
/// is real content, is left exactly as decoded. Returns whether it changed
/// the image.
pub fn repair_antimeridian_column(tile: crate::geo::TileId, image: &mut image::RgbaImage) -> bool {
    let (w, h) = image.dimensions();
    let last_x = (1u64 << tile.z.min(62)) - 1;
    if w < 2 || u64::from(tile.x) != last_x {
        return false;
    }
    let column_clear = |img: &image::RgbaImage, x: u32| (0..h).all(|y| img.get_pixel(x, y)[3] == 0);
    if !column_clear(image, w - 1) || column_clear(image, w - 2) {
        return false;
    }
    for y in 0..h {
        let p = *image.get_pixel(w - 2, y);
        image.put_pixel(w - 1, y, p);
    }
    true
}

#[cfg(test)]
mod antimeridian_tests {
    use super::*;
    use crate::geo::TileId;

    fn tile_with_clear_last_column() -> image::RgbaImage {
        let mut img = image::RgbaImage::from_pixel(4, 4, image::Rgba([10, 20, 30, 255]));
        for y in 0..4 {
            img.put_pixel(3, y, image::Rgba([2, 0, 2, 0]));
        }
        img
    }

    #[test]
    fn the_easternmost_tiles_clear_column_is_filled_from_its_neighbour() {
        let mut img = tile_with_clear_last_column();
        assert!(repair_antimeridian_column(TileId { z: 3, x: 7, y: 3 }, &mut img));
        assert_eq!(img.get_pixel(3, 2).0, [10, 20, 30, 255]);
        let mut z0 = tile_with_clear_last_column();
        assert!(repair_antimeridian_column(TileId { z: 0, x: 0, y: 0 }, &mut z0));
    }

    #[test]
    fn any_other_tile_or_real_transparency_is_left_alone() {
        let mut img = tile_with_clear_last_column();
        assert!(!repair_antimeridian_column(TileId { z: 3, x: 6, y: 3 }, &mut img), "not on the antimeridian");
        assert_eq!(img, tile_with_clear_last_column());
        let mut empty = image::RgbaImage::new(4, 4);
        assert!(!repair_antimeridian_column(TileId { z: 3, x: 7, y: 3 }, &mut empty), "a wholly clear tile is content");
        let mut partial = tile_with_clear_last_column();
        partial.put_pixel(3, 1, image::Rgba([1, 1, 1, 255]));
        assert!(!repair_antimeridian_column(TileId { z: 3, x: 7, y: 3 }, &mut partial), "a partly opaque column is content");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Four solid quadrants in four primaries, at a size that is a
    /// multiple of 8 so JPEG's own 8x8 blocks line up with the quadrant
    /// boundaries. That alignment is what makes the lossy round-trip below
    /// assertable at all: a block straddling a colour boundary rings, and
    /// the corner pixels this test reads would carry that ringing.
    fn four_quadrants(size: u32) -> image::RgbaImage {
        let mut img = image::RgbaImage::new(size, size);
        let half = size / 2;
        for y in 0..size {
            for x in 0..size {
                let color = match (x < half, y < half) {
                    (true, true) => [255, 0, 0, 255],
                    (false, true) => [0, 255, 0, 255],
                    (true, false) => [0, 0, 255, 255],
                    (false, false) => [255, 255, 255, 255],
                };
                img.put_pixel(x, y, image::Rgba(color));
            }
        }
        img
    }

    fn encode(img: &image::RgbaImage, format: image::ImageFormat) -> Vec<u8> {
        let mut bytes = Vec::new();
        // JPEG has no alpha channel, so the RGBA buffer has to lose one
        // before it will encode at all.
        match format {
            image::ImageFormat::Jpeg => {
                let rgb = image::DynamicImage::ImageRgba8(img.clone()).to_rgb8();
                rgb.write_to(&mut std::io::Cursor::new(&mut bytes), format)
                    .expect("encoding a synthesised image");
            }
            _ => {
                img.write_to(&mut std::io::Cursor::new(&mut bytes), format)
                    .expect("encoding a synthesised image");
            }
        }
        bytes
    }

    /// PNG is lossless, so this one asserts at tolerance zero. Anything
    /// less than exact here would mean the decode is doing something to
    /// the pixels, which it must not.
    #[test]
    fn a_png_tile_round_trips_exactly() {
        let original = four_quadrants(16);
        let decoded = decode(&encode(&original, image::ImageFormat::Png)).expect("a PNG decodes");

        assert_eq!(decoded.dimensions(), (16, 16));
        assert_eq!(*decoded.get_pixel(0, 0), image::Rgba([255, 0, 0, 255]));
        assert_eq!(*decoded.get_pixel(15, 0), image::Rgba([0, 255, 0, 255]));
        assert_eq!(*decoded.get_pixel(0, 15), image::Rgba([0, 0, 255, 255]));
        assert_eq!(*decoded.get_pixel(15, 15), image::Rgba([255, 255, 255, 255]));
    }

    /// **JPEG is lossy and this test says so in its tolerance rather than
    /// in a comment.** Asserting exact bytes here would be a flake waiting
    /// for a decoder version bump. What is being tested is not fidelity --
    /// it is that the format is SNIFFED (the caller never says "jpeg") and
    /// that three channels come back as four.
    #[test]
    fn a_jpeg_tile_is_sniffed_and_gains_an_alpha_channel() {
        let original = four_quadrants(16);
        let decoded = decode(&encode(&original, image::ImageFormat::Jpeg)).expect("a JPEG decodes");

        assert_eq!(decoded.dimensions(), (16, 16));
        for (x, y, expected) in [
            (0, 0, [255u8, 0, 0]),
            (15, 0, [0, 255, 0]),
            (0, 15, [0, 0, 255]),
            (15, 15, [255, 255, 255]),
        ] {
            let got = *decoded.get_pixel(x, y);
            for channel in 0..3 {
                let delta = got[channel].abs_diff(expected[channel]);
                assert!(
                    delta <= 24,
                    "({x},{y}) channel {channel}: {} is {delta} away from {}, which is more \
                     than JPEG's quantisation explains",
                    got[channel],
                    expected[channel]
                );
            }
            assert_eq!(
                got[3], 255,
                "({x},{y}) must come back opaque: a three-channel JPEG has to gain an \
                 alpha channel here, or the shader needs a second sampling path"
            );
        }
    }

    /// The whole point of `decode_tile` over `decode`.
    #[test]
    fn a_tile_of_the_wrong_size_is_refused_rather_than_stretched() {
        let bytes = encode(&four_quadrants(256), image::ImageFormat::Png);

        let right = decode_tile(&bytes, 256).expect("256 is what this one is");
        assert_eq!(right.dimensions(), (256, 256));

        let wrong = decode_tile(&bytes, DEFAULT_TILE_SIZE);
        let message = wrong.expect_err("a 256px tile must not pass as a 512px one").to_string();
        assert!(
            message.contains("256x256") && message.contains("512x512"),
            "the error has to name both sizes or it cannot be acted on: {message}"
        );
    }

    /// Bytes that are not an image at all -- the shape a tile server's
    /// HTML error page arrives in when it answers 200.
    #[test]
    fn bytes_that_are_not_an_image_fail_without_quoting_themselves() {
        let error = decode(b"<html><body>404 Not Found</body></html>")
            .expect_err("HTML is not an image");
        let message = error.to_string();
        assert!(message.contains("not a decodable image"), "got {message}");
        // A tile URL can carry an access token and this error is logged.
        // The bytes must not come along for the ride.
        assert!(
            !message.contains("html"),
            "the error must not quote the bytes it failed on: {message}"
        );
    }
}

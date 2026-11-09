//! The `color-theme` colour lookup table -- Mapbox's whole-map colour grade.
//!
//! A Standard style document can carry a top-level `color-theme`, alongside
//! `lights` and `schema`:
//!
//! ```text
//! "color-theme": { "data": ["match", ["config", "theme"], "faded", "iVBORw0KGgo...", ...] }
//! ```
//!
//! `data` is an ordinary style expression that folds (in the declaring
//! document's config scope) to one base64 PNG, and that PNG is a 3D colour
//! cube: every colour the map draws is looked up through it on the way to
//! the screen. `theme: monochrome` is the case this exists for -- it is what
//! turns Standard's genuinely pink `colorAdminBoundaries`
//! (`hsl(345, 100%, 70%)`) into the grey Mapbox actually renders. Without
//! it the boundaries come out vivid pink, and every other colour is off by a
//! smaller amount in the same way.
//!
//! # The image
//!
//! `N = image.height`, and the image is `N * N` wide -- an `N x N x N` cube
//! unrolled into one strip, `N` sample points per channel. Standard's
//! `monochrome` LUT is 32 tall and 1024 wide, so 32768 entries.
//!
//! # The arithmetic
//!
//! Transcribed from `RenderColor`'s constructor in
//! `src/style-spec/util/color.ts` (Mapbox GL JS 3.15.0). The reference has
//! two branches, one for a premultiplied colour and one for a straight one;
//! they sample the SAME cube location (the premultiplied branch divides the
//! channel back out by alpha first, which undoes exactly the multiply that
//! put it there) and differ only in re-premultiplying the result. This
//! crate's `Rgba` is straight, never premultiplied -- `parse_css_color`
//! writes `r`/`g`/`b` independently of `a` -- so this is the straight
//! branch, and the alpha is not looked up at all.
//!
//! The index formula is NOT symmetric in r/g/b: green strides by `N * N`
//! (it picks the image row), blue by `N`, red by 1. It is copied literally
//! from the reference rather than re-derived, because a plausible-looking
//! re-derivation that swaps two channels still returns colours -- just the
//! wrong ones, and only for colours off the grey diagonal.
//!
//! # Where it is applied
//!
//! On the CPU, when a paint colour is read, and BEFORE the lighting
//! multiply -- `draw_background.ts` does
//! `color.toPremultipliedRenderColor(lut)` and the shader's
//! `apply_lighting_with_emission_ground` then runs on that already-graded
//! colour. This compiler folds the lighting at compile time
//! (`style_spec::light_ground_color`), so the LUT has to be applied to the
//! colour before that fold. See `style_spec::themed_and_lit_color`, which is
//! the single place that composes the two so the order cannot be got wrong
//! at a call site, and
//! `the_lut_is_applied_before_the_lighting_multiply_not_after`, which pins
//! it. Getting it backwards changes every colour subtly, and no colour
//! assertion written against a single style would notice.

use anyhow::{bail, Context, Result};
use base64::Engine as _;

use crate::style::Rgba;

/// The prefix `Style._loadColorTheme` prepends when the payload does not
/// already carry one, which is the same thing as accepting both forms.
const DATA_URL_MARKER: &str = ";base64,";

/// The reference refuses a taller image outright ("The height of the image
/// must be less than or equal to 32 pixels", `style.ts`), so a document
/// asking for more is malformed rather than merely expensive.
const MAX_SIZE: usize = 32;

/// A decoded `color-theme` cube: `size` samples per channel, RGBA8, one
/// entry per `(r, g, b)` grid point.
///
/// Decoded ONCE per document that declares a `color-theme`, and then
/// borrowed by every layer's `Scope` -- see `Scope::with_lut`. A cube is
/// 128 KiB at `size = 32`; rebuilding it per layer would decode a PNG 150
/// times for one style.
#[derive(Clone, PartialEq, Eq)]
pub struct ColorLut {
    /// `N` -- the cube's edge in samples, `image.height`.
    size: usize,
    /// RGBA8, `size.pow(3)` pixels, in the image's own row-major order.
    /// Indexed exactly as the reference indexes it; see `sample`.
    data: Vec<u8>,
}

/// Deliberately not derived: `data` is 128 KiB of bytes at the real cube
/// size, and `Scope` is `Debug` and gets printed in compile-error context.
impl std::fmt::Debug for ColorLut {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("ColorLut").field("size", &self.size).field("bytes", &self.data.len()).finish()
    }
}

impl ColorLut {
    /// Decodes a `color-theme.data` payload: a bare base64 PNG, or the
    /// same wrapped in a `data:image/png;base64,` URI. The reference
    /// accepts both by prepending the prefix when it is absent
    /// (`Style._loadColorTheme`), so both are the same input.
    pub fn decode(payload: &str) -> Result<Self> {
        let payload = payload.trim();
        let encoded = match payload.find(DATA_URL_MARKER) {
            Some(at) => &payload[at + DATA_URL_MARKER.len()..],
            None => payload,
        };
        // The style JSON can wrap a long payload; the reference's `Image`
        // decoder tolerates whitespace in a data URI, and `base64` does
        // not, so it is stripped rather than rejected.
        let encoded: String = encoded.chars().filter(|c| !c.is_ascii_whitespace()).collect();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&encoded)
            .context("\"color-theme\" data is not valid base64")?;
        let image = image::load_from_memory(&bytes)
            .context("\"color-theme\" data is not a decodable image")?
            .to_rgba8();
        let (width, height) = image.dimensions();
        Self::from_rgba(width as usize, height as usize, image.into_raw())
    }

    /// The decoded cube, checked. `width` must be `height * height` and
    /// `height` must be a usable edge -- the two shape rules
    /// `Style._loadColorTheme` enforces, restated here because everything
    /// downstream indexes on them holding.
    pub fn from_rgba(width: usize, height: usize, data: Vec<u8>) -> Result<Self> {
        let size = height;
        if size < 2 {
            bail!("a \"color-theme\" cube needs at least 2 samples per channel, got a {width}x{height} image");
        }
        if size > MAX_SIZE {
            bail!("a \"color-theme\" image must be at most {MAX_SIZE} pixels tall, got {height}");
        }
        if width != size * size {
            bail!("a \"color-theme\" image must be its height squared wide ({} for a height of {height}), got {width}", size * size);
        }
        let expected = size * size * size * 4;
        if data.len() != expected {
            bail!("a {width}x{height} RGBA image is {expected} bytes, got {}", data.len());
        }
        Ok(Self { size, data })
    }

    /// `N`, the cube's edge in samples.
    pub const fn size(&self) -> usize {
        self.size
    }

    /// The graded colour: `color` looked up through the cube, trilinearly
    /// interpolated between the eight surrounding grid points. Alpha is
    /// carried through untouched -- the reference never looks it up.
    ///
    /// Channels are clamped into the cube. Every colour that reaches here
    /// comes from `parse_css_color` or an interpolation between two such,
    /// so it is already in `[0, 1]`; the reference throws "out of range"
    /// instead, which would turn one out-of-gamut literal into a whole
    /// layer degrading to black.
    pub fn apply(&self, color: Rgba) -> Rgba {
        let size = self.size;
        let squared = size * size;
        let last = (size - 1) as f32;

        // Normalize to cube dimensions. Straight (non-premultiplied)
        // branch: `r * (N - 1)`, no divide by alpha.
        let scale = |channel: f32| (channel * last).clamp(0.0, last);
        let (red, green, blue) = (scale(color.r), scale(color.g), scale(color.b));

        // The cube corners the colour sits between, and the weights within.
        let (r0, r1, rw) = (red.floor() as usize, red.ceil() as usize, red - red.floor());
        let (g0, g1, gw) = (green.floor() as usize, green.ceil() as usize, green - green.floor());
        let (b0, b1, bw) = (blue.floor() as usize, blue.ceil() as usize, blue - blue.floor());

        // `(r + g * N * N + b * N) * 4`, verbatim from the reference. Green
        // strides a whole image row, blue a cube slice within the row, red a
        // single pixel.
        let corner = |r: usize, g: usize, b: usize, channel: usize| -> f32 {
            f32::from(self.data[(r + g * squared + b * size) * 4 + channel])
        };
        let lerp = |a: f32, b: f32, t: f32| a * (1.0 - t) + b * t;
        let interpolated = |channel: usize| -> f32 {
            let along_blue = |r: usize, g: usize| lerp(corner(r, g, b0, channel), corner(r, g, b1, channel), bw);
            let along_green = |r: usize| lerp(along_blue(r, g0), along_blue(r, g1), gw);
            lerp(along_green(r0), along_green(r1), rw) / 255.0
        };

        Rgba { r: interpolated(0), g: interpolated(1), b: interpolated(2), a: color.a }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a cube by evaluating `grade` at every grid point, in the
    /// image's own layout -- the inverse of `apply`'s index formula, written
    /// out rather than reusing it, so a test cube and the sampler agree only
    /// if both are right.
    fn cube(size: usize, grade: impl Fn(f32, f32, f32) -> (f32, f32, f32)) -> ColorLut {
        let last = (size - 1) as f32;
        let mut data = vec![0u8; size * size * size * 4];
        for b in 0..size {
            for g in 0..size {
                for r in 0..size {
                    let (out_r, out_g, out_b) = grade(r as f32 / last, g as f32 / last, b as f32 / last);
                    let at = (r + g * size * size + b * size) * 4;
                    data[at] = (out_r * 255.0).round() as u8;
                    data[at + 1] = (out_g * 255.0).round() as u8;
                    data[at + 2] = (out_b * 255.0).round() as u8;
                    data[at + 3] = 255;
                }
            }
        }
        ColorLut::from_rgba(size * size, size, data).expect("a well-shaped cube")
    }

    fn identity(size: usize) -> ColorLut {
        cube(size, |r, g, b| (r, g, b))
    }

    fn rgba(r: f32, g: f32, b: f32, a: f32) -> Rgba {
        Rgba { r, g, b, a }
    }

    fn assert_close(got: Rgba, want: Rgba, tolerance: f32) {
        assert!(
            (got.r - want.r).abs() < tolerance
                && (got.g - want.g).abs() < tolerance
                && (got.b - want.b).abs() < tolerance
                && (got.a - want.a).abs() < 1e-6,
            "got {got:?}, want {want:?} (tolerance {tolerance})"
        );
    }

    /// The first acceptance test, and the one that catches a swapped index
    /// stride: an identity cube samples back to exactly what went in, for
    /// every channel independently. A cube built with r and g transposed
    /// would still pass on greys, so these are deliberately off-diagonal.
    #[test]
    fn an_identity_cube_returns_its_input_unchanged() {
        let lut = identity(9);
        for color in [
            rgba(0.0, 0.0, 0.0, 1.0),
            rgba(1.0, 1.0, 1.0, 1.0),
            rgba(1.0, 0.0, 0.0, 1.0),
            rgba(0.0, 1.0, 0.0, 1.0),
            rgba(0.0, 0.0, 1.0, 1.0),
            rgba(0.25, 0.5, 0.75, 0.5),
            rgba(0.125, 0.875, 0.375, 1.0),
        ] {
            assert_close(lut.apply(color), color, 1.0 / 255.0);
        }
    }

    /// An inverting cube inverts, and does so per channel -- so a sampler
    /// that read the red byte for all three would fail here.
    #[test]
    fn an_inverting_cube_inverts_each_channel() {
        let lut = cube(9, |r, g, b| (1.0 - r, 1.0 - g, 1.0 - b));
        assert_close(lut.apply(rgba(1.0, 0.0, 0.0, 1.0)), rgba(0.0, 1.0, 1.0, 1.0), 1.0 / 255.0);
        assert_close(lut.apply(rgba(0.25, 0.5, 0.75, 0.25)), rgba(0.75, 0.5, 0.25, 0.25), 1.0 / 255.0);
    }

    /// A cube that writes each output channel from a DIFFERENT input
    /// channel is the direct test that `(r + g * N * N + b * N)` is the
    /// formula in use: with any two strides swapped, `(1, 0, 0)` and
    /// `(0, 1, 0)` land on each other's grid points and the rotation comes
    /// back wrong.
    #[test]
    fn the_index_formula_keeps_the_three_channels_apart() {
        let lut = cube(5, |r, g, b| (g, b, r));
        assert_close(lut.apply(rgba(1.0, 0.0, 0.0, 1.0)), rgba(0.0, 0.0, 1.0, 1.0), 1.0 / 255.0);
        assert_close(lut.apply(rgba(0.0, 1.0, 0.0, 1.0)), rgba(1.0, 0.0, 0.0, 1.0), 1.0 / 255.0);
        assert_close(lut.apply(rgba(0.0, 0.0, 1.0, 1.0)), rgba(0.0, 1.0, 0.0, 1.0), 1.0 / 255.0);
    }

    /// Hand-computed trilinear sample. A 2-sample cube has exactly the
    /// eight corners and nothing between them, so every interior colour is
    /// a pure weighted sum this test can write out by hand:
    ///
    /// with `red = 1` at every corner where `r` is high and `0` where it is
    /// low, the red output at `r = 0.25` is `0.75 * 0 + 0.25 * 1 = 0.25`,
    /// independent of g and b. Making green's output depend on `b` and
    /// blue's on `g` pins the two inner lerps as well: at `(g, b) = (0.5,
    /// 0.2)` the green output is `0.2` and the blue output `0.5`.
    #[test]
    fn a_two_sample_cube_interpolates_the_eight_corners_by_hand() {
        let lut = cube(2, |r, g, b| (r, b, g));
        let got = lut.apply(rgba(0.25, 0.5, 0.2, 0.9));
        assert_close(got, rgba(0.25, 0.2, 0.5, 0.9), 2.0 / 255.0);
    }

    /// Alpha is not looked up: a cube that would drive every channel to
    /// zero leaves alpha exactly where it was.
    #[test]
    fn alpha_is_carried_through_untouched() {
        let lut = cube(4, |_, _, _| (0.0, 0.0, 0.0));
        assert!((lut.apply(rgba(0.5, 0.5, 0.5, 0.37)).a - 0.37).abs() < 1e-6);
    }

    /// The payload may arrive bare or as a `data:` URI; the reference
    /// normalises the two by prepending the prefix when it is missing.
    #[test]
    fn decodes_a_bare_base64_png_and_the_same_one_as_a_data_uri() {
        let source = identity(4);
        let image = image::RgbaImage::from_raw(16, 4, source.data.clone()).expect("a 16x4 RGBA image");
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .expect("encodes");
        let bare = base64::engine::general_purpose::STANDARD.encode(&png);

        let from_bare = ColorLut::decode(&bare).expect("decodes a bare payload");
        let from_uri = ColorLut::decode(&format!("data:image/png;base64,{bare}")).expect("decodes a data URI");
        assert_eq!(from_bare, source);
        assert_eq!(from_uri, source);
        assert_eq!(from_bare.size(), 4);
    }

    #[test]
    fn refuses_an_image_that_is_not_its_height_squared_wide() {
        let error = ColorLut::from_rgba(8, 4, vec![0; 8 * 4 * 4]).expect_err("8 != 4 * 4");
        assert!(format!("{error:#}").contains("height squared"), "{error:#}");
    }

    #[test]
    fn refuses_an_image_taller_than_the_reference_allows() {
        let error = ColorLut::from_rgba(64 * 64, 64, vec![0; 64 * 64 * 64 * 4]).expect_err("64 > 32");
        assert!(format!("{error:#}").contains("at most 32"), "{error:#}");
    }

    #[test]
    fn refuses_a_payload_that_is_not_base64() {
        assert!(ColorLut::decode("this is not base64!!").is_err());
    }

    /// The real thing, straight out of the vendored fixture: Standard's
    /// `monochrome` cube is 32 samples per channel, and it takes the pink
    /// `colorAdminBoundaries` a long way towards grey. Saturation here is
    /// the HSV one, `(max - min) / max`.
    #[test]
    fn the_vendored_monochrome_cube_decodes_at_32_samples_and_desaturates_pink() {
        let style: serde_json::Value =
            serde_json::from_slice(&mapbox_fixture_or_skip!("mapbox-standard-dark-style.json"))
                .expect("valid JSON");
        let data = style["imports"][0]["data"]["color-theme"]["data"].as_array().expect("a match expression");
        // `["match", ["config", "theme"], "faded", <png>, "monochrome", <png>, ...]`
        let monochrome = data
            .windows(2)
            .find(|pair| pair[0] == serde_json::json!("monochrome"))
            .and_then(|pair| pair[1].as_str())
            .expect("the fixture has a monochrome branch");

        let lut = ColorLut::decode(monochrome).expect("the vendored payload decodes");
        assert_eq!(lut.size(), 32);

        let pink = crate::style_spec::parse_css_color("hsl(345, 100%, 70%)").expect("a colour");
        let saturation = |c: Rgba| {
            let max = c.r.max(c.g).max(c.b);
            let min = c.r.min(c.g).min(c.b);
            if max <= 0.0 {
                0.0
            } else {
                (max - min) / max
            }
        };
        let before = saturation(pink);
        let after = saturation(lut.apply(pink));
        assert!(before > 0.55, "hsl(345, 100%, 70%) is vivid, got saturation {before}");
        assert!(after < 0.1, "monochrome must nearly desaturate it, got saturation {after} (from {before})");
    }
}

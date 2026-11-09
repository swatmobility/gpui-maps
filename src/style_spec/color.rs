use anyhow::Result;

/// Parse a CSS color string to an RGBA color.
/// Supports:
/// - hex: #d8e8c8, #d8e8c880 (alpha), #abc, #abc8
/// - RGB: rgb(158,189,255) or rgb(158, 189, 255)
/// - RGBA: rgba(95, 208, 100, 1)
/// - HSL: hsl(35,8%,85%)
/// - HSLA: hsla(0,3%,85%,0.84)
pub fn parse_css_color(s: &str) -> Result<crate::style::Rgba> {
    let s = s.trim();

    if s.starts_with('#') {
        parse_hex_color(s)
    } else if s.starts_with("rgba(") {
        parse_rgba(s)
    } else if s.starts_with("rgb(") {
        parse_rgb(s)
    } else if s.starts_with("hsla(") {
        parse_hsla(s)
    } else if s.starts_with("hsl(") {
        parse_hsl(s)
    } else {
        anyhow::bail!("Unsupported color format: {}", s)
    }
}

/// The CSS 2.1 named colours (the 16 HTML 4 names, `grey`, `orange`) and
/// `transparent`, as RGBA bytes: the ONE table every hand-written colour
/// in the crate is read against ([`css_named_color`]), and the list an
/// error hint names.
pub const CSS_NAMED_COLORS: &[(&str, [u8; 4])] = &[
    ("black", [0, 0, 0, 255]),
    ("silver", [192, 192, 192, 255]),
    ("gray", [128, 128, 128, 255]),
    ("grey", [128, 128, 128, 255]),
    ("white", [255, 255, 255, 255]),
    ("maroon", [128, 0, 0, 255]),
    ("red", [255, 0, 0, 255]),
    ("purple", [128, 0, 128, 255]),
    ("fuchsia", [255, 0, 255, 255]),
    ("green", [0, 128, 0, 255]),
    ("lime", [0, 255, 0, 255]),
    ("olive", [128, 128, 0, 255]),
    ("yellow", [255, 255, 0, 255]),
    ("navy", [0, 0, 128, 255]),
    ("blue", [0, 0, 255, 255]),
    ("teal", [0, 128, 128, 255]),
    ("aqua", [0, 255, 255, 255]),
    ("orange", [255, 165, 0, 255]),
    ("transparent", [0, 0, 0, 0]),
];

/// The names in [`CSS_NAMED_COLORS`], in table order (for error hints).
pub fn css_color_names() -> impl Iterator<Item = &'static str> {
    CSS_NAMED_COLORS.iter().map(|(n, _)| *n)
}

/// A [`CSS_NAMED_COLORS`] name, case-insensitive and trimmed.
///
/// DELIBERATELY not wired into [`parse_css_color`]: the basemap styles
/// parse through that, and a name that errors there today must keep
/// erroring (goldens). Callers that accept hand-written colours opt in:
/// `overlay::source_paint` (every colour key of a story overlay layer's
/// paint, and `line-gradient`) and `story::timeline` (captions, paint
/// tweens, effect colours).
pub fn css_named_color(name: &str) -> Option<crate::style::Rgba> {
    let name = name.trim().to_ascii_lowercase();
    CSS_NAMED_COLORS.iter().find(|(n, _)| *n == name).map(|(_, [r, g, b, a])| crate::style::Rgba {
        r: f32::from(*r) / 255.0,
        g: f32::from(*g) / 255.0,
        b: f32::from(*b) / 255.0,
        a: f32::from(*a) / 255.0,
    })
}

fn parse_hex_color(s: &str) -> Result<crate::style::Rgba> {
    if !s.starts_with('#') {
        anyhow::bail!("Invalid hex color format: {}", s);
    }

    let hex_str = &s[1..];

    // #rgb, #rgba, #rrggbb and #rrggbbaa (CSS Color 4). The short forms
    // double each digit: #abc -> #aabbcc. The alpha forms reach here from
    // a host's data-driven `["get", "color"]` values; no vendored style
    // uses them, so no basemap golden moves.
    let expanded_hex = match hex_str.len() {
        3 | 4 => hex_str.chars().flat_map(|c| std::iter::repeat_n(c, 2)).collect::<String>(),
        6 | 8 => hex_str.to_string(),
        _ => anyhow::bail!("Invalid hex color format: {}. Expected 3, 4, 6 or 8 hex digits.", s),
    };
    if !expanded_hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        anyhow::bail!("Invalid hex color format: {}", s);
    }
    let byte = |i: usize| u8::from_str_radix(&expanded_hex[i..i + 2], 16).map(|v| f32::from(v) / 255.0);

    Ok(crate::style::Rgba {
        r: byte(0)?,
        g: byte(2)?,
        b: byte(4)?,
        a: if expanded_hex.len() == 8 { byte(6)? } else { 1.0 },
    })
}

fn parse_rgb(s: &str) -> Result<crate::style::Rgba> {
    if !s.starts_with("rgb(") || !s.ends_with(')') {
        anyhow::bail!("Invalid rgb format: {}", s);
    }

    let inner = &s[4..s.len() - 1];
    let parts: Vec<&str> = inner.split(',').map(|p| p.trim()).collect();

    if parts.len() != 3 {
        anyhow::bail!("Expected 3 components in rgb(), got {}", parts.len());
    }

    let r = parts[0].parse::<f32>()? / 255.0;
    let g = parts[1].parse::<f32>()? / 255.0;
    let b = parts[2].parse::<f32>()? / 255.0;

    Ok(crate::style::Rgba {
        r,
        g,
        b,
        a: 1.0,
    })
}

fn parse_rgba(s: &str) -> Result<crate::style::Rgba> {
    if !s.starts_with("rgba(") || !s.ends_with(')') {
        anyhow::bail!("Invalid rgba format: {}", s);
    }

    let inner = &s[5..s.len() - 1];
    let parts: Vec<&str> = inner.split(',').map(|p| p.trim()).collect();

    if parts.len() != 4 {
        anyhow::bail!("Expected 4 components in rgba(), got {}", parts.len());
    }

    let r = parts[0].parse::<f32>()? / 255.0;
    let g = parts[1].parse::<f32>()? / 255.0;
    let b = parts[2].parse::<f32>()? / 255.0;
    let a = parts[3].parse::<f32>()?;

    Ok(crate::style::Rgba {
        r,
        g,
        b,
        a,
    })
}

fn parse_hsl(s: &str) -> Result<crate::style::Rgba> {
    if !s.starts_with("hsl(") || !s.ends_with(')') {
        anyhow::bail!("Invalid hsl format: {}", s);
    }

    let inner = &s[4..s.len() - 1];
    let parts: Vec<&str> = inner.split(',').map(|p| p.trim()).collect();

    if parts.len() != 3 {
        anyhow::bail!("Expected 3 components in hsl(), got {}", parts.len());
    }

    let h = parts[0].parse::<f32>()?;
    let s_str = parts[1].trim_end_matches('%');
    let l_str = parts[2].trim_end_matches('%');
    let s = s_str.parse::<f32>()? / 100.0;
    let l = l_str.parse::<f32>()? / 100.0;

    let (r, g, b) = hsl_to_rgb(h, s, l);

    Ok(crate::style::Rgba { r, g, b, a: 1.0 })
}

fn parse_hsla(s: &str) -> Result<crate::style::Rgba> {
    if !s.starts_with("hsla(") || !s.ends_with(')') {
        anyhow::bail!("Invalid hsla format: {}", s);
    }

    let inner = &s[5..s.len() - 1];
    let parts: Vec<&str> = inner.split(',').map(|p| p.trim()).collect();

    if parts.len() != 4 {
        anyhow::bail!("Expected 4 components in hsla(), got {}", parts.len());
    }

    let h = parts[0].parse::<f32>()?;
    let s_str = parts[1].trim_end_matches('%');
    let l_str = parts[2].trim_end_matches('%');
    let s = s_str.parse::<f32>()? / 100.0;
    let l = l_str.parse::<f32>()? / 100.0;
    let a = parts[3].parse::<f32>()?;

    let (r, g, b) = hsl_to_rgb(h, s, l);

    Ok(crate::style::Rgba { r, g, b, a })
}

/// Convert HSL to RGB using the standard CSS formula.
/// h: hue (0-360)
/// s: saturation (0-1)
/// l: lightness (0-1)
pub(crate) fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let h_prime = (h % 360.0) / 60.0;
    let x = c * (1.0 - (h_prime % 2.0 - 1.0).abs());
    let m = l - c / 2.0;

    let (r_prime, g_prime, b_prime) = if h_prime < 1.0 {
        (c, x, 0.0)
    } else if h_prime < 2.0 {
        (x, c, 0.0)
    } else if h_prime < 3.0 {
        (0.0, c, x)
    } else if h_prime < 4.0 {
        (0.0, x, c)
    } else if h_prime < 5.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };

    (r_prime + m, g_prime + m, b_prime + m)
}

/// The exact inverse of `hsl_to_rgb`: the standard CSS RGB->HSL formula,
/// returning `(hue in [0, 360), saturation in [0, 1], lightness in [0, 1])`
/// -- the same units `hsl_to_rgb` takes, so the two compose both ways.
///
/// The crate needs this for the style-spec `to-hsla` operator, which every
/// Mapbox Standard fill and background colour goes through (it decomposes a
/// configured colour into `let`-bound h/s/l, adjusts one component, and
/// recomposes with `hsl`). A wrong inverse would shift every one of those
/// colours by a small, uniform amount rather than failing visibly, which is
/// why `hsl_round_trips_*` below check it against `hsl_to_rgb` across a
/// swept grid rather than at one sample.
///
/// Two degeneracies, handled the way CSS defines them:
/// - **Achromatic** (`r == g == b`): hue is undefined and saturation is 0.
///   Hue is reported as 0, so a grey round-trips to the same grey but not
///   necessarily to the same hue it was built from -- there is no hue to
///   recover.
/// - **Black and white** (`l == 0` or `l == 1`): the saturation denominator
///   `1 - |2l - 1|` is 0, so saturation is reported as 0 for the same
///   reason. `hsl_to_rgb` maps every saturation to the same colour there.
pub(crate) fn rgb_to_hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let chroma = max - min;
    let lightness = (max + min) / 2.0;

    if chroma == 0.0 {
        return (0.0, 0.0, lightness);
    }

    // The inverse of `hsl_to_rgb`'s `c = (1 - |2l - 1|) * s`. The
    // denominator is only zero at pure black/white, where `chroma` is
    // already 0 and we returned above.
    let saturation = chroma / (1.0 - (2.0 * lightness - 1.0).abs());

    // Which of the six 60-degree sectors `hsl_to_rgb` would have picked,
    // read back off whichever channel came out largest.
    let hue_prime = if max == r {
        ((g - b) / chroma).rem_euclid(6.0)
    } else if max == g {
        (b - r) / chroma + 2.0
    } else {
        (r - g) / chroma + 4.0
    };
    let hue = (hue_prime * 60.0).rem_euclid(360.0);

    (hue, saturation.min(1.0), lightness)
}

#[cfg(test)]
mod tests {
    #[test]
    fn hex_takes_alpha_in_the_long_and_short_forms() {
        let c = parse_css_color("#F4A26180").unwrap();
        assert!((c.r - 244.0 / 255.0).abs() < 1e-6 && (c.a - 128.0 / 255.0).abs() < 1e-6, "{c:?}");
        let short = parse_css_color("#f008").unwrap();
        assert_eq!((short.r, short.g, short.a), (1.0, 0.0, 136.0 / 255.0));
        assert_eq!(parse_css_color("#ff0000").unwrap().a, 1.0);
        assert_eq!(parse_css_color("#abc").unwrap(), parse_css_color("#aabbcc").unwrap());
        for bad in ["#ff00000", "#gg0000", "#ff00000000", "#f0"] {
            assert!(parse_css_color(bad).is_err(), "{bad}");
        }
    }

    use super::*;

    #[test]
    fn parses_six_digit_hex() {
        let c = parse_css_color("#d8e8c8").unwrap();
        assert!((c.r - 0xd8 as f32 / 255.0).abs() < 1e-4);
        assert!((c.g - 0xe8 as f32 / 255.0).abs() < 1e-4);
        assert!((c.b - 0xc8 as f32 / 255.0).abs() < 1e-4);
        assert_eq!(c.a, 1.0);
    }

    #[test]
    fn parses_six_digit_hex_uppercase() {
        let c = parse_css_color("#DEE3CD").unwrap();
        assert!((c.r - 0xde as f32 / 255.0).abs() < 1e-4);
        assert!((c.g - 0xe3 as f32 / 255.0).abs() < 1e-4);
        assert!((c.b - 0xcd as f32 / 255.0).abs() < 1e-4);
        assert_eq!(c.a, 1.0);
    }

    #[test]
    fn parses_three_digit_hex_white() {
        // #fff expands to #ffffff
        let c = parse_css_color("#fff").unwrap();
        assert_eq!(c.r, 1.0);
        assert_eq!(c.g, 1.0);
        assert_eq!(c.b, 1.0);
        assert_eq!(c.a, 1.0);
    }

    #[test]
    fn parses_three_digit_hex_bbb() {
        // #bbb expands to #bbbbbb
        let c = parse_css_color("#bbb").unwrap();
        assert!((c.r - 0xbb as f32 / 255.0).abs() < 1e-4);
        assert!((c.g - 0xbb as f32 / 255.0).abs() < 1e-4);
        assert!((c.b - 0xbb as f32 / 255.0).abs() < 1e-4);
        assert_eq!(c.a, 1.0);
    }

    #[test]
    fn parses_three_digit_hex_fc8() {
        // #fc8 expands to #ffcc88
        let c = parse_css_color("#fc8").unwrap();
        assert!((c.r - 0xff as f32 / 255.0).abs() < 1e-4);
        assert!((c.g - 0xcc as f32 / 255.0).abs() < 1e-4);
        assert!((c.b - 0x88 as f32 / 255.0).abs() < 1e-4);
        assert_eq!(c.a, 1.0);
    }

    #[test]
    fn parses_three_digit_hex_fde() {
        // #fde expands to #ffddee
        let c = parse_css_color("#fde").unwrap();
        assert!((c.r - 0xff as f32 / 255.0).abs() < 1e-4);
        assert!((c.g - 0xdd as f32 / 255.0).abs() < 1e-4);
        assert!((c.b - 0xee as f32 / 255.0).abs() < 1e-4);
        assert_eq!(c.a, 1.0);
    }

    #[test]
    fn parses_three_digit_hex_fea() {
        // #fea expands to #ffeeaa
        let c = parse_css_color("#fea").unwrap();
        assert!((c.r - 0xff as f32 / 255.0).abs() < 1e-4);
        assert!((c.g - 0xee as f32 / 255.0).abs() < 1e-4);
        assert!((c.b - 0xaa as f32 / 255.0).abs() < 1e-4);
        assert_eq!(c.a, 1.0);
    }

    #[test]
    fn parses_three_digit_hex_uppercase() {
        // #ABC expands to #AABBCC
        let c = parse_css_color("#ABC").unwrap();
        assert!((c.r - 0xaa as f32 / 255.0).abs() < 1e-4);
        assert!((c.g - 0xbb as f32 / 255.0).abs() < 1e-4);
        assert!((c.b - 0xcc as f32 / 255.0).abs() < 1e-4);
        assert_eq!(c.a, 1.0);
    }

    #[test]
    fn parses_rgba() {
        let c = parse_css_color("rgba(95, 208, 100, 1)").unwrap();
        assert!((c.r - 95.0 / 255.0).abs() < 1e-4);
        assert!((c.g - 208.0 / 255.0).abs() < 1e-4);
        assert!((c.b - 100.0 / 255.0).abs() < 1e-4);
        assert_eq!(c.a, 1.0);
    }

    #[test]
    fn parses_rgba_with_lower_alpha() {
        let c = parse_css_color("rgba(255,255,255,0.7)").unwrap();
        assert!((c.r - 1.0).abs() < 1e-4);
        assert!((c.g - 1.0).abs() < 1e-4);
        assert!((c.b - 1.0).abs() < 1e-4);
        assert!((c.a - 0.7).abs() < 1e-4);
    }

    #[test]
    fn parses_rgb_without_alpha() {
        let c = parse_css_color("rgb(158,189,255)").unwrap();
        assert!((c.r - 158.0 / 255.0).abs() < 1e-4);
        assert!((c.g - 189.0 / 255.0).abs() < 1e-4);
        assert!((c.b - 1.0).abs() < 1e-4);
        assert_eq!(c.a, 1.0);
    }

    #[test]
    fn parses_rgb_with_spaces() {
        let c = parse_css_color("rgb(158, 189, 255)").unwrap();
        assert!((c.r - 158.0 / 255.0).abs() < 1e-4);
        assert!((c.g - 189.0 / 255.0).abs() < 1e-4);
        assert!((c.b - 1.0).abs() < 1e-4);
        assert_eq!(c.a, 1.0);
    }

    #[test]
    fn parses_hsl_without_alpha() {
        let c = parse_css_color("hsl(35,8%,85%)").unwrap();
        // Standard HSL->RGB formula:
        // h=35, s=0.08, l=0.85
        // c = (1 - |2*0.85 - 1|) * 0.08 = 0.3 * 0.08 = 0.024
        // h' = 35/60 = 0.5833
        // x = 0.024 * (1 - |0.5833 - 1|) = 0.024 * 0.5833 ≈ 0.014
        // m = 0.85 - 0.012 = 0.838
        // r' = 0.024, g' = 0.014, b' = 0 (sector 0-1)
        // rgb ≈ (0.862, 0.852, 0.838)
        assert!((c.r - 0.862).abs() < 0.01);
        assert!((c.g - 0.852).abs() < 0.01);
        assert!((c.b - 0.838).abs() < 0.01);
        assert_eq!(c.a, 1.0);
    }

    #[test]
    fn parses_hsla_with_alpha() {
        let c = parse_css_color("hsla(0,3%,85%,0.84)").unwrap();
        assert!((c.a - 0.84).abs() < 1e-4);
        // Standard HSL->RGB formula:
        // h=0, s=0.03, l=0.85
        // c = 0.3 * 0.03 = 0.009
        // h' = 0/60 = 0
        // x = 0.009 * (1 - |0 - 1|) = 0
        // m = 0.85 - 0.0045 = 0.8455
        // r' = 0.009, g' = 0, b' = 0 (sector 0)
        // rgb ≈ (0.8545, 0.8455, 0.8455)
        assert!((c.r - 0.8545).abs() < 0.01);
        assert!((c.g - 0.8455).abs() < 0.01);
        assert!((c.b - 0.8455).abs() < 0.01);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_css_color("not-a-color").is_err());
    }

    /// Every (h, s, l) in a swept grid, through `hsl_to_rgb` and back, has
    /// to come out where it went in -- the components themselves wherever
    /// they are defined, and the RGB triple always.
    #[test]
    fn hsl_round_trips_across_a_swept_grid() {
        let mut checked = 0;
        for hue_step in 0..36 {
            let h = hue_step as f32 * 10.0;
            for sat_step in 1..=10 {
                let s = sat_step as f32 / 10.0;
                for light_step in 1..10 {
                    let l = light_step as f32 / 10.0;
                    let (r, g, b) = hsl_to_rgb(h, s, l);
                    let (h2, s2, l2) = rgb_to_hsl(r, g, b);
                    assert!((h2 - h).abs() < 1e-3, "hue {h} -> rgb -> {h2} (s={s}, l={l})");
                    assert!((s2 - s).abs() < 1e-4, "saturation {s} -> rgb -> {s2} (h={h}, l={l})");
                    assert!((l2 - l).abs() < 1e-4, "lightness {l} -> rgb -> {l2} (h={h}, s={s})");
                    let (r2, g2, b2) = hsl_to_rgb(h2, s2, l2);
                    assert!(
                        (r2 - r).abs() < 1e-5 && (g2 - g).abs() < 1e-5 && (b2 - b).abs() < 1e-5,
                        "rgb ({r}, {g}, {b}) -> hsl -> rgb ({r2}, {g2}, {b2})"
                    );
                    checked += 1;
                }
            }
        }
        assert_eq!(checked, 36 * 10 * 9);
    }

    /// A grey has no hue to recover, so only the RGB round trip can be
    /// asserted -- and it must be exact, not merely close, because greys are
    /// what most of the Mapbox Standard dark config actually asks for
    /// (`colorWater` is `hsl(0, 0%, 76%)`).
    #[test]
    fn greys_round_trip_through_an_undefined_hue() {
        for step in 0..=100 {
            let l = step as f32 / 100.0;
            let (r, g, b) = hsl_to_rgb(0.0, 0.0, l);
            assert_eq!((r, g, b), (l, l, l), "hsl(_, 0%, {l}) should be the grey {l}");
            let (h2, s2, l2) = rgb_to_hsl(r, g, b);
            assert_eq!(h2, 0.0, "a grey has no hue, so it reports 0");
            assert_eq!(s2, 0.0, "a grey has no saturation");
            assert!((l2 - l).abs() < 1e-6);
            assert_eq!(hsl_to_rgb(h2, s2, l2), (r, g, b));
        }
    }

    /// The six fully-saturated primaries and secondaries, where `chroma`
    /// is 1 and the sector boundaries are exactly on a multiple of 60 --
    /// the inputs most likely to fall into the wrong sector.
    #[test]
    fn fully_saturated_hues_land_on_their_own_sector_boundary() {
        for (hue, expected) in [
            (0.0, (1.0, 0.0, 0.0)),
            (60.0, (1.0, 1.0, 0.0)),
            (120.0, (0.0, 1.0, 0.0)),
            (180.0, (0.0, 1.0, 1.0)),
            (240.0, (0.0, 0.0, 1.0)),
            (300.0, (1.0, 0.0, 1.0)),
        ] {
            let rgb = hsl_to_rgb(hue, 1.0, 0.5);
            assert_eq!(rgb, expected, "hsl({hue}, 100%, 50%)");
            let (h2, s2, l2) = rgb_to_hsl(rgb.0, rgb.1, rgb.2);
            assert!((h2 - hue).abs() < 1e-3, "hue {hue} came back as {h2}");
            assert!((s2 - 1.0).abs() < 1e-6);
            assert!((l2 - 0.5).abs() < 1e-6);
        }
    }

    /// Near-black and near-white, where `1 - |2l - 1|` approaches zero and
    /// the saturation division is at its most ill-conditioned. Pure black
    /// and pure white have no saturation to recover at all.
    #[test]
    fn near_black_and_near_white_do_not_blow_up_the_saturation() {
        assert_eq!(rgb_to_hsl(0.0, 0.0, 0.0), (0.0, 0.0, 0.0));
        assert_eq!(rgb_to_hsl(1.0, 1.0, 1.0), (0.0, 0.0, 1.0));
        for l in [0.001_f32, 0.005, 0.01, 0.99, 0.995, 0.999] {
            for h in [0.0_f32, 90.0, 210.0, 330.0] {
                let (r, g, b) = hsl_to_rgb(h, 1.0, l);
                let (h2, s2, l2) = rgb_to_hsl(r, g, b);
                assert!((0.0..=1.0).contains(&s2), "saturation {s2} out of range at l={l}");
                let (r2, g2, b2) = hsl_to_rgb(h2, s2, l2);
                assert!(
                    (r2 - r).abs() < 1e-5 && (g2 - g).abs() < 1e-5 && (b2 - b).abs() < 1e-5,
                    "hsl({h}, 100%, {l}) -> rgb ({r}, {g}, {b}) -> hsl -> rgb ({r2}, {g2}, {b2})"
                );
            }
        }
    }

    /// The one colour the whole Mapbox Standard background hangs off:
    /// `colorLand`'s schema default. Decomposing and recomposing it has to
    /// give back the same colour `parse_css_color` produces directly.
    #[test]
    fn the_standard_land_colour_survives_a_decompose_recompose() {
        let land = parse_css_color("hsl(20, 20%, 95%)").unwrap();
        let (h, s, l) = rgb_to_hsl(land.r, land.g, land.b);
        assert!((h - 20.0).abs() < 1e-2, "hue came back as {h}");
        assert!((s - 0.20).abs() < 1e-4, "saturation came back as {s}");
        assert!((l - 0.95).abs() < 1e-4, "lightness came back as {l}");
        let (r, g, b) = hsl_to_rgb(h, s, l);
        assert!((r - land.r).abs() < 1e-6 && (g - land.g).abs() < 1e-6 && (b - land.b).abs() < 1e-6);
    }
}

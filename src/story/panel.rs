//! Panel themes, motion and text fit (spec §4.2): the named looks a panel
//! or callout card is drawn in -- fill, text colours, corner radius and
//! padding (Task 9) -- plus, from Task 10, the slide-in offset elements
//! enter with and how oversized text shrinks then ellipsizes.
//!
//! Created in Task 9 because [`crate::story::layout::LayoutItem`] carries a
//! [`PanelTheme`] (pre-flight ruling F1).
//!
//! Pure (no clock, no GPU, no I/O) and `#[path]`-included by
//! `web/src/lib.rs` and `ios/src/lib.rs`.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing))]

use super::layout::{TextMeasure, TEXT_FLOOR};
use super::script::{Easing, Placement};
use unicode_segmentation::UnicodeSegmentation;

/// How a card looks. Colours are straight (not premultiplied) RGBA.
#[derive(Clone, Debug, PartialEq)]
pub struct PanelTheme {
    pub fill: [u8; 4],
    /// The headline and body colour.
    pub text: [u8; 4],
    /// The source line's colour.
    pub secondary: [u8; 4],
    /// Corner radius, 1080 units.
    pub radius: f32,
    /// Room between the card's edge and its content, 1080 units.
    pub padding: f32,
}

/// Every built-in theme's corner radius, 1080 units.
pub const PANEL_RADIUS: f32 = 12.0;
/// Every built-in theme's padding, 1080 units.
pub const PANEL_PADDING: f32 = 24.0;
/// Every built-in theme's fill opacity: near-solid, so map labels and road
/// shields under a card do not read through its text (Task 11 review; it
/// was 0.86 dark / 0.92 light and accent).
pub const PANEL_FILL_ALPHA: f32 = 0.98;

/// `0.0..=1.0` as a byte, rounded.
fn alpha_byte(a: f32) -> u8 {
    (a.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// WCAG 2 relative luminance of an sRGB colour with channels in `0..=1`.
fn relative_luminance(r: f32, g: f32, b: f32) -> f32 {
    let lin = |c: f32| if c <= 0.039_28 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) };
    0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
}

/// The named theme, or `None` for a name this function does not know.
///
/// - `dark` (also the default when `name` is `None`): fill #141414 at 0.98,
///   text #ffffff, secondary #c8c8c8;
/// - `light`: fill #ffffff at 0.98, text #141414, secondary #505050;
/// - `accent:<colour>`: fill = the colour at 0.98, text black or white,
///   whichever has the higher WCAG contrast against it (so >= 4.5 whenever
///   either can reach it), secondary = the text colour at 0.8.
///
/// Every built-in theme has radius 12 and padding 24. Task 2 already
/// refuses unknown names and unparsable accent colours structurally.
pub fn theme(name: Option<&str>) -> Option<PanelTheme> {
    let name = name.unwrap_or("dark");
    let (fill, text, secondary) = match name {
        "dark" => ([0x14, 0x14, 0x14, alpha_byte(PANEL_FILL_ALPHA)], [255, 255, 255, 255], [0xc8, 0xc8, 0xc8, 255]),
        "light" => ([255, 255, 255, alpha_byte(PANEL_FILL_ALPHA)], [0x14, 0x14, 0x14, 255], [0x50, 0x50, 0x50, 255]),
        other => {
            let colour = other.strip_prefix("accent:")?;
            let c = crate::style_spec::parse_css_color(colour).ok()?;
            let l = relative_luminance(c.r, c.g, c.b);
            let against_black = (l + 0.05) / 0.05;
            let against_white = 1.05 / (l + 0.05);
            let ink: [u8; 3] = if against_black >= against_white { [0, 0, 0] } else { [255, 255, 255] };
            let [ir, ig, ib] = ink;
            (
                [alpha_byte(c.r), alpha_byte(c.g), alpha_byte(c.b), alpha_byte(PANEL_FILL_ALPHA)],
                [ir, ig, ib, 255],
                [ir, ig, ib, alpha_byte(0.8)],
            )
        }
    };
    Some(PanelTheme { fill, text, secondary, radius: PANEL_RADIUS, padding: PANEL_PADDING })
}

/// How far an entering element slides, 1080 units (spec §4.2 "Motion").
pub const SLIDE_UNITS: f32 = 40.0;

/// The CSS `ease-out` curve ([`super::timeline::ActiveElement::enter`]'s
/// doc comment names it), applied to `enter` (already clamped to `0..1`
/// there).
fn ease_out(enter: f32) -> f32 {
    Easing::EaseOut.apply(enter as f64) as f32
}

/// The offset an entering element is drawn at, 1080 units (spec §4.2
/// "fade and slide-in over `fade_s`"): `(0, (1 - ease_out(enter)) *
/// SLIDE_UNITS)` for a lower-third, side or corner card (it slides up into
/// place), `(0, 0)` for a title (it already fills the band, so sliding it
/// would slide the dimmed map's edge into view) and `(0, 0)` always under
/// `reduced_motion` (piece 3's flag; `capture` passes `false`) -- the
/// motion becomes a plain fade, driven by [`super::timeline::ActiveElement::alpha`]
/// alone. The slide plays only on the way in, never back out (Task 3
/// review round 1): `enter` never eases down.
pub fn slide_offset(placement: Placement, enter: f32, reduced_motion: bool) -> (f32, f32) {
    if reduced_motion || placement == Placement::Title {
        (0.0, 0.0)
    } else {
        (0.0, (1.0 - ease_out(enter)) * SLIDE_UNITS)
    }
}

/// [`fit_text`]'s shrink step, 1080 units.
const FIT_SHRINK_STEP: f32 = 2.0;

/// Slack `fit_text`'s own fit check allows a measurement, so a result
/// that lands within float noise of `max` still counts as fitting.
const FIT_EPS: f32 = 1e-2;

/// Fits `text` into `max` (width, height), starting at `size` (1080
/// units; spec §4.1 "Overflow"): if it already fits whole, returns it
/// unchanged. Otherwise shrinks by [`FIT_SHRINK_STEP`] units at a time
/// down to [`TEXT_FLOOR`]; if even the floor doesn't fit whole, truncates
/// with an ellipsis -- at the longest WORD prefix (split on spaces) when
/// `text` has word breaks, otherwise (continuous CJK/Thai, or one
/// pathological token; controller ruling, Task 10 review F3) at the
/// longest GRAPHEME-CLUSTER prefix, so an emoji held together by a ZWJ or
/// a base character with a combining mark is never split apart. Every
/// candidate this considers, down to the bare ellipsis alone, is itself
/// re-checked by the same fit test as the whole text was -- nothing is
/// ever returned without having been verified to fit (Task 10 review
/// F2): if not even `"…"` fits `max`, the text is dropped rather than
/// drawn overflowing, and that is its own warning code.
///
/// Returns the size actually used, the text as it should be drawn (empty
/// when dropped), and a warning code whenever the result is not the
/// whole input at `size`: `"text_shrunk"` (a smaller size still drew the
/// text whole), `"text_truncated"` (the floor needed an ellipsis, verified
/// to fit) or `"text_dropped"` (nothing, not even `"…"`, fits `max`).
/// Pure: fit is decided by `measure`, the caller's text system, never by
/// counting characters -- so CJK and Latin text are both handled by
/// whatever it reports (plan ruling D4).
pub fn fit_text(text: &str, size: f32, max: (f32, f32), measure: &dyn TextMeasure) -> (f32, String, Option<&'static str>) {
    let (max_w, max_h) = max;
    let fits = |s: f32, t: &str| {
        let (w, h) = measure.measure(t, s, Some(max_w));
        w <= max_w + FIT_EPS && h <= max_h + FIT_EPS
    };
    if fits(size, text) {
        return (size, text.to_string(), None);
    }
    let mut s = size;
    while s > TEXT_FLOOR {
        s = (s - FIT_SHRINK_STEP).max(TEXT_FLOOR);
        if fits(s, text) {
            return (s, text.to_string(), Some("text_shrunk"));
        }
    }
    let mut best: Option<String> = None;
    let words: Vec<&str> = text.split(' ').filter(|w| !w.is_empty()).collect();
    if words.len() > 1 {
        for n in 1..=words.len() {
            let candidate = format!("{}…", words.iter().take(n).copied().collect::<Vec<_>>().join(" "));
            if fits(TEXT_FLOOR, &candidate) {
                best = Some(candidate);
            } else {
                break;
            }
        }
    }
    if best.is_none() && fits(TEXT_FLOOR, "…") {
        // No word boundary gave anything (a single token, or none at all): grow a grapheme-cluster prefix instead,
        // one cluster at a time, so a multi-codepoint emoji or a base character plus a combining mark is taken or
        // left whole, never split in the middle.
        best = Some("…".to_string());
        let mut prefix = String::new();
        for g in text.graphemes(true) {
            let candidate = format!("{prefix}{g}…");
            if fits(TEXT_FLOOR, &candidate) {
                prefix.push_str(g);
                best = Some(candidate);
            } else {
                break;
            }
        }
    }
    match best {
        Some(text) => (TEXT_FLOOR, text, Some("text_truncated")),
        None => (TEXT_FLOOR, String::new(), Some("text_dropped")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_named_themes_are_the_spec_values() {
        let dark = theme(Some("dark")).unwrap();
        assert_eq!(dark.fill, [0x14, 0x14, 0x14, 250]);
        assert_eq!(dark.text, [255, 255, 255, 255]);
        assert_eq!(dark.secondary, [0xc8, 0xc8, 0xc8, 255]);
        assert_eq!((dark.radius, dark.padding), (12.0, 24.0));
        assert_eq!(theme(None), Some(dark));
        let light = theme(Some("light")).unwrap();
        assert_eq!(light.fill, [255, 255, 255, 250]);
        assert_eq!(light.text, [0x14, 0x14, 0x14, 255]);
        assert_eq!(theme(Some("neon")), None);
        assert_eq!(theme(Some("accent:#zz0000")), None);
    }

    #[test]
    fn an_accent_theme_fills_with_its_colour_and_picks_readable_ink() {
        let yellow = theme(Some("accent:#ffe600")).unwrap();
        assert_eq!(yellow.fill, [0xff, 0xe6, 0x00, 250]);
        assert_eq!(yellow.text, [0, 0, 0, 255]);
        assert_eq!(theme(Some("accent:#1a237e")).unwrap().text, [255, 255, 255, 255]);
    }

    /// Named and valued as the Task 10 brief's sketch: the same pair the
    /// previous test already pins, under the interface's own test name.
    #[test]
    fn accent_themes_pick_readable_text() {
        assert_eq!(theme(Some("accent:#ffe600")).unwrap().text, [0, 0, 0, 255]);
        assert_eq!(theme(Some("accent:#1a237e")).unwrap().text, [255, 255, 255, 255]);
    }

    #[test]
    fn reduced_motion_turns_the_slide_into_a_plain_fade() {
        assert_eq!(slide_offset(Placement::LowerThird, 0.0, false), (0.0, SLIDE_UNITS));
        assert_eq!(slide_offset(Placement::LowerThird, 0.0, true), (0.0, 0.0));
        assert_eq!(slide_offset(Placement::LowerThird, 1.0, false), (0.0, 0.0));
    }

    #[test]
    fn every_non_title_placement_slides_and_a_title_never_does() {
        for placement in [Placement::LowerThird, Placement::Side, Placement::Corner] {
            assert_eq!(slide_offset(placement, 0.0, false), (0.0, SLIDE_UNITS), "{placement:?}");
            assert_eq!(slide_offset(placement, 0.0, true), (0.0, 0.0), "{placement:?} reduced");
        }
        assert_eq!(slide_offset(Placement::Title, 0.0, false), (0.0, 0.0));
        assert_eq!(slide_offset(Placement::Title, 0.0, true), (0.0, 0.0));
    }

    struct Mono;
    impl TextMeasure for Mono {
        fn measure(&self, text: &str, size: f32, wrap: Option<f32>) -> (f32, f32) {
            crate::story::layout::test_support::mono(text, size, wrap)
        }
    }

    #[test]
    fn fit_text_keeps_text_whole_when_it_already_fits() {
        let (size, text, warning) = fit_text("Harbour", 56.0, (400.0, 200.0), &Mono);
        assert_eq!((size, text.as_str(), warning), (56.0, "Harbour", None));
    }

    #[test]
    fn fit_text_shrinks_before_it_truncates() {
        // "Harbour" is 7 chars; at 28 (the floor) it is 7 * 0.5 * 28 = 98 wide, 1.2 * 28 = 33.6 tall -- both fit a
        // 100 x 40 box, but nothing above the floor does (56 is 196 wide).
        let (size, text, warning) = fit_text("Harbour", 56.0, (100.0, 40.0), &Mono);
        assert_eq!((size, text.as_str(), warning), (TEXT_FLOOR, "Harbour", Some("text_shrunk")));
    }

    #[test]
    fn fit_text_ellipsizes_at_the_floor_through_the_measure_callback() {
        let long = "one two three four five six seven eight nine ten".repeat(4);
        let (size, text, warning) = fit_text(&long, 56.0, (120.0, 60.0), &Mono);
        assert_eq!(size, TEXT_FLOOR);
        assert!(text.ends_with('…') && text.len() < long.len(), "{text}");
        assert_eq!(warning, Some("text_truncated"));
        // The ellipsis is decided by what `Mono` reports fitting, not a character count: a measure that always
        // reports everything as 1x1 never needs to shrink or truncate.
        struct TinyAlways;
        impl TextMeasure for TinyAlways {
            fn measure(&self, _: &str, _: f32, _: Option<f32>) -> (f32, f32) {
                (1.0, 1.0)
            }
        }
        let (size, text, warning) = fit_text(&long, 56.0, (120.0, 60.0), &TinyAlways);
        assert_eq!((size, text.as_str(), warning), (56.0, long.as_str(), None));
    }

    /// A measure a one-line-tall box can never satisfy: nothing, not even a bare `"…"`, fits (Task 10 review
    /// F2). `fit_text` must say so explicitly rather than return an unverified fallback.
    struct NeverFits;
    impl TextMeasure for NeverFits {
        fn measure(&self, _: &str, size: f32, _: Option<f32>) -> (f32, f32) {
            (1.0, size)
        }
    }

    #[test]
    fn fit_text_drops_the_text_when_not_even_an_ellipsis_fits() {
        let (size, text, warning) = fit_text("word word word", 56.0, (100.0, 0.0), &NeverFits);
        assert_eq!(size, TEXT_FLOOR);
        assert_eq!(text, "");
        assert_eq!(warning, Some("text_dropped"));
    }

    #[test]
    fn fit_text_never_returns_a_result_its_own_fit_check_would_reject() {
        // For a spread of budgets, from generous down to nothing, whatever `fit_text` returns must itself pass
        // the same fit test it was asked to satisfy (or be the empty, explicitly-dropped case) -- it never hands
        // back an unverified guess (Task 10 review F2).
        let fits = |t: &str, s: f32, max_w: f32, max_h: f32| {
            let (w, h) = crate::story::layout::test_support::mono(t, s, Some(max_w));
            w <= max_w + 0.5 && h <= max_h + 0.5
        };
        let long = "word ".repeat(50);
        for max_h in [1000.0, 200.0, 60.0, 34.0, 10.0, 1.0, 0.0] {
            let (size, text, code) = fit_text(&long, 56.0, (150.0, max_h), &Mono);
            if text.is_empty() {
                assert_eq!(code, Some("text_dropped"), "max_h={max_h}");
            } else {
                assert!(fits(&text, size, 150.0, max_h), "max_h={max_h}: {text:?} at {size} does not actually fit");
            }
        }
    }

    #[test]
    fn a_cjk_headline_with_no_spaces_keeps_its_leading_content_then_ellipsizes() {
        // Continuous CJK and Thai carry no ASCII space, so `words.len() <= 1`: truncation must fall back to the
        // grapheme-cluster path rather than collapsing to a bare ellipsis (Task 10 review F3).
        for text in [
            "这是一段很长很长完全没有空格的中文标题用来测试省略号的位置是否正确而不是直接被清空的情况",
            "นี่คือหัวข้อข่าวภาษาไทยที่ยาวมากและไม่มีการเว้นวรรคเลยเพื่อทดสอบการตัดข้อความที่ถูกต้อง",
        ] {
            let (size, out, code) = fit_text(text, 56.0, (80.0, 40.0), &Mono);
            assert_eq!(size, TEXT_FLOOR, "{text}");
            assert_eq!(code, Some("text_truncated"), "{text}");
            assert!(out.ends_with('…'), "{out:?}");
            assert!(out.chars().count() > 1, "{out:?}: collapsed to a bare ellipsis");
        }
    }

    #[test]
    fn truncation_never_splits_a_grapheme_cluster() {
        // A measure that counts GRAPHEME CLUSTERS, not scalar codepoints, as its unit of width: if the grapheme
        // fallback instead walked `.chars()` or raw bytes, a cluster held together by a ZWJ or a base character
        // plus a combining mark could be cut apart. The check below -- the truncated text, minus its ellipsis,
        // re-segmented by the same grapheme rules, must equal an exact prefix of the original's own clusters --
        // catches that (Task 10 review F3).
        struct ClusterWidth;
        impl TextMeasure for ClusterWidth {
            fn measure(&self, text: &str, size: f32, _wrap: Option<f32>) -> (f32, f32) {
                (text.graphemes(true).count() as f32 * size, 1.2 * size)
            }
        }
        let assert_grapheme_safe = |original: &str, max_w: f32| {
            let (size, text, code) = fit_text(original, 56.0, (max_w, 1000.0), &ClusterWidth);
            assert_eq!(code, Some("text_truncated"), "{original:?}");
            assert_eq!(size, TEXT_FLOOR);
            assert!(text.ends_with('…'), "{text:?}");
            let stripped = text.strip_suffix('…').unwrap();
            let orig: Vec<&str> = original.graphemes(true).collect();
            let got: Vec<&str> = stripped.graphemes(true).collect();
            assert!(!got.is_empty(), "{original:?}: nothing survived truncation");
            assert_eq!(got.as_slice(), &orig[..got.len()], "{original:?} -> {text:?}");
        };
        // A family emoji, 4 faces joined by ZWJs into ONE cluster, repeated so no single prefix is the whole
        // text; it has no ASCII space, so this exercises the grapheme fallback, not the word-prefix path.
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}"; // 👨‍👩‍👧‍👦
        assert_grapheme_safe(&family.repeat(6), 56.0);
        // A base letter plus a combining acute accent: each cluster is 2 scalar values.
        let accented = "e\u{0301}"; // é as two codepoints
        assert_grapheme_safe(&accented.repeat(20), 140.0);
    }
}

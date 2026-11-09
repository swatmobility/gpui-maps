//! **S1 (Task 1): output aspect ratios, resolution tiers and safe areas.**
//!
//! A story's `output` names an `aspect` (the frame's shape) and a `tier`
//! (its resolution and sharpness), independently of each other: the same
//! chapter, panels and camera path render identically -- just sharper --
//! at every tier, because layout always works in 1080-referenced units
//! and the tier only changes the pixel ratio (spec §2.4). `safe_area`
//! marks the rectangles a short-form app's own UI (captions, buttons, the
//! progress bar) draws over the video, so layout can avoid them.
//!
//! This module is pure (no clock, no GPU, no I/O) and is `#[path]`-included
//! by both `web/src/lib.rs` and `ios/src/lib.rs`, so it compiles on wasm32
//! and iOS exactly as written here.

use serde::{Deserialize, Serialize};

/// The frame's shape. The serde spelling is the short form used in a
/// story script (`"16:9"`, `"9:16"`, `"4:5"`, `"1:1"`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Aspect {
    #[serde(rename = "16:9")]
    Landscape,
    #[serde(rename = "9:16")]
    Portrait,
    #[serde(rename = "4:5")]
    Feed,
    #[serde(rename = "1:1")]
    Square,
}

/// A resolution tier: how sharp the frame is, at a fixed set of shapes
/// per [`Aspect`] (spec §2.4's table). The short edge is what names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Hd,
    Fullhd,
    Qhd,
    #[serde(rename = "4k")]
    Uhd,
}

/// The entitlement a [`Tier`] carries. Nothing is enforced in 1a (spec
/// §2.4): this is recorded in the manifest only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Entitlement {
    Free,
    Pro,
}

/// A short-form app's own UI, as a name. `none` covers every aspect (no
/// rectangles); the others are meaningful only for the aspect they were
/// measured against -- see [`safe_area_rects`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SafeArea {
    Tiktok,
    Reels,
    Shorts,
    Youtube,
    None,
    /// The union of `tiktok`/`reels`/`shorts` (spec §5.1): the strictest
    /// box for one video posted to every short-form platform at once.
    All,
}

impl SafeArea {
    /// Its `output.safe_area` name (R10's validation report), never the
    /// Rust variant name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            SafeArea::Tiktok => "tiktok",
            SafeArea::Reels => "reels",
            SafeArea::Shorts => "shorts",
            SafeArea::Youtube => "youtube",
            SafeArea::None => "none",
            SafeArea::All => "all",
        }
    }
}

/// A rectangle `(left, top, right, bottom)` in 1080-referenced units.
pub type Rect = (f32, f32, f32, f32);

/// Every [`Tier`]'s short edge is expressed as a multiple of this: a
/// `fullhd` frame is exactly 1080 units of short edge, so its pixel
/// ratio is 1.0 and its layout units equal its physical pixels.
pub const REFERENCE_SHORT_EDGE: u32 = 1080;

impl Tier {
    /// The short edge in physical pixels (spec §2.4's "Short edge"
    /// column).
    pub fn short_edge(self) -> u32 {
        match self {
            Tier::Hd => 720,
            Tier::Fullhd => 1080,
            Tier::Qhd => 1440,
            Tier::Uhd => 2160,
        }
    }

    /// The entitlement tag recorded in the manifest (spec §2.4:
    /// "Entitlement"). Not enforced in 1a.
    pub fn entitlement(self) -> Entitlement {
        match self {
            Tier::Uhd => Entitlement::Pro,
            Tier::Hd | Tier::Fullhd | Tier::Qhd => Entitlement::Free,
        }
    }

    /// The name used in the manifest and CLI (`"hd"`, `"fullhd"`,
    /// `"qhd"`, `"4k"` -- matching its serde spelling).
    pub fn name(self) -> &'static str {
        match self {
            Tier::Hd => "hd",
            Tier::Fullhd => "fullhd",
            Tier::Qhd => "qhd",
            Tier::Uhd => "4k",
        }
    }
}

impl Aspect {
    /// The frame's width:height ratio as small integers.
    pub fn ratio(self) -> (u32, u32) {
        match self {
            Aspect::Landscape => (16, 9),
            Aspect::Portrait => (9, 16),
            Aspect::Feed => (4, 5),
            Aspect::Square => (1, 1),
        }
    }

    /// `true` for the two taller-than-wide aspects (`9:16` and `4:5`),
    /// which is what [`default_safe_area`] and the layout function's
    /// `side` placement (spec §4.1) branch on.
    pub fn is_portrait_like(self) -> bool {
        matches!(self, Aspect::Portrait | Aspect::Feed)
    }
}

/// The frame's physical size in pixels, from the spec §2.4 table.
pub fn frame_size(aspect: Aspect, tier: Tier) -> (u32, u32) {
    match tier {
        Tier::Hd => match aspect {
            Aspect::Landscape => (1280, 720),
            Aspect::Portrait => (720, 1280),
            Aspect::Feed => (720, 900),
            Aspect::Square => (720, 720),
        },
        Tier::Fullhd => match aspect {
            Aspect::Landscape => (1920, 1080),
            Aspect::Portrait => (1080, 1920),
            Aspect::Feed => (1080, 1350),
            Aspect::Square => (1080, 1080),
        },
        Tier::Qhd => match aspect {
            Aspect::Landscape => (2560, 1440),
            Aspect::Portrait => (1440, 2560),
            Aspect::Feed => (1440, 1800),
            Aspect::Square => (1440, 1440),
        },
        Tier::Uhd => match aspect {
            Aspect::Landscape => (3840, 2160),
            Aspect::Portrait => (2160, 3840),
            Aspect::Feed => (2160, 2700),
            Aspect::Square => (2160, 2160),
        },
    }
}

/// `short_edge / 1080`: `0.6667` (hd), `1.0` (fullhd), `1.3333` (qhd),
/// `2.0` (4k). Layout works in 1080-referenced units; this is what
/// turns those into physical pixels (spec §2.4 "Scaling").
pub fn pixel_ratio(tier: Tier) -> f32 {
    ratio_for_short_edge(tier.short_edge() as f32)
}

/// `short_edge / 1080`: the pixel ratio that puts a frame with this short
/// edge (physical pixels) into 1080-referenced layout units -- a capture's
/// normalisation ([`pixel_ratio`]), and the live map's for a story frame
/// on a window of any size (spec §13), so both lay out in one frame.
#[must_use]
pub fn ratio_for_short_edge(short_edge: f32) -> f32 {
    short_edge / REFERENCE_SHORT_EDGE as f32
}

/// The [`Tier`] whose short edge is nearest to, but not below,
/// `short_edge` -- `4k` once `short_edge` exceeds every tier's short
/// edge, so this never fails to name a tier. Used to key a bitrate
/// table (or any other tier-shaped lookup) off a resolved physical
/// size rather than a script's explicit `tier` field, which a custom
/// `width`/`height` or an old `preset` never sets.
pub fn tier_for_short_edge(short_edge: u32) -> Tier {
    if short_edge <= Tier::Hd.short_edge() {
        Tier::Hd
    } else if short_edge <= Tier::Fullhd.short_edge() {
        Tier::Fullhd
    } else if short_edge <= Tier::Qhd.short_edge() {
        Tier::Qhd
    } else {
        Tier::Uhd
    }
}

/// The H.264 level for a `width`x`height` stream at `fps` (spec §2.4
/// "Encoding"), compared by `(long edge, short edge)` so a portrait frame
/// takes its landscape twin's level (`1080x1920` is `"4.0"`, ruling F15):
///
/// | Frame (long x short) | <= 30 fps | <= 60 fps |
/// |---|---|---|
/// | <= 1280x720 | `3.1` | `3.2` |
/// | <= 1920x1080 | `4.0` | `4.2` |
/// | <= 2560x1440 | `5.0` | `5.1` |
/// | <= 3840x2160 | `5.1` | `5.2` |
/// | above | `6.0` | `6.1` |
///
/// Above 60 fps a frame takes the NEXT row's 60 fps level (`6.2` above
/// 4K): each is the lowest level whose macroblock rate covers twice the
/// row's 60 fps rate. Every entry is checked against the H.264 Annex A
/// frame-size and macroblock-rate limits by this module's tests.
#[must_use]
pub fn h264_level(width: u32, height: u32, fps: u32) -> &'static str {
    const ROWS: [((u32, u32), &str, &str); 5] = [
        ((1280, 720), "3.1", "3.2"),
        ((1920, 1080), "4.0", "4.2"),
        ((2560, 1440), "5.0", "5.1"),
        ((3840, 2160), "5.1", "5.2"),
        ((u32::MAX, u32::MAX), "6.0", "6.1"),
    ];
    let (long, short) = (width.max(height), width.min(height));
    let row = ROWS.iter().position(|((l, s), ..)| long <= *l && short <= *s).unwrap_or(ROWS.len() - 1);
    match fps {
        0..=30 => ROWS.get(row).map_or("6.0", |r| r.1),
        31..=60 => ROWS.get(row).map_or("6.1", |r| r.2),
        _ => ROWS.get(row + 1).map_or("6.2", |r| r.2),
    }
}

/// The encoder's maximum bitrate for `tier`, in kbit/s (spec §2.4: "one
/// table"): `hd` 5 000, `fullhd` 10 000, `qhd` 16 000, `4k` 35 000 at up
/// to 30 fps, and 1.5x that above 30 fps. A custom size keys this through
/// [`tier_for_short_edge`] (ruling F15).
#[must_use]
pub fn bitrate_kbps(tier: Tier, fps: u32) -> u32 {
    let base = match tier {
        Tier::Hd => 5_000,
        Tier::Fullhd => 10_000,
        Tier::Qhd => 16_000,
        Tier::Uhd => 35_000,
    };
    if fps > 30 {
        base * 3 / 2
    } else {
        base
    }
}

/// A raster pixel ratio above this draws `@2x` tiles where the source
/// offers them (spec §2.4 "Scaling"): `4k` (2.0) does, `qhd` (1.333)
/// does not.
pub const RASTER_2X_ABOVE: f32 = 1.5;

/// `true` when a raster source should use `@2x` tiles: the pixel ratio is
/// above [`RASTER_2X_ABOVE`] and the source offers them
/// ([`raster_offers_2x`]). Otherwise the 1x tiles are drawn upscaled
/// (validation warns `raster_upscaled` above the threshold).
#[must_use]
pub fn use_2x_rasters(pixel_ratio: f32, offers_2x: bool) -> bool {
    offers_2x && pixel_ratio > RASTER_2X_ABOVE
}

/// Whether a raster source offers `@2x` tiles: its URL template has a
/// `{r}` or `{ratio}` placeholder (the Mapbox / MapLibre spellings), or
/// already names `@2x` tiles, or its TileJSON states a `scale` of 2 or
/// more (`tilejson_scale`, `None` when there is no TileJSON).
#[must_use]
pub fn raster_offers_2x(template: &str, tilejson_scale: Option<f64>) -> bool {
    template.contains("{r}") || template.contains("{ratio}") || template.contains("@2x") || tilejson_scale.is_some_and(|s| s >= 2.0)
}

/// `template` with its `{r}` / `{ratio}` placeholder filled: `"@2x"` when
/// `two_x`, else `""`. A template that names `@2x` literally is left as
/// it is (its author chose the tiles).
#[must_use]
pub fn raster_template(template: &str, two_x: bool) -> String {
    let r = if two_x { "@2x" } else { "" };
    template.replace("{ratio}", r).replace("{r}", r)
}

/// The raster tile template `scene.raster` becomes at `pixel_ratio`, and
/// whether it draws `@2x` tiles: [`raster_template`] filled per
/// [`use_2x_rasters`] of [`raster_offers_2x`]. Credits do not change
/// with the tile density.
#[must_use]
pub fn resolve_raster(template: &str, pixel_ratio: f32, tilejson_scale: Option<f64>) -> (String, bool) {
    let offers = raster_offers_2x(template, tilejson_scale);
    let two_x = use_2x_rasters(pixel_ratio, offers) || (offers && template.contains("@2x"));
    (raster_template(template, use_2x_rasters(pixel_ratio, offers)), two_x)
}

/// `none` for `16:9` and `1:1`, `reels` for `9:16` and `4:5` (spec
/// §2.4's default). A script's explicit `output.safe_area` overrides
/// this.
pub fn default_safe_area(aspect: Aspect) -> SafeArea {
    if aspect.is_portrait_like() {
        SafeArea::Reels
    } else {
        SafeArea::None
    }
}

/// `reels`/`tiktok`/`shorts`, measured against the `9:16` frame
/// (1080x1920 units): top bar, bottom bar, right-hand action rail.
///
/// These rectangles are approximations of each platform's UI as of
/// 2026-09: every named app redraws its chrome over time, so this table
/// is a best-effort snapshot, not a contract with the platform.
const REELS_916: &[Rect] = &[(0.0, 0.0, 1080.0, 220.0), (0.0, 1500.0, 1080.0, 1920.0), (940.0, 700.0, 1080.0, 1500.0)];
const TIKTOK_916: &[Rect] = &[(0.0, 0.0, 1080.0, 160.0), (0.0, 1440.0, 1080.0, 1920.0), (930.0, 800.0, 1080.0, 1440.0)];
const SHORTS_916: &[Rect] = &[(0.0, 0.0, 1080.0, 190.0), (0.0, 1540.0, 1080.0, 1920.0), (940.0, 700.0, 1080.0, 1540.0)];
/// `all` (spec §5.1): the union of [`TIKTOK_916`], [`REELS_916`] and
/// [`SHORTS_916`] -- the strictest top/bottom band and the widest rail,
/// so one render stays clear of every platform's own UI at once.
const ALL_916: &[Rect] = &[(0.0, 0.0, 1080.0, 220.0), (0.0, 1440.0, 1080.0, 1920.0), (930.0, 700.0, 1080.0, 1540.0)];
/// `youtube`, measured against the `16:9` frame (1920x1080 units): the
/// title/progress bar at top, the controls bar at bottom.
///
/// Also an approximation of the platform's UI as of 2026-09 (see
/// [`REELS_916`]'s doc comment).
const YOUTUBE_169: &[Rect] = &[(0.0, 0.0, 1920.0, 100.0), (0.0, 960.0, 1920.0, 1080.0)];

/// `4:5`'s frame (1080x1350) sits inside the `9:16` frame (1080x1920)
/// centred vertically, so its rectangles are the `9:16` ones shifted up
/// by this and clipped to the shorter frame (spec §2.4).
const FEED_SHIFT_UP: f32 = (1920.0 - 1350.0) / 2.0;

/// Shifts every rectangle's `top`/`bottom` up by `shift` and clips the
/// result to `(0, 0, frame_w, frame_h)`, dropping any rectangle that
/// ends up empty (fully off-frame). Used to turn a `9:16` safe-area
/// table into `4:5`'s.
fn shift_and_clip(rects: &[Rect], shift: f32, frame: (f32, f32)) -> Vec<Rect> {
    rects
        .iter()
        .filter_map(|&(l, t, r, b)| {
            let (l, r) = (l.max(0.0), r.min(frame.0));
            let (t, b) = ((t - shift).max(0.0), (b - shift).min(frame.1));
            if r > l && b > t {
                Some((l, t, r, b))
            } else {
                None
            }
        })
        .collect()
}

/// The rectangles `area`'s UI covers on a frame of `aspect`, in
/// 1080-referenced units. `none`, and every combination the app's
/// table was never measured for -- `1:1` or `16:9` with a portrait
/// app, `9:16`/`4:5` with `youtube` -- return no rectangles (the
/// caller reports `safe_area_mismatch`, not this function: it is pure
/// and carries no diagnostics).
pub fn safe_area_rects(area: SafeArea, aspect: Aspect) -> Vec<Rect> {
    match area {
        SafeArea::None => Vec::new(),
        SafeArea::Reels | SafeArea::Tiktok | SafeArea::Shorts | SafeArea::All => {
            let table = match area {
                SafeArea::Reels => REELS_916,
                SafeArea::Tiktok => TIKTOK_916,
                SafeArea::Shorts => SHORTS_916,
                SafeArea::All => ALL_916,
                _ => unreachable!("matched above"),
            };
            match aspect {
                Aspect::Portrait => table.to_vec(),
                Aspect::Feed => shift_and_clip(table, FEED_SHIFT_UP, (1080.0, 1350.0)),
                Aspect::Landscape | Aspect::Square => Vec::new(),
            }
        }
        SafeArea::Youtube => match aspect {
            Aspect::Landscape => YOUTUBE_169.to_vec(),
            Aspect::Portrait | Aspect::Feed | Aspect::Square => Vec::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_size_table_is_the_spec_table() {
        use Aspect::*;
        use Tier::*;
        let want = [
            (Hd, [(1280, 720), (720, 1280), (720, 900), (720, 720)]),
            (Fullhd, [(1920, 1080), (1080, 1920), (1080, 1350), (1080, 1080)]),
            (Qhd, [(2560, 1440), (1440, 2560), (1440, 1800), (1440, 1440)]),
            (Uhd, [(3840, 2160), (2160, 3840), (2160, 2700), (2160, 2160)]),
        ];
        for (tier, sizes) in want {
            for (aspect, size) in [Landscape, Portrait, Feed, Square].into_iter().zip(sizes) {
                assert_eq!(frame_size(aspect, tier), size, "{aspect:?} {tier:?}");
            }
        }
        assert_eq!(Uhd.entitlement(), Entitlement::Pro);
        assert_eq!(Fullhd.entitlement(), Entitlement::Free);
        assert!((pixel_ratio(Hd) - 720.0 / 1080.0).abs() < 1e-6);
        assert_eq!(pixel_ratio(Uhd), 2.0);
    }

    /// D1's new `all` value (the strictest union, spec §5.1) has rects on
    /// 4:5 too (R10 Task 2 minor): 1a only shifted and clipped `reels` on
    /// a feed frame; `all` takes the same `shift_and_clip` path.
    #[test]
    fn safe_area_all_has_rects_on_4x5() {
        let rects = safe_area_rects(SafeArea::All, Aspect::Feed);
        assert!(!rects.is_empty());
        for (l, t, r, b) in &rects {
            assert!(l < r && t < b && *b <= 1350.0, "{rects:?}");
        }
    }

    #[test]
    fn every_tier_has_the_same_layout_frame_in_1080_units() {
        for aspect in [Aspect::Landscape, Aspect::Portrait, Aspect::Feed, Aspect::Square] {
            let (w0, h0) = frame_size(aspect, Tier::Fullhd);
            for tier in [Tier::Hd, Tier::Qhd, Tier::Uhd] {
                let (w, h) = frame_size(aspect, tier);
                let r = pixel_ratio(tier);
                assert!((w as f32 / r - w0 as f32).abs() < 0.5 && (h as f32 / r - h0 as f32).abs() < 0.5, "{aspect:?} {tier:?}");
            }
        }
    }

    #[test]
    fn feed_safe_areas_are_the_portrait_ones_shifted_and_clipped() {
        let feed = safe_area_rects(SafeArea::Reels, Aspect::Feed);
        assert!(feed.iter().all(|r| r.0 >= 0.0 && r.1 >= 0.0 && r.2 <= 1080.0 && r.3 <= 1350.0 && r.2 > r.0 && r.3 > r.1));
        assert!(feed.iter().any(|r| (r.1 - (1500.0 - 285.0)).abs() < 1e-3), "{feed:?}");
        assert_eq!(default_safe_area(Aspect::Portrait), SafeArea::Reels);
        assert_eq!(default_safe_area(Aspect::Square), SafeArea::None);
        assert!(safe_area_rects(SafeArea::None, Aspect::Portrait).is_empty());
    }

    #[test]
    fn all_is_the_union_of_tiktok_reels_and_shorts() {
        let r = safe_area_rects(SafeArea::All, Aspect::Portrait);
        let top = r.iter().filter(|x| x.1 == 0.0).map(|x| x.3).fold(0.0, f32::max);
        let bottom = r.iter().filter(|x| x.3 >= 1920.0).map(|x| x.1).fold(f32::MAX, f32::min);
        assert_eq!((top, bottom), (220.0, 1440.0));
        assert!(r.contains(&(930.0, 700.0, 1080.0, 1540.0)));
    }

    #[test]
    fn mismatched_aspect_safe_area_pairs_get_no_rectangles() {
        assert!(safe_area_rects(SafeArea::Reels, Aspect::Landscape).is_empty());
        assert!(safe_area_rects(SafeArea::Tiktok, Aspect::Square).is_empty());
        assert!(safe_area_rects(SafeArea::Youtube, Aspect::Portrait).is_empty());
        assert!(!safe_area_rects(SafeArea::Youtube, Aspect::Landscape).is_empty());
    }

    #[test]
    fn tier_for_short_edge_picks_the_nearest_tier_at_or_above() {
        assert_eq!(tier_for_short_edge(720), Tier::Hd);
        assert_eq!(tier_for_short_edge(1), Tier::Hd);
        assert_eq!(tier_for_short_edge(721), Tier::Fullhd);
        assert_eq!(tier_for_short_edge(1080), Tier::Fullhd);
        assert_eq!(tier_for_short_edge(1081), Tier::Qhd);
        assert_eq!(tier_for_short_edge(1440), Tier::Qhd);
        assert_eq!(tier_for_short_edge(1441), Tier::Uhd);
        assert_eq!(tier_for_short_edge(2160), Tier::Uhd);
        assert_eq!(tier_for_short_edge(1_000_000), Tier::Uhd);
    }

    #[test]
    fn levels_follow_the_table() {
        assert_eq!(h264_level(3840, 2160, 30), "5.1");
        assert_eq!(h264_level(1080, 1920, 30), "4.0");
        assert_eq!(h264_level(1280, 720, 60), "3.2");
        assert_eq!(h264_level(3840, 2160, 60), "5.2");
        assert_eq!(h264_level(2160, 3840, 30), "5.1", "portrait 4K takes the landscape level");
        assert_eq!(h264_level(2560, 1440, 30), "5.0");
        assert_eq!(h264_level(1920, 1080, 60), "4.2");
        assert_eq!(h264_level(7680, 4320, 30), "6.0");
        assert_eq!(h264_level(1920, 1080, 120), "5.1");
    }

    /// Each level the table names admits its frame: H.264 Annex A's
    /// MaxFS (frame size in macroblocks), its sqrt(8 * MaxFS) bound on
    /// each side, and MaxMBPS (macroblocks per second), for every tier,
    /// aspect and rate a story can ask for.
    #[test]
    fn every_level_admits_its_frame_by_annex_a() {
        let limits = |level: &str| -> (u64, u64) {
            match level {
                "3.1" => (3_600, 108_000),
                "3.2" => (5_120, 216_000),
                "4.0" => (8_192, 245_760),
                "4.2" => (8_704, 522_240),
                "5.0" => (22_080, 589_824),
                "5.1" => (36_864, 983_040),
                "5.2" => (36_864, 2_073_600),
                "6.0" => (139_264, 4_177_920),
                "6.1" => (139_264, 8_355_840),
                "6.2" => (139_264, 16_711_680),
                other => panic!("unknown level {other}"),
            }
        };
        let mut sizes: Vec<(u32, u32)> = Vec::new();
        for tier in [Tier::Hd, Tier::Fullhd, Tier::Qhd, Tier::Uhd] {
            for aspect in [Aspect::Landscape, Aspect::Portrait, Aspect::Feed, Aspect::Square] {
                sizes.push(frame_size(aspect, tier));
            }
        }
        sizes.push((7680, 4320));
        for (w, h) in sizes {
            for fps in [24, 30, 60, 120] {
                let level = h264_level(w, h, fps);
                let (max_fs, max_mbps) = limits(level);
                let (mbw, mbh) = (u64::from(w.div_ceil(16)), u64::from(h.div_ceil(16)));
                let side = ((8 * max_fs) as f64).sqrt() as u64;
                assert!(mbw * mbh <= max_fs && mbw <= side && mbh <= side, "{w}x{h}@{fps}: level {level} frame size");
                assert!(mbw * mbh * u64::from(fps) <= max_mbps, "{w}x{h}@{fps}: level {level} macroblock rate");
            }
        }
    }

    #[test]
    fn one_bitrate_table_per_tier() {
        assert_eq!(bitrate_kbps(Tier::Hd, 30), 5_000);
        assert_eq!(bitrate_kbps(Tier::Fullhd, 30), 10_000);
        assert_eq!(bitrate_kbps(Tier::Qhd, 24), 16_000);
        assert_eq!(bitrate_kbps(Tier::Uhd, 30), 35_000);
        assert_eq!(bitrate_kbps(Tier::Uhd, 60), 52_500);
        assert_eq!(bitrate_kbps(tier_for_short_edge(1080), 30), 10_000, "1080x1920 is fullhd's rate");
    }

    #[test]
    fn two_x_rasters_only_above_1_5() {
        assert!(!use_2x_rasters(pixel_ratio(Tier::Qhd), true));
        assert!(!use_2x_rasters(1.3333, true));
        assert!(use_2x_rasters(2.0, true));
        assert!(!use_2x_rasters(2.0, false));
        assert!(!use_2x_rasters(1.0, true));
    }

    #[test]
    fn a_raster_template_offers_2x_by_placeholder_literal_or_tilejson_scale() {
        let t = "https://tiles.example.com/{z}/{x}/{y}{r}.png";
        assert!(raster_offers_2x(t, None));
        assert!(raster_offers_2x("https://a/{z}/{x}/{y}{ratio}.png", None));
        assert!(raster_offers_2x("https://a/{z}/{x}/{y}@2x.png", None));
        assert!(!raster_offers_2x("https://a/{z}/{x}/{y}.png", None));
        assert!(raster_offers_2x("https://a/{z}/{x}/{y}.png", Some(2.0)));
        assert!(!raster_offers_2x("https://a/{z}/{x}/{y}.png", Some(1.0)));
        assert_eq!(raster_template(t, true), "https://tiles.example.com/{z}/{x}/{y}@2x.png");
        assert_eq!(raster_template(t, false), "https://tiles.example.com/{z}/{x}/{y}.png");
        assert_eq!(raster_template("https://a/{z}/{x}/{y}{ratio}.png", true), "https://a/{z}/{x}/{y}@2x.png");
        assert_eq!(raster_template("https://a/{z}/{x}/{y}@2x.png", false), "https://a/{z}/{x}/{y}@2x.png");
        assert_eq!(resolve_raster(t, 2.0, None), ("https://tiles.example.com/{z}/{x}/{y}@2x.png".to_string(), true));
        assert_eq!(resolve_raster(t, 1.0, None), ("https://tiles.example.com/{z}/{x}/{y}.png".to_string(), false));
        assert!(!resolve_raster("https://a/{z}/{x}/{y}.png", 2.0, None).1, "1x, upscaled");
        assert!(resolve_raster("https://a/{z}/{x}/{y}@2x.png", 1.0, None).1, "a literal @2x template draws @2x tiles");
    }
}

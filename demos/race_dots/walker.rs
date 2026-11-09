//! Kyle Walker's 2020 race and ethnicity map, as the numbers this demo
//! copies from it (spec §1, fetched 2026-09-27 from
//! <https://walker-data.com/maps/census-2020/race.html>): the dots' style,
//! the zoom range, the start camera, the zooms the status line and the
//! legend switch at, and the 8 groups.

use gpui_maps::overlay::dotgrid::layer::{DotStyle, Platform, RACE_PALETTE};
use gpui_maps::style::Rgba;

use super::panel::Mode;

/// `circle-radius`: `interpolate linear zoom 3 -> 0.55, 7 -> 0.8, 12 -> 1.6,
/// 14 -> 2.4` px.
pub const DOT_RADIUS_STOPS: [(f64, f32); 4] = [(3.0, 0.55), (7.0, 0.8), (12.0, 1.6), (14.0, 2.4)];

/// `circle-opacity`.
pub const DOT_OPACITY: f32 = 0.8;

/// The map's `minZoom` / `maxZoom`.
pub const MIN_ZOOM: f64 = 3.0;
pub const MAX_ZOOM: f64 = 14.0;

/// The map's start: centre (-98, 39), z3.5.
pub const START: (f64, f64, f64) = (-98.0, 39.0, 3.5);

/// The dots' status reads "One dot per person" from this zoom.
pub const ONE_DOT_PER_PERSON_ZOOM: f64 = 13.0;

/// Hexagon mode hands the legend and status over to the dots here
/// (`legendSwitch`).
pub const LEGEND_SWITCH_HEX: f64 = 12.375;

/// One [`HEX_WINDOWS`] entry: resolution, min zoom, max zoom and the
/// opacity stops.
pub type HexWindow = (u8, f64, f64, [(f64, f32); 4]);

/// Walker's hexagon layers (spec §1.2): per H3 resolution, the layer's zoom
/// range (`minzoom`, `maxzoom`) and its `fill-opacity` stops, `interpolate
/// linear zoom`. Each plateaus at 0.9 and fades linearly over one zoom
/// into its neighbour, so two adjacent resolutions sum to 0.9 through every
/// cross-fade. r4 has no fade-in (it starts at the map's `minZoom` on its
/// plateau), so its first stop is repeated; outside `min..=max` a
/// resolution is not drawn at all.
///
/// **For [`hex_opacity`] only**: the repeated 3.0 is not valid input to a
/// style-spec `interpolate` expression (whose stops must strictly
/// ascend) -- do not port these stops into one as they are.
pub const HEX_WINDOWS: [HexWindow; 5] = [
    (4, 3.0, 5.5, [(3.0, 0.9), (3.0, 0.9), (4.5, 0.9), (5.5, 0.0)]),
    (5, 4.5, 7.5, [(4.5, 0.0), (5.5, 0.9), (6.5, 0.9), (7.5, 0.0)]),
    (6, 6.5, 9.5, [(6.5, 0.0), (7.5, 0.9), (8.5, 0.9), (9.5, 0.0)]),
    (7, 8.5, 11.5, [(8.5, 0.0), (9.5, 0.9), (10.5, 0.9), (11.5, 0.0)]),
    (8, 10.5, 12.75, [(10.5, 0.0), (11.5, 0.9), (12.0, 0.9), (12.75, 0.0)]),
];

/// In hexagon mode the dots fade in underneath the r8 hexagons (`dotFade`):
/// opacity 0 at z12 to 0.8 (Walker's dot opacity) at z12.75.
pub const DOT_FADE_IN_HEX: [(f64, f32); 2] = [(12.0, 0.0), (12.75, 0.8)];

/// A hexagon whose largest group is shared ("No single largest group
/// (tie)"): `#D9D9D9`.
pub const HEX_TIE_COLOR: Rgba = hex_rgb(0xD9D9D9);

/// A hexagon matching no group (`group_id:_other`): `#999999`. No record
/// the sources emit reaches it (every record has a modal group or a tie);
/// it is the colour match's default arm, as Walker's.
pub const HEX_FALLBACK_COLOR: Rgba = hex_rgb(0x999999);

/// Every hexagon's outline, `rgba(45,62,78,0.45)`.
pub const HEX_OUTLINE: Rgba = Rgba { r: 45.0 / 255.0, g: 62.0 / 255.0, b: 78.0 / 255.0, a: 0.45 };

/// The legend's key for [`HEX_TIE_COLOR`].
pub const HEX_TIE_LABEL: &str = "No single largest group (tie)";

const fn hex_rgb(rgb: u32) -> Rgba {
    Rgba { r: ((rgb >> 16) & 0xff) as f32 / 255.0, g: ((rgb >> 8) & 0xff) as f32 / 255.0, b: (rgb & 0xff) as f32 / 255.0, a: 1.0 }
}

/// `interpolate linear zoom` over `stops` (clamped at both ends).
fn interpolate(stops: &[(f64, f32)], zoom: f64) -> f32 {
    let (first, last) = (stops[0], stops[stops.len() - 1]);
    if zoom <= first.0 {
        return first.1;
    }
    if zoom >= last.0 {
        return last.1;
    }
    for w in stops.windows(2) {
        let ((z0, v0), (z1, v1)) = (w[0], w[1]);
        if zoom <= z1 && z1 > z0 {
            let t = ((zoom - z0) / (z1 - z0)) as f32;
            return v0 + (v1 - v0) * t;
        }
    }
    last.1
}

/// Resolution `res`'s hexagon opacity at `zoom`: its [`HEX_WINDOWS`]
/// stops inside its zoom range, 0 outside it (and for a resolution the map
/// does not draw).
pub fn hex_opacity(res: u8, zoom: f64) -> f32 {
    match HEX_WINDOWS.iter().find(|w| w.0 == res) {
        Some(&(_, min, max, stops)) if (min..=max).contains(&zoom) => interpolate(&stops, zoom),
        _ => 0.0,
    }
}

/// The dot layer's opacity in hexagon mode at `zoom`: [`DOT_FADE_IN_HEX`].
pub fn dot_opacity_in_hex_mode(zoom: f64) -> f32 {
    interpolate(&DOT_FADE_IN_HEX, zoom)
}

/// A popup share, in percent, to 0.1 % -- and "<0.1%" for a share that
/// is there but would round to nothing (Walker's popup).
pub fn percent_label(p: f32) -> String {
    if p > 0.0 && p < 0.1 {
        "<0.1%".to_string()
    } else {
        format!("{p:.1}%")
    }
}

/// Cluster mode's `people` layer starts here (spec §9: dots take over at z12).
pub const CLUSTER_DOTS_FROM: f64 = 12.0;

/// Walker's donut `size` steps on the population (spec §1.2): `(from,
/// px)`, 12 px below 100 people up to 64 px from 50 M. The size is the
/// donut image's width, so the drawn radius is half of it.
pub const DONUT_SIZE_STEPS: [(u32, f32); 11] = [
    (0, 12.),
    (100, 15.),
    (500, 18.),
    (2_500, 22.),
    (10_000, 27.),
    (50_000, 32.),
    (250_000, 37.),
    (1_000_000, 43.),
    (5_000_000, 50.),
    (20_000_000, 58.),
    (50_000_000, 64.),
];

/// A donut's size (its width, logical px) for `pop` people:
/// [`DONUT_SIZE_STEPS`]' last step at or below `pop`.
pub fn donut_size(pop: u32) -> f32 {
    DONUT_SIZE_STEPS.iter().rev().find(|&&(from, _)| pop >= from).map_or(DONUT_SIZE_STEPS[0].1, |s| s.1)
}

/// The count label's colour, `#263443`, and size, 12 px.
pub const COUNT_LABEL_COLOR: &str = "#263443";
pub const COUNT_LABEL_SIZE: f32 = 12.0;

/// A donut's count label in Walker's format (spec §1.2): "950", "1.3k",
/// "12k", "1.2M", "12M" -- rounded half up, a trailing ".0" dropped.
/// Supercluster's own `point_count_abbreviated` has no "M" (it would print
/// "12400k"); this is the map's format, not the library's.
pub fn count_label(n: u32) -> String {
    // `tenths` of `unit`, rounded half up, as "d.d" without a ".0".
    fn tenths(n: u32, unit: u64, suffix: &str) -> String {
        let t = (u64::from(n) * 10 + unit / 2) / unit;
        if t.is_multiple_of(10) { format!("{}{suffix}", t / 10) } else { format!("{}.{}{suffix}", t / 10, t % 10) }
    }
    fn whole(n: u32, unit: u64, suffix: &str) -> String {
        format!("{}{suffix}", (u64::from(n) + unit / 2) / unit)
    }
    match n {
        0..=999 => n.to_string(),
        // 9,950 rounds to "10k", not "10.0k".
        1_000..=9_949 => tenths(n, 1_000, "k"),
        // 999,500 would round to "1000k": that is "1M".
        9_950..=999_499 => whole(n, 1_000, "k"),
        999_500..=9_949_999 => tenths(n, 1_000_000, "M"),
        _ => whole(n, 1_000_000, "M"),
    }
}

/// The donuts' opacity in cluster mode (spec §5.3): 1 up to z11.5, fading
/// linearly to 0 at z12.5 as the dots take over.
pub const CLUSTER_FADE: [(f64, f32); 2] = [(11.5, 1.0), (12.5, 0.0)];

pub fn cluster_alpha(zoom: f64) -> f32 {
    interpolate(&CLUSTER_FADE, zoom)
}

/// In cluster mode the dots draw from z12 and fade in under the fading
/// donuts: 0 at z12 to Walker's 0.8 at z12.5 (spec §5.3).
pub const DOT_FADE_IN_CLUSTERS: [(f64, f32); 2] = [(12.0, 0.0), (12.5, 0.8)];

pub fn dot_opacity_in_cluster_mode(zoom: f64) -> f32 {
    interpolate(&DOT_FADE_IN_CLUSTERS, zoom)
}

/// Walker's `people` layer zoom range's floor per mode
/// (`setLayerZoomRange('people', ..)`): the map's `minZoom` in dot and
/// hexagon mode, [`CLUSTER_DOTS_FROM`] in cluster mode. (Hexagon mode's
/// dots are at opacity 0 below z12 anyway, and the panel culls them there
/// too -- `DOT_FADE_IN_HEX` -- which Walker's layer range does not.)
pub fn dots_min_zoom(mode: Mode) -> f64 {
    match mode {
        Mode::Dots | Mode::Hexagons => MIN_ZOOM,
        Mode::Clusters => CLUSTER_DOTS_FROM,
        // States mode draws no dots: a floor past the map's max zoom.
        Mode::States => MAX_ZOOM + 1.0,
    }
}

/// One of the 8 groups, in `group_id` order (the archive's and
/// [`RACE_PALETTE`]'s order).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Group {
    /// Walker's legend label.
    pub label: &'static str,
    /// The 2020 P.L. 94-171 P2 US total (50 states and DC): MG-R2 read these
    /// exactly, and every level of the archive sums to them.
    pub people: u64,
}

pub const GROUPS: [Group; 8] = [
    Group { label: "Hispanic or Latino (any race)", people: 62_080_044 },
    Group { label: "White", people: 191_697_647 },
    Group { label: "Black", people: 39_940_338 },
    Group { label: "American Indian / Alaska Native", people: 2_251_699 },
    Group { label: "Asian", people: 19_618_719 },
    Group { label: "Native Hawaiian / Pacific Islander", people: 622_018 },
    Group { label: "Some other race", people: 1_689_833 },
    Group { label: "Two or more races", people: 13_548_983 },
];

/// The 2020 resident population of the 50 states and DC.
pub const US_TOTAL: u64 = 331_449_281;

/// Group `g`'s colour (the Okabe-Ito palette, grey for its black).
pub fn color(g: usize) -> Rgba {
    RACE_PALETTE[g]
}

/// Walker's dots on the native window's budget: the engine's race style
/// (categorical, no relocation, shrink or overlap fade -- C12) with this
/// module's opacity and radii; `budget` overrides the dot budget
/// (`--budget`).
pub fn dot_style(budget: Option<u32>) -> DotStyle {
    let mut style = DotStyle::race(Platform::Native);
    style.opacity = DOT_OPACITY;
    style.radius_stops = Some(DOT_RADIUS_STOPS.to_vec());
    style.min_zoom = MIN_ZOOM;
    if let Some(b) = budget {
        style.budget_dots = b;
    }
    style
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn donut_sizes_follow_walkers_steps() {
        assert_eq!(donut_size(99), 12.0);
        assert_eq!(donut_size(100), 15.0);
        assert_eq!(donut_size(49_999_999), 58.0);
        assert_eq!(donut_size(50_000_000), 64.0);
    }

    #[test]
    fn count_labels_follow_walker() {
        assert_eq!(count_label(950), "950");
        assert_eq!(count_label(1_250), "1.3k");
        assert_eq!(count_label(12_400), "12k");
        assert_eq!(count_label(1_240_000), "1.2M");
        assert_eq!(count_label(12_400_000), "12M");
    }

    #[test]
    fn count_labels_round_across_their_units() {
        assert_eq!(count_label(1_000), "1k");
        assert_eq!(count_label(9_949), "9.9k");
        assert_eq!(count_label(9_950), "10k");
        assert_eq!(count_label(999_499), "999k");
        assert_eq!(count_label(999_500), "1M");
        assert_eq!(count_label(9_950_000), "10M");
        assert_eq!(count_label(u32::MAX), "4295M");
    }

    #[test]
    fn clusters_hand_over_to_dots_at_twelve() {
        assert_eq!(cluster_alpha(11.0), 1.0);
        assert_eq!(cluster_alpha(12.5), 0.0);
        assert_eq!(dots_min_zoom(Mode::Clusters), 12.0);
        assert_eq!(dots_min_zoom(Mode::Dots), 3.0);
    }

    #[test]
    fn the_handover_band_crosses_fades() {
        assert_eq!(cluster_alpha(11.5), 1.0);
        assert_eq!(cluster_alpha(12.0), 0.5);
        assert_eq!(dot_opacity_in_cluster_mode(11.9), 0.0);
        assert_eq!(dot_opacity_in_cluster_mode(12.25), 0.4);
        assert_eq!(dot_opacity_in_cluster_mode(13.0), DOT_OPACITY);
        assert_eq!(dots_min_zoom(Mode::Hexagons), MIN_ZOOM);
    }

    #[test]
    fn hex_opacity_matches_walkers_stops() {
        assert_eq!(hex_opacity(4, 4.0), 0.9);
        assert_eq!(hex_opacity(4, 5.0), 0.45);
        assert_eq!(hex_opacity(5, 5.0), 0.45);
        assert_eq!(hex_opacity(8, 12.375), 0.45);
        assert_eq!(hex_opacity(8, 12.75), 0.0);
    }

    #[test]
    fn adjacent_resolutions_sum_to_the_plateau_in_every_crossfade() {
        for z10 in 45..=115 {
            let z = f64::from(z10) / 10.0;
            let s: f32 = (4..=8).map(|r| hex_opacity(r, z)).sum();
            assert!(s >= 0.9 - 1e-6 || z < 4.5, "z={z} sum={s}");
        }
    }

    /// Review N5: and never more than the plateau, which catches two
    /// windows overlapping where they should hand over.
    #[test]
    fn crossfades_never_exceed_the_plateau() {
        for z100 in 300..=1275 {
            let z = f64::from(z100) / 100.0;
            let s: f32 = (4..=8).map(|r| hex_opacity(r, z)).sum();
            assert!(s <= 0.9 + 1e-6, "z={z} sum={s}");
        }
    }

    #[test]
    fn percent_labels_follow_walker() {
        assert_eq!(percent_label(0.04), "<0.1%");
        assert_eq!(percent_label(0.0), "0.0%");
        assert_eq!(percent_label(12.345), "12.3%");
    }

    #[test]
    fn hex_windows_are_the_spec_table() {
        let windows: Vec<(u8, f64, f64)> = HEX_WINDOWS.iter().map(|w| (w.0, w.1, w.2)).collect();
        assert_eq!(windows, vec![(4, 3.0, 5.5), (5, 4.5, 7.5), (6, 6.5, 9.5), (7, 8.5, 11.5), (8, 10.5, 12.75)]);
        // Outside its window a resolution draws nothing; unknown ones never.
        assert_eq!(hex_opacity(6, 6.4), 0.0);
        assert_eq!(hex_opacity(6, 9.6), 0.0);
        assert_eq!(hex_opacity(4, 3.0), 0.9);
        assert_eq!(hex_opacity(8, 11.75), 0.9);
        assert_eq!(hex_opacity(9, 11.0), 0.0);
        assert_eq!(DOT_FADE_IN_HEX, [(12.0, 0.0), (12.75, 0.8)]);
        assert_eq!(LEGEND_SWITCH_HEX, 12.375);
        assert_eq!(dot_opacity_in_hex_mode(11.9), 0.0);
        assert_eq!(dot_opacity_in_hex_mode(12.375), 0.4);
        assert_eq!(dot_opacity_in_hex_mode(13.0), 0.8);
    }

    #[test]
    fn hex_colours_are_walkers() {
        assert_eq!(HEX_TIE_COLOR, Rgba { r: 217.0 / 255.0, g: 217.0 / 255.0, b: 217.0 / 255.0, a: 1.0 });
        assert_eq!(HEX_FALLBACK_COLOR, Rgba { r: 153.0 / 255.0, g: 153.0 / 255.0, b: 153.0 / 255.0, a: 1.0 });
        assert_eq!(HEX_OUTLINE, Rgba { r: 45.0 / 255.0, g: 62.0 / 255.0, b: 78.0 / 255.0, a: 0.45 });
    }

    #[test]
    fn the_groups_sum_to_the_us_total() {
        assert_eq!(GROUPS.iter().map(|g| g.people).sum::<u64>(), US_TOTAL);
    }

    #[test]
    fn walker_style_matches_the_engines_race_style() {
        let engine = DotStyle::race(Platform::Native);
        let ours = dot_style(None);
        assert_eq!(engine.radius_stops, ours.radius_stops, "walker::DOT_RADIUS_STOPS drifted from layer::RACE_RADIUS_STOPS");
        assert_eq!(engine.opacity, ours.opacity);
        assert_eq!(ours.min_zoom, MIN_ZOOM);
        assert_eq!(dot_style(Some(1000)).budget_dots, 1000);
    }
}

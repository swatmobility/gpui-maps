//! The satellites demo's state and every decision it makes, with no gpui in
//! it: the group chips, the ±24 h scrubber, the card an object shows, the
//! follow camera's zoom cap, what a selection does when its group is hidden,
//! when a ring or a ground track is stale, the staleness chip, the group
//! colours and dot sizes, and the catalogue built from the cached bodies.
//! `panel.rs` only wires these to the map and the pointer.

use std::sync::Arc;

use gpui_maps::map_view::AltitudePointState;
pub use gpui_maps::orbit::catalog::GROUP_COUNT;
use gpui_maps::orbit::catalog::{Catalog, Group, Membership, SatObject, GROUP_COLOURS};
use gpui_maps::orbit::omm::parse_omm;
use gpui_maps::orbit::propagate;
use gpui_maps::render::altitude_pipeline::{AltitudeLook, AltitudeStyle, TrailStyle};
use gpui_maps::render::globe::GLOBE_RADIUS_M;
use gpui_maps::style::Rgba;

/// The speeds the time bar offers, scene seconds per wall second; each is
/// also offered backwards. `0` is pause.
pub const SPEEDS: [f64; 6] = [0.0, 1.0, 10.0, 60.0, 300.0, 1000.0];
/// How far the scrubber reaches either side of now, seconds.
pub const SCRUB_SPAN_S: f64 = 86_400.0;
/// The geostationary shell's radius from the Earth's centre, m ("Show GEO").
pub const GEO_RADIUS_M: f64 = 42_164_000.0;
/// The LEO view's shell radius from the Earth's centre, m ("Show LEO").
pub const LEO_RADIUS_M: f64 = 8_000_000.0;

/// The groups in chip order (indexed elsewhere by `group as usize`): the
/// navigation family -- GPS, Galileo, GLONASS, BeiDou -- together.
pub const GROUPS: [Group; GROUP_COUNT] =
    [Group::Stations, Group::Starlink, Group::Gps, Group::Galileo, Group::Glonass, Group::Beidou, Group::Geo, Group::Other];

/// Base dot diameter per group, logical px, before the R20 depth cue
/// (`clamp(sqrt(|E| / |P - E|), 0.7, 1.5)`):
///
/// - **Starlink 1.6** (spec §2.5): 11k dots in one thin shell; any larger and
///   the shell reads as a solid blob that hides the ground.
/// - **Stations, other 2.0** (spec §2.5).
/// - **GPS 3.0**: 32 objects at 20,200 km. At 2 px they were specks among the
///   "other" MEO dots in the Task 9 showcase and GPS did not read as a shell
///   of its own; 3 px (2.25x the area) makes the six planes' ring of orange
///   dots findable at z -1 while 32 dots cannot clutter anything.
/// - **GEO 2.5**: 566 objects on one ring at 42,164 km, where the far half
///   shrinks by the depth cue's 0.7 floor to ~1.4 px, under the outline's
///   visible size. 2.5 keeps the far arc a dotted line rather than a haze,
///   and the ring is 1-D, so the extra size overlaps neighbours on the ring,
///   not the map.
/// - **Galileo, GLONASS, BeiDou 3.0**: GPS's MEO size, for the same reason
///   (a few dozen objects each, on their own shells at 19-23k km).
pub const GROUP_BASE_PX: [f32; GROUP_COUNT] = [2.0, 1.6, 3.0, 2.5, 2.0, 3.0, 3.0, 3.0];

/// The chip's name for a group.
#[must_use]
pub fn group_name(group: Group) -> &'static str {
    match group {
        Group::Stations => "Stations",
        Group::Starlink => "Starlink",
        Group::Gps => "GPS",
        Group::Geo => "GEO",
        Group::Other => "Other",
        Group::Galileo => "Galileo",
        Group::Glonass => "GLONASS",
        Group::Beidou => "BeiDou",
    }
}

/// The card's longer name for a group.
#[must_use]
pub fn group_long_name(group: Group) -> &'static str {
    match group {
        Group::Stations => "Space stations",
        Group::Starlink => "Starlink",
        Group::Gps => "GPS",
        Group::Geo => "Geostationary",
        Group::Other => "Other active satellites",
        Group::Galileo => "Galileo (EU navigation)",
        Group::Glonass => "GLONASS (Russian navigation)",
        Group::Beidou => "BeiDou (Chinese navigation)",
    }
}

/// A group's colour at alpha `a`.
#[must_use]
pub fn group_rgba(group: Group, a: f32) -> Rgba {
    let c = GROUP_COLOURS[group as usize];
    let ch = |shift: u32| ((c >> shift) & 0xFF) as f32 / 255.0;
    Rgba { r: ch(16), g: ch(8), b: ch(0), a }
}

/// A group's colour as `#rrggbb` (the ground track's GeoJSON).
#[must_use]
pub fn group_hex(group: Group) -> String {
    format!("#{:06x}", GROUP_COLOURS[group as usize])
}

/// The Glow look's colours, `0xRRGGBB` per group: neon on a dark ground.
///
/// - **Stations** hot orange-red;
/// - **Starlink** icy cyan-white, with little red in it on purpose: the
///   screen blend saturates channel by channel, so where 11k halos pile up
///   the shell blooms to a bright cyan and stays cyan rather than white;
/// - **GPS** amber; **GEO** violet-magenta;
/// - **Other** a dim steel blue, so the ~3,000 miscellaneous objects sit
///   behind the named constellations instead of competing with them.
/// - **the navigation family**, related warm-to-cool neighbours of GPS's
///   amber: Galileo blue-cyan, GLONASS red-orange, BeiDou gold-green.
pub const NEON_COLOURS: [u32; GROUP_COUNT] = [0xFF4A26, 0x3CC6FF, 0xFFB02E, 0xD65CFF, 0x5B7FA8, 0x38A8FF, 0xFF6A3D, 0xC8E03A];

/// A group's colour in `look` at alpha `a`: the catalogue's colours for
/// the flat dots, [`NEON_COLOURS`] for the glowing ones.
#[must_use]
pub fn look_rgba(look: AltitudeLook, group: Group, a: f32) -> Rgba {
    let c = match look {
        AltitudeLook::Flat => GROUP_COLOURS[group as usize],
        AltitudeLook::Glow => NEON_COLOURS[group as usize],
    };
    let ch = |shift: u32| ((c >> shift) & 0xFF) as f32 / 255.0;
    Rgba { r: ch(16), g: ch(8), b: ch(0), a }
}

/// A group's colour in `look` as `#rrggbb` (the ground track's GeoJSON).
#[must_use]
pub fn look_hex(look: AltitudeLook, group: Group) -> String {
    let c = look_rgba(look, group, 1.0);
    let b = |v: f32| (v * 255.0).round() as u32;
    format!("#{:06x}", (b(c.r) << 16) | (b(c.g) << 8) | b(c.b))
}

/// Starlink's halo gain in the Glow look: 11k dots in one shell overlap
/// dozens deep at z -0.3, so each halo is faint and the shell still
/// blooms where it is densest (the limb) without burning out.
pub const STARLINK_GLOW_GAIN: f32 = 0.3;

/// The satellites' motion trails (the panel's "Trails" toggle): about
/// 0.1 s of wall-clock motion: at 1000x a LEO object moves ~760 km in it,
/// ~20 px at z 1.2 and ~9 px at z -0.3; at 60x ~46 km, a few px until the
/// camera closes in; nothing at 1x or paused. At most 28 px.
pub const TRAIL: TrailStyle = TrailStyle { wall_s: 0.1, max_px: 28.0, segments: 4, glow_px: 3.0 };

/// The dots' style in the flat look: one paint index per object (its
/// group), the group colours and [`GROUP_BASE_PX`], and the default
/// 60%-black outline. No trails.
#[must_use]
pub fn style_for(catalog: &Catalog) -> AltitudeStyle {
    style_for_look(catalog, AltitudeLook::Flat, false)
}

/// [`style_for_look`] over `earth`. On a dark Earth (Real, Night) the
/// look as asked. On a light one (Streets, Map) a screen-blended glow
/// washes out to white, so the dots draw in [`scene_look`] (Flat: blended
/// "over", trails too) in the saturated [`LIGHT_COLOURS`] with
/// [`LIGHT_OUTLINE`].
#[must_use]
pub fn style_for_earth(catalog: &Catalog, look: AltitudeLook, trails: bool, earth: Earth) -> AltitudeStyle {
    let mut style = style_for_look(catalog, scene_look(look, earth), trails);
    if earth.is_light() {
        for g in GROUPS {
            style.palette[g as usize] = scene_rgba(look, earth, g, 1.0);
        }
        style.outline = LIGHT_OUTLINE;
    }
    style
}

/// The look the dots, rings and trails draw in over `earth`: Flat on a
/// light Earth whatever was asked (see [`style_for_earth`]), else `look`.
#[must_use]
pub fn scene_look(look: AltitudeLook, earth: Earth) -> AltitudeLook {
    if earth.is_light() { AltitudeLook::Flat } else { look }
}

/// Group colours for a light Earth: saturated MID-tones, because the same
/// dot sits on white land, pale-blue water AND black space -- a dark slate
/// vanished against space in the first capture. The dark outline carries
/// the contrast on the globe, the fill's own lightness in space. Starlink a
/// clear blue, "other" a mid slate, so the named constellations lead.
pub const LIGHT_COLOURS: [u32; GROUP_COUNT] = [0xF03A24, 0x2F6BFF, 0xF29A00, 0xB04AF0, 0x7F8EA3, 0x1597C4, 0xE8561E, 0x7DBB12];

/// Owner-class colours for a light Earth, [`COUNTRY_COLOURS`]' hues at the
/// same mid-tones as [`LIGHT_COLOURS`].
pub const COUNTRY_COLOURS_LIGHT: [u32; COUNTRY_COUNT] = [0x2F6BFF, 0xE83030, 0xF29A00, 0x8B5CF6, 0xE8479A, 0x22B455, 0x14A098, 0x7F8EA3];

/// The dots' outline on a light Earth: a thin dark ring, lighter than the
/// flat default's 60% so an 11k Starlink shell does not go grey.
pub const LIGHT_OUTLINE: Rgba = Rgba { r: 0.04, g: 0.06, b: 0.10, a: 0.45 };

fn hex_rgba(v: u32, a: f32) -> Rgba {
    let ch = |shift: u32| ((v >> shift) & 0xFF) as f32 / 255.0;
    Rgba { r: ch(16), g: ch(8), b: ch(0), a }
}

/// A group's colour as the scene draws it over `earth`: [`LIGHT_COLOURS`]
/// on a light Earth, else [`look_rgba`].
#[must_use]
pub fn scene_rgba(look: AltitudeLook, earth: Earth, group: Group, a: f32) -> Rgba {
    if earth.is_light() { hex_rgba(LIGHT_COLOURS[group as usize], a) } else { look_rgba(look, group, a) }
}

/// [`scene_rgba`] as `#rrggbb` (the ground track).
#[must_use]
pub fn scene_hex(look: AltitudeLook, earth: Earth, group: Group) -> String {
    let c = scene_rgba(look, earth, group, 1.0);
    let b = |v: f32| (v * 255.0).round() as u32;
    format!("#{:06x}", (b(c.r) << 16) | (b(c.g) << 8) | b(c.b))
}

/// An owner class's colour as the scene draws it over `earth`.
#[must_use]
pub fn country_scene_rgba(c: Country, earth: Earth, a: f32) -> Rgba {
    if earth.is_light() { hex_rgba(COUNTRY_COLOURS_LIGHT[c as usize], a) } else { country_rgba(c, a) }
}

/// The dots' style in `look`, with [`TRAIL`] when `trails`.
#[must_use]
pub fn style_for_look(catalog: &Catalog, look: AltitudeLook, trails: bool) -> AltitudeStyle {
    let mut style = AltitudeStyle { paint: catalog.objects.iter().map(|o| o.group as u8).collect(), look, ..AltitudeStyle::default() };
    for g in GROUPS {
        style.palette[g as usize] = look_rgba(look, g, 1.0);
        style.base_px[g as usize] = GROUP_BASE_PX[g as usize];
    }
    style.trail = trails.then_some(TRAIL);
    style.glow_gain[Group::Starlink as usize] = STARLINK_GLOW_GAIN;
    style
}

/// The ground track's GeoJSON in `look`. Flat: one line
/// (`propagate::ground_track_geojson`). Glow: the same path three times,
/// wide and faint, narrower, then a 1.5 px core lightened toward white --
/// a glow the overlay's "over" blend can draw on any ground (the track is
/// a map overlay, under the altitude layer's screen-blended pass).
#[must_use]
pub fn track_geojson(look: AltitudeLook, pieces: &[Vec<(f64, f64)>], colour_hex: &str) -> String {
    if look == AltitudeLook::Flat {
        return propagate::ground_track_geojson(pieces, colour_hex);
    }
    let coordinates: Vec<Vec<[f64; 2]>> = pieces.iter().map(|p| p.iter().map(|&(lon, lat)| [lon, lat]).collect()).collect();
    let core = lighten_hex(colour_hex, 0.55);
    let feature = |colour: &str, width: f64, opacity: f64| {
        serde_json::json!({
            "type": "Feature",
            "properties": { "kind": "ground-track", "color": colour, "line-color": colour, "line-width": width, "line-opacity": opacity },
            "geometry": { "type": "MultiLineString", "coordinates": coordinates },
        })
    };
    serde_json::json!({
        "type": "FeatureCollection",
        "features": [feature(colour_hex, 8.0, 0.16), feature(colour_hex, 4.0, 0.32), feature(&core, 1.5, 1.0)],
    })
    .to_string()
}

/// `#rrggbb` moved `t` of the way to white (`t` clamped to 0..=1); an
/// unparsable colour comes back as it was.
#[must_use]
pub fn lighten_hex(hex: &str, t: f64) -> String {
    let Some(v) = hex.strip_prefix('#').filter(|h| h.len() == 6).and_then(|h| u32::from_str_radix(h, 16).ok()) else {
        return hex.to_string();
    };
    let t = t.clamp(0.0, 1.0);
    let ch = |shift: u32| {
        let c = f64::from((v >> shift) & 0xFF);
        (c + (255.0 - c) * t).round() as u32
    };
    format!("#{:06x}", (ch(16) << 16) | (ch(8) << 8) | ch(0))
}

/// A basemap style (MapLibre JSON) with its lightness turned over for the
/// Glow look: the glow is screen-blended and cannot show over a white
/// globe, so the satellites panel draws a night-dark copy of its basemap.
/// Every colour string in every layer's `paint` (inside expressions too)
/// keeps its hue, saturation and alpha; lightness `L` becomes
/// `0.08 + 0.72 (1 - L)^1.15` -- white land to near-black, black text to
/// light grey -- and water layers (`id` containing "water" or "ocean") are
/// taken a further 60% darker, so the sea reads below the land as in a
/// night map. `None` for bytes that are not a style object. Skipped, so
/// left as they were: named CSS colours (`"white"`), anything outside a
/// layer's `paint` (the style's `fog`/`sky`/`light`, `layout`), and the
/// sprite sheet's baked icon colours.
#[must_use]
pub fn darken_style_json(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut style: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    for layer in style.get_mut("layers")?.as_array_mut()? {
        let id = layer.get("id").and_then(|v| v.as_str()).unwrap_or("").to_ascii_lowercase();
        let water = id.contains("water") || id.contains("ocean");
        if let Some(paint) = layer.get_mut("paint") {
            darken_colours(paint, water);
        }
    }
    serde_json::to_vec(&style).ok()
}

fn darken_colours(v: &mut serde_json::Value, water: bool) {
    map_colours(v, &|c| night_colour(c, water));
}

/// Every literal colour string inside `v` (expressions included) through `f`.
fn map_colours(v: &mut serde_json::Value, f: &dyn Fn(Rgba) -> String) {
    match v {
        serde_json::Value::String(text) => {
            if let Some(c) = gpui_maps::style_spec::color::parse_css_color(text).ok().filter(|_| looks_like_colour(text)) {
                *text = f(c);
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(|x| map_colours(x, f)),
        serde_json::Value::Object(map) => map.values_mut().for_each(|x| map_colours(x, f)),
        _ => {}
    }
}

/// Only literal colours: `#…`, `rgb(…)`, `rgba(…)`, `hsl(…)`, `hsla(…)` --
/// never a property name or a font that a CSS parser might also accept.
fn looks_like_colour(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with('#') || t.starts_with("rgb") || t.starts_with("hsl")
}

/// [`darken_style_json`]'s lightness map, as `rgba(r, g, b, a)`.
#[must_use]
pub fn night_colour(c: Rgba, water: bool) -> String {
    let (r, g, b) = (f64::from(c.r), f64::from(c.g), f64::from(c.b));
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let l = 0.5 * (max + min);
    let d = max - min;
    let sat = if d <= 1e-9 { 0.0 } else { d / (1.0 - (2.0 * l - 1.0).abs()).max(1e-9) };
    let hue = if d <= 1e-9 {
        0.0
    } else if max == r {
        60.0 * ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    let mut l2 = 0.08 + 0.72 * (1.0 - l).max(0.0).powf(1.15);
    if water {
        l2 *= 0.4;
    }
    let c2 = (1.0 - (2.0 * l2 - 1.0).abs()) * sat.min(1.0);
    let x = c2 * (1.0 - ((hue / 60.0).rem_euclid(2.0) - 1.0).abs());
    let (r1, g1, b1) = match (hue / 60.0) as u32 {
        0 => (c2, x, 0.0),
        1 => (x, c2, 0.0),
        2 => (0.0, c2, x),
        3 => (0.0, x, c2),
        4 => (x, 0.0, c2),
        _ => (c2, 0.0, x),
    };
    let m = l2 - c2 / 2.0;
    let q = |v: f64| ((v + m).clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("rgba({}, {}, {}, {})", q(r1), q(g1), q(b1), c.a)
}

/// What the planet under the satellites looks like (the panel's "Earth"
/// button, `SatellitesPanel::set_earth`; a gallery-wide map-options
/// control can drive the same setter).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Earth {
    /// A street map at its own brightness: Mapbox Streets with a token,
    /// else OpenFreeMap Liberty. A light Earth: the dots draw "over"
    /// ([`style_for_earth`]) and the panel takes its light theme.
    Streets,
    /// Natural Earth II shaded relief imagery (public domain), dimmed to
    /// [`REAL_EARTH_OPACITY`] over near-black, with the basemap's country
    /// borders drawn faintly on top ([`real_earth_style_json`]).
    Real,
    /// The vector basemap as it is.
    Map,
    /// The vector basemap in night colours ([`darken_style_json`]).
    Night,
}

impl Earth {
    /// The button's cycle order.
    pub const ALL: [Earth; 4] = [Earth::Real, Earth::Streets, Earth::Map, Earth::Night];

    /// The button's text.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Earth::Real => "Earth: real",
            Earth::Streets => "Earth: streets",
            Earth::Map => "Earth: map",
            Earth::Night => "Earth: night",
        }
    }

    /// Streets and Map: a light globe, a light panel, dots blended "over".
    #[must_use]
    pub fn is_light(self) -> bool {
        matches!(self, Earth::Streets | Earth::Map)
    }

    /// The panel's colours over this Earth.
    #[must_use]
    pub fn theme(self) -> Theme {
        if self.is_light() { Theme::LIGHT } else { Theme::DARK }
    }

    /// The next in [`Self::ALL`], wrapping.
    #[must_use]
    pub fn next(self) -> Self {
        let i = Self::ALL.iter().position(|&e| e == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }
}

/// The panel's colour tokens: every card, chip, chart, status pill and the
/// time bar reads its colours here, never ad hoc. Two sets: the dark
/// "instrument" look over the dark Earths (Real, Night) and a light one
/// over the light Earths (Streets, Map) -- [`Earth::theme`]. Colours are
/// `0xRRGGBB`, or `0xRRGGBBAA` where the name says `_a`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    pub dark: bool,
    /// Card fill (with alpha: the globe shows faintly through).
    pub card_a: u32,
    /// Primary text.
    pub ink: u32,
    /// Secondary text (counts, axes, card lines).
    pub muted: u32,
    /// The small-caps section labels.
    pub label: u32,
    /// A deselected chip's text.
    pub off: u32,
    /// Links, the active preset, the brush band.
    pub accent: u32,
    /// Text on an accent fill.
    pub on_accent: u32,
    /// Hairlines and a deselected chip's border.
    pub rule_a: u32,
    /// A chip's hover border.
    pub hover_a: u32,
    /// The swatch of the dimension the dots are NOT coloured by.
    pub neutral: u32,
    /// The histogram's brush handles.
    pub handle_a: u32,
    /// An active button's fill and text.
    pub button_on: u32,
    pub button_on_ink: u32,
    /// The scrubber track and its thumb.
    pub track: u32,
    pub thumb: u32,
    /// Warnings (stale elements) and their pill.
    pub warn: u32,
    pub warn_bg_a: u32,
    /// The SYNTHETIC pill.
    pub synthetic_bg_a: u32,
    pub synthetic_ink: u32,
}

impl Theme {
    /// Over Real and Night: dark glass, pale text, a sky-blue accent.
    pub const DARK: Theme = Theme {
        dark: true,
        card_a: 0x0b12_20e6,
        ink: 0xe2e8f0,
        muted: 0x94a3b8,
        label: 0x7f8ea6,
        off: 0x64748b,
        accent: 0x9fd8ff,
        on_accent: 0x0b1220,
        rule_a: 0xffff_ff1a,
        hover_a: 0xffff_ff4d,
        neutral: 0xb4c0d0,
        handle_a: 0xf2f7_ffe6,
        button_on: 0x9fd8ff,
        button_on_ink: 0x0b1220,
        track: 0x334155,
        thumb: 0xe2e8f0,
        warn: 0xfca5a5,
        warn_bg_a: 0x7f1d_1dcc,
        synthetic_bg_a: 0x78350fe6,
        synthetic_ink: 0xfde68a,
    };
    /// Over Streets and Map: white glass, slate text, a deep-blue accent.
    pub const LIGHT: Theme = Theme {
        dark: false,
        card_a: 0xffff_fff0,
        ink: 0x1f2937,
        muted: 0x4b5563,
        label: 0x6b7280,
        // >= 4.5:1 on white (card_a); 0x9ca3af was ~2.5:1, unreadable in
        // daylight.
        off: 0x6b7280,
        accent: 0x1d4ed8,
        on_accent: 0xffffff,
        rule_a: 0x0f17_2a1f,
        hover_a: 0x0f17_2a59,
        neutral: 0x6b7280,
        handle_a: 0x1f29_37e6,
        button_on: 0x374151,
        button_on_ink: 0xffffff,
        track: 0xc4c8cf,
        thumb: 0x374151,
        warn: 0x991b1b,
        warn_bg_a: 0xfee2_e2f0,
        synthetic_bg_a: 0xfde68aff,
        synthetic_ink: 0x78350f,
    };
}

/// Natural Earth II shaded relief, served by OpenFreeMap (public domain;
/// the same tiles as `basic_map`'s S key), 512 px, to z6.
pub const NATURAL_EARTH_URL: &str = "https://tiles.openfreemap.org/natural_earth/ne2sr/{z}/{x}/{y}.png";
/// Its on-map credit.
pub const NATURAL_EARTH_ATTRIBUTION: &str = r#"<a href="https://www.naturalearthdata.com/" target="_blank">Natural Earth</a>"#;
/// The imagery's `raster-opacity` over the near-black background. The
/// relief's oceans are a flat pale grey-blue -- the light ground a
/// screen-blended glow cannot show on (the rejected light-basemap
/// capture), and a grey ball rather than an Earth -- so the basemap's own
/// water polygons are laid over them in ocean blue ([`REAL_EARTH_WATER`]),
/// and the land keeps 60% of its relief.
pub const REAL_EARTH_OPACITY: f32 = 0.6;
/// The sea over the imagery: the basemap's water fills, recoloured a
/// bright ocean blue (#2f6fa8 at 0.85; the user found #1f4f80 "very
/// dark"): a lit sea, still dark enough that the screen-blended glow shows
/// on it.
pub const REAL_EARTH_WATER: &str = "rgba(47, 111, 168, 0.85)";
/// The background the dimmed imagery is laid over.
pub const REAL_EARTH_BACKGROUND: &str = "#03060a";

/// The "Earth: real" style, built from the basemap's own style JSON: its
/// background turned near-black, a `raster` layer for the imagery
/// (the caller gives `MapView::set_raster_source` the tiles) at
/// [`REAL_EARTH_OPACITY`], its water fills recoloured ocean blue over the
/// imagery's pale sea, and its country and state borders (line layers
/// whose id names a boundary or admin line) kept, recoloured a faint
/// white. Every other layer -- land fills, roads, labels -- is dropped:
/// the imagery is the land. `None` for bytes that are not a style.
#[must_use]
pub fn real_earth_style_json(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut style: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let layers = style.get("layers")?.as_array()?.clone();
    let mut out = vec![
        serde_json::json!({"id": "background", "type": "background", "paint": {"background-color": REAL_EARTH_BACKGROUND}}),
        serde_json::json!({"id": "earth", "type": "raster", "source": "earth", "paint": {"raster-opacity": REAL_EARTH_OPACITY}}),
    ];
    for mut layer in layers {
        let id = layer.get("id").and_then(|v| v.as_str()).unwrap_or("").to_ascii_lowercase();
        let kind = layer.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if kind == "fill" && (id == "water" || id.starts_with("water")) {
            if let Some(paint) = layer.get_mut("paint").and_then(|p| p.as_object_mut()) {
                paint.clear();
                paint.insert("fill-color".into(), serde_json::json!(REAL_EARTH_WATER));
            }
            out.push(layer);
            continue;
        }
        if !(kind == "line" && (id.contains("boundary") || id.contains("admin"))) {
            continue;
        }
        if let Some(paint) = layer.get_mut("paint").and_then(|p| p.as_object_mut()) {
            paint.insert("line-color".into(), serde_json::json!("rgba(235, 240, 255, 0.35)"));
            paint.remove("line-opacity");
        }
        out.push(layer);
    }
    style["layers"] = serde_json::Value::Array(out);
    style.get_mut("sources")?.as_object_mut()?.insert(
        "earth".into(),
        serde_json::json!({"type": "raster", "tiles": [], "tileSize": 512, "maxzoom": 6}),
    );
    serde_json::to_vec(&style).ok()
}

/// The owner classes colour-by-country draws: the big launching states,
/// the EU as one, everything else together.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Country {
    Us = 0,
    China = 1,
    Russia = 2,
    Eu = 3,
    Japan = 4,
    India = 5,
    Uk = 6,
    Other = 7,
}

/// How many [`Country`] classes there are.
pub const COUNTRY_COUNT: usize = 8;

/// The classes in chip order (indexed elsewhere by `country as usize`).
pub const COUNTRIES: [Country; COUNTRY_COUNT] =
    [Country::Us, Country::China, Country::Russia, Country::Eu, Country::Japan, Country::India, Country::Uk, Country::Other];

/// Country colours for the glow: eight well-separated hues that each keep
/// some brightness in two channels, so none sinks into the dark sea or
/// burns to white. US sky blue, China red, Russia amber, EU violet, Japan
/// pink, India green, UK teal, other steel.
pub const COUNTRY_COLOURS: [u32; COUNTRY_COUNT] = [0x4FB4FF, 0xFF4040, 0xFFB030, 0xA67BFF, 0xFF7AC8, 0x5EE06A, 0x3FE0D0, 0x6F86A6];

/// A class's chip name.
#[must_use]
pub fn country_name(c: Country) -> &'static str {
    match c {
        Country::Us => "US",
        Country::China => "China",
        Country::Russia => "Russia",
        Country::Eu => "EU",
        Country::Japan => "Japan",
        Country::India => "India",
        Country::Uk => "UK",
        Country::Other => "Other",
    }
}

/// CelesTrak SATCAT `OWNER` codes and what they name, for the card --
/// every code here is on celestrak.org/satcat/sources.php (checked against
/// `tests/fixtures/orbit/satcat_sources.tsv`, fetched 2026-09-29). The
/// classes ([`country_of_owner`]): `US`; `PRC` China; `CIS` Russia (the
/// code CelesTrak keeps for the USSR/Russia); `JPN`; `IND`; `UK`; and the
/// **EU as one class**: ESA (and its forerunner ESRO), EUMETSAT, the 27
/// member states' own codes, CelesTrak's joint codes of two members
/// (France/Germany, France/Italy), and operators domiciled there --
/// Eutelsat (France), SES and its O3b fleet (Luxembourg). The UK is its
/// own class. Latvia, Slovakia, Malta and Cyprus have no code of their
/// own on the list. China/ESA joint objects (`PRES`), anything else, and a
/// missing owner are Other.
pub const OWNERS: &[(&str, &str, Country)] = &[
    ("US", "United States", Country::Us),
    ("PRC", "People's Republic of China", Country::China),
    ("CIS", "Russia (CIS / former USSR)", Country::Russia),
    ("JPN", "Japan", Country::Japan),
    ("IND", "India", Country::India),
    ("UK", "United Kingdom", Country::Uk),
    ("ESA", "European Space Agency", Country::Eu),
    ("ESRO", "European Space Research Organisation", Country::Eu),
    ("EUME", "EUMETSAT", Country::Eu),
    ("EUTE", "Eutelsat (France)", Country::Eu),
    ("SES", "SES (Luxembourg)", Country::Eu),
    ("O3B", "O3b Networks (SES)", Country::Eu),
    ("PRES", "China / European Space Agency", Country::Other),
    ("FR", "France", Country::Eu),
    ("FGER", "France / Germany", Country::Eu),
    ("FRIT", "France / Italy", Country::Eu),
    ("GER", "Germany", Country::Eu),
    ("IT", "Italy", Country::Eu),
    ("SPN", "Spain", Country::Eu),
    ("NETH", "Netherlands", Country::Eu),
    ("BEL", "Belgium", Country::Eu),
    ("SWED", "Sweden", Country::Eu),
    ("POL", "Poland", Country::Eu),
    ("ASRA", "Austria", Country::Eu),
    ("CZCH", "Czechia", Country::Eu),
    ("DEN", "Denmark", Country::Eu),
    ("EST", "Estonia", Country::Eu),
    ("FIN", "Finland", Country::Eu),
    ("GREC", "Greece", Country::Eu),
    ("HUN", "Hungary", Country::Eu),
    ("IRL", "Ireland", Country::Eu),
    ("LTU", "Lithuania", Country::Eu),
    ("LUXE", "Luxembourg", Country::Eu),
    ("POR", "Portugal", Country::Eu),
    ("ROM", "Romania", Country::Eu),
    ("SVN", "Slovenia", Country::Eu),
    ("BUL", "Bulgaria", Country::Eu),
    ("HRV", "Croatia", Country::Eu),
];

/// The class an `OWNER` code is drawn in ([`OWNERS`]); unknown: Other.
#[must_use]
pub fn country_of_owner(code: &str) -> Country {
    OWNERS.iter().find(|(c, _, _)| *c == code.trim()).map_or(Country::Other, |(_, _, k)| *k)
}

/// The card's owner line: `"France (EU)"`, `"Unknown owner"`, or the bare
/// code for one not in [`OWNERS`].
#[must_use]
pub fn owner_line(code: Option<&str>) -> String {
    match code {
        None => "Owner unknown".to_string(),
        Some(c) => {
            let class = country_name(country_of_owner(c));
            match OWNERS.iter().find(|(k, _, _)| *k == c.trim()) {
                Some((_, name, _)) => format!("Owner: {name} ({class})"),
                None => format!("Owner: {c} ({class})"),
            }
        }
    }
}

/// SATCAT JSON (CelesTrak `satcat/records.php?FORMAT=json`): each
/// record's `NORAD_CAT_ID` and `OWNER`. Lenient per record: a row whose id
/// is missing, not a whole number in range, or whose owner is missing,
/// empty or not a string is skipped, never the body. A body that is not a
/// JSON array is an error.
pub fn parse_satcat(body: &[u8]) -> Result<std::collections::HashMap<u32, Box<str>>, String> {
    let recs: Vec<serde_json::Value> = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    Ok(recs
        .iter()
        .filter_map(|r| {
            let id = u32::try_from(r.get("NORAD_CAT_ID")?.as_u64()?).ok()?;
            let owner = r.get("OWNER")?.as_str()?.trim();
            (!owner.is_empty()).then(|| (id, owner.into()))
        })
        .collect())
}

/// Each drawn object's owner code, joined by NORAD id (`None` where the
/// SATCAT has no record, or there is no SATCAT).
#[must_use]
pub fn owners_for(catalog: &Catalog, satcat: Option<&std::collections::HashMap<u32, Box<str>>>) -> Vec<Option<Box<str>>> {
    catalog.objects.iter().map(|o| satcat.and_then(|m| m.get(&o.record.norad_id).cloned())).collect()
}

/// Each object's class: its owner's, else US for a Starlink without a
/// SATCAT record (SpaceX is a US operator), else Other.
#[must_use]
pub fn countries_for(catalog: &Catalog, owners: &[Option<Box<str>>]) -> Vec<Country> {
    catalog
        .objects
        .iter()
        .enumerate()
        .map(|(i, o)| match owners.get(i).and_then(|x| x.as_deref()) {
            Some(code) => country_of_owner(code),
            None if o.group == Group::Starlink => Country::Us,
            None => Country::Other,
        })
        .collect()
}

/// A class's colour in `look` at alpha `a` (the same hues flat and glowing).
#[must_use]
pub fn country_rgba(c: Country, a: f32) -> Rgba {
    let v = COUNTRY_COLOURS[c as usize];
    let ch = |shift: u32| ((v >> shift) & 0xFF) as f32 / 255.0;
    Rgba { r: ch(16), g: ch(8), b: ch(0), a }
}

/// What the dots, the chips' swatches and the chart stacks are coloured by
/// ("Colour by"). A palette only: it never changes what is filtered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColourBy {
    Group,
    Country,
}

/// The dots' style coloured by country: paint index = class, the group's
/// dot size kept per object is not possible with one size per paint, so
/// every class draws at 2 px but the US, which carries Starlink's shell and
/// takes Starlink's 1.6 px and halo gain.
#[must_use]
pub fn style_by_country(catalog: &Catalog, countries: &[Country], look: AltitudeLook, trails: bool, earth: Earth) -> AltitudeStyle {
    let mut style = style_for_earth(catalog, look, trails, earth);
    style.paint = countries.iter().map(|&c| c as u8).collect();
    // Every class shares the LEO shell with the others (US Starlink, UK
    // OneWeb, China's shells), and eight hues summed by a screen blend go
    // white faster than one: every halo at 0.45, dots at 1.8 px.
    style.glow_gain = [0.45; 16];
    for c in COUNTRIES {
        style.palette[c as usize] = country_scene_rgba(c, earth, 1.0);
        style.base_px[c as usize] = 1.8;
    }
    // The US class holds Starlink's 11k: as dense, as small and as faint as
    // the Starlink group, or the shell burns white.
    style.glow_gain[Country::Us as usize] = STARLINK_GLOW_GAIN;
    style.base_px[Country::Us as usize] = GROUP_BASE_PX[Group::Starlink as usize];
    style
}

/// The chips with only `group` on (the `starlink_only` capture).
#[must_use]
pub fn only(group: Group) -> [bool; GROUP_COUNT] {
    std::array::from_fn(|g| g == group as usize)
}

/// The status line's head: `"16,556 of 16,556 shown"`.
#[must_use]
pub fn shown_line(shown: usize, total: usize) -> String {
    format!("{} of {} shown", thousands(shown), thousands(total))
}

/// One dimension's chips in a few words, or `None` when every chip is on:
/// `"none"`, `"all but Starlink"`, `"GPS, Galileo"` (up to three named),
/// else `"4 of 8"`.
#[must_use]
pub fn chips_summary(on: &[bool], names: &[&str]) -> Option<String> {
    let n = on.iter().filter(|&&v| v).count();
    if n == on.len() {
        return None;
    }
    let named = |want: bool| names.iter().zip(on).filter(|(_, &v)| v == want).map(|(s, _)| *s).collect::<Vec<_>>();
    Some(match n {
        0 => "none".to_string(),
        _ if n == on.len() - 1 => format!("all but {}", named(false).join("")),
        1..=3 => named(true).join(", "),
        _ => format!("{n} of {}", on.len()),
    })
}

/// Every active filter, one short phrase each, in the panel's order
/// (type, country, altitude, inclination): `["Type: all but Starlink",
/// "Alt: LEO", "Incl: 50-60°"]`. Empty when nothing narrows the set.
#[must_use]
pub fn filter_summaries(f: &Filters) -> Vec<String> {
    let d = Filters::default();
    let mut out = Vec::new();
    let type_on: Vec<bool> = GROUPS.iter().map(|&g| f.groups[g as usize]).collect();
    let type_names: Vec<&str> = GROUPS.iter().map(|&g| group_name(g)).collect();
    if let Some(s) = chips_summary(&type_on, &type_names) {
        out.push(format!("Type: {s}"));
    }
    let country_on: Vec<bool> = COUNTRIES.iter().map(|&c| f.countries[c as usize]).collect();
    let country_names: Vec<&str> = COUNTRIES.iter().map(|&c| country_name(c)).collect();
    if let Some(s) = chips_summary(&country_on, &country_names) {
        out.push(format!("Country: {s}"));
    }
    if let Some(p) = AltPreset::of(f).filter(|p| *p != AltPreset::All) {
        out.push(format!("Alt: {}", p.label()));
    } else if (f.alt_km, f.heo_only) != (d.alt_km, false) {
        let end = |k: f64| if k.is_finite() { thousands(k.round() as usize) } else { "∞".to_string() };
        out.push(format!("Alt: {}-{} km", end(f.alt_km.0), end(f.alt_km.1)));
    }
    if f.incl_deg != d.incl_deg {
        out.push(format!("Incl: {:.0}-{:.0}°", f.incl_deg.0, f.incl_deg.1));
    }
    out
}

/// The filter card's summary line, always drawn so a press never inserts
/// or removes a row: the active filters joined (`"Type: all but
/// Starlink · Alt: LEO"`), or [`NO_FILTER_SUMMARY`] when none is.
#[must_use]
pub fn filter_summary_line(f: &Filters) -> String {
    let parts = filter_summaries(f);
    if parts.is_empty() { NO_FILTER_SUMMARY.to_string() } else { parts.join(" · ") }
}

/// The summary line with nothing filtered.
pub const NO_FILTER_SUMMARY: &str = "All types · all countries";

/// Whether any filter narrows the set ("reset filters" is live).
#[must_use]
pub fn filters_active(f: &Filters) -> bool {
    *f != Filters::default()
}

/// The chip row: which groups are shown, how many objects each has, and
/// whether its CelesTrak list was downloaded at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chips {
    pub enabled: [bool; GROUP_COUNT],
    pub counts: [usize; GROUP_COUNT],
    pub downloaded: [bool; GROUP_COUNT],
}

impl Default for Chips {
    fn default() -> Self {
        Self { enabled: [true; GROUP_COUNT], counts: [0; GROUP_COUNT], downloaded: [false; GROUP_COUNT] }
    }
}

impl Chips {
    /// The chip's text: its count, or that its list is missing.
    #[must_use]
    pub fn label(&self, group: Group) -> String {
        let g = group as usize;
        chip_label(group, self.downloaded[g].then_some(self.counts[g]))
    }
}

/// `n` with thousands separators.
#[must_use]
pub fn thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `"Starlink 7,112"`, or `"GPS: not downloaded"` without a count.
#[must_use]
pub fn chip_label(group: Group, count: Option<usize>) -> String {
    match count {
        Some(n) => format!("{} {}", group_name(group), thousands(n)),
        None => format!("{}: not downloaded", group_name(group)),
    }
}

/// One flag per object: its group's chip is on.
#[must_use]
pub fn visibility_mask(catalog: &Catalog, enabled: [bool; GROUP_COUNT]) -> Vec<bool> {
    catalog.objects.iter().map(|o| enabled[o.group as usize]).collect()
}

/// The scrubber at `fraction` (0 left, 1 right; clamped) as a scene time:
/// `now - 24 h` to `now + 24 h`.
#[must_use]
pub fn scrub_to_unix(now_unix: f64, fraction: f32) -> f64 {
    let f = f64::from(if fraction.is_nan() { 0.5 } else { fraction.clamp(0.0, 1.0) });
    now_unix + (2.0 * f - 1.0) * SCRUB_SPAN_S
}

/// Where the scrubber's thumb sits for scene time `scene_unix`: the inverse
/// of [`scrub_to_unix`], clamped to the bar.
#[must_use]
pub fn scrub_fraction(now_unix: f64, scene_unix: f64) -> f32 {
    ((scene_unix - now_unix) / SCRUB_SPAN_S * 0.5 + 0.5).clamp(0.0, 1.0) as f32
}

/// `"2026-09-29 14:03:22 UTC"`.
#[must_use]
pub fn format_utc(unix: f64) -> String {
    chrono::DateTime::from_timestamp(unix.floor() as i64, 0)
        .map_or_else(|| format!("{unix:.0}"), |d| d.format("%Y-%m-%d %H:%M:%S UTC").to_string())
}

/// What an object's card says.
#[derive(Clone, Debug, PartialEq)]
pub struct CardText {
    /// `"ISS (ZARYA) · 25544"`.
    pub title: String,
    pub group: String,
    pub lines: Vec<String>,
    /// The elements are more than [`CARD_STALE_S`] from the scene time.
    pub stale: bool,
    /// `"+ 7 attached: POISK, …"`: the stack drawn as this one dot.
    pub attached: Option<String>,
}

/// Elements further than this from the scene time are flagged on the card
/// (SGP4's error grows by kilometres a day away from the epoch).
pub const CARD_STALE_S: f64 = 3.0 * 86_400.0;

/// The card for `obj` at `scene_unix`. `at` is where the map last drew it
/// (`MapView::altitude_point_state`); without it the altitude reads `—`.
#[must_use]
pub fn card_for(obj: &SatObject, at: Option<&AltitudePointState>, scene_unix: f64) -> CardText {
    let r = &obj.record;
    let d = propagate::derived(r);
    let altitude = at.map_or_else(
        || "Altitude —".to_string(),
        |s| {
            // The sphere frame's axes: lon = atan2(x, z), lat = asin(y / r),
            // so Earth-fixed (x, y, z) is the sphere's (z, x, y).
            let k = GLOBE_RADIUS_M / 1000.0;
            let ecef = [s.sphere.z * k, s.sphere.x * k, s.sphere.y * k];
            format!("Altitude {:.0} km", propagate::geodetic_altitude_km(ecef))
        },
    );
    let age_s = scene_unix - r.epoch_unix;
    let age_days = age_s / 86_400.0;
    let age = if age_days.abs() < 1.0 / 24.0 {
        format!("Elements {:.0} min old", age_s / 60.0)
    } else if age_days < 0.0 {
        format!("Elements from {:.1} days ahead", -age_days)
    } else {
        format!("Elements {age_days:.1} days old")
    };
    let group = format!("Group: {}", group_long_name(obj.group));
    let lines = vec![
        group.clone(),
        altitude,
        format!("Inclination {:.2}°", d.inclination_deg),
        format!("Period {:.2} min", d.period_min),
        format!("Perigee {:.0} km · apogee {:.0} km", d.perigee_km, d.apogee_km),
        age,
    ];
    let attached = (!obj.attached.is_empty()).then(|| {
        let mut names: Vec<&str> = obj.attached.iter().take(3).map(|(_, n)| &**n).collect();
        if obj.attached.len() > 3 {
            names.push("…");
        }
        format!("+ {} attached: {}", obj.attached.len(), names.join(", "))
    });
    CardText { title: format!("{} · {}", r.object_name, r.norad_id), group, lines, stale: age_s.abs() > CARD_STALE_S, attached }
}

/// The highest zoom the follow camera may use for an object `alt_m` above
/// the ground at `lat_deg` on a `viewport_h` px view: the eye's altitude
/// stays at least 1.5x the object's. Spec §2.6 inverted: the eye is
/// `d / r_w` radii above the surface, with `d = H / (2 tan 30° 2^z)` and
/// `r_w = 512 / (2π cos lat)`, so `z = log2(H / (2 tan 30° · 1.5 · alt/R · r_w))`.
#[must_use]
pub fn follow_zoom_cap(alt_m: f64, lat_deg: f64, viewport_h: f32) -> f64 {
    let r_w = 512.0 / (std::f64::consts::TAU * lat_deg.to_radians().cos().max(1e-6));
    let tan30 = (std::f64::consts::PI / 6.0).tan();
    let d = 1.5 * (alt_m.max(1.0) / GLOBE_RADIUS_M) * r_w;
    (f64::from(viewport_h) / (2.0 * tan30 * d)).log2()
}

/// The pointer's objects and the follow camera.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    pub hovered: Option<usize>,
    pub selected: Option<usize>,
    pub following: bool,
}

impl Selection {
    /// A chip changed: a hovered or selected object whose group is now
    /// hidden is dropped, and follow ends with the selection.
    pub fn on_groups_changed(&mut self, catalog: &Catalog, enabled: [bool; GROUP_COUNT]) {
        let shown = |i: Option<usize>| i.filter(|&i| catalog.objects.get(i).is_some_and(|o| enabled[o.group as usize]));
        self.hovered = shown(self.hovered);
        self.selected = shown(self.selected);
        if self.selected.is_none() {
            self.following = false;
        }
    }

    /// The mask changed: a hovered or selected object it hides is dropped,
    /// and follow ends with the selection.
    pub fn on_mask_changed(&mut self, mask: &[bool]) {
        let shown = |i: Option<usize>| i.filter(|&i| mask.get(i).copied().unwrap_or(false));
        self.hovered = shown(self.hovered);
        self.selected = shown(self.selected);
        if self.selected.is_none() {
            self.following = false;
        }
    }

    /// The user moved the camera: follow ends, the selection stays.
    pub fn on_user_camera_move(&mut self) {
        self.following = false;
    }

    /// A click: on an object selects it (and ends a follow of another); on
    /// empty space deselects.
    pub fn on_click(&mut self, hit: Option<usize>) {
        if hit != self.selected {
            self.following = false;
        }
        self.selected = hit;
    }

    /// Remaps indices after the catalogue was rebuilt (a fetch landed):
    /// `norad_of` names each old index's object, `index_of` finds it anew.
    pub fn remap(&mut self, norad_of: impl Fn(usize) -> Option<u32>, index_of: impl Fn(u32) -> Option<usize>) {
        let map = |i: Option<usize>| i.and_then(&norad_of).and_then(&index_of);
        self.hovered = map(self.hovered);
        self.selected = map(self.selected);
        if self.selected.is_none() {
            self.following = false;
        }
    }
}

/// How far the camera centre may drift from the one follow set before it
/// counts as a user move, degrees.
pub const FOLLOW_SLACK_DEG: f64 = 1e-6;

/// Whether the camera centre `now` differs from `set` (the centre follow
/// put there last frame) by more than [`FOLLOW_SLACK_DEG`]: a drag, a key,
/// a scroll-zoom about the pointer.
#[must_use]
pub fn moved_by_user(set: (f64, f64), now: (f64, f64)) -> bool {
    let dlon = (now.0 - set.0 + 540.0).rem_euclid(360.0) - 180.0;
    dlon.abs() > FOLLOW_SLACK_DEG || (now.1 - set.1).abs() > FOLLOW_SLACK_DEG
}

/// Elements older than this raise the staleness chip.
pub const STALE_CHIP_S: f64 = 7.0 * 86_400.0;

/// `"Elements 8 days old: positions drift"` when the newest element set is
/// more than a week older than the scene; `None` otherwise.
#[must_use]
pub fn staleness_chip(newest_epoch_unix: f64, scene_unix: f64) -> Option<String> {
    let age = scene_unix - newest_epoch_unix;
    (age > STALE_CHIP_S).then(|| format!("Elements {:.0} days old: positions drift by kilometres a day", (age / 86_400.0).floor()))
}

/// Whether a ring (or ground track) built at scene time `last_unix` is
/// stale at `scene_unix`: the scene has moved `fraction` of a `period_s`
/// either way since. The ring is rebuilt every period/8 (J2 walks a LEO dot
/// off a ring sampled once by ~24 km an hour), the track every period/20
/// (its window is `[t - P/2, t + P]` about the dot).
#[must_use]
#[allow(clippy::neg_cmp_op_on_partial_ord, reason = "NaN-safe: a NaN scene time counts as due")]
pub fn ring_due(last_unix: f64, scene_unix: f64, period_s: f64, fraction: f64) -> bool {
    !((scene_unix - last_unix).abs() < period_s * fraction)
}

/// Fractions of a period after which [`ring_due`] rebuilds a ring and a
/// ground track.
pub const RING_FRACTION: f64 = 1.0 / 8.0;
pub const TRACK_FRACTION: f64 = 1.0 / 20.0;

/// An object's orbital period, seconds.
#[must_use]
pub fn period_s(obj: &SatObject) -> f64 {
    1440.0 / obj.record.mean_motion * 60.0
}

/// The orbit ring's first sample time for an object shown at `scene_unix`:
/// half a period back. The ring is `orbit_ring_teme` over one period, and
/// its closing segment joins samples a period apart, which J2 has moved
/// ~0.3° apart for the ISS (a 15-20 px jog at z5). Starting half a period
/// back puts that seam on the far side of the orbit from the object, behind
/// the planet for LEO; the dot moves at most 45° before the P/8 rebuild.
#[must_use]
pub fn ring_start(obj: &SatObject, scene_unix: f64) -> f64 {
    scene_unix - period_s(obj) / 2.0
}

/// Whether the panel repaints on a tick: something changed, or something
/// on it moves -- follow, a fetch or a scrub in progress, a hover card
/// following its dot on a running clock, or the scene time's printed
/// second. A paused panel with a hover or a selection asks for nothing (its
/// cards cannot change), and a running one repaints once a printed second,
/// not at 30 Hz: the map draws its moving dots on its own.
#[must_use]
pub fn tick_repaints(changed: bool, rate: f64, hovering: bool, following: bool, busy: bool, label_changed: bool) -> bool {
    changed || following || busy || label_changed || (rate != 0.0 && hovering)
}

/// The panel tick's period while anything it tracks can move: 30 Hz.
pub const TICK_ACTIVE: std::time::Duration = std::time::Duration::from_millis(33);
/// The panel tick's period when paused and idle: 4 Hz, so an idle panel
/// wakes the CPU 4 times a second rather than 30 (final review M2). A
/// camera move is still noticed within one idle tick, which then goes
/// back to [`TICK_ACTIVE`].
pub const TICK_IDLE: std::time::Duration = std::time::Duration::from_millis(250);

/// How long the panel sleeps before its next tick: [`TICK_IDLE`] only when
/// the clock is paused (rate 0) and nothing else is in flight -- no follow,
/// no fetch or scrub (`busy`), the camera did not move since the last tick,
/// and the first dots have been stamped (`settled`); [`TICK_ACTIVE`]
/// otherwise. A running clock ticks at 30 Hz for the rings' cadence, the
/// hover card and the printed second.
#[must_use]
pub fn tick_interval(rate: f64, following: bool, busy: bool, camera_moved: bool, settled: bool) -> std::time::Duration {
    let paused = rate == 0.0 || rate.is_nan();
    if paused && !following && !busy && !camera_moved && settled {
        TICK_IDLE
    } else {
        TICK_ACTIVE
    }
}

/// Pixels a press may travel and still be a click (not a drag).
pub const CLICK_SLOP_PX: f32 = 4.0;

/// Whether a press at `down` released at `up` is a click.
#[must_use]
pub fn is_click(down: (f32, f32), up: (f32, f32)) -> bool {
    (up.0 - down.0).hypot(up.1 - down.1) <= CLICK_SLOP_PX
}

/// The CelesTrak groups (`fetch::GROUPS`) the enabled chips need: `active`
/// carries Starlink and every "other" object; each named list only
/// classifies its own chip. A hidden chip's list is never requested (R16:
/// only enabled groups are fetched).
#[must_use]
pub fn celestrak_groups(enabled: [bool; GROUP_COUNT]) -> Vec<&'static str> {
    let on = |g: Group| enabled[g as usize];
    let mut out = Vec::new();
    if on(Group::Starlink) || on(Group::Other) {
        out.push("active");
    }

    for (g, name) in [
        (Group::Stations, "stations"),
        (Group::Gps, "gps-ops"),
        (Group::Galileo, "galileo"),
        (Group::Glonass, "glo-ops"),
        (Group::Beidou, "beidou"),
        (Group::Geo, "geo"),
    ] {
        if on(g) {
            out.push(name);
        }
    }
    out
}

/// The cached (or given) OMM bodies the catalogue is built from.
#[derive(Clone, Debug, Default)]
pub struct Bodies {
    pub active: Option<Vec<u8>>,
    pub stations: Option<Vec<u8>>,
    pub gps: Option<Vec<u8>>,
    pub geo: Option<Vec<u8>>,
    pub galileo: Option<Vec<u8>>,
    pub glonass: Option<Vec<u8>>,
    pub beidou: Option<Vec<u8>>,
    /// SATCAT JSON (owners), for colour-by-country.
    pub satcat: Option<Vec<u8>>,
}

/// A catalogue ready to draw.
pub struct Loaded {
    pub catalog: Arc<Catalog>,
    /// Per group: its list was there to classify by (`Starlink` and `Other`
    /// come from `active`).
    pub downloaded: [bool; GROUP_COUNT],
    /// Records the parse refused plus those sgp4 would not initialise.
    pub skipped: usize,
    pub synthetic: bool,
    /// Per object (catalogue order): its SATCAT owner code, if known.
    pub owners: Vec<Option<Box<str>>>,
    /// Per object: what every filter and chart reads, computed once here
    /// from the mean elements and the owner -- never by propagating.
    pub stats: Vec<ObjStats>,
}

impl Loaded {
    /// Wraps `catalog` with its owners and per-object stats.
    #[must_use]
    pub fn new(catalog: Catalog, downloaded: [bool; GROUP_COUNT], skipped: usize, synthetic: bool, satcat: Option<&[u8]>) -> Self {
        let owner_map = satcat.and_then(|b| parse_satcat(b).ok());
        let owners = owners_for(&catalog, owner_map.as_ref());
        let countries = countries_for(&catalog, &owners);
        let stats = catalog.objects.iter().zip(&countries).map(|(o, &c)| ObjStats::of(o, c)).collect();
        Self { catalog: Arc::new(catalog), downloaded, skipped, synthetic, owners, stats }
    }

    /// Whether a SATCAT was there to class owners by.
    #[must_use]
    pub fn has_owners(&self) -> bool {
        self.owners.iter().any(Option::is_some)
    }
}

/// One object's filterable numbers, from its mean elements.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ObjStats {
    pub group: Group,
    pub country: Country,
    /// Perigee and apogee altitude, km above the equatorial radius
    /// (`propagate::derived`: `a (1 -+ e) - R`).
    pub perigee_km: f64,
    pub apogee_km: f64,
    pub eccentricity: f64,
    pub inclination_deg: f64,
    /// The object's bin in the altitude and inclination histograms,
    /// computed once here, so a chart is a count, never a recompute.
    pub alt_bin: u8,
    pub incl_bin: u8,
}

impl ObjStats {
    #[must_use]
    pub fn of(o: &SatObject, country: Country) -> Self {
        let d = propagate::derived(&o.record);
        Self::from_orbit(o.group, country, d.perigee_km, d.apogee_km, o.record.eccentricity, d.inclination_deg)
    }

    /// The stats of an orbit given directly (tests, fixtures).
    #[must_use]
    pub fn from_orbit(group: Group, country: Country, perigee_km: f64, apogee_km: f64, eccentricity: f64, inclination_deg: f64) -> Self {
        let bin = |x: f32, n: usize| ((x.max(0.0) * n as f32) as usize).min(n - 1) as u8;
        Self {
            group,
            country,
            perigee_km,
            apogee_km,
            eccentricity,
            inclination_deg,
            alt_bin: bin(alt_to_frac(0.5 * (perigee_km + apogee_km)), ALT_BINS),
            incl_bin: bin((inclination_deg / 180.0) as f32, INCL_BINS),
        }
    }
}

/// The altitude filter's reach, km (the slider's log scale ends).
pub const ALT_MIN_KM: f64 = 150.0;
pub const ALT_MAX_KM: f64 = 60_000.0;

/// Every filter the dots are masked by: the group chips, the country
/// chips, an ORBIT altitude range (an object is in when its
/// [perigee, apogee] overlaps it, so an eccentric orbit never blinks as the
/// object climbs), an inclination range, and HEO (the HEO preset).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Filters {
    pub groups: [bool; GROUP_COUNT],
    pub countries: [bool; COUNTRY_COUNT],
    pub alt_km: (f64, f64),
    pub incl_deg: (f64, f64),
    /// Only highly elliptical orbits (eccentricity > 0.25 or apogee above
    /// 40,000 km); off unless the HEO preset is chosen.
    pub heo_only: bool,
}

impl Default for Filters {
    fn default() -> Self {
        Self { groups: [true; GROUP_COUNT], countries: [true; COUNTRY_COUNT], alt_km: (0.0, f64::INFINITY), incl_deg: (0.0, 180.0), heo_only: false }
    }
}

/// Whether `[lo, hi]` and `[a, b]` overlap (inclusive).
#[must_use]
pub fn overlaps(lo: f64, hi: f64, a: f64, b: f64) -> bool {
    lo <= b && a <= hi
}

/// Whether an orbit is highly elliptical: e > 0.25 or apogee > 40,000 km.
#[must_use]
pub fn is_heo(s: &ObjStats) -> bool {
    s.eccentricity > 0.25 || s.apogee_km > 40_000.0
}

impl Filters {
    /// Whether one object passes every filter.
    #[must_use]
    pub fn passes(&self, s: &ObjStats) -> bool {
        self.pass_group(s) && self.pass_country(s) && self.pass_alt(s) && self.pass_incl(s)
    }

    /// The type dimension: the object's group chip is on.
    #[must_use]
    pub fn pass_group(&self, s: &ObjStats) -> bool {
        self.groups[s.group as usize]
    }

    /// The country dimension: the object's owner-class chip is on.
    #[must_use]
    pub fn pass_country(&self, s: &ObjStats) -> bool {
        self.countries[s.country as usize]
    }

    /// The altitude dimension: the orbit overlaps the range (and is HEO
    /// when the HEO preset is on).
    #[must_use]
    pub fn pass_alt(&self, s: &ObjStats) -> bool {
        overlaps(self.alt_km.0, self.alt_km.1, s.perigee_km, s.apogee_km) && (!self.heo_only || is_heo(s))
    }

    /// The inclination dimension.
    #[must_use]
    pub fn pass_incl(&self, s: &ObjStats) -> bool {
        s.inclination_deg >= self.incl_deg.0 && s.inclination_deg <= self.incl_deg.1
    }

    /// Every filter except one kind, for a chart that shows what that kind
    /// would add back: `except_alt` ignores the altitude range (and HEO),
    /// `except_incl` the inclination range.
    #[must_use]
    pub fn without(&self, except_alt: bool, except_incl: bool) -> Self {
        let d = Filters::default();
        Self {
            alt_km: if except_alt { d.alt_km } else { self.alt_km },
            heo_only: if except_alt { false } else { self.heo_only },
            incl_deg: if except_incl { d.incl_deg } else { self.incl_deg },
            ..*self
        }
    }
}

/// The dots' mask: one flag per object, [`Filters::passes`]. One mask for
/// every filter, pushed through `set_altitude_visibility` (flag bits, no
/// re-propagation).
#[must_use]
pub fn filter_mask(stats: &[ObjStats], f: &Filters) -> Vec<bool> {
    stats.iter().map(|s| f.passes(s)).collect()
}

/// The altitude presets (the filter's buttons).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AltPreset {
    All,
    Leo,
    Meo,
    Geo,
    Heo,
}

impl AltPreset {
    pub const ALL: [AltPreset; 5] = [AltPreset::All, AltPreset::Leo, AltPreset::Meo, AltPreset::Geo, AltPreset::Heo];

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            AltPreset::All => "All",
            AltPreset::Leo => "LEO",
            AltPreset::Meo => "MEO",
            AltPreset::Geo => "GEO",
            AltPreset::Heo => "HEO",
        }
    }

    /// `(altitude range km, HEO only)`: LEO below 2,000 km; MEO 2,000 to
    /// 35,586; GEO the 35,586-35,986 band; HEO by shape, any altitude.
    #[must_use]
    pub fn range(self) -> ((f64, f64), bool) {
        match self {
            AltPreset::All | AltPreset::Heo => ((0.0, f64::INFINITY), self == AltPreset::Heo),
            AltPreset::Leo => ((0.0, 2_000.0), false),
            AltPreset::Meo => ((2_000.0, 35_586.0), false),
            AltPreset::Geo => ((35_586.0, 35_986.0), false),
        }
    }

    /// The preset `f` is exactly on, if any.
    #[must_use]
    pub fn of(f: &Filters) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.range() == (f.alt_km, f.heo_only))
    }
}

/// Log-scale position (0..1) of `km` on the altitude slider, clamped.
#[must_use]
pub fn alt_to_frac(km: f64) -> f32 {
    let k = km.clamp(ALT_MIN_KM, ALT_MAX_KM);
    ((k / ALT_MIN_KM).ln() / (ALT_MAX_KM / ALT_MIN_KM).ln()) as f32
}

/// [`alt_to_frac`]'s inverse; the ends map to "no bound" (0 and infinity),
/// so a handle dragged fully out includes everything past the scale.
#[must_use]
pub fn frac_to_alt(f: f32) -> f64 {
    let f = f64::from(f.clamp(0.0, 1.0));
    if f <= 0.0 {
        0.0
    } else if f >= 1.0 {
        f64::INFINITY
    } else {
        ALT_MIN_KM * (ALT_MAX_KM / ALT_MIN_KM).powf(f)
    }
}

/// A brush drag on a `[0, 1]` axis: moves the `low` handle (or, `None`,
/// whichever handle is nearer `x`) to `x`; the pair is kept ordered, the
/// moved handle swapping sides if it crosses the other. Returns the new
/// pair and which handle is now the moved one (`true` low).
#[must_use]
pub fn brush(sel: (f32, f32), x: f32, low: Option<bool>) -> ((f32, f32), bool) {
    let x = x.clamp(0.0, 1.0);
    let low = low.unwrap_or((x - sel.0).abs() <= (x - sel.1).abs());
    let (a, b) = if low { (x, sel.1) } else { (sel.0, x) };
    if a <= b { ((a, b), low) } else { ((b, a), !low) }
}

/// Histogram bins for the charts.
pub const ALT_BINS: usize = 40;
pub const INCL_BINS: usize = 36;

/// Per bin, per class: counts of the objects that pass `f` (the chart's
/// own filter left out by the caller with [`Filters::without`]), stacked by
/// `class_of` (`n_classes` classes). Altitude bins are on the slider's log
/// scale by the orbit's mean altitude ((perigee + apogee) / 2); inclination
/// bins are 5 degrees.
#[must_use]
pub fn histogram(stats: &[ObjStats], f: &Filters, altitude: bool, n_classes: usize, class_of: impl Fn(&ObjStats) -> usize) -> Vec<Vec<u32>> {
    let bins = if altitude { ALT_BINS } else { INCL_BINS };
    let mut out = vec![vec![0u32; n_classes]; bins];
    for s in stats.iter().filter(|s| f.passes(s)) {
        let b = usize::from(if altitude { s.alt_bin } else { s.incl_bin }).min(bins - 1);
        let c = class_of(s).min(n_classes - 1);
        out[b][c] += 1;
    }
    out
}

/// Per class: objects passing `f`.
#[must_use]
pub fn class_counts(stats: &[ObjStats], f: &Filters, n_classes: usize, class_of: impl Fn(&ObjStats) -> usize) -> Vec<u32> {
    let mut out = vec![0u32; n_classes];
    for s in stats.iter().filter(|s| f.passes(s)) {
        out[class_of(s).min(n_classes - 1)] += 1;
    }
    out
}

/// The crossfilter over every dimension at once (type, country, altitude,
/// inclination): for each dimension, what EVERY OTHER dimension's filter
/// lets through, so a chip's count is what it holds under the rest of the
/// filters whether it is on or off, and a chart keeps its brush's context.
/// One pass over the per-object stats (bins and classes precomputed at
/// load); the panel recounts it only when a filter or the colouring
/// changes, never per frame.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CrossCounts {
    /// Per group (`group as usize`): objects of that group passing the
    /// country, altitude and inclination filters.
    pub groups: [u32; GROUP_COUNT],
    /// Per owner class: objects of that class passing the type, altitude
    /// and inclination filters.
    pub countries: [u32; COUNTRY_COUNT],
    /// Per group and per owner class with NO filter: the most a chip can
    /// ever count, so its count's slot is sized once and a press never
    /// changes the chip's width.
    pub group_totals: [u32; GROUP_COUNT],
    pub country_totals: [u32; COUNTRY_COUNT],
    /// `[bin][class]`: objects passing every filter but altitude
    /// (the altitude chart's context) ...
    pub alt: Vec<Vec<u32>>,
    /// ... and those of them passing altitude too (its highlighted part).
    pub alt_in: Vec<Vec<u32>>,
    /// The inclination chart, likewise.
    pub incl: Vec<Vec<u32>>,
    pub incl_in: Vec<Vec<u32>>,
    /// Objects passing every filter, and every object.
    pub shown: u32,
    pub total: u32,
    /// The dots' mask, one flag per object in `stats`' order: the same
    /// pass/fail [`Filters::passes`] would give, computed in this same
    /// pass so a filter change (including a brush drag) walks the
    /// catalogue once, not once for the mask and once for the counts.
    pub mask: Vec<bool>,
}

/// [`CrossCounts`] of `stats` under `f`, the charts stacked by `class_of`
/// (`n_classes` classes); also the dots' mask ([`CrossCounts::mask`]), in
/// the same one pass.
#[must_use]
pub fn cross_counts(stats: &[ObjStats], f: &Filters, n_classes: usize, class_of: impl Fn(&ObjStats) -> usize) -> CrossCounts {
    let mut out = CrossCounts {
        alt: vec![vec![0; n_classes]; ALT_BINS],
        alt_in: vec![vec![0; n_classes]; ALT_BINS],
        incl: vec![vec![0; n_classes]; INCL_BINS],
        incl_in: vec![vec![0; n_classes]; INCL_BINS],
        total: u32::try_from(stats.len()).unwrap_or(u32::MAX),
        mask: vec![false; stats.len()],
        ..CrossCounts::default()
    };
    for (idx, s) in stats.iter().enumerate() {
        let (g, c, a, i) = (f.pass_group(s), f.pass_country(s), f.pass_alt(s), f.pass_incl(s));
        let class = class_of(s).min(n_classes - 1);
        let (ab, ib) = (usize::from(s.alt_bin).min(ALT_BINS - 1), usize::from(s.incl_bin).min(INCL_BINS - 1));
        out.group_totals[s.group as usize] += 1;
        out.country_totals[s.country as usize] += 1;
        if c && a && i {
            out.groups[s.group as usize] += 1;
        }
        if g && a && i {
            out.countries[s.country as usize] += 1;
        }
        if g && c && i {
            out.alt[ab][class] += 1;
            if a {
                out.alt_in[ab][class] += 1;
            }
        }
        if g && c && a {
            out.incl[ib][class] += 1;
            if i {
                out.incl_in[ib][class] += 1;
                out.shown += 1;
                out.mask[idx] = true;
            }
        }
    }
    out
}

/// The catalogue from `bodies`: every body's records (so a cache with only
/// `stations` still draws the stations), classified by whichever group
/// lists are there. `None` when no body has a usable record.
#[must_use]
pub fn catalog_from_bodies(bodies: &Bodies) -> Option<Loaded> {
    let mut records = Vec::new();
    let mut skipped = 0;
    let mut ids = |body: &Option<Vec<u8>>| -> Option<std::collections::HashSet<u32>> {
        let report = parse_omm(body.as_deref()?).ok()?;
        skipped += report.skipped;
        let set = report.records.iter().map(|r| r.norad_id).collect();
        records.extend(report.records);
        Some(set)
    };
    let active = ids(&bodies.active).is_some();
    let membership = Membership {
        stations: ids(&bodies.stations),
        gps: ids(&bodies.gps),
        geo: ids(&bodies.geo),
        galileo: ids(&bodies.galileo),
        glonass: ids(&bodies.glonass),
        beidou: ids(&bodies.beidou),
    };
    if records.is_empty() {
        return None;
    }
    // The group lists' records are also in `active`: counting their skips
    // twice would overstate the About card's number.
    let downloaded = [
        membership.stations.is_some(),
        active,
        membership.gps.is_some(),
        membership.geo.is_some(),
        active,
        membership.galileo.is_some(),
        membership.glonass.is_some(),
        membership.beidou.is_some(),
    ];
    let catalog = Catalog::build(records, &membership);
    let skipped = if active { parse_omm(bodies.active.as_deref().unwrap_or_default()).map_or(0, |r| r.skipped) } else { skipped };
    let skipped = skipped + catalog.skipped_by_sgp4;
    Some(Loaded::new(catalog, downloaded, skipped, false, bodies.satcat.as_deref()))
}

/// The seeded synthetic set (`orbit::synthetic`), every group present.
#[must_use]
pub fn synthetic_loaded() -> Loaded {
    use gpui_maps::orbit::synthetic::{synthetic_membership, synthetic_records};
    let records = synthetic_records(7);
    let membership = synthetic_membership(&records);
    let catalog = Catalog::build(records, &membership);
    let skipped = catalog.skipped_by_sgp4;
    Loaded::new(catalog, [true; GROUP_COUNT], skipped, true, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_maps::orbit::{catalog::*, omm::parse_omm};
    fn stations() -> Catalog {
        let recs = parse_omm(include_bytes!("../../tests/fixtures/orbit/stations.json")).unwrap().records;
        let ids = recs.iter().map(|r| r.norad_id).collect();
        Catalog::build(recs, &Membership { stations: Some(ids), ..Membership::default() })
    }

    /// The Starlink chip is the one place Starlink is shown or hidden:
    /// toggling it flips exactly the Starlink objects in the mask.
    #[test]
    fn the_starlink_chip_toggles_starlink() {
        let loaded = synthetic_loaded();
        let catalog = &loaded.catalog;
        let counts = catalog.group_counts();
        let starlink = counts[Group::Starlink as usize];
        let total: usize = counts.iter().sum();
        assert!(starlink > 0 && starlink < total);
        let mut hidden = [true; GROUP_COUNT];
        hidden[Group::Starlink as usize] = false;
        for (enabled, want) in [([true; GROUP_COUNT], total), (only(Group::Starlink), starlink), (hidden, total - starlink)] {
            let mask = visibility_mask(catalog, enabled);
            assert_eq!(mask.iter().filter(|&&v| v).count(), want, "{enabled:?}");
            for (o, v) in catalog.objects.iter().zip(&mask) {
                assert_eq!(*v, enabled[o.group as usize]);
            }
        }
        let c = Chips { enabled: only(Group::Starlink), counts: [13, 11_114, 32, 566, 4_831, 0, 0, 0], downloaded: [true; GROUP_COUNT] };
        assert_eq!(c.label(Group::Starlink), "Starlink 11,114", "the chip names Starlink plainly");
    }

    /// Galileo, GLONASS and BeiDou each come from their own CelesTrak list
    /// and win over `geo` (BeiDou's GEO satellites are navigation first);
    /// their chips are the navigation family next to GPS, drawn at GPS's
    /// MEO size, and toggle only their own objects.
    #[test]
    fn the_navigation_lists_classify_their_own_chips() {
        let body = |name: &str, ids: &[u32]| -> Vec<u8> {
            let recs: Vec<String> = ids.iter().map(|id| {
                format!(r#"{{"OBJECT_NAME":"{name} {id}","OBJECT_ID":"2020-001A","EPOCH":"2026-09-28T00:00:00.000000","MEAN_MOTION":2.0056,"ECCENTRICITY":0.0001,"INCLINATION":55.0,"RA_OF_ASC_NODE":{},"ARG_OF_PERICENTER":0.0,"MEAN_ANOMALY":{},"EPHEMERIS_TYPE":0,"CLASSIFICATION_TYPE":"U","NORAD_CAT_ID":{id},"ELEMENT_SET_NO":999,"REV_AT_EPOCH":1,"BSTAR":0,"MEAN_MOTION_DOT":0,"MEAN_MOTION_DDOT":0}}"#, id % 360, (id * 7) % 360)
            }).collect();
            format!("[{}]", recs.join(",")).into_bytes()
        };
        let bodies = Bodies {
            gps: Some(body("NAVSTAR", &[40001, 40002])),
            galileo: Some(body("GSAT", &[41001, 41002, 41003])),
            glonass: Some(body("COSMOS", &[42001])),
            beidou: Some(body("BEIDOU", &[43001, 43002])),
            geo: Some(body("BEIDOU", &[43002])),
            ..Bodies::default()
        };
        let loaded = catalog_from_bodies(&bodies).expect("a catalogue");
        let counts = loaded.catalog.group_counts();
        assert_eq!(
            (counts[Group::Gps as usize], counts[Group::Galileo as usize], counts[Group::Glonass as usize], counts[Group::Beidou as usize], counts[Group::Geo as usize]),
            (2, 3, 1, 2, 0),
            "{counts:?}"
        );
        assert!(loaded.downloaded[Group::Galileo as usize] && loaded.downloaded[Group::Beidou as usize]);
        assert!(!loaded.downloaded[Group::Stations as usize], "no stations list, not downloaded");
        let at = GROUPS.iter().position(|&g| g == Group::Gps).unwrap();
        assert_eq!(&GROUPS[at..at + 4], &[Group::Gps, Group::Galileo, Group::Glonass, Group::Beidou]);
        for g in [Group::Galileo, Group::Glonass, Group::Beidou] {
            assert_eq!(GROUP_BASE_PX[g as usize], GROUP_BASE_PX[Group::Gps as usize]);
        }
        let mut enabled = [true; GROUP_COUNT];
        enabled[Group::Glonass as usize] = false;
        let mask = visibility_mask(&loaded.catalog, enabled);
        for (o, v) in loaded.catalog.objects.iter().zip(&mask) {
            assert_eq!(*v, o.group != Group::Glonass);
        }
        assert_eq!(Chips { enabled, counts, downloaded: loaded.downloaded }.label(Group::Glonass), "GLONASS 1");
    }

    /// EU members and EU agencies merge into one EU class; the UK does not;
    /// unknown codes are Other; the card names the owner and the class.
    #[test]
    fn owner_codes_map_to_classes_with_the_eu_as_one_and_the_uk_apart() {
        for code in ["ESA", "EUME", "FR", "GER", "IT", "SPN", "NETH", "BEL", "SWED", "POL", "FGER", "LUXE", "FIN", "ASRA", "SES", "O3B", "EUTE", "BUL"] {
            assert_eq!(country_of_owner(code), Country::Eu, "{code}");
        }
        // Not circular: every code the table names is on CelesTrak's own
        // list (the committed fixture of satcat/sources.php).
        let listed: std::collections::HashSet<&str> = include_str!("../../tests/fixtures/orbit/satcat_sources.tsv")
            .lines()
            .filter(|l| !l.starts_with('#'))
            .filter_map(|l| l.split('\t').next())
            .collect();
        assert!(listed.len() > 100, "{}", listed.len());
        for (code, _, _) in OWNERS {
            assert!(listed.contains(code), "{code} is not a CelesTrak owner code");
        }
        // The member states' codes that exist are all EU.
        for code in ["ASRA", "BEL", "BUL", "HRV", "CZCH", "DEN", "EST", "FIN", "FR", "GER", "GREC", "HUN", "IRL", "IT", "LTU", "LUXE", "NETH", "POL", "POR", "ROM", "SVN", "SPN", "SWED"] {
            assert!(listed.contains(code) && country_of_owner(code) == Country::Eu, "{code}");
        }
        assert_eq!(country_of_owner("UK"), Country::Uk);
        assert_eq!(country_of_owner("US"), Country::Us);
        assert_eq!(country_of_owner("PRC"), Country::China);
        assert_eq!(country_of_owner("CIS"), Country::Russia);
        assert_eq!(country_of_owner("JPN"), Country::Japan);
        assert_eq!(country_of_owner("IND"), Country::India);
        for code in ["SWTZ", "NOR", "ISRA", "XYZ", ""] {
            assert_eq!(country_of_owner(code), Country::Other, "{code:?}");
        }
        assert_eq!(owner_line(Some("FR")), "Owner: France (EU)");
        assert_eq!(owner_line(Some("UK")), "Owner: United Kingdom (UK)");
        assert_eq!(owner_line(Some("NOR")), "Owner: NOR (Other)");
        assert_eq!(owner_line(None), "Owner unknown");
        assert_eq!(country_of_owner("PRES"), Country::Other);
    }

    /// The SATCAT joins by NORAD id; an object without a record is Other,
    /// except a Starlink (SpaceX, US).
    #[test]
    fn the_satcat_joins_by_norad_id_and_classes_every_object() {
        let cat = stations();
        let iss = cat.index_of(ISS_NORAD_ID).unwrap();
        let body = format!(r#"[{{"NORAD_CAT_ID":{ISS_NORAD_ID},"OWNER":"ISS"}},{{"NORAD_CAT_ID":999999,"OWNER":"PRC"}},{{"OWNER":"US"}}]"#);
        let satcat = parse_satcat(body.as_bytes()).unwrap();
        assert_eq!(satcat.len(), 2, "a record without an id is skipped");
        let owners = owners_for(&cat, Some(&satcat));
        assert_eq!(owners[iss].as_deref(), Some("ISS"));
        assert_eq!(owners.iter().filter(|o| o.is_some()).count(), 1);
        let countries = countries_for(&cat, &owners);
        assert!(countries.iter().all(|&c| c == Country::Other), "ISS partners and unknowns are Other");
        assert!(parse_satcat(b"{}").is_err());
        let lenient = parse_satcat(br#"[{"NORAD_CAT_ID":"x","OWNER":"US"},{"NORAD_CAT_ID":5,"OWNER":7},{"NORAD_CAT_ID":6,"OWNER":" "},{"NORAD_CAT_ID":-1,"OWNER":"US"},{"NORAD_CAT_ID":7,"OWNER":"UK"}]"#).unwrap();
        assert_eq!(lenient.len(), 1, "bad rows skipped, the good one kept");
        assert_eq!(lenient.get(&7).map(|s| &**s), Some("UK"));
        let loaded = Loaded::new(Catalog::build(parse_omm(include_bytes!("../../tests/fixtures/orbit/stations.json")).unwrap().records, &Membership::default()), [true; GROUP_COUNT], 0, false, Some(body.as_bytes()));
        assert!(loaded.has_owners());
        assert_eq!(loaded.stats.len(), loaded.catalog.objects.len());
    }

    const ISS_NORAD_ID: u32 = gpui_maps::orbit::catalog::ISS_NORAD;

    /// Perigee and apogee from the mean elements: the ISS near 415/420 km,
    /// a GEO satellite at 35,786, a Molniya near 500 / 39,000.
    #[test]
    fn perigee_and_apogee_come_from_the_mean_elements() {
        let cat = stations();
        let iss = &cat.objects[cat.index_of(ISS_NORAD_ID).unwrap()];
        let s = ObjStats::of(iss, Country::Other);
        assert!((380.0..450.0).contains(&s.perigee_km) && (380.0..450.0).contains(&s.apogee_km), "{s:?}");
        let with = |mean_motion: f64, e: f64| {
            let mut r = iss.record.clone();
            r.mean_motion = mean_motion;
            r.eccentricity = e;
            propagate::derived(&r)
        };
        let geo = with(1.002_737_8, 0.0);
        assert!((geo.perigee_km - 35_786.0).abs() < 10.0, "{geo:?}");
        let molniya = with(2.005_6, 0.737);
        assert!((300.0..800.0).contains(&molniya.perigee_km) && (38_500.0..40_500.0).contains(&molniya.apogee_km), "{molniya:?}");
    }

    fn orbit(group: Group, country: Country, per: f64, apo: f64, e: f64, inc: f64) -> ObjStats {
        ObjStats::from_orbit(group, country, per, apo, e, inc)
    }

    /// An orbit is in an altitude range when [perigee, apogee] overlaps it
    /// (a Molniya is in LEO, MEO and GEO alike); the presets; HEO by shape.
    #[test]
    fn the_altitude_filter_is_by_orbit_overlap_and_the_presets_cut_it() {
        let iss = orbit(Group::Stations, Country::Other, 415.0, 420.0, 0.0005, 51.6);
        let gps = orbit(Group::Gps, Country::Us, 20_150.0, 20_250.0, 0.005, 55.0);
        let geo = orbit(Group::Geo, Country::Eu, 35_780.0, 35_795.0, 0.0002, 0.05);
        let molniya = orbit(Group::Other, Country::Russia, 500.0, 39_000.0, 0.737, 63.4);
        let set = [iss, gps, geo, molniya];
        let on = |p: AltPreset| {
            let (alt_km, heo_only) = p.range();
            filter_mask(&set, &Filters { alt_km, heo_only, ..Filters::default() })
        };
        assert_eq!(on(AltPreset::All), vec![true; 4]);
        assert_eq!(on(AltPreset::Leo), vec![true, false, false, true]);
        assert_eq!(on(AltPreset::Meo), vec![false, true, false, true]);
        assert_eq!(on(AltPreset::Geo), vec![false, false, true, true]);
        assert_eq!(on(AltPreset::Heo), vec![false, false, false, true]);
        assert!(overlaps(100.0, 200.0, 200.0, 300.0) && !overlaps(100.0, 199.9, 200.0, 300.0));
        let f = Filters { alt_km: AltPreset::Meo.range().0, ..Filters::default() };
        assert_eq!(AltPreset::of(&f), Some(AltPreset::Meo));
        assert_eq!(AltPreset::of(&Filters { alt_km: (300.0, 900.0), ..Filters::default() }), None);
        // The log slider round-trips inside its ends; the ends are unbounded.
        assert!((frac_to_alt(alt_to_frac(20_200.0)) - 20_200.0).abs() < 1.0);
        assert_eq!((frac_to_alt(0.0), frac_to_alt(1.0)), (0.0, f64::INFINITY));
    }

    /// One mask for every filter: chips, countries, altitude, inclination.
    #[test]
    fn every_filter_combines_into_one_mask() {
        let set = [
            orbit(Group::Starlink, Country::Us, 540.0, 560.0, 0.0001, 53.0),
            orbit(Group::Galileo, Country::Eu, 23_200.0, 23_250.0, 0.0002, 56.0),
            orbit(Group::Beidou, Country::China, 35_700.0, 35_860.0, 0.001, 55.0),
            orbit(Group::Other, Country::Uk, 700.0, 720.0, 0.001, 98.0),
        ];
        let mut f = Filters::default();
        f.countries[Country::Us as usize] = false;
        assert_eq!(filter_mask(&set, &f), vec![false, true, true, true]);
        f.groups[Group::Beidou as usize] = false;
        assert_eq!(filter_mask(&set, &f), vec![false, true, false, true]);
        f.incl_deg = (90.0, 180.0);
        assert_eq!(filter_mask(&set, &f), vec![false, false, false, true], "only the sun-synchronous one");
        f.alt_km = (0.0, 500.0);
        assert_eq!(filter_mask(&set, &f), vec![false; 4]);
        let lines = filter_summaries(&f);
        assert_eq!(lines, vec!["Type: all but BeiDou", "Country: all but US", "Alt: 0-500 km", "Incl: 90-180°"], "{lines:?}");
        assert_eq!(shown_line(0, 16_556), "0 of 16,556 shown");
        assert!(filters_active(&f) && !filters_active(&Filters::default()));
        // The mask is exactly the AND of the four dimensions.
        let mut f = Filters::default();
        f.groups[Group::Other as usize] = false;
        f.countries[Country::China as usize] = false;
        f.incl_deg = (50.0, 60.0);
        for s in &set {
            assert_eq!(f.passes(s), f.pass_group(s) && f.pass_country(s) && f.pass_alt(s) && f.pass_incl(s));
        }
        assert_eq!(filter_mask(&set, &f), vec![true, true, false, false]);
    }

    /// A fixture with a known cross-tabulation (type x country, with
    /// altitudes and inclinations): each chip counts its own class under
    /// every OTHER dimension's filter, never its own; China AND the
    /// navigation types is exactly the BeiDou count.
    fn cross_fixture() -> Vec<ObjStats> {
        let mut v = Vec::new();
        let mut add = |n: usize, g: Group, c: Country, per: f64, apo: f64, inc: f64| {
            for _ in 0..n {
                v.push(orbit(g, c, per, apo, 0.001, inc));
            }
        };
        add(10, Group::Starlink, Country::Us, 540.0, 560.0, 53.0);
        add(4, Group::Gps, Country::Us, 20_150.0, 20_250.0, 55.0);
        add(3, Group::Glonass, Country::Russia, 19_100.0, 19_150.0, 64.8);
        add(2, Group::Galileo, Country::Eu, 23_200.0, 23_250.0, 56.0);
        add(5, Group::Beidou, Country::China, 21_500.0, 21_560.0, 55.0);
        add(2, Group::Geo, Country::China, 35_780.0, 35_795.0, 0.1);
        add(1, Group::Stations, Country::Us, 410.0, 420.0, 51.6);
        add(1, Group::Stations, Country::China, 380.0, 390.0, 41.5);
        add(1, Group::Other, Country::Japan, 600.0, 620.0, 97.8);
        v
    }

    #[test]
    fn a_chip_counts_its_class_under_every_other_dimension() {
        let set = cross_fixture();
        let by_group = |s: &ObjStats| s.group as usize;
        // Nothing filtered: the marginals.
        let x = cross_counts(&set, &Filters::default(), GROUP_COUNT, by_group);
        assert_eq!((x.shown, x.total), (29, 29));
        assert_eq!(x.groups[Group::Starlink as usize], 10);
        assert_eq!(x.countries[Country::China as usize], 8);
        assert_eq!(x.countries[Country::Us as usize], 15);
        // China only: the type chips count China's objects; the country
        // chips are untouched by their own filter.
        let mut f = Filters { countries: [false; COUNTRY_COUNT], ..Filters::default() };
        f.countries[Country::China as usize] = true;
        let x = cross_counts(&set, &f, GROUP_COUNT, by_group);
        assert_eq!(x.groups[Group::Beidou as usize], 5);
        assert_eq!(x.groups[Group::Geo as usize], 2);
        assert_eq!(x.groups[Group::Stations as usize], 1);
        assert_eq!(x.groups[Group::Starlink as usize], 0, "a zero chip: greyed");
        assert_eq!(x.countries[Country::Us as usize], 15, "a chip's own dimension never narrows it");
        assert_eq!(x.shown, 8);
        // China AND the navigation types = BeiDou.
        f.groups = [false; GROUP_COUNT];
        for g in [Group::Gps, Group::Galileo, Group::Glonass, Group::Beidou] {
            f.groups[g as usize] = true;
        }
        let x = cross_counts(&set, &f, GROUP_COUNT, by_group);
        assert_eq!(x.shown, 5);
        assert_eq!(x.shown, x.groups[Group::Beidou as usize]);
        assert_eq!(x.countries[Country::China as usize], 5);
        assert_eq!((x.countries[Country::Us as usize], x.countries[Country::Russia as usize], x.countries[Country::Eu as usize]), (4, 3, 2));
        assert_eq!(x.groups[Group::Gps as usize], 0, "GPS is on but no Chinese GPS exists");
        // Every count equals a brute-force AND over the other dimensions.
        let mut f = Filters::default();
        f.groups[Group::Starlink as usize] = false;
        f.countries[Country::Russia as usize] = false;
        f.alt_km = (0.0, 2_000.0);
        f.incl_deg = (40.0, 60.0);
        let x = cross_counts(&set, &f, COUNTRY_COUNT, |s| s.country as usize);
        for g in GROUPS {
            let want = set.iter().filter(|s| s.group == g && f.pass_country(s) && f.pass_alt(s) && f.pass_incl(s)).count();
            assert_eq!(x.groups[g as usize] as usize, want, "{g:?}");
        }
        for c in COUNTRIES {
            let want = set.iter().filter(|s| s.country == c && f.pass_group(s) && f.pass_alt(s) && f.pass_incl(s)).count();
            assert_eq!(x.countries[c as usize] as usize, want, "{c:?}");
        }
        assert_eq!(x.mask, filter_mask(&set, &f), "the mask cross_counts emits matches Filters::passes exactly");
        assert_eq!(x.shown as usize, x.mask.iter().filter(|&&v| v).count());
        // The altitude chart: everything but altitude, the in-range part
        // a subset of it.
        let ctx: u32 = x.alt.iter().flatten().sum();
        let inside: u32 = x.alt_in.iter().flatten().sum();
        assert_eq!(ctx as usize, set.iter().filter(|s| f.pass_group(s) && f.pass_country(s) && f.pass_incl(s)).count());
        assert_eq!(inside, x.shown);
        assert!(x.alt.iter().zip(&x.alt_in).all(|(a, b)| a.iter().zip(b).all(|(a, b)| b <= a)));
        let inc_ctx: u32 = x.incl.iter().flatten().sum();
        assert_eq!(inc_ctx as usize, set.iter().filter(|s| f.pass_group(s) && f.pass_country(s) && f.pass_alt(s)).count());
        assert_eq!(x.incl_in.iter().flatten().sum::<u32>(), x.shown);
    }

    /// A brushed altitude range moves the chip counts and the other chart,
    /// and not its own context; the colouring restacks the charts and
    /// leaves every count and the mask as they were.
    #[test]
    fn a_brush_moves_the_counts_and_colour_by_moves_nothing_but_the_stacks() {
        let set = cross_fixture();
        let by_group = |s: &ObjStats| s.group as usize;
        let open = cross_counts(&set, &Filters::default(), GROUP_COUNT, by_group);
        let leo = Filters { alt_km: AltPreset::Leo.range().0, ..Filters::default() };
        let x = cross_counts(&set, &leo, GROUP_COUNT, by_group);
        assert_eq!(x.groups[Group::Gps as usize], 0);
        assert_eq!(x.groups[Group::Starlink as usize], 10);
        assert_eq!(x.countries[Country::China as usize], 1, "the Chinese station only");
        assert_eq!(x.alt, open.alt, "the brushed chart keeps its context");
        assert_ne!(x.incl, open.incl, "the other chart follows the brush");
        assert_eq!(x.shown, 13);
        let by_country = cross_counts(&set, &leo, COUNTRY_COUNT, |s| s.country as usize);
        assert_eq!((by_country.groups, by_country.countries, by_country.shown), (x.groups, x.countries, x.shown));
        assert_eq!(by_country.alt[0].len(), COUNTRY_COUNT);
        assert_eq!(x.mask, filter_mask(&set, &leo), "one pass computes both the counts and the mask");
    }

    #[test]
    fn a_dimension_summarises_in_a_few_words() {
        let names = ["A", "B", "C", "D", "E"];
        assert_eq!(chips_summary(&[true; 5], &names), None);
        assert_eq!(chips_summary(&[false; 5], &names).as_deref(), Some("none"));
        assert_eq!(chips_summary(&[true, false, true, true, true], &names).as_deref(), Some("all but B"));
        assert_eq!(chips_summary(&[false, true, false, true, false], &names).as_deref(), Some("B, D"));
        assert_eq!(chips_summary(&[true, true, true, false, false], &names).as_deref(), Some("A, B, C"));
        let mut f = Filters { countries: [false; COUNTRY_COUNT], alt_km: AltPreset::Leo.range().0, ..Filters::default() };
        f.countries[Country::China as usize] = true;
        f.groups[Group::Starlink as usize] = false;
        assert_eq!(filter_summaries(&f), vec!["Type: all but Starlink", "Country: China", "Alt: LEO"]);
        assert_eq!(filter_summary_line(&f), "Type: all but Starlink · Country: China · Alt: LEO");
        assert_eq!(filter_summary_line(&Filters::default()), NO_FILTER_SUMMARY, "the line is always there, neutral when nothing is filtered");
    }

    /// Histogram bins count the per-object bins computed at load, stacked
    /// by class; the brush moves the nearer handle, then the one it holds.
    #[test]
    fn the_histograms_count_their_bins_and_the_brush_moves_a_handle() {
        let set = [
            orbit(Group::Starlink, Country::Us, 540.0, 560.0, 0.0001, 53.0),
            orbit(Group::Starlink, Country::Us, 545.0, 555.0, 0.0001, 53.1),
            orbit(Group::Gps, Country::Us, 20_150.0, 20_250.0, 0.005, 55.0),
            orbit(Group::Geo, Country::Eu, 35_780.0, 35_795.0, 0.0002, 0.05),
        ];
        let h = histogram(&set, &Filters::default(), true, GROUP_COUNT, |s| s.group as usize);
        assert_eq!(h.len(), ALT_BINS);
        let bin = |km: f64| ((alt_to_frac(km) * ALT_BINS as f32) as usize).min(ALT_BINS - 1);
        assert_eq!(h[bin(550.0)][Group::Starlink as usize], 2);
        assert_eq!(h[bin(20_200.0)][Group::Gps as usize], 1);
        assert_eq!(h[bin(35_787.5)][Group::Geo as usize], 1);
        assert_eq!(h.iter().flatten().sum::<u32>(), 4);
        let i = histogram(&set, &Filters::default(), false, COUNTRY_COUNT, |s| s.country as usize);
        assert_eq!(i[(53.0 / 5.0) as usize][Country::Us as usize], 2);
        assert_eq!(i[0][Country::Eu as usize], 1);
        assert_eq!(class_counts(&set, &Filters::default(), COUNTRY_COUNT, |s| s.country as usize)[Country::Us as usize], 3);
        // Brushing: the nearer handle, then the held one; crossing swaps.
        assert_eq!(brush((0.2, 0.8), 0.3, None), ((0.3, 0.8), true));
        assert_eq!(brush((0.2, 0.8), 0.7, None), ((0.2, 0.7), false));
        assert_eq!(brush((0.2, 0.8), 0.9, Some(true)), ((0.8, 0.9), false));
        // A brushed altitude range sets the mask.
        let (sel, _) = brush((0.0, 1.0), alt_to_frac(2_000.0), Some(false));
        let f = Filters { alt_km: (frac_to_alt(sel.0), frac_to_alt(sel.1)), ..Filters::default() };
        assert_eq!(filter_mask(&set, &f), vec![true, true, false, false]);
    }

    #[test]
    fn the_glow_style_uses_neon_colours_and_the_flat_one_the_old() {
        let cat = stations();
        let flat = style_for(&cat);
        assert_eq!((flat.look, flat.trail), (AltitudeLook::Flat, None));
        assert_eq!(flat.palette[1], group_rgba(Group::Starlink, 1.0), "flat keeps the catalogue colours");
        let glow = style_for_look(&cat, AltitudeLook::Glow, true);
        assert_eq!((glow.look, glow.trail), (AltitudeLook::Glow, Some(TRAIL)));
        assert_eq!(look_hex(AltitudeLook::Glow, Group::Gps), format!("#{:06x}", NEON_COLOURS[2]));
        assert!(glow.glow_gain[Group::Starlink as usize] < 1.0 && glow.glow_gain[Group::Geo as usize] == 1.0);
    }

    #[test]
    fn the_night_basemap_turns_light_ground_dark_and_dark_text_light() {
        let style = br##"{"version":8,"sources":{},"layers":[
            {"id":"background","type":"background","paint":{"background-color":"#f8f4f0"}},
            {"id":"water","type":"fill","paint":{"fill-color":"hsl(195,17%,78%)"}},
            {"id":"label","type":"symbol","layout":{"text-font":["Noto Sans Regular"]},"paint":{"text-color":["interpolate",["linear"],["zoom"],3,"#000",8,"#333"],"text-halo-color":"rgba(255,255,255,0.7)"}}]}"##;
        let out: serde_json::Value = serde_json::from_slice(&darken_style_json(style).unwrap()).unwrap();
        let lum = |v: &serde_json::Value| {
            let c = gpui_maps::style_spec::color::parse_css_color(v.as_str().unwrap()).unwrap();
            0.3 * c.r + 0.59 * c.g + 0.11 * c.b
        };
        let layers = out["layers"].as_array().unwrap();
        let land = lum(&layers[0]["paint"]["background-color"]);
        let water = lum(&layers[1]["paint"]["fill-color"]);
        assert!(land < 0.15 && water < land, "land {land} water {water}");
        assert!(lum(&layers[2]["paint"]["text-color"][4]) > 0.6, "black text turns light");
        assert!(lum(&layers[2]["paint"]["text-halo-color"]) < 0.15);
        assert!(layers[2]["paint"]["text-halo-color"].as_str().unwrap().ends_with("0.7)"), "alpha kept");
        assert_eq!(layers[2]["layout"]["text-font"][0], "Noto Sans Regular", "layout untouched");
        assert!(darken_style_json(b"[1]").is_none());
    }

    #[test]
    fn the_real_earth_style_is_imagery_under_faint_borders() {
        let bytes = std::fs::read(concat!(concat!(env!("CARGO_MANIFEST_DIR"), "/.."), "/demos/positron-style.json")).unwrap();
        let out = real_earth_style_json(&bytes).expect("a style");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let layers = v["layers"].as_array().unwrap();
        assert_eq!(layers[0]["type"], "background");
        assert_eq!((layers[1]["type"].as_str(), layers[1]["source"].as_str()), (Some("raster"), Some("earth")));
        assert!(layers[2..].iter().any(|l| l["type"] == "fill" && l["paint"]["fill-color"] == REAL_EARTH_WATER), "the sea is darkened");
        assert!(layers[2..].iter().any(|l| l["type"] == "line"), "borders kept");
        assert!(layers[2..].iter().all(|l| (l["type"] == "fill" && l["id"].as_str().unwrap().starts_with("water"))
            || (l["type"] == "line" && l["id"].as_str().unwrap().contains("boundary"))));
        assert_eq!(v["sources"]["earth"]["type"], "raster");
        let style = gpui_maps::style_spec::parse_and_compile(&out).expect("it compiles");
        assert!(style.layers.iter().any(|l| matches!(l, gpui_maps::style::StyleLayer::Raster { .. })));
        assert_eq!(Earth::Night.next(), Earth::Real);
        assert_eq!(Earth::Real.next(), Earth::Streets);
        assert_eq!(REAL_EARTH_WATER, "rgba(47, 111, 168, 0.85)", "a lit ocean blue");
    }

    /// Over a light Earth the glow (a screen blend) would wash out: the
    /// dots, trails and rings draw Flat ("over") in saturated colours with
    /// a thin dark outline; the dark Earths keep the glow. The panel's
    /// theme tokens follow the Earth.
    #[test]
    fn a_light_earth_draws_over_blended_dots_and_a_light_theme() {
        let cat = stations();
        for earth in [Earth::Streets, Earth::Map] {
            let s = style_for_earth(&cat, AltitudeLook::Glow, true, earth);
            assert_eq!(s.look, AltitudeLook::Flat, "{earth:?}");
            assert!(s.trail.is_some(), "trails kept, over-blended");
            assert!(!gpui_maps::render::altitude_pipeline::trails_screen_blended(s.look));
            assert_eq!(s.outline, LIGHT_OUTLINE);
            assert_eq!(s.palette[Group::Starlink as usize], scene_rgba(AltitudeLook::Glow, earth, Group::Starlink, 1.0));
            assert_eq!(scene_hex(AltitudeLook::Glow, earth, Group::Gps), format!("#{:06x}", LIGHT_COLOURS[Group::Gps as usize]));
            assert_eq!(earth.theme(), Theme::LIGHT);
            let by_country = style_by_country(&cat, &vec![Country::China; cat.objects.len()], AltitudeLook::Glow, false, earth);
            assert_eq!(by_country.palette[Country::China as usize], country_scene_rgba(Country::China, earth, 1.0));
            assert_ne!(by_country.palette[Country::China as usize], country_rgba(Country::China, 1.0));
        }
        for earth in [Earth::Real, Earth::Night] {
            let s = style_for_earth(&cat, AltitudeLook::Glow, true, earth);
            assert_eq!(s.look, AltitudeLook::Glow, "{earth:?}");
            assert_eq!(s.palette[Group::Gps as usize], look_rgba(AltitudeLook::Glow, Group::Gps, 1.0));
            assert_eq!(earth.theme(), Theme::DARK);
        }
        // Switching Earth switches every token that matters.
        let (d, l) = (Theme::DARK, Theme::LIGHT);
        assert!(d.dark && !l.dark);
        for (name, a, b) in [("card", d.card_a, l.card_a), ("ink", d.ink, l.ink), ("muted", d.muted, l.muted), ("accent", d.accent, l.accent), ("rule", d.rule_a, l.rule_a), ("thumb", d.thumb, l.thumb)] {
            assert_ne!(a, b, "{name}");
        }
        let lum = |v: u32| 0.3 * f64::from((v >> 16) & 0xff) + 0.59 * f64::from((v >> 8) & 0xff) + 0.11 * f64::from(v & 0xff);
        assert!(lum(d.card_a >> 8) < 40.0 && lum(d.ink) > 200.0, "dark: dark glass, pale ink");
        assert!(lum(l.card_a >> 8) > 240.0 && lum(l.ink) < 60.0, "light: white glass, dark ink");
        assert!(real_earth_style_json(b"{}").is_none());
    }

    #[test]
    fn the_glowing_track_is_three_lines_with_a_light_core() {
        let pieces = vec![vec![(0.0, 0.0), (10.0, 5.0)]];
        let flat: serde_json::Value = serde_json::from_str(&track_geojson(AltitudeLook::Flat, &pieces, "#ff4a26")).unwrap();
        assert_eq!(flat["features"].as_array().unwrap().len(), 1);
        let glow: serde_json::Value = serde_json::from_str(&track_geojson(AltitudeLook::Glow, &pieces, "#ff4a26")).unwrap();
        let f = glow["features"].as_array().unwrap();
        assert_eq!(f.len(), 3);
        assert!(f[0]["properties"]["line-width"].as_f64() > f[2]["properties"]["line-width"].as_f64());
        assert_eq!(f[2]["properties"]["line-color"], lighten_hex("#ff4a26", 0.55));
        assert_eq!(lighten_hex("#000000", 1.0), "#ffffff");
        assert_eq!(lighten_hex("nope", 0.5), "nope");
    }

    #[test]
    fn chips_format_counts_and_say_when_a_list_is_missing() {
        assert_eq!(chip_label(Group::Starlink, Some(7_112)), "Starlink 7,112");
        assert_eq!(chip_label(Group::Gps, None), "GPS: not downloaded");
        assert_eq!(thousands(11_114), "11,114");
        assert_eq!(thousands(566), "566");
        assert_eq!(thousands(1_000_000), "1,000,000");
        let chips = Chips { counts: [9, 0, 32, 0, 0, 0, 0, 0], downloaded: [true, false, true, false, false, false, false, false], ..Chips::default() };
        assert_eq!(chips.label(Group::Gps), "GPS 32");
        assert_eq!(chips.label(Group::Geo), "GEO: not downloaded");
    }

    #[test]
    fn the_scrubber_spans_a_day_either_side_and_prints_utc() {
        let now = 1_790_000_000.0;
        assert_eq!(scrub_to_unix(now, 0.0), now - SCRUB_SPAN_S);
        assert_eq!(scrub_to_unix(now, 0.5), now);
        assert_eq!(scrub_to_unix(now, 1.0), now + SCRUB_SPAN_S);
        assert_eq!(scrub_to_unix(now, 7.0), now + SCRUB_SPAN_S, "clamped");
        // The brief had 19:33:20; 1,790,000,000 is 14:13:20 UTC (`date -u -r 1790000000`).
        assert_eq!(format_utc(1_790_000_000.0), "2026-09-21 14:13:20 UTC");
        for f in [0.0f32, 0.25, 0.5, 1.0] {
            assert!((scrub_fraction(now, scrub_to_unix(now, f)) - f).abs() < 1e-6);
        }
        assert_eq!(scrub_fraction(now, now + 10.0 * SCRUB_SPAN_S), 1.0, "a scene past the bar pins the thumb");
    }

    #[test]
    fn the_iss_card_names_its_attached_modules_and_flags_old_elements() {
        let c = stations();
        let iss = &c.objects[c.index_of(ISS_NORAD).unwrap()];
        let card = card_for(iss, None, iss.record.epoch_unix + 3_600.0);
        assert!(card.title.contains("ISS (ZARYA)") && card.title.contains("25544"));
        assert!(card.attached.as_deref().unwrap().starts_with("+ 7 attached: "), "{:?}", card.attached);
        assert!(card.lines.iter().any(|l| l.contains("92.98 min")), "{:?}", card.lines);
        assert!(!card.stale);
        assert!(card_for(iss, None, iss.record.epoch_unix + 3.5 * 86_400.0).stale, "> 3 days from epoch");
    }

    /// The card's altitude is geodetic, from the drawn sphere point: the
    /// ISS's ~420 km, and "—" without a drawn state.
    #[test]
    fn the_card_reads_the_geodetic_altitude_off_the_drawn_point() {
        let c = stations();
        let iss = &c.objects[c.index_of(ISS_NORAD).unwrap()];
        assert!(card_for(iss, None, iss.record.epoch_unix).lines.contains(&"Altitude —".to_string()));
        let t = iss.record.epoch_unix;
        let st = propagate::state_at(iss, t).unwrap();
        let [x, y, z] = propagate::teme_to_ecef_km(st.position_km, gpui_maps::orbit::gmst_rad(t));
        let r = GLOBE_RADIUS_M / 1000.0;
        // ECEF (x, y, z) is the sphere frame's (z, x, y).
        let state = AltitudePointState::from_sphere(glam::DVec3::new(y / r, z / r, x / r));
        let card = card_for(iss, Some(&state), t);
        let alt = card.lines.iter().find_map(|l| l.strip_prefix("Altitude ")).unwrap();
        let km: f64 = alt.trim_end_matches(" km").parse().unwrap();
        let want = propagate::geodetic_altitude_km([x, y, z]);
        assert!((km - want).abs() <= 1.0 && (380.0..450.0).contains(&km), "{km} vs {want}");
    }

    #[test]
    fn following_the_iss_caps_zoom_near_six_on_a_900_px_view() {
        let z = follow_zoom_cap(420_000.0, 0.0, 900.0);
        assert!((6.3..6.9).contains(&z), "{z}");
        assert!(follow_zoom_cap(35_786_000.0, 0.0, 900.0) < 1.0, "GEO is followed from much further out");
    }

    #[test]
    fn hiding_the_selected_objects_group_deselects_and_ends_follow() {
        let c = stations();
        let i = c.index_of(ISS_NORAD).unwrap();
        let mut s = Selection { hovered: Some(i), selected: Some(i), following: true };
        let mut enabled = [true; GROUP_COUNT];
        enabled[Group::Stations as usize] = false;
        s.on_groups_changed(&c, enabled);
        assert_eq!((s.hovered, s.selected, s.following), (None, None, false));
        assert_eq!(visibility_mask(&c, enabled).iter().filter(|v| **v).count(), 0);
        // Another group hidden keeps it.
        let mut s = Selection { hovered: Some(i), selected: Some(i), following: true };
        enabled = [true; GROUP_COUNT];
        enabled[Group::Gps as usize] = false;
        s.on_groups_changed(&c, enabled);
        assert_eq!((s.hovered, s.selected, s.following), (Some(i), Some(i), true));
    }

    #[test]
    fn a_user_drag_ends_follow() {
        let mut s = Selection { hovered: None, selected: Some(0), following: true };
        s.on_user_camera_move();
        assert!(!s.following && s.selected == Some(0), "the selection stays; only follow ends");
        assert!(!moved_by_user((10.0, 20.0), (10.0, 20.0)));
        assert!(!moved_by_user((179.999_999_9, 0.0), (-179.999_999_9, 0.0)), "the antimeridian is not a move");
        assert!(moved_by_user((10.0, 20.0), (10.001, 20.0)));
        assert!(moved_by_user((10.0, 20.0), (10.0, 19.9)));
    }

    #[test]
    fn a_click_selects_deselects_and_ends_a_follow_of_another() {
        let mut s = Selection { hovered: None, selected: Some(3), following: true };
        s.on_click(Some(3));
        assert!(s.following, "clicking the followed object again keeps following");
        s.on_click(Some(5));
        assert_eq!((s.selected, s.following), (Some(5), false));
        s.on_click(None);
        assert_eq!(s.selected, None, "empty space deselects");
        assert!(is_click((10.0, 10.0), (12.0, 13.0)) && !is_click((10.0, 10.0), (20.0, 10.0)));
    }

    #[test]
    fn a_rebuilt_catalogue_keeps_the_selection_by_norad_id() {
        let mut s = Selection { hovered: Some(1), selected: Some(2), following: true };
        let old = [100u32, 200, 300];
        s.remap(|i| old.get(i).copied(), |n| [300u32, 100].iter().position(|&x| x == n));
        assert_eq!((s.hovered, s.selected, s.following), (None, Some(0), true));
        s.remap(|_| Some(999), |_| None);
        assert_eq!((s.selected, s.following), (None, false), "gone from the new catalogue: follow ends");
    }

    #[test]
    fn rings_and_tracks_refresh_on_their_own_cadence() {
        let p = 5_580.0;
        assert!(!ring_due(0.0, p / 8.0 - 1.0, p, 1.0 / 8.0));
        assert!(ring_due(0.0, p / 8.0, p, 1.0 / 8.0));
        assert!(ring_due(0.0, -p / 20.0, p, 1.0 / 20.0), "running backwards counts too");
        assert!(ring_due(0.0, f64::NAN, p, 1.0 / 8.0), "a NaN scene time rebuilds rather than keeps a stale ring");
    }

    #[test]
    fn only_the_enabled_chips_lists_are_refreshed() {
        assert_eq!(celestrak_groups([true; GROUP_COUNT]), vec!["active", "stations", "gps-ops", "galileo", "glo-ops", "beidou", "geo"]);
        let mut only_gps = [false; GROUP_COUNT];
        only_gps[Group::Gps as usize] = true;
        assert_eq!(celestrak_groups(only_gps), vec!["gps-ops"]);
        assert!(celestrak_groups([false; GROUP_COUNT]).is_empty());
    }

    /// Review F2: a sun-synchronous object (97.6°) at ~70°N heading over the
    /// pole. The old ±150° cut dropped the pass after the pole, 53° of arc
    /// from the object; now every sample is drawn, in one overlay source (the
    /// engine's grid reaches the whole world from any origin).
    #[test]
    fn a_polar_track_keeps_the_pass_over_the_pole() {
        let c = stations();
        let mut rec = c.objects[c.index_of(ISS_NORAD).unwrap()].record.clone();
        rec.inclination_deg = 97.6;
        rec.mean_motion = 15.2;
        rec.eccentricity = 0.001;
        let obj = gpui_maps::orbit::catalog::Catalog::build(vec![rec], &Membership::default());
        let obj = &obj.objects[0];
        // Find an instant at ~70°N, northbound.
        let lat_at = |t: f64| {
            let st = propagate::state_at(obj, t).unwrap();
            let [x, y, z] = propagate::teme_to_ecef_km(st.position_km, gpui_maps::orbit::gmst_rad(t));
            (z / (x * x + y * y + z * z).sqrt()).asin().to_degrees()
        };
        let t0 = obj.record.epoch_unix;
        let t = (0..6000).map(|k| t0 + k as f64).find(|&t| lat_at(t) > 70.0 && lat_at(t + 10.0) > lat_at(t)).expect("a northbound 70°N pass");
        let pieces = propagate::ground_track(obj, t);
        // The pass past the pole, 14 min ahead, is in the track.
        let ahead = t + 14.0 * 60.0;
        let st = propagate::state_at(obj, ahead).unwrap();
        let [x, y, _] = propagate::teme_to_ecef_km(st.position_km, gpui_maps::orbit::gmst_rad(ahead));
        let lon = y.atan2(x).to_degrees();
        assert!(pieces.iter().flatten().any(|p| (p.0 - lon).abs() < 3.0 && (p.1 - lat_at(ahead)).abs() < 3.0), "the post-pole pass at {lon:.1},{:.1} is drawn", lat_at(ahead));
    }

    /// Review F1: the ring starts half a period back, so the vertex nearest
    /// the object turns by one step's angle (2π/256), not the closing seam's
    /// jog, and the seam sits on the far side of the orbit.
    #[test]
    fn the_ring_seam_is_not_on_the_object() {
        let c = stations();
        let iss = &c.objects[c.index_of(ISS_NORAD).unwrap()];
        let t = iss.record.epoch_unix + 3.0 * 3600.0;
        let turn_at_object = |start: f64| {
            let ring = propagate::orbit_ring_teme(iss, start, 256);
            let p = glam::DVec3::from_array(propagate::state_at(iss, t).unwrap().position_km) * 1000.0;
            let n = ring.len();
            let i = (0..n).min_by(|&a, &b| ring[a].distance(p).total_cmp(&ring[b].distance(p))).unwrap();
            let (a, b, c) = (ring[(i + n - 1) % n], ring[i], ring[(i + 1) % n]);
            ((b - a).angle_between(c - b).to_degrees(), ring[0].distance(p))
        };
        let (turn, seam_m) = turn_at_object(ring_start(iss, t));
        assert!(turn < 2.5, "turn {turn:.2}° at the object (one step is 1.41°)");
        assert!(seam_m > 10_000_000.0, "the seam is {seam_m:.0} m from the object");
        let (old_turn, _) = turn_at_object(t);
        assert!(old_turn > turn, "starting at the object puts the seam on it: {old_turn:.2}° vs {turn:.2}°");
    }

    /// Final review M2: a paused, idle panel's timer slows to 4 Hz; any
    /// motion or work brings it back to 30 Hz.
    #[test]
    fn a_paused_idle_panel_ticks_slowly() {
        assert_eq!(tick_interval(0.0, false, false, false, true), TICK_IDLE, "paused and idle");
        assert!(TICK_IDLE >= 5 * TICK_ACTIVE);
        assert_eq!(tick_interval(1.0, false, false, false, true), TICK_ACTIVE, "running clock");
        assert_eq!(tick_interval(0.0, true, false, false, true), TICK_ACTIVE, "following");
        assert_eq!(tick_interval(0.0, false, true, false, true), TICK_ACTIVE, "fetch or scrub");
        assert_eq!(tick_interval(0.0, false, false, true, true), TICK_ACTIVE, "camera moving");
        assert_eq!(tick_interval(0.0, false, false, false, false), TICK_ACTIVE, "still loading");
    }

    #[test]
    fn a_paused_panel_repaints_only_for_a_change() {
        assert!(!tick_repaints(false, 0.0, true, false, false, false), "paused with a hover: still");
        assert!(tick_repaints(false, 60.0, true, false, false, false), "running: the hover card follows its dot");
        assert!(!tick_repaints(false, 1.0, false, false, false, false), "running, nothing shown, same second: the map repaints itself");
        assert!(tick_repaints(false, 1.0, false, false, false, true));
        assert!(tick_repaints(true, 0.0, false, false, false, false) && tick_repaints(false, 0.0, false, true, false, false));
    }

    #[test]
    fn week_old_elements_raise_the_staleness_chip() {
        assert!(staleness_chip(0.0, 6.0 * 86_400.0).is_none());
        assert!(staleness_chip(0.0, 8.0 * 86_400.0).unwrap().contains("8 days"));
    }

    /// Distinct colours per group (the ring and track take the object's),
    /// and the per-group sizes, GPS and GEO above the 2 px default.
    #[test]
    fn the_style_paints_each_group_its_own_colour_and_size() {
        let c = stations();
        let s = style_for(&c);
        assert_eq!(s.paint.len(), c.objects.len());
        assert!(s.paint.iter().all(|&p| p == Group::Stations as u8));
        for a in GROUPS {
            for b in GROUPS {
                if a != b {
                    assert_ne!(s.palette[a as usize], s.palette[b as usize]);
                }
            }
        }
        assert_eq!(s.base_px[Group::Starlink as usize], 1.6);
        assert!(s.base_px[Group::Gps as usize] > 2.0 && s.base_px[Group::Geo as usize] > 2.0);
        assert_eq!(group_hex(Group::Stations), "#ff5a36");
        assert_eq!(group_rgba(Group::Starlink, 0.5), Rgba { r: 0x3D as f32 / 255.0, g: 0x8B as f32 / 255.0, b: 1.0, a: 0.5 });
    }

    /// A cache with only the stations list still draws them, classified as
    /// stations; Starlink/Other/GPS/GEO say "not downloaded". No body at
    /// all, or none that parses: no catalogue.
    #[test]
    fn the_catalogue_is_built_from_whatever_lists_are_cached() {
        let stations = include_bytes!("../../tests/fixtures/orbit/stations.json").to_vec();
        let l = catalog_from_bodies(&Bodies { stations: Some(stations.clone()), ..Bodies::default() }).expect("stations alone");
        assert_eq!(l.downloaded, [true, false, false, false, false, false, false, false]);
        assert_eq!(l.catalog.group_counts(), [11, 0, 0, 0, 0, 0, 0, 0]);
        let l = catalog_from_bodies(&Bodies { active: Some(stations.clone()), ..Bodies::default() }).expect("active alone");
        assert_eq!(l.downloaded, [false, true, false, false, true, false, false, false]);
        assert_eq!(l.catalog.group_counts()[Group::Other as usize], 11);
        assert!(catalog_from_bodies(&Bodies::default()).is_none());
        assert!(catalog_from_bodies(&Bodies { active: Some(b"not json".to_vec()), ..Bodies::default() }).is_none());
        let s = synthetic_loaded();
        assert!(s.synthetic && s.catalog.objects.len() > 1_000 && s.downloaded == [true; GROUP_COUNT]);
    }
}

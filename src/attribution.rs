//! **Every credit a map owes, in one list.**
//!
//! A map is several independently licensed datasets: a basemap, imagery
//! under it, terrain, overlays over it, a data layer's own credit, the
//! opt-in space-look assets, and whatever the host adds. Each owes its own
//! credit, and a map that can show only one of the credits it owes is out
//! of compliance with the others' licences.
//!
//! This module is the shell-neutral half: the [`Credit`] type, the
//! collection rule ([`collect`]: ordered by [`CreditRole`], basemap first,
//! exact and normalised duplicates dropped) and the compact-plate rule
//! ([`needs_compact`], [`CompactState`]). It is plain `std` so the browser
//! and iOS shells `#[path]`-include it as they do `symbol_pass`; the plate's
//! layout (`symbol_pass::attribution_plate_layout`) and its draw stay with
//! the text code.
//!
//! # Dedupe, after MapLibre
//!
//! MapLibre's `AttributionControl._updateAttributions` drops an entry that
//! is a substring of another (`attribution_control.ts`). [`collect`] does
//! the same on a **normalised** form -- tags and entities out, `©`, `(c)`,
//! "copyright" and "contributors" out, case and punctuation folded, and the
//! match on whole words -- so "© OpenStreetMap" and "© OpenStreetMap
//! contributors" are one credit, and so is "U.S. Census Bureau" inside "U.S.
//! Census Bureau, 2020 P.L. 94-171". The longer credit is kept, at the place
//! the first of them held, so the order stays stable as credits arrive.

/// What a credit is for: the order credits are shown in, basemap first.
///
/// `#[non_exhaustive]`: a new kind of source may need its own place.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum CreditRole {
    /// The vector basemap (`TileSource::attribution`).
    Basemap,
    /// Raster imagery under or over the basemap.
    Imagery,
    /// Elevation data.
    Terrain,
    /// An overlay source's own data (a GeoJSON source, a dot-density or
    /// cluster archive).
    Overlay,
    /// A data credit the host states for what an overlay draws
    /// (`MapView::set_data_credit`).
    Data,
    /// Opt-in assets: the space look's star map and clouds, story images.
    Asset,
    /// Anything else the host adds (`MapView::set_credit`).
    Host,
}

/// One credit line a map owes, and what it is for.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Credit {
    /// The credit as the source states it (text; HTML tags are tolerated
    /// and ignored by the dedupe).
    pub text: String,
    /// Where it sorts: see [`CreditRole`].
    pub role: CreditRole,
}

impl Credit {
    /// A credit with `text` (trimmed) for `role`.
    #[must_use]
    pub fn new(role: CreditRole, text: impl Into<String>) -> Self {
        let text: String = text.into();
        Self { text: text.trim().to_string(), role }
    }
}

/// MapLibre's separator between credits (`attribution_control.ts` joins
/// with `' | '`), and the one `symbol_pass::stacked_attribution_lines`
/// splits on.
pub const SEPARATOR: &str = " | ";

/// The credit's words for comparison: tags and entities out, `©`, `(c)`,
/// "copyright" and "contributors" out, lower-cased, every run of
/// non-alphanumerics one space.
#[must_use]
pub fn normalise(text: &str) -> Vec<String> {
    let mut plain = String::with_capacity(text.len());
    let mut in_tag = false;
    for ch in text.chars() {
        match ch {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => plain.push(ch),
            _ => {}
        }
    }
    let plain = plain.replace("&copy;", " ").replace("&amp;", "&").replace("&nbsp;", " ").replace("(c)", " ").replace("(C)", " ");
    plain
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .filter(|w| !matches!(w.as_str(), "copyright" | "contributors" | "contributor"))
        .collect()
}

/// Whether `inner`'s words appear, in order and contiguous, in `outer`'s.
fn contains_words(outer: &[String], inner: &[String]) -> bool {
    !inner.is_empty() && outer.windows(inner.len()).any(|w| w == inner)
}

/// Every credit, in a stable order and once: sorted by [`CreditRole`]
/// (stable, so credits of one role keep the order they were given in),
/// blanks dropped, and a credit whose normalised words are inside an
/// earlier one's dropped; one that holds an earlier one's whole takes its
/// place (see the module docs).
#[must_use]
pub fn collect(credits: impl IntoIterator<Item = Credit>) -> Vec<Credit> {
    let mut all: Vec<Credit> = credits.into_iter().filter(|c| !c.text.trim().is_empty()).collect();
    all.sort_by_key(|c| c.role);
    let mut kept: Vec<(Credit, Vec<String>)> = Vec::with_capacity(all.len());
    for credit in all {
        let words = normalise(&credit.text);
        let same = |(k, kw): &(Credit, Vec<String>)| k.text == credit.text || (!words.is_empty() && contains_words(kw, &words));
        if kept.iter().any(same) {
            continue;
        }
        let holds = |(_, kw): &(Credit, Vec<String>)| contains_words(&words, kw);
        match kept.iter().position(holds) {
            Some(at) => {
                // Every credit this one holds goes; it takes the first
                // one's place (nothing before `at` is held, so `at` stands).
                kept.retain(|k| !holds(k));
                kept.insert(at.min(kept.len()), (credit, words));
            }
            None => kept.push((credit, words)),
        }
    }
    kept.into_iter().map(|(c, _)| c).collect()
}

/// The credits as one line, joined with [`SEPARATOR`]; `None` for none.
#[must_use]
pub fn join(credits: &[Credit]) -> Option<String> {
    (!credits.is_empty()).then(|| credits.iter().map(|c| c.text.as_str()).collect::<Vec<_>>().join(SEPARATOR))
}

/// The tallest the expanded plate may stand, as a fraction of the corner's
/// height, before the plate is **compact**: shown expanded until the map is
/// first moved, then collapsed to an "i" button that expands it again
/// (MapLibre's compact attribution, `attribution_control.ts`
/// `_updateCompact` / `_updateCompactMinimize`).
///
/// MapLibre switches on width (a map 640 CSS px wide or less). Its own docs
/// say a credit "should not be collapsed if it can comfortably fit", so the
/// rule here is the space the credits actually take: a plate taller than a
/// fifth of the map is one the map cannot comfortably carry.
pub const COMPACT_HEIGHT_FRACTION: f32 = 0.2;

/// A map this many logical pixels wide or less is **narrow**: its compact
/// plate starts collapsed (MapLibre's `_updateCompact`, 640 CSS px).
pub const NARROW_MAP_PX: f32 = 640.0;

/// Whether a map `logical_width` wide is narrow ([`NARROW_MAP_PX`]).
#[must_use]
pub fn is_narrow(logical_width: f32) -> bool {
    logical_width <= NARROW_MAP_PX
}

/// Whether a plate of `lines` lines is compact on a map `logical_width`
/// wide whose corner is `corner_height` tall: taller than
/// [`COMPACT_HEIGHT_FRACTION`] of it ([`needs_compact`]), more than one
/// line (a stacked credit takes the room cards need, so a long credit
/// collapses), or
/// any credit on a narrow map.
#[must_use]
pub fn compact_plate(plate_height: f32, corner_height: f32, lines: usize, logical_width: f32) -> bool {
    needs_compact(plate_height, corner_height) || lines > 1 || is_narrow(logical_width)
}

/// Whether an expanded plate `plate_height` tall is too much for a corner
/// `corner_height` tall (both in the same pixels).
#[must_use]
pub fn needs_compact(plate_height: f32, corner_height: f32) -> bool {
    plate_height > corner_height * COMPACT_HEIGHT_FRACTION
}

/// The compact plate's toggle, after MapLibre: expanded at first; the first
/// move of the map collapses it (once -- a user who reopens it keeps it
/// open), and a press on the plate or the button toggles it. Only consulted
/// while [`needs_compact`] holds; a plate that fits is always expanded.
///
/// `Default` is the state of a picture nobody can press: a capture, a story
/// frame, a burn-in. It never starts collapsed, so an exported image always
/// carries its credits in full. A map a person interacts with uses
/// [`Self::interactive`], whose plate also starts collapsed on a narrow map.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CompactState {
    collapsed: bool,
    moved: bool,
    /// A narrow map starts collapsed ([`Self::collapsed_on`]). Only for an
    /// interactive map, where a press can open it.
    narrow_start: bool,
}

impl CompactState {
    /// The state of an interactive map: as `Default`, and on a narrow map
    /// ([`is_narrow`]) the plate starts collapsed to the (i) button, as
    /// MapLibre's compact control does on a small screen.
    #[must_use]
    pub fn interactive() -> Self {
        Self { narrow_start: true, ..Self::default() }
    }

    /// Whether the plate draws collapsed, given whether it is `compact`
    /// ([`needs_compact`]) this frame.
    #[must_use]
    pub fn collapsed(&self, compact: bool) -> bool {
        compact && self.collapsed
    }

    /// Whether the plate draws collapsed on a map that may be `narrow`
    /// ([`is_narrow`]): on an interactive map ([`Self::interactive`]) a
    /// narrow map starts collapsed, a wide one expanded until the first
    /// move; a press flips what is drawn either way. A `Default` state (a
    /// capture) is expanded until moved, narrow or not.
    #[must_use]
    pub fn collapsed_on(&self, compact: bool, narrow: bool) -> bool {
        compact && (self.collapsed || self.starts_collapsed(narrow))
    }

    fn starts_collapsed(&self, narrow: bool) -> bool {
        self.narrow_start && narrow && !self.moved
    }

    /// The map moved under a `compact` plate: collapse it, the first time.
    /// Returns whether that changed what draws.
    pub fn map_moved(&mut self, compact: bool) -> bool {
        if !compact || self.moved {
            return false;
        }
        self.moved = true;
        let changed = !self.collapsed;
        self.collapsed = true;
        changed
    }

    /// A press on the plate or the button: flip it.
    pub fn toggle(&mut self) {
        self.moved = true;
        self.collapsed = !self.collapsed;
    }

    /// A press on a plate drawn on a map that is `narrow` or not: flips
    /// what is drawn ([`Self::collapsed_on`]), so the first press on a
    /// narrow map's collapsed button opens it.
    pub fn toggle_on(&mut self, narrow: bool) {
        let drawn = self.collapsed || self.starts_collapsed(narrow);
        self.moved = true;
        self.collapsed = !drawn;
    }
}

/// Whether the credit pill is drawn for a light or a dark map: a white
/// pill with dark ink, or a near-black one with light ink. MapLibre's
/// compact attribution (`.maplibregl-ctrl-attrib.maplibregl-compact`,
/// `maplibre-gl.css`) is the shape; its colours are light-only, so the dark
/// set is this engine's own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum AttributionTone {
    /// A light map, or one whose colour is not known (no background).
    #[default]
    Light,
    /// A dark map.
    Dark,
}

/// The pill's colours, sRGB-encoded RGBA in `0..=1` (straight alpha).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AttributionPalette {
    /// The pill behind the credits.
    pub plate: [f32; 4],
    /// The credits' ink.
    pub text: [f32; 4],
    /// The round (i) button's fill, inside the pill's right end.
    pub button: [f32; 4],
    /// The "i" on the button.
    pub glyph: [f32; 4],
}

/// sRGB `0xRRGGBB` and an alpha as `[r, g, b, a]` in `0..=1`.
const fn hex(rgb: u32, a: f32) -> [f32; 4] {
    [((rgb >> 16) & 0xff) as f32 / 255.0, ((rgb >> 8) & 0xff) as f32 / 255.0, (rgb & 0xff) as f32 / 255.0, a]
}

impl AttributionTone {
    /// Light: MapLibre's compact pill -- `#fff`, `#333` ink
    /// (`.maplibregl-ctrl-attrib a`'s `rgb(0 0 0 / 75%)` on white), and the
    /// button's `rgb(0 0 0 / 5%)` composited on white (`#f2f2f2`) under a
    /// grey "i". Dark: `#1d2025` at 90%, `#e8eaed` ink, a `#2a2f38` button.
    /// The button is opaque so the collapsed pill (the button alone) reads
    /// the same over any map.
    #[must_use]
    pub const fn palette(self) -> AttributionPalette {
        match self {
            AttributionTone::Light => AttributionPalette {
                plate: hex(0xffffff, 1.0),
                text: hex(0x333333, 1.0),
                button: hex(0xf2f2f2, 1.0),
                glyph: hex(0x666666, 1.0),
            },
            AttributionTone::Dark => AttributionPalette {
                plate: hex(0x1d2025, 0.9),
                text: hex(0xe8eaed, 1.0),
                button: hex(0x2a2f38, 1.0),
                glyph: hex(0xe8eaed, 1.0),
            },
        }
    }

    /// The tone for a map whose background is the sRGB colour `rgb`
    /// (`None`: unknown, so light). Dark when the background's WCAG
    /// relative luminance is below 0.179 -- the point where white text
    /// contrasts with it more than black does -- so a catalogue basemap's
    /// `basemaps::Basemap::is_dark` and its style's background agree, and a
    /// style outside the catalogue is decided the same way.
    #[must_use]
    pub fn for_background(rgb: Option<[f32; 3]>) -> Self {
        fn linear(c: f32) -> f32 {
            if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
        }
        match rgb {
            Some([r, g, b]) if 0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b) < 0.179 => Self::Dark,
            _ => Self::Light,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tone_follows_the_backgrounds_luminance() {
        assert_eq!(AttributionTone::for_background(None), AttributionTone::Light);
        assert_eq!(AttributionTone::for_background(Some([1.0, 1.0, 1.0])), AttributionTone::Light);
        assert_eq!(AttributionTone::for_background(Some([0.95, 0.94, 0.91])), AttributionTone::Light);
        assert_eq!(AttributionTone::for_background(Some([0.05, 0.05, 0.06])), AttributionTone::Dark);
        assert_eq!(AttributionTone::for_background(Some([0.2, 0.22, 0.25])), AttributionTone::Dark);
        let (light, dark) = (AttributionTone::Light.palette(), AttributionTone::Dark.palette());
        assert_eq!(light.plate, [1.0, 1.0, 1.0, 1.0]);
        assert_eq!(light.text, hex(0x333333, 1.0));
        assert_eq!(dark.plate, hex(0x1d2025, 0.9));
        assert_eq!(dark.text, hex(0xe8eaed, 1.0));
        assert_eq!(dark.button, hex(0x2a2f38, 1.0));
    }

    fn texts(credits: &[Credit]) -> Vec<&str> {
        credits.iter().map(|c| c.text.as_str()).collect()
    }

    #[test]
    fn credits_sort_by_role_basemap_first_and_keep_their_order_within_one() {
        let got = collect([
            Credit::new(CreditRole::Host, "Host"),
            Credit::new(CreditRole::Overlay, "Overlay A"),
            Credit::new(CreditRole::Imagery, "Imagery"),
            Credit::new(CreditRole::Overlay, "Overlay B"),
            Credit::new(CreditRole::Basemap, "Basemap"),
            Credit::new(CreditRole::Terrain, "Terrain"),
        ]);
        assert_eq!(texts(&got), ["Basemap", "Imagery", "Terrain", "Overlay A", "Overlay B", "Host"]);
    }

    #[test]
    fn exact_and_normalised_duplicates_are_one_credit() {
        let got = collect([
            Credit::new(CreditRole::Basemap, "© OpenStreetMap contributors"),
            Credit::new(CreditRole::Overlay, "© OpenStreetMap"),
            Credit::new(CreditRole::Overlay, "(c) openstreetmap"),
            Credit::new(CreditRole::Data, "&copy; <a href=\"x\">OpenStreetMap</a> contributors"),
            Credit::new(CreditRole::Host, "© OpenStreetMap contributors"),
        ]);
        assert_eq!(texts(&got), ["© OpenStreetMap contributors"]);
    }

    #[test]
    fn a_longer_credit_takes_the_place_of_the_one_it_holds() {
        let got = collect([
            Credit::new(CreditRole::Basemap, "© Mapbox © OpenStreetMap"),
            Credit::new(CreditRole::Overlay, "U.S. Census Bureau"),
            Credit::new(CreditRole::Data, "GHSL"),
            Credit::new(CreditRole::Data, "U.S. Census Bureau, 2020 P.L. 94-171"),
        ]);
        assert_eq!(texts(&got), ["© Mapbox © OpenStreetMap", "U.S. Census Bureau, 2020 P.L. 94-171", "GHSL"]);
        // ...and a shorter one later is inside a kept one: dropped.
        let got = collect([
            Credit::new(CreditRole::Basemap, "© Mapbox © OpenStreetMap"),
            Credit::new(CreditRole::Imagery, "© Mapbox"),
        ]);
        assert_eq!(texts(&got), ["© Mapbox © OpenStreetMap"]);
    }

    #[test]
    fn words_match_whole_not_inside_other_words() {
        let got = collect([Credit::new(CreditRole::Basemap, "Cosmos Maps"), Credit::new(CreditRole::Overlay, "OS")]);
        assert_eq!(texts(&got), ["Cosmos Maps", "OS"]);
    }

    #[test]
    fn blanks_are_dropped_and_none_joins_to_none() {
        assert!(collect([Credit::new(CreditRole::Basemap, "  ")]).is_empty());
        assert_eq!(join(&[]), None);
        let got = collect([Credit::new(CreditRole::Basemap, "A"), Credit::new(CreditRole::Imagery, "B")]);
        assert_eq!(join(&got).as_deref(), Some("A | B"));
    }

    #[test]
    fn a_compact_plate_collapses_on_the_first_move_and_toggles_after() {
        let mut s = CompactState::default();
        assert!(!s.collapsed(true), "expanded at first");
        assert!(!s.map_moved(false), "a plate that fits never collapses");
        assert!(s.map_moved(true));
        assert!(s.collapsed(true));
        assert!(!s.collapsed(false), "grown map: the plate fits, so it is expanded");
        s.toggle();
        assert!(!s.collapsed(true));
        assert!(!s.map_moved(true), "reopened by the user: stays open");
        assert!(!s.collapsed(true));
        assert!(needs_compact(30.0, 100.0) && !needs_compact(20.0, 100.0));
    }

    #[test]
    fn a_stacked_or_narrow_credit_is_compact_and_a_narrow_map_starts_collapsed() {
        assert!(!compact_plate(20.0, 800.0, 1, 1400.0), "one line on a wide map: always expanded");
        assert!(compact_plate(40.0, 800.0, 3, 1400.0), "three stacked lines take the cards' room");
        assert!(compact_plate(20.0, 800.0, 1, 600.0), "a narrow map");
        // Wide: expanded until the first move, then the (i).
        let mut s = CompactState::default();
        assert!(!s.collapsed_on(true, false));
        s.map_moved(true);
        assert!(s.collapsed_on(true, false));
        s.toggle_on(false);
        assert!(!s.collapsed_on(true, false), "a press opens it again");
        // Narrow and interactive: collapsed from the start; the first
        // press opens it.
        let mut n = CompactState::interactive();
        assert!(n.collapsed_on(true, true));
        n.toggle_on(true);
        assert!(!n.collapsed_on(true, true));
        n.toggle_on(true);
        assert!(n.collapsed_on(true, true));
        // A capture (`Default`) on a narrow frame: expanded until moved, so
        // an exported picture carries its credits in full.
        let mut c = CompactState::default();
        assert!(!c.collapsed_on(true, true), "a narrow capture draws its credits");
        c.map_moved(true);
        assert!(c.collapsed_on(true, true));
    }
}

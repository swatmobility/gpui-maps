//! The About card's text (spec §4.2): where the orbits come from, when they
//! were fetched, how old the elements are, how politely they are
//! refreshed, what propagates them and how far to trust a position.

use super::model::format_utc;

/// The data's source and holder.
pub const SOURCE: &str = "Orbits: CelesTrak (celestrak.org), data from the US Space Force (18th/19th SDS) via Space-Track.";
/// What the Starlink filter picks out.
pub const STARLINK: &str = "Starlink is SpaceX's satellite internet constellation; its orbits come from CelesTrak like the rest.";
/// The navigation family and the owners.
pub const CLASSES: &str = "Navigation chips: GPS, Galileo, GLONASS and BeiDou, from CelesTrak's gps-ops, galileo, glo-ops and beidou lists. Colour by country uses each object's SATCAT owner; the EU is one class (ESA, EUMETSAT, the EU, its 27 member states and operators domiciled there), the UK its own.";
/// The orbit filters.
pub const FILTERS: &str = "Altitude filters by orbit, not position: an object is shown when its perigee-apogee range overlaps the chosen band, so an eccentric orbit never blinks. Charts count each object once, from its mean elements.";
/// Why this panel is dressed up: the engine's own defaults are plain
/// (flat dots, no trails, the basemap as given); the space scene is the
/// documented exception.
pub const DEFAULTS: &str = "This panel is the exception to the engine's plain defaults: glowing dots, trails and real Earth imagery are its choices, all switchable; the engine draws none of them unless asked.";
/// The refresh policy (`fetch.rs`, R16).
pub const REFRESH: &str = "Refresh: at most once per group every 2 h; stops on any refusal; stops after 3 errors until you retry.";
/// The propagator and its accuracy.
pub const PROPAGATION: &str = "Propagation: sgp4 (MIT). Positions are SGP4 predictions, accurate to about 1 km near the element epoch.";
/// CelesTrak, linked from the card.
pub const CELESTRAK_URL: &str = "https://celestrak.org";

/// What the About card knows about the data on the map.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AboutFacts {
    /// When the `active` body (or, without one, the newest group body) was
    /// fetched, Unix seconds; `None` for synthetic or a given file.
    pub fetched_unix: Option<f64>,
    /// `(min, median, max)` element epochs, Unix seconds.
    pub epochs: Option<(f64, f64, f64)>,
    pub skipped: usize,
    pub synthetic: bool,
}

/// The card's paragraphs, in order.
#[must_use]
pub fn about_lines(facts: &AboutFacts) -> Vec<String> {
    let mut out = Vec::new();
    if facts.synthetic {
        out.push("SYNTHETIC data: a seeded constellation, not real satellites.".to_string());
    }
    out.push(SOURCE.to_string());
    out.push(STARLINK.to_string());
    out.push(CLASSES.to_string());
    out.push(FILTERS.to_string());
    if let Some(t) = facts.fetched_unix {
        out.push(format!("Fetched {}", format_utc(t)));
    }
    if let Some((min, median, max)) = facts.epochs {
        let day = |t: f64| format_utc(t).get(..16).map_or_else(|| format_utc(t), str::to_string);
        out.push(format!("Element epochs {} / {} / {} (min / median / max)", day(min), day(median), day(max)));
    }
    out.push(REFRESH.to_string());
    out.push(PROPAGATION.to_string());
    if facts.skipped > 0 {
        out.push(format!("Skipped records: {}", facts.skipped));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_about_card_states_source_fetch_epochs_policy_and_accuracy() {
        let facts = AboutFacts { fetched_unix: Some(1_790_000_000.0), epochs: Some((1.789e9, 1.7895e9, 1.79e9)), skipped: 50, synthetic: false };
        let lines = about_lines(&facts);
        let all = lines.join("\n");
        for s in [
            "CelesTrak (celestrak.org)",
            "US Space Force (18th/19th SDS) via Space-Track",
            "Fetched 2026-09-21 14:13:20 UTC",
            "Element epochs 2026-09-10 ",
            "at most once per group every 2 h",
            "stops after 3 errors until you retry",
            "sgp4 (MIT)",
            "Starlink is SpaceX's satellite internet constellation",
            "accurate to about 1 km near the element epoch",
            "Skipped records: 50",
        ] {
            assert!(all.contains(s), "{s} in\n{all}");
        }
        assert!(!all.contains("SYNTHETIC"));
        let bare = about_lines(&AboutFacts { synthetic: true, ..AboutFacts::default() }).join("\n");
        assert!(bare.contains("SYNTHETIC") && !bare.contains("Skipped") && !bare.contains("Fetched"), "{bare}");
    }
}

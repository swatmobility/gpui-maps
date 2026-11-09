//! A collection's data licence, classified for what a host may do with it.
//!
//! STAC's `license` is an SPDX id, an SPDX expression, `other` (with a
//! `rel: license` link to the terms) or, in older catalogs, `proprietary`
//! or `various`. The classes follow the policy in the Portolan research
//! (agentdocs/research/2026-10-24-valdrada-portolan.md §5):
//!
//! - [`LicenceClass::PublicDomain`] (CC0, PDDL, DL-DE-ZERO) and
//!   [`LicenceClass::Attribution`] (CC-BY, DL-DE-BY, OGL, etalab, NLOD)
//!   show with the producer's credit.
//! - [`LicenceClass::ShareAlike`] (CC-BY-SA, ODbL) shows too; an export
//!   or a published derivative keeps the licence.
//! - [`LicenceClass::NonCommercial`] and [`LicenceClass::Other`] are kept
//!   out of a curated listing until a person has read the terms
//!   ([`Licence::curated_ok`]). `Other` covers `other`, `proprietary`,
//!   `various`, a missing licence and anything unrecognised.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LicenceClass {
    PublicDomain,
    Attribution,
    ShareAlike,
    NonCommercial,
    Other,
}

/// A licence as a collection states it, with its class.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Licence {
    /// The `license` field as written (`None` when absent).
    pub id: Option<String>,
    pub class: LicenceClass,
    /// The `rel: license` link (the terms), when there is one.
    pub terms_url: Option<String>,
}

impl Licence {
    pub fn new(id: Option<&str>, terms_url: Option<String>) -> Licence {
        Licence { id: id.map(str::to_string), class: classify(id), terms_url }
    }

    /// Whether a curated listing (a Gallery card) may show it without a
    /// person having read the terms first.
    pub fn curated_ok(&self) -> bool {
        matches!(self.class, LicenceClass::PublicDomain | LicenceClass::Attribution | LicenceClass::ShareAlike)
    }

    /// The chip's text: the SPDX id, or "other" / "no licence".
    pub fn label(&self) -> String {
        match &self.id {
            Some(id) if !id.trim().is_empty() => id.trim().to_string(),
            _ => "no licence".into(),
        }
    }
}

/// The class of a `license` value.
pub fn classify(id: Option<&str>) -> LicenceClass {
    let Some(id) = id.map(str::trim).filter(|s| !s.is_empty()) else { return LicenceClass::Other };
    let u = id.to_ascii_uppercase();
    // An SPDX expression: the most restrictive part decides (`OR` would
    // allow the more open one, but a curated listing does not choose).
    if u.contains(" AND ") || u.contains(" OR ") || u.contains(" WITH ") {
        let parts = u.split([' ', '(', ')']).filter(|p| !p.is_empty() && !matches!(*p, "AND" | "OR" | "WITH"));
        return parts.map(|p| classify(Some(p))).max_by_key(|c| rank(*c)).unwrap_or(LicenceClass::Other);
    }
    if u.contains("-NC") {
        return LicenceClass::NonCommercial;
    }
    if u.starts_with("CC0") || u == "PDDL-1.0" || u.starts_with("DL-DE-ZERO") || u == "UNLICENSE" || u == "PUBLIC-DOMAIN" {
        return LicenceClass::PublicDomain;
    }
    if u.starts_with("CC-BY-SA") || u.starts_with("ODBL") || u.starts_with("CC-BY-ND") {
        // ND is not share-alike, but it restricts derivatives just as much:
        // a curated map of it is a display, never a derived database.
        return LicenceClass::ShareAlike;
    }
    if u.starts_with("CC-BY") || u.starts_with("DL-DE-BY") || u.starts_with("OGL") || u.starts_with("ETALAB") || u.starts_with("NLOD") || u.starts_with("ODC-BY") || u.starts_with("CDLA-PERMISSIVE") || u == "APACHE-2.0" || u == "MIT" {
        return LicenceClass::Attribution;
    }
    LicenceClass::Other
}

fn rank(c: LicenceClass) -> u8 {
    match c {
        LicenceClass::PublicDomain => 0,
        LicenceClass::Attribution => 1,
        LicenceClass::ShareAlike => 2,
        LicenceClass::NonCommercial => 3,
        LicenceClass::Other => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_research_tables_licences_classify() {
        for (id, class) in [
            ("CC0-1.0", LicenceClass::PublicDomain),
            ("CC-BY-4.0", LicenceClass::Attribution),
            ("DL-DE-ZERO-2.0", LicenceClass::PublicDomain),
            ("DL-DE-BY-2.0", LicenceClass::Attribution),
            ("CC-BY-SA-4.0", LicenceClass::ShareAlike),
            ("ODbL-1.0", LicenceClass::ShareAlike),
            ("CC-BY-NC-4.0", LicenceClass::NonCommercial),
            ("other", LicenceClass::Other),
            ("proprietary", LicenceClass::Other),
            ("various", LicenceClass::Other),
            ("OGL-UK-3.0", LicenceClass::Attribution),
            ("CC-BY-4.0 AND ODbL-1.0", LicenceClass::ShareAlike),
            ("CC0-1.0 OR CC-BY-NC-4.0", LicenceClass::NonCommercial),
        ] {
            assert_eq!(classify(Some(id)), class, "{id}");
        }
        assert_eq!(classify(None), LicenceClass::Other);
        assert_eq!(classify(Some("  ")), LicenceClass::Other);
    }

    #[test]
    fn other_and_non_commercial_stay_out_of_a_curated_listing() {
        assert!(Licence::new(Some("CC0-1.0"), None).curated_ok());
        assert!(Licence::new(Some("CC-BY-4.0"), None).curated_ok());
        assert!(!Licence::new(Some("other"), Some("https://www.pdok.nl/gebruiksvoorwaarden".into())).curated_ok());
        assert!(!Licence::new(Some("CC-BY-NC-4.0"), None).curated_ok());
        assert!(!Licence::new(None, None).curated_ok());
        assert_eq!(Licence::new(None, None).label(), "no licence");
    }
}

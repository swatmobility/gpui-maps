//! The census hover popup: what it shows, and when. The page's DOM only
//! places the card; every word in it comes from [`popup_json`].

use std::collections::HashMap;

/// The picked block group's row, as the popup reads it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HoverInfo {
    pub geoid: u64,
    pub income: Option<u32>,
    pub aland_km2: f32,
}

#[derive(Default)]
pub struct HoverTracker {
    last: Option<HoverInfo>,
}

impl HoverTracker {
    /// What the popup shows now: `None` (hide) while the camera moves or
    /// off every block group.
    pub fn show(&mut self, picked: Option<HoverInfo>, camera_moving: bool) -> Option<HoverInfo> {
        self.last = if camera_moving { None } else { picked };
        self.last
    }
}

/// `12345` -> `"12,345"`. The one grouping both shells print with.
pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// The popup's four lines as JSON: `title`, `subtitle`, `income` ("No
/// estimate" for a block group without one, never "$0"), `aland`, plus the
/// 12-digit `geoid`.
/// The popup's income line, the same in both shells:
/// `"Median household income: $87,250"`, or `"No estimate"` -- never
/// `"$0"` -- for a block group the ACS gave none.
pub fn income_line(income: Option<f64>) -> String {
    income.map_or_else(|| "No estimate".to_string(), |v| format!("Median household income: ${}", thousands(v.round() as u64)))
}

/// The popup's land-area line: `"Land area: 0.42 km²"`, `"Land area:
/// unknown"` without a finite value. `f32` km², the pack's own type, so
/// native (which reads `ALAND` in m²) and the page round the same number.
pub fn area_line(aland_km2: Option<f32>) -> String {
    match aland_km2.filter(|v| v.is_finite()) {
        Some(km2) => format!("Land area: {km2:.2} km²"),
        None => "Land area: unknown".to_string(),
    }
}

/// The card's top-left corner for a pointer at `(x, y)`: `GAP` right of
/// and below it, flipped to the other side where the card would cross the
/// viewport's right or bottom edge. Both shells place it with this.
pub fn popup_origin(pointer: (f32, f32), card: (f32, f32), viewport: (f32, f32)) -> (f32, f32) {
    const GAP: f32 = 14.0;
    let (x, y) = pointer;
    let left = if x + GAP + card.0 > viewport.0 { x - GAP - card.0 } else { x + GAP };
    let top = if y + GAP + card.1 > viewport.1 { y - GAP - card.1 } else { y + GAP };
    (left, top)
}

/// The card's smallest line: `"GEOID 060375432021"`.
pub fn geoid_line(geoid: u64) -> String {
    format!("GEOID {geoid:012}")
}

pub fn popup_json(info: &HoverInfo, counties: &HashMap<u32, String>) -> String {
    serde_json::json!({
        "geoid": format!("{:012}", info.geoid),
        "title": crate::census::names::popup_title(info.geoid, counties),
        "subtitle": crate::census::names::popup_subtitle(info.geoid),
        "income": income_line(info.income.map(f64::from)),
        "aland": area_line(Some(info.aland_km2)),
        "geoid_line": geoid_line(info.geoid),
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(income: Option<u32>) -> HoverInfo {
        HoverInfo { geoid: 60375432021, income, aland_km2: 0.42 }
    }

    #[test]
    fn popup_for_no_data_says_no_estimate() {
        let names = std::collections::HashMap::from([(6037u32, "Los Angeles County".to_string())]);
        let json: serde_json::Value = serde_json::from_str(&popup_json(&info(None), &names)).unwrap();
        assert_eq!(json["income"], "No estimate");
        assert_eq!(json["title"], "Los Angeles County, California");
        assert_eq!(json["subtitle"], "Tract 5432.02 · Block group 1");
        assert_eq!(json["aland"], "Land area: 0.42 km²");
    }

    #[test]
    fn popup_formats_income_with_thousands() {
        let json: serde_json::Value =
            serde_json::from_str(&popup_json(&info(Some(87_250)), &Default::default())).unwrap();
        assert_eq!(json["income"], "Median household income: $87,250");
        assert_eq!(thousands(250_001), "250,001");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
    }

    #[test]
    fn hover_is_hidden_while_the_camera_moves() {
        let mut t = HoverTracker::default();
        assert!(t.show(Some(info(Some(1))), false).is_some());
        assert!(t.show(Some(info(Some(1))), true).is_none(), "moving camera: no popup");
    }

    #[test]
    fn area_is_unknown_without_a_finite_value() {
        assert_eq!(area_line(None), "Land area: unknown");
        assert_eq!(area_line(Some(f32::NAN)), "Land area: unknown");
        assert_eq!(income_line(Some(0.0)), "Median household income: $0", "a real zero is a number");
    }

    #[test]
    fn the_card_flips_at_the_right_and_bottom_edges() {
        let (card, view) = ((250.0, 122.0), (1280.0, 800.0));
        assert_eq!(popup_origin((100.0, 100.0), card, view), (114.0, 114.0));
        assert_eq!(popup_origin((1200.0, 100.0), card, view), (1200.0 - 14.0 - 250.0, 114.0));
        assert_eq!(popup_origin((100.0, 780.0), card, view), (114.0, 780.0 - 14.0 - 122.0));
    }
}

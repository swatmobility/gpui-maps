//! The census demo's income ramp and basemap-label treatment: the paint
//! both shells compile and the legend both shells draw, from one table.
//! Moved from `demos/us_block_groups.rs` so the browser shell shares it --
//! as are the range slider's snapping ([`slider_stops`], [`IncomeRange`])
//! and the paints it writes ([`range_paint_json`], [`dimmed_paint_json`]).

/// Median household income on viridis, linear from $2,500 to $250,000 --
/// the Census's own bottom and top codes (2,499 is "2,500-", 250,001 is
/// "250,000+"), so the ramp's clamps are exactly the table's.
///
/// The nine stops are matplotlib's viridis sampled at 0, 1/8, ..., 1
/// (`_cm_listed.py`'s 256-entry `_viridis_data`, index `int(v * 256)`),
/// evenly spaced over the income range. The SAME table drives the legend's
/// gradient bar ([`legend`]), so the swatch on screen and the ramp painting
/// the map can never drift apart.
pub const VIRIDIS_STOPS: [(f64, u32); 9] = [
    (2_500.0, 0x440154),
    (33_437.5, 0x472d7b),
    (64_375.0, 0x3b528b),
    (95_312.5, 0x2c728e),
    (126_250.0, 0x21918c),
    (157_187.5, 0x28ae80),
    (188_125.0, 0x5ec962),
    (219_062.5, 0xaddc30),
    (250_000.0, 0xfde725),
];

/// No-data grey -- a block group with no estimate has no `income` property
/// at all, so `["has", "income"]` is the no-data test.
pub const NO_DATA_COLOR: &str = "#9e9e9e";

/// Fill opacity. Kyle Walker's mapgl reference reads richer than the fill
/// alone can carry at 0.5 against a saturated basemap; 0.75 over Light/
/// Positron's pale grey is what gets the ramp back to that saturation
/// without the polygons going opaque and hiding the basemap entirely.
pub const FILL_OPACITY: f64 = 0.75;

/// The hairline between block groups. `fill-opacity` multiplies into
/// `fill-outline-color` too (as in MapLibre), so this alpha is the
/// hairline's OWN alpha before that multiply, not what lands on screen --
/// at [`FILL_OPACITY`] 0.75 this rgba(20,20,26,0.85) hairline lands at an
/// effective ~0.64, dark enough to read as a real border against the pale
/// basemap rather than the near-invisible 0.3 the old 0.5/0.6 pair gave.
pub const OUTLINE_COLOR: &str = "rgba(20,20,26,0.85)";

/// Builds the `fill-color`/`fill-opacity`[/`fill-outline-color`] paint
/// object from [`VIRIDIS_STOPS`], so the JSON the engine compiles and the
/// legend's gradient bar are read off one array rather than kept in sync by
/// hand.
pub fn fill_paint_json(outline: bool) -> String {
    let stops: String =
        VIRIDIS_STOPS.iter().map(|(value, color)| format!(", {value}, \"#{color:06x}\"")).collect();
    let outline_field =
        if outline { format!(",\n    \"fill-outline-color\": \"{OUTLINE_COLOR}\"") } else { String::new() };
    format!(
        r#"{{
    "fill-color": ["case", ["has", "income"],
        ["interpolate", ["linear"], ["get", "income"]{stops}],
        "{NO_DATA_COLOR}"],
    "fill-opacity": {FILL_OPACITY}{outline_field}
}}"#
    )
}


/// [`fill_paint_json`] as the object `declare_source_paint` takes.
pub fn fill_paint_object(outline: bool) -> serde_json::Map<String, serde_json::Value> {
    serde_json::from_str(&fill_paint_json(outline)).expect("the ramp's paint JSON is valid")
}

/// What the page's legend draws, as JSON: the nine stops as `#rrggbb`, in
/// order, and [`LEGEND_TICKS`] as `[label, fraction]` pairs.
pub fn ramp_json() -> String {
    let stops: Vec<String> = VIRIDIS_STOPS.iter().map(|(_, color)| format!("#{color:06x}")).collect();
    let ticks: Vec<serde_json::Value> =
        LEGEND_TICKS.iter().map(|(label, frac)| serde_json::json!([label, frac])).collect();
    serde_json::json!({ "stops": stops, "ticks": ticks }).to_string()
}

/// The legend's tick labels, and where along the bar each belongs: the
/// ramp's stops 0, 2, 4, 6 and 8 (quarters of the income range).
pub const LEGEND_TICKS: [(&str, f32); 5] =
    [("$2.5K", 0.0), ("$64.37K", 0.25), ("$126.25K", 0.5), ("$188.13K", 0.75), ("$250K", 1.0)];

/// Light's place names are pale grey (`hsl(220, 1%, 49%)` to `71%`) with a
/// 1 px white halo: made for a grey map, and unreadable over the income
/// ramp's saturated mid-tones. So the demo darkens them to near-black --
/// keeping the `symbolrank` step, each rank darkened -- and gives them a
/// white halo at 0.9 alpha, 1.75 pt wide with a 0.5 pt blur, the usual
/// data-over-basemap treatment. The engine honours all three (width and
/// blur as whole-pixel rings, `symbol_pass::halo_offsets`; the colour's
/// alpha multiplies each ring). State labels also lose most of their 0.5
/// `text-opacity`. Applied to the style JSON before it is compiled.
pub const PLACE_LABEL_LAYERS: [&str; 5] =
    ["settlement-major-label", "settlement-minor-label", "settlement-subdivision-label", "state-label", "country-label"];

pub fn readable_place_labels(style: &mut serde_json::Value) {
    use serde_json::json;
    let Some(layers) = style["layers"].as_array_mut() else { return };
    for layer in layers {
        let id = layer["id"].as_str().unwrap_or_default().to_string();
        if !PLACE_LABEL_LAYERS.contains(&id.as_str()) {
            continue;
        }
        let paint = &mut layer["paint"];
        // Ranked settlements keep the step, one darker shade per rank band.
        let ranked = paint["text-color"].as_array().is_some_and(|e| e.first() == Some(&json!("step")));
        paint["text-color"] = if ranked {
            json!(["step", ["get", "symbolrank"], "#1f1f24", 11, "#2e2e35", 16, "#43434b"])
        } else if id == "settlement-subdivision-label" {
            json!("#3a3a42")
        } else {
            json!("#1f1f24")
        };
        paint["text-halo-color"] = json!("rgba(255, 255, 255, 0.9)");
        paint["text-halo-width"] = json!(1.75);
        paint["text-halo-blur"] = json!(0.5);
        if id == "state-label" {
            paint["text-opacity"] = json!(0.85);
        }
    }
}

/// The paint for an income RANGE: inside `[lo, hi)` exactly
/// [`fill_paint_json`]'s ramp; outside it the SAME ramp at alpha 0, and
/// the no-data grey at alpha 0 too. `None` is an open end.
///
/// Why "the same colour at alpha 0" rather than one transparent colour:
/// the engine collapses features into paint CLASSES by resolved colour
/// (`overlay::plan::classify`). A hidden block group that keeps its own
/// (invisible) colour keeps its own class, so moving the range changes
/// class COLOURS and never class membership -- which is what lets
/// `MapView::update_source_paint` repaint by rewriting the source's paint
/// table alone (`SourcePaintPath::TableOnly`) instead of re-baking tiles.
/// That holds only when `lo`/`hi` fall on the ramp's own step boundaries
/// ([`slider_stops`]); anything else splits a class and re-bakes.
///
/// No-data block groups are hidden whenever a range is set: "only the
/// households inside the range" has no place for a block group with no
/// estimate. The full span ([`IncomeRange::is_full`]) is [`fill_paint_json`]
/// itself, grey included.
pub fn range_paint_json(outline: bool, lo: Option<f64>, hi: Option<f64>) -> String {
    let stops = |alpha: bool| -> String {
        VIRIDIS_STOPS
            .iter()
            .map(|(value, color)| {
                if alpha {
                    format!(", {value}, \"rgba({},{},{},0)\"", color >> 16, (color >> 8) & 0xff, color & 0xff)
                } else {
                    format!(", {value}, \"#{color:06x}\"")
                }
            })
            .collect()
    };
    let mut conds = Vec::new();
    if let Some(lo) = lo {
        conds.push(format!("[\">=\", [\"get\", \"income\"], {lo}]"));
    }
    if let Some(hi) = hi {
        conds.push(format!("[\"<\", [\"get\", \"income\"], {hi}]"));
    }
    let inside = if conds.is_empty() { "true".to_string() } else { format!("[\"all\", {}]", conds.join(", ")) };
    let (shown, hidden) = (stops(false), stops(true));
    let outline_field = if outline {
        format!(
            ",\n    \"fill-outline-color\": [\"case\", [\"!\", [\"has\", \"income\"]], \"rgba(20,20,26,0)\", {inside}, \"{OUTLINE_COLOR}\", \"rgba(20,20,26,0)\"]"
        )
    } else {
        String::new()
    };
    format!(
        r#"{{
    "fill-color": ["case", ["!", ["has", "income"]], "rgba(158,158,158,0)",
        {inside}, ["interpolate", ["linear"], ["get", "income"]{shown}],
        ["interpolate", ["linear"], ["get", "income"]{hidden}]],
    "fill-opacity": {FILL_OPACITY}{outline_field}
}}"#
    )
}

/// [`IncomeRange::paint_json`] for a state the breakdowns panel is NOT
/// pointing at: every opaque `#rrggbb` colour in `json` becomes the same
/// colour at alpha [`DIM_ALPHA`], and the outline fades with it. Colours
/// only, never the expression's shape, so every block group keeps its own
/// paint class and the swap stays a table-only update (as a slider move
/// is).
pub fn dimmed_paint_json(json: &str) -> String {
    let mut out = String::with_capacity(json.len() + 256);
    let mut rest = json;
    while let Some(at) = rest.find("\"#") {
        let hex = rest.get(at + 2..at + 8).filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit()));
        match (hex, rest.as_bytes().get(at + 8)) {
            (Some(hex), Some(b'"')) => {
                let c = u32::from_str_radix(hex, 16).expect("six hex digits");
                out.push_str(&rest[..at]);
                out.push_str(&format!("\"rgba({},{},{},{DIM_ALPHA})\"", c >> 16, (c >> 8) & 0xff, c & 0xff));
                rest = &rest[at + 9..];
            }
            _ => {
                out.push_str(&rest[..at + 1]);
                rest = &rest[at + 1..];
            }
        }
    }
    out.push_str(rest);
    out.replace(OUTLINE_COLOR, &dimmed_outline())
}

/// [`OUTLINE_COLOR`] at [`DIM_ALPHA`] of its own alpha, so fills and
/// hairlines fade together.
pub fn dimmed_outline() -> String {
    format!("rgba(20,20,26,{})", ((0.85 * DIM_ALPHA) * 1000.0).round() / 1000.0)
}

/// How much of a dimmed state's colour is left (see [`dimmed_paint_json`]).
pub const DIM_ALPHA: f64 = 0.28;

/// The incomes the range slider's handles can sit on: the ramp's two ends
/// and, between them, every boundary between two of the engine's
/// quantised ramp steps -- so a handle never splits a paint class.
///
/// Mirrors the engine's quantisation (`overlay::source_paint::RAMP_STEPS`,
/// documented there): segment `i` of the ramp gets
/// `n_i = max(1, floor(255 * w_i / W))` steps, and an input rounds to the
/// nearest step, so the boundaries sit at `(k + 0.5) / n_i` of each
/// segment. Viridis's nine even stops give 8 x 31 = 248 boundaries, 249
/// classes, 250 handle positions. Should the engine's grid ever move, the
/// slider still works -- a split class just takes the re-bake path, which
/// the demo's `--slider-sweep` counts.
pub fn slider_stops() -> &'static [f64] {
    static STOPS: std::sync::OnceLock<Vec<f64>> = std::sync::OnceLock::new();
    STOPS.get_or_init(|| {
        let (first, last) = (VIRIDIS_STOPS[0].0, VIRIDIS_STOPS[VIRIDIS_STOPS.len() - 1].0);
        let width = last - first;
        let mut stops = vec![first];
        for pair in VIRIDIS_STOPS.windows(2) {
            let (a, b) = (pair[0].0, pair[1].0);
            let n = ((255.0 * (b - a) / width).floor() as u32).max(1);
            stops.extend((0..n).map(|k| a + (b - a) * (f64::from(k) + 0.5) / f64::from(n)));
        }
        stops.push(last);
        stops
    })
}

/// A slider selection: handle positions into [`slider_stops`], `lo < hi`.
/// Selects every block group with `stops[lo] <= income < stops[hi]`, the
/// first handle's end open below and the last's open above (the Census
/// codes "2,500-" and "250,000+" sit on the ramp's clamps).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IncomeRange {
    pub lo: usize,
    pub hi: usize,
}

impl IncomeRange {
    pub fn full() -> Self {
        Self { lo: 0, hi: slider_stops().len() - 1 }
    }

    pub fn is_full(self) -> bool {
        self == Self::full()
    }

    /// `(inclusive lower, exclusive upper)` income bounds; `None` is open.
    pub fn bounds(self) -> (Option<f64>, Option<f64>) {
        let stops = slider_stops();
        ((self.lo > 0).then(|| stops[self.lo]), (self.hi < stops.len() - 1).then(|| stops[self.hi]))
    }

    /// Whether a block group with this income is drawn. No data is drawn
    /// only at the full span.
    pub fn contains(self, income: Option<f64>) -> bool {
        let Some(income) = income else { return self.is_full() };
        let (lo, hi) = self.bounds();
        lo.is_none_or(|lo| income >= lo) && hi.is_none_or(|hi| income < hi)
    }

    pub fn paint_json(self, outline: bool) -> String {
        if self.is_full() {
            return fill_paint_json(outline);
        }
        let (lo, hi) = self.bounds();
        range_paint_json(outline, lo, hi)
    }

    /// The handle position nearest `income`, clamped into the slider.
    pub fn nearest_stop(income: f64) -> usize {
        let stops = slider_stops();
        let at = stops.partition_point(|&s| s < income).min(stops.len() - 1);
        if at > 0 && (income - stops[at - 1]) < (stops[at] - income) {
            at - 1
        } else {
            at
        }
    }

    /// `"$33.4K – $126.3K"`.
    pub fn label(self) -> String {
        let stops = slider_stops();
        format!("{} – {}", dollars_k(stops[self.lo]), dollars_k(stops[self.hi]))
    }
}

impl IncomeRange {
    /// The legend's caption for this selection, as both shells print it.
    pub fn caption(self) -> String {
        if self.is_full() {
            "All incomes, and no data (grey)".to_string()
        } else {
            format!("Showing {} · no data hidden", self.label())
        }
    }
}

/// Where an income sits along the gradient bar, 0..=1.
pub fn income_fraction(income: f64) -> f64 {
    let (first, last) = (VIRIDIS_STOPS[0].0, VIRIDIS_STOPS[VIRIDIS_STOPS.len() - 1].0);
    ((income - first) / (last - first)).clamp(0.0, 1.0)
}

/// The viridis colour of `income`, linear between the ramp's nine stops,
/// as `[r, g, b, a]` in 0..=1 (alpha 1).
pub fn viridis_rgba(income: f64) -> [f32; 4] {
    let rgb = |c: u32| [((c >> 16) & 0xff) as f32, ((c >> 8) & 0xff) as f32, (c & 0xff) as f32];
    let i = VIRIDIS_STOPS.partition_point(|&(s, _)| s <= income).clamp(1, VIRIDIS_STOPS.len() - 1);
    let ((a, ca), (b, cb)) = (VIRIDIS_STOPS[i - 1], VIRIDIS_STOPS[i]);
    let t = (((income - a) / (b - a)).clamp(0.0, 1.0)) as f32;
    let (ca, cb) = (rgb(ca), rgb(cb));
    let c = |j: usize| (ca[j] + (cb[j] - ca[j]) * t) / 255.0;
    [c(0), c(1), c(2), 1.0]
}

/// `2500.0` -> `"$2.5K"`, `126_250.0` -> `"$126.3K"`.
pub fn dollars_k(income: f64) -> String {
    let k = (income / 100.0).round() / 10.0;
    if k.fract() == 0.0 { format!("${k:.0}K") } else { format!("${k:.1}K") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_paint_parses_and_carries_every_stop() {
        for outline in [false, true] {
            let paint = fill_paint_object(outline);
            assert_eq!(paint.contains_key("fill-outline-color"), outline);
            let text = fill_paint_json(outline);
            for (_, color) in VIRIDIS_STOPS {
                assert!(text.contains(&format!("\"#{color:06x}\"")), "{text}");
            }
            assert!(text.contains(NO_DATA_COLOR));
        }
    }

    #[test]
    fn the_ramp_json_is_the_stops_and_ticks_in_order() {
        let v: serde_json::Value = serde_json::from_str(&ramp_json()).unwrap();
        assert_eq!(v["stops"].as_array().unwrap().len(), 9);
        assert_eq!(v["stops"][0], "#440154");
        assert_eq!(v["stops"][8], "#fde725");
        assert_eq!(v["ticks"][2][0], "$126.25K");
        assert_eq!(v["ticks"][4][1], 1.0);
    }

    #[test]
    fn income_fraction_and_viridis_follow_the_ramp() {
        assert_eq!(income_fraction(2_500.0), 0.0);
        assert_eq!(income_fraction(250_000.0), 1.0);
        assert_eq!(income_fraction(1.0), 0.0, "clamped below");
        assert_eq!(income_fraction(126_250.0), 0.5);
        assert_eq!(viridis_rgba(2_500.0), [0x44 as f32 / 255.0, 0x01 as f32 / 255.0, 0x54 as f32 / 255.0, 1.0]);
        assert_eq!(viridis_rgba(1e9), [0xfd as f32 / 255.0, 0xe7 as f32 / 255.0, 0x25 as f32 / 255.0, 1.0]);
    }

    #[test]
    fn the_caption_names_the_range_or_says_all() {
        assert_eq!(IncomeRange::full().caption(), "All incomes, and no data (grey)");
        let r = IncomeRange { lo: 1, hi: 2 };
        assert_eq!(r.caption(), format!("Showing {} · no data hidden", r.label()));
    }

    #[test]
    fn place_labels_darken_and_halo_and_nothing_else_moves() {
        let mut style = serde_json::json!({"layers": [
            {"id": "state-label", "paint": {"text-color": "#999", "text-opacity": 0.5}},
            {"id": "settlement-major-label", "paint": {"text-color": ["step", ["get", "symbolrank"], "#aaa", 9, "#bbb"]}},
            {"id": "road-label", "paint": {"text-color": "#ccc"}}
        ]});
        readable_place_labels(&mut style);
        assert_eq!(style["layers"][0]["paint"]["text-color"], "#1f1f24");
        assert_eq!(style["layers"][0]["paint"]["text-opacity"], 0.85);
        assert_eq!(style["layers"][1]["paint"]["text-color"][0], "step");
        assert_eq!(style["layers"][1]["paint"]["text-halo-width"], 1.75);
        assert_eq!(style["layers"][2]["paint"]["text-color"], "#ccc", "not a place label");
    }
}

//! Kepler.gl's colour and size scales, compiled into the style-spec
//! expressions an overlay source's paint already takes
//! ([`crate::overlay::OverlayStore::declare_source_paint`]).
//!
//! Kepler (and d3 under it) scales a field into a colour or a radius on
//! the GPU per row. Here a scale is computed ONCE from the data on the CPU
//! -- the thresholds of a quantile scale, the bins of a quantize scale,
//! the categories of an ordinal one -- and written out as a `step`,
//! `match` or `interpolate` expression over `["get", field]`, which the
//! overlay's paint compiler evaluates per feature and collapses into one
//! draw op per colour. Pure functions, no gpui, so any shell can call
//! them.
//!
//! | Kepler `colorScale` | d3 scale | expression |
//! |---|---|---|
//! | `quantile` | `scaleQuantile` (R-7 quantiles of the sorted values) | `step` |
//! | `quantize` | `scaleQuantize` (equal-width bins over `[min, max]`) | `step` |
//! | `ordinal` | `scaleOrdinal` (sorted unique values, colours cycled) | `match` |
//! | `linear` | `scaleLinear` (evenly spaced colour stops) | `interpolate` |
//!
//! Sizes (`sizeScale` / `radiusScale`): `linear`, `sqrt` and `log`
//! ([`SizeScale`]), as an `interpolate` on `circle-radius`; `sqrt` and `log`
//! are sampled at evenly spaced points of the transformed domain, so the
//! piecewise-linear curve passes exactly through the d3 value at every
//! stop.
//!
//! The named palettes ([`PALETTES`]) are Kepler's own static colour ranges
//! (`src/constants/custom-color-ranges.js` in kepler.gl 2.5.5, MIT,
//! Copyright (c) 2021 Uber Technologies, Inc.): the Uber Viz sequential,
//! diverging and qualitative ranges and the Uber ramps (Global Warming,
//! Sunrise, Ocean Green, UberPool, ...), plus the ColorBrewer ranges the
//! Kepler sample maps name (ColorBrewer, Apache-2.0, Cynthia Brewer), with
//! the colours exactly as those maps' configs carry them.

use serde_json::{json, Value};

use crate::style::Rgba;

/// What a palette is for, as Kepler's colour picker groups them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteKind {
    Sequential,
    Diverging,
    Qualitative,
}

/// A named colour range: Kepler's `colorRange`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub name: &'static str,
    pub kind: PaletteKind,
    /// `#RRGGBB`, low to high.
    pub colors: &'static [&'static str],
}

impl Palette {
    /// The colours as hex strings, reversed when `reversed` (Kepler's
    /// `colorRange.reversed`).
    pub fn hex(&self, reversed: bool) -> Vec<String> {
        let mut out: Vec<String> = self.colors.iter().map(|c| (*c).to_string()).collect();
        if reversed {
            out.reverse();
        }
        out
    }
}

/// The palette called `name` (Kepler's `colorRange.name`, case-insensitive).
/// Old names a saved document may carry still resolve
/// ([`PALETTE_ALIASES`]).
pub fn palette(name: &str) -> Option<&'static Palette> {
    let name = PALETTE_ALIASES.iter().find(|(old, _)| old.eq_ignore_ascii_case(name)).map_or(name, |(_, new)| *new);
    PALETTES.iter().find(|p| p.name.eq_ignore_ascii_case(name))
}

/// Palettes renamed since documents were saved with them: `(old, new)`.
pub const PALETTE_ALIASES: &[(&str, &str)] = &[("Edge Glow", "Edge bundling green")];

/// Every named palette. Names are unique (a test checks).
pub const PALETTES: &[Palette] = &[
    Palette { name: "Uber Viz Qualitative 0", kind: PaletteKind::Qualitative, colors: &["#12939A", "#DDB27C", "#88572C"] },
    Palette { name: "Uber Viz Qualitative 0.5", kind: PaletteKind::Qualitative, colors: &["#12939A", "#DDB27C", "#88572C", "#FF991F"] },
    Palette { name: "Uber Viz Qualitative 1", kind: PaletteKind::Qualitative, colors: &["#12939A", "#DDB27C", "#88572C", "#FF991F", "#F15C17"] },
    Palette { name: "Uber Viz Qualitative 1.2", kind: PaletteKind::Qualitative, colors: &["#12939A", "#DDB27C", "#88572C", "#FF991F", "#F15C17", "#223F9A"] },
    Palette { name: "Uber Viz Qualitative 1.4", kind: PaletteKind::Qualitative, colors: &["#12939A", "#DDB27C", "#88572C", "#FF991F", "#F15C17", "#223F9A", "#DA70BF"] },
    Palette { name: "Uber Viz Qualitative 1.6", kind: PaletteKind::Qualitative, colors: &["#12939A", "#DDB27C", "#88572C", "#FF991F", "#F15C17", "#223F9A", "#DA70BF", "#125C77"] },
    Palette { name: "Uber Viz Qualitative 1.8", kind: PaletteKind::Qualitative, colors: &["#12939A", "#DDB27C", "#88572C", "#FF991F", "#F15C17", "#223F9A", "#DA70BF", "#125C77", "#4DC19C"] },
    Palette { name: "Uber Viz Qualitative 2", kind: PaletteKind::Qualitative, colors: &["#12939A", "#DDB27C", "#88572C", "#FF991F", "#F15C17", "#223F9A", "#DA70BF", "#125C77", "#4DC19C", "#776E57"] },
    Palette { name: "Uber Viz Qualitative 3", kind: PaletteKind::Qualitative, colors: &["#12939A", "#DDB27C", "#88572C", "#FF991F", "#F15C17", "#223F9A", "#DA70BF", "#125C77", "#4DC19C", "#776E57", "#17B8BE", "#F6D18A", "#B7885E", "#FFCB99", "#F89570"] },
    Palette { name: "Uber Viz Qualitative 4", kind: PaletteKind::Qualitative, colors: &["#12939A", "#DDB27C", "#88572C", "#FF991F", "#F15C17", "#223F9A", "#DA70BF", "#125C77", "#4DC19C", "#776E57", "#17B8BE", "#F6D18A", "#B7885E", "#FFCB99", "#F89570", "#829AE3", "#E79FD5", "#1E96BE", "#89DAC1", "#B3AD9E"] },
    Palette { name: "Uber Viz Sequential 1", kind: PaletteKind::Sequential, colors: &["#E6FAFA", "#89C6CA", "#00939C"] },
    Palette { name: "Uber Viz Sequential 2", kind: PaletteKind::Sequential, colors: &["#E6FAFA", "#AAD7DA", "#68B4BB", "#00939C"] },
    Palette { name: "Uber Viz Sequential 3", kind: PaletteKind::Sequential, colors: &["#E6FAFA", "#B8E0E1", "#89C6CA", "#56ACB3", "#00939C"] },
    Palette { name: "Uber Viz Sequential 4", kind: PaletteKind::Sequential, colors: &["#E6FAFA", "#C1E5E6", "#9DD0D4", "#75BBC1", "#4BA7AF", "#00939C"] },
    Palette { name: "Uber Viz Sequential 5", kind: PaletteKind::Sequential, colors: &["#E6FAFA", "#C1E5E6", "#9DD0D4", "#75BBC1", "#4BA7AF", "#00939C", "#108188"] },
    Palette { name: "Uber Viz Sequential 6", kind: PaletteKind::Sequential, colors: &["#E6FAFA", "#C1E5E6", "#9DD0D4", "#75BBC1", "#4BA7AF", "#00939C", "#108188", "#0E7077"] },
    Palette { name: "Uber Viz Diverging 0", kind: PaletteKind::Diverging, colors: &["#C22E00", "#FEEEE8", "#00939C"] },
    Palette { name: "Uber Viz Diverging 0.5", kind: PaletteKind::Diverging, colors: &["#C22E00", "#EFBEAE", "#A2D4D7", "#00939C"] },
    Palette { name: "Uber Viz Diverging 1", kind: PaletteKind::Diverging, colors: &["#C22E00", "#EC9370", "#FEEEE8", "#85C4C8", "#00939C"] },
    Palette { name: "Uber Viz Diverging 1.5", kind: PaletteKind::Diverging, colors: &["#C22E00", "#DD7755", "#F8C0AA", "#BAE1E2", "#5DBABF", "#00939C"] },
    Palette { name: "Uber Viz Diverging 2", kind: PaletteKind::Diverging, colors: &["#C22E00", "#E17449", "#F5B097", "#FEEEE8", "#A2D4D7", "#65B3BA", "#00939C"] },
    Palette { name: "Uber Viz Diverging 2.5", kind: PaletteKind::Diverging, colors: &["#C22E00", "#D45F39", "#E68F71", "#F8C0AA", "#BAE1E2", "#7CC7CB", "#3EADB3", "#00939C"] },
    Palette { name: "Uber Viz Diverging 3", kind: PaletteKind::Diverging, colors: &["#C22E00", "#DA6436", "#EC9370", "#F8C0AA", "#FEEEE8", "#B2DCDF", "#65B3BA", "#49A6AE", "#00939C"] },
    Palette { name: "Uber Viz Diverging 3.5", kind: PaletteKind::Diverging, colors: &["#C22E00", "#D0532B", "#DD7755", "#EB9C80", "#F8C0AA", "#BAE1E2", "#8CCED1", "#5DBABF", "#2FA7AE", "#00939C"] },
    Palette { name: "Uber Viz Diverging 4", kind: PaletteKind::Diverging, colors: &["#C22E00", "#D55A2B", "#E68059", "#F2A587", "#F8C0AA", "#FEEEE8", "#BAE1E2", "#97CED1", "#71BABF", "#49A6AE", "#00939C"] },
    Palette { name: "UberPool", kind: PaletteKind::Diverging, colors: &["#223F9A", "#2C51BE", "#482BBD", "#7A0DA6", "#AE0E7F", "#CF1750", "#E31A1A", "#FD7900", "#FAC200", "#FAE300"] },
    Palette { name: "UberPool 9", kind: PaletteKind::Diverging, colors: &["#2C51BE", "#482BBD", "#7A0DA6", "#AE0E7F", "#CF1750", "#E31A1A", "#FD7900", "#FAC200", "#FAE300"] },
    Palette { name: "UberPool 8", kind: PaletteKind::Diverging, colors: &["#213E9A", "#3C1FA7", "#811CB5", "#C318B0", "#D01367", "#DE0F0E", "#EC7007", "#F9E200"] },
    Palette { name: "UberPool 7", kind: PaletteKind::Diverging, colors: &["#213E9A", "#461FA9", "#9B1BBA", "#CA168E", "#DA102C", "#E95E08", "#F9E200"] },
    Palette { name: "UberPool 6", kind: PaletteKind::Diverging, colors: &["#213E9A", "#551EAD", "#C019BD", "#D31256", "#E6470A", "#F9E200"] },
    Palette { name: "UberPool 5", kind: PaletteKind::Diverging, colors: &["#213E9A", "#6E1DB2", "#CA168E", "#E2260C", "#F9E200"] },
    Palette { name: "UberPool 4", kind: PaletteKind::Diverging, colors: &["#213E9A", "#9B1BBA", "#DA102C", "#F9E200"] },
    Palette { name: "UberPool 3", kind: PaletteKind::Diverging, colors: &["#213E9A", "#CA168E", "#F9E200"] },
    Palette { name: "Ice And Fire 3", kind: PaletteKind::Diverging, colors: &["#0198BD", "#FAFEB3", "#D50255"] },
    Palette { name: "Ice And Fire 4", kind: PaletteKind::Diverging, colors: &["#0198BD", "#E8FEB5", "#FEAD54", "#D50255"] },
    Palette { name: "Ice And Fire 5", kind: PaletteKind::Diverging, colors: &["#0198BD", "#49E3CE", "#FAFEB3", "#FEAD54", "#D50255"] },
    Palette { name: "Ice And Fire", kind: PaletteKind::Diverging, colors: &["#0198BD", "#49E3CE", "#E8FEB5", "#FEEDB1", "#FEAD54", "#D50255"] },
    Palette { name: "Ice And Fire 7", kind: PaletteKind::Diverging, colors: &["#0198BD", "#54BAB9", "#A7DCB6", "#FAFEB3", "#FCD583", "#FEAD54", "#D50255"] },
    Palette { name: "Ice And Fire 8", kind: PaletteKind::Diverging, colors: &["#007A99", "#0198BD", "#49E3CE", "#E8FEB5", "#FEEDB1", "#FEAD54", "#D50255", "#7F1941"] },
    Palette { name: "Global Warming 3", kind: PaletteKind::Sequential, colors: &["#5A1846", "#C70039", "#FFC300"] },
    Palette { name: "Global Warming 4", kind: PaletteKind::Sequential, colors: &["#5A1846", "#831A3D", "#E3611C", "#FFC300"] },
    Palette { name: "Global Warming 5", kind: PaletteKind::Sequential, colors: &["#5A1846", "#831A3D", "#AC1C17", "#D55D0E", "#FFC300"] },
    Palette { name: "Global Warming", kind: PaletteKind::Sequential, colors: &["#5A1846", "#900C3F", "#C70039", "#E3611C", "#F1920E", "#FFC300"] },
    Palette { name: "Global Warming 7", kind: PaletteKind::Sequential, colors: &["#5A1846", "#751A43", "#911932", "#AC1C17", "#C84411", "#E37B0A", "#FFC300"] },
    Palette { name: "Global Warming 8", kind: PaletteKind::Sequential, colors: &["#4C0035", "#650031", "#7F0023", "#98000A", "#B21800", "#CB4600", "#E57F00", "#FFC300"] },
    Palette { name: "Sunrise 3", kind: PaletteKind::Sequential, colors: &["#355C7D", "#C06C84", "#F8B195"] },
    Palette { name: "Sunrise 4", kind: PaletteKind::Sequential, colors: &["#355C7D", "#9A627C", "#C86A7E", "#F8B195"] },
    Palette { name: "Sunrise", kind: PaletteKind::Sequential, colors: &["#355C7D", "#6C5B7B", "#C06C84", "#F67280", "#F8B195"] },
    Palette { name: "Sunrise 6", kind: PaletteKind::Sequential, colors: &["#355C7D", "#63617F", "#916681", "#D88185", "#E8998D", "#F8B195"] },
    Palette { name: "Sunrise 7", kind: PaletteKind::Sequential, colors: &["#355C7D", "#63617F", "#916681", "#C06C84", "#D28389", "#E59A8F", "#F8B195"] },
    Palette { name: "Sunrise 8", kind: PaletteKind::Sequential, colors: &["#194266", "#355C7D", "#63617F", "#916681", "#C06C84", "#D28389", "#E59A8F", "#F8B195"] },
    Palette { name: "Ocean Green 3", kind: PaletteKind::Sequential, colors: &["#3A748A", "#3EACA8", "#E5EEC1"] },
    Palette { name: "Ocean Green 4", kind: PaletteKind::Sequential, colors: &["#547A82", "#3EACA8", "#A2D4AB", "#E5EEC1"] },
    Palette { name: "Ocean Green 5", kind: PaletteKind::Sequential, colors: &["#3A748A", "#54A38F", "#73BC84", "#A9D597", "#E5EEC1"] },
    Palette { name: "Ocean Green 6", kind: PaletteKind::Sequential, colors: &["#37535E", "#3A748A", "#4095B5", "#52AEC9", "#72BFC4", "#93CFBF"] },
    Palette { name: "Ocean Green 7", kind: PaletteKind::Sequential, colors: &["#3A748A", "#4B9A95", "#5EAB8B", "#73BC84", "#92CC8B", "#BEDDA5", "#E5EEC1"] },
    Palette { name: "Ocean Green 8", kind: PaletteKind::Sequential, colors: &["#37535E", "#3A748A", "#4B9A95", "#5EAB8B", "#73BC84", "#92CC8B", "#BEDDA5", "#E5EEC1"] },
    Palette { name: "Pink Wine 3", kind: PaletteKind::Sequential, colors: &["#50315E", "#956485", "#EDD1CA"] },
    Palette { name: "Pink Wine 4", kind: PaletteKind::Sequential, colors: &["#50315E", "#774976", "#B28294", "#EDD1CA"] },
    Palette { name: "Pink Wine 5", kind: PaletteKind::Sequential, colors: &["#50315E", "#643D68", "#956485", "#C1939D", "#EDD1CA"] },
    Palette { name: "Pink Wine 6", kind: PaletteKind::Sequential, colors: &["#2C1E3D", "#573660", "#83537C", "#A6758E", "#C99DA4", "#EDD1CA"] },
    Palette { name: "Pink Wine 7", kind: PaletteKind::Sequential, colors: &["#2C1E3D", "#4F315A", "#774976", "#956485", "#B28294", "#CFA4A8", "#EDD1CA"] },
    Palette { name: "Pink Wine", kind: PaletteKind::Sequential, colors: &["#2C1E3D", "#50315E", "#764476", "#9A5B88", "#B77495", "#CF91A3", "#E0B1B3", "#EDD1CA"] },
    Palette { name: "Purple Blue Yellow 3", kind: PaletteKind::Sequential, colors: &["#2B1E3E", "#5EA28D", "#D6DEBF"] },
    Palette { name: "Purple Blue Yellow 4", kind: PaletteKind::Sequential, colors: &["#2B1E3E", "#466373", "#7BA889", "#D6DEBF"] },
    Palette { name: "Purple Blue Yellow 5", kind: PaletteKind::Sequential, colors: &["#2B1E3E", "#3A4B66", "#5F8E86", "#8BB68D", "#D6DEBF"] },
    Palette { name: "Purple Blue Yellow 6", kind: PaletteKind::Sequential, colors: &["#2B1E3E", "#343D5E", "#4F777E", "#709E87", "#99BE95", "#D6DEBF"] },
    Palette { name: "Purple Blue Yellow 7", kind: PaletteKind::Sequential, colors: &["#2B1E3E", "#303558", "#466373", "#5F8E86", "#7BA889", "#A4C39B", "#D6DEBF"] },
    Palette { name: "Purple Blue Yellow", kind: PaletteKind::Sequential, colors: &["#2B1E3E", "#383C65", "#3E5F7E", "#49838A", "#5EA28D", "#82BB92", "#AECEA1", "#D6DEBF"] },
    Palette { name: "ColorBrewer BuPu-6", kind: PaletteKind::Sequential, colors: &["#EDF8FB", "#BFD3E6", "#9EBCDA", "#8C96C6", "#8856A7", "#810F7C"] },
    Palette { name: "ColorBrewer PRGn-6", kind: PaletteKind::Diverging, colors: &["#762A83", "#AF8DC3", "#E7D4E8", "#D9F0D3", "#7FBF7B", "#1B7837"] },
    Palette { name: "ColorBrewer RdPu-9", kind: PaletteKind::Sequential, colors: &["#FFF7F3", "#FDE0DD", "#FCC5C0", "#FA9FB5", "#F768A1", "#DD3497", "#AE017E", "#7A0177", "#49006A"] },
    // Last, so the picker's Kepler families keep their places. Not
    // Kepler's: the nine luminance steps of the mobitwin-edgebundling
    // README render (DigitalGeographyLab), measured off its pixels in
    // an in-house desktop app. Lightness climbs 21% -> 98% while
    // saturation climbs 44% -> 87%, collapsing only in the last step, so
    // it brightens into white rather than fading to a pastel.
    Palette { name: "Edge bundling green", kind: PaletteKind::Sequential, colors: &["#1E4D3B", "#256743", "#2C874C", "#35A659", "#3AC876", "#3EDD9A", "#72F2CC", "#C3FBF4", "#F9FCFB"] },
];

/// Kepler's colour scale types (`visualChannels.colorScale`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorScaleKind {
    Quantile,
    Quantize,
    Ordinal,
    Linear,
}

impl ColorScaleKind {
    /// Kepler's config name (`"quantile"`, ...).
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "quantile" => Some(Self::Quantile),
            "quantize" => Some(Self::Quantize),
            "ordinal" => Some(Self::Ordinal),
            "linear" => Some(Self::Linear),
            _ => None,
        }
    }
}

/// One legend row: a swatch and what it stands for.
#[derive(Debug, Clone, PartialEq)]
pub struct LegendEntry {
    pub color: String,
    pub label: String,
}

/// Why a scale could not be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScaleError {
    /// No colours (or, for a size scale, a degenerate range).
    NoColors,
    /// No finite value (or category) to take a domain from.
    EmptyDomain,
    /// A colour that is not `#rgb`/`#rrggbb`/`#rrggbbaa`.
    BadColor(String),
    /// `log` over a domain that is not strictly positive.
    LogDomain,
}

impl std::fmt::Display for ScaleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoColors => f.write_str("a scale needs at least one colour"),
            Self::EmptyDomain => f.write_str("no finite value to take a domain from"),
            Self::BadColor(c) => write!(f, "not a hex colour: {c}"),
            Self::LogDomain => f.write_str("a log scale needs a strictly positive domain"),
        }
    }
}

impl std::error::Error for ScaleError {}

/// A data-driven colour: Kepler's `colorField` + `colorScale` +
/// `colorRange`, resolved against the data.
#[derive(Debug, Clone, PartialEq)]
pub struct ColorScale {
    pub kind: ColorScaleKind,
    pub field: String,
    pub colors: Vec<String>,
    /// `Quantile`/`Quantize`: the `colors.len() - 1` thresholds (a value
    /// `>= thresholds[i]` takes `colors[i + 1]`), duplicates kept so the
    /// legend can say so. `Linear`: the `colors.len()` evenly spaced stops.
    /// `Ordinal`: empty.
    pub thresholds: Vec<f64>,
    /// `Ordinal` only: the sorted categories, `categories[i]` taking
    /// `colors[i % colors.len()]`.
    pub categories: Vec<Value>,
    /// `[min, max]` of the values the scale was built from (`Ordinal`: unset).
    pub domain: Option<[f64; 2]>,
}

fn check_colors(colors: &[String]) -> Result<(), ScaleError> {
    if colors.is_empty() {
        return Err(ScaleError::NoColors);
    }
    for c in colors {
        if crate::overlay::paint::parse_color(c).is_none() {
            return Err(ScaleError::BadColor(c.clone()));
        }
    }
    Ok(())
}

fn finite_sorted(values: impl IntoIterator<Item = f64>) -> Vec<f64> {
    let mut v: Vec<f64> = values.into_iter().filter(|x| x.is_finite()).collect();
    v.sort_by(f64::total_cmp);
    v
}

/// d3's `quantileSorted` (R-7: linear interpolation between the closest
/// ranks), over values already sorted ascending.
pub fn quantile_sorted(sorted: &[f64], p: f64) -> Option<f64> {
    let n = sorted.len();
    if n == 0 {
        return None;
    }
    if p <= 0.0 || n < 2 {
        return Some(sorted[0]);
    }
    if p >= 1.0 {
        return Some(sorted[n - 1]);
    }
    let i = (n - 1) as f64 * p;
    let i0 = i.floor() as usize;
    let (lo, hi) = (sorted[i0], sorted[(i0 + 1).min(n - 1)]);
    Some(lo + (hi - lo) * (i - i0 as f64))
}

impl ColorScale {
    /// `scaleQuantile`: `colors.len()` classes holding equal shares of
    /// `values` (non-finite values are ignored, as Kepler ignores nulls).
    pub fn quantile(field: &str, values: impl IntoIterator<Item = f64>, colors: Vec<String>) -> Result<Self, ScaleError> {
        check_colors(&colors)?;
        let sorted = finite_sorted(values);
        if sorted.is_empty() {
            return Err(ScaleError::EmptyDomain);
        }
        let k = colors.len();
        let thresholds = (1..k).filter_map(|i| quantile_sorted(&sorted, i as f64 / k as f64)).collect();
        Ok(Self {
            kind: ColorScaleKind::Quantile,
            field: field.to_string(),
            colors,
            thresholds,
            categories: Vec::new(),
            domain: Some([sorted[0], sorted[sorted.len() - 1]]),
        })
    }

    /// `scaleQuantize` over `[min, max]`: `colors.len()` equal-width bins.
    pub fn quantize(field: &str, min: f64, max: f64, colors: Vec<String>) -> Result<Self, ScaleError> {
        check_colors(&colors)?;
        if !(min.is_finite() && max.is_finite()) {
            return Err(ScaleError::EmptyDomain);
        }
        let k = colors.len();
        // d3: ((i + 1) * x1 - (i - n) * x0) / (n + 1), n = k - 1.
        let thresholds = (0..k.saturating_sub(1))
            .map(|i| ((i as f64 + 1.0) * max - (i as f64 - (k as f64 - 1.0)) * min) / k as f64)
            .collect();
        Ok(Self {
            kind: ColorScaleKind::Quantize,
            field: field.to_string(),
            colors,
            thresholds,
            categories: Vec::new(),
            domain: Some([min, max]),
        })
    }

    /// [`Self::quantize`] with the domain taken from `values`.
    pub fn quantize_values(field: &str, values: impl IntoIterator<Item = f64>, colors: Vec<String>) -> Result<Self, ScaleError> {
        let sorted = finite_sorted(values);
        let (Some(&min), Some(&max)) = (sorted.first(), sorted.last()) else { return Err(ScaleError::EmptyDomain) };
        Self::quantize(field, min, max, colors)
    }

    /// `scaleLinear` with the colours spread evenly over `[min, max]`.
    pub fn linear(field: &str, min: f64, max: f64, colors: Vec<String>) -> Result<Self, ScaleError> {
        check_colors(&colors)?;
        if !(min.is_finite() && max.is_finite()) {
            return Err(ScaleError::EmptyDomain);
        }
        let k = colors.len();
        let thresholds =
            (0..k).map(|i| if k == 1 { min } else { min + (max - min) * i as f64 / (k - 1) as f64 }).collect();
        Ok(Self {
            kind: ColorScaleKind::Linear,
            field: field.to_string(),
            colors,
            thresholds,
            categories: Vec::new(),
            domain: Some([min, max]),
        })
    }

    /// `scaleOrdinal` over the sorted unique `values` (Kepler's
    /// `getOrdinalDomain`), colours cycled. Strings sort before numbers'
    /// text is never compared: numbers sort numerically, strings
    /// lexically, numbers first.
    pub fn ordinal(field: &str, values: impl IntoIterator<Item = Value>, colors: Vec<String>) -> Result<Self, ScaleError> {
        check_colors(&colors)?;
        let mut numbers: Vec<f64> = Vec::new();
        let mut strings: Vec<String> = Vec::new();
        let mut bools: Vec<bool> = Vec::new();
        for v in values {
            match v {
                Value::Number(n) => {
                    if let Some(x) = n.as_f64().filter(|x| x.is_finite()) {
                        numbers.push(x);
                    }
                }
                Value::String(s) => strings.push(s),
                Value::Bool(b) => bools.push(b),
                _ => {}
            }
        }
        numbers.sort_by(f64::total_cmp);
        numbers.dedup();
        strings.sort();
        strings.dedup();
        bools.sort();
        bools.dedup();
        let categories: Vec<Value> = numbers
            .into_iter()
            .map(|x| json!(x))
            .chain(strings.into_iter().map(Value::String))
            .chain(bools.into_iter().map(Value::Bool))
            .collect();
        if categories.is_empty() {
            return Err(ScaleError::EmptyDomain);
        }
        Ok(Self {
            kind: ColorScaleKind::Ordinal,
            field: field.to_string(),
            colors,
            thresholds: Vec::new(),
            categories,
            domain: None,
        })
    }

    /// The colour `value` takes, as d3 would give it. `None` for a value
    /// the scale does not cover (a non-finite number, an unknown category).
    pub fn color_for(&self, value: &Value) -> Option<&str> {
        match self.kind {
            ColorScaleKind::Ordinal => {
                let at = self.categories.iter().position(|c| same_label(c, value))?;
                Some(&self.colors[at % self.colors.len()])
            }
            ColorScaleKind::Quantile | ColorScaleKind::Quantize => {
                let x = value.as_f64().filter(|x| x.is_finite())?;
                // bisectRight: how many thresholds are <= x.
                let i = self.thresholds.partition_point(|t| *t <= x);
                Some(&self.colors[i.min(self.colors.len() - 1)])
            }
            ColorScaleKind::Linear => {
                let x = value.as_f64().filter(|x| x.is_finite())?;
                let i = self.thresholds.partition_point(|t| *t <= x);
                Some(&self.colors[i.saturating_sub(1).min(self.colors.len() - 1)])
            }
        }
    }

    /// The scale as a colour expression over `["get", field]`, for
    /// `fill-color`, `line-color` or `circle-color`.
    ///
    /// Quantile/quantize thresholds that coincide (a skewed field whose
    /// quantiles repeat) collapse into one `step` stop taking the LAST of
    /// their colours, which is what d3's `bisectRight` gives a value equal
    /// to them -- `step` needs strictly ascending stops.
    pub fn expression(&self) -> Value {
        let get = json!(["get", self.field]);
        match self.kind {
            ColorScaleKind::Quantile | ColorScaleKind::Quantize => {
                let mut stops: Vec<(f64, &str)> = Vec::new();
                for (i, t) in self.thresholds.iter().enumerate() {
                    let c = self.colors[(i + 1).min(self.colors.len() - 1)].as_str();
                    match stops.last_mut() {
                        Some(last) if last.0 == *t => last.1 = c,
                        _ => stops.push((*t, c)),
                    }
                }
                let mut out = vec![json!("step"), get, json!(self.colors[0])];
                for (t, c) in stops {
                    out.push(json!(t));
                    out.push(json!(c));
                }
                Value::Array(out)
            }
            ColorScaleKind::Linear => {
                if self.thresholds.len() < 2 || self.thresholds[0] >= self.thresholds[self.thresholds.len() - 1] {
                    return json!(self.colors[0]);
                }
                let mut out = vec![json!("interpolate"), json!(["linear"]), get];
                for (t, c) in self.thresholds.iter().zip(&self.colors) {
                    out.push(json!(t));
                    out.push(json!(c));
                }
                Value::Array(out)
            }
            ColorScaleKind::Ordinal => {
                let mut out = vec![json!("match"), get];
                for (i, c) in self.categories.iter().enumerate() {
                    out.push(c.clone());
                    out.push(json!(self.colors[i % self.colors.len()]));
                }
                // A value outside the domain: Kepler's "no value" grey.
                out.push(json!(NO_VALUE_COLOR));
                Value::Array(out)
            }
        }
    }

    /// The legend Kepler draws for this scale: one row per colour (per
    /// category, for an ordinal scale), labelled with its range.
    pub fn legend(&self) -> Vec<LegendEntry> {
        match self.kind {
            ColorScaleKind::Ordinal => self
                .categories
                .iter()
                .enumerate()
                .map(|(i, c)| LegendEntry {
                    color: self.colors[i % self.colors.len()].clone(),
                    label: match c {
                        Value::String(s) => s.clone(),
                        Value::Number(n) => format_number(n.as_f64().unwrap_or(f64::NAN)),
                        other => other.to_string(),
                    },
                })
                .collect(),
            ColorScaleKind::Linear => self
                .thresholds
                .iter()
                .zip(&self.colors)
                .map(|(t, c)| LegendEntry { color: c.clone(), label: format_number(*t) })
                .collect(),
            ColorScaleKind::Quantile | ColorScaleKind::Quantize => {
                let [min, max] = self.domain.unwrap_or([f64::NAN, f64::NAN]);
                let k = self.colors.len();
                (0..k)
                    .map(|i| {
                        let lo = if i == 0 { min } else { self.thresholds[i - 1] };
                        let hi = if i + 1 == k { max } else { self.thresholds[i] };
                        LegendEntry { color: self.colors[i].clone(), label: format!("{} to {}", format_number(lo), format_number(hi)) }
                    })
                    .collect()
            }
        }
    }
}

/// The colour a value outside an ordinal scale's domain draws in.
pub const NO_VALUE_COLOR: &str = "#808080";

fn same_label(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        _ => a == b,
    }
}

/// A number for a legend: grouped thousands above 1,000 (no decimals),
/// otherwise at most two decimals with trailing zeros dropped.
pub fn format_number(x: f64) -> String {
    if !x.is_finite() {
        return "-".to_string();
    }
    if x.abs() >= 1000.0 {
        let rounded = x.round() as i64;
        let digits = rounded.unsigned_abs().to_string();
        let mut out = String::new();
        for (i, ch) in digits.chars().enumerate() {
            if i > 0 && (digits.len() - i).is_multiple_of(3) {
                out.push(',');
            }
            out.push(ch);
        }
        return if rounded < 0 { format!("-{out}") } else { out };
    }
    let s = format!("{x:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0".to_string() } else { s.to_string() }
}

/// Kepler's size scale types (`sizeScale`, `radiusScale`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeScaleKind {
    Linear,
    Sqrt,
    Log,
}

impl SizeScaleKind {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "linear" => Some(Self::Linear),
            "sqrt" => Some(Self::Sqrt),
            "log" => Some(Self::Log),
            _ => None,
        }
    }

    fn forward(self, x: f64) -> f64 {
        match self {
            Self::Linear => x,
            // d3's scalePow(0.5) transform: sign-preserving.
            Self::Sqrt => x.signum() * x.abs().sqrt(),
            Self::Log => x.ln(),
        }
    }

    fn inverse(self, y: f64) -> f64 {
        match self {
            Self::Linear => y,
            Self::Sqrt => y.signum() * y * y,
            Self::Log => y.exp(),
        }
    }
}

/// A data-driven size: a field's `[min, max]` mapped onto `range` (in
/// pixels) through a linear, square-root or log transform, as d3's
/// `scaleLinear`/`scaleSqrt`/`scaleLog` map it (clamped at the ends, as
/// `interpolate` clamps).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SizeScale {
    pub kind: SizeScaleKind,
    pub domain: [f64; 2],
    pub range: [f64; 2],
}

/// How many stops a `sqrt`/`log` size scale is sampled at.
pub const SIZE_SCALE_STOPS: usize = 16;

impl SizeScale {
    pub fn new(kind: SizeScaleKind, domain: [f64; 2], range: [f64; 2]) -> Result<Self, ScaleError> {
        if !(domain.iter().chain(&range).all(|v| v.is_finite())) {
            return Err(ScaleError::EmptyDomain);
        }
        if kind == SizeScaleKind::Log && (domain[0] <= 0.0 || domain[1] <= 0.0) {
            return Err(ScaleError::LogDomain);
        }
        Ok(Self { kind, domain, range })
    }

    /// [`Self::new`] with the domain taken from `values`.
    pub fn from_values(kind: SizeScaleKind, values: impl IntoIterator<Item = f64>, range: [f64; 2]) -> Result<Self, ScaleError> {
        let sorted = finite_sorted(values);
        let (Some(&min), Some(&max)) = (sorted.first(), sorted.last()) else { return Err(ScaleError::EmptyDomain) };
        Self::new(kind, [min, max], range)
    }

    /// The size `x` takes (d3's value, clamped to the range).
    pub fn value_for(&self, x: f64) -> f64 {
        let [d0, d1] = self.domain.map(|d| self.kind.forward(d));
        if d1 == d0 {
            return self.range[0];
        }
        let t = ((self.kind.forward(x) - d0) / (d1 - d0)).clamp(0.0, 1.0);
        self.range[0] + (self.range[1] - self.range[0]) * t
    }

    /// The scale as `["interpolate", ["linear"], ["get", field], ...]`,
    /// for `circle-radius`. Linear is two stops; sqrt and log are
    /// [`SIZE_SCALE_STOPS`] stops evenly spaced in the transformed domain.
    pub fn expression(&self, field: &str) -> Value {
        if self.domain[0] >= self.domain[1] {
            return json!(self.range[0]);
        }
        let n = if self.kind == SizeScaleKind::Linear { 2 } else { SIZE_SCALE_STOPS };
        let [t0, t1] = self.domain.map(|d| self.kind.forward(d));
        let mut out = vec![json!("interpolate"), json!(["linear"]), json!(["get", field])];
        let mut last = f64::NEG_INFINITY;
        for i in 0..n {
            let x = if i == 0 {
                self.domain[0]
            } else if i + 1 == n {
                self.domain[1]
            } else {
                self.kind.inverse(t0 + (t1 - t0) * i as f64 / (n - 1) as f64)
            };
            if x <= last {
                continue;
            }
            last = x;
            out.push(json!(x));
            out.push(json!(self.value_for(x).max(0.0)));
        }
        Value::Array(out)
    }
}

/// Kepler's point radius (`radius`, or a `radiusRange` end with a size
/// field) in screen pixels at latitude `lat`: deck.gl metres times
/// Kepler's zoom factor `2^(14 - zoom)` (`getRadiusScaleByZoom`), which
/// below z14 cancels the zoom and leaves `r * 2^14 / (78271.517 *
/// cos(lat))` -- a pixel size that holds at every zoom under 14. Kepler's
/// point layer sets no `radiusMinPixels`, so radius 10 is about 2 px at
/// the equator: Kepler's own look, which a host wanting larger default
/// dots gets by choosing a larger radius, not by changing this.
pub fn kepler_radius_px(r: f64, lat: f64) -> f64 {
    r * 16384.0 / (78_271.517 * lat.to_radians().cos())
}

/// `colors` as engine colours (an unparsable entry is skipped).
pub fn rgba(colors: &[String]) -> Vec<Rgba> {
    colors.iter().filter_map(|c| crate::overlay::paint::parse_color(c)).collect()
}

/// The automatic outline ("curb") shade of a fill colour: a darker shade
/// of the same hue, dark enough to edge the mark on a light basemap.
///
/// Every channel is scaled by one factor, 0.7 for a mid or dark colour (a
/// shade darker, as before). A pale colour (Viridis' yellow, the light end
/// of `Blues`) scaled by 0.7 is still pale -- `#fde725` gives an olive
/// `#b1a119`, about 2.3:1 against a light basemap's `#f2f2f2`, and the
/// smallest class disappears. So the factor steps down by 0.05, to at most
/// 0.45, until the shade's relative luminance (WCAG 2) is at most
/// [`CURB_MAX_LUMINANCE`], which is 3:1 against `#f2f2f2` (WCAG's floor for
/// a graphical object's edge). Integer steps (`(14 - i) / 20`) and
/// truncation, so the Pro web viewer's `curb` (viewer/shade.js) gives the
/// same bytes.
#[must_use]
pub fn curb_shade(rgb: [u8; 3]) -> [u8; 3] {
    let mut shade = rgb;
    for i in 0..=5 {
        let k = f64::from(14 - i) / 20.0;
        shade = rgb.map(|v| (f64::from(v) * k).floor() as u8);
        if relative_luminance(shade) <= CURB_MAX_LUMINANCE {
            break;
        }
    }
    shade
}

/// The relative luminance [`curb_shade`] stops at: 3:1 contrast against
/// `#f2f2f2` ((0.888 + 0.05) / (0.26 + 0.05) = 3.0).
pub const CURB_MAX_LUMINANCE: f64 = 0.26;

/// The automatic outline on a DARK basemap: a lighter tint of the fill,
/// the same hue mixed toward white.
///
/// On a dark basemap the [`curb_shade`] sits between the fill and the
/// background in lightness, so fill, ring and background make one
/// monotonic ramp -- the 1.5 px ring reads as blur, not as an edge
/// (measured on Graphite at 2x: the fill-to-background transition spread
/// over 3.6 device pixels, against 0.9 on a light map, where the darker
/// ring is a line between two light areas). A ring LIGHTER than the fill
/// puts the whole contrast in one antialiased pixel.
///
/// Each channel moves `k` of the way to 255, `k` = 0.5 first and up by
/// 0.1 (to at most 1.0, white) until the tint has 3:1 contrast against
/// [`DARK_EDGE_REFERENCE`], the lightest dark basemap background shipped.
/// Integer steps (`(10 + 2i) / 20`) and truncation, so the Pro web
/// viewer's `rimTint` (viewer/shade.js) gives the same bytes.
#[must_use]
pub fn rim_tint(rgb: [u8; 3]) -> [u8; 3] {
    let reference = relative_luminance(DARK_EDGE_REFERENCE);
    let mut tint = rgb;
    for i in 0..=5 {
        let k = f64::from(10 + 2 * i) / 20.0;
        tint = rgb.map(|v| v + (f64::from(255 - v) * k).floor() as u8);
        if (relative_luminance(tint) + 0.05) / (reference + 0.05) >= 3.0 {
            break;
        }
    }
    tint
}

/// The background [`rim_tint`] keeps 3:1 against: Graphite's `#1b1f26`,
/// the lightest of the dark basemaps (Dark Grey is `#1c1d21`, Dark Matter
/// `#0c0c0c`).
pub const DARK_EDGE_REFERENCE: [u8; 3] = [0x1b, 0x1f, 0x26];

/// A background under this relative luminance is dark: white text and a
/// light outline read on it better than black ones (0.179 is where white
/// and black have the same contrast against it). Every dark basemap is
/// under 0.02, every light one over 0.8.
pub const DARK_BACKGROUND_LUMINANCE: f64 = 0.179;

/// Whether a basemap whose background is `rgb` is dark
/// ([`DARK_BACKGROUND_LUMINANCE`]).
#[must_use]
pub fn is_dark_background(rgb: [u8; 3]) -> bool {
    relative_luminance(rgb) < DARK_BACKGROUND_LUMINANCE
}

/// The automatic (`"auto"`) outline of a point of colour `rgb`: the
/// darker [`curb_shade`] on a light basemap, the lighter [`rim_tint`] on a
/// dark one (`dark_basemap`).
#[must_use]
pub fn auto_edge(rgb: [u8; 3], dark_basemap: bool) -> [u8; 3] {
    if dark_basemap {
        rim_tint(rgb)
    } else {
        curb_shade(rgb)
    }
}

/// Whether `style`'s background (its first `background` layer, at zoom
/// 12) is dark ([`is_dark_background`]); a style with none is light.
#[must_use]
pub fn style_is_dark(style: &crate::style::Style) -> bool {
    style.layers.iter().find_map(|l| match l {
        crate::style::StyleLayer::Background { color, .. } => {
            let c = color.value_at(12);
            Some(is_dark_background([c.r, c.g, c.b].map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8)))
        }
        _ => None,
    }) == Some(true)
}

/// WCAG 2 relative luminance of an sRGB colour, 0 (black) to 1 (white).
#[must_use]
pub fn relative_luminance(rgb: [u8; 3]) -> f64 {
    let lin = |v: u8| {
        let c = f64::from(v) / 255.0;
        if c <= 0.039_28 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(rgb[0]) + 0.7152 * lin(rgb[1]) + 0.0722 * lin(rgb[2])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(p: &str) -> Vec<String> {
        palette(p).unwrap_or_else(|| panic!("palette {p}")).hex(false)
    }

    #[test]
    fn kepler_radius_px_matches_the_samples() {
        // The Gallery's California earthquakes range, 4.2..96.2 at 37 N.
        assert!((kepler_radius_px(4.2, 37.0) - 1.10).abs() < 0.01);
        assert!((kepler_radius_px(96.2, 37.0) - 25.2).abs() < 0.1);
        // Kepler's default radius 10 is about 2 px at the equator.
        assert!((kepler_radius_px(10.0, 0.0) - 2.093).abs() < 0.001);
    }

    #[test]
    fn palette_names_are_unique_and_every_colour_parses() {
        let mut names: Vec<&str> = PALETTES.iter().map(|p| p.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "duplicate palette name");
        for p in PALETTES {
            assert!(!p.colors.is_empty(), "{}", p.name);
            assert_eq!(rgba(&p.hex(false)).len(), p.colors.len(), "{}", p.name);
        }
    }

    #[test]
    fn the_sample_maps_palettes_match_their_configs() {
        // Exactly as kepler.gl-data's sample configs carry them.
        assert_eq!(hex("Ocean Green 8")[0], "#37535E");
        assert_eq!(hex("UberPool 6"), ["#213E9A", "#551EAD", "#C019BD", "#D31256", "#E6470A", "#F9E200"]);
        assert_eq!(hex("ColorBrewer RdPu-9").len(), 9);
        assert_eq!(hex("ColorBrewer PRGn-6")[0], "#762A83");
        assert_eq!(palette("uber viz sequential 6").map(|p| p.kind), Some(PaletteKind::Sequential));
        assert_eq!(palette("Uber Viz Diverging 1.5").map(|p| p.kind), Some(PaletteKind::Diverging));
        assert_eq!(palette("Uber Viz Diverging 1.5").map(|p| p.colors.len()), Some(6));
        assert_eq!(hex("Sunrise 8").len(), 8);
        assert_eq!(hex("Global Warming").len(), 6);
    }

    #[test]
    fn reversed_flips_the_order() {
        let p = palette("Global Warming").unwrap();
        let (fwd, rev) = (p.hex(false), p.hex(true));
        assert_eq!(fwd.first(), rev.last());
    }

    #[test]
    fn quantile_sorted_is_d3s_r7() {
        let v = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(quantile_sorted(&v, 0.5), Some(2.5));
        assert_eq!(quantile_sorted(&v, 0.25), Some(1.75));
        assert_eq!(quantile_sorted(&v, 0.0), Some(1.0));
        assert_eq!(quantile_sorted(&v, 1.0), Some(4.0));
        assert_eq!(quantile_sorted(&[], 0.5), None);
    }

    #[test]
    fn quantile_classes_hold_equal_shares() {
        let values: Vec<f64> = (1..=100).map(f64::from).collect();
        let colors = hex("Uber Viz Sequential 2"); // 4 colours
        let s = ColorScale::quantile("v", values.iter().copied(), colors.clone()).unwrap();
        assert_eq!(s.thresholds, vec![25.75, 50.5, 75.25]);
        let mut counts = [0usize; 4];
        for v in &values {
            let c = s.color_for(&json!(v)).unwrap();
            counts[colors.iter().position(|x| x == c).unwrap()] += 1;
        }
        assert_eq!(counts, [25, 25, 25, 25]);
        assert_eq!(s.domain, Some([1.0, 100.0]));
        // NaN is ignored, not a class.
        let with_nan = ColorScale::quantile("v", values.iter().copied().chain([f64::NAN]), colors).unwrap();
        assert_eq!(with_nan.thresholds, s.thresholds);
    }

    #[test]
    fn quantize_bins_are_equal_width_as_d3() {
        let s = ColorScale::quantize("v", 0.0, 10.0, hex("Uber Viz Sequential 1")).unwrap();
        // d3.scaleQuantize().domain([0, 10]).range([a, b, c]).thresholds()
        assert_eq!(s.thresholds.len(), 2);
        assert!((s.thresholds[0] - 10.0 / 3.0).abs() < 1e-12);
        assert!((s.thresholds[1] - 20.0 / 3.0).abs() < 1e-12);
        assert_eq!(s.color_for(&json!(0.0)), Some(s.colors[0].as_str()));
        assert_eq!(s.color_for(&json!(5.0)), Some(s.colors[1].as_str()));
        assert_eq!(s.color_for(&json!(10.0)), Some(s.colors[2].as_str()));
        assert_eq!(s.color_for(&json!(99.0)), Some(s.colors[2].as_str()), "clamped above");
    }

    #[test]
    fn the_step_expression_agrees_with_color_for() {
        let values: Vec<f64> = (0..1000).map(|i| (i as f64 * 0.37).sin() * 50.0 + 50.0).collect();
        let s = ColorScale::quantile("v", values.iter().copied(), hex("ColorBrewer RdPu-9")).unwrap();
        let expr = s.expression();
        let mut store = crate::overlay::OverlayStore::new();
        let paint = json!({ "fill-color": expr.clone() });
        store.declare_source_paint("t", paint.as_object().unwrap()).expect("compiles as a source paint");
        // Evaluate the step by hand: the last stop <= x.
        let arr = expr.as_array().unwrap();
        for v in values.iter().step_by(37) {
            let mut out = arr[2].as_str().unwrap();
            for pair in arr[3..].chunks(2) {
                if pair[0].as_f64().unwrap() <= *v {
                    out = pair[1].as_str().unwrap();
                }
            }
            assert_eq!(Some(out), s.color_for(&json!(v)), "value {v}");
        }
    }

    #[test]
    fn repeated_quantiles_collapse_to_strictly_ascending_stops() {
        // 90% zeros: the first eight thresholds are all 0.
        let values: Vec<f64> = (0..100).map(|i| if i < 90 { 0.0 } else { f64::from(i) }).collect();
        let s = ColorScale::quantile("v", values, hex("ColorBrewer RdPu-9")).unwrap();
        let arr = s.expression();
        let stops: Vec<f64> = arr.as_array().unwrap()[3..].chunks(2).map(|p| p[0].as_f64().unwrap()).collect();
        assert!(stops.windows(2).all(|w| w[0] < w[1]), "{stops:?}");
        // A zero takes what bisectRight gives it: the colour after the last 0 threshold.
        let zero = s.color_for(&json!(0.0)).unwrap();
        let first_stop_color = arr.as_array().unwrap()[4].as_str().unwrap();
        assert_eq!(zero, first_stop_color);
    }

    #[test]
    fn ordinal_sorts_unique_values_and_cycles_colours() {
        let values = ["b", "a", "c", "a", "d"].map(|s| json!(s));
        let colors = vec!["#000000".to_string(), "#ffffff".to_string()];
        let s = ColorScale::ordinal("k", values, colors).unwrap();
        assert_eq!(s.categories, vec![json!("a"), json!("b"), json!("c"), json!("d")]);
        assert_eq!(s.color_for(&json!("c")), Some("#000000"));
        assert_eq!(s.color_for(&json!("d")), Some("#ffffff"));
        assert_eq!(s.color_for(&json!("zzz")), None);
        let expr = s.expression();
        assert_eq!(expr[0], "match");
        assert_eq!(expr.as_array().unwrap().last(), Some(&json!(NO_VALUE_COLOR)));
        let mut store = crate::overlay::OverlayStore::new();
        store.declare_source_paint("t", json!({ "circle-color": expr }).as_object().unwrap()).expect("compiles");
        assert_eq!(s.legend().len(), 4);
    }

    #[test]
    fn linear_is_an_interpolate_over_even_stops() {
        let s = ColorScale::linear("v", 0.0, 100.0, hex("Global Warming 3")).unwrap();
        assert_eq!(s.thresholds, vec![0.0, 50.0, 100.0]);
        let e = s.expression();
        assert_eq!(e[0], "interpolate");
        let mut store = crate::overlay::OverlayStore::new();
        store.declare_source_paint("t", json!({ "line-color": e }).as_object().unwrap()).expect("compiles");
    }

    #[test]
    fn bad_input_is_refused() {
        assert_eq!(ColorScale::quantile("v", [1.0], vec![]), Err(ScaleError::NoColors));
        assert_eq!(ColorScale::quantile("v", [f64::NAN], vec!["#fff".into()]), Err(ScaleError::EmptyDomain));
        assert_eq!(ColorScale::quantize("v", 0.0, 1.0, vec!["red".into()]), Err(ScaleError::BadColor("red".into())));
        assert_eq!(SizeScale::new(SizeScaleKind::Log, [0.0, 10.0], [1.0, 2.0]), Err(ScaleError::LogDomain));
    }

    #[test]
    fn legend_labels_ranges_from_min_to_max() {
        let s = ColorScale::quantize("v", 0.0, 3000.0, hex("Uber Viz Sequential 1")).unwrap();
        let l = s.legend();
        assert_eq!(l.len(), 3);
        assert_eq!(l[0].label, "0 to 1,000");
        assert_eq!(l[2].label, "2,000 to 3,000");
        assert_eq!(format_number(2.5), "2.5");
        assert_eq!(format_number(-1234.4), "-1,234");
        assert_eq!(format_number(0.126), "0.13");
    }

    #[test]
    fn sqrt_size_matches_d3_scale_sqrt_at_every_stop() {
        let s = SizeScale::new(SizeScaleKind::Sqrt, [2.5, 7.3], [1.0, 20.0]).unwrap();
        // d3.scaleSqrt().domain([2.5, 7.3]).range([1, 20])(4)
        let d3 = |x: f64| 1.0 + 19.0 * (x.sqrt() - 2.5f64.sqrt()) / (7.3f64.sqrt() - 2.5f64.sqrt());
        for x in [2.5, 3.0, 4.0, 5.5, 7.3] {
            assert!((s.value_for(x) - d3(x)).abs() < 1e-9, "{x}");
        }
        let e = s.expression("Magnitude");
        let arr = e.as_array().unwrap();
        assert_eq!(arr.len(), 3 + 2 * SIZE_SCALE_STOPS);
        for pair in arr[3..].chunks(2) {
            let (x, y) = (pair[0].as_f64().unwrap(), pair[1].as_f64().unwrap());
            assert!((y - d3(x)).abs() < 1e-9, "stop {x} -> {y}");
        }
        let mut store = crate::overlay::OverlayStore::new();
        store.declare_source_paint("t", json!({ "circle-radius": e }).as_object().unwrap()).expect("compiles");
    }

    #[test]
    fn log_and_linear_sizes() {
        let log = SizeScale::new(SizeScaleKind::Log, [1.0, 100.0], [0.0, 2.0]).unwrap();
        assert!((log.value_for(10.0) - 1.0).abs() < 1e-12);
        let lin = SizeScale::from_values(SizeScaleKind::Linear, [5.0, 1.0, 3.0], [0.0, 8.0]).unwrap();
        assert_eq!(lin.domain, [1.0, 5.0]);
        assert_eq!(lin.value_for(3.0), 4.0);
        assert_eq!(lin.value_for(100.0), 8.0, "clamped");
        assert_eq!(lin.expression("v").as_array().unwrap().len(), 7);
        assert_eq!(SizeScaleKind::parse("sqrt"), Some(SizeScaleKind::Sqrt));
        assert_eq!(ColorScaleKind::parse("quantile"), Some(ColorScaleKind::Quantile));
    }

    /// The curb shade: a shade darker (x0.7) for mid and dark colours, and
    /// darker still for pale ones until it edges the mark at 3:1 on a light
    /// basemap. The same table is pinned in the Pro web viewer's
    /// `viewer/shade.test.mjs`.
    #[test]
    fn the_curb_shade_of_a_pale_class_still_edges_it_on_a_light_basemap() {
        // Viridis, dark to pale, and Blues' pale end.
        let cases: [([u8; 3], [u8; 3]); 6] = [
            ([0x44, 0x01, 0x54], [0x2f, 0x00, 0x3a]),
            ([0x3b, 0x52, 0x8b], [0x29, 0x39, 0x61]),
            ([0x21, 0x91, 0x8c], [0x17, 0x65, 0x62]),
            ([0x5e, 0xc9, 0x62], [0x41, 0x8c, 0x44]),
            ([0xfd, 0xe7, 0x25], [0x97, 0x8a, 0x16]),
            ([0xde, 0xeb, 0xf7], [0x7a, 0x81, 0x87]),
        ];
        for (fill, curb) in cases {
            assert_eq!(curb_shade(fill), curb, "curb of {fill:02x?}");
        }
        let bg = relative_luminance([0xf2, 0xf2, 0xf2]);
        for (fill, _) in cases {
            let l = relative_luminance(curb_shade(fill));
            let contrast = (bg + 0.05) / (l + 0.05);
            assert!(contrast >= 2.95, "{fill:02x?}'s curb is {contrast:.2}:1 on #f2f2f2");
        }
        // White, the palest of all: a mid grey.
        assert_eq!(curb_shade([255, 255, 255]), [127, 127, 127]);
    }

    /// The rim tint: half-way to white, the same hue, and at least 3:1
    /// against Graphite's background. The same table is pinned in the Pro
    /// web viewer's `viewer/shade.test.mjs`.
    #[test]
    fn the_rim_tint_edges_a_point_on_a_dark_basemap() {
        // Viridis, dark to pale, Blues' pale end, the reported lime and
        // teal stations, black and Graphite's own background.
        let cases: [([u8; 3], [u8; 3]); 10] = [
            ([0x44, 0x01, 0x54], [0xa1, 0x80, 0xa9]),
            ([0x3b, 0x52, 0x8b], [0x9d, 0xa8, 0xc5]),
            ([0x21, 0x91, 0x8c], [0x90, 0xc8, 0xc5]),
            ([0x5e, 0xc9, 0x62], [0xae, 0xe4, 0xb0]),
            ([0xfd, 0xe7, 0x25], [0xfe, 0xf3, 0x92]),
            ([0xde, 0xeb, 0xf7], [0xee, 0xf5, 0xfb]),
            ([0x8f, 0xd1, 0x4f], [0xc7, 0xe8, 0xa7]),
            ([0x3f, 0xae, 0x8a], [0x9f, 0xd6, 0xc4]),
            ([0x00, 0x00, 0x00], [0x7f, 0x7f, 0x7f]),
            ([0x1b, 0x1f, 0x26], [0x8d, 0x8f, 0x92]),
        ];
        let bg = relative_luminance(DARK_EDGE_REFERENCE);
        for (fill, rim) in cases {
            let got = rim_tint(fill);
            assert_eq!(got, rim, "rim of {fill:02x?}");
            let contrast = (relative_luminance(got) + 0.05) / (bg + 0.05);
            assert!(contrast >= 3.0, "{fill:02x?}'s rim is {contrast:.2}:1 on #1b1f26");
            assert!(relative_luminance(got) >= relative_luminance(fill), "{fill:02x?}'s rim is lighter than the fill");
            assert_eq!(auto_edge(fill, true), rim);
            assert_eq!(auto_edge(fill, false), curb_shade(fill), "a light basemap keeps the curb");
        }
    }

    /// Every dark basemap's background is dark and every light one's is
    /// not.
    #[test]
    fn the_shipped_backgrounds_split_dark_and_light() {
        for dark in [[0x1b, 0x1f, 0x26], [0x1c, 0x1d, 0x21], [0x0c, 0x0c, 0x0c]] {
            assert!(is_dark_background(dark), "{dark:02x?}");
        }
        for light in [[0xf8, 0xf4, 0xf0], [0xf2, 0xf3, 0xf0], [0xf6, 0xf2, 0xec]] {
            assert!(!is_dark_background(light), "{light:02x?}");
        }
        for (bytes, dark) in [(crate::basemaps::Basemap::SwatDark, true), (crate::basemaps::Basemap::Liberty, false), (crate::basemaps::Basemap::OpenDark, true), (crate::basemaps::Basemap::Positron, false)] {
            let style = crate::style_spec::parse_and_compile(bytes.style_json()).expect("compiles");
            assert_eq!(style_is_dark(&style), dark, "{bytes:?}");
        }
    }
}

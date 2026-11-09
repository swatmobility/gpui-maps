//! The Studio's one hover / popup / highlight component, for any page's
//! `MapView`: hover an object and it lights up with a compact card beside
//! the pointer; click it and the card pins (Esc, a click elsewhere or the
//! close button unpins it).
//!
//! Studio-side only. The library answers "what is under the pointer"
//! (`MapView::pick`, every kind: overlay features, arcs, extrusion columns,
//! altitude points) and "light it" (`MapView::set_highlight`); the card,
//! its words and where it goes are the host's.
//!
//! A page hooks it up with two children and one constructor:
//!
//! ```ignore
//! let popup = kit::popup::install(&map, PopupConfig::new().source("cities", CardSpec::new().title("name")), window, cx);
//! // in render, right after the map and BEFORE the page's own cards, so
//! // an occluding legend keeps the pointer off the map beneath it:
//! .child(self.inner.clone())
//! .child(kit::popup::input_layer(&self.popup, cx))
//! // ... the page's legend / status cards ...
//! .child(self.popup.clone()) // the cards, over everything
//! ```
//!
//! The input layer is a transparent element over the map: it never
//! occludes, so the map keeps its own pan, zoom and hover; it picks on a
//! pointer move only (never per frame), throttled to the display rate,
//! reusing the last answer while the pointer has not moved; and it asks
//! for the pointing hand over anything pickable.
//!
//! `GPUI_MAP_STUDIO_PROBE_HOVER=x,y` (map-local logical pixels; values
//! `<= 1` are fractions of the map) dispatches a pointer move into the
//! window's own event path at launch -- gpui's hit test, the map's and
//! this layer's handlers, no OS pointer moved -- repeated until a card is
//! up (bounded at 40 s), for screenshots. `GPUI_MAP_STUDIO_PROBE_CLICK=1`
//! then clicks there too, pinning the card.
#![allow(dead_code)]

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::prelude::FluentBuilder as _;
use gpui::{
    div, point, px, rgba, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    StatefulInteractiveElement as _, Styled as _, Subscription, WeakEntity, Window,
};
use gpui_maps::map_view::{MapObject, Pick};
use gpui_maps::map_view::MapView;
use gpui_maps::overlay::{FeatureEvent, FeatureProperties, HighlightStyle, TagValue};

// ---------------------------------------------------------------------------
// Card content
// ---------------------------------------------------------------------------

/// What a card says. Built once per hovered object, not per frame.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Card {
    pub title: String,
    /// A small line under the title: what kind of thing this is.
    pub subtitle: Option<String>,
    /// A colour chip before the title, `0xRRGGBB`.
    pub swatch: Option<u32>,
    /// Label / value pairs, in order.
    pub rows: Vec<(String, String)>,
    /// How many more rows there were than the card shows.
    pub more: usize,
    /// The rows past `rows` that only the pinned card shows, scrolling
    /// (at most [`MAX_PINNED_ROWS`] in all); the hover card counts them in
    /// `more`.
    pub overflow: Vec<(String, String)>,
    /// A muted last line, e.g. what a click does.
    pub hint: Option<String>,
}

/// The height of a pinned card's action row.
pub const ACTION_H: f32 = 26.0;

/// How a number prints.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NumFmt {
    /// Whole numbers grouped (`12,345`), others to a sensible precision
    /// (`3.14`, `0.0123`).
    Auto,
    /// Rounded and grouped: `12,346`.
    Int,
    /// `n` decimals, grouped: `1,234.50`.
    Fixed(usize),
    /// A 0..1 fraction as a percentage with `n` decimals: `0.123` -> `12.3%`.
    Fraction(usize),
    /// Already a percentage, `n` decimals: `12.3` -> `12.3%`.
    Percent(usize),
    /// `1.2k`, `3.4M`, `5.6B`.
    Compact,
    /// Milliseconds since 1970 as a UTC date and time: `2024-03-01 14:05`
    /// (the date alone at midnight).
    DateTime,
    /// Milliseconds since 1970 as a UTC date: `2024-03-01`.
    Date,
    /// Milliseconds since 1970 as a UTC time of day: `14:05`.
    Time,
}

/// One row a page asks for.
#[derive(Clone, Debug)]
pub struct Field {
    pub key: String,
    pub label: String,
    pub num: NumFmt,
    pub prefix: String,
    /// Appended after a space: `km²`, `people`.
    pub unit: Option<String>,
    /// Raw value (as printed by `NumFmt::Auto`, or the string) -> label.
    pub labels: Vec<(String, String)>,
    /// Shown when the property is missing; `None` drops the row.
    pub missing: Option<String>,
    /// A number is multiplied by this before it prints (a column in
    /// millions, say).
    pub scale: f64,
}

/// A declarative card: a title field, chosen fields with units, number
/// formats and value labels, then (optionally) every other property.
#[derive(Clone, Debug)]
pub struct CardSpec {
    pub title_key: Option<String>,
    /// Title when the title field is missing.
    pub title_fallback: Option<String>,
    pub subtitle: Option<String>,
    pub fields: Vec<Field>,
    /// After the chosen fields, every other property (sorted by key).
    pub rest: bool,
    pub hide: Vec<String>,
    pub max_rows: usize,
    pub hint: Option<String>,
    pub swatch: Option<u32>,
}

/// Rows past this many collapse into "+N more".
pub const MAX_ROWS: usize = 8;
/// A value longer than this many characters is cut with an ellipsis.
pub const MAX_VALUE_CHARS: usize = 40;
/// A list (`a, b, c` or `a / b`) longer than this many items shows this
/// many and "+N".
pub const MAX_LIST_ITEMS: usize = 4;

/// Keys tried first for a default title, before the first string field.
const TITLE_KEYS: [&str; 8] = ["name", "NAME", "title", "label", "names.primary", "NAME_EN", "name_en", "ADMIN"];

impl Default for CardSpec {
    /// The default card: the first string field as the title, every other
    /// property as a key/value row.
    fn default() -> Self {
        Self { title_key: None, title_fallback: None, subtitle: None, fields: Vec::new(), rest: true, hide: Vec::new(), max_rows: MAX_ROWS, hint: None, swatch: None }
    }
}

impl CardSpec {
    /// Only what the page names: no "every other property".
    pub fn new() -> Self {
        Self { rest: false, ..Self::default() }
    }
    pub fn title(mut self, key: &str) -> Self {
        self.title_key = Some(key.into());
        self
    }
    pub fn title_or(mut self, fallback: &str) -> Self {
        self.title_fallback = Some(fallback.into());
        self
    }
    pub fn subtitle(mut self, s: &str) -> Self {
        self.subtitle = Some(s.into());
        self
    }
    pub fn swatch(mut self, rgb: u32) -> Self {
        self.swatch = Some(rgb);
        self
    }
    pub fn hint(mut self, s: &str) -> Self {
        self.hint = Some(s.into());
        self
    }
    /// A row: `key`'s value, labelled.
    pub fn field(mut self, key: &str, label: &str) -> Self {
        self.fields.push(Field { key: key.into(), label: label.into(), num: NumFmt::Auto, prefix: String::new(), unit: None, labels: Vec::new(), missing: None, scale: 1.0 });
        self
    }
    /// The last field's number format.
    pub fn num(mut self, num: NumFmt) -> Self {
        if let Some(f) = self.fields.last_mut() {
            f.num = num;
        }
        self
    }
    /// The last field's numbers are multiplied by `k` before printing.
    pub fn scale(mut self, k: f64) -> Self {
        if let Some(f) = self.fields.last_mut() {
            f.scale = k;
        }
        self
    }
    /// The last field's unit.
    pub fn unit(mut self, unit: &str) -> Self {
        if let Some(f) = self.fields.last_mut() {
            f.unit = Some(unit.into());
        }
        self
    }
    /// The last field's prefix, e.g. `$`.
    pub fn prefix(mut self, prefix: &str) -> Self {
        if let Some(f) = self.fields.last_mut() {
            f.prefix = prefix.into();
        }
        self
    }
    /// The last field's value labels: `("true", "Capital")`.
    pub fn labels(mut self, labels: &[(&str, &str)]) -> Self {
        if let Some(f) = self.fields.last_mut() {
            f.labels = labels.iter().map(|(a, b)| ((*a).into(), (*b).into())).collect();
        }
        self
    }
    /// The last field shows `text` when the property is missing.
    pub fn missing(mut self, text: &str) -> Self {
        if let Some(f) = self.fields.last_mut() {
            f.missing = Some(text.into());
        }
        self
    }
    /// After the chosen fields, every other property too.
    pub fn rest(mut self) -> Self {
        self.rest = true;
        self
    }
    pub fn hide(mut self, keys: &[&str]) -> Self {
        self.hide.extend(keys.iter().map(|k| (*k).to_string()));
        self
    }
    pub fn max_rows(mut self, n: usize) -> Self {
        self.max_rows = n;
        self
    }

    /// The card for one object's properties. `None` only when there is
    /// nothing at all to say (no properties, no title, no fields).
    pub fn format(&self, props: Option<&FeatureProperties>) -> Option<Card> {
        let empty = FeatureProperties::new();
        let props = props.unwrap_or(&empty);
        let mut keys: Vec<&String> = props.keys().collect();
        keys.sort();
        let title_key: Option<String> = match &self.title_key {
            Some(k) => Some(k.clone()),
            None => TITLE_KEYS
                .iter()
                .find(|k| matches!(props.get(**k), Some(TagValue::String(s)) if !s.is_empty()))
                .map(|k| (*k).to_string())
                .or_else(|| keys.iter().find(|k| matches!(props.get(k.as_str()), Some(TagValue::String(s)) if !s.is_empty())).map(|k| (*k).clone())),
        };
        let title = title_key.as_ref().and_then(|k| props.get(k)).map(|v| clip(&value_text(v, NumFmt::Auto), 48)).filter(|t| !t.is_empty());
        let mut rows = Vec::new();
        for f in &self.fields {
            match props.get(&f.key) {
                Some(v) => rows.push((f.label.clone(), field_text(f, v))),
                None => {
                    if let Some(m) = &f.missing {
                        rows.push((f.label.clone(), m.clone()));
                    }
                }
            }
        }
        if self.rest {
            for k in keys {
                if Some(k) == title_key.as_ref() || self.fields.iter().any(|f| &f.key == k) || self.hide.iter().any(|h| h == k) || k.starts_with('_') {
                    continue;
                }
                // A value that is JSON (Overture's nested structs and
                // arrays arrive in the tiles as JSON strings) opens into
                // labelled, indented rows.
                match nested_json(&props[k]) {
                    Some(json) => expand(k, &json, title.as_deref(), &mut rows),
                    None => rows.push((friendly_label(k).unwrap_or_else(|| pretty_key(k)), value_text(&props[k], NumFmt::Auto))),
                }
            }
        }
        let more = rows.len().saturating_sub(self.max_rows);
        let mut overflow = if more > 0 { rows.split_off(self.max_rows) } else { Vec::new() };
        overflow.truncate(MAX_PINNED_ROWS.saturating_sub(self.max_rows));
        let title = title.or_else(|| self.title_fallback.clone());
        if title.is_none() && rows.is_empty() {
            return None;
        }
        Some(Card { title: title.unwrap_or_else(|| "Feature".into()), subtitle: self.subtitle.clone(), swatch: self.swatch, rows, more, overflow, hint: self.hint.clone() })
    }
}

/// `12345.6` -> `"12,346"` at 0 decimals; grouping on the integer part.
pub fn grouped(v: f64, decimals: usize) -> String {
    if !v.is_finite() {
        return v.to_string();
    }
    let s = format!("{:.*}", decimals, v.abs());
    let (int, frac) = s.split_once('.').map_or((s.as_str(), None), |(a, b)| (a, Some(b)));
    let mut out = String::new();
    if v < 0.0 && s.chars().any(|c| c.is_ascii_digit() && c != '0') {
        out.push('-');
    }
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if let Some(frac) = frac {
        out.push('.');
        out.push_str(frac);
    }
    out
}

/// A number as `fmt` prints it.
pub fn number(v: f64, fmt: NumFmt) -> String {
    if !v.is_finite() {
        return "—".into();
    }
    match fmt {
        NumFmt::DateTime => {
            let date = gpui_maps::overlay::filter::format_date(v);
            let minutes = (v.rem_euclid(86_400_000.0) / 60_000.0).floor() as u64;
            if minutes == 0 {
                date
            } else {
                format!("{date} {:02}:{:02}", minutes / 60, minutes % 60)
            }
        }
        NumFmt::Date => gpui_maps::overlay::filter::format_date(v),
        NumFmt::Time => {
            let minutes = (v.rem_euclid(86_400_000.0) / 60_000.0).floor() as u64;
            format!("{:02}:{:02}", minutes / 60, minutes % 60)
        }
        NumFmt::Int => grouped(v.round(), 0),
        NumFmt::Fixed(n) => grouped(v, n),
        NumFmt::Fraction(n) => format!("{}%", grouped(v * 100.0, n)),
        NumFmt::Percent(n) => format!("{}%", grouped(v, n)),
        NumFmt::Compact => {
            let a = v.abs();
            let (d, s) = if a >= 1e12 { (1e12, "T") } else if a >= 1e9 { (1e9, "B") } else if a >= 1e6 { (1e6, "M") } else if a >= 1e3 { (1e3, "k") } else { (1.0, "") };
            if s.is_empty() {
                number(v, NumFmt::Auto)
            } else {
                let x = v / d;
                let t = if x.abs() >= 100.0 { format!("{x:.0}") } else { format!("{x:.1}") };
                format!("{}{s}", t.trim_end_matches(".0"))
            }
        }
        NumFmt::Auto => {
            let a = v.abs();
            if (v.fract() == 0.0 && a < 1e15) || a >= 1000.0 {
                grouped(v, 0)
            } else if a >= 1.0 {
                trim_zeros(&format!("{v:.2}"))
            } else {
                // Three significant digits under 1.
                let digits = (-a.log10()).floor() as usize + 3;
                trim_zeros(&format!("{v:.*}", digits.min(10)))
            }
        }
    }
}

fn trim_zeros(s: &str) -> String {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s.to_string()
    }
}

/// A property value as a card prints it.
pub fn value_text(v: &TagValue, fmt: NumFmt) -> String {
    match v {
        TagValue::Number(n) => number(*n, fmt),
        TagValue::Bool(b) => if *b { "yes" } else { "no" }.into(),
        TagValue::String(s) => list_or_clip(s),
    }
}

fn field_text(f: &Field, v: &TagValue) -> String {
    let raw = match v {
        TagValue::Number(n) => number(*n, NumFmt::Auto),
        TagValue::Bool(b) => b.to_string(),
        TagValue::String(s) => s.clone(),
    };
    if let Some((_, label)) = f.labels.iter().find(|(k, _)| *k == raw) {
        return label.clone();
    }
    let body = match v {
        TagValue::Number(n) => number(n * f.scale, f.num),
        other => value_text(other, f.num),
    };
    let body = if f.prefix.is_empty() { body } else { format!("{}{body}", f.prefix) };
    match &f.unit {
        Some(u) => format!("{body} {u}"),
        None => body,
    }
}

/// A long list (`a, b, c, d, e, f`) shows its first items and "+N"; any
/// other long string is cut with an ellipsis.
pub fn list_or_clip(s: &str) -> String {
    for sep in [", ", " / ", "; ", ","] {
        let items: Vec<&str> = s.split(sep).map(str::trim).filter(|x| !x.is_empty()).collect();
        if items.len() > MAX_LIST_ITEMS {
            let shown = items[..MAX_LIST_ITEMS].join(sep);
            return format!("{} +{}", clip(&shown, MAX_VALUE_CHARS), items.len() - MAX_LIST_ITEMS);
        }
    }
    clip(s, MAX_VALUE_CHARS)
}

/// At most `n` characters, the last an ellipsis when cut.
pub fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// `pop_est` / `POP_EST` / `popEst` -> `Pop est`.
pub fn pretty_key(k: &str) -> String {
    let mut words = String::new();
    let mut prev_lower = false;
    for c in k.chars() {
        if c == '_' || c == '-' || c == '.' {
            words.push(' ');
            prev_lower = false;
        } else if c.is_uppercase() && prev_lower {
            words.push(' ');
            words.extend(c.to_lowercase());
            prev_lower = false;
        } else {
            words.extend(c.to_lowercase());
            prev_lower = c.is_lowercase();
        }
    }
    let words = words.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut cs = words.chars();
    match cs.next() {
        Some(f) => f.to_uppercase().chain(cs).collect(),
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Nested values: JSON in a property, opened into rows
// ---------------------------------------------------------------------------
//
// Vector tiles carry flat key/value tags, so a source with nested data
// (Overture: `names {primary, common, rules}`, `taxonomy`, `sources [...]`,
// `speed_limits`, `access_restrictions`, ...) writes it as a JSON string.
// A card opens such a value into rows: an object's keys as labelled rows,
// indented under the object's own label; an array of scalars on one line
// when it fits, else a bulleted row per item; an array of objects as
// numbered items. Nulls and empty values drop out, long lists end in
// "+N more", and past [`MAX_DEPTH`] levels a value prints on one line.

/// Rows a pinned card shows at most, scrolling; the rest count in "+N more".
pub const MAX_PINNED_ROWS: usize = 60;
/// Items of one array (or keys of one object) shown before "+N more".
pub const MAX_ITEMS: usize = 6;
/// Levels of nesting opened into rows; deeper values print on one line.
pub const MAX_DEPTH: usize = 3;
/// One level of indent at the start of a row's label. The card turns each
/// into left padding; as plain text (the agent's view, a test) it still
/// reads indented.
pub const INDENT: char = '\u{2003}';
/// The label of an item of a list of scalars.
pub const BULLET: &str = "•";

/// Friendly labels by dotted path (array indices left out): the path, its
/// i18n key (Studio's `locales/app.yml`) and the English text.
pub const FRIENDLY: &[(&str, &str, &str)] = &[
    ("names.primary", "popup.label.name", "Name"),
    ("names.common", "popup.label.other_names", "Other names"),
    ("names.rules", "popup.label.name_variants", "Name variants"),
    ("categories.primary", "popup.label.category", "Category"),
    ("categories.alternate", "popup.label.other_categories", "Other categories"),
    ("taxonomy.primary", "popup.label.category", "Category"),
    ("taxonomy.hierarchy", "popup.label.category_path", "Category path"),
    ("taxonomy.alternates", "popup.label.other_categories", "Other categories"),
    ("basic_category", "popup.label.kind", "Kind"),
    ("road_flags", "popup.label.road_flags", "Road flags"),
    ("rail_flags", "popup.label.rail_flags", "Rail flags"),
    ("speed_limits", "popup.label.speed_limits", "Speed limits"),
    ("access_restrictions", "popup.label.access", "Access restrictions"),
    ("road_surface", "popup.label.surface", "Surface"),
    ("sources", "popup.label.sources", "Sources"),
    ("source_tags", "popup.label.osm_tags", "OSM tags"),
    ("websites", "popup.label.websites", "Websites"),
    ("phones", "popup.label.phones", "Phones"),
    ("socials", "popup.label.socials", "Social links"),
    ("emails", "popup.label.emails", "Emails"),
    ("addresses", "popup.label.addresses", "Addresses"),
    ("routes", "popup.label.routes", "Routes"),
    ("level_rules", "popup.label.level", "Level"),
    ("width_rules", "popup.label.width", "Width"),
    ("subclass_rules", "popup.label.subclass", "Subclass"),
    ("prohibited_transitions", "popup.label.turn_restrictions", "Turn restrictions"),
];

/// The "+N more" line's key and English text (`%{n}`: the count).
pub const MORE: (&str, &str) = ("popup.more", "+%{n} more");

static TRANSLATE: std::sync::OnceLock<fn(&str) -> Option<String>> = std::sync::OnceLock::new();

/// The host's translation of a key (Studio: rust-i18n); without one, or
/// for a key it lacks, the English text.
pub fn set_translate(f: fn(&str) -> Option<String>) {
    let _ = TRANSLATE.set(f);
}

fn tr(key: &str, english: &str) -> String {
    TRANSLATE.get().and_then(|f| f(key)).unwrap_or_else(|| english.to_string())
}

/// "+N more", in the host's language.
pub fn more_text(n: usize) -> String {
    tr(MORE.0, MORE.1).replace("%{n}", &n.to_string())
}

/// The friendly label of a dotted path, if it has one.
pub fn friendly_label(path: &str) -> Option<String> {
    FRIENDLY.iter().find(|(p, ..)| *p == path).map(|(_, key, en)| tr(key, en))
}

/// The JSON inside a property, when it holds an object or an array.
pub fn nested_json(v: &TagValue) -> Option<serde_json::Value> {
    let TagValue::String(s) = v else { return None };
    let t = s.trim_start();
    if !(t.starts_with('{') || t.starts_with('[')) {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(s).ok().filter(|j| j.is_object() || j.is_array())
}

/// Rows for the property `key` holding `v`. `title` is the card's title:
/// a `names.primary` equal to it is not repeated.
pub fn expand(key: &str, v: &serde_json::Value, title: Option<&str>, rows: &mut Vec<(String, String)>) {
    let label = friendly_label(key).unwrap_or_else(|| pretty_key(key));
    walk(key, &label, v, 0, title, rows);
}

fn indented(depth: usize, label: &str) -> String {
    let mut s: String = std::iter::repeat_n(INDENT, depth).collect();
    s.push_str(label);
    s
}

/// The indent depth of a row's label and the label without it.
pub fn split_indent(label: &str) -> (usize, &str) {
    let rest = label.trim_start_matches(INDENT);
    ((label.len() - rest.len()) / INDENT.len_utf8(), rest)
}

/// Null, an empty string, or an array / object of nothing but those.
fn is_empty(v: &serde_json::Value) -> bool {
    use serde_json::Value as J;
    match v {
        J::Null => true,
        J::String(s) => s.trim().is_empty(),
        J::Array(a) => a.iter().all(is_empty),
        J::Object(m) => m.values().all(is_empty),
        J::Bool(_) | J::Number(_) => false,
    }
}

fn scalar_text(v: &serde_json::Value) -> Option<String> {
    use serde_json::Value as J;
    match v {
        J::String(s) => Some(s.clone()),
        J::Number(n) => n.as_f64().map(|f| number(f, NumFmt::Auto)),
        J::Bool(b) => Some(if *b { "yes" } else { "no" }.into()),
        _ => None,
    }
}

/// A value on one line: `key: value, ...` for an object, `a, b` for an
/// array, nested ones in braces / brackets.
pub fn compact(v: &serde_json::Value) -> String {
    fn inner(v: &serde_json::Value, top: bool) -> String {
        use serde_json::Value as J;
        match v {
            J::Object(m) => {
                let s = m.iter().filter(|(_, c)| !is_empty(c)).map(|(k, c)| format!("{k}: {}", inner(c, false))).collect::<Vec<_>>().join(", ");
                if top { s } else { format!("{{{s}}}") }
            }
            J::Array(a) => {
                let s = a.iter().filter(|c| !is_empty(c)).map(|c| inner(c, false)).collect::<Vec<_>>().join(", ");
                if top { s } else { format!("[{s}]") }
            }
            other => scalar_text(other).unwrap_or_default(),
        }
    }
    inner(v, true)
}

/// `{value: 50, unit: "km/h"}` as `50 km/h`.
fn value_with_unit(m: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    if !m.contains_key("value") || m.keys().any(|k| k != "value" && k != "unit") {
        return None;
    }
    let value = scalar_text(&m["value"])?;
    Some(match m.get("unit").and_then(scalar_text) {
        Some(u) => format!("{value} {u}"),
        None => value,
    })
}

fn walk(path: &str, label: &str, v: &serde_json::Value, depth: usize, title: Option<&str>, rows: &mut Vec<(String, String)>) {
    use serde_json::Value as J;
    if is_empty(v) {
        return;
    }
    if let Some(text) = scalar_text(v) {
        if path == "names.primary" && title == Some(text.as_str()) {
            return;
        }
        rows.push((indented(depth, label), list_or_clip(&text)));
        return;
    }
    if depth >= MAX_DEPTH {
        rows.push((indented(depth, label), clip(&compact(v), MAX_VALUE_CHARS)));
        return;
    }
    match v {
        J::Object(m) => {
            if let Some(text) = value_with_unit(m) {
                rows.push((indented(depth, label), text));
                return;
            }
            // An object whose keys have friendly labels of their own
            // (`names`, `taxonomy`) is a group of properties, not a thing:
            // its keys stand at its level, with no heading.
            let hoist = m.keys().any(|k| FRIENDLY.iter().any(|(p, ..)| *p == format!("{path}.{k}")));
            let at = if hoist {
                depth
            } else {
                rows.push((indented(depth, label), String::new()));
                depth + 1
            };
            // Keys with friendly labels first, in `FRIENDLY`'s order, then
            // the rest by key: the same order whatever order the JSON had.
            let rank = |k: &str| FRIENDLY.iter().position(|(p, ..)| *p == format!("{path}.{k}")).unwrap_or(usize::MAX);
            let mut kept: Vec<(&String, &J)> = m.iter().filter(|(_, c)| !is_empty(c)).collect();
            kept.sort_by(|a, b| rank(a.0).cmp(&rank(b.0)).then_with(|| a.0.cmp(b.0)));
            for (k, c) in kept.iter().take(MAX_ITEMS) {
                let p = format!("{path}.{k}");
                let l = friendly_label(&p).unwrap_or_else(|| pretty_key(k));
                walk(&p, &l, c, at, title, rows);
            }
            if kept.len() > MAX_ITEMS {
                rows.push((indented(at, ""), more_text(kept.len() - MAX_ITEMS)));
            }
        }
        J::Array(items) => {
            let items: Vec<&J> = items.iter().filter(|c| !is_empty(c)).collect();
            if items.iter().all(|c| scalar_text(c).is_some()) {
                let texts: Vec<String> = items.iter().filter_map(|c| scalar_text(c)).collect();
                let line = texts.join(", ");
                if line.chars().count() <= MAX_VALUE_CHARS {
                    rows.push((indented(depth, label), line));
                    return;
                }
                rows.push((indented(depth, label), String::new()));
                for t in texts.iter().take(MAX_ITEMS) {
                    rows.push((indented(depth + 1, BULLET), clip(t, MAX_VALUE_CHARS)));
                }
                if texts.len() > MAX_ITEMS {
                    rows.push((indented(depth + 1, ""), more_text(texts.len() - MAX_ITEMS)));
                }
                return;
            }
            // One item: the item is the value.
            if let [only] = items.as_slice() {
                walk(path, label, only, depth, title, rows);
                return;
            }
            rows.push((indented(depth, label), String::new()));
            for (i, item) in items.iter().take(MAX_ITEMS).enumerate() {
                walk(path, &format!("{}.", i + 1), item, depth + 1, title, rows);
            }
            if items.len() > MAX_ITEMS {
                rows.push((indented(depth + 1, ""), more_text(items.len() - MAX_ITEMS)));
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Placement
// ---------------------------------------------------------------------------

/// Pointer to card edge, logical pixels: clear of the pointed-at spot and
/// of a typical point marker's radius.
pub const GAP: f32 = 20.0;
/// The card keeps this far inside the map's edges.
pub const MARGIN: f32 = 8.0;
pub const CARD_W: f32 = 264.0;
const PAD: f32 = 10.0;
const TITLE_H: f32 = 20.0;
const LINE_H: f32 = 16.0;
const ROW_GAP: f32 = 2.0;
const SECTION_GAP: f32 = 6.0;

/// Row lines a pinned card shows before its rows scroll.
pub const MAX_VISIBLE_PINNED: usize = 16;
/// Left padding per level of a nested row.
const INDENT_PX: f32 = 10.0;

fn rows_height(lines: usize) -> f32 {
    lines as f32 * LINE_H + lines.saturating_sub(1) as f32 * ROW_GAP
}

/// The rows a card shows and the count its "+N more" line names: the
/// hover card its first rows, the pinned card the overflow too.
pub fn shown_rows(card: &Card, pinned: bool) -> (Vec<&(String, String)>, usize) {
    if pinned {
        (card.rows.iter().chain(&card.overflow).collect(), card.more.saturating_sub(card.overflow.len()))
    } else {
        (card.rows.iter().collect(), card.more)
    }
}

/// The card's size as it lays out: fixed width, height from its lines
/// (every line is one line: values truncate, they never wrap); a pinned
/// card's rows scroll past [`MAX_VISIBLE_PINNED`] lines.
pub fn card_size(card: &Card, pinned: bool) -> (f32, f32) {
    let mut h = PAD * 2.0 + TITLE_H;
    if card.subtitle.is_some() {
        h += LINE_H;
    }
    let (shown, more) = shown_rows(card, pinned);
    let rows = shown.len() + usize::from(more > 0);
    if rows > 0 {
        h += SECTION_GAP + rows_height(rows).min(if pinned { rows_height(MAX_VISIBLE_PINNED) } else { f32::INFINITY });
    }
    if card.hint.is_some() || pinned {
        h += SECTION_GAP + LINE_H;
    }
    (CARD_W, h)
}

/// Where the card's top-left goes for a pointer at `pointer` (map-local):
/// `GAP` right of and below it; flipped to the left / above where it would
/// cross the right / bottom edge; clamped inside `MARGIN` of the map where
/// neither side fits. The flip keeps it off the pointed-at spot; only a
/// map smaller than the card can make the clamp cover it.
pub fn place(pointer: (f32, f32), card: (f32, f32), map: (f32, f32)) -> (f32, f32) {
    fn axis(p: f32, size: f32, extent: f32) -> f32 {
        let after = p + GAP;
        let before = p - GAP - size;
        let x = if after + size <= extent - MARGIN {
            after
        } else if before >= MARGIN {
            before
        } else if extent - p > p {
            after
        } else {
            before
        };
        x.clamp(MARGIN, (extent - MARGIN - size).max(MARGIN))
    }
    (axis(pointer.0, card.0, map.0), axis(pointer.1, card.1, map.1))
}

/// [`place`], kept off the floating cards: where the pointer is on open
/// map left of the right card column (or right of the left one), the card
/// is placed as if the map ended at the column's edge, so it flips beside
/// the pointer rather than sliding under the chat, a chart or a page
/// panel. A pointer below a short column (outside its band) places as
/// usual.
pub fn place_clear(pointer: (f32, f32), card: (f32, f32), map: (f32, f32)) -> (f32, f32) {
    let (left, right) = (left_band(), right_band());
    let lo = if pointer.0 > left { left } else { 0.0 };
    let hi = if pointer.0 < map.0 - right { map.0 - right } else { map.0 };
    if hi - lo < card.0 + 2.0 * MARGIN {
        return place(pointer, card, map);
    }
    let (x, y) = place((pointer.0 - lo, pointer.1), card, (hi - lo, map.1));
    (x + lo, y)
}

thread_local! {
    /// The floating cards' bands this frame and last (`(current, previous)`),
    /// keyed by card: how far each reaches in from the map's right (or
    /// left) edge, gap included. A popup renders before the cards that
    /// register here, so it reads both frames.
    static BANDS: std::cell::RefCell<[(std::collections::BTreeMap<&'static str, f32>, std::collections::BTreeMap<&'static str, f32>); 3]> = std::cell::RefCell::new(Default::default());
}

/// Registers a floating card at the map's right edge reaching `px` in
/// (0: not shown). `key` names it, so re-registering replaces.
#[allow(dead_code)] // not every binary that includes this floats cards
pub fn reserve_right(key: &'static str, px: f32) {
    BANDS.with(|b| {
        b.borrow_mut()[1].0.insert(key, px.max(0.0));
    });
}

/// [`reserve_right`] for the map's left edge.
#[allow(dead_code)]
pub fn reserve_left(key: &'static str, px: f32) {
    BANDS.with(|b| {
        b.borrow_mut()[0].0.insert(key, px.max(0.0));
    });
}

/// [`reserve_right`] for a strip along the map's top (a page's KPI
/// strip), reaching `px` down, gap included: the right column's floating
/// cards that cannot step left of it start below it ([`top_band`]).
#[allow(dead_code)]
pub fn reserve_top(key: &'static str, px: f32) {
    BANDS.with(|b| {
        b.borrow_mut()[2].0.insert(key, px.max(0.0));
    });
}

/// How far the strips registered by [`reserve_top`] reach down from the
/// map's top edge (0: none).
#[allow(dead_code)]
pub fn top_band() -> f32 {
    band(2)
}

/// A host's frame start (Studio's gallery render): this frame's bands
/// become the previous frame's, and the page's cards register afresh.
#[allow(dead_code)]
pub fn new_card_frame() {
    BANDS.with(|b| {
        for side in b.borrow_mut().iter_mut() {
            side.1 = std::mem::take(&mut side.0);
        }
    });
}

fn band(side: usize) -> f32 {
    BANDS.with(|b| {
        let b = b.borrow();
        let (cur, prev) = &b[side];
        cur.values().chain(prev.iter().filter(|(k, _)| !cur.contains_key(*k)).map(|(_, v)| v)).copied().fold(0.0, f32::max)
    })
}

/// How far the floating cards reach in from the map's right edge.
pub fn right_band() -> f32 {
    band(1)
}

/// [`right_band`] without the card `key` itself: where a card that
/// stands left of all the others goes (Studio's agent chart cards), read
/// without its own last width so it never pushes itself further out.
#[allow(dead_code)] // Studio only
pub fn reserved_right_except(key: &str) -> f32 {
    BANDS.with(|b| {
        let b = b.borrow();
        let (cur, prev) = &b[1];
        cur.iter().filter(|(k, _)| **k != key).map(|(_, v)| *v).chain(prev.iter().filter(|(k, _)| **k != key && !cur.contains_key(*k)).map(|(_, v)| *v)).fold(0.0, f32::max)
    })
}

/// How far the floating cards reach in from the map's left edge.
pub fn left_band() -> f32 {
    band(0)
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// A page's card for one object: `None` shows nothing (no card, no
/// highlight, no hand) -- e.g. an object the page's filter hides.
pub type FormatFn = Rc<dyn Fn(&Pick) -> Option<Card>>;

#[derive(Clone)]
pub enum Formatter {
    Spec(CardSpec),
    Custom(FormatFn),
}

impl Formatter {
    pub fn card(&self, pick: &Pick) -> Option<Card> {
        match self {
            Self::Spec(spec) => spec.format(pick.properties.as_deref()),
            Self::Custom(f) => f(pick),
        }
    }
}

impl From<CardSpec> for Formatter {
    fn from(spec: CardSpec) -> Self {
        Self::Spec(spec)
    }
}

/// What a click on an object did, when the page handles it itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClickOutcome {
    /// Pin the card (the default).
    Pin,
    /// The page acted (e.g. a cluster eased in): no pin.
    Handled,
}

pub type ClickFn = Rc<dyn Fn(&Pick, Option<(f64, f64)>, &Entity<MapView>, &mut App) -> ClickOutcome>;
pub type HighlightFn = Rc<dyn Fn(&Pick) -> HighlightStyle>;
/// A click that hit no object with a card, at a map-local point: the page
/// may still act on it (a click in the gap between its cells).
pub type MissFn = Rc<dyn Fn((f32, f32), &Entity<MapView>, &mut App) -> ClickOutcome>;
/// A pinned card's action button pressed: its id and the pinned object.
pub type ActionFn = Rc<dyn Fn(&str, &Pick, &Entity<MapView>, &mut App)>;
/// The action buttons a pinned object's card shows, `(id, label)`.
pub type ActionsFn = Rc<dyn Fn(&Pick, &App) -> Vec<(String, String)>>;

/// Which objects get which card, and what a click does.
#[derive(Clone)]
pub struct PopupConfig {
    /// By overlay source id, or arc / extrusion / altitude layer id.
    pub formats: Vec<(String, Formatter)>,
    /// Objects of any other source or layer: the default card unless set
    /// to `None` (then they show nothing).
    pub fallback: Option<Formatter>,
    pub highlight: Option<HighlightFn>,
    pub on_click: Option<ClickFn>,
    /// A click on nothing ([`MissFn`]); without one it unpins.
    pub on_miss: Option<MissFn>,
    /// Ease to a pinned object, `Some(min zoom)` -- the camera keeps its
    /// zoom if already closer. Features ease to where the click landed,
    /// arcs to their midpoint; columns and altitude points stay put.
    pub ease_on_pin: Option<f64>,
    /// The buttons a PINNED card shows under its rows (the Transit page's
    /// vehicle "Follow"), and what a press does.
    pub actions: Option<(ActionsFn, ActionFn)>,
    /// Whether the cards registered by source ([`register_source_card`])
    /// show here (default). A second popup on the same map (an agent's
    /// layer popup beside the page's own) turns it off, so one hover never
    /// shows the same card twice.
    pub registered: bool,
}

impl Default for PopupConfig {
    fn default() -> Self {
        Self { formats: Vec::new(), fallback: Some(Formatter::Spec(CardSpec::default())), highlight: None, on_click: None, on_miss: None, ease_on_pin: None, actions: None, registered: true }
    }
}

impl PopupConfig {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn source(mut self, id: &str, f: impl Into<Formatter>) -> Self {
        self.formats.push((id.into(), f.into()));
        self
    }
    pub fn custom(self, id: &str, f: impl Fn(&Pick) -> Option<Card> + 'static) -> Self {
        self.source(id, Formatter::Custom(Rc::new(f)))
    }
    /// Only the named sources show cards.
    pub fn only_listed(mut self) -> Self {
        self.fallback = None;
        self
    }
    pub fn highlight(mut self, f: impl Fn(&Pick) -> HighlightStyle + 'static) -> Self {
        self.highlight = Some(Rc::new(f));
        self
    }
    pub fn on_click(mut self, f: impl Fn(&Pick, Option<(f64, f64)>, &Entity<MapView>, &mut App) -> ClickOutcome + 'static) -> Self {
        self.on_click = Some(Rc::new(f));
        self
    }
    #[allow(dead_code)] // the pages with cells only
    pub fn on_miss(mut self, f: impl Fn((f32, f32), &Entity<MapView>, &mut App) -> ClickOutcome + 'static) -> Self {
        self.on_miss = Some(Rc::new(f));
        self
    }
    /// Action buttons on a pinned card: `list` names them for the pinned
    /// object, `run` is a press (its id and the object).
    pub fn actions(mut self, list: impl Fn(&Pick, &App) -> Vec<(String, String)> + 'static, run: impl Fn(&str, &Pick, &Entity<MapView>, &mut App) + 'static) -> Self {
        self.actions = Some((Rc::new(list), Rc::new(run)));
        self
    }
    pub fn ease_on_pin(mut self, min_zoom: f64) -> Self {
        self.ease_on_pin = Some(min_zoom);
        self
    }

    /// The card for `pick`, by its source or layer: the page's own format,
    /// else one a source registered for itself ([`register_source_card`]),
    /// else the fallback.
    pub fn card(&self, pick: &Pick) -> Option<Card> {
        let key = object_key(&pick.object);
        match self.formats.iter().find(|(id, _)| id == key) {
            Some((_, f)) => f.card(pick),
            None => match registered_card(key).filter(|_| self.registered) {
                Some(spec) => spec.format(pick.properties.as_deref()),
                None => self.fallback.as_ref().and_then(|f| f.card(pick)),
            },
        }
    }
}

thread_local! {
    static SOURCE_CARDS: std::cell::RefCell<Vec<(String, CardSpec)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// A card for every feature of overlay source `id`, on ANY page's popup
/// (a page's own format for the source still wins); `None` takes it back.
/// For layers a page did not make itself -- an agent's data layer gets the
/// fields that explain it on whichever map it lands.
pub fn register_source_card(id: &str, spec: Option<CardSpec>) {
    SOURCE_CARDS.with(|c| {
        let mut c = c.borrow_mut();
        c.retain(|(k, _)| k != id);
        if let Some(spec) = spec {
            c.push((id.to_string(), spec));
        }
    });
}

/// The card registered for `id`, if any.
pub fn registered_card(id: &str) -> Option<CardSpec> {
    SOURCE_CARDS.with(|c| c.borrow().iter().find(|(k, _)| k == id).map(|(_, s)| s.clone()))
}

/// An object's source (feature) or layer (arc, column, altitude point).
pub fn object_key(object: &MapObject) -> &str {
    match object {
        MapObject::Feature(f) => &f.source,
        MapObject::Arc { layer, .. } | MapObject::Extrusion { layer, .. } | MapObject::AltitudePoint { layer, .. } | MapObject::Trip { layer, .. } => layer,
    }
}

/// The default hover highlight: a 2 px white outline over a dark casing,
/// and a slight brighten -- legible over light and dark basemaps alike.
pub fn default_highlight() -> HighlightStyle {
    HighlightStyle { fill_color: Some(gpui_maps::style::Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.16 }), ..HighlightStyle::default() }
}

// ---------------------------------------------------------------------------
// The component
// ---------------------------------------------------------------------------

/// One object the card is about.
#[derive(Clone)]
struct Shown {
    pick: Pick,
    card: Rc<Card>,
}

struct Pinned {
    shown: Shown,
    /// Map-local top-left, fixed where it was pinned (kepler.gl's pinned
    /// tooltip: it stays put while the map moves; the highlight follows
    /// the object).
    at: (f32, f32),
}

/// The hover / pin state for one map. See the module doc.
pub struct MapPopup {
    map: WeakEntity<MapView>,
    config: PopupConfig,
    /// The input layer's window box `(x, y, w, h)` at each prepaint: the
    /// map's, as the layer covers it.
    area: Rc<Cell<(f32, f32, f32, f32)>>,
    /// The pointer, map-local, as last moved.
    pointer: Option<(f32, f32)>,
    /// Where the last pick ran and the camera it ran under: the same
    /// pointer under the same camera reuses its answer.
    picked_at: Option<((f32, f32), gpui_maps::render::Camera)>,
    last_pick: Option<Instant>,
    trailing: bool,
    hover: Option<Shown>,
    pinned: Option<Pinned>,
    /// What `set_highlight` was last asked to light.
    lit: Option<MapObject>,
    press: Option<(f32, f32)>,
    /// The last `FeatureEvent::Click`'s ground point (it lands on the
    /// press, before our release).
    click_lon_lat: Option<(f64, f64)>,
    camera: Option<gpui_maps::render::Camera>,
    /// Picks run, for the "no pick storm" test.
    picks: usize,
    /// Every render while a host records (census's `--hover-sweep`).
    pub hover_log: Option<Vec<PopupRender>>,
    /// The map notified since the last render.
    map_notified: bool,
    _subs: Vec<Subscription>,
}

/// One render of the cards, as a recording host sees it.
#[derive(Clone, Copy, Debug)]
pub struct PopupRender {
    pub at: Instant,
    /// The pointer, WINDOW coordinates.
    pub cursor: gpui::Point<gpui::Pixels>,
    /// Whether a hover card was drawn.
    pub tooltip: bool,
    /// Whether the map had asked to be redrawn since the previous render.
    pub map_notified: bool,
}

/// One pick per display frame at most.
pub const PICK_INTERVAL: Duration = Duration::from_millis(8);

/// Builds the component for `map`, and starts the
/// `GPUI_MAP_STUDIO_PROBE_HOVER` probe if it is set.
pub fn install(map: &Entity<MapView>, config: PopupConfig, _window: &mut Window, cx: &mut App) -> Entity<MapPopup> {
    cx.new(|cx| MapPopup::new(map, config, cx))
}

impl MapPopup {
    pub fn new(map: &Entity<MapView>, config: PopupConfig, cx: &mut Context<Self>) -> Self {
        let mut subs = Vec::new();
        // Esc closes, wherever the keyboard is.
        subs.push(cx.observe_keystrokes(|this: &mut Self, event, _window, cx| {
            if event.keystroke.key == "escape" {
                this.close(cx);
            }
        }));
        // A camera move ends the hover (it is stale a frame later); the
        // next pointer move picks again. A pinned card stays.
        subs.push(cx.observe(map, |this: &mut Self, map, cx| {
            this.map_notified = true;
            let camera = *map.read(cx).camera();
            if this.camera != Some(camera) {
                this.camera = Some(camera);
                if this.hover.take().is_some() {
                    this.picked_at = None;
                    this.sync_highlight(cx);
                    cx.notify();
                }
            }
        }));
        subs.push(cx.subscribe(map, |this: &mut Self, _map, event: &FeatureEvent, _cx| {
            if let FeatureEvent::Click { lon_lat, .. } = event {
                this.click_lon_lat = Some(*lon_lat);
            }
        }));
        if let Some(at) = probe_point() {
            probe(cx.entity().downgrade(), at, std::env::var("GPUI_MAP_STUDIO_PROBE_CLICK").is_ok_and(|v| v == "1"), cx);
        }
        let camera = Some(*map.read(cx).camera());
        Self {
            map: map.downgrade(),
            config,
            area: Rc::new(Cell::new((0.0, 0.0, 0.0, 0.0))),
            pointer: None,
            picked_at: None,
            last_pick: None,
            trailing: false,
            hover: None,
            pinned: None,
            lit: None,
            press: None,
            click_lon_lat: None,
            camera,
            picks: 0,
            hover_log: None,
            map_notified: false,
            _subs: subs,
        }
    }

    /// The hovered object's card, if one is up.
    pub fn hovered(&self) -> Option<(&MapObject, &Card)> {
        self.hover.as_ref().map(|s| (&s.pick.object, &*s.card))
    }
    /// The pinned object's card, if one is pinned.
    pub fn pinned(&self) -> Option<(&MapObject, &Card)> {
        self.pinned.as_ref().map(|p| (&p.shown.pick.object, &*p.shown.card))
    }
    /// The hovered object's pick (its properties too): what a linked chart
    /// reads to light the hovered object's bar.
    pub fn hovered_pick(&self) -> Option<&Pick> {
        self.hover.as_ref().map(|s| &s.pick)
    }
    /// Presses the pinned card's action `id`, as its button does: the
    /// action runs and the card closes (it has done what it offered).
    pub fn run_action(&mut self, id: &str, cx: &mut Context<Self>) {
        let (Some((_, f)), Some(pin), Some(map)) = (self.config.actions.clone(), self.pinned.as_ref().map(|p| p.shown.pick.clone()), self.map.upgrade()) else { return };
        f(id, &pin, &map, cx);
        self.close(cx);
    }

    /// The pinned card's action buttons, `(id, label)`.
    pub fn pinned_actions(&self, cx: &App) -> Vec<(String, String)> {
        match (&self.config.actions, &self.pinned) {
            (Some((list, _)), Some(p)) => list(&p.shown.pick, cx),
            _ => Vec::new(),
        }
    }

    /// The pinned object's pick.
    pub fn pinned_pick(&self) -> Option<&Pick> {
        self.pinned.as_ref().map(|p| &p.shown.pick)
    }
    /// What the component asked the map to highlight.
    pub fn lit(&self) -> Option<&MapObject> {
        self.lit.as_ref()
    }
    pub fn picks(&self) -> usize {
        self.picks
    }

    /// Closes everything: the hover card, the pinned card, the highlight.
    pub fn close(&mut self, cx: &mut Context<Self>) {
        if self.hover.is_none() && self.pinned.is_none() {
            return;
        }
        self.hover = None;
        self.pinned = None;
        self.sync_highlight(cx);
        cx.notify();
    }

    fn local(&self, window_pos: gpui::Point<gpui::Pixels>) -> (f32, f32) {
        let (ox, oy, _, _) = self.area.get();
        (f32::from(window_pos.x) - ox, f32::from(window_pos.y) - oy)
    }

    /// Re-asks the formatter for what is shown, after the page changed
    /// what it would say (a filter moved): a card it now declines closes.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let config = self.config.clone();
        let again = |s: &Shown| config.card(&s.pick).map(|card| Shown { pick: s.pick.clone(), card: Rc::new(card) });
        self.hover = self.hover.as_ref().and_then(again);
        self.pinned = self.pinned.take().and_then(|p| again(&p.shown).map(|shown| Pinned { shown, at: p.at }));
        self.sync_highlight(cx);
        cx.notify();
    }

    /// A pointer move over the map (map-local).
    pub fn pointer_moved(&mut self, at: (f32, f32), button_held: bool, cx: &mut Context<Self>) {
        self.pointer = Some(at);
        if button_held {
            // A drag pans the map: no hover while it does.
            if self.hover.take().is_some() {
                self.sync_highlight(cx);
                cx.notify();
            }
            return;
        }
        let now = Instant::now();
        match self.last_pick {
            Some(t) if now.duration_since(t) < PICK_INTERVAL => {
                // Throttled: one trailing pick at the latest pointer.
                if !self.trailing {
                    self.trailing = true;
                    let wait = PICK_INTERVAL - now.duration_since(t);
                    cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(wait).await;
                        let _ = this.update(cx, |this, cx| {
                            this.trailing = false;
                            if let Some(p) = this.pointer {
                                this.repick(p, cx);
                            }
                        });
                    })
                    .detach();
                }
            }
            _ => self.repick(at, cx),
        }
        if self.hover.is_some() {
            // The card follows the pointer.
            cx.notify();
        }
    }

    /// Picks at `at` unless the pointer and camera are what the last pick
    /// saw.
    fn repick(&mut self, at: (f32, f32), cx: &mut Context<Self>) {
        let Some(map) = self.map.upgrade() else { return };
        let (camera, moving) = {
            let view = map.read(cx);
            (*view.camera(), view.camera_in_motion())
        };
        if moving {
            if self.hover.take().is_some() {
                self.sync_highlight(cx);
                cx.notify();
            }
            return;
        }
        if self.picked_at == Some((at, camera)) {
            return;
        }
        self.picked_at = Some((at, camera));
        self.last_pick = Some(Instant::now());
        self.picks += 1;
        let pick = map.read(cx).pick(at.0, at.1);
        self.set_hover(pick, cx);
    }

    /// Takes a pick result as the hover (tests drive this directly).
    pub fn set_hover(&mut self, pick: Option<Pick>, cx: &mut Context<Self>) {
        let same = match (&self.hover, &pick) {
            (Some(h), Some(p)) => h.pick.object == p.object,
            (None, None) => true,
            _ => false,
        };
        if same {
            return;
        }
        self.hover = pick.and_then(|pick| self.config.card(&pick).map(|card| Shown { pick, card: Rc::new(card) }));
        self.sync_highlight(cx);
        cx.notify();
    }

    /// Lights the hovered object, else the pinned one, else nothing --
    /// asking the map only when that changes.
    fn sync_highlight(&mut self, cx: &mut Context<Self>) {
        let want = self.hover.as_ref().or(self.pinned.as_ref().map(|p| &p.shown));
        let object = want.map(|s| s.pick.object.clone());
        if object == self.lit {
            return;
        }
        let style = match (want, &self.config.highlight) {
            (Some(s), Some(f)) => f(&s.pick),
            _ => default_highlight(),
        };
        self.lit = object.clone();
        if let Some(map) = self.map.upgrade() {
            map.update(cx, |view, cx| {
                view.set_highlight(object, style, cx);
            });
        }
    }

    fn pressed(&mut self, at: (f32, f32)) {
        self.press = Some(at);
        self.click_lon_lat = None;
    }

    /// A left release: a click (within 4 px of its press) on an object
    /// pins its card; on nothing it unpins.
    pub fn released(&mut self, at: (f32, f32), cx: &mut Context<Self>) {
        let Some(press) = self.press.take() else { return };
        if (press.0 - at.0).hypot(press.1 - at.1) > 4.0 {
            return;
        }
        let Some(map) = self.map.upgrade() else { return };
        let pick = map.read(cx).pick(at.0, at.1);
        let shown = pick.and_then(|pick| self.config.card(&pick).map(|card| Shown { pick, card: Rc::new(card) }));
        let Some(shown) = shown else {
            if let Some(on_miss) = self.config.on_miss.clone() {
                if on_miss(at, &map, cx) == ClickOutcome::Handled {
                    if self.pinned.take().is_some() {
                        self.sync_highlight(cx);
                    }
                    cx.notify();
                    return;
                }
            }
            if self.pinned.take().is_some() {
                self.sync_highlight(cx);
                cx.notify();
            }
            return;
        };
        let lon_lat = self.click_lon_lat.or_else(|| arc_midpoint(&shown.pick));
        if let Some(on_click) = self.config.on_click.clone() {
            // The ground under the click when the map's own click event has
            // not said (an unpainted map, or the event still to come): what
            // a host reading a place from a feature click needs.
            let lon_lat = lon_lat.or_else(|| matches!(shown.pick.object, MapObject::Feature(_)).then(|| map.read(cx).lon_lat_at(at.0, at.1)).flatten());
            if on_click(&shown.pick, lon_lat, &map, cx) == ClickOutcome::Handled {
                return;
            }
        }
        self.pin(shown, at, cx);
        if let (Some(min_zoom), Some((lon, lat))) = (self.config.ease_on_pin, lon_lat) {
            map.update(cx, |view, cx| {
                let zoom = view.camera().zoom.max(min_zoom);
                view.ease_to(
                    gpui_maps::camera_control::CameraTarget { center: Some((lon, lat)), zoom: Some(zoom), ..Default::default() },
                    Duration::from_millis(700),
                );
                cx.notify();
            });
        }
    }

    /// Pins `pick`'s card as a click at `at` (map-local) would, without a
    /// pick under the pointer: a host that selects an object some other
    /// way (a table row) shows its card. `false` when the formatter
    /// declines it.
    pub fn pin_pick(&mut self, pick: Pick, at: (f32, f32), cx: &mut Context<Self>) -> bool {
        let Some(card) = self.config.card(&pick) else { return false };
        self.pin(Shown { pick, card: Rc::new(card) }, at, cx);
        true
    }

    fn pin(&mut self, shown: Shown, at: (f32, f32), cx: &mut Context<Self>) {
        let (_, _, w, h) = self.area.get();
        let mut size = card_size(&shown.card, true);
        if self.config.actions.as_ref().is_some_and(|(list, _)| !list(&shown.pick, cx).is_empty()) {
            size.1 += SECTION_GAP + ACTION_H;
        }
        let origin = place_clear(at, size, (w, h));
        self.hover = None;
        self.pinned = Some(Pinned { shown, at: origin });
        self.sync_highlight(cx);
        cx.notify();
    }

    fn left_map(&mut self, cx: &mut Context<Self>) {
        self.pointer = None;
        self.picked_at = None;
        if self.hover.take().is_some() {
            self.sync_highlight(cx);
            cx.notify();
        }
    }

    fn colours(cx: &App) -> Palette {
        if cx.try_global::<gpui_component::Theme>().is_some_and(|t| t.mode.is_dark()) {
            Palette::DARK
        } else {
            Palette::LIGHT
        }
    }
}

fn arc_midpoint(pick: &Pick) -> Option<(f64, f64)> {
    if !matches!(pick.object, MapObject::Arc { .. }) {
        return None;
    }
    let p = pick.properties.as_ref()?;
    let n = |k: &str| match p.get(k) {
        Some(TagValue::Number(v)) => Some(*v),
        _ => None,
    };
    Some(((n("source_lon")? + n("target_lon")?) / 2.0, (n("source_lat")? + n("target_lat")?) / 2.0))
}

/// The card's colours, by the app theme.
#[derive(Clone, Copy)]
struct Palette {
    bg: u32,
    border: u32,
    text: u32,
    muted: u32,
    faint: u32,
}

impl Palette {
    const LIGHT: Self = Self { bg: 0xffff_ffff, border: 0x1f293326, text: 0x1f2933ff, muted: 0x616e7cff, faint: 0x8a96a3ff };
    const DARK: Self = Self { bg: 0x1d21_27ff, border: 0xffffff26, text: 0xe6e9edff, muted: 0xa3adb8ff, faint: 0x7b8794ff };
}

/// The transparent pointer layer over the map. Put it right after the
/// map, before the page's own (occluding) cards.
pub fn input_layer(popup: &Entity<MapPopup>, cx: &App) -> gpui::AnyElement {
    let state = popup.read(cx);
    let area = state.area.clone();
    let hand = state.hover.is_some();
    let (moved, down, up, hover) = (popup.downgrade(), popup.downgrade(), popup.downgrade(), popup.downgrade());
    div()
        .id("map-popup-input")
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .child(
            gpui::canvas(
                move |bounds, _, _| {
                    area.set((f32::from(bounds.origin.x), f32::from(bounds.origin.y), f32::from(bounds.size.width), f32::from(bounds.size.height)));
                },
                |_, _, _, _| {},
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full(),
        )
        .when(hand, |d| d.cursor(gpui::CursorStyle::PointingHand))
        .on_mouse_move(move |event: &gpui::MouseMoveEvent, _window, cx| {
            let _ = moved.update(cx, |this, cx| {
                let at = this.local(event.position);
                this.pointer_moved(at, event.pressed_button.is_some(), cx);
            });
        })
        .on_mouse_down(gpui::MouseButton::Left, move |event: &gpui::MouseDownEvent, _window, cx| {
            let _ = down.update(cx, |this, _| {
                let at = this.local(event.position);
                this.pressed(at);
            });
        })
        .on_mouse_up(gpui::MouseButton::Left, move |event: &gpui::MouseUpEvent, _window, cx| {
            let _ = up.update(cx, |this, cx| {
                let at = this.local(event.position);
                this.released(at, cx);
            });
        })
        .on_hover(move |hovered: &bool, _window, cx| {
            if !*hovered {
                let _ = hover.update(cx, |this, cx| this.left_map(cx));
            }
        })
        .into_any_element()
}

fn card_element(card: &Card, p: Palette, origin: (f32, f32), pinned: bool, close: Option<WeakEntity<MapPopup>>, buttons: &[(String, String)]) -> gpui::Stateful<gpui::Div> {
    let actions = (pinned && !buttons.is_empty()).then(|| {
        let weak = close.clone();
        div().mt(px(SECTION_GAP)).h(px(ACTION_H)).flex().flex_row().gap(px(6.0)).children(buttons.iter().map(|(id, label)| {
            let (weak, id) = (weak.clone(), id.clone());
            div()
                .id(gpui::SharedString::from(format!("map-popup-action-{}", id)))
                .debug_selector({
                    let id = id.clone();
                    move || format!("map-popup-action-{id}")
                })
                .h(px(ACTION_H))
                .px(px(10.0))
                .flex()
                .items_center()
                .rounded(px(5.0))
                .cursor_pointer()
                .border_1()
                .border_color(rgba(p.border))
                .text_xs()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(rgba(p.text))
                .hover(|s| s.bg(rgba(p.border)))
                .child(label.clone())
                .on_click(move |_, _, cx| {
                    if let Some(w) = &weak {
                        let _ = w.update(cx, |this, cx| this.run_action(&id, cx));
                    }
                })
        }))
    });
    let title = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .h(px(TITLE_H))
        .children(card.swatch.map(|c| div().flex_none().w(px(10.0)).h(px(10.0)).rounded(px(2.0)).bg(rgba((c << 8) | 0xff))))
        .child(div().flex_1().min_w_0().truncate().text_sm().font_weight(gpui::FontWeight::SEMIBOLD).text_color(rgba(p.text)).child(card.title.clone()))
        .children(close.map(|weak| {
            div()
                .id("map-popup-close")
                .flex_none()
                .w(px(18.0))
                .h(px(18.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.0))
                .cursor_pointer()
                .text_color(rgba(p.muted))
                .hover(|s| s.bg(rgba(p.border)))
                .child("×")
                .on_click(move |_, _, cx| {
                    let _ = weak.update(cx, |this, cx| this.close(cx));
                })
        }));
    let subtitle = card.subtitle.as_ref().map(|s| div().h(px(LINE_H)).truncate().text_xs().text_color(rgba(p.muted)).child(s.clone()));
    let (shown, more) = shown_rows(card, pinned);
    let mut rows: Vec<gpui::Div> = shown
        .into_iter()
        .map(|(k, v)| {
            // A nested row's indent (`INDENT`s at the label's start) is
            // padding; a heading (an object or list, no value of its own)
            // reads in the value's ink.
            let (depth, label) = split_indent(k);
            let heading = v.is_empty();
            div()
                .flex()
                .flex_row()
                .h(px(LINE_H))
                .gap(px(10.0))
                .pl(px(depth as f32 * INDENT_PX))
                .text_xs()
                .child(div().flex_none().max_w(px(160.0 - depth as f32 * INDENT_PX)).truncate().text_color(rgba(if heading { p.text } else { p.muted })).child(label.to_string()))
                .child(div().flex_1().min_w_0().truncate().text_right().text_color(rgba(p.text)).child(v.clone()))
        })
        .collect();
    if more > 0 {
        rows.push(div().h(px(LINE_H)).text_xs().text_color(rgba(p.faint)).child(more_text(more)));
    }
    let scrolls = pinned && rows.len() > MAX_VISIBLE_PINNED;
    let hint = if pinned {
        Some("Esc or × to close".to_string())
    } else {
        card.hint.clone()
    };
    div()
        .id(if pinned { "map-popup-pinned" } else { "map-popup-hover" })
        .absolute()
        .left(px(origin.0))
        .top(px(origin.1))
        .w(px(CARD_W))
        .p(px(PAD))
        .flex()
        .flex_col()
        .rounded(px(8.0))
        .bg(rgba(p.bg))
        .border_1()
        .border_color(rgba(p.border))
        .shadow_md()
        .child(title)
        .children(subtitle)
        .when(!rows.is_empty(), |d| {
            let list = div().id("map-popup-rows").mt(px(SECTION_GAP)).flex().flex_col().gap(px(ROW_GAP));
            // A pinned card past its height scrolls its rows.
            let list = if scrolls { list.h(px(rows_height(MAX_VISIBLE_PINNED))).overflow_y_scroll() } else { list };
            d.child(list.children(rows.into_iter().map(|r| r.flex_none())))
        })
        .children(actions)
        .children(hint.map(|h| div().mt(px(SECTION_GAP)).h(px(LINE_H)).truncate().text_xs().text_color(rgba(p.faint)).child(h)))
}

impl Render for MapPopup {
    /// The cards, over everything. The hover card does NOT occlude -- the
    /// map under it must keep its mouse moves, or the card would hide the
    /// hover it reports; the pinned card does (its close button).
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Self::colours(cx);
        let (_, _, w, h) = self.area.get();
        let hover = self.hover.as_ref().zip(self.pointer).map(|(s, at)| {
            let origin = place_clear(at, card_size(&s.card, false), (w, h));
            card_element(&s.card, p, origin, false, None, &[])
        });
        let buttons = self.pinned_actions(cx);
        let pinned = self.pinned.as_ref().map(|pin| card_element(&pin.shown.card, p, pin.at, true, Some(cx.entity().downgrade()), &buttons).occlude());
        if let Some(log) = &mut self.hover_log {
            let (ox, oy, _, _) = self.area.get();
            let cursor = self.pointer.map_or_else(gpui::Point::default, |(x, y)| point(px(x + ox), px(y + oy)));
            log.push(PopupRender { at: Instant::now(), cursor, tooltip: hover.is_some(), map_notified: std::mem::take(&mut self.map_notified) });
        }
        div().absolute().top_0().left_0().size_full().children(pinned).children(hover)
    }
}

// ---------------------------------------------------------------------------
// Card ink: a page's own cards in the app theme
// ---------------------------------------------------------------------------

thread_local! {
    static INK_DARK: Cell<bool> = const { Cell::new(false) };
}

/// Reads the app theme for [`ink`] (the popup, the charts and the time
/// slider read it the same way): call at the top of a page's `render`,
/// before its cards are built. A window without a theme (a demo binary,
/// a test) stays light.
pub fn sync_ink(cx: &App) {
    let dark = cx.try_global::<gpui_component::Theme>().is_some_and(|t| t.mode.is_dark());
    INK_DARK.with(|d| d.set(dark));
}

/// Whether [`ink`] draws dark now.
pub fn ink_is_dark() -> bool {
    INK_DARK.with(Cell::get)
}

/// A card colour written for the light theme (`0xRRGGBB`), in the app's
/// theme: the light theme's greys map to the dark palette's (text to
/// light text, a white card to a dark one), anything else -- a legend's
/// data colour -- is itself. So a page's own cards and the shared
/// components are one theme.
pub fn ink(rgb: u32) -> gpui::Rgba {
    gpui::rgb(if ink_is_dark() { dark_ink(rgb) } else { rgb })
}

/// [`ink`] for `0xRRGGBBAA` (the alpha kept).
pub fn ink_a(rgba: u32) -> gpui::Rgba {
    let (rgb, a) = (rgba >> 8, rgba & 0xff);
    gpui::rgba(((if ink_is_dark() { dark_ink(rgb) } else { rgb }) << 8) | a)
}

/// The dark theme's colour for a light card colour (Tailwind's greys, as
/// the cards are written, onto the shared components' dark palette).
pub fn dark_ink(rgb: u32) -> u32 {
    match rgb {
        // Card backgrounds.
        0xffffff => 0x1d2127,
        0xf3f4f6 => 0x2b3038,
        0xe5e7eb => 0x343a42,
        0xd1d5db => 0x4a5058,
        0xc4c8cf => 0x4a5058,
        0xb0b4ba => 0x5a616b,
        0xa1a6ae => 0x6b7480,
        // Text, strongest first.
        0x111827 => 0xf3f4f6,
        0x1f2937 | 0x1f2933 => 0xe6e9ed,
        0x374151 => 0xd1d5db,
        0x4b5563 => 0xb6bdc6,
        0x6b7280 => 0xa3adb8,
        0x9ca3af => 0x7b8794,
        // A link, a hover tint.
        0x1d4ed8 => 0x6ba8e5,
        0x000000 => 0xffffff,
        other => other,
    }
}

// ---------------------------------------------------------------------------
// The screenshot probe
// ---------------------------------------------------------------------------

fn probe_point() -> Option<(f32, f32)> {
    let v = std::env::var("GPUI_MAP_STUDIO_PROBE_HOVER").ok()?;
    let (x, y) = v.split_once(',')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

/// Dispatches pointer moves at `at` through the window's own event path
/// until a card is up (40 s at most), then optionally a click there.
fn probe(popup: WeakEntity<MapPopup>, at: (f32, f32), click: bool, cx: &mut App) {
    cx.spawn(async move |cx| {

        let deadline = Instant::now() + Duration::from_secs(40);
        let mut jitter = 0.0f32;
        loop {
            cx.background_executor().timer(Duration::from_millis(750)).await;
            let Some(handle) = cx.update(|cx| cx.active_window().or_else(|| cx.windows().first().copied())) else { continue };
            let Ok((pos, up)) = popup.update(cx, |this, _| {
                let (ox, oy, w, h) = this.area.get();
                let x = if at.0 <= 1.0 { at.0 * w } else { at.0 };
                let y = if at.1 <= 1.0 { at.1 * h } else { at.1 };
                (point(px(ox + x + jitter), px(oy + y)), this.hover.is_some() || this.pinned.is_some())
            }) else {
                return;
            };
            if up {
                let _ = popup.update(cx, |this, _| {
                    let card = this.hover.as_ref().map(|s| s.card.clone()).or_else(|| this.pinned.as_ref().map(|p| p.shown.card.clone()));
                    if let Some(card) = card {
                        eprintln!("popup probe: card \"{}\" ({} rows) for {:?}", card.title, card.rows.len(), this.lit);
                    }
                });
                if click {
                    let _ = handle.update(cx, |_, window, cx| {
                        let (b, m) = (gpui::MouseButton::Left, gpui::Modifiers::default());
                        window.dispatch_event(gpui::PlatformInput::MouseDown(gpui::MouseDownEvent { button: b, position: pos, modifiers: m, click_count: 1, first_mouse: false }), cx);
                        window.dispatch_event(gpui::PlatformInput::MouseUp(gpui::MouseUpEvent { button: b, position: pos, modifiers: m, click_count: 1 }), cx);
                    });
                    eprintln!("popup probe: clicked at ({:.0}, {:.0})", f32::from(pos.x), f32::from(pos.y));
                }
                return;
            }
            if Instant::now() > deadline {
                eprintln!("popup probe: no card at {at:?} after 40 s");
                return;
            }
            let _ = handle.update(cx, |_, window, cx| {
                window.dispatch_event(gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent { position: pos, pressed_button: None, modifiers: gpui::Modifiers::default() }), cx);
            });
            jitter = if jitter == 0.0 { 0.5 } else { 0.0 };
        }
    })
    .detach();
}

// ---------------------------------------------------------------------------
// Cards several pages share
// ---------------------------------------------------------------------------

/// The bundled `world_cities` sample (CSV, FlatGeobuf and GeoParquet
/// pages): the city, its country, population and whether it is the
/// capital.
pub fn world_city_card() -> CardSpec {
    CardSpec::new()
        .title("name")
        .subtitle("City")
        .field("country", "Country")
        .field("population", "Population")
        .num(NumFmt::Int)
        .field("capital", "Capital")
        .labels(&[("true", "Yes"), ("false", "No")])
}

/// Natural Earth's countries (the GeoParquet spec's example file).
pub fn natural_earth_country_card() -> CardSpec {
    CardSpec::new()
        .title("name")
        .subtitle("Country")
        .field("continent", "Continent")
        .field("iso_a3", "ISO code")
        .field("pop_est", "Population")
        .num(NumFmt::Compact)
        .field("gdp_md_est", "GDP")
        .scale(1e6)
        .num(NumFmt::Compact)
        .prefix("$")
}

/// For pages with no per-object identity: the legend line that says so.
pub const AGGREGATED_NOTE: &str = "Aggregated: no per-dot details";

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_maps::overlay::FeatureRef;
    use std::sync::Arc;

    /// The hover/pinned card stands over map detail: no map through it,
    /// in either theme.
    #[test]
    fn the_card_is_opaque_in_both_themes() {
        for p in [Palette::LIGHT, Palette::DARK] {
            assert_eq!(p.bg & 0xff, 0xff, "bg {:08x} lets the map through", p.bg);
        }
    }

    fn props(pairs: &[(&str, TagValue)]) -> FeatureProperties {
        pairs.iter().map(|(k, v)| ((*k).to_string(), v.clone())).collect()
    }
    fn s(v: &str) -> TagValue {
        TagValue::String(v.into())
    }
    fn n(v: f64) -> TagValue {
        TagValue::Number(v)
    }

    #[test]
    fn numbers_format_for_reading() {
        assert_eq!(number(12345.0, NumFmt::Auto), "12,345");
        assert_eq!(number(-1234567.0, NumFmt::Auto), "-1,234,567");
        assert_eq!(number(12345.67, NumFmt::Auto), "12,346");
        assert_eq!(number(3.14659, NumFmt::Auto), "3.15");
        assert_eq!(number(2.5, NumFmt::Auto), "2.5");
        assert_eq!(number(0.012345, NumFmt::Auto), "0.0123");
        assert_eq!(number(0.5, NumFmt::Auto), "0.5");
        assert_eq!(number(1234.5, NumFmt::Fixed(2)), "1,234.50");
        assert_eq!(number(0.1234, NumFmt::Fraction(1)), "12.3%");
        assert_eq!(number(7.25, NumFmt::Percent(1)), "7.2%");
        assert_eq!(number(1_250_000.0, NumFmt::Compact), "1.2M");
        assert_eq!(number(8_000_000_000.0, NumFmt::Compact), "8B");
        assert_eq!(number(950.0, NumFmt::Compact), "950");
        assert_eq!(number(21_433_226.0 * 1e6, NumFmt::Compact), "21.4T");
        assert_eq!(number(87249.6, NumFmt::Int), "87,250");
        assert_eq!(number(f64::NAN, NumFmt::Auto), "—");
    }

    #[test]
    fn the_default_card_titles_with_a_name_and_tables_the_rest() {
        let p = props(&[("name", s("Tokyo")), ("population", n(37_400_068.0)), ("capital", TagValue::Bool(true)), ("country", s("Japan")), ("_internal", n(1.0))]);
        let card = CardSpec::default().format(Some(&p)).unwrap();
        assert_eq!(card.title, "Tokyo");
        assert_eq!(card.rows, vec![("Capital".into(), "yes".into()), ("Country".into(), "Japan".into()), ("Population".into(), "37,400,068".into())]);
        assert_eq!(card.more, 0);
    }

    #[test]
    fn the_default_title_is_the_first_string_field_by_key_when_no_name() {
        let p = props(&[("zone", s("Z")), ("area", n(2.0)), ("district", s("D"))]);
        let card = CardSpec::default().format(Some(&p)).unwrap();
        assert_eq!(card.title, "D", "first string field in key order: deterministic, not hash order");
        assert_eq!(card.rows.len(), 2);
    }

    #[test]
    fn long_tables_and_lists_and_values_are_truncated() {
        let many: Vec<(String, TagValue)> = (0..12).map(|i| (format!("k{i:02}"), n(f64::from(i)))).collect();
        let p: FeatureProperties = many.into_iter().collect();
        let card = CardSpec::default().format(Some(&p)).unwrap();
        assert_eq!(card.rows.len(), MAX_ROWS);
        assert_eq!(card.more, 12 - MAX_ROWS);
        assert_eq!(list_or_clip("a, b, c, d, e, f"), "a, b, c, d +2");
        assert_eq!(list_or_clip("EW4 / CG"), "EW4 / CG");
        let long = "x".repeat(100);
        let cut = list_or_clip(&long);
        assert_eq!(cut.chars().count(), MAX_VALUE_CHARS);
        assert!(cut.ends_with('…'));
    }

    /// Overture's nested properties arrive as JSON strings; the card opens
    /// them into labelled, indented rows: friendly labels, nulls and empty
    /// values dropped, a list of scalars on one line when it fits and
    /// bulleted when not, lists of objects numbered, long lists cut with
    /// "+N more", and past three levels one compact line.
    #[test]
    fn nested_json_values_open_into_formatted_rows() {
        let i = |d: usize, l: &str| format!("{}{l}", INDENT.to_string().repeat(d));
        let p = props(&[
            ("@name", s("Raffles Place")),
            ("class", s("subway")),
            ("names", s(r#"{"primary":"Raffles Place","common":{"en":"Raffles Place","zh":"莱佛士坊"},"rules":null}"#)),
            ("taxonomy", s(r#"{"primary":"metro_station","hierarchy":["travel","transit","metro_station"]}"#)),
            ("speed_limits", s(r#"[{"max_speed":{"value":50,"unit":"km/h"}}]"#)),
            ("access_restrictions", s(r#"[{"access_type":"denied","when":{"heading":"backward","mode":["hgv"]}},{"access_type":"allowed","when":{"during":{"days":[1,2,3],"hours":"07:00-09:00"}}}]"#)),
            ("websites", s(r#"["https://one.example/a-very-long-path","https://two.example/another-long-path","c","d","e","f","g","h"]"#)),
            ("brand", s(r#"{"names":{}}"#)),
            ("emails", s("[]")),
            ("road_flags", s(r#"[{"values":["is_bridge"],"between":null}]"#)),
        ]);
        let card = CardSpec::default().title("@name").max_rows(100).format(Some(&p)).unwrap();
        let rows: Vec<(String, String)> = card.rows.clone();
        let want: Vec<(String, String)> = [
            // access_restrictions: two items, numbered; `when` opens; the
            // third level (`during`) prints on one line.
            (i(0, "Access restrictions"), ""),
            (i(1, "1."), ""),
            (i(2, "Access type"), "denied"),
            (i(2, "When"), ""),
            (i(3, "Heading"), "backward"),
            (i(3, "Mode"), "hgv"),
            (i(1, "2."), ""),
            (i(2, "Access type"), "allowed"),
            (i(2, "When"), ""),
            (i(3, "During"), "days: [1, 2, 3], hours: 07:00-09:00"),
            // `brand` is empty all the way down: no row.
            (i(0, "Class"), "subway"),
            // `emails` is an empty list: no row.
            // `names` has friendly keys: no heading; the primary name is
            // the title already; `rules: null` drops.
            (i(0, "Other names"), ""),
            (i(1, "En"), "Raffles Place"),
            (i(1, "Zh"), "莱佛士坊"),
            // One flag object: the object is the value.
            (i(0, "Road flags"), ""),
            (i(1, "Values"), "is_bridge"),
            // One limit: the item is the value; `{value, unit}` reads as
            // one quantity.
            (i(0, "Speed limits"), ""),
            (i(1, "Max speed"), "50 km/h"),
            (i(0, "Category"), "metro_station"),
            (i(0, "Category path"), "travel, transit, metro_station"),
            // A long list: bulleted, six shown, then "+2 more".
            (i(0, "Websites"), ""),
            (i(1, BULLET), "https://one.example/a-very-long-path"),
            (i(1, BULLET), "https://two.example/another-long-path"),
            (i(1, BULLET), "c"),
            (i(1, BULLET), "d"),
            (i(1, BULLET), "e"),
            (i(1, BULLET), "f"),
            (i(1, ""), "+2 more"),
        ]
        .into_iter()
        .map(|(a, b)| (a, b.to_string()))
        .collect();
        assert_eq!(rows, want);
        assert_eq!(split_indent(&i(2, "When")), (2, "When"));
        assert!(rows.iter().all(|(_, v)| !v.contains('{') || v.contains(':')), "no raw JSON object left: {rows:?}");

        // The hover card shows the first rows; the pinned card scrolls
        // through the rest, its height capped.
        let card = CardSpec::default().title("@name").format(Some(&p)).unwrap();
        assert_eq!(card.rows.len(), MAX_ROWS);
        assert_eq!(card.more, want.len() - MAX_ROWS);
        assert_eq!(card.overflow.len(), want.len() - MAX_ROWS, "the pinned card has every row");
        assert_eq!(shown_rows(&card, true).0.len(), want.len());
        assert_eq!(shown_rows(&card, true).1, 0);
        let (_, pinned_h) = card_size(&card, true);
        assert!(pinned_h <= PAD * 2.0 + TITLE_H + SECTION_GAP * 2.0 + rows_height(MAX_VISIBLE_PINNED) + LINE_H + 0.5, "the pinned card stays within its max size: {pinned_h}");

        // Not JSON, or JSON of a scalar: printed as before.
        assert!(nested_json(&s("{not json")).is_none() && nested_json(&s("42")).is_none() && nested_json(&n(1.0)).is_none());
    }

    #[test]
    fn a_spec_picks_fields_units_formats_and_labels() {
        let p = props(&[("NAME", s("Kings County")), ("rate", n(0.0731)), ("pop", n(2_559_903.0)), ("area", n(250.4)), ("capital", TagValue::Bool(false)), ("noise", n(1.0))]);
        let spec = CardSpec::new()
            .title("NAME")
            .subtitle("County")
            .field("rate", "Unemployment")
            .num(NumFmt::Fraction(1))
            .field("pop", "Population")
            .num(NumFmt::Compact)
            .unit("people")
            .field("area", "Area")
            .num(NumFmt::Int)
            .unit("km²")
            .field("capital", "Capital")
            .labels(&[("false", "No"), ("true", "Yes")])
            .field("income", "Income")
            .missing("No estimate");
        let card = spec.format(Some(&p)).unwrap();
        assert_eq!(card.title, "Kings County");
        assert_eq!(card.subtitle.as_deref(), Some("County"));
        assert_eq!(
            card.rows,
            vec![
                ("Unemployment".into(), "7.3%".into()),
                ("Population".into(), "2.6M people".into()),
                ("Area".into(), "250 km²".into()),
                ("Capital".into(), "No".into()),
                ("Income".into(), "No estimate".into()),
            ]
        );
        assert!(!card.rows.iter().any(|(k, _)| k == "Noise"), "new() shows only the named fields");
        assert_eq!(spec.clone().rest().format(Some(&p)).unwrap().rows.last().unwrap(), &("Noise".to_string(), "1".to_string()));
    }

    #[test]
    fn nothing_to_say_is_no_card_and_a_fallback_title_fills_in() {
        assert_eq!(CardSpec::default().format(None), None);
        let card = CardSpec::new().title_or("Arc").field("count", "Trips").format(Some(&props(&[("count", n(42.0))]))).unwrap();
        assert_eq!((card.title.as_str(), card.rows[0].1.as_str()), ("Arc", "42"));
    }

    #[test]
    fn keys_read_as_labels() {
        assert_eq!(pretty_key("pop_est"), "Pop est");
        assert_eq!(pretty_key("POP_EST"), "Pop est");
        assert_eq!(pretty_key("medianIncome"), "Median income");
        assert_eq!(pretty_key("names.primary"), "Names primary");
    }

    #[test]
    fn the_card_sits_beside_the_pointer_and_flips_at_the_edges() {
        let map = (800.0, 600.0);
        let card = (CARD_W, 120.0);
        // Room right and below: GAP right of and below the pointer.
        assert_eq!(place((100.0, 100.0), card, map), (100.0 + GAP, 100.0 + GAP));
        // Near the right edge: flips left of the pointer.
        let (x, y) = place((700.0, 100.0), card, map);
        assert_eq!((x, y), (700.0 - GAP - CARD_W, 100.0 + GAP));
        // Near the bottom: flips above.
        let (_, y) = place((100.0, 560.0), card, map);
        assert_eq!(y, 560.0 - GAP - 120.0);
        // The corner: both.
        let (x, y) = place((790.0, 590.0), card, map);
        assert!(x + CARD_W <= 790.0 - GAP + 0.01 && y + 120.0 <= 590.0 - GAP + 0.01);
    }

    #[test]
    fn the_card_never_covers_the_pointer_and_stays_inside_the_map() {
        let map = (800.0, 600.0);
        for card in [(CARD_W, 80.0), (CARD_W, 200.0)] {
            for x in (0..=800).step_by(25) {
                for y in (0..=600).step_by(25) {
                    let p = (x as f32, y as f32);
                    let (l, t) = place(p, card, map);
                    assert!(l >= MARGIN - 0.01 && l + card.0 <= map.0 - MARGIN + 0.01, "x inside at {p:?}: {l}");
                    assert!(t >= MARGIN - 0.01 && t + card.1 <= map.1 - MARGIN + 0.01, "y inside at {p:?}: {t}");
                    let covers = p.0 >= l - GAP + 0.01 && p.0 <= l + card.0 + GAP - 0.01 && p.1 >= t - GAP + 0.01 && p.1 <= t + card.1 + GAP - 0.01;
                    assert!(!covers, "the card (or its gap) covers the pointer at {p:?}: {:?}", (l, t));
                }
            }
        }
    }

    #[test]
    fn a_map_smaller_than_the_card_clamps_to_the_margin() {
        assert_eq!(place((50.0, 40.0), (CARD_W, 120.0), (200.0, 100.0)), (MARGIN, MARGIN));
    }

    #[test]
    fn card_height_grows_with_its_lines() {
        let mut card = Card { title: "T".into(), ..Card::default() };
        let (_, h0) = card_size(&card, false);
        card.rows = vec![("a".into(), "1".into()); 3];
        let (_, h3) = card_size(&card, false);
        assert_eq!(h3 - h0, SECTION_GAP + 3.0 * LINE_H + 2.0 * ROW_GAP);
        assert!(card_size(&card, true).1 > h3, "a pinned card has its close line");
    }

    #[test]
    fn the_config_routes_by_source_and_layer() {
        let feature = |src: &str| Pick { object: MapObject::Feature(FeatureRef { source: src.into(), index: 0, id: None }), properties: Some(Arc::new(props(&[("name", s("X"))]))) };
        let cfg = PopupConfig::new().source("a", CardSpec::new().title("name").subtitle("A thing")).custom("b", |_| None);
        assert_eq!(cfg.card(&feature("a")).unwrap().subtitle.as_deref(), Some("A thing"));
        assert_eq!(cfg.card(&feature("b")), None, "a formatter may decline");
        assert_eq!(cfg.card(&feature("c")).unwrap().title, "X", "the fallback is the default card");
        assert_eq!(cfg.clone().only_listed().card(&feature("c")), None);
        let arc = Pick { object: MapObject::Arc { layer: "a".into(), index: 3 }, properties: None };
        assert_eq!(object_key(&arc.object), "a");
    }

    // -- the component in a window ------------------------------------------

    fn window_with_popup(app: &mut gpui::TestAppContext) -> (Entity<MapView>, Entity<MapPopup>, &mut gpui::VisualTestContext) {
        struct Host {
            map: Entity<MapView>,
            popup: Entity<MapPopup>,
        }
        impl Render for Host {
            fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                div().relative().size_full().child(input_layer(&self.popup, cx)).child(self.popup.clone())
            }
        }
        let rt = tokio::runtime::Runtime::new().expect("a runtime");
        let handle = rt.handle().clone();
        std::mem::forget(rt);
        let (host, cx) = app.add_window_view(|_, cx| {
            let style = gpui_maps::style::Style { layers: vec![], sprite: None, sky: Default::default() };
            let map = cx.new(|_| MapView::new(103.85, 1.30, 13.0, 800.0, 600.0, style, Arc::new(gpui_maps::tile_source::EmptyTileSource), handle));
            let popup = cx.new(|cx| MapPopup::new(&map, PopupConfig::new(), cx));
            Host { map, popup }
        });
        let (map, popup) = host.read_with(cx, |h, _| (h.map.clone(), h.popup.clone()));
        cx.run_until_parked();
        (map, popup, cx)
    }

    fn square_pick() -> Pick {
        Pick { object: MapObject::Feature(FeatureRef { source: "blocks".into(), index: 0, id: None }), properties: Some(Arc::new(props(&[("name", s("Square")), ("pop", n(1200.0))]))) }
    }

    /// Hover sets the highlight (what the component asks the map to light)
    /// and the card; Esc clears both. (A gpui test window draws no map, so
    /// the hover is handed in as the pick result rather than picked.)
    #[test]
    fn hover_lights_the_object_and_esc_clears_it() {
        let mut app = gpui::TestAppContext::single();
        let (_map, popup, cx) = window_with_popup(&mut app);
        popup.update(cx, |p, cx| p.set_hover(Some(square_pick()), cx));
        popup.read_with(cx, |p, _| {
            assert_eq!(p.lit(), Some(&square_pick().object));
            assert_eq!(p.hovered().unwrap().1.title, "Square");
        });
        cx.simulate_keystrokes("escape");
        popup.read_with(cx, |p, _| {
            assert!(p.hovered().is_none() && p.lit().is_none(), "Esc closes the card and clears the highlight");
        });
    }

    /// A pointer that has not moved reuses the last pick: a run of
    /// identical moves picks once.
    #[test]
    fn an_unmoved_pointer_does_not_pick_again() {
        let mut app = gpui::TestAppContext::single();
        let (_map, popup, cx) = window_with_popup(&mut app);
        let at = point(px(200.0), px(150.0));
        for _ in 0..20 {
            cx.simulate_mouse_move(at, None, gpui::Modifiers::default());
        }
        cx.run_until_parked();
        let picks = popup.read_with(cx, |p, _| p.picks());
        assert_eq!(picks, 1, "twenty identical moves, one pick");
        // A drag never picks.
        cx.simulate_mouse_move(point(px(260.0), px(150.0)), Some(gpui::MouseButton::Left), gpui::Modifiers::default());
        cx.run_until_parked();
        assert_eq!(popup.read_with(cx, |p, _| p.picks()), 1);
    }

    /// A click elsewhere unpins; a pinned card survives the hover leaving.
    #[test]
    fn a_pinned_card_stays_until_closed() {
        let mut app = gpui::TestAppContext::single();
        let (_map, popup, cx) = window_with_popup(&mut app);
        popup.update(cx, |p, cx| {
            p.area.set((0.0, 0.0, 800.0, 600.0));
            let shown = Shown { pick: square_pick(), card: Rc::new(p.config.card(&square_pick()).unwrap()) };
            p.pin(shown, (100.0, 100.0), cx);
            p.set_hover(None, cx);
        });
        popup.read_with(cx, |p, _| {
            assert_eq!(p.pinned().unwrap().1.title, "Square");
            assert_eq!(p.lit(), Some(&square_pick().object), "the pinned object stays lit");
        });
        // An empty click (the test map picks nothing) unpins.
        let at = point(px(400.0), px(300.0));
        cx.simulate_mouse_down(at, gpui::MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_up(at, gpui::MouseButton::Left, gpui::Modifiers::default());
        popup.read_with(cx, |p, _| assert!(p.pinned().is_none() && p.lit().is_none()));
    }
}

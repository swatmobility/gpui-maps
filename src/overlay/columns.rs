//! A source's feature properties as COLUMNS: one typed column per key,
//! a row per feature.
//!
//! # Why
//!
//! Each feature used to carry an `Arc<FeatureProperties>` -- a
//! `HashMap<String, TagValue>` with its own `String` keys. A national
//! block-group push held one of those per distinct property set, and paint
//! resolution walked them one string lookup at a time. Here every key name
//! is interned once per source, each key's values are one typed `Vec`, and
//! a feature is just a row number (its `OverlayFeature::index`).
//!
//! # Shape
//!
//! - `Number`: `Vec<f64>`.
//! - `Bool`: a bitmap.
//! - `Str`: dictionary-encoded, `u32` codes into the column's string table.
//! - `Mixed`: the fallback for a key whose values are not all one type --
//!   a `Cell` per row, strings still dictionary-encoded.
//!
//! Every column also has a validity bitmap; a missing value is a clear bit
//! (and a filler in the values). A column is typed by the first value it
//! sees and promoted to `Mixed` the first time a different type arrives.
//!
//! See `docs/superpowers/specs/2026-09-26-columnar-overlay-properties-design.md`.

use std::collections::HashMap;
use std::sync::Arc;

use crate::mvt::TagValue;

use super::source_paint::FeatureProperties;

/// One property value, borrowed from a table (or from a map).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Cell<'a> {
    Number(f64),
    Bool(bool),
    Str(&'a str),
}

impl<'a> Cell<'a> {
    pub(crate) fn of(value: &'a TagValue) -> Self {
        match value {
            TagValue::Number(n) => Self::Number(*n),
            TagValue::Bool(b) => Self::Bool(*b),
            TagValue::String(s) => Self::Str(s),
        }
    }

    pub(crate) fn to_tag(self) -> TagValue {
        match self {
            Self::Number(n) => TagValue::Number(n),
            Self::Bool(b) => TagValue::Bool(b),
            Self::Str(s) => TagValue::String(s.to_string()),
        }
    }
}

/// A caller's typed column, for [`crate::overlay::OverlayStore::set_source_columns`]:
/// one entry per feature, `None` where that feature has no value.
#[derive(Debug, Clone, Copy)]
pub enum PropertyColumn<'a> {
    Number(&'a [Option<f64>]),
    Bool(&'a [Option<bool>]),
    Str(&'a [Option<&'a str>]),
}

impl PropertyColumn<'_> {
    fn len(&self) -> usize {
        match self {
            Self::Number(values) => values.len(),
            Self::Bool(values) => values.len(),
            Self::Str(values) => values.len(),
        }
    }
}

#[derive(Debug, Clone, Default)]
struct Bitmap {
    words: Vec<u64>,
    len: usize,
}

impl Bitmap {
    fn push(&mut self, bit: bool) {
        if self.len.is_multiple_of(64) {
            self.words.push(0);
        }
        if bit {
            self.words[self.len / 64] |= 1 << (self.len % 64);
        }
        self.len += 1;
    }

    fn get(&self, at: usize) -> bool {
        at < self.len && self.words[at / 64] & (1 << (at % 64)) != 0
    }

    fn set(&mut self, at: usize, bit: bool) {
        while self.len <= at {
            self.push(false);
        }
        if bit {
            self.words[at / 64] |= 1 << (at % 64);
        } else {
            self.words[at / 64] &= !(1 << (at % 64));
        }
    }

    fn bytes(&self) -> usize {
        self.words.capacity() * 8
    }
}

/// A `Mixed` column's per-row value. A missing row holds `Number(0.0)`
/// behind a clear validity bit.
#[derive(Debug, Clone, Copy)]
enum Mixed {
    Number(f64),
    Bool(bool),
    Str(u32),
}

#[derive(Debug, Clone)]
enum Values {
    Number(Vec<f64>),
    Bool(Bitmap),
    Str(Vec<u32>),
    Mixed(Vec<Mixed>),
}

#[derive(Debug, Clone)]
struct Column {
    values: Values,
    valid: Bitmap,
    dict: Vec<Box<str>>,
}

impl Column {
    fn len(&self) -> usize {
        self.valid.len
    }

    fn cell(&self, row: usize) -> Option<Cell<'_>> {
        if !self.valid.get(row) {
            return None;
        }
        Some(match &self.values {
            Values::Number(values) => Cell::Number(values[row]),
            Values::Bool(bits) => Cell::Bool(bits.get(row)),
            Values::Str(codes) => Cell::Str(&self.dict[codes[row] as usize]),
            Values::Mixed(cells) => match cells[row] {
                Mixed::Number(n) => Cell::Number(n),
                Mixed::Bool(b) => Cell::Bool(b),
                Mixed::Str(code) => Cell::Str(&self.dict[code as usize]),
            },
        })
    }

    /// A hashable, collision-free stand-in for the row's value: a type tag
    /// and a payload (dictionary code, f64 bits, or the bool). Tag 0 is
    /// "missing".
    fn key(&self, row: usize) -> (u8, u64) {
        if !self.valid.get(row) {
            return (0, 0);
        }
        match &self.values {
            Values::Number(values) => (1, values[row].to_bits()),
            Values::Bool(bits) => (2, u64::from(bits.get(row))),
            Values::Str(codes) => (3, u64::from(codes[row])),
            Values::Mixed(cells) => match cells[row] {
                Mixed::Number(n) => (1, n.to_bits()),
                Mixed::Bool(b) => (2, u64::from(b)),
                Mixed::Str(code) => (3, u64::from(code)),
            },
        }
    }

    fn bytes(&self) -> usize {
        let values = match &self.values {
            Values::Number(values) => values.capacity() * 8,
            Values::Bool(bits) => bits.bytes(),
            Values::Str(codes) => codes.capacity() * 4,
            Values::Mixed(cells) => cells.capacity() * std::mem::size_of::<Mixed>(),
        };
        values
            + self.valid.bytes()
            + self.dict.capacity() * std::mem::size_of::<Box<str>>()
            + self.dict.iter().map(|s| s.len()).sum::<usize>()
    }
}

/// One source generation's properties. See the module doc.
#[derive(Debug, Clone, Default)]
pub struct PropertyTable {
    rows: usize,
    keys: Vec<Box<str>>,
    by_key: HashMap<Box<str>, u32>,
    columns: Vec<Column>,
    /// Tombstones: rows whose feature a live update removed
    /// (`OverlayStore::update_source_features`). Empty until the first.
    dead: Bitmap,
    /// `Self::bytes`, worked out once when the table is built.
    bytes: usize,
}

/// One cell write for [`PropertyTable::edited`]: `(row, key, value)`,
/// `None` clearing the key for that row.
pub(crate) type CellWrite<'a> = (usize, &'a str, Option<&'a TagValue>);

impl PropertyTable {
    /// This table grown to `rows` rows (every new cell missing), the
    /// `writes` applied in order, and the `kill` rows tombstoned -- the
    /// live-update path. Dictionaries only grow; a column that meets a
    /// value of another type is promoted to `Mixed`, as at build.
    pub(crate) fn edited(&self, rows: usize, writes: &[CellWrite<'_>], kill: &[usize]) -> Self {
        let mut t = self.clone();
        t.rows = t.rows.max(rows);
        // Per column: string -> code, built only for a string column written to.
        let mut codes: HashMap<usize, HashMap<Box<str>, u32>> = HashMap::new();
        for &(row, key, value) in writes {
            debug_assert!(row < t.rows);
            let at = match (t.by_key.get(key), value) {
                (Some(&at), _) => at as usize,
                (None, None) => continue,
                (None, Some(v)) => {
                    let values = match v {
                        TagValue::Number(_) => Values::Number(Vec::new()),
                        TagValue::Bool(_) => Values::Bool(Bitmap::default()),
                        TagValue::String(_) => Values::Str(Vec::new()),
                    };
                    t.push_column(key, Column { values, valid: Bitmap::default(), dict: Vec::new() });
                    t.columns.len() - 1
                }
            };
            let column = &mut t.columns[at];
            pad(column, t.rows);
            let Some(value) = value else {
                column.valid.set(row, false);
                continue;
            };
            let cell = Cell::of(value);
            let same_type = matches!(
                (&column.values, cell),
                (Values::Number(_), Cell::Number(_))
                    | (Values::Bool(_), Cell::Bool(_))
                    | (Values::Str(_), Cell::Str(_))
                    | (Values::Mixed(_), _)
            );
            if !same_type {
                promote(column);
            }
            let mut code = |column: &mut Column, text: &str| -> u32 {
                let map = codes.entry(at).or_insert_with(|| {
                    column.dict.iter().enumerate().map(|(i, s)| (s.clone(), i as u32)).collect()
                });
                if let Some(&c) = map.get(text) {
                    return c;
                }
                column.dict.push(text.into());
                let c = (column.dict.len() - 1) as u32;
                map.insert(text.into(), c);
                c
            };
            match cell {
                Cell::Number(n) => match &mut column.values {
                    Values::Number(v) => v[row] = n,
                    Values::Mixed(v) => v[row] = Mixed::Number(n),
                    _ => unreachable!("promoted above"),
                },
                Cell::Bool(b) => match &mut column.values {
                    Values::Bool(bits) => bits.set(row, b),
                    Values::Mixed(v) => v[row] = Mixed::Bool(b),
                    _ => unreachable!("promoted above"),
                },
                Cell::Str(text) => {
                    let c = code(column, text);
                    match &mut column.values {
                        Values::Str(v) => v[row] = c,
                        Values::Mixed(v) => v[row] = Mixed::Str(c),
                        _ => unreachable!("promoted above"),
                    }
                }
            }
            column.valid.set(row, true);
        }
        for column in &mut t.columns {
            pad(column, t.rows);
        }
        for &row in kill {
            t.dead.set(row, true);
            for column in &mut t.columns {
                column.valid.set(row, false);
            }
        }
        t.sealed()
    }

    /// Whether `row` is a tombstone.
    pub(crate) fn is_dead(&self, row: usize) -> bool {
        self.dead.get(row)
    }

    /// Tombstoned rows.
    pub(crate) fn dead_rows(&self) -> usize {
        self.dead.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    /// Only the rows `keep` marks, renumbered in order: compaction. Every
    /// kept row's values are unchanged; dictionaries are re-interned.
    pub(crate) fn compacted(&self, keep: &[bool]) -> Self {
        let rows: Vec<usize> = (0..self.rows).filter(|&r| keep.get(r).copied().unwrap_or(false)).collect();
        let mut builder = Builder::default();
        for &row in &rows {
            for (key, column) in self.keys.iter().zip(&self.columns) {
                if let Some(cell) = column.cell(row) {
                    builder.set(key, cell);
                }
            }
            builder.next_row();
        }
        let mut t = builder.finish();
        // Keep every key, even one no kept row holds, so bound expressions
        // resolve the same columns.
        for key in &self.keys {
            if !t.by_key.contains_key(key) {
                t.push_column(key, Column { values: Values::Number(Vec::new()), valid: Bitmap::default(), dict: Vec::new() });
                let last = t.columns.len() - 1;
                pad(&mut t.columns[last], t.rows);
            }
        }
        t.sealed()
    }
}

impl PropertyTable {
    /// The shared table with no rows and no keys: every read is "missing".
    pub(crate) fn empty() -> Arc<Self> {
        static EMPTY: std::sync::OnceLock<Arc<PropertyTable>> = std::sync::OnceLock::new();
        Arc::clone(EMPTY.get_or_init(|| Arc::new(PropertyTable::default())))
    }

    /// Builds the columns from per-feature maps, row `i` from `rows[i]`.
    pub(crate) fn from_maps<'a>(rows: impl IntoIterator<Item = &'a FeatureProperties>) -> Self {
        let mut builder = Builder::default();
        for map in rows {
            for (key, value) in map {
                builder.set(key, Cell::of(value));
            }
            builder.next_row();
        }
        builder.finish()
    }

    /// Builds the columns from GeoJSON `properties` objects, with the same
    /// mapping `properties_from_json` applies: strings, numbers and bools
    /// are kept; `null`, arrays and objects are dropped.
    pub(crate) fn from_json<'a>(rows: impl IntoIterator<Item = &'a serde_json::Map<String, serde_json::Value>>) -> Self {
        let mut builder = Builder::default();
        for map in rows {
            for (key, value) in map {
                let cell = match value {
                    serde_json::Value::String(text) => Cell::Str(text),
                    serde_json::Value::Number(number) => match number.as_f64() {
                        Some(n) => Cell::Number(n),
                        None => continue,
                    },
                    serde_json::Value::Bool(flag) => Cell::Bool(*flag),
                    _ => continue,
                };
                builder.set(key, cell);
            }
            builder.next_row();
        }
        builder.finish()
    }

    /// Builds the table straight from typed columns. Every column must be
    /// `rows` long, and no key may repeat.
    pub(crate) fn from_columns(rows: usize, columns: &[(&str, PropertyColumn<'_>)]) -> Result<Self, String> {
        let mut table = Self { rows, ..Self::default() };
        for (key, column) in columns {
            if column.len() != rows {
                return Err(format!("property column {key:?} has {} values for {rows} features", column.len()));
            }
            if table.by_key.contains_key(*key) {
                return Err(format!("property column {key:?} is given twice"));
            }
            let mut valid = Bitmap::default();
            let mut dict = Vec::new();
            let values = match column {
                PropertyColumn::Number(values) => Values::Number(
                    values
                        .iter()
                        .map(|v| {
                            valid.push(v.is_some());
                            v.unwrap_or(0.0)
                        })
                        .collect(),
                ),
                PropertyColumn::Bool(values) => {
                    let mut bits = Bitmap::default();
                    for v in *values {
                        valid.push(v.is_some());
                        bits.push(v.unwrap_or(false));
                    }
                    Values::Bool(bits)
                }
                PropertyColumn::Str(values) => {
                    let mut codes_of: HashMap<&str, u32> = HashMap::new();
                    Values::Str(
                        values
                            .iter()
                            .map(|v| {
                                valid.push(v.is_some());
                                v.map_or(0, |text| {
                                    *codes_of.entry(text).or_insert_with(|| {
                                        dict.push(Box::<str>::from(text));
                                        (dict.len() - 1) as u32
                                    })
                                })
                            })
                            .collect(),
                    )
                }
            };
            table.push_column(key, Column { values, valid, dict });
        }
        Ok(table.sealed())
    }

    fn push_column(&mut self, key: &str, column: Column) {
        let at = self.columns.len() as u32;
        self.keys.push(key.into());
        self.by_key.insert(key.into(), at);
        self.columns.push(column);
    }

    pub(crate) fn rows(&self) -> usize {
        self.rows
    }

    /// The column index of `key`, resolved once per expression rather than
    /// per feature.
    pub(crate) fn column(&self, key: &str) -> Option<u32> {
        self.by_key.get(key).copied()
    }

    /// Row `row`'s value in column `column`, or `None` when missing.
    pub(crate) fn cell(&self, column: u32, row: usize) -> Option<Cell<'_>> {
        self.columns.get(column as usize)?.cell(row)
    }

    /// See `Column::key`.
    pub(crate) fn cell_key(&self, column: Option<u32>, row: usize) -> (u8, u64) {
        column.and_then(|c| self.columns.get(c as usize)).map_or((0, 0), |c| c.key(row))
    }

    /// A pure string column's codes and dictionary, for resolving a
    /// `["get", key]` colour once per distinct string.
    pub(crate) fn dictionary(&self, column: u32) -> Option<(&[u32], &[Box<str>])> {
        let column = self.columns.get(column as usize)?;
        match &column.values {
            Values::Str(codes) => Some((codes, &column.dict)),
            _ => None,
        }
    }

    pub(crate) fn is_valid(&self, column: u32, row: usize) -> bool {
        self.columns.get(column as usize).is_some_and(|c| c.valid.get(row))
    }

    /// Row `row` as a map: what a caller who asks for one hovered or
    /// clicked feature's properties gets. Built on demand; nothing keeps it.
    pub(crate) fn row_map(&self, row: usize) -> FeatureProperties {
        self.keys
            .iter()
            .zip(&self.columns)
            .filter_map(|(key, column)| Some((key.to_string(), column.cell(row)?.to_tag())))
            .collect()
    }

    /// Heap bytes held, capacity-based. O(1): counted once at build.
    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    fn sealed(mut self) -> Self {
        self.bytes = self.count_bytes();
        self
    }

    fn count_bytes(&self) -> usize {
        let keys: usize = self.keys.iter().map(|k| k.len() * 2).sum();
        keys + self.keys.capacity() * std::mem::size_of::<Box<str>>()
            + crate::render::hashmap_table_bytes::<Box<str>, u32>(self.by_key.capacity())
            + self.columns.capacity() * std::mem::size_of::<Column>()
            + self.columns.iter().map(Column::bytes).sum::<usize>()
            + self.dead.bytes()
    }
}

/// Row-at-a-time construction, for the doors that hand in maps.
#[derive(Default)]
struct Builder {
    table: PropertyTable,
    /// Per column: string -> dictionary code. Build-time only.
    codes: Vec<HashMap<Box<str>, u32>>,
}

impl Builder {
    fn set(&mut self, key: &str, cell: Cell<'_>) {
        let row = self.table.rows;
        let at = match self.table.by_key.get(key) {
            Some(&at) => at as usize,
            None => {
                let values = match cell {
                    Cell::Number(_) => Values::Number(Vec::new()),
                    Cell::Bool(_) => Values::Bool(Bitmap::default()),
                    Cell::Str(_) => Values::Str(Vec::new()),
                };
                self.table.push_column(key, Column { values, valid: Bitmap::default(), dict: Vec::new() });
                self.codes.push(HashMap::new());
                self.table.columns.len() - 1
            }
        };
        let column = &mut self.table.columns[at];
        pad(column, row);
        if column.len() > row {
            // A key twice in one row: a map cannot do that, so this is
            // unreachable from the doors; keep the first.
            return;
        }
        let code = |codes: &mut HashMap<Box<str>, u32>, dict: &mut Vec<Box<str>>, text: &str| {
            if let Some(&code) = codes.get(text) {
                return code;
            }
            dict.push(text.into());
            let code = (dict.len() - 1) as u32;
            codes.insert(text.into(), code);
            code
        };
        let same_type = matches!(
            (&column.values, cell),
            (Values::Number(_), Cell::Number(_))
                | (Values::Bool(_), Cell::Bool(_))
                | (Values::Str(_), Cell::Str(_))
                | (Values::Mixed(_), _)
        );
        if !same_type {
            promote(column);
        }
        match (&mut column.values, cell) {
            (Values::Number(values), Cell::Number(n)) => values.push(n),
            (Values::Bool(bits), Cell::Bool(b)) => bits.push(b),
            (Values::Str(codes), Cell::Str(text)) => {
                codes.push(code(&mut self.codes[at], &mut column.dict, text));
            }
            (Values::Mixed(cells), cell) => cells.push(match cell {
                Cell::Number(n) => Mixed::Number(n),
                Cell::Bool(b) => Mixed::Bool(b),
                Cell::Str(text) => Mixed::Str(code(&mut self.codes[at], &mut column.dict, text)),
            }),
            _ => unreachable!("promoted above"),
        }
        column.valid.push(true);
    }

    fn next_row(&mut self) {
        self.table.rows += 1;
    }

    fn finish(mut self) -> PropertyTable {
        let rows = self.table.rows;
        for column in &mut self.table.columns {
            pad(column, rows);
            match &mut column.values {
                Values::Number(values) => values.shrink_to_fit(),
                Values::Str(codes) => codes.shrink_to_fit(),
                Values::Mixed(cells) => cells.shrink_to_fit(),
                Values::Bool(bits) => bits.words.shrink_to_fit(),
            }
            column.valid.words.shrink_to_fit();
            column.dict.shrink_to_fit();
        }
        self.table.sealed()
    }
}

/// Fills rows before `row` as missing.
fn pad(column: &mut Column, row: usize) {
    while column.len() < row {
        match &mut column.values {
            Values::Number(values) => values.push(0.0),
            Values::Bool(bits) => bits.push(false),
            Values::Str(codes) => codes.push(0),
            Values::Mixed(cells) => cells.push(Mixed::Number(0.0)),
        }
        column.valid.push(false);
    }
}

/// Rewrites a typed column as `Mixed`, keeping every row.
fn promote(column: &mut Column) {
    let cells = match &column.values {
        Values::Number(values) => values.iter().map(|&n| Mixed::Number(n)).collect(),
        Values::Bool(bits) => (0..bits.len).map(|at| Mixed::Bool(bits.get(at))).collect(),
        Values::Str(codes) => codes.iter().map(|&code| Mixed::Str(code)).collect(),
        Values::Mixed(_) => return,
    };
    column.values = Values::Mixed(cells);
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn map(entries: &[(&str, TagValue)]) -> FeatureProperties {
        entries.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    /// A tiny deterministic generator, so the round trip covers many shapes
    /// without a dependency.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            self.0 >> 33
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    pub(crate) fn random_maps(seed: u64, rows: usize) -> Vec<FeatureProperties> {
        let mut rng = Lcg(seed);
        let keys = ["a", "b", "route", "income", "flag", "mixed", "colour"];
        let colours = ["#ff0000", "red", "rgba(0,0,255,0.5)", "not a colour", "#0f0"];
        (0..rows)
            .map(|_| {
                let mut m = FeatureProperties::new();
                for key in keys {
                    if rng.below(4) == 0 {
                        continue; // missing
                    }
                    let kind = match key {
                        "route" => 2,
                        "income" => 0,
                        "flag" => 1,
                        "colour" => 3,
                        _ => rng.below(3),
                    };
                    let value = match kind {
                        0 => TagValue::Number((rng.below(2000) as f64 - 500.0) * 125.0),
                        1 => TagValue::Bool(rng.below(2) == 0),
                        2 => TagValue::String(format!("s{}", rng.below(7))),
                        _ => TagValue::String(colours[rng.below(colours.len() as u64) as usize].to_string()),
                    };
                    m.insert(key.to_string(), value);
                }
                m
            })
            .collect()
    }

    #[test]
    fn edits_tombstones_and_compaction_round_trip() {
        let mut maps = random_maps(3, 200);
        let table = PropertyTable::from_maps(&maps);
        // Append 20 rows, write values (new key, type change, clear), kill 30.
        maps.extend((0..20).map(|_| FeatureProperties::new()));
        let mut writes: Vec<(usize, String, Option<TagValue>)> = Vec::new();
        for row in (0..220).step_by(7) {
            writes.push((row, "income".into(), Some(TagValue::Number(row as f64 * 3.5))));
            writes.push((row, "new".into(), Some(TagValue::String(format!("n{}", row % 5)))));
            writes.push((row, "route".into(), if row % 2 == 0 { None } else { Some(TagValue::Bool(true)) }));
        }
        for (row, key, value) in &writes {
            match value {
                Some(v) => {
                    maps[*row].insert(key.clone(), v.clone());
                }
                None => {
                    maps[*row].remove(key);
                }
            }
        }
        let kill: Vec<usize> = (0..220).step_by(7).take(30).map(|r| (r + 3) % 220).collect();
        let borrowed: Vec<CellWrite<'_>> = writes.iter().map(|(r, k, v)| (*r, k.as_str(), v.as_ref())).collect();
        let edited = table.edited(220, &borrowed, &kill);
        assert_eq!(edited.rows(), 220);
        assert_eq!(edited.dead_rows(), kill.len());
        for (row, m) in maps.iter().enumerate() {
            if kill.contains(&row) {
                assert!(edited.is_dead(row) && edited.row_map(row).is_empty());
            } else {
                assert_eq!(&edited.row_map(row), m, "row {row}");
            }
        }
        assert!(table.row_map(0) != edited.row_map(0) || !maps[0].contains_key("new"), "the original is untouched");
        let keep: Vec<bool> = (0..220).map(|r| !kill.contains(&r)).collect();
        let compact = edited.compacted(&keep);
        assert_eq!(compact.rows(), 220 - kill.len());
        assert_eq!(compact.dead_rows(), 0);
        let live: Vec<&FeatureProperties> = maps.iter().enumerate().filter(|(r, _)| !kill.contains(r)).map(|(_, m)| m).collect();
        for (row, m) in live.iter().enumerate() {
            assert_eq!(&compact.row_map(row), *m, "compacted row {row}");
        }
        for key in ["income", "new", "route"] {
            assert!(compact.column(key).is_some());
        }
    }

    #[test]
    fn every_property_of_every_row_reads_back_equal() {
        for seed in 0..20 {
            let maps = random_maps(seed, 300);
            let table = PropertyTable::from_maps(&maps);
            assert_eq!(table.rows(), maps.len());
            for (row, m) in maps.iter().enumerate() {
                assert_eq!(&table.row_map(row), m, "seed {seed} row {row}");
            }
        }
    }

    #[test]
    fn a_key_with_mixed_types_and_gaps_round_trips() {
        let maps = vec![
            map(&[("k", TagValue::Number(1.5))]),
            map(&[]),
            map(&[("k", TagValue::String("x".into()))]),
            map(&[("k", TagValue::Bool(true)), ("late", TagValue::Number(f64::NAN))]),
            map(&[("k", TagValue::String("x".into()))]),
        ];
        let table = PropertyTable::from_maps(&maps);
        for (row, m) in maps.iter().enumerate() {
            let back = table.row_map(row);
            assert_eq!(back.len(), m.len());
            for (key, value) in m {
                match (value, back.get(key)) {
                    (TagValue::Number(a), Some(TagValue::Number(b))) if a.is_nan() => assert!(b.is_nan()),
                    (a, b) => assert_eq!(Some(a), b),
                }
            }
        }
        let k = table.column("k").unwrap();
        assert_eq!(table.cell_key(Some(k), 2), table.cell_key(Some(k), 4), "one dictionary code per string");
        assert_eq!(table.cell_key(Some(k), 1), (0, 0));
    }

    #[test]
    fn typed_columns_equal_the_same_values_as_maps() {
        let income = [Some(1.0), None, Some(3.0)];
        let route = [None, Some("12"), Some("12")];
        let flag = [Some(true), Some(false), None];
        let table = PropertyTable::from_columns(
            3,
            &[
                ("income", PropertyColumn::Number(&income)),
                ("route", PropertyColumn::Str(&route)),
                ("flag", PropertyColumn::Bool(&flag)),
            ],
        )
        .unwrap();
        assert_eq!(table.row_map(0), map(&[("income", TagValue::Number(1.0)), ("flag", TagValue::Bool(true))]));
        assert_eq!(table.row_map(1), map(&[("route", TagValue::String("12".into())), ("flag", TagValue::Bool(false))]));
        assert_eq!(table.row_map(2), map(&[("income", TagValue::Number(3.0)), ("route", TagValue::String("12".into()))]));
        assert!(PropertyTable::from_columns(2, &[("income", PropertyColumn::Number(&income))]).is_err());
    }

    #[test]
    fn a_row_past_the_end_is_empty() {
        let table = PropertyTable::from_maps(&[map(&[("a", TagValue::Number(1.0))])]);
        assert!(table.row_map(5).is_empty());
        assert!(PropertyTable::empty().row_map(0).is_empty());
    }
}

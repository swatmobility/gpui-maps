//! A database query as a document source (explorer-sql spec §6.1, plan
//! P3 3.1). The change is additive: `mapdoc` stays 1 and the keys live in
//! [`Source::extra`], so an older Studio round-trips them untouched and a
//! document without them is unchanged byte for byte.
//!
//! ```jsonc
//! "trips": {
//!   "kind": "clickhouse",
//!   "data": { "ref": "query" },
//!   "connection": "analytics-local",   // a NAME only: never a host, user or password
//!   "database": "flows",               // optional
//!   "sql": "SELECT …",                  // a string, or an array of lines
//!   "max_rows": 100000
//! }
//! ```
//!
//! [`seal`] is how a Studio recognises a document it saved itself (the
//! user's decision: documents from others wait for "Run queries"): a hash
//! of a key that never leaves the machine and the SQL sources as saved.

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::doc::{DataRef, Doc, Source};

/// `data.ref` of a query source.
pub const QUERY_REF: &str = "query";
/// The source kinds that are database queries.
pub const SQL_KINDS: &[&str] = &["clickhouse", "postgres"];
/// The document key the seal is saved under (`explorer.sql_seal`).
pub const SEAL_KEY: &str = "sql_seal";

/// A query source's settings, read from [`Source::extra`].
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct SqlSpec {
    /// `clickhouse` | `postgres`.
    pub kind: String,
    pub connection: String,
    pub database: Option<String>,
    /// The query, or in [`SqlMode::Live`] the tile template (its
    /// `{bbox_w}`, `{z}`, … placeholders are bound per tile).
    pub sql: String,
    pub max_rows: Option<u64>,
    /// Load rows once, or query tiles as the camera moves.
    pub mode: SqlMode,
    /// Live tiles: below this zoom the server aggregates.
    pub agg_below_z: Option<u8>,
    /// Live tiles: raw rows per tile; past it the tile aggregates.
    pub tile_cap: Option<u64>,
    /// Live tiles: the template's location columns.
    pub live: Option<LiveGeometry>,
}

/// How a query dataset loads (`"mode"`; absent is [`SqlMode::Rows`], so
/// an older document is unchanged).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SqlMode {
    /// Run once, up to the row cap ("Load rows").
    #[default]
    Rows,
    /// Query the tiles in view, again as the camera moves ("Live tiles").
    Live,
}

/// The live template's location columns (`"live_geometry"`).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct LiveGeometry {
    /// A feature's centre: what owns it to one tile and what aggregates.
    pub lon: String,
    pub lat: String,
    /// The row id: deterministic thinning and the hover fetch.
    pub id: Option<String>,
    /// A line or polygon column (WKB, WKT or a ClickHouse geo type); `None`
    /// for points.
    pub shape: Option<String>,
    /// The aggregate's cells: `grid` (pixel grid) or `h3`.
    pub agg: String,
    /// A numeric column the aggregate sums (`k_sum`).
    pub measure: Option<String>,
    /// H3 aggregates: the resolution by data zoom, `(from_zoom, res)`
    /// ascending (`"h3_levels": [[0, 3], [4, 5]]`); empty is about one
    /// cell per 16 px.
    pub h3_levels: Vec<(u8, u8)>,
    /// A column whose most common value each aggregate cell carries
    /// under the column's own name (`topK(1)`), so a colour by it reads
    /// the same on cells and on raw rows.
    pub dominant: Option<String>,
    /// H3 aggregates drawn as the cells' hexagons (`h3ToGeoBoundary`)
    /// rather than as points at the rows' mean centre.
    pub hex_cells: bool,
}

impl LiveGeometry {
    fn to_value(&self) -> Value {
        let mut m = serde_json::Map::new();
        m.insert("lon".into(), Value::String(self.lon.clone()));
        m.insert("lat".into(), Value::String(self.lat.clone()));
        for (k, v) in [
            ("id", &self.id),
            ("shape", &self.shape),
            ("measure", &self.measure),
        ] {
            if let Some(v) = v {
                m.insert(k.into(), Value::String(v.clone()));
            }
        }
        if !self.agg.is_empty() {
            m.insert("agg".into(), Value::String(self.agg.clone()));
        }
        if !self.h3_levels.is_empty() {
            m.insert(
                "h3_levels".into(),
                Value::Array(
                    self.h3_levels
                        .iter()
                        .map(|(z, r)| serde_json::json!([z, r]))
                        .collect(),
                ),
            );
        }
        if let Some(d) = &self.dominant {
            m.insert("dominant".into(), Value::String(d.clone()));
        }
        if self.hex_cells {
            m.insert("hex_cells".into(), Value::Bool(true));
        }
        Value::Object(m)
    }

    fn from_value(v: &Value) -> Option<LiveGeometry> {
        let s = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let h3_levels = v
            .get("h3_levels")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|p| {
                        Some((
                            u8::try_from(p.get(0)?.as_u64()?).ok()?,
                            u8::try_from(p.get(1)?.as_u64()?).ok()?.min(15),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some(LiveGeometry {
            lon: s("lon")?,
            lat: s("lat")?,
            id: s("id"),
            shape: s("shape"),
            agg: s("agg").unwrap_or_default(),
            measure: s("measure"),
            h3_levels,
            dominant: s("dominant"),
            hex_cells: v.get("hex_cells").and_then(Value::as_bool).unwrap_or(false),
        })
    }
}

impl Source {
    /// Whether this source is a database query.
    pub fn is_sql(&self) -> bool {
        SQL_KINDS.contains(&self.kind.as_str()) && self.data.kind == QUERY_REF
    }

    /// The query's settings, or `None` for a file source.
    pub fn sql_spec(&self) -> Option<SqlSpec> {
        if !self.is_sql() {
            return None;
        }
        let s = |k: &str| {
            self.extra
                .get(k)
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        let sql = match self.extra.get("sql") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(lines)) => lines
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        Some(SqlSpec {
            kind: self.kind.clone(),
            connection: s("connection").unwrap_or_default(),
            database: s("database").filter(|d| !d.is_empty()),
            sql,
            max_rows: self.extra.get("max_rows").and_then(Value::as_u64),
            mode: if self.extra.get("mode").and_then(Value::as_str) == Some("live") {
                SqlMode::Live
            } else {
                SqlMode::Rows
            },
            agg_below_z: self
                .extra
                .get("agg_below_z")
                .and_then(Value::as_u64)
                .and_then(|z| u8::try_from(z).ok()),
            tile_cap: self.extra.get("tile_cap").and_then(Value::as_u64),
            live: self
                .extra
                .get("live_geometry")
                .and_then(LiveGeometry::from_value),
        })
    }

    /// A new query source. Only a connection NAME is stored.
    pub fn new_sql(spec: &SqlSpec, label: &str) -> Source {
        let mut s = Source {
            kind: spec.kind.clone(),
            data: DataRef {
                kind: QUERY_REF.into(),
                ..DataRef::default()
            },
            label: label.to_string(),
            ..Source::default()
        };
        s.set_sql_spec(spec);
        s
    }

    /// Writes `spec`'s keys; an absent database or row cap removes its key.
    pub fn set_sql_spec(&mut self, spec: &SqlSpec) {
        self.kind = spec.kind.clone();
        self.extra
            .insert("connection".into(), Value::String(spec.connection.clone()));
        match &spec.database {
            Some(d) => self
                .extra
                .insert("database".into(), Value::String(d.clone())),
            None => self.extra.remove("database"),
        };
        // The text as the user wrote it: one string (an array of lines is
        // read, and kept until the text changes).
        let same = self.sql_spec().is_some_and(|old| old.sql == spec.sql);
        if !same {
            self.extra
                .insert("sql".into(), Value::String(spec.sql.clone()));
        }
        match spec.max_rows {
            Some(n) => self.extra.insert("max_rows".into(), Value::from(n)),
            None => self.extra.remove("max_rows"),
        };
        // Live keys only for a live source: a rows source stays as it was.
        match spec.mode {
            SqlMode::Live => self
                .extra
                .insert("mode".into(), Value::String("live".into())),
            SqlMode::Rows => self.extra.remove("mode"),
        };
        match spec.agg_below_z {
            Some(z) => self.extra.insert("agg_below_z".into(), Value::from(z)),
            None => self.extra.remove("agg_below_z"),
        };
        match spec.tile_cap {
            Some(n) => self.extra.insert("tile_cap".into(), Value::from(n)),
            None => self.extra.remove("tile_cap"),
        };
        match &spec.live {
            Some(g) => self.extra.insert("live_geometry".into(), g.to_value()),
            None => self.extra.remove("live_geometry"),
        };
    }
}

impl Default for Source {
    fn default() -> Self {
        Source {
            kind: String::new(),
            data: DataRef::default(),
            label: String::new(),
            columns: Default::default(),
            geometry: Value::Null,
            attribution: None,
            extra: Default::default(),
        }
    }
}

/// The seal of `doc`'s query sources under `key`: hex SHA-256 of the key
/// and every query source's id, kind, connection, database, SQL and cap,
/// in id order. Empty when the document has no query source.
pub fn seal(doc: &Doc, key: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"gpui-map explorer sql seal v1\0");
    h.update(key.as_bytes());
    let mut any = false;
    for (id, s) in &doc.sources {
        let Some(spec) = s.sql_spec() else { continue };
        any = true;
        for part in [
            id.as_str(),
            &spec.kind,
            &spec.connection,
            spec.database.as_deref().unwrap_or(""),
            &spec.sql,
            &spec.max_rows.map(|n| n.to_string()).unwrap_or_default(),
        ] {
            h.update((part.len() as u64).to_le_bytes());
            h.update(part.as_bytes());
        }
        // A live source's settings too (a rows source's seal is unchanged).
        if spec.mode == SqlMode::Live {
            let geometry = spec
                .live
                .as_ref()
                .map(|g| g.to_value().to_string())
                .unwrap_or_default();
            for part in [
                "live",
                &spec.agg_below_z.map(|n| n.to_string()).unwrap_or_default(),
                &spec.tile_cap.map(|n| n.to_string()).unwrap_or_default(),
                &geometry,
            ] {
                h.update((part.len() as u64).to_le_bytes());
                h.update(part.as_bytes());
            }
        }
    }
    if !any {
        return String::new();
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

impl Doc {
    /// Writes the seal for `key` under `explorer.sql_seal` (or removes it
    /// when there is no query source).
    pub fn set_sql_seal(&mut self, key: &str) {
        let s = seal(self, key);
        if !self.explorer.is_object() {
            if s.is_empty() {
                return;
            }
            self.explorer = Value::Object(Default::default());
        }
        if let Some(root) = self.explorer.as_object_mut() {
            if s.is_empty() {
                root.remove(SEAL_KEY);
            } else {
                root.insert(SEAL_KEY.into(), Value::String(s));
            }
        }
    }

    /// Whether this document's queries were saved by the holder of `key`
    /// and are unchanged since: then they run on open.
    pub fn sql_sealed_by(&self, key: &str) -> bool {
        let saved = self
            .explorer
            .get(SEAL_KEY)
            .and_then(Value::as_str)
            .unwrap_or("");
        !saved.is_empty() && saved == seal(self, key)
    }

    /// The ids of the query sources.
    pub fn sql_sources(&self) -> Vec<String> {
        self.sources
            .iter()
            .filter(|(_, s)| s.is_sql())
            .map(|(id, _)| id.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> SqlSpec {
        SqlSpec {
            kind: "clickhouse".into(),
            connection: "analytics-local".into(),
            database: Some("flows".into()),
            sql: "SELECT 1 AS n".into(),
            max_rows: Some(10_000),
            ..Default::default()
        }
    }

    #[test]
    fn a_sql_source_round_trips_byte_for_byte() {
        let text = r#"{"mapdoc":1,"profile":"explorer","title":"t","sources":{"trips":{"kind":"clickhouse","data":{"ref":"query"},"label":"Trips","connection":"analytics-local","database":"flows","max_rows":100000,"sql":["SELECT trip_id","FROM trips"],"variables":[{"key":"city"}]}}}"#;
        let doc = Doc::from_json(text).unwrap();
        let s = &doc.sources["trips"];
        let spec = s.sql_spec().unwrap();
        assert_eq!(spec.sql, "SELECT trip_id\nFROM trips");
        assert_eq!(spec.connection, "analytics-local");
        assert_eq!(spec.max_rows, Some(100_000));
        let again = Doc::from_json(&doc.to_json()).unwrap();
        assert_eq!(again, doc, "the raw keys, lines and variables survive");
        assert!(
            doc.to_json().contains(r#""sql": ["#) || doc.to_json().contains(r#""sql":["#),
            "lines stay lines"
        );
    }

    #[test]
    fn a_document_without_sql_keys_is_unchanged() {
        let text = r#"{"mapdoc":1,"profile":"explorer","title":"t","sources":{"c":{"kind":"csv","data":{"ref":"file","path":"c.csv"}}}}"#;
        let doc = Doc::from_json(text).unwrap();
        assert!(doc.sources["c"].sql_spec().is_none());
        assert!(doc.sql_sources().is_empty());
        let mut sealed = doc.clone();
        sealed.set_sql_seal("k");
        assert_eq!(sealed, doc, "no query source, no seal");
    }

    #[test]
    fn new_sql_stores_a_name_and_no_connection_detail() {
        let s = Source::new_sql(&spec(), "Trips");
        let json = serde_json::to_string(&s).unwrap();
        for absent in ["url", "host", "user", "password", "port"] {
            assert!(!json.contains(&format!("\"{absent}\"")), "{json}");
        }
        assert_eq!(s.sql_spec().unwrap(), spec());
    }

    #[test]
    fn the_seal_holds_for_its_key_and_breaks_on_an_edit() {
        let mut doc = Doc::default();
        doc.sources
            .insert("q".into(), Source::new_sql(&spec(), "Q"));
        doc.set_sql_seal("this-machine");
        assert!(doc.sql_sealed_by("this-machine"));
        assert!(
            !doc.sql_sealed_by("another-machine"),
            "someone else's document"
        );
        let mut edited = doc.clone();
        let mut s = spec();
        s.sql = "SELECT secret FROM t".into();
        edited.sources.get_mut("q").unwrap().set_sql_spec(&s);
        assert!(
            !edited.sql_sealed_by("this-machine"),
            "SQL changed outside the Studio"
        );
    }

    #[test]
    fn a_live_source_saves_its_h3_levels_dominant_column_and_hex_cells() {
        let g = LiveGeometry {
            lon: "lon".into(),
            lat: "lat".into(),
            agg: "h3".into(),
            h3_levels: vec![(0, 3), (4, 5), (7, 7)],
            dominant: Some("radio".into()),
            hex_cells: true,
            ..Default::default()
        };
        let live = SqlSpec {
            mode: SqlMode::Live,
            live: Some(g.clone()),
            ..spec()
        };
        let mut doc = Doc::default();
        doc.sources.insert("q".into(), Source::new_sql(&live, "Q"));
        let again = Doc::from_json(&doc.to_json()).unwrap();
        assert_eq!(again.sources["q"].sql_spec().unwrap().live, Some(g));
        // Without them, none of the three keys is written.
        let plain = SqlSpec {
            mode: SqlMode::Live,
            live: Some(LiveGeometry {
                lon: "lon".into(),
                lat: "lat".into(),
                ..Default::default()
            }),
            ..spec()
        };
        let text = serde_json::to_string(&Source::new_sql(&plain, "P")).unwrap();
        for k in ["h3_levels", "dominant", "hex_cells"] {
            assert!(!text.contains(k), "{k} in {text}");
        }
    }

    #[test]
    fn a_live_source_saves_its_mode_zoom_cap_and_geometry() {
        let live = SqlSpec {
            mode: SqlMode::Live,
            agg_below_z: Some(9),
            tile_cap: Some(5_000),
            live: Some(LiveGeometry {
                lon: "lon".into(),
                lat: "lat".into(),
                id: Some("id".into()),
                shape: None,
                agg: "grid".into(),
                measure: Some("fare".into()),
                ..Default::default()
            }),
            ..spec()
        };
        let mut doc = Doc::default();
        doc.sources.insert("q".into(), Source::new_sql(&live, "Q"));
        let again = Doc::from_json(&doc.to_json()).unwrap();
        assert_eq!(again.sources["q"].sql_spec().unwrap(), live);
        assert!(
            doc.to_json().contains("\"mode\": \"live\"")
                || doc.to_json().contains("\"mode\":\"live\"")
        );
        // A rows source writes none of the live keys.
        let rows = serde_json::to_string(&Source::new_sql(&spec(), "R")).unwrap();
        for k in ["mode", "agg_below_z", "tile_cap", "live_geometry"] {
            assert!(!rows.contains(k), "{k} in {rows}");
        }
        // The seal covers the live settings.
        doc.set_sql_seal("k");
        let mut edited = doc.clone();
        edited.sources.get_mut("q").unwrap().set_sql_spec(&SqlSpec {
            tile_cap: Some(50_000),
            ..live
        });
        assert!(doc.sql_sealed_by("k") && !edited.sql_sealed_by("k"));
    }
}

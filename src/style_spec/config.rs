//! Parses a style document's top-level `imports` array and resolves each
//! entry's effective config.
//!
//! A Mapbox Standard style's real basemap arrives through
//! `imports[0].data`, a complete nested style document whose expressions
//! reference `["config", k]` (796 times in the vendored
//! `examples/mapbox-standard-dark-style.json`). `k` is resolved from two
//! places: the imported document's own `schema` (a per-key `"default"`,
//! 45 keys in the fixture) and the consumer's `imports[0].config` block
//! (a subset override, 20 keys in the fixture). This module produces that
//! resolution; `expression::eval`'s `"config"` arm reads it back out
//! through `Scope`.
//!
//! This module only parses and resolves -- it does not compile
//! `data.layers` into `StyleLayer`s. `parse_and_compile` does that, which
//! is why `Import` exposes `data` untouched: it reads both `layers` and
//! `lights` straight back out of it.

use std::collections::HashMap;

use anyhow::{Context, Result};
use serde_json::Value;

/// One entry of a style document's top-level `imports` array.
///
/// `config` is already resolved (schema defaults overridden by this
/// entry's own `config` block) -- not the raw override object the JSON
/// carries -- because that resolution is exactly what every consumer of an
/// `Import` needs: attached to a `expression::Scope` via
/// `Scope::with_config`, it is what makes `["config", k]` inside
/// `data.layers` fold.
#[derive(Debug, Clone)]
pub struct Import {
    pub id: String,
    pub url: Option<String>,
    pub data: Value,
    pub config: ResolvedConfig,
    /// The `color-theme` specification in force for this fragment's own
    /// layers -- the import ENTRY's `color-theme` where it has one, else the
    /// imported document's own. That precedence is the reference's:
    /// `Style._createFragmentStyle` passes `importSpec["color-theme"]` as
    /// `colorThemeOverride`, and `Style._load` prefers the override over
    /// `stylesheet['color-theme']` (Mapbox GL JS 3.15.0).
    ///
    /// Still the raw `{ "data": <expression> }` object: its `data` is an
    /// ordinary style expression reaching for THIS import's `config`
    /// (Standard's picks one base64 PNG per `["config", "theme"]` branch),
    /// so it can only be folded in a scope carrying `config` above. See
    /// `style_spec::compile_color_theme`.
    pub color_theme: Option<Value>,
}

/// The resolved config for one import: every key its imported document's
/// `schema` declares a default for, overridden by the import entry's own
/// `config` block where present.
///
/// A key present in neither is unresolvable -- `resolve` returns `None`
/// rather than a placeholder, so `["config", k]` for such a key is the
/// folder refusal (`Unfoldable`) it needs to be, not a panic or a made-up
/// value.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedConfig(HashMap<String, Value>);

impl ResolvedConfig {
    /// The resolved value for `key`, as raw JSON -- `expression::eval`
    /// converts it with `EvalValue::from_json`, the same conversion a
    /// style literal goes through.
    pub fn resolve(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }

    /// An empty resolution: no schema, no overrides. Only useful in tests
    /// that want a `Scope` carrying *a* config (so `["config", k]` is a
    /// "key not found" refusal rather than "no config resolved at all") --
    /// real callers get their `ResolvedConfig` from `parse_imports`.
    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        ResolvedConfig(HashMap::new())
    }

    #[cfg(test)]
    pub(crate) fn from_pairs(pairs: impl IntoIterator<Item = (&'static str, Value)>) -> Self {
        ResolvedConfig(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }
}

/// Resolves one import entry's config: `schema`'s per-key `"default"`
/// values, overridden by `overrides`'s own keys. Either argument may be
/// any JSON value; only a JSON object contributes keys, so a missing or
/// malformed `schema`/`config` resolves to "nothing from that side"
/// rather than an error -- `parse_import` already requires `data` to be
/// present, and a schema-less or override-less import is still a
/// structurally valid one (just with fewer keys resolvable).
fn resolve_config(schema: &Value, overrides: &Value) -> ResolvedConfig {
    let mut resolved = HashMap::new();
    if let Some(schema_obj) = schema.as_object() {
        for (key, spec) in schema_obj {
            if let Some(default) = spec.get("default") {
                resolved.insert(key.clone(), default.clone());
            }
        }
    }
    if let Some(overrides_obj) = overrides.as_object() {
        for (key, value) in overrides_obj {
            resolved.insert(key.clone(), value.clone());
        }
    }
    ResolvedConfig(resolved)
}

/// Parses a style document's top-level `imports` array. A document with no
/// `imports` key -- every style this crate compiled before Mapbox
/// Standard, including both vendored `examples/liberty-style.json` and
/// `examples/mapbox-streets-style.json` -- resolves to an empty `Vec`
/// rather than an error, so this is safe to call unconditionally from
/// `parse_and_compile`.
pub fn parse_imports(root: &Value) -> Result<Vec<Import>> {
    let Some(imports_json) = root.get("imports") else {
        return Ok(Vec::new());
    };
    let imports_arr =
        imports_json.as_array().ok_or_else(|| anyhow::anyhow!("style document's \"imports\" must be an array"))?;
    imports_arr.iter().enumerate().map(|(i, entry)| parse_import(entry).with_context(|| format!("imports[{i}]"))).collect()
}

fn parse_import(entry: &Value) -> Result<Import> {
    let id = entry
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("import entry has no \"id\": {entry}"))?
        .to_string();
    let url = entry.get("url").and_then(Value::as_str).map(str::to_string);
    let data = entry.get("data").cloned().ok_or_else(|| anyhow::anyhow!("import {id:?} has no \"data\": {entry}"))?;
    let empty = Value::Object(serde_json::Map::new());
    let overrides = entry.get("config").unwrap_or(&empty);
    let schema = data.get("schema").unwrap_or(&empty);
    let config = resolve_config(schema, overrides);
    // The entry's override first, then the imported document's own.
    let color_theme = entry.get("color-theme").or_else(|| data.get("color-theme")).cloned();
    Ok(Import { id, url, data, config, color_theme })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema(pairs: &[(&str, Value)]) -> Value {
        Value::Object(pairs.iter().map(|(k, v)| (k.to_string(), serde_json::json!({ "default": v }))).collect())
    }

    #[test]
    fn a_document_with_no_imports_key_resolves_to_an_empty_vec() {
        let root = serde_json::json!({ "version": 8, "layers": [] });
        assert_eq!(parse_imports(&root).unwrap().len(), 0);
    }

    #[test]
    fn an_imports_value_that_is_not_an_array_is_an_error() {
        let root = serde_json::json!({ "imports": "not-an-array" });
        assert!(parse_imports(&root).is_err());
    }

    #[test]
    fn an_import_entry_reads_its_id_url_data_and_config() {
        let root = serde_json::json!({
            "imports": [{
                "id": "basemap",
                "url": "mapbox://styles/mapbox/standard",
                "data": { "schema": {}, "layers": [] },
                "config": { "lightPreset": "night" }
            }]
        });
        let imports = parse_imports(&root).unwrap();
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].id, "basemap");
        assert_eq!(imports[0].url.as_deref(), Some("mapbox://styles/mapbox/standard"));
        assert_eq!(imports[0].data["layers"], serde_json::json!([]));
        assert_eq!(imports[0].config.resolve("lightPreset"), Some(&serde_json::json!("night")));
    }

    #[test]
    fn an_import_entry_with_no_url_or_config_still_parses() {
        let root = serde_json::json!({
            "imports": [{ "id": "basemap", "data": { "layers": [] } }]
        });
        let imports = parse_imports(&root).unwrap();
        assert_eq!(imports[0].url, None);
        assert_eq!(imports[0].config.resolve("anything"), None);
    }

    #[test]
    fn an_import_entry_with_no_id_is_an_error() {
        let root = serde_json::json!({ "imports": [{ "data": { "layers": [] } }] });
        assert!(parse_imports(&root).is_err());
    }

    #[test]
    fn an_import_entry_with_no_data_is_an_error() {
        let root = serde_json::json!({ "imports": [{ "id": "basemap" }] });
        assert!(parse_imports(&root).is_err());
    }

    #[test]
    fn schema_default_is_used_when_the_import_config_does_not_override_it() {
        let s = schema(&[("colorLand", serde_json::json!("hsl(20, 20%, 95%)"))]);
        let resolved = resolve_config(&s, &serde_json::json!({}));
        assert_eq!(resolved.resolve("colorLand"), Some(&serde_json::json!("hsl(20, 20%, 95%)")));
    }

    #[test]
    fn the_import_config_overrides_the_schema_default_when_present() {
        let s = schema(&[("lightPreset", serde_json::json!("day"))]);
        let overrides = serde_json::json!({ "lightPreset": "night" });
        let resolved = resolve_config(&s, &overrides);
        assert_eq!(resolved.resolve("lightPreset"), Some(&serde_json::json!("night")));
    }

    #[test]
    fn a_key_in_neither_schema_nor_overrides_does_not_resolve() {
        let resolved = resolve_config(&serde_json::json!({}), &serde_json::json!({}));
        assert_eq!(resolved.resolve("unknownKey"), None);
    }

    /// The real fixture: 45-key schema, 20-key override.
    #[test]
    fn resolves_the_vendored_mapbox_standard_dark_fixture() {
        let bytes = mapbox_fixture_or_skip!("mapbox-standard-dark-style.json");
        let root: Value = serde_json::from_slice(&bytes).unwrap();
        let imports = parse_imports(&root).unwrap();
        assert_eq!(imports.len(), 1);
        let import = &imports[0];
        assert_eq!(import.id, "basemap");

        let schema_obj = import.data.get("schema").and_then(Value::as_object).expect("schema object");
        assert_eq!(schema_obj.len(), 45);
        let overrides_obj =
            root["imports"][0].get("config").and_then(Value::as_object).expect("config object");
        assert_eq!(overrides_obj.len(), 20);

        // Overridden in the fixture's config.
        assert_eq!(import.config.resolve("lightPreset"), Some(&serde_json::json!("night")));
        assert_eq!(import.config.resolve("theme"), Some(&serde_json::json!("monochrome")));
        // Not overridden -- falls back to the schema default.
        assert_eq!(import.config.resolve("colorLand"), Some(&serde_json::json!("hsl(20, 20%, 95%)")));
        // Neither schema nor override.
        assert_eq!(import.config.resolve("notAKey"), None);
    }
}

//! Switches Mapbox Streets' (and Mapbox Standard's) disputed-border
//! worldview by rewriting whichever worldview code currently sits inside
//! every `["match", ["get", "worldview"], [labels...], true, false]`
//! filter clause -- the shape every worldview-aware layer in the vendored
//! style writes (16 of them in `examples/mapbox-streets-style.json`; see
//! `streets_has_sixteen_worldview_layers` below, pinned against
//! `grep -c '"worldview"'` on that fixture). The vendored style ships
//! `"US"` there, but the rewrite is not keyed on that literal: it replaces
//! whichever non-`"all"` label is present, so a `Value` already switched
//! to one worldview can be switched again to a different one (`IN` then
//! `CN` lands on `CN`, not stuck on `IN`) -- see
//! `switching_from_a_non_us_worldview_replaces_it_too`.
//!
//! There is no `["config", "worldview"]` escape hatch to resolve instead --
//! see `mapbox_standard_worldview_is_not_exposed_as_a_config_key` -- so the
//! literal replace is the only lever a style document offers.

use serde_json::Value;

/// Replaces whichever worldview code currently sits inside every
/// `["match", ["get", "worldview"], [labels...], true, false]` label
/// array with `worldview` -- not only the vendored style's `"US"`, so
/// this is safe to call again on a `Value` a previous call already
/// switched to a different worldview. Returns the number of top-level
/// `layers` entries changed (0 if `worldview` was already current
/// everywhere, which is what makes calling it twice with the same value
/// idempotent). Touches nothing else: a style with no such clause is
/// returned byte-for-byte identical on re-serialisation, and a label
/// array's other entries (such as the `"all"` keyword Mapbox Streets
/// ships alongside the country code) are left alone.
pub fn set_worldview(style: &mut Value, worldview: &str) -> usize {
    let mut changed = 0;
    if let Some(layers) = style.get_mut("layers").and_then(Value::as_array_mut) {
        for layer in layers.iter_mut() {
            if replace_worldview_label(layer, worldview) {
                changed += 1;
            }
        }
    }
    changed
}

/// `true` if `v` is exactly `["match", ["get", "worldview"], [..], true,
/// false]` -- the shape `compile_top_level_match`/`compile_general_match`
/// in `style_spec::filter` reads a `match` filter clause as.
fn is_worldview_match(v: &Value) -> bool {
    let Some(arr) = v.as_array() else { return false };
    arr.len() == 5
        && arr[0] == Value::String("match".to_string())
        && arr[1] == serde_json::json!(["get", "worldview"])
        && arr[3] == Value::Bool(true)
        && arr[4] == Value::Bool(false)
}

/// Walks `v` looking for the worldview-match shape anywhere inside it
/// (filters nest it under `all`/`case`/another `match`'s fallback -- see
/// `waterway-label`'s class match in the vendored style), rewriting every
/// label array entry that is not `"all"` and not already `worldview` to
/// `worldview`. This targets whichever country code is currently there --
/// not literally `"US"` -- so a second call with a different `worldview`
/// on an already-rewritten `Value` still takes effect. Returns whether
/// anything changed, so the caller can count layers rather than clauses.
fn replace_worldview_label(v: &mut Value, worldview: &str) -> bool {
    let mut changed = false;
    if is_worldview_match(v) {
        if let Some(labels) = v.get_mut(2).and_then(Value::as_array_mut) {
            for label in labels.iter_mut() {
                if let Some(s) = label.as_str() {
                    if s != "all" && s != worldview {
                        *label = Value::String(worldview.to_string());
                        changed = true;
                    }
                }
            }
        }
    }
    match v {
        Value::Array(items) => {
            for item in items.iter_mut() {
                changed |= replace_worldview_label(item, worldview);
            }
        }
        Value::Object(map) => {
            for item in map.values_mut() {
                changed |= replace_worldview_label(item, worldview);
            }
        }
        _ => {}
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mvt::TagValue;
    use crate::style_spec::expression::Scope;
    use crate::style_spec::filter::compile_filter;
    use std::collections::HashMap;

    #[test]
    fn streets_has_sixteen_worldview_layers() {
        let mut s: Value = serde_json::from_str(&mapbox_fixture_str_or_skip!("mapbox-streets-style.json")).unwrap();
        assert_eq!(set_worldview(&mut s, "IN"), 16);
    }

    /// `admin-0-boundary-disputed`'s filter, compiled with the
    /// `style_spec` filter compiler after `set_worldview(IN)`: a feature
    /// tagged `worldview: "IN"` still passes, `worldview: "US"` no longer
    /// does, and `worldview: "all"` (Streets' own "shown everywhere"
    /// keyword, untouched by the rewrite) still passes.
    #[test]
    fn the_disputed_boundary_keeps_in_and_drops_us_only() {
        let mut s: Value = serde_json::from_str(&mapbox_fixture_str_or_skip!("mapbox-streets-style.json")).unwrap();
        assert_eq!(set_worldview(&mut s, "IN"), 16);
        let layer = s["layers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["id"] == "admin-0-boundary-disputed")
            .expect("admin-0-boundary-disputed is in the vendored style");
        let filter_json = layer["filter"].clone();
        let filter = compile_filter(&filter_json, Scope::root()).expect("the rewritten filter still compiles");

        let tags = |worldview: &str| -> HashMap<String, TagValue> {
            [
                ("disputed".to_string(), TagValue::String("true".to_string())),
                ("admin_level".to_string(), TagValue::Number(0.0)),
                ("maritime".to_string(), TagValue::String("false".to_string())),
                ("worldview".to_string(), TagValue::String(worldview.to_string())),
            ]
            .into_iter()
            .collect()
        };

        assert!(filter.matches(&tags("IN"), 5.0), "IN must pass after set_worldview(IN)");
        assert!(!filter.matches(&tags("US"), 5.0), "US must be dropped after set_worldview(IN)");
        assert!(filter.matches(&tags("all"), 5.0), "the \"all\" keyword must still pass, untouched by the rewrite");
    }

    /// A style with no worldview-match clause at all (`demos/positron-style.json`,
    /// a MapLibre-only vector style) is untouched: `set_worldview` returns 0,
    /// and re-serialising before and after the call produces identical JSON.
    #[test]
    fn a_style_without_worldview_is_untouched() {
        let raw = include_str!("../../demos/positron-style.json");
        assert_eq!(raw.matches("worldview").count(), 0, "positron-style.json is expected to carry no worldview clause");
        let mut s: Value = serde_json::from_str(raw).unwrap();
        let before = serde_json::to_string(&s).unwrap();
        assert_eq!(set_worldview(&mut s, "IN"), 0);
        let after = serde_json::to_string(&s).unwrap();
        assert_eq!(before, after, "set_worldview must not mutate a style with no worldview clause");
    }

    /// Setting the same worldview twice: the second call finds no more
    /// literal `"US"` labels to replace, so it changes nothing.
    #[test]
    fn setting_it_twice_is_idempotent() {
        let mut s: Value = serde_json::from_str(&mapbox_fixture_str_or_skip!("mapbox-streets-style.json")).unwrap();
        assert_eq!(set_worldview(&mut s, "IN"), 16);
        assert_eq!(set_worldview(&mut s, "IN"), 0, "a second set_worldview(IN) must be a no-op");
    }

    /// Fix round 1, HIGH: `set_worldview` must switch from whatever
    /// worldview is currently in the label array, not only from the
    /// vendored style's own `"US"`. On 0270a33, `replace_us_label` only
    /// ever matched the literal string `"US"`, so `set_worldview(&mut s,
    /// "IN")` followed by `set_worldview(&mut s, "CN")` on the SAME
    /// `Value` found no more `"US"` labels, returned 0 and silently left
    /// the document on `"IN"` -- this test's middle block (asserting a
    /// `CN`-tagged feature matches and an `IN`-tagged one no longer does,
    /// after the second call) fails against that code.
    #[test]
    fn switching_from_a_non_us_worldview_replaces_it_too() {
        let mut s: Value = serde_json::from_str(&mapbox_fixture_str_or_skip!("mapbox-streets-style.json")).unwrap();
        let disputed_filter = |s: &Value| -> Value {
            s["layers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|l| l["id"] == "admin-0-boundary-disputed")
                .expect("admin-0-boundary-disputed is in the vendored style")["filter"]
                .clone()
        };
        let tags = |worldview: &str| -> HashMap<String, TagValue> {
            [
                ("disputed".to_string(), TagValue::String("true".to_string())),
                ("admin_level".to_string(), TagValue::Number(0.0)),
                ("maritime".to_string(), TagValue::String("false".to_string())),
                ("worldview".to_string(), TagValue::String(worldview.to_string())),
            ]
            .into_iter()
            .collect()
        };

        assert_eq!(set_worldview(&mut s, "IN"), 16);
        let filter = compile_filter(&disputed_filter(&s), Scope::root()).unwrap();
        assert!(filter.matches(&tags("IN"), 5.0), "IN must pass after set_worldview(IN)");
        assert!(!filter.matches(&tags("US"), 5.0));
        assert!(!filter.matches(&tags("CN"), 5.0));
        assert!(filter.matches(&tags("all"), 5.0));

        assert_eq!(set_worldview(&mut s, "CN"), 16, "switching an already-IN document to CN must still touch all 16 layers");
        let filter = compile_filter(&disputed_filter(&s), Scope::root()).unwrap();
        assert!(filter.matches(&tags("CN"), 5.0), "CN must pass after set_worldview(CN)");
        assert!(!filter.matches(&tags("IN"), 5.0), "IN must no longer pass -- the document switched to CN");
        assert!(!filter.matches(&tags("US"), 5.0));
        assert!(filter.matches(&tags("all"), 5.0));

        assert_eq!(set_worldview(&mut s, "US"), 16, "switching an already-CN document to US must still touch all 16 layers");
        let filter = compile_filter(&disputed_filter(&s), Scope::root()).unwrap();
        assert!(filter.matches(&tags("US"), 5.0), "US must pass after set_worldview(US)");
        assert!(!filter.matches(&tags("CN"), 5.0), "CN must no longer pass -- the document switched to US");
        assert!(!filter.matches(&tags("IN"), 5.0));
        assert!(filter.matches(&tags("all"), 5.0));
    }

    /// Step 5's Mapbox Standard check: does `examples/mapbox-standard-dark-style.json`
    /// (or its `imports[0].config`/schema) expose a `worldview` config key
    /// that `style_spec::config::ResolvedConfig` resolves? It does not --
    /// `worldview` is absent from both the schema defaults and the
    /// consumer's override block, and the disputed-boundary split inside
    /// `imports[0].data.layers` writes the exact same literal-`"US"`
    /// `match` shape as Streets (just wrapped in a `case` guarding
    /// `$localized`). So `set_worldview`'s literal replace is the only
    /// lever available for Standard too, once it is pointed at
    /// `imports[0].data` -- this task does not wire that in.
    #[test]
    fn mapbox_standard_worldview_is_not_exposed_as_a_config_key() {
        let raw = &mapbox_fixture_str_or_skip!("mapbox-standard-dark-style.json");
        let root: Value = serde_json::from_str(raw).unwrap();
        let imports = crate::style_spec::config::parse_imports(&root).expect("imports parse");
        let basemap = imports.iter().find(|i| i.id == "basemap").expect("a \"basemap\" import");
        assert!(
            basemap.config.resolve("worldview").is_none(),
            "worldview resolved via config in this fixture -- Standard support should read it instead of literal-replacing \"US\""
        );
        assert!(
            raw.contains(r#"["match",["get","worldview"],["all","US"],true,false]"#),
            "expected the same literal-US match shape as Streets inside imports[0].data.layers"
        );
    }
}

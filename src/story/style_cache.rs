//! One compiled [`Style`] per style generation of a [`Timeline`], kept in
//! a least-recently-used cache of four (spec §4.1), so scrubbing back and
//! forth across a style edit never recompiles in a loop; and the semantic
//! style checks of spec §2.2 stage 2.
//!
//! Native only: compiling a generation is `style_spec::parse_and_compile`,
//! which the player needs and the shells' pure story half does not.
//!
//! Like the other story modules, this one denies `unwrap`, `expect` and
//! indexing outside tests (spec §2, "No-panic contract").
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)
)]

use std::sync::Arc;

use super::script::{Diagnostic, Severity};
use super::timeline::Timeline;
use crate::style::Style;
use crate::style_spec::{this_thread_compile_diagnostics, DiagnosticKind, StyleDiagnostic};

/// How many compiled generations the cache keeps.
pub const STYLE_CACHE_CAPACITY: usize = 4;

/// Compiled styles by generation, least recently used first.
#[derive(Default)]
pub struct StyleCache {
    entries: Vec<(u64, Arc<Style>)>,
    compiles: u64,
}

impl StyleCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Generation `generation` of `timeline`, compiled once and then
    /// served from the cache until four newer generations push it out.
    /// `Err` for a generation the timeline does not have, or a document
    /// that does not compile.
    ///
    /// The cache is keyed by generation alone: use one cache per timeline.
    pub fn get(&mut self, timeline: &Timeline, generation: u64) -> anyhow::Result<Arc<Style>> {
        if let Some(i) = self.entries.iter().position(|(g, _)| *g == generation) {
            let entry = self.entries.remove(i);
            let style = Arc::clone(&entry.1);
            self.entries.push(entry);
            return Ok(style);
        }
        let style = Arc::new(compile(timeline, generation)?);
        self.compiles += 1;
        self.entries.push((generation, Arc::clone(&style)));
        if self.entries.len() > STYLE_CACHE_CAPACITY {
            self.entries.remove(0);
        }
        Ok(style)
    }

    /// How many times [`get`](Self::get) has compiled a document.
    pub fn compiles(&self) -> u64 {
        self.compiles
    }
}

/// Compiles generation `generation` of `timeline`, uncached: what a live
/// preview runs for every generation on a worker thread before it plays.
///
/// # Errors
/// A generation the timeline does not have, or a document that does not
/// compile.
pub fn compile(timeline: &Timeline, generation: u64) -> anyhow::Result<Style> {
    let doc = timeline
        .style_document(generation)
        .ok_or_else(|| anyhow::anyhow!("style generation {generation} does not exist (the timeline has {})", timeline.generations()))?;
    let bytes = serde_json::to_vec(doc)?;
    crate::style_spec::parse_and_compile(&bytes)
}

/// Semantic style checks (spec §2.2 stage 2, the style half): every edited
/// layer exists when its event fires, and every generation compiles.
///
/// The first half holds by construction: [`Timeline::new`] refuses an edit
/// to a layer its generation lacks (`unknown_layer`), so a `Timeline`
/// exists only when every edit found its layer. This checks the second,
/// both ways a document can fail to compile:
///
/// - The whole document is refused (`parse_and_compile` returns `Err`):
///   `style_compile_failed`, at the event that made the generation
///   (`/events/<i>`), or at `/scene/style` for the scene's own document.
/// - The compiler drops a layer it cannot compile -- a malformed filter or
///   paint value skips the one layer with a stderr notice rather than
///   failing the style. A layer an EDIT made uncompilable, skipped in
///   generation `n` but not in `n - 1`, is `style_edit_not_compiled` at
///   `/events/<i>`. Layers the scene's own style already skips are not the
///   story's doing and are not reported.
pub fn validate_style_edits(timeline: &Timeline) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    let mut previous_skipped: Option<Vec<StyleDiagnostic>> = None;
    for generation in 0..timeline.generations() {
        let path = timeline.generation_source(generation).map_or_else(|| "/scene/style".to_string(), |i| format!("/events/{i}"));
        let result = compile(timeline, generation);
        let skipped: Vec<StyleDiagnostic> =
            this_thread_compile_diagnostics().into_iter().filter(|d| d.kind == DiagnosticKind::SkippedLayer).collect();
        match result {
            Err(e) => diags.push(Diagnostic {
                severity: Severity::Error,
                path,
                code: "style_compile_failed",
                message: format!("style generation {generation} does not compile: {e:#}"),
                hint: None,
            }),
            Ok(_) => {
                // R2: `previous_skipped` is the last generation that
                // COMPILED. A failed compile leaves a cleared or partial
                // list, and comparing against that would report every
                // layer this generation skips, the scene style's own
                // skips included.
                if let Some(before) = &previous_skipped {
                    for d in skipped.iter().filter(|d| !before.contains(d)) {
                        diags.push(Diagnostic {
                            severity: Severity::Error,
                            path: path.clone(),
                            code: "style_edit_not_compiled",
                            message: format!(
                                "after this edit the layer `{}` no longer compiles and would not draw: {}",
                                d.layer.as_deref().unwrap_or("?"),
                                d.detail
                            ),
                            hint: Some("check the filter or property value this event sets".to_string()),
                        });
                    }
                }
                previous_skipped = Some(skipped);
            }
        }
    }
    diags
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::story::camera_path::CameraPath;
    use crate::story::test_util::test_script;
    use crate::story::tracks::TrackSet;

    fn timeline(base: serde_json::Value, events: serde_json::Value) -> Timeline {
        let mut script = test_script(serde_json::json!([{"type":"jump","to":{"zoom":2}},{"type":"hold","duration":10}]));
        script.events = serde_json::from_value(events).unwrap();
        let path = CameraPath::new((800.0, 600.0), &script, &TrackSet::default()).unwrap();
        Timeline::new(&script, &path, base, &|_| None, &[]).unwrap()
    }

    fn base() -> serde_json::Value {
        serde_json::json!({"version":8,"sources":{},"layers":[{"id":"bg","type":"background","paint":{"background-color":"#fff"}}]})
    }

    #[test]
    fn the_cache_keeps_four_and_evicts_the_least_recently_used() {
        let events: Vec<serde_json::Value> = (1..=5)
            .map(|k| serde_json::json!({"at": k, "set_paint_property": {"layer": "bg", "name": "background-color", "value": format!("#00000{k}")}}))
            .collect();
        let tl = timeline(base(), serde_json::Value::Array(events));
        let mut cache = StyleCache::new();
        for g in 0..4 {
            cache.get(&tl, g).unwrap();
        }
        cache.get(&tl, 0).unwrap(); // 0 is now the most recent; 1 the least.
        cache.get(&tl, 4).unwrap(); // evicts 1
        assert_eq!(cache.compiles(), 5);
        cache.get(&tl, 0).unwrap();
        cache.get(&tl, 2).unwrap();
        assert_eq!(cache.compiles(), 5, "0 and 2 are still cached");
        cache.get(&tl, 1).unwrap();
        assert_eq!(cache.compiles(), 6, "1 was evicted");
        assert!(cache.get(&tl, 9).is_err(), "a generation past the last is an error, not a panic");
    }

    #[test]
    fn a_whole_document_that_does_not_compile_is_a_diagnostic_at_the_scene_style() {
        let tl = timeline(base(), serde_json::json!([{"at":2,"set_layout_property":{"layer":"bg","name":"visibility","value":"none"}}]));
        assert!(validate_style_edits(&tl).is_empty(), "hiding a layer is not a compile failure");
        let broken = timeline(serde_json::json!({"version":8,"layers":"nope"}), serde_json::json!([]));
        let d = validate_style_edits(&broken);
        assert_eq!(d.len(), 1);
        assert_eq!((d[0].code, d[0].path.as_str()), ("style_compile_failed", "/scene/style"));
    }

    /// F8: a valid scene style and one edit that breaks a layer: the
    /// diagnostic is at that event, and only there.
    #[test]
    fn an_edit_that_breaks_a_layer_is_a_diagnostic_at_that_event() {
        let style = serde_json::json!({"version":8,"sources":{"s":{"type":"vector","tiles":["http://x/{z}/{x}/{y}.pbf"]}},"layers":[
            {"id":"bg","type":"background","paint":{"background-color":"#fff"}},
            {"id":"water","type":"fill","source":"s","source-layer":"water","paint":{"fill-color":"#00f"}}
        ]});
        for (k, edit) in [
            serde_json::json!({"set_filter":{"layer":"water","filter":["no-such-operator", 1]}}),
            serde_json::json!({"set_paint_property":{"layer":"water","name":"fill-color","value":["no-such-expression"]}}),
        ]
        .into_iter()
        .enumerate()
        {
            let mut event = edit;
            event["at"] = serde_json::json!(1);
            let events = serde_json::json!([{"at":0.5,"set_paint_property":{"layer":"bg","name":"background-color","value":"#eee"}}, event]);
            let tl = timeline(style.clone(), events);
            let d = validate_style_edits(&tl);
            let got: Vec<(&str, &str)> = d.iter().map(|d| (d.code, d.path.as_str())).collect();
            assert_eq!(got, vec![("style_edit_not_compiled", "/events/1")], "case {k}: {d:?}");
        }
    }

    /// R2: a generation after one that failed to compile is compared with
    /// the last generation that compiled, not with the failed one's
    /// cleared list. The scene style already skips `water`; a broken
    /// `set_style` and then a `set_style` back to the same document must
    /// report only the broken one.
    #[test]
    fn a_generation_after_a_failed_compile_is_compared_with_the_last_that_compiled() {
        let skipping = serde_json::json!({"version":8,"sources":{"s":{"type":"vector","tiles":["http://x/{z}/{x}/{y}.pbf"]}},"layers":[
            {"id":"bg","type":"background","paint":{"background-color":"#fff"}},
            {"id":"water","type":"fill","source":"s","source-layer":"water","filter":["no-such-operator", 1],"paint":{"fill-color":"#00f"}}
        ]});
        let mut script = test_script(serde_json::json!([{"type":"jump","to":{"zoom":2}},{"type":"hold","duration":10}]));
        script.events = serde_json::from_value(serde_json::json!([
            {"at":1,"set_style":{"style":"broken.json"}},
            {"at":2,"set_style":{"style":"skipping.json"}}
        ]))
        .unwrap();
        let path = CameraPath::new((800.0, 600.0), &script, &TrackSet::default()).unwrap();
        let s2 = skipping.clone();
        let load = move |name: &str| match name {
            "broken.json" => Some(serde_json::json!({"version":8,"layers":"nope"})),
            "skipping.json" => Some(s2.clone()),
            _ => None,
        };
        let tl = Timeline::new(&script, &path, skipping, &load, &[]).unwrap();
        assert_eq!(tl.generations(), 3);
        let d = validate_style_edits(&tl);
        let got: Vec<(&str, &str)> = d.iter().map(|d| (d.code, d.path.as_str())).collect();
        assert_eq!(got, vec![("style_compile_failed", "/events/0")], "{d:?}");
    }
}

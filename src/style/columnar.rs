//! Columnar style filtering: a `Filter` evaluated over a whole tile layer
//! at once, into a bitset over its features.
//!
//! # Why
//!
//! Filtering used to be row-at-a-time: for every style layer, walk every
//! feature of its source layer and run `Filter::matches_geometry`, which
//! hashes the key string of every predicate against the feature's
//! `HashMap<String, TagValue>`. Measured on `benches/fixtures/
//! singapore_z14.pbf` with Liberty (1,950 features, 108 style layers):
//! 20,977 feature visits per bake (a 10.8x re-walk, since the outer loop is
//! the style layers), 47,064 string-keyed tag probes and 86,607 filter-tree
//! nodes -- 92% of `match_tile_static`. The `transportation` layer alone
//! has 61 style layers over only 22 distinct leaf predicates.
//!
//! # How
//!
//! A decoded MVT layer keeps its wire dictionary (`mvt::TagDict`), and each
//! feature's tags are `(key id, value id)` pairs into it. [`LayerEval`] is
//! built once per (tile layer, style, zoom) and shared by every style layer
//! on that source layer:
//!
//! 1. A filter's key is resolved to a key id ONCE, and the per-feature
//!    value ids for that key are gathered into a column ONCE.
//! 2. A leaf predicate (`==`, `in`, `has`, a numeric comparison) is decided
//!    once per distinct VALUE ID it meets (memoised over the dictionary),
//!    not once per feature, and its result is a bitset over the features.
//!    Equal leaves across style layers are evaluated once per tile layer.
//! 3. `all`/`any`/`!` are word-wise AND/OR/NOT on bitsets; `case`/`match`/
//!    zoom `step` compose the same way (see [`LayerEval::eval`]).
//!
//! A leaf's truth for one value is `Filter::leaf_matches_value` -- the SAME
//! function `Filter::matches_geometry` calls for that leaf -- so the two
//! paths cannot disagree on what a predicate means; `columnar_parity` tests
//! assert the matched sets are identical on every vendored style and
//! fixture.
//!
//! # The bitset
//!
//! A plain `Vec<u64>`, not Roaring. A z14 tile layer holds at most tens of
//! thousands of features (25,264 in the whole dense Tokyo tile), so a
//! dense bitset is at most a few KB, and filter results are dense-ish
//! (`class == minor` selects a large share of `transportation`). Roaring
//! pays container dispatch and allocation to save memory on sparse sets of
//! millions of ids -- a regime a tile layer never reaches -- while a flat
//! word array makes AND/OR/NOT straight loops the compiler vectorises.
//!
//! # The fallback
//!
//! A layer whose features do NOT all share one dictionary (hand-built
//! tiles in tests, or merged layers) is evaluated through the same bitset
//! composition, with each leaf read per feature through `Tags::get`.
//! Nothing in `Filter` needs per-feature expression evaluation beyond its
//! leaves, so there is no other fallback path.

use std::collections::HashMap;

use crate::mvt::{Feature, Layer, TagDict, TagValue};

use super::{Filter, GeometryKind};

const NONE: u32 = u32::MAX;

/// A set of feature indices of one tile layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Bits {
    words: Vec<u64>,
    len: usize,
}

impl Bits {
    fn zeros(len: usize) -> Self {
        Self { words: vec![0; len.div_ceil(64)], len }
    }

    fn ones(len: usize) -> Self {
        let mut bits = Self { words: vec![u64::MAX; len.div_ceil(64)], len };
        bits.trim();
        bits
    }

    /// Clears the bits past `len` in the last word, so NOT stays in range.
    fn trim(&mut self) {
        let rem = self.len % 64;
        if rem != 0 {
            if let Some(last) = self.words.last_mut() {
                *last &= (1u64 << rem) - 1;
            }
        }
    }

    fn set(&mut self, i: usize) {
        self.words[i / 64] |= 1u64 << (i % 64);
    }

    #[cfg(test)]
    fn contains(&self, i: usize) -> bool {
        self.words[i / 64] & (1u64 << (i % 64)) != 0
    }

    fn and_with(&mut self, other: &Self) {
        for (a, b) in self.words.iter_mut().zip(&other.words) {
            *a &= b;
        }
    }

    fn or_with(&mut self, other: &Self) {
        for (a, b) in self.words.iter_mut().zip(&other.words) {
            *a |= b;
        }
    }

    fn and_not_with(&mut self, other: &Self) {
        for (a, b) in self.words.iter_mut().zip(&other.words) {
            *a &= !b;
        }
    }

    fn not(mut self) -> Self {
        for w in &mut self.words {
            *w = !*w;
        }
        self.trim();
        self
    }

    fn is_empty(&self) -> bool {
        self.words.iter().all(|w| *w == 0)
    }

    /// Set indices, ascending -- the features' original order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.words.iter().enumerate().flat_map(|(wi, &word)| {
            let mut w = word;
            std::iter::from_fn(move || {
                if w == 0 {
                    return None;
                }
                let bit = w.trailing_zeros() as usize;
                w &= w - 1;
                Some(wi * 64 + bit)
            })
        })
    }
}

/// Whether two leaves are the same predicate, so one bitset serves both.
fn same_leaf(a: &Filter, b: &Filter) -> bool {
    match (a, b) {
        (Filter::Equals { key: ka, value: va }, Filter::Equals { key: kb, value: vb }) => ka == kb && va == vb,
        (Filter::In { key: ka, values: va }, Filter::In { key: kb, values: vb }) => ka == kb && va == vb,
        (Filter::NumberCompare { key: ka, op: oa, value: va }, Filter::NumberCompare { key: kb, op: ob, value: vb }) => {
            ka == kb && oa == ob && va.to_bits() == vb.to_bits()
        }
        (Filter::Has(ka), Filter::Has(kb)) => ka == kb,
        (Filter::BoolEquals { key: ka, value: va }, Filter::BoolEquals { key: kb, value: vb }) => ka == kb && va == vb,
        (Filter::GeometryType(a), Filter::GeometryType(b)) => a == b,
        _ => false,
    }
}

/// One tile layer, ready to evaluate filters against. See the module doc.
pub(crate) struct LayerEval<'a> {
    features: &'a [Feature],
    /// The dictionary every feature shares, or `None` (per-feature reads).
    dict: Option<&'a TagDict>,
    zoom: f64,
    /// Per key id: the value id each feature carries for it, or `NONE`.
    columns: HashMap<u32, Vec<u32>>,
    kinds: Option<Vec<Option<GeometryKind>>>,
    /// Leaf bitsets already computed, by predicate.
    leaves: Vec<(&'a Filter, Bits)>,
}

impl<'a> LayerEval<'a> {
    pub(crate) fn new(layer: &'a Layer, zoom: f64) -> Self {
        let features = layer.features.as_slice();
        let dict = features.first().map(|f| f.tags.dict()).filter(|first| {
            features.iter().all(|f| std::sync::Arc::ptr_eq(f.tags.dict(), first))
        });
        Self { features, dict: dict.map(|d| &**d), zoom, columns: HashMap::new(), kinds: None, leaves: Vec::new() }
    }

    fn len(&self) -> usize {
        self.features.len()
    }

    /// The features `filter` matches, as `Filter::matches_geometry(tags,
    /// zoom, GeometryKind::of(geometry))` would decide them one by one.
    pub(crate) fn eval(&mut self, filter: &'a Filter) -> Bits {
        let n = self.len();
        match filter {
            Filter::Any => Bits::ones(n),
            Filter::Never => Bits::zeros(n),
            Filter::All(filters) => {
                let mut acc = Bits::ones(n);
                for f in filters {
                    if acc.is_empty() {
                        break;
                    }
                    let bits = self.eval(f);
                    acc.and_with(&bits);
                }
                acc
            }
            Filter::AnyOf(filters) => {
                let mut acc = Bits::zeros(n);
                for f in filters {
                    let bits = self.eval(f);
                    acc.or_with(&bits);
                }
                acc
            }
            Filter::Not(inner) => self.eval(inner).not(),
            Filter::ZoomStep { base, stops } => {
                let zoom = self.zoom;
                let branch = stops.iter().rev().find(|(threshold, _)| zoom >= *threshold).map_or(&**base, |(_, f)| f);
                self.eval(branch)
            }
            Filter::Case { arms, fallback } => {
                let mut remaining = Bits::ones(n);
                let mut out = Bits::zeros(n);
                for (cond, then) in arms {
                    if remaining.is_empty() {
                        break;
                    }
                    let mut hit = self.eval(cond);
                    hit.and_with(&remaining);
                    remaining.and_not_with(&hit);
                    if !hit.is_empty() {
                        hit.and_with(&self.eval(then));
                        out.or_with(&hit);
                    }
                }
                if !remaining.is_empty() {
                    remaining.and_with(&self.eval(fallback));
                    out.or_with(&remaining);
                }
                out
            }
            Filter::MultiMatch { key, cases, fallback } => {
                let pick = |value: Option<&TagValue>| -> usize {
                    let s = match value {
                        Some(TagValue::String(s)) => s.as_str(),
                        _ => return cases.len(),
                    };
                    cases.iter().position(|(labels, _)| labels.iter().any(|l| l == s)).unwrap_or(cases.len())
                };
                let groups = self.partition(key, cases.len() + 1, pick);
                self.compose_groups(groups, cases.iter().map(|(_, f)| f), fallback)
            }
            Filter::NumberMultiMatch { key, cases, fallback } => {
                let pick = |value: Option<&TagValue>| -> usize {
                    let v = match value {
                        Some(TagValue::Number(v)) => *v,
                        _ => return cases.len(),
                    };
                    cases.iter().position(|(labels, _)| labels.contains(&v)).unwrap_or(cases.len())
                };
                let groups = self.partition(key, cases.len() + 1, pick);
                self.compose_groups(groups, cases.iter().map(|(_, f)| f), fallback)
            }
            Filter::GeometryType(kinds) => {
                if let Some(bits) = self.cached(filter) {
                    return bits;
                }
                let features = self.features;
                let geometry_kinds =
                    self.kinds.get_or_insert_with(|| features.iter().map(|f| GeometryKind::of(&f.geometry)).collect());
                let mut bits = Bits::zeros(n);
                for (i, kind) in geometry_kinds.iter().enumerate() {
                    // `matches_geometry`'s rule: an unknown class is not a non-match.
                    if kind.is_none_or(|k| kinds.contains(&k)) {
                        bits.set(i);
                    }
                }
                self.leaves.push((filter, bits.clone()));
                bits
            }
            Filter::Equals { key, .. }
            | Filter::BoolEquals { key, .. }
            | Filter::In { key, .. }
            | Filter::NumberCompare { key, .. }
            | Filter::Has(key)
            | Filter::NumberCompareWithZoomOffset { key, .. } => {
                if let Some(bits) = self.cached(filter) {
                    return bits;
                }
                let zoom = self.zoom;
                let groups = self.partition(key, 2, |value| usize::from(filter.leaf_matches_value(value, zoom)));
                let bits = groups.into_iter().nth(1).expect("two groups");
                if !matches!(filter, Filter::NumberCompareWithZoomOffset { .. }) {
                    self.leaves.push((filter, bits.clone()));
                }
                bits
            }
        }
    }

    fn cached(&self, filter: &Filter) -> Option<Bits> {
        self.leaves.iter().find(|(f, _)| same_leaf(f, filter)).map(|(_, bits)| bits.clone())
    }

    /// `out_i` for the features in group `i`, `fallback` for the last group.
    fn compose_groups(
        &mut self,
        groups: Vec<Bits>,
        outputs: impl Iterator<Item = &'a Filter>,
        fallback: &'a Filter,
    ) -> Bits {
        let mut out = Bits::zeros(self.len());
        let mut groups = groups.into_iter();
        for filter in outputs {
            let mut group = groups.next().expect("one group per case");
            if !group.is_empty() {
                group.and_with(&self.eval(filter));
                out.or_with(&group);
            }
        }
        let mut rest = groups.next().expect("a fallback group");
        if !rest.is_empty() {
            rest.and_with(&self.eval(fallback));
            out.or_with(&rest);
        }
        out
    }

    /// Splits the features into `count` groups by `pick` of their value for
    /// `key` (`None` when they carry none). `pick` runs once per distinct
    /// value id over a shared dictionary, once per feature otherwise.
    fn partition(&mut self, key: &str, count: usize, pick: impl Fn(Option<&TagValue>) -> usize) -> Vec<Bits> {
        let n = self.len();
        let mut groups = vec![Bits::zeros(n); count];
        let Some(dict) = self.dict else {
            for (i, feature) in self.features.iter().enumerate() {
                groups[pick(feature.tags.get(key))].set(i);
            }
            return groups;
        };
        let Some(key_id) = dict.key_id(key) else {
            // No feature in this layer can carry the key.
            groups[pick(None)] = Bits::ones(n);
            return groups;
        };
        let features = self.features;
        let column = self.columns.entry(key_id).or_insert_with(|| {
            features
                .iter()
                .map(|f| f.tags.pairs().iter().find(|(k, _)| *k == key_id).map_or(NONE, |&(_, v)| v))
                .collect()
        });
        const UNKNOWN: u16 = u16::MAX;
        let mut memo = vec![UNKNOWN; dict.values.len()];
        let absent = pick(None);
        for (i, &value_id) in column.iter().enumerate() {
            let group = if value_id == NONE {
                absent
            } else {
                let slot = &mut memo[value_id as usize];
                if *slot == UNKNOWN {
                    *slot = pick(Some(&dict.values[value_id as usize])) as u16;
                }
                *slot as usize
            };
            groups[group].set(i);
        }
        groups
    }
}

/// Every tile layer of one tile, each evaluated lazily at one zoom: what a
/// whole-style pass (`match_tile_static`, `resolve_plan`,
/// `match_tile_symbols`) shares across its style layers, so a source layer
/// used by 61 style layers resolves its keys and leaves once.
pub(crate) struct TileEval<'a> {
    tile: &'a crate::mvt::DecodedTile,
    zoom: f64,
    layers: HashMap<&'a str, Option<LayerEval<'a>>>,
}

impl<'a> TileEval<'a> {
    pub(crate) fn new(tile: &'a crate::mvt::DecodedTile, zoom: f64) -> Self {
        Self { tile, zoom, layers: HashMap::new() }
    }

    /// The tile layer a style layer's `source_layer` names -- the FIRST one
    /// with that name, as every row-at-a-time caller picked -- and the
    /// features of it `filter` matches. `None` when the tile has no such
    /// layer.
    pub(crate) fn matching(&mut self, source_layer: &str, filter: &'a Filter) -> Option<(&'a Layer, Bits)> {
        let tile = self.tile;
        let zoom = self.zoom;
        let layer = tile.layers.iter().find(|l| l.name == source_layer)?;
        let eval = self
            .layers
            .entry(layer.name.as_str())
            .or_insert_with(|| Some(LayerEval::new(layer, zoom)))
            .as_mut()?;
        Some((layer, eval.eval(filter)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mvt::{DecodedTile, Tags};
    use crate::style::StyleLayer;

    /// Every filter of a style layer, with the source layer it names.
    fn filters(style: &crate::style::Style) -> Vec<(&str, &str, &Filter)> {
        style
            .layers
            .iter()
            .filter_map(|layer| match layer {
                StyleLayer::Fill { id, source_layer, filter, .. }
                | StyleLayer::Line { id, source_layer, filter, .. }
                | StyleLayer::Symbol { id, source_layer, filter, .. }
                // Liberty's `building-3d` carries the tree's one real
                // `BoolEquals` (final-review M2).
                | StyleLayer::Extrusion { id, source_layer, filter, .. } => {
                    Some((id.as_str(), source_layer.as_str(), filter))
                }
                _ => None,
            })
            .collect()
    }

    fn row_at_a_time(layer: &Layer, filter: &Filter, zoom: f64) -> Vec<usize> {
        layer
            .features
            .iter()
            .enumerate()
            .filter(|(_, f)| filter.matches_geometry(&f.tags, zoom, GeometryKind::of(&f.geometry)))
            .map(|(i, _)| i)
            .collect()
    }

    /// The same layer with every feature on its OWN dictionary, which
    /// forces `LayerEval`'s per-feature fallback.
    fn unshared(layer: &Layer) -> Layer {
        Layer {
            name: layer.name.clone(),
            features: layer
                .features
                .iter()
                .map(|f| crate::mvt::Feature { geometry: f.geometry.clone(), tags: Tags::from(f.tags.to_map()) })
                .collect(),
        }
    }

    fn tiles() -> Vec<(String, DecodedTile)> {
        let mut paths = vec![concat!(env!("CARGO_MANIFEST_DIR"), "/benches/fixtures/singapore_z14.pbf").to_string()];
        // Extra real tiles (e.g. a dense z14) for a local run:
        // GPUI_MAP_PARITY_TILES=/a.pbf:/b.pbf cargo test columnar_parity
        if let Ok(extra) = std::env::var("GPUI_MAP_PARITY_TILES") {
            paths.extend(extra.split(':').filter(|p| !p.is_empty()).map(str::to_string));
        }
        paths
            .into_iter()
            .map(|p| {
                let bytes = std::fs::read(&p).unwrap_or_else(|e| panic!("read {p}: {e}"));
                (p, crate::mvt::decode(bytes).expect("fixture decodes"))
            })
            .collect()
    }

    fn styles() -> Vec<(String, crate::style::Style)> {
        let root = env!("CARGO_MANIFEST_DIR");
        let mut out = Vec::new();
        // `examples/styles-open/` too: the open library cut drops the
        // Mapbox styles from `examples/`, and these three keep it at 8.
        for dir in ["examples", "examples/styles-open", "demos"] {
            let mut entries: Vec<_> = std::fs::read_dir(format!("{root}/{dir}"))
                .expect("style dir")
                .map(|e| e.expect("entry").path())
                .filter(|p| p.extension().is_some_and(|e| e == "json"))
                .collect();
            entries.sort();
            for path in entries {
                let bytes = std::fs::read(&path).expect("style reads");
                // Only style documents; a JSON that is not one is skipped.
                if let Ok(style) = crate::style_spec::parse_and_compile(&bytes) {
                    out.push((path.display().to_string(), style));
                }
            }
        }
        assert!(out.len() >= 8, "expected the vendored styles, found {}", out.len());
        out
    }

    /// THE parity gate: for every style layer of every vendored style, on
    /// every fixture tile, the columnar matched set equals the
    /// row-at-a-time one -- at every zoom where a filter can change, on
    /// both the shared-dictionary path and the per-feature fallback.
    /// Filters are also run against EVERY tile layer, not only the one
    /// they name, so the Mapbox-schema styles meet real dictionaries too.
    #[test]
    fn columnar_parity_with_row_at_a_time_on_every_vendored_style() {
        let tiles = tiles();
        let styles = styles();
        assert!(
            styles.iter().any(|(_, style)| filters(style).iter().any(|(id, _, _)| *id == "building-3d")),
            "the sweep covers Liberty's building-3d, the tree's one real BoolEquals"
        );
        let mut checked = 0usize;
        let mut nonempty = 0usize;
        for (tile_name, tile) in &tiles {
            let fallbacks: Vec<Layer> = tile.layers.iter().map(unshared).collect();
            for (style_name, style) in &styles {
                for (id, source_layer, filter) in filters(style) {
                    for (layer_index, layer) in tile.layers.iter().enumerate() {
                        let own = layer.name == source_layer;
                        let zooms: &[f64] = if own && filter.uses_zoom() {
                            &[0.0, 3.0, 6.0, 9.0, 11.0, 12.0, 13.0, 14.0, 15.0, 16.0, 17.0, 18.0, 20.0, 22.0]
                        } else if own {
                            &[0.0, 14.0]
                        } else {
                            &[14.0]
                        };
                        for &zoom in zooms {
                            let expected = row_at_a_time(layer, filter, zoom);
                            let columnar: Vec<usize> = LayerEval::new(layer, zoom).eval(filter).iter().collect();
                            assert_eq!(
                                columnar, expected,
                                "{style_name} layer {id:?} on {tile_name} tile layer {:?} at z{zoom}",
                                layer.name
                            );
                            let fallback: Vec<usize> =
                                LayerEval::new(&fallbacks[layer_index], zoom).eval(filter).iter().collect();
                            assert_eq!(fallback, expected, "fallback: {style_name} layer {id:?} at z{zoom}");
                            checked += 1;
                            nonempty += usize::from(!expected.is_empty() && expected.len() < layer.features.len());
                        }
                    }
                }
            }
        }
        // A gate that compared only empty or full sets would prove little.
        // The monorepo checks well over 10,000. The open library cut drops
        // the Mapbox styles from `examples/` (scripts/split-manifest.txt),
        // so it checks ~7,500 (~590 partial); only there is the floor lower.
        let library_cut = !styles.iter().any(|(path, _)| {
            std::path::Path::new(path).file_name().is_some_and(|f| f.to_string_lossy().starts_with("mapbox-"))
        });
        let floor = if library_cut { 5_000 } else { 10_000 };
        assert!(
            checked > floor && nonempty > 300,
            "checked {checked} (floor {floor}, library cut: {library_cut}), of which {nonempty} partial"
        );
    }

    /// One `TileEval` shared across a style's layers (the leaf and column
    /// caches carry over between them) gives each layer the same set as a
    /// fresh evaluator would.
    #[test]
    fn a_shared_tile_eval_matches_a_fresh_one_per_layer() {
        for (_, tile) in tiles() {
            for (_, style) in styles() {
                let mut shared = TileEval::new(&tile, 14.0);
                for (_, source_layer, filter) in filters(&style) {
                    let got = shared.matching(source_layer, filter).map(|(_, b)| b.iter().collect::<Vec<_>>());
                    let want = tile
                        .layers
                        .iter()
                        .find(|l| l.name == source_layer)
                        .map(|l| row_at_a_time(l, filter, 14.0));
                    assert_eq!(got, want);
                }
            }
        }
    }

    /// `Filter::BoolEquals` on the shared-dictionary path. Five features:
    /// `Bool(true)`, `Bool(false)`, `String("true")`, `Number(1)`, absent.
    /// `== true` / `!= true` equal the row-at-a-time sets, and
    /// `AnyOf[== true, == false]` in ONE eval selects exactly the two
    /// booleans -- so the leaf cache (`same_leaf`) never hands the `true`
    /// leaf's bitset to the `false` leaf.
    #[test]
    fn bool_leaves_match_row_at_a_time_and_true_and_false_are_not_conflated_in_the_cache() {
        let dict = std::sync::Arc::new(crate::mvt::TagDict {
            keys: vec!["k".to_string()],
            values: vec![
                TagValue::Bool(true),
                TagValue::Bool(false),
                TagValue::String("true".to_string()),
                TagValue::Number(1.0),
            ],
        });
        let point = geo_types::Geometry::Point(geo_types::Point::new(0, 0));
        let feature = |pairs: &[(u32, u32)]| crate::mvt::Feature {
            geometry: point.clone(),
            tags: Tags::from_pairs(dict.clone(), pairs.to_vec().into_boxed_slice()),
        };
        let layer = Layer {
            name: "l".to_string(),
            features: vec![feature(&[(0, 0)]), feature(&[(0, 1)]), feature(&[(0, 2)]), feature(&[(0, 3)]), feature(&[])],
        };
        let compile = |json: serde_json::Value| {
            crate::style_spec::filter::compile_filter(&json, crate::style_spec::expression::Scope::root()).unwrap()
        };
        let eq_true = compile(serde_json::json!(["==", ["get", "k"], true]));
        let ne_true = compile(serde_json::json!(["!=", ["get", "k"], true]));
        let either = compile(serde_json::json!(["any", ["==", ["get", "k"], true], ["==", ["get", "k"], false]]));
        assert!(matches!(&either, Filter::AnyOf(arms) if arms.len() == 2), "{either:?}");
        for filter in [&eq_true, &ne_true, &either] {
            let mut eval = LayerEval::new(&layer, 14.0);
            assert!(eval.dict.is_some(), "the shared-dictionary path");
            assert_eq!(eval.eval(filter).iter().collect::<Vec<_>>(), row_at_a_time(&layer, filter, 14.0), "{filter:?}");
        }
        assert_eq!(row_at_a_time(&layer, &eq_true, 14.0), vec![0]);
        assert_eq!(row_at_a_time(&layer, &ne_true, 14.0), vec![1, 2, 3, 4]);
        // One eval, both leaves: the second must not come from the first's
        // cache entry.
        let mut eval = LayerEval::new(&layer, 14.0);
        assert_eq!(eval.eval(&either).iter().collect::<Vec<_>>(), vec![0, 1], "exactly the two booleans");
    }

    #[test]
    fn bits_not_stays_in_range_and_iterates_in_order() {
        for n in [0, 1, 63, 64, 65, 130] {
            let ones = Bits::ones(n);
            assert_eq!(ones.iter().count(), n);
            assert!(Bits::zeros(n).not().iter().eq(0..n));
            assert!(ones.not().is_empty());
        }
        let mut b = Bits::zeros(200);
        for i in [3, 64, 65, 199] {
            b.set(i);
        }
        assert_eq!(b.iter().collect::<Vec<_>>(), vec![3, 64, 65, 199]);
        assert!(b.contains(64) && !b.contains(66));
    }
}

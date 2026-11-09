//! Edits are actions (spec §6.5). Every UI edit is a [`DocAction`];
//! [`Doc::apply`] changes the document and returns the [`Effect`]s the
//! host's binding must carry out on its map, cheapest first. The mapping
//! from action to effect is pinned by a table test, so "an opacity drag
//! never re-pushes the data" is checked without a window.

use serde::{Deserialize, Serialize};

use crate::doc::{ChannelBinding, Chart, Doc, Filter, Hover, Layer, Source, View};
use crate::filters::FilterValue;
use crate::table::FieldType;

/// A partial change to a layer's [`crate::doc::Vis`]: `None` keeps the
/// field. On the wire an absent key keeps the field; `"strokeColor": null`
/// returns the outline to its automatic colour.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct VisPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub radius: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outline: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thickness: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<[u8; 3]>,
    /// `Some(None)` returns the outline to its automatic colour.
    #[serde(
        default,
        rename = "strokeColor",
        skip_serializing_if = "Option::is_none",
        with = "wire::double_option"
    )]
    pub stroke_color: Option<Option<[u8; 3]>>,
}

impl VisPatch {
    pub fn opacity(v: f64) -> Self {
        Self {
            opacity: Some(v),
            ..Self::default()
        }
    }
    pub fn radius(v: f64) -> Self {
        Self {
            radius: Some(v),
            ..Self::default()
        }
    }
    pub fn color(c: [u8; 3]) -> Self {
        Self {
            color: Some(c),
            ..Self::default()
        }
    }
    pub fn thickness(v: f64) -> Self {
        Self {
            thickness: Some(v),
            ..Self::default()
        }
    }
    pub fn filled(v: bool) -> Self {
        Self {
            filled: Some(v),
            ..Self::default()
        }
    }
    pub fn outline(v: bool) -> Self {
        Self {
            outline: Some(v),
            ..Self::default()
        }
    }
    pub fn stroke_color(c: Option<[u8; 3]>) -> Self {
        Self {
            stroke_color: Some(c),
            ..Self::default()
        }
    }
}

/// An edit. The wire form (logs, sync, a server applying edits) is an
/// object tagged by `op`, the variant's snake_case name, with the
/// variant's fields beside it: `{"op": "set_vis", "layer": "l1", "patch":
/// {"opacity": 0.5}}`. A variant wrapping a struct (`add_layer`,
/// `set_view`, `add_filter`, `replace_layer`, `add_chart`) carries that
/// struct's own keys beside `op`. The form is pinned by
/// `tests/fixtures/actions.json`: renaming a variant or a field is a
/// breaking change to every stored log.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum DocAction {
    AddSource {
        id: String,
        source: Source,
    },
    AddLayer(Layer),
    RemoveLayer {
        layer: String,
    },
    SetLayerVisible {
        layer: String,
        visible: bool,
    },
    SetVis {
        layer: String,
        patch: VisPatch,
    },
    SetLabel {
        layer: String,
        label: String,
    },
    /// Moves a layer to index `to` in draw order (0 draws first).
    MoveLayer {
        layer: String,
        to: usize,
    },
    SetView(View),
    /// Binds a channel to a field (spec §2.4, Kepler's `colorField` +
    /// `colorScale` + `colorRange`, or `sizeField` + `sizeScale` +
    /// `radiusRange`).
    SetChannel {
        layer: String,
        channel: Channel,
        binding: ChannelBinding,
    },
    /// Back to the fixed colour or size.
    ClearChannel {
        layer: String,
        channel: Channel,
    },
    /// A new filter (spec §2.5); its placement is decided here.
    AddFilter(Filter),
    RemoveFilter {
        id: String,
    },
    /// A filter's value. `live` while a control is held: a GPU filter
    /// follows it, a CPU filter waits for the release (`live: false`).
    SetFilterValue {
        id: String,
        value: FilterValue,
        live: bool,
    },
    SetFilterEnabled {
        id: String,
        enabled: bool,
    },
    /// A time filter's playback speed: the card's state, nothing drawn.
    SetFilterSpeed {
        id: String,
        speed: f64,
    },
    /// A layer's hover card: its fields, their order and formats (spec
    /// §2.6, Kepler's `interactionConfig.tooltip`).
    SetHover {
        layer: String,
        hover: Hover,
    },
    /// Overrides a column's detected type (`explorer.field_types`), or
    /// returns it to the detected type with `None` (plan Task 4.4). The
    /// layers reading it keep only the scales the new type offers: a
    /// number read as a string is coloured by an ordinal scale.
    SetFieldType {
        source: String,
        field: String,
        #[serde(with = "wire::field_type")]
        ty: Option<FieldType>,
    },
    /// Replaces a layer with `layer` (same id): a type switch, new
    /// columns, or a binning setting. Its map objects go and the new
    /// layer is drawn in its place, at the same position in the list.
    ReplaceLayer(Layer),
    /// A floating chart card (agent chat spec §5.3.1); a chart with the
    /// same id is replaced in place.
    AddChart(Chart),
    RemoveChart {
        id: String,
    },
    /// A query source's settings (explorer-sql §6.2: `SetSourceSql`,
    /// `SetSourceConnection` and `SetSourceMaxRows` as one action). A new
    /// connection, database or row cap runs the query again; new SQL text
    /// alone waits for Run (editing never runs by itself).
    SetSourceSql {
        source: String,
        #[serde(with = "wire::sql_spec")]
        spec: crate::sql::SqlSpec,
    },
}

/// A layer's data-driven channels (`"color"`, `"size"`, `"height"` on the
/// wire).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    /// Fill (or circle) colour.
    Color,
    /// Point radius.
    Size,
    /// A 3D GeoJSON layer's height.
    Height,
}

/// What the binding must do on the map.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// Push (or re-push) the layer's data and paint.
    Push(String),
    /// Re-declare the layer's paint; no data moves.
    Repaint(String),
    /// Set the layer's opacity; no data moves, nothing re-bakes.
    Opacity(String),
    /// Show or hide the layer.
    Visibility(String),
    Remove(String),
    Reorder,
    /// Move the camera to the document's view.
    Camera,
    /// Frame the layer's data (a newly added layer).
    Fit(String),
    /// Declare the dataset's GPU filter fields (in slot order) on every
    /// point source drawing it, then write every window: a view rebuild
    /// from the index, never a re-push.
    GpuFields(String),
    /// Move one GPU filter's window: a uniform write.
    GpuWindow(String),
    /// Recompute the dataset's CPU mask and re-push the passing rows.
    CpuFilter(String),
    /// Rebuild the layer's hover card; the card on screen follows.
    Popup(String),
    /// Re-type the dataset's columns from `explorer.field_types` (off the
    /// UI thread), then push its layers again.
    Retype(String),
    /// A chart card appears, changes or goes; the map is untouched.
    Chart(String),
    /// Run query source `source` again and land its rows on its layers
    /// (explorer-sql §6.2). The host compiles, guards and runs it.
    Run(String),
    /// Nothing on the map changes (a label, a source with no layer yet).
    None,
}

impl Doc {
    /// Applies `action`, returning what the map must do. An action naming
    /// a layer that does not exist changes nothing and returns
    /// `[Effect::None]`.
    pub fn apply(&mut self, action: DocAction) -> Vec<Effect> {
        match action {
            DocAction::AddSource { id, source } => {
                self.sources.insert(id, source);
                vec![Effect::None]
            }
            DocAction::AddLayer(layer) => {
                let id = layer.id.clone();
                let before = self.placements();
                match self.map.layers.iter().position(|l| l.id == id) {
                    Some(i) => self.map.layers[i] = layer,
                    None => self.map.layers.push(layer),
                }
                let mut out = vec![Effect::Push(id.clone()), Effect::Fit(id)];
                out.extend(self.replace_filters(&before));
                out
            }
            DocAction::RemoveLayer { layer } => {
                match self.map.layers.iter().position(|l| l.id == layer) {
                    Some(i) => {
                        let before = self.placements();
                        self.map.layers.remove(i);
                        let mut out = vec![Effect::Remove(layer)];
                        out.extend(self.replace_filters(&before));
                        out
                    }
                    None => vec![Effect::None],
                }
            }
            DocAction::SetLayerVisible { layer, visible } => match self.layer_mut(&layer) {
                Some(l) if l.visible != visible => {
                    l.visible = visible;
                    vec![Effect::Visibility(layer)]
                }
                _ => vec![Effect::None],
            },
            DocAction::SetVis { layer, patch } => {
                let Some(l) = self.layer_mut(&layer) else {
                    return vec![Effect::None];
                };
                let v = &mut l.vis;
                let mut repaint = false;
                let mut opacity = false;
                if let Some(r) = patch.radius.filter(|r| *r != v.radius) {
                    v.radius = r.clamp(0.0, 100.0);
                    repaint = true;
                }
                if let Some(t) = patch.thickness.filter(|t| *t != v.thickness) {
                    v.thickness = t.clamp(0.0, 10.0);
                    repaint = true;
                }
                if let Some(c) = patch.color.filter(|c| *c != v.color) {
                    v.color = c;
                    repaint = true;
                }
                if let Some(f) = patch.filled.filter(|f| *f != v.filled) {
                    v.filled = f;
                    repaint = true;
                }
                if let Some(c) = patch.stroke_color.filter(|c| *c != v.stroke_color) {
                    v.stroke_color = c;
                    repaint = true;
                }
                if let Some(o) = patch.outline.filter(|o| *o != v.outline) {
                    v.outline = o;
                    repaint = true;
                }
                if let Some(o) = patch.opacity.filter(|o| *o != v.opacity) {
                    v.opacity = o.clamp(0.0, 1.0);
                    opacity = true;
                }
                let mut out = Vec::new();
                if repaint {
                    out.push(Effect::Repaint(layer.clone()));
                }
                if opacity {
                    out.push(Effect::Opacity(layer));
                }
                if out.is_empty() {
                    out.push(Effect::None);
                }
                out
            }
            DocAction::SetLabel { layer, label } => {
                if let Some(l) = self.layer_mut(&layer) {
                    l.label = label;
                }
                vec![Effect::None]
            }
            DocAction::MoveLayer { layer, to } => {
                match self.map.layers.iter().position(|l| l.id == layer) {
                    Some(from) => {
                        let to = to.min(self.map.layers.len() - 1);
                        if from == to {
                            return vec![Effect::None];
                        }
                        let l = self.map.layers.remove(from);
                        self.map.layers.insert(to, l);
                        vec![Effect::Reorder]
                    }
                    None => vec![Effect::None],
                }
            }
            DocAction::SetView(view) => {
                self.map.view = view;
                vec![Effect::Camera]
            }
            DocAction::SetChannel {
                layer,
                channel,
                binding,
            } => {
                let Some(l) = self.layer_mut(&layer) else {
                    return vec![Effect::None];
                };
                let slot = channel_slot(l, channel);
                if slot.as_ref() == Some(&binding) {
                    return vec![Effect::None];
                }
                *slot = Some(binding);
                vec![Effect::Repaint(layer)]
            }
            DocAction::ClearChannel { layer, channel } => {
                let Some(l) = self.layer_mut(&layer) else {
                    return vec![Effect::None];
                };
                match channel_slot(l, channel).take() {
                    Some(_) => vec![Effect::Repaint(layer)],
                    None => vec![Effect::None],
                }
            }
            DocAction::AddFilter(filter) => {
                if self.filters.iter().any(|f| f.id == filter.id) {
                    return vec![Effect::None];
                }
                let (id, source) = (filter.id.clone(), filter.source.clone());
                self.filters.push(filter);
                self.place_filters();
                match self.filter(&id).map(|f| f.placement.is_gpu()) {
                    Some(true) => vec![Effect::GpuFields(source)],
                    _ => vec![Effect::CpuFilter(source)],
                }
            }
            DocAction::RemoveFilter { id } => {
                let Some(at) = self.filters.iter().position(|f| f.id == id) else {
                    return vec![Effect::None];
                };
                let gone = self.filters.remove(at);
                let before: Vec<(String, bool)> = self
                    .filters
                    .iter()
                    .map(|f| (f.id.clone(), f.placement.is_gpu()))
                    .collect();
                self.place_filters();
                // A CPU filter promoted into the freed slot leaves the CPU set.
                let promoted = self
                    .filters
                    .iter()
                    .zip(&before)
                    .any(|(f, (_, was))| f.placement.is_gpu() && !*was);
                match (gone.placement.is_gpu(), promoted) {
                    (true, true) => vec![
                        Effect::GpuFields(gone.source.clone()),
                        Effect::CpuFilter(gone.source),
                    ],
                    (true, false) => vec![Effect::GpuFields(gone.source)],
                    (false, _) => vec![Effect::CpuFilter(gone.source)],
                }
            }
            DocAction::SetFilterValue { id, value, live } => {
                let Some(f) = self.filters.iter_mut().find(|f| f.id == id) else {
                    return vec![Effect::None];
                };
                let changed = f.value != value;
                f.value = value;
                match (f.placement.is_gpu(), changed, live) {
                    (true, true, _) => vec![Effect::GpuWindow(id)],
                    (true, false, _) | (false, _, true) => vec![Effect::None],
                    (false, _, false) => vec![Effect::CpuFilter(f.source.clone())],
                }
            }
            DocAction::SetFilterEnabled { id, enabled } => {
                let Some(f) = self.filters.iter_mut().find(|f| f.id == id) else {
                    return vec![Effect::None];
                };
                if f.enabled == enabled {
                    return vec![Effect::None];
                }
                f.enabled = enabled;
                if f.placement.is_gpu() {
                    vec![Effect::GpuWindow(id)]
                } else {
                    vec![Effect::CpuFilter(f.source.clone())]
                }
            }
            DocAction::SetFilterSpeed { id, speed } => {
                if let Some(f) = self.filters.iter_mut().find(|f| f.id == id) {
                    f.speed = Some(speed);
                }
                vec![Effect::None]
            }
            DocAction::SetHover { layer, hover } => match self.layer_mut(&layer) {
                Some(l) if l.hover != hover => {
                    l.hover = hover;
                    vec![Effect::Popup(layer)]
                }
                _ => vec![Effect::None],
            },
            DocAction::ReplaceLayer(layer) => {
                let id = layer.id.clone();
                match self.map.layers.iter().position(|l| l.id == id) {
                    Some(i) if self.map.layers[i] == layer => vec![Effect::None],
                    Some(i) => {
                        let before = self.placements();
                        self.map.layers[i] = layer;
                        let mut out = vec![Effect::Remove(id.clone()), Effect::Push(id)];
                        // A type switch can take a dataset's range filters
                        // off (or onto) the GPU: a bin reads every row. A
                        // dataset change moves both datasets' filters.
                        out.extend(self.replace_filters(&before));
                        out
                    }
                    None => vec![Effect::None],
                }
            }
            DocAction::AddChart(chart) => {
                let id = chart.id.clone();
                match self.charts.iter().position(|c| c.id == id) {
                    Some(i) if self.charts[i] == chart => return vec![Effect::None],
                    Some(i) => self.charts[i] = chart,
                    None => self.charts.push(chart),
                }
                vec![Effect::Chart(id)]
            }
            DocAction::RemoveChart { id } => match self.charts.iter().position(|c| c.id == id) {
                Some(i) => {
                    self.charts.remove(i);
                    vec![Effect::Chart(id)]
                }
                None => vec![Effect::None],
            },
            DocAction::SetSourceSql { source, spec } => {
                let Some(s) = self.sources.get_mut(&source) else {
                    return vec![Effect::None];
                };
                let Some(old) = s.sql_spec() else {
                    return vec![Effect::None];
                };
                if old == spec {
                    return vec![Effect::None];
                }
                s.set_sql_spec(&spec);
                if (
                    old.connection,
                    old.database,
                    old.max_rows,
                    old.mode,
                    old.agg_below_z,
                    old.tile_cap,
                    old.live,
                ) != (
                    spec.connection,
                    spec.database,
                    spec.max_rows,
                    spec.mode,
                    spec.agg_below_z,
                    spec.tile_cap,
                    spec.live,
                ) {
                    vec![Effect::Run(source)]
                } else {
                    vec![Effect::None]
                }
            }
            DocAction::SetFieldType { source, field, ty } => {
                if !self.sources.contains_key(&source)
                    || self.field_type_override(&source, &field) == ty
                {
                    return vec![Effect::None];
                }
                self.set_field_type_override(&source, &field, ty);
                let Some(effective) = self.field_type(&source, &field) else {
                    return vec![Effect::Retype(source)];
                };
                for l in self.map.layers.iter_mut().filter(|l| l.source == source) {
                    if let Some(b) = l.color.as_mut().filter(|b| b.field == field) {
                        let offered = crate::scales::color_scales(effective);
                        if offered.is_empty() {
                            l.color = None;
                        } else if !offered.contains(&b.scale.as_str()) {
                            b.scale = offered[0].to_string();
                            let (palette, steps) = if b.scale == "ordinal" {
                                (
                                    crate::scales::DEFAULT_QUALITATIVE,
                                    crate::scales::DEFAULT_QUALITATIVE_STEPS,
                                )
                            } else {
                                (
                                    crate::scales::DEFAULT_SEQUENTIAL,
                                    crate::scales::DEFAULT_SEQUENTIAL_STEPS,
                                )
                            };
                            b.palette = Some(palette.to_string());
                            b.steps = Some(steps);
                            b.reversed = false;
                            b.extra.remove("colors");
                        }
                    }
                    if l.radius.as_ref().is_some_and(|b| {
                        b.field == field
                            && !crate::scales::size_scales(effective).contains(&b.scale.as_str())
                    }) {
                        l.radius = None;
                    }
                    if l.height.as_ref().is_some_and(|b| {
                        b.field == field
                            && !crate::scales::size_scales(effective).contains(&b.scale.as_str())
                    }) {
                        l.height = None;
                    }
                }
                vec![Effect::Retype(source)]
            }
        }
    }

    /// Whether each filter runs on the GPU, in order.
    fn placements(&self) -> Vec<bool> {
        self.filters.iter().map(|f| f.placement.is_gpu()).collect()
    }

    /// Places the filters again after the layers changed (a dataset's
    /// filters run on the GPU only while point layers alone draw it), and
    /// returns the effects for every dataset whose filters moved: its GPU
    /// fields declared again, then its CPU mask recomputed.
    fn replace_filters(&mut self, before: &[bool]) -> Vec<Effect> {
        self.place_filters();
        let mut moved: Vec<String> = Vec::new();
        for (f, was) in self.filters.iter().zip(before) {
            if f.placement.is_gpu() != *was && !moved.contains(&f.source) {
                moved.push(f.source.clone());
            }
        }
        moved
            .into_iter()
            .flat_map(|s| [Effect::GpuFields(s.clone()), Effect::CpuFilter(s)])
            .collect()
    }

    /// A layer id from `base` that no layer of the document holds: `base`,
    /// else `base-2`, `base-3`, ...
    pub fn unique_layer_id(&self, base: &str) -> String {
        if self.layer(base).is_none() {
            return base.to_string();
        }
        (2..)
            .map(|n| format!("{base}-{n}"))
            .find(|id| self.layer(id).is_none())
            .expect("an unused id")
    }

    /// `explorer.field_types[source][field]`, the user's override.
    pub fn field_type_override(&self, source: &str, field: &str) -> Option<FieldType> {
        self.explorer
            .get("field_types")?
            .get(source)?
            .get(field)?
            .as_str()
            .and_then(FieldType::parse)
    }

    fn set_field_type_override(&mut self, source: &str, field: &str, ty: Option<FieldType>) {
        use serde_json::{Map, Value};
        if !self.explorer.is_object() {
            self.explorer = Value::Object(Map::new());
        }
        let Some(root) = self.explorer.as_object_mut() else {
            return;
        };
        let types = root
            .entry("field_types")
            .or_insert_with(|| Value::Object(Map::new()));
        if !types.is_object() {
            *types = Value::Object(Map::new());
        }
        let Some(types) = types.as_object_mut() else {
            return;
        };
        let of_source = types
            .entry(source.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !of_source.is_object() {
            *of_source = Value::Object(Map::new());
        }
        if let Some(map) = of_source.as_object_mut() {
            match ty {
                Some(t) => {
                    map.insert(field.to_string(), Value::String(t.name().into()));
                }
                None => {
                    map.remove(field);
                }
            }
            if map.is_empty() {
                types.remove(source);
            }
        }
        if types.is_empty() {
            root.remove("field_types");
        }
        if root.is_empty() {
            self.explorer = Value::Null;
        }
    }

    /// A column's type: the override, else the type the source recorded
    /// when it was loaded.
    pub fn field_type(&self, source: &str, field: &str) -> Option<FieldType> {
        self.field_type_override(source, field).or_else(|| {
            self.sources
                .get(source)?
                .columns
                .get(field)
                .and_then(|t| FieldType::parse(t))
        })
    }
}

fn channel_slot(layer: &mut Layer, channel: Channel) -> &mut Option<ChannelBinding> {
    match channel {
        Channel::Color => &mut layer.color,
        Channel::Size => &mut layer.radius,
        Channel::Height => &mut layer.height,
    }
}

/// Wire helpers for the fields whose types carry no serde form of their
/// own.
mod wire {
    /// `Option<Option<T>>` as "absent" / `null` / a value.
    pub mod double_option {
        use serde::{Deserialize, Deserializer, Serialize, Serializer};

        pub fn serialize<T: Serialize, S: Serializer>(
            v: &Option<Option<T>>,
            s: S,
        ) -> Result<S::Ok, S::Error> {
            match v {
                Some(inner) => inner.serialize(s),
                None => s.serialize_none(),
            }
        }

        pub fn deserialize<'de, T: Deserialize<'de>, D: Deserializer<'de>>(
            d: D,
        ) -> Result<Option<Option<T>>, D::Error> {
            Option::<T>::deserialize(d).map(Some)
        }
    }

    /// A field type by its document name (`"integer"`, `"real"`, ...), or
    /// `null` for "back to the detected type".
    pub mod field_type {
        use serde::{de::Error, Deserialize, Deserializer, Serializer};

        use crate::table::FieldType;

        pub fn serialize<S: Serializer>(v: &Option<FieldType>, s: S) -> Result<S::Ok, S::Error> {
            match v {
                Some(t) => s.serialize_str(t.name()),
                None => s.serialize_none(),
            }
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<FieldType>, D::Error> {
            match Option::<String>::deserialize(d)? {
                None => Ok(None),
                Some(n) => FieldType::parse(&n)
                    .map(Some)
                    .ok_or_else(|| D::Error::custom(format!("unknown field type `{n}`"))),
            }
        }
    }

    /// A query source's settings as the same keys a source stores them
    /// under (`crate::sql`): `{"kind", "connection", "database"?, "sql",
    /// "max_rows"?, "mode"?, ...}`.
    pub mod sql_spec {
        use serde::{de::Error, Deserialize, Deserializer, Serialize, Serializer};
        use serde_json::{Map, Value};

        use crate::doc::{DataRef, Source};
        use crate::sql::{SqlSpec, QUERY_REF};

        pub fn serialize<S: Serializer>(spec: &SqlSpec, s: S) -> Result<S::Ok, S::Error> {
            let src = Source::new_sql(spec, "");
            let mut m = src.extra.clone();
            m.insert("kind".into(), Value::String(src.kind));
            m.serialize(s)
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<SqlSpec, D::Error> {
            let mut m = Map::<String, Value>::deserialize(d)?;
            let kind = match m.remove("kind") {
                Some(Value::String(k)) => k,
                _ => return Err(D::Error::custom("a query spec needs a `kind`")),
            };
            let src = Source {
                kind,
                data: DataRef {
                    kind: QUERY_REF.into(),
                    ..DataRef::default()
                },
                extra: m,
                ..Source::default()
            };
            src.sql_spec().ok_or_else(|| {
                D::Error::custom(format!("`{}` is not a query source kind", src.kind))
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{DataRef, LayerType, Vis};

    fn layer(id: &str) -> Layer {
        Layer {
            id: id.into(),
            kind: LayerType::Points,
            source: "s".into(),
            label: id.into(),
            visible: true,
            columns: Default::default(),
            vis: Vis::default(),
            color: None,
            radius: None,
            height: None,
            hover: Default::default(),
            extra: Default::default(),
        }
    }

    fn doc_with(ids: &[&str]) -> Doc {
        let mut d = Doc::default();
        for id in ids {
            d.apply(DocAction::AddLayer(layer(id)));
        }
        d
    }

    /// Which row of the pinned table covers `a`. Exhaustive on purpose: a
    /// new variant does not compile until it has a row here and in
    /// [`ROWS`].
    fn variant(a: &DocAction) -> usize {
        match a {
            DocAction::AddSource { .. } => 0,
            DocAction::AddLayer(_) => 1,
            DocAction::RemoveLayer { .. } => 2,
            DocAction::SetLayerVisible { .. } => 3,
            DocAction::SetVis { .. } => 4,
            DocAction::SetLabel { .. } => 5,
            DocAction::MoveLayer { .. } => 6,
            DocAction::SetView(_) => 7,
            DocAction::SetChannel { .. } => 8,
            DocAction::ClearChannel { .. } => 9,
            DocAction::AddFilter(_) => 10,
            DocAction::RemoveFilter { .. } => 11,
            DocAction::SetFilterValue { .. } => 12,
            DocAction::SetFilterEnabled { .. } => 13,
            DocAction::SetFilterSpeed { .. } => 14,
            DocAction::SetHover { .. } => 15,
            DocAction::SetFieldType { .. } => 16,
            DocAction::ReplaceLayer(_) => 17,
            DocAction::AddChart(_) => 18,
            DocAction::RemoveChart { .. } => 19,
            DocAction::SetSourceSql { .. } => 20,
        }
    }
    const VARIANTS: usize = 21;

    #[test]
    fn the_action_effect_table_is_pinned() {
        let source = Source {
            kind: "csv".into(),
            data: DataRef::file("a.csv"),
            label: "a".into(),
            columns: Default::default(),
            geometry: Default::default(),
            attribution: None,
            extra: Default::default(),
        };
        let rows: Vec<(DocAction, Vec<Effect>)> = vec![
            (
                DocAction::AddSource {
                    id: "s".into(),
                    source,
                },
                vec![Effect::None],
            ),
            (
                DocAction::AddLayer(layer("c")),
                vec![Effect::Push("c".into()), Effect::Fit("c".into())],
            ),
            (
                DocAction::RemoveLayer { layer: "a".into() },
                vec![Effect::Remove("a".into())],
            ),
            (
                DocAction::SetLayerVisible {
                    layer: "a".into(),
                    visible: false,
                },
                vec![Effect::Visibility("a".into())],
            ),
            (
                DocAction::SetVis {
                    layer: "a".into(),
                    patch: VisPatch::radius(30.0),
                },
                vec![Effect::Repaint("a".into())],
            ),
            (
                DocAction::SetLabel {
                    layer: "a".into(),
                    label: "A".into(),
                },
                vec![Effect::None],
            ),
            (
                DocAction::MoveLayer {
                    layer: "a".into(),
                    to: 1,
                },
                vec![Effect::Reorder],
            ),
            (DocAction::SetView(View::default()), vec![Effect::Camera]),
            (
                DocAction::SetChannel {
                    layer: "a".into(),
                    channel: Channel::Color,
                    binding: ChannelBinding::new("pop", "quantile"),
                },
                vec![Effect::Repaint("a".into())],
            ),
            // Nothing bound yet: clearing changes nothing.
            (
                DocAction::ClearChannel {
                    layer: "a".into(),
                    channel: Channel::Size,
                },
                vec![Effect::None],
            ),
            (
                DocAction::AddFilter(range("f", [0.0, 1.0])),
                vec![Effect::GpuFields("s".into())],
            ),
            (
                DocAction::RemoveFilter { id: "g0".into() },
                vec![Effect::GpuFields("s".into())],
            ),
            (
                DocAction::SetFilterValue {
                    id: "g0".into(),
                    value: FilterValue::Range([1.0, 2.0]),
                    live: true,
                },
                vec![Effect::GpuWindow("g0".into())],
            ),
            (
                DocAction::SetFilterEnabled {
                    id: "g0".into(),
                    enabled: false,
                },
                vec![Effect::GpuWindow("g0".into())],
            ),
            (
                DocAction::SetFilterSpeed {
                    id: "g0".into(),
                    speed: 2.0,
                },
                vec![Effect::None],
            ),
            (
                DocAction::SetHover {
                    layer: "a".into(),
                    hover: crate::tooltip::toggled(&Hover::default(), "pop"),
                },
                vec![Effect::Popup("a".into())],
            ),
            // No such dataset: nothing to re-type.
            (
                DocAction::SetFieldType {
                    source: "zz".into(),
                    field: "pop".into(),
                    ty: Some(FieldType::String),
                },
                vec![Effect::None],
            ),
            // A type switch takes the old objects off and draws the new; a
            // bin reads every row, so the dataset's GPU filter moves to the
            // CPU.
            (
                DocAction::ReplaceLayer(Layer {
                    kind: crate::doc::LayerType::Hexbin,
                    ..layer("a")
                }),
                vec![
                    Effect::Remove("a".into()),
                    Effect::Push("a".into()),
                    Effect::GpuFields("s".into()),
                    Effect::CpuFilter("s".into()),
                ],
            ),
            // A chart card: the map is untouched.
            (
                DocAction::AddChart(crate::doc::Chart::new(
                    "c1",
                    "s",
                    crate::doc::ChartKind::Histogram,
                    "pop",
                )),
                vec![Effect::Chart("c1".into())],
            ),
            // No such chart: nothing goes.
            (
                DocAction::RemoveChart { id: "zz".into() },
                vec![Effect::None],
            ),
            // Not a query source: nothing to set.
            (
                DocAction::SetSourceSql {
                    source: "s".into(),
                    spec: crate::sql::SqlSpec {
                        kind: "clickhouse".into(),
                        connection: "c".into(),
                        database: None,
                        sql: "SELECT 1".into(),
                        max_rows: None,
                        ..Default::default()
                    },
                },
                vec![Effect::None],
            ),
        ];
        let mut seen = [false; VARIANTS];
        for (action, want) in rows {
            seen[variant(&action)] = true;
            let mut d = doc_with(&["a", "b"]);
            d.apply(DocAction::AddFilter(range("g0", [0.0, 9.0])));
            assert_eq!(d.apply(action.clone()), want, "{action:?}");
        }
        assert!(
            seen.iter().all(|s| *s),
            "every DocAction variant has a row: {seen:?}"
        );
    }

    #[test]
    fn set_source_sql_runs_only_when_the_connection_or_cap_changes() {
        use crate::sql::SqlSpec;
        let spec = SqlSpec {
            kind: "clickhouse".into(),
            connection: "a".into(),
            database: None,
            sql: "SELECT 1 AS n".into(),
            max_rows: Some(10),
            ..Default::default()
        };
        let mut d = Doc::default();
        d.sources.insert("q".into(), Source::new_sql(&spec, "Q"));
        let set = |d: &mut Doc, s: &SqlSpec| {
            d.apply(DocAction::SetSourceSql {
                source: "q".into(),
                spec: s.clone(),
            })
        };
        assert_eq!(set(&mut d, &spec), vec![Effect::None], "unchanged");
        let text = SqlSpec {
            sql: "SELECT 2 AS n".into(),
            ..spec.clone()
        };
        assert_eq!(
            set(&mut d, &text),
            vec![Effect::None],
            "new text waits for Run"
        );
        assert_eq!(d.sources["q"].sql_spec().unwrap().sql, "SELECT 2 AS n");
        let conn = SqlSpec {
            connection: "b".into(),
            ..text.clone()
        };
        assert_eq!(set(&mut d, &conn), vec![Effect::Run("q".into())]);
        let cap = SqlSpec {
            max_rows: Some(20),
            ..conn.clone()
        };
        assert_eq!(set(&mut d, &cap), vec![Effect::Run("q".into())]);
        // One undo entry puts the whole spec back.
        let before = d.clone();
        let back = crate::history::inverse(
            &before,
            &DocAction::SetSourceSql {
                source: "q".into(),
                spec: spec.clone(),
            },
        )
        .unwrap();
        d.apply(DocAction::SetSourceSql {
            source: "q".into(),
            spec: spec.clone(),
        });
        for a in back {
            d.apply(a);
        }
        assert_eq!(d, before);
    }

    fn range(id: &str, r: [f64; 2]) -> Filter {
        Filter::new(
            id,
            "s",
            id,
            crate::filters::FilterKind::Range,
            FilterValue::Range(r),
        )
    }

    #[test]
    fn a_gpu_drag_moves_a_window_and_a_cpu_drag_waits_for_the_release() {
        let mut d = doc_with(&["a"]);
        for i in 0..5 {
            d.apply(DocAction::AddFilter(range(&format!("f{i}"), [0.0, 9.0])));
        }
        assert!(
            d.filter("f3").unwrap().placement.is_gpu()
                && !d.filter("f4").unwrap().placement.is_gpu()
        );
        let set = |d: &mut Doc, id: &str, v: f64, live| {
            d.apply(DocAction::SetFilterValue {
                id: id.into(),
                value: FilterValue::Range([0.0, v]),
                live,
            })
        };
        assert_eq!(
            set(&mut d, "f0", 5.0, true),
            vec![Effect::GpuWindow("f0".into())]
        );
        assert_eq!(
            set(&mut d, "f0", 5.0, false),
            vec![Effect::None],
            "the release of an unchanged GPU window writes nothing"
        );
        assert_eq!(
            set(&mut d, "f4", 5.0, true),
            vec![Effect::None],
            "a CPU filter follows the hand only on its card"
        );
        assert_eq!(
            set(&mut d, "f4", 5.0, false),
            vec![Effect::CpuFilter("s".into())]
        );
        // Removing a GPU filter promotes f4: the fields change and the CPU
        // set empties.
        assert_eq!(
            d.apply(DocAction::RemoveFilter { id: "f1".into() }),
            vec![Effect::GpuFields("s".into()), Effect::CpuFilter("s".into())]
        );
        assert_eq!(
            d.filter("f4").unwrap().placement,
            crate::filters::Placement::Gpu(1)
        );
        // A filter added to a polygon dataset is a CPU filter.
        let mut poly = layer("p");
        poly.kind = crate::doc::LayerType::GeoJson;
        poly.source = "polys".into();
        d.apply(DocAction::AddLayer(poly));
        let mut f = range("pf", [0.0, 1.0]);
        f.source = "polys".into();
        assert_eq!(
            d.apply(DocAction::AddFilter(f)),
            vec![Effect::CpuFilter("polys".into())]
        );
    }

    #[test]
    fn filters_survive_a_save_and_are_placed_again_on_load() {
        let mut d = doc_with(&["a"]);
        for i in 0..5 {
            d.apply(DocAction::AddFilter(range(&format!("f{i}"), [0.0, 9.0])));
        }
        let text = d.to_json();
        assert!(
            text.contains("\"dataId\": \"s\"")
                && text.contains("\"type\": \"range\"")
                && !text.contains("placement"),
            "{text}"
        );
        let back = Doc::from_json(&text).expect("parses");
        assert_eq!(back, d);
    }

    #[test]
    fn a_second_layer_on_a_dataset_moves_its_filters_and_removing_it_moves_them_back() {
        let mut d = doc_with(&["a"]);
        d.apply(DocAction::AddFilter(range("t", [0.0, 9.0])));
        assert!(
            d.filter("t").unwrap().placement.is_gpu(),
            "points alone: the filter is on the GPU"
        );
        // A second points layer on the dataset keeps it there.
        assert_eq!(
            d.apply(DocAction::AddLayer(layer("b"))),
            vec![Effect::Push("b".into()), Effect::Fit("b".into())]
        );
        // A hexbin reads every row: the dataset's filter moves to the CPU,
        // where it applies to every layer of the dataset.
        let hex = Layer {
            kind: crate::doc::LayerType::Hexbin,
            ..layer("h")
        };
        assert_eq!(
            d.apply(DocAction::AddLayer(hex)),
            vec![
                Effect::Push("h".into()),
                Effect::Fit("h".into()),
                Effect::GpuFields("s".into()),
                Effect::CpuFilter("s".into())
            ]
        );
        assert!(!d.filter("t").unwrap().placement.is_gpu());
        assert_eq!(
            d.apply(DocAction::RemoveLayer { layer: "h".into() }),
            vec![
                Effect::Remove("h".into()),
                Effect::GpuFields("s".into()),
                Effect::CpuFilter("s".into())
            ]
        );
        assert!(
            d.filter("t").unwrap().placement.is_gpu(),
            "points alone again: back on the GPU"
        );
        // Moving a layer to another dataset re-places both datasets' filters.
        let mut other = Filter::new(
            "u",
            "o",
            "u",
            crate::filters::FilterKind::Range,
            FilterValue::Range([0.0, 1.0]),
        );
        other.source = "o".into();
        d.apply(DocAction::AddLayer(Layer {
            kind: crate::doc::LayerType::Hexbin,
            source: "o".into(),
            ..layer("oh")
        }));
        d.apply(DocAction::AddFilter(other));
        assert!(!d.filter("u").unwrap().placement.is_gpu());
        let moved = Layer {
            source: "o".into(),
            ..layer("b")
        };
        let effects = d.apply(DocAction::ReplaceLayer(moved));
        assert_eq!(
            &effects[..2],
            &[Effect::Remove("b".into()), Effect::Push("b".into())]
        );
        assert_eq!(d.unique_layer_id("a"), "a-2");
        assert_eq!(d.unique_layer_id("zz"), "zz");
    }

    #[test]
    fn opacity_is_never_a_push() {
        let mut d = doc_with(&["a"]);
        for o in [0.0, 0.25, 0.5, 1.0] {
            let effects = d.apply(DocAction::SetVis {
                layer: "a".into(),
                patch: VisPatch::opacity(o),
            });
            assert!(
                effects
                    .iter()
                    .all(|e| matches!(e, Effect::Opacity(_) | Effect::None)),
                "{effects:?}"
            );
        }
        assert_eq!(d.layer("a").unwrap().vis.opacity, 1.0);
        // Opacity with a colour: a repaint and an opacity, still no push.
        let effects = d.apply(DocAction::SetVis {
            layer: "a".into(),
            patch: VisPatch {
                opacity: Some(0.3),
                color: Some([1, 2, 3]),
                ..Default::default()
            },
        });
        assert_eq!(
            effects,
            vec![Effect::Repaint("a".into()), Effect::Opacity("a".into())]
        );
    }

    #[test]
    fn an_unknown_layer_changes_nothing() {
        let mut d = doc_with(&["a"]);
        let before = d.clone();
        assert_eq!(
            d.apply(DocAction::SetVis {
                layer: "zz".into(),
                patch: VisPatch::radius(3.0)
            }),
            vec![Effect::None]
        );
        assert_eq!(
            d.apply(DocAction::RemoveLayer { layer: "zz".into() }),
            vec![Effect::None]
        );
        assert_eq!(d, before);
    }

    #[test]
    fn values_are_clamped_to_their_ranges() {
        let mut d = doc_with(&["a"]);
        d.apply(DocAction::SetVis {
            layer: "a".into(),
            patch: VisPatch {
                radius: Some(500.0),
                opacity: Some(2.0),
                thickness: Some(-1.0),
                ..Default::default()
            },
        });
        let v = &d.layer("a").unwrap().vis;
        assert_eq!((v.radius, v.opacity, v.thickness), (100.0, 1.0, 0.0));
    }

    #[test]
    fn a_channel_edit_is_a_repaint_never_a_push() {
        let mut d = doc_with(&["a"]);
        let mut b = ChannelBinding::new("pop", "quantile");
        b.palette = Some("Global Warming".into());
        let set = |d: &mut Doc, b: &ChannelBinding| {
            d.apply(DocAction::SetChannel {
                layer: "a".into(),
                channel: Channel::Color,
                binding: b.clone(),
            })
        };
        assert_eq!(set(&mut d, &b), vec![Effect::Repaint("a".into())]);
        assert_eq!(
            set(&mut d, &b),
            vec![Effect::None],
            "the same binding again changes nothing"
        );
        b.reversed = true;
        assert_eq!(
            set(&mut d, &b),
            vec![Effect::Repaint("a".into())],
            "a palette change is a repaint"
        );
        assert_eq!(d.layer("a").unwrap().color.as_ref(), Some(&b));
        assert_eq!(
            d.apply(DocAction::SetChannel {
                layer: "a".into(),
                channel: Channel::Size,
                binding: ChannelBinding::new("pop", "sqrt")
            }),
            vec![Effect::Repaint("a".into())]
        );
        assert!(d.layer("a").unwrap().radius.is_some());
        assert_eq!(
            d.apply(DocAction::ClearChannel {
                layer: "a".into(),
                channel: Channel::Color
            }),
            vec![Effect::Repaint("a".into())]
        );
        assert!(d.layer("a").unwrap().color.is_none() && d.layer("a").unwrap().radius.is_some());
        assert_eq!(
            d.apply(DocAction::SetChannel {
                layer: "zz".into(),
                channel: Channel::Color,
                binding: b
            }),
            vec![Effect::None]
        );
    }

    #[test]
    fn overriding_a_number_to_string_rebinds_its_colour_as_ordinal_and_drops_its_size() {
        let mut d = doc_with(&["a"]);
        let mut columns = std::collections::BTreeMap::new();
        columns.insert("pop".to_string(), "integer".to_string());
        d.sources.insert(
            "s".into(),
            Source {
                kind: "csv".into(),
                data: DataRef::file("a.csv"),
                label: "a".into(),
                columns,
                geometry: Default::default(),
                attribution: None,
                extra: Default::default(),
            },
        );
        d.apply(DocAction::SetChannel {
            layer: "a".into(),
            channel: Channel::Color,
            binding: ChannelBinding::new("pop", "quantile"),
        });
        d.apply(DocAction::SetChannel {
            layer: "a".into(),
            channel: Channel::Size,
            binding: ChannelBinding::new("pop", "sqrt"),
        });
        let set = |d: &mut Doc, ty| {
            d.apply(DocAction::SetFieldType {
                source: "s".into(),
                field: "pop".into(),
                ty,
            })
        };
        assert_eq!(
            set(&mut d, Some(FieldType::String)),
            vec![Effect::Retype("s".into())]
        );
        let l = d.layer("a").unwrap();
        assert_eq!(l.color.as_ref().map(|b| b.scale.as_str()), Some("ordinal"));
        assert_eq!(
            l.color.as_ref().and_then(|b| b.palette.as_deref()),
            Some(crate::scales::DEFAULT_QUALITATIVE)
        );
        assert!(l.radius.is_none(), "a string has no size scale");
        assert_eq!(d.field_type("s", "pop"), Some(FieldType::String));
        assert_eq!(
            set(&mut d, Some(FieldType::String)),
            vec![Effect::None],
            "the same override again changes nothing"
        );
        // Back to the detected type: the override goes, the explorer block empties.
        assert_eq!(set(&mut d, None), vec![Effect::Retype("s".into())]);
        assert_eq!(d.field_type("s", "pop"), Some(FieldType::Integer));
        assert!(d.explorer.is_null(), "{:?}", d.explorer);
    }

    #[test]
    fn the_outline_and_its_colour_are_repaints() {
        let mut d = doc_with(&["a"]);
        assert_eq!(
            d.apply(DocAction::SetVis {
                layer: "a".into(),
                patch: VisPatch::outline(true)
            }),
            vec![Effect::Repaint("a".into())]
        );
        assert_eq!(
            d.apply(DocAction::SetVis {
                layer: "a".into(),
                patch: VisPatch::stroke_color(Some([1, 2, 3]))
            }),
            vec![Effect::Repaint("a".into())]
        );
        assert_eq!(
            d.apply(DocAction::SetVis {
                layer: "a".into(),
                patch: VisPatch::stroke_color(Some([1, 2, 3]))
            }),
            vec![Effect::None]
        );
        assert_eq!(
            d.apply(DocAction::SetVis {
                layer: "a".into(),
                patch: VisPatch::stroke_color(None)
            }),
            vec![Effect::Repaint("a".into())]
        );
        assert_eq!(d.layer("a").unwrap().vis.stroke_color, None);
    }
}

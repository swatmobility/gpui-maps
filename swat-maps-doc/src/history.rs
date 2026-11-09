//! Undo and redo (spec §6.5): every recorded edit keeps the actions that
//! put the document back, computed from the document before the edit.
//! Undo applies them; redo applies the edit again. Pure: the host applies
//! the returned actions through its own dispatch, so an undo changes the
//! map by the same effects an edit does.
//!
//! A drag is one step: a held filter control's live values fold into its
//! release, and consecutive changes to the same layer setting (an opacity
//! slider's stream of values) fold into one entry.
//!
//! A batch is one step too (agent chat spec §6.3): the actions recorded
//! between [`History::begin_batch`] and [`History::end_batch`] become one
//! entry, `forward` in order and `inverse` the reversed concatenation of
//! each action's own inverse. Nothing folds inside a batch. An action
//! that cannot be undone (adding a dataset) ends the batch early: what
//! came before it is one entry, and the end reports it.

use std::collections::HashMap;

use crate::action::{Channel, DocAction, VisPatch};
use crate::doc::Doc;
use crate::filters::FilterValue;

/// Steps kept (plan Task 5.6).
pub const MAX_STEPS: usize = 200;

/// An undo entry's identity: stable while it moves between the undo and
/// redo stacks, never reused.
pub type EntryId = u64;

#[derive(Clone, Debug, PartialEq)]
struct Entry {
    id: EntryId,
    /// The edit, as applied (redo applies these in order).
    forward: Vec<DocAction>,
    /// What puts the document back (undo applies these in order).
    inverse: Vec<DocAction>,
    /// A batch's entry: nothing ever folds into it.
    batch: bool,
}

/// An open batch: every action so far, and the inverses in the order undo
/// applies them (latest first).
#[derive(Clone, Debug, Default)]
struct Batch {
    forward: Vec<DocAction>,
    inverse: Vec<DocAction>,
}

/// How a batch ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatchEnd {
    /// The one entry the batch became; `None` when it changed nothing
    /// undoable.
    pub entry: Option<EntryId>,
    /// An action that cannot be undone ended it early: what followed that
    /// action was recorded as ordinary steps.
    pub ended_early: bool,
}

#[derive(Clone, Debug, Default)]
pub struct History {
    undo: Vec<Entry>,
    redo: Vec<Entry>,
    /// A held filter control's value from before the hold, by filter id.
    held: HashMap<String, FilterValue>,
    /// The batch being recorded, if any.
    batch: Option<Batch>,
    /// A batch was opened and ended early by an unundoable action.
    broken: bool,
    next_id: EntryId,
}

impl History {
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn clear(&mut self) {
        let next_id = self.next_id;
        *self = History {
            next_id,
            ..History::default()
        };
    }

    /// Starts a batch: until [`Self::end_batch`], every recorded action
    /// joins one entry. A batch already open is ended first.
    pub fn begin_batch(&mut self) {
        if self.batch.is_some() {
            self.end_batch();
        }
        self.batch = Some(Batch::default());
        self.broken = false;
    }

    /// Ends the batch: its actions become one entry (none when nothing
    /// undoable changed).
    pub fn end_batch(&mut self) -> BatchEnd {
        let ended_early = std::mem::take(&mut self.broken);
        let entry = match self.batch.take() {
            Some(b) if !b.forward.is_empty() => Some(self.push(Entry {
                id: 0,
                forward: b.forward,
                inverse: b.inverse,
                batch: true,
            })),
            Some(_) => None,
            // Ended early: the entry was pushed then.
            None if ended_early => self.undo.last().map(|e| e.id),
            None => None,
        };
        BatchEnd { entry, ended_early }
    }

    /// Whether entry `id` is the next one undo would take back.
    pub fn is_top(&self, id: EntryId) -> bool {
        self.batch.is_none() && self.undo.last().is_some_and(|e| e.id == id)
    }

    /// Whether entry `id` is on the undo stack at all.
    pub fn is_undoable(&self, id: EntryId) -> bool {
        self.undo.iter().any(|e| e.id == id)
    }

    /// Records `action`, about to be applied to `before`. Call it before
    /// [`Doc::apply`]. An action that cannot be undone (adding a dataset)
    /// is not recorded.
    pub fn record(&mut self, before: &Doc, action: &DocAction) {
        if let Some(batch) = &mut self.batch {
            match inverse(before, action) {
                Some(mut back) => {
                    batch.forward.push(action.clone());
                    // Undo applies the latest action's inverse first.
                    back.append(&mut batch.inverse);
                    batch.inverse = back;
                }
                None if matches!(action, DocAction::AddSource { .. }) => {
                    // Not undoable: what came before is one entry; the
                    // rest are ordinary steps.
                    let b = self.batch.take().unwrap_or_default();
                    if !b.forward.is_empty() {
                        self.push(Entry {
                            id: 0,
                            forward: b.forward,
                            inverse: b.inverse,
                            batch: true,
                        });
                    }
                    self.broken = true;
                }
                None => {}
            }
            return;
        }
        // A held control: remember where it started, record on release.
        if let DocAction::SetFilterValue { id, live, value } = action {
            let Some(f) = before.filter(id) else { return };
            if *live {
                self.held
                    .entry(id.clone())
                    .or_insert_with(|| f.value.clone());
                return;
            }
            let start = self.held.remove(id).unwrap_or_else(|| f.value.clone());
            if &start == value {
                return;
            }
            let forward = DocAction::SetFilterValue {
                id: id.clone(),
                value: value.clone(),
                live: false,
            };
            self.push(Entry {
                id: 0,
                forward: vec![forward],
                inverse: vec![DocAction::SetFilterValue {
                    id: id.clone(),
                    value: start,
                    live: false,
                }],
                batch: false,
            });
            return;
        }
        let Some(inverse) = inverse(before, action) else {
            return;
        };
        // A slider's stream: the same layer's same setting again keeps the
        // first entry's way back and the latest value.
        if let Some(top) = self.undo.last_mut() {
            if self.redo.is_empty()
                && !top.batch
                && top.forward.len() == 1
                && folds(&top.forward[0], action)
            {
                top.forward[0] = action.clone();
                return;
            }
        }
        self.push(Entry {
            id: 0,
            forward: vec![action.clone()],
            inverse,
            batch: false,
        });
    }

    /// Pushes `entry` with a fresh id, which it returns.
    fn push(&mut self, mut entry: Entry) -> EntryId {
        self.next_id += 1;
        entry.id = self.next_id;
        self.redo.clear();
        self.undo.push(entry);
        if self.undo.len() > MAX_STEPS {
            self.undo.remove(0);
        }
        self.next_id
    }

    /// The actions that undo the latest step, or `None`.
    pub fn undo(&mut self) -> Option<Vec<DocAction>> {
        let entry = self.undo.pop()?;
        let out = entry.inverse.clone();
        self.redo.push(entry);
        Some(out)
    }

    /// The actions that redo the latest undone step, or `None`.
    pub fn redo(&mut self) -> Option<Vec<DocAction>> {
        let entry = self.redo.pop()?;
        let out = entry.forward.clone();
        self.undo.push(entry);
        Some(out)
    }
}

/// Whether `next` continues `prev`'s gesture (the same field of the same
/// layer, or the same label).
fn folds(prev: &DocAction, next: &DocAction) -> bool {
    match (prev, next) {
        (DocAction::SetVis { layer: a, patch: p }, DocAction::SetVis { layer: b, patch: q }) => {
            a == b && shape(p) == shape(q)
        }
        (DocAction::SetLabel { layer: a, .. }, DocAction::SetLabel { layer: b, .. }) => a == b,
        (DocAction::SetFilterSpeed { id: a, .. }, DocAction::SetFilterSpeed { id: b, .. }) => {
            a == b
        }
        _ => false,
    }
}

/// Which fields a patch sets.
fn shape(p: &VisPatch) -> [bool; 7] {
    [
        p.radius.is_some(),
        p.opacity.is_some(),
        p.filled.is_some(),
        p.outline.is_some(),
        p.thickness.is_some(),
        p.color.is_some(),
        p.stroke_color.is_some(),
    ]
}

/// The actions that take `after(before, action)` back to `before`, or
/// `None` when the action is not undoable or changes nothing.
pub fn inverse(before: &Doc, action: &DocAction) -> Option<Vec<DocAction>> {
    Some(match action {
        DocAction::AddSource { .. } => return None,
        DocAction::AddLayer(layer) => match before.layer(&layer.id) {
            Some(old) => vec![DocAction::AddLayer(old.clone())],
            None => vec![DocAction::RemoveLayer {
                layer: layer.id.clone(),
            }],
        },
        DocAction::RemoveLayer { layer } => {
            let at = before.map.layers.iter().position(|l| &l.id == layer)?;
            vec![
                DocAction::AddLayer(before.map.layers[at].clone()),
                DocAction::MoveLayer {
                    layer: layer.clone(),
                    to: at,
                },
            ]
        }
        DocAction::SetLayerVisible { layer, .. } => vec![DocAction::SetLayerVisible {
            layer: layer.clone(),
            visible: before.layer(layer)?.visible,
        }],
        DocAction::SetVis { layer, patch } => {
            let v = &before.layer(layer)?.vis;
            let back = VisPatch {
                radius: patch.radius.map(|_| v.radius),
                opacity: patch.opacity.map(|_| v.opacity),
                filled: patch.filled.map(|_| v.filled),
                outline: patch.outline.map(|_| v.outline),
                thickness: patch.thickness.map(|_| v.thickness),
                color: patch.color.map(|_| v.color),
                stroke_color: patch.stroke_color.map(|_| v.stroke_color),
            };
            vec![DocAction::SetVis {
                layer: layer.clone(),
                patch: back,
            }]
        }
        DocAction::SetLabel { layer, .. } => vec![DocAction::SetLabel {
            layer: layer.clone(),
            label: before.layer(layer)?.label.clone(),
        }],
        DocAction::MoveLayer { layer, .. } => {
            let at = before.map.layers.iter().position(|l| &l.id == layer)?;
            vec![DocAction::MoveLayer {
                layer: layer.clone(),
                to: at,
            }]
        }
        DocAction::SetView(_) => vec![DocAction::SetView(before.map.view.clone())],
        DocAction::SetChannel { layer, channel, .. }
        | DocAction::ClearChannel { layer, channel } => {
            let l = before.layer(layer)?;
            let old = match channel {
                Channel::Color => &l.color,
                Channel::Size => &l.radius,
                Channel::Height => &l.height,
            };
            match old {
                Some(b) => vec![DocAction::SetChannel {
                    layer: layer.clone(),
                    channel: *channel,
                    binding: b.clone(),
                }],
                None => vec![DocAction::ClearChannel {
                    layer: layer.clone(),
                    channel: *channel,
                }],
            }
        }
        DocAction::AddFilter(f) => {
            if before.filter(&f.id).is_some() {
                return None;
            }
            vec![DocAction::RemoveFilter { id: f.id.clone() }]
        }
        DocAction::RemoveFilter { id } => vec![DocAction::AddFilter(before.filter(id)?.clone())],
        DocAction::SetFilterValue { id, .. } => vec![DocAction::SetFilterValue {
            id: id.clone(),
            value: before.filter(id)?.value.clone(),
            live: false,
        }],
        DocAction::SetFilterEnabled { id, .. } => vec![DocAction::SetFilterEnabled {
            id: id.clone(),
            enabled: before.filter(id)?.enabled,
        }],
        DocAction::SetFilterSpeed { id, .. } => vec![DocAction::SetFilterSpeed {
            id: id.clone(),
            speed: before.filter(id)?.speed.unwrap_or(1.0),
        }],
        DocAction::ReplaceLayer(layer) => {
            vec![DocAction::ReplaceLayer(before.layer(&layer.id)?.clone())]
        }
        DocAction::SetHover { layer, .. } => vec![DocAction::SetHover {
            layer: layer.clone(),
            hover: before.layer(layer)?.hover.clone(),
        }],
        DocAction::AddChart(chart) => match before.chart(&chart.id) {
            Some(old) if old == chart => return None,
            Some(old) => vec![DocAction::AddChart(old.clone())],
            None => vec![DocAction::RemoveChart {
                id: chart.id.clone(),
            }],
        },
        DocAction::RemoveChart { id } => vec![DocAction::AddChart(before.chart(id)?.clone())],
        DocAction::SetSourceSql { source, .. } => vec![DocAction::SetSourceSql {
            source: source.clone(),
            spec: before.sources.get(source)?.sql_spec()?,
        }],
        DocAction::SetFieldType { source, field, .. } => {
            // The type back, then every layer of the dataset as it was
            // (the re-type may have re-bound their channels).
            let mut out = vec![DocAction::SetFieldType {
                source: source.clone(),
                field: field.clone(),
                ty: before.field_type_override(source, field),
            }];
            out.extend(
                before
                    .layers_of(source)
                    .map(|l| DocAction::AddLayer(l.clone())),
            );
            out
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{ChannelBinding, Filter, Layer, LayerType, Vis};
    use crate::filters::FilterKind;

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

    /// Applies `a` through the history, as a host does.
    fn edit(doc: &mut Doc, h: &mut History, a: DocAction) {
        h.record(doc, &a);
        doc.apply(a);
    }

    fn run(doc: &mut Doc, actions: Option<Vec<DocAction>>) {
        for a in actions.expect("a step") {
            doc.apply(a);
        }
    }

    #[test]
    fn every_edit_undoes_to_the_document_before_it_and_redoes_to_the_one_after() {
        let mut doc = Doc::default();
        doc.apply(DocAction::AddLayer(layer("a")));
        doc.apply(DocAction::AddLayer(layer("b")));
        doc.apply(DocAction::AddFilter(Filter::new(
            "f",
            "s",
            "x",
            FilterKind::Range,
            FilterValue::Range([0.0, 9.0]),
        )));
        let edits = vec![
            DocAction::SetVis {
                layer: "a".into(),
                patch: VisPatch {
                    radius: Some(40.0),
                    color: Some([1, 2, 3]),
                    ..Default::default()
                },
            },
            DocAction::SetLayerVisible {
                layer: "b".into(),
                visible: false,
            },
            DocAction::SetLabel {
                layer: "a".into(),
                label: "Renamed".into(),
            },
            DocAction::MoveLayer {
                layer: "a".into(),
                to: 1,
            },
            DocAction::SetChannel {
                layer: "a".into(),
                channel: Channel::Color,
                binding: ChannelBinding::new("x", "quantile"),
            },
            DocAction::ClearChannel {
                layer: "a".into(),
                channel: Channel::Color,
            },
            DocAction::RemoveLayer { layer: "b".into() },
            DocAction::AddLayer(layer("c")),
            DocAction::SetFilterEnabled {
                id: "f".into(),
                enabled: false,
            },
            DocAction::SetFilterValue {
                id: "f".into(),
                value: FilterValue::Range([1.0, 2.0]),
                live: false,
            },
            DocAction::RemoveFilter { id: "f".into() },
            DocAction::SetHover {
                layer: "a".into(),
                hover: crate::tooltip::toggled(&Default::default(), "x"),
            },
        ];
        for a in edits {
            let mut h = History::default();
            let before = doc.clone();
            edit(&mut doc, &mut h, a.clone());
            let after = doc.clone();
            assert_ne!(before, after, "{a:?} changed something");
            run(&mut doc, h.undo());
            assert_eq!(doc, before, "undo of {a:?}");
            run(&mut doc, h.redo());
            assert_eq!(doc, after, "redo of {a:?}");
        }
    }

    #[test]
    fn a_held_control_is_one_step_back_to_where_the_hold_began() {
        let mut doc = Doc::default();
        doc.apply(DocAction::AddLayer(layer("a")));
        doc.apply(DocAction::AddFilter(Filter::new(
            "f",
            "s",
            "x",
            FilterKind::Range,
            FilterValue::Range([0.0, 9.0]),
        )));
        let mut h = History::default();
        for v in [8.0, 7.0, 6.0] {
            edit(
                &mut doc,
                &mut h,
                DocAction::SetFilterValue {
                    id: "f".into(),
                    value: FilterValue::Range([0.0, v]),
                    live: true,
                },
            );
        }
        edit(
            &mut doc,
            &mut h,
            DocAction::SetFilterValue {
                id: "f".into(),
                value: FilterValue::Range([0.0, 5.0]),
                live: false,
            },
        );
        run(&mut doc, h.undo());
        assert_eq!(
            doc.filter("f").unwrap().value,
            FilterValue::Range([0.0, 9.0])
        );
        assert!(!h.can_undo());
    }

    #[test]
    fn a_slider_stream_is_one_step() {
        let mut doc = Doc::default();
        doc.apply(DocAction::AddLayer(layer("a")));
        let mut h = History::default();
        for o in [0.7, 0.6, 0.5, 0.4] {
            edit(
                &mut doc,
                &mut h,
                DocAction::SetVis {
                    layer: "a".into(),
                    patch: VisPatch::opacity(o),
                },
            );
        }
        edit(
            &mut doc,
            &mut h,
            DocAction::SetVis {
                layer: "a".into(),
                patch: VisPatch::radius(50.0),
            },
        );
        run(&mut doc, h.undo());
        assert_eq!(doc.layer("a").unwrap().vis.radius, Vis::default().radius);
        assert_eq!(doc.layer("a").unwrap().vis.opacity, 0.4);
        run(&mut doc, h.undo());
        assert_eq!(doc.layer("a").unwrap().vis.opacity, Vis::default().opacity);
        assert!(!h.can_undo());
    }

    #[test]
    fn a_new_edit_clears_redo_and_the_stack_is_capped() {
        let mut doc = Doc::default();
        doc.apply(DocAction::AddLayer(layer("a")));
        let mut h = History::default();
        for i in 0..(MAX_STEPS + 20) {
            edit(
                &mut doc,
                &mut h,
                DocAction::SetLayerVisible {
                    layer: "a".into(),
                    visible: i % 2 == 1,
                },
            );
        }
        assert_eq!(h.undo.len(), MAX_STEPS);
        h.undo();
        assert!(h.can_redo());
        edit(
            &mut doc,
            &mut h,
            DocAction::SetLabel {
                layer: "a".into(),
                label: "x".into(),
            },
        );
        assert!(!h.can_redo());
    }

    fn source() -> crate::doc::Source {
        crate::doc::Source {
            kind: "csv".into(),
            data: crate::doc::DataRef::file("a.csv"),
            label: "a".into(),
            columns: Default::default(),
            geometry: Default::default(),
            attribution: None,
            extra: Default::default(),
        }
    }

    #[test]
    fn a_batch_is_one_entry_with_reversed_inverses() {
        let mut doc = Doc::default();
        doc.apply(DocAction::AddLayer(layer("a")));
        let start = doc.clone();
        let mut h = History::default();
        h.begin_batch();
        // Colour, radius and a colour-by in one agent call; two changes to
        // the same setting must not fold inside a batch.
        edit(
            &mut doc,
            &mut h,
            DocAction::SetVis {
                layer: "a".into(),
                patch: VisPatch::color([1, 2, 3]),
            },
        );
        edit(
            &mut doc,
            &mut h,
            DocAction::SetVis {
                layer: "a".into(),
                patch: VisPatch::radius(30.0),
            },
        );
        edit(
            &mut doc,
            &mut h,
            DocAction::SetVis {
                layer: "a".into(),
                patch: VisPatch::radius(40.0),
            },
        );
        edit(
            &mut doc,
            &mut h,
            DocAction::SetChannel {
                layer: "a".into(),
                channel: Channel::Color,
                binding: ChannelBinding::new("x", "quantile"),
            },
        );
        edit(
            &mut doc,
            &mut h,
            DocAction::AddChart(crate::doc::Chart::new(
                "c1",
                "s",
                crate::doc::ChartKind::Histogram,
                "x",
            )),
        );
        assert!(!h.can_undo(), "nothing is an entry until the batch ends");
        let end = h.end_batch();
        let id = end.entry.expect("one entry");
        assert!(!end.ended_early);
        assert!(h.is_top(id));
        assert_eq!(h.undo.len(), 1, "one entry");
        assert_eq!(h.undo[0].forward.len(), 5);
        let after = doc.clone();
        // A user's slider stream afterwards sits above it, never inside.
        edit(
            &mut doc,
            &mut h,
            DocAction::SetVis {
                layer: "a".into(),
                patch: VisPatch::radius(50.0),
            },
        );
        assert!(!h.is_top(id), "a later edit is on top");
        assert_eq!(h.undo.len(), 2);
        run(&mut doc, h.undo());
        assert_eq!(doc, after);
        assert!(h.is_top(id));
        run(&mut doc, h.undo());
        assert_eq!(doc, start, "one undo takes the whole batch back");
        run(&mut doc, h.redo());
        assert_eq!(doc, after, "one redo puts it all back");
        assert!(h.is_top(id), "the id survives a redo");
    }

    #[test]
    fn a_non_undoable_action_ends_the_batch() {
        let mut doc = Doc::default();
        doc.apply(DocAction::AddLayer(layer("a")));
        let mut h = History::default();
        h.begin_batch();
        edit(
            &mut doc,
            &mut h,
            DocAction::SetLayerVisible {
                layer: "a".into(),
                visible: false,
            },
        );
        edit(
            &mut doc,
            &mut h,
            DocAction::AddSource {
                id: "t".into(),
                source: source(),
            },
        );
        edit(
            &mut doc,
            &mut h,
            DocAction::SetLabel {
                layer: "a".into(),
                label: "x".into(),
            },
        );
        let end = h.end_batch();
        assert!(end.ended_early, "reported");
        assert_eq!(
            h.undo.len(),
            2,
            "the part before the dataset is one entry, the label its own step"
        );
        // An empty batch is no entry.
        h.begin_batch();
        assert_eq!(
            h.end_batch(),
            BatchEnd {
                entry: None,
                ended_early: false
            }
        );
        assert_eq!(h.undo.len(), 2);
    }

    #[test]
    fn a_chart_undoes_and_redoes() {
        let mut doc = Doc::default();
        let mut h = History::default();
        let chart = crate::doc::Chart::new("c1", "s", crate::doc::ChartKind::Bar, "kind");
        edit(&mut doc, &mut h, DocAction::AddChart(chart.clone()));
        edit(&mut doc, &mut h, DocAction::RemoveChart { id: "c1".into() });
        assert!(doc.charts.is_empty());
        run(&mut doc, h.undo());
        assert_eq!(doc.charts, vec![chart]);
        run(&mut doc, h.undo());
        assert!(doc.charts.is_empty());
    }

    #[test]
    fn adding_a_dataset_is_not_a_step() {
        let mut h = History::default();
        let doc = Doc::default();
        let source = crate::doc::Source {
            kind: "csv".into(),
            data: crate::doc::DataRef::file("a.csv"),
            label: "a".into(),
            columns: Default::default(),
            geometry: Default::default(),
            attribution: None,
            extra: Default::default(),
        };
        h.record(
            &doc,
            &DocAction::AddSource {
                id: "s".into(),
                source,
            },
        );
        assert!(!h.can_undo());
    }
}

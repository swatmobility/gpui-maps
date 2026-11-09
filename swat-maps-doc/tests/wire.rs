//! The wire form of [`DocAction`] (`{"op": ..., ...}`): every variant
//! round-trips, and the golden fixture stays byte-stable.

use swat_maps_doc::action::{Channel, DocAction, VisPatch};
use swat_maps_doc::doc::{
    Chart, ChartKind, DataRef, Doc, Hover, HoverField, Layer, LayerType, Source, View,
};
use swat_maps_doc::filters::{FilterKind, FilterValue};
use swat_maps_doc::sql::{SqlMode, SqlSpec};
use swat_maps_doc::{ChannelBinding, FieldType, Filter};

const GOLDEN: &str = include_str!("fixtures/actions.json");

/// Exhaustive on purpose: a new variant does not compile until it has a
/// sample here (and so a line in the golden fixture).
fn op_index(a: &DocAction) -> usize {
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

fn layer(id: &str) -> Layer {
    let mut l: Layer = serde_json::from_value(
        serde_json::json!({"id": id, "type": "points", "source": "stations"}),
    )
    .unwrap();
    l.label = "Stations".into();
    l
}

fn samples() -> Vec<DocAction> {
    let source = Source {
        kind: "csv".into(),
        data: DataRef::dataset("ds_1", Some(3)),
        label: "stations".into(),
        ..Source::default()
    };
    let mut colour = ChannelBinding::new("capacity", "quantile");
    colour.palette = Some("Viridis".into());
    colour.steps = Some(6);
    let view = View {
        lon: 139.76,
        lat: 35.68,
        zoom: 11.0,
        ..View::default()
    };
    let filter = Filter::new(
        "f1",
        "stations",
        "capacity",
        FilterKind::Range,
        FilterValue::Range([1.0, 20.0]),
    );
    let sql = SqlSpec {
        kind: "clickhouse".into(),
        connection: "analytics".into(),
        database: Some("trips".into()),
        sql: "SELECT 1".into(),
        max_rows: Some(1000),
        mode: SqlMode::Rows,
        ..SqlSpec::default()
    };
    let mut replaced = layer("l1");
    replaced.kind = LayerType::Heatmap;
    vec![
        DocAction::AddSource {
            id: "stations".into(),
            source,
        },
        DocAction::AddLayer(layer("l1")),
        DocAction::RemoveLayer { layer: "l1".into() },
        DocAction::SetLayerVisible {
            layer: "l1".into(),
            visible: false,
        },
        DocAction::SetVis {
            layer: "l1".into(),
            patch: VisPatch {
                opacity: Some(0.5),
                color: Some([255, 0, 0]),
                stroke_color: Some(None),
                ..VisPatch::default()
            },
        },
        DocAction::SetLabel {
            layer: "l1".into(),
            label: "Bikes".into(),
        },
        DocAction::MoveLayer {
            layer: "l1".into(),
            to: 0,
        },
        DocAction::SetView(view),
        DocAction::SetChannel {
            layer: "l1".into(),
            channel: Channel::Color,
            binding: colour,
        },
        DocAction::ClearChannel {
            layer: "l1".into(),
            channel: Channel::Size,
        },
        DocAction::AddFilter(filter),
        DocAction::RemoveFilter { id: "f1".into() },
        DocAction::SetFilterValue {
            id: "f1".into(),
            value: FilterValue::Values(vec!["a".into()]),
            live: false,
        },
        DocAction::SetFilterEnabled {
            id: "f1".into(),
            enabled: true,
        },
        DocAction::SetFilterSpeed {
            id: "f1".into(),
            speed: 2.0,
        },
        DocAction::SetHover {
            layer: "l1".into(),
            hover: Hover {
                card: vec![HoverField::new("name")],
                ..Hover::default()
            },
        },
        DocAction::SetFieldType {
            source: "stations".into(),
            field: "capacity".into(),
            ty: Some(FieldType::Real),
        },
        DocAction::ReplaceLayer(replaced),
        DocAction::AddChart(Chart::new(
            "c1",
            "stations",
            ChartKind::Histogram,
            "capacity",
        )),
        DocAction::RemoveChart { id: "c1".into() },
        DocAction::SetSourceSql {
            source: "trips".into(),
            spec: sql,
        },
    ]
}

#[test]
fn the_samples_cover_every_variant() {
    let mut seen = [false; VARIANTS];
    for a in samples() {
        seen[op_index(&a)] = true;
    }
    assert!(seen.iter().all(|s| *s), "a variant has no sample: {seen:?}");
}

#[test]
fn doc_action_round_trips() {
    for a in samples() {
        let json = serde_json::to_string(&a).unwrap();
        let back: DocAction = serde_json::from_str(&json).unwrap_or_else(|e| panic!("{json}: {e}"));
        assert_eq!(back, a, "{json}");
        // And through a `Value`, as a server holding JSON does.
        let v = serde_json::to_value(&a).unwrap();
        assert_eq!(serde_json::from_value::<DocAction>(v).unwrap(), a);
    }
}

#[test]
fn doc_action_golden() {
    let now = serde_json::to_string_pretty(&samples()).unwrap() + "\n";
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/actions.json"),
            &now,
        )
        .unwrap();
        return;
    }
    assert_eq!(
        now, GOLDEN,
        "the DocAction wire form changed; a stored log would no longer read the same"
    );
    let parsed: Vec<DocAction> = serde_json::from_str(GOLDEN).unwrap();
    assert_eq!(parsed, samples());
}

#[test]
fn every_op_is_tagged_snake_case() {
    for a in samples() {
        let v = serde_json::to_value(&a).unwrap();
        let op = v["op"].as_str().unwrap().to_string();
        assert!(
            op.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "{op}"
        );
    }
}

#[test]
fn an_unknown_op_is_refused_with_its_name() {
    let err = serde_json::from_str::<DocAction>(r#"{"op":"explode"}"#)
        .unwrap_err()
        .to_string();
    assert!(err.contains("explode"), "{err}");
}

#[test]
fn a_vis_patch_without_stroke_color_keeps_it_and_null_resets_it() {
    let keep: VisPatch = serde_json::from_str(r#"{"opacity":0.4}"#).unwrap();
    assert_eq!(keep.stroke_color, None);
    let reset: VisPatch = serde_json::from_str(r#"{"strokeColor":null}"#).unwrap();
    assert_eq!(reset.stroke_color, Some(None));
    let set: VisPatch = serde_json::from_str(r#"{"strokeColor":[1,2,3]}"#).unwrap();
    assert_eq!(set.stroke_color, Some(Some([1, 2, 3])));
    assert_eq!(
        serde_json::to_string(&VisPatch::opacity(0.4)).unwrap(),
        r#"{"opacity":0.4}"#
    );
}

#[test]
fn dataset_ref_round_trips() {
    let mut doc = Doc::default();
    doc.apply(DocAction::AddSource {
        id: "a".into(),
        source: Source {
            kind: "geojson".into(),
            data: DataRef::dataset("ds_7", None),
            ..Source::default()
        },
    });
    doc.apply(DocAction::AddSource {
        id: "b".into(),
        source: Source {
            kind: "csv".into(),
            data: DataRef::dataset("ds_8", Some(3)),
            ..Source::default()
        },
    });
    let text = serde_json::to_string(&doc).unwrap();
    assert!(
        text.contains(r#""data":{"ref":"dataset","dataset":"ds_7","version":"latest"}"#),
        "{text}"
    );
    let back: Doc = serde_json::from_str(&text).unwrap();
    assert_eq!(back, doc);
    assert_eq!(back.sources["a"].data.dataset_id(), Some("ds_7"));
    assert_eq!(back.sources["a"].data.dataset_version(), Some(None));
    assert_eq!(back.sources["b"].data.dataset_version(), Some(Some(3)));
    assert_eq!(DataRef::url("https://x").dataset_id(), None);
}

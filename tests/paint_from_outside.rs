//! The view of `FeaturePaint` an APPLICATION has.
//!
//! An integration test is a separate crate, which is the whole point of
//! putting these here: a unit test inside `gpui-maps` can name
//! `overlay::paint::IconAnchor` whether or not that type is re-exported,
//! and would go on passing while every consumer of the crate was unable to
//! write down the type of a field they can see in the docs. Only a test
//! that links `gpui-maps` from outside grades the public surface.
//!
//! `set_features_from` takes a `gpui::Context<MapView>`, which no test in
//! or out of this crate can build, so the round trip below goes through
//! `OverlayStore::replace_from` -- the whole fallible body of
//! `set_features_from`, and the only part of it that can refuse a paint.

use gpui_maps::overlay::{
    FeatureId, FeatureInput, FeaturePaint, GeometryInput, IconAnchor, OverlayStore, TextFit, TextJustify,
};
use gpui_maps::style::{DashPattern, Rgba, ZoomValue};

/// Every field of `FeaturePaint`, with every field's type WRITTEN DOWN in
/// a crate that is not `gpui-maps`.
///
/// The struct literal is the assertion. A field whose type an application
/// cannot name makes this signature unwritable, and a private field makes
/// the literal itself illegal -- so this failing to compile is exactly the
/// bug being fixed, and it cannot pass by accident.
#[allow(clippy::too_many_arguments)]
fn assemble(
    line_color: Rgba,
    line_width: ZoomValue<f32>,
    line_dash: DashPattern,
    fill_color: Rgba,
    circle_radius: f32,
    circle_color: Rgba,
    icon_image: Option<String>,
    icon_color: Rgba,
    icon_halo_color: Rgba,
    icon_halo_width: f32,
    icon_anchor: IconAnchor,
    icon_rotate_deg: f32,
    icon_text_fit: TextFit,
    icon_allow_overlap: bool,
    text_allow_overlap: bool,
    icon_text_fit_padding: [f32; 4],
    text_field: Option<String>,
    text_offset: (f32, f32),
    text_anchor: gpui_maps::style::TextAnchor,
    text_justify: TextJustify,
    text_clip_behind_icons: bool,
    min_zoom: Option<f32>,
    text_color: Rgba,
    text_size: f32,
    text_halo_color: Rgba,
    text_halo_width: f32,
) -> FeaturePaint {
    FeaturePaint {
        line_color,
        line_width,
        line_dash,
        fill_color,
        circle_radius,
        circle_color,
        icon_image,
        icon_color,
        icon_halo_color,
        icon_halo_width,
        icon_anchor,
        icon_rotate_deg,
        icon_text_fit,
        icon_allow_overlap,
        text_allow_overlap,
        icon_text_fit_padding,
        text_field,
        text_offset,
        text_anchor,
        text_justify,
        text_clip_behind_icons,
        min_zoom,
        text_color,
        text_size,
        text_halo_color,
        text_halo_width,
    }
}

#[test]
fn every_field_type_of_a_feature_paint_can_be_named_from_outside_the_crate() {
    let blue = Rgba { r: 0.23, g: 0.51, b: 0.96, a: 1.0 };
    let paint = assemble(
        blue,
        ZoomValue::constant(4.0),
        DashPattern::from_runs(&[2.0, 2.0]),
        blue,
        5.0,
        blue,
        Some("pin".into()),
        blue,
        blue,
        1.0,
        IconAnchor::Bottom,
        45.0,
        TextFit::Both,
        true,
        true,
        [1.0, 2.0, 1.0, 2.0],
        Some("42".into()),
        (0.0, -1.5),
        gpui_maps::style::TextAnchor::Left,
        TextJustify::Left,
        true,
        Some(11.0),
        blue,
        12.0,
        blue,
        1.0,
    );

    // Not just nameable but USABLE: the two enums have to carry their
    // behaviour across the module boundary too, or a caller can build the
    // value and still not reason about it.
    assert_eq!(paint.icon_anchor.centre_offset(), (0.0, -0.5));
    assert!(paint.icon_text_fit.fits_width());
    assert!(paint.icon_text_fit.fits_height());
    assert_eq!(paint.line_dash.count, 2);
    // Degrees, not radians, and carried verbatim across the boundary: a
    // consumer holding a compass bearing writes it in directly. The
    // `.to_radians()` happens at the draw site, matching the basemap's
    // `style::SymbolFeature::icon_rotate_deg`.
    assert_eq!(paint.icon_rotate_deg, 45.0);
}

/// The enums' own variants, all of them, from outside. A partial
/// re-export would compile the test above and still leave a caller unable
/// to say `top-left`.
#[test]
fn every_variant_of_the_re_exported_enums_can_be_named_from_outside_the_crate() {
    let anchors = [
        IconAnchor::Center,
        IconAnchor::Left,
        IconAnchor::Right,
        IconAnchor::Top,
        IconAnchor::Bottom,
        IconAnchor::TopLeft,
        IconAnchor::TopRight,
        IconAnchor::BottomLeft,
        IconAnchor::BottomRight,
    ];
    assert_eq!(anchors.len(), 9);
    let fits = [TextFit::None, TextFit::Width, TextFit::Height, TextFit::Both];
    assert_eq!(fits.len(), 4);
    let justifications = [TextJustify::Center, TextJustify::Left, TextJustify::Right];
    assert_eq!(justifications.len(), 3);
}

/// A paint built entirely by hand reaches the store, which is what the
/// consumer could not do: `FeatureInput` wants a `FeaturePaint`, and until
/// now the only way to obtain one was to build a `serde_json::Map`.
#[test]
fn a_hand_built_paint_goes_through_the_structured_door() {
    let route: &[[f64; 2]] = &[[103.85, 1.29], [103.86, 1.30]];
    let parts = [route];
    let paint = assemble(
        Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
        ZoomValue::constant(6.0),
        DashPattern::none(),
        Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 },
        0.0,
        Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 },
        None,
        Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 },
        Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 },
        0.0,
        IconAnchor::Center,
        0.0,
        TextFit::None,
        false,
        false,
        [0.0; 4],
        None,
        (0.0, 0.0),
        gpui_maps::style::TextAnchor::Center,
        TextJustify::Center,
        true,
        None,
        Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 },
        12.0,
        Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 },
        1.0,
    );

    let mut store = OverlayStore::new();
    store
        .replace_from(
            &[FeatureInput {
                id: Some(FeatureId::String("route-42".into())),
                geometry: GeometryInput::Line(&parts),
                paint,
            }],
            12,
        )
        .expect("a hand-built paint is a legal paint");
    assert_eq!(store.features().len(), 1);
    assert_eq!(store.features()[0].paint.line_color.r, 1.0);
}

/// The acceptance criterion: the thing the consumer sat down to write and
/// could not.
///
/// A struct literal naming only the handful of properties this feature
/// actually differs on, `..Default::default()` for the other seventeen,
/// and no `serde_json` anywhere in it. Both re-exported enums appear,
/// because they are the two types whose absence made this unwritable.
#[test]
fn a_struct_literal_with_default_default_reaches_the_map() {
    let stop: &[[f64; 2]] = &[[103.8511, 1.2905]];
    let route: &[[f64; 2]] = &[[103.85, 1.29], [103.86, 1.30], [103.87, 1.31]];
    let route_parts = [route];

    let features = [
        FeatureInput {
            id: Some(FeatureId::String("route-42".into())),
            geometry: GeometryInput::Line(&route_parts),
            paint: FeaturePaint {
                line_color: Rgba { r: 0.9, g: 0.2, b: 0.2, a: 1.0 },
                line_width: ZoomValue::constant(6.0),
                line_dash: DashPattern::from_runs(&[2.0, 2.0]),
                ..Default::default()
            },
        },
        FeatureInput {
            id: Some(FeatureId::Number(7.0)),
            geometry: GeometryInput::Point(stop),
            paint: FeaturePaint {
                icon_image: Some("pin".into()),
                // The two field types that were unnameable, which is the
                // whole reason this test exists.
                icon_anchor: IconAnchor::Bottom,
                icon_text_fit: TextFit::Both,
                icon_text_fit_padding: [2.0, 4.0, 2.0, 4.0],
                text_field: Some("42".into()),
                text_offset: (0.0, -1.5),
                text_allow_overlap: true,
                ..Default::default()
            },
        },
    ];

    let mut store = OverlayStore::new();
    store.replace_from(&features, 12).expect("a hand-built collection is a legal collection");

    assert_eq!(store.features().len(), 2);
    assert_eq!(store.generation(), 1);

    // What was NOT written came from `Default`, and `Default` is the
    // documented default rather than a zero -- a stop drawn with a 0pt
    // label and an invisible dot would be the derived-impl bug, and it
    // would look exactly like a map that simply failed to draw.
    let stop = &store.features()[1].paint;
    assert_eq!(stop.icon_anchor, IconAnchor::Bottom);
    assert_eq!(stop.icon_text_fit, TextFit::Both);
    assert_eq!(stop.text_size, 12.0, "an unwritten text-size is 12, not 0");
    assert_eq!(stop.circle_radius, 5.0, "an unwritten circle-radius is 5, not 0");
    assert_eq!(stop.text_halo_width, 1.0, "an unwritten text-halo-width is 1, not 0");
    assert!(stop.text_color.a > 0.0, "an unwritten text-colour is visible");

    let route = &store.features()[0].paint;
    assert_eq!(route.line_width.value_at(12), 6.0);
    assert_eq!(route.line_dash.count, 2);
    assert_eq!(route.icon_anchor, IconAnchor::Center, "untouched, so Mapbox's own default");
}

/// The invariant, from outside: `Default` and an empty property map are
/// the same paint. Stated here as well as in the unit tests because it is
/// a PUBLIC promise -- a caller mixing `..Default::default()` with paints
/// that came in as GeoJSON is relying on exactly this.
#[test]
fn the_default_paint_equals_what_an_empty_property_map_produces() {
    assert_eq!(
        FeaturePaint::default(),
        FeaturePaint::from_properties(&serde_json::Map::new()).unwrap(),
    );
}

/// The view of DATA-DRIVEN PAINT an application has, graded the same way
/// and for the same reason: a unit test inside `gpui-maps` can name
/// `overlay::source_paint::FeatureProperties` whether or not it is
/// re-exported, and would go on passing while no consumer could write down
/// the type of the field they are being told to fill in.
///
/// Every type this needs is named explicitly below --
/// `DataFeatureInput`, `FeatureProperties`, `TagValue`, `no_properties` --
/// and the struct literal is the assertion.
#[test]
fn a_data_driven_source_can_be_declared_and_pushed_from_outside_the_crate() {
    use gpui_maps::overlay::{no_properties, DataFeatureInput, FeatureProperties, TagValue};
    use std::sync::Arc;

    let mut store = OverlayStore::new();
    let declaration: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(r#"{"circle-color":["get","route_colour"]}"#).unwrap();
    store.declare_source_paint("vehicles", &declaration).unwrap();

    // Built once, cloned per push -- the shape the doc comment describes.
    let red: Arc<FeatureProperties> = Arc::new(FeatureProperties::from([(
        "route_colour".to_string(),
        TagValue::String("#ff0000".to_string()),
    )]));
    let blue: Arc<FeatureProperties> = Arc::new(FeatureProperties::from([(
        "route_colour".to_string(),
        TagValue::String("#0000ff".to_string()),
    )]));

    let one: &[[f64; 2]] = &[[103.85, 1.29]];
    let two: &[[f64; 2]] = &[[103.86, 1.30]];
    let three: &[[f64; 2]] = &[[103.87, 1.31]];
    let dot = |id: &str, positions: &'static [[f64; 2]], properties: Arc<FeatureProperties>| {
        DataFeatureInput {
            feature: FeatureInput {
                id: Some(FeatureId::String(id.to_string())),
                geometry: GeometryInput::Point(positions),
                paint: FeaturePaint { circle_radius: 6.0, ..FeaturePaint::default() },
            },
            properties,
        }
    };

    store
        .set_source_data_from(
            "vehicles",
            &[
                dot("bus-1", one, Arc::clone(&red)),
                dot("bus-2", two, Arc::clone(&blue)),
                dot("bus-3", three, red),
            ],
            12,
        )
        .unwrap();

    // Two colours, so two ops -- and still three features with three ids.
    assert_eq!(store.source_op_count("vehicles"), Some(2));
    assert_eq!(store.source_is_data_driven("vehicles"), Some(true));
    assert_eq!(store.source_features("vehicles").map(<[_]>::len), Some(3));

    // And a feature with no properties at all is expressible, which is what
    // `no_properties` is re-exported for.
    store
        .set_source_data_from("plain", &[dot("x", one, no_properties())], 12)
        .unwrap();
    assert_eq!(store.source_op_count("plain"), Some(1));
}

/// A declaration an application gets wrong comes back as an ERROR it can
/// read, not a panic and not a silently un-applied expression.
#[test]
fn a_bad_paint_declaration_is_a_readable_error_from_outside_the_crate() {
    let mut store = OverlayStore::new();
    let declaration: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(r#"{"line-width":4}"#).unwrap();
    let error = store.declare_source_paint("vehicles", &declaration).unwrap_err();
    let message = error.to_string();
    assert!(message.contains("vehicles"), "message was {message}");
    assert!(message.contains("line-width"), "message was {message}");
}
